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
import threading
import time
import types
import unittest
import wave
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


def voxcpm2_backend(model=None):
    """Build a VoxCpm2Backend with mlx and the mlx_audio loader stubbed out.

    Both stubs are needed: the backend imports ``mlx.core`` (to own a stream on
    its MLX thread) and ``mlx_audio.tts.utils`` **inside that thread**, so both
    must be importable while ``__init__`` waits for startup.
    """
    fake = model or FakeVoxModel()
    core = types.ModuleType("mlx.core")
    core.set_default_stream = lambda stream: None
    core.new_stream = lambda device: "stream"
    core.default_device = lambda: "device"
    mlx = types.ModuleType("mlx")
    mlx.__path__ = []
    mlx.core = core
    utils = types.ModuleType("mlx_audio.tts.utils")
    utils.load_model = lambda path: fake
    package = types.ModuleType("mlx_audio")
    package.__path__ = []
    tts = types.ModuleType("mlx_audio.tts")
    tts.__path__ = []
    modules = {
        "mlx": mlx,
        "mlx.core": core,
        "mlx_audio": package,
        "mlx_audio.tts": tts,
        "mlx_audio.tts.utils": utils,
    }
    saved = {name: sys.modules.get(name) for name in modules}
    sys.modules.update(modules)
    try:
        return backends.VoxCpm2Backend(model="stub-model"), fake
    finally:
        for name, previous in saved.items():
            if previous is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = previous


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
        """语气和语系都必须真的进 instruct。

        菜单点了没效果、而日志里一切正常——这是最难查的一类（用户只会说
        「选了没用」）。所以直接断言送进模型的 instruct 里两句都在。
        """
        backend, fake = voxcpm2_backend()
        backend.synthesize("hi", "zh", tone="lively", style="yue")
        instruct = fake.calls[0]["kwargs"].get("instruct", "")
        self.assertIn(backends.TONE_INSTRUCTS["lively"], instruct, instruct)
        self.assertIn(backends.STYLE_INSTRUCTS["yue"], instruct, instruct)


class LoudnessTests(unittest.TestCase):
    """响度归一：直接治「声量飘忽」（实测不归一时 RMS 差 4.54 倍）。"""

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
