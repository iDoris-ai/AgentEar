//! 投递编排：写前入队、成功出队、启动时补投（ADR-0003 §4.2）。
//!
//! ## 为什么是「先入队，再投递」
//!
//! 直觉的顺序是「投递失败了才入队」。那样有个洞：**进程在投递过程中被杀
//! （kill -9、休眠、崩溃），既没成功也没入队，这条就永远没人再管了。**
//!
//! 所以顺序是写前日志式的：
//!
//! ```text
//! write_route()  →  enqueue()  →  （剪贴板 / 上屏）  →  attempt()
//!                                                        ├ 成功 → 删 marker
//!                                                        └ 失败 → marker 留着，下次启动补投
//! ```
//!
//! 入队只是在 `routes/.pending/` 下写一个几字节的空壳文件，代价可忽略；
//! 换来的是「只要 `routes/` 写成功了，这条就一定会被投递或明确失败」。
//!
//! ## 为什么投递排在剪贴板后面
//!
//! 架构边界：**任何下游都不能挡住上屏**。文件适配器快，但组织档适配器
//! 要走网络——那时用户不该为了一次超时多等几秒才拿到自己的文字。
//! 入队已经保证了不丢，所以投递可以放心地排在后面。

use anyhow::Context;
use crate::index::Index;
use crate::kb::{KbEntry, KbSink};
use crate::route::{Delivery, DeliveryState, Route};
use crate::store::Store;
use std::fs;

/// 放弃前的最大尝试次数。
///
/// 需要一个上限：一条**永远投不进去**的记录（比如正文里带了适配器不接受的
/// 东西），没有上限就会在每次启动时重试到天荒地老，还把日志刷满。
/// 放弃不等于丢失——`routes/` 里 `state: failed` + `last_error` 是完整记录，
/// 修好问题后 `--replay-kb` 可以全部重来。
pub const MAX_ATTEMPTS: u32 = 10;

/// 记录「这条待投递」。**必须在 `write_route` 成功之后调用**——
/// marker 指向一条不存在的 route 是没有意义的。
///
/// **返回 `Result` 而不是自己吞掉**：入队失败意味着这条投递失败后
/// 不会被自动补投，调用方必须知道，才能如实告诉用户「跑一次 `--replay-kb`」。
/// 模块文档里那句「只要 routes 写成功就一定会被投递或明确失败」，
/// 前提正是这里成功。
pub fn enqueue(store: &Store, route: &Route) -> anyhow::Result<()> {
    if !crate::kb::should_deliver(route.label) {
        return Ok(());
    }
    // `content_hash` 直接变成 marker 的文件名。今天这个函数只有一个调用点，
    // 而且被 `write_route` 成功过这件事门住（那里已经验过形状）——
    // 但那是**调用顺序**给的保证，不是本地保证。多一个调用点就没了。
    anyhow::ensure!(
        crate::route::is_content_hash(&route.content_hash),
        "content_hash 形状不对，拒绝入队: {:?}",
        route.content_hash
    );
    let dir = store.pending_dir();
    fs::create_dir_all(&dir).context("建重试队列目录失败")?;
    // 临时文件 + rename：`fs::write` 会先 truncate 再写，进程在中间被杀
    // 就留下一个空 marker。而空 marker 恰恰会被 drain 当成坏数据。
    let tmp = dir.join(format!(".{}.{}.tmp", route.content_hash, std::process::id()));
    // 内容是月份，drain 靠它直接找到 `routes/<month>/<hash>.json`；
    // 内容坏了还能反查（见 `Store::find_route_month`），所以它是提示不是权威。
    fs::write(&tmp, route.month()).context("写队列条目失败")?;
    fs::rename(&tmp, dir.join(&route.content_hash)).context("rename 队列条目失败")?;
    Ok(())
}

/// 投递一条，并把结果写回 `routes/`。返回是否成功。
///
/// 失败**只记日志、不向上抛**：调用点都在主链路上，而主链路已经把
/// 文字交给用户了。
pub fn attempt(store: &Store, sink: &dyn KbSink, route: &Route, index: Option<&Index>) -> bool {
    if !crate::kb::should_deliver(route.label) {
        return false;
    }
    let entry = KbEntry::from_route(route);
    let mut updated = route.clone();

    match sink.deliver(&entry) {
        Ok(location) => {
            log::info!("已投递知识库 → {location}");
            updated.delivery = Delivery {
                state: DeliveryState::Delivered,
                attempts: route.delivery.attempts + 1,
                // 失败原因**不清空**：投成功之后它仍然是「这条曾经失败过、
                // 为什么」的证据。
                last_error: route.delivery.last_error.clone(),
                location: Some(location),
            };
            // **状态没写成就不许出队。** 否则 `routes/` 里还写着 pending、
            // 队列里却已经没有这一条，下次启动既不会重试也看不出真实结果——
            // 队列和权威记录永久失配。多投一次是幂等的，失配不是。
            index_it(store, index, updated.delivery.location.as_deref());
            if persist(store, &updated) {
                dequeue(store, &route.content_hash);
            } else {
                log::warn!("投递成功但状态没写回，marker 保留，下次启动会再投一次（幂等）");
            }
            true
        }
        Err(e) => {
            let attempts = route.delivery.attempts + 1;
            let give_up = attempts >= MAX_ATTEMPTS;
            log::error!(
                "投递知识库失败（第 {attempts} 次{}）: {e:#}",
                if give_up { "，放弃" } else { "，将重试" }
            );
            updated.delivery = Delivery {
                state: if give_up { DeliveryState::Failed } else { DeliveryState::Pending },
                attempts,
                last_error: Some(format!("{e:#}")),
                location: route.delivery.location.clone(),
            };
            // 同上：放弃是个终态，终态没落盘就不能把重试依据删掉。
            if persist(store, &updated) && give_up {
                dequeue(store, &route.content_hash);
            }
            false
        }
    }
}

/// 启动时补投上次没投成的。返回 `(成功数, 仍待投数)`。
///
/// **不因为单条失败就中断**：一条投不进去不该连累后面几十条。
pub fn drain(store: &Store, sink: &dyn KbSink, index: Option<&Index>) -> (usize, usize) {
    let dir = store.pending_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return (0, 0),
    };

    let (mut ok, mut left) = (0usize, 0usize);
    for e in entries.flatten() {
        let path = e.path();
        let Some(hash) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // 入队写了一半的临时文件，不是队列条目
        if hash.starts_with('.') {
            continue;
        }
        // 只认内容哈希形状的文件名。marker 名字会被拼进路径，
        // 手工塞一个 `../../etc/x` 进来不该能让我们去读那里。
        if !is_hash(hash) {
            log::warn!("重试队列里有不认识的文件，跳过: {}", path.display());
            continue;
        }
        // marker 的内容是**提示**：进程可能在写它的中途被杀，留下一个空文件。
        // 内容不可用就反查月份目录——一年 12 个目录，代价可忽略，
        // 而「marker 内容坏了就把这条丢掉」是把可恢复故障变成永久丢失。
        let hint = fs::read_to_string(&path).unwrap_or_default();
        let month = if is_month(&hint) {
            Some(hint)
        } else {
            let found = store.find_route_month(hash);
            if found.is_none() {
                log::error!("队列条目 {hash} 的月份无法识别且反查不到对应 route");
            }
            found
        };
        let Some(month) = month else {
            // 反查不到 = 这条 route 真的不在了（被手工删过）。
            // 留着只会让每次启动重复报同一个错。
            log::error!("待投递的记录已不存在，移出队列: {hash}");
            let _ = fs::remove_file(&path);
            continue;
        };

        match store.read_route(&month, hash) {
            Ok(r) => {
                if attempt(store, sink, &r, index) {
                    ok += 1;
                } else {
                    left += 1;
                }
            }
            Err(e) => {
                // **只有确认它不存在才移出队列。** 权限、I/O 这类暂时性错误
                // 下次可能就好了，把 marker 删掉等于把可恢复故障做成永久丢失。
                let gone = e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                if gone {
                    log::error!("待投递的记录已不存在，移出队列: {e:#}");
                    let _ = fs::remove_file(&path);
                } else {
                    log::error!("待投递的记录暂时读不出来，保留在队列里: {e:#}");
                    left += 1;
                }
            }
        }
    }
    if ok > 0 || left > 0 {
        log::info!("启动补投：成功 {ok} 条，仍待投 {left} 条");
    }
    (ok, left)
}

/// 一次投递统计。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub delivered: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// 从 `routes/` 全量重建知识库（`--replay-kb`）。
///
/// 这是 ADR-0003 §7「L1 可以从 L0 全量重放」的可执行证明，也是三种场景的
/// 唯一出路：删了 `kb/` 想重来、换了适配器要迁移、修好 bug 要补投失败的。
///
/// **重放依赖投递的幂等性**（`FileSink` 按 front matter 的 `id` 去重），
/// 所以重复跑不会堆出重复文档。
pub fn replay(store: &Store, sink: &dyn KbSink, index: Option<&Index>) -> anyhow::Result<ReplayStats> {
    let mut st = ReplayStats::default();
    let (routes, unreadable) = store.all_routes()?;
    // **读不出来的也算失败。** 「全量重建」如果悄悄漏掉几条却报告成功，
    // 自动化就发现不了数据缺口——那比直接报错更糟。
    st.failed += unreadable;
    for r in routes {
        if !crate::kb::should_deliver(r.label) {
            st.skipped += 1;
            continue;
        }
        // 重放不看已有的 delivery 状态：`delivered` 的那些文件可能已经被
        // 用户删了，而重放的意义正是「不管之前如何，让 kb/ 和 routes/ 一致」。
        if attempt(store, sink, &r, index) {
            st.delivered += 1;
        } else {
            st.failed += 1;
        }
    }
    Ok(st)
}

/// 回写投递状态，返回是否成功落盘。**调用方要看这个返回值**——
/// 出队与否取决于它。
/// 投递成功后更新 L2 索引。
///
/// **读刚写好的那个文件再解析**，而不是拿 `KbEntry` 直接建索引：
/// 这样「索引内容 == 文件内容」是由构造保证的，而不是靠两处代码
/// 各自算出同样的结果。它走的也正是 `--reindex` 的那条解析路径，
/// 增量和全量重建不会漂移。
///
/// 失败只记日志：**索引是 L2，随时可以 `--reindex` 重建**，
/// 不值得为它让一次已经成功的投递看起来像失败。
fn index_it(store: &Store, index: Option<&Index>, location: Option<&str>) {
    let (Some(ix), Some(loc)) = (index, location) else { return };
    let path = store.root().join(loc);
    match std::fs::read_to_string(&path).ok().as_deref().and_then(crate::kb::parse_document) {
        Some(doc) => {
            if let Err(e) = ix.upsert(&doc, loc) {
                log::error!("更新索引失败（可用 --reindex 重建）: {e:#}");
            }
        }
        None => log::warn!("刚投递的文档解析不回来，跳过索引: {}", path.display()),
    }
}

fn persist(store: &Store, route: &Route) -> bool {
    match store.write_route(route) {
        Ok(_) => true,
        Err(e) => {
            // 投递已经发生了，只是状态没记上。marker 会留着，
            // 下次启动重投一次——而投递是幂等的，后果只是多写一次同一个文件。
            log::error!("回写投递状态失败: {e:#}");
            false
        }
    }
}

fn dequeue(store: &Store, content_hash: &str) {
    let p = store.pending_dir().join(content_hash);
    if let Err(e) = fs::remove_file(&p) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("出队失败 {}: {e}", p.display());
        }
    }
}

// 形状判据和 `route::` 共用一份：两处各写一份迟早会漂移，
// 而它们挡的是同一件事——被拼进路径的垃圾值。
use crate::route::{is_content_hash as is_hash, is_month};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::FileSink;
    use crate::label::{Label, Source};
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "agentear-deliver-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn route(hash: &str, label: Label, text: &str) -> Route {
        let mut r = Route::new(hash, label, Source::Model, text);
        r.created_at = "2026-09-03T10:30:22+0800".into();
        r
    }

    /// 想失败就失败的适配器，用来测重试路径——文件适配器几乎不会失败，
    /// 而**重试路径正是最需要测的那条**。
    struct FlakySink {
        fail: Mutex<bool>,
        calls: Mutex<usize>,
    }
    impl FlakySink {
        fn new(fail: bool) -> Self {
            Self { fail: Mutex::new(fail), calls: Mutex::new(0) }
        }
    }
    impl KbSink for FlakySink {
        fn deliver(&self, _e: &KbEntry) -> anyhow::Result<String> {
            *self.calls.lock().unwrap() += 1;
            if *self.fail.lock().unwrap() {
                anyhow::bail!("模拟的投递失败");
            }
            Ok("kb/fake.md".into())
        }
        fn health(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn success_records_location_and_clears_the_queue() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let sink = FileSink::new(store.root(), store.root().join("kb"));
        let r = route("abc0", Label::Idea, "一个想法");
        store.write_route(&r).unwrap();
        enqueue(&store, &r).unwrap();
        assert!(store.pending_dir().join("abc0").exists(), "入队应在投递之前");

        assert!(attempt(&store, &sink, &r, None));

        let back = store.read_route("2026-09", "abc0").unwrap();
        assert_eq!(back.delivery.state, DeliveryState::Delivered);
        assert!(back.delivery.location.as_deref().is_some_and(|l| l.ends_with(".md")));
        assert!(!store.pending_dir().join("abc0").exists(), "成功后必须出队");
        fs::remove_dir_all(&d).ok();
    }

    /// **失败后 marker 必须留着**，否则这条就再也没人补投了。
    #[test]
    fn failure_keeps_the_marker_for_next_startup() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let sink = FlakySink::new(true);
        let r = route("bad0", Label::Note, "投不进去的");
        store.write_route(&r).unwrap();
        enqueue(&store, &r).unwrap();

        assert!(!attempt(&store, &sink, &r, None));
        assert!(store.pending_dir().join("bad0").exists(), "失败后 marker 不能删");

        let back = store.read_route("2026-09", "bad0").unwrap();
        assert_eq!(back.delivery.state, DeliveryState::Pending);
        assert_eq!(back.delivery.attempts, 1);
        assert!(back.delivery.last_error.is_some(), "失败原因要留证据");
        fs::remove_dir_all(&d).ok();
    }

    /// 启动补投把上次没投成的接上，且**成功后失败原因仍然留着**。
    #[test]
    fn drain_retries_what_the_last_run_left_behind() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let r = route("dd00", Label::Note, "上次没投成");
        store.write_route(&r).unwrap();
        enqueue(&store, &r).unwrap();
        attempt(&store, &FlakySink::new(true), &r, None);

        // 下次启动，这次边车/磁盘正常了
        let sink = FileSink::new(store.root(), store.root().join("kb"));
        assert_eq!(drain(&store, &sink, None), (1, 0));
        assert!(!store.pending_dir().join("dd00").exists());

        let back = store.read_route("2026-09", "dd00").unwrap();
        assert_eq!(back.delivery.state, DeliveryState::Delivered);
        assert_eq!(back.delivery.attempts, 2, "重试次数要累计，不能归零");
        assert!(back.delivery.last_error.is_some(), "投成功也不抹掉曾经失败的记录");
        fs::remove_dir_all(&d).ok();
    }

    /// 投不进去的记录不能无限重试到天荒地老。
    #[test]
    fn a_hopeless_entry_is_eventually_given_up_on() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let sink = FlakySink::new(true);
        let mut r = route("dead", Label::Note, "永远失败");
        store.write_route(&r).unwrap();
        enqueue(&store, &r).unwrap();

        for _ in 0..MAX_ATTEMPTS {
            attempt(&store, &sink, &r, None);
            r = store.read_route("2026-09", "dead").unwrap();
        }
        assert_eq!(r.delivery.attempts, MAX_ATTEMPTS);
        assert_eq!(r.delivery.state, DeliveryState::Failed);
        assert!(!store.pending_dir().join("dead").exists(), "放弃后要出队");
        // 但记录还在，改好之后 replay 能全部补回来
        assert!(r.delivery.last_error.is_some());
        fs::remove_dir_all(&d).ok();
    }

    /// `unknown` / `command` 连队都不该入——它们本来就不投递。
    #[test]
    fn undeliverable_labels_never_enter_the_queue() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let sink = FlakySink::new(false);
        for (h, l) in [("0000", Label::Unknown), ("0c00", Label::Command)] {
            let r = route(h, l, "不投递的");
            store.write_route(&r).unwrap();
            enqueue(&store, &r).unwrap();
            assert!(!store.pending_dir().join(h).exists(), "{l:?} 不该入队");
            assert!(!attempt(&store, &sink, &r, None), "{l:?} 不该投递");
        }
        assert_eq!(*sink.calls.lock().unwrap(), 0, "适配器根本不该被调到");
        fs::remove_dir_all(&d).ok();
    }

    /// 重放是幂等的：跑两遍不会得到两倍的文档。
    #[test]
    fn replay_rebuilds_the_tree_and_is_idempotent() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        for (h, l) in [
            ("1111", Label::Idea),
            ("2222", Label::Note),
            ("3333", Label::Unknown),
        ] {
            store.write_route(&route(h, l, "内容")).unwrap();
        }
        let sink = FileSink::new(store.root(), store.root().join("kb"));

        let st = replay(&store, &sink, None).unwrap();
        assert_eq!(st, ReplayStats { delivered: 2, skipped: 1, failed: 0 });
        let count = || {
            fs::read_dir(store.root().join("kb/2026/09/03"))
                .unwrap()
                .filter(|e| e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "md")))
                .count()
        };
        assert_eq!(count(), 2);

        // 再跑一遍——重放的常见用法就是反复跑，不能每跑一次翻一倍
        assert_eq!(replay(&store, &sink, None).unwrap().delivered, 2);
        assert_eq!(count(), 2, "重放必须幂等");
        fs::remove_dir_all(&d).ok();
    }

    /// 用户把 `kb/` 整个删了，重放要能原样长回来——这是「L1 可从 L0 重建」
    /// 的实际含义。
    #[test]
    fn replay_restores_a_deleted_knowledge_base() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let r = route("aa11", Label::Idea, "删了还能回来");
        store.write_route(&r).unwrap();
        let sink = FileSink::new(store.root(), store.root().join("kb"));
        attempt(&store, &sink, &r, None);

        fs::remove_dir_all(store.root().join("kb")).unwrap();
        assert_eq!(replay(&store, &sink, None).unwrap().delivered, 1);
        assert!(store.root().join("kb/2026/09/03").exists());
        fs::remove_dir_all(&d).ok();
    }

    /// 队列里的垃圾不能把我们引到别的目录去读文件。
    #[test]
    fn queue_rejects_names_and_months_that_are_not_what_they_should_be() {
        assert!(is_hash("abcdef0123"));
        assert!(!is_hash("../../etc/passwd"));
        assert!(!is_hash(""));
        assert!(!is_hash("zzzz"));
        assert!(is_month("2026-09"));
        assert!(!is_month("../.."));
        assert!(!is_month("2026-9"));

        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        fs::write(store.pending_dir().join("not-a-hash"), "2026-09").unwrap();
        fs::write(store.pending_dir().join("beef"), "../../x").unwrap();
        let sink = FlakySink::new(false);
        assert_eq!(drain(&store, &sink, None), (0, 0));
        assert_eq!(*sink.calls.lock().unwrap(), 0, "垃圾条目不该触发任何投递");
        // 形状不对的月份直接丢弃 marker；名字不对的留着不动（可能是用户的东西）
        assert!(!store.pending_dir().join("beef").exists());
        assert!(store.pending_dir().join("not-a-hash").exists());
        fs::remove_dir_all(&d).ok();
    }

    /// 入队的形状校验必须是**本地**的，不能指望调用方先过了 `write_route`。
    ///
    /// 验证它承重：删掉 `enqueue` 里的 `ensure!` 后这条必须变红。
    #[test]
    fn enqueue_validates_the_hash_itself_not_relying_on_call_order() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        let mut r = route("aaaa", Label::Note, "x");
        r.content_hash = "../../../escaped".into();

        let err = enqueue(&store, &r).expect_err("坏 hash 必须被拒绝入队");
        assert!(format!("{err:#}").contains("content_hash 形状不对"), "{err:#}");
        // 队列目录外面不能凭空多出东西
        assert!(!d.join("escaped").exists());
        fs::remove_dir_all(&d).ok();
    }

    /// marker 指向一条不存在的 route（比如 routes 被手工删过）时，
    /// 不能每次启动都重复报错。
    #[test]
    fn a_marker_pointing_nowhere_is_dropped() {
        let d = tmpdir();
        let store = Store::open(&d).unwrap();
        fs::write(store.pending_dir().join("dead0"), "2026-09").unwrap();
        let sink = FlakySink::new(false);
        assert_eq!(drain(&store, &sink, None), (0, 0));
        assert!(!store.pending_dir().join("dead0").exists());
        fs::remove_dir_all(&d).ok();
    }
}
