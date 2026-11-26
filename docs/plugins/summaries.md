# fleetlayer-summaries

Summaries of every agent session on a machine: a one-liner, a paragraph and a page per hour of activity. Fleetlayer is
about seeing and steering what your agents do, and this is how you see it without reading transcripts.

It's a **node plugin**, never part of the control plane: it reads transcripts on the machine where they are. Each
summary goes to two places:

- **A Markdown file on the node**, your own copy: `<dir>/<session id>/<YYYY-MM-DDTHH>.md` (default `dir`:
  `~/.local/share/fleetlayer/summaries`), with front matter (session, node, agent, tab, dir, period, activity times)
  and `## Sentence`, `## Paragraph`, `## Page` sections (claude-summarizer's layout). Sync and back it up however you
  like, point it into a notes vault, feed it to your own RAG. Fleetlayer doesn't sync it for you; Syncthing is the
  guide's suggestion.
- **The control plane**, for search and the web UI: the node publishes each summary over its link (held in order
  while the link is down), and the control plane stores it with full-text search. `fleetlayer summaries <words>`
  searches the fleet, `fleetlayer summaries --session <id>` shows a timeline, and `fleetlayer fleet` shows each
  session's latest one-liner. `publish: sentence` sends only the one-liner, `publish: none` nothing.

## Setup

```yaml
plugins:
  - name: summaries
    command: fleetlayer-summaries
    grants: [sessions.watch, summary.publish]
    config:
      dir: ~/Notes/Agents/summaries     # optional
      publish: full                      # full | sentence | none
      model: sonnet
      user_name: Sam                     # "since Sam's last message"
```

It runs headless Claude Code (`claude -p`) for each summary, so it spends tokens: by default at most one summary per
session per 30 minutes, only after a session has been idle for a minute, and only when there's enough new activity to
be worth a call. With the Claude account rotator installed it uses the account with the soonest weekly reset, and
skips accounts that are 98% through their week.

## How it decides (the engine)

A port of [claude-summarizer](https://github.com/ndbroadbent/claude-summarizer) (MIT, same author), first moved into
Quietdeck. The plugin follows the node's sessions (`_fleetlayer.dev/sessions/subscribe`) and turns their status
changes into the engine's signals. The engine owns: transcript discovery (`~/.claude/projects`, `~/.codex/sessions`)
and byte offsets, the noise filter (wake-ups, tool output, compactions, slash commands, the session manager's own
auto-continue messages), the "worth a call" threshold, `no_change` answers, the idle timer and its cancellation, the
gap (an idle inside it is scheduled for when it ends, never dropped), one pending summary per session, at most
`max_concurrent` calls, the start-up and daily sweeps, day and week chunks for old history, the 48,000-character input
cap and its trimming order, the skips (subagents, `codex exec` reviews, headless runs), giving up on a chunk after
three failures, and its own state under `~/.local/state/fleetlayer/summaries`. Offsets move only after a successful
call. A blocked session (waiting on a person) is never summarised; `done` counts as idle.

Settings and defaults: `idle_seconds` 60, `min_gap_seconds` 1800, `max_concurrent` 2, `input_cap_chars` 48000,
`daily_chunk_days` 30, `sweep` true, `model` sonnet, `enabled` true.

The records are `fleetlayer_protocol::summary`: `SummaryRecord` (key: session ID and period; a later record for the
same period replaces it), `Activity` (the last real activity, after every successful call), `Catchup` (not published
yet).
