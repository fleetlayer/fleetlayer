# Moving a piece to another machine

You can start with everything on your laptop (`fleetlayer up`) and later move any one piece to a home server or a VPS:
the control plane, a node, the gateway, the Claude accounts hub, pages with its apps, sign-in, or any other plugin. One
command does it, and it keeps the piece's identity, keys and data:

```sh
fleetlayer move pages server --dry-run   # what it would do
fleetlayer move pages server
```

`server` is anything `ssh` takes: a name from `~/.ssh/config`, `me@10.0.0.5`, a tailnet name. Asking an agent to
"move pages to the server" works too: the [Fleetlayer skill](agents-know-fleetlayer.md) tells it how.

## Before you start

- **Fleetlayer on the other machine**, the same version or newer. There:

  ```sh
  curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
  ```

- **SSH without a password prompt**: `ssh server true` should just work (a key, or your SSH agent). The move runs
  with `BatchMode`, so it never stops to ask.
- **On a Linux server, lingering on** (`loginctl enable-linger "$USER"` there), so the user services it starts keep
  running after you log out.
- **For a plugin (hub, pages, sign-in...), a node there**, linked to the same control plane. If it has none yet, enrol
  one (`fleetlayer-control node add server` where the control plane runs) and on the server run
  `fleetlayer up --join wss://control.example/v1/link --token-file <file> --no-hub --no-pages`.
- **From an older release:** run `fleetlayer up` once on the machine with the control plane. It writes the control
  plane's settings file (`~/.config/fleetlayer/control.yaml`), which the move carries.

## What a move does

1. **Prints the plan**: every file it will copy, the settings it will write, the address the piece will have, and
   anything you should know. `--dry-run` stops here. Anything that can't work (Fleetlayer missing there, a file
   already there with other content, no node there for a plugin) stops it here too, before anything changes.
2. **Stops it here.** A plugin's block in `node.yaml` is commented out, with a line saying where it went, and the node
   restarts without it. The control plane tells every linked node its new address as it stops.
3. **Copies its files**, sealed to a key the other machine makes for this move only, and sent over SSH. The other
   machine checks each file's SHA-256 against the list before it moves any of them into place, and never writes over a
   file that's different. Paths in its settings are rewritten for the other machine's home folder
   (`/Users/me/…` becomes `/home/me/…`).
4. **Starts it there** and waits until it's healthy: the control plane's and the gateway's `/healthz`; a node with the
   same name and key, linked to its control plane; a plugin running in the node there and answering its health page.
5. **Points everything at the new address**: the gateway here routes the piece's name to it, so
   `http://pages.localhost:7680` keeps working on this machine; nodes follow a moved control plane; this node checks a
   moved gateway.
6. **Removes the copy here**, only once it's healthy there. `--keep-old` keeps it instead, renamed
   `<name>.moved-<date>`.

If anything fails before step 4 says healthy, the move undoes itself on the other machine and starts the piece here
again, as it was.

## The pieces

| Piece | What moves | Afterwards |
|---|---|---|
| `control` | `control.yaml` and its folder (the database, the admin token, the audit key) | Nodes switch to the new address by themselves (below) |
| `node` | `node.yaml`, the node's key and token, and the files its plugins use | Same name and key, so the control plane accepts it and secrets sealed to it still open |
| `gateway` | `gateway.yaml` and its folder | This machine's node checks it at its new address |
| `hub` | The hub's folder (sign-ins, setup tokens) and its login key | The gateway here routes `claude-hub` to it |
| `pages` (or `apps`) | Its login key, and the space folders unless they're already there | The gateway here routes `pages` and the apps' names to it |
| `sign-in` | Its folder (passkeys, the token-signing key, sessions) and its token | Tokens it signed still verify; see passkeys below |
| any plugin's name | The files its settings name, and its usual state folder | Its block moves to the node there |

A node is one per Unix user, so `fleetlayer move node` needs a user on the other machine with no node yet.

## Addresses

- **Where it listens there:** on the address SSH reached that machine on (its LAN or tailnet address), never on
  every interface. Its port is the one it has here, or its usual one (sign-in 7650, the hub 7660, pages 7670);
  `--port` and `--listen` change them.
- **Behind a proxy with TLS**, give the address people and nodes use: `--url wss://control.example/v1/link`, or
  `--url https://pages.example`. Set up the proxy first ([On a server](laptop.md#on-a-server-traefik-and-gatus-from-the-same-list)).
- **Settings that name this machine** (a plugin's `control_plane: http://127.0.0.1:7600`, a gateway route to
  `127.0.0.1`) are listed in the plan: on the other machine they'd point at itself. Change them there, or move that
  piece too.

## When the control plane moves

As the old copy stops, it tells every linked node the new address (`link/moved`, in the [link protocol](../link.md)).
Each node tries it, and once it links there it saves the address as `control.url` in its `node.yaml`. Until then, and
for an hour, it still tries the old address, so a move that was undone costs nothing. A node that was offline during
the move doesn't hear it: set `control.url` in its `node.yaml` by hand and restart it.

Nodes keep their tokens: the database moves with the control plane.

## Sign-in and passkeys

A passkey belongs to a domain: by default the issuer's parent domain (`example.com` for `signin.example.com`), so it
works for any address under it. Moved to another machine with the same issuer address (your DNS name now points at
the new machine), everyone's passkeys keep working, and so they do at a new address under the same domain. Changing
the issuer address still means repointing every service that signs in through it:
[Sign-in: moving it](../sso.md#moving-it) explains it, and the plan reminds you.

## Pages and the agents that write them

Agents on this machine write pages straight into the space folders. Once pages runs elsewhere, either share those
folders with the new machine (and the move leaves them alone, since they're already there), or have the agents use
`fleetlayer pages push`.

## Secrets

Keys and tokens never travel in the clear and never pass through the control plane: the whole copy is sealed to a key
made for this move on the other machine, and it's sent over SSH as well. That key is deleted when the move ends. The
copy here is deleted once the piece is healthy there, unless you pass `--keep-old`; delete a kept copy once you're
happy, since it still holds the secrets.

## Trying it on one machine

`local:<folder>` moves a piece to another home folder on this machine (a second instance, `$FLEETLAYER_INSTANCE`),
which is how Fleetlayer's own tests move every piece. With `FLEETLAYER_MOVE_SERVICES=process`, the pieces run as
plain background processes instead of launchd or systemd services, with pid files in
`~/.local/state/fleetlayer/run/`.
