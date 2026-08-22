//! 界面语言。**只管菜单上显示什么字，不影响识别行为。**
//!
//! 和「识别语言」是两回事：界面切成泰文不会改变 ASR 走哪个模型，
//! 识别切成泰语也不会改变菜单文字。
//!
//! ## 为什么 `t()` 要显式传 `lang`，而不是读一个全局「当前语言」
//!
//! 全局单例在这里没有必要，还会引出线程安全问题：录音状态在工作线程里变，
//! 菜单和标题在主线程里画。而**只有主线程需要翻译**——工作线程只更新原子
//! 状态位，从不碰文案。显式传参把这件事写进了类型里，零隐式状态。
//!
//! ## 漏翻译为什么是编译错误
//!
//! `t()` 对 `Key` 做穷尽匹配且**不写 `_` 通配分支**，每个分支又必须给
//! `pick()` 三个语言的字符串。所以加一个 `Key` 而不给全三种语言，
//! 编译不过。
//!
//! 编译器管得了「有没有」，管不了「对不对」——泰文栏里误粘英文、占位符
//! 对不上、菜单被撑太宽，这些只能靠 `tests` 里的快照和人工过一遍。
//! **泰文文案尚未经母语者校对**，见 `docs/plan-i18n-thai.md` §6。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    /// 默认。AgentEar 面向的不只是中文用户，英文是最安全的起点。
    #[default]
    En,
    Zh,
    Th,
}

impl Lang {
    /// 语言自己的名字，**永远用该语言书写**——菜单里列出选项时，
    /// 「中文」不应该在英文界面下显示成 "Chinese"，否则找不着。
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
            Lang::Th => "ไทย",
        }
    }

    pub const ALL: [Lang; 3] = [Lang::En, Lang::Zh, Lang::Th];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    StartRecording,
    Transcribing,
    /// 菜单栏标题里的「转写中」。**要比菜单里那条短**——菜单栏寸土寸金。
    TitleTranscribing,
    TriggerSection,
    TriggerRightCommand,
    TriggerRightCommandNoPerm,
    TriggerCtrlShiftR,
    InputDeviceSection,
    AutoPaste,
    RetentionSection,
    Retention7,
    Retention30,
    Retention90,
    RetentionNever,
    LanguageSection,
    OpenDataDir,
    ViewLog,
    Quit,
}

/// 三种语言的文案。参数顺序固定 `(en, zh, th)`。
fn pick(lang: Lang, en: &'static str, zh: &'static str, th: &'static str) -> &'static str {
    match lang {
        Lang::En => en,
        Lang::Zh => zh,
        Lang::Th => th,
    }
}

pub fn t(lang: Lang, key: Key) -> &'static str {
    use Key as K;
    match key {
        K::StartRecording => pick(lang, "● Start Recording", "● 开始录音", "● เริ่มบันทึกเสียง"),
        K::Transcribing => pick(lang, "◌ Transcribing…", "◌ 转写中……", "◌ กำลังถอดความ…"),
        K::TitleTranscribing => pick(lang, "◌ ASR", "◌ 转写中", "◌ ถอดความ"),
        K::TriggerSection => pick(lang, "Trigger Key", "触发键", "ปุ่มลัด"),
        K::TriggerRightCommand => pick(
            lang,
            "Tap Right Command",
            "轻点右 Command",
            "แตะปุ่ม Command ขวา",
        ),
        K::TriggerRightCommandNoPerm => pick(
            lang,
            "Tap Right Command (needs Accessibility)",
            "轻点右 Command（需辅助功能权限）",
            "แตะปุ่ม Command ขวา (ต้องการสิทธิ์ Accessibility)",
        ),
        // 键名不翻译：它是键盘上印的字，翻了反而找不到
        K::TriggerCtrlShiftR => pick(lang, "Ctrl+Shift+R", "Ctrl+Shift+R", "Ctrl+Shift+R"),
        K::InputDeviceSection => pick(lang, "Input Device", "输入设备", "อุปกรณ์รับเสียง"),
        K::AutoPaste => pick(
            lang,
            "Auto-paste at cursor",
            "自动上屏（粘到光标处）",
            "วางอัตโนมัติที่เคอร์เซอร์",
        ),
        K::RetentionSection => pick(lang, "Keep Raw Audio", "原始音频保留", "เก็บเสียงต้นฉบับ"),
        K::Retention7 => pick(lang, "7 days", "7 天", "7 วัน"),
        K::Retention30 => pick(lang, "30 days", "30 天", "30 วัน"),
        K::Retention90 => pick(lang, "90 days", "90 天", "90 วัน"),
        K::RetentionNever => pick(lang, "Never delete", "永不清理", "ไม่ลบ"),
        K::LanguageSection => pick(lang, "Language", "语言", "ภาษา"),
        K::OpenDataDir => pick(lang, "Open Data Folder", "打开数据目录", "เปิดโฟลเดอร์ข้อมูล"),
        K::ViewLog => pick(lang, "View Log", "查看日志", "ดูบันทึก"),
        K::Quit => pick(lang, "Quit AgentEar", "退出 AgentEar", "ออกจาก AgentEar"),
    }
}

// —— 带参数的文案 ——
//
// 这些不能走 `t()`：占位符的个数和类型得由编译器管住。每个都单独一个
// 函数，签名里写死参数，比在调用点随手 `format!` 安全。

/// `■ 停止录音（已录 12s）`
pub fn stop_recording(lang: Lang, secs: u32) -> String {
    match lang {
        Lang::En => format!("■ Stop Recording ({secs}s)"),
        Lang::Zh => format!("■ 停止录音（已录 {secs}s）"),
        Lang::Th => format!("■ หยุดบันทึก ({secs} วินาที)"),
    }
}

/// `系统默认（MacBook Pro麦克风）`
pub fn system_default_device(lang: Lang, name: &str) -> String {
    match lang {
        Lang::En => format!("System Default ({name})"),
        Lang::Zh => format!("系统默认（{name}）"),
        Lang::Th => format!("ค่าเริ่มต้นของระบบ ({name})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KEYS: [Key; 18] = [
        Key::StartRecording,
        Key::Transcribing,
        Key::TitleTranscribing,
        Key::TriggerSection,
        Key::TriggerRightCommand,
        Key::TriggerRightCommandNoPerm,
        Key::TriggerCtrlShiftR,
        Key::InputDeviceSection,
        Key::AutoPaste,
        Key::RetentionSection,
        Key::Retention7,
        Key::Retention30,
        Key::Retention90,
        Key::RetentionNever,
        Key::LanguageSection,
        Key::OpenDataDir,
        Key::ViewLog,
        Key::Quit,
    ];

    #[test]
    fn nothing_is_empty() {
        for lang in Lang::ALL {
            for key in ALL_KEYS {
                assert!(!t(lang, key).is_empty(), "{lang:?}/{key:?} 是空串");
            }
        }
    }

    /// 抓「漏翻译时直接把英文粘过去」——编译器管不了这个。
    /// 键名和纯符号是合法的例外。
    #[test]
    fn translations_differ_from_english() {
        const SAME_ON_PURPOSE: [Key; 1] = [Key::TriggerCtrlShiftR];
        for key in ALL_KEYS {
            if SAME_ON_PURPOSE.contains(&key) {
                continue;
            }
            for lang in [Lang::Zh, Lang::Th] {
                assert_ne!(
                    t(lang, key),
                    t(Lang::En, key),
                    "{lang:?} 的 {key:?} 和英文一模一样，八成是漏翻了"
                );
            }
        }
    }

    /// 泰文文案必须真的含泰文字符，防止误填成拉丁转写。
    #[test]
    fn thai_is_actually_thai() {
        const NO_THAI_SCRIPT: [Key; 1] = [Key::TriggerCtrlShiftR];
        for key in ALL_KEYS {
            if NO_THAI_SCRIPT.contains(&key) {
                continue;
            }
            let s = t(Lang::Th, key);
            assert!(
                s.chars().any(|c| matches!(c as u32, 0x0E00..=0x0E7F)),
                "{key:?} 的泰文文案里没有泰文字符: {s:?}"
            );
        }
    }

    /// 带参数的文案，占位符必须真的被替换掉。
    #[test]
    fn formatted_strings_substitute() {
        for lang in Lang::ALL {
            assert!(stop_recording(lang, 12).contains("12"), "{lang:?} 丢了秒数");
            let d = system_default_device(lang, "MacBook Pro麦克风");
            assert!(d.contains("MacBook Pro麦克风"), "{lang:?} 丢了设备名");
        }
    }

    /// 语言名永远用它自己书写——英文界面下也得显示「中文」而不是 "Chinese"，
    /// 否则用户在一堆看不懂的字里找不到自己的语言。
    #[test]
    fn endonyms_are_self_written() {
        assert_eq!(Lang::Zh.endonym(), "中文");
        assert_eq!(Lang::Th.endonym(), "ไทย");
        assert_eq!(Lang::En.endonym(), "English");
    }

    #[test]
    fn default_is_english() {
        assert_eq!(Lang::default(), Lang::En);
    }
}
