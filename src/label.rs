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

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

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

pub struct Classifier {
    url: String,
}

impl Classifier {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    /// 给一段文字分类。**不返回 Result**——见模块文档：
    /// 任何失败都是 `unknown`，调用方不该被迫处理错误分支。
    pub fn classify(&self, text: &str) -> Label {
        if text.trim().is_empty() {
            return Label::Unknown;
        }
        match self.try_classify(text) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("标签识别不可用，落 unknown: {e:#}");
                Label::Unknown
            }
        }
    }

    fn try_classify(&self, text: &str) -> Result<Label> {
        // **先确认对面是谁。** 8793 被本机另一个项目的 node 服务占用过
        // （见 correct.rs 的 probe 说明）——不校验的话，转写文本会被发给
        // 那个服务，而它只要返回一个形状合法、内容恰好是某个类名的响应，
        // 结果就会被采信。
        //
        // 复用 correct 的探测：它带 3 秒缓存，而分类和纠错连的是同一个边车，
        // 各做一份只会让缓存失效得更频繁。
        if !crate::correct::service_reachable() {
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

        let out = curl_post(&self.url, &body)?;
        let v: serde_json::Value = serde_json::from_str(&out)
            .with_context(|| format!("解析响应失败: {}", out.chars().take(200).collect::<String>()))?;

        // 和 correct.rs 一样检查结束原因。这里截断的后果不同但一样坏：
        // `max_tokens=16` 被打满通常意味着模型在解释而不是给类名，
        // 那种输出解析出来多半是 unknown，但也可能碰巧命中一个类名。
        let finish = v["choices"][0]["finish_reason"].as_str().unwrap_or("");
        if finish != "stop" {
            bail!("响应未正常结束（finish_reason={finish:?}）");
        }

        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .context("响应里没有 content")?;

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

fn curl_post(url: &str, body: &str) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("/usr/bin/curl")
        // `-f`：4xx/5xx 直接以非零退出，而不是把错误页当正文交给我们。
        // 少了它，一个 HTTP 500 只要响应体形状恰好合法（比如错误信息里
        // 带着 choices 结构）就会被当成分类结果——那种降级失败是静默的。
        .arg("-fsS")
        .arg("--max-time")
        .arg(TIMEOUT_SECS.to_string())
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg("@-")
        .arg(format!("{url}/v1/chat/completions"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 curl 失败")?;
    // 写请求体失败时**也要把子进程收掉**。直接 `?` 返回的话 `Child` 被
    // drop 而没有 wait，留下僵尸进程——守护进程一开就是几周，
    // 每次录音漏一个，积少成多。
    let write_result = (|| -> Result<()> {
        child
            .stdin
            .take()
            .context("拿不到 curl 的 stdin")?
            .write_all(body.as_bytes())
            .context("写请求体失败")
    })();
    if let Err(e) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    let out = child.wait_with_output().context("等待 curl 失败")?;
    if !out.status.success() {
        bail!(
            "curl 退出码 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
        assert_eq!(c.classify(""), Label::Unknown);
        assert_eq!(c.classify("   \n "), Label::Unknown);
    }

    /// **边车不可达时落 unknown，不 panic、不返回 Err。**
    #[test]
    fn unreachable_service_yields_unknown() {
        let c = Classifier::new("http://127.0.0.1:1");
        assert_eq!(c.classify("随便一句话"), Label::Unknown);
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
            let got = c.classify(text);
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
}
