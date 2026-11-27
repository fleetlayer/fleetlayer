# Fleetlayer and Rex

[Rex](https://www.superlogical.com/rex/docs) is a terminal and session manager from
[Superlogical](https://www.superlogical.com/). Fleetlayer works with it: Rex keeps your terminals and sessions alive
and shows them on your Mac, phone and other machines, and Fleetlayer connects the Claude Code and Codex agents
running in those terminals so they can message each other, receive webhooks, and work under access rules you set.
Neither replaces the other.

This page says what Rex is, what Fleetlayer adds, what works today, and what's coming. Everything it says about Rex
comes from Superlogical's public documentation, linked beside each claim.

## What Rex is

- **A terminal multiplexer with a separate server.** Sessions live in a Rex server process. Clients attach to it,
  show it and detach, so sessions survive a closed window or a lost connection. A server holds sessions, a session
  holds windows (tabs), and a window holds panes of blocks; today the main kind of block is a terminal
  ([CLI reference](https://www.superlogical.com/rex/docs/reference/cli)).
- **Clients on many devices.** There's a native Mac app, an iOS and iPadOS app in the
  [App Store](https://apps.apple.com/us/app/rex-by-superlogical/id6813649254), and a Windows app in testing. Servers
  run on macOS, Linux, FreeBSD and Windows ([Installation](https://www.superlogical.com/rex/docs/installation)). One
  client reaches many servers, over SSH or Tailscale among other ways
  ([Remote Access](https://www.superlogical.com/rex/docs/linux/remote-access)).
- **Scriptable.** The `rex` command line manages sessions, windows and panes, sends keys, reads screens and waits
  for output, and the server describes its own API (`rex api list`, `rex api describe`)
  ([Server API](https://www.superlogical.com/rex/docs/automate/server-api)). Configuration and event handlers are
  written in Lua.
- **Friendly to coding agents.** `rex skill setup` installs a skill and a session-start hook for Claude Code and
  Codex (and a skill for OpenCode and Pi), so an agent can open tabs, split panes, read another terminal's screen and
  wait for a dev server ([Agents](https://www.superlogical.com/rex/docs/automate/agent-skill)).
- **Two open terminal protocols.** With the [Program Status Protocol
  (OSC 7501)](https://www.superlogical.com/rex/docs/build/program-status), a program tells the terminal whether it's
  idle, working, blocked on the user (a permission prompt, a question, a sign-in), done or failed. With the [Program
  Resume Protocol (OSC 7577)](https://www.superlogical.com/rex/docs/build/program-resume), a program tells the
  terminal how to bring it back after a restart. Superlogical publishes both as generic protocols any terminal can
  adopt.

Rex is in public testing. The Mac app and the Linux, FreeBSD and Windows servers are open to all testers
([Installation](https://www.superlogical.com/rex/docs/installation)), and Superlogical says you don't need an
account and can self-host fully ([public testing
announcement](https://www.superlogical.com/updates/public-testing-beginning)).

## What Fleetlayer adds

Rex manages sessions and terminals. Fleetlayer works one level up, with the agents in them, across every machine
and every session manager you use:

- **Messages between agents.** `fleetlayer msg send --to infra@home "the migration is done"` reaches the agent in
  that session on any machine, labelled with who sent it and how to reply. For Claude Code it arrives through Claude
  Code's own messaging channel, so nothing is typed into a terminal you're working in ([How a message
  travels](concepts.md#how-a-message-travels)).
- **Events and webhooks.** A CI result or another webhook goes to the agent that owns the branch, not to your inbox
  ([CI results to the right agent](guides/ci-results.md)).
- **Access rules, approvals and an audit log.** Each agent gets its own rules about who it may message, which
  machines it may start sessions on and which tools it may use. Every decision is written to an audit log
  ([Access control](access-control.md)).
- **Agents that read untrusted input.** An agent that reads strangers' email or issues runs as its own user, and
  everything it sends is marked as outside data for the agents that receive it ([An agent that reads strangers'
  text](guides/untrusted-input.md)).
- **Sealed secrets.** Tokens and keys are encrypted to each machine's own key; the control plane relays them without
  being able to read them ([Secrets stay on your machines](concepts.md#secrets-stay-on-your-machines)).
- **Claude accounts.** Several Claude subscriptions shared across the fleet, with long-lived tokens on each machine
  ([Many Claude accounts](guides/claude-accounts.md)).
- **Shared tools.** One machine's MCP servers, such as the browser on your laptop, used by agents on other machines
  ([Share your browser](guides/browser.md)).

Fleetlayer is open source under Apache 2.0 and self-hosted: you run the control plane yourself, on your laptop to
start or on a server later.

## How they fit

Fleetlayer never replaces or hides a session manager. It attaches to the one you use through a **session-manager
plugin**, a separate program the node starts ([the plugin protocol](protocol.md#4-sessions-the-session-manager-role)).
herdr has one, and Rex has [`fleetlayer-rex`](plugins/rex.md), which uses only Rex's public command line and API:

| Fleetlayer needs | From Rex |
|---|---|
| The list of sessions and where each runs | `session.list`, `session.view` and each terminal's `process` |
| Each agent's status (idle, working, blocked, done) | Program status reports (OSC 7501), which Claude Code sends |
| Status changes as they happen | `rex events` |
| Messages to an agent that has no channel of its own | A paste into its terminal (`block.event`) |
| Starting and closing sessions | `rex new`, `block.close` and `session.destroy` |
| Watching or stepping into a terminal from anywhere in the fleet | The terminal's `format`, `write` and `resize` methods, for `fleetlayer attach` |

Your Rex apps keep working alongside it. You watch and type into sessions from Rex on your Mac or phone, and
Fleetlayer routes messages and events to the agents in them, under your rules.

## What works today

`fleetlayer-rex` ships with Fleetlayer from v0.3.9 ([its directory
entry](https://fleetlayer.ai/directory/fleetlayer-rex/)). With it, the Claude Code, Codex and other
agents in your Rex terminals join the fleet by their Rex session names, with their status, messages and webhooks,
starting and closing sessions from anywhere in the fleet, and `fleetlayer attach` on Rex terminals. Rex and herdr can
also run side by side on one machine, each with its plugin. [The plugin's page](plugins/rex.md) has the setup.

To start with Fleetlayer, see [Getting started](getting-started.md).
