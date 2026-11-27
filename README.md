# Fleetlayer

Fleetlayer connects the agent sessions you already run, on every machine you use, into one fleet.

It sits one level above session managers like [herdr](https://herdr.dev), T3 Code, Conductor or Cursor. It doesn't
replace them or hide your sessions: you keep using their apps and terminals. Fleetlayer adds what none of them do
across machines and tools:

- **One view of every machine and session**, with summaries of what each agent has been doing.
- **Messages between sessions**, on the same machine or across machines.
- **Webhooks routed to the session that owns the work**: a CI failure goes to the agent that wrote the branch, not to
  a fresh agent with no context.
- **A runner** that carries out typed actions on your own device without spending AI tokens, and wakes an agent only
  when a decision needs judgment.
- **Shared Claude sign-ins and usage across all your parallel agents**, through a service you run yourself. Your Claude
  credentials never leave machines you control.

Fleetlayer is a small set of components you run, and a large set of guides on plugging them into what you already use.

**No vendor lock-in.** Keep using Anthropic's and OpenAI's own tools and features for as long as they serve you. When
you outgrow them (several subscriptions, several machines, several accounts, agents from more than one vendor), you
don't have to give anything up: Fleetlayer's parts work with any agent and any model, run on machines you control, and
speak open protocols.

## Status

Early. Being built in public, starting from parts of [Quietdeck](https://quietdeck.ai) that already run one person's
fleet of agents. It runs, from source: see [docs/getting-started.md](docs/getting-started.md). Expect breaking changes.

## The parts

| Part | What it does |
|---|---|
| `fleetlayer-node` | One daemon per machine (or per Unix user). Starts plugins, delivers messages into sessions, keeps one outbound connection to the control plane. |
| `fleetlayer-herdr`, `fleetlayer-claude-code` | The first plugins: herdr as the session manager, and delivery through Claude Code's own messaging socket. |
| Plugins | Separate processes the node talks to with JSON-RPC over stdio, in any language: adapters for session managers (herdr first), event sources, runner actions. The protocol is the [Agent Client Protocol](https://agentclientprotocol.com) plus Fleetlayer's extensions ([docs/protocol.md](docs/protocol.md)). |
| `fleetlayer-control` | The control plane: machines, sessions, messages, channels and a public webhook endpoint. Self-host it (a hosted one is planned). It never holds credentials. |
| `fleetlayer` | The command line. |
| `claude-auth-manager` (planned) | A plugin you run on your own machine: Claude sign-ins, usage across accounts, and year-long tokens handed to every agent. |

## Licence

Apache 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
