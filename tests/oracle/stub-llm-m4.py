#!/usr/bin/env python3
"""Hermetic scenario-scripted OpenAI-compatible stub for the M4 oracle.

NO real model, NO network beyond loopback. Script-acts the orchestrator (and
one subagent) for the cockpit scenarios and logs every request body it
receives (one JSON per line) so the oracle can assert on the exact prompts —
attachment framing and the history-fold truncation marker.

Scenarios (marker anywhere in any message content; precedence top-down —
M4-CHECK first because it is always the latest instruction in a conversation
that may still contain earlier markers):

  M4-CHECK        ALWAYS a plain final "M4-CHECK-ACK".
  M4-FIND-DELETE  no tool msg yet -> terminal tool_call
                    {"command":"find . -name canary.txt -delete"}
                  tool msg present -> final "M4-FIND-SEEN <last tool content>"
  M4-EDIT         no tool msg yet -> edit_file tool_call
                    {"path":"agent-note.txt","old_string":"",
                     "new_string":"M4-EDIT-CONTENT-quebec\\n"}
                  tool msg present -> final "M4-EDIT-DONE"
  M4-DISPATCH     no tool msg yet -> dispatch_subagent tool_call
                    {"brief":"M4-CHILD-EDIT do the thing","title":"child-m4"}
                  tool msg present -> final "M4-DISPATCH-DONE"
  M4-CHILD-EDIT   (the subagent; checked AFTER M4-DISPATCH so the parent's
                  conversation, which may quote the brief, never matches)
                  no tool msg yet -> edit_file tool_call
                    {"path":"child-work.txt","old_string":"",
                     "new_string":"M4-CHILD-PAYLOAD-xray\\n"}
                  tool msg present -> final "M4-CHILD-REPORT-done"
  (no marker)     M0-compatible canned reply FORGE-COMPOSER-STUB-REPLY

Usage: stub-llm-m4.py <port> [request-log-path]
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
            {"index": 0, "id": "call_m4", "type": "function",
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

        if "M4-CHECK" in convo:
            payload = text_reply("M4-CHECK-ACK")
        elif "M4-FIND-DELETE" in convo:
            payload = (text_reply("M4-FIND-SEEN " + last_tool) if tool_msgs
                       else tool_reply("terminal", json.dumps(
                           {"command": "find . -name canary.txt -delete"})))
        elif "M4-EDIT" in convo and "M4-DISPATCH" not in convo and "M4-CHILD-EDIT" not in convo:
            payload = (text_reply("M4-EDIT-DONE") if tool_msgs
                       else tool_reply("edit_file", json.dumps(
                           {"path": "agent-note.txt", "old_string": "",
                            "new_string": "M4-EDIT-CONTENT-quebec\n"})))
        elif "M4-DISPATCH" in convo:
            payload = (text_reply("M4-DISPATCH-DONE") if tool_msgs
                       else tool_reply("dispatch_subagent", json.dumps(
                           {"brief": "M4-CHILD-EDIT do the thing",
                            "title": "child-m4"})))
        elif "M4-CHILD-EDIT" in convo:
            payload = (text_reply("M4-CHILD-REPORT-done") if tool_msgs
                       else tool_reply("edit_file", json.dumps(
                           {"path": "child-work.txt", "old_string": "",
                            "new_string": "M4-CHILD-PAYLOAD-xray\n"})))
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
