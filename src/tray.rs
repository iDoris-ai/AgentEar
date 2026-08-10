//! 菜单栏状态显示与设置菜单（NSStatusItem + NSMenu）。
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
//!
//! ## 菜单为什么每次打开都重建
//!
//! 输入设备列表会随耳机插拔变化。菜单只在 `install()` 时建一次的话，插上
//! 耳机后列表就是错的。走 `NSMenuDelegate::menuNeedsUpdate:`，每次展开前
//! 重新枚举——这也是 AppKit 里做动态菜单的正规姿势。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn,
    NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObjectProtocol, NSString, NSTimer};

use crate::config::{self, Trigger};

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

/// 数据目录，供「打开数据目录 / 查看日志」两项使用。
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();
/// 上一次建菜单时枚举到的设备列表。点击回调按下标取名字——必须用建菜单
/// 那一刻的快照，不能重新枚举，否则期间插拔设备会选错。
static DEVICE_SNAPSHOT: Mutex<Vec<String>> = Mutex::new(Vec::new());

// 菜单项的 tag。用一个 action 加 tag 分发，省掉十几个 ObjC 方法。
/// 开始 / 停止录音。放在菜单第一项——**这是触发键失灵时唯一的出路**，
/// v0.2.0 漏了它，结果录音开起来就只能靠快捷键停，或者干脆退出程序。
const TAG_TOGGLE: isize = 0;
const TAG_AUTO_PASTE: isize = 1;
const TAG_OPEN_DATA: isize = 2;
const TAG_OPEN_LOG: isize = 3;
const TAG_QUIT: isize = 4;
const TAG_TRIGGER_BASE: isize = 100;
const TAG_RETENTION_BASE: isize = 200;
/// `+0` 是「系统默认」，`+1..` 对应 `DEVICE_SNAPSHOT` 的下标。
const TAG_DEVICE_BASE: isize = 1000;

const RETENTION_CHOICES: [(u32, &str); 4] = [
    (7, "7 天"),
    (30, "30 天"),
    (90, "90 天"),
    (0, "永不清理"),
];

pub fn set(s: Status) {
    STATUS.store(s as u8, Ordering::Relaxed);
}

pub fn set_secs(v: u32) {
    SECS.store(v.min(255) as u8, Ordering::Relaxed);
}

pub fn set_data_root(p: PathBuf) {
    DATA_ROOT.set(p).ok();
}

fn title() -> String {
    match STATUS.load(Ordering::Relaxed) {
        1 => format!("● {}s", SECS.load(Ordering::Relaxed)),
        2 => "◌ 转写中".to_string(),
        _ => "🎙".to_string(),
    }
}

define_class!(
    // SAFETY: 超类 NSObject 无子类化要求；本类不实现 Drop。
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AgentEarMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(onItem:))]
        fn on_item(&self, sender: &NSMenuItem) {
            handle(sender.tag(), MainThreadMarker::from(self));
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl NSMenuDelegate for MenuTarget {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            populate(menu, MainThreadMarker::from(self), self);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        unsafe { msg_send![this, init] }
    }
}

/// 建一个菜单项。`tag < 0` 表示纯展示项（不可点）。
fn item(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    title: &str,
    tag: isize,
    checked: bool,
) -> Retained<NSMenuItem> {
    let action: Option<Sel> = if tag >= 0 { Some(sel!(onItem:)) } else { None };
    let it = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(""),
        )
    };
    if tag >= 0 {
        it.setTag(tag);
        unsafe { it.setTarget(Some(AsRef::<AnyObject>::as_ref(target))) };
    } else {
        it.setEnabled(false);
    }
    it.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    it
}

fn submenu(mtm: MainThreadMarker, parent: &NSMenuItem, items: Vec<Retained<NSMenuItem>>) {
    let m = NSMenu::init(NSMenu::alloc(mtm));
    m.setAutoenablesItems(false);
    for i in items {
        m.addItem(&i);
    }
    parent.setSubmenu(Some(&m));
}

/// 清空并重新填充菜单。每次展开前调用。
fn populate(menu: &NSMenu, mtm: MainThreadMarker, target: &MenuTarget) {
    let cfg = config::get();
    menu.removeAllItems();
    // 关掉自动启用：我们自己用 setEnabled 控制,免得 AppKit 按响应链把
    // 有 target 的项也判成不可用
    menu.setAutoenablesItems(false);

    // —— 录音开关。第一项，因为它是唯一「正在发生的事」——
    // 设置项什么时候点都行，录音停不下来是急事。
    match STATUS.load(Ordering::Relaxed) {
        1 => menu.addItem(&item(
            mtm,
            target,
            &format!("■ 停止录音（已录 {}s）", SECS.load(Ordering::Relaxed)),
            TAG_TOGGLE,
            false,
        )),
        // 转写中不能打断：raw 已提交，此刻再发触发事件只会开一段新录音
        2 => menu.addItem(&item(mtm, target, "◌ 转写中……", -1, false)),
        _ => menu.addItem(&item(mtm, target, "● 开始录音", TAG_TOGGLE, false)),
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    menu.addItem(&item(
        mtm,
        target,
        &format!("AgentEar {}", env!("CARGO_PKG_VERSION")),
        -1,
        false,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // —— 触发键 ——
    let trigger_item = item(mtm, target, "触发键", -1, false);
    trigger_item.setEnabled(true);
    let trusted = crate::hotkey::is_accessibility_trusted();
    submenu(
        mtm,
        &trigger_item,
        vec![
            item(
                mtm,
                target,
                if trusted {
                    "轻点右 Command"
                } else {
                    "轻点右 Command（需辅助功能权限）"
                },
                TAG_TRIGGER_BASE,
                cfg.trigger == Trigger::RightCommand,
            ),
            item(
                mtm,
                target,
                "Ctrl+Shift+R",
                TAG_TRIGGER_BASE + 1,
                cfg.trigger == Trigger::CtrlShiftR,
            ),
        ],
    );
    menu.addItem(&trigger_item);

    // —— 输入设备 ——
    let devices = crate::audio::list_input_devices();
    let default_name = crate::audio::default_input_name().unwrap_or_else(|| "?".into());
    let mut dev_items = vec![item(
        mtm,
        target,
        &format!("系统默认（{default_name}）"),
        TAG_DEVICE_BASE,
        cfg.input_device.is_none(),
    )];
    for (i, name) in devices.iter().enumerate() {
        dev_items.push(item(
            mtm,
            target,
            name,
            TAG_DEVICE_BASE + 1 + i as isize,
            cfg.input_device.as_deref() == Some(name.as_str()),
        ));
    }
    *DEVICE_SNAPSHOT.lock().unwrap() = devices;

    let dev_item = item(mtm, target, "输入设备", -1, false);
    dev_item.setEnabled(true);
    submenu(mtm, &dev_item, dev_items);
    menu.addItem(&dev_item);

    // —— 自动上屏 ——
    menu.addItem(&item(
        mtm,
        target,
        "自动上屏（粘到光标处）",
        TAG_AUTO_PASTE,
        cfg.auto_paste,
    ));

    // —— 保留期 ——
    let ret_item = item(mtm, target, "原始音频保留", -1, false);
    ret_item.setEnabled(true);
    submenu(
        mtm,
        &ret_item,
        RETENTION_CHOICES
            .iter()
            .enumerate()
            .map(|(i, (days, label))| {
                item(
                    mtm,
                    target,
                    label,
                    TAG_RETENTION_BASE + i as isize,
                    cfg.retention_days == *days,
                )
            })
            .collect(),
    );
    menu.addItem(&ret_item);

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(mtm, target, "打开数据目录", TAG_OPEN_DATA, false));
    menu.addItem(&item(mtm, target, "查看日志", TAG_OPEN_LOG, false));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(mtm, target, "退出 AgentEar", TAG_QUIT, false));
}

fn handle(tag: isize, mtm: MainThreadMarker) {
    match tag {
        TAG_TOGGLE => crate::hotkey::trigger_now(),
        TAG_AUTO_PASTE => {
            let on = !config::get().auto_paste;
            config::update(|c| c.auto_paste = on);
            crate::paste::set_enabled(on && crate::hotkey::is_accessibility_trusted());
            log::info!("自动上屏：{}", if on { "开" } else { "关" });
        }
        TAG_OPEN_DATA => open_path(DATA_ROOT.get().cloned()),
        TAG_OPEN_LOG => open_path(DATA_ROOT.get().map(|r| r.join("agentear.log"))),
        TAG_QUIT => {
            log::info!("从菜单退出");
            NSApplication::sharedApplication(mtm).terminate(None);
        }
        t if (TAG_TRIGGER_BASE..TAG_TRIGGER_BASE + 2).contains(&t) => {
            let want = if t == TAG_TRIGGER_BASE {
                Trigger::RightCommand
            } else {
                Trigger::CtrlShiftR
            };
            if want == config::get().trigger {
                return;
            }
            config::update(|c| c.trigger = want);
            // CGEventTap 挂在一个跑 CFRunLoop 的线程上，运行时换不掉，
            // 只能重启进程。这是整个菜单里唯一需要重启的一项。
            log::info!("触发键改为 {} → 重启生效", want.label());
            crate::restart_self();
        }
        t if (TAG_RETENTION_BASE..TAG_RETENTION_BASE + RETENTION_CHOICES.len() as isize)
            .contains(&t) =>
        {
            let (days, label) = RETENTION_CHOICES[(t - TAG_RETENTION_BASE) as usize];
            config::update(|c| c.retention_days = days);
            log::info!("原始音频保留期改为 {label}");
        }
        t if t >= TAG_DEVICE_BASE => {
            let idx = (t - TAG_DEVICE_BASE) as usize;
            let chosen = if idx == 0 {
                None
            } else {
                DEVICE_SNAPSHOT.lock().unwrap().get(idx - 1).cloned()
            };
            log::info!(
                "输入设备改为 {}（下次录音生效）",
                chosen.as_deref().unwrap_or("系统默认")
            );
            config::update(|c| c.input_device = chosen);
        }
        other => log::warn!("未知菜单项 tag={other}"),
    }
}

fn open_path(p: Option<PathBuf>) {
    let Some(p) = p else {
        log::error!("数据目录未初始化");
        return;
    };
    if let Err(e) = std::process::Command::new("/usr/bin/open").arg(&p).spawn() {
        log::error!("打开 {} 失败: {e}", p.display());
    }
}

pub struct Tray {
    _item: Retained<NSStatusItem>,
    _timer: Retained<NSTimer>,
    _target: Retained<MenuTarget>,
}

/// 在主线程上装好菜单栏图标。必须在 `NSApplication::run()` 之前调用。
pub fn install(mtm: MainThreadMarker) -> Option<Tray> {
    let app = NSApplication::sharedApplication(mtm);
    // Accessory：只在菜单栏出现，不占 Dock、不抢焦点。
    // 不抢焦点这一点是自动上屏能工作的前提——前台窗口始终是用户的目标窗口。
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(NSVariableStatusItemLength);

    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str(&title()));
    }

    let target = MenuTarget::new(mtm);
    let menu = NSMenu::init(NSMenu::alloc(mtm));
    menu.setAutoenablesItems(false);
    // 内容在 menuNeedsUpdate: 里填，这里只先建一次好让首次点击就有东西
    populate(&menu, mtm, &target);
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*target)));
    item.setMenu(Some(&menu));

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
        _target: target,
    })
}

/// 进入 AppKit 事件循环。不会返回。
pub fn run(mtm: MainThreadMarker) -> ! {
    let app = NSApplication::sharedApplication(mtm);
    app.run();
    unreachable!("NSApplication::run 不应返回")
}
