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
mod correct;
mod deliver;
mod hotkey;
mod i18n;
mod index;
mod kb;
mod label;
mod paste;
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
    let backend = if args.iter().any(|a| a == "--asr-backend") {
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

    let asr = engine::build(backend, &vendor, Some(&data_root))?;
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
        let reply = talk::answer(&engines, &cfg, &text, lang)?;
        println!("\n问：{text}\n答：{reply}\n");
        let played = talk::speak(&engines, &reply, lang)?;
        println!("（已播放 {:.2}s）", played.as_secs_f32());
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
            match hotkey::intent(signal, recording) {
                // 正在录音时收到双击：只切模式，**不要停**——否则「快速点两下」
                // 会把刚开的那段录成 0.2 秒碎片（会被当噪音丢掉）。
                hotkey::Intent::SwitchToConversation => {
                    log::info!("双击右 Command → 切到对话模式（这一段继续录）");
                    set_mode(config::TalkMode::Conversation);
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
                answer_out_loud(&cfg, &text, cfg.talk_lang);
            }
        }
        Ok(_) => log::warn!("转写结果为空（这段音频可能没有语音）"),
        // raw 已经安全落盘，转写失败只是丢了一次派生结果，可以重跑
        Err(e) => log::error!("转写失败（raw 音频已保留，可重试）: {e:#}"),
    }

    tray::set(tray::Status::Idle);
    Ok(State::Idle)
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
    let reply = match talk::answer(&engines, cfg, heard, lang) {
        Ok(reply) => reply,
        Err(e) => {
            log::error!("通话没拿到回答（LLM 边车起了吗？scripts/serve-talk-llm.sh）: {e:#}");
            println!("（没拿到回答，详见日志）");
            // ⚠️ **转写照样记一轮**：用户说了什么是有价值的记录，
            // 不能因为 LLM 挂了就当这一轮不存在（session.rs 的用例钉住这条）。
            // 记完再把相标成 Failed——通话本身还活着，下一轮照常能开。
            if let Some(Err(e)) = with_session(|s| s.turn_ready(heard, None)) {
                log::warn!("通话会话记录失败: {e}");
            }
            with_session(|s| s.fail(format!("这一轮没拿到回答: {e}")));
            return;
        }
    };
    println!("🔊 {reply}");
    if let Some(Err(e)) = with_session(|s| s.turn_ready(heard, Some(reply.clone()))) {
        log::warn!("通话会话记录失败: {e}");
    }
    match talk::speak(&engines, &reply, lang) {
        Ok(played) => {
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
        Err(e) => {
            log::error!("通话合成/播放失败（TTS 边车起了吗？scripts/serve-tts.sh）: {e:#}");
            println!("（语音没出来，详见日志）");
            if let Some(Err(e)) = with_session(|s| s.speaking_done(std::time::Duration::ZERO)) {
                log::warn!("通话会话收尾失败: {e}");
            }
            with_session(|s| s.fail(format!("这一轮没播出来: {e}")));
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
        match talk::probe_endpoint(&llm_url) {
            Ok(()) => println!("  ✅ LLM 边车在跑"),
            Err(e) => {
                println!("  ⚪ LLM 边车: {e}");
                println!("     启动：scripts/serve-talk-llm.sh（首次需先跑 scripts/setup-talk.sh）");
                missing.push("LLM");
            }
        }
    }
    if cfg.talk_tts_engine != "say" {
        match talk::probe_endpoint(&tts_url) {
            Ok(()) => println!("  ✅ TTS 边车在跑"),
            Err(e) => {
                println!("  ⚪ TTS 边车: {e}");
                println!("     启动：scripts/serve-tts.sh（首次需先跑 scripts/setup-talk.sh）");
                missing.push("TTS");
            }
        }
    }
    println!("  通话语言: {}", cfg.talk_lang.as_str());
    if cfg.talk_mode == config::TalkMode::Conversation && !missing.is_empty() {
        println!(
            "  ⚠️ 对话模式开着，但 {} 边车没起——这一轮会只有文字没有声音",
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
