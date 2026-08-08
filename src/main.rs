//! AgentEar M1：按快捷键录音 → raw 落盘 → 转写 → 剪贴板。
//!
//! 范围严格限定在 `docs/milestones.md` 的 M1：不含 LLM、标签路由、TTS。
//! M1 恰好绕开了 AEC 和无边界流式 raw 语义两个难点——快捷键的按下/再按
//! 天然给出段边界，每次录音就是一个有头有尾的文件对象。**不要在 M1 里
//! 提前引入 TTS 或无边界流。**

mod asr;
mod audio;
mod hotkey;
mod paste;
mod store;
mod tray;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

enum State {
    Idle,
    Recording {
        session: store::Session,
        recorder: audio::Recorder,
        started: Instant,
    },
}

fn main() -> Result<()> {
    init_logging();

    let args: Vec<String> = std::env::args().collect();
    let vendor = vendor_root()?;
    log::debug!("vendor 目录: {}", vendor.display());

    let asr = asr::Asr::new(&vendor)?;
    log::debug!("ASR 依赖检查通过");

    // 离线转写一个已有的 wav，不占麦克风，用于验证 ASR 链路
    if args.len() == 3 && args[1] == "--transcribe" {
        let t0 = Instant::now();
        let text = asr.transcribe(std::path::Path::new(&args[2]))?;
        println!("{text}");
        eprintln!("（耗时 {:.2}s）", t0.elapsed().as_secs_f32());
        return Ok(());
    }

    // 环境自检，排查「按了没反应」
    if args.len() == 2 && args[1] == "--diagnose" {
        return diagnose(&vendor);
    }

    // 打印每一个修饰键事件，确认按键到底有没有被收到
    if args.iter().any(|a| a == "--debug-keys") {
        hotkey::set_debug_keys(true);
        log::info!("已开启按键调试：将打印每一个 flagsChanged 事件");
    }

    let data_root = data_root()?;
    let store = store::Store::open(&data_root)?;
    log::info!("数据目录: {}", store.root().display());

    // 权限引导：想用右 Command 就必须有辅助功能权限。
    // 用 log:: 而非 println!，因为从 Finder 启动 .app 时 stdout 无处可去。
    if !hotkey::is_accessibility_trusted() {
        log::warn!("未获得「辅助功能」权限，无法监听单独的右 Command 键");
        log::warn!("  正在弹出系统授权对话框——授权后**必须重启本程序**才生效");
        log::warn!("  路径：系统设置 → 隐私与安全性 → 辅助功能");
        log::warn!("  注意：.app 的权限与终端是分开的，各自要授权一次");
        hotkey::prompt_accessibility();
    }

    let mut listener = hotkey::Listener::start()?;

    // 自动上屏。默认开，`--no-auto-paste` 或 AGENTEAR_AUTO_PASTE=0 关掉。
    //
    // 同样吃辅助功能权限：CGEventPost 未授权时**静默失败**——不报错、什么也
    // 不发生。所以这里主动降级，不然用户会看到「转写成功但没上屏」且日志无痕。
    let want_paste = !args.iter().any(|a| a == "--no-auto-paste")
        && !matches!(
            std::env::var("AGENTEAR_AUTO_PASTE").as_deref(),
            Ok("0") | Ok("false") | Ok("no")
        );
    let can_paste = want_paste && hotkey::is_accessibility_trusted();
    if want_paste && !can_paste {
        log::warn!("自动上屏需要辅助功能权限，未授予 → 只写剪贴板，请手动 ⌘V");
    }
    paste::set_enabled(can_paste);

    println!("\n╭─────────────────────────────────────────────╮");
    println!("│  AgentEar M1 已就绪                          │");
    println!("╰─────────────────────────────────────────────╯");
    println!("  触发键：{}（按一下开始，再按一下停止）", listener.describe());
    println!(
        "  上屏：  {}",
        if can_paste {
            "自动粘贴到当前窗口（不会替你按回车）"
        } else {
            "仅写剪贴板，手动 ⌘V"
        }
    );
    println!("  数据：  {}", store.root().display());
    println!("  退出：  Ctrl+C\n");

    // macOS 的关键约束：Carbon 快捷键和 NSEvent 全局监听都靠 CFRunLoop 派发事件。
    // 主线程必须跑 run loop，否则事件注册成功但永远送不到——这正是最初
    // 「按 Ctrl+Shift+R 毫无反应」的原因。
    // 所以：状态机放工作线程，主线程只负责 run loop。
    let rx = listener.take_receiver();
    std::thread::spawn(move || {
        if let Err(e) = worker(rx, store, asr) {
            log::error!("工作线程退出: {e:#}");
            std::process::exit(1);
        }
    });

    // 菜单栏必须在主线程装，且要在 NSApplication::run() 之前
    let mtm = objc2::MainThreadMarker::new().expect("install 必须在主线程");
    let _tray = tray::install(mtm);
    log::debug!("菜单栏图标已安装");

    log::debug!("主线程进入 AppKit 事件循环……");
    tray::run(mtm)
}

fn worker(
    rx: std::sync::mpsc::Receiver<()>,
    store: store::Store,
    asr: asr::Asr,
) -> Result<()> {
    let mut state = State::Idle;
    let mut last_heartbeat = Instant::now();

    loop {
        // 录音期间持续把采样搬进 session，避免 channel 无限堆积
        if let State::Recording {
            session, recorder, ..
        } = &mut state
        {
            let pcm = recorder.drain();
            if !pcm.is_empty() {
                session.write(&pcm)?;
            }
            // 每秒报一次时长，让「正在录」这件事可见
            tray::set_secs(session.duration_secs() as u32);
            if last_heartbeat.elapsed() >= Duration::from_secs(1) {
                log::info!("● 录音中 {:.0}s", session.duration_secs());
                last_heartbeat = Instant::now();
            }
            if session.duration_secs() > asr::MAX_SEGMENT_SECS {
                log::warn!(
                    "录音超过 {:.0} 秒上限，自动停止（见 ADR-0001 §5）",
                    asr::MAX_SEGMENT_SECS
                );
                state = finish(state, &asr)?;
                continue;
            }
        }

        if rx.try_recv().is_ok() {
            log::debug!("收到触发事件");
            state = match state {
                State::Idle => match begin(&store) {
                    Ok(s) => {
                        last_heartbeat = Instant::now();
                        s
                    }
                    Err(e) => {
                        log::error!("开始录音失败: {e:#}");
                        State::Idle
                    }
                },
                s @ State::Recording { .. } => finish(s, &asr)?,
            };
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn begin(store: &store::Store) -> Result<State> {
    let t0 = Instant::now();
    log::debug!("打开麦克风……（首次运行时 macOS 会在此弹出权限请求）");
    let recorder = audio::Recorder::start()?;
    let session = store.begin()?;
    tray::set(tray::Status::Recording);
    println!("● 开始录音…… 再按一次停止");
    log::debug!("录音启动耗时 {:.0}ms", t0.elapsed().as_secs_f32() * 1000.0);
    Ok(State::Recording {
        session,
        recorder,
        started: Instant::now(),
    })
}

fn finish(state: State, asr: &asr::Asr) -> Result<State> {
    let State::Recording {
        mut session,
        recorder,
        started,
    } = state
    else {
        return Ok(state);
    };

    // 收尾：把停止瞬间还在缓冲里的采样也写进去
    let tail = recorder.drain();
    if !tail.is_empty() {
        session.write(&tail)?;
    }
    drop(recorder);

    let secs = session.duration_secs();
    if secs < 0.3 {
        log::warn!("录音过短（{secs:.1}s），丢弃");
        tray::set(tray::Status::Idle);
        return Ok(State::Idle);
    }
    tray::set(tray::Status::Transcribing);

    // raw 先落盘并走完提交协议，再谈转写。
    // 转写失败不能影响原始音频——这是 README「先存后分流」的执行点。
    let t_commit = Instant::now();
    let committed = session.commit().context("提交 raw 音频失败")?;
    log::debug!(
        "raw 已提交 ({:.0}ms): {}",
        t_commit.elapsed().as_secs_f32() * 1000.0,
        committed.path.display()
    );
    println!("✓ 已保存 {secs:.1}s 录音");

    let t_asr = Instant::now();
    match asr.transcribe(&committed.path) {
        Ok(text) if !text.is_empty() => {
            log::debug!("转写耗时 {:.2}s", t_asr.elapsed().as_secs_f32());
            let text = paste::sanitize(&text);
            println!("\n{text}\n");
            // 先进剪贴板再谈上屏。上屏失败还能手动 ⌘V，顺序反过来就没有退路了。
            match copy_to_clipboard(&text) {
                Ok(()) => {
                    let pasted = if paste::enabled() {
                        match paste::paste() {
                            Ok(()) => " → 已上屏",
                            Err(e) => {
                                log::error!("自动上屏失败（文本仍在剪贴板，可手动 ⌘V）: {e:#}");
                                ""
                            }
                        }
                    } else {
                        ""
                    };
                    println!(
                        "（已复制到剪贴板{pasted}，全程 {:.1}s）\n",
                        started.elapsed().as_secs_f32()
                    );
                }
                Err(e) => log::error!("写剪贴板失败: {e:#}"),
            }
        }
        Ok(_) => log::warn!("转写结果为空（这段音频可能没有语音）"),
        // raw 已经安全落盘，转写失败只是丢了一次派生结果，可以重跑
        Err(e) => log::error!("转写失败（raw 音频已保留，可重试）: {e:#}"),
    }

    tray::set(tray::Status::Idle);
    Ok(State::Idle)
}

/// 环境自检。「按了没反应」时先跑这个。
fn diagnose(vendor: &std::path::Path) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait};

    println!("=== AgentEar 环境自检 ===\n");

    let trusted = hotkey::is_accessibility_trusted();
    println!("辅助功能权限: {}", if trusted { "✅ 已授予" } else { "❌ 未授予" });
    println!(
        "  → 触发键将是: {}",
        if trusted { "右 Command" } else { "Ctrl+Shift+R（降级）" }
    );
    if !trusted {
        println!("  → 想用右 Command：系统设置 → 隐私与安全性 → 辅助功能，勾选本程序后重启");
    }

    println!("\n音频输入设备:");
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(d) => {
            println!("  默认: {}", d.name().unwrap_or_else(|_| "?".into()));
            match d.default_input_config() {
                Ok(c) => println!(
                    "  配置: {} Hz, {} ch, {:?}",
                    c.sample_rate().0,
                    c.channels(),
                    c.sample_format()
                ),
                Err(e) => println!("  ❌ 读取配置失败: {e}（麦克风权限？）"),
            }
        }
        None => println!("  ❌ 找不到输入设备"),
    }

    println!("\nASR 依赖:");
    for (name, p) in [
        ("二进制", vendor.join("bin/llama-funasr-sensevoice")),
        ("模型", vendor.join("models/sensevoice-small-q8.gguf")),
        ("VAD", vendor.join("models/fsmn-vad.gguf")),
    ] {
        let ok = p.exists();
        let size = p.metadata().map(|m| m.len() / 1048576).unwrap_or(0);
        println!(
            "  {} {name}: {} ({} MiB)",
            if ok { "✅" } else { "❌" },
            p.display(),
            size
        );
    }

    println!("\n数据目录: {}", data_root()?.display());
    Ok(())
}

/// 日志同时写 stderr 和 `~/.agentear/agentear.log`。
///
/// 从 Finder / `open` 启动 .app 时 stderr 无处可去，只有文件日志能看到
/// 发生了什么——这对一个没有主窗口的菜单栏程序是刚需。
fn init_logging() {
    struct Tee(std::fs::File);
    impl std::io::Write for Tee {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
            self.0.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let _ = std::io::Write::flush(&mut std::io::stderr());
            self.0.flush()
        }
    }

    let mut b = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug"),
    );
    b.format_timestamp_millis();

    if let Ok(root) = data_root() {
        let _ = std::fs::create_dir_all(&root);
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("agentear.log"))
        {
            b.target(env_logger::Target::Pipe(Box::new(Tee(f))));
        }
    }
    b.init();
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_text(text.to_string())?;
    Ok(())
}

fn data_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AGENTEAR_DATA") {
        return Ok(PathBuf::from(p));
    }
    Ok(dirs::home_dir()
        .context("找不到 home 目录")?
        .join(".agentear"))
}

/// vendor/ 里放 ASR 二进制和模型。
///
/// 查找顺序：环境变量 → .app bundle 内的 Resources → 源码树。
/// 打包后可执行文件在 `AgentEar.app/Contents/MacOS/`，
/// vendor 在 `AgentEar.app/Contents/Resources/vendor`。
fn vendor_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AGENTEAR_VENDOR") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        // .../Contents/MacOS/AgentEar → .../Contents/Resources/vendor
        if let Some(contents) = exe.parent().and_then(|p| p.parent()) {
            let bundled = contents.join("Resources/vendor");
            if bundled.exists() {
                return Ok(bundled);
            }
        }
    }
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor"))
}
