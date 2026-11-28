# Fleetlayer and OpenClaw

[OpenClaw](https://docs.openclaw.ai) is an open-source personal AI assistant that runs on your own hardware and works
through the chat apps you already use: Telegram, WhatsApp, Discord, Slack, Signal, iMessage and many more. One
long-lived Gateway process owns your sessions, channels and state, runs the agent, and connects the web Control UI,
the command line and companion apps on macOS, iOS and Android
([Architecture](https://docs.openclaw.ai/concepts/architecture), [Remote access](https://docs.openclaw.ai/gateway/remote)).
It's MIT licensed and run by the OpenClaw Foundation, a non-profit, with no paid tier or hosted service
([GitHub](https://github.com/openclaw/openclaw)).

Fleetlayer connects the agent sessions you already run, Claude Code and Codex in herdr, Rex or tmux, on your laptop, a home
server or a VPS, into one fleet. Any agent can message any other, events and webhooks reach the agent that owns the
work, and each agent works under access rules you set, with approvals and an audit log
([How Fleetlayer fits together](concepts.md)). It's source-available under the Functional Source License, and you run
every piece yourself.

The short version: OpenClaw is one assistant you install and talk to. Fleetlayer gives you the parts to build your own,
from the agent harnesses and session manager you already use, with each part on whichever machine suits it.

Everything this page says about OpenClaw comes from its public documentation, linked beside each claim, as of its
v2026.11.10 release ([Releases](https://docs.openclaw.ai/releases)). Fleetlayer's side is checked against its own docs:
"coming" means it isn't in a release yet.

## Side by side

| | OpenClaw | Fleetlayer |
|---|---|---|
| Chat channels | About 30: Telegram built in, WhatsApp, Discord, Slack, Signal, iMessage, Matrix, Teams and more as plugins, plus WebChat ([Channels](https://docs.openclaw.ai/channels)) | Matrix rooms as channels (coming, in the next release). Telegram is planned |
| A persistent agent you talk to | Yes: the Gateway's agent, reached from any channel or the Control UI ([Architecture](https://docs.openclaw.ai/concepts/architecture)) | A Claude Code or Codex session in herdr, Rex or tmux that stays running, reached by message, terminal, phone or (coming) a Matrix room |
| Scheduled prompts | Built-in automations: one-shot, interval and cron jobs, with output to a chat or a webhook ([Automation](https://docs.openclaw.ai/automation)) | The scheduler plugin: every few hours, a cron expression or once at a set time, to any session ([fleetlayer-scheduler](plugins/scheduler.md)) |
| Multiple agents, agent-to-agent messages | Isolated agents in one Gateway, routed by bindings; agent-to-agent access on by default, restrictable; sub-agents ([Multi-agent](https://docs.openclaw.ai/concepts/multi-agent)) | Any session messages any other across machines, `fleetlayer msg send --to api@laptop`, checked by the access policy ([Watch, steer and message](guides/day-to-day.md)) |
| Skills and tools | Built-in tools (exec, browser, web search, messaging), plugins, `SKILL.md` skills and the ClawHub registry ([Tools](https://docs.openclaw.ai/tools), [Skills](https://docs.openclaw.ai/tools/skills)) | Your harness's own tools and skills; Fleetlayer installs its skills on every machine ([Tell your agents about Fleetlayer](guides/agents-know-fleetlayer.md)) |
| MCP | Both ways: a registry of MCP servers for its agents, and `openclaw mcp serve` ([MCP](https://docs.openclaw.ai/cli/mcp)) | Your harness's own MCP, plus one machine's MCP servers shared with the fleet behind a gate ([The MCP gate](access-control.md#the-mcp-gate)) |
| Shared tools across machines | Nodes on other devices run commands the Gateway asks for (camera, screen, location, system commands) ([Nodes](https://docs.openclaw.ai/nodes)); a remote browser or a node browser proxy ([Browser](https://docs.openclaw.ai/tools/browser)) | Any MCP server shared by name: your signed-in Chrome on your laptop used from a server, your laptop's screenshots and downloads ([Share your browser](guides/browser.md), [Shared folders](guides/shared-folders.md)) |
| Memory and notes | Markdown memory in the workspace (`MEMORY.md`, daily notes) with hybrid search and pluggable backends ([Memory](https://docs.openclaw.ai/concepts/memory)) | Your harness's own memory; summaries of every session, searchable ([Summaries](plugins/summaries.md)); notes and skills synced between machines ([Sync](guides/sync.md)) |
| Running across several machines | One Gateway is the master; nodes and clients on other machines connect to it ([Remote access](https://docs.openclaw.ai/gateway/remote)) | Agents run on every machine; a node on each keeps one outbound connection to your control plane ([Add a machine](guides/node-services.md)) |
| Per-agent permissions | Per-agent tool allow and deny lists and sandbox settings; one trust boundary per Gateway ([Multi-agent](https://docs.openclaw.ai/concepts/multi-agent), [Security](https://docs.openclaw.ai/gateway/security)) | An access policy per agent and role: whom it may message, which sessions it may start or take over, which tools it may call ([Access control](access-control.md)) |
| Sandboxing | Optional tool sandboxes: Docker, Podman, SSH and remote backends, off by default ([Sandboxing](https://docs.openclaw.ai/gateway/sandboxing)) | A separate Unix user and node per role; Fleetlayer doesn't sandbox the harness itself ([An agent that reads strangers' text](guides/untrusted-input.md)) |
| Approvals | Command approvals with allowlists, in the Control UI, the macOS app or some chats ([Exec approvals](https://docs.openclaw.ai/tools/exec-approvals)) | Anything the policy marks sensitive waits for a person, on by default, from the command line or the dashboard (coming) ([Approvals](access-control.md#asking-the-person-approval-and-justification)) |
| Audit log | A metadata ledger of agent runs and tool actions, `openclaw audit` ([Audit](https://docs.openclaw.ai/gateway/audit)) | Every decision recorded before it takes effect, in a log you can check hasn't been altered ([The audit log](access-control.md#the-audit-log)) |
| Secrets | SecretRefs from env, files, a store or a command (1Password and others), opt-in per credential; sentinels swapped for real values at egress ([Secrets](https://docs.openclaw.ai/gateway/secrets)) | Sealed to each machine's key, granted per role, 1Password items without an agent holding a token, `fleetlayer secrets run`, scrubbing of what leaves a machine ([Credentials](credentials.md)) |
| Network egress control | An optional egress proxy for the Gateway's own exec commands, with a host allowlist; it terminates TLS to swap in secrets ([Secret store and egress](https://docs.openclaw.ai/gateway/secrets/secret-store-and-egress)) | A host list per role, through a proxy that reads host names and never decrypts, log mode first, new hosts approved by a person; Linux only, experimental ([Network access per role](egress.md)) |
| Sign-in and SSO | Token or password, Tailscale identity, or a trusted reverse proxy that does the sign-in ([Trusted proxy auth](https://docs.openclaw.ai/gateway/trusted-proxy-auth)) | One sign-in for every service the fleet runs: passkeys, Google Workspace or another OpenID Connect provider (experimental) ([Sign-in](plugins/signin.md)) |
| Web dashboard | The Control UI: chat, sessions, automations, settings and approvals ([Control UI](https://docs.openclaw.ai/web/control-ui)) | Machines, sessions, events and approvals across the fleet (coming, in the next release) ([The dashboard](dashboard.md)) |
| Phone | Your chat apps, and iOS and Android companion nodes ([Platforms](https://docs.openclaw.ai/platforms)) | herdr's phone app showing every machine's sessions ([Every machine on your phone](guides/phone.md)); one pairing for everything is coming |
| Harness and model | Many providers, local models included, a Claude subscription through the Claude CLI or a setup token ([Providers](https://docs.openclaw.ai/providers), [Anthropic](https://docs.openclaw.ai/providers/anthropic)); Claude Code, Codex and others as ACP harnesses ([ACP agents](https://docs.openclaw.ai/tools/acp-agents)) | The harness you already run (Claude Code, Codex, others in your session manager) with whatever model it uses; several Claude accounts shared across machines ([Many Claude accounts](guides/claude-accounts.md)) |
| Where it runs | macOS, Linux, WSL2 and Windows ([Platforms](https://docs.openclaw.ai/platforms)) | Linux and macOS, everything on one laptop or spread over several machines ([Getting started](getting-started.md)) |
| Licence | MIT ([GitHub](https://github.com/openclaw/openclaw)) | Functional Source License (FSL-1.1-ALv2): source-available, each release becoming Apache 2.0 two years after it ships |

## Build your own OpenClaw with Fleetlayer

OpenClaw puts the agent, the channels, the scheduler and the tools in one Gateway. With Fleetlayer each of those is a
separate piece you choose, and each one can run on a different machine. Here's how the pieces map:

- **The agent.** A Claude Code or Codex session in a herdr tab, with a name that stays put (`assistant@server`). It's
  the harness you already use, with its own tools, skills, memory and model. Start it from anywhere with
  `fleetlayer session start --on server --tab assistant` ([Watch, steer and message](guides/day-to-day.md)), and set
  it up to keep working when nobody's watching ([Agents that run while you're away](guides/unattended-agents.md)).
- **Talking to it.** `fleetlayer msg send --to assistant "..."` from any machine, herdr's phone app for every machine's
  sessions ([Every machine on your phone](guides/phone.md)), and `fleetlayer attach` for its terminal. A chat room as
  the way in comes with [fleetlayer-matrix](plugins/matrix.md) in the next release: each message in the room reaches
  the agents subscribed to it, marked as outside data, and they answer as the room's bot. A Telegram plugin is planned.
- **Recurring prompts.** One entry in the [scheduler's](plugins/scheduler.md) settings file ("plan the day from the
  open stories" at 9 on weekdays). It waits for a session that's blocked on a question and never sends a burst of
  catch-up messages after downtime.
- **Events.** CI results, webhooks and alerts go to the agent that owns the work
  ([CI results to the right agent](guides/ci-results.md)).
- **Skills and instructions.** Fleetlayer's skills are installed on every machine
  ([Tell your agents about Fleetlayer](guides/agents-know-fleetlayer.md)), and your own notes, skills and instructions
  stay the same everywhere ([Sync](guides/sync.md)).
- **A browser and other tools.** Your signed-in Chrome on your laptop, used by an agent on your server
  ([Share your browser](guides/browser.md)), and an MCP gateway such as Executor for Gmail, Calendar and the rest, with
  its token on a node no agent can read ([Executor behind Fleetlayer](guides/executor.md)).
- **Secrets per role.** Claude setup tokens, browser cookies and 1Password items delivered sealed to the roles you
  grant them to, and used by reference so the value stays out of the agent's context ([Credentials](credentials.md)).
- **Where it may connect.** A host list per role, built from a week of logged traffic, with new hosts approved by you
  ([Network access per role](egress.md)).
- **Approvals and a record.** The access policy decides whom each agent may message and what it may use, sensitive
  actions wait for you, and every decision goes to the audit log ([Access control](access-control.md)). An agent that
  reads strangers' messages runs as its own user, and what it sends arrives as outside data
  ([An agent that reads strangers' text](guides/untrusted-input.md)).

A setup might look like this: the assistant and the scheduler on a home server that's always on, your browser and
screenshots shared from your laptop, the control plane on a small VPS, and a second agent that reads email on its own
Unix user. You can also start with everything on your laptop (`fleetlayer up`) and move a piece later
([Move a piece to another machine](guides/moving.md)).

## When OpenClaw is the better fit

- **You want one thing to install.** OpenClaw is one Gateway with a guided setup. Fleetlayer is a node, a control
  plane and plugins, plus the harness and session manager you bring.
- **You want to talk to it from your chat apps.** OpenClaw reaches about 30 of them today, Telegram and WhatsApp
  included ([Channels](https://docs.openclaw.ai/channels)). Fleetlayer has Matrix in its next release and nothing
  else yet.
- **You want a model of your choice, local ones included, behind one assistant.** OpenClaw talks to dozens of
  providers directly ([Providers](https://docs.openclaw.ai/providers)). With Fleetlayer the model is whatever your
  harness supports.
- **You want device features.** OpenClaw's companion apps add voice, camera, screen and location on your phone and
  desktop ([GitHub](https://github.com/openclaw/openclaw), [Nodes](https://docs.openclaw.ai/nodes)).
- **You want built-in memory and a large skills registry.** OpenClaw ships both
  ([Memory](https://docs.openclaw.ai/concepts/memory), [ClawHub](https://clawhub.ai)).
- **You need an MIT licence.**

Fleetlayer suits you better when you already work in Claude Code or Codex sessions and want to keep them, when your
agents are spread over several machines or Unix users, or when you want access rules, approvals and secrets per role
across all of them. The two can also run side by side: an OpenClaw Gateway on one machine and your Fleetlayer fleet
beside it.

To start with Fleetlayer, see [Getting started](getting-started.md).
