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
  pages        http://pages.localhost:7680
Hub login key: ~/.local/state/fleetlayer/claude-hub-login
Pages login key: ~/.local/state/fleetlayer/pages/login  Pages go in ~/Fleetlayer/personal/pages/
```

Open the status page. It shows each service, whether it's answering, and where alerts go. The hub and pages ask for
their login key the first time (the file's contents); Fleetlayer's own sign-in replaces the keys when it ships.

Each piece runs as a user service (a launchd agent on macOS, a systemd user service on Linux), so it starts when you
log in and restarts if it crashes. On Linux, `loginctl enable-linger "$USER"` keeps them running while you're logged
out. `fleetlayer down` stops them all and keeps every file; `fleetlayer up` again starts them where they left off.

Useful flags: `--alerts-to planning` (a session that hears when something goes down), `--no-hub`, `--no-pages`,
`--port 7680`.

## What's running

| Piece | Address | Its files |
|---|---|---|
| Gateway and status page | `http://fleetlayer.localhost:7680` | `~/.config/fleetlayer/gateway.yaml`, `~/.local/state/fleetlayer/gateway/` |
| Control plane | `http://control.localhost:7680` | `~/.local/state/fleetlayer/control/` |
| Node | (no web page: `fleetlayer status`) | `~/.config/fleetlayer/node.yaml`, `node-key`, `node-token` |
| Claude sign-in hub | `http://claude-hub.localhost:7680` | `~/.local/state/fleetlayer/claude-hub/` |
| Pages and apps | `http://pages.localhost:7680`, apps at `<app>--<space>.apps.localhost:7680` | `~/Fleetlayer/personal/` |

Everything listens on this machine's own addresses only (`127.0.0.1`, and `[::1]` for the gateway, since macOS
answers `*.localhost` with `::1` first): nothing is reachable from your network. The hub and pages pick a free port each
time they start and tell the node where they are; the gateway follows them. `fleetlayer services` lists every route.

**Plain HTTP, on purpose.** Browsers send `*.localhost` to your own machine and treat it as a secure context, so
sign-ins and passkeys work without certificates. TLS belongs on a server, in front of whatever you move there.

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
`fleetlayer status` says when it keeps failing.

## Moving a piece to a server

Each piece keeps its state in one folder and its settings in one file, so a move is: stop it, copy the folder, start it
on the other machine, point the laptop at its new address.

- **Pages, the hub, any plugin:** run it on the server's node, then on the laptop either leave it out of node.yaml or
  hide it, and route its name to the new address in gateway.yaml:

  ```yaml
  services:
    pages: { url: "http://10.0.0.5:7670", health: /healthz }
  ```

  `http://pages.localhost:7680` keeps working on the laptop, now served from the server. On the server, its own proxy
  serves it at its real name (below).
- **The control plane:** `fleetlayer down`, copy `~/.local/state/fleetlayer/control/` to the server, start
  `fleetlayer-control serve` there behind TLS, then set `control.url` in each node's node.yaml to `wss://…/v1/link`.
  Nodes keep their keys and tokens.
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
file current as plugins restart on new ports, for Traefik's file watcher. On a server, give plugins fixed ports
(`listen: 127.0.0.1:7670`) if your proxy runs elsewhere. `emit gatus` writes an endpoint for each health check and an
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

</details>

## Not in this guide

TLS on the laptop, file sync between machines, and backups: for backups, see [Backups for a fleet of
agents](backups.md).
