# 逐组件许可证表（T3.5.3）

**核实日期：2026-11-10；FU-23 修订（2026-11-11）重读了 SenseVoice / Ornith / VoiceChat / 泰语模型的一手来源。** 核实方式：读 HuggingFace 模型卡与 `api/models/<repo>` 的许可元数据、
读 GitHub 上的 LICENSE / README、读 PyPI 元数据。本文**不是法律意见**。

读法：
- 「✅」= 我读到了许可文本或模型卡明确声明，且与别处不矛盾。
- 「⚠️」= 读到了，但有矛盾、有附加条款，或我们现有文档与上游现状对不上。
- 「未核实」= 没读到权威文本（常见原因：门控模型要登录；仓库没有 LICENSE 文件）。**不要当成「宽松」。**
- 「修订」类链接优先带 commit / 版本；HF 的 sha 是 2026-11-10 查到的 `main` 当时的 revision，会随上游更新漂移。

三类关系：**随包**（`.app` 里带）、**按需下载**（用户在设置里点才下）、**仅评估**（没进产品，T3.5.3 点名要查）。

---

## 1. 随包（打进 `.app` 的）

| 组件 | 代码许可 | 权重许可 | 再分发义务 | 商用限制 | 来源（修订） |
|---|---|---|---|---|---|
| **SenseVoiceSmall q8**（`sensevoice-small-q8.gguf`） | 上游代码 MIT（QwenAudio/SenseVoice） | ⚠️ **官方权重是「FunASR Model Open Source License Agreement v1.1」，不是 Apache-2.0**。我们用的 GGUF 转换版 HF 页写 `apache-2.0`，与官方权重的条款**对不上** | 必须**注明出处与作者，并保留模型名称**（协议 §2.2）。协议 §4.2：不得无端诋毁/恶意抹黑该模型，否则许可自动失效；§6：协议可被单方修订 | ⚠️ **商用是否允许：未最终确认。** 协议 §3 字面写「[FunASR 软件]仅作为参考和学习使用，不对您使用或修改[FunASR 软件]造成的任何直接或间接损失承担任何责任」（英文版：“[FunASR Software] is provided for reference and learning purposes only, and Alibaba Group assumes no responsibility for any direct or indirect losses…”；2026-11-11 重读 @58830eca 原文）；SenseVoice 维护者在 [issue #334](https://github.com/QwenAudio/SenseVoice/issues/334#issuecomment-5083546605) 的早期回复称 §3 只是免责、不是非商用限制，README 也照此转述，但该回复已被加注（2026-09-22）**「不是许可方最终确认」**，issue 重新打开，仍待许可方/核心维护者确认。该说明也只覆盖官方权重，不覆盖第三方转换；GGUF 的 `apache-2.0` 标注由谁授权，**未核实** | 协议文本：[FunASR MODEL_LICENSE @58830eca](https://github.com/modelscope/FunASR/blob/58830eca4012644aac0c3218c3ccc7d98f003fda/MODEL_LICENSE)；官方卡：[FunAudioLLM/SenseVoiceSmall @3847d57](https://huggingface.co/FunAudioLLM/SenseVoiceSmall)（`license: other / model-license`）；GGUF：[FunAudioLLM/SenseVoiceSmall-GGUF @90c1c61](https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF)（`apache-2.0`）；SenseVoice [README License 节](https://github.com/QwenAudio/SenseVoice#license) |
| **FSMN-VAD**（`fsmn-vad.gguf`） | 同上（FunASR，MIT） | HF GGUF 页写 `apache-2.0`；**上游 FSMN-VAD 权重的原始许可我没查**，很可能同属 FunASR 模型协议 | 同 SenseVoice 一行的保守处理：注明出处 + 保留模型名 | **未核实**（上游原模型卡没读） | [FunAudioLLM/fsmn-vad-GGUF @6840bae](https://huggingface.co/FunAudioLLM/fsmn-vad-GGUF) |
| **llama-funasr-sensevoice**（推理二进制） | ✅ MIT（FunASR 仓库级 LICENSE；`runtime/llama.cpp` 子目录是否另有声明**未核实**） | — | 保留 MIT 版权声明（NOTICE 已有） | 无 | [modelscope/FunASR](https://github.com/modelscope/FunASR)，release tag `runtime-llamacpp-v0.1.9`（tag 对象 `d10aedff3c50`） |
| **whisper-cli**（泰语引擎，2.5 MB 静态链接） | ✅ whisper.cpp MIT（GitHub API：MIT；本机构建 commit `7de8dd78`，`v1.7.6-121`） | — | 保留 MIT 声明。✅ `NOTICE` 已于 FU-23 修订（2026-11-11）补上 whisper.cpp（版权行 “Copyright (c) 2023-2026 The ggml authors”）——见 §6.5 | 无 | [ggml-org/whisper.cpp](https://github.com/ggml-org/whisper.cpp) |
| **默认音色库**（`assets/talk-voices/*.wav`，2 条） | — | 见 §6.4：VoxCPM2 生成物，非真人录音 | 见 §6.4 | 见 §6.4 | `assets/talk-voices/*.json` 的 `source` 字段 |

## 2. 按需下载（用户在设置/菜单里选了才下，不进安装包）

| 组件 | 代码许可 | 权重许可 | 再分发义务 | 商用限制 | 来源（修订） |
|---|---|---|---|---|---|
| **泰语 Whisper**：`biodatlab/distill-whisper-th-large-v3` 转 GGML q5_0（**我们自己托管**在 GitHub release `models-th-v1`） | whisper.cpp MIT | ✅ 上游卡 `license: mit`。⚠️ 微调数据列了 Common Voice v13、Gowajee、SLSCU、Thai Elderly Speech Corpus，**这些数据集各自的许可及其对衍生权重的影响，未核实**；其基座（distil-large-v3 / Whisper）的许可链也**未核实**。2026-11-11 补查：biodatlab 的另两个 Thonburian 仓库 `whisper-th-medium-combined`（@eebf842）、`whisper-th-large-v3-combined`（@ba7197f）HF 元数据为 `apache-2.0`，**我们没有使用它们**（ADR-0004 评估过） | 我们是**再分发转换产物**：要随产物给出 MIT 声明与版权归属。release 正文写了「MIT、版权归 biodatlab」，但**没有附 MIT 许可全文**（只有一句话）——保守起见应补 | MIT 本身无限制；数据集链上的限制未核实 | [biodatlab/distill-whisper-th-large-v3 @62df42c](https://huggingface.co/biodatlab/distill-whisper-th-large-v3)；托管：[iDoris-ai/AgentEar release models-th-v1](https://github.com/iDoris-ai/AgentEar/releases/tag/models-th-v1)；常量见 `src/download.rs` 的 `THAI` |
| **Qwen3-ASR 0.6B（MLX 4bit）** `aufklarer/Qwen3-ASR-0.6B-MLX-4bit` | speech-swift Apache-2.0（见下行） | ✅ `apache-2.0`，基座 `Qwen/Qwen3-ASR-0.6B` 也是 `apache-2.0`，二者一致 | Apache-2.0 §4：再分发要带许可副本与版权声明。我们**不托管**权重（用户端直接从 HF 拉，钉 commit + 逐文件 sha），所以我们本身不是再分发者；分发者是 HF | 无 | [aufklarer/…0.6B-MLX-4bit @bc441bd](https://huggingface.co/aufklarer/Qwen3-ASR-0.6B-MLX-4bit)；基座 [Qwen/Qwen3-ASR-0.6B @5eb1441](https://huggingface.co/Qwen/Qwen3-ASR-0.6B) |
| **Qwen3-ASR 1.7B（MLX 8bit）** `aufklarer/Qwen3-ASR-1.7B-MLX-8bit` | 同上 | ✅ `apache-2.0`；基座 `Qwen/Qwen3-ASR-1.7B` `apache-2.0` | 同上 | 无 | [aufklarer/…1.7B-MLX-8bit @e5450a2](https://huggingface.co/aufklarer/Qwen3-ASR-1.7B-MLX-8bit)；基座 [@7278e1e](https://huggingface.co/Qwen/Qwen3-ASR-1.7B) |
| **speech-swift 运行时（上游）** `soniqo/speech-swift` v0.0.28（commit `231f8eb9f097`） | ✅ Apache-2.0（GitHub API + 仓库 `LICENSE`） | — | 同下行 | 无 | [soniqo/speech-swift @v0.0.28](https://github.com/soniqo/speech-swift/tree/v0.0.28) |
| **speech-swift 运行时（我们打补丁的再分发版）** `speech-runtime-v0.0.28-agentear.1`，我们托管在自己的 GitHub release | Apache-2.0 + 我们的补丁（`patches/speech-swift-v0.0.28-agentear.patch`） | — | **义务清单见 §6.3**。⚠️ 现状有缺口：tarball 里只有 `AGENTEAR-PATCH.md`，**没有 Apache-2.0 全文、没有上游 NOTICE/ThirdPartyNotices、也没有它静态链接的 MLX 等依赖的 MIT 声明** | 无 | [release](https://github.com/iDoris-ai/AgentEar/releases/tag/speech-runtime-v0.0.28-agentear.1)；构建脚本 `scripts/build-speech-runtime.sh` |
| **speech-swift 静态链接的 Swift 依赖**（`Package.swift @v0.0.28`） | mlx-swift **MIT**；mlx-swift-lm **MIT**；swift-argument-parser / swift-system / swift-transformers / hummingbird / hummingbird-websocket / swift-websocket **Apache-2.0**；WhisperKit **MIT**；⚠️ `modelcontextprotocol/swift-sdk` GitHub 报 **NOASSERTION，未核实** | — | 二进制再分发 → 这些依赖的版权/许可声明理论上都要随附。具体哪些真的被链进 `speech-server`，**我没逐个查依赖闭包** | 无已知 | [Package.swift @v0.0.28](https://github.com/soniqo/speech-swift/blob/v0.0.28/Package.swift)；各仓库 GitHub 许可元数据 2026-11-10 |
| **MLX 运行时 `mlx.metallib`**（随上面 tarball） | ✅ ml-explore/mlx MIT | — | 保留 MIT 声明（目前 tarball 里没有） | 无 | [ml-explore/mlx](https://github.com/ml-explore/mlx) |

## 3. 通话/理解层边车（用户自己跑 `scripts/setup-*.sh`，**不随包、不由我们托管**）

这些权重是用户机器上直接从 HF 下载的，我们不做再分发；但它们是我们**推荐的默认配置**，所以许可也要记。

| 组件 | 代码许可 | 权重许可 | 再分发义务 | 商用限制 | 来源（修订） |
|---|---|---|---|---|---|
| **VoxCPM2 4bit / 8bit**（`mlx-community/VoxCPM2-4bit`、`-8bit`） | OpenBMB/VoxCPM Apache-2.0 | ✅ mlx-community 两份都 `apache-2.0`，基座 `openbmb/VoxCPM2` 卡写 `apache-2.0`，并明确 “free for commercial use” | 用户自取，我们不分发 | 许可本身无商用限制（卡写 free for commercial use）。⚠️ 但模型卡 Limitations 写明：**严禁用于冒充、欺诈、虚假信息；AI 生成内容应明确标注**——是否构成许可条件需法务判断，至少是我们产品必须遵守的使用要求（语音输出应考虑标注） | [4bit @dc9e5c1](https://huggingface.co/mlx-community/VoxCPM2-4bit)、[8bit @d527258](https://huggingface.co/mlx-community/VoxCPM2-8bit)、[基座 @7936080](https://huggingface.co/openbmb/VoxCPM2) |
| **MiniCPM5-2B（MLX 4bit）** `mlx-community/MiniCPM5-2B-mlx-4Bit` | OpenBMB/MiniCPM Apache-2.0 | ✅ `apache-2.0`；基座卡明写 “model weights are released under Apache-2.0” | 同上 | 无 | [mlx-community @86ad518](https://huggingface.co/mlx-community/MiniCPM5-2B-mlx-4Bit)、[openbmb/MiniCPM5-2B @42433e9](https://huggingface.co/openbmb/MiniCPM5-2B) |
| **Ornith-1.0-9B（MLX 6bit）** `mlx-community/Ornith-1.0-9B-6bit` | — | ⚠️ **只有声明，没有许可文本**（2026-11-11 重核）：HF 元数据 `license: mit`，基座 README 正文写 “MIT licensed, globally accessible, and free from regional limitations”；但 ① 基座 `ornith-ai/Ornith-1.0-9B` 与量化版 `mlx-community/Ornith-1.0-9B-6bit` 的文件列表里**都没有 LICENSE 文件**（HF API `siblings`）；② 两份卡的 `license_link` 都指向 `deepreinforce-ai/Ornith-1.0-9B/blob/main/LICENSE`，跟随重定向后落到 `ornith-ai/…/blob/main/LICENSE`，**HTTP 404**；③ 仓库归属有改名/迁移痕迹（`deepreinforce-ai` → `ornith-ai`）。**所以：声明为 MIT，但没有任何可引用的许可全文与版权人署名行。保守处理：不要当作「已确认可再分发」** | MIT 的核心义务是随附版权与许可声明——**没有许可文本可附**。**我们不随包分发它**（见下：`scripts/bundle.sh` 的体积守卫禁止 `vendor/models` 出现大权重，仅允许 SenseVoice / FSMN-VAD 两个 gguf；权重由用户自己 `scripts/setup-*.sh` 从 HF 下载），所以目前没有再分发义务触发；**任何把它打进包或自己托管的动作，都要先拿到上游许可文本** | 若 MIT 成立则无；**许可文本未核实** | [mlx-community @a280093](https://huggingface.co/mlx-community/Ornith-1.0-9B-6bit)、[ornith-ai/Ornith-1.0-9B @83dc1f5](https://huggingface.co/ornith-ai/Ornith-1.0-9B) |
| **mlx / mlx-lm / mlx-audio**（Python 包） | ✅ mlx MIT（PyPI 0.32.4 许可字段 MIT）；✅ mlx-lm MIT（GitHub API；PyPI 许可字段为空）；mlx-audio MIT（GitHub API；PyPI 许可字段为空，⚠️ GitHub 上 `Blaizzy/mlx-audio` 现在重定向到 `Neywa-Labs/mlx-audio`，项目主页是 neywalabs.ai，**所有者变更我没追查**） | — | 用户 `pip install`，我们不分发 | 无 | [mlx](https://github.com/ml-explore/mlx)、[mlx-lm](https://github.com/ml-explore/mlx-lm)、[mlx-audio](https://github.com/Blaizzy/mlx-audio)，PyPI 0.6.5（mlx-audio） |
| **mlx-community/OmniVoice-bf16**（ADR-0005 评估过，未进默认） | — | ⚠️ **无许可标注**（HF 元数据 `license: None`；README 取不到，返回 “Entry not found”） | — | **未核实 → 不得进入默认方案** | [@8fb0b75](https://huggingface.co/mlx-community/OmniVoice-bf16) |

## 4. 仅评估（T3.5.3 点名；没有任何一个进产品）

| 组件 | 代码许可 | 权重许可 | 再分发义务 | 商用限制 | 来源（修订） |
|---|---|---|---|---|---|
| **IndexTTS2**（`index-tts/index-tts`、`IndexTeam/IndexTTS-2`） | ⚠️ GitHub 报 `NOASSERTION/Other`，仓库 `LICENSE` 是 **“bilibili Model Use License Agreement”**（自定义协议，不是 OSI 许可） | **同一份协议**覆盖权重。**HF 模型卡没有任何 license 字段** | 下游接收方必须受同一协议约束并保留协议副本（§3.4a/b）；分发衍生物要写免责声明（§4.1a）；衍生物含微调/量化/**模型输出的衍生**（§1.5） | ⚠️ **有实质限制**：①使用者**或其任一关联方**上一自然月产品月活 >1 亿、或上一自然年收入 >人民币 10 亿，须另申请授权，未获书面授权前不得行使任何权利（§2.2；小团队身份本身不构成豁免，要看关联方）；② **不得用它或衍生物改进任何 AI 模型**，**例外**：IndexTTS2 自身、其衍生物、**非商用** AI 模型（§3.4c；对 IndexTTS2 自身及其衍生物的豁免未附加非商用条件）；③ 须确保不部署于高风险场景（医疗诊断、自动驾驶、军事等）；坚持部署的，须自行完成全部合规义务（§4.2）；④ 使用者承担全部侵权与赔偿责任（§4.3）。对个人/小团队产品可商用，但**协议可被理解为限制「拿输出训练别的商用模型」**，需法务判断 | [index-tts/index-tts `LICENSE`](https://github.com/index-tts/index-tts/blob/main/LICENSE)（2026-11-10 读取，未取固定 commit）；[HF 卡 @740dcaf](https://huggingface.co/IndexTeam/IndexTTS-2) |
| **Smart Turn v3**（`pipecat-ai/smart-turn`） | ✅ BSD-2-Clause | ✅ `pipecat-ai/smart-turn-v3` 卡 `license: bsd-2-clause`。训练数据许可**未核实** | 保留 BSD 声明 | 无 | [pipecat-ai/smart-turn](https://github.com/pipecat-ai/smart-turn)；[HF @f766f81](https://huggingface.co/pipecat-ai/smart-turn-v3) |
| **LocalVQE**（`localai-org/LocalVQE`） | ✅ Apache-2.0 | ✅ `LocalAI-io/LocalVQE` 卡 `apache-2.0`。README 说明训练数据来自 Microsoft ICASSP 2023 DNS Challenge（CC BY 4.0）+ AEC Challenge——**CC BY 4.0 对数据署名，对权重是否传导归属，未核实**。README 另有「不得用于紧急/安全关键场景」的警示（非许可条款） | Apache-2.0 §4 | 无 | [localai-org/LocalVQE](https://github.com/localai-org/LocalVQE)（搜索命中的是这个仓库，**与 ADR/任务里所指是否同一项目，未核实**）；[HF @29ca384](https://huggingface.co/LocalAI-io/LocalVQE) |
| **FunctionGemma 270M**（`google/functiongemma-270m-it`） | — | ⚠️ HF 元数据 `license: gemma`，**门控（手动批准）**，我没登录，**Gemma Terms of Use 与使用政策的原文没读** | Gemma 条款通常含再分发须带条款副本、衍生模型须传递使用限制——**属记忆，未核实** | **未核实**（Gemma 条款含使用限制清单，不是 OSI 开源） | [HF @39eccb0](https://huggingface.co/google/functiongemma-270m-it) |
| **PersonaPlex 7B**（`nvidia/personaplex-7b-v1`、`NVIDIA/personaplex`） | ✅ MIT（GitHub API + README “The present code is provided under the MIT license”） | ⚠️ README：权重按 **NVIDIA Open Model License**；HF 卡 `license: other / nvidia-open-model-license`，**门控（自动批准）**。条款原文**我没读**。基座 `kyutai/moshiko-pytorch-bf16` 的许可也未核实 | **未核实** | **未核实** | [NVIDIA/personaplex](https://github.com/NVIDIA/personaplex)；[HF @fdaf409](https://huggingface.co/nvidia/personaplex-7b-v1) |
| **VoiceChat 11B**（`nvidia/NVIDIA-NemotronLabs-VoiceChat-11B`） | 未查 | ⚠️ **与我们现有文档矛盾**：ADR-0007 写“OpenMDW 1.1 **仅研究用途**”，但 **OpenMDW-1.1 全文是宽松许可**：允许无限制使用/复制/修改/分发，含版权与专利；分发时保留协议副本与来源声明；对**输出**不设限；有诉讼终止条款（专利或版权）。HF 卡 `license: openmdw-1.1`。卡上“targeted for researchers, developers…”是**适用对象描述**，不是许可限制 | 分发时保留协议副本与原有版权/来源声明（全文无编号条款，见以 “If you distribute…” 开头的段落）；专利**或版权**诉讼会终止全部授权（对方先起诉的反诉除外） | 按 OpenMDW-1.1 文本**无商用限制**。⚠️ 但卡上还引用了其他数据/组件，**其附加条款未逐项查**；且 ADR-0007 里「仅英文」「RTF 1.13」等技术结论与许可无关、本文不动 | [HF @443794e](https://huggingface.co/nvidia/NVIDIA-NemotronLabs-VoiceChat-11B)；[OpenMDW-1.1 全文](https://github.com/OpenMDW/OpenMDW/blob/main/1.1/LICENSE.OpenMDW-1.1) |

> **VoiceChat（FU-23 已改）**：2026-11-11 重读 HF 卡：`license: openmdw-1.1`，「GOVERNING TERMS: The use of this model is governed by the OpenMDW License Agreement, version 1.1」；仓库自带 `LICENSE`（“permission is hereby granted, free of charge, to deal in the Model Materials without restriction…”）。ADR-0007 已加修订注、CLAUDE.md 同句已更正（ADR 原文保留，修订注标 2026-11-11）。

## 5. Rust 依赖（`Cargo.lock`，`cargo metadata --locked`，2026-11-10，agentear v0.28.1）

284 个第三方 crate，许可分布（SPDX 表达式，按 `license` 字段）：

| 许可表达式 | 数量 |
|---|---|
| MIT OR Apache-2.0 | 161 |
| MIT | 26 |
| Unicode-3.0 | 18 |
| MIT/Apache-2.0、Zlib OR Apache-2.0 OR MIT、Apache-2.0 OR MIT 等同类宽松组合 | 其余绝大部分 |
| Apache-2.0 | 7 |
| BSL-1.0 | 2（`clipboard-win`、`error-code`） |
| BSD-3-Clause | 1（`bindgen`，构建期） |
| ISC / MIT-0 / Zlib / 0BSD 等 | 各 1 |
| **MPL-2.0** | **1（`option-ext 0.2.0`，经 `dirs-sys → dirs 5.0.1 → agentear`）** |
| **含 LGPL 的 OR 选项** | **2（`r-efi 5.3.0 / 6.0.0`：`MIT OR Apache-2.0 OR LGPL-2.1-or-later`）** |
| GPL / AGPL（无例外的） | **0** |
| 许可字段缺失 | 0 |

要点：
- **没有 GPL / AGPL 依赖。**
- `option-ext` 是 **MPL-2.0（文件级弱 copyleft）**：作为未修改的依赖静态链接进二进制，义务是**对该库的源文件保持 MPL 并能提供其源码**；不波及我们自己的代码。目前我们没改它。
- `r-efi` 是三选一（MIT / Apache-2.0 / LGPL）；我们选 MIT 或 Apache-2.0 即可。它是 UEFI 目标用的，macOS 目标 `cargo tree` 下通常不进依赖树（本次 `cargo tree -i r-efi` 未单独验证，仅验证了 `option-ext` 在树内）。
- 此表按 `cargo metadata` 全平台依赖统计，含 Windows/Linux 目标的 crate；macOS 实际链接的是其子集，数量会少一些。**没有跑 `cargo-about` / `cargo-deny`，也没有逐个读 crate 内的 LICENSE 文件**，只信 `Cargo.toml` 里的 `license` 字段。
- 项目自身：`Cargo.toml` 与仓库 `LICENSE` 为 Apache-2.0。

## 6. 结论

### 6.1 许可尚未确认、**不能进默认方案**的组件

| 组件 | 原因 |
|---|---|
| **IndexTTS2** | 自定义 bilibili 协议，有「不得用于改进商用 AI 模型」「大厂另申请」等限制；HF 卡无许可标注。可评估，不进默认。 |
| **FunctionGemma 270M** | Gemma 条款原文没读（门控）。 |
| **PersonaPlex 7B** | 权重走 NVIDIA Open Model License，原文没读（门控）；基座 moshiko 的许可也没查。 |
| **mlx-community/OmniVoice-bf16** | 无许可标注（ADR-0005 已记）。 |
| **Ornith-1.0-9B** | 只有元数据 `mit`；`LICENSE` 文件找不到、`license_link` 404、仓库归属有变动。它现在是 M2 理解层的**默认 LLM**（ADR-0002），建议在拿到上游明确的许可文本前把它标成「默认但许可待确认」，并确认 `deepreinforce-ai` / `ornith-ai` 谁是权利人。 |
| **FSMN-VAD 权重** | 上游原始许可没查，HF GGUF 页的 `apache-2.0` 不足以证明；它**随包分发**，所以优先级最高之一。 |
| **LocalVQE / Smart Turn** | 代码与权重声明都是宽松的（Apache-2.0 / BSD-2），但训练数据许可没查。可评估；进默认方案前补一遍数据溯源。 |

### 6.2 随包组件里最该先处理的：SenseVoiceSmall 的许可标注不一致

`NOTICE` 现在写 SenseVoiceSmall「Apache License 2.0」，依据是 GGUF 转换版的 HF 标签。
但**官方权重的许可是 FunASR Model Open Source License Agreement v1.1**（HF 官方卡 `license: other`），
且 SenseVoice README 明说「转换版可能写了不同条款，要单独核对」。

按 v1.1 文本，发行时我们**至少应**：注明出处与作者、保留模型名称（`SenseVoiceSmall`）。⚠️ **商用是否允许尚无许可方的最终确认**（见 §1 表；§3 字面是「仅作参考和学习」），而它是**随包默认 ASR**，所以这是本表里优先级最高的一项：建议由 jason 向许可方（FunASR/Alibaba）发问或换用许可清晰的 ASR 作随包默认。
**2026-11-11 拍板**：开源 / 直接分发版不换；App Store 版（视同商用）换成许可清晰的模型，候选调研见 [`asr-commercial-alternatives.md`](asr-commercial-alternatives.md)（首选 Qwen3-ASR-0.6B，Apache-2.0）。
**已改（FU-23，2026-11-11）**：`NOTICE` 已改写为「FunASR Model Open Source License Agreement v1.1（官方权重）；转换版 HF 页标注 Apache-2.0 但对不上」，附协议链接与 §2.2 使用要求，并标注商用待确认。

**⚠️ 待决（需 jason 处理）：SenseVoiceSmall 商用是否允许，未获许可方确认。** 在拿到确认前，本文不下「可商用」结论；备选是换用许可清晰的 ASR 作随包默认。

### 6.3 我们再分发「打过补丁的 speech-server」要承担什么义务

依据：Apache-2.0 §4（上游 `LICENSE` 即 Apache-2.0 标准文本），以及补丁头与 `AGENTEAR-PATCH.md` 现状。

| Apache-2.0 条款 | 要求 | 现状 |
|---|---|---|
| §4(a) 给接收方一份许可副本 | tarball / 安装目录里要有 Apache-2.0 全文（URL 链接**不等同于**随附副本） | ⚠️ **缺**：`AGENTEAR-PATCH.md` 只给了指向上游 LICENSE 的链接 |
| §4(b) 被改动的文件要带显著的「已修改」声明 | `Sources/AudioServer/AudioServer.swift` | ✅ 补丁头说明已在文件内加 “Modified by AgentEar” 注释；补丁文件本身也有说明 |
| §4(c) 保留源码形式分发里的版权/专利/商标/署名声明 | 不得删除上游的声明 | ✅ 补丁不删（只改一个文件的一个分支）；但**二进制**里是否带出上游版权信息取决于发布包，见下 |
| §4(d) 若上游带 NOTICE 文件，衍生作品要随附其可读内容 | 上游仓库根目录**没有 `NOTICE` 文件**（目录列表已看：有 `ThirdPartyNotices/`，没有 `NOTICE`）；`ThirdPartyNotices/` 里是 BreezeTTS、FireRedTTS3、MossTTSNano、MossVoiceGenerator、Qwen3TTSCodec 5 份，我们没用这些模型，但**二进制是否链入这些模型的代码，未核实** | ⚠️ tarball 里**没有**任何 NOTICE / 第三方声明 |
| §3 专利授权随许可传递；§6 不授予商标使用权（不得暗示上游背书） | 不能使用上游商标暗示官方 | ✅ release 正文写明「这是 AgentEar 自己的运行时分发渠道，不是上游 soniqo/speech-swift 的 release」 |
| 静态链接的依赖（mlx-swift、mlx-swift-lm、WhisperKit 等 MIT；其余 Apache-2.0） | 各自要求保留版权声明/许可文本；MIT 的核心义务就是「所有副本都带上版权与许可声明」 | ⚠️ **缺**：这些声明在我们托管的 tarball 里一条都没有 |
| 我们的补丁本身 | 采用什么许可？ | `patches/…agentear.patch` 在仓库内受仓库 Apache-2.0 约束；补丁头写明「不是我们自己的代码——这是对上游的修改」。✅ 兼容 |

**FU-23 已做（只改脚本，未重新打包、未发 release）**：`scripts/build-speech-runtime.sh` 现在会把 speech-swift 的 `LICENSE` / `ThirdPartyNotices/` 与 `.build/checkouts/*/` 下各依赖（含 mlx-swift 及其 vendored mlx）的 LICENSE / NOTICE 复制进 tarball 的 `LICENSES/`，`AGENTEAR-PATCH.md` 同步改写。**下次构建运行时生效**；已发布的 `speech-runtime-v0.0.28-agentear.1`（及待发的 `.2` 构建产物如用旧脚本）不带。NOTICE 已补 whisper.cpp / speech-swift / MLX / 泰语模型。**原行动项原文**：给 `scripts/build-speech-runtime.sh` 的打包步骤加一步，把上游 `LICENSE`（@v0.0.28）、mlx 与 Swift 依赖的许可文本放进 tarball（例如 `LICENSES/`），并发新的 `speech-runtime-<版本>`；同时补 `NOTICE` 里 whisper.cpp、speech-swift、MLX、泰语模型的条目。官方未打补丁的 `speech` CLI、`mlx.metallib` 等我们也在再分发，**同样要带声明**。

### 6.4 `assets/talk-voices/` 参考音频：来源与许可

- 2 条：`female_zh_01.wav`、`female_zh_02.wav`，每条旁有 `.json` 元数据。
- 来源：元数据 `source` 字段写「**VoxCPM2 自举（生成候选 → 按 F0 / 浊音帧 / RMS 挑）**」，生成脚本 `services/tts/make_voice.py`，**不传参考音频、不是真人录音**（该脚本文档与 `NOTICE` 均如此说明），入库提交 `12cb239`（#60）。
- 许可：生成器 VoxCPM2 为 Apache-2.0（卡明确可商用）；Apache-2.0 本身没有对模型**输出**设版权归属条款；但模型卡 Limitations 另写「严禁冒充、欺诈、虚假信息；AI 生成内容应明确标注」。这 2 条由模型自举生成、不是真人录音，**据我们所知**不涉及某个真人的录音版权/人格权，但 Apache-2.0 §7 排除了权属与不侵权保证，上游也未对输出作任何保证，所以这是**未被上游担保的推断**，不是已核实的结论。
- 两个保留：① 「没有任何版权/人格权问题」的断言取决于 VoxCPM2 零样本生成时**确实没有隐式克隆训练集里某个真实说话人**——这一点模型卡没有保证，也没有任何测量支持，**未核实**（既不能说概率低，也不能说有）；② 作者本机的 `男声`（作者本人的声音）**不在仓库里也不得入库**（CLAUDE.md 已规定），不在本表范围。

### 6.5 本次核实中发现的需要后续动作的文档问题（FU-23 已处理 1、2、3 中的脚本部分；仍待：泰语 release 补许可全文、Ornith 许可文本、SenseVoice 商用确认）

1. （已改）`NOTICE`：SenseVoiceSmall 许可标注应改为 FunASR Model License v1.1；缺 whisper.cpp、泰语模型、speech-swift、MLX 条目。
2. （已改）`docs/decisions/0007-realtime-voice-architecture.md`：VoiceChat 11B「OpenMDW 1.1 仅研究用途」与 OpenMDW-1.1 全文不符。
3. 泰语模型 release 与 speech 运行时 tarball 都没有随附许可全文。
4. Ornith 的许可文本要找上游确认。

### 6.6 未核实项汇总

FSMN-VAD 上游权重许可；`runtime/llama.cpp` 子目录的许可声明；GGUF 版 `apache-2.0` 标注的授权依据；泰语微调数据集（Common Voice v13 / Gowajee / SLSCU / Thai Elderly Speech）及基座 distil-large-v3 的许可链；`modelcontextprotocol/swift-sdk` 许可；speech-server 实际静态链接了 speech-swift 的哪些依赖/模型代码；Ornith 的许可文本与权利人；OmniVoice 许可；IndexTTS2 的 HF 侧是否另有声明及其协议是否会随版本变；FunctionGemma 的 Gemma 条款原文；PersonaPlex 的 NVIDIA Open Model License 原文及 moshiko 基座许可；VoiceChat 引用的附加组件条款；LocalVQE 搜索结果是否就是任务所指项目、其训练数据（CC BY 4.0）的署名传导；Smart Turn 训练数据许可；mlx-audio 所有者变更；VoxCPM2 零样本生成物是否隐含真实说话人。
