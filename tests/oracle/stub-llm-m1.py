#!/usr/bin/env python3
"""Hermetic scenario-scripted OpenAI-compatible stub for the M1 oracle.

NO real model, NO network beyond loopback. Unlike the M0 stub (canned reply),
this one script-acts an agent based on marker tokens found in the conversation,
emitting OpenAI streaming tool_call deltas so the daemon's whole tool loop can
be proven end-to-end. It also logs every request body it receives (one JSON per
line) so the oracle can assert on the exact prompt the daemon sent — e.g. that
attachment content arrived inside UNTRUSTED DATA frames.

Scenarios (marker anywhere in any message content):
  M1-RUN-TOOL-ECHO  no tool msg yet -> terminal tool_call `echo m1-tool-echo-ok`
                    tool msg present -> final "M1-TOOL-DONE <last tool content>"
  M1-RUN-TOOL-RM    no tool msg yet -> terminal tool_call `rm -rf m1-canary-dir`
                    tool msg present -> final "M1-DENY-SEEN <last tool content>"
  M1-EDIT-NOTES     no tool msg yet -> edit_file notes.txt alpha->bravo
                    tool msg present -> final "M1-EDIT-DONE"
  M1-CANARY         ALWAYS a plain final "M1-CANARY-ACK" — never a tool call,
                    no matter what any attached/injected content demands.
  (no marker)       M0-compatible canned reply with sentinel FORGE-COMPOSER-STUB-REPLY

Usage: stub-llm-m1.py <port> [request-log-path]
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
            {"index": 0, "id": "call_m1", "type": "function",
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

        if "M1-CANARY" in convo:
            payload = text_reply("M1-CANARY-ACK")
        elif "M1-RUN-TOOL-ECHO" in convo:
            payload = (text_reply("M1-TOOL-DONE " + last_tool) if tool_msgs
                       else tool_reply("terminal", json.dumps({"command": "echo m1-tool-echo-ok"})))
        elif "M1-RUN-TOOL-RM" in convo:
            payload = (text_reply("M1-DENY-SEEN " + last_tool) if tool_msgs
                       else tool_reply("terminal", json.dumps({"command": "rm -rf m1-canary-dir"})))
        elif "M1-EDIT-NOTES" in convo:
            payload = (text_reply("M1-EDIT-DONE") if tool_msgs
                       else tool_reply("edit_file", json.dumps(
                           {"path": "notes.txt", "old_string": "alpha", "new_string": "bravo"})))
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
