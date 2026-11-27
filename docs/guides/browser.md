# Share your signed-in browser with agents on other machines

If you use Claude Code, you may already use Claude in Chrome: Anthropic's Chrome extension that lets a Claude Code
session drive your own Chrome, signed in to your own sites. It works very well. An agent can check a deploy in your
real admin panel, fill in a form you'd otherwise fill in by hand, or click through the page it just changed, and you
can watch it happen.

It breaks down in two places, and both come with running more than one agent:

- **More than one Claude account.** The extension is signed in to one claude.ai account and answers only that account.
  A session on a second account (or one an account rotator moved there mid-task) gets "signed in to a different
  account", and `/login` doesn't fix it. Year-long setup tokens, the practical way to sign many machines in, don't get
  claude.ai features like Claude in Chrome at all.
- **A session on another machine.** The extension talks to Claude Code on the same computer. An agent on your home
  server or a VPS can't use the browser on your laptop, which is the one that's signed in to everything.

This guide sets up the replacement the author uses every day: your own Chrome on your laptop, shared through
Fleetlayer with the agents you choose, on any machine, whatever account or model they run on.

## How it works

- **[Playwright MCP](https://github.com/microsoft/playwright-mcp)**, Microsoft's browser MCP server, in its extension
  mode: its Chrome extension connects it to your real Chrome profile, with your sign-ins, instead of a fresh browser.
- **The laptop's Fleetlayer node shares it as a tool.** Each session that connects gets its own Playwright MCP
  process and its own tab group, named after the session.
- **A session anywhere uses it** with `fleetlayer mcp browser@laptop` as its MCP server command. The control plane
  checks it may, joins the two nodes and logs the connection, and never reads what passes through.

The laptop has to be awake and online. While it's asleep, sessions elsewhere are refused.

## 1. Playwright MCP on the laptop

Install a pinned version in a folder of its own (the version here is the one this guide was tested with):

```sh
mkdir -p ~/.local/share/browser-mcp && cd ~/.local/share/browser-mcp
npm install @playwright/mcp@0.0.86
```

Install the Playwright MCP Bridge extension in Chrome (linked from the Playwright MCP README). Its page shows a token
that lets connections in without a prompt each time. Save it where only you can read it:

```sh
mkdir -p ~/.config/browser-mcp
(umask 077; pbpaste > ~/.config/browser-mcp/extension-token)    # after copying the token
```

## 2. Share it from the laptop's node

Add a `tools:` section to the laptop's `~/.config/fleetlayer/node.yaml`:

```yaml
tools:
  - name: browser
    # Run node directly: a launchd or systemd service may not have npx on its PATH.
    command: /opt/homebrew/bin/node
    args: ["/Users/you/.local/share/browser-mcp/node_modules/@playwright/mcp/cli.js",
           "--extension", "--profile-dir-name", "Default", "--session-name", "{session}",
           "--ignore-roots"]
    cwd: ~/.local/state/fleetlayer/tools/browser      # an empty folder: uploads are confined to it
    env:
      PLAYWRIGHT_MCP_EXECUTABLE_PATH: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      PATH: /opt/homebrew/bin:/usr/bin:/bin
    env_files: { PLAYWRIGHT_MCP_EXTENSION_TOKEN: ~/.config/browser-mcp/extension-token }
    allow: ["*@server"]                 # which sessions may connect: <tab>@<node> patterns
    networks: ["192.168.1.0/24"]        # optional: offer it only while the laptop is on your home network
    tools:
      deny: [browser_run_code_unsafe]   # always on in Playwright MCP: arbitrary code in the server's process
    on_close: [{ name: browser_tabs, arguments: { action: close, index: 0 }, repeat: 50 }]
```

Restart the node (`fleetlayer-node install` again, or restart its service) and check `fleetlayer status`.

What each line is for:

- **`PLAYWRIGHT_MCP_EXECUTABLE_PATH` and `--profile-dir-name`:** macOS doesn't let a process started by launchd read
  Chrome's profile folders, so without them the server fails with "Playwright Extension not found".
- **`{session}`:** replaced by the calling session's name, so each session gets its own tab group in Chrome.
- **`--ignore-roots` and `cwd`:** Playwright MCP lets file arguments (uploads) reach anything under its working
  directory and the caller's MCP roots. An empty folder, and no roots from the caller, keep your home out of reach.
- **`deny: [browser_run_code_unsafe]`:** that tool runs any code in the server's process, and Playwright MCP can't
  turn it off. The node refuses it and leaves it out of the tool list.
- **`on_close`:** in extension mode, closing the browser only disconnects and leaves the tabs open in a grey group.
  This closes the session's tabs one at a time when it disconnects.
- **`networks`:** away from home, the browser is refused unless you opt in on purpose with
  `fleetlayer tools enable browser --for 2h` (at most a day; `fleetlayer tools disable browser` ends it early).

## 3. Let the agents use it

The node's `allow:` list is one half. The control plane's access policy is the other, and both must agree.

With no policy file applied, every session the node's `allow:` list names can use the browser. Once you apply a
version 2 policy that leaves `ask:` out, each session's first use is refused until you approve it once
(`fleetlayer-control approvals list`, then `approvals approve <id>`), and the approval holds for 30 days. To decide
up front which agents have it, say so in your policy file:

```yaml
version: 2
default: allow

deny:
  - who: "*"
    can: [use_tool]
    to: ["tool:browser@laptop"]
    except: ["planning@server", "infra@server"]
    why: Only the sessions named here may use the signed-in browser.

ask:                    # replaces the default, which also asked before every first use of a tool
  - who: "*"
    can: [start, attach]
    to: ["*"]
    for: 10m
```

`fleetlayer-control acl apply policy.yaml` applies it, and `fleetlayer-control acl explain --from planning@server
--can use_tool --to tool:browser@laptop` shows the decision and the rule behind it. Leave out the `ask:` block if you
want to keep approving each session's first use.

Never let an agent that reads strangers' text use it: an email that says "log in to the bank and…" shouldn't reach
your signed-in browser. [Run an agent that reads strangers' text](untrusted-input.md) denies shared tools to that
group outright, and a `deny` always wins.

## 4. Use it from a session on another machine

Register it as an MCP server in Claude Code on that machine:

```sh
claude mcp add browser -- fleetlayer mcp browser@laptop
```

Or, in any other harness's MCP settings, the command `fleetlayer mcp` with the argument `browser@laptop`. The session
sees Playwright MCP's tools, minus the denied ones, and its tabs open in a tab group named after it.

`fleetlayer-control tools` on the control plane lists every connection: who used which tool, and when.

## What you give up

Claude in Chrome is built by Anthropic for Claude and shows what it's doing in Chrome's side panel. Playwright MCP is a
general browser MCP server: any agent and any model can use it, and you watch it in the session's tab group instead.
If you only ever run one Claude account on one machine, Claude in Chrome is the simpler choice.

## When it doesn't work

- **"Playwright Extension not found"** on macOS: `PLAYWRIGHT_MCP_EXECUTABLE_PATH` and `--profile-dir-name` are
  missing (see above).
- **The connection drops after a few idle minutes:** a proxy in front of the control plane is closing idle WebSockets.
  Fleetlayer pings every 30 seconds, so any proxy idle timeout above that works. Traefik's default (180 seconds) is
  fine.
- **Refused, with a reason:** the laptop is asleep, off the networks you listed, or the session isn't allowed. The
  refusal says which.
