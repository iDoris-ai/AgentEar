//! Qwen3-ASR 可选后端的**安装与常驻服务**（T3.5.6，ADR-0010）。
//!
//! ## 这个模块管什么
//!
//! 1. **运行时**：speech-swift 的预编译包（`speech` / `speech-server`），
//!    **版本钉死**（[`SPEECH_VERSION`] + tarball 的 sha256）、校验、解包到
//!    `~/.agentear/models/qwen3/runtime/speech-<版本>/`。**不走 brew、不随包分发**
//!    （jason 2026-09-26 拍板）。
//!    ⚠️ **T3.5.9 方案 B-lite（2026-10 起）：这个 tarball 不再是上游官方发布的那份**。
//!    上游 speech-server 每次请求后不释放 MLX 用过的 Metal 缓冲区，常驻服务会越用
//!    越涨（见 [`SERVER_FOOTPRINT_BUDGET`] 上的实测数字）。我们改成**自己从上游
//!    pinned 源码 + 一个小补丁编译 `speech-server`**（补丁见
//!    `patches/speech-swift-v0.0.28-agentear.patch`，构建脚本见
//!    `scripts/build-speech-runtime.sh`），**只换这一个二进制**，`speech` CLI /
//!    `mlx.metallib` / 各个 bundle 仍是官方发布包里原样那份（省掉自己装 Metal
//!    Toolchain 现编 `.metallib` 的麻烦）。产物发布在 AgentEar 自己的 GitHub
//!    release 上（`speech-runtime-<版本>` 这个 tag），`RUNTIME_URL` 指向那里，
//!    不再指向 `soniqo/speech-swift` 的 release。
//! 2. **模型权重**：0.6B / 1.7B 两档，**直接从 Hugging Face 下载**，
//!    钉在某个 commit（`resolve/<revision>/`），逐文件 sha256 校验，
//!    按 speech 认的布局 `$QWEN3_ASR_CACHE_DIR/qwen3-speech/models/<org>/<repo>/`
//!    摆好。之后 speech **不再联网**（它的下载器永远不会被触发）。
//!    **升级运行时不影响模型**——模型安装判据（[`model_installed`]）与
//!    [`SPEECH_VERSION`] 无关，不会因为换了运行时版本就被判定成「没装」。
//! 3. **常驻服务**（可选，菜单栏开关）：`speech-server` 热态约 0.1–0.2 s 一轮，
//!    代价是常驻约 2 GiB 起（含 Metal 缓冲区）；空闲超时自动退出，退出/信号时收掉；
//!    **每轮转完仍保留「超过 4 GiB 就重启」的最后一道保险**（见
//!    [`SERVER_FOOTPRINT_BUDGET`] 上的注释——方案 B-lite 修的是常态下的累积，
//!    不是单次超长录音的峰值，也不假设补丁在所有情况下都生效）。
//!
//! ## 为什么所有 speech 进程都套一层「断网沙箱」
//!
//! speech 找不到模型时会**静默下载**：ADR-0010 调研时 speech-server 在一个
//! 没带模型名的请求上当场下了 611 MB 的英文模型（RAW §4）；`-m <本地目录>`
//! 会被当成仓库名、重试 5 次约 2 分钟才报错。我们的承诺是「模型由我们下、
//! 校验过的那份才用」，所以 speech 的每一次启动都跑在
//! `sandbox-exec` 的「禁止出站 TCP」里：布局错了就**当场报错**，
//! 不会在用户不知情的情况下去 Hugging Face 拉几百 MB。
//!
//! ⚠️ **它挡的只是「静默联网下载（TCP）」，不是隐私边界**：profile 是
//! `(allow default)` + 禁出站 TCP，**文件系统全开、出站 UDP 也开着**
//! （PR #93 评审实测：沙箱里 `dig @1.1.1.1` 能通，TCP 到 huggingface.co 是 `curl: (7)`；
//! 为什么不连 UDP 一起禁，见下面 `speech_command` 处的说明）。
//! 所以别把它写成「不会把录音相关的任何东西送出去」——那是它做不到的承诺。
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

/// speech-swift 的版本。**升级 = 改这里 + 下面几个常量 + 本机实测一遍**。
///
/// 钉死而不是跟最新（jason 2026-09-26 拍板）：上游约一个多月出了 5 版，
/// CLI 参数变过；`SpeechSwiftEngine::preflight` 的契约探测只能挡住一部分。
///
/// ⚠️ **T3.5.9 方案 B-lite（2026-10-03）起，这个字符串不再是「照抄上游的 tag」**：
/// 上游基线仍是 [`UPSTREAM_TAG`]，但我们发布的是自己编译 + 打了补丁的运行时
/// （修「常驻越用越涨」那个内存泄漏，见 [`SERVER_FOOTPRINT_BUDGET`]），
/// 版本号里带 `-agentear.<修订号>` 以便跟官方未修改的包区分。
/// **目录名 / 安装记录都从这个字符串派生**（[`runtime_dir`]/[`runtime_marker`]），
/// 所以改它会让所有现有安装（不管是不是真的换了上游版本）都判定为「没装」、
/// 重新下载——升级时这正是想要的效果：旧版本号对应的运行时有那个内存泄漏，
/// 不该被新代码当成「已经装好」。
pub const SPEECH_VERSION: &str = "v0.0.28-agentear.1";
/// 上游 pinned 的基线 tag/commit（`patches/speech-swift-v0.0.28-agentear.patch`
/// 打在这个 tag 上）。只用于日志和文档，不参与安装判据。
pub const UPSTREAM_TAG: &str = "v0.0.28";
/// 我们自己发布的运行时 tarball——**不是**上游 `soniqo/speech-swift` 的 release。
/// 由 `scripts/build-speech-runtime.sh` 产出，发布在 AgentEar 自己的 GitHub
/// release 上（tag `speech-runtime-v0.0.28-agentear.1`）。
const RUNTIME_URL: &str = "https://github.com/iDoris-ai/AgentEar/releases/download/speech-runtime-v0.0.28-agentear.1/speech-macos-arm64-v0.0.28-agentear.1.tar.gz";
/// 2026-10-03 实测（`scripts/build-speech-runtime.sh` 的输出，`shasum -a 256`）。
const RUNTIME_SHA256: &str = "58f3701e663b157d1da427af8257d4cb44ac51f839b0401f3380f92c8e28f2d1";
const RUNTIME_BYTES: u64 = 99_502_348;
/// 解包后体积（实测 `du -sk` ≈ 382.9 MB），磁盘预检用，往上取整留余量。
const RUNTIME_UNPACKED_BYTES: u64 = 420_000_000;
/// 常驻服务的 Metal 缓存上限（MB），传给打了补丁的 `speech-server`
/// （`AGENTEAR_MLX_CACHE_MB` 环境变量，见 `patches/speech-swift-v0.0.28-agentear.patch`）。
/// 不设这个变量时补丁版行为与官方版一致（即不限）——**AgentEar 自己起
/// speech-server 时必须显式传它**，取值依据与 TTS 边车 v0.17.0 一致（256 MB 折中：
/// 留一点够复用、又不会无限长；设成 0 反而更慢）。
///
/// 实测依据：`docs/data/qwen3-memory-2026-10/README.md`——25 段真实录音 ×3 轮，
/// 设了这个变量后末态 ≈ 865 MB（不设时 47–49 GB）。
pub const AGENTEAR_MLX_CACHE_MB: u32 = 256;

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

// ⚠️ **升级后旧版本号对应的目录（比如方案 B-lite 之前的 `speech-v0.0.28/`）
// 不会被自动删**——这是刻意的，不是漏做：
// ① 按版本号字符串拼目录名、再拿这个名字去删一个目录，删错的代价
//    （删掉用户机器上一个我们不知道内容的路径）比它省下的磁盘
//    （运行时解包后约 370 MB）大得多；
// ② 这个模块从没写过「删除某个版本目录」的代码路径，第一次写就让它
//    在生产环境删文件，风险收益不对称；
// ③ 用户确认新版本能用之后，想清的话自己删
//    `~/.agentear/models/qwen3/runtime/speech-<旧版本号>/` 就行。
// 真要自动清理，应该是一个单独评估的任务（枚举 runtime/ 下的目录、
// 排除当前 SPEECH_VERSION、确认不是正在用的那个再删），不是升级逻辑的一部分。

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

/// T3.5.9 方案 B-lite 续（PR #106 评审 CHANGES_REQUESTED，阻塞项）：升级空档恢复
/// 「跨启动仍能重试」的标记。**故意不用 `asr_backend` 当判据**——`main.rs` 的
/// preflight 退回逻辑会把 `asr_backend` 持久化成 `builtin`（为了让这一轮的
/// Dispatch 正常工作），如果下一次启动只看 `asr_backend` 来判断「要不要继续
/// 恢复」，这条线索会在它被改写的那一刻起永久丢失：进程被杀、崩溃，或者升级后
/// 第一次调用恰好是一次性 CLI 且没跑完下载，往后就再也没有人会去后台补那 99 MB
/// 的运行时了——用户永久卡在 SenseVoice 上，这正是评审抓到的那个 bug。
/// 这个标记专门记「还在等运行时补上」这件事，生命周期与 `asr_backend` 分开：
/// 只在确认装好（[`runtime_installed`] 真的变 true）之后才清掉。
fn recovering_marker() -> Option<PathBuf> {
    root().map(|r| r.join("runtime-recovering.json"))
}

/// 读标记。文件不存在、读不出来、或者内容不是一个认得的模型名，都当「没有在
/// 恢复」——宁可漏一次重试，也不要让一个偶然损坏的标记文件变成硬错误。
pub fn read_recovering_marker() -> Option<Qwen3Model> {
    let p = recovering_marker()?;
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写标记：原子写（tmp + rename），复用 [`write_marker`] 同一套安全写法。
/// 失败只记日志、不阻断这一轮的退回——标记只是「跨启动的记忆」，
/// 写不进去也不该让这一轮的 ASR 用不了。
pub fn write_recovering_marker(m: Qwen3Model) -> Result<()> {
    let p = recovering_marker().context("数据目录未初始化")?;
    let content = serde_json::to_string(&m).context("序列化模型名失败")?;
    write_marker(&p, &content)
}

/// 清标记：运行时真的补上了（不管最后有没有真的切回 speech_swift——
/// 用户可能在下载期间手动选了别的引擎）。`is_ready` 这时已经会重新变 true，
/// 「空档」本身不存在了，标记没必要留着等下次启动误触发重试。
pub fn clear_recovering_marker() {
    if let Some(p) = recovering_marker() {
        let _ = fs::remove_file(p);
    }
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
    // 任何一次成功安装（不管是 `start` 的后台线程、`install_blocking` 的
    // `--fetch-qwen3`，还是哪个模型）都会先确保共享的运行时装好（上面
    // `need_runtime` 那一段），所以这里是「运行时现在肯定装好了」的唯一汇合点：
    // 不管升级恢复标记当初记的是不是这个 `m`，运行时装好之后那个标记都已经
    // 没有意义了（PR #106 评审第 3 轮阻塞项：「on_qwen3_installed /
    // install_blocking 成功之后也要清」，放在这个共用的核心函数里比在每个
    // 调用方分别清更不容易漏）。
    clear_recovering_marker();
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
    // T3.5.9 方案 B-lite：只有我们自己编译 + 打了补丁的 speech-server 认这个
    // 环境变量（见 patches/speech-swift-v0.0.28-agentear.patch）；
    // 官方未修改的 `speech` CLI / 旧版 speech-server 不会读它，设了也无害。
    // 设在这个共用的构造函数里而不是只在起常驻服务那处设，是因为一次性调用
    // （`--transcribe` 走的那条 CLI 路径）如果有一天也走到打了补丁的二进制，
    // 不该因为少设了这个变量而表现不一致。
    cmd.env("AGENTEAR_MLX_CACHE_MB", AGENTEAR_MLX_CACHE_MB.to_string());
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
    /// 正在这个服务上跑的请求数（持 `SERVER` 锁增减）。内存回收只在**最后一个**请求结束时判断，
    /// 不然预热和真实请求叠在一起时，先结束的那个会把另一个正在用的服务杀掉。
    inflight: u32,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// 同一个 pid 的无锁副本，给信号处理函数用（async-signal-safe：只能 `kill(2)`）。
static SERVER_PID: AtomicI32 = AtomicI32::new(0);
static REAPER_STARTED: AtomicBool = AtomicBool::new(false);
/// 内存回收放到后台去「等退出 / 硬杀」的线程。`stop_server` 会 join 它们，
/// 一次性命令退出前因此仍有「2 s 不退就硬杀」的兜底（PR #104 评审）。
static TERMINATORS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());
/// 内存回收后要不要马上在后台起一个新的。**只有守护进程打开**（`enable_rewarm`）：
/// 一次性命令马上就退出了，多起一个约 2 GiB 的服务只会拖慢退出（PR #104 评审）。
static REWARM: AtomicBool = AtomicBool::new(false);

/// 守护进程启动时调用：内存回收之后在后台预热一个新的常驻服务。
pub fn enable_rewarm() {
    REWARM.store(true, Ordering::SeqCst);
}

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
///
/// ## 为什么按「拥有者」分文件（v0.23.1，PR #93 评审 N1）
///
/// v0.23.0 只有一个 `speech-server.pid`，守护进程和命令行子命令
/// （`--transcribe` / `--talk-turn` / `--asr-bench`）共用：常驻开着时跑一条命令行，
/// 它会把**守护进程正在用的**常驻服务当成孤儿杀掉、再把文件改写成自己的；
/// 命令行退出时又不看内容就删文件，守护进程的孤儿兜底也跟着没了。
/// 现在每个拉起服务的 AgentEar 进程写自己的 `speech-server-<拥有者 pid>.pid`，
/// 内容是 `<拥有者 pid> <服务 pid>`；**拥有者还活着、且不是自己，就不碰**。
fn pid_file_for(dir: &Path, owner: i32) -> PathBuf {
    dir.join(format!("speech-server-{owner}.pid"))
}

/// v0.23.0 的旧文件名（没有拥有者信息）。升级后第一次启动时按老规矩收一次。
const LEGACY_PID_FILE: &str = "speech-server.pid";

/// 解析 pid 文件：`<拥有者> <服务>`，或 v0.23.0 的旧格式 `<服务>`（拥有者未知）。
fn parse_pid_file(text: &str) -> Option<(Option<i32>, i32)> {
    let mut it = text.split_whitespace();
    let first = it.next()?.parse::<i32>().ok()?;
    match it.next() {
        None => Some((None, first)),
        Some(second) => Some((Some(first), second.parse::<i32>().ok()?)),
    }
}

/// 对一条 pid 记录该怎么办。纯函数，测试钉住（真值表见 `reap_verdict_truth_table`）。
#[derive(Debug, PartialEq, Eq)]
enum ReapVerdict {
    /// 别人的（拥有者还活着）或自己正在用的：文件和进程都不碰。
    Keep,
    /// 孤儿：命令行确认是我们的 speech-server 才杀，然后删文件。
    KillAndRemove,
    /// 记录本身坏了：只删文件。
    RemoveOnly,
}

fn reap_verdict(
    owner: Option<i32>,
    server: i32,
    me: i32,
    current_server: Option<i32>,
    owner_alive: bool,
) -> ReapVerdict {
    if server <= 0 {
        return ReapVerdict::RemoveOnly;
    }
    if Some(server) == current_server {
        return ReapVerdict::Keep;
    }
    match owner {
        // 另一个还活着的 AgentEar（守护进程 ↔ 命令行）的服务：不是孤儿
        Some(o) if o != me && owner_alive => ReapVerdict::Keep,
        // 拥有者死了 / 是自己但已经不是当前那个 / 旧格式不知道拥有者
        _ => ReapVerdict::KillAndRemove,
    }
}

/// 拥有者还在不在：进程存在，**且命令行里带 agentear**。
/// 只看「进程存在」不够——拥有者的 pid 也会被系统复用；复用的话我们判「还活着」，
/// 后果是这次漏收一个孤儿（下次拥有者 pid 空出来再收），方向是安全的。
fn owner_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    // 进程在、但读不到命令行（`ps` 起不来）：**当它还活着**——
    // 判错成「死了」就会去收一个可能正在被用的服务，那是误杀方向；判成活着只是这次漏收。
    exists && cmdline_of(pid).map_or(true, |c| c.to_lowercase().contains("agentear"))
}

/// 读进程命令行。`None` = **读不到**（`ps` 起不来），与「读到了、是空的」（进程已不在）分开。
fn cmdline_of(pid: i32) -> Option<String> {
    Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// 收掉上一次留下的孤儿 speech-server。
///
/// **只收孤儿**：拥有者（拉起它的那个 AgentEar 进程）还活着就不碰，
/// 所以守护进程和命令行子命令不会再互相杀对方的常驻服务。
/// **真动手之前再核命令行**：pid 会被系统复用，必须是我们运行时目录里的
/// `speech-server` 才发信号——宁可漏收，不能误杀别人的进程。
pub fn reap_stale_server() {
    let Some(dir) = root() else { return };
    // PR #106 评审非阻塞②：传运行时根目录（`runtime/`），不是当前版本那一个
    // 具体的二进制路径——见 `is_our_server_cmdline` 上的注释。
    let runtime_root = dir.join("runtime");
    reap_stale_in(&dir, &runtime_root, std::process::id() as i32, server_pid());
}

fn reap_stale_in(dir: &Path, runtime_root: &Path, me: i32, current_server: Option<i32>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let is_pid_file = name == LEGACY_PID_FILE
            || (name.starts_with("speech-server-") && name.ends_with(".pid"));
        if !is_pid_file {
            continue;
        }
        let path = e.path();
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Some((owner, server)) = parse_pid_file(&text) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let alive = owner.is_some_and(owner_alive);
        match reap_verdict(owner, server, me, current_server, alive) {
            ReapVerdict::Keep => {}
            ReapVerdict::RemoveOnly => {
                let _ = fs::remove_file(&path);
            }
            ReapVerdict::KillAndRemove => {
                // 读不到命令行就不杀（宁可漏收）
                if cmdline_of(server).is_some_and(|c| is_our_server_cmdline(&c, runtime_root)) {
                    log::warn!("发现上一次留下的 Qwen3-ASR 常驻服务（pid {server}），收掉");
                    unsafe { libc::kill(server, libc::SIGTERM) };
                }
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// 删自己那份 pid 文件——**只删内容里的服务 pid 就是 `server` 的那份**。
fn remove_own_pid_file(server: i32) {
    let Some(dir) = root() else { return };
    let pf = pid_file_for(&dir, std::process::id() as i32);
    if let Ok(text) = fs::read_to_string(&pf) {
        if parse_pid_file(&text).is_some_and(|(_, s)| s == server) {
            let _ = fs::remove_file(pf);
        }
    }
}

/// 这条命令行是不是「我们 qwen3 运行时根目录下**任意版本**的 speech-server」。
/// 纯函数，测试钉住。
///
/// ⚠️ **PR #106 评审非阻塞②**：原来只认当前 `SPEECH_VERSION` 对应的那一个
/// 具体路径（`tok == our_bin`）。升级之后，旧版本号对应的运行时目录故意不删
/// （见 `runtime_dir` 上的注释），如果里面留着一个孤儿 `speech-server`，
/// 旧判法会因为路径对不上**当前**版本而永远认不出它——`reap_verdict` 已经判了
/// `KillAndRemove`，pid 文件会被删，但因为这里核对不过，进程本身杀不掉，
/// 于是「孤儿还活着、pid 文件却没了」，之后再没有人记得去收它。
/// 现在只要求「在我们自己的运行时根目录下、文件名恰好是 `speech-server`」——
/// **仍然不认别的程序**：根目录前缀必须匹配（不是别的运行时目录），
/// 文件名必须整段相等（不是 `speech-server-evil` 这种前缀碰巧对上的）。
fn is_our_server_cmdline(cmdline: &str, runtime_root: &Path) -> bool {
    if runtime_root.as_os_str().is_empty() {
        return false;
    }
    // PR #106 评审第 3 轮非阻塞项：原来用字符串 `starts_with` 判断「在不在
    // 运行时根目录下」，`.../runtime-evil/speech-server` 这种前缀字符串碰巧
    // 对上、但实际在**另一个目录**的路径会被误判成「我们的」（字符串前缀
    // 不等于路径分量前缀：`"…/runtime-evil"` 的字符串确实以 `"…/runtime"`
    // 开头）。改成 `Path::starts_with`——按路径分量比较，`runtime-evil`
    // 和 `runtime` 是两个不同的分量，不会再被误判。
    cmdline.split_whitespace().any(|tok| {
        let p = Path::new(tok);
        p.starts_with(runtime_root) && p.file_name().is_some_and(|f| f == "speech-server")
    })
}

fn kill_server(s: &mut Server, why: &str) {
    retire_server(s, why);
    await_exit(s);
}

/// 撤登记（`SERVER_PID` + pid 文件）**并发出 SIGTERM**。**必须在持 `SERVER` 锁时、同步做**：
/// - 撤登记放后台线程的话，新服务可能已经登记过了，这里会把**新服务**的 `SERVER_PID`
///   清成 0——之后 Ctrl+C 就收不掉它。
/// - SIGTERM 放后台线程的话（PR #104 评审阻塞项）：一次性命令（`--transcribe`）在唯一一次
///   请求就触发回收、`main` 随即返回，没被 join 的线程若还没被调度到，信号就**发不出去**
///   （竞态窗口很短，旧代码实测 3 次没触发，但读代码成立）；
///   而 pid 文件已经删了，下次启动也找不到这个 >4 GiB 的孤儿。`kill(2)` 不阻塞，同步发没有代价。
fn retire_server(s: &Server, why: &str) {
    log::info!("停掉 Qwen3-ASR 常驻服务（{}，端口 {}）：{why}", s.model.label(), s.port);
    let pid = s.child.id() as i32;
    let _ = SERVER_PID.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    remove_own_pid_file(pid);
    unsafe { libc::kill(pid, libc::SIGTERM) };
}

/// 已经发过 SIGTERM 之后：等 2 s，不退就硬杀并回收。可以放后台线程（只碰这个 `Child` 自己）。
fn await_exit(s: &mut Server) {
    // 给它 2 s 自己退，不退就硬杀——别让一个卡住的进程攥着几个 GB 内存
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
/// 返回 `(端口, 服务 pid)`。pid 用来认「回来时还是不是同一个服务」——端口会被系统复用，pid 在
/// `Child` 被回收前不会。
fn ensure_server_locked(slot: &mut Option<Server>, m: Qwen3Model) -> Result<(u16, u32)> {
    if let Some(s) = slot.as_mut() {
        let alive = matches!(s.child.try_wait(), Ok(None));
        if alive && s.model == m {
            // 请求开始时就刷新：空闲回收按「最后一次使用」算，一个正好在超时边上
            // 开始的请求不该在半路被回收。
            s.last_used = Instant::now();
            s.inflight += 1;
            return Ok((s.port, s.child.id()));
        }
        if alive {
            // 不看 inflight：用户刚在设置里换了模型，旧模型上还没跑完的那个请求失败后会退回逐次调用
            //（`SpeechSwiftEngine`），不丢转写；等它跑完再换会让新模型的第一轮白等。
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
    if let Some(dir) = root() {
        let me = std::process::id() as i32;
        let _ = fs::write(pid_file_for(&dir, me), format!("{me} {}", child.id()));
    }
    let start = Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            // try_wait 已经把它回收了——这一条没法「先清后收」，窗口只有这几行
            SERVER_PID.store(0, Ordering::SeqCst);
            remove_own_pid_file(child.id() as i32);
            bail!("speech-server 启动后立刻退出了（{st}），日志：{}", log_path.display());
        }
        if health_ok(port) {
            break;
        }
        if start.elapsed() > SERVER_READY_TIMEOUT {
            // 与 kill_server 同一个顺序：先撤登记（SERVER_PID + pid 文件），再杀、再回收
            SERVER_PID.store(0, Ordering::SeqCst);
            remove_own_pid_file(child.id() as i32);
            let _ = child.kill();
            let _ = child.wait();
            bail!("speech-server 在 {SERVER_READY_TIMEOUT:?} 内没就绪，日志：{}", log_path.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    log::info!(
        "Qwen3-ASR 常驻服务已起（{}，端口 {port}，{:.1}s）",
        m.label(),
        start.elapsed().as_secs_f64()
    );
    let pid = child.id();
    *slot = Some(Server { child, port, model: m, last_used: Instant::now(), inflight: 1 });
    start_reaper();
    Ok((port, pid))
}

/// 用常驻服务转写。**调用方负责失败时退回逐次调用**（见 `SpeechSwiftEngine`）。
pub fn server_transcribe(
    m: Qwen3Model,
    wav: &Path,
    language: Option<&str>,
    context: &str,
) -> Result<crate::asr::Transcript> {
    transcribe_on_server(m, wav, language, context, true)
}

/// `rewarm`：因内存回收之后要不要马上在后台起一个新的。**预热自己传 `false`**——
/// 不然「预热完就超上限」会变成回收→预热→回收的死循环。
fn transcribe_on_server(
    m: Qwen3Model,
    wav: &Path,
    language: Option<&str>,
    context: &str,
    rewarm: bool,
) -> Result<crate::asr::Transcript> {
    let (port, server) = {
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
    // 先别 `?`：不管成败都要先把 inflight 减回去，否则这个服务再也不会被回收
    let out = cmd.output();
    let recycled = {
        let mut slot = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        // 只认「这次用的那个」：期间换过服务（模型变了、被重启过）就不是它了
        let mut last_one = false;
        if let Some(s) = slot.as_mut().filter(|s| s.child.id() == server) {
            s.last_used = Instant::now();
            s.inflight = s.inflight.saturating_sub(1);
            last_one = s.inflight == 0;
        }
        // 还有别的请求在用就先不判，留给最后结束的那个
        let footprint = slot.as_ref().filter(|_| last_one).and_then(|s| footprint_bytes(s.child.id() as i32));
        match recycle_reason(footprint, SERVER_FOOTPRINT_BUDGET) {
            Some(why) => {
                let mut s = slot.take().expect("last_one 已确认是 Some");
                retire_server(&s, &why);
                // 等它退出最多 2 s，放到后台，别把这一轮的上屏拖慢；句柄交给 stop_server 去 join
                let h = std::thread::spawn(move || await_exit(&mut s));
                let mut t = TERMINATORS.lock().unwrap_or_else(|e| e.into_inner());
                t.retain(|h| !h.is_finished());
                t.push(h);
                true
            }
            None => false,
        }
    };
    if recycled && rewarm && REWARM.load(Ordering::SeqCst) {
        // 下一轮别去付那 2 s 冷启动：后台马上起一个干净的（热态约 2 GiB）
        let cfg = crate::config::get();
        if reap_reason(
            cfg.qwen3_resident,
            cfg.asr_backend == crate::engine::AsrBackend::SpeechSwift,
            Duration::ZERO,
            Duration::from_secs(cfg.qwen3_idle_secs),
        )
        .is_none()
        {
            warm_async(m);
        }
    }
    let out = out.context("调用 speech-server 失败")?;
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
        match smoke_wav().and_then(|w| transcribe_on_server(m, &w, None, "", false)) {
            Ok(_) => log::info!("Qwen3-ASR 常驻服务预热完成（{}，{:.1}s）", m.label(), t.elapsed().as_secs_f64()),
            Err(e) => log::warn!("Qwen3-ASR 常驻服务预热失败（下一轮会退回逐次调用）：{e:#}"),
        }
    });
}

/// 停掉常驻服务（关掉常驻开关、换回 SenseVoice、菜单退出时调用）。
pub fn stop_server(why: &str) {
    {
        let mut slot = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut s) = slot.take() {
            kill_server(&mut s, why);
        }
    }
    // 内存回收留在后台的那些也等完（SIGTERM 早就同步发过了，这里补「2 s 不退就硬杀」）
    let pending = std::mem::take(&mut *TERMINATORS.lock().unwrap_or_else(|e| e.into_inner()));
    for h in pending {
        let _ = h.join();
    }
}

/// 当前常驻服务的 pid（`--asr-bench` 读它的 RSS 用）。
pub fn server_pid() -> Option<i32> {
    Some(SERVER_PID.load(Ordering::SeqCst)).filter(|p| *p > 0)
}

/// 命令行子命令（`--transcribe` / `--talk-turn` …）用：`main` 返回时停掉
/// 这次拉起的常驻服务，并等完内存回收留在后台的那些。不然一条一次性命令会留下
/// 一个占 2 GiB 起（按 `phys_footprint` 算）的孤儿进程。
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
        // 正在转写就不回收：空闲上限最短 60 s，而一次请求的超时是 120 s，「空闲」可能在请求中途到点
        if s.inflight > 0 {
            continue;
        }
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

/// 常驻服务的内存上限 = ASR 高资源档的预算（CLAUDE.md「ASR 侧分档」：≤4 GiB）。
///
/// ## 为什么要它（v0.26.2，jason 2026-10-02：「开始很快，越用越慢，一句话要好几分钟」）
///
/// speech-server（speech-swift v0.0.28）**每次请求用过的 Metal 缓冲区都不还给系统**，
/// 而它没有任何限制缓存的参数。实测（25 段 jason 当天的真实录音，0.6B，同一个进程）：
/// 3.4 GB → **48 GB**；单段 112 s 的录音在全新进程里就到 15 GB。
/// 空闲回收（`qwen3_idle_secs`，600 s）管不住——只要每隔几分钟说一句就永远不触发。
/// jason 那台机器上它涨到 **30 GB**，把系统推进 26.8 GB swap，
/// 于是 30 s 的录音要转 97–237 s。
///
/// 所以每轮转完看一眼它的 `phys_footprint`，超了就重启。
/// ⚠️ **看的是 footprint 不是 RSS**：这块内存记在 IOAccelerator（Metal）名下，
/// `ps` 的 RSS 只报了几十 MB——`--asr-bench` 以前记的「0.6B ≈ 816 MiB」就是这么少算的。
/// ⚠️ **它管的是「累积」，管不住单次峰值**：一段很长的录音在那一次请求里照样会冲到
/// 十几 GB（上游行为），只是转完马上还回去。
///
/// ## T3.5.9 方案 B-lite 之后，这条还要留着（2026-10-03）
///
/// 方案 B 的补丁（`AGENTEAR_MLX_CACHE_MB`）已经让「从外面重启」不再是日常路径——
/// 实测补丁版 25 段 ×3 轮末态 ≈ 865 MB，不再累积（见
/// `docs/data/qwen3-memory-2026-10/README.md`）。但这道「超了就重启」**不删**，
/// 当成最后一道保险：① 它不依赖上游/我们的运行时行为对不对——哪天运行时的来源
/// 又换了（比如回退到官方包、或者补丁哪天失效），这道闸照样兜得住；
/// ② 单次超长录音的峰值管不住是补丁管不了的那部分（见上一段），这道闸是唯一的
/// 后盾；③ 代码已经在生产跑过，删掉换不来什么、只换来一个新的风险窗口。
const SERVER_FOOTPRINT_BUDGET: u64 = 4 << 30;

/// 进程的 `phys_footprint`（活动监视器「内存」那一列；含 Metal 缓冲区）。读不到给 `None`。
pub fn footprint_bytes(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    (rc == 0).then_some(info.ri_phys_footprint)
}

/// 转完一轮后该不该因为内存重启常驻服务。纯函数，见测试。
/// 读不到 footprint 时**不重启**：宁可漏一次，不因为读数失败让每轮都冷启动。
fn recycle_reason(footprint: Option<u64>, budget: u64) -> Option<String> {
    let f = footprint?;
    (f > budget).then(|| {
        format!("内存 {:.1} GiB 超过上限 {:.0} GiB（Metal 缓存不释放）", f as f64 / (1u64 << 30) as f64, budget as f64 / (1u64 << 30) as f64)
    })
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

/// T3.5.9 方案 B-lite 续（PR #106 评审阻塞项）：升级空档恢复要不要重试、
/// 要不要真的发起后台下载——纯函数，调用方（`main.rs`）只负责算好这四个
/// 输入、照返回值去动作，所有判断逻辑都在这里，真值表见测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// 不是恢复场景：没有空档，或者既没有标记也没有「配置里选着 speech_swift」
    /// 这条线索——比如用户从没装过，或者早就手动切到别的引擎了。
    None,
    /// 是恢复场景，但这个进程不该发起下载（一次性 CLI）：记下标记，
    /// 这一轮本地退回 builtin（不改配置、不起下载线程），交给下一次
    /// 守护进程启动去接着做。
    Note,
    /// 是恢复场景，且该发起下载（守护进程）：写标记、退回 builtin、后台下载。
    Download,
}

/// - `gap`：这个模型「装过但现在用不了，且运行时缺失」
///   （`model_installed ∧ !runtime_installed ∧ !is_ready`，调用方算好传进来）。
/// - `marker_present`：磁盘上有没有「上一次恢复被中断」的标记
///   （[`read_recovering_marker`]）。
/// - `configured_backend_is_speech_swift`：**只在没有标记时才看它**——
///   首次发现空档（用户本来就选着 speech_swift，这一轮刚发现装不上）靠它
///   判断「这是该恢复的场景」；有标记之后就不再依赖它，因为它随时可能已经被
///   同一次 preflight 的退回逻辑改写成 `builtin`（这正是这次修的 bug：不能让
///   「标记还在、但配置已经被我们自己改掉」被误判成「不是恢复场景」）。
/// - `is_daemon`：只有长驻的守护进程才发起下载线程——一次性 CLI 子命令等不到
///   下载跑完就退出了（运行时约 99 MB，守护进程里实测几秒钟内能完成，但一次性
///   命令通常比这个还快），交给下一次守护进程启动去接着做。
pub fn recovery_decision(
    gap: bool,
    marker_present: bool,
    configured_backend_is_speech_swift: bool,
    is_daemon: bool,
) -> RecoveryAction {
    if !gap || (!marker_present && !configured_backend_is_speech_swift) {
        return RecoveryAction::None;
    }
    if is_daemon {
        RecoveryAction::Download
    } else {
        RecoveryAction::Note
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
        // 方案 B-lite 起 RUNTIME_URL 指向 AgentEar 自己的 release（tag 是
        // `speech-runtime-<SPEECH_VERSION>`），不再是 `/download/{SPEECH_VERSION}/`
        // 这种上游的路径形状——但版本号本身必须原样出现在 URL 里，
        // 不然改了 SPEECH_VERSION 忘改 URL 这种事检不出来。
        assert!(RUNTIME_URL.contains(SPEECH_VERSION), "URL 里要原样带着版本号");
        assert!(RUNTIME_URL.contains("iDoris-ai/AgentEar"), "方案 B-lite：运行时由我们自己发布，不是上游的 release");
        assert_eq!(RUNTIME_SHA256.len(), 64);
    }

    /// `AGENTEAR_MLX_CACHE_MB` 必须真的被传给每一个 speech 子进程——这是
    /// 补丁生效的唯一开关（不设 = 补丁版行为等同官方版，常驻服务照样会涨到几十 GB）。
    #[test]
    fn speech_command_always_sets_the_cache_limit_env() {
        let cmd = speech_command(Path::new("/tmp/does-not-matter"));
        let val = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("AGENTEAR_MLX_CACHE_MB"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().to_string());
        assert_eq!(val, Some(AGENTEAR_MLX_CACHE_MB.to_string()));
    }

    /// pid 被复用时不能误杀：命令行必须真的是我们 qwen3 运行时根目录下的
    /// 某个 `speech-server`。**升级后旧版本号的目录也要认得出**（PR #106
    /// 评审非阻塞②）——不再要求与当前版本的路径逐字相同。
    #[test]
    fn stale_server_is_recognised_under_our_runtime_root_any_version() {
        let root = Path::new("/Users/x/.agentear/models/qwen3/runtime");
        let current = root.join("speech-v0.0.28-agentear.1/speech-server");
        let old = root.join("speech-v0.0.28/speech-server");
        let sandboxed = format!("/usr/bin/sandbox-exec -p (version 1) {} --host 127.0.0.1 --port 5000", current.display());

        assert!(is_our_server_cmdline(&format!("{} --host 127.0.0.1 --port 5000", current.display()), root), "当前版本");
        assert!(is_our_server_cmdline(&format!("{} --host 127.0.0.1 --port 5000", old.display()), root), "升级后，旧版本号对应的目录也认得出");
        assert!(is_our_server_cmdline(&sandboxed, root), "套了 sandbox-exec 也认得出");
        assert!(!is_our_server_cmdline("/opt/homebrew/bin/speech-server --port 8080", root), "brew 的那个不是我们拉的");
        assert!(!is_our_server_cmdline("/usr/bin/vim notes.txt", root));
        assert!(!is_our_server_cmdline("", root));
        assert!(
            !is_our_server_cmdline(&format!("{}/speech-server-evil --port 1", root.display()), root),
            "文件名前缀相同的别的程序不算（整段文件名必须相等）"
        );
        assert!(
            !is_our_server_cmdline(&format!("{}/other/speech-server --port 1", root.parent().unwrap().display()), root),
            "根目录不对的（比如别的运行时目录）不算"
        );
        // PR #106 评审第 3 轮非阻塞项：`runtime-evil` 与 `runtime` 字符串上
        // 共享前缀，但是**两个不同的目录**——按路径分量比较就不会把它认错；
        // 旧的 `starts_with` 字符串比较会在这条上误判成「我们的」。
        let sibling = root.with_file_name("runtime-evil").join("speech-server");
        assert!(
            !is_our_server_cmdline(&format!("{} --port 1", sibling.display()), root),
            "runtime-evil 跟 runtime 字符串前缀碰巧对上，但不是同一个目录，不该认成我们的"
        );
    }

    #[test]
    fn pid_file_parses_new_and_legacy_formats() {
        assert_eq!(parse_pid_file("123 456\n"), Some((Some(123), 456)));
        assert_eq!(parse_pid_file("456"), Some((None, 456)), "v0.23.0 的旧格式：拥有者未知");
        assert_eq!(parse_pid_file(""), None);
        assert_eq!(parse_pid_file("abc 1"), None);
        assert_eq!(parse_pid_file("1 abc"), None);
    }

    /// N1 的判据：拥有者还活着、且不是自己 → 绝不碰。
    #[test]
    fn reap_verdict_truth_table() {
        use ReapVerdict::*;
        let me = 100;
        assert_eq!(reap_verdict(Some(200), 900, me, None, true), Keep, "另一个活着的 AgentEar 的服务不是孤儿");
        assert_eq!(reap_verdict(Some(200), 900, me, None, false), KillAndRemove, "拥有者死了 → 孤儿");
        assert_eq!(reap_verdict(Some(me), 900, me, Some(900), true), Keep, "自己正在用的");
        assert_eq!(reap_verdict(Some(me), 900, me, Some(901), true), KillAndRemove, "自己的旧服务（已换新的）");
        assert_eq!(reap_verdict(Some(me), 900, me, None, true), KillAndRemove, "自己名下但当前没有服务：上一个同 pid 进程留下的");
        assert_eq!(reap_verdict(None, 900, me, None, false), KillAndRemove, "旧格式按老规矩收（仍要过命令行核对）");
        assert_eq!(reap_verdict(Some(200), 0, me, None, true), RemoveOnly);
        assert_eq!(reap_verdict(Some(200), 900, me, Some(900), true), Keep);
    }

    /// 起一个「命令行里带某个路径」的真进程：`/bin/sh <path>`，脚本里循环 sleep。
    fn spawn_script(path: &Path) -> std::process::Child {
        fs::write(path, "while :; do sleep 1; done\n").unwrap();
        Command::new("/bin/sh")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn is_running(child: &mut std::process::Child) -> bool {
        // 给 SIGTERM 一点时间生效
        for _ in 0..20 {
            if !matches!(child.try_wait(), Ok(None)) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    /// PR #93 评审 N1 的真实场景：命令行视角（自己没有常驻服务）去 reap，
    /// **不能杀掉另一个还活着的 AgentEar（守护进程）拥有的常驻服务**；
    /// 拥有者死了以后才收。用真进程 + 真 `ps` 走 `reap_stale_in` 这条调用路径。
    #[test]
    fn reap_does_not_kill_a_live_owners_server_but_reaps_orphans() {
        let dir = std::env::temp_dir().join(format!("agentear-reap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("speech-server");
        let mut server = spawn_script(&bin);
        let mut owner = spawn_script(&dir.join("agentear-owner"));
        let (sp, op) = (server.id() as i32, owner.id() as i32);
        // 断言失败（panic）时也要收掉这些脚本进程，否则会一直 sleep 下去
        struct Reap(Vec<i32>, PathBuf);
        impl Drop for Reap {
            fn drop(&mut self) {
                for p in &self.0 {
                    unsafe { libc::kill(*p, libc::SIGKILL) };
                }
                let _ = fs::remove_dir_all(&self.1);
            }
        }
        let mut guard = Reap(vec![sp, op], dir.clone());
        let pf = pid_file_for(&dir, op);
        fs::write(&pf, format!("{op} {sp}")).unwrap();
        let me = std::process::id() as i32;

        // ① 拥有者活着：命令行视角 reap（current=None）不能动它
        reap_stale_in(&dir, &dir, me, None);
        assert!(is_running(&mut server), "守护进程的常驻服务被命令行杀掉了（N1）");
        assert!(pf.exists(), "别人的 pid 文件不能删");

        // ② 拥有者死了：这才是孤儿，收掉并删文件
        let _ = owner.kill();
        let _ = owner.wait();
        reap_stale_in(&dir, &dir, me, None);
        assert!(!is_running(&mut server), "拥有者死了之后孤儿要被收掉");
        assert!(!pf.exists());

        // ③ 旧格式文件 + 命令行不是我们的 speech-server：不杀（pid 复用保护仍在）
        let mut other = spawn_script(&dir.join("unrelated"));
        guard.0.push(other.id() as i32);
        let legacy = dir.join(LEGACY_PID_FILE);
        fs::write(&legacy, other.id().to_string()).unwrap();
        reap_stale_in(&dir, &dir, me, None);
        assert!(is_running(&mut other), "命令行对不上的进程不能杀");
        assert!(!legacy.exists());
        let _ = other.kill();
        let _ = other.wait();
        let _ = server.kill();
        let _ = server.wait();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reaper_truth_table() {
        let s = Duration::from_secs;
        assert_eq!(reap_reason(true, true, s(10), s(600)), None, "在用、没超时：留着");
        assert!(reap_reason(true, true, s(600), s(600)).is_some(), "到点就回收");
        assert!(reap_reason(false, true, s(1), s(600)).is_some(), "关了常驻立刻回收");
        assert!(reap_reason(true, false, s(1), s(600)).is_some(), "换回 SenseVoice 立刻回收");
    }

    /// PR #106 评审阻塞项的真值表。
    #[test]
    fn recovery_decision_truth_table() {
        use RecoveryAction::*;
        // 没有空档：不管别的条件是什么，都不是恢复场景
        assert_eq!(recovery_decision(false, true, true, true), None);
        assert_eq!(recovery_decision(false, false, false, false), None);

        // 有空档，但既没有标记也没有「配置里选着 speech_swift」这条线索——
        // 不是恢复场景（比如用户从没装过，或者早就手动切走了）
        assert_eq!(recovery_decision(true, false, false, true), None);
        assert_eq!(recovery_decision(true, false, false, false), None);

        // 首次发现（还没有标记）：靠配置判断
        assert_eq!(recovery_decision(true, false, true, true), Download, "守护进程首次发现，该发起下载");
        assert_eq!(recovery_decision(true, false, true, false), Note, "一次性 CLI 首次发现，只记标记");

        // 已经有标记（哪怕配置这时已经被退回成 builtin 了）：仍然要恢复——
        // 这正是评审要修的那条 bug：不能只看 asr_backend。
        assert_eq!(
            recovery_decision(true, true, false, true),
            Download,
            "标记在、配置已被退回成 builtin：守护进程仍要重试"
        );
        assert_eq!(recovery_decision(true, true, false, false), Note, "同上，但这次是一次性 CLI");
        // 标记在、配置碰巧还是 speech_swift：同样要恢复（两条线索都指向恢复）
        assert_eq!(recovery_decision(true, true, true, true), Download);
    }

    /// ①「boot 1 中断 → boot 2 仍能恢复」：模拟两次启动。
    #[test]
    fn boot_1_interrupted_boot_2_still_recovers() {
        // boot 1：守护进程，配置还是 speech_swift，还没有标记——决定发起下载。
        // 调用方据此会写标记、把配置退回 builtin，然后下载被打断（进程被杀 /
        // 网络失败，这个决策函数看不出两者的区别，也不需要看出）。
        let boot1 = recovery_decision(true, false, true, true);
        assert_eq!(boot1, RecoveryAction::Download);

        // boot 2：配置已经是 builtin 了（boot 1 退回时持久化的那一步），
        // 但标记还在——仍要重试。如果这里看的是 asr_backend 而不是标记，
        // 这一步会错判成 None，用户就永久卡在 SenseVoice 上了。
        let boot2 = recovery_decision(true, true, false, true);
        assert_eq!(boot2, RecoveryAction::Download, "标记保住了恢复的线索，boot 2 仍要重试");
    }

    /// ② 一次性 CLI 不发起下载：`Note` 这个返回值本身就是契约——
    /// `main.rs` 只在 `Download` 分支里调 `qwen3::start` / `config::update`，
    /// `Note` 分支只写标记、在内存里退回 builtin，不碰磁盘上的 `asr_backend`。
    #[test]
    fn one_shot_cli_only_notes_does_not_download() {
        assert_eq!(recovery_decision(true, false, true, false), RecoveryAction::Note);
        assert_eq!(recovery_decision(true, true, false, false), RecoveryAction::Note);
    }

    /// ③ 下载失败后下次启动仍会重试：失败和「被杀」对这个决策函数是同一件事——
    /// 两者都不会调 `clear_recovering_marker()`（只有真正装好那一刻才调），
    /// 所以下一次启动看到的都是「标记在、配置可能已经是 builtin」，
    /// 与 `boot_1_interrupted_boot_2_still_recovers` 是同一条断言。
    #[test]
    fn failed_download_is_retried_next_boot() {
        assert_eq!(recovery_decision(true, true, false, true), RecoveryAction::Download);
    }

    #[test]
    fn recycle_when_the_server_outgrows_the_budget() {
        let gib = 1u64 << 30;
        assert_eq!(recycle_reason(None, 4 * gib), None, "读不到就不动");
        assert_eq!(recycle_reason(Some(2 * gib), 4 * gib), None, "热态约 2 GiB：留着");
        assert_eq!(recycle_reason(Some(4 * gib), 4 * gib), None, "正好在线上：留着");
        // jason 机器上实测的 30 GB
        let why = recycle_reason(Some(30 * gib), 4 * gib).expect("超了要重启");
        assert!(why.contains("30.0 GiB"), "{why}");
        assert_eq!(SERVER_FOOTPRINT_BUDGET, 4 * gib, "= ASR 高资源档预算");
    }

    #[test]
    fn footprint_reads_this_process() {
        let me = std::process::id() as i32;
        assert!(footprint_bytes(me).is_some_and(|b| b > 0));
        assert_eq!(footprint_bytes(0), None);
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
