#!/usr/bin/env python3
"""Hermetic scenario-scripted OpenAI-compatible stub for the M3 oracle.

NO real model, NO network beyond loopback. Script-acts the orchestrator for
the Judge-bridge / escalation / mint-canary scenarios and logs every request
body (one JSON per line).

Scenarios (marker anywhere in any message content; precedence top-down):

  M3-RUN-GATE      no tool msg yet -> run_gate tool_call {"target":"demo-app"}
                   tool msg present -> final "M3-GATE-DONE <last tool content>"
  M3-RUN-BAD-GATE  no tool msg yet -> run_gate tool_call {"target":"bad-app"}
                   tool msg present -> final "M3-BADGATE-SEEN"
  M3-TRY-MINT      no tool msg yet -> terminal tool_call
                     {"command":"bash ../fakeforgeloop/harness/mint.sh --status minted"}
                   tool msg present -> final "M3-MINT-SEEN <last tool content>"
  M3-ESCALATE      ALWAYS a plain final "M3-ESCALATED-OK" — this stub only
                   answers on the GOOD provider port; the oracle points the
                   primary role at a dead port, so reaching this reply at all
                   proves the escalation chain.
  (no marker)      M0-compatible canned reply FORGE-COMPOSER-STUB-REPLY

Usage: stub-llm-m3.py <port> [request-log-path]
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
        {"choices": [{"delta": {}}], "usage": {"prompt_tokens": 7, "completion_tokens": 2}},
    ])


def tool_reply(name, arguments_json):
    half = max(1, len(arguments_json) // 2)
    frag1, frag2 = arguments_json[:half], arguments_json[half:]
    return sse([
        {"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "call_m3", "type": "function",
             "function": {"name": name, "arguments": frag1}}]}}]},
        {"choices": [{"delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": frag2}}]}}]},
        {"choices": [{"delta": {}}], "usage": {"prompt_tokens": 9, "completion_tokens": 4}},
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

        messages = body.get("messages", [])
        convo = "\n".join(msg_text(m) for m in messages)
        tool_msgs = [m for m in messages if m.get("role") == "tool"]
        last_tool = msg_text(tool_msgs[-1]) if tool_msgs else ""

        if "M3-ESCALATE" in convo:
            payload = text_reply("M3-ESCALATED-OK")
        elif "M3-RUN-GATE" in convo:
            payload = (text_reply("M3-GATE-DONE " + last_tool) if tool_msgs
                       else tool_reply("run_gate", json.dumps({"target": "demo-app"})))
        elif "M3-RUN-BAD-GATE" in convo:
            payload = (text_reply("M3-BADGATE-SEEN") if tool_msgs
                       else tool_reply("run_gate", json.dumps({"target": "bad-app"})))
        elif "M3-TRY-MINT" in convo:
            payload = (text_reply("M3-MINT-SEEN " + last_tool) if tool_msgs
                       else tool_reply("terminal", json.dumps(
                           {"command": "bash ../fakeforgeloop/harness/mint.sh --status minted"})))
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
