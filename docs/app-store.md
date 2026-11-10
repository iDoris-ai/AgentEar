# Mac App Store 分发：可行性审计（T7.1，2026-11-10）

> 结论先行：**现在这份 AgentEar 不能原样上 Mac App Store。** 有 4 个硬阻塞项，
> 其中 2 个需要产品取舍（右 Command 热键与自动上屏），不是改几行代码能解决的。
> 但「输入法模式 + 随包 SenseVoice + 组合键 + 剪贴板」这个子集**有望**做成 MAS 版，
> 需要先做一次最小沙箱原型来验证标「未核实」的几项。
> 直接分发版（Developer ID + 公证）保持现状，本分支不改变它的任何行为。

**核实等级**：✅ 已核实（引了 Apple 官方原文）｜📖 读代码得出（本仓库事实）｜❓ 未核实
（凭一般知识或第三方经验，写代码之前必须用真实沙箱包验证）。
本次抓取 Apple 文档站时，开发者文档页多数只返回了标题、没有正文，所以
「entitlement 细节」类结论大多只能标 ❓；审核指南正文抓到了。

## 1. 本分支已经做的

| 项 | 说明 |
|---|---|
| `assets/entitlements/AppStore.entitlements` | app-sandbox、麦克风（sandbox 的 `device.microphone` + Hardened Runtime 的 `device.audio-input` 两把锁）、网络 client/server |
| `scripts/bundle.sh` | `AGENTEAR_DISTRIBUTION=app-store` 时用该 entitlements + `--options runtime` 签名；**不设该变量时命令与之前完全相同** |
| `resolve_data_root` | 沙箱下数据落在容器里。比起步工作多处理了一点：沙箱进程的 `HOME` 通常**本身就是**容器的 `Data` 目录，再拼一次 `Library/Containers/<id>/Data` 会嵌套（❓ HOME 行为未在真实沙箱里核实，所以两种情形都有单测） |
| 测试 | `sandboxed_data_root_stays_in_the_container`、`app_store_entitlements_enable_sandbox_and_microphone` |

⚠️ 用该变量打出来的包**还不能上传审核**：它仍带着下面第 3 节的全部阻塞项。
当前只是把「怎么签」备好，方便做沙箱原型。

## 2. 逐项审计

### 2.1 右 Command 热键（`CGEventTap`，`src/hotkey.rs`）— 🔴 硬阻塞（需产品取舍）
- 📖 现状：监听 flagsChanged，且为判定「轻点」还订阅 KeyDown（只读一个 bool）。需要「辅助功能」权限（`AXIsProcessTrustedWithOptions`）。
- ❓ 沙箱应用是否允许拿到辅助功能授权、是否允许建 `CGEventTap`：一般认知是
  **沙箱应用不能使用辅助功能 API 控制其它应用**，listen-only 的 tap 可能改走
  「输入监控」权限（`CGRequestListenEventAccess`）。**必须用真实沙箱包验证**，
  不要凭此下结论。即便能建，「键盘记录器形状」的能力在审核中也是风险点。
- 替代：`hotkey.rs` 头部注释已记录——**组合键走 Carbon `RegisterEventHotKey`，不需要辅助功能权限**，
  ❓ 沙箱兼容性未核实（社区经验是可用，Apple 无明文）。MAS 版建议只提供组合键触发，
  **丢掉「单击/双击右 Command」手势**。双击切模式的手势因此在 MAS 版不存在，需另给入口（菜单已有）。

### 2.2 自动上屏（`src/paste.rs`，`CGEventPost` 发 ⌘V）— 🔴 硬阻塞
- 📖 向前台应用合成按键，依赖辅助功能。❓ 沙箱下同样大概率不可用。
- 替代：只写剪贴板（`NSPasteboard` 沙箱内可用 ❓未逐条核实，但属普通能力），由用户自己按 ⌘V。
  这等于回到 M1「文字进剪贴板」。需要 jason 决定能不能接受。

### 2.3 子进程 — 🟠 可做但工作量大
- 📖 现状会起：`llama-funasr-sensevoice`（随包 vendor）、`whisper-cli`（随包）、
  `/usr/bin/curl`（12 处）、`afplay`、`say`、`afconvert`、`open`、`plutil`、`tar`、`xattr`、`lsof`、`ps`、`launchctl`、`/bin/sh`。
- ❓ 沙箱子进程默认继承父沙箱；Apple 文档对随包 helper 的做法是：helper 放进 bundle，用同一 Team 签名，
  并带 `com.apple.security.app-sandbox` + `com.apple.security.inherit`（本次抓取该页无正文，来自一般知识）。
  `--deep` 签名不会给嵌套二进制各自带 entitlements，需要**逐个**单独签再签外层。
- ❓ 系统自带的 `/usr/bin/*` 在沙箱里能否执行因工具而异（`open` 有专门的 `NSWorkspace` 替代）。
  建议：下载改 `URLSession`/reqwest，不再用 curl；开网页用 `NSWorkspace`；播放用 `AVAudioPlayer`/rodio 替 `afplay`；
  `say` 改 `AVSpeechSynthesizer`。每一项都是代码改动，且都要保证默认构建行为不变（用 cfg 或运行期开关隔开）。
- `lsof`/`ps`/`launchctl`/`xattr`/`tar` 主要用在 Qwen3 常驻服务、开机自启、下载解压，MAS 子集里这些功能整体不在，不需要替代。

### 2.4 `sandbox-exec` 嵌套（`src/qwen3.rs`）— 🟡 随 Qwen3 一并排除
- 📖 只用在 Qwen3 speech 进程上（禁出站 TCP，防它静默下载模型）。
- ❓ 已在沙箱内的进程再调 `sandbox-exec` 通常会失败（`sandbox_apply` 不允许在已沙箱化进程里放宽/重套，行为未实测）。
  MAS 子集不含 Qwen3，故不需要替代。若将来要含：靠 `network.client` 的子进程继承是否能禁网**没有**可用做法，只能让 speech 模型预置、不联网。

### 2.5 下载可执行代码 — 🔴 硬阻塞（对相关功能）
- ✅ 审核指南 2.5.2：「…may not download, install, or execute code which introduces or changes features or functionality of the app」；
  2.4.5(iv)：「may not download or install standalone apps, kexts, additional code, or resources to add functionality or significantly change the app from what we see during the review process」。
  来源：<https://developer.apple.com/app-store/review/guidelines/>
- 📖 受影响：`src/qwen3.rs` 运行时下载 speech-server 可执行文件（`RUNTIME_URL`，约 99 MB）→ **MAS 版必须删除这条路径**。
  Qwen3 要用就得把运行时（含补丁版 speech-server）**随包**，包体 +99 MB 并重新签名。
- 📖 泰语：`src/download.rs` 下载的是 whisper 的**模型权重**（`.bin`，数据），引擎 `whisper-cli` 已随包 → 权重下载按指南字面是「resources」，
  2.4.5(iv) 把 resources 也列了，措辞是「to add functionality … from what we see during review」。❓ 实务上下载 ML 模型数据的 App 大量存在，但
  对本项目是否会被认定为「添加功能」**无法事先确定**，保守做法是把泰语作为需随包的选项，或在审核备注里讲清楚。
- 📖 `bundle.sh` 现有 `vendor/` 整体拷入，MAS 版可沿用（随包二进制本来就合规，但需逐个签名，见 2.3）。

### 2.6 Python 边车（对话模式的 LLM 与 TTS）— 🔴 硬阻塞（对对话模式）
- 📖 `scripts/setup-talk.sh` 在用户机器上建 venv、装 mlx_lm / mlx-audio、下 2.3 GB+ 模型，`serve-*.sh` 再拉起。
  既是「下载并执行代码」（2.5.2），又在容器外写文件、起不属于 bundle 的进程。
- 结论：**MAS 版没有对话模式**（或只保留 `say`/AVSpeech 兜底念固定句，不建议）。

### 2.7 开机自启 — 🟡 可替换
- 📖 现状：`src/launch_agent.rs` 往 `~/Library/LaunchAgents/` 写 plist。沙箱下写不了真实 `~/Library`，而且 2.4.5(ii) 禁止在共享位置装东西。
- ✅ 2.4.5(iii)：不得「在启动/登录时自动运行…without consent」——必须有用户同意。
- 替代：`SMAppService.mainApp.register()`（macOS 13+，主应用登录项）。❓ 官方页本次未抓到正文，版本与沙箱兼容性来自一般知识。
  实现要调 ObjC/Swift API（Rust 里可用 `objc2-service-management` 之类，❓未选型）。`LSMinimumSystemVersion` 现为 11.0，用此 API 需提到 13.0 或运行期降级。

### 2.8 `~/.agentear` 与老用户迁移 — 🟡 可做
- ✅ 2.5.2：「may not read or write data outside the designated container area」。
- 本分支已把沙箱下数据根放进容器。
- 迁移：MAS 版与直接分发版是**两个不同 bundle 身份/签名**，沙箱进程**不能读** `~/.agentear`（除非用户在 `NSOpenPanel` 里授权，
  拿到 security-scoped bookmark，需 `files.user-selected.read-only` entitlement，❓）。
  方案：首次启动提示「从旧版导入」，让用户选 `~/.agentear`，只复制 `config.json`、`commands.json`、`kb/`、`raw/`、`derived/`、`routes/`，不复制模型。
  **不做自动迁移**。直接分发版保持原路径不动，两个版本数据互相独立。

### 2.9 连接 Agent24（UDS，`src/a3.rs`、`a3_pair.rs`）— 🟠 多半不支持
- 📖 `UnixStream::connect(creds.socket)`，socket 在 Agent24 的数据目录里，配对时还代跑 `agent24 os attach add`（起外部二进制）。
- ❓ 沙箱应用只能连：自身容器内、app group 容器内的 socket（或 `temporary-exception.files.absolute-path` 这类 MAS 通常不批的例外）。
  要支持需要 Agent24 把 socket 放在共享的 app group 路径（且 UDS 路径有 104 字节上限，group 路径很长，❓）。
  MAS 版建议**不含附着功能**，保持「独立版」行为。

### 2.10 其它
- 菜单栏 `LSUIElement` 应用：MAS 允许 ❓（无明文禁止，常见）。
- 更新：✅ 2.4.5(vii)，必须走 MAS 更新，当前没有自更新机制，不冲突。
- 本地化：✅ 2.4.5(ix) 单 bundle 内，现状满足。
- 麦克风说明 `NSMicrophoneUsageDescription`：已有。
- 架构：仅 arm64，MAS 允许仅 Apple Silicon ❓（提交时要核实，当前 Xcode 流程是否强制 universal 未查）。
- 签名：MAS 需要 **Apple Distribution / 3rd Party Mac Developer Application** 证书 + provisioning profile + 
  `com.apple.application-identifier`、`com.apple.developer.team-identifier` entitlements，并用 `productbuild` 出带 Installer 证书签名的 `.pkg`，用 Transporter/altool 上传。
  **`AgentEar Local Signing` 自签证书做不到，本分支的 `bundle.sh` 分支因此只适合本机沙箱原型验证**。❓ 流程细节按 Apple 当前文档为准。
- 提审：需要 App Store Connect 记录、隐私问卷（无数据收集，需要写明）、审核备注解释麦克风用途。

## 3. 硬阻塞摘要

| # | 阻塞 | 影响 | 出路 |
|---|---|---|---|
| B1 | 右 Command 单击/双击靠 CGEventTap + 辅助功能 | 核心手势 | MAS 版只提供组合键（Carbon），❓需验证 |
| B2 | 自动 ⌘V 上屏靠 CGEventPost + 辅助功能 | 「上屏」 | 只写剪贴板，需产品拍板 |
| B3 | 运行时下载 speech-server 可执行文件（2.5.2） | Qwen3 | 随包，或 MAS 版不含 Qwen3 |
| B4 | Python 边车 + 用户机上 setup 脚本 | 对话模式 | MAS 版不含对话模式 |
| 次要 | 容器外写文件、LaunchAgent、`~/.agentear`、UDS、`sandbox-exec`、众多 `/usr/bin` 子进程 | 见上 | 见 2.3–2.9 |

## 4. 分阶段方案

**阶段 0（本 PR）**：entitlements + 签名开关 + 数据根。无行为变化。

**阶段 1：沙箱原型（只验证，不改产品）**——用 `AGENTEAR_DISTRIBUTION=app-store` 打包，在本机真跑：
1. 沙箱里 `HOME` 到底是什么（验证 `resolve_data_root` 的两种分支）；
2. `RegisterEventHotKey` 能否触发；`CGEventTap` listen-only 能否建（要什么权限）；
3. 随包 `llama-funasr-sensevoice` 逐个签 `inherit` 后能否被拉起、读 bundle 内模型；
4. `curl`/`afplay`/`say`/`open` 在沙箱里哪些能跑；
5. `NSPasteboard` 写剪贴板。
任何一项不过，MAS 版方案相应收缩。

**阶段 2：MAS 功能子集（建议用 cargo feature `app-store` 隔离，默认构建不含）**
- 包含：输入法模式 · 随包 SenseVoice（中英）· 组合键触发 · 写剪贴板 · 本地知识库/全文检索 · 语音指令表中的 `builtin` 与 `open_url`（用 NSWorkspace）· 界面三语。
- 视阶段 1 结果与审核备注：泰语（模型下载或随包）。
- 去掉：右 Command 手势、自动上屏、对话模式与全部边车、Qwen3（或随包后再议）、LaunchAgent（改 SMAppService）、Agent24 附着、`http_post`（需 network.client，可留，但注意审核对「向外发数据」的描述）。

**阶段 3：提审工程**——证书/profile/pkg、App Store Connect、隐私问卷、截图、审核备注。

**只能留在直接分发版**：右 Command 手势与自动上屏（除非阶段 1 验证它们在沙箱里可用）、对话模式（LLM+TTS 边车）、Qwen3 运行时下载、Agent24 附着、开机自启的 LaunchAgent 方式、`sandbox-exec` 防护。

## 5. 来源

- App Review Guidelines 2.4.5 / 2.5.2：<https://developer.apple.com/app-store/review/guidelines/>（✅ 已抓取正文）
- 配置 App Sandbox：<https://developer.apple.com/documentation/xcode/configuring-the-macos-app-sandbox>（❓ 本次只取到标题）
- SMAppService：<https://developer.apple.com/documentation/servicemanagement/smappservice>（❓ 同上）
- 嵌入 helper 工具：<https://developer.apple.com/documentation/xcode/embedding-a-helper-tool-in-a-sandboxed-app>（❓ 同上）
