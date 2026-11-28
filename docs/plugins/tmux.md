# fleetlayer-tmux

The session manager plugin for [tmux](https://github.com/tmux/tmux). It puts the Claude Code, Codex and other agents
running in your tmux panes in the fleet: with their status, messages and webhooks, blocked prompts answered from
anywhere, sessions started and closed from anywhere in the fleet, and `fleetlayer attach` on their panes.

It uses only tmux's own command line (`list-panes`, `capture-pane`, `send-keys`, `paste-buffer`, `new-window -d`) and
never attaches a tmux client, so your tmux keeps working exactly as before: no extra client in `list-clients`, no
window resized to fit Fleetlayer, nothing in your status line, no layout or name changed, and no window taking focus.

## Setup

Install tmux (`apt install tmux`, `brew install tmux`), then add the plugin to `node.yaml`. `fleetlayer up` adds it for
you on a fresh setup when it finds tmux.

```yaml
plugins:
  - name: tmux
    command: fleetlayer-tmux
    config:
      program: /usr/bin/tmux        # the full path, so a service PATH can't hide it (default: tmux)
      # socket: work                # a named server, as `tmux -L work` (default: your default server)
      # socket_path: /run/t.sock    # or a socket path, as `tmux -S` takes it
      # session: fleetlayer         # the tmux session new agents open in, made when it isn't there
      # shell: /bin/zsh             # the login shell new agents start through (default: $SHELL)
      # claude_dir: ~/.claude       # where Claude Code keeps its session registry
```

The node runs as a service, so it reaches the tmux server of its own Unix user at tmux's default socket
(`/tmp/tmux-<uid>/default`, or under `$TMUX_TMPDIR` when you set one). If you start tmux with `-L` or `-S`, give the
plugin the same `socket` or `socket_path`. No tmux server running is fine: the plugin lists no sessions until one
starts, and it never starts one except to open a session you asked for.

Run it beside the Claude Code plugin (`fleetlayer-claude-code`): messages to Claude Code then arrive through Claude
Code's own messaging channel, labelled with who sent them, and its hooks report what a blocked session is asking. This
plugin types a message in only for agents with no channel of their own (Codex, for instance).

## What it does

- **Sessions.** A pane whose process, or a process under it, is an agent (`claude`, `codex`, `opencode`, `gemini`,
  `aider` and others) is a session: the nearest one, so a `claude -p` a tool call started isn't a second session.
  Agents under another session manager inside a pane (herdr run in tmux) are left to that manager's plugin, so nothing
  is listed twice. A Claude Code session is known by its own session ID, from Claude Code's session registry; a Codex
  session by the ID in the transcript it has open (on Linux); other agents go by their pane (`@p12` for `%12`).
- **Names.** A session's name in the fleet (`planning@laptop`) is the name you gave the Claude Code session
  (`claude --name planning`, or `/rename`); else the window's name when you named it (`rename-window`, `new-window
  -n`); else the tmux session's name, or `<session>:<window>` when that session runs several agents. Two agents in one
  window add `.<pane>`, as tmux's own targets do. Its workspace is the tmux session.
- **Status.** Claude Code writes its state to its session registry: `working`, `idle`, and `blocked` while it waits
  on a person (a permission prompt, a question, a dialog). Other agents are read off the screen: `working` while they
  offer "esc to interrupt", `blocked` on Codex's approval menus, otherwise `idle`; an agent whose screen the plugin
  doesn't know is only ever `working` or `unknown`, so nothing is typed into a screen it can't read. A turn that
  finishes while nobody can see the pane (its session isn't attached, or another window is showing) is `done` until
  someone looks. The plugin reads tmux once a second (tmux has no event stream that doesn't attach a client).
- **Messages.** Pasted into the pane through a tmux buffer of the plugin's own (never your paste buffers), as a
  bracketed paste so a message of several lines stays one message, then Enter. Nothing is typed into an agent that's
  blocked on a person, a pane someone is scrolling in (copy mode), or a pane whose agent has exited back to a shell.
- **Blocked prompts and commands.** The node answers a prompt (`fleetlayer session answer`), types an allowed slash
  command (`fleetlayer session command`) or what a person said from the dashboard by reading the pane's screen and
  pressing keys in it, with the same checks as everywhere: only into the prompt that's showing, only into an empty
  input line.
- **Starting and closing.** `fleetlayer session start` opens a new window in the `session` tmux session (made,
  detached, when it isn't there) with `new-window -d`, so no client switches to it. The agent starts through your
  login shell, so it finds what's on your PATH, with its arguments passed as they are, never spliced into shell text.
  The plugin waits until the agent is ready (up to a minute), then types the first prompt. Closing a session closes
  its pane, and its window when that was the last pane. Interrupting presses Esc.
- **Terminals.** `fleetlayer attach` reads the pane with `capture-pane -e` a few times a second and sends the screen
  when it changed, with the cursor where the pane has it. `--observe` only watches (and reads scrollback); typing goes
  in with `send-keys`. The pane keeps its size in your layout in every mode: a resize from the attaching terminal is
  ignored, and the frames carry the pane's real size.
- **Who's watching.** A session shows `humanAttached` when a client has its tmux session open.

## Security

It sees every pane on its user's tmux server (session and window names, folders, the process tree under each pane, the
screen when you attach or the node reads a prompt) and types into a pane only to deliver a message or a first prompt,
answer a prompt, type an allowed slash command or a person's answer from the dashboard, press Esc to interrupt, or
when someone attaches without `--observe`, all under the access policy. It runs `tmux` and `ps` with argv, never
through a shell. Anyone who can run commands as the node's Unix user can drive that user's tmux already: the plugin
adds no access of its own.

## Testing it

`cargo test -p fleetlayer-tmux` runs the plugin against a real tmux: a private server (its own socket, no
`~/.tmux.conf`, a throwaway HOME), agents that are a fake `claude` writing Claude Code's session registry, and the
plugin driven over JSON-RPC as the node drives it. It checks listing, names and status, delivery, `done`, a refused
message to a blocked agent and to a pane in copy mode, answering a prompt with keys, `fleetlayer attach` through the
terminal helper, opening a session with a first prompt, closing it, an agent exiting back to its shell, and that the
person's own session gained no client or window. It's skipped where tmux isn't installed, except in CI.
