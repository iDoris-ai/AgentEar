//! 通话会话层的状态机（T3.4.2）。
//!
//! ## 这一层解决什么
//!
//! `talk.rs` 提供的是**原子调用**（问一次 LLM、合成一段语音）。
//! 「一次通话」是这些调用按轮次串起来的生命周期，而**状态归属只能有一个**。
//! ADR-0007 §4.4 把这件事定成二选一：
//!
//! - **A 自主编排**：会话状态机在 R2（这里），R0 只提供原子调用
//! - B 委托编排：整段会话交给上游 `/v1/realtime`
//!
//! **本模块按 A 落地**（ADR 明确倾向 A，但要求 M3.0 正式拍板）。
//! 选 A 的理由是打断语义与持久化耦合紧：打断要掐的是本进程里的播放句柄，
//! 要落的是本进程写的 `raw`/`routes`，把状态机交给一个 HTTP 上游之后，
//! 这两件事都要跨进程回传，而 §4.3 已经算过——每次跨进程往返都在吃
//! 那 300 ms 的打断预算。
//!
//! ## 状态图（V1 打断式半双工）
//!
//! ```text
//!            begin_turn                transcript_ready
//!   Idle ──────────────▶ Listening ──────────────────▶ Thinking
//!    ▲                     │                              │
//!    │              abort  │                     reply_ready│
//!    │                     ▼                              ▼
//!    │                   Idle ◀────────speaking_done── Speaking
//!    │                                                  │  ▲
//!    └──────────────── barge_in ────────────────────────┘  │
//!                        （打断 = 立刻回到 Listening）      │
//! ```
//!
//! ## 措辞纪律（不要写成「体感接近全双工」）
//!
//! 这里实现的是**打断式半双工**：VAD/按键检出用户开口，就掐掉正在放的
//! 声音。它解决「说完才轮到我」，**不解决「一边听一边想」**。
//! 快速来回时用户能感知到延迟（`CLAUDE.md` / ADR-0007 §3）。
//!
//! ## 持久化纪律
//!
//! 通话属于**路径 B（实时流）= 有界丢失**，不是文件导入那种零丢失
//! （ADR-0007 §4.5）。本模块因此只记录「这一段该不该 commit」，
//! 具体策略由 `BatchCommitPolicy` / `StreamCheckpointPolicy` 决定——
//! **不要把文件导入的零丢失语义套到通话上**。

use std::time::{Duration, Instant};

use crate::talk::TalkLang;

/// 通话状态机当前处于哪一相。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// 没在通话。
    Idle,
    /// 正在录这一段。
    Listening,
    /// 录完了，在等 ASR/LLM 的结果（用户此时听到的是沉默）。
    Thinking,
    /// 正在播回答。
    Speaking,
    /// 上一次操作失败了，但通话本身还活着，可以继续下一轮。
    Failed(String),
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Listening => "listening",
            Phase::Thinking => "thinking",
            Phase::Speaking => "speaking",
            Phase::Failed(_) => "failed",
        }
    }

}

/// 一轮的记录。**文字与语音同时产出**（ADR-0007 §4.7），所以两者都留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub index: usize,
    pub lang: TalkLang,
    /// ASR 的结果（用户说了什么）。
    pub heard: String,
    /// LLM 的结果（要念出来的回答）。失败时为 `None`，但 `heard` 仍然有效——
    /// **转写本身不该因为 LLM 挂了而丢失**，它照样进剪贴板和知识库。
    pub reply: Option<String>,
    /// 这一轮实际说了多久（毫秒）。给打断延迟与总耗时的统计用。
    pub spoke_ms: u128,
}

/// 一次通话。
#[derive(Debug)]
pub struct Session {
    phase: Phase,
    lang: TalkLang,
    turns: Vec<Turn>,
    /// 每一轮各自的计时起点，用来算「这一轮花了多久」。
    turn_started: Option<Instant>,
    /// 被用户打断过几次。**这是 V1 的出口判据之一**：
    /// 误打断 < 1 次 / 10 分钟（`docs/benchmarks-m3.md` §7.6）。
    barge_ins: usize,
    interrupted_playbacks: usize,
}

impl Session {
    pub fn new(lang: TalkLang) -> Self {
        Self {
            phase: Phase::Idle,
            lang,
            turns: Vec::new(),
            turn_started: None,
            barge_ins: 0,
            interrupted_playbacks: 0,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn lang(&self) -> TalkLang {
        self.lang
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn barge_ins(&self) -> usize {
        self.barge_ins
    }

    pub fn interrupted_playbacks(&self) -> usize {
        self.interrupted_playbacks
    }

    /// 通话中随时切语言（jason 2026-09-08 的要求）。
    ///
    /// **相位不允许跨越**：允许在 Idle / Listening / Speaking 时切——
    /// 用户在听回答的中途说「换成英文说」，下一轮就该用英文。
    /// 但 Thinking 中途切语言会造成一个自相矛盾的轮次（用户说的是 A 语言、
    /// 回答被要求用 B 语言），所以那一相拒绝，让调用方在下一轮再切。
    pub fn set_lang(&mut self, lang: TalkLang) -> Result<(), String> {
        if matches!(self.phase, Phase::Thinking) {
            return Err("正在推理，语言切换要等这一轮结束（否则这一轮的语言归属不清）".into());
        }
        if self.lang != lang {
            log::info!("通话语言 {} → {}", self.lang.as_str(), lang.as_str());
        }
        self.lang = lang;
        Ok(())
    }

    /// 开始录一段。
    pub fn begin_turn(&mut self) -> Result<(), String> {
        match self.phase {
            Phase::Idle | Phase::Failed(_) => {
                self.turn_started = Some(Instant::now());
                self.phase = Phase::Listening;
                Ok(())
            }
            Phase::Listening => Err("已经在录了，重复按下不再开一段".into()),
            Phase::Thinking => Err("上一轮还在推理，等它出结果".into()),
            Phase::Speaking => {
                // 用户在回答播放期间又按了录音键 —— 这就是 V1 的打断入口。
                // **不要递归调用 `begin_turn`**：`barge_in` 已经把相位推到
                // Listening，再进来一次会撞上「已经在录了」那条拒绝。
                // 这个坑是 `session_state_pressing_record_while_speaking…`
                // 那条用例抓出来的。
                self.barge_in();
                Ok(())
            }
        }
    }

    /// 录音结束，等 ASR/LLM 的结果。
    pub fn finish_listening(&mut self) -> Result<(), String> {
        match &self.phase {
            Phase::Listening => {
                self.phase = Phase::Thinking;
                Ok(())
            }
            other => Err(format!("当前是 {}，不在录音中", other.as_str())),
        }
    }

    /// 拿到转写、也拿到回答（或回答失败），进入播放。
    ///
    /// **`reply` 为 `None` 也照样记一轮**：用户说的话已经转写出来了，
    /// 那是有价值的记录，不能因为 LLM 边车没起就丢。
    pub fn turn_ready(&mut self, heard: impl Into<String>, reply: Option<String>) -> Result<(), String> {
        if !matches!(self.phase, Phase::Thinking) {
            return Err(format!("当前是 {}，不该有轮次结果", self.phase.as_str()));
        }
        let heard = heard.into();
        if heard.trim().is_empty() {
            // 空转写 = 这一段没人说话。**不记轮次**，直接回到空闲，
            // 否则 10 分钟的静音会攒出一堆空轮次。
            self.turn_started = None;
            self.phase = Phase::Idle;
            return Ok(());
        }
        let index = self.turns.len() + 1;
        let lang = self.lang;
        self.turns.push(Turn {
            index,
            lang,
            heard,
            reply,
            spoke_ms: 0,
        });
        // 没有回答就没有播放，直接回到空闲等下一轮。
        self.phase = if self.turns.last().and_then(|t| t.reply.as_ref()).is_some() {
            Phase::Speaking
        } else {
            Phase::Idle
        };
        Ok(())
    }

    /// 播完了。记下这一轮的实际时长。
    pub fn speaking_done(&mut self, spoke: Duration) -> Result<(), String> {
        if !matches!(self.phase, Phase::Speaking) {
            return Err(format!("当前是 {}，没有在播", self.phase.as_str()));
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.spoke_ms = spoke.as_millis();
        }
        self.phase = Phase::Idle;
        self.turn_started = None;
        Ok(())
    }

    /// 用户开口打断。**这是 V1 的核心动作**：立刻掐掉播放、回到 Listening。
    ///
    /// 返回 `true` 表示确实有一次正在播的声音被掐掉了——调用方据此决定
    /// 要不要真的去 kill 播放子进程（没在播就别去动它）。
    pub fn barge_in(&mut self) -> bool {
        self.barge_ins += 1;
        let was_speaking = matches!(self.phase, Phase::Speaking);
        if was_speaking {
            self.interrupted_playbacks += 1;
            self.phase = Phase::Listening;
            self.turn_started = Some(Instant::now());
            log::info!("用户打断，掐掉播放，进入下一轮");
        } else if matches!(self.phase, Phase::Thinking) {
            // 推理中途用户又说话了：本轮结果降级为「已作废」但事实仍要留。
            // 这里不丢数据，只把相位推回 Listening —— 那段转写该不该上屏
            // 由调用方按 `turn_ready` 的语义处理。
            self.phase = Phase::Listening;
            self.turn_started = Some(Instant::now());
        }
        was_speaking
    }

    /// 出错：通话还活着，只是这一轮失败了。
    pub fn fail(&mut self, why: impl Into<String>) {
        let why = why.into();
        log::warn!("通话这一轮失败：{why}");
        self.turn_started = None;
        self.phase = Phase::Failed(why);
    }

    /// 结束通话。
    ///
    /// ⚠️ **目前只有测试在调它**（所以标了 `allow`）。T3.4.2 要的是
    /// 「打电话式入口（起/停一次通话）」，而**产品侧还没有那个按钮/菜单项**
    /// ——这轮打通的是链路，入口没做。留着这个方法是让「挂断」有确定的语义，
    /// 而不是让入口出现时再补一个含义暧昧的 `reset()`。
    #[allow(dead_code)]
    pub fn hang_up(&mut self) -> Result<(), String> {
        if matches!(self.phase, Phase::Listening | Phase::Thinking) {
            return Err("还在录音或推理，先让它收尾再挂断".into());
        }
        self.phase = Phase::Idle;
        self.turn_started = None;
        Ok(())
    }

    /// 这一轮从按下到现在的时长。
    pub fn turn_elapsed(&self) -> Option<Duration> {
        self.turn_started.map(|t| t.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::new(TalkLang::Zh)
    }

    /// T3.4.2 的验收命令点名要这条：`cargo test session_state`。
    #[test]
    fn session_state_walks_the_v1_cycle() {
        let mut s = session();
        assert_eq!(s.phase(), &Phase::Idle);

        s.begin_turn().unwrap();
        assert_eq!(s.phase(), &Phase::Listening);

        s.finish_listening().unwrap();
        assert_eq!(s.phase(), &Phase::Thinking);

        s.turn_ready("今天天气怎么样", Some("今天清迈多云".to_string())).unwrap();
        assert_eq!(s.phase(), &Phase::Speaking);

        s.speaking_done(Duration::from_millis(2400)).unwrap();
        assert_eq!(s.phase(), &Phase::Idle);
        assert_eq!(s.turns().len(), 1);
        assert_eq!(s.turns()[0].spoke_ms, 2400);
    }

    /// 验收点名的第二条：「通话中切语言」。
    #[test]
    fn session_state_language_can_switch_mid_call() {
        let mut s = session();
        s.begin_turn().unwrap();
        s.finish_listening().unwrap();
        s.turn_ready("今天天气怎么样", Some("今天清迈多云".to_string())).unwrap();
        assert_eq!(s.phase(), &Phase::Speaking);

        // 用户在听回答的中途说「换成泰语」
        s.set_lang(TalkLang::Th).unwrap();
        assert_eq!(s.lang(), TalkLang::Th);

        s.speaking_done(Duration::from_millis(100)).unwrap();
        s.begin_turn().unwrap();
        s.finish_listening().unwrap();
        s.turn_ready("วันนี้อากาศเป็นอย่างไร", Some("อากาศดี".to_string())).unwrap();
        let turns = s.turns();
        assert_eq!(turns[0].lang, TalkLang::Zh, "历史轮次的语言不能被改写");
        assert_eq!(turns[1].lang, TalkLang::Th, "新轮次用新语言");
    }

    /// 推理中途不许切语言：那一轮「用户说的语言」和「要求回答的语言」
    /// 会互相矛盾，宁可让调用方晚一轮再切。
    #[test]
    fn session_state_language_switch_is_refused_while_thinking() {
        let mut s = session();
        s.begin_turn().unwrap();
        s.finish_listening().unwrap();
        assert!(s.set_lang(TalkLang::En).is_err());
        assert_eq!(s.lang(), TalkLang::Zh, "被拒之后语言不能变");
    }

    /// 验收点名的第三条：「打断后恢复行为一致」。
    ///
    /// ⚠️ 「一致」的精确含义是：**从打断留下的状态往后，状态机的行为与
    /// 正常收尾后完全一样**——轮次记录形状相同、每一次转移相同、最终收敛到
    /// 同一处。不一致的只有「谁发起了这一段录音」：正常收尾后要再按一次键
    /// （`begin_turn`），而打断本身就已经在录了，这时再按一次会被正确地
    /// 拒绝（那是重复按下，不是 bug）。
    #[test]
    fn session_state_recovers_identically_after_barge_in() {
        let mut normal = session();
        normal.begin_turn().unwrap();
        normal.finish_listening().unwrap();
        normal.turn_ready("一", Some("答一".to_string())).unwrap();
        normal.speaking_done(Duration::from_millis(500)).unwrap();
        assert_eq!(normal.phase(), &Phase::Idle);
        normal.begin_turn().unwrap(); // 正常路径：再按一次才开始录

        let mut interrupted = session();
        interrupted.begin_turn().unwrap();
        interrupted.finish_listening().unwrap();
        interrupted.turn_ready("一", Some("答一".to_string())).unwrap();
        // 说到一半用户开口
        assert!(interrupted.barge_in(), "正在播时打断要报告 true");
        assert_eq!(interrupted.phase(), &Phase::Listening);
        assert_eq!(interrupted.turns().len(), 1, "被打断的那一轮仍然算一轮");
        assert!(
            interrupted.begin_turn().is_err(),
            "打断已经开了一段录音，再按一次必须被拒绝（否则会同时开两段）"
        );

        // 从这一刻起，两条路径的每一次转移都必须一样
        for s in [&mut normal, &mut interrupted] {
            assert_eq!(s.phase(), &Phase::Listening);
            s.finish_listening().unwrap();
            assert_eq!(s.phase(), &Phase::Thinking);
            s.turn_ready("二", Some("答二".to_string())).unwrap();
            assert_eq!(s.phase(), &Phase::Speaking);
            s.speaking_done(Duration::from_millis(500)).unwrap();
            assert_eq!(s.phase(), &Phase::Idle);
            assert_eq!(s.turns().len(), 2);
            assert_eq!(s.turns()[1].heard, "二");
            assert_eq!(s.turns()[1].reply.as_deref(), Some("答二"));
            assert_eq!(s.turns()[1].spoke_ms, 500);
        }
        assert_eq!(interrupted.turns()[0].spoke_ms, 0, "被打断的轮次没有播完时长");
        assert_eq!(interrupted.interrupted_playbacks(), 1);
    }

    /// 没在播的时候打断不该被当成「掐掉了声音」——否则统计出来的
    /// 打断延迟会包含一堆根本没播的轮次。
    #[test]
    fn session_state_barge_in_only_interrupts_when_speaking() {
        let mut s = session();
        assert!(!s.barge_in(), "空闲时没有声音可掐");
        s.begin_turn().unwrap();
        assert!(!s.barge_in(), "录音时没有声音可掐");
        assert_eq!(s.phase(), &Phase::Listening);
    }

    /// 播放中再按录音键 = 打断 + 开新的一段，不能报错。
    #[test]
    fn session_state_pressing_record_while_speaking_barges_in_and_listens() {
        let mut s = session();
        s.begin_turn().unwrap();
        s.finish_listening().unwrap();
        s.turn_ready("一", Some("答一".to_string())).unwrap();
        s.begin_turn().unwrap();
        assert_eq!(s.phase(), &Phase::Listening);
        assert_eq!(s.interrupted_playbacks(), 1);
    }

    /// 空转写（静音段）不记轮次，直接回空闲。
    #[test]
    fn session_state_silence_does_not_become_a_turn() {
        let mut s = session();
        s.begin_turn().unwrap();
        s.finish_listening().unwrap();
        s.turn_ready("   ", Some("不该出现的回答".to_string())).unwrap();
        assert_eq!(s.phase(), &Phase::Idle);
        assert!(s.turns().is_empty());
    }

    /// LLM 挂了不能把「用户说了什么」一起丢掉。
    #[test]
    fn session_state_failed_reply_still_records_the_transcript() {
        let mut s = session();
        s.begin_turn().unwrap();
        s.finish_listening().unwrap();
        s.turn_ready("今天天气怎么样", None).unwrap();
        assert_eq!(s.phase(), &Phase::Idle, "没有回答就没有播放");
        assert_eq!(s.turns().len(), 1);
        assert_eq!(s.turns()[0].heard, "今天天气怎么样");
        assert_eq!(s.turns()[0].reply, None);
    }

    #[test]
    fn session_state_rejects_illegal_transitions() {
        let mut s = session();
        assert!(s.finish_listening().is_err(), "没开始录就不能结束");
        assert!(s.turn_ready("x", None).is_err(), "没在推理就不该有结果");
        assert!(s.speaking_done(Duration::ZERO).is_err(), "没在播就没有播完");
        s.begin_turn().unwrap();
        assert!(s.begin_turn().is_err(), "重复按下不该开第二段");
        assert!(s.hang_up().is_err(), "录音中不许挂断");
    }

    /// 失败之后通话还在，能继续下一轮——不该要求用户重启进程。
    #[test]
    fn session_state_recovers_after_a_failure() {
        let mut s = session();
        s.begin_turn().unwrap();
        s.fail("LLM 边车没起");
        assert!(matches!(s.phase(), Phase::Failed(_)));
        s.begin_turn().unwrap();
        assert_eq!(s.phase(), &Phase::Listening);
    }
}
