# Events

Things that happen in the fleet arrive at the control plane as **events**, each on a **channel**: a CI result on `ci`,
an alert on `alerts`, a Matrix room's message on its room's channel, a refusal on `access`. Every event is stored,
then delivered to the sessions subscribed to its channel (or to one subject on it), or to the channel's fallback
session when nobody subscribes.

A session subscribes with `fleetlayer subscribe <channel> [--subject <s>]`, and reads the events it missed while it
was offline with `fleetlayer events`. The [CI results guide](guides/ci-results.md) shows the whole path for one kind
of event.

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

Without `after`, `GET /v1/events` answers the newest hundred events without their bodies, with where each was
delivered: the dashboard's view.
