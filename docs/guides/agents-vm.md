# A server VM for your agents

Agents on a laptop stop when the lid closes. A session halfway through a task waits until morning, mail and messages
pile up with nobody to read them, and an update reboot ends every tab. Give the agents a machine that's always on
instead: a VM on a home server, a small rented VPS, a spare mini PC. You keep working from the laptop, and its herdr
shows the server's tabs next to its own.

The shape:

- **A Linux VM that stays on**, made by you.
- **A login user for the agents, without sudo**, so an agent can't change the system. You keep an admin account for
  the few things that need root.
- **Lingering on**, so that user's services run with nobody logged in.
- **SSH in from the laptop**, through Tailscale SSH or plain SSH keys.
- **herdr and Claude Code for that user**, with herdr's server running as a user service.
- **A Fleetlayer node**, so the server's sessions join the fleet.

The examples call the VM `server`, the agents' user `agents`, and your own machine `laptop`. Ask your agent to do the
steps on the server once you can SSH in; the ones only you can do are marked **You:**.

## Why not the laptop

Anything an always-on agent depends on has to be always on too. A laptop sleeps, travels, changes networks and
reboots for updates, and each time it does, the sessions on it stop and anything they serve to other machines goes
with them. Keep the laptop for what needs you there: your signed-in browser, a passkey touch, the terminal you type
in. Put the agents, and the services they lean on, on the server.

## 1. The machine

**You:** create the VM, or rent the server. Two cores, 4 to 8 GB of memory and 40 GB of disk is plenty for a long-lived
triage agent and a few more sessions. Install a current Debian or Ubuntu, and make an admin account for yourself with
sudo, reachable only with your SSH key. That account is how you get root later. Your provider's or hypervisor's console
is the way back in if SSH ever breaks.

Install system software as that admin, or through whatever manages the machine (Ansible, cloud-init, a NixOS config).
When an agent needs a package or a system setting, it writes the change into that config or asks you for the command.
It never gets root itself.

## 2. A user for the agents, without sudo

As your admin account:

```sh
sudo useradd --create-home --shell /bin/bash agents
sudo loginctl enable-linger agents     # its services start at boot and run with nobody logged in
```

Don't add `agents` to the `sudo` or `wheel` group. Check that your own home isn't readable by it (`ls -ld ~` shows
`drwx------`; if not, `chmod 700 ~`), and keep your admin account's keys and credentials out of the agents' home.

Without lingering, a user's systemd services stop when their last session ends and start again only when they log in.
herdr's server and the Fleetlayer node are both user services, so lingering is what keeps the tabs alive overnight.

## 3. SSH in from the laptop

The laptop's herdr keeps an SSH connection open to the server and reconnects after every sleep, so whatever you use
must let it back in without a prompt. Both of these work.

### Tailscale SSH

Tailscale gives the server a name on your private network (`server.tail1234.ts.net`) without opening a port to the
internet, and its SSH server checks your tailnet policy instead of keys on the machine. As your admin account on the
server:

```sh
sudo tailscale up --ssh --advertise-tags=tag:agents
```

**You:** open the link it prints and approve the machine in your Tailscale account. Then, in your tailnet policy, let
your own devices SSH in as `agents` with `"action": "accept"`:

```json
"ssh": [
  { "action": "accept", "src": ["autogroup:member"], "dst": ["tag:agents"], "users": ["agents"] }
]
```

The default action, `check`, asks you to sign in again in a browser every so often. herdr's reconnect can't answer
that, so the server's tabs drop out of the laptop's herdr until you do. Give `tag:agents` access only to what it needs
on the rest of the tailnet.

### Plain SSH keys

On a LAN, or a VPS with a public address, ordinary OpenSSH works as well and needs no extra account. Put the laptop's
public key in `/home/agents/.ssh/authorized_keys` (mode 600, in a folder of mode 700), and turn off password sign-in
in `/etc/ssh/sshd_config`:

```text
PasswordAuthentication no
KbdInteractiveAuthentication no
```

A VPS's public SSH port gets scanned all day, so keys only, and a firewall that admits only the addresses you connect
from if you can. Reaching a home server from outside your home needs a VPN of some kind either way.

Whichever you use, check from the laptop that `ssh agents@server` logs straight in.

## 4. herdr and Claude Code

**herdr.** Install it system-wide as your admin account (from [herdr.dev](https://herdr.dev)), the same version the
laptop runs: the laptop's herdr attaches to this server, and an older server lacks what a newer client asks for. Then,
as `agents`, run its server as a user service so the tabs keep running when no laptop is attached.
`~/.config/systemd/user/herdr.service`:

```ini
[Unit]
Description=herdr server

[Service]
ExecStart=/usr/local/bin/herdr server
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now herdr
```

If you switched to `agents` with `sudo -iu agents`, set `export XDG_RUNTIME_DIR=/run/user/$(id -u)` first, or
`systemctl --user` fails with "Failed to connect to bus".

On the laptop, save the machine once:

```sh
herdr machine add agents@server --label server
```

Its tabs now show in the laptop's herdr. Make a tab, start `claude` in a project folder, and check that you can type in
it and scroll back.

**Claude Code.** Install it for the `agents` user, following Claude Code's own instructions. Sign it in on this machine:
run `claude auth login` in a terminal there, and **You:** open the link, sign in and paste the code back. Never copy
another machine's sign-in folder: two machines refreshing the same sign-in knock each other out.
[Many Claude accounts on many machines](claude-accounts.md) explains why, and sets up year-long setup tokens instead.

Start each session in a project or scratch folder, never the home folder: Claude Code asks whether to trust the
folder, and never remembers that answer for the home folder, so an unattended session there waits forever.
[Agents that run while you're away](unattended-agents.md) covers the other prompts that stop a session nobody watches.

## 5. A Fleetlayer node

Install a node as `agents` and run it as a service, following [Set up a node that keeps running](node-services.md).
Lingering is already on from step 2. Run `fleetlayer-node install` from a shell whose PATH finds both herdr and
`claude`, since the service keeps that PATH. Add the node's heartbeat while you're there, so you hear when it stops
doing its job ([monitoring](monitoring.md)).

Once it's linked, the server's sessions can message the laptop's and the laptop's can reach them, under your
[access policy](../access-control.md).

## More users for agents you trust less

Every process of a Unix user can act as any of that user's sessions, so all the agents in `agents` trust each other
completely. An agent that reads email, web pages or other text strangers wrote belongs in a user of its own, with its
own node and rules that limit what it can reach. [Run an agent that reads strangers' text](untrusted-input.md) sets
that up on the same server. An agent that must never reach your network needs a VM of its own.

## Builds go somewhere bigger

A small VM is right for agents, which mostly wait, and wrong for big builds. A full Rust build or test run keeps every
core busy for minutes and fills the disk, and every other agent on the machine slows down with it. Send heavy builds
to a build server; [Notes for agents that build software](agents-building-software.md) has a worked example and the
traps it avoids.

## Dev servers on a remote machine

An agent runs `npm run dev` on the server and gives you a link. Opening it from the laptop goes wrong in two ways.

**Vite answers 403.** It answers only requests for host names it knows: `localhost`, names under `.localhost`, IP
addresses, and the names you list. Any other name gets a 403 with
`Blocked request. This host ("server.tail1234.ts.net") is not allowed.` Add it to `server.allowedHosts`
(`preview.allowedHosts` for `vite preview`) in `vite.config.js`:

```js
export default {
  server: {
    allowedHosts: ['server.tail1234.ts.net'],   // or '.tail1234.ts.net' for every name under it
  },
}
```

Don't set it to `true`: Vite's docs warn that this leaves the dev server open to DNS rebinding.

**The page loads, then breaks.** `crypto.randomUUID`, and other browser features kept for secure contexts, exist only
on HTTPS pages and on `localhost`. On `http://server:5173` or `http://10.0.0.5:5173` it's undefined, and the app fails
with "crypto.randomUUID is not a function" or something stranger.

So open the dev server one of two ways:

- **A localhost tunnel.** Nothing to set up on the server, and Vite already allows `localhost`. From the laptop:

  ```sh
  ssh -N -L 5173:localhost:5173 agents@server
  ```

  Then open `http://localhost:5173`. Vite's default port is 5173; use whatever the agent's server printed.
- **The private network's full name over HTTPS.** Use this when a phone or another machine should open it too. On a
  tailnet, `tailscale serve --bg 5173` serves the dev server at `https://server.tail1234.ts.net/` with a certificate
  for that name. **You:** turn on HTTPS certificates for your tailnet in its admin console. `tailscale serve` needs
  root, or a user named with `sudo tailscale set --operator=agents`, which lets that user change everything about the
  machine's Tailscale, so you may prefer to run it yourself for a fixed port. Then add that name to
  `server.allowedHosts`.

Put the rule in the project's agent instructions (`AGENTS.md` or `CLAUDE.md`): "dev server links use
`http://localhost:<port>` through an SSH tunnel, or `https://server.tail1234.ts.net`, never a bare host name or IP
address". Then each new session doesn't hand you a link that half works.

## When it doesn't work

- **The server's tabs disappear from the laptop's herdr after a while.** Tailscale SSH is on `check`, which wants a
  fresh browser sign-in. Set the rule for `agents` to `accept`.
- **The laptop's herdr can't do something on the server that works locally.** The server's herdr may be older. Install
  the laptop's version there and restart the service.
- **Tabs and the node stop when you log out.** Lingering isn't on: `sudo loginctl enable-linger agents`.
- **"Failed to connect to bus"** from `systemctl --user` after `sudo -iu agents`: set
  `XDG_RUNTIME_DIR=/run/user/$(id -u)`.
- **Sessions keep asking for `/login`.** Two machines share one sign-in. Sign this one in on its own, or
  use setup tokens ([claude-accounts](claude-accounts.md)).
- **A session sits at "Do you trust this folder?"** It was started in the home folder or a folder never trusted. Start
  it in a project folder.
- **Vite answers 403 "Blocked request"**, or **`crypto.randomUUID` is not a function**: see
  [Dev servers on a remote machine](#dev-servers-on-a-remote-machine).

Related: [Set up a node that keeps running](node-services.md), [Start on your laptop](laptop.md).
