//! 与 Agent24 配对（A3 §3.6，jason Q2=b：AgentEar 设置里一键，代跑 `agent24` CLI）。
//!
//! 流程：找 CLI → `agent24 --version` 版本闸 → 把 manifest **原始字节**落盘 →
//! `agent24 os attach add <manifest> --json` → token 进 **`<数据目录>/agent24/token`（0600）**，
//! `socket_path` / `token_id` / 注册时的 digest 进 config。
//!
//! ⚠️ **token 只在两处出现**：CLI 的 stdout（我们读完就写文件）与那个 0600 文件本身。
//! 不进 config.json、不进日志、不进错误信息（错误里只放 `token_id`）、不进 argv、不进事件。
//!
//! ## 为什么不再用 macOS Keychain（v0.26.1，jason 2026-09-27 拍板）
//!
//! v0.25.0–v0.25.2 存 Keychain。但我们用的是**自签证书**（没有 Apple Team ID），macOS 的
//! 钥匙串「分区列表」对这种 app 按 **cdhash** 认人（实测该项 partition 里是 `cdhash:4935334a…`）；
//! 每次升级 cdhash 都变 → 升级后第一次启动弹「AgentEar wants to access key
//! ai.idoris.agentear.agent24 … enter the login keychain password」。
//! A3 的威胁模型本来就**不防同 UID**（SPEC-ME3 §0；A3 设计 §11 Q2 里也写了同 UID 下
//! 两种方案一样安全），所以 0600 文件与钥匙串等价，却不会每次升级都要密码。
//! 真想回到钥匙串，得先有 Team ID 签名（分区才会按 team 认）。
//!
//! ⚠️ **只做不放宽隐私的注册**（A3 §3.5）：我们的 manifest 恒为 `local_only`；
//! CLI 在非 TTY 下永远不发 `allow_relax`，放宽必失败于 `relax_requires_confirmation`，
//! 这时提示用户去 Agent24 那边确认，**AgentEar 不替用户放宽**。

use crate::a3::{self, StopReason};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 开始随正式版一起发出 A3 附着的 Agent24 版本（计划 v0.5.0）。
///
/// ⚠️ **只用于附加提示，不作门槛**：Agent24 在 ME4 期间没发版，main 上 `agent24 --version`
/// 一直报 0.3.0，而 A3 已经在里面了——按版本号挡会把能用的 CLI 拒掉。
/// 能不能用只看能力探测（[`probe_attach`]）。
pub const A3_RELEASE_VERSION: (u64, u64, u64) = (0, 5, 0);

/// token 文件名（在 `<数据目录>/agent24/` 下）。
pub const TOKEN_FILE: &str = "token";
/// v0.25.0–v0.25.2 用过的钥匙串项（只在日志里提示用户可以手动删，**不去读也不去删**，见 [`start_blocking`]）。
pub const LEGACY_KEYCHAIN_SERVICE: &str = "ai.idoris.agentear.agent24";

// ---------------------------------------------------------------- CLI 定位与版本

/// 找 `agent24`：设置里的覆盖路径 → `~/.agent24/bin/agent24` → PATH（A3 §3.6）。
pub fn find_cli(override_path: Option<&str>) -> Option<PathBuf> {
    find_cli_in(override_path, dirs::home_dir().as_deref(), std::env::var_os("PATH"))
}

pub fn find_cli_in(
    override_path: Option<&str>,
    home: Option<&Path>,
    path_var: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = override_path.map(str::trim).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        // 显式配置了就只认它：配错了要报出来，不悄悄换成别的 CLI。
        return is_executable(&p).then_some(p);
    }
    if let Some(h) = home {
        let p = h.join(".agent24/bin/agent24");
        if is_executable(&p) {
            return Some(p);
        }
    }
    for dir in std::env::split_paths(&path_var?) {
        let p = dir.join("agent24");
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// 从 `--version` 输出里挑第一个 `x.y.z`。
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    for tok in s.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = tok.split('.').collect();
        if parts.len() >= 3 {
            let n: Vec<Option<u64>> = parts[..3].iter().map(|p| p.parse().ok()).collect();
            if let [Some(a), Some(b), Some(c)] = n[..] {
                return Some((a, b, c));
            }
        }
    }
    None
}

/// 能力探测的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachSupport {
    /// `agent24 os attach list --json` 退出码 0 且输出是 `{"modules":[...]}`。
    Supported,
    /// 子命令不存在（clap 报 unrecognized subcommand）或输出不是预期形状：这个 CLI 不支持附着模块。
    Unsupported(String),
}

/// 按 `os attach list --json` 的结果判断 CLI 是否支持 A3（纯函数，便于用真值表测）。
pub fn judge_attach_probe(success: bool, stdout: &str, stderr: &str) -> AttachSupport {
    if !success {
        let why = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        return AttachSupport::Unsupported(if why.is_empty() { "`os attach list` 失败".into() } else { why });
    }
    match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(v) if v.get("modules").is_some_and(Value::is_array) => AttachSupport::Supported,
        _ => AttachSupport::Unsupported(format!(
            "`os attach list --json` 的输出不是 {{\"modules\":[...]}}：{}",
            stdout.trim().chars().take(120).collect::<String>()
        )),
    }
}

/// 跑一次 `agent24 os attach list --json`（只读，不改任何注册）。
///
/// ⚠️ **有副作用、别反复调**：没有常驻 daemon 时，CLI 会临时拉起一个 ephemeral daemon 应答后
/// 再收掉（agent24-13 在 Agent24 main 上实测）。所以只在「点连接」和「manifest digest 变了要
/// 自动轮换」这两处各探一次（都经 [`pair`]），**不在任何重连/定时循环里调用**。
pub fn probe_attach(cli: &Path) -> Result<AttachSupport> {
    let out = Command::new(cli)
        .args(["os", "attach", "list", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("跑不了 {}", cli.display()))?;
    Ok(judge_attach_probe(
        out.status.success(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    ))
}

/// 版本号只做附加提示（见 [`A3_RELEASE_VERSION`]）。
pub fn version_note(version_output: &str) -> Option<String> {
    let v = parse_semver(version_output)?;
    Some(if v >= A3_RELEASE_VERSION {
        format!("Agent24 {}.{}.{}：正式版已支持附着模块", v.0, v.1, v.2)
    } else {
        format!("Agent24 {}.{}.{}（开发版；是否支持以能力探测为准）", v.0, v.1, v.2)
    })
}

/// 配对前的兼容检查：**能力探测**，不是版本闸。
fn check_attach_support(cli: &Path) -> Result<()> {
    if let Ok(out) = Command::new(cli).arg("--version").output() {
        if let Some(note) = version_note(&String::from_utf8_lossy(&out.stdout)) {
            log::info!("{note}");
        }
    }
    match probe_attach(cli)? {
        AttachSupport::Supported => Ok(()),
        AttachSupport::Unsupported(why) => {
            bail!("你的 Agent24 版本还不支持附着模块，请升级 Agent24（{why}）")
        }
    }
}

// ---------------------------------------------------------------- CLI 输出

/// `agent24 os attach add --json` 的成功输出（A3 §3.6）。`Debug` 打码。
#[derive(Clone)]
pub struct Registration {
    pub name: String,
    pub manifest_digest: String,
    pub token: String,
    pub socket_path: String,
    pub token_id: String,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("name", &self.name)
            .field("manifest_digest", &self.manifest_digest)
            .field("token", &"<redacted>")
            .field("socket_path", &self.socket_path)
            .field("token_id", &self.token_id)
            .finish()
    }
}

/// 配对失败的分类（决定设置里怎么提示）。
#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    /// 放宽隐私，需要去 Agent24 确认（`relax_requires_confirmation`）。
    NeedsHostConfirm,
    /// 其它失败（CLI 给的 code + message，已去掉任何 token）。
    Failed(String),
}

/// 解析 `attach add --json` 的 stdout。成功 = 恰好一个带全部字段的对象；
/// 失败 = `{"error":{"code","message"}}`。
pub fn parse_add_output(success: bool, stdout: &str, expect_digest: &str) -> Result<Registration, AddError> {
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| AddError::Failed("CLI 的输出不是 JSON".into()))?;
    if let Some(err) = v.get("error") {
        let code = err["code"].as_str().unwrap_or("unknown");
        if code == "relax_requires_confirmation" {
            return Err(AddError::NeedsHostConfirm);
        }
        return Err(AddError::Failed(format!(
            "{code}：{}",
            err["message"].as_str().unwrap_or("")
        )));
    }
    if !success {
        return Err(AddError::Failed("CLI 退出码非 0".into()));
    }
    let field = |k: &str| -> Result<String, AddError> {
        v[k].as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| AddError::Failed(format!("CLI 输出缺字段 {k}")))
    };
    let reg = Registration {
        name: field("name")?,
        manifest_digest: field("manifest_digest")?,
        token: field("token")?,
        socket_path: field("socket_path")?,
        token_id: field("token_id")?,
    };
    if reg.name != a3::MODULE {
        return Err(AddError::Failed(format!("注册回来的名字不对：{}", reg.name)));
    }
    // 内核算的 digest 必须等于我们本地算的——否则握手必然 manifest_mismatch。
    if reg.manifest_digest != expect_digest {
        return Err(AddError::Failed(format!(
            "digest 对不上（内核 {}，本地 {}）",
            reg.manifest_digest, expect_digest
        )));
    }
    Ok(reg)
}

// ---------------------------------------------------------------- token 文件

/// `<数据目录>/agent24/token`。
pub fn token_path(data_root: &Path) -> PathBuf {
    data_root.join("agent24").join(TOKEN_FILE)
}

/// token 文件的状态。`Debug` 打码（`Present` 永不打出 token）。
#[derive(Clone, PartialEq, Eq)]
pub enum TokenState {
    /// 没有文件（没配对，或者从 Keychain 时代升级上来）。
    Missing,
    /// 权限合格（只有属主可读写）且非空。
    Present(String),
    /// 权限比 0600 宽（组或其他人有任何位）——**拒绝使用**，照 ssh 的做法（带实际 mode）。
    TooOpen(u32),
    /// 其它问题：是符号链接、不是普通文件、属主不是当前用户、读不了、是空的。
    Unusable(String),
}

impl std::fmt::Debug for TokenState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenState::Missing => write!(f, "Missing"),
            TokenState::Present(_) => write!(f, "Present(<redacted>)"),
            TokenState::TooOpen(m) => write!(f, "TooOpen({m:o})"),
            TokenState::Unusable(why) => write!(f, "Unusable({why})"),
        }
    }
}

/// 读 token（不打日志、不回显内容）。
pub fn load_token(data_root: &Path) -> TokenState {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let p = token_path(data_root);
    // symlink_metadata：不跟随链接——被人换成链接指到别处就不用。
    let meta = match std::fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TokenState::Missing,
        Err(e) => return TokenState::Unusable(format!("读不了 {}：{e}", p.display())),
    };
    if !meta.file_type().is_file() {
        return TokenState::Unusable(format!("{} 不是普通文件", p.display()));
    }
    // SAFETY: getuid 没有前置条件、不会失败。
    if meta.uid() != unsafe { libc::getuid() } {
        return TokenState::Unusable(format!("{} 的属主不是当前用户", p.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return TokenState::TooOpen(mode);
    }
    match std::fs::read_to_string(&p) {
        Ok(s) if !s.trim().is_empty() => TokenState::Present(s.trim().to_string()),
        Ok(_) => TokenState::Unusable(format!("{} 是空的", p.display())),
        Err(e) => TokenState::Unusable(format!("读不了 {}：{e}", p.display())),
    }
}

/// 原子写 token：目录 0700 → 临时文件（0600 创建）→ 写 + fsync → rename → fsync 目录 → 校验权限。
pub fn store_token(data_root: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    anyhow::ensure!(!token.trim().is_empty(), "拿到的 token 是空的");
    let dir = data_root.join("agent24");
    std::fs::create_dir_all(&dir).with_context(|| format!("建不了 {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("改不了 {} 的权限", dir.display()))?;
    let path = token_path(data_root);
    let tmp = dir.join(format!("{TOKEN_FILE}.tmp"));
    let _ = std::fs::remove_file(&tmp); // 上次崩在半路留下的
    {
        // mode(0o600) 在创建那一刻就生效，不存在「先 0644 再 chmod」的窗口。
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("写不了 {}", tmp.display()))?;
        f.write_all(token.trim().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("换不上 {}", path.display()))?;
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    // 写后校验：umask / 文件系统怪癖都不许把它变宽。
    match load_token(data_root) {
        TokenState::Present(_) => Ok(()),
        TokenState::TooOpen(m) => bail!("token 文件权限是 {m:o}，不是 600"),
        other => bail!("token 写完读不回来：{other:?}"),
    }
}

/// 删 token 文件（撤销配对时）。没有就算了。
pub fn delete_token(data_root: &Path) {
    let _ = std::fs::remove_file(token_path(data_root));
}

// ---------------------------------------------------------------- 配对 / 撤销 / 轮换

/// 把 manifest **原始字节**写到数据目录，交给 CLI（字节必须与 [`a3::MANIFEST`] 完全相同）。
pub fn write_manifest(data_root: &Path) -> Result<PathBuf> {
    let dir = data_root.join("agent24");
    std::fs::create_dir_all(&dir)?;
    let p = dir.join("domain-os.yml");
    let tmp = dir.join("domain-os.yml.tmp");
    std::fs::write(&tmp, a3::MANIFEST)?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

/// 代跑 `attach add` 并落凭据。成功后唤醒连接守护线程。
pub fn pair(data_root: &Path) -> Result<(), AddError> {
    let cfg = crate::config::get();
    let cli = find_cli(cfg.agent24_cli_path.as_deref()).ok_or_else(|| {
        AddError::Failed("找不到 agent24 命令（设置里可指定路径，或装到 ~/.agent24/bin/）".into())
    })?;
    check_attach_support(&cli).map_err(|e| AddError::Failed(format!("{e:#}")))?;
    let manifest = write_manifest(data_root).map_err(|e| AddError::Failed(format!("写 manifest 失败：{e:#}")))?;
    let out = Command::new(&cli)
        .args(["os", "attach", "add"])
        .arg(&manifest)
        .arg("--json")
        // 非 TTY：CLI 不会发 allow_relax（A3 §3.5），放宽必失败——这正是我们要的。
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| AddError::Failed(format!("跑不了 {}：{e}", cli.display())))?;
    let reg = parse_add_output(
        out.status.success(),
        &String::from_utf8_lossy(&out.stdout),
        &a3::manifest_digest(),
    )?;
    store_token(data_root, &reg.token).map_err(|e| AddError::Failed(format!("{e:#}")))?;
    crate::config::update(|c| {
        c.agent24_socket_path = Some(reg.socket_path.clone());
        c.agent24_token_id = Some(reg.token_id.clone());
        c.agent24_registered_digest = Some(reg.manifest_digest.clone());
    });
    log::info!("已与 Agent24 配对（token_id {}，socket {}）", reg.token_id, reg.socket_path);
    a3::set_status(a3::LinkStatus::Connecting);
    a3::wake();
    Ok(())
}

/// 撤销配对：代跑 `attach revoke`（失败也继续清本地），删 token 文件、清 config、断开。
pub fn revoke(data_root: &Path) -> Result<()> {
    let cfg = crate::config::get();
    let mut remote_err = None;
    match find_cli(cfg.agent24_cli_path.as_deref()) {
        Some(cli) => {
            let out = Command::new(&cli)
                .args(["os", "attach", "revoke", a3::MODULE, "--json"])
                .stdin(std::process::Stdio::null())
                .output();
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => remote_err = Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
                Err(e) => remote_err = Some(e.to_string()),
            }
        }
        None => remote_err = Some("找不到 agent24 命令".into()),
    }
    delete_token(data_root);
    crate::config::update(|c| {
        c.agent24_socket_path = None;
        c.agent24_token_id = None;
        c.agent24_registered_digest = None;
    });
    a3::disconnect_now();
    a3::set_status(a3::LinkStatus::Unpaired);
    if let Some(e) = remote_err {
        bail!("本地配对已清除，但 Agent24 那边的撤销没确认成功：{e}（可在 Agent24 里手动撤销）");
    }
    Ok(())
}

/// 当前凭据（config + token 文件）。任何一样缺（或 token 文件权限不合格）就当没配对。
pub fn current_creds(data_root: &Path) -> Option<a3::Creds> {
    creds_from(crate::config::get().agent24_socket_path, data_root)
}

/// [`current_creds`] 的判据（不碰全局 config，便于测试）。
pub fn creds_from(socket: Option<String>, data_root: &Path) -> Option<a3::Creds> {
    let socket = socket.filter(|s| !s.is_empty())?;
    let token = match load_token(data_root) {
        TokenState::Present(t) => t,
        _ => return None,
    };
    Some(a3::Creds {
        socket: PathBuf::from(socket),
        token,
        digest: a3::manifest_digest(),
    })
}

/// 本地 manifest 与上次注册的不一样（升级改了 manifest）→ 要轮换。
pub fn needs_rotation(registered: Option<&str>, local: &str) -> bool {
    registered.is_some_and(|r| r != local)
}

/// 守护线程的自动轮换（§5.6 manifest_mismatch 那一行）。
pub fn rotate(data_root: &Path) -> Result<(), StopReason> {
    let cfg = crate::config::get();
    if !needs_rotation(cfg.agent24_registered_digest.as_deref(), &a3::manifest_digest()) {
        // digest 与上次注册相同却仍不符：凭据/记录出了问题，只能重新配对。
        return Err(StopReason::Repair);
    }
    match pair(data_root) {
        Ok(()) => Ok(()),
        Err(AddError::NeedsHostConfirm) => Err(StopReason::ConfirmOnHost),
        Err(AddError::Failed(e)) => {
            log::warn!("自动重新注册失败：{e}");
            Err(StopReason::Repair)
        }
    }
}

/// 守护进程启动时调：配对过就起连接守护线程；manifest 变了先轮换一次。
///
/// **整个放后台线程**：轮换要代跑 CLI（可能几秒），不能挡住菜单栏图标出现。
pub fn start(data_root: PathBuf) {
    std::thread::Builder::new()
        .name("a3-start".into())
        .spawn(move || start_blocking(data_root))
        .expect("起不了 Agent24 启动线程");
}

/// 启动时该做什么（纯函数，真值表测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupPlan {
    /// 没配对过。
    Unpaired,
    /// 凭据齐全，直接连。
    Connect,
    /// manifest 变了：代跑 `attach add` 轮换（顺带也会重写 token 文件）。
    Rotate,
    /// **从 Keychain 时代升级上来**（config 说配对过，token 文件却没有）：代跑 `attach add`
    /// 重新签发一个 token 写进文件。**绝不去读旧钥匙串项**——读就会弹密码框，那正是要修的问题。
    Reissue,
    /// token 文件存在但不能用（权限太宽 / 不是普通文件 / 属主不对 / 空）：要用户重新配对。
    Repair(String),
}

pub fn startup_plan(paired: bool, token: &TokenState, digest_changed: bool) -> StartupPlan {
    if !paired {
        return StartupPlan::Unpaired;
    }
    if digest_changed {
        return StartupPlan::Rotate;
    }
    match token {
        TokenState::Present(_) => StartupPlan::Connect,
        TokenState::Missing => StartupPlan::Reissue,
        TokenState::TooOpen(m) => StartupPlan::Repair(format!(
            "token 文件权限是 {m:o}，比 600 宽，不用它（chmod 600 或在设置里重新连接 Agent24）"
        )),
        TokenState::Unusable(why) => StartupPlan::Repair(why.clone()),
    }
}

/// 重新签发（迁移用）：结果映射同 [`rotate`]。
fn reissue(data_root: &Path) -> Result<(), StopReason> {
    match pair(data_root) {
        Ok(()) => Ok(()),
        Err(AddError::NeedsHostConfirm) => Err(StopReason::ConfirmOnHost),
        Err(AddError::Failed(e)) => {
            log::warn!("重新签发 Agent24 token 失败：{e}");
            Err(StopReason::Repair)
        }
    }
}

/// 执行启动计划。返回要设的连接状态（`None` = 不动，交给连接守护线程）。
/// 轮换 / 重新签发以闭包注入，好在测试里验证「迁移时真的去重新签发了」。
pub fn execute_plan(
    plan: StartupPlan,
    rotate: impl FnOnce() -> Result<(), StopReason>,
    reissue: impl FnOnce() -> Result<(), StopReason>,
) -> Option<a3::LinkStatus> {
    match plan {
        StartupPlan::Unpaired => Some(a3::LinkStatus::Unpaired),
        StartupPlan::Connect => None,
        StartupPlan::Rotate => {
            log::info!("附着 manifest 变了：启动时自动重新注册");
            rotate().err().map(a3::LinkStatus::NeedsAction)
        }
        StartupPlan::Reissue => {
            log::info!(
                "Agent24 token 改存本地文件（v0.26.1）：不读旧钥匙串项（读会弹密码框），\
                 代跑 `agent24 os attach add` 重新签发一次"
            );
            match reissue() {
                Ok(()) => {
                    log::info!(
                        "已重新签发；旧钥匙串项「{LEGACY_KEYCHAIN_SERVICE}」里的 token 已随之作废，\
                         可在「钥匙串访问」里手动删除（AgentEar 不去碰它，碰就会弹框）"
                    );
                    None
                }
                Err(r) => Some(a3::LinkStatus::NeedsAction(r)),
            }
        }
        StartupPlan::Repair(why) => {
            log::warn!("Agent24 token 文件不可用：{why}");
            Some(a3::LinkStatus::NeedsAction(StopReason::Repair))
        }
    }
}

/// 启动时的判定 + 执行（不起连接守护线程）。守护进程与 `--agent24-startup` 共用这一段。
pub fn run_startup(data_root: &Path) -> (StartupPlan, Option<a3::LinkStatus>) {
    let cfg = crate::config::get();
    let paired = cfg.agent24_socket_path.as_deref().is_some_and(|s| !s.is_empty());
    let token = load_token(data_root);
    let plan = startup_plan(
        paired,
        &token,
        needs_rotation(cfg.agent24_registered_digest.as_deref(), &a3::manifest_digest()),
    );
    let st = execute_plan(plan.clone(), || rotate(data_root), || reissue(data_root));
    (plan, st)
}

fn start_blocking(data_root: PathBuf) {
    if let (_, Some(st)) = run_startup(&data_root) {
        a3::set_status(st);
    }
    let root = data_root.clone();
    let creds_root = data_root.clone();
    a3::start_supervisor(a3::Supervisor {
        creds: Box::new(move || current_creds(&creds_root)),
        rotate: Box::new(move || rotate(&root)),
        handler: a3::default_handler(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_lookup_order_is_override_then_home_then_path() {
        let tmp = std::env::temp_dir().join(format!("a3pair-{}", std::process::id()));
        let home = tmp.join("home");
        let pathdir = tmp.join("bin");
        std::fs::create_dir_all(home.join(".agent24/bin")).unwrap();
        std::fs::create_dir_all(&pathdir).unwrap();
        let mk = |p: &Path| {
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let in_path = pathdir.join("agent24");
        mk(&in_path);
        let path_var = Some(std::ffi::OsString::from(pathdir.as_os_str()));
        // 只有 PATH 里有
        assert_eq!(find_cli_in(None, Some(&home), path_var.clone()), Some(in_path.clone()));
        // ~/.agent24/bin 优先于 PATH
        let in_home = home.join(".agent24/bin/agent24");
        mk(&in_home);
        assert_eq!(find_cli_in(None, Some(&home), path_var.clone()), Some(in_home.clone()));
        // 覆盖路径优先；覆盖路径不可执行 → None（不悄悄换成别的）
        let ov = tmp.join("custom-agent24");
        mk(&ov);
        assert_eq!(find_cli_in(Some(ov.to_str().unwrap()), Some(&home), path_var.clone()), Some(ov.clone()));
        assert_eq!(find_cli_in(Some("/nonexistent/agent24"), Some(&home), path_var), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn semver_parsing_and_version_is_only_a_note() {
        assert_eq!(parse_semver("agent24 0.5.2\n"), Some((0, 5, 2)));
        assert_eq!(parse_semver("agent24 v1.10.3-beta (abc)"), Some((1, 10, 3)));
        assert_eq!(parse_semver("no version here"), None);
        assert!(version_note("agent24 0.5.0").unwrap().contains("已支持"));
        assert!(version_note("agent24 0.3.0").unwrap().contains("以能力探测为准"));
        assert_eq!(version_note("garbage"), None);
    }

    #[test]
    fn attach_probe_truth_table() {
        assert_eq!(judge_attach_probe(true, "{\"modules\":[]}\n", ""), AttachSupport::Supported);
        assert_eq!(
            judge_attach_probe(true, r#"{"modules":[{"name":"agentear"}]}"#, ""),
            AttachSupport::Supported
        );
        assert!(matches!(
            judge_attach_probe(false, "", "error: unrecognized subcommand 'attach'\n"),
            AttachSupport::Unsupported(w) if w.contains("unrecognized subcommand")
        ));
        assert!(matches!(judge_attach_probe(true, "not json", ""), AttachSupport::Unsupported(_)));
        assert!(matches!(judge_attach_probe(true, "{\"modules\":{}}", ""), AttachSupport::Unsupported(_)));
        assert!(matches!(judge_attach_probe(true, "[]", ""), AttachSupport::Unsupported(_)));
    }

    /// 用假 CLI 脚本跑真实的 `probe_attach`（调用点），覆盖三种：支持 / 子命令不存在 / 输出非 JSON。
    #[test]
    fn attach_probe_against_fake_clis() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agentear-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let supported = mk(
            "supported",
            r#"if [ "$1 $2 $3 $4" = "os attach list --json" ]; then echo '{"modules":[]}'; exit 0; fi; exit 9"#,
        );
        let old = mk(
            "old",
            "echo \"error: unrecognized subcommand 'attach'\" >&2; exit 2",
        );
        let garbage = mk("garbage", "echo 'hello'; exit 0");
        assert_eq!(probe_attach(&supported).unwrap(), AttachSupport::Supported);
        assert!(matches!(
            probe_attach(&old).unwrap(),
            AttachSupport::Unsupported(w) if w.contains("unrecognized subcommand")
        ));
        assert!(matches!(probe_attach(&garbage).unwrap(), AttachSupport::Unsupported(_)));
        let e = check_attach_support(&old).unwrap_err().to_string();
        assert!(e.contains("还不支持附着模块"), "{e}");
        assert!(check_attach_support(&supported).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const D: &str = "sha256:aa";

    #[test]
    fn add_output_success_and_redaction() {
        let ok = r#"{"name":"agentear","manifest_digest":"sha256:aa","token":"SECRET-TOKEN","socket_path":"/tmp/x.sock","token_id":"tok_12345678"}"#;
        let r = parse_add_output(true, ok, D).unwrap();
        assert_eq!(r.token, "SECRET-TOKEN");
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("SECRET-TOKEN"), "Debug 不许带 token：{dbg}");
    }

    #[test]
    fn add_output_errors() {
        let relax = r#"{"error":{"code":"relax_requires_confirmation","message":"x"}}"#;
        assert_eq!(parse_add_output(false, relax, D).unwrap_err(), AddError::NeedsHostConfirm);
        let taken = r#"{"error":{"code":"name_taken","message":"已装包"}}"#;
        assert!(matches!(parse_add_output(false, taken, D), Err(AddError::Failed(m)) if m.contains("name_taken")));
        let wrong_digest = r#"{"name":"agentear","manifest_digest":"sha256:bb","token":"t","socket_path":"/s","token_id":"tok_1"}"#;
        assert!(matches!(parse_add_output(true, wrong_digest, D), Err(AddError::Failed(m)) if m.contains("digest")));
        let missing = r#"{"name":"agentear","manifest_digest":"sha256:aa","socket_path":"/s","token_id":"tok_1"}"#;
        assert!(matches!(parse_add_output(true, missing, D), Err(AddError::Failed(m)) if m.contains("token")));
        assert!(parse_add_output(true, "not json", D).is_err());
        let other_name = r#"{"name":"evil","manifest_digest":"sha256:aa","token":"t","socket_path":"/s","token_id":"tok_1"}"#;
        assert!(parse_add_output(true, other_name, D).is_err());
    }

    #[test]
    fn rotation_only_when_registered_digest_differs() {
        assert!(!needs_rotation(None, "sha256:a"));
        assert!(!needs_rotation(Some("sha256:a"), "sha256:a"));
        assert!(needs_rotation(Some("sha256:old"), "sha256:a"));
    }

    fn tmp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "a3tok-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn token_file_roundtrip_is_0600_and_dir_0700() {
        let root = tmp_root("rt");
        assert_eq!(load_token(&root), TokenState::Missing);
        store_token(&root, "tok-value\n").unwrap();
        assert_eq!(load_token(&root), TokenState::Present("tok-value".into()));
        assert_eq!(mode_of(&token_path(&root)), 0o600);
        assert_eq!(mode_of(&root.join("agent24")), 0o700);
        // 轮换：原子替换，不留临时文件。
        store_token(&root, "tok-rotated").unwrap();
        assert_eq!(load_token(&root), TokenState::Present("tok-rotated".into()));
        assert!(!root.join("agent24").join("token.tmp").exists());
        delete_token(&root);
        assert_eq!(load_token(&root), TokenState::Missing);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn store_survives_a_stale_tmp_and_a_wide_umask() {
        let root = tmp_root("stale");
        std::fs::create_dir_all(root.join("agent24")).unwrap();
        std::fs::write(root.join("agent24/token.tmp"), "half-written").unwrap();
        // SAFETY: umask 只影响本进程随后新建的文件；测试完恢复。
        let old = unsafe { libc::umask(0) };
        let r = store_token(&root, "tok-x");
        unsafe { libc::umask(old) };
        r.unwrap();
        assert_eq!(mode_of(&token_path(&root)), 0o600, "umask 0 也不能把它变宽");
        assert_eq!(load_token(&root), TokenState::Present("tok-x".into()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wider_than_0600_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_root("wide");
        store_token(&root, "tok-y").unwrap();
        for m in [0o640, 0o604, 0o644, 0o660] {
            std::fs::set_permissions(token_path(&root), std::fs::Permissions::from_mode(m)).unwrap();
            assert_eq!(load_token(&root), TokenState::TooOpen(m), "mode {m:o}");
        }
        std::fs::set_permissions(token_path(&root), std::fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(load_token(&root), TokenState::Present("tok-y".into()), "只读 0400 更严，照用");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn symlink_empty_and_non_file_are_unusable() {
        let root = tmp_root("odd");
        std::fs::create_dir_all(root.join("agent24")).unwrap();
        let target = root.join("elsewhere");
        std::fs::write(&target, "tok-z").unwrap();
        std::os::unix::fs::symlink(&target, token_path(&root)).unwrap();
        assert!(matches!(load_token(&root), TokenState::Unusable(_)), "符号链接不跟随");
        std::fs::remove_file(token_path(&root)).unwrap();
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create(true).mode(0o600).open(token_path(&root)).unwrap();
        }
        assert!(matches!(load_token(&root), TokenState::Unusable(_)), "空文件");
        std::fs::remove_file(token_path(&root)).unwrap();
        std::fs::create_dir(token_path(&root)).unwrap();
        assert!(matches!(load_token(&root), TokenState::Unusable(_)), "目录");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn token_state_debug_never_prints_the_token() {
        let s = format!("{:?}", TokenState::Present("SECRET-TOKEN-XYZ".into()));
        assert!(!s.contains("SECRET-TOKEN-XYZ"), "{s}");
    }

    /// 调用点：每种计划真的调了该调的那一个（而且只调它），结果映射到对的状态。
    #[test]
    fn execute_plan_calls_the_right_path() {
        use std::cell::Cell;
        let (rot, re) = (Cell::new(0), Cell::new(0));
        let run = |plan, r_ok: bool, i_ok: bool| {
            execute_plan(
                plan,
                || {
                    rot.set(rot.get() + 1);
                    if r_ok { Ok(()) } else { Err(StopReason::ConfirmOnHost) }
                },
                || {
                    re.set(re.get() + 1);
                    if i_ok { Ok(()) } else { Err(StopReason::Repair) }
                },
            )
        };
        // 迁移：去重新签发，成功就交给守护线程连（不设状态）。
        assert_eq!(run(StartupPlan::Reissue, true, true), None);
        assert_eq!((rot.get(), re.get()), (0, 1));
        // 迁移失败（Agent24 没开 / CLI 找不到）→ 需要重新配对。
        assert_eq!(run(StartupPlan::Reissue, true, false), Some(a3::LinkStatus::NeedsAction(StopReason::Repair)));
        assert_eq!((rot.get(), re.get()), (0, 2));
        assert_eq!(run(StartupPlan::Rotate, false, true), Some(a3::LinkStatus::NeedsAction(StopReason::ConfirmOnHost)));
        assert_eq!((rot.get(), re.get()), (1, 2));
        assert_eq!(run(StartupPlan::Connect, true, true), None);
        assert_eq!(run(StartupPlan::Unpaired, true, true), Some(a3::LinkStatus::Unpaired));
        assert_eq!(
            run(StartupPlan::Repair("x".into()), true, true),
            Some(a3::LinkStatus::NeedsAction(StopReason::Repair))
        );
        assert_eq!((rot.get(), re.get()), (1, 2), "Connect/Unpaired/Repair 不许代跑 CLI");
    }

    /// current_creds 的判据：只有 socket 与合格的 token 文件都在才交出凭据。
    #[test]
    fn creds_need_socket_and_a_valid_token_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_root("creds");
        let sock = Some("/tmp/a24.sock".to_string());
        assert!(creds_from(sock.clone(), &root).is_none(), "没 token 文件");
        store_token(&root, "tok-c").unwrap();
        let c = creds_from(sock.clone(), &root).expect("齐全");
        assert_eq!(c.token, "tok-c");
        assert!(creds_from(None, &root).is_none(), "没配对");
        assert!(creds_from(Some(String::new()), &root).is_none(), "空 socket");
        std::fs::set_permissions(token_path(&root), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(creds_from(sock, &root).is_none(), "权限太宽的 token 不许交出去");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 启动计划真值表：迁移（配对过、没文件）必须走重新签发，**不是读钥匙串、也不是当没配对**。
    #[test]
    fn startup_plan_truth_table() {
        let present = TokenState::Present("t".into());
        assert_eq!(startup_plan(false, &present, false), StartupPlan::Unpaired);
        assert_eq!(startup_plan(false, &TokenState::Missing, true), StartupPlan::Unpaired);
        assert_eq!(startup_plan(true, &present, false), StartupPlan::Connect);
        assert_eq!(startup_plan(true, &TokenState::Missing, false), StartupPlan::Reissue);
        assert_eq!(startup_plan(true, &present, true), StartupPlan::Rotate);
        assert_eq!(startup_plan(true, &TokenState::Missing, true), StartupPlan::Rotate);
        assert!(matches!(startup_plan(true, &TokenState::TooOpen(0o644), false), StartupPlan::Repair(_)));
        assert!(matches!(
            startup_plan(true, &TokenState::Unusable("x".into()), false),
            StartupPlan::Repair(_)
        ));
    }

    #[test]
    fn written_manifest_is_byte_identical() {
        let tmp = std::env::temp_dir().join(format!("a3man-{}", std::process::id()));
        let p = write_manifest(&tmp).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), a3::MANIFEST);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
