#!/usr/bin/env python3
"""Local TTS sidecar (T3.3.1). Run with: python3 services/tts/server.py

The HTTP contract is fixed and shared by every backend:

    POST /speak  {"text": "...", "lang": "zh|en|th"}  ->  audio/wav
    GET  /health                                      ->  {"ok": true, ...}
    GET  /voices                                      ->  {"zh": ..., ...}

``--backend voxcpm2`` (default) uses ``mlx-community/VoxCPM2-4bit`` through
``mlx-audio``; ``--backend say`` keeps the original zero-dependency macOS path.
See ``README.md`` for the model download step and ``backends.py`` for the split.
"""

import argparse
import json
import math
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from backends import (  # noqa: F401 - read_wav/MacOSTTS re-exported for tests
    SUPPORTED_LANGS,
    VOICES,
    SayBackend,
    TTSError,
    VoxCpm2Backend,
    build_backend,
    read_wav,
)

#: Historical name for the `say` backend, kept so existing callers and the
#: acceptance script keep working after the backend split.
MacOSTTS = SayBackend

MAX_BODY_BYTES = 64 * 1024
CONNECTION_TIMEOUT = 5


def validate_request(payload):
    """Return (text, lang, voice, style, tone).

    Language is never guessed: an unknown value is a 400. ``voice`` / ``style``
    are optional per-request overrides of the backend defaults — that is what
    lets the app switch timbre or dialect for a single turn (voice command
    "请你用广东话" arrives this way) without restarting anything.
    """
    if not isinstance(payload, dict):
        raise TTSError(400, "The JSON body must be an object.")
    text = payload.get("text")
    lang = payload.get("lang")
    voice = payload.get("voice")
    style = payload.get("style")
    tone = payload.get("tone")
    if not isinstance(text, str) or not text.strip():
        raise TTSError(400, "text must be a non-empty string.")
    if not isinstance(lang, str) or lang not in SUPPORTED_LANGS:
        raise TTSError(400, "lang must be exactly one of: zh, en, th.")
    for name, value in (("voice", voice), ("style", style), ("tone", tone)):
        if value is not None and (not isinstance(value, str) or not value.strip()):
            raise TTSError(400, f"{name} must be a non-empty string when present.")
    try:
        text.encode("utf-8")
    except UnicodeEncodeError:
        raise TTSError(400, "text must contain valid Unicode.") from None
    return text, lang, voice, style, tone


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
            payload = {"ok": True}
            payload.update(self.server.engine.describe())
            self.reply_json(200, payload)
        elif self.path == "/voices":
            # 语言目标 + （voxcpm2 后端）可用的音色与语系：**菜单要从这里读**，
            # 免得菜单里写死一份、后端里再写一份，两边迟早对不上。
            payload = {"langs": self.server.engine.available_langs()}
            describe = self.server.engine.describe()
            for key in ("voices", "styles", "tones", "default_voice", "default_style", "default_tone"):
                if key in describe:
                    payload[key] = describe[key]
            self.reply_json(200, payload)
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
            text, lang, voice, style, tone = validate_request(self.read_json())
            wav_bytes = self.server.engine.synthesize(
                text, lang, voice=voice, style=style, tone=tone
            )
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


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument(
        "--backend",
        choices=("voxcpm2", "say"),
        default="voxcpm2",
        help="voxcpm2 = mlx-community/VoxCPM2-4bit (default); say = macOS built-in fallback",
    )
    parser.add_argument(
        "--model",
        default=None,
        help="VoxCPM2 model path or HuggingFace repo id (default: mlx-community/VoxCPM2-4bit)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=None,
        help="per-request synthesis timeout in seconds (default: 30 for say, 60 for voxcpm2)",
    )
    parser.add_argument(
        "--voices-dir",
        default=None,
        help="directory of <name>.wav + <name>.json voice references (voxcpm2)",
    )
    parser.add_argument(
        "--voice", default=None, help="default voice name from --voices-dir"
    )
    parser.add_argument(
        "--style", default=None, help="default dialect/accent style key (e.g. zh, yue, en-gb)"
    )
    parser.add_argument(
        "--tone", default=None, help="default tone key: warm / calm / lively / serious"
    )
    parser.add_argument(
        "--queue-wait",
        type=float,
        default=0.0,
        help="seconds a voxcpm2 request waits for the single model slot before 503 (default: 0)",
    )
    args = parser.parse_args(argv)
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535 (0 selects an available port).")
    if args.timeout is not None and (not math.isfinite(args.timeout) or args.timeout <= 0):
        parser.error("--timeout must be a positive finite number.")
    if not math.isfinite(args.queue_wait) or args.queue_wait < 0:
        parser.error("--queue-wait must be a non-negative finite number.")
    return args


def main():
    args = parse_args()
    if sys.platform != "darwin":
        print("This service requires macOS (say/afconvert, or the MLX runtime).", file=sys.stderr)
        raise SystemExit(2)
    try:
        engine = build_backend(
            args.backend,
            model=args.model,
            timeout=args.timeout,
            queue_wait=args.queue_wait,
            voices_dir=args.voices_dir,
            default_voice=args.voice,
            default_style=args.style,
            default_tone=args.tone,
        )
    except TTSError as error:
        print(f"Cannot start the {args.backend} backend: {error}", file=sys.stderr)
        raise SystemExit(1) from None
    try:
        with TTSServer(("127.0.0.1", args.port), engine) as server:
            print(
                f"AgentEar TTS ({engine.name}) listening on http://127.0.0.1:{server.server_port}",
                flush=True,
            )
            print(f"  backend: {json.dumps(engine.describe(), ensure_ascii=False)}", flush=True)
            try:
                server.serve_forever(poll_interval=0.1)
            except KeyboardInterrupt:
                pass
    except OSError as error:
        print(f"Cannot start TTS service: {error}", file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
