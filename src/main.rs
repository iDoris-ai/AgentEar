//! AgentEar M1：按快捷键录音 → raw 落盘 → 转写 → 剪贴板。
//!
//! 范围严格限定在 `docs/milestones.md` 的 M1：不含 LLM、标签路由、TTS。
//! M1 恰好绕开了 AEC 和无边界流式 raw 语义两个难点——快捷键的按下/再按
//! 天然给出段边界，每次录音就是一个有头有尾的文件对象。**不要在 M1 里
//! 提前引入 TTS 或无边界流。**

mod asr;
mod download;
mod audio;
mod config;
mod correct;
mod deliver;
mod hotkey;
mod i18n;
mod kb;
mod label;
mod paste;
mod route;
mod sidecar;
mod store;
mod terms;
mod tray;

use crate::kb::KbSink;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

enum State {
    Idle,
    Recording {
        session: store::Session,
        recorder: audio::Recorder,
        started: Instant,
        /// 这一段录音用哪个引擎转，**在按下录音键那一刻就定死**。
        ///
        /// 菜单说的是「下次录音生效」，那就得说到做到：录到一半时用户
        /// 改了识别语言、或者泰语模型刚好下载完成自动切了过去，
        /// 都不该改变**这一段**音频的去向。不快照的话，`finish` 是在
        /// 录音结束后才读配置的，那时读到的已经是新值了。
        asr_lang: asr::AsrLang,
    },
}

fn main() -> Result<()> {
    init_logging();

    let args: Vec<String> = std::env::args().collect();
    let vendor = vendor_root()?;
    log::debug!("vendor 目录: {}", vendor.display());

    // 泰语链路的加载冒烟跑在下载线程上，那里拿不到下面这个 `asr` 实例，
    // 所以 vendor 路径单独存一份。必须在任何可能触发下载的东西之前设好。
    asr::set_vendor(vendor.clone());

    // 数据目录和配置要在**所有子命令之前**就绪：`--transcribe --lang th`
    // 得能找到下载好的泰语模型（在数据目录里），也得读得到配置。
    // 早期版本把这两步放在子命令后面，于是离线转写永远走默认配置——
    // 那种错很难发现，因为默认配置恰好是大多数情况下对的那个。
    let data_root = data_root()?;
    download::set_data_root(data_root.clone());
    // 术语表在**启动时**就确保存在，不等到第一次纠错。
    //
    // 早先只在纠错路径里 load，而纠错默认是关的——于是「首次启动写入默认表」
    // 这条规格实际上从没发生过：用户想去编辑术语表，会发现文件根本不在。
    // 失败只记日志，不阻断启动（同 config 的策略）。
    let _ = terms::load(&data_root);
    let cfg = config::load(&data_root);

    // 引擎指纹要在对账之前设好——`download::is_installed` 拿它判断
    // 「验过这个模型的引擎，是不是现在这一个」。
    match asr::engine_fingerprint() {
        Some(id) => {
            log::debug!("泰语引擎指纹 {id}");
            download::set_engine_id(id);
        }
        None => log::debug!("vendor 里没有泰语引擎，泰语功能不可用"),
    }

    // **配置和实际安装状态对账。**
    //
    // 配置里写着泰语，但模型可能已经被删了、被换过、或者当初压根没装完。
    // 不对账的话，菜单只是不显示勾，而**每一次录音都会走泰语分支然后失败**，
    // 错误只出现在日志里——用户看到的是「按了键，什么都没出来」。
    // 宁可退回自动（那条链路的模型随包走，一定在），并把原因写清楚。
    let cfg = if cfg.asr_lang == asr::AsrLang::Thai && !download::is_installed(&download::THAI) {
        // 「没装好」涵盖三种：模型不在、模型坏了、**以及引擎换了**——
        // 升级把 whisper-cli 换成不兼容的版本时，旧的冒烟结果不再作数
        // （安装记录绑定了引擎指纹）。三种的处置一样：退回自动。
        log::warn!("配置里选的是泰语，但泰语模型现在不可用（缺失、损坏，或引擎已更换）");
        log::warn!("  已退回自动识别。要用泰语：菜单「识别语言 → ไทย」，或跑 --fetch-thai");
        config::update(|c| c.asr_lang = asr::AsrLang::Auto);
        config::get()
    } else {
        cfg
    };

    let asr = asr::Asr::new(&vendor)?;
    log::debug!("ASR 依赖检查通过");

    // 离线转写一个已有的 wav，不占麦克风，用于验证 ASR 链路。
    //
    // `--lang th` 可以在不改配置的情况下试泰语链路——排查「是模型的问题还是
    // 录音的问题」时，不该逼用户先去菜单里改设置再改回来。
    if args.len() >= 3 && args[1] == "--transcribe" {
        // 取 `--lang` **紧跟着的那个值**，不是「参数里出现过 th 就算」——
        // 后者会把 `--transcribe th.wav` 里的文件名当成语言选择。
        // 写错了就报错退出，不静默用配置里的值：排障时最怕的就是
        // 「我明明指定了泰语」而它其实走了别的引擎。
        let lang = match args.iter().position(|a| a == "--lang") {
            Some(i) => match args.get(i + 1).map(String::as_str) {
                Some("th") | Some("thai") => asr::AsrLang::Thai,
                Some("auto") => asr::AsrLang::Auto,
                Some(other) => anyhow::bail!("--lang 只认 th / auto，收到 {other:?}"),
                None => anyhow::bail!("--lang 后面要跟语言（th 或 auto）"),
            },
            None => config::get().asr_lang,
        };
        let t0 = Instant::now();
        let t = asr.transcribe(std::path::Path::new(&args[2]), lang)?;
        // 离线转写也走一遍纠错，否则「开了纠错但效果不对」这类问题
        // 只能靠反复录音来复现。配置关着就跳过，行为和守护进程一致。
        if cfg.correct_terms && !t.text.is_empty() {
            let url = cfg.llm_url.as_deref().unwrap_or(correct::DEFAULT_URL);
            // 同 --classify：只探不拉，但必须探，否则门控会挡下一切
            sidecar::probe(url);
            let tb = terms::load(&data_root);
            if let Some(fixed) = correct::Corrector::with_terms(url, &tb).correct(&t.text) {
                if fixed != t.text {
                    println!("{fixed}");
                    eprintln!("（纠错前：{}）", t.text);
                    eprintln!(
                        "（语种 {}，耗时 {:.2}s）",
                        t.lang.as_deref().unwrap_or("?"),
                        t0.elapsed().as_secs_f32()
                    );
                    return Ok(());
                }
            }
        }
        println!("{}", t.text);
        eprintln!(
            "（语种 {}，耗时 {:.2}s）",
            t.lang.as_deref().unwrap_or("?"),
            t0.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    // 先把泰语模型下下来，不必等到在菜单里点。
    //
    // 存在的理由有三个：想在有网的时候提前下好；菜单那条路出问题时的
    // 备用入口；以及排障时能看到完整的失败原因——菜单里只显示
    // 「失败（网络）」五个字，这里能看到 curl 的退出码。
    if args.len() == 2 && args[1] == "--fetch-thai" {
        println!("下载泰语模型（{:.0} MB）…", download::THAI.bytes as f64 / 1e6);
        // **只装，不选。** 这条命令的语义是「先把模型下好」，
        // 不该顺手改掉用户的识别语言——预下载和「我要开始用泰语」
        // 是两件事。选择留给菜单（或用户自己改配置）。
        download::start(&download::THAI, asr::verify_thai_model, || {});
        // 下载跑在后台线程上，这里等它。**每秒打一次进度**——
        // 574 MB 在慢网上要十几分钟，一个不动的光标会让人以为卡死了。
        loop {
            match download::state(&download::THAI) {
                download::State::Downloading(pct) => {
                    print!("\r  {pct}%   ");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                download::State::Ready => {
                    println!("\r  ✅ 已安装。到菜单「识别语言 → ไทย」选用它");
                    return Ok(());
                }
                download::State::Failed(f) => {
                    println!();
                    anyhow::bail!("下载失败：{f}（详情见日志）");
                }
                download::State::Verifying => {
                    print!("\r  验证中……");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                download::State::Absent => {
                    // 线程刚起来还没把状态置上，再等一轮
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    // 从 `routes/` 全量重建知识库。
    //
    // 这是 ADR-0003 §7「L1 文档层可以从 L0 事实层全量重放」的可执行证明。
    // 三种场景都只有这一条出路：手滑删了 `kb/` 想重来、换了适配器要迁移、
    // 修好 bug 要补投之前失败的。**投递是幂等的，所以反复跑是安全的。**
    if args.iter().any(|a| a == "--replay-kb") {
        let store = store::Store::open(&data_root)?;
        let kb_root = cfg.kb_root(&data_root);
        println!("从 {} 重建 → {}", store.root().join("routes").display(), kb_root.display());
        let sink = kb::FileSink::new(store.root(), &kb_root);
        let st = deliver::replay(&store, &sink)?;
        println!(
            "完成：投递 {} 条，跳过 {} 条（unknown / command 不进知识库），失败 {} 条",
            st.delivered, st.skipped, st.failed
        );
        // 有失败就用非零退出码，脚本里能判
        if st.failed > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    // 给一段文字分类，输出一级标签。
    //
    // 存在的理由不只是排障：`spike/m2_bench.py` 的标签评测**必须走这条路**，
    // 否则它自带一份提示词和解析器，报出来的分数和产品实际行为对不上。
    // 那个坑真的踩过——基准报 18/18 而生产 17/18，差异稳定复现却查不出根因
    // （`docs/benchmarks-m2.md` §9）。评测和产品共用同一段代码，
    // 这类疑问从根上就不会出现。
    if args.len() == 3 && args[1] == "--classify" {
        let url = cfg.llm_url.as_deref().unwrap_or(correct::DEFAULT_URL);
        // 一次性命令**只探不拉**：边车冷启动要几十秒，为一句分类去拉起
        // 不合理。但必须探一次——门控读的是全局健康状态，
        // 而这条路径没有守护进程那套 ensure_available 去填它。
        sidecar::probe(url);
        let r = label::Classifier::new(url).classify(&args[2]);
        // 只输出类名，便于脚本消费；来源走 stderr，不污染 stdout
        println!("{}", r.label.as_str());
        eprintln!(
            "（来源：{}）",
            match r.source {
                label::Source::Explicit => "用户明说的显式标记",
                label::Source::Model => "模型推断",
            }
        );
        return Ok(());
    }

    // 环境自检，排查「按了没反应」
    if args.len() == 2 && args[1] == "--diagnose" {
        return diagnose(&vendor);
    }

    // 打印每一个修饰键事件，确认按键到底有没有被收到
    if args.iter().any(|a| a == "--debug-keys") {
        hotkey::set_debug_keys(true);
        log::info!("已开启按键调试：将打印每一个 flagsChanged 事件");
    }

    let store = store::Store::open(&data_root)?;
    log::info!("数据目录: {}", store.root().display());
    tray::set_data_root(data_root.clone());
    log::debug!("配置: {cfg:?}");

    // 启动时清一次过期 raw。之后由工作线程每 6 小时再查一次——守护进程
    // 一开就是几周，只在启动时清等于对常开的机器不生效。
    if let Err(e) = store.purge_older_than(cfg.retention_days) {
        log::error!("清理过期 raw 音频失败: {e:#}");
    }

    // 上次没投成的，这次启动补上（ADR-0003 §4.2）。
    //
    // 放在这里而不是工作线程里：补投只读写本地文件，几毫秒的事，
    // 而放到线程里会和第一次录音抢同一批 route 文件。
    if cfg.kb_enabled {
        let sink = kb::FileSink::new(store.root(), cfg.kb_root(&data_root));
        if let Err(e) = sink.health() {
            log::error!("知识库目录不可用，本次运行只写 routes/: {e:#}");
        } else {
            deliver::drain(&store, &sink);
        }
    }

    // 权限引导：想用右 Command 就必须有辅助功能权限。
    // 用 log:: 而非 println!，因为从 Finder 启动 .app 时 stdout 无处可去。
    if cfg.trigger == config::Trigger::RightCommand && !hotkey::is_accessibility_trusted() {
        log::warn!("未获得「辅助功能」权限，无法监听单独的右 Command 键");
        log::warn!("  正在弹出系统授权对话框——授权后**必须重启本程序**才生效");
        log::warn!("  路径：系统设置 → 隐私与安全性 → 辅助功能");
        log::warn!("  注意：.app 的权限与终端是分开的，各自要授权一次");
        hotkey::prompt_accessibility();
    }

    let mut listener = hotkey::Listener::start(cfg.trigger)?;

    // 自动上屏。配置里的开关优先，`--no-auto-paste` / AGENTEAR_AUTO_PASTE=0
    // 作为一次性覆盖（不写回配置，重启即恢复菜单里的设置）。
    //
    // 同样吃辅助功能权限：CGEventPost 未授权时**静默失败**——不报错、什么也
    // 不发生。所以这里主动降级，不然用户会看到「转写成功但没上屏」且日志无痕。
    let want_paste = cfg.auto_paste
        && !args.iter().any(|a| a == "--no-auto-paste")
        && !matches!(
            std::env::var("AGENTEAR_AUTO_PASTE").as_deref(),
            Ok("0") | Ok("false") | Ok("no")
        );
    let can_paste = want_paste && hotkey::is_accessibility_trusted();
    if want_paste && !can_paste {
        log::warn!("自动上屏需要辅助功能权限，未授予 → 只写剪贴板，请手动 ⌘V");
    }
    paste::set_enabled(can_paste);

    println!("\n╭─────────────────────────────────────────────╮");
    println!("│  AgentEar M1 已就绪                          │");
    println!("╰─────────────────────────────────────────────╯");
    println!("  触发键：{}（按一下开始，再按一下停止）", listener.describe());
    println!(
        "  上屏：  {}",
        if can_paste {
            "自动粘贴到当前窗口（不会替你按回车）"
        } else {
            "仅写剪贴板，手动 ⌘V"
        }
    );
    println!("  数据：  {}", store.root().display());
    println!(
        "  留档：  {}",
        match cfg.retention_days {
            0 => "原始音频永久保留".to_string(),
            d => format!("原始音频保留 {d} 天，过期自动清理"),
        }
    );
    println!("  设置：  菜单栏图标 → 触发键 / 输入设备 / 自动上屏 / 保留期");
    println!("  退出：  菜单栏「退出 AgentEar」或 Ctrl+C\n");

    // macOS 的关键约束：Carbon 快捷键和 NSEvent 全局监听都靠 CFRunLoop 派发事件。
    // 主线程必须跑 run loop，否则事件注册成功但永远送不到——这正是最初
    // 「按 Ctrl+Shift+R 毫无反应」的原因。
    // 所以：状态机放工作线程，主线程只负责 run loop。
    let rx = listener.take_receiver();
    std::thread::spawn(move || {
        if let Err(e) = worker(rx, store, asr) {
            log::error!("工作线程退出: {e:#}");
            // 这条路径也要收拾边车，否则它会活过 AgentEar
            sidecar::shutdown();
            std::process::exit(1);
        }
    });

    // 让 Ctrl+C / SIGTERM 也能收拾边车。**必须在拉起之前注册**，
    // 否则启动过程中收到信号会留下孤儿。
    sidecar::install_signal_handlers();

    // 边车按需拉起。**放后台线程**：拉起要等模型加载（实测冷启动几十秒），
    // 卡在这里会让菜单栏图标迟迟不出现，用户以为程序没启动。
    //
    // 只在纠错开着时才管它——关着的话连探测都省了。
    if cfg.correct_terms {
        let url = cfg.llm_url.clone().unwrap_or_else(|| correct::DEFAULT_URL.to_string());
        let autostart = cfg.llm_autostart;
        let command = cfg.llm_start_command.clone();
        std::thread::spawn(move || {
            if sidecar::ensure_available(&url, autostart, &command) {
                log::info!("边车可用：{url}");
            } else {
                log::warn!("边车不可用，术语纠错和标签识别会降级（文字照常上屏）");
            }
        });
    }

    // 菜单栏必须在主线程装，且要在 NSApplication::run() 之前
    let mtm = objc2::MainThreadMarker::new().expect("install 必须在主线程");
    let _tray = tray::install(mtm);
    log::debug!("菜单栏图标已安装");

    log::debug!("主线程进入 AppKit 事件循环……");
    tray::run(mtm)
}

fn worker(
    rx: std::sync::mpsc::Receiver<()>,
    store: store::Store,
    asr: asr::Asr,
) -> Result<()> {
    let mut state = State::Idle;
    let mut last_heartbeat = Instant::now();
    let mut last_purge = Instant::now();
    /// 过期 raw 的复查间隔。守护进程一开就是几周，只在启动时清等于对
    /// 常开的机器永远不生效。
    const PURGE_EVERY: Duration = Duration::from_secs(6 * 3600);

    loop {
        // 录音期间不做清理：删文件的 IO 会和写 WAV 抢盘,
        // 而这条循环每 20ms 就要把采样搬进 session 一次
        if matches!(state, State::Idle) && last_purge.elapsed() >= PURGE_EVERY {
            last_purge = Instant::now();
            // 每次都重读配置，菜单里改了保留期不用重启
            if let Err(e) = store.purge_older_than(config::get().retention_days) {
                log::error!("清理过期 raw 音频失败: {e:#}");
            }
        }

        // 录音期间持续把采样搬进 session，避免 channel 无限堆积
        if let State::Recording {
            session, recorder, ..
        } = &mut state
        {
            let pcm = recorder.drain();
            if !pcm.is_empty() {
                session.write(&pcm)?;
            }
            // 每秒报一次时长，让「正在录」这件事可见
            tray::set_secs(session.duration_secs() as u32);
            if last_heartbeat.elapsed() >= Duration::from_secs(1) {
                log::info!("● 录音中 {:.0}s", session.duration_secs());
                last_heartbeat = Instant::now();
            }
            if session.duration_secs() > asr::MAX_SEGMENT_SECS {
                log::warn!(
                    "录音超过 {:.0} 秒上限，自动停止（见 ADR-0001 §5）",
                    asr::MAX_SEGMENT_SECS
                );
                state = finish(state, &store, &asr)?;
                continue;
            }
        }

        if rx.try_recv().is_ok() {
            log::debug!("收到触发事件");
            state = match state {
                State::Idle => match begin(&store) {
                    Ok(s) => {
                        last_heartbeat = Instant::now();
                        s
                    }
                    Err(e) => {
                        log::error!("开始录音失败: {e:#}");
                        State::Idle
                    }
                },
                s @ State::Recording { .. } => finish(s, &store, &asr)?,
            };
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn begin(store: &store::Store) -> Result<State> {
    let t0 = Instant::now();
    log::debug!("打开麦克风……（首次运行时 macOS 会在此弹出权限请求）");
    // **一次读完，两处都用这一份快照。**
    //
    // 每次录音才读配置，所以菜单里换设备立刻生效，不用重启。
    // 但设备和识别语言必须来自**同一时刻**：`Recorder::start` 可能要花
    // 好几秒（首次运行时 macOS 在这里弹权限框），期间用户改了识别语言、
    // 或者泰语模型刚好下载完成自动切了过去，事后再读就把这段音频
    // 送去了另一个引擎——而用户按下录音键时看到的还是旧设置。
    let cfg = config::get();
    let recorder = audio::Recorder::start(cfg.input_device.as_deref())?;
    let session = store.begin()?;
    tray::set(tray::Status::Recording);
    println!("● 开始录音…… 再按一次停止");
    log::debug!("录音启动耗时 {:.0}ms", t0.elapsed().as_secs_f32() * 1000.0);
    Ok(State::Recording {
        session,
        recorder,
        started: Instant::now(),
        asr_lang: cfg.asr_lang,
    })
}

fn finish(state: State, store: &store::Store, asr: &asr::Asr) -> Result<State> {
    let State::Recording {
        mut session,
        recorder,
        started,
        asr_lang,
    } = state
    else {
        return Ok(state);
    };

    // 收尾：把停止瞬间还在缓冲里的采样也写进去
    let tail = recorder.drain();
    if !tail.is_empty() {
        session.write(&tail)?;
    }
    drop(recorder);

    let secs = session.duration_secs();
    if secs < 0.3 {
        log::warn!("录音过短（{secs:.1}s），丢弃");
        tray::set(tray::Status::Idle);
        return Ok(State::Idle);
    }
    tray::set(tray::Status::Transcribing);

    // raw 先落盘并走完提交协议，再谈转写。
    // 转写失败不能影响原始音频——这是 README「先存后分流」的执行点。
    let t_commit = Instant::now();
    let committed = session.commit().context("提交 raw 音频失败")?;
    log::debug!(
        "raw 已提交 ({:.0}ms): {}",
        t_commit.elapsed().as_secs_f32() * 1000.0,
        committed.path.display()
    );
    println!("✓ 已保存 {secs:.1}s 录音");

    let t_asr = Instant::now();
    match asr.transcribe(&committed.path, asr_lang) {
        Ok(t) if !t.text.is_empty() => {
            log::debug!(
                "转写耗时 {:.2}s，语种 {}",
                t_asr.elapsed().as_secs_f32(),
                t.lang.as_deref().unwrap_or("?")
            );
            let raw_text = paste::sanitize(&t.text);

            // 术语纠错。**默认关**，开了也只是尽力而为：
            // 边车没起、超时、返回垃圾，一律退回原文继续上屏——
            // 用户宁可拿到一句有错别字的话，也不想按完键什么都没有。
            let cfg = config::get();
            let (text, corrected) = if cfg.correct_terms {
                let t_fix = Instant::now();
                let url = cfg.llm_url.as_deref().unwrap_or(correct::DEFAULT_URL);
                // 每次都重新读术语表：用户改完下次录音即生效，不用重启。
                let tb = terms::load(store.root());
                match correct::Corrector::with_terms(url, &tb).correct(&raw_text) {
                    Some(fixed) if fixed != raw_text => {
                        log::info!("术语纠错 {:.1}s: {raw_text:?} → {fixed:?}",
                                   t_fix.elapsed().as_secs_f32());
                        (paste::sanitize(&fixed), true)
                    }
                    // 模型认为不用改，或者纠错不可用。两种都走原文，
                    // 区别只在日志——对用户是同一件事。
                    Some(_) => (raw_text.clone(), false),
                    None => (raw_text.clone(), false),
                }
            } else {
                (raw_text.clone(), false)
            };

            println!("\n{text}\n");
            // 派生数据落盘。失败只记日志——raw 还在，随时可以重算，
            // 不值得为它中断上屏
            match store.write_transcript(&committed.content_hash, &text) {
                Ok(p) => log::debug!("转写已存 {}", p.display()),
                Err(e) => log::error!("写转写文件失败（不影响剪贴板与上屏）: {e:#}"),
            }
            // 纠错是**有损**的：模型可能改错、可能过度改写。真改了就把
            // 改之前的也留一份，否则出问题时分不清是 ASR 错了还是 LLM 改坏了。
            // 没改就不写——每次录音多一个内容相同的文件纯属噪音。
            if corrected {
                match store.write_raw_transcript(&committed.content_hash, &raw_text) {
                    Ok(p) => log::debug!("纠错前的原始转写已存 {}", p.display()),
                    Err(e) => log::error!("写原始转写失败: {e:#}"),
                }
            }

            // —— 标签识别 + routes 落盘 ——
            //
            // **无条件写一条 routes 记录**，即使标签是 unknown、即使边车没起。
            // routes 是「这段话被判成了什么」的本地权威记录（架构边界 B6），
            // 它的价值不取决于判得准不准——判成 unknown 也是一条有用的记录，
            // 而缺一条记录会让这段音频在下游彻底消失。
            //
            // 和纠错一样：这一层的任何失败都不能挡住上屏，所以它在
            // 剪贴板与上屏**之前**做完，失败只记日志。
            let classified = if cfg.correct_terms {
                // 复用纠错的开关：两者都要边车，分开设两个开关只会让
                // 「为什么没生效」多一种可能。边车没起时 classify 自己会落 unknown。
                let url = cfg.llm_url.as_deref().unwrap_or(correct::DEFAULT_URL);
                label::Classifier::new(url).classify(&text)
            } else {
                // 没开边车功能时也要落 routes：标签留 unknown，
                // 记录本身不能少——将来开了功能可以从 transcript 重算。
                label::Classified { label: label::Label::Unknown, source: label::Source::Model }
            };
            let route = route::Route::new(
                &committed.content_hash,
                classified.label,
                classified.source,
                &text,
            );
            let route_written = match store.write_route(&route) {
                Ok(p) => {
                    log::info!(
                        "标签 {}（{}）→ {}",
                        classified.label.as_str(),
                        match classified.source {
                            label::Source::Explicit => "用户明说",
                            label::Source::Model => "模型推断",
                        },
                        p.display()
                    );
                    true
                }
                Err(e) => {
                    log::error!("写 routes 记录失败（不影响剪贴板与上屏）: {e:#}");
                    false
                }
            };

            // —— 知识库投递 ——
            //
            // **入队在投递之前**：进程要是在投递中途被杀，marker 还在，
            // 下次启动会补上。反过来（失败了才入队）就有一个「既没成功
            // 也没入队」的洞（`deliver` 的模块文档展开说了）。
            //
            // 真正的投递排在剪贴板/上屏**之后**——任何下游都不能挡住上屏。
            //
            // `routes/` 没写成就整段跳过：投递状态要回写到那份记录里，
            // 记录不存在的话投出去的东西无从追溯，队列里的 marker 也会
            // 指向一条读不出来的 route。
            let sink = (cfg.kb_enabled && route_written)
                .then(|| kb::FileSink::new(store.root(), cfg.kb_root(store.root())));
            if sink.is_some() {
                if let Err(e) = deliver::enqueue(store, &route) {
                    // 入队失败 = 这条投递失败后不会被自动补投。用户得知道，
                    // 否则他会以为链路是通的。`--replay-kb` 能补回来。
                    log::error!("加入投递队列失败，这条不会自动重试（可跑 --replay-kb 补投）: {e:#}");
                }
            }
            // 先进剪贴板再谈上屏。上屏失败还能手动 ⌘V，顺序反过来就没有退路了。
            match copy_to_clipboard(&text) {
                Ok(()) => {
                    let pasted = if paste::enabled() {
                        match paste::paste() {
                            Ok(()) => " → 已上屏",
                            Err(e) => {
                                log::error!("自动上屏失败（文本仍在剪贴板，可手动 ⌘V）: {e:#}");
                                ""
                            }
                        }
                    } else {
                        ""
                    };
                    println!(
                        "（已复制到剪贴板{pasted}，全程 {:.1}s）\n",
                        started.elapsed().as_secs_f32()
                    );
                }
                Err(e) => log::error!("写剪贴板失败: {e:#}"),
            }

            // 用户已经拿到文字了，这一步慢一点、失败了都不影响他。
            if let Some(sink) = sink {
                deliver::attempt(store, &sink, &route);
            }
        }
        Ok(_) => log::warn!("转写结果为空（这段音频可能没有语音）"),
        // raw 已经安全落盘，转写失败只是丢了一次派生结果，可以重跑
        Err(e) => log::error!("转写失败（raw 音频已保留，可重试）: {e:#}"),
    }

    tray::set(tray::Status::Idle);
    Ok(State::Idle)
}

/// launchd 的 job label，`scripts/bundle.sh` 与 plist 里保持一致。
const LAUNCHD_LABEL: &str = "ai.idoris.agentear";

/// 重启自己。改触发键时用——`CGEventTap` 挂在一个跑 `CFRunLoop` 的线程上，
/// 运行时换不掉。
///
/// 两条路径：**由 launchd 托管时必须走 `kickstart`**，因为自己 fork 一个新
/// 实例再退出会绕开 launchd 的单实例保证，落得两个进程同时抢热键；
/// 从终端裸跑时才 re-exec。
pub fn restart_self() {
    // 重启也是一条退出路径：不收拾的话，重启后的新实例会发现端口被
    // 「上一个自己拉起的边车」占着，而那个进程已经没人管了。
    sidecar::shutdown();

    let target = format!("gui/{}/{}", unsafe { libc::getuid() }, LAUNCHD_LABEL);
    let managed = std::process::Command::new("/bin/launchctl")
        .args(["print", &target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if managed {
        log::info!("由 launchd 托管，用 kickstart 重启");
        // kickstart -k 会先杀掉当前实例再拉起，所以这行之后本进程就没了
        if let Err(e) = std::process::Command::new("/bin/launchctl")
            .args(["kickstart", "-k", &target])
            .spawn()
        {
            log::error!("launchctl kickstart 失败: {e}");
        }
        return;
    }

    match std::env::current_exe() {
        Ok(exe) => {
            log::info!("非 launchd 托管，re-exec {}", exe.display());
            match std::process::Command::new(exe).spawn() {
                Ok(_) => std::process::exit(0),
                Err(e) => log::error!("re-exec 失败: {e}"),
            }
        }
        Err(e) => log::error!("拿不到自身路径，无法重启: {e}。请手动重启 AgentEar"),
    }
}

/// 环境自检。「按了没反应」时先跑这个。
fn diagnose(vendor: &std::path::Path) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait};

    println!("=== AgentEar 环境自检 ===\n");

    let trusted = hotkey::is_accessibility_trusted();
    println!("辅助功能权限: {}", if trusted { "✅ 已授予" } else { "❌ 未授予" });
    println!(
        "  → 触发键将是: {}",
        if trusted { "右 Command" } else { "Ctrl+Shift+R（降级）" }
    );
    if !trusted {
        println!("  → 想用右 Command：系统设置 → 隐私与安全性 → 辅助功能，勾选本程序后重启");
    }

    println!("\n音频输入设备:");
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(d) => {
            println!("  默认: {}", d.name().unwrap_or_else(|_| "?".into()));
            match d.default_input_config() {
                Ok(c) => println!(
                    "  配置: {} Hz, {} ch, {:?}",
                    c.sample_rate().0,
                    c.channels(),
                    c.sample_format()
                ),
                Err(e) => println!("  ❌ 读取配置失败: {e}（麦克风权限？）"),
            }
        }
        None => println!("  ❌ 找不到输入设备"),
    }

    println!("\nASR 依赖:");
    for (name, p) in [
        ("二进制", vendor.join("bin/llama-funasr-sensevoice")),
        ("模型", vendor.join("models/sensevoice-small-q8.gguf")),
        ("VAD", vendor.join("models/fsmn-vad.gguf")),
    ] {
        let ok = p.exists();
        let size = p.metadata().map(|m| m.len() / 1048576).unwrap_or(0);
        println!(
            "  {} {name}: {} ({} MiB)",
            if ok { "✅" } else { "❌" },
            p.display(),
            size
        );
    }

    // 泰语是**可选**链路：缺东西不是故障，是「还没装」。
    // 所以这里不用 ❌ 而用 ⚪，免得自检看起来像是坏了。
    println!("\n泰语识别（可选，按需下载）:");
    let wbin = vendor.join("bin/whisper-cli");
    println!(
        "  {} 引擎: {}",
        if wbin.exists() { "✅" } else { "⚪" },
        wbin.display()
    );
    match download::path_of(&download::THAI) {
        Some(m) => {
            // 判据必须是 `is_installed`（清单 + 体积），**不能是 `exists()`**。
            // 用 exists 的话，删掉清单、清单里 sha 对不上、文件被截断
            // 这三种坏情况自检全都报 ✅，而实际一录音就失败——
            // 自检骗人比没有自检更糟。
            // 判据必须是 `is_installed`（记录 + 体积 + 引擎指纹），
            // **不能是 `exists()`**。用 exists 的话，删掉记录、记录对不上、
            // 文件被截断、引擎换了这几种坏情况自检全都报 ✅，
            // 而实际一录音就失败——自检骗人比没有自检更糟。
            let issue = download::install_issue(&download::THAI);
            let size = m.metadata().map(|x| x.len() / 1048576).unwrap_or(0);
            println!(
                "  {} 模型: {} ({} MiB{})",
                if issue.is_none() { "✅" } else { "⚪" },
                m.display(),
                size,
                issue.map(|r| format!("，{r}")).unwrap_or_default()
            );
        }
        None => println!("  ⚪ 模型: 数据目录未初始化"),
    }
    println!("  当前识别语言: {:?}", config::get().asr_lang);

    // 术语纠错也是**可选**链路：没起服务不是故障。
    let cfg = config::get();
    let url = cfg.llm_url.clone().unwrap_or_else(|| correct::DEFAULT_URL.into());
    println!("\n技术术语纠错（可选，需要 LLM 边车）:");
    println!("  开关: {}", if cfg.correct_terms { "✅ 开" } else { "⚪ 关" });
    let reachable = correct::Corrector::new(&url).probe();
    match &reachable {
        Ok(()) => println!("  ✅ 服务: {url}"),
        Err(e) => {
            println!("  ⚪ 服务: {url} —— {e}");
            println!("     启动：scripts/serve-llm.sh（首次需先跑 scripts/setup-llm.sh）");
        }
    }
    if cfg.correct_terms && reachable.is_err() {
        // 这个组合每次录音都会白等一次超时，值得单独喊一嗓子
        println!("  ⚠️ 开关是开的但服务没起——每次录音会多等一次超时后才上屏");
    }

    println!("\n数据目录: {}", data_root()?.display());
    Ok(())
}

/// 日志同时写 stderr 和 `~/.agentear/agentear.log`。
///
/// 从 Finder / `open` 启动 .app 时 stderr 无处可去，只有文件日志能看到
/// 发生了什么——这对一个没有主窗口的菜单栏程序是刚需。
fn init_logging() {
    struct Tee(std::fs::File);
    impl std::io::Write for Tee {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
            self.0.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let _ = std::io::Write::flush(&mut std::io::stderr());
            self.0.flush()
        }
    }

    let mut b = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug"),
    );
    b.format_timestamp_millis();

    if let Ok(root) = data_root() {
        let _ = std::fs::create_dir_all(&root);
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("agentear.log"))
        {
            b.target(env_logger::Target::Pipe(Box::new(Tee(f))));
        }
    }
    b.init();
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_text(text.to_string())?;
    Ok(())
}

fn data_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AGENTEAR_DATA") {
        return Ok(PathBuf::from(p));
    }
    Ok(dirs::home_dir()
        .context("找不到 home 目录")?
        .join(".agentear"))
}

/// vendor/ 里放 ASR 二进制和模型。
///
/// 查找顺序：环境变量 → .app bundle 内的 Resources → 源码树。
/// 打包后可执行文件在 `AgentEar.app/Contents/MacOS/`，
/// vendor 在 `AgentEar.app/Contents/Resources/vendor`。
fn vendor_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AGENTEAR_VENDOR") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        // .../Contents/MacOS/AgentEar → .../Contents/Resources/vendor
        if let Some(contents) = exe.parent().and_then(|p| p.parent()) {
            let bundled = contents.join("Resources/vendor");
            if bundled.exists() {
                return Ok(bundled);
            }
        }
    }
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor"))
}
