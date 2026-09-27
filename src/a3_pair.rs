//! 与 Agent24 配对（A3 §3.6，jason Q2=b：AgentEar 设置里一键，代跑 `agent24` CLI）。
//!
//! 流程：找 CLI → `agent24 --version` 版本闸 → 把 manifest **原始字节**落盘 →
//! `agent24 os attach add <manifest> --json` → token 进 **macOS Keychain**，
//! `socket_path` / `token_id` / 注册时的 digest 进 config。
//!
//! ⚠️ **token 只在两处出现**：CLI 的 stdout（我们读完就进 Keychain）与 Keychain 本身。
//! 不进 config.json、不进日志、不进错误信息（错误里只放 `token_id`）。
//!
//! ⚠️ **只做不放宽隐私的注册**（A3 §3.5）：我们的 manifest 恒为 `local_only`；
//! CLI 在非 TTY 下永远不发 `allow_relax`，放宽必失败于 `relax_requires_confirmation`，
//! 这时提示用户去 Agent24 那边确认，**AgentEar 不替用户放宽**。

use crate::a3::{self, StopReason};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 引入 `os attach` 的 Agent24 版本。
///
/// TODO(T6.1.2)：**待 Agent24 A3-2 合并后填入真实版本**（A3 §3.6 `<A3_MIN_VERSION>`）。
/// 现在填 0.0.0 = 不挡任何版本；真正的兼容由 CLI 本身报错兜底（老 CLI 没有 `os attach` 子命令）。
pub const A3_MIN_VERSION: (u64, u64, u64) = (0, 0, 0);

/// Keychain 里存 token 的 service / account。
pub const KEYCHAIN_SERVICE: &str = "ai.idoris.agentear.agent24";
pub const KEYCHAIN_ACCOUNT: &str = "agentear";

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

pub fn version_ok(v: (u64, u64, u64)) -> bool {
    v >= A3_MIN_VERSION
}

fn check_version(cli: &Path) -> Result<()> {
    let out = Command::new(cli).arg("--version").output().with_context(|| format!("跑不了 {}", cli.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = parse_semver(&text).with_context(|| format!("看不懂 `agent24 --version` 的输出：{}", text.trim()))?;
    if !version_ok(v) {
        bail!(
            "Agent24 版本太旧（{}.{}.{}，要 ≥ {}.{}.{}）：请先升级 Agent24",
            v.0, v.1, v.2, A3_MIN_VERSION.0, A3_MIN_VERSION.1, A3_MIN_VERSION.2
        );
    }
    Ok(())
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

// ---------------------------------------------------------------- Keychain

pub fn keychain_store(service: &str, token: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(service, KEYCHAIN_ACCOUNT, token.as_bytes())
        .context("写 Keychain 失败")
}

pub fn keychain_load(service: &str) -> Option<String> {
    security_framework::passwords::get_generic_password(service, KEYCHAIN_ACCOUNT)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|s| !s.is_empty())
}

pub fn keychain_delete(service: &str) {
    let _ = security_framework::passwords::delete_generic_password(service, KEYCHAIN_ACCOUNT);
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
    check_version(&cli).map_err(|e| AddError::Failed(format!("{e:#}")))?;
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
    keychain_store(KEYCHAIN_SERVICE, &reg.token).map_err(|e| AddError::Failed(format!("{e:#}")))?;
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

/// 撤销配对：代跑 `attach revoke`（失败也继续清本地），删 Keychain、清 config、断开。
pub fn revoke() -> Result<()> {
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
    keychain_delete(KEYCHAIN_SERVICE);
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

/// 当前凭据（config + Keychain）。任何一样缺就是没配对。
pub fn current_creds() -> Option<a3::Creds> {
    let cfg = crate::config::get();
    let socket = cfg.agent24_socket_path.filter(|s| !s.is_empty())?;
    let token = keychain_load(KEYCHAIN_SERVICE)?;
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

fn start_blocking(data_root: PathBuf) {
    let cfg = crate::config::get();
    if cfg.agent24_socket_path.is_none() {
        a3::set_status(a3::LinkStatus::Unpaired);
    } else if needs_rotation(cfg.agent24_registered_digest.as_deref(), &a3::manifest_digest()) {
        log::info!("附着 manifest 变了：启动时自动重新注册");
        if let Err(r) = rotate(&data_root) {
            a3::set_status(a3::LinkStatus::NeedsAction(r));
        }
    }
    let root = data_root.clone();
    a3::start_supervisor(a3::Supervisor {
        creds: Box::new(current_creds),
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
    fn semver_parsing_and_gate() {
        assert_eq!(parse_semver("agent24 0.5.2\n"), Some((0, 5, 2)));
        assert_eq!(parse_semver("agent24 v1.10.3-beta (abc)"), Some((1, 10, 3)));
        assert_eq!(parse_semver("no version here"), None);
        assert!(version_ok(A3_MIN_VERSION));
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

    /// 用**独立的测试 service 名**，测完删掉，不碰用户真实的配对项。
    #[test]
    fn keychain_roundtrip_with_test_service() {
        let svc = format!("ai.idoris.agentear.agent24.test.{}", std::process::id());
        keychain_delete(&svc);
        assert_eq!(keychain_load(&svc), None);
        if keychain_store(&svc, "tok-value").is_err() {
            // CI / 无 GUI 会话时 Keychain 可能锁着：如实跳过，别假绿。
            eprintln!("Keychain 不可写（可能是无登录会话），跳过");
            return;
        }
        assert_eq!(keychain_load(&svc).as_deref(), Some("tok-value"));
        keychain_store(&svc, "tok-rotated").unwrap();
        assert_eq!(keychain_load(&svc).as_deref(), Some("tok-rotated"));
        keychain_delete(&svc);
        assert_eq!(keychain_load(&svc), None);
    }

    #[test]
    fn written_manifest_is_byte_identical() {
        let tmp = std::env::temp_dir().join(format!("a3man-{}", std::process::id()));
        let p = write_manifest(&tmp).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), a3::MANIFEST);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
