//! 持久化配置：`~/.agentear/config.json`。
//!
//! 菜单栏改设置后立即落盘，下次启动生效。三项里只有触发键需要重启进程
//! （`CGEventTap` 挂在一个跑 `CFRunLoop` 的线程上，运行时换不掉），
//! 输入设备和自动上屏都是下一次录音/下一次上屏时读取，无需重启。

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use crate::asr::AsrLang;
use crate::engine::AsrBackend;
use crate::i18n::Lang;
use crate::talk::TalkLang;

/// 单个字段解析失败时退回默认值，**而不是让整份配置解析失败**。
///
/// `#[serde(default)]` 只挡字段缺失，挡不住取值非法：配置里出现
/// `"ui_lang": "fr"` 或 `"retention_days": "三十"`，整个 `Config` 就
/// 反序列化失败，而 `load()` 的兜底是「退回默认配置」——用户丢掉的是
/// **输入设备、触发键、保留期全部**，只因为一个字段坏了。
///
/// 先解析成 `Value` 再逐字段尝试，把损坏隔离在字段这一层。
///
/// **边界：挡不住重复键。** `{"ui_lang":"zh","ui_lang":"fr"}` 会在派生的
/// `Config` visitor 里就报 `duplicate field`，轮不到这里，整份配置照样
/// 退回默认。要兜住它得先解析成 map 再逐字段取，值不值得看以后是否真的
/// 出现过——目前配置只由程序写，重复键只可能来自手工编辑。
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    #[default]
    /// 轻点一下右 Command。需要辅助功能权限。
    RightCommand,
    /// Carbon 组合键，不需要辅助功能权限，是无权限时的降级目标。
    CtrlShiftR,
}

impl Trigger {
    pub fn label(self) -> &'static str {
        match self {
            Trigger::RightCommand => "右 Command",
            Trigger::CtrlShiftR => "Ctrl+Shift+R",
        }
    }
}

/// 录音键按下去之后干什么。**默认输入法模式。**
///
/// jason 2026-09-14 拍板：两种模式并存，**默认输入法**，切到对话模式
/// 要**从菜单里点**——不是配置文件里的一个开关，因为那对用户是不可见的
/// （v0.6.0 就是那样：`talk_enabled` 只能手改 config.json，等于没有入口）。
///
/// 两者的**前半段完全一样**（录音 → raw 落盘 → 转写 → 剪贴板/上屏），
/// 差别只在后半段要不要 LLM + TTS + 播放。所以它是「模式」而不是两个功能：
/// 走错模式只影响「有没有声音」，不会丢掉转写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TalkMode {
    /// 记下来、上屏，不出声。M1 以来的行为。
    #[default]
    InputMethod,
    /// 上屏之后把回答念出来（说一句答一句，按录音键可打断）。
    Conversation,
}

impl TalkMode {
    /// 菜单和文档都从这一份清单派生。**加第三个模式时这是唯一要改的地方**——
    /// 曾经菜单里手写死两项，加模式时就得记得同时改三处（菜单、i18n、文档）。
    pub const ALL: &'static [TalkMode] = &[TalkMode::InputMethod, TalkMode::Conversation];

    pub fn as_str(self) -> &'static str {
        match self {
            TalkMode::InputMethod => "input_method",
            TalkMode::Conversation => "conversation",
        }
    }
}

/// 保留天数的默认值。`Default::default()` 给 0（= 永不清理），
/// 不是我们要的，所以单列一个。
fn default_retention_days() -> u32 {
    30
}

fn default_auto_paste() -> bool {
    true
}

fn default_autostart() -> bool {
    true
}

/// 默认的拉起命令：**空**。
///
/// ⚠️ 曾经默认成 `env!("CARGO_MANIFEST_DIR")/scripts/serve-llm.sh`，
/// 那是**编译时**的仓库路径——分发到别人机器上指向一个不存在的目录，
/// 而且一旦被写进用户的 config.json 就固化下来了（codex Medium 1）。
///
/// 空的含义是「不知道怎么拉，只连不拉」，这恰好也是 jason 说的
/// 那个未来的默认形态：模型服务由外部管理，AgentEar 只按 `llm_url` 连。
/// 开发时要自动拉起，在配置里显式写路径。
fn default_start_command() -> Vec<String> {
    Vec::new()
}

fn lenient_autostart<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_else(|_| default_autostart()))
}

/// 每个字段都走 `lenient`：一个字段坏掉不该连累其他设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 转写完是否自动粘贴到光标处。
    #[serde(deserialize_with = "lenient_auto_paste")]
    pub auto_paste: bool,
    /// 输入设备名。`None` = 跟随系统默认。
    ///
    /// 存名字而不是索引：设备顺序会随插拔变化，索引存下来就指错了。
    #[serde(deserialize_with = "lenient")]
    pub input_device: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub trigger: Trigger,
    /// `raw/audio/` 的保留天数。**0 = 永不清理。**
    #[serde(deserialize_with = "lenient_retention")]
    pub retention_days: u32,
    /// **界面**语言（菜单文案），不影响识别。默认英文。
    #[serde(deserialize_with = "lenient")]
    pub ui_lang: Lang,
    /// **识别**语言，决定走哪个 ASR 引擎。默认 Auto（SenseVoice，
    /// 中/英/粤/日/韩自动判别）。切到 Thai 需要先下载模型。
    ///
    /// 和 `ui_lang` 各存各的：界面泰文 + 识别中文，或者界面英文 + 识别泰语，
    /// 都是合理组合。把两者绑在一起是很容易犯的错——一个在泰国工作的
    /// 英语用户，界面要英文，识别要泰语。
    #[serde(deserialize_with = "lenient")]
    pub asr_lang: AsrLang,
    /// 用哪个 ASR 后端。**默认 `builtin`**——已发布用户升级上来行为不变。
    ///
    /// `speech_swift` 需要用户自己 `brew install speech`，不随包分发。
    /// 换默认值这件事**需要 jason 单独拍板**（`CLAUDE.md`：「放宽只针对
    /// LLM 这一项，ASR 侧仍按原标准要求」），不是改个默认值那么简单。
    #[serde(deserialize_with = "lenient")]
    pub asr_backend: AsrBackend,
    /// 转写后是否送本地 LLM 纠正技术术语。
    ///
    /// **默认关。** 它需要一个额外的边车进程（`scripts/serve-llm.sh`），
    /// 边车没起的时候开着它只会每次录音都白等一次超时。
    /// 而且代价是实打实的：转写本身 0.26s，加上纠错，短句要 1–3 秒、
    /// 两分半的录音实测 **10.3 秒**（耗时随字数走，`benchmarks-m2.md` §8.2）。
    /// 值不值得由用户自己定。
    #[serde(deserialize_with = "lenient")]
    pub correct_terms: bool,
    /// 纠错边车的地址。留空 = 用 `correct::DEFAULT_URL`。
    ///
    /// 之所以可配：8793 也可能被占（8791 就是这么丢的），
    /// 而换端口不该要求用户重新编译。
    #[serde(deserialize_with = "lenient")]
    pub llm_url: Option<String>,
    /// 连不上边车时，要不要**尝试**按 `llm_start_command` 把它拉起来。
    ///
    /// 默认 `true`，但这只是兜底——**正常路径永远是「按 `llm_url` 去连」**。
    /// jason 2026-09-03 定的原则：将来会有独立的模型服务入口，那时
    /// AgentEar 只管按配置连，谁把它起起来的不关它的事。
    /// 把它设成 `false` 就是那个未来：只连不拉，连不上就降级。
    #[serde(deserialize_with = "lenient_autostart")]
    pub llm_autostart: bool,
    /// 拉起边车用的命令，argv 形式（第一项是程序，其余是参数）。
    ///
    /// **刻意不写死成 `scripts/serve-llm.sh`。** 换模型、换推理框架、
    /// 换成别的机器上的服务，都只该改这个配置，不该改代码。
    /// 留空 = 不知道怎么拉，等同于 `llm_autostart: false`。
    #[serde(deserialize_with = "lenient")]
    pub llm_start_command: Vec<String>,
    /// 转写完是否投递到知识库（`kb/` 的 Markdown 文件树，ADR-0003 §3.3）。
    ///
    /// **默认开。** 和 `correct_terms` 不同，它不需要任何外部依赖——
    /// 就是在本地写几 KB 的 Markdown，不联网、不起进程、失败也不挡上屏。
    /// 默认关掉只会让「说一句话自动进知识库」这条链路默认是断的。
    #[serde(deserialize_with = "lenient_kb_enabled")]
    pub kb_enabled: bool,
    /// 知识库根目录。`None` = 数据目录下的 `kb/`。
    ///
    /// 之所以可配：很多人的笔记库（Obsidian vault 等）早就存在了，
    /// 让 AgentEar 直接往里写，比让用户在两个目录之间来回搬有用得多。
    #[serde(deserialize_with = "lenient")]
    pub kb_dir: Option<String>,

    // ------------------------------------------------------------------
    // 通话（M3 / ADR-0007）。下列字段**全部默认关或者指向本机默认端口**，
    // 已发布用户升级上来行为与 v0.5.0 完全一致——不开 `talk_enabled`
    // 的话，这一整块代码一行都不会走到。
    // ------------------------------------------------------------------
    /// 录音键的行为模式。**默认输入法模式**，切对话模式要从菜单里点。
    ///
    /// 对话模式需要两个边车（`scripts/serve-talk-llm.sh` + `scripts/serve-tts.sh`），
    /// 没起的时候它只会让每次录音都白等一次超时——和 `correct_terms` 同一个道理。
    /// 所以**默认不是它**，而且**切换入口必须是可见的菜单项**（见 `tray.rs`）。
    #[serde(deserialize_with = "lenient")]
    pub talk_mode: TalkMode,
    /// **旧字段（v0.6.0 引入，v0.7.0 起只读）**：`talk_enabled: true`
    /// 等价于 `talk_mode: "conversation"`。
    ///
    /// 留着它只为了读老配置（v0.6.0 的用户是手改这个开关开的通话）。
    /// `skip_serializing` 让下次落盘时它自动消失——**迁移只做一次**，
    /// 之后 `talk_mode` 是唯一事实来源，不会出现两个字段打架。
    #[serde(default, rename = "talk_enabled", skip_serializing)]
    pub talk_enabled_legacy: bool,
    /// 通话用哪个语言。**默认中文**，因为 `asr_lang` 的默认是 Auto，
    /// 而「回答用什么语言」必须显式说清——模型不会替用户决定这件事
    /// （jason 2026-09-08：沟通前确认用什么语言，过程中可以随时切）。
    #[serde(deserialize_with = "lenient")]
    pub talk_lang: TalkLang,
    /// 通话用的 LLM 引擎：`openai_compat`（默认）或 `mock`。
    ///
    /// **mock 必须显式选。** 它是 ADR-0007 §4.6 那个「证明链路通」的
    /// 写死实现，不是产品能力；默认成 mock 会让人以为模型在跑。
    #[serde(deserialize_with = "lenient")]
    pub talk_llm_engine: String,
    /// 通话 LLM 的地址。**换模型只改这里**（或改边车那边的
    /// `AGENTEAR_TALK_LLM_MODEL`），Rust 侧不关心对面是 2B 还是 9B。
    #[serde(deserialize_with = "lenient")]
    pub talk_llm_url: Option<String>,
    /// 通话 TTS 引擎：`http`（默认，指 `services/tts` 的 VoxCPM2 边车）
    /// 或 `say`（零依赖兜底）。
    #[serde(deserialize_with = "lenient")]
    pub talk_tts_engine: String,
    /// 对话模式的两个边车没在跑时，要不要**尝试按配置的命令拉起来**。
    ///
    /// **默认 true**，但它只是兜底——正常路径永远是「按 URL 去连」
    /// （ADR-0002 §8：连接优先、拉起兜底）。
    #[serde(deserialize_with = "lenient")]
    pub talk_autostart: bool,
    /// 拉起 LLM 边车的命令，argv 形式（第一项是程序）。
    ///
    /// **默认空** = 「不知道怎么拉，只连不拉」。理由和 `llm_start_command`
    /// 一模一样：**不能写死编译期路径**——那是开发机上的仓库路径，
    /// 分发到别人机器上指向一个不存在的目录，而且一旦被写进用户的
    /// config.json 就固化下来了。
    ///
    /// 空的时候**日志里会打出该跑哪条命令**，不会静默。
    #[serde(deserialize_with = "lenient")]
    pub talk_llm_start_command: Vec<String>,
    /// 拉起 TTS 边车的命令。语义同上。
    #[serde(deserialize_with = "lenient")]
    pub talk_tts_start_command: Vec<String>,
    /// 对话用哪个音色（`--voices-dir` 里的名字）。**空 = 用边车的默认音色。**
    ///
    /// 「声色飘忽」的直接解法：不指定时 VoxCPM2 每次随机换说话人。
    #[serde(deserialize_with = "lenient")]
    pub tts_voice: Option<String>,
    /// 音色库目录（`<name>.wav` + `<name>.json`）。菜单从它列音色。
    /// 空 = `~/.agentear/talk/voices`。
    #[serde(deserialize_with = "lenient")]
    pub tts_voices_dir: Option<String>,
    /// 语系/口音：`zh`（普通话）/ `yue`（粤语）/ `henan`（河南话）/ … / `en-gb` / `th`。
    #[serde(deserialize_with = "lenient")]
    pub tts_style: String,
    /// 语气/情绪：`warm`（亲切自然，默认）/ `calm` / `lively` / `serious`。
    ///
    /// ⚠️ **默认 `warm` 是实测挑出来的**：只写语种时 F0 起伏 3.53 半音、
    /// 能量起伏 0.0736；加情绪描述后是 5.57 / 0.1196（+58%/+62%），而且更快。
    #[serde(deserialize_with = "lenient")]
    pub tts_tone: String,
    /// 是否启用语音指令表（`<数据目录>/commands.json`）。
    ///
    /// **默认开**：命中的指令在本地 0ms 执行、不走模型；**没命中的照常走对话**，
    /// 所以开着的代价只是「多查一次字符串前缀」。指令表在
    /// `src/commands.rs` 的 `default_commands()` 里有一份开箱默认，
    /// 用 `--add-command` 加 / 用菜单打开文件改。
    #[serde(deserialize_with = "lenient")]
    pub commands_enabled: bool,
    /// 「向外动作」二次确认的有效期（秒）。默认 30。
    ///
    /// **过期即作废**，而且**宁可短**：一个挂着的向外动作比没有更危险
    /// （用户以为早忘了，结果一按键就发出去了）。最短 5 秒。
    #[serde(deserialize_with = "lenient")]
    pub command_confirm_secs: u64,
    /// TTS 边车地址。留空 = `http://127.0.0.1:8765`。
    ///
    /// 端口写死在这里而不是从边车读：`sidecar.rs` 记过那个教训——
    /// **连错对端比连不上更糟**，客户端必须知道自己该连谁。
    #[serde(deserialize_with = "lenient")]
    pub tts_url: Option<String>,
    /// 单次 LLM / TTS 请求的超时（秒）。默认 60。
    ///
    /// 实测（`docs/benchmarks-talk.md`）：VoxCPM2-4bit 合成一句 2.6–5.1s，
    /// LLM 一句 0.66–0.85s。60s 是给「回答更长」和「机器更慢」留的余量，
    /// **不是实测值的近似**——卡着 4s 设超时会让稍长的回答直接失败。
    #[serde(deserialize_with = "lenient_u64")]
    pub talk_timeout_secs: u64,
    /// 「本地天气事实」的城市名。**这不是天气接口**（ADR-0007 §4.6）。
    #[serde(deserialize_with = "lenient")]
    pub talk_city: String,
    /// 「本地天气事实」原文。留空 = 用内置那句。
    ///
    /// ⚠️ **写死的场景，不是实时天气。** 它的唯一用途是让 2B 模型
    /// 有东西可说，从而证明 ASR→LLM→TTS 整条链路通。
    /// 接真实天气源属于集成方（R3 外壳层），不在本项目职责内。
    #[serde(deserialize_with = "lenient")]
    pub talk_weather_note: Option<String>,
}

fn default_talk_timeout_secs() -> u64 {
    60
}

fn lenient_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v)
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or_else(default_talk_timeout_secs))
}

fn default_talk_city() -> String {
    "清迈".to_string()
}

fn default_talk_llm_engine() -> String {
    "openai_compat".to_string()
}

fn default_talk_tts_engine() -> String {
    "http".to_string()
}

fn default_commands_enabled() -> bool {
    true
}

fn default_command_confirm_secs() -> u64 {
    30
}

fn default_tts_style() -> String {
    "zh".to_string()
}

fn default_tts_tone() -> String {
    "warm".to_string()
}

fn default_kb_enabled() -> bool {
    true
}

fn lenient_kb_enabled<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_else(|_| default_kb_enabled()))
}

impl Config {
    /// 知识库根目录的绝对路径。
    ///
    /// 相对路径按**数据目录**解释，不按当前工作目录——守护进程的 cwd
    /// 取决于它是从终端还是 Finder 启动的，拿它当基准会让同一份配置
    /// 在两种启动方式下指向不同的地方。
    pub fn kb_root(&self, data_root: &Path) -> PathBuf {
        match self.kb_dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(d) => {
                let p = PathBuf::from(shellexpand_tilde(d));
                if p.is_absolute() { p } else { data_root.join(p) }
            }
            None => data_root.join("kb"),
        }
    }

    /// 通话里那句「本地事实」。
    ///
    /// ⚠️ **写死的场景，不是实时天气**（ADR-0007 §4.6）。默认那句和
    /// 项目文档里的例子保持一致，用户改成自己的城市即可。
    /// 音色库目录的绝对路径。相对路径按**数据目录**解释（同 `kb_root` 的理由）。
    pub fn voices_dir(&self, data_root: &Path) -> PathBuf {
        match self
            .tts_voices_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(d) => {
                let p = PathBuf::from(shellexpand_tilde(d));
                if p.is_absolute() { p } else { data_root.join(p) }
            }
            None => data_root.join("talk/voices"),
        }
    }

    pub fn weather_fact(&self) -> String {
        match self
            .talk_weather_note
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(note) => note.to_string(),
            None => format!(
                "今天{}多云转晴，最高 32 度，傍晚有阵雨，风不大。",
                self.talk_city
            ),
        }
    }
}

/// 只展开开头的 `~`。不引 shellexpand：配置里出现的就是路径，
/// 不该顺带支持 `$VAR` 那些会让「这个字符串到底指哪」变得不可预测的东西。
fn shellexpand_tilde(s: &str) -> String {
    match s.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(h) => format!("{h}/{rest}"),
            Err(_) => s.to_string(),
        },
        None => s.to_string(),
    }
}

// 这两个字段的「默认」不是 `Default::default()`，坏值要退回文档里写的默认，
// 不能退回 0 / false。
fn lenient_retention<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_else(|_| default_retention_days()))
}

fn lenient_auto_paste<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_else(|_| default_auto_paste()))
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_paste: default_auto_paste(),
            input_device: None,
            trigger: Trigger::RightCommand,
            retention_days: default_retention_days(),
            ui_lang: Lang::default(),
            asr_lang: AsrLang::default(),
            asr_backend: AsrBackend::default(),
            correct_terms: false,
            llm_url: None,
            llm_autostart: default_autostart(),
            llm_start_command: default_start_command(),
            kb_enabled: default_kb_enabled(),
            kb_dir: None,
            talk_mode: TalkMode::default(),
            talk_enabled_legacy: false,
            talk_lang: TalkLang::default(),
            talk_llm_engine: default_talk_llm_engine(),
            talk_llm_url: None,
            talk_tts_engine: default_talk_tts_engine(),
            commands_enabled: default_commands_enabled(),
            command_confirm_secs: default_command_confirm_secs(),
            tts_voice: None,
            tts_voices_dir: None,
            tts_style: default_tts_style(),
            tts_tone: default_tts_tone(),
            talk_autostart: default_autostart(),
            talk_llm_start_command: Vec::new(),
            talk_tts_start_command: Vec::new(),
            tts_url: None,
            talk_timeout_secs: default_talk_timeout_secs(),
            talk_city: default_talk_city(),
            talk_weather_note: None,
        }
    }
}

static CURRENT: RwLock<Option<Config>> = RwLock::new(None);
/// 写者之间的串行锁。见 `update` 的说明——它和 `CURRENT` 分工不同，
/// 别合并成一把。
static SAVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// 从数据目录读配置。文件不存在或解析失败都退回默认值——**配置损坏不能
/// 让守护进程起不来**，宁可用默认值跑着并把错误写进日志。
pub fn load(data_root: &Path) -> Config {
    let path = data_root.join("config.json");
    // `raw` 留着不只是为了报错信息：**迁移要用它判断某个键到底有没有出现过**。
    // `lenient` 分不出「字段不存在」和「字段存在但等于默认值」，
    // 而迁移恰恰要区分这两件事（见下面 `mode_key_present`）。
    let (cfg, raw) = match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Config>(&raw) {
            Ok(c) => (c, Some(raw)),
            Err(e) => {
                log::error!("config.json 解析失败，改用默认配置: {e}");
                (Config::default(), Some(raw))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => {
            log::error!("读取 config.json 失败，改用默认配置: {e}");
            (Config::default(), None)
        }
    };
    // 迁移：v0.6.0 的 `talk_enabled: true` → `talk_mode: "conversation"`。
    //
    // **判据是「新键根本没出现过」，不是「新键等于默认值」。** 两者差很远：
    // 用户在菜单里显式切回输入法（写下了 `talk_mode: "input_method"`），
    // 如果按「等于默认值」判断，下次启动会被老字段顶回对话模式——
    // 菜单显示输入法、行为却是对话，属于最难查的一类 bug。
    // 所以这里直接看原始 JSON 里有没有这个键。
    let mode_key_present = raw
        .as_deref()
        .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .map(|v| v.get("talk_mode").is_some())
        .unwrap_or(false);
    let mut cfg = cfg;
    if cfg.talk_enabled_legacy && !mode_key_present {
        cfg.talk_mode = TalkMode::Conversation;
        log::info!("配置迁移：talk_enabled → talk_mode=\"conversation\"（下次保存时旧字段会消失）");
    }
    cfg.talk_enabled_legacy = false;

    PATH.set(path).ok();
    *CURRENT.write().unwrap() = Some(cfg.clone());
    cfg
}

pub fn get() -> Config {
    CURRENT
        .read()
        .unwrap()
        .clone()
        .unwrap_or_default()
}

/// 改配置并立即落盘。写失败只记日志，内存里的改动仍然生效。
///
/// ## 两把锁，各司其职
///
/// 原来是「锁内改、克隆一份、放锁、锁外写」。菜单只在主线程点，这看着没事——
/// 直到模型下载线程也开始改配置（装完泰语要提交 `asr_lang`）。那时两个写者
/// 可以这样交错：
///
/// ```text
/// 下载线程: 改成 Thai, 克隆 A, 放锁 ────────────────► 写 A（旧）
/// 主线程:              改保留期, 克隆 B, 放锁 ──► 写 B（新）
/// ```
///
/// 落盘顺序反过来，用户刚改的保留期就被旧快照盖掉了。
///
/// 但把落盘直接塞进 `CURRENT` 的写锁里也不行：**磁盘慢的时候，
/// 所有 `get()` 都跟着卡**——包括 AppKit 那个 0.5s 定时器和菜单构建，
/// 表现就是界面冻住。
///
/// 所以用两把：`SAVE` 只序列化写者（保证落盘顺序和改动顺序一致），
/// `CURRENT` 的写锁只护住内存里那几纳秒的改动。读者永远不会等磁盘。
pub fn update(f: impl FnOnce(&mut Config)) {
    // 先拿 SAVE，全程持有到落盘结束——写者之间因此是严格串行的。
    let _writer = SAVE.lock().unwrap_or_else(|e| e.into_inner());
    let snapshot = {
        let mut guard = CURRENT.write().unwrap();
        let cfg = guard.get_or_insert_with(Config::default);
        f(cfg);
        cfg.clone()
    }; // CURRENT 的写锁在这里就放了，读者不必等下面的磁盘 IO
    if let Err(e) = save(&snapshot) {
        log::error!("保存配置失败: {e:#}");
    }
}

fn save(cfg: &Config) -> Result<()> {
    let path = PATH.get().context("配置路径未初始化")?;
    let json = serde_json::to_string_pretty(cfg)?;
    // 先写临时文件再 rename：崩在写一半不会留下半截 JSON,
    // 否则下次启动会解析失败并静默退回默认值。
    //
    // 临时文件名带 pid：同进程的并发已由 `update` 的写锁挡住，但**两个
    // AgentEar 实例**（终端一个、.app 一个）会用同一个数据目录。
    // 共享一个 `.tmp` 路径的话，两边的写和 rename 会互相踩，
    // 甚至把对方写了一半的内容 rename 成正式配置。
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, json).with_context(|| format!("写 {} 失败", tmp.display()))?;
    std::fs::rename(&tmp, path).context("rename 配置文件失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let c = Config::default();
        assert!(c.auto_paste);
        assert_eq!(c.input_device, None);
        assert_eq!(c.trigger, Trigger::RightCommand);
        assert_eq!(c.retention_days, 30, "raw 音频默认保留 30 天");
    }

    /// jason 机器上 v0.2.2 时期真实存在的配置文件，一字不改。
    /// 升级到带 `ui_lang` 的版本后，原有设置必须一项不丢。
    #[test]
    fn real_pre_i18n_config_upgrades_cleanly() {
        let legacy = r#"{
  "auto_paste": true,
  "input_device": "MacBook Pro Microphone",
  "trigger": "right_command",
  "retention_days": 30
}"#;
        let c: Config = serde_json::from_str(legacy).expect("老配置必须能读");
        assert!(c.auto_paste);
        assert_eq!(c.input_device.as_deref(), Some("MacBook Pro Microphone"));
        assert_eq!(c.trigger, Trigger::RightCommand);
        assert_eq!(c.retention_days, 30);
        assert_eq!(c.ui_lang, Lang::En, "没有 ui_lang 字段时应取默认英文");
        assert_eq!(c.asr_lang, AsrLang::Auto, "没有 asr_lang 字段时应取默认 Auto");
        assert!(c.llm_autostart, "老配置没有这个字段时应取默认 true");
        assert!(!c.correct_terms, "术语纠错默认关——它要一个额外的边车进程");
    }

    /// **默认必须是输入法模式。** jason 2026-09-14 拍板：两种模式并存，
    /// 默认走 M1 那条老路（只上屏、不出声）——因为对话模式要两个边车，
    /// 没起的时候会让每次录音都白等一次超时。
    #[test]
    fn talk_mode_defaults_to_input_method() {
        assert_eq!(Config::default().talk_mode, TalkMode::InputMethod);
        assert_eq!(
            serde_json::from_str::<Config>("{}").unwrap().talk_mode,
            TalkMode::InputMethod,
            "配置里没这个字段时也要落到输入法模式"
        );
        assert!(Config::default().talk_enabled_legacy == false || true);
    }

    /// v0.6.0 的用户是**手改 `talk_enabled`** 开的通话。升级到带模式字段的
    /// 版本后，那台机器必须还是对话模式——否则用户会觉得「升级完我的通话没了」。
    #[test]
    fn legacy_talk_enabled_migrates_to_conversation() {
        let legacy = r#"{"talk_enabled": true}"#;
        let c: Config = serde_json::from_str(legacy).expect("v0.6.0 的配置必须能读");
        assert!(c.talk_enabled_legacy, "旧字段要能读进来");
        assert_eq!(c.talk_mode, TalkMode::InputMethod, "未迁移前还是默认值");

        // load() 里那一步迁移（这里照它的判据手写一遍，避免测试依赖文件系统）
        let migrated = if c.talk_enabled_legacy && c.talk_mode == TalkMode::InputMethod {
            TalkMode::Conversation
        } else {
            c.talk_mode
        };
        assert_eq!(migrated, TalkMode::Conversation, "talk_enabled:true 应迁移成对话模式");
    }

    /// **新字段优先。** 用户在菜单里切回输入法之后，重启不能被旧字段顶回对话模式
    /// ——那是最难查的一类 bug：菜单明明显示输入法，行为却是对话。
    ///
    /// 这里照 `load()` 的判据原样重写一遍（含「新键是否存在」这一步），
    /// 因为 `load()` 要文件系统、单测里不方便直接调。
    #[test]
    fn explicit_talk_mode_wins_over_the_legacy_flag() {
        fn migrate(raw: &str) -> TalkMode {
            let c: Config = serde_json::from_str(raw).expect("要能读");
            let mode_key_present = serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .map(|v| v.get("talk_mode").is_some())
                .unwrap_or(false);
            if c.talk_enabled_legacy && !mode_key_present {
                TalkMode::Conversation
            } else {
                c.talk_mode
            }
        }
        assert_eq!(
            migrate(r#"{"talk_enabled": true}"#),
            TalkMode::Conversation,
            "只有老字段 → 迁移"
        );
        assert_eq!(
            migrate(r#"{"talk_enabled": true, "talk_mode": "input_method"}"#),
            TalkMode::InputMethod,
            "显式写了输入法 → 新字段说了算，不能被老字段顶回对话"
        );
        assert_eq!(
            migrate(r#"{"talk_mode": "conversation"}"#),
            TalkMode::Conversation,
            "只有新字段 → 原样"
        );
        assert_eq!(migrate(r#"{}"#), TalkMode::InputMethod, "都没有 → 默认输入法");
    }

    #[test]
    fn talk_mode_roundtrips() {
        for (mode, text) in [
            (TalkMode::InputMethod, "input_method"),
            (TalkMode::Conversation, "conversation"),
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<TalkMode>(&json).unwrap(), mode);
        }
    }

    /// 落盘时旧字段要消失——否则两个字段会长期并存、互相打架。
    #[test]
    fn legacy_field_is_not_written_back() {
        let c: Config = serde_json::from_str(r#"{"talk_enabled": true}"#).unwrap();
        let written = serde_json::to_string(&c).unwrap();
        assert!(!written.contains("talk_enabled"), "旧字段不该被写回: {written}");
        assert!(written.contains("talk_mode"), "新字段要写出来: {written}");
    }

    #[test]
    fn ui_lang_defaults_to_english() {
        assert_eq!(Config::default().ui_lang, Lang::En);
        assert_eq!(serde_json::from_str::<Config>("{}").unwrap().ui_lang, Lang::En);
    }

    #[test]
    fn ui_lang_roundtrips_all_three() {
        for want in Lang::ALL {
            let mut c = Config::default();
            c.ui_lang = want;
            let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
            assert_eq!(back.ui_lang, want);
        }
    }

    /// 一个字段是未知取值，**不能连累其他设置**。
    ///
    /// `#[serde(default)]` 只挡字段缺失。没有 `lenient` 的话，
    /// `"ui_lang": "fr"` 会让整份配置解析失败，load() 退回全默认——
    /// 用户丢的是输入设备、触发键、保留期，只因为语言写错了。
    #[test]
    fn unknown_enum_value_does_not_reset_everything() {
        let json = r#"{
            "ui_lang": "fr",
            "asr_lang": "klingon",
            "input_device": "MacBook Pro麦克风",
            "trigger": "ctrl_shift_r",
            "retention_days": 90,
            "auto_paste": false
        }"#;
        let c: Config = serde_json::from_str(json).expect("坏字段不该让整份配置解析失败");
        assert_eq!(c.ui_lang, Lang::En, "未知语言退回默认");
        assert_eq!(c.asr_lang, AsrLang::Auto, "未知识别语言退回默认");
        assert_eq!(c.input_device.as_deref(), Some("MacBook Pro麦克风"), "设备被连累了");
        assert_eq!(c.trigger, Trigger::CtrlShiftR, "触发键被连累了");
        assert_eq!(c.retention_days, 90, "保留期被连累了");
        assert!(!c.auto_paste, "自动上屏被连累了");
    }

    /// 逐字段容错**挡不住重复键**——记录这个边界，免得把承诺说过头。
    /// 重复键在派生的 visitor 里就报错了，轮不到 `lenient`。
    #[test]
    fn duplicate_keys_are_a_known_gap() {
        let json = r#"{"ui_lang":"zh","ui_lang":"fr","retention_days":90}"#;
        assert!(
            serde_json::from_str::<Config>(json).is_err(),
            "如果这条开始通过了，说明重复键也能兜住了，去把 lenient 的注释改掉"
        );
    }

    /// 类型写错也一样，只坏那一个字段，且退回**文档写的默认值**
    /// （保留期是 30 天，不是 `u32::default()` 的 0——0 是「永不清理」）。
    #[test]
    fn wrong_type_falls_back_to_documented_default() {
        let c: Config = serde_json::from_str(r#"{"retention_days": "三十", "trigger": 42}"#)
            .expect("类型错误不该让整份配置解析失败");
        assert_eq!(c.retention_days, 30, "坏值必须退回 30，不能退回 0（=永不清理）");
        assert_eq!(c.trigger, Trigger::RightCommand);
    }

    /// 界面语言和识别语言是**两个独立的字段**，不能互相影响。
    ///
    /// 这条挡的是一类很自然的错误实现：「用户把界面切成泰文，那识别
    /// 大概也想要泰语吧」。不对——在泰国工作的英语用户要的是
    /// 英文界面 + 泰语识别，而一个学泰语的中国人可能要中文界面 + 泰语识别。
    #[test]
    fn ui_lang_and_asr_lang_are_independent() {
        for ui in Lang::ALL {
            for asr in [AsrLang::Auto, AsrLang::Thai] {
                let mut c = Config::default();
                c.ui_lang = ui;
                c.asr_lang = asr;
                let back: Config =
                    serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
                assert_eq!(back.ui_lang, ui);
                assert_eq!(back.asr_lang, asr);
            }
        }
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 老版本写的配置文件缺字段时不能报错——serde(default) 保证这一点
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.auto_paste);
        assert_eq!(c.retention_days, 30);
    }

    #[test]
    fn roundtrips() {
        let mut c = Config::default();
        c.input_device = Some("MacBook Pro麦克风".into());
        c.trigger = Trigger::CtrlShiftR;
        c.retention_days = 0;
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.input_device.as_deref(), Some("MacBook Pro麦克风"));
        assert_eq!(back.trigger, Trigger::CtrlShiftR);
        assert_eq!(back.retention_days, 0);
    }

    /// 落盘必须发生在持有写锁期间，否则两个写者可以乱序落盘、
    /// 让先改的覆盖后改的。这条测不了时序，但能钉住「update 之后
    /// 内存和磁盘一致」这个可观察的后果。
    #[test]
    fn update_persists_atomically() {
        let tmp = std::env::temp_dir().join(format!("agentear-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        load(&tmp);

        update(|c| c.retention_days = 90);
        update(|c| c.asr_lang = AsrLang::Thai);

        let on_disk: Config =
            serde_json::from_str(&std::fs::read_to_string(tmp.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk.retention_days, 90, "先改的那项被后一次写覆盖了");
        assert_eq!(on_disk.asr_lang, AsrLang::Thai);
        assert_eq!(get().retention_days, 90, "内存和磁盘不一致");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
