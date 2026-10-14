//! 内置 SenseVoice+VAD 模型的按需定位（AgentEar Lite PR1）。
//!
//! 只做"定位"：两个真实 ModelSpec（pins 见 `AGENTEAR-LITE-CONTRACT.md`）+
//! 一份数据目录安装记录的读法。下载 job 留给 PR2，这里先把 `preflight`/
//! `transcribe` 共用的 resolver 建好——每次现查，不缓存，所以下载完成后
//! 已有 `Asr` 实例下一轮转写即可用，不需要重建。

use crate::download::ModelSpec;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 识别模型权重。HF pinned revision，apache-2.0，公开非 gated。
pub const SENSEVOICE: ModelSpec = ModelSpec {
    file: "sensevoice-small-q8.gguf",
    url: "https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/90c1c61912018b70ada0fcc024ea24aca62f2e63/sensevoice-small-q8.gguf",
    sha256: "4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5",
    bytes: 254208320,
};

/// VAD。同一来源文档、同一 pin 日期。
pub const VAD: ModelSpec = ModelSpec {
    file: "fsmn-vad.gguf",
    url: "https://huggingface.co/FunAudioLLM/fsmn-vad-GGUF/resolve/6840bae4c5c92ee8c04faaf4db23dd0105098d7f/fsmn-vad.gguf",
    sha256: "1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479",
    bytes: 1720512,
};

/// 数据目录位置的安装记录。PR2 的下载 job 在两个文件全量 sha256 对上 pin、
/// 且跑完加载冒烟之后才写它；这里只读，不重新验证——同 `download::is_installed`
/// 的取舍（它自己文档已写明："是优化，不是完整性边界"）。
#[derive(Serialize, Deserialize)]
pub(crate) struct PairRecord {
    pub sensevoice_bytes: u64,
    pub vad_bytes: u64,
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

/// 数据目录里这一对文件是否就绪：两个文件都是普通文件、体积和记录里的
/// 声明对得上、记录里的 bin 指纹和当前 bin 对得上。
fn data_pair(data_root: &Path, bin: &Path) -> Option<(PathBuf, PathBuf)> {
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
    if fs::symlink_metadata(&model).ok()?.len() != rec.sensevoice_bytes
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
            bin_fingerprint: bin_fingerprint(&bin).unwrap(),
        };
        fs::write(record_path(&data), serde_json::to_string(&rec).unwrap()).unwrap();

        assert_eq!(
            resolve(&vendor, Some(&data), &bin),
            Some((model, vad)),
            "装完小文件+合法记录后应能解析出新路径"
        );
    }
}
