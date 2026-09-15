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
    /// **强制二次确认**（除了「本来就必须确认」的那些）。
    ///
    /// 「会把内容送出这台机器」的动作（`http_post`、`mailto:`）**总是**要确认，
    /// 不看这个字段；这个字段是给**其余动作**用的额外保险，比如你想让
    /// 「搜索 ▁▁」也问一句。默认 `false`。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
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

/// 把 `{rest}` / `{text}` **原样**填进模板（不转义）。
///
/// 给「念给用户听」用：`mailto:a@b.com%20%E8%AE%A8...` 这种是给机器看的，
/// 念出来或印出来都没法读，而确认的关键恰恰是**用户能读懂要发什么**。
pub fn fill_raw(template: &str, rest: &str, text: &str) -> String {
    template.replace("{rest}", rest).replace("{text}", text)
}

/// 把 `{rest}` / `{text}` 填进模板。两者都是**原文**（见 `Hit` 的注释）。
///
/// ⚠️ 与 `fill_raw` 的关系要看清：**转义是运输层的编码，不是内容的改变**
/// （`%20` 解出来就是空格）。所以「念给用户的那份」与「发出去的那份」
/// 内容仍然一致——`confirm_prompt` 用 `fill_raw` 只是为了可读，
/// **不是**为了显示别的东西。
pub fn fill(template: &str, rest: &str, text: &str) -> String {
    let (rest, text) = if template_is_url(template) {
        (url_encode(rest), url_encode(text))
    } else {
        (rest.to_string(), text.to_string())
    };
    fill_raw(template, &rest, &text)
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

// ------------------------------------------------------- 向外动作的二次确认
//
// ## 为什么这一层必须有
//
// 「写 Notion / 发邮件」这一类动作有两个性质：
// **① 出了这台机器 ② 大多数不可撤**。而触发它们的是**语音识别**——
// 一个会听错的东西。听错一次描述、代价是「重说一遍」；
// 听错一次写出去，代价是别人收到了错的东西，而且**没有撤销键**。
// 所以这一类动作不靠「识别得准」，靠**执行前再问一次**。
//
// ## 为什么是「念出来 + 等你一句话」，而不是弹窗
//
// V1 是推键式：麦克风只在按键时开。弹窗要用户离开键盘去点，
// 在这个产品里等于把语音闭环打断两次。所以确认走**同一套语音回路**：
// 把「将要做什么、内容是什么」念出来，用户**按一下录音键**或者说「确认」。
// ⚠️ **必须把内容念出来**：只说「确认吗」的确认是假确认——
// 用户没法确认一个他不知道的东西。

/// 用户对「待确认动作」的答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// 明确同意 → 执行。
    Confirm,
    /// 明确不同意 → 丢弃。
    Cancel,
    /// 别的话 → **既不算同意也不算拒绝**：丢弃这一条，并照常当新的一轮处理。
    Other,
}

/// 肯定词。⚠️ 只认**独立的短答复**，不要在长句里认——
/// 「我确认一下天气」不是同意发邮件。
const YES: &[&str] = &[
    "确认", "确定", "对的", "对", "好的", "好", "是的", "是", "发送", "发出", "发吧", "可以",
    "行", "没问题", "yes", "ok", "send", "confirm",
];

/// 否定词。
const NO: &[&str] = &[
    "取消", "不用", "不要", "别发", "不发", "算了", "停下", "停", "no", "cancel", "算了",
];

/// 同意只在**短答复**里认（归一化后的字数上限）。
///
/// ⚠️ 这条是必需的，不是保守：**提到「确认」的长句通常不是同意**
/// （「我刚才确认过了吗」「帮我确认一下明天的会」）。
/// 只做子串匹配的话，这两句都会被当成「同意发送」。
/// 7 个字够放 `ok` / `yes` / `send` / `confirm` / 「好的发吧」「没问题」。
const MAX_CONSENT_CHARS: usize = 7;

/// 否定前缀。**肯定词前面挂一个它，就是否定**。
///
/// 光靠一张否定词表挡不住「不确认」「不要发」这类——它们含的是肯定词，
/// 而 `不`/`别` 只是一个前缀。这里按「前缀 + 肯定词」组合判，
/// 因为真正会出事的方向只有这一个：**误判成同意就发出去了**。
const NEG_PREFIXES: &[&str] = &["不", "别", "没", "勿", "无须", "不用", "不要"];

/// 一个词怎么算命中：**单字必须整句相等**，多字才允许包含。
///
/// 单字放行包含匹配会出人命类的问题：`嗯`、`对`、`行` 出现在
/// 「嗯……那个……对，我是说搜索」这种句子里时，用户显然不是在确认。
fn word_hits(normalized: &str, word: &str) -> bool {
    if word.chars().count() <= 1 {
        normalized == word
    } else {
        normalized.contains(word)
    }
}

/// 把「用户说的那句话」判成确认 / 取消 / 其它。
///
/// ⚠️ **否定必须先判。** 「不确认」「不要发」里都**含着肯定词**
/// （`确认` / `发`），先判肯定就会把「别发」执行成「发」——
/// 这是这一层唯一会真正出事的方向，所以顺序是承重的，有用例钉住。
pub fn classify_confirmation(text: &str) -> Reply {
    let n = normalize(text);
    if n.is_empty() {
        // 空转写**不是同意**：那是「按了键但没说话」，
        // 交给调用方按「按键确认」处理（见 `main.rs` 里那段）。
        return Reply::Other;
    }
    // ① 否定词表（拒绝**不设长度限制**：误判成拒绝只是重说一遍，
    //    误判成同意就发出去了 —— 这个方向的不对称是有意的）
    if NO.iter().any(|w| word_hits(&n, w)) {
        return Reply::Cancel;
    }
    // ② 否定前缀 + 肯定词（「不确认」「别发」「不好」）
    for w in YES {
        for neg in NEG_PREFIXES {
            if n.contains(&format!("{neg}{w}")) {
                return Reply::Cancel;
            }
        }
    }
    // ③ 同意：**必须在短答复里**，否则「提到确认」的长句会被当成同意
    if n.chars().count() <= MAX_CONSENT_CHARS && YES.iter().any(|w| word_hits(&n, w)) {
        return Reply::Confirm;
    }
    Reply::Other
}

fn is_mailto(url: &str) -> bool {
    url.trim_start().to_ascii_lowercase().starts_with("mailto:")
}

/// 这个动作执行前要不要问一句。
///
/// 判据是「**会不会把内容送出这台机器**」：
/// - `http_post` → 出去，而且大多数不可撤 → **一定问**
/// - `mailto:` → 把内容交给邮件客户端（虽然只到草稿，但内容已经出去了）→ **一定问**
/// - 打开网页（GET）→ 默认不问（搜索是高频动作，每次都问会让人把确认点成肌肉记忆，
///   那等于没有确认）；想让它也问就 `confirm: true`
/// - 本地内置动作 → 默认不问（切音色、切模式都在本机，且可逆）
pub fn needs_confirm(action: &Action, opt_in: bool) -> bool {
    match action {
        Action::HttpPost { .. } => true,
        Action::OpenUrl { url } => is_mailto(url) || opt_in,
        Action::Builtin { .. } => opt_in,
    }
}

/// 给用户念的确认问句：**说清要做什么，以及内容是什么**。
///
/// ⚠️ 不要写成「确认执行吗」——用户没法确认一个没被告知的东西。
pub fn confirm_prompt(action: &Action, rest: &str, text: &str) -> String {
    match action {
        Action::HttpPost { url, body } => {
            let payload = fill(body, rest, text);
            let shown: String = if payload.chars().count() > 120 {
                payload.chars().take(120).collect::<String>() + "…"
            } else {
                payload
            };
            format!("要把这条发到 {url}，内容是「{shown}」。确认就按一下键，或者说「确认」。")
        }
        Action::OpenUrl { url } if is_mailto(url) => {
            // 收件人/主题用**原文**展示：`mailto:a@b.com%20%E8%AE%A8` 没法读，
            // 而这一步的全部意义就是让用户看清收件人是谁。
            // `rest` 就是「发给谁 + 说什么」，直接念它比念一个 mailto: URL 有用。
            let shown = if rest.trim().is_empty() {
                fill_raw(url, rest, text)
            } else {
                rest.to_string()
            };
            format!("要打开邮件草稿：{shown}。确认就按一下键，或者说「确认」。")
        }
        Action::OpenUrl { url } => {
            let target = fill_raw(url, rest, text);
            format!("要打开 {target}。确认就按一下键，或者说「确认」。")
        }
        Action::Builtin { name, .. } => {
            format!("要执行 {name}。确认就按一下键，或者说「确认」。")
        }
    }
}

/// 从 webhook 的响应里挑出**最该给用户看的那一句**。
///
/// 存在的理由：Notion / n8n 这类服务写入成功后回的正是**新页面的 URL 或 id**，
/// 而 v0.12.x 之前我们把整个响应体丢掉了（`Stdio::null()`）——
/// 于是用户问「你写入到哪了、网址给我看看」时，**系统手里根本没有那个答案**。
/// 用户那句「你别骗我」是有道理的：没有回执的"已完成"就是一种骗。
///
/// 优先级：`url` → `link` → `id` → 正文里的第一个 http 链接 → 截断的原文。
/// 认不出结构也不算失败——那是**对方服务**的响应格式问题，不该让我们报错。
pub fn summarize_response(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["url", "link", "html_url", "id", "page_id"] {
            if let Some(val) = v.get(key) {
                let text = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if !text.trim().is_empty() {
                    return Some(text.trim().to_string());
                }
            }
        }
    }
    // 正文里裸着一条链接也认（很多自建服务的返回就是一行 URL）
    if let Some(start) = body.find("http") {
        let rest = &body[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '<' || c == '\\' || c == '}')
            .unwrap_or(rest.len());
        let url = &rest[..end];
        if url.len() > 8 {
            return Some(url.to_string());
        }
    }
    // 实在认不出就截一段原文——**总比什么都不给用户看好**
    let short: String = body.chars().take(200).collect();
    Some(if body.chars().count() > 200 {
        format!("{short}…")
    } else {
        short
    })
}

/// 待确认的动作。**带截止时间**：没人确认就作废，不能一直挂着。
#[derive(Debug, Clone)]
pub struct Pending {
    /// 念给用户听的那句话（含内容）。
    pub prompt: String,
    /// 命中时用来执行的原件。
    pub hit: Hit,
    /// 这一轮的完整正文。执行时**必须用它**，和 `prompt` 同源。
    pub text: String,
    /// 这一轮录音的 hash（执行时写 routes 用）。
    pub content_hash: String,
    pub deadline: std::time::Instant,
}

impl Pending {
    /// ⚠️ **问句和以后要执行的正文必须来自同一个 `text`。**
    ///
    /// 这一点是承重的：如果念给用户的是 A、真正发出去的是 B，
    /// 那这个「二次确认」就是走过场——**用户确认的必须正是要执行的东西**。
    /// 所以 `text` 存在这里，执行时用它，不重新推导。
    pub fn new(
        hit: Hit,
        content_hash: String,
        text: impl Into<String>,
        ttl: std::time::Duration,
    ) -> Self {
        let text = text.into();
        let prompt = confirm_prompt(&hit.action, &hit.rest, &text);
        Self {
            prompt,
            hit,
            text,
            content_hash,
            deadline: std::time::Instant::now() + ttl,
        }
    }

    /// 还没过期吗。**过期即作废**：一个挂着的向外动作比没有更危险。
    pub fn is_fresh(&self) -> bool {
        std::time::Instant::now() < self.deadline
    }

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
            confirm: false,
        },
        Command {
            phrase: "搜代码".into(),
            aliases: vec!["搜 github".into()],
            action: Action::OpenUrl {
                url: "https://github.com/search?q={rest}".into(),
            },
            note: None,
            confirm: false,
        },
        Command {
            phrase: "发邮件给".into(),
            aliases: vec!["写邮件给".into(), "发邮件".into()],
            action: Action::OpenUrl {
                url: "mailto:{rest}".into(),
            },
            note: Some("只打开邮件草稿，发送仍由你自己确认".into()),
            confirm: false,
        },
        Command {
            phrase: "记一下".into(),
            aliases: vec!["记一条".into(), "记下来".into()],
            action: Action::Builtin {
                name: "note".into(),
                value: None,
            },
            note: Some("把后面的内容记一条到 kb/".into()),
            confirm: false,
        },
        Command {
            phrase: "切换对话模式".into(),
            aliases: vec!["进入对话模式".into()],
            action: Action::Builtin {
                name: "mode".into(),
                value: Some("conversation".into()),
            },
            note: None,
            confirm: false,
        },
        Command {
            phrase: "切换输入法模式".into(),
            aliases: vec!["退出对话模式".into()],
            action: Action::Builtin {
                name: "mode".into(),
                value: Some("input_method".into()),
            },
            note: None,
            confirm: false,
        },
        Command {
            phrase: "说粤语".into(),
            aliases: vec!["用粤语".into(), "换成粤语".into(), "请你用粤语".into()],
            action: Action::Builtin {
                name: "style".into(),
                value: Some("yue".into()),
            },
            note: None,
            confirm: false,
        },
        Command {
            phrase: "说普通话".into(),
            aliases: vec!["用普通话".into(), "换成普通话".into()],
            action: Action::Builtin {
                name: "style".into(),
                value: Some("zh".into()),
            },
            note: None,
            confirm: false,
        },
        Command {
            phrase: "说英语".into(),
            aliases: vec!["用英语".into(), "换成英语".into(), "speakenglish".into()],
            action: Action::Builtin {
                name: "style".into(),
                value: Some("en".into()),
            },
            note: None,
            confirm: false,
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
            confirm: false,
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
            confirm: false,
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

    // -------------------------------------------- 向外动作的二次确认

    /// **会把内容送出去的动作一定要问。** 这是这一层的全部意义。
    #[test]
    fn outward_actions_always_need_confirmation() {
        let post = Action::HttpPost {
            url: "https://hooks.example.com/x".into(),
            body: String::new(),
        };
        assert!(needs_confirm(&post, false), "http_post 一定要问");

        let mail = Action::OpenUrl {
            url: "mailto:{rest}".into(),
        };
        assert!(needs_confirm(&mail, false), "mailto 一定要问（内容已经交出去了）");
    }

    /// 高频的「打开网页」默认不问，但用户可以给单条指令加 `confirm: true`。
    ///
    /// 理由写在这里：**每次都问的确认会变成肌肉记忆**，
    /// 用户会条件反射地按下去，那等于没有确认，还把搜索变慢了一倍。
    #[test]
    fn opening_a_page_is_not_confirmed_unless_opted_in() {
        let search = Action::OpenUrl {
            url: "https://www.google.com/search?q={rest}".into(),
        };
        assert!(!needs_confirm(&search, false));
        assert!(needs_confirm(&search, true), "单条指令可以自己要求确认");

        let local = Action::Builtin {
            name: "style".into(),
            value: Some("yue".into()),
        };
        assert!(!needs_confirm(&local, false), "切音色在本机、可逆，不必问");
    }

    /// ⚠️ **否定必须先判**：这条是承重用例。
    ///
    /// 「不确认」「不要发」里**含着肯定词**（`确认` / `发`）。
    /// 先判肯定就会把「别发」执行成「发」——这一层唯一会真正出事的方向。
    #[test]
    fn negation_is_checked_before_agreement() {
        assert_eq!(classify_confirmation("不确认"), Reply::Cancel);
        assert_eq!(classify_confirmation("不要发"), Reply::Cancel);
        assert_eq!(classify_confirmation("别发出去"), Reply::Cancel);
        assert_eq!(classify_confirmation("算了，取消"), Reply::Cancel);
        assert_eq!(classify_confirmation("no"), Reply::Cancel);
    }

    #[test]
    fn plain_agreement_is_a_confirm() {
        for yes in ["确认", "确定", "对", "好的", "好", "是", "发送", "可以", "行", "ok"] {
            assert_eq!(classify_confirmation(yes), Reply::Confirm, "{yes} 应当算同意");
        }
        // 带标点、带语气也认（匹配走归一化）
        assert_eq!(classify_confirmation("确认。"), Reply::Confirm);
        assert_eq!(classify_confirmation("好的，发吧"), Reply::Confirm);
    }

    /// **别的话既不是同意也不是拒绝** —— 调用方要按「新的一轮」处理，
    /// 而不是偷偷执行。
    #[test]
    fn anything_else_is_not_consent() {
        assert_eq!(classify_confirmation("今天天气怎么样"), Reply::Other);
        assert_eq!(classify_confirmation(""), Reply::Other, "空转写不是同意");
        assert_eq!(classify_confirmation("   "), Reply::Other);
        // 单字肯定词**不许在长句里命中**：「嗯」出现在句首的犹豫句里不是同意
        assert_eq!(classify_confirmation("嗯那个我是说搜索"), Reply::Other);
        // 提到「确认」但在说别的事 —— **长句里不认同意**
        assert_eq!(classify_confirmation("我刚才确认过了吗"), Reply::Other);
        assert_eq!(classify_confirmation("帮我确认一下明天的会"), Reply::Other);
    }

    /// 单字必须整句相等，多字才允许包含。
    #[test]
    fn single_character_agreement_must_be_the_whole_utterance() {
        assert_eq!(classify_confirmation("对"), Reply::Confirm);
        assert_eq!(classify_confirmation("对的"), Reply::Confirm);
        // 疑问句含「不对」→ 判成拒绝。**语义上不精确，方向是安全的**：
        // 待确认动作不会执行，用户再说一次就行。下面钉的是这条性质。
        assert_ne!(classify_confirmation("对不对"), Reply::Confirm, "疑问句绝不能算同意");
        // 「行不行」是疑问句。它落成 Cancel 而不是 Other（含「不行」）——
        // **语义上不精确，但方向是安全的**：待确认动作不会被执行，
        // 用户再说一次「确认」就行。所以这里钉的是**性质**（绝不是同意），
        // 不是那个标签本身。
        assert_ne!(
            classify_confirmation("行不行"),
            Reply::Confirm,
            "疑问句绝不能算同意"
        );
    }

    /// 确认问句必须**带上内容**——只说「确认吗」的确认是假确认。
    #[test]
    fn the_prompt_tells_the_user_what_will_be_sent() {
        let post = Action::HttpPost {
            url: "https://hooks.example.com/notion".into(),
            body: r#"{"title":"{rest}"}"#.into(),
        };
        let prompt = confirm_prompt(&post, "明天要测 AEC", "记一下 明天要测 AEC");
        assert!(prompt.contains("hooks.example.com"), "{prompt}");
        assert!(prompt.contains("明天要测 AEC"), "必须念出内容：{prompt}");

        let mail = Action::OpenUrl {
            url: "mailto:{rest}".into(),
        };
        let prompt = confirm_prompt(&mail, "a@b.com", "发邮件给 a@b.com");
        assert!(prompt.contains("a@b.com"), "{prompt}");
        assert!(
            !prompt.contains("{rest}") && !prompt.contains("mailto:"),
            "给用户看的不该是模板或 mailto: 前缀（要能读懂收件人）：{prompt}"
        );
    }

    /// 超长的 body 要截断——念给用户听的东西不能无限长。
    #[test]
    fn a_huge_payload_is_truncated_in_the_prompt() {
        let post = Action::HttpPost {
            url: "https://x/y".into(),
            body: "{text}".into(),
        };
        let long = "很长的内容".repeat(80);
        let prompt = confirm_prompt(&post, "", &long);
        assert!(prompt.chars().count() < 220, "念出来要短：{}", prompt.chars().count());
        assert!(prompt.ends_with("。") || prompt.contains('…'), "{prompt}");
    }

    /// **念给用户的 == 将要发出去的。** 这条不成立的话，
    /// 「二次确认」就只是让用户点了个头，内容却可以不是他看到的那份。
    #[test]
    fn the_prompt_uses_the_same_text_that_will_be_sent() {
        let hit = Hit {
            phrase: "记到notion".into(),
            rest: "明天要测 AEC".into(),
            action: Action::HttpPost {
                url: "https://hooks.example.com/n".into(),
                body: r#"{"title":"{rest}","full":"{text}"}"#.into(),
            },
        };
        let text = "记到notion 明天要测 AEC";
        let pending = Pending::new(hit, "h".into(), text, std::time::Duration::from_secs(30));
        // 执行侧会对同一个 body 模板做同样的 fill
        let payload = fill(r#"{"title":"{rest}","full":"{text}"}"#, &pending.hit.rest, &pending.text);
        assert!(payload.contains("明天要测 AEC"), "{payload}");
        assert!(payload.contains("记到notion"), "`{{text}}` 要拿到整句：{payload}");
        assert!(
            pending.prompt.contains("明天要测 AEC"),
            "问句里必须有要发的内容：{}",
            pending.prompt
        );
    }

    /// **回执**：写入成没成、写到哪了，要从对方服务的响应里读出来给用户。
    ///
    /// 这条是「你别骗我」那句抱怨的落点：没有回执的「已完成」就是一种骗。
    #[test]
    fn the_webhook_reply_is_read_back_for_the_user() {
        // Notion / n8n 这类服务写入成功后回的就是 URL 或 id
        assert_eq!(
            summarize_response(r#"{"url":"https://notion.so/abc123","id":"abc123"}"#).unwrap(),
            "https://notion.so/abc123"
        );
        assert_eq!(
            summarize_response(r#"{"object":"page","id":"abc-123"}"#).unwrap(),
            "abc-123"
        );
        // 自建服务常常就回一行裸 URL
        assert_eq!(
            summarize_response("ok https://example.com/new/1\n").unwrap(),
            "https://example.com/new/1"
        );
        // 认不出结构也不报错，截一段原文（**总比什么都不给好**）
        let long = "好".repeat(300);
        let short = summarize_response(&long).unwrap();
        assert!(short.chars().count() <= 201, "要截断：{}", short.chars().count());
        assert!(short.ends_with('…'));
        // 空响应就是没有回执，不要编一个出来
        assert!(summarize_response("").is_none());
        assert!(summarize_response("   \n ").is_none());
    }

    /// 待确认是会**过期**的：一个永远挂着的向外动作比没有更危险。
    #[test]
    fn a_pending_action_expires() {
        let hit = Hit {
            phrase: "发邮件给".into(),
            rest: "a@b.com".into(),
            action: Action::OpenUrl {
                url: "mailto:{rest}".into(),
            },
        };
        let fresh = Pending::new(
            hit.clone(),
            "h".into(),
            "发邮件给 a@b.com",
            std::time::Duration::from_secs(30),
        );
        assert!(fresh.is_fresh());
        assert!(
            fresh.prompt.contains("a@b.com"),
            "问句要自己从 hit 和 text 生成：{}",
            fresh.prompt
        );

        let expired = Pending::new(hit, "h".into(), "发邮件给 a@b.com", std::time::Duration::ZERO);
        assert!(!expired.is_fresh(), "不能留一个永不过期的待确认");
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
