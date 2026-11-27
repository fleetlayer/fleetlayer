# fleetlayer-herdr-fleet

Every herdr in your fleet, as one herdr. A program made to talk to the herdr on its own machine (herdr's phone relay,
a dashboard, your own script) points at this plugin instead, unchanged, and sees that machine's herdr plus every other
machine's sessions this node may see. Each other machine shows up as a workspace of its own, named after its node.

It's a node plugin that ships with Fleetlayer as `fleetlayer-herdr-fleet`. It never replaces herdr: herdr keeps its
own socket and its own windows, and everything that doesn't use the plugin carries on as before.

## What works on which sessions

| | This machine's panes | Other machines' sessions |
|---|---|---|
| Listing (`agent.list`, `tab.list`, `pane.list`, `workspace.list`, `session.snapshot`) | herdr's own answer | added after this machine's, one workspace per node |
| Status and lifecycle events (`events.subscribe`) | herdr's own | from the fleet listing, every few seconds |
| Reading the screen or history (`agent.read`, `pane.read`) | herdr | through a terminal stream (access: `read`) |
| Prompting, keys, text (`agent.prompt`, `agent.send_keys`, `pane.send_keys`, `pane.send_text`, `pane.send_input`) | herdr | typed in `input` mode, which never resizes the pane (access: `attach`) |
| Watching a terminal (`herdr terminal session observe\|control`) | herdr | a Fleetlayer terminal stream, in herdr's format |
| Closing (`tab.close`, `pane.close`) | herdr | the session is closed through the node (access: `close`) |
| Anything else (splits, layouts, worktrees, starting agents, focus changes in herdr's window) | herdr | `unsupported_method` |

herdr's client contract says clients treat `unsupported_method` as a normal error, so a client built for one herdr
shows the other machines' sessions with those actions turned off. The rules herdr keeps hold for the other machines'
sessions too: a prompt to a session that's waiting for a person (`blocked`) is refused with `agent_blocked`, and an
empty one with `empty_agent_prompt`.

Every read, prompt and close of another machine's session is checked by the access policy, as `fleetlayer attach` is,
and shows in the audit log with the reason "a herdr client (such as a phone app) reading or typing into this session
through herdr-fleet". If your policy asks the person before attaching, the first prompt to a session waits for that
approval.

## Transcripts

Clients that read an agent's own transcript files (Claude Code's `~/.claude/projects`, for the phone relay's
Conversation view) find this machine's sessions on disk as usual. With the grant `transcript.read`, the plugin also
copies the other machines' Claude Code transcripts into `<state_dir>/claude/projects/<folder>/<session id>.jsonl`, laid
out as Claude Code lays out its own, so pointing the client at `<state_dir>/claude` finds them too (the relay:
`HERDR_CLAUDE_CONFIG_DIRS`). A session seen for the first time gets the end of its transcript (`transcript_tail_kb`,
1 MB by default, from a line start); after that only what's appended, while it works and once after each change of
status. A copy is deleted when its session ends, and the folder is emptied when the plugin starts. Each read is checked
as `read` on the session, and the control plane passes the bytes on without keeping them. Without the grant nothing
is copied.

## Setup

In `node.yaml`, on the machine the client runs on:

```yaml
plugins:
  - name: herdr-fleet
    command: fleetlayer-herdr-fleet
    grants: [fleet.query, transcript.read, session.close]
    # transcript.read: copy other machines' Claude Code transcripts here (below); session.close: let clients close
    # other machines' sessions. Leave either out to turn it off.
    config:                                # all optional
      state_dir: ~/.local/state/fleetlayer/herdr-fleet
      herdr: /usr/local/bin/herdr          # the real herdr; default the first on the node's PATH
      local_socket: ~/.config/herdr/herdr.sock   # default: what `herdr status server` reports
      poll_seconds: 3                      # how often the fleet listing is read
      transcript_tail_kb: 1024             # how much of a transcript is copied when it's first seen
```

The plugin makes, in its state folder:

- `herdr.sock`: the merged socket, herdr's own API (mode 0600).
- `bin/herdr`: the HERDR_BIN shim. It runs the real herdr against the merged socket, and streams other machines'
  terminals itself. With the plugin stopped it's the real herdr, unchanged.
- `shim.json`: where the shim finds the real herdr and both sockets.
- `claude/`: the other machines' Claude Code transcripts, with `transcript.read`.
- `ids.json`: the workspace and pane numbers given to other machines' sessions, kept so a client's saved order and
  open views survive restarts.

Point the client at both, with full paths (the shim finds `shim.json` from the path it was started by):

```sh
HERDR_SOCKET_PATH=~/.local/state/fleetlayer/herdr-fleet/herdr.sock
HERDR_BIN=~/.local/state/fleetlayer/herdr-fleet/bin/herdr
HERDR_CLAUDE_CONFIG_DIRS=~/.local/state/fleetlayer/herdr-fleet/claude   # the phone relay's transcript folders
```

For herdr's phone relay, that's two lines in its service's environment ([the phone guide](../guides/phone.md)).

## IDs

This machine's herdr IDs pass through unchanged (`w1:p3`). Other machines' sessions get IDs in herdr's form from
workspace `w1000` up: one workspace per node (per node and herdr workspace when a node has several), one tab and one
pane per session (`w1000:t2`, `w1000:p2`), and terminal IDs `fleetlayer-1000-2`. The agent records carry the session's
Fleetlayer address in `_meta["fleetlayer.dev/session"]`.

## Cost

A client that polls screens (herdr's phone relay reads the whole history of every working agent about every seven
seconds) gets each other machine's screen through one terminal stream per session, opened at the smallest size herdr
renders and closed after a minute and a half unused. History comes back at the pane's own width whatever that size,
and is small for full-screen agents (Claude Code's is about 8 KB).

## Monitoring

The plugin reports its status to the node every minute. It's `failing` when its own socket doesn't answer an
`agent.list` (it asks itself, as a client would), or when it hasn't been able to read the fleet for two minutes. Its
detail has `remoteSessions`, `lastFleetAt`, `clients`, `openStreams` and `transcripts` (copies, and why copying is
off when it is). Read it anywhere with `fleetlayer readings
plugin.status`.

## Needs

Both ends on Fleetlayer 0.3.13 or later: the client's node for the fleet listing (`_fleetlayer.dev/fleet/query`), and
each other machine's node for reads (a watcher's `terminal.scrollback`), typing (`input` mode) and transcripts
(`link/transcript_serve`), and the control plane for the fleet listing and transcripts. herdr 0.9 or later
on each machine.

## Later

Fleetlayer's own phone page, served by the node's gateway behind its sign-in, reaching every session without a
herdr client in between.
