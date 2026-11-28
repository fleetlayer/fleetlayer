# Getting started

Fleetlayer is early. Expect breaking changes. Before you install, check [what you need](requirements.md) (macOS or
Linux, a session manager such as herdr, Claude Code or Codex), and read [what installing it trusts](trust.md) and how
to verify a release first. To see it working across two machines first, follow the [ten-minute
walkthrough](walkthrough.md).

The docs are written for your agent as much as for you. To have it set Fleetlayer up, ask it to, and point it at
https://fleetlayer.ai/llms.txt (every page, as Markdown). This page is what it does, step by step. A few steps are
yours alone, such as approving a prompt on a Mac or pasting a secret; the guides mark them.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
```

It installs the latest release's binaries for Linux or macOS into `~/.local/lib/fleetlayer/`, checked against the
release's checksums, with symlinks in `~/.local/bin`. A node installed this way updates itself from later
releases (`update: {mode: auto | notify | off}` in node.yaml); one installed any other way only logs that a release is
out.

Or build from source: `cargo build --release`, and put `target/release/fleetlayer*` on your `PATH`.

## Everything on one machine

`fleetlayer up` starts the control plane, a node, the Claude sign-in hub, pages and the gateway on this machine, each at
`http://<name>.localhost:7680`, with a status page at `http://fleetlayer.localhost:7680`. See
[Start on your laptop](guides/laptop.md). The rest of this page sets each piece up by hand.

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
           "--extension", "--profile-dir-name", "Default", "--session-name", "{session}",
           "--ignore-roots"]        # don't let a caller widen file access with its own MCP roots
    # File arguments (uploads) are confined to the server's working directory: give it an empty one, never your home.
    # (Without cwd: a tool gets an empty directory of its own, ~/.local/state/fleetlayer/tools/<name>.)
    cwd: ~/.local/state/fleetlayer/tools/browser
    env:
      PLAYWRIGHT_MCP_EXECUTABLE_PATH: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      PATH: /opt/homebrew/bin:/usr/bin:/bin
    env_files: { PLAYWRIGHT_MCP_EXTENSION_TOKEN: ~/.config/browser-mcp/extension-token }   # read at start; keep it 0600
    description: Your Chrome, signed in to your sites   # optional: one line agents see in `fleetlayer me`
    allow: ["*@server"]          # <tab>@<node> patterns; nobody when empty
    networks: ["10.5.0.0/16"]    # optional: offer it only while this machine is on these networks (at home)
    tools:
      deny: [browser_run_code_unsafe]   # always on in Playwright MCP: arbitrary code in the server's process
    # When a session's connection ends, close its tabs one at a time (browser_close only disconnects in extension
    # mode, leaving the tabs open in a grey group).
    on_close: [{ name: browser_tabs, arguments: { action: close, index: 0 }, repeat: 50 }]
    close_with: [browser_close]   # browser_close only disconnects in extension mode, so it runs the cleanup instead
```

In a session on another machine, the MCP server command is `fleetlayer mcp browser@laptop`. With `networks:` set, the
browser is offered only while the laptop's link to the control plane goes out from one of those networks; away from
home it's refused with the reason, until its person opts in on purpose with `fleetlayer tools enable browser --for
2h` (at most a day; `fleetlayer tools disable browser` ends it early). Each connection starts
its own server, named after the calling session's tab (`{session}`), so each session gets its own tab group
(`{caller}` in `args` is the caller as the control plane recorded it, `<tab>@<node>`, for a tool that decides by who
calls, such as [the leases authority](plugins/leases.md)). The
control plane checks access (`acl` rules on `tool:<name>@<node>`), joins the two nodes' streams and logs every
connection (`fleetlayer-control tools`). It never reads what passes through.

A remote MCP server can be shared the same way, with `url:` (Streamable HTTP) in place of `command:` and its token in a
file only this node's user can read (`bearer_file:`, or `header_files:` for other headers). With `check_calls: true`
each call is decided by the access policy before the server sees it, and calls that change things can wait for your
approval: [the MCP gate](access-control.md#the-mcp-gate), and [Executor behind Fleetlayer](guides/executor.md) for
the whole setup.

A session finds the tools it may use with `fleetlayer me`: each one with its description, whether its
first use still asks, its folders for `fleetlayer-shared-folders`, and the `claude mcp add` line that adds it. Tools a rule
refuses it aren't listed. For agents to know they can, give them the skills and a few lines in their global
instructions: [Tell your agents about Fleetlayer](guides/agents-know-fleetlayer.md).

## Monitoring the node

```yaml
heartbeat:
  url: https://gatus.example.com/api/v1/endpoints/fleetlayer_laptop/external
  token_file: ~/.config/fleetlayer/heartbeat-token
  every: 60
```

The node reports `success=false` with the reason when a plugin isn't running or the control plane link is down, and
stops reporting when it dies, so the monitor alerts either way.
