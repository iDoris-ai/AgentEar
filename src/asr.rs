//! 调用 SenseVoiceSmall 做转写。
//!
//! 选型依据：`docs/decisions/0001-asr-model-selection.md`。
//!
//! 关键设计：SenseVoice 冷启动仅 0.2s（含加载 242 MiB 权重），所以**不需要常驻
//! 模型服务**——每次录音直接起一个子进程即可。这是换掉 Fun-ASR-Nano 换来的
//! 简化（Nano 冷启动 11.45s，必须常驻）。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 单段送入 ASR 的时长上限。
///
/// M0 实测 RSS 随音频长度线性增长约 0.52 MB/s，约 54 分钟破 2 GiB。
/// M1 的快捷键录音通常只有几十秒，这里只作为兜底护栏。
pub const MAX_SEGMENT_SECS: f32 = 300.0;

pub struct Asr {
    bin: PathBuf,
    model: PathBuf,
    vad: PathBuf,
}

/// 一次转写的产物。
pub struct Transcript {
    /// 去掉全部标记后的正文。
    pub text: String,
    /// 模型判定的语种（首个 `<|xx|>` 标记），如 `zh` / `en`。无语音时为 `None`。
    ///
    /// 注意这是**模型的判断**，不一定对——实测泰语会被误判成 `en`。
    /// M2 的术语纠错可以拿它选择术语表。
    pub lang: Option<String>,
}

impl Asr {
    pub fn new(vendor: &Path) -> Result<Self> {
        let a = Self {
            bin: vendor.join("bin/llama-funasr-sensevoice"),
            model: vendor.join("models/sensevoice-small-q8.gguf"),
            vad: vendor.join("models/fsmn-vad.gguf"),
        };
        for p in [&a.bin, &a.model, &a.vad] {
            if !p.exists() {
                bail!("缺少 ASR 依赖文件: {}", p.display());
            }
        }
        Ok(a)
    }

    pub fn transcribe(&self, wav: &Path) -> Result<Transcript> {
        let out = Command::new(&self.bin)
            .arg("-m")
            .arg(&self.model)
            .arg("--vad")
            .arg(&self.vad)
            .arg("-a")
            .arg(wav)
            // 必须保留标记：`<|zh|>` / `<|en|>` 是区分「转写结果」与「日志」的唯一
            // 可靠判据。曾经按「有没有汉字」来判，把英文/日文/泰文结果全当日志丢了。
            .arg("--keep-tags")
            .output()
            .with_context(|| format!("启动 {} 失败", self.bin.display()))?;

        if !out.status.success() {
            bail!(
                "转写失败 (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        Ok(clean(&stdout, &stderr))
    }
}

/// 从运行时输出里挑出转写文本，并过滤特殊 token。
///
/// 判据是**带不带 `<|...|>` 标记**，不是「有没有汉字」。实测转写结果走 stdout、
/// 日志走 stderr，但历史上出现过混流，所以两条流都扫一遍——标记的存在与否足够
/// 区分，不依赖流的划分。
///
/// M0 实测 `/sil` 之类的 token 也会泄漏进输出（`--chunk` 模式尤其明显），
/// 直接粘进剪贴板会很难看。见 `docs/benchmarks.md` §3.4。
fn clean(stdout: &str, stderr: &str) -> Transcript {
    let mut lang = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let t = line.trim();
        let tags = tag_names(t);
        if tags.is_empty() {
            continue;
        }
        if lang.is_none() {
            lang = tags.iter().find(|n| is_lang(n)).map(|n| n.to_string());
        }
        kept.push(t);
    }

    if kept.is_empty() {
        // 兜底：万一将来的构建不再输出标记，也不能把结果全丢掉。
        // 只扫 stdout（转写结果所在的流），按已知日志前缀过滤。
        let fallback: Vec<&str> = stdout
            .lines()
            .map(str::trim)
            .filter(|t| !t.is_empty() && !is_diagnostic(t))
            .collect();
        if !fallback.is_empty() {
            log::warn!("ASR 输出没有 <|lang|> 标记，退回前缀过滤——检查 --keep-tags 是否仍被支持");
        }
        return Transcript {
            text: strip_tags(&fallback.join("")),
            lang: None,
        };
    }

    Transcript {
        text: strip_tags(&kept.join("")),
        lang,
    }
}

/// SenseVoice 会输出的语种标记。泰语等不在其中，会被误判成这里的某一个。
fn is_lang(name: &str) -> bool {
    matches!(name, "zh" | "en" | "yue" | "ja" | "ko" | "nospeech")
}

/// 匹配 `s[start..]` 开头的 `<|名字|>`，返回 (名字, 标记结束的字节下标)。
///
/// 刻意收紧：名字非空、不超过 24 字节、不含尖括号。正文里出现孤立的 `<`
/// 或 `<|` 不会被误吞。
fn tag_at(s: &str, start: usize) -> Option<(&str, usize)> {
    let body = s[start..].strip_prefix("<|")?;
    let end = body.find("|>")?;
    let name = &body[..end];
    if name.is_empty() || name.len() > 24 || name.contains(['<', '>']) {
        return None;
    }
    Some((name, start + 2 + end + 2))
}

/// 一行里所有 `<|...|>` 标记的名字，按出现顺序。
fn tag_names(s: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if let Some((name, next)) = tag_at(s, i) {
            names.push(name);
            i = next;
        } else {
            // 按字符步进，保证 i 永远落在字符边界上
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    names
}

/// 判断是否为运行时诊断输出而非转写结果。**只用于无标记时的兜底路径。**
fn is_diagnostic(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sched_reserve", "graph_reserve", "llama_", "ggml_", "load", "print_info",
        "build", "main:", "repack:", "init", "set_abort_callback", "~llama_context",
        "[vad]", "[done]", "[sensevoice]", "system_info", "register_", "get_memory",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

/// 去掉 `<|...|>` 与 `/sil` 这类特殊标记。
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some((_, next)) = tag_at(s, i) {
            i = next;
            continue;
        }
        let c = s[i..].chars().next().expect("i 在字符边界上");
        out.push(c);
        i += c.len_utf8();
    }
    out.replace("/sil", "")
        .replace("/eos", "")
        .replace("/bos", "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--keep-tags` 下每段转写的固定前缀。
    fn tagged(lang: &str, body: &str) -> String {
        format!("<|{lang}|><|NEUTRAL|><|Speech|><|withitn|>{body}")
    }

    const LOG: &str = "[sensevoice] 1 vad segments\n[sensevoice] done 0.37s";

    #[test]
    fn strips_sil_tokens() {
        assert_eq!(strip_tags("他在讲什么？/sil/sil能尝尝吗/sil"), "他在讲什么？能尝尝吗");
    }

    #[test]
    fn strips_angle_tags() {
        assert_eq!(strip_tags("<|zh|><|NEUTRAL|>你好世界"), "你好世界");
    }

    #[test]
    fn keeps_lone_angle_bracket() {
        // 正文里的 `<` 不是标记，不能被吞掉
        assert_eq!(strip_tags("if a < b then"), "if a < b then");
        assert_eq!(strip_tags("未闭合 <|zh 后面还有正文"), "未闭合 <|zh 后面还有正文");
    }

    #[test]
    fn drops_diagnostic_lines() {
        let t = clean(&tagged("zh", "你好，世界。"), LOG);
        assert_eq!(t.text, "你好，世界。");
        assert_eq!(t.lang.as_deref(), Some("zh"));
    }

    /// 回归：英文结果一个汉字都没有，旧的「无汉字即日志」判据会整段丢掉它。
    #[test]
    fn keeps_pure_english() {
        let t = clean(
            &tagged("en", "Hello, I am testing the English recognition."),
            LOG,
        );
        assert_eq!(t.text, "Hello, I am testing the English recognition.");
        assert_eq!(t.lang.as_deref(), Some("en"));
    }

    /// 回归：日/韩有汉字或全无汉字都可能，同样不能靠字符集判断。
    #[test]
    fn keeps_japanese_and_korean() {
        assert_eq!(clean(&tagged("ja", "こんにちは、テストです"), LOG).text, "こんにちは、テストです");
        assert_eq!(clean(&tagged("ko", "안녕하세요"), LOG).text, "안녕하세요");
    }

    /// 回归：泰语 SenseVoice 认不准（会误判成 en）、输出是乱码，但那是模型的问题。
    /// 解析层的职责是**如实交出模型说了什么**，不是替它判断对错。
    #[test]
    fn keeps_thai_even_when_misdetected() {
        let t = clean(&tagged("en", "Satly cr passa tie"), LOG);
        assert_eq!(t.text, "Satly cr passa tie");
        assert_eq!(t.lang.as_deref(), Some("en"));
    }

    /// 多段 VAD 时，运行时把各段拼在同一行，每段各带一套标记。
    #[test]
    fn joins_multiple_segments() {
        let line = format!("{}{}", tagged("en", "First segment."), tagged("en", "Second segment."));
        let t = clean(&line, LOG);
        assert_eq!(t.text, "First segment.Second segment.");
        assert_eq!(t.lang.as_deref(), Some("en"));
    }

    /// 静音：运行时只吐日志，没有带标记的行。
    #[test]
    fn silence_yields_empty() {
        let t = clean("", LOG);
        assert!(t.text.is_empty());
        assert_eq!(t.lang, None);
    }

    /// 历史上转写结果混进过 stderr，两条流都要扫。
    #[test]
    fn finds_result_on_stderr() {
        let t = clean("", &format!("{LOG}\n{}", tagged("zh", "混流的结果")));
        assert_eq!(t.text, "混流的结果");
    }

    /// 兜底路径：若将来不再输出标记，也不能把结果整段丢掉。
    #[test]
    fn falls_back_when_untagged() {
        let t = clean("[sensevoice] done 0.4s\nplain english output", LOG);
        assert_eq!(t.text, "plain english output");
        assert_eq!(t.lang, None);
    }
}
