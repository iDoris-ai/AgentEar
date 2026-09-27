//! Agent24 A3 附着客户端（T6.1.2 / P2）。
//!
//! **唯一依据是 Agent24 的 `docs/design/A3-ATTACHED-MODULE.md`（v2，PR #524 @68c2412）
//! §4 / §5.6 / §6**——这里按文档独立实现，**不依赖 Agent24 的任何 crate**
//! （两仓库只通过协议配合，jason 2026-09-27；agent24-13 同意 B4）。
//!
//! 形态：一条 Unix 域流 socket（注册时拿到的 `socket_path`），上面跑 NDJSON 帧的
//! JSON-RPC 2.0，**两个方向共用**：
//!
//! - 模块 → 内核：`initialize`（握手）、`_a24/events/emit`、`_a24/model/complete`、
//!   `$/cancelRequest`（通知）；
//! - 内核 → 模块：`_a24/command/invoke`（反向命令），交给 [`crate::host::handle_rpc_request`]。
//!
//! 实现用 std `UnixStream` + 线程，**不引 tokio**：一个读线程分拣帧，写方向一把锁按整行写。
//!
//! ⚠️ **token 绝不进日志**：[`Creds`] 的 `Debug` 打码；错误信息里只出现 `token_id`。

use crate::host::{self, HostError, HostLink, ModelReply, ModelRequest};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 帧负载上限（不含 `\n`），与内核 `frame.rs` `MAX_FRAME_BYTES` 相同（§4.2）。
pub const MAX_FRAME_BYTES: usize = 1 << 20;
/// JSON-RPC `id` 上限（§4.2，`rpc.rs` `MAX_ID_BYTES`）。
pub const MAX_ID_BYTES: usize = 256;
/// 我们支持的协议版本区间。
pub const PROTOCOL_MIN: u64 = 1;
pub const PROTOCOL_MAX: u64 = 1;
/// 握手总时限（§4.3：内核从 accept 起 5 s）。模块侧多给一点余量。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(6);
/// 单次 `emit` 等应答的上限。
pub const EMIT_TIMEOUT: Duration = Duration::from_secs(10);
/// 模块名（manifest 的 `name`）。
pub const MODULE: &str = "agentear";
/// 握手里声明的能力（仅提示；实际授予看 offer）。
pub const CAPABILITIES: [&str; 2] = ["events", "models"];
/// offer 里的能力前缀。
pub const PROVIDES_EVENTS: &str = "_a24/events/";
pub const PROVIDES_MODEL: &str = "_a24/model/";

// ---------------------------------------------------------------- manifest

/// 附着 manifest 的**原始字节**（`assets/agent24/domain-os.yml`）。
/// digest 按这些字节算，所以注册时交给 CLI 的文件必须就是它（[`crate::a3_pair`] 原样落盘）。
pub const MANIFEST: &[u8] = include_bytes!("../assets/agent24/domain-os.yml");

/// `sha256:<小写 hex>`（§4.3）。
pub fn manifest_digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(7 + 64);
    s.push_str("sha256:");
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn manifest_digest() -> String {
    manifest_digest_of(MANIFEST)
}

// ---------------------------------------------------------------- 帧

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// 一行超过 [`MAX_FRAME_BYTES`]：按规格断开连接。
    TooLong,
    Io(String),
}

/// 读一帧（一行，不含 `\n`）。`Ok(None)` = 对端关了（**最后一行没有 `\n` 不算帧**，§4.2）。
///
/// 超长时**不会把整行读进内存**：读到上限就停，返回 [`FrameError::TooLong`]。
pub fn read_frame<R: BufRead>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut buf = Vec::new();
    loop {
        let (done, used) = {
            let avail = match r.fill_buf() {
                Ok(a) => a,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(FrameError::Io(e.to_string())),
            };
            if avail.is_empty() {
                return Ok(None); // EOF；不完整的最后一行丢弃
            }
            match avail.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    buf.extend_from_slice(&avail[..i]);
                    (true, i + 1)
                }
                None => {
                    buf.extend_from_slice(avail);
                    (false, avail.len())
                }
            }
        };
        r.consume(used);
        if buf.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLong);
        }
        if done {
            return Ok(Some(buf));
        }
    }
}

/// 编码一帧：JSON 对象 + `\n`。超长返回错误（不发出去比发出去被对端断开好）。
pub fn encode_frame(v: &Value) -> Result<Vec<u8>, FrameError> {
    let mut bytes = serde_json::to_vec(v).map_err(|e| FrameError::Io(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLong);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// 分拣后的一帧（§4.2：**有 `method` = 请求/通知；无 `method` = 响应**）。
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// 对端请求（有 `method` 且有字符串 `id`）——要应答。
    Request { id: String, method: String, params: Value },
    /// 通知（有 `method` 无 `id`）——**一律忽略**（§4.5：为将来内核加通知留余地）。
    Notification { method: String },
    /// 对我们某个请求的响应。`Ok(result)` / `Err(error 对象)`。
    Response { id: String, outcome: Result<Value, Value> },
    /// 其它一切（非对象、id 非字符串、形状不对）——丢弃并记日志，**不回帧**。
    Stray(String),
}

pub fn classify(bytes: &[u8]) -> Frame {
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => return Frame::Stray(format!("不是 JSON：{e}")),
    };
    let Some(obj) = v.as_object() else {
        return Frame::Stray("不是 JSON 对象".into());
    };
    if let Some(method) = obj.get("method") {
        let method = method.as_str().unwrap_or_default().to_string();
        return match obj.get("id") {
            None => Frame::Notification { method },
            Some(Value::String(id)) if id.len() > MAX_ID_BYTES => Frame::Stray("请求 id 超过 256 字节".into()),
            Some(Value::String(id)) => Frame::Request {
                id: id.clone(),
                method,
                params: obj.get("params").cloned().unwrap_or_else(|| json!({})),
            },
            Some(_) => Frame::Stray("请求的 id 不是字符串".into()),
        };
    }
    let Some(Value::String(id)) = obj.get("id") else {
        return Frame::Stray("响应没有字符串 id".into());
    };
    if id.len() > MAX_ID_BYTES {
        return Frame::Stray("响应 id 超过 256 字节".into());
    }
    match (obj.get("result"), obj.get("error")) {
        (Some(r), None) => Frame::Response { id: id.clone(), outcome: Ok(r.clone()) },
        (None, Some(e)) if e.is_object() => Frame::Response { id: id.clone(), outcome: Err(e.clone()) },
        _ => Frame::Stray(format!("响应形状不对（id {id}）")),
    }
}

fn request_frame(id: &str, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn notification_frame(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn result_frame(id: &str, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_frame(id: &str, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

// ---------------------------------------------------------------- 握手

/// `initialize` 的 params（§4.3）。**A3 没有专有字段**，与 A1 完全同形。
pub fn initialize_params(digest: &str, token: &str) -> Value {
    json!({
        "protocol_versions": {"min": PROTOCOL_MIN, "max": PROTOCOL_MAX},
        "module": MODULE,
        "manifest_digest": digest,
        "auth_token": token,
        "capabilities": CAPABILITIES,
    })
}

/// 握手拿到的授权：`provides` 是前缀列表，未列出即未授权（§4.3）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    pub provides: Vec<String>,
}

impl Offer {
    pub fn has(&self, prefix: &str) -> bool {
        self.provides.iter().any(|p| p == prefix)
    }
}

/// 握手失败之后怎么办——**§5.6 的完整处置表**，纯函数真值表钉住。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// 退避重连（连不上、EOF、busy、超时……）。
    Retry,
    /// `manifest_mismatch`：若本地 digest 与上次注册的不同就自动轮换一次，否则停。
    Rotate,
    /// 停止重连，设置里提示（原因见 [`StopReason`]）。
    Stop(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `auth_failed`：凭据问题，提示重新配对。
    Repair,
    /// `version_mismatch`：提示升级 AgentEar 或 Agent24。
    Upgrade,
    /// `forbidden`：已在 Agent24 停用。
    Disabled,
    /// `-32700/-32600/-32602`：实现缺陷。
    Protocol,
    /// 放宽隐私需要去 Agent24 侧确认（`relax_requires_confirmation`）。
    ConfirmOnHost,
}

/// 把握手错误对象（JSON-RPC `error`）映射到处置。
pub fn disposition_for(error: &Value) -> Disposition {
    let code = error["code"].as_i64().unwrap_or(0);
    match code {
        -32700 | -32600 | -32602 => Disposition::Stop(StopReason::Protocol),
        -32000 => match error["data"]["kind"].as_str().unwrap_or("") {
            "auth_failed" => Disposition::Stop(StopReason::Repair),
            "manifest_mismatch" => Disposition::Rotate,
            "version_mismatch" => Disposition::Stop(StopReason::Upgrade),
            "forbidden" => Disposition::Stop(StopReason::Disabled),
            // busy（另一实例在线）与其余一切：退避重连。
            _ => Disposition::Retry,
        },
        _ => Disposition::Retry,
    }
}

/// 解析握手应答。`Ok(offer)` 或 `Err(disposition)`。
pub fn parse_initialize_reply(frame: &Frame) -> Result<Offer, (Disposition, String)> {
    match frame {
        Frame::Response { id, outcome: Ok(result) } if id == "1" => {
            // 宽松解析：忽略未知字段（§4.3）。
            let pv = result["protocol_version"].as_u64();
            if !pv.is_some_and(|v| (PROTOCOL_MIN..=PROTOCOL_MAX).contains(&v)) {
                return Err((
                    Disposition::Stop(StopReason::Upgrade),
                    format!("内核给的协议版本 {pv:?} 不在我们的区间 {PROTOCOL_MIN}..={PROTOCOL_MAX}"),
                ));
            }
            let provides = result["offer"]["provides"]
                .as_array()
                .map(|a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Ok(Offer { provides })
        }
        Frame::Response { outcome: Err(e), .. } => Err((
            disposition_for(e),
            format!(
                "握手被拒：{} {}",
                e["data"]["kind"].as_str().unwrap_or(""),
                e["message"].as_str().unwrap_or("")
            ),
        )),
        other => Err((Disposition::Stop(StopReason::Protocol), format!("握手应答不对：{other:?}"))),
    }
}

// ---------------------------------------------------------------- 连接

/// 连接用的凭据。**`Debug` 打码**——它会出现在日志里的结构体附近。
#[derive(Clone)]
pub struct Creds {
    pub socket: PathBuf,
    pub token: String,
    pub digest: String,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Creds")
            .field("socket", &self.socket)
            .field("token", &"<redacted>")
            .field("digest", &self.digest)
            .finish()
    }
}

/// 内核→模块请求的处理函数（生产里就是 `host::handle_rpc_request`）。
pub type Handler = Arc<dyn Fn(&str, &Value) -> Result<Value, (i64, String)> + Send + Sync>;

type Pending = Mutex<HashMap<String, mpsc::Sender<Result<Value, Value>>>>;

/// 一条已握手的附着连接。
pub struct Conn {
    writer: Mutex<UnixStream>,
    pending: Arc<Pending>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    closed: Arc<(Mutex<bool>, Condvar)>,
    pub offer: Offer,
    /// 丢弃的「无主」响应帧数（迟到、未知 id）。
    pub stray: Arc<AtomicU64>,
}

impl Conn {
    /// 连上并握手。失败返回 §5.6 的处置。
    pub fn connect(creds: &Creds, handler: Handler) -> Result<Arc<Self>, (Disposition, String)> {
        let stream = UnixStream::connect(&creds.socket).map_err(|e| {
            (Disposition::Retry, format!("连不上 Agent24（{}）：{e}", creds.socket.display()))
        })?;
        let io_err = |e: std::io::Error| (Disposition::Retry, format!("握手 I/O：{e}"));
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).map_err(io_err)?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT)).map_err(io_err)?;
        let mut w = stream.try_clone().map_err(io_err)?;
        let init = request_frame("1", "initialize", initialize_params(&creds.digest, &creds.token));
        let bytes = encode_frame(&init).map_err(|e| (Disposition::Retry, format!("{e:?}")))?;
        w.write_all(&bytes).map_err(io_err)?;
        let mut reader = BufReader::new(stream.try_clone().map_err(io_err)?);
        let reply = match read_frame(&mut reader) {
            Ok(Some(b)) => classify(&b),
            Ok(None) => return Err((Disposition::Retry, "握手时对端关了连接".into())),
            Err(FrameError::TooLong) => {
                return Err((Disposition::Stop(StopReason::Protocol), "握手应答超长".into()))
            }
            Err(FrameError::Io(e)) => return Err((Disposition::Retry, format!("握手读失败：{e}"))),
        };
        let offer = parse_initialize_reply(&reply)?;
        // 握手后读不设超时（长连接），写保留超时，免得对端不读时把我们卡死。
        stream.set_read_timeout(None).map_err(io_err)?;
        stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(io_err)?;

        let conn = Arc::new(Self {
            writer: Mutex::new(w),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
            closed: Arc::new((Mutex::new(false), Condvar::new())),
            offer,
            stray: Arc::new(AtomicU64::new(0)),
        });
        let c2 = conn.clone();
        std::thread::Builder::new()
            .name("a3-read".into())
            .spawn(move || c2.read_loop(reader, handler))
            .map_err(|e| (Disposition::Retry, format!("起不了读线程：{e}")))?;
        Ok(conn)
    }

    fn read_loop(self: Arc<Self>, mut reader: BufReader<UnixStream>, handler: Handler) {
        loop {
            let bytes = match read_frame(&mut reader) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    log::info!("Agent24 连接关闭（EOF）");
                    break;
                }
                Err(e) => {
                    log::warn!("Agent24 连接读失败：{e:?}");
                    break;
                }
            };
            match classify(&bytes) {
                Frame::Request { id, method, params } => {
                    let reply = match handler(&method, &params) {
                        // §6.1：result 必须是对象，否则宿主回 502 malformed。
                        Ok(r) if r.is_object() => result_frame(&id, r),
                        Ok(_) => error_frame(&id, -32603, "internal: result 不是对象"),
                        Err((code, msg)) => error_frame(&id, code, &msg),
                    };
                    if let Err(e) = self.write(&reply) {
                        log::warn!("应答内核请求失败：{e}");
                    }
                }
                Frame::Notification { method } => {
                    log::debug!("忽略内核通知 {method}");
                }
                Frame::Response { id, outcome } => {
                    let tx = self.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                    match tx {
                        Some(tx) => {
                            let _ = tx.send(outcome);
                        }
                        None => {
                            self.stray.fetch_add(1, Ordering::Relaxed);
                            log::debug!("丢弃无主响应（id {id}）");
                        }
                    }
                }
                Frame::Stray(why) => {
                    self.stray.fetch_add(1, Ordering::Relaxed);
                    log::debug!("丢弃无法识别的帧：{why}");
                }
            }
        }
        self.mark_closed();
    }

    fn mark_closed(&self) {
        self.alive.store(false, Ordering::SeqCst);
        // 在途调用立刻失败，不等超时。
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
        let (m, cv) = &*self.closed;
        *m.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }

    fn write(&self, v: &Value) -> Result<(), String> {
        let bytes = encode_frame(v).map_err(|e| format!("{e:?}"))?;
        let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        w.write_all(&bytes).map_err(|e| e.to_string())
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// 阻塞到连接结束。
    pub fn wait_closed(&self) {
        let (m, cv) = &*self.closed;
        let mut done = m.lock().unwrap_or_else(|p| p.into_inner());
        while !*done {
            done = cv.wait(done).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// 主动断开（撤销配对、退出时）。
    pub fn close(&self) {
        self.mark_closed();
    }

    /// 发一个请求并等应答。
    ///
    /// 超时：发 `$/cancelRequest`（§4.4），**立即**返回 `timeout`；之后那一帧（取消错误或
    /// 抢先完成的结果）是该调用的终态，到了就被读线程按 id 收走丢掉。
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, HostError> {
        if !self.alive() {
            return Err(lost());
        }
        let id = format!("m{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), tx);
        if let Err(e) = self.write(&request_frame(&id, method, params)) {
            self.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
            log::warn!("发 {method} 失败：{e}");
            return Err(lost());
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(err)) => Err(HostError::from_rpc_error(&err)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 等待者走了，但 pending 里留着 id：终态帧到了由读线程收走，不算无主。
                let _ = self.write(&notification_frame("$/cancelRequest", host::cancel_params(&id)));
                Err(HostError::new("timeout", format!("{method} 超过 {timeout:?} 没有应答，已取消"), true))
            }
            // 发送端被丢：连接断了（mark_closed 清空了 pending）。
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(lost()),
        }
    }
}

fn lost() -> HostError {
    // AgentEar 自己的传输断连：wire 里没有这个值，用自有 code `internal`（P0 约定）。
    HostError::new("internal", "与 Agent24 的连接已断开", true)
}

// ---------------------------------------------------------------- HostLink

/// 真实宿主：一条 A3 附着连接。
pub struct A3Host {
    pub conn: Arc<Conn>,
}

impl HostLink for A3Host {
    fn name(&self) -> &'static str {
        "agent24-a3"
    }

    fn attached(&self) -> bool {
        self.conn.alive()
    }

    fn grants_models(&self) -> bool {
        self.conn.offer.has(PROVIDES_MODEL)
    }

    fn emit(&self, event: &Value) -> Result<(), HostError> {
        if !self.conn.offer.has(PROVIDES_EVENTS) {
            return Err(HostError::new("forbidden", "宿主未授予 events 能力", false));
        }
        self.conn
            .call("_a24/events/emit", host::emit_params(event), EMIT_TIMEOUT)
            .map(|_| ())
    }

    fn model_complete(&self, req: &ModelRequest, timeout: Duration) -> Result<ModelReply, HostError> {
        if !self.grants_models() {
            return Err(HostError::new("forbidden", "宿主未授予模型", false));
        }
        let mut params = req.to_params();
        strip_nulls(&mut params);
        // §4.4：A3 下 request_id 无意义，**必须省略**（带了反而得 timeout）。
        if let Some(o) = params.as_object_mut() {
            o.remove("request_id");
        }
        let v = self.conn.call("_a24/model/complete", params, timeout)?;
        host::parse_model_result(&v)
    }
}

/// 生产用的命令处理：交给 P1 的 `handle_rpc_request`。
pub fn default_handler() -> Handler {
    Arc::new(|method, params| {
        let player = host::default_player();
        host::handle_rpc_request(method, params, &player)
            .map_err(|e| (e.code, e.message))
    })
}

// ---------------------------------------------------------------- 状态（给设置窗口 / 菜单）

/// 设置里显示的连接状态（T6.1.2 交付 4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    Unpaired,
    Connecting,
    Connected,
    /// 断开，已回独立模式（B5：独立推理是本机的）。
    DisconnectedStandalone,
    /// 断开，停听（B5）。
    DisconnectedStopped,
    /// 停止重连，需要人处理。
    NeedsAction(StopReason),
}

static STATUS: Mutex<LinkStatus> = Mutex::new(LinkStatus::Unpaired);

pub fn status() -> LinkStatus {
    STATUS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub fn set_status(s: LinkStatus) {
    log::info!("Agent24 连接状态：{s:?}");
    *STATUS.lock().unwrap_or_else(|p| p.into_inner()) = s;
}

// ---------------------------------------------------------------- 守护：连接 + 退避重连

/// 退避（§5.6）：1 s 起、翻倍、上限 30 s。
pub fn next_backoff(cur: Duration) -> Duration {
    (cur * 2).min(Duration::from_secs(30))
}
pub const BACKOFF_START: Duration = Duration::from_secs(1);

/// 唤醒守护线程（配对完成、撤销、手动重连之后）。
static WAKE: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());
static CURRENT: Mutex<Option<Arc<Conn>>> = Mutex::new(None);
static SUPERVISOR: AtomicBool = AtomicBool::new(false);

pub fn wake() {
    let (m, cv) = &WAKE;
    *m.lock().unwrap_or_else(|p| p.into_inner()) += 1;
    cv.notify_all();
}

/// 睡到超时或被唤醒。返回是否被唤醒。
fn sleep_or_wake(d: Duration) -> bool {
    let (m, cv) = &WAKE;
    let g = m.lock().unwrap_or_else(|p| p.into_inner());
    let start = *g;
    let deadline = Instant::now() + d;
    let mut g = g;
    while *g == start {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        g = cv.wait_timeout(g, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
    }
    true
}

/// 主动断开当前连接（撤销配对时）。守护线程随后按「没配对」处理。
pub fn disconnect_now() {
    if let Some(c) = CURRENT.lock().unwrap_or_else(|p| p.into_inner()).take() {
        c.close();
    }
    wake();
}

/// 守护线程要的依赖：凭据从哪来、manifest_mismatch 时怎么轮换。
pub struct Supervisor {
    /// 读当前凭据（配置 + `<数据目录>/agent24/token`）。`None` = 没配对。
    pub creds: Box<dyn Fn() -> Option<Creds> + Send>,
    /// 自动轮换：`Ok(())` = 已拿到新 token（下一轮读 creds 就是新的）。
    pub rotate: Box<dyn Fn() -> Result<(), StopReason> + Send>,
    pub handler: Handler,
}

/// 起守护线程（进程里只起一次）。
pub fn start_supervisor(sup: Supervisor) {
    if SUPERVISOR.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("a3-supervisor".into())
        .spawn(move || supervise(sup))
        .expect("起不了 Agent24 连接守护线程");
}

fn supervise(sup: Supervisor) {
    let mut backoff = BACKOFF_START;
    let mut rotated_once = false;
    loop {
        let Some(creds) = (sup.creds)() else {
            set_status(LinkStatus::Unpaired);
            sleep_or_wake(Duration::from_secs(3600));
            continue;
        };
        if matches!(status(), LinkStatus::NeedsAction(_)) {
            // 停止重连：等人处理（重新配对 / 手动重连会 wake）。
            if !sleep_or_wake(Duration::from_secs(3600)) {
                continue;
            }
            set_status(LinkStatus::Connecting);
            continue;
        }
        set_status(LinkStatus::Connecting);
        match Conn::connect(&creds, sup.handler.clone()) {
            Ok(conn) => {
                backoff = BACKOFF_START;
                rotated_once = false;
                if !conn.offer.has(PROVIDES_MODEL) {
                    log::warn!("Agent24 没授予模型能力（offer 里没有 {PROVIDES_MODEL}）");
                }
                *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(conn.clone());
                host::attach(Arc::new(A3Host { conn: conn.clone() }));
                set_status(LinkStatus::Connected);
                conn.wait_closed();
                CURRENT.lock().unwrap_or_else(|p| p.into_inner()).take();
                let next = host::detach(&crate::config::get());
                set_status(if next == host::AttachState::Standalone {
                    LinkStatus::DisconnectedStandalone
                } else {
                    LinkStatus::DisconnectedStopped
                });
                // 被撤销配对（creds 没了）就不用退避，直接下一轮。
                if (sup.creds)().is_none() {
                    continue;
                }
            }
            Err((disp, why)) => {
                log::warn!("连 Agent24 失败：{why}");
                match disp {
                    Disposition::Retry => set_status(if host::state() == host::AttachState::Disconnected {
                        LinkStatus::DisconnectedStopped
                    } else {
                        LinkStatus::DisconnectedStandalone
                    }),
                    Disposition::Rotate if !rotated_once => {
                        rotated_once = true;
                        match (sup.rotate)() {
                            Ok(()) => {
                                log::info!("manifest 变了，已自动重新注册，马上重连");
                                continue;
                            }
                            Err(r) => set_status(LinkStatus::NeedsAction(r)),
                        }
                    }
                    Disposition::Rotate => set_status(LinkStatus::NeedsAction(StopReason::Repair)),
                    Disposition::Stop(r) => set_status(LinkStatus::NeedsAction(r)),
                }
            }
        }
        if matches!(status(), LinkStatus::NeedsAction(_)) {
            continue;
        }
        if sleep_or_wake(backoff) {
            backoff = BACKOFF_START;
        } else {
            backoff = next_backoff(backoff);
        }
    }
}

/// CLI 用：一次性连接（不起守护、不重连）。
pub fn connect_once(creds: &Creds) -> anyhow::Result<Arc<Conn>> {
    Conn::connect(creds, default_handler()).map_err(|(d, why)| anyhow::anyhow!("{why}（处置 {d:?}）"))
}

/// 读 token 文件（测试用覆盖参数 `--a3-token-file`）：去掉首尾空白。
pub fn read_token_file(p: &Path) -> anyhow::Result<String> {
    let mut s = String::new();
    std::fs::File::open(p)?.read_to_string(&mut s)?;
    let t = s.trim().to_string();
    anyhow::ensure!(!t.is_empty(), "token 文件是空的：{}", p.display());
    Ok(t)
}

/// 把 JSON 对象里的可选字段为 `null` 的去掉（§4.2：可选字段不发 null）。
///
/// 递归进对象的值**和数组的元素**（数组里的对象同样要去掉 null 字段）。
/// 但**数组元素本身是 null 时保留**：那是一个值，不是「省略的可选字段」，
/// 删掉会改变数组长度与下标语义。
pub fn strip_nulls(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.retain(|_, v| !v.is_null());
            for (_, v) in o.iter_mut() {
                strip_nulls(v);
            }
        }
        Value::Array(a) => {
            for v in a.iter_mut() {
                strip_nulls(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
