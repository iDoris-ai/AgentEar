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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSControl,
    NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuDelegate, NSMenuItem,
    NSPopUpButton, NSStatusBar, NSStatusItem, NSTextField, NSVariableStatusItemLength, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::asr::AsrLang;
use crate::config::{self, Trigger};
use crate::download;
use crate::i18n::{self, Key, Lang};

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
/// 「下载完成后要不要自动切到泰语」。
///
/// 下载要好几分钟，用户完全可能中途改主意点回 Auto。没有这个标志的话，
/// 下载线程完成时会无条件把识别语言写成泰语，**把用户后来的选择覆盖掉**。
///
/// ⚠️ **必须是 Mutex，不能是 AtomicBool。** 原子量只保证单次读写有序，
/// 保证不了「读意图 + 改配置」这两步之间没人插队：
///
/// ```text
/// 下载线程: 读到 want=true ──────────────────► 写配置 Thai（晚了一步）
/// 主线程:            用户点 Auto, want=false, 写配置 Auto
/// ```
///
/// 结果是用户明确选的 Auto 被推翻。把「判断 + 落配置」整段放进同一把锁，
/// 两条路径（`on_thai_installed` 和 Auto 菜单项）都持它，才真的互斥。
static THAI_INTENT: Mutex<bool> = Mutex::new(false);

/// 泰语模型装好之后调用（由下载器在安装记录落地后回调）。
///
/// **只在用户此刻仍然想要泰语时才切**——见 `THAI_INTENT`。
pub fn on_thai_installed() {
    let intent = THAI_INTENT.lock().unwrap();
    if *intent {
        config::update(|c| c.asr_lang = AsrLang::Thai);
        log::info!("泰语模型已安装，识别语言切到泰语（下次录音生效）");
    } else {
        log::info!("泰语模型已安装；用户期间改选了别的语言，识别语言保持不变");
    }
}

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
const TAG_UI_LANG_BASE: isize = 300;
const TAG_ASR_LANG_BASE: isize = 400;
const TAG_MODE_BASE: isize = 500;
const TAG_STYLE_BASE: isize = 600;
const TAG_TONE_BASE: isize = 700;
const TAG_VOICE_BASE: isize = 800;
const TAG_CORRECT_TERMS: isize = 5;
const TAG_OPEN_TERMS: isize = 6;
const TAG_START_SIDECAR: isize = 7;
const TAG_OPEN_COMMANDS: isize = 8;
const TAG_OPEN_SETTINGS: isize = 9;
const TAG_LAUNCH_AT_LOGIN: isize = 10;
/// `+0` 是「系统默认」，`+1..` 对应 `DEVICE_SNAPSHOT` 的下标。
const TAG_DEVICE_BASE: isize = 1000;

const RETENTION_CHOICES: [(u32, Key); 4] = [
    (7, Key::Retention7),
    (30, Key::Retention30),
    (90, Key::Retention90),
    (0, Key::RetentionNever),
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

/// 菜单栏标题。语言显式传入——只有主线程调用它（0.5s 定时器），
/// 工作线程只更新上面那两个原子量，不碰文案。
/// 有没有一个「正在等你确认」的向外动作。
///
/// 和 `STATUS` 一样是**原子变量**：工作线程在确认流程里改它，
/// 主线程读它画菜单栏，中间不该为这几个字节引一条通道。
static PENDING: AtomicBool = AtomicBool::new(false);

/// 待确认状态变了。**必须让它在菜单栏可见**：
/// 一个「等着你点头、不点头就作废」的动作，如果界面上毫无痕迹，
/// 用户只会觉得「说了没反应」，而其实三秒后它自己作废了。
pub fn set_pending(on: bool) {
    // 只存原子量：标题由主线程那个 0.5s 定时器统一刷新（和 `set` 同一个套路），
    // 工作线程不碰 AppKit。
    PENDING.store(on, Ordering::Relaxed);
}

fn title(lang: Lang) -> String {
    let base = match STATUS.load(Ordering::Relaxed) {
        1 => format!("● {}s", SECS.load(Ordering::Relaxed)),
        2 => i18n::t(lang, Key::TitleTranscribing).to_string(),
        _ => "🎙".to_string(),
    };
    // **模式小标记**（jason 2026-09-14）：「后边有个小就行」。
    //
    // 只在**非默认**的对话模式加一个符号——默认状态不该也多背一个字符，
    // 而菜单栏寸土寸金。要是不加这一笔，用户只能靠回忆自己那天点了几下，
    // 或者点开菜单看勾选（那正是 v0.7.0 被抱怨「看不到」的原因）。
    //
    // 放在**后面**而不是前面：前面那个 🎙 表达「我在录音」这件更急的事，
    // 不能因为切模式就把它挤走。
    let base = match config::get().talk_mode {
        config::TalkMode::Conversation => format!("{base}💬"),
        config::TalkMode::InputMethod => base,
    };
    // **待确认标记**：比模式标记更急（它有时限），所以放最后、
    // 而且和模式标记用的是不同的符号，一眼能分辨是「等问题」还是「等确认」。
    if PENDING.load(Ordering::Relaxed) {
        format!("{base}❓")
    } else {
        base
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

        /// 设置窗口里的按钮/勾选框走这个——**跟 `onItem:` 分开**，
        /// 而不是把 `sender` 硬转成 `&NSMenuItem`：`NSButton` 不是
        /// `NSMenuItem`，混用类型是未定义行为的边界，哪怕两边都恰好
        /// 有 `tag()` 这个方法。`NSControl` 是两者共同的父类之一
        /// （按钮/勾选框都继承它），签名写对，`handle()` 复用同一套
        /// tag 分发不用改。
        #[unsafe(method(onControl:))]
        fn on_control(&self, sender: &NSControl) {
            handle(sender.tag(), MainThreadMarker::from(self));
        }

        /// 保留期那个下拉框（`NSPopUpButton`）单独一个方法：它要读的是
        /// `selectedTag()`（当前选中项的 tag），跟按钮/勾选框读自己的
        /// `tag()` 不是一回事，不能共用 `onControl:`。
        #[unsafe(method(onPopup:))]
        fn on_popup(&self, sender: &NSPopUpButton) {
            handle(sender.selectedTag(), MainThreadMarker::from(self));
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
    let lang = cfg.ui_lang;
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
            &i18n::stop_recording(lang, SECS.load(Ordering::Relaxed) as u32),
            TAG_TOGGLE,
            false,
        )),
        // 转写中不能打断：raw 已提交，此刻再发触发事件只会开一段新录音
        2 => menu.addItem(&item(mtm, target, i18n::t(lang, Key::Transcribing), -1, false)),
        _ => menu.addItem(&item(
            mtm,
            target,
            i18n::t(lang, Key::StartRecording),
            TAG_TOGGLE,
            false,
        )),
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

    // —— 模式（输入法 / 对话）——
    //
    // **放在最上面、在触发键之前**：它决定「按一下键会发生什么」，
    // 是这份菜单里唯一改变主行为的开关；触发键只决定「怎么按」。
    //
    // 两个选项都要能看见（radio 式勾选），不要做成一个「对话模式」开关——
    // 单开关的失败形态是用户不知道自己现在**不在**哪个模式里。
    // **标题里带上当前模式**：jason 找不到这个入口，就是因为标题只写「模式」，
    // 而他在找「对话模式」。现在不展开子菜单也能看见自己在哪一档。
    let mode_title = format!(
        "{}: {}",
        i18n::t(lang, Key::ModeSection),
        i18n::t(
            lang,
            match cfg.talk_mode {
                config::TalkMode::InputMethod => Key::ModeInputShort,
                config::TalkMode::Conversation => Key::ModeConversationShort,
            }
        )
    );
    let mode_item = item(mtm, target, &mode_title, -1, false);
    mode_item.setEnabled(true);
    submenu(
        mtm,
        &mode_item,
        mode_menu_entries(lang, cfg.talk_mode)
            .into_iter()
            .map(|(title, tag, checked)| item(mtm, target, &title, tag, checked))
            .collect(),
    );
    menu.addItem(&mode_item);

    // —— 说话（语系 / 语气 / 音色）——
    //
    // 放在模式后面：先决定「按一下键干什么」，再决定「它怎么说话」。
    // ⚠️ 这三项**只在对话模式有意义**，但**不做条件隐藏**——菜单里少一项比多一项更容易
    // 让人以为「功能没了」（v0.7.0 的「模式」就是这么被投诉的）。
    let speech_item = item(mtm, target, i18n::t(lang, Key::SpeechSection), -1, false);
    speech_item.setEnabled(true);
    // **语言**与**方言**分成两栏（jason 2026-09-15）：
    // 「切换男女声音一个菜单，切换方言单独一个菜单」。
    // 两栏写的是同一个 `tts_style`，只是把 12 项按用途切开——
    // 方言是高频低门槛的尝试项，混在语言里会让人找不到。
    let mut speech_sections: Vec<Retained<NSMenuItem>> = Vec::new();
    for (section, keys) in [
        (Key::StyleSection, crate::talk::LANGUAGE_STYLES),
        (Key::DialectSection, crate::talk::DIALECT_STYLES),
    ] {
        let sub = item(mtm, target, i18n::t(lang, section), -1, false);
        sub.setEnabled(true);
        submenu(
            mtm,
            &sub,
            keys.iter()
                .filter_map(|k| crate::talk::style_index(k).map(|i| (i, k)))
                .map(|(i, key)| {
                    let opt = crate::talk::STYLE_OPTIONS[i];
                    item(
                        mtm,
                        target,
                        crate::talk::option_label(&opt, lang),
                        TAG_STYLE_BASE + i as isize,
                        cfg.tts_style == opt.0,
                    )
                })
                .collect(),
        );
        speech_sections.push(sub);
    }
    let tone_item = item(mtm, target, i18n::t(lang, Key::ToneSection), -1, false);
    tone_item.setEnabled(true);
    submenu(
        mtm,
        &tone_item,
        crate::talk::TONE_OPTIONS
            .iter()
            .enumerate()
            .map(|(i, opt)| {
                item(
                    mtm,
                    target,
                    crate::talk::option_label(opt, lang),
                    TAG_TONE_BASE + i as isize,
                    cfg.tts_tone == opt.0,
                )
            })
            .collect(),
    );
    // 音色：**扫描音色库目录**（用户丢一个自己的 wav+json 进去就能选），
    // 目录里没有时给一项「(无)」——不能给空子菜单，那看起来像坏了。
    let voices = list_voices(&cfg, &store_root());
    let voice_item = item(
        mtm,
        target,
        &format!(
            "{}: {}",
            i18n::t(lang, Key::VoiceSection),
            voices
                .iter()
                .find(|v| Some(v.as_str()) == cfg.tts_voice.as_deref())
                .cloned()
                .unwrap_or_else(|| i18n::t(lang, Key::VoiceDefault).to_string())
        ),
        -1,
        false,
    );
    voice_item.setEnabled(true);
    if voices.is_empty() {
        submenu(
            mtm,
            &voice_item,
            vec![item(mtm, target, i18n::t(lang, Key::VoiceNone), -1, false)],
        );
    } else {
        submenu(
            mtm,
            &voice_item,
            voices
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    item(
                        mtm,
                        target,
                        name,
                        TAG_VOICE_BASE + i as isize,
                        cfg.tts_voice.as_deref() == Some(name.as_str()),
                    )
                })
                .collect(),
        );
    }
    // 顺序：先「说话用什么音色」→ 再「怎么说」（语言/方言/语气）。
    // 音色在最前面，因为那是用户最常改的一项。
    speech_sections.push(tone_item);
    speech_sections.push(voice_item);
    submenu(mtm, &speech_item, speech_sections);
    menu.addItem(&speech_item);

    // —— 触发键 ——
    let trigger_item = item(mtm, target, i18n::t(lang, Key::TriggerSection), -1, false);
    trigger_item.setEnabled(true);
    let trusted = crate::hotkey::is_accessibility_trusted();
    submenu(
        mtm,
        &trigger_item,
        vec![
            item(
                mtm,
                target,
                i18n::t(
                    lang,
                    if trusted {
                        Key::TriggerRightCommand
                    } else {
                        Key::TriggerRightCommandNoPerm
                    },
                ),
                TAG_TRIGGER_BASE,
                cfg.trigger == Trigger::RightCommand,
            ),
            item(
                mtm,
                target,
                i18n::t(lang, Key::TriggerCtrlShiftR),
                TAG_TRIGGER_BASE + 1,
                cfg.trigger == Trigger::CtrlShiftR,
            ),
        ],
    );
    menu.addItem(&trigger_item);

    // —— 界面语言 ——
    // 每个选项都用它自己的语言书写（English / 中文 / ไทย）：在看不懂的
    // 界面里，用户要靠认出自己的语言名找回来。
    let lang_item = item(mtm, target, i18n::t(lang, Key::LanguageSection), -1, false);
    lang_item.setEnabled(true);
    submenu(
        mtm,
        &lang_item,
        Lang::ALL
            .iter()
            .enumerate()
            .map(|(i, l)| {
                item(
                    mtm,
                    target,
                    l.endonym(),
                    TAG_UI_LANG_BASE + i as isize,
                    *l == lang,
                )
            })
            .collect(),
    );
    menu.addItem(&lang_item);

    // —— 识别语言 ——
    //
    // 紧挨着界面语言放，但**文案必须让人分清**（i18n 里有一条测试钉着
    // 两个标题不许相同）。泰语那项的标题带下载状态，见 i18n::thai_option。
    let asr_item = item(mtm, target, i18n::t(lang, Key::AsrLangSection), -1, false);
    asr_item.setEnabled(true);
    let thai_state = download::state(&download::THAI);
    submenu(
        mtm,
        &asr_item,
        vec![
            item(
                mtm,
                target,
                i18n::t(lang, Key::AsrLangAuto),
                TAG_ASR_LANG_BASE,
                cfg.asr_lang == AsrLang::Auto,
            ),
            item(
                mtm,
                target,
                &i18n::thai_option(lang, thai_state),
                TAG_ASR_LANG_BASE + 1,
                // **只有模型真的就绪时才显示勾**。配置里写着 Thai 但模型
                // 被删了，勾上就是在骗人——那种状态下一录音就报错。
                cfg.asr_lang == AsrLang::Thai && thai_state == download::State::Ready,
            ),
        ],
    );
    menu.addItem(&asr_item);

    // —— 输入设备 ——
    let devices = crate::audio::list_input_devices();
    let default_name = crate::audio::default_input_name().unwrap_or_else(|| "?".into());
    let mut dev_items = vec![item(
        mtm,
        target,
        &i18n::system_default_device(lang, &default_name),
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

    let dev_item = item(mtm, target, i18n::t(lang, Key::InputDeviceSection), -1, false);
    dev_item.setEnabled(true);
    submenu(mtm, &dev_item, dev_items);
    menu.addItem(&dev_item);

    // —— 自动上屏 ——
    menu.addItem(&item(
        mtm,
        target,
        i18n::t(lang, Key::AutoPaste),
        TAG_AUTO_PASTE,
        cfg.auto_paste,
    ));

    // —— 设置…（原生窗口）——
    //
    // 2026-09-23（jason 拍板）：菜单栏越堆越长，「自动上屏」以下那一串
    // （纠错开关/边车状态/保留期/术语表/指令表/数据目录/日志）挪进一个
    // 独立的原生设置窗口，顶层菜单只留「自动上屏」和更急的那几项。
    // 详见 `open_settings_window`。
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(
        mtm,
        target,
        i18n::t(lang, Key::OpenSettings),
        TAG_OPEN_SETTINGS,
        false,
    ));

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(mtm, target, i18n::t(lang, Key::Quit), TAG_QUIT, false));
}

/// 模式子菜单的**纯**内容：`(标题, tag, 是否勾选)`。
///
/// 抽出来是为了可测——菜单本身跑在 AppKit 主线程上、点不了，而这里最容易出的错
/// **恰恰是 tag 和模式的对应关系写反**（点「输入法」却切到「对话」），
/// 那种错误靠人眼看一眼菜单是发现不了的（两项都在，勾选也对，就是行为反了）。
///
/// 清单来自 `TalkMode::ALL` 的**顺序**，所以「枚举顺序」和「tag 偏移」是同一份事实。
fn mode_menu_entries(lang: Lang, current: config::TalkMode) -> Vec<(String, isize, bool)> {
    config::TalkMode::ALL
        .iter()
        .enumerate()
        .map(|(i, mode)| {
            let key = match mode {
                config::TalkMode::InputMethod => Key::ModeInputMethod,
                config::TalkMode::Conversation => Key::ModeConversation,
            };
            (
                i18n::t(lang, key).to_string(),
                TAG_MODE_BASE + i as isize,
                current == *mode,
            )
        })
        .collect()
}

/// 从菜单 tag 反推模式。**和 `mode_menu_entries` 共用同一份 `ALL`**，
/// 两边不可能各写一套偏移。
fn mode_for_tag(tag: isize) -> Option<config::TalkMode> {
    let index = tag.checked_sub(TAG_MODE_BASE)?;
    usize::try_from(index)
        .ok()
        .and_then(|i| config::TalkMode::ALL.get(i).copied())
}

/// 切模式。**副作用只有一份实现**，在 `crate::set_mode` 里
/// （写配置 / 掐播放 / 拉起边车）。这里只做「点的是哪一项」的判断。
fn set_talk_mode(mode: config::TalkMode) {
    crate::set_mode(mode);
}

/// 数据目录。菜单里要用它把音色库目录解析成绝对路径。
fn store_root() -> PathBuf {
    let mut root = DATA_ROOT.get().cloned().unwrap_or_default();
    if root.as_os_str().is_empty() {
        root = crate::data_root().unwrap_or_default();
    }
    root
}

/// 音色库目录里的音色名（`<name>.wav`）。
///
/// **直接扫目录**，不在代码里写死清单：用户丢一个自己的 `my.wav` + `my.json` 进去，
/// 重启后菜单里就能选。写死清单等于「支持自定义音色」这句话是假的。
fn list_voices(cfg: &config::Config, root: &std::path::Path) -> Vec<String> {
    let dir = cfg.voices_dir(root);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "wav").unwrap_or(false))
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn handle(tag: isize, mtm: MainThreadMarker) {
    // 模式切换单独处理：它的副作用不止改配置（要掐播放、要开关会话）。
    if let Some(mode) = mode_for_tag(tag) {
        set_talk_mode(mode);
        return;
    }
    // 语系 / 语气 / 音色：都只是改配置，下一轮生效（不需要重启任何东西）。
    if let Some(i) = tag.checked_sub(TAG_STYLE_BASE) {
        if let Some(opt) = crate::talk::STYLE_OPTIONS.get(i as usize) {
            config::update(|c| c.tts_style = opt.0.to_string());
            log::info!("语系：{}（{}）", opt.1, opt.0);
            return;
        }
    }
    if let Some(i) = tag.checked_sub(TAG_TONE_BASE) {
        if let Some(opt) = crate::talk::TONE_OPTIONS.get(i as usize) {
            config::update(|c| c.tts_tone = opt.0.to_string());
            log::info!("语气：{}（{}）", opt.1, opt.0);
            return;
        }
    }
    if let Some(i) = tag.checked_sub(TAG_VOICE_BASE) {
        let cfg = config::get();
        if let Some(name) = list_voices(&cfg, &store_root()).get(i as usize) {
            let name = name.clone();
            config::update(|c| c.tts_voice = Some(name.clone()));
            log::info!("音色：{name}");
            return;
        }
    }
    match tag {
        TAG_TOGGLE => crate::hotkey::trigger_now(),
        TAG_AUTO_PASTE => {
            let on = !config::get().auto_paste;
            config::update(|c| c.auto_paste = on);
            crate::paste::set_enabled(on && crate::hotkey::is_accessibility_trusted());
            log::info!("自动上屏：{}", if on { "开" } else { "关" });
        }
        TAG_CORRECT_TERMS => {
            let on = !config::get().correct_terms;
            config::update(|c| c.correct_terms = on);
            log::info!("技术术语纠错：{}", if on { "开（下次录音生效）" } else { "关" });
            if on && !crate::correct::service_reachable() {
                log::warn!("  ⚠️ 纠错服务没在跑，先跑 scripts/serve-llm.sh，否则每次录音会白等一次超时");
            }
        }
        TAG_LAUNCH_AT_LOGIN => {
            let on = !config::get().launch_at_login;
            config::update(|c| c.launch_at_login = on);
            log::info!("开机自动启动：{}", if on { "开" } else { "关" });
            // 文件 I/O 没必要卡住点击这一下——设置窗口里其它控件照样能点。
            // `apply()` 自己现读配置（上面这行 `config::update` 已经落盘），
            // 不用把 `on` 带进闭包。
            std::thread::spawn(crate::launch_agent::apply);
        }
        TAG_START_SIDECAR => {
            let cfg = config::get();
            let url = cfg.llm_url.clone().unwrap_or_else(|| crate::correct::DEFAULT_URL.to_string());
            let autostart = cfg.llm_autostart;
            let command = cfg.llm_start_command.clone();
            // **后台拉起**：模型要加载几十秒，卡在主线程会冻住整个菜单栏。
            std::thread::spawn(move || {
                log::info!("从菜单拉起边车……");
                if crate::sidecar::ensure_available(&url, autostart, &command) {
                    log::info!("边车已就绪");
                } else {
                    log::error!("边车拉不起来，看日志里的原因");
                }
            });
        }
        TAG_OPEN_COMMANDS => {
            // 同 OpenTerms：这是 AppKit 主线程，**不在这里解析文件**，
            // 只保证它存在、然后交给系统的默认编辑器。
            let Some(root) = DATA_ROOT.get() else {
                log::error!("数据目录未初始化");
                return;
            };
            let path = crate::commands::path_in(root);
            if !path.exists() {
                // 第一次点：把默认表写出来，用户改的就是它
                if let Err(e) = crate::commands::save(root, &crate::commands::default_commands()) {
                    log::error!("写默认指令表失败: {e:#}");
                    return;
                }
            }
            if let Err(e) = std::process::Command::new("/usr/bin/open")
                .arg("-t")
                .arg(&path)
                .spawn()
            {
                log::error!("打开指令表失败: {e}");
            } else {
                log::info!("已打开指令表 {}", path.display());
            }
        }
        TAG_OPEN_TERMS => {
            let Some(root) = DATA_ROOT.get() else {
                log::error!("数据目录未初始化");
                return;
            };
            let path = crate::terms::path_in(root);
            // **不在这里调完整的 `load`。**
            //
            // 这是 AppKit 主线程：`load` 会读整个文件、解析 JSON，
            // 文件缺失时还要写盘并 fsync。慢磁盘或误编辑出的超大文件
            // 会直接冻住菜单和整个界面。
            //
            // 启动时 `main` 已经确保过文件存在，所以常见路径只要一次 stat。
            // 真不存在才补一次——那种情况本来就罕见。
            if !path.exists() {
                log::info!("术语表还不存在，先创建默认表");
                let _ = crate::terms::load(root);
            }
            // 仍然不存在说明创建失败（目录只读、磁盘满……），
            // 这时 `open` 一个不存在的路径只会弹一个看不懂的系统错误框。
            if path.exists() {
                open_path(Some(path));
            } else {
                log::error!("无法创建术语表 {}，不打开", path.display());
            }
        }
        TAG_OPEN_DATA => open_path(DATA_ROOT.get().cloned()),
        TAG_OPEN_LOG => open_path(DATA_ROOT.get().map(|r| r.join("agentear.log"))),
        TAG_OPEN_SETTINGS => open_settings_window(mtm),
        TAG_QUIT => {
            log::info!("从菜单退出");
            // 收拾**我们自己拉起的**边车。不是我们拉起的一律不动——
            // 用户可能自己开着终端跑服务，退出时把它杀了是很难排查的越权。
            crate::sidecar::shutdown();
            crate::talk::shutdown_spawned();
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
            let (days, _) = RETENTION_CHOICES[(t - TAG_RETENTION_BASE) as usize];
            config::update(|c| c.retention_days = days);
            log::info!("原始音频保留期改为 {days} 天（0 = 永不清理）");
        }
        t if (TAG_UI_LANG_BASE..TAG_UI_LANG_BASE + Lang::ALL.len() as isize).contains(&t) => {
            let want = Lang::ALL[(t - TAG_UI_LANG_BASE) as usize];
            config::update(|c| c.ui_lang = want);
            // 不需要重启：菜单每次展开都走 menuNeedsUpdate: 重建，
            // 菜单栏标题由 0.5s 定时器刷新。但**当前这个已经打开的菜单
            // 不会原地重绘**——点完它就关了，下次展开才是新语言。
            log::info!("界面语言改为 {}（下次展开菜单生效）", want.endonym());
        }
        TAG_ASR_LANG_BASE => {
            // 取消意图和落配置必须在**同一把锁**里，否则下载线程可能
            // 已经读到了 true、正卡在两步之间，随后把 Thai 写回去。
            let mut intent = THAI_INTENT.lock().unwrap();
            *intent = false;
            config::update(|c| c.asr_lang = AsrLang::Auto);
            drop(intent);
            log::info!("识别语言改为自动（中/英/日/韩/粤，下次录音生效）");
        }
        t if t == TAG_ASR_LANG_BASE + 1 => {
            match download::state(&download::THAI) {
                download::State::Ready => {
                    config::update(|c| c.asr_lang = AsrLang::Thai);
                    log::info!("识别语言改为泰语（下次录音生效）");
                }
                download::State::Downloading(_) | download::State::Verifying => {
                    // 再点一次泰语 = 重新表达「下完就切」的意图
                    // （用户可能中途点过 Auto 又反悔）
                    *THAI_INTENT.lock().unwrap() = true;
                    log::info!("泰语模型正在下载/验证中，完成后会自动切过去");
                }
                // 没下过、或者上次失败了 —— 两种都是「点一下开始下」。
                //
                // ⚠️ **这里故意不改 `asr_lang`。** 下载要几分钟，
                // 期间把识别语言设成泰语的话，用户这几分钟里每次录音
                // 都会失败，而失败只写在日志里。
                // 配置在下载完成**并通过加载冒烟之后**才提交，
                // 见 `asr::finish_thai_install`。
                _ => {
                    *THAI_INTENT.lock().unwrap() = true;
                    log::info!("开始下载泰语模型（574 MB）");
                    download::start(
                        &download::THAI,
                        crate::asr::verify_thai_model,
                        on_thai_installed,
                    );
                }
            }
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

// —— 设置窗口 ——
//
// **一个进程只留一个设置窗口**：`thread_local!` 存着上次建好的那个，
// 再点「设置…」就直接把它调到前台、重建内容（读最新配置），不会一点
// 一个、点几次就叠出一摞重复窗口。用 `thread_local!` 而不是 `static`
// 是因为 `Retained<NSWindow>` 这类 `MainThreadOnly` 类型本来就不是
// `Send`/`Sync`——`thread_local!` 不要求这个，`static` 要求。反正这个
// 值只会在主线程被摸到（`MainThreadMarker` 保证），跟线程本地存储的
// 语义正合适。
thread_local! {
    static SETTINGS_WINDOW: std::cell::RefCell<Option<Retained<NSWindow>>> =
        const { std::cell::RefCell::new(None) };
    /// 菜单栏那份 `MenuTarget` 的一份拷贝（`Retained` 只是加了个引用计数，
    /// 不是深拷贝）。设置窗口的控件要挂 target 时从这里取，**不新建一个**
    /// ——分发逻辑全在自由函数 `handle()` 里，target 只是 selector 落点，
    /// 多一个实例没有任何意义，只会多一份要管的生命周期。
    /// 在 `install()` 里、菜单栏那份建好的同一刻写入。
    static CURRENT_TARGET: std::cell::RefCell<Option<Retained<MenuTarget>>> =
        const { std::cell::RefCell::new(None) };
}

const SETTINGS_WIDTH: f64 = 420.0;
const ROW_H: f64 = 26.0;
const ROW_GAP: f64 = 12.0;
const MARGIN: f64 = 20.0;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn checkbox(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    title: &str,
    tag: isize,
    checked: bool,
    y: f64,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            Some(AsRef::<AnyObject>::as_ref(target)),
            Some(sel!(onControl:)),
            mtm,
        )
    };
    b.setFrame(rect(MARGIN, y, SETTINGS_WIDTH - 2.0 * MARGIN, ROW_H));
    b.setTag(tag);
    b.setState(if checked { NSControlStateValueOn } else { NSControlStateValueOff });
    b
}

fn action_button(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    title: &str,
    tag: isize,
    y: f64,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(AsRef::<AnyObject>::as_ref(target)),
            Some(sel!(onControl:)),
            mtm,
        )
    };
    b.setFrame(rect(MARGIN, y, SETTINGS_WIDTH - 2.0 * MARGIN, ROW_H));
    b.setTag(tag);
    b
}

fn info_label(mtm: MainThreadMarker, text: &str, y: f64) -> Retained<NSTextField> {
    let f = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    f.setFrame(rect(MARGIN + 18.0, y, SETTINGS_WIDTH - 2.0 * MARGIN - 18.0, ROW_H));
    f
}

/// 保留期下拉框。**内部菜单项不挂 target/action**——只有 `NSPopUpButton`
/// 自己那一份 target/action 会触发（挂在弹出的那些 `NSMenuItem` 上会跟
/// 外层重复触发，行为对不上）。选中哪项靠 `onPopup:` 读 `selectedTag()`。
fn retention_popup(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    lang: Lang,
    current_days: u32,
    y: f64,
) -> Retained<NSPopUpButton> {
    let popup = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        rect(MARGIN + 100.0, y, SETTINGS_WIDTH - 2.0 * MARGIN - 100.0, ROW_H),
        false,
    );
    let menu = NSMenu::init(NSMenu::alloc(mtm));
    for (i, (days, key)) in RETENTION_CHOICES.iter().enumerate() {
        let it = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(i18n::t(lang, *key)),
                None,
                &NSString::from_str(""),
            )
        };
        it.setTag(TAG_RETENTION_BASE + i as isize);
        menu.addItem(&it);
        let _ = days;
    }
    popup.setMenu(Some(&menu));
    unsafe {
        popup.setTarget(Some(AsRef::<AnyObject>::as_ref(target)));
        popup.setAction(Some(sel!(onPopup:)));
    }
    if let Some(i) = RETENTION_CHOICES.iter().position(|(d, _)| *d == current_days) {
        popup.selectItemAtIndex(i as isize);
    }
    popup
}

/// 弹出（或调到前台）设置窗口。**每次都重建内容**——跟菜单
/// `menuNeedsUpdate:` 同一个做法：设置窗口不常开，重建的成本远低于
/// 维护一份「点开后还要不要跟着配置活刷新」的状态。
fn open_settings_window(mtm: MainThreadMarker) {
    let cfg = config::get();
    let lang = cfg.ui_lang;

    let window = SETTINGS_WINDOW.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(w) = slot.as_ref() {
            return w.clone();
        }
        let w = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, SETTINGS_WIDTH, 1.0),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { w.setReleasedWhenClosed(false) }; // 关窗口不能把它释放掉，下次还要用同一个实例
        *slot = Some(w.clone());
        w
    });
    window.setTitle(&NSString::from_str(i18n::t(lang, Key::SettingsTitle)));

    build_settings_content(mtm, &window, &cfg, lang);

    window.center();
    window.makeKeyAndOrderFront(None);
    // ⚠️ **不能用 `NSApplication::activate()`**：那是 macOS 14+ 才有的方法，
    // `Info.plist` 的 `LSMinimumSystemVersion` 写的是 11.0——真在老系统上
    // 跑会是「unrecognized selector」直接崩溃（codex 复查抓到的）。
    // `activateIgnoringOtherApps:` 虽然标了 deprecated，但从 10.0 就有，
    // 兼容面覆盖到 11.0——**这里正确性优先于消掉一条 deprecation 警告**。
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn build_settings_content(
    mtm: MainThreadMarker,
    window: &NSWindow,
    cfg: &config::Config,
    lang: Lang,
) {
    // 这里拿到的 target 必须是菜单栏那份 `MenuTarget`，不能新建一个——
    // 新建的话点按钮时 `on_control`/`on_popup` 是活的，但它读的全局状态
    // （`DATA_ROOT` 等）没问题，问题在**多一个 target 实例本身没有意义**：
    // 所有分发逻辑都在自由函数 `handle()` 里，target 只是个 selector 落点。
    // 所以从菜单栏拿现成的那份。
    let target = CURRENT_TARGET.with(|t| t.borrow().clone()).expect("菜单栏必须先装好");

    let reachable = crate::sidecar::is_ready();
    let correct_key = if reachable { Key::CorrectTerms } else { Key::CorrectTermsOffline };
    // 和 `populate()` 里菜单那份完全同一套判据（codex 复查抓到：这里原来
    // 全部渲染成不可点的 `NSTextField`，`TAG_START_SIDECAR` 变成了死代码，
    // 但文案还留着"点击拉起"，跟按钮那种一样，不点开源码看不出区别）。
    let sidecar_line: Option<(String, isize)> = if cfg.correct_terms {
        let (key, tag) = match crate::sidecar::health() {
            crate::sidecar::Health::Up => (Key::SidecarUp, -1),
            crate::sidecar::Health::WrongService => (Key::SidecarWrongService, -1),
            crate::sidecar::Health::Down => {
                if cfg.llm_autostart && !cfg.llm_start_command.is_empty() {
                    (Key::SidecarDown, TAG_START_SIDECAR)
                } else {
                    (Key::SidecarDown, -1)
                }
            }
        };
        Some((i18n::t(lang, key).to_string(), tag))
    } else {
        None
    };

    // 行数固定：勾选×2 + 边车状态(可能为空) + 保留期 + 4 个按钮。
    // 边车状态那一行**即使是空文案也占位**——用固定行数换布局代码简单，
    // 空标签不可见，视觉上跟"少一行"没区别。
    let rows = 2 + 1 + 1 + 4;
    let content_h = MARGIN * 2.0 + rows as f64 * ROW_H + (rows - 1) as f64 * ROW_GAP;
    window.setContentSize(NSSize::new(SETTINGS_WIDTH, content_h));

    let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, SETTINGS_WIDTH, content_h));

    let mut y = content_h - MARGIN - ROW_H;
    let mut next_row = || {
        let cur = y;
        y -= ROW_H + ROW_GAP;
        cur
    };

    content.addSubview(&checkbox(
        mtm,
        &target,
        i18n::t(lang, Key::LaunchAtLogin),
        TAG_LAUNCH_AT_LOGIN,
        cfg.launch_at_login,
        next_row(),
    ));
    content.addSubview(&checkbox(
        mtm,
        &target,
        i18n::t(lang, correct_key),
        TAG_CORRECT_TERMS,
        cfg.correct_terms,
        next_row(),
    ));
    {
        let row_y = next_row();
        match &sidecar_line {
            Some((text, tag)) if *tag >= 0 => {
                content.addSubview(&action_button(mtm, &target, text, *tag, row_y));
            }
            Some((text, _)) => content.addSubview(&info_label(mtm, text, row_y)),
            None => content.addSubview(&info_label(mtm, "", row_y)),
        }
    }
    {
        let row_y = next_row();
        content.addSubview(&info_label(mtm, i18n::t(lang, Key::RetentionSection), row_y));
        content.addSubview(&retention_popup(mtm, &target, lang, cfg.retention_days, row_y));
    }
    content.addSubview(&action_button(
        mtm,
        &target,
        i18n::t(lang, Key::OpenTerms),
        TAG_OPEN_TERMS,
        next_row(),
    ));
    content.addSubview(&action_button(
        mtm,
        &target,
        i18n::t(lang, Key::OpenCommands),
        TAG_OPEN_COMMANDS,
        next_row(),
    ));
    content.addSubview(&action_button(
        mtm,
        &target,
        i18n::t(lang, Key::OpenDataDir),
        TAG_OPEN_DATA,
        next_row(),
    ));
    content.addSubview(&action_button(
        mtm,
        &target,
        i18n::t(lang, Key::ViewLog),
        TAG_OPEN_LOG,
        next_row(),
    ));

    window.setContentView(Some(&content));
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
        button.setTitle(&NSString::from_str(&title(config::get().ui_lang)));
    }

    let target = MenuTarget::new(mtm);
    CURRENT_TARGET.with(|t| *t.borrow_mut() = Some(target.clone()));
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
                // 顺便在后台刷新一次边车健康状态。**不能在主线程探**
                // （见 populate 里的说明），所以丢给一个线程去做，
                // 主线程只读结果。用 CAS 保证同一时刻只有一个探测在飞。
                {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static PROBING: AtomicBool = AtomicBool::new(false);
                    if config::get().correct_terms
                        && PROBING
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        std::thread::spawn(move || {
                            let cfg = config::get();
                            let url = cfg
                                .llm_url
                                .unwrap_or_else(|| crate::correct::DEFAULT_URL.to_string());
                            crate::sidecar::probe(&url);
                            PROBING.store(false, Ordering::SeqCst);
                        });
                    }
                }
                let mtm = MainThreadMarker::new_unchecked();
                if let Some(button) = item_for_timer.button(mtm) {
                    // 每次都重读语言，这样切换后标题最多 0.5s 就跟上
                    button.setTitle(&NSString::from_str(&title(config::get().ui_lang)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TalkMode;

    /// **点哪一项就切到哪个模式**——这是这次菜单唯一不能靠人眼看出来的错法。
    #[test]
    fn mode_menu_tag_roundtrips_to_the_right_mode() {
        for mode in TalkMode::ALL.iter().copied() {
            let entries = mode_menu_entries(Lang::Zh, mode);
            let (_, tag, checked) = entries
                .iter()
                .find(|(_, _, checked)| *checked)
                .expect("当前模式必须被勾上");
            assert_eq!(
                mode_for_tag(*tag),
                Some(mode),
                "tag {tag} 反推出来的模式和勾选的那一项不一致（点它会切错）"
            );
        }
    }

    /// 两项都要在菜单里，而且**恰好一项被勾选**。
    /// 单开关式的「对话模式」勾选框会让人不知道自己现在在哪个模式里。
    #[test]
    fn mode_menu_lists_every_mode_and_checks_exactly_one() {
        for lang in Lang::ALL.iter().copied() {
            let entries = mode_menu_entries(lang, TalkMode::Conversation);
            assert_eq!(entries.len(), TalkMode::ALL.len());
            assert_eq!(entries.iter().filter(|(_, _, c)| *c).count(), 1);
            for (title, _, _) in &entries {
                assert!(!title.trim().is_empty(), "空标题的菜单项等于没有这一项");
            }
        }
    }

    /// 三种语言的文案不能撞车：撞了就等于没有区分。
    #[test]
    fn mode_titles_differ_per_language_and_per_mode() {
        for lang in Lang::ALL.iter().copied() {
            let entries = mode_menu_entries(lang, TalkMode::InputMethod);
            assert_ne!(entries[0].0, entries[1].0, "{lang:?} 下两个模式标题相同");
        }
    }

    /// 模式小标记：**只在对话模式加**，而且不能把「正在录音」那个更急的指示挤掉。
    #[test]
    fn mode_marker_is_appended_only_in_conversation() {
        let marker = |mode: config::TalkMode, base: &str| match mode {
            config::TalkMode::Conversation => format!("{base}💬"),
            config::TalkMode::InputMethod => base.to_string(),
        };
        use config::TalkMode::{Conversation, InputMethod};
        assert_eq!(marker(InputMethod, "🎙"), "🎙", "默认模式不该多背一个字符");
        assert_eq!(marker(Conversation, "🎙"), "🎙💬");
        // 录音中：计时在前、标记在后，计时不被挤掉
        assert_eq!(marker(Conversation, "● 7s"), "● 7s💬");
        assert!(marker(Conversation, "● 7s").starts_with("● 7s"));
    }

    /// 不属于模式区的 tag 一律返回 None——否则别的菜单项会被误当成模式切换。
    #[test]
    fn unrelated_tags_are_not_modes() {
        for tag in [TAG_TOGGLE, TAG_QUIT, TAG_MODE_BASE - 1, TAG_MODE_BASE + 99] {
            assert_eq!(mode_for_tag(tag), None, "tag {tag} 不该被当成模式");
        }
    }
}
