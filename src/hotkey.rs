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
//!
//! ## 「轻点一下」的判据（v0.2 修掉的误触发）
//!
//! 右 Command 是常用修饰键。最初的实现在**按下**的瞬间就触发，于是用右手
//! 按 ⌘C / ⌘V 每次都会顺带启动一次录音。
//!
//! 现在改成在**松开**时判定，且要同时满足：
//!
//! 1. 按下期间没有任何普通键按下（所以 ⌘V 不会触发）
//! 2. 按下期间没有其他修饰键参与（所以 ⌘⇧… 不会触发）
//! 3. 按下到松开不超过 `TAP_MAX`（所以「按住右 Cmd 想快捷键」不会触发）
//!
//! 判据 1 要求 tap 也订阅 `KeyDown`。**注意这是键盘记录器形状的能力**：
//! 这里只读「有没有键按下」这一个 bool，**从不读键码、不记录、不落盘**，
//! 键码只有 `--debug-keys` 显式打开时才会打印。改这段代码时请保持这个边界。

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};

pub use crate::config::Trigger;

/// macOS 在事件 flags 的低位里用独立比特区分左右修饰键
/// （`IOKit/hidsystem/IOLLEvent.h` 的 `NX_DEVICE*KEYMASK`）。
/// 光看 `CGEventFlagCommand` 分不出左右，必须看这些位。
const NX_DEVICE_L_CMD: u64 = 0x0000_0008;
const NX_DEVICE_R_CMD: u64 = 0x0000_0010;

/// 「同时按着别的修饰键」的判据。左 Command 也算——右手按右 Cmd 的同时
/// 左手按左 Cmd，那显然不是想录音。
const F_SHIFT: u64 = 0x0002_0000;
const F_CONTROL: u64 = 0x0004_0000;
const F_ALTERNATE: u64 = 0x0008_0000;
const F_SECONDARY_FN: u64 = 0x0080_0000;
const OTHER_MODS: u64 = F_SHIFT | F_CONTROL | F_ALTERNATE | F_SECONDARY_FN | NX_DEVICE_L_CMD;

/// 「轻点一下」的最长时长。超过就认为是在把右 Command 当修饰键用。
///
/// 500ms 是个折中：短于 300ms 会误伤按得慢的人，长于 800ms 就盖不住
/// 「按住右 Cmd 犹豫要按什么」这种情况了。
const TAP_MAX: Duration = Duration::from_millis(500);

pub struct Listener {
    rx: Option<Receiver<()>>,
    pub trigger: Trigger,
    _carbon: Option<global_hotkey::GlobalHotKeyManager>,
}

static TX: OnceLock<Sender<()>> = OnceLock::new();

/// 右 Command 当前是否按下。flagsChanged 按下/松开各来一次，靠它做边沿检测。
static R_CMD_DOWN: AtomicBool = AtomicBool::new(false);
/// 本次按下至今是否仍是「干净的一次轻点」。任何普通键或其他修饰键都会弄脏它。
static CLEAN: AtomicBool = AtomicBool::new(false);
/// 按下时刻，相对 `START` 的毫秒数。用原子量而非 Mutex——KeyDown 回调在
/// 每一次敲键时都会跑，不能在那条路径上加锁。
static DOWN_AT_MS: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

/// 打印每一个修饰键事件，用于排查「按了没反应」。由 `--debug-keys` 打开。
static DEBUG_KEYS: OnceLock<bool> = OnceLock::new();

pub fn set_debug_keys(on: bool) {
    DEBUG_KEYS.set(on).ok();
}

fn debug_keys() -> bool {
    *DEBUG_KEYS.get().unwrap_or(&false)
}

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn fire() {
    if let Some(tx) = TX.get() {
        let _ = tx.send(());
    }
}

/// 从菜单栏手动触发一次「开始/停止」，等价于按了一下触发键。
///
/// 走的是**同一条 channel**，所以状态机只有一个入口——菜单和快捷键不会
/// 各自维护一份状态而对不上。触发键失灵时这也是唯一能停下录音的出路。
pub fn trigger_now() {
    log::debug!("菜单手动触发");
    fire();
}

impl Listener {
    /// 按配置选触发方式。要用右 Command 但没有辅助功能权限时降级到组合键。
    pub fn start(want: Trigger) -> Result<Self> {
        if want == Trigger::CtrlShiftR {
            log::info!("触发键：Ctrl+Shift+R（配置指定）");
            return Self::start_combo();
        }
        if !is_accessibility_trusted() {
            log::warn!("辅助功能权限：未授予 → 降级为 Ctrl+Shift+R");
            log::warn!("  想用右 Command：系统设置 → 隐私与安全性 → 辅助功能，勾选本程序后重启");
            return Self::start_combo();
        }
        log::info!("辅助功能权限：已授予 → 使用「轻点一下右 Command」触发");
        match Self::start_right_command() {
            Ok(l) => Ok(l),
            Err(e) => {
                log::error!("右 Command 监听启动失败：{e:#}");
                log::warn!("降级为 Ctrl+Shift+R");
                Self::start_combo()
            }
        }
    }

    fn start_right_command() -> Result<Self> {
        let (tx, rx) = channel();
        TX.set(tx).ok();
        START.get_or_init(Instant::now);

        // event tap 必须在跑 run loop 的那个线程上创建并挂载。
        // 放到独立线程，让它自带 run loop。
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();

        std::thread::spawn(move || {
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                // ListenOnly：只观察不吞事件，右 Command 仍能正常当修饰键使用
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
                move |_proxy, typ, event| {
                    match typ {
                        // 有普通键按下 → 这次右 Command 是在当修饰键用，作废。
                        // 只读「有没有」，不读是哪个键。
                        CGEventType::KeyDown => {
                            if R_CMD_DOWN.load(Ordering::Relaxed) {
                                CLEAN.store(false, Ordering::Relaxed);
                            }
                        }
                        CGEventType::FlagsChanged => {
                            let raw = event.get_flags().bits();
                            let r_cmd = raw & NX_DEVICE_R_CMD != 0;

                            if debug_keys() {
                                let code = event
                                    .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                                log::debug!(
                                    "flagsChanged: keyCode={code} raw=0x{raw:x} \
                                     左Cmd={} 右Cmd={r_cmd}",
                                    raw & NX_DEVICE_L_CMD != 0
                                );
                            }

                            // 判据只看设备位。
                            //
                            // 不要再加 `code == 54` 之类的兜底：松开事件的 keyCode
                            // 同样是 54，raw 也非 0（实测 0x10100），那样会把松开
                            // 也当成按下，状态位永远卡在 true，上升沿再不出现——
                            // 表现就是「能开始录音但按第二次停不下来」。
                            let was = R_CMD_DOWN.swap(r_cmd, Ordering::Relaxed);

                            if r_cmd && !was {
                                // 按下：开始计时。按下的同时已有别的修饰键就直接作废。
                                DOWN_AT_MS.store(now_ms(), Ordering::Relaxed);
                                CLEAN.store(raw & OTHER_MODS == 0, Ordering::Relaxed);
                            } else if !r_cmd && was {
                                // 松开：这里才判定
                                let held = now_ms().saturating_sub(
                                    DOWN_AT_MS.load(Ordering::Relaxed),
                                );
                                let clean = CLEAN.load(Ordering::Relaxed);
                                if is_tap(held, clean) {
                                    log::debug!("→ 右 Command 轻点（{held}ms）");
                                    fire();
                                } else if debug_keys() {
                                    log::debug!(
                                        "→ 右 Command 松开但不算轻点（{held}ms, clean={clean}）"
                                    );
                                }
                            } else if r_cmd && raw & OTHER_MODS != 0 {
                                // 仍按住，但期间又按下了别的修饰键 → 作废
                                CLEAN.store(false, Ordering::Relaxed);
                            }
                        }
                        _ => {}
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
                    fire();
                }
            }
        });

        Ok(Self {
            rx: Some(rx),
            trigger: Trigger::CtrlShiftR,
            _carbon: Some(manager),
        })
    }

    pub fn take_receiver(&mut self) -> Receiver<()> {
        self.rx.take().expect("receiver 已被取走")
    }

    pub fn describe(&self) -> &'static str {
        match self.trigger {
            Trigger::RightCommand => "轻点右 Command",
            Trigger::CtrlShiftR => "Ctrl+Shift+R",
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

/// 轻点判据的纯函数版本，便于测试真值表。
///
/// 事件回调本身依赖 CGEventTap 跑不了单测，但判据是这里最容易出错的部分
/// （误触发就是它错了），所以把它单独提出来测。
pub fn is_tap(held_ms: u64, clean: bool) -> bool {
    clean && held_ms <= TAP_MAX.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_clean_press_is_a_tap() {
        assert!(is_tap(120, true));
    }

    #[test]
    fn cmd_v_is_not_a_tap() {
        // 按住右 Cmd 再按 V：CLEAN 被 KeyDown 打掉
        assert!(!is_tap(120, false));
    }

    #[test]
    fn long_hold_is_not_a_tap() {
        // 按住右 Cmd 不放当修饰键用
        assert!(!is_tap(1500, true));
    }

    #[test]
    fn boundary_is_inclusive() {
        assert!(is_tap(TAP_MAX.as_millis() as u64, true));
        assert!(!is_tap(TAP_MAX.as_millis() as u64 + 1, true));
    }
}
