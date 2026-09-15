"""TTS backends for the local sidecar. Standard library only, except VoxCPM2.

Two backends, selectable with ``--backend``:

``say``
    macOS ``say`` + ``afconvert``. Zero downloads, zero Python packages,
    three languages (Tingting / Samantha / Kanya). This is the zero-dependency
    fallback required by ADR-0007 §4.2.

``voxcpm2`` (default)
    ``mlx-community/VoxCPM2-4bit`` through ``mlx-audio``. Apache-2.0 weights,
    48 kHz output, and the only local candidate measured to speak Thai
    (``docs/benchmarks-m3.md`` §6). Requires a Python 3.11+ virtualenv with
    ``mlx-audio`` installed — see ``scripts/setup-talk.sh``.

Both satisfy the same contract: ``synthesize(text, lang) -> bytes`` returning a
complete 16-bit PCM WAV. The sample rate is the backend's own (22050 for ``say``,
48000 for VoxCPM2) — the contract never pinned a rate, and callers must read it
from the WAV header.
"""

import io
import json
import queue
import struct
import subprocess
import tempfile
import threading
import time
import wave
from concurrent.futures import Future
from concurrent.futures import TimeoutError as FuturesTimeout
from pathlib import Path

SUPPORTED_LANGS = ("zh", "en", "th")

# macOS voices for the `say` backend. Not an inventory of installed voices:
# these three names must exist on the machine or `say` fails loudly.
VOICES = {"zh": "Tingting", "en": "Samantha", "th": "Kanya"}

MAX_WAV_BYTES = 16 * 1024 * 1024

#: The 4-bit MLX build of VoxCPM2. A local directory path is accepted too, which
#: is what makes the model replaceable without touching this file.
DEFAULT_VOXCPM2_MODEL = "mlx-community/VoxCPM2-4bit"


class TTSError(Exception):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


#: 归一化目标：约 -20 dBFS 的 RMS。人耳对「一句话比另一句响 4 倍」极其敏感，
#: 而 VoxCPM2 不同次生成的 RMS 实测能差 **4.54 倍**（`vendor/models/talk/measure_f0.py`）。
#: -20 dBFS ≈ 0.1 的 RMS：够响、又给峰值留了 6dB 以上余量。
TARGET_RMS = 0.1
#: 归一化后的峰值上限。**必须留**：只按 RMS 拉满会把峰值推过 1.0 削波成破音，
#: 而破音比「忽大忽小」更难听。
PEAK_CEILING = 0.97


def normalize_loudness(samples):
    """把一段音频的**响度**拉到统一水平，峰值不越界。

    这是「声量飘忽」的直接解法：VoxCPM2 每次生成的整体音量本来就随机，
    实测 RMS 差 4.54 倍。钉参考音频能把它压到 1.04 倍，但那是**副作用**不是保证——
    换个音色、换个语言就不一定了，所以这里再兜一层确定性的归一。

    ⚠️ **按整句归一，不按帧**（不做压缩器）：帧级压缩会改变韵律，
    而这段音频是要拿来做对话回答的，韵律比「音量绝对平」更重要。
    """
    try:
        import numpy as np
    except ImportError:
        return samples  # 裸 Python 环境（say 后端）用不到这个
    array = np.asarray(samples, dtype=np.float32).reshape(-1)
    if array.size == 0:
        return samples
    rms = float(np.sqrt(np.mean(array.astype(np.float64) ** 2)))
    if rms <= 1e-6:
        return samples  # 全静音，别去放大噪声
    gain = TARGET_RMS / rms
    peak = float(np.max(np.abs(array)))
    if peak * gain > PEAK_CEILING:
        gain = PEAK_CEILING / max(peak, 1e-9)
    return array * gain


def wav_from_float(samples, sample_rate):
    """Pack float samples in [-1, 1] into a mono 16-bit PCM WAV byte string.

    Peaks above 1.0 are clipped rather than wrapped: wrapping turns a loud
    sentence into noise, clipping only costs a little distortion.

    numpy is used when present — it is what converts an ``mx.array`` without a
    round trip through Python floats. The struct fallback keeps this module
    usable on a bare system Python, which is how the ``say`` backend runs.
    """
    try:
        import numpy as np
    except ImportError:
        values = samples.tolist() if hasattr(samples, "tolist") else list(samples)
        if not values:
            raise TTSError(500, "The TTS model produced no audio samples.")
        scaled = []
        for value in values:
            value = float(value)
            value = 1.0 if value > 1.0 else (-1.0 if value < -1.0 else value)
            scaled.append(int(value * 32767.0))
        pcm_bytes = struct.pack(f"<{len(scaled)}h", *scaled)
    else:
        array = np.asarray(samples, dtype=np.float32).reshape(-1)
        if array.size == 0:
            raise TTSError(500, "The TTS model produced no audio samples.")
        clipped = np.clip(array, -1.0, 1.0)
        pcm_bytes = (clipped * 32767.0).astype("<i2").tobytes()
    output = io.BytesIO()
    with wave.open(output, "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(int(sample_rate))
        audio.writeframes(pcm_bytes)
    return output.getvalue()


def join_audio(chunks):
    """Concatenate the waveform segments of one synthesis, in order."""
    if len(chunks) == 1:
        return chunks[0]
    try:
        import numpy as np
    except ImportError:
        joined = []
        for chunk in chunks:
            joined.extend(chunk.tolist() if hasattr(chunk, "tolist") else list(chunk))
        return joined
    return np.concatenate([np.asarray(chunk, dtype=np.float32).reshape(-1) for chunk in chunks])


class SayBackend:
    """macOS `say` + `afconvert`. The zero-dependency fallback."""
    name = "say"

    def __init__(self, timeout=30, voices=None):
        self.timeout = timeout
        self.voices_map = dict(voices or VOICES)
        self._slots = threading.BoundedSemaphore(5)
        self._lock = threading.Lock()
        self._children = set()
        self._stopping = False

    # -- interface ------------------------------------------------------
    def describe(self):
        return {"backend": self.name, "voices": dict(self.voices_map)}

    def available_langs(self):
        return {lang: voice for lang, voice in self.voices_map.items() if lang in SUPPORTED_LANGS}

    def synthesize(self, text, lang, voice=None, style=None, tone=None):
        """`voice` / `style` 对后端无意义（`say` 的音色由系统发音人决定）。

        **接住而不是拒绝**：HTTP 契约是共享的，调用方不该为了换后端而改请求体。
        但**不能静默**——用户点了「换音色」却发现没变化时，
        日志里得能看出「这个后端不支持」。
        """
        if voice or style or tone:
            log_once_unsupported_voice_style(voice, style, tone)
        voice = self.voices_map.get(lang)
        if voice is None:
            raise TTSError(400, "lang must be exactly one of: zh, en, th.")
        if not self._slots.acquire(blocking=False):
            raise TTSError(503, "TTS is busy with five requests; retry later.")
        try:
            deadline = time.monotonic() + self.timeout
            with tempfile.TemporaryDirectory(prefix="agentear-tts-") as directory:
                aiff = Path(directory) / "speech.aiff"
                wav = Path(directory) / "speech.wav"
                # stdin keeps text out of command-line arguments and treats '-' as text.
                self._run(
                    ["/usr/bin/say", "-v", voice, "-o", str(aiff), "-f", "-"],
                    deadline,
                    text.encode("utf-8"),
                )
                self._run(
                    ["/usr/bin/afconvert", str(aiff), str(wav), "-d", "LEI16", "-f", "WAVE"],
                    deadline,
                )
                return read_wav(wav)
        except OSError:
            raise TTSError(500, "Unable to run macOS audio tools or read their output.") from None
        finally:
            self._slots.release()

    # -- internals ------------------------------------------------------
    def _run(self, command, deadline, input_bytes=None):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TTSError(504, "Speech synthesis timed out.")
        # Register under the same lock used by close(), so shutdown misses no child.
        with self._lock:
            if self._stopping:
                raise TTSError(503, "TTS service is stopping.")
            child = subprocess.Popen(
                command,
                stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self._children.add(child)
        try:
            try:
                child.communicate(input=input_bytes, timeout=remaining)
            except subprocess.TimeoutExpired:
                child.kill()
                child.communicate()
                raise TTSError(504, "Speech synthesis timed out.") from None
            if self._stopping:
                raise TTSError(503, "TTS service is stopping.")
            if child.returncode != 0:
                tool = Path(command[0]).name
                raise TTSError(
                    500,
                    f"macOS {tool} failed (exit {child.returncode}); "
                    "check that the requested voice and audio tools are available.",
                )
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()
            with self._lock:
                self._children.discard(child)

    def close(self):
        with self._lock:
            self._stopping = True
            for child in self._children:
                try:
                    child.kill()
                except ProcessLookupError:
                    pass
        # Each request's communicate() reaps its child before the server joins workers.


#: 语系/风格 → VoxCPM2 的 instruct 文案。
#:
#: ⚠️ **这是「创意描述」不是开关**：VoxCPM2 吃自然语言，效果没有硬保证。
#: `benchmarks-m3.md` §6.2.2 已经记过——方言到底生没生效，**当前没有客观判据**
#: （`language-id` 只到语言级，区分不了中文内部方言）。所以这里的每一条
#: 都只是「让模型朝那个方向走」，**必须在文档里写明未经人耳验收**。
#: 这几种是**方言**：文档要求控制指令只写方言名，且正文必须是方言本身。
DIALECT_STYLES = frozenset({"yue", "henan", "sichuan", "shandong", "dongbei", "tianjin"})

STYLE_INSTRUCTS = {
    # ⚠️ **控制指令只写名字，不要写描述。**
    #
    # 官方 cookbook 的 "Keep Instructions Simple" 明说：
    #   In the Control Instruction, **simply type the dialect name**（例如 `Cantonese`）。
    #   **Adding too many complex voice instructions might spoil the broth.**
    #
    # 我们原来写的是「用四川话说，地道四川口音」——正是它警告的那类啰嗦描述。
    # 实测（jason 2026-09-15）：那么写出来的**根本不是四川话**；
    # 换成 `(四川话)` 之后才出四川腔。
    #
    # ⚠️ **但方言的决定性因素是正文**：usage guide 的 Dialect tips 写着
    #   「write the target text in that dialect's own vocabulary and expressions,
    #    not in standard Mandarin」——同一个 `(四川话)` 下，
    #   正文是地道四川话就出四川腔，正文是普通话就出普通话。
    #   所以这一栏只是**开关**，不能把普通话变成方言。
    "zh": "普通话",
    "yue": "广东话",
    "henan": "河南话",
    "sichuan": "四川话",
    "shandong": "山东话",
    "dongbei": "东北话",
    "tianjin": "天津话",
    "en": "English",
    "en-gb": "British English",
    "en-us": "American English",
    "en-ca": "Canadian English",
    "th": "Thai",
}


#: 语气/情绪 → instruct。**默认 `warm`，这是实测挑出来的。**
#:
#: jason 2026-09-14 听完第一版说「像个机器人，一点感情都没有」。实测三个旋钮：
#:
#: | 配置 | 耗时 | F0 起伏 | 能量起伏 |
#: |---|---|---|---|
#: | 只写语种 | 3.90s | 3.53 半音 | 0.0736 |
#: | **加情绪描述** | 2.73s | **5.57** | **0.1196** |
#: | 情绪 + steps=5 | 1.46s | 4.10 | 0.0630 |
#: | 情绪 + steps=20 | 5.54s | 4.50 | 0.1012 |
#:
#: 结论：**加情绪描述是免费且显著的**（起伏 +58%/+62%），
#: 而 `inference_timesteps` 降到 5 只快 1.9 倍、**明显变平** → 不降档。
TONE_INSTRUCTS = {
    "warm": "亲切自然，像跟朋友聊天，语气有起伏，带一点微笑",
    "calm": "平静温和，语速平稳",
    "lively": "活泼明快，语调起伏明显",
    "serious": "沉稳专业，播报口吻",
}


class VoiceLibrary:
    """音色库：一个目录里成对的 `<name>.wav` + `<name>.json`。

    `.json` 里存 `ref_text`（**必须**：克隆模式要参考音频对应的文本，
    文本不对会带偏）以及造它时的客观量（F0/RMS），便于事后判断哪条更稳。

    没有参考音频时**退化为不钉音色**（就是用户抱怨的那种飘忽），
    所以 `default` 为空时要明确告警，不要静默。
    """

    def __init__(self, directory, default=None):
        self.dir = Path(directory) if directory else None
        self.entries = {}
        self.load_error = None
        if self.dir and self.dir.is_dir():
            for wav in sorted(self.dir.glob("*.wav")):
                meta_path = wav.with_suffix(".json")
                meta = {}
                if meta_path.exists():
                    try:
                        meta = json.loads(meta_path.read_text())
                    except (OSError, ValueError) as error:
                        meta["ref_text_error"] = str(error)
                self.entries[wav.stem] = {
                    "wav": wav,
                    "ref_text": meta.get("ref_text"),
                    "f0_median": meta.get("f0_median"),
                    "rms": meta.get("rms"),
                }
        self.default = default or (next(iter(self.entries), None))

    def describe(self):
        return {
            name: {
                "ref_audio": str(e["wav"]),
                "ref_text": e["ref_text"],
                "f0_median": e["f0_median"],
                "rms": e["rms"],
            }
            for name, e in self.entries.items()
        }

    def get(self, name):
        if not self.entries:
            return None
        key = name if name in self.entries else self.default
        return self.entries.get(key)


_WARNED_NO_VOICE = threading.Event()
_WARNED_UNSUPPORTED = threading.Event()


def log_once_unsupported_voice_style(voice, style, tone=None):
    """`say` 后端不支持选音色/语系——喊一次，别让用户以为点了没生效。"""
    if not _WARNED_UNSUPPORTED.is_set():
        _WARNED_UNSUPPORTED.set()
        import sys

        print(
            f"⚠️ say 后端不支持 voice/style/tone（收到 voice={voice!r} style={style!r} tone={tone!r}）——"
            "那两个参数只有 voxcpm2 后端有效。",
            file=sys.stderr,
        )


_WARNED_NO_VOICE = threading.Event()


def log_once_no_voice():
    """没有参考音频时喊一次。**只喊一次**：这是每次合成都成立的事实，
    刷屏只会把真正重要的日志挤掉。"""
    if not _WARNED_NO_VOICE.is_set():
        _WARNED_NO_VOICE.set()
        import sys

        print(
            "⚠️ 音色库里没有参考音频 → 每次生成会随机换一个说话人"
            "（实测 F0 极差 65%、音量差 4.5 倍）。"
            "跑 services/tts/make_voice.py 造一个，或用 --voices-dir 指定目录。",
            file=sys.stderr,
        )


class VoxCpm2Backend:
    """``mlx-community/VoxCPM2-4bit`` held resident in this process.

    VoxCPM2 is tokenizer-free: it infers the language from the text, and there
    is no per-language voice to select. ``lang`` is still validated and still
    required — the service refuses to guess, because guessing wrong means the
    caller hears a language they cannot understand and cannot tell why.

    ## One dedicated MLX thread, and why it is not optional

    **MLX streams are thread-local.** Loading the weights on one thread and
    synthesizing on an HTTP worker thread does not raise a Python exception —
    it aborts the whole process:

        libc++abi: terminating due to uncaught exception of type std::runtime_error:
        There is no Stream(cpu, 1) in current thread.

    Measured 2026-09-14: the abort happens on the first request, after the
    model has generated, at the point where the lazy ``mx.array`` is finally
    evaluated (``np.asarray``). Touching only ``.shape``/``.size`` does **not**
    evaluate, which is exactly how a probe can "pass" and still leave this bug
    in place — the earlier probe did.

    So everything MLX touches — load, generate, joining segments, converting to
    numpy, packing the WAV — runs on one thread owned by this object, and HTTP
    workers hand work to it through a queue. That also happens to serialize
    synthesis, which is what we want anyway.

    One synthesis at a time. The previous five-way concurrency existed only
    because ``say`` spawns a process per request. ``MAX_TOKENS`` keeps a runaway
    generation bounded; the measured cost is ~2.4-4.2 s per short sentence on an
    M1 Max.
    """

    name = "voxcpm2"

    #: Guard rail: VoxCPM2 generates ~24 tokens per second of audio.
    MAX_TOKENS = 2000

    def __init__(self, model=DEFAULT_VOXCPM2_MODEL, timeout=60, queue_wait=0.0,
                 load_timeout=600, voices_dir=None, default_voice=None,
                 default_style=None, default_tone=None):
        self.model_id = str(model)
        # 音色库：**默认必须有一个**，否则就是用户抱怨的「每次换一个人」
        self.voices = VoiceLibrary(voices_dir, default_voice)
        self.default_style = default_style or "zh"
        self.default_tone = default_tone or "warm"
        self._ref_cache = {}
        self.timeout = timeout
        self.queue_wait = queue_wait
        self.sample_rate = 48000
        self.load_seconds = 0.0
        self._lock = threading.Lock()
        self._stopping = False
        self._jobs = queue.Queue()
        self._startup = Future()
        self._thread = threading.Thread(
            target=self._mlx_thread, name="voxcpm2-mlx", daemon=True
        )
        self._thread.start()
        try:
            self._startup.result(timeout=load_timeout)
        except FuturesTimeout:
            raise TTSError(500, "VoxCPM2 model load timed out.") from None
        except TTSError:
            raise
        except Exception as error:  # noqa: BLE001 - startup failure is actionable
            raise TTSError(500, f"VoxCPM2 model failed to load: {error}") from None

    # -- interface ------------------------------------------------------
    def describe(self):
        return {
            "backend": self.name,
            "model": self.model_id,
            "sample_rate": self.sample_rate,
            "load_seconds": round(self.load_seconds, 3),
            # **峰值 RSS 自报**：仓库规矩是「高资源档的峰值 RSS 必须实测入库」，
            # 而在 macOS 沙箱里 `ps`/`top` 都读不到别人的内存——
            # 让进程自己报才是最可靠的口径（ru_maxrss 在 macOS 上是字节）。
            "peak_rss_mb": self.peak_rss_mb(),
            "default_voice": self.voices.default,
            "default_style": self.default_style,
            "default_tone": self.default_tone,
            "tones": sorted(TONE_INSTRUCTS),
            "voices": sorted(self.voices.entries),
            "styles": sorted(STYLE_INSTRUCTS),
            "loudness": {"target_rms": TARGET_RMS, "peak_ceiling": PEAK_CEILING},
        }

    def peak_rss_mb(self):
        """本进程峰值 RSS（MB）。macOS 上 `ru_maxrss` 单位是**字节**。"""
        import resource

        return round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / (1024 * 1024))

    def available_langs(self):
        # Language is inferred from the text; no separate voice per language.
        return {lang: self.model_id for lang in SUPPORTED_LANGS}

    def synthesize(self, text, lang, voice=None, style=None, tone=None):
        if lang not in SUPPORTED_LANGS:
            raise TTSError(400, "lang must be exactly one of: zh, en, th.")
        entry = self.voices.get(voice)
        if self.voices.entries and entry is None:
            raise TTSError(400, f"unknown voice {voice!r}; known: {sorted(self.voices.entries)}")
        if voice and voice not in self.voices.entries:
            raise TTSError(400, f"unknown voice {voice!r}; known: {sorted(self.voices.entries)}")
        style = style or self.default_style
        if style not in STYLE_INSTRUCTS:
            raise TTSError(400, f"unknown style {style!r}; known: {sorted(STYLE_INSTRUCTS)}")
        tone = tone or self.default_tone
        if tone not in TONE_INSTRUCTS:
            raise TTSError(400, f"unknown tone {tone!r}; known: {sorted(TONE_INSTRUCTS)}")
        # The lock is what turns "second request arrives mid-synthesis" into an
        # explicit 503 instead of an unbounded queue.
        if not self._lock.acquire(timeout=self.queue_wait):
            raise TTSError(503, "TTS is busy synthesizing; retry later.")
        try:
            if self._stopping:
                raise TTSError(503, "TTS service is stopping.")
            job = Future()
            self._jobs.put((text, style, tone, entry, job))
            try:
                return job.result(timeout=self.timeout)
            except FuturesTimeout:
                # The MLX thread cannot be interrupted mid-generation, so the
                # work keeps running; the caller is told rather than left hanging.
                raise TTSError(504, "Speech synthesis timed out.") from None
        finally:
            self._lock.release()

    def close(self):
        self._stopping = True
        self._jobs.put(None)

    # -- the MLX thread -------------------------------------------------
    def _mlx_thread(self):
        try:
            import mlx.core as mx

            # Own a stream on this thread before anything else touches MLX.
            mx.set_default_stream(mx.new_stream(mx.default_device()))
            from mlx_audio.tts.utils import load_model

            started = time.monotonic()
            self._model = load_model(self.model_id)
            self.load_seconds = time.monotonic() - started
            self.sample_rate = int(getattr(self._model, "sample_rate", 48000))
            self._startup.set_result(True)
        except BaseException as error:  # noqa: BLE001 - surfaced to __init__
            self._startup.set_exception(error)
            return
        while True:
            job = self._jobs.get()
            if job is None:
                return
            text, style, tone, entry, future = job
            try:
                audio, sample_rate = self._generate(text, style, tone, entry)
                future.set_result(
                    wav_from_float(normalize_loudness(audio), sample_rate or self.sample_rate)
                )
            except BaseException as error:  # noqa: BLE001 - surfaced to the caller
                future.set_exception(error)

    def _load_ref(self, entry):
        """把参考音频交给模型——**给路径，不给数组**。

        ⚠️ **这里踩过一个很隐蔽的坑（2026-09-15 定位）**：原来用
        `mlx_audio.utils.load_audio(path)` 预解码成数组缓存起来，看着是优化，
        实际是错的 ——

        - `load_audio(path)` 的默认目标是 **24 kHz**，它会把 48 kHz 的参考
          重采样到 24 kHz 再返回；
        - 返回的是**裸 mx.array，不带采样率**；
        - 而模型的 `_encode_wav` 拿到数组时**只能按它自己假定的速率解释**，
          拿到**路径**时才会自己正确解码 + 重采样。

        后果：参考音频被按错误速率解释 → **克隆出来的音高整体偏了一个八度**。
        实测同一条 48 kHz 参考（F0 142.4 Hz）：

            给路径（正确）→ 输出 147.7 / 133.7 / 150.5 Hz  ✅ 跟住
            给数组（原做法）→ 输出 287.4 Hz                ❌ 高了一倍

        这就是「换成谁的参考都是同一个机器人声」的真正原因 ——
        跟参考音频的质量、跟 instruct、跟量化档都无关。

        代价：每次合成都让模型重新读一次参考 wav（约 1 MB / 10.9 秒；98 秒的
        约 9 MB）。相对一次 2.4–6 s 的合成，这点 IO 可以忽略；**正确性优先**。
        """
        if entry is None:
            return None, None
        return str(entry["wav"]), entry.get("ref_text")

    def _generate(self, text, style, tone, entry):
        """Collect every segment of the generator into one waveform.

        ``mlx_audio``'s ``generate`` is a **generator**, not a call returning one
        result: a long text can come back as several segments that have to be
        joined in order. Measured on an M1 Max, a short spoken sentence is
        emitted as a single segment — 2.4-4.2 s from call to full audio — so
        there is currently nothing to stream to the caller early.
        """
        chunks = []
        sample_rate = None
        # **钉音色**：给了参考音频就走克隆模式，不给才会每次随机换人。
        ref_audio, ref_text = self._load_ref(entry)
        # ⚠️ **方言档只给方言名，不拼语气描述。**
        #
        # 文档的 "Keep Instructions Simple" 警告：控制指令里加太多复杂描述
        # 「might spoil the broth」。而 jason 实测认可的那一版四川话（2026-09-15）
        # 用的正文前缀就只有 `(四川话)` 三个字、**没有**任何语气描述。
        #
        # 所以：方言（含粤语等）→ instruct = 方言名本身；
        # 其余语系（普通话/英/泰）→ 保留「语气，语系」的拼法（语气是实测调出来的）。
        if style in DIALECT_STYLES:
            instruct = STYLE_INSTRUCTS[style]
        else:
            instruct = f"{TONE_INSTRUCTS[tone]}，{STYLE_INSTRUCTS[style]}"
        if ref_audio is None:
            log_once_no_voice()
        try:
            for segment in self._model.generate(
                text=text,
                max_tokens=self.MAX_TOKENS,
                ref_audio=ref_audio,
                ref_text=ref_text,
                instruct=instruct,
            ):
                audio = getattr(segment, "audio", None)
                if audio is None:
                    continue
                chunks.append(audio)
                sample_rate = getattr(segment, "sample_rate", None) or sample_rate
        except TTSError:
            raise
        except Exception as error:  # noqa: BLE001 - reported as a 500 JSON body
            raise TTSError(500, f"VoxCPM2 synthesis failed: {error}") from None
        if not chunks:
            raise TTSError(500, "VoxCPM2 produced no audio segments.")
        return join_audio(chunks), sample_rate


def read_wav(path):
    data = Path(path).read_bytes()
    if len(data) > MAX_WAV_BYTES:
        raise TTSError(500, "Generated WAV exceeds the 16 MiB response limit.")
    validate_wav_bytes(data)
    return data


def validate_wav_bytes(data):
    """Reject empty or non-16-bit PCM before it reaches the caller."""
    if len(data) > MAX_WAV_BYTES:
        raise TTSError(500, "Generated WAV exceeds the 16 MiB response limit.")
    try:
        with wave.open(io.BytesIO(data), "rb") as audio:
            frames = audio.getnframes()
            frame_size = audio.getnchannels() * audio.getsampwidth()
            if audio.getsampwidth() != 2 or frames == 0:
                raise ValueError("Expected non-empty 16-bit PCM")
            if len(audio.readframes(frames)) != frames * frame_size:
                raise ValueError("Truncated audio")
    except (wave.Error, EOFError, ValueError):
        raise TTSError(500, "The TTS backend produced an empty or invalid 16-bit WAV.") from None
    return data


def build_backend(kind, model=None, timeout=None, queue_wait=0.0,
                  voices_dir=None, default_voice=None, default_style=None,
                  default_tone=None):
    if kind == "say":
        return SayBackend(timeout=timeout if timeout is not None else 30)
    if kind == "voxcpm2":
        return VoxCpm2Backend(
            model=model or DEFAULT_VOXCPM2_MODEL,
            timeout=timeout if timeout is not None else 60,
            queue_wait=queue_wait,
            voices_dir=voices_dir,
            default_voice=default_voice,
            default_style=default_style,
            default_tone=default_tone,
        )
    raise TTSError(500, f"unknown backend {kind!r}")


def describe_for_log(backend):
    return json.dumps(backend.describe(), ensure_ascii=False)
