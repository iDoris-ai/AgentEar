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

/// 列出可用的输入设备名，供菜单栏选择。
///
/// 每次调用都重新枚举，不缓存——耳机插拔后列表要立刻是对的。
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(it) => it.filter_map(|d| d.name().ok()).collect(),
        Err(e) => {
            log::error!("枚举输入设备失败: {e}");
            Vec::new()
        }
    }
}

/// 当前系统默认输入设备名，用于在菜单里标注「系统默认（XXX）」。
pub fn default_input_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()
        .and_then(|d| d.name().ok())
}

impl Recorder {
    /// 打开输入设备并开始采集。返回后即有数据流入。
    ///
    /// `want` 为 `None` 时跟随系统默认。**指定的设备找不到时退回默认而不是
    /// 报错**——设备是可拔的，为了一个拔掉的耳机让录音直接失败不划算，
    /// 但要把降级这件事明确写进日志。
    pub fn start(want: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = match want {
            Some(name) => match host
                .input_devices()
                .ok()
                .and_then(|mut it| it.find(|d| d.name().map(|n| n == name).unwrap_or(false)))
            {
                Some(d) => d,
                None => {
                    log::warn!("配置的输入设备「{name}」不在线，退回系统默认");
                    host.default_input_device()
                        .ok_or_else(|| anyhow!("找不到默认输入设备（麦克风权限被拒？）"))?
                }
            },
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("找不到默认输入设备（麦克风权限被拒？）"))?,
        };
        let config = device
            .default_input_config()
            .context("读取输入设备默认配置失败")?;

        let src_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let name = device.name().unwrap_or_else(|_| "?".into());
        log::info!(
            "输入设备: {} | {} Hz, {} ch, {:?}",
            name,
            src_rate,
            channels,
            config.sample_format()
        );
        // 蓝牙耳机当输入时走 HFP，采样率被压到 16 kHz 且带宽很窄，
        // 明显差于内置麦克风（48 kHz）。长期使用会拉低 CER，值得提醒一次。
        if src_rate <= 16_000 && channels == 1 {
            log::warn!(
                "「{name}」以 {src_rate} Hz 采集,像是蓝牙耳机的 HFP 模式,识别质量会下降。\
                 可在菜单栏「输入设备」里改选内置麦克风"
            );
        }

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
