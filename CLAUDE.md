# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 当前状态：**v0.9.0 —— 可配置语音指令表（本地快路径）**；M1/M2 已发布；**M3 通话链路已跑通，AEC / 自动打断未做**

M1 完成；**知识库投递 + 全文检索默认开（v0.4.2）**；M2 理解层已发布（v0.4.0）但默认关；
**v0.6.0 加了通话链路（说一句答一句、可按键打断）**，**v0.5.0 加了可切换的 ASR 后端**（`--asr-backend` / `config.json` 的 `asr_backend`，
**默认仍是 `builtin`**，`speech_swift` 要用户自己装 `speech` CLI，且**菜单栏里没有这一项**）。

**M3 实时对话（ADR-0007）：V1 的通话链路已随 v0.6.0 发布；v0.7.0 给了它产品入口。**

**两种模式，默认输入法（jason 2026-09-14 拍板）：**

| 模式 | 按一下录音键 → | 默认 |
|---|---|---|
| **输入法模式** | 录音 → raw 落盘 → 转写 → 剪贴板 / 上屏。**不出声** | ✅ **是** |
| **对话模式** | 上面全部 + LLM → TTS → 播放（按录音键可打断正在播的回答） | 否 |

- **切换入口有两个**（v0.7.2 起）：
  1. **按键手势**（jason 2026-09-14 定的）：**单击右 Command = 输入法模式**，
     **双击右 Command = 对话模式**。判据在 `hotkey::classify_tap`/`intent`（纯函数 + 真值表）。
     - 双击窗口 **500ms**（`DOUBLE_TAP_MAX_MS`）——**照 macOS 自己的
       `NSEvent.doubleClickInterval` 默认值来的，不要凭手感调小**。
       jason 第一次试的时候两下相隔 **1004ms**（当时窗口 350ms），被判成两次单击 →
       「录音过短丢弃」→ 他的感受是「双击没反应」。**窗口太紧的失败是静默的**，
       所以对齐系统惯例；这条失败样本已经进了回归用例。
     - **菜单栏状态项会加一个模式标记**：对话模式显示 `🎙💬`，输入法模式只有 `🎙`。
       只在非默认档加符号——默认状态不该也多背一个字符，而菜单栏寸土寸金。
     - **「停止」不许改模式**——`finish()` 是松手那一刻读配置的，如果停止顺手切回输入法，
       双击选出来的对话会被顶掉，症状看起来像「双击没生效」。
     - **录音中收到双击只切模式、不停录音**：第一次点击已经开了录音，
       第二次若按 toggle 处理会立刻停成 0.2 秒碎片（会被当噪音丢弃）。
     - 组合键（Ctrl+Shift+R）没有双击这个维度，走 `Signal::ManualToggle`，**不改模式**。
  2. **菜单栏 → 模式**（`src/tray.rs` 的 `TAG_MODE_BASE`），两项都列出来带勾选。
     **标题里带当前模式**（`模式: 对话`）——只写「模式」时 jason 找不到入口
     （他在找「对话模式」四个字）。**点一下立刻生效**，不用重启、不用等下一轮。
  ⚠️ **不要把它做成配置文件里的开关**——v0.6.0 就是那样（`talk_enabled` 只能手改
  config.json），等于没有入口。
- 配置字段是 **`talk_mode`**（`"input_method"` / `"conversation"`）。
  v0.6.0 的 `talk_enabled` **降级为只读旧字段**：`talk_enabled: true` →
  `talk_mode: "conversation"` 迁移一次，之后落盘时旧字段自动消失（`skip_serializing`）。
  **迁移判据是「新键根本没出现过」，不是「新键等于默认值」**——两者差很远：
  用户从菜单显式切回输入法之后，按后者判断会被旧字段顶回对话模式
  （菜单显示输入法、行为却是对话，属于最难查的一类 bug）。
- 两种模式的**前半段完全相同**（录音/落盘/转写/上屏），只有后半段要不要出声不同。
  所以走错模式**只会让这一轮没声音，不会丢转写**。
- ⚠️ **切模式时要把会话推到 `Listening`**（`ensure_session_listening`）：会话可能
  ①压根不存在（启动时是输入法、中途双击切对话）②存在但停在上轮的 `Idle`。
  这两种情况下 `finish_listening` 会被状态机按非法转移**静默**拒掉（只写 warning），
  统计变成自相矛盾的「0 轮」而声音照样出得来——`--talk-turn` 当初就栽在这上面。
  **顺序**：先定模式（可能刚建会话）→ 再推会话相位 → 最后才开麦克风。
- **对话模式的两个边车：连接优先、拉起兜底**（ADR-0002 §8 的老规矩，v0.7.1 补齐）。
  启动（对话模式）或从菜单切进对话模式时，`talk::ensure_sidecars_async` 在**后台线程**里
  逐个探活；没起就按 `talk_llm_start_command` / `talk_tts_start_command` 拉起，
  再等就绪（最长 90s）。
  ⚠️ **必须异步**：就绪等待放主线程上，菜单栏会整整一分半不响应，而用户此刻正在按键。
  - **拉起命令默认是空的**（= 不知道怎么拉，只连不拉）。理由和 `llm_start_command`
    一模一样：**不能写死编译期路径**——那是开发机的仓库路径，分发出去指向不存在的目录，
    而且一旦写进用户 config.json 就固化了。空的时候**日志里会打出该跑哪条命令**，不静默。
  - **退出时必须收掉我们拉起的那些**：菜单 Quit 走 `talk::shutdown_spawned()`，
    **信号路径（Ctrl+C / kill）走 `sidecar::on_signal` 里新增的
    `talk::kill_spawned_pids_from_signal()`**——只做 `kill(2)`，符合 async-signal-safe。
    漏了信号那条路会留下两个常驻约 4 GB 的进程（实测 2026-09-14：修好后 SIGTERM 能收干净）。
  - **只收自己拉起的**：用户手工跑的进程一律不动（`sidecar.rs` 定的规矩）。

- **`src/talk.rs` = 通话引擎适配层**（ADR-0007 §4 的 R0）。
  `TalkLang{zh,en,th}`；`LlmEngine` 两个实现——`OpenAiCompat`（任何 OpenAI 兼容端点，
  **默认指向 `127.0.0.1:8794`** 的 MiniCPM5-2B 边车）与 `WeatherMock`
  （零依赖、写死回答、**必须显式配置 `talk_llm_engine: "mock"` 才启用**）；
  `TtsEngine` 两个实现——`HttpTts`（走 `services/tts` 边车，默认 VoxCPM2-4bit）
  与 `SayTts`（macOS `say`，零依赖兜底）。另有 `AudioTransport`/`CurlAudio`
  （音频是**二进制 POST，不能走 String**）、`validate_wav`（**HTTP 200 但不是 WAV 一律拒绝**，
  否则播放器会把一段 HTML 错误页当音频）、`play_blocking`（`afplay` 子进程，可打断）、
  `strip_thinking`（兜掉 ` thinking` 段）。
- **`src/session.rs` = 通话会话状态机**，ADR-0007 §4.4 **选 A（R2 自主编排）**的落地：
  相 `Idle/Listening/Thinking/Speaking/Failed`；`begin_turn` / `finish_listening` /
  `turn_ready` / `speaking_done` / `barge_in` / `set_lang` / `fail` / `hang_up` / `turn_elapsed`。
  **语言可在通话中随时切**，但 Thinking 相拒绝（理由写在文档注释里）；
  **空转写不记轮次**；LLM 失败时转写照样记一轮（`reply: None`）。
- **配置项全部默认关、或指向本机默认端口**（`src/config.rs`）：`talk_mode`(input_method)、
  `talk_lang`(zh)、`talk_llm_engine`("openai_compat")、`talk_llm_url`(None→8794)、
  `talk_tts_engine`("http")、`tts_url`(None→8765)、`talk_timeout_secs`(60)、
  `talk_city`("清迈")、`talk_weather_note`(None)，另有 `Config::weather_fact()`。
  **留在输入法模式时，已发布用户的行为一个字节都没变**（v0.6.0 的老 `talk_enabled`
  会迁移成对话模式，所以那台机器升级后仍是对话——这是有意的，见上）。
- **CLI 三个新入口**（都不碰麦克风）：
  `--ask <文字>`（文字 → LLM → TTS → 播放，跳过 ASR）、`--say <文字>`（只测 TTS）、
  **`--talk-turn <wav> [--lang zh|en|th]`**（**完整一轮，且走守护进程那条代码路径**：
  ASR → 会话状态机 → LLM → TTS → 播放）。加 `--talk-turn` 的理由是**可测性**：
  守护进程那一轮的入口是录音键，按键/麦克风权限/TCC 都没法无人值守复现，
  没有它「推键式链路真的通」就只能靠人肉按一次键来证明。
  ⚠️ 它实测**当场抓出一个真 bug**：这个入口最初漏了「等价于按下键」的
  `begin_turn()`，于是状态机把 `finish_listening` / `turn_ready` 全部按非法转移
  拒掉——**只写 warning**，统计出来是自相矛盾的「0 轮」。
- **守护进程两处改动**：① 录音键**先 `talk::stop_playback()` 掐掉正在播的回答再开麦克风**
  ——这是 V1 的打断入口；② 转写 / 上屏 / 知识库都走完之后，**若 `talk_mode` 是对话模式**
  则 `answer_out_loud()` 把回答念出来，**失败只记日志，不挡上屏**。
  模式是**每轮现读配置**的，所以菜单里切一下对下一轮立刻生效。
- **语音输出三栏（v0.8.1）**：菜单 → **说话** → 语系（12 项）/ 语气（4 项）/ 音色（**扫目录**）。
  配置 `tts_style` / `tts_tone` / `tts_voice` / `tts_voices_dir`；
  `HttpTts` **逐请求**带上这三个参数，所以菜单改完下一轮立刻生效。
  - ⚠️ 选项表在 Rust（`talk::STYLE_OPTIONS`/`TONE_OPTIONS`）和 Python
    （`STYLE_INSTRUCTS`/`TONE_INSTRUCTS`）**各有一份**（菜单不能为列一次表去发 HTTP）。
    **漂移的后果是「菜单点得下去、边车回 400」**，所以两边各有一条测试钉住**完整键集**，
    改一处必须改另一处。
  - **默认语气 `warm` 是实测挑的**：只写语种时 F0 起伏 3.53 半音 / 能量起伏 0.0736，
    加情绪描述后 5.57 / 0.1196（+58%/+62%，且更快）。
  - ⚠️ **`inference_timesteps` 不要降档**：实测 steps=5 快 1.9 倍但起伏掉到 4.10/0.0630
    （最平），steps=20 只更慢（5.54s）。**默认 10 是甜点。**
  - **`/health` 现在自报 `peak_rss_mb`**（`resource.getrusage`）：macOS 沙箱里
    `ps`/`top` 读不到别的进程内存，而仓库规矩要求「高资源档峰值 RSS 必须实测入库」。
    实测 4bit **2386MB** / 8bit **3266MB**。
- **语音指令表（v0.9.0）= 本地快路径 + LLM 兜底的两段式**。
  用户先说一句声明过的短语（「记一下…」「搜索…」「发邮件给…」），
  **本地先查表**（`src/commands.rs`，纯字符串前缀匹配，0ms，断网也能用）；
  **没命中就走对话模式那条 LLM 路**——开放式的句子本来就该由模型理解，
  指令表只认用户声明过的那几条。表在 **`<数据目录>/commands.json`**，
  文件不存在时给一份开箱默认（`default_commands()`），**坏文件报错、不静默退回默认**
  （静默退回会让用户以为自己的指令还在）。
  - **动作是一个三种的封闭集合**：`builtin`（style/tone/mode/note）、
    `open_url`（**只允许 http / https / mailto**）、`http_post`（用户自己的 webhook）。
    ⚠️ **绝不执行 shell**：语音识别错一个字就变成在你机器上执行命令，而且没有撤销键。
    `validate()` 里拒掉 `file:` 之类，「打开 X」不能变成读本地文件。
    「发邮件」用 `mailto:` **打开草稿让人确认**，不是替他发出去——误识别的代价是草稿。
  - **匹配是「精确 + 去标点 + 归一大小写」，长的短语优先**
    （否则「搜索 github」永远被「搜索」抢走）。⚠️ **繁简归一没做**——
    没有离线表，不要写成「中英泰繁简都行」。
  - **槽位（`rest`）取自原文，不是归一句子**。这一点踩过：
    最初从归一句子截槽位，「帮我搜代码 talk.rs」变成搜 `talkrs`——
    点没了、大写也没了，**而且当时的测试把这个坏行为钉住了**
    （`rest == "rustasync"` 看着还挺像那么回事）。**归一只用于判断命中**。
  - **URL 模板里的槽位要转义**（`fill` 按模板判断：`http/https/mailto` 才转，
    JSON body 不转）。槽位是**语音听出来的任意内容**，
    不转的话 `&` 能凭空多一个参数、`#` 会把后面整段吃掉。
  - **入口三个**：菜单「打开指令表」（`TAG_OPEN_COMMANDS`，直接开 `commands.json`
    给人改——ASR 会把短语听错，文件必须可编辑，否则用户得到一条永远匹配不上的指令）、
    CLI `--add-command <短语>` / `--add-command-wav <wav>`（**录一句→ASR 成短语→写进表**）、
    CLI `--commands`（列表）/ **`--match-command <一句话>`（干跑：只报命中，不执行）**。
    `--match-command` 是**可验收性**入口：不必真录一条音、也不必真发一封邮件
    就能验证匹配器——它上线当天就抓出了上面那个槽位 bug。
  - 配置 `commands_enabled`（**默认开**）：开着的代价只是「多查一次字符串前缀」。
  - ⚠️ **指令表在两种模式下都会触发，不限对话模式**——这是有意的：
    「切换到对话模式」这条内置动作**只有在输入法模式下说才有意义**，
    锁进对话模式等于把最有用的那条指令废掉。
    **代价要认**：输入法模式下说了一句「恰好以某个已声明短语开头」的话，
    它会**顺手多干一件事**（开浏览器 / 记一条 / 切音色），
    **但文字照样进剪贴板、照样上屏，一个字节都不会丢**。
    所以默认表的短语故意写得具体（「搜索」「帮我搜」「发邮件给」「记一下」），
    而不是「打开」这种半句话。**不想要就 `commands_enabled: false`。**
    ⚠️ 这条与「留在输入法模式时已发布用户行为不变」是**有冲突**的：
    默认开 = 输入法用户也会遇到「多干一件事」。判断是**收益大于这个风险**
    （文字不丢、动作可见、一句话能关掉），但**不要写成「输入法模式完全没变」**。
- **边车与脚本**：`services/tts/backends.py`（新增 `SayBackend` + `VoxCpm2Backend`，
  HTTP 契约不变）、`services/tts/server.py`（`--backend {voxcpm2,say}`，**默认 voxcpm2**）、
  `scripts/setup-talk.sh`（按需下载两个模型，**不随包分发**）、
  `scripts/serve-talk-llm.sh`（`mlx_lm.server` + `--chat-template-args '{"enable_thinking": false}'`，
  端口 8794）、`scripts/serve-tts.sh`（用带 mlx-audio 的 venv 起 `server.py`）、
  `scripts/talk-e2e.sh`（端到端验收）。

**⚠️ M3 这轮的诚实边界（不要美化、也不要外推）**：

> 本轮的实测明细在 **`docs/benchmarks-talk.md`**（**MLX 4bit 路径**：
> `VoxCPM2-4bit` + `MiniCPM5-2B-4bit`，2026-09-14）。
> ⚠️ 它与 `docs/benchmarks-m3.md` **不是同一条路径**——那份是 **speech-swift 的 bf16**
> （Swift CLI），**两份数字不能互相引用、也不能横比**（连「4bit 更省内存」都不成立：
> 2.5 GB 是 Python + MLX 进程峰值，1.54 GiB 是 speech-swift 进程峰值，口径不同）。

1. **天气那句话是本地写死的场景，不是天气接口**（ADR-0007 §4.6）。它的唯一作用是
   **证明 ASR→LLM→TTS 通**，不是产品能力；接真实天气源是集成方的事（R3 外壳层）。
2. **V1 只是打断式半双工**，解决「说完才轮到我」，**不解决「一边听一边想」**——
   **不许写「体感接近全双工」**。**AEC 这一格仍然没解决**：T3.4.0 实测 VPIO
   **内容级自触发 0/5、事件级仍 5/5**（残留单字）。本轮的解法是**推键式**
   （用户按键即打断、播放前先掐），**没有做 VAD 自动打断，也没有做双讲**；
   **误打断率、端到端打断延迟 <300ms 仍是未测**。
3. **MiniCPM5-2B 的模型卡只声明 en/zh**，泰语在能力边界外：实测能听懂泰语问句、
   也能产出泰语，但提示词不钉死语言时会用中文回答；钉死之后**仍然不稳定**——
   实测到两种坏形态：**夹英文词**（`overall`）与**把问句原样退回来**
   （后者已由 `src/talk.rs::is_echo` 挡住，这一轮不播，宁可没声音）。
   **不要把泰语质量写成与中英同级**，也不要写成「稳定纯泰语」；要稳就得换模型
   （`talk_llm_url` 指向任何 OpenAI 兼容端点，Rust 侧不用改）。
4. **4bit 与 bf16 的质量对比没做**（只测了 4bit 能跑、时延与内存）。
   `benchmarks-m3.md` 里那批 VoxCPM2 数据是 **speech-swift 的 bf16 路径**，
   与本轮的 **MLX 4bit 路径不是同一个运行时，不要混着引用**。
5. **whisper 泰语的时延数字在本 sandbox 里无效**：实测每次调用约 19.9s，
   而 `docs/data/thai-coldstart-raw.txt` 记的是 0.96s；CPU-only（`-ng`）是 5.9s，
   差异来自 Metal 着色器缓存写不进去（sandbox 禁止写仓库外）。
   **任何在本环境测出的 whisper/Metal 时延都不可引用**，要么在正常 Terminal 里复测，
   要么标注为环境无效。

M2 = 术语纠错 + 一级标签识别 + `routes/` 落盘，需要一个本地 LLM 边车
（`scripts/setup-llm.sh` / `serve-llm.sh`，模型 7.8 GB **不随包分发**）。
边车的生命周期见 ADR-0002 §8：**连接优先、拉起兜底**，
`llm_autostart: false` 就是「只连不拉」的形态。

`routes/` 已经接上下游：**文件适配器**把每条记录渲染成 `kb/**/*.md`
（ADR-0003 §3.3 的 front matter），失败进 `routes/.pending/` 重试队列，
`--replay-kb` 可从 `routes/` 全量重建。**默认开**（`kb_enabled`，不需要任何外部依赖）。
**没开理解层时仍然认显式标记**（`label::explicit_only`，纯本地字符串匹配）——
不要退回硬编码 `unknown`。但**显式标记只认中英文固定句式，没有泰语**，
所以「不启用理解层」实际等于「只有明说了标签的话会进知识库」，
**不要把它描述成「每次转写都会进知识库」**。

**L2 索引（`--search` / `--reindex`）已落地**：`derived/index.sqlite`，
rusqlite + FTS5。**中文靠「写入前逐字切开」**——FTS5 自带分词器把整句中文
当一个 token，`trigram` 又要求查询 ≥3 字符。改分词方案要重跑
`docs/agent/tasks.md` T2.4.4 里那张横比表，别凭印象换。
**标签也要走 `segment`**，只切正文会让中文标签整个搜不到（栽过）。

**`--replay-kb` 不重新分类**：它按 `routes/` 里已有的标签重放。
之前落成 `unknown` 的记录，即使后来启用了理解层，重放也捞不回来——
重新分类是单独的事，还没做（见 `docs/agent/tasks.md` T2.4.6）。
ADR-0003 的**组织档适配器（memos）还没做**，等真有企业需求再定（ADR-0003 §6）。

**CI**：`.github/workflows/ci.yml` 每次 push/PR 跑 build+clippy+test，
**必须用 macOS runner**（CGEventTap/CoreAudio/`say`，Linux 编译都过不了）；
`external-links.yml` 每周跑那条联网判据（模型 URL 指向的资产还在不在）——
**它守的是外部世界变了，push 永远触发不了它**。
clippy **刻意不加 `-D warnings`**（既有 13 条警告，加了会让 CI 长期红，
而长期红的 CI 等于没有 CI）。CI **不需要 `vendor/`**（实测移走后 203 全绿）。

**构建与测试**：

```bash
cargo build --release
cargo test                                    # 268 passed / 0 failed / 6 ignored（ignored 6 条：4 条要边车、1 条要联网、1 条要能出声的环境）：提交协议、崩溃语义、token 过滤、i18n、下载协议、知识库投递、通话会话状态机、语音指令表
./target/release/agentear                     # 守护进程，Ctrl+Shift+R 开始/停止录音
./target/release/agentear --transcribe x.wav  # 离线转写，不占麦克风，用于验证 ASR 链路
./target/release/agentear --diagnose          # 环境自检：权限、音频设备、ASR 依赖
./target/release/agentear --debug-keys        # 打印每个修饰键事件，排查按键问题
./target/release/agentear --fetch-thai        # 预下载泰语模型（574 MB），只装不改识别语言
./target/release/agentear --transcribe x.wav --lang th   # 不改配置试泰语链路
./target/release/agentear --classify "这是一个 idea"      # 给一段文字分类（评测脚本也走这条）
./target/release/agentear --replay-kb                    # 从 routes/ 全量重建 kb/，幂等，可反复跑
./target/release/agentear --say "你好"                    # 只测 TTS：合成 + 播放（跳过 ASR 和 LLM）
./target/release/agentear --ask "今天天气怎么样"           # 文字 → LLM → TTS → 播放（跳过 ASR）
./target/release/agentear --talk-turn q_zh.wav --lang zh   # **完整一轮**（ASR→会话→LLM→TTS→播放），走守护进程同一条路径
./target/release/agentear --commands                       # 列出语音指令表（默认给一份开箱表）
./target/release/agentear --add-command "记一下"            # 加一条指令（--action builtin|open_url|http_post）
./target/release/agentear --add-command-wav my.wav          # **录一句**定义指令：先 ASR 成短语再写进表
./target/release/agentear --match-command "搜索 talk.rs"    # 干跑：只报命中/槽位，**不执行**任何动作
cargo test -- --ignored stop_playback         # 打断机制：真掐掉一段 5s 音频（需要能出声的环境）
scripts/bundle.sh                             # 打 .app bundle → dist/

# 通话链路（M3）的两个边车 + 端到端验收，都要单独起，都**不随包分发**：
scripts/setup-talk.sh                         # 首次：下 MiniCPM5-2B-4bit（LLM）与 VoxCPM2-4bit（TTS）
scripts/serve-talk-llm.sh                     # 每次：LLM 边车，默认 127.0.0.1:8794
scripts/serve-tts.sh                          # 每次：TTS 边车，默认 127.0.0.1:8765（--backend voxcpm2）
scripts/talk-e2e.sh --text '今天天气怎么样' --lang zh   # 全链路：ASR → LLM → TTS → 音频文件

python3 -m unittest discover -s services/tts -p 'test_*.py'   # 34 passed（TTS 边车）
```

日志同时写 stderr 和 `~/.agentear/agentear.log`。

⚠️ **`--transcribe` 的 `--lang` 只认 `th` / `auto`**（实测传 `zh` 会被参数解析拒绝，exit 1；
中英走 SenseVoice 默认路径，不需要这个参数）。`scripts/talk-e2e.sh` 里那条按语言决定加不加
参数的写法就是为它让路的。

**macOS 上按键相关的三个坑**（症状都是「按了没反应」，见 `docs/m1-status.md`）：
主线程必须跑 AppKit/CFRunLoop 事件循环；`NSEvent` 全局监听在纯 CLI 二进制里回调
永不触发（用 `CGEventTap`）；修饰键的松开事件 keyCode 与按下相同，判据只能看
设备位 `NX_DEVICE_R_CMD (0x10)`。

**TCC 权限不会从终端带到 .app**：两者是独立主体，麦克风与辅助功能各自要授权一次。

**MLX 的一个硬约束（本轮踩到，值得先记着）：MLX stream 是线程局部的。**
在一个线程加载权重、在 HTTP 工作线程里推理**不会抛 Python 异常，而是直接 SIGABRT**：

```
libc++abi: terminating due to uncaught exception of type std::runtime_error:
There is no Stream(cpu, 1) in current thread.
```

崩点在惰性数组**第一次被求值**（`np.asarray`）的那一刻，而只读 `.shape` / `.size`
**不会触发求值**——所以探针可能「通过」而 bug 还在（本轮最初的探针就是这样）。
修法：**load + generate + numpy 转换全放在同一条专用线程上**，HTTP 工作线程
通过队列投递（`services/tts/backends.py::VoxCpm2Backend._mlx_thread`）。

数据落在 `~/.agentear/`（`AGENTEAR_DATA` 可覆盖）；ASR 二进制与模型在 `vendor/`（`AGENTEAR_VENDOR` 可覆盖，**不入库**）。

文档阅读顺序：`docs/milestones.md`（里程碑）→ `docs/decisions/`（决策记录，**选型结论以此为准**）→ `docs/benchmarks.md`（实测数据）→ `docs/ingest-design.md`（接入层设计）。`docs/asr-selection.md` 是初版调研，其中的选型结论**已被 ADR-0001 推翻**，仅作历史参考。

仓库在 GitHub 上（`git@github.com:iDoris-ai/AgentEar.git`，分支 `main`）。

### 已拍板（jason 2026-07-29 确认，不要重开讨论）

1. **M1 只做到转写** —— 快捷键 toggle 录音 → raw 落盘 → 转写 → 文字进剪贴板。不含 LLM、路由、TTS。
2. **常驻守护进程用 Rust** —— 单二进制、无运行时，契合 ASR 的选型理由。
3. **先做丢弃式 Python spike 跑 M0 基准** —— 「不用 Python」的约束针对常驻进程，不针对一次性测量脚本。spike 用完即删。

### 技术栈

- **LLM 已定为 `Ornith-1.0-9B` MLX 6bit**，经 mlx-dspark 提供 HTTP 服务，**常驻**。见 `docs/decisions/0002-m2-understanding-layer.md`。注意 GGUF 的 Q5_K_M 与 MLX 格式不兼容，MLX 侧的对应档是 6bit。
- **ASR 已定为 `SenseVoiceSmall q8` + FSMN-VAD**，走 `llama-funasr-sensevoice` 单二进制。**决策依据见 `docs/decisions/0001-asr-model-selection.md`，不要重开选型讨论。**
- **Fun-ASR-Nano 已被推翻**：四模型实测横比后，它是唯一在 30 分钟音频就爆 2 GiB 预算的（16 分钟即破），且冷启动 11.45s。SenseVoice 常驻仅 419 MB（Nano 的 27%）、冷启动 0.2s、术语命中反超。
- **Qwen3-ASR-0.6B 指标最好但出局**：需 Python + MLX 运行时。若将来出现 GGUF/纯 Rust 路径，应重新评估。
- **Paraformer 出局**：完全不输出标点。
- **Whisper 不做主链路**：中文 CER 远差于中文专用模型。但不要重复「差一个数量级」这个说法——那是混用了两种测试集口径。
- **复用 `ququ/` 仅限配置与思路层面**（模型选择、中文后处理经验），**不继承其运行时**。常驻进程背一个内嵌 Python 环境不划算。`huniu/`、`whisper.cpp/` 同理，是参考不是迁移源。
- 目标机器 **M1 Max / 64 GB**。
  **常驻内存预算：M1 阶段 ≤2 GiB；M2 起放宽到 ≤9 GiB**（引入常驻 LLM，见 ADR-0002）。

  **ASR 侧改为分档（jason 2026-09-08 拍板，取代原来的「ASR 侧仍按原标准」）：**

  | 档 | 预算 | 谁在这一档 |
  |---|---|---|
  | **默认档** | **≤2 GiB** | 随包分发的 `builtin`（SenseVoice + whisper 泰语） |
  | **高资源档** | **≤4 GiB** | 用户显式启用的可选后端，如 `speech_swift`（Qwen3-ASR 1.7B 实测 2.43 GiB） |

  **准入条件（高资源档三条都要满足）**：① 必须是**用户显式启用**的非默认项；
  ② 必须**不随包分发**（用户自己装）；③ 峰值 RSS 必须**实测入库**。

  ⚠️ **分档不是取消约束。** 默认路径的 ≤2 GiB 一步没退——
  没装外部依赖的用户拿到的内存占用和 v0.4.2 完全一样。
  这条放宽只覆盖「用户主动选了更大的模型」这一种情况。
- **「不背 Python 运行时」的精确措辞**（ADR-0002 修订）：**Rust 守护进程自身**不内嵌
  Python 运行时；**外部推理服务**的实现语言不受限，但必须是独立进程 + 明确协议边界
  （可独立重启、独立崩溃）。M2 的 mlx-dspark 就是这样的边车。

### M0 实测产出的硬约束

**SenseVoiceSmall q8 实测关键数字**：权重 242 MiB + VAD 1.6 MiB、**冷启动 0.2s**、RSS 419 MB(2min) / 1.27 GiB(30min)、RTF 0.030–0.033。

1. **冷启动 0.2 秒 → M1 不需要常驻模型服务。** 每次录音直接 `Command::new()` 调 `llama-funasr-sensevoice` 子进程即可，无需链接 C++ 库、无需 server 模式。（这是换掉 Fun-ASR-Nano 的直接红利——Nano 冷启动要 11.45s，必须常驻。）
2. **长音频仍需分段送入 ASR，单段建议 ≤5 分钟。** RSS 随音频长度增长 0.52 MB/s，54 分钟破 2 GiB。影响 `ingest-design.md` 的**路径 A**；M1 的快捷键录音不受影响。
3. **特殊 token 需过滤，但 `--keep-tags` 必须开着。** 已实测（2026-08-22）：转写走 stdout、日志走 stderr，多个 VAD 段拼在同一行，默认不泄漏 `/sil`。**`asr.rs` 靠 `<|zh|>`/`<|en|>` 标记的存在与否区分「转写结果」和「日志」，所以不能去掉 `--keep-tags`。** 绝不要退回「按有没有汉字判断」——那会把英/日/韩/泰的结果整段丢掉（已修，见 `docs/m1-status.md`）。
4. **中英混杂技术术语不可靠。** 实测 `raw` 一词四个模型全错（row/road/ro/roll）；Docker → `doocca`、Kubernetes → `cuubber needs`。**术语纠错是 M2 的职责**，不要指望换 ASR 解决。
5. **语种支持边界（实测）**：中文 ✅、英文 ✅、中英混合的中文部分 ✅。**泰语 ❌**。
   **`llama-funasr-sensevoice` 的语种集合里根本没有 `th`**（只有 zh/en/yue/ja/ko/nospeech），
   所以它永远不可能把音频标成泰语——两次实测分别误判成 `<|en|>` 和 `<|yue|>`。
   这排除了「拿 SenseVoice 的语种标记当泰语路由依据」这一条路线（**但推不出
   「只能用户显式选择」**——显式菜单是产品决策，不是实测结论，见 ADR-0004 §1）。
   `src/asr.rs::thai_is_not_a_sensevoice_language` 钉住这个事实。
6. **泰语引擎见 `docs/decisions/0004-thai-asr-engine.md`（已临时选定 `distill` q5_0，
   v0.3.1 落地）**。⚠️ 是「先用起来、拿到语料再复评」的默认值，不是终局；
   换模型 = 改 `src/download.rs` 四个常量 + 重跑 `scripts/build-thai-model.sh`。
   已完成：
   三个 Whisper 系泰语微调（Thonburian medium / Thonburian distil-large-v3 /
   typhoon-whisper-turbo）已转 GGML 并量化，跑在现有 whisper.cpp 上、不引入新运行时。
   FLEURS 泰语 test 上实测 CER（n=80 条录音，含自助法 CI）：Thonburian 两个约
   6.1–6.5%、typhoon-turbo 约 9.5%。六组**事后固定的探索性比较**里只有两组
   检出差异，都是 Thonburian 优于 turbo；**但 Thonburian 的模型卡声明训练用过
   FLEURS，比较不中立**。q5_0 与 q8_0 **未检出**准确率差异
   （「未检出」不是「等效」——没有预定义非劣界）。
   f16 档因峰值 1.84–1.90 GB 出局。**选型卡在 code-switch 数据**——FLEURS 没有夹英文，
   而那正是实际场景。模型**按需下载**不随包分发（jason 2026-08-22 拍板），
   q5_0 约 540–575 MB。
7. **⚠️ 「长音频必须分段、单段 ≤5 分钟」是 SenseVoice 的约束，在 whisper 路径上
   状态是「未验证」，不是「不适用」。** SenseVoice 的 RSS 每秒涨 0.52 MB；
   whisper.cpp 在 11–98 秒区间只涨 18–40 MB，但**98 秒外推不到几十分钟**，
   要测 5/15/30/60 分钟才能下结论。见 ADR-0004 §3。

### M1 的两个红利，不要提前破坏

选择「M1 只到转写」让 M1 恰好绕开了两个最难的部分：

- **不需要 AEC**（没有 TTS 播放，麦克风不会收到自己的声音）
- **不需要流式 raw 语义**（快捷键的按下/再按天然给出段边界，每次录音就是一个有头有尾的文件对象，可直接用 `ingest-design.md` §3.3 的零丢失提交协议）

**不要在 M1 里提前引入 TTS 或无边界流**，那会把这两块难度一起拽进来。它们属于 M3。

### 存储语义（已定，不要在实现时重新解释）

`raw/audio/` = **ASR 之前**的原始字节，丢了不可重建；`derived/transcripts/` = 模型输出，可重算；`routes/` = 下游决策，可重算；`kb/` = 人读的 Markdown 文档层，**可从 `routes/` 全量重放**（`--replay-kb`）。

**分层的分界线是「能不能从语音重算」，不是「存在哪」**（ADR-0003 §7）：L0 事实层（raw+derived+routes，音频不可重建）→ L1 文档层（`kb/`，可重放）→ L2 索引层（还没做，可重建）→ **L3 行动层（任务/日程，带用户后来改的状态，不可重放）**。L3 不能塞进 Markdown 文件树，否则重放会覆盖用户改过的状态。**原始音频的持久化不得依赖任何下游步骤成功**——这是 README「先存后分流」的执行点。

**两条接入路径的持久化保证等级不同，措辞上不要混用**：文件导入（路径 A）是**零丢失**，走完整提交协议后才 ACK；实时流（路径 B）是**有界丢失**——音频以 tee 同时喂 ASR 和落盘，崩溃会丢掉最后一次 fsync 之后的部分，raw 对象按定长时间片（非 VAD 边界）切分。**下游路由只消费 committed 的转写，不消费 provisional 的。** 详见 `docs/ingest-design.md` §3.7。

### 「双工」的正确理解

jason 要的「边说边理解、可互相打断」是全双工 speech-to-speech（Moshi 一类）的能力。
**产品形态已分成两档（jason 2026-09-08 拍板，见 [ADR-0007](docs/decisions/0007-realtime-voice-architecture.md)）：
V1 = 打断式半双工，内存门槛 ≤10 GB；V2 = 实时全双工，放宽到 64 GB 级。**
V1 不是 V2 的临时替代品，是**长期保留的低配档位**——不是所有人都有 64 GB。

⚠️ **V1 的打断入口，截至 2026-09-14 落地的是「推键式」，不是 VAD 自动打断**
（ADR-0007 初稿写的「VAD 检测到开口就掐掉 TTS」**还不是事实**）：用户按录音键，
程序**先掐掉正在播的回答再开麦克风**。**VAD 自动打断与双讲都没做**，
所以「误打断率」与「端到端打断延迟 <300 ms」两条出口判据**仍未测**（T3.4.2 收口）。

⚠️ **不要再用「2 GB 预算内做不到」解释为什么不做全双工。** 那个理由已经过期
（M2 起预算 ≤9 GiB，目标机 64 GB）。**真正的障碍是：目前没有「已验证覆盖中泰
且达到实时」的本地全双工候选**——注意措辞是「没有已验证的」，不是「不存在」：

- **VoiceChat 11B**：仅英文 + 许可证仅研究用途 + aarch64 上 RTF 1.13 达不到实时
- **PersonaPlex 7B**：`speech respond` 提供，**语言覆盖与性能都还没测**，不能当作不存在

理由说错会让后续决策走偏。PersonaPlex 那格没关掉之前，不要写「不存在方案」。

**不要把它描述成「体感接近全双工」**——它解决「说完才轮到我」，不解决「一边听一边想」，快速来回时能被感知到。

**v1 半双工有一个硬需求：回声消除（AEC）。** TTS 从扬声器出来会被麦克风收回去，ASR 会自触发。要么强制耳机，要么走 CoreAudio 的 Voice Processing I/O。这是真实工程量，不是细节。

### 未验证前提

1. SenseVoice 的准确率仅基于**单个样本**，样本量不足；且现有语料均为安静环境
2. 那支爱国者录音笔的能力 —— **机器没到手，`docs/ingest-design.md` §0 的「实时互斥」是条件式结论**。到手当天先做 §1.0 的刻画清单，它可能推翻整个坞站方案。
3. AEC 方案是否够用

## 产品意图（读 README 才能拼出的全貌）

AgentEar 要做的是一条**端到端、全本地**的语音 → 文字 → AI 工作流管线，用来替代市面上的 AI 录音卡 / 录音助手。两个不可让步的设计约束，决定了后续所有技术选型：

1. **隐私**：录音和转写**不经过任何第三方服务器**。这排除了云端 ASR（Whisper API、各家语音服务）作为主链路 —— 转写必须跑在 jason 自己的机器上（本地模型）。
2. **成本**：硬件用便宜的二手设备（一支闲鱼淘的爱国者 8G 录音笔，几十块），而不是买成品 AI 录音卡。

### 设想中的数据链路

```
录音设备（爱国者录音笔 / 手机 / MacBook 麦克风）
   ↓  传输层：WiFi 上的 HTTP/WebSocket（蓝牙仅用于配对与控制信令，不传数据）
本地机器（MacBook 优先）
   ↓
raw/audio/  ← 原始音频字节先落盘并 fsync，**早于 ASR**，不可重建
   ↓  本地模型做 ASR（失败可重试，不影响 raw）
derived/transcripts/  ← 文字，可从 raw 重算
   ↓  按语音里带的"标签"路由到不同分支
routes/ → 存入 knowledge base / 触发调研并出 report / 查日程并给回复 / 记 idea / 建任务
```

几个实现上要留意的点，都是 README 里已经定调的：

- **raw 优先**：原始**音频**必须在 ASR 之前就落盘，转写失败、VAD 切错段、进程 OOM 都不能毁掉唯一一份忠实记录。注意 README 的口述稿里 raw 是排在转写之后的，**以本文件为准**。
- **分支路由由语音内容里的标签驱动** —— 用户说"这是一个 idea"/"这是一个任务"，系统据此选下游分支。这意味着 ASR 之后需要一层意图/标签识别。
- **录音笔的可改造性未知**：那支爱国者录音笔还没到手，是否能加 WiFi、能否自动传输数据，都还没验证。任何依赖"录音笔能主动推送数据"的设计都是未经证实的假设；先做的是**手机 / MacBook 这条链路**（README 明确说了先完成这个）。
- **Mac mini 中转是后期选项**，不是第一版目标。

## 与工作区其他项目的关系

`ququ/`（FunASR 语音输入，Electron + 内嵌 Python）已经解决了"本地 ASR"这一段，`huniu/`（本地语音助手）和 `whisper.cpp/` 在同一片领域。

**这三个是参考资料，不是迁移源。** 可以复用的是**配置与思路层面**的东西：模型选择的经验、中文后处理、VAD 参数、踩过的坑。**不要继承它们的运行时** —— 尤其不要为了复用 ququ 而把内嵌 Python 环境搬进来，常驻守护进程背一个 Python 栈不划算（见上方技术栈一节）。任何"直接复用某段实现"的想法都要先过 `docs/benchmarks.md` 的实测。
