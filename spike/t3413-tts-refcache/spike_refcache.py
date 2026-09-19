"""方案 A spike：验证「参考音色的前向能不能只算一次、跨请求复用」。

丢弃式，不进主工程。复制 mlx_audio VoxCPM2.generate() 里 Mode 3（Reference
cloning only）分支的输入构造逻辑，拆成两段：
  1) 只对参考音色部分跑一次 base_lm/residual_lm 前向，拿到 cache
  2) 对不同的新文本，复用同一份 cache 继续跑

对照组：调用未改动的 model.generate() 从头跑一遍（含参考音色）。
比较：耗时差异 + 输出音频是否等价（F0/时长，非逐字节）。
"""
import sys, time, json
sys.path.insert(0, "/Users/jason/Dev/tools/AgentEar/services/tts")
from mlx_audio.tts.utils import load_model
import mlx.core as mx
import mlx.nn as nn
import numpy as np

MODEL_PATH = "/Users/jason/.agentear/talk/models/voxcpm2-4bit"
VOICES_DIR = "/Users/jason/.agentear/talk/voices"
with open(f"{VOICES_DIR}/男声.json") as f:
    ref_json = json.load(f)
REF_TEXT = ref_json["ref_text"]
REF_AUDIO = f"{VOICES_DIR}/男声.wav"
INSTRUCT = "亲切自然，像跟朋友聊天，语气有起伏，带一点微笑，Mandarin"

print("loading model...")
model = load_model(MODEL_PATH)

# ---------------------------------------------------------------------------
# 1) 复刻 generate() 里构造参考音色前缀的那部分（Mode 3 分支，只到 ref 为止）
# ---------------------------------------------------------------------------
def build_ref_prefix():
    ref_feat = model._encode_wav(REF_AUDIO, padding_mode="right")
    ref_tokens, ref_feats, ref_t_mask, ref_a_mask = model._make_ref_prefix(ref_feat)
    return ref_tokens, ref_feats, ref_t_mask, ref_a_mask

def embed(text_token, audio_feat, text_mask, audio_mask, scale_emb):
    text_token = text_token[None, :]
    audio_feat = audio_feat[None, :, :, :]
    text_mask = text_mask[None, :]
    audio_mask = audio_mask[None, :]
    feat_embed = model.feat_encoder(audio_feat)
    feat_embed = model.enc_to_lm_proj(feat_embed)
    text_embed = model.base_lm.embed_tokens(text_token) * scale_emb
    combined = text_mask[:, :, None] * text_embed + audio_mask[:, :, None] * feat_embed
    return combined, feat_embed, text_mask, audio_mask

scale_emb = model.args.lm_config.scale_emb if model.args.lm_config.use_mup else 1.0

print("\n--- 建立参考音色缓存（只做一次） ---")
t0 = time.perf_counter()
ref_tokens, ref_feats, ref_t_mask, ref_a_mask = build_ref_prefix()
ref_combined, ref_feat_embed, ref_text_mask, ref_audio_mask = embed(
    ref_tokens, ref_feats, ref_t_mask, ref_a_mask, scale_emb
)
enc_out_ref, lm_cache_ref = model.base_lm(ref_combined)
enc_out_ref_fsq = (
    model.fsq_layer(enc_out_ref) * ref_audio_mask[:, :, None]
    + enc_out_ref * ref_text_mask[:, :, None]
)
res_in_ref = model.fusion_concat_proj(
    mx.concatenate([enc_out_ref_fsq, ref_audio_mask[:, :, None] * ref_feat_embed], axis=-1)
)
res_out_ref, res_cache_ref = model.residual_lm(res_in_ref)
mx.eval(lm_cache_ref, res_cache_ref)
ref_build_time = time.perf_counter() - t0
print(f"参考音色缓存建立耗时：{ref_build_time:.3f}s（缓存长度：{lm_cache_ref[0][0].shape[1]} 个 token/patch 位置）")

# 保存 prefix_feat_cond（最后一个 audio_feat 位置，供第一个 patch 的 DiT 条件用）
prefix_feat_cond_ref = ref_feats[None, -1:, :, :].transpose(0, 1, 2, 3)[:, 0, :, :]  # (B, P, D)

lm_hidden_ref_last = enc_out_ref_fsq[:, -1, :]
residual_hidden_ref_last = res_out_ref[:, -1, :]

# ---------------------------------------------------------------------------
# 2) 对新文本：只跑「新文本」这一小段，复用上面缓存的 cache 继续
# ---------------------------------------------------------------------------
def continue_with_cache(text, lm_cache_ref, res_cache_ref, lm_hidden_ref_last, residual_hidden_ref_last, prefix_feat_cond_ref, label):
    print(f"\n--- 复用缓存，只处理新文本「{text}」（{label}） ---")
    t0 = time.perf_counter()
    text_ids = model._tokenize(f"({INSTRUCT}){text}")
    text_token = mx.array(text_ids + [model.audio_start_token], dtype=mx.int32)
    text_length = text_token.shape[0]
    latent_dim = model.audio_vae.latent_dim
    audio_feat = mx.zeros((text_length, model.patch_size, latent_dim))
    text_mask = mx.ones(text_length, dtype=mx.float32)
    audio_mask = mx.zeros(text_length, dtype=mx.float32)

    combined, feat_embed, tmask, amask = embed(text_token, audio_feat, text_mask, audio_mask, scale_emb)

    try:
        enc_out, lm_cache_new = model.base_lm(combined, cache=lm_cache_ref)
    except Exception as e:
        print(f"  ❌ base_lm 多 token 续接缓存报错：{type(e).__name__}: {e}")
        return None, time.perf_counter() - t0

    enc_out_fsq = model.fsq_layer(enc_out) * amask[:, :, None] + enc_out * tmask[:, :, None]
    res_in = model.fusion_concat_proj(mx.concatenate([enc_out_fsq, amask[:, :, None] * feat_embed], axis=-1))
    try:
        res_out, res_cache_new = model.residual_lm(res_in, cache=res_cache_ref)
    except Exception as e:
        print(f"  ❌ residual_lm 多 token 续接缓存报错：{type(e).__name__}: {e}")
        return None, time.perf_counter() - t0

    mx.eval(enc_out_fsq, res_out)
    text_forward_time = time.perf_counter() - t0
    print(f"  新文本前向耗时：{text_forward_time:.3f}s")

    lm_hidden = enc_out_fsq[:, -1, :]
    residual_hidden = res_out[:, -1, :]
    prefix_feat_cond = prefix_feat_cond_ref  # 文本部分 audio_feat 全零，跟参考末尾一致

    # --- 后续 patch 生成循环（照抄 generate() 的循环体） ---
    pred_feat_seq = []
    lm_cache = lm_cache_new
    res_cache = res_cache_new
    for i in range(200):
        dit_h1 = model.lm_to_dit_proj(lm_hidden)
        dit_h2 = model.res_to_dit_proj(residual_hidden)
        dit_h = mx.concatenate([dit_h1, dit_h2], axis=-1)
        cond_in = prefix_feat_cond.transpose(0, 2, 1)
        pred_feat = model.feat_decoder.sample(mu=dit_h, n_timesteps=10, patch_size=model.patch_size, cond=cond_in, cfg_value=2.0)
        pred_feat = pred_feat.transpose(0, 2, 1)
        pred_feat_seq.append(pred_feat[:, None, :, :])
        curr_embed = model.feat_encoder(pred_feat[:, None, :, :])
        curr_embed = model.enc_to_lm_proj(curr_embed)
        stop_logits = model.stop_head(nn.silu(model.stop_proj(lm_hidden)))
        stop_flag = mx.argmax(stop_logits, axis=-1).item()
        if i > 2 and stop_flag == 1:
            break
        new_lm_out, lm_cache = model.base_lm(inputs_embeds=curr_embed, cache=lm_cache)
        lm_hidden = new_lm_out[:, -1, :]
        lm_hidden = model.fsq_layer(lm_hidden)
        curr_res_in = model.fusion_concat_proj(mx.concatenate([lm_hidden[:, None, :], curr_embed], axis=-1))
        new_res_out, res_cache = model.residual_lm(inputs_embeds=curr_res_in, cache=res_cache)
        residual_hidden = new_res_out[:, -1, :]
        prefix_feat_cond = pred_feat

    all_feats = mx.concatenate(pred_feat_seq, axis=1)
    B = all_feats.shape[0]
    all_feats_flat = all_feats.reshape(B, -1, model.feat_dim)
    audio = model.audio_vae.decode(all_feats_flat)
    audio = audio.flatten()
    mx.eval(audio)
    total_time = time.perf_counter() - t0
    print(f"  含后续 patch 生成总耗时：{total_time:.3f}s（{len(pred_feat_seq)} 个 patch）")
    return np.array(audio), total_time

def wave_write(path, audio, sr=48000):
    import wave
    pcm = (np.clip(audio, -1, 1) * 32767).astype("<i2")
    with wave.open(path, "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
        w.writeframes(pcm.tobytes())

TEXTS = ["现在几点了", "今天天气怎么样啊"]
for i, text in enumerate(TEXTS):
    audio, t = continue_with_cache(text, lm_cache_ref, res_cache_ref, lm_hidden_ref_last, residual_hidden_ref_last, prefix_feat_cond_ref, f"cached-{i}")
    if audio is not None:
        wave_write(f"/tmp/refcache_cached_{i}.wav", audio)

print("\n--- 对照组：未改动的 model.generate()（从头带参考音色跑）---")
for i, text in enumerate(TEXTS):
    t0 = time.perf_counter()
    chunks = []
    sr = None
    for seg in model.generate(text=text, max_tokens=2000, ref_audio=REF_AUDIO, ref_text=REF_TEXT,
                               instruct=INSTRUCT, inference_timesteps=10):
        if getattr(seg, "audio", None) is not None:
            chunks.append(seg.audio)
            sr = getattr(seg, "sample_rate", None) or sr
    baseline_time = time.perf_counter() - t0
    full = np.concatenate([np.array(c) for c in chunks])
    wave_write(f"/tmp/refcache_baseline_{i}.wav", full, sr or 48000)
    print(f"[baseline-{i}] 「{text}」总耗时：{baseline_time:.3f}s")

# ---------------------------------------------------------------------------
# 3) 补丁：mask 构造没考虑 cache 场景下的偏移，导致多 token 续接直接报错。
#    标准修法：causal mask 要覆盖 (L, offset+L)，前 offset 列全通过（可见已缓存内容），
#    后 L×L 是常规下三角因果 mask。
# ---------------------------------------------------------------------------
import types

def patched_minicpm_call(self, inputs_embeds=None, input_ids=None, mask=None, cache=None, is_causal=True):
    if inputs_embeds is None:
        inputs_embeds = self.embed_tokens(input_ids)
    B, L, D = inputs_embeds.shape
    offset = 0
    if cache is not None:
        offset = cache[0][0].shape[1]
    if self.rope is not None:
        position_ids = mx.arange(offset, offset + L).astype(mx.int32)
        cos, sin = self.rope(position_ids)
        cos = cos[None, :, :]
        sin = sin[None, :, :]
    else:
        cos, sin = None, None

    if mask is None and is_causal and L > 1:
        causal = mx.triu(mx.full((L, L), float("-inf")), k=1)
        if offset > 0:
            pad = mx.zeros((L, offset))
            causal = mx.concatenate([pad, causal], axis=1)
        mask = causal[None, None, :, :]

    h = inputs_embeds
    new_caches = []
    for i, layer in enumerate(self.layers):
        layer_cache = cache[i] if cache is not None else None
        h, c = layer(h, cos, sin, mask=mask, cache=layer_cache)
        new_caches.append(c)
    h = self.norm(h)
    return h, new_caches

# __call__ 是 dunder，实例属性赋值对 obj(...) 语法不生效，必须打在类上
type(model.base_lm).__call__ = patched_minicpm_call
type(model.residual_lm).__call__ = patched_minicpm_call
print('patched classes:', type(model.base_lm).__name__, type(model.residual_lm).__name__, type(model.base_lm) is type(model.residual_lm))

print("\n\n========== 打补丁后重跑 ==========")
for i, text in enumerate(TEXTS):
    audio, t = continue_with_cache(text, lm_cache_ref, res_cache_ref, lm_hidden_ref_last, residual_hidden_ref_last, prefix_feat_cond_ref, f"patched-{i}")
    if audio is not None:
        wave_write(f"/tmp/refcache_patched_{i}.wav", audio)
        print(f"  ✅ 生成成功，已存 /tmp/refcache_patched_{i}.wav")
