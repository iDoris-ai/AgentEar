# T3.5.6 调研原始记录（2026-09-26，MacBook M1 Max / 64 GB）

ADR-0010 里每个数字都出自这份记录。命令就是当时跑的命令，输出是原样摘录（只删了无关行）。
测试音频：`vendor/models/talk/out/zh_smoke_000.wav`（48 kHz / 1 ch / Int16 / 3.52 s，
内容「今天清迈天气还不错，最高三十二度。」），以及它用
`afconvert -f WAVE -d LEI16@16000 -c 1` 转出来的 16 kHz 版 `zh16k.wav`。
AgentEar 自己录的 raw 是 16 kHz（`afinfo ~/.agentear/raw/audio/<最新>.wav` →
`1 ch, 16000 Hz, Int16`），所以 16 kHz 这一组才对应真实链路。

⚠️ **全部是单条样本、每格 3–5 次**。能说明「时延是哪个量级」「能不能跑通」，
**说明不了准确率**——准确率的横比仍以 `docs/benchmarks-asr-zh-en.md`（n=60×3 组）为准，
且那份只测了 speech CLI 这一条运行时。

## 1. speech-swift 的分发形态

```
$ brew info speech
==> speech: 0.0.26 → stable 0.0.27 (bottled), HEAD
https://soniqo.audio
License: Apache-2.0
From: https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/s/speech.rb

$ cat /opt/homebrew/Cellar/speech/0.0.26/bin/speech
#!/bin/bash
exec "/opt/homebrew/Cellar/speech/0.0.26/libexec/speech" "$@"

# formula：swift build + build_mlx_metallib.sh；libexec 装 speech / speech-server /
# mlx.metallib / *.bundle
$ du -sh /opt/homebrew/Cellar/speech/0.0.26/libexec
241M

$ gh release list -R soniqo/speech-swift -L 5
v0.0.28  Latest  2026-09-24
v0.0.27          2026-09-02
v0.0.26          2026-08-17
v0.0.25          2026-08-16
v0.0.24          2026-08-16
$ gh release view -R soniqo/speech-swift --json assets     # v0.0.28
speech-macos-arm64.tar.gz 99089736
$ shasum -a 256 speech-macos-arm64.tar.gz
cc144cac7985884f026a76281fdb504ce6e0fe2ad11a9b0a7901cf8b617b930a
$ tar tzf …  → speech, speech-server, audio, audio-server, mlx.metallib, 5 个 *.bundle
$ du -sh <解包目录>
364M
$ gh api repos/soniqo/speech-swift --jq .license.spdx_id
Apache-2.0
```

签名与 Gatekeeper：

```
$ codesign -dv <解包>/speech
flags=0x20002(adhoc,linker-signed)  Signature=adhoc  TeamIdentifier=not set
$ spctl -a -vv <解包>/speech
speech: rejected
$ xattr -r <解包> | head -1
…: com.apple.provenance          # 没有 com.apple.quarantine（用 gh CLI 下载）
```

依赖（`otool -L libexec/speech`）：只有系统 framework 与 `/usr/lib/swift/*.dylib`
（Foundation / Metal / Accelerate / AVFoundation / CoreML / CoreAudio / Network …），
**没有任何 brew 路径**。

`speech --version` 不存在（`Error: Unknown option '--version'`）。

## 2. 独立二进制能不能脱离 brew 跑

```
$ cd <解包目录>; for i in 1 2 3; do /usr/bin/time -p ./speech transcribe --engine qwen3 -m 1.7B -- zh_smoke_000.wav; done
Result: 今天清迈天气还不错，最高三十二度。  real 4.20     # 第一次（推断：Metal 着色器缓存首编）
Result: 今天清迈天气还不错，最高三十二度。  real 1.77
Result: 今天清迈天气还不错，最高三十二度。  real 1.79
# 对照：brew 装的 0.0.26 同命令
real 1.81 / 1.81 / 1.81
```

## 3. 模型权重从哪来、放在哪

```
$ ls ~/Library/Caches/qwen3-speech/models/aufklarer/
Qwen3-ASR-0.6B-MLX-4bit  Qwen3-ASR-1.7B-MLX-8bit  Silero-VAD-v6.2.1-MLX  …
$ du -sh …/Qwen3-ASR-1.7B-MLX-8bit …/Qwen3-ASR-0.6B-MLX-4bit
2.3G  680M
$ ls …/Qwen3-ASR-1.7B-MLX-8bit
config.json merges.txt model.safetensors model.safetensors.index.json tokenizer_config.json vocab.json

# HF API（?blobs=true）
aufklarer/Qwen3-ASR-1.7B-MLX-8bit  license apache-2.0  sha e5450a26d1fd…
  model.safetensors 2463307541  lfs sha256 bf304b009cc7eca79283056f787b44c952d24ac22cec787b39732bba3c23c13c
  total 2467857518
aufklarer/Qwen3-ASR-0.6B-MLX-4bit  license apache-2.0  sha bc441bd1e429…
  model.safetensors 708236945   lfs sha256 70c7e67e588062adce4f10796e47ad42ead51c6671eda61a0987eae38ca95ddf
  total 712779703
Qwen/Qwen3-ASR-1.7B (官方原版)      license apache-2.0  total 4703114308

$ shasum -a 256 ~/Library/Caches/qwen3-speech/models/aufklarer/Qwen3-ASR-1.7B-MLX-8bit/model.safetensors
bf304b009cc7eca79283056f787b44c952d24ac22cec787b39732bba3c23c13c   # 与 HF 声明一致
```

下载目录能不能指定（`strings speech` 找到的环境变量）：
`QWEN3_ASR_CACHE_DIR`、`HF_DOWNLOAD_RANGE_CHUNK`、`HF_DOWNLOAD_RANGE_CONCURRENCY`、
`HF_DOWNLOAD_RANGE_THRESHOLD`、`HF_DOWNLOAD_STALL_TIMEOUT`；**没有 `HF_ENDPOINT`**，
基址写死 `https://huggingface.co`。

- `-m <本地目录>`（help 里写着 "alternatively a model ID or local directory"）**对 qwen3 引擎不管用**：
  ```
  Error: failedToDownload("…/Qwen3-ASR-1.7B-MLX-8bit after 5 attempts (target:
  …/models/Users/jason/Library/…): … tree listing HTTP 404")   real 117.64
  ```
  它把路径当成 HF 仓库名，重试 5 次、耗时约 2 分钟后才失败。
- `QWEN3_ASR_CACHE_DIR` **管用**，但目录布局是 `$DIR/qwen3-speech/models/<org>/<repo>/`
  （试了 `$DIR/models/…` 与 `$DIR/<org>/…`，它都会在 `$DIR/qwen3-speech/models/…` 下另起目录开始下载）；
  **模型目录不能是符号链接**（`NSPOSIXErrorDomain Code=20 "Not a directory"`），文件放实体（这里用硬链接）即可：
  ```
  $ QWEN3_ASR_CACHE_DIR=$PWD/cc speech transcribe --engine qwen3 -m 1.7B -- zh16k.wav
  Result: 今天清迈天气还不错，最高三十二度。  real 1.78 / 1.79 / 1.82
  ```
- **断网能跑**（缓存齐全时）：
  ```
  $ sandbox-exec -p '(version 1)(allow default)(deny network-outbound (remote tcp "*:*"))(allow network-outbound (remote unix-socket))' \
      env QWEN3_ASR_CACHE_DIR=$PWD/cc speech transcribe --engine qwen3 -m 1.7B -- zh16k.wav
  Result: 今天清迈天气还不错，最高三十二度。  real 1.77
  # 对照：同一 sandbox 下 curl https://huggingface.co → 000（确实断了）
  ```

## 4. 常驻形态：speech-server（v0.0.28 预编译包里自带）

```
$ speech-server --help
USAGE: speech-server [--host <host>] [--port <port>] [--preload]
# strings 里的路由：POST /transcribe、POST /v1/audio/transcriptions（OpenAI 兼容，multipart）、
# /speak、/v1/audio/speech、/respond、/enhance、/v1/chat/completions …
# 模型名：qwen3-asr-1.7b、qwen3-asr-1.7b-mlx-int8、qwen3-asr-0.6b-mlx-int4 …

$ speech-server --port 8797 &          # 2s 起来，/health → {"status":"ok"}
$ curl -F file=@zh_smoke_000.wav(48k) -F model=qwen3-asr-1.7b …/v1/audio/transcriptions
t=1.721  今天钦脉天气还不错，最高三十二度。     # 首次（含加载）
t=0.158 / 0.157 / 0.157 / 0.156  同上文本
$ … 同上但 zh16k.wav
t=0.229  今天清迈天气还不错，最高三十二度。
t=0.160  （model=qwen3-asr-1.7b-mlx-int8）今天清迈…
$ ps -o rss= -p <pid>     # 只加载了 1.7B 时
2551952   (KB ≈ 2.43 GiB)
```

- 48 kHz 输入时 server 出「钦脉」、CLI 出「清迈」；换成 16 kHz 两边一致。
  **推断**：server 的重采样路径与 CLI 不同。AgentEar 录的是 16 kHz，不受影响；
  但「server 与 CLI 输出等价」**只在这一条 16 kHz 样本上成立**，没有统计意义。
- ⚠️ **副作用（我造成的，需要 jason 知道）**：`POST /transcribe` 不带模型名时默认用
  `parakeet-tdt-v3-coreml-int8-30s`，**当场下载了 611 MB** 到
  `~/Library/Caches/qwen3-speech/models/aufklarer/Parakeet-TDT-v3-CoreML-INT8-30s`
  （首个请求 130.7 s，转写结果为空字符串——那是英文模型）。**没有删**，要不要删由 jason 决定。
  落地时**必须显式传模型名**，否则同样会在用户机器上静默下载一个用不上的模型。

## 5. GGUF 路线：llama.cpp + ggml-org/Qwen3-ASR-*-GGUF

```
# ggml-org 的模型卡原文：「llama-server -hf ggml-org/Qwen3-ASR-1.7B-GGUF」
Qwen3-ASR-1.7B-Q8_0.gguf        2165034944  sha256 58e22d0532d4eacaf034cfac17a6fed159f37c41390c710186783be439d1fc57
mmproj-Qwen3-ASR-1.7B-Q8_0.gguf  355709344  sha256 46c1d533af3f354ceb37ce855dbceff7da7fa7cf1e6a523df3b13440bd164c0d
Qwen3-ASR-0.6B-Q8_0.gguf         804749248
mmproj-Qwen3-ASR-0.6B-Q8_0.gguf  214392480
# 模型卡 cardData.license = None（base_model Qwen/Qwen3-ASR-1.7B 是 apache-2.0）

$ gh release view b11200 -R ggml-org/llama.cpp → llama-b11200-bin-macos-arm64.tar.gz 11755479（解包 30 MB）
$ shasum 下载的两个 gguf → 与上面 HF 声明一致

$ llama-server -m q17.gguf --mmproj mm17.gguf --port 8798 -ngl 99 --temp 0     # 默认上下文
# POST /v1/chat/completions，content = [{type: input_audio, input_audio:{data:<b64 wav>, format: wav}}]
0 0.612 'language Chinese<asr_text>今天钦脉天气还不错，最高三十二度。'
1 0.122 / 2 0.121 / 3 0.123 / 4 0.121  同上
$ ps -o rss=  →  32012176 (KB ≈ 30.5 GiB)   ⚠️
# 日志：n_ctx_seq (262144) > n_ctx_train (65536)；n_slots = 4, n_ctx_slot = 65536
$ … 同上加 -c 4096
0 0.203 / 1 0.121 / 2 0.119
$ ps -o rss=  →  3097792 (KB ≈ 2.95 GiB)；n_ctx_slot = 4096
```

- 输入是 16 kHz 的 `zh16k.wav`，结果是「钦脉」，与 speech CLI 的「清迈」不同。**单条样本，不能据此说谁更准**。
- 输出带 `language Chinese<asr_text>` 前缀，要自己剥。
- **默认上下文会吃掉 30 GB 级内存**——这与 v0.17.0 查过的「边车 38 GB 把别的任务 OOM 掉」是同一类风险，
  走这条路必须把 `-c` 钉死并有测试守着。
- `llama-mtmd-cli -p ""` 会进入交互模式等 stdin（本轮挂住 5 分钟后手动杀掉），CLI 形态要写真实 prompt 或只走 server。

## 6. Python MLX 路线：mlx-audio 0.5.1 自带 qwen3_asr

```
$ ~/.agentear/llm/venv/bin/python -c "import importlib.metadata as m;print(m.version('mlx-audio'))"
0.5.1
$ ls …/site-packages/mlx_audio/stt/models/
… qwen3_asr qwen3_forced_aligner sensevoice whisper …
# 本机 HF 缓存里只有 mlx-community/Qwen3-ASR-0.6B-8bit（960 MB），没有 1.7B
load_model("mlx-community/Qwen3-ASR-0.6B-8bit")  load 2.76 s
generate(zh16k.wav): 1.039 / 0.155 / 0.281 / 0.204 s   → 今天青脉天气还不错，最高三十二度。
```

⚠️ 测的是 **0.6B**，与上面两条 1.7B 不可横比；只证明「这条路存在、常驻后是亚秒级」。
