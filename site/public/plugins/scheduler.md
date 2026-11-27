# fleetlayer-scheduler

Sends a message to an agent session on a schedule: every few hours, on a cron expression, or once at a set time. The
prompt you send a worker agent every three hours ("check the queue and fix anything stuck") is one entry in a settings
file, instead of you remembering it or a cron line that types into a terminal.

It's a node plugin that ships with Fleetlayer as `fleetlayer-scheduler`. Run it on a machine that's always on (a home
server, a VPS), never a laptop that sleeps: a fire that comes while the machine is off is sent late, once, when it's
back.

## Setup

In `node.yaml`:

```yaml
plugins:
  - name: scheduler
    command: fleetlayer-scheduler
    grants: [message.send]
    config:                                   # all optional
      settings: ~/.config/fleetlayer/scheduler.yaml
      state_dir: ~/.local/state/fleetlayer/scheduler
      tz: Europe/London                       # for cron entries without tz; default the machine's zone
```

Then the entries, in the settings file (`~/.config/fleetlayer/scheduler.yaml` unless `settings` says otherwise):

```yaml
alert_after: 2              # failed fires in a row before the plugin reports a problem (default 2)
entries:
  - name: worker check
    to: worker@server       # <tab>@<node>, a bare <tab>, or a session ID
    every: 3h               # 90m, 1h30m, 1d, 2w: at least a minute
    text: Check the queue and fix anything stuck.

  - name: weekday plan
    to: planning@server
    cron: "0 9 * * 1-5"     # minute hour day-of-month month day-of-week
    tz: America/New_York    # the cron's zone; default the plugin's tz
    text: Plan the day from the open stories.
    if_busy: skip           # queue (default) or skip

  - name: look at the PR
    to: reviewer@server
    at: 2026-11-28T15:00:00Z  # once; or "2026-11-28 15:00" read in tz
    text: CI should be done by now; review the PR.
    catch_up: false         # default true
    enabled: true           # default true
```

The plugin reads the file again within a few seconds of a change: no restart. Before saving an edit, check it:
`fleetlayer-scheduler check` (or `check <file>`) reads it, lists every problem at once, and prints each entry's next
three fires. If a saved edit has a mistake anyway, the plugin keeps running the last good entries and reports the
problem (below).

### Entries

| Field | |
|---|---|
| `name` | Unique, one line. The message names it, and `fire` takes it. |
| `to` | The session: `<tab>@<node>`, `<tab>` (this node first, then a unique tab anywhere), or a session ID. |
| `text` | The message. |
| `every` / `cron` / `at` | When, exactly one. `every` counts from the entry's last fire, so it never drifts. `cron` takes five fields with `*`, lists, ranges, steps, month and day names (`mon-fri`) and `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`; when both day fields are set, a day matching either fires, as in cron. |
| `tz` | The zone for `cron` and a civil `at`. A time skipped when the clocks go forward fires just after the jump; a time that happens twice fires once. |
| `if_busy` | When the session is waiting on a person (a permission prompt or a question): `queue` tries again every minute until it takes the message or the next fire replaces it; `skip` drops this fire. A session that's simply working gets the message straight away: its harness queues it for the next turn. |
| `catch_up` | After the plugin or its machine was off: `true` sends one late message for the fires missed, `false` sends none and waits for the next. |
| `enabled` | `false` pauses it. Turned back on, it starts afresh from that moment. |

A new entry first fires one interval (or the next cron time) after the plugin first sees it, never for times before it
existed. An `at` already in the past when it's added never fires.

## What the session sees

The plugin sends through its node like any other sender, as `plugin:scheduler@<node>`:

```
Message from plugin scheduler (server) via Fleetlayer:

From the scheduler entry "worker check" (every 3h):

Check the queue and fix anything stuck.
```

A late fire adds "(Sent late: it was due at …, while the scheduler wasn't running; 2 earlier fires folded into this
one.)", a queued one "(Due at …; held until the session could take it.)", and a test fire says "fired by hand".

## When a send doesn't go through

Nothing is ever sent as a burst, and a restart neither sends a fire twice nor skips one: the plugin saves which fires
it has handled (`state.json` in its state directory) before each send and after.

| What happened | What it does | Counts as a failure |
|---|---|---|
| Delivered | Records it | |
| The session is waiting on a person | `queue`: tries every minute until the next fire; `skip`: waits for the next fire | Only if still waiting when the next fire comes |
| The session isn't running, or no session has that name | Records it, tries again at the next fire | Yes |
| Refused (access control, a missing `message.send` grant) | Records why, tries again at the next fire | Yes |
| No answer within 30 seconds | Records it, doesn't send it again (it may have arrived) | Yes |
| The plugin stopped mid-send | Doesn't send it again after the restart | |
| Fires missed while the plugin was off | `catch_up: true`: one late message; `false`: none | |

## Checking on it

```sh
fleetlayer-scheduler list          # each entry: where, when, next fire, last result
fleetlayer-scheduler list --json   # the state file as it is
fleetlayer-scheduler fire "worker check"   # send it now, to test; prints how it went
```

They read the plugin's state directory, so run them on the plugin's machine, as its user (`--state-dir` if it isn't
the default). A test fire doesn't change the schedule, and doesn't count as a failure.

## Watching it

The plugin reports whether it does its job with `_fleetlayer.dev/plugin/status` ([protocol](https://fleetlayer.ai/protocol.md) section
10), every minute and on every change. Its problem is set when:

- an entry's last `alert_after` fires (default 2) in a row weren't delivered: "'worker check': the last 2 fires to
  worker@server weren't delivered (not delivered: the session isn't running: …)";
- the settings file can't be read (the last good entries keep running);
- its loop hasn't finished a round for three minutes ("stuck since …"), reported from a separate task.

A problem turns the node's health and heartbeat red, so the node's own monitoring catches it, and it's the reading
`plugin.status` with key `scheduler` (`fleetlayer readings plugin.status`), which a monitor anywhere in the fleet can
check. If the plugin stops reporting altogether, the node marks it failing after five minutes. The report's `detail`
has each entry's next fire, last fire, result, error and failure count.

## Trust

A message from the scheduler is only as trusted as its node's Unix user: any process of that user can send as
`plugin:scheduler@<node>`. So it's safe to run beside the agents it messages, and it isn't a way to give an agent
orders the policy wouldn't let that node give anyway. A message to a session on the same node is delivered with no
policy check, as every message on one node is ([access control](https://fleetlayer.ai/access-control.md#who-and-what)); a message to
another node is checked as `plugin:scheduler@<node>` → `message` on the target, so a rule can allow or forbid it like
any sender's.

Don't give `plugin:scheduler@<node>` more reach than the node's agents have unless the plugin runs under a Unix user
of its own, with its own node.

## Moving it

All its state is the state directory, and its settings are the one file. Stop the node, copy both to the new machine
(the same paths, or set `settings` and `state_dir`), add the plugin to that node's `node.yaml`, and start it. Fires
due while it was stopped are handled by `catch_up`. Entries with a bare `<tab>` in `to` now look on the new node first,
so write `<tab>@<node>` for a session on a particular machine.
