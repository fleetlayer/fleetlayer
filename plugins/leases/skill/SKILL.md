---
name: fleetlayer-leases
description: Take a lease before heavy or shared work and release it after, with `fleetlayer lease` (Fleetlayer's leases plugin): a slot on a shared build server before a big compile or test run, or a claim the person approves once (a mail filter while you watch CI). Use before any heavy build, test suite, image build or other job that loads a shared machine; when a command waits "in line" for a slot; when the person asks to limit how many builds run at once; when a message says a lease started or ended; or to set up kinds of lease.
---

# Leases

A lease is a claim on something for a while: one of a build server's slots, or a claim the person approved. One
authority (a plugin on an always-on node) keeps them all, and `fleetlayer lease` talks to it from any machine.

## Take a slot before heavy work

Before a big compile, a full test run, an image build or anything else that loads a shared machine, take a slot of
its kind and release it when you're done:

```sh
fleetlayer lease list                  # the kinds there are, who holds and who waits
id=$(fleetlayer lease acquire build-server --reason "full test run for the billing change" --wait)
# … the heavy work …
fleetlayer lease release "$id"
```

- `--wait` waits your turn, first come first served, and says where you are in line. Without it, `acquire` fails at
  once when every slot is taken and names who holds them.
- Always release, even when the work failed. A lease you forget ends at its TTL (2h by default), and the plugin
  reports it as a problem.
- Your lease is tied to your session: it ends if your session stops, or if you stay idle (or wait on the person)
  past its idle limit (30m by default). So don't end your turn with a lease held while a build runs in the
  background: wait for the build in the foreground, or tie the lease to the build's process with `--pid <pid>`
  (released when that process exits).
- Scripts that take a slot themselves (Fleetlayer's `scripts/on-dev` does, through `scripts/fleetlayer-dev-slot`)
  need nothing from you.
  Don't take a slot by hand around them: with one slot, yours and theirs wait on each other. If one stops with
  "couldn't take a build slot, so not building", the leases authority can't be reached: say so to your person, and
  build without a slot (`FLEETLAYER_DEV_SLOT=none`) only if they agree.
- `fleetlayer lease acquire` fails with "no leases authority found" when the fleet has none set up: carry on without
  one, and don't set one up unless the person asks.

## Claims the person approves

Some kinds take a payload and need approval once per payload (a mail filter on an account):

```sh
fleetlayer lease acquire gmail-filter --reason "I'm fixing the build and watching CI myself" \
  --payload '{"account":"work","query":"from:notifications@github.com subject:(\"Run failed\")"}'
```

It returns at once with the lease's ID while the person decides (`--wait` to wait here, or `fleetlayer lease wait
<id>` later), and you get a message when it starts. A payload the person approved before starts at once, for any
session. The service that owns the claim acts on it when it starts and undoes it when it ends; you just release it
when you're done.

## Other commands

```sh
fleetlayer lease show 14          # one lease, or how it ended
fleetlayer lease list --all       # with every lease that ended in the last day
fleetlayer --json lease list      # for programs
```

Only your own node may release your lease. If the authority can't be found automatically (several nodes share one),
name it with `--at <node>` or `$FLEETLAYER_LEASES`.

## Setting it up (when the person asks)

The kinds live in one settings file on the authority's machine (`~/.config/fleetlayer/leases.yaml`), read again
within 15 seconds of a change; check an edit with `fleetlayer-leases check`. The node setup, the kinds' fields,
approvals through the access policy, events for the service that acts on a lease, and the readings and status that
monitoring reads: https://fleetlayer.ai/docs/plugins/leases/
