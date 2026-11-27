# Every machine on your phone

herdr has a phone app: [herdr-mobile-relay](https://github.com/0cv/herdr-mobile-relay) runs beside herdr, and your
phone pairs with it to see your agents, read their screens, answer their questions and send prompts. One relay
reaches one herdr. With agents on a laptop, a home server and a VPS, that's a relay to install, pair and keep running
on each.

With Fleetlayer, one relay shows them all. The [herdr-fleet plugin](../plugins/herdr-fleet.md) serves herdr's own API
for every herdr in your fleet, and the relay talks to it as if it were herdr. Each other machine shows up on the phone
as a workspace named after it. The relay itself isn't changed.

## What you get

- Every session on every machine in the agent list, with its status, grouped by machine.
- Their screens and terminal history, prompts, keys and slash commands, and answering a blocked question, on any
  machine. Typing never resizes the session's terminal on its own machine, so your own window there stays as it was.
- Closing a session on another machine, if you give the plugin `session.close`.
- Everything the relay does for the machine it runs on, unchanged.

Not yet for other machines' sessions: the Conversation view (it reads transcript files on the relay's machine; their
Terminal view works), workspace files and diffs, starting agents and worktrees there. The phone shows those actions as
unavailable rather than failing.

## Set it up

Pick the machine the relay runs on: one that's always on, since the phone reaches everything through it. Then:

1. Run the relay there as usual, paired with your phone ([its quick start](https://github.com/0cv/herdr-mobile-relay#readme)).
2. Add the plugin to that machine's `node.yaml` and restart the node:

   ```yaml
   plugins:
     - name: herdr-fleet
       command: fleetlayer-herdr-fleet
       grants: [fleet.query, session.close]
   ```

   `fleetlayer status` shows `herdr-fleet` running; `~/.local/state/fleetlayer/herdr-fleet/bin/herdr agent list`
   lists every machine's agents.
3. Point the relay at the plugin. Its systemd user service takes two more lines of environment, in a drop-in:

   ```ini
   # ~/.config/systemd/user/herdr-mobile-relay.service.d/fleetlayer.conf
   [Service]
   Environment=HERDR_SOCKET_PATH=%h/.local/state/fleetlayer/herdr-fleet/herdr.sock
   Environment=HERDR_BIN=%h/.local/state/fleetlayer/herdr-fleet/bin/herdr
   ```

   then `systemctl --user daemon-reload && systemctl --user restart herdr-mobile-relay`. On a Mac, add the same two
   variables to the relay's `relay.env` and restart it.
4. Open the app. The other machines appear as workspaces after this one's.

To undo it, remove the drop-in (or the two lines) and restart the relay: it's back on the local herdr alone.

## Access

The phone acts through the relay's machine, so the access policy sees it as that node: reading another machine's
session needs `read`, typing into it `attach`, closing it `close`. If your policy asks you before attaching, the first
prompt to each session waits for your approval, then passes for a while like any other. A policy that keeps a session
private keeps it off the phone too: it isn't listed at all.

## What it costs

The relay polls: it reads the history of every working agent about every seven seconds. For another machine's
session that's one small terminal stream, kept open while it's used and closed after a minute and a half without.

## Later

Fleetlayer's own phone page, served behind your sign-in, reaching every session without a relay in between.
