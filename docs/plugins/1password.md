# fleetlayer-1password

Agents need tokens: a GitHub token for `gh`, a cloud API key for Terraform, a database password for a script. The
usual way to get them out of 1Password is a service-account token in each agent's shell and `op run`. That token
opens every item in its vaults, so every agent holding it can read all of them, and making a vault and a token per
agent to narrow it is more work than anyone does.

`fleetlayer-1password` holds the service-account token under a Unix user of its own, where agents can't read it. You
grant items to roles, item by item. Each role's node gets exactly its items, sealed to its key, and again within
minutes whenever one changes in 1Password. It ships with Fleetlayer as `fleetlayer-1password`; how it fits with the
rest of Fleetlayer's secrets is on [Credentials](../credentials.md).

> **A role is a Unix user with its own node.** Every process of one user can read what any of its sessions can, so a
> grant goes to a role, never to a session: sessions start, end and get renamed all day, and a new session in a role
> has its secrets with no setup. Which session used a secret can't be enforced inside a role.

## What you need

- 1Password's command line, `op` (2.x), on the source machine. It's 1Password's own program, free to use; the plugin
  runs it with the token in its environment, never on its command line.
- A [service account](https://developer.1password.com/docs/service-accounts/) with read access to the vaults holding
  the items you'll grant. One service account for all of them is fine: what each role gets is decided here, not by the
  vaults the token can see.
- A Unix user for the source (`fleetlayer-1password`, say) with its own Fleetlayer node, on an always-on machine. The
  token file belongs to that user, mode 0600. Agents run as other users and can't read it.

## Configuration

On the source's node (the 1Password user), in `node.yaml`:

```yaml
plugins:
  - name: 1password
    command: fleetlayer-1password
    grants: [sync.publish, reading.publish]
    config:
      source:
        token_files: [~/.config/fleetlayer/1password/token]
        roles:
          - role: everyday                 # a name for people and readings
            nodes: [agents, mac-agents]    # the same role on two machines: each node gets its own sealed copy
            read:
              - op://github/deploy-bot/token
              - op://cloudflare/dns-edit/credential
          - role: infrastructure
            nodes: [infra]
            read: [op://terraform/aws-deploy/access-key, op://terraform/aws-deploy/secret-key]
```

| Field | Default | What it is |
|---|---|---|
| `token_files` | | One service-account token file per 1Password account. Each reference is read with the token whose account has its vault (refer to a vault by its ID if two accounts share a name) |
| `roles` | | Each `role`, its `nodes`, and the items it may `read` |
| `op` | `op` | 1Password's command line, when it isn't on `PATH` |
| `every_minutes` | 5 | How often it checks 1Password for changes |
| `resend_hours` | 12 | It sends again after this long even when nothing changed, so targets can tell it's alive |

References are 1Password's own: `op://<vault>/<item>/[<section>/]<field>`, vault and item by name or ID, with
`?attribute=otp` and the like allowed. A node in several roles gets all of their items in one bundle.

On each role's node:

```yaml
plugins:
  - name: 1password
    command: fleetlayer-1password
    grants: [reading.publish]
    config:
      receive:
        write:                              # optional: files for tools that read one
          - { format: env, path: ~/.config/fleetlayer/secrets/github.env, values: { GITHUB_TOKEN: op://github/deploy-bot/token } }
          - { format: file, path: ~/.config/fleetlayer/secrets/aws-key, ref: op://terraform/aws-deploy/secret-key }
```

`receive: {}` with no files is enough: agents then use the items through `fleetlayer secrets run` (below), and nothing
is written but the node's registry.

| Field | Default | What it is |
|---|---|---|
| `write` | none | Files to write: `format: env` with `values` (variable → reference), or `format: file` with one `ref` |
| `stale_hours` | 48 | Report a problem when nothing has arrived for this long |

Each file is mode 0600 (a missing folder is made 0700), replaced in one rename, and never written over a file that's
already there and wasn't written by this plugin. A file whose references are no longer delivered (the grant was taken
away, or the item removed from 1Password) is removed. A value with a line break can't go in an env file; write it as
a `file`.

## Using the items

Agents on a role's node use items by reference, so values never appear in their commands:

```sh
GITHUB_TOKEN=op://github/deploy-bot/token fleetlayer secrets run -- gh api user
fleetlayer secrets run --env-file .env -- terraform plan     # .env lines like AWS_SECRET_ACCESS_KEY=op://terraform/aws-deploy/secret-key
```

That's `op run`'s job, from what the role was granted and with no 1Password token on the machine. The command's output
comes back with every delivered secret replaced by `[REDACTED <reference>]`. A reference the role wasn't granted is
refused, with a message saying so. `fleetlayer secrets list` shows what this node has.

## Approving it, and who may read what

The source's first bundle to each node waits for your approval, like any sync (the default access policy's `ask:` for
`sync`; [access control](../access-control.md)). `fleetlayer readings sync.status` on the source shows a waiting node
and the command that approves it. Your access policy decides which nodes may receive the plugin's bundles at all
(`sync:1password/<node>@<node>`). Each delivery is written to the control plane's audit log; values never appear in
logs, messages, readings or the plugin's state files.

`fleetlayer secrets grants`, from any node that may read the source's readings, lists every role, its nodes and its
items. Grants live in the source's own `node.yaml`, under a user the agents can't write to.

## Taking a grant away

Remove the reference (or the role, or a node from a role) and restart the source's node. Within a round, each node
affected gets a new bundle without it: the plugin drops it from the node's registry and removes any file that needed
it. A node that loses every grant gets an empty bundle, even after the source restarted (the source remembers which
nodes it delivered to, in `~/.local/state/fleetlayer/1password/source.json`).

Whatever an agent already read could have been copied: rotate the item in 1Password if that matters.

## Monitoring

The plugin reports whether it does its job (`fleetlayer status`, the `plugin.status` reading, the node's heartbeat):

- **Source:** a token file it can't read, `op` missing or failing (no network, a rate limit, a refused token), or a
  granted item that isn't in 1Password (renamed or removed). While `op` fails, nothing is sent, so a target never loses
  an item over a network blip.
- **Target:** a file it couldn't write, a file whose item isn't delivered, or nothing arrived for `stale_hours` (the
  source is down, no role names this node, or its delivery isn't approved).

Readings, which never hold values:

| Kind | Key | Data |
|---|---|---|
| `secrets.grants` | `1password/<role>` | `{source, role, nodes, items: [{ref, access}]}` |
| `onepassword.source` | node | `{node, items: [{ref, updatedAt, delivered}], version, publishedAt, readAt}` |
| `onepassword.receive` | node | `{version, appliedAt, readAt, items: [{ref, updatedAt}], files, missing, problem}` |

A 1Password service account has hourly read limits (`op service-account ratelimit` shows them). The plugin reads each
vault's item list once a round and reads an item again only when its version changed, so a few dozen grants checked
every five minutes stay far below them.

## The bundle

`1password/<node>`, one item, `secrets.json`: `{node, readAt, secrets: [{ref, value, updatedAt}]}`, where `updatedAt`
is when 1Password last changed the item. A bundle is the node's complete set.

## Moving it

The source keeps its state in `~/.local/state/fleetlayer/1password/` and its settings in `node.yaml`. To move it:
stop its node, copy the token file (sealed, or through 1Password itself: it's a service-account token you can also
make again), the node's config folder (its key) and that state folder, start it on the new machine, and remove the old
node's token file.
