//! 输入法模式每一轮的分段耗时 + 系统快照（v0.29.2，jason 2026-11-14）。
//!
//! 起因：同一段 20 秒录音、同一个二进制，重跑 7.7 s / 6.3 s / 27 s，而 SenseVoice 正常只要
//! 约 1 s。怀疑是机器负载 / 内存（load 113 / 10 核，swap 几乎满），但日志里没有分段数据，
//! **没法证明**。这个模块给输入法模式的每一轮记：
//!
//! * 各环节耗时（毫秒，单调时钟）：提交 raw / ASR（含拉起与解码）/ 纠错 / 标签 / 落库 /
//!   翻译 / 上屏 / 索引，以及松键 → 上屏的总时延（jason 感受到的那个数）；
//! * 轮次开始与结束各一份**系统快照**：load、核数、swap、内存压力、本进程与 ASR 子进程 RSS。
//!
//! 数据追加到 `derived/timings.jsonl`（与 v0.26.0 的对话计时同一个文件，靠 `"kind":"input"`
//! 区分；老读取代码对不认识的行只会当成一条没有对话字段的记录），`agentear --timings`
//! 的「输入法模式」一节做统计与慢轮归因。
//!
//! ## 规矩（与 `timings.rs` 同源）
//!
//! 1. **只存数字与枚举**：不存转写、不存路径、不存任何用户信息（有测试钉住字符串值的集合）。
//! 2. **缺了的就省略，不填 0**。
//! 3. **计时 / 采样绝不影响这一轮**：拿不到的字段省略，写盘失败只 warn。
//! 4. **采样不起子进程**（`top` / `vm_stat` 在高负载下本身就慢、会污染测量）：
//!    只用 `getloadavg` / `sysctlbyname` / `proc_pidinfo`。
//!
//! ## 几个分段的口径（别凭名字猜）
//!
//! * `key_to_commit`：`finish()` 入口（松键）→ raw 提交完成（含收尾采样、结束音、tray 切换）。
//! * `asr_total`：`asr.transcribe()` 调用前后。
//! * `asr_spawn_to_first_byte`：**仅内置引擎（SenseVoice / whisper 泰语）有**——从调 `spawn()`
//!   起到子进程第一次往 stdout / stderr 写出任何字节。它覆盖「fork+exec + 加载模型到第一行日志」，
//!   机器忙、内存紧时这一段会变长。Qwen3（常驻 / 逐次 CLI）走的是别的路径，**这里拿不到，省略**，
//!   只有 `asr_total`。`asr_decode = asr_total − asr_spawn_to_first_byte`（只在前者存在时算）。
//! * `routes_kb`：写转写文件 + 写 routes + 知识库入队（都在上屏**之前**）。
//! * `deliver`：剪贴板 + 上屏（不含翻译）；`translate`：翻译输入（没开则省略）。
//! * `index`：上屏**之后**的知识库投递 + 索引（不计入 `release_to_delivered`）。
//! * `release_to_delivered`：松键 → 上屏完成。`other` = 它减去上面各段之和（println、sanitize、
//!   指令表预检等零碎）；**若 `other` 很大说明有没测到的环节**。
use serde::Serialize;
use std::cell::Cell;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 慢轮判据：`release_to_delivered` 超过中位数的这个倍数。
pub const SLOW_FACTOR: u64 = 2;
/// 样本少于这个数就不下结论。
pub const MIN_SAMPLES: usize = 10;

pub const KIND: &str = "input";
/// 结局枚举。字符串值只能取这几个（隐私测试按它断言）。
#[cfg(test)]
pub const OUTCOMES: &[&str] = &["ok", "fallback", "failed", "empty"];
/// 后端枚举。
#[cfg(test)]
pub const BACKENDS: &[&str] = &["builtin", "speech_swift"];

// ------------------------------------------------------------------ 系统快照

/// 一次系统快照。**只有数字**；拿不到的字段是 `None`（序列化时省略）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    /// `getloadavg` 的 1 分钟值（保留两位小数）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load1: Option<f64>,
    /// 逻辑核数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ncpu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_used_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_total_mb: Option<u64>,
    /// `kern.memorystatus_vm_pressure_level`：1 正常 / 2 警告 / 4 严重。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<u8>,
    /// agentear 自身的常驻内存。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_rss_mb: Option<u64>,
    /// ASR 子进程的峰值 RSS（只有结束快照、且走内置引擎时有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_rss_mb: Option<u64>,
}

impl Snapshot {
    /// load / 核数。任一缺失 = `None`。
    pub fn load_per_core(&self) -> Option<f64> {
        match (self.load1, self.ncpu) {
            (Some(l), Some(n)) if n > 0 => Some(l / n as f64),
            _ => None,
        }
    }

    /// swap 使用率（0.0–1.0+）。没有 swap 总量（拿不到）或总量为 0 = `None`。
    pub fn swap_ratio(&self) -> Option<f64> {
        match (self.swap_used_mb, self.swap_total_mb) {
            (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64),
            _ => None,
        }
    }
}

/// `vm.swapusage` 的返回结构（`<sys/sysctl.h>` 的 `xsw_usage`）。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XswUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: bool,
}

/// `sysctlbyname` 读一个 POD 值。读不到 / 长度对不上 = `None`。
fn sysctl_by_name<T: Copy + Default>(name: &str) -> Option<T> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut val = T::default();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: val 是 len 字节的合法可写内存；name 是以 NUL 结尾的 C 字符串。
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut val as *mut T as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<T>()).then_some(val)
}

/// 某个进程当前常驻内存（MB）。拿不到（进程已退出 / 无权限）= `None`。
pub fn rss_mb_of(pid: u32) -> Option<u64> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: info 是 size 字节的合法可写内存。
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (n == size).then_some(info.pti_resident_size / (1024 * 1024))
}

/// 现在拍一份快照。**不起子进程。**
pub fn capture() -> Snapshot {
    let mut s = Snapshot::default();
    let mut la = [0f64; 3];
    // SAFETY: la 有 3 个 f64。
    if unsafe { libc::getloadavg(la.as_mut_ptr(), 1) } >= 1 {
        s.load1 = Some((la[0] * 100.0).round() / 100.0);
    }
    s.ncpu = sysctl_by_name::<i32>("hw.logicalcpu").and_then(|n| u32::try_from(n).ok());
    if let Some(sw) = sysctl_by_name::<XswUsage>("vm.swapusage") {
        s.swap_used_mb = Some(sw.used / (1024 * 1024));
        s.swap_total_mb = Some(sw.total / (1024 * 1024));
    }
    s.pressure = sysctl_by_name::<i32>("kern.memorystatus_vm_pressure_level")
        .and_then(|p| u8::try_from(p).ok());
    s.self_rss_mb = rss_mb_of(std::process::id());
    s
}

// ------------------------------------------------------------------ ASR 子进程探针

/// 一次子进程 ASR 调用量到的东西。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AsrProbe {
    /// 调 `spawn()` 本身花的时间（fork+exec；内存紧时这一步也会慢）。
    pub spawn_ms: u64,
    /// 从调 `spawn()` 起到第一次有输出（stdout 或 stderr）。一直没输出 = `None`。
    pub first_byte_ms: Option<u64>,
    /// 采样到的子进程峰值 RSS（MB）。
    pub peak_rss_mb: Option<u64>,
}

thread_local! {
    static PROBE: Cell<Option<AsrProbe>> = const { Cell::new(None) };
    static FALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// 一轮转写开始前调：清掉上一轮遗留的探针与退回标记。
pub fn begin_asr() {
    PROBE.with(|p| p.set(None));
    FALLBACK.with(|f| f.set(false));
}

/// 转写结束后取走探针（没走子进程路径 = `None`）。
pub fn take_probe() -> Option<AsrProbe> {
    PROBE.with(|p| p.take())
}

/// 引擎在这一轮发生了后端退回（如 Qwen3 常驻服务失败 → 逐次 CLI）时调。
pub fn note_fallback() {
    FALLBACK.with(|f| f.set(true));
}

pub fn take_fallback() -> bool {
    FALLBACK.with(|f| f.replace(false))
}

/// 等价于 `Command::output()`，但额外量：`spawn()` 耗时、第一字节时刻、子进程峰值 RSS。
/// 探针存进当前线程的槽（[`take_probe`] 取）。stdin 一律 null（与 `output()` 行为一致）。
/// 量不到的部分只是省略，**绝不让转写失败**。
pub fn run_probed(cmd: &mut Command) -> std::io::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let t0 = Instant::now();
    let mut child = cmd.spawn()?;
    let spawn_ms = t0.elapsed().as_millis() as u64;
    let pid = child.id();

    // 第一字节时刻（距 t0 的纳秒；u64::MAX = 还没有）。
    let first = Arc::new(AtomicU64::new(u64::MAX));
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));

    fn reader<R: Read + Send + 'static>(
        mut r: R,
        t0: Instant,
        first: Arc<AtomicU64>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let ns = t0.elapsed().as_nanos().min(u64::MAX as u128 - 1) as u64;
                        let _ = first.compare_exchange(u64::MAX, ns, Ordering::Relaxed, Ordering::Relaxed);
                        all.extend_from_slice(&buf[..n]);
                    }
                }
            }
            all
        })
    }
    let h_out = child.stdout.take().map(|r| reader(r, t0, first.clone()));
    let h_err = child.stderr.take().map(|r| reader(r, t0, first.clone()));

    // RSS 采样线程：每 50 ms 看一眼，记峰值。
    let sampler = {
        let (stop, peak) = (stop.clone(), peak.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(m) = rss_mb_of(pid) {
                    peak.fetch_max(m, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };

    let status = child.wait();
    stop.store(true, Ordering::Relaxed);
    let joined = |h: Option<std::thread::JoinHandle<Vec<u8>>>| match h {
        Some(h) => h.join().map_err(|_| std::io::Error::other("读子进程输出的线程 panic")),
        None => Ok(Vec::new()),
    };
    // 读线程出问题就报错，**不能**当成「子进程没输出」放行（那会变成一次空转写）。
    let stdout = joined(h_out);
    let stderr = joined(h_err);
    let _ = sampler.join();
    let (status, stdout, stderr) = (status?, stdout?, stderr?);

    let f = first.load(Ordering::Relaxed);
    let probe = AsrProbe {
        spawn_ms,
        first_byte_ms: (f != u64::MAX).then_some(f / 1_000_000),
        peak_rss_mb: match peak.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        },
    };
    PROBE.with(|p| p.set(Some(probe)));
    Ok(Output { status, stdout, stderr })
}

// ------------------------------------------------------------------ 一轮的记录

/// 落盘的一行。**全是数字与枚举**（`kind` / `backend` / `outcome` 是封闭集合）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputRecord {
    /// 恒为 `"input"`。
    pub kind: &'static str,
    pub ts_ms: u64,
    /// `builtin` / `speech_swift`。
    pub backend: &'static str,
    /// `ok` / `fallback` / `failed` / `empty`。
    pub outcome: &'static str,
    /// 这一轮发生了后端退回。
    pub backend_fallback: bool,
    /// 录音时长（毫秒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_to_commit_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_total_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_spawn_to_first_byte_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_decode_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correct_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routes_kb_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translate_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliver_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_to_delivered_ms: Option<u64>,
    /// 总时延里没被任何一段解释的部分（零碎 + 没测到的环节）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_ms: Option<u64>,
    pub sys_start: Snapshot,
    pub sys_end: Snapshot,
}

/// 记录里的毫秒数。
pub fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

impl InputRecord {
    pub fn new(backend: &'static str, record_ms: Option<u64>, sys_start: Snapshot) -> Self {
        Self {
            kind: KIND,
            ts_ms: now_ms(),
            backend,
            outcome: "ok",
            record_ms,
            sys_start,
            ..Default::default()
        }
    }

    /// 填入 ASR 段（总时长 + 探针）。`asr_decode = total − first_byte`，探针缺失时省略。
    pub fn set_asr(&mut self, total: Duration, probe: Option<AsrProbe>, fallback: bool) {
        let total_ms = ms(total);
        self.asr_total_ms = Some(total_ms);
        self.backend_fallback = fallback;
        if let Some(p) = probe {
            if let Some(fb) = p.first_byte_ms {
                self.asr_spawn_to_first_byte_ms = Some(fb);
                self.asr_decode_ms = Some(total_ms.saturating_sub(fb));
            }
            self.sys_end.asr_rss_mb = p.peak_rss_mb;
        }
    }

    /// 上屏完成那一刻：定总时延与 `other`。
    pub fn set_delivered(&mut self, release_to_delivered: Duration) {
        let total = ms(release_to_delivered);
        self.release_to_delivered_ms = Some(total);
        self.other_ms = Some(total.saturating_sub(self.accounted_ms()));
    }

    /// 已被各段解释的毫秒数（上屏前的串行环节；`index` 在上屏后，不计）。
    /// ASR 的拉起 / 解码是 `asr_total` 的内部拆分，不重复算。
    pub fn accounted_ms(&self) -> u64 {
        [
            self.key_to_commit_ms,
            self.asr_total_ms,
            self.correct_ms,
            self.label_ms,
            self.routes_kb_ms,
            self.translate_ms,
            self.deliver_ms,
        ]
        .iter()
        .flatten()
        .sum()
    }

    /// 收尾：补结局与结束快照，打一行 INFO，追加落盘。**写盘失败只 warn。**
    pub fn finish(mut self, outcome: &'static str) {
        self.outcome = if outcome == "ok" && self.backend_fallback { "fallback" } else { outcome };
        let rss = self.sys_end.asr_rss_mb;
        self.sys_end = capture();
        self.sys_end.asr_rss_mb = rss;
        log::info!("{}", summary_line(&self));
        crate::timings::append_default(&self);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn s(v: Option<u64>) -> String {
    v.map(|x| format!("{:.2}", x as f64 / 1000.0)).unwrap_or_else(|| "—".into())
}

/// 日志里那一行：
/// `⏱ 输入轮 总 X.XXs = 提交 a + ASR b（拉起 c / 解码 d）+ 纠错 e + 标签 f + 落库 g + 上屏 h | 录音 Ns | load 12.3/10核 swap 20.1/22.0G 压力 1`
pub fn summary_line(r: &InputRecord) -> String {
    let mut seg = format!(
        "提交 {} + ASR {}（拉起 {} / 解码 {}）+ 纠错 {} + 标签 {} + 落库 {}",
        s(r.key_to_commit_ms),
        s(r.asr_total_ms),
        s(r.asr_spawn_to_first_byte_ms),
        s(r.asr_decode_ms),
        s(r.correct_ms),
        s(r.label_ms),
        s(r.routes_kb_ms),
    );
    if r.translate_ms.is_some() {
        seg.push_str(&format!(" + 翻译 {}", s(r.translate_ms)));
    }
    seg.push_str(&format!(" + 上屏 {}", s(r.deliver_ms)));
    let sys = &r.sys_start;
    let load = match (sys.load1, sys.ncpu) {
        (Some(l), Some(n)) => format!("load {l:.1}/{n}核"),
        (Some(l), None) => format!("load {l:.1}"),
        _ => "load —".into(),
    };
    let swap = match (sys.swap_used_mb, sys.swap_total_mb) {
        (Some(u), Some(t)) => format!("swap {:.1}/{:.1}G", u as f64 / 1024.0, t as f64 / 1024.0),
        _ => "swap —".into(),
    };
    let pressure = sys.pressure.map(|p| p.to_string()).unwrap_or_else(|| "—".into());
    format!(
        "⏱ 输入轮 总 {}s = {seg} | 其他 {}s 索引(上屏后) {}s | 录音 {}s | {load} {swap} 压力 {pressure} | {}{}",
        s(r.release_to_delivered_ms),
        s(r.other_ms),
        s(r.index_ms),
        s(r.record_ms),
        r.backend,
        if r.backend_fallback { " 发生后端退回" } else { "" },
    )
}

// ------------------------------------------------------------------ 统计（--timings 的「输入法模式」一节）

/// 参与统计的分段（键 → 显示名）。顺序即报表顺序。
pub const SEGMENTS: &[(&str, &str)] = &[
    ("release_to_delivered_ms", "总时延(松键→上屏)"),
    ("key_to_commit_ms", "提交 raw"),
    ("asr_total_ms", "ASR 合计"),
    ("asr_spawn_to_first_byte_ms", "  其中 拉起→首字节"),
    ("asr_decode_ms", "  其中 解码"),
    ("correct_ms", "纠错"),
    ("label_ms", "标签"),
    ("routes_kb_ms", "落库(转写/routes/入队)"),
    ("translate_ms", "翻译"),
    ("deliver_ms", "上屏(剪贴板+粘贴)"),
    ("index_ms", "索引(上屏后)"),
    ("other_ms", "其他(未解释)"),
];

/// 「慢轮占比最大」的候选分段（互斥的串行环节；不含 ASR 的内部拆分）。
const ATTRIB: &[(&str, &str)] = &[
    ("key_to_commit_ms", "提交 raw"),
    ("asr_total_ms", "ASR"),
    ("correct_ms", "纠错"),
    ("label_ms", "标签"),
    ("routes_kb_ms", "落库"),
    ("translate_ms", "翻译"),
    ("deliver_ms", "上屏"),
    ("other_ms", "其他"),
];

/// load/核数 分桶。边界：≤1 / (1,2] / >2；缺失 = 未知。
pub fn load_bucket(ratio: Option<f64>) -> &'static str {
    match ratio {
        None => "load 未知",
        Some(r) if r <= 1.0 => "load/核 ≤1",
        Some(r) if r <= 2.0 => "load/核 1–2",
        Some(_) => "load/核 >2",
    }
}

/// swap 使用率分桶。边界：<50% / [50%,90%] / >90%；缺失 = 未知。
pub fn swap_bucket(ratio: Option<f64>) -> &'static str {
    match ratio {
        None => "swap 未知",
        Some(r) if r < 0.5 => "swap <50%",
        Some(r) if r <= 0.9 => "swap 50–90%",
        Some(_) => "swap >90%",
    }
}

fn med(v: &[u64]) -> Option<u64> {
    crate::timings::stats(v.to_vec()).map(|t| t.0)
}

fn snap_of(r: &serde_json::Value, key: &str) -> Snapshot {
    let o = &r[key];
    Snapshot {
        load1: o["load1"].as_f64(),
        ncpu: o["ncpu"].as_u64().map(|v| v as u32),
        swap_used_mb: o["swap_used_mb"].as_u64(),
        swap_total_mb: o["swap_total_mb"].as_u64(),
        pressure: o["pressure"].as_u64().map(|v| v as u8),
        self_rss_mb: o["self_rss_mb"].as_u64(),
        asr_rss_mb: o["asr_rss_mb"].as_u64(),
    }
}

/// `--timings` 的「输入法模式」一节。只读 `"kind":"input"` 的行；没有则返回空串。
pub fn report(records: &[serde_json::Value], last_n: Option<usize>) -> String {
    let all: Vec<&serde_json::Value> = records.iter().filter(|r| r["kind"] == KIND).collect();
    if all.is_empty() {
        return String::new();
    }
    let rs: &[&serde_json::Value] = match last_n {
        Some(n) if n < all.len() => &all[all.len() - n..],
        _ => &all,
    };
    let mut out = format!("\n== 输入法模式（共 {} 轮；毫秒，中位数 / p90 / 最大值；n = 有这一段数据的轮数）==\n", rs.len());
    let mut by_backend: std::collections::BTreeMap<&str, Vec<&serde_json::Value>> = Default::default();
    for r in rs {
        by_backend.entry(r["backend"].as_str().unwrap_or("-")).or_default().push(r);
    }
    for (backend, g) in by_backend {
        let count = |o: &str| g.iter().filter(|r| r["outcome"] == o).count();
        out.push_str(&format!(
            "\n[{backend}] {} 轮（ok {} / fallback {} / failed {} / empty {}）\n",
            g.len(),
            count("ok"),
            count("fallback"),
            count("failed"),
            count("empty"),
        ));
        for (k, name) in SEGMENTS {
            let v: Vec<u64> = g.iter().filter_map(|r| r[*k].as_u64()).collect();
            if let Some((p50, p90, max, n)) = crate::timings::stats(v) {
                out.push_str(&format!("  {name:<22} {p50:>7} / {p90:>7} / {max:>7}   n={n}\n"));
            }
        }
        out.push_str(&slow_section(&g));
    }
    out.push_str("\n口径：各段含义见 src/input_timings.rs 顶部；拉起→首字节只有内置引擎（SenseVoice/whisper）能测到，Qwen3 只有 ASR 合计。\n");
    out
}

fn slow_section(g: &[&serde_json::Value]) -> String {
    let mut out = String::from("  -- 慢轮归因 --\n");
    // 只用成功送达的轮（有总时延）。
    let timed: Vec<(&serde_json::Value, u64)> = g
        .iter()
        .filter_map(|r| r["release_to_delivered_ms"].as_u64().map(|t| (*r, t)))
        .collect();
    if timed.len() < MIN_SAMPLES {
        out.push_str(&format!("  样本不足（n={} < {MIN_SAMPLES}），不下结论\n", timed.len()));
        return out;
    }
    let totals: Vec<u64> = timed.iter().map(|t| t.1).collect();
    let median = med(&totals).unwrap_or(0);
    let threshold = median * SLOW_FACTOR;
    let slow: Vec<&(&serde_json::Value, u64)> = timed.iter().filter(|t| t.1 > threshold).collect();
    out.push_str(&format!(
        "  慢轮 = 总时延 > {SLOW_FACTOR}×中位数（{median} ms → {threshold} ms）：{} / {} 轮\n",
        slow.len(),
        timed.len()
    ));
    for (r, total) in slow.iter().rev().take(20).map(|t| (t.0, t.1)) {
        let (name, v) = ATTRIB
            .iter()
            .filter_map(|(k, n)| r[*k].as_u64().map(|v| (*n, v)))
            .max_by_key(|x| x.1)
            .unwrap_or(("?", 0));
        let st = snap_of(r, "sys_start");
        let load = st.load_per_core().map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".into());
        let swap = st.swap_ratio().map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "—".into());
        out.push_str(&format!(
            "    ts={} 总 {total} ms，最大段 {name} {v} ms（{:.0}%），load/核 {load}，swap {swap}，录音 {} ms\n",
            r["ts_ms"].as_u64().unwrap_or(0),
            if total > 0 { v as f64 * 100.0 / total as f64 } else { 0.0 },
            r["record_ms"].as_u64().unwrap_or(0),
        ));
    }
    if slow.len() > 20 {
        out.push_str(&format!("    …另有 {} 轮略\n", slow.len() - 20));
    }
    // 分桶：用轮次开始时的快照。
    for (title, f) in [
        ("按 load/核数 分桶（总时延中位数）", 0u8),
        ("按 swap 使用率 分桶（总时延中位数）", 1u8),
    ] {
        out.push_str(&format!("  {title}\n"));
        let mut buckets: std::collections::BTreeMap<&str, Vec<u64>> = Default::default();
        for (r, t) in &timed {
            let st = snap_of(r, "sys_start");
            let b = if f == 0 { load_bucket(st.load_per_core()) } else { swap_bucket(st.swap_ratio()) };
            buckets.entry(b).or_default().push(*t);
        }
        for (b, v) in buckets {
            if v.len() < MIN_SAMPLES {
                out.push_str(&format!("    {b:<14} n={:<4} 样本不足，不下结论\n", v.len()));
            } else {
                out.push_str(&format!("    {b:<14} n={:<4} 中位数 {} ms\n", v.len(), med(&v).unwrap_or(0)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
