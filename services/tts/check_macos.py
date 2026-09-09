#!/usr/bin/env python3
"""Run real macOS checks via curl; save WAVs and measurements in a temporary directory."""

from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import platform
import select
import signal
import subprocess
import sys
import tempfile
import time
import wave


def request(url, method, path, output, payload=None):
    command = [
        "/usr/bin/curl", "--silent", "--show-error", "--noproxy", "*",
        "--max-time", "35", "--request", method, url + path,
        "--output", str(output), "--write-out", "%{json}",
    ]
    data = None
    if payload is not None:
        command += ["-H", "Content-Type: application/json", "--data-binary", "@-"]
        data = json.dumps(payload).encode("utf-8")
    result = subprocess.run(command, input=data, capture_output=True, timeout=40, check=True)
    metadata = json.loads(result.stdout)
    return {"status": metadata["http_code"], "content_type": metadata["content_type"],
            "seconds": metadata["time_total"], "file": str(output)}


def inspect_wav(path):
    with wave.open(str(path), "rb") as audio:
        frames = audio.getnframes()
        pcm = audio.readframes(frames)
        assert frames > 0 and audio.getsampwidth() == 2
        assert len(pcm) == frames * audio.getnchannels() * 2
        return {"frames": frames, "sample_rate": audio.getframerate(),
                "channels": audio.getnchannels(), "sample_width_bytes": 2,
                "audio_seconds": frames / audio.getframerate(),
                "pcm_sha256": hashlib.sha256(pcm).hexdigest()}


def process_table():
    result = subprocess.run(
        ["/bin/ps", "-axo", "pid=,ppid=,comm="], capture_output=True, text=True, check=True, timeout=5,
    )
    rows = [line.strip().split(None, 2) for line in result.stdout.splitlines()]
    return [(int(pid), int(parent), name) for pid, parent, name in rows if pid.isdigit()]


def main():
    if sys.platform != "darwin":
        raise SystemExit("Real V1 acceptance checks require macOS.")
    output = Path(tempfile.mkdtemp(prefix="agentear-tts-http-"))
    report = {
        "environment": {"macos": platform.mac_ver()[0], "architecture": platform.machine(),
                        "python": platform.python_version()},
        "output": str(output),
        "timing_note": "One run; HTTP timings use curl time_total. Not a post-reboot voice cold start.",
        "listening": "Pending: listen to the new HTTP WAVs; prior direct-say listening is separate.",
        "checks": {},
    }
    print(f"Acceptance artifacts: {output}", flush=True)
    started = time.perf_counter()
    process = subprocess.Popen(
        [sys.executable, str(Path(__file__).with_name("server.py")), "--port", "0"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        if not select.select([process.stdout], [], [], 5)[0]:
            raise RuntimeError("Service did not announce startup within five seconds.")
        line = process.stdout.readline().strip()
        if not line:
            process.wait(timeout=5)
            raise RuntimeError(process.stderr.read().strip() or "Service exited before startup.")
        if not line.startswith("AgentEar TTS listening on http://127.0.0.1:"):
            raise RuntimeError(f"Unexpected service startup: {line!r}")
        url = line.rsplit(" ", 1)[1]
        health = request(url, "GET", "/health", output / "health.json")
        report["process_start_to_health_seconds"] = time.perf_counter() - started
        assert health["status"] == 200
        assert json.loads((output / "health.json").read_text()) == {"ok": True}
        voices = request(url, "GET", "/voices", output / "voices.json")
        assert voices["status"] == 200
        assert json.loads((output / "voices.json").read_text()) == {
            "zh": "Tingting", "en": "Samantha", "th": "Kanya",
        }

        def speak(name, text, lang="en"):
            result = request(url, "POST", "/speak", output / f"{name}.wav", {"text": text, "lang": lang})
            assert (result["status"], result["content_type"]) == (200, "audio/wav"), result
            result.update(inspect_wav(result["file"]))
            result.update({"text": text, "lang": lang, "characters": len(text)})
            return result

        languages = [
            ("th", "\u0e2a\u0e27\u0e31\u0e2a\u0e14\u0e35\u0e04\u0e23\u0e31\u0e1a \u0e27\u0e31\u0e19\u0e19\u0e35\u0e49\u0e1d\u0e19\u0e08\u0e30\u0e15\u0e01"),
            ("en", "It will rain tomorrow morning"),
            ("zh", "\u660e\u5929\u65e9\u4e0a\u4f1a\u4e0b\u96e8"),
        ]
        report["checks"]["languages"] = [speak(lang, text, lang) for lang, text in languages]
        for name, payload in [
            ("unsupported_lang", {"text": "Hello", "lang": "jp"}),
            ("empty_text", {"text": "", "lang": "en"}),
        ]:
            result = request(url, "POST", "/speak", output / f"{name}.json", payload)
            assert (result["status"], result["content_type"]) == (400, "application/json")
            result["body"] = json.loads(Path(result["file"]).read_text())
            assert result["body"]["error"]
            report["checks"][name] = result
        assert request(url, "GET", "/health", output / "health-after-errors.json")["status"] == 200

        long_text = "hello " * 83 + "hi"
        assert len(long_text) == 500
        report["checks"]["500_characters"] = speak("500-characters", long_text)

        english = "Tomorrow morning the weather will be rainy so please remember to bring your umbrella before leaving home for the office."
        chinese = "\u660e\u5929\u65e9\u4e0a\u53ef\u80fd\u4f1a\u4e0b\u96e8\u8bf7\u5927\u5bb6\u51fa\u95e8\u65f6\u8bb0\u5f97\u5e26\u96e8\u4f1e"
        assert len(english.split()) == 20 and len(chinese) == 20
        report["checks"]["english_20_words"] = speak("english-20-words", english)
        report["checks"]["chinese_20_characters"] = speak("chinese-20-characters", chinese, "zh")

        texts = [
            "Request one. The red train arrives at noon.",
            "Request two. Please close the kitchen window.",
            "Request three. My meeting begins at nine.",
            "Request four. Remember to bring an umbrella.",
            "Request five. The book is on the table.",
        ]
        baselines = [speak(f"baseline-{i + 1}", text) for i, text in enumerate(texts)]
        assert len({result["pcm_sha256"] for result in baselines}) == 5
        batch_started = time.perf_counter()
        with ThreadPoolExecutor(max_workers=5) as pool:
            concurrent = list(pool.map(lambda pair: speak(f"concurrent-{pair[0] + 1}", pair[1]), enumerate(texts)))
        matches = [a["pcm_sha256"] == b["pcm_sha256"] for a, b in zip(baselines, concurrent)]
        report["checks"]["concurrent"] = {
            "batch_seconds": time.perf_counter() - batch_started,
            "baselines": baselines, "results": concurrent,
            "pcm_matches_individual_baselines": matches,
        }
        assert all(matches), "PCM differs from baseline: inspect/listen; do not assume no mixups."

        with ThreadPoolExecutor(max_workers=1) as pool:
            interrupted = pool.submit(
                request, url, "POST", "/speak", output / "interrupted-response",
                {"text": "This is an active shutdown test. " * 1000, "lang": "en"},
            )
            deadline = time.monotonic() + 5
            owned = []
            while time.monotonic() < deadline:
                owned = [(pid, name) for pid, parent, name in process_table()
                         if parent == process.pid and Path(name).name == "say"]
                if owned:
                    break
                time.sleep(0.01)
            assert owned, "Did not observe an active say process; shutdown check is inconclusive."
            stopping = time.perf_counter()
            process.send_signal(signal.SIGINT)
            process.wait(timeout=5)
            elapsed = time.perf_counter() - stopping
            remaining_pids = {pid for pid, _, _ in process_table()}
            assert process.returncode == 0
            assert not any(pid in remaining_pids for pid, _ in owned)
            try:
                interrupted.result(timeout=5)
            except subprocess.CalledProcessError:
                pass
            report["checks"]["sigint"] = {
                "seconds": elapsed, "exit_code": process.returncode,
                "observed_say_pids": [pid for pid, _ in owned], "remaining_say_pids": [],
            }
        stderr = process.stderr.read()
        assert not stderr, f"Unexpected service stderr: {stderr}"
        report["automated_checks"] = "passed"
    except Exception as error:
        report["automated_checks"] = "failed"
        report["error"] = str(error)
        raise
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        process.stdout.close()
        process.stderr.close()
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    main()
