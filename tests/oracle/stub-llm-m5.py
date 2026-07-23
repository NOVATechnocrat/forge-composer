#!/usr/bin/env python3
"""Hermetic scenario-scripted OpenAI-compatible stub for the M5 oracle.

NO real model, NO network beyond loopback. M5's anchors are about routing and
prompt assembly, not tool loops, so this stub only ever produces plain text —
but it logs every request body (one JSON per line), which is what the oracle
inspects: the "model" field proves which role's config the daemon used, and
the messages prove system-prompt rules folding.

Scenarios (marker anywhere in any message content):

  M5-CHECK     plain final "M5-CHECK-ACK".
  (no marker)  M0-compatible canned reply FORGE-COMPOSER-STUB-REPLY

Usage: stub-llm-m5.py <port> [request-log-path]
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG_PATH = None


def sse(chunks):
    return ("".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n").encode()


def text_reply(text):
    return sse([
        {"choices": [{"delta": {"content": text}}]},
        {"choices": [{"delta": {}}], "usage": {"prompt_tokens": 11, "completion_tokens": 2}},
    ])


def msg_text(m):
    c = m.get("content")
    return c if isinstance(c, str) else json.dumps(c)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if not self.path.endswith("/chat/completions"):
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        if LOG_PATH:
            with open(LOG_PATH, "a") as f:
                f.write(json.dumps({"path": self.path, "body": body}) + "\n")

        convo = "\n".join(msg_text(m) for m in body.get("messages", []))
        if "M5-CHECK" in convo:
            payload = text_reply("M5-CHECK-ACK")
        else:
            payload = sse([
                {"choices": [{"delta": {"content": "FORGE-COMPOSER-STUB-REPLY "}}]},
                {"choices": [{"delta": {"content": "pong"}}],
                 "usage": {"prompt_tokens": 7, "completion_tokens": 2}},
            ])

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):  # keep oracle output clean
        pass


def main():
    global LOG_PATH
    port = int(sys.argv[1])
    if len(sys.argv) > 2:
        LOG_PATH = sys.argv[2]
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
