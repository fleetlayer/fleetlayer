# fleetlayer-rex

The session manager plugin for [Rex](https://www.superlogical.com/rex/docs), Superlogical's terminal and session
manager. It puts the Claude Code, Codex and other agents running in your Rex terminals in the fleet: with their
status, messages and webhooks, sessions started and closed from anywhere in the fleet, and `fleetlayer attach` on
their terminals. [Fleetlayer and Rex](../rex.md) explains how the two fit.

It uses only Rex's public command line and server API (`rex api call`, `rex events`, `rex new`), and never takes a
session over: you keep using Rex's apps on your Mac and phone as before.

## Setup

Install Rex ([Superlogical's install docs](https://www.superlogical.com/rex/docs/installation)), then add the plugin to
`node.yaml`. `fleetlayer up` adds it for you on a fresh setup when it finds Rex.

```yaml
plugins:
  - name: rex
    command: fleetlayer-rex
    config:
      program: /usr/local/bin/rex   # the full path, so a service PATH can't hide it (default: rex)
      # server: ssh://build.example.com  # another Rex server, as `rex -S` takes it (default: your own)
      # claude_dir: ~/.claude            # where Claude Code keeps its session registry
```

The plugin talks to the Rex server of the node's Unix user, and never starts one: a Rex server that isn't running
shows as a failing plugin in `fleetlayer status` until it is.

Run it beside the Claude Code plugin (`fleetlayer-claude-code`): messages to Claude Code then arrive through Claude
Code's own messaging channel, labelled with who sent them, and this plugin types a message in only for agents that
have no channel of their own (Codex, for instance).

## What it does

- **Sessions.** Each terminal block whose program is an agent is a session. A Rex session's label is the session's
  name in the fleet (`planning@laptop`); a Rex session with several agents names each `<session>/<pane>`, after the
  pane's label. A Claude Code session is known by its own session ID, read from Claude Code's session registry; other
  agents go by their terminal (`@block:<id>`) until a harness plugin names them.
- **Status.** From the program status reports (OSC 7501) the agent sends: `working`, `idle`, `done`, and `blocked`
  when it waits on a person (a permission prompt, a question). A blocked record anywhere in the terminal makes the
  session blocked; a failed run (`error`) reads as `done`. Claude Code sends these reports itself. An agent that sends
  none shows `unknown`. Changes arrive from `rex events`, without polling.
- **Messages.** Pasted into the terminal, then Return, so a message of several lines stays one message. A message to
  a blocked agent is refused ("it's waiting on a person") rather than typed into its prompt.
- **Starting and closing.** `fleetlayer session start` opens a new Rex session (`rex new <tab> --cwd <dir> --
  <agent>`), waits until the agent reports it's at rest (up to a minute), then types the first prompt. Closing a
  session closes its terminal, and the Rex session with it when that was its last terminal. Interrupting presses Esc.
- **Terminals.** `fleetlayer attach` reads the screen with Rex's `format` method a few times a second and sends what
  changed; in control mode it types with `write` and asks Rex to resize. Rex streams terminals over a data connection
  whose wire format isn't published, so the plugin reads the screen instead: fine for watching and stepping in, slower
  than a native stream.
- **Who's watching.** A session shows `humanAttached` when one of Rex's apps (Mac, phone, Windows) is attached to it.

## Security

It sees every session on its user's Rex server (labels, the program and folder in each terminal, the screen when you
attach) and types into a terminal only to deliver a message or a first prompt, or when someone attaches without
`--observe`, under the access policy. It runs `rex` with argv, never through a shell, and passes `--autostart=false`
so it never starts a server.

## Testing it

`plugins/rex/e2e/run <rex> <fleetlayer-rex>` checks the plugin end to end against a real Rex server: listing and
status, a refused message to a blocked agent, delivery, starting a session with a first prompt, `fleetlayer attach`,
status changes from `rex events`, and closing. It runs in a throwaway sandbox (its own HOME, XDG folders and runtime
folder, a cleared environment, a fake agent in place of Claude Code, the real home read-only where Linux allows it)
and checks afterwards that the real home is untouched. It never downloads or installs anything: download Rex's release
yourself.
