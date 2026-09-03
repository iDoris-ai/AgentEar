//! 泰语模型的按需下载。
//!
//! ## 为什么模型不随包走
//!
//! 主包 241 MB 里的 SenseVoice 服务的是中/英/日/韩/粤，**所有人都用得上**。
//! 泰语模型 574 MB，只有一部分人用。把它塞进主包等于让每个下载 AgentEar 的
//! 人多背一倍多的体积去换一个自己可能永远不开的功能。
//! （jason 2026-08-22 拍板，见 `docs/plan-i18n-thai.md` §4。）
//!
//! ## 为什么不能下到 `vendor/`
//!
//! `vendor/` 在打包后解析到 `AgentEar.app/Contents/Resources/vendor`，
//! 往那里写会同时踩四个坑：**破坏 app 的代码签名封印**（TCC 把辅助功能
//! 授权钉在 cdhash 上，签名一破授权就没了）；升级时整个 .app 被替换，
//! 下载的模型直接消失；app 装在 `/Applications` 时普通用户没有写权限；
//! 以及多用户共用一份 app 时互相踩踏。
//!
//! 所以下到 `~/.agentear/models/`——可写、跨升级保留、随数据目录一起备份。
//!
//! ## 下载协议
//!
//! `.part → 校验 → rename` 这三步只保证「最终文件不出现半截内容」，
//! 不足以应付真实世界。这里额外处理的情形：
//!
//! - **下到一半退出**：curl `-C -` 断点续传，`.part` 保留
//! - **重复点击 / 两个进程同时下**：`.lock` 上的 `flock`，跨进程互斥
//! - **目标路径是目录或符号链接**：`symlink_metadata` 显式拒绝，
//!   不能让一个指向别处的链接被当成「模型已就绪」
//! - **磁盘不够**：下载前就查，别下到 90% 才失败
//! - **传输完整但内容不对**（CDN 返回了错误页、镜像给了旧版本）：
//!   全量 SHA-256 校验，不匹配就删掉重来
//!
//! - **`.part` 本身是符号链接**：curl 会顺着链接写，把模型字节灌进
//!   用户的别的文件。落地前 `symlink_metadata` 拒绝
//! - **`.part` 比目标还大**（上次下的是别的东西、或服务端换了文件）：
//!   `-C -` 会请求一个超出远端 EOF 的偏移，服务器拒绝，curl 失败，
//!   **每次重试都在同一个毒文件上重复**。落地前按体积判断，超了就删重下
//! - **服务端流出比声明更多的字节**：轮询时超过 `spec.bytes` 就杀掉 curl，
//!   不等它把磁盘写满
//! - **冒烟失败**：安装校验（模型能不能被 whisper 加载）在 **rename 之前**
//!   跑，失败就不产生最终文件——不能出现「文件在那儿、但根本加载不了，
//!   而 `state()` 说就绪」
//! - **进程重启后拿什么判断「装好了」**：一份 `.installed` 清单，
//!   内容是这次校验通过的 sha256。**光有同名文件不算装好**——
//!   截断的、手动替换的、旧版本的文件都会有同一个名字
//!
//! ⚠️ **不处理**的：清单写好之后文件被外部改动（没有持续校验，
//! 每次启动重算 574 MB 的 SHA 代价太大）。那种情况会在 whisper
//! 加载模型时报错。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

/// 一个可下载的模型。
pub struct ModelSpec {
    /// 落盘文件名，同时也是 URL 的最后一段。
    pub file: &'static str,
    pub url: &'static str,
    /// 完整 sha256（小写十六进制）。**必须是全量 64 位**——
    /// 这是唯一能挡住「CDN 返回错误页」和「镜像给了旧版本」的东西。
    pub sha256: &'static str,
    /// 期望体积，用于算进度和预检磁盘空间。
    pub bytes: u64,
}

/// ADR-0004 §4 临时选定：`biodatlab/distill-whisper-th-large-v3` q5_0。
///
/// **这是可撤销的默认值**，不是终局：选它的依据是 FLEURS 泰语朗读语料上的
/// CER，而那套语料对 Thonburian 系（本模型的出处）有利，且**完全没有
/// code-switch（泰语夹英文技术词）**——恰恰是 jason 实际要用的场景。
/// 换模型的成本就是改这四个常量，见 `scripts/build-thai-model.sh`。
pub const THAI: ModelSpec = ModelSpec {
    file: "ggml-distill-whisper-th-large-v3-q5_0.bin",
    url: "https://github.com/iDoris-ai/AgentEar/releases/download/models-th-v1/ggml-distill-whisper-th-large-v3-q5_0.bin",
    sha256: "5bfc04f1931a1bb9af9f6c7942b4a63b8d8f956377fe6b9827c6b286420a9c6d",
    bytes: 574041195,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    /// 下不动：断网、URL 404、CDN 出错。
    Network,
    /// 下完了但内容不对。
    Checksum,
    /// 磁盘空间不足。
    Disk,
    /// 另一个进程正在下同一个文件。
    Busy,
    /// 本地文件系统层面的问题（目标是目录、没有写权限……）。
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 没下过，也没在下。
    Absent,
    /// 正在下，附百分比（0–100）。
    Downloading(u8),
    /// 字节已落盘、校验已过，正在做加载冒烟。**还不能用。**
    Verifying,
    /// 文件在，且**上一次校验通过**。
    Ready,
    Failed(Fail),
}

/// 进度与失败原因。
///
/// 用两个原子量而不是 `Mutex<State>`：读方是主线程的菜单构建和 0.5s 定时器，
/// 写方是下载线程，读远多于写，且 `State` 全是 Copy 的小值。
///
/// `PHASE`：0=闲 1=下载中 2=失败 3=验证中。**没有「就绪」这一档**——
/// 就绪与否由文件是否存在决定（见 `state()`），程序重启后内存状态没了，
/// 文件还在。
///
/// 「验证中」这一档是**必需的，不是锦上添花**：字节落盘之后还要跑一次
/// 加载冒烟才算装好（`docs/plan-i18n-thai.md` §4）。少了它，文件一落地
/// `state()` 就报「就绪」，用户在那一两秒里点泰语会绕过冒烟直接提交配置，
/// 而那正是这套状态机要防的事。
static PHASE: AtomicU8 = AtomicU8::new(0);
static PCT: AtomicU8 = AtomicU8::new(0);
static FAIL: AtomicU8 = AtomicU8::new(0);

/// 数据目录。由 `main` 在启动时设进来。
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn set_data_root(p: PathBuf) {
    DATA_ROOT.set(p).ok();
}

/// 当前引擎的身份指纹。由 `main` 在启动时设进来（`asr::engine_fingerprint`）。
///
/// **为什么安装记录必须绑定引擎**：模型跨版本升级是保留的（在数据目录里），
/// 引擎却是随 .app 走的。升级换掉 `whisper-cli` 之后，上一次的加载冒烟
/// 就不再能说明任何问题了——新引擎可能根本读不了这个量化格式。
/// 不绑的话，用户升级完一录音就失败，而自检还说「已安装」。
static ENGINE_ID: OnceLock<String> = OnceLock::new();

pub fn set_engine_id(id: String) {
    ENGINE_ID.set(id).ok();
}

fn engine_id() -> Option<&'static str> {
    ENGINE_ID.get().map(String::as_str)
}

/// 下载的模型放哪。
///
/// **和 `vendor/` 分开**（见模块文档）：`vendor/` 是随包分发、只读、
/// 升级即替换；这里是用户数据、可写、跨升级保留。
pub fn models_root() -> Option<PathBuf> {
    DATA_ROOT.get().map(|r| r.join("models"))
}

pub fn path_of(spec: &ModelSpec) -> Option<PathBuf> {
    models_root().map(|r| r.join(spec.file))
}

/// 安装记录：这一份模型，被哪个引擎验过。
#[derive(Serialize, Deserialize)]
struct Manifest {
    /// 校验通过的模型 sha256。换模型（改 `ModelSpec`）就对不上。
    model_sha256: String,
    /// 跑冒烟的那个引擎的指纹。app 升级换了引擎就对不上。
    engine_id: String,
}

fn manifest_of(spec: &ModelSpec) -> Option<PathBuf> {
    models_root().map(|r| r.join(format!("{}.installed", spec.file)))
}

/// 真的装好了吗。**这是 `Ready` 的唯一判据。**
///
/// 四条都要成立：模型文件在且是普通文件、体积对得上、安装记录在、
/// 记录里的模型 sha 和**引擎指纹**都是当前这一套。
///
/// ## 这份记录能证明什么、不能证明什么
///
/// 它是**优化，不是完整性边界**。真正的保证来自「记录只在 SHA 全量校验 +
/// 加载冒烟都通过之后才写」；这里省掉的只是每次启动重算 574 MB 的开销
/// （那要几百毫秒，菜单每次展开都算会明显卡顿）。
///
/// 所以它挡得住的是**意外**：下到一半被 kill、手动拷进来一个同名文件、
/// 升级后引擎换了、旧版本残留。**挡不住蓄意伪造**——能写
/// `~/.agentear/models/` 的进程完全可以造一个同体积的文件再把公开可知的
/// sha 写进记录里。但那样的进程同样能改 app 自己的二进制，
/// 所以这条不在威胁模型内，加密签名也解决不了同用户权限下的问题。
pub fn is_installed(spec: &ModelSpec) -> bool {
    let (Some(m), Some(f)) = (manifest_of(spec), path_of(spec)) else {
        return false;
    };
    if !is_present(&f) || !fs::symlink_metadata(&f).is_ok_and(|x| x.len() == spec.bytes) {
        return false;
    }
    // 记录本身也可能被符号链接冒充，一样要 symlink_metadata 把关
    if !fs::symlink_metadata(&m).is_ok_and(|x| x.is_file()) {
        return false;
    }
    let Ok(text) = fs::read_to_string(&m) else {
        return false;
    };
    let Ok(rec) = serde_json::from_str::<Manifest>(&text) else {
        return false;
    };
    // 引擎指纹还没设进来时**一律判未安装**：宁可让用户多点一次，
    // 也不能在不知道引擎是谁的情况下声称装好了。
    rec.model_sha256 == spec.sha256 && Some(rec.engine_id.as_str()) == engine_id()
}

/// 没装好的**具体原因**，给 `--diagnose` 用。
///
/// 不合并进 `is_installed`（那个只回答是非题，菜单每次展开都要调）。
/// 分开是为了自检能说人话：「引擎换了」和「文件是手动放的」处置完全不同，
/// 前者重新校验几秒就好，后者可能要重下。
pub fn install_issue(spec: &ModelSpec) -> Option<String> {
    if is_installed(spec) {
        return None;
    }
    let (Some(m), Some(f)) = (manifest_of(spec), path_of(spec)) else {
        return Some("数据目录未初始化".into());
    };
    if !f.exists() {
        return Some("未下载——菜单里选「识别语言 → ไทย」，或跑 --fetch-thai".into());
    }
    if !is_present(&f) {
        return Some("⚠️ 不是普通文件（符号链接或目录）".into());
    }
    let size = fs::symlink_metadata(&f).map(|x| x.len()).unwrap_or(0);
    if size != spec.bytes {
        return Some(format!(
            "⚠️ 体积不符（{} MiB，期望 {} MiB）——重跑 --fetch-thai",
            size / 1048576,
            spec.bytes / 1048576
        ));
    }
    if engine_id().is_none() {
        return Some("⚠️ 泰语引擎不在 vendor 里，模型没法用".into());
    }
    match fs::read_to_string(&m).ok().and_then(|t| serde_json::from_str::<Manifest>(&t).ok()) {
        None => Some("⚠️ 没有安装记录（手动放的？装到一半？）——跑 --fetch-thai 校验一次".into()),
        Some(rec) if rec.model_sha256 != spec.sha256 => {
            Some("⚠️ 安装记录指向另一个模型版本——跑 --fetch-thai".into())
        }
        // 最常见的一种：app 升级换了引擎。**不用重下**，重新校验几秒就好。
        Some(_) => Some("⚠️ 引擎已更换，需重新校验——跑 --fetch-thai（不会重下）".into()),
    }
}

fn write_manifest(spec: &ModelSpec) -> Result<()> {
    let m = manifest_of(spec).context("数据目录未初始化")?;
    let id = engine_id()
        .context("引擎指纹未初始化，拒绝写安装记录")?
        .to_string();
    let rec = Manifest {
        model_sha256: spec.sha256.to_string(),
        engine_id: id,
    };
    let json = serde_json::to_string(&rec)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("序列化清单失败: {e}")))?;

    // 临时文件名带 pid，且**写之前把可能存在的符号链接清掉**——
    // `fs::write` 会顺着链接写，这是和 `.part` 同一类的问题
    // （第一轮修了 .part 却漏了这里）。
    let tmp = m.with_file_name(format!(
        "{}.tmp.{}",
        m.file_name().and_then(|x| x.to_str()).unwrap_or("installed"),
        std::process::id()
    ));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_file(&tmp).ok();
    }
    fs::write(&tmp, json)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("写清单失败: {e}")))?;
    fs::rename(&tmp, &m)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("落地清单失败: {e}")))?;
    Ok(())
}

fn fail_code(f: Fail) -> u8 {
    match f {
        Fail::Network => 1,
        Fail::Checksum => 2,
        Fail::Disk => 3,
        Fail::Busy => 4,
        Fail::Io => 5,
    }
}

fn code_fail(c: u8) -> Fail {
    match c {
        1 => Fail::Network,
        2 => Fail::Checksum,
        3 => Fail::Disk,
        4 => Fail::Busy,
        _ => Fail::Io,
    }
}

/// 当前状态。菜单每次展开都会调用。
///
/// ## 为什么不每次重算 SHA
///
/// 574 MB 的 SHA-256 要几百毫秒，菜单每次展开都算会明显卡顿。
/// 完整校验发生在**安装那一刻**（`run` 里），之后由 `.installed` 清单
/// 加体积检查代替（见 `is_installed`）。
///
/// 代价是：清单写好之后文件被外部损坏，这里看不出来——那种情况会在
/// whisper 加载模型时报错。这是有意的取舍，模块文档的「不处理」一节记着它。
pub fn state(spec: &ModelSpec) -> State {
    // Acquire 配 `start`/下载线程里的 Release：先写 PCT/FAIL 再发布 PHASE，
    // 这里先读 PHASE 再读它们，保证读到的载荷和相位是配套的。
    // 用 Relaxed 的话，可以读到「已失败」却配着上一次的失败码，
    // 或者「下载中」配着上一轮残留的百分比。
    match PHASE.load(Ordering::Acquire) {
        1 => return State::Downloading(PCT.load(Ordering::Relaxed)),
        2 => return State::Failed(code_fail(FAIL.load(Ordering::Relaxed))),
        3 => return State::Verifying,
        _ => {}
    }
    if is_installed(spec) {
        State::Ready
    } else {
        State::Absent
    }
}

/// 文件在不在，且**确实是个普通文件**。
///
/// 用 `symlink_metadata` 而不是 `exists()`：符号链接会让 `exists()`
/// 返回 true，而链接指向的东西完全不受我们控制。目录同理——
/// `~/.agentear/models/xxx.bin/` 这么个目录会让「已就绪」判成真，
/// 然后 whisper 在加载时报一个没人看得懂的错。
fn is_present(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// 启动下载。已在下载中就什么也不做（重复点击是常态，不是错误）。
///
/// 两个回调，**时机完全不同，不要合并**：
///
/// - `on_verify(&part)`：字节下完、SHA 过了，但**还没 rename**。
///   在这里做加载冒烟。返回 `Err` 就不产生最终文件——这正是要害：
///   一个「文件在那儿但 whisper 加载不了」的安装比没装还糟，
///   因为 `state()` 会说就绪，而每次录音都失败。
/// - `on_installed()`：清单已落地，`state()` 从此刻起报 `Ready`。
///   要不要把识别语言切过去在这里决定（用户可能在这几分钟里改了主意，
///   见 `tray::on_thai_installed`）。
///
/// 用回调而不是让本模块直接去调 asr/config，是为了让下载器对「下的是
/// 什么」保持无知：它只管把字节安全地搬到磁盘上。
pub fn start(
    spec: &'static ModelSpec,
    on_verify: fn(&Path) -> Result<()>,
    on_installed: fn(),
) {
    // 0 → 1 的 CAS：只有把 PHASE 从「闲」抢到「下载中」的那个调用者
    // 才真的起线程。**失败状态也允许重来**（2 → 1），用户点重试就该重试。
    // 先看一眼当前相位，别用 swap 抢——抢了再还原会让并发的读方
    // 看到一瞬间的「下载中 0%」，而实际什么都没开始。
    let was = PHASE.load(Ordering::Acquire);
    if was == 1 || was == 3 {
        log::info!("模型已在下载/验证中，忽略重复请求");
        return;
    }
    // 载荷先写，相位后发布（Release）。反过来的话，读方可能先看到
    // 「下载中」再读到上一轮残留的百分比。
    PCT.store(0, Ordering::Relaxed);
    PHASE.store(1, Ordering::Release);

    std::thread::spawn(move || {
        // 包一层 catch_unwind：线程 panic 的话相位会**永远停在「下载中」**，
        // 菜单一直显示进度、`--fetch-thai` 的等待循环一直转，
        // 而实际上什么都不会再发生了。宁可报成失败，用户还能重试。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(spec, on_verify)
        }));
        let outcome = match outcome {
            Ok(r) => r,
            Err(_) => {
                log::error!("下载线程 panic");
                Err(anyhow::Error::new(Fail::Io).context("下载线程 panic"))
            }
        };
        match outcome {
            Ok(()) => {
                log::info!("模型安装完成：{}", spec.file);
                PHASE.store(0, Ordering::Release); // 就绪由 .installed 清单表达
                on_installed();
            }
            Err(e) => {
                log::error!("模型安装失败：{e:#}");
                // 原因分类挂在 anyhow 的 downcast 上——run 里用 Fail 作为
                // 上下文塞进去，这里取出来给菜单显示对应的文案。
                let f = e.downcast_ref::<Fail>().copied().unwrap_or(Fail::Io);
                FAIL.store(fail_code(f), Ordering::Relaxed);
                PHASE.store(2, Ordering::Release);
            }
        }
    });
}

fn run(spec: &ModelSpec, on_verify: fn(&Path) -> Result<()>) -> Result<()> {
    let dir = models_root().context("数据目录未初始化")?;
    fs::create_dir_all(&dir).with_context(|| format!("建 {} 失败", dir.display()))?;
    let dest = dir.join(spec.file);
    let part = dir.join(format!("{}.part", spec.file));
    let lock = dir.join(format!("{}.lock", spec.file));

    // 目标路径已被目录或符号链接占着 —— 必须在下载之前就发现。
    // 下完 574 MB 再发现 rename 失败，用户白等一场。
    if let Ok(m) = fs::symlink_metadata(&dest) {
        if !m.is_file() {
            return Err(anyhow::Error::new(Fail::Io)
                .context(format!("{} 不是普通文件（是目录或符号链接）", dest.display())));
        }
    }

    // 跨进程互斥。用户完全可能同时开着终端里跑的和 .app 里跑的两份。
    let _guard = FileLock::acquire(&lock)?;

    // 锁拿到之后再查一遍：可能刚才另一个进程已经装完了。
    if is_installed(spec) {
        log::info!("模型已安装，跳过");
        return Ok(());
    }

    // 模型文件还在、体积也对，只是安装记录不认了——最常见的原因是
    // **app 升级换了引擎**（记录绑定引擎指纹，见 `is_installed`）。
    // 这种情况**不该重下 574 MB**：字节大概率还是好的，
    // 重新校验 + 用新引擎跑一次冒烟就够了。
    if fs::symlink_metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == spec.bytes) {
        log::info!("模型文件在但安装记录不匹配（多半是引擎变了），重新校验而不是重下");
        PHASE.store(3, Ordering::Release);
        match verify(&dest, spec).and_then(|()| on_verify(&dest)) {
            Ok(()) => {
                write_manifest(spec)?;
                sync_dir(&dir);
                return Ok(());
            }
            Err(e) => {
                // 重验不过就说明文件真坏了（verify 失败时已经把它删了）
                // 或者新引擎读不了它。往下走正常下载流程。
                log::warn!("重新校验失败，改为重新下载: {e:#}");
            }
        }
    }

    // —— `.part` 的卫生检查。**这一段每一条都是真会出事的** ——
    //
    // curl 用 `-o` 写这个路径，它顺着符号链接写。`.part` 要是个指向别处的
    // 链接，574 MB 的模型字节就灌进用户的别的文件里去了。
    // 注意必须用 symlink_metadata：`fs::metadata` 会跟随链接，正好看不见问题。
    if let Ok(m) = fs::symlink_metadata(&part) {
        if !m.is_file() {
            log::warn!("{} 不是普通文件（符号链接或目录），删掉重来", part.display());
            fs::remove_file(&part)
                .or_else(|_| fs::remove_dir_all(&part))
                .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("清理 .part 失败: {e}")))?;
        }
    }

    let have = fs::symlink_metadata(&part).map(|m| m.len()).unwrap_or(0);
    // 比目标还大的残留是**毒文件**：`-C -` 会请求一个超出远端 EOF 的偏移，
    // 服务器拒绝，curl 失败，而失败又保留 .part —— 每次重试都在同一个坑里
    // 重复，用户永远好不了。直接删掉重下。
    if have > spec.bytes {
        log::warn!("残留的 .part 比目标还大（{have} > {}），删掉重下", spec.bytes);
        fs::remove_file(&part).ok();
    }
    let have = fs::symlink_metadata(&part).map(|m| m.len()).unwrap_or(0);

    if have == spec.bytes {
        // 上次字节下全了，但校验/落地没走完（断电、被 kill）。
        // **不要再联网**——`-C -` 在文件已完整时会请求越界偏移而失败。
        // 直接进校验，对了就用，不对就删掉重来。
        log::info!("发现完整的 .part，跳过下载直接校验");
        PCT.store(100, Ordering::Relaxed);
    } else {
        ensure_space(&dir, spec.bytes - have)?;
        log::info!(
            "开始下载 {}（{:.0} MB{}）",
            spec.file,
            spec.bytes as f64 / 1e6,
            if have > 0 {
                format!("，已有 {:.0} MB，续传", have as f64 / 1e6)
            } else {
                String::new()
            }
        );
        fetch(spec, &part)?;
    }

    // 相位切到「验证中」。SHA 要几百毫秒，Metal 首次冒烟要几秒——
    // 这段时间界面显示「下载中 100%」会让人以为卡住了。
    // （第一轮加了 Verifying 这一档，第二轮才发现把冒烟移进 run() 之后
    // 忘了在这里发布它，那个分支一直是死代码。）
    PHASE.store(3, Ordering::Release);

    verify(&part, spec)?;

    // **加载冒烟在 rename 之前。** 这是整个流程的要害：
    // 一个「文件在那儿、但 whisper 根本加载不了」的安装比没装还糟——
    // `state()` 会说就绪，而用户每次录音都失败，错误只在日志里。
    // 冒烟不过就不产生最终文件。
    on_verify(&part).map_err(|e| {
        // 保留 .part：字节是对的（SHA 过了），坏的是「这个引擎能不能用它」。
        // 删掉的话，换个引擎版本重试又要重下 574 MB。
        anyhow::Error::new(Fail::Io).context(format!("模型加载冒烟失败: {e:#}"))
    })?;

    // 落盘顺序：**先把数据刷到盘上，再 rename，再刷目录项。**
    // 少了这几步，掉电后可能出现「清单说装好了、文件却是半截或干脆不见」。
    // store.rs 的提交协议是同一套讲究，这里保持一致。
    fs::File::open(&part)
        .and_then(|f| f.sync_all())
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("sync .part 失败: {e}")))?;
    fs::rename(&part, &dest)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("rename 失败: {e}")))?;
    sync_dir(&dir);

    // 清单最后写。它是「装好了」的唯一凭据，必须在模型文件已经稳稳落盘
    // 之后才出现——反过来就会有「清单在、模型不完整」的窗口。
    write_manifest(spec)?;
    sync_dir(&dir);
    Ok(())
}

/// 用系统的 curl 把字节搬下来。
///
/// 不引 HTTP 客户端依赖：一个 reqwest 会带进上百个传递依赖和一整套 TLS 栈，
/// 只为下一个文件。curl 是 macOS 自带的，还免费附送断点续传和重定向处理。
fn fetch(spec: &ModelSpec, part: &Path) -> Result<()> {
    let mut child = Command::new("/usr/bin/curl")
        .arg("-fL")           // 4xx/5xx 返回非零；跟随重定向（GitHub Release 一律 302 到 CDN）
        .arg("-C").arg("-")   // 断点续传
        .arg("--retry").arg("3")
        .arg("--retry-delay").arg("2")
        // ⚠️ **进度条必须关掉。** curl 默认往 stderr 持续写进度，而我们把
        // stderr 接成了管道。管道有容量上限，写满之后 curl 会阻塞在写上、
        // 永远不退出，而我们的轮询循环只调 try_wait()——于是双方一起卡死，
        // 还攥着 flock 不放。
        //
        // 用 `-sS` 而不是 `--no-progress-meter`：后者是 curl **7.67** 才有的，
        // 而我们在 Info.plist 里声明支持到 macOS 11，那上面自带的是 curl 7.64。
        // 传一个不认识的选项，curl 会直接以「unknown option」退出——
        // 结果就是在最老的受支持系统上，下载功能一次都成功不了。
        // `-s` 关掉进度条，`-S` 保证真出错时错误信息仍然写出来。
        .arg("-sS")
        .arg("-o").arg(part)
        .arg(spec.url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::Error::new(Fail::Network).context(format!("启动 curl 失败: {e}")))?;

    // 即便关了进度条，stderr 也必须有人读——错误信息本身也可能写满管道，
    // 而且我们要拿它做诊断（原来的实现把它丢了，日志里只剩一个退出码）。
    let mut err_pipe = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        // **必须读到 EOF**，不能读满 64 KiB 就撒手——那样管道读端提前关闭，
        // curl 还在写就会被打回来，等于把刚修掉的阻塞风险又请回来了。
        // 内存上只留最后 64 KiB（滚动丢弃前面的），诊断信息里有用的
        // 本来就在末尾。
        const KEEP: usize = 64 * 1024;
        let mut tail = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            use std::io::Read;
            let mut chunk = [0u8; 8192];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > KEEP {
                            tail.drain(..tail.len() - KEEP);
                        }
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    // 进度靠轮询 .part 的体积。curl 自己的进度条要解析终端控制字符，
    // 还得处理续传时的偏移，不如直接看文件长到哪了。
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if let Ok(m) = fs::metadata(part) {
                    // 硬上限：坏端点或被劫持的重定向可能一直往外吐数据，
                    // 而体积检查在 curl 结束后才做——那时磁盘可能已经满了。
                    // 留 1 MB 容差，超了就掐掉。
                    if m.len() > spec.bytes.saturating_add(1 << 20) {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = fs::remove_file(part);
                        return Err(anyhow::Error::new(Fail::Network).context(format!(
                            "服务端吐出的数据超过声明体积（{} > {}），已中止",
                            m.len(),
                            spec.bytes
                        )));
                    }
                    if spec.bytes > 0 {
                        let pct = (m.len().saturating_mul(100) / spec.bytes).min(99) as u8;
                        PCT.store(pct, Ordering::Relaxed);
                    }
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            Err(e) => {
                return Err(anyhow::Error::new(Fail::Network)
                    .context(format!("等待 curl 失败: {e}")))
            }
        }
    };

    let stderr = err_thread.join().unwrap_or_default();

    if !status.success() {
        let code = status.code();
        // 断点续传本身谈崩了的两种退出码：
        //   33 = 服务端不支持 byte range
        //   36 = 续传位置不对（bad download resume）
        // 这两种下**留着 .part 就是永久卡住**：每次重试都从同一个偏移
        // 续起，服务端每次都拒绝。清掉它，下一次从零开始还有救。
        // 其余失败（断网、超时）保留 .part，续传是有价值的。
        let range_broken = matches!(code, Some(33) | Some(36))
            || stderr.contains("Range")
            || stderr.contains("resume");
        if range_broken {
            log::warn!("续传被拒（exit {code:?}），删掉 .part 以便下次从零开始");
            let _ = fs::remove_file(part);
        }
        return Err(anyhow::Error::new(Fail::Network)
            .context(format!("curl 退出码 {code:?}: {}", stderr.trim())));
    }

    PCT.store(100, Ordering::Relaxed);
    Ok(())
}

/// fsync 一个目录，让其中的 rename 真正落盘。
///
/// 失败只记日志：这是耐久性的加固，不是功能的前提。为它中断一次
/// 已经成功的安装不划算。
fn sync_dir(dir: &Path) {
    match fs::File::open(dir).and_then(|f| f.sync_all()) {
        Ok(()) => {}
        Err(e) => log::warn!("fsync {} 失败（不影响本次安装）: {e}", dir.display()),
    }
}

/// 全量 SHA-256 校验。不匹配就删掉 `.part`——**不能留着续传**，
/// 因为内容已经证明是错的，续传只会在错的基础上接着错。
fn verify(part: &Path, spec: &ModelSpec) -> Result<()> {
    let size = fs::metadata(part)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("读 .part 失败: {e}")))?
        .len();
    if spec.bytes > 0 && size != spec.bytes {
        let _ = fs::remove_file(part);
        return Err(anyhow::Error::new(Fail::Checksum)
            .context(format!("体积不符：期望 {} 字节，实得 {size}", spec.bytes)));
    }

    let mut f = fs::File::open(part)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("打开 .part 失败: {e}")))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("读 .part 失败: {e}")))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got = format!("{:x}", h.finalize());
    if got != spec.sha256 {
        let _ = fs::remove_file(part);
        return Err(anyhow::Error::new(Fail::Checksum).context(format!(
            "sha256 不符：期望 {}，实得 {got}",
            spec.sha256
        )));
    }
    log::info!("校验通过 sha256={}", &got[..12]);
    Ok(())
}

/// 下载前查磁盘。留 10% 余量——文件系统本身要开销，塞到一个字节不剩
/// 也不是好事。
fn ensure_space(dir: &Path, need: u64) -> Result<()> {
    let avail = available_bytes(dir).unwrap_or(u64::MAX);
    let want = need.saturating_add(need / 10);
    if avail < want {
        return Err(anyhow::Error::new(Fail::Disk).context(format!(
            "磁盘空间不足：需要约 {:.1} GB，可用 {:.1} GB",
            want as f64 / 1e9,
            avail as f64 / 1e9
        )));
    }
    Ok(())
}

fn available_bytes(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    let c = CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c 是有效的 NUL 结尾路径，st 是本栈上的合法可写对象。
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // f_bavail 是**非特权用户可用**的块数，不是 f_bfree（后者含保留块）。
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// `flock` 包装。进程退出（包括崩溃）时内核自动释放，不会留下死锁。
#[derive(Debug)]
struct FileLock(fs::File);

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("建锁文件失败: {e}")))?;
        // 非阻塞：拿不到就立刻告诉用户「另一个进程在下」，
        // 而不是让菜单卡在那里等一个可能几分钟的下载。
        // SAFETY: fd 来自上面刚打开的 File，在本作用域内有效。
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(anyhow::Error::new(Fail::Busy).context("另一个进程正在下载同一个模型"));
        }
        Ok(Self(f))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: fd 仍然有效（self.0 还没被 drop）。
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Fail::Network => "网络错误",
            Fail::Checksum => "校验失败",
            Fail::Disk => "磁盘空间不足",
            Fail::Busy => "另一个进程正在下载",
            Fail::Io => "文件系统错误",
        };
        f.write_str(s)
    }
}

impl std::error::Error for Fail {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 符号链接和目录都不能被当成「模型已就绪」。
    ///
    /// 这条挡的是一个很难查的失效模式：用户为了省空间把模型软链到别处，
    /// 链接目标后来被删了——`exists()` 会说 false 没问题，但**指向一个
    /// 还在的、内容不对的文件**时它会说 true，然后 whisper 加载报错，
    /// 错误信息里完全看不出是链接的问题。
    #[test]
    fn symlinks_and_dirs_are_not_present() {
        let tmp = std::env::temp_dir().join(format!("agentear-dl-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        let real = tmp.join("real.bin");
        fs::write(&real, b"x").unwrap();
        assert!(is_present(&real), "普通非空文件应算就绪");

        let link = tmp.join("link.bin");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(!is_present(&link), "符号链接不算就绪，哪怕它指向一个好文件");

        let dir = tmp.join("dir.bin");
        fs::create_dir_all(&dir).unwrap();
        assert!(!is_present(&dir), "目录不算就绪");

        let empty = tmp.join("empty.bin");
        fs::write(&empty, b"").unwrap();
        assert!(!is_present(&empty), "空文件不算就绪——多半是上次下崩了");

        fs::remove_dir_all(&tmp).ok();
    }

    /// 校验失败必须删掉 `.part`。
    ///
    /// 留着的话下次 `curl -C -` 会在错误内容后面接着写，
    /// 于是每次重试都下一遍全量、每次都校验失败，用户永远好不了。
    #[test]
    fn checksum_failure_removes_the_part_file() {
        let tmp = std::env::temp_dir().join(format!("agentear-dl-v-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let part = tmp.join("x.part");
        fs::write(&part, b"not the model").unwrap();

        let spec = ModelSpec {
            file: "x",
            url: "",
            // "not the model" 的 sha256 不是这个
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            bytes: 0,
        };
        let err = verify(&part, &spec).unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Checksum));
        assert!(!part.exists(), "校验失败的 .part 必须删掉");

        fs::remove_dir_all(&tmp).ok();
    }

    /// 体积对不上时**先于** SHA 报错，且同样删掉 `.part`。
    /// 体积检查便宜，能在几百毫秒的 SHA 之前挡住明显的失败。
    #[test]
    fn size_mismatch_is_caught_before_hashing() {
        let tmp = std::env::temp_dir().join(format!("agentear-dl-s-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let part = tmp.join("y.part");
        fs::write(&part, b"short").unwrap();

        let spec = ModelSpec { file: "y", url: "", sha256: "irrelevant", bytes: 999_999 };
        let err = verify(&part, &spec).unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Checksum));
        assert!(err.to_string().contains("体积不符"));
        assert!(!part.exists());

        fs::remove_dir_all(&tmp).ok();
    }

    /// 同一个进程里第二次 `acquire` 必须拿不到锁。
    ///
    /// ⚠️ 注意 `flock` 的语义是**按打开的文件描述而不是按进程**，
    /// 所以同进程再 `open` 一次是能测出互斥的。这条测的是我们
    /// 确实用了 `LOCK_NB`（阻塞的话这个测试会挂死）。
    #[test]
    fn lock_is_exclusive_and_nonblocking() {
        let tmp = std::env::temp_dir().join(format!("agentear-dl-l-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let lock = tmp.join("z.lock");

        let held = FileLock::acquire(&lock).expect("第一次应该拿得到");
        let second = FileLock::acquire(&lock);
        assert!(second.is_err(), "第二次必须立刻失败，不能阻塞");
        assert_eq!(second.unwrap_err().downcast_ref::<Fail>(), Some(&Fail::Busy));

        drop(held);
        FileLock::acquire(&lock).expect("释放后应该又能拿到");

        fs::remove_dir_all(&tmp).ok();
    }

    /// **`url` 和 `bytes` 必须同时与外部世界一致。**
    ///
    /// 这条不是「断言字符串等于它自己」——`url` 和 `bytes` 是**两个各自
    /// 维护的常量**，它要求 `url` 指向的东西确实是我们以为的那个。
    /// 域名改错、tag 拼错、资产被换掉，都会让它红。
    ///
    /// 仓库迁到 `iDoris-ai` 时，这个 URL 改动**没有任何判据**
    /// 守着（sha256 没变，因为还是同一个文件），这条测试补的就是那个洞。
    ///
    /// **它抓不住什么**：抓的是「URL 现在指对了」，不是「已装机的旧版本
    /// 还能下」——后者依赖 GitHub 的转移重定向，任何仓内判据都钉不住。
    #[test]
    #[ignore = "要联网：HEAD 一次模型 URL"]
    fn the_model_url_still_serves_exactly_the_bytes_we_expect() {
        let out = std::process::Command::new("/usr/bin/curl")
            .args(["-fLsI", "--max-time", "60", THAI.url])
            .output()
            .expect("跑 curl 失败");
        assert!(out.status.success(), "HEAD {} 失败", THAI.url);
        let head = String::from_utf8_lossy(&out.stdout).to_lowercase();
        // **取最后一个。** `curl -I -L` 会把整条重定向链的响应头都打出来，
        // 而 GitHub 的 release 资产是 302 到 CDN——第一个 `content-length`
        // 是那个 302 的 `0`。取第一个的话，这条测试**两侧都红**，
        // 什么都证明不了（第一版就是这么写的，跑红绿两侧才发现）。
        let len: u64 = head
            .lines()
            .filter_map(|l| l.strip_prefix("content-length:"))
            .filter_map(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .next_back()
            .unwrap_or_else(|| panic!("响应链里没有非零的 content-length:\n{head}"));
        assert_eq!(
            len, THAI.bytes,
            "URL 指向的文件大小和 ModelSpec.bytes 对不上——URL 或 bytes 有一个是错的"
        );
    }

    /// 旧的仓库 owner **不该再出现在源码里**。
    ///
    /// 迁仓时漏改一处 URL 是很容易发生的事，而它不会让任何别的测试变红。
    /// 这条几乎免费，而且能进默认那一轮（不打网络）。
    ///
    /// ⚠️ **它只禁旧名字，拦不住把新名字拼错**——那种情况要靠上面那条
    /// 联网测试。两条合起来才覆盖迁仓的两种失败形态。
    #[test]
    fn the_old_repository_owner_is_gone_from_the_source() {
        let mut offenders = Vec::new();
        for entry in fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            // 拼起来写，免得这行自己命中自己
            let old = concat!("jhf", "netboy");
            if text.contains(old) {
                offenders.push(path.display().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "源码里还有旧 owner，迁仓漏改了: {offenders:?}"
        );
    }
}
