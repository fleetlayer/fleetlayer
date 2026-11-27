# herdr

Connects herdr, the terminal session manager, so every session it runs is in the fleet.

| | |
|---|---|
| Kind | Plugin |
| Roles | Session manager, Harness |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/herdr |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-herdr` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.1.0 and later |
| Plugin protocol | version 1 |
| Needs | herdr (https://herdr.dev) |

## What it does

The session manager plugin. It lists herdr's sessions with their status, streams changes, opens and closes
sessions, delivers messages by typing them in when nothing better is available, and streams terminals for
`fleetlayer attach`. Codex and other agents running in herdr are in the fleet through it.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: herdr
    command: fleetlayer-herdr
```

## What it can read and do

Sees every herdr session of its Unix user (names, status, the agent running in each) and types into a session's terminal when a message can't go through the agent's own channel. Set typing_fallback false in node.yaml on a machine where people work in those tabs.
