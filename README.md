# Fleetlayer

Every agent on every machine, in one fleet.

Fleetlayer connects and controls the agent sessions you already run, on your laptop, a home server or a VPS. See every
session in one list, send messages between them, and route a CI failure to the agent that wrote the branch. It attaches
to your session manager ([herdr](https://herdr.dev) first) without taking it over, and runs on machines you control.

**Start small, replace any component as you go.** Begin with everything on your laptop, and move a piece to a home
server or a VPS when you outgrow it, or swap it for the tool you'd rather use.

**No vendor lock-in.** Keep using Anthropic's and OpenAI's own features for as long as they serve you. When you
outgrow them, you give nothing up: Fleetlayer works with any agent and any model, and when a vendor ships its own
version of a feature, Fleetlayer can support it too.

**Docs: [fleetlayer.ai](https://fleetlayer.ai)** · [Getting started](https://fleetlayer.ai/getting-started/) ·
[Guides](https://fleetlayer.ai/guides/laptop/) ·
[Plugin directory](https://fleetlayer.ai/directory/) · for agents: [llms.txt](https://fleetlayer.ai/llms.txt)

## Status

Early, built in public, and running one person's fleet of agents every day. Expect breaking changes. Releases and
their notes are on the [releases page](https://github.com/fleetlayer/fleetlayer/releases).

## The parts

| Part | What it does |
|---|---|
| `fleetlayer-node` | One daemon per machine (or per Unix user). Starts plugins, delivers messages into sessions, keeps one outbound connection to the control plane. |
| Plugins | Separate processes the node talks to over JSON-RPC on stdio, in any language: herdr, Claude Code, summaries, Claude accounts, pages. The protocol is the [Agent Client Protocol](https://agentclientprotocol.com) plus Fleetlayer's extensions ([docs/protocol.md](docs/protocol.md)). |
| `fleetlayer-control` | The control plane: machines, sessions, messages, channels, webhooks and access rules. Self-host it. It never holds credentials. |
| `fleetlayer` | The command line. |

## This repo

The docs live in [`docs/`](docs/), plain Markdown that reads fine on GitHub; [`site/`](site/) builds fleetlayer.ai
from them on every push to main. Work on Fleetlayer is described in [AGENTS.md](AGENTS.md).

## Licence

Apache 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
