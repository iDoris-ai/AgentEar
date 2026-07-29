//! 触发键监听。
//!
//! ## 为什么有两条实现路径
//!
//! macOS 上「单独按一下右 Command」和「Ctrl+Shift+R 这种组合键」是两种
//! 完全不同的机制，权限要求也不同：
//!
//! | 触发方式 | 机制 | 需要辅助功能权限 |
//! |---|---|---|
//! | 组合键（Ctrl+Shift+R） | Carbon `RegisterEventHotKey` | **否** |
//! | 单个修饰键（右 Command） | `NSEvent` 全局监听 flagsChanged | **是** |
//!
//! Carbon 的 hotkey 只接受「修饰键 + 普通键」，注册不了裸修饰键。所以
//! 想要「按一下右 Command」，只能走 NSEvent，代价是必须授予辅助功能权限。
//!
//! 默认用右 Command；未授权时自动降级到 Ctrl+Shift+R 并给出提示。

use anyhow::{Context, Result};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

/// 右 Command 的 virtual keycode（`Events.h` 中的 `kVK_RightCommand`）。
const KVK_RIGHT_COMMAND: u16 = 54;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 单独按一下右 Command（需辅助功能权限）
    RightCommand,
    /// Ctrl+Shift+R（无需额外权限）
    ComboCtrlShiftR,
}

pub struct Listener {
    rx: Option<Receiver<()>>,
    pub trigger: Trigger,
    // 监听器句柄要一直活着，否则 NSEvent 会摘掉回调
    _monitor: Option<Retained<objc2::runtime::AnyObject>>,
    _carbon: Option<global_hotkey::GlobalHotKeyManager>,
}

static TX: OnceLock<Sender<()>> = OnceLock::new();

impl Listener {
    /// 优先用右 Command；权限不足时降级到组合键。
    pub fn start() -> Result<Self> {
        if is_accessibility_trusted() {
            log::info!("辅助功能权限：已授予 → 使用「单独按右 Command」触发");
            Self::start_right_command()
        } else {
            log::warn!("辅助功能权限：未授予 → 降级为 Ctrl+Shift+R");
            log::warn!(
                "  想用右 Command，请到「系统设置 → 隐私与安全性 → 辅助功能」勾选本程序后重启它"
            );
            Self::start_combo()
        }
    }

    fn start_right_command() -> Result<Self> {
        let (tx, rx) = channel();
        TX.set(tx).ok();

        // flagsChanged 事件携带修饰键的按下/松开。裸修饰键不会产生 keyDown。
        let handler = objc2::rc::autoreleasepool(|_| unsafe {
            let block = block2::RcBlock::new(move |event: core::ptr::NonNull<NSEvent>| {
                let event = event.as_ref();
                let code = event.keyCode();
                if code != KVK_RIGHT_COMMAND {
                    return;
                }
                let flags = event.modifierFlags();
                let pressed = flags.contains(NSEventModifierFlags::Command);
                log::debug!("flagsChanged: keyCode={code} command={pressed}");
                // 只在「按下」的那一次触发，松开时忽略
                if pressed {
                    if let Some(tx) = TX.get() {
                        let _ = tx.send(());
                    }
                }
            });
            NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
                NSEventMask::FlagsChanged,
                &block,
            )
        });

        if handler.is_none() {
            anyhow::bail!("NSEvent 全局监听注册失败（辅助功能权限被撤销？）");
        }

        Ok(Self {
            rx: Some(rx),
            trigger: Trigger::RightCommand,
            _monitor: handler,
            _carbon: None,
        })
    }

    fn start_combo() -> Result<Self> {
        use global_hotkey::hotkey::{Code, HotKey, Modifiers};
        use global_hotkey::GlobalHotKeyManager;

        let (tx, rx) = channel();
        TX.set(tx).ok();

        let manager = GlobalHotKeyManager::new().context("创建 Carbon 快捷键管理器失败")?;
        let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
        manager
            .register(hk)
            .context("注册 Ctrl+Shift+R 失败（可能已被其他程序占用）")?;
        log::info!("已注册 Ctrl+Shift+R（Carbon，hotkey id={}）", hk.id());

        // Carbon 的事件走它自己的全局队列，这里转发到统一 channel
        std::thread::spawn(move || {
            let carbon_rx = global_hotkey::GlobalHotKeyEvent::receiver();
            loop {
                if let Ok(ev) = carbon_rx.recv() {
                    log::debug!("Carbon 事件: id={} state={:?}", ev.id, ev.state);
                    if ev.state == global_hotkey::HotKeyState::Pressed {
                        if let Some(tx) = TX.get() {
                            let _ = tx.send(());
                        }
                    }
                }
            }
        });

        Ok(Self {
            rx: Some(rx),
            trigger: Trigger::ComboCtrlShiftR,
            _monitor: None,
            _carbon: Some(manager),
        })
    }

    /// 取走事件接收端交给工作线程。主线程要留着跑 CFRunLoop。
    pub fn take_receiver(&mut self) -> Receiver<()> {
        self.rx.take().expect("receiver 已被取走")
    }

    pub fn describe(&self) -> &'static str {
        match self.trigger {
            Trigger::RightCommand => "右 Command 键",
            Trigger::ComboCtrlShiftR => "Ctrl+Shift+R",
        }
    }
}

/// 查询本进程是否已被授予辅助功能权限。
///
/// 传 `prompt = true` 会弹出系统的授权引导对话框。
pub fn is_accessibility_trusted() -> bool {
    check_accessibility(false)
}

pub fn prompt_accessibility() -> bool {
    check_accessibility(true)
}

fn check_accessibility(prompt: bool) -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    extern "C" {
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation::dictionary::CFDictionaryRef,
        ) -> bool;
    }

    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let val = CFBoolean::from(prompt);
    let opts = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), val.as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) }
}
