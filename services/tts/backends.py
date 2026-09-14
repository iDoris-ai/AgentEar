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

    def synthesize(self, text, lang):
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
                 load_timeout=600):
        self.model_id = str(model)
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
        }

    def available_langs(self):
        # Language is inferred from the text; no separate voice per language.
        return {lang: self.model_id for lang in SUPPORTED_LANGS}

    def synthesize(self, text, lang):
        if lang not in SUPPORTED_LANGS:
            raise TTSError(400, "lang must be exactly one of: zh, en, th.")
        # The lock is what turns "second request arrives mid-synthesis" into an
        # explicit 503 instead of an unbounded queue.
        if not self._lock.acquire(timeout=self.queue_wait):
            raise TTSError(503, "TTS is busy synthesizing; retry later.")
        try:
            if self._stopping:
                raise TTSError(503, "TTS service is stopping.")
            job = Future()
            self._jobs.put((text, job))
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
            text, future = job
            try:
                audio, sample_rate = self._generate(text)
                future.set_result(wav_from_float(audio, sample_rate or self.sample_rate))
            except BaseException as error:  # noqa: BLE001 - surfaced to the caller
                future.set_exception(error)

    def _generate(self, text):
        """Collect every segment of the generator into one waveform.

        ``mlx_audio``'s ``generate`` is a **generator**, not a call returning one
        result: a long text can come back as several segments that have to be
        joined in order. Measured on an M1 Max, a short spoken sentence is
        emitted as a single segment — 2.4-4.2 s from call to full audio — so
        there is currently nothing to stream to the caller early.
        """
        chunks = []
        sample_rate = None
        try:
            for segment in self._model.generate(text=text, max_tokens=self.MAX_TOKENS):
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


def build_backend(kind, model=None, timeout=None, queue_wait=0.0):
    if kind == "say":
        return SayBackend(timeout=timeout if timeout is not None else 30)
    if kind == "voxcpm2":
        return VoxCpm2Backend(
            model=model or DEFAULT_VOXCPM2_MODEL,
            timeout=timeout if timeout is not None else 60,
            queue_wait=queue_wait,
        )
    raise TTSError(500, f"unknown backend {kind!r}")


def describe_for_log(backend):
    return json.dumps(backend.describe(), ensure_ascii=False)
