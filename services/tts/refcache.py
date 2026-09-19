"""Reference-voice forward-pass caching for VoxCPM2 (ADR-0009 §5.1, T3.4.13 Phase 2).

**What this buys**: every ``VoxCPM2.Model.generate()`` call re-encodes the
reference **audio** — its whole duration, turned into a long latent sequence —
through ``base_lm``'s initial forward pass. Measured 2026-09-19 at **~2.8 s**
for a ~99 s reference clip, regardless of how short the reply is
(``docs/decisions/0009-tts-chunked-streaming.md`` §3.3). ⚠️ **Not** the
reference *text*: Mode 3 (see ``build_ref_cache``'s docstring) never reads
``ref_text`` at all — an earlier draft of this module's docs blamed a
"465-character reference text" for the cost, which was wrong and has been
corrected; the two known voices' warmup times (男声 3.56 s / 98.7 s audio vs.
女声 1.05 s / 10.9 s audio) track audio duration, not any text length. That
cost is identical every time the *same* reference audio is used, so it is
wasted work: this module runs it **once per voice**, keeps the resulting
KV-cache resident, and has later requests continue from it instead of
starting over.

Spike results (``spike/t3413-tts-refcache/``, 2026-09-19): total synthesis time
drops from ~4.2–4.5 s to ~1.2–1.4 s for short replies — about a 3x speedup, not
just the ~55%/37% the raw time-accounting in ADR-0009 §3 suggested, because the
new-text forward pass itself also gets cheap (continuing 8-10 tokens onto an
existing cache costs ~0.08 s, versus paying for the whole combined sequence
from scratch).

## The bug this had to work around

``mlx_audio``'s ``MiniCPMModel.__call__`` builds a plain ``(L, L)`` causal mask
whenever it is asked to process more than one new position (``L > 1``) with
``mask=None``. That is correct when there is **no existing cache** (the normal
first pass over a whole sequence), but wrong when there **is** a cache: the
attention keys/values are ``cache_len + L`` long after ``Attention.__call__``
concatenates the cache on, so an ``(L, L)`` mask cannot broadcast against
scores of shape ``(..., L, cache_len + L)`` — MLX raises ``ValueError:
broadcast_shapes`` immediately. This path was never exercised before: the only
place ``mlx_audio`` itself uses ``cache=`` is the per-patch autoregressive
loop, and that always continues **one token at a time** (``L == 1``), which
sidesteps the buggy branch entirely (the library's own ``L > 1`` check is
false, so ``mask`` stays ``None`` and single-token decode just works).

``patch_multi_token_cache_continuation`` fixes exactly this one combination —
``cache is not None and L > 1`` — by computing the correct
``(L, cache_len + L)`` mask ourselves (first ``cache_len`` columns fully
visible, trailing ``L × L`` block causal) and handing it to the *original*
``__call__`` as an explicit ``mask=`` argument, so every other code path
(``L == 1`` continuation, or ``L > 1`` with no cache) is untouched — verified
by the fact that ``build_ref_cache`` below, which does exactly that "``L > 1``,
no cache" case, still works unpatched.

⚠️ This patches ``mlx_audio``'s installed package **in this process**, not the
package on disk — it is not persisted, does not survive a `pip install`
upgrade of ``mlx-audio``, and must be re-applied (harmlessly idempotent) after
every model load. If a future ``mlx-audio`` version fixes this upstream, this
patch becomes a no-op once ``_agentear_refcache_patched`` stops matching the
new class, at which point it should be deleted rather than layered on top.

## Failure is not fatal

Every function here can raise (missing model attributes on an unexpected
``mlx_audio`` version, a genuinely malformed reference file, ...). Callers
**must** treat that as "skip the optimization, fall back to
``model.generate()``" — never as a reason to refuse to speak. See
``VoxCpm2Backend._generate`` for the fallback wiring.
"""

import mlx.core as mx
import mlx.nn as nn

#: The exact ``mlx-audio`` version this patch and the rest of this module were
#: verified against (source read + empirical cache-mutation test, both
#: 2026-09-19). Not a hard requirement — see ``patch_multi_token_cache_continuation``.
VERIFIED_MLX_AUDIO_VERSION = "0.5.1"


def _installed_mlx_audio_version():
    try:
        import importlib.metadata

        return importlib.metadata.version("mlx-audio")
    except Exception:  # noqa: BLE001 - version probing is best-effort, never fatal
        return None


def patch_multi_token_cache_continuation(model):
    """Fix ``base_lm``/``residual_lm``'s causal mask for multi-token cache
    continuation. Safe to call more than once (idempotent) and safe to call on
    a model that turns out not to need it — it only ever changes behaviour for
    calls that would otherwise raise.

    ⚠️ **Loudly warns, but still patches, on an unverified ``mlx-audio``
    version.** This whole module is a hand-written mirror of that package's
    private internals (see module docstring); if a future version restructures
    them, the honest failure mode is "``refcache`` raises and every caller
    falls back to the uncached path" — not silent corruption, because every
    call site here (``VoxCpm2Backend._generate``/``_warm_ref_caches``) already
    wraps this in a try/except. So refusing to patch outright would only
    trade "definitely falls back" for "definitely falls back, plus a scarier
    log line" — not worth doing. What *is* worth doing is making a version
    drift **visible** instead of a developer having to rediscover it from
    scratch: a mismatch prints once, doesn't block startup.
    """
    installed = _installed_mlx_audio_version()
    if installed is not None and installed != VERIFIED_MLX_AUDIO_VERSION:
        import sys

        print(
            f"⚠️ refcache.py 是照着 mlx-audio {VERIFIED_MLX_AUDIO_VERSION} 的内部实现写的，"
            f"当前装的是 {installed}——mask 补丁和 build_ref_cache/generate_with_cache 里对内部结构"
            "的假设可能不再成立。仍会尝试打补丁；真出问题时每次调用会自然退回未缓存路径"
            "（见 _generate 的 try/except），不会静默出错音，但排查起来会更绕，先看这条日志。",
            file=sys.stderr,
        )
    minicpm_cls = type(model.base_lm)
    if getattr(minicpm_cls, "_agentear_refcache_patched", False):
        return
    original_call = minicpm_cls.__call__

    def patched_call(self, inputs_embeds=None, input_ids=None, mask=None, cache=None, is_causal=True):
        if (
            mask is None
            and is_causal
            and cache is not None
            and inputs_embeds is not None
            and inputs_embeds.shape[1] > 1
        ):
            offset = cache[0][0].shape[1]
            seq_len = inputs_embeds.shape[1]
            causal = mx.triu(mx.full((seq_len, seq_len), float("-inf")), k=1)
            if offset > 0:
                causal = mx.concatenate([mx.zeros((seq_len, offset)), causal], axis=1)
            mask = causal[None, None, :, :]
        return original_call(
            self, inputs_embeds=inputs_embeds, input_ids=input_ids, mask=mask, cache=cache, is_causal=is_causal
        )

    minicpm_cls.__call__ = patched_call
    minicpm_cls._agentear_refcache_patched = True


class RefCache:
    """Everything a later ``generate_with_cache`` call needs to skip
    re-encoding one voice's reference audio+text."""

    __slots__ = ("lm_cache", "res_cache", "prefix_feat_cond")

    def __init__(self, lm_cache, res_cache, prefix_feat_cond):
        self.lm_cache = lm_cache
        self.res_cache = res_cache
        self.prefix_feat_cond = prefix_feat_cond


def _scale_emb(model):
    return model.args.lm_config.scale_emb if model.args.lm_config.use_mup else 1.0


def build_ref_cache(model, ref_audio_path):
    """Run the reference-only forward pass once and keep the resulting cache.

    Mirrors the first half of ``VoxCPM2.Model.generate()``'s "Mode 3:
    Reference cloning only" branch, stopping right before any of the caller's
    own text would be appended. Raises on any unexpected model shape — the
    caller must catch and fall back (see module docstring).

    ⚠️ **Takes no ``ref_text`` argument — this is not an oversight.** Mode 3
    never reads it (only Mode 2/4, "continuation", read `prompt_text`/
    `ref_text` into `combined_text`; verified against ``mlx_audio`` 0.5.1's
    ``voxcpm2.py`` ``elif has_ref:`` branch, which only touches
    ``ref_audio``). Cost accounting in ``docs/decisions/0009-...`` was
    corrected 2026-09-19 after a review caught this: the ~2.8 s fixed cost
    comes from encoding the **reference audio's own duration** into a long
    latent sequence and running that through ``base_lm``'s initial forward
    pass — not from any reference *text*. This matches the measured warmup
    times for the two known voices: 男声 (98.7 s reference) took 3.56 s to
    warm, 女声 (10.9 s reference) took 1.05 s — same direction as audio
    duration, nothing to do with either voice's ``ref_text`` length.
    """
    ref_feat = model._encode_wav(ref_audio_path, padding_mode="right")
    ref_tokens, ref_feats, ref_t_mask, ref_a_mask = model._make_ref_prefix(ref_feat)

    ref_tokens = ref_tokens[None, :]
    ref_feats = ref_feats[None, :, :, :]
    ref_t_mask = ref_t_mask[None, :]
    ref_a_mask = ref_a_mask[None, :]

    feat_embed = model.enc_to_lm_proj(model.feat_encoder(ref_feats))
    text_embed = model.base_lm.embed_tokens(ref_tokens) * _scale_emb(model)
    combined = ref_t_mask[:, :, None] * text_embed + ref_a_mask[:, :, None] * feat_embed

    enc_out, lm_cache = model.base_lm(combined)
    enc_out = model.fsq_layer(enc_out) * ref_a_mask[:, :, None] + enc_out * ref_t_mask[:, :, None]
    res_in = model.fusion_concat_proj(
        mx.concatenate([enc_out, ref_a_mask[:, :, None] * feat_embed], axis=-1)
    )
    _res_out, res_cache = model.residual_lm(res_in)
    mx.eval(lm_cache, res_cache)

    # Same value `generate()` itself would have carried forward: the last
    # audio-feature position, which is the ref's zero-padded end marker —
    # identical to what the first *text* position would contribute, since
    # text positions are zero-padded too (see build's `text_pad_feat`).
    prefix_feat_cond = ref_feats[:, -1, :, :]
    return RefCache(lm_cache=lm_cache, res_cache=res_cache, prefix_feat_cond=prefix_feat_cond)


def generate_with_cache(model, ref_cache, text, instruct, max_tokens, inference_timesteps=10, cfg_value=2.0, min_tokens=2):
    """Synthesize ``text`` continuing from ``ref_cache`` instead of
    re-encoding the reference. Returns ``(audio: mx.array, sample_rate)``.

    Mirrors the per-patch autoregressive loop in ``VoxCPM2.Model.generate()``
    exactly (same stop condition, same DiT conditioning) — only the reference
    encoding is skipped, because ``ref_cache`` already carries its result.
    Requires ``patch_multi_token_cache_continuation(model)`` to have been
    called at least once on this model; raises the original
    ``broadcast_shapes`` error otherwise (see module docstring).
    """
    if instruct:
        text = f"({instruct}){text}"
    text_ids = model._tokenize(text)
    text_token = mx.array(text_ids + [model.audio_start_token], dtype=mx.int32)[None, :]
    text_length = text_token.shape[1]
    latent_dim = model.audio_vae.latent_dim
    audio_feat = mx.zeros((1, text_length, model.patch_size, latent_dim))
    text_mask = mx.ones((1, text_length), dtype=mx.float32)
    audio_mask = mx.zeros((1, text_length), dtype=mx.float32)

    feat_embed = model.enc_to_lm_proj(model.feat_encoder(audio_feat))
    text_embed = model.base_lm.embed_tokens(text_token) * _scale_emb(model)
    combined = text_mask[:, :, None] * text_embed + audio_mask[:, :, None] * feat_embed

    enc_out, lm_cache = model.base_lm(combined, cache=ref_cache.lm_cache)
    enc_out = model.fsq_layer(enc_out) * audio_mask[:, :, None] + enc_out * text_mask[:, :, None]
    res_in = model.fusion_concat_proj(
        mx.concatenate([enc_out, audio_mask[:, :, None] * feat_embed], axis=-1)
    )
    res_out, res_cache = model.residual_lm(res_in, cache=ref_cache.res_cache)

    lm_hidden = enc_out[:, -1, :]
    residual_hidden = res_out[:, -1, :]
    prefix_feat_cond = ref_cache.prefix_feat_cond

    pred_feat_seq = []
    for i in range(max_tokens):
        dit_h1 = model.lm_to_dit_proj(lm_hidden)
        dit_h2 = model.res_to_dit_proj(residual_hidden)
        dit_h = mx.concatenate([dit_h1, dit_h2], axis=-1)
        cond_in = prefix_feat_cond.transpose(0, 2, 1)
        pred_feat = model.feat_decoder.sample(
            mu=dit_h, n_timesteps=inference_timesteps, patch_size=model.patch_size,
            cond=cond_in, cfg_value=cfg_value,
        )
        pred_feat = pred_feat.transpose(0, 2, 1)
        pred_feat_seq.append(pred_feat[:, None, :, :])

        curr_embed = model.enc_to_lm_proj(model.feat_encoder(pred_feat[:, None, :, :]))
        stop_logits = model.stop_head(nn.silu(model.stop_proj(lm_hidden)))
        stop_flag = mx.argmax(stop_logits, axis=-1).item()
        if i > min_tokens and stop_flag == 1:
            break

        new_lm_out, lm_cache = model.base_lm(inputs_embeds=curr_embed, cache=lm_cache)
        lm_hidden = model.fsq_layer(new_lm_out[:, -1, :])
        curr_res_in = model.fusion_concat_proj(mx.concatenate([lm_hidden[:, None, :], curr_embed], axis=-1))
        new_res_out, res_cache = model.residual_lm(inputs_embeds=curr_res_in, cache=res_cache)
        residual_hidden = new_res_out[:, -1, :]
        prefix_feat_cond = pred_feat

    if not pred_feat_seq:
        raise RuntimeError("VoxCPM2 cached generation produced no patches")

    all_feats = mx.concatenate(pred_feat_seq, axis=1)
    batch = all_feats.shape[0]
    all_feats_flat = all_feats.reshape(batch, -1, model.feat_dim)
    audio = model.audio_vae.decode(all_feats_flat)
    audio = audio.flatten()
    mx.eval(audio)
    return audio, getattr(model, "sample_rate", None)
