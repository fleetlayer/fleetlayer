# fleetlayer-harness: pi, OpenCode, Gemini CLI and Cursor

The harness plugins for pi, OpenCode, Gemini CLI and Cursor's CLI: one binary, run as one plugin per harness
(`fleetlayer-harness pi`, `… opencode`, `… gemini`, `… cursor`). What each harness can do with Fleetlayer is in
[Harnesses](../harnesses.md); this page is what the plugins install, keep and read.

```yaml
plugins:
  - name: opencode
    command: fleetlayer-harness
    args: [opencode]
    config:                  # all optional
      dir: ~/.config/opencode    # the harness's own folder (defaults below)
      install: true              # false takes the shim or hooks out again
      program: opencode          # OpenCode only: the command that exports a session
```

## What each installs

| Harness | Installs | Default folder |
|---|---|---|
| pi | `extensions/fleetlayer.ts`, a pi extension | `~/.pi/agent` (or `$PI_CODING_AGENT_DIR`) |
| OpenCode | `plugins/fleetlayer.js`, an OpenCode plugin | `~/.config/opencode` |
| Gemini CLI | hooks in `settings.json` (`SessionStart`, `SessionEnd`, `BeforeAgent`, `AfterAgent`, `Notification`, `AfterTool`) | `~/.gemini` |
| Cursor | hooks in `hooks.json` (`sessionStart`, `sessionEnd`, `beforeSubmitPrompt`, `stop`) | `~/.cursor` (or `$CURSOR_CONFIG_DIR`) |

It installs only where the harness's folder exists (the harness has run for that user), checks again every five
minutes, and puts its file or hooks back if something rewrites them. Every other setting and hook stays as it was;
files are replaced in one rename. A settings file that isn't plain JSON (Gemini CLI allows comments) is left alone,
and the plugin reports it as its problem (`plugin.status`, shown in `fleetlayer status`). The pi and OpenCode shims
take effect in agents started after they're installed.

`install: false` and a restart of the node take them out again.

## How it talks to the agents

Everything stays on the machine, under `~/.local/state/fleetlayer/harness/<harness>/` (0700):

- **pi and OpenCode:** each agent process writes `<pid>.json` (its sessions, its folder, the pane it runs in) and
  listens on `<pid>.sock` (0600). The plugin delivers a message there (`{"type":"deliver","sessionId","text"}`) and
  follows what each session waits on (`{"type":"watch"}`). A registration whose process has gone is removed.
- **Gemini CLI and Cursor:** each hook runs `fleetlayer-harness hook <harness>`, which adds the agent's process ID
  and pane to the event and passes it to `hooks.sock`, then exits 0 with no output (Cursor gets the empty answer it
  expects). In a herdr pane, Gemini CLI's hook also reports the session's state to herdr (`herdr pane report-agent
  --source fleetlayer:gemini`, released when the session ends), since herdr doesn't recognise Gemini CLI's process
  everywhere. It never blocks or changes what the agent does, and does nothing when the plugin isn't running. The
  sessions seen are kept in `sessions.json`, and a session whose agent has exited is dropped.

Any process of the same Unix user can reach those sockets, as it can reach the agents' own files: the node's Unix user
is the trust boundary ([concepts](../concepts.md)).

## Transcripts

Each plugin writes a copy of a session's transcript in `transcripts/<session>.jsonl`, in Fleetlayer's
`fleetlayer-jsonl` format: Claude Code's JSONL shape, cut down to user and assistant lines with text, tool calls and
tool results (a result past 16 KB is cut). So `fleetlayer transcript`, transcript search and the phone's conversation
view read every harness the same way. The copy is written again when the harness's own file is newer; OpenCode's
comes from `opencode export` (at most once every five seconds). Search covers the newest 300 sessions of each harness.

## Starting sessions

- **Getting a folder ready** (`harness/prepare`, right before a session opens there with nobody watching): pi gets the
  folder in `~/.pi/agent/trust.json` (`"<folder>": true`, so pi loads the project's own extensions and settings
  without asking), Gemini CLI in `~/.gemini/trustedFolders.json` (`"<folder>": "TRUST_FOLDER"`), Cursor a
  `.workspace-trusted` file in `~/.cursor/projects/<folder as a slug>/`. OpenCode doesn't ask. Both pi's and Gemini
  CLI's trust cover every folder below, so for them the plugin refuses `/`, the home folder and any folder above it:
  start a session in a project's own folder. Each folder it trusts is recorded in `trusted.json` in its state folder;
  a start that fails takes that folder's entry back, and `install: false` takes back every entry it added. A folder
  you had trusted yourself is never touched. To remove one by hand, delete its line from those files.
- **Resuming by ID** (`harness/locate`): pi's session file names the folder, OpenCode's export does, Gemini CLI's
  hooks and its `.project_root` file do, Cursor's hooks do.

## Limits

- Answering a blocked prompt from elsewhere (`fleetlayer session answer`) is Claude Code's only, for now: these
  plugins report the prompt, and you answer it in the terminal.
- OpenCode 2 (beta) has a new plugin API; the shim is written for OpenCode 1.
- Where herdr doesn't recognise Gemini CLI (seen on Linux), messages to Gemini CLI sessions fail: the session is
  listed, with its status and prompts, but herdr won't type into it ([Harnesses](../harnesses.md#how-each-is-reached)).
- Cursor's transcript format isn't documented: lines that don't look like a user or assistant turn are left out of
  the copy.
- Subagents' transcripts aren't listed.
