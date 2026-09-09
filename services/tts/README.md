# Local TTS Service (V1)

Standalone HTTP sidecar for task T3.3.1 in `docs/tasks/tts-module-arm.md`.
Uses Python's standard library and macOS `say` + `afconvert`; no packages,
model downloads, cloud calls, or Rust changes. Not yet wired into AgentEar.

## Run

From the repository root on macOS with Python 3:

```bash
python3 services/tts/server.py
```

The service listens only on `127.0.0.1:8765`. Use `--port 8766` if that port
is occupied, or `--port 0` to select an available port (printed at startup).
Stop with Ctrl+C. Active audio subprocesses are killed and reaped, temporary
files are removed, and request workers are joined before exit.

The configured voices must be installed locally. Check with `say -v '?'`:
Chinese (`zh`) uses Tingting, English (`en`) Samantha, and Thai (`th`) Kanya.
No language detection or fallback is performed. `/voices` reports this fixed
mapping, not an inventory of installed voices; `/health` is a liveness check,
not an audio-quality check. Verify voices by synthesizing and listening.

## HTTP Interface

```bash
curl --max-time 5 -i http://127.0.0.1:8765/health
curl --max-time 5 -i http://127.0.0.1:8765/voices
curl --max-time 35 --fail-with-body http://127.0.0.1:8765/speak \
  -H 'Content-Type: application/json' \
  -d '{"text":"It will rain tomorrow morning","lang":"en"}' \
  --output /tmp/agentear-tts-en.wav
afplay /tmp/agentear-tts-en.wav
```

Run playback only if curl succeeds. A failed request returns JSON, not audio.

| Endpoint | Success |
| --- | --- |
| `GET /health` | `200 application/json`, `{"ok": true}` |
| `GET /voices` | `200 application/json`, `{"zh":"Tingting","en":"Samantha","th":"Kanya"}` |
| `POST /speak` | `200 audio/wav`, non-empty 16-bit PCM WAV bytes |

`POST /speak` accepts a JSON object with a non-blank string `text` and an exact
`lang` of `zh`, `en`, or `th`. Invalid input, including `jp`, returns `400`
with `{"error":"an explanation"}` before starting any audio subprocess.
Each request has its own temporary directory. User text goes to `say` through
its standard input, never through a shell or process command-line arguments.
HTTP remains the only interface between this service and other applications.
The service does not play the audio; the caller receives and plays the WAV.

Implementation defaults (not additional task requirements):

- At most five syntheses at once; excess requests get `503` and may retry.
- A shared 30-second timeout for `say` + `afconvert`; use `--timeout SECONDS`
  to change it. Expiry returns `504` after terminating the active child.
- Audio-tool failures or invalid/empty WAV output return `500` JSON.
- JSON bodies are limited to 64 KiB (`413` when exceeded), WAV responses to
  16 MiB (`500` when exceeded). A 500-character input is within the input limit.
- Send UTF-8 JSON with one `Content-Length`; chunked request bodies are not
  supported. Connections have a five-second socket inactivity timeout and
  are closed after each response.

This is a local development service, not a public HTTP deployment. It has no
authentication or CORS support. Do not expose it through a public proxy.
Application request logs are disabled to avoid recording user text.

## Tests

```bash
python3 -m unittest discover -s services/tts -p 'test_*.py' -v
```

Tests use loopback HTTP with simulated audio tools, plus real short-lived
Python children for timeout and shutdown tests. They require no voices and
do not play sound. They check validation, exact response bytes, five-way
concurrency, temporary-file cleanup, invalid WAV output, and child cleanup.
They do not prove real speech quality. Real macOS acceptance measurements
and listening checks are also required before a PR.

Run the real acceptance checks separately in a normal macOS Terminal:

```bash
python3 services/tts/check_macos.py
```

This starts its own service on an available loopback port, makes requests
with the installed curl, checks WAV data, compares five simultaneous
responses against individually generated PCM baselines, and sends SIGINT
while an owned `say` process is active. It stops the service afterward.
It prints the temporary directory containing WAVs and `results.json`.
No audio plays automatically. Review speech by listening separately.
The measured development run is recorded in [ACCEPTANCE.md](ACCEPTANCE.md).

If a sandboxed run produces an empty WAV even though `say` exits successfully,
try the service in a normal macOS Terminal. On the development machine,
the same initial three-language synthesis commands yielded zero frames in
the sandbox and non-empty WAVs outside it. Do not disable WAV validation
to hide this failure.
