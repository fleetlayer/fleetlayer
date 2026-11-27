# How Fleetlayer fits together

Fleetlayer is an open framework for running AI agents securely and at scale. It connects the agent sessions you
already run, on every machine you use, into one fleet. This page explains the few pieces it's made of, how they talk
to each other, and the rules that shape them. [Getting started](getting-started.md) and [Start on your
laptop](guides/laptop.md) show how to run them.

## The pieces

- **A node** on each machine, or on each Unix user of a shared machine. It starts the plugins, keeps one outbound
  connection to the control plane, delivers messages into sessions, updates itself from releases, and reports a
  heartbeat. The `fleetlayer` command line talks to the node on its own machine.
- **Plugins**, separate programs the node starts and talks to over JSON-RPC on stdin and stdout. They can be written
  in any language. A plugin takes one or more roles: a session manager knows where sessions run (`fleetlayer-herdr`,
  `fleetlayer-rex` for [Rex](rex.md)); a harness knows one agent program from the inside
  (`fleetlayer-claude-code` delivers through Claude Code's own messaging socket); others turn outside events into Fleetlayer events, apply files or secrets sent to the machine,
  write session summaries, or serve pages. The protocol is the [Agent Client
  Protocol](https://agentclientprotocol.com) plus Fleetlayer's extensions ([the plugin protocol](protocol.md)).
- **The control plane**, one per fleet. It knows every node and session, relays messages between nodes, routes
  webhooks and events to the sessions subscribed to them, and checks every request against the access policy, writing
  each decision to an audit log. It's a single program with a SQLite database, and you run it yourself: on your laptop
  to start, on a home server or a VPS later.
- **The gateway**, on a laptop. It gives each service its own address (`http://pages.localhost:7680`), checks each
  one's health, and tells you or an agent when something stops.

## Names

Every session has a name of the form `<tab>@<node>`: `planning@laptop`, `triage@mail`. The tab is the session's name
in its session manager, and it stays the same when the agent restarts. When a tab name is unique in the fleet, the
`@<node>` part can be left out. Shared tools are `tool:<name>@<node>` (`tool:browser@laptop`), and a script with no
session is `-@<node>`.

**A node is the trust boundary.** Every process of a Unix user can claim to be any of that user's sessions, so a rule
about one agent holds only if that agent has its own Unix user and node. An agent you trust less, one that reads
strangers' email for instance, gets a user of its own ([Run an agent that reads strangers'
text](guides/untrusted-input.md)).

## How a message travels

`fleetlayer msg send --to infra@home "the migration is done"` goes from the sender's node to the control plane, which
checks the access policy and passes it to the node `home`. That node finds the session `infra` and delivers the text
through the best channel it has: the agent's own messaging channel when a harness plugin offers one, otherwise typed
into its terminal by the session manager. The message is labelled with who sent it and how to reply. If the sender
reads untrusted input, the message is also marked as outside data, so the agent that receives it treats it as it would
treat the email itself. A session may always answer a session that wrote to it in the last 24 hours.

Events work the same way. A webhook (a CI result, say) arrives at the control plane on a channel, and goes to the
sessions subscribed to that channel or to one subject on it: the agent that wrote the branch hears that its build
failed. A channel can name a fallback session for events nobody subscribed to.

## Secrets stay on your machines

The control plane holds no credentials. When a node enrols it makes a keypair, and its private key never leaves it.
A plugin that needs to put a secret on other machines (the Claude sign-in hub sending year-long tokens, for example)
encrypts it to each target node's public key. The control plane relays the ciphertext without being able to read it,
and doesn't store it. Only numbers travel the other way: usage, health and session counts, as readings.

## Start small, move pieces later

Each piece keeps all its state in one folder and its settings in one file, and never assumes it shares a machine with
another piece. So you can start with everything on your laptop (`fleetlayer up`), then move the control plane, pages
or the sign-in hub to an always-on machine when you outgrow it with one command (`fleetlayer move pages server`): it
stops the piece, copies its folder sealed and checked, starts it there and points the others at its new address
([Moving a piece](guides/moving.md)). A node keeps its key through a move, so secrets sealed to it stay readable.

## Rules that shape the design

- **Fleetlayer works with your session manager.** It attaches to herdr (and later tmux, T3 Code and others)
  without taking it over. Your own apps and terminals keep working beside it.
- **Plugins are separate processes.** Nothing is compiled into the node, so a plugin can't crash it, and you can write
  one in any language.
- **Open protocols.** ACP for plugins, JSON-RPC over a WebSocket for the node's [link](link.md) to the control plane,
  age keys for sealed secrets, OpenID Connect for sign-in.
- **Everything in the background is monitored.** Every daemon, sync or scheduled job reports a heartbeat or answers a
  health check, and the check fails when it stops doing its job, not only when it stops running.

## What's being built

One sign-in for every service you run, with your company's provider or a simple built-in directory
([Sign-in and SSO](sso.md)); one deployment shared by a household or a team, each person with their own agents and
data; reaching every session from your phone with one pairing; and a server setup an agent can build for you on a fresh
machine.
