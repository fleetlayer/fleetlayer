# scheduler

Sends a message to an agent session on a schedule (every few hours, a cron expression, or once at a set time).

| | |
|---|---|
| Kind | Plugin |
| Roles | None (a service the node runs) |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/scheduler |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-scheduler` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.12 and later |
| Plugin protocol | version 1 |
| Grants | `message.send` |

## What it does

The recurring prompt you'd otherwise send a worker agent by hand ("check the queue every three hours") as one entry
in a settings file. It retries a session that's waiting on a person, never sends a burst of catch-up messages after
downtime, and reports a problem when fires stop getting through, so your monitoring hears about it.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: scheduler
    command: fleetlayer-scheduler
    grants: [message.send]
    # entries go in ~/.config/fleetlayer/scheduler.yaml
```

## What it can read and do

Sends messages as plugin:scheduler@<node>, under the same access policy as any sender: a message to another node is checked, one to a session on its own node isn't (as with every message on one node). It's only as trusted as its node's Unix user, so it can't give an agent orders that node couldn't. Reads only its settings file and state directory.

More: https://fleetlayer.ai/plugins/scheduler.md.
