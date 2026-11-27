# Many Claude accounts on many machines

You run agents on a laptop, a home server and maybe a VPS, and one Claude subscription stopped being enough, so you
have two or three. Then the trouble starts. Sessions ask for `/login` in the middle of their work. One account runs
out while another sits unused. Signing every account in on every machine takes an afternoon, and a month later the
sign-ins expire and you do it again.

This guide explains why that happens and sets up what the author runs instead: one sign-in hub on an always-on machine
that holds the sign-ins, a year-long setup token per account sent to every machine, and usage shared across the fleet
so an account rotator moves sessions before an account runs out.

The hub is early: it works and the author's fleet runs on it, and it will be redesigned.

## Why sign-ins break when they're shared

A normal claude.ai sign-in (`/login`, `claude auth login`) holds an access token that lasts about 8 hours and a refresh
token that's used once. Each refresh returns a new refresh token and revokes the old one, and the sign-in ends after
30 days however often it's refreshed.

That works for one person in one terminal. Two holders of the same sign-in knock each other out: two machines, a
copied `.credentials.json`, or many sessions on one machine. Whichever refreshes second gets `invalid_grant`, sometimes
writes blank tokens over the winner's, and every session on that account asks for `/login` (Claude Code issues
#21765, #24317, #88583 and #108871 describe it). So every machine needs its own sign-in for every account, redone
every month: with 3 accounts and 4 machines, that's 12 sign-ins a month. The same race signs remote OAuth MCP servers
out daily when dozens of sessions share one token entry.

A setup token avoids all of it. `claude setup-token` makes a token that lasts a year and never refreshes, so any
number of machines and sessions can hold the same one. You make one per account, once a year.

## What you give up with setup tokens

A setup token has only the inference scope. Sessions run Claude normally, but the features tied to a claude.ai account
go away: claude.ai connectors disappear from `claude mcp list`, and very likely artifacts, Remote Control and cloud
sessions too, since the profile endpoint refuses the token.

Several accounts already break those features on their own. Connectors belong to one claude.ai account, so each
account has its own set, set up by hand, and each connector holds one identity (one Google account per Gmail or
Calendar connector). A session that a rotator moves to another account gets that account's connectors, or none, in
the middle of its work. Claude in Chrome answers only the account its extension is signed in to, so a moved session
gets "signed in to a different account", and `/login` doesn't fix it.

What replaces each one:

- **Connectors:** one MCP gateway that holds the sign-ins for every remote MCP server, used by every session on every
  account.
- **Claude in Chrome:** your own Chrome shared through Fleetlayer with Playwright MCP. See [Share your signed-in
  browser with agents on other machines](browser.md).
- **Remote Control:** a relay to your phone for your session manager.
- **Artifacts:** pages you host yourself (Fleetlayer's [pages](../plugins/pages.md)).

If you run one account on one machine and use these features every day, stay on a normal sign-in. This guide is for
the fleet that has outgrown that.

## How it fits together

- **The hub** is Fleetlayer's claude-auth-manager plugin with `role: hub`, on an always-on machine under a Unix user of
  its own. It holds each account's normal sign-in, which it uses only to read usage and identity, and each account's
  setup token. It serves a small web page where you do every sign-in from your own browser.
- **Every machine's node** gets the setup tokens in a sealed bundle. The hub's node seals the bundle to each target
  node's key, and the control plane relays it without being able to read it. Each node writes the tokens into Claude
  Code's account slots and checks that each one works.
- **Usage is shared as readings.** Sessions report the five-hour and seven-day figures from response headers as they
  work, and the hub polls each account every five minutes. Every node sees the newest figures for every account.
- **The account rotator** reads those figures through the coordinator the plugin serves on each node, and moves a
  session to another account before the one it's on runs out.

Account numbers are the same everywhere: account 1 is each machine's default sign-in (`~/.claude`), and 2 and up are
the rotator's slots (`~/.claude/accounts/<n>`).

Keep the hub off the laptop. The agents on servers depend on it, and it can't poll usage while the lid is closed.
`fleetlayer up` starts a hub on the laptop to begin with (see [Start on your laptop](laptop.md)); move it to a server
before the server's agents rely on it.

The steps below use `server` for the always-on machine, `claudehub` for the hub's Unix user (so its node is
`server-claudehub`, the default name for a second user on a machine), and `laptop` and `server` for the nodes whose
agents use the tokens. Ask your agent to do steps 1 to 4 and 6; step 5 is yours.

## 1. A Unix user for the hub

On the server, as root:

```sh
useradd --create-home --shell /bin/bash claudehub
loginctl enable-linger claudehub     # its services run without anyone logged in
```

The hub's folder holds every account's normal sign-in. With its own user, the agent users on the same machine can't
read it. Whoever is root on the machine still can, so put the hub on a machine you administer.

## 2. Its node

Enrol it on the control plane (`--db <path>` if the control plane keeps its database elsewhere):

```sh
fleetlayer-control node add server-claudehub
```

As `claudehub`, install Fleetlayer, save the token it printed to `~/.config/fleetlayer/node-token` (mode 0600), and
make a login key for the hub's page:

```sh
(umask 077; mkdir -p ~/.config/fleetlayer && head -c 32 /dev/urandom | base64 > ~/.config/fleetlayer/claude-hub-login)
```

Then `~/.config/fleetlayer/node.yaml`:

```yaml
control:
  url: wss://control.example/v1/link
  token_file: ~/.config/fleetlayer/node-token
plugins:
  - name: claude-auth-manager
    command: fleetlayer-claude-auth-manager
    grants: [sync.publish, reading.publish]
    config:
      role: hub
      targets: [laptop, server]                    # the nodes that get the tokens
      hub:
        listen: 10.0.0.5:7660                      # behind a private HTTPS proxy, never the internet
        login_file: ~/.config/fleetlayer/claude-hub-login
        trusted_proxies: [10.0.0.2]                # the proxy's address
        heartbeat:                                 # optional: a Gatus external endpoint
          url: https://status.example/api/v1/endpoints/claude-hub/external
          token_file: ~/.config/fleetlayer/claude-hub-heartbeat-token
```

```sh
fleetlayer-node install
fleetlayer status
```

Put the page behind your private HTTPS proxy (`fleetlayer-gateway emit traefik` writes the route for it, see [Start
on your laptop](laptop.md#on-a-server-traefik-and-gatus-from-the-same-list)) and firewall the port so only the proxy
reaches it. Set `trusted_proxies`: the page locks a client address out for 15 minutes after five wrong logins, and
behind a proxy every visitor shares the proxy's address unless the hub believes its `X-Forwarded-For`.

From v0.3.9 the page can sign people in with Fleetlayer's own sign-in (passkeys, one sign-in for every service)
instead of the login key: see the `signin` keys under [The hub](../plugins/claude-auth-manager.md#the-hub), and
[Sign-in](../plugins/signin.md) for the other side.

Its state lives in `~/.local/state/fleetlayer/claude-hub` (0700). Leave that folder out of backups. Losing it costs a
few minutes of signing in again, and the tokens already on the nodes keep working for up to a year.

## 3. Monitor it

Add an uptime check on the page's `/healthz` and an external endpoint for the heartbeat in your monitor. The hub
pushes the heartbeat after every five-minute round, with `success=false` and the reasons when an account couldn't be
polled or refreshed, or a token belongs to the wrong account. A hub that's running but no longer working alerts too.

## 4. The node half on every machine that runs agents

On `laptop` and `server` (each agent user's node), add the plugin with its default role:

```yaml
plugins:
  - name: claude-auth-manager
    command: fleetlayer-claude-auth-manager
    grants: [reading.publish, reading.subscribe]
```

Restart the node. When a bundle arrives, the plugin writes each account's token into its slot, removes slots for
accounts that left the bundle, and replaces any sign-in someone made by hand in a slot it manages (a hand sign-in
expires in a month). `default_slot: false` in its `config:` leaves `~/.claude` alone. Then it runs one small
`claude -p` from each slot and publishes the result as a `claude.token_health` reading.

- **macOS:** the tokens go into the login keychain, so the node must run as a per-user LaunchAgent, which is what
  `fleetlayer-node install` sets up. A LaunchDaemon, or an SSH session on a Mac whose keychain is locked, fails with
  "User interaction is not allowed". The items are `Claude Code-credentials` for account 1 and
  `Claude Code-credentials-<first 8 hex of sha256 of the slot path>` for the others, the names Claude Code itself
  reads.
- **Linux:** `<slot>/.credentials.json`, mode 0600.

A LaunchAgent's PATH doesn't include `~/.local/bin`, where Claude Code's installer puts `claude`. The plugin looks there
and in the other install places itself; if yours is somewhere else, set `claude: /path/to/claude` in its `config:`.

## 5. Sign each account in (yours)

This step is the person's. Open the hub's page in your browser and log in with the key from step 2. For each account:

1. **Press Add account, then Set up.** The page shows Claude Code's sign-in link.
2. **Sign in to claude.ai as that account** in your browser first. A setup token belongs to whichever account the
   browser is signed in as, and nothing in the token says which.
3. **Open the link, approve, and copy the code** the page shows. It has a `#` in the middle: copy all of it. The hub
   refuses a code without a `#` before sending it, since `claude setup-token` would fail on a short one and need a new
   link.
4. **Paste it back** into the hub's page. Set up runs the normal sign-in and then the setup token, so you go through
   the link twice. The dialog closes by itself when the account is done.

The hub runs Claude Code's own `claude auth login --claudeai` and `claude setup-token` for each step, and never calls
Anthropic's sign-in endpoints itself. It checks each new token with one request and compares the token's organization
with the account's. A token made for the wrong account is refused ("That token is for a different Claude account…").

The hub warns before anything expires: from day 25 for a normal sign-in (which only the hub uses, for usage), and a
month ahead for a setup token. The page then offers **Sign in** or **Make token** for that account.

## 6. Use the accounts

The hub publishes the bundle whenever a token changes. Check on any machine:

```sh
fleetlayer readings claude.token_health claude.usage
```

Each line is a reading's kind, account, node, time and data. `claude.token_health` with `"ok": true` means that node's
check passed; `claude.usage` shows the five-hour and seven-day figures, and the hub's polls add each model's own week.

For sessions to switch accounts as they fill, install [the many-sessions fork of the Claude account
rotator](https://github.com/ndbroadbent/claude-account-rotator), version 0.7.7 or later, in Claude Code. Its sessions
find the coordinator the plugin serves and use it. The rotator's panel then says the sign-ins come from the auth hub
and offers no Sign in, Add or Remove: those happen on the hub's page.

To run one command from a slot by hand (a script, a test), set `CLAUDE_SECURESTORAGE_CONFIG_DIR` to the slot, which is
where Claude Code reads the token, and `CLAUDE_CONFIG_DIR` to a scratch folder, so Claude Code leaves its own files
there and not in the slot. Unset `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN`, which would override it.

## Every account in the menu bar

[CodexBar](https://github.com/steipete/CodexBar), a macOS menu bar app for AI usage limits, can show several Claude
accounts from a program that prints claude-swap's account list. `fleetlayer-claude-accounts` prints the hub's
accounts in that format, with the newest usage from any node, so every account shows on every Mac whether or not that
Mac holds a sign-in.

In CodexBar: Settings › Providers › Claude, turn on "Read accounts from claude-swap", put the full path to
`fleetlayer-claude-accounts` (for example `/Users/you/.local/bin/fleetlayer-claude-accounts`) in "claude-swap
executable", and leave Source on claude-swap. It needs the Mac's node running and linked, and that node allowed to
`read` the hub's node.

An account shows `token_expired` when this machine's token check fails or its year is up, `relogin_required` when the
hub can't poll it, and `unavailable` until a usage reading exists. CodexBar can't switch accounts through it: the
rotator picks each session's account.

## Other ways to run several accounts

Four tools solve parts of this, and the right one depends on how many sessions and machines you run.

- **[claude-swap](https://github.com/realiti4/claude-swap)** (MIT): a command line that keeps several accounts,
  switches the machine's active account by hand or automatically before a limit, shows a usage dashboard and a menu
  bar app, and launches one terminal as another account (`cswap run 2`) for parallel work. It takes setup tokens too
  (`cswap add-token`), reading their usage from a one-token request when you opt in. It's the most complete tool for
  one machine, and its account list format is the one CodexBar reads.
- **[claude-account-rotator](https://github.com/ivokund/claude-account-rotator)** (MIT): a Claude Code mod that shows
  each account's usage under the prompt and moves each session to another account before its account runs out, per
  session rather than machine-wide. It keeps Claude in Chrome connected as the session's account.
- **[The many-sessions fork](https://github.com/ndbroadbent/claude-account-rotator)** (MIT): the same mod with one
  coordinator process per user that reads every account's sign-in and polls usage for all sessions, built for 40 to
  60 sessions on one machine.
- **Fleetlayer's claude-auth-manager:** the sign-ins live on one always-on hub, every machine gets year-long setup
  tokens sealed to its key, and usage is shared across the fleet. It runs the fork's coordinator on each node and needs
  the fork for switching.

One machine and a few sessions: claude-swap or the original rotator. Many sessions on one machine: the fork. Many
machines: claude-auth-manager with the fork. The design notes are in
[claude-auth-manager](../plugins/claude-auth-manager.md).

## Limit who can send tokens

A node that could publish the tokens bundle could put its own tokens in your slots. Allow only the hub's node, in your
[access policy](../access-control.md):

```yaml
version: 2
default: allow

deny:
  - who: "*"
    can: [sync]
    to: ["sync:claude-auth-manager/tokens@*"]
    except: ["-@server-claudehub"]
    why: Only the hub's node sends Claude tokens.

ask:                    # replaces the default, which also asks before each first sync
  - who: "*"
    can: [start, attach]
    to: ["*"]
    for: 10m
```

Keep `sync` out of your `ask:` rules, or write an `ask:` of your own as above. Fleetlayer's default `ask:`, used when no
policy is applied or a policy leaves it out, holds each target's first bundle for your approval, and the approval lasts 30 days from the last bundle sent, so it
lapses only if the hub stops publishing for a month. While a bundle waits, the hub's node reports it (a `sync.status`
reading with the command that approves it) and tries again once you approve, without a restart. `fleetlayer-control acl explain --from=-@server-claudehub --can sync --to
sync:claude-auth-manager/tokens@laptop` shows the decision.

## When it doesn't work

- **The plugin waits and never serves the coordinator:** one coordinator runs per user, held by `coordinator.lock` in
  `~/.claude/account-rotator`. A rotator's own coordinator is asked to step down, and the `claude.coordinator` reading
  says who holds the lock. Sessions need rotator 0.7.6 or later to leave the plugin's coordinator alone.
- **"User interaction is not allowed"** on a Mac: the node runs as a LaunchDaemon or over SSH with the keychain locked.
  Run it as a LaunchAgent (`fleetlayer-node install` as that user, logged in).
- **"No such file or directory"** from the token checks on a Mac: the LaunchAgent can't find `claude`. Set `claude:`
  in the plugin's config.
- **"That doesn't look like the full code"** when signing in: the copy lost the part after the `#`. Copy the whole
  code again.
- **"Wrong account"** on an account: its setup token was made while claude.ai was signed in as another account. Sign
  in to claude.ai as the right account in your browser and press **Make token** again. The hub leaves such a token out
  of the bundle and fails its heartbeat until then.
- **Everyone is locked out of the page after one person's typos:** `trusted_proxies` isn't set, so every visitor
  shares the proxy's address.
- **Nothing reaches a node:** look in the hub's node log for a refused sync (your policy) or a target whose key
  changed. The hub's node pins each target's key the first time it seals to it, in
  `~/.local/state/fleetlayer/known-keys.json`, and refuses a changed key until you remove the old pin.
- **CodexBar shows nothing:** run `fleetlayer-claude-accounts --list --json` in a terminal. It prints the reason
  (`NodeUnreachable`, `NoAccounts` when this node may not read the hub's readings, `HubTooOld`).
