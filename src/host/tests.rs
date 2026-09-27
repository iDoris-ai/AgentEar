//! 宿主适配层的测试。全部**不启动守护进程、不出声、不连网**：
//! 宿主用进程内 [`FakeHost`]，播放用记录型的假播放器。
//!
//! 发出去的每一种事件都用 `contracts/schema/agentear.event.v1` 校验——
//! 这是「实现与契约不漂移」在事件这一侧的落点（提案那侧在 `main.rs` 的 `contract_tests`）。

use super::*;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

fn global_lock() -> std::sync::MutexGuard<'static, ()> {
    test_guard()
}

// ---------------------------------------------------------------- schema

const SCHEMA_BASE: &str = "https://github.com/iDoris-ai/AgentEar/contracts/schema/";

fn contracts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts")
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn compile(name: &str) -> (boon::Schemas, boon::SchemaIndex) {
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    for f in [
        "agentear.event.v1.schema.json",
        "agentear.proposal.v1.schema.json",
        "agentear.command.v1.schema.json",
    ] {
        let v = read_json(&contracts().join("schema").join(f));
        compiler.add_resource(&format!("{SCHEMA_BASE}{f}"), v).unwrap();
    }
    let idx = compiler
        .compile(&format!("{SCHEMA_BASE}{name}"), &mut schemas)
        .unwrap();
    (schemas, idx)
}

fn assert_event_valid(ev: &Value) {
    let (schemas, idx) = compile("agentear.event.v1.schema.json");
    if let Err(e) = schemas.validate(ev, idx) {
        panic!("发出的事件不符合 agentear.event/1：{ev}\n{e:#}");
    }
}

fn hash64() -> String {
    "a".repeat(64)
}

// ---------------------------------------------------------------- transport

#[test]
fn transport_parse_defaults_to_sidecar() {
    assert_eq!(LlmTransport::parse(""), LlmTransport::Sidecar);
    assert_eq!(LlmTransport::parse("sidecar"), LlmTransport::Sidecar);
    assert_eq!(LlmTransport::parse("idoris"), LlmTransport::Idoris);
    assert_eq!(LlmTransport::parse("agent24"), LlmTransport::Agent24);
    assert_eq!(LlmTransport::parse("cloud"), LlmTransport::Sidecar, "写错了不能坏掉，落到最保守的本机档");
    for t in [LlmTransport::Sidecar, LlmTransport::Idoris, LlmTransport::Agent24] {
        assert_eq!(LlmTransport::parse(t.as_str()), t);
    }
}

/// 独立版默认：没附着、默认配置 → `host` 不接管，走原来的本机边车。
#[test]
fn default_config_keeps_the_standalone_engine() {
    let _g = global_lock();
    let cfg = crate::config::Config::default();
    assert_eq!(cfg.talk_llm_transport, "sidecar");
    assert!(llm_engine(&cfg).is_none(), "默认配置必须走独立版原来那条路");
    assert_eq!(talk::Engines::from_config(&cfg).llm.name(), "openai_compat");
}

/// 旧配置文件没有新字段 → 等同默认（独立版行为不变）。
#[test]
fn old_config_without_new_fields_means_sidecar() {
    let old: crate::config::Config =
        serde_json::from_str(r#"{"talk_llm_url": "http://127.0.0.1:8794"}"#).unwrap();
    assert_eq!(old.talk_llm_transport, "sidecar");
    assert_eq!(old.idoris_privacy, "local_only");
    assert_eq!(old.agent24_model_access, "local_only");
    assert_eq!(old.attach_fallback, "auto_local");
}

/// 边车那档的请求体一个字节不变：不带 `model`；iDoris 档才带。
#[test]
fn model_field_only_when_configured() {
    let plain = talk::OpenAiCompat::new("http://x", Arc::new(crate::sidecar::Curl), 5);
    let body: Value = serde_json::from_str(&plain.body("s", "u", false)).unwrap();
    assert!(body.get("model").is_none());
    assert!(body.get("stream").is_none());
    let with = talk::OpenAiCompat::new("http://x", Arc::new(crate::sidecar::Curl), 5)
        .with_model(Some("qwen-7b".into()));
    let body: Value = serde_json::from_str(&with.body("s", "u", true)).unwrap();
    assert_eq!(body["model"], "qwen-7b");
    assert_eq!(body["stream"], true);
    let blank = talk::OpenAiCompat::new("http://x", Arc::new(crate::sidecar::Curl), 5)
        .with_model(Some("  ".into()));
    let body: Value = serde_json::from_str(&blank.body("s", "u", false)).unwrap();
    assert!(body.get("model").is_none(), "空白 model 不该写进去");
}

#[test]
fn idoris_transport_is_selected_and_private_by_default() {
    let _g = global_lock();
    let mut cfg = crate::config::Config::default();
    cfg.talk_llm_transport = "idoris".into();
    cfg.idoris_url = Some("http://127.0.0.1:8800".into());
    let e = llm_engine(&cfg).expect("idoris 档由 host 接管");
    assert_eq!(e.name(), "openai_compat");
    assert_eq!(idoris_privacy_header(&cfg.idoris_privacy), "local_only");
    assert_eq!(idoris_privacy_header("any"), "any");
    assert_eq!(idoris_privacy_header("ANY"), "local_only", "只认显式小写 any");
    assert_eq!(idoris_privacy_header("remote"), "local_only");
}

#[test]
fn agent24_transport_without_host_fails_honestly() {
    let _g = global_lock();
    let mut cfg = crate::config::Config::default();
    cfg.talk_llm_transport = "agent24".into();
    let e = llm_engine(&cfg).expect("agent24 档由 host 接管");
    assert_eq!(e.name(), "agent24");
    let err = e.reply("s", "u", TalkLang::Zh).unwrap_err();
    let h = err.downcast_ref::<HostError>().expect("是宿主错误");
    assert_eq!(h.kind, "unavailable", "没附着时不能偷偷退回边车");
}

#[test]
fn loopback_judgement() {
    for (u, want) in [
        ("http://127.0.0.1:8794", true),
        ("http://localhost:8794/v1", true),
        ("http://[::1]:8794", true),
        ("127.0.0.1:8794", true),
        ("http://127.9.9.9", true),
        ("http://user@127.0.0.1:1", true),
        ("http://192.168.1.5:8794", false),
        ("http://mac-mini.tailnet:8794", false),
        ("http://127.0.0.1.evil.com", false),
        ("http://localhost.evil.com", false),
        ("http://evil.com/?127.0.0.1", false),
        ("", false),
    ] {
        assert_eq!(is_loopback_url(u), want, "{u}");
    }
}

#[test]
fn standalone_locality() {
    let mut cfg = crate::config::Config::default();
    assert!(standalone_is_local(&cfg), "默认本机边车");
    cfg.talk_llm_url = Some("http://192.168.1.9:8794".into());
    assert!(!standalone_is_local(&cfg), "边车指到局域网 = 不是本机（jason D2）");
    cfg.talk_llm_engine = "mock".into();
    assert!(standalone_is_local(&cfg), "mock 根本不出进程");
    cfg.talk_llm_engine = "openai_compat".into();
    cfg.talk_llm_url = None;
    cfg.talk_llm_transport = "idoris".into();
    cfg.idoris_url = Some("http://127.0.0.1:8800".into());
    assert!(!standalone_is_local(&cfg), "iDoris 即使在回环也可能转外部，不算本机");
    cfg.talk_llm_transport = "agent24".into();
    assert!(!standalone_is_local(&cfg));
}

// ---------------------------------------------------------------- 模型回调

#[test]
fn model_request_only_sends_allowed_keys() {
    let req = ModelRequest {
        messages: vec![("system".into(), "s".into()), ("user".into(), "u".into())],
        complexity: Complexity::Simple,
        request_id: "req_1".into(),
        max_tokens: Some(99_999),
    };
    let p = req.to_params();
    let mut keys: Vec<_> = p.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["complexity", "max_tokens", "messages", "request_id"]);
    for banned in ["privacy", "model", "tools", "model_access"] {
        assert!(p.get(banned).is_none(), "宿主 deny_unknown_fields：不许带 {banned}");
    }
    assert_eq!(p["max_tokens"], 4096, "夹到宿主上限");
    assert_eq!(p["messages"][1]["role"], "user");
    let complex = ModelRequest {
        complexity: Complexity::Complex,
        max_tokens: None,
        ..req
    };
    let p = complex.to_params();
    assert_eq!(p["complexity"], "complex");
    assert!(p.get("max_tokens").is_none());
}

#[test]
fn model_result_parsing() {
    let ok = parse_model_result(&json!({
        "text": "你好", "model_id": "qwen", "tier": "local",
        "usage": {"prompt_tokens": 3, "completion_tokens": 2}
    }))
    .unwrap();
    assert_eq!((ok.text.as_str(), ok.tier, ok.completion_tokens), ("你好", Tier::Local, 2));
    assert_eq!(
        parse_model_result(&json!({"text": "x", "model_id": "m", "tier": "remote"})).unwrap().tier,
        Tier::Remote
    );
    for bad in [
        json!({"model_id": "m", "tier": "local"}),
        json!({"text": "x", "tier": "local"}),
        json!({"text": "x", "model_id": "m"}),
        json!({"text": "x", "model_id": "m", "tier": "lan"}),
    ] {
        assert!(parse_model_result(&bad).is_err(), "{bad}");
    }
}

#[test]
fn host_error_kinds_are_the_closed_set() {
    assert_eq!(HostError::new("timeout", "x", true).kind, "timeout");
    assert_eq!(HostError::new("privacy_denied", "x", false).kind, "privacy_denied");
    assert_eq!(HostError::new("model_timeout", "x", true).kind, "internal", "旧名不许漏出去");
    assert_eq!(HostError::new("a24_busy", "x", true).kind, "internal", "不加前缀");
    let e = HostError::from_rpc_error(&json!({
        "code": -32000, "message": "no local provider",
        "data": {"kind": "unavailable", "retryable": true, "cause": "no_provider"}
    }));
    assert_eq!(e.kind, "unavailable");
    assert!(e.retryable);
    assert_eq!(e.cause.as_deref(), Some("no_provider"));
    assert_eq!(HostError::from_rpc_error(&json!({})).kind, "internal");
}

/// 与 contracts 的 error.code 闭集完全一致（防漂移）。
#[test]
fn error_codes_match_the_schema_enum() {
    let schema = read_json(&contracts().join("schema/agentear.event.v1.schema.json"));
    let mut in_schema: Vec<String> = schema["$defs"]["error"]["properties"]["code"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut ours: Vec<String> = HOST_ERROR_KINDS
        .iter()
        .chain(AGENTEAR_ERROR_CODES.iter())
        .map(|s| s.to_string())
        .collect();
    in_schema.sort();
    ours.sort();
    assert_eq!(ours, in_schema);
}

#[test]
fn tier_check_truth_table() {
    assert!(check_tier("local_only", Tier::Local).is_ok());
    assert_eq!(check_tier("local_only", Tier::Remote).unwrap_err().kind, "privacy_denied");
    assert_eq!(check_tier("", Tier::Remote).unwrap_err().kind, "privacy_denied", "缺省按 local_only");
    assert_eq!(check_tier("whatever", Tier::Remote).unwrap_err().kind, "privacy_denied");
    assert!(check_tier("remote_allowed", Tier::Remote).is_ok());
    assert!(check_tier("remote_allowed", Tier::Local).is_ok());
}

fn host_llm(fake: &Arc<FakeHost>, access: &str) -> HostLlm {
    let mut cfg = crate::config::Config::default();
    cfg.agent24_model_access = access.into();
    HostLlm::new(fake.clone(), &cfg)
}

fn reply(text: &str, tier: Tier) -> ModelReply {
    ModelReply {
        text: text.into(),
        model_id: "m".into(),
        tier,
        prompt_tokens: 1,
        completion_tokens: 1,
    }
}

#[test]
fn host_llm_success_failure_and_cancel() {
    let fake = Arc::new(FakeHost::new());
    let llm = host_llm(&fake, "local_only");
    // 成功（默认回显）
    assert_eq!(llm.reply("系统", "今天天气", TalkLang::Zh).unwrap(), "（假宿主）收到：今天天气");
    let sent = fake.requests.lock().unwrap()[0].clone();
    assert_eq!(sent["complexity"], "simple");
    assert_eq!(sent["messages"][0]["content"], "系统");
    assert!(sent["request_id"].as_str().unwrap().starts_with("req_"));
    // 思考段不念
    fake.push_reply(Ok(reply("<think>嗯</think>好的。", Tier::Local)));
    assert_eq!(llm.reply("s", "u", TalkLang::Zh).unwrap(), "好的。");
    // 各种宿主错误原样带出 kind
    for kind in ["timeout", "cancelled", "busy", "unavailable", "rate_limited"] {
        fake.push_reply(Err(HostError::new(kind, "x", true)));
        let e = llm.reply("s", "u", TalkLang::Zh).unwrap_err();
        assert_eq!(e.downcast_ref::<HostError>().unwrap().kind, kind);
    }
    // 空回答不当成功
    fake.push_reply(Ok(reply("   ", Tier::Local)));
    assert!(llm.reply("s", "u", TalkLang::Zh).is_err());
    // 断开
    fake.set_attached(false);
    assert_eq!(
        llm.reply("s", "u", TalkLang::Zh).unwrap_err().downcast_ref::<HostError>().unwrap().kind,
        "unavailable"
    );
}

fn reply_n(text: &str, completion_tokens: u64) -> ModelReply {
    ModelReply {
        text: text.into(),
        model_id: "Qwen3-8B-4bit".into(),
        tier: Tier::Local,
        prompt_tokens: 100,
        completion_tokens,
    }
}

/// jason 2026-09-27 真机：附着 Agent24、路由到 Qwen3-8B-4bit，
/// 输出不带 `<think>` 开头标签、160 token 全耗在思考上 → 整段内心独白被念出来。
#[test]
fn judge_host_reply_never_speaks_the_monologue() {
    let max = HOST_MAX_TOKENS;
    // ① 无开头标签的思考 + </think> + 答案 → 只念答案
    assert_eq!(
        judge_host_reply(&reply_n("好的，用户让我讲个笑话。我先想想……\n</think>\n\n为什么星星会笑？", 80), max),
        HostAnswer::Speak("为什么星星会笑？".into())
    );
    // ② 截断的纯思考（用满 max_tokens、没见到 </think>）→ 不念原文
    let monologue = "好的，用户让我讲个笑话。我需要先确认自己是否符合要求。用户之前提到过如果问天气";
    assert_eq!(judge_host_reply(&reply_n(monologue, u64::from(max)), max), HostAnswer::ReasoningTruncated);
    // ③ /no_think 下的空标签 + 答案
    assert_eq!(
        judge_host_reply(&reply_n("<think>\n\n</think>\n\n今天挺好。", 12), max),
        HostAnswer::Speak("今天挺好。".into())
    );
    // ④ 正常答案
    assert_eq!(judge_host_reply(&reply_n("今天挺好。", 6), max), HostAnswer::Speak("今天挺好。".into()));
    // 截断但已经想完（有 </think>）：答案可能被截短，但它是正文，照念
    assert_eq!(
        judge_host_reply(&reply_n("想……</think>答案的前半句", u64::from(max)), max),
        HostAnswer::Speak("答案的前半句".into())
    );
    // 没截断、剥完为空
    assert_eq!(judge_host_reply(&reply_n("<think>只有思考</think>", 20), max), HostAnswer::Empty);
}

#[test]
fn host_llm_asks_for_no_think_and_speaks_a_hint_when_reasoning_is_cut() {
    let _g = global_lock();
    let fake = Arc::new(FakeHost::new());
    attach(fake.clone());
    let llm = host_llm(&fake, "local_only");
    // 请求：user 末尾带 /no_think、上限 HOST_MAX_TOKENS
    fake.push_reply(Ok(reply_n("<think>\n\n</think>\n\n好的。", 5)));
    assert_eq!(llm.reply("系统", "讲个笑话", TalkLang::Zh).unwrap(), "好的。");
    let sent = fake.requests.lock().unwrap()[0].clone();
    assert_eq!(sent["messages"][1]["content"], "讲个笑话 /no_think");
    assert_eq!(sent["max_tokens"], HOST_MAX_TOKENS);
    // 截断在思考里：念提示，不念独白，并报一条 error
    fake.push_reply(Ok(reply_n("好的，用户让我讲个笑话。我需要先确认", u64::from(HOST_MAX_TOKENS))));
    let said = llm.reply("系统", "讲个笑话", TalkLang::Zh).unwrap();
    assert_eq!(said, reasoning_truncated_hint(TalkLang::Zh));
    assert!(!said.contains("用户让我"));
    assert!(flush(Duration::from_secs(3)));
    let errs: Vec<_> = fake
        .events()
        .into_iter()
        .filter(|e| e["type"] == "error")
        .collect();
    assert_eq!(errs.len(), 1, "截断要报且只报一条 error");
    assert_eq!(errs[0]["payload"]["code"], "internal");
    assert_eq!(errs[0]["payload"]["message"], "reasoning_truncated");
    detach(&crate::config::Config::default());
    // 三种语言的提示都有、都不空
    for l in [TalkLang::Zh, TalkLang::En, TalkLang::Th] {
        assert!(!reasoning_truncated_hint(l).is_empty());
    }
    // no_think 不重复加
    assert_eq!(no_think("你好 /no_think"), "你好 /no_think");
}

#[test]
fn host_llm_refuses_a_remote_answer_under_local_only() {
    let fake = Arc::new(FakeHost::new());
    fake.push_reply(Ok(reply("来自云端", Tier::Remote)));
    let e = host_llm(&fake, "local_only").reply("s", "u", TalkLang::Zh).unwrap_err();
    assert_eq!(e.downcast_ref::<HostError>().unwrap().kind, "privacy_denied");
    fake.push_reply(Ok(reply("来自云端", Tier::Remote)));
    assert_eq!(
        host_llm(&fake, "remote_allowed").reply("s", "u", TalkLang::Zh).unwrap(),
        "来自云端"
    );
}

// ---------------------------------------------------------------- 事件

#[test]
fn bcp47_mapping() {
    for (asr, want) in [
        (Some("zh"), "zh-CN"),
        (Some("en"), "en-US"),
        (Some("th"), "th-TH"),
        (Some("yue"), "zh-HK"),
        (Some("ja"), "ja-JP"),
        (Some("ko"), "ko-KR"),
        (Some("ZH"), "zh-CN"),
        (Some("?"), "und"),
        (Some("nospeech"), "und"),
        (None, "und"),
    ] {
        assert_eq!(bcp47(asr), want, "{asr:?}");
    }
    assert_eq!(talk_lang_bcp47(TalkLang::Th), "th-TH");
}

#[test]
fn every_payload_builder_produces_a_valid_event() {
    let em = Emitter::new();
    let evs = vec![
        em.next("transcript", transcript_payload("今天天气怎么样？", "zh-CN", None)),
        em.next("transcript", transcript_payload("hi", "en-US", Some(&hash64()))),
        em.next("transcript", transcript_payload("x", bcp47(Some("?")), None)),
        em.next("turn", turn_payload("thinking", Some(1))),
        em.next("turn", turn_payload("idle", None)),
        em.next("turn", turn_payload("listening", Some(0))),
        em.next("speech", speech_payload("started", Some("cmd_1"), None)),
        em.next("speech", speech_payload("completed", None, None)),
        em.next("speech", speech_payload("stopped", Some("cmd_1"), Some("barge_in"))),
        em.next("speech", speech_payload("failed", Some("cmd_1"), None)),
        em.next("error", error_payload(&HostError::new("timeout", "慢", true), None)),
        em.next("error", error_payload(&HostError::new("tts_unavailable", "x", true), Some("cmd_2"))),
        em.next("confirm_reply", confirm_reply_payload("evt_1", true, None)),
        em.next("confirm_reply", confirm_reply_payload("evt_1", false, Some("不要"))),
        em.next("confirm_reply", confirm_reply_payload("evt_1", true, Some("  "))),
    ];
    for (i, ev) in evs.iter().enumerate() {
        assert_event_valid(ev);
        assert_eq!(ev["seq"], (i + 1) as u64, "seq 从 1 起单调递增");
        assert_eq!(ev["session_id"], em.session_id());
    }
    let ids: std::collections::HashSet<_> = evs.iter().map(|e| e["event_id"].clone()).collect();
    assert_eq!(ids.len(), evs.len(), "event_id 互不相同");
    // 两个 Emitter（两次附着）session 不同
    assert_ne!(Emitter::new().session_id(), em.session_id());
}

/// 音频永不出本机：content_hash 字段只收 64 位十六进制，路径塞不进去。
#[test]
fn transcript_never_carries_an_audio_path() {
    for bad in [
        "/Users/jason/.agentear/raw/audio/abc.wav",
        &"A".repeat(64),
        &"a".repeat(63),
        "",
    ] {
        let p = transcript_payload("x", "zh-CN", Some(bad));
        assert!(p.get("content_hash").is_none(), "{bad}");
    }
    let p = transcript_payload("x", "zh-CN", Some(&hash64()));
    assert_eq!(p["content_hash"], hash64());
    let s = p.to_string();
    assert!(!s.contains(".wav") && !s.contains("raw/audio"));
}

#[test]
fn retries_reuse_the_same_event_id_and_seq() {
    let fake = FakeHost::new();
    fake.emit_failures.store(2, Ordering::SeqCst);
    let em = Emitter::new();
    let id = em.emit(&fake, "turn", turn_payload("thinking", None));
    let evs = fake.events();
    assert_eq!(evs.len(), 3, "两次失败 + 一次成功");
    assert!(evs.iter().all(|e| e["event_id"] == id.as_str() && e["seq"] == 1));
    // 下一个事件 seq +1（重试不递增）
    em.emit(&fake, "turn", turn_payload("idle", None));
    assert_eq!(fake.events().last().unwrap()["seq"], 2);
    // 一直失败：最多投 EMIT_ATTEMPTS 次，不无限重试
    let stubborn = FakeHost::new();
    stubborn.emit_failures.store(100, Ordering::SeqCst);
    em.emit(&stubborn, "turn", turn_payload("idle", None));
    assert_eq!(stubborn.events().len(), EMIT_ATTEMPTS);
}

// ---------------------------------------------------------------- 附着状态机

#[test]
fn after_disconnect_truth_table() {
    use AttachFallback::*;
    assert_eq!(after_disconnect(AutoLocal, true), AttachState::Standalone);
    assert_eq!(after_disconnect(AutoLocal, false), AttachState::Disconnected);
    assert_eq!(after_disconnect(Stop, true), AttachState::Disconnected);
    assert_eq!(after_disconnect(Stop, false), AttachState::Disconnected);
    assert_eq!(AttachFallback::parse("stop"), Stop);
    assert_eq!(AttachFallback::parse("auto_local"), AutoLocal);
    assert_eq!(AttachFallback::parse("乱写"), AutoLocal);
    assert!(listening_allowed(AttachState::Standalone));
    assert!(listening_allowed(AttachState::Attached));
    assert!(!listening_allowed(AttachState::Disconnected));
}

#[test]
fn attach_detach_lifecycle_follows_b5() {
    let _g = global_lock();
    assert!(!attached());
    assert!(emit("turn", turn_payload("idle", None)).is_none(), "独立模式不发事件");

    let fake = Arc::new(FakeHost::new());
    attach(fake.clone());
    assert!(attached());
    assert_eq!(state(), AttachState::Attached);
    let id = emit("turn", turn_payload("listening", None)).unwrap();
    assert!(flush(Duration::from_secs(2)));
    assert_eq!(fake.events()[0]["event_id"], id.as_str());
    // 附着时推理经宿主（不论 talk_llm_transport 选的是什么，连 mock 也让位）
    let cfg = crate::config::Config::default();
    assert_eq!(talk::Engines::from_config(&cfg).llm.name(), "agent24");
    let mut mock = cfg.clone();
    mock.talk_llm_engine = "mock".into();
    assert_eq!(talk::Engines::from_config(&mock).llm.name(), "agent24");

    // 连接掉了但还没 detach：attached() 跟随连接
    fake.set_attached(false);
    assert!(!attached());
    fake.set_attached(true);

    // 默认配置（本机边车）断开 → 回独立模式
    await_confirm("evt_x".into(), Duration::from_secs(30));
    assert_eq!(detach(&cfg), AttachState::Standalone);
    assert!(!attached());
    assert!(!has_awaiting(), "断开后挂着的 proposal 作废");
    assert_eq!(talk::Engines::from_config(&cfg).llm.name(), "openai_compat");
    assert_eq!(talk::Engines::from_config(&mock).llm.name(), "mock", "独立模式 mock 照旧");

    // 独立模式配成 iDoris → 断开后停听
    attach(fake.clone());
    let mut idoris = cfg.clone();
    idoris.talk_llm_transport = "idoris".into();
    assert_eq!(detach(&idoris), AttachState::Disconnected);
    assert!(!listening_allowed(state()));
    resume_standalone();
    assert!(listening_allowed(state()));

    // stop：本机边车也停听
    attach(fake.clone());
    let mut stop = cfg.clone();
    stop.attach_fallback = "stop".into();
    assert_eq!(detach(&stop), AttachState::Disconnected);
}

#[test]
fn a_new_attachment_starts_a_new_session() {
    let _g = global_lock();
    let fake = Arc::new(FakeHost::new());
    attach(fake.clone());
    emit("turn", turn_payload("idle", None));
    // 换代前先等第一条投递完：换代会作废旧队列里**没发出去**的事件（#96 评审修复），
    // 这条测的是「新附着 = 新 session」，不是「旧队列能跨代存活」。
    assert!(flush(Duration::from_secs(2)));
    attach(fake.clone());
    emit("turn", turn_payload("idle", None));
    assert!(flush(Duration::from_secs(2)));
    let evs = fake.events();
    assert_ne!(evs[0]["session_id"], evs[1]["session_id"]);
    assert_eq!(evs[1]["seq"], 1, "新 session 的 seq 从 1 起");
}

#[test]
fn awaiting_expires_and_newest_wins() {
    let _g = global_lock();
    await_confirm("evt_old".into(), Duration::from_secs(30));
    await_confirm("evt_new".into(), Duration::from_secs(30));
    assert_eq!(take_awaiting().as_deref(), Some("evt_new"), "只留最新的一条");
    assert!(take_awaiting().is_none(), "取走即清");
    await_confirm("evt_gone".into(), Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(10));
    assert!(!has_awaiting());
    assert!(take_awaiting().is_none(), "过期的不交出去");
}

#[test]
fn confirm_reply_judgement() {
    assert!(confirm_reply_for(None), "短按录音键 = 确认");
    assert!(confirm_reply_for(Some("确认")));
    assert!(confirm_reply_for(Some("好的")));
    assert!(!confirm_reply_for(Some("不确认")), "否定先判");
    assert!(!confirm_reply_for(Some("别发")));
    assert!(!confirm_reply_for(Some("帮我确认一下明天的会")), "长句不算同意");
    assert!(!confirm_reply_for(Some("今天天气怎么样")), "别的话一律 reject");
}

// ---------------------------------------------------------------- 命令

/// 命令解析与 schema 一致：合法 fixture 都能解析，非法的都拒（双向防漂移）。
#[test]
fn command_parser_agrees_with_the_fixtures() {
    let dir = contracts().join("fixtures/command");
    let (schemas, idx) = compile("agentear.command.v1.schema.json");
    for sub in ["valid", "invalid"] {
        for e in std::fs::read_dir(dir.join(sub)).unwrap() {
            let p = e.unwrap().path();
            let v = read_json(&p);
            let schema_ok = schemas.validate(&v, idx).is_ok();
            let ours = parse_command(&v);
            assert_eq!(schema_ok, sub == "valid", "fixture 自身：{}", p.display());
            assert_eq!(ours.is_ok(), schema_ok, "解析器与 schema 不一致：{} → {ours:?}", p.display());
        }
    }
}

#[test]
fn command_parser_edge_cases_match_schema() {
    let (schemas, idx) = compile("agentear.command.v1.schema.json");
    let base = |payload: Value| json!({"schema": "agentear.command/1", "command_id": "c1", "type": "speak", "payload": payload});
    let cases = vec![
        base(json!({"text": "你好", "lang": "zh-CN"})),
        base(json!({"text": "你好", "lang": "zh"})),
        base(json!({"text": "你好", "lang": "zh-CN", "extra": 1})),
        base(json!({"text": "x".repeat(2000), "lang": "en-US"})),
        base(json!({"text": "x".repeat(2001), "lang": "en-US"})),
        base(json!({"text": "你好", "lang": "zh-CN", "voice": ""})),
        base(json!({"text": "你好", "lang": "zh-CN", "voice": "女声", "style": "yue"})),
        json!({"schema": "agentear.command/1", "command_id": "c".repeat(129), "type": "stop_playback", "payload": {}}),
        json!({"schema": "agentear.command/1", "command_id": "c", "type": "stop_playback", "payload": {"x": 1}}),
        json!({"schema": "agentear.command/1", "command_id": "c", "type": "stop_playback", "payload": {"session_id": "s"}}),
        json!({"schema": "agentear.command/2", "command_id": "c", "type": "stop_playback", "payload": {}}),
    ];
    for v in cases {
        assert_eq!(
            parse_command(&v).is_ok(),
            schemas.validate(&v, idx).is_ok(),
            "解析器与 schema 不一致：{v}"
        );
    }
}

fn speak(id: &str) -> HostCommand {
    HostCommand::Speak(SpeakCmd {
        command_id: id.into(),
        text: format!("播报 {id}"),
        lang: "zh-CN".into(),
        voice: None,
        style: None,
    })
}

fn stop(id: &str) -> HostCommand {
    HostCommand::Stop {
        command_id: id.into(),
        session_id: None,
    }
}

fn speech_states(fx: &[Effect]) -> Vec<(String, String)> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Emit("speech", p) => Some((
                p["command_id"].as_str().unwrap_or("").to_string(),
                format!("{}{}", p["state"].as_str().unwrap(), p["reason"].as_str().map(|r| format!(":{r}")).unwrap_or_default()),
            )),
            _ => None,
        })
        .collect()
}

fn plays(fx: &[Effect]) -> Vec<String> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Play(s) => Some(s.command_id.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn speak_plays_now_when_idle_and_queues_when_busy() {
    let mut c = CommandCenter::new();
    let (ack, fx) = c.handle(speak("a"));
    assert_eq!(ack, Ack::Started);
    assert_eq!(plays(&fx), ["a"]);
    assert_eq!(speech_states(&fx), [("a".into(), "started".into())]);
    let (ack, fx) = c.handle(speak("b"));
    assert_eq!(ack, Ack::Queued);
    assert!(fx.is_empty());
    // a 放完 → completed + 开始放 b
    let fx = c.on_played("a", PlayOutcome::Completed);
    assert_eq!(speech_states(&fx), [("a".into(), "completed".into()), ("b".into(), "started".into())]);
    assert_eq!(plays(&fx), ["b"]);
    // b 失败 → failed(带 reason)，队列空
    let fx = c.on_played("b", PlayOutcome::Failed);
    assert_eq!(speech_states(&fx), [("b".into(), "failed:tts_unavailable".into())]);
    assert!(c.playing().is_none());
}

#[test]
fn speak_waits_for_the_current_turn() {
    let mut c = CommandCenter::new();
    assert!(c.set_turn_active(true).is_empty());
    let (ack, fx) = c.handle(speak("a"));
    assert_eq!(ack, Ack::Queued, "当前轮回答在说，宿主播报排在后面（B7）");
    assert!(plays(&fx).is_empty());
    let fx = c.set_turn_active(false);
    assert_eq!(plays(&fx), ["a"]);
}

#[test]
fn duplicate_command_ids_are_not_executed_twice() {
    let mut c = CommandCenter::new();
    c.handle(speak("a"));
    let (ack, fx) = c.handle(speak("a"));
    assert_eq!(ack, Ack::Duplicate(Box::new(Ack::Started)));
    assert!(fx.is_empty(), "重复的 command_id 不能再执行一次");
    assert_eq!(ack.as_json("a")["duplicate"], true);
    assert_eq!(ack.as_json("a")["status"], "started", "原样返回第一次的回执");
    let (ack, _) = c.handle(stop("s"));
    assert_eq!(ack, Ack::Stopped);
    let (ack, fx) = c.handle(stop("s"));
    assert_eq!(ack, Ack::Duplicate(Box::new(Ack::Stopped)));
    assert!(fx.is_empty());
}

#[test]
fn stop_clears_the_queue_and_stops_playback() {
    let mut c = CommandCenter::new();
    c.handle(speak("a"));
    c.handle(speak("b"));
    c.handle(speak("c"));
    let (ack, fx) = c.handle(stop("s"));
    assert_eq!(ack, Ack::Stopped);
    assert!(fx.contains(&Effect::StopPlayback));
    assert_eq!(
        speech_states(&fx),
        [
            ("b".into(), "stopped:stop_command".into()),
            ("c".into(), "stopped:stop_command".into()),
            ("a".into(), "stopped:stop_command".into()),
        ]
    );
    assert_eq!(c.queue_len(), 0);
    assert!(c.playing().is_none());
    // 掐掉之后播放线程回报 → 不重复报（已经不是「正在放」了）
    assert!(c.on_played("a", PlayOutcome::Interrupted).is_empty());
}

#[test]
fn stop_with_nothing_playing_still_succeeds() {
    let mut c = CommandCenter::new();
    let (ack, fx) = c.handle(stop("s"));
    assert_eq!(ack, Ack::Stopped);
    assert!(fx.is_empty(), "没东西在放：不掐、不报");
    // 当前轮在说时，stop 也要掐（宿主要求停的是「正在说的话」）
    c.set_turn_active(true);
    let (_, fx) = c.handle(stop("s2"));
    assert_eq!(fx, [Effect::StopPlayback]);
}

#[test]
fn barge_in_clears_everything_with_reason() {
    let mut c = CommandCenter::new();
    c.handle(speak("a"));
    c.handle(speak("b"));
    let fx = c.on_barge_in();
    assert_eq!(
        speech_states(&fx),
        [("b".into(), "stopped:barge_in".into()), ("a".into(), "stopped:barge_in".into())]
    );
    assert_eq!(c.queue_len(), 0);
    assert!(c.on_barge_in().is_empty(), "空闲时打断什么都不发");
}

#[test]
fn every_speech_effect_is_a_valid_event() {
    let mut c = CommandCenter::new();
    let mut all = Vec::new();
    all.extend(c.handle(speak("a")).1);
    c.handle(speak("b"));
    all.extend(c.on_played("a", PlayOutcome::Completed));
    all.extend(c.on_played("b", PlayOutcome::Failed));
    c.handle(speak("c"));
    c.handle(speak("d"));
    all.extend(c.handle(stop("s")).1);
    let em = Emitter::new();
    for f in all {
        if let Effect::Emit(ty, p) = f {
            assert_event_valid(&em.next(ty, p));
        }
    }
}

#[test]
fn seen_ids_are_bounded() {
    let mut c = CommandCenter::new();
    for i in 0..(SEEN_CAP + 10) {
        c.handle(stop(&format!("s{i}")));
    }
    assert_eq!(c.seen.len(), SEEN_CAP);
    assert!(!c.seen.contains_key("s0"), "最老的被淘汰");
    assert!(c.seen.contains_key(&format!("s{}", SEEN_CAP + 9)));
}

/// 端到端（不出声）：入站命令 → 播放器 → 事件回报到假宿主。
#[test]
fn inbound_commands_drive_playback_and_report_back() {
    let _g = global_lock();
    let fake = Arc::new(FakeHost::new());
    attach(fake.clone());
    let played: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let rec = played.clone();
    let player: Player = Arc::new(move |s: &SpeakCmd| {
        rec.lock().unwrap().push(s.text.clone());
        PlayOutcome::Completed
    });
    let cmd = |id: &str, text: &str| {
        json!({"schema": "agentear.command/1", "command_id": id, "type": "speak",
               "payload": {"text": text, "lang": "zh-CN"}})
    };
    assert_eq!(handle_inbound(&cmd("c1", "第一句"), &player)["status"], "started");
    // 第二条可能排队也可能直接开（取决于第一条放完没有），两者都合法
    let st = handle_inbound(&cmd("c2", "第二句"), &player)["status"].clone();
    assert!(st == "queued" || st == "started", "{st}");
    // 重复
    assert_eq!(handle_inbound(&cmd("c1", "第一句"), &player)["duplicate"], true);
    // 非法命令：返回 error，不改状态
    let bad = handle_inbound(&json!({"schema": "agentear.command/1"}), &player);
    assert!(bad.get("error").is_some());

    // 等两条都放完
    let deadline = Instant::now() + Duration::from_secs(5);
    while played.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(flush(Duration::from_secs(2)));
    assert_eq!(*played.lock().unwrap(), ["第一句", "第二句"], "按到达顺序、各放一次");
    let speech: Vec<(String, String)> = fake
        .events_of("speech")
        .iter()
        .map(|e| (e["payload"]["command_id"].as_str().unwrap().into(), e["payload"]["state"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        speech,
        [
            ("c1".into(), "started".into()),
            ("c1".into(), "completed".into()),
            ("c2".into(), "started".into()),
            ("c2".into(), "completed".into()),
        ]
    );
    for e in fake.events() {
        assert_event_valid(&e);
    }
}


// ---------------------------------------------------------------- A3 形态（草稿 @7fe6de1）

#[test]
fn emit_wire_shape() {
    let ev = Emitter::new().next("turn", turn_payload("idle", None));
    let p = emit_params(&ev);
    assert_eq!(p["kind"], "agentear.event");
    assert_eq!(p["payload"], ev, "payload 是完整 envelope，原样");
    let mut keys: Vec<_> = p.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["kind", "payload"]);
    // kind 满足内核的点分小写规则 [a-z0-9_-]+(\.[a-z0-9_-]+)+，≤96 字节
    assert!(EVENT_KIND.len() <= 96);
    assert!(EVENT_KIND.split('.').count() >= 2);
    assert!(EVENT_KIND
        .split('.')
        .all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')));
    assert_eq!(cancel_params("m7"), json!({"id": "m7"}));
}

#[test]
fn long_strings_are_truncated_with_a_mark() {
    let short = "你好".to_string();
    assert_eq!(clamp_str(&short), short);
    let exact = "a".repeat(MAX_WIRE_STRING);
    assert_eq!(clamp_str(&exact).len(), MAX_WIRE_STRING, "正好 8 KiB 不动");
    // 中文 3 字节一个，截断点必须落在字符边界上
    let long = "中".repeat(3000);
    let c = clamp_str(&long);
    assert!(c.len() <= MAX_WIRE_STRING);
    assert!(c.ends_with(TRUNCATED_MARK));
    assert!(c.trim_end_matches(TRUNCATED_MARK).chars().all(|ch| ch == '中'));
    // 事件里的超长转写：截断后仍是合法事件，整条不超限
    let ev = Emitter::new().next("transcript", transcript_payload(&long, "zh-CN", None));
    assert_event_valid(&ev);
    assert!(ev["payload"]["text"].as_str().unwrap().len() <= MAX_WIRE_STRING);
    // 嵌套的也截（proposal 里的 text / rest）
    let nested = clamp_strings(json!({"a": [ {"b": long.clone()} ], "n": 1}));
    assert!(nested["a"][0]["b"].as_str().unwrap().ends_with(TRUNCATED_MARK));
    assert_eq!(nested["n"], 1);
}

/// 投递在后台：宿主应答再慢，发事件的调用方（worker / 按键路径）也不被卡住；
/// 同一 session 串行，顺序不乱。
#[test]
fn emit_never_blocks_the_caller_and_stays_in_order() {
    let _g = global_lock();
    let fake = Arc::new(FakeHost::new());
    fake.emit_delay_ms.store(150, Ordering::SeqCst);
    attach(fake.clone());
    let t0 = Instant::now();
    for i in 0..3 {
        emit("turn", turn_payload("idle", Some(i + 1)));
    }
    let spent = t0.elapsed();
    assert!(spent < Duration::from_millis(100), "emit 阻塞了调用方 {spent:?}");
    assert!(flush(Duration::from_secs(3)));
    let seqs: Vec<u64> = fake.events().iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, [1, 2, 3]);
    assert_eq!(fake.emit_wire.lock().unwrap().len(), 3);
}

#[test]
fn model_call_times_out_on_the_module_side_and_cancels() {
    let fake = Arc::new(FakeHost::new());
    fake.model_delay_ms.store(3000, Ordering::SeqCst);
    let mut cfg = crate::config::Config::default();
    cfg.talk_timeout_secs = 1;
    let llm = HostLlm::new(fake.clone(), &cfg);
    let t0 = Instant::now();
    let e = llm.reply("s", "u", TalkLang::Zh).unwrap_err();
    assert!(t0.elapsed() < Duration::from_millis(2500), "按模块侧上限返回，不等宿主");
    assert_eq!(e.downcast_ref::<HostError>().unwrap().kind, "timeout");
    let sent = fake.requests.lock().unwrap()[0]["request_id"].as_str().unwrap().to_string();
    assert_eq!(*fake.cancels.lock().unwrap(), [sent], "超时后取消的正是那一个请求");
}

fn silent_player() -> (Player, Arc<StdMutex<Vec<String>>>) {
    let played: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let rec = played.clone();
    (
        Arc::new(move |s: &SpeakCmd| {
            rec.lock().unwrap().push(s.command_id.clone());
            PlayOutcome::Completed
        }),
        played,
    )
}

#[test]
fn rpc_inbound_accepts_valid_commands_only() {
    let _g = global_lock();
    let fake = Arc::new(FakeHost::new());
    attach(fake.clone());
    let (player, played) = silent_player();
    let body = |id: &str| json!({"schema": "agentear.command/1", "command_id": id, "type": "speak",
                                 "payload": {"text": "你好", "lang": "zh-CN"}});
    // 合法：立即 accepted（只表示已入队）
    assert_eq!(fake.invoke_command("speak", body("k1"), &player), Ok(json!({"accepted": true})));
    // 重复：回首次结果、不再执行
    assert_eq!(fake.invoke_command("speak", body("k1"), &player), Ok(json!({"accepted": true})));
    // name 与 body.type 不一致
    assert_eq!(fake.invoke_command("stop_playback", body("k2"), &player).unwrap_err().code, RPC_INVALID_PARAMS);
    // 未知命令名 / 未知版本 / 坏 body
    let mut bad = body("k3");
    bad["schema"] = json!("agentear.command/2");
    assert_eq!(fake.invoke_command("speak", bad, &player).unwrap_err().code, RPC_INVALID_PARAMS);
    let mut unknown = body("k4");
    unknown["type"] = json!("dance");
    assert_eq!(fake.invoke_command("dance", unknown, &player).unwrap_err().code, RPC_INVALID_PARAMS);
    // params 多了字段
    assert_eq!(
        handle_rpc_request(COMMAND_METHOD, &json!({"name": "speak", "body": body("k5"), "x": 1}), &player).unwrap_err().code,
        RPC_INVALID_PARAMS
    );
    // 未知方法
    assert_eq!(handle_rpc_request("_a24/other", &json!({}), &player).unwrap_err().code, RPC_METHOD_NOT_FOUND);
    // 无播放时 stop 也 accepted
    let stop = json!({"schema": "agentear.command/1", "command_id": "k6", "type": "stop_playback", "payload": {}});
    assert_eq!(fake.invoke_command("stop_playback", stop, &player), Ok(json!({"accepted": true})));

    let deadline = Instant::now() + Duration::from_secs(3);
    while played.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(*played.lock().unwrap(), ["k1"], "k1 只放一次，非法命令一条都不放");
}

/// 命令 fixtures 经 `_a24/command/invoke` 的形态送进来：合法的 accepted，非法的 -32602。
#[test]
fn rpc_inbound_agrees_with_command_fixtures() {
    let _g = global_lock();
    let fake = Arc::new(FakeHost::new());
    attach(fake.clone());
    let (player, _) = silent_player();
    let dir = contracts().join("fixtures/command");
    for sub in ["valid", "invalid"] {
        for e in std::fs::read_dir(dir.join(sub)).unwrap() {
            let p = e.unwrap().path();
            let body = read_json(&p);
            let name = body["type"].as_str().unwrap_or("speak").to_string();
            let r = fake.invoke_command(&name, body, &player);
            if sub == "valid" {
                assert_eq!(r, Ok(json!({"accepted": true})), "{}", p.display());
            } else {
                assert_eq!(r.unwrap_err().code, RPC_INVALID_PARAMS, "{}", p.display());
            }
        }
    }
}

#[test]
fn paths_for_the_host_hide_the_user_name() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(tilde_path(&format!("{home}/.agentear/commands.json")), "~/.agentear/commands.json");
    assert_eq!(tilde_path(&home), "~");
    assert_eq!(tilde_path(&format!("{home}x/commands.json")), "commands.json", "前缀相同但不是家目录");
    assert_eq!(tilde_path("/private/tmp/data/commands.json"), "commands.json");
    let user = home.rsplit('/').next().unwrap();
    assert!(!tilde_path(&format!("{home}/a/b.json")).contains(user));
}

/// PR #96 评审的场景（常驻回归）：事件入队后、还没投递完就 detach——
/// **缓冲里的不再投递，失败的那一次也不再重试**。评审原话：delivered_after_detach=true。
#[test]
fn detach_voids_queued_and_retrying_events() {
    let _g = test_guard();
    let fake = Arc::new(FakeHost::new());
    fake.emit_delay_ms.store(200, Ordering::SeqCst);
    // 第一条第一次投递「失败但已记录」，于是会退避重试——重试不许在 detach 之后发生。
    fake.emit_failures.store(1, Ordering::SeqCst);
    attach(fake.clone());
    emit("turn", turn_payload("thinking", Some(1)));
    emit("transcript", transcript_payload("一", "zh-CN", None));
    emit("transcript", transcript_payload("二", "zh-CN", None));
    std::thread::sleep(Duration::from_millis(50)); // 第一条正在投递
    detach(&crate::config::Config::default());
    assert!(flush(Duration::from_secs(3)), "作废后队列要能清空，IN_FLIGHT 不能卡住");
    std::thread::sleep(Duration::from_millis(500));
    let n = fake.events().len();
    assert!(n <= 1, "detach 之后不许再投递（只有已在线上的那一次可能落地），实际 {n} 条");
}

/// 换新附着：旧队列的事件绝不被带到新宿主，新宿主只收到新 session 的事件。
#[test]
fn reattach_does_not_carry_old_queue_to_the_new_host() {
    let _g = test_guard();
    let old = Arc::new(FakeHost::new());
    old.emit_delay_ms.store(200, Ordering::SeqCst);
    attach(old.clone());
    for i in 0..3 {
        emit("transcript", transcript_payload(&format!("旧{i}"), "zh-CN", None));
    }
    std::thread::sleep(Duration::from_millis(50));
    let new = Arc::new(FakeHost::new());
    attach(new.clone());
    emit("transcript", transcript_payload("新", "zh-CN", None));
    assert!(flush(Duration::from_secs(3)));
    std::thread::sleep(Duration::from_millis(400));
    let got = new.events();
    assert_eq!(got.len(), 1, "新宿主只该收到新事件：{got:?}");
    assert_eq!(got[0]["payload"]["text"], "新");
    assert_eq!(got[0]["seq"], 1, "新附着 = 新 session，seq 从 1");
    assert!(old.events().len() <= 1, "旧队列里没上线的不许再发往旧宿主");
    detach(&crate::config::Config::default());
}
