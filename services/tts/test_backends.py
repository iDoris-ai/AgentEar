"""Backend-selection tests. No mlx-audio, no downloads, no audio playback.

The VoxCPM2 backend is exercised through a stub that mimics only the shape of
``mlx_audio``'s result object. That proves the service wiring (validation,
serialization, WAV packing, busy handling); it proves nothing about the real
model's speech quality, which stays a listening check.
"""

import contextlib
import io
import json
import struct
import sys
import tempfile
import threading
import time
import types
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

import backends
import server


class FakeResult:
    def __init__(self, samples, sample_rate=48000):
        self.audio = samples
        self.sample_rate = sample_rate


class FakeVoxModel:
    """Stand-in for the loaded mlx_audio model.

    The real ``generate`` is a *generator* that yields one or more segments, so
    the fake yields too: a stub returning a bare result object would let the
    production code keep a bug the real model already exposed.
    """

    sample_rate = 48000

    def __init__(self, samples=None, delay=0.0, segments=1):
        self.calls = []
        self.samples = samples if samples is not None else [0.0, 0.5, -0.5]
        self.delay = delay
        self.segments = segments

    def generate(self, text=None, max_tokens=None, **kwargs):
        self.calls.append({"text": text, "max_tokens": max_tokens, "kwargs": kwargs})
        if self.delay:
            time.sleep(self.delay)
        for _ in range(self.segments):
            yield FakeResult(list(self.samples), self.sample_rate)


def voxcpm2_backend(model=None, **kwargs):
    """Build a VoxCpm2Backend with mlx and the mlx_audio loader stubbed out.

    Both stubs are needed: the backend imports ``mlx.core`` (to own a stream on
    its MLX thread) and ``mlx_audio.tts.utils`` **inside that thread**, so both
    must be importable while ``__init__`` waits for startup.

    ``mlx.nn`` is stubbed too (empty module, nothing calls into it unless a
    test explicitly exercises ``refcache``'s real body) — ``refcache.py``
    imports it at module level, and ``_warm_ref_caches``/``_generate`` only
    reach that import when ``voices_dir`` actually has entries, which most
    callers here don't pass.
    """
    fake = model or FakeVoxModel()
    core = types.ModuleType("mlx.core")
    core.set_default_stream = lambda stream: None
    core.new_stream = lambda device: "stream"
    core.default_device = lambda: "device"
    nn_module = types.ModuleType("mlx.nn")
    mlx = types.ModuleType("mlx")
    mlx.__path__ = []
    mlx.core = core
    mlx.nn = nn_module
    utils = types.ModuleType("mlx_audio.tts.utils")
    utils.load_model = lambda path: fake
    package = types.ModuleType("mlx_audio")
    package.__path__ = []
    tts = types.ModuleType("mlx_audio.tts")
    tts.__path__ = []
    modules = {
        "mlx": mlx,
        "mlx.core": core,
        "mlx.nn": nn_module,
        "mlx_audio": package,
        "mlx_audio.tts": tts,
        "mlx_audio.tts.utils": utils,
    }
    saved = {name: sys.modules.get(name) for name in modules}
    sys.modules.update(modules)
    try:
        # `_warm_ref_caches` runs synchronously before `__init__` returns
        # (see backends.py), so any `import refcache` it triggers happens
        # inside this stub window — deliberately **not** undone afterwards:
        # `refcache` then stays cached in `sys.modules` like any other
        # module, which is what lets a test `import refcache` later and
        # `patch.object` the same module object `_generate` will call into.
        return backends.VoxCpm2Backend(model="stub-model", **kwargs), fake
    finally:
        for name, previous in saved.items():
            if previous is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = previous


@contextlib.contextmanager
def voice_library_dir(names=("男声", "女声")):
    """A real directory with one dummy ``.wav`` (+ ``.json``) per name, so
    ``VoiceLibrary`` (and therefore ``_warm_ref_caches``) has entries to work
    with. Content is a placeholder — nothing here decodes real audio."""
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        for name in names:
            (directory / f"{name}.wav").write_bytes(b"RIFF....WAVEfmt ")
            (directory / f"{name}.json").write_text(json.dumps({"ref_text": f"{name} 的参考文本"}))
        yield str(directory)


class WavPackingTests(unittest.TestCase):
    def test_float_samples_become_mono_16bit_pcm(self):
        data = backends.wav_from_float([0.0, 1.0, -1.0], 48000)
        with wave.open(io.BytesIO(data), "rb") as audio:
            self.assertEqual(audio.getnchannels(), 1)
            self.assertEqual(audio.getsampwidth(), 2)
            self.assertEqual(audio.getframerate(), 48000)
            self.assertEqual(audio.getnframes(), 3)
            values = struct.unpack("<3h", audio.readframes(3))
        self.assertEqual(values, (0, 32767, -32767))

    def test_out_of_range_samples_clip_instead_of_wrapping(self):
        data = backends.wav_from_float([2.0, -2.0], 16000)
        with wave.open(io.BytesIO(data), "rb") as audio:
            values = struct.unpack("<2h", audio.readframes(2))
        self.assertEqual(values, (32767, -32767))

    def test_empty_audio_is_an_error_not_a_wav(self):
        with self.assertRaises(backends.TTSError) as caught:
            backends.wav_from_float([], 48000)
        self.assertEqual(caught.exception.status, 500)


class ContractWithRustTests(unittest.TestCase):
    """**跨语言契约**：这两张表的键集必须与 `src/talk.rs` 的
    `STYLE_OPTIONS` / `TONE_OPTIONS` 一致（那边也有一条同名断言）。

    漂移的后果是「菜单点得下去、边车回 400」——这种错只能靠两边各钉一条测试拦住。
    """

    def test_style_keys_match_the_app(self):
        self.assertEqual(
            sorted(backends.STYLE_INSTRUCTS),
            sorted(["zh", "yue", "henan", "sichuan", "shandong", "dongbei", "tianjin",
                    "en", "en-gb", "en-us", "en-ca", "th"]),
        )

    def test_tone_keys_match_the_app(self):
        self.assertEqual(
            sorted(backends.TONE_INSTRUCTS), sorted(["warm", "calm", "lively", "serious"])
        )

    def test_tone_and_style_both_reach_the_instruct(self):
        """语言档：语系和语气都必须真的进 instruct。

        菜单点了没效果、而日志里一切正常——这是最难查的一类（用户只会说
        「选了没用」）。所以直接断言送进模型的 instruct 里两句都在。

        ⚠️ 只对**语言档**成立。方言档走另一条分支，见下一个用例。
        """
        backend, fake = voxcpm2_backend()
        backend.synthesize("hi", "zh", tone="lively", style="zh")
        instruct = fake.calls[0]["kwargs"].get("instruct", "")
        self.assertIn(backends.TONE_INSTRUCTS["lively"], instruct, instruct)
        self.assertIn(backends.STYLE_INSTRUCTS["zh"], instruct, instruct)

    def test_a_dialect_instruct_is_the_bare_name_and_nothing_else(self):
        """方言档的 instruct **只能是方言名本身**（v0.16.0 的实测结论）。

        原来往方言指令里塞「用四川话说，地道四川口音」＋语气描述，正是官方
        cookbook 的 "Keep Instructions Simple" 警告的那类啰嗦描述，实测
        **出来的根本不是四川话**。改成只写方言名之后 jason 验收「效果不错」。

        ⚠️ 这条测试是**故意**钉住「语气描述不进方言档」的：看着像漏了语气，
        实际是踩过坑才砍掉的。要改回去得先拿到人耳验收，别顺手改绿。
        """
        backend, fake = voxcpm2_backend()
        backend.synthesize("hi", "zh", tone="lively", style="yue")
        instruct = fake.calls[0]["kwargs"].get("instruct", "")
        self.assertEqual(instruct.strip(), backends.STYLE_INSTRUCTS["yue"])
        self.assertNotIn(backends.TONE_INSTRUCTS["lively"], instruct, instruct)

    def test_every_dialect_style_takes_the_bare_name_branch(self):
        """`DIALECT_STYLES` 里每一条都得走裸方言名那条分支。"""
        for style in sorted(backends.DIALECT_STYLES):
            with self.subTest(style=style):
                backend, fake = voxcpm2_backend()
                backend.synthesize("hi", "zh", tone="lively", style=style)
                instruct = fake.calls[0]["kwargs"].get("instruct", "")
                self.assertEqual(instruct.strip(), backends.STYLE_INSTRUCTS[style])


def _has_numpy():
    try:
        import numpy  # noqa: F401
    except ImportError:
        return False
    return True


class CacheLimitTests(unittest.TestCase):
    """缓存上限「到底设上了没有」。

    ⚠️ 这组测试的存在理由是一次**报喜不报忧**：v0.17.0 的 `/health` 把
    **我们想要的** 256 当成事实报出去，而 MLX 根本没有 `get_cache_limit()`
    可以读回（实测 0.32.2：两个命名空间都没有）。调用失败时它照样报 256。
    而且失败**真的会发生**：`mx.metal.*` 已废弃，将来被删就静默失效。
    """

    def _mlx_with(self, setter=None, metal_setter=None, with_metal=True):
        core = types.ModuleType("mlx.core")
        if setter is not None:
            core.set_cache_limit = setter
        if with_metal:
            core.metal = types.SimpleNamespace(set_cache_limit=metal_setter)
        # `_mlx_memory()` 要读这三个数：缺了它会走 except 返回 None（那是它
        # 「读数拿不到」的正常行为），于是测试以看不懂的 TypeError 失败，
        # 而不是告诉你「假模块不够真」。
        core.get_active_memory = lambda: 100 * 1024 * 1024
        core.get_cache_memory = lambda: 0
        core.get_peak_memory = lambda: 200 * 1024 * 1024
        return {"mlx": types.ModuleType("mlx"), "mlx.core": core}

    def test_the_current_api_wins_over_the_deprecated_one(self):
        """两个都在时必须走 `mx.set_cache_limit`（`mx.metal.*` 会被删）。"""
        calls = []
        modules = self._mlx_with(
            setter=lambda n: calls.append(("new", n)),
            metal_setter=lambda n: calls.append(("deprecated", n)),
        )
        with patch.dict(sys.modules, modules):
            api, error = backends.install_cache_limit()
        self.assertEqual(api, "mx.set_cache_limit")
        self.assertIsNone(error)
        self.assertEqual(calls, [("new", backends.CACHE_LIMIT_BYTES)])
        self.assertTrue(backends.CACHE_LIMIT_STATE["installed"])

    def test_the_deprecated_api_still_works_for_old_mlx(self):
        """老 mlx 只有 `mx.metal`：还得能设上，但**要能看出是谁设的**。"""
        calls = []
        modules = self._mlx_with(metal_setter=lambda n: calls.append(n))
        with patch.dict(sys.modules, modules):
            api, _ = backends.install_cache_limit()
        self.assertEqual(api, "mx.metal.set_cache_limit")
        self.assertEqual(calls, [backends.CACHE_LIMIT_BYTES])
        self.assertEqual(backends.CACHE_LIMIT_STATE["api"], "mx.metal.set_cache_limit")

    def test_a_failure_is_recorded_and_never_reported_as_success(self):
        """设不上时必须留痕 —— 这是这条测试唯一在守的东西。"""
        def boom(_):
            raise AttributeError("no such method")

        modules = self._mlx_with(setter=boom, metal_setter=boom)
        with patch.dict(sys.modules, modules):
            api, error = backends.install_cache_limit()
        self.assertIsNone(api)
        self.assertIsNotNone(error)
        self.assertFalse(backends.CACHE_LIMIT_STATE["installed"])
        self.assertIn("no such method", backends.CACHE_LIMIT_STATE["error"])

    def test_health_says_whether_the_limit_is_actually_in_place(self):
        """`/health` 里那三个字段必须跟着真实结果走。"""
        backend, _ = voxcpm2_backend()
        with patch.dict(sys.modules, self._mlx_with(setter=lambda n: None)):
            backends.install_cache_limit()
            report = backend._mlx_memory()
        self.assertTrue(report["cache_limit_set"])
        self.assertEqual(report["cache_limit_api"], "mx.set_cache_limit")
        self.assertIsNone(report["cache_limit_error"])

        with patch.dict(sys.modules, self._mlx_with(setter=lambda n: (_ for _ in ()).throw(OSError("boom")))):
            backends.install_cache_limit()
            report = backend._mlx_memory()
        self.assertFalse(report["cache_limit_set"])
        self.assertIsNone(report["cache_limit_api"])
        self.assertIn("boom", report["cache_limit_error"])


@unittest.skipUnless(_has_numpy(), "需要 numpy：没有它 normalize_loudness 是恒等函数，这三条测不到东西")
class LoudnessTests(unittest.TestCase):
    """响度归一：直接治「声量飘忽」（实测不归一时 RMS 差 4.54 倍）。

    ⚠️ **没 numpy 时这三条没有意义**，而且症状会骗人。`normalize_loudness`
    在 `ImportError` 分支里**原样返回**（裸 Python 的 `say` 后端用不到它），
    于是：
      - `test_quiet_and_loud_inputs_come_out_at_the_same_level` 报
        **「RMS 差 10 倍」**——看着像响度归一真坏了；
      - 另外两条**假绿**（恒等输出恰好满足断言）——永远不会失败。
    实测确认（2026-09-15，把 `sys.modules['numpy']` 置 None 复现）：
    `rms(quiet) = 0.01` vs `TARGET_RMS = 0.1`，与当时看到的失败一模一样。
    **所以整类显式跳过并说清原因**，而不是给一个会被误读成产品缺陷的红、
    或一个不会失败的绿。

    ⚠️ 本机三个解释器（`/usr/bin/python3` 3.9.6 / venv 3.11.15 / 3.12.13）
    **都有 numpy**，所以在这里它正常跑；上面那个失败是在**某个没有 numpy 的
    解释器**下出现的，**具体是哪一个没有查实**（`~/.pyenv/versions/3.11.9`
    在该路径下不存在）。不要照抄「非交互 python3 没有 numpy」这种说法——
    这条**没验证**，而且 `/usr/bin/python3` 反例就在眼前。
    """

    def test_quiet_and_loud_inputs_come_out_at_the_same_level(self):
        quiet = backends.normalize_loudness([0.01, -0.01] * 500)
        loud = backends.normalize_loudness([0.5, -0.5] * 500)
        rms = lambda a: (sum(float(x) ** 2 for x in a) / len(a)) ** 0.5  # noqa: E731
        self.assertAlmostEqual(rms(quiet), backends.TARGET_RMS, places=6)
        self.assertAlmostEqual(rms(loud), backends.TARGET_RMS, places=6)
        self.assertAlmostEqual(rms(quiet) / rms(loud), 1.0, places=6)

    def test_peak_never_clips(self):
        # 尖峰 + 低 RMS：只按 RMS 拉满会把峰值推过 1.0 → 削波破音
        samples = [0.0] * 900 + [0.9, -0.9] + [0.0] * 98
        out = backends.normalize_loudness(samples)
        self.assertLessEqual(max(abs(x) for x in out), backends.PEAK_CEILING)

    def test_silence_is_not_amplified(self):
        silence = [0.0] * 100
        self.assertEqual(list(backends.normalize_loudness(silence)), silence)


class VoxCpm2BackendTests(unittest.TestCase):
    def test_synthesize_returns_a_valid_wav_and_passes_text_through(self):
        backend, fake = voxcpm2_backend()
        data = backend.synthesize("今天天气怎么样？", "zh")
        self.assertEqual(fake.calls[0]["text"], "今天天气怎么样？")
        backends.validate_wav_bytes(data)

    def test_unknown_language_is_rejected_before_the_model_runs(self):
        backend, fake = voxcpm2_backend()
        for lang in ["jp", "EN", "", None]:
            with self.subTest(lang=lang):
                with self.assertRaises(backends.TTSError) as caught:
                    backend.synthesize("hello", lang)
                self.assertEqual(caught.exception.status, 400)
        self.assertEqual(fake.calls, [])

    def test_concurrent_requests_do_not_enter_the_model_twice(self):
        backend, fake = voxcpm2_backend(FakeVoxModel(delay=0.2))
        results = []

        def worker():
            try:
                results.append(backend.synthesize("hi", "en"))
            except backends.TTSError as error:
                results.append(error.status)

        threads = [threading.Thread(target=worker) for _ in range(3)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(len(fake.calls), 1, "同一时刻只能有一次合成")
        self.assertEqual(results.count(503), 2, "抢不到的请求要明确 503，不是排队或串行合流")

    def test_model_failure_is_reported_as_500_not_a_crash(self):
        class Boom(FakeVoxModel):
            def generate(self, **kwargs):
                raise RuntimeError("metal out of memory")

        backend, _ = voxcpm2_backend(Boom())
        with self.assertRaises(backends.TTSError) as caught:
            backend.synthesize("hi", "en")
        self.assertEqual(caught.exception.status, 500)
        self.assertIn("metal out of memory", str(caught.exception))
        # 失败后锁要放开，否则一次失败会让服务永久 503
        self.assertFalse(backend._lock.locked())

    def test_multi_segment_output_is_joined_in_order(self):
        backend, fake = voxcpm2_backend(FakeVoxModel([0.25, -0.25], segments=3))
        data = backend.synthesize("a longer sentence", "en")
        with wave.open(io.BytesIO(data), "rb") as audio:
            values = struct.unpack("<6h", audio.readframes(6))
        # **不断言绝对值**：v0.8.0 起输出会过一遍响度归一（见 normalize_loudness），
        # 所以幅度由归一目标决定，不由输入决定。这里钉的是「三段按顺序拼接」：
        # 6 个采样、正负交替、每段幅度一致。
        self.assertEqual(len(values), 6, "三段都要在，不能只取第一段")
        self.assertEqual(
            [1 if v > 0 else -1 for v in values], [1, -1] * 3, "顺序不能乱、符号不能翻"
        )
        self.assertEqual(len({abs(v) for v in values}), 1, "每段幅度应一致（同一归一增益）")
        self.assertGreater(values[0], 0)

    def test_a_generator_that_yields_nothing_is_a_500(self):
        backend, _ = voxcpm2_backend(FakeVoxModel(segments=0))
        with self.assertRaises(backends.TTSError) as caught:
            backend.synthesize("hi", "en")
        self.assertEqual(caught.exception.status, 500)

    def test_describe_reports_the_model_so_it_can_be_swapped(self):
        backend, _ = voxcpm2_backend()
        described = backend.describe()
        self.assertEqual(described["backend"], "voxcpm2")
        self.assertEqual(described["model"], "stub-model")


class RefCacheWiringTests(unittest.TestCase):
    """T3.4.13 Phase 2: does `_generate` actually reach `refcache`, and does
    it fail safe when the cache path breaks? The cache's own correctness
    (does cached synthesis sound right) is validated separately in
    `spike/t3413-tts-refcache/` against the real model — a stub model can't
    prove that, only that the wiring calls the right thing with the right
    arguments and falls back when it should.
    """

    def test_warming_with_a_stub_model_fails_safe_and_leaves_no_cache(self):
        # FakeVoxModel has no `base_lm`/`residual_lm` — exactly the shape
        # mismatch `_warm_ref_caches` must survive without crashing startup.
        with voice_library_dir() as voices_dir:
            backend, _ = voxcpm2_backend(voices_dir=voices_dir)
        self.assertEqual(set(backend.voices.entries), {"男声", "女声"}, "音色库本身要读到两个条目")
        self.assertEqual(backend._ref_cache, {}, "stub 模型建不出缓存，不该假装建成了")

    def test_generate_uses_the_cache_when_one_is_available(self):
        with voice_library_dir() as voices_dir:
            backend, fake = voxcpm2_backend(voices_dir=voices_dir)
            # 到这里 `voxcpm2_backend` 内部的预热已经至少尝试过 `import refcache`
            # 一次（不管预热本身成不成功），`sys.modules["refcache"]` 已经有了——
            # 这里才 import 才能拿到同一个模块对象，`patch.object` 才打得准。
            import refcache
            # 自然预热在 stub 模型上必然失败（见上一条测试），这里手工注入一个
            # 哨兵，只为了让 `_generate` 走到"有缓存"分支——`generate_with_cache`
            # 本身整个被替身掉，所以这个哨兵长什么样不重要。
            backend._ref_cache["男声"] = object()
            fake_audio = [0.1, -0.1, 0.2]
            with patch.object(refcache, "generate_with_cache", return_value=(fake_audio, 48000)) as mock_generate:
                data = backend.synthesize("你好世界", "zh", voice="男声")
            backends.validate_wav_bytes(data)
            mock_generate.assert_called_once()
            call = mock_generate.call_args
            self.assertIs(call.args[0], backend._model, "要传真正在跑的那个 model 实例")
            self.assertIs(call.args[1], backend._ref_cache["男声"], "要传我们注入的那份缓存")
            self.assertEqual(call.args[2], "你好世界", "文本不能被缓存路径动过")
            self.assertEqual(call.args[4], backend.MAX_TOKENS)
            self.assertEqual(fake.calls, [], "走了缓存路径就不该再调没缓存的 model.generate()")

    def test_generate_falls_back_when_cache_continuation_raises(self):
        with voice_library_dir() as voices_dir:
            backend, fake = voxcpm2_backend(voices_dir=voices_dir)
            import refcache
            backend._ref_cache["男声"] = object()
            with patch.object(refcache, "generate_with_cache", side_effect=RuntimeError("broadcast_shapes boom")):
                # 退回老路必须有输出（T3.4.8 的教训同一条纪律）——不是 500，
                # 是老老实实调一遍没有缓存的 model.generate()。
                data = backend.synthesize("你好世界", "zh", voice="男声")
            backends.validate_wav_bytes(data)
            self.assertEqual(len(fake.calls), 1, "缓存续接失败要退回未缓存路径，不能就此没有声音")
            self.assertEqual(fake.calls[0]["text"], "你好世界")
            self.assertNotIn(
                "男声", backend._ref_cache,
                "失败要把这个音色的缓存直接扔掉，不能让下一次请求还去踩同一个坑"
                "（先白跑一遍缓存路径、再退回慢路径，比一开始就退回慢路径更差）",
            )

    def test_a_second_failure_after_eviction_does_not_touch_refcache_again(self):
        # 上一条测试证明失败会把缓存扔掉；这一条证明扔掉之后就真的不会再碰
        # `refcache.generate_with_cache` 了——不是"扔了但还是会再试一次"。
        with voice_library_dir() as voices_dir:
            backend, fake = voxcpm2_backend(voices_dir=voices_dir)
            import refcache

            backend._ref_cache["男声"] = object()
            with patch.object(refcache, "generate_with_cache", side_effect=RuntimeError("boom")) as mock_generate:
                backend.synthesize("第一次", "zh", voice="男声")
                backend.synthesize("第二次", "zh", voice="男声")
            self.assertEqual(mock_generate.call_count, 1, "第一次失败就该被永久禁用，第二次请求不该再摸一次缓存路径")
            self.assertEqual(len(fake.calls), 2, "两次请求都该落到未缓存路径")

    def test_cache_object_identity_is_reused_across_successful_calls(self):
        # 证明"缓存只建一次、之后反复复用"，不是每次请求偷偷重建或替换。
        with voice_library_dir() as voices_dir:
            backend, fake = voxcpm2_backend(voices_dir=voices_dir)
            import refcache

            sentinel = object()
            backend._ref_cache["男声"] = sentinel
            with patch.object(refcache, "generate_with_cache", return_value=([0.0], 48000)) as mock_generate:
                backend.synthesize("第一句", "zh", voice="男声")
                backend.synthesize("第二句", "zh", voice="男声")
            self.assertEqual(mock_generate.call_count, 2)
            self.assertIs(mock_generate.call_args_list[0].args[1], sentinel)
            self.assertIs(mock_generate.call_args_list[1].args[1], sentinel, "第二次调用应该复用同一份缓存对象，不是重建的")
            self.assertEqual(fake.calls, [], "两次都该走缓存路径，一次都不该落到未缓存的 model.generate()")

    def test_unknown_voice_has_no_cache_entry_to_use(self):
        # `entry` 为 None（音色库为空）时，缓存分支必须整个跳过而不是报错。
        backend, fake = voxcpm2_backend()
        data = backend.synthesize("你好", "zh")
        backends.validate_wav_bytes(data)
        self.assertEqual(len(fake.calls), 1)


class BackendSelectionTests(unittest.TestCase):
    def test_say_is_still_selectable_and_reports_its_voices(self):
        engine = backends.build_backend("say")
        self.assertEqual(engine.name, "say")
        self.assertEqual(engine.available_langs(), backends.VOICES)
        self.assertEqual(engine.describe()["backend"], "say")

    def test_unknown_backend_is_rejected(self):
        with self.assertRaises(backends.TTSError):
            backends.build_backend("whisper")

    def test_cli_default_is_voxcpm2_and_say_stays_available(self):
        self.assertEqual(server.parse_args([]).backend, "voxcpm2")
        self.assertEqual(server.parse_args([]).model, None)
        self.assertEqual(server.parse_args(["--backend", "say"]).backend, "say")
        self.assertEqual(
            server.parse_args(["--model", "org/other-voxcpm"]).model, "org/other-voxcpm"
        )

    def test_cli_rejects_bad_numbers(self):
        for argv in [["--port", "-1"], ["--port", "70000"], ["--timeout", "0"],
                     ["--timeout", "nan"], ["--queue-wait", "-1"]]:
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    server.parse_args(argv)

    def test_validate_request_only_accepts_the_three_languages(self):
        self.assertEqual(
            server.validate_request({"text": "hi", "lang": "th"}), ("hi", "th", None, None, None)
        )
        # voice / style 是可选的逐请求覆盖（菜单和语音指令都走它）
        self.assertEqual(
            server.validate_request(
                {"text": "hi", "lang": "th", "voice": "f1", "style": "yue", "tone": "warm"}
            ),
            ("hi", "th", "f1", "yue", "warm"),
        )
        for bad in ({"text": "hi", "lang": "th", "voice": ""},
                    {"text": "hi", "lang": "th", "style": 7},
                    {"text": "hi", "lang": "th", "tone": ""}):
            with self.subTest(bad=bad):
                with self.assertRaises(backends.TTSError):
                    server.validate_request(bad)
        for payload in [{}, {"text": "hi"}, {"text": "hi", "lang": "jp"},
                        {"text": "", "lang": "en"}, {"text": 3, "lang": "en"}]:
            with self.subTest(payload=payload):
                with self.assertRaises(backends.TTSError) as caught:
                    server.validate_request(payload)
                self.assertEqual(caught.exception.status, 400)


class VoxCpm2HTTPTests(unittest.TestCase):
    """The HTTP shell must not care which backend is behind it."""

    def setUp(self):
        self.backend, self.fake = voxcpm2_backend()
        self.httpd = server.TTSServer(("127.0.0.1", 0), self.backend)
        self.thread = threading.Thread(
            target=self.httpd.serve_forever, kwargs={"poll_interval": 0.01}
        )
        self.thread.start()
        self.addCleanup(self.thread.join, 2)
        self.addCleanup(self.httpd.server_close)
        self.addCleanup(self.httpd.shutdown)

    def request(self, method, path, payload=None):
        import http.client

        body = json.dumps(payload).encode() if payload is not None else None
        connection = http.client.HTTPConnection("127.0.0.1", self.httpd.server_port, timeout=10)
        try:
            connection.request(method, path, body=body, headers={"Content-Type": "application/json"})
            response = connection.getresponse()
            return response.status, response.getheader("Content-Type"), response.read()
        finally:
            connection.close()

    def test_health_names_the_backend_and_model(self):
        status, content_type, body = self.request("GET", "/health")
        self.assertEqual((status, content_type), (200, "application/json"))
        payload = json.loads(body)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["backend"], "voxcpm2")
        self.assertEqual(payload["model"], "stub-model")

    def test_speak_returns_wav_for_all_three_languages(self):
        for lang in ["zh", "en", "th"]:
            with self.subTest(lang=lang):
                status, content_type, body = self.request(
                    "POST", "/speak", {"text": "hello", "lang": lang}
                )
                self.assertEqual((status, content_type), (200, "audio/wav"))
                backends.validate_wav_bytes(body)

    def test_unsupported_language_is_400_json(self):
        status, content_type, body = self.request("POST", "/speak", {"text": "hi", "lang": "jp"})
        self.assertEqual((status, content_type), (400, "application/json"))
        self.assertIn("zh, en, th", json.loads(body)["error"])
        self.assertEqual(self.fake.calls, [])


if __name__ == "__main__":
    unittest.main()
