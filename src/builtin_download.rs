//! SenseVoice+VAD 下载组 job（AgentEar Lite PR2）。
//!
//! 两文件一组、独立 `Mutex` 相位和取消位——不复用 `download.rs` 里泰语的
//! 单文件全局状态。复用它的底层传输/校验/锁/空间/同步函数。两份文件 SHA
//! 全过 + 真的跑一次加载冒烟（`asr::smoke_sensevoice_pair`）之后才写
//! `PairRecord`；写之前单个文件下完都不算「就绪」。

use crate::builtin_models::{self, PairRecord, SENSEVOICE, VAD};
use crate::download::{self, Fail, ModelSpec, State};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static VENDOR: OnceLock<PathBuf> = OnceLock::new();
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// 由 `main` 在启动时设进来，之后 `start`/`state` 都不再要求调用方传路径。
pub fn set_paths(vendor: PathBuf, data_root: PathBuf) {
    VENDOR.set(vendor).ok();
    DATA_ROOT.set(data_root).ok();
}

fn bin_path() -> Option<PathBuf> {
    VENDOR.get().map(|v| v.join("bin/llama-funasr-sensevoice"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    /// 已落盘字节数（两文件累计），不是百分比——给设置窗口的「已下/总共 MB」用。
    Downloading(u64),
    Verifying,
    Failed(Fail),
}

static PHASE: Mutex<Phase> = Mutex::new(Phase::Idle);
static CANCEL: AtomicBool = AtomicBool::new(false);

const TOTAL_BYTES: u64 = SENSEVOICE.bytes + VAD.bytes;

fn phase() -> Phase {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_phase(p: Phase) {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner()) = p;
}

/// 单把锁内原子检查+置相位，避免两次独立加锁留出竞态窗口。
fn try_begin_download() -> bool {
    let mut p = PHASE.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(*p, Phase::Downloading(_) | Phase::Verifying) {
        return false;
    }
    *p = Phase::Downloading(0);
    true
}

/// 当前状态。`Ready`/`Absent` 现查磁盘（含旧 bundle 兼容），其余来自相位。
pub fn state() -> State {
    match phase() {
        Phase::Downloading(done) => {
            State::Downloading((done.saturating_mul(100) / TOTAL_BYTES).min(99) as u8)
        }
        Phase::Verifying => State::Verifying,
        Phase::Failed(f) => State::Failed(f),
        Phase::Idle => match (VENDOR.get(), DATA_ROOT.get(), bin_path()) {
            (Some(v), Some(d), Some(b)) if builtin_models::resolve(v, Some(d), &b).is_some() => {
                State::Ready
            }
            _ => State::Absent,
        },
    }
}

/// 下载中的「已下 / 总共」字节数。`None` = 不在下载中。
pub fn progress_bytes() -> Option<(u64, u64)> {
    match phase() {
        Phase::Downloading(done) => Some((done, TOTAL_BYTES)),
        _ => None,
    }
}

/// 只置本组的取消位——不影响 Thai/Qwen3 各自的取消。
pub fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// 启动下载。重复点击幂等——已在下载/验证中就什么也不做。
pub fn start(on_installed: fn()) {
    let (Some(data_root), Some(bin)) = (DATA_ROOT.get().cloned(), bin_path()) else {
        log::error!("SenseVoice+VAD 下载：数据目录/vendor 还没初始化");
        return;
    };
    if !try_begin_download() {
        log::info!("SenseVoice+VAD 已在下载/验证中，忽略重复请求");
        return;
    }
    CANCEL.store(false, Ordering::SeqCst);

    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with(
                &data_root,
                &bin,
                &[SENSEVOICE, VAD],
                &|done| set_phase(Phase::Downloading(done)),
                &|| set_phase(Phase::Verifying),
                &CANCEL,
                crate::asr::smoke_sensevoice_pair,
            )
        }));
        let outcome = outcome.unwrap_or_else(|_| {
            log::error!("SenseVoice+VAD 下载线程 panic");
            Err(anyhow::Error::new(Fail::Io).context("下载线程 panic"))
        });
        match outcome {
            Ok(()) => {
                log::info!("SenseVoice+VAD 安装完成");
                set_phase(Phase::Idle);
                on_installed();
            }
            Err(e) => {
                log::error!("SenseVoice+VAD 安装失败：{e:#}");
                set_phase(Phase::Failed(
                    e.downcast_ref::<Fail>().copied().unwrap_or(Fail::Io),
                ));
            }
        }
    });
}

/// 安装逻辑，和全局相位/静态路径解耦，供生产与测试共用：生产传真实
/// `SENSEVOICE`/`VAD`，测试传本地 HTTP fixture 的微型 `ModelSpec`，
/// 走完全真实的下载/续传/校验/冒烟/落记录逻辑而不必打真网络。
///
/// ⚠️ `builtin_models::data_pair` 认的是硬编码文件名：开头的"已安装就跳过"
/// 短路对生产常量总生效，对 fixture 文件名不触发，但 `ensure_one` 自己仍会
/// 判断每个文件是否已是合法内容，同样不会重新触网。
pub(crate) fn run_with(
    data_root: &Path,
    bin: &Path,
    specs: &[ModelSpec; 2],
    on_progress: &dyn Fn(u64),
    on_verifying: &dyn Fn(),
    cancel: &AtomicBool,
    smoke: fn(&Path, &Path, &Path) -> Result<()>,
) -> Result<()> {
    let dir = data_root.join("models");
    fs::create_dir_all(&dir).with_context(|| format!("建 {} 失败", dir.display()))?;
    let lock = dir.join("sensevoice-pair.lock");
    let _guard = download::FileLock::acquire(&lock)?;

    // 拿到锁之后再查一遍：可能刚才另一个进程/线程已经装完了。
    if builtin_models::data_pair(data_root, bin).is_some() {
        log::info!("SenseVoice+VAD 已安装，跳过");
        return Ok(());
    }

    let mut done_before = 0u64;
    for spec in specs {
        ensure_one(&dir, spec, done_before, on_progress, cancel)?;
        done_before += spec.bytes;
    }

    // 冒烟在 rename 之后、写记录之前：两文件 SHA 过了还不算就绪。
    on_verifying();
    let model = dir.join(specs[0].file);
    let vad = dir.join(specs[1].file);
    smoke(bin, &model, &vad)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("加载冒烟失败: {e:#}")))?;

    let fp = builtin_models::bin_fingerprint(bin).ok_or_else(|| {
        anyhow::Error::new(Fail::Io).context("无法为当前引擎生成指纹，拒绝写安装记录")
    })?;
    // 两个 sha 字段是刚才 `ensure_one` 已经全量校验过的值，不是占位符
    // （PR1 评审 Sol 抓到的洞：旧记录只信自己声明的体积，见 `PairRecord` 注释）。
    builtin_models::write_pair_record(
        data_root,
        &PairRecord {
            sensevoice_bytes: specs[0].bytes,
            vad_bytes: specs[1].bytes,
            sensevoice_sha256: specs[0].sha256.to_string(),
            vad_sha256: specs[1].sha256.to_string(),
            bin_fingerprint: fp,
        },
    )?;
    download::sync_dir(&dir);
    Ok(())
}

/// 下载组里的一个文件：已有合法内容就复用（重哈希，不信 exists/size），
/// 否则按 `.part` 续传协议走一遍。
fn ensure_one(
    dir: &Path,
    spec: &ModelSpec,
    done_before: u64,
    on_progress: &dyn Fn(u64),
    cancel: &AtomicBool,
) -> Result<()> {
    let dest = dir.join(spec.file);
    let part = dir.join(format!("{}.part", spec.file));
    reject_bad_target(&dest)?;

    if download::is_present(&dest)
        && fs::symlink_metadata(&dest).map(|m| m.len()).unwrap_or(0) == spec.bytes
        && download::verify_file(&dest, spec.sha256, spec.bytes).is_ok()
    {
        on_progress(done_before + spec.bytes);
        return Ok(());
    }

    reject_bad_target(&part)?;
    if fs::symlink_metadata(&part).map(|m| m.len()).unwrap_or(0) > spec.bytes {
        log::warn!("{} 残留的 .part 比目标还大，删掉重下", spec.file);
        let _ = fs::remove_file(&part);
    }
    let have = fs::symlink_metadata(&part).map(|m| m.len()).unwrap_or(0);

    if have != spec.bytes {
        download::ensure_space(dir, spec.bytes - have)?;
        download::fetch_url(
            spec.url,
            spec.bytes,
            &part,
            &|len| on_progress(done_before + len),
            Some(cancel),
        )?;
    }

    download::verify_file(&part, spec.sha256, spec.bytes)?;
    fs::File::open(&part)
        .and_then(|f| f.sync_all())
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("sync .part 失败: {e}")))?;
    fs::rename(&part, &dest)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("rename 失败: {e}")))?;
    download::sync_dir(dir);
    on_progress(done_before + spec.bytes);
    Ok(())
}

/// 目标路径已被目录或符号链接占着——必须在写之前发现，不能顺着链接写进
/// 用户的别的文件，也不能把一个目录当文件 rename 过去。
fn reject_bad_target(p: &Path) -> Result<()> {
    if fs::symlink_metadata(p).is_ok_and(|m| !m.is_file()) {
        return Err(anyhow::Error::new(Fail::Io)
            .context(format!("{} 不是普通文件（目录或符号链接）", p.display())));
    }
    Ok(())
}

/// ⚠️ 测试只覆盖 `run_with`/`ensure_one`（显式传路径）。`start` 内部硬编码
/// 真实 `SENSEVOICE`/`VAD`（指向 HF pins），跑起来会打真网络——不测它和
/// `download.rs` 里泰语的 `start`/`run` 同理，不是本 PR 新开的缺口。
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// 极简单线程本地 HTTP fixture：GET 已注册路径返回固定字节（支持
    /// `Range: bytes=N-`，续传要用），未注册路径返回 404。没有 keep-alive，
    /// 每个连接处理一次就关——够用，不需要真实 HTTP 服务器的其余部分。
    struct Fixture {
        base: String,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn start(files: HashMap<&'static str, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let handle = std::thread::spawn(move || {
                while !stop2.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).ok();
                            serve_one(&mut stream, &files);
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    }
                }
            });
            Fixture {
                base: format!("http://{addr}"),
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> &'static str {
            // 测试里的 `ModelSpec.url` 要求 `&'static str`；泄漏一次没问题，
            // 测试进程本来就是一次性的。
            Box::leak(format!("{}{}", self.base, path).into_boxed_str())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn serve_one(stream: &mut TcpStream, files: &HashMap<&'static str, Vec<u8>>) {
        let mut buf = [0u8; 4096];
        let n = match stream.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ => return,
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let mut lines = req.lines();
        let path = lines
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let range_start = lines
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .and_then(|l| l.split("bytes=").nth(1))
            // 请求行按 `\r\n` 分隔，`lines()` 只认 `\n`——每行末尾会留一个
            // `\r`。必须先 `trim()` 把它去掉，再去掉 `bytes=10-` 的尾部
            // `-`；顺序反了的话 `"10-\r".trim_end_matches('-')` 什么也不
            // 会削掉（结尾字符是 `\r` 不是 `-`），`parse` 必定失败，续传
            // 请求的 `Range` 永远被当成没发，服务器只会回 200 全量——
            // curl 因此判定"看起来不支持 Range"而失败（exit 33）。
            .and_then(|r| r.trim().trim_end_matches('-').parse::<usize>().ok())
            .unwrap_or(0);

        let resp = match files.get(path.as_str()) {
            None => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            }
            Some(body) => {
                let slice = &body[range_start.min(body.len())..];
                // curl 判断"服务器真的支持续传"不只看 206，还要有
                // `Content-Range` 说清这段对应原文件的哪一段——缺了它，
                // curl 即便收到 206 也会报 "doesn't seem to support byte
                // ranges" 直接失败（本地复现过，调试这个 fixture 时踩过）。
                let (status, range_header) = if range_start > 0 {
                    (
                        "206 Partial Content",
                        format!(
                            "Content-Range: bytes {range_start}-{}/{}\r\n",
                            body.len() - 1,
                            body.len()
                        ),
                    )
                } else {
                    ("200 OK", String::new())
                };
                let mut head = format!(
                    "HTTP/1.1 {status}\r\n{range_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                )
                .into_bytes();
                head.extend_from_slice(slice);
                head
            }
        };
        let _ = stream.write_all(&resp);
    }

    fn sha256_hex(bytes: &[u8]) -> &'static str {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        Box::leak(format!("{:x}", h.finalize()).into_boxed_str())
    }

    fn noop_progress(_: u64) {}
    fn noop_verifying() {}

    fn ok_smoke(_bin: &Path, _model: &Path, _vad: &Path) -> Result<()> {
        Ok(())
    }

    /// `try_begin_download` 是检查+置相位的单把锁 CAS：并发点击（两次
    /// 调用）只有第一次能拿到，第二次必须在同一把锁里就被拒绝，不能有
    /// 竞态窗口让两边都以为自己是第一个。这是本模块唯一会碰全局 `PHASE`
    /// 的测试，结束前还原成 `Idle`，不留状态给别的测试。
    #[test]
    fn try_begin_download_is_atomic_cas() {
        set_phase(Phase::Idle);
        assert!(try_begin_download(), "第一次应该拿到");
        assert!(!try_begin_download(), "已在下载中时第二次必须被拒绝");
        set_phase(Phase::Idle);
    }

    /// RED：没有真实实现之前，`ensure_one` 对一个全新的小文件应该下完、
    /// 落到 `dest`、内容与声明的 sha256 一致。
    #[test]
    fn ensure_one_downloads_a_fresh_file_and_matches_hash() {
        let body = b"hello sensevoice fixture bytes".to_vec();
        let sha = sha256_hex(&body);
        let fx = Fixture::start(HashMap::from([("/m.bin", body.clone())]));
        let dir = crate::testutil::tmpdir("sv-dl-fresh");
        let spec = ModelSpec {
            file: "m.bin",
            url: fx.url("/m.bin"),
            sha256: sha,
            bytes: body.len() as u64,
        };
        let cancel = AtomicBool::new(false);

        ensure_one(&dir, &spec, 0, &noop_progress, &cancel).expect("应该下载成功");

        let dest = dir.join("m.bin");
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!dir.join("m.bin.part").exists(), "成功后不该留 .part");
    }

    /// 已有合法 `dest`（体积、sha 都对）时**复用、不重下**——哪怕 fixture
    /// 根本没注册这个路径，`ensure_one` 也不该去碰网络。
    #[test]
    fn ensure_one_reuses_an_existing_valid_file_without_hitting_the_network() {
        let body = b"already installed bytes".to_vec();
        let sha = sha256_hex(&body);
        let dir = crate::testutil::tmpdir("sv-dl-reuse");
        fs::write(dir.join("m.bin"), &body).unwrap();
        // 故意指向一个没人监听的地址：真去请求的话会直接连接失败。
        let spec = ModelSpec {
            file: "m.bin",
            url: "http://127.0.0.1:1",
            sha256: sha,
            bytes: body.len() as u64,
        };
        let cancel = AtomicBool::new(false);

        ensure_one(&dir, &spec, 0, &noop_progress, &cancel).expect("已有合法文件应直接复用");
        assert_eq!(fs::read(dir.join("m.bin")).unwrap(), body);
    }

    /// 断点续传：预置一份正确前缀的 `.part`，fixture 支持 `Range`，
    /// 最终文件必须是完整内容（不是前缀 + 垃圾，也不是从零重下导致的
    /// 别的内容）。
    #[test]
    fn ensure_one_resumes_from_an_existing_part_via_range() {
        let body = b"this is a longer fixture body for resume test".to_vec();
        let sha = sha256_hex(&body);
        let fx = Fixture::start(HashMap::from([("/m.bin", body.clone())]));
        let dir = crate::testutil::tmpdir("sv-dl-resume");
        fs::write(dir.join("m.bin.part"), &body[..10]).unwrap();
        let spec = ModelSpec {
            file: "m.bin",
            url: fx.url("/m.bin"),
            sha256: sha,
            bytes: body.len() as u64,
        };
        let cancel = AtomicBool::new(false);

        ensure_one(&dir, &spec, 0, &noop_progress, &cancel).expect("续传应该成功");
        assert_eq!(fs::read(dir.join("m.bin")).unwrap(), body);
    }

    /// 内容对不上声明的 sha256：删掉 `.part`，不产生 `dest`，报
    /// `Fail::Checksum`。
    #[test]
    fn ensure_one_rejects_checksum_mismatch() {
        let fx = Fixture::start(HashMap::from([("/m.bin", b"wrong-bytes".to_vec())]));
        let dir = crate::testutil::tmpdir("sv-dl-badsha");
        let spec = ModelSpec {
            file: "m.bin",
            url: fx.url("/m.bin"),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            bytes: 11,
        };
        let cancel = AtomicBool::new(false);

        let err = ensure_one(&dir, &spec, 0, &noop_progress, &cancel).unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Checksum));
        assert!(!dir.join("m.bin").exists());
        assert!(!dir.join("m.bin.part").exists());
    }

    /// 404：不产生任何文件，报 `Fail::Network`。
    #[test]
    fn ensure_one_reports_network_failure_on_404() {
        let fx = Fixture::start(HashMap::new());
        let dir = crate::testutil::tmpdir("sv-dl-404");
        let spec = ModelSpec {
            file: "missing.bin",
            url: fx.url("/missing.bin"),
            sha256: "irrelevant",
            bytes: 5,
        };
        let cancel = AtomicBool::new(false);

        let err = ensure_one(&dir, &spec, 0, &noop_progress, &cancel).unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Network));
        assert!(!dir.join("missing.bin").exists());
    }

    /// `dest` 是符号链接时拒绝，且**链接目标内容不能被动**——这条挡的是
    /// 「curl 顺着链接写、把模型字节灌进用户的别的文件」那类事故。
    #[test]
    fn ensure_one_rejects_symlink_dest_without_touching_the_link_target() {
        let dir = crate::testutil::tmpdir("sv-dl-symlink");
        let protected = dir.join("protected-user-file");
        fs::write(&protected, b"do not touch").unwrap();
        let link = dir.join("m.bin");
        std::os::unix::fs::symlink(&protected, &link).unwrap();
        let spec = ModelSpec {
            file: "m.bin",
            url: "http://127.0.0.1:1",
            sha256: "irrelevant",
            bytes: 5,
        };
        let cancel = AtomicBool::new(false);

        let err = ensure_one(&dir, &spec, 0, &noop_progress, &cancel).unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Io));
        assert_eq!(fs::read(&protected).unwrap(), b"do not touch");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// 残留的 `.part` 比目标还大（上次下的是别的东西）：删掉重下，
    /// 不能卡在一个越界偏移上永远失败。
    #[test]
    fn ensure_one_resets_an_oversized_part() {
        let body = b"correct final content".to_vec();
        let sha = sha256_hex(&body);
        let fx = Fixture::start(HashMap::from([("/m.bin", body.clone())]));
        let dir = crate::testutil::tmpdir("sv-dl-oversized-part");
        fs::write(dir.join("m.bin.part"), vec![0u8; body.len() + 100]).unwrap();
        let spec = ModelSpec {
            file: "m.bin",
            url: fx.url("/m.bin"),
            sha256: sha,
            bytes: body.len() as u64,
        };
        let cancel = AtomicBool::new(false);

        ensure_one(&dir, &spec, 0, &noop_progress, &cancel).expect("应该重下成功");
        assert_eq!(fs::read(dir.join("m.bin")).unwrap(), body);
    }

    /// 取消位在下载开始前就置位：`fetch_url` 第一次轮询就该发现，
    /// 返回 `Fail::Cancelled`，不产生最终文件。
    #[test]
    fn ensure_one_honors_a_precancelled_flag() {
        let fx = Fixture::start(HashMap::from([("/m.bin", vec![0u8; 1000])]));
        let dir = crate::testutil::tmpdir("sv-dl-cancel");
        let spec = ModelSpec {
            file: "m.bin",
            url: fx.url("/m.bin"),
            sha256: "irrelevant",
            bytes: 1000,
        };
        let cancel = AtomicBool::new(true);

        let err = ensure_one(&dir, &spec, 0, &noop_progress, &cancel).unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Cancelled));
        assert!(!dir.join("m.bin").exists());
    }

    /// 组装一对微型 `ModelSpec`，指向同一个 fixture 服务器，供
    /// `run_with` 的测试用——和生产的 `SENSEVOICE`/`VAD` 完全无关。
    fn tiny_pair(fx: &Fixture) -> (ModelSpec, ModelSpec) {
        let a = b"tiny sensevoice".to_vec();
        let b = b"tiny vad".to_vec();
        let sha_a = sha256_hex(&a);
        let sha_b = sha256_hex(&b);
        (
            ModelSpec {
                file: "a.bin",
                url: fx.url("/a.bin"),
                sha256: sha_a,
                bytes: a.len() as u64,
            },
            ModelSpec {
                file: "b.bin",
                url: fx.url("/b.bin"),
                sha256: sha_b,
                bytes: b.len() as u64,
            },
        )
    }

    /// RED/GREEN 的核心验收：**只有两文件哈希都过 + 冒烟都成功**才会落地
    /// 一份安装记录。两文件都真下载、真校验过了，但冒烟故意失败——那种
    /// 情况下不能留下任何安装记录（一个「文件在、但没验证过」的安装比
    /// 没装还糟）。
    #[test]
    fn group_is_not_ready_until_both_files_and_smoke_all_pass() {
        let a = b"tiny sensevoice".to_vec();
        let b = b"tiny vad".to_vec();
        let fx = Fixture::start(HashMap::from([
            ("/a.bin", a.clone()),
            ("/b.bin", b.clone()),
        ]));
        let (spec_a, spec_b) = tiny_pair(&fx);
        let data_root = crate::testutil::tmpdir("sv-pair-smoke-fail");
        let bin = data_root.join("fake-bin");
        fs::write(&bin, b"fake engine bytes").unwrap();
        let cancel = AtomicBool::new(false);

        fn failing_smoke(_: &Path, _: &Path, _: &Path) -> Result<()> {
            anyhow::bail!("模拟：VAD 把静音全滤掉了，没有识别标记")
        }

        let err = run_with(
            &data_root,
            &bin,
            &[spec_a, spec_b],
            &noop_progress,
            &noop_verifying,
            &cancel,
            failing_smoke,
        )
        .unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Io));
        // 两个文件各自的 SHA 校验是真的都过了——冒烟只拦就绪记录，不删已校验内容。
        assert_eq!(fs::read(data_root.join("models/a.bin")).unwrap(), a);
        assert_eq!(fs::read(data_root.join("models/b.bin")).unwrap(), b);
        assert!(
            !data_root.join("models/sensevoice-pair.installed").exists(),
            "冒烟没过就不该有安装记录"
        );
    }

    /// `run_with` 端到端：两文件下载 + 真实 hash 校验 + 冒烟成功
    /// 之后，记录才落地。冒烟换成一个总是成功的 sentinel——这里只验证
    /// "管线把下载/校验/冒烟/落记录四步正确串起来"，不验证真实引擎的
    /// 冒烟判据（那条判据的测试在 `asr.rs`）。
    ///
    /// 注意：`builtin_models::data_pair` 认的是 `SENSEVOICE.file`/`VAD.file`
    /// 这两个硬编码文件名，这里用的是 fixture 的 `a.bin`/`b.bin`，所以
    /// 直接读安装记录本身来断言，不借 `data_pair`。
    #[test]
    fn run_with_happy_path_writes_the_pair_record() {
        let a = b"tiny sensevoice".to_vec();
        let b = b"tiny vad".to_vec();
        let fx = Fixture::start(HashMap::from([
            ("/a.bin", a.clone()),
            ("/b.bin", b.clone()),
        ]));
        let (spec_a, spec_b) = tiny_pair(&fx);
        let data_root = crate::testutil::tmpdir("sv-pair-happy");
        let bin = data_root.join("fake-bin");
        fs::write(&bin, b"fake engine bytes").unwrap();
        let cancel = AtomicBool::new(false);

        run_with(
            &data_root,
            &bin,
            &[spec_a, spec_b],
            &noop_progress,
            &noop_verifying,
            &cancel,
            ok_smoke,
        )
        .expect("两文件 + 冒烟都成功，应该落地安装记录");

        let rec_path = data_root.join("models/sensevoice-pair.installed");
        let rec: PairRecord =
            serde_json::from_str(&fs::read_to_string(&rec_path).unwrap()).unwrap();
        assert_eq!(rec.sensevoice_bytes, spec_a.bytes);
        assert_eq!(rec.vad_bytes, spec_b.bytes);
        // 两个 sha 字段必须是**这次真的校验过**的值，不是随便填的占位符
        // （Sol 评审在 PR1 抓到的洞：旧记录只信自己声明的体积）。
        assert_eq!(rec.sensevoice_sha256, spec_a.sha256);
        assert_eq!(rec.vad_sha256, spec_b.sha256);
        assert_eq!(
            Some(rec.bin_fingerprint),
            builtin_models::bin_fingerprint(&bin)
        );

        // 第二次调用：每个文件已经是合法内容，`ensure_one` 会直接复用、
        // 不重新触网（fixture 没注册过第二份内容，真去下的话会落空）。
        run_with(
            &data_root,
            &bin,
            &[spec_a, spec_b],
            &noop_progress,
            &noop_verifying,
            &cancel,
            ok_smoke,
        )
        .expect("已装好时重跑应该照样成功，不触网");
    }

    /// 同一把组锁跨进程/跨线程互斥：锁被占着时 `run_with` 必须报
    /// `Fail::Busy`，不能产生"两个写者同时写同一组文件"的局面。
    #[test]
    fn run_with_reports_busy_when_the_group_lock_is_held() {
        let data_root = crate::testutil::tmpdir("sv-pair-busy");
        let dir = data_root.join("models");
        fs::create_dir_all(&dir).unwrap();
        let _held = download::FileLock::acquire(&dir.join("sensevoice-pair.lock")).unwrap();
        let (spec_a, spec_b) = tiny_pair(&Fixture::start(HashMap::new()));
        let bin = data_root.join("fake-bin");
        fs::write(&bin, b"fake engine bytes").unwrap();
        let cancel = AtomicBool::new(false);

        let err = run_with(
            &data_root,
            &bin,
            &[spec_a, spec_b],
            &noop_progress,
            &noop_verifying,
            &cancel,
            ok_smoke,
        )
        .unwrap_err();
        assert_eq!(err.downcast_ref::<Fail>(), Some(&Fail::Busy));
    }
}
