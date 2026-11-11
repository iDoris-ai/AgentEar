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

### 1.3 仍然没锁住的（诚实清单）

| 项 | 现状 | 风险 / 后续 |
|---|---|---|
| `vendor/bin/llama-funasr-sensevoice`、`vendor/bin/whisper-cli` | 随发布包分发，**仓库里没有构建脚本，也没有记录它们对应的上游 commit**（开发机上现有副本 sha256：`49d66b2f…435` / `322b8ce3…603`） | 无法从源码复建；换机器只能从旧 release 拷。后续：补 `build-llama-funasr.sh` 并记录 commit |
| `whisper.cpp` checkout | `build-thai-model.sh` / `build-whisper-cli.sh` 读同级目录；ADR-0004 记的是 `7de8dd78`，**脚本不核对** | 换 commit 产出的就不是测过的东西。后续：脚本里 `git rev-parse HEAD` 与常量比对 |
| 泰语构建的 `torch` / `transformers` / `numpy` | `build-thai-model.sh` 里仍未钉版本 | 一次性维护者脚本，产物有 sha12 指纹兜底；没有当时用的版本记录，不能凭空钉 |
| `pip install --upgrade pip` | `setup-talk.sh` 里新建 venv 时仍升级 pip 到最新 | 只是安装器本身，不影响运行时行为 |
| Python 间接依赖（mlx-metal 等） | 只钉了直接依赖 | 要完全可复现需 `pip freeze` 约束文件；暂未做 |
| HF 镜像（`HF_ENDPOINT`） | `setup-llm.sh` / `build-thai-model.sh` 允许镜像优先；revision 钉了，但镜像内容未做 sha 校验 | 内置模型和 Qwen3 有 sha256，这两个脚本的大权重没有 |
| 对话 LLM / TTS 权重文件 sha256 | 只钉到 commit（HF 对 commit 内容不可变），无逐文件 sha | 够用；要更严就照 `FILES_*` 补 |

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
