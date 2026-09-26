# Qwen3-ASR 两档 × 两形态的时延与内存（T3.5.6，2026-09-26）

`docs/benchmarks-asr-zh-en.md` §9 的原始数据。

## 环境

- MacBook M1 Max / 64 GB，macOS 26.4.1。
- **没有隔离**：测的时候 jason 自己的 AgentEar 守护进程和 TTS 边车都开着
  （他在用），所以数字里含正常的后台负载。
- 运行时：speech-swift **v0.0.28** 预编译包（`qwen3.rs` 钉死的那一份），
  权重来自 `aufklarer/Qwen3-ASR-0.6B-MLX-4bit@bc441bd1` 与
  `aufklarer/Qwen3-ASR-1.7B-MLX-8bit@e5450a26`，由 AgentEar 自己下载、校验，
  speech 跑在断网沙箱里。

## 音频

**合成语音，不是真人录音**——只用来量时延和内存，**不用来比准确率**
（准确率以 `benchmarks-asr-zh-en.md` §2 的 n=60×3 组为准）：

```bash
say -v Tingting -o zh.aiff "今天清迈天气还不错，最高三十二度。"
afconvert -f WAVE -d LEI16@16000 -c 1 zh.aiff zh16k.wav          # 3.9 s
say -v Tingting -o long.aiff "今天我们讨论一下语音识别的方案。默认还是用随包的 SenseVoice，Qwen3 做成可以在设置窗口里下载的可选项，零点六B给内存小的机器，一点七B给想要更准的人。"
afconvert -f WAVE -d LEI16@16000 -c 1 long.aiff zh16k-long.wav   # 16.9 s
```

16 kHz 单声道 Int16，与 AgentEar 自己录的 raw 同格式。

## 命令

```bash
AGENTEAR_DATA=<临时目录> agentear --fetch-qwen3 0.6b
AGENTEAR_DATA=<临时目录> agentear --fetch-qwen3 1.7b
AGENTEAR_DATA=<临时目录> agentear --asr-bench zh16k.wav --runs 5      > bench-3.9s.tsv
AGENTEAR_DATA=<临时目录> agentear --asr-bench zh16k-long.wav --runs 5 > bench-16.9s.tsv
```

`--asr-bench` 在**同一个进程**里用 `config::update` 依次切到五种组合，
每种跑 N 轮，走的是守护进程同一个分派引擎（`engine::Dispatch`）——
所以它同时也是「切换即时生效」的实测证据。

## 列的含义

| 列 | 含义 |
|---|---|
| `secs` | 一次 `transcribe()` 的墙钟（逐次调用 = 起进程 + 加载模型 + 转写；常驻 = 一次 HTTP 请求） |
| `child_maxrss_mb` | `getrusage(RUSAGE_CHILDREN).ru_maxrss`：**已回收子进程里最大的那个**的峰值 RSS。组合按内存从小到大排，所以每一行读到的就是当前组合的峰值 |
| `server_rss_mb` | 常驻时 `ps -o rss=` 读 speech-server 的 RSS（**热态快照，不是峰值**） |
| `text` | 转写结果 |

⚠️ 第 1 轮与后面各轮要分开看：常驻组合的第 1 轮含起服务 + 加载模型（约 2 s），
逐次组合的第 1 轮含 Metal 着色器首编。
