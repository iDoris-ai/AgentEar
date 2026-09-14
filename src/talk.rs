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
}

impl HttpTts {
    pub fn new(url: impl Into<String>, transport: Arc<dyn AudioTransport>, timeout_secs: u64) -> Self {
        Self {
            url: url.into(),
            transport,
            timeout_secs,
        }
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
        let body = serde_json::json!({"text": text, "lang": lang.as_str()}).to_string();
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
                cfg.talk_llm_url
                    .clone()
                    .unwrap_or_else(|| DEFAULT_LLM_URL.to_string()),
                Arc::new(sidecar::Curl),
                cfg.talk_timeout_secs,
            )),
        };
        let tts: Arc<dyn TtsEngine> = match cfg.talk_tts_engine.as_str() {
            "say" => Arc::new(SayTts::new(cfg.talk_timeout_secs)),
            _ => Arc::new(HttpTts::new(
                cfg.tts_url
                    .clone()
                    .unwrap_or_else(|| DEFAULT_TTS_URL.to_string()),
                Arc::new(CurlAudio),
                cfg.talk_timeout_secs,
            )),
        };
        Self { llm, tts }
    }
}

/// 探活：这个地址上有没有东西在听，而且是 HTTP。
///
/// **只回答「通不通」**，不判断对面是不是对的服务——那要看 `/health` 的
/// 内容（`services/tts` 会报 `backend`/`model`）。这里给 `--diagnose` 用的，
/// 它要回答的是用户那句「按了没反应」背后最常见的原因：边车没起。
pub fn probe_endpoint(url: &str) -> Result<()> {
    let health = format!("{}/health", url.trim_end_matches('/'));
    let out = Command::new("/usr/bin/curl")
        .arg("-fsS")
        .arg("--max-time")
        .arg("3")
        .arg(&health)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("跑 curl 失败（{health}）"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("连不上（{}）", err.trim());
    }
    Ok(())
}

/// 一个通话边车的规格：叫什么、连哪、连不上时按什么命令拉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// 只用来写日志（「LLM 边车」「TTS 边车」）。
    pub name: &'static str,
    pub url: String,
    /// 空 = 不知道怎么拉。**这是默认值，不是缺陷**——见 `sidecar_action`。
    pub command: Vec<String>,
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
}

pub fn sidecar_action(up: bool, autostart: bool, command: &[String]) -> SidecarAction {
    if up {
        return SidecarAction::AlreadyUp;
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
        });
    }
    out
}

/// 我们自己拉起来的边车。**只收自己起的**——用户手工跑的进程一律不动，
/// 退出时杀掉别人的服务是很难排查的越权（`sidecar.rs` 定的规矩）。
static SPAWNED: std::sync::Mutex<Vec<(&'static str, std::process::Child)>> =
    std::sync::Mutex::new(Vec::new());

/// 同一批 pid 的**无锁副本**，专供信号处理函数用。
///
/// 信号处理函数必须 async-signal-safe：不能锁 `Mutex`、不能分配内存。
/// 所以这里存原子值，handler 里只做 `kill(2)`（`sidecar.rs` 定的同一条规矩）。
///
/// 通信边车最多两个（LLM + TTS），固定长度就够——**不引 Vec 是因为
/// handler 里不能分配**。
static SPAWNED_PIDS: [std::sync::atomic::AtomicI32; 2] =
    [std::sync::atomic::AtomicI32::new(0), std::sync::atomic::AtomicI32::new(0)];

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

fn register_pid(child: &std::process::Child) {
    use std::sync::atomic::Ordering;
    for slot in SPAWNED_PIDS.iter() {
        if slot
            .compare_exchange(0, child.id() as i32, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
    // 两个槽都满了：说明清单里超过两个边车了（现在不可能，见 sidecar_specs）
    log::warn!("边车 pid 槽位已满，这个进程退出时不会被自动收掉");
}

fn clear_pids() {
    for slot in SPAWNED_PIDS.iter() {
        slot.store(0, std::sync::atomic::Ordering::SeqCst);
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
    for spec in sidecar_specs(cfg) {
        let up = probe_endpoint(&spec.url).is_ok();
        match sidecar_action(up, cfg.talk_autostart, &spec.command) {
            SidecarAction::AlreadyUp => {
                log::info!("{} 边车已在跑：{}", spec.name, spec.url);
                out.push((spec.name, true));
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
                    Ok(()) => {
                        let ready = wait_ready(spec.name, &spec.url);
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
pub fn ensure_sidecars_async(cfg: &crate::config::Config) {
    let cfg = cfg.clone();
    std::thread::spawn(move || {
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
}

fn spawn_sidecar(name: &'static str, command: &[String]) -> Result<()> {
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
    register_pid(&child);
    SPAWNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name, child));
    Ok(())
}

/// 等某个边车就绪。**按名字盯住我们自己起的那个进程**：
/// 它要是启动后就退出（命令写错、venv 缺包），不该再傻等满 90 秒。
fn wait_ready(name: &'static str, url: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        std::thread::sleep(Duration::from_millis(1500));
        if probe_endpoint(url).is_ok() {
            log::info!("{name} 边车等了 {:.1}s", start.elapsed().as_secs_f32());
            return true;
        }
        let mut guard = SPAWNED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, child)) = guard.iter_mut().find(|(n, _)| *n == name) {
            if let Ok(Some(code)) = child.try_wait() {
                log::error!("{name} 边车启动后立刻退出了（{code:?}），检查拉起命令能不能单独跑通");
                guard.retain(|(n, _)| *n != name);
                return false;
            }
        }
    }
    false
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
        let _ = child.kill();
        let _ = child.wait();
    }
    clear_pids();
}

pub const DEFAULT_LLM_URL: &str = "http://127.0.0.1:8794";
pub const DEFAULT_TTS_URL: &str = "http://127.0.0.1:8765";

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

    /// **V1 的打断机制本身**：`stop_playback` 必须真的把声音掐断，
    /// 而且 `play_blocking` 要把它当成「播完了」正常返回——
    /// 被打断不是错误，调用方拿到的是一个明显短于音频总长的时长
    /// （这正是打断延迟可以直接量出来的地方）。
    ///
    /// ⚠️ **标了 `ignore`：它需要能出声的环境。** `afplay` 在 macOS 上恒在，
    /// 但**没有可用输出设备**（CI runner、无声卡容器）时它会直接失败，
    /// 那种失败不是「打断坏了」。仓库里另外 5 条 ignored 用例是同一个套路
    /// （4 条要边车、1 条要联网）。
    /// 本机实测：`cargo test -- --ignored` 里它通过（见 `docs/benchmarks-talk.md`）。
    #[test]
    #[ignore = "需要能出声的环境（afplay 要有可用输出设备）"]
    fn stop_playback_really_cuts_the_sound_short() {
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
        assert!(stop_playback(), "正在播的时候应该报告「掐掉了」");
        let played = handle.join().unwrap().expect("被打断不该是 Err");
        assert!(
            played < Duration::from_secs(4),
            "掐掉之后不该继续播满 5 秒，实测 {played:?}"
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
        let cmd = vec!["/bin/true".to_string()];
        assert_eq!(sidecar_action(true, false, &[]), SidecarAction::AlreadyUp);
        assert_eq!(sidecar_action(true, true, &cmd), SidecarAction::AlreadyUp);
        assert_eq!(sidecar_action(false, false, &cmd), SidecarAction::CantStartDisabled);
        assert_eq!(sidecar_action(false, true, &[]), SidecarAction::CantStartNoCommand);
        assert_eq!(sidecar_action(false, true, &cmd), SidecarAction::Start);
        // 已经在跑时，另外两个参数不该影响判断（别去动一个健康的服务）
        assert_eq!(sidecar_action(true, true, &[]), SidecarAction::AlreadyUp);
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
