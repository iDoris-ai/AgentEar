//! 转写结果自动上屏：写完剪贴板后合成一次 ⌘V，发给当前前台应用。
//!
//! ## 为什么是「合成 ⌘V」而不是逐字符敲进去
//!
//! `CGEventKeyboardSetUnicodeString` 可以逐字符注入，不碰剪贴板，看着更干净。
//! 但它在中文输入法激活时会被输入法拦截重组，几百字的转写结果逐字符发还很慢。
//! 走剪贴板 + ⌘V 是一次原子操作，跟输入法无关，且失败时文本仍在剪贴板里，
//! 用户手动粘一下就行——**降级路径天然存在**。
//!
//! ## 为什么不会误触发自己的热键
//!
//! 两层保证：
//!
//! 1. 这里合成的是 **keyDown/keyUp** 事件，而 `hotkey.rs` 的 tap 只订阅了
//!    `FlagsChanged`，压根收不到。
//! 2. 即便收到，判据看的是设备位 `NX_DEVICE_R_CMD (0x10)`；这里只设
//!    `CGEventFlagCommand (0x100000)`，不带左右设备位。
//!
//! ## 为什么不自动按回车
//!
//! 目标窗口通常是终端。ASR 有错字（M0 实测中英混杂术语必错，见
//! `docs/benchmarks.md`），自动回车等于直接执行一条没人看过的命令。
//! **上屏到此为止，回车永远交给人。**

use anyhow::{anyhow, Result};
use std::sync::OnceLock;
use std::time::Duration;

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

/// `kVK_ANSI_V`（`Carbon/Events.h`）。
const KVK_ANSI_V: u16 = 9;

/// 写剪贴板到发 ⌘V 之间的等待。
///
/// NSPasteboard 的写入对本进程是同步的，但目标应用是在收到 ⌘V 之后才去读
/// pasteboard，中间隔着一次跨进程调度。留一点余量，否则偶发粘到旧内容。
const SETTLE: Duration = Duration::from_millis(40);

static ENABLED: OnceLock<bool> = OnceLock::new();

pub fn set_enabled(on: bool) {
    ENABLED.set(on).ok();
}

pub fn enabled() -> bool {
    *ENABLED.get().unwrap_or(&true)
}

/// 向当前前台应用发一次 ⌘V。
///
/// 需要辅助功能权限——`CGEventPost` 在未授权时**静默失败**，不返回错误，
/// 所以调用方要先自己查权限，不要指望这里报错。
pub fn paste() -> Result<()> {
    std::thread::sleep(SETTLE);

    // CGEventSource 是 by-value 传参，down/up 各建一个
    let key = |down: bool| -> Result<CGEvent> {
        let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| anyhow!("创建 CGEventSource 失败"))?;
        let ev = CGEvent::new_keyboard_event(src, KVK_ANSI_V, down)
            .map_err(|_| anyhow!("创建键盘事件失败"))?;
        ev.set_flags(CGEventFlags::CGEventFlagCommand);
        Ok(ev)
    };

    key(true)?.post(CGEventTapLocation::HID);
    key(false)?.post(CGEventTapLocation::HID);
    Ok(())
}

/// 去掉换行与其他控制字符。
///
/// `asr::clean()` 目前用 `""` 拼接各行，正常情况下拿不到换行。这是第二道闸：
/// **粘进终端的多行文本会被 shell 逐行执行**，一旦哪天 ASR 侧改了拼接方式，
/// 后果不是显示难看而是执行了未知命令。宁可在这里重复防一次。
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_never_survive() {
        assert_eq!(sanitize("rm -rf /\n echo hi"), "rm -rf / echo hi");
        assert_eq!(sanitize("a\r\nb"), "a b");
    }

    #[test]
    fn keeps_chinese_and_punctuation() {
        assert_eq!(sanitize("你好，世界。"), "你好，世界。");
    }

    #[test]
    fn trims_and_collapses() {
        assert_eq!(sanitize("  多余   空格  "), "多余 空格");
    }
}
