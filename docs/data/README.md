# 原始测量输出

ADR-0004 的数字从这里来。**保留原始与逐样本数据**，免得只剩一张手工整理的表
——出了分歧无从对账，也无法重算统计量。

| 文件 | 内容 | 怎么重算 |
|---|---|---|
| `thai-bench-raw.txt` | 性能（体积/RTF/峰值 RSS） | `scripts/bench-thai.sh <模型...>` |
| `thai-cer-per-sample/*.json` | **逐句** hyp、edits、参考长度 | `scripts/cer-thai.py <模型> <cli> <数据目录>` |
| `thai-cer-stats.txt` | CI 与配对比较 | `scripts/cer-stats.py docs/data/thai-cer-per-sample 4000` |

统计量**不需要重跑推理**——`cer-stats.py` 直接吃 `thai-cer-per-sample/`，
种子固定（CI=20260822、配对=20260823），任何人重跑必得同一组数字。

## 评测集

`scripts/fleurs-thai-manifest.json` —— 80 条 FLEURS 泰语 test 的 id 与参考文本，
含指纹。`scripts/fleurs-thai-fetch.py` 重新取样时会与它对账，
上游数据变了会直接报错而不是悄悄给出不可比的数字。

音频不入库（约 100 MB），用 `fleurs-thai-fetch.py` 重新生成。
来源 `google/fleurs` 配置 `th_th` split `test`，**CC-BY-4.0**。

## 环境

- MacBook Pro M1 Max / 64 GB，macOS 25.4.0（Darwin）
- whisper.cpp `7de8dd78`，CMake Release，**CPU 后端，4 线程**
- 解码：贪心，`-bo 1 -bs 1`，`-l th` 强制语言
- 机器空闲；文件缓存热
- AgentEar 分支 `c1-thai-asr-baseline`

## 模型

每个 `.bin` 的 sha256 前 12 位记在逐样本 JSON 与性能表里，用来对上是哪份产物。

| 代号 | HF 仓库 | 转换 |
|---|---|---|
| `medium` | `biodatlab/whisper-th-medium-combined` | `convert-h5-to-ggml.py` 直转 |
| `distill` | `biodatlab/distill-whisper-th-large-v3` | 同上 |
| `turbo` | `typhoon-ai/typhoon-whisper-turbo` | **先用 transformers 以 fp32 重存**（原权重 bf16，转换脚本会崩） |

量化用 whisper.cpp 的 `quantize`，q5_0 / q8_0。

⚠️ **未记录 HF revision。** 上游若更新过权重，重跑可能对不上号。
这是本次基线的已知缺口，ADR 定稿前应补。
