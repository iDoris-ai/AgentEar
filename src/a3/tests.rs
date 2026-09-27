//! A3 客户端测试：一个**只按 A3 设计文档 §4 / §5.6 / §6 写成**的假内核
//! （UnixListener + 线程，**不 import 任何 Agent24 代码**），逐条验客户端行为。

use super::*;
use crate::host;
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::AtomicUsize;

const TOKEN: &str = "tok-secret-for-tests";

fn sock_path(tag: &str) -> PathBuf {
    // UDS 路径上限 104 字节：放短目录。
    let p = PathBuf::from(format!("/tmp/a3t-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn creds(p: &Path) -> Creds {
    Creds {
        socket: p.to_path_buf(),
        token: TOKEN.into(),
        digest: manifest_digest(),
    }
}

/// 假内核这一侧的一条连接。
struct K {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl K {
    fn recv(&mut self) -> Value {
        let b = read_frame(&mut self.r).expect("读帧").expect("对端关了");
        serde_json::from_slice(&b).expect("模块发来的不是 JSON")
    }
    /// 读到对端关闭为止（模块侧 close 之后），中间的帧丢弃。
    fn wait_eof(&mut self) {
        while let Ok(Some(_)) = read_frame(&mut self.r) {}
    }
    fn send(&mut self, v: Value) {
        let mut b = serde_json::to_vec(&v).unwrap();
        b.push(b'\n');
        self.w.write_all(&b).unwrap();
    }
    /// 严格校验 initialize（§4.3：deny_unknown_fields、字段齐全、id "1"）。
    fn expect_initialize(&mut self) -> Value {
        let v = self.recv();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["id"], "1", "握手 id 固定为字符串 \"1\"");
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        for k in &keys {
            assert!(["jsonrpc", "id", "method", "params"].contains(&k.as_str()), "请求只许四个成员，多了 {k}");
        }
        let p = v["params"].as_object().unwrap();
        let mut pk: Vec<&str> = p.keys().map(|s| s.as_str()).collect();
        pk.sort();
        assert_eq!(
            pk,
            ["auth_token", "capabilities", "manifest_digest", "module", "protocol_versions"],
            "A3 没有专有字段（§4.3）"
        );
        assert_eq!(p["protocol_versions"], json!({"min": 1, "max": 1}));
        assert_eq!(p["module"], "agentear");
        assert_eq!(p["manifest_digest"], manifest_digest());
        v
    }
    fn accept_ok(&mut self, provides: &[&str]) {
        let v = self.expect_initialize();
        assert_eq!(v["params"]["auth_token"], TOKEN);
        self.send(json!({"jsonrpc":"2.0","id":"1","result":{"protocol_version":1,"offer":{"provides":provides},"future_field":1}}));
    }
    fn reject(&mut self, code: i64, kind: Option<&str>) {
        self.expect_initialize();
        let mut e = json!({"code": code, "message": "no"});
        if let Some(k) = kind {
            e["data"] = json!({"kind": k});
        }
        self.send(json!({"jsonrpc":"2.0","id":"1","error":e}));
    }
}

/// 起一个假内核：每来一条连接就交给 `script` 处理（在独立线程里）。
///
/// ⚠️ 返回的 [`Kernel`] **必须 `.join()`**：假内核那一侧的断言在别的线程里，
/// 不 join 的话它 panic 了测试照样绿（变异测试实测抓到过——3 个变异因此存活）。
fn kernel(path: &Path, conns: usize, script: impl Fn(usize, K) + Send + Sync + 'static) -> Kernel {
    let l = UnixListener::bind(path).unwrap();
    let script = Arc::new(script);
    Kernel(std::thread::spawn(move || {
        let mut inner = Vec::new();
        for i in 0..conns {
            let (s, _) = l.accept().unwrap();
            let k = K { r: BufReader::new(s.try_clone().unwrap()), w: s };
            let sc = script.clone();
            inner.push(std::thread::spawn(move || sc(i, k)));
        }
        for h in inner {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    }))
}

struct Kernel(std::thread::JoinHandle<()>);

impl Kernel {
    /// 等假内核跑完；它那一侧任何断言失败都在这里让测试失败。
    fn join(self) {
        if let Err(e) = self.0.join() {
            std::panic::resume_unwind(e);
        }
    }
}

/// 测试用的命令处理：真 `handle_rpc_request` + 不出声的播放器。
fn quiet_handler() -> Handler {
    Arc::new(|method, params| {
        let player: host::Player = Arc::new(|_s: &host::SpeakCmd| host::PlayOutcome::Completed);
        host::handle_rpc_request(method, params, &player).map_err(|e| (e.code, e.message))
    })
}

// ---------------------------------------------------------------- 帧

#[test]
fn frame_limit_is_one_mib_and_partial_last_line_is_not_a_frame() {
    let ok = vec![b'a'; MAX_FRAME_BYTES];
    let mut data = ok.clone();
    data.push(b'\n');
    data.extend_from_slice(&vec![b'b'; MAX_FRAME_BYTES + 1]);
    data.push(b'\n');
    data.extend_from_slice(b"{\"partial\":1}"); // 没有 \n
    let mut r = BufReader::new(&data[..]);
    assert_eq!(read_frame(&mut r).unwrap().unwrap().len(), MAX_FRAME_BYTES, "恰好 1 MiB 是合法帧");
    assert_eq!(read_frame(&mut r), Err(FrameError::TooLong));
    let mut r2 = BufReader::new(&b"{\"a\":1}\n{\"partial\":1}"[..]);
    assert!(read_frame(&mut r2).unwrap().is_some());
    assert_eq!(read_frame(&mut r2), Ok(None), "最后一行没有 \\n 不算帧");
    let big = json!({"x": "y".repeat(MAX_FRAME_BYTES)});
    assert_eq!(encode_frame(&big), Err(FrameError::TooLong), "发出去前就拒掉超长帧");
}

#[test]
fn classify_follows_method_presence() {
    assert!(matches!(classify(br#"{"jsonrpc":"2.0","id":"k1","method":"_a24/command/invoke","params":{}}"#), Frame::Request { .. }));
    assert!(matches!(classify(br#"{"jsonrpc":"2.0","method":"$/future"}"#), Frame::Notification { .. }));
    assert!(matches!(classify(br#"{"jsonrpc":"2.0","id":"m1","result":{}}"#), Frame::Response { outcome: Ok(_), .. }));
    assert!(matches!(classify(br#"{"jsonrpc":"2.0","id":"m1","error":{"code":-32000,"message":"x"}}"#), Frame::Response { outcome: Err(_), .. }));
    assert!(matches!(classify(br#"{"jsonrpc":"2.0","id":7,"result":{}}"#), Frame::Stray(_)), "非字符串 id");
    assert!(matches!(classify(br#"[1,2]"#), Frame::Stray(_)));
    let long = format!(r#"{{"jsonrpc":"2.0","id":"{}","method":"x"}}"#, "i".repeat(MAX_ID_BYTES + 1));
    assert!(matches!(classify(long.as_bytes()), Frame::Stray(_)), "id 超过 256 字节");
    assert!(matches!(classify(br#"{"id":"m1","result":{},"error":{}}"#), Frame::Stray(_)));
}

// ---------------------------------------------------------------- 握手与处置表

/// §5.6 处置表的真值表。
#[test]
fn disposition_table_matches_spec() {
    let e = |code: i64, kind: &str| json!({"code": code, "message": "x", "data": {"kind": kind}});
    assert_eq!(disposition_for(&e(-32000, "auth_failed")), Disposition::Stop(StopReason::Repair));
    assert_eq!(disposition_for(&e(-32000, "manifest_mismatch")), Disposition::Rotate);
    assert_eq!(disposition_for(&e(-32000, "version_mismatch")), Disposition::Stop(StopReason::Upgrade));
    assert_eq!(disposition_for(&e(-32000, "forbidden")), Disposition::Stop(StopReason::Disabled));
    assert_eq!(disposition_for(&e(-32000, "busy")), Disposition::Retry);
    assert_eq!(disposition_for(&e(-32000, "unavailable")), Disposition::Retry);
    for c in [-32700, -32600, -32602] {
        assert_eq!(disposition_for(&json!({"code": c, "message": "x"})), Disposition::Stop(StopReason::Protocol));
    }
}

#[test]
fn handshake_success_parses_offer_leniently() {
    let p = sock_path("ok");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        k.wait_eof();
    });
    let c = Conn::connect(&creds(&p), quiet_handler()).expect("握手应成功");
    assert!(c.offer.has(PROVIDES_EVENTS) && c.offer.has(PROVIDES_MODEL));
    c.close();
    t.join();
}

#[test]
fn handshake_rejections_map_to_dispositions_over_a_real_socket() {
    for (i, (code, kind, want)) in [
        (-32000, Some("auth_failed"), Disposition::Stop(StopReason::Repair)),
        (-32000, Some("manifest_mismatch"), Disposition::Rotate),
        (-32000, Some("version_mismatch"), Disposition::Stop(StopReason::Upgrade)),
        (-32000, Some("forbidden"), Disposition::Stop(StopReason::Disabled)),
        (-32000, Some("busy"), Disposition::Retry),
        (-32602, None, Disposition::Stop(StopReason::Protocol)),
    ]
    .into_iter()
    .enumerate()
    {
        let p = sock_path(&format!("rej{i}"));
        let t = kernel(&p, 1, move |_, mut k| k.reject(code, kind));
        let err = Conn::connect(&creds(&p), quiet_handler()).err().expect("应被拒");
        assert_eq!(err.0, want, "{kind:?}");
        assert!(!err.1.contains(TOKEN), "错误信息里不许有 token：{}", err.1);
        t.join();
    }
}

#[test]
fn connect_to_missing_socket_is_retry() {
    let p = sock_path("absent");
    let err = Conn::connect(&creds(&p), quiet_handler()).err().unwrap();
    assert_eq!(err.0, Disposition::Retry, "daemon 不在 = 宿主离线，退避重连");
}

#[test]
fn protocol_version_outside_our_range_stops() {
    let p = sock_path("ver");
    let t = kernel(&p, 1, |_, mut k| {
        k.expect_initialize();
        k.send(json!({"jsonrpc":"2.0","id":"1","result":{"protocol_version":2,"offer":{"provides":[]}}}));
    });
    let err = Conn::connect(&creds(&p), quiet_handler()).err().unwrap();
    assert_eq!(err.0, Disposition::Stop(StopReason::Upgrade));
    t.join();
}

// ---------------------------------------------------------------- 模型

#[test]
fn model_complete_omits_request_id_and_parses_result() {
    let p = sock_path("model");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        let v = k.recv();
        assert_eq!(v["method"], "_a24/model/complete");
        assert!(v["params"].get("request_id").is_none(), "A3 下 request_id 必须省略（§4.4）");
        assert!(v["params"].get("privacy").is_none() && v["params"].get("model").is_none());
        k.send(json!({"jsonrpc":"2.0","id":v["id"],"result":{"text":"你好","model_id":"m","tier":"local","usage":{"prompt_tokens":3,"completion_tokens":2}}}));
        let v2 = k.recv();
        k.send(json!({"jsonrpc":"2.0","id":v2["id"],"error":{"code":-32000,"message":"none","data":{"kind":"unavailable","retryable":true,"cause":"no_provider"}}}));
        k.wait_eof();
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    let h = A3Host { conn: conn.clone() };
    let req = ModelRequest {
        messages: vec![("user".into(), "hi".into())],
        complexity: host::Complexity::Simple,
        request_id: "req-x".into(),
        max_tokens: Some(16),
    };
    let r = h.model_complete(&req, Duration::from_secs(5)).unwrap();
    assert_eq!(r.text, "你好");
    let e = h.model_complete(&req, Duration::from_secs(5)).unwrap_err();
    assert_eq!(e.kind, "unavailable");
    assert_eq!(e.cause.as_deref(), Some("no_provider"));
    conn.close();
    t.join();
}

#[test]
fn model_timeout_sends_cancel_and_the_terminal_frame_is_absorbed() {
    let p = sock_path("cancel");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        let v = k.recv();
        let id = v["id"].clone();
        let c = k.recv();
        assert_eq!(c["method"], "$/cancelRequest");
        assert!(c.get("id").is_none(), "取消是通知，不带 id");
        assert_eq!(c["params"]["id"], id, "只取消自己发出的那个 id");
        // 取消后的终态帧：cancelled（§4.4）——必须被客户端按 id 收走，不算无主。
        k.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"cancelled by $/cancelRequest","data":{"kind":"cancelled"}}}));
        k.wait_eof();
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    let h = A3Host { conn: conn.clone() };
    let req = ModelRequest {
        messages: vec![("user".into(), "slow".into())],
        complexity: host::Complexity::Simple,
        request_id: "r".into(),
        max_tokens: None,
    };
    let e = h.model_complete(&req, Duration::from_millis(300)).unwrap_err();
    assert_eq!(e.kind, "timeout");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(conn.stray.load(Ordering::Relaxed), 0, "取消后的终态帧不是无主响应");
    conn.close();
    t.join();
}

#[test]
fn no_model_offer_means_forbidden_and_zero_frames() {
    let p = sock_path("nomodel");
    let frames = Arc::new(AtomicUsize::new(0));
    let f2 = frames.clone();
    let t = kernel(&p, 1, move |_, mut k| {
        k.accept_ok(&["_a24/events/"]);
        // 数一下握手之后还收到几帧（应为 0，直到对端关）
        while read_frame(&mut k.r).ok().flatten().is_some() {
            f2.fetch_add(1, Ordering::SeqCst);
        }
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    let h = A3Host { conn: conn.clone() };
    assert!(!h.grants_models());
    let req = ModelRequest {
        messages: vec![("user".into(), "x".into())],
        complexity: host::Complexity::Simple,
        request_id: "r".into(),
        max_tokens: None,
    };
    let e = h.model_complete(&req, Duration::from_secs(1)).unwrap_err();
    assert_eq!(e.kind, "forbidden");
    conn.close();
    t.join();
    assert_eq!(frames.load(Ordering::SeqCst), 0, "没授予模型就一帧都不该发");
}

/// 没授予模型时 [`host::llm_engine`] 的选择（A3 §4.3，对齐 2）。
#[test]
fn no_model_grant_uses_local_sidecar_only_if_standalone_is_local() {
    let _g = host::test_guard();
    let p = sock_path("nmg");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/"]);
        // 应答 emit（那条 error{forbidden}）
        while let Ok(Some(b)) = read_frame(&mut k.r) {
            let v: Value = serde_json::from_slice(&b).unwrap();
            if v["method"] == "_a24/events/emit" {
                assert_eq!(v["params"]["payload"]["type"], "error");
                assert_eq!(v["params"]["payload"]["payload"]["code"], "forbidden");
                k.send(json!({"jsonrpc":"2.0","id":v["id"],"result":{}}));
            }
        }
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    host::attach(Arc::new(A3Host { conn: conn.clone() }));
    // 本机边车 → None（走本机边车）
    let mut cfg = crate::config::Config {
        talk_llm_transport: "sidecar".into(),
        ..Default::default()
    };
    assert!(host::llm_engine(&cfg).is_none(), "本机边车 = 视同 local_only，可以用");
    // 独立档是 iDoris（可能出本机）→ 只转写：给一个必失败的宿主引擎
    cfg.talk_llm_transport = "idoris".into();
    let eng = host::llm_engine(&cfg).expect("应返回宿主引擎");
    assert_eq!(eng.name(), "agent24");
    assert!(eng.reply("s", "u", crate::talk::TalkLang::Zh).is_err(), "绝不改发可能出本机的路径");
    assert!(host::flush(Duration::from_secs(3)));
    conn.close();
    host::detach(&cfg);
    t.join();
}

// ---------------------------------------------------------------- 事件

#[test]
fn emits_are_serial_with_wire_shape_and_rate_limit_rules() {
    let _g = host::test_guard();
    let p = sock_path("emit");
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let s2 = seen.clone();
    let t = kernel(&p, 1, move |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        let mut n = 0;
        while let Ok(Some(b)) = read_frame(&mut k.r) {
            let v: Value = serde_json::from_slice(&b).unwrap();
            if v["method"] != "_a24/events/emit" {
                continue;
            }
            n += 1;
            let params = v["params"].as_object().unwrap();
            let mut keys: Vec<&str> = params.keys().map(|s| s.as_str()).collect();
            keys.sort();
            assert_eq!(keys, ["kind", "payload"], "emit 只有 kind 与 payload（request_id 省略）");
            assert_eq!(v["params"]["kind"], "agentear.event");
            s2.lock().unwrap().push(v["params"]["payload"].clone());
            // 串行（§7.1）：**应答之前模块不许再发下一条**。缓冲里没有残余，且 200 ms 内读不到新帧。
            assert!(k.r.buffer().is_empty(), "应答前缓冲里已经有下一帧——emit 没有串行");
            k.r.get_ref().set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            match read_frame(&mut k.r) {
                Err(FrameError::Io(_)) => {} // 超时：好
                other => panic!("应答前又收到一帧（{other:?}）——emit 没有串行"),
            }
            k.r.get_ref().set_read_timeout(None).unwrap();
            // 第 1 条（turn）与第 2 条（transcript 首发）限流；transcript 重试放行。
            let reply = if n <= 2 {
                json!({"jsonrpc":"2.0","id":v["id"],"error":{"code":-32000,"message":"slow down","data":{"kind":"rate_limited","retryable":true}}})
            } else {
                json!({"jsonrpc":"2.0","id":v["id"],"result":{}})
            };
            std::thread::sleep(Duration::from_millis(30));
            k.send(reply);
        }
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    host::attach(Arc::new(A3Host { conn: conn.clone() }));
    host::emit("turn", host::turn_payload("thinking", Some(1)));
    host::emit("transcript", host::transcript_payload("你好", "zh-CN", None));
    assert!(host::flush(Duration::from_secs(5)));
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 3, "turn 被丢（不重试）、transcript 重试一次：{got:?}");
    assert_eq!(got[0]["type"], "turn");
    assert_eq!(got[1]["type"], "transcript");
    assert_eq!(got[1], got[2], "transcript 重试必须复用同一 event_id 与 seq");
    assert!(got[1]["seq"].as_u64() > got[0]["seq"].as_u64());
    conn.close();
    host::detach(&crate::config::Config::default());
    t.join();
}

#[test]
fn rate_limit_rule_truth_table() {
    let rl = HostError::new("rate_limited", "x", true);
    for ty in ["transcript", "proposal", "confirm_reply"] {
        assert!(host::should_retry_emit(ty, &rl), "{ty} 限流要重试");
    }
    for ty in ["turn", "speech", "error"] {
        assert!(!host::should_retry_emit(ty, &rl), "{ty} 限流要丢弃");
    }
}

// ---------------------------------------------------------------- 反向命令

#[test]
fn command_invoke_is_answered_and_unknown_notifications_are_ignored() {
    let _g = host::test_guard();
    let p = sock_path("cmd");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        // 先来一条未知通知：必须被忽略、不回帧、不断开（§4.5）
        k.send(json!({"jsonrpc":"2.0","method":"$/somethingNew","params":{}}));
        let speak = json!({"schema":"agentear.command/1","command_id":"c1","type":"speak","payload":{"text":"你好","lang":"zh-CN"}});
        k.send(json!({"jsonrpc":"2.0","id":"k1","method":"_a24/command/invoke","params":{"name":"speak","body":speak}}));
        let r1 = loop {
            let v = k.recv();
            if v.get("method").is_none() {
                break v;
            }
        };
        assert_eq!(r1["id"], "k1", "第一个回帧就是 k1 的应答（通知没有产生回帧）");
        assert_eq!(r1["result"], json!({"accepted": true}), "result 必须是对象（§6.1）");
        // 重复 command_id：回首次结果
        k.send(json!({"jsonrpc":"2.0","id":"k2","method":"_a24/command/invoke","params":{"name":"speak","body":speak}}));
        let r2 = loop { let v = k.recv(); if v.get("method").is_none() { break v; } };
        assert_eq!(r2["id"], "k2");
        assert_eq!(r2["result"], json!({"accepted": true}));
        // name 与 body.type 不一致 → -32602
        k.send(json!({"jsonrpc":"2.0","id":"k3","method":"_a24/command/invoke","params":{"name":"stop_playback","body":speak}}));
        let r3 = loop { let v = k.recv(); if v.get("method").is_none() { break v; } };
        assert_eq!(r3["error"]["code"], -32602);
        // 未知方法 → -32601
        k.send(json!({"jsonrpc":"2.0","id":"k4","method":"_a24/unknown","params":{}}));
        let r4 = loop { let v = k.recv(); if v.get("method").is_none() { break v; } };
        assert_eq!(r4["error"]["code"], -32601);
        k.w.shutdown(std::net::Shutdown::Both).unwrap();
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    host::attach(Arc::new(A3Host { conn: conn.clone() }));
    conn.wait_closed();
    host::detach(&crate::config::Config::default());
    t.join();
}

#[test]
fn oversized_inbound_frame_closes_the_connection() {
    let p = sock_path("big");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/"]);
        let mut b = vec![b'x'; MAX_FRAME_BYTES + 10];
        b.push(b'\n');
        let _ = k.w.write_all(&b);
        std::thread::sleep(Duration::from_millis(200));
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    let start = Instant::now();
    conn.wait_closed();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!conn.alive());
    t.join();
}

#[test]
fn in_flight_call_fails_fast_when_the_connection_drops() {
    let p = sock_path("drop");
    let t = kernel(&p, 1, |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        let _ = k.recv();
        k.w.shutdown(std::net::Shutdown::Both).unwrap(); // 不应答，直接断
    });
    let conn = Conn::connect(&creds(&p), quiet_handler()).unwrap();
    let start = Instant::now();
    let e = conn.call("_a24/model/complete", json!({"messages":[]}), Duration::from_secs(30)).unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5), "断开时在途调用要立刻失败，不等超时");
    assert_eq!(e.kind, "internal");
    t.join();
}

// ---------------------------------------------------------------- 守护：断连 → B5 → 重连新 session

/// 进程里只起一次守护线程，所以只有这一条测试用它。
#[test]
fn supervisor_reconnects_with_a_new_session_after_eof() {
    let _g = host::test_guard();
    let p = sock_path("sup");
    let sessions = Arc::new(Mutex::new(Vec::<(String, u64)>::new()));
    let s2 = sessions.clone();
    let t = kernel(&p, 2, move |i, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        let v = k.recv();
        let ev = &v["params"]["payload"];
        s2.lock().unwrap().push((ev["session_id"].as_str().unwrap().to_string(), ev["seq"].as_u64().unwrap()));
        k.send(json!({"jsonrpc":"2.0","id":v["id"],"result":{}}));
        if i == 0 {
            k.w.shutdown(std::net::Shutdown::Both).unwrap(); // 第一条：断开
        } else {
            k.wait_eof(); // 第二条：保持到测试结束
        }
    });
    let pc = p.clone();
    start_supervisor(Supervisor {
        creds: Box::new(move || Some(creds(&pc))),
        rotate: Box::new(|| Err(StopReason::Repair)),
        handler: quiet_handler(),
    });
    // 等第一次附着，发一条事件
    let wait = |want: LinkStatus| {
        let t0 = Instant::now();
        while status() != want {
            assert!(t0.elapsed() < Duration::from_secs(10), "等 {want:?} 超时，现在 {:?}", status());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait(LinkStatus::Connected);
    host::emit("turn", host::turn_payload("listening", Some(1)));
    // 断开后：按 B5 离开 Connected（回独立模式或停听，取决于配置），再退避 1 s 重连
    let t0 = Instant::now();
    while sessions.lock().unwrap().is_empty() || status() == LinkStatus::Connected {
        assert!(t0.elapsed() < Duration::from_secs(10), "没观察到断开，现在 {:?}", status());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        matches!(status(), LinkStatus::DisconnectedStandalone | LinkStatus::DisconnectedStopped | LinkStatus::Connecting),
        "断开后状态 {:?}",
        status()
    );
    // 退避 1 s 后重连成功
    wait(LinkStatus::Connected);
    host::emit("turn", host::turn_payload("listening", Some(1)));
    let t1 = Instant::now();
    while sessions.lock().unwrap().len() < 2 {
        assert!(t1.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    let s = sessions.lock().unwrap().clone();
    assert_ne!(s[0].0, s[1].0, "重连必须是新 session");
    assert_eq!(s[1].1, 1, "新 session 的 seq 从 1 开始");
    disconnect_now();
    t.join();
}

// ---------------------------------------------------------------- manifest

#[test]
fn manifest_digest_is_the_raw_file_bytes_and_ignores_app_version() {
    let disk = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/agent24/domain-os.yml")).unwrap();
    assert_eq!(MANIFEST, &disk[..]);
    assert_eq!(manifest_digest(), manifest_digest_of(&disk));
    let text = std::str::from_utf8(MANIFEST).unwrap();
    assert!(text.contains("version: \"1\""), "version 是 manifest 自身修订号");
    assert!(
        !text.contains(env!("CARGO_PKG_VERSION")),
        "manifest 不许含 app 版本号——否则每次升级都改 digest、断一次配对"
    );
    for must in ["name: agentear", "impl_kind: attached_process", "model_access: local_only", "host_commands: [speak, stop_playback]", "kernel_capabilities: [events, models]"] {
        assert!(text.contains(must), "manifest 缺 {must}");
    }
    let d = manifest_digest();
    assert!(d.starts_with("sha256:") && d.len() == 71 && d[7..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn creds_debug_never_shows_the_token() {
    let c = Creds { socket: "/tmp/x".into(), token: "SUPER-SECRET".into(), digest: "sha256:0".into() };
    assert!(!format!("{c:?}").contains("SUPER-SECRET"));
}

#[test]
fn backoff_doubles_to_thirty_seconds() {
    let mut b = BACKOFF_START;
    let mut seq = vec![b.as_secs()];
    for _ in 0..7 {
        b = next_backoff(b);
        seq.push(b.as_secs());
    }
    assert_eq!(seq, [1, 2, 4, 8, 16, 30, 30, 30]);
}

/// PR #96 评审在 A3 上的版本：断开后旧队列里的事件**不会出现在新连接上**，
/// 也不再发往旧连接；新连接只收到新 session 的事件。
#[test]
fn old_connection_queue_never_reaches_the_new_connection() {
    let _g = host::test_guard();
    let p1 = sock_path("oldq");
    let p2 = sock_path("newq");
    let old_seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let new_seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let o2 = old_seen.clone();
    let t1 = kernel(&p1, 1, move |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        // 慢内核：每条 emit 200 ms 后才应答，让后面的事件堆在模块的队列里。
        while let Ok(Some(b)) = read_frame(&mut k.r) {
            let v: Value = serde_json::from_slice(&b).unwrap();
            if v["method"] == "_a24/events/emit" {
                o2.lock().unwrap().push(v["params"]["payload"].clone());
                std::thread::sleep(Duration::from_millis(200));
                let mut out = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":v["id"],"result":{}})).unwrap();
                out.push(b'\n');
                if k.w.write_all(&out).is_err() {
                    break;
                }
            }
        }
    });
    let n2 = new_seen.clone();
    let t2 = kernel(&p2, 1, move |_, mut k| {
        k.accept_ok(&["_a24/events/", "_a24/model/"]);
        while let Ok(Some(b)) = read_frame(&mut k.r) {
            let v: Value = serde_json::from_slice(&b).unwrap();
            if v["method"] == "_a24/events/emit" {
                n2.lock().unwrap().push(v["params"]["payload"].clone());
                k.send(json!({"jsonrpc":"2.0","id":v["id"],"result":{}}));
            }
        }
    });
    let c1 = Conn::connect(&creds(&p1), quiet_handler()).unwrap();
    host::attach(Arc::new(A3Host { conn: c1.clone() }));
    for i in 0..4 {
        host::emit("transcript", host::transcript_payload(&format!("旧{i}"), "zh-CN", None));
    }
    std::thread::sleep(Duration::from_millis(50)); // 第一条在线上
    // 断连：与守护线程的顺序一致——关连接、detach，然后连新的。
    c1.close();
    host::detach(&crate::config::Config::default());
    let c2 = Conn::connect(&creds(&p2), quiet_handler()).unwrap();
    host::attach(Arc::new(A3Host { conn: c2.clone() }));
    host::emit("transcript", host::transcript_payload("新", "zh-CN", None));
    assert!(host::flush(Duration::from_secs(5)), "旧队列不能卡住新连接的投递");
    c2.close();
    host::detach(&crate::config::Config::default());
    t1.join();
    t2.join();
    let old = old_seen.lock().unwrap().clone();
    let new = new_seen.lock().unwrap().clone();
    assert!(old.len() <= 1, "断开后旧队列不许再发往旧连接：{old:?}");
    assert_eq!(new.len(), 1, "新连接只该收到新事件：{new:?}");
    assert_eq!(new[0]["payload"]["text"], "新");
    assert_eq!(new[0]["seq"], 1);
    if let Some(o) = old.first() {
        assert_ne!(o["session_id"], new[0]["session_id"]);
    }
}

/// #97 评审：`strip_nulls` 要递归进数组元素里的对象；数组里的 null 值本身保留。
#[test]
fn strip_nulls_recurses_into_array_elements() {
    let mut v = serde_json::json!({
        "a": null,
        "messages": [
            {"role": "user", "content": "hi", "name": null},
            [ {"x": null, "y": 1} ],
            null
        ],
        "nested": {"b": null, "c": [ {"d": null} ]}
    });
    strip_nulls(&mut v);
    assert_eq!(
        v,
        serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                [ {"y": 1} ],
                null
            ],
            "nested": {"c": [ {} ]}
        })
    );
}
