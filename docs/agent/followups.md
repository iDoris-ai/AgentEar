# Follow-ups ledger（append-only · 永不删行 · 提交进仓库）

> pilot 的 review triage 把「真问题但不阻塞（B）」和延后项记在这里。
> 主线 task 全部完成后，由 `pilot run` 批量合成一个 cleanup PR 做掉，逐条标 [x] done=PR#n。
> `- [ ]`=OPEN，`- [x]`=DONE。GitHub PR comment 是永久兜底。

- [ ] FU-1 · B · src=PR#T2.1.1-codex · 2026-09-02 · 术语表没有总上下文预算：边车窗口 32768 固定，术语表+正文+max_tokens 无上限检查，超大术语表会挤掉正文（codex Medium 6）
- [ ] FU-2 · B · src=PR#T2.1.1-codex · 2026-09-02 · terms.json 的 version 字段读了不校验，0/999 都接受，无法承担格式升级保护（codex Low 10）
- [ ] FU-3 · B · src=PR#T2.1.1-codex · 2026-09-02 · sanitize 未检测重复 alias / 同一 alias 指向多个 canonical 的冲突（codex Medium 3 的一部分）
- [ ] FU-4 · B · src=PR#T2.2.2-codex · 2026-09-03 · label/correct 的错误降级缺确定性测试：垃圾 JSON、缺字段、HTTP 500、空 content、finish_reason=length 都没经 HTTP 路径验证过（需 mock server 或可注入 transport）
- [ ] FU-5 · B · src=PR#T2.2.2-codex · 2026-09-03 · curl 子进程缺父进程强制的墙钟超时：--max-time 只覆盖传输阶段，卡在 stdin 交互时不保证；且 stdin 同步写入发生在排空 stdout/stderr 之前
- [ ] FU-6 · B · src=PR#T2.2.2-codex · 2026-09-03 · spike/m2_bench.py 与生产用不同的解析器和标签顺序，应让基准直接调用生产分类路径
