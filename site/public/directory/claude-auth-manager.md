# claude-auth-manager

Year-long Claude setup tokens on every machine, usage shared across the fleet, sign-ins kept on one hub you run.

| | |
|---|---|
| Kind | Plugin |
| Roles | Sync source, Sync target |
| Status | experimental |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/claude-auth-manager |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-claude-auth-manager` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.2.0 and later |
| Plugin protocol | version 1 |
| Needs | Claude Code; an always-on machine for the hub |
| Grants | `sync.publish`, `reading.publish`, `reading.subscribe` |

## What it does

One plugin with two roles. The hub signs each account in once (from your browser, headless), makes a setup token
per account, polls usage, and sends the tokens to the machines you list. On every machine, the node half writes the
tokens into account slots, checks each one works, and serves the Claude account rotator's coordinator. Early: it
works, and it will be redesigned.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
# On every machine that runs agents (the node half):
plugins:
  - name: claude-auth-manager
    command: fleetlayer-claude-auth-manager
    grants: [reading.publish, reading.subscribe]
# The hub, on an always-on machine under its own Unix user: see the guide.
```

## What it can read and do

The hub holds every account's claude.ai sign-in and setup token, so it runs under a Unix user of its own on a machine you administer. Tokens reach other nodes encrypted to each node's key; the control plane can't read them. On each node it writes tokens into Claude Code's account slots (the login keychain on macOS, a 0600 file on Linux). Limit who may send its bundle in your access policy.

More: https://fleetlayer.ai/guides/claude-accounts.md.
