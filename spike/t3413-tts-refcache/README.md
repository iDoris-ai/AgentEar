# T3.4.13 方案 A spike：参考音色的前向能不能只算一次、跨请求复用

丢弃式 spike，**不进主工程**。结论见 [`docs/decisions/0009-tts-chunked-streaming.md`](../../docs/decisions/0009-tts-chunked-streaming.md)。

`spike_refcache.py` —— 复刻 `mlx_audio` VoxCPM2 `generate()` 里 Mode 3（参考音色克隆）
分支的输入构造逻辑，拆成两段：① 只对参考音色部分跑一次 `base_lm`/`residual_lm`
前向拿到 KV-cache；② 对不同的新文本，复用同一份 cache 继续跑生成循环。
对照组是未改动的 `model.generate()`（每次都带着参考音色从头跑）。

```bash
source ~/.agentear/llm/venv/bin/activate
python3 spike_refcache.py
```

## 结论：**方案可行，但 `mlx_audio` 当前实现有一个阻塞性 bug，需要打一个标准补丁**

### 1. 不打补丁：直接复用缓存会报错

```
ValueError: [broadcast_shapes] Shapes (1,1,35,35) and (1,16,35,654) cannot be broadcast.
```

根因：`mlx_audio/tts/models/voxcpm2/minicpm.py::MiniCPMModel.__call__` 构造因果
mask 时只考虑了"当前这批 L 个 token 互相看"（`(L,L)` 方阵），没考虑"cache 里已经
有 `offset` 个位置、新 token 还要看得见它们"这件事——`Attention.__call__` 会把
`k_cache`/`v_cache` 拼上新的 k/v（变成 `offset+L` 长），但 mask 还是 `(L,L)`，
形状对不上。**这条路径在原实现里从没被走过**：唯一用到 `cache` 的地方是
自回归循环，每次只喂 1 个新 token（`L=1`），`L=1` 时代码走的是另一个分支
（`mask is None and is_causal and L>1` 这个条件不成立，`mask` 保持 `None`），
所以这个 bug 一直没被暴露过。

### 2. 打补丁后：可行，而且收益比 Phase 0 估算的更大

标准修法——mask 要覆盖 `(L, offset+L)`：前 `offset` 列全通过（已缓存内容永远可见），
后 `L×L` 是常规下三角因果 mask。补丁只有十几行，加在 `spike_refcache.py` 里用
`types.MethodType`（准确说是打在类上，`__call__` 是 dunder，打实例属性对
`obj(...)` 语法不生效，这个坑也记一笔）。

打完补丁，两句新文本复用同一份参考音色缓存，**都生成成功**：

| | 参考音色缓存建立（只做一次） | 新文本前向（复用缓存） | 含后续 patch 生成总耗时 | 对照组（从头带参考音色） |
|---|---|---|---|---|
| 「现在几点了」 | 3.91s | **0.079s** | **1.242s** | 4.19s |
| 「今天天气怎么样啊」 | （复用同一份缓存） | **0.081s** | **1.379s** | 4.53s |

**总耗时降到约 1/3**（4.19s→1.24s，4.53s→1.38s），**比 Phase 0 用"总耗时占比"
估算的 55%/37% 降幅更好**——因为 Phase 0 的估算只扣掉了"参考上下文编码"这一段，
没算上"新文本前向"本身也从 4.79ms/token 级别的独立计算变成了几乎免费的续接
（0.08s 处理一整句新文本，比单独跑一次要快得多）。

⚠️ **这是一次性缓存建立的成本**（3.91s，比对照组的 4.19s 还贵）——只有在
**同一个音色被连续多次使用**时才划算，第一次用某个音色仍然要付这笔钱。
边车是常驻进程，实际部署时应该在**启动时或首次用到某音色时**建好缓存、
之后常驻复用，而不是每次请求都重建。

### 3. 正确性：抽样检查，不是穷尽验证

- `scripts/measure-f0.py` 对四条样本（baseline×2 + patched×2）：F0 中位数全部落在
  男声带（85–155Hz，参考音色本身是 142.4Hz），没有出现"性别跳变"这类
  明显错误；时长量级相近（1.12–1.60s）。
- **两条都拿 `afplay` 实际播放过，人耳确认不是乱码/卡顿**（这一步的判断
  留给 jason，见对话记录）。
- ⚠️ **不是逐字节比对**——VoxCPM2 扩散采样没有固定 seed，同一份缓存、
  同一句话重复合成本来就会有正常的采样方差（这一点在 T3.4.13 的 steps=9
  测试里已经验证过一次），F0/RMS 有差异是预期的，不代表缓存续接本身有问题。
- **没测的**：不同 `instruct`（语气/方言切换）会不会让参考部分的编码结果
  跟着变（如果会，缓存要按"音色×instruct"分粒度，不能只按音色分）；
  长文本（多句、需要句子级流水线配合）下的行为；缓存内存占用（`lm_cache_ref`/
  `res_cache_ref` 长期常驻要占多少内存，没测）。

## 下一步（Phase 2 范围，不在本次 spike 里做）

1. 验证 `instruct` 是否影响参考部分编码，确定缓存分粒度策略。
2. 在 `services/tts/backends.py` 里包一层（不改已安装的 `mlx_audio` 包本身，
   升级会冲掉补丁）：启动时或首次用到某音色时建立并常驻缓存。
3. 测长文本、测内存占用、测边车重启后缓存重建的耗时是否可接受。
