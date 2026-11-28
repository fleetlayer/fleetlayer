# fleetlayer-presence

Whether the person is here, so agents skip background work nobody will read and know when to expect an answer:

- **present**: at their desk, with the screen unlocked and input in the last 15 minutes.
- **semi_present**: away from the desk, but they just looked at a page on their phone (a deck, a relay). They may
  answer.
- **away**: the desk is locked, idle or asleep, or they said so ("I'm going out for lunch", for an hour), or it's a
  planned absence (a holiday). Away isn't absolute: a look at the phone during it shows as semi_present.
- **asleep**: night where they are (11 pm to 8 am unless set otherwise), or they said they're going to bed.

Agents read it with `fleetlayer presence`, and hear changes as events on the `presence` channel. Typical uses: an
overseer that audits every 30 minutes only while the person is present, a deck that skips background refreshes
nobody looks at, a mail triage agent that pauses overnight and does one catch-up when the person wakes.

It's a node plugin that ships with Fleetlayer as `fleetlayer-presence`, in two parts:

- **The authority**: one per fleet, on a machine that's always on. It keeps what people said and planned, hears their
  desks, decides each person's state every 15 seconds, and publishes it. Agents everywhere reach it through a tool
  its node shares, so the control plane checks every call against the access policy and writes it to the audit log.
- **The desk source**: on the person's own computer, it reports whether the screen is locked and how long since the
  last keyboard or mouse input.

## Setup

On the always-on machine, in `node.yaml`:

```yaml
plugins:
  - name: presence
    command: fleetlayer-presence
    grants: [reading.subscribe, reading.publish, event.publish]
    config:                                   # all optional
      settings: ~/.config/fleetlayer/presence.yaml
      state_dir: ~/.local/state/fleetlayer/presence

tools:
  - name: presence
    command: fleetlayer-presence
    args: [serve, --caller, "{caller}"]
    allow: ["*"]                              # who may read and set presence: <tab>@<node> patterns
    check_calls: true                         # every call checked and audited by the control plane
    call_targets:                             # so rules can name the person: tool:presence/set/alice@<node>
      set: ["/person"]
      plan_add: ["/person"]
      plan_remove: ["/person"]
      seen: ["/person"]
```

What each grant is for: `reading.subscribe` to hear the desks, `reading.publish` for the `presence` reading,
`event.publish` for changes on the `presence` channel.

Then the people, in the settings file:

```yaml
people:
  alice:                        # their handle, the same as in sign-in
    timezone: Europe/London     # default: the authority machine's time zone
    night: "23:00-08:00"        # asleep unless something says otherwise; "off" for none
    desks: [laptop]             # nodes whose desk source speaks for alice
    away_after: 15m             # an unlocked desk idle this long no longer counts as present
    seen_for: 10m               # a look at a page on the phone counts as semi_present this long
```

The plugin reads the file again within 15 seconds of a change: no restart. Check an edit first with
`fleetlayer-presence check` (or `check <file>`), which lists every problem at once. A saved file with a mistake keeps
the last good settings running, and the plugin reports the problem.

On the person's own computer, in its `node.yaml`:

```yaml
plugins:
  - name: presence-desk
    command: fleetlayer-presence
    args: [desk]
    grants: [reading.publish]
    config:
      person: alice
      # command: "my-idle-check"   # anywhere but macOS: prints {"locked": true|false, "idleSeconds": 42}
```

On macOS it needs no privacy permission: it reads the screen lock and the idle time from `ioreg`, the I/O Registry
any user may read. It looks every 10 seconds and reports once a minute, and at once when the screen locks or unlocks
or when input comes after a minute without any. On another system, `command:` names a program that prints the same
two fields as JSON (on Linux desktops, `loginctl show-session` has `LockedHint` and `IdleSinceHint`).

The authority trusts a desk report only from a node listed in that person's `desks:`: the control plane says which
node a reading came from, so another node can't speak for someone's desk. The authority's node must be allowed to
read the desk's node (`read` in the access policy), as it is by default in a fleet of one person.

## Reading it

```sh
fleetlayer presence                                   # one line per person
fleetlayer presence --person alice --json             # everything: state, since, until, why, desks, plans
fleetlayer presence --is present,semi_present || exit 0   # scripts: skip work while nobody's there
```

A line reads like this:

```
alice is away since 12:04, until 13:00: said away (lunch) until 13:00 at 12:01 via planning@laptop. Their time: Mon 12:30 (Europe/London).
```

`--json` gives `person`, `state` (`present`, `semi_present`, `away` or `asleep`), `since`, `until` (for a stated
status, a planned absence or the night), `note`, `why` (one line on what decided it), `line`, `at`, `timezone`,
`localTime`, `night`, `desks` (each with `locked`, `idleSeconds`, `at` and `fresh`), `seen`, `stated` and the upcoming
`plans`.

With one person set up, `--person` can be left out; `$FLEETLAYER_PERSON` sets it for a session or script.
`--at <node>` (or `$FLEETLAYER_PRESENCE`) names the authority when more than one node shares a `presence` tool.

### Changes, as events and readings

- Each change is an event on the `presence` channel, kind `presence.changed`, subject `person:<handle>`, once the new
  state has held for a minute (so a moment's flicker wakes nobody). Its body has `person`, `state`, `from`, `since`,
  `until`, `note` and `why`. A session subscribes with `fleetlayer subscribe presence`; a script reads what it missed
  with `fleetlayer events`.
- The state is also a reading, kind `presence`, key the person, sent on every change and at least every 5 minutes.
  Plugins subscribe to it (`_fleetlayer.dev/reading/subscribe`); `fleetlayer readings presence` prints the latest. A
  reading whose `at` is more than 15 minutes old means the authority has stopped: treat the person as unknown.

## Saying it

When the person tells any session they're going out, that session records it:

```sh
fleetlayer presence set away --for 1h --note lunch
fleetlayer presence set away --until 15:30 --note "school run"
fleetlayer presence set asleep                         # until the end of their night
fleetlayer presence set back
fleetlayer presence plan away --from 2026-12-20 --to 2027-01-05 --note holiday
fleetlayer presence plan list
fleetlayer presence plan remove 3
```

Times are the person's own (their `timezone`): `14:00` (the next 14:00), `2026-12-20 14:00`, `2026-12-20` (a bare
date for `--to` includes that day) or an RFC 3339 timestamp. A stated status lasts at most 31 days (plan anything
longer), and one with no end lasts at most a day.

A stated away holds while the person is still at the desk: they're typing when they say it. Once the desk shows them
gone (locked, idle 5 minutes, or asleep), input at the desk again means they're back early, and the stated away ends
by itself. During a planned absence, the desk in use shows them present, and the plan stays.

Services that show the person pages (a deck, a phone relay) report a look:

```sh
fleetlayer presence seen --via quietdeck --device phone
```

## How the state is decided

In order, the first that holds wins:

1. **present**: a desk of theirs reported in the last 3 minutes, unlocked, with input in the last `away_after`
   (unless they just said they're going: see above); or they looked at a page on a desk in the last 5 minutes.
2. **semi_present**: they looked at a page on their phone (or another device) in the last `seen_for`.
3. What they said, until its time.
4. **away** during a planned absence.
5. **asleep** during their night.
6. **away**: nothing says they're here.

A desk report older than 3 minutes counts for nothing, so a laptop that sleeps or loses its connection never leaves
someone present.

## Access

Every call goes through the shared tool, so the access policy decides who may read and who may change whose status,
as verb `call` on `tool:presence/<call>/<person>@<node>` (with the `call_targets` above; `get` has no person part).
The person's own fleet may set their status; keep other roles to reading:

```yaml
deny:
  - who: "*@triage"                           # the mail agent may read presence, not change it
    can: call
    to: ["tool:presence/set/*@*", "tool:presence/plan_add/*@*", "tool:presence/plan_remove/*@*"]
  - who: "*"
    can: call
    to: "tool:presence/seen/*@*"
allow:
  - who: "-@hub"                              # only the deck's own service reports looks
    can: call
    to: "tool:presence/seen/*@*"
```

The authority records who said what (the caller the control plane recorded, never what the caller claims): it shows
in `stated.by` and the plans' `by`.

## Monitoring

The authority reports its status every 15 seconds (`fleetlayer status`, the `plugin.status` reading), and a problem
when its settings have mistakes, its state can't be saved, it can't hear desk reports, events have failed for 10
minutes, or a round hasn't finished in 3 minutes. The node's heartbeat carries it, so a stopped or stuck authority
alerts like any failing plugin. The desk source reports a problem when it can't read the desk for a minute. A desk
that's simply off (a laptop asleep) isn't a problem: its reports go stale, and the person shows as away with "laptop
silent since 18:02".

## Moving it

`fleetlayer move presence <machine>` moves the authority with its settings file and state directory. Point the
`presence` tool at the new node (or leave `--at` unset, and agents find it). The desk source stays on the person's
computer.

## Planned

Home Assistant as another source (the companion app reports whether the person is home), so a person who's home but
away from the desk reads differently from one who's out.
