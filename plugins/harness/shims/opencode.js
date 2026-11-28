// fleetlayer-harness shim for OpenCode (installed by `fleetlayer-harness opencode` as
// ~/.config/opencode/plugins/fleetlayer.js; it rewrites this file when it changes, and takes it out with
// `install: false`). Part of Fleetlayer (FSL-1.1-ALv2).
//
// It lets Fleetlayer's OpenCode plugin reach this OpenCode process: a registration file and a Unix socket in
// ~/.local/state/fleetlayer/harness/opencode/ (0700), named after the process. Over the socket the plugin delivers a
// message into a session through OpenCode's own API (`session.promptAsync`, so the TUI shows it as it shows any
// message) and follows what each session waits on (a permission or a question). Nothing leaves the machine.
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

const PROTOCOL = 1;

export const FleetlayerHarness = async ({ client, directory }) => {
  const dir = path.join(process.env.HOME || os.homedir(), ".local/state/fleetlayer/harness/opencode");
  const base = path.join(dir, String(process.pid));
  /** id -> {id, cwd, title?} for the sessions this process has run or opened. */
  const sessions = new Map();
  /** The session this process works in now (the TUI shows one at a time): the only one it registers. */
  let current = null;
  /** id -> the prompt the session waits on. */
  const prompts = new Map();
  const watchers = new Set();

  // A session named on the command line (`opencode -s <id>`) is this TUI's before it does anything.
  const argv = process.argv;
  for (let i = 0; i < argv.length; i++) {
    const m = argv[i].match(/^--session=(.+)$/);
    const id = m ? m[1] : (argv[i] === "-s" || argv[i] === "--session") && argv[i + 1];
    if (id && id.startsWith("ses")) {
      sessions.set(id, { id, cwd: directory });
      current = id;
    }
  }

  const register = () => {
    try {
      fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
      const reg = {
        protocol: PROTOCOL,
        harness: "opencode",
        pid: process.pid,
        socket: base + ".sock",
        address: process.env.HERDR_PANE_ID || null,
        sessions: current && sessions.has(current) ? [sessions.get(current)] : [],
      };
      fs.writeFileSync(base + ".json.tmp", JSON.stringify(reg), { mode: 0o600 });
      fs.renameSync(base + ".json.tmp", base + ".json");
    } catch {}
  };
  const state = () =>
    JSON.stringify({ type: "state", sessions: current ? [{ id: current, prompt: prompts.get(current) || null }] : [] }) + "\n";
  const tell = () => {
    const line = state();
    for (const w of watchers) w.write(line);
  };
  const cleanup = () => {
    for (const f of [base + ".json", base + ".sock"]) {
      try {
        fs.unlinkSync(f);
      } catch {}
    }
  };

  try {
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    fs.unlinkSync(base + ".sock");
  } catch {}
  const server = net.createServer((c) => {
    let buf = "";
    c.on("error", () => {});
    c.on("close", () => watchers.delete(c));
    c.on("data", async (d) => {
      buf += d;
      if (buf.length > 1 << 20) {
        c.destroy(); // a request is a line of a few kilobytes at most
        return;
      }
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 1);
        let req;
        try {
          req = JSON.parse(line);
        } catch {
          c.end(JSON.stringify({ ok: false, code: "failed", error: "not JSON" }) + "\n");
          return;
        }
        if (req.type === "watch") {
          watchers.add(c);
          c.write(state());
        } else if (req.type === "deliver") {
          // Only into a session this process runs: anything else is for another OpenCode, or for typing in.
          if (!sessions.has(String(req.sessionId))) {
            c.end(JSON.stringify({ ok: false, code: "unknown", error: "this OpenCode isn't running that session" }) + "\n");
            return;
          }
          try {
            const r = await client.session.promptAsync({
              path: { id: String(req.sessionId) },
              body: { parts: [{ type: "text", text: String(req.text) }] },
            });
            if (r && r.error) {
              const gone = r.response && r.response.status === 404;
              c.end(JSON.stringify({ ok: false, code: gone ? "gone" : "failed", error: JSON.stringify(r.error) }) + "\n");
            } else {
              c.end(JSON.stringify({ ok: true }) + "\n");
            }
          } catch (e) {
            c.end(JSON.stringify({ ok: false, code: "failed", error: String(e) }) + "\n");
          }
        } else {
          c.end(JSON.stringify({ ok: false, code: "failed", error: "unknown request" }) + "\n");
        }
      }
    });
  });
  server.on("error", () => {});
  server.listen(base + ".sock", () => {
    try {
      fs.chmodSync(base + ".sock", 0o600);
    } catch {}
  });
  process.on("exit", cleanup);
  register();

  /** A session this process knows; `active` (created, started a turn, asked something) makes it the current one. */
  const seen = (id, info, active) => {
    if (!id || (info && info.parentID)) return; // a subagent's session belongs to its parent's
    if (!active && !sessions.has(id)) return; // another session's update is no reason to list it
    const had = sessions.get(id);
    const next = { id, cwd: (info && info.directory) || (had && had.cwd) || directory, title: (info && info.title) || (had && had.title) };
    const switched = active && current !== id;
    if (switched) current = id;
    if (switched || !had || had.title !== next.title || had.cwd !== next.cwd) {
      sessions.set(id, next);
      register();
      if (switched) tell();
    }
  };
  const clear = (id, match) => {
    const p = prompts.get(id);
    if (p && (!match || p.id === match)) {
      prompts.delete(id);
      tell();
    }
  };

  return {
    event: async ({ event }) => {
      const p = event.properties || {};
      switch (event.type) {
        case "session.created":
          seen(p.info && p.info.id, p.info, true);
          break;
        case "session.updated":
          seen(p.info && p.info.id, p.info, false);
          break;
        case "session.status":
          if (p.status && p.status.type === "busy") seen(p.sessionID, null, true);
          break;
        case "session.deleted":
          if (p.info && sessions.delete(p.info.id)) {
            prompts.delete(p.info.id);
            if (current === p.info.id) current = null;
            register();
            tell();
          }
          break;
        case "permission.asked":
        case "permission.updated":
          seen(p.sessionID, null, true);
          prompts.set(p.sessionID, {
            id: p.id,
            kind: "permission",
            tool: { name: p.permission || p.type || "tool", input: p.metadata || { patterns: p.patterns || p.pattern } },
          });
          tell();
          break;
        case "permission.replied":
          clear(p.sessionID, p.requestID || p.permissionID);
          break;
        case "question.asked":
          seen(p.sessionID, null, true);
          prompts.set(p.sessionID, {
            id: p.id,
            kind: "question",
            questions: (p.questions || []).map((q) => ({
              header: q.header || "",
              question: q.question || "",
              multiSelect: !!q.multiple,
              options: (q.options || []).map((o, n) => ({ n: n + 1, label: o.label || String(o), description: o.description })),
            })),
          });
          tell();
          break;
        case "question.replied":
        case "question.rejected":
          clear(p.sessionID, p.requestID);
          break;
        case "session.idle":
        case "session.error":
          clear(p.sessionID);
          break;
      }
    },
  };
};
