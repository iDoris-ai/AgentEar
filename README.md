# AgentEar

**全本地的语音 → 文字 → AI 工作流管线。** Listen and Collect, Response, get the Report and Result.

市面上那些 AI 录音卡、录音助手,产品设计确实不错。但作为能动手的 hack,应该自己攒一个——因为用别人的有一个很大的担心:数据经过他的服务器、经过第三方中转,你的隐私数据等于是裸体的。

**两个不可让步的设计约束:成本和隐私。** 录音和转写全部在本机完成,不经过任何第三方服务器。

---

## 当前状态:M1 已完成(v0.3.1)

按一下右 Command 开始录音,再按一下停止,几百毫秒后转写文字就在剪贴板里。

```
按右 Command → 录音 → raw 原始音频落盘(零丢失) → 本地转写 → 剪贴板
                                                      ↓
                                            菜单栏显示状态
```

实测一次 7.4 秒的录音,**全程 7.8 秒**完成(提交 28ms + 转写 0.26s),空闲常驻内存 **13 MB**。

| 里程碑 | 状态 |
|---|---|
| **M0** ASR 选型与基准 | ✅ 四模型横比,选定 SenseVoiceSmall |
| **M1** 按键即录即转写 | ✅ 验收 8/8 |
| **M1+** 三语界面 + 泰语识别 | ✅ v0.3.0 / v0.3.1 |
| M2 理解与标签路由 | 待开始 |
| M3 语音输出与打断 | |
| M3.5 录音笔能力刻画 | 机器未到手 |
| M4 录音笔接入 | |
| M5 独立硬件 | |

---

## 安装

**要求:Apple Silicon Mac(M1 及以上)、macOS 11+。**
不支持 Intel——内置的 ASR 运行时官方只发 arm64 版本。上游
[modelscope/FunASR](https://github.com/modelscope/FunASR/releases) 从
`runtime-llamacpp-v0.1.9` 到最新的 `v0.2.6`,macOS 一直只有 `macos-arm64`
(Linux/Windows 才有 x64)。所以这不是「暂时没做」,是上游没有——
就算把主程序编成通用二进制,Intel 上 ASR 子进程照样起不来。

### 用发布版

到 [Releases](https://github.com/jhfnetboy/AgentEar/releases) 下载 `AgentEar-*-macos-arm64.zip`,解压后把 `AgentEar.app` 拖进「应用程序」。

**首次运行要授权两项权限**(两者是独立的,各授权一次):

1. **辅助功能** —— 监听右 Command 键所需。启动时会弹授权框,或到「系统设置 → 隐私与安全性 → 辅助功能」把 AgentEar 加进去。**授权后必须重启程序才生效。**
2. **麦克风** —— 第一次按键录音时会弹窗。

没授权辅助功能也能用,会自动降级到 `Ctrl+Shift+R` 组合键。

> app 用**本地自签证书**签名(不是 ad-hoc)、未做公证。首次打开如果被 Gatekeeper 拦,
> 在「系统设置 → 隐私与安全性」里点「仍要打开」。
>
> 为什么不是 ad-hoc:ad-hoc 签名每次重新打包都产生新的 cdhash,而 macOS 把「辅助功能」
> 授权钉在 cdhash 上——后果是每次升级授权都静默失效,开关看着是开的、程序却报未授予。
> 见 `docs/m1-status.md`。

### 从源码构建

```bash
git clone git@github.com:jhfnetboy/AgentEar.git && cd AgentEar

# ASR 运行时与模型不入库,需要单独下载到 vendor/
mkdir -p vendor/bin vendor/models
curl -sL https://github.com/modelscope/FunASR/releases/download/runtime-llamacpp-v0.1.9/funasr-llamacpp-macos-arm64.tar.gz \
  | tar xz -C vendor/bin --strip-components=0
chmod +x vendor/bin/llama-funasr-*
B=https://huggingface.co/FunAudioLLM
curl -sL -o vendor/models/sensevoice-small-q8.gguf "$B/SenseVoiceSmall-GGUF/resolve/main/sensevoice-small-q8.gguf"
curl -sL -o vendor/models/fsmn-vad.gguf            "$B/fsmn-vad-GGUF/resolve/main/fsmn-vad.gguf"

# 泰语引擎(可选):静态单二进制,需要本地有 whisper.cpp 的 checkout
WHISPER_CPP=../whisper.cpp scripts/build-whisper-cli.sh

cargo build --release
scripts/bundle.sh        # → dist/AgentEar.app
```

泰语**模型**不用在这里准备——它由 app 按需下载。要自己从上游权重复现那份
GGML 产物(会校验指纹是否与 ADR-0004 记录一致):

```bash
scripts/build-thai-model.sh
```

## 用法

```bash
agentear                      # 守护进程,按右 Command 开始/停止
agentear --diagnose           # 环境自检:权限、音频设备、ASR 依赖
agentear --transcribe x.wav   # 离线转写,不占麦克风
agentear --debug-keys         # 打印每个修饰键事件,排查按键问题
```

菜单栏状态:`🎙` 空闲 / `● 7s` 录音中 / `◌ 转写中`

## 语言

**界面**和**识别**是两套语言设置,菜单里分开列,互不影响
(在泰国工作的英语用户要的是英文界面 + 泰语识别)。

| | 支持 |
|---|---|
| **界面语言** | English / 中文 / ไทย |
| **识别语言** | 自动(中 / 英 / 日 / 韩 / 粤,模型自己判)、ไทย 泰语(需显式选择) |

### 泰语要单独开,还要下模型

主链路的 SenseVoiceSmall **根本不支持泰语**——它的语种集合里只有
zh/en/yue/ja/ko,永远不可能输出泰语。所以泰语走的是**另一个引擎**
(whisper.cpp + Thonburian 泰语微调模型),模型 574 MB,**第一次在菜单里
选「识别语言 → ไทย」时才下载**,落到 `~/.agentear/models/`。

也可以提前下好:

```bash
agentear --fetch-thai            # 下载泰语模型(574 MB)
agentear --transcribe x.wav --lang th   # 不改配置试一下泰语链路
```

> **这不违背「不经过第三方服务器」那条约束。** 下模型是一次性的、
> 用户显式触发的,取的是模型权重;**你的音频和转写始终不出本机**。

**泰语现在的成色要说清楚**:纯泰语朗读的准确率测过(FLEURS,CER 0.062),
但**泰语夹英文技术词的场景没有任何可信数据**,而实测里 `review pull request`
会被音译成 `รีวิว พูล รีเควสต์`。泰文界面文案也**尚未经母语者校对**。
选型依据与它的局限见
[`docs/decisions/0004-thai-asr-engine.md`](docs/decisions/0004-thai-asr-engine.md)。

数据落在 `~/.agentear/`(`AGENTEAR_DATA` 可覆盖):

| 目录 | 内容 | 可否重建 |
|---|---|---|
| `raw/audio/` | **ASR 之前**的原始音频,内容寻址 | ❌ 丢了就没了 |
| `derived/transcripts/` | 转写结果 | ✅ 可从 raw 重算 |
| `agentear.log` | 日志 | |

## 附带的小工具:浏览器录音器

```bash
scripts/recorder.sh          # macOS / Linux，端口默认 8899
scripts/recorder.sh 8900     # 换端口
scripts\recorder.bat         # Windows（⚠️ 见下）
```

在浏览器里录自己的声音,拿去做**声音克隆 / TTS 参考音 / 转写校对**。
录音全程在本机,不上传任何地方;Ctrl-C 停掉服务。

**界面三语**(中 / English / ไทย),右上角切换,选择存在本机 localStorage。
首次打开跟随浏览器语言。⚠️ **泰文由非母语者撰写、未经母语者校对**
(与 `src/i18n.rs` 同一状态),泰语界面底部对使用者明说了这一点。

⚠️ **`scripts/recorder.bat` 没有在真实 Windows 上跑过**(开发机是 macOS)。
里面两段 Python 单行命令单独验过(端口探测、就绪探测),
但 cmd.exe 的语法部分未验证。跑不起来请把报错发回来。

**为什么要起个 HTTP 服务而不是直接双击 HTML**:`getUserMedia` 只在
**安全上下文**里可用。`file://` 不算,而 `http://127.0.0.1` 算。
直接开文件的话麦克风一定拿不到,而且报的错会指向权限,很难查。
服务只绑 `127.0.0.1`,不暴露到局域网。

它做的事:

- 贴一段脚本,按**空行**分段;留空则是自由录音
- **目标采样率可选**,并在界面上说明各自的用途
- 录完当场体检:时长、峰值、SNR、有声帧占比、削波、以及**采集时间线有没有洞**
  (时间线有洞不可人工放行 —— 听起来往往只是有点跳,但科学上不可用)
- **一键打包 .zip**,里面含 `manifest.txt`:文本、WAV 文件名与 SHA-256、
  采集参数(含 `getSettings()` 报的**实际生效**值)、质量指标

⚠️ **采样率别照抄泰语那个页面。** `docs/thai-recorder.html` 写死 16 kHz,
因为它是 ASR 语料工具;**拿 16 kHz 做声音克隆音色会明显变闷** ——
8 kHz 以上全被丢掉,而那正是齿音和空气感所在。所以这个工具默认
「原样(不重采样)」,主流 TTS 栈原生是 22.05 / 24 / 32 kHz。

信号处理与采集在 `docs/recorder-core.js`,两个页面共用同一份 ——
那套竞态和抗混叠滤波器改了五轮,**不要复制第二份**。

## 技术选型

**ASR:SenseVoiceSmall q8**(242 MiB,Apache-2.0),经四模型实测横比后选定——详见 [ADR-0001](docs/decisions/0001-asr-model-selection.md)。

关键权衡:CER 四家相差不到 1 个百分点(准确率无法区分),差异全在工程形态。SenseVoice 常驻内存只有 Fun-ASR-Nano 的 27%、冷启动 0.2s vs 11.45s、单二进制零 Python。

**因为冷启动只要 0.2 秒,模型不需要常驻**——每次录音结束才起子进程转写,用完即退。所以守护进程空闲时只占 13 MB。

## 已知限制

- **蓝牙耳机会被优先选中**。系统默认输入是蓝牙耳机时走 HFP 模式,采样率被压到 16 kHz 且带宽窄,会拉低准确率。用内置麦克风(48 kHz)效果更好。
- **右 Command 可能误触发**。它是常用修饰键,按 ⌘C / ⌘V 时如果用右手那个会误启动录音。
- **中英混杂技术术语不可靠**。实测 `raw` 一词四个 ASR 模型全错(row / road / ro / roll)。这不是换模型能解决的,计划在 M2 用本地 LLM 结合上下文纠错。
- **长音频需分段**。RSS 随音频长度增长约 0.52 MB/s,约 54 分钟破 2 GiB。当前录音上限 300 秒自动停止。

## 文档

| | |
|---|---|
| [milestones.md](docs/milestones.md) | 里程碑与验收标准 |
| [decisions/](docs/decisions/) | 架构决策记录(ADR) |
| [benchmarks.md](docs/benchmarks.md) | ASR 实测数据与四模型横比 |
| [m1-status.md](docs/m1-status.md) | M1 实现细节与 macOS 踩坑记录 |
| [ingest-design.md](docs/ingest-design.md) | 音频接入层设计(硬件改造、传输协议) |

## 许可

Apache-2.0。内置第三方组件的许可见 [NOTICE](NOTICE)。
