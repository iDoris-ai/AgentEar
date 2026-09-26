//! 通话链路的引擎适配层与播放（ADR-0007 §4 的 R0/R1 里属于 R0 的那一半）。
//!
//! 一次「轮次」是：**录音 → ASR → LLM → TTS → 播放**。ASR 走既有的
//! `engine::AsrEngine`（本模块不重复抽象它），这里补三件它没有的：
//!
//! | 引擎 | 多后端 | 零依赖默认实现 |
//! |---|---|---|
//! | `LlmEngine` | ✅ OpenAI 兼容端点（模型可换）/ 自带 mock | ✅ mock |
//! | `TtsEngine` | ✅ VoxCPM2 边车 / macOS `say` | ✅ `say` |
//! | 播放 | 只有一个（`afplay` 子进程） | — |
//!
//! **为什么音频播放走 `afplay` 子进程而不是 cpal**：打断要求「立刻掐掉
//! 正在放的声音」，而杀一个子进程是确定的、可测的；cpal 的输出流要自己
//! 管缓冲与 drain，掐掉之后还有多少已经进了 CoreAudio 的缓冲是不确定的。
//! V1 的打断阈值是 300 ms（`benchmarks-m3.md` §7.4），确定性比省一个进程值钱。
//!
//! **不引 HTTP 客户端库**，理由与 `sidecar.rs` / `download.rs` 相同：
//! 连的是 127.0.0.1，不需要 TLS，而 reqwest 会带进上百个传递依赖。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::sidecar::{self, Transport};

/// 通话支持的语言。**只有这三种**（jason 2026-09-08 的清单）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TalkLang {
    #[default]
    Zh,
    En,
    Th,
}

impl TalkLang {
    pub const NAMES: &'static [&'static str] = &["zh", "en", "th"];

    pub fn as_str(self) -> &'static str {
        match self {
            TalkLang::Zh => "zh",
            TalkLang::En => "en",
            TalkLang::Th => "th",
        }
    }

    /// 给系统提示用的人话名字。**不要**把 `zh` 直接写进提示词里。
    pub fn human(self) -> &'static str {
        match self {
            TalkLang::Zh => "中文",
            TalkLang::En => "English",
            TalkLang::Th => "ภาษาไทย",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "zh" => Ok(TalkLang::Zh),
            "en" => Ok(TalkLang::En),
            "th" => Ok(TalkLang::Th),
            other => bail!(
                "通话语言只认 {}，收到 {other:?}",
                Self::NAMES.join(" / ")
            ),
        }
    }

    /// 兜底音色（`say` 后端）。VoxCPM2 不用这个：它是零样本音色，
    /// 语言从文本推断，没有按语言选的发音人。
    pub fn say_voice(self) -> &'static str {
        match self {
            TalkLang::Zh => "Tingting",
            TalkLang::En => "Samantha",
            TalkLang::Th => "Kanya",
        }
    }
}

/// 一次通话里，模型该遵守的约束。
///
/// ⚠️ **天气那条事实是本地写死的，不是天气接口**（ADR-0007 §4.6）。
/// 它的作用只是「让模型有东西可说」，从而证明 ASR→LLM→TTS 整条链路通了。
/// 文档和提示词都要写清这一点，否则会被当成智能助手来评价。
pub fn system_prompt(lang: TalkLang, weather_fact: &str) -> String {
    // ⚠️ 「只使用 X，不要混用其他语言」这一句不是啰嗦。
    // 实测（2026-09-14，MiniCPM5-2B-4bit）：只说「用泰语回答」时，
    // 它会用中文回一句泰语问题；而泰语那一轮即使答了泰语，也会夹一个
    // 英文词（`overall`）。MiniCPM5-2B 的模型卡只声明 en/zh，
    // 泰语是它能力边界外的东西——所以提示词要尽量把语言钉死，
    // **并把「泰语质量有限」写进文档而不是假装它和中文一样好**。
    let language_rule = match lang {
        TalkLang::Zh => "必须用中文回答".to_string(),
        _ => format!(
            "必须只使用{}回答，一个其他语言的词都不要出现（专有名词除外）",
            lang.human()
        ),
    };
    format!(
        "你是 AgentEar 的语音助手，正在和用户打电话。\
         {language_rule}，一到两句，总共不超过 40 个字，直接给结论。\
         不要 markdown、不要列表、不要表情符号、不要念出标点符号。\
         已知本地事实：{weather_fact} 用户问天气时就用这条事实回答，\
         不要说自己无法获取天气数据。"
    )
}

// ---------------------------------------------------------------- LLM 引擎

pub trait LlmEngine: Send + Sync {
    fn name(&self) -> &'static str;
    /// 一轮问答。**返回的是要念出来的正文**，不含任何思考过程。
    fn reply(&self, system: &str, user: &str, lang: TalkLang) -> Result<String>;

    /// 流式版：**每攒出一段就回调一次**，返回完整正文。
    ///
    /// 默认实现直接调 `reply` 再一次性回调——**语义正确，只是不省时间**。
    /// 这样 `WeatherMock` 之类不必关心流式，而调用方只管调这一个入口。
    fn reply_stream(
        &self,
        system: &str,
        user: &str,
        lang: TalkLang,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<String> {
        let text = self.reply(system, user, lang)?;
        on_delta(&text);
        Ok(text)
    }

    /// 是否真的逐段产出。调用方据此判断「边出边合成」能不能省下首字时间。
    fn streams(&self) -> bool {
        false
    }
}

/// 零依赖的写死回答（ADR-0007 §4.6）。
///
/// 用途只有一个：**在没有模型、没有网络的情况下证明链路是通的**。
/// 它不智能，也不打算智能——所以它只在用户显式配置时才启用。
pub struct WeatherMock {
    facts: String,
}

impl WeatherMock {
    pub fn new(facts: impl Into<String>) -> Self {
        Self { facts: facts.into() }
    }

    /// 中英泰三种问法都认。**认不出时给一句通用回答**，而不是报错——
    /// mock 的职责是「让链路有东西可念」，不是判断用户问了什么。
    fn is_weather(&self, text: &str) -> bool {
        const KEYS: &[&str] = &[
            "天气", "下雨", "气温", "温度", "weather", "rain", "temperature", "อากาศ", "ฝน",
        ];
        let lower = text.to_lowercase();
        KEYS.iter().any(|k| lower.contains(k))
    }
}

impl LlmEngine for WeatherMock {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn reply(&self, _system: &str, user: &str, _lang: TalkLang) -> Result<String> {
        if self.is_weather(user) {
            return Ok(self.facts.clone());
        }
        Ok("我在本地跑，这一轮只做天气这一个演示。".to_string())
    }
}

/// 任何 OpenAI 兼容端点：本机的 MiniCPM5-2B 边车、别的模型、别人自己的服务。
///
/// **模型可换就是这一条**：换模型只改 `talk_llm_url` / 边车的
/// `AGENTEAR_TALK_LLM_MODEL`，Rust 这边一个字都不动（ADR-0007 §4.2 原则 1）。
pub struct OpenAiCompat {
    url: String,
    transport: Arc<dyn Transport>,
    timeout_secs: u64,
}

impl OpenAiCompat {
    pub fn new(url: impl Into<String>, transport: Arc<dyn Transport>, timeout_secs: u64) -> Self {
        Self {
            url: url.into(),
            transport,
            timeout_secs,
        }
    }
}

impl LlmEngine for OpenAiCompat {
    fn name(&self) -> &'static str {
        "openai_compat"
    }

    fn reply(&self, system: &str, user: &str, _lang: TalkLang) -> Result<String> {
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": 160,
            "temperature": 0.3,
        })
        .to_string();
        let raw = self
            .transport
            .post_json(&self.url, &body, self.timeout_secs)
            .context("通话 LLM 请求失败")?;
        let text = sidecar::extract_content(&raw)?;
        Ok(strip_thinking(&text))
    }

    fn streams(&self) -> bool {
        self.transport.supports_stream()
    }

    /// 走 OpenAI 兼容的 SSE：`"stream": true`，逐行 `data: {...}`。
    ///
    /// ⚠️ **思考段要边收边扣**：`strip_thinking` 是「整段文本」的函数，
    /// 而流式下我们拿到的是一段段碎片——一个 ` thinking` 还没闭合时就把
    /// 里面的字念出来，用户会听到模型的自言自语。
    /// 判据是「最后一个 `<think` 比最后一个 `</think>` 更靠后」，
    /// 在这种状态下**一个字符都不往下游放**。
    fn reply_stream(
        &self,
        system: &str,
        user: &str,
        lang: TalkLang,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<String> {
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": 160,
            "temperature": 0.3,
            "stream": true,
        })
        .to_string();

        let mut acc = String::new();
        // 回调里要区分「正文」和「思考」：先在本地按未闭合思考段截断，
        // 再把新增的正文交给调用方。
        let mut emitted = 0usize;
        let mut err: Option<anyhow::Error> = None;
        {
            let mut on_line = |line: &str| {
                if err.is_some() {
                    return;
                }
                let Some(payload) = line.strip_prefix("data:") else {
                    return; // 空行、注释行（`: keep-alive`）、`event:` 都不管
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    return;
                }
                match sse_delta(payload) {
                    Ok(Some(delta)) => {
                        acc.push_str(&delta);
                        let safe = think_filtered(&acc);
                        if safe.len() > emitted {
                            let fresh = safe[emitted..].to_string();
                            emitted = safe.len();
                            on_delta(&fresh);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => err = Some(e),
                }
            };
            // 流式失败（比如对面不支持 `stream`）就退回整句路径：
            // **宁可慢一点，也不要这一轮没声音**。
            match self
                .transport
                .post_sse_lines(&self.url, &body, self.timeout_secs, &mut on_line)
            {
                Ok(()) => {}
                Err(e) => {
                    log::warn!("流式通话失败（{e:#}），退回整句路径");
                    // ⚠️ **`on_delta` 必须照发**：下游（切句 → 合成 → 播放）
                    // 全靠它驱动。这里少调一次，用户拿到的是「有文字、没声音」——
                    // 比慢得多更糟，而且日志里一切正常。实测踩到过。
                    let text = self.reply(system, user, lang)?;
                    on_delta(&text);
                    return Ok(text);
                }
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
        let text = strip_thinking(&acc);
        if text.is_empty() {
            bail!("流式通话没拿到任何正文");
        }
        Ok(text)
    }
}

/// 流式下「现在就可以念出来」的正文。
///
/// 分两步：先去闭合的思考段（复用 `strip_thinking` 的规则），再把**尚未闭合**
/// 的思考段扣住——流式的好处在这里，也是它的风险：一个 ` thinking` 还没等到
/// `</think>`，里面的字就已经到手了，**不扣住就会听见模型的自言自语**。
/// 标签被切在半个字上（`<thi`）时也一起扣住，等后续字节补齐再放行。
///
/// ⚠️ 这与 `strip_thinking` 对**未闭合**段的规则不同（那边不吞，见它的用例）。
/// 差别是有理由的：一次性路径拿到的是一整段文本，无法区分「真的是思考段」
/// 和「正文里恰好写了 `<think`」；流式路径明确知道**这个标签正在写**。
/// 保守方向是「宁可少念一句」。
fn think_filtered(text: &str) -> String {
    let out = strip_thinking(text);
    if let Some(pos) = out.rfind("<think") {
        return out[..pos].to_string();
    }
    // 尾部可能是半个标签：从长到短找最长的那个前缀
    for k in (1..THINK_TAG.len()).rev() {
        if out.ends_with(&THINK_TAG[..k]) {
            return out[..out.len() - k].to_string();
        }
    }
    out
}

/// `think_filtered` 里用来判断「尾部是不是半个标签」的那个标签。
const THINK_TAG: &str = "<think";

/// 一行 SSE 里的 `choices[0].delta.content`。
///
/// `Ok(None)` = 这一块没有正文（首包只带 role、末包带 finish_reason），
/// **不是错误**。
fn sse_delta(payload: &str) -> Result<Option<String>> {
    let v: serde_json::Value =
        serde_json::from_str(payload).with_context(|| format!("SSE 块不是合法 JSON: {payload}"))?;
    Ok(v["choices"][0]["delta"]["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

/// 去掉模型可能带出来的 ` thinking... response` 段。
///
/// 边车那边已经用 `--chat-template-args '{"enable_thinking": false}'`
/// 关掉了思考模式（MiniCPM5 的模板里有这个开关），但**换模型之后不保证
/// 还有这个开关**——而这段文字要被念出来，漏出去就是用户听了一耳朵内心独白。
/// 所以在客户端再兜一层：这道防线不依赖任何模型的具体行为。
pub fn strip_thinking(text: &str) -> String {
    let mut out = text.to_string();
    while let (Some(start), Some(end)) = (out.find("<think"), out.find("</think>")) {
        if end < start {
            break;
        }
        let after = out[end + "</think>".len()..].to_string();
        out = format!("{}{}", &out[..start], after);
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------- TTS 引擎

pub trait TtsEngine: Send + Sync {
    fn name(&self) -> &'static str;
    /// 合成为一段完整的 16-bit PCM WAV 字节。
    fn synthesize(&self, text: &str, lang: TalkLang) -> Result<Vec<u8>>;
    /// 是否依赖外部进程/服务。给诊断与日志用。
    fn endpoint(&self) -> Option<&str> {
        None
    }
}

/// 走 `services/tts` 的 HTTP 边车（默认 VoxCPM2-4bit）。
///
/// 契约见 `services/tts/README.md`：`POST /speak {text, lang} -> audio/wav`。
/// **Rust 侧不关心后端是 VoxCPM2 还是 say**——那正是边车契约存在的意义。
pub struct HttpTts {
    url: String,
    transport: Arc<dyn AudioTransport>,
    timeout_secs: u64,
    /// 默认音色（None = 用边车的默认）。菜单改配置，下一轮生效。
    voice: Option<String>,
    style: String,
    tone: String,
}

impl HttpTts {
    pub fn new(url: impl Into<String>, transport: Arc<dyn AudioTransport>, timeout_secs: u64) -> Self {
        Self {
            url: url.into(),
            transport,
            timeout_secs,
            voice: None,
            style: "zh".to_string(),
            tone: "warm".to_string(),
        }
    }

    /// 带上音色 / 语系 / 语气（都来自配置，菜单改了下一轮生效）。
    pub fn with_voice(mut self, voice: Option<String>, style: String, tone: String) -> Self {
        self.voice = voice.filter(|v| !v.trim().is_empty());
        self.style = style;
        self.tone = tone;
        self
    }
}

impl TtsEngine for HttpTts {
    fn name(&self) -> &'static str {
        "http"
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.url)
    }

    fn synthesize(&self, text: &str, lang: TalkLang) -> Result<Vec<u8>> {
        // 音色/语系/语气都随请求走：这样菜单改完**下一轮立刻生效**，
        // 不用重启边车、也不用重启守护进程。
        let mut payload = serde_json::json!({
            "text": text,
            "lang": lang.as_str(),
            "style": self.style,
            "tone": self.tone,
        });
        if let Some(voice) = &self.voice {
            payload["voice"] = serde_json::Value::String(voice.clone());
        }
        let body = payload.to_string();
        let url = format!("{}/speak", self.url.trim_end_matches('/'));
        let bytes = self
            .transport
            .post_json_bytes(&url, &body, self.timeout_secs)
            .context("TTS 边车请求失败")?;
        validate_wav(&bytes)?;
        Ok(bytes)
    }
}

/// 零依赖保底：macOS 的 `say` + `afconvert`（ADR-0007 §4.2 表格里 TTS 那一格）。
///
/// 它比 VoxCPM2 难听得多，但**不需要任何模型文件、任何 Python 环境**，
/// 是「边车没装/没起」时链路依然能出声的兜底。
pub struct SayTts {
    timeout_secs: u64,
}

impl SayTts {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }
}

impl TtsEngine for SayTts {
    fn name(&self) -> &'static str {
        "say"
    }

    fn synthesize(&self, text: &str, lang: TalkLang) -> Result<Vec<u8>> {
        let dir = temp_dir("agentear-say")?;
        let aiff = dir.join("speech.aiff");
        let wav = dir.join("speech.wav");
        let run = |cmd: &mut Command| -> Result<()> {
            let out = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .context("启动 macOS 音频工具失败")?;
            if !out.status.success() {
                bail!(
                    "{} 失败（{:?}）：{}",
                    cmd.get_program().to_string_lossy(),
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        };
        // 文本走 stdin：它可能含引号和换行，塞进 argv 会被 shell 语义咬到，
        // 也有长度上限（services/tts 里同一条理由）。
        let text = text.to_string();
        let mut say = Command::new("/usr/bin/say");
        say.arg("-v").arg(lang.say_voice()).arg("-o").arg(&aiff).arg("-f").arg("-");
        let mut child = say
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("启动 say 失败")?;
        child
            .stdin
            .take()
            .context("拿不到 say 的 stdin")?
            .write_all(text.as_bytes())?;
        let out = child.wait_with_output().context("等待 say 失败")?;
        if !out.status.success() {
            bail!("say 失败：{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        run(Command::new("/usr/bin/afconvert")
            .arg(&aiff)
            .arg(&wav)
            .arg("-d")
            .arg("LEI16")
            .arg("-f")
            .arg("WAVE"))?;
        let bytes = std::fs::read(&wav).context("读 say 的输出失败")?;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = self.timeout_secs; // say 自己有墙钟上限，这里不重复造
        validate_wav(&bytes)?;
        Ok(bytes)
    }
}

/// 二进制 HTTP POST：TTS 返回的是音频，不能走 `String`。
///
/// `sidecar::Transport` 的 `post_json` 返回 `String`，把 WAV 字节当 UTF-8
/// 读进来会在第一个非法序列处丢掉内容（而且是静默的，往往只剩一个空文件）。
pub trait AudioTransport: Send + Sync {
    fn post_json_bytes(&self, url: &str, body: &str, timeout_secs: u64) -> Result<Vec<u8>>;
}

/// 生产实现：`curl` 把响应体直接写到 stdout，父进程按字节读回来。
pub struct CurlAudio;

impl AudioTransport for CurlAudio {
    fn post_json_bytes(&self, url: &str, body: &str, timeout_secs: u64) -> Result<Vec<u8>> {
        let mut cmd = Command::new("/usr/bin/curl");
        cmd.arg("-fsS")
            .arg("--max-time")
            .arg(timeout_secs.to_string())
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("--data-binary")
            .arg("@-")
            .arg(url);
        run_with_deadline_bytes(cmd, body, Duration::from_secs(timeout_secs + 5))
    }
}

/// 与 `sidecar::run_with_deadline` 同一个形状，只是**按字节**收 stdout。
///
/// 三个方向各一个线程：写 stdin、读 stdout、读 stderr。少任何一个都可能
/// 死锁——父进程在写、子进程在写 stdout、管道缓冲满了谁也动不了
/// （`sidecar.rs` 那轮踩过同一个坑）。
fn run_with_deadline_bytes(mut cmd: Command, stdin_body: &str, deadline: Duration) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动子进程失败")?;

    let mut stdin = child.stdin.take().context("拿不到 stdin")?;
    let body = stdin_body.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(body.as_bytes());
        drop(stdin);
    });

    let mut out_pipe = child.stdout.take().context("拿不到 stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });

    let mut err_pipe = child.stderr.take().context("拿不到 stderr")?;
    let err_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = err_pipe.read_to_string(&mut buf);
        let n = buf.chars().count();
        buf.chars().skip(n.saturating_sub(2048)).collect::<String>()
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writer.join();
                    let _ = reader.join();
                    let _ = err_reader.join();
                    bail!("子进程超过墙钟上限 {deadline:?}，已终止");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("等待子进程失败");
            }
        }
    };

    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if !status.success() {
        bail!("子进程退出码 {:?}: {}", status.code(), err.trim());
    }
    Ok(out)
}

/// 收到的东西**必须真是一段非空 WAV**。
///
/// 不校验的话，一个返回 JSON 错误体、状态码却是 200 的边车会让
/// `afplay` 去放一个文本文件——用户听到的是沉默，而日志里一切正常。
pub fn validate_wav(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 44 {
        bail!("TTS 返回的不是 WAV：只有 {} 字节", bytes.len());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("TTS 返回的不是 WAV：缺 RIFF/WAVE 头");
    }
    Ok(())
}

// ---------------------------------------------------------------- 播放

fn temp_dir(prefix: &str) -> Result<PathBuf> {
    let unique = format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&path).with_context(|| format!("建临时目录失败: {}", path.display()))?;
    Ok(path)
}

/// `afplay` 的句柄。**打断靠它**：`stop()` 杀掉子进程，声音立刻停。
pub struct Player {
    child: Child,
    dir: PathBuf,
}

/// 当前正在播的那个句柄。
///
/// **为什么是全局的**：录音键的事件在主线程上处理，而播放发生在工作线程
/// （`finish` 那条路）。V1 的打断就是「用户在主线程按了键，要去掐掉工作
/// 线程正在放的声音」，两者之间必须有一个共享点——就是这个。
///
/// 和 `tray::set` 用原子变量传状态是同一个套路：**不为一次跨线程的
/// 「停」信号去引入一整套消息通道**。
static PLAYING: std::sync::Mutex<Option<Player>> = std::sync::Mutex::new(None);

/// 打断次数。**流式播放要靠它才知道「这一句是被掐掉的」**：
/// `play_blocking` 被打断时返回的时长与「播完」长得一样，
/// 光看时长分不出来——而流式下分不出来就会继续把后面的句子往下播，
/// 用户按了键却还在被念，这是最刺眼的一类 bug。
static INTERRUPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 打断计数器的当前值。调用方记下开始时的值，事后比对。
pub fn interrupts() -> u64 {
    INTERRUPTS.load(std::sync::atomic::Ordering::SeqCst)
}

/// 掐掉正在播的回答。**给录音键调用**（V1 的打断入口）。
///
/// 返回 `true` 表示确实掐掉了东西。没在播时是 no-op，调用方不必先查状态。
pub fn stop_playback() -> bool {
    let Ok(mut slot) = PLAYING.lock() else {
        return false;
    };
    match slot.take() {
        Some(mut player) => {
            player.stop();
            INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            log::info!("打断：掐掉正在播放的回答");
            true
        }
        None => false,
    }
}

impl Player {
    /// 写临时文件并起 `afplay`，**不阻塞**。
    pub fn start(wav_bytes: &[u8]) -> Result<Self> {
        let dir = temp_dir("agentear-play")?;
        let path = dir.join("reply.wav");
        std::fs::write(&path, wav_bytes).context("写播放用临时文件失败")?;
        let child = Command::new("/usr/bin/afplay")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("启动 afplay 失败")?;
        Ok(Self { child, dir })
    }

    /// 播完了没有。`Ok(true)` = 已经退出（正常或被掐）。
    pub fn is_done(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// 掐掉正在放的声音，并清掉临时文件。幂等。
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // 播放句柄被丢下时不能留下还在响的声音，也不能留下临时文件。
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 播到底。**返回实际播了多久**，供打断延迟的测量使用。
///
/// 播放期间句柄挂在全局槽上，录音键因此能掐掉它。被 `stop_playback` 掐掉
/// 之后，这里的循环会在下一次轮询发现句柄已经不在了并正常返回——
/// **被打断不是错误**：调用方拿到的是一个明显短于音频总长的时长，
/// 这正是打断延迟可以直接量出来的地方。
pub fn play_blocking(wav_bytes: &[u8]) -> Result<Duration> {
    let started = Instant::now();
    let player = Player::start(wav_bytes)?;
    {
        let mut slot = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
        // 上一句要是还没收干净（正常不会，播放是串行的）就先让它收掉，
        // 绝不留下两个同时在响的句柄。
        if let Some(mut old) = slot.replace(player) {
            old.stop();
        }
    }
    loop {
        {
            let mut slot = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_mut() {
                Some(player) => {
                    if player.is_done() {
                        slot.take(); // Drop 会清掉临时文件
                        return Ok(started.elapsed());
                    }
                }
                None => return Ok(started.elapsed()), // 被掐了
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------- 组装

/// 按配置组装这一轮要用的两个引擎。
pub struct Engines {
    pub llm: Arc<dyn LlmEngine>,
    pub tts: Arc<dyn TtsEngine>,
}

impl Engines {
    /// 从配置派生的引擎组合。
    ///
    /// ⚠️ **LLM 默认是外部端点，不是 mock。** mock 要用户显式选
    /// （`talk_llm_engine: "mock"`）——否则用户会以为「模型在跑」，
    /// 实际听到的是写死的句子。
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let llm: Arc<dyn LlmEngine> = match cfg.talk_llm_engine.as_str() {
            "mock" => Arc::new(WeatherMock::new(cfg.weather_fact())),
            _ => Arc::new(OpenAiCompat::new(
                effective_url(
                    "LLM",
                    cfg.talk_llm_url.as_deref().unwrap_or(DEFAULT_LLM_URL),
                ),
                Arc::new(sidecar::Curl),
                cfg.talk_timeout_secs,
            )),
        };
        let tts: Arc<dyn TtsEngine> = match cfg.talk_tts_engine.as_str() {
            "say" => Arc::new(SayTts::new(cfg.talk_timeout_secs)),
            _ => Arc::new(
                HttpTts::new(
                    effective_url("TTS", cfg.tts_url.as_deref().unwrap_or(DEFAULT_TTS_URL)),
                    Arc::new(CurlAudio),
                    cfg.talk_timeout_secs,
                )
                .with_voice(cfg.tts_voice.clone(), cfg.tts_style.clone(), cfg.tts_tone.clone()),
            ),
        };
        Self { llm, tts }
    }
}

/// 通话边车端点的三种状态。**判据照抄 `sidecar.rs::Health`**（2026-09-26 补）。
///
/// 早先只分「活 / 没活」（`curl -f` 成功与否），于是 jason 实测撞上的情形——
/// **TTS 默认端口 8765 被另一个 app 占着、对 `/health` 回 HTTP 401**——被当成
/// 「边车没起」→ 去拉起 → `serve-tts.sh` 发现端口被占立刻退出 →
/// 日志只剩「启动后立刻退出」「90s 内没就绪」，**真正的原因（端口被别人占了）
/// 一个字都没有**，而对话模式那一轮就这么没有声音。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointHealth {
    /// `/health` 回了 2xx。
    Up,
    /// 连接被拒——那个地址上**确实没人在听**。只有这一档才允许拉起。
    Down,
    /// 有东西在听，但不是能用的边车：HTTP 4xx/5xx、超时、空回复、协议错……
    ///
    /// ⚠️ 这一档**故意宽**：它的后果是「不拉起」，而误判成 `Down` 的后果是
    /// 往一个已被占用的端口再起一个服务，真正的问题被「启动失败」盖住。
    WrongService,
}

/// 纯函数：curl 退出码 + HTTP 状态码 → 三种状态。
///
/// | curl 退出码 | HTTP 状态 | 判成 |
/// |---|---|---|
/// | 0 | 2xx | `Up` |
/// | 0 | 其余（401、404、500……） | `WrongService` |
/// | 7（连接被拒） | — | `Down` |
/// | 其余（28 超时、52 空回复、56 收包失败……） / 跑不起 curl | — | `WrongService` |
pub fn classify_probe(curl_exit: Option<i32>, http_status: Option<u16>) -> EndpointHealth {
    match curl_exit {
        Some(0) => match http_status {
            Some(s) if (200..300).contains(&s) => EndpointHealth::Up,
            _ => EndpointHealth::WrongService,
        },
        Some(7) => EndpointHealth::Down,
        _ => EndpointHealth::WrongService,
    }
}

/// 探活一次，返回状态 + 一句给人看的细节（HTTP 状态码 / curl 的报错）。
///
/// **不用 `curl -f`**：`-f` 会把 4xx/5xx 变成「失败」，与「连不上」混为一谈——
/// 这正是上面那条 bug 的来路。
pub fn probe_health(url: &str) -> (EndpointHealth, String) {
    let health = format!("{}/health", url.trim_end_matches('/'));
    let out = Command::new("/usr/bin/curl")
        .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "3"])
        .arg(&health)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Err(e) => (EndpointHealth::WrongService, format!("跑 curl 失败（{health}）：{e}")),
        Ok(o) => {
            let status = String::from_utf8_lossy(&o.stdout).trim().parse::<u16>().ok();
            let h = classify_probe(o.status.code(), status);
            let detail = match h {
                EndpointHealth::Up => format!("HTTP {}", status.unwrap_or(0)),
                EndpointHealth::Down => "连接被拒（没有程序在听这个端口）".to_string(),
                EndpointHealth::WrongService => match status {
                    Some(s) if s != 0 => format!("有程序在听，但 /health 回了 HTTP {s}"),
                    _ => format!(
                        "有程序在听，但没有正常应答（{}）",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                },
            };
            (h, detail)
        }
    }
}

/// 只回答「能不能用」的旧接口，**`Ok` 只在 `Up` 时给**。`--diagnose` 以外的
/// 调用方大多只关心这一句；要区分「没起」和「端口被占」请用 [`probe_health`]。
pub fn probe_endpoint(url: &str) -> Result<()> {
    match probe_health(url) {
        (EndpointHealth::Up, _) => Ok(()),
        (_, detail) => bail!("{detail}"),
    }
}

/// 从 `http://127.0.0.1:8765` 这样的地址里取端口。没写端口时按协议给默认值。
pub fn port_of(url: &str) -> Option<u16> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or("");
    // 去掉 user:pass@
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // IPv6 字面量 [::1]:8765
    let port_part = if let Some(end) = host_port.find(']') {
        host_port[end + 1..].strip_prefix(':')
    } else {
        host_port.rsplit_once(':').map(|(_, p)| p)
    };
    match port_part {
        Some(p) => p.parse().ok(),
        None => match scheme {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        },
    }
}

/// 谁在听这个端口：`lsof` 拿进程名与 pid。拿不到就 `None`（日志照样打，只是少这一段）。
pub fn port_occupant(port: u16) -> Option<String> {
    let out = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_lsof_occupant(&String::from_utf8_lossy(&out.stdout))
}

/// `lsof -Fpc` 的输出是一行一个字段：`p<pid>` / `c<命令名>`。取第一个进程。
fn parse_lsof_occupant(out: &str) -> Option<String> {
    let mut pid = None;
    let mut cmd = None;
    for line in out.lines() {
        if let Some(v) = line.strip_prefix('p') {
            if pid.is_some() {
                break; // 第二个进程开始了
            }
            pid = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix('c') {
            cmd.get_or_insert_with(|| v.to_string());
        }
    }
    match (cmd, pid) {
        (Some(c), Some(p)) => Some(format!("{c}（pid {p}）")),
        (Some(c), None) => Some(c),
        (None, Some(p)) => Some(format!("pid {p}")),
        (None, None) => None,
    }
}

/// 端口被占时，**告诉用户该改哪里**——只说「被占了」等于没说。
pub fn port_taken_hint(name: &str) -> &'static str {
    match name {
        "LLM" => "改 config.json 的 talk_llm_url，并在 talk_llm_start_command 里加 \
                  AGENTEAR_TALK_LLM_PORT=<新端口>；或者关掉占着端口的那个程序",
        _ => "改 config.json 的 tts_url，并在 talk_tts_start_command 里加 \
              AGENTEAR_TTS_PORT=<新端口>；或者关掉占着端口的那个程序",
    }
}

/// 一句完整的「端口被占」说明（给日志和 `--diagnose` 共用，两处措辞不许分叉）。
///
/// `relocate` = 我们会不会自动换一个空闲端口去拉起（有拉起命令且没关自动拉起时会，
/// 见 [`SidecarAction::Relocate`]）。两种情况给用户的下一步完全不同，措辞必须分开。
pub fn describe_port_taken(name: &str, url: &str, detail: &str, relocate: bool) -> String {
    let who = port_of(url)
        .and_then(port_occupant)
        .map(|w| format!("，占着它的是 {w}"))
        .unwrap_or_default();
    if relocate {
        format!(
            "{name} 边车的地址 {url} 被别的程序占了（{detail}{who}）——\
             对话模式会自动换一个空闲端口把它拉起来（不改你的 config.json）"
        )
    } else {
        format!(
            "{name} 边车的地址 {url} 被别的程序占了（{detail}{who}）——不会去拉起。{}",
            port_taken_hint(name)
        )
    }
}

// ---------------------------------------------------------------- 端口避让（v0.22.0）
//
// jason 2026-09-26 拍板：「别人占了这个端口之后，我们可以自动地发现服务无效，
// 再拉起、换一个端口」。落地规则：
//
// - **只对「我们会拉起的」边车生效**（有拉起命令、没关 talk_autostart）。
//   没有拉起命令时保持「只报不拉」——我们不知道怎么在别的端口上起一个用户自己的服务。
// - **不改用户的 config.json**：换出来的端口是**运行期覆盖**（[`effective_url`]），
//   外加一份运行态记录（`<数据目录>/run/talk-sidecars.json`），见 [`RunEntry`]。
// - 端口通过环境变量交给拉起命令（`AGENTEAR_TTS_PORT` / `AGENTEAR_TALK_LLM_PORT`，
//   两个脚本都认）。命令里原来写死的同名变量会被**替换掉**，否则内层的赋值会赢
//   （`env A=1 env A=2 …` 最终是 2）——jason 本机的拉起命令里就写着 `AGENTEAR_TTS_PORT=8766`。

/// 这个边车的拉起脚本从哪个环境变量读端口。
pub fn port_env_var(name: &str) -> &'static str {
    match name {
        "LLM" => "AGENTEAR_TALK_LLM_PORT",
        _ => "AGENTEAR_TTS_PORT",
    }
}

/// 纯函数：把 URL 里的端口换成 `port`。只认 `http://host[:port][/path]` 这种形状
/// （IPv6 字面量也认）；认不出就 `None`——宁可不避让，也不拼出一个错地址去等 90 秒。
pub fn replace_port(url: &str, port: u16) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = if authority.starts_with('[') {
        let end = authority.find(']')?;
        &authority[..=end]
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}:{port}{path}"))
}

/// 纯函数：给拉起命令注入「用这个端口」。
///
/// - 命令本身以 `env` 开头：**删掉它参数段里的同名赋值**，把新值放在
///   **紧挨着要跑的程序之前**（`env` 的选项如 `-i` / `-u NAME` / `-P` 原样保留）；
/// - 否则在最前面套一层 `/usr/bin/env VAR=port`。
///
/// 程序名之后的参数一律不动——那是脚本自己的参数，长得像赋值也不是环境变量。
///
/// **`env -S`（整串拆分）返回 `None`**：那一串里可能也写着同名赋值，
/// 我们没法可靠地改写它；调用方据此放弃换端口并记日志，而不是拼出一条
/// 「看上去换了、其实还在原端口」的命令。
pub fn with_port_env(command: &[String], var: &str, port: u16) -> Option<Vec<String>> {
    let assign = format!("{var}={port}");
    let prefix = format!("{var}=");
    let is_env = command
        .first()
        .map(|c| c == "env" || c.ends_with("/env"))
        .unwrap_or(false);
    if !is_env {
        let mut out = vec!["/usr/bin/env".to_string(), assign];
        out.extend(command.iter().cloned());
        return Some(out);
    }
    let mut out = vec![command[0].clone()];
    let mut i = 1;
    let mut saw_double_dash = false;
    while i < command.len() {
        let arg = &command[i];
        if arg.starts_with("--split-string") || (arg.starts_with("-S") && !arg.starts_with("--")) {
            return None;
        }
        if arg == "--" {
            saw_double_dash = true;
            i += 1;
            break; // 之后就是程序名
        }
        if matches!(arg.as_str(), "-u" | "-P" | "-C") {
            // 带一个值的选项：连值一起原样保留
            out.push(arg.clone());
            if let Some(v) = command.get(i + 1) {
                out.push(v.clone());
            }
            i += 2;
        } else if arg.starts_with('-') {
            out.push(arg.clone());
            i += 1;
        } else if arg.contains('=') {
            if !arg.starts_with(&prefix) {
                out.push(arg.clone()); // 旧的同名赋值丢掉，否则它会盖住我们的
            }
            i += 1;
        } else {
            break; // 到程序名了
        }
    }
    // 赋值必须在 `--` 之前：`--` 之后的第一个词会被当成程序名。
    out.push(assign);
    if saw_double_dash {
        out.push("--".to_string());
    }
    out.extend(command[i..].iter().cloned());
    Some(out)
}

/// 找一个本机空闲端口：让系统分一个（bind `127.0.0.1:0`）。
///
/// ⚠️ 有 TOCTOU：我们放开它到边车 bind 之间可能被别人抢走。
/// 所以避让拉起失败（进程立刻退出）时会**再换一次**，见 `relocate_and_start`。
pub fn free_local_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()?
        .local_addr()
        .ok()
        .map(|a| a.port())
}

/// 运行期覆盖：`(边车名, 配置里的地址, 实际在用的地址)`。
///
/// 带上「配置里的地址」是为了**配置一改就失效**：用户后来把 `tts_url` 改掉了，
/// 旧的覆盖不该继续把请求导到别处。
static OVERRIDES: std::sync::Mutex<Vec<Override>> = std::sync::Mutex::new(Vec::new());

/// 一条运行期覆盖。**带上进程的 pid**：作废这条覆盖时要知道收掉的是哪个进程
/// （2026-09-26 评审：「按 pid 统一登记与摘除」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub name: &'static str,
    /// 配置里的地址（被占的那个）。配置改了，这条就不再生效。
    pub configured: String,
    /// 实际在用的地址。
    pub effective: String,
    pub pid: i32,
}

/// 纯函数：在覆盖表里查实际地址与 pid。
pub fn resolve_override(table: &[Override], name: &str, configured: &str) -> Option<(String, i32)> {
    table
        .iter()
        .find(|o| o.name == name && o.configured == configured)
        .map(|o| (o.effective.clone(), o.pid))
}

/// 本进程内存里的避让覆盖（只有守护进程自己换过端口才有）。
fn override_entry(name: &str, configured: &str) -> Option<(String, i32)> {
    let t = OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
    resolve_override(&t, name, configured)
}

/// 这个边车现在实际该连哪里：
/// ① 本进程换过端口 → 用它；② 否则运行态记录里有**正在应答且 pid 对得上**的避让地址
/// （守护进程换的，`--ask` / `--say` / `--talk-turn` 这些 CLI 入口靠这条跟上）→ 用它；
/// ③ 否则用配置的地址。
///
/// ②要探一次活（本机 curl，毫秒级），只在有记录时才探。
pub fn effective_url(name: &str, configured: &str) -> String {
    override_entry(name, configured)
        .map(|(u, _)| u)
        .or_else(|| relocated_live(name, configured))
        .unwrap_or_else(|| configured.to_string())
}

fn set_override(name: &'static str, configured: &str, effective: &str, pid: i32) {
    let mut t = OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
    t.retain(|o| o.name != name);
    t.push(Override {
        name,
        configured: configured.to_string(),
        effective: effective.to_string(),
        pid,
    });
}

fn clear_override(name: &str) {
    OVERRIDES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|o| o.name != name);
}

/// 运行态记录：我们**在避让端口上**拉起的边车。
///
/// **为什么要落盘**：v0.20.1 起守护进程崩溃后会被 launchd 自动拉起，
/// 而崩溃不走收尾，避让端口上的边车（常驻 2–4 GB）会活下来。没有这份记录，
/// 重启后的我们只会再探一次被占的默认端口、再换一个端口、**再起一份**——
/// 每崩一次多 4 GB。有了它，重启后先把上次那个**接回来**（确认 pid 对得上才接）。
///
/// **不写进 config.json**：那是用户的配置，端口避让是运行期的事，
/// 占端口的那个程序一关，下次就该回到默认端口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEntry {
    pub name: String,
    /// 配置里的地址（被占的那个）。配置改了，这条就作废。
    pub configured: String,
    /// 实际拉起的地址。
    pub url: String,
    pub pid: i32,
}

fn run_file() -> Option<PathBuf> {
    // 测试绝不能碰真实数据目录（会删掉正在用的运行态记录）：用进程私有的临时目录。
    #[cfg(test)]
    {
        return Some(
            std::env::temp_dir()
                .join(format!("agentear-test-run-{}", std::process::id()))
                .join("talk-sidecars.json"),
        );
    }
    #[allow(unreachable_code)]
    crate::data_root().ok().map(|d| d.join("run").join("talk-sidecars.json"))
}

fn load_run_entries() -> Vec<RunEntry> {
    run_file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_run_entries(entries: &[RunEntry]) {
    let Some(path) = run_file() else { return };
    if entries.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let write = || -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(entries)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        log::warn!("写运行态记录 {} 失败：{e}（崩溃重启后可能多起一份边车）", path.display());
    }
}

fn upsert_run_entry(entry: RunEntry) {
    let mut all = load_run_entries();
    all.retain(|e| e.name != entry.name);
    all.push(entry);
    save_run_entries(&all);
}

fn remove_run_entry(name: &str) {
    let mut all = load_run_entries();
    let before = all.len();
    all.retain(|e| e.name != name);
    if all.len() != before {
        save_run_entries(&all);
    }
}

/// 谁在听这个端口：只要 pid。
pub fn port_listener_pid(port: u16) -> Option<i32> {
    let out = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_lsof_pid(&String::from_utf8_lossy(&out.stdout))
}

fn parse_lsof_pid(out: &str) -> Option<i32> {
    out.lines().find_map(|l| l.strip_prefix('p')).and_then(|p| p.parse().ok())
}

/// 纯函数：一条运行态记录能不能接回来。
///
/// 三条都要满足：① 配置没改过；② 那个地址 `/health` 是 `Up`；
/// ③ 在听那个端口的**正是记录里的 pid**——pid 会被系统复用，
/// 只凭「端口上有个健康的服务」就接，可能接走别人的进程、退出时还把它杀了。
#[cfg(test)]
pub fn can_adopt(entry: &RunEntry, configured: &str, health: EndpointHealth, listener: Option<i32>) -> bool {
    adopt_decision(entry, configured, health, listener) == AdoptDecision::Adopt
}

/// 一条运行态记录该怎么处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptDecision {
    /// 三条都满足：接回来用。
    Adopt,
    /// 还是我们那个进程（听端口的正是记录里的 pid），但不能再用了
    /// （配置改了，或者它不应答了）：**先 SIGTERM 它、再删记录**——
    /// 只删记录的话，那个 2–4 GB 的进程就永远没人管了（评审 Low 项）。
    KillThenForget,
    /// 已经不是我们的进程了（死了，或 pid 对不上）：只删记录，**绝不发信号**。
    Forget,
}

/// 纯函数：记录 + 配置地址 + 探活结果 + 端口上的监听者 → 怎么处理。
pub fn adopt_decision(
    entry: &RunEntry,
    configured: &str,
    health: EndpointHealth,
    listener: Option<i32>,
) -> AdoptDecision {
    let ours = listener == Some(entry.pid);
    if !ours {
        return AdoptDecision::Forget;
    }
    if entry.configured == configured && health == EndpointHealth::Up {
        AdoptDecision::Adopt
    } else {
        AdoptDecision::KillThenForget
    }
}

/// **调用点**：给一条记录做判断。探活与查监听者作为参数传进来，
/// 测试才能用假的替掉——评审实测过，只测纯函数 `can_adopt` 时，
/// 把调用点的 pid 查询改成恒等于记录的 pid，全量测试一个都不红。
pub fn judge_run_entry(
    entry: &RunEntry,
    configured: &str,
    probe: &dyn Fn(&str) -> EndpointHealth,
    listener_of: &dyn Fn(u16) -> Option<i32>,
) -> AdoptDecision {
    let health = probe(&entry.url);
    let listener = port_of(&entry.url).and_then(listener_of);
    adopt_decision(entry, configured, health, listener)
}

fn real_probe(url: &str) -> EndpointHealth {
    probe_health(url).0
}

/// 运行态记录里这个边车的避让地址——**只在它确实还是我们那个进程时**返回
/// （同 [`adopt_decision`] 的三条：配置没改、在应答、听端口的正是记录的 pid）。
/// 只看「在应答」不够：守护进程被信号杀掉后记录会留下，那个端口之后
/// 可能被别的健康服务占了，CLI 入口就会把请求送到别人那里。
///
/// **只读**：CLI 入口不删记录、不发信号，那是守护进程的事。
pub fn relocated_live(name: &str, configured: &str) -> Option<String> {
    let entry = load_run_entries()
        .into_iter()
        .find(|e| e.name == name && e.configured == configured)?;
    (judge_run_entry(&entry, configured, &real_probe, &port_listener_pid) == AdoptDecision::Adopt)
        .then_some(entry.url)
}

/// 我们「接回来」的边车：`(名字, pid, 端口)`。它们**不是**这个进程的子进程
/// （没有 `Child` 句柄），死了之后 pid 会被系统立刻复用——所以：
/// - **不放进信号槽位**（`SPAWNED_PIDS`）：信号处理函数不能做任何校验，
///   把一个可能已被复用的 pid 交给它等于随机误杀。代价：Ctrl+C / kill 退出时
///   接回的边车留下，但运行态记录还在，下次启动会按 pid 重新接回
///   （或确认不是它了、只删记录）——**泄漏可自愈，误杀不可逆**。
/// - 退出时发信号前**重核**「在听那个端口的仍是这个 pid」（[`adopted_kill_ok`]）。
static ADOPTED: std::sync::Mutex<Vec<(&'static str, i32, u16)>> = std::sync::Mutex::new(Vec::new());

/// 纯函数：退出时能不能给一个接回来的 pid 发信号。
pub fn adopted_kill_ok(pid: i32, listener_now: Option<i32>) -> bool {
    pid > 0 && listener_now == Some(pid)
}

/// 试着把上次避让拉起的那个接回来。接上了返回它的地址。
fn adopt_from_run_file(name: &'static str, configured: &str) -> Option<String> {
    let entry = load_run_entries().into_iter().find(|e| e.name == name)?;
    match judge_run_entry(&entry, configured, &real_probe, &port_listener_pid) {
        AdoptDecision::Adopt => {}
        AdoptDecision::KillThenForget => {
            log::warn!(
                "{name} 边车：上次换端口拉起的那个（{}，pid {}）不能再用了\
                 （配置已改成 {configured}，或它不应答）——收掉它",
                entry.url,
                entry.pid
            );
            unsafe { libc::kill(entry.pid, libc::SIGTERM) };
            remove_run_entry(name);
            return None;
        }
        AdoptDecision::Forget => {
            remove_run_entry(name);
            return None;
        }
    }
    let port = port_of(&entry.url)?;
    log::info!(
        "{name} 边车：接回上次换端口拉起的那个（{}，pid {}）——配置的地址 {configured} 仍被占着时就用它",
        entry.url,
        entry.pid
    );
    ADOPTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name, entry.pid, port));
    set_override(name, configured, &entry.url, entry.pid);
    Some(entry.url)
}

/// 作废一个边车的避让覆盖，**连同它的进程**（按 pid）：
/// 我们的子进程 → 杀掉并回收；接回来的 → 重核监听者后才发信号。
/// 然后删覆盖与运行态记录。
fn discard_relocated(name: &'static str, configured: &str) {
    if let Some((url, pid)) = override_entry(name, configured) {
        if !kill_child(pid) {
            let adopted = {
                let mut a = ADOPTED.lock().unwrap_or_else(|e| e.into_inner());
                let found = a.iter().position(|(_, p, _)| *p == pid).map(|i| a.remove(i));
                found
            };
            if let Some((_, pid, port)) = adopted {
                if adopted_kill_ok(pid, port_listener_pid(port)) {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
        }
        log::info!("{name} 边车：作废避让端口 {url}（pid {pid}）");
    }
    clear_override(name);
    remove_run_entry(name);
}

/// 一个通话边车的规格：叫什么、连哪、连不上时按什么命令拉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// 只用来写日志（「LLM 边车」「TTS 边车」）。
    pub name: &'static str,
    pub url: String,
    /// 空 = 不知道怎么拉。**这是默认值，不是缺陷**——见 `sidecar_action`。
    pub command: Vec<String>,
    /// 地址是用户在 config.json 里**显式写的**（不是默认值）。
    /// 显式地址被占时照样避让，但日志升一级——用户明确要的地址没用上，得让他看见。
    pub explicit: bool,
}

/// 连不上时该干什么。**抽成纯函数是为了能把四种组合都测掉**：
/// 最容易出的错是「autostart 开着但命令是空的」被当成「已经处理过了」，
/// 结果用户按了键才发现没声音，而日志里什么都没有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarAction {
    /// 已经在跑，什么都不用做。
    AlreadyUp,
    /// 按配置的命令拉起来。
    Start,
    /// 拉不了，理由要说出来（日志里必须能看出**该跑哪条命令**）。
    CantStartNoCommand,
    /// 拉不了：用户把自动拉起关了。
    CantStartDisabled,
    /// **不许拉**：那个端口上有别的程序（见 [`EndpointHealth::WrongService`]），
    /// 而我们又没法自己拉（没有拉起命令，或关了自动拉起）——只报不拉。
    PortTaken,
    /// 端口被别的程序占了，**换一个空闲端口拉起**（v0.22.0，jason 2026-09-26 拍板）。
    /// 往原端口上再起服务只会立刻失败，所以不是 `Start`。
    Relocate,
}

pub fn sidecar_action(health: EndpointHealth, autostart: bool, command: &[String]) -> SidecarAction {
    match health {
        EndpointHealth::Up => return SidecarAction::AlreadyUp,
        EndpointHealth::WrongService => {
            return if autostart && !command.is_empty() {
                SidecarAction::Relocate
            } else {
                SidecarAction::PortTaken
            };
        }
        EndpointHealth::Down => {}
    }
    if !autostart {
        return SidecarAction::CantStartDisabled;
    }
    if command.is_empty() {
        return SidecarAction::CantStartNoCommand;
    }
    SidecarAction::Start
}

/// 对话模式需要的边车清单。**按引擎派生，不是写死两个**：
/// 用 `mock` LLM 或 `say` TTS 时那一路根本不需要边车，探它只会误导用户。
pub fn sidecar_specs(cfg: &crate::config::Config) -> Vec<Sidecar> {
    let mut out = Vec::new();
    if cfg.talk_llm_engine != "mock" {
        out.push(Sidecar {
            name: "LLM",
            url: cfg
                .talk_llm_url
                .clone()
                .unwrap_or_else(|| DEFAULT_LLM_URL.to_string()),
            command: cfg.talk_llm_start_command.clone(),
            explicit: cfg.talk_llm_url.is_some(),
        });
    }
    if cfg.talk_tts_engine != "say" {
        out.push(Sidecar {
            name: "TTS",
            url: cfg
                .tts_url
                .clone()
                .unwrap_or_else(|| DEFAULT_TTS_URL.to_string()),
            command: cfg.talk_tts_start_command.clone(),
            explicit: cfg.tts_url.is_some(),
        });
    }
    out
}

/// 我们自己拉起来的边车。**只收自己起的**——用户手工跑的进程一律不动，
/// 退出时杀掉别人的服务是很难排查的越权（`sidecar.rs` 定的规矩）。
///
/// ⚠️ **一切按 pid 操作，不按名字**（2026-09-26 评审）：同名的旧进程死了还留在表里时，
/// 按名字 `find` 会先命中死的那个、`retain` 会把刚拉起的活进程一起丢掉。
/// 观测到死亡（`try_wait` 返回 `Some`）或主动收掉时，**同一刻**从这里和信号槽位里摘除。
static SPAWNED: std::sync::Mutex<Vec<(&'static str, std::process::Child)>> =
    std::sync::Mutex::new(Vec::new());

/// 同一批 pid 的**无锁副本**，专供信号处理函数用。
///
/// 信号处理函数必须 async-signal-safe：不能锁 `Mutex`、不能分配内存。
/// 所以这里存原子值，handler 里只做 `kill(2)`（`sidecar.rs` 定的同一条规矩）。
///
/// **只放我们自己的子进程**：子进程在被 `wait` 回收之前，pid 不会被系统复用
/// （僵尸进程占着它），所以「登记 → 回收时摘除」这个顺序保证了信号不会打到别人。
/// 接回来的进程**不放进来**，理由见 [`ADOPTED`]。
///
/// 4 个槽：同时活着的最多 2 个（LLM + TTS），多 2 个给「换端口重试」的过渡。
/// **不引 Vec 是因为 handler 里不能分配**。
static SPAWNED_PIDS: [std::sync::atomic::AtomicI32; 4] = [
    std::sync::atomic::AtomicI32::new(0),
    std::sync::atomic::AtomicI32::new(0),
    std::sync::atomic::AtomicI32::new(0),
    std::sync::atomic::AtomicI32::new(0),
];

/// **给信号处理函数调用**：把我们拉起的边车都 SIGTERM 掉。
///
/// 不这么做的话，`Ctrl+C` / `kill` 退出时那两个进程（常驻约 4 GB）会活下来——
/// 菜单 Quit 那条路会收，但信号那条路不走它。
pub fn kill_spawned_pids_from_signal() {
    for slot in SPAWNED_PIDS.iter() {
        let pid = slot.load(std::sync::atomic::Ordering::SeqCst);
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn register_pid(pid: i32) {
    use std::sync::atomic::Ordering;
    for slot in SPAWNED_PIDS.iter() {
        if slot
            .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
    log::warn!("边车 pid 槽位已满，pid {pid} 在 Ctrl+C / kill 退出时不会被自动收掉（菜单退出仍会收）");
}

/// 从信号槽位里摘掉这个 pid。**只摘等于它的那一格**（CAS），不碰别的格。
fn unregister_pid(pid: i32) {
    use std::sync::atomic::Ordering;
    for slot in SPAWNED_PIDS.iter() {
        let _ = slot.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn clear_pids() {
    for slot in SPAWNED_PIDS.iter() {
        slot.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 信号槽位里现在有哪些 pid（测试用）。
#[cfg(test)]
fn registered_pids() -> Vec<i32> {
    SPAWNED_PIDS
        .iter()
        .map(|s| s.load(std::sync::atomic::Ordering::SeqCst))
        .filter(|p| *p > 0)
        .collect()
}

/// 按 pid 从登记表里拿走一个子进程（同时摘掉信号槽位）。
fn take_child(pid: i32) -> Option<std::process::Child> {
    let mut guard = SPAWNED.lock().unwrap_or_else(|e| e.into_inner());
    let i = guard.iter().position(|(_, c)| c.id() as i32 == pid)?;
    let (_, child) = guard.remove(i);
    // 先摘槽位、再回收：回收之后 pid 才可能被复用，此时它已经不在槽位里了。
    unregister_pid(pid);
    Some(child)
}

/// 杀掉并回收我们的一个子进程。它不在登记表里就返回 `false`（什么都不做）。
fn kill_child(pid: i32) -> bool {
    match take_child(pid) {
        Some(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        None => false,
    }
}

/// 这个子进程退出了没有。退出了就**当场**摘除登记并回收，返回 `true`。
fn child_exited(pid: i32) -> Option<std::process::ExitStatus> {
    let status = {
        let mut guard = SPAWNED.lock().unwrap_or_else(|e| e.into_inner());
        let (_, child) = guard.iter_mut().find(|(_, c)| c.id() as i32 == pid)?;
        match child.try_wait() {
            Ok(Some(st)) => st,
            _ => return None,
        }
    };
    let _ = take_child(pid);
    Some(status)
}

/// 扫一遍登记表：已经退出的子进程全部摘除（槽位跟着清）。
fn reap_dead_children() {
    let dead: Vec<i32> = {
        let mut guard = SPAWNED.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .iter_mut()
            .filter_map(|(_, c)| matches!(c.try_wait(), Ok(Some(_))).then_some(c.id() as i32))
            .collect()
    };
    for pid in dead {
        let _ = take_child(pid);
    }
}

/// 就绪等待上限。MLX 那两个模型要加载权重：TTS 实测 0.9–1.5s，
/// LLM 冷启动要几秒到十几秒（含 Metal 着色器编译），所以给足。
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// **连接优先、拉起兜底**（ADR-0002 §8 的老规矩，通话边车照抄）。
///
/// 返回每个边车最终是否可用。**这个函数会阻塞**（最多 `READY_TIMEOUT`），
/// 所以调用方要用 [`ensure_sidecars_async`]——菜单和启动路径都不能卡住。
pub fn ensure_sidecars(cfg: &crate::config::Config) -> Vec<(&'static str, bool)> {
    let mut out = Vec::new();
    // 先把已经死掉的子进程从登记表和信号槽位里摘掉（它们的 pid 回收后可能被复用）。
    reap_dead_children();
    for spec in sidecar_specs(cfg) {
        // ① 这个进程里已经换过端口：换出来的那个还活着就接着用，死了就作废。
        // ⚠️ 这里只看**内存**里的覆盖，不看运行态记录：记录里的那个要走下面的
        // 「接回」（校验 pid、登记收尾），直接拿来用会漏掉退出时的收尾。
        if let Some((current, _pid)) = override_entry(spec.name, &spec.url) {
            if probe_health(&current).0 == EndpointHealth::Up {
                log::info!("{} 边车已在跑（换过端口）：{current}", spec.name);
                out.push((spec.name, true));
                continue;
            }
            log::warn!("{} 边车换出来的端口 {current} 不再应答——作废（连同进程），重新判断", spec.name);
            // **按 pid 收掉并摘除所有登记**，不只是删覆盖——评审的阻塞项：
            // 只删覆盖、pid 还挂在登记表里，退出时就可能打到复用了这个 pid 的别人。
            discard_relocated(spec.name, &spec.url);
        }
        // ② 上次（崩溃前）换端口拉起的那个还活着：接回来，别再起一份。
        if adopt_from_run_file(spec.name, &spec.url).is_some() {
            out.push((spec.name, true));
            continue;
        }
        let (health, detail) = probe_health(&spec.url);
        match sidecar_action(health, cfg.talk_autostart, &spec.command) {
            SidecarAction::AlreadyUp => {
                log::info!("{} 边车已在跑：{}", spec.name, spec.url);
                out.push((spec.name, true));
            }
            SidecarAction::PortTaken => {
                log::error!("{}", describe_port_taken(spec.name, &spec.url, &detail, false));
                out.push((spec.name, false));
            }
            SidecarAction::Relocate => {
                out.push((spec.name, relocate_and_start(&spec, &detail)));
            }
            SidecarAction::CantStartDisabled => {
                log::warn!(
                    "{} 边车没起（{}），且配置里关了自动拉起（talk_autostart=false）",
                    spec.name,
                    spec.url
                );
                log::warn!("  自己起一下：{}", startup_hint(spec.name));
                out.push((spec.name, false));
            }
            SidecarAction::CantStartNoCommand => {
                // **不许静默。** 没有拉起命令不是错，但用户必须知道该跑什么，
                // 否则他按了键只会有文字没有声音（这是 v0.6/v0.7.0 的短板）。
                log::warn!("{} 边车没起（{}），也没有配置拉起命令", spec.name, spec.url);
                log::warn!("  自己起一下：{}", startup_hint(spec.name));
                out.push((spec.name, false));
            }
            SidecarAction::Start => {
                log::info!(
                    "{} 边车没起，按配置拉起：{}",
                    spec.name,
                    spec.command.join(" ")
                );
                match spawn_sidecar(spec.name, &spec.command) {
                    // 等满了还没好（TimedOut）时，子进程**留在登记表里**：它在配置的地址上，
                    // 之后真起来了下一次探活就是 `AlreadyUp`，退出时也照常被收掉。
                    // （换端口那条路不一样，见 `relocate_and_start_with`。）
                    Ok(pid) => {
                        let ready = wait_ready(spec.name, pid, &spec.url, READY_TIMEOUT) == ReadyOutcome::Ready;
                        if ready {
                            log::info!("{} 边车已就绪", spec.name);
                        } else {
                            log::error!(
                                "{} 边车拉起后在 {:?} 内没就绪，用 --diagnose 看详情",
                                spec.name,
                                READY_TIMEOUT
                            );
                        }
                        out.push((spec.name, ready));
                    }
                    Err(e) => {
                        log::error!("拉起 {} 边车失败：{e}", spec.name);
                        out.push((spec.name, false));
                    }
                }
            }
        }
    }
    out
}

/// 起一条后台线程做 [`ensure_sidecars`]——**菜单和启动路径都不能被它卡住**。
///
/// 90 秒的就绪等待放在主线程上，菜单栏会整整一分半不响应，
/// 而用户此刻正在按录音键。
///
/// **已经有一次拉起在进行时不再起第二次**（2026-09-26 补）：连着双击两次
/// 会让两条线程都探到「没起」、各拉一个，第二个撞端口立刻退出，日志一片红。
pub fn ensure_sidecars_async(cfg: &crate::config::Config) {
    if !STARTUP.try_begin() {
        log::info!("边车正在拉起中，这次不重复拉");
        return;
    }
    let cfg = cfg.clone();
    // 用 Builder 而不是 `thread::spawn`：后者起线程失败会 panic，而闸已经占上了——
    // 那样后面每一轮对话都会白等满 TURN_STARTUP_WAIT。
    let spawned = std::thread::Builder::new()
        .name("talk-sidecars".into())
        .spawn(move || {
            // 放在 guard 里收尾：ensure 中途 panic 也要把闸放开，
            // 否则后面每一轮对话都会白等满 TURN_STARTUP_WAIT。
            let _done = StartupDone;
            let results = ensure_sidecars(&cfg);
            let down: Vec<&str> = results
                .iter()
                .filter(|(_, up)| !up)
                .map(|(name, _)| *name)
                .collect();
            if down.is_empty() {
                log::info!("对话模式的边车都就绪了");
            } else {
                log::warn!(
                    "对话模式还缺 {} 边车——这一轮会只有文字没有声音（跑 --diagnose 看详情）",
                    down.join(" / ")
                );
            }
        });
    if let Err(e) = spawned {
        STARTUP.end();
        log::error!("起边车检查线程失败：{e}——这次不拉边车");
    }
}

/// 「边车正在被我们拉起」这件事的闸。**抽成结构体是为了能在测试里用局部实例**——
/// 全局那一个会被并行跑的测试互相干扰。
pub struct StartupGate {
    in_flight: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

/// 等拉起的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupWait {
    /// 没有拉起在进行，不用等。
    NotStarting,
    /// 等到它结束了（**结束 ≠ 成功**：拉起失败也算结束，下一步照常试、照常报错）。
    Finished(Duration),
    /// 等满上限还没结束。
    TimedOut,
}

impl StartupGate {
    pub const fn new() -> Self {
        Self {
            in_flight: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        }
    }

    /// 占闸。已经有一次在进行就返回 `false`。
    pub fn try_begin(&self) -> bool {
        let mut g = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if *g {
            return false;
        }
        *g = true;
        true
    }

    /// 放闸并叫醒所有在等的回答线程。
    pub fn end(&self) {
        *self.in_flight.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.cv.notify_all();
    }

    /// 如果有拉起在进行，最多等 `max`。
    pub fn wait(&self, max: Duration) -> StartupWait {
        let start = Instant::now();
        let mut g = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if !*g {
            return StartupWait::NotStarting;
        }
        while *g {
            let left = match max.checked_sub(start.elapsed()) {
                Some(d) if !d.is_zero() => d,
                _ => return StartupWait::TimedOut,
            };
            g = self
                .cv
                .wait_timeout(g, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        StartupWait::Finished(start.elapsed())
    }
}

static STARTUP: StartupGate = StartupGate::new();

struct StartupDone;
impl Drop for StartupDone {
    fn drop(&mut self) {
        STARTUP.end();
    }
}

/// 切进对话模式后第一轮最多等边车多久。
///
/// **不沿用 `READY_TIMEOUT`（90s）**：那是「边车最多给多久去加载」，
/// 而这里是「用户说完话后愿意干等多久」。实测（2026-09-26，本机）LLM 冷启动
/// 就绪 3.1s、TTS 8bit 约 10s，两个串行约 13s；30s 给了两倍多的余量。
/// 超过它还没好，这一轮的用户早就不在等了——边车在后台照样继续起，
/// **下一轮**能用，这一轮如实报错。
pub const TURN_STARTUP_WAIT: Duration = Duration::from_secs(30);

/// 每开一段录音就加一。回答线程用它判断「我等边车的这段时间里，
/// 用户是不是已经开始说下一句了」。
static RECORDING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 守护进程在「开始一段录音」时调用。
pub fn note_recording_started() {
    RECORDING_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

pub fn recording_seq() -> u64 {
    RECORDING_SEQ.load(std::sync::atomic::Ordering::SeqCst)
}

/// 等完边车之后这一轮还答不答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnGo {
    Proceed,
    /// 等的期间用户已经开始下一段录音：这一轮的回答作废。
    /// 不作废的话，迟到的回答会在用户**正在说话时**开始播放、还会被录进去。
    Superseded,
}

/// 纯函数：等待结果 + 等之前/之后的录音序号 → 答不答。
///
/// 没等（`NotStarting`）时不看序号——那是今天之前的老路径，
/// 行为一个字节都不改（LLM 本身慢导致的迟到回答不归这里管）。
pub fn turn_after_wait(waited: StartupWait, seq_before: u64, seq_now: u64) -> TurnGo {
    match waited {
        StartupWait::NotStarting => TurnGo::Proceed,
        _ if seq_now != seq_before => TurnGo::Superseded,
        _ => TurnGo::Proceed,
    }
}

/// **对话模式那一轮回答之前调用**（在回答线程里，不在 worker 线程里——
/// 这里可能阻塞最多 [`TURN_STARTUP_WAIT`]）。返回 `false` = 这一轮不答了。
///
/// 修的是 jason 2026-09-26 实测的「双击进对话模式后第一轮没回答」：
/// 双击那一刻才在后台拉边车，而这一轮说完、转写完（约 3 秒）就去请求 TTS，
/// 那时 TTS 还在加载——**第一轮必然失败**。
pub fn await_sidecars_for_turn(seq_before: u64) -> bool {
    let waited = STARTUP.wait(TURN_STARTUP_WAIT);
    match waited {
        StartupWait::NotStarting => {}
        StartupWait::Finished(d) => {
            log::info!("这一轮先等边车拉起：等了 {:.1}s", d.as_secs_f32());
        }
        StartupWait::TimedOut => {
            log::warn!(
                "等边车拉起等满 {:?} 还没结束——这一轮照样去试（多半会失败，见下一条日志），\
                 边车在后台继续起，下一轮应当能用",
                TURN_STARTUP_WAIT
            );
        }
    }
    match turn_after_wait(waited, seq_before, recording_seq()) {
        TurnGo::Proceed => true,
        TurnGo::Superseded => {
            log::info!("等边车的这段时间里已经开始了下一段录音——这一轮的回答作废，不去抢麦");
            false
        }
    }
}

/// 端口被占时：换一个空闲端口拉起。最多试两次（第二次是给 TOCTOU 的：
/// 选好的端口在边车 bind 之前被别人抢了，边车会发现端口被占、立刻退出）。
fn relocate_and_start(spec: &Sidecar, detail: &str) -> bool {
    relocate_and_start_with(spec, detail, READY_TIMEOUT)
}

/// [`relocate_and_start`] 的可测版本（就绪上限可调）。
///
/// **每条失败路径都要把拉起的子进程收干净**（评审阻塞项 ③）：换出来的端口是随机的，
/// 没有覆盖记录、没有运行态记录的话，谁都找不到它——下次会再换一个端口起一份，
/// 崩溃后它就成了永远没人管的 2–4 GB 孤儿。
fn relocate_and_start_with(spec: &Sidecar, detail: &str, timeout: Duration) -> bool {
    let old_port = port_of(&spec.url).map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    let who = port_of(&spec.url)
        .and_then(port_occupant)
        .unwrap_or_else(|| "别的程序".to_string());
    for attempt in 1..=2 {
        let Some(port) = free_local_port() else {
            log::error!("{} 边车：找不到空闲端口，没法避让", spec.name);
            return false;
        };
        let Some(new_url) = replace_port(&spec.url, port) else {
            log::error!(
                "{} 边车：地址 {} 认不出端口段，没法换端口（{detail}）——{}",
                spec.name,
                spec.url,
                port_taken_hint(spec.name)
            );
            return false;
        };
        let Some(command) = with_port_env(&spec.command, port_env_var(spec.name), port) else {
            log::error!(
                "{} 边车：拉起命令用了 `env -S`（整串拆分），没法可靠地替换端口变量，不换端口——{}",
                spec.name,
                port_taken_hint(spec.name)
            );
            return false;
        };
        let msg = format!(
            "{} 边车：端口 {old_port} 被 {who} 占了（{detail}）→ 改用端口 {port} 拉起{}",
            spec.name,
            if attempt > 1 { "（第二次）" } else { "" }
        );
        if spec.explicit {
            // 用户在 config.json 里明确写的地址没用上：升一级，让他看得见。
            log::error!("{msg}——⚠️ 这是你在 config.json 里写的地址，本次运行不会用它");
        } else {
            log::warn!("{msg}");
        }
        let pid = match spawn_sidecar(spec.name, &command) {
            Ok(pid) => pid,
            Err(e) => {
                log::error!("拉起 {} 边车失败：{e}", spec.name);
                return false;
            }
        };
        match wait_ready(spec.name, pid, &new_url, timeout) {
            ReadyOutcome::Ready => {
                log::info!("{} 边车已在新端口就绪：{new_url}", spec.name);
                set_override(spec.name, &spec.url, &new_url, pid);
                upsert_run_entry(RunEntry {
                    name: spec.name.to_string(),
                    configured: spec.url.clone(),
                    url: new_url,
                    pid,
                });
                return true;
            }
            // 进程已退出：`wait_ready` 当场摘除了它的登记，不用再收。
            ReadyOutcome::Exited if attempt == 1 => {
                log::warn!("{} 边车在端口 {port} 上起来就退出了（多半是端口刚被抢走），再换一个试", spec.name);
            }
            ReadyOutcome::Exited => {
                log::error!("{} 边车换到端口 {port} 也没起来，用 --diagnose 看详情", spec.name);
                return false;
            }
            ReadyOutcome::TimedOut => {
                // 还活着但一直没就绪：**杀掉并回收**，不留一个没人知道端口的进程。
                kill_child(pid);
                log::error!(
                    "{} 边车在端口 {port} 上 {:?} 内没就绪——已收掉（不留孤儿），用 --diagnose 看详情",
                    spec.name,
                    timeout
                );
                return false;
            }
        }
    }
    false
}

// ---------------------------------------------------------------- 边车不可用时念一句（v0.22.0）
//
// jason 2026-09-26 拍板「可以 say」：对话模式下这一轮没说出来、而原因是边车不可用时，
// 用 macOS 自带的 `say`（零依赖，不需要任何边车）念一句提示——否则用户只会觉得
// 「按了没反应」，而那正是 2026-09-26 那次的体验。
//
// **节流规则**：同一个原因（哪几个边车不可用）**3 分钟内只念一次**；
// 原因变了（比如从「TTS 没起」变成「LLM 和 TTS 都没起」）立刻再念；
// **有一轮正常说出来了就清零**，下一次出事第一时间就提示。
// 只在对话模式里念（调用点只在对话模式那条路上）：输入法模式本来就不出声。

/// 同一原因两次提示之间至少隔多久。
pub const UNAVAILABLE_NOTICE_WINDOW: Duration = Duration::from_secs(180);

/// 纯函数：这次要不要念。
pub fn should_notice(last: Option<(&str, Instant)>, key: &str, now: Instant, window: Duration) -> bool {
    match last {
        None => true,
        Some((k, _)) if k != key => true,
        Some((_, at)) => now.saturating_duration_since(at) >= window,
    }
}

/// 念给用户的那一句（按通话语言）。
pub fn unavailable_notice(lang: TalkLang) -> &'static str {
    match lang {
        TalkLang::Zh => "语音服务没有启动，这一轮没法回答，请看日志。",
        TalkLang::En => "The voice service isn't running, so I can't answer this time. Please check the log.",
        TalkLang::Th => "บริการเสียงยังไม่ทำงาน ตอบรอบนี้ไม่ได้ กรุณาดูบันทึก",
    }
}

static LAST_NOTICE: std::sync::Mutex<Option<(String, Instant)>> = std::sync::Mutex::new(None);

/// 这一轮正常说出来了：清掉节流，下一次出事立刻提示。
pub fn note_turn_ok() {
    *LAST_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 现在哪几个边车不可用（按**实际在用的**地址探，换过端口的算换过之后的）。
pub fn sidecars_down(cfg: &crate::config::Config) -> Vec<&'static str> {
    sidecar_specs(cfg)
        .into_iter()
        .filter(|s| probe_health(&effective_url(s.name, &s.url)).0 != EndpointHealth::Up)
        .map(|s| s.name)
        .collect()
}

/// 对话模式这一轮没说出来之后调用：**如果原因是边车不可用**，用 `say` 念一句。
/// 边车都在（失败是别的原因，比如模型回了空）就不念——那不是「服务没起」。
pub fn notice_if_sidecars_down(cfg: &crate::config::Config, lang: TalkLang) {
    let down = sidecars_down(cfg);
    if down.is_empty() {
        return;
    }
    let key = down.join("+");
    {
        let mut last = LAST_NOTICE.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let prev = last.as_ref().map(|(k, at)| (k.as_str(), *at));
        if !should_notice(prev, &key, now, UNAVAILABLE_NOTICE_WINDOW) {
            log::info!("{key} 边车不可用——{UNAVAILABLE_NOTICE_WINDOW:?} 内已经提示过，这次不念");
            return;
        }
        *last = Some((key.clone(), now));
    }
    log::warn!("{key} 边车不可用：用 say 念一句提示");
    let spoken = SayTts::new(15)
        .synthesize(unavailable_notice(lang), lang)
        .and_then(|wav| play_blocking(&wav));
    if let Err(e) = spoken {
        log::error!("连 say 的提示都念不出来：{e:#}");
    }
}

fn spawn_sidecar(name: &'static str, command: &[String]) -> Result<i32> {
    let (prog, args) = command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("拉起命令为空"))?;
    let child = Command::new(prog)
        .args(args)
        // 输出丢弃：边车自己写日志；接进没人读的管道，写满会把它卡死
        // （download.rs 踩过同类坑，sidecar.rs 同一条理由）
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动 {prog} 失败"))?;
    let pid = child.id() as i32;
    register_pid(pid);
    SPAWNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name, child));
    Ok(pid)
}

/// 等边车就绪的结果。**把「进程退出了」和「等满了」分开**：前者可能是端口刚被抢
/// （值得换个端口再试一次），后者是真的起不来（再试也是白等 90 秒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadyOutcome {
    Ready,
    Exited,
    TimedOut,
}

/// 等某个边车就绪。**按 pid 盯住我们自己起的那个进程**（不按名字——
/// 同名的旧记录会先被命中，见 [`SPAWNED`]）：它要是启动后就退出
/// （命令写错、venv 缺包、端口被抢），不该再傻等满 90 秒。
fn wait_ready(name: &'static str, pid: i32, url: &str, timeout: Duration) -> ReadyOutcome {
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(500).min(timeout));
        if probe_endpoint(url).is_ok() {
            log::info!("{name} 边车等了 {:.1}s", start.elapsed().as_secs_f32());
            return ReadyOutcome::Ready;
        }
        if let Some(code) = child_exited(pid) {
            log::error!("{name} 边车（pid {pid}）启动后立刻退出了（{code:?}），检查拉起命令能不能单独跑通");
            return ReadyOutcome::Exited;
        }
    }
    ReadyOutcome::TimedOut
}

/// 该跑哪条命令——**日志里必须给出这条**，否则「没声音」对用户就是无解的。
fn startup_hint(name: &str) -> &'static str {
    match name {
        "LLM" => "scripts/serve-talk-llm.sh（首次先跑 scripts/setup-talk.sh）",
        _ => "scripts/serve-tts.sh（首次先跑 scripts/setup-talk.sh）",
    }
}

/// 退出时收掉**我们自己拉起的**边车。幂等。
///
/// 为什么不留在后台：它们常驻 2.5 GB + 1.6 GB。AgentEar 退出了还占着 4 GB，
/// 用户只能去活动监视器里找——这是不能留的。
pub fn shutdown_spawned() {
    let kids: Vec<(&'static str, std::process::Child)> = {
        let mut guard = SPAWNED.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    for (name, mut child) in kids {
        log::info!("退出：关掉我们拉起的 {name} 边车");
        let pid = child.id() as i32;
        let _ = child.kill();
        let _ = child.wait();
        unregister_pid(pid);
    }
    let adopted: Vec<(&'static str, i32, u16)> =
        std::mem::take(&mut *ADOPTED.lock().unwrap_or_else(|e| e.into_inner()));
    for (name, pid, port) in adopted {
        // 接回来的不是我们的子进程：它可能早就死了、pid 已经被别人复用。
        // **发信号前重核**「在听那个端口的仍是这个 pid」。
        if adopted_kill_ok(pid, port_listener_pid(port)) {
            log::info!("退出：关掉接回来的 {name} 边车（pid {pid}）");
            unsafe { libc::kill(pid, libc::SIGTERM) };
        } else {
            log::info!("退出：接回来的 {name} 边车（pid {pid}）已经不在端口 {port} 上了，不发信号");
        }
    }
    // 我们拉起的都收了：运行态记录跟着作废，下次启动从配置的地址重新判断。
    let had_overrides = !OVERRIDES.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    if had_overrides {
        save_run_entries(&[]);
    }
    OVERRIDES.lock().unwrap_or_else(|e| e.into_inner()).clear();
    clear_pids();
}

/// 语系/口音选项：`(键, 中文名, 英文名, 泰文名)`。
///
/// ⚠️ **这份清单必须与 `services/tts/backends.py` 的 `STYLE_INSTRUCTS` 一致**——
/// 两边各有一份（菜单不能为了一次列表去发 HTTP，而边车也不能读 Rust 的常量）。
/// 漂移了会怎样：菜单点得下去、边车回 400。所以两边各有一条测试钉住**完整键集**，
/// 改一处就必须改另一处。
pub const STYLE_OPTIONS: &[(&str, &str, &str, &str)] = &[
    ("zh", "普通话", "Mandarin", "จีนกลาง"),
    ("yue", "粤语（广东话）", "Cantonese", "กวางตุ้ง"),
    ("henan", "河南话", "Henan", "เหอหนาน"),
    ("sichuan", "四川话", "Sichuan", "เสฉวน"),
    ("shandong", "山东话", "Shandong", "ซานตง"),
    ("dongbei", "东北话", "Northeastern", "ตงเป่ย"),
    ("tianjin", "天津话", "Tianjin", "เทียนจิน"),
    ("en", "英语", "English", "อังกฤษ"),
    ("en-gb", "英式英语", "British English", "อังกฤษบริเตน"),
    ("en-us", "美式英语", "American English", "อังกฤษอเมริกัน"),
    ("en-ca", "加拿大英语", "Canadian English", "อังกฤษแคนาดา"),
    ("th", "泰语", "Thai", "ไทย"),
];

/// **语言**档：助手用哪种语言回答。菜单「说话 → 语言」用这一组。
///
/// ⚠️ 与 `DIALECT_STYLES` 的分组**只是菜单呈现**：两者都是 `STYLE_OPTIONS` 的键，
/// 走同一个 `tts_style` 配置、同一条 HTTP 参数。分成两个菜单是 jason 2026-09-15 的要求
/// （「切换男女声音一个菜单，切换方言单独一个菜单」）。
///
/// 默认是 `zh`（普通话）——**这是他明确拍的默认**。
pub const LANGUAGE_STYLES: &[&str] = &["zh", "en", "th"];

/// **方言 / 口音**档。菜单「说话 → 方言」用这一组。
///
/// ⚠️ **方言靠正文、不靠这一项**：官方 usage guide 的 Dialect tips 写着
/// 「write the target text in that dialect's own vocabulary and expressions」——
/// 同一个 `(四川话)` 下，正文是地道四川话才出四川腔，正文是普通话就还是普通话。
/// 这一栏只是**开关**。CLI/文档里不要写成「选了四川话就会说四川话」。
pub const DIALECT_STYLES: &[&str] = &[
    "yue", "henan", "sichuan", "shandong", "dongbei", "tianjin", "en-gb", "en-us", "en-ca",
];

/// 按 key 找 `STYLE_OPTIONS` 里的下标（菜单 tag 要用它）。
pub fn style_index(key: &str) -> Option<usize> {
    STYLE_OPTIONS.iter().position(|o| o.0 == key)
}

/// 语气选项，键与 `TONE_INSTRUCTS` 一致。
pub const TONE_OPTIONS: &[(&str, &str, &str, &str)] = &[
    ("warm", "亲切自然（默认）", "Warm & natural", "อบอุ่นเป็นธรรมชาติ"),
    ("calm", "平静温和", "Calm", "สงบ"),
    ("lively", "活泼明快", "Lively", "มีชีวิตชีวา"),
    ("serious", "沉稳专业", "Serious", "จริงจัง"),
];

/// 按界面语言取选项名。
pub fn option_label(option: &(&'static str, &'static str, &'static str, &'static str), lang: crate::i18n::Lang) -> &'static str {
    match lang {
        crate::i18n::Lang::Zh => option.1,
        crate::i18n::Lang::En => option.2,
        crate::i18n::Lang::Th => option.3,
    }
}

pub const DEFAULT_LLM_URL: &str = "http://127.0.0.1:8794";
/// ⚠️ **不是 8765 了**（v0.22.0）：8765 是 Python `websockets` 等一大批开发工具
/// 示例里的默认端口，2026-09-26 在 jason 机器上被一个桌面 app 占着回 401，
/// 对话模式因此整轮没声音。8796 与本仓库另外两个边车（8793 理解层 / 8794 通话 LLM）
/// 挨着，好认；8795 留给 serve-talk-llm.sh 提示里的「手动换一个」。
/// **与 `scripts/serve-tts.sh` 的默认端口必须一致**（tests/script_defaults.rs 钉住）。
pub const DEFAULT_TTS_URL: &str = "http://127.0.0.1:8796";

// ------------------------------------------------- 句子切分（流式合成用）

/// 一个句子至少要这么长才单独送去合成。
///
/// 太短的碎片（「3.」「好的。」）单独合成有两个坏处：合成一次的**固定开销**
/// （一次 HTTP + 一次 MLX 调用，实测 2.6s 起）会比它念出来的时间还长，
/// 而且零样本音色在极短文本上更不稳。宁可多攒一点。
pub const MIN_SENTENCE_CHARS: usize = 6;

/// 句末标点。中英泰都算上——泰语虽然用空格分词，但句末也可能出现这些。
fn is_sentence_end(c: char) -> bool {
    // `.` 也在里面：英文的句子就是靠它断的。它是小数点和版本号的一部分
    // 那种情况在 `find_cut` 里单独排掉。
    matches!(c, '。' | '！' | '？' | '!' | '?' | '；' | ';' | '\n' | '…' | '.')
}

/// 把流式碎片攒成「可以送去合成的句子」。
///
/// **为什么不能按标点随手切**：句号在数字和版本号里到处都是
/// （`3.5`、`v0.9.0`），切错了会把一句话撕成两半、或者合成出一个半截词。
/// 所以除了句末标点，还要看**攒够长度没有**、以及**后面确实还有内容**。
#[derive(Default)]
pub struct SentenceSplitter {
    pending: String,
    emitted: usize,
}

impl SentenceSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已经交出去的句子数（日志与测试用）。
    pub fn emitted(&self) -> usize {
        self.emitted
    }

    /// 喂一段流式碎片，返回**现在就能送去合成的句子**（可能不止一句）。
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.pending.push_str(delta);
        let mut out = Vec::new();
        while let Some(cut) = self.find_cut() {
            let sentence: String = self.pending.drain(..cut).collect();
            let sentence = sentence.trim();
            if !sentence.is_empty() {
                out.push(sentence.to_string());
                self.emitted += 1;
            }
        }
        out
    }

    /// 流结束：把剩下的一并交出来。
    pub fn finish(&mut self) -> Option<String> {
        let rest = self.pending.trim().to_string();
        self.pending.clear();
        if rest.is_empty() {
            return None;
        }
        self.emitted += 1;
        Some(rest)
    }

    /// 下一个切点（字节下标，切在标点**之后**）。
    ///
    /// ⚠️ 两个「先不切」的情形都要留着：
    /// ① 标点是当前最后一个字符——后面可能还有内容，是小数点的前半截；
    /// ② 攒的字数不够 `MIN_SENTENCE_CHARS`——继续往后找，下一处标点会自然接上。
    fn find_cut(&self) -> Option<usize> {
        let chars: Vec<(usize, char)> = self.pending.char_indices().collect();
        for (i, (byte_idx, c)) in chars.iter().enumerate() {
            if !is_sentence_end(*c) {
                continue;
            }
            if i + 1 >= chars.len() {
                continue;
            }
            let end = byte_idx + c.len_utf8();
            let non_ws = self.pending[..end].chars().filter(|c| !c.is_whitespace()).count();
            if non_ws < MIN_SENTENCE_CHARS {
                continue;
            }
            // 小数点 / 版本号：`3.5`、`v0.9.0` —— 前后都是数字就不算句末。
            // （`。` 与 `.` 是两个不同的字符，中文那句不受这里影响。）
            if *c == '.' {
                let prev = chars[..i].iter().rev().find(|(_, c)| !c.is_whitespace());
                let next = chars[i + 1..].iter().find(|(_, c)| !c.is_whitespace());
                if prev.map(|(_, c)| c.is_ascii_digit()).unwrap_or(false)
                    && next.map(|(_, c)| c.is_ascii_digit()).unwrap_or(false)
                {
                    continue;
                }
            }
            return Some(end);
        }
        None
    }
}

/// 把一段**已经完整**的文本切成句子（目前只有测试用；
/// 留着是因为它是 `SentenceSplitter` 最直观的用法示例）。
#[cfg(test)]
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut sp = SentenceSplitter::new();
    let mut out = sp.push(text);
    if let Some(rest) = sp.finish() {
        out.push(rest);
    }
    out
}

/// 把回答和问句做**去标点、去空白**的归一化比较，判断模型是不是把问题原样退了回来。
///
/// 实测动机（2026-09-14）：泰语那一轮 MiniCPM5-2B 有一次**直接把问句复述了回来**
/// （问 `วันนี้อากาศเป็นอย่างไรบ้าง`，答也是 `วันนี้อากาศเป็นอย่างไรบ้าง`）。
/// 那不是回答，但它是**非空的、语种也对的**字符串——不挡的话 TTS 会把用户
/// 自己的问题念回给他听，而他只会觉得「这东西坏了，还说不清哪坏了」。
///
/// ⚠️ 只判「几乎一模一样」，不判语义。宽松一点没关系：
/// 误杀一个好回答的代价（用户听不到话）比放过一个复述更大，
/// 所以只在**去掉标点空白后完全相等**时才判定为复述。
pub fn is_echo(reply: &str, question: &str) -> bool {
    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && !is_punctuation(*c))
            .flat_map(|c| c.to_lowercase())
            .collect()
    }
    let (a, b) = (normalize(reply), normalize(question));
    // 太短的比较没有意义（「好」和「好」那是真的在回答）
    !a.is_empty() && a == b && b.chars().count() >= 4
}

/// 判断一个字符是不是「念出来没有意义」的标点。中英泰三语都要覆盖。
fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        '。' | '，' | '、' | '？' | '！' | '；' | '：' | '“' | '”' | '‘' | '’'
            | '（' | '）' | '《' | '》' | '…' | '—' | '·'
    ) || c.is_ascii_punctuation()
        || matches!(c, '\u{0E4F}' | '\u{0E5A}' | '\u{0E5B}' | '\u{0E46}')
}

/// 一次问答：给一段用户文字，拿到要念出来的回答。
pub fn answer(engines: &Engines, cfg: &crate::config::Config, user_text: &str, lang: TalkLang) -> Result<String> {
    let system = system_prompt(lang, &cfg.weather_fact());
    let started = Instant::now();
    let reply = engines.llm.reply(&system, user_text, lang)?;
    log::info!(
        "通话 LLM（{}）{:.2}s → {:?}",
        engines.llm.name(),
        started.elapsed().as_secs_f32(),
        reply
    );
    if reply.is_empty() {
        bail!("模型返回了空回答");
    }
    if is_echo(&reply, user_text) {
        // 明显不是回答，宁可这一轮没有声音，也不要让用户听见自己在说话
        bail!(
            "模型把问题原样退了回来（{}），这一轮不播",
            engines.llm.name()
        );
    }
    Ok(reply)
}

/// 合成 + 播放，返回播放时长。
pub fn speak(engines: &Engines, text: &str, lang: TalkLang) -> Result<Duration> {
    let started = Instant::now();
    let wav = engines.tts.synthesize(text, lang)?;
    let synth = started.elapsed();
    log::info!(
        "通话 TTS（{}）{:.2}s，{} 字节",
        engines.tts.name(),
        synth.as_secs_f32(),
        wav.len()
    );
    play_blocking(&wav)
}

/// 一轮问答 + 逐句合成 + 逐句播放：**「边出边合成」的落点**。
///
/// ## 它和 `answer` + `speak` 的区别
///
/// 老路径是严格串行的三拍：等 LLM 把整段话说完（0.7–0.9s）→ 整段送去做 TTS
/// （2.6–5.1s）→ 才开始播。用户从说完到听见第一个字要 **4–6s**。
/// 这里把后两拍**重叠**起来：LLM 一出第一个句子就送去合成，合成一段播一段，
/// 剩下的句子在播放期间继续合成。
///
/// ## 为什么必须能退回老路
///
/// 流式依赖 SSE、依赖边车认 `stream: true`、依赖模型中途就给出句末标点。
/// 这三条**任何一条不成立都不能变成「这一轮没声音」**——那比慢得多更糟。
/// 所以：传输不支持流式、或流式请求直接失败，一律退回整句路径。
///
/// 返回 `(完整回答, 实际播出去的时长)`。
pub fn answer_and_speak_streamed(
    engines: &Engines,
    cfg: &crate::config::Config,
    user_text: &str,
    lang: TalkLang,
    // 第一段音频**即将开始播**时回调一次。调用方用它把会话推进到
    // `Speaking`——流式下这一刻回答还没写完，过了这一刻再推就晚了。
    on_first_audio: &mut dyn FnMut(),
) -> Result<(String, Duration)> {
    if !engines.llm.streams() {
        // 引擎不支持流式：老老实实走老路，行为与 v0.9.0 完全一致。
        let reply = answer(engines, cfg, user_text, lang)?;
        on_first_audio();
        let played = speak(engines, &reply, lang)?;
        return Ok((reply, played));
    }

    let system = system_prompt(lang, &cfg.weather_fact());
    let question = user_text.to_string();
    // 生产者：收 LLM 的流 → 切句 → 逐句合成 → 投进通道。
    // ⚠️ **合成的顺序就是播出的顺序**，FIFO 通道就够，不带序号——
    // 多一个序号就多一处可能对不上的地方。
    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>>>();
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let llm = engines.llm.clone();
    let tts = engines.tts.clone();
    let abort_prod = abort.clone();
    let started = Instant::now();
    let interrupts_at_start = interrupts();

    let producer = std::thread::spawn(move || -> Result<String> {
        let mut splitter = SentenceSplitter::new();
        let mut first_sentence = true;
        let mut echoed = false;
        let send = |sentence: &str| -> bool {
            match tts.synthesize(sentence, lang) {
                Ok(wav) => tx.send(Ok(wav)).is_ok(),
                Err(e) => {
                    let _ = tx.send(Err(e));
                    false
                }
            }
        };
        let mut on_delta = |delta: &str| {
            if echoed {
                return; // 已经判定为复述：后面的碎片一律不再合成
            }
            for sentence in splitter.push(delta) {
                if abort_prod.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                // ⚠️ **复述检查必须在第一句播出去之前做**：老路径能拿整段回答
                // 去比，流式下第一句可能已经开口了。拿第一句去比是等价的代理——
                // 复述那种坏形态就是从第一个字开始复述。
                if first_sentence && is_echo(&sentence, &question) {
                    log::warn!("模型把问题原样退了回来（第一句），这一轮不播");
                    echoed = true;
                    return;
                }
                first_sentence = false;
                if !send(&sentence) {
                    abort_prod.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
            }
        };
        let text = llm.reply_stream(&system, &question, lang, &mut on_delta)?;
        // 尾句没有句末标点，`splitter` 里还压着——在这里补上。
        if let Some(rest) = splitter.finish() {
            if !echoed && !abort_prod.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = send(&rest);
            }
        }
        log::info!("流式切句：共 {} 句", splitter.emitted());
        if echoed {
            bail!("模型把问题原样退了回来（{}），这一轮不播", llm.name());
        }
        Ok(text)
    });

    let mut played = Duration::ZERO;
    let mut first_audio: Option<Duration> = None;
    let mut consumer_err: Option<anyhow::Error> = None;
    // 消费者：按顺序播。**这里不主动判定打断**——`play_blocking` 会被
    // `stop_playback` 掐掉，我们只在事后看「发生过没有」，并让生产者别再合成。
    for item in rx.iter() {
        match item {
            Ok(wav) => {
                if first_audio.is_none() {
                    let t = started.elapsed();
                    first_audio = Some(t);
                    log::info!(
                        "首字延迟（流式，{} → 切句播放）：{:.2}s 起播",
                        engines.llm.name(),
                        t.as_secs_f32()
                    );
                    on_first_audio();
                }
                played += play_blocking(&wav)?;
                if interrupts() != interrupts_at_start {
                    // 被打断了：剩下的句子不必再合成，也不必再播。
                    abort.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
            Err(e) => {
                abort.store(true, std::sync::atomic::Ordering::Relaxed);
                consumer_err = Some(e);
                break;
            }
        }
    }
    let produced = producer.join().unwrap_or_else(|_| Err(anyhow::anyhow!("合成线程崩了")));
    // ⚠️ 已经播出去的不补、不撤——用户听到半句，好过听到一句错位的话。
    // 但**错误要如实报上去**，不能因为「播了一点」就说这一轮成功。
    if let Some(e) = consumer_err {
        return Err(e).context("流式 TTS 合成失败");
    }
    let reply = produced?;
    if reply.trim().is_empty() {
        bail!("模型返回了空回答");
    }
    if let Some(t) = first_audio {
        log::info!(
            "通话播完：首字 {:.2}s、共播 {:.2}s、回答 {} 字",
            t.as_secs_f32(),
            played.as_secs_f32(),
            reply.chars().count()
        );
    }
    // ⚠️ **回答正文必须进日志**（v0.13.0 补）。
    //
    // 流式改造（v0.10.0）时这里只留了字数，把正文丢了——原来整句路径是打出来的
    // （`通话 LLM（…）{:.2}s → {:?}`）。后果是**事后查不了**：用户问
    // 「它刚才到底说了什么」，日志里只有「回答 27 字」。
    // 实测踩到（jason 2026-09-15 人工测试）：他复述模型说过
    // 「我无法访问外部链接」，而我们从日志里读不出这句话，只能重新打一遍模型才知道。
    // 这一行是**可追溯性**，不是调试噪音。
    log::info!("通话回答正文：{reply}");
    Ok((reply, played))
}

/// 测试专用的小工具。
#[cfg(test)]
pub mod tests_support {
    use std::io::Write;

    /// 把 [-1,1] 的 float 采样包成 mono 16-bit PCM WAV。
    /// 只用标准库——测试不该为了造一段音频去拉依赖。
    pub fn wav_from_samples(samples: &[f32], rate: u32) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| {
                let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                v.to_le_bytes()
            })
            .collect();
        let mut out = Vec::with_capacity(44 + data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk 长度
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes()); // 字节率
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // 位深
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.write_all(&data).unwrap();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::test_support::Fake;

    // ------------------------------------------------ 流式：切句

    /// 逐字喂进去，看它什么时候才肯交出一句。
    ///
    /// **为什么要逐字喂**：真实流式就是一次几个字的碎片，
    /// 「一次喂一整段」测不出「碎片边界上判错」这类 bug。
    #[test]
    fn splitter_waits_for_a_complete_sentence() {
        let mut sp = SentenceSplitter::new();
        // 「今天天气」只有 4 个字，还没到句末标点，一句都不该出
        assert!(sp.push("今天").is_empty());
        assert!(sp.push("天气").is_empty());
        // 句末标点到了，而且够长 → 出一句
        let out = sp.push("怎么样？今天适合出门。");
        assert_eq!(out, vec!["今天天气怎么样？"]);
        // 剩下的留在里面，等 `finish`
        assert_eq!(sp.finish().unwrap(), "今天适合出门。");
    }

    /// 句末标点是**最后一个字符**时先不切：后面可能还有内容。
    ///
    /// 这条是「按标点随手切」最容易翻车的地方——`3.` 切下去，
    /// 下一段 `5 度` 就变成独立的一句，合成出来是「三、五度」。
    #[test]
    fn splitter_does_not_cut_on_a_trailing_dot() {
        let mut sp = SentenceSplitter::new();
        assert!(sp.push("气温 3.").is_empty(), "小数点后面还没到，不许切");
        let out = sp.push("5 度，适合出门散步。");
        // ⚠️ 句末标点**正好落在当前缓冲区的最后一个字符**时也是「先不切」——
        // 因为此刻还分不清它是「一句话说完了」还是「v0.9.0 的前半截」。
        // 真实流式里下一个碎片几毫秒就到，所以这点延迟无感；
        // **判据是「没被切成两句」**，那句「3.」和「5 度」没有被拆开。
        assert!(out.is_empty(), "末尾的句号要等下一个碎片才敢切");
        assert_eq!(sp.emitted(), 0);
        assert_eq!(sp.finish().unwrap(), "气温 3.5 度，适合出门散步。");
    }

    /// 中文的 `。` 与英文的 `.` 都要能断句——三语支持不能只顾中文。
    #[test]
    fn splitter_handles_all_three_languages() {
        // 英文：靠 `.`
        let en = split_sentences("It is sunny today. The high is 32 degrees.");
        assert_eq!(en.len(), 2, "{en:?}");
        assert!(en[0].ends_with("today."));
        // 版本号里的点不能断
        let ver = split_sentences("版本是 v0.9.0 修好的。");
        assert_eq!(ver.len(), 1, "{ver:?}");
        // 泰语：没有句末标点时靠 `finish` 兜底，不能丢字
        let th = split_sentences("วันนี้อากาศดี");
        assert_eq!(th, vec!["วันนี้อากาศดี"]);
    }

    /// 太短的碎片不单独合成：合成一次的固定开销比它念出来的时间还长。
    #[test]
    fn splitter_does_not_emit_tiny_fragments() {
        let mut sp = SentenceSplitter::new();
        // 「好。」只有 2 个字，不够 MIN_SENTENCE_CHARS → 攒着
        assert!(sp.push("好。").is_empty());
        // 攒够了才交出来，而且是**一句话**——「好。」没有被单独送去合成
        let _ = sp.push("那就这么定了，明天见。");
        assert_eq!(sp.emitted(), 0, "「好。」不许自己合成一次");
        assert_eq!(sp.finish().unwrap(), "好。那就这么定了，明天见。");
    }

    /// 空碎片、纯空白不能变成空句子（空文本送合成会得到一段静音）。
    #[test]
    fn splitter_never_emits_empty_sentences() {
        let mut sp = SentenceSplitter::new();
        assert!(sp.push("   ").is_empty());
        assert!(sp.push("\n\n").is_empty());
        assert_eq!(sp.finish(), None);
        assert_eq!(sp.emitted(), 0);
    }

    // ------------------------------------------------ 流式：思考段

    /// 未闭合的思考段**一个字都不许放出去**——放出去就是听见模型的自言自语。
    #[test]
    fn unclosed_think_is_held_back_while_streaming() {
        assert_eq!(think_filtered("你好。<think>我在想"), "你好。");
        assert_eq!(think_filtered("<think>全是思考"), "");
        // 闭合之后，里面的内容被去掉、后面的正文照常放行
        assert_eq!(
            think_filtered("你好。<think>想完了</think>再见。"),
            "你好。再见。"
        );
    }

    /// 标签被切在半个字上也要扣住：流式的碎片边界不可控。
    #[test]
    fn a_half_written_tag_is_held_back_too() {
        assert_eq!(think_filtered("你好。<thi"), "你好。");
        assert_eq!(think_filtered("你好。<think"), "你好。");
        // 不是标签的 `<` 也要恢复：等后续字节到了自然放行
        assert_eq!(think_filtered("1 < 2 是对的。"), "1 < 2 是对的。");
    }

    // ------------------------------------------------ 流式：SSE 解析

    #[test]
    fn sse_delta_reads_the_content_piece() {
        let line = r#"{"choices":[{"delta":{"content":"你好"}}]}"#;
        assert_eq!(sse_delta(line).unwrap().unwrap(), "你好");
        // 首包只带 role、末包只带 finish_reason —— **都不是错误**
        assert!(sse_delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#)
            .unwrap()
            .is_none());
        assert!(
            sse_delta(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
                .unwrap()
                .is_none()
        );
        // 垃圾要报错，不能静默当成「没有正文」——那会变成「这一轮没声音」且查不出来
        assert!(sse_delta("not json").is_err());
    }

    /// 不支持流式的传输层**必须被认出来**，否则会走进去再失败一次。
    ///
    /// 默认实现返回 `false`，而 `answer_and_speak_streamed` 靠它决定
    /// 走老路还是走流式——这条判据错了，用户要么白等、要么直接没声音。
    #[test]
    fn a_transport_that_cannot_stream_says_so() {
        let fake = Fake::always("{}");
        assert!(!fake.supports_stream(), "测试替身默认不支持流式");
        let engine = OpenAiCompat::new(
            String::from("http://127.0.0.1:1"),
            std::sync::Arc::new(fake),
            1,
        );
        assert!(!engine.streams(), "引擎要把传输层的能力如实报出来");
    }

    /// ⚠️ 这条用例的名字是承重的：`--lang zh` 被 CLI 拒过一次，
    /// 通话语言的取值必须和 ASR/CLI 那一侧对齐，不能各自发明。
    #[test]
    fn talk_lang_matches_the_cli_vocabulary() {
        for (text, expected) in [("zh", TalkLang::Zh), ("en", TalkLang::En), ("th", TalkLang::Th)] {
            assert_eq!(TalkLang::parse(text).unwrap(), expected);
            assert_eq!(expected.as_str(), text);
        }
        assert!(TalkLang::parse("jp").is_err(), "不认识的语言要报错，不要猜");
        assert!(TalkLang::parse("Zh").is_err(), "大小写不宽容");
    }

    #[test]
    fn say_voices_are_the_three_measured_ones() {
        assert_eq!(TalkLang::Zh.say_voice(), "Tingting");
        assert_eq!(TalkLang::En.say_voice(), "Samantha");
        assert_eq!(TalkLang::Th.say_voice(), "Kanya");
    }

    #[test]
    fn system_prompt_asks_for_the_reply_language_and_the_local_fact() {
        let prompt = system_prompt(TalkLang::Th, "今天清迈多云");
        assert!(prompt.contains("ภาษาไทย"), "提示词要指定回答语言：{prompt}");
        assert!(prompt.contains("一个其他语言的词都不要出现"), "非中文要钉死语言：{prompt}");
        assert!(prompt.contains("今天清迈多云"), "本地事实必须进提示词：{prompt}");
        assert!(prompt.contains("不要说自己无法获取天气数据"), "2B 模型会拒答，必须挡掉");
    }

    /// mock 的用途是「证明链路通」，所以三种语言的问法都要认，
    /// 认不出时也不能报错。
    #[test]
    fn weather_mock_answers_in_three_languages() {
        let mock = WeatherMock::new("今天清迈多云，最高 32 度");
        for question in ["今天天气怎么样", "What is the weather?", "วันนี้อากาศเป็นอย่างไร"] {
            let reply = mock.reply("", question, TalkLang::Zh).unwrap();
            assert_eq!(reply, "今天清迈多云，最高 32 度", "问句 {question:?} 没被认出来");
        }
        let other = mock.reply("", "帮我算一下 2+2", TalkLang::Zh).unwrap();
        assert!(!other.is_empty(), "非天气问题也要给一句话，不能是空回复");
    }

    /// 复述问句不是回答，必须挡住——否则 TTS 会把用户自己的问题念回给他听。
    /// 实测依据：2026-09-14 泰语那一轮 MiniCPM5-2B 真的这么答过一次。
    #[test]
    fn echoing_the_question_is_not_an_answer() {
        // 真出现过的那一条
        assert!(is_echo(
            "วันนี้อากาศเป็นอย่างไรบ้าง",
            "วันนี้อากาศเป็นอย่างไรบ้าง"
        ));
        // 标点/大小写/空白不同也算复述
        assert!(is_echo("What's the weather like today?", "whats the weather like today"));
        assert!(is_echo("今天天气怎么样？", "今天天气怎么样"));
        // 真的在回答就不能误杀
        assert!(!is_echo("今天清迈多云，最高 32 度。", "今天天气怎么样"));
        assert!(!is_echo("วันนี้อากาศดี", "วันนี้อากาศเป็นอย่างไรบ้าง"));
        // 太短的相等不算复述（「好」就是在回答）
        assert!(!is_echo("好", "好"));
        assert!(!is_echo("", ""));
    }

    /// 思考段必须被剥掉：它会被念出来，用户听到的是一段内心独白。
    #[test]
    fn thinking_blocks_never_reach_the_tts() {
        assert_eq!(strip_thinking("今天天气不错。"), "今天天气不错。");
        assert_eq!(
            strip_thinking("<think>\n先想一下\n</think>\n今天天气不错。"),
            "今天天气不错。"
        );
        assert_eq!(strip_thinking("<think>half"), "<think>half", "没闭合时不要吞掉正文");
    }

    #[test]
    fn openai_engine_reports_a_truncated_answer_as_an_error() {
        // finish_reason=length 的半句话不能当成成功结果（sidecar 那轮定的规矩）
        let transport = Fake::always(
            r#"{"choices":[{"message":{"content":"今天清迈多云"},"finish_reason":"length"}]}"#,
        );
        let engine = OpenAiCompat::new("http://127.0.0.1:1", Arc::new(transport), 1);
        assert!(engine.reply("sys", "hi", TalkLang::Zh).is_err());
    }

    #[test]
    fn openai_engine_returns_the_content_of_a_good_response() {
        let transport = Fake::always(
            r#"{"choices":[{"message":{"content":"今天清迈多云，最高 32 度。"},"finish_reason":"stop"}]}"#,
        );
        let engine = OpenAiCompat::new("http://127.0.0.1:1", Arc::new(transport), 1);
        let reply = engine.reply("sys", "今天天气怎么样", TalkLang::Zh).unwrap();
        assert_eq!(reply, "今天清迈多云，最高 32 度。");
    }

    /// 「HTTP 200 但正文不是 WAV」是最危险的静默失败：afplay 会去放一个
    /// 文本文件，用户听到沉默，日志里一切正常。
    #[test]
    fn non_wav_tts_output_is_rejected() {
        assert!(validate_wav(b"{\"error\":\"nope\"}").is_err());
        assert!(validate_wav(b"").is_err());
        assert!(validate_wav(b"RIFF____WAVE").is_err(), "太短的一律不当 WAV");
        let mut ok = b"RIFF".to_vec();
        ok.extend_from_slice(&[0u8; 40]);
        ok[8..12].copy_from_slice(b"WAVE");
        assert!(validate_wav(&ok).is_ok());
    }

    /// **测的是 `stop_playback`/`play_blocking` 这两个原语，不是 V1 打断
    /// 机制本身**（2026-09-20 改名前的名字 `stop_playback_really_cuts_the_
    /// sound_short` 暗示了后者，是过度声称）。这里用一个 spawn 出来的线程
    /// 模拟"播放跑在另一个线程"，从测试主线程直接调 `stop_playback()`——
    /// 这验证的是原语本身线程安全、`play_blocking` 能正确感知被掐断，
    /// **不验证** `main.rs::worker()` 那条真实的"按键 → channel →
    /// `stop_playback()`"路径接得对不对（2026-09-19 codex 复查发现过
    /// 一次这条路径实际接错了——播放调用曾经同步跑在 worker 线程里，
    /// 导致这条原语从未在真实播放中被触发过；2026-09-20 已修，
    /// 见 `docs/agent/tasks.md` T3.4.2）。
    /// `play_blocking` 要把被掐断当成「播完了」正常返回——被打断不是
    /// 错误，调用方拿到的是一个明显短于音频总长的时长。
    ///
    /// ⚠️ **标了 `ignore`：它需要能出声的环境。** `afplay` 在 macOS 上恒在，
    /// 但**没有可用输出设备**（CI runner、无声卡容器）时它会直接失败，
    /// 那种失败不是「打断坏了」。仓库里另外 5 条 ignored 用例是同一个套路
    /// （4 条要边车、1 条要联网）。
    /// 本机实测：`cargo test -- --ignored` 里它通过（见 `docs/benchmarks-talk.md`）。
    #[test]
    #[ignore = "需要能出声的环境（afplay 要有可用输出设备）"]
    fn direct_stop_returns_playback_thread_quickly() {
        // 造一段 5 秒的 440 Hz 正弦波——够长，短了就看不出「被掐」
        let rate = 16000u32;
        let seconds = 5u32;
        let samples: Vec<f32> = (0..rate * seconds)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2
            })
            .collect();
        let wav = crate::talk::tests_support::wav_from_samples(&samples, rate);

        let handle = std::thread::spawn(move || play_blocking(&wav));
        // 让它真的开始播（afplay 拉起需要几十毫秒）
        std::thread::sleep(Duration::from_millis(600));
        // ⚠️ **这条量的不是"端到端"打断延迟，一轮 codex 评审指出来的**：
        // 起点是直接调用 stop_playback()，**不包含**：
        //   ① 键盘事件从系统分发到 channel 里的延迟——`classify_tap()`
        //      本身是按键一松开立刻分类、不等待，`DOUBLE_TAP_MAX_MS`
        //      （500ms）只是"两次敲击之间的间隔阈值"，不是单次按键的
        //      处理延迟，**之前这里写"判定窗口量级 500ms"是错的，已删**；
        //      但真实的系统级事件分发（CGEventTap → channel）仍有它
        //      自己未测的延迟；
        //   ② 主循环收到"结束一段"信号到调用 stop_playback() 之间的
        //      channel 分发延迟；
        //   ③ `play_blocking` 返回到扬声器缓冲区真正播完（听不见了）
        //      之间的音频设备延迟——这里量的是"线程确认停止"，不是"耳朵
        //      听到安静"。
        // 这条测的是**这条链路里我们代码能控制的那一段**（stop_playback
        // 调用 → play_blocking 感知到并返回），是"端到端"里的一部分，
        // 不是全部。真正的端到端（物理按键 → 真正听不到声音）需要另外测，
        // 这条没做到。
        let internal_interval_started = std::time::Instant::now();
        assert!(stop_playback(), "正在播的时候应该报告「掐掉了」");
        let played = handle.join().unwrap().expect("被打断不该是 Err");
        let internal_interval = internal_interval_started.elapsed();
        eprintln!("stop_playback() → play_blocking 返回，内部区间实测：{internal_interval:?}");
        assert!(
            played < Duration::from_secs(4),
            "掐掉之后不该继续播满 5 秒，实测 {played:?}"
        );
        // ⚠️ 这个 300ms 阈值钉的是**上面这段内部区间**，不是 T3.4.2 出口
        // 判据本身——出口判据要求的是真实的物理按键到声音停止，这条测试
        // 证明不了那件事，只能证明"我们代码这一段没有引入明显延迟"。
        // 实测方差 3–24ms 主要来自 `play_blocking` 的 20ms 轮询间隔
        // （`std::thread::sleep(Duration::from_millis(20))`），不是
        // kill 系统调用本身的抖动——kill 几乎瞬时，轮询检测到才是瓶颈。
        assert!(
            internal_interval < Duration::from_millis(300),
            "stop_playback 到 play_blocking 返回的内部区间应 <300ms，实测 {internal_interval:?}"
        );
        // 幂等：没有在播的时候调用不该 panic，也不该报告成功
        assert!(!stop_playback(), "没有在播时不该报告掐掉了");
    }

    // ---- 边车生命周期（连接优先、拉起兜底）----

    /// **这四种组合就是这条链路的全部判据。** 最容易出的错不是「拉不起来」，
    /// 而是「autostart 开着但命令是空的」被当成已经处理过——用户按了键才发现
    /// 没声音，日志里却什么都没有。
    #[test]
    fn sidecar_action_covers_the_whole_matrix() {
        use EndpointHealth::*;
        let cmd = vec!["/bin/true".to_string()];
        assert_eq!(sidecar_action(Up, false, &[]), SidecarAction::AlreadyUp);
        assert_eq!(sidecar_action(Up, true, &cmd), SidecarAction::AlreadyUp);
        assert_eq!(sidecar_action(Down, false, &cmd), SidecarAction::CantStartDisabled);
        assert_eq!(sidecar_action(Down, true, &[]), SidecarAction::CantStartNoCommand);
        assert_eq!(sidecar_action(Down, true, &cmd), SidecarAction::Start);
        // 已经在跑时，另外两个参数不该影响判断（别去动一个健康的服务）
        assert_eq!(sidecar_action(Up, true, &[]), SidecarAction::AlreadyUp);
        // **端口被别人占着：绝不在原端口上 `Start`**（2026-09-26 的 bug：
        // 占用者回 401 被当成「没起」，拉起后撞端口立刻退出，原因被盖住）。
        // v0.22.0 起：能自己拉的就换端口拉（Relocate），拉不了的只报（PortTaken）。
        assert_eq!(sidecar_action(WrongService, true, &cmd), SidecarAction::Relocate);
        for (auto, c) in [(true, &[][..]), (false, &cmd[..]), (false, &[][..])] {
            assert_eq!(sidecar_action(WrongService, auto, c), SidecarAction::PortTaken);
        }
        for (auto, c) in [(true, &cmd[..]), (true, &[][..]), (false, &cmd[..]), (false, &[][..])] {
            assert_ne!(sidecar_action(WrongService, auto, c), SidecarAction::Start);
        }
    }

    #[test]
    fn replace_port_keeps_host_and_path() {
        assert_eq!(replace_port("http://127.0.0.1:8796", 50123).as_deref(), Some("http://127.0.0.1:50123"));
        assert_eq!(replace_port("http://127.0.0.1:8794/v1", 5).as_deref(), Some("http://127.0.0.1:5/v1"));
        assert_eq!(replace_port("http://localhost", 9).as_deref(), Some("http://localhost:9"));
        assert_eq!(replace_port("http://[::1]:8796/x", 7).as_deref(), Some("http://[::1]:7/x"));
        // 认不出的形状一律不换：宁可不避让，也不拼一个错地址去白等 90 秒
        assert_eq!(replace_port("127.0.0.1:8796", 1), None);
        assert_eq!(replace_port("unix:///tmp/x.sock", 1), None);
        assert_eq!(replace_port("http://user:pw@host:1", 2), None);
        assert_eq!(replace_port("http://", 2), None);
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    /// **jason 本机的拉起命令里写着 `AGENTEAR_TTS_PORT=8766`**——不删掉它，
    /// 内层赋值会盖住我们的新端口，边车照样起在被占的那个上。
    #[test]
    fn with_port_env_replaces_an_existing_assignment() {
        let cmd = v(&["/usr/bin/env", "AGENTEAR_TTS_QUANT=8bit", "AGENTEAR_TTS_PORT=8766", "/x/serve-tts.sh"]);
        assert_eq!(
            with_port_env(&cmd, "AGENTEAR_TTS_PORT", 50000),
            Some(v(&["/usr/bin/env", "AGENTEAR_TTS_QUANT=8bit", "AGENTEAR_TTS_PORT=50000", "/x/serve-tts.sh"]))
        );
        // env 的选项原样保留，新赋值紧挨着程序名（放在 `-i` 前面会被当成程序名）
        let cmd = v(&["env", "-i", "-u", "HOME", "AGENTEAR_TTS_PORT=8766", "/x/s.sh"]);
        assert_eq!(
            with_port_env(&cmd, "AGENTEAR_TTS_PORT", 9),
            Some(v(&["env", "-i", "-u", "HOME", "AGENTEAR_TTS_PORT=9", "/x/s.sh"]))
        );
        // 不以 env 开头：外面套一层
        assert_eq!(
            with_port_env(&v(&["/x/serve-talk-llm.sh"]), "AGENTEAR_TALK_LLM_PORT", 7),
            Some(v(&["/usr/bin/env", "AGENTEAR_TALK_LLM_PORT=7", "/x/serve-talk-llm.sh"]))
        );
        // 程序参数里长得像赋值的东西（在程序名之后）不许动
        let cmd = v(&["env", "A=1", "/x/s.sh", "AGENTEAR_TTS_PORT=1"]);
        assert_eq!(
            with_port_env(&cmd, "AGENTEAR_TTS_PORT", 2),
            Some(v(&["env", "A=1", "AGENTEAR_TTS_PORT=2", "/x/s.sh", "AGENTEAR_TTS_PORT=1"]))
        );
        // 结果里我们那条之后不能再有同名的环境赋值
        let out = with_port_env(
            &v(&["/usr/bin/env", "AGENTEAR_TTS_PORT=8766", "AGENTEAR_TTS_PORT=9", "/x/s.sh"]),
            "AGENTEAR_TTS_PORT",
            3,
        )
        .unwrap();
        assert_eq!(out.iter().filter(|a| a.starts_with("AGENTEAR_TTS_PORT=")).count(), 1);
        assert_eq!(out, v(&["/usr/bin/env", "AGENTEAR_TTS_PORT=3", "/x/s.sh"]));
        // `--` 之后是程序名：赋值必须放在 `--` 之前
        assert_eq!(
            with_port_env(&v(&["env", "A=1", "--", "/x/s.sh"]), "AGENTEAR_TTS_PORT", 4),
            Some(v(&["env", "A=1", "AGENTEAR_TTS_PORT=4", "--", "/x/s.sh"]))
        );
    }

    /// `env -S`（整串拆分）改写不可靠 → 明确拒绝（评审 Low 项），调用方据此不换端口。
    #[test]
    fn with_port_env_refuses_split_string() {
        for cmd in [
            v(&["env", "-S", "AGENTEAR_TTS_PORT=8766 /x/s.sh"]),
            v(&["/usr/bin/env", "-SAGENTEAR_TTS_PORT=1 /x/s.sh"]),
            v(&["env", "--split-string=A=1 /x/s.sh"]),
        ] {
            assert_eq!(with_port_env(&cmd, "AGENTEAR_TTS_PORT", 5), None, "{cmd:?}");
        }
    }

    #[test]
    fn free_local_port_gives_a_bindable_port() {
        let p = free_local_port().expect("本机总该有一个空闲端口");
        assert_ne!(p, 0);
        std::net::TcpListener::bind(("127.0.0.1", p)).expect("刚分到的端口应当能 bind");
    }

    /// 覆盖只对「同一个配置地址」生效：用户改了 config，旧覆盖必须失效。
    #[test]
    fn overrides_follow_the_configured_url() {
        let t = vec![Override {
            name: "TTS",
            configured: "http://127.0.0.1:8796".to_string(),
            effective: "http://127.0.0.1:50001".to_string(),
            pid: 77,
        }];
        assert_eq!(
            resolve_override(&t, "TTS", "http://127.0.0.1:8796"),
            Some(("http://127.0.0.1:50001".to_string(), 77))
        );
        assert_eq!(resolve_override(&t, "TTS", "http://127.0.0.1:8766"), None, "配置改了就不再覆盖");
        assert_eq!(resolve_override(&t, "LLM", "http://127.0.0.1:8796"), None, "不串到别的边车");
    }

    /// 接回崩溃前的边车：三条都满足才接（尤其 pid 必须对得上——pid 会被复用）。
    #[test]
    fn can_adopt_truth_table() {
        use EndpointHealth::*;
        let e = RunEntry {
            name: "TTS".into(),
            configured: "http://127.0.0.1:8796".into(),
            url: "http://127.0.0.1:50001".into(),
            pid: 4242,
        };
        let c = "http://127.0.0.1:8796";
        assert!(can_adopt(&e, c, Up, Some(4242)));
        assert!(!can_adopt(&e, c, Up, Some(999)), "端口上是别的进程：不接（退出时会误杀它）");
        assert!(!can_adopt(&e, c, Up, None));
        assert!(!can_adopt(&e, c, Down, Some(4242)));
        assert!(!can_adopt(&e, c, WrongService, Some(4242)));
        assert!(!can_adopt(&e, "http://127.0.0.1:8766", Up, Some(4242)), "配置改了：不接");
    }

    fn entry(configured: &str, pid: i32) -> RunEntry {
        RunEntry {
            name: "TTS".into(),
            configured: configured.into(),
            url: "http://127.0.0.1:50001".into(),
            pid,
        }
    }

    /// 还是我们那个进程但不能再用（配置改了 / 不应答）→ **先收掉再删记录**；
    /// 不是我们的进程 → 只删记录，绝不发信号。
    #[test]
    fn adopt_decision_truth_table() {
        use AdoptDecision::*;
        use EndpointHealth::*;
        let c = "http://127.0.0.1:8796";
        let e = entry(c, 4242);
        assert_eq!(adopt_decision(&e, c, Up, Some(4242)), Adopt);
        assert_eq!(adopt_decision(&e, "http://127.0.0.1:8766", Up, Some(4242)), KillThenForget, "配置改了：收掉");
        assert_eq!(adopt_decision(&e, c, WrongService, Some(4242)), KillThenForget, "卡死了：收掉");
        assert_eq!(adopt_decision(&e, c, Up, Some(999)), Forget, "端口上是别人：绝不发信号");
        assert_eq!(adopt_decision(&e, "http://127.0.0.1:8766", Up, Some(999)), Forget);
        assert_eq!(adopt_decision(&e, c, Down, None), Forget, "死了：只删记录");
    }

    /// **调用点**：判断必须用真的查到的监听者，而不是记录里写的 pid。
    /// 评审实测过：把调用点的查询改成恒等于记录 pid，只测纯函数时一个都不红。
    #[test]
    fn judge_run_entry_uses_the_real_listener() {
        use AdoptDecision::*;
        let c = "http://127.0.0.1:8796";
        let e = entry(c, 4242);
        let up = |_: &str| EndpointHealth::Up;
        let someone_else = |_: u16| Some(999);
        let us = |port: u16| (port == 50001).then_some(4242);
        let nobody = |_: u16| None;
        assert_eq!(judge_run_entry(&e, c, &up, &someone_else), Forget, "端口被别的进程接手了：不接、不杀");
        assert_eq!(judge_run_entry(&e, c, &up, &nobody), Forget);
        assert_eq!(judge_run_entry(&e, c, &up, &us), Adopt, "查的得是记录里 url 的端口");
        assert_eq!(judge_run_entry(&e, "http://127.0.0.1:8766", &up, &us), KillThenForget);
    }

    #[test]
    fn adopted_kill_needs_the_listener_to_still_be_that_pid() {
        assert!(adopted_kill_ok(4242, Some(4242)));
        assert!(!adopted_kill_ok(4242, Some(999)), "pid 被复用：不发信号");
        assert!(!adopted_kill_ok(4242, None));
        assert!(!adopted_kill_ok(0, Some(0)));
    }

    // ---- 进程登记表按 pid 管理（2026-09-26 评审阻塞项）----
    //
    // 这几条碰全局登记表（SPAWNED / SPAWNED_PIDS / ADOPTED / OVERRIDES），用同一把锁串行跑。

    static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn in_spawned(pid: i32) -> bool {
        SPAWNED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(_, c)| c.id() as i32 == pid)
    }

    fn alive(pid: i32) -> bool {
        // 我们自己的子进程：还在登记表里就用 try_wait 看；已经被回收的，kill(0) 会失败。
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// 阻塞项 ④：同名的旧进程死了还在表里时，按名字匹配会命中死的那个、
    /// 把刚拉起的活进程一起丢掉。按 pid 就不会。
    #[test]
    fn wait_ready_matches_by_pid_not_by_name() {
        let _g = registry_guard();
        let dead = spawn_sidecar("TEST-W", &v(&["/usr/bin/true"])).unwrap();
        std::thread::sleep(Duration::from_millis(200)); // 让它先退出（不回收）
        let live = spawn_sidecar("TEST-W", &v(&["/bin/sleep", "30"])).unwrap();
        let out = wait_ready("TEST-W", live, "http://127.0.0.1:1", Duration::from_millis(1200));
        assert_eq!(out, ReadyOutcome::TimedOut, "活着的新进程不该被当成「已退出」");
        assert!(in_spawned(live), "活进程的句柄必须还在登记表里");
        assert!(registered_pids().contains(&live));
        assert!(kill_child(live));
        assert!(!in_spawned(live) && !registered_pids().contains(&live));
        reap_dead_children();
        assert!(!in_spawned(dead) && !registered_pids().contains(&dead));
    }

    /// 观测到退出的那一刻，**同时**从登记表与信号槽位里摘除（阻塞项 ①②）。
    #[test]
    fn an_exited_child_is_unregistered_when_observed() {
        let _g = registry_guard();
        let pid = spawn_sidecar("TEST-E", &v(&["/usr/bin/true"])).unwrap();
        assert!(registered_pids().contains(&pid));
        let out = wait_ready("TEST-E", pid, "http://127.0.0.1:1", Duration::from_secs(5));
        assert_eq!(out, ReadyOutcome::Exited);
        assert!(!in_spawned(pid), "退出的进程要从登记表里摘掉");
        assert!(!registered_pids().contains(&pid), "退出的 pid 要从信号槽位里摘掉——否则回收后被复用会误杀");
    }

    #[test]
    fn reap_dead_children_frees_slots() {
        let _g = registry_guard();
        let pid = spawn_sidecar("TEST-R", &v(&["/usr/bin/true"])).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        reap_dead_children();
        assert!(!in_spawned(pid) && !registered_pids().contains(&pid));
    }

    /// 阻塞项 ③：换端口拉起的进程等满了没就绪 → 必须杀掉并摘除，不留孤儿。
    #[test]
    fn relocate_timeout_kills_and_unregisters_the_child() {
        let _g = registry_guard();
        let spec = Sidecar {
            name: "TEST-T",
            url: "http://127.0.0.1:1".into(),
            command: v(&["/bin/sleep", "30"]),
            explicit: false,
        };
        let before: Vec<i32> = registered_pids();
        assert!(!relocate_and_start_with(&spec, "测试", Duration::from_millis(1200)));
        let leftovers: Vec<i32> = SPAWNED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(n, _)| *n == "TEST-T")
            .map(|(_, c)| c.id() as i32)
            .collect();
        assert!(leftovers.is_empty(), "超时后子进程还挂在登记表里：{leftovers:?}");
        assert_eq!(registered_pids(), before, "超时后信号槽位里多了东西");
        assert!(override_entry("TEST-T", &spec.url).is_none());
    }

    /// 作废一条覆盖 = 连同它的进程一起收掉并摘除所有登记（阻塞项 ①）。
    #[test]
    fn discard_relocated_kills_our_child_and_clears_registrations() {
        let _g = registry_guard();
        let pid = spawn_sidecar("TEST-D", &v(&["/bin/sleep", "30"])).unwrap();
        set_override("TEST-D", "http://127.0.0.1:1", "http://127.0.0.1:2", pid);
        discard_relocated("TEST-D", "http://127.0.0.1:1");
        assert!(override_entry("TEST-D", "http://127.0.0.1:1").is_none());
        assert!(!in_spawned(pid) && !registered_pids().contains(&pid));
        assert!(!alive(pid), "作废时要把进程也收掉");
    }

    /// 退出时：接回来的 pid 如果已经不在它的端口上了（死了、被复用），**不发信号**。
    #[test]
    fn shutdown_does_not_signal_an_adopted_pid_that_moved_on() {
        let _g = registry_guard();
        // 一个跟端口无关的「别人的进程」，冒充 pid 被复用后的样子
        let mut bystander = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = bystander.id() as i32;
        let port = free_local_port().unwrap(); // 没人在听
        ADOPTED.lock().unwrap_or_else(|e| e.into_inner()).push(("TEST-S", pid, port));
        shutdown_spawned();
        std::thread::sleep(Duration::from_millis(200));
        let still_running = matches!(bystander.try_wait(), Ok(None));
        let _ = bystander.kill();
        let _ = bystander.wait();
        assert!(still_running, "退出时误杀了一个已经不在那个端口上的 pid");
    }

    /// 测试收尾守卫：**断言失败（panic）时也要把假边车和临时目录收掉**，
    /// 否则变异验证每红一次就漏一个常驻的 http.server。
    struct FakeSidecar {
        child: std::process::Child,
        dir: PathBuf,
    }
    impl Drop for FakeSidecar {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
            if let Some(run_dir) = run_file().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
                let _ = std::fs::remove_dir_all(run_dir);
            }
        }
    }

    /// 起一个**真的在听端口**的进程（`/health` 回 200），冒充换端口拉起的边车。
    fn fake_sidecar() -> (std::process::Child, u16, PathBuf) {
        let dir = temp_dir("agentear-fake-sidecar").unwrap();
        std::fs::write(dir.join("health"), "ok").unwrap();
        let port = free_local_port().unwrap();
        let child = Command::new("/usr/bin/python3")
            .args(["-m", "http.server", &port.to_string(), "--bind", "127.0.0.1", "--directory"])
            .arg(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..50 {
            if port_listener_pid(port) == Some(child.id() as i32) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        (child, port, dir)
    }

    /// **`adopt_from_run_file` 这个真实调用点**的三种结局，都用真进程、真端口、真 lsof。
    #[test]
    fn adopt_from_run_file_call_site() {
        let _g = registry_guard();
        let (child, port, dir) = fake_sidecar();
        let pid = child.id() as i32;
        let mut guard = FakeSidecar { child, dir };
        let sidecar = &mut guard.child;
        let url = format!("http://127.0.0.1:{port}");
        let configured = "http://127.0.0.1:1";
        let put = |pid: i32| {
            save_run_entries(&[RunEntry {
                name: "TEST-A".into(),
                configured: configured.into(),
                url: url.clone(),
                pid,
            }])
        };
        let has_entry = || load_run_entries().iter().any(|e| e.name == "TEST-A");

        // ① 记录的 pid 不是在听端口的那个（pid 被复用）→ 不接、不发信号、只删记录
        put(pid + 100_000);
        assert_eq!(adopt_from_run_file("TEST-A", configured), None);
        assert!(!has_entry());
        assert!(matches!(sidecar.try_wait(), Ok(None)), "pid 对不上时绝不能发信号");

        // ② 三条都满足 → 接回：登记到 ADOPTED + 覆盖，但**不进信号槽位**
        put(pid);
        assert_eq!(adopt_from_run_file("TEST-A", configured).as_deref(), Some(url.as_str()));
        assert!(ADOPTED.lock().unwrap().iter().any(|(n, p, _)| *n == "TEST-A" && *p == pid));
        assert!(!registered_pids().contains(&pid), "接回的 pid 不许进信号槽位");
        ADOPTED.lock().unwrap().retain(|(n, _, _)| *n != "TEST-A");
        clear_override("TEST-A");

        // ③ 配置改了、但还是我们那个进程 → 先收掉再删记录（评审 Low 项）
        put(pid);
        assert_eq!(adopt_from_run_file("TEST-A", "http://127.0.0.1:2"), None);
        assert!(!has_entry());
        let mut gone = false;
        for _ in 0..30 {
            if matches!(sidecar.try_wait(), Ok(Some(_))) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        save_run_entries(&[]);
        drop(guard);
        assert!(gone, "配置改了时，还在跑的旧边车要被收掉，不然永远没人管");
    }

    /// **真边车冒烟**（默认忽略：要本机装好 TTS 边车）。不起守护进程——
    /// 守护进程会装按键监听，实测时会截到正在用键盘的人（2026-09-26 踩过）。
    ///
    /// ```text
    /// AGENTEAR_SMOKE_TTS_CMD='/usr/bin/env AGENTEAR_TTS_QUANT=4bit scripts/serve-tts.sh' \
    ///   cargo test -- --ignored smoke_relocate_real_tts --nocapture
    /// ```
    #[test]
    #[ignore]
    fn smoke_relocate_real_tts() {
        let _g = registry_guard();
        let cmd: Vec<String> = std::env::var("AGENTEAR_SMOKE_TTS_CMD")
            .expect("设 AGENTEAR_SMOKE_TTS_CMD")
            .split_whitespace()
            .map(String::from)
            .collect();
        // 占住一个端口，冒充「别的程序」
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = blocker.local_addr().unwrap().port();
        let blocker_thread = std::thread::spawn(move || {
            for s in blocker.incoming().flatten() {
                let mut s = s;
                let _ = s.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let mut cfg = crate::config::Config::default();
        cfg.talk_llm_engine = "mock".into();
        cfg.tts_url = Some(format!("http://127.0.0.1:{busy}"));
        cfg.talk_tts_start_command = cmd;
        cfg.talk_autostart = true;
        let results = ensure_sidecars(&cfg);
        assert_eq!(results, vec![("TTS", true)], "换端口拉起失败");
        let configured = cfg.tts_url.clone().unwrap();
        let (url, pid) = override_entry("TTS", &configured).expect("应当有覆盖");
        assert_ne!(url, configured);
        assert!(registered_pids().contains(&pid) && in_spawned(pid));
        assert_eq!(load_run_entries().iter().filter(|e| e.pid == pid).count(), 1);
        let wav = Engines::from_config(&cfg).tts.synthesize("端口换好了。", TalkLang::Zh).unwrap();
        assert!(wav.len() > 1000, "避让端口上的 TTS 应当能合成");
        eprintln!("smoke: 被占 {busy} → 换到 {url}（pid {pid}），合成 {} 字节", wav.len());
        // 第二次 ensure：直接复用，不再起一份
        assert_eq!(ensure_sidecars(&cfg), vec![("TTS", true)]);
        assert_eq!(SPAWNED.lock().unwrap().iter().filter(|(n, _)| *n == "TTS").count(), 1, "不许重复拉起");
        shutdown_spawned();
        assert!(!alive(pid), "退出时要收掉");
        assert!(registered_pids().is_empty() && override_entry("TTS", &configured).is_none());
        assert!(load_run_entries().is_empty(), "退出后运行态记录要清掉");
        drop(blocker_thread); // 线程随进程结束
    }

    /// 接回来的 pid **不进信号槽位**：信号处理函数没法校验，复用了就是误杀。
    #[test]
    fn adopted_pids_never_enter_the_signal_slots() {
        let src = include_str!("talk.rs");
        let body = &src[src.find("fn adopt_from_run_file").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(!body.contains("register_pid"), "adopt_from_run_file 不许把接回的 pid 放进信号槽位");
    }

    #[test]
    fn parse_lsof_pid_takes_the_first_process() {
        assert_eq!(parse_lsof_pid("p123\nfcwd\np456\n"), Some(123));
        assert_eq!(parse_lsof_pid(""), None);
        assert_eq!(parse_lsof_pid("pabc\n"), None);
    }

    /// 节流：同一原因 3 分钟内只念一次；原因变了立刻念。
    #[test]
    fn should_notice_truth_table() {
        let t0 = Instant::now();
        let w = UNAVAILABLE_NOTICE_WINDOW;
        assert!(should_notice(None, "TTS", t0, w), "第一次一定念");
        assert!(!should_notice(Some(("TTS", t0)), "TTS", t0 + Duration::from_secs(10), w), "刚念过：不念");
        assert!(!should_notice(Some(("TTS", t0)), "TTS", t0 + w - Duration::from_millis(1), w));
        assert!(should_notice(Some(("TTS", t0)), "TTS", t0 + w, w), "过了窗口：再念");
        assert!(should_notice(Some(("TTS", t0)), "LLM+TTS", t0 + Duration::from_secs(1), w), "原因变了：立刻念");
    }

    #[test]
    fn unavailable_notice_is_in_the_talk_language() {
        assert!(unavailable_notice(TalkLang::Zh).contains("语音服务"));
        assert!(unavailable_notice(TalkLang::En).contains("voice service"));
        assert!(unavailable_notice(TalkLang::Th).chars().any(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c)));
    }

    /// curl 退出码 × HTTP 状态 → 三种状态的真值表。**第一行就是 jason 撞上的那个**。
    #[test]
    fn classify_probe_truth_table() {
        use EndpointHealth::*;
        let rows: &[(Option<i32>, Option<u16>, EndpointHealth)] = &[
            (Some(0), Some(401), WrongService), // Chat On Steroids 占着 8765，回 401
            (Some(0), Some(200), Up),
            (Some(0), Some(204), Up),
            (Some(0), Some(404), WrongService),
            (Some(0), Some(500), WrongService),
            (Some(0), Some(302), WrongService),
            (Some(0), None, WrongService), // 拿不到状态码：保守
            (Some(0), Some(0), WrongService),
            (Some(7), None, Down), // 连接被拒：只有这一档允许拉起
            (Some(7), Some(0), Down),
            (Some(28), None, WrongService), // 超时
            (Some(52), None, WrongService), // 空回复
            (Some(56), None, WrongService), // 收包失败
            (None, None, WrongService),     // curl 被信号杀了
        ];
        for (code, status, want) in rows {
            assert_eq!(classify_probe(*code, *status), *want, "curl={code:?} http={status:?}");
        }
    }

    /// 真起一个回 401 的端口，**走真 curl**，确认判成 WrongService 而不是 Down。
    #[test]
    fn a_401_listener_is_wrong_service_not_down() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let body = r#"{"error":"unauthorised"}"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let url = format!("http://127.0.0.1:{port}");
        let (health, detail) = probe_health(&url);
        h.join().unwrap();
        assert_eq!(health, EndpointHealth::WrongService, "{detail}");
        assert!(detail.contains("401"), "细节里要带上状态码：{detail}");
        assert!(probe_endpoint(&url).is_err(), "旧接口对被占端口也必须报不可用");
    }

    /// 没人听的端口 = Down（先绑一个端口拿到号，再放掉）。
    #[test]
    fn a_closed_port_is_down() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (health, detail) = probe_health(&format!("http://127.0.0.1:{port}"));
        assert_eq!(health, EndpointHealth::Down, "{detail}");
    }

    #[test]
    fn port_of_reads_explicit_and_default_ports() {
        assert_eq!(port_of("http://127.0.0.1:8765"), Some(8765));
        assert_eq!(port_of("http://127.0.0.1:8766/"), Some(8766));
        assert_eq!(port_of("http://localhost:8794/v1"), Some(8794));
        assert_eq!(port_of("http://[::1]:8765"), Some(8765));
        assert_eq!(port_of("http://user:pw@host:9000/x"), Some(9000));
        assert_eq!(port_of("http://example.com"), Some(80));
        assert_eq!(port_of("https://example.com/a"), Some(443));
        assert_eq!(port_of("127.0.0.1:8765"), None, "没有协议头不猜");
        assert_eq!(port_of("http://h:notaport"), None);
    }

    #[test]
    fn lsof_occupant_is_parsed_from_field_output() {
        assert_eq!(
            parse_lsof_occupant("p59906\ncChat On Steroids\n").as_deref(),
            Some("Chat On Steroids（pid 59906）")
        );
        // 两个进程只取第一个
        assert_eq!(
            parse_lsof_occupant("p1\ncA\np2\ncB\n").as_deref(),
            Some("A（pid 1）")
        );
        assert_eq!(parse_lsof_occupant(""), None);
    }

    /// 「被占了」必须同时说出**该改哪里**。
    #[test]
    fn port_taken_message_says_what_to_change() {
        let m = describe_port_taken("TTS", "http://127.0.0.1:1", "HTTP 401", false);
        assert!(m.contains("tts_url") && m.contains("AGENTEAR_TTS_PORT"), "{m}");
        assert!(m.contains("不会去拉起"), "{m}");
        let m = describe_port_taken("LLM", "http://127.0.0.1:1", "HTTP 401", false);
        assert!(m.contains("talk_llm_url") && m.contains("AGENTEAR_TALK_LLM_PORT"), "{m}");
        // 会自动换端口时，不许再说「不会去拉起」（那样用户会去手改配置，白忙）
        let m = describe_port_taken("TTS", "http://127.0.0.1:1", "HTTP 401", true);
        assert!(!m.contains("不会去拉起") && m.contains("自动换"), "{m}");
    }

    // ---- 首轮与边车拉起赛跑（2026-09-26）----

    #[test]
    fn startup_gate_refuses_a_second_concurrent_start() {
        let g = StartupGate::new();
        assert!(g.try_begin());
        assert!(!g.try_begin(), "拉起进行中不许再起第二次");
        g.end();
        assert!(g.try_begin(), "放闸之后可以再起");
    }

    #[test]
    fn waiting_without_a_startup_returns_immediately() {
        let g = StartupGate::new();
        let t = Instant::now();
        assert_eq!(g.wait(Duration::from_secs(5)), StartupWait::NotStarting);
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    /// **这条就是 bug 本身**：回答线程在拉起进行中进来，必须等到拉起结束才走。
    #[test]
    fn a_turn_waits_until_the_startup_finishes() {
        let g = std::sync::Arc::new(StartupGate::new());
        assert!(g.try_begin());
        let g2 = g.clone();
        let ender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            g2.end();
        });
        match g.wait(Duration::from_secs(5)) {
            StartupWait::Finished(d) => {
                assert!(d >= Duration::from_millis(250), "不能没等就走：{d:?}")
            }
            other => panic!("应当等到结束，实际 {other:?}"),
        }
        ender.join().unwrap();
    }

    #[test]
    fn a_turn_gives_up_waiting_at_the_cap() {
        let g = StartupGate::new();
        assert!(g.try_begin());
        let t = Instant::now();
        assert_eq!(g.wait(Duration::from_millis(200)), StartupWait::TimedOut);
        assert!(t.elapsed() >= Duration::from_millis(190));
        assert!(t.elapsed() < Duration::from_secs(2), "上限要真的生效");
    }

    #[test]
    fn a_late_answer_is_dropped_if_the_user_already_started_talking_again() {
        use StartupWait::*;
        use TurnGo::*;
        let d = Duration::from_secs(1);
        assert_eq!(turn_after_wait(NotStarting, 1, 1), Proceed);
        // 没等过就不看序号：今天之前的老路径不改
        assert_eq!(turn_after_wait(NotStarting, 1, 2), Proceed);
        assert_eq!(turn_after_wait(Finished(d), 1, 1), Proceed);
        assert_eq!(turn_after_wait(Finished(d), 1, 2), Superseded);
        assert_eq!(turn_after_wait(TimedOut, 3, 3), Proceed);
        assert_eq!(turn_after_wait(TimedOut, 3, 4), Superseded);
    }

    /// **按引擎派生，不是写死两个。** 用 mock LLM / say TTS 时那一路不需要边车，
    /// 探它只会让日志里出现一条永远连不上的警告。
    #[test]
    fn sidecar_specs_follow_the_configured_engines() {
        let mut cfg = crate::config::Config::default();
        cfg.talk_llm_engine = "mock".to_string();
        cfg.talk_tts_engine = "say".to_string();
        assert!(sidecar_specs(&cfg).is_empty(), "全内置时一个边车都不该探");

        cfg.talk_llm_engine = "openai_compat".to_string();
        let specs = sidecar_specs(&cfg);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "LLM");
        assert_eq!(specs[0].url, DEFAULT_LLM_URL);

        cfg.talk_tts_engine = "http".to_string();
        let specs = sidecar_specs(&cfg);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[1].name, "TTS");
        assert_eq!(specs[1].url, DEFAULT_TTS_URL);

        // 自定义地址要透传下去（用户可能把模型跑在别的机器/端口上）
        cfg.talk_llm_url = Some("http://127.0.0.1:9999".to_string());
        assert_eq!(sidecar_specs(&cfg)[0].url, "http://127.0.0.1:9999");
    }

    /// 「该跑哪条命令」必须在日志里给出来——没有它，「没声音」对用户就是无解的。
    #[test]
    fn startup_hints_name_the_scripts() {
        for name in ["LLM", "TTS"] {
            let hint = startup_hint(name);
            assert!(hint.contains("scripts/serve-"), "{name} 的提示要指向脚本：{hint}");
            assert!(hint.contains("setup-talk"), "首次还要说清先跑 setup：{hint}");
        }
    }

    /// 命令是空的时**不能**去 spawn——空的 argv 会让 `split_first` 拿到 None，
    /// 这里顺手钉住那条错误路径不会 panic。
    #[test]
    fn spawning_an_empty_command_is_an_error_not_a_panic() {
        assert!(spawn_sidecar("LLM", &[]).is_err());
    }

    /// **跨语言契约**：语系/语气的键集必须与 `services/tts/backends.py` 的
    /// `STYLE_INSTRUCTS` / `TONE_INSTRUCTS` 完全一致。
    ///
    /// 两边各有一份实现（菜单不能为列一次表去发 HTTP；边车也读不到 Rust 常量），
    /// 漂移的后果是**菜单点得下去、边车回 400**——那种错很难查，所以两边各钉一条测试。
    /// Python 侧的同名清单在 `services/tts/test_backends.py`。
    #[test]
    fn style_keys_match_the_sidecar_contract() {
        // 菜单把 12 项分成「语言 / 方言」两组：**两组加起来必须正好是全集**，
        // 漏一项 = 菜单里少一个可选项；重一项 = 同一项出现两次。
        {
            let mut grouped: Vec<&str> = LANGUAGE_STYLES
                .iter()
                .chain(DIALECT_STYLES.iter())
                .copied()
                .collect();
            grouped.sort_unstable();
            let mut all: Vec<&str> = STYLE_OPTIONS.iter().map(|o| o.0).collect();
            all.sort_unstable();
            assert_eq!(grouped, all, "语言组 + 方言组必须正好覆盖 STYLE_OPTIONS");
            assert!(LANGUAGE_STYLES.contains(&"zh"), "普通话必须在语言组里");
            assert_eq!(
                STYLE_OPTIONS
                    .iter()
                    .find(|o| o.0 == "zh")
                    .map(|o| o.0),
                Some("zh")
            );
        }
        let keys: Vec<&str> = STYLE_OPTIONS.iter().map(|o| o.0).collect();
        assert_eq!(
            keys,
            vec![
                "zh", "yue", "henan", "sichuan", "shandong", "dongbei", "tianjin", "en",
                "en-gb", "en-us", "en-ca", "th"
            ],
            "改了这里就必须同时改 services/tts/backends.py 的 STYLE_INSTRUCTS"
        );
        let tones: Vec<&str> = TONE_OPTIONS.iter().map(|o| o.0).collect();
        assert_eq!(
            tones,
            vec!["warm", "calm", "lively", "serious"],
            "改这里就必须同时改 TONE_INSTRUCTS"
        );
    }

    /// 三语名字都不许空——空标题的菜单项等于没有这一项。
    #[test]
    fn every_option_has_a_name_in_all_three_languages() {
        for opt in STYLE_OPTIONS.iter().chain(TONE_OPTIONS.iter()) {
            for lang in [crate::i18n::Lang::Zh, crate::i18n::Lang::En, crate::i18n::Lang::Th] {
                assert!(
                    !option_label(opt, lang).trim().is_empty(),
                    "{} 缺 {:?} 的名字",
                    opt.0,
                    lang
                );
            }
        }
    }

    #[test]
    fn http_tts_sends_the_language_it_was_given() {
        struct Echo(std::sync::Mutex<String>);
        impl AudioTransport for Echo {
            fn post_json_bytes(&self, _url: &str, body: &str, _t: u64) -> Result<Vec<u8>> {
                *self.0.lock().unwrap() = body.to_string();
                let mut wav = b"RIFF".to_vec();
                wav.extend_from_slice(&[0u8; 40]);
                wav[8..12].copy_from_slice(b"WAVE");
                Ok(wav)
            }
        }
        let transport = Arc::new(Echo(std::sync::Mutex::new(String::new())));
        let tts = HttpTts::new("http://127.0.0.1:1", transport.clone(), 1);
        tts.synthesize("สวัสดี", TalkLang::Th).unwrap();
        let body = transport.0.lock().unwrap().clone();
        assert!(body.contains("\"lang\":\"th\""), "语言必须显式传下去：{body}");
        assert!(body.contains("สวัสดี"), "正文原样传下去：{body}");
    }
}
