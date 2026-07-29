//! 麦克风采集。目标格式：16 kHz / 单声道 / i16，与 SenseVoice 的输入一致。
//!
//! macOS 注意：从终端直接跑的二进制会继承终端的麦克风权限（TCC），
//! 容易产生「我这儿能跑」的假象。正式分发必须打成 .app bundle 并在
//! Info.plist 里声明 NSMicrophoneUsageDescription。见 `docs/milestones.md`。

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::sync::mpsc::{channel, Receiver, Sender};

pub const TARGET_RATE: u32 = 16_000;

pub struct Recorder {
    _stream: Stream,
    rx: Receiver<Vec<i16>>,
}

impl Recorder {
    /// 打开默认输入设备并开始采集。返回后即有数据流入。
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("找不到默认输入设备（麦克风权限被拒？）"))?;
        let config = device
            .default_input_config()
            .context("读取输入设备默认配置失败")?;

        let src_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        log::info!(
            "输入设备: {} | {} Hz, {} ch, {:?}",
            device.name().unwrap_or_else(|_| "?".into()),
            src_rate,
            channels,
            config.sample_format()
        );

        let (tx, rx) = channel::<Vec<i16>>();
        let err_fn = |e| log::error!("音频流错误: {e}");

        let stream = match config.sample_format() {
            SampleFormat::F32 => device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    let _ = tx.send(convert(data, channels, src_rate));
                },
                err_fn,
                None,
            )?,
            SampleFormat::I16 => {
                let tx2: Sender<Vec<i16>> = tx;
                device.build_input_stream(
                    &config.into(),
                    move |data: &[i16], _| {
                        let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                        let _ = tx2.send(convert(&f, channels, src_rate));
                    },
                    err_fn,
                    None,
                )?
            }
            f => return Err(anyhow!("不支持的采样格式: {f:?}")),
        };

        stream.play().context("启动音频流失败")?;
        Ok(Self {
            _stream: stream,
            rx,
        })
    }

    /// 取走当前已缓冲的采样。非阻塞。
    pub fn drain(&self) -> Vec<i16> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
    }
}

/// 下混到单声道并重采样到 16 kHz。
///
/// 用的是最近邻抽取——对语音识别的前处理足够，且没有引入 FFT 依赖。
/// 若日后发现高频混叠影响 CER，再换带抗混叠滤波的重采样器。
fn convert(input: &[f32], channels: usize, src_rate: u32) -> Vec<i16> {
    let mono: Vec<f32> = if channels <= 1 {
        input.to_vec()
    } else {
        input
            .chunks(channels)
            .map(|f| f.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    if src_rate == TARGET_RATE {
        return mono.iter().map(to_i16).collect();
    }

    let ratio = src_rate as f64 / TARGET_RATE as f64;
    let n = (mono.len() as f64 / ratio).floor() as usize;
    (0..n)
        .map(|i| {
            let idx = (i as f64 * ratio) as usize;
            to_i16(&mono[idx.min(mono.len() - 1)])
        })
        .collect()
}

fn to_i16(s: &f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}
