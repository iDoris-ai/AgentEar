//! raw/audio 的提交协议实现。
//!
//! 设计依据：`docs/ingest-design.md` §3.3。要点：
//!   1. 服务端生成高熵 session_id，以 O_EXCL 创建临时对象
//!   2. fsync 临时文件
//!   3. rename() 原子改名到 raw/audio/<content_hash>.wav
//!   4. **fsync 目录** —— 少了这步目录项可能丢
//!   5. 写入并 fsync 清单
//!   6. 此时才算 COMMITTED
//!
//! 崩溃语义：未 COMMITTED 的临时对象一律作废，启动时清理。
//! 不尝试从半截临时文件恢复。

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
}

/// 一次录音的提交结果。
pub struct Committed {
    pub path: PathBuf,
    pub content_hash: String,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for d in ["raw/audio", "raw/.tmp", "derived/transcripts"] {
            fs::create_dir_all(root.join(d))
                .with_context(|| format!("创建目录失败: {}", root.join(d).display()))?;
        }
        let s = Self { root };
        s.sweep_tmp()?;
        Ok(s)
    }

    /// 清理上次崩溃残留的临时对象。未 COMMITTED 的一律作废。
    fn sweep_tmp(&self) -> Result<()> {
        let tmp = self.root.join("raw/.tmp");
        let mut n = 0;
        for e in fs::read_dir(&tmp)? {
            let p = e?.path();
            if p.is_file() {
                fs::remove_file(&p)?;
                n += 1;
            }
        }
        if n > 0 {
            log::warn!("清理了 {n} 个未提交的临时录音（上次异常退出）");
        }
        Ok(())
    }

    /// 开一个录音会话。session_id 由本进程生成且高熵，不接受外部指定。
    pub fn begin(&self) -> Result<Session> {
        let session_id = new_session_id();
        let path = self.root.join("raw/.tmp").join(&session_id);
        // O_EXCL：撞上并发会话时直接失败，而不是静默复用别人的文件
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_EXCL)
            .open(&path)
            .with_context(|| format!("创建临时对象失败: {}", path.display()))?;

        Ok(Session {
            root: self.root.clone(),
            tmp_path: path,
            writer: Some(hound::WavWriter::new(
                std::io::BufWriter::new(file),
                wav_spec(),
            )?),
            hasher: Sha256::new(),
            samples: 0,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把转写结果写到 `derived/transcripts/<content_hash>.txt`。
    ///
    /// 文件名用 raw 对象的 content hash，所以转写和它的音频天然对得上，
    /// 不需要额外的索引。内容寻址的另一个红利：同一段音频重复转写会覆盖
    /// 同一个文件，不会堆出一堆重复。
    ///
    /// **刻意不走 raw 那套提交协议**：派生数据丢了可以从 raw 重算，
    /// 为它 fsync 是浪费。但仍然走临时文件 + rename，避免留下半截文件
    /// 让人误以为转写内容就那么短。
    ///
    /// 写失败不应该影响主链路——调用方只记日志，不要往上抛。
    /// 写**纠错前**的原始转写，文件名带 `.raw` 后缀。
    ///
    /// 为什么单独存一份：术语纠错是**有损**的——模型可能改错、可能过度改写。
    /// 只留纠正后的版本，等于把「模型认为的」当成了唯一记录，
    /// 而 CLAUDE.md 的存储语义要求 derived 层可以从上一层重算、可以对照。
    /// 出了问题时，这份原始转写是判断「是 ASR 错了还是 LLM 改坏了」的唯一依据。
    ///
    /// 只在**真的做了纠错且结果与原文不同**时才写——否则每次录音多出一个
    /// 内容一模一样的文件，纯属噪音。
    pub fn write_raw_transcript(&self, content_hash: &str, text: &str) -> Result<PathBuf> {
        self.write_transcript_named(&format!("{content_hash}.raw"), text)
    }

    pub fn write_transcript(&self, content_hash: &str, text: &str) -> Result<PathBuf> {
        self.write_transcript_named(content_hash, text)
    }

    fn write_transcript_named(&self, content_hash: &str, text: &str) -> Result<PathBuf> {
        let dir = self.root.join("derived/transcripts");
        let path = dir.join(format!("{content_hash}.txt"));
        let tmp = dir.join(format!(".{content_hash}.tmp"));
        fs::write(&tmp, text).with_context(|| format!("写 {} 失败", tmp.display()))?;
        fs::rename(&tmp, &path).context("rename 转写文件失败")?;
        Ok(path)
    }

    /// 写一条 `routes/` 记录。
    ///
    /// ## 这一层的定位
    ///
    /// `routes/` 是**下游决策的本地权威记录**（CLAUDE.md 的存储语义）。
    /// 它可以从 `raw` + `derived` 重算，但在重算之前它就是那份记录——
    /// 投递到知识库失败、下游服务挂了，都不影响这里已经落好的东西
    /// （架构边界 B6：先落盘再投递）。
    ///
    /// ## 按月分目录
    ///
    /// `routes/2026-09/<hash>.json`。一天几十条、一年上万条，
    /// 平铺在一个目录里会让 `ls` 和 Finder 都变慢，也不好按时间归档。
    /// 用内容哈希做文件名（和 `raw/audio/`、`derived/transcripts/` 对齐），
    /// 同一段音频重跑不会产生第二条记录——**幂等**。
    ///
    /// 写入走「临时文件 + rename」，和本模块其他写入一致：
    /// 崩在写一半不会留下半截 JSON 让下次读取失败。
    pub fn write_route(&self, record: &crate::route::Route) -> Result<PathBuf> {
        let dir = self.root.join("routes").join(record.month());
        fs::create_dir_all(&dir).with_context(|| format!("建 {} 失败", dir.display()))?;
        let path = dir.join(format!("{}.json", record.content_hash));
        let tmp = dir.join(format!(".{}.tmp", record.content_hash));
        let json = serde_json::to_string_pretty(record).context("序列化 route 失败")?;
        fs::write(&tmp, json).with_context(|| format!("写 {} 失败", tmp.display()))?;
        fs::rename(&tmp, &path).context("rename route 文件失败")?;
        Ok(path)
    }

    /// 删除 `raw/audio/` 下超过 `days` 天未修改的音频，返回删除数量。
    /// `days == 0` 表示永不清理，直接返回。
    ///
    /// **这是真实且不可逆的数据删除。** raw 是「丢了不可重建」的那一份
    /// （见 CLAUDE.md 的存储语义），所以这里刻意保守：
    ///
    /// - 只动 `raw/audio/` 下的 `*.wav`，不递归、不碰其他扩展名、不碰目录
    /// - `derived/transcripts/` 一律保留——它体积小，而且正是留档的价值所在，
    ///   过期的是几十 MB 的音频，不是几 KB 的文字
    /// - 单个文件删失败只记日志、继续处理其余的，不让一个坏文件卡住整轮清理
    ///
    /// 已知取舍：`raw/manifest.jsonl` 里对应的行不会被删掉，清理后清单会指向
    /// 不存在的对象。M1 没有消费清单的代码，等 M4 换成 SQLite 时一并处理。
    pub fn purge_older_than(&self, days: u32) -> Result<usize> {
        if days == 0 {
            return Ok(0);
        }
        let dir = self.root.join("raw/audio");
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(days as u64 * 86_400))
            .context("保留天数过大，时间计算溢出")?;

        let mut n = 0usize;
        let mut bytes = 0u64;
        for entry in fs::read_dir(&dir).with_context(|| format!("读取 {} 失败", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("wav") {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    log::error!("读取 {} 的元数据失败，跳过: {e}", path.display());
                    continue;
                }
            };
            let modified = match meta.modified() {
                Ok(t) => t,
                Err(e) => {
                    log::error!("读取 {} 的修改时间失败，跳过: {e}", path.display());
                    continue;
                }
            };
            if modified >= cutoff {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    n += 1;
                    bytes += meta.len();
                }
                Err(e) => log::error!("删除 {} 失败: {e}", path.display()),
            }
        }
        if n > 0 {
            log::info!(
                "已清理 {n} 个超过 {days} 天的 raw 音频，释放 {:.1} MB",
                bytes as f64 / 1_048_576.0
            );
        }
        Ok(n)
    }
}

pub fn wav_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}

pub struct Session {
    root: PathBuf,
    tmp_path: PathBuf,
    writer: Option<hound::WavWriter<std::io::BufWriter<File>>>,
    hasher: Sha256,
    samples: u64,
}

impl Session {
    pub fn write(&mut self, pcm: &[i16]) -> Result<()> {
        let w = self.writer.as_mut().expect("session 已提交");
        for &s in pcm {
            w.write_sample(s)?;
        }
        // content hash 覆盖 PCM 采样本身，不含 WAV 头
        self.hasher.update(bytemuck_cast(pcm));
        self.samples += pcm.len() as u64;
        Ok(())
    }

    pub fn duration_secs(&self) -> f32 {
        self.samples as f32 / 16_000.0
    }

    /// 走完整提交协议。返回后才可认为数据已持久化。
    pub fn commit(mut self) -> Result<Committed> {
        // 1. 收尾 WAV：回填头部的长度字段并 flush 到内核
        let writer = self.writer.take().expect("session 已提交");
        writer.finalize().context("WAV 收尾失败")?;

        // 2. fsync 临时文件 —— finalize 只保证写进内核缓冲，没落盘
        OpenOptions::new()
            .write(true)
            .open(&self.tmp_path)
            .context("重新打开临时文件失败")?
            .sync_all()
            .context("fsync 临时文件失败")?;

        // 3. 原子改名到内容寻址路径
        let hash = format!("{:x}", std::mem::take(&mut self.hasher).finalize());
        let final_path = self.root.join("raw/audio").join(format!("{hash}.wav"));
        fs::rename(&self.tmp_path, &final_path).context("rename 失败")?;

        // 4. fsync 目录 —— 少了这步，目录项可能在崩溃后丢失
        fsync_dir(&self.root.join("raw/audio"))?;

        // 5. 清单 + fsync
        self.append_manifest(&hash)?;

        Ok(Committed {
            path: final_path,
            content_hash: hash,
        })
    }

    fn append_manifest(&self, hash: &str) -> Result<()> {
        let manifest = self.root.join("raw/manifest.jsonl");
        let mut f = OpenOptions::new().create(true).append(true).open(&manifest)?;
        // 接收序号用行号隐含表达；M1 单进程单会话，M4 协议化时换成 SQLite（见 ingest-design §3.3）
        writeln!(
            f,
            r#"{{"content_hash":"{hash}","samples":{},"received_at":{}}}"#,
            self.samples,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        )?;
        f.sync_all().context("fsync 清单失败")?;
        drop(f);
        fsync_dir(&self.root.join("raw"))?;
        Ok(())
    }
}

fn fsync_dir(p: &Path) -> Result<()> {
    File::open(p)
        .with_context(|| format!("打开目录失败: {}", p.display()))?
        .sync_all()
        .with_context(|| format!("fsync 目录失败: {}", p.display()))
}

fn new_session_id() -> String {
    // 高熵：时间戳 + 进程 id + 随机数
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut h = Sha256::new();
    h.update(t.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(rand_bytes());
    format!("{:x}", h.finalize())[..32].to_string()
}

fn rand_bytes() -> [u8; 16] {
    let mut b = [0u8; 16];
    // getentropy(2) 在 macOS 上可用，不需要额外依赖
    unsafe {
        libc::getentropy(b.as_mut_ptr() as *mut libc::c_void, b.len());
    }
    b
}

fn bytemuck_cast(pcm: &[i16]) -> &[u8] {
    // i16 slice 的字节视图，用于 hash
    unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("agentear-test-{}", new_session_id()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn commit_lands_in_content_addressed_path() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let mut sess = s.begin().unwrap();
        sess.write(&vec![1i16; 16_000]).unwrap();
        assert!((sess.duration_secs() - 1.0).abs() < 1e-6);
        let c = sess.commit().unwrap();

        assert!(c.path.exists(), "提交后文件应存在");
        assert_eq!(c.path.file_name().unwrap(), format!("{}.wav", c.content_hash).as_str());
        // 临时目录必须已清空
        assert_eq!(fs::read_dir(root.join("raw/.tmp")).unwrap().count(), 0);
        // 清单里有一条记录
        let m = fs::read_to_string(root.join("raw/manifest.jsonl")).unwrap();
        assert!(m.contains(&c.content_hash));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn identical_audio_converges_to_same_object() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let mut a = s.begin().unwrap();
        a.write(&vec![7i16; 800]).unwrap();
        let ca = a.commit().unwrap();
        let mut b = s.begin().unwrap();
        b.write(&vec![7i16; 800]).unwrap();
        let cb = b.commit().unwrap();

        // 内容寻址：同样的音频收敛到同一个对象
        assert_eq!(ca.content_hash, cb.content_hash);
        assert_eq!(fs::read_dir(root.join("raw/audio")).unwrap().count(), 1);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn uncommitted_tmp_is_swept_on_open() {
        let root = tmpdir();
        {
            let s = Store::open(&root).unwrap();
            let mut sess = s.begin().unwrap();
            sess.write(&vec![3i16; 1000]).unwrap();
            // 模拟 kill -9：session 直接泄漏，不 commit
            std::mem::forget(sess);
        }
        assert_eq!(
            fs::read_dir(root.join("raw/.tmp")).unwrap().count(),
            1,
            "崩溃后应残留临时对象"
        );

        // 重新打开 store 时清理，且不污染 raw/audio
        let _s2 = Store::open(&root).unwrap();
        assert_eq!(fs::read_dir(root.join("raw/.tmp")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join("raw/audio")).unwrap().count(), 0);
        fs::remove_dir_all(&root).ok();
    }

    /// 把文件的 mtime 改到 `days` 天前。测试保留策略用。
    fn age_file(p: &Path, days: u64) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - days * 86_400;
        let tv = libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) }, 0);
    }

    fn commit_one(s: &Store, sample: i16) -> PathBuf {
        let mut sess = s.begin().unwrap();
        sess.write(&vec![sample; 800]).unwrap();
        sess.commit().unwrap().path
    }

    #[test]
    fn transcript_is_named_after_its_audio() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let mut sess = s.begin().unwrap();
        sess.write(&vec![5i16; 800]).unwrap();
        let c = sess.commit().unwrap();

        let p = s.write_transcript(&c.content_hash, "你好，世界。").unwrap();
        // 文件名就是音频的 content hash，转写和音频天然对得上，不需要索引
        assert_eq!(p.file_name().unwrap(), format!("{}.txt", c.content_hash).as_str());
        assert_eq!(fs::read_to_string(&p).unwrap(), "你好，世界。");

        // 重转写覆盖同一个文件，不堆重复
        s.write_transcript(&c.content_hash, "改好了。").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "改好了。");
        assert_eq!(fs::read_dir(root.join("derived/transcripts")).unwrap().count(), 1);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn purge_removes_only_expired_audio() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let old = commit_one(&s, 1);
        let fresh = commit_one(&s, 2);
        age_file(&old, 40);

        assert_eq!(s.purge_older_than(30).unwrap(), 1);
        assert!(!old.exists(), "40 天前的应被删除");
        assert!(fresh.exists(), "新录的必须留下");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn purge_zero_days_never_deletes() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let old = commit_one(&s, 3);
        age_file(&old, 9999);

        // 0 = 永不清理。这是关掉保留策略的唯一开关，必须真的什么都不做。
        assert_eq!(s.purge_older_than(0).unwrap(), 0);
        assert!(old.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn purge_leaves_transcripts_alone() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let t = root.join("derived/transcripts/old.txt");
        fs::write(&t, "转写结果可重算,但删了也没必要").unwrap();
        age_file(&t, 400);

        s.purge_older_than(30).unwrap();
        assert!(t.exists(), "清理只针对 raw 音频,不碰派生数据");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn session_ids_are_unique() {
        let n = 200;
        let set: std::collections::HashSet<_> = (0..n).map(|_| new_session_id()).collect();
        assert_eq!(set.len(), n, "session_id 必须唯一");
    }

    /// routes 记录按月分目录，文件名是内容哈希。
    #[test]
    fn route_lands_in_month_directory_named_by_hash() {
        let d = tmpdir();
        let s = Store::open(&d).unwrap();
        let mut r = crate::route::Route::new(
            "abc123",
            crate::label::Label::Idea,
            crate::label::Source::Explicit,
            "给录音笔加 WiFi",
        );
        r.created_at = "2026-09-03T10:00:00+0700".into();

        let p = s.write_route(&r).unwrap();
        assert!(p.ends_with("routes/2026-09/abc123.json"), "路径不对: {}", p.display());

        let back: crate::route::Route =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back.label, crate::label::Label::Idea);
        assert_eq!(back.text, "给录音笔加 WiFi");

        std::fs::remove_dir_all(&d).ok();
    }

    /// **同一段音频重跑不产生第二条记录**——文件名是内容哈希，天然幂等。
    ///
    /// 这条要紧：转写失败重试、用户手动重跑 --transcribe，都不该在
    /// routes 里堆出一串重复条目。
    #[test]
    fn rewriting_the_same_hash_is_idempotent() {
        let d = tmpdir();
        let s = Store::open(&d).unwrap();
        let r = crate::route::Route::new(
            "same",
            crate::label::Label::Note,
            crate::label::Source::Model,
            "第一次",
        );
        let p1 = s.write_route(&r).unwrap();

        let mut r2 = r.clone();
        r2.text = "重跑后的文本".into();
        let p2 = s.write_route(&r2).unwrap();

        assert_eq!(p1, p2, "同一个 hash 应该覆盖同一个文件");
        let dir = d.join("routes").join(r.month());
        let n = std::fs::read_dir(&dir).unwrap().filter(|e| {
            e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json"))
        }).count();
        assert_eq!(n, 1, "不该堆出重复记录");

        std::fs::remove_dir_all(&d).ok();
    }

    /// **标签识别失败（unknown）也要落盘。**
    ///
    /// 判成 unknown 是一条有用的记录；缺一条记录才是真的丢东西——
    /// 那段音频会在下游彻底消失。
    #[test]
    fn unknown_label_is_still_recorded() {
        let d = tmpdir();
        let s = Store::open(&d).unwrap();
        let r = crate::route::Route::new(
            "unk",
            crate::label::Label::Unknown,
            crate::label::Source::Model,
            "嗯这个那个",
        );
        let p = s.write_route(&r).unwrap();
        assert!(p.exists(), "unknown 也必须落盘");

        std::fs::remove_dir_all(&d).ok();
    }

    /// 写入不留半截文件：临时文件用完即走。
    #[test]
    fn no_temp_file_is_left_behind() {
        let d = tmpdir();
        let s = Store::open(&d).unwrap();
        let r = crate::route::Route::new("t", crate::label::Label::Task, crate::label::Source::Model, "x");
        s.write_route(&r).unwrap();

        let dir = d.join("routes").join(r.month());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "留下了临时文件: {leftovers:?}");

        std::fs::remove_dir_all(&d).ok();
    }
}
