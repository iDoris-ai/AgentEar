//! ASR 引擎适配层（ADR-0007 §4，T3.4.1）。
//!
//! ## 这一层不是新发明的
//!
//! `asr.rs` 本来就在按语言分派两个子进程后端（SenseVoice / whisper 泰语）。
//! 这里做的是把那个既成事实提升成显式的 trait，让**后端可以按配置切换**，
//! 而不是把选择硬编码在 `match` 里。
//!
//! ## 为什么要这一层
//!
//! M3 的通话形态要用一个引擎同时覆盖中/英/泰（实测见 `benchmarks-m3.md`），
//! 而既有的两条链路是按语言分开的。但**换默认引擎需要 jason 单独拍板**
//! （`CLAUDE.md`：「放宽只针对 LLM 这一项，ASR 侧仍按原标准要求」），
//! 所以这里的设计原则是：
//!
//! **新后端以可选项并存，默认值一个字节都不改。**
//!
//! 已发布的 v0.4.2 用户升级上来，行为完全一致——除非他自己去改配置。
//!
//! ## 刻意没有做的事
//!
//! ADR-0007 §4.2 的初稿写过「每个 trait 至少两个实现 + 每个都有零依赖默认实现」，
//! **那条规则是错的，已经推翻**。这里只抽象 ASR：TTS 还在 Arm 手里（T3.3.1），
//! LLM 早就有 `sidecar.rs` 的边界，AEC 只有一个候选（VPIO）。
//! 为了对称而造 no-op 适配器没有意义。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::asr::{Asr, AsrLang, Transcript};

/// 选哪个 ASR 后端。**默认 `Builtin`，与 v0.4.2 行为完全一致。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AsrBackend {
    /// 随包分发的两条链路：SenseVoice（中/英/粤/日/韩）+ whisper（泰语）。
    #[default]
    Builtin,
    /// 外部 `speech` CLI（soniqo/speech-swift）的 Qwen3-ASR，一个模型覆盖中英泰。
    ///
    /// **不随包分发**，要用户自己 `brew install speech`。
    /// 实测数据见 `docs/benchmarks-m3.md`：泰语纯净集 CER 0.0%，
    /// 但 code-switch 那一格没有胜过现有链路，**所以它不是默认值**。
    ///
    /// **资源档位：高资源档**（jason 2026-09-08 拍板，`CLAUDE.md` 已改为分档）。
    /// 实测 Qwen3-ASR 1.7B 单次峰值 RSS **2.43 GiB**，超过默认档的 ≤2 GiB，
    /// 落在高资源档的 ≤4 GiB 内。
    ///
    /// **高资源档的准入三条，改这个后端时必须逐条对照**：
    /// ① 用户显式启用（不能设成默认）；② 不随包分发；③ 峰值 RSS 实测入库。
    ///
    /// ⚠️ 分档**不是**批准把它设成默认引擎——那要看中英横比（T3.5.1），还没做。
    SpeechSwift,
}

impl AsrBackend {
    /// 配置文件里的合法取值。给错误信息用——报错时要能告诉用户可以填什么。
    pub const NAMES: &'static [&'static str] = &["builtin", "speech_swift"];

    /// 从字符串解析。**给 CLI 用，不给配置文件用**——两者的容错策略不同，
    /// 见模块下方 `parse_cli` 的说明。
    pub fn parse_cli(s: &str) -> Result<Self> {
        match s {
            "builtin" => Ok(Self::Builtin),
            "speech_swift" | "speech-swift" => Ok(Self::SpeechSwift),
            other => bail!(
                "未知的 ASR 后端 {other:?}；可选值：{}",
                Self::NAMES.join(" / ")
            ),
        }
    }
}

/// 一个 ASR 后端要能回答的三件事。
pub trait AsrEngine: Send + Sync {
    /// 给日志和菜单看的名字。
    fn name(&self) -> &'static str;

    /// 这个后端支不支持某个识别语言。
    ///
    /// 存在的意义：`Builtin` 的泰语要另外下模型，而 `SpeechSwift` 一个模型全包。
    /// 上层据此决定要不要提示用户下载，而不是等到转写失败再报错。
    fn supports(&self, lang: AsrLang) -> bool;

    /// 依赖自检：二进制在不在、模型下没下。**在真正转写之前调用。**
    ///
    /// 单独拿出来是因为 `--diagnose` 要在不录音的情况下报告环境状态。
    fn preflight(&self, lang: AsrLang) -> Result<()>;

    fn transcribe(&self, wav: &Path, lang: AsrLang) -> Result<Transcript>;
}

/// 随包分发的内置链路。就是原来的 `asr::Asr`，一行逻辑都没改。
pub struct BuiltinEngine {
    inner: Asr,
}

impl BuiltinEngine {
    pub fn new(vendor: &Path) -> Result<Self> {
        Ok(Self { inner: Asr::new(vendor)? })
    }
}

impl AsrEngine for BuiltinEngine {
    fn name(&self) -> &'static str {
        "builtin"
    }

    fn supports(&self, _lang: AsrLang) -> bool {
        // 两种语言都支持，泰语的模型缺失由 preflight 报告，不在这里判。
        true
    }

    fn preflight(&self, lang: AsrLang) -> Result<()> {
        if lang == AsrLang::Thai && !crate::download::is_installed(&crate::download::THAI) {
            bail!("泰语模型还没下载，先跑 --fetch-thai");
        }
        Ok(())
    }

    fn transcribe(&self, wav: &Path, lang: AsrLang) -> Result<Transcript> {
        self.inner.transcribe(wav, lang)
    }
}

/// 外部 `speech` CLI 后端。
///
/// ## 为什么走 CLI 而不是链接它的 Swift 库
///
/// 上游是 **v0.0.x，API 不保证稳定**（发布历史里 v0.0.24 打包次日就被 v0.0.25
/// 取代）。进程边界把这个风险挡在外面——沿用 ADR-0002 「独立进程 + 明确协议
/// 边界」的既有纪律。
///
/// ⚠️ **但要清楚这道边界挡不住什么**：它隔离的是 Swift ABI，
/// 隔离不了 CLI 参数和 stdout 格式的变化。所以 `preflight` 里做版本探测，
/// 不兼容时**拒绝启动并说清楚**，而不是在运行中静默出错。
pub struct SpeechSwiftEngine {
    bin: PathBuf,
    /// 术语表渲染出的 context 字符串。空字符串 = 不传 `--context`。
    ///
    /// 实测（`benchmarks-m3.md` §2.1）：给它 terms.json 里的拉丁词，
    /// 英文词命中从 36% 提到 44%，而且**纯泰语不退化**。
    context: String,
}

impl SpeechSwiftEngine {
    /// `data_root` 用来读 `terms.json`。传 `None` 表示不加 context。
    pub fn new(data_root: Option<&Path>) -> Self {
        let context = data_root.map(latin_context_from_terms).unwrap_or_default();
        Self { bin: PathBuf::from("speech"), context }
    }

    /// Qwen3-ASR 的语言提示。
    ///
    /// `Auto` 时**故意不传** `--language`：Qwen3-ASR 自带语种判别，
    /// 硬塞一个语言反而会压制它的判断。这正是选它的理由之一——
    /// 「通话中随时切语言」不需要用户去菜单里切。
    fn lang_flag(lang: AsrLang) -> Option<&'static str> {
        match lang {
            AsrLang::Auto => None,
            AsrLang::Thai => Some("th"),
        }
    }
}

impl AsrEngine for SpeechSwiftEngine {
    fn name(&self) -> &'static str {
        "speech_swift"
    }

    fn supports(&self, _lang: AsrLang) -> bool {
        true
    }

    fn preflight(&self, _lang: AsrLang) -> Result<()> {
        let out = Command::new(&self.bin).arg("--help").output().with_context(|| {
            format!(
                "找不到 `{}`。speech-swift 不随包分发，先装：brew install speech",
                self.bin.display()
            )
        })?;
        if !out.status.success() {
            bail!("`speech --help` 返回非零，装的可能不是 speech-swift");
        }
        let top = String::from_utf8_lossy(&out.stdout);
        if !top.contains("transcribe") {
            bail!("`speech` 没有 transcribe 子命令，版本不兼容");
        }

        // 契约探测要探**我们真正依赖的每一个参数**，不能只看子命令在不在。
        //
        // 初版只检查顶层 help 里出现过 "transcribe" 就算通过，那等于没探——
        // 上游把 `--context` 改名的话照样过检，然后在运行时静默拿到更差的结果。
        // 进程边界隔离的是 Swift ABI，**隔离不了 CLI 参数的变化**，
        // 所以这道检查是那个风险的唯一防线。
        let sub = Command::new(&self.bin)
            .arg("transcribe")
            .arg("--help")
            .output()
            .with_context(|| format!("探测 {} transcribe --help 失败", self.bin.display()))?;
        let h = String::from_utf8_lossy(&sub.stdout);
        for need in ["--engine", "--language", "--context"] {
            if !h.contains(need) {
                bail!(
                    "`speech transcribe` 不认 {need}，上游 CLI 协议已变；\
                     本项目验证过的版本是 v0.0.26"
                );
            }
        }
        Ok(())
    }

    fn transcribe(&self, wav: &Path, lang: AsrLang) -> Result<Transcript> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("transcribe")
            .arg("--engine")
            .arg("qwen3")
            // 1.7B 而不是 0.6B：实测 0.6B 三项指标都退
            //（纯泰语 0.8%→2.4%，英文词命中 36%→28%）。
            .arg("-m")
            .arg("1.7B");
        if let Some(l) = Self::lang_flag(lang) {
            cmd.arg("--language").arg(l);
        }
        if !self.context.is_empty() {
            // `--context=VALUE` 而不是两个参数：术语表是用户可编辑的，
            // 里头出现 `--help` 这样的词时，分开传会被下游当成选项而不是值。
            cmd.arg(format!("--context={}", self.context));
        }
        // `--` 之后一律当路径。否则名字以 `-` 开头的合法文件会被当成选项。
        cmd.arg("--").arg(wav);

        let out = cmd
            .output()
            .with_context(|| format!("启动 {} 失败", self.bin.display()))?;
        if !out.status.success() {
            bail!(
                "speech transcribe 失败 (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        parse_speech_output(&String::from_utf8_lossy(&out.stdout))
    }
}

/// 解析 `speech transcribe` 的 stdout。
///
/// 它把进度和模型加载信息也打在 stdout 上（`[80%] Downloading weights...`），
/// **结果行以 `Result: ` 开头**。所以判据是这个前缀，不是「最后一行」——
/// 后者在有 `Time: ... RTF: ...` 尾行时会拿错。
///
/// 这和 `asr.rs` 里两套解析的教训是同一条：**每个后端的输出协议都要单独钉住**，
/// 不要靠「看起来像正文」这类启发式。SenseVoice 那边靠 `<|zh|>` 标记，
/// whisper 那边靠 `-np -nt` 之后的非空行，这里靠 `Result: ` 前缀。
fn parse_speech_output(stdout: &str) -> Result<Transcript> {
    // 尾部元数据行，出现即表示结果已经结束。
    fn is_trailer(t: &str) -> bool {
        t.starts_with("Time:") || t.starts_with("RTF:")
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for line in stdout.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Result:") {
            // 上一段收尾，开新的一段。多个 Result 会被收集起来，
            // 用来在下面报错——不是静默丢掉。
            if let Some(c) = cur.take() {
                chunks.push(c.join("\n"));
            }
            cur = Some(vec![rest.trim().to_string()]);
        } else if let Some(c) = cur.as_mut() {
            // 结果的续行：空行和尾部元数据结束这一段，其余算正文。
            //
            // ⚠️ 只取首行会**静默截断多行转写**。静默截断比报错难查得多——
            // 用户拿到半句话，而日志里什么都没有。
            if t.is_empty() || is_trailer(t) {
                chunks.push(c.join("\n"));
                cur = None;
            } else {
                c.push(t.to_string());
            }
        }
    }
    if let Some(c) = cur {
        chunks.push(c.join("\n"));
    }

    match chunks.len() {
        // 没有 Result 行 = 协议不对（上游改了输出格式），**必须报错**。
        // 退回空转写会让用户以为「没听见」，而真正的原因是我们看不懂它的输出了。
        0 => bail!(
            "speech 的输出里没有 `Result:` 行，输出协议可能变了；原始输出：\n{}",
            stdout.trim()
        ),
        1 => Ok(Transcript { text: chunks.remove(0), lang: None }),
        n => bail!("speech 返回了 {n} 段 Result，无法确定用哪一段"),
    }
}

/// 从 `terms.json` 取纯拉丁词，拼成 `--context`。
///
/// **只取纯拉丁的**：含中文的 alias（`我的妈book`）对 Qwen3-ASR 的
/// 「保持拉丁书写」提示没有帮助，只是白占 token。
///
/// 上限 40 个词。`RESULTS.md` 实测过 whisper 侧的长度拐点：
/// 20–40 词稳定有收益，85 词往后明确有害，100 词能把纯泰语 CER 从 3.9% 打到 22.2%。
/// ⚠️ **那条曲线是 whisper 的，Qwen3-ASR 侧还没测**——这里沿用同一个上限
/// 是保守取值，不是实测结论。`terms.json` 是用户会自己编辑的文件，
/// 只会越来越长，不设上限的话某天会毫无征兆地变差。
fn latin_context_from_terms(data_root: &Path) -> String {
    const MAX_WORDS: usize = 40;
    let terms = crate::terms::load(data_root);
    let mut entries: Vec<String> = Vec::new();
    // ⚠️ 数的是**空白分词后的词数**，不是条目数。
    // `knowledge base` / `Mac mini` 这类多词条目各占 2 个词，
    // 按条目数计会让真实 prompt 长度悄悄超出 40，而那正是要设上限的东西。
    let mut count = 0usize;
    for t in &terms.terms {
        for w in std::iter::once(&t.canonical).chain(t.aliases.iter()) {
            if !is_pure_latin(w) || entries.iter().any(|x| x == w) {
                continue;
            }
            let n = w.split_whitespace().count();
            // 整条加或整条不加，不切一半——半个术语给不出「保持拉丁书写」的提示。
            if count + n > MAX_WORDS {
                return entries.join(", ");
            }
            count += n;
            entries.push(w.clone());
        }
    }
    entries.join(", ")
}

/// 至少有一个拉丁字母，且**不含任何非拉丁文字**。
///
/// ## 两个边界是踩出来的
///
/// 1. **只认 ASCII 字母会自相矛盾**：`é` 被拒，而 `café` 因为含 `caf` 被接受。
///    所以拉丁扩展区（`À`–`ſ`）也算拉丁字母。
/// 2. **只排基本 CJK 区不够**：`abc𠀀`（扩展 B）、`abc㐀`（扩展 A）都会漏网。
///    改成**白名单**——只允许拉丁字母、数字和少数标点，其余一律拒绝。
///    白名单在这里比黑名单安全：漏掉一个文字区间的代价是往 prompt 里塞进
///    非拉丁字符，而那正是这个函数要挡的东西。
fn is_pure_latin(s: &str) -> bool {
    let mut has_latin = false;
    for c in s.chars() {
        let latin_letter = c.is_ascii_alphabetic()
            || matches!(c, '\u{00C0}'..='\u{024F}'); // 拉丁-1 补充 + 扩展 A/B
        if latin_letter {
            has_latin = true;
        } else if !(c.is_ascii_digit() || matches!(c, ' ' | '-' | '.' | '_' | '+' | '/' | '#')) {
            return false;
        }
    }
    has_latin
}

/// 按后端选择造一个引擎。**配置切换走这里，不需要重新编译。**
pub fn build(
    backend: AsrBackend,
    vendor: &Path,
    data_root: Option<&Path>,
) -> Result<Box<dyn AsrEngine>> {
    match backend {
        AsrBackend::Builtin => Ok(Box::new(BuiltinEngine::new(vendor)?)),
        AsrBackend::SpeechSwift => Ok(Box::new(SpeechSwiftEngine::new(data_root))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验收判据之一：**未知后端名要给出可操作的错误**，
    /// 而不是静默退回默认让用户以为切成功了。
    ///
    /// 注意这只针对 **CLI 显式指定**。配置文件里的未知值走 `lenient`
    /// 反序列化退回默认（与 `ui_lang` / `asr_lang` 一致）——两者容错策略
    /// 不同是有意的：改配置文件是低频动作、错了要能启动；
    /// 敲命令行是当场行为、错了就该当场说。
    #[test]
    fn unknown_backend_name_is_actionable() {
        let err = AsrBackend::parse_cli("klingon").unwrap_err().to_string();
        assert!(err.contains("klingon"), "错误信息要带上用户输入的值：{err}");
        for name in AsrBackend::NAMES {
            assert!(err.contains(name), "错误信息要列出可选值 {name}：{err}");
        }
    }

    #[test]
    fn backend_names_round_trip() {
        for name in AsrBackend::NAMES {
            let b = AsrBackend::parse_cli(name).expect("NAMES 里的名字必须能解析");
            assert_eq!(
                serde_json::to_value(b).unwrap().as_str().unwrap(),
                *name,
                "parse_cli 与 serde 的名字必须一致，否则配置文件和命令行会对不上"
            );
        }
    }

    /// 连字符写法也收：命令行上 `speech-swift` 比 `speech_swift` 顺手。
    #[test]
    fn hyphenated_alias_is_accepted() {
        assert_eq!(AsrBackend::parse_cli("speech-swift").unwrap(), AsrBackend::SpeechSwift);
    }

    #[test]
    fn default_backend_is_builtin() {
        assert_eq!(
            AsrBackend::default(),
            AsrBackend::Builtin,
            "默认必须是 builtin —— 已发布用户升级上来行为不能变"
        );
    }

    /// 验收判据之二：**切换后端不需要重新编译**，
    /// 同一份二进制按参数造出不同的引擎。
    #[test]
    fn build_dispatches_by_backend_without_recompiling() {
        let tmp = std::env::temp_dir().join("agentear-engine-test");
        // Builtin 需要 vendor 里的真实文件，这里只验证 SpeechSwift 这条分支
        // 能在没有任何 vendor 文件的情况下造出来——它本来就不依赖 vendor。
        let e = build(AsrBackend::SpeechSwift, &tmp, None).expect("speech_swift 不该依赖 vendor");
        assert_eq!(e.name(), "speech_swift");
    }

    /// 多行转写不能被静默截断——用户拿到半句话而日志里什么都没有，
    /// 是最难查的一类问题。
    #[test]
    fn multiline_result_is_kept_whole() {
        let stdout = "Result: 第一行\n第二行\n  Time: 3.12s, RTF: 0.860\n";
        let t = parse_speech_output(stdout).unwrap();
        assert_eq!(t.text, "第一行\n第二行", "续行不能丢");
    }

    /// 没有 Result 行 = 上游改了输出协议，**必须报错**。
    /// 退回空转写会让用户以为「没听见」，而真正的原因是我们看不懂它的输出了。
    #[test]
    fn missing_result_marker_is_an_error_not_empty_text() {
        let err = parse_speech_output("  [50%] loading\n").unwrap_err().to_string();
        assert!(err.contains("Result"), "错误要点明协议问题：{err}");
    }

    #[test]
    fn multiple_result_markers_are_an_error() {
        let stdout = "Result: 甲\n\nResult: 乙\n";
        assert!(parse_speech_output(stdout).is_err(), "两段结果无法确定用哪段");
    }

    /// 40 的口径是**词数**不是条目数：`knowledge base` 占 2 个词。
    /// 按条目数计会让真实 prompt 悄悄超出上限，而那正是要限制的东西。
    #[test]
    fn context_budget_counts_words_not_entries() {
        let dir = std::env::temp_dir().join(format!("agentear-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let many: Vec<String> = (0..30).map(|i| format!("alpha{i} beta{i}")).collect();
        let json = serde_json::json!({
            "version": 2,
            "terms": many.iter().map(|c| serde_json::json!({"canonical": c})).collect::<Vec<_>>(),
        });
        std::fs::write(crate::terms::path_in(&dir), serde_json::to_string(&json).unwrap()).unwrap();
        let ctx = latin_context_from_terms(&dir);
        let n = ctx.split_whitespace().count();
        assert!(n <= 40, "真实词数必须 ≤40，实得 {n}：{ctx}");
        assert!(n >= 30, "也不该过度截断，实得 {n}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_result_line_not_last_line() {
        // 真实输出形态：进度行 + 结果行 + 计时尾行。
        let stdout = "  [80%] Downloading weights...\n  [100%] Ready\nTranscribing...\nResult: 今天天气不错\n  Time: 3.12s, RTF: 0.860\n";
        let t = parse_speech_output(stdout).unwrap();
        assert_eq!(t.text, "今天天气不错", "取的必须是 Result: 行，不是最后一行");
    }

    #[test]
    fn empty_output_is_an_error() {
        assert!(parse_speech_output("").is_err());
    }

    #[test]
    fn latin_filter_rejects_mixed_scripts() {
        assert!(is_pure_latin("Kubernetes"));
        assert!(is_pure_latin("Mac mini"));
        assert!(is_pure_latin("ESP32"));
        // 混了中文的 alias 对「保持拉丁书写」的提示没用，只占 token
        assert!(!is_pure_latin("我的妈book"));
        assert!(!is_pure_latin("闹铃是base"));
        // 纯中文、纯泰文都不要
        assert!(!is_pure_latin("术语"));
        assert!(!is_pure_latin("สวัสดี"));
        // 没有拉丁字母的也不要
        assert!(!is_pure_latin("24"));
        // 带音标的拉丁要收——只认 ASCII 会自相矛盾：
        // `é` 被拒，而 `café` 因为含 `caf` 反而被接受。
        assert!(is_pure_latin("café"));
        assert!(is_pure_latin("é"));
        // CJK 扩展区也要挡住（只排基本区会漏）
        assert!(!is_pure_latin("abc㐀"), "CJK 扩展 A 要挡住");
        assert!(!is_pure_latin("abc\u{20000}"), "CJK 扩展 B 要挡住");
        assert!(!is_pure_latin("abcあ"), "假名要挡住");
    }

    /// `Auto` 不传 `--language`：Qwen3-ASR 自带语种判别，
    /// 硬塞语言会压制它，而「通话中随时切语言」正靠这个能力。
    #[test]
    fn auto_lang_sends_no_language_flag() {
        assert_eq!(SpeechSwiftEngine::lang_flag(AsrLang::Auto), None);
        assert_eq!(SpeechSwiftEngine::lang_flag(AsrLang::Thai), Some("th"));
    }
}
