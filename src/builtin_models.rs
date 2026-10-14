//! 内置 SenseVoice+VAD 模型的按需定位（AgentEar Lite PR1）。
//!
//! 只做"定位"：两个真实 ModelSpec（pins 见 `AGENTEAR-LITE-CONTRACT.md`）+
//! 一份数据目录安装记录的读法。下载 job 留给 PR2，这里先把 `preflight`/
//! `transcribe` 共用的 resolver 建好——每次现查，不缓存，所以下载完成后
//! 已有 `Asr` 实例下一轮转写即可用，不需要重建。

use crate::download::{Fail, ModelSpec};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 识别模型权重。HF pinned revision，apache-2.0，公开非 gated。
///
/// ⚠️ **`bytes`/`sha256` 在 `cfg(test)` 下换成极小的测试值**：
/// `data_pair` 要拿当前 spec 校验安装记录（见下），而单测/集成测试里的
/// 正控 fixture 只造几字节的假文件——真实 254 MB 不可能进测试 fixture。
/// 只有 `file`/`url` 两个字段在测试下也保持不变，`cfg(test)` 只整体编译
/// 进单测二进制（`cargo test`），**不影响实际发布的二进制**：process 级
/// 集成测试（`tests/builtin_model_locator.rs`）跑的是 release/debug 正式
/// 二进制，链接的是下面 `#[cfg(not(test))]` 这一份真实常量。
#[cfg(not(test))]
pub const SENSEVOICE: ModelSpec = ModelSpec {
    file: "sensevoice-small-q8.gguf",
    url: "https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/90c1c61912018b70ada0fcc024ea24aca62f2e63/sensevoice-small-q8.gguf",
    sha256: "4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5",
    bytes: 254208320,
};
#[cfg(test)]
pub const SENSEVOICE: ModelSpec = ModelSpec {
    file: "sensevoice-small-q8.gguf",
    url: "https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/90c1c61912018b70ada0fcc024ea24aca62f2e63/sensevoice-small-q8.gguf",
    sha256: "test-fixture-sensevoice-sha256-not-real",
    bytes: 10,
};

/// VAD。同一来源文档、同一 pin 日期。同上，`cfg(test)` 下换成极小测试值。
#[cfg(not(test))]
pub const VAD: ModelSpec = ModelSpec {
    file: "fsmn-vad.gguf",
    url: "https://huggingface.co/FunAudioLLM/fsmn-vad-GGUF/resolve/6840bae4c5c92ee8c04faaf4db23dd0105098d7f/fsmn-vad.gguf",
    sha256: "1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479",
    bytes: 1720512,
};
#[cfg(test)]
pub const VAD: ModelSpec = ModelSpec {
    file: "fsmn-vad.gguf",
    url: "https://huggingface.co/FunAudioLLM/fsmn-vad-GGUF/resolve/6840bae4c5c92ee8c04faaf4db23dd0105098d7f/fsmn-vad.gguf",
    sha256: "test-fixture-vad-sha256-not-real",
    bytes: 8,
};

/// 数据目录位置的安装记录。PR2 的下载 job 在两个文件全量 sha256 对上 pin、
/// 且跑完加载冒烟之后才写它；这里只读，不重新验证——同 `download::is_installed`
/// 的取舍（它自己文档已写明："是优化，不是完整性边界"）。
///
/// ⚠️ **两个 sha256 字段校验的是「记录自称的 pin」对不对得上当前
/// `ModelSpec`，不是重新对实际文件内容取哈希**（254 MB 每次 `resolve`
/// 都算一遍代价太大，且这正是 PR2 安装时已经做过一次全量校验的事）。
/// 挡的是：①同体积但不同内容的 pin 升级（光比字节数看不出来）、
/// ②压根没有这两个字段的旧/伪造记录（`serde_json` 反序列化直接失败）。
#[derive(Serialize, Deserialize)]
pub(crate) struct PairRecord {
    pub sensevoice_bytes: u64,
    pub vad_bytes: u64,
    pub sensevoice_sha256: String,
    pub vad_sha256: String,
    /// 装的时候跑冒烟那个 SenseVoice bin 的指纹；app 升级换了 bin 就对不上。
    pub bin_fingerprint: String,
}

fn record_path(data_root: &Path) -> PathBuf {
    data_root.join("models/sensevoice-pair.installed")
}

/// SenseVoice bin 的身份指纹：sha256 前 16 位（同 `asr::engine_fingerprint`
/// 的算法，这里直接吃调用方给的路径，不另开一个 static）。
pub(crate) fn bin_fingerprint(bin: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(bin).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(format!("{:x}", h.finalize())[..16].to_string())
}

/// 原子写安装记录，调用方保证两份文件 SHA 全过 + 加载冒烟通过之后才调用。
pub(crate) fn write_pair_record(data_root: &Path, rec: &PairRecord) -> Result<()> {
    let path = record_path(data_root);
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("installed"),
        std::process::id()
    ));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_file(&tmp).ok();
    }
    let json = serde_json::to_string(rec)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("序列化安装记录失败: {e}")))?;
    fs::write(&tmp, json)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("写安装记录失败: {e}")))?;
    fs::rename(&tmp, &path)
        .map_err(|e| anyhow::Error::new(Fail::Io).context(format!("落地安装记录失败: {e}")))?;
    Ok(())
}

/// 数据目录里这一对文件是否就绪：两个文件都是普通文件、体积和记录里的
/// 声明对得上、记录里的 bin 指纹和当前 bin 对得上。
pub(crate) fn data_pair(data_root: &Path, bin: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = data_root.join("models");
    let (model, vad) = (dir.join(SENSEVOICE.file), dir.join(VAD.file));
    if !crate::download::is_present(&model) || !crate::download::is_present(&vad) {
        return None;
    }
    let rec_path = record_path(data_root);
    if !fs::symlink_metadata(&rec_path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let rec: PairRecord = serde_json::from_str(&fs::read_to_string(&rec_path).ok()?).ok()?;
    // 先核对记录自称的 pin 是不是当前这一套 `ModelSpec`——同体积但换了内容
    // 的 pin 升级，光比字节数看不出来，必须连 sha256 一起比。
    if rec.sensevoice_bytes != SENSEVOICE.bytes
        || rec.vad_bytes != VAD.bytes
        || rec.sensevoice_sha256 != SENSEVOICE.sha256
        || rec.vad_sha256 != VAD.sha256
        || fs::symlink_metadata(&model).ok()?.len() != rec.sensevoice_bytes
        || fs::symlink_metadata(&vad).ok()?.len() != rec.vad_bytes
        || Some(rec.bin_fingerprint) != bin_fingerprint(bin)
    {
        return None;
    }
    Some((model, vad))
}

/// 旧完整 bundle：vendor 里随包分发的那一对，没有安装记录也信——这是
/// `Asr::new` 原来就有的行为，这里只是纳入同一个 resolver。
fn vendor_pair(vendor: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = vendor.join("models");
    let (model, vad) = (dir.join(SENSEVOICE.file), dir.join(VAD.file));
    (crate::download::is_present(&model) && crate::download::is_present(&vad))
        .then_some((model, vad))
}

/// `preflight`(Auto) 与 `transcribe_sensevoice` 共用的解析器：每次现查，
/// 不缓存。先信旧 bundle（已发布用户升级上来一个字节不变），查不到再看
/// 数据目录的安装记录。
pub(crate) fn resolve(
    vendor: &Path,
    data_root: Option<&Path>,
    bin: &Path,
) -> Option<(PathBuf, PathBuf)> {
    vendor_pair(vendor).or_else(|| data_pair(data_root?, bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agentear-bm-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolves_vendor_bundle_without_any_record() {
        let vendor = tmp("vendor-bundle");
        let models = vendor.join("models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join(SENSEVOICE.file), b"fake-model").unwrap();
        fs::write(models.join(VAD.file), b"fake-vad").unwrap();
        let bin = vendor.join("bin-unused-for-vendor-path");
        assert!(
            resolve(&vendor, None, &bin).is_some(),
            "旧 bundle 全在时不需要安装记录"
        );
    }

    #[test]
    fn data_pair_needs_matching_record_and_fingerprint() {
        let vendor = tmp("vendor-empty");
        let data = tmp("data");
        let models = data.join("models");
        fs::create_dir_all(&models).unwrap();
        let bin = vendor.join("bin/llama-funasr-sensevoice");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"fixture-bin-bytes").unwrap();

        assert!(
            resolve(&vendor, Some(&data), &bin).is_none(),
            "没装文件/记录时查不到"
        );

        let model = models.join(SENSEVOICE.file);
        let vad = models.join(VAD.file);
        fs::write(&model, b"tiny-model").unwrap();
        fs::write(&vad, b"tiny-vad").unwrap();
        let rec = PairRecord {
            sensevoice_bytes: fs::metadata(&model).unwrap().len(),
            vad_bytes: fs::metadata(&vad).unwrap().len(),
            sensevoice_sha256: SENSEVOICE.sha256.to_string(),
            vad_sha256: VAD.sha256.to_string(),
            bin_fingerprint: bin_fingerprint(&bin).unwrap(),
        };
        fs::write(record_path(&data), serde_json::to_string(&rec).unwrap()).unwrap();

        assert_eq!(
            resolve(&vendor, Some(&data), &bin),
            Some((model, vad)),
            "装完小文件+合法记录+当前 spec sha256 后应能解析出新路径"
        );
    }

    /// P1：记录自称的体积/sha256 跟当前 `ModelSpec` 不一致——无论是尺寸
    /// 对不上、还是「同尺寸但 sha256 对不上」（同体积的 pin 升级，光比
    /// 字节数看不出来）——都要判未就绪，不能只靠「文件体积等于记录里
    /// 自己写的那个数」这种自证式检查。
    #[test]
    fn data_pair_rejects_record_whose_pin_does_not_match_current_spec() {
        let vendor = tmp("vendor-empty-pin-mismatch");
        let data = tmp("data-pin-mismatch");
        let models = data.join("models");
        fs::create_dir_all(&models).unwrap();
        let bin = vendor.join("bin/llama-funasr-sensevoice");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"fixture-bin-bytes").unwrap();

        let model = models.join(SENSEVOICE.file);
        let vad = models.join(VAD.file);
        fs::write(&model, b"tiny-model").unwrap();
        fs::write(&vad, b"tiny-vad").unwrap();

        // 体积对不上当前 spec（记录自称的体积与实际文件体积一致，
        // 但都不是当前 SENSEVOICE.bytes/VAD.bytes）。
        let wrong_size_rec = PairRecord {
            sensevoice_bytes: fs::metadata(&model).unwrap().len() + 1,
            vad_bytes: fs::metadata(&vad).unwrap().len(),
            sensevoice_sha256: SENSEVOICE.sha256.to_string(),
            vad_sha256: VAD.sha256.to_string(),
            bin_fingerprint: bin_fingerprint(&bin).unwrap(),
        };
        fs::write(
            record_path(&data),
            serde_json::to_string(&wrong_size_rec).unwrap(),
        )
        .unwrap();
        assert!(
            resolve(&vendor, Some(&data), &bin).is_none(),
            "记录自称的体积跟当前 spec 不一致时必须判未就绪"
        );

        // 同体积，但 sha256 跟当前 spec 不一致——伪造同尺寸文件 + 乱写哈希。
        let wrong_sha_rec = PairRecord {
            sensevoice_bytes: fs::metadata(&model).unwrap().len(),
            vad_bytes: fs::metadata(&vad).unwrap().len(),
            sensevoice_sha256: "0".repeat(64),
            vad_sha256: "f".repeat(64),
            bin_fingerprint: bin_fingerprint(&bin).unwrap(),
        };
        fs::write(
            record_path(&data),
            serde_json::to_string(&wrong_sha_rec).unwrap(),
        )
        .unwrap();
        assert!(
            resolve(&vendor, Some(&data), &bin).is_none(),
            "同尺寸但 sha256 对不上当前 spec 时必须判未就绪"
        );
    }

    /// P1：没有 sha256 字段的旧/伪造记录——反序列化就该直接失败，
    /// 不能退回「忽略缺字段、只比体积」。
    #[test]
    fn data_pair_rejects_record_missing_sha_fields() {
        let vendor = tmp("vendor-empty-no-sha");
        let data = tmp("data-no-sha");
        let models = data.join("models");
        fs::create_dir_all(&models).unwrap();
        let bin = vendor.join("bin/llama-funasr-sensevoice");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"fixture-bin-bytes").unwrap();

        let model = models.join(SENSEVOICE.file);
        let vad = models.join(VAD.file);
        fs::write(&model, b"tiny-model").unwrap();
        fs::write(&vad, b"tiny-vad").unwrap();

        let legacy_json = serde_json::json!({
            "sensevoice_bytes": fs::metadata(&model).unwrap().len(),
            "vad_bytes": fs::metadata(&vad).unwrap().len(),
            "bin_fingerprint": bin_fingerprint(&bin).unwrap(),
        });
        fs::write(record_path(&data), legacy_json.to_string()).unwrap();

        assert!(
            resolve(&vendor, Some(&data), &bin).is_none(),
            "没有 sha256 字段的记录反序列化就该失败，必须判未就绪"
        );
    }
}
