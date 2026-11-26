# Getting started

Fleetlayer is early. This is how to run it today, from source.

## Build

```sh
cargo build --release
# target/release/: fleetlayer, fleetlayer-node, fleetlayer-control, fleetlayer-herdr, fleetlayer-claude-code
```

Put the binaries on your `PATH`.

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

## Monitoring the node

```yaml
heartbeat:
  url: https://gatus.example.com/api/v1/endpoints/fleetlayer_laptop/external
  token_file: ~/.config/fleetlayer/heartbeat-token
  every: 60
```

The node reports `success=false` with the reason when a plugin isn't running or the control plane link is down, and
stops reporting when it dies, so the monitor alerts either way.
