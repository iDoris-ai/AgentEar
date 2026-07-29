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

    pub fn transcribe(&self, wav: &Path) -> Result<String> {
        let out = Command::new(&self.bin)
            .arg("-m")
            .arg(&self.model)
            .arg("--vad")
            .arg(&self.vad)
            .arg("-a")
            .arg(wav)
            .output()
            .with_context(|| format!("启动 {} 失败", self.bin.display()))?;

        if !out.status.success() {
            bail!(
                "转写失败 (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // 运行时把转写文本和诊断信息都写到同一个流，需要挑出真正的结果行。
        let merged = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(clean(&merged))
    }
}

/// 从运行时输出里挑出转写文本，并过滤特殊 token。
///
/// M0 实测 `/sil` 之类的 token 会泄漏进输出（`--chunk` 模式尤其明显），
/// 直接粘进剪贴板会很难看。见 `docs/benchmarks.md` §3.4。
fn clean(raw: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() || is_diagnostic(t) {
            continue;
        }
        lines.push(t);
    }
    let text = lines.join("");
    strip_tags(&text)
}

/// 判断是否为运行时诊断输出而非转写结果。
fn is_diagnostic(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sched_reserve", "graph_reserve", "llama_", "ggml_", "load", "print_info",
        "build", "main:", "repack:", "init", "set_abort_callback", "~llama_context",
        "[vad]", "[done]", "system_info", "register_", "get_memory",
    ];
    if PREFIXES.iter().any(|p| line.starts_with(p)) {
        return true;
    }
    // 纯 ASCII 且不含 CJK 的行，绝大多数是日志；转写结果总会带中文或明确的标点
    !line.chars().any(|c| {
        matches!(c as u32, 0x4E00..=0x9FFF)  // CJK 统一汉字
            || matches!(c, '，' | '。' | '？' | '！' | '、' | '：')
    })
}

/// 去掉 `<|...|>` 与 `/sil` 这类特殊标记。
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // 吞掉 <|...|> 形式的标记
            let mut buf = String::from(c);
            let mut closed = false;
            for c2 in chars.by_ref() {
                buf.push(c2);
                if c2 == '>' {
                    closed = true;
                    break;
                }
                if buf.len() > 32 {
                    break;
                }
            }
            if !closed {
                out.push_str(&buf);
            }
            continue;
        }
        out.push(c);
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

    #[test]
    fn strips_sil_tokens() {
        assert_eq!(strip_tags("他在讲什么？/sil/sil能尝尝吗/sil"), "他在讲什么？能尝尝吗");
    }

    #[test]
    fn strips_angle_tags() {
        assert_eq!(strip_tags("<|zh|><|NEUTRAL|>你好世界"), "你好世界");
    }

    #[test]
    fn drops_diagnostic_lines() {
        let raw = "load: loading model\n[vad] 2 segments\n你好，世界。\n[done] 1.0s";
        assert_eq!(clean(raw), "你好，世界。");
    }

    #[test]
    fn keeps_pure_chinese() {
        assert_eq!(clean("我们来测试一下"), "我们来测试一下");
    }
}
