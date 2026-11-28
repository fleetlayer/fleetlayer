# Ten minutes: two machines, two agents, one shared tool

This walkthrough links a laptop and a server, has a Claude Code session on each message the other, and lets the
server's agent read a folder on the laptop. It takes about ten minutes, most of it installing. It's the shortest path
to seeing what Fleetlayer does; the [guides](guides/laptop.md) set up the same things properly.

You need two machines you can SSH between (a laptop and a server, or two laptops), macOS or Linux, with
[herdr](https://herdr.dev) and Claude Code on both ([requirements](requirements.md)). The examples call them `laptop`
and `server`: each node is named after its machine's hostname.

## How it fits

```text
        laptop                                          server
 +-------------------------------+            +-------------------------------+
 |  herdr                        |            |  herdr                        |
 |    tab "docs": Claude Code    |            |    tab "api": Claude Code     |
 |               |               |            |               |               |
 |  fleetlayer-node              |            |  fleetlayer-node              |
 |    plugins: herdr, claude-code|            |    plugins: herdr, claude-code|
 |    tool: shared-folders       |            |                               |
 +---------------|---------------+            +---------------|---------------+
                 |  one outbound WebSocket per node          |
                 v                                            v
          +-------------------------------------------------------+
          |  fleetlayer-control (here, on the laptop)              |
          |  sessions, messages, tool calls, access policy,        |
          |  approvals, audit log; no credentials                  |
          +-------------------------------------------------------+
```

- **A node** runs on each machine (one per Unix user). It attaches to your session manager through a plugin, sees
  every agent session there, and keeps one outbound connection to the control plane, so it needs no open port.
- **Plugins** are separate programs the node starts: one per session manager (herdr here), one per harness (Claude
  Code), and others for shared tools, accounts, pages and the rest.
- **The control plane** knows every node and session, relays messages and tool calls between nodes, and decides each
  one by your access policy, writing every decision to the audit log. It holds no credentials.

Here the control plane runs on the laptop. Later it can move to the server with one command.

## 1. The laptop: everything in one go (3 minutes)

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
# open a new shell, or add ~/.local/bin to your PATH, so the next commands are found
fleetlayer up
fleetlayer-control node add server     # prints the server's token once: copy it
```

`fleetlayer up` starts a control plane, a node, and a status page at `http://fleetlayer.localhost:7680`, all for your
user, as services that start again after a reboot. On a Linux laptop they keep running after you log out only with
lingering on (`loginctl enable-linger "$USER"`, which `fleetlayer up` reminds you of). Running `node add` again
prints a new token, and only the newest works. In herdr, open a tab named `docs` and start `claude` in it.

## 2. A way from the server to the control plane (1 minute)

The control plane listens on the laptop's loopback address only. For this walkthrough, carry it to the server over
SSH, and leave this running:

```sh
ssh -N -R 7610:127.0.0.1:7600 server
```

The server now reaches the laptop's control plane at its own `127.0.0.1:7610`. For anything longer-lived, use a
private network (Tailscale, a LAN) or move the control plane to the server (step 6).

## 3. The server: join (3 minutes)

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
# open a new shell, or add ~/.local/bin to your PATH
loginctl enable-linger "$USER"         # Linux: keep the node running after you log out of SSH
mkdir -p ~/.config/fleetlayer
printf '%s\n' '<the token from step 1>' > ~/.config/fleetlayer/node-token
chmod 600 ~/.config/fleetlayer/node-token
fleetlayer up --join ws://127.0.0.1:7610/v1/link --token-file ~/.config/fleetlayer/node-token --no-hub --no-pages
```

With `--join`, no control plane starts on the server: its node links to the laptop's. In the server's herdr, open a
tab named `api` and start `claude` in it.

On either machine:

```sh
fleetlayer sessions
```

```text
api@server               claude   idle    …
docs@laptop              claude   idle    …
```

## 4. Two agents talking (1 minute)

In the laptop's `docs` session, ask Claude:

> Send a message to api@server asking which port its dev server uses.

It runs `fleetlayer msg send --to api@server "…"`. The message arrives in the server's session through Claude Code's
own messaging channel, labelled with who sent it, and Claude there answers with `fleetlayer msg send --to docs@laptop`.
You can send one yourself from any shell too:

```sh
fleetlayer msg send --to api@server "the staging deploy is done"
```

If a session in bypass-permissions mode holds the message for your approval, see
[getting started](getting-started.md#a-node-on-one-machine).

## 5. A tool that lives on the laptop (2 minutes)

Share a folder on the laptop with the server's agents. Make `~/Share` and put a file in it, then add this to
`~/.config/fleetlayer/node.yaml` on the laptop:

```yaml
tools:
  - name: shared-folders
    command: /Users/you/.local/bin/fleetlayer-shared-folders    # the full path; on Linux, /home/you/...
    args: ["--folder", "share=~/Share"]
    allow: ["*@server"]          # which sessions may connect
```

Restart the laptop's node (`fleetlayer-node install` again), then in the server's `api` session ask Claude:

> Run `claude mcp add laptop-files -- fleetlayer mcp shared-folders@laptop`, restart yourself, and tell me what's in
> the share folder.

The first use waits for your approval, because a fleet with no policy of its own asks before each session first uses
a shared tool. Approve it on the laptop:

```sh
fleetlayer-control approvals list
fleetlayer-control approvals approve <id>
```

The agent on the server reads the laptop's folder. It never had a shell on the laptop, and the approval, the call and
its caller are in the audit log.

## 6. What next

- **Move the control plane to the server**, so it doesn't depend on the laptop being awake:
  `fleetlayer move control server` ([Moving a piece](guides/moving.md)). The nodes follow it by themselves.
- **Write your own access policy** instead of the built-in default: who may message whom, use which tools, and what
  waits for you ([Access control](access-control.md)).
- **Tell your agents about Fleetlayer**, so every session knows it can message others and use shared tools
  ([the guide](guides/agents-know-fleetlayer.md)).
- **Share your signed-in browser** the same way as the folder ([Share your browser](guides/browser.md)).

To undo all of this: [uninstalling](requirements.md#uninstalling).
