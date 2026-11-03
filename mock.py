#!/usr/bin/env python3
"""Deterministic OpenAI-compatible mock for protocol testing.

Not part of the binary: a dev tool. Serves `/v1/models` and
`/v1/chat/completions` with fixed SSE output, so streaming, tool-call
accumulation and the event loop can be exercised without a provider.

The reply is chosen by the last user message:

  text:...        stream that text, split into small chunks
  split:...       same, but each character arrives in its own chunk
  blank:...       same, padded with leading and trailing blank lines
  slow:...        same, one chunk every 20 ms
  tool:NAME:ARG   one tool call, arguments streamed in pieces
  reason:...      reasoning deltas first, then the text after `|`
  empty:          no content at all, no calls

Usage: mock.py [port]
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "mock/mock-1"

CHUNKS = {
    "id": "chatcmpl-mock",
    "object": "chat.completion.chunk",
    "model": MODEL,
}


def frame(delta, finish=None):
    body = {
        "id": CHUNKS["id"],
        "object": CHUNKS["object"],
        "created": 0,
        "model": CHUNKS["model"],
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    return f"data: {json.dumps(body)}\n\n".encode()


def plan(reply):
    """A list of (delay, bytes) for one completion."""
    kind, _, rest = reply.partition(":")
    out = []

    if kind == "empty":
        return [(0.0, frame({"role": "assistant", "content": ""})), (0.0, b"data: [DONE]\n\n")]

    if kind == "tool":
        name, _, args = rest.partition(":")
        out.append((0.0, frame({"role": "assistant", "content": ""})))
        out.append(
            (
                0.0,
                frame(
                    {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_mock",
                                "type": "function",
                                "function": {"name": name, "arguments": ""},
                            }
                        ]
                    }
                ),
            )
        )
        # Arguments arrive in pieces: this is what accumulation must survive.
        for i in range(0, len(args), 7):
            out.append(
                (
                    0.0,
                    frame(
                        {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "function": {"arguments": args[i : i + 7]},
                                }
                            ]
                        }
                    ),
                )
            )
        out.append((0.0, frame({}, finish="tool_calls")))
        out.append((0.0, b"data: [DONE]\n\n"))
        return out

    if kind == "reason":
        text, _, answer = rest.partition("|")
        for piece in wrap(text, 6):
            out.append((0.0, frame({"reasoning_content": piece})))
        rest = answer
        kind = "text"

    text = rest
    if kind == "blank":
        text = "\n\n" + text + "\n\n\n"
    for piece in wrap(text, 4 if kind != "split" else 1):
        out.append((0.02 if kind == "slow" else 0.0, frame({"content": piece})))
    out.append((0.0, frame({}, finish="stop")))
    out.append((0.0, b"data: [DONE]\n\n"))
    return out


def wrap(text, n):
    return [text[i : i + n] for i in range(0, len(text), n)] or [""]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = json.dumps(
            {"object": "list", "data": [{"id": MODEL, "context_length": 8192}]}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        reply = "text:ok"
        for message in reversed(request.get("messages", [])):
            if message.get("role") == "user":
                reply = message.get("content") or reply
                break

        # Chunked, like a real provider: the client must decode framing.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        for delay, data in plan(reply):
            if delay:
                time.sleep(delay)
            # A keep-alive comment ahead of each event: SSE parsers must skip it.
            for part in (b": keep-alive\n\n", data):
                self.wfile.write(b"%x\r\n%s\r\n" % (len(part), part))
                self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
    print(f"mock on http://127.0.0.1:{port}/v1", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
