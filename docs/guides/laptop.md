# Start on your laptop

Everything Fleetlayer runs can start on one Mac or Linux laptop: the control plane, a node, the Claude sign-in hub,
pages, and a small gateway that gives each of them its own address. When you outgrow the laptop, you move one piece at
a time to a home server or a VPS, and the rest keeps working.

## One command

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
fleetlayer up
```

`fleetlayer up` prints where everything is:

```
Fleetlayer is running. Status: http://fleetlayer.localhost:7680
  claude-hub   http://claude-hub.localhost:7680
  control      http://control.localhost:7680
  dashboard    http://dashboard.localhost:7680
  pages        http://pages.localhost:7680
Hub login key: ~/.local/state/fleetlayer/claude-hub-login
Pages login key: ~/.local/state/fleetlayer/pages/login  Pages go in ~/Fleetlayer/personal/pages/
Dashboard login key: ~/.local/state/fleetlayer/dashboard/login
```

It also adds your sessions to the fleet: the herdr plugin when [herdr](https://herdr.dev) is installed, the
[Rex](../plugins/rex.md) plugin when Rex is, the [tmux](../plugins/tmux.md) plugin when tmux is, and the Claude Code
plugin when Claude Code is, a [harness plugin](../harnesses.md) for each of pi, OpenCode, Gemini CLI and Cursor's CLI
it finds, and says which it added or skipped and why. Then `fleetlayer sessions` lists them, other
machines can message them, and `fleetlayer session start` opens new ones. Session summaries stay off, since
each one runs Claude Code and spends tokens: `fleetlayer up --summaries` turns them on.

Open the status page. It shows each service, whether it's answering, and where alerts go. The hub, pages and the
[dashboard](../dashboard.md) ask for their login key the first time (the file's contents), or put them behind
[Fleetlayer sign-in](sign-in.md) instead.

Each piece runs as a user service (a launchd agent on macOS, a systemd user service on Linux), so it starts when you
log in and restarts if it crashes. On Linux, `loginctl enable-linger "$USER"` keeps them running while you're logged
out. `fleetlayer down` stops them all and keeps every file; `fleetlayer up` again starts them where they left off.

Useful flags: `--alerts-to planning` (a session that hears when something goes down; on a later `up` it doesn't
change an existing gateway.yaml, so set `alerts.session` there), `--no-hub`, `--no-pages`, `--no-dashboard`,
`--port 7680`, `--summaries`.

## What's running

| Piece | Address | Its files |
|---|---|---|
| Gateway and status page | `http://fleetlayer.localhost:7680` | `~/.config/fleetlayer/gateway.yaml`, `~/.local/state/fleetlayer/gateway/` |
| Control plane | `http://control.localhost:7680` | `~/.config/fleetlayer/control.yaml`, `~/.local/state/fleetlayer/control/` |
| Node | (no web page: `fleetlayer status`) | `~/.config/fleetlayer/node.yaml`, `node-key`, `node-token` |
| Claude sign-in hub | `http://claude-hub.localhost:7680` | `~/.local/state/fleetlayer/claude-hub/` |
| Pages and apps | `http://pages.localhost:7680`, apps at `<app>--<space>.apps.localhost:7680` | `~/Fleetlayer/personal/` |
| Dashboard | `http://dashboard.localhost:7680` | its login key and control plane token in `~/.local/state/fleetlayer/dashboard/` |

Everything listens on this machine's own addresses only (`127.0.0.1`, and `[::1]` for the gateway, since macOS
answers `*.localhost` with `::1` first): nothing is reachable from your network. The hub and pages pick a free port each
time they start and tell the node where they are; the gateway follows them. `fleetlayer services` lists every route.

**Plain HTTP, on purpose.** Browsers send `*.localhost` to your own machine and treat it as a secure context, so
sign-ins and passkeys work without certificates. TLS belongs on a server, in front of whatever you move there.

## When an agent asks first

A new fleet asks you before an agent does something sensitive:

- starting, attaching to, answering or closing a session on another machine (an approval holds 10 minutes);
- each slash command typed into a session, and each run of a shared command on another machine (every time, with the
  command in front of you);
- each agent's first use of a shared tool, a sync, an admin action, a write or a connection to a target (it holds 30
  days; for a tool, a sync or a connection, 30 days from its last use);
- each read of your signed-in browser for cookies (every time).

Sessions on the same machine aren't checked. The agent's request is refused with a request ID until you answer:

```sh
fleetlayer-control approvals list            # pending requests: id, who, what, to what
fleetlayer-control approvals approve <id>    # or: approvals deny <id> (it stands for an hour)
```

Then the agent tries again. `--for 1h` on `approve` changes how long it holds.

If your agents work on their own and you'd rather not be asked, apply a policy that turns asking off, except before
each cookie read, which [Access control](../access-control.md) says to keep:

```yaml
# policy.yaml
version: 2
default: allow
ask:
  - who: "*"
    can: [sync]
    to: ["sync:cookies-from-browser/*"]
    for: once
```

```sh
fleetlayer-control acl apply policy.yaml
```

`fleetlayer-control acl show` prints the policy in force. Who may do what, and how to ask before only some things,
is in [Access control](../access-control.md).

## When something stops

The gateway checks each service's health every 30 seconds and asks the node how it is (its plugins, its link to the
control plane). When something goes down or comes back, you get a desktop notification and, with `--alerts-to` (or
`alerts.session` in gateway.yaml), a Fleetlayer message to that session, so an agent can look into it. While the node
itself is down, the messages wait and go out when it's back. A crash that the service manager or the node fixes within
a few seconds isn't an alert.

The gateway also takes heartbeats the way [Gatus](https://gatus.io) does on a server (`POST
/api/v1/endpoints/<key>/external?success=true|false&error=…`), so a scheduled job (a backup, a sync) reports to the
laptop and to a server with the same config, only a different address. Name each one in gateway.yaml, and a heartbeat
that stops arriving is an alert too:

```yaml
heartbeats:
  backup: { every: 3600, token_file: ~/.config/fleetlayer/backup-heartbeat-token }
```

Who watches the gateway: launchd or systemd restart it, and the node's health includes the gateway's `/healthz`, so
`fleetlayer status` prints `unhealthy: …` when it keeps failing (`health` in `fleetlayer status --json`).

## Moving a piece to a server

Each piece keeps its state in one folder and its settings in one file, so any of them can move to an always-on
machine with one command, keeping its identity, keys and data:

```sh
fleetlayer move pages server --dry-run   # the plan
fleetlayer move pages server             # control, node, gateway, hub, pages, sign-in, or any plugin
```

It stops the piece here, copies it sealed and checked, starts it there, and points this machine at its new address: the
gateway routes `pages` there, so `http://pages.localhost:7680` keeps working here. [Moving a piece](moving.md) has the
details: what each piece carries, addresses, the control plane's nodes following it, and what happens if it fails.

- **Joining a fleet that already has a control plane:** on the server, `fleetlayer-control node add <laptop name>`
  prints a token; on the laptop, save it to a file and run
  `fleetlayer up --join wss://control.example/v1/link --token-file <file>`. No control plane starts on the laptop.

## On a server: Traefik and Gatus from the same list

On a server you already run a proxy and a monitor. The gateway writes their config from the same routes:

```sh
fleetlayer-gateway emit traefik --domain home.example --entrypoint websecure --cert-resolver le \
  --middleware auth@file --out /etc/traefik/dynamic/fleetlayer.yml --watch
fleetlayer-gateway emit gatus --alert matrix > /etc/gatus/fleetlayer.yaml
```

`emit traefik` writes a router and service for each route (and a `HostRegexp` for pages' apps); `--watch` keeps the
file current as plugins restart on new ports, for Traefik's file watcher. On a server, give plugins fixed ports:
`listen: 127.0.0.1:7670` when the proxy runs on the same machine, the machine's LAN or tailnet address when it runs
elsewhere. `emit gatus` writes an endpoint for each health check and an
external endpoint for each heartbeat, with `${FLEETLAYER_HEARTBEAT_<NAME>_TOKEN}` for Gatus to fill in from its
environment.

## Advanced

<details>
<summary>gateway.yaml, ports and browsers</summary>

```yaml
domain: localhost              # services are <name>.<domain>
listen: 127.0.0.1:7680         # and [::1]:7680 too; 0.0.0.0 exposes every service to your network, without TLS
services:                      # services that aren't plugins, or that moved to another machine
  control: { url: "http://127.0.0.1:7600", health: /healthz }
  # claude-hub: { hidden: true }   # no route and no check for a service a plugin announces
alerts:
  session: planning            # tab, or tab@node
  desktop: true
heartbeats: {}
node: true                     # follow the services the node's plugins announce
```

- **Port 80**, so addresses have no port: macOS lets any user bind it; on Linux it needs
  `sysctl net.ipv4.ip_unprivileged_port_start=80`. Set `listen: 127.0.0.1:80`, and pages' `base_url` to match.
- **`*.localhost` outside the browser.** Browsers resolve it themselves; other programs ask the system, and not every
  system answers (Debian 13 does; Ubuntu 22.04 and its curl 7.81 don't). There, use
  `curl --connect-to ::127.0.0.1:7680 http://pages.localhost:7680/`, and everywhere, have services talk to each other
  by their backend address, not their `.localhost` one.
- **Cookies on plain HTTP:** browsers refuse `Secure` cookies on subdomains of localhost, so pages and sign-in drop
  `Secure` when their address is `http://…localhost`, and allow plain HTTP only there.
- **A second copy beside the first** (to try a new version, or test without touching the fleet you use): give it its
  own name and folder. Its services get their own names (`dev.fleetlayer.try.node`, `fleetlayer-try-node`) and
  its node its own socket, and the first copy keeps running untouched:

  ```sh
  export FLEETLAYER_INSTANCE=try HOME=~/fleetlayer-try   # in this shell only
  mkdir -p "$HOME" && fleetlayer up     # ports 7690 and 7700 for a second copy; --port, --control-port for a third
  fleetlayer status; fleetlayer down                     # the same variables pick this copy
  ```

</details>

## Not in this guide

TLS on the laptop, file sync between machines, and backups: for backups, see [Backups for a fleet of
agents](backups.md).
