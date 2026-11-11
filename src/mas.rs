//! Mac App Store 沙箱版（cargo feature `app-store`）专用代码。
//!
//! 这个文件只在 `--features app-store` 下编译；默认构建不含它，行为一个字节不变。
//! 内容：
//! - `--mas-probe`：沙箱探针。把沙箱里 HOME / 数据根 / 辅助功能与输入监控权限 /
//!   `CGEventTap` / Carbon `RegisterEventHotKey` / 剪贴板 / 子进程能否运行一次性打出来，
//!   给 docs/app-store.md §4 阶段 1 填实测结果用。**只探测，不改任何配置、不装任何东西。**
//! - `--mas-probe-run <命令> [参数…]`：在本进程的沙箱里拉起一个子进程并报告结果。
//! - `apply_limits`：把配置压到 MAS 子集（见 `config::load` / `config::update`）。

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Config, InputTranslate, TalkMode, Trigger};
use crate::engine::AsrBackend;

/// MAS 子集不提供的功能一律在这里压回安全值。配置文件里写了也不生效。
///
/// 为什么压配置而不是到处加 `cfg`：这些功能的入口（菜单、命令行、启动对账）
/// 都读配置，压在读取处，下游所有分支一次性变成「功能关闭」的既有行为，
/// 不需要在几十处各开一个新分支。
pub fn apply_limits(c: &mut Config) {
    // 右 Command 手势要 CGEventTap（输入监控/辅助功能）；MAS 初期版只给组合键。
    c.trigger = Trigger::CtrlShiftR;
    // 自动 ⌘V 靠 CGEventPost + 辅助功能；MAS 版只写剪贴板。
    c.auto_paste = false;
    // 对话模式要 Python 边车（下载并执行代码，审核指南 2.5.2）。
    c.talk_mode = TalkMode::InputMethod;
    // Qwen3 要下载运行时可执行文件（审核指南 2.5.2）。
    c.asr_backend = AsrBackend::Builtin;
    c.qwen3_resident = false;
    // 开机自启靠写 ~/Library/LaunchAgents。
    c.launch_at_login = false;
    // 翻译输入走本机 LLM 边车。
    c.input_translate = InputTranslate::Off;
    // 理解层 LLM 边车（自动拉起）。
    c.llm_autostart = false;
    c.llm_start_command.clear();
}

fn line(k: &str, v: impl std::fmt::Display) {
    println!("{k} = {v}");
}

extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn AXIsProcessTrusted() -> bool;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut std::ffi::c_void,
        options: u32,
        out_ref: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn UnregisterEventHotKey(r: *mut std::ffi::c_void) -> i32;
    fn GetApplicationEventTarget() -> *mut std::ffi::c_void;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

/// 子进程限时执行，返回一行结果描述。
fn run_limited(cmd: &str, args: &[&str], secs: u64) -> String {
    let t0 = Instant::now();
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("SPAWN_FAILED: {e}"),
    };
    let deadline = t0 + Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("TIMEOUT after {secs}s (killed)");
            }
            Err(e) => return format!("WAIT_FAILED: {e}"),
        }
    }
    match child.wait_with_output() {
        Ok(o) => {
            let cut = |b: &[u8]| {
                let s: String = String::from_utf8_lossy(b).chars().take(300).collect();
                s.replace('\n', "\\n")
            };
            format!(
                "SPAWNED exit={:?} signal={:?} {}ms stdout=[{}] stderr=[{}]",
                o.status.code(),
                std::os::unix::process::ExitStatusExt::signal(&o.status),
                t0.elapsed().as_millis(),
                cut(&o.stdout),
                cut(&o.stderr)
            )
        }
        Err(e) => format!("OUTPUT_FAILED: {e}"),
    }
}

pub fn run_probe_run(args: &[String]) -> i32 {
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("用法：--mas-probe-run <命令> [参数…]");
        return 2;
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    println!("{}", run_limited(cmd, &rest, 120));
    0
}

pub fn run_probe(vendor: &Path) -> i32 {
    println!("# AgentEar --mas-probe（feature app-store）");
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "<unset>".into());
    line(
        "exe",
        std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
    );
    line("env.HOME", env("HOME"));
    line(
        "dirs::home_dir",
        dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_default(),
    );
    line("env.APP_SANDBOX_CONTAINER_ID", env("APP_SANDBOX_CONTAINER_ID"));
    line("env.TMPDIR", env("TMPDIR"));
    line("env.AGENTEAR_DATA", env("AGENTEAR_DATA"));
    line("vendor", vendor.display());
    let data = crate::data_root();
    match &data {
        Ok(p) => {
            line("data_root", p.display());
            match std::fs::create_dir_all(p).and_then(|_| std::fs::write(p.join(".probe"), b"x")) {
                Ok(()) => line("data_root.write", "OK"),
                Err(e) => line("data_root.write", format!("FAILED {e}")),
            }
            let _ = std::fs::remove_file(p.join(".probe"));
        }
        Err(e) => line("data_root", format!("ERR {e}")),
    }
    // 容器外写入（沙箱应拒绝）。成功就立刻删掉。
    let outside = "/private/tmp/agentear-mas-probe-outside";
    match std::fs::write(outside, b"x") {
        Ok(()) => {
            line("write_outside_container(/private/tmp)", "OK (沙箱没拦！)");
            let _ = std::fs::remove_file(outside);
        }
        Err(e) => line("write_outside_container(/private/tmp)", format!("DENIED {e}")),
    }
    // 真实 home 下的 .agentear：只问 metadata，不读内容。
    line(
        "real_home .agentear metadata",
        match std::fs::read_dir("/Users") {
            Ok(rd) => {
                let mut r = "not visible".to_string();
                for e in rd.flatten() {
                    let p = e.path().join(".agentear");
                    if std::fs::metadata(&p).is_ok() {
                        r = format!("VISIBLE {}", p.display());
                    }
                }
                r
            }
            Err(e) => format!("read_dir(/Users) DENIED {e}"),
        },
    );

    // 2) 权限与热键
    unsafe {
        line("CGPreflightListenEventAccess", CGPreflightListenEventAccess());
        line("CGPreflightPostEventAccess", CGPreflightPostEventAccess());
        line("AXIsProcessTrusted", AXIsProcessTrusted());
    }
    line("CGEventTapCreate(listen-only,session,flagsChanged+keyDown)", tap_probe());
    carbon_probe();

    // 5) 剪贴板
    clipboard_probe();

    // 3) 随包二进制
    let sv = vendor.join("bin/llama-funasr-sensevoice");
    line(
        "helper.llama-funasr-sensevoice --help",
        run_limited(&sv.to_string_lossy(), &["--help"], 20),
    );
    let wc = vendor.join("bin/whisper-cli");
    line("helper.whisper-cli --help", run_limited(&wc.to_string_lossy(), &["--help"], 20));

    // 4) 系统子进程
    line("sub.curl --version", run_limited("/usr/bin/curl", &["--version"], 20));
    line(
        "sub.curl http://127.0.0.1:9/ (network.client)",
        run_limited("/usr/bin/curl", &["-sS", "-m", "3", "http://127.0.0.1:9/"], 10),
    );
    line("sub.afplay (nonexistent file)", run_limited("/usr/bin/afplay", &["/nonexistent.wav"], 10));
    line("sub.say -v ? (list voices)", run_limited("/usr/bin/say", &["-v", "?"], 10));
    let say_out = data
        .as_ref()
        .map(|p| p.join("probe-say.aiff"))
        .unwrap_or_else(|_| "/dev/null".into());
    line(
        "sub.say -o <data_root>/probe-say.aiff hi",
        run_limited("/usr/bin/say", &["-o", &say_out.to_string_lossy(), "hi"], 20),
    );
    line(
        "say output size",
        std::fs::metadata(&say_out)
            .map(|m| m.len().to_string())
            .unwrap_or_else(|e| format!("none ({e})")),
    );
    let _ = std::fs::remove_file(&say_out);
    line(
        "sub.open -g -b com.apple.finder",
        run_limited("/usr/bin/open", &["-g", "-b", "com.apple.finder"], 10),
    );
    line("sub.sh -c true", run_limited("/bin/sh", &["-c", "true"], 10));
    line("sub.afconvert -h", run_limited("/usr/bin/afconvert", &["-h"], 10));
    0
}

fn tap_probe() -> String {
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult,
    };
    let r = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
        |_p, _t, _e| CallbackResult::Keep,
    );
    match r {
        Ok(_) => "created (non-null)".into(),
        Err(_) => "FAILED (null)".into(),
    }
}

fn carbon_probe() {
    // 直接调 Carbon，拿到 OSStatus。kVK_ANSI_R=15；controlKey=0x1000 | shiftKey=0x0200。
    let mut hk_ref: *mut std::ffi::c_void = std::ptr::null_mut();
    let st = unsafe {
        RegisterEventHotKey(
            15,
            0x1000 | 0x0200,
            EventHotKeyID { signature: u32::from_be_bytes(*b"AgEr"), id: 1 },
            GetApplicationEventTarget(),
            0,
            &mut hk_ref,
        )
    };
    line("RegisterEventHotKey(Ctrl+Shift+R) OSStatus", st);
    if st == 0 && !hk_ref.is_null() {
        let u = unsafe { UnregisterEventHotKey(hk_ref) };
        line("UnregisterEventHotKey OSStatus", u);
    }
    // 走 global-hotkey（产品里实际用的那条）。
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    match global_hotkey::GlobalHotKeyManager::new() {
        Ok(m) => {
            let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            line(
                "global-hotkey register(Ctrl+Shift+R)",
                match m.register(hk) {
                    Ok(()) => "OK".to_string(),
                    Err(e) => format!("ERR {e}"),
                },
            );
            let _ = m.unregister(hk);
        }
        Err(e) => line("global-hotkey manager", format!("ERR {e}")),
    }
}

fn clipboard_probe() {
    let stamp = format!("agentear-mas-probe-{}", std::process::id());
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            let w = cb.set_text(stamp.clone());
            line(
                "NSPasteboard write (arboard)",
                match &w {
                    Ok(()) => "OK".to_string(),
                    Err(e) => format!("ERR {e}"),
                },
            );
            let back = cb.get_text().unwrap_or_else(|e| format!("<read err {e}>"));
            line("NSPasteboard read-back matches", back == stamp);
        }
        Err(e) => line("NSPasteboard open", format!("ERR {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_force_the_mas_subset_whatever_the_config_says() {
        let mut c = Config::default();
        c.trigger = Trigger::RightCommand;
        c.auto_paste = true;
        c.talk_mode = TalkMode::Conversation;
        c.asr_backend = AsrBackend::SpeechSwift;
        c.qwen3_resident = true;
        c.launch_at_login = true;
        c.input_translate = InputTranslate::En;
        c.llm_autostart = true;
        c.llm_start_command = vec!["x".into()];
        apply_limits(&mut c);
        assert_eq!(c.trigger, Trigger::CtrlShiftR);
        assert!(!c.auto_paste);
        assert_eq!(c.talk_mode, TalkMode::InputMethod);
        assert_eq!(c.asr_backend, AsrBackend::Builtin);
        assert!(!c.qwen3_resident);
        assert!(!c.launch_at_login);
        assert_eq!(c.input_translate, InputTranslate::Off);
        assert!(!c.llm_autostart);
        assert!(c.llm_start_command.is_empty());
    }

    #[test]
    fn a_config_file_asking_for_the_removed_features_is_clamped() {
        let mut c: Config = serde_json::from_str(
            r#"{"trigger":"right_command","auto_paste":true,"talk_mode":"conversation"}"#,
        )
        .unwrap();
        assert_eq!(c.trigger, Trigger::RightCommand, "前提：文件里确实写了");
        apply_limits(&mut c);
        assert_eq!(c.trigger, Trigger::CtrlShiftR);
        assert!(!c.auto_paste);
        assert_eq!(c.talk_mode, TalkMode::InputMethod);
    }
}
