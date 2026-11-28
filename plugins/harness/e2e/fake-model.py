#!/usr/bin/env python3
"""A fake model for end-to-end tests of agent harnesses (no account, no tokens): OpenAI-compatible chat completions
for pi and OpenCode, and Gemini's generateContent for Gemini CLI (GOOGLE_GEMINI_BASE_URL). Part of Fleetlayer (FSL-1.1-ALv2).

POST /v1/chat/completions (streaming or not): answers "pong: <the last user text>". When the last user text contains
`RUN: <command>`, it asks for the `bash` tool with that command instead (so a harness shows a permission prompt), and
after the tool result it answers "ran it". When it contains `SLOW`, it waits 20 seconds before answering (so a test
can deliver a message to a working session). GET /v1/models lists one model, `fake`.

Usage: fake-model.py [port]  (default 18080, on 127.0.0.1)
"""
import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def reply_for(messages):
    last = messages[-1] if messages else {}
    if last.get("role") == "tool":
        return {"text": "ran it"}
    user = next((text_of(m.get("content")) for m in reversed(messages) if m.get("role") == "user"), "")
    m = re.search(r"RUN: (.+)", user)
    if m:
        return {"tool": {"name": "bash", "arguments": json.dumps({"command": m.group(1).strip(), "description": "run it"})}}
    if "SLOW" in user:
        time.sleep(20)
    return {"text": "pong: " + user.strip().splitlines()[-1][:200] if user.strip() else "pong"}


class H(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write("fake-model: " + fmt % args + "\n")

    def send_json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.send_json({"object": "list", "data": [{"id": "fake", "object": "model", "owned_by": "fake"}]})
        else:
            self.send_json({"error": "not found"}, 404)

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        if ":" in self.path.split("?")[0].rsplit("/", 1)[-1]:
            return self.gemini(req)
        with open("/tmp/fake-model-last.json", "w") as f:
            json.dump(req, f)
        r = reply_for(req.get("messages", []))
        cid, now = "chatcmpl-fake", int(time.time())
        if r.get("tool"):
            call = {"index": 0, "id": "call_%d" % now, "type": "function", "function": r["tool"]}
            delta, finish = {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
            message = {"role": "assistant", "content": None, "tool_calls": [call]}
        else:
            delta, finish = {"role": "assistant", "content": r["text"]}, "stop"
            message = {"role": "assistant", "content": r["text"]}
        usage = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        if not req.get("stream"):
            self.send_json({"id": cid, "object": "chat.completion", "created": now, "model": "fake",
                            "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage})
            return
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for chunk in (
            {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
            {"choices": [], "usage": usage},
        ):
            chunk.update({"id": cid, "object": "chat.completion.chunk", "created": now, "model": "fake"})
            self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")


    def gemini(self, req):
        """Gemini's generateContent, streamGenerateContent and countTokens (Gemini CLI with GOOGLE_GEMINI_BASE_URL)."""
        action = self.path.split("?")[0].rsplit(":", 1)[-1]
        if action == "countTokens":
            return self.send_json({"totalTokens": 10})
        messages = []
        for c in req.get("contents", []):
            for part in c.get("parts", []):
                if "functionResponse" in part:
                    messages.append({"role": "tool"})
                elif "text" in part:
                    messages.append({"role": "user" if c.get("role") == "user" else "assistant", "content": part["text"]})
        if req.get("generationConfig", {}).get("responseMimeType") == "application/json":
            # Gemini CLI's own side questions (routing, next speaker): a fixed answer that ends the turn.
            r = {"text": json.dumps({"reasoning": "fake", "next_speaker": "user", "model_choice": "flash"})}
        else:
            r = reply_for(messages)
        if r.get("tool"):
            part = {"functionCall": {"name": "run_shell_command", "args": json.loads(r["tool"]["arguments"])}}
        else:
            part = {"text": r["text"]}
        body = {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP", "index": 0}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
                "modelVersion": "fake"}
        if action != "streamGenerateContent":
            return self.send_json(body)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        self.wfile.write(b"data: " + json.dumps(body).encode() + b"\r\n\r\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
