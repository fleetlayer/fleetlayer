---
name: fleetlayer-presence
description: Check whether the person is at their desk, on their phone, away or asleep with `fleetlayer presence` (Fleetlayer's presence plugin) before background work nobody may read, and record it when they tell you they're going out, going to bed, back, or away on holiday. Use before a periodic audit, a refresh, a report or a message that only matters if someone reads it soon; whenever the person says "I'm going out", "back in an hour", "off to bed", "I'm back" or names dates they'll be away; when a `presence.changed` event arrives; or to set presence up for them.
---

# Presence

One authority in the fleet knows whether the person is here, from their desk (screen lock and idle time), their
looks at pages on the phone, what they said, their planned absences and the time where they are. Ask it before
spending tokens on work nobody will read.

```sh
fleetlayer presence                      # alice is away since 12:04, until 13:00: said away (lunch) …
fleetlayer presence --json               # state, since, until, why, desks, plans
fleetlayer presence --is present,semi_present || echo "nobody's looking"
```

| State | What it means for you |
|---|---|
| `present` | At the desk. Normal work; they'll likely read and answer soon. |
| `semi_present` | Looked at their phone just now. They may answer a short question; skip the long reports. |
| `away` | Out, or said so, or on a planned absence (`until` says when they're back). Save background work for later. |
| `asleep` | Night where they are, or they said so. Hold anything that can wait until `until`. |

## Before background work

- Periodic work whose only reader is the person (an audit, a status refresh, a summary): do it while they're
  `present`, much less often when they're not, and catch up once when they come back. Work that's due regardless (a
  deploy, a fix for an alert) goes ahead.
- Don't poll presence in a loop: subscribe to changes instead (`fleetlayer subscribe presence`). Each
  `presence.changed` event says who, the new state, the old one, until when and why, once the change has held a minute.
- A question that blocks your work goes to them whatever the state, but when they're away or asleep, say what you'll
  do meanwhile instead of waiting.

## When the person tells you

Record it right away, whichever session they told. Use their own words for the note and their times (they're read in
the person's time zone):

```sh
fleetlayer presence set away --for 1h --note lunch            # "going out for lunch"
fleetlayer presence set away --until 15:30 --note "school run"
fleetlayer presence set asleep                                # "off to bed": until their night ends
fleetlayer presence set back                                  # "I'm back"
fleetlayer presence plan away --from 2026-12-20 --to 2027-01-05 --note holiday   # --to's date is included
fleetlayer presence plan list
fleetlayer presence plan remove 3
```

- Only record what they said; never set someone away because they went quiet (the desk already tells).
- They don't need to say they're back from lunch at the desk: input after the desk showed them gone ends a stated
  away by itself.
- With several people set up, add `--person <handle>`. With no presence authority in the fleet, the command says
  "no presence authority found": carry on without it, and don't set one up unless the person asks.
- A refusal from the access policy means your role may read presence but not change it: tell the person.

## Setting it up for the person

Read `docs/plugins/presence.md` in the Fleetlayer repo (fleetlayer.ai/docs/plugins/presence): the authority plugin and
its `presence` tool on an always-on node, a settings file naming each person (time zone, night, desks), and the desk
source on their own computer (`args: [desk]`, `person:`). On macOS the desk source needs no privacy permission; if
anything else would, ask the person first and say what it opens up.
