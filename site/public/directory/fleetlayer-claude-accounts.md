# fleetlayer-claude-accounts

Shows every Claude account the hub manages, with its usage, in menu bar apps such as [CodexBar](https://codexbar.app).

| | |
|---|---|
| Kind | Command-line tool |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/crates/fleetlayer-accounts |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-claude-accounts` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.4 and later |
| Needs | claude-auth-manager's hub; a running node allowed to read the hub's readings |

## What it does

Prints the hub's accounts in claude-swap's account-list format (`--list --json`), with the newest usage from any
node, so every account shows on every Mac whether or not that Mac holds a sign-in.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
# In CodexBar: Settings › Providers › Claude › "Read accounts from claude-swap",
# with the full path to fleetlayer-claude-accounts as the claude-swap executable.
```

## What it can read and do

Prints account names and usage figures that the node already receives as readings. It holds no tokens.

More: https://fleetlayer.ai/guides/claude-accounts.md.
