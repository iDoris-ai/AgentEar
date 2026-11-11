//! T3.5.5：钉住「上游外部依赖都锁到了具体版本」。
//!
//! 清单与回滚流程见 `docs/upstream-pins.md`。这里挡的是回退：谁顺手把
//! `resolve/<commit>/` 改回 `resolve/main/`、把 `pkg==1.2.3` 改回 `pkg`，
//! 编译和功能都不会报错，只是「同一条命令」从此在不同日子装到不同的东西。

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("读不了 {}: {e}", path.display()))
}

fn rust_sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push((p.display().to_string(), fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 非注释行（脚本里 `#` 开头、Rust 里 `//` 开头）。
fn code_lines(body: &str, comment: &str) -> Vec<String> {
    body.lines()
        .filter(|l| !l.trim_start().starts_with(comment))
        .map(str::to_string)
        .collect()
}

const PINNED_SCRIPTS: [&str; 4] = [
    "scripts/setup-talk.sh",
    "scripts/setup-llm.sh",
    "scripts/build-thai-model.sh",
    "scripts/build-speech-runtime.sh",
];

/// 源码和下载脚本里不许出现跟着分支走的 URL。
#[test]
fn no_download_follows_a_moving_branch() {
    for (path, body) in rust_sources() {
        for l in code_lines(&body, "//") {
            assert!(!l.contains("resolve/main"), "{path} 用了 resolve/main：{l}");
        }
    }
    for s in PINNED_SCRIPTS {
        for l in code_lines(&read(s), "#") {
            assert!(!l.contains("resolve/main"), "{s} 用了 resolve/main：{l}");
            assert!(
                !l.contains("raw.githubusercontent.com") || !l.contains("/main/"),
                "{s} 从分支取原始文件：{l}"
            );
            assert!(!l.contains("/api/models/"), "{s} 还在向 HF API 要 HEAD revision：{l}");
        }
    }
}

/// Rust 里每一个 HF URL 都是 `resolve/<完整 commit>/`，每个 revision 常量都是 40 位。
#[test]
fn every_hf_url_in_rust_is_pinned_to_a_full_commit() {
    let mut seen = 0;
    for (path, body) in rust_sources() {
        for l in body.lines().filter(|l| l.contains("huggingface.co/")) {
            if let Some(i) = l.find("/resolve/") {
                let rest = &l[i + "/resolve/".len()..];
                let rev = rest.split('/').next().unwrap();
                // `{}` 是 qwen3.rs 里用 revision() 拼 URL 的格式化占位
                assert!(rev == "{}" || is_full_sha(rev), "{path}：{l}");
                seen += 1;
            }
        }
    }
    assert!(seen >= 5, "应当至少看到 builtin 4 条 + qwen3 1 条 HF URL，实际 {seen}");
    let q = read("src/qwen3.rs");
    let start = q.find("fn revision(self)").expect("qwen3.rs 要有 revision()");
    let body = &q[start..q[start..].find("pub fn files").map(|i| start + i).unwrap()];
    let revs: Vec<&str> = body.split('"').skip(1).step_by(2).collect();
    assert_eq!(revs.len(), 2, "qwen3 两档 revision：{revs:?}");
    for rev in revs {
        assert!(is_full_sha(rev), "qwen3 revision 不是完整 40 位：{rev}");
    }
}

/// talk 边车：模型 `hf download --revision <40 位>`，不再下 main 再事后问 API。
#[test]
fn setup_talk_pins_model_revisions_and_pip() {
    let body = read("scripts/setup-talk.sh");
    assert!(body.contains("--revision \"$rev\""), "hf download 必须带 --revision");
    for name in ["LLM_REV_DEFAULT", "TTS_REV_DEFAULT"] {
        assert!(body.contains(name), "缺 {name}");
    }
    let mut shas = 0;
    for l in code_lines(&body, "#") {
        if l.contains("_REV_DEFAULT=") {
            let v = l.split('"').nth(1).unwrap();
            assert!(is_full_sha(v), "默认 revision 不是完整 40 位：{l}");
            shas += 1;
        }
    }
    assert_eq!(shas, 3, "LLM 1 个 + TTS 4bit/8bit 2 个");
    assert_pip_pins(&body, "scripts/setup-talk.sh");
}

#[test]
fn setup_llm_pins_model_revision_and_pip() {
    let body = read("scripts/setup-llm.sh");
    let line = code_lines(&body, "#")
        .into_iter()
        .find(|l| l.contains("HF_REV=\"${AGENTEAR_LLM_REV:-"))
        .expect("setup-llm.sh 要有钉死的 HF_REV 常量");
    let v = line.split(":-").nth(1).unwrap().trim_end_matches("}\"");
    assert!(is_full_sha(v), "HF_REV 不是完整 40 位：{line}");
    assert_pip_pins(&body, "scripts/setup-llm.sh");
}

/// 每个 pip/uv 安装的包都必须是 `name==版本`（`${PIP_PINS[@]}` 展开的那一行除外，
/// 它的内容由 PIP_PINS 数组这一行检查）。
fn assert_pip_pins(body: &str, script: &str) {
    for l in code_lines(body, "#") {
        if l.contains("PIP_PINS=(") {
            let inner = l.split('(').nth(1).unwrap().split(')').next().unwrap();
            for pkg in inner.split_whitespace() {
                assert!(pkg.contains("=="), "{script} PIP_PINS 里 {pkg} 没钉版本");
            }
        }
        if l.contains("pip install") && !l.contains("--upgrade pip") && !l.contains("PIP_PINS") {
            for tok in l.split_whitespace().skip_while(|t| *t != "install").skip(1) {
                if tok.starts_with('-') || tok.starts_with('"') || tok.starts_with('$') {
                    continue;
                }
                assert!(tok.contains("=="), "{script}：{tok} 没钉版本：{l}");
            }
        }
    }
}

/// 泰语模型构建：HF commit 与 mel_filters 都钉死，且带文件 sha256 校验。
#[test]
fn thai_build_pins_hf_commit_and_mel_filters() {
    let body = read("scripts/build-thai-model.sh");
    let rev = code_lines(&body, "#")
        .into_iter()
        .find(|l| l.starts_with("HF_REV=\""))
        .expect("缺 HF_REV");
    assert!(is_full_sha(rev.split('"').nth(1).unwrap()), "{rev}");
    for name in ["MEL_COMMIT", "MEL_SHA256"] {
        let l = code_lines(&body, "#")
            .into_iter()
            .find(|l| l.starts_with(&format!("{name}=\"")))
            .unwrap_or_else(|| panic!("缺 {name}"));
        let v = l.split('"').nth(1).unwrap();
        assert!(v.len() == 40 || v.len() == 64, "{l}");
    }
    assert!(body.contains("shasum -a 256 \"$MEL\""), "mel_filters.npz 下载后必须校验 sha256");
}

/// speech 运行时构建：上游 commit 完整 40 位，官方 tarball 带 sha256。
#[test]
fn speech_runtime_build_pins_upstream() {
    let body = read("scripts/build-speech-runtime.sh");
    for name in ["UPSTREAM_COMMIT", "OFFICIAL_SHA256"] {
        let l = code_lines(&body, "#")
            .into_iter()
            .find(|l| l.starts_with(&format!("{name}=\"")))
            .unwrap_or_else(|| panic!("缺 {name}"));
        let v = l.split('"').nth(1).unwrap();
        assert!(v.len() == if name == "UPSTREAM_COMMIT" { 40 } else { 64 }, "{l}");
    }
}

/// 回滚文档存在，且提到回滚要改的那几个常量。
#[test]
fn rollback_doc_names_the_constants_to_change() {
    let doc = read("docs/upstream-pins.md");
    for needle in ["SPEECH_VERSION", "RUNTIME_URL", "RUNTIME_SHA256", "RUNTIME_BYTES", "runtime/speech-"] {
        assert!(doc.contains(needle), "docs/upstream-pins.md 回滚一节要提到 {needle}");
    }
}
