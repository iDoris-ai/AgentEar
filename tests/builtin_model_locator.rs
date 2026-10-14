//! PR1 RED3：进程级验收——`AGENTEAR_DATA`/`AGENTEAR_VENDOR` 指向临时目录、
//! 只有 bin fixture、没有模型时，`--diagnose` 必须到达完整报告并 exit 0；
//! `--transcribe` 一个合成 wav 必须非零退出，且绝不调用引擎二进制
//! （sentinel 0 次），不能产出空成功转写。
//!
//! 修复前：`main()` 在 `build_dynamic`/`preflight` 那一步先因为模型缺失而
//! `?` 掉，`--diagnose` 根本走不到自己的报告逻辑——这正是本文件要钉住的 RED。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 临时 vendor：只放一个 sentinel bin，不放任何模型/VAD/whisper。
/// bin 被调用一次就在 `base` 下留一个 `called` 标记文件。
fn fixture_vendor(base: &Path) -> PathBuf {
    let vendor = base.join("vendor");
    let bin = vendor.join("bin/llama-funasr-sensevoice");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(
        &bin,
        format!("#!/bin/sh\ntouch {}\n", base.join("called").display()),
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    vendor
}

fn run(vendor: &std::path::Path, data: &std::path::Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentear"))
        .args(extra)
        .env("AGENTEAR_VENDOR", vendor)
        .env("AGENTEAR_DATA", data)
        .output()
        .expect("跑二进制失败")
}

#[test]
fn diagnose_finishes_with_exit_0_when_models_are_missing() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-diagnose-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();

    let out = run(&vendor, &data, &["--diagnose"]);
    assert!(
        out.status.success(),
        "--diagnose 在模型缺失时也要跑完并 exit 0: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ASR 依赖"),
        "报告里要有 ASR 依赖这一段：{stdout}"
    );
    assert!(
        !base.join("called").exists(),
        "diagnose 不该调用引擎二进制去探测模型"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn transcribe_fails_nonzero_without_calling_the_engine() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-transcribe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();

    // 合成一个极简 wav 头，内容是否合法无关紧要——preflight 应该在
    // 调用引擎二进制之前就因为模型缺失而失败。
    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();

    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "模型缺失时 --transcribe 必须非零退出"
    );
    assert!(!base.join("called").exists(), "sentinel 必须是 0 次调用");

    let _ = fs::remove_dir_all(&base);
}

// ======================================================================
// R1：独立评审 EAR-PR1-REVIEW.md 的三点 REQUEST_CHANGES 的 RED→GREEN 复现。
// ======================================================================

/// 和 `src/builtin_models.rs` 里 `SENSEVOICE`/`VAD`/`PairRecord` 的生产常量
/// 保持一致——这是独立进程测试，没有 lib target 可以 `use agentear::...`，
/// 只能照着源码常量抄一份。改生产常量时这两组数字要跟着改。
const SENSEVOICE_BYTES: u64 = 254208320;
const SENSEVOICE_SHA256: &str = "4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5";
const VAD_BYTES: u64 = 1720512;
const VAD_SHA256: &str = "1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479";
const THAI_BYTES: u64 = 574041195;
const THAI_SHA256: &str = "5bfc04f1931a1bb9af9f6c7942b4a63b8d8f956377fe6b9827c6b286420a9c6d";

fn sha256_hex16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())[..16].to_string()
}

fn write_config(data: &Path, asr_lang: &str) {
    fs::write(
        data.join("config.json"),
        format!(
            r#"{{"asr_backend":"builtin","asr_lang":"{asr_lang}","correct_terms":false,"talk_mode":"input_method"}}"#
        ),
    )
    .unwrap();
}

/// 造一个 `size` 字节的稀疏文件（只占用文件系统的洞，不真写那么多字节），
/// 同真实 254 MB/574 MB 模型文件在磁盘上的占用方式一致。
fn sparse_file(path: &Path, size: u64) {
    let f = fs::File::create(path).unwrap();
    f.set_len(size).unwrap();
}

/// P1 RED→GREEN case 1（review 的 `tiny_self_declared_record`）：10/8 字节的
/// 假模型 + 记录自称这两个字节数、**没有 sha256 字段**（修复前的记录schema）。
/// 修复前：反序列化成功、体积对得上记录自己写的数，被判 Ready，sentinel 调用 1 次。
/// 修复后：`PairRecord` 多了必填的 sha256 字段，这条记录反序列化直接失败 → None。
#[test]
fn transcribe_rejects_tiny_self_declared_record_with_no_sha_fields() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-tiny-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    let models = data.join("models");
    fs::create_dir_all(&models).unwrap();
    write_config(&data, "auto");

    fs::write(models.join("sensevoice-small-q8.gguf"), b"tiny-model").unwrap(); // 10 字节
    fs::write(models.join("fsmn-vad.gguf"), b"tiny-vad").unwrap(); // 8 字节
    let bin_fp = sha256_hex16(&fs::read(vendor.join("bin/llama-funasr-sensevoice")).unwrap());
    fs::write(
        models.join("sensevoice-pair.installed"),
        format!(r#"{{"sensevoice_bytes":10,"vad_bytes":8,"bin_fingerprint":"{bin_fp}"}}"#),
    )
    .unwrap();

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "10/8 字节的假模型配一份没有 sha256 字段的记录必须被拒绝，不能 Ready：{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!base.join("called").exists(), "sentinel 必须是 0 次调用");

    let _ = fs::remove_dir_all(&base);
}

/// P1 RED→GREEN case 2（review 的 `current_size_wrong_sha_record`）：
/// 两个文件都是**当前 spec 的真实体积**（稀疏文件），记录里的体积也对得上，
/// 但 sha256 字段跟当前 `ModelSpec` 不一致——同体积的 pin 升级，
/// 光比字节数看不出来，必须靠 sha256 挡住。
#[test]
fn transcribe_rejects_current_size_record_with_wrong_sha() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-wrongsha-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    let models = data.join("models");
    fs::create_dir_all(&models).unwrap();
    write_config(&data, "auto");

    sparse_file(&models.join("sensevoice-small-q8.gguf"), SENSEVOICE_BYTES);
    sparse_file(&models.join("fsmn-vad.gguf"), VAD_BYTES);
    let bin_fp = sha256_hex16(&fs::read(vendor.join("bin/llama-funasr-sensevoice")).unwrap());
    fs::write(
        models.join("sensevoice-pair.installed"),
        format!(
            r#"{{"sensevoice_bytes":{SENSEVOICE_BYTES},"vad_bytes":{VAD_BYTES},"sensevoice_sha256":"{}","vad_sha256":"{}","bin_fingerprint":"{bin_fp}"}}"#,
            "0".repeat(64),
            "f".repeat(64)
        ),
    )
    .unwrap();

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "体积对得上但 sha256 跟当前 spec 不一致时必须拒绝：{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!base.join("called").exists(), "sentinel 必须是 0 次调用");

    let _ = fs::remove_dir_all(&base);
}

/// P1 RED→GREEN case 3（review 的 `current_size_no_sha_record`）：
/// 当前 spec 的真实体积，但记录里**压根没有 sha256 字段**（反序列化失败）。
#[test]
fn transcribe_rejects_current_size_record_with_no_sha_fields() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-nosha-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    let models = data.join("models");
    fs::create_dir_all(&models).unwrap();
    write_config(&data, "auto");

    sparse_file(&models.join("sensevoice-small-q8.gguf"), SENSEVOICE_BYTES);
    sparse_file(&models.join("fsmn-vad.gguf"), VAD_BYTES);
    let bin_fp = sha256_hex16(&fs::read(vendor.join("bin/llama-funasr-sensevoice")).unwrap());
    fs::write(
        models.join("sensevoice-pair.installed"),
        format!(r#"{{"sensevoice_bytes":{SENSEVOICE_BYTES},"vad_bytes":{VAD_BYTES},"bin_fingerprint":"{bin_fp}"}}"#),
    )
    .unwrap();

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "没有 sha256 字段的记录必须被拒绝：{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!base.join("called").exists(), "sentinel 必须是 0 次调用");

    let _ = fs::remove_dir_all(&base);
}

/// P1 正控：正确 sha256 + 正确体积的记录仍然能被判 Ready（防止矫枉过正）。
#[test]
fn transcribe_accepts_current_size_record_with_matching_sha() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-goodsha-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor(&base);
    let data = base.join("data");
    let models = data.join("models");
    fs::create_dir_all(&models).unwrap();
    write_config(&data, "auto");

    sparse_file(&models.join("sensevoice-small-q8.gguf"), SENSEVOICE_BYTES);
    sparse_file(&models.join("fsmn-vad.gguf"), VAD_BYTES);
    let bin_fp = sha256_hex16(&fs::read(vendor.join("bin/llama-funasr-sensevoice")).unwrap());
    fs::write(
        models.join("sensevoice-pair.installed"),
        format!(
            r#"{{"sensevoice_bytes":{SENSEVOICE_BYTES},"vad_bytes":{VAD_BYTES},"sensevoice_sha256":"{SENSEVOICE_SHA256}","vad_sha256":"{VAD_SHA256}","bin_fingerprint":"{bin_fp}"}}"#
        ),
    )
    .unwrap();

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "体积+sha256 都对得上当前 spec 时应该判 Ready：{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(base.join("called").exists(), "sentinel 应该被调用一次");

    let _ = fs::remove_dir_all(&base);
}

/// 同时放 SenseVoice 与 whisper 两个 sentinel 引擎的 vendor，分别用
/// `sv-called`/`whisper-called` 两个标记文件区分谁被调用过。
fn fixture_vendor_with_whisper(base: &Path) -> PathBuf {
    let vendor = base.join("vendor");
    let sv = vendor.join("bin/llama-funasr-sensevoice");
    fs::create_dir_all(sv.parent().unwrap()).unwrap();
    fs::write(
        &sv,
        format!(
            "#!/bin/sh\necho called >> \"{}\"\nprintf '<|en|><|NEUTRAL|><|Speech|><|withitn|>sv fixture\\n'\n",
            base.join("sv-called").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&sv, fs::Permissions::from_mode(0o755)).unwrap();

    let whisper = vendor.join("bin/whisper-cli");
    fs::write(
        &whisper,
        format!(
            "#!/bin/sh\necho called >> \"{}\"\nprintf 'สวัสดี\\n'\n",
            base.join("whisper-called").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&whisper, fs::Permissions::from_mode(0o755)).unwrap();
    vendor
}

/// 装好一份有效的泰语模型安装记录：真实体积的稀疏文件 + sha256/engine_id
/// 都对得上的 `.installed` manifest（算法同 `src/download.rs::is_installed`
/// 与 `src/asr.rs::engine_fingerprint`）。
fn install_thai_model(data: &Path, whisper_bin: &Path) {
    let models = data.join("models");
    fs::create_dir_all(&models).unwrap();
    let model = models.join("ggml-distill-whisper-th-large-v3-q5_0.bin");
    sparse_file(&model, THAI_BYTES);
    let engine_id = sha256_hex16(&fs::read(whisper_bin).unwrap());
    fs::write(
        models.join("ggml-distill-whisper-th-large-v3-q5_0.bin.installed"),
        format!(r#"{{"model_sha256":"{THAI_SHA256}","engine_id":"{engine_id}"}}"#),
    )
    .unwrap();
}

/// P2/P3 RED→GREEN（review finding 3，review 的 `cli_thai_without_sensevoice`）：
/// 存储配置是 Auto、没有 SenseVoice 模型、Thai 模型装好且记录有效，
/// 显式 `--transcribe ... --lang th` 之前：preflight 用的是 `cfg.asr_lang`
/// （Auto），Auto 需要 SenseVoice，于是在真正看到 `--lang th` 之前就报错退出。
/// 修复后：`--transcribe` 的显式 `--lang` 要在 preflight 之前就生效。
#[test]
fn explicit_cli_thai_lang_bypasses_auto_preflight_without_sensevoice() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-thaicli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor_with_whisper(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();
    write_config(&data, "auto");
    install_thai_model(&data, &vendor.join("bin/whisper-cli"));

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap(), "--lang", "th"]);
    assert!(
        out.status.success(),
        "配置 Auto + 显式 --lang th + Thai 模型就绪 + 没装 SenseVoice 时应该成功：{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(base.join("whisper-called").exists(), "whisper 必须被调用一次");
    assert!(!base.join("sv-called").exists(), "SenseVoice 不该被调用");

    let stored = fs::read_to_string(data.join("config.json")).unwrap();
    assert!(
        stored.contains("\"asr_lang\":\"auto\""),
        "显式 --lang 只影响这一次调用，不能写回存储配置：{stored}"
    );

    let _ = fs::remove_dir_all(&base);
}

/// 回归护栏：没有显式 `--lang`、存储配置仍是 Auto、且两个引擎的模型都没装时，
/// 必须照旧非零退出且两个 sentinel 都是 0 次——证明上面的修复
/// 没有把“Auto 该报错的场景”也放行。
#[test]
fn auto_without_explicit_lang_still_fails_when_no_models_installed() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-autoneg-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor_with_whisper(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();
    write_config(&data, "auto");

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "Auto 且两边模型都没装时必须非零退出：{:?}",
        out.status
    );
    assert!(!base.join("sv-called").exists());
    assert!(!base.join("whisper-called").exists());

    let _ = fs::remove_dir_all(&base);
}

/// `--lang` 给了非法值时要维持原有的那条报错，不受本次「提前解析」改动影响。
#[test]
fn transcribe_lang_invalid_value_keeps_original_error() {
    let base = std::env::temp_dir().join(format!("agentear-pr1-r1-langinvalid-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let vendor = fixture_vendor_with_whisper(&base);
    let data = base.join("data");
    fs::create_dir_all(&data).unwrap();
    write_config(&data, "auto");

    let wav = base.join("synthetic.wav");
    fs::write(&wav, b"RIFF....WAVEfmt ").unwrap();
    let out = run(&vendor, &data, &["--transcribe", wav.to_str().unwrap(), "--lang", "zz"]);
    assert!(!out.status.success(), "非法 --lang 必须非零退出");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--lang 只认 th / auto"),
        "必须保留原有报错文案：{stderr}"
    );
    assert!(!base.join("sv-called").exists());
    assert!(!base.join("whisper-called").exists());

    let _ = fs::remove_dir_all(&base);
}
