---
name: fleetlayer-claude-accounts
description: What Fleetlayer's Claude account setup means for a Claude Code session: year-long setup tokens sent to every machine by a hub, usage shared across the fleet, and an account rotator that can move sessions between accounts. Use when you're asked to log in or see "signed in to a different account", when claude.ai connectors, Claude in Chrome or artifacts are missing, when the person asks how much Claude usage is left on each account, or when a Claude credential, account slot or token check fails.
---

# Claude accounts on a Fleetlayer machine

On a machine whose node runs claude-auth-manager, Claude Code's sign-ins aren't made by hand. A hub on one always-on
machine holds each account's sign-in, makes a year-long setup token per account, and sends the tokens, encrypted, to
every machine. The node writes them into Claude Code's account slots and checks each one works. An account rotator,
if installed, moves sessions to another account before one runs out.

## What this means for you

- **Never run `/login` or `claude auth login`, and never edit an account slot** (`~/.claude/.credentials.json` and
  `~/.claude/accounts/<n>` on Linux, the `Claude Code-credentials-…` keychain items on macOS). The node owns them and
  replaces a hand sign-in with the hub's token. If a session asks you to log in, tell your person: the hub fixes it.
- **Setup tokens are inference only.** A session on one runs Claude normally but has no claude.ai connectors, no
  Claude in Chrome, and very likely no artifacts, Remote Control or cloud sessions. Use the replacements instead:
  the shared browser (`fleetlayer-browser` skill) for Chrome, pages (`pages` skill) for artifacts, and whatever MCP
  servers your setup instructions name for connectors. Don't try to reconnect those features.
- **Your account can change mid-task.** The rotator may move you to another account as one fills. Your work carries
  on; anything tied to one account (a connector, a claude.ai feature) doesn't follow you.

## Usage and token health

```sh
fleetlayer readings claude.usage claude.token_health
```

One line per account and node: its kind, account ID, node, time and data.

- `claude.usage`: `fiveHour` and `sevenDay` percentages with their reset times; readings from the hub's polls add
  each model's own week (`models`). Figures from sessions come as they work; the hub's per-model figures go stale
  while it sleeps, so check `at`.
- `claude.token_health`: `{"ok": true, "email": …, "claudeVersion": …}` means that node's token check passed;
  `"ok": false` with an `error` means the token in that slot doesn't work. Tell your person which account and node:
  they fix it on the hub's page. When a node's checks stop and `fleetlayer status` there shows the auth manager's
  problem `claude not found at <path>` (or `won't run`), the tokens are fine: that machine's Claude Code install is
  broken (often `~/.local/bin/claude` linking to a version that's gone), so tell your person that, not to fix the
  accounts.

Add `--json` for the whole readings.

## Running one command as a given account

Only when your person asks (a script or test that must use a particular account): set
`CLAUDE_SECURESTORAGE_CONFIG_DIR` to that account's slot, `CLAUDE_CONFIG_DIR` to a scratch folder, and unset
`ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN`, which would override it. Never print, copy or move a token.
