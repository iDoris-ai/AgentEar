# 历次发布的 release notes

一版一个文件：`v0.18.0-notes.md`，内容就是 GitHub release 的正文。

**为什么放仓库里**（2026-09-16 搬进来的）：这些文件原来躺在
`vendor/models/talk/release/`，而 `vendor/models/` **被 gitignore** ——
新克隆出来的仓库里没有它们，只能上网看 release 页面。
搬进来的代价是零（纯 Markdown，几 KB），换来的是**离线可查、可 diff、跟着历史走**。

**权威源仍然是 GitHub release**（`gh release view v0.18.0`）：资产、发布时间、
tag 指向哪个 commit 只有那边有。这里放的是**正文**。

发版流程里同步更新（见 CLAUDE.md「构建与测试」那一节）：
写完 notes → 用 `gh release create --notes-file` 发出去 → 同一版的文件落在
本目录。⚠️ **不要只写不落**，`docs/agent/progress.md` 里已经记过一次
「代码写完、实测做完、但没提交没发版」的教训。
