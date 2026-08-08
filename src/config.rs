//! 持久化配置：`~/.agentear/config.json`。
//!
//! 菜单栏改设置后立即落盘，下次启动生效。三项里只有触发键需要重启进程
//! （`CGEventTap` 挂在一个跑 `CFRunLoop` 的线程上，运行时换不掉），
//! 输入设备和自动上屏都是下一次录音/下一次上屏时读取，无需重启。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 转写完是否自动粘贴到光标处。
    pub auto_paste: bool,
    /// 输入设备名。`None` = 跟随系统默认。
    ///
    /// 存名字而不是索引：设备顺序会随插拔变化，索引存下来就指错了。
    pub input_device: Option<String>,
    pub trigger: Trigger,
    /// `raw/audio/` 的保留天数。**0 = 永不清理。**
    pub retention_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_paste: true,
            input_device: None,
            trigger: Trigger::RightCommand,
            retention_days: 30,
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
