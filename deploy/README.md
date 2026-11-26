# Deploying Fleetlayer

The contract between Fleetlayer and whoever hosts it. Change this file in the same commit as any change to ports,
paths or environment variables.

Two parts:
- **The control plane:** one container, one replica, a persistent volume. It holds no credentials of the user's.
- **A node for each machine, or each Unix user on a shared machine:** a user service (systemd user unit, or a launchd
  agent on macOS), with its plugins beside it.

## The control plane

### Image

- `deploy/Dockerfile`: the `fleetlayer-control` binary on distroless `nonroot` (uid and gid 65532). Entrypoint
  `["/usr/local/bin/fleetlayer-control", "serve"]`.
- `scripts/build-images <commit>` builds it on the build host and pushes `<registry>/fleetlayer/control:<short sha>`,
  plus `<registry>/fleetlayer/node-bin:<short sha>` (a `FROM scratch` image holding the Linux `fleetlayer`,
  `fleetlayer-node`, `fleetlayer-herdr` and `fleetlayer-claude-code` binaries, to copy out). The registry comes from
  `.registry` at the repo root (gitignored) or `$FLEETLAYER_REGISTRY`.
- Root filesystem can be read-only. It writes only to `/data`.

### Ports

| Port | Serves | Exposure |
|---|---|---|
| 7600 | `GET /healthz`, the node links (`GET /v1/link`, a WebSocket), the API (`GET /v1/fleet`, `/v1/events`, `/v1/subscriptions`, `/v1/endpoints`, `/v1/summaries`; `POST /v1/sessions/<id>/catchup`) | Private network only, through an HTTPS ingress that passes WebSocket upgrades through unbuffered, with long idle timeouts (the link pings every 30 s) |
| 7601 | `GET /healthz` and `POST /hooks/<endpoint id>/<secret>`, nothing else | Public, through a tunnel whose ingress sends only `^/hooks/` here and answers everything else with 404 |

Health: `GET /healthz` on 7600 answers `{"ok":true,"linkedNodes":N}` once the database answers, 503 otherwise. Use it
for liveness and readiness, and for an uptime check.

### Environment

| Variable | Default in the image | Meaning |
|---|---|---|
| `FLEETLAYER_CONTROL_DB` | `/data/control.db` | SQLite, WAL mode; migrations run at start-up |
| `FLEETLAYER_CONTROL_LISTEN` | `0.0.0.0:7600` | |
| `FLEETLAYER_CONTROL_HOOKS_LISTEN` | `0.0.0.0:7601` | |
| `FLEETLAYER_CONTROL_ADMIN_TOKEN_FILE` | unset | A file holding the admin token (mount a secret). The API also takes tokens made with `token add` |
| `FLEETLAYER_LOG` | `info` | tracing filter |

### Admin commands

Run inside the container (`kubectl exec … -- fleetlayer-control <command>`), against the same database:

- `node add <name>`: enrol a node; prints its token once. `node list`, `node remove <name>`.
- `endpoint add --label <l> --channel <c>`: a webhook endpoint; prints `/hooks/<id>/<secret>` once. `endpoint list`.
- `token add <name> [--scope read|admin]`: an API token for a program that reads the fleet (`read`: every GET and
  catch-ups); prints it once. `token list`, `token remove <name>`.
- `acl list|add|remove|default|check`: access control between sessions.
- `subscribe <channel> <tab>@<node> [--subject s]`, `unsubscribe …`, `fallback <channel> <tab>@<node>|--none`.

### Backups

Take an online copy with `sqlite3 /data/control.db ".backup '<target>'"` or `VACUUM INTO`, never a file copy. The
database holds node token hashes, webhook secret hashes, subscriptions, the last session lists and the event log; no
user credentials.

## Nodes

- Binaries on `PATH`: `fleetlayer`, `fleetlayer-node`, and the plugins it runs (`fleetlayer-herdr`,
  `fleetlayer-claude-code`).
- Config `~/.config/fleetlayer/node.yaml` (see `docs/getting-started.md`), the node token in a 0600 file it names.
- `fleetlayer-node install` writes and starts the user service (`loginctl enable-linger` on Linux so it runs logged
  out). On macOS it must be a launchd agent in the user's GUI session, never a daemon.
- Local socket: `$XDG_RUNTIME_DIR/fleetlayer/node.sock` (Linux) or `~/.local/state/fleetlayer/node.sock`.
- Monitoring: the `heartbeat:` section pushes `success=true|false&error=…` every 60 s (Gatus external-endpoint style).
  Alert on `success=false` and on a missing heartbeat.
