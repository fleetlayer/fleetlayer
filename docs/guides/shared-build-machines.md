# Many agents, a few build machines

A dozen agent sessions each want to build and test a Rust workspace. Each build keeps eight cores busy and needs
several gigabytes to link. You have one or two build machines. Left alone, the sessions all build at once, the machine
runs out of memory, and everything on it stalls: the builds, the CI runner beside them, and any service that happens
to live there.

The author's fleet hit each of these:

- Several release builds with link-time optimisation linked at the same moment, next to a CI VM. The host ran out of
  memory and every service on it stalled for ten minutes. A person's sign-in failed because the sign-in service was
  one of them.
- Too many agent sessions on one small VM ate its memory before any of them started a build.
- One build slot for everyone turned into a long queue.
- A session took a slot by hand, then ran a script that takes its own slot. It waited on itself forever.
- Killed builds and killed waits left places in the queue behind. Later one of them held the only slot, idle, for 30
  minutes.
- A merge left conflict markers in the build script, and every build failed until someone looked.

This guide sets up build machines so none of that happens: slots that are leases, a memory scope around every build,
builds made cheaper so more fit, a cap on sessions, and checks that tell an agent when something goes wrong. The
examples are Rust and cargo; the same shape works for any heavy toolchain. It assumes builds already go to a build
server over SSH, as in [Notes for agents that build software](agents-building-software.md).

## 1. The model: slots, classes and memory scopes

A build host has a few **slots**. A build runs only while it holds one. There are two classes:

| Class | Commands | Why |
|---|---|---|
| Heavy | `cargo build`, `cargo test` | They link. Linking is where memory peaks, often several GB per binary. |
| Light | `cargo fmt`, `cargo check`, `cargo clippy` | No linking. Less memory, so more of them run at once. |

Each build also runs inside a **memory scope**, with its CPUs pinned and its job count set:

```sh
systemd-run --scope -q -p MemoryMax=12G -p MemorySwapMax=0 \
  nice -n 10 taskset -c 0-7 cargo build -j 8
```

- `MemoryMax` is a hard limit for everything in the scope. A runaway build is killed inside its scope, by the kernel,
  before the host itself runs short. The agent sees a failed build, not a frozen machine.
- `MemorySwapMax=0` stops the scope from swapping its way past the limit. A build that swaps is slower than one that
  fails and gets retried, and it drags the whole host down with it.
- `taskset` keeps builds on some cores and leaves the rest for other work (a CI runner, the host's services).
- `-j` matches the CPUs and the memory: fewer jobs, fewer rustc processes holding memory at the same time.

Example numbers, from Fleetlayer's own [`scripts/on-dev`](../../scripts/on-dev):

| Class | Memory scope | Jobs | CPUs |
|---|---|---|---|
| Heavy | 12 GB | `-j 8` | 0-7 |
| Light | 6 GB | `-j 4` | 0-7 |

Then add up the worst case. Every slot full at its limit, plus whatever else the host runs, must fit in the host's
memory with room to spare. On a 64 GB host with a CI VM beside the builds:

| | Memory |
|---|---|
| CI VM, capped | 16 GB |
| 2 heavy slots × 12 GB | 24 GB |
| 2 light slots × 6 GB | 12 GB |
| The host itself, page cache, a compile cache server | 12 GB |

**Cap anything else that builds on the host too.** A CI VM gets a fixed memory size in the hypervisor, not a balloon
that grows. Containers get `--memory` with `--memory-swap` set to the same value. Release builds in a container get the
same treatment (section 3). The first incident above came from builds that had no limit at all, sharing a host with a
VM that also had none.

**Check:** on the build host, `systemd-run --scope -q -p MemoryMax=1G -p MemorySwapMax=0 true && echo ok` prints `ok`
as the build user. If it asks for authentication, either let that user start scopes with a polkit rule, or use
`systemd-run --user --scope` with a lingering user session (`loginctl enable-linger <user>`). Recent systemd delegates
the memory controller to user sessions, so `MemoryMax` works there too.

## 2. Slots as leases

A slot is a lease from Fleetlayer's [leases plugin](../plugins/leases.md). One authority on an always-on machine keeps
every lease for the fleet. Agents on every machine take a lease before a build and wait their turn when every slot is
taken.

One kind per class, in the authority's leases settings:

```yaml
kinds:
  build-server:
    description: Heavy builds and test runs (they link)
    slots: 2
    ttl: 2h
    stuck_after: 45m          # someone waiting this long is a problem
  build-server-light:
    description: fmt, check and clippy (no linking)
    slots: 2
    ttl: 1h
    stuck_after: 30m
```

Check an edit with `fleetlayer-leases check` before saving it; the plugin reads the file again within 15 seconds.

What a lease gives you that a lock file doesn't:

- **A queue.** `acquire --wait` waits in line, first come first served, and says where it is.
- **A reason.** Every lease says what it's for, so `fleetlayer lease list` shows who holds each slot and why.
- **It ends with its holder.** A lease tied to a process (`--pid`, or the process that ran the helper) ends within
  seconds of that process exiting, however it exits. A killed build frees its slot. A killed wait leaves the queue.
- **A time limit.** Every lease ends at its TTL even if nothing else ends it.

```sh
id=$(fleetlayer lease acquire build-server --reason "test fleetlayer-node (my-branch)" --wait)
cargo test -j 8 -p fleetlayer-node
fleetlayer lease release "$id"
```

### Let the scripts take the slot

Agents shouldn't take slots by hand around a build. The build scripts take their own:

- [`scripts/on-dev`](../../scripts/on-dev) takes a light slot for `fmt`, `check` and `clippy` and a heavy one for
  `build` and `test`, tied to its own process. It releases the slot when it returns, and a killed `on-dev` releases it
  within seconds.
- [`scripts/build-images`](../../scripts/build-images) builds in the background on the host, so it ties its slot to a
  stand-in process that ends when the build writes its status file (or after two hours).

Both call one **slot helper**, so the leases plugin's details live in one place.
[`scripts/fleetlayer-dev-slot`](../../scripts/fleetlayer-dev-slot) is that helper: `acquire <reason>` prints a lease
ID once a slot is free, `release <id>` gives it back, and the class picks the kind (`build-server` or
`build-server-light`). Put it on PATH once per machine and every checkout finds it:

```sh
ln -s "$PWD/scripts/fleetlayer-dev-slot" ~/.local/bin/
```

Your own projects' scripts can call the same helper. Write the rule into the agents' instructions (`AGENTS.md`): run
builds through the scripts, never take a slot by hand and then run a script, and never run cargo on the build host
outside them.

**Check:** `fleetlayer lease list` shows both kinds with their slot counts. Run `scripts/on-dev check` and, while it
runs, `fleetlayer lease list` in another shell shows a `build-server-light` lease with the reason
`fleetlayer on-dev check (<checkout>)`. Press Ctrl-C on the build, and within a few seconds the lease is gone.

## 3. Make each build cheaper

Every gigabyte a build doesn't need is room for another slot.

**Debug info.** Full debug info makes the linker read and write gigabytes per test binary. Line tables are enough for
backtraces with file and line numbers, and your dependencies rarely need any:

```toml
[profile.dev]
debug = "line-tables-only"

[profile.dev.package."*"]
debug = false
```

`cargo test` uses the dev profile, so test builds get it too. In Fleetlayer's workspace this made debug and test
builds link in a fraction of the memory, which is what let the light and heavy classes have separate, smaller limits.
Use full debug info locally when you need a debugger.

**A shared compile cache.** Twelve worktrees of the same repo compile the same dependencies twelve times.
[sccache](https://github.com/mozilla/sccache) caches each compiled crate, so the second worktree gets most of them
from disk. Give each project its own cache directory and server port, so it never mixes with other projects' builds on
the same host and one project's cache size can't push out another's:

```sh
export RUSTC_WRAPPER=sccache
export SCCACHE_DIR=/opt/code/myproject/sccache
export SCCACHE_CACHE_SIZE=20G
export SCCACHE_SERVER_PORT=4227      # a port per project
```

`scripts/on-dev` sets these only when the host has `sccache`, so a host without it still builds.

**Release builds.** Link-time optimisation needs the most memory of any build, all at the end. Run release builds
with fewer jobs (`-j 4`), in a container with a hard limit (`docker run --memory 12g --memory-swap 12g`), and holding a
heavy slot like any other build. That turns "several release builds link at once" from a host outage into a queue.

**Check:** on the build host, with the variables above set, `sccache --show-stats` shows cache hits climbing after a
second worktree builds. `cargo build` in a fresh worktree should take a fraction of the first one's time.

## 4. Cap the sessions on each machine

Builds are only half of it. Agent sessions use memory when they do nothing: the harness itself, its MCP servers,
language servers and file watchers each stay resident. Often that's several hundred megabytes per idle session. A
dozen idle sessions on an 8 GB VM leave nothing for the ones that work.

- **Pick a number per project and machine.** For example, at most six sessions of one project on a small VM. Put it in
  the project's `AGENTS.md`, so a coordinator agent knows the limit before it starts a seventh.
- **Close finished sessions.** A coordinator looks over the project's sessions every so often and closes the ones
  whose work is done. A closed Claude Code session can be resumed by name later, so closing loses nothing.
- **Alert when the count stays high.** A count above the limit for an hour usually means a coordinator stopped closing
  sessions. Give it a check like any other background thing (see [Monitoring](monitoring.md)), sent to the coordinator.

`fleetlayer sessions --json` lists the node's running sessions, so a scheduled check is one line:

```sh
n=$(fleetlayer sessions --json | jq length); [ "$n" -le 8 ] || echo "$n sessions on $(hostname)"
```

**Check:** `fleetlayer sessions` on each agent machine lists no more sessions than your limit, and `free -h` shows
memory available after the idle ones.

## 5. A second build host

When the heavy queue is long most of the day, the next step is another build host, not more slots on the first. More
slots on one host means less memory per build or a host that runs out.

The shape: one pair of kinds per host (`build-a`, `build-a-light`, `build-b`, `build-b-light`), and scripts that take
whichever host has a free slot. The script tries each host's kind without `--wait`, takes the first that's free, and
waits on the shortest queue when none is. The checkout is copied to the host it got.

Letting `scripts/on-dev` and the slot helper pick a host by themselves is coming. Until then, split sessions between
hosts by hand: give each machine's agents their own `.dev-host` (or `$FLEETLAYER_DEV_HOST`) and their own
`$FLEETLAYER_DEV_SLOT_KIND`, so each host's slots guard its own memory.

**Check:** `fleetlayer lease list` shows one pair of kinds per host, and each host's builds hold only that host's kind
(the reason names the checkout, and `.dev-host` in that checkout names the host).

## 6. Watch it

An agent should hear about each of these before a person notices builds are slow:

- **Memory pressure on the build host.** The kernel reports how long tasks stalled waiting for memory in
  `/proc/pressure/memory`. Alert when `some avg60` stays above a few percent for minutes, or on any `full` stall.
  Prometheus's node exporter has the same as `node_pressure_memory_stalled_seconds_total`. This catches the outage
  before it's an outage, which "is the host up" doesn't.
- **A queue that doesn't move.** The leases plugin reports a problem when a kind's first in line has waited past
  `stuck_after`. The problem turns its node's health red, so the node's heartbeat carries it to your monitor.
- **A lease that outlived its TTL.** A lease that ran into its TTL means a holder forgot to release it or was stuck.
  The plugin reports that as a problem too, for 30 minutes after.
- **The session count**, from section 4.

The leases plugin's numbers are readings, readable anywhere in the fleet:

```sh
fleetlayer readings lease.kind      # held, waiting, oldest held and oldest waiting, per kind
```

Send each alert to the session that owns the build hosts, with instructions to look before asking: which lease is
stuck, whose session holds it, whether its process still exists.

**Check:** `cat /proc/pressure/memory` on the build host shows `some avg60` near zero when nothing builds.
`fleetlayer readings lease.kind` shows both kinds. `fleetlayer status` on the authority's node says `healthy`.

## Footguns

- **Unlimited builds next to an unlimited VM.** Several release builds linked at once next to a CI VM, the host ran
  out of memory and every service on it stalled for ten minutes. Fix: a memory scope around every build, a fixed
  memory size for the VM, release builds in a capped container, and every build behind a slot (sections 1 and 3).
- **Too many sessions on one VM.** Idle sessions ate the memory before any build ran. Fix: a session limit per project
  and machine, a coordinator that closes finished sessions, and an alert on the count (section 4).
- **One slot for everyone.** A single slot turned into a long queue, and quick checks waited behind full test runs.
  Fix: light slots for commands that don't link, cheaper builds so more slots fit, then a second host (sections 2, 3
  and 5).
- **A slot taken twice.** A session took a slot by hand, then ran a script that takes its own. The script waited for
  the slot its own session held, forever. Slots don't nest. Fix: scripts take their own slot, and agents never take
  one around a script. If you need a slot for a one-off command, take it and run cargo directly, not the script.
- **Places in line that outlive their builds.** Killed builds and killed waits left leases queued. One later reached
  the front and held the only slot, idle, for 30 minutes. Fix: tie every lease to the process doing the work (the
  helper does, through `FLEETLAYER_LEASE_PID`), so a killed process leaves the queue within seconds; keep TTLs short
  for the class (an hour for light commands); and set `stuck_after` so a queue that stops moving is an alert. On the
  authority's machine, `fleetlayer-leases release <id>` ends a stray lease by hand.
- **Conflict markers in the build script.** A merge left them in, and the script failed for everyone until someone
  read it. Fix: a CI step and a pre-commit hook that fail on them, and `bash -n` over every script:

  ```sh
  ! git grep -nE '^(<<<<<<<|=======|>>>>>>>)( |$)' -- scripts/ && for f in scripts/*; do bash -n "$f"; done
  ```

- **A build that swaps.** Without `MemorySwapMax=0`, a scope at its limit pushes into swap and the whole host slows
  down instead of one build failing. Set it on every scope.
- **One cache for every project.** A compile cache shared between projects on a host mixes their entries and lets
  one evict the other. Fix: a directory and server port per project (section 3).

**Check:** run the conflict-marker line above in each repo that has build scripts; it prints nothing and exits 0.
