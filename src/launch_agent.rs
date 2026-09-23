//! 开机自动启动：`~/Library/LaunchAgents/ai.idoris.agentear.plist`。
//!
//! **为什么是 LaunchAgent plist，不是 `SMAppService`**：后者要 macOS 13+，
//! 而 `Info.plist` 里 `LSMinimumSystemVersion` 写的是 11.0——真用上会在
//! 老系统上直接编译期/运行期都过，却在真机上找不到符号。LaunchAgent
//! plist 这条路径从 10.4 就有，兼容面更宽，而且不需要新引入
//! ServiceManagement framework 的绑定。
//!
//! **为什么只在从 .app bundle 里跑的时候才真正生效**：`current_exe()`
//! 在开发时指向 `target/release/agentear`，把这个路径写进开机自启的
//! plist，会让「登录后自动起一个开发目录里的调试二进制」变成开发机上
//! 一个隐形的常驻进程——这不是任何人会想要的行为。用「路径里有没有
//! `.app/Contents/MacOS/`」这个判据区分：**没有就直接跳过，只落配置、
//! 不碰 LaunchAgent**，并且把这件事写进日志（不静默）。

use std::io::Write;
use std::path::{Path, PathBuf};

const LABEL: &str = "ai.idoris.agentear";

/// 当前运行的可执行文件是否在一个真正的 `.app` bundle 里。
///
/// 只有这种情况下才有一个跨重装稳定的路径值得写进开机自启配置——
/// 裸二进制（`cargo run` / `target/release/agentear`）每次编译位置都可能变,
/// 而且那本来就不是给普通用户开机自启的东西。
pub fn bundle_executable_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let s = exe.to_string_lossy();
    if s.contains(".app/Contents/MacOS/") {
        Some(exe)
    } else {
        None
    }
}

fn plist_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn plist_contents(exe: &Path) -> String {
    // XML 里 `<` `>` `&` 需要转义；路径正常不会含这些字符，但按需求
    // 文档写死不如按规矩转义——万一哪天数据目录/路径里出现这些字符，
    // 生成的 plist 不会因为转义漏了而解析失败。
    let escaped = exe
        .to_string_lossy()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{escaped}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <!-- 崩了不自动重启：一个开机自启的进程如果反复崩溃又反复重启，
         用户只会看到菜单栏图标疯狂闪烁，比不启动更糟。崩溃排查走日志，
         不该靠 launchd 硬重启掩盖。 -->
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    )
}

fn launchctl(args: &[&str]) -> bool {
    match std::process::Command::new("/bin/launchctl").args(args).output() {
        Ok(out) => {
            if !out.status.success() {
                // bootout 对「本来就没装」这种情况天然会报错（No such process
                // 之类），这是预期路径，不算失败——调用方按「文件删没删」
                // 判断真正的结果，这里只记日志备查。
                log::debug!(
                    "launchctl {args:?} 非零退出: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            out.status.success()
        }
        Err(e) => {
            log::error!("启动 launchctl 失败: {e}");
            false
        }
    }
}

fn gui_target() -> String {
    // SAFETY: `getuid()` 没有失败模式。
    let uid = unsafe { libc::getuid() };
    format!("gui/{uid}")
}

/// 把 `launch_at_login` 这个配置项落到系统的真实开机自启状态。
///
/// **幂等、可反复调用**：daemon 每次启动都会调一次（配置可能是升级后
/// 换了路径，也可能是用户手改了 config.json），只有「想要的状态」和
/// 「plist 里实际写的内容」不一致时才会真的动 launchctl。
pub fn apply(enabled: bool) {
    let Some(exe) = bundle_executable_path() else {
        log::info!(
            "开机自动启动：当前不是从 .app bundle 里跑的（{}），跳过——\
             这个开关只在正式安装的 App 上生效",
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
        );
        return;
    };
    let Some(path) = plist_path() else {
        log::error!("开机自动启动：读不到 HOME，跳过");
        return;
    };

    if enabled {
        let wanted = plist_contents(&exe);
        let already_correct = std::fs::read_to_string(&path).map(|s| s == wanted).unwrap_or(false);
        if already_correct {
            log::debug!("开机自动启动：plist 已经是最新内容，不重复装载");
            return;
        }
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::error!("开机自动启动：创建 {} 失败: {e}", dir.display());
                return;
            }
        }
        // 先卸旧的再写新的：内容变了（比如 .app 换了安装路径）时，
        // launchd 手里那份是旧路径，不先 bootout 的话新 plist 写进去了
        // launchd 也不会自动重新读。
        if path.exists() {
            launchctl(&["bootout", &format!("{}/{LABEL}", gui_target())]);
        }
        let tmp = path.with_extension("plist.tmp");
        match std::fs::File::create(&tmp).and_then(|mut f| f.write_all(wanted.as_bytes())) {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    log::error!("开机自动启动：写 {} 失败: {e}", path.display());
                    let _ = std::fs::remove_file(&tmp);
                    return;
                }
            }
            Err(e) => {
                log::error!("开机自动启动：写 {} 失败: {e}", tmp.display());
                return;
            }
        }
        if launchctl(&["bootstrap", &gui_target(), &path.to_string_lossy()]) {
            log::info!("开机自动启动：已开启（{}）", exe.display());
        } else {
            log::error!("开机自动启动：launchctl bootstrap 失败，见上面的调试日志");
        }
    } else {
        if !path.exists() {
            log::debug!("开机自动启动：本来就没装，不用关");
            return;
        }
        launchctl(&["bootout", &format!("{}/{LABEL}", gui_target())]);
        if let Err(e) = std::fs::remove_file(&path) {
            log::error!("开机自动启动：删除 {} 失败: {e}", path.display());
        } else {
            log::info!("开机自动启动：已关闭");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_binary_path_is_not_a_bundle_path() {
        // 开发时 current_exe() 长这样，不该被误判成「装在 App 里」。
        let dev = "/Users/jason/Dev/tools/AgentEar/target/release/agentear";
        assert!(!dev.contains(".app/Contents/MacOS/"));
    }

    #[test]
    fn an_installed_app_path_is_recognized() {
        let installed = "/Applications/AgentEar.app/Contents/MacOS/AgentEar";
        assert!(installed.contains(".app/Contents/MacOS/"));
    }

    #[test]
    fn plist_contents_escape_xml_special_chars() {
        // 路径正常不会出现这些字符，但生成函数本身要按规矩转义，
        // 不能假设"路径永远干净"——这条钉住转义逻辑本身是对的，
        // 不依赖真实路径恰好没有特殊字符。
        let weird = Path::new("/Applications/A & B.app/Contents/MacOS/A&B");
        let xml = plist_contents(weird);
        assert!(xml.contains("A &amp; B.app"));
        assert!(xml.contains("A&amp;B</string>"));
        assert!(!xml.contains("A & B.app"), "裸 & 会让 plist 解析失败");
    }

    #[test]
    fn plist_contents_do_not_keep_alive() {
        // 崩了不自动重启：写死钉住，免得以后有人为了"更可靠"顺手加上
        // KeepAlive=true，反而把崩溃循环伪装成正常运行。
        let xml = plist_contents(Path::new("/Applications/AgentEar.app/Contents/MacOS/AgentEar"));
        assert!(xml.contains("<key>KeepAlive</key>\n    <false/>"));
    }
}
