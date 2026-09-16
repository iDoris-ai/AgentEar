# 初始化 / 换机器 / 协作规矩

> 这份是**照着做就能跑起来**的那一份：新机器怎么装、什么东西不在 git 里、
> 改动怎么进主干（**必须由 PR-Daemon 审**）。
> 仓库的技术细节看 `CLAUDE.md`，实测数字看 `docs/benchmarks*.md`。

**本文件里的命令都实际跑过**；没跑过的会明确写「未验证」。

---

## 1. 换到新机器（Mac mini）最短路径

### 1.1 先在脑子里分开三堆东西

| 堆 | 在 git 里吗 | 说明 |
|---|---|---|
| 仓库代码 / 脚本 / 文档 | ✅ | `git clone` 就全有（含 `docs/releases/` 历次 release notes、`scripts/measure-f0.py`） |
| 运行期依赖（ASR 二进制、各模型、音色、配置） | ❌ **按设计不入库** | 见 §1.4，必须单独搬或重下 |
| Python 运行环境（venv） | ❌ | **不要搬**，脚本会自己建（§1.5） |

⚠️ 这一节存在的理由：**「都 push 了吗」和「新机器上能跑吗」是两个问题**。
2026-09-16 就踩过一次——仓库侧一个未推送的 commit 都没有，
但被源码引用的 `measure_f0.py` 躺在 gitignore 的 `vendor/models/talk/` 下，
换台机器那些实测数字就没有可复现的来源（已修，并加了钉子
`tests/script_defaults.rs::cited_measurement_tools_are_in_the_repo`）。

### 1.2 四条命令

```bash
# ① 仓库
git clone git@github.com:iDoris-ai/AgentEar.git ~/Dev/tools/AgentEar
cd ~/Dev/tools/AgentEar

# ② vendor/（ASR 二进制 + SenseVoice/VAD/泰语模型，268 MB）
#    ⚠️ **没有 CLI 子命令能抓它**（`--fetch-thai` 只抓泰语那一个模型）。
#    三条路，按省事程度排：
#    (a) 从发布包解 —— 实测包里有完整的一份（下面三行）
#    (b) 按 README「从源码构建」那段的 curl 从上游装（**本次未复跑**，
#        URL 会随上游漂移；`external-links.yml` 每周查的就是这类资产还在不在）
#    (c) 从旧机器直接拷
gh release download v0.18.0 --pattern '*macos-arm64.zip' -D /tmp
unzip -q /tmp/AgentEar-0.18.0-macos-arm64.zip -d /tmp/ae
cp -R /tmp/ae/AgentEar.app/Contents/Resources/vendor ~/Dev/tools/AgentEar/vendor

# ③ 两个边车模型 + Python 环境 + 默认音色库
scripts/setup-talk.sh --tts-quant 8bit     # jason 这台用 8bit；**发布默认是 4bit**
agentear --fetch-thai                      # 泰语 ASR（547 MB），可选

# ④ 跑起来
cargo build --release
./target/release/agentear --diagnose       # 权限 / 设备 / ASR 依赖自检
```

已验证的包内布局：`AgentEar.app/Contents/Resources/vendor/{bin,models}`，
其中 `bin/llama-funasr-sensevoice`、`bin/whisper-cli`、`models/fsmn-vad.gguf` 等。

### 1.3 配置里的绝对路径要改

`~/.agentear/config.json` 里有**指向本机仓库的绝对路径**（实测两条）：

```
/Users/jason/Dev/tools/AgentEar/scripts/serve-tts.sh
/Users/jason/Dev/tools/AgentEar/scripts/serve-talk-llm.sh
```

新机器上**用户名或 clone 路径不同就必须改**，否则守护进程切到对话模式时
「拉不起边车」（命令默认是空的时只连不拉、日志里会打出该跑哪条命令，不静默）。

### 1.4 不在 git 里的东西（实测清单）

| 路径 | 体积 | 怎么拿 |
|---|---|---|
| `vendor/` | 268 MB | §1.2 ② 的三条路 |
| `~/.agentear/config.json` | 995 B | 拷过去 + 改路径（§1.3） |
| `~/.agentear/talk/voices/` | 12 MB | **拷**。⚠️ 里面是 jason 的声音（`男声.wav`）——**绝不进任何公开仓库** |
| `~/.agentear/talk/models/` | 6.5 GB | 重下 `setup-talk.sh --tts-quant 8bit`，或拷 |
| `~/.agentear/models/ggml-distill-whisper-th-*.bin` | 547 MB | `agentear --fetch-thai`，或拷 |
| `~/.agentear/raw/` | 1.4 GB | **你自己定** —— 按存储语义这是 L0，**丢了不可重建** |
| `~/.agentear/llm/venv`、`~/.venvs/mlx-audio` | 8.2 GB | **不要搬**，§1.5 |

`commands.json` 这台机器**没有**（用开箱默认表），所以不用管它。

### 1.5 Python 环境：不要搬 venv

`scripts/setup-talk.sh` 会**自己建**：优先复用 `~/.agentear/llm/venv`（M2 那套，
里面同时有 `mlx-lm` 与 `mlx-audio`），找不到就在 `<数据目录>/talk/venv` 新建并装依赖。

⚠️ **脚本一律不许裸调 `python3`**：非交互 shell 里 `python3` 是 Xcode 的 3.9.6
（`import mlx_lm` 都做不到）。`serve-tts.sh` / `serve-talk-llm.sh` 都钉
`$VENV/bin/python`，这是对的，别改。

⚠️ `setup-talk.sh` 挑解释器会**过版本闸**（每个候选跑一次
`sys.version_info >= (3, 11)` 才算数）——早先只判「这个名字存在吗」，
于是 `python3.14` 会排在 3.11 前面被选中。有测试钉住。

### 1.6 新机器上应当自检的三件事

```bash
cargo test                                                    # 期望 298 passed / 6 ignored
python3 -m unittest discover -s services/tts -p 'test_*.py'   # 期望 46 passed
scripts/serve-tts.sh & sleep 5; curl -s localhost:8765/health  # 看 mlx.cache_limit_set
```

⚠️ **TTS 单测里响度那 3 条需要 numpy**：`normalize_loudness` 在没有 numpy 时
是**恒等函数**，于是第一条会以「RMS 差 10 倍」的样子失败（像真 bug）、
另外两条**假绿**。已有 skip 守卫，但 **skip 不等于通过** ——
本机三个解释器（3.9.6 / 3.11.15 / 3.12.13）都有 numpy，正常应当 **46 passed**。
⚠️ **CI 的 `python3` 没有 numpy**，所以响度 3 条在 CI 里永远是 skip，
CI 覆盖不到它们。

---

## 2. 改动怎么进主干：**必须由 PR-Daemon 审**

### 2.1 流程（每一步都别跳）

```bash
git checkout -b <type>/<短名>
# …改完、本地测试绿…
git commit && git push -u origin <branch>
gh pr create --title "…" --body "…"     # 正文写清「改了什么 / 为什么 / 验证了什么 / 没验证什么」

# ② 审查：走 PR-Daemon，**不是自己写一段 approval**
#    在装了 PR-Daemon skills 的 Claude Code 会话里：
#      /pr iDoris-ai/AgentEar#<N>
# ③ 等 CI 绿（`test` 是必需检查）
gh pr checks <N> --watch
# ④ 合并（squash）
gh pr merge <N> --squash --delete-branch
```

### 2.2 硬规矩

1. **不许自审自并**：agent 不得自己写 verdict 再合并。审查必须来自
   PR-Daemon 的 pipeline（`$pr OWNER/REPO#N`，四轮：
   DeepSeek 双通道 R1 → Opus R2 → Codex R3 PK → Opus R4 拍板），
   verdict 用 `scripts/post_pr_review.sh` 发。
2. **审批必须落在最后一次 push 之后**（GitHub 已强制，见 §2.3）。
   改了代码就重新审——**不要**拿旧的 approval 合新代码。
3. **CI 绿是前提**，不是装饰：`required_status_checks.strict=true`、context = `test`。
4. **文档跟着代码进同一个 PR**。仓库已经吃过两次「代码写完、实测做完、没提交没发版」的亏
   （见 `docs/agent/progress.md` 顶部）。

### 2.3 分支保护的真实配置（2026-09-16 实测 + 已加固）

```
required_status_checks:        strict=true, contexts=["test"]     # CI 必须绿，且必须基于最新 main
required_pull_request_reviews: required_approving_review_count=1
                               dismiss_stale_reviews=true         # 新 push 作废旧审批
                               require_last_push_approval=true    # 2026-09-16 加的，见下
enforce_admins:                true                               # 管理员也不能绕过
allow_force_pushes / deletions: false
```

**`require_last_push_approval` 是这次补的那一项**：补之前，
「先审 → 再 push 代码 → 直接合」在机制上是被 `dismiss_stale_reviews`
拦住的（PR #77 实测被平台拒过一次，只能重新审），但**审批人与最后 push 的人
可以是同一个**这一格是空的。现在填上了。

⚠️ **GitHub 能强制什么、不能强制什么，要说准**：

- 能强制：**审批人不能是 PR 作者**（平台自带）、审批不能早于最后一次 push、
  CI 必须绿、管理员也不能绕过、不能强推、不能删分支。
- **不能强制**：「审批的人是不是真人 / 是不是独立第三方」。平台只看账号。
  当前配置下，**PR-Daemon 的审查账号（clestons）与作者账号（jhfnetboy）
  是两个账号**，机制上满足「另一个人审」；但**没有任何机制能阻止 agent
  用 clestons 的 PAT 发一段自己写的 approval** —— 2026-09-16 之前我正是这么做的。
  **所以第 1 条硬规矩（必须走 pipeline）是流程约束，不是平台约束。**
  想让平台也帮上忙，只有一条路：把 reviewer 换成第三个真人账号。
- `require_code_owner_reviews` **没开**，是**故意**的：见 §2.4。

### 2.4 为什么没开 CODEOWNERS

只有一个真人账号时，`require_code_owner_reviews` + `CODEOWNERS=@jhfnetboy`
会把**作者本人的 PR 锁死**（作者不能批自己的 PR），而 `enforce_admins=true`
连管理员都绕不过 —— 那就等于**谁都合不了**。这是不可恢复的锁，**不要顺手开**。

### 2.5 PR-Daemon 这边的现状（实测，别被它的输出骗了）

| 事实 | 证据 |
|---|---|
| `iDoris-ai` **在**它的扫描范围里 | `scripts/scan_scope.py` 的兜底组织列表含 `iDoris-ai`，另有 `config/candidate-orgs.conf` |
| 四轮所需的 CLI **都在** | `claude`、`codex` 都在 `/Users/jason/.local/bin/`；`PR_DAEMON_REVIEWER_CLI=claude`；`.env` 里各段 key 齐全 |
| **但它没在跑** | 它的库 `pr-watch.sqlite` 里 `last_full_sync_epoch` = **2026-08-19** |
| 它的「0 待审」**不是事实** | AgentEar 在它库里只到 **PR #46**（#47–#78 从没进去过）；`review_queue.py` 那句 `queue empty — all open PRs reviewed at head ✓` 是**过期账本的产物** |

⚠️ **结论**：想让它审，得**真的触发一次**（`/pr iDoris-ai/AgentEar#N`，
或把巡检起起来）。**不要拿 `review_queue.py` 的「全部已审」当成审过了**。

---

## 3. 发版流程（发新版本时）

```bash
# ① 版本号 + 文档 + notes 在同一个 PR 里
#    - Cargo.toml 的 version
#    - CLAUDE.md 顶部「当前状态」那一行
#    - docs/releases/vX.Y.Z-notes.md（**发版正文，入库**）
#    - docs/agent/progress.md 加一条本轮记录
# ② bundle
scripts/bundle.sh                              # → dist/AgentEar-X.Y.Z-macos-arm64.zip
# ③ 合并后（见 §2）
git tag -a vX.Y.Z -m "…" && git push origin vX.Y.Z
gh release create vX.Y.Z dist/AgentEar-X.Y.Z-macos-arm64.zip \
  --title "…" --notes-file docs/releases/vX.Y.Z-notes.md
```

⚠️ **`scripts/` 与 `docs/` 不随包**（实测
`unzip -l dist/*.zip | grep -c scripts/` = 0），所以只改这两处的 PR
**不需要发包**，也不用升版本号。
⚠️ 打包产物 `dist/` 不入库（`.gitignore`）；**`vendor/` 也不入库**，
但发布包里**带** vendor（`Contents/Resources/vendor`）——这两件事不矛盾，别搞混。
