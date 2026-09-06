"""Standard-library tests; no installed voices or audio playback required."""

from concurrent.futures import ThreadPoolExecutor
import http.client
import io
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import wave

import server


def make_wav(marker=b"test"):
    pcm = marker + (b"\x00" if len(marker) % 2 else b"")
    output = io.BytesIO()
    with wave.open(output, "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(22050)
        audio.writeframes(pcm)
    return output.getvalue()


class FakeAudioTools:
    """Replace only the OS commands, retaining real HTTP and temporary-file logic."""

    def __init__(self, barrier=None):
        self.barrier = barrier
        self.directories = set()
        self.lock = threading.Lock()

    def __call__(self, command, deadline, input_bytes=None):
        if command[0] == "/usr/bin/say":
            aiff = Path(command[command.index("-o") + 1])
            with self.lock:
                self.directories.add(aiff.parent)
            if self.barrier:
                self.barrier.wait(timeout=5)
            voice = command[command.index("-v") + 1]
            aiff.write_bytes(voice.encode() + b":" + input_bytes)
        else:
            Path(command[2]).write_bytes(make_wav(Path(command[1]).read_bytes()))


class HTTPTests(unittest.TestCase):
    def setUp(self):
        self.engine = server.MacOSTTS()
        self.httpd = server.TTSServer(("127.0.0.1", 0), self.engine)
        self.thread = threading.Thread(target=self.httpd.serve_forever, kwargs={"poll_interval": 0.01})
        self.thread.start()
        self.addCleanup(self.thread.join, 2)
        self.addCleanup(self.httpd.server_close)
        self.addCleanup(self.httpd.shutdown)

    def request(self, method, path, payload=None, body=None, headers=None):
        if payload is not None:
            body = json.dumps(payload).encode()
        if headers is None:
            headers = {"Content-Type": "application/json"}
        connection = http.client.HTTPConnection("127.0.0.1", self.httpd.server_port, timeout=10)
        try:
            connection.request(method, path, body=body, headers=headers)
            response = connection.getresponse()
            data = response.read()
            self.assertEqual(int(response.getheader("Content-Length")), len(data))
            return response.status, response.getheader("Content-Type"), data
        finally:
            connection.close()

    def test_health_and_voices(self):
        for path, expected in [("/health", {"ok": True}), ("/voices", server.VOICES)]:
            with self.subTest(path=path):
                status, content_type, body = self.request("GET", path)
                self.assertEqual((status, content_type), (200, "application/json"))
                self.assertEqual(json.loads(body), expected)

    def test_all_languages_return_their_own_wav(self):
        cases = [("zh", "Tingting", "\u660e\u5929"), ("en", "Samantha", "Hello"),
                 ("th", "Kanya", "\u0e2a\u0e27\u0e31\u0e2a\u0e14\u0e35")]
        with patch.object(self.engine, "_run", side_effect=FakeAudioTools()):
            for lang, voice, text in cases:
                with self.subTest(lang=lang):
                    status, content_type, body = self.request("POST", "/speak", {"lang": lang, "text": text})
                    self.assertEqual((status, content_type), (200, "audio/wav"))
                    self.assertEqual(body, make_wav(f"{voice}:{text}".encode()))

    def test_invalid_input_never_runs_audio_tools(self):
        payloads = [[], {}, {"text": "", "lang": "en"}, {"text": " \n\t", "lang": "en"},
                    {"text": 7, "lang": "en"}, {"text": "hello", "lang": "jp"},
                    {"text": "hello", "lang": ["en"]}, {"text": "hello", "lang": None},
                    {"text": "hello", "lang": "EN"}, {"text": "\ud800", "lang": "en"}]
        with patch.object(self.engine, "_run") as run:
            for payload in payloads:
                with self.subTest(payload=payload):
                    status, content_type, body = self.request("POST", "/speak", payload)
                    self.assertEqual((status, content_type), (400, "application/json"))
                    self.assertTrue(json.loads(body)["error"])
            run.assert_not_called()

    def test_invalid_json_and_headers(self):
        cases = [(b"{", {"Content-Type": "application/json"}, 400),
                 (b"\xff", {"Content-Type": "application/json"}, 400),
                 (b"null", {"Content-Type": "application/json"}, 400),
                 (b"{}", {"Content-Type": "text/plain"}, 400),
                 (b"", {"Content-Type": "application/json", "Content-Length": "-1"}, 400),
                 (b"", {"Content-Type": "application/json", "Content-Length": "bad"}, 400),
                 (b"", {"Content-Type": "application/json", "Transfer-Encoding": "chunked"}, 400),
                 (b"", {"Content-Type": "application/json", "Content-Length": "65537"}, 413)]
        with patch.object(self.engine, "_run") as run:
            for body, headers, expected in cases:
                with self.subTest(headers=headers, body=body):
                    status, _, response = self.request("POST", "/speak", body=body, headers=headers)
                    self.assertEqual(status, expected)
                    self.assertTrue(json.loads(response)["error"])
            run.assert_not_called()

    def test_unknown_endpoint(self):
        for method in ["GET", "POST"]:
            self.assertEqual(self.request(method, "/missing")[0], 404)

    def test_500_characters_and_option_like_text(self):
        with patch.object(self.engine, "_run", side_effect=FakeAudioTools()):
            for text in ["hello " * 83 + "hi", "-v Kanya; $(echo test)\nGoodbye"]:
                status, _, body = self.request("POST", "/speak", {"text": text, "lang": "en"})
                self.assertEqual(status, 200)
                self.assertEqual(body, make_wav(f"Samantha:{text}".encode()))

    def test_five_concurrent_requests_are_isolated_and_cleaned_up(self):
        fake_tools = FakeAudioTools(threading.Barrier(5))
        payloads = [{"text": f"request {index}", "lang": lang}
                    for index, lang in enumerate(["en", "th", "zh", "en", "th"])]
        with patch.object(self.engine, "_run", side_effect=fake_tools):
            with ThreadPoolExecutor(max_workers=5) as pool:
                results = list(pool.map(lambda payload: self.request("POST", "/speak", payload), payloads))
        for payload, (status, content_type, body) in zip(payloads, results):
            self.assertEqual((status, content_type), (200, "audio/wav"))
            marker = f"{server.VOICES[payload['lang']]}:{payload['text']}".encode()
            self.assertEqual(body, make_wav(marker))
        self.assertEqual(len(fake_tools.directories), 5)
        self.assertTrue(all(not path.exists() for path in fake_tools.directories))

    def test_backend_failures_are_json_and_health_still_works(self):
        errors = [(server.TTSError(504, "Speech synthesis timed out."), 504),
                  (OSError("missing tool"), 500)]
        for error, expected in errors:
            with self.subTest(error=error):
                with patch.object(self.engine, "_run", side_effect=error):
                    status, content_type, body = self.request("POST", "/speak", {"text": "hello", "lang": "en"})
                    self.assertEqual((status, content_type), (expected, "application/json"))
                    self.assertTrue(json.loads(body)["error"])
                self.assertEqual(self.request("GET", "/health")[0], 200)

    def test_empty_audio_is_never_a_successful_response(self):
        def empty_conversion(command, deadline, input_bytes=None):
            if command[0] == "/usr/bin/afconvert":
                Path(command[2]).write_bytes(make_wav(b""))
        with patch.object(self.engine, "_run", side_effect=empty_conversion):
            status, content_type, body = self.request("POST", "/speak", {"text": "hello", "lang": "en"})
        self.assertEqual((status, content_type), (500, "application/json"))
        self.assertIn("invalid", json.loads(body)["error"])

    def test_busy_is_explicit(self):
        for _ in range(5):
            self.engine._slots.acquire()
            self.addCleanup(self.engine._slots.release)
        self.assertEqual(self.request("POST", "/speak", {"text": "hello", "lang": "en"})[0], 503)

    def test_shutdown_unblocks_an_incomplete_request(self):
        client = socket.create_connection(("127.0.0.1", self.httpd.server_port), timeout=2)
        self.addCleanup(client.close)
        client.sendall(b"POST /speak HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            with self.httpd._connections_lock:
                if self.httpd._connections:
                    break
            time.sleep(0.01)
        started = time.monotonic()
        self.httpd.shutdown()
        self.httpd.server_close()
        self.assertLess(time.monotonic() - started, 1)
        self.assertFalse(self.httpd._connections)


class AudioTests(unittest.TestCase):
    def test_invalid_and_truncated_wav(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.wav"
            for data in [b"", b"not a wav", make_wav(b""), make_wav()[:-1]]:
                with self.subTest(data=data):
                    path.write_bytes(data)
                    with self.assertRaises(server.TTSError) as caught:
                        server.read_wav(path)
                    self.assertEqual(caught.exception.status, 500)

    def test_child_failure_stops_pipeline_and_removes_directory(self):
        engine = server.MacOSTTS()
        for failed_tool in ["/usr/bin/say", "/usr/bin/afconvert"]:
            calls = []
            def fail(command, deadline, input_bytes=None):
                calls.append(command)
                if command[0] == failed_tool:
                    raise server.TTSError(500, "failed")
            with self.subTest(tool=failed_tool), patch.object(engine, "_run", side_effect=fail):
                with self.assertRaises(server.TTSError):
                    engine.synthesize("hello", "Samantha")
                self.assertEqual(calls[-1][0], failed_tool)
                self.assertFalse(Path(calls[0][4]).parent.exists())

    def test_process_failure_returns_clear_error(self):
        engine = server.MacOSTTS()
        with self.assertRaises(server.TTSError) as caught:
            engine._run([sys.executable, "-c", "raise SystemExit(7)"], time.monotonic() + 5)
        self.assertEqual(caught.exception.status, 500)
        self.assertIn("exit 7", str(caught.exception))
        self.assertFalse(engine._children)

    def test_timeout_kills_and_reaps_the_child(self):
        engine = server.MacOSTTS()
        children = []
        real_popen = subprocess.Popen
        def capture(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            children.append(child)
            return child
        with patch.object(server.subprocess, "Popen", side_effect=capture):
            with self.assertRaises(server.TTSError) as caught:
                engine._run([sys.executable, "-c", "import time; time.sleep(30)"], time.monotonic() + 0.2)
        self.assertEqual(caught.exception.status, 504)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode)
        self.assertFalse(engine._children)

    def test_shutdown_kills_active_child_and_rejects_new_commands(self):
        engine = server.MacOSTTS()
        with ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(engine._run, [sys.executable, "-c", "import time; time.sleep(30)"], time.monotonic() + 5)
            try:
                deadline = time.monotonic() + 2
                children = []
                while time.monotonic() < deadline:
                    with engine._lock:
                        children = list(engine._children)
                    if children:
                        break
                    time.sleep(0.01)
                self.assertEqual(len(children), 1)
            finally:
                engine.close()
            with self.assertRaises(server.TTSError) as caught:
                future.result(timeout=2)
            self.assertEqual(caught.exception.status, 503)
            self.assertIsNotNone(children[0].returncode)
        with self.assertRaises(server.TTSError) as caught:
            engine._run([sys.executable, "-c", "pass"], time.monotonic() + 1)
        self.assertEqual(caught.exception.status, 503)
        self.assertFalse(engine._children)


if __name__ == "__main__":
    unittest.main()
