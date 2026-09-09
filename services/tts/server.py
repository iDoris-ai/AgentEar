#!/usr/bin/env python3
"""Local macOS TTS sidecar. Run with: python3 services/tts/server.py"""

import argparse
import io
import json
import math
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


VOICES = {"zh": "Tingting", "en": "Samantha", "th": "Kanya"}
MAX_BODY_BYTES = 64 * 1024
MAX_WAV_BYTES = 16 * 1024 * 1024
CONNECTION_TIMEOUT = 5


class TTSError(Exception):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


def validate_request(payload):
    if not isinstance(payload, dict):
        raise TTSError(400, "The JSON body must be an object.")
    text = payload.get("text")
    lang = payload.get("lang")
    if not isinstance(text, str) or not text.strip():
        raise TTSError(400, "text must be a non-empty string.")
    if not isinstance(lang, str) or lang not in VOICES:
        raise TTSError(400, "lang must be exactly one of: zh, en, th.")
    try:
        text.encode("utf-8")
    except UnicodeEncodeError:
        raise TTSError(400, "text must contain valid Unicode.") from None
    return text, VOICES[lang]


def read_wav(path):
    with path.open("rb") as source:
        data = source.read(MAX_WAV_BYTES + 1)
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
        raise TTSError(500, "macOS produced an empty or invalid 16-bit WAV.") from None
    return data


class MacOSTTS:
    def __init__(self, timeout=30):
        self.timeout = timeout
        self._slots = threading.BoundedSemaphore(5)
        self._lock = threading.Lock()
        self._children = set()
        self._stopping = False

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

    def synthesize(self, text, voice):
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

    def close(self):
        with self._lock:
            self._stopping = True
            for child in self._children:
                try:
                    child.kill()
                except ProcessLookupError:
                    pass
        # Each request's communicate() reaps its child before the server joins workers.


class TTSHandler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        # Do not log request paths or user text in this local speech service.
        pass

    def reply(self, status, content_type, body):
        self.close_connection = True
        try:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, TimeoutError):
            pass

    def reply_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.reply(status, "application/json", body)

    def do_GET(self):
        if self.path == "/health":
            self.reply_json(200, {"ok": True})
        elif self.path == "/voices":
            self.reply_json(200, VOICES)
        else:
            self.reply_json(404, {"error": "Unknown endpoint."})

    def read_json(self):
        if self.headers.get_content_type() != "application/json":
            raise TTSError(400, "Content-Type must be application/json.")
        lengths = self.headers.get_all("Content-Length", [])
        if self.headers.get("Transfer-Encoding") or len(lengths) != 1:
            raise TTSError(400, "Send one Content-Length header; chunked bodies are not supported.")
        try:
            length = int(lengths[0])
        except ValueError:
            raise TTSError(400, "Content-Length must be a positive integer.") from None
        if length <= 0:
            raise TTSError(400, "The JSON body must not be empty.")
        if length > MAX_BODY_BYTES:
            raise TTSError(413, "JSON body exceeds the 64 KiB request limit.")
        try:
            body = self.rfile.read(length)
        except TimeoutError:
            raise TTSError(408, "Timed out reading the request body.") from None
        if len(body) != length:
            raise TTSError(400, "Incomplete request body.")
        try:
            return json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
            raise TTSError(400, "Body must be valid UTF-8 JSON.") from None

    def do_POST(self):
        if self.path != "/speak":
            self.reply_json(404, {"error": "Unknown endpoint."})
            return
        try:
            text, voice = validate_request(self.read_json())
            wav_bytes = self.server.engine.synthesize(text, voice)
        except TTSError as error:
            self.reply_json(error.status, {"error": str(error)})
            return
        self.reply(200, "audio/wav", wav_bytes)


class TTSServer(ThreadingHTTPServer):
    daemon_threads = False

    def __init__(self, address, engine):
        self.engine = engine
        self._connections = set()
        self._connections_lock = threading.Lock()
        super().__init__(address, TTSHandler)

    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(CONNECTION_TIMEOUT)
        with self._connections_lock:
            self._connections.add(connection)
        return connection, address

    def shutdown_request(self, request):
        with self._connections_lock:
            self._connections.discard(request)
        super().shutdown_request(request)

    def server_close(self):
        self.engine.close()
        # Unblock readers too, including a client that never finishes its JSON body.
        with self._connections_lock:
            for connection in self._connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        super().server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--timeout", type=float, default=30, help="say + afconvert timeout in seconds")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("V1 requires macOS with say and afconvert.")
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535 (0 selects an available port).")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a positive finite number.")
    try:
        with TTSServer(("127.0.0.1", args.port), MacOSTTS(args.timeout)) as server:
            print(f"AgentEar TTS listening on http://127.0.0.1:{server.server_port}", flush=True)
            try:
                server.serve_forever(poll_interval=0.1)
            except KeyboardInterrupt:
                pass
    except OSError as error:
        parser.exit(1, f"Cannot start TTS service: {error}\n")


if __name__ == "__main__":
    main()
