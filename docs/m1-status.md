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

## 端到端验收（2026-07-29 jason 实测通过）

一次完整录音的实测时间线：

| 阶段 | 耗时 |
|---|---|
| 按键 → 麦克风就绪 | **146 ms** |
| 录音 | 7.4 s |
| raw 提交（走完整协议） | **28 ms** |
| 转写 | **0.26 s** |
| **全程** | **7.8 s** |

转写结果正确进入剪贴板。

### 验收标准对照

- [x] 快捷键 toggle 录音（右 Command）
- [x] 录音按提交协议落进 `raw/audio/<content_hash>.wav`
- [x] 崩溃语义：未提交的临时对象下次启动被清理
      （单元测试 + 一次真实事故双重验证：那次「停不下来」的 27 秒录音
      正确停留在 `.tmp/`，重启后被自动清理）
- [x] 转写结果进剪贴板，已过滤特殊 token
- [x] 按键到开始录音 < 200 ms（实测 146 ms）
- [x] 常驻内存 ≤ 2 GB（实测空闲 13 MB）
- [x] 状态在菜单栏可见（`🎙` 空闲 / `● Ns` 录音中 / `◌ 转写中`）
- [x] **转写失败时 raw 音频仍然完好** —— 实测：把 `sensevoice-small-q8.gguf`
      头部写成 `GARBAGE` → 运行时报 `invalid magic characters` 优雅退出、
      未 panic，raw 的 sha256 前后完全一致。恢复模型后转写正常。
      （这一项此前漏在对照表外，2026-07-30 补测）

### 一个实测中发现的问题：蓝牙耳机会被优先选中

实测日志：`输入设备: Bose QC35 II | 16000 Hz, 1 ch, F32`

蓝牙耳机作为输入设备时走 HFP 模式，采样率被压到 16 kHz 且带宽很窄，
音质明显差于 MacBook 内置麦克风（48 kHz）。**长期使用会拉低 CER。**

当前用的是 `default_input_device()`，跟随系统默认。应当加设备选择，
或至少在用了蓝牙输入时给出提示。

## 待办

跑法：

```bash
cd /Users/jason/Dev/tools/AgentEar
./target/release/agentear             # 按右 Command 开始/停止
./target/release/agentear --debug-keys  # 排查按键问题
./target/release/agentear --diagnose    # 环境自检
```

数据落在 `~/.agentear/`（可用 `AGENTEAR_DATA` 覆盖）。

## 打包

```bash
scripts/bundle.sh          # → dist/AgentEar.app（248 MB，模型占 242 MB）
open dist/AgentEar.app
```

bundle 做了这几件事：`LSUIElement=true`（只在菜单栏、不占 Dock）、
`NSMicrophoneUsageDescription`（**缺了这条系统会直接拒绝而不是弹窗询问**）、
ad-hoc 签名、把 `vendor/` 放进 `Contents/Resources/`。

`vendor_root()` 的查找顺序：环境变量 → bundle 的 Resources → 源码树。

### ⚠️ bundle 的权限与终端是分开的

打包后第一次运行立刻验证了这一点——日志显示：

```
WARN 辅助功能权限：未授予 → 降级为 Ctrl+Shift+R
```

从终端跑二进制时继承的是**终端**的 TCC 授权，`.app` 是独立主体，
麦克风和辅助功能都要**再单独授权一次**，且辅助功能授权后**必须重启程序**。

### ⚠️ 每次重新打包，辅助功能授权都会失效

2026-08-07 部署时实测到的坑，**症状具有欺骗性**：系统设置里 AgentEar
的开关看着是打开的，TCC 数据库里也确实有 `auth_value=2`（已允许），
但程序启动仍然报「未授予」并降级到 Ctrl+Shift+R。

根因：`bundle.sh` 里的 `codesign --force --deep --sign -` 每次都会生成新的
cdhash，而 TCC 那条记录把签名标识**钉死在旧 cdhash 上**。签名对不上，
授权就不生效——但旧记录还在，所以 UI 上看不出任何异常。

诊断（读的是**系统级** TCC 库，不是 `~/Library/` 下那个；需要终端有完全磁盘访问权限）：

```bash
sqlite3 "/Library/Application Support/com.apple.TCC/TCC.db" \
  "select client,auth_value,datetime(last_modified,'unixepoch','localtime')
   from access where service='kTCCServiceAccessibility' and client like '%gentear%';"
```

如果 `last_modified` 早于最近一次 `bundle.sh`，那这条记录就是死的。修复：

```bash
tccutil reset Accessibility ai.idoris.agentear
# 然后在 系统设置 → 隐私与安全性 → 辅助功能 重新用 + 添加 /Applications/AgentEar.app
launchctl kickstart -k gui/$(id -u)/ai.idoris.agentear
```

日志里出现这一行才算真的成功：

```
INFO agentear::hotkey: 辅助功能权限：已授予 → 使用「单独按右 Command」触发
DEBUG agentear::hotkey: CGEventTap 已挂载到监听线程的 run loop
```

**降级路径是有效的兜底**：没有辅助功能权限时 `Ctrl+Shift+R` 走 Carbon 注册，
不需要该权限，功能完全可用——只是丢了右 Command 这个更顺手的触发方式。

## 部署：launchd 常驻

日常使用不要从 `dist/` 里直接跑——那个目录会被 `bundle.sh` 整个 `rm -rf`。

```bash
cp -R dist/AgentEar.app /Applications/
```

`~/Library/LaunchAgents/ai.idoris.agentear.plist`：

```xml
<key>ProgramArguments</key>
<array><string>/Applications/AgentEar.app/Contents/MacOS/AgentEar</string></array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>StandardErrorPath</key><string>/Users/jason/.agentear/launchd.err.log</string>
```

`KeepAlive` 必须写成 `SuccessfulExit: false` 而**不是** `<true/>`——写成 `true`
的话，从菜单栏正常退出也会被 launchd 立刻拉起来，程序关不掉。

launchd 直接执行 bundle 内的二进制（而非 `open -a`）是刻意的：`open` 起的进程
不归 launchd 管，`KeepAlive` 就失去意义。TCC 仍按 bundle 归属，不受影响；
`Info.plist` 的 `LSUIElement` 也照常生效，不会跳出 Dock 图标。

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.idoris.agentear.plist
launchctl kickstart -k gui/$(id -u)/ai.idoris.agentear   # 重启
launchctl print    gui/$(id -u)/ai.idoris.agentear       # 看状态
launchctl bootout  gui/$(id -u)/ai.idoris.agentear       # 停掉并取消开机自启
```

`bootstrap` 报 `5: Input/output error` 时，先确认 plist 文件真的存在且
`plutil -lint` 通过——这个错误码不区分「文件不存在」和「格式错误」。

**更新流程**：`scripts/bundle.sh` → `cp -R dist/AgentEar.app /Applications/`
→ `tccutil reset` + 重新授权（见上）→ `launchctl kickstart -k`。

### 日志

日志同时写 stderr 和 `~/.agentear/agentear.log`。从 Finder 启动 `.app` 时
stderr 无处可去，文件日志是唯一能看到发生了什么的途径——对一个没有主窗口
的菜单栏程序这是刚需。

## v0.2.0（2026-08-08）：可用性补齐

M1 的验收项在 v0.1.0 就齐了，但真正日常常驻起来之后暴露出四个缺口，
v0.2.0 逐条补掉。

### 菜单栏设置菜单

此前 `tray.rs` 只设了 `NSStatusItem` 的标题，**没有 `NSMenu`——想退出只能
`launchctl bootout` 或 `kill`**。现在有完整菜单：触发键 / 输入设备 /
自动上屏 / 原始音频保留期 / 打开数据目录 / 查看日志 / 退出。

实现上两个点：

- 菜单走 `NSMenuDelegate::menuNeedsUpdate:` **每次展开前重建**。输入设备
  列表会随耳机插拔变化，只在 `install()` 时建一次的话插上耳机列表就是错的。
- 用**一个 action + tag 分发**，而不是十几个 ObjC 方法。设备项的 tag 是
  下标，点击时按 `DEVICE_SNAPSHOT`（建菜单那一刻的快照）取名字——不能重新
  枚举，否则期间插拔会选错设备。

### 输入设备可选

`Recorder::start()` 收一个 `Option<&str>`。**指定的设备不在线时退回系统默认
而不是报错**——设备是可拔的，为了一个拔掉的耳机让录音直接失败不划算，但
降级要明确写进日志。另外采样率 ≤16 kHz 且单声道时会警告一次，这是蓝牙 HFP
模式的特征，识别质量会下降。

设置在**下一次录音**生效，不用重启。

### 右 Command 误触发已修掉

原来在**按下**瞬间就触发，于是用右手按 ⌘C / ⌘V 每次都顺带启动一次录音。
现在改为在**松开**时判定，三个条件同时满足才算一次「轻点」：

1. 按下期间没有任何普通键按下 → ⌘V 不触发
2. 按下期间没有其他修饰键参与 → ⌘⇧… 不触发
3. 按下到松开 ≤ 500 ms → 「按住右 Cmd 想快捷键」不触发

条件 1 要求 event tap 也订阅 `KeyDown`。**这是键盘记录器形状的能力**，所以
代码里只读「有没有键按下」这一个 bool，从不读键码、不记录、不落盘；键码只在
`--debug-keys` 显式打开时才打印。改那段代码时请守住这个边界。

判据抽成了纯函数 `hotkey::is_tap(held_ms, clean)` 并单测真值表——事件回调
本身依赖 CGEventTap 跑不了单测，但误触发正是判据错了，值得单独测。

### 原始音频 30 天自动清理

`Store::purge_older_than(days)`，默认 30 天，菜单里可改 7/30/90/永不。
启动时清一次，之后工作线程每 6 小时复查一次（守护进程一开就是几周，
只在启动时清等于对常开的机器不生效），且**只在空闲时清**，不和录音抢盘。

**这是真实且不可逆的删除**，所以实现上刻意保守：只动 `raw/audio/` 下的
`*.wav`，不递归、不碰其他扩展名；`derived/transcripts/` 一律保留——过期的是
几十 MB 的音频，不是几 KB 的文字。单个文件删失败只记日志、继续处理其余的。

已知取舍：`raw/manifest.jsonl` 里对应的行不会删，清理后清单会指向不存在的
对象。M1 没有消费清单的代码，等 M4 换 SQLite 时一并处理。

### 配置文件

`~/.agentear/config.json`，写入走临时文件 + rename（崩在写一半不会留下半截
JSON）。**解析失败退回默认值并继续启动**——配置损坏不能让守护进程起不来。

三项里只有**触发键**需要重启进程（`CGEventTap` 挂在跑 `CFRunLoop` 的线程上，
运行时换不掉），菜单里改完会自动重启：由 launchd 托管时走
`launchctl kickstart`，裸跑时 re-exec。**托管时必须走 kickstart**，自己 fork
新实例再退出会绕开 launchd 的单实例保证，落得两个进程同时抢热键。

### 仍未实现

- **签名与公证**。当前是 ad-hoc 签名，别人下载会被 Gatekeeper 拦住。
  这也是每次重新打包都要重新授权辅助功能的根因（见上）。
- **240 MB 的包体**。模型占 242 MB，随包分发是刻意选择——开箱即用优先。
- **`--keep-tags` 的默认行为未实测**。当前 `clean()` 是防御性过滤，
  真实输出里没见到 `/sil` 泄漏，但没有正面确认过运行时的默认行为。

## macOS 上踩过的三个坑（都表现为「按了没反应」）

值得记下来，因为它们的表面症状完全一样，但根因层层递进：

1. **主线程没跑 CFRunLoop。** Carbon 快捷键和 NSEvent 都靠 run loop 派发事件。
   原来的主循环只是 `sleep` + `try_recv`，事件注册成功但永远送不到。
2. **`NSEvent addGlobalMonitorForEventsMatchingMask:` 在纯 CLI 二进制里回调永不触发。**
   它依赖 AppKit 的 `NSApplication` 机制。改用 Quartz 层的 `CGEventTap`，
   只需一个 CFRunLoop 即可工作。
3. **松开事件的 keyCode 和按下一样是 54。** 判据里加 `code == 54 && raw != 0`
   作兜底，会把松开也算成按下，状态位永远卡在 true，上升沿再不出现——
   表现是「能开始录音但停不下来」。**只看设备位 `NX_DEVICE_R_CMD (0x10)`。**

## 已知的设计取舍

- **重采样用最近邻抽取**（`audio.rs::convert`）。对 ASR 前处理够用，且不引入 FFT 依赖。
  若日后发现高频混叠影响 CER，再换带抗混叠滤波的重采样器。
- **清单是 JSONL 追加**。M1 单进程单会话够用；`ingest-design.md` §3.3 要求接收序号
  与清单原子写入，**M4 协议化时必须换成 SQLite**，届时会有并发上传。
- **录音上限 300 秒**自动停止。M0 实测 RSS 随音频长度增长 0.52 MB/s，
  这是兜底护栏，M1 的快捷键录音通常远短于此。
