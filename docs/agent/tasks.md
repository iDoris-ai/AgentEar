# AgentEar 任务台账 — Task

> 前置：[`roadmap.md`](roadmap.md)（M→F）·[`architecture.md`](architecture.md)（边界）·[`spec.md`](spec.md)（数据模型）
> 每个 Task 自包含，可独立开发与验收。**验收标准必须可机器验证**。
> 状态：BACKLOG · READY · IN_PROGRESS · BLOCKED · PR_OPEN · CHANGES_REQUESTED · APPROVED · DONE

**本仓库的强制规矩（每个 task 都适用）**：
- 不许直推 main。单主干仓库，PR 直接开向 main，合并要带 `--allow-trunk`。
- 提 PR 前跑**一轮** codex 自我挑战并修完。**一轮即可，不要连刷多轮**——
  jason 2026-09-02 明确要求过。
- 改了 `scripts/*.sh` 必须跑 `scripts/lint-shell.sh`（bash 3.2 的全角字符坑）。
- LLM 边车要在跑（`scripts/serve-llm.sh`），否则 F2.1/F2.2 的验收命令必失败。

---

## F2.1 — 项目术语表

### T2.1.1 术语表数据结构与默认表  `DONE`
- **优先级**：high
- **目标**：让纠错时有一份本项目固定词汇的清单可用，而不是每次从上下文猜。
- **开发范围**：按 spec.md §1 实现 `~/.agentear/terms.json` 的读写：默认表内置、
  首次启动写入、已存在不覆盖、解析失败退回默认表并记日志。把术语表拼进
  `correct.rs` 的提示词。
- **明确不做**：不做逐字符替换（会误伤用户真的在说 road 的情况）；不做菜单编辑界面。
- **依赖**：无
- **交付物**：`src/terms.rs`；`correct.rs` 注入术语表；默认表覆盖 M0 已知的全部错例。
- **验收命令**：`cargo test terms` 全绿，且包含「文件损坏退回默认表」「已存在不覆盖」两条用例
- **涉及文件**：`src/terms.rs`、`src/correct.rs`、`src/main.rs`
- **风险/回滚**：术语表进提示词会增加 token 数、拉长纠错耗时。若实测超过 15 秒需在 PR 里说明并给出取舍。
- **证据**：PR #11（合并 commit af1f695）。提示词格式踩了五版才对，
  详见 PR 描述；`#[ignore]` 集成测试 `real_road_and_id_survive` 是关键——
  没有它前三版都会带着「已修复」的标签合并。

### T2.1.2 长文回归：ro 必须还原成 raw 而不是 repo  `DONE`
- **优先级**：high
- **目标**：把 `docs/benchmarks-m2.md` §8.1 那次真实失败钉成回归测试，防止复发。
- **开发范围**：用 `spike/audio/sample02.wav` 的转写文本做长文用例，断言纠错后
  `raw` 出现且 `repo` 不出现；同时断言其余已知正确的纠正没有退化
  （MacBook / knowledge base / 24 小时 / Mac mini / WiFi）。
- **明确不做**：不重新跑 ASR（用已有转写文本当输入，测的是纠错层不是 ASR）。
- **依赖**：T2.1.1
- **交付物**：`src/correct.rs` 的长文回归测试；`docs/benchmarks-m2.md` §8.1 补上「已修复」的实测结果。
- **验收命令**：`cargo test correct` 全绿；`./target/release/agentear --transcribe spike/audio/sample02.wav` 的输出里出现 `raw 的目录`
- **涉及文件**：`src/correct.rs`、`docs/benchmarks-m2.md`、`tests/fixtures/sample02-asr-raw.txt`
- **证据**：fixture 是一次真实转写的快照（不重跑 ASR，否则 ASR 的随机性会以
  和纠错无关的方式让测试失败）。判据里踩过一个坑：`!contains("repo")` 会被
  `report` 误触发。

### T2.1.4 长文分句后逐句纠错  `DONE`
- **优先级**：high
- **目标**：让术语纠错在长录音上也可靠。
- **背景（2026-09-03 实测）**：术语表在**短句**上稳定有效，
  但 700 字长文连跑 5 次 0 次通过——模型倾向于原样输出。
  T2.1.2 当时报「已修复」是基于单次通过。见 `docs/benchmarks-m2.md` §8.1。
- **开发范围**：按句号/问号/换行分句，逐句送纠错再拼回；
  保持段间空格规则与现有 `join` 一致；给分句加长度上限（过短的合并）。
- **明确不做**：不改术语表内容；不做并发（串行足够，且并发会让边车排队）。
- **依赖**：无（术语表已在）
- **交付物**：分句纠错 + `longform_regression_ro_becomes_raw_not_repo` 转为通过
- **验收命令**：`cargo test --release -- --ignored longform` 连跑 3 次全绿
- **涉及文件**：`src/correct.rs`
- **风险**：逐句调用会让长录音的纠错耗时进一步上升（现在 700 字已经 10 秒）。
  需要实测并在 PR 里给出数字；若不可接受，考虑按段落而非句子切分。

### T2.1.3 术语表可由用户扩展  `DONE`
- **优先级**：mid
- **目标**：jason 能自己加词，不用改代码不用重新编译。
- **开发范围**：菜单加一项「打开术语表」（用 `open` 调系统编辑器）；
  每次纠错时读文件（不缓存到进程生命周期，改完下次录音即生效）；
  文件读失败时退回上一次成功加载的表。i18n 三语文案。
  ⚠️ **codex 已确认「退回上次成功表」目前没实现**（T2.1.1 评审 Medium 5）：
  编辑器截断重写的瞬间读到半文件，会突然退回内置默认表而不是用户的表。
  这个 task 必须真的做掉它，不是顺带。
- **明确不做**：不做图形化编辑器；不做热重载监听（每次读文件足够，几 KB）。
- **依赖**：T2.1.1
- **交付物**：菜单项 + 三语文案 + 读取策略
- **验收命令**：`cargo test i18n` 全绿（新 Key 的三语覆盖由现有测试强制）
- **涉及文件**：`src/tray.rs`、`src/i18n.rs`、`src/terms.rs`

---

## F2.2 — 标签识别与路由

### T2.2.1 重新定义 8 类标签边界并建评测集  `DONE`
- **优先级**：high
- **目标**：把 M0 基准判错的两条从「模型不行」还原成「定义不清」，给出可判别的边界。
- **开发范围**：按 spec.md §2 的判别依据，为 8 个类各写 2 条以上正例；
  把 M0 那两条判错用例（开会讨论 / 帮我查日程）连同判定理由写进评测集；
  扩充 `spike/m2_bench.py` 的标签用例到至少 16 条。**这一步不写产品代码。**
- **明确不做**：不改 `src/`；不做 few-shot 注入（那是 T2.2.2）。
- **依赖**：无（可与 F2.1 并行）
- **交付物**：`docs/agent/label-taxonomy.md`（定义 + 判别依据 + 全部用例）；扩充后的评测脚本
- **验收命令**：`~/.agentear/llm/venv/bin/python spike/m2_bench.py --url http://127.0.0.1:8793` 能跑完并给出扩充后的分数
- **涉及文件**：`docs/agent/label-taxonomy.md`、`spike/m2_bench.py`
- **风险**：Q1（开会讨论该判 note 还是 journal）是产品决策。按 spec.md 先做，**在文档里标为待 jason 确认，不当定论**。

### T2.2.2 标签识别实现  `DONE`
- **优先级**：high
- **目标**：转写之后拿到一个一级标签，失败时降级为 unknown 而不是中断。
- **开发范围**：`src/label.rs`：调边车、few-shot 用 T2.2.1 的用例、只取最后一行非空
  （同 correct.rs 的判据）、解析成封闭枚举、非法值落 unknown。
- **明确不做**：不做二级标签抽取；不做路由落盘（T2.2.4）。
- **依赖**：T2.2.1
- **交付物**：`src/label.rs` + 单元测试（含「模型返回垃圾 → unknown」「边车不可达 → unknown」）
- **验收命令**：`cargo test label` 全绿
- **涉及文件**：`src/label.rs`、`src/main.rs`

### T2.2.3 显式标记优先于模型推断  `DONE`
- **优先级**：high
- **目标**：用户说「这是一个 idea」就必须按 idea 走，模型推断不得覆盖。这是架构边界 B5。
- **开发范围**：中英文显式标记的识别规则（可被单元测试覆盖，**不靠模型判断**）；
  识别到就直接定标签并标记 `label_source=explicit`，跳过模型调用。
- **明确不做**：不做模糊匹配（「我觉得这算个想法吧」不算显式）。
- **依赖**：T2.2.2
- **交付物**：显式标记解析 + 测试（含中英文各 3 种表述、以及「不该被误判为显式」的反例）
- **验收命令**：`cargo test explicit` 全绿
- **涉及文件**：`src/label.rs`

### T2.2.4 routes 落盘  `DONE`
- **优先级**：high
- **目标**：每次转写产出一条 `routes/` 记录，只增不删，可重算。
- **开发范围**：按 spec.md §3 的 JSON 结构落盘到 `routes/yyyy-mm/`；
  写入走「先写临时文件再 rename」（同 store.rs 的既有做法）；
  `delivery.state` 初始为 pending。
- **明确不做**：不做实际投递（ADR-0003 的适配器，不在本轮）；不做重试队列。
- **依赖**：T2.2.2
- **交付物**：`src/store.rs` 的 routes 写入 + 测试（含「标签识别失败仍落盘且 label=unknown」）
- **验收命令**：`cargo test routes` 全绿
- **涉及文件**：`src/store.rs`、`src/main.rs`

### T2.2.5 标签基准回归到至少 7/8  `DONE`
- **优先级**：mid
- **目标**：证明重新定义边界 + few-shot 确实解决了 M0 那 6/8。
- **开发范围**：在 T2.2.1 的扩充评测集上跑分，记录结果到 `docs/benchmarks-m2.md`；
  **达不到 7/8 就分析原因**：是定义仍不清（回 T2.2.1）还是模型能力不足（记为待决问题）。
- **明确不做**：不为了刷分而把用例改简单。
- **依赖**：T2.2.2、T2.2.3
- **交付物**：`docs/benchmarks-m2.md` 新增标签回归一节，含逐条结果与失败分析
- **验收命令**：评测脚本跑出 ≥7/8；未达标时 progress.md 里有明确的待决问题记录
- **涉及文件**：`docs/benchmarks-m2.md`、`spike/m2_bench.py`

---

## F3.1 — TTS 方言可行性摸底

### T3.1.1 候选调研：闽南语与粤语的本地 TTS  `DONE`
- **优先级**：high
- **目标**：回答「有没有」，不是「哪个好」。
- **开发范围**：调研本地可跑的 TTS 方案，每个候选记录：支持哪些方言、
  能不能纯本地跑（不依赖云）、运行时形态（是否又要背一个 Python 边车）、
  模型体积、许可与再分发义务、有没有公开的质量证据。
  **至少覆盖普通话、粤语、闽南语三档，泰语与英语一并记录。**
- **明确不做**：不实际下载模型（那是 T3.1.2）；不做质量主观评价。
- **依赖**：无（可与 M2 并行，一个改代码一个出文档）
- **交付物**：`docs/tts-survey.md`，每个候选一行，三栏不许留空：本地可跑 / 许可 / 证据来源
- **验收命令**：`test -s docs/tts-survey.md && grep -c '闽南\|台语\|Hokkien' docs/tts-survey.md`（至少命中 1 条）
- **涉及文件**：`docs/tts-survey.md`

### T3.1.2 实测最有希望的候选  `DONE`
- **优先级**：mid
- **目标**：把「据说支持」变成「实际跑通/跑不通」。
- **开发范围**：从 T3.1.1 选 1–2 个最有希望的，实际下载并合成一句测试语音；
  记录冷启动、RTF、内存、产物是否可听。**跑不通也是结论**，照实记。
- **明确不做**：不做多候选横比（那是选型，等 jason 拍板方向之后）。
- **依赖**：T3.1.1
- **交付物**：`docs/tts-survey.md` 补上实测数据；音频样本放 `spike/audio/tts/`（不入库）
- **验收命令**：`docs/tts-survey.md` 里出现「实测」小节且含具体数字
- **涉及文件**：`docs/tts-survey.md`

### T3.1.3 出 ADR-0005 草稿  `DONE`
- **优先级**：mid
- **目标**：给 jason 一份能据以拍板的材料，**不替他拍板**。
- **开发范围**：按现有 ADR 格式写 `docs/decisions/0005-tts-selection.md`，
  状态标「草稿，待拍板」；写清候选、证据、局限、以及**如果闽南语确实没有可用方案，
  M3 的三条可能走向**（只做普通话 / 推迟 M3 / 其他），每条列出代价。
- **明确不做**：**不选定方案**。这是产品决策，属于无人值守纪律里「不猜产品决策」那一条。
- **依赖**：T3.1.2
- **交付物**：`docs/decisions/0005-tts-selection.md`（草稿状态）；`docs/milestones.md` 的 M3 一节引用它
- **验收命令**：`grep -q '草稿' docs/decisions/0005-tts-selection.md`
- **涉及文件**：`docs/decisions/0005-tts-selection.md`、`docs/milestones.md`
- **风险**：这个 task 完成后会产生一个 BLOCKED 项（Q2：M3 怎么走），带着问题清单停下来问，符合无人值守纪律。

---

## F2.3 — 从跟进账本提升上来的正式 task

这几条原本在 `followups.md` 里，但规模已经超出「批量小修」——
按 pilot 的规矩提升为正式 task，单独走流程。

### T2.3.1 把 HTTP 调用抽象成可注入 transport  `DONE`
- **优先级**：high
- **目标**：让 `correct` / `label` 的错误分支能被**确定性测试**覆盖。
- **背景**：FU-4 + FU-11（codex 两次提到）。现在「垃圾 JSON / 缺字段 /
  HTTP 500 / 空 content / finish_reason=length / 批边界空白 / 中间批失败」
  这些分支**只能靠真实边车碰运气覆盖**，而它们恰恰是最容易出问题的地方。
- **开发范围**：定义一个 `Transport` trait（或函数指针），生产实现走 curl，
  测试实现返回固定响应；为上述每个分支写测试。
- **明确不做**：不引入 HTTP 客户端依赖（curl 子进程的选择不变）。
- **验收命令**：`cargo test transport` 全绿，且覆盖列出的每个失败分支
- **涉及文件**：`src/correct.rs`、`src/label.rs`

### T2.3.2 让基准脚本直接调用生产分类路径  `DONE`
- **优先级**：mid
- **目标**：消掉「基准 18/18 而生产 17/18」那个未定位的差异（现已同为 18/18，
  但差异的根因仍未知，两套代码还在各跑各的）。
- **背景**：FU-6 + FU-14。基准用的解析器更宽松、标签顺序也不同，
  两边的数字本来就不该拿来互相印证。
- **开发范围**：让 `spike/m2_bench.py` 调 `agentear` 的某个子命令
  （或直接跑 `cargo test --ignored`），不再自带一份提示词和解析器。
- **验收命令**：基准与生产报出同一个分数
- **涉及文件**：`spike/m2_bench.py`、可能需要给二进制加一个 `--classify` 子命令

### T2.3.3 curl 子进程的父进程墙钟超时  `DONE`
- **优先级**：mid
- **目标**：`--max-time` 只覆盖传输阶段，卡在 stdin 交互时不保证。
- **背景**：FU-5（codex Medium）。长录音分批后一次纠错要起 N 个子进程，
  任何一个卡住都会拖住整段。
- **开发范围**：父进程侧的 deadline + 到点 kill/wait；stdin 写入与
  stdout/stderr 排空并发进行（现在是先写完再读，有背压窗口）。
- **验收命令**：新增测试模拟一个不读 stdin 的子进程，断言父进程在 deadline 后返回
- **涉及文件**：`src/correct.rs`、`src/label.rs`

### T2.3.4 last-good 持久化  `PR_OPEN`
- **优先级**：low
- **目标**：进程重启后 last-good 缓存为空；若编辑器截断文件后崩溃或断电，
  启动加载只能退回内置默认表。
- **背景**：FU-12（codex Low）。
- **开发范围**：每次成功解析并清洗后，原子更新 `terms.json.bak`；
  主文件坏且内存缓存为空时，验证并读备份。**解析失败时绝不更新备份**。
- **验收命令**：`cargo test terms` 新增「主文件坏 + 无内存缓存 → 读备份」用例
- **涉及文件**：`src/terms.rs`

---

## F2.4 — 让 M2 真的能用（**待 jason 拍板，不擅自开工**）

2026-09-03 他问「工具做好了没」时暴露的问题：**M2 的 3421 行代码全在 main 上、
20 个 PR 全绿，但最新发布版 v0.3.1 一行都不包含**；就算从源码构建，
要用术语纠错还得手动跑 `setup-llm.sh` 下 7.8 GB、再开一个终端挂着
`serve-llm.sh`。**那不是工具，是开发环境。**

下面三条是我的建议，但**每一条都涉及架构或产品决策，必须他点头才动**：

### T2.4.1 守护进程管理边车生命周期  `DONE`
- **待决**：Rust 守护进程要不要负责拉起/守护 mlx-dspark 子进程？
  这会改变 ADR-0002 划的那条线——那里说边车是「独立进程、可独立重启、
  独立崩溃」，而自动拉起意味着守护进程要管它的生死。
  **算不算违反那条约束，是架构决策。**
- **若拍板做**：菜单显示边车状态；启动时按需拉起；崩溃后重试有上限；
  退出时不留孤儿进程。

### T2.4.2 发 v0.4.0，带上 M2  `PR_OPEN`
- **jason 2026-09-03 拍板**：等 T2.4.1 做完再发；**不打包 7.8 GB 模型**，
  「最差手动启动或者静默失败继续」，未来适配上下游组件协作。
- 因此 `llm_start_command` 默认为空 = 只连不拉；用户要自动拉起就自己配路径。

### T2.4.3 routes 的下游投递  `DONE`
- **jason 2026-09-03 拍板**：先做投递，三件事一起做完；
  L2 索引（rusqlite + SQLite FTS5）排在这三件之后，不并进来。
- 落地：
  1. `KbSink` trait + `FileSink`（`src/kb.rs`）——渲染 ADR-0003 §3.3 的
     front matter，按 front matter 的 `id` 去重，正文改了也不会堆出第二篇
  2. 重试队列 `routes/.pending/`（`src/deliver.rs`）——**入队在投递之前**，
     所以「投递中途被杀」不会留下没人管的记录；`MAX_ATTEMPTS = 10` 后放弃并标 `failed`
  3. `--replay-kb` 从 `routes/` 全量重建，幂等，可反复跑
- 两处偏离 ADR-0003 §3.3 的地方已回写进 ADR §7.1：`journal` 走 `kb/private/`
  独立子树；`unknown` / `command` 不投递；`kb/index/tags.md` 推迟到 L2。
- **没做的**：组织档（memos）适配器、L2 索引、L3 行动层。

### T2.4.4 L2 索引  `DONE`
- `src/index.rs`：rusqlite + SQLite FTS5，`--search` / `--reindex`。
- **分词方案实测选出**（同语料同查询，2026-09-03）：

  | 查询 | unicode61 | trigram | **unigram（选中）** |
  |---|---|---|---|
  | 「录音」2 字 | 0 | 0 | **2** |
  | 「录音 设备」跨词边界 | 0 | 0 | **1** |
  | 英文前缀 `know*` | 1 | 0 | **1** |
  | 英文任意子串 `know` | 0 | **1** | 0 |
  | 索引体积 | 1.0× | 3.5× | 1.55× |

  **换分词方案要重跑这张表**，别凭印象换。
- 前置假设已证实：`rusqlite` 的 `bundled` 随包 SQLite **3.50.2**，
  FTS5 / unicode61 / trigram / porter 全部可用。
- 硬约束满足：索引能从 `kb/` 全量重建，`derived/index.sqlite` 可随时删。
- **踩过的坑**：只给正文切分、忘了标签 → 中文标签整个搜不到，
  而英文标签恰好没事，所以只测 `esp32` 时溜过去了。

### T3.3.1 TTS 模块（三语，交给 Arm）  `ASSIGNED`
- **任务书**：[`docs/tasks/tts-module-arm.md`](../tasks/tts-module-arm.md)（中/英/泰三语）
- 目标链路的最后一段：拿到文字 → 用声音说出来，**中/英/泰可切换**。
  「大模型回答问题」那一段依赖外部服务，**本项目不做**。
- **接口先定死**：独立 HTTP 服务，`POST /speak {text, lang}` → `audio/wav`，
  加 `/health` 与 `/voices`。形态照 LLM 边车（配置里一行 `tts_url`）。
  这样 V2 换实现时 V1 的工作不白费。
- **V1 用 macOS 内置 `say`**：已实测目标机器上三语音色都在 ——
  `Tingting`(zh) / `Samantha`(en) / **`Kanya`(th)**。零安装、零下载、全离线。
- V2 再换神经 TTS（`facebook/mms-tts-tha` 有专门泰语模型）。**V1 合并后才开。**
- 约束：**不许联网**（隐私红线，排除所有云 TTS）；**独立进程**（ADR-0002）。

### T3.2.1 泰语 code-switch：给 whisper 加 initial prompt  `READY`
- **实测依据**：[`docs/data/thai-corpus-arm-2026-09/RESULTS.md`](../data/thai-corpus-arm-2026-09/RESULTS.md)
- 用**已经在发的** `terms.json` 当 whisper 的 `--prompt`：
  夹英文 CER **31.1% → 18.4%**，英文词命中 **8% → 51%**，纯泰语 3.9% → 3.1%。
  **约 10 行改动，不需要边车，不需要换模型。**
- ⚠️ **必须带长度护栏**：100 词的通用词表让纯泰语 CER 崩到 53.7%
  （截断 / 语言漂移 / 重复循环）。`whisper-cli` 的上限是 `n_text_ctx/2` tokens。
  **拐点没测**——C(37 词) 好、D(54 词) 好、E(100 词) 崩，落地前要找到安全上限。
- 落地前还要：D 方案按 3 遍复核（目前 1 遍，依据是 A 与 3 遍结果逐位一致）。
- **不要动 `-bs 1 -bo 1`**：实测换默认 beam search 只值 2 个词、0.3 个百分点。

### T3.2.2 泰语模型复评（三方横比）  `BLOCKED`
- **卡在模型文件**：ADR-0004 的另两个候选（Thonburian medium /
  typhoon-whisper-turbo）**已经不在机器上**，要重跑 `scripts/build-thai-model.sh`。
- 在跑完之前，「换泰语模型也修不了 code-switch」**是推断不是结论**
  （依据：泰语本身已 3.9%，且失败是这一类模型的共性——训练集都是泰文转写）。
- 跑的时候必须**分组算 CER**（对照组 vs 两个 code-switch 组），不要给总分。

### T2.4.8 索引一致性判据补齐  `TODO`
- PR #34 评审的变异矩阵指出三格没人接住：
  1. `--reindex` 末尾的 FTS5 `'rebuild'`（评审给了可用的测试，红绿两侧都验过）
  2. `--search` 的 `created` 按字符截断（防 panic）——在 CLI 路径上，
     需要真重构可测性，不是浅抽取能接住的
  3. `rebuild` 抢 `kb/.lock`——要起两个线程真抢锁
- 1 可以直接抄；2/3 需要单独设计，不要浅抽取（上个 PR 实测过接不住）。

### T2.4.6 重新分类已有 routes  `TODO`
- **缺口**：`--replay-kb` 按 `routes/` 里**已有的**标签重放，不重新分类。
  所以在启用理解层之前落成 `unknown` 的记录，后来启用了也捞不回来。
- 需要一条单独的 `--reclassify-routes`：读 `derived/transcripts/`，
  重跑分类，更新 `routes/`，再让 `--replay-kb` 投递。
- **不要偷偷塞进 replay 里**：重放的语义是「按记录重建」，
  重新分类会改记录本身，那是两件事。

### T2.4.7 泰语的显式标记  `TODO`
- `LEADERS` / `SPOKEN`（`src/label.rs`）只有中英文，**没有泰语**。
  ASR 认得泰语，但泰语用户说「นี่คือไอเดีย」不会被识别成显式标记。
- 卡在**语料**：我没法判断泰语里哪些说法自然、哪些会误伤正常句子。
  需要一个泰语母语者给出七类标签的常用说法，再照中文那套加边界判据和测试。

### T2.4.5 OpenKnowledge  `DEFERRED`
- 见 [ADR-0006](../decisions/0006-openknowledge-as-personal-frontend.md)：
  它是编辑器不是服务，对当前定位没有增量能力，**现阶段不做**。
- 真要做的时候规则是：可以在文档里推荐，**不 fork、不 vendor、不进 bundle**
  （GPL-3.0 vs 我们的 Apache-2.0，单向不兼容）。
