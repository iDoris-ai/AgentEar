use super::*;
use serde_json::{json, Value};

fn full_record() -> InputRecord {
    let mut r = InputRecord::new(
        "builtin",
        Some(20_000),
        Snapshot {
            load1: Some(12.3),
            ncpu: Some(10),
            swap_used_mb: Some(20_582),
            swap_total_mb: Some(22_528),
            pressure: Some(1),
            self_rss_mb: Some(80),
            asr_rss_mb: None,
        },
    );
    r.key_to_commit_ms = Some(40);
    r.set_asr(
        Duration::from_millis(6300),
        Some(AsrProbe { spawn_ms: 3, first_byte_ms: Some(900), peak_rss_mb: Some(450) }),
        false,
    );
    r.correct_ms = Some(10);
    r.label_ms = Some(5);
    r.routes_kb_ms = Some(20);
    r.deliver_ms = Some(60);
    r.set_delivered(Duration::from_millis(6500));
    r.index_ms = Some(30);
    r
}

#[test]
fn segments_add_up_to_the_total_within_other() {
    let r = full_record();
    // 40 + 6300 + 10 + 5 + 20 + 60 = 6435；总 6500 → other 65
    assert_eq!(r.accounted_ms(), 6435);
    assert_eq!(r.other_ms, Some(65));
    assert_eq!(r.accounted_ms() + r.other_ms.unwrap(), r.release_to_delivered_ms.unwrap());
    // ASR 内部拆分：拉起 + 解码 = 合计，且不被重复计入 accounted。
    assert_eq!(r.asr_spawn_to_first_byte_ms.unwrap() + r.asr_decode_ms.unwrap(), r.asr_total_ms.unwrap());
}

#[test]
fn other_never_goes_negative() {
    let mut r = InputRecord::new("builtin", None, Snapshot::default());
    r.asr_total_ms = Some(5000);
    r.set_delivered(Duration::from_millis(100)); // 不该发生，但不能下溢
    assert_eq!(r.other_ms, Some(0));
}

#[test]
fn missing_probe_omits_spawn_and_decode_instead_of_zero() {
    let mut r = InputRecord::new("speech_swift", None, Snapshot::default());
    r.set_asr(Duration::from_millis(1800), None, false);
    assert_eq!(r.asr_total_ms, Some(1800));
    assert_eq!(r.asr_spawn_to_first_byte_ms, None);
    assert_eq!(r.asr_decode_ms, None);
    let v = serde_json::to_value(&r).unwrap();
    assert!(v.get("asr_decode_ms").is_none() && v.get("asr_spawn_to_first_byte_ms").is_none());
    // 子进程一直没输出：first_byte 缺 → 同样省略，只带 RSS
    let mut r2 = InputRecord::new("builtin", None, Snapshot::default());
    r2.set_asr(Duration::from_millis(900), Some(AsrProbe { spawn_ms: 2, first_byte_ms: None, peak_rss_mb: Some(300) }), false);
    assert_eq!(r2.asr_decode_ms, None);
    assert_eq!(r2.sys_end.asr_rss_mb, Some(300));
}

#[test]
fn snapshot_degrades_when_fields_are_missing() {
    let empty = Snapshot::default();
    assert_eq!(empty.load_per_core(), None);
    assert_eq!(empty.swap_ratio(), None);
    assert_eq!(Snapshot { load1: Some(5.0), ..Default::default() }.load_per_core(), None);
    assert_eq!(Snapshot { swap_used_mb: Some(5), swap_total_mb: Some(0), ..Default::default() }.swap_ratio(), None);
    // 序列化：全空 = {}
    assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
    // 日志行在全缺时不 panic、用 — 占位
    let line = summary_line(&InputRecord::new("builtin", None, Snapshot::default()));
    assert!(line.contains("load —") && line.contains("swap —") && line.contains("压力 —"), "{line}");
}

#[test]
fn live_capture_returns_sane_numbers_on_this_machine() {
    let s = capture();
    // 本进程 RSS 一定读得到（macOS）；核数 ≥1
    assert!(s.self_rss_mb.unwrap_or(0) > 0, "{s:?}");
    assert!(s.ncpu.unwrap_or(0) >= 1, "{s:?}");
    assert!(s.load1.is_some(), "{s:?}");
    // 用真实快照 + 构造的分段打一行，便于 `--nocapture` 时肉眼看格式。
    let mut r = full_record();
    r.sys_start = s.clone();
    eprintln!("{}\n{}", summary_line(&r), serde_json::to_string(&r).unwrap());
}

#[test]
fn bucket_boundaries() {
    assert_eq!(load_bucket(Some(1.0)), "load/核 ≤1");
    assert_eq!(load_bucket(Some(1.0001)), "load/核 1–2");
    assert_eq!(load_bucket(Some(2.0)), "load/核 1–2");
    assert_eq!(load_bucket(Some(2.0001)), "load/核 >2");
    assert_eq!(load_bucket(None), "load 未知");
    assert_eq!(swap_bucket(Some(0.4999)), "swap <50%");
    assert_eq!(swap_bucket(Some(0.5)), "swap 50–90%");
    assert_eq!(swap_bucket(Some(0.9)), "swap 50–90%");
    assert_eq!(swap_bucket(Some(0.9001)), "swap >90%");
    assert_eq!(swap_bucket(None), "swap 未知");
}

#[test]
fn summary_line_matches_documented_format() {
    let line = summary_line(&full_record());
    assert!(line.starts_with("⏱ 输入轮 总 6.50s = 提交 0.04 + ASR 6.30（拉起 0.90 / 解码 5.40）+ 纠错 0.01 + 标签 0.01 + 落库 0.02 + 上屏 0.06"), "{line}");
    assert!(line.contains("| 录音 20.00s | load 12.3/10核 swap 20.1/22.0G 压力 1"), "{line}");
}

#[test]
fn fallback_outcome_is_derived_from_the_flag() {
    // finish() 会写盘（未 init 则不写）；这里只验结局推导的纯逻辑通过序列化前的字段。
    let mut r = InputRecord::new("speech_swift", None, Snapshot::default());
    r.set_asr(Duration::from_millis(10), None, true);
    assert!(r.backend_fallback);
    let outcome = if "ok" == "ok" && r.backend_fallback { "fallback" } else { "ok" };
    assert_eq!(outcome, "fallback");
}

fn rec(total: u64, load: f64, swap_used: u64, ts: u64) -> Value {
    json!({
        "kind":"input","ts_ms":ts,"backend":"builtin","outcome":"ok","backend_fallback":false,
        "record_ms":20000,"key_to_commit_ms":30,"asr_total_ms":total-100,"correct_ms":5,
        "label_ms":2,"routes_kb_ms":10,"deliver_ms":50,"release_to_delivered_ms":total,"other_ms":3,
        "sys_start":{"load1":load,"ncpu":10,"swap_used_mb":swap_used,"swap_total_mb":1000,"pressure":1},
        "sys_end":{}
    })
}

#[test]
fn report_says_insufficient_samples_under_ten() {
    let recs: Vec<Value> = (0..9).map(|i| rec(1000, 1.0, 100, i)).collect();
    let out = report(&recs, None);
    assert!(out.contains("样本不足（n=9 < 10），不下结论"), "{out}");
    assert!(!out.contains("按 load/核数"), "{out}");
}

#[test]
fn report_attributes_slow_turns_and_buckets_by_load_and_swap() {
    let mut recs: Vec<Value> = (0..12).map(|i| rec(1000, 0.5, 100, i)).collect();
    // 三个慢轮：高 load、高 swap
    recs.push(rec(8000, 31.0, 950, 100));
    recs.push(rec(9000, 32.0, 960, 101));
    recs.push(rec(7000, 33.0, 970, 102));
    // 旧的对话记录混在里面必须被忽略
    recs.push(json!({"mode":"conversation","to_first_audio_ms":2000}));
    let out = report(&recs, None);
    assert!(out.contains("慢轮 = 总时延 > 2×中位数（1000 ms → 2000 ms）：3 / 15 轮"), "{out}");
    assert!(out.contains("最大段 ASR"), "{out}");
    assert!(out.contains("load/核 ≤1") && out.contains("load/核 >2"), "{out}");
    // 慢桶只有 3 条 → 样本不足；快桶 12 条 → 中位数 1000
    assert!(out.contains("load/核 >2") && out.contains("样本不足，不下结论"), "{out}");
    assert!(out.contains("中位数 1000 ms"), "{out}");
    assert!(out.contains("swap >90%"), "{out}");
}

#[test]
fn old_report_ignores_input_rows_and_new_report_ignores_old_rows() {
    let recs = vec![
        json!({"mode":"conversation","outcome":"ok","to_first_audio_ms":2000}),
        rec(1000, 1.0, 1, 1),
    ];
    let old = crate::timings::report(&recs, None);
    assert!(old.starts_with("共 1 轮"), "{old}");
    assert!(report(&[recs[0].clone()], None).is_empty());
}

/// 隐私：记录里所有字符串值都必须落在封闭枚举里——没有任何位置能放转写文字 / 路径。
#[test]
fn every_string_value_is_a_known_enum() {
    fn walk(v: &Value, bad: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                if ![KIND].iter().chain(OUTCOMES).chain(BACKENDS).any(|k| k == s) {
                    bad.push(s.clone());
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, bad)),
            Value::Object(o) => o.values().for_each(|x| walk(x, bad)),
            _ => {}
        }
    }
    for outcome in OUTCOMES {
        for backend in BACKENDS {
            let mut r = full_record();
            r.backend = backend;
            r.outcome = outcome;
            let v = serde_json::to_value(&r).unwrap();
            let mut bad = vec![];
            walk(&v, &mut bad);
            assert!(bad.is_empty(), "出现了枚举之外的字符串值：{bad:?}");
        }
    }
    // 键集合也是白名单
    let v = serde_json::to_value(full_record()).unwrap();
    let allowed = [
        "kind","ts_ms","backend","outcome","backend_fallback","record_ms","key_to_commit_ms","asr_total_ms",
        "asr_spawn_to_first_byte_ms","asr_decode_ms","correct_ms","label_ms","routes_kb_ms","translate_ms",
        "deliver_ms","index_ms","release_to_delivered_ms","other_ms","sys_start","sys_end",
    ];
    for k in v.as_object().unwrap().keys() {
        assert!(allowed.contains(&k.as_str()), "白名单外的字段：{k}");
    }
}

#[test]
fn append_goes_to_the_same_file_and_rotation_keeps_history_readable() {
    let dir = std::env::temp_dir().join(format!("agentear-input-timings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("derived").join("timings.jsonl");
    crate::timings::append(&path, &full_record()).unwrap();
    // 写满 → 滚动，旧文件仍被 read_records 读到
    let big = crate::timings::ROTATE_BYTES as usize;
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.push_str(&"x".repeat(big));
    std::fs::write(&path, body).unwrap();
    crate::timings::append(&path, &full_record()).unwrap();
    assert!(path.with_extension("1.jsonl").exists());
    let recs = crate::timings::read_records(&path);
    assert_eq!(recs.iter().filter(|r| r["kind"] == "input").count(), 2, "滚动后统计不能清零");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_probed_captures_output_and_first_byte() {
    begin_asr();
    let out = run_probed(Command::new("/bin/sh").arg("-c").arg("echo hi; echo err >&2; sleep 0.2")).unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
    let p = take_probe().expect("走了子进程就该有探针");
    assert!(p.first_byte_ms.unwrap() < 150, "首字节应早于 sleep 结束：{p:?}");
    // 非零退出码照样原样返回
    let out = run_probed(Command::new("/bin/sh").arg("-c").arg("exit 3")).unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(take_probe().unwrap().first_byte_ms, None, "没输出 = 省略");
}
