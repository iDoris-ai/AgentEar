//! 调用 SenseVoiceSmall 做转写。
//!
//! 选型依据：`docs/decisions/0001-asr-model-selection.md`。
//!
//! 关键设计：SenseVoice 冷启动仅 0.2s（含加载 242 MiB 权重），所以**不需要常驻
//! 模型服务**——每次录音直接起一个子进程即可。这是换掉 Fun-ASR-Nano 换来的
//! 简化（Nano 冷启动 11.45s，必须常驻）。
//!
//! ## 两条链路，不是一条链路的两个参数
//!
//! 泰语走的是**完全不同的引擎**：`whisper-cli` + Thonburian 微调模型
//! （`docs/decisions/0004-thai-asr-engine.md`）。原因不是「SenseVoice 泰语
//! 差」，而是 `llama-funasr-sensevoice` 的语种集合里**根本没有 `th`**
//! （只有 zh/en/yue/ja/ko/nospeech），它永远不可能输出泰语。
//!
//! 两条链路的输出格式毫不相干，所以解析也是两套：SenseVoice 靠 `<|zh|>`
//! 这类标记区分结果与日志（`clean`），whisper 靠 `-np -nt` 把输出压成纯文本
//! （`clean_whisper`）。**不要试图合并它们**——合并意味着放松判据，而放松
//! 判据的后果是日志被当成转写敲进用户的窗口。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// vendor 目录。`Asr` 实例被 move 进了工作线程，而「下载完成后跑一次加载
/// 冒烟」发生在下载线程上，两边碰不着面——所以路径单独存一份。
static VENDOR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_vendor(p: PathBuf) {
    VENDOR.set(p).ok();
}

/// 单段送入 ASR 的时长上限。
///
/// M0 实测 RSS 随音频长度线性增长约 0.52 MB/s，约 54 分钟破 2 GiB。
/// M1 的快捷键录音通常只有几十秒，这里只作为兜底护栏。
pub const MAX_SEGMENT_SECS: f32 = 300.0;

/// 识别语言的选择。**和界面语言（`i18n::Lang`）是两回事**：
/// 界面切成泰文不会改变走哪个 ASR 引擎，识别切成泰语也不会改变菜单文字。
///
/// 为什么泰语要**用户显式选**，而不是自动路由：泰语是低频场景，为它引入
/// 第二个常驻语种判别器和额外延迟不划算。这是**产品决策**，
/// 不是「实测证明只能这样」——ADR-0004 §1 特意把这一点写清楚了，
/// 自动路由列为后续实验项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AsrLang {
    /// SenseVoice：中 / 英 / 粤 / 日 / 韩，模型自己判语种。
    #[default]
    Auto,
    /// whisper + 泰语微调模型。需要先下载模型（`crate::download`）。
    Thai,
}

pub struct Asr {
    bin: PathBuf,
    model: PathBuf,
    vad: PathBuf,
    /// 泰语链路的二进制。**缺失不是致命错误**——它只影响泰语，
    /// 主链路照常工作。老版本升级上来时 vendor 里没有这个文件，
    /// 那时候不该连启动都失败。
    whisper: PathBuf,
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
            whisper: vendor.join("bin/whisper-cli"),
        };
        // 只校验主链路。whisper 的缺失留到真要用泰语时再报——
        // 见 whisper 字段的说明。
        for p in [&a.bin, &a.model, &a.vad] {
            if !p.exists() {
                bail!("缺少 ASR 依赖文件: {}", p.display());
            }
        }
        Ok(a)
    }

    /// 按选定的识别语言分派到对应引擎。
    pub fn transcribe(&self, wav: &Path, lang: AsrLang) -> Result<Transcript> {
        match lang {
            AsrLang::Auto => self.transcribe_sensevoice(wav),
            AsrLang::Thai => self.transcribe_thai(wav),
        }
    }

    fn transcribe_sensevoice(&self, wav: &Path) -> Result<Transcript> {
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

    /// 泰语：whisper-cli + Thonburian 微调模型。
    ///
    /// 解码参数**照抄 ADR-0004 §3 的基线**（线程 4、贪心 beam 1）——
    /// 那张 RTF/RSS 表和 §4 的 CER 都是在这组参数下测的。改这里的任何一个
    /// 数字，入库的数据就不再描述产品的实际行为了。
    fn transcribe_thai(&self, wav: &Path) -> Result<Transcript> {
        if !self.whisper.exists() {
            bail!(
                "泰语识别需要 {}，但它不在 vendor 里（跑 scripts/build-whisper-cli.sh）",
                self.whisper.display()
            );
        }
        let model = crate::download::path_of(&crate::download::THAI)
            .context("数据目录未初始化，找不到泰语模型")?;
        if !model.exists() {
            bail!("泰语模型还没下载：{}", model.display());
        }

        let out = Command::new(&self.whisper)
            .arg("-m").arg(&model)
            .arg("-f").arg(wav)
            // 强制泰语，不让它自己猜。模型是泰语微调的，猜错的代价远大于收益。
            .arg("-l").arg("th")
            .arg("-t").arg("4")
            // 贪心解码：beam 1 + best-of 1。**两个都要给**——
            // `-bo` 默认是 5，只给 `-bs 1` 的话温度回退时仍会采样五次，
            // 那就不是基线测的那套解码参数了。
            .arg("-bs").arg("1")
            .arg("-bo").arg("1")
            // -np：只输出结果，不打进度和模型信息
            // -nt：不要时间戳。**这两个一起才够**——只给 -nt 的话
            //      加载日志照样会混进 stdout。
            .arg("-np")
            .arg("-nt")
            .output()
            .with_context(|| format!("启动 {} 失败", self.whisper.display()))?;

        if !out.status.success() {
            bail!(
                "泰语转写失败 (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(clean_whisper(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// whisper 的输出清理。
///
/// 和 SenseVoice 那套完全不同：`-np -nt` 之后 stdout 就是纯文本，没有
/// `<|zh|>` 那样的标记可以拿来区分结果与日志。**所以判据只能是「stdout
/// 上的非空行」**，这也是为什么必须同时给 `-np`——少了它，模型信息会
/// 直接混进结果里被敲到用户的窗口。
///
/// 要过滤的是 whisper 的**非语音标注**：静音段会输出 `[BLANK_AUDIO]`、
/// `(silence)`、`[音乐]` 之类。它们是模型的元信息，不是用户说的话，
/// 粘到光标处纯属噪音。
fn clean_whisper(stdout: &str) -> Transcript {
    let mut text = String::new();
    for line in stdout.lines() {
        let t = line.trim();
        if t.is_empty() || is_nonspeech_marker(t) {
            continue;
        }
        // 段间要不要补空格：**两侧都是连写文字才不补**。
        //
        // ⚠️ 这条和 SenseVoice 那边的 `join_segments` **故意不一样**，
        // 别去「统一」它们。那边是「任意一侧连写就不补」，为的是中英混排
        // （`提交PR人家都review了`——中文夹英文本来就不带空格）。
        // 泰语场景相反：泰文夹英文**是带空格的**，实测输出就是
        // `ช่วย review pull request`。用「任意一侧」的判据，
        // whisper 要是把它切成两段（`ช่วย` / `review …`），
        // 拼出来就成了 `ช่วยreview` —— 词粘在一起。
        let need_space = match (text.chars().last(), t.chars().next()) {
            (Some(a), Some(b)) => !(is_scriptio_continua(a) && is_scriptio_continua(b)),
            _ => false,
        };
        if need_space {
            text.push(' ');
        }
        text.push_str(t);
    }
    let text = text.trim().to_string();
    Transcript {
        text,
        // 强制 `-l th` 解码，语种就是我们指定的，不是模型判出来的。
        // 这里如实写 `th`——`Transcript::lang` 的文档说它是「模型判定」，
        // 在这条链路上它是「用户指定」，两者的可信度不同，但对下游
        // （M2 的术语纠错选表）来说都是同一个用途。
        lang: Some("th".into()),
    }
}

/// whisper 的非语音标注。整行都是标注时才算——**不做行内剥离**，
/// 因为 `[` 也可能是用户真的说了个方括号里的内容。
fn is_nonspeech_marker(line: &str) -> bool {
    let b = line.as_bytes();
    matches!(
        (b.first(), b.last()),
        (Some(b'['), Some(b']')) | (Some(b'('), Some(b')'))
    )
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
    // stdout 优先：实测转写只走 stdout。两条流都拼起来的话，万一某天两边
    // 都出现结果就会**重复一遍**，而重复的文本会被自动上屏，很难察觉。
    // 只有 stdout 一条合法记录都没有时，才去 stderr 找（历史上混过流）。
    let found = collect(stdout);
    let found = if found.is_empty() { collect(stderr) } else { found };

    if found.is_empty() {
        // 没有合法记录时**返回空**，不做「按前缀过滤剩下的行」的兜底。
        //
        // 兜底看着稳妥，实际很危险：自动上屏默认开着，猜错就是把运行时
        // 日志直接敲进用户当时正在用的任何窗口。宁可什么都不出，
        // 也不能把内部日志替用户打出来——空剪贴板一眼能看出没转成功，
        // 混进日志的文本不会。
        // 两条流都看：只查 stdout 的话，「结果混进了 stderr 且格式变了」
        // 这种失效模式会连一行诊断都留不下
        if !stdout.trim().is_empty() || !stderr.trim().is_empty() {
            let peek = |s: &str| s.trim().chars().take(200).collect::<String>();
            log::error!(
                "ASR 输出里没有带语种标记的记录，判为转写失败（检查 --keep-tags 是否仍被支持）。\n  stdout: {}\n  stderr: {}",
                peek(stdout),
                peek(stderr)
            );
        }
        return Transcript { text: String::new(), lang: None };
    }

    let lang = found[0].0.to_string();
    // 段与段之间补一个空格。运行时把多个 VAD 段直接粘在一起输出，
    // 英文会连成 `worldthis`。中日泰不用空格分词，多出来的空格由
    // paste::sanitize 之外的这一步收敛——见下面的 join_segments。
    let text = join_segments(found.iter().map(|(_, body)| *body));
    Transcript { text, lang: Some(lang) }
}

/// 从一条流里挑出「带语种标记的转写记录」，返回 (语种, 正文) 列表。
///
/// 判据收紧为**必须有已知语种标记**，而不是「有任意 `<|...|>` 就算」——
/// 后者只要日志里出现一次尖括号标记就会把整行当成转写结果。
fn collect(stream: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for line in stream.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        for (lang, body) in split_records(t) {
            out.push((lang, body));
        }
    }
    out
}

/// 把一行拆成若干 `(语种, 正文)`。运行时会把多个 VAD 段拼在同一行，
/// 每段形如 `<|zh|><|NEUTRAL|><|Speech|><|withitn|>正文`。
///
/// ## 为什么判据要这么严
///
/// 只要「行内任何位置出现语种标记」就当成转写结果，日志会被切出正文来：
///
/// ```text
/// [sensevoice] unexpected token <|en|> while decoding frame 42
///                               ^ 松判据从这里开始当正文
/// ```
///
/// 结果「while decoding frame 42」会被**自动敲进用户当时正在用的窗口**。
/// 语种白名单挡得住 `<|debug|>`，挡不住这个。
///
/// 所以要求两条，缺一不可：
/// 1. **trim 后的行首**就得是语种标记——前面有任何别的内容，整行不是结果
/// 2. 语种标记后面**紧跟至少一个标记**（实测是 emotion/event/itn 三个）
fn split_records(line: &str) -> Vec<(&str, &str)> {
    // (记录起点, 语种, 正文起点)
    let mut starts: Vec<(usize, &str, usize)> = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let Some((name, after)) = tag_at(line, i) else {
            i += line[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        // 记录头 = 语种标记 + 紧跟的其他标记
        if is_lang(name) && tag_at(line, after).is_some() {
            // 吃掉记录头剩下的标记，正文从最后一个标记之后开始
            let mut p = after;
            while let Some((_, next)) = tag_at(line, p) {
                p = next;
            }
            starts.push((i, name, p));
            i = p;
        } else {
            i = after;
        }
    }

    // 第一条记录必须就在行首。不在行首说明它前面还有别的东西，
    // 那是日志，不是转写。
    if starts.first().is_none_or(|&(pos, _, _)| pos != 0) {
        return Vec::new();
    }

    starts
        .iter()
        .enumerate()
        .map(|(k, &(_, lang, body_start))| {
            let end = starts.get(k + 1).map_or(line.len(), |n| n.0);
            (lang, &line[body_start..end])
        })
        .collect()
}

/// 拼接各段正文。
///
/// 段间补一个空格，**但只要接缝任意一侧是不用空格分词的文字就不补**。
/// 「任意一侧」而不是「两侧」是有意的：中文段后面接英文段时也不该补，
/// SenseVoice 自己输出的中英混排就是不带空格的（`提交PR人家都review了`），
/// 我们在段接缝处补一个，反而和段内不一致。
fn join_segments<'a>(bodies: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for body in bodies {
        let piece = strip_tags(body);
        if piece.is_empty() {
            continue;
        }
        let need_space = match (out.chars().last(), piece.chars().next()) {
            (Some(a), Some(b)) => !is_scriptio_continua(a) && !is_scriptio_continua(b),
            _ => false,
        };
        if need_space {
            out.push(' ');
        }
        out.push_str(&piece);
    }
    out.trim().to_string()
}

/// 不用空格分词的文字。
///
/// **韩文不在其列** —— 现代韩文是分词写的，`안녕하세요 반갑습니다` 中间
/// 那个空格是必需的。曾经把谚文和中日泰并列，两段韩文会粘成
/// `안녕하세요반갑습니다`。
fn is_scriptio_continua(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF      // 平假名 / 片假名
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 统一汉字
        | 0x0E00..=0x0E7F    // 泰文
        | 0xFF00..=0xFFEF    // 全角标点
    ) || matches!(c, '，' | '。' | '？' | '！' | '、' | '：' | '；' | '「' | '」')
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

/// 泰语模型的安装校验：**让 whisper 真的加载一次这个文件**。
///
/// 由下载器在 **rename 之前**调用，所以传进来的是 `.part` 的路径，
/// 不是最终路径——冒烟不过就不该产生最终文件。
///
/// 为什么 SHA 校验之外还要这一步：SHA 保证文件和我们测过的一致，
/// 但保证不了**这个 whisper-cli 能加载它**——量化格式和 whisper.cpp
/// 的版本是会脱节的。
///
/// ⚠️ **这个函数的契约是「模型能被加载」，不是「端到端转写可用」。**
/// 它喂的是静音，只走通「解析模型头 → 分配张量 → 初始化」。
/// 一个能加载、却在真实音频上解码失败的构建**能通过这一关**。
/// 要覆盖那种情况得随包带一段真人泰语音频并断言输出内容，
/// 那是另一笔成本，目前没做。
///
/// （`docs/plan-i18n-thai.md` §4 的状态机把「装好才提交配置」写成了硬要求；
/// 「提交配置」这一半现在归 `tray::on_thai_installed`——用户可能在
/// 这几分钟的下载里改了主意。）
pub fn verify_thai_model(model: &Path) -> Result<()> {
    let vendor = VENDOR.get().context("vendor 路径未初始化")?;
    let bin = vendor.join("bin/whisper-cli");
    if !bin.exists() {
        bail!("泰语引擎不在 vendor 里：{}", bin.display());
    }

    let wav = std::env::temp_dir().join(format!("agentear-smoke-{}.wav", std::process::id()));
    write_silence(&wav, 0.5).context("写冒烟用的静音 wav 失败")?;

    let out = Command::new(&bin)
        .arg("-m").arg(model)
        .arg("-f").arg(&wav)
        .arg("-l").arg("th")
        .arg("-np").arg("-nt")
        .output()
        .with_context(|| format!("启动 {} 失败", bin.display()));
    // 先删临时文件再判结果，别让失败路径漏掉清理
    let _ = std::fs::remove_file(&wav);

    let out = out?;
    if !out.status.success() {
        bail!(
            "whisper 加载模型失败 (exit {:?}):\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// 16 kHz 单声道静音。whisper 只吃 16 kHz。
fn write_silence(path: &Path, secs: f32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for _ in 0..(16_000.0 * secs) as usize {
        w.write_sample(0i16)?;
    }
    w.finalize()?;
    Ok(())
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
    /// 英文段之间要补空格，否则会连成 `segment.Second`。
    #[test]
    fn joins_multiple_segments() {
        let line = format!("{}{}", tagged("en", "First segment."), tagged("en", "Second segment."));
        let t = clean(&line, LOG);
        assert_eq!(t.text, "First segment. Second segment.");
        assert_eq!(t.lang.as_deref(), Some("en"));
    }

    /// 中文段之间不能补空格——中文不用空格分词。
    #[test]
    fn joins_chinese_without_space() {
        let line = format!("{}{}", tagged("zh", "第一段。"), tagged("zh", "第二段。"));
        assert_eq!(clean(&line, LOG).text, "第一段。第二段。");
    }

    /// 只有带**已知语种**标记的行才算转写结果。日志里混进别的尖括号标记
    /// （比如 `<|debug|>`）不能把整行当成结果吐给用户。
    #[test]
    fn requires_a_language_tag_not_just_any_tag() {
        let t = clean("", "<|debug|> internal state dump");
        assert!(t.text.is_empty(), "非语种标记不应被当成转写结果：{:?}", t.text);
    }

    /// 日志行里偶然出现一个语种标记，**不能**把它右边的内容当成转写正文。
    /// 松判据会切出「while decoding frame 42」并自动敲进用户窗口。
    #[test]
    fn language_tag_mid_line_is_not_a_record() {
        let t = clean(
            "[sensevoice] unexpected token <|en|> while decoding frame 42",
            LOG,
        );
        assert!(
            t.text.is_empty(),
            "日志里的语种标记被当成记录头了：{:?}",
            t.text
        );
    }

    /// 就算日志行以语种标记开头，后面没有紧跟其他标记也不算记录头。
    #[test]
    fn bare_language_tag_without_following_tags_is_not_a_record() {
        let t = clean("<|en|> decoding failed, retrying", LOG);
        assert!(t.text.is_empty(), "光一个语种标记不构成记录头：{:?}", t.text);
    }

    /// 韩文是分词写的，两段之间必须有空格。
    #[test]
    fn korean_segments_get_a_space() {
        let line = format!("{}{}", tagged("ko", "안녕하세요"), tagged("ko", "반갑습니다"));
        assert_eq!(clean(&line, LOG).text, "안녕하세요 반갑습니다");
    }

    /// 泰文不分词，段间不补空格。
    ///
    /// 这里直接测 `join_segments` 而不是走 `clean()`：**SenseVoice 根本不会
    /// 输出 `<|th|>`**（泰语不在它支持的语种里，会被误判成 `en` 或 `yue`，
    /// 见 `keeps_thai_even_when_misdetected`）。将来接泰语引擎时走的是另一条
    /// 解析路径，但拼接规则是共用的。
    #[test]
    fn thai_segments_get_no_space() {
        let joined = join_segments(["สวัสดี", "ครับ"].into_iter());
        assert_eq!(joined, "สวัสดีครับ");
    }

    /// `is_lang` 只列 SenseVoice 真会输出的语种。泰语不在其中——
    /// 这不是遗漏，加进去反而会让「泰语被误判」这件事更难发现。
    #[test]
    fn thai_is_not_a_sensevoice_language() {
        assert!(!is_lang("th"));
        for l in ["zh", "en", "yue", "ja", "ko", "nospeech"] {
            assert!(is_lang(l), "{l} 应该是已知语种");
        }
    }

    /// 无标记时**返回空**而不是猜。自动上屏默认开着，猜错等于把运行时日志
    /// 直接敲进用户正在用的窗口。
    #[test]
    fn untagged_output_yields_nothing() {
        let t = clean("[sensevoice] done 0.4s\nsome unexpected log line", LOG);
        assert!(t.text.is_empty(), "无语种标记时必须返回空，实际: {:?}", t.text);
        assert_eq!(t.lang, None);
    }

    /// stdout 有结果时不再去 stderr 找，避免两边都有时输出重复一遍。
    #[test]
    fn does_not_duplicate_across_streams() {
        let line = tagged("zh", "只该出现一次");
        let t = clean(&line, &format!("{LOG}\n{line}"));
        assert_eq!(t.text, "只该出现一次");
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

}

#[cfg(test)]
mod whisper_tests {
    use super::*;

    /// whisper 的非语音标注不能进剪贴板。
    ///
    /// 静音段会输出 `[BLANK_AUDIO]`——自动上屏开着的话，用户按了录音键
    /// 但没说话，光标处就会被敲进一行 `[BLANK_AUDIO]`。
    #[test]
    fn nonspeech_markers_are_dropped() {
        let t = clean_whisper("[BLANK_AUDIO]\n(silence)\n[เสียงเพลง]\n");
        assert_eq!(t.text, "", "只有非语音标注时应该什么都不输出");
    }

    /// 泰文段之间**不补空格**——泰文和中日文一样不用空格分词。
    /// 补了的话，转写出来的句子中间会多出词典里没有的断点。
    #[test]
    fn thai_segments_are_not_space_joined() {
        let t = clean_whisper("สวัสดี\nครับ\n");
        assert_eq!(t.text, "สวัสดีครับ");
    }

    /// **泰文↔拉丁的接缝要补空格。**
    ///
    /// 这条是 codex 评审抓出来的：判据原本抄了 SenseVoice 那边的
    /// 「任意一侧是连写文字就不补」，那是为中英混排设计的。
    /// 泰语夹英文本来就带空格（实测 `ช่วย review pull request`），
    /// 用那条判据、且 whisper 恰好在此处切了段，就会粘成 `ช่วยreview`。
    #[test]
    fn thai_to_latin_boundary_keeps_the_space() {
        assert_eq!(clean_whisper("ช่วย\nreview\n").text, "ช่วย review");
        assert_eq!(clean_whisper("review\nช่วย\n").text, "review ช่วย");
        // 真实那句被切成两段的情形
        assert_eq!(
            clean_whisper("ช่วย\nreview pull request\nของผมหน่อยครับ\n").text,
            "ช่วย review pull request ของผมหน่อยครับ"
        );
    }

    /// 但拉丁字母之间要补。泰语里夹的英文技术词是真实场景
    /// （ADR-0004 §4 记的 `ช่วย review pull request`），
    /// 两段都是英文时粘成 `pullrequest` 就废了。
    #[test]
    fn latin_segments_still_get_a_space() {
        let t = clean_whisper("pull\nrequest\n");
        assert_eq!(t.text, "pull request");
    }

    /// 语种如实标 `th`。
    ///
    /// 注意这和 SenseVoice 那条链路的语义不同：那边是**模型判**的，
    /// 这边是**用户指定**的（`-l th` 强制）。两者可信度不一样，
    /// 但对下游（M2 按语种选术语表）是同一个用途。
    #[test]
    fn thai_path_reports_th() {
        assert_eq!(clean_whisper("สวัสดี").lang.as_deref(), Some("th"));
    }

    /// 空输出不是错误——用户按了录音键又没说话，就该什么都不出。
    #[test]
    fn empty_output_is_empty_text() {
        assert_eq!(clean_whisper("").text, "");
        assert_eq!(clean_whisper("   \n  \n").text, "");
    }

    /// 识别语言默认是 Auto，**不是泰语**。
    /// 泰语要用户显式选，还得先下 574 MB 的模型。
    #[test]
    fn default_recognition_language_is_auto() {
        assert_eq!(AsrLang::default(), AsrLang::Auto);
    }

    /// 配置里的取值名要稳定——改了它，用户升级后设置会静默丢失。
    #[test]
    fn asr_lang_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&AsrLang::Auto).unwrap(), "\"auto\"");
        assert_eq!(serde_json::to_string(&AsrLang::Thai).unwrap(), "\"thai\"");
    }
}
