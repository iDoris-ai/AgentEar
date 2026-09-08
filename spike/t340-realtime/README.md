# T3.4.0 实时链路可行性 spike

丢弃式 spike，**不进主工程**。结论见 [`docs/benchmarks-m3.md`](../../docs/benchmarks-m3.md) §7。

`vpio_test.swift` —— 测 macOS Voice Processing I/O 的回声消除。

```bash
swift vpio_test.swift <要播放的wav> <录音输出wav>            # VPIO 开
swift vpio_test.swift <要播放的wav> <录音输出wav> --no-vpio  # 对照组
```

它会**从扬声器真实外放**并同时录音，用来测「系统会不会听见自己」。
VPIO 开启时输入是多声道（含参考信号），取第 0 路即麦克风主通道：

```bash
ffmpeg -i out.wav -filter_complex "pan=mono|c0=c0" -ar 16000 out16.wav
speech transcribe --engine qwen3 -m 1.7B out16.wav   # 能转出播放内容 = 自触发
```

⚠️ **至少跑 3 轮**：VPIO 是自适应滤波，单次结果会明显偏乐观。
