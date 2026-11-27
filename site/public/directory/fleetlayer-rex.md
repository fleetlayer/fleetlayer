# fleetlayer-rex

Connects Rex, Superlogical's terminal and session manager, so the Claude Code and Codex sessions in it are in the fleet.

| | |
|---|---|
| Kind | Plugin |
| Roles | Session manager |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/rex |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-rex` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.9 and later |
| Plugin protocol | version 1 |
| Needs | Rex (https://www.superlogical.com/rex/docs) |

## What it does

The session manager plugin for [Rex](https://www.superlogical.com/rex/docs). It lists the agents in Rex terminals
with their status from program status reports (OSC 7501), streams changes from `rex events`, opens and closes
sessions, delivers messages by pasting them in when nothing better is available, and lets `fleetlayer attach` watch
and drive Rex terminals. It uses only Rex's public command line and API. More in [Fleetlayer and
Rex](https://fleetlayer.ai/rex/).

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: rex
    command: fleetlayer-rex
    config:
      program: rex
```

## What it can read and do

Sees every session on its user's Rex server (labels, the program and folder in each terminal) and types into a terminal only to deliver a message to an agent with no channel of its own, a first prompt, or for `fleetlayer attach` under the access policy. It never starts a Rex server.

More: https://fleetlayer.ai/plugins/rex.md.
