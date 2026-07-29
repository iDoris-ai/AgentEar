//! AgentEar M1：按快捷键录音 → raw 落盘 → 转写 → 剪贴板。
//!
//! 范围严格限定在 `docs/milestones.md` 的 M1：不含 LLM、标签路由、TTS。
//! M1 恰好绕开了 AEC 和无边界流式 raw 语义两个难点——快捷键的按下/再按
//! 天然给出段边界，每次录音就是一个有头有尾的文件对象。**不要在 M1 里
//! 提前引入 TTS 或无边界流。**

mod asr;
mod audio;
mod store;

use anyhow::{Context, Result};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let data_root = data_root()?;
    let vendor = vendor_root()?;

    let asr = asr::Asr::new(&vendor)?;

    // 离线转写一个已有的 wav，用于验证 ASR 链路而不占用麦克风
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--transcribe" {
        let t0 = Instant::now();
        let text = asr.transcribe(std::path::Path::new(&args[2]))?;
        println!("{text}");
        eprintln!("（耗时 {:.2}s）", t0.elapsed().as_secs_f32());
        return Ok(());
    }

    let store = store::Store::open(&data_root)?;
    log::info!("数据目录: {}", store.root().display());

    // Carbon RegisterEventHotKey —— 不需要辅助功能权限
    let manager = GlobalHotKeyManager::new().context("注册全局快捷键失败")?;
    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
    manager.register(hotkey).context("注册 Ctrl+Shift+R 失败")?;

    println!("AgentEar M1 已就绪。按 Ctrl+Shift+R 开始/停止录音，Ctrl+C 退出。");

    let rx = GlobalHotKeyEvent::receiver();
    let mut state = State::Idle;

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
            if session.duration_secs() > asr::MAX_SEGMENT_SECS {
                log::warn!(
                    "录音超过 {:.0} 秒上限，自动停止（见 ADR-0001 §5）",
                    asr::MAX_SEGMENT_SECS
                );
                state = finish(state, &asr)?;
                continue;
            }
        }

        match rx.try_recv() {
            Ok(ev) if ev.state == HotKeyState::Pressed => {
                state = match state {
                    State::Idle => match begin(&store) {
                        Ok(s) => s,
                        Err(e) => {
                            log::error!("开始录音失败: {e:#}");
                            State::Idle
                        }
                    },
                    s @ State::Recording { .. } => finish(s, &asr)?,
                };
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn begin(store: &store::Store) -> Result<State> {
    let recorder = audio::Recorder::start()?;
    let session = store.begin()?;
    println!("● 录音中…… 再按一次 Ctrl+Shift+R 停止");
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
    let committed = session.commit().context("提交 raw 音频失败")?;
    println!(
        "✓ 已保存 {:.1}s → {}",
        secs,
        committed.path.file_name().unwrap().to_string_lossy()
    );

    match asr.transcribe(&committed.path) {
        Ok(text) if !text.is_empty() => {
            println!("\n{text}\n");
            match copy_to_clipboard(&text) {
                Ok(()) => println!(
                    "（已复制到剪贴板，全程 {:.1}s）",
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
