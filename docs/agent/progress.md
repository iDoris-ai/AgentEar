# 仓库实时状态

> 此刻在做什么、卡在哪、分支与 PR。每推进一步就更新，宁可啰嗦不可与仓库脱节。

**更新时间**：2026-09-09
**当前分支**：main（干净）
**main HEAD**：`3daa112` — chore(release): **v0.5.0 —— ASR 引擎可切换**
**测试**：`cargo test` **219 passed / 0 failed / 5 ignored**（源码共 224 条）。
ignored 的 5 条：**4 条要 LLM 边车**（3 条要边车在跑 + 1 条会真的拉起边车），**1 条要联网**（HEAD 一次模型 URL）。

> ⚠️ 上一版本文停在 2026-09-03（`2b3b916`），中间漏记了 17 个 commit、
> 4 个版本（v0.4.0 / v0.4.1 / v0.4.2 / v0.5.0）。这次是补记。
> **教训**：发版和文档同步要放进同一个 PR，不要事后补。

## 此刻状态：M2 已发布可用；**M3 实时对话在实现阶段，可行性 spike 判定「不通过」**

- 本地已无未合并分支。**远程只剩 `origin/c1-thai-asr-baseline` 未合**（ahead=13）。
  `origin/arm/agentear-dev` **ahead=0 / behind=5 —— 已经合进 main 了**
  （PR #41 `feat(tts): add local macOS TTS service`，merge `5fe1302`，2026-09-09T11:09:46Z）。
  ⚠️ **量具用 `git rev-list --count origin/main..<branch>`，不要用 `git branch -r --no-merged`**：
  后者读的是**可能过期的 remote-tracking ref**，我就是没 `git fetch` 就下了结论，
  把一个 79 分钟前已合并的分支报成「3 个 commit 未进 main」。
- 跟进账本（`followups.md`）：**FU-1…15 已 done，FU-16 开着**（测试套件的不明失败，见下）。
- 已清理：9 个 squash-merged 的本地分支 + 5 个失效 worktree（2026-09-09）。
  ⚠️ 这两个数字是当时的操作记录，**git 里不留删除痕迹，事后无法从仓库自证**。

## 用户装上到底能用什么（这张表比 task 状态更重要）

| 能力 | 状态 | 装上能用吗 |
|---|---|---|
| M1 输入法（按右 Command 说话上屏） | ✅ | **能** |
| 泰语识别（按需下模型） | ✅ | **能** |
| M2 术语纠错 / 标签识别 | ✅ v0.4.0 | **能，但要自己起 LLM 边车**（7.8 GB，不随包分发） |
| M2 知识库投递（`kb/**/*.md`） | ✅ v0.4.1 **默认开** | **能，零外部依赖** |
| L2 全文检索（`--search`） | ✅ v0.4.2 | **能** |
| ASR 后端可切换（`--asr-backend speech_swift`） | ✅ v0.5.0 | **能，但要自己装 speech CLI**；默认仍是 builtin |
| **M3 实时通话（打电话式、可打断）** | 只有 spike + 引擎层 | ❌ **还没有** |
| TTS 说话 | **V1 HTTP 服务已在 main**（`services/tts/`，PR #41） | ⚠️ **能单独跑，但 Rust 侧没接** |

**措辞纪律**：不启用理解层时，只有**明说了标签**的中英文句子会进知识库
（`label::explicit_only`，纯本地字符串匹配，**没有泰语**）。
不要说成「每次转写都会进知识库」。

## M3 实时对话：现在到哪一步了

方案见 [ADR-0007](../decisions/0007-realtime-voice-architecture.md)，
产品分两档：**V1 打断式半双工（≤10 GB，M3 目标）** /
**V2 实时全双工（64 GB 级，未排期）**。

```
T3.4.0 可行性 spike  ✅ 跑完   **判定：不通过**（四条阈值 1 PASS / 2 部分 / 1 未测），
                              但没到要推翻方案的程度——卡点只有一个：AEC 残留单字
   ↓
T3.4.1 引擎适配层    ✅ DONE   PR #44 → v0.5.0，src/engine.rs
   ↓
T3.4.2 通话会话层    ▶ READY  ← 下一个该做的，也是整个 M3 的重心
   ↓
T3.4.3 mock LLM（查天气）      BLOCKED（等 T3.4.2）
T3.4.4 serve / mcp 集成接口    BLOCKED（等 T3.4.2）
```

**T3.4.0 的四条阈值实测（benchmarks-m3.md §7）**：

| 判据 | 阈值 | 结果 |
|---|---|---|
| 并发常驻峰值内存 | ≤10 GB | **2.04 GiB PASS** |
| VAD 检出延迟 | — | ≈0 ms 部分 PASS |
| AEC 自触发率 | = 0 | **部分通过**：VPIO 抑制 20.4 dB，内容级 0/5，**事件级 5/5** |
| 端到端打断 <300 ms | — | **spike 里测不了，转为 T3.4.2 的出口判据** |
| 误打断 <1 次 / 10 分钟 | — | ❌ **未测** |

残留的 5 个输出**全是单字**（`嗯`/`金`/`啊`），是 VPIO 残余噪声被 ASR 强行解释成的字，
不是 TTS 的内容。（`1–5 字` 是解法里提出的过滤类别，**不是实测到的上限**。）

**AEC 走 CoreAudio VPIO，不走 speech-swift**（后者没有 AEC 出口，端点实测 404）。

## 下一步（按优先级）

1. **T3.4.2 通话会话层** —— M3 的重心。⚠️ **但它不是唯一的门**：M3 要能发，
   还得有 **TTS 的 Rust 集成**、T3.4.3 mock LLM、T3.4.4 集成接口。
   ✅ **TTS 的 V1 HTTP 服务已经合进 main 了**（`services/tts/`，PR #41，2026-09-09）：
   `POST /speak {text,lang}` → `audio/wav`，中英泰三语走 macOS `say`，
   带 `/health` 与 `/voices`，16 条测试。
   **但 `services/tts/ACCEPTANCE.md` 开头明写「No Rust integration performed」** ——
   守护进程一行都没调它，所以 T3.3.1 仍是 `ASSIGNED`（理由见 `tasks.md`）。
   开工第一件事是还两笔债：
   ADR-0007 §4.4 的**编排职责二选一**（ADR 倾向 A 自主编排，但明写了不能默认已定），
   和 T3.4.0 留下的 **AEC 事件级自触发**（最小语音时长 + 能量门限 + 播放期 gating）。
2. **T3.2.1 泰语 code-switch initial prompt** —— 约 10 行，与 M3 无依赖，
   夹英文 CER 31.1%→18.4%。**落地要截到 40 词**：20/30/40 词三格收益可复现（纯泰语 CER 都是 2.2%），
   54–70 词与基线区分不开且不单调（4.2% / 3.1%），85 词 6.6%，100 词纯泰语崩到 22.2%。
   ⚠️ **40 是护栏建议，不是测出来的拐点**——RESULTS.md 明说劣化起点定位不了。
3. **T3.5.1 中文/英文 ASR 横比** —— 「换默认 ASR 引擎」拍板的前置，没有它 jason 无法决策。
4. T3.5.3 逐组件许可证表 —— 未确认许可的组件不得进默认方案（VoiceChat 11B 就是仅研究用途）。

## 待 jason 拍板 / 需要人

| # | 问题 | 阻塞谁 | 为什么我决定不了 |
|---|---|---|---|
| **Q3** | `~/Desktop/agentear-tts-samples/` 的样本是真方言还是普通话念汉字？ | ADR-0005 全部结论、T3.5.4 | **需要人耳**，我播不了音频 |
| **Q4** | 要不要把默认 ASR 换成 Qwen3-ASR？ | ADR-0001 / ADR-0007 §5 | 影响全部语种，**要等 T3.5.1 的中英横比**才谈得上 |
| Q1 | 「今天开会讨论了传输协议」判 note 还是 journal？ | 标签定义权威性 | 实现按 journal 做了，标为临时决策 |
| Q2 | 若闽南语 TTS 确实没方案，M3 怎么走 | T3.1.3 | ADR-0005 已按两种情形写好，等 Q3 |

## 环境前置

```bash
scripts/setup-llm.sh          # 首次：装环境 + 拉 7.8 GB 模型
scripts/serve-llm.sh          # 每次：起服务，默认 127.0.0.1:8793
./target/release/agentear --diagnose | tail -6   # 确认「✅ 服务」
```

⚠️ 边车没起时，`cargo test --release -- --ignored` 的集成测试会失败——
**那是环境问题不是代码问题**，先查这一条再怀疑实现。

## 踩过且已钉住的坑（给后面的 task 看）

1. **worktree 里没有 `vendor/`**（不入库）。跑真实转写要
   `AGENTEAR_VENDOR=/Users/jason/Dev/tools/AgentEar/vendor`。
2. **提示词的格式歧义会让整个功能失灵**，不是错一条。术语表踩了五版。
3. **改提示词必须真调一次边车验证**——单元测试和真实录音都可能全绿而功能是错的。
4. **模型输出的结论至少连跑 3 次**。「术语表修好了长文」和「标签 18/18」
   都曾因单次通过而下早了，后来被推翻。
5. **`env -u HF_ENDPOINT`** 是本机 huggingface_hub 报 `LocalEntryNotFoundError`
   的通用绕法（踩了三次才总结出来）。
6. **bash 3.2 会把 `$VAR` 后紧跟的中文标点当成变量名**。改完 `scripts/*.sh`
   跑 `scripts/lint-shell.sh`。
7. **手写的 CLI 解析器会静默改变程序形态**：加 `--asr-backend` 后
   `--asr-backend x --transcribe y` 让 `args[1]` 不再是子命令，
   **程序无报错地变成守护进程**。已改位置无关并加回归测试，
   但根治要 T3.4.5（改用 clap）。
8. **spike 的对照组必须能证伪自己**：T3.4.0 第一批三轮 AEC 数据
   因扬声器被静音而全部作废，重测才加了「对照组必须复现 TTS 内容」的守卫。

## 变更日志

- 2026-09-02 建立 `docs/agent/` 七件套规划
- 2026-09-03 F2.1 / F2.2 / F3.1 / F2.3 完成，账本清空
- 2026-09-03 v0.4.0（M2 理解层）→ v0.4.1（知识库投递默认开）→ v0.4.2（全文检索）
- 2026-09-04 CI 落地（macOS runner）、泰语语料归档、initial prompt 长度拐点实测
- 2026-09-08 ADR-0007 定稿草案；T3.4.0 可行性 spike；T3.4.1 引擎适配层 → **v0.5.0**
- 2026-09-09 **PR #41 合入 Arm 的 TTS V1 HTTP 服务**（`services/tts/`，macOS `say`，中英泰）
- 2026-09-09 补记本文与 tasks.md（漏记了 4 个版本）；清理 9 分支 + 5 worktree
