# 原始测量输出

ADR-0004 表格里的数字是从这里整理来的。**保留原始输出**，免得只剩一张手工
整理过的表——出了分歧无从对账。

- `thai-bench-raw.txt` —— `scripts/bench-thai.sh` 的性能输出（自编合成语料）
- `thai-cer-fleurs-raw.txt` —— `scripts/cer-thai.py` 在 FLEURS 泰语 test 上的 CER

## 测量环境

- MacBook Pro M1 Max / 64 GB，macOS 25.4.0（Darwin）
- whisper.cpp `7de8dd78`，CMake Release，CPU 后端，4 线程
- 解码：贪心，`-bo 1 -bs 1`，`-l th` 强制语言
- 机器空闲；文件缓存热

## 模型来源

| 代号 | HF 仓库 | 转换 |
|---|---|---|
| `medium` | `biodatlab/whisper-th-medium-combined` | `convert-h5-to-ggml.py` 直转 |
| `distill` | `biodatlab/distill-whisper-th-large-v3` | 同上 |
| `turbo` | `typhoon-ai/typhoon-whisper-turbo` | **先用 transformers 以 fp32 重存**（原权重 bf16，转换脚本会崩） |

量化用 whisper.cpp 的 `quantize`，q5_0 / q8_0。
`bench-thai.sh` 会打印每个 `.bin` 的 sha256 前 12 位，用来对上是哪一份产物。

**未记录 HF revision**，这是个缺口——重跑时若上游更新过权重，数字可能对不上。
