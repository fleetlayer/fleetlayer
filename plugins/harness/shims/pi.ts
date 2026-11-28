// fleetlayer-harness shim for pi (installed by `fleetlayer-harness pi` as ~/.pi/agent/extensions/fleetlayer.ts; it
// rewrites this file when it changes, and takes it out with `install: false`). Part of Fleetlayer (FSL-1.1-ALv2).
//
// It lets Fleetlayer's pi plugin reach this pi process: a registration file and a Unix socket in
// ~/.local/state/fleetlayer/harness/pi/ (0700), named after the process. Over the socket the plugin delivers a message
// (`pi.sendUserMessage`, steered into a running turn) and follows what the session waits on (an extension's dialog).
// Nothing leaves the machine, and outside the socket it does nothing.
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";

const PROTOCOL = 1;

export default function (pi: any) {
  const dir = path.join(process.env.HOME || os.homedir(), ".local/state/fleetlayer/harness/pi");
  const base = path.join(dir, String(process.pid));
  let session: { id: string; cwd: string; file?: string; title?: string } | null = null;
  let ctx: any = null;
  let prompt: any = null;
  const watchers = new Set<net.Socket>();
  let server: net.Server | null = null;

  const register = () => {
    try {
      fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
      const reg = {
        protocol: PROTOCOL,
        harness: "pi",
        pid: process.pid,
        socket: base + ".sock",
        address: process.env.HERDR_PANE_ID || null,
        sessions: session ? [session] : [],
      };
      fs.writeFileSync(base + ".json.tmp", JSON.stringify(reg), { mode: 0o600 });
      fs.renameSync(base + ".json.tmp", base + ".json");
    } catch {}
  };
  const state = () => JSON.stringify({ type: "state", sessions: session ? [{ id: session.id, prompt }] : [] }) + "\n";
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

  const listen = () => {
    if (server) return;
    try {
      fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
      fs.unlinkSync(base + ".sock");
    } catch {}
    server = net.createServer((c) => {
      let buf = "";
      c.on("error", () => {});
      c.on("close", () => watchers.delete(c));
      c.on("data", (d) => {
        buf += d;
        let i;
        while ((i = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, i);
          buf = buf.slice(i + 1);
          let req: any;
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
            if (!session || req.sessionId !== session.id) {
              c.end(JSON.stringify({ ok: false, code: "gone", error: "this pi is in another session now" }) + "\n");
              return;
            }
            try {
              const idle = !ctx || ctx.isIdle();
              pi.sendUserMessage(String(req.text), idle ? undefined : { deliverAs: "steer" });
              c.end(JSON.stringify({ ok: true }) + "\n");
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
  };

  pi.on("session_start", async (_event: any, c: any) => {
    ctx = c;
    const sm = c.sessionManager;
    const id = sm.getSessionId?.();
    if (!id) return;
    session = { id, cwd: sm.getCwd?.() || process.cwd(), file: sm.getSessionFile?.() || undefined, title: sm.getSessionName?.() || undefined };
    prompt = null;
    listen();
    register();
    tell();
  });
  pi.on("session_shutdown", async (event: any) => {
    if (event?.reason === "quit") {
      session = null;
      cleanup();
      server?.close();
      server = null;
    }
  });
  for (const e of ["agent_start", "turn_start", "agent_settled"]) pi.on(e, async (_e: any, c: any) => (ctx = c));
  pi.on("ui_prompt_start", async (event: any, c: any) => {
    ctx = c;
    prompt = {
      id: "ui-" + Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 8),
      kind: "question",
      questions: [{ header: "", question: event?.title || "pi is waiting for an answer", options: [] }],
    };
    tell();
  });
  pi.on("ui_prompt_end", async () => {
    prompt = null;
    tell();
  });
}
