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
use crate::i18n::Lang;

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

/// 保留天数的默认值。`Default::default()` 给 0（= 永不清理），
/// 不是我们要的，所以单列一个。
fn default_retention_days() -> u32 {
    30
}

fn default_auto_paste() -> bool {
    true
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
            correct_terms: false,
            llm_url: None,
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
    let cfg = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<Config>(&s) {
            Ok(c) => c,
            Err(e) => {
                log::error!("config.json 解析失败，改用默认配置: {e}");
                Config::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            log::error!("读取 config.json 失败，改用默认配置: {e}");
            Config::default()
        }
    };
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
        assert!(!c.correct_terms, "术语纠错默认关——它要一个额外的边车进程");
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
