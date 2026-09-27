//! `contracts/` 的自检：schema 本身能编译，合法 fixture 全过、非法 fixture 全拒。
//!
//! 这是 AgentEar ↔ Agent24 两边共用的那组 fixtures（docs/agent24-embedding.md §6.1）。
//! Agent24 的 CI 按 commit hash 引用同一批文件；这里保证它们**在我们这边**是自洽的——
//! 一个「合法样例其实过不了 schema」的 fixture 会让对方的契约测试从第一天就测错东西。
//!
//! 「AgentEar 真实发出的 proposal 符合 schema」不在这里测（二进制入口在没有 ASR vendor
//! 的 CI 上跑不到那一步），而是在 `src/main.rs` 的 `contract_tests` 里直接调
//! `proposal_json()`——同一条代码路径。

use std::path::{Path, PathBuf};

const SCHEMA_BASE: &str = "https://github.com/iDoris-ai/AgentEar/contracts/schema/";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts")
}

fn read_json(p: &Path) -> serde_json::Value {
    let s = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("读不了 {}: {e}", p.display()));
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("{} 不是合法 JSON: {e}", p.display()))
}

/// 把三份 schema 都注册进去（event 用相对 `$ref` 引 proposal），编译出要的那一份。
fn compile(name: &str) -> (boon::Schemas, boon::SchemaIndex) {
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    for f in [
        "agentear.event.v1.schema.json",
        "agentear.proposal.v1.schema.json",
        "agentear.command.v1.schema.json",
    ] {
        let v = read_json(&root().join("schema").join(f));
        assert_eq!(
            v["$id"].as_str(),
            Some(format!("{SCHEMA_BASE}{f}").as_str()),
            "{f} 的 $id 必须和文件名对应，否则相对 $ref 解析到别处"
        );
        compiler.add_resource(&format!("{SCHEMA_BASE}{f}"), v).unwrap();
    }
    let idx = compiler
        .compile(&format!("{SCHEMA_BASE}{name}"), &mut schemas)
        .unwrap_or_else(|e| panic!("{name} 编译失败: {e}"));
    (schemas, idx)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("读不了目录 {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    v.sort();
    assert!(!v.is_empty(), "{} 里一个 fixture 都没有——空目录会让测试假绿", dir.display());
    v
}

fn check_dir(schema: &str, kind: &str) {
    let (schemas, idx) = compile(schema);
    for p in json_files(&root().join("fixtures").join(kind).join("valid")) {
        if let Err(e) = schemas.validate(&read_json(&p), idx) {
            panic!("合法 fixture 被拒：{}\n{e:#}", p.display());
        }
    }
    for p in json_files(&root().join("fixtures").join(kind).join("invalid")) {
        assert!(
            schemas.validate(&read_json(&p), idx).is_err(),
            "非法 fixture 竟然通过了：{}",
            p.display()
        );
    }
}

#[test]
fn proposal_fixtures_match_schema() {
    check_dir("agentear.proposal.v1.schema.json", "proposal");
}

#[test]
fn event_fixtures_match_schema() {
    check_dir("agentear.event.v1.schema.json", "event");
}

#[test]
fn command_fixtures_match_schema() {
    check_dir("agentear.command.v1.schema.json", "command");
}

/// sequences/ 描述的是**宿主**的去重/排序行为（由 Agent24 测），
/// 但里面每一条事件本身都必须是合法事件——否则对方测的是「拒绝坏事件」，不是去重。
#[test]
fn sequence_events_are_each_valid() {
    let (schemas, idx) = compile("agentear.event.v1.schema.json");
    for p in json_files(&root().join("fixtures").join("sequences")) {
        let v = read_json(&p);
        let events = v["events"].as_array().unwrap_or_else(|| panic!("{} 缺 events", p.display()));
        assert!(v["expect"].is_object(), "{} 缺 expect", p.display());
        for e in events {
            if let Err(err) = schemas.validate(e, idx) {
                panic!("{} 里有非法事件：{err:#}", p.display());
            }
        }
    }
}
