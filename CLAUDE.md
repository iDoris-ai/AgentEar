# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 当前状态：**v0.23.0 —— Qwen3-ASR 可选后端：设置窗口里选 0.6B / 1.7B 即下载（speech-swift v0.0.28 钉死 + HF 权重钉 commit，不随包），菜单栏切常驻；默认 ASR 仍是随包 SenseVoice，Qwen3 内默认 0.6B 逐次（小内存）**；v0.22.0 —— 语音边车端口被别的程序占着时自动换空闲端口拉起（不改 config.json，运行态记录 `run/talk-sidecars.json`，崩溃重启后按 pid 接回）；默认 TTS 端口 8765→8796；对话模式因边车不可用没说出来时用 `say` 念一句（同一原因 3 分钟一次）；v0.21.1 修「双击进对话模式后第一轮没回答」：边车端口被别的程序占着时报清楚是谁占的、不再误拉起；第一轮先等边车拉起完（最多 30s）再答**；v0.21.0 录音开始/结束提示音（单击一声、双击两声；开始高音、结束低音；设置里可关，默认开）**；v0.20.1 开机自启的实例崩溃后由 launchd 自动拉起（`KeepAlive.SuccessfulExit=false`，菜单退出 / SIGTERM 以 0 退出、不拉起）；v0.20.0 的开机自动启动 + 原生设置窗口仍**均未经真实点击验收**；M1/M2 已发布；**M3 通话链路已跑通，V1 打断定为推键式长期终态（v0.19.0 起才在守护进程里真正生效），不做 VAD 自动打断 / 双讲**

M1 完成；**知识库投递 + 全文检索默认开（v0.4.2）**；M2 理解层已发布（v0.4.0）但默认关；
**v0.6.0 加了通话链路（说一句答一句、可按键打断）**，**v0.5.0 加了可切换的 ASR 后端**（`--asr-backend` / `config.json` 的 `asr_backend`，
**默认仍是 `builtin`**）。**v0.23.0 起 `speech_swift`（Qwen3-ASR）在设置窗口里选了就下载**
（`src/qwen3.rs`，ADR-0010；见下方「Qwen3-ASR 可选后端」一节），不再要求用户自己装 brew。

### Qwen3-ASR 可选后端（v0.23.0，T3.5.6，jason 2026-09-26 拍板 ADR-0010 §6）

- **入口**：设置窗口「语音识别」下拉（SenseVoice / Qwen3 0.6B / 1.7B）+ 每个模型一行下载进度；
  菜单栏「Qwen3-ASR 常驻」开关。**选了没装的那档 → 下载，装好并过断网冒烟才切**
  （`tray::QWEN3_INTENT`，同泰语的 `THAI_INTENT`：下载期间改主意就不切）。
- **默认小内存**：Qwen3 内默认 0.6B、逐次调用；常驻空闲 `qwen3_idle_secs`（600 s，最短 60）后退出。
  老 speech_swift 用户（config 无 `qwen3_model` 键）迁移为 1.7B——**判据是键没出现过**。
- **钉死**：`qwen3::SPEECH_VERSION` + tarball sha；HF 权重 `resolve/<commit>/` + 逐文件 sha。
  **升级 speech = 改常量 + sha + 本机重跑 `--fetch-qwen3` 与 `--asr-bench`。**
- ⚠️ **所有 speech 进程都跑在 `sandbox-exec` 禁出站 TCP 里**：speech 找不到模型会静默下载
  （调研时当场下过 611 MB）。**只禁 TCP、不禁 UDP 是实测出来的**——禁 UDP 会让它判缓存无效、
  转而重试下载约 2 分钟（`qwen3.rs` 常量注释 + 测试钉住）。
- ⚠️ **常驻服务必须显式传模型名**（`Qwen3Model::server_model`），端口用系统分配的空闲端口
  （8765 被别家占过，固定端口迟早撞车）；`--transcribe` 这类一次性子命令结束时由
  `qwen3::ServerGuard` 收掉；被 kill -9 留下的孤儿下次启动按 `speech-server.pid` 收（先核对命令行）。
- ⚠️ **下载中被 kill，curl 子进程不会跟着死**（泰语下载也是）——`download::kill_curls()`
  在信号 / 菜单退出 / 重启三条路上调用。
- 实测（`docs/benchmarks-asr-zh-en.md` §9，合成语音、只量速度与内存）：常驻热态 0.6B 0.09–0.35 s、
  1.7B 0.19–0.69 s；逐次 1.8–2.5 s；0.6B ≈ 816 MiB、1.7B ≈ 2.49 GiB。
  **常驻形态的准确率没用 60×3 组重跑过**，不要写成「与 CLI 相同」。
- GGUF 路线（llama.cpp）另开 **T3.5.7** 评准确率，没做。

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
  `talk_tts_engine`("http")、`tts_url`(None→8796，v0.22.0 前是 8765)、`talk_timeout_secs`(60)、
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
  - **`/health` 还自报一个 `mlx` 块**（v0.18.0，`_mlx_memory()`）：
    `active_mb` / `cache_mb` / `peak_mb` / `cache_limit_mb` +
    **`cache_limit_set` / `cache_limit_api` / `cache_limit_error`**。
    ⚠️ **RSS 会被系统回收/压缩，看增长要看 MLX 自己报的数**（v0.17.0 查
    「38 GB」那条就是靠它）。实测（8bit，重启后）：
    `active 3072 / cache 0 / peak 3072` + `cache_limit_set true` /
    `api "mx.set_cache_limit"`。
    ⚠️ **但别把 `cache_limit_mb: 256` 当成事实**：MLX **没有
    `get_cache_limit()`**（实测 0.32.2：`mx.get_cache_limit` 与
    `mx.metal.get_cache_limit` 都不存在），所以那是**我们想要的值**，
    读不回来。**「设上了没有」只看 `cache_limit_set`**——v0.17.0 直接把 256
    当事实报出去，属于**报喜不报忧**（调用失败时照样报 256），v0.18.0 改掉了。
    `cache_mb` 长期为 0 证明的是**第二道闸**（每次合成后 `clear_cache()`）。
  - ⚠️ **`mx.metal.set_cache_limit` 已废弃**（mlx 0.32 运行时会打
    "will be removed in a future version. Use mx.set_cache_limit"）。
    所以 `install_cache_limit()` **优先调 `mx.set_cache_limit`**，`mx.metal`
    只作为老版本 mlx 的退路，并把**是哪个 API 生效的**记进 `/health`
    （哪天老名字被删掉，新名字还在，不会静默失效）。
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
- **句子级流水线（v0.10.0）= 「首字起播」中位数 4.86s → 2.80s**（n=8 / n=11）。
  LLM 侧走 SSE 流式（`stream: true`），TTS 侧**做不到流式**（§2.3），
  所以做的不是「音频流式」，而是**把「等 LLM 说完」和「等 TTS 合成」重叠**：
  出一句就合成一句，合成一段播一段。
  - **入口**：`talk::answer_and_speak_streamed`（守护进程与 `--ask` 都走它）；
    `--ask --no-stream` 强制走老的整句路径——**它是 A/B 测首字延迟的唯一开关**，
    也是流式出问题时的退路（不用改配置、不用重编译）。
  - ⚠️ **两个分布是重叠的**：流式最差的一次（5.55s）比整句最好的一次（3.27s）还慢。
    **只准报中位数与极值，不准报一个好看的区间**——本表第一版只采了 5 次流式样本
    （2.27–2.78s），那一批恰好落在好的一侧，多采几次就冒出 3.31/4.49/5.55。
    这是「同配置复测方差大于配置间差异」的**第二次实例**（第一次是音色那轮的半音数）。
  - ⚠️ **地板是「一次 TTS 合成」的约 2.4s 固定开销**（实测 2.4–6.5s，方差很大），
    不是 LLM（LLM 只占 0.35–0.78s）。**不要把这条优化说成「压到了 1 秒」**，
    也别把「首字变快」写成「一轮变快」——**总时长基本没变**，省的是等待。
  - ⚠️ **必须能退回老路，而且退回去必须有输出**：传输不支持流式 → 外层退回；
    流式请求运行期失败 → `reply_stream` 内部退回。**两条都实测踩过坑**：
    ① 流式 URL 漏拼 `/v1/chat/completions` → 404 → 被当成「不支持流式」；
    ② 内部退回时忘了再调一次 `on_delta` → **有文字、没声音**且日志正常。
    **倒退分支没有输出，比慢得多更糟。**
  - **切句**（`SentenceSplitter`）：句末标点 + **攒够 `MIN_SENTENCE_CHARS`（6）**
    + **标点不是当前缓冲区的最后一个字符**。最后这条是「按标点随手切」最容易
    翻车的地方（`3.` 切下去，`5 度` 就变成独立一句）。`.` 与 `。` 是两个字，
    版本号 `v0.9.0` 靠「前后都是数字」排掉。
  - **思考段要边收边扣**（`think_filtered`）：`strip_thinking` 是整段文本的函数，
    流式下必须「未闭合就一个字都不放」，否则用户会听见模型的内心独白。
    ⚠️ 它与 `strip_thinking` 对**未闭合**段的规则不同（那边不吞，见它的用例）——
    差别有理由：流式明确知道这个标签正在写。保守方向是「宁可少念一句」。
  - **会话多了一条流式路径**：`speaking_started`（进 `Speaking` 相、回答先留
    `None`）+ `note_reply`（播完补全文）。**不能用 `turn_ready(heard, None)` 代替**
    ——那个的语义是「这一轮没有回答」，会直接回 `Idle`，于是用户按键打断时
    `barge_in` 看不到正在播的相，`interrupted_playbacks` 统计不到，
    而打断延迟是 V1 的出口判据。
  - **打断**：`play_blocking` 被打断时返回的时长与「播完」长得一样，
    所以流式用**打断计数器**（`talk::interrupts()`）区分，打断后不再合成、
    不再播后面的句子。
- **TTS 量化档：默认 4bit，8bit 要显式要**（jason 2026-09-15 拍板）。
  `scripts/setup-talk.sh --tts-quant 4bit|8bit`（或 `AGENTEAR_TTS_QUANT`）、
  `scripts/serve-tts.sh` 同名的档位开关。权重体积（HF 实测）：
  **4bit 2.30 GB / 8bit 3.22 GB**；边车进程**峰值 RSS 4bit 约 2.4–2.5 GB /
  8bit 约 3.3 GB**（8bit 实测 3268 MB，与上一轮 3266 MB 复现）。
  - ⚠️ **默认档只能是 4bit**：发布的普通人电脑没这么大内存。
    这条**有测试钉住**（`tests/script_defaults.rs`），别人改默认档会红。
  - ⚠️ **实测没有检出 4bit 与 8bit 的输出质量差异**（差异在噪声里）。
    所以「为了更好的音质上 8bit」**目前买不到可测的东西**——
    想要更好的音色，走**换参考音频**那条路（`services/tts/make_voice.py`）。
    **不要把 8bit 说成「音质档」。**
  - ⚠️ **每个档位有自己的下载完成下界**（4bit 1500 MB / 8bit 2400 MB）：
    沿用同一个数，一份只下到一半的 8bit（约 1.6 GB）会被当成完整的放过去。
- **开箱默认音色库（v0.11.0）**：`assets/talk-voices/`（**入库**，1.9 MB，
  VoxCPM2 自举生成的参考音频，不是真人录音），`setup-talk.sh` 装进
  `<数据目录>/talk/voices/`（**已存在就不覆盖**——用户可能自己造过更好的）。
  - ⚠️ **这条修的是一个真 bug，不是锦上添花**：VoxCPM2 是**零样本克隆**，
    **没有参考音频就每次合成随机换一个说话人**（边车自己会告警：实测
    F0 极差 65%、音量差 4.5 倍）。而 v0.10.0 的句子级流水线把一次回答切成
    好几句、**每句各发一次请求**——于是「音色飘」升级成「一句话里换好几个人」。
    旧版 `serve-tts.sh` **压根没传 `--voices-dir`**，所以只要边车是被守护进程
    按配置拉起来的，就一直是这个状态（手工带 `--voices-dir` 起的那个是好的，
    两套起法行为不同，这也是它一直没被发现的原因）。
  - **两道兜底**：数据目录没有音色库时回退到仓库里那份（clone 出来就有，不用下载）；
    钉哪条音色要在库里真存在才钉——钉一条不存在的会让边车回 400，用户听到「没声音」。
- **向外动作的二次确认（v0.12.0，jason 2026-09-15 拍板：「所有向外输出的内容
  （写 Notion、发邮件……）都要二次确认」）**。
  - **判据是「会不会把内容送出这台机器」**（`commands::needs_confirm`）：
    `http_post`（Notion / webhook）**一定问**；`mailto:` **一定问**；
    打开网页（GET）**默认不问**，想让它也问就给那条指令加 `"confirm": true`。
    ⚠️ 「打开网页默认不问」是**有意的**：搜索是高频动作，每次都问会让用户
    把确认按成肌肉记忆——**那等于没有确认，还把搜索慢了一倍**。
  - **两条确认路径**（推键式下这两条必须共存）：
    **① 短按录音键**（<0.3s，里面不可能有语音）→ 确认；
    **② 按键后说「确认 / 对 / 好的」** → 确认。
    两者用「这一轮有没有转写」区分，不抢同一个动作。
  - **实现要点（每条都有用例钉住）**：
    - **念出来的内容 == 将要发出去的内容**：问句由 `Pending` 自己从
      `hit` + `text` 生成，执行时用同一个 `pending.text`，**不各算一遍**。
      念 A 发 B 的话，这个「二次确认」就是走过场。
    - **否定必须先判**：「不确认」「不要发」里**含着肯定词**——
      先判肯定就会把「别发」执行成「发」。这是唯一会真正出事的方向。
    - **带否定前缀的组合**（`不`/`别`/`没` + 肯定词）也要判成否定：
      光靠一张否定词表挡不住「不确认」。
    - **同意只在短答复里认**（≤7 字）：提到了「确认」的长句通常不是同意
      （「我刚才确认过了吗」「帮我确认一下明天的会」）。
      **拒绝不设长度限制**——这个不对称是有意的（误判成拒绝只是重说一遍，
      误判成同意就发出去了）。
    - **别的话一律作废待确认**，不偷偷执行；**空转写不算同意**
      （空 = 用户按键确认那条路，见上）。
    - **待确认会过期**（`command_confirm_secs`，默认 30s，最短 5s）：
      一个永远挂着的向外动作比没有更危险。**同时只留一条**，
      新的顶掉旧的并记日志——挂两条时用户说「确认」我们不知道他确认的是哪条，
      而猜错的代价是把错的东西发出去。
  - **入口**：菜单栏状态项加 `❓`（待确认在界面上必须可见，否则用户只会觉得
    「说了没反应」，其实三秒后它自己作废了）；CLI **`--run-command <文本>
    [--reply <答复>]`** ——它和守护进程走**同一个 `run_command_turn`**，
    不是模拟，专门为了无人值守地验确认逻辑（对着麦克风按键没法自动复现）。
    `--match-command` 也会报这条指令要不要确认。
  - ⚠️ **`mailto:` 的执行分支没有实跑验证过**（跑它会在你机器上打开邮件客户端）。
    已验证的是「它会问」以及 http_post 的完整链路。
- **动作回执必须是真的（v0.13.0）。** 起因是 jason 人工测试（2026-09-15）后
  问了一句「**你别骗我啊**」——查下去发现 `http_post` 分支有**两处报喜不报忧**：
  - `stdout(Stdio::null())`：**把对方的响应体丢了**。而 Notion / n8n 写入成功后
    回的正是新页面的 **URL**，所以用户问「写到哪了、网址给我看看」时，
    **系统手里根本没有那个答案**（不是不给，是没有）。
  - `let _ = child.wait();`：**不看退出码**。HTTP 401（token 过期）/ 500
    也照样报「已发送到 …」。**没写进去却说写进去了，比失败更糟。**
  现在：留着响应体并从中挑回执（`commands::summarize_response`：
  `url` → `link` → `id` → 正文里的第一条链接 → 截断原文），
  退出码非零就**报失败并带上服务端的解释**。
  - ⚠️ 用 **`--fail-with-body` 而不是 `-f`**：`-f` 在 HTTP 出错时**一个字都不输出**，
    于是最有用的一句被吞掉——而它往往正是「哪里配错了」。
    实测 Notion 式 401 的返回体现在能带出来：
    `没写进去（curl 退出码 Some(22)）：… 401 / {"message":"API token is invalid"}`。
  - `open_url` 也等了退出码，但**语义要说准**：它表示「有没有把 URL 交出去」，
    **不表示网页能打开**（实测 `open https://不存在的域名.invalid` 退出码是 0）。
    **不要宣传成「验证了链接可达」。**
- **通话回答正文进日志（v0.13.0 补）。** 流式改造（v0.10.0）时只留了「回答 N 字」，
  把正文丢了——原来整句路径是会打出来的。后果是**事后查不了**：
  jason 问「它刚才到底说了什么」，日志里只有字数。实测就是这么卡住的
  （他复述模型说过「我无法访问外部链接」，我们从日志读不出这句，
  只能重新打一遍模型才知道）。**这一行是可追溯性，不是调试噪音。**
- **边车与脚本**：`services/tts/backends.py`（新增 `SayBackend` + `VoxCpm2Backend`，
  HTTP 契约不变）、`services/tts/server.py`（`--backend {voxcpm2,say}`，**默认 voxcpm2**）、
  `scripts/setup-talk.sh`（按需下载两个模型，**不随包分发**）、
  `scripts/serve-talk-llm.sh`（`mlx_lm.server` + `--chat-template-args '{"enable_thinking": false}'`，
  端口 8794）、`scripts/serve-tts.sh`（用带 mlx-audio 的 venv 起 `server.py`）、
  `scripts/talk-e2e.sh`（端到端验收）。

### ⚠️ 音色「像机器人」的真凶（v0.15.0 修，2026-09-15）

**跟参考音频的质量、`instruct`、量化档都无关** —— 是边车把参考音频喂错了采样率：

```python
load_audio(path)   # mlx_audio 默认 sample_rate=24000 → 48k 参考被重采样到 24k
ref_audio=<数组>    # 裸 mx.array 不带采样率；模型拿到【路径】才会自己正确解码
```

→ **参考被按错误速率解释 → 克隆输出整体高一个八度**。实测同一条 48 kHz
参考（F0 142.4 Hz）：给路径 **147.7 / 133.7 / 150.5 Hz**（跟住），
给数组 **287.4 Hz**（高了一倍，jason 的原话「什么傻逼声音，还是机器人声音」）。

- 修法：`_load_ref` **返回路径字符串**，让模型按自己的契约解码 + 重采样。
  ⚠️ **这句原话错了，2026-09-19 实测更正、10-06 又纠正过一次根因**：
  「代价是每次合成重读一次参考 wav，相对合成时长可忽略」——把成本理解成了
  **文件 I/O**，实际成本是**计算**：实测 **2.8–2.9 秒**（同一进程内连打 5 次，
  同文不同文都一样，不会因重复调用而降），是短句合成里最大的单项开销。
  ⚠️ **但"过一遍参考文本"这句归因也是错的**——`_generate` 走的 Mode 3
  （只给 `ref_audio`）**根本不读 `ref_text`**（读源码 + 对照 `mlx_audio` 0.5.1
  的 `voxcpm2.py` `elif has_ref:` 分支核实过，两边都不碰这个参数）。真正的
  成本来自**参考音频本身的时长**：编码成长长的音频 latent 序列、过一遍
  `base_lm` 的初始前向——男声参考 98.7 秒 vs 女声参考 10.9 秒，预热耗时
  3.56s vs 1.05s，跟音频时长同方向，跟哪份 `ref_text` 无关。**详见 ADR-0009**，
  T3.4.13 已用 `services/tts/refcache.py` 缓存掉这笔重复计算（不传 `ref_text`，
  因为它本来就没被用过）。
- ⚠️ **这个 bug 的正确诊断路径值得记住**：先拿「直接调模型」当基准把变量隔离出来
  （三种模式全都跟住参考），才能确认问题在**我们自己这一层**；
  在那之前我连换了三轮参考音频（16 kHz 会话录音 → 48 kHz 规范录音 → 自举），
  **方向全错**。
- 结论：`instruct` 不是元凶（带 instruct + 正确路径仍然 133.7 Hz）；
  官方 **Mode 5「Ultimate Cloning」**（`ref_audio`+`prompt_audio`+`prompt_text`）
  相似度最高（150.5 Hz），是以后要再提相似度该走的路。

### 音色与方言的用法（jason 2026-09-15 定：只要男声/女声两条）

- 菜单 **说话** 四栏（v0.16.0 拆的，jason：「切换男女声音一个菜单，切换方言单独一个菜单」）：
  **音色**（扫目录，文件名即显示名）→ **语言**（中/英/泰，**默认普通话**）→
  **方言 / 口音**（粤/河南/四川/山东/东北/天津 + 英式/美式/加式）→ **语气**（4 项）。
  两栏写的是同一个 `tts_style`，只是把 12 项按用途切开。
- 所以「音色名字」= 参考音频的文件名。jason 机器上已重命名为 **`男声`**
  （他自己的声音，142.4 Hz）与 **`女声`**，其余两条移进 `voices/archive/`
  （边车 `glob("*.wav")` 不递归，所以归档即从菜单消失）。
- **音色 × 方言是任意组合**：参考决定音色、instruct 决定口音/语气
  （官方叫 Controllable Voice Cloning）。
- ⚠️ **方言的写法按官方文档改对了（v0.16.0）**，原来写错了：
  - **控制指令只写方言名**：`(四川话)`。原来写「用四川话说，地道四川口音」——
    正是 cookbook 的 "Keep Instructions Simple" 警告的那类啰嗦描述，
    实测**出来的根本不是四川话**。
  - **方言档不拼语气描述**（`DIALECT_STYLES` 走单独分支）。jason 认可的那版就是
    零多余描述。
  - **但方言的决定权在正文**：同一个 `(四川话)` 下，正文写 `幺儿，哈戳戳得你屋头来噶！`
    出四川腔，正文写普通话还是普通话。usage guide 的 Dialect tips 明说
    「write the target text in that dialect's own vocabulary」。
    **所以这一栏只是开关，不能把普通话变成方言**——不许写成「选了就会说」。
  - **实测**（2026-09-15，边车真实链路，音色=男声）：粤/河南/东北用文档原例，
    山东/天津我自己写词；**jason 验收：「效果不错」**。
- ⚠️ **非语言标记（`[Uhm]` / `[laughing]` / `[sigh]` …）是治「机械感」的正解**：
  官方 cookbook 的 "Extra Spice" 明说它们能让语音「less mechanical」。
  jason 实测**加了 `[Uhm]` 的那条比不加的好**（同一句话）。
  **还没接进链路**（要不要接、怎么接他说了算）。
- ⚠️ **方言正不正宗只能人耳验收**，声学指标只能证明「发音变了、音色没被带跑」。
  **不要在文档里写「四川话可用」**——那是没测过的结论。
- ⚠️ **不要把 jason 的声音打进公开的发布包**（`assets/talk-voices/` 里保持
  现有的自举参考；重命名只在本地）。

### ⚠️ 边车内存：上限与缓存回收（v0.17.0，2026-09-15）

jason 报「一个 voxcpm2 TTS 进程跑了快一小时、占了 38 GB，把我别的任务 OOM 掉了」。

- **我没能复现那个数字**：查的时候系统上已经没有任何 38 GB 的进程，
  当前边车 RSS 只有 0.3 GB（自报峰值 3.4 GB）；短文本循环 10 次稳定在 ~200 MB；
  **重负荷**（长文本 + 98 秒长参考）8 次后**稳定在 ~3.5 GB 且趋于平坦**。
- **但缺陷是真的，而且正好能造成无上限增长**：代码里**没有任何 MLX 缓存上限、
  也不清缓存**（`grep clear_cache` 当时是空的）。MLX 释放张量时**不把 Metal 缓冲
  还给系统**，只留在自己的池子里 —— 一个长期跑的服务在反复合成之后 RSS 只增不减。
  重负荷路径（长参考 + 长文本）每次要编码 98 秒音频的 latent，池子更容易涨。
- **修法两道闸**：`mx.metal.set_cache_limit(256MB)`（超了 MLX 自己回收）
  + **每次合成后 `mx.clear_cache()`**（含异常路径）。
  ⚠️ 256 MB 是折中：留一点够复用、又不会无限长；**设成 0 反而更慢**（每次重新分配）。
- ⚠️ **可观测性也补了**：`mlx_memory_mb()`（active / cache / peak）——
  `ps` 的 RSS 会被系统回收/压缩，**要看增长得看 MLX 自己报的数**。
- **同一个排查里抓到一个更严重的 bug**：我把音色改名成「男声/女声」之后，
  启动脚本里写死的默认音色 `female_zh_02` 不存在了 → 走 else 分支 →
  `echo "…$VOICE（…）"` —— **`$VOICE` 后面紧跟全角括号，bash 3.2 把多字节字符
  当成变量名的一部分**（`VOICE\xef\xbc\x88`），`set -u` 下直接
  「unbound variable」退出 → **整个边车起不来**（日志里只有一行 shell 报错）。
  修法：① 所有 `$VAR` 紧跟非 ASCII 时一律加花括号；② **音色不在库里时兜底用
  库里第一条**——失败方向要选对：**宁可音色挑错（能听见、能改），也不能没声音
  （静默失败）**。

### ⛔ 职责边界（jason 2026-09-15 拍板，见 **ADR-0008**）

**AgentEar 只做「听见」和「说出」。命令执行、确认 UI、回执展示、
初始化配置、凭据管理 —— 全部归宿主 Agent24。**

- **冻结的接口**：`agentear --match-command <文本> --json` → `agentear.proposal/1`
  （**只提出动作，绝不执行**）。字段表与两条硬约束见 ADR-0008 §3。
- **最基础的逻辑验证**：`scripts/agent24-standin.py`（假宿主：问 → 显示 → 确认 →
  **自己执行** → 自己展示回执）。它**故意不调 `--run-command`**。
- ⛔ **在这条线上停止开发**：不要再加初始化向导 / 凭据管理 / 回执界面 / 历史列表。
  已做出来的语音二次确认（v0.12.0）与回执读取（v0.13.0）**保留**——
  AgentEar 单独跑时仍然有用。
- ⏳ **等 Agent24 回答 6 个问题**（ADR-0008 §5）：事件通道、热键归属、
  麦克风/播放归属、配置归属、TCC 权限、回执留档。**没有答案之前不要再往下做。**

**⚠️ M3 这轮的诚实边界（不要美化、也不要外推）**：

> 本轮的实测明细在 **`docs/benchmarks-talk.md`**（**MLX 4bit 路径**：
> `VoxCPM2-4bit` + `MiniCPM5-2B-4bit`，2026-09-14）。
> ⚠️ 它与 `docs/benchmarks-m3.md` **不是同一条路径**——那份是 **speech-swift 的 bf16**
> （Swift CLI），**两份数字不能互相引用、也不能横比**（连「4bit 更省内存」都不成立：
> 2.5 GB 是 Python + MLX 进程峰值，1.54 GiB 是 speech-swift 进程峰值，口径不同）。

1. **天气那句话是本地写死的场景，不是天气接口**（ADR-0007 §4.6）。它的唯一作用是
   **证明 ASR→LLM→TTS 通**，不是产品能力；接真实天气源是集成方的事（R3 外壳层）。
2. **V1 只是打断式半双工**，解决「说完才轮到我」，**不解决「一边听一边想」**——
   **不许写「体感接近全双工」**。✅ **打断方式已拍板（jason 2026-09-19）：
   推键式是 V1 的长期终态，不补 VAD 自动打断、不补双讲**——不是过渡方案。
   **连带结论**：AEC 事件级自触发（T3.4.0 实测 VPIO 内容级 0/5、事件级仍
   5/5 残留单字）**不再是 V1 的必做项**——推键式不依赖"系统听出你在说话"，
   这个残留跟打断体验无关，只在将来真要重开 VAD 自动打断时才会再成为前置。
   **「误打断率」这条判据对推键式不适用**（不存在"系统误判"这回事），
   **不用测**；「端到端打断延迟 <300ms」在推键式下改指"按键到声音真的停"——
   **实测了其中一段**（`stop_playback()` 到播放线程确认返回，3–24ms，
   见 `docs/agent/tasks.md` T3.4.2），**但那不是完整的端到端**：键盘事件
   分发、channel 到主循环的延迟、扬声器真正静音的设备延迟都不在测量范围内，
   仍未测，不能假设已经达标。
   ⚠️ **2026-09-20 修了一个更根本的 bug**：在这条测量补上之前，
   `main.rs::worker()` 的接线让"按录音键打断"在真实守护进程里根本不会
   被触发——对话模式的播放是**同步**跑在唯一处理按键事件的那个线程上，
   播放没结束那个线程就回不到读按键的地方，`stop_playback()` 因此从未
   在真实播放中被调用过。已改成把播放 spawn 到独立线程，让按键路径重新
   可达；上面"3–24ms 内部区间"这条测量本身没变（它测的是原语，不是这条
   接线），但接线修好之后它测的那个原语终于是真的会在生产环境里被用到的。
   **这个修法没有在真实守护进程上用物理键盘实测过**，只过了编译 + 全量
   单测 + 代码走读，见 `docs/agent/tasks.md` T3.4.2。
   ⚠️ **`StreamCheckpointPolicy`（ADR-0007 §4.5）不是"不需要了"**：
   推键式录音有明确起止点，但提交前的录音（最长 300 秒）崩溃时会整段丢失，
   比它想解决的"有界丢失"更差——这是一次真实犯过、又撤回的错误结论，
   待 jason 决定怎么处理，不是已关闭的问题。
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
cargo test                                    # 300 passed / 0 failed / 7 ignored（294 条单测 + 6 条钉脚本默认值的集成测试；ignored 7 条：4 条要边车、1 条要联网、1 条要能出声的环境、1 条要真实 whisper-cli+已下载泰语模型+macOS say）：提交协议、崩溃语义、token 过滤、i18n、下载协议、知识库投递、通话会话状态机、语音指令表、泰语 code-switch 提示词
./target/release/agentear                     # 守护进程，Ctrl+Shift+R 开始/停止录音
./target/release/agentear --transcribe x.wav  # 离线转写，不占麦克风，用于验证 ASR 链路
./target/release/agentear --diagnose          # 环境自检：权限、音频设备、ASR 依赖
./target/release/agentear --debug-keys        # 打印每个修饰键事件，排查按键问题
./target/release/agentear --fetch-thai        # 预下载泰语模型（574 MB），只装不改识别语言
./target/release/agentear --fetch-qwen3 0.6b  # 预下载 Qwen3-ASR（运行时 99 MB + 0.6B 713 MB / 1.7B 2.47 GB），只装不切
./target/release/agentear --asr-bench x.wav --runs 5   # 同进程轮流切 SenseVoice / Qwen3 两档 × 两形态并计时（会改 config，配临时 AGENTEAR_DATA）
./target/release/agentear --transcribe x.wav --lang th   # 不改配置试泰语链路
./target/release/agentear --classify "这是一个 idea"      # 给一段文字分类（评测脚本也走这条）
./target/release/agentear --replay-kb                    # 从 routes/ 全量重建 kb/，幂等，可反复跑
./target/release/agentear --say "你好"                    # 只测 TTS：合成 + 播放（跳过 ASR 和 LLM）
./target/release/agentear --cue start                    # 试听录音提示音（start / upgrade / end / start-conversation / end-conversation）
scripts/cue-asr-check.py --agentear target/release/agentear --out docs/data/cue-asr-2026-09   # 提示音混进录音后 ASR 会不会被带偏
./target/release/agentear --ask "今天天气怎么样"           # 文字 → LLM → TTS → 播放（跳过 ASR），**走句子级流水线**
./target/release/agentear --ask "…" --no-stream            # 同一个入口走**老的整句路径**（A/B 测首字延迟用）
./target/release/agentear --talk-turn q_zh.wav --lang zh   # **完整一轮**（ASR→会话→LLM→TTS→播放），走守护进程同一条路径
./target/release/agentear --commands                       # 列出语音指令表（默认给一份开箱表）
./target/release/agentear --add-command "记一下"            # 加一条指令（--action builtin|open_url|http_post）
./target/release/agentear --add-command-wav my.wav          # **录一句**定义指令：先 ASR 成短语再写进表
./target/release/agentear --match-command "搜索 talk.rs"    # 干跑：只报命中/槽位，**不执行**任何动作
./target/release/agentear --match-command "发邮件给 a@b.com" --json   # **给宿主用的契约**（agentear.proposal/1）
scripts/agent24-standin.py "发邮件给 a@b.com"   # 假宿主：验证「提出 → 确认 → 宿主执行 → 回执」
./target/release/agentear --run-command "记到notion 明天要测 AEC"        # 走完整流程（会问，不执行）
./target/release/agentear --run-command "记到notion 明天要测 AEC" --reply 确认   # 两轮：先问、再确认，才执行
cargo test -- --ignored stop_playback         # 打断机制：真掐掉一段 5s 音频（需要能出声的环境）
scripts/bundle.sh                             # 打 .app bundle → dist/

# 通话链路（M3）的两个边车 + 端到端验收，都要单独起，都**不随包分发**：
scripts/setup-talk.sh                         # 首次：下 MiniCPM5-2B-4bit（LLM）与 VoxCPM2-4bit（TTS）+ 默认音色库
scripts/setup-talk.sh --tts-quant 8bit         # 同上但 TTS 用 8bit（3.22GB 权重 / 约 3.3GB 内存，要自己显式要）
scripts/serve-talk-llm.sh                     # 每次：LLM 边车，默认 127.0.0.1:8794
scripts/serve-tts.sh                          # 每次：TTS 边车，默认 127.0.0.1:8796（--backend voxcpm2；v0.22.0 前是 8765）
scripts/talk-e2e.sh --text '今天天气怎么样' --lang zh   # 全链路：ASR → LLM → TTS → 音频文件

python3 -m unittest discover -s services/tts -p 'test_*.py'   # 46 passed（TTS 边车，CI 里也跑）
# ⚠️ 响度那 3 条要 numpy：`normalize_loudness` 没有 numpy 时是恒等函数，
#    于是第一条报「RMS 差 10 倍」（像真 bug）、另两条假绿。已有 skip 守卫，
#    但**别把 skip 当成通过**——本机三个解释器都有 numpy，正常应当 46 passed。
```

**release notes 在 `docs/releases/`**（一版一个 `v0.x.y-notes.md`，内容就是 release 正文）。
⚠️ 2026-09-16 才从 `vendor/models/talk/release/` 搬进来——那个目录**被 gitignore**，
所以新克隆的仓库里根本没有历次 notes。**权威源仍是 GitHub release**
（资产、发布时间、tag 指向只有那边有），这里放正文，为的是离线可查、可 diff。
`docs/releases/README.md` 里写了来龙去脉。

⚠️ **`scripts/measure-f0.py` 同理**：它原来是 `vendor/models/talk/measure_f0.py`，
而 `services/tts/backends.py` 与 `services/tts/make_voice.py` **引用它的数字当实测来源**
（「不归一时 RMS 差 4.54 倍」）——**被引用的实测工具不能躺在 gitignore 的目录里**，
否则新克隆的仓库里那些数字**没有可复现的来源**，而编译/测试/CI 都不会报错。
这条现在有测试钉住（`tests/script_defaults.rs::cited_measurement_tools_are_in_the_repo`）。
**凡是「源码里引用了某个文件」，那个文件就必须在仓库内。**

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

### 这台机器上的 Python（jason 2026-09-15 交代：「默认 python 是 3.11.9」）

- **交互式 shell 里** `python3` = **pyenv 的 3.11.9**（`~/.pyenv/version`，
  `eval "$(pyenv init - zsh)"` 在 `~/.zshrc` 里）。他说的就是这一条。
- ⚠️ **非交互环境里完全不是这个**：`~/.zshrc` 不加载 → pyenv 不在 PATH →
  `python3` 落到 `/usr/bin/python3` = **Xcode 的 3.9.6**，而 **`python3.11` 直接找不到**。
  受影响的是 launchd / GUI 启动的进程、CI、以及 agent 自己起的 shell。
  **3.9.6 连 `import mlx_lm` 都做不到**（mlx 要 3.11+），
  所以「裸调 `python3`」在本仓库是一条会安静走错解释器的路。
- **结论：脚本一律不许裸调 `python3`。** 现状是对的——
  `serve-tts.sh` / `serve-talk-llm.sh` 都钉 `$VENV/bin/python`，
  而 `~/.agentear/llm/venv` 是 **uv 的 cpython 3.11.15**，与 pyenv 无关、也不受它影响。
- **`setup-talk.sh` 挑解释器要过版本闸**（2026-09-15 加）：
  早先只判「这个名字存在吗」，于是 `python3.14`（Homebrew 3.14.6，**未验证**）
  会排在 3.11 前面被选中；而 `python3` 根本没进候选，
  在 pyenv-only 的机器上会直接报「找不到 Python 3.11+」——明明有可用的。
  现在每个候选都跑一次 `sys.version_info >= (3, 11)` 才算数，低于的打一行日志跳过。
  **这条有测试钉住**（`tests/script_defaults.rs::the_interpreter_picker_checks_the_version`）。
- 本机实测（两种 shell 都验过）：非交互 → **python3.12**（Homebrew 3.12.13）；
  交互 → 同样选 3.12（3.12 在清单里排在 3.11 前面），所以**pyenv 那份 3.11.9
  实际上没被 setup 用到**。别以为「默认 python 是 3.11.9」= 边车跑在 3.11.9 上。

数据落在 `~/.agentear/`（`AGENTEAR_DATA` 可覆盖）；ASR 二进制与模型在 `vendor/`（`AGENTEAR_VENDOR` 可覆盖，**不入库**）。

**换机器 / 新环境初始化 / 改动怎么进主干 → [`docs/dev-setup.md`](docs/dev-setup.md)**（含「不在 git 里的东西」清单、**必须由 PR-Daemon 审**的规矩、分支保护实际配置）。

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
- **不需要流式 raw 语义**（快捷键的按下/再按天然给出段边界，每次录音就是一个有头有尾的文件对象，可以直接复用 `ingest-design.md` §3.3 的提交协议——⚠️ 但"有头有尾"只决定了段的边界，不等于提交前就零丢失；录音进行中崩溃仍会丢掉整段未提交的部分，这条风险和 V1 通话共用同一套机制，详见 ADR-0007 §4.5、§8 第 7 条）

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

✅ **V1 的打断入口已拍板（jason 2026-09-19）：「推键式」，长期终态，不是过渡方案**
（ADR-0007 初稿写的「VAD 检测到开口就掐掉 TTS」**不再是要做的事**）：用户按录音键，
程序**先掐掉正在播的回答再开麦克风**。**不补 VAD 自动打断、不补双讲**——
这不是妥协，是 V1 的完整定义。「误打断率」这条判据对推键式不适用，不用测；
「端到端打断延迟 <300ms」**只实测了其中一段**（`stop_playback()` 到播放
线程确认返回，3–24ms），完整的端到端（含键盘事件分发、音频设备静音延迟）
仍未测。⚠️ **2026-09-20 之前，这个打断在真实守护进程里根本不会被触发**
（播放同步跑在唯一处理按键的线程上，把它自己卡住了）——已修成异步，
但没有物理键盘实测过，详见 `docs/agent/tasks.md` T3.4.2。

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
