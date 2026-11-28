# Agents on a Mac, inside a Linux VM

An agent on your Mac runs as you. It can read your home folder, your SSH keys and your browser's files, and one bad
command reaches all of them: an `rm -rf ~` from a confused cleanup, or a `pkill -f node` that also ends your editor,
your chat app and every other agent's dev server. Put the agents in a Linux VM on the Mac instead. The same mistake
then hits only the VM, and the agents still do all their work: worktrees and databases on the VM's own disk, dev servers
you open in the Mac's browser, and your Mac's files and signed-in browser through Fleetlayer's plugins.

The shape:

- **A Linux VM made with [Lima](https://lima-vm.io)** (Apache 2.0), using Apple's own virtualisation, with **no
  folders shared from the Mac**.
- **Only the ports you choose forwarded** to the Mac's `localhost`, so dev servers open in your browser and nothing
  else of the VM's lands on the Mac.
- **Set up like a [server VM for agents](agents-vm.md):** a Unix user per role without sudo, herdr, Claude Code, and a
  Fleetlayer node per user.
- **Your Mac's files and browser reached through Fleetlayer**, never mounted.
- **What needs the Mac itself stays there:** Xcode, signing, Keychain and Touch ID, Claude in Chrome, computer use.

The examples call the VM `agents`, the agents' user inside it `agent`, and your Mac `your-mac`. Ask your agent to do the
steps; the ones only you can do are marked **You:**. Each section ends with a check an agent can run.

## Why a VM

macOS has no good native containers for this. Its sandbox profiles are deprecated and hard to write for a whole
toolchain, and a second macOS user still shares a lot with the first (`/Applications`, `/tmp`, Homebrew's folders,
whatever you made world-readable). Docker Desktop and its rivals run containers in a Linux VM already, but a container
is a poor home for a long-lived agent with herdr tabs, services and a systemd of its own.

A Linux VM is a whole machine with its own disk, users and process table. Set up like a server's agents VM, everything
the server guides say applies to it, and an agent inside can't see the Mac's files or processes at all.

The VM is not an always-on machine: when the Mac sleeps, the VM sleeps with it. Agents that must keep running belong on
[a server VM](agents-vm.md).

## 1. Install Lima

**You:** on the Mac, with Homebrew:

```sh
brew install lima
```

This guide needs Lima 2.0 or later (it uses the `base:` field and Apple's virtualisation, `vz`, by default). On Apple
silicon the VM is arm64, so everything you install inside must have an arm64 Linux build. Most tools do; see
[Browser tests](#9-browser-tests) for the one that used to be missing.

**Check:** `limactl --version` prints 2.0 or later.

## 2. The VM's settings

Lima's own templates share your home folder with the VM, read-only. Read-only sounds safe, but it means every agent in
the VM can read `~/.ssh`, your shell history, your browser profiles, every `.env` file in every project, and your
cloud credentials. That's most of what the VM is meant to keep away. So start from the Debian image alone, without
Lima's default mounts, and set `mounts: []`.

Save this as `agents.yaml`:

```yaml
minimumLimaVersion: 2.0.0
base: template:_images/debian-13   # the image only, without template:_default/mounts
vmType: vz
cpus: 6
memory: 12GiB
disk: 150GiB
mounts: []                         # nothing from the Mac: the agents reach your files through Fleetlayer
ssh:
  localPort: 60022                 # a fixed port, so herdr's SSH config doesn't change at every start
portForwards:
  - guestPortRange: [3000, 3099]   # dev servers
  - guestPortRange: [5173, 5199]   # Vite
  - guestIP: 0.0.0.0               # every other port, on any address: not forwarded
    proto: any
    ignore: true
```

**Sizing.** The VM takes what you give it from the Mac, so leave the Mac enough to stay quick:

- **CPUs:** about half the Mac's cores. Builds and test runs in the VM use all of them while they run.
- **Memory:** about a third to a half of the Mac's memory. 12 GB of a 32 GB Mac runs a few agents, Docker with a
  database or two and a browser test run. Under 8 GB, a test suite and a database fight for memory.
- **Disk:** size it from your work: worktrees times their size (a checkout, its dependencies, its build cache, its
  database), plus Docker's images, plus 20 GB for the system. The disk image is sparse and grows as the VM writes, so a
  big number costs nothing at first, but it grows into the Mac's free space. Check that space before the trial, not
  after: a Mac's disk is often the first thing to run out, and Docker Desktop's own VM disk may already take tens of
  gigabytes. The disk can be grown later with the VM stopped (`limactl edit agents --disk 200`), never shrunk.

Rosetta isn't needed: it runs x86 Linux programs in the VM, slower, and only for a tool with no arm64 build.

**Check**, once the VM is started (section 4): `limactl shell agents -- findmnt -t virtiofs,9p,fuse.sshfs` prints
nothing, and `limactl shell agents -- ls /Users` fails with "No such file or directory".

## 3. Ports: only the ones you choose

With no `portForwards` rules, Lima forwards every TCP port a program in the VM listens on to the same port on the
Mac's `127.0.0.1`. That's handy for a dev server and a trap for everything else.

**The trap: the VM's ports shadow the Mac's.** Say Docker Desktop on the Mac publishes Postgres on port 5432, which
binds every address (`*:5432`). Then an agent starts a Postgres container in the VM, on 5432 too, and Lima binds the
Mac's `127.0.0.1:5432` for it. macOS lets both bind, and sends connections to `localhost:5432` to the more specific
one: the VM's. Your Mac's apps and tests now talk to the agent's database, with no error. The same goes for Redis,
MinIO, a dev server on 3000 you run yourself, or anything else both sides run.

The rules in `agents.yaml` forward only the ranges you list and ignore every other port. Lima checks rules in order and
the first that matches wins, so keep the `ignore` rule last. Pick ranges your Mac doesn't use itself, and tell the
agents to start dev servers in them (put it in the project's `AGENTS.md`). Change the rules with `limactl edit agents`
and restart the VM.

Forwarded ports bind only the Mac's `127.0.0.1`, so nothing in the VM is reachable from your network.

**The other direction.** The VM can usually reach services on the Mac's `localhost` through `host.lima.internal`.
Anything on your Mac that listens there without a password (a database, a dev tool's admin page) is reachable from the
VM's agents. Give such services a password, or stop them while you don't need them.

**Check:** on the Mac, `lsof -nP -iTCP -sTCP:LISTEN | grep -i lima` lists only ports in your ranges (and the SSH port).
After an agent starts a container on 5432 in the VM, `lsof -nP -iTCP:5432 -sTCP:LISTEN` on the Mac doesn't show Lima.

## 4. Start it, and start it at login

```sh
limactl create --name=agents agents.yaml
limactl start agents
limactl start-at-login agents        # a launchd agent starts the VM when you log in
```

The first start downloads the Debian image and takes a few minutes. Lima makes a user in the VM with your Mac's
username and passwordless sudo. That's your admin account for the VM, reached with `limactl shell agents`. No agent
runs as it.

**Check:** `limactl list` shows `agents` as `Running` with `vz`, and the CPUs, memory and disk you set.

## 5. A user per role

Inside the VM, as the admin user (`limactl shell agents`), make a user for the agents, without sudo, and turn on
lingering so its services run with nobody logged in:

```sh
sudo useradd --create-home --shell /bin/bash agent
sudo loginctl enable-linger agent
```

This is the same as step 2 of [A server VM for your agents](agents-vm.md), and the same reasoning holds: every process
of a Unix user can act as any of that user's sessions, so an agent you trust less (one that reads email or web pages)
gets a user of its own, with its own node ([An agent that reads strangers' text](untrusted-input.md)). Install system
packages as the admin user, or from a script you keep (cloud-init, Ansible). Agents never get root.

**Check:** `limactl shell agents -- sudo -u agent sudo -n true` fails ("a password is required" or "not in the
sudoers file"), and `loginctl show-user agent -p Linger` says `Linger=yes`.

## 6. herdr and Claude Code

Lima writes an SSH config for the VM. **You:** include it at the top of the Mac's `~/.ssh/config`:

```text
Include ~/.lima/*/ssh.config
```

That gives the VM the host name `lima-agents`. Lima's config uses its own key (`~/.lima/_config/user`) and only that
key, so let that key log in as `agent`:

```sh
limactl shell agents -- sudo install -d -m 700 -o agent -g agent /home/agent/.ssh
limactl shell agents -- sudo install -m 600 -o agent -g agent /dev/stdin /home/agent/.ssh/authorized_keys \
  < ~/.lima/_config/user.pub
```

Then follow step 4 of [A server VM for your agents](agents-vm.md#4-herdr-and-claude-code) as `agent`: herdr installed
system-wide by the admin user (the arm64 Linux build, the same version as the Mac's), its server as a user service, and
Claude Code signed in on this machine or with setup tokens ([Many Claude accounts](claude-accounts.md)). On the Mac:

```sh
herdr machine add agent@lima-agents --label vm
```

**Check:** on the Mac, `ssh agent@lima-agents true` returns without a prompt, and the Mac's herdr shows the `vm`
machine with a tab you can type in.

## 7. A Fleetlayer node per user

Each user in the VM gets its own node, exactly as on any Linux machine:
[Set up a node that keeps running](node-services.md). The installer picks the `aarch64-unknown-linux-gnu` release by
itself. Run it as `agent`, enrol the node with a name that says where it is (`your-mac-vm`), and install it as a
service from a shell whose PATH finds herdr and `claude`.

If the control plane runs on the Mac itself ([Start on your laptop](laptop.md)), point the VM's node at the Mac through
`host.lima.internal` and the control plane's port, for example `url: ws://host.lima.internal:7600/v1/link`. Plain `ws`
is fine here: the traffic never leaves the Mac. If the control plane is on a server, use its usual address.

**Check:** in the VM as `agent`, `fleetlayer status` says `linked to the control plane`, and on the Mac
`fleetlayer sessions` lists the VM's tabs.

## 8. Projects on the VM's own disk

Clone each project inside the VM, into `agent`'s home, and make worktrees there (`git worktree add ../app-feature
main`). Don't work on a checkout shared from the Mac: that brings back the mount you turned off, and shared folders are
much slower than the VM's own disk for the thousands of small files a project has.

Databases and other services go in the VM too. Install Docker Engine for arm64 Linux as the admin user, from Docker's
own apt repository. Each worktree can then run its own database. A user in the `docker` group is root in the VM in all
but name (the daemon runs as root), which is acceptable when the VM is the boundary, but never add a user you trust less
than the others to that group. Rootless Docker or Podman avoids it.

Language runtimes (mise, asdf, nvm and the like) install per user, as on any Linux machine.

**Check:** as `agent`, `df -h ~` shows the VM's disk with room for the next worktree, and `docker run --rm hello-world`
works.

## 9. Browser tests

Chrome for Testing, the pinned Chrome builds that Selenium, Puppeteer and many test suites use, ships `linux-arm64`
builds of Chrome, ChromeDriver and the headless shell since version 153. So a project's browser tests run in the VM
with the same pinned browser as everywhere else, and there's no need for a distribution's Chromium package. A project
that pins an older Chrome has to move to 153 or later first: older versions were built for `linux64` (x86) only.

To find the download for the current stable version:

```sh
curl -s https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json \
  | jq -r '.channels.Stable.downloads[][] | select(.platform == "linux-arm64") | .url'
```

Chrome needs some system libraries a minimal Debian lacks. If it won't start, `ldd chrome | grep 'not found'` lists
them; install them as the admin user.

**Check:** in the VM, `chrome-headless-shell --version` (or `chrome --version`) prints the version the project pins, and
one browser test passes.

## 10. Dev servers in the Mac's browser

An agent starts a dev server in the VM on a port in your forwarded ranges, and you open `http://localhost:<port>` on
the Mac. Because the address is `localhost`, the problems a server VM has don't arise: Vite allows `localhost` without
an `allowedHosts` entry, and the browser treats it as a secure context, so `crypto.randomUUID` and the rest work.

The dev server must listen on `127.0.0.1` or `0.0.0.0` inside the VM. Both are forwarded.

**Check:** with a dev server running in the VM on port 5173, `curl -sI http://localhost:5173` on the Mac answers.

## 11. Your Mac's files and browser, through Fleetlayer

Nothing of the Mac is mounted, so the agents reach what they need from it the same way agents on a server do, through
the Mac's node, under your [access policy](../access-control.md):

- **Screenshots, downloads and other folders you choose:** [Your screenshots and downloads](shared-folders.md). An
  agent in the VM uses `fleetlayer mcp shared-folders@your-mac`.
- **Your signed-in Chrome:** [Share your browser](browser.md), with `fleetlayer mcp browser@your-mac`.
- **Something to show you:** [Pages](pages.md), instead of a file on the Mac's desktop.

Each reaches only what you named, every use is logged, and you can take it away without touching the VM.

**Check:** from a session in the VM, the shared folders tool lists your screenshots folder, and the browser tool opens a
tab in your Mac's Chrome.

## What stays on the Mac

Some work needs macOS itself or you in person. Keep a session on the Mac, as you, for:

- **Xcode and iOS builds**, simulators, and anything else that only runs on macOS.
- **Code signing and notarisation**, with certificates in your keychain.
- **Keychain and Touch ID**: anything that asks for your fingerprint or unlocks a keychain item.
- **Claude in Chrome**, which talks only to Claude Code on the same computer. Sessions in the VM use the shared browser
  instead.
- **Computer use**: clicking around the Mac's own apps and screen.

Keep those sessions few and short, since they run with your full access.

## Speed

On Apple silicon with `vz`, the VM runs arm64 code natively. In a real trial, warm test runs (dependencies installed,
caches filled) took about as long in the VM as on the Mac. The first run in a fresh worktree is slower while caches
fill, as it would be on the Mac. File access on the VM's own disk is fast. Shared folders are what make Linux VMs on a
Mac feel slow, and this setup has none.

## Monitoring

The VM's nodes report like any node: their heartbeats go red when a plugin stops or the link drops, and stop arriving
when the node, or the whole VM, stops ([Monitoring](monitoring.md)). Expect them to stop while the Mac sleeps; if that's
an alert you'd act on, the agents belong on a server.

**Check:** the monitor you use lists a heartbeat for each of the VM's nodes, and it's green.

## OrbStack, in short

[OrbStack](https://orbstack.dev) is the polished alternative: fast, with the best macOS integration of any of these
(Linux machines and Docker in one app, names like `agents.orb.local`, little to configure). It's free for personal use
and needs a paid licence for business use.

Its integration is the catch. By default an OrbStack Linux machine sees all of `/Users`, read and write, and can run
commands on the Mac with `mac`. For agents, turn that off: make the machine isolated from the Mac (in OrbStack's settings,
or when you create the machine). Then the rest of this guide applies from section 5 on: a user per role, herdr, Claude
Code, a node per user, projects on the machine's own disk.

**Check:** inside the OrbStack machine, `ls /Users /mnt/mac` shows nothing of yours, and `mac uname` fails.

## When it doesn't work

- **The Mac's app or tests reach the wrong database** (or a port on the Mac is "already in use"): the VM's forwarding is
  shadowing the Mac's port. Add `portForwards` rules with `ignore` last (section 3).
- **A dev server in the VM doesn't open on the Mac:** its port isn't in a forwarded range, or it listens on another
  address than `127.0.0.1` or `0.0.0.0`.
- **`ssh agent@lima-agents` asks for a password or is refused:** Lima's key isn't in `agent`'s `authorized_keys`, or
  the `Include` line isn't at the top of `~/.ssh/config`.
- **The VM's tabs vanish from herdr after a restart:** the SSH port changed. Set `ssh.localPort` (section 2).
- **A download fails with "exec format error"** or a tool won't start: it's an x86 build. Find its arm64 Linux build
  (often named `aarch64` or `arm64`), and pin its checksum rather than taking "latest".
- **The Mac's disk fills up:** the VM's disk image grew. Delete finished worktrees and their databases in the VM, and
  `docker system prune` there. The image doesn't shrink by itself when the VM deletes files; `sudo fstrim -av` in the
  VM hands the freed space back to the Mac where the macOS version supports it.

Related: [A server VM for your agents](agents-vm.md), [Set up a node that keeps running](node-services.md),
[Start on your laptop](laptop.md).
