# claude-code

Delivers messages into Claude Code sessions through Claude Code's own messaging channel.

| | |
|---|---|
| Kind | Plugin |
| Roles | Harness |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/claude-code |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-claude-code` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.1.0 and later |
| Plugin protocol | version 1 |
| Needs | Claude Code |

## What it does

The harness plugin for Claude Code. A message arrives in the session as a turn of its own, labelled with who sent
it and how to reply, with nothing typed into the terminal. It also points the node at each session's transcript.
Use it beside a session manager plugin such as herdr.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: claude-code
    command: fleetlayer-claude-code
```

## What it can read and do

Writes to the messaging socket of each Claude Code session of its Unix user. Claude Code sessions in bypass mode hold such messages for approval unless crossSessionInbound is set to accept (see the unattended agents guide).
