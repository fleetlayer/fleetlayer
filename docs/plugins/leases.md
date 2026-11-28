# fleetlayer-leases

Leases for agents across a fleet. A lease is a claim on something for a while, held by a session or a script:

- **A slot**: a build server takes one big build at a time, so agents on every machine take a slot of `build-server`
  before a heavy build and wait their turn when both are taken. Nothing else changes: the slots only keep big jobs
  from running at once.
- **An approved claim**: a session asks for a mail filter while it watches CI. The person approves that filter once;
  the service that owns filters creates it when the lease starts and removes it when the lease ends. Fleetlayer keeps
  the lease, the approval and the expiry; the service keeps only the filter.

Every lease ends on its own: when its holder releases it, at its time limit (TTL), or when its holder dies. A holder
is a session (it stops running, its node goes offline, or it stays idle too long) or, for a script, a process (it
exits).

It's a node plugin that ships with Fleetlayer as `fleetlayer-leases`. One authority keeps every lease: run it on a
machine that's always on (a home server, a VPS). Agents everywhere reach it through a tool its node shares, so the
control plane checks every call against the access policy and writes it to the audit log, and `fleetlayer lease` is
the command line for it.

## Setup

On the always-on machine, in `node.yaml`:

```yaml
plugins:
  - name: leases
    command: fleetlayer-leases
    grants: [fleet.query, access.check, event.publish, message.send, reading.publish]
    config:                                   # all optional
      settings: ~/.config/fleetlayer/leases.yaml
      state_dir: ~/.local/state/fleetlayer/leases

tools:
  - name: leases
    command: fleetlayer-leases
    args: [serve, --caller, "{caller}"]
    allow: ["*"]                              # who may take leases: <tab>@<node> patterns
    check_calls: true                         # every call checked and audited by the control plane
    call_targets:
      acquire: ["/kind"]                      # so rules can name a kind: tool:leases/acquire/build-server@<node>
```

What each grant is for: `fleet.query` to see which sessions still run (a lease ends with its session),
`access.check` to ask the access policy about kinds that need approval, `event.publish` for kinds with events,
`message.send` to tell a session its lease started or ended while it wasn't waiting, `reading.publish` for the
readings below. Leave out what you don't use: a fleet with only build slots needs `fleet.query`, `message.send` and
`reading.publish`.

Then the kinds, in the settings file:

```yaml
kinds:
  build-server:
    description: Heavy builds and test runs on the build server
    slots: 1                    # one held at a time; the rest wait in turn
    ttl: 2h                     # a lease's limit unless --ttl asks for less (default 2h)
    max_ttl: 6h                 # the most --ttl may ask for (default 24h, or ttl if longer)
    idle: 30m                   # a session holder idle this long loses it (default 30m)
  build-server-light:
    description: Checks that don't link (fmt, check, clippy)
    slots: 3

  terraform:
    description: One session at a time per Terraform component
    slots: 1
    per: component              # counted per component: a lock per named thing
    ttl: 8h
    payload:
      required: [component]
      fields:
        component: '[a-z0-9_/-]{1,64}'

  gmail-filter:
    approval: true              # the access policy decides each new payload, once
    ttl: 24h
    max_per_holder: 5
    events: leases              # lease.started and lease.ended go to this channel
    payload:
      required: [account, query]
      fields:                   # each field's value must match its pattern
        account: '[a-z0-9_-]{1,40}'
        query: '.*\bfrom:\S+.*'
```

The plugin reads the file again within 15 seconds of a change: no restart. Check an edit first with
`fleetlayer-leases check` (or `check <file>`), which lists every problem at once. A saved file with a mistake keeps
the last good kinds running, and the plugin reports the problem.

| Field | |
|---|---|
| `slots` | At most this many held at once; the others wait, first come first served. Leave it out for no limit. |
| `per` | Count `slots` per value of this payload field instead of for the whole kind: a lock per named thing. `terraform` above lets one session hold `racks/us` while another holds `dns`, and a second session asking for `racks/us` waits in that component's own line. The field must be one of the payload's `required` fields. |
| `ttl`, `max_ttl` | The time limit a lease gets, and the most a caller may ask for with `--ttl`. |
| `idle` | A held lease whose session has been idle (or waiting on the person) this long ends. `--idle` sets it per lease. |
| `approval` | Each new payload is decided by the access policy (below) before the lease can start. Needs `payload`. |
| `payload` | The kind takes a JSON object of strings, at most 600 bytes: `required` fields, a pattern per field in `fields` that must match the whole value, and `others: true` to allow fields not named. Control and invisible characters (zero-width, direction marks) are refused, so the person sees everything they approve. Leave it out and the kind takes none. |
| `max_per_holder` | The most leases of this kind one holder may have at once. |
| `events` | A channel for `lease.started` and `lease.ended` events. Leave it out for none. |
| `stuck_after` | Someone waiting this long is a problem (default: the ttl). |
| `description` | One line for `list`. |

## Taking a lease

```sh
id=$(fleetlayer lease acquire build-server --reason "release build" --wait)
cargo build --release
fleetlayer lease release "$id"
```

`acquire` prints the lease's ID on standard output once it's held, and the rest on standard error. Without `--wait`,
it fails at once when every slot is taken, naming who holds them; with `--wait` it waits its turn, saying where it is
in line. Other options: `--ttl 30m`, `--idle 1h`, `--payload '{"account":"work","query":"from:ci@example.com"}'`,
`--pid <pid>`, and `--at <node>` for the authority (default `$FLEETLAYER_LEASES`, else the one `leases` tool this
session may use).

```sh
fleetlayer lease list          # every kind: who holds, who waits, who's asking, and what ended lately
fleetlayer lease show 14       # one lease, or how it ended
fleetlayer lease wait 14       # wait until a lease that asks or waits is held
fleetlayer lease release 14
```

A kind with `per` takes one value per lease. To hold several things at once (a change that touches two
components), take one lease each, always in the same order (sorted), so two sessions after the same two can't each
hold one and wait for the other:

```sh
for c in dns racks/us; do      # sorted
  ids="$ids $(fleetlayer lease acquire terraform --reason "move the DNS records" --payload "{\"component\":\"$c\"}" --wait)"
done
```

Only the holder's node may release a lease (the node is the trust boundary: any process of its user can act for any
of its sessions). On the authority's machine, `fleetlayer-leases release <id>` ends any lease, and
`fleetlayer-leases list` shows them all without going through the control plane.

### Who holds it, and when it ends by itself

| Holder | Tied to | Ends when |
|---|---|---|
| A session (`acquire` run by an agent) | The session, by its `<tab>@<node>` | The session stops running (after 2 minutes unseen), its node goes offline (after 5 minutes), or a held lease's session stays idle or waits on the person for the idle limit |
| A script, or `--pid` | A process on the caller's machine | The process exits: a keeper (`fleetlayer lease keep`, started in the background by `acquire`) watches it, holds a connection to the authority while it lives, and releases the lease when it exits. If the keeper itself goes away, the lease ends 2 minutes later |

If the authority restarts while a caller waits with `--wait`, that waiting lease leaves the queue (nobody waits for it
any more), and `fleetlayer lease acquire --wait` asks again by itself for up to ten minutes. A keeper cut off from the
authority longer than its grace takes a new lease for the same process when it can reach it again.

Every lease also ends at its TTL. A session-tied lease suits work done in the agent's own turns. For a long build in
the background, tie the lease to the build's process (`--pid`), or keep waiting for it in the foreground: an agent
that ends its turn is idle, and loses the lease after the idle limit. If the authority can't read the fleet (its node
isn't linked), sessions are left alone, and the plugin reports it after 5 minutes.

A session that isn't waiting on the call when its lease starts or ends by itself (an approval came through, the TTL
passed) gets a message saying so, from `plugin:leases@<node>`.

## Approval

For a kind with `approval: true`, a new lease first asks the access policy: verb `write` on
`service:leases/<kind>/<key>`, as `plugin:leases@<node>`, with the holder, reason and payload as the justification.
`<key>` is the first 16 hex digits of the SHA-256 of the payload's canonical JSON (keys sorted), so the same payload
always has the same key. The policy decides as it does anything else:

```yaml
ask:
  - who: "plugin:leases@*"
    can: write
    to: "service:leases/gmail-filter/*"
    for: 365d                  # an approved payload starts at once for every session for a year
```

- A lease whose reason and payload together are too long to show the person whole (over 1000 characters) is refused,
  never put to them cut short.
- The person sees an approval request naming the holder, the reason and the payload
  (`fleetlayer-control approvals list`, or wherever `access.ask` events reach them). Approving it approves that
  payload of that kind for every session, because the request is the plugin's: the next session that asks for the
  same payload starts at once.
- While it waits, the lease is `asking`: `acquire` returns at once with its ID unless `--wait`, and the session gets a
  message when it starts. A refusal ends it; nobody answering within a day ends it.
- Without an `ask` rule, Fleetlayer's default asks first for `write` (30 days). A fleet whose policy has `ask: []`
  allows every payload at once unless a rule like the one above asks.
- Revoke an approval with `fleetlayer-control approvals`; the next new lease of that payload asks again.

## Events: for the service that acts on a lease

A kind with `events: <channel>` publishes on that channel, with the kind as the subject:

- `lease.started`: `body.lease` is the lease (`id`, `kind`, `payload`, `payloadKey`, `holder`, `reason`, `startedAt`,
  `expiresAt`...), and `body.firstOfKey` is true when no other held lease has the same payload: create the filter.
- `lease.ended`: `body.lease` adds `endedAt`, `why` (`released`, `expired`, `idle`, `session_gone`, `node_gone`,
  `process_gone`, `cancelled`, `kind_removed`) and `by`, and `body.lastOfKey` is true when no held lease has the same
  payload any more: remove the filter. Only leases that started have an ended event.

Events are kept in the plugin's state file (its outbox) with the change that made them, and sent in order until the
control plane takes them, so a node that was unlinked, or a plugin that restarted, delays an event but never loses
it; an outbox event waiting more than 5 minutes is a status problem. A lease's events go to the channel it got when
it started, so a lease of a kind removed or changed since still ends with an event. `body.seq` is 1 for started and 2
for ended, and each event has a `dedupeKey` (`lease:plugin:leases@<node>:<epoch>:<id>:started`, where the epoch is
random per state file, so a state file started afresh never repeats a key), so a repeat is dropped. A service that
was down catches up with `fleetlayer lease list --json --all`: the held leases, and every lease that ended in the
last day. The plugin publishes as `plugin:leases@<node>`, which needs `publish` on the channel. Events go to whoever
subscribes to the channel, and to its fallback session when nobody does.

## Access control

With `check_calls: true`, each call is decided as `call` on `tool:leases/<tool>@<node>`, and `acquire` as
`tool:leases/acquire/<kind>@<node>`:

```yaml
only:
  - who: "*@scraper"
    can: call
    to: ["tool:leases/list@*"]                      # may look, may not take
deny:
  - who: group:untrusted-input
    can: call
    to: "tool:leases/acquire/gmail-filter@*"
```

Connecting at all is `use_tool` on `tool:leases@<node>`, which Fleetlayer's default asks the person about once per
session (30 days): allow it for your agents in the policy (`allow: [{who: "*", can: use_tool, to:
"tool:leases@*"}]`). The tool's `allow:` in `node.yaml` is checked as well.

The holder is the caller the control plane recorded when the connection opened (`{caller}`), never what the caller
says. A script's caller is `-@<node>`.

## Build slots for `scripts/on-dev`

Fleetlayer's own `scripts/on-dev` and `scripts/build-images` take a build slot first when the machine has a helper:
`$FLEETLAYER_DEV_SLOT`, `.dev-slot` at the checkout's root, or `fleetlayer-dev-slot` on PATH, called as `acquire
<reason>` (prints an ID once a slot is free) and `release <ID>`. `scripts/fleetlayer-dev-slot` is that helper on
`fleetlayer lease`:

```sh
echo scripts/fleetlayer-dev-slot > .dev-slot       # in a checkout
ln -s "$PWD/scripts/fleetlayer-dev-slot" ~/.local/bin/   # or for every checkout on this machine
```

It takes a lease of the build host's slot name (the second word of its `.dev-host` line, which the scripts pass as
`$FLEETLAYER_DEV_SLOT_RESOURCE`; `build-server` when the line has none, `$FLEETLAYER_DEV_SLOT_KIND` to choose another),
or of that name with `-light` for the commands that don't link (fmt, check, clippy: on-dev sets
`FLEETLAYER_DEV_SLOT_CLASS=light`). With two build hosts, `build-a` and `build-b`, the authority has four kinds:
`build-a`, `build-a-light`, `build-b` and `build-b-light`. The lease is tied to the calling script's
process, so a killed `on-dev` releases its slot within seconds. When a machine has a helper and no slot can be taken,
the scripts don't build (unlimited builds are what the slots prevent): they stop with exit code 75.
`FLEETLAYER_DEV_SLOT=none` builds without one, on purpose. `build-images` builds in the background on the build
server, so it holds its slot until the build's status file appears (at most two hours). The same helper works for
any project's scripts.

## Watching it

- **Readings**: `lease.kind`, one per kind, key the kind's name: `{slots, held, waiting, asking, oldestHeldSince,
  oldestWaitingSince, holders}`, published on every change and every 15 seconds. `fleetlayer readings lease.kind`
  reads them anywhere in the fleet.
- **Status** (`_fleetlayer.dev/plugin/status`), a problem when a lease ran into its TTL in the last 30 minutes (a
  holder that forgot to release), a kind's queue is stuck (its first in line waited past `stuck_after`), the settings
  can't be read, the fleet can't be read for 5 minutes, or its loop hasn't finished a round for 3 minutes. A problem
  turns the node's health and heartbeat red, so the node's own monitoring catches it.
- **Log**: every start and end, with the whole lease, in `log.jsonl` in its state directory (cut to its newest half
  past 8 MB).

## Moving it

All its state is the state directory (`state.json`, its backup `state.json.bak`, `log.jsonl`), and its settings are
the one file. The state file is written whole and flushed to disk; if it can't be read at start (a disk fault), the
plugin starts from the backup, the state before the last change, and says so in its log.
`fleetlayer move leases <machine>` carries both to that machine's node ([Moving a piece](../guides/moving.md)); add
the `tools:` entry there too, and point `$FLEETLAYER_LEASES` at the new node if you set it. Leases survive the move:
a process-tied lease's keeper reconnects within 2 minutes of the authority starting again.

## Trust

The authority is only as trusted as its node's Unix user: any process of that user can talk to its socket and end
any lease. Agents elsewhere reach it only through the shared tool, as themselves. Its approvals are requests in its
own name (`plugin:leases@<node>`), so give that name no more reach in the policy than its node's agents have, or run
the authority under a Unix user of its own.
