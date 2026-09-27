//! Qwen3-ASR 可选后端的**安装与常驻服务**（T3.5.6，ADR-0010）。
//!
//! ## 这个模块管什么
//!
//! 1. **运行时**：speech-swift 的预编译包（`speech` / `speech-server`），
//!    **版本钉死**（[`SPEECH_VERSION`] + tarball 的 sha256），从上游 GitHub release
//!    下载、校验、解包到 `~/.agentear/models/qwen3/runtime/speech-<版本>/`。
//!    **不走 brew、不随包分发**（jason 2026-09-26 拍板）。
//! 2. **模型权重**：0.6B / 1.7B 两档，**直接从 Hugging Face 下载**，
//!    钉在某个 commit（`resolve/<revision>/`），逐文件 sha256 校验，
//!    按 speech 认的布局 `$QWEN3_ASR_CACHE_DIR/qwen3-speech/models/<org>/<repo>/`
//!    摆好。之后 speech **不再联网**（它的下载器永远不会被触发）。
//! 3. **常驻服务**（可选，菜单栏开关）：`speech-server` 热态约 0.1–0.2 s 一轮，
//!    代价是常驻 1–2.5 GiB；空闲超时自动退出，退出/信号时收掉。
//!
//! ## 为什么所有 speech 进程都套一层「断网沙箱」
//!
//! speech 找不到模型时会**静默下载**：ADR-0010 调研时 speech-server 在一个
//! 没带模型名的请求上当场下了 611 MB 的英文模型（RAW §4）；`-m <本地目录>`
//! 会被当成仓库名、重试 5 次约 2 分钟才报错。我们的承诺是「模型由我们下、
//! 校验过的那份才用」，所以 speech 的每一次启动都跑在
//! `sandbox-exec` 的「禁止出站 TCP」里：布局错了就**当场报错**，
//! 不会在用户不知情的情况下去 Hugging Face 拉几百 MB，也不会把录音相关的任何东西送出去。
//! `sandbox-exec` 不存在（未来系统删掉它）时退化为直接运行，并打一行告警。
//!
//! ## 默认值（jason 2026-09-26：「默认小内存」）
//!
//! - 整体默认 ASR 仍是随包的 SenseVoice（`asr_backend = builtin`），不变。
//! - 选了 Qwen3 时默认 **0.6B**、默认 **逐次调用**（不常驻）——都是小内存那一档。
//!   1.7B 与常驻都要用户自己选。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::download::{self, Fail, FileLock, State};

/// speech-swift 的版本。**升级 = 改这里 + 下面三个常量 + 本机实测一遍**。
///
/// 钉死而不是跟最新（jason 2026-09-26 拍板）：上游约一个多月出了 5 版，
/// CLI 参数变过；`SpeechSwiftEngine::preflight` 的契约探测只能挡住一部分。
pub const SPEECH_VERSION: &str = "v0.0.28";
const RUNTIME_URL: &str =
    "https://github.com/soniqo/speech-swift/releases/download/v0.0.28/speech-macos-arm64.tar.gz";
/// 2026-09-26 实测（`shasum -a 256`），与 ADR-0010 RAW §1 一致。
const RUNTIME_SHA256: &str = "cc144cac7985884f026a76281fdb504ce6e0fe2ad11a9b0a7901cf8b617b930a";
const RUNTIME_BYTES: u64 = 99_089_736;
/// 解包后体积（实测 `du -sh` = 364M），磁盘预检用，往上取整留余量。
const RUNTIME_UNPACKED_BYTES: u64 = 400_000_000;

/// 一个要下载的文件。
pub struct FileSpec {
    pub name: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

// 两个仓库的文件清单：**就是 speech 自己下载时拉的那 6 个文件**
// （对照过它的缓存目录，2026-09-26），README / .gitattributes 不要。
// 小文件的 sha 是本机下载后算的；`model.safetensors` 的 sha 与 HF 的 LFS 声明一致。
const FILES_06B: [FileSpec; 6] = [
    FileSpec { name: "config.json", sha256: "923618cf5ca452fda0253a6be5c1a17f94a2e4851d3b98beb45848565587bd72", bytes: 7187 },
    FileSpec { name: "merges.txt", sha256: "8831e4f1a044471340f7c0a83d7bd71306a5b867e95fd870f74d0c5308a904d5", bytes: 1671853 },
    FileSpec { name: "model.safetensors", sha256: "70c7e67e588062adce4f10796e47ad42ead51c6671eda61a0987eae38ca95ddf", bytes: 708236945 },
    FileSpec { name: "model.safetensors.index.json", sha256: "e3bb80ef0fd42a5be07b04e90c97d60460bbde8af3531e0bfe9100a61404d81a", bytes: 71814 },
    FileSpec { name: "tokenizer_config.json", sha256: "4942d005604266809309cabc9f4e9cb89ce855d59b14681fdc0e1cc62ea26c4c", bytes: 12487 },
    FileSpec { name: "vocab.json", sha256: "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910", bytes: 2776833 },
];
const FILES_17B: [FileSpec; 6] = [
    FileSpec { name: "config.json", sha256: "1b76b3b6c655fc54595da025f7a96474ad9fa86363303fbdd61a7d8483ccfaf7", bytes: 7188 },
    FileSpec { name: "merges.txt", sha256: "8831e4f1a044471340f7c0a83d7bd71306a5b867e95fd870f74d0c5308a904d5", bytes: 1671853 },
    FileSpec { name: "model.safetensors", sha256: "bf304b009cc7eca79283056f787b44c952d24ac22cec787b39732bba3c23c13c", bytes: 2463307541 },
    FileSpec { name: "model.safetensors.index.json", sha256: "0a5d0ec11188602242ff81a9969883d0fdeb98cd5d85cd1413089d897c201af5", bytes: 78968 },
    FileSpec { name: "tokenizer_config.json", sha256: "4942d005604266809309cabc9f4e9cb89ce855d59b14681fdc0e1cc62ea26c4c", bytes: 12487 },
    FileSpec { name: "vocab.json", sha256: "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910", bytes: 2776833 },
];

/// Qwen3-ASR 的两档模型。
///
/// **默认 0.6B**：jason 2026-09-26「默认小内存」。T3.5.1 的横比里 1.7B 更准
/// （英文 / 中英混说），0.6B 与 SenseVoice 持平，但 0.6B 常驻只要约 1 GiB。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Qwen3Model {
    #[default]
    #[serde(rename = "0.6b")]
    Small,
    #[serde(rename = "1.7b")]
    Large,
}

impl Qwen3Model {
    pub const ALL: [Qwen3Model; 2] = [Qwen3Model::Small, Qwen3Model::Large];

    pub fn index(self) -> usize {
        match self {
            Qwen3Model::Small => 0,
            Qwen3Model::Large => 1,
        }
    }

    /// Hugging Face 仓库（speech 认的就是这两个）。
    pub fn repo(self) -> &'static str {
        match self {
            Qwen3Model::Small => "aufklarer/Qwen3-ASR-0.6B-MLX-4bit",
            Qwen3Model::Large => "aufklarer/Qwen3-ASR-1.7B-MLX-8bit",
        }
    }

    /// 钉死的 commit。**用 `resolve/<commit>/` 而不是 `resolve/main/`**：
    /// 第三方转换仓库随时可能改动，钉住 commit 才能让 sha 永远对得上。
    fn revision(self) -> &'static str {
        match self {
            Qwen3Model::Small => "bc441bd1e4295c1f42d9879f056049a925b6e013",
            Qwen3Model::Large => "e5450a26d1fd417c45fc9c405651ddc3180a27a6",
        }
    }

    pub fn files(self) -> &'static [FileSpec] {
        match self {
            Qwen3Model::Small => &FILES_06B,
            Qwen3Model::Large => &FILES_17B,
        }
    }

    pub fn total_bytes(self) -> u64 {
        self.files().iter().map(|f| f.bytes).sum()
    }

    /// `speech transcribe -m` 的取值。
    pub fn cli_size(self) -> &'static str {
        match self {
            Qwen3Model::Small => "0.6B",
            Qwen3Model::Large => "1.7B",
        }
    }

    /// `speech-server` 请求里的 `model` 字段。**必须显式传**——不传会路由到
    /// 默认的英文模型并当场下载 611 MB（ADR-0010 RAW §4）。
    /// 2026-09-26 在断网沙箱里实测：这两个名字分别落到上面两个仓库。
    pub fn server_model(self) -> &'static str {
        match self {
            Qwen3Model::Small => "qwen3-asr-0.6b-mlx-int4",
            Qwen3Model::Large => "qwen3-asr-1.7b-mlx-int8",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Qwen3Model::Small => "Qwen3-ASR 0.6B",
            Qwen3Model::Large => "Qwen3-ASR 1.7B",
        }
    }

    fn dir_name(self) -> &'static str {
        self.repo().rsplit('/').next().unwrap_or(self.repo())
    }

    /// 命令行用：`0.6b` / `1.7b`（大小写、`B` 后缀都收）。
    pub fn parse_cli(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "0.6b" | "0.6" | "small" => Ok(Qwen3Model::Small),
            "1.7b" | "1.7" | "large" => Ok(Qwen3Model::Large),
            other => bail!("未知的 Qwen3-ASR 模型 {other:?}；可选值：0.6b / 1.7b"),
        }
    }
}

// —— 路径 ——

/// `~/.agentear/models/qwen3/`。和泰语模型一样放用户数据目录：可写、跨升级保留
/// （`download.rs` 模块文档讲了为什么不能放 `vendor/`）。
pub fn root() -> Option<PathBuf> {
    download::models_root().map(|r| r.join("qwen3"))
}

fn runtime_dir() -> Option<PathBuf> {
    root().map(|r| r.join("runtime").join(format!("speech-{SPEECH_VERSION}")))
}

/// 传给 speech 的 `QWEN3_ASR_CACHE_DIR`。
pub fn cache_dir() -> Option<PathBuf> {
    root().map(|r| r.join("cache"))
}

fn model_dir(m: Qwen3Model) -> Option<PathBuf> {
    cache_dir().map(|c| c.join("qwen3-speech").join("models").join(m.repo()))
}

fn staging_dir() -> Option<PathBuf> {
    root().map(|r| r.join("downloads"))
}

fn runtime_marker() -> Option<PathBuf> {
    root().map(|r| r.join(format!("runtime-{SPEECH_VERSION}.installed")))
}

fn model_marker(m: Qwen3Model) -> Option<PathBuf> {
    root().map(|r| r.join(format!("{}.installed", m.dir_name())))
}

pub fn speech_bin() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join("speech"))
}

fn server_bin() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join("speech-server"))
}

// —— 安装判据 ——

/// 真目录（不是符号链接）。speech 对符号链接的模型目录直接报 `Not a directory`
/// （RAW §3），而且链接指向的东西不受我们控制。
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn marker_says(path: &Path, want: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        && fs::read_to_string(path).is_ok_and(|t| t.trim() == want)
}

/// 运行时装好了吗：安装记录对得上这个版本的 sha，且两个可执行文件都在。
pub fn runtime_installed() -> bool {
    let (Some(dir), Some(marker)) = (runtime_dir(), runtime_marker()) else {
        return false;
    };
    is_real_dir(&dir)
        && marker_says(&marker, RUNTIME_SHA256)
        && ["speech", "speech-server"]
            .iter()
            .all(|b| download::is_present(&dir.join(b)))
}

/// 模型装好了吗：安装记录对得上钉死的 commit，目录是真目录，每个文件体积都对。
///
/// 和 `download::is_installed` 同一个取舍：**不每次重算 sha**（2.4 GB 要好几秒），
/// 全量校验发生在安装那一刻；这里挡的是「下到一半」「手动放的」「换了版本」。
pub fn model_installed(m: Qwen3Model) -> bool {
    let (Some(dir), Some(marker)) = (model_dir(m), model_marker(m)) else {
        return false;
    };
    is_real_dir(&dir)
        && marker_says(&marker, m.revision())
        && m.files().iter().all(|f| {
            let p = dir.join(f.name);
            download::is_present(&p) && fs::symlink_metadata(&p).is_ok_and(|x| x.len() == f.bytes)
        })
}

/// 这个模型能不能用（运行时 + 模型都装好）。
pub fn is_ready(m: Qwen3Model) -> bool {
    runtime_installed() && model_installed(m)
}

// —— 下载状态（按模型分开）——

#[derive(Clone)]
struct Job {
    /// 0=闲 1=下载中 2=失败/取消 3=验证中
    phase: u8,
    done: u64,
    total: u64,
    fail: Fail,
    cancel: Arc<AtomicBool>,
}

static JOBS: Mutex<[Option<Job>; 2]> = Mutex::new([None, None]);

/// 当前状态。设置窗口的 0.5 s 定时器会调它，所以**不做任何 I/O 之外的重活**
/// （安装判据只是几次 stat + 读两个小文件）。
pub fn state(m: Qwen3Model) -> State {
    if let Some(job) = JOBS.lock().unwrap_or_else(|e| e.into_inner())[m.index()].clone() {
        match job.phase {
            1 => {
                let pct = if job.total > 0 {
                    (job.done.saturating_mul(100) / job.total).min(99) as u8
                } else {
                    0
                };
                return State::Downloading(pct);
            }
            3 => return State::Verifying,
            // 失败 / 取消之后，模型可能已被别的途径装好（终端里的 --fetch-qwen3）——
            // 那时继续显示「失败」就是在骗人。
            2 if !is_ready(m) => return State::Failed(job.fail),
            _ => {}
        }
    }
    if is_ready(m) {
        State::Ready
    } else {
        State::Absent
    }
}

/// 已下字节 / 总字节（给设置窗口显示「312 / 812 MB」用）。
pub fn progress_bytes(m: Qwen3Model) -> Option<(u64, u64)> {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs[m.index()]
        .as_ref()
        .filter(|j| j.phase == 1)
        .map(|j| (j.done, j.total))
}

fn set_job(m: Qwen3Model, f: impl FnOnce(&mut Job)) {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(j) = jobs[m.index()].as_mut() {
        f(j);
    }
}

/// 开始下载（运行时缺的话一并下）。**已在下载/验证中就什么也不做**——重复点击是常态。
///
/// `on_installed` 在安装记录落地之后调用（要不要切过去由调用方按用户此刻的意图定，
/// 和泰语模型的 `THAI_INTENT` 同一个理由：下载要几分钟，用户可能中途改主意）。
pub fn start(m: Qwen3Model, on_installed: fn(Qwen3Model)) {
    let cancel = {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(j) = &jobs[m.index()] {
            if j.phase == 1 || j.phase == 3 {
                log::info!("{} 已在下载/验证中，忽略重复请求", m.label());
                return;
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        jobs[m.index()] = Some(Job { phase: 1, done: 0, total: 0, fail: Fail::Io, cancel: cancel.clone() });
        cancel
    };

    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(m, &cancel)));
        let outcome = outcome.unwrap_or_else(|_| {
            Err(anyhow::Error::new(Fail::Io).context("下载线程 panic"))
        });
        match outcome {
            Ok(()) => {
                log::info!("{} 安装完成", m.label());
                JOBS.lock().unwrap_or_else(|e| e.into_inner())[m.index()] = None;
                on_installed(m);
            }
            Err(e) => {
                let f = e.downcast_ref::<Fail>().copied().unwrap_or(Fail::Io);
                if f == Fail::Cancelled {
                    log::info!("{} 下载已取消（已下的部分保留，下次从断点续）", m.label());
                } else {
                    log::error!("{} 安装失败：{e:#}", m.label());
                }
                set_job(m, |j| {
                    j.fail = f;
                    j.phase = 2;
                });
            }
        }
    });
}

/// 取消正在进行的下载。下载线程会在 ≤400 ms 内掐掉 curl，`.part` 保留。
pub fn cancel(m: Qwen3Model) {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(j) = &jobs[m.index()] {
        if j.phase == 1 {
            j.cancel.store(true, Ordering::SeqCst);
            log::info!("请求取消 {} 的下载", m.label());
        }
    }
}

/// 同步安装（给 `--fetch-qwen3` 用）：在当前线程跑，进度打到 stderr。
pub fn install_blocking(m: Qwen3Model) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    JOBS.lock().unwrap_or_else(|e| e.into_inner())[m.index()] = Some(Job {
        phase: 1,
        done: 0,
        total: 0,
        fail: Fail::Io,
        cancel: cancel.clone(),
    });
    let r = install(m, &cancel);
    JOBS.lock().unwrap_or_else(|e| e.into_inner())[m.index()] = None;
    r
}

fn io_err(msg: String) -> anyhow::Error {
    anyhow::Error::new(Fail::Io).context(msg)
}

/// 拿锁；拿不到（另一个下载正在装同一个东西）就**等**，而不是立刻报 Busy——
/// 同一个进程里「先点 0.6B 再点 1.7B」两个任务都要装运行时，后来的那个该排队。
/// 等的时候照样响应取消。
fn lock_wait(path: &Path, cancel: &AtomicBool) -> Result<FileLock> {
    loop {
        match FileLock::acquire(path) {
            Ok(l) => return Ok(l),
            Err(e) if e.downcast_ref::<Fail>() == Some(&Fail::Busy) => {
                if cancel.load(Ordering::SeqCst) {
                    return Err(anyhow::Error::new(Fail::Cancelled).context("等锁时取消"));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(e),
        }
    }
}

fn part_len(p: &Path) -> u64 {
    fs::symlink_metadata(p)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// 清掉「不是普通文件」的 `.part`（符号链接会让 curl 顺着写到别处去，见 download.rs）。
fn sanitize_part(part: &Path, expected: u64) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(part) {
        if !m.is_file() {
            fs::remove_file(part)
                .or_else(|_| fs::remove_dir_all(part))
                .map_err(|e| io_err(format!("清理 {} 失败: {e}", part.display())))?;
        } else if m.len() > expected {
            // 比目标还大的残留会让 `-C -` 永远续不上（download.rs 里写过这个坑）
            fs::remove_file(part).ok();
        }
    }
    Ok(())
}

/// 下载一个文件到 `part`（续传），校验 sha，失败则按 download.rs 的语义处理。
fn fetch_verified(
    url: &str,
    sha: &str,
    bytes: u64,
    part: &Path,
    base_done: u64,
    m: Qwen3Model,
    cancel: &AtomicBool,
) -> Result<()> {
    sanitize_part(part, bytes)?;
    if part_len(part) < bytes {
        download::fetch_url(
            url,
            bytes,
            part,
            &|len| set_job(m, |j| j.done = base_done + len),
            Some(cancel),
        )?;
    }
    set_job(m, |j| j.done = base_done + bytes);
    download::verify_file(part, sha, bytes)
}

fn install(m: Qwen3Model, cancel: &AtomicBool) -> Result<()> {
    let root = root().context("数据目录未初始化")?;
    let staging = staging_dir().context("数据目录未初始化")?;
    fs::create_dir_all(&staging).map_err(|e| io_err(format!("建 {} 失败: {e}", staging.display())))?;

    // 算账：要下多少字节、磁盘要留多少。**下载之前就查**，别下到 90% 才发现盘满。
    let need_runtime = !runtime_installed();
    let rt_part = staging.join(format!("speech-{SPEECH_VERSION}.tar.gz.part"));
    let mdir = model_dir(m).context("数据目录未初始化")?;
    let missing: Vec<&FileSpec> = m
        .files()
        .iter()
        .filter(|f| {
            let p = mdir.join(f.name);
            !(download::is_present(&p) && fs::symlink_metadata(&p).is_ok_and(|x| x.len() == f.bytes))
        })
        .collect();
    let mut total = 0u64;
    let mut need_disk = 0u64;
    if need_runtime {
        total += RUNTIME_BYTES;
        need_disk += RUNTIME_BYTES.saturating_sub(part_len(&rt_part)) + RUNTIME_UNPACKED_BYTES;
    }
    for f in &missing {
        total += f.bytes;
        need_disk += f.bytes.saturating_sub(part_len(&staging.join(m.dir_name()).join(format!("{}.part", f.name))));
    }
    set_job(m, |j| j.total = total);
    if need_disk > 0 {
        download::ensure_space(&root, need_disk)?;
    }

    let mut done = 0u64;
    if need_runtime {
        let _lock = lock_wait(&root.join("runtime.lock"), cancel)?;
        // 拿到锁之后再看一眼：可能别的任务刚装完
        if !runtime_installed() {
            log::info!("下载 speech-swift {SPEECH_VERSION}（{:.0} MB）", RUNTIME_BYTES as f64 / 1e6);
            fetch_verified(RUNTIME_URL, RUNTIME_SHA256, RUNTIME_BYTES, &rt_part, done, m, cancel)?;
            unpack_runtime(&rt_part)?;
        }
        done += RUNTIME_BYTES;
    }

    let _lock = lock_wait(&root.join(format!("{}.lock", m.dir_name())), cancel)?;
    // 拿到锁之后**重新判断**：等锁期间另一个进程（终端里的 --fetch-qwen3 和 .app 同时开着）
    // 可能已经装完了，上面那份「缺哪些文件」已经过时——不重算会白下几百 MB。
    if model_installed(m) {
        log::info!("{} 已被另一个进程装好，跳过", m.label());
        return Ok(());
    }
    let missing: Vec<&FileSpec> = missing
        .into_iter()
        .filter(|f| {
            let p = mdir.join(f.name);
            !(download::is_present(&p) && fs::symlink_metadata(&p).is_ok_and(|x| x.len() == f.bytes))
        })
        .collect();
    let file_staging = staging.join(m.dir_name());
    fs::create_dir_all(&file_staging)
        .map_err(|e| io_err(format!("建 {} 失败: {e}", file_staging.display())))?;
    ensure_real_dir(&mdir)?;
    for f in &missing {
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            m.repo(),
            m.revision(),
            f.name
        );
        let part = file_staging.join(format!("{}.part", f.name));
        log::info!("下载 {} / {}（{:.1} MB）", m.label(), f.name, f.bytes as f64 / 1e6);
        fetch_verified(&url, f.sha256, f.bytes, &part, done, m, cancel)?;
        let dest = mdir.join(f.name);
        fs::File::open(&part)
            .and_then(|x| x.sync_all())
            .map_err(|e| io_err(format!("sync 失败: {e}")))?;
        if fs::symlink_metadata(&dest).is_ok() {
            fs::remove_file(&dest).map_err(|e| io_err(format!("清理旧的 {} 失败: {e}", dest.display())))?;
        }
        fs::rename(&part, &dest).map_err(|e| io_err(format!("落地 {} 失败: {e}", f.name)))?;
        done += f.bytes;
    }
    download::sync_dir(&mdir);

    // 已存在（体积对）的文件也要全量校验一次——它们可能是上一次手动放进来的，
    // 或者安装记录是旧版本的。安装记录只在全部校验 + 冒烟通过之后才写。
    set_job(m, |j| j.phase = 3);
    for f in m.files() {
        let p = mdir.join(f.name);
        download::verify_file(&p, f.sha256, f.bytes)
            .with_context(|| format!("{} 校验失败（已删除，重新下载即可）", f.name))?;
    }
    smoke(m).map_err(|e| io_err(format!("加载冒烟失败: {e:#}")))?;
    write_marker(&model_marker(m).context("数据目录未初始化")?, m.revision())?;
    download::sync_dir(&root);
    Ok(())
}

/// `create_dir_all`，然后确认**路径上的每一段**都不是符号链接。
/// speech 要求模型目录是实体；而且一个指向别处的链接会让「校验过的文件」落到我们不控制的地方。
fn ensure_real_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| io_err(format!("建 {} 失败: {e}", dir.display())))?;
    let root = root().context("数据目录未初始化")?;
    let mut p = dir.to_path_buf();
    while p.starts_with(&root) && p != root {
        if !is_real_dir(&p) {
            return Err(io_err(format!("{} 不是实体目录（符号链接？）", p.display())));
        }
        if !p.pop() {
            break;
        }
    }
    Ok(())
}

fn write_marker(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_file(&tmp).ok();
    }
    fs::write(&tmp, content).map_err(|e| io_err(format!("写安装记录失败: {e}")))?;
    fs::rename(&tmp, path).map_err(|e| io_err(format!("落地安装记录失败: {e}")))?;
    Ok(())
}

/// 解包运行时：先解到临时目录，检查、去掉 quarantine，再整体 rename 到位。
///
/// **quarantine**：speech 是 ad-hoc 签名、无 Team ID（`spctl` 判 rejected）。
/// 我们用 curl 下载时不会打 `com.apple.quarantine`，但用户若手动从浏览器
/// 下了同一个包塞进来就会有——那种情况下 Gatekeeper 会拦住执行。
/// 所以解包后一律 `xattr -dr` 清一遍，并**再查一遍确实没了**（清不掉就报错，
/// 而不是装好一个每次都被系统拦住的运行时）。
fn unpack_runtime(tarball: &Path) -> Result<()> {
    let dir = runtime_dir().context("数据目录未初始化")?;
    let parent = dir.parent().context("运行时目录没有父目录")?.to_path_buf();
    fs::create_dir_all(&parent).map_err(|e| io_err(format!("建 {} 失败: {e}", parent.display())))?;
    let tmp = parent.join(format!(".unpack-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| io_err(format!("建 {} 失败: {e}", tmp.display())))?;

    let out = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(&tmp)
        .output()
        .map_err(|e| io_err(format!("启动 tar 失败: {e}")))?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(io_err(format!(
            "解包失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    for b in ["speech", "speech-server", "mlx.metallib"] {
        if !download::is_present(&tmp.join(b)) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(io_err(format!("运行时包里缺 {b}，上游包结构可能变了")));
        }
    }
    strip_quarantine(&tmp)?;

    if fs::symlink_metadata(&dir).is_ok() {
        fs::remove_dir_all(&dir).map_err(|e| io_err(format!("清理旧运行时失败: {e}")))?;
    }
    fs::rename(&tmp, &dir).map_err(|e| io_err(format!("运行时落地失败: {e}")))?;
    download::sync_dir(&parent);
    write_marker(&runtime_marker().context("数据目录未初始化")?, RUNTIME_SHA256)?;
    let _ = fs::remove_file(tarball);
    log::info!("speech-swift {SPEECH_VERSION} 已解包到 {}", dir.display());
    Ok(())
}

fn strip_quarantine(dir: &Path) -> Result<()> {
    let _ = Command::new("/usr/bin/xattr")
        .arg("-dr")
        .arg("com.apple.quarantine")
        .arg(dir)
        .output();
    for b in ["speech", "speech-server"] {
        if has_quarantine(&dir.join(b)) {
            return Err(io_err(format!("{b} 上的 com.apple.quarantine 清不掉，Gatekeeper 会拦住它")));
        }
    }
    Ok(())
}

pub(crate) fn has_quarantine(p: &Path) -> bool {
    Command::new("/usr/bin/xattr")
        .arg("-p")
        .arg("com.apple.quarantine")
        .arg(p)
        .output()
        .is_ok_and(|o| o.status.success())
}

// —— 运行 speech ——

/// 断网沙箱。speech 的所有进程都套它（见模块文档）。
/// 禁出站 TCP；本机 unix socket 放行，入站不受影响（speech-server 照样能被本机 curl 访问）。
///
/// ⚠️ **只禁 TCP、不禁 UDP 是实测出来的**（2026-09-26）：同样的缓存目录，
/// 加上 `deny network-outbound (remote udp …)` 之后 speech 会判定缓存无效、
/// 开始「下载」（在沙箱里失败，重试 5 次、约 2 分钟后报
/// `failedToDownload … Operation not permitted`）；只禁 TCP 时直接用缓存，3.4 s 出结果。
/// Hugging Face 的下载走 HTTPS/TCP，禁 TCP 已经挡住了「静默去下模型」这件事。
const SANDBOX_PROFILE: &str = "(version 1)(allow default)\
(deny network-outbound (remote tcp \"*:*\"))\
(allow network-outbound (remote unix-socket))";
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// 造一个跑 speech 的命令：断网沙箱 + 指向我们自己的模型目录。
pub fn speech_command(bin: &Path) -> Command {
    let mut cmd = if Path::new(SANDBOX_EXEC).exists() {
        let mut c = Command::new(SANDBOX_EXEC);
        c.arg("-p").arg(SANDBOX_PROFILE).arg(bin);
        c
    } else {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::SeqCst) {
            log::warn!("系统里没有 sandbox-exec，speech 直接运行（缺模型时它可能自己去联网下载）");
        }
        Command::new(bin)
    };
    if let Some(c) = cache_dir() {
        cmd.env("QWEN3_ASR_CACHE_DIR", c);
    }
    cmd
}

/// 冒烟用的一小段音频（macOS 自带的 `say` + `afconvert`，16 kHz 单声道，
/// 与 AgentEar 自己录的 raw 同格式）。生成一次后缓存在 qwen3 根目录。
fn smoke_wav() -> Result<PathBuf> {
    let root = root().context("数据目录未初始化")?;
    let wav = root.join("smoke-16k.wav");
    if download::is_present(&wav) {
        return Ok(wav);
    }
    let aiff = root.join(format!("smoke-{}.aiff", std::process::id()));
    let ok = Command::new("/usr/bin/say")
        .arg("-o")
        .arg(&aiff)
        .arg("Testing, one, two, three.")
        .status()
        .is_ok_and(|s| s.success())
        && Command::new("/usr/bin/afconvert")
            .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
            .arg(&aiff)
            .arg(&wav)
            .status()
            .is_ok_and(|s| s.success());
    let _ = fs::remove_file(&aiff);
    if !ok {
        bail!("生成冒烟音频失败（say / afconvert）");
    }
    Ok(wav)
}

/// 跑一个子进程，超时就杀。speech 在布局不对时会重试约 2 分钟才报错，
/// 冒烟不能陪它等那么久。
fn output_with_timeout(mut cmd: Command, limit: Duration) -> Result<std::process::Output> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 speech 失败")?;
    // 输出不大（几行），但为防管道写满卡死，还是在单独线程里读
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = so.as_mut() {
            use std::io::Read;
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = se.as_mut() {
            use std::io::Read;
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().context("等待 speech 失败")? {
            break s;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("speech 超过 {limit:?} 没结束，已中止");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok(std::process::Output {
        status,
        stdout: t_out.join().unwrap_or_default(),
        stderr: t_err.join().unwrap_or_default(),
    })
}

/// 安装冒烟：用刚装好的运行时、在断网沙箱里转写一小段音频。
/// **过了才写安装记录**——一个「文件都在但加载不了」的安装比没装还糟。
fn smoke(m: Qwen3Model) -> Result<()> {
    let bin = speech_bin().context("数据目录未初始化")?;
    let wav = smoke_wav()?;
    let mut cmd = speech_command(&bin);
    cmd.args(["transcribe", "--engine", "qwen3", "-m", m.cli_size(), "--"]).arg(&wav);
    let out = output_with_timeout(cmd, Duration::from_secs(120))?;
    if !out.status.success() {
        bail!(
            "speech 返回 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let t = crate::engine::parse_speech_output(&stdout)?;
    log::info!("{} 冒烟通过：{:?}", m.label(), t.text);
    Ok(())
}

// —— 常驻服务 ——

struct Server {
    child: Child,
    port: u16,
    model: Qwen3Model,
    last_used: Instant,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// 同一个 pid 的无锁副本，给信号处理函数用（async-signal-safe：只能 `kill(2)`）。
static SERVER_PID: AtomicI32 = AtomicI32::new(0);
static REAPER_STARTED: AtomicBool = AtomicBool::new(false);

/// 常驻服务的就绪等待上限。实测起进程约 2 s；首个请求才加载模型。
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);

fn free_port() -> Result<u16> {
    // 让系统挑一个空闲端口。**不用固定端口**：8765 被别的 app 占过
    //（v0.21.1 那次），固定端口迟早撞车。选完到 speech-server bind 之间
    // 有个小窗口可能被抢——那种情况下就绪等待会失败，下一次调用会换端口重来。
    let l = std::net::TcpListener::bind("127.0.0.1:0").context("找空闲端口失败")?;
    Ok(l.local_addr()?.port())
}

fn health_ok(port: u16) -> bool {
    Command::new("/usr/bin/curl")
        .args(["-s", "-m", "1", "-o", "/dev/null", "-w", "%{http_code}"])
        .arg(format!("http://127.0.0.1:{port}/health"))
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "200")
}

/// 记下我们拉起的 speech-server 的 pid。**给「上一次没收干净」兜底**：
/// 守护进程被 `kill -9`（v0.20.1 起 launchd 会把它拉回来）或崩溃时，
/// 信号处理函数没机会跑，常驻服务就成了一个没人管的 1–2.5 GiB 孤儿。
/// 下次启动时 [`reap_stale_server`] 按这个文件收掉它。
fn pid_file() -> Option<PathBuf> {
    root().map(|r| r.join("speech-server.pid"))
}

/// 收掉上一次留下的孤儿 speech-server。
///
/// **只杀确认是我们的那个**：pid 会被系统复用，所以先读这个 pid 的命令行，
/// 必须是我们运行时目录里的 `speech-server` 才动手——宁可漏收，不能误杀别人的进程。
pub fn reap_stale_server() {
    let Some(pf) = pid_file() else { return };
    let Ok(text) = fs::read_to_string(&pf) else { return };
    let _ = fs::remove_file(&pf);
    let Ok(pid) = text.trim().parse::<i32>() else { return };
    if pid <= 0 || Some(pid) == server_pid() {
        return;
    }
    let Some(bin) = server_bin() else { return };
    let cmdline = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if is_our_server_cmdline(&cmdline, &bin) {
        log::warn!("发现上一次留下的 Qwen3-ASR 常驻服务（pid {pid}），收掉");
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// 这条命令行是不是「我们运行时目录里的 speech-server」。纯函数，测试钉住。
fn is_our_server_cmdline(cmdline: &str, our_bin: &Path) -> bool {
    let bin = our_bin.to_string_lossy();
    !bin.is_empty() && cmdline.split_whitespace().any(|tok| tok == bin)
}

fn kill_server(s: &mut Server, why: &str) {
    log::info!("停掉 Qwen3-ASR 常驻服务（{}，端口 {}）：{why}", s.model.label(), s.port);
    SERVER_PID.store(0, Ordering::SeqCst);
    if let Some(pf) = pid_file() {
        let _ = fs::remove_file(pf);
    }
    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
    // 给它 2 s 自己退，不退就硬杀——别让一个卡住的进程攥着 2 GB 内存
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if matches!(s.child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = s.child.kill();
    let _ = s.child.wait();
}

/// 确保有一个加载着 `m` 的常驻服务在跑，返回端口。
///
/// 已有服务但**模型不同**：先停掉旧的再起新的——两个模型同时在一个进程里
/// 实测占 3.1 GiB，「换模型」不该让内存叠加。
fn ensure_server_locked(slot: &mut Option<Server>, m: Qwen3Model) -> Result<u16> {
    if let Some(s) = slot.as_mut() {
        let alive = matches!(s.child.try_wait(), Ok(None));
        if alive && s.model == m {
            // 请求开始时就刷新：空闲回收按「最后一次使用」算，一个正好在超时边上
            // 开始的请求不该在半路被回收。
            s.last_used = Instant::now();
            return Ok(s.port);
        }
        if alive {
            kill_server(s, "换了模型");
        } else {
            log::warn!("Qwen3-ASR 常驻服务已经退出了，重新拉起");
            SERVER_PID.store(0, Ordering::SeqCst);
        }
        *slot = None;
    }
    if !is_ready(m) {
        bail!("{} 还没装好，不能起常驻服务", m.label());
    }
    let bin = server_bin().context("数据目录未初始化")?;
    let port = free_port()?;
    let log_path = root().context("数据目录未初始化")?.join("speech-server.log");
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("打开 {} 失败", log_path.display()))?;
    let mut cmd = speech_command(&bin);
    cmd.args(["--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    reap_stale_server();
    let mut child = cmd.spawn().context("拉起 speech-server 失败")?;
    SERVER_PID.store(child.id() as i32, Ordering::SeqCst);
    // 注意：套了 sandbox-exec 时它会 exec 成 speech-server，pid 不变——记的就是服务本身
    if let Some(pf) = pid_file() {
        let _ = fs::write(pf, child.id().to_string());
    }
    let start = Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            SERVER_PID.store(0, Ordering::SeqCst);
            bail!("speech-server 启动后立刻退出了（{st}），日志：{}", log_path.display());
        }
        if health_ok(port) {
            break;
        }
        if start.elapsed() > SERVER_READY_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            SERVER_PID.store(0, Ordering::SeqCst);
            bail!("speech-server 在 {SERVER_READY_TIMEOUT:?} 内没就绪，日志：{}", log_path.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    log::info!(
        "Qwen3-ASR 常驻服务已起（{}，端口 {port}，{:.1}s）",
        m.label(),
        start.elapsed().as_secs_f64()
    );
    *slot = Some(Server { child, port, model: m, last_used: Instant::now() });
    start_reaper();
    Ok(port)
}

/// 用常驻服务转写。**调用方负责失败时退回逐次调用**（见 `SpeechSwiftEngine`）。
pub fn server_transcribe(
    m: Qwen3Model,
    wav: &Path,
    language: Option<&str>,
    context: &str,
) -> Result<crate::asr::Transcript> {
    let port = {
        let mut slot = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        ensure_server_locked(&mut slot, m)?
    };
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args(["-sS", "--fail-with-body", "--max-time", "120"])
        .arg("-F")
        .arg(format!("file=@\"{}\"", wav.display()))
        .arg("-F")
        .arg(format!("model={}", m.server_model()));
    if let Some(l) = language {
        cmd.arg("-F").arg(format!("language={l}"));
    }
    if !context.is_empty() {
        // `-F name=value` 里 value 以 `@`/`<` 开头会被 curl 当成读文件——
        // 术语表是用户可编辑的，用 `--form-string` 按字面量发送。
        cmd.arg("--form-string").arg(format!("context={context}"));
    }
    cmd.arg(format!("http://127.0.0.1:{port}/v1/audio/transcriptions"));
    let out = cmd.output().context("调用 speech-server 失败")?;
    {
        let mut slot = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = slot.as_mut() {
            s.last_used = Instant::now();
        }
    }
    if !out.status.success() {
        bail!(
            "speech-server 请求失败（curl {:?}）：{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    parse_server_json(&String::from_utf8_lossy(&out.stdout))
}

fn parse_server_json(body: &str) -> Result<crate::asr::Transcript> {
    let v: serde_json::Value =
        serde_json::from_str(body).with_context(|| format!("speech-server 返回的不是 JSON：{body}"))?;
    let Some(text) = v.get("text").and_then(|t| t.as_str()) else {
        bail!("speech-server 的返回里没有 text 字段：{body}");
    };
    Ok(crate::asr::Transcript { text: text.trim().to_string(), lang: None })
}

/// 预热：切到常驻时在后台起服务并加载模型，**别让第一轮去付那 1.6 s**。
pub fn warm_async(m: Qwen3Model) {
    std::thread::spawn(move || {
        let t = Instant::now();
        match smoke_wav().and_then(|w| server_transcribe(m, &w, None, "")) {
            Ok(_) => log::info!("Qwen3-ASR 常驻服务预热完成（{}，{:.1}s）", m.label(), t.elapsed().as_secs_f64()),
            Err(e) => log::warn!("Qwen3-ASR 常驻服务预热失败（下一轮会退回逐次调用）：{e:#}"),
        }
    });
}

/// 停掉常驻服务（关掉常驻开关、换回 SenseVoice、菜单退出时调用）。
pub fn stop_server(why: &str) {
    let mut slot = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut s) = slot.take() {
        kill_server(&mut s, why);
    }
}

/// 当前常驻服务的 pid（`--asr-bench` 读它的 RSS 用）。
pub fn server_pid() -> Option<i32> {
    Some(SERVER_PID.load(Ordering::SeqCst)).filter(|p| *p > 0)
}

/// 命令行子命令（`--transcribe` / `--talk-turn` …）用：`main` 返回时停掉
/// 这次拉起的常驻服务。不然一条一次性命令会留下一个占 1–2.5 GiB 的孤儿进程。
/// 守护进程走不到这里（`tray::run` 不返回），它的常驻服务由空闲回收 / 退出菜单 / 信号收掉。
pub struct ServerGuard;

impl Drop for ServerGuard {
    fn drop(&mut self) {
        stop_server("命令行子命令结束");
    }
}

/// **给信号处理函数调用**：只做 `kill(2)`，async-signal-safe。
pub fn kill_server_from_signal() {
    let pid = SERVER_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// 空闲回收：每 15 s 看一眼，常驻开关关了、后端换了、或者空闲超过
/// `qwen3_idle_secs`，就把服务停掉，把内存还给系统。
fn start_reaper() {
    if REAPER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(15));
        let cfg = crate::config::get();
        let mut slot = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = slot.as_mut() else { continue };
        let reason = reap_reason(
            cfg.qwen3_resident,
            cfg.asr_backend == crate::engine::AsrBackend::SpeechSwift,
            s.last_used.elapsed(),
            Duration::from_secs(cfg.qwen3_idle_secs),
        );
        if let Some(why) = reason {
            let mut s = slot.take().expect("上面刚确认过是 Some");
            kill_server(&mut s, why);
        }
    });
}

/// 该不该回收常驻服务。纯函数，真值表见测试。
fn reap_reason(resident: bool, backend_is_qwen3: bool, idle: Duration, limit: Duration) -> Option<&'static str> {
    if !resident {
        Some("常驻开关已关")
    } else if !backend_is_qwen3 {
        Some("识别引擎已换回 SenseVoice")
    } else if idle >= limit {
        Some("空闲超时")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_the_small_one() {
        // jason 2026-09-26：「默认小内存」
        assert_eq!(Qwen3Model::default(), Qwen3Model::Small);
    }

    #[test]
    fn model_names_roundtrip_through_serde_and_cli() {
        for m in Qwen3Model::ALL {
            let v = serde_json::to_value(m).unwrap();
            let s = v.as_str().unwrap();
            assert_eq!(Qwen3Model::parse_cli(s).unwrap(), m, "配置里的名字 {s} 在命令行也要认");
            assert_eq!(serde_json::from_value::<Qwen3Model>(v).unwrap(), m);
        }
        assert!(Qwen3Model::parse_cli("7b").is_err());
    }

    /// 清单自洽：每档 6 个文件、sha 是全量 64 位、没有重名、总量和 HF 声明一致。
    #[test]
    fn file_manifests_are_complete() {
        for m in Qwen3Model::ALL {
            let files = m.files();
            assert_eq!(files.len(), 6, "{} 应当正好是 speech 缓存里那 6 个文件", m.label());
            for f in files {
                assert_eq!(f.sha256.len(), 64, "{}/{} 的 sha 不是全量", m.label(), f.name);
                assert!(f.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
                assert!(f.bytes > 0);
            }
            let mut names: Vec<_> = files.iter().map(|f| f.name).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), 6, "重名文件");
            assert!(names.contains(&"model.safetensors"));
        }
        // ADR-0010 RAW §3 的 HF 声明值（去掉 README / .gitattributes 之后）
        assert_eq!(Qwen3Model::Small.total_bytes(), 712_779_703 - 1065 - 1519);
        assert_eq!(Qwen3Model::Large.total_bytes(), 2_467_857_518 - 1129 - 1519);
    }

    /// 服务端模型名必须显式、且两档不同——不传或传错会路由到别的模型并静默下载。
    #[test]
    fn server_model_names_are_explicit_and_distinct() {
        assert_ne!(Qwen3Model::Small.server_model(), Qwen3Model::Large.server_model());
        for m in Qwen3Model::ALL {
            assert!(m.server_model().starts_with("qwen3-asr-"));
        }
    }

    #[test]
    fn revisions_are_pinned_commits_not_branches() {
        for m in Qwen3Model::ALL {
            let r = m.revision();
            assert_eq!(r.len(), 40, "{} 要钉到 commit，不能是 main", m.label());
            assert!(r.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn runtime_pin_is_consistent() {
        assert!(RUNTIME_URL.contains(&format!("/download/{SPEECH_VERSION}/")), "URL 与版本常量要一致");
        assert_eq!(RUNTIME_SHA256.len(), 64);
    }

    /// pid 被复用时不能误杀：命令行必须**恰好**是我们运行时目录里的 speech-server。
    #[test]
    fn stale_server_is_recognised_only_by_our_exact_path() {
        let ours = Path::new("/Users/x/.agentear/models/qwen3/runtime/speech-v0.0.28/speech-server");
        let sandboxed = format!("/usr/bin/sandbox-exec -p (version 1) {} --host 127.0.0.1 --port 5000", ours.display());
        assert!(is_our_server_cmdline(&format!("{} --host 127.0.0.1 --port 5000", ours.display()), ours));
        assert!(is_our_server_cmdline(&sandboxed, ours));
        assert!(!is_our_server_cmdline("/opt/homebrew/bin/speech-server --port 8080", ours), "brew 的那个不是我们拉的");
        assert!(!is_our_server_cmdline("/usr/bin/vim notes.txt", ours));
        assert!(!is_our_server_cmdline("", ours));
        assert!(
            !is_our_server_cmdline(&format!("{}-evil --port 1", ours.display()), ours),
            "前缀相同的别的程序不算"
        );
    }

    #[test]
    fn reaper_truth_table() {
        let s = Duration::from_secs;
        assert_eq!(reap_reason(true, true, s(10), s(600)), None, "在用、没超时：留着");
        assert!(reap_reason(true, true, s(600), s(600)).is_some(), "到点就回收");
        assert!(reap_reason(false, true, s(1), s(600)).is_some(), "关了常驻立刻回收");
        assert!(reap_reason(true, false, s(1), s(600)).is_some(), "换回 SenseVoice 立刻回收");
    }

    #[test]
    fn server_json_is_parsed_and_errors_are_loud() {
        let t = parse_server_json("{\n  \"text\" : \" 今天清迈天气还不错 \"\n}").unwrap();
        assert_eq!(t.text, "今天清迈天气还不错");
        assert!(parse_server_json("<html>502</html>").is_err(), "不是 JSON 要报错，不能当空转写");
        assert!(parse_server_json("{\"error\":\"x\"}").is_err(), "没有 text 要报错");
    }

    /// 沙箱规则必须禁出站网络——这是「speech 不会背着我们去下模型」的唯一保证。
    #[test]
    fn sandbox_profile_denies_outbound_network() {
        assert!(SANDBOX_PROFILE.contains("deny network-outbound (remote tcp"));
        // 不能禁 UDP：实测会让 speech 判缓存无效、转而去下载（见常量上的注释）
        assert!(!SANDBOX_PROFILE.contains("remote udp"));
    }
}
