# Events

Things that happen in the fleet arrive at the control plane as **events**, each on a **channel**: a CI result on `ci`,
an alert on `alerts`, a Matrix room's message on its room's channel, a refusal on `access`. Every event is stored,
then delivered to the sessions subscribed to its channel (or to one subject on it), or to the channel's fallback
session when nobody subscribes.

A session subscribes with `fleetlayer subscribe <channel> [--subject <s>]`, and reads the events it missed while it
was offline with `fleetlayer events`. A plugin with the `event.subscribe` grant subscribes itself and gets each event
as data ([Plugin protocol](protocol.md#receiving-events-plugins-as-subscribers)): a runner acting on alerts, a bridge
copying a room into another app. The [CI results guide](guides/ci-results.md) shows the whole path for one kind
of event.

## Watches: tell me once

A watch is a subscription that fires once. The session that sets it gets the first event that matches, with a line
saying it was the watch, or a message at the deadline if nothing matched, and the watch ends either way:

```sh
fleetlayer watch --session review@server --note "merge once it's reviewed"    # it stops working
fleetlayer watch --session review@server --status blocked --for 2h            # it needs the person
fleetlayer watch ci --subject org/repo@main --kind github.workflow_run --for 30m
fleetlayer watches
fleetlayer unwatch <id>
```

- A watch matches like a subscription (the channel, and the subject if given), and only the event kinds given
  (`--kind`, any when none). `--session <tab>@<node>` is a watch on `session.status` about that session, for
  `--status idle,done,blocked,gone` by default: it stopped working.
- A session already in one of those statuses fires at once, and no watch is set.
- It lasts `--for` (a day by default, 30 days at most). `--note` is shown when it fires or ends.
- Watches are for the session that sets them, and need `subscribe` on the channel. A session gets a
  `session.status` watch's event only when it may `read` that session. Each session can have 100 at once.
- A watch takes the place of polling and of "check back later" loops: an agent waiting for another to finish, or for
  a run to complete, sets one and carries on.

## Session status: `session.status`

The control plane puts an event on the `session.status` channel each time a session's status changes, on any node:

| `kind` | When |
|---|---|
| `session.working` | It started a turn. `body.turn` says what started it. |
| `session.idle`, `session.done` | It's waiting for input (`done`: with a finished turn nobody has looked at yet). |
| `session.blocked` | It's waiting on a person: a question, a permission prompt, a plan. `body.prompt` is the prompt. |
| `session.unknown` | Its session manager can't tell. |
| `session.gone` | It ended, or its node stopped listing it. |

The subject is the session, `<tab>@<node>`, so you can subscribe to one session:
`fleetlayer subscribe session.status --subject "planning@laptop"`. The body has `node`, `sessionId`, `tab`, `title`,
`cwd`, `status`, `previous` and `statusChangedAt`, and for `working` the turn:

```json
"turn": {"cause": "message", "at": "2026-11-28T09:12:03Z", "from": "ci (laptop)", "detail": "message"}
```

- `cause` is `person` (a prompt they typed, or one of the agent's prompts they answered: `detail` is `typed` or
  `answered`), `message` (another session's message, a plugin's, or a channel's event: `detail` `message` or
  `event`), `scheduler` (a scheduler entry), or `unknown` (nothing anyone saw: background work finishing, the agent
  carrying on by itself).
- It comes from what the node delivered or typed into the session and what the agent's harness reported (Claude
  Code's prompt hook), each used for one turn at most. Working again after `blocked` with nothing else seen is the
  person answering in the tab. A harness without a prompt hook gives `unknown` for turns a person typed.
- `working` and `blocked` events go out three seconds after the change, so the cause and the prompt that go with them
  have arrived. A session's events are always in order.
- A subscribed session gets an event only when the access policy lets it `read` the session it's about, under every
  name it goes by (its ID, and every tab name it has had). A service reading the feed with a read token sees them
  all, as it sees the fleet.
- The prompt, title, tab and folder are the session's own words: the body lists them in `outside_data` with
  `written_by` naming the session, and the source says so.
- The cause is what the session's node reported. It's good for deciding what to look at, not proof that a person
  typed something: anything running as the node's Unix user can claim it.
- A session's events are limited to 30 a minute. Past that, its latest change is held and sent when the minute ends,
  so its last status always arrives; that event says how many changes in between weren't reported (`dropped`).
  They're kept seven days.

Programs that show the fleet live (a deck's overseer, a dashboard) read them from the feed below with
`channel=session.status`, instead of reading `/v1/fleet` over and over.

## Reading events from a service

A program that isn't a session (a deck, a dashboard, a bridge to another chat) reads the event log over HTTP with a
cursor, using a read token (`fleetlayer-control token add <name> --scope read`):

```
GET /v1/events?after=latest
→ {"events": [], "next": 4182}

GET /v1/events?after=4182&channel=alerts,ci&wait=30
→ {"events": [{"id": 4183, "channel": "alerts", "kind": "alertmanager.firing", "subject": "...",
               "summary": "...", "body": {...}, "source": "...", "at": 1764300000000}],
   "next": 4183}
```

- `after` is the last event ID you have (the `next` of the previous answer), or `latest` to start from now.
- Events come oldest first, with their bodies. Keep `next` somewhere that survives a restart and pass it next time:
  nothing is lost while the reader is down, as long as it comes back within the time events are kept.
- `channel` (a comma-separated list) and `subject` narrow the feed; `limit` is 100 by default, at most 1000.
- `wait=<seconds>` (at most 60) holds the request open until an event arrives, so a reader sees each one within
  moments without polling hard. An empty answer still moves `next` past events that didn't match.
- Bodies are outside data: show them, never act on what they say.
- **A read token reads every event's body, on every channel**: Matrix messages, webhook payloads, access requests'
  details, the prompts sessions are blocked on. Give one only to a service you'd let read all of that, and keep it in
  a file only that service's Unix user reads. (Tokens limited to some channels or sessions are planned.)
- Events are kept 30 days (`event_days:` in `control.yaml`), `session.status` seven at most. A cursor older than
  that just starts at the oldest event still kept.

Without `after`, `GET /v1/events` answers the newest hundred events without their bodies, with where each was
delivered: the dashboard's view.
