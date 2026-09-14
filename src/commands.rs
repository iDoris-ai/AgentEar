//! 可配置的语音指令表：**本地精确匹配 → 命中就本地执行；没命中才交给 LLM**。
//!
//! ## 为什么是「两段式」而不是「全靠模型」
//!
//! jason 2026-09-15 的判断是对的：**代码是有限的，字符串匹配只能认固定指令**。
//! 「帮我看看上周那份报价单还差谁签字」这种开放指令，本地匹配永远做不了——那必须靠 LLM。
//!
//! 但反过来说，**日常高频的那十条左右指令不该每次都走模型**：
//!
//! ```text
//! ASR 文字 ──▶ ① 本地指令表（0ms、离线、可预测、用户自己配）
//!                    │ 没命中
//!                    └──▶ ② LLM 兜底（开放式理解）
//! ```
//!
//! 这也是业界主流形状：本地命令词 + 模型兜底。
//!
//! ## 安全边界（写死在代码里，不做成配置）
//!
//! - **绝不执行 shell。** 语音识别错一个字就变成在你机器上执行命令，而且没有撤销键。
//!   动作只有三种：打开 URL、POST 一个 webhook、内置动作。
//! - **URL 只允许 `http` / `https` / `mailto`。** `file:` 之类一律拒绝——
//!   否则「打开 X」可以被诱导去读本地文件。
//! - **webhook 只在用户显式配置后才会存在**（他自己的 Notion / n8n / Zapier 地址），
//!   我们不自带任何外部服务凭据。
//!
//! ## 匹配策略（jason 选的：精确 + 归一，没中走 LLM）
//!
//! 归一：去空白、去标点、转小写，然后**前缀匹配**（长的短语优先，
//! 所以「搜索 github」比「搜索」更具体时先命中它）。
//!
//! ⚠️ **繁简归一还没做**：那需要一张几千字的对照表，本仓库没有离线来源。
//! 所以「搜尋」不会匹配到「搜索」——**记在这里，别以为做了**。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 归一化：去空白、去标点、转小写。匹配只在这个形态上做。
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !is_punct(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn is_punct(c: char) -> bool {
    matches!(
        c,
        '。' | '，' | '、' | '？' | '！' | '；' | '：' | '“' | '”' | '‘' | '’' | '（' | '）'
            | '《' | '》' | '…' | '—' | '·' | '「' | '」'
    ) || c.is_ascii_punctuation()
}

/// 一个动作。**只有这三种**——见模块文档的安全边界。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// 内置动作（不需要任何外部凭据）。
    Builtin {
        /// 动作名，见 `BuiltinAction::parse` 的合法值。
        name: String,
        /// 值（切语系/语气时用，例如 `yue` / `warm`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// 用默认浏览器打开一个 URL。`{rest}` = 短语之后的内容，`{text}` = 整句。
    ///
    /// ⚠️ **只允许 http / https / mailto**（在 `validate` 里强制）。
    /// 「发邮件」推荐用 `mailto:` **打开草稿让人确认**，而不是真的替他发出去——
    /// 误识别的代价是「草稿」，不是「已发出」。
    OpenUrl { url: String },
    /// POST 一个 JSON 到用户自己的 webhook（Notion / n8n / Zapier / 自建服务）。
    HttpPost {
        url: String,
        /// 请求体模板（JSON 字符串），同样支持 `{rest}` / `{text}`。
        #[serde(default)]
        body: String,
    },
}

/// 内置动作的白名单。**加新动作时这里必须同步**，否则菜单/文档说的和实际能跑的对不上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinAction {
    /// 切语系（值 = `talk::STYLE_OPTIONS` 的键）。
    Style,
    /// 切语气。
    Tone,
    /// 切模式（值 = `input_method` / `conversation`）。
    Mode,
    /// 把这句话（去掉指令短语之后的部分，为空则整句）记一条到 `kb/`。
    Note,
}

impl BuiltinAction {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "style" => Ok(Self::Style),
            "tone" => Ok(Self::Tone),
            "mode" => Ok(Self::Mode),
            "note" => Ok(Self::Note),
            other => bail!("未知的内置动作 {other:?}；可选：style / tone / mode / note"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// 触发短语（人写的，原样保留——**归一化只在匹配时做**，
    /// 这样用户看到的就是他自己写的那句）。
    pub phrase: String,
    /// 同义说法。
    #[serde(default)]
    pub aliases: Vec<String>,
    pub action: Action,
    /// 备注，给用户自己看。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Command {
    /// 校验：URL 方案、内置动作名、空短语。
    ///
    /// **在写入与执行前都要过一遍**：配置是用户手工可编辑的，
    /// 只在写入时校验挡不住手改。
    pub fn validate(&self) -> Result<()> {
        if normalize(&self.phrase).is_empty() {
            bail!("触发短语不能为空");
        }
        match &self.action {
            Action::Builtin { name, value } => {
                let action = BuiltinAction::parse(name)?;
                if matches!(action, BuiltinAction::Style | BuiltinAction::Tone | BuiltinAction::Mode)
                    && value.as_deref().map(str::trim).unwrap_or("").is_empty()
                {
                    bail!("内置动作 {name} 需要 value");
                }
            }
            Action::OpenUrl { url } => check_scheme(url, &["http", "https", "mailto"])?,
            Action::HttpPost { url, .. } => check_scheme(url, &["http", "https"])?,
        }
        Ok(())
    }
}

fn check_scheme(url: &str, allowed: &[&str]) -> Result<()> {
    let lower = url.trim().to_lowercase();
    let scheme = lower.split(':').next().unwrap_or("");
    if !lower.contains(':') || !allowed.contains(&scheme) {
        bail!(
            "URL 方案不允许：{url:?}（只允许 {}）——`file:` 之类可以被诱导去读本地文件",
            allowed.join(" / ")
        );
    }
    Ok(())
}

/// 命中结果：命中的指令 + 短语之后的部分（槽位）。
///
/// ⚠️ `rest` 是**原文**（只把首尾的标点/空白修掉），**不是归一句子**。
/// 这一点踩过：一开始从归一句子截槽位，于是
/// 「帮我搜代码 talk.rs」的槽位变成 `talkrs`——点没了、大写也没了，
/// 搜索和记事都跟着变形，而且**测试当时把这个坏行为钉住了**
/// （`rest == "rustasync"` 看着还挺像那么回事）。归一只用于**判断命中**。
///
/// 槽位里的空格**保留**（`Rust async` 不会粘成 `Rustasync`）：
/// 粘起来只对「拼 URL」方便，对记事就是错的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub phrase: String,
    pub rest: String,
    pub action: Action,
}

/// 把「归一句子里短语的长度」映射回**原文**，返回短语之后剩下的原文。
///
/// 逐字符走原文：跳过不参与归一的字符（空白/标点）并累计归一长度，
/// 累计够了就切在这里。这样大小写、`.`、词间空格都原样留着。
fn rest_in_original(text: &str, phrase: &str) -> String {
    let want = phrase.chars().count();
    let mut seen = 0usize;
    for (idx, ch) in text.char_indices() {
        if !ch.is_whitespace() && !is_punct(ch) {
            // 用实际归一长度累计：少数字符小写后会展开成多个（例如 'İ'）
            seen += ch.to_lowercase().count();
        }
        if seen >= want {
            let after = &text[idx + ch.len_utf8()..];
            return after
                .trim_matches(|c: char| c.is_whitespace() || is_punct(c))
                .to_string();
        }
    }
    String::new()
}

/// 在指令表里找一条命中。**长的短语优先**（「搜索 github」比「搜索」更具体）。
pub fn match_text(commands: &[Command], text: &str) -> Option<Hit> {
    let normalized = normalize(text);
    if normalized.is_empty() {
        return None;
    }
    let mut candidates: Vec<(&Command, String)> = Vec::new();
    for cmd in commands {
        for phrase in std::iter::once(&cmd.phrase).chain(cmd.aliases.iter()) {
            let p = normalize(phrase);
            if !p.is_empty() && normalized.starts_with(&p) {
                candidates.push((cmd, p));
            }
        }
    }
    candidates.sort_by_key(|(_, p)| std::cmp::Reverse(p.chars().count()));
    let (cmd, phrase) = candidates.first()?;
    let rest = rest_in_original(text, phrase);
    Some(Hit {
        phrase: cmd.phrase.clone(),
        rest,
        action: cmd.action.clone(),
    })
}

/// 填进模板的值要不要按 URL 转义：看**模板**是不是 URL。
///
/// 搜索词里的空格/`+`/`#` 不转义会把 URL 拆坏（`q=Rust async` 到浏览器那里
/// 是「q=Rust」加一个野参数）；而 JSON body 里的值**不能**转义，
/// 转了就多出一堆 `%20`。所以判据落在模板上，不落在值上。
fn template_is_url(template: &str) -> bool {
    let t = template.trim_start().to_ascii_lowercase();
    t.starts_with("http://") || t.starts_with("https://") || t.starts_with("mailto:")
}

/// 把 `{rest}` / `{text}` 填进模板。两者都是**原文**（见 `Hit` 的注释）。
pub fn fill(template: &str, rest: &str, text: &str) -> String {
    let (rest, text) = if template_is_url(template) {
        (url_encode(rest), url_encode(text))
    } else {
        (rest.to_string(), text.to_string())
    };
    template.replace("{rest}", &rest).replace("{text}", &text)
}

/// 百分号转义。保留非保留字（`A-Za-z0-9-_.~`）与 `@` `:` `/`——
/// 后三个是留给 `mailto:a@b.com` 这类**本身就带结构的槽位**的；
/// 其余（空格、`+`、`#`、`&`、中文）一律转义，
/// 免得用户的搜索词把 URL 结构撑坏。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b'@' | b':' | b'/' => out.push(*byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 指令表落盘位置：`<数据目录>/commands.json`。
pub fn path_in(data_root: &Path) -> PathBuf {
    data_root.join("commands.json")
}

/// 读指令表。**文件不存在时给一份默认表**（jason 要的是「开箱能用」），
/// 坏文件则报错——不要静默退回默认，那会让用户以为自己的指令还在。
pub fn load(data_root: &Path) -> Result<Vec<Command>> {
    let path = path_in(data_root);
    if !path.exists() {
        return Ok(default_commands());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读不了指令表 {}", path.display()))?;
    let commands: Vec<Command> =
        serde_json::from_str(&raw).with_context(|| format!("指令表不是合法 JSON: {}", path.display()))?;
    for cmd in &commands {
        cmd.validate()
            .with_context(|| format!("指令 {:?} 不合法", cmd.phrase))?;
    }
    Ok(commands)
}

pub fn save(data_root: &Path, commands: &[Command]) -> Result<()> {
    for cmd in commands {
        cmd.validate()?;
    }
    let path = path_in(data_root);
    std::fs::create_dir_all(data_root)?;
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(commands)?))
        .with_context(|| format!("写指令表失败 {}", path.display()))?;
    Ok(())
}

/// 默认指令表：**开箱能用，且全部不碰外部凭据**。
///
/// 「发邮件」用 `mailto:` **打开草稿**让人确认，而不是替他发出去——
/// 语音误识别的代价应该是「一封没写完的草稿」，不是「一封已发出的邮件」。
pub fn default_commands() -> Vec<Command> {
    vec![
        Command {
            phrase: "搜索".into(),
            aliases: vec!["搜一下".into(), "帮我搜".into(), "查一下".into()],
            action: Action::OpenUrl {
                url: "https://www.google.com/search?q={rest}".into(),
            },
            note: Some("打开浏览器搜索。{rest} 是短语后面的内容".into()),
        },
        Command {
            phrase: "搜代码".into(),
            aliases: vec!["搜 github".into()],
            action: Action::OpenUrl {
                url: "https://github.com/search?q={rest}".into(),
            },
            note: None,
        },
        Command {
            phrase: "发邮件给".into(),
            aliases: vec!["写邮件给".into(), "发邮件".into()],
            action: Action::OpenUrl {
                url: "mailto:{rest}".into(),
            },
            note: Some("只打开邮件草稿，发送仍由你自己确认".into()),
        },
        Command {
            phrase: "记一下".into(),
            aliases: vec!["记一条".into(), "记下来".into()],
            action: Action::Builtin {
                name: "note".into(),
                value: None,
            },
            note: Some("把后面的内容记一条到 kb/".into()),
        },
        Command {
            phrase: "切换对话模式".into(),
            aliases: vec!["进入对话模式".into()],
            action: Action::Builtin {
                name: "mode".into(),
                value: Some("conversation".into()),
            },
            note: None,
        },
        Command {
            phrase: "切换输入法模式".into(),
            aliases: vec!["退出对话模式".into()],
            action: Action::Builtin {
                name: "mode".into(),
                value: Some("input_method".into()),
            },
            note: None,
        },
        Command {
            phrase: "说粤语".into(),
            aliases: vec!["用粤语".into(), "换成粤语".into(), "请你用粤语".into()],
            action: Action::Builtin {
                name: "style".into(),
                value: Some("yue".into()),
            },
            note: None,
        },
        Command {
            phrase: "说普通话".into(),
            aliases: vec!["用普通话".into(), "换成普通话".into()],
            action: Action::Builtin {
                name: "style".into(),
                value: Some("zh".into()),
            },
            note: None,
        },
        Command {
            phrase: "说英语".into(),
            aliases: vec!["用英语".into(), "换成英语".into(), "speakenglish".into()],
            action: Action::Builtin {
                name: "style".into(),
                value: Some("en".into()),
            },
            note: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(phrase: &str, action: Action) -> Command {
        Command {
            phrase: phrase.into(),
            aliases: vec![],
            action,
            note: None,
        }
    }

    #[test]
    fn normalization_strips_punctuation_and_case() {
        assert_eq!(normalize("搜索，GitHub。"), "搜索github");
        assert_eq!(normalize("  Hello, World!  "), "helloworld");
        // 泰语没有词间空格，但也不能把字符吃掉
        assert_eq!(normalize("สวัสดี"), "สวัสดี");
    }

    /// **长的短语优先**：否则「搜索 github」永远被「搜索」抢走，
    /// 用户会看到「打开了 Google 搜 'github'」而不是 GitHub 搜索。
    #[test]
    fn the_longest_phrase_wins() {
        let commands = vec![
            cmd(
                "搜索",
                Action::OpenUrl {
                    url: "https://google.com/search?q={rest}".into(),
                },
            ),
            cmd(
                "搜索github",
                Action::OpenUrl {
                    url: "https://github.com/search?q={rest}".into(),
                },
            ),
        ];
        let hit = match_text(&commands, "搜索 GitHub rust async").unwrap();
        assert_eq!(hit.phrase, "搜索github");
        assert_eq!(hit.rest, "rust async");
    }

    #[test]
    fn aliases_match_and_unmatched_text_returns_none() {
        let commands = vec![Command {
            phrase: "搜索".into(),
            aliases: vec!["帮我搜".into()],
            action: Action::Builtin {
                name: "note".into(),
                value: None,
            },
            note: None,
        }];
        assert!(match_text(&commands, "帮我搜一下 Rust").is_some());
        assert!(match_text(&commands, "今天天气怎么样").is_none());
        assert!(match_text(&commands, "").is_none());
    }

    /// 槽位取自**原文**：标点修掉、大小写和点号留住。
    ///
    /// 这条测试原来是反的（钉的是「归一化后截」），
    /// 结果是搜 `talk.rs` 变成搜 `talkrs`。见 `Hit` 的注释。
    #[test]
    fn rest_keeps_the_original_spelling() {
        let commands = vec![cmd(
            "搜索",
            Action::OpenUrl {
                url: "https://x/?q={rest}".into(),
            },
        )];
        let hit = match_text(&commands, "搜索，Docker Compose。").unwrap();
        assert_eq!(hit.rest, "Docker Compose");
        // 点号不能被吃掉：这是当初发现的那一例
        let hit = match_text(&commands, "搜索 talk.rs").unwrap();
        assert_eq!(hit.rest, "talk.rs");
        // 没命中就是没命中
        assert!(match_text(&commands, "").is_none());
    }

    /// **安全边界**：`file:` 一律拒绝——否则「打开 X」能被诱导去读本地文件。
    #[test]
    fn file_urls_are_rejected() {
        let bad = cmd(
            "打开",
            Action::OpenUrl {
                url: "file:///etc/passwd".into(),
            },
        );
        assert!(bad.validate().is_err());
        let good = cmd(
            "打开",
            Action::OpenUrl {
                url: "https://example.com".into(),
            },
        );
        assert!(good.validate().is_ok());
        // mailto 允许（发邮件用「打开草稿」而不是真发）
        let mail = cmd(
            "发邮件给",
            Action::OpenUrl {
                url: "mailto:{rest}".into(),
            },
        );
        assert!(mail.validate().is_ok());
    }

    #[test]
    fn builtin_actions_are_a_closed_set() {
        assert_eq!(BuiltinAction::parse("style").unwrap(), BuiltinAction::Style);
        assert!(BuiltinAction::parse("shell").is_err(), "不许有 shell 这个动作");
        assert!(BuiltinAction::parse("").is_err());
        // 切语系必须给值，否则跑起来不知道切到哪
        let no_value = cmd(
            "换语系",
            Action::Builtin {
                name: "style".into(),
                value: None,
            },
        );
        assert!(no_value.validate().is_err());
    }

    #[test]
    fn templates_fill_rest_and_text() {
        // URL 模板：值要转义（空格 → %20），否则 `q=Rust async` 到浏览器那边就散了
        assert_eq!(
            fill("https://x/?q={rest}&full={text}", "Rust async", "搜索 Rust async"),
            "https://x/?q=Rust%20async&full=%E6%90%9C%E7%B4%A2%20Rust%20async"
        );
        // JSON body（非 URL 模板）：**不转义**，否则 body 里全是 %20
        assert_eq!(
            fill("{\"title\":\"{rest}\"}", "明天要测 AEC", "记一下 明天要测 AEC"),
            "{\"title\":\"明天要测 AEC\"}"
        );
        // mailto 的结构字符留住
        assert_eq!(fill("mailto:{rest}", "a@b.com", ""), "mailto:a@b.com");
    }

    /// 槽位里是**语音听出来的任意内容**，不能让它把 URL 结构撑坏：
    /// `&` 会凭空多一个参数，`#` 会把后面整段吃掉。
    #[test]
    fn slot_cannot_break_out_of_the_url() {
        assert_eq!(
            fill("https://x/?q={rest}", "a&admin=1 #frag", ""),
            "https://x/?q=a%26admin%3D1%20%23frag"
        );
    }

    #[test]
    fn default_table_is_valid_and_covers_the_daily_ten() {
        let commands = default_commands();
        for c in &commands {
            c.validate().unwrap_or_else(|e| panic!("默认指令 {:?} 不合法: {e}", c.phrase));
        }
        // 默认表里必须能认出发邮件/搜索/记事/切语系这几类
        let text = |s: &str| match_text(&commands, s);
        assert!(text("搜索 rust 异步").is_some());
        assert!(text("发邮件给 a@b.com").is_some());
        assert!(text("记一下 明天要交报告").is_some());
        assert!(text("请你用粤语").is_some());
        assert!(text("今天天气怎么样").is_none(), "普通问句不该被指令表抢走");
    }

    #[test]
    fn roundtrip_through_json() {
        let dir = std::env::temp_dir().join(format!("agentear-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let commands = default_commands();
        save(&dir, &commands).unwrap();
        let back = load(&dir).unwrap();
        assert_eq!(back, commands);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
