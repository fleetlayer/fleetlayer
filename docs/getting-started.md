# Getting started

Fleetlayer is early. Expect breaking changes.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
```

It installs the latest release's binaries for Linux or macOS into `~/.local/lib/fleetlayer/<version>/`, checked
against the release's checksums, with symlinks in `~/.local/bin`. A node installed this way updates itself from later
releases (`update: {mode: auto | notify | off}` in node.yaml); one installed any other way only logs that a release is
out.

Or build from source: `cargo build --release`, and put `target/release/fleetlayer*` on your `PATH`.

## A node on one machine

`~/.config/fleetlayer/node.yaml`:

```yaml
name: laptop
plugins:
  - name: herdr
    command: fleetlayer-herdr
  - name: claude-code
    command: fleetlayer-claude-code
```

```sh
fleetlayer-node install      # a systemd user service on Linux, a launchd agent on macOS
fleetlayer status
fleetlayer sessions
fleetlayer msg send --to planning "the migration is done"
```

From inside an agent session, `fleetlayer msg send` names the sending session, so the receiver knows who wrote and how
to reply.

Claude Code sessions running in bypass-permissions mode hold incoming messages for your approval unless
`~/.claude/settings.json` has `"crossSessionInbound": "accept"`. That setting also accepts messages from any process of
your user that can read the session's key file; decide whether that's right for your machine.

## A control plane, for several machines

```sh
fleetlayer-control --db /var/lib/fleetlayer/control.db node add laptop    # prints the node's token once
fleetlayer-control --db /var/lib/fleetlayer/control.db node add server
fleetlayer-control --db /var/lib/fleetlayer/control.db serve --listen 0.0.0.0:7600 --hooks-listen 127.0.0.1:7601
```

On each node, save its token to a file (mode 0600) and add:

```yaml
control:
  url: wss://control.example.com/v1/link     # ws:// only on a private network
  token_file: ~/.config/fleetlayer/node-token
```

Then `fleetlayer fleet` shows every machine and session, and `fleetlayer msg send --to planning@server "…"` reaches a
session on another machine.

## Webhooks into the session that owns the work

```sh
fleetlayer-control endpoint add --label github --channel ci     # prints /hooks/<id>/<secret>
```

Expose only the hooks listener to the internet (a Cloudflare Tunnel to port 7601, say) and give GitHub the URL. Then,
from the session working on a branch:

```sh
fleetlayer subscribe ci --subject your-org/your-repo@your-branch
```

CI results for that branch arrive in that session. `fleetlayer-control fallback ci <tab>@<node>` names where events go
when nobody subscribes.

## Using a tool on another machine

A node can share a stdio tool, for example an MCP server driving your own signed-in browser on your laptop, with
sessions on other machines. On the laptop's node:

```yaml
tools:
  - name: browser
    # A pinned install (npm install @playwright/mcp@<version> in a folder of its own), run with node directly: a
    # service may not have npx on its PATH.
    command: /opt/homebrew/bin/node
    args: ["/Users/you/.local/share/browser-mcp/node_modules/@playwright/mcp/cli.js",
           "--extension", "--profile-dir-name", "Default", "--session-name", "{session}"]
    env:
      PLAYWRIGHT_MCP_EXECUTABLE_PATH: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      PATH: /opt/homebrew/bin:/usr/bin:/bin
    env_files: { PLAYWRIGHT_MCP_EXTENSION_TOKEN: ~/.config/browser-mcp/extension-token }   # read at start; keep it 0600
    allow: ["*@server"]         # <tab>@<node> patterns; nobody when empty
    tools:                      # optional: which of the MCP server's own tools callers may use
      deny: [browser_evaluate]  # refused calls get an error reply and never reach the server
```

In a session on another machine, the MCP server command is `fleetlayer mcp browser@laptop`. Each connection starts
its own server, named after the calling session's tab (`{session}`), so each session gets its own tab group. The
control plane checks access (`acl` rules on `tool:<name>@<node>`), joins the two nodes' streams and logs every
connection (`fleetlayer-control tools`). It never reads what passes through.

## Monitoring the node

```yaml
heartbeat:
  url: https://gatus.example.com/api/v1/endpoints/fleetlayer_laptop/external
  token_file: ~/.config/fleetlayer/heartbeat-token
  every: 60
```

The node reports `success=false` with the reason when a plugin isn't running or the control plane link is down, and
stops reporting when it dies, so the monitor alerts either way.
