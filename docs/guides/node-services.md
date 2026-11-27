# Set up a node that keeps running

You've added a second machine to your fleet: a Linux server, a Mac, or a second Unix user on a machine you already
use. Running `fleetlayer-node` in a terminal works until you close the terminal, log out, reboot or update. This guide
sets the node up as a service that starts at boot, stays up when nobody is logged in, updates itself, and tells a
monitor when it stops doing its job.

Ask your agent to add the machine. The steps below are what it does, run as the user the node will run as. A few need
you in person (a sudo password, a macOS privacy prompt); those are marked **You:**.

The shape:

- **Installed by the installer**, so it can update itself from later releases.
- **Enrolled with a token** from your control plane, kept in a file only its user can read.
- **Run by the machine's own service manager** for its user: a systemd user service on Linux, a launchd agent on
  macOS. `fleetlayer-node install` writes and starts it.
- **Watched by a heartbeat** that goes red when a plugin stops or the link to the control plane drops, and stops
  arriving when the node dies.

If everything still lives on one laptop, [`fleetlayer up`](laptop.md) does all of this (and starts the control plane,
the gateway and the rest) in one command. This guide is for a node that joins a fleet whose control plane is elsewhere.
The examples use `server` for the new node and `control.example.com` for the control plane.

## 1. Install the binaries

As the user the node will run as, on the new machine:

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
```

It puts the latest release in `~/.local/lib/fleetlayer/`, checked against the release's checksums, and links each
binary from `~/.local/bin`. If it prints "Add ~/.local/bin to your PATH", do that before step 3: the service
remembers the PATH you install it with.

## 2. Enrol it

On the machine that runs the control plane:

```sh
fleetlayer-control node add server
```

It prints the node's token once. Enrolling the same name again replaces the token. Since v0.3.8 it finds the database
`fleetlayer up` made by itself; on a control plane that keeps it elsewhere, add `--db <path>` (or set
`FLEETLAYER_CONTROL_DB`).

Back on the new machine, save the token where only this user can read it, without putting it in a command line or
shell history (piped over SSH from the control plane's machine, say):

```sh
mkdir -p ~/.config/fleetlayer
(umask 077; cat > ~/.config/fleetlayer/node-token)     # the token on standard input
```

`~/.config/fleetlayer/node.yaml`:

```yaml
name: server                     # the name you enrolled
control:
  url: wss://control.example.com/v1/link     # ws:// only on a private network
  token_file: ~/.config/fleetlayer/node-token
plugins:
  - name: herdr
    command: fleetlayer-herdr
  - name: claude-code
    command: fleetlayer-claude-code
```

Set `name:` to the enrolled name. Without it the node names itself after the machine (plus `-<user>` for a user other
than the machine's first), and if that differs, its log says "the control plane knows this node as …".

## 3. Install it as a service

```sh
fleetlayer-node install
fleetlayer status
```

`install` reads node.yaml first and refuses a broken one, so a typo doesn't become a service that restarts forever.
Then it writes the service file and starts it:

| | Service file | Logs |
|---|---|---|
| Linux | `~/.config/systemd/user/fleetlayer-node.service` | `journalctl --user -u fleetlayer-node` |
| macOS | `~/Library/LaunchAgents/dev.fleetlayer.node.plist` | `~/Library/Logs/fleetlayer-node.log` |

Both restart the node 10 seconds after it exits. The service runs `~/.local/bin/fleetlayer-node` when that's the
binary you ran, and it gets the PATH of the shell you ran `install` from, which is where it finds its plugins, herdr
and `claude`. So run `fleetlayer-node install` again after you change your PATH, move the binary or edit node.yaml.
On macOS it unloads the old copy and loads the new one; on Linux it restarts the service.

`fleetlayer status` should say `linked to the control plane` and list each plugin as running. `--no-start` writes the
service file without starting it, and `fleetlayer-node uninstall` stops the service and removes its file.

## 4. Keep it running when you log out

**On Linux**, a user's systemd services stop when that user's last session ends, and start again only when they log
in. Turn on lingering so they start at boot and stay up (`fleetlayer-node install` prints the same reminder):

```sh
loginctl enable-linger "$USER"
```

**You:** for another user it takes sudo, `sudo loginctl enable-linger <user>`, so it's yours to type unless your agent
already has sudo on that machine.

**On macOS**, the node runs as a launchd agent in your own login session, never as a system-wide LaunchDaemon. Plugins
such as [claude-auth-manager](../plugins/claude-auth-manager.md) write Claude Code's sign-in to your login keychain,
and that needs the keychain unlocked. A LaunchDaemon, or an SSH session on a Mac whose keychain is locked, fails with
"User interaction is not allowed".

## 5. Updates

A node installed by the installer checks for a new release every 6 hours (the first check 5 minutes after it starts).
When there is one, it downloads it, checks it against the release's checksums, unpacks it next to the old one,
repoints the links in `~/.local/bin`, and exits so the service manager starts the new version. Choose how much of that
it does in node.yaml:

```yaml
update:
  mode: auto        # auto: install it. notify: only log that it's out. off: don't check.
  every_hours: 6
```

The default is `auto` for a node the installer put in `~/.local/lib/fleetlayer/`, and `notify` for one installed any
other way (a package, Ansible, a build from source), since whatever installed it should update it. Such a node only
logs that a release is out, even with `mode: auto`.

Releases are tagged once per set of features, and each one can need a backup or a look at the release notes first.
Set `mode: notify` on a machine where you want to choose the moment.

**Upgrading by hand.** Rerun the installer, then `fleetlayer-node install`. A node installed before 26 November 2026
wrote the versioned path (`~/.local/lib/fleetlayer/<version>/fleetlayer-node`) into its service file, so swapping
the links never reached it and it kept running the old binary. Rerunning `install` writes the link instead.

**Where the files are, and going back.** From v0.3.13 the live binaries sit directly in `~/.local/lib/fleetlayer/`
and keep the same paths from one release to the next: an upgrade copies each new binary beside the old one and renames
it over it. `VERSION` there names the live release, and `versions/<tag>/` keeps the live release and the one before as
they were unpacked. To go back, copy the binaries from `versions/<tag>/` over the live ones and restart the node (set
`update: {mode: notify}` first, or it updates again). An install from before v0.3.13, with a folder per release, moves
itself to this layout the first time a v0.3.13 node starts, and restarts once. The fixed path is what lets macOS keep
the permissions you gave the node across upgrades ([Releases and updates](../releasing.md)); that first move is a new path, so
macOS asks once more.

**What a self-update relinks.** The node repoints the links for every binary in the release: `fleetlayer`,
`fleetlayer-node`, the plugins, and (from v0.3.8) `fleetlayer-control`, `fleetlayer-gateway` and `fleetlayer-pages`.
On a machine still running an older node that runs those three, rerun the installer once.

**New binaries in a release.** A release can add a binary the old node didn't know, so the update that installed it
didn't link it. When the new node starts from the installer's folder, it links any of its release's binaries that have
no link in `~/.local/bin` yet. It never replaces a file or link that's already there, so a link you pointed elsewhere
stays yours. A node installed some other way doesn't do this: add the new binary to whatever put the others on PATH.

## 6. Monitor it

A service manager restarts a node that crashes. It doesn't notice a node that's running but has lost its link, or
whose plugin keeps failing. The heartbeat does:

```yaml
heartbeat:
  url: https://gatus.example.com/api/v1/endpoints/fleetlayer_server/external
  token_file: ~/.config/fleetlayer/heartbeat-token
  every: 60
```

From v0.3.8 the node logs an error when it can't read `token_file` (older nodes sent reports without a token, and
didn't expand `~` there). A monitor that refuses a report shows in the node's log as
`heartbeat: <url> answered <status>`, so check the log once after setting this up.

After a minute's grace at start, the node posts `?success=true` every `every` seconds, or `success=false` with the
reason when an enabled plugin isn't running or the control plane link is down. When the node dies, the reports stop,
and the monitor's own timeout raises the alert. The URL takes the form Gatus external endpoints use; the
[laptop gateway](laptop.md#when-something-stops) takes the same heartbeats, if you don't run Gatus.

## A second Unix user on the same machine

Each Unix user runs a node of their own, enrolled under its own name. A second user is the way to keep an agent you
trust less apart from yours: for one that reads email or other text strangers wrote, follow
[Run an agent that reads strangers' text](untrusted-input.md), which adds the access rules.

On Linux, turn on lingering for the new user before anything else (`sudo loginctl enable-linger <user>`): it starts
that user's systemd instance, which their services need. Then switch to them (**You:** both take sudo, unless your
agent has it). `sudo -iu <user>` gives you a login
shell without the variable systemd's user commands look for, so `systemctl --user` (and `fleetlayer-node install`,
which runs it) fails with "Failed to connect to bus". Set it yourself:

```sh
sudo -iu server
export XDG_RUNTIME_DIR=/run/user/$(id -u)
fleetlayer-node install
fleetlayer status
```

`fleetlayer status` needs it too: the node's socket is under `$XDG_RUNTIME_DIR` when the service runs, and the
command looks for it there.

## Test nodes on a machine where people work

A node you start by hand to try something (a policy, a plugin) shares the machine with the real node and with people
working in their tabs. Three things keep it out of their way.

**Turn off typing.** When a session has no messaging socket, the node types the message into the session's terminal
through herdr. A test node with only the herdr plugin always falls back to typing, so a test message lands in the
middle of what someone is typing in that tab. In the test node's node.yaml:

```yaml
typing_fallback: false     # such messages fail at once instead
```

**Give it its own instance.** From v0.3.9, `FLEETLAYER_INSTANCE` runs a second node under the same
account without touching the real one: its own launchd label or systemd unit (`dev.fleetlayer.test.node`,
`fleetlayer-test-node.service`), its own config and state under the `HOME` you give it, and its own socket. Pointing
`XDG_CONFIG_HOME` somewhere else isn't enough: `fleetlayer-node install` would replace the real node's service.

```sh
export FLEETLAYER_INSTANCE=test HOME=~/fleetlayer-test
mkdir -p "$HOME/.config/fleetlayer"
cat > "$HOME/.config/fleetlayer/node.yaml" <<'YAML'
name: server-test
typing_fallback: false
YAML
fleetlayer-node install
fleetlayer status          # the same two variables point the command line at the test node
```

Enrol it under its own name if it should link to the control plane. Open a new shell to talk to the real node again;
`fleetlayer-node uninstall`, run with both variables set, removes the test node's service.

**Stop it carefully.** `pkill -f fleetlayer-node` also matches the shell running that command (its own command line
contains the pattern) and kills it. Put a bracket in the pattern so it can't match itself, and never use
`pkill -x fleetlayer-node` where the node runs as a service, since that stops the real one too:

```sh
pkill -f 'bin/[f]leetlayer-node'
```

## When it doesn't work

- **"No route to host (os error 65)" on a new Mac.** On macOS 15 and later, a program launchd starts that isn't
  Apple's can't reach addresses on your local network until it's allowed. **You:** accept the prompt on the Mac. The
  node retries with backoff, so it usually links once you do. If a fresh install keeps failing this way, check System
  Settings › Privacy & Security › Local Network.
- **"Bootstrap failed: 5: Input/output error"** when reinstalling on macOS: `launchctl bootout` returns before launchd
  has unloaded the service, and loading it again too soon fails. `fleetlayer-node install` waits until the old copy is
  gone and retries once. If you reload the plist by hand, wait the same way.
- **"Failed to connect to bus"** on Linux: `XDG_RUNTIME_DIR` isn't set, or the user has no systemd instance yet. See
  [A second Unix user](#a-second-unix-user-on-the-same-machine).
- **The node stops when you log out** on Linux: `loginctl enable-linger` isn't on for that user.
- **A plugin keeps restarting with "couldn't start fleetlayer-herdr: No such file or directory"**: the service's
  PATH doesn't include where it lives. Fix your PATH (or give the plugin's `command:` a full path) and run
  `fleetlayer-node install` again.
- **"the control plane knows this node as …"**: `name:` in node.yaml doesn't match the name you enrolled. Set it to
  the name in the message.
- **The node keeps running an old version after an upgrade**: its service file names a versioned path. Run
  `fleetlayer-node install` again.
- **A command from a new release isn't found**: see "New binaries in a release" above.
- **"another node is already running on <socket> (pid N)"** (from v0.3.9, after the node has waited 10 seconds): a
  node already serves that socket, usually the real one while you start a second by hand. Stop it first, or give the
  second one its own instance with `FLEETLAYER_INSTANCE` (see "Test nodes" above).
- **"<path> exists and isn't a socket"**: something else is at the socket's path. Move it away, or set `socket:` in
  node.yaml.

Related: [getting started](../getting-started.md), [the node's link to the control plane](../link.md).
