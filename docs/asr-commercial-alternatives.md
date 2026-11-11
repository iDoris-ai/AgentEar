# App Store 版的可商用 ASR 替代（调研，2026-11-11）

> **范围（jason 2026-11-11 拍板）**：上 Mac App Store 视同商用，随包 ASR 必须是**明确允许商用**的模型；
> **开源 / 直接分发版不换**，仍随包 SenseVoiceSmall（许可问题见 [`licenses.md`](licenses.md) §6.2）。
> 本文是网页调研（HF 模型卡元数据 + 一手页面），**不是法律意见**；HF 的 `license` 字段是卡片声明，
> 多数仓库没有单独的 LICENSE 文件。训练数据条款没有逐一核。**所有质量数字都是上游自报，本项目未在自己的语料上复测**。

## 结论

| | 模型 | 许可 | 为什么 | 代价 |
|---|---|---|---|---|
| **首选** | `Qwen/Qwen3-ASR-0.6B`（MLX 4bit ≈ 708 MB） | Apache-2.0（权重卡 + GitHub 代码） | 中英泰都覆盖，上游指标显著优于 Whisper；**运行时（speech-swift + 自编补丁 speech-server）与下载/校验管线项目里已有**（v0.23.0 起的可选后端） | 冷启动与常驻内存差于 SenseVoice（逐次 1.8–2.5 s vs 0.2 s）；MAS 必须**随包**运行时与权重（审核指南 2.5.2 不许下载可执行代码），包体 +~800 MB；内嵌 speech-server 在沙箱里的签名/继承**未验证**；`sandbox-exec` 禁网那层在沙箱里大概率不能嵌套（见 `app-store.md` §2.4） |
| **备选** | Whisper large-v3-turbo q5_0（574 MB）/ small q5_1（190 MB），经 whisper.cpp | MIT | 许可最干净，运行时项目里本来就随包（泰语路径） | 中文明显弱于 Qwen3-ASR（上游 WenetSpeech 9.86/19.11 vs 5.97/6.88），长录音有幻觉风险 |
| **VAD** | Silero VAD（ONNX ≈ 2 MB） | MIT | 去掉 FSMN-VAD，避开 FunASR 体系的许可疑问 | 要新接一个 ONNX 推理（或只在 Qwen3 / whisper 路径用它们自带的分段） |

**建议的下一步**（未排期，等 MAS 路线定）：① 在 `docs/benchmarks-asr-zh-en.md` 的同一批语料上，把 Qwen3-ASR-0.6B 作为**默认**跑一遍准确率（目前只量过速度与内存）；② App Store 原型里验证随包 speech-server 能在沙箱里被拉起；③ 若①不达标，退回 whisper turbo。

## 调研原文

## 任务一：ASR 候选

### 现状基线：SenseVoiceSmall 的问题已确认
- HF 元数据：license=other，license_name=model-license，指向 FunASR 仓库 MODEL_LICENSE。
- MODEL_LICENSE v1.1 原文要点（已核实）：§2.1「free to use, copy, modify, and share」；§2.2 必须署名并保留模型名；§3「provided for reference and learning purposes only」；全文**不提商用**。结论：商用授权不明，App Store 不宜依赖。
- FunASR 仓库 README 明说：工具包代码 MIT；「Pretrained model weights are licensed separately. Check the license shown on each model card」；链接到 MODEL_LICENSE 的模型适用该协议。

### 候选总表

| 候选 | HF repo id | 许可证（卡片声明）| 商用 | 语言 | 体积 | 公开指标 | 非 Python 运行时 |
|---|---|---|---|---|---|---|---|
| **Qwen3-ASR-0.6B** | Qwen/Qwen3-ASR-0.6B | **apache-2.0**（front matter 已核实；仓库无 LICENSE 文件；GitHub 代码 Apache-2.0）| 是，无 MAU/署名附加限制 | 30 种语言 + 22 种中文方言，含中/英/**泰**；中英混说官方未单列 | 0.94B 参数(BF16)，safetensors 1.88 GB；MLX 4bit 约 708 MB | 官方卡：WER LibriSpeech clean/other 2.11/4.55；WenetSpeech net/meeting 5.97/6.88；AISHELL-2 3.15 | speech-swift/MLX（项目已有）；CoreML（aufklarer/Qwen3-ASR-CoreML，INT8 约 180 MB，第三方，自报 WER 3.02%）；社区 GGUF（CrispASR / cstr/qwen3-asr-0.6b-GGUF，第三方）。官方只给 Python/vLLM |
| **Qwen3-ASR-1.7B** | Qwen/Qwen3-ASR-1.7B | apache-2.0（页面标签；无正文）| 是 | 同上 | 约 2B 参数 | LibriSpeech 1.63/3.38；WenetSpeech 4.97/5.88；AISHELL-2 2.71（同卡，对比 Whisper-large-v3：WenetSpeech 9.86/19.11，AISHELL-2 5.06）| 同上，speech-swift 实测 5bit 峰值约 1.9 GB |
| Whisper large-v3-turbo | openai/whisper-large-v3-turbo | **MIT** | 是 | ~100 种，含中英泰 | 809M 参数；ggml q5_0 574 MB，q8_0 874 MB（ggerganov/whisper.cpp，MIT）| 中文差于 Qwen3/FireRed；英文 LibriSpeech 约 1.7（soniqo 自测）| whisper.cpp GGML / CoreML（项目已有）|
| Whisper small / base | openai/whisper-small 等 | MIT | 是 | 多语 | small ggml q5_1 190 MB，q8_0 264 MB；base q5_1 60 MB | 中文明显弱 | whisper.cpp（已有）|
| distil-whisper (distil-large-v3.5) | distil-whisper/distil-large-v3.5 | MIT | 是 | **仅英文** | 756M 参数 | 英文好 | whisper.cpp 有转换，但中文用不了 |
| FireRedASR2-AED | FireRedTeam/FireRedASR2-AED | apache-2.0（卡片；GitHub 代码 Apache-2.0，权重未另列许可）| 是（卡片层面）| 中文、方言、英文、中英混说（卡片明说 code-switching）| model.pth.tar **4.73 GB**（过大）| 4 个中文公开集平均 CER 3.05%；README 表：AISHELL-1 0.57，AISHELL-2 2.51，WS-net 4.57，WS-meeting 4.53；无 LibriSpeech | 官方只有 Python；TensorRT-LLM/Triton（NVIDIA）；未见 ONNX/GGUF/MLX。不适合随包 |
| FireRedASR-AED-L（旧）| FireRedTeam/FireRedASR-AED-L | apache-2.0 | 是 | 中英 | 4.68 GB | 论文口径 | Python |
| FireRedVAD | FireRedTeam/FireRedVAD | apache-2.0 | 是 | 100+ 语言 | 0.6M 参数 | — | Python |
| Paraformer-zh（FunASR）| funasr/paraformer-zh | HF 卡片 **apache-2.0**；ModelScope 的 seaco-paraformer 页面也写 Apache License 2.0（LicenseName/Link 为空，无商用说明）| 卡片层面是；但与 SenseVoice 同属 FunASR 体系，**HF 与 MODEL_LICENSE 的口径需向作者确认** | 中文为主，含英文 | 约 880 MB 仓库（含多文件）| 无标点（项目已有结论：Paraformer 出局）| sherpa-onnx ONNX 有 |
| FSMN-VAD | funasr/fsmn-vad | HF 卡片 **apache-2.0**（无 license_name/link）| 卡片层面是 | 语言无关 | 约 1.6 MiB | — | 项目已用；**与 SenseVoice 同发行体系，建议同时确认或直接换 Silero** |
| Fun-ASR-Nano-2512 | FunAudioLLM/Fun-ASR-Nano-2512 | **apache-2.0**（卡片已核实，无单独 license_name）| 卡片层面是 | 中/英/日 + 方言；**泰语只在 Fun-ASR-MLT-Nano（31 语）**| 800M 参数 | WER AISHELL-1 1.80，LibriSpeech clean/other 1.76/4.33 | 卡片称有 llama.cpp/GGUF；项目 ADR-0001 已因内存/冷启动淘汰过旧 Nano |
| Dolphin | DataoceanAI/dolphin-base/small | **Apache-2.0**（卡片原文：code and model weights are released under Apache 2.0）| 是 | 40 种东方语言 + 22 种中文方言，**英文未列** | base 140M；small 372M；medium/large 未公开 | 只给平均 WER（base 33.3，small 25.2），无 CER | 仅 Python 包，无 ONNX。不适合 |
| Moonshine | UsefulSensors/moonshine-base | MIT（英文 base）；仓库 README：新模型默认 MIT，**例外是旧版非英文非流式模型，属非商用 Moonshine Community License**（例外清单在 LICENSE 文件，我没取到）| 英文 MIT 可；非英文要逐个核 | 英文；README 列 Mandarin 等 10 种 STT | base 61.5M 参数 | 无中文指标 | 自有 C++/ONNX 运行时。**moonshine-base-zh 卡片为 license:other**，不要用 |
| NVIDIA Parakeet-tdt-0.6b-v3 | nvidia/parakeet-tdt-0.6b-v3 | **CC-BY-4.0**（可商用，需署名）| 是 | 25 种欧洲语言，**无中文、无泰语** | 627M | — | 有 GGUF 标签 |
| NVIDIA Canary-1b-v2 | nvidia/canary-1b-v2 | CC-BY-4.0 | 是 | 同样 25 种欧洲语言，**无中文** | 979M | — | NeMo |
| Kimi-Audio-7B | moonshotai/Kimi-Audio-7B-Instruct | MIT（卡片；无 LICENSE 文件）| 是 | 中/英 | **9.77B 参数** | — | Python。太大，出局 |
| sherpa-onnx streaming-zipformer 中英 | csukuangfj/sherpa-onnx-streaming-zipformer-bilingual-zh-en-2023-02-20 | **apache-2.0**（卡片）；训练数据卡片未写 | 卡片层面是；**训练语料条款未核实** | 中英双语流式 | 仓库共 651 MB（含 fp32+int8 全套），仅 int8 部分明显更小 | 卡片无 CER/WER | **sherpa-onnx ONNX，无需 Python**。新增运行时 |

### VAD 替代
- **Silero VAD**：仓库 LICENSE 原文「MIT License, Copyright (c) 2020-present Silero Team」（已核实；该 LICENSE 覆盖仓库含模型文件的说法为推断，权重未单独声明）。有 ONNX，sherpa-onnx / 多数 Rust crate 可直接跑，约 2 MB。
- FireRedVAD（apache-2.0，0.6M 参数）也可，但只有 Python/PyTorch。
- 建议：**换 Silero VAD，去掉 FSMN-VAD**，避免任何 FunASR 体系的许可疑问。

### 许可证口径对照（代码 vs 权重）
- Qwen3-ASR：代码 Apache-2.0（GitHub 侧边栏），权重卡片 apache-2.0；两者一致。
- FunASR：代码 MIT；权重逐模型。SenseVoice = model-license（商用不明）；paraformer-zh 与 fsmn-vad 在 HF funasr 组织下标 apache-2.0，但与 FunASR README「以各模型卡为准」的说法并存，建议给作者发邮件书面确认再用。
- FireRed：代码 Apache-2.0，权重卡片 apache-2.0。
- Moonshine：代码 MIT，权重分语言/版本，存在非商用例外。

### 推荐

**首选：Qwen3-ASR-0.6B（Apache-2.0）**
- 许可：权重卡片与代码均 Apache-2.0，无 MAU 门槛、无署名之外的限制（Apache-2.0 本身要求保留 NOTICE/许可文本）。
- 质量：官方卡 WenetSpeech 5.97/6.88、AISHELL-2 3.15、LibriSpeech 2.11/4.55，均显著优于 Whisper large-v3（WenetSpeech 9.86/19.11）；1.7B 更好但体积/内存翻倍。支持泰语，可顺带**取代泰语 whisper 随包路径**（泰语质量项目侧仍需实测，官方未给泰语指标）。
- 体积：MLX 4bit 约 708 MB（CoreML INT8 约 180 MB，第三方）。App Store 随包可行，也可按需下载（但需 App Review 接受；随包最稳）。内存：speech-swift 实测 0.6B 5bit 峰值约 1.03 GB。
- 运行时：项目已经有 speech-swift/MLX 与自编译的 speech-server 补丁（AGENTEAR_MLX_CACHE_MB），也已有 Qwen3 的下载/校验管线，改动最小。
- 代价：冷启动、常驻内存、逐次路径 1.8–2.5 s（项目 FU 实测）比 SenseVoice 的 0.2 s 差；这是体验退化，不是许可问题。MAS 沙箱里自编译的 speech-server 子进程、sandbox-exec 嵌套、App Store 对内嵌可执行文件的签名要求需要另行核实（本次未查）。

**备选：Whisper large-v3-turbo q5_0（MIT，574 MB）或 small q5_1（MIT，190 MB）经 whisper.cpp**
- 许可最干净（MIT，OpenAI 与 ggml 转换仓库均 MIT），运行时项目里本来就有、无需新依赖。
- 中文质量明显差于 Qwen3-ASR（WenetSpeech 9.86/19.11 对 5.97/6.88），长录音有幻觉风险。作为「保底、零新依赖」可以。small 体积最小但中文更弱。

**不推荐**：FireRedASR2（中文最强，但权重 4.7 GB、只有 Python/TensorRT）；Dolphin（无英文、无非 Python 运行时）；Kimi-Audio（9.8B）；Parakeet/Canary（无中文）；Moonshine（中文模型许可不清，zh 卡为 license:other）；Fun-ASR-Nano（Apache-2.0 且指标好，但项目此前因内存和冷启动淘汰，如重评需要重测）。
**可观察**：sherpa-onnx 中英 zipformer（Apache-2.0 流式、纯 ONNX），但训练语料条款与精度未核，作为将来「非 LLM、低内存」方案的备选。

## 任务二：Mac App Store 沙箱能力

### 1) listen-only CGEventTap 监听 flagsChanged（右 Command）
- 结论：**可行，走「输入监控」(ListenEvent)，不是辅助功能；Apple DTS 明确确认沙箱内可用。**
- 原文（Apple DTS Quinn，https://developer.apple.com/forums/thread/780626，已核实）：「It _is_ possible for a sandboxed app to use the Input Monitoring privilege. I regularly test this. Specifically, I'm using the `CGEventTap` API.」「It's not possible for a sandboxed app to use the Accessibility privilege」。
- 原文（Quinn，https://developer.apple.com/forums/thread/789896，2025-06）：「If so, then `CGEventTap` should work for you. The user will need to grant your app the System Settings > Privacy & Security > Input Monitoring privilege, but once they do it'll be able to use `CGEventTap` to monitor keyboard events.」及「you need to use an API that relies on ... Input Monitoring rather than ... Accessibility. That means `CGEventTap` rather than the `NSEvent` monitor.」
- 原文（Quinn，https://developer.apple.com/forums/thread/820594，2026-03）：`ListenEvent` 映射到 Input Monitoring，且「The APIs gated by the other two privileges [PostEvent, ListenEvent] _are_ compatible with the App Sandbox.」「Weirdly, ... `NSEvent.addGlobalMonitorForEvents` ... requires the `Accessibility` privilege.」
- 额外 entitlement：以上 Apple 原文**均未提需要额外 entitlement**；dev.to 作者自述也无需（二手，https://dev.to/quicopy/shipping-global-keyboard-shortcuts-on-macos-sandbox-the-part-apple-doesnt-document-57no）。App Sandbox 的常规 entitlement（`com.apple.security.app-sandbox`、麦克风 `com.apple.security.device.audio-input`）照旧。
- 用 listenOnly 创建 tap 才会触发输入监控而非辅助功能：该点仅有论坛用户 skucas 的实测（https://developer.apple.com/forums/thread/122492，非 Apple 官方，标「用户经验」）。
- CGPreflightListenEventAccess / CGRequestListenEventAccess：Apple 帖子里提到的是对应的 PostEvent 版本（见下）；ListenEvent 版本是同族 API，本次**未找到 Apple 员工在沙箱语境对其正文的说明，标「未核实」**。
- 审核：Quinn 反复声明他不代表 App Review，无法给出审核结论。我没找到「仅因监听键盘被拒」的案例。监听属于 DTS 认可的技术路径，但**审核接受度不能保证**。
- 已知坑（用户反馈，未解决）：授予辅助功能后输入监控列表可能不显示应用（https://developer.apple.com/forums/thread/828052）。

### 2) 沙箱应用用 CGEventPost 合成 ⌘V 自动粘贴
- 技术上：**可行，需要 PostEvent 权限（系统设置里显示在「辅助功能」面板），不是完整辅助功能权限。**
  - Quinn，https://developer.apple.com/forums/thread/789896（2025-06/10）：「You can post events using `CGEvent.post(…)`. That uses its own privilege, one that's also compatible with App Sandox. ... While this privilege shows up in the UI as ... Accessibility, it doesn't give you complete accessibility access.」「If you try to post a `CGEvent`, the system will present the TCC alert for you. Alternatively ... call `CGPreflightPostEventAccess` and `CGRequestPostEventAccess`.」
  - Quinn，https://developer.apple.com/forums/thread/708652：「Have you tried using `CGRequestPostEventAccess`? And its companion `CGPreflightPostEventAccess`?」「The only folks who can give you a definitive answer about what is or isn't allowed in the App Store is App Review.」
  - Quinn，https://developer.apple.com/forums/thread/820594：「The APIs gated by the other two privileges _are_ compatible with the App Sandbox.」
- 沙箱应用**不能**获得完整辅助功能授权（Quinn，thread 780626：「not possible for a sandboxed app to use the Accessibility privilege」，除非 temporary exception entitlement）。因此 AXUIElement 直接插字不可用。
- **审核层面风险很高（关键）**：
  - https://developer.apple.com/forums/thread/820594：沙箱剪贴板管理器因 CGEvent.post 发 ⌘V 被以 Guideline 2.4.5 拒两次（审核员说法由发帖人转述，非原文）；Apple 员工只答技术侧，把审核问题推给 App Review；之后两位开发者回帖称同样被拒、无进展。
  - WhisperPad（听写应用）2026-04/05 因 Accessibility API 非无障碍用途被 2.4.5 拒两次，之后出了不自动粘贴的精简版上架，完整版走 Paddle 直销（https://mjtsai.com/blog/2026/06/03/whisperpad-rejected-from-the-mac-app-store/，二手汇总，已核实该页内容）。同页还列了 Iris 被拒；THOHT、Type Mac 曾被批准，但 Type Mac 之后被告知不能更新（审核不一致）。
  - Spokenly 官方文档：Mac App Store 版「can't paste text automatically」，「App Store rules don't allow it to use Accessibility」，用户需手动按 ⌘V（https://spokenly.app/docs/mac-app-store）。
  - 一篇 dev.to 自述开发者称其 MAS 应用用 `com.apple.security.automation.apple-events` + System Events AppleScript 发 ⌘V 且 Accessibility 代码被审核移除后上架（二手，未独立核实；同文称沙箱下 CGEvent.post 被禁，与 Quinn 说法矛盾，应以 Apple 说法为准）；另有开发者报告沙箱构建里 CGEvent.post「静默无效」（thread 724603 等，未逐条核）。
  - Apple 员工对听写类应用插入焦点输入框的说明（https://developer.apple.com/forums/thread/848711，2026-09）：发帖人在沙箱原型里「write a marker to the general pasteboard and post Command–V with CGEvent.post after the user grants event-posting access」，TextEdit 中成功；Quinn 只答：无支持的沙箱兼容方式得知目标是否有可编辑焦点或粘贴是否被接受（「No.」），可用 `IsSecureEventInputEnabled` 检测安全输入；**没有就审核是否允许表态**。
- 结论：技术可行但**没有任何 Apple 员工公开说「App Store 允许」**，已有多起 2.4.5 拒绝（含听写应用）。可靠做法：MAS 版只做「写剪贴板 + 提示用户按 ⌘V」（上架听写/剪贴板应用的实际常态）；自动粘贴作为可能被拒的可选功能，需先开 App Review 申诉/预审确认。

### 3) RegisterEventHotKey 在沙箱中的可用性
- **没有 Apple 员工的正文确认**（未核实到官方原文）。已有的证据：
  - dev.to 作者自述 MAS 剪贴板应用用它做 ⌘⇧V 全局热键，「No Input Monitoring prompt, no Accessibility prompt」（二手：https://dev.to/eduardo_revillavaquero_0/i-shipped-a-clipboard-manager-inside-the-macos-app-store-sandbox-heres-what-works-and-what-5a30）。
  - 论坛帖 https://developer.apple.com/forums/thread/790771：沙箱 MAS 应用用 HotKey（封装 RegisterEventHotKey）库，用户称在商店版里工作正常（无 Apple 回复）。
  - 2012 年 Macworld 转述：用官方支持的 API 注册系统级热键的应用可留在 MAS（二手，旧）。
  - Sequoia 变更（Apple Frameworks Engineer，https://developer.apple.com/forums/thread/763878，原文）：热键注册要求至少一个非 Shift/Option 修饰键；15.2 beta 2 后又放宽为允许仅 Option（或 Option-Shift）。用户称此变更只影响沙箱应用（用户声明，非 Apple 确认）。
- **对本项目的关键含义**：RegisterEventHotKey 只能注册「修饰键 + 普通键」组合，**无法表达「右 Command 单击/双击」**（那是 flagsChanged 事件）。右 Command 手势在 MAS 版仍须走 listen-only CGEventTap + 输入监控（或改为组合键热键，免权限）。

## 对 AgentEar MAS 版的落地建议（摘要）
1. ASR：换 Qwen3-ASR-0.6B（MLX 4bit，Apache-2.0），保留 whisper.cpp（MIT）作低资源/泰语保底；VAD 换 Silero（MIT）。
2. 输入：listen-only CGEventTap + 输入监控，DTS 明确认可；同时提供组合键（RegisterEventHotKey）作免权限备选。
3. 输出：MAS 版默认只写剪贴板并提示 ⌘V；自动粘贴单独评估，被 2.4.5 拒的概率不低。
4. 未核实/待办：CGPreflight/RequestListenEventAccess 的 Apple 官方沙箱说明；Qwen3-ASR 训练数据与权重之外的限制；内嵌 speech-server 子进程在 MAS 的签名/沙箱继承可行性；FunASR 体系 apache-2.0 标注与 MODEL_LICENSE 的关系（如要保留 paraformer/fsmn-vad）。
