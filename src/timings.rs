//! 每轮分段耗时（v0.26.0，jason 2026-09-27）。
//!
//! 本地模型的两大卖点是**隐私**和**快**，所以每一轮都要能看到「慢在哪一段」，
//! 并且**落盘积累**，作为以后定位慢点的依据。一轮 = 松开录音键之后发生的全部事情：
//!
//! ```text
//! 松键 ─ASR→ 转写 ─LLM→ 首 token … 全文 ─TTS→ 首段音频 ─播放→ 播完
//!  t0     asr_ms        llm_first_ms  llm_ms   tts_first_ms       play_ms
//!  └──────────────── to_first_audio_ms（说完到听到）───┘
//! ```
//!
//! ## 三条硬规矩
//!
//! 1. **只存数字与枚举，绝不存内容**：不存转写、不存回答、不存音频路径。
//!    `derived/timings.jsonl` 和随 `turn` 事件报给宿主的 `timings` 都是如此（有测试钉住）。
//! 2. **缺了的段就省略，不填 0**：0 ms 是一个「很快」的测量值，
//!    拿它代替「没测到」会把统计的中位数往下拉——那是在报喜。
//! 3. **计时不能影响这一轮**：拿不到锁、写不了盘，一律只记日志，绝不让回答失败。
//!
//! ## 为什么是全局时钟 + 线程绑定
//!
//! 守护进程里「松键」发生在 worker 线程，回答在另开的线程里、LLM 生产者又是一条线程。
//! 把计时器穿过这些函数签名会让 `talk.rs` 的每个入口都多一个参数，
//! 所以这里用一个全局「当前这一轮」的时钟，每条线程**绑定**自己属于哪一轮（`bind`）：
//! 播放中按键打断、新一轮已经开始时，旧线程迟到的打点会因为轮次号对不上而被丢弃，
//! 不会污染新一轮。
use serde::Serialize;
use std::cell::Cell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 落盘文件超过这个大小就滚动一次（`timings.jsonl` → `timings.1.jsonl`，旧的 .1 被覆盖）。
/// 一行约 300 字节，10 MB ≈ 3 万轮，足够回看好几个月；再多就是在无限增长。
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// 一轮里靠「时刻」打点的几个节点（相对松键时刻 t0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    AsrDone,
    LlmFirst,
    LlmDone,
    AudioFirst,
    PlayEnd,
}

impl Stage {
    fn idx(self) -> usize {
        self as usize
    }
}

/// 一轮的分段耗时。**所有字段都是数字或枚举**——这是隐私边界，不是风格偏好。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Timings {
    /// 录音时长（按下到松开）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ms: Option<u64>,
    /// 松键 → 转写出来（含 raw 落盘）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_ms: Option<u64>,
    /// 转写出来 → 模型第一个字（只有流式才有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_first_ms: Option<u64>,
    /// 转写出来 → 模型全文返回。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_ms: Option<u64>,
    /// 第一段 TTS 合成本身花的时间。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_first_ms: Option<u64>,
    /// 松键 → 第一声（端到端「说完到听到」）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_first_audio_ms: Option<u64>,
    /// 第一声 → 播完（被打断时是实际播出的长度）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_ms: Option<u64>,
    /// 松键 → 这一轮结束。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
    /// 推理走哪条路：`sidecar` / `idoris` / `agent24`（其他引擎如 `mock` 原样报名字）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_via: Option<String>,
    /// 实际回答的模型（宿主推理带 model_id；本机边车拿不到就省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `local` / `remote`（只有宿主推理才知道）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// `builtin` / `speech_swift`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    /// 这一轮的播放被按键打断过。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

/// 落盘的一行：`Timings` 再加时间戳、模式、结局。**仍然没有任何内容字段。**
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// Unix 毫秒。
    pub ts_ms: u64,
    /// `conversation` / `input_method`。
    pub mode: &'static str,
    /// `ok` / `failed` / `empty` / `asr_failed` / `command` / `superseded`。
    pub outcome: &'static str,
    #[serde(flatten)]
    pub timings: Timings,
}

/// 纯函数：由各节点相对 t0 的偏移算出分段。
///
/// 两端有一端缺，这一段就是 `None`；后一端早于前一端（不该发生，时钟来自不同线程时
/// 也可能差几微秒）同样当 `None`——**宁可说没测到，也不报一个负数或 0**。
pub fn compute(
    marks: &[Option<Duration>; 5],
    record_ms: Option<u64>,
    tts_first_ms: Option<u64>,
    total: Duration,
) -> Timings {
    let at = |s: Stage| marks[s.idx()];
    let span = |from: Option<Duration>, to: Option<Duration>| -> Option<u64> {
        match (from, to) {
            (Some(a), Some(b)) if b >= a => Some((b - a).as_millis() as u64),
            _ => None,
        }
    };
    let zero = Some(Duration::ZERO);
    Timings {
        record_ms,
        asr_ms: span(zero, at(Stage::AsrDone)),
        llm_first_ms: span(at(Stage::AsrDone), at(Stage::LlmFirst)),
        llm_ms: span(at(Stage::AsrDone), at(Stage::LlmDone)),
        tts_first_ms,
        to_first_audio_ms: span(zero, at(Stage::AudioFirst)),
        play_ms: span(at(Stage::AudioFirst), at(Stage::PlayEnd)),
        total_ms: Some(total.as_millis() as u64),
        ..Default::default()
    }
}

struct Clock {
    id: u64,
    t0: Instant,
    mode: &'static str,
    record_ms: Option<u64>,
    marks: [Option<Duration>; 5],
    tts_first_ms: Option<u64>,
    meta: Timings,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static LAST: Mutex<Option<Timings>> = Mutex::new(None);
static FILE: OnceLock<PathBuf> = OnceLock::new();

thread_local! {
    /// 这条线程在为哪一轮干活（0 = 没绑定：**打点一律丢弃**——
    /// 没人开过表的调用（`--say`、单元测试里的一轮）不该悄悄落到别人的那一轮上）。
    static BOUND: Cell<u64> = const { Cell::new(0) };
}

/// 测试专用：会开表（`begin_at`）的测试之间互斥——全局时钟只有一个，
/// 并行跑的两条测试会互相把对方的那一轮顶掉。
#[cfg(test)]
pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static G: Mutex<()> = Mutex::new(());
    G.lock().unwrap_or_else(|p| p.into_inner())
}

/// 启动时调一次：之后的每一轮追加写 `<数据目录>/derived/timings.jsonl`。
/// 不调（单元测试、某些 CLI）就只算不写。
pub fn init(data_root: &std::path::Path) {
    let _ = FILE.set(data_root.join("derived").join("timings.jsonl"));
}

fn lock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// 一轮开始计时（t0 = 松键那一刻）。上一轮如果还没收尾（被新一轮顶掉），
/// 先按 `superseded` 收掉并落盘——它的数据是真的，只是没有走完。
///
/// 返回这一轮的编号，并把**当前线程**绑定到它。
pub fn begin_at(t0: Instant, mode: &'static str, record_ms: Option<u64>, asr_backend: &str) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let prev = {
        let mut g = lock();
        let prev = g.take();
        *g = Some(Clock {
            id,
            t0,
            mode,
            record_ms,
            marks: [None; 5],
            tts_first_ms: None,
            meta: Timings {
                asr_backend: Some(asr_backend.to_string()),
                ..Default::default()
            },
        });
        prev
    };
    if let Some(p) = prev {
        seal(p, "superseded", Instant::now());
    }
    bind(id);
    id
}

/// 这条线程为第 `id` 轮干活（回答线程、LLM 生产者线程开工前调）。
pub fn bind(id: u64) {
    BOUND.with(|b| b.set(id));
}

/// 当前线程绑定的轮次（给要再开线程的地方往下传）。
pub fn bound() -> u64 {
    BOUND.with(|b| b.get())
}

/// 当前那一轮的编号（没有就 0）。
pub fn current() -> u64 {
    lock().as_ref().map(|c| c.id).unwrap_or(0)
}

/// 在属于本线程的那一轮上做点事；轮次对不上（旧线程迟到）就什么都不做。
fn with_mine(f: impl FnOnce(&mut Clock)) {
    let mine = bound();
    let mut g = lock();
    if let Some(c) = g.as_mut() {
        if mine != 0 && mine == c.id {
            f(c);
        }
    }
}

/// 打一个节点。**先到的为准**（第一个 token、第一段音频只算第一次）。
pub fn mark(stage: Stage) {
    let now = Instant::now();
    with_mine(|c| {
        let slot = &mut c.marks[stage.idx()];
        if slot.is_none() {
            *slot = Some(now.saturating_duration_since(c.t0));
        }
    });
}

/// 第一段 TTS 合成花了多久（只记第一次）。
pub fn note_tts_first(d: Duration) {
    with_mine(|c| {
        if c.tts_first_ms.is_none() {
            c.tts_first_ms = Some(d.as_millis() as u64);
        }
    });
}

/// 推理走的路（每轮开头按配置/附着状态定）。
pub fn set_via(via: &str) {
    with_mine(|c| c.meta.llm_via = Some(via.to_string()));
}

/// 宿主推理回来时知道的：模型、tier、token 数。
pub fn set_llm_reply(model: &str, tier: &str, prompt_tokens: u64, completion_tokens: u64) {
    with_mine(|c| {
        c.meta.model = Some(model.to_string());
        c.meta.tier = Some(tier.to_string());
        c.meta.prompt_tokens = Some(prompt_tokens);
        c.meta.completion_tokens = Some(completion_tokens);
    });
}

/// 这一轮的播放被打断过。
pub fn note_interrupted() {
    with_mine(|c| c.meta.interrupted = true);
}

/// 收尾：算分段、落盘、记日志、留给菜单栏。返回算好的分段（给 `turn` 事件用）。
///
/// 只有**属于本线程的那一轮**才会被收；轮次对不上返回 `None`（已经被新一轮顶掉并落过盘了）。
pub fn finish(outcome: &'static str) -> Option<Timings> {
    let mine = bound();
    let clock = {
        let mut g = lock();
        match g.as_ref() {
            Some(c) if mine != 0 && mine == c.id => g.take(),
            _ => None,
        }
    }?;
    Some(seal(clock, outcome, Instant::now()))
}

fn seal(c: Clock, outcome: &'static str, end: Instant) -> Timings {
    let mut t = compute(
        &c.marks,
        c.record_ms,
        c.tts_first_ms,
        end.saturating_duration_since(c.t0),
    );
    let m = c.meta;
    t.llm_via = m.llm_via;
    t.model = m.model;
    t.tier = m.tier;
    t.asr_backend = m.asr_backend;
    t.prompt_tokens = m.prompt_tokens;
    t.completion_tokens = m.completion_tokens;
    t.interrupted = m.interrupted;
    log::info!("{}", summary_line(c.mode, outcome, &t));
    let rec = Record {
        ts_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        mode: c.mode,
        outcome,
        timings: t.clone(),
    };
    if let Some(path) = FILE.get() {
        if let Err(e) = append(path, &rec) {
            log::warn!("分段耗时没写进 {}：{e}（不影响这一轮）", path.display());
        }
    }
    // 菜单栏只展示「真的出了声」的那一轮：失败/空转写的数字会让用户以为「就这么慢」。
    if t.to_first_audio_ms.is_some() {
        *LAST.lock().unwrap_or_else(|p| p.into_inner()) = Some(t.clone());
    }
    t
}

/// 追加一行；超过 [`ROTATE_BYTES`] 先滚动。
pub fn append(path: &std::path::Path, rec: &Record) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).map(|m| m.len() >= ROTATE_BYTES).unwrap_or(false) {
        let rolled = path.with_extension("1.jsonl");
        std::fs::rename(path, rolled)?;
    }
    let mut line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

fn secs(ms: Option<u64>) -> String {
    ms.map(|v| format!("{:.2}s", v as f64 / 1000.0)).unwrap_or_else(|| "—".into())
}

/// 日志里那一行紧凑摘要。
pub fn summary_line(mode: &str, outcome: &str, t: &Timings) -> String {
    format!(
        "本轮耗时（{mode}/{outcome}）：说完→出声 {}｜ASR {}｜模型 首字 {} 全文 {}｜合成首段 {}｜播放 {}｜合计 {}{}{}",
        secs(t.to_first_audio_ms),
        secs(t.asr_ms),
        secs(t.llm_first_ms),
        secs(t.llm_ms),
        secs(t.tts_first_ms),
        secs(t.play_ms),
        secs(t.total_ms),
        t.llm_via.as_deref().map(|v| format!("｜经 {v}")).unwrap_or_default(),
        t.model.as_deref().map(|m| format!("·{m}")).unwrap_or_default(),
    )
}

/// 菜单栏那一行：「上一轮：说完→出声 X.XXs（ASR a / 模型 b / 合成 c）」（按界面语言）。
/// 还没有出过声的一轮时返回 `None`（菜单里就不显示这一行）。
pub fn menu_line(t: &Timings, lang: crate::i18n::Lang) -> Option<String> {
    use crate::i18n::Lang;
    let first = t.to_first_audio_ms? as f64 / 1000.0;
    let (asr, llm, tts) = (secs(t.asr_ms), secs(t.llm_ms.or(t.llm_first_ms)), secs(t.tts_first_ms));
    Some(match lang {
        Lang::Zh => format!("上一轮：说完→出声 {first:.2}s（ASR {asr} / 模型 {llm} / 合成 {tts}）"),
        Lang::En => format!("Last turn: done speaking→voice {first:.2}s (ASR {asr} / model {llm} / TTS {tts})"),
        Lang::Th => format!("รอบล่าสุด: พูดจบ→ได้ยิน {first:.2}s (ASR {asr} / โมเดล {llm} / สังเคราะห์ {tts})"),
    })
}

/// 最近一轮出过声的分段（菜单栏读）。
pub fn last() -> Option<Timings> {
    LAST.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

// ------------------------------------------------------------------ 统计（--timings）

/// 读回落盘记录（只读当前文件；坏行跳过）。
pub fn read_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect()
}

/// 中位数 / p90 / 最大值（最近邻取秩，不插值——样本少时插值会编出一个没测到过的数）。
pub fn stats(mut v: Vec<u64>) -> Option<(u64, u64, u64, usize)> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    let rank = |q: f64| v[(((q * n as f64).ceil() as usize).max(1) - 1).min(n - 1)];
    Some((rank(0.5), rank(0.9), v[n - 1], n))
}

pub const REPORT_FIELDS: &[&str] = &[
    "to_first_audio_ms",
    "asr_ms",
    "llm_first_ms",
    "llm_ms",
    "tts_first_ms",
    "play_ms",
    "total_ms",
];

/// `--timings` 的报表：按（llm_via, model）分组，每段报中位数 / p90 / 最大值与样本数。
/// 只报这三个数——仓库规矩：**不报一个好看的区间**。
pub fn report(records: &[serde_json::Value], last_n: Option<usize>) -> String {
    let rs: Vec<&serde_json::Value> = match last_n {
        Some(n) if n < records.len() => records[records.len() - n..].iter().collect(),
        _ => records.iter().collect(),
    };
    let mut groups: std::collections::BTreeMap<String, Vec<&serde_json::Value>> = Default::default();
    for r in &rs {
        let via = r["llm_via"].as_str().unwrap_or("-");
        let model = r["model"].as_str().unwrap_or("-");
        let mode = r["mode"].as_str().unwrap_or("-");
        groups.entry(format!("{mode} · {via} · {model}")).or_default().push(r);
    }
    let mut out = format!("共 {} 轮（中位数 / p90 / 最大值，毫秒；n = 有这一段数据的轮数）\n", rs.len());
    for (k, g) in groups {
        out.push_str(&format!("\n[{k}] {} 轮\n", g.len()));
        for f in REPORT_FIELDS {
            let v: Vec<u64> = g.iter().filter_map(|r| r[*f].as_u64()).collect();
            if let Some((p50, p90, max, n)) = stats(v) {
                out.push_str(&format!("  {f:<18} {p50:>7} / {p90:>7} / {max:>7}   n={n}\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(ms: u64) -> Option<Duration> {
        Some(Duration::from_millis(ms))
    }

    #[test]
    fn compute_derives_each_segment_from_marks() {
        // 松键 t0；ASR 300；首字 800；全文 1500；首声 2600；播完 5600；合计 5700
        let marks = [d(300), d(800), d(1500), d(2600), d(5600)];
        let t = compute(&marks, Some(4200), Some(1100), Duration::from_millis(5700));
        assert_eq!(t.record_ms, Some(4200));
        assert_eq!(t.asr_ms, Some(300));
        assert_eq!(t.llm_first_ms, Some(500));
        assert_eq!(t.llm_ms, Some(1200));
        assert_eq!(t.tts_first_ms, Some(1100));
        assert_eq!(t.to_first_audio_ms, Some(2600));
        assert_eq!(t.play_ms, Some(3000));
        assert_eq!(t.total_ms, Some(5700));
    }

    #[test]
    fn missing_segments_are_omitted_not_zero() {
        // 输入法模式：只有 ASR；非流式：没有首字
        let marks = [d(250), None, None, None, None];
        let t = compute(&marks, None, None, Duration::from_millis(260));
        assert_eq!(t.asr_ms, Some(250));
        assert_eq!(t.llm_first_ms, None);
        assert_eq!(t.llm_ms, None);
        assert_eq!(t.to_first_audio_ms, None);
        assert_eq!(t.play_ms, None);
        let v = serde_json::to_value(&t).unwrap();
        for k in ["llm_first_ms", "llm_ms", "to_first_audio_ms", "play_ms", "record_ms", "interrupted"] {
            assert!(v.get(k).is_none(), "缺的段必须省略而不是填 0：{k} = {:?}", v.get(k));
        }
    }

    #[test]
    fn backwards_marks_become_none_not_a_negative_or_zero() {
        let marks = [d(900), d(100), None, None, None];
        let t = compute(&marks, None, None, Duration::from_millis(1000));
        assert_eq!(t.llm_first_ms, None);
    }

    #[test]
    fn menu_line_formats_and_hides_turns_without_audio() {
        let t = Timings {
            to_first_audio_ms: Some(2805),
            asr_ms: Some(130),
            llm_ms: Some(870),
            tts_first_ms: Some(1650),
            ..Default::default()
        };
        use crate::i18n::Lang;
        assert_eq!(
            menu_line(&t, Lang::Zh).unwrap(),
            "上一轮：说完→出声 2.81s（ASR 0.13s / 模型 0.87s / 合成 1.65s）"
        );
        assert_eq!(
            menu_line(&t, Lang::En).unwrap(),
            "Last turn: done speaking→voice 2.81s (ASR 0.13s / model 0.87s / TTS 1.65s)"
        );
        assert!(menu_line(&t, Lang::Th).unwrap().contains("2.81s"));
        let silent = Timings { asr_ms: Some(130), ..Default::default() };
        assert_eq!(menu_line(&silent, Lang::Zh), None);
        let partial = Timings { to_first_audio_ms: Some(1000), ..Default::default() };
        assert_eq!(
            menu_line(&partial, Lang::Zh).unwrap(),
            "上一轮：说完→出声 1.00s（ASR — / 模型 — / 合成 —）"
        );
    }

    #[test]
    fn stats_are_median_p90_max_by_nearest_rank() {
        assert_eq!(stats(vec![]), None);
        assert_eq!(stats(vec![5]), Some((5, 5, 5, 1)));
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(stats(v), Some((5, 9, 10, 10)));
    }

    /// 落盘只有数字与枚举：拿一条带哨兵文字的「回答」跑一遍，文件里不能出现它。
    #[test]
    fn records_never_contain_content() {
        let dir = std::env::temp_dir().join(format!("agentear-timings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("derived").join("timings.jsonl");
        let t = Timings {
            asr_ms: Some(1),
            llm_via: Some("sidecar".into()),
            model: Some("Qwen3-8B-4bit".into()),
            ..Default::default()
        };
        let rec = Record { ts_ms: 1, mode: "conversation", outcome: "ok", timings: t };
        append(&path, &rec).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        // Record 的类型里压根没有能放内容的字段——这里再用反向断言确认序列化出来的键集合。
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        let allowed = [
            "ts_ms", "mode", "outcome", "record_ms", "asr_ms", "llm_first_ms", "llm_ms",
            "tts_first_ms", "to_first_audio_ms", "play_ms", "total_ms", "llm_via", "model",
            "tier", "asr_backend", "prompt_tokens", "completion_tokens", "interrupted",
        ];
        for k in v.as_object().unwrap().keys() {
            assert!(allowed.contains(&k.as_str()), "落盘出现了白名单外的字段：{k}");
        }
        assert!(!body.contains("SENTINEL_TRANSCRIPT"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_rolls_over_at_the_limit() {
        let dir = std::env::temp_dir().join(format!("agentear-timings-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timings.jsonl");
        std::fs::write(&path, vec![b'x'; ROTATE_BYTES as usize]).unwrap();
        let rec = Record { ts_ms: 1, mode: "conversation", outcome: "ok", timings: Timings::default() };
        append(&path, &rec).unwrap();
        assert!(dir.join("timings.1.jsonl").exists(), "满了要滚动");
        assert!(std::fs::metadata(&path).unwrap().len() < 1000, "新文件从头写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 被新一轮顶掉的旧线程，迟到的打点不能落到新一轮上。
    /// 全局时钟：这条测试独占它（同文件其他测试不碰全局状态）。
    #[test]
    fn late_marks_from_a_superseded_turn_are_dropped() {
        let _g = test_guard();
        let old = begin_at(Instant::now(), "conversation", None, "builtin");
        let new = begin_at(Instant::now(), "conversation", None, "builtin");
        assert_ne!(old, new);
        // 旧线程（绑定在旧轮次上）迟到的打点
        std::thread::spawn(move || {
            bind(old);
            mark(Stage::AudioFirst);
            note_interrupted();
            assert!(finish("ok").is_none(), "旧轮次已被顶掉，不能再收一次");
        })
        .join()
        .unwrap();
        bind(new);
        mark(Stage::AsrDone);
        let t = finish("ok").expect("新一轮归本线程");
        assert_eq!(t.to_first_audio_ms, None, "旧线程的首声打点污染了新一轮");
        assert!(!t.interrupted, "旧线程的打断标记污染了新一轮");
        assert!(t.asr_ms.is_some());
        assert_eq!(current(), 0);
    }

    #[test]
    fn report_groups_by_route_and_model() {
        let recs: Vec<serde_json::Value> = vec![
            serde_json::json!({"mode":"conversation","llm_via":"agent24","model":"Qwen3-8B-4bit","to_first_audio_ms":2000,"asr_ms":100}),
            serde_json::json!({"mode":"conversation","llm_via":"agent24","model":"Qwen3-8B-4bit","to_first_audio_ms":4000,"asr_ms":300}),
            serde_json::json!({"mode":"conversation","llm_via":"sidecar","to_first_audio_ms":2800}),
        ];
        let r = report(&recs, None);
        assert!(r.contains("[conversation · agent24 · Qwen3-8B-4bit] 2 轮"), "{r}");
        assert!(r.contains("[conversation · sidecar · -] 1 轮"), "{r}");
        assert!(r.contains("to_first_audio_ms     2000 /    4000 /    4000   n=2"), "{r}");
        let last1 = report(&recs, Some(1));
        assert!(last1.starts_with("共 1 轮"), "{last1}");
    }
}
