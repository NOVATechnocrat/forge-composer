#!/usr/bin/env python3
"""Hermetic OpenAI-compatible stub for the M0 oracle — NO real model, NO network.

Serves POST */chat/completions as text/event-stream with a canned two-delta reply
containing the sentinel FORGE-COMPOSER-STUB-REPLY, a usage object, then [DONE].

Usage: stub-llm.py <port>
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if not self.path.endswith("/chat/completions"):
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)  # drain request body

        chunks = [
            {"choices": [{"delta": {"content": "FORGE-COMPOSER-STUB-REPLY "}}]},
            {
                "choices": [{"delta": {"content": "pong"}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 2},
            },
        ]
        body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        payload = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):  # keep oracle output clean
        pass


def main():
    port = int(sys.argv[1])
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
