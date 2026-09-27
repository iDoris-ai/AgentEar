//! 宿主适配层：让 AgentEar 既能**独立运行**，又能**附着到 Agent24** 当它的耳朵和嘴巴。
//!
//! 约定见 `docs/agent24-embedding.md`、`contracts/`（事件/命令/提案的 JSON Schema），
//! 边界见 ADR-0008。这一层只做三件事：
//!
//! 1. **推理走哪条路**（[`LlmTransport`]）：本机边车 / 直连 iDoris / 经宿主 `_a24/model/complete`；
//! 2. **向宿主报告**（[`Emitter`]）：按 `agentear.event/1` 发 turn / transcript / proposal /
//!    speech / error / confirm_reply；
//! 3. **接宿主的命令**（[`CommandCenter`] + [`handle_inbound`]）：`speak` / `stop_playback`，
//!    幂等、排队、清队。
//!
//! 外加一个**附着状态机**（[`AttachState`]、[`after_disconnect`]）：断开之后是回独立模式
//! 还是停听提示（B5，jason 2026-09-27 拍板）。
//!
//! ## 这一版（P1）不做什么
//!
//! **不连真实宿主。** A3 附着协议（Unix socket 注册 / token / 握手 / 重连 / 入站命令的
//! HTTP 服务端）由 Agent24 侧冻结后在 P2 实现为 `A3Host`。这里只有：
//! - [`NoHost`]：独立模式（进程里没有宿主），所有上报都是空操作——
//!   **独立版的行为因此一个字节都不变**；
//! - [`FakeHost`]：进程内的假宿主，测试和 `--talk-turn --host fake` 用。
//!
//! ⚠️ **命令不从同一条回调连接进来。** Agent24 的内核→模块命令是经模块提供的 HTTP
//! （UDS）投递的，所以「接命令」不放进 [`HostLink`]，而是一个与传输无关的入口
//! [`handle_inbound`]：P2 的 HTTP 服务端收到请求体就交给它，返回值就是响应体。
//!
//! ⚠️ **音频永不出本机**：事件里只放文字和 `content_hash`（raw 音频的 sha256），
//! 不放音频字节、不放音频路径（有用例钉住）。

// P1 只有 `NoHost` / `FakeHost`：真实宿主 `A3Host`（P2）才会用到的部分
// （`parse_model_result`、`HostError::from_rpc_error`、`detach` 等）在非测试构建里暂时没有调用方。
// 它们不是死代码，是 P2 的接口，并且每一个都有用例钉住。
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::talk::{self, LlmEngine, TalkLang, TtsEngine};

// ---------------------------------------------------------------- 推理走哪条路

/// 独立模式下对话 LLM 的三档（`talk_llm_transport`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmTransport {
    /// 本机 OpenAI 兼容边车（默认）。
    Sidecar,
    /// 直连 iDoris 网关。
    Idoris,
    /// 经 Agent24 宿主（只有附着时可用）。
    Agent24,
}

impl LlmTransport {
    /// 不认识的取值按默认档处理并记日志：**配置写错不该让对话整个坏掉**，
    /// 而默认档恰好是最保守的（本机）。
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "" | "sidecar" => Self::Sidecar,
            "idoris" => Self::Idoris,
            "agent24" => Self::Agent24,
            other => {
                log::warn!("talk_llm_transport = {other:?} 不认识，按 sidecar（本机边车）处理");
                Self::Sidecar
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sidecar => "sidecar",
            Self::Idoris => "idoris",
            Self::Agent24 => "agent24",
        }
    }
}

/// 回环地址判定：只有 `127.x.x.x` / `localhost` / `[::1]` 算本机。
///
/// 与 Agent24 `loopback_only` 同一口径（jason D2：LAN / Tailscale 上的机器**算远程**）。
pub fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .trim()
        .strip_prefix("http://")
        .or_else(|| url.trim().strip_prefix("https://"))
        .unwrap_or(url.trim());
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // userinfo@host:port —— 取 @ 之后
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    // ⚠️ **按 IP 真解析，不按前缀猜**：`127.0.0.1.evil.com` 以 `127.` 开头，
    // 却是一个会被 DNS 解析到任意地方的域名（用例钉住）。
    if host == "localhost" {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 独立模式的推理**是不是本机的**——B5 断连默认值的判据。
///
/// - 本机边车且地址是回环 → 本机；
/// - mock → 本机（写死的句子，根本不出进程）；
/// - 直连 iDoris → **不算**：iDoris 即使监听 127.0.0.1，也可能把请求转给外部 API
///   （ADR-032 R13 那个缺口），AgentEar 这边核实不了；
/// - 经宿主 → 不算（宿主都断开了，这一档本来就不可用）。
pub fn standalone_is_local(cfg: &crate::config::Config) -> bool {
    if cfg.talk_llm_engine == "mock" {
        return true;
    }
    match LlmTransport::parse(&cfg.talk_llm_transport) {
        LlmTransport::Sidecar => {
            is_loopback_url(cfg.talk_llm_url.as_deref().unwrap_or(talk::DEFAULT_LLM_URL))
        }
        LlmTransport::Idoris | LlmTransport::Agent24 => false,
    }
}

/// `X-iDoris-Privacy` 取值。**只认显式的 `any`**，其它一律 `local_only`。
pub fn idoris_privacy_header(configured: &str) -> &'static str {
    if configured.trim() == "any" {
        "any"
    } else {
        "local_only"
    }
}

/// 按配置挑对话引擎。返回 `None` = 走独立版默认那条路（本机边车），
/// 调用方（`talk::Engines::from_config`）原样构造，**这一档的行为不因本模块而变**。
pub fn llm_engine(cfg: &crate::config::Config) -> Option<Arc<dyn LlmEngine>> {
    if attached() {
        let link = link();
        if !link.grants_models() {
            return no_model_grant(cfg, link);
        }
        return Some(Arc::new(HostLlm::new(link, cfg)));
    }
    match LlmTransport::parse(&cfg.talk_llm_transport) {
        LlmTransport::Sidecar => None,
        LlmTransport::Idoris => {
            let url = cfg
                .idoris_url
                .clone()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| {
                    log::warn!("talk_llm_transport = idoris 但没配 idoris_url，这一档会连不上");
                    String::new()
                });
            let headers = vec![format!(
                "X-iDoris-Privacy: {}",
                idoris_privacy_header(&cfg.idoris_privacy)
            )];
            Some(Arc::new(
                talk::OpenAiCompat::new(
                    url,
                    Arc::new(crate::sidecar::CurlWith { headers }),
                    cfg.talk_timeout_secs,
                )
                .with_model(cfg.idoris_model.clone()),
            ))
        }
        // 没附着却选了「经宿主」：如实失败，**不偷偷退回边车**——
        // 那会让「我以为走的是 Agent24 的隐私策略」变成假的。
        LlmTransport::Agent24 => Some(Arc::new(HostLlm::new(Arc::new(NoHost), cfg))),
    }
}

/// 附着着、但宿主**没授予模型**（A3 §4.3，AgentEar 对齐 2）。
///
/// - 独立推理档是本机的（回环边车 / mock）→ 视同 `local_only` 可用：返回 `None` 走本机边车，
///   并报一条 `error{code: forbidden, "宿主未授予模型"}` 让宿主知道；
/// - 否则 → **只转写、不回答**：返回一个必失败的宿主引擎（`forbidden`），绝不改发可能出本机的路径。
pub fn no_model_grant(cfg: &crate::config::Config, link: Arc<dyn HostLink>) -> Option<Arc<dyn LlmEngine>> {
    emit(
        "error",
        error_payload(&HostError::new("forbidden", "宿主未授予模型", false), None),
    );
    if standalone_is_local(cfg) {
        log::warn!("Agent24 没授予模型：这一轮用本机边车回答（回环，等价 local_only）");
        None
    } else {
        log::warn!("Agent24 没授予模型、独立推理又不是本机的：这一轮只转写、不回答");
        Some(Arc::new(HostLlm::new(link, cfg)))
    }
}

// ---------------------------------------------------------------- 模型回调

/// `complexity`：simple 本地优先，complex 允许的层里优先强的（宿主定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    Simple,
    Complex,
}

/// `_a24/model/complete` 的请求。
///
/// ⚠️ 宿主那边 `deny_unknown_fields`：**不能带 privacy / model / tools**——
/// 隐私档只由已注册的 manifest 定，模块不能在请求里自己抬（ME4-S2）。
/// [`ModelRequest::to_params`] 只产出允许的键，有用例钉住。
#[derive(Debug, Clone)]
pub struct ModelRequest {
    /// `(role, content)`。
    pub messages: Vec<(String, String)>,
    pub complexity: Complexity,
    pub request_id: String,
    /// 1..=4096，宿主默认 1024。
    pub max_tokens: Option<u32>,
}

impl ModelRequest {
    pub fn to_params(&self) -> Value {
        let mut v = json!({
            "messages": self.messages.iter()
                .map(|(r, c)| json!({"role": r, "content": c}))
                .collect::<Vec<_>>(),
            "complexity": match self.complexity {
                Complexity::Simple => "simple",
                Complexity::Complex => "complex",
            },
            "request_id": self.request_id,
        });
        if let Some(n) = self.max_tokens {
            v["max_tokens"] = json!(n.clamp(1, 4096));
        }
        v
    }
}

/// 回答来自哪一层：`local` = 没出这台电脑，`remote` = 出去了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
pub struct ModelReply {
    pub text: String,
    pub model_id: String,
    pub tier: Tier,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// 解析 `_a24/model/complete` 的 result。字段缺失 / tier 不认识一律报错——
/// 尤其 tier：**认不出是本地还是远端时不能当本地**。
pub fn parse_model_result(v: &Value) -> std::result::Result<ModelReply, HostError> {
    let bad = |what: &str| HostError::new("internal", format!("宿主回包缺 {what}"), false);
    let text = v["text"].as_str().ok_or_else(|| bad("text"))?.to_string();
    let model_id = v["model_id"].as_str().ok_or_else(|| bad("model_id"))?.to_string();
    let tier = match v["tier"].as_str() {
        Some("local") => Tier::Local,
        Some("remote") => Tier::Remote,
        _ => return Err(bad("合法的 tier（local|remote）")),
    };
    Ok(ModelReply {
        text,
        model_id,
        tier,
        prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    })
}

/// 宿主来源的错误种类：**严格等于** Agent24 `agent24_os_proto::rpc::ErrorKind::ALL`
/// 的 wire 字符串（来源 commit 见 contracts/README.md）。原样透传、不加前缀。
pub const HOST_ERROR_KINDS: [&str; 18] = [
    "forbidden",
    "busy",
    "cancelled",
    "timeout",
    "quota_exceeded",
    "invalid_lease",
    "unknown_capability",
    "version_mismatch",
    "auth_failed",
    "manifest_mismatch",
    "not_ready",
    "draining",
    "revoked",
    "rate_limited",
    "payload_too_large",
    "token_invalid",
    "not_found",
    "unavailable",
];

/// AgentEar 自有的错误码（contracts 里后 4 个）。
pub const AGENTEAR_ERROR_CODES: [&str; 4] = ["asr_failed", "tts_unavailable", "privacy_denied", "internal"];

/// 宿主调用失败。`kind` 永远落在 contracts 的 `error.code` 闭集里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub kind: String,
    pub message: String,
    pub retryable: bool,
    /// unavailable 时宿主给的原因（no_provider / request_rejected / …），展示用。
    pub cause: Option<String>,
}

impl HostError {
    /// 不在闭集里的 kind 一律归 `internal`——**闭集是契约**，漏出去一个没人认识的
    /// code 会让宿主的 schema 校验直接拒掉整条 error 事件。
    pub fn new(kind: &str, message: impl Into<String>, retryable: bool) -> Self {
        let kind = if HOST_ERROR_KINDS.contains(&kind) || AGENTEAR_ERROR_CODES.contains(&kind) {
            kind.to_string()
        } else {
            "internal".to_string()
        };
        Self {
            kind,
            message: message.into(),
            retryable,
            cause: None,
        }
    }

    /// 从 JSON-RPC 错误对象解析：`{code: -32000, message, data: {kind, retryable, cause?}}`。
    pub fn from_rpc_error(v: &Value) -> Self {
        let kind = v["data"]["kind"].as_str().unwrap_or("internal");
        let mut e = Self::new(
            kind,
            v["message"].as_str().unwrap_or("宿主返回了错误"),
            v["data"]["retryable"].as_bool().unwrap_or(false),
        );
        e.cause = v["data"]["cause"].as_str().map(str::to_string);
        e
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "宿主错误 {}：{}", self.kind, self.message)?;
        if let Some(c) = &self.cause {
            write!(f, "（{c}）")?;
        }
        Ok(())
    }
}

impl std::error::Error for HostError {}

/// 模型回答的隐私自检：manifest 声明 `local_only` 却拿到 `tier: remote` = 违例。
///
/// 宿主本来就不该这么做（它按 manifest 路由）；这一道是**模块自己的绊线**：
/// 真出现了就不念、记 `privacy_denied`——把一句出过本机的回答当成功播出去，
/// 用户会以为这一轮是本地的。
pub fn check_tier(model_access: &str, tier: Tier) -> std::result::Result<(), HostError> {
    let local_only = model_access.trim() != "remote_allowed";
    if local_only && tier == Tier::Remote {
        return Err(HostError::new(
            "privacy_denied",
            "manifest 声明 local_only，宿主却返回了 tier=remote 的回答；这一轮不念",
            false,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- 宿主链接

/// AgentEar → 宿主的出站调用（同一条 UDS 连接上的 JSON-RPC 请求）。
///
/// 宿主 → AgentEar 的命令也走这条连接（内核发 `_a24/command/invoke` 请求帧），
/// 但**处理与传输无关**：连接读到对端请求帧就交给 [`handle_rpc_request`]，
/// 把返回值作为 result / error 应答回去。所以这里只放出站的两个调用。
pub trait HostLink: Send + Sync {
    fn name(&self) -> &'static str;
    /// 现在是否附着着（连接可用）。
    fn attached(&self) -> bool;
    /// 宿主有没有授予模型能力（A3：握手 offer 里有没有 `_a24/model/`）。
    /// 没授予时**不得回落到任何可能出本机的路径**（A3 §4.3，AgentEar 对齐 2），见 [`llm_engine`]。
    fn grants_models(&self) -> bool {
        true
    }
    /// `_a24/events/emit`，params = [`emit_params`]`(event)`。
    ///
    /// **只由后台投递线程调用**（见 [`emit`]）：同一 session 串行、等到应答再发下一条，
    /// 并且绝不在音频 / 按键路径上阻塞。
    fn emit(&self, event: &Value) -> std::result::Result<(), HostError>;
    /// `_a24/model/complete`（非流式）。`timeout` 是**模块侧**的等待上限
    /// （`talk_timeout_secs`）：到点仍无应答，真实宿主实现要发
    /// `$/cancelRequest {id}`（[`cancel_params`]）并返回 `timeout`。
    fn model_complete(
        &self,
        req: &ModelRequest,
        timeout: Duration,
    ) -> std::result::Result<ModelReply, HostError>;
}

/// `_a24/events/emit` 的 params：`kind` 固定 `agentear.event`，`payload` = 完整 envelope
/// （A3 设计 §7：内核不理解、不校验这份 schema，原样广播）。
pub fn emit_params(event: &Value) -> Value {
    json!({"kind": EVENT_KIND, "payload": event})
}

pub const EVENT_KIND: &str = "agentear.event";

/// `$/cancelRequest` 通知的 params（只能取消自己发出的请求）。
pub fn cancel_params(rpc_id: &str) -> Value {
    json!({"id": rpc_id})
}

/// 独立模式：进程里没有宿主。上报都是空操作，推理直接不可用。
pub struct NoHost;

impl HostLink for NoHost {
    fn name(&self) -> &'static str {
        "none"
    }
    fn attached(&self) -> bool {
        false
    }
    fn emit(&self, _event: &Value) -> std::result::Result<(), HostError> {
        Ok(())
    }
    fn model_complete(
        &self,
        _req: &ModelRequest,
        _timeout: Duration,
    ) -> std::result::Result<ModelReply, HostError> {
        Err(HostError::new(
            "unavailable",
            "没有附着到 Agent24：talk_llm_transport = agent24 这一档现在不可用",
            true,
        ))
    }
}

/// 进程内假宿主：记下收到的每个事件和模型请求，按脚本回答。
///
/// 模型回答的来源有两种：先吃 [`FakeHost::script`] 里预排的结果（成功 / 各种错误），
/// 排空了再用 `responder`（默认回显一句「（假宿主）收到：…」）。
pub struct FakeHost {
    attached: AtomicBool,
    pub events: Mutex<Vec<Value>>,
    /// 每次 emit 在线上的 params 形态（`{kind, payload}`），测 wire 形状用。
    pub emit_wire: Mutex<Vec<Value>>,
    pub requests: Mutex<Vec<Value>>,
    pub script: Mutex<VecDeque<std::result::Result<ModelReply, HostError>>>,
    /// 前 N 次 emit 失败（测重试复用 event_id）。
    pub emit_failures: AtomicU64,
    /// 每次 emit 应答前睡多久（测「投递不阻塞调用方」）。
    pub emit_delay_ms: AtomicU64,
    /// 模型回答前睡多久（测模块侧超时 + 取消）。
    pub model_delay_ms: AtomicU64,
    /// 被模块取消的请求（`$/cancelRequest` 的 request_id）。
    pub cancels: Mutex<Vec<String>>,
    responder: Box<dyn Fn(&ModelRequest) -> ModelReply + Send + Sync>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    pub fn new() -> Self {
        Self {
            attached: AtomicBool::new(true),
            events: Mutex::new(Vec::new()),
            emit_wire: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            script: Mutex::new(VecDeque::new()),
            emit_failures: AtomicU64::new(0),
            emit_delay_ms: AtomicU64::new(0),
            model_delay_ms: AtomicU64::new(0),
            cancels: Mutex::new(Vec::new()),
            responder: Box::new(|req| ModelReply {
                text: format!(
                    "（假宿主）收到：{}",
                    req.messages.last().map(|(_, c)| c.as_str()).unwrap_or("")
                ),
                model_id: "fake-local".into(),
                tier: Tier::Local,
                prompt_tokens: 1,
                completion_tokens: 1,
            }),
        }
    }

    pub fn set_attached(&self, on: bool) {
        self.attached.store(on, Ordering::SeqCst);
    }

    pub fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn events_of(&self, ty: &str) -> Vec<Value> {
        self.events().into_iter().filter(|e| e["type"] == ty).collect()
    }

    pub fn push_reply(&self, r: std::result::Result<ModelReply, HostError>) {
        self.script.lock().unwrap_or_else(|p| p.into_inner()).push_back(r);
    }

    /// 扮演内核：在连接上向模块发一个 `_a24/command/invoke` 请求帧，拿模块的应答。
    pub fn invoke_command(
        &self,
        name: &str,
        body: Value,
        player: &Player,
    ) -> std::result::Result<Value, RpcError> {
        handle_rpc_request(COMMAND_METHOD, &json!({"name": name, "body": body}), player)
    }
}

impl HostLink for FakeHost {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }
    fn emit(&self, event: &Value) -> std::result::Result<(), HostError> {
        if !self.attached() {
            return Err(HostError::new("unavailable", "假宿主已断开", true));
        }
        let delay = self.emit_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        self.emit_wire
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(emit_params(event));
        let left = self.emit_failures.load(Ordering::SeqCst);
        if left > 0 {
            self.emit_failures.store(left - 1, Ordering::SeqCst);
            // 送达失败，但宿主可能已经收到过——所以也记一份，测去重用。
            self.events.lock().unwrap_or_else(|p| p.into_inner()).push(event.clone());
            return Err(HostError::new("busy", "假宿主：这次投递失败", true));
        }
        self.events.lock().unwrap_or_else(|p| p.into_inner()).push(event.clone());
        Ok(())
    }
    fn model_complete(
        &self,
        req: &ModelRequest,
        timeout: Duration,
    ) -> std::result::Result<ModelReply, HostError> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(req.to_params());
        if !self.attached() {
            return Err(HostError::new("unavailable", "假宿主已断开", true));
        }
        // 模拟「宿主迟迟不答」：超过模块侧上限就按真实实现的做法——取消并报 timeout。
        let delay = Duration::from_millis(self.model_delay_ms.load(Ordering::SeqCst));
        if delay > timeout {
            std::thread::sleep(timeout);
            self.cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(req.request_id.clone());
            return Err(HostError::new("timeout", "宿主在模块侧时限内没有回答，已发 $/cancelRequest", true));
        }
        std::thread::sleep(delay);
        if let Some(r) = self.script.lock().unwrap_or_else(|p| p.into_inner()).pop_front() {
            return r;
        }
        Ok((self.responder)(req))
    }
}

// ---------------------------------------------------------------- 经宿主推理

/// 经宿主 `_a24/model/complete` 的对话引擎。**非流式**：宿主的推理回调不做流式
/// （ME4-S2 §0；jason D1 拍板 P2 先上非流式），所以首字起播会比本机流式慢
/// （实测整句路径中位数 4.86s vs 流式 2.80s，docs/benchmarks-talk.md）。
pub struct HostLlm {
    link: Arc<dyn HostLink>,
    model_access: String,
    timeout: Duration,
}

impl HostLlm {
    pub fn new(link: Arc<dyn HostLink>, cfg: &crate::config::Config) -> Self {
        Self {
            link,
            model_access: cfg.agent24_model_access.clone(),
            timeout: Duration::from_secs(cfg.talk_timeout_secs.max(1)),
        }
    }
}

impl LlmEngine for HostLlm {
    fn name(&self) -> &'static str {
        "agent24"
    }

    fn reply(&self, system: &str, user: &str, _lang: TalkLang) -> Result<String> {
        let req = ModelRequest {
            messages: vec![
                ("system".into(), system.into()),
                ("user".into(), user.into()),
            ],
            complexity: Complexity::Simple,
            request_id: new_id("req"),
            // 与本机边车同一个上限：一到两句话，不需要宿主默认的 1024。
            max_tokens: Some(160),
        };
        let reply = self.link.model_complete(&req, self.timeout)?;
        check_tier(&self.model_access, reply.tier)?;
        log::info!(
            "宿主推理：{}（tier {:?}，{}+{} tokens）",
            reply.model_id,
            reply.tier,
            reply.prompt_tokens,
            reply.completion_tokens
        );
        let text = talk::strip_thinking(&reply.text);
        if text.trim().is_empty() {
            return Err(HostError::new("internal", "宿主回了一段空回答", true).into());
        }
        Ok(text)
    }
}

// ---------------------------------------------------------------- 事件

/// 事件与 id 的生成：进程内唯一即可（时间 + 计数器 + pid），不引 uuid 依赖。
pub fn new_id(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{prefix}_{:x}_{:x}_{:x}",
        nanos,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// ASR 报的语种 → BCP-47（B9，已与 agent24-13 约定）。认不出 → `und`。
///
/// SenseVoice 报 `zh/en/yue/ja/ko`，whisper 泰语报 `th`，Qwen3 路径可能给 `?`。
pub fn bcp47(asr_lang: Option<&str>) -> &'static str {
    match asr_lang.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("zh") | Some("zh-cn") => "zh-CN",
        Some("en") | Some("en-us") => "en-US",
        Some("th") | Some("th-th") => "th-TH",
        Some("yue") | Some("zh-hk") => "zh-HK",
        Some("ja") | Some("ja-jp") => "ja-JP",
        Some("ko") | Some("ko-kr") => "ko-KR",
        _ => "und",
    }
}

/// 对话语言 → BCP-47（speak 命令 / 没有 ASR 语种时用）。
pub fn talk_lang_bcp47(lang: TalkLang) -> &'static str {
    match lang {
        TalkLang::Zh => "zh-CN",
        TalkLang::En => "en-US",
        TalkLang::Th => "th-TH",
    }
}

/// 一个事件的 envelope。纯函数，测试直接拿它对 schema 校验。
pub fn envelope(session_id: &str, seq: u64, event_id: &str, ty: &str, payload: Value) -> Value {
    json!({
        "schema": "agentear.event/1",
        "event_id": event_id,
        "session_id": session_id,
        "seq": seq,
        "type": ty,
        "payload": payload,
    })
}

pub fn transcript_payload(text: &str, lang: &str, content_hash: Option<&str>) -> Value {
    let mut p = json!({"text": text, "lang": lang, "final": true});
    // 只接受 64 位小写十六进制：别的东西（比如一个路径）不许借这个字段溜出去。
    if let Some(h) = content_hash.filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())) {
        p["content_hash"] = json!(h);
    }
    p
}

pub fn turn_payload(phase: &str, turn: Option<u64>) -> Value {
    let mut p = json!({"phase": phase});
    if let Some(n) = turn.filter(|n| *n >= 1) {
        p["turn"] = json!(n);
    }
    p
}

pub fn speech_payload(state: &str, command_id: Option<&str>, reason: Option<&str>) -> Value {
    let mut p = json!({"state": state});
    if let Some(c) = command_id {
        p["command_id"] = json!(c);
    }
    // failed 必须带 reason（schema 的 allOf）。
    let reason = reason.or(if state == "failed" { Some("tts_unavailable") } else { None });
    if let Some(r) = reason {
        p["reason"] = json!(r);
    }
    p
}

pub fn error_payload(e: &HostError, command_id: Option<&str>) -> Value {
    let mut p = json!({"code": e.kind, "message": e.message, "retryable": e.retryable});
    if let Some(c) = command_id {
        p["command_id"] = json!(c);
    }
    p
}

pub fn confirm_reply_payload(proposal_event_id: &str, confirm: bool, text: Option<&str>) -> Value {
    let mut p = json!({
        "proposal_event_id": proposal_event_id,
        "reply": if confirm { "confirm" } else { "reject" },
    });
    if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
        p["text"] = json!(t);
    }
    p
}

/// 事件里单个字符串（含键）的上限：宿主 `_a24/events/emit` 要求 ≤8 KiB，超了整条拒收
/// （`payload_too_large`）。A3 设计 §7：**截断并在末尾标注，不拆成多条**。
pub const MAX_WIRE_STRING: usize = 8 * 1024;
pub const TRUNCATED_MARK: &str = "…（已截断）";

/// 按字节上限截断一个字符串（落在字符边界上），并在末尾加标注；没超就原样返回。
pub fn clamp_str(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= MAX_WIRE_STRING {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut cut = MAX_WIRE_STRING - TRUNCATED_MARK.len();
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}{TRUNCATED_MARK}", &s[..cut]))
}

/// 递归截断一个 JSON 值里所有超长的字符串。
pub fn clamp_strings(v: Value) -> Value {
    match v {
        Value::String(s) => match clamp_str(&s) {
            std::borrow::Cow::Borrowed(_) => Value::String(s),
            std::borrow::Cow::Owned(t) => {
                log::warn!("事件里有一段 {} 字节的文字，超过 8 KiB，已截断", s.len());
                Value::String(t)
            }
        },
        Value::Array(a) => Value::Array(a.into_iter().map(clamp_strings).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, clamp_strings(v))).collect()),
        other => other,
    }
}

/// 一次附着（一个 session）的事件发射器：单调 seq、重试复用 event_id。
pub struct Emitter {
    session_id: String,
    seq: Mutex<u64>,
}

/// 同一事件最多投递几次（第一次 + 重试）。**重试复用同一个 envelope**：
/// 宿主按 `(session_id, seq)` 去重，所以重发不会被当成新事件。
pub const EMIT_ATTEMPTS: usize = 3;

impl Emitter {
    pub fn new() -> Self {
        Self {
            session_id: new_id("ses"),
            seq: Mutex::new(0),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 生成下一个 envelope（seq +1）。
    pub fn next(&self, ty: &str, payload: Value) -> Value {
        let mut s = self.seq.lock().unwrap_or_else(|p| p.into_inner());
        *s += 1;
        envelope(&self.session_id, *s, &new_id("evt"), ty, clamp_strings(payload))
    }

    /// 生成并投递。返回 event_id（投递彻底失败也返回——调用方可能要拿它关联，
    /// 比如 proposal 的 event_id 要留给后面的 confirm_reply）。
    pub fn emit(&self, link: &dyn HostLink, ty: &str, payload: Value) -> String {
        let ev = self.next(ty, payload);
        let id = ev["event_id"].as_str().unwrap_or_default().to_string();
        deliver(link, &ev);
        id
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

/// 这类事件被限流（`rate_limited`）时要不要重试（A3 §4.4，AgentEar 对齐 4）：
/// `transcript` / `proposal` / `confirm_reply` 退避重试（复用 event_id 与 seq）；
/// `turn` / `speech` / `error` 等状态事件**丢弃并计数**——宿主容忍 seq 缺口。
pub fn retry_when_rate_limited(ty: &str) -> bool {
    matches!(ty, "transcript" | "proposal" | "confirm_reply")
}

/// 被限流丢掉的状态事件数。
pub static DROPPED_RATE_LIMITED: AtomicU64 = AtomicU64::new(0);

/// 投递失败后要不要再试。
pub fn should_retry_emit(ty: &str, e: &HostError) -> bool {
    if e.kind == "rate_limited" {
        return retry_when_rate_limited(ty);
    }
    e.retryable
}

/// 投递一个 envelope，按规则重试（复用同一个 envelope）。
fn deliver(link: &dyn HostLink, ev: &Value) {
    deliver_until(link, ev, &AtomicBool::new(false));
}

/// 同 [`deliver`]，但每次尝试前看一眼 `cancelled`：作废了就不再发、不再重试。
/// （已经写上线的那一次无法收回——A3 下断开会关连接，那一次随之失败。）
fn deliver_until(link: &dyn HostLink, ev: &Value, cancelled: &AtomicBool) {
    let ty = ev["type"].as_str().unwrap_or_default();
    for attempt in 1..=EMIT_ATTEMPTS {
        if cancelled.load(Ordering::SeqCst) {
            log::debug!("附着已作废，{ty} 不再投递 / 重试");
            return;
        }
        match link.emit(ev) {
            Ok(()) => return,
            Err(e) if should_retry_emit(ty, &e) && attempt < EMIT_ATTEMPTS => {
                log::debug!("事件投递失败（第 {attempt} 次，重试）：{e}");
                // 限流时退避长一点：宿主令牌桶 5/s。
                let base = if e.kind == "rate_limited" { 300 } else { 50 };
                std::thread::sleep(Duration::from_millis(base * attempt as u64));
            }
            Err(e) if e.kind == "rate_limited" => {
                DROPPED_RATE_LIMITED.fetch_add(1, Ordering::Relaxed);
                log::info!("状态事件被限流，丢弃（{ty}）");
                return;
            }
            Err(e) => {
                log::warn!("事件投递失败，放弃（{}）：{e}", ev["type"]);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------- 附着状态机

/// AgentEar 与宿主的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachState {
    /// 没有宿主（或断开后已回独立模式）：照独立版的规矩工作。
    Standalone,
    /// 附着着：推理经宿主、向宿主报告、确认归宿主。
    Attached,
    /// 断开了且**不回独立模式**：停听并提示，等用户处理或宿主回来。
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachFallback {
    AutoLocal,
    Stop,
}

impl AttachFallback {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "stop" => Self::Stop,
            // 不认识的按默认，默认本身是保守的（非本机路径照样停听）。
            _ => Self::AutoLocal,
        }
    }
}

/// 断开之后去哪（B5，jason 2026-09-27 拍板）。纯函数，真值表钉住。
///
/// | fallback | 独立模式推理是本机的 | 结果 |
/// |---|---|---|
/// | auto_local | 是 | Standalone（自动回独立模式） |
/// | auto_local | 否 | Disconnected（停听提示） |
/// | stop | 任意 | Disconnected |
pub fn after_disconnect(fallback: AttachFallback, standalone_local: bool) -> AttachState {
    match (fallback, standalone_local) {
        (AttachFallback::AutoLocal, true) => AttachState::Standalone,
        _ => AttachState::Disconnected,
    }
}

/// 这个状态下按录音键能不能开始录。
pub fn listening_allowed(state: AttachState) -> bool {
    !matches!(state, AttachState::Disconnected)
}

/// 用户对一条待确认 proposal 的答复 → confirm_reply 的 `reply`。
///
/// - 短按录音键（`None`，没说话）= confirm；
/// - 说了话：沿用独立版的判词（否定先判、肯定只认短答复）；
///   **其余的话一律 reject**——作废，不偷偷执行（contracts 里写明了这一条）。
pub fn confirm_reply_for(text: Option<&str>) -> bool {
    match text {
        None => true,
        Some(t) => crate::commands::classify_confirmation(t) == crate::commands::Reply::Confirm,
    }
}

// ---------------------------------------------------------------- 全局状态

static LINK: RwLock<Option<Arc<dyn HostLink>>> = RwLock::new(None);
static STATE: Mutex<AttachState> = Mutex::new(AttachState::Standalone);

/// 当前附着的发射器 + 它的投递队列。
///
/// ⚠️ **投递在后台线程里串行做**：A3 设计要求同一 session 的 emit 等到应答再发下一条，
/// 而发事件的地方（按键确认、指令轮次）在 worker 线程上——同步投递会让一次慢应答
/// 卡住读按键的循环，打断就失灵了（2026-09-20 那个真 bug 的同类）。
struct Outbox {
    emitter: Arc<Emitter>,
    tx: std::sync::mpsc::Sender<Value>,
    /// 这一代附着已作废（detach / 换新附着）。投递线程看到它就**不再投递、不再重试**，
    /// 把缓冲里剩下的事件清掉。（PR #96 评审：原来只丢 `tx`，已入队的事件照样送到旧宿主。）
    cancelled: Arc<AtomicBool>,
}

impl Outbox {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}
static EMITTER: Mutex<Option<Outbox>> = Mutex::new(None);
/// 已入队、还没投递完的事件数（`flush` 用）。
static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

fn start_outbox(link: Arc<dyn HostLink>) -> Outbox {
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    std::thread::Builder::new()
        .name("host-emit".into())
        .spawn(move || {
            // 发送端被丢掉（断开 / 换了新附着）时 recv 返回 Err，线程自然退出。
            // 作废之后缓冲里剩下的每一条，`deliver_until` 第一次尝试前就会看到标志而放弃。
            while let Ok(ev) = rx.recv() {
                deliver_until(link.as_ref(), &ev, &flag);
                IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .expect("起不了事件投递线程");
    Outbox {
        emitter: Arc::new(Emitter::new()),
        tx,
        cancelled,
    }
}

/// 等投递队列清空（测试与 CLI 打印前用）。返回是否在时限内清空。
pub fn flush(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while IN_FLIGHT.load(Ordering::SeqCst) > 0 {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// 等待宿主答复的那条 proposal（附着模式下取代独立版的本地待确认）。
struct Awaiting {
    event_id: String,
    deadline: Instant,
}
static AWAITING: Mutex<Option<Awaiting>> = Mutex::new(None);

/// 附着到一个宿主：换 session、状态进 Attached。
pub fn attach(link: Arc<dyn HostLink>) {
    log::info!("附着到宿主：{}", link.name());
    // 换新附着：旧那一代的队列作废，**新连接的事件绝不被旧队列带走**，旧事件也不再发往旧宿主。
    if let Some(old) = EMITTER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .replace(start_outbox(link.clone()))
    {
        old.cancel();
    }
    *LINK.write().unwrap_or_else(|p| p.into_inner()) = Some(link);
    *STATE.lock().unwrap_or_else(|p| p.into_inner()) = AttachState::Attached;
    crate::tray::set_host_disconnected(false);
    *AWAITING.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// 与宿主断开。返回断开后的状态（按 B5）。
///
/// 断开时：挂着的 proposal 作废（宿主都没了，没人能确认）；宿主排的播报全部作废
/// （它们是宿主的意图，宿主不在了就不该继续念）。
pub fn detach(cfg: &crate::config::Config) -> AttachState {
    *LINK.write().unwrap_or_else(|p| p.into_inner()) = None;
    // 作废已入队未投递的事件（PR #96 评审）：只丢 `tx` 不够，缓冲里的照样会被投递到旧宿主。
    if let Some(old) = EMITTER.lock().unwrap_or_else(|p| p.into_inner()).take() {
        old.cancel();
    }
    *AWAITING.lock().unwrap_or_else(|p| p.into_inner()) = None;
    center().lock().unwrap_or_else(|p| p.into_inner()).reset();
    let next = after_disconnect(
        AttachFallback::parse(&cfg.attach_fallback),
        standalone_is_local(cfg),
    );
    *STATE.lock().unwrap_or_else(|p| p.into_inner()) = next;
    crate::tray::set_host_disconnected(next == AttachState::Disconnected);
    match next {
        AttachState::Standalone => log::info!("与宿主断开 → 回到独立模式（推理在本机）"),
        _ => log::warn!(
            "与宿主断开 → 停听（独立模式的推理不是本机的，或设置为 stop）。\
             改 attach_fallback 或 talk_llm_transport，或等宿主回来"
        ),
    }
    next
}

/// 用户在停听状态下手动恢复独立模式（菜单 / 设置里）。
pub fn resume_standalone() {
    *STATE.lock().unwrap_or_else(|p| p.into_inner()) = AttachState::Standalone;
    crate::tray::set_host_disconnected(false);
}

pub fn state() -> AttachState {
    *STATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// 附着着（且连接可用）。
pub fn attached() -> bool {
    state() == AttachState::Attached && link().attached()
}

pub fn link() -> Arc<dyn HostLink> {
    LINK.read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_else(|| Arc::new(NoHost))
}

/// 向宿主发一个事件。**没附着时什么都不做**（返回 None）——独立版因此零开销、零行为变化。
///
/// 立即返回 event_id（事件入队即生成）；真正的投递在后台线程里串行完成。
pub fn emit(ty: &str, payload: Value) -> Option<String> {
    if !attached() {
        return None;
    }
    let slot = EMITTER.lock().unwrap_or_else(|p| p.into_inner());
    let outbox = slot.as_ref()?;
    let ev = outbox.emitter.next(ty, payload);
    let id = ev["event_id"].as_str().unwrap_or_default().to_string();
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    if outbox.tx.send(ev).is_err() {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        log::warn!("事件投递线程已经退出，{ty} 没发出去");
    }
    Some(id)
}

/// 记一条等宿主答复的 proposal（只留一条，新的顶掉旧的——与独立版同一个理由：
/// 挂两条时用户说「确认」我们不知道他确认的是哪条）。
pub fn await_confirm(proposal_event_id: String, ttl: Duration) {
    let mut slot = AWAITING.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(old) = slot.replace(Awaiting {
        event_id: proposal_event_id,
        deadline: Instant::now() + ttl,
    }) {
        log::info!("新的待确认顶掉了旧的（{}）", old.event_id);
    }
}

/// 取走还没过期的那条待确认（过期的顺手丢掉）。
pub fn take_awaiting() -> Option<String> {
    let mut slot = AWAITING.lock().unwrap_or_else(|p| p.into_inner());
    match slot.take() {
        Some(a) if Instant::now() <= a.deadline => Some(a.event_id),
        Some(a) => {
            log::info!("待确认已过期作废（{}）", a.event_id);
            None
        }
        None => None,
    }
}

pub fn has_awaiting() -> bool {
    AWAITING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|a| Instant::now() <= a.deadline)
}

/// 停听状态下按录音键时念的一句话。
pub fn disconnected_notice(lang: TalkLang) -> &'static str {
    match lang {
        TalkLang::Zh => "和 Agent24 的连接断了，现在不听。请在菜单里恢复独立模式，或者等 Agent24 回来。",
        TalkLang::En => "I've lost the connection to Agent24, so I'm not listening. Resume standalone mode from the menu, or wait for Agent24.",
        TalkLang::Th => "การเชื่อมต่อกับ Agent24 ขาดหาย ตอนนี้ไม่ได้ฟัง กรุณากลับสู่โหมดใช้งานเดี่ยวจากเมนู หรือรอ Agent24",
    }
}

static LAST_DISCONNECT_NOTICE: Mutex<Option<Instant>> = Mutex::new(None);

/// 停听状态下按了录音键：念一句（节流，与边车不可用提示同一个窗口），
/// **别让用户以为按了没反应**。播放放后台线程，不挡 worker 读按键。
pub fn notice_disconnected(lang: TalkLang) {
    {
        let mut last = LAST_DISCONNECT_NOTICE.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let prev = last.map(|at| ("disconnected", at));
        if !talk::should_notice(prev, "disconnected", now, talk::UNAVAILABLE_NOTICE_WINDOW) {
            log::info!("停听中（与宿主断开）——提示过了，这次不念");
            return;
        }
        *last = Some(now);
    }
    log::warn!("停听中（与宿主断开）：用 say 念一句提示");
    std::thread::spawn(move || {
        let spoken = talk::SayTts::new(15)
            .synthesize(disconnected_notice(lang), lang)
            .and_then(|wav| talk::play_blocking(&wav));
        if let Err(e) = spoken {
            log::error!("连 say 的提示都念不出来：{e:#}");
        }
    });
}

/// 附着模式下给宿主看的路径：**不暴露用户名**（A3 设计要求）。
/// 在家目录下 → `~/…`；不在家目录下 → 只给文件名。
pub fn tilde_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        if let Some(rest) = path.strip_prefix(&home) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 命令

/// `agentear.command/1` 的两种命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCommand {
    Speak(SpeakCmd),
    Stop {
        command_id: String,
        session_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakCmd {
    pub command_id: String,
    pub text: String,
    /// BCP-47：zh-CN / en-US / th-TH。
    pub lang: String,
    pub voice: Option<String>,
    pub style: Option<String>,
}

impl HostCommand {
    pub fn command_id(&self) -> &str {
        match self {
            Self::Speak(s) => &s.command_id,
            Self::Stop { command_id, .. } => command_id,
        }
    }
}

/// 解析一条命令。校验与 contracts/schema/agentear.command.v1 一致（必填、长度、枚举）。
pub fn parse_command(v: &Value) -> std::result::Result<HostCommand, String> {
    if v["schema"] != "agentear.command/1" {
        return Err(format!("不认识的命令版本：{}", v["schema"]));
    }
    let id_ok = |s: &str| !s.is_empty() && s.len() <= 128;
    let command_id = v["command_id"]
        .as_str()
        .filter(|s| id_ok(s))
        .ok_or("command_id 缺失或长度不对")?
        .to_string();
    let p = &v["payload"];
    if !p.is_object() {
        return Err("payload 必须是对象".into());
    }
    match v["type"].as_str() {
        Some("speak") => {
            let allowed = ["text", "lang", "voice", "style"];
            if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| !allowed.contains(&k.as_str()))) {
                return Err(format!("speak 不认识的字段：{k}"));
            }
            let text = p["text"].as_str().ok_or("speak 缺 text")?;
            if text.is_empty() || text.chars().count() > 2000 {
                return Err("speak.text 长度必须在 1..=2000".into());
            }
            let lang = p["lang"].as_str().ok_or("speak 缺 lang")?;
            if !["zh-CN", "en-US", "th-TH"].contains(&lang) {
                return Err(format!("speak.lang 只认 zh-CN / en-US / th-TH，收到 {lang}"));
            }
            let opt = |k: &str| -> std::result::Result<Option<String>, String> {
                match &p[k] {
                    Value::Null => Ok(None),
                    Value::String(s) if !s.is_empty() => Ok(Some(s.clone())),
                    _ => Err(format!("speak.{k} 必须是非空字符串")),
                }
            };
            Ok(HostCommand::Speak(SpeakCmd {
                command_id,
                text: text.to_string(),
                lang: lang.to_string(),
                voice: opt("voice")?,
                style: opt("style")?,
            }))
        }
        Some("stop_playback") => {
            if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| k.as_str() != "session_id")) {
                return Err(format!("stop_playback 不认识的字段：{k}"));
            }
            let session_id = match &p["session_id"] {
                Value::Null => None,
                Value::String(s) if id_ok(s) => Some(s.clone()),
                _ => return Err("stop_playback.session_id 长度不对".into()),
            };
            Ok(HostCommand::Stop {
                command_id,
                session_id,
            })
        }
        other => Err(format!("不认识的命令类型：{other:?}")),
    }
}

/// 命令处理后要做的事。**纯数据**：[`CommandCenter`] 只决定「做什么」，
/// 真正的播放 / 掐断 / 发事件由调用方执行——这样排队与幂等的规则能脱离声卡测试。
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 发一个事件（type, payload）。
    Emit(&'static str, Value),
    /// 现在开始念这一条。
    Play(SpeakCmd),
    /// 掐掉正在放的声音。
    StopPlayback,
}

/// 命令的回执（宿主那边同步拿到的响应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    /// speak：立刻开念。
    Started,
    /// speak：排在后面。
    Queued,
    /// stop：执行了（有没有东西在放都算成功）。
    Stopped,
    /// 重复的 command_id：原样返回第一次的回执，**不再执行一次**。
    Duplicate(Box<Ack>),
}

impl Ack {
    pub fn as_json(&self, command_id: &str) -> Value {
        match self {
            Ack::Duplicate(first) => {
                let mut v = first.as_json(command_id);
                v["duplicate"] = json!(true);
                v
            }
            Ack::Started => json!({"command_id": command_id, "status": "started"}),
            Ack::Queued => json!({"command_id": command_id, "status": "queued"}),
            Ack::Stopped => json!({"command_id": command_id, "status": "stopped"}),
        }
    }
}

/// 宿主命令的排队与幂等（B7，已与 agent24-13 约定）：
///
/// - `speak` 排在**当前轮回答**和**正在放的宿主播报**之后；
/// - `stop_playback` 与用户按键打断都**清空队列**；
/// - 同一 command_id 只执行一次；没东西在放时 stop 也成功。
#[derive(Default)]
pub struct CommandCenter {
    seen: HashMap<String, Ack>,
    /// 按到达顺序记下 id，超上限时淘汰最老的（防止长期运行无界增长）。
    seen_order: VecDeque<String>,
    queue: VecDeque<SpeakCmd>,
    /// 正在放的宿主播报。
    playing: Option<SpeakCmd>,
    /// 当前轮回答正在进行（想 / 说）。
    turn_active: bool,
}

/// 记住多少个 command_id 用于去重。宿主重试是秒级的事，几百个足够。
pub const SEEN_CAP: usize = 512;

impl CommandCenter {
    pub fn new() -> Self {
        Self::default()
    }

    fn remember(&mut self, id: &str, ack: Ack) {
        if self.seen.insert(id.to_string(), ack).is_none() {
            self.seen_order.push_back(id.to_string());
            while self.seen_order.len() > SEEN_CAP {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn playing(&self) -> Option<&SpeakCmd> {
        self.playing.as_ref()
    }

    /// 处理一条命令。
    pub fn handle(&mut self, cmd: HostCommand) -> (Ack, Vec<Effect>) {
        if let Some(first) = self.seen.get(cmd.command_id()) {
            return (Ack::Duplicate(Box::new(first.clone())), Vec::new());
        }
        let id = cmd.command_id().to_string();
        let (ack, fx) = match cmd {
            HostCommand::Speak(s) => {
                if self.playing.is_none() && !self.turn_active {
                    self.playing = Some(s.clone());
                    (
                        Ack::Started,
                        vec![
                            Effect::Emit("speech", speech_payload("started", Some(&s.command_id), None)),
                            Effect::Play(s),
                        ],
                    )
                } else {
                    self.queue.push_back(s);
                    (Ack::Queued, Vec::new())
                }
            }
            HostCommand::Stop { .. } => {
                // 当前 P1 只有一个会话，session_id 过滤到 P2 多会话时才有意义；
                // 缺省 = 停所有，与 schema 描述一致。
                let mut fx = self.clear("stop_command");
                if self.playing.is_some() || self.turn_active {
                    fx.push(Effect::StopPlayback);
                }
                if let Some(p) = self.playing.take() {
                    fx.push(Effect::Emit(
                        "speech",
                        speech_payload("stopped", Some(&p.command_id), Some("stop_command")),
                    ));
                }
                (Ack::Stopped, fx)
            }
        };
        self.remember(&id, ack.clone());
        (ack, fx)
    }

    /// 清空排队，每条都报 `stopped`（带原因）。
    fn clear(&mut self, reason: &str) -> Vec<Effect> {
        self.queue
            .drain(..)
            .map(|s| {
                Effect::Emit(
                    "speech",
                    speech_payload("stopped", Some(&s.command_id), Some(reason)),
                )
            })
            .collect()
    }

    /// 用户按了录音键（打断）：清队 + 正在放的宿主播报报 stopped(barge_in)。
    /// 真正的掐声音由调用方（`talk::stop_playback`）已经做了。
    pub fn on_barge_in(&mut self) -> Vec<Effect> {
        let mut fx = self.clear("barge_in");
        if let Some(p) = self.playing.take() {
            fx.push(Effect::Emit(
                "speech",
                speech_payload("stopped", Some(&p.command_id), Some("barge_in")),
            ));
        }
        fx
    }

    /// 一条宿主播报放完（或失败 / 被掐）。报状态，然后放队里的下一条。
    pub fn on_played(&mut self, command_id: &str, outcome: PlayOutcome) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.playing.as_ref().map(|p| p.command_id.as_str()) == Some(command_id) {
            self.playing = None;
            let (state, reason) = match outcome {
                PlayOutcome::Completed => ("completed", None),
                PlayOutcome::Interrupted => ("stopped", Some("barge_in")),
                PlayOutcome::Failed => ("failed", Some("tts_unavailable")),
            };
            fx.push(Effect::Emit("speech", speech_payload(state, Some(command_id), reason)));
        }
        fx.extend(self.next_if_idle());
        fx
    }

    /// 当前轮回答开始 / 结束（`answer_out_loud` 调）。结束时放队里的下一条。
    pub fn set_turn_active(&mut self, on: bool) -> Vec<Effect> {
        self.turn_active = on;
        if on {
            Vec::new()
        } else {
            self.next_if_idle()
        }
    }

    fn next_if_idle(&mut self) -> Vec<Effect> {
        if self.playing.is_some() || self.turn_active {
            return Vec::new();
        }
        match self.queue.pop_front() {
            Some(s) => {
                self.playing = Some(s.clone());
                vec![
                    Effect::Emit("speech", speech_payload("started", Some(&s.command_id), None)),
                    Effect::Play(s),
                ]
            }
            None => Vec::new(),
        }
    }

    /// 断开宿主时：排队与「正在放」一并作废（不发事件——宿主已经不在了）。
    pub fn reset(&mut self) {
        self.queue.clear();
        self.playing = None;
        self.turn_active = false;
    }
}

/// 一次宿主播报的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayOutcome {
    Completed,
    Interrupted,
    Failed,
}

static CENTER: std::sync::OnceLock<Mutex<CommandCenter>> = std::sync::OnceLock::new();

pub fn center() -> &'static Mutex<CommandCenter> {
    CENTER.get_or_init(|| Mutex::new(CommandCenter::new()))
}

/// 播放器：给定一条 speak，合成并播放，返回结果。默认用 `talk` 的引擎；
/// 测试 / fake 宿主可以换成不出声的实现。
pub type Player = Arc<dyn Fn(&SpeakCmd) -> PlayOutcome + Send + Sync>;

/// 默认播放器：按 speak 的 lang/voice/style 合成，走 `talk::play_blocking`（可被按键掐断）。
pub fn default_player() -> Player {
    Arc::new(|s: &SpeakCmd| {
        let mut cfg = crate::config::get();
        if let Some(v) = &s.voice {
            cfg.tts_voice = Some(v.clone());
        }
        if let Some(st) = &s.style {
            cfg.tts_style = st.clone();
        }
        let lang = match s.lang.as_str() {
            "en-US" => TalkLang::En,
            "th-TH" => TalkLang::Th,
            _ => TalkLang::Zh,
        };
        let engines = talk::Engines::from_config(&cfg);
        let before = talk::interrupts();
        match talk::speak(&engines, &s.text, lang) {
            Ok(_) if talk::interrupts() != before => PlayOutcome::Interrupted,
            Ok(_) => PlayOutcome::Completed,
            Err(e) => {
                log::warn!("宿主播报合成/播放失败：{e:#}");
                PlayOutcome::Failed
            }
        }
    })
}

/// 执行一批 [`Effect`]：发事件、掐声音；`Play` 在后台线程里放，放完回到
/// [`CommandCenter::on_played`] 取下一条。
pub fn run_effects(fx: Vec<Effect>, player: &Player) {
    for f in fx {
        match f {
            Effect::Emit(ty, payload) => {
                emit(ty, payload);
            }
            Effect::StopPlayback => {
                talk::stop_playback();
            }
            Effect::Play(s) => {
                let player = player.clone();
                std::thread::spawn(move || {
                    let outcome = player(&s);
                    let next = center()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .on_played(&s.command_id, outcome);
                    run_effects(next, &player);
                });
            }
        }
    }
}

/// 内核 → 模块的反向命令方法（A3 设计 §6）。
pub const COMMAND_METHOD: &str = "_a24/command/invoke";

/// JSON-RPC 错误（模块作为被调用方应答内核时用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

pub const RPC_METHOD_NOT_FOUND: i64 = -32601;
pub const RPC_INVALID_PARAMS: i64 = -32602;

/// 连接上收到**内核发来的请求帧**时调：`(method, params)` → result 或 error。
///
/// - 只认 `_a24/command/invoke`，其它方法 → `-32601`；
/// - params 必须恰好是 `{name, body}`；`body` 是完整的 `agentear.command/1`，
///   **`name` 必须等于 `body.type`**；任何一条不满足 → `-32602`；
/// - 合法 → 立即回 `{"accepted": true}`（**只表示已入队**，播放结果经 speech / error
///   事件回报）；同一 `command_id` 重复送达 → 回首次的结果，不再执行。
pub fn handle_rpc_request(
    method: &str,
    params: &Value,
    player: &Player,
) -> std::result::Result<Value, RpcError> {
    if method != COMMAND_METHOD {
        return Err(RpcError {
            code: RPC_METHOD_NOT_FOUND,
            message: format!("AgentEar 不认识方法 {method}"),
        });
    }
    let invalid = |m: String| RpcError {
        code: RPC_INVALID_PARAMS,
        message: m,
    };
    let obj = params.as_object().ok_or_else(|| invalid("params 必须是对象".into()))?;
    if let Some(k) = obj.keys().find(|k| *k != "name" && *k != "body") {
        return Err(invalid(format!("params 不认识的字段：{k}")));
    }
    let name = params["name"].as_str().ok_or_else(|| invalid("缺 name".into()))?;
    let body = &params["body"];
    if body["type"].as_str() != Some(name) {
        return Err(invalid(format!("name（{name}）与 body.type（{}）不一致", body["type"])));
    }
    let resp = handle_inbound(body, player);
    if let Some(err) = resp.get("error") {
        return Err(invalid(err["message"].as_str().unwrap_or("命令不合法").to_string()));
    }
    Ok(json!({"accepted": true}))
}

/// **与传输无关的入站命令入口**：请求体（一条 `agentear.command/1`）→ 响应体。
///
/// P2 的 A3 入站 HTTP 服务端（UDS）收到请求就调它；fake 宿主测试直接调它。
/// 解析失败返回 `{"error": ...}`（传输层据此回 4xx），并不改变任何状态。
pub fn handle_inbound(body: &Value, player: &Player) -> Value {
    let cmd = match parse_command(body) {
        Ok(c) => c,
        Err(msg) => return json!({"error": {"code": "invalid_params", "message": msg}}),
    };
    let id = cmd.command_id().to_string();
    let (ack, fx) = center().lock().unwrap_or_else(|p| p.into_inner()).handle(cmd);
    run_effects(fx, player);
    ack.as_json(&id)
}

/// 用户按录音键打断时调（worker 的 Begin 分支）。独立模式下队列恒空，零开销。
pub fn on_barge_in() {
    let fx = center().lock().unwrap_or_else(|p| p.into_inner()).on_barge_in();
    if !fx.is_empty() {
        run_effects(fx, &default_player());
    }
}

/// 当前轮回答开始 / 结束。结束时若有排队的宿主播报就开始放。
pub fn set_turn_active(on: bool) {
    let fx = center().lock().unwrap_or_else(|p| p.into_inner()).set_turn_active(on);
    if !fx.is_empty() {
        run_effects(fx, &default_player());
    }
}

/// 碰全局状态（附着 / 待确认 / 命令中心）的测试要串行，否则互相踩。
/// 宿主模块自己的测试和 `main.rs` 里的接线测试共用这一把。
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GLOBAL: Mutex<()> = Mutex::new(());
    let g = GLOBAL.lock().unwrap_or_else(|p| p.into_inner());
    *LINK.write().unwrap_or_else(|p| p.into_inner()) = None;
    if let Some(old) = EMITTER.lock().unwrap_or_else(|p| p.into_inner()).take() {
        old.cancel();
    }
    *STATE.lock().unwrap_or_else(|p| p.into_inner()) = AttachState::Standalone;
    *AWAITING.lock().unwrap_or_else(|p| p.into_inner()) = None;
    *center().lock().unwrap_or_else(|p| p.into_inner()) = CommandCenter::new();
    g
}

#[cfg(test)]
mod tests;
