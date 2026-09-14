# Local TTS Service

Standalone HTTP sidecar. Two backends behind one contract; task T3.3.1 in
`docs/tasks/tts-module-arm.md` defined the interface, and it has not changed.
The Rust daemon now calls this service over that same contract
(`src/talk.rs`), so the interface being stable is what made the backend swap a
one-flag change.

| `--backend` | What it is | Needs |
| --- | --- | --- |
| `voxcpm2` (**default**) | `mlx-community/VoxCPM2-4bit`, 48 kHz, held resident in this process | a Python 3.11+ venv with `mlx-audio`, plus the model on disk |
| `say` | macOS `say` + `afconvert`, 22.05 kHz | nothing (zero downloads, zero packages) |

`say` is the fallback required by ADR-0007 §4.2 — it keeps the link working when
no model is installed. Measured numbers for both paths, and the MLX
thread-affinity trap that killed the first voxcpm2 version, are in
[`docs/benchmarks-talk.md`](../../docs/benchmarks-talk.md).

## Run

From the repository root:

```bash
# recommended: VoxCPM2-4bit, via scripts/serve-tts.sh
scripts/setup-talk.sh      # once: downloads the model (~2.1 GB, not shipped with the app)
scripts/serve-tts.sh

# or by hand
~/.agentear/llm/venv/bin/python services/tts/server.py \
  --port 8765 --backend voxcpm2 --model vendor/models/talk/voxcpm2-4bit

# zero-dependency fallback
python3 services/tts/server.py --backend say
```

The service listens only on `127.0.0.1:8765`. Use `--port 8766` if that port is
occupied, or `--port 0` to select an available port (printed at startup).
Stop with Ctrl+C. On shutdown, active audio subprocesses are killed and reaped,
temporary files are removed, and request workers are joined before exit.

⚠️ **Start it with a Python that can import `mlx-audio`.** `/usr/bin/python3` is
3.9 and cannot install mlx; the voxcpm2 backend fails at startup with a message
saying exactly that, rather than silently downgrading to `say`.

The `say` backend's voices must be installed locally. Check with `say -v '?'`:
Chinese (`zh`) uses Tingting, English (`en`) Samantha, Thai (`th`) Kanya.
No language detection or fallback is performed. `/voices` reports which voice
this backend uses per language; `/health` is a liveness check, not an
audio-quality check. Verify voices by synthesizing and listening.

## HTTP Interface

```bash
curl --max-time 5 -i http://127.0.0.1:8765/health
curl --max-time 5 -i http://127.0.0.1:8765/voices
curl --max-time 60 --fail-with-body http://127.0.0.1:8765/speak \
  -H 'Content-Type: application/json' \
  -d '{"text":"It will rain tomorrow morning","lang":"en"}' \
  --output /tmp/agentear-tts-en.wav
afplay /tmp/agentear-tts-en.wav
```

Run playback only if curl succeeds. A failed request returns JSON, not audio.

| Endpoint | Success |
| --- | --- |
| `GET /health` | `200 application/json`, `{"ok": true, "backend": "voxcpm2", "model": ..., "sample_rate": 48000, "load_seconds": ...}` |
| `GET /voices` | `200 application/json`, the per-language target for this backend |
| `POST /speak` | `200 audio/wav`, non-empty 16-bit PCM WAV bytes |

`/health` naming the backend and model is deliberate: connecting to the wrong
service is the failure mode that looks like success (`scripts/serve-llm.sh`
records the same lesson for the LLM sidecar).

**The sample rate is the backend's own**: 22050 Hz for `say`, 48000 Hz for
VoxCPM2. The contract never pinned a rate, so callers must read it from the WAV
header. `agentear`'s `talk::validate_wav` only checks that it really is a WAV.

`POST /speak` accepts a JSON object with a non-blank string `text`, an exact
`lang` of `zh`, `en`, or `th`, and two **optional per-request overrides**:

| field | meaning |
| --- | --- |
| `voice` | name from `--voices-dir` (pins the timbre; see below) |
| `style` | dialect/accent key: `zh`, `yue`, `henan`, `sichuan`, `shandong`, `dongbei`, `tianjin`, `en`, `en-gb`, `en-us`, `en-ca`, `th` |

Unknown values are a `400` — never a silent fallback, because a wrong-sounding
voice is indistinguishable from a broken model to the caller.

### Why voices are pinned (and what happens if they are not)

VoxCPM2 is **zero-shot**: with no reference audio it samples a *new speaker every
call*. Measured 2026-09-14 on the 4-bit MLX build, three generations of the same
sentence spanned **F0 142–286 Hz（one sample in the male band）with RMS varying
4.54×** — which is exactly the "the voice keeps changing" complaint. Pinning a
reference clip (`ref_audio` + `ref_text`) removes the speaker lottery, and
`normalize_loudness()` makes the level deterministic: **RMS spread 4.54× → 1.00×**.

⚠️ **The MLX build does not implement `seed`** (the official PyTorch API and the
llama.cpp-omni CLI do). So reproducibility here comes from the reference clip,
not from a fixed seed.

Build a reference with `services/tts/make_voice.py`: it generates candidates,
scores them on F0 / voiced frames / RMS, keeps the best, and transcribes the
result with this repo's own ASR for `ref_text`. `--voices-dir` points at a
directory of `<name>.wav` + `<name>.json` pairs.

⚠️ **Dialect correctness has no objective check here.** `speech language-id`
resolves language, not Chinese dialect (ADR-0007 §6.2.2), so `style=yue` is
"steer the model", not "verified Cantonese". Only a human ear can close that.

`POST /speak` also accepts the legacy shape (no `voice`/`style`). Invalid input, including `jp`, returns `400`
with `{"error":"an explanation"}` before any synthesis starts. VoxCPM2 infers
the language from the text and has no per-language voice; `lang` is still
required and still validated, because guessing wrong means the caller hears a
language they cannot understand and cannot tell why.

With the `say` backend, user text goes to `say` through its standard input,
never through a shell or process command-line arguments. HTTP remains the only
interface between this service and other applications. The service does not
play the audio; the caller receives and plays the WAV.

Backend-specific behaviour:

- `say`: at most five syntheses at once; excess requests get `503` and may
  retry. A shared 30-second timeout covers `say` + `afconvert`; use `--timeout
  SECONDS` to change it. Expiry returns `504` after terminating the active child.
- `voxcpm2`: **one synthesis at a time** — a single MLX context is shared by the
  process, and the previous five-way concurrency existed only because `say`
  spawns a process per request. A request that cannot take the slot gets `503`
  immediately unless `--queue-wait SECONDS` is set. A timeout is reported as
  `504` but **cannot interrupt generation**: the model call is synchronous, so
  the work keeps running in the background. Measured warm cost is 2.6–3.9 s per
  short sentence on an M1 Max.
- Audio-tool failures or invalid/empty WAV output return `500` JSON.
- JSON bodies are limited to 64 KiB (`413` when exceeded), WAV responses to
  16 MiB (`500` when exceeded). A 500-character input is within the input limit.
- Send UTF-8 JSON with one `Content-Length`; chunked request bodies are not
  supported. Connections have a five-second socket inactivity timeout and
  are closed after each response.

This is a local development service, not a public HTTP deployment. It has no
authentication or CORS support. Do not expose it through a public proxy.
Application request logs are disabled to avoid recording user text. The VoxCPM2
backend keeps the model in this process and never contacts the network; start it
with `HF_HUB_OFFLINE=1` (as `scripts/serve-tts.sh` does) so that stays true even
if the local model directory is incomplete.

## Model

`mlx-community/VoxCPM2-4bit` — Apache-2.0, base model `openbmb/VoxCPM2`,
~2.1 GB on disk. Swap it with `--model <path-or-repo-id>`; a local directory is
what `scripts/serve-tts.sh` passes. Weights are downloaded on demand and are not
shipped with the application.

⚠️ **4-bit versus bf16 output quality has not been compared.** This service was
measured only for "does it work, how long, how much memory".
`docs/benchmarks-m3.md` §6 contains bf16 numbers from a different runtime
(speech-swift), so those must not be read as this path's numbers.

## Tests

```bash
python3 -m unittest discover -s services/tts -p 'test_*.py' -v
```

34 tests. They use loopback HTTP with simulated audio tools and a stub MLX
module; they need no voices, no `mlx-audio`, no model files, and they do not
play sound. They check validation, exact response bytes, concurrency, WAV
packing (including clipping and multi-segment joins), busy handling, temporary
file cleanup, invalid WAV output, child cleanup, and backend/CLI selection.

They prove the wiring, **not** speech quality. Real macOS acceptance
measurements and listening checks are also required before a PR — see
[ACCEPTANCE.md](ACCEPTANCE.md) and `docs/benchmarks-talk.md` for measured runs.

Run the real acceptance checks separately in a normal macOS Terminal:

```bash
python3 services/tts/check_macos.py
```

`check_macos.py` drives the `say` backend only (it starts the service with
default arguments, which are now `--backend voxcpm2`; pass `--backend say`
explicitly or run it against the `say` backend deliberately). The VoxCPM2 path's
acceptance is `scripts/talk-e2e.sh` plus the numbers in `docs/benchmarks-talk.md`.

If a sandboxed run produces an empty WAV even though `say` exits successfully,
try the service in a normal macOS Terminal. On the development machine, the same
three-language synthesis commands yielded zero frames in the sandbox and
non-empty WAVs outside it. Do not disable WAV validation to hide that failure.
