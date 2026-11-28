# Harnesses: Claude Code, Codex, pi, OpenCode, Gemini CLI, Cursor, Aider

A harness is the agent program in a session: Claude Code, Codex, pi and the rest. Fleetlayer works with a session in
two layers:

- **The session manager** (herdr, tmux, Rex) knows where the session runs. Through it Fleetlayer lists the session
  with its status, opens and closes tabs, types into the terminal and shows the terminal elsewhere. That part works
  for every agent your session manager recognises.
- **A harness plugin** knows the agent from the inside. Through it a message arrives through the agent's own API
  instead of being typed into the person's terminal, Fleetlayer can read the transcript, see what the session waits on
  (a permission prompt, a question), and start or resume sessions with the right flags.

## What works with each

| Harness | Messages arrive | Status | Waiting on a person | Transcript | Start and resume | How |
|---|---|---|---|---|---|---|
| **Claude Code** | its own messaging channel | yes | permission prompts, questions and plans, answered from anywhere | yes, with subagents | yes, `--model`, `--effort` | `fleetlayer-claude-code`, Claude Code's hooks |
| **Codex** | typed in | yes | from the screen | summaries only | yes, `--model`, `--effort` | the session manager |
| **pi** | its own API (`sendUserMessage`), steered into a running turn | yes | an extension's dialog | yes | yes, `--model`, `--effort` (pi's thinking level) | `fleetlayer-harness pi`, a pi extension |
| **OpenCode** | its own API (`session.promptAsync`), shown in the TUI | yes | permissions and questions, with the command | yes | yes, `--model` | `fleetlayer-harness opencode`, an OpenCode plugin |
| **Gemini CLI** | typed in, where herdr recognises it ([below](#how-each-is-reached)) | yes | permission prompts, with the command | yes | yes, `--model` | `fleetlayer-harness gemini`, Gemini CLI's hooks |
| **Cursor's CLI** | typed in | yes | from the screen | yes | yes, `--model` | `fleetlayer-harness cursor`, Cursor's hooks |
| **oh-my-pi** | typed in | yes | from the screen | no | yes, `--model`, `--effort` | the session manager |
| **Aider** | not yet | not yet | not yet | not yet | not yet | see [below](#aider) |

"Status" is working, idle, done or blocked, from the session manager (herdr reads it off the screen or from the
agent's own integration). Answering a blocked prompt from elsewhere (`fleetlayer session answer`) works for Claude
Code today; for the others you see what it waits on and answer in the terminal, or from anywhere with `fleetlayer
attach`.

Any agent that can run a shell command can also use Fleetlayer itself, whatever its harness: `fleetlayer msg send`,
`fleetlayer mcp browser@laptop`, `fleetlayer secrets run`, pages. The table is about what Fleetlayer can see and do *to*
a session.

## Setting them up

`fleetlayer up` adds a plugin for each harness it finds installed. To add one by hand, put it in
`~/.config/fleetlayer/node.yaml` and restart the node:

```yaml
plugins:
  - name: pi
    command: fleetlayer-harness
    args: [pi]
  - name: opencode
    command: fleetlayer-harness
    args: [opencode]
  - name: gemini
    command: fleetlayer-harness
    args: [gemini]
  - name: cursor
    command: fleetlayer-harness
    args: [cursor]
```

Each is its own plugin process; add only the harnesses you use. What each one installs, and how to take it out
again, is in [the harness plugins](plugins/harness.md). Start a session with one:

```sh
fleetlayer session start --tab review --cwd ~/src/app --harness opencode --model anthropic/claude-sonnet-5-5 --prompt "…"
fleetlayer session start --tab notes --cwd ~/src/app --harness pi --model sonnet --effort high
fleetlayer session resume <session-id>      # finds the harness and folder itself
```

The node adds the arguments you give each harness in `harnesses:` in `node.yaml` (a permission mode, say), then the
session's name and the session to resume. With herdr, the agent must be one herdr can start (`herdr tab create --agent
<harness>`).

## How each is reached

**pi.** The plugin installs a small extension (`~/.pi/agent/extensions/fleetlayer.ts`). Inside each pi process it
opens a Unix socket that only your user can use, and the plugin delivers a message through pi's
`sendUserMessage`: as a new turn when pi is idle, steered into the turn when it's working. It also reports when an
extension's dialog waits for an answer. Transcripts come from pi's own session files.

**OpenCode.** The plugin installs an OpenCode plugin (`~/.config/opencode/plugins/fleetlayer.js`) that does the same
inside each OpenCode process, through OpenCode's own `session.promptAsync`, so the message appears in the TUI like any
other. It reports permission requests (with the command) and questions as they happen. OpenCode keeps sessions in
SQLite; the plugin reads them with `opencode export`. Tested with OpenCode 1.19; OpenCode 2 (in beta) has a new plugin
API that the shim doesn't speak yet.

**Gemini CLI.** Gemini CLI has hooks like Claude Code's, and the plugin adds a few to `~/.gemini/settings.json`:
they report each session, its transcript and its permission prompts. Gemini CLI takes no message into a running
session from outside, so the node types messages in. herdr doesn't recognise Gemini CLI's process on every system
(it runs as `node`), and then shows no agent in that pane at all. For such a pane the hooks report the session's state
to herdr through herdr's own reporting API (`herdr pane report-agent`), so the person's sidebar shows it working,
idle or blocked and Fleetlayer lists it under its own ID. herdr still won't type into it then (Gemini CLI runs its
work in a second `node` process, and herdr's `agent prompt` refuses an agent that isn't the pane's foreground
process), so on such a machine a message to a Gemini CLI session fails with herdr's reason instead of arriving.

**Cursor's CLI.** Cursor's hooks (`~/.cursor/hooks.json`) report each session and its transcript. Like Gemini CLI it
takes no message from outside, so messages are typed in. Cursor's CLI is closed source; the plugin uses only its
documented hooks and the files it documents.

### ACP

Fleetlayer's plugin protocol is built on the [Agent Client Protocol](https://agentclientprotocol.com), and OpenCode
(`opencode acp`), Gemini CLI (`gemini --acp`), Cursor's CLI (`agent acp`) and oh-my-pi (`omp acp`) all speak it as
agents. ACP gives a session to the program that started it, over that program's stdin: it can't reach a session a
person runs in their terminal. So ACP fits sessions with no terminal at all, started and driven by the node itself, and
that's coming as one generic ACP harness plugin. Sessions stored that way stay the harness's own: Gemini CLI and
OpenCode resume them in their TUI.

### Aider

Aider has no session IDs, no way to take a message into a running session and no hooks (only a bell or a command
when it wants input), and its last release was in February 2026. herdr doesn't recognise it either. A plugin would
need a wrapper that starts Aider with a chat history file per session and reports its state to the session manager.
Not planned until someone asks.

## Licences

Fleetlayer's plugins use each harness's public interfaces only: its CLI flags, hooks, plugin or extension API and the
files it documents. pi, OpenCode and oh-my-pi are MIT, Gemini CLI and Aider Apache-2.0, Codex Apache-2.0; Claude
Code and Cursor's CLI are closed source.
