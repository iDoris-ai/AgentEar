# 上游外部依赖：锁定清单与回滚流程（T3.5.5）

> 只记代码里真实存在的值（2026-11-10 读代码得出）。改任何一行 pin，同步改这里；
> `tests/upstream_pins.rs` 钉住「没有退回到跟着 main / 最新版走」。

## 1. 清单

### 1.1 运行时下载（用户机器上发生，sha256 校验）

| 依赖 | 来源 | 锁定方式 | 位置 |
|---|---|---|---|
| 泰语 whisper 模型（q5_0） | AgentEar 自己的 release `models-th-v1` | URL 固定 tag + **sha256 + 字节数** | `src/download.rs` `THAI` |
| SenseVoice q8 / FSMN-VAD（内置 ASR） | HF `FunAudioLLM/*-GGUF` | `resolve/<完整 40 位 commit>/` + **sha256 + 字节数** | `src/builtin_models.rs` |
| Qwen3-ASR 运行时（自编 speech-server） | AgentEar release `speech-runtime-<版本>` | 版本号字符串 + **sha256 + 字节数 + 解包后体积** | `src/qwen3.rs` `SPEECH_VERSION` / `RUNTIME_URL` / `RUNTIME_SHA256` / `RUNTIME_BYTES` |
| Qwen3-ASR 权重 0.6B / 1.7B | HF `aufklarer/Qwen3-ASR-*` | `resolve/<完整 40 位 commit>/` + **逐文件 sha256 + 字节数**；安装记录写 commit | `src/qwen3.rs` `revision()` / `FILES_*` |

### 1.2 构建期 / 安装期（开发者或用户跑脚本）

| 依赖 | 锁定方式 | 位置 |
|---|---|---|
| speech-swift 上游源码 | tag `v0.0.28` + **完整 commit `231f8eb9…`** 核对 + 本仓库 `patches/*.patch` | `scripts/build-speech-runtime.sh` |
| 官方 speech 发布包（借 `speech` CLI / metallib / bundle） | **sha256 + 字节数** | 同上 |
| 对话 LLM 权重 MiniCPM5-2B | `hf download --revision <40 位>`（T3.5.5 新增）；`.installed` 记录该 commit | `scripts/setup-talk.sh` `LLM_REV_DEFAULT` |
| 对话 TTS 权重 VoxCPM2 4bit / 8bit | 同上，两档各一个 commit | `scripts/setup-talk.sh` `TTS_REV_DEFAULT` |
| 理解层 LLM 权重 Ornith-1.0-9B-6bit | `resolve/<40 位>/`（T3.5.5 新增，原先每次问 API 要 HEAD） | `scripts/setup-llm.sh` `HF_REV` |
| 泰语微调权重 `biodatlab/distill-whisper-th-large-v3` | `resolve/<40 位>/`（T3.5.5 新增，原先取 HEAD 再比 12 位前缀）+ 产物 sha12 指纹 | `scripts/build-thai-model.sh` |
| `mel_filters.npz`（OpenAI whisper 资产） | commit `86098128…` + 文件 **sha256**（T3.5.5 新增，原先取 `main`） | 同上 |
| Python 包 `mlx==0.32.2 mlx-lm==0.31.3 mlx-audio==0.5.1 huggingface_hub==1.29.0` | `==` 钉版本（T3.5.5 新增，取自实测在用的 venv） | `scripts/setup-talk.sh` `PIP_PINS` |
| Python 包 `mlx==0.32.2 mlx-dspark==0.18.0 huggingface_hub==1.29.0` | 同上 | `scripts/setup-llm.sh` |

### 1.3 FU-24 补锁的（2026-11-11）

| 项 | 锁定方式 | 位置 |
|---|---|---|
| `vendor/bin/llama-funasr-sensevoice` | 开发机上的副本与 FunASR 官方 release `runtime-llamacpp-v0.1.9`（tag 对象 `d10aedff3c50`，指向 commit `9b918d74d7090b38211a6414c6ee08893a34afd4`）的 `funasr-llamacpp-macos-arm64.tar.gz`（sha256 `2d5786784ad09d8f4def1d942f678728638fe601d00acf0dad7cf094a9328363`）里的同名二进制**逐字节相同**：sha256 `49d66b2f79d439e2db7933627e1deb9eb7f3ebf0d708473757828130f3619435`。即它是官方预编译产物，不是自编。该 commit 的 `runtime/llama.cpp/CMakeLists.txt` 用 FetchContent 拉 `ggml-org/llama.cpp` @ `8086439a4cea94c71a5dfb8fe4ad1546aebd640f`；官方 CI 的 macos-arm64 参数是 `-DGGML_NATIVE=OFF` | `scripts/build-vendor-asr.sh` |
| `vendor/bin/whisper-cli` | `ggml-org/whisper.cpp` @ `7de8dd783f7b2eab56bff6bbc5d3369e34f0e77f`，静态链接 + Metal 嵌入（参数见 `scripts/build-whisper-cli.sh`）。开发机副本 sha256 `322b8ce3c2bc7790adcc9fd28e7c24f25338e08d1ae27d4585e94981743c0603` | 同上 |
| `whisper.cpp` checkout | `/Users/jason/Dev/tools/whisper.cpp` 的 `HEAD` = `7de8dd783f7b2eab56bff6bbc5d3369e34f0e77f`，**与 ADR-0004 记载的 `7de8dd78` 一致**（2026-11-11 `git rev-parse` 核对）。`build-vendor-asr.sh` 现在比对**完整 40 位** commit，不一致直接失败（`build-whisper-cli.sh` 单跑时仍只对 8 位前缀发警告）。checkout 不存在时脚本按钉死的 commit 自己浅克隆 | 同上 |
| 泰语构建的 torch / transformers / numpy 等 | `==` 钉在要求文件，`build-thai-model.sh` 用 `uv pip install -r` | `scripts/pins/thai-build-requirements.txt` |
| 间接 pip 依赖（mlx-metal、mlx-vlm、numpy…） | 约束文件（`-c`）= 2026-11-11 `uv pip freeze` 全量 57 个包；`setup-llm.sh` / `setup-talk.sh` 安装时带上 | `scripts/pins/venv-constraints.txt` |
| 理解层 LLM 权重 Ornith 6bit 逐文件 sha256 | 12 个文件；大文件取 HF API `lfs.oid`，小文件按该 commit 的 resolve URL 实算；`setup-llm.sh` 下载后 `shasum -a 256 -c`。本机 `~/.agentear/llm/models/ornith` 实测全部 OK。用 `AGENTEAR_LLM_REV` 覆盖 revision 时清单不适用，脚本明说跳过 | `scripts/pins/ornith-1.0-9b-6bit.sha256` |
| 泰语微调权重 distill-whisper-th-large-v3 逐文件 sha256 | 11 个文件，取法同上；`build-thai-model.sh` 在转换前校验（镜像内容也要过） | `scripts/pins/distill-whisper-th-large-v3.sha256` |

**复建实测（2026-11-11，机器负载高）**：`scripts/build-vendor-asr.sh` 两个目标都跑了一次，各约 1.5 分钟（FunASR 只编 sensevoice 一个 target）。

- `whisper-cli` 自编产物 sha256 = `322b8ce3…0603`，**与 vendor 副本逐字节相同**（同一台机器、同一工具链，所以这一次可复现；换工具链不保证）。
- `llama-funasr-sensevoice` 自编产物 sha256 = `76190ea7686330d3ed9e0731d34ef369af9a8d37dbdb67861776fb5e0e0d3864`，**与官方预编译 `49d66b2f…` 不同**（官方是 GitHub runner 编的，路径、编译器、`GGML_NATIVE=OFF` 之外的细节都可能不同）。行为：用 `AGENTEAR_VENDOR` 指向自编产物，`agentear --transcribe` 一段中文 `say` 和一段英文 `say`，转写文本与 vendor 副本**完全相同**（只有 2 段合成语音，耗时在高负载下无参考价值）。
- 想要逐字节一致的 sensevoice：直接下官方 tarball，核对上面的 tarball sha256 与二进制 sha256（README「从源码构建」里的 curl 命令即是；注意它没有校验 sha256，是遗留项）。

### 1.4 仍然没锁住的（诚实清单）

| 项 | 现状 | 风险 / 后续 |
|---|---|---|
| 泰语要求文件的版本是**未经完整重跑验证**的 | 取自本机 `~/venvs/ml` 里实装的 torch 2.12.0 / transformers 5.15.1 / numpy 2.4.4 …；2026-08-28 那次跑通转换的版本**没有记录**，本次也没有重下 3 GB 权重跑完整转换 | 下一次实际重跑 `build-thai-model.sh` 时，若转换失败或产物 sha12 指纹不符，改要求文件并记录；torch 的间接依赖与 `--hash` 未做 |
| `pip install --upgrade pip` | `setup-talk.sh` 里新建 venv 时仍升级 pip 到最新 | 只是安装器本身，不影响运行时行为 |
| 约束文件来自一个 venv 的 freeze | 该 venv 同时装了 mlx-dspark / mlx-vlm / fastapi 等，约束对 talk venv 多余但无害；没有 `--hash` | 要更严：`uv pip compile --generate-hashes` |
| README 里 `funasr-llamacpp-macos-arm64.tar.gz` 的下载命令 | 没校验 tarball sha256，且 SenseVoice/VAD 的 GGUF 用 `resolve/main`（README 示例；运行时下载走 `builtin_models.rs`，已钉） | 文档示例，不影响发布包；后续可补 sha256 校验 |
| 对话 LLM / TTS（MiniCPM5 / VoxCPM2）权重文件 sha256 | 只钉到 commit（HF 对 commit 内容不可变），无逐文件 sha | 够用；要更严就照 `scripts/pins/*.sha256` 补 |

## 2. 版本握手 / 能力探测

- **Qwen3 运行时**（本任务新增）：`SpeechSwiftEngine::preflight` 在「安装记录对得上」之后，
  还会真跑 `speech --help` / `speech transcribe --help` / `speech-server --help`
  （套断网沙箱），要求 `transcribe` 子命令与 `--engine --model --language --context`
  `--host --port` 都在（`qwen3::probe_runtime_contract`）。缺任何一个就**拒绝使用**，
  报错里写明缺什么和怎么修。成功后本进程内缓存；失败不缓存。
  `required_flags_cover_what_transcribe_passes` 测试防止「代码开始传新参数而探测表没跟上」。
- **PATH 上的老 `speech`（brew）**：原本就有同样的探测，未改。
- 没做：输出格式（`Result:` 行）由 `parse_speech_output` 的单测与运行期报错兜底；
  speech-server 没有版本接口，所以不做运行期版本比对——版本由安装记录 + sha 决定。

## 3. 回滚到上一个已验证版本（Qwen3 运行时）

升级时旧版本目录**不会被自动删**（见 `src/qwen3.rs` `runtime_dir` 上的注释），所以回滚不需要重新下载：

1. 确认旧目录还在：`ls ~/.agentear/models/qwen3/runtime/`，里面有 `runtime/speech-<旧版本>/`。
   同级的 `~/.agentear/models/qwen3/runtime-<旧版本>.installed` 是它的安装记录（内容是当时的 sha256）。
2. 改 `src/qwen3.rs` 四个常量回到旧值：`SPEECH_VERSION`、`RUNTIME_URL`、`RUNTIME_SHA256`、
   `RUNTIME_BYTES`（以及 `RUNTIME_UNPACKED_BYTES` 如有变）。旧值取自 git 历史里发那一版的提交，
   或 `docs/releases/v*-notes.md`。`UPSTREAM_TAG` 只用于日志。
3. 因为目录名与记录文件名都从 `SPEECH_VERSION` 派生，旧目录与旧记录会立刻被重新认作「已安装」，不重下。
   旧目录被删过时，会按旧 URL + sha256 重新下载（旧 release 资产不要从 GitHub 删）。
4. 模型权重与运行时版本无关，不用动。
5. 发回滚版：`scripts/build-speech-runtime.sh` 的产物是可复现的（pinned commit + 补丁 + 官方包 sha）；
   旧补丁在 git 历史的 `patches/` 里。
6. 其余模型回滚：改对应常量里的 commit（见第 1 节表），HF 上 commit 内容不可变。
   `~/.agentear/talk/models/*/.installed` 记着当前装的 commit，换 commit 后删掉对应模型目录重跑 `scripts/setup-talk.sh`。

## 4. 升级一个 pin 的最小清单

改常量 → 本机实测（`--asr-bench` / `talk-e2e.sh`）→ 更新本文件 → `cargo test`
（`tests/upstream_pins.rs` 会拦下退回 main / 不带 `==` 的改法）。
