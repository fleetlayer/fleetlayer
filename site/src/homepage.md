# Fleetlayer

Every agent on every machine, in one fleet.

Fleetlayer connects the agent sessions you already run, on your laptop, a home server or a VPS. See every session in
one list, send messages between them, and route a CI failure to the agent that wrote the branch. It runs on machines
you control. Open source under Apache 2.0, early, built in public: https://github.com/fleetlayer/fleetlayer

## How it fits

A node on each machine, or each Unix user, keeps one outbound connection to the control plane, so it works behind NAT
with no ports to open. It works with herdr, with tmux and T3 Code to come, and doesn't replace them: your apps and
terminals keep working beside it. Plugins are separate processes in any language, speaking the Agent Client Protocol
plus Fleetlayer's extensions.

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
- **Rules for agents you trust less**: an agent that reads strangers' email gets its own Unix user, and the access
  policy limits whom it may message and what it may use. Every refusal names the rule.
- **Tools that live on one machine**: your signed-in browser on a laptop, used by an agent on a server with
  `fleetlayer mcp browser@laptop`.

## Works with

The tools the author's own fleet runs on, and what Fleetlayer does with each:

- **herdr** (https://herdr.dev): runs the sessions; Fleetlayer lists them, delivers messages into them and attaches to
  their terminals.
- **herdr relay** (coming): herdr on your phone through Fleetlayer's one connection, so you pair once and reach every
  machine.
- **Claude Code**: messages through its own channel, shared accounts and usage, year-long tokens on every machine.
- **Codex**: its sessions in herdr are listed, get messages (typed in through herdr), and can be watched or driven
  with `fleetlayer attach`. No Codex plugin yet.
- **Playwright MCP**: your signed-in browser on the laptop, shared with agents on other machines, limited to the
  callers, networks and tools you allow.
- **Syncthing, or any sync**: pages are plain folders; Fleetlayer hosts whatever your sync keeps in step.
- **Gatus and Traefik**: on a server, Fleetlayer writes their config, a health check and an address for every service.
- **restic** (a guide, not built in): https://fleetlayer.ai/guides/backups.md sets it up for every machine in a fleet.

## Start small, replace any component as you go

Begin with everything on your laptop. When you outgrow it, move one piece to a home server or a VPS, or swap it for
the tool you'd rather use. Each piece keeps its state in one folder and speaks open protocols.

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
