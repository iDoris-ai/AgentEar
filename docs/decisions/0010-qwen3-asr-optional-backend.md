# ADR-0010：Qwen3-ASR 作为可选后端——运行时从哪来、模型怎么下、要不要常驻

- 状态：**提议**（T3.5.6 第一步「调研」的产出，2026-09-26）。**本 ADR 不拍板**，
  §6 列出要 jason 定的点；在他定之前不写产品代码。
- 前提（已拍板，不重开）：Q4（jason 2026-09-26）——**默认仍是随包的 SenseVoice**；
  Qwen3-ASR 做成**可配置、选了才自动下载**的可选后端；**选择与下载入口在设置窗口**。
- 原始证据：[`docs/data/qwen3-asr-runtime-2026-09/RAW.md`](../data/qwen3-asr-runtime-2026-09/RAW.md)
  （下文凡是数字都出自那里；标「推断」的是没实测的）。
- 相关：ADR-0001（ASR 选型，里面写着「若将来出现 GGUF/纯 Rust 路径，应重新评估」）、
  [`benchmarks-asr-zh-en.md`](../benchmarks-asr-zh-en.md)（T3.5.1，准确率唯一可引用的数据）、
  `src/engine.rs`（现有 `speech_swift` 后端）、`src/download.rs`（泰语模型的按需下载）。

## 0. 一句话

**「不依赖 brew」这件事是能做的，而且三条路都能做**；真正要定的是**用哪个运行时**
和**要不要常驻**。现在的 `speech_swift` 后端每按一次键要多等约 1.8 s，
**原因是每次都重新加载模型——常驻之后实测降到 0.12–0.16 s**，
代价是这段时间一直占 2.4–3.0 GiB 内存。

## 1. 现状（为什么普通用户选不到）

`src/engine.rs` 的 `SpeechSwiftEngine` 直接 `Command::new("speech")`，
找不到就报「先装：brew install speech」；每一轮起一次 `speech transcribe -m 1.7B`。
模型由 speech 自己下到 `~/Library/Caches/qwen3-speech/`。
**没有菜单项、没有设置项、没有下载入口**，只能手改 `config.json` 的 `asr_backend`。

## 2. 调研结论

### 2.1 speech-swift 能不能脱离 brew（能）

| 问题 | 结论 | 依据 |
|---|---|---|
| 来源 / 许可证 | `soniqo/speech-swift`，**Apache-2.0**（仓库与 brew formula 两处一致） | RAW §1 |
| 有没有预编译包 | **有**：GitHub release 每版带 `speech-macos-arm64.tar.gz`（v0.0.28：99 MB，解包 364 MB；含 `speech`、`speech-server`、`mlx.metallib` 与 5 个 resource bundle） | RAW §1 |
| 依赖 | 只链系统 framework 与 `/usr/lib/swift`，**不引用任何 brew 路径** | RAW §1 `otool -L` |
| 从任意目录跑 | **能**：解包到临时目录直接跑，结果与 brew 版逐字相同，时延一致（1.77–1.82 s） | RAW §2 |
| 签名 / Gatekeeper | **ad-hoc 签名、无 Team ID，`spctl` 判 rejected**。本轮用 `gh` 下载，**文件上没有 `com.apple.quarantine`，所以能跑**。**推断**：AgentEar 用 curl 子进程下载同样不会打 quarantine 标（与泰语模型现状一致）；**若用户用浏览器手动下载再放进去，就会被 Gatekeeper 拦**。落地时应在安装步骤里检查并去掉 quarantine 属性，而且要有测试 | RAW §1 |
| 版本号 | **没有 `--version`**；版本只能靠「我们下的是哪个 release」自己记。本机 brew 是 0.0.26，上游已到 0.0.28（08-16 一天两版 0.0.24/0.0.25，之后 08-17、09-02、09-24 各一版，迭代很快） | RAW §1 |
| CLI 契约 | 0.0.28 仍有 `--engine / --language / --context`，`preflight` 的探测能过 | RAW §2 |

### 2.2 模型权重

| | 1.7B（现用） | 0.6B |
|---|---|---|
| speech 实际拉的仓库 | `aufklarer/Qwen3-ASR-1.7B-MLX-8bit` | `aufklarer/Qwen3-ASR-0.6B-MLX-4bit` |
| 体积（HF API 声明） | **2.47 GB**（主体 `model.safetensors` 2 463 307 541 B） | **0.71 GB** |
| 许可证 | apache-2.0（转换仓库 + 官方 `Qwen/Qwen3-ASR-1.7B` 都是） | 同左 |
| sha256 可校验 | HF 给出 LFS sha256，**本机缓存实测与之一致** | 同左（HF 有声明，本轮没算本地） |

- **能指定下载目录**：环境变量 **`QWEN3_ASR_CACHE_DIR`**，布局必须是
  `$DIR/qwen3-speech/models/<org>/<repo>/`，**模型目录不能是符号链接**（会报 Not a directory）。
  → 所以**我们可以自己下载、自己校验 sha，再按这个布局摆好，speech 直接用、不再联网**。
- `-m <本地目录>` **对 qwen3 引擎不管用**（被当成仓库名，重试 5 次、约 2 分钟后才报错）。
  help 文本与实际行为不符，**别走这条**。
- **断网可用**：缓存齐全时，在 `sandbox-exec` 拒绝全部出站 TCP 的情况下照样转写成功。
- **`HF_ENDPOINT` 镜像坑**：speech 的基址**写死** `https://huggingface.co`，
  不读 `HF_ENDPOINT`（`strings` 里没有这个名字），所以 CLAUDE.md 记的那个镜像坑**对它不适用**；
  反过来，**如果用户所在网络只能走镜像，speech 自己的下载器就下不动**——这是
  「由我们自己下载」的又一个理由。
- 首次下载时长：**本轮没有可靠计时**（下 2.5 GB GGUF 时计时输出丢了），只知道在这台机器上是分钟级；设置窗口里必须给进度，不能给预估时长。

### 2.3 三条运行时路线

| | (a) speech-swift | (b) Python MLX（mlx-audio） | (c) llama.cpp + GGUF |
|---|---|---|---|
| 运行时体积 | 99 MB 下载 / 364 MB 解包 | 复用 `~/.agentear/llm/venv`（**只有装过对话边车的人才有**，那个 venv 8.2 GB） | **12 MB 下载 / 30 MB 解包** |
| 1.7B 模型体积 | 2.47 GB（MLX 8bit） | 本机没有 1.7B（`mlx-community/Qwen3-ASR-1.7B-8bit` 存在于 HF，未下） | 2.52 GB（Q8_0 + mmproj） |
| 每轮时延（逐次起进程） | **1.77–1.82 s**（3.5 s 音频，n=6） | — | — |
| 每轮时延（常驻，热） | **0.156–0.160 s**（`speech-server`，n≥5；首个请求 1.72 s） | 0.155–0.281 s（**0.6B**，不可横比） | **0.119–0.123 s**（`llama-server`，n≥5；首个 0.20–0.61 s） |
| 常驻内存 | **2.43 GiB**（只载 1.7B） | 未测 | **2.95 GiB（`-c 4096`）/ 30.5 GiB（默认上下文）⚠️** |
| 准确率 | **有 n=60×3 组的实测**（T3.5.1 就是这个运行时 + 这份权重） | 没测 | **没测**；单条样本上与 (a) 输出不同（「钦脉」vs「清迈」），说明不了谁好 |
| 许可证 | Apache-2.0 | mlx-audio MIT（`gh api repos/Blaizzy/mlx-audio`） | llama.cpp MIT（`gh api repos/ggml-org/llama.cpp`）；GGUF 模型卡未声明许可证，base 是 apache-2.0 |
| 维护负担 | 上游约一个多月出了 5 版；CLI 参数有变动风险（已有契约探测兜底）；有 server 模式但 48 kHz 输入与 CLI 结果不同 | Python 边车，版本锁、venv 体积大；与 TTS 共用 venv 会互相牵连 | 与随包的 `llama-funasr-sensevoice` **同一家族**；需钉死 `-c`、自己剥 `language …<asr_text>` 前缀 |
| 与「默认随包、Qwen3 按需下」的契合度 | 高：一个 tarball + 一份权重 | 低：普通用户没有这个 venv，等于先装一套 8 GB 的 Python 环境 | 高：体积最小，形态和现有 ASR 最像 |

**(b) 不推荐**：它要求用户先有 MLX 的 Python 环境，而输入法模式的普通用户根本没装对话边车。
「为了一个可选 ASR 再装 8 GB」违背按需下载的本意。

**(c) 值得认真对待**：ADR-0001 当初把 Qwen3-ASR 刷掉的理由是「需要 Python + MLX 运行时」，
并写明「若将来出现 GGUF/纯 Rust 路径，应重新评估」——**这条路径现在确实出现了**
（`ggml-org` 官方发布，模型卡给的就是 `llama-server` 用法）。
但本轮**只证明了它能跑、快、内存可控**，**没有任何准确率数据**，所以不能直接选它。

### 2.4 「每轮 +2 s」能不能消掉（能，代价是常驻内存）

T3.5.1 测到的 +2 s，**绝大部分是每轮重新加载模型**：同一条 3.5 s 音频，
逐次起 CLI 1.77–1.82 s，常驻 server 热态 0.156–0.160 s（RAW §2、§4）。
所以「常驻」这一个决定就把差距从约 1.6 s 压到零点几秒。

代价是常驻内存：(a) 2.43 GiB、(c) 2.95 GiB——**都在高资源档 ≤4 GiB 以内**，
但会和对话模式的两个边车（LLM + TTS）**叠加**。按空闲超时自动卸载可以缓解，
那是实现细节，放到实现阶段测。

## 3. 设置窗口里的下载需要什么后端能力

对照 `src/download.rs`（泰语模型）逐项看：

| 能力 | download.rs 现在有没有 | 要做的事 |
|---|---|---|
| 进度 | 有（`State::Downloading(pct)`，0.5 s 轮询） | 改成**按字节聚合多个文件**的进度 |
| 断点续传 | 有（curl `-C -`，`.part` 保留） | 直接复用 |
| sha256 校验 | 有（全量 64 位，不匹配就删掉重来） | 直接复用；sha 写死在常量里（与 THAI 同一做法） |
| 失败原因分类 | 有（`Fail::{Network, Checksum, Disk, Busy, Io}`） | 直接复用 |
| 磁盘空间预检 | 有 | 改成按「所有文件之和 + 解包后体积」算 |
| 跨进程互斥 | 有（`.lock` + flock） | 复用 |
| 加载冒烟再算装好 | 有（`Verifying` 相 + `.installed` 清单） | 冒烟改成「用下好的运行时转写一小段内置音频」 |
| **多文件 / 解包** | **没有**：一个 `ModelSpec` = 一个文件 | 需要「一组文件 + 一个 tarball 解包」的清单 |
| **多个模型各自的状态** | **没有**：`PHASE/PCT/FAIL` 是全局 static，同一时刻只能表达一个下载 | 改成按 spec 分开（否则泰语和 Qwen3 同时存在时状态会串） |
| **取消** | **没有** | 设置窗口里需要「取消」按钮：杀 curl、保留 `.part` |
| 去掉 quarantine 属性 | 没有（泰语模型是数据文件，不需要） | 运行时是可执行文件，安装时检查并去掉 |
| 下载源 | GitHub release（我们自己的 `models-th-v1`） | 见 §6 第 4 点：直连 HF 还是转存到我们的 release |

结论：**下载协议本身（续传、校验、冒烟、清单）能直接复用**；
要补的是**多文件清单、按模型分开的状态、取消、解包与 quarantine 处理**。

## 4. 推荐方案（供拍板，不是决定）

1. **运行时先用 (a) speech-swift 的预编译 release**，钉死版本（目前 v0.0.28）与 tarball 的 sha256，
   解包到 `~/.agentear/runtimes/speech-<版本>/`。理由：**准确率数据只有这条路有**，
   而且和现有 `speech_swift` 后端是同一个程序，改动最小。
2. **模型由我们自己下**（复用 download.rs 的协议，按 HF 声明的 sha256 校验），
   按 `QWEN3_ASR_CACHE_DIR` 要求的布局摆进 `~/.agentear/models/qwen3-asr/`，
   启动 speech 时带上这个环境变量。这样 speech 自己的下载器**永远不会被触发**
   （包括它会绕开 `HF_ENDPOINT` 这一点），断网也能用。
3. **常驻**：用 `speech-server`，按对话边车的规矩「连接优先、拉起兜底」，
   **显式传模型名**（不传会静默下载一个 611 MB 的英文模型，RAW §4），并加空闲超时卸载。
   常驻前要补测：**server 路径的准确率要和 T3.5.1 的 CLI 路径对上**（重跑那 60×3 组即可），
   因为已经看到 48 kHz 输入下两者输出不同。
4. **只提供 1.7B**：T3.5.1 的结论是 0.6B「在三组里都没有显著赢过 SenseVoice」，
   提供它只会让用户多下 0.7 GB 换不来东西。
5. **(c) GGUF 路线单独开一个评测 task**：用 T3.5.1 同一套 60×3 组语料打分。
   如果它不比 (a) 差，就换过去（体积小一个数量级、形态与随包 ASR 一致、MIT）。
   这一步同时回答 ADR-0001 里那句「出现 GGUF 路径应重新评估」。

## 5. 明确不做的

- 不改默认引擎（Q4 已定）。
- 不走 brew（这正是本 task 要去掉的依赖）。
- 不走 Python 路线（§2.3）。
- 不在设置窗口里提供「自定义模型仓库 / 任意 URL」——那是把下载源交给用户输入，校验就无从谈起。

## 6. 要 jason 拍板的点

1. **运行时**：先上 (a) speech-swift，还是**先做 (c) GGUF 的准确率评测**再决定？
   前者更快上线，后者可能省一个 364 MB 的运行时并关掉 ADR-0001 的遗留问题。
2. **常驻还是逐次**：常驻每轮快约 1.6 s，但选了 Qwen3 期间一直占 2.4–3.0 GiB；
   逐次不占内存，每轮多等约 1.8 s。（推荐常驻 + 空闲超时卸载。）
3. **只给 1.7B，还是 1.7B / 0.6B 都给**？（推荐只给 1.7B。）
4. **下载源**：直连 HF（第三方转换仓库 `aufklarer/*`，随时可能改动或下架），
   还是像泰语模型那样**转存到我们自己的 GitHub release**（固定、可审计；
   Apache-2.0 允许再分发——这条是**推断**，应并入 T3.5.3 许可证表核对）。
5. **speech 版本的升级策略**：上游约一个多月出了 5 版。钉死一个版本、人工验证后再升，
   还是跟随最新？（推荐钉死：CLI 协议变过，探测只能挡住一部分。）
6. **本轮调研在你机器上留下的缓存**：speech-server 的默认路由当场下载了一个用不上的
   `Parakeet-TDT-v3-CoreML-INT8-30s`（611 MB，在 `~/Library/Caches/qwen3-speech/models/aufklarer/`）。
   删不删由你定。
