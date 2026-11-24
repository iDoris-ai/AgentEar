//! 命令行入口的最基本约定：`--version` / `--help` 立刻退出，未知 `--xxx` 报错退出（exit 2），
//! 三者都不许起守护进程、不许碰数据目录。
//!
//! 起因：发版冒烟脚本跑 `agentear --version`，旧版不认识它，当成无参数启动成常驻守护进程，
//! 一直挂着（卡了 4 小时，还抢了麦克风/热键）。所以这里的子进程一律带超时，超时就杀掉并判失败。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str], data: &std::path::Path) -> Out {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentear"))
        .args(args)
        .env("AGENTEAR_DATA", data)
        .env("HOME", data) // 万一落到 ~/.agentear 也只落在临时目录
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("起不了 agentear");
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            let o = child.wait_with_output().unwrap();
            return Out {
                code: o.status.code(),
                stdout: String::from_utf8_lossy(&o.stdout).into(),
                stderr: String::from_utf8_lossy(&o.stderr).into(),
            };
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{args:?} 5 秒内没退出（会被当成守护进程挂住）");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn is_empty_dir(p: &std::path::Path) -> bool {
    std::fs::read_dir(p).map(|mut d| d.next().is_none()).unwrap_or(true)
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("agentear-cli-flags-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn version_prints_one_line_and_exits_zero() {
    let tmp = tmpdir("version");
    for flag in ["--version", "-V"] {
        let o = run(&[flag], &tmp);
        assert_eq!(o.code, Some(0), "{flag}: {}", o.stderr);
        assert_eq!(o.stdout, format!("agentear {}\n", env!("CARGO_PKG_VERSION")));
    }
    assert!(is_empty_dir(&tmp), "--version 不该碰数据目录");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn help_lists_subcommands_and_exits_zero() {
    let tmp = tmpdir("help");
    for flag in ["--help", "-h"] {
        let o = run(&[flag], &tmp);
        assert_eq!(o.code, Some(0), "{flag}: {}", o.stderr);
        for sub in ["--transcribe", "--diagnose", "--version"] {
            assert!(o.stdout.contains(sub), "{flag} 输出缺 {sub}");
        }
    }
    assert!(is_empty_dir(&tmp), "--help 不该碰数据目录");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn unknown_flag_exits_2_without_starting_daemon() {
    let tmp = tmpdir("unknown");
    let o = run(&["--no-such-flag"], &tmp);
    assert_eq!(o.code, Some(2));
    assert!(o.stderr.contains("未知参数") && o.stderr.contains("--help"), "{}", o.stderr);
    assert!(is_empty_dir(&tmp), "未知参数不该创建 pid/run 等文件");
    let _ = std::fs::remove_dir_all(&tmp);
}
