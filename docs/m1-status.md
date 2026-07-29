# M1 开发状态

日期：2026-07-29
代码：`src/`（Rust，`cargo build --release`）

## 已完成

| 项 | 状态 | 证据 |
|---|---|---|
| Rust 工程骨架 | ✅ | `cargo build --release` 通过 |
| 麦克风采集（cpal，下混单声道 + 重采样到 16kHz） | ✅ | `src/audio.rs` |
| raw 提交协议 | ✅ | `src/store.rs`，4 个测试 |
| 崩溃语义（未提交的临时对象启动时清理） | ✅ | `uncommitted_tmp_is_swept_on_open` |
| 内容寻址去重 | ✅ | `identical_audio_converges_to_same_object` |
| ASR 子进程调用 | ✅ | `src/asr.rs` |
| 特殊 token 过滤 | ✅ | 4 个测试 + 真实语料验证 |
| 全局快捷键注册（Ctrl+Shift+R） | ✅ | 启动无报错，未申请辅助功能权限 |
| 剪贴板写入 | ✅ | 代码就位，待交互验证 |
| 端到端转写 | ✅ | `--transcribe` 跑通真实语料，4.99s / 120s 音频 |

**测试**：8 passed，0 failed。

## 实测数字

| 指标 | 值 | M1 门槛 |
|---|---|---|
| **空闲常驻 RSS** | **11.8 MB** | ≤ 2 GB ✅ |
| 转写 120s 音频 | 4.99s（含模型加载） | — |
| 二进制体积 | release + strip | — |

### 11.8 MB 是怎么来的

因为 **ASR 模型不常驻**。SenseVoice 冷启动仅 0.2s（ADR-0001 §2.4），所以每次录音结束才起一个子进程做转写，用完即退。

守护进程本身只有音频缓冲和快捷键监听，**空闲时几乎不占内存**。这是选型换成 SenseVoice 的直接红利——如果还用 Fun-ASR-Nano（冷启动 11.45s），就必须常驻 1.5 GiB。

## 待办

### 需要 jason 交互验证（我这边跑不了）

1. **按 Ctrl+Shift+R 实测录音**。首次运行时 macOS 会弹麦克风权限请求。
2. **验证剪贴板**：录完后直接 Cmd+V 看能否粘贴。
3. **验证按键响应时延** < 200ms（体感即可）。

跑法：

```bash
cd /Users/jason/Dev/tools/AgentEar
./target/release/agentear
# 按 Ctrl+Shift+R 开始，说几句，再按一次停止
```

数据落在 `~/.agentear/`（可用 `AGENTEAR_DATA` 覆盖）。

### 未实现

- **菜单栏图标与状态显示**。当前是终端程序，状态靠 stdout。
  `milestones.md` 的验收标准要求「状态在菜单栏可见」，这一项还没做。
- **`.app` bundle 打包**。当前从终端跑会继承终端的麦克风权限（TCC），
  这正是 `milestones.md` 里警告过的「我这儿能跑」的假象。
  正式分发必须打 bundle 并在 `Info.plist` 写 `NSMicrophoneUsageDescription`。
- **`--keep-tags` 的默认行为未实测**。当前 `clean()` 是防御性过滤，
  真实输出里没见到 `/sil` 泄漏，但没有正面确认过运行时的默认行为。

## 已知的设计取舍

- **重采样用最近邻抽取**（`audio.rs::convert`）。对 ASR 前处理够用，且不引入 FFT 依赖。
  若日后发现高频混叠影响 CER，再换带抗混叠滤波的重采样器。
- **清单是 JSONL 追加**。M1 单进程单会话够用；`ingest-design.md` §3.3 要求接收序号
  与清单原子写入，**M4 协议化时必须换成 SQLite**，届时会有并发上传。
- **录音上限 300 秒**自动停止。M0 实测 RSS 随音频长度增长 0.52 MB/s，
  这是兜底护栏，M1 的快捷键录音通常远短于此。
