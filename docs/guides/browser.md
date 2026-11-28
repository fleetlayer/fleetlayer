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
      PLAYWRIGHT_MCP_EXECUTABLE_PATH: ~/.local/state/fleetlayer/open-chrome   # the node writes it (macOS)
      PATH: /opt/homebrew/bin:/usr/bin:/bin
    env_files: { PLAYWRIGHT_MCP_EXTENSION_TOKEN: ~/.config/browser-mcp/extension-token }
    allow: ["*@server"]                 # which sessions may connect: <tab>@<node> patterns
    networks: ["192.168.1.0/24"]        # optional: offer it only while the laptop is on your home network
    tools:
      deny: [browser_run_code_unsafe]   # always on in Playwright MCP: arbitrary code in the server's process
    on_close: [{ name: browser_tabs, arguments: { action: close, index: 0 }, repeat: 50 }]
    close_with: [browser_close]   # browser_close only disconnects in extension mode, so it runs the cleanup instead
```

Restart the node (`fleetlayer-node install` again, or restart its service) and check `fleetlayer status`.

What each line is for:

- **`PLAYWRIGHT_MCP_EXECUTABLE_PATH` and `--profile-dir-name`:** macOS doesn't let a process started by launchd read
  Chrome's profile folders, so without them the server fails with "Playwright Extension not found". Playwright MCP
  opens its connect page by starting Chrome's binary, and macOS counts that Chrome as the node's: it shows "would like
  to access data from other apps" at every connection. Pointed at `~/.local/state/fleetlayer/open-chrome` (the node
  writes it the first time), the page goes to the Chrome that's running through `open -g -b com.google.Chrome`
  instead, and macOS asks nothing. It drops Chrome's other arguments, so the page opens in the profile you used last.
  To use another profile, keep Chrome open in it, or set Chrome's own binary
  (`/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`) and accept the question at each connection.
- **`{session}`:** replaced by the calling session's name, so each session gets its own tab group in Chrome.
- **`--ignore-roots` and `cwd`:** Playwright MCP lets file arguments (uploads) reach anything under its working
  directory and the caller's MCP roots. An empty folder, and no roots from the caller, keep your home out of reach.
- **`deny: [browser_run_code_unsafe]`:** that tool runs any code in the server's process, and Playwright MCP can't
  turn it off. The node refuses it and leaves it out of the tool list.
- **`on_close`:** in extension mode, closing the browser only disconnects and leaves the tabs open in a grey group.
  This closes the session's tabs one at a time when it disconnects, and when the node itself stops (an upgrade, a
  restart), before it stops the server. It runs only for a session that made a tool call: Playwright MCP
  connects to Chrome, and opens a tab, at its first call of any kind, so a cleanup call for a session that never used
  the browser would open the very tab it meant to close.
- **Nothing runs until it's used:** a session connects at launch whether or not it will use the browser. The
  node answers a new connection from what Playwright MCP answered last time (kept in
  `~/.local/state/fleetlayer/tools/browser.mcp-cache.json`) and starts the process only at the session's first browser
  call, so twenty idle sessions cost nothing on the laptop. When Playwright MCP is upgraded, the first call after it
  notices the new tools, rewrites that file and tells the session. `lazy: false` starts it at every connection
  instead.
- **`networks`:** away from home, the browser is refused unless you opt in on purpose with
  `fleetlayer tools enable browser --for 2h` (at most a day; `fleetlayer tools disable browser` ends it early).

## 3. Let the agents use it

The node's `allow:` list is one half. The control plane's access policy is the other, and both must agree.

A fleet with no policy of its own uses a built-in default: each session's first use of the browser is
refused until you approve it once (`fleetlayer-control approvals list`, then `approvals approve <id>`), and the
approval holds for 30 days, renewed each time the browser is used. The default `ask:` rules are listed in
[access control](../access-control.md#asking-the-person-approval-and-justification). A policy you apply that leaves
`ask:` out asks the same way. To decide up front which agents have it, say so in your policy file:

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
your signed-in browser. [Run an agent that reads strangers' text](untrusted-input.md) denies shared tools and shared
commands to that group outright, and a `deny` always wins.

## 4. Use it from a session on another machine

Register it as an MCP server in Claude Code on that machine:

```sh
claude mcp add browser -- fleetlayer mcp browser@laptop
```

Or, in any other harness's MCP settings, the command `fleetlayer mcp` with the argument `browser@laptop`. The session
sees Playwright MCP's tools, minus the denied ones, and its tabs open in a tab group named after it.

A call that gets no answer within 120 seconds is answered with an error ("browser@laptop didn't answer within 120s (the
machine may be asleep, or its node restarting)"), and the next call works as usual; a stream that has heard nothing for
90 seconds closes. For slow work (a page that takes minutes to load), raise the limit after the target:
`claude mcp add browser -- fleetlayer mcp browser@laptop --call-timeout 300`.

**When the laptop sleeps or its node restarts**, a session keeps the browser's tools: the call in flight
gets an error at once, later calls get "browser@laptop can't be reached now" until the laptop is back, and the first
call after that reconnects by itself. Claude Code never restarts an MCP server that exited, so `fleetlayer mcp` stays
running while the laptop is away instead of exiting. It keeps the tool's list (in `~/.local/state/fleetlayer/mcp/`)
so a session that starts while the laptop sleeps still loads it. An MCP server you add with `claude mcp add` reaches only
sessions that start after it: restart the ones already running.

`fleetlayer-control tools` on the control plane lists every connection: who used which tool, and when.

The same mechanism shares other things on the laptop: [your screenshots and downloads](shared-folders.md), for example.

## Leftover tabs

Each session's tabs live in a tab group named after it, and close when the session calls `browser_close`, when its
connection ends, or when the laptop's node stops. A grey group is one whose session left without closing it: the
extension greys a group when its connection ends and never closes tabs itself. You can close grey groups by
hand (right-click the group, **Close group**); no session can reach them any more.

The laptop's node publishes a `tool.connections` reading for each shared tool: open connections, how many have used
the tool, and `unfinishedCleanups`, the connections whose cleanup didn't finish (the server didn't answer in time) and
may have left a tab behind. `fleetlayer readings tool.connections` shows it from any machine allowed to read it; alert
on `unfinishedCleanups` going up.

## What you give up

Claude in Chrome is built by Anthropic for Claude and shows what it's doing in Chrome's side panel. Playwright MCP is a
general browser MCP server: any agent and any model can use it, and you watch it in the session's tab group instead.
If you only ever run one Claude account on one machine, Claude in Chrome is the simpler choice.

## When it doesn't work

- **"Playwright Extension not found"** on macOS: `PLAYWRIGHT_MCP_EXECUTABLE_PATH` and `--profile-dir-name` are
  missing (see above).
- **macOS asks "would like to access data from other apps" at every connection:** `PLAYWRIGHT_MCP_EXECUTABLE_PATH`
  names Chrome's own binary. Point it at `~/.local/state/fleetlayer/open-chrome` (see above).
- **The connection drops after a few idle minutes:** a proxy in front of the control plane is closing idle WebSockets.
  Fleetlayer pings every 30 seconds, so any proxy idle timeout above that works. Traefik's default (180 seconds) is
  fine.
- **Refused, with a reason:** the laptop is asleep, off the networks you listed, or the session isn't allowed. The
  refusal says which.
