# An admin role that's off until you open it

Some jobs need an agent that can change everything on a machine: upgrade packages, edit files under `/etc`, restart
services, rotate a production key. Leaving such an agent on all day means one bad instruction (or one prompt injection
that reaches it) can do anything. This guide sets up an **admin role** that sits switched off, gets its rights only for
a window you approve (an hour, a day), and loses them again at the end: sudo mode for agents.

The shape:

- **A Unix user of its own, `admin`, with its own node.** The node is the trust boundary: every process of a Unix user
  can act as any of its sessions, so the admin role must never share a user with everyday agents.
- **Off by default.** While its window is closed, the control plane refuses everything it asks for and everything
  addressed to it: no messages, no shared tools, no secrets delivered.
- **A window through approval.** One of its sessions asks (`fleetlayer role activate admin --for 1h --why "…"`), and
  you approve it the way you approve anything else.
- **A real end.** At the end, its node closes every session it has, removes the secrets delivered to it, and runs a
  hook; a separate, more trusted user removes its sudo rule. The node closes the window by itself at the end time even
  if the control plane is unreachable.

This uses a server node called `server` for the trusted side and `server-admin` for the admin role's node. Change both
to yours.

## 1. A Unix user for the role, and one for the trusted side

```sh
sudo useradd --create-home --shell /bin/bash admin
sudo loginctl enable-linger admin
```

The sudo rule is added and removed by a user the admin agents can't act as. Your own everyday user can do it, but a
small user of its own is cleaner, because it needs only one narrow sudo rule:

```sh
sudo useradd --create-home --shell /usr/sbin/nologin fleetlayer-runner
sudo loginctl enable-linger fleetlayer-runner
```

Give each its own node (`fleetlayer-control node add server-admin` and `node add server-runner`, then the node set up
as in [Add a machine](https://fleetlayer.ai/guides/node-services.md), run as that user).

## 2. The script that adds and removes the sudo rule

Put it where only root can change it, `/usr/local/sbin/admin-window`, mode 0755, owned by root:

```sh
#!/bin/sh
# admin-window open|close: give the admin user full sudo for a window, and take it away.
set -eu
rule=/etc/sudoers.d/90-admin-window
case "${1:-}" in
  open)
    tmp=$(mktemp)
    echo 'admin ALL=(ALL:ALL) NOPASSWD: ALL' > "$tmp"
    visudo -cf "$tmp" >/dev/null
    install -m 0440 -o root -g root "$tmp" "$rule"
    rm -f "$tmp"
    ;;
  close)
    rm -f "$rule"
    ;;
  *) echo "usage: admin-window open|close" >&2; exit 2 ;;
esac
```

Then the sudo rules that let each side run it:

```sh
# /etc/sudoers.d/80-admin-window (visudo -f): the runner opens and closes; admin may only close.
fleetlayer-runner ALL=(root) NOPASSWD: /usr/local/sbin/admin-window open, /usr/local/sbin/admin-window close
admin ALL=(root) NOPASSWD: /usr/local/sbin/admin-window close
```

Closing is always safe, so the admin role's own node can close too: if the runner is down when a window ends, the
rule still goes.

## 3. The nodes' hooks

On the runner's node (`~fleetlayer-runner/.config/fleetlayer/node.yaml`), watch the role and run both hooks:

```yaml
windows:
  - role: admin
    on_start: [sudo, -n, /usr/local/sbin/admin-window, open]
    on_end: [sudo, -n, /usr/local/sbin/admin-window, close]
```

On the admin node (`~admin/.config/fleetlayer/node.yaml`), only the end:

```yaml
windows:
  - role: admin
    on_end: [sudo, -n, /usr/local/sbin/admin-window, close]
```

The admin node also closes its sessions and removes what was delivered to it at the end without any configuration.

## 4. The policy

```yaml
roles:
  admin:
    who: "*@server-admin"
    activation: window
    longest: 12h
    asked_by: "*@laptop"         # your everyday sessions may ask for it too
    why: changes the server itself

allow:
  - who: "*@server-admin"
    can: [use_tool, sync, message]
```

Apply it (`fleetlayer-control acl apply policy.yaml`, or let the server read it with `--policy-file`), and check it:

```yaml
# policy-tests.yaml, for `fleetlayer-control acl test`: the role's own rules, as if its window were open.
- { from: ops@server-admin, can: use_tool, to: "tool:db@server", expect: allow }
```

## 5. Using it

From a session on the admin node (or one of `asked_by:`):

```sh
fleetlayer role activate admin --for 1h --why "upgrade postgres to 17 and restart it"
fleetlayer role list
```

You approve it like any other request (`fleetlayer-control approvals approve <id>`, or wherever your approvals reach
you; `--for` there can shorten it, never stretch it past `longest:`). Within seconds the runner adds the sudo rule,
the role's grants apply, and secrets granted to it are delivered. Closing early: `fleetlayer role deactivate admin`.

To open one yourself without the request: `fleetlayer-control role activate admin --for 1h`.

## Credentials that end with the window

The sudo rule and Fleetlayer's grants end on time. A long-lived token the agent read during the window doesn't: it
could have been copied. Prefer credentials that expire on their own:

- **AWS:** grant the role access to assume an IAM role, and have it mint a session for the window:
  `aws sts assume-role --role-arn … --role-session-name admin-window --duration-seconds 3600` (up to the IAM role's
  maximum session duration, 1 to 12 hours).
- **GitHub:** a GitHub App's installation tokens last an hour; mint one per job instead of a personal access token.
- **Databases:** a user created for the window with `VALID UNTIL` (PostgreSQL), dropped by the end hook.

Rotate the static keys it could reach on a schedule.

## Monitoring

- The control plane's `/healthz` turns degraded when a window ended and the role's node hasn't confirmed closing it
  within five minutes, or reported a problem (a hook that failed, a session that wouldn't close). Point an uptime check
  at `/healthz?strict` ([Monitoring](https://fleetlayer.ai/guides/monitoring.md)).
- The admin node's own health and heartbeat say the same, from its side.
- Opening and closing are events on the `access` channel (`access.window.opened`, `access.window.closed`): subscribe
  your inbox session to hear each one.

## What it doesn't cover

- **A person on the machine.** Anyone who can log in as `admin` can start an agent there outside Fleetlayer. The window
  controls what Fleetlayer gives the role (sudo through the hook, secrets, tools, messages), not the Unix account.
- **Processes started as root during the window.** Removing the rule stops new ones; one already running as root
  keeps running. Add what your setup needs to the close hook (stop a service the agent started, say).
- **What it read.** Anything the role read in the window, it could have sent somewhere. Short-lived credentials and
  rotation limit what that's worth.
