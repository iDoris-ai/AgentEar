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
//! 崩溃语义（T3.4.19 起）：录音中每 `CHECKPOINT_EVERY`（5 秒）把头部与数据
//! flush + fdatasync 一次，崩溃最多丢最后约 5 秒（**有界丢失**）。启动时对
//! `raw/.tmp` 里的残留做**抢救**而不是删除：按文件实际长度修 WAV 头，够长
//! 的走与正常停止相同的提交（rename → fsync 目录 → 清单），清单里标
//! `recovered:true`；过短 / 头部不是我们写的格式才删除。
//! 正在被别的进程录着的临时文件靠 `flock` 识别并跳过。

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
    /// 启动时抢救出来、还没转写的录音（`take_recovered` 取走）。
    recovered: std::sync::Mutex<Vec<Recovered>>,
}

/// 录音中途 flush + fdatasync 的间隔。崩溃 / 断电最多丢这么久的音频。
/// 取 5 秒：每次同步 ≈ 一次几十 KB 的小写（16 kHz × 16 bit = 32 KB/s），
/// 开销可忽略；再短没有意义（用户感知不到 3 秒与 5 秒的差别），再长丢得多。
pub const CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// 抢救的最短长度（采样数）= 0.3 s，与 `main.rs::finish` 的「录音过短丢弃」同一阈值。
const MIN_RECOVER_SAMPLES: u64 = 16_000 * 3 / 10;

/// `begin()` 创建临时文件时的过渡名前缀（见 `Store::begin`）。
const PENDING_PREFIX: &str = ".pending-";
/// 过渡名文件超过这么久还在，说明创建它的进程在 begin 中途死了。
const PENDING_STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// WAV 头长度：`hound` 写的是标准 44 字节（RIFF 12 + fmt 24 + data 8）。
const WAV_HEADER_LEN: usize = 44;

/// 一个被抢救提交的录音。
pub struct Recovered {
    pub committed: Committed,
    pub secs: f32,
}

/// 一次录音的提交结果。
pub struct Committed {
    pub path: PathBuf,
    pub content_hash: String,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for d in ["raw/audio", "raw/.tmp", "derived/transcripts", "routes/.pending"] {
            fs::create_dir_all(root.join(d))
                .with_context(|| format!("创建目录失败: {}", root.join(d).display()))?;
        }
        let s = Self { root, recovered: std::sync::Mutex::new(Vec::new()) };
        s.sweep_tmp()?;
        Ok(s)
    }

    /// 启动时处理上次崩溃残留的临时对象：**抢救**够长的，删除过短 / 坏头的。
    ///
    /// 任何一个文件出问题只记日志、继续下一个——启动不能因为一个坏文件起不来。
    fn sweep_tmp(&self) -> Result<()> {
        let tmp = self.root.join("raw/.tmp");
        let (mut saved, mut dropped, mut busy) = (0, 0, 0);
        for e in fs::read_dir(&tmp)? {
            let p = e?.path();
            if !p.is_file() {
                continue;
            }
            // `begin()` 先以 `.pending-` 名字创建并加锁、再改成正式名：
            // 新的 pending 可能属于正在 begin 的另一个进程，绝不碰；
            // 超过 PENDING_STALE 的才是 begin 中途崩溃留下的空壳，删掉。
            if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PENDING_PREFIX)) {
                let stale = fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age >= PENDING_STALE);
                if stale {
                    let _ = fs::remove_file(&p);
                    dropped += 1;
                }
                continue;
            }
            match self.recover_one(&p) {
                Ok(Outcome::Recovered(r)) => {
                    log::warn!(
                        "抢救了上次中断的录音 {}，时长 {:.1} 秒 → raw/audio/{}.wav",
                        p.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                        r.secs,
                        r.committed.content_hash
                    );
                    saved += 1;
                    self.recovered.lock().unwrap_or_else(|e| e.into_inner()).push(r);
                }
                Ok(Outcome::Discarded(why)) => {
                    log::warn!("丢弃残留临时录音 {}：{why}", p.display());
                    dropped += 1;
                }
                Ok(Outcome::InUse) => busy += 1,
                Err(e) => {
                    // 留着不删：下次启动再试，比丢掉一段可能能救的录音好
                    log::error!("抢救临时录音 {} 失败（保留原文件）: {e:#}", p.display());
                }
            }
        }
        if saved + dropped + busy > 0 {
            log::warn!("临时录音处理：抢救 {saved}、丢弃 {dropped}、正被其他进程使用而跳过 {busy}");
        }
        Ok(())
    }

    /// 取走启动时抢救出来的录音（只能取一次）。调用方负责补转写。
    pub fn take_recovered(&self) -> Vec<Recovered> {
        std::mem::take(&mut *self.recovered.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn recover_one(&self, tmp: &Path) -> Result<Outcome> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = OpenOptions::new().read(true).write(true).open(tmp)?;
        // 活着的录音会话持有 flock：另一个进程（比如 `--replay-kb` 这类一次性
        // 命令也会 `Store::open`）绝不能把它当残留处理。进程死了锁自动释放。
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(Outcome::InUse);
        }
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let Some(data_start) = parse_wav_header(&bytes) else {
            fs::remove_file(tmp)?;
            return Ok(Outcome::Discarded("头部不可解析".into()));
        };
        // 数据一律取到文件末尾，**不信头里声明的长度**：崩溃时头里是 0，
        // 或是最近一次 checkpoint 时的旧值，而 checkpoint 之后的数据照样在文件里。
        // 我们是这个文件的唯一写者，data 是最后一个 chunk，不会有后继 chunk。
        // 单声道 16 bit：块对齐 2 字节，丢掉末尾半个采样。
        let data_len = (bytes.len() - data_start) & !1;
        let samples = (data_len / 2) as u64;
        if samples < MIN_RECOVER_SAMPLES {
            fs::remove_file(tmp)?;
            return Ok(Outcome::Discarded(format!("过短（{samples} 个采样）")));
        }
        let pcm = &bytes[data_start..data_start + data_len];

        // 修头：截掉半个采样，回填 RIFF size 与 data size
        f.set_len((data_start + data_len) as u64)?;
        let riff = (data_start + data_len - 8) as u32;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&riff.to_le_bytes())?;
        f.seek(SeekFrom::Start((data_start - 4) as u64))?;
        f.write_all(&(data_len as u32).to_le_bytes())?;
        f.sync_all().context("fsync 修复后的临时文件失败")?;

        let hash = format!("{:x}", Sha256::digest(pcm));
        let final_path = publish_rename(&self.root, tmp, &hash)?;
        // rename 之后临时文件已经不在了：音频已安全地在 raw/audio 里。
        // 后面的目录 fsync / 清单失败不能让这条录音丢出 `take_recovered()`
        // （否则永远不会被转写、也没有重试），只告警。
        if let Err(e) = finish_publish(&self.root, &hash, samples, true) {
            log::warn!("抢救的录音 {hash} 已入库，但写清单 / fsync 失败（不影响音频）: {e:#}");
        }
        Ok(Outcome::Recovered(Recovered {
            committed: Committed { path: final_path, content_hash: hash },
            secs: samples as f32 / 16_000.0,
        }))
    }

    /// 开一个录音会话。session_id 由本进程生成且高熵，不接受外部指定。
    pub fn begin(&self) -> Result<Session> {
        let session_id = new_session_id();
        let path = self.root.join("raw/.tmp").join(&session_id);
        // 先用 sweep 会忽略的 `.pending-` 名字创建 → 加锁 → 改成正式名。
        // 直接以正式名创建的话，创建与加锁之间别的进程的 `Store::open` 可能
        // 锁到这个 0 字节文件、判为坏头并删掉（锁跟着 inode 走，改名不影响）。
        let pending = self.root.join("raw/.tmp").join(format!("{PENDING_PREFIX}{session_id}"));
        // O_EXCL：撞上并发会话时直接失败，而不是静默复用别人的文件
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_EXCL)
            .open(&pending)
            .with_context(|| format!("创建临时对象失败: {}", pending.display()))?;

        // 独占 flock：让启动抢救认得出「这个文件有人正在写」。失败只告警，
        // 最坏情况是别的进程误把它当残留（与旧行为一样），不值得拒绝录音。
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            log::warn!("临时录音文件加锁失败: {}", std::io::Error::last_os_error());
        }
        fs::rename(&pending, &path).context("临时文件改名失败")?;
        // 同一个打开文件描述的 dup：用来 fdatasync 和 pwrite 头部（不动游标）。
        let sync_file = file.try_clone().context("复制临时文件句柄失败")?;
        let sink = Sink(std::sync::Arc::new(std::sync::Mutex::new(std::io::BufWriter::new(file))));

        Ok(Session {
            root: self.root.clone(),
            tmp_path: path,
            writer: Some(hound::WavWriter::new(sink.clone(), wav_spec())?),
            sink,
            sync_file,
            last_checkpoint: std::time::Instant::now(),
            hasher: Sha256::new(),
            samples: 0,
            #[cfg(test)]
            fail_next_checkpoint: false,
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
        // `content_hash` 会直接变成文件名。正常路径下它来自 sha256，
        // 但这个函数也被投递状态回写调用，而那条 route 是从磁盘读进来的
        // ——手工编辑过的记录不该能让我们往 `routes/` 外面写。
        anyhow::ensure!(
            crate::route::is_content_hash(&record.content_hash),
            "content_hash 形状不对，拒绝写入: {:?}",
            record.content_hash
        );
        let dir = self.root.join("routes").join(record.month());
        fs::create_dir_all(&dir).with_context(|| format!("建 {} 失败", dir.display()))?;
        let path = dir.join(format!("{}.json", record.content_hash));
        let tmp = dir.join(format!(".{}.tmp", record.content_hash));
        let json = serde_json::to_string_pretty(record).context("序列化 route 失败")?;
        fs::write(&tmp, json).with_context(|| format!("写 {} 失败", tmp.display()))?;
        fs::rename(&tmp, &path).context("rename route 文件失败")?;
        Ok(path)
    }

    /// 投递重试队列所在目录（ADR-0003 §4.2）。
    ///
    /// 放在 `routes/` **里面**是有意的：待投递是 route 的一个状态，
    /// 不是另一份数据。放外面会多出一个「两个目录不同步」的失败模式。
    /// 用 `.` 开头，Obsidian / Finder 都不会把它当成内容。
    pub fn pending_dir(&self) -> PathBuf {
        self.root.join("routes/.pending")
    }

    /// 读回一条 route 记录。两个参数都会被拼进路径，所以都要先验形状。
    pub fn read_route(&self, month: &str, content_hash: &str) -> Result<crate::route::Route> {
        anyhow::ensure!(crate::route::is_month(month), "月份形状不对: {month:?}");
        anyhow::ensure!(
            crate::route::is_content_hash(content_hash),
            "content_hash 形状不对: {content_hash:?}"
        );
        let p = self.root.join("routes").join(month).join(format!("{content_hash}.json"));
        let s = fs::read_to_string(&p).with_context(|| format!("读 {} 失败", p.display()))?;
        serde_json::from_str(&s).with_context(|| format!("解析 {} 失败", p.display()))
    }

    /// 一条 route 现在在哪个月份目录里。
    ///
    /// 重试队列的 marker 里存了月份，但那是**提示**不是权威：marker 可能
    /// 在写入途中被杀而留下空文件。所以还要能反查——月份目录一年 12 个，
    /// 扫一遍的代价可以忽略，换来的是「marker 内容坏了也不丢这条」。
    pub fn find_route_month(&self, content_hash: &str) -> Option<String> {
        if !crate::route::is_content_hash(content_hash) {
            return None;
        }
        let routes = self.root.join("routes");
        let mut months: Vec<String> = fs::read_dir(&routes)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| crate::route::is_month(n) || n == "unknown")
            .collect();
        // 新的更可能命中，从后往前找
        months.sort();
        months.reverse();
        months.into_iter().find(|m| {
            routes.join(m).join(format!("{content_hash}.json")).is_file()
        })
    }

    /// 全量列出 route 记录，按月份目录名排序。
    ///
    /// 返回 `(记录, 读不出来的条数)`。单条解析失败**只跳过它**，不让整轮
    /// 失败——一个坏文件不该挡住其余上万条的重放。**但跳过的数量必须报出去**：
    /// 「全量重建」如果悄悄漏掉了几条却报告成功，自动化就发现不了数据缺口。
    pub fn all_routes(&self) -> Result<(Vec<crate::route::Route>, usize)> {
        let routes = self.root.join("routes");
        let mut months: Vec<PathBuf> = match fs::read_dir(&routes) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(e).context("读 routes/ 失败"),
        };
        months.sort();

        let mut out = Vec::new();
        let mut unreadable = 0usize;
        for m in months {
            let mut files: Vec<PathBuf> = fs::read_dir(&m)
                .with_context(|| format!("读 {} 失败", m.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect();
            files.sort();
            for f in files {
                match fs::read_to_string(&f).map_err(anyhow::Error::from).and_then(|s| {
                    serde_json::from_str::<crate::route::Route>(&s).map_err(anyhow::Error::from)
                }) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        unreadable += 1;
                        log::error!("跳过无法解析的 route {}: {e:#}", f.display());
                    }
                }
            }
        }
        Ok((out, unreadable))
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
    writer: Option<hound::WavWriter<Sink>>,
    /// 与 `writer` 共享同一个 BufWriter：checkpoint 从这里直接 flush，
    /// **不走 hound 的 `flush()`**——它会 seek 到头部改长度，中途出错时游标
    /// 停在头部，之后的采样会覆盖文件开头（评审 M1）。我们 flush 与写头都不动游标。
    sink: Sink,
    #[cfg(test)]
    fail_next_checkpoint: bool,
    /// 同一文件的另一个句柄，只用来 fdatasync。
    sync_file: File,
    last_checkpoint: std::time::Instant,
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
        // 注意：`write` 在工作线程（`worker()` 的 20ms 循环）里被调用，
        // 不是 CoreAudio 回调线程——回调线程只往 channel 里塞采样，
        // 所以这里的 fdatasync 最多让搬运晚几毫秒，不会让音频丢帧。
        if self.last_checkpoint.elapsed() >= CHECKPOINT_EVERY {
            // 刷盘失败不能打断录音（磁盘满时 write 自己也会失败，那条才该报错）
            if let Err(e) = self.checkpoint() {
                log::warn!("录音 checkpoint 失败（继续录音）: {e:#}");
            }
        }
        Ok(())
    }

    /// 测试用：模拟进程被 kill -9——BufWriter 里没刷出去的数据丢掉、不改头、
    /// 不 finalize；进程内测试里 fd 会泄漏，所以显式放锁（真死掉时内核会放）。
    #[cfg(test)]
    pub(crate) fn crash(self) {
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.sync_file), libc::LOCK_UN) };
        std::mem::forget(self);
    }

    /// 让此刻之前的数据在崩溃后可恢复：flush 共享的 BufWriter，用 `pwrite`
    /// 回填头部两个长度字段（游标不动，写位置永远在文件末尾），再 fdatasync。
    /// 任何一步失败都**不会**影响后续写入的位置；头部长度其实也不是恢复所必需
    /// （抢救按文件长度重修），只是让半截文件对普通播放器更友好。
    pub(crate) fn checkpoint(&mut self) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.last_checkpoint = std::time::Instant::now();
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_checkpoint) {
            anyhow::bail!("注入的 EIO");
        }
        self.sink.flush().context("flush 录音数据失败")?;
        let data_len = (self.samples * 2) as u32;
        self.sync_file
            .write_all_at(&(data_len + 36).to_le_bytes(), 4)
            .context("回填 RIFF size 失败")?;
        self.sync_file
            .write_all_at(&data_len.to_le_bytes(), (WAV_HEADER_LEN - 4) as u64)
            .context("回填 data size 失败")?;
        self.sync_file.sync_data().context("fdatasync 临时文件失败")?;
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

        // 3–5. rename → fsync 目录 → 清单（与启动抢救共用同一段）
        let hash = format!("{:x}", std::mem::take(&mut self.hasher).finalize());
        let final_path = publish_rename(&self.root, &self.tmp_path, &hash)?;
        finish_publish(&self.root, &hash, self.samples, false)?;

        Ok(Committed {
            path: final_path,
            content_hash: hash,
        })
    }
}

enum Outcome {
    Recovered(Recovered),
    Discarded(String),
    InUse,
}

/// 校验 `hound` 按 `wav_spec()` 写出的 44 字节头，返回 data 起点。
/// 任何偏差（不是 RIFF/WAVE、不是 16k/单声道/16bit PCM、data 不在预期位置）都返回
/// `None`——那不是我们写的文件，不去猜。
fn parse_wav_header(b: &[u8]) -> Option<usize> {
    if b.len() < WAV_HEADER_LEN || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    if &b[12..16] != b"fmt " || u32::from_le_bytes(b[16..20].try_into().ok()?) != 16 {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let spec = wav_spec();
    let ok = u16_at(20) == 1 // PCM
        && u16_at(22) == spec.channels
        && u32_at(24) == spec.sample_rate
        && u16_at(34) == spec.bits_per_sample;
    if !ok || &b[36..40] != b"data" {
        return None;
    }
    Some(WAV_HEADER_LEN)
}

/// 与 hound 共享的写入端：`Arc<Mutex<BufWriter<File>>>`。
#[derive(Clone)]
struct Sink(std::sync::Arc<std::sync::Mutex<std::io::BufWriter<File>>>);

impl Sink {
    fn inner(&self) -> std::sync::MutexGuard<'_, std::io::BufWriter<File>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}
impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.inner().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner().flush()
    }
}
impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner().seek(p)
    }
}

/// 提交协议后半段第 3 步：原子改名到内容寻址路径。调用前临时文件必须已 fsync。
fn publish_rename(root: &Path, tmp: &Path, hash: &str) -> Result<PathBuf> {
    let final_path = root.join("raw/audio").join(format!("{hash}.wav"));
    fs::rename(tmp, &final_path).context("rename 失败")?;
    Ok(final_path)
}

/// 第 4、5 步：fsync 目录 → 追加清单。
fn finish_publish(root: &Path, hash: &str, samples: u64, recovered: bool) -> Result<()> {
    fsync_dir(&root.join("raw/audio"))?;
    append_manifest(root, hash, samples, recovered)
}

fn append_manifest(root: &Path, hash: &str, samples: u64, recovered: bool) -> Result<()> {
    let manifest = root.join("raw/manifest.jsonl");
    let mut f = OpenOptions::new().create(true).append(true).open(&manifest)?;
    // 接收序号用行号隐含表达；M1 单进程单会话，M4 协议化时换成 SQLite（见 ingest-design §3.3）
    // `recovered` 只在抢救时出现；读取方应按 `#[serde(default)]`（缺省 = false）处理老行。
    let extra = if recovered { r#","recovered":true"# } else { "" };
    // 先拼成一整行、单次 write_all：O_APPEND 下单次 write 不会与别的进程的行交错
    // （`writeln!` 会拆成多次 write）。
    let line = format!(
        "{{\"content_hash\":\"{hash}\",\"samples\":{samples},\"received_at\":{}{extra}}}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    f.write_all(line.as_bytes())?;
    f.sync_all().context("fsync 清单失败")?;
    drop(f);
    fsync_dir(&root.join("raw"))?;
    Ok(())
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

    fn tmp_files(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root.join("raw/.tmp")).unwrap().map(|e| e.unwrap().path()).collect()
    }

    fn read_wav(p: &Path) -> Vec<i16> {
        hound::WavReader::open(p).unwrap().samples::<i16>().map(|s| s.unwrap()).collect()
    }

    #[test]
    fn too_short_leftover_is_deleted_on_open() {
        let root = tmpdir();
        {
            let s = Store::open(&root).unwrap();
            let mut sess = s.begin().unwrap();
            sess.write(&vec![3i16; 1000]).unwrap();
            sess.checkpoint().unwrap();
            sess.crash();
        }
        assert_eq!(tmp_files(&root).len(), 1, "崩溃后应残留临时对象");
        let s2 = Store::open(&root).unwrap();
        assert!(tmp_files(&root).is_empty());
        assert_eq!(fs::read_dir(root.join("raw/audio")).unwrap().count(), 0);
        assert!(s2.take_recovered().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn checkpointed_audio_is_recovered_and_tail_loss_is_bounded() {
        let root = tmpdir();
        let kept: Vec<i16> = (0..32_000).map(|i| (i % 1000) as i16).collect(); // 2 s
        {
            let s = Store::open(&root).unwrap();
            let mut sess = s.begin().unwrap();
            sess.write(&kept).unwrap();
            sess.checkpoint().unwrap();
            // checkpoint 之后又写了一点但没刷：崩溃后允许丢
            sess.write(&vec![7i16; 100]).unwrap();
            sess.crash();
        }
        let s2 = Store::open(&root).unwrap();
        let r = s2.take_recovered();
        assert_eq!(r.len(), 1);
        assert!(tmp_files(&root).is_empty());
        assert!((r[0].secs - 2.0).abs() < 0.01);
        assert_eq!(read_wav(&r[0].committed.path), kept);
        // 内容寻址：哈希 = PCM 的 sha256，与正常 commit 同一口径
        let mut h = Sha256::new();
        h.update(bytemuck_cast(&kept));
        assert_eq!(r[0].committed.content_hash, format!("{:x}", h.finalize()));
        let m = fs::read_to_string(root.join("raw/manifest.jsonl")).unwrap();
        assert!(m.contains(r#""recovered":true"#) && m.contains(r#""samples":32000"#), "{m}");
        assert!(s2.take_recovered().is_empty(), "只能取一次");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn normal_commit_manifest_has_no_recovered_key() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        commit_one(&s, 5);
        let m = fs::read_to_string(root.join("raw/manifest.jsonl")).unwrap();
        assert!(!m.contains("recovered"), "{m}");
        fs::remove_dir_all(&root).ok();
    }

    /// 头里全是 0（hound 刚 new 时的样子）、数据已在文件里：按文件长度修头。
    /// 同时覆盖奇数字节：末尾半个采样被丢掉。
    #[test]
    fn zeroed_header_and_odd_tail_are_repaired() {
        let root = tmpdir();
        Store::open(&root).unwrap();
        let pcm: Vec<i16> = (0..8000).map(|i| (i % 300) as i16 - 150).collect();
        let mut bytes = Vec::new();
        {
            let mut c = std::io::Cursor::new(Vec::new());
            let mut w = hound::WavWriter::new(&mut c, wav_spec()).unwrap();
            for &x in &pcm {
                w.write_sample(x).unwrap();
            }
            w.finalize().unwrap();
            bytes.extend_from_slice(&c.into_inner());
        }
        bytes[4..8].fill(0); // RIFF size
        bytes[40..44].fill(0); // data size
        bytes.push(0xAB); // 半个采样
        fs::write(root.join("raw/.tmp/crashed"), &bytes).unwrap();

        let s = Store::open(&root).unwrap();
        let r = s.take_recovered();
        assert_eq!(r.len(), 1);
        assert_eq!(read_wav(&r[0].committed.path), pcm);
        assert_eq!(fs::metadata(&r[0].committed.path).unwrap().len(), 44 + 16_000);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn garbage_and_truncated_headers_are_deleted_without_panic() {
        let root = tmpdir();
        Store::open(&root).unwrap();
        fs::write(root.join("raw/.tmp/empty"), b"").unwrap();
        fs::write(root.join("raw/.tmp/half_header"), &b"RIFF\0\0\0\0WAVEfmt "[..]).unwrap();
        fs::write(root.join("raw/.tmp/garbage"), vec![0xFFu8; 50_000]).unwrap();
        // 头合法但格式不是我们写的（44.1 kHz）
        let mut c = std::io::Cursor::new(Vec::new());
        let mut spec = wav_spec();
        spec.sample_rate = 44_100;
        let mut w = hound::WavWriter::new(&mut c, spec).unwrap();
        for _ in 0..20_000 {
            w.write_sample(1i16).unwrap();
        }
        w.finalize().unwrap();
        fs::write(root.join("raw/.tmp/other_rate"), c.into_inner()).unwrap();

        let s = Store::open(&root).unwrap();
        assert!(s.take_recovered().is_empty());
        assert!(tmp_files(&root).is_empty());
        assert_eq!(fs::read_dir(root.join("raw/audio")).unwrap().count(), 0);
        fs::remove_dir_all(&root).ok();
    }

    /// 别的进程正在录的文件（持有 flock）绝不能被当残留处理。
    #[test]
    fn live_session_is_not_touched_by_a_second_open() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let mut sess = s.begin().unwrap();
        sess.write(&vec![1i16; 40_000]).unwrap();
        sess.checkpoint().unwrap();
        // 同进程里另开一个 Store 等价于另一个进程的 `Store::open`（flock 按打开文件描述冲突）
        let s2 = Store::open(&root).unwrap();
        assert!(s2.take_recovered().is_empty());
        assert_eq!(tmp_files(&root).len(), 1, "活会话的临时文件必须还在");
        sess.write(&vec![1i16; 100]).unwrap();
        let c = sess.commit().unwrap();
        assert_eq!(read_wav(&c.path).len(), 40_100);
        fs::remove_dir_all(&root).ok();
    }

    /// M1：checkpoint 失败后继续录音，之后的采样必须仍然追加在末尾，
    /// 不能覆盖文件开头（旧实现靠 hound 的 flush，出错时游标会停在头部）。
    #[test]
    fn failed_checkpoint_does_not_corrupt_later_samples() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let mut sess = s.begin().unwrap();
        let a: Vec<i16> = (0..16_000).map(|i| (i % 500) as i16).collect();
        let b: Vec<i16> = (0..16_000).map(|i| -((i % 400) as i16)).collect();
        sess.write(&a).unwrap();
        sess.fail_next_checkpoint = true;
        assert!(sess.checkpoint().is_err());
        sess.write(&b).unwrap();
        sess.checkpoint().unwrap(); // 下一次正常 checkpoint 恢复
        sess.write(&a).unwrap();
        let c = sess.commit().unwrap();
        let want: Vec<i16> = a.iter().chain(&b).chain(&a).copied().collect();
        assert_eq!(read_wav(&c.path), want);
        let mut h = Sha256::new();
        h.update(bytemuck_cast(&want));
        assert_eq!(c.content_hash, format!("{:x}", h.finalize()));
        fs::remove_dir_all(&root).ok();
    }

    /// checkpoint 之后的头部长度是对的（pwrite 回填），不靠抢救也能读出已刷的部分。
    #[test]
    fn checkpoint_backfills_header_lengths() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let mut sess = s.begin().unwrap();
        sess.write(&vec![5i16; 20_000]).unwrap();
        sess.checkpoint().unwrap();
        let f = tmp_files(&root).pop().unwrap();
        assert_eq!(read_wav(&f).len(), 20_000);
        sess.write(&vec![5i16; 10]).unwrap();
        sess.commit().unwrap();
        fs::remove_dir_all(&root).ok();
    }

    /// M2：begin 创建与加锁之间别的进程不能删掉它——过渡名 `.pending-*`
    /// 新的不碰、陈旧的清掉；begin 完成后文件用正式名、无 pending 残留。
    #[test]
    fn fresh_pending_file_is_left_alone_and_stale_one_is_removed() {
        let root = tmpdir();
        Store::open(&root).unwrap();
        let fresh = root.join("raw/.tmp/.pending-fresh");
        let stale = root.join("raw/.tmp/.pending-stale");
        fs::write(&fresh, b"").unwrap();
        fs::write(&stale, b"").unwrap();
        age_file(&stale, 1);
        let s = Store::open(&root).unwrap();
        assert!(fresh.exists(), "新的 pending 可能属于正在 begin 的进程");
        assert!(!stale.exists());
        let sess = s.begin().unwrap();
        let names: Vec<String> = tmp_files(&root)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().filter(|n| !n.starts_with(".pending-")).count() == 1, "{names:?}");
        sess.crash();
        fs::remove_dir_all(&root).ok();
    }

    /// M3：rename 之后清单写不进去（manifest.jsonl 是目录），录音仍要进 take_recovered。
    #[test]
    fn manifest_failure_after_rename_still_reports_recovered() {
        let root = tmpdir();
        let kept = vec![4i16; 16_000];
        {
            let s = Store::open(&root).unwrap();
            let mut sess = s.begin().unwrap();
            sess.write(&kept).unwrap();
            sess.checkpoint().unwrap();
            sess.crash();
        }
        fs::create_dir(root.join("raw/manifest.jsonl")).unwrap();
        let s2 = Store::open(&root).unwrap();
        let r = s2.take_recovered();
        assert_eq!(r.len(), 1, "音频已入库，必须交给调用方转写");
        assert_eq!(read_wav(&r[0].committed.path), kept);
        assert!(tmp_files(&root).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    /// L1：清单每行是一次 write_all 写出的完整一行。
    #[test]
    fn manifest_rows_are_whole_lines() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        commit_one(&s, 1);
        commit_one(&s, 2);
        let m = fs::read_to_string(root.join("raw/manifest.jsonl")).unwrap();
        assert_eq!(m.lines().count(), 2);
        assert!(m.ends_with('\n') && m.lines().all(|l| l.starts_with('{') && l.ends_with('}')));
        fs::remove_dir_all(&root).ok();
    }

    const CRASH_CHILD_ENV: &str = "AGENTEAR_STORE_CRASH_CHILD";

    /// 真 kill -9：子进程（本测试二进制自己）录一段、checkpoint、再写点没刷的，
    /// 父进程 SIGKILL 它，然后重新打开 Store 断言找回。不碰麦克风。
    #[test]
    fn kill_9_mid_recording_is_recovered_on_next_open() {
        if let Ok(root) = std::env::var(CRASH_CHILD_ENV) {
            let s = Store::open(&root).unwrap();
            let mut sess = s.begin().unwrap();
            sess.write(&vec![9i16; 48_000]).unwrap(); // 3 s
            sess.checkpoint().unwrap();
            sess.write(&vec![8i16; 500]).unwrap();
            println!("READY");
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        use std::io::{BufRead, BufReader};
        let root = tmpdir();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::tests::kill_9_mid_recording_is_recovered_on_next_open", "--nocapture", "--test-threads=1"])
            .env(CRASH_CHILD_ENV, &root)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        loop {
            let l = lines.next().expect("子进程提前退出").unwrap();
            if l.contains("READY") {
                break;
            }
        }
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();

        let s = Store::open(&root).unwrap();
        let r = s.take_recovered();
        assert_eq!(r.len(), 1, "kill -9 后应找回这段录音");
        assert!((r[0].secs - 3.0).abs() < 0.01, "secs={}", r[0].secs);
        assert!(read_wav(&r[0].committed.path).iter().all(|&x| x == 9));
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

    /// **`write_route` 的哈希守卫是那条路径上最后一道，不是装饰。**
    ///
    /// 链路：`all_routes()` 只反序列化 JSON **正文**，不校验正文里的
    /// `content_hash` 跟文件名对不对得上 → `--replay-kb` 遍历它 →
    /// `deliver::attempt` → `persist` → `write_route`。所以一条
    /// **文件名合法、正文里写着 `../../..`** 的记录（手工编辑过的、
    /// 从别处导入的、写坏的）会一路走到 `write_route`，只有那句
    /// `ensure!` 挡得住。
    ///
    /// 验证这条测试确实承重：把 `write_route` 里的 `ensure!` 整块删掉，
    /// 它必须变红。
    #[test]
    fn a_route_whose_body_lies_about_its_hash_cannot_escape_routes() {
        let root = tmpdir();
        let s = Store::open(&root).unwrap();
        let dir = root.join("routes/2026-09");
        fs::create_dir_all(&dir).unwrap();
        // 文件名合法，正文里的 content_hash 指到外面
        let evil = r#"{"content_hash":"../../../escaped","created_at":"2026-09-03T10:00:00+08:00","label":"note","label_source":"model","text":"t","secondary":[],"delivery":{"state":"pending","attempts":0,"last_error":null,"location":null}}"#;
        fs::write(dir.join("deadbeef00.json"), evil).unwrap();

        // 这两格**也是承重的**：哪天 `all_routes` 自己开始校验正文，
        // 它们会先红，提醒你这条测试的前提变了——而不是让它在一个
        // 空集合上静默通过。
        let (routes, _) = s.all_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].content_hash, "../../../escaped");

        // 唯一挡住它的是 write_route。
        //
        // **必须断言到错误原因，光断言 `is_err()` 接不住。** 实测：把守卫
        // 删掉之后 `write_route` 仍然报错，但那是碰巧的——临时文件名
        // `format!(".{content_hash}.tmp")` 拼出了一个带 `...` 字面量目录的
        // 路径，`fs::write` 因为那个目录不存在而失败。换一个不带 `..` 前缀的
        // 恶意 hash（比如绝对路径）就不会有这个巧合。
        let err = s.write_route(&routes[0]).expect_err("正文里的坏 hash 必须被拒绝");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("content_hash 形状不对"),
            "必须是守卫拒绝的，而不是碰巧写失败: {msg}"
        );

        // 绝对路径版：没有 `...` 那个巧合，守卫是唯一拦得住的东西。
        let mut abs = routes[0].clone();
        abs.content_hash = format!("{}/pwned", root.display());
        let err = s.write_route(&abs).expect_err("绝对路径的 hash 必须被拒绝");
        assert!(format!("{err:#}").contains("content_hash 形状不对"));
        assert!(!root.join("pwned.json").exists(), "不能写到 routes/ 之外");
        fs::remove_dir_all(&root).ok();
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
            "5a3e",
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
            "4e6b",
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
        let r = crate::route::Route::new("7a", crate::label::Label::Task, crate::label::Source::Model, "x");
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
