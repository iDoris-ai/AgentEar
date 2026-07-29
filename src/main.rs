//! AgentEar M1：按快捷键录音 → raw 落盘 → 转写 → 剪贴板。
//!
//! 范围严格限定在 `docs/milestones.md` 的 M1：不含 LLM、标签路由、TTS。
//! M1 恰好绕开了 AEC 和无边界流式 raw 语义两个难点——快捷键的按下/再按
//! 天然给出段边界，每次录音就是一个有头有尾的文件对象。**不要在 M1 里
//! 提前引入 TTS 或无边界流。**

mod asr;
mod audio;
mod hotkey;
mod store;

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
    // 默认 debug 级别：M1 阶段需要看得见每一步在干什么
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .format_timestamp_millis()
        .init();

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

    let data_root = data_root()?;
    let store = store::Store::open(&data_root)?;
    log::info!("数据目录: {}", store.root().display());

    // 权限引导：想用右 Command 就必须有辅助功能权限
    if !hotkey::is_accessibility_trusted() {
        println!("\n⚠️  未获得「辅助功能」权限，无法监听单独的右 Command 键。");
        println!("    正在弹出系统授权对话框——授权后请重启本程序。");
        println!("    路径：系统设置 → 隐私与安全性 → 辅助功能\n");
        hotkey::prompt_accessibility();
    }

    let mut listener = hotkey::Listener::start()?;

    println!("\n╭─────────────────────────────────────────────╮");
    println!("│  AgentEar M1 已就绪                          │");
    println!("╰─────────────────────────────────────────────╯");
    println!("  触发键：{}（按一下开始，再按一下停止）", listener.describe());
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

    log::debug!("主线程进入 CFRunLoop，等待按键事件……");
    core_foundation::runloop::CFRunLoop::run_current();
    Ok(())
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
        return Ok(State::Idle);
    }

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
            println!("\n{text}\n");
            match copy_to_clipboard(&text) {
                Ok(()) => println!(
                    "（已复制到剪贴板，全程 {:.1}s）\n",
                    started.elapsed().as_secs_f32()
                ),
                Err(e) => log::error!("写剪贴板失败: {e:#}"),
            }
        }
        Ok(_) => log::warn!("转写结果为空（这段音频可能没有语音）"),
        // raw 已经安全落盘，转写失败只是丢了一次派生结果，可以重跑
        Err(e) => log::error!("转写失败（raw 音频已保留，可重试）: {e:#}"),
    }

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

/// vendor/ 里放 ASR 二进制和模型。开发时用仓库内的，
/// 打包成 .app 之后应改为读 bundle 内的 Resources。
fn vendor_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AGENTEAR_VENDOR") {
        return Ok(PathBuf::from(p));
    }
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor"))
}
