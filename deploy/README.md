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
| 7600 | `GET /healthz`, `GET /livez`, the node links (`GET /v1/link`, a WebSocket), the API (`GET /v1/fleet`, `/v1/events`, `/v1/subscriptions`, `/v1/endpoints`, `/v1/summaries`; `POST /v1/sessions/<id>/catchup`) | Private network only, through an HTTPS ingress that passes WebSocket upgrades through unbuffered, with long idle timeouts (the link pings every 30 s) |
| 7601 | `GET /healthz` and `POST /hooks/<endpoint id>/<secret>`, nothing else | Public, through a tunnel whose ingress sends only `^/hooks/` here and answers everything else with 404 |

Health: `GET /healthz` on 7600 answers `{"ok":true,"linkedNodes":N,"degraded":[]}` once the database answers, 503
otherwise. Use it for readiness, and `GET /livez` (200 while the server answers at all; it never touches the database)
for liveness, so a busy disk never gets the server restarted. `/healthz` stays 200 while something is degraded (`ok` false, the reason in
`degraded`: an audit write failed within the last five minutes, so access decisions are being refused; a policy file
couldn't be applied; or the database was busy, with no connection free within two seconds), because a restart wouldn't
fix that. For an uptime check that alerts on it, use `GET /healthz?strict`, which answers 503 while degraded. A
database busy for over a minute is an outage, and every check gets 503. The database runs in WAL mode with
`synchronous=NORMAL`.

On SIGTERM the control plane stops within a few seconds: it closes node links with "restarting" (1012), so nodes
reconnect as soon as the new process listens.

### Environment

| Variable | Default in the image | Meaning |
|---|---|---|
| `FLEETLAYER_CONTROL_DB` | `/data/control.db` | SQLite, WAL mode; migrations run at start-up |
| `FLEETLAYER_CONTROL_LISTEN` | `0.0.0.0:7600` | |
| `FLEETLAYER_CONTROL_HOOKS_LISTEN` | `0.0.0.0:7601` | |
| `FLEETLAYER_CONTROL_POLICY_FILE` | unset | Version 2 policy files the server applies itself, at start and when they change (see [The access policy](#the-access-policy)): `<tenant>=<path>`, a bare `<path>` for the first tenant, or a folder of `<tenant>.yaml`; commas between several |
| `FLEETLAYER_CONTROL_ADMIN_TOKEN_FILE` | unset | A file holding the admin token (mount a secret). The API also takes tokens made with `token add` |
| `FLEETLAYER_CONTROL_AUDIT_KEY_FILE` | unset | The audit log's HMAC key, at least 32 characters (mount a secret; keep a copy outside the cluster, since verifying needs it). Unset: plain SHA-256 chain |
| `FLEETLAYER_CONTROL_AUDIT_ANCHOR_DIR` | unset | Optional: a directory outside the database (another volume, a git checkout) for hourly anchors |
| `FLEETLAYER_LOG` | `info` | tracing filter |

### Admin commands

Run inside the container (`kubectl exec … -- fleetlayer-control <command>`), against the same database. Commands that
only read (`audit verify|tail`, `acl explain|list|show|log|check|suggest`, every `list`) open it without migrating or
writing, so they never hold up the server's writes; they refuse a database whose schema is older or newer than their
own version. Commands that change it take SQLite's write lock briefly, as the server does, and wait up to 15 s for it.
Run admin commands from the same image as the server. A read-only volume mount isn't a way to make a command safe:
SQLite in WAL mode needs its `-shm` file writable even to read.

- `node add <name>`: enrol a node; prints its token once. `node list`, `node remove <name>`.
- `endpoint add --label <l> --channel <c> [--github-secret-file <f>]`: a webhook endpoint; prints `/hooks/<id>/<secret>`
  once. `endpoint list`; `endpoint secret <id> --github-secret-file <f>|--clear` sets GitHub's signing secret;
  `endpoint remove <id>` deletes an endpoint and its secrets (its URL answers 404; the events it brought in stay);
  `endpoint rotate <id>` gives it a new path secret, printed once, keeping its signing secret, and the old URL stops
  working at once (deliveries until the sender is repointed fail; GitHub can redeliver them). For no gap at all, add a
  new endpoint with the same label, channel and signing secret, repoint the sender, then remove the old one.
- `token add <name> [--scope read|admin|send]`: an API token for a program that reads the fleet (`read`: every GET and
  catch-ups), administers it (`admin`), or only sends messages (`send`: a service such as a deck or a bot, which
  sends as `service:<name>` with `POST /v1/messages {to, text, label?, subkind?}` and can read nothing); prints it
  once. A `send` token makes its service a member of the token's tenant, so the access policy decides its messages
  (the `message` verb) like any other sender's, audited and rate-limited. `token list`, `token remove <name>`.
- `audit verify` (exits non-zero on any break) and `audit tail [--denied]`: the tamper-evident log of every access
  decision. Check it on a schedule and alert when it fails. The server does it for you with `GET /v1/audit/verify`,
  so a CronJob needs only `curl` and a token, not the database volume or the audit key. It answers only the result
  and counts, never rows: 200 `{"result":"OK","keyed":true,"rows":N,"problems":0,"anchors":N}`, 409 with
  `"result":"FAILED"` and how many problems (run `audit verify` in the container to see which rows). It needs an
  admin token: make one for the check alone (`token add audit-check --scope admin`), put it in a Secret, and mount it
  in the CronJob, for example as the file `/run/secrets/audit-check/token`:
  `curl -fsS --retry 5 --retry-connrefused -H "Authorization: Bearer $(cat /run/secrets/audit-check/token)" http://<control>:7600/v1/audit/verify`.
  `curl -f` exits non-zero on the 409, so the CronJob fails and your alerting sees it. Keep the retry on refused
  connections: with a network policy, a new pod's first connections can be refused until the policy controller has
  programmed it (k3s takes a few seconds).
- `acl apply <file> [--check]`: make the access policy match a YAML file (a version 2 policy, example
  `deploy/examples/policy.yaml`, or version 1 rules, `deploy/examples/acl.yaml`), printing what changed; running it
  again changes nothing. For a version 2 policy, prefer `FLEETLAYER_CONTROL_POLICY_FILE` (below).
  `acl explain|show|list|check` for inspection.
- `subscribe <channel> <tab>@<node> [--subject s]`, `unsubscribe …`, `fallback <channel> <tab>@<node>|--none`.

### The access policy

Keep the policy in git, where the agents it limits can't change it, and let the server apply it: mount it (a
ConfigMap) and point `FLEETLAYER_CONTROL_POLICY_FILE` at it. The server applies it before it answers anything, then
reads it again every 10 seconds and applies any change at once: no restart, no init container, and nothing else
writing the policy. A file that isn't a valid version 2 policy is refused: the policy already stored keeps deciding,
the error is logged, and `/healthz` reports it (`?strict` answers 503) until the file is fixed. Check a change before
it's merged with `acl apply <file> --check` and `acl test` in CI.

For several tenants, mount a folder with one `<tenant slug>.yaml` per tenant, or give `<slug>=<path>` for each.

Moving from an init container that runs `acl apply` (and a restart on every ConfigMap change, such as Reloader's):

1. Deploy this version with `FLEETLAYER_CONTROL_POLICY_FILE` pointing at the mounted file, and keep the init container
   for this one rollout. Both apply the same text, so the second finds nothing to change.
2. Check `/healthz` shows nothing degraded, and that the log says the policy applied (or nothing, when it already
   matched).
3. Remove the init container and the restart-on-change annotation for the policy. From now on a ConfigMap change takes
   effect within about a minute (the kubelet's sync, then the server's 10-second check), with no restart.

To move back, add the init container again and unset the variable.

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
