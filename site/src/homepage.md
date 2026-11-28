# Fleetlayer

An open framework for running AI agents securely and at scale.

Fleetlayer connects the Claude Code and Codex sessions you already run in [herdr](https://herdr.dev/), on your laptop, a
home server or a VPS. Any agent can message any other agent, webhooks can be routed to specific agents, and each agent
gets its own access rules. Source-available, early, built in public:
https://github.com/fleetlayer/fleetlayer

## Your whole fleet on one page

The dashboard shows every machine and what it runs, every agent session and whether it's waiting on you, the events
that arrived and which agent each reached, and the requests waiting for your approval. Live, behind your own sign-in,
and everything on it works from the command line too. Screenshot: https://fleetlayer.ai/img/dashboard-light.webp.

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

- **Fleetlayer Pages and Apps.** Claude, Codex and Gemini agents write pages and interactive apps as plain folders, and Fleetlayer hosts them behind one sign-in. [How](https://fleetlayer.ai/docs/guides/pages/)
- **Messages between any two agents.** Claude or Codex, on the same machine or another one: `fleetlayer msg send --to api@laptop`. [How](https://fleetlayer.ai/docs/guides/day-to-day/)
- **Any agent can control your browser.** Agents on any machine and any Claude account can use the Chrome on your Mac, signed in as you (via Playwright MCP). [How](https://fleetlayer.ai/docs/guides/browser/)
- **Every account in one place.** Year-long sign-ins on every machine, and every account's usage shared with every session. [How](https://fleetlayer.ai/docs/guides/claude-accounts/)

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

## All the security, none of the pain

Most people run Claude Code with every token in their shell and hope nothing goes wrong. That works until an agent
reads a malicious email, or a stray `rm -rf` runs in the wrong folder. Fleetlayer gives you the protections a security
team would ask for, an agent sets them up for you, and each one can be switched off when you don't need it.

- **Ask before the risky things.** Access rules decide which agent may message, start or take over which session.
  Anything sensitive waits for your approval, and you can leave approvals off for the work you trust.
- **Everything on the record.** An audit log shows who did what and when, and you can check it hasn't been altered.
- **Secrets stay on your machines.** Tokens travel encrypted to the machine that needs them, and the control plane
  never stores them.
- **Untrusted input stays untrusted.** Messages from an agent that reads email or the web arrive marked as outside
  data. That agent runs as its own user and can't borrow a trusted agent's rights.
- **One sign-in, one sign-out.** One sign-in for your self-hosted services (experimental), and signing out of one
  signs you out of all of them.

[Coming next](https://fleetlayer.ai/docs/credentials.md): secrets granted by role and scrubbed from everything that leaves a machine; an allowlist of where agents
may connect, with you approving new hosts; an admin role that stays switched off until you open a time-limited window.

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
- **Backups:** restic (https://fleetlayer.ai/docs/guides/backups.md)
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

Next: https://fleetlayer.ai/docs/guides/laptop.md

## No vendor lock-in

Your own stack instead of a vendor's cloud, made of separate pieces that work with the tools you already use. Keep
using Anthropic's and OpenAI's own features for as long as they serve you; when you outgrow them, you give nothing up,
and you can still change models and harnesses whenever you like.

## Who it's for

It's for people who run many agents side by side (errands, email triage, product work, infrastructure, workers), each
with its own machine or Unix user and permissions, and for people whose work and life share the same agents.

## Where it is today

Runs now: the node, the control plane and the command line; plugins for herdr, Claude Code, session summaries, Claude
accounts and pages; one sign-in, with a passkey, for every service; everything on one laptop with `fleetlayer up`;
access rules with an audit log; releases that update every node themselves; a whole server setup an agent can build
for you, from [an example repository](https://github.com/fleetlayer/server-iac-example).

Being built: the phone, and moving a piece with one command. Expect breaking changes.
