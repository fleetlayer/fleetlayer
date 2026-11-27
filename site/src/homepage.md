# Fleetlayer

An open framework for running AI agents securely and at scale.

Fleetlayer connects the Claude Code and Codex sessions you already run in [herdr](https://herdr.dev/), on your laptop, a
home server or a VPS. Any agent can message any other agent, webhooks can be routed to specific agents, and each agent
gets its own access rules. Open source under Apache 2.0, early, built in public:
https://github.com/fleetlayer/fleetlayer

## Claude Code is awesome…

…when you have one Claude account and run it on one machine. Add a second account, a server or another agent,
and this starts happening:

- claude.ai artifacts say “access denied” unless you’re signed in to the right account.
- A session on your server can’t message one on your laptop.
- You keep signing in and out of the Claude in Chrome extension.
- An agent on your server can’t use the browser on your Mac.
- An agent on your server can’t reach the Downloads or Screenshots folders on your Mac.
- Codex and Gemini agents can’t make claude.ai artifacts at all.
- You find out an account is out of usage when a session stops.

## Fleetlayer solves all of this, and much more

- **Fleetlayer Pages and Apps.** Claude, Codex and Gemini agents write pages and interactive apps as plain folders, and Fleetlayer hosts them behind one sign-in. [How](https://fleetlayer.ai/guides/pages/)
- **Messages between any two agents.** Claude or Codex, on the same machine or another one: `fleetlayer msg send --to api@laptop`. [How](https://fleetlayer.ai/guides/day-to-day/)
- **Any agent can control your browser.** Agents on any machine and any Claude account can use the Chrome on your Mac, signed in as you (via Playwright MCP). [How](https://fleetlayer.ai/guides/browser/)
- **Every account in one place.** Year-long sign-ins on every machine, and every account's usage shared with every session. [How](https://fleetlayer.ai/guides/claude-accounts/)

## How it fits

A node on each machine, or each Unix user, keeps one outbound connection to the control plane, so it works behind NAT
with no ports to open. It attaches to herdr without taking it over: your apps and terminals keep working beside it.
Plugins are separate processes in any language, speaking the Agent Client Protocol plus Fleetlayer's extensions.

The pieces: `fleetlayer-node` (one per machine or Unix user), plugins (herdr, Claude Code, summaries, Claude accounts,
pages), `fleetlayer-control` (the control plane: machines, sessions, messages, webhooks, access rules; it holds no
credentials) and `fleetlayer` (the command line).

## What you get

- **Every session in one list**: every machine and session, with a one-line summary of what each agent is doing and a
  searchable record of what it did. `fleetlayer attach api@laptop` watches or drives a session's terminal from
  anywhere.
- **Agents that message each other**: `fleetlayer msg send --to api@laptop "the staging deploy is done"`, on one
  machine or across machines, delivered through the agent's own channel and labelled with who sent it.
- **Webhooks that find the right agent**: a CI failure goes to the session that wrote the branch.
- **Many Claude accounts, many machines**: usage across all your accounts, shared with every session; year-long tokens
  reach each machine sealed to its own key; sign-ins stay on a machine you control.
- **Separate users, separate rules**: run several nodes and herdr instances on the same machine or VM, each under its
  own Unix user. The access policy limits whom each agent may message and what it may use.
- **Tools that live on one machine**: your signed-in browser on a laptop, used by an agent on a server with
  `fleetlayer mcp browser@laptop`.

## A foundation for your own agent workflows

Fleetlayer started inside Quietdeck. Once we saw how useful that layer was on its own, we pulled it out into its own
project and refined it. It gives you the primitives to build your own apps and layers on top: sessions at `tab@node`,
messages, webhooks and channels, access rules, approvals, an audit log, plugins in any language, shared MCP tools,
and pages and apps.

Built on Fleetlayer: [Quietdeck](https://quietdeck.ai/). An agent triages your email and proposes what to do, and you
approve. Quietdeck is designed to run on Fleetlayer and uses its features to:

- forward emails and notifications to the agent that owns the work through Fleetlayer’s messaging
- know what all your agents are doing from Fleetlayer’s session summaries, and route each message to the right agent
- start an isolated email triage agent on its own Unix user, connected through its own Fleetlayer node, with access
  rules that limit whom it may message

## One system, many parts

Fleetlayer is the layer between your agents and the tools around them. Each part does one job and can be replaced.

- **Agents:** Claude Code, Codex, Gemini
- **Sessions:** herdr (https://herdr.dev)
- **Fleetlayer:** cross-agent messaging, webhooks, single sign-on, access rules, approvals, audit log, remote terminals
- **Fleetlayer Plugins:** herdr, Claude Code, AI account management, session summaries, file sync, shared folders, pages and apps (all optional)
- **Tools:** Executor (https://executor.sh) for MCP servers and sign-ins to your services
- **Secrets:** 1Password
- **Browser:** Playwright MCP
- **Files:** Syncthing, Obsidian Sync, Obsidian LiveSync, or any file syncing solution
- **Network:** Tailscale, Cloudflare Tunnel
- **Services:** Gatus for health checks, Traefik for addresses
- **Backups:** restic (https://fleetlayer.ai/guides/backups.md)
- **Mobile:** herdr-mobile-relay (https://github.com/0cv/herdr-mobile-relay)

## Start small, replace any component as you go

Begin with everything on your laptop. When you outgrow it, move one piece to a home server or a VPS, or swap it for
the tool you'd rather use. Each piece keeps its state in one folder and speaks open protocols.

Ask your agent: "Set up Fleetlayer on this machine. The docs are at https://fleetlayer.ai/llms.txt". What it runs:

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
fleetlayer up        # everything on this machine; status page at http://fleetlayer.localhost:7680
fleetlayer sessions
```

Next: https://fleetlayer.ai/guides/laptop.md

## No vendor lock-in

Your own stack instead of a vendor's cloud, made of separate pieces that work with the tools you already use. Keep
using Anthropic's and OpenAI's own features for as long as they serve you; when you outgrow them, you give nothing up,
and you can still change models and harnesses whenever you like.

## Who it's for

It's for people who run many agents side by side (errands, email triage, product work, infrastructure, workers), each
with its own machine or Unix user and permissions, and for people whose work and life share the same agents.

## Where it is today

Runs now: the node, the control plane and the command line; plugins for herdr, Claude Code, session summaries, Claude
accounts and pages; everything on one laptop with `fleetlayer up`; access rules with an audit log; releases that
update every node themselves.

Being built: one sign-in for every service, the phone, moving a piece with one command, and a server setup an agent
can build for you. Expect breaking changes.
