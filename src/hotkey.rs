//! 触发键监听。
//!
//! ## 为什么用 CGEventTap 而不是 NSEvent
//!
//! macOS 上「单独按一下右 Command」和「Ctrl+Shift+R 这种组合键」是两种
//! 完全不同的机制：
//!
//! | 触发方式 | 机制 | 需要辅助功能权限 |
//! |---|---|---|
//! | 组合键（Ctrl+Shift+R） | Carbon `RegisterEventHotKey` | **否** |
//! | 单个修饰键（右 Command） | 监听 flagsChanged 事件 | **是** |
//!
//! Carbon 的 hotkey 只接受「修饰键 + 普通键」，注册不了裸修饰键，所以想要
//! 「按一下右 Command」只能监听 flagsChanged。
//!
//! 监听 flagsChanged 有两条路，**这里踩过一个坑**：
//!
//! - `NSEvent addGlobalMonitorForEventsMatchingMask:` —— 依赖 AppKit 的
//!   `NSApplication` 机制。在纯 CLI 二进制里注册**会返回句柄但回调永不触发**，
//!   因为没有 AppKit 的事件派发。第一版栽在这里。
//! - **`CGEventTap`** —— Quartz 层的接口，只需要一个 CFRunLoop 就能工作，
//!   适合没有 GUI 主体的守护进程。当前用的是这条。
//!
//! 无论哪条路，都必须有线程在跑 CFRunLoop，否则事件不会派发。

use anyhow::{Context, Result};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};

/// 右 Command 的 virtual keycode（`Events.h` 里的 `kVK_RightCommand`）。
const KVK_RIGHT_COMMAND: i64 = 54;

/// macOS 在事件 flags 的低位里用独立比特区分左右修饰键
/// （`IOKit/hidsystem/IOLLEvent.h` 的 `NX_DEVICE*KEYMASK`）。
/// 光看 `CGEventFlagCommand` 分不出左右，必须看这些位。
const NX_DEVICE_L_CMD: u64 = 0x0000_0008;
const NX_DEVICE_R_CMD: u64 = 0x0000_0010;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    RightCommand,
    ComboCtrlShiftR,
}

pub struct Listener {
    rx: Option<Receiver<()>>,
    pub trigger: Trigger,
    _carbon: Option<global_hotkey::GlobalHotKeyManager>,
}

static TX: OnceLock<Sender<()>> = OnceLock::new();

/// 右 Command 当前是否按下。用于上升沿检测——flagsChanged 按下/松开
/// 各来一次，不做边沿检测会触发两次。
static R_CMD_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 打印每一个修饰键事件，用于排查「按了没反应」。由 `--debug-keys` 打开。
static DEBUG_KEYS: OnceLock<bool> = OnceLock::new();

pub fn set_debug_keys(on: bool) {
    DEBUG_KEYS.set(on).ok();
}

fn debug_keys() -> bool {
    *DEBUG_KEYS.get().unwrap_or(&false)
}

impl Listener {
    /// 优先用右 Command；权限不足或启动失败时降级到组合键。
    pub fn start() -> Result<Self> {
        if is_accessibility_trusted() {
            log::info!("辅助功能权限：已授予 → 使用「单独按右 Command」触发");
            match Self::start_right_command() {
                Ok(l) => Ok(l),
                Err(e) => {
                    log::error!("右 Command 监听启动失败：{e:#}");
                    log::warn!("降级为 Ctrl+Shift+R");
                    Self::start_combo()
                }
            }
        } else {
            log::warn!("辅助功能权限：未授予 → 降级为 Ctrl+Shift+R");
            log::warn!("  想用右 Command：系统设置 → 隐私与安全性 → 辅助功能，勾选本程序后重启");
            Self::start_combo()
        }
    }

    fn start_right_command() -> Result<Self> {
        let (tx, rx) = channel();
        TX.set(tx).ok();

        // event tap 必须在跑 run loop 的那个线程上创建并挂载。
        // 放到独立线程，让它自带 run loop。
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();

        std::thread::spawn(move || {
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                // ListenOnly：只观察不吞事件，右 Command 仍能正常当修饰键使用
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::FlagsChanged],
                move |_proxy, _typ, event| {
                    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    let raw = event.get_flags().bits();
                    let r_cmd = raw & NX_DEVICE_R_CMD != 0;

                    if debug_keys() {
                        log::debug!(
                            "flagsChanged: keyCode={code} raw=0x{raw:x} \
                             左Cmd={} 右Cmd={r_cmd}",
                            raw & NX_DEVICE_L_CMD != 0
                        );
                    }

                    // 判据优先用设备位（能分左右），keyCode 作为兜底。
                    // flagsChanged 在按下和松开各来一次，这里做上升沿检测，
                    // 只在「原本没按 → 现在按下」时触发一次。
                    let down = r_cmd || code == KVK_RIGHT_COMMAND && raw != 0;
                    let was = R_CMD_DOWN.swap(down, std::sync::atomic::Ordering::Relaxed);
                    if down && !was {
                        log::debug!("→ 右 Command 按下");
                        if let Some(tx) = TX.get() {
                            let _ = tx.send(());
                        }
                    }
                    CallbackResult::Keep
                },
            );

            let tap = match tap {
                Ok(t) => t,
                Err(_) => {
                    let _ = ready_tx
                        .send(Err("CGEventTap 创建失败（辅助功能权限未真正生效？）".into()));
                    return;
                }
            };

            let loop_source = match tap.mach_port().create_runloop_source(0) {
                Ok(s) => s,
                Err(_) => {
                    let _ = ready_tx.send(Err("创建 run loop source 失败".into()));
                    return;
                }
            };

            let current = CFRunLoop::get_current();
            unsafe {
                current.add_source(&loop_source, kCFRunLoopCommonModes);
            }
            tap.enable();
            log::debug!("CGEventTap 已挂载到监听线程的 run loop");
            let _ = ready_tx.send(Ok(()));

            // 这个线程从此专职派发按键事件
            CFRunLoop::run_current();
        });

        match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                rx: Some(rx),
                trigger: Trigger::RightCommand,
                _carbon: None,
            }),
            Ok(Err(e)) => anyhow::bail!("{e}"),
            Err(_) => anyhow::bail!("监听线程启动超时"),
        }
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
        log::info!("已注册 Ctrl+Shift+R（Carbon，id={}）", hk.id());

        std::thread::spawn(move || {
            let carbon_rx = global_hotkey::GlobalHotKeyEvent::receiver();
            while let Ok(ev) = carbon_rx.recv() {
                log::debug!("Carbon 事件: id={} state={:?}", ev.id, ev.state);
                if ev.state == global_hotkey::HotKeyState::Pressed {
                    if let Some(tx) = TX.get() {
                        let _ = tx.send(());
                    }
                }
            }
        });

        Ok(Self {
            rx: Some(rx),
            trigger: Trigger::ComboCtrlShiftR,
            _carbon: Some(manager),
        })
    }

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

/// 本进程是否已被授予辅助功能权限。
///
/// 注意：从终端跑时查到的是**终端**的权限（TCC 会被继承），
/// 打包成 .app 之后需要对该 .app 单独授权。
pub fn is_accessibility_trusted() -> bool {
    check_accessibility(false)
}

pub fn prompt_accessibility() -> bool {
    check_accessibility(true)
}

fn check_accessibility(prompt: bool) -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;

    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    }

    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let val = CFBoolean::from(prompt);
    let opts = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), val.as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) }
}
