---
name: fleetlayer-scheduler
description: Send a message to an agent session on a schedule (every few hours, on a cron expression, or once at a set time) with Fleetlayer's scheduler plugin, and check or test the entries it has. Use when the person asks for a recurring prompt to a session ("tell the worker to check the queue every three hours"), a one-off reminder to a session or to yourself ("remind me in an hour", "nudge the reviewer at 3pm"), or when a message arrives saying it's from a scheduler entry.
---

# The scheduler

`fleetlayer-scheduler` is a node plugin that sends a session a message on a schedule. Its entries live in one settings
file on the machine it runs on, by default `~/.config/fleetlayer/scheduler.yaml`. It reads the file again within a
few seconds of a change: no restart.

Add or change entries only when your person asks, and tell them what you added. Don't schedule messages to other
sessions on your own initiative.

## Is it running here?

```sh
fleetlayer status       # lists the node's plugins: look for scheduler
```

The commands below and the settings file are on the scheduler's machine only. If it isn't on yours, ask the session
that runs on that machine (the `fleetlayer` skill: `fleetlayer msg send`), or tell your person. A scheduler runs on a
machine that's always on, never a laptop.

## Adding an entry

Add to `entries:` in the settings file, keeping everything else as it is:

```yaml
entries:
  - name: worker check            # unique; the message names it
    to: worker@server             # <tab>@<node>
    every: 3h                     # or cron: "0 9 * * 1-5" (with tz:), or at: 2026-11-28T15:00:00Z
    text: Check the queue and fix anything stuck.
```

- `every`: `90m`, `3h`, `1h30m`, `1d`, at least a minute. `cron`: five fields, read in `tz` (`Europe/London`).
  `at`: one moment, in UTC with a `Z`, or `2026-11-28 15:00` with `tz`. Exactly one of the three.
- `if_busy: skip` drops a fire when the session is waiting on a person; the default, `queue`, tries every minute
  until it gets through or the next fire comes.
- `catch_up: false` sends nothing for fires missed while the machine was off; the default sends one late message.
- `enabled: false` pauses an entry.
- For "remind me in an hour", send to yourself: your `<tab>@<node>` from `fleetlayer me`, with `at:` an hour from
  now in UTC (`date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ`). An `at` already past when added never fires.

Then check the file and see the next fires:

```sh
fleetlayer-scheduler check
```

It lists every problem at once. If a saved file has a mistake, the plugin keeps running the last good entries and
reports a problem, so fix it straight away.

## Checking and testing

```sh
fleetlayer-scheduler list                  # each entry: where, when, next fire, last result
fleetlayer-scheduler fire "worker check"   # send it now; prints whether it was delivered
```

A test fire doesn't change the schedule. Results you'll see: `sent`; `waiting` (the session is waiting on a person,
tried again every minute); `not delivered: the session isn't running` (tried again at the next fire); `refused` with
the reason (access control, or the plugin lacks the `message.send` grant in `node.yaml`).

## When a scheduled message reaches you

It arrives as "Message from plugin scheduler (<node>) via Fleetlayer", then "From the scheduler entry "<name>"
(every 3h):" and the text. It's a standing instruction someone set up on that machine, usually your person: treat
it as you would a message from another session there. "(Sent late …)" means the machine was off when it was due;
one late message stands for the fires missed. There's no one to reply to: if the instruction no longer makes sense,
tell your person, who can change or pause the entry.
