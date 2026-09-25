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
//! 不碰 LaunchAgent**，并且把这件事写进日志（不静默）。同一个判据也
//! 挡掉 App Translocation 的临时路径（Gatekeeper 隔离下没移出下载目录
//! 就打开时，系统会把 `.app` 挂到一个 `AppTranslocation/<uuid>/...`
//! 的临时只读位置——那个路径下次登录就可能不存在了，写进开机自启
//! 只会留下一条指向空地的 plist）。
//!
//! ## ⚠️ 只写文件，不调 `launchctl bootstrap`/`bootout`（2026-09-23 改，codex 复查抓到）
//!
//! 最初的版本会在这里直接调 `launchctl bootstrap`/`bootout` 把改动
//! **立刻**在当前登录会话里生效，结果是两个真实会出事的场景：
//!
//! 1. **开启时**：`bootstrap` 一个 `RunAtLoad=true` 的 job 会让 launchd
//!    **立刻**再启动一个 AgentEar 实例——而调用这段代码的，正是
//!    **已经在跑的那个实例本身**。于是勾选"开机自动启动"这个动作，
//!    会当场把自己复制出第二个进程，两个实例抢同一个全局热键、
//!    同一份 `~/.agentear/config.json`、同一个日志文件。
//! 2. **关闭或改路径时**：`bootout` 卸载的目标有可能正是**当前进程自己
//!    所属的那个 job**（如果这次运行本来就是 launchd 在登录时启动的）
//!    ——那样一调 `bootout`，launchd 会把当前正在执行这段代码的进程
//!    直接杀掉，**写新 plist 的后续代码根本执行不到**，配置和实际状态
//!    从此对不上，且没有任何报错（进程已经没了，日志也来不及写）。
//!
//! 而"开机自动启动"这件事本来就**不需要立刻生效**——它承诺的是
//! "下次登录会自动启动"，不是"现在立刻再启动一个"。launchd 会在每次
//! 登录时自己扫描 `~/Library/LaunchAgents/`，plist 在那儿就够了，
//! 不需要 `bootstrap` 去手动激活当前会话。所以这里**只做文件层面的
//! 增删**，不碰 launchd 的运行时状态——**代价是**：如果当前这个实例
//! 恰好是被 launchd 用一份旧 plist（比如指向升级前的安装路径）启动的,
//! 那么直到下次登录之前,这个旧 job 依然会按旧路径被 launchd
//! 记着——但旧路径的可执行文件在原地没动过（.app 内部升级只换文件
//! 内容，不换路径),所以这不是一个真实会出问题的场景。

use std::io::Write;
use std::path::{Path, PathBuf};

const LABEL: &str = "ai.idoris.agentear";

/// 一条可执行文件路径是不是"值得写进开机自启配置"的那种：真正、稳定的
/// `.app` bundle（不是开发时的裸二进制，不是 App Translocation 的临时
/// 挂载点，也不含会让生成的 plist 解析失败的 XML 非法字符）。
///
/// **抽成纯函数**是为了能被测试真正调用到——早先两条同名测试只是在
/// 断言手写字符串本身的性质，从没调用过这个判据，改坏了判据本身
/// 测试也不会红（codex 复查抓到）。
fn is_stable_bundle_path(s: &str) -> bool {
    if !s.contains(".app/Contents/MacOS/") {
        return false;
    }
    if s.contains("AppTranslocation") {
        return false;
    }
    // plist 里的路径必须是合法 XML 文本：控制字符（除了 XML 1.0 允许的
    // tab/LF/CR）会让生成的 plist 直接解析失败，而这种字符正常路径里
    // 不会出现——出现了大概率是数据损坏，与其生成一份 launchd 读不了
    // 的 plist，不如直接当成"不是一个正常安装"跳过。
    if s.chars().any(|c| (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r')) {
        return false;
    }
    true
}

/// 当前运行的可执行文件是否在一个真正、稳定的 `.app` bundle 里。
pub fn bundle_executable_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    is_stable_bundle_path(&exe.to_string_lossy()).then_some(exe)
}

fn plist_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn plist_contents(exe: &Path) -> String {
    // XML 元素文本里 `<` `>` `&` 需要转义（引号在 `<string>` 的 PCDATA
    // 里不需要）；路径正常不会含这些字符，但生成函数本身要按规矩转义，
    // 不能假设"路径永远干净"。
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

/// 把 `launch_at_login` 这个配置项落到磁盘上的 LaunchAgent plist。
///
/// **只写/删文件，不碰 launchd 的运行时状态**（见模块文档"为什么只写
/// 文件"）。下次登录生效，不影响当前正在跑的这个实例。
///
/// **幂等、可反复调用**：daemon 每次启动都会调一次（配置可能是升级后
/// 换了路径，也可能是用户手改了 config.json），内容没变就直接跳过。
///
/// ⚠️ **不接收参数、自己现读配置**，而且整段过程持一把锁
/// （codex 复查抓到的竞态）：启动时的对账线程和用户点开关触发的线程
/// 都会调这个函数，如果各自带着"调用那一刻"的配置值、又不互斥，
/// 两个线程谁后写文件谁说了算——先点开关（关）后启动对账（还在用
/// 旧值"开"）这种顺序会让最终文件状态和用户刚做的选择相反。
/// 现读 + 持锁，保证"最后落盘的就是当前最新配置"，不管两个调用点
/// 谁先谁后开始跑。
pub fn apply() {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let enabled = crate::config::get().launch_at_login;

    let Some(exe) = bundle_executable_path() else {
        log::info!(
            "开机自动启动：当前不是从一个稳定的 .app bundle 里跑的（{}），跳过——\
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
        if std::fs::read_to_string(&path).map(|s| s == wanted).unwrap_or(false) {
            log::debug!("开机自动启动：plist 已经是最新内容，不用重写");
            return;
        }
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::error!("开机自动启动：创建 {} 失败: {e}", dir.display());
                return;
            }
        }
        // 原子替换：先写临时文件再 rename，避免半截写坏的 plist
        // 被 launchd 在下次登录时读到。
        let tmp = path.with_extension("plist.tmp");
        match std::fs::File::create(&tmp).and_then(|mut f| f.write_all(wanted.as_bytes())) {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    log::error!("开机自动启动：写 {} 失败: {e}", path.display());
                    let _ = std::fs::remove_file(&tmp);
                    return;
                }
                log::info!("开机自动启动：已写入（下次登录生效）: {}", exe.display());
            }
            Err(e) => {
                log::error!("开机自动启动：写 {} 失败: {e}", tmp.display());
            }
        }
    } else {
        if !path.exists() {
            log::debug!("开机自动启动：本来就没装，不用关");
            return;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            log::error!("开机自动启动：删除 {} 失败: {e}", path.display());
        } else {
            log::info!("开机自动启动：已关闭（下次登录起不再自动启动）");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_binary_path_is_not_a_bundle_path() {
        // 开发时 current_exe() 长这样，不该被误判成「装在 App 里」。
        assert!(!is_stable_bundle_path(
            "/Users/jason/Dev/tools/AgentEar/target/release/agentear"
        ));
    }

    #[test]
    fn an_installed_app_path_is_recognized() {
        assert!(is_stable_bundle_path(
            "/Applications/AgentEar.app/Contents/MacOS/AgentEar"
        ));
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

    /// `RunAtLoad=true` 是给 launchd 在下次登录时用的语义。这条只钉住
    /// plist 内容本身写对了——**不证明** `apply()` 不会立刻触发它，
    /// 那件事是靠"整个模块没有任何 `launchctl` 调用"这个结构性事实
    /// 保证的（`grep -c launchctl src/launch_agent.rs` 应该是 0，
    /// 见模块文档"为什么只写文件"）。
    #[test]
    fn plist_content_declares_run_at_load() {
        let xml = plist_contents(Path::new("/Applications/AgentEar.app/Contents/MacOS/AgentEar"));
        assert!(xml.contains("<key>RunAtLoad</key>\n    <true/>"));
    }

    #[test]
    fn app_translocation_paths_are_rejected() {
        // 从 Finder 直接双击一个还在隔离属性下、没挪出下载目录的 .app，
        // 系统会把它挂到 AppTranslocation 的临时只读路径——这个路径
        // 移出隔离或重新打开后就可能不存在了，写进开机自启只会留下
        // 一条指向空地的 plist。
        let translocated = "/private/var/folders/xy/abc123/T/AppTranslocation/\
                             11111111-2222-3333-4444-555555555555/d/AgentEar.app/\
                             Contents/MacOS/AgentEar";
        assert!(!is_stable_bundle_path(translocated));
    }

    #[test]
    fn control_characters_in_path_are_rejected() {
        // 真实路径几乎不可能出现控制字符，但生成函数不能假设"路径永远
        // 干净"——这条钉住判据本身会挡住它，不是钉住"正常路径没有它"。
        assert!(!is_stable_bundle_path(
            "/Applications/Weird\u{0007}.app/Contents/MacOS/AgentEar"
        ));
    }
}
