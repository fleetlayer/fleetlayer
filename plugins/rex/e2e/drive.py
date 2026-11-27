"""Drives fleetlayer-rex over JSON-RPC on stdio, the way a node does, against the Rex server `run` started.

Run by `run` inside the sandbox; exits non-zero when a check fails. Expects two Rex sessions: `planning` (an idle
agent) and `third` (an agent waiting on a person).
"""

import base64
import json
import os
import queue
import subprocess
import sys
import threading
import time

plugin, log_path = sys.argv[1], sys.argv[2]
home = os.environ["HOME"]
failures = []


def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        failures.append(what)


p = subprocess.Popen([plugin], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(log_path, "w"), text=True, bufsize=1)
answers, notes = queue.Queue(), []


def reader():
    for line in p.stdout:
        m = json.loads(line)
        if "id" in m and "method" not in m:
            answers.put(m)
        else:
            notes.append(m)


threading.Thread(target=reader, daemon=True).start()
n = [0]


def req(method, params, timeout=90):
    n[0] += 1
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n[0], "method": method, "params": params}) + "\n")
    m = answers.get(timeout=timeout)
    return m.get("result"), m.get("error")


def notify(method, params):
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}) + "\n")


node = {"protocol": 1, "node": "e2e", "machine": "e2e", "user": "e2e", "home": home, "config": {"program": "rex"}}
r, e = req("initialize", {"protocolVersion": 1, "clientCapabilities": {"_meta": {"fleetlayer.dev/node": node}}})
check(e is None and "fleetlayer.dev/plugin" in json.dumps(r), "initialize")

r, e = req("session/list", {})
sessions = {s["title"]: s for s in (r or {}).get("sessions", [])}
meta = lambda s: s["_meta"]["fleetlayer.dev/session"]
for s in sessions.values():
    print("     listed", s["sessionId"], meta(s).get("tab"), meta(s).get("status"), meta(s).get("harness"))
check(set(sessions) == {"planning", "third"}, "session/list lists both agents")
check(meta(sessions["planning"]).get("status") == "idle", "an idle agent reads idle")
check(meta(sessions["third"]).get("status") == "blocked", "an agent waiting on a person reads blocked")


def deliver(s, text, did):
    return req(
        "_fleetlayer.dev/session/deliver",
        {"sessionId": s["sessionId"], "address": meta(s)["address"], "deliveryId": did, "kind": "message", "from": {"label": "e2e"}, "text": text},
    )


r, e = deliver(sessions["third"], "hello", "d1")
check(e is not None and e.get("code") == -32012, "a message to a blocked agent is refused (BUSY)")
r, e = deliver(sessions["planning"], "Message from e2e via Fleetlayer:\n\nline one\nline two", "d2")
check(e is None and r.get("via") == "typed", "a message to an idle agent is typed in")

t0 = time.time()
o, e = req("_fleetlayer.dev/session/open", {"label": "opened", "cwd": home, "harness": os.path.join(os.path.dirname(plugin), "claude"), "args": [], "prompt": "first task"})
check(e is None and o.get("address"), "session/open starts an agent and types the first prompt (%.1fs)" % (time.time() - t0))
time.sleep(3)
oid = "@" + o["address"]

tc, e = req("_fleetlayer.dev/terminal/command", {"sessionId": oid, "mode": "observe", "cols": 80, "rows": 10})
check(e is None, "terminal/command")
h = subprocess.Popen([tc["command"]] + tc["args"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
time.sleep(1.5)
h.stdin.write(json.dumps({"type": "terminal.release"}) + "\n")
h.stdin.flush()
frames = [json.loads(line) for line in h.communicate(timeout=10)[0].splitlines()]
screens = [base64.b64decode(f["bytes"]).decode(errors="replace") for f in frames if f["type"] == "terminal.frame"]
check(any("heard: first task" in s for s in screens), "the attached terminal shows the agent got its first prompt")
check(frames and frames[-1]["type"] == "terminal.closed", "the terminal helper closes on release")

notify("session/cancel", {"sessionId": oid})
time.sleep(0.5)
r, e = req("session/close", {"sessionId": oid})
check(e is None, "session/close")
time.sleep(2)
methods = [m["method"] for m in notes]
updates = json.dumps([m["params"] for m in notes if m["method"] == "session/update"])
check('"working"' in updates, "status changes arrive as session/update from rex events")
check(any(m.endswith("session/ended") for m in methods), "closing a session reports it ended")

p.stdin.close()
p.terminate()
print("%d failed" % len(failures) if failures else "all passed")
sys.exit(1 if failures else 0)
