# Fleetlayer

Claude Code is awesome, until you add a second machine or account.

Fleetlayer connects the Claude Code and Codex sessions on your laptop, home server and VPS, under every account you
use. They message each other, share your signed-in browser and files, and work under rules you set, on machines you
control. Source-available (FSL-1.1-ALv2), early, built in public: https://github.com/fleetlayer/fleetlayer

## What starts going wrong

One Claude account on one machine is easy. Add a second account, a server, or another kind of agent, and this starts
happening:

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
- **Your own accounts, on your own machines.** Year-long sign-ins for each of your Claude accounts on every machine you run, and each account's usage visible to every session. [How](https://fleetlayer.ai/docs/guides/claude-accounts/)


## You need this if

- your agents run on more than one machine, or under more than one Claude account;
- you run Codex or other agents next to Claude Code;
- an agent reads email or the web, and you want it kept away from the others' rights and secrets;
- you want your agents, secrets and the record of what they did on machines you control.

You probably don't if you use one Claude account on one laptop and Claude Code's own features cover the rest. Use
those, and come back when you add a second machine or account:
https://fleetlayer.ai/docs/claude-code/


## Your whole fleet on one page

The dashboard shows every machine and what it runs, every agent session and whether it's waiting on you, the events
that arrived and which agent each reached, and the requests waiting for your approval. Live, behind your own sign-in,
and everything on it works from the command line too. Screenshot: https://fleetlayer.ai/img/dashboard-light.webp.

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
- **Your own accounts, on your own machines**: each of your Claude accounts' usage, visible to every session; year-long
  tokens reach each machine sealed to its own key; sign-ins stay on a machine you control.
- **Separate users, separate rules**: run several nodes and herdr instances on the same machine or VM, each under its
  own Unix user. The access policy limits whom each agent may message and what it may use.
- **Tools that live on one machine**: your signed-in browser on a laptop, used by an agent on a server with
  `fleetlayer mcp browser@laptop`.

## All the security, none of the pain

Most people run Claude Code with access to everything and hope nothing goes wrong. That works until an agent reads a
malicious email, or a stray `rm -rf` runs in the wrong folder. Fleetlayer gives you the protections a security team
would ask for, an agent sets them up for you, and each one can be switched off when you don't need it.

- **Ask before the risky things.** Access rules decide which agent may message, start or take over which session.
  Anything sensitive waits for your approval, and you can leave approvals off for the work you trust.
- **Everything on the record.** An audit log shows who did what and when, and you can check it hasn't been altered.
- **Secrets stay on your machines.** Tokens travel encrypted to the machine that needs them, and the control plane
  never stores them.
- **Untrusted input stays untrusted.** Messages from an agent that reads email or the web arrive marked as outside
  data. That agent runs as its own user and can't borrow a trusted agent's rights.
- **One sign-in, one sign-out.** One sign-in for your self-hosted services, and signing out of one signs you out of
  all of them.
- **Secrets per role.** Items from 1Password reach only the roles you grant them to, and are scrubbed from everything
  that leaves a machine. https://fleetlayer.ai/docs/credentials/
- **Agents reach only the hosts they need.** Each role gets a list of hosts, and you approve new ones. Linux for now.
  https://fleetlayer.ai/docs/guides/egress/
- **An admin role that's off until you start a timed window.** The agent that can change everything has no rights
  until then, and is cleaned up when the window ends. https://fleetlayer.ai/docs/guides/admin-window/

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

A person running many agents across several machines and accounts: errands, email triage, product work,
infrastructure, workers, each with its own machine or Unix user and permissions. It grows with you: a household or a
team shares one system, and a company gets what it needs (single sign-on, teams, separate organisations, audit) when
it needs it.

## Features

- Every session on every machine in one list, with what each agent is doing: https://fleetlayer.ai/docs/guides/day-to-day/
- Messages between any two agents, Claude Code or Codex, on any machine: https://fleetlayer.ai/docs/guides/day-to-day/
- Sessions started, resumed, answered and handed off on any machine: https://fleetlayer.ai/docs/guides/day-to-day/
- A summary of each hour of every session: https://fleetlayer.ai/docs/plugins/summaries/
- CI results to the agent that pushed, through webhooks: https://fleetlayer.ai/docs/guides/ci-results/
- The dashboard: machines, sessions, events and approvals, live: https://fleetlayer.ai/docs/dashboard/
- Every machine on your phone, through one herdr relay: https://fleetlayer.ai/docs/guides/phone/
- Your own Claude accounts on every machine, with each account's usage: https://fleetlayer.ai/docs/guides/claude-accounts/
- Your signed-in browser, for agents on any machine: https://fleetlayer.ai/docs/guides/browser/
- Your Mac's screenshots and downloads, for agents anywhere: https://fleetlayer.ai/docs/guides/shared-folders/
- Approved commands that run on another machine: https://fleetlayer.ai/docs/guides/shared-commands/
- Executor's MCP tools, with each call checked by your policy: https://fleetlayer.ai/docs/guides/executor/
- Pages and apps your agents publish, behind one sign-in: https://fleetlayer.ai/docs/guides/pages/
- Prompts sent to a session on a schedule: https://fleetlayer.ai/docs/plugins/scheduler/
- Matrix rooms as channels agents hear and answer: https://fleetlayer.ai/docs/plugins/matrix/
- Leases, so many agents can share a few build machines: https://fleetlayer.ai/docs/guides/shared-build-machines/
- Notes, skills and instructions kept the same on every machine: https://fleetlayer.ai/docs/guides/sync/
- Access rules, approvals and a tamper-evident audit log: https://fleetlayer.ai/docs/access-control/
- Agents that read strangers' text, kept in a Unix user of their own: https://fleetlayer.ai/docs/guides/untrusted-input/
- Secrets per role from 1Password, scrubbed from what leaves a machine: https://fleetlayer.ai/docs/credentials/
- Network access per role, with approvals for new hosts (Linux): https://fleetlayer.ai/docs/guides/egress/
- An admin role that's off until you start a timed window: https://fleetlayer.ai/docs/guides/admin-window/
- One passkey sign-in for your services, with single sign-on and teams: https://fleetlayer.ai/docs/guides/sign-in/
- Your browser's cookies for command-line tools on other machines: https://fleetlayer.ai/docs/plugins/cookies/
- herdr and Rex as session managers: https://fleetlayer.ai/docs/rex/
- Everything on one laptop with fleetlayer up: https://fleetlayer.ai/docs/guides/laptop/
- Any piece moved to another machine with one command: https://fleetlayer.ai/docs/guides/moving/
- Signed releases that update every node themselves: https://fleetlayer.ai/docs/releasing/
- Heartbeats and health checks for everything in the background: https://fleetlayer.ai/docs/guides/monitoring/
- A production-grade server set up by an agent, from an example: https://fleetlayer.ai/docs/guides/server-setup/

Coming soon:

- tmux as a session manager (v0.4.2)
- More harnesses: pi, OpenCode, Gemini CLI and Cursor's CLI (v0.4.2), then Aider: https://fleetlayer.ai/docs/harnesses/
- SAML sign-in providers

Expect breaking changes: what changed in each release is in the changelog, https://fleetlayer.ai/docs/changelog/

Will it cost money? We might provide a hosted control plane and provision a VPS where you can run your remote agents
and services, but we haven't decided on anything yet.
