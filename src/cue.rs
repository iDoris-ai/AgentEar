//! 录音提示音（jason 2026-09-26）：按下录音键「嘟」一声 = 话筒开了；
//! 停下「嘟」一声（更低）= 收到了、开始转文字。**让人知道它确实在跑。**
//!
//! | 时刻 | 输入法模式 | 对话模式 |
//! |---|---|---|
//! | 开始 | 一声高音 | 两声高音 |
//! | 结束 | 一声低音 | 两声低音 |
//!
//! ## 双击的时序（这是本模块唯一需要想清楚的地方）
//!
//! 双击的**第一下**到达时还不知道会不会有第二下（`hotkey::intent`
//! 已经按单击开了录音），所以第一下只能响**一声**高音。第二下到达时
//! 是「录音中收到双击 → 只切模式」，这时**再补一声**同样的高音——
//! 用户听到的就是跟着自己手指节奏的「嘟、嘟」，正好是对话模式的开始音。
//! 反过来如果第二下响的是完整的「嘟嘟」，用户会听到三声，
//! 那才是 jason 说的「分不清」。
//!
//! 所以「补一声」（[`Cue::UpgradeToConversation`]）与「开始·输入法」
//! 用的是**同一段波形**，但是**两个 `NSSound` 实例**：手快的时候第二下到达时
//! 第一声可能还没播完，同一个实例会被 `stop()` 掐掉，用户只听到一声。
//! 判据见 [`for_intent`]，有真值表。
//!
//! ## 为什么是进程内生成的正弦波，而不是系统音 / afplay
//!
//! - **不 spawn 进程**：`afplay` 每次起一个子进程，冷启动几十毫秒，
//!   而提示音的价值全在「跟手」。这里用 `NSSound` 在一条专用线程上播，
//!   波形在第一次用时生成一次、之后复用。
//! - **不用 `/System/Library/Sounds`**：系统音的时长、音高、能量都不归我们管，
//!   换个 macOS 版本就可能变；而下面「会不会被录进去」那条要实测的风险，
//!   只有波形固定才测得住。
//!
//! ## 会不会被麦克风录进去
//!
//! 会——开始音和录音几乎同时发生，「补一声」更是**在录音中**响的。
//! 所以波形故意做得**短、纯、无语音共振峰**（单一频率正弦 + 淡入淡出），
//! 并用 `scripts/cue-asr-check.py` 实测把它混进录音后 ASR 不会多出字、
//! 也不会凭空多出一段（数字见 `docs/data/cue-asr-2026-09/`）。
//! ⚠️ 那是**数字混音**的测法，不是「扬声器 → 空气 → 麦克风」的真实声学路径；
//! 后者要真机按键才测得到，边界写在 release notes 里。

use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;

use crate::config::TalkMode;
use crate::hotkey::Intent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cue {
    /// 开始录音（输入法模式）：一声高音。
    StartInput,
    /// 开始录音（对话模式，从空闲直接进来的那种）：两声高音。
    StartConversation,
    /// 录音中收到双击、切到对话模式：**补一声**高音，与第一下凑成「嘟、嘟」。
    UpgradeToConversation,
    /// 结束录音（输入法模式）：一声低音。
    EndInput,
    /// 结束录音（对话模式）：两声低音。
    EndConversation,
}

impl Cue {
    pub const ALL: [Cue; 5] = [
        Cue::StartInput,
        Cue::StartConversation,
        Cue::UpgradeToConversation,
        Cue::EndInput,
        Cue::EndConversation,
    ];

    /// CLI 用的名字（`--cue <name>` / `--cue-wav <name>`）。
    pub fn name(self) -> &'static str {
        match self {
            Cue::StartInput => "start",
            Cue::StartConversation => "start-conversation",
            Cue::UpgradeToConversation => "upgrade",
            Cue::EndInput => "end",
            Cue::EndConversation => "end-conversation",
        }
    }

    pub fn from_name(s: &str) -> Option<Cue> {
        Cue::ALL.into_iter().find(|c| c.name() == s)
    }

    /// 这一声由哪几个音组成：(频率 Hz, 个数)。
    fn shape(self) -> (f32, usize) {
        match self {
            Cue::StartInput | Cue::UpgradeToConversation => (HIGH_HZ, 1),
            Cue::StartConversation => (HIGH_HZ, 2),
            Cue::EndInput => (LOW_HZ, 1),
            Cue::EndConversation => (LOW_HZ, 2),
        }
    }

    /// 整段波形的时长（毫秒）。
    pub fn duration_ms(self) -> u32 {
        let (_, n) = self.shape();
        n as u32 * TONE_MS + (n as u32 - 1) * GAP_MS
    }
}

/// 高音 1320 Hz（E6）、低音 660 Hz（低一个八度）。
/// 差一个八度是为了「不用学就分得出高低」——半音级的差别，人在不经意时听不出来。
const HIGH_HZ: f32 = 1320.0;
const LOW_HZ: f32 = 660.0;
/// 单个音 70ms：够被听见，又短到不会压着用户的第一个字。
const TONE_MS: u32 = 70;
/// 两声之间 60ms 空隙：比这短就糊成一声。
const GAP_MS: u32 = 60;
/// 淡入淡出 8ms：没有它正弦波起止处会有「咔哒」声（阶跃 = 宽带噪声），
/// 而宽带噪声正是 ASR/VAD 最可能当成语音起点的东西。
const FADE_MS: u32 = 8;
/// 峰值幅度（满幅的 30%）。提示音不该比说话声还响。
const AMPLITUDE: f32 = 0.30;

/// 该响哪一声。**纯函数，真值表见测试**。
///
/// - `intent`：这一次按键被判成什么（`hotkey::intent` 的输出）。
/// - `mode_before`：执行这个动作**之前**的模式（`Begin { None }` 与 `End`
///   都按它响——菜单 toggle 不改模式、结束也不改模式）。
pub fn for_intent(intent: Intent, mode_before: TalkMode) -> Option<Cue> {
    match intent {
        Intent::Begin { set_conversation: Some(false) } => Some(Cue::StartInput),
        Intent::Begin { set_conversation: Some(true) } => Some(Cue::StartConversation),
        Intent::Begin { set_conversation: None } => Some(start_for(mode_before)),
        // 已经是对话模式（三连击的第三下）就什么都不响：
        // 再补一声会变成「嘟、嘟、嘟」，而模式其实没变。
        Intent::SwitchToConversation => match mode_before {
            TalkMode::InputMethod => Some(Cue::UpgradeToConversation),
            TalkMode::Conversation => None,
        },
        Intent::End => Some(end_for(mode_before)),
    }
}

pub fn start_for(mode: TalkMode) -> Cue {
    match mode {
        TalkMode::InputMethod => Cue::StartInput,
        TalkMode::Conversation => Cue::StartConversation,
    }
}

pub fn end_for(mode: TalkMode) -> Cue {
    match mode {
        TalkMode::InputMethod => Cue::EndInput,
        TalkMode::Conversation => Cue::EndConversation,
    }
}

/// 生成波形（16-bit 单声道）。`rate` 由调用方定：播放用 44.1 kHz，
/// 实测脚本要 16 kHz 以便直接混进录音。
pub fn samples(cue: Cue, rate: u32) -> Vec<i16> {
    let (hz, n) = cue.shape();
    let per_ms = rate as f32 / 1000.0;
    let tone_len = (TONE_MS as f32 * per_ms) as usize;
    let gap_len = (GAP_MS as f32 * per_ms) as usize;
    let fade_len = ((FADE_MS as f32 * per_ms) as usize).max(1);
    let mut out = Vec::with_capacity(n * tone_len + (n - 1) * gap_len);
    for k in 0..n {
        if k > 0 {
            out.extend(std::iter::repeat_n(0i16, gap_len));
        }
        for i in 0..tone_len {
            let env = if i < fade_len {
                i as f32 / fade_len as f32
            } else if i >= tone_len - fade_len {
                (tone_len - 1 - i) as f32 / fade_len as f32
            } else {
                1.0
            };
            let t = i as f32 / rate as f32;
            let v = (2.0 * std::f32::consts::PI * hz * t).sin() * env * AMPLITUDE;
            out.push((v * i16::MAX as f32) as i16);
        }
    }
    out
}

/// 包成 WAV 字节（`NSSound initWithData:` 与 `--cue-wav` 共用）。
pub fn wav_bytes(cue: Cue, rate: u32) -> Vec<u8> {
    let pcm = samples(cue, rate);
    let data_len = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

const PLAY_RATE: u32 = 44_100;

static TX: OnceLock<Sender<Cue>> = OnceLock::new();

/// 守护进程启动时调一次：起播放线程并**预先构造全部波形**。
///
/// 实测（2026-09-26）：构造全部波形只要 3–4ms，慢的是**进程里第一次 play()**
/// （97–142ms）；之后每次 play() 返回 8.8–27.6ms（n=20），闲置 20s 后约 48ms（n=3）。
/// 不预热的话，每次开机后的**第一下**提示音会比手指晚 0.1 秒——
/// 正好是最需要它跟手的那一下。
pub fn warm_up() {
    sender();
}

fn sender() -> &'static Sender<Cue> {
    TX.get_or_init(|| {
        let (tx, rx) = channel::<Cue>();
        std::thread::Builder::new()
            .name("record-cue".into())
            .spawn(move || player(rx))
            .expect("起提示音线程失败");
        tx
    })
}

/// 响一声。**不阻塞**：投递到专用线程就返回，调用方（录音工作线程）
/// 不会因为提示音晚开麦克风。开关（`record_cue`）由调用方判断——
/// 这里只管播，便于 `--cue` 在关着开关时也能试听。
pub fn play(cue: Cue) {
    if sender().send(cue).is_err() {
        log::warn!("提示音线程已退出，这一声没响");
    }
}

/// 专用线程：持有全部 `NSSound`（它不是 `Send`，只能待在一条线程上），
/// 收到就播。**每次都先 `stop` 再 `play`**：同一个实例还在播时再 `play`
/// 会直接返回 false（没声）。
///
/// ⚠️ 不要改成「`isPlaying()` 为真才 stop」：实测（2026-09-26）这条线程
/// 没有 run loop，200ms 的声音播完 450ms 后 `isPlaying()` 仍然是 true，
/// 读回来的值不可靠。无条件 `stop()` 对没在播的实例是空操作。
/// 输出设备在最后一声之后约 2.5 秒回到空闲（CoreAudio 自己的超时，实测），
/// 所以不会因为这些常驻的 `NSSound` 让声卡一直开着。
fn player(rx: std::sync::mpsc::Receiver<Cue>) {
    use objc2::rc::Retained;
    use objc2::AllocAnyThread;
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;
    use std::collections::HashMap;

    let build = |cue: Cue| -> Option<Retained<NSSound>> {
        let data = NSData::with_bytes(&wav_bytes(cue, PLAY_RATE));
        let s = NSSound::initWithData(NSSound::alloc(), &data);
        // 理论上不会失败（波形是自己生成的合法 WAV）；真失败了就记一条、
        // 这一种提示音不响——提示音坏了不能拖垮录音。
        if s.is_none() {
            log::warn!("提示音 {} 构造失败，这一种不会响", cue.name());
        }
        s
    };
    // 预热：线程一起来就把全部波形构造好（见 `warm_up`）。
    let t_warm = std::time::Instant::now();
    let mut sounds: HashMap<Cue, Retained<NSSound>> = HashMap::new();
    objc2::rc::autoreleasepool(|_| {
        for cue in Cue::ALL {
            if let Some(s) = build(cue) {
                sounds.insert(cue, s);
            }
        }
    });
    // 构造本身只要几毫秒；慢的是**进程里第一次 play()**（实测约 120–140ms，
    // 音频输出链路在这时才建起来）。所以静音地真播一次，把这笔钱在启动时付掉。
    if let Some(s) = sounds.get(&Cue::StartInput) {
        s.setVolume(0.0);
        s.play();
        std::thread::sleep(std::time::Duration::from_millis(
            Cue::StartInput.duration_ms() as u64 + 50,
        ));
        s.stop();
        s.setVolume(1.0);
    }
    log::debug!("提示音预热 {:.0}ms", t_warm.elapsed().as_secs_f64() * 1000.0);
    while let Ok(cue) = rx.recv() {
        objc2::rc::autoreleasepool(|_| {
            let t0 = std::time::Instant::now();
            let Some(sound) = sounds.get(&cue) else {
                return;
            };
            sound.stop();
            let ok = sound.play();
            log::debug!(
                "提示音 {}：play()={ok}，本线程收到→play 返回 {:.1}ms",
                cue.name(),
                t0.elapsed().as_secs_f64() * 1000.0
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{intent, Signal, TapKind};
    use TalkMode::{Conversation as C, InputMethod as I};

    /// 真值表：按键信号 × 是否在录音 × 当前模式 → 响什么。
    /// 走 `hotkey::intent` 再到 `for_intent`，钉的是**整条判定链**，
    /// 不是手写的 Intent。
    #[test]
    fn truth_table() {
        let single = Signal::Tap(TapKind::Single);
        let double = Signal::Tap(TapKind::Double);
        let manual = Signal::ManualToggle;
        let cases = [
            // (信号, 录音中?, 当前模式, 期望)
            (single, false, I, Some(Cue::StartInput)),
            (single, false, C, Some(Cue::StartInput)), // 单击 = 回到输入法
            (single, true, I, Some(Cue::EndInput)),
            (single, true, C, Some(Cue::EndConversation)),
            (double, false, I, Some(Cue::StartConversation)),
            (double, false, C, Some(Cue::StartConversation)),
            (double, true, I, Some(Cue::UpgradeToConversation)),
            (double, true, C, None), // 三连击的第三下：模式没变，不再响
            (manual, false, I, Some(Cue::StartInput)),
            (manual, false, C, Some(Cue::StartConversation)),
            (manual, true, I, Some(Cue::EndInput)),
            (manual, true, C, Some(Cue::EndConversation)),
        ];
        for (sig, rec, mode, want) in cases {
            assert_eq!(for_intent(intent(sig, rec), mode), want, "{sig:?} rec={rec} {mode:?}");
        }
    }

    /// jason 最在意的那条：**从空闲双击**听到的必须正好两声高音，
    /// 不能是「一声 + 两声」的三声。
    #[test]
    fn idle_double_tap_sounds_like_two_high_beeps() {
        let first = for_intent(intent(Signal::Tap(TapKind::Single), false), I).unwrap();
        let second = for_intent(intent(Signal::Tap(TapKind::Double), true), I).unwrap();
        let beeps = |c: Cue| c.shape();
        assert_eq!(beeps(first), (HIGH_HZ, 1));
        assert_eq!(beeps(second), (HIGH_HZ, 1));
        assert_eq!(beeps(Cue::StartConversation), (HIGH_HZ, 2));
    }

    #[test]
    fn start_and_end_differ_in_pitch() {
        assert!(Cue::StartInput.shape().0 > Cue::EndInput.shape().0);
        assert!(Cue::StartConversation.shape().0 > Cue::EndConversation.shape().0);
    }

    #[test]
    fn waveform_length_matches_declared_duration_and_is_short() {
        for cue in Cue::ALL {
            let n = samples(cue, 16_000).len();
            let want = (cue.duration_ms() * 16) as usize;
            assert!(n.abs_diff(want) <= 2, "{cue:?}: {n} vs {want}");
            assert!(cue.duration_ms() <= 200, "{cue:?} 太长会压住第一个字");
        }
    }

    /// 起止必须是 0 附近（淡入淡出生效），否则阶跃会产生「咔哒」宽带噪声。
    #[test]
    fn edges_are_faded() {
        for cue in Cue::ALL {
            let s = samples(cue, 44_100);
            assert!(s.first().unwrap().abs() < 200, "{cue:?} 开头没淡入");
            assert!(s.last().unwrap().abs() < 200, "{cue:?} 结尾没淡出");
            let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(peak as f32 <= i16::MAX as f32 * AMPLITUDE + 1.0);
        }
    }

    #[test]
    fn wav_header_is_well_formed() {
        let b = wav_bytes(Cue::EndConversation, 16_000);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(&b[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(b[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len, b.len() - 44);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize, b.len() - 8);
    }

    #[test]
    fn names_round_trip() {
        for cue in Cue::ALL {
            assert_eq!(Cue::from_name(cue.name()), Some(cue));
        }
        assert_eq!(Cue::from_name("nope"), None);
    }
}
