#!/usr/bin/env python3
"""Hermetic scenario-scripted OpenAI-compatible stub for the M2 oracle.

NO real model, NO network beyond loopback. Script-acts BOTH sides of the M2
orchestra based on marker tokens found in the conversation, and logs every
request body it receives (one JSON per line) so the oracle can assert on the
exact prompts the daemon sent — e.g. that a human steer on a CHILD session
appears in the PARENT's next prompt (no-invisible-interventions, design D4),
and that subagent reports arrive only inside UNTRUSTED DATA frames.

Scenarios (marker anywhere in any message content; precedence top-down —
M2-CHECK first because it is always the LATEST instruction in a conversation
that may still contain earlier markers):

  M2-CHECK             ALWAYS a plain final "M2-CHECK-ACK" — its logged
                       request is what the oracle inspects for steer/inject
                       visibility.
  M2-CHILD-PLEASE-ECHO plain final "M2-CHILD-REPORT-bravo" (the subagent's
                       report; never a tool call).
  M2-DISPATCH          no tool msg yet -> dispatch_subagent tool_call
                       {brief:"M2-CHILD-PLEASE-ECHO do the thing",
                        title:"child-a"}
                       tool msg present -> final "M2-DISPATCH-DONE"
  (no marker)          M0-compatible canned reply FORGE-COMPOSER-STUB-REPLY

Usage: stub-llm-m2.py <port> [request-log-path]
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
    # Arguments split into two fragments on purpose: exercises streaming accumulation.
    half = max(1, len(arguments_json) // 2)
    frag1, frag2 = arguments_json[:half], arguments_json[half:]
    return sse([
        {"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "call_m2", "type": "function",
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

        if "M2-CHECK" in convo:
            payload = text_reply("M2-CHECK-ACK")
        elif "M2-CHILD-PLEASE-ECHO" in convo:
            payload = text_reply("M2-CHILD-REPORT-bravo")
        elif "M2-DISPATCH" in convo:
            payload = (text_reply("M2-DISPATCH-DONE") if tool_msgs
                       else tool_reply("dispatch_subagent", json.dumps(
                           {"brief": "M2-CHILD-PLEASE-ECHO do the thing",
                            "title": "child-a"})))
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
