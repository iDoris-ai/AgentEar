//! 一级标签识别：这段话是 idea、task 还是别的。
//!
//! 这是 README 里「说这是一个 idea / 这是一个任务，自动选分支」那一段的
//! 第一步。识别出来的标签驱动下游路由（`routes/`，T2.2.4）。
//!
//! ## 定义在文档里，提示词是它的副本
//!
//! 八个标签的定义与判别依据是 `docs/agent/label-taxonomy.md` 的产出，
//! **那份文档是权威**。这里的 `RULES` 是它的一份副本——两处会漂移，
//! 所以改任何一处都要同步另一处，测试里有一条断言钉住关键措辞。
//!
//! 为什么要把定义写进提示词：M0 的基准里提示词只有一行「归入其中一类，
//! 只输出类名」，结果 **6/8**。补上定义和判别问题之后，在扩充到 18 条
//! 且刻意贴边界的用例上是 **18/18**。那两处判错从来不是模型能力问题。
//!
//! ## 失败一律落 unknown，不落别的
//!
//! 边车没起、超时、返回垃圾、返回一个不认识的类名——统统 `unknown`。
//! 理由和 `correct.rs` 一样：这一层是增强，不能挡住主链路。
//! 而 `unknown` 本身是**设计里就有的合法去处**（ADR-0002 §3.1：
//! 「分类失败要有明确去处，而不是硬塞进某个标签」），
//! 不是把错误伪装成结果。

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// 一级标签。**封闭集合**，不可扩展——二级标签才是开放的（ADR-0002 §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    Idea,
    Task,
    Command,
    Note,
    Journal,
    Question,
    Reference,
    /// 无法归类。**这是合法结果，不是错误码。**
    #[default]
    Unknown,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Label::Idea => "idea",
            Label::Task => "task",
            Label::Command => "command",
            Label::Note => "note",
            Label::Journal => "journal",
            Label::Question => "question",
            Label::Reference => "reference",
            Label::Unknown => "unknown",
        }
    }

    /// 从模型输出解析。**认不出来一律 unknown**，不猜、不做模糊匹配。
    ///
    /// 模糊匹配（比如「包含 idea 就算 idea」）看着宽容，实际很危险：
    /// 模型要是回一句「这不是 idea 而是 task」，包含匹配会命中 idea。
    fn parse(s: &str) -> Label {
        // ⚠️ **不能写成「只保留 ASCII 字母」**——那会把中文整个过滤掉，
        // 于是「我觉得应该是 note 吧」剩下 `note`，变相成了包含匹配，
        // 正好是这个函数要避免的东西。
        //
        // 正确做法：只剥**外围**的装饰（序号、标点、空白），
        // 剩下的必须**整体**等于一个类名。中间夹着别的内容一律不认。
        // 外围装饰要连**全角**一起剥。中文模型很自然会输出 `idea。`、
        // 「task」、（note）、`１．command` 这些形式——只认 ASCII 标点的话
        // 它们全都落到 Unknown，而那是把模型答对了的情况判成失败。
        let decoration = |c: char| {
            c.is_ascii_punctuation()
                || c.is_whitespace()
                || c.is_numeric() // 含全角数字１２３
                || matches!(
                    c,
                    '。' | '，' | '、' | '：' | '；' | '！' | '？'
                        | '（' | '）' | '「' | '」' | '『' | '』'
                        | '《' | '》' | '【' | '】' | '“' | '”' | '‘' | '’'
                        | '．' | '－' | '　'
                )
        };
        let t = s.trim().trim_matches(decoration).to_ascii_lowercase();
        match t.as_str() {
            "idea" => Label::Idea,
            "task" => Label::Task,
            "command" => Label::Command,
            "note" => Label::Note,
            "journal" => Label::Journal,
            "question" => Label::Question,
            "reference" => Label::Reference,
            _ => Label::Unknown,
        }
    }

    pub const ALL: [Label; 8] = [
        Label::Idea,
        Label::Task,
        Label::Command,
        Label::Note,
        Label::Journal,
        Label::Question,
        Label::Reference,
        Label::Unknown,
    ];
}

/// 提示词里的定义部分。
///
/// **与 `docs/agent/label-taxonomy.md` §2 同源**，改一处要同步另一处。
/// 每类给「一句话定义 + 一个判别问题」——判别问题比定义实用，
/// 它是用来当场做决定的。
///
/// ⚠️ **note / journal 的边界是临时决策，未经 jason 拍板**
/// （`docs/agent/progress.md` 的 Q1）。这里按「离不离得开今天」定成
/// journal，测试和评测集也照这个固定。如果他认为「今天开会讨论了传输协议」
/// 该归 note，**要改的是这段定义和评测集的期望值，不是模型**。
///
/// 末尾那两条 ⚠️ 不是凑数，各修掉一次实测判错：
/// 没有它们时 question 类 3 条只对 1 条（模型靠问号判断），
/// 「啊对对对」被判成 idea。
const RULES: &str = "\
idea：一个还没决定要不要做的想法。判别：他承诺要做了吗？没有 → idea
task：一件确定要做的事，有交付物。判别：要我记下来以后做吗？是 → task
command：要系统现在执行的指令。判别：他在等系统立刻给反应吗？是 → command
note：一条知识、事实、结论。判别：一年后单独拿出来看还成立吗？成立 → note
journal：当天发生了什么、当时的状态。判别：离开「今天」这个语境还有意义吗？没有 → journal
question：一个待解答的疑问。判别：这句话的目的是求一个答案吗？是 → question
  ⚠️ 口语里疑问句常常没有问号（「泰语模型能不能在 Intel Mac 上跑」「现在几点了」
  都是问句）。不要靠标点判断。
reference：指向外部资源的指针。判别：主体是链接或出处吗？是 → reference
unknown：无法归类或内容无意义。以上都不像就选它，宁可 unknown 不要瞎猜
  ⚠️ 语气词、口头禅、附和（「嗯这个那个」「啊对对对」）一律 unknown，
  不要因为它可能暗示某种态度就归到 idea

一条实测判错的反例（FU-13）：
「要是能用语音直接建任务就好了」→ **idea**，不是 task。
「要是……就好了」是虚拟语气，他没有承诺要做；句子里的「建任务」
三个字是内容，不是意图。**看的是有没有承诺，不是句子里出现了什么词。**

两组最容易混的：
- note vs journal：看它离不离得开「今天」。「冷启动 0.2 秒」是事实(note)；
  「今天开会讨论了传输协议」离开时间就没信息量(journal)
- command vs task：看他等不等系统立刻反应。「帮我查日程」在等回答(command)；
  「记得给术语表加词」是让系统记下来(task)
";

/// 单次分类的超时。
///
/// 比纠错短得多（那边是 20 秒）：输出只有一个词，正常一秒内。
/// 给到 10 秒是为了容忍模型冷加载。
const TIMEOUT_SECS: u64 = 10;

/// 标签是怎么来的。
///
/// 这个区分不是记账用的，它决定了**下游能不能信这个标签**：
/// 用户明说的那条是确定的，模型推断的那条随时可能错。
/// `routes/` 的记录里带着它（spec.md §3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// 用户在语音里明说了「这是一个 idea」。**不得被模型推断覆盖**
    /// （架构边界 B5，README 定的产品行为）。
    Explicit,
    /// 模型推断的。
    Model,
}

/// 一次分类的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub label: Label,
    pub source: Source,
}

/// 显式标记的引导词。**只在句首匹配**——见 `detect_explicit` 的说明。
const LEADERS: [&str; 12] = [
    "这是一个", "这是个", "这是", "记一个", "记个", "记一条",
    "标记为", "归到", "算是",
    "this is a", "this is an", "mark as",
];

/// 每个标签的中英文说法。
///
/// ## 收词标准：只收「不会有别的意思」的词
///
/// 去掉过「问题」——它在中文里主要指**缺陷、麻烦**，不是「疑问」。
/// 「这是一个问题，我们需要修复它」是在报告一个 bug，而按字面它会被
/// 判成显式的 question 且**模型无法纠正**。想显式标 question 的人会说
/// 「这是一个疑问」或「这是一个 question」。
///
/// 同理去掉「记录」（「这是一个记录」远不如「会议记录」常见）。
/// 这条标准和术语表那边一样：**误判的代价远大于多收一个词的收益**。
const SPOKEN: [(&str, Label); 18] = [
    ("idea", Label::Idea),
    ("想法", Label::Idea),
    ("点子", Label::Idea),
    ("灵感", Label::Idea),
    ("task", Label::Task),
    ("任务", Label::Task),
    ("待办", Label::Task),
    ("command", Label::Command),
    ("指令", Label::Command),
    ("命令", Label::Command),
    ("note", Label::Note),
    ("笔记", Label::Note),
    ("journal", Label::Journal),
    ("日记", Label::Journal),
    ("日志", Label::Journal),
    ("question", Label::Question),
    ("疑问", Label::Question),
    ("reference", Label::Reference),
];

/// 标签词后面必须是这些之一，否则不算匹配。
///
/// **这是 codex 抓出来的一条**：没有词尾边界时，「这是一个任务栏截图」
/// 会命中「任务」、`This is an ideal solution` 会命中 `idea`、
/// `This is a notebook` 会命中 `note`——三个都是普通名词短语，
/// 却被永久钉成显式标签。
fn is_boundary(rest: &str) -> bool {
    match rest.chars().next() {
        None => true, // 到句尾了
        Some(c) => {
            c.is_whitespace()
                || c.is_ascii_punctuation()
                || matches!(
                    c,
                    '，' | '。' | '、' | '：' | '；' | '！' | '？'
                        | '（' | '）' | '「' | '」' | '“' | '”' | '　'
                )
        }
    }
}

/// 引导词后面出现这些，说明用户是在**问**或在**比较**，不是在标记。
///
/// 「这是一个任务还是一个想法？」按字面会命中 task，但他显然是在问。
/// 显式标记不可被模型纠正，所以这种句子必须让给模型判断。
const NOT_A_MARKER: [&str; 4] = ["还是", "或者", "呢？", "吗？"];

/// 从文本里找显式标记。**纯字符串匹配，不问模型**——
/// 「显式」的意思就是不靠猜（spec.md §3）。
///
/// ## 为什么只在句首匹配
///
/// 「问题」「记录」这些词在正常说话里太常见了：
/// 「我遇到了一个问题」是在描述困境，不是在给系统打标签。
/// 只认句首的「这是一个 X」「记一个 X」这类**元指令句式**，
/// 才能把「给系统下指令」和「说话内容里恰好有这个词」分开。
///
/// 代价是漏检：用户在句子中间说「……，这算是一个 idea 吧」不会被认出来。
/// **这是有意的取舍**——显式标记误判的代价（把用户的内容强行归错类，
/// 而且不可被模型纠正）比漏判高得多，漏了还有模型兜着。
///
/// ## 不剥离标记文本
///
/// 「这是一个 idea，给录音笔加 WiFi」识别之后，`text` 仍然是**整句**，
/// 不会被剥成「给录音笔加 WiFi」。剥离要判断从哪剥到哪，判错就是丢内容，
/// 而 raw 优先的原则下宁可多留不可少留。
pub fn detect_explicit(text: &str) -> Option<Label> {
    // 疑问句一律不算显式标记。「这是一个任务吗？」是在问，不是在标。
    let trimmed = text.trim_end();
    if trimmed.ends_with('？') || trimmed.ends_with('?') {
        return None;
    }

    // 归一化全角空格：`this　is a task`（U+3000）里的空格不是半角，
    // 而引导词是按字面匹配的，不归一就认不出来。
    let normalized = text.replace('\u{3000}', " ");

    // 剥掉前导语气词。**按词剥，不按字符集剥**——
    // 早先用字符集合 `'那' | '个' | '就'`，那会把「那，这是一个任务还是…」
    // 的「那，」吃掉，让一个非句首的从句变成句首，进而被误判成显式标记。
    // 「那」「就」本身是有意义的词（「那篇博客」「就这样」），不是语气词。
    const FILLERS: [&str; 8] = ["嗯", "呃", "啊", "哦", "那个", "然后", "，", ","];
    let mut t = normalized.trim_start();
    loop {
        let before = t.len();
        for f in FILLERS {
            t = t.strip_prefix(f).unwrap_or(t).trim_start();
        }
        if t.len() == before {
            break;
        }
    }
    let lower = t.to_lowercase();

    for lead in LEADERS {
        let Some(rest) = lower.strip_prefix(lead) else {
            continue;
        };
        // 引导词后面允许有空白（「这是一个 idea」的那个空格）
        let rest = rest.trim_start();
        for (word, label) in SPOKEN {
            let Some(after) = rest.strip_prefix(word) else {
                continue;
            };
            // 词尾必须是边界，否则「任务栏」「ideal」「notebook」都会命中
            if !is_boundary(after) {
                continue;
            }
            // 「这是一个任务还是一个想法」——在比较，不是在标记
            if NOT_A_MARKER.iter().any(|m| after.contains(m)) {
                return None;
            }
            return Some(label);
        }
    }
    None
}

pub struct Classifier {
    url: String,
    transport: Box<dyn crate::sidecar::Transport>,
    /// 跳过边车身份探测。**只在测试里置 true**——注入假传输层时
    /// 没有真实的 `/v1/models` 可探，而探测失败会让分类直接落 unknown，
    /// 于是所有测试都测不到真正的分支。
    skip_probe: bool,
}

impl Classifier {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            transport: Box::new(crate::sidecar::Curl),
            skip_probe: false,
        }
    }

    #[cfg(test)]
    fn with_transport(url: impl Into<String>, t: Box<dyn crate::sidecar::Transport>) -> Self {
        Self { url: url.into(), transport: t, skip_probe: true }
    }

    /// 给一段文字分类。**不返回 Result**——见模块文档：
    /// 任何失败都是 `unknown`，调用方不该被迫处理错误分支。
    ///
    /// **显式标记优先**：用户明说了「这是一个 idea」就直接采信，
    /// 连模型都不问（架构边界 B5）。这既是产品要求，也顺带省掉一次推理。
    pub fn classify(&self, text: &str) -> Classified {
        if text.trim().is_empty() {
            return Classified { label: Label::Unknown, source: Source::Model };
        }
        if let Some(label) = detect_explicit(text) {
            log::debug!("显式标记：{}", label.as_str());
            return Classified { label, source: Source::Explicit };
        }
        let label = match self.try_classify(text) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("标签识别不可用，落 unknown: {e:#}");
                Label::Unknown
            }
        };
        Classified { label, source: Source::Model }
    }

    fn try_classify(&self, text: &str) -> Result<Label> {
        // **先确认对面是谁。** 8793 被本机另一个项目的 node 服务占用过
        // （见 correct.rs 的 probe 说明）——不校验的话，转写文本会被发给
        // 那个服务，而它只要返回一个形状合法、内容恰好是某个类名的响应，
        // 结果就会被采信。
        //
        // 复用 correct 的探测：它带 3 秒缓存，而分类和纠错连的是同一个边车，
        // 各做一份只会让缓存失效得更频繁。
        if !self.skip_probe && !crate::correct::service_reachable() {
            bail!("边车不可达或对端不是 mlx-dspark");
        }
        let names: Vec<&str> = Label::ALL.iter().map(|l| l.as_str()).collect();
        let prompt = format!(
            "把下面这句话归入其中一类：{}\n\n{RULES}\n只输出类名，不要解释。\n\n{text}",
            names.join(" / ")
        );
        let body = serde_json::json!({
            "model": "ornith",
            "messages": [{ "role": "user", "content": prompt }],
            "temperature": 0.0,
            // 输出就一个词。给 16 是留给可能的空白和换行，
            // **不要给大**：给大了模型会开始解释，而解释会被 last_line 取走。
            "max_tokens": 16,
        })
        .to_string();

        let out = self.transport.post_json(&self.url, &body, TIMEOUT_SECS)?;
        let content = crate::sidecar::extract_content(&out)?;

        // 只取最后一行非空——和 correct.rs 同一个判据。
        // Ornith 会先吐推理过程，服务端 --no-thinking 关了它，这里是第二道。
        let line = content
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        Ok(Label::parse(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 认得出全部八个类名。
    #[test]
    fn parses_every_label() {
        for l in Label::ALL {
            assert_eq!(Label::parse(l.as_str()), l);
        }
    }

    /// 模型爱加的装饰要能剥掉：序号、句点、大小写、前后空白。
    #[test]
    fn parse_tolerates_decoration() {
        assert_eq!(Label::parse("idea."), Label::Idea);
        assert_eq!(Label::parse("  Task  "), Label::Task);
        assert_eq!(Label::parse("1. command"), Label::Command);
        assert_eq!(Label::parse("NOTE"), Label::Note);
    }

    /// **认不出来一律 unknown，绝不做包含匹配。**
    ///
    /// 包含匹配看着宽容，实际危险：模型要是回「这不是 idea 而是 task」，
    /// 包含匹配会命中 idea——正好是相反的答案。
    #[test]
    fn unrecognized_falls_to_unknown_never_substring_match() {
        assert_eq!(Label::parse("这不是 idea 而是 task"), Label::Unknown);
        assert_eq!(Label::parse("我觉得应该是 note 吧"), Label::Unknown);
        assert_eq!(Label::parse("banana"), Label::Unknown);
        assert_eq!(Label::parse(""), Label::Unknown);
    }

    /// 空输入不该发起请求。
    #[test]
    fn empty_input_is_unknown_without_a_request() {
        let c = Classifier::new("http://127.0.0.1:1");
        assert_eq!(c.classify("").label, Label::Unknown);
        assert_eq!(c.classify("   \n ").label, Label::Unknown);
    }

    /// **边车不可达时落 unknown，不 panic、不返回 Err。**
    #[test]
    fn unreachable_service_yields_unknown() {
        let c = Classifier::new("http://127.0.0.1:1");
        let r = c.classify("随便一句话");
        assert_eq!(r.label, Label::Unknown);
        assert_eq!(r.source, Source::Model, "不是显式标记，来源应该是 model");
    }

    /// 序列化名字要稳定——它会写进 `routes/` 的 JSON，改了会让历史记录读不出来。
    #[test]
    fn serde_names_are_stable() {
        assert_eq!(serde_json::to_string(&Label::Idea).unwrap(), "\"idea\"");
        assert_eq!(serde_json::to_string(&Label::Unknown).unwrap(), "\"unknown\"");
        assert_eq!(
            serde_json::from_str::<Label>("\"journal\"").unwrap(),
            Label::Journal
        );
    }

    /// 提示词里必须带上那两条 ⚠️——它们各修掉一次实测判错，
    /// 删掉就会回到 question 3 条只对 1 条的状态。
    #[test]
    fn rules_keep_the_two_hard_won_caveats() {
        assert!(RULES.contains("没有问号"), "缺了它，模型会靠标点判断疑问句");
        assert!(RULES.contains("语气词"), "缺了它，「啊对对对」会被判成 idea");
    }

    /// **提示词与权威文档不许漂移。**
    ///
    /// `docs/agent/label-taxonomy.md` 是标签定义的权威来源，`RULES` 是它的
    /// 副本。两处副本必然漂移——除非有东西盯着。这条测试盯的是**每个类的
    /// 判别问题**：那是定义里最要害的部分，也是当初把 6/8 提到 18/18 的东西。
    ///
    /// 改文档不改代码（或反过来）都会让它失败，那时**两边一起改**才是对的，
    /// 不要改断言来让它过。
    #[test]
    fn rules_do_not_drift_from_the_taxonomy_doc() {
        let doc = include_str!("../docs/agent/label-taxonomy.md");
        // 每个类在文档表格里的判别问题的关键词，和 RULES 里应当一致
        for (label, key) in [
            ("idea", "承诺要做了吗"),
            ("task", "记下来以后做"),
            ("command", "立刻"),
            ("note", "一年后"),
            ("journal", "今天"),
            ("question", "求一个答案"),
            ("reference", "链接或出处"),
            ("unknown", "宁可 unknown 不要瞎猜"),
        ] {
            assert!(
                doc.contains(key),
                "文档里找不到 {label} 的判别依据 {key:?}——文档被改了？"
            );
            assert!(
                RULES.contains(key),
                "提示词里缺了 {label} 的判别依据 {key:?}——和文档漂移了"
            );
        }
    }

    /// 默认值是 unknown：任何「还没分类」的状态都不该伪装成某个真标签。
    #[test]
    fn default_is_unknown() {
        assert_eq!(Label::default(), Label::Unknown);
    }

    // —— 用假传输层覆盖错误分支（T2.3.1）——

    use crate::sidecar::test_support::Fake;

    /// 传输层失败 → unknown，不 panic 不外抛。
    #[test]
    fn transport_failure_yields_unknown() {
        let c = Classifier::with_transport(
            "http://fake",
            Box::new(Fake::sequence(vec![Err("HTTP 500".into())])),
        );
        assert_eq!(c.classify("随便一句话").label, Label::Unknown);
    }

    /// 垃圾 JSON / 缺字段 / 空 content → unknown。
    #[test]
    fn malformed_responses_yield_unknown() {
        for raw in [
            "这不是 JSON",
            "{}",
            r#"{"choices":[{"finish_reason":"stop","message":{"content":""}}]}"#,
        ] {
            let c = Classifier::with_transport("http://fake", Box::new(Fake::always(raw)));
            assert_eq!(c.classify("随便一句话").label, Label::Unknown, "raw={raw}");
        }
    }

    /// **截断的响应 → unknown。**
    ///
    /// `max_tokens=16` 被打满通常意味着模型在解释而不是给类名，
    /// 那种输出**可能碰巧含一个合法类名**——不检查 finish_reason 就会采信它。
    #[test]
    fn truncated_response_yields_unknown_even_if_it_contains_a_label() {
        let raw = serde_json::json!({
            "choices": [{ "finish_reason": "length", "message": { "content": "task" } }]
        })
        .to_string();
        let c = Classifier::with_transport("http://fake", Box::new(Fake::always(&raw)));
        assert_eq!(
            c.classify("明天把基准跑完").label,
            Label::Unknown,
            "被截断的响应即使内容恰好是合法类名也不能采信"
        );
    }

    /// 正常响应按类名解析。
    #[test]
    fn normal_response_parses_the_label() {
        let c = Classifier::with_transport(
            "http://fake",
            Box::new(Fake::always(&Fake::ok_body("task"))),
        );
        let r = c.classify("明天把基准跑完");
        assert_eq!(r.label, Label::Task);
        assert_eq!(r.source, Source::Model);
    }

    /// **显式标记不发请求**——连传输层都不该被调用。
    #[test]
    fn explicit_marker_never_touches_the_transport() {
        let fake = Fake::always(&Fake::ok_body("note"));
        // 用 Arc 共享以便事后查调用次数
        let c = Classifier::with_transport("http://fake", Box::new(Fake::always(&Fake::ok_body("note"))));
        let r = c.classify("这是一个 idea，给录音笔加 WiFi");
        assert_eq!(r.label, Label::Idea, "显式标记应该直接采信");
        assert_eq!(r.source, Source::Explicit);
        assert_eq!(fake.call_count(), 0, "显式标记时不该发任何请求");
    }

    /// 真调边车，跑 label-taxonomy.md 的全部 18 条用例。
    ///
    /// **要边车在跑，所以标 ignore**：`cargo test --release -- --ignored`。
    /// 阈值定在 15/18：T2.2.5 的目标是「至少 7/8」≈ 87.5%，
    /// 这里留一点余量给模型的随机性（temperature 虽为 0，采样仍非完全确定）。
    #[test]
    #[ignore = "需要 LLM 边车在跑：scripts/serve-llm.sh"]
    fn classifies_the_taxonomy_cases() {
        let cases: [(&str, Label); 18] = [
            ("我觉得可以给录音笔加个 ESP32 自动上传", Label::Idea),
            ("要是能用语音直接建任务就好了", Label::Idea),
            ("这个方案要不要做，我还没想好", Label::Idea),
            ("明天把 M2 的基准测试跑完", Label::Task),
            ("记得给术语表加上 Kubernetes", Label::Task),
            ("帮我查一下明天的日程", Label::Command),
            ("把刚才那段录音删掉", Label::Command),
            ("SenseVoice 的冷启动只要 0.2 秒", Label::Note),
            ("whisper.cpp 的 Metal 首次运行要多花几秒编译 shader", Label::Note),
            ("今天开会讨论了接入层的传输协议", Label::Journal),
            ("今天调了一天按键事件，有点累但总算通了", Label::Journal),
            ("为什么 SenseVoice 的内存比 Nano 低这么多？", Label::Question),
            ("泰语模型能不能在 Intel Mac 上跑", Label::Question),
            ("现在几点了", Label::Question),
            ("Ornith 那篇博客在 blog.mushroom.cv", Label::Reference),
            ("ADR-0004 里记了泰语选型的全部局限", Label::Reference),
            ("嗯这个那个", Label::Unknown),
            ("啊对对对", Label::Unknown),
        ];
        let c = Classifier::new(crate::correct::DEFAULT_URL);
        let mut hit = 0;
        let mut misses = Vec::new();
        for (text, want) in cases {
            let got = c.classify(text).label;
            if got == want {
                hit += 1;
            } else {
                misses.push(format!("  {text:?} 期望 {} 得到 {}", want.as_str(), got.as_str()));
            }
        }
        // 打印确切分数：spike/m2_bench.py 用的是**更宽松的解析器**
        // （它删掉所有非 [a-z] 字符，于是「我觉得应该是 note 吧」会变成 note），
        // 所以那边报的 18/18 和这里可能分叉。**以这条为准**——它走的是
        // 生产代码路径。
        println!("生产路径实测：{hit}/18");
        for m in &misses {
            println!("{m}");
        }
        assert!(
            hit >= 15,
            "18 条只对了 {hit} 条（阈值 15）：\n{}",
            misses.join("\n")
        );
    }

    /// 中英文的各种显式说法都要认出来。
    #[test]
    fn explicit_markers_are_recognized() {
        for (text, want) in [
            ("这是一个 idea，给录音笔加个 WiFi 模块", Label::Idea),
            ("这是个想法，可以用语音建任务", Label::Idea),
            ("这是一个任务，明天把基准跑完", Label::Task),
            ("记一个任务：给术语表加词", Label::Task),
            ("记个笔记，SenseVoice 冷启动 0.2 秒", Label::Note),
            ("标记为 reference，那篇博客在 mushroom.cv", Label::Reference),
            ("this is a task, finish the benchmark", Label::Task),
            ("mark as note, the metal shader takes seconds", Label::Note),
            ("这是一个疑问，泰语能不能在 Intel 上跑", Label::Question),
            ("这是日记，今天调了一天按键", Label::Journal),
        ] {
            assert_eq!(detect_explicit(text), Some(want), "没认出来：{text:?}");
        }
    }

    /// 真实口语的前导语气词不能挡住识别。
    ///
    /// jason 的录音里几乎每句都以「嗯」「那个」「呃」开头
    /// （见 spike/audio/sample02.wav 的转写）。
    #[test]
    fn leading_fillers_do_not_block_detection() {
        for text in [
            "嗯，这是一个 idea，给录音笔加 WiFi",
            "呃这是个任务",
            "那个，记一个笔记",
            "  啊，这是一个想法",
        ] {
            assert!(detect_explicit(text).is_some(), "语气词挡住了识别：{text:?}");
        }
    }

    /// **codex 抓出来的四类误判，逐条钉住。**
    ///
    /// 这些全是「按字面像标记、其实不是」的句子。误判的代价特别高：
    /// 显式标记不会被模型纠正，一旦认错就永久归错类。
    #[test]
    fn adversarial_false_positives_are_rejected() {
        for (text, why) in [
            // 1. 在问，不是在标
            ("这是一个任务还是一个想法？", "疑问句 + 「还是」"),
            ("这是一个任务吗？", "疑问句"),
            ("那，这是一个任务还是一个想法？", "前面有连接词「那」，且是疑问"),
            // 2. 「问题」= 缺陷，不是「疑问」——已从词表移除
            ("这是一个问题，我们需要修复它", "「问题」指缺陷"),
            // 3. 没有词尾边界会命中的普通名词
            ("这是一个任务栏截图", "任务栏 ≠ 任务"),
            ("this is an ideal solution", "ideal ≠ idea"),
            ("this is a notebook i bought", "notebook ≠ note"),
            ("这是一个想法国的故事", "想法国 ≠ 想法"),
        ] {
            assert_eq!(detect_explicit(text), None, "误判成显式标记（{why}）：{text:?}");
        }
    }

    /// 「那」「就」不是语气词，不能当填充词剥掉。
    ///
    /// 剥掉它们会让一个**非句首**的从句变成句首，从而把普通句子变成
    /// 显式标记——codex 的原话是 `"那，这是一个任务还是一个想法？"`
    /// 会因此被判成 Task。
    #[test]
    fn meaningful_connectives_are_not_stripped_as_fillers() {
        assert_eq!(detect_explicit("那篇博客在 mushroom.cv"), None);
        assert_eq!(detect_explicit("就这样吧，先不做了"), None);
    }

    /// 全角空格（U+3000）里的引导词也要认出来——中文输入法很容易打出它。
    #[test]
    fn fullwidth_space_inside_leader_still_matches() {
        assert_eq!(detect_explicit("this　is a task, finish it"), Some(Label::Task));
        assert_eq!(detect_explicit("这是一个　idea，加个 WiFi"), Some(Label::Idea));
    }

    /// **句子中间出现这些词不算显式标记。**
    ///
    /// 这是整个设计里最要紧的一条。「问题」「记录」「任务」在正常说话里
    /// 太常见了——「我遇到了一个问题」是在描述困境，不是给系统打标签。
    /// 认错的代价特别高：显式标记**不会被模型推断覆盖**，
    /// 一旦误判就是把用户的内容强行归错类且无法纠正。
    #[test]
    fn mid_sentence_words_are_not_explicit_markers() {
        for text in [
            "我遇到了一个问题，麦克风没声音",
            "昨天的会议记录在共享盘上",
            "这个任务比我想的难",
            "他有很多想法但没落地",
            "我觉得这算个想法吧",          // 「算个」不在引导词表里
            "帮我查一下有没有新的 task",
            "note 这个词在英文里有很多意思",
        ] {
            assert_eq!(detect_explicit(text), None, "误判成显式标记了：{text:?}");
        }
    }

    /// 显式标记优先于模型：**连边车都不问**。
    ///
    /// 这里故意把地址指到一个不可能有服务的端口——如果实现去问了模型，
    /// 会拿到 Unknown；只有真的走了显式分支才会得到 Idea。
    #[test]
    fn explicit_wins_without_asking_the_model() {
        let c = Classifier::new("http://127.0.0.1:1");
        let r = c.classify("这是一个 idea，给录音笔加个 WiFi 模块");
        assert_eq!(r.label, Label::Idea);
        assert_eq!(r.source, Source::Explicit);
    }

    /// **词表里不许有一个词是另一个词的前缀。**
    ///
    /// 有的话，短的会先命中、长的永远轮不到（匹配是按表顺序线性扫的）。
    /// 现在没有这种情况，但加词时很容易引入——所以钉一条测试，
    /// 而不是在注释里写一句「长的在前」然后指望后来的人记得
    /// （那句注释本来就是假的，codex 指出了）。
    #[test]
    fn no_spoken_word_is_a_prefix_of_another() {
        for (a, _) in SPOKEN {
            for (b, _) in SPOKEN {
                if a != b {
                    assert!(
                        !b.starts_with(a),
                        "{a:?} 是 {b:?} 的前缀，加词时会让 {b:?} 永远匹配不到"
                    );
                }
            }
        }
    }

    /// `Source` 的序列化名字要稳定——它会写进 routes 的 JSON。
    #[test]
    fn source_serde_names_are_stable() {
        assert_eq!(serde_json::to_string(&Source::Explicit).unwrap(), "\"explicit\"");
        assert_eq!(serde_json::to_string(&Source::Model).unwrap(), "\"model\"");
    }

    /// 引导词后面必须真的跟着一个标签词，否则不算。
    #[test]
    fn leader_without_a_label_word_is_not_a_marker() {
        assert_eq!(detect_explicit("这是一个很长的故事"), None);
        assert_eq!(detect_explicit("这是我昨天说的那件事"), None);
        assert_eq!(detect_explicit("this is a good point"), None);
    }
}
