//! 菜单栏状态显示（NSStatusItem）。
//!
//! ## 线程约束
//!
//! AppKit 的所有 UI 调用都必须在主线程。而录音状态是在工作线程里变化的，
//! 所以这里不做跨线程调用：工作线程只更新一个原子变量，主线程用定时器
//! 轮询它并刷新标题。这比 `dispatch_async` 到主队列更简单，也不需要
//! 在回调里持有 Objective-C 对象。
//!
//! 另外：`NSStatusItem` 需要 `NSApplication` 已初始化并在跑事件循环。
//! 所以 `main` 的主线程改跑 `NSApplication::run()`，不再是裸 CFRunLoop。
//! 按键监听的 `CGEventTap` 在自己的线程上带独立 run loop，不受影响。

use std::sync::atomic::{AtomicU8, Ordering};

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, NSTimer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Idle = 0,
    Recording = 1,
    Transcribing = 2,
}

static STATUS: AtomicU8 = AtomicU8::new(0);
/// 录音时长，单位秒。让菜单栏能显示「● 12s」而不只是「● 」。
static SECS: AtomicU8 = AtomicU8::new(0);

pub fn set(s: Status) {
    STATUS.store(s as u8, Ordering::Relaxed);
}

pub fn set_secs(v: u32) {
    SECS.store(v.min(255) as u8, Ordering::Relaxed);
}

fn title() -> String {
    match STATUS.load(Ordering::Relaxed) {
        1 => format!("● {}s", SECS.load(Ordering::Relaxed)),
        2 => "◌ 转写中".to_string(),
        _ => "🎙".to_string(),
    }
}

pub struct Tray {
    _item: Retained<NSStatusItem>,
    _timer: Retained<NSTimer>,
}

/// 在主线程上装好菜单栏图标。必须在 `NSApplication::run()` 之前调用。
pub fn install(mtm: MainThreadMarker) -> Option<Tray> {
    let app = NSApplication::sharedApplication(mtm);
    // Accessory：只在菜单栏出现，不占 Dock、不抢焦点
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let bar = unsafe { NSStatusBar::systemStatusBar() };
    let item = unsafe { bar.statusItemWithLength(NSVariableStatusItemLength) };

    unsafe {
        if let Some(button) = item.button(mtm) {
            button.setTitle(&NSString::from_str(&title()));
        }
    }

    // 主线程定时器轮询状态。0.5s 足够让「录音中 Ns」看起来是活的，
    // 又不会因为频繁刷新而浪费。
    let item_for_timer = item.clone();
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(
            0.5,
            true,
            &block2::RcBlock::new(move |_t: core::ptr::NonNull<NSTimer>| {
                let mtm = MainThreadMarker::new_unchecked();
                if let Some(button) = item_for_timer.button(mtm) {
                    button.setTitle(&NSString::from_str(&title()));
                }
            }),
        )
    };

    Some(Tray {
        _item: item,
        _timer: timer,
    })
}

/// 进入 AppKit 事件循环。不会返回。
pub fn run(mtm: MainThreadMarker) -> ! {
    let app = NSApplication::sharedApplication(mtm);
    app.run();
    unreachable!("NSApplication::run 不应返回")
}
