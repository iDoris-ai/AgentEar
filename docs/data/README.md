# 测量记录与逐样本数据

ADR-0004 的数字从这里来。**保留逐样本数据**，免得只剩一张手工整理的表
——出了分歧无从对账，也无法重算统计量。

⚠️ 两份东西的等级不一样，标题里那个「原始」曾经把它们混为一谈：
`thai-cer-per-sample/*.json` 是**脚本原样输出**、可完全重算；
`thai-bench-raw.txt` 是**人工誊抄**、且已无法重算（见该文件头）。

| 文件 | 内容 | 怎么重算 |
|---|---|---|
| `thai-bench-raw.txt` | 性能（体积/RTF/峰值 RSS） | ⚠️ **人工誊抄，非脚本原样输出；模型已不在本机，现阶段无法重算**。见文件头 |
| `thai-cer-per-sample/*.json` | **逐句** hyp、edits、参考长度 | `scripts/cer-thai.py <模型> <cli> <数据目录>` |
| `thai-cer-stats.txt` | CI 与配对比较 | `scripts/cer-stats.py docs/data/thai-cer-per-sample 4000` — 除开头 3 行说明外**逐字节一致，已验**。⚠️ 它会 **exit 4**：这六份结果产出于指纹机制之前，「跑在同一批录音上」这个前提无法事后验证 |

统计量**不需要重跑推理**——`cer-stats.py` 直接吃 `thai-cer-per-sample/`，
种子固定（CI=20260822、配对=20260823），任何人重跑必得同一组数字。

**性能那张表则相反**：它依赖本机上的 .bin，而那些文件已经删了。
两张表的可复算等级不一样，不要混着说。

## 评测集

`scripts/fleurs-thai-manifest.json` —— 80 条 FLEURS 泰语 test 的 id 与参考文本，
含指纹。`scripts/fleurs-thai-fetch.py` 重新取样时会与它对账，
上游数据变了会直接报错而不是悄悄给出不可比的数字。

音频不入库（80 条 wav 约 32 MB；生成它需要先下约 753 MB 的上游 parquet），
用 `fleurs-thai-fetch.py` 重新生成。
来源 `google/fleurs` 配置 `th_th` split `test`，**CC-BY-4.0**。

## 环境

- MacBook Pro M1 Max / 64 GB，macOS 25.4.0（Darwin）
- whisper.cpp `7de8dd78`，CMake Release，**CPU 后端，4 线程**
- 解码：贪心，`-bo 1 -bs 1`，`-l th` 强制语言
- 机器空闲；文件缓存热
- AgentEar 分支 `c1-thai-asr-baseline`

## 模型

每个 `.bin` 的 sha256 前 12 位记在**逐样本 JSON**（字段 `model_sha256_12`）里，
用来对上是哪份产物。⚠️ **性能表没有这一列** —— 誊抄时丢掉了，见
`thai-bench-raw.txt` 文件头。

| 代号 | HF 仓库 | revision | 转换 |
|---|---|---|---|
| `medium` | `biodatlab/whisper-th-medium-combined` | `eebf84255cc7f242a504f64ec09ec33d32903fe1` | `convert-h5-to-ggml.py` 直转 |
| `distill` | `biodatlab/distill-whisper-th-large-v3` | `62df42cecab9f484226ad5f9afdb557552021bbb` | 同上 |
| `turbo` | `typhoon-ai/typhoon-whisper-turbo` | `3c03fa84c26f172944422ceb8a4e88a2dbc08b10` | **先用 transformers 以 fp32 重存**（原权重 bf16，转换脚本会崩） |

量化用 whisper.cpp 的 `quantize`，q5_0 / q8_0。

⚠️ **revision 是事后补记的，不是下载时记录的。**
下载发生在 2026-08-22，上面三个 sha 是当天稍晚从 HF API 查到的仓库 HEAD。
**没有证据证明下载那一刻的 HEAD 与此相同**——若上游当天做过更新，就对不上。
下次必须在下载时就记下来。

同样没有记录：原始权重文件的 SHA-256、转换命令的脚本 commit。
GGML 产物的 sha256 只存了**前 12 位**（在逐样本 JSON 里；性能表没有这一列）。

**2026-08-28 更新：「HF revision → 转换 → 量化产物」这一段已经验过了。**
按记录的 revision 重新下载 `distill`、用同一个 whisper.cpp commit（`7de8dd78`）
转换量化，产物 sha256 前 12 位 `5bfc04f1931a` 与入库记录一致；
控制组（同一 f32 中间文件量化两遍）证明工具本身是确定的，所以这次对账
携带信息。详见 ADR-0004 §2「这条来源链是可复现的」。

**仍然不完整的**：原始权重文件的 SHA-256 当时没记，所以只能证明
「今天下到的和今天转出来的自洽」，证明不了它等于 2026-08-22 下到的那一份。
本基线仍属「部分可复现」，但缺口比原来小一格。
