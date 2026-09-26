//! AgentEar M1：按快捷键录音 → raw 落盘 → 转写 → 剪贴板。
//!
//! 范围严格限定在 `docs/milestones.md` 的 M1：不含 LLM、标签路由、TTS。
//! M1 恰好绕开了 AEC 和无边界流式 raw 语义两个难点——快捷键的按下/再按
//! 天然给出段边界，每次录音就是一个有头有尾的文件对象。**不要在 M1 里
//! 提前引入 TTS 或无边界流。**

mod asr;
mod download;
mod engine;
mod audio;
mod config;
mod commands;
mod correct;
mod cue;
mod deliver;
mod hotkey;
mod i18n;
mod index;
mod kb;
mod label;
mod launch_agent;
mod paste;
mod qwen3;
mod route;
mod session;
mod sidecar;
mod store;
mod talk;
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
    // 一次性子命令结束时收掉它拉起的 Qwen3 常驻服务（守护进程不会走到 Drop）。
    let _qwen3_server_guard = qwen3::ServerGuard;
    // 信号处理**在所有子命令之前**就装上：`--transcribe` 这类命令在常驻模式下也会
    // 拉起 speech-server，被 Ctrl+C / kill 打断时要把它一起收掉（守护进程之后会再装一次，幂等）。
    sidecar::install_signal_handlers();
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
    // 后端选择：命令行 `--asr-backend` 优先于配置，配置优先于默认。
    //
    // 命令行能覆盖是为了排障——「换个引擎试试」不该逼用户先改配置再改回来，
    // 和 `--lang` 是同一个理由。
    let pinned_backend = args.iter().any(|a| a == "--asr-backend");
    let backend = if pinned_backend {
        let v = flag_value(&args, "--asr-backend").ok_or_else(|| {
            anyhow::anyhow!(
                "--asr-backend 后面要跟后端名（{}）",
                engine::AsrBackend::NAMES.join(" / ")
            )
        })?;
        engine::AsrBackend::parse_cli(v)?
    } else {
        cfg.asr_backend
    };

    // 预下载 Qwen3-ASR（设置窗口那条路的命令行版，也是排障入口：能看到完整的失败原因）。
    // **只装，不选**——同 `--fetch-thai`。放在引擎对账之前：配置里选着 Qwen3
    // 而还没装好时，下面的对账会退回 SenseVoice，这条命令不该被那个挡住。
    if args.iter().any(|a| a == "--fetch-qwen3") {
        let m = match flag_value(&args, "--fetch-qwen3") {
            Some(v) if !v.starts_with("--") => qwen3::Qwen3Model::parse_cli(v)?,
            _ => cfg.qwen3_model,
        };
        if qwen3::is_ready(m) {
            println!("✅ {} 已安装（{}）", m.label(), qwen3::root().map(|r| r.display().to_string()).unwrap_or_default());
            return Ok(());
        }
        println!(
            "下载 {}（模型 {:.0} MB{}）…",
            m.label(),
            m.total_bytes() as f64 / 1e6,
            if qwen3::runtime_installed() {
                String::new()
            } else {
                format!(" + speech-swift {} 运行时 99 MB", qwen3::SPEECH_VERSION)
            }
        );
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d2 = done.clone();
        let progress = std::thread::spawn(move || {
            while !d2.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some((a, b)) = qwen3::progress_bytes(m) {
                    if b > 0 {
                        eprint!("\r  {:>5.1}%  {:.0} / {:.0} MB   ", a as f64 * 100.0 / b as f64, a as f64 / 1e6, b as f64 / 1e6);
                    }
                } else if matches!(qwen3::state(m), download::State::Verifying) {
                    eprint!("\r  校验 + 断网冒烟中……                ");
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        });
        let t0 = Instant::now();
        let r = qwen3::install_blocking(m);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = progress.join();
        eprintln!();
        r?;
        println!("✅ {} 已安装（{:.0}s）。到菜单「设置…→ 语音识别」选用它", m.label(), t0.elapsed().as_secs_f64());
        return Ok(());
    }

    // 配置里选着 Qwen3-ASR，但它现在用不了（没下载、被删了、或者下到一半），
    // 而 PATH 上也没有老的 brew 版 speech：不对账的话下面的 preflight 会让
    // **整个守护进程起不来**。宁可退回随包的 SenseVoice 并把原因写清楚——
    // 录音是这个程序的本职，不能因为一个可选后端没装好就全挂。
    let backend = if !pinned_backend
        && backend == engine::AsrBackend::SpeechSwift
        && !qwen3::is_ready(cfg.qwen3_model)
        && std::process::Command::new("speech").arg("--help").output().is_err()
    {
        log::warn!(
            "配置里选的是 {}，但它还没装好，PATH 上也没有 speech",
            cfg.qwen3_model.label()
        );
        log::warn!("  已退回随包的 SenseVoice。要用它：菜单「设置…→ 语音识别」选它就会下载");
        config::update(|c| c.asr_backend = engine::AsrBackend::Builtin);
        engine::AsrBackend::Builtin
    } else {
        backend
    };
    let cfg = config::get();

    // 配置里写着泰语，但模型可能已经被删了、被换过、或者当初压根没装完。
    // 不对账的话，菜单只是不显示勾，而**每一次录音都会走泰语分支然后失败**，
    // 错误只出现在日志里——用户看到的是「按了键，什么都没出来」。
    // 宁可退回自动（那条链路的模型随包走，一定在），并把原因写清楚。
    //
    // ⚠️ **这条对账只对 builtin 成立。** speech_swift 的泰语走 Qwen3-ASR，
    // 根本不用 `download::THAI` 那个 whisper 模型——如果不加这个前置判断，
    // 用 speech_swift + 泰语的用户会在每次启动时被**持久化地**改回自动识别，
    // 而原因（"泰语模型不可用"）跟他选的后端毫无关系。
    let backend_needs_thai_model = backend == engine::AsrBackend::Builtin;
    let cfg = if backend_needs_thai_model
        && cfg.asr_lang == asr::AsrLang::Thai
        && !download::is_installed(&download::THAI)
    {
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

    // 守护进程与离线子命令共用这一个引擎：**每轮按配置分派**（设置窗口里切换即时生效）；
    // `--asr-backend` 显式指定时锁死不跟配置走。
    let asr = engine::build_dynamic(pinned_backend.then_some(backend), &vendor, Some(&data_root))?;
    // **构造成功 ≠ 依赖齐全。** `build` 只是造对象，
    // speech_swift 甚至根本不碰 vendor——不跑 preflight 的话，
    // `speech` 没装也能把守护进程起起来，直到第一次录完音才失败，
    // 而那时候用户已经对着麦克风说完话了。
    asr.preflight(cfg.asr_lang)
        .with_context(|| format!("ASR 后端 {} 依赖检查失败", asr.name()))?;
    log::debug!("ASR 后端 = {}，依赖检查通过", asr.name());

    // 离线转写一个已有的 wav，不占麦克风，用于验证 ASR 链路。
    //
    // `--lang th` 可以在不改配置的情况下试泰语链路——排查「是模型的问题还是
    // 录音的问题」时，不该逼用户先去菜单里改设置再改回来。
    // ⚠️ 用 position 找，**不要写 `args[1] == "--transcribe"`**。
    //
    // 这是踩出来的：加了 `--asr-backend` 之后，
    // `--asr-backend speech_swift --transcribe x.wav` 这种写法会让
    // `args[1]` 变成 `--asr-backend`，于是**整个分支被跳过、程序静默变成守护进程**——
    // 用户看到的是菜单栏帮助，而不是转写结果，也没有任何报错。
    // 静默降级比报错难查得多，所以子命令的识别必须与参数顺序无关。
    if let Some(ti) = args.iter().position(|a| a == "--transcribe") {
        let wav = flag_value(&args, "--transcribe")
            .ok_or_else(|| anyhow::anyhow!("--transcribe 后面要跟 wav 路径"))?
            .to_string();
        let _ = ti;
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
        let t = asr.transcribe(std::path::Path::new(&wav), lang)?;
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

    // **开发/验收用**：同一个进程里轮流切换识别引擎（SenseVoice / Qwen3 0.6B / 1.7B ×
    // 逐次 / 常驻），每种跑 N 轮，打印每轮耗时与文字（TSV）。
    //
    // 两个用途：① 证明「设置里一切换，下一轮就生效」——这里切换走的就是
    // `config::update`，引擎是守护进程同一个 `Dispatch`，不重启进程；
    // ② 给高资源档的准入条件提供实测数（`docs/benchmarks-asr-zh-en.md` 的 T3.5.6 一节）。
    // ⚠️ 会改写 config.json（结束时恢复原值）——请配 `AGENTEAR_DATA` 指向临时目录跑。
    if args.iter().any(|a| a == "--asr-bench") {
        let wav = flag_value(&args, "--asr-bench")
            .ok_or_else(|| anyhow::anyhow!("--asr-bench 后面要跟 wav 路径"))?
            .to_string();
        let runs: usize = flag_value(&args, "--runs").and_then(|v| v.parse().ok()).unwrap_or(5);
        let saved = config::get();
        let combos: Vec<(&str, engine::AsrBackend, qwen3::Qwen3Model, bool)> = vec![
            ("sensevoice", engine::AsrBackend::Builtin, qwen3::Qwen3Model::Small, false),
            ("qwen3-0.6b-cli", engine::AsrBackend::SpeechSwift, qwen3::Qwen3Model::Small, false),
            ("qwen3-0.6b-resident", engine::AsrBackend::SpeechSwift, qwen3::Qwen3Model::Small, true),
            ("qwen3-1.7b-cli", engine::AsrBackend::SpeechSwift, qwen3::Qwen3Model::Large, false),
            ("qwen3-1.7b-resident", engine::AsrBackend::SpeechSwift, qwen3::Qwen3Model::Large, true),
        ];
        println!("combo\trun\tsecs\tchild_maxrss_mb\tserver_rss_mb\ttext");
        let only = flag_value(&args, "--only").map(str::to_string);
        for (name, backend, model, resident) in combos {
            if only.as_deref().is_some_and(|o| o != name) {
                continue;
            }
            if backend == engine::AsrBackend::SpeechSwift && !qwen3::is_ready(model) {
                eprintln!("跳过 {name}：{} 没装", model.label());
                continue;
            }
            config::update(|c| {
                c.asr_backend = backend;
                c.qwen3_model = model;
                c.qwen3_resident = resident;
            });
            if !resident {
                qwen3::stop_server("bench 切到逐次调用");
            }
            for run in 1..=runs {
                let t0 = Instant::now();
                let t = asr.transcribe(std::path::Path::new(&wav), asr::AsrLang::Auto)?;
                let secs = t0.elapsed().as_secs_f64();
                // 逐次调用的峰值：子进程的 ru_maxrss（macOS 单位是字节）。
                // 它是「所有已回收子进程里最大的那个」，所以 combo 按内存从小到大排。
                let child = unsafe {
                    let mut ru: libc::rusage = std::mem::zeroed();
                    libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru);
                    ru.ru_maxrss as f64 / 1048576.0
                };
                let server = qwen3::server_pid()
                    .and_then(|pid| {
                        std::process::Command::new("/bin/ps")
                            .args(["-o", "rss=", "-p", &pid.to_string()])
                            .output()
                            .ok()
                    })
                    .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
                    .map(|kb| format!("{:.0}", kb / 1024.0))
                    .unwrap_or_else(|| "-".into());
                println!("{name}\t{run}\t{secs:.3}\t{child:.0}\t{server}\t{}", t.text.replace('\t', " "));
            }
        }
        // `--hold <秒>`：跑完后进程再挂一会儿，用来实测空闲回收（常驻服务到点自己退）
        if let Some(h) = flag_value(&args, "--hold").and_then(|v| v.parse::<u64>().ok()) {
            let t0 = Instant::now();
            while t0.elapsed() < std::time::Duration::from_secs(h) {
                std::thread::sleep(std::time::Duration::from_secs(5));
                eprintln!("hold {:>4}s  server_pid={:?}", t0.elapsed().as_secs(), qwen3::server_pid());
            }
        }
        qwen3::stop_server("bench 结束");
        config::update(|c| {
            c.asr_backend = saved.asr_backend;
            c.qwen3_model = saved.qwen3_model;
            c.qwen3_resident = saved.qwen3_resident;
        });
        return Ok(());
    }

    // 先把泰语模型下下来，不必等到在菜单里点。
    //
    // 存在的理由有三个：想在有网的时候提前下好；菜单那条路出问题时的
    // 备用入口；以及排障时能看到完整的失败原因——菜单里只显示
    // 「失败（网络）」五个字，这里能看到 curl 的退出码。
    if args.iter().any(|a| a == "--fetch-thai") {
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

    // 从 `kb/` 全量重建 L2 索引。
    //
    // 索引是**可以随便删的**（ADR-0003 §7：L2 必须能从 L1 全量重建），
    // 这条命令就是那个「重建」。schema 改了、索引写坏了、
    // 手工往 `kb/` 里加过文件，跑一次就对上了。
    if args.iter().any(|a| a == "--reindex") {
        let kb_root = cfg.kb_root(&data_root);
        let mut ix = index::Index::open(&data_root)?;
        let (n, skipped) = ix.rebuild(&data_root, &kb_root)?;
        println!("已索引 {n} 篇（跳过 {skipped} 个不是 AgentEar 写的 Markdown）→ {}",
                 data_root.join("derived/index.sqlite").display());
        return Ok(());
    }

    // 全文检索。
    if args.iter().any(|a| a == "--search") {
        let q = args_after(&args, "--search").join(" ");
        let ix = index::Index::open(&data_root)?;
        let hits = ix.search(&q, 20)?;
        if hits.is_empty() {
            // 空索引和「真的没有」是两回事，别让用户以为东西丢了
            let total = ix.count().unwrap_or(0);
            println!("没有命中。（索引里共 {total} 篇{}）", if total == 0 { "，先跑一次 --reindex" } else { "" });
            return Ok(());
        }
        for h in &hits {
            // **按字符切，不按字节。** `created` 可能是从磁盘读来的脏数据
            // （坏时间戳会被原样加引号存进 front matter），里面有多字节字符时
            // 按字节切会直接 panic —— 「搜索一下」把程序搞崩是最不该有的。
            let when: String = h.created.chars().take(16).collect();
            println!("{when}  [{}]  {}", h.label, h.path);
            println!("    {}", h.snippet);
        }
        println!("\n共 {} 条", hits.len());
        return Ok(());
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
        // 重放顺带更新索引——否则重建完 kb/ 还得再跑一次 --reindex，
        // 而「重建完搜不到」是最容易让人以为功能坏了的那种状态。
        let ix = index::Index::open(&data_root)
            .map_err(|e| log::error!("索引打不开，本次重放不更新索引: {e:#}"))
            .ok();
        let st = deliver::replay(&store, &sink, ix.as_ref())?;
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
    if args.iter().any(|a| a == "--classify") {
        let url = cfg.llm_url.as_deref().unwrap_or(correct::DEFAULT_URL);
        // 一次性命令**只探不拉**：边车冷启动要几十秒，为一句分类去拉起
        // 不合理。但必须探一次——门控读的是全局健康状态，
        // 而这条路径没有守护进程那套 ensure_available 去填它。
        sidecar::probe(url);
        let text = args_after(&args, "--classify")
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("--classify 后面要跟一段文字"))?
            .to_string();
        let r = label::Classifier::new(url).classify(&text);
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
    if args.iter().any(|a| a == "--diagnose") {
        return diagnose(&vendor);
    }

    // ------------------------------------------------------------------
    // 通话（M3 / ADR-0007）。
    //
    // 三个入口，都**不碰麦克风**：
    //   --ask <文字>       文字进 → 语音出（跳过 ASR）
    //   --say <文字>       只测 TTS 那一段
    //   --talk-turn <wav>  **完整一轮，且走的是守护进程那条代码路径**
    //                      （ASR → 会话状态机 → LLM → TTS → 播放）
    //
    // 为什么 `--talk-turn` 不是可有可无的：守护进程那一轮的入口是**录音键**，
    // 而按键、麦克风权限、TCC 这几样都没法在无人值守下复现。没有这个入口，
    // 「推键式链路真的通」就永远只能靠人肉按一次键来证明——
    // 而那正是这个仓库反复吃过亏的地方（`benchmarks-m3.md` §7.5：
    // 测量设计有缺陷时，数字算得再对也是假的）。
    // 它跑的是 `answer_out_loud` **同一个函数**，只是文字从 wav 来而不是从麦克风来。
    // ------------------------------------------------------------------
    if args.iter().any(|a| a == "--talk-turn") {
        let wav = args_after(&args, "--talk-turn")
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("--talk-turn 后面要跟一个 wav 路径"))?
            .to_string();
        let lang = match flag_value(&args, "--lang") {
            Some(v) => talk::TalkLang::parse(v)?,
            None => cfg.talk_lang,
        };
        // 会话状态机要么已经开着，要么按这次的语言现开一个。
        // 命令行要能独立跑完整一轮，所以不能依赖 `talk_enabled`——
        // 那个开关管的是**守护进程**要不要在录音后自动接话。
        if with_session(|s| s.lang()).is_none() {
            open_session(lang);
        }
        // ⚠️ **这一步不能省：它等价于守护进程里「按下录音键」。**
        // 少了它，会话还停在 Idle，后面 finish_listening / turn_ready
        // 会被状态机当成非法转移全部拒掉——而拒绝只写 warning，
        // 于是日志里会出现「0 轮」这种自相矛盾的结果（实测踩到过）。
        if let Some(Err(e)) = with_session(|s| s.begin_turn()) {
            log::warn!("开不了一轮：{e}");
        }
        println!("== 通话一轮（离线，不走麦克风）==");
        // ASR：与守护进程同一个引擎、同一条参数规则（`--lang` 只认 th / auto）。
        // 与守护进程同一个后端、同一套构造（`config.json` 的 `asr_backend` 说了算）
        let engine = engine::build(cfg.asr_backend, &vendor, Some(&data_root))?;
        let asr_lang = if lang == talk::TalkLang::Th {
            asr::AsrLang::Thai
        } else {
            asr::AsrLang::Auto
        };
        let t_asr = Instant::now();
        let transcript = engine
            .transcribe(std::path::Path::new(&wav), asr_lang)
            .with_context(|| format!("转写失败：{wav}"))?;
        let heard = paste::sanitize(&transcript.text);
        println!(
            "① 听到（ASR {:.2}s）：{heard}",
            t_asr.elapsed().as_secs_f32()
        );
        if heard.trim().is_empty() {
            // 空转写要**显式收尾**：会话那边会把这一轮当作「没人说话」丢掉，
            // 不留空轮次（`session.rs` 的用例钉住这条）。
            with_session(|s| s.finish_listening());
            with_session(|s| s.turn_ready("", None));
            println!("（这段音频没有语音，轮次结束）");
            return Ok(());
        }
        // ②③④ 与守护进程一模一样：会话推进 → LLM → TTS → 播放
        answer_out_loud(&cfg, &heard, lang);
        return Ok(());
    }

    if args.iter().any(|a| a == "--ask") {
        let text = args_after(&args, "--ask")
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("--ask 后面要跟一句问话"))?
            .to_string();
        let lang = match flag_value(&args, "--lang") {
            Some(v) => talk::TalkLang::parse(v)?,
            None => cfg.talk_lang,
        };
        let engines = talk::Engines::from_config(&cfg);
        println!(
            "LLM {} @ {}\nTTS {} @ {}",
            engines.llm.name(),
            cfg.talk_llm_url.as_deref().unwrap_or(talk::DEFAULT_LLM_URL),
            engines.tts.name(),
            engines.tts.endpoint().unwrap_or("(内置)")
        );
        // **和守护进程同一条路**：边出边合成。`--no-stream` 强制走老的
        // 整句路径——它是 A/B 测量首字延迟的唯一开关，也是流式出问题时的
        // 退路（不用改配置、不用重编译）。
        if args.iter().any(|a| a == "--no-stream") {
            let reply = talk::answer(&engines, &cfg, &text, lang)?;
            println!("\n问：{text}\n答：{reply}\n");
            let played = talk::speak(&engines, &reply, lang)?;
            println!("（整句路径，已播放 {:.2}s）", played.as_secs_f32());
            return Ok(());
        }
        let (reply, played) =
            talk::answer_and_speak_streamed(&engines, &cfg, &text, lang, &mut || {})?;
        println!("\n问：{text}\n答：{reply}");
        println!("\n（流式路径，已播放 {:.2}s）", played.as_secs_f32());
        return Ok(());
    }

    if args.iter().any(|a| a == "--say") {
        let text = args_after(&args, "--say")
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("--say 后面要跟一段文字"))?
            .to_string();
        let lang = match flag_value(&args, "--lang") {
            Some(v) => talk::TalkLang::parse(v)?,
            None => cfg.talk_lang,
        };
        let engines = talk::Engines::from_config(&cfg);
        let played = talk::speak(&engines, &text, lang)?;
        println!(
            "TTS {} 说了 {:.2}s：{text}",
            engines.tts.name(),
            played.as_secs_f32()
        );
        return Ok(());
    }

    // ---- 录音提示音：试听 / 导出波形 ----
    //
    // `--cue <名字>` 直接播一声（不看 `record_cue` 开关，方便关着时也能试听）；
    // `--cue-wav <名字> <out.wav> [--rate 16000]` 把**守护进程播的同一段波形**
    // 写成文件——`scripts/cue-asr-check.py` 拿它混进录音，实测 ASR 会不会被它带偏。
    // 名字：start / start-conversation / upgrade / end / end-conversation。
    if args.iter().any(|a| a == "--cue" || a == "--cue-wav") {
        let wav_mode = args.iter().any(|a| a == "--cue-wav");
        let flag = if wav_mode { "--cue-wav" } else { "--cue" };
        let rest = args_after(&args, flag);
        let name = rest.first().copied().unwrap_or("");
        let c = cue::Cue::from_name(name).ok_or_else(|| {
            let names: Vec<_> = cue::Cue::ALL.iter().map(|c| c.name()).collect();
            anyhow::anyhow!("{flag} 要跟一个名字：{}", names.join(" / "))
        })?;
        if wav_mode {
            let out = rest.get(1).copied().ok_or_else(|| anyhow::anyhow!("--cue-wav 还要一个输出路径"))?;
            let rate: u32 = match flag_value(&args, "--rate") {
                Some(v) => v.parse().context("--rate 要是整数")?,
                None => 16_000,
            };
            std::fs::write(out, cue::wav_bytes(c, rate)).with_context(|| format!("写 {out} 失败"))?;
            println!("{}：{}ms @ {rate} Hz → {out}", c.name(), c.duration_ms());
        } else {
            // `--repeat N`：同一进程里连播 N 次，量「复用」路径的延迟——
            // 守护进程里除了第一声，走的都是这条。
            let n: u32 = match flag_value(&args, "--repeat") {
                Some(v) => v.parse().context("--repeat 要是整数")?,
                None => 1,
            };
            // `--gap-ms`：两次之间等多久。量「闲置一阵之后再按」是不是又变慢。
            let gap: u64 = match flag_value(&args, "--gap-ms") {
                Some(v) => v.parse().context("--gap-ms 要是整数")?,
                None => 300,
            };
            for _ in 0..n.max(1) {
                cue::play(c);
                // 播放在专用线程上是异步的；进程退出会把它一起带走，所以等它播完。
                std::thread::sleep(Duration::from_millis(c.duration_ms() as u64 + gap));
            }
            println!("{}（{}ms）×{}", c.name(), c.duration_ms(), n.max(1));
        }
        return Ok(());
    }

    // ---- 语音指令表：列 / 加 / 用录音加 ----
    //
    // 「录一条语音定义指令」的落点：**录的那句话先过 ASR 变成触发短语**，
    // 再写进 `commands.json`。⚠️ ASR 会听错，所以文件必须可编辑
    // （菜单里也有「打开指令表」），否则用户会得到一条永远匹配不上的指令。
    if args.iter().any(|a| a == "--commands") {
        let dir = data_root.clone();
        for c in commands::load(&dir)? {
            println!(
                "{:<16} → {:?}   别名: {}",
                c.phrase,
                c.action,
                if c.aliases.is_empty() { "（无）".into() } else { c.aliases.join(" / ") }
            );
        }
        println!("\n文件: {}", commands::path_in(&dir).display());
        return Ok(());
    }

    if args.iter().any(|a| a == "--add-command") || args.iter().any(|a| a == "--add-command-wav") {
        let dir = data_root.clone();
        let from_wav = args.iter().any(|a| a == "--add-command-wav");
        let flag = if from_wav { "--add-command-wav" } else { "--add-command" };
        let raw = args_after(&args, flag)
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("{flag} 后面要跟{}", if from_wav { "一个 wav 路径" } else { "一句短语" }))?
            .to_string();
        let phrase = if from_wav {
            // 要显式给语言：中英走 SenseVoice 默认路径，泰语要指明（同 --transcribe 的规矩）
            let lang = match flag_value(&args, "--lang") {
                Some("th") => asr::AsrLang::Thai,
                _ => asr::AsrLang::Auto,
            };
            let engine = engine::build(config::get().asr_backend, &vendor, Some(&dir))?;
            let t = engine.transcribe(std::path::Path::new(&raw), lang)?;
            let text = paste::sanitize(&t.text);
            println!("听到: {text}");
            text
        } else {
            raw
        };
        let action = match flag_value(&args, "--action").unwrap_or("builtin") {
            "builtin" => commands::Action::Builtin {
                name: flag_value(&args, "--name").unwrap_or("note").to_string(),
                value: flag_value(&args, "--value").map(str::to_string),
            },
            "open_url" => commands::Action::OpenUrl {
                url: flag_value(&args, "--url")
                    .ok_or_else(|| anyhow::anyhow!("open_url 需要 --url"))?
                    .to_string(),
            },
            "http_post" => commands::Action::HttpPost {
                url: flag_value(&args, "--url")
                    .ok_or_else(|| anyhow::anyhow!("http_post 需要 --url"))?
                    .to_string(),
                body: flag_value(&args, "--body").unwrap_or("").to_string(),
            },
            other => anyhow::bail!("--action 只认 builtin / open_url / http_post，收到 {other:?}"),
        };
        let cmd = commands::Command {
            phrase: phrase.clone(),
            aliases: Vec::new(),
            action,
            note: None,
            // `--add-command --confirm` 可以给「本来不用问」的动作加一道确认
            confirm: args.iter().any(|a| a == "--confirm"),
        };
        cmd.validate().context("这条指令不合法")?;
        let mut all = commands::load(&dir)?;
        if all.iter().any(|c| commands::normalize(&c.phrase) == commands::normalize(&phrase)) {
            println!("⚠️ 「{phrase}」已经存在，先删掉再加（指令表文件: {}）", commands::path_in(&dir).display());
            return Ok(());
        }
        all.push(cmd);
        commands::save(&dir, &all)?;
        println!("✅ 已加指令「{phrase}」→ {} 条，文件: {}", all.len(), commands::path_in(&dir).display());
        if from_wav {
            println!("   ⚠️ 短语是 ASR 听出来的，**听错了就打开那个文件改**（菜单里也有「打开指令表」）");
        }
        return Ok(());
    }

    // ---- 语音指令表：**走一遍完整流程**（含二次确认）----
    //
    // 和内建那条路用的是**同一个函数**（`run_command_turn`），所以它不是
    // 「模拟」：它就是守护进程按一次录音键之后跑的东西。
    // 加它的理由是这一层的性质——**确认逻辑错了就会把东西发出去**，
    // 而「对着麦克风按键 + 说话」没法无人值守复现。
    //
    //   agentear --run-command "记到 notion 明天要测 AEC"
    //     → 打印要确认什么，**不执行**
    //   agentear --run-command "记到 notion 明天要测 AEC" --reply "确认"
    //     → 第一轮问，第二轮答，然后才执行
    if args.iter().any(|a| a == "--run-command") {
        let text = args_after(&args, "--run-command")
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("--run-command 后面要跟一句话"))?
            .to_string();
        let hash = format!("dryrun-{}", std::process::id());
        // CLI 路径上还没有 store（守护进程那条在 636 行才建），这里自己开一个：
        // 只用来读指令表，不写任何东西。
        let store = store::Store::open(&data_root)?;
        println!("① 用户说：{text}");
        let first = run_command_turn(&store, &cfg, &text, &hash);
        match first {
            CommandTurn::NotMine => {
                println!("   → 没命中指令表（正常路径会走对话）");
                return Ok(());
            }
            CommandTurn::Handled => {
                println!("   → 已执行（这个动作不需要二次确认）");
                return Ok(());
            }
            CommandTurn::Asked => println!("   → 等确认，**什么都没执行**"),
        }
        let reply = args_after(&args, "--reply").first().copied();
        let Some(reply) = reply else {
            println!("\n（要接着测确认，加 --reply \"确认\" / --reply \"取消\"）");
            // 进程要退出了，把待确认丢掉，别留下一个「挂着」的假状态
            drop_pending("命令行干跑结束");
            return Ok(());
        };
        println!("\n② 用户回答：{reply}");
        let verdict = commands::classify_confirmation(reply);
        println!("   → 判定：{verdict:?}");
        let outcome = run_command_turn(&store, &cfg, reply, &hash);
        match outcome {
            CommandTurn::Handled => println!("   → 处理完毕"),
            CommandTurn::Asked => println!("   → 又要确认一次（回答里又命中了一条向外指令）"),
            CommandTurn::NotMine => println!("   → 待确认已作废，这一轮按普通输入处理"),
        }
        drop_pending("命令行干跑结束");
        return Ok(());
    }

    // ---- 语音指令表：**干跑**（只报命中，不执行）----
    //
    // 两个用处：① 用户改完 `commands.json` 可以先干跑一遍再上嘴；
    // ② 我自己验收匹配器——**不必真的发一条录音、也不必真的发一次邮件**。
    // 刻意**不执行**任何动作：这个入口的全部价值就是「可看不可做」。
    if args.iter().any(|a| a == "--match-command") {
        let dir = data_root.clone();
        let text = args_after(&args, "--match-command")
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("--match-command 后面要跟一句话"))?
            .to_string();
        let all = commands::load(&dir)?;
        // ---- 给宿主程序（Agent24）的机器可读输出 ----
        //
        // ⚠️ **这是 AgentEar 与外壳之间冻结的那条边界**（见 ADR-0008）：
        // AgentEar 只**提出**一个动作，**执行由宿主做**。
        // 所以这里**绝不执行**任何东西，只把「命中了什么、要不要确认、
        // 对象是谁」结构化交出去。
        //
        // 为什么是「提出」而不是「执行」：确认界面、执行、回执展示、凭据管理
        // 全在宿主那一侧（jason 2026-09-15 拍板）——AgentEar 是推键式一问一答，
        // 没有界面，也**已经停止**在这条线上继续开发。
        if args.iter().any(|a| a == "--json") {
            let hit = commands::match_text(&all, &text);
            let opt_in = hit
                .as_ref()
                .and_then(|h| all.iter().find(|c| c.phrase == h.phrase))
                .map(|c| c.confirm)
                .unwrap_or(false);
            let needs_confirm = hit
                .as_ref()
                .map(|h| commands::needs_confirm(&h.action, opt_in))
                .unwrap_or(false);
            let payload = serde_json::json!({
                "schema": "agentear.proposal/1",
                "text": text,
                "normalized": commands::normalize(&text),
                "matched": hit.as_ref().map(|h| h.phrase.clone()),
                "rest": hit.as_ref().map(|h| h.rest.clone()),
                "action": hit.as_ref().map(|h| &h.action),
                "needs_confirm": needs_confirm,
                // ⚠️ **只有真要确认时才有 prompt**：宿主不该拿到一句
                // 「要执行 style」这种内部动作名去显示（那是写给人看的问句，
                // 不是通用的动作描述）。契约收紧成「有确认才有问句」。
                "prompt": if needs_confirm {
                    hit.as_ref().map(|h| commands::confirm_prompt(&h.action, &h.rest, &text))
                } else {
                    None
                },
                "command_count": all.len(),
                "commands_path": commands::path_in(&dir).display().to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        }
        println!("原文:     {text}");
        println!("归一后:   {}", commands::normalize(&text));
        match commands::match_text(&all, &text) {
            None => {
                println!("命中:     （无）→ 走 LLM 兜底，当作一次普通对话");
                println!("\n指令表 {} 条，文件: {}", all.len(), commands::path_in(&dir).display());
            }
            Some(hit) => {
                println!("命中短语: {}", hit.phrase);
                println!("剩下的:   {:?}", hit.rest);
                println!("动作:     {:?}", hit.action);
                let opt_in = all
                    .iter()
                    .find(|c| c.phrase == hit.phrase)
                    .map(|c| c.confirm)
                    .unwrap_or(false);
                if commands::needs_confirm(&hit.action, opt_in) {
                    println!("二次确认: **要**（向外动作，执行前会先问一句）");
                    println!("问句:     {}", commands::confirm_prompt(&hit.action, &hit.rest, &text));
                } else {
                    println!("二次确认: 不要（本机动作 / 只打开网页）");
                }
                println!("（干跑：**没有执行**任何动作）");
            }
        }
        return Ok(());
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
            let ix = index::Index::open(&data_root).ok();
            deliver::drain(&store, &sink, ix.as_ref());
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

    // 通话形态：只有显式打开时才建会话，并在启动时把「用哪个语言」定下来。
    // 之后用户在菜单里改 `talk_lang` 会在**下一轮**生效（生效点是每轮
    // `answer_out_loud` 重新读配置的那一刻），不需要重启——
    // 这正是 jason 要的「过程中可以随时切」。
    log::info!(
        "模式：{}（talk_mode = {}）",
        match cfg.talk_mode {
            config::TalkMode::InputMethod => "输入法（按一下键 → 转写 → 上屏，不出声）",
            config::TalkMode::Conversation => "对话（按一下键 → 转写 → 上屏 → 把回答念出来）",
        },
        cfg.talk_mode.as_str()
    );
    if cfg.talk_mode == config::TalkMode::Conversation {
        open_session(cfg.talk_lang);
        // **连接优先、拉起兜底**（ADR-0002 §8）。异步做：就绪等待最长 90 秒，
        // 卡在启动路径上等于菜单栏一分半不出来。
        talk::ensure_sidecars_async(&cfg);
        let engines = talk::Engines::from_config(&cfg);
        log::info!(
            "对话模式：LLM {} @ {}，TTS {} @ {}，语言 {}",
            engines.llm.name(),
            cfg.talk_llm_url.as_deref().unwrap_or(talk::DEFAULT_LLM_URL),
            engines.tts.name(),
            engines.tts.endpoint().unwrap_or("(内置)"),
            cfg.talk_lang.as_str()
        );
        if cfg.talk_llm_engine == "mock" {
            log::warn!("对话用的是 mock 引擎：回答是本地写死的，不是模型产出");
        }
    }

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
    // 提示音线程先起、波形先构造好——否则开机后第一下提示音会晚约 100ms。
    // 开关关着也照样预热：代价是一条空闲线程 + 几十 KB，换来设置里
    // 随时打开都立刻跟手。
    cue::warm_up();
    std::thread::spawn(move || {
        if let Err(e) = worker(rx, store, asr) {
            log::error!("工作线程退出: {e:#}");
            // 这条路径也要收拾边车，否则它会活过 AgentEar
            sidecar::shutdown();
    talk::shutdown_spawned();
            talk::shutdown_spawned();
            std::process::exit(1);
        }
    });

    // 让 Ctrl+C / SIGTERM 也能收拾边车。**必须在拉起之前注册**，
    // 否则启动过程中收到信号会留下孤儿。
    sidecar::install_signal_handlers();

    // 开机自动启动：每次启动都对一次账，不是只在用户点开关时才处理——
    // 升级换了 .app 安装路径、或者用户手改了 config.json，都要在这里收敛
    // 到"配置说的" == "磁盘上 plist 里写的"。放后台线程：文件 I/O 没有
    // 理由挡住菜单栏图标出现。`apply()` 自己现读配置、自己持锁，
    // 这里不用传参数。
    std::thread::spawn(launch_agent::apply);

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
    // 上一次（崩溃 / kill -9）留下的孤儿常驻服务先收掉，再决定要不要起新的。
    qwen3::reap_stale_server();
    // 选着 Qwen3 且开了常驻：启动时就在后台把服务起好、模型加载好，
    // 别让开机后的第一句话去付那 1.6 s。
    {
        let c = config::get();
        if c.asr_backend == engine::AsrBackend::SpeechSwift
            && c.qwen3_resident
            && qwen3::is_ready(c.qwen3_model)
        {
            qwen3::warm_async(c.qwen3_model);
        }
    }
    let _tray = tray::install(mtm);
    log::debug!("菜单栏图标已安装");

    log::debug!("主线程进入 AppKit 事件循环……");
    tray::run(mtm)
}

fn worker(
    rx: std::sync::mpsc::Receiver<hotkey::Signal>,
    store: store::Store,
    // 收 trait 对象而不是具体类型：后端是运行时按配置选的，
    // 这个函数不该知道自己在跑哪一个。
    asr: Box<dyn engine::AsrEngine>,
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
                state = finish(state, &store, asr.as_ref())?;
                continue;
            }
        }

        if let Ok(signal) = rx.try_recv() {
            let recording = matches!(state, State::Recording { .. });
            // **手势 → 动作** 的判定在 `hotkey::intent` 里（纯函数、有真值表），
            // 这里只负责执行。放那边是因为「双击写成两次单击」这种错
            // 只有真按键能暴露，而真按键在单测里造不出来。
            let intent = hotkey::intent(signal, recording);
            // 提示音按**动作之前**的模式判（见 `cue::for_intent`）。
            // 结束音不在这里响，在 `finish()` 里关掉麦克风之后响。
            let beep = cue::for_intent(intent, config::get().talk_mode);
            match intent {
                // 正在录音时收到双击：只切模式，**不要停**——否则「快速点两下」
                // 会把刚开的那段录成 0.2 秒碎片（会被当噪音丢掉）。
                hotkey::Intent::SwitchToConversation => {
                    log::info!("双击右 Command → 切到对话模式（这一段继续录）");
                    set_mode(config::TalkMode::Conversation);
                    // 补一声，和第一下的开始音凑成「嘟、嘟」。
                    record_cue(beep);
                }
                hotkey::Intent::End => {
                    log::debug!("收到触发事件：结束这一段");
                    state = finish(state, &store, asr.as_ref())?;
                }
                hotkey::Intent::Begin { set_conversation } => {
                    log::debug!("收到触发事件：开始一段（{signal:?}）");
                    // ⚠️ **打断优先于开始录音。** 用户按这一下键的意思是
                    // 「别说了，听我说」——工作线程那边可能正在播上一轮的回答。
                    // 先掐掉播放再开麦克风，顺序反了就会先录到自己正在放的
                    // 声音（AEC 那一格 T3.4.0 实测残留单字，见 ADR-0007 §4.3.0）。
                    // 输入法模式下不会有东西在播，但照样调一次：
                    // 代价是一次 mutex 尝试，收益是「刚切回输入法时那段没播完的
                    // 音频」也会被掐掉。
                    if talk::stop_playback() {
                        log::info!("对话被打断，开始听下一句");
                    }
                    // 还在等边车的上一轮回答靠这个序号知道「用户已经开口了」，
                    // 从而作废自己，不在用户说话时开始播（见 talk::await_sidecars_for_turn）。
                    talk::note_recording_started();
                    if let Some(conversation) = set_conversation {
                        let mode = if conversation {
                            config::TalkMode::Conversation
                        } else {
                            config::TalkMode::InputMethod
                        };
                        // 单击 = 输入法、双击 = 对话。**在「开始」这一刻定模式**，
                        // 因为 `finish()` 是结束时才读配置的——如果等到松开才定，
                        // 双击选出来的对话模式会被「停止」那一下抹掉。
                        set_mode(mode);
                    }
                    // **顺序有讲究**：先定模式（可能刚建好会话），再推会话相位，
                    // 最后才开麦克风。反过来的话，双击切对话时这一轮的
                    // `begin_turn` 会落在一个还不存在的会话上。
                    if config::get().talk_mode == config::TalkMode::Conversation {
                        ensure_session_listening(config::get().talk_lang);
                    }
                    state = match begin(&store) {
                        Ok(s) => {
                            last_heartbeat = Instant::now();
                            // **麦克风真的开了才响**：jason 要的是「告诉我话筒打开了」，
                            // 开麦失败时响一声就是在报喜不报忧。没听到声 = 没在录。
                            record_cue(beep);
                            s
                        }
                        Err(e) => {
                            log::error!("开始录音失败: {e:#}");
                            State::Idle
                        }
                    };
                }
            }
            continue;
        }
        // 没有事件的时候别空转
        std::thread::sleep(Duration::from_millis(20));
    }

    // ⚠️ 主循环是**不退出**的（`loop {}`，只能被信号杀掉），所以这里没有
    // 「退出前收尾」可写：挂在 loop 之后的清理代码是死代码。
    // 通话的统计因此每轮就写进日志（见 `answer_out_loud` 的 info 行），
    // 而不是等一个永远不会到来的退出点。
}

/// 响一声录音提示音；设置里关掉了（`record_cue: false`）就不响。
/// 每次现读配置，设置窗口里切一下立刻生效。
fn record_cue(cue: Option<cue::Cue>) {
    if let Some(c) = cue {
        if config::get().record_cue {
            cue::play(c);
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

fn finish(state: State, store: &store::Store, asr: &dyn engine::AsrEngine) -> Result<State> {
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
    // 结束音在**关掉麦克风之后**响——早于这里就会被录进这一段的结尾。
    // 过短丢弃 / 按键确认那条路也照样响：麦克风确实关了，声音如实反映这件事。
    // 模式在结束时不会变，所以就按此刻的模式挑（与 `cue::for_intent(End)` 同一判据）。
    record_cue(Some(cue::end_for(config::get().talk_mode)));

    let secs = session.duration_secs();
    if secs < 0.3 {
        // **「按一下录音键」就是确认。**
        //
        // 推键式架构里，麦克风只在按键时开，所以「说确认」必须先从按键开始。
        // 于是「按一下键（不说话）」和「按键 + 说确认」是同一串动作的前半截，
        // 用一个很短、里面不可能有语音的录音把它们区分开：
        //   · 短按 → 没说话 → 当成**按键确认**
        //   · 按键后说话 → 有转写 → 交给 `classify_confirmation` 判词
        // 这也正是原来「录音过短（<0.3s）丢弃」那条路的复用——
        // 那个长度里本来就不可能有话。
        if has_pending() {
            let cfg = config::get();
            if let Some(pending) = take_pending() {
                log::info!("短按录音键 → 确认「{}」", pending.hit.phrase);
                println!("✔ 已确认（按键）");
                run_confirmed(store, &cfg, pending);
                tray::set(tray::Status::Idle);
                return Ok(State::Idle);
            }
        }
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
                // 没开边车功能时**仍然认显式标记**——它是纯本地字符串匹配，
                // 不需要模型。用户说了「这是一个 idea」就该进知识库，
                // 不该因为他没装 7.8 GB 的 LLM 就一并丢掉。
                // 没明说的落 unknown，记录本身仍然要写。
                label::explicit_only(&text)
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
                // 索引每次现开：SQLite 连接开销是微秒级，而把一个
                // `Connection`（非 Sync）一路穿过状态机会让线程模型变复杂。
                // 打不开就只是没索引——L2 随时可以 `--reindex` 重建。
                let ix = index::Index::open(store.root())
                    .map_err(|e| log::error!("索引打不开（可用 --reindex 重建）: {e:#}"))
                    .ok();
                deliver::attempt(store, &sink, &route, ix.as_ref());
            }

            // —— 语音指令表（本地快路径）——
            //
            // **排在 LLM 之前**：命中的指令在本地 0ms 执行完，不用等模型
            // （省 0.7–0.9s，断网也能用）。没命中就照常走对话——
            // 开放式的句子本来就该由 LLM 理解，指令表只认用户声明过的那几条。
            //
            // ⚠️ **上屏/剪贴板在前面已经做完了**，所以指令轮次也不丢文字。
            if cfg.commands_enabled {
                match run_command_turn(store, &cfg, &text, &committed.content_hash) {
                    CommandTurn::Handled => {
                        tray::set(tray::Status::Idle);
                        return Ok(State::Idle);
                    }
                    // 待确认的向外动作**已经念给用户了**，这一轮到此为止：
                    // 不往下走 LLM（用户刚才是在下指令，不是聊天），
                    // 也不当作没命中。
                    CommandTurn::Asked => {
                        tray::set(tray::Status::Idle);
                        return Ok(State::Idle);
                    }
                    CommandTurn::NotMine => {}
                }
            }

            // —— 通话形态：说一句、答一句 ——
            //
            // **排在最后**，和知识库投递同一个道理：文字已经进了剪贴板、
            // 也上了屏，语音这条路慢一点或者失败都不会让用户白说一次。
            // 默认关（`talk_enabled`），所以已发布用户的链路一个字节都没变。
            // **只有对话模式才出声。** 模式是每轮现读的，所以菜单里切一下
            // 对下一轮立刻生效，不用重启（输入法模式走的就是 M1 那条老路）。
            if cfg.talk_mode == config::TalkMode::Conversation {
                // 这一轮用哪个语言，**在配置里读**：菜单改了下一轮生效，
                // 这就是 jason 要的「过程中可以随时切」。
                //
                // ⚠️ **必须 spawn，不能同步调用**（2026-09-20 codex 复查揪出的真
                // bug，见 ADR-0007 §8）：`answer_out_loud` 一路阻塞到播放结束，
                // 而这里是 `worker()` 主循环内部——那个循环是唯一读录音键事件
                // （`rx.try_recv()`）的地方。同步调用会让 worker 线程卡在
                // `play_blocking()` 里回不了循环顶部，于是播放期间按录音键这个
                // "V1 打断入口"永远等不到被处理的那一刻，只有播完之后才会读到
                // 那次按键——**打断形同虚设**。
                // `talk::PLAYING`/`stop_playback()` 与 `TALK_SESSION` 都已经是
                // 跨线程安全的设计（mutex 保护），`session.rs::begin_turn()` 的
                // `Phase::Speaking` 分支也是专门为"播放中又按下录音键"这个场景写的
                // ——问题只在这个调用点从没真的跑在另一个线程上，那些设计因此
                // 从未被触发过。spawn 之后，worker 循环立刻能回到 `rx.try_recv()`，
                // 用户按键时 `Intent::Begin` 分支会先 `stop_playback()` 掐掉这个
                // 线程正在放的声音，再开新一段录音。
                //
                // ⚠️ **先等边车**（2026-09-26 jason 实测的 bug）：双击切进对话模式的
                // 那一刻才在后台拉 LLM/TTS，而这一轮说完就来请求——TTS 还在加载，
                // 第一轮必然失败。等待放在这条回答线程里，**worker 照样能读按键**。
                let cfg_owned = cfg.clone();
                let text_owned = text.clone();
                let talk_lang = cfg.talk_lang;
                let seq = talk::recording_seq();
                std::thread::spawn(move || {
                    if talk::await_sidecars_for_turn(seq) {
                        answer_out_loud(&cfg_owned, &text_owned, talk_lang);
                    }
                });
            }
        }
        Ok(_) => log::warn!("转写结果为空（这段音频可能没有语音）"),
        // raw 已经安全落盘，转写失败只是丢了一次派生结果，可以重跑
        Err(e) => log::error!("转写失败（raw 音频已保留，可重试）: {e:#}"),
    }

    tray::set(tray::Status::Idle);
    Ok(State::Idle)
}

/// 指令表命中就执行，返回是否命中。
///
/// - `open_url` → 系统 `open`（**只允许 http/https/mailto**，在 `commands.rs` 里校验）
/// - `http_post` → `curl` POST 一个 JSON 到**用户自己配的** webhook（Notion / n8n / 自建）
/// - `builtin` → 本地动作（切语系/语气/模式、记一条到 kb）
///
/// ⚠️ **绝不执行 shell**：语音识别错一个字就变成在你机器上执行命令，而且没有撤销键。
/// 所以动作是一个**三种的封闭集合**，不是「随便配个命令」。
/// 一轮语音进来以后，指令表这条线自己的结论。
enum CommandTurn {
    /// 已经执行完了（或执行失败），这一轮不再往下走。
    Handled,
    /// 需要二次确认，问句已经念出去了。**这一轮不执行任何东西。**
    Asked,
    /// 跟指令表无关，交给后面的对话/上屏流程。
    NotMine,
}

/// 指令表这条线的入口：**先看是不是在回答上一个待确认**，再看是不是新指令。
///
/// 顺序是承重的：用户上一轮被问了「要发到 Notion 吗」，
/// 这一轮说「确认」——那句话要是先拿去查表，就可能被当成一条新指令。
fn run_command_turn(
    store: &store::Store,
    cfg: &config::Config,
    text: &str,
    content_hash: &str,
) -> CommandTurn {
    // ① 有待确认的动作吗？这一轮可能是在回答它
    if has_pending() {
        match commands::classify_confirmation(text) {
            commands::Reply::Confirm => {
                let Some(pending) = take_pending() else {
                    return CommandTurn::NotMine;
                };
                log::info!("用户确认了「{}」", pending.hit.phrase);
                run_confirmed(store, cfg, pending);
                return CommandTurn::Handled;
            }
            commands::Reply::Cancel => {
                drop_pending("用户说了取消");
                println!("⛔ 已取消，没有发出去");
                return CommandTurn::Handled;
            }
            commands::Reply::Other => {
                // ⚠️ **别的话一律作废待确认**，然后把这一轮当正常输入/对话。
                // 「不吭声也算」是最危险的默认值：一个走神的「嗯」就能发出去。
                drop_pending("用户说了别的话，不是确认");
                println!("（刚才那条待确认已作废）");
            }
        }
    }

    // ② 新指令？
    match decide_command(store, text) {
        Decision::None => CommandTurn::NotMine,
        Decision::AskFirst(hit) => {
            let prompt = stash_pending(hit, content_hash, text, cfg);
            announce_pending(cfg, &prompt);
            CommandTurn::Asked
        }
        Decision::RunNow(hit) => {
            let rest = hit.rest.clone();
            match execute_command(store, cfg, &hit, &rest, content_hash) {
                Ok(msg) => {
                    log::info!("指令执行完成：{msg}");
                    println!("⚡ {msg}");
                }
                // 失败**不挡上屏**：文字已经在剪贴板里了
                Err(e) => log::error!("指令执行失败（不影响上屏）: {e:#}"),
            }
            CommandTurn::Handled
        }
    }
}

/// 指令命中之后**要做什么**：立刻执行，还是先问一句。
enum Decision {
    /// 本地动作，或者用户已经确认过——直接干。
    RunNow(commands::Hit),
    /// 向外动作：**不执行**，把内容念给用户听，等第二次确认。
    AskFirst(commands::Hit),
    /// 没命中。
    None,
}

/// 只做「查表 + 判断要不要确认」，**不产生任何副作用**。
///
/// 拆出这一步是因为「确认」要跨轮次：命中的那个 `Hit` 得先存起来，
/// 等下一轮用户点头了再执行。把匹配和执行揉在一起就没法延迟执行。
fn decide_command(store: &store::Store, text: &str) -> Decision {
    let commands = match commands::load(store.root()) {
        Ok(list) => list,
        Err(e) => {
            log::error!("读指令表失败，这一轮按普通对话处理: {e:#}");
            return Decision::None;
        }
    };
    let Some(hit) = commands::match_text(&commands, text) else {
        return Decision::None;
    };
    let opt_in = commands
        .iter()
        .find(|c| c.phrase == hit.phrase)
        .map(|c| c.confirm)
        .unwrap_or(false);
    log::info!(
        "指令命中「{}」→ {:?}（槽位 {:?}，需确认 {}）",
        hit.phrase,
        hit.action,
        hit.rest,
        commands::needs_confirm(&hit.action, opt_in)
    );
    if commands::needs_confirm(&hit.action, opt_in) {
        Decision::AskFirst(hit)
    } else {
        Decision::RunNow(hit)
    }
}

// ------------------------------------------------- 待确认的向外动作
//
// **全局的**，因为确认要跨轮次：这一轮把内容念出来，下一轮用户才点头。
// 和 `talk::PLAYING` 一个道理——不为一次跨轮次的「等你确认」引入一整套
// 消息通道，一个互斥锁足够了。
//
// ⚠️ **只留一条**。挂两条待确认，用户说「确认」时我们会不知道他确认的是哪条，
// 而猜错的代价是**把错的东西发出去**。新的命中直接顶掉旧的（并把旧的作废记日志）。
static PENDING_CONFIRM: std::sync::Mutex<Option<commands::Pending>> =
    std::sync::Mutex::new(None);

/// 存一条待确认，返回它的问句（调用方负责念/打印）。
fn stash_pending(
    hit: commands::Hit,
    content_hash: &str,
    text: &str,
    cfg: &config::Config,
) -> String {
    let ttl = std::time::Duration::from_secs(cfg.command_confirm_secs.max(5));
    // 问句由 `Pending` 自己生成：**念的和以后发的同源**，不给它们分叉的机会。
    let pending = commands::Pending::new(hit, content_hash.to_string(), text, ttl);
    let prompt = pending.prompt.clone();
    let mut slot = PENDING_CONFIRM.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = slot.replace(pending) {
        // 顶掉旧的要**说出来**：静默替换会让用户以为他确认的是上一条。
        log::warn!("上一条待确认（{}）被新的顶掉，已作废", old.hit.phrase);
    }
    tray::set_pending(true);
    prompt
}

/// 取一条**还没过期**的待确认。过期的当场丢掉——一个挂着的向外动作
/// 比没有更危险。
fn take_pending() -> Option<commands::Pending> {
    let mut slot = PENDING_CONFIRM.lock().unwrap_or_else(|e| e.into_inner());
    match slot.take() {
        Some(p) if p.is_fresh() => Some(p),
        Some(p) => {
            log::info!("待确认的「{}」已过期（没等到确认），作废", p.hit.phrase);
            tray::set_pending(false);
            None
        }
        None => None,
    }
}

/// 放弃待确认（用户说了别的话 / 说了取消）。
fn drop_pending(reason: &str) {
    let mut slot = PENDING_CONFIRM.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = slot.take() {
        log::info!("待确认的「{}」被丢弃：{reason}", p.hit.phrase);
    }
    tray::set_pending(false);
}

fn has_pending() -> bool {
    PENDING_CONFIRM
        .lock()
        .map(|s| s.as_ref().map(|p| p.is_fresh()).unwrap_or(false))
        .unwrap_or(false)
}

/// 把确认问句**念出来**（对话模式才有声音），并打印。
///
/// ⚠️ 念是这层的核心动作，不是日志：用户要确认的东西**必须先被告知**。
/// 输入法模式下没有声音，所以至少要落到 stdout 和日志里，别让它变成隐形状态。
fn announce_pending(cfg: &config::Config, prompt: &str) {
    log::info!("需要二次确认（{} 秒内有效）：{prompt}", cfg.command_confirm_secs);
    println!("❓ {prompt}");
    if cfg.talk_mode == config::TalkMode::Conversation {
        let engines = talk::Engines::from_config(cfg);
        if let Err(e) = talk::speak(&engines, prompt, cfg.talk_lang) {
            log::error!("确认问句没能念出来（不影响状态，待确认仍有效）: {e:#}");
        }
    }
}

/// 执行一个已确认的待确认动作。
fn run_confirmed(store: &store::Store, cfg: &config::Config, pending: commands::Pending) {
    tray::set_pending(false);
    // ⚠️ 用 `pending.text`——**就是刚才念给用户的那份正文**，不重新推导。
    match execute_command(store, cfg, &pending.hit, &pending.text, &pending.content_hash) {
        Ok(msg) => {
            log::info!("（已确认）指令执行完成：{msg}");
            println!("⚡ {msg}");
        }
        Err(e) => log::error!("（已确认）指令执行失败: {e:#}"),
    }
}

/// 执行一个已经命中的动作。**这是唯一有副作用的地方。**
fn execute_command(
    store: &store::Store,
    cfg: &config::Config,
    hit: &commands::Hit,
    text: &str,
    content_hash: &str,
) -> Result<String> {
    let outcome: Result<String> = match &hit.action {
        // 这些分支原样保留（见下），此处只加注释：内容已在上面查过表

        commands::Action::OpenUrl { url } => {
            let target = commands::fill(url, &hit.rest, text);
            // ⚠️ **`spawn()` 成功 ≠ 这条 URL 交出去了。** `spawn()` 只说明
            // `/usr/bin/open` 这个进程起来了。`open` 很快就返回（它只是把请求
            // 交给 LaunchServices），所以等一下退出码是值得的。
            //
            // ⚠️ **但退出码的语义要说准**：它表示「有没有把 URL 交出去」，
            // **不表示那个网页能打开**。实测：`open https://不存在的域名.invalid`
            // 退出码是 **0** —— 浏览器照常打开，然后自己显示错误页。
            // 所以这里能抓到的是「没有应用处理这个 scheme」之类的失败，
            // **不要把它宣传成「验证了链接可达」**。
            let out = std::process::Command::new("/usr/bin/open")
                .arg(&target)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .map_err(|e| anyhow::anyhow!("打开 URL 失败: {e}"))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let err = err.trim();
                anyhow::bail!("打开失败（open 退出码 {:?}）：{err}", out.status.code());
            }
            Ok(format!("已打开 {target}"))
        }
        commands::Action::HttpPost { url, body } => {
            let target = commands::fill(url, &hit.rest, text);
            let payload = commands::fill(body, &hit.rest, text);
            let payload = if payload.trim().is_empty() {
                serde_json::json!({ "text": text, "rest": hit.rest }).to_string()
            } else {
                payload
            };
            // ⚠️ 这里原来有两处「报喜不报忧」，都是用户那句「你别骗我」的落点：
            // ① `stdout(Stdio::null())` —— **把对方的响应体丢了**，
            //    而 Notion 这类服务写入成功后回的正是新页面的 URL；
            //    用户问「写哪了、网址给我」时，我们手里根本没有那个答案。
            // ② `let _ = child.wait();` —— **不看退出码**，于是 HTTP 500 /
            //    401（token 过期）也照样报「已发送到 …」。没写进去却说写进去了，
            //    比失败更糟：用户会以为事情成了。
            let mut child = std::process::Command::new("/usr/bin/curl")
                // ⚠️ **`--fail-with-body` 而不是 `-f`**：`-f` 在 HTTP 出错时
                // **一个字都不输出**，于是服务端最有用的一句话被吞掉了——
                // 而那句话往往正是「哪里配错了」：
                // Notion 回 `{"message":"API token is invalid"}` /
                // `{"message":"Could not find database"}`。
                // 没有它，用户只能看到 `curl: (22) ... error: 401`，
                // 然后来问我们「为什么写不进去」。
                // （macOS 自带的 curl 8.7.1 支持，7.76+ 就有。）
                .arg("--fail-with-body")
                .arg("-sS")
                .arg("--max-time")
                .arg(cfg.talk_timeout_secs.to_string())
                .arg("-X")
                .arg("POST")
                .arg("-H")
                .arg("Content-Type: application/json")
                .arg("--data-binary")
                .arg("@-")
                .arg(&target)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| anyhow::anyhow!("webhook 调用失败: {e}"))?;
            {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(payload.as_bytes());
                }
            }
            let out = child
                .wait_with_output()
                .map_err(|e| anyhow::anyhow!("等待 webhook 返回失败: {e}"))?;
            let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let err = err.trim();
                // 把响应体也带上：很多服务把错误原因放在 body 里而不是 stderr
                let detail = if body.is_empty() {
                    err.to_string()
                } else {
                    format!("{err} / {body}")
                };
                // curl 用 `-f` 时 HTTP 错误会以非零退出，这里就是它抓到的
                anyhow::bail!("没写进去（curl 退出码 {:?}）：{detail}", out.status.code());
            }
            log::info!("webhook 返回（{} 字节）：{body}", body.len());
            // **回执要给用户看**：优先 URL / id，其次正文截断
            match commands::summarize_response(&body) {
                Some(receipt) => Ok(format!("已发送到 {target}；返回：{receipt}")),
                None => Ok(format!("已发送到 {target}（对方没有返回内容）")),
            }
        }
        commands::Action::Builtin { name, value } => match commands::BuiltinAction::parse(name) {
            Ok(commands::BuiltinAction::Style) => {
                let v = value.clone().unwrap_or_default();
                config::update(|c| c.tts_style = v.clone());
                Ok(format!("语系已切到 {v}"))
            }
            Ok(commands::BuiltinAction::Tone) => {
                let v = value.clone().unwrap_or_default();
                config::update(|c| c.tts_tone = v.clone());
                Ok(format!("语气已切到 {v}"))
            }
            Ok(commands::BuiltinAction::Mode) => {
                let v = value.clone().unwrap_or_default();
                match v.as_str() {
                    "conversation" => set_mode(config::TalkMode::Conversation),
                    "input_method" => set_mode(config::TalkMode::InputMethod),
                    other => log::error!("内置动作 mode 的值不认识: {other:?}"),
                }
                Ok(format!("模式已切到 {v}"))
            }
            Ok(commands::BuiltinAction::Note) => {
                // 记一条到 kb/：**复用现成的 routes → kb 投递链路**，不另造文件写入。
                // hash 用「这一段录音的 hash + 后缀」：指令笔记是**派生记录**，
                // 直接复用原 hash 会覆盖普通流程刚写的那条 routes。
                let route = route::Route::new(
                    format!("{content_hash}-cmd"),
                    label::Label::Note,
                    label::Source::Explicit,
                    text,
                );
                match store.write_route(&route) {
                    Ok(_) => {
                        let sink = kb::FileSink::new(store.root(), cfg.kb_root(store.root()));
                        let ix = index::Index::open(store.root()).ok();
                        deliver::attempt(store, &sink, &route, ix.as_ref());
                        Ok("已记一条到知识库".to_string())
                    }
                    Err(e) => Err(anyhow::anyhow!("写 routes 失败: {e:#}")),
                }
            }
            Err(e) => Err(e),
        },
    };
    outcome
}

/// 切模式。**按键和菜单都走这里**，副作用只有一份实现。
///
/// 三件事缺一不可：
/// 1. 写配置（下次启动还是这个模式）
/// 2. 离开对话模式时**掐掉正在播的回答**——否则留下一段没人管的音频
/// 3. 进对话模式时**去把边车弄起来**（连接优先、拉起兜底）
fn set_mode(mode: config::TalkMode) {
    if config::get().talk_mode == mode {
        return; // 已经是这个模式：不要有副作用（尤其别把正在播的掐了）
    }
    config::update(|c| c.talk_mode = mode);
    log::info!(
        "模式：{}（talk_mode = {}）",
        match mode {
            config::TalkMode::InputMethod => "输入法（只上屏，不出声）",
            config::TalkMode::Conversation => "对话（说一句答一句）",
        },
        mode.as_str()
    );
    match mode {
        config::TalkMode::InputMethod => {
            if talk::stop_playback() {
                log::info!("已切回输入法模式，掐掉正在播放的回答");
            }
        }
        config::TalkMode::Conversation => {
            // 切进对话 = 这一轮（或下一轮）要说出来，会话必须就位
            ensure_session_listening(config::get().talk_lang);
            // 异步：90 秒的就绪等待放主线程上，菜单栏会一分半不响应。
            talk::ensure_sidecars_async(&config::get());
        }
    }
}

/// 通话会话状态机（`src/session.rs`）。
///
/// **只在 `talk_enabled` 时创建**：没打通话就不该有「轮次」这个概念，
/// 否则每次普通录音都会推进一个没人看的相。
///
/// 为什么是全局而不是 `State` 的一个字段：录音键在主线程处理，而
/// 一轮的推进（转写 → LLM → 播放）在工作线程的 `finish` 里。
/// 和 `talk::PLAYING` / `tray` 用同一个套路——**跨线程共享的是一个
/// 「现在到哪一相」的事实，不是一套消息通道**。
static TALK_SESSION: std::sync::Mutex<Option<session::Session>> = std::sync::Mutex::new(None);

/// 通话开着的时候，对当前会话做一件事。
fn with_session<R>(f: impl FnOnce(&mut session::Session) -> R) -> Option<R> {
    let mut slot = TALK_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    slot.as_mut().map(f)
}

fn open_session(lang: talk::TalkLang) {
    let mut slot = TALK_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(session::Session::new(lang));
}

/// 保证会话存在，**并且把相位推到 Listening**（本轮的起点）。
///
/// ⚠️ 这一步不是可有可无的：会话可能①压根不存在（启动时是输入法模式，
/// 用户中途双击/点菜单切到对话），②存在但停在上轮的 `Idle`。
/// 这两种情况下直接走 `finish_listening` 会被状态机按非法转移拒掉，
/// **而且只写 warning**——统计出来就是自相矛盾的「0 轮」，
/// 而声音照样出得来（`--talk-turn` 当初就栽在这上面）。
///
/// 相位已经在 `Listening` 时 `begin_turn` 会报错，**那是正常的**（重复按下），
/// 所以这里吞掉返回值。
fn ensure_session_listening(lang: talk::TalkLang) {
    if with_session(|_| ()).is_none() {
        open_session(lang);
    }
    let _ = with_session(|s| s.begin_turn());
}

/// 把一句话交给 LLM，把回答念出来。**通话形态的收尾动作。**
///
/// 任何一步失败都只记日志：文字已经上屏了，用户这一轮不算白说。
/// 但**不许静默**——边车没起时日志里必须能看出是「谁没起」，
/// 否则用户只会觉得「按了没反应」。
fn answer_out_loud(cfg: &config::Config, heard: &str, lang: talk::TalkLang) {
    use crate::talk;
    // 会话可能还不存在：启动时是输入法模式、用户中途从菜单切到对话模式
    // 就是这种情况。**这里懒创建**，而不是要求菜单切换时必须成功创建——
    // 少一个「菜单点了但状态没建起来」的失败面。
    if with_session(|_| ()).is_none() {
        open_session(lang);
    }
    let engines = talk::Engines::from_config(cfg);

    // 每轮都按配置对齐一次会话语言：这是 jason 要的「过程中可以随时切」
    // 的生效点。会话自己会在不该切的时候（正在推理）拒绝，返回值记日志即可。
    if let Some(Err(e)) = with_session(|s| s.set_lang(lang)) {
        log::warn!("本轮不改通话语言: {e}");
    }
    // 录音结束 → 等结果
    if let Some(Err(e)) = with_session(|s| s.finish_listening()) {
        log::warn!("通话会话状态不对（{e}），这一轮按普通录音处理");
    }

    println!("🔊 通话（{} → {}）…", engines.llm.name(), engines.tts.name());
    // **边出边合成**：LLM 一出第一个句子就送去合成，合成一段播一段。
    // 引擎不支持流式时内部会自动退回整句路径（见 `answer_and_speak_streamed`），
    // 所以这里不必判断分支。
    //
    // ⚠️ 进 `Speaking` 相（`speaking_started`）必须发生在**第一段音频播出之前**：
    // 流式下这一刻全文还没生成完，但用户已经能按键打断了，
    // 而 `barge_in` 只在 `Speaking` 相才统计 `interrupted_playbacks`。
    let piped = talk::answer_and_speak_streamed(&engines, cfg, heard, lang, &mut || {
        // 转写为空时 `speaking_started` 不记轮次，与 `turn_ready` 一致。
        if let Some(Err(e)) = with_session(|s| s.speaking_started(heard)) {
            log::warn!("通话会话记录失败: {e}");
        }
    });
    let (reply, played) = match piped {
        Ok(v) => v,
        Err(e) => {
            log::error!("通话没拿到回答（LLM 或 TTS 边车起了吗？）: {e:#}");
            println!("（这一轮没说出来，详见日志）");
            // ⚠️ **转写照样记一轮**：用户说了什么是有价值的记录，
            // 不能因为边车挂了就当这一轮不存在（session.rs 的用例钉住这条）。
            // 记完再把相标成 Failed——通话本身还活着，下一轮照常能开。
            //
            // 两种情况要分开：已经开播过的（会话在 Speaking 相）不能再记一轮，
            // 只能补上这句话然后收尾；还没开播的才走 `turn_ready`。
            let speaking = matches!(with_session(|s| s.phase().as_str()), Some("speaking"));
            if speaking {
                with_session(|s| s.note_reply(format!("（没说出来：{e}）")));
                with_session(|s| s.speaking_done(std::time::Duration::ZERO));
                with_session(|s| s.fail(format!("这一轮没说完: {e}")));
            } else {
                if let Some(Err(e)) = with_session(|s| s.turn_ready(heard, None)) {
                    log::warn!("通话会话记录失败: {e}");
                }
                with_session(|s| s.fail(format!("这一轮没拿到回答: {e}")));
            }
            return;
        }
    };
    println!("🔊 {reply}");
    // 全文补进这一轮（进 Speaking 相时还没有它）。
    if let Some(Err(e)) = with_session(|s| s.note_reply(reply.clone())) {
        log::warn!("补记回答失败: {e}");
    }
    {
        log::info!("通话播完，用时 {:.2}s", played.as_secs_f32());
        // ⚠️ 这一轮的总时长要在 `speaking_done` **之前**读：
        // 那个调用会把计时起点清掉，之后读永远是 0（实测踩到，
        // 日志里出现过自相矛盾的「1 轮，本轮 0ms」）。
        let turn_ms = with_session(|s| s.turn_elapsed())
            .flatten()
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // 时长要回填进这一轮：它是「被打断」与「播完」唯一的客观区别
        // （被掐掉的那一轮时长会明显短于音频本身）。
        if let Some(Err(e)) = with_session(|s| s.speaking_done(played)) {
            log::warn!("通话会话收尾失败: {e}");
        }
        // 每轮都写一条 info：主循环不会正常退出，所以没有「退出时汇总」
        // 这个时机。「打断了几次」是 V1 的出口判据之一，只放 debug 会在
        // 默认日志级别下丢掉。
        if let Some(s) = with_session(|s| {
            format!(
                "通话统计：{} 轮，相 {}，语言 {}，本轮 {}ms，打断 {} 次（掐掉播放 {} 次）",
                s.turns().len(),
                s.phase().as_str(),
                s.lang().as_str(),
                turn_ms,
                s.barge_ins(),
                s.interrupted_playbacks()
            )
        }) {
            log::info!("{s}");
        }
    }
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
    talk::shutdown_spawned();
    qwen3::stop_server("进程重启");
    download::kill_curls();

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

    // Qwen3-ASR 同样是可选项，没装用 ⚪。
    {
        let c = config::get();
        println!(
            "\nQwen3-ASR（可选，设置窗口里按需下载；当前识别引擎：{}）:",
            match c.asr_backend {
                engine::AsrBackend::Builtin => "SenseVoice（随包）".to_string(),
                engine::AsrBackend::SpeechSwift => format!(
                    "{}，{}",
                    c.qwen3_model.label(),
                    if c.qwen3_resident { "常驻" } else { "逐次调用" }
                ),
            }
        );
        let rt = qwen3::runtime_installed();
        println!(
            "  {} speech-swift {} 运行时: {}",
            if rt { "✅" } else { "⚪" },
            qwen3::SPEECH_VERSION,
            qwen3::speech_bin().map(|p| p.display().to_string()).unwrap_or_default()
        );
        if rt {
            if let Some(b) = qwen3::speech_bin() {
                if qwen3::has_quarantine(&b) {
                    println!("    ⚠️ 带着 com.apple.quarantine，Gatekeeper 会拦住它——在设置里重新下载一次");
                }
            }
        }
        for m in qwen3::Qwen3Model::ALL {
            println!(
                "  {} {}（{:.0} MB）{}",
                if qwen3::model_installed(m) { "✅" } else { "⚪" },
                m.label(),
                m.total_bytes() as f64 / 1e6,
                if qwen3::model_installed(m) { "" } else { " 未下载" }
            );
        }
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

    // 通话（M3）同样是**可选**链路：两个边车都要单独起，没起不是故障。
    // 但**必须在这里能看出来**：通话失败的症状是「按了键只上屏、没有声音」，
    // 而那两个边车跑在别的进程里，光看 AgentEar 自己什么都看不出来
    // （菜单栏那三个坑记的是同一类教训：症状都是「按了没反应」）。
    println!("\n实时通话（可选，需要两个边车）:");
    println!(
        "  模式: {}（talk_mode = {}）",
        match cfg.talk_mode {
            config::TalkMode::InputMethod => "⚪ 输入法（只上屏，不出声）",
            config::TalkMode::Conversation => "✅ 对话（说一句答一句）",
        },
        cfg.talk_mode.as_str()
    );
    let talk_cfg_engines = talk::Engines::from_config(&cfg);
    let llm_url = cfg
        .talk_llm_url
        .clone()
        .unwrap_or_else(|| talk::DEFAULT_LLM_URL.to_string());
    let tts_url = cfg
        .tts_url
        .clone()
        .unwrap_or_else(|| talk::DEFAULT_TTS_URL.to_string());
    println!(
        "  LLM 引擎: {} @ {}",
        talk_cfg_engines.llm.name(),
        if cfg.talk_llm_engine == "mock" {
            "(内置 mock，不连服务)"
        } else {
            &llm_url
        }
    );
    println!(
        "  TTS 引擎: {} @ {}",
        talk_cfg_engines.tts.name(),
        talk_cfg_engines.tts.endpoint().unwrap_or("(内置 say)")
    );
    // ⚠️ 两条「谁没起」要真的**分别**记下来，最后那句警告只在**确实缺东西**时打。
    // 曾经写成无条件打——两个边车都在跑也照样喊「有一个没起」，
    // 而自检骗人比没有自检更糟（这个仓库为这条栽过不止一次）。
    let mut missing: Vec<&str> = Vec::new();
    if cfg.talk_llm_engine != "mock" {
        match talk::probe_health(&llm_url) {
            (talk::EndpointHealth::Up, _) => println!("  ✅ LLM 边车在跑"),
            (talk::EndpointHealth::Down, detail) => {
                println!("  ⚪ LLM 边车: {detail}");
                println!("     启动：scripts/serve-talk-llm.sh（首次需先跑 scripts/setup-talk.sh）");
                missing.push("LLM");
            }
            // 端口被别的程序占着：**不能**只报「没起」——用户照着去起，
            // 边车会撞端口立刻退出（2026-09-26 的真实情形）。
            (talk::EndpointHealth::WrongService, detail) => {
                println!("  ❌ {}", talk::describe_port_taken("LLM", &llm_url, &detail));
                missing.push("LLM");
            }
        }
    }
    if cfg.talk_tts_engine != "say" {
        match talk::probe_health(&tts_url) {
            (talk::EndpointHealth::Up, _) => println!("  ✅ TTS 边车在跑"),
            (talk::EndpointHealth::Down, detail) => {
                println!("  ⚪ TTS 边车: {detail}");
                println!("     启动：scripts/serve-tts.sh（首次需先跑 scripts/setup-talk.sh）");
                missing.push("TTS");
            }
            // 端口被别的程序占着：**不能**只报「没起」——用户照着去起，
            // 边车会撞端口立刻退出（2026-09-26 的真实情形）。
            (talk::EndpointHealth::WrongService, detail) => {
                println!("  ❌ {}", talk::describe_port_taken("TTS", &tts_url, &detail));
                missing.push("TTS");
            }
        }
    }
    println!("  通话语言: {}", cfg.talk_lang.as_str());
    if cfg.talk_mode == config::TalkMode::Conversation && !missing.is_empty() {
        println!(
            "  ⚠️ 对话模式开着，但 {} 边车不可用——这一轮会只有文字没有声音",
            missing.join(" / ")
        );
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

/// 取 `--flag` **紧跟着的那个值**，与参数顺序无关。
///
/// ## 为什么不能用 `args[1] == "--xxx"`
///
/// 加 `--asr-backend` 时踩到过：`--asr-backend speech_swift --transcribe x.wav`
/// 会让 `args[1]` 变成 `--asr-backend`，于是 `--transcribe` 分支被整个跳过，
/// **程序静默变成守护进程** —— 用户看到菜单栏帮助而不是转写结果，且没有任何报错。
/// 静默降级比报错难查得多。
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).map(String::as_str)
}

/// 全局选项（不属于任何子命令，可以出现在任意位置）。
///
/// 集中列出来是为了让 `args_after` 能把它们从子命令的参数里剔掉——
/// 否则 `--asr-backend x --search 关键词` 会把 `--asr-backend x`
/// 一起拼进搜索词。
const GLOBAL_FLAGS_WITH_VALUE: &[&str] = &["--asr-backend", "--lang"];

/// 取子命令 `flag` 之后的参数，**剔除全局选项及其值**。
///
/// ⚠️ 这仍然是个简化的解析器，不是完整的 CLI 解析。
/// 它挡不住「值恰好等于另一个选项名」这类情况（`--classify --search`）。
/// 真正的修法是换 `clap`，那是独立的一次重构（见 `docs/agent/tasks.md` T3.4.5），
/// 不该混在引擎适配层这个改动里。
fn args_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    let Some(i) = args.iter().position(|a| a == flag) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut skip_next = false;
    for a in &args[i + 1..] {
        if skip_next {
            skip_next = false;
            continue;
        }
        if GLOBAL_FLAGS_WITH_VALUE.contains(&a.as_str()) {
            skip_next = true;
            continue;
        }
        out.push(a.as_str());
    }
    out
}

pub(crate) fn data_root() -> Result<PathBuf> {
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

/// 测试用的临时目录。**必须能被并行跑的多个测试同时用。**
///
/// ⚠️ 原来三个模块各自写的是 `{pid}-{SystemTime::now().as_nanos()}`，
/// 看着够唯一，实际会撞：**macOS 上 realtime 时钟的分辨率约 1 µs**，
/// 实测连续两次取到的纳秒值相同的情况占 **91.6%**（20 万次里 18.3 万次）。
/// 测试是并行跑的（默认多线程），两个模块的 `tmpdir()` 落在同一个时钟刻度
/// 就指向**同一个目录**，于是互相覆盖对方写的文件。
///
/// 症状正是 FU-16 里那条查不出身份的 flake：**随机有一条用例失败，
/// 而且每次还不是同一条**。2026-09-14 复现并定位：
/// 连跑 20 次全量 `cargo test` 失败 1 次，失败用例是
/// `deliver::tests::drain_retries_what_the_last_run_left_behind` 与
/// `kb::tests::identity_is_the_full_hash_not_a_prefix`**两个不同模块里的
/// 不同用例**——单个用例自己有 bug 不会长这样，共享的临时目录才会。
///
/// 修法：名字里再加一个**进程内单调递增的计数器**，它不依赖时钟分辨率。
/// 时钟那一项保留，是为了不同进程（并行跑多份 `cargo test`）之间仍然不同。
#[cfg(test)]
pub mod testutil {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub fn tmpdir(prefix: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

#[cfg(test)]
mod cli_tests {
    use super::flag_value;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 钉住那个真踩到的 bug：子命令的识别**不能依赖参数位置**。
    ///
    /// 事故形态：`--asr-backend speech_swift --transcribe x.wav` 里
    /// `args[1]` 是 `--asr-backend`，旧写法 `args[1] == "--transcribe"`
    /// 会跳过整个分支，程序**静默变成守护进程**——没有报错，
    /// 用户只看到菜单栏帮助。
    #[test]
    fn subcommand_is_found_regardless_of_position() {
        let a = args(&["agentear", "--asr-backend", "speech_swift", "--transcribe", "x.wav"]);
        assert_eq!(flag_value(&a, "--transcribe"), Some("x.wav"));
        assert_eq!(flag_value(&a, "--asr-backend"), Some("speech_swift"));

        // 反过来放也要一样
        let b = args(&["agentear", "--transcribe", "x.wav", "--asr-backend", "builtin"]);
        assert_eq!(flag_value(&b, "--transcribe"), Some("x.wav"));
        assert_eq!(flag_value(&b, "--asr-backend"), Some("builtin"));
    }

    /// 取的是「紧跟着的那个值」，不是「出现过就算」——
    /// 否则 `--transcribe th.wav` 里的文件名会被当成语言选择。
    #[test]
    fn takes_the_value_right_after_the_flag() {
        let a = args(&["agentear", "--transcribe", "th.wav", "--lang", "auto"]);
        assert_eq!(flag_value(&a, "--lang"), Some("auto"), "不能被文件名 th.wav 干扰");
    }

    #[test]
    fn missing_value_is_none_not_panic() {
        let a = args(&["agentear", "--transcribe"]);
        assert_eq!(flag_value(&a, "--transcribe"), None);
        assert_eq!(flag_value(&a, "--nonexistent"), None);
    }
}
