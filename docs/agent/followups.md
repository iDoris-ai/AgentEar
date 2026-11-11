# Follow-ups ledger（append-only · 永不删行 · 提交进仓库）

> pilot 的 review triage 把「真问题但不阻塞（B）」和延后项记在这里。
> 主线 task 全部完成后，由 `pilot run` 批量合成一个 cleanup PR 做掉，逐条标 [x] done=PR#n。
> `- [ ]`=OPEN，`- [x]`=DONE。GitHub PR comment 是永久兜底。

- [x] FU-1 · B · src=PR#T2.1.1-codex · 2026-09-02 · 术语表没有总上下文预算：边车窗口 32768 固定，术语表+正文+max_tokens 无上限检查，超大术语表会挤掉正文（codex Medium 6） · done=PR#23
- [x] FU-2 · B · src=PR#T2.1.1-codex · 2026-09-02 · terms.json 的 version 字段读了不校验，0/999 都接受，无法承担格式升级保护（codex Low 10） · done=PR#23
- [x] FU-3 · B · src=PR#T2.1.1-codex · 2026-09-02 · sanitize 未检测重复 alias / 同一 alias 指向多个 canonical 的冲突（codex Medium 3 的一部分） · done=PR#23
- [x] FU-4 · B · src=PR#T2.2.2-codex · 2026-09-03 · label/correct 的错误降级缺确定性测试：垃圾 JSON、缺字段、HTTP 500、空 content、finish_reason=length 都没经 HTTP 路径验证过（需 mock server 或可注入 transport） · done=PR#23
- [x] FU-5 · B · src=PR#T2.2.2-codex · 2026-09-03 · curl 子进程缺父进程强制的墙钟超时：--max-time 只覆盖传输阶段，卡在 stdin 交互时不保证；且 stdin 同步写入发生在排空 stdout/stderr 之前 · done=PR#23
- [x] FU-6 · B · src=PR#T2.2.2-codex · 2026-09-03 · spike/m2_bench.py 与生产用不同的解析器和标签顺序，应让基准直接调用生产分类路径 · done=PR#23
- [x] FU-7 · B · src=PR#T2.2.3-codex · 2026-09-03 · 显式标记只覆盖句首元指令句式，句中的「……，这算是一个 idea 吧」认不出来。有意的取舍（误判代价 > 漏判），但值得记着：若用户实际习惯是句尾标记，需要重新设计 · done=PR#23
- [x] FU-8 · B · src=T2.2.4 实测发现 · 2026-09-03 · 默认术语表的修正无法传播给已有 terms.json 的用户:write_default 刻意不覆盖(保护用户编辑),于是 T2.1.1 里删掉的危险 alias(肉/raw的/road目录)仍留在老文件里,实测导致纠错整体失效。需要版本迁移:version 不匹配时备份旧文件并写新默认表,或合并内置条目。与 FU-5(version 不校验)是同一处 · done=PR#23
- [x] FU-9 · B · src=T2.2.4 实测发现 · 2026-09-03 · longform 回归测试当前稳定失败(5/5),已在代码与文档里标注为已知缺陷并新建 T2.1.4 修它。注意:这条测试挂着不影响 preflight(cargo test 不跑 ignored) · done=PR#23
- [x] FU-10 · B · src=T2.1.4 实测 · 2026-09-03 · 分批纠错后「我的妈 book」不再被纠正(分批前某次是对的):上下文变短导致,补了带空格 alias 也没用。是分批的已知代价,若将来影响体感可考虑重叠窗口(相邻批带一点上文) · done=PR#23
- [x] FU-11 · B · src=PR#T2.1.4-codex · 2026-09-03 · correct/label 的 HTTP 调用应抽象成可注入 transport,才能为「全成功/中间批失败/全失败/token 截断/批边界空白」写确定性测试。现在这些分支只能靠真实边车碰运气覆盖(codex Medium 4 + Low 1) · done=PR#23
- [x] FU-12 · B · src=PR#T2.1.3-codex · 2026-09-03 · last-good 不持久化:进程重启后缓存为空,若编辑器截断文件后崩溃/断电,启动加载只能用内置默认表。可考虑每次成功解析后原子更新 terms.json.bak,主文件坏且无内存缓存时读备份(codex Low 5) · done=PR#23
- [x] FU-13 · B · src=T2.2.5 · 2026-09-03 · 标签分类唯一稳定判错:「要是能用语音直接建任务就好了」被句中「建任务」三字带走判成 task。定义已说清「承诺要做了吗」,是模型能力问题——修法是 few-shot 补一条虚拟语气的反例,不要改定义去迁就 · done=PR#23
- [x] FU-14 · B · src=T2.2.5 · 2026-09-03 · spike/m2_bench.py 报 18/18 而生产路径 17/18,差异稳定复现但根因未定位(已排除标签顺序、期望值、规则语义)。让基准直接调用生产分类路径可一并消掉这个疑点(与 FU-6 同源) · done=PR#23
- [x] FU-15 · B · src=T3.1.2 · 2026-09-03 · env -u HF_ENDPOINT 是本机 huggingface_hub 报 LocalEntryNotFoundError 的通用绕法(第三次遇到)。build-thai-model.sh 和 setup-llm.sh 现在用的是 curl 逐文件,可以简化成这个 · done=PR#23
- [x] FU-16 · A · src=2026-09-09 文档同步 preflight · 测试套件里那条不稳定用例 · **已定位并修复（2026-09-14）**
  - **它是谁**：不是**一条**用例不稳定，而是**一条共享的临时目录命名规则**会撞。复现时抓到过两个失败用例名——`deliver::tests::drain_retries_what_the_last_run_left_behind` 与 `kb::tests::identity_is_the_full_hash_not_a_prefix`，**分属两个模块**。单个用例自己有 bug 不会长这样，共享状态才会。
  - **量具**：连跑 20 次全量 `cargo test`，失败 1 次（约 5%）。这与「一次失败 + 六次通过」的原始观察同量级。
  - **根因（有独立证据）**：`deliver.rs` / `kb.rs` 各自的 `tmpdir()` 名字是 `{pid}-{SystemTime::now().as_nanos()}`，看着够唯一，**但 macOS 的 realtime 时钟分辨率约 1 µs**：单独写了个 10 行 Rust 程序实测，**连续两次取到相同纳秒值的比例是 91.6%**（200000 次里 183154 次）。测试默认并行跑，两个模块的 `tmpdir()` 落在同一个时钟刻度就指向**同一个目录**，于是互相覆盖对方写的文件。
  - **修法**：新增共用的 `crate::testutil::tmpdir()`（`src/main.rs`），名字里加一个**进程内单调递增的计数器**——不依赖时钟分辨率；时钟那一项保留，供多进程并行时区分。三处调用点收敂到一处（`deliver` / `kb`；`store` 用的是 `new_session_id()`，含 `getentropy` 随机字节，本来就安全，未改）。
  - **验证**：修复后连跑 30 次全量 `cargo test`，**0 次失败**（修复前 20 次里 1 次）。
  - ⚠️ **教训**：这个 flake 的形状（「随机一条用例失败，每次还不是同一条」）本身就是**共享状态**的指纹，而当时的记录只写了「身份未知」。**下次遇到 flake，第一条要记的不是「它又失败了」，而是「这次失败的是哪一条」**——两次不同的名字几乎就能定性。
  - **顺带更正**：`docs/benchmarks-m3.md` §7.5 记的是「测量设计缺陷比测量误差危险得多」，本条是它的另一面：**并发测试里的共享资源不比测量前提更安全**。
- [x] FU-17 · B · src=2026-11-10 文档对账 · 2026-11-10 · #114 的新 speech-server（解码上限 6144，`AGENTEAR_REV=2`）还没发布：需重跑 `scripts/build-speech-runtime.sh`、发新 `speech-runtime-<版本>` release、更新 `src/qwen3.rs` 常量与 `SPEECH_VERSION`；15 分钟录音的解码从未实测 · done=PR#121（v0.28.2）
- [x] FU-18 · B · src=2026-11-10 文档对账 · 2026-11-10 · `feat/app-store-sandbox` 对账时只有本机 worktree 的未提交改动（无远端、无 PR），tasks.md 的 T-APPSTORE-1 设计/验收写「未核实」；负责人补范围与设计文档 · done=PR#118
- [x] FU-19 · B · src=2026-11-10 文档对账 · 2026-11-10 · PR #112（v0.28.0 release notes 英文在前）仍 open · done=PR#112
- [x] FU-20 · A · src=PR#121-codex High · 2026-11-11 · 解码上限 6144 只在 Qwen3「常驻」路径生效；常驻默认关，关掉或常驻失败时走一次性 `speech` CLI，那条仍是上游 448——默认设置下的 Qwen3 用户长录音仍会被截断。要么让 CLI 路径也带上 maxTokens（补丁 + 运行时 rev 3），要么把常驻设为默认（内存代价见 T3.5.9） · **结论**：官方 `speech transcribe` 没有能改 qwen3 解码上限的参数（`--max-tokens` 只标注 [moss]，实测对 qwen3 无效：353 秒录音加 `--max-tokens 50` 前后输出一样、都停在 448）→ 不重编运行时；改为一次性路径遇 >60 s 的录音临时拉起补丁版 speech-server（`qwen3::transcribe_long_oneshot`，复用 ServerGuard/stop_server 机制），转完即停。实测（241 s 合成英文：旧 CLI 348 词 → 新路径 604 词）仅用 `say` 合成录音，**未用真人长录音、未测 15 分钟档，也未实测临时服务与常驻/预热并发的组合**；临时服务只在「这次自己拉起且无请求在用」时才回收。 · done=PR#待回填（v0.28.3）
- [ ] FU-21 · B · src=PR#121-codex Medium · 2026-11-11 · `.2` 运行时的安装标记先于「回切 speech_swift 后端」落盘：升级中途退出时 `recovery_decision` 会判成「没有空档」，不再恢复。既有缺口，非 #121 引入
- [x] FU-22 · B · src=v0.28.2 冒烟 · 2026-11-11 · `--diagnose` 在本机卡在 CoreAudio `AudioDeviceCreateIOProcID`（等 coreaudiod，sample 抓到），v0.28.0 旧包同样卡——疑似新签名包的麦克风 TCC 待授权或机器 swap 满。集成测试 `builtin_model_locator::diagnose_and_fetch_thai_keep_speech_swift_and_write_recovery_marker` 因此在本机 20s 超时（时好时坏，CI 绿）。修法候选：diagnose 打开输入流加超时并报「麦克风无响应/待授权」，而不是无限等 · **修法**：`diagnose` 的麦克风探测放进独立线程 + 3 s 超时（`probe_with_timeout`），超时报「麦克风无响应（可能等待授权或设备被占用）」并继续其余自检；卡住的线程无法取消，泄漏到进程退出（`--diagnose` 是一次性命令，可接受）。集成测试不需要音频，`run_isolated` 设 `AGENTEAR_DIAGNOSE_SKIP_AUDIO=1` 跳过探测。⚠️ **超时分支只有单测（模拟卡住 / panic 的探测），未在真实卡死的 CoreAudio 上验证**（本机探测此刻没再卡）——真机验证项仍开着；超时/panic/起线程失败三种如实分开报 · done=PR#待回填（v0.28.3）
- [ ] FU-23 · A · src=PR#116 · 2026-11-11 · 许可证缺口。**已做（PR docs/fu23-licenses，2026-11-11）**：NOTICE 改正 SenseVoice 为 FunASR Model License v1.1 并补 whisper.cpp / 泰语模型 / speech-swift / MLX 条目；`scripts/build-speech-runtime.sh` 改为把上游与依赖许可文本打进 tarball 的 `LICENSES/`（**下次构建运行时生效，未重新打包**）；ADR-0007 与 CLAUDE.md 的 VoiceChat「仅研究用途」更正为 OpenMDW-1.1（宽松）；Ornith 如实记为「元数据/README 声明 MIT，但无 LICENSE 文件、license_link 404，未随包分发」。**剩余（需 jason）**：① SenseVoiceSmall 商用是否允许——向许可方（FunASR/Alibaba）确认（协议 §3「仅作为参考和学习使用」），未确认前不下可商用结论；② 下次发运行时 release 时验收 tarball 里 `LICENSES/`；③ Ornith 上游许可文本、FSMN-VAD 上游权重许可、泰语 release 附 MIT 全文仍未核实/未做。详见 docs/licenses.md
- [ ] FU-24 · B · src=PR#119 · 2026-11-11 · 未锁项：vendor 的 llama-funasr-sensevoice / whisper-cli 无构建脚本与上游 commit；whisper.cpp checkout 未核对 ADR-0004 的 commit；泰语构建的 torch/transformers/numpy 未钉；pip 间接依赖未钉；setup-llm / build-thai-model 大权重无逐文件 sha256。详见 docs/upstream-pins.md
- [x] FU-25（#125 起由 `spawn_gated` 修：对话模式拉起遇闸改为排队等闸后再做；仍未在真机冷启动场景实测）· B · src=PR#120 · 2026-11-11 · 宿主冷启动 TTS 期间占着启动闸，这时切进对话模式，对话模式那次拉起被跳过、LLM 不会顺带拉起，要再切一次
