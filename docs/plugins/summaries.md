# fleetlayer-summaries

Summaries of every agent session on a machine: a one-liner, a paragraph and a page per hour of activity. Fleetlayer is
about seeing and steering what your agents do, and this is how you see it without reading transcripts.

It's a **node plugin**, never part of the control plane: it reads transcripts on the machine where they are. Each
summary goes to two places:

- **A Markdown file on the node**, your own copy: `<dir>/<session id>/<YYYY-MM-DDTHH>.md` for an hour (default
  `dir`: `~/.local/share/fleetlayer/summaries`), `day-<YYYY-MM-DD>.md` and `week-<YYYY-Www>.md` for the day and week
  chunks of old history. Characters other than letters, digits, `-`, `_` and `.` in a session ID become `_`. Each file
  has front matter (session, node, agent, tab, dir, period, activity times, `written_at`) and `## Sentence`,
  `## Paragraph`, `## Page` sections (claude-summarizer's layout). Sync and back it up however you like, point it into
  a notes vault, or feed it to your own search index. Fleetlayer doesn't sync it for you: the
  [sync guide](../guides/sync.md) covers the choices.
- **The control plane**, for search and the web UI: the node publishes each summary over its link (held in order
  while the link is down), and the control plane stores it with full-text search. `fleetlayer summaries <words>`
  searches the fleet, `fleetlayer summaries --session <id>` shows a timeline, and `fleetlayer fleet` shows each
  session's latest one-liner. `publish: sentence` sends only the one-liner, `publish: none` nothing.

## Reading them

- `GET /v1/fleet`: each session carries `summary: {sentence, paragraph, page, lastActivity}` from its newest record.
- `GET /v1/summaries?session=<id>`: the session's whole timeline, oldest first, every record with its period,
  activity times, `writtenAt`, tab, dir and `origin` (`node`, or `claude_summarizer` for imported history).
- `GET /v1/summaries?q=<words>`: full-text search, best match first.
- `GET /v1/summaries?mentions=<text>`: the sessions whose summaries mention that exact text (an ID such as `sc-1234`,
  a PR number, a Sentry short ID), with the newest mentioning record of each. `fleetlayer summaries --mentions`.
- `GET /v1/summaries?since=<cursor>&limit=<n>`: the feed, for a program that keeps its own copy: every record
  written after the cursor, oldest first, with `nextCursor`. A cursor is `<writtenAt ms>:<rowid>` (each record carries
  its own `cursor`) and pages exactly; a bare `<writtenAt ms>` is inclusive, so records sharing that moment come again
  and an upsert by (node, session, period) absorbs them. A replaced record gets a new `writtenAt` and comes round
  again. Deletions come through the feed too, in order: `{deleted: true, node, sessionId, period: {kind, key} | null,
  cursor}`, where a null period means every record of that session from that node (`fleetlayer-control
  summaries-delete`). A tombstone's cursor is `<writtenAt>:t<id>`.
- `POST /v1/sessions/<id>/catchup`: "since your last message", from the session's node. Answers at once from the
  node's cache (with `stale: true` and a new one started when the transcript has grown), else waits while it's
  written.
- These take a bearer token: `fleetlayer-control token add <name> --scope read`.

## Importing claude-summarizer's history

Once per machine: `fleetlayer summaries --import <claude-summarizer output dir>`. Every Markdown update becomes a
record (`origin: claude_summarizer`), written as a file and published, and claude-summarizer's byte offsets are
adopted so nothing it summarised is summarised again.

## Setup

```yaml
plugins:
  - name: summaries
    command: fleetlayer-summaries
    grants: [sessions.watch, summary.publish]
    config:
      dir: ~/notes/summaries            # optional
      publish: full                      # full | sentence | none
      model: sonnet
      user_name: Sam                     # "since Sam's last message"
```

It runs headless Claude Code (`claude -p`) for each summary, so it spends tokens: by default at most one summary per
session per 30 minutes, only after a session has been idle for a minute, and only when there's enough new activity to
be worth a call. With the account rotator installed (it moves Claude Code sessions between your accounts, see the
[Claude accounts guide](../guides/claude-accounts.md)) it uses the account with the soonest weekly reset, and skips
accounts that are 98% through their week or have a full 5-hour window. An account that answers with a usage limit is
left alone until its 5-hour window resets (an hour when the reading doesn't say). A summary that stops for want of an
account waits and is tried again by itself: a minute after a usage limit (on another account), or when the soonest
limited account has room again (every 15 minutes when nothing says when).

**Watching it.** The plugin reports to its node with `_fleetlayer.dev/plugin/status` (the protocol's section 10),
published as the reading `plugin.status`, key `summaries`:

```json
{"plugin":"summaries","problem":null,"detail":{"lastSummaryAt":"2026-11-27T08:12:40Z","waiting":1,
  "oldestWaiting":{"session":"45be65c7-…","tab":"billing tests","since":"2026-11-27T09:30:02Z",
  "why":"You've hit your 5-hour limit · resets 10:30pm","nextTryAt":"2026-11-27T09:31:02Z"}}}
```

`problem` is set once a summary has waited more than an hour past the soonest reset it waits for (an hour after it
began, when no account says when it frees up), and the node's health and heartbeat turn red with it.

## How it decides (the engine)

A port of claude-summarizer (MIT). The plugin follows the node's sessions (`_fleetlayer.dev/sessions/subscribe`) and
turns their status changes into the engine's signals. The engine owns: transcript discovery (`~/.claude/projects`,
`~/.codex/sessions`) and byte offsets, the noise filter (wake-ups, tool output, compactions, slash commands, the
session manager's own auto-continue messages), the "worth a call" threshold, `no_change` answers, the idle timer and
its cancellation, the gap (an idle inside it is scheduled for when it ends, never dropped), one pending summary per
session, at most `max_concurrent` calls, the start-up and daily sweeps, day and week chunks for old history, the
48,000-character input cap and its trimming order, the skips (subagents, `codex exec` reviews, headless runs), giving
up on a chunk after three failures, and its own state under `~/.local/state/fleetlayer/summaries`. Offsets move only
after a successful call. A blocked session (waiting on a person) is never summarised; `done` counts as idle.

Settings and defaults: `idle_seconds` 60, `min_gap_seconds` 1800, `max_concurrent` 2, `input_cap_chars` 48000,
`daily_chunk_days` 30, `sweep` true, `model` sonnet, `enabled` true.

The plugin hands the node a `fleetlayer_protocol::summary::SummaryEvent`: `Summary(SummaryRecord)` (key: session ID
and period; a later record for the same period replaces it), `Activity` (the last real activity, after every
successful call) or `Catchup` (not published yet).
