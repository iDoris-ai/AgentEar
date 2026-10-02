# Qwen3-ASR 常驻服务内存：根因验证（T3.5.9 第 1 步，2026-10-02）

## 问的是什么

speech-server（speech-swift v0.0.28）跑多段长度不同的录音后内存一路涨到 ~50 GB。
读源码的推断是：MLX 缓存池没有上限（0.6B 不走上游那段 `if modelSize == .large` 的保护），
且服务端每次请求后不清缓存。**这里验证这个推断。**

## 怎么做的

- 从 speech-swift `v0.0.28`（`231f8eb`）源码编 `speech-server`，打 [`speech-swift-v0.0.28-cache.patch`](speech-swift-v0.0.28-cache.patch)：
  `qwen3-asr` 分支里若设了 `AGENTEAR_MLX_CACHE_MB`，则 `MLX.Memory.cacheLimit = N MB`
  且请求结束 `MLX.Memory.clearCache()`；**不设 = 原版行为**。所以「自编、不开补丁」与「自编 + 补丁」
  是**同一个二进制**，只差一个环境变量。
- ⚠️ 本机没装 Xcode Metal Toolchain，`mlx.metallib` 用的是**官方 v0.0.28 发布包里那份**
  （同 tag、同一锁定的 mlx-swift）；三组的 GPU 着色器完全相同。
- 语料：jason 2026-10-02 当天 25 段真实录音（长度见 [`durations.txt`](durations.txt)，2.5–113.5 s）。
  **转写文字不入库**（私人语音内容），只入数字。
- 脚本 [`exp.sh`](exp.sh)：起一个独立 speech-server（同 AgentEar 的断网沙箱），逐段 POST，
  请求期间每 0.2 s 采样 `footprint`（`phys_footprint`，含 Metal），记下峰值与转完后的值。
- 0.6B（`qwen3-asr-0.6b-mlx-int4`），不带 context。

## 结果（[`results.tsv`](results.tsv)：段号 / 音频秒 / 转写秒 / 请求中峰值 MB / 转完 MB）

| 组 | 轮 | 前 20 段总耗时 | 中位 | 请求中峰值最大 | 末态 |
|---|---|---|---|---|---|
| 官方 v0.0.28 | 1 | 33.5 s | 1.94 s | 50 176 MB | 50 176 MB |
| 自编、不开补丁 | 1 / 2 / 3 | 38.5 / 37.3 / **65.0** s | 1.96–1.97 s | 47–49 GB | 47–49 GB |
| **自编 + 补丁（256 MB）** | 1 / 2 / 3 | 39.1 / 37.5 / 34.8 s | 1.97–2.11 s | **2.36–2.43 GB** | **≈ 865 MB** |

- **累积消失**：三轮补丁版末态都 ≈ 865 MB；不开补丁三轮都涨到 47–49 GB。
- **「112 s 单段 ≈ 15 GB」也是缓存**：补丁版最长一段（113.5 s）峰值 2.4 GB。
  → 原计划的「长音频切段」**很可能不需要**（待方案 B 上线后在守护进程里复测）。
- **速度没检出差异**：同一二进制补丁 / 不补丁三轮前 20 段 39.1/38.5、37.5/37.3、34.8/65.0 s，中位都 ≈ 1.97 s。
  第 3 轮不开补丁那组的 65 s 是**涨到 47 GB 后进了 swap**——第 21 段（89.9 s）300 s 超时，
  **正是 jason 在守护进程里遇到的症状**；之后的段未跑完（实验中止，`n=21`）。
  官方二进制比自编快约 12%（33.5 vs 38.5 s，仅 1 轮），是**编译差异**，与补丁无关。
- **文字逐字一致**：补丁版第 1 轮与自编、官方两组第 1 轮 25/25 段 `cmp` 相同。

## 不能外推的

- 只有 0.6B；1.7B 本身已有上游的 4 GB 上限保护，没测。
- 只量了速度与内存，**不是准确率评测**（文字一致只说明补丁不改结果）。
- 256 MB 是照 TTS 边车（v0.17.0）的取值，没扫别的档位。

## 复现

```bash
git clone --depth 1 --branch v0.0.28 https://github.com/soniqo/speech-swift && cd speech-swift
git apply <本目录>/speech-swift-v0.0.28-cache.patch
swift build -c release --product speech-server   # 约 5.5 分钟（M1 Max）
cp ~/.agentear/models/qwen3/runtime/speech-v0.0.28/{mlx.metallib,*.bundle} .build/release/   # 没 Metal Toolchain 时
BIN=.build/release/speech-server TAG=patched AGENTEAR_MLX_CACHE_MB=256 OUT=/tmp/exp exp.sh wavs.txt
BIN=.build/release/speech-server TAG=selfbuilt                         OUT=/tmp/exp exp.sh wavs.txt
```
