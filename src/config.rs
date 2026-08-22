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

use crate::i18n::Lang;

/// 单个字段解析失败时退回默认值，**而不是让整份配置解析失败**。
///
/// `#[serde(default)]` 只挡字段缺失，挡不住取值非法：配置里出现
/// `"ui_lang": "fr"` 或 `"retention_days": "三十"`，整个 `Config` 就
/// 反序列化失败，而 `load()` 的兜底是「退回默认配置」——用户丢掉的是
/// **输入设备、触发键、保留期全部**，只因为一个字段坏了。
///
/// 先解析成 `Value` 再逐字段尝试，把损坏隔离在字段这一层。
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
        }
    }
}

static CURRENT: RwLock<Option<Config>> = RwLock::new(None);
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
pub fn update(f: impl FnOnce(&mut Config)) {
    let cfg = {
        let mut guard = CURRENT.write().unwrap();
        let cfg = guard.get_or_insert_with(Config::default);
        f(cfg);
        cfg.clone()
    };
    if let Err(e) = save(&cfg) {
        log::error!("保存配置失败: {e:#}");
    }
}

fn save(cfg: &Config) -> Result<()> {
    let path = PATH.get().context("配置路径未初始化")?;
    let json = serde_json::to_string_pretty(cfg)?;
    // 先写临时文件再 rename：崩在写一半不会留下半截 JSON,
    // 否则下次启动会解析失败并静默退回默认值
    let tmp = path.with_extension("json.tmp");
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
            "input_device": "MacBook Pro麦克风",
            "trigger": "ctrl_shift_r",
            "retention_days": 90,
            "auto_paste": false
        }"#;
        let c: Config = serde_json::from_str(json).expect("坏字段不该让整份配置解析失败");
        assert_eq!(c.ui_lang, Lang::En, "未知语言退回默认");
        assert_eq!(c.input_device.as_deref(), Some("MacBook Pro麦克风"), "设备被连累了");
        assert_eq!(c.trigger, Trigger::CtrlShiftR, "触发键被连累了");
        assert_eq!(c.retention_days, 90, "保留期被连累了");
        assert!(!c.auto_paste, "自动上屏被连累了");
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
}
