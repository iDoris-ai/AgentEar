//! PR1 RED3：进程级验收——`AGENTEAR_DATA`/`AGENTEAR_VENDOR` 指向临时目录、
//! 只有 bin fixture、没有模型时，`--diagnose` 必须到达完整报告并 exit 0；
//! `--transcribe` 一个合成 wav 必须非零退出，且绝不调用引擎二进制
//! （sentinel 0 次），不能产出空成功转写。
//!
//! 修复前：`main()` 在 `build_dynamic`/`preflight` 那一步先因为模型缺失而
//! `?` 掉，`--diagnose` 根本走不到自己的报告逻辑——这正是本文件要钉住的 RED。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 临时 vendor：只放一个 sentinel bin，不放任何模型/VAD/whisper。
/// bin 被调用一次就在 `base` 下留一个 `called` 标记文件。
fn fixture_vendor(base: &Path) -> PathBuf {
    let vendor = base.join("vendor");
    let bin = vendor.join("bin/llama-funasr-sensevoice");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(
        &bin,
        format!("#!/bin/sh\ntouch {}\n", base.join("called").display()),
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    vendor
}

fn run(vendor: &std::path::Path, data: &std::path::Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentear"))
        .args(extra)
        .env("AGENTEAR_VENDOR", vendor)
        .env("AGENTEAR_DATA", data)
        .output()
        .expect("跑二进制失败")
}

#[test]
fn diagnose_finishes_with_exit_0_when_models_are_missing() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-diagnose-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();

    let out = run(&vendor, &data, &["--diagnose"]);
    assert!(
        out.status.success(),
        "--diagnose 在模型缺失时也要跑完并 exit 0: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ASR 依赖"),
        "报告里要有 ASR 依赖这一段：{stdout}"
    );
    assert!(
        !base.join("called").exists(),
        "diagnose 不该调用引擎二进制去探测模型"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn transcribe_fails_nonzero_without_calling_the_engine() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-transcribe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();

    // 合成一个极简 wav 头，内容是否合法无关紧要——preflight 应该在
    // 调用引擎二进制之前就因为模型缺失而失败。
    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();

    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "模型缺失时 --transcribe 必须非零退出"
    );
    assert!(!base.join("called").exists(), "sentinel 必须是 0 次调用");

    let _ = fs::remove_dir_all(&base);
}
