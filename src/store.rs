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

    #[test]
    fn session_ids_are_unique() {
        let n = 200;
        let set: std::collections::HashSet<_> = (0..n).map(|_| new_session_id()).collect();
        assert_eq!(set.len(), n, "session_id 必须唯一");
    }
}
