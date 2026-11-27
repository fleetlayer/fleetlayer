# Keep notes, skills and instructions the same on every machine

An agent is only as useful as what it knows about you: the instructions you gave it, the skills you wrote, the notes it
reads and the working notes it left itself last week. When all of that lives on your laptop, an agent on your home
server or a second laptop starts from nothing. It repeats mistakes you already corrected and asks questions you already
answered. Copying files by hand works for a day, then the copies drift and nobody can say which one is current.

This guide sets up sync so every machine sees the same notes, skills and instructions, and each kind of file is synced
the way that suits it. You decide what is shared and which machine is the source of truth for each thing; your agents
set it up and keep it working. The steps are written so an agent can follow them, and the few that need you are marked.

This uses `laptop` and `server` for machines and nodes. Change them to yours.

## The shape

Three kinds of files, three answers:

| What | How it syncs | Why |
|---|---|---|
| A notes vault you and your agents both edit | Two-way and live (Syncthing or similar) | Anyone may write anywhere |
| Skills and shared instructions | One-way from one source (the file-sync plugin) | One place to edit |
| Sign-ins, per-machine settings, build output | Never | Secret, per machine, or rebuilt |

Under all of it sits a backup with history ([Backups](backups.md)). Sync copies a bad edit or a deletion to every
machine just as faithfully as a good one, so it isn't a backup.

## What goes where

**Two-way, in the vault:**

- your notes, if agents read or write them;
- each agent's kept scratchpad, a folder per agent named after its role;
- pages and reports agents write for you ([Pages](pages.md)).

**One-way, published from one source:**

- skills (`~/.claude/skills/`);
- the shared instructions, one `AGENTS.md` for every machine;
- subagent definitions and other configuration kept as text.

**Never synced:**

- **Sign-ins:** `~/.claude/.credentials.json`, extra account folders, the macOS Keychain, `gh` and cloud CLI tokens.
  Two machines refreshing the same OAuth token race each other and sign each other out, so sign in on each machine
  ([Claude accounts](claude-accounts.md) covers Claude's).
- **Per-machine state:** `~/.claude.json`, `settings.local.json`, session and cache folders.
- **Per-machine instructions** ([step 3](#3-per-machine-instructions)).
- **Dependency trees and build caches:** `node_modules`, `.next`, `.turbo`, `.cache`, `target`, `.venv`,
  `__pycache__`.
- **Live session transcripts.** They're large, append-only and written constantly: back them up one way instead.

## 1. The notes vault: two-way sync

A vault is edited by you on your phone and laptop and by agents on every machine, so it needs a tool that syncs both
ways and keeps running. Pick by what you already run:

- **Syncthing** syncs any folder peer to peer, with no server or account, and runs as a daemon on macOS and Linux.
  Conflicts become `.sync-conflict-*` files that someone cleans up by hand. Turn on file versioning for some history.
  It copies symlinks as links.
- **Obsidian Sync** suits a vault you also use on your phone: encrypted, versioned, nothing to run. It's paid, with a
  storage cap, needs the Obsidian app or its headless client on each machine, and is built for vaults rather than
  `~/.claude`.
- **Self-hosted LiveSync** (an Obsidian plugin on CouchDB) is the same idea on a server you run. It skips hidden files,
  the first sync of a big vault is slow, and when two edits touch the same lines the newer one wins.
- **A cloud drive** (Google Drive for desktop, `rclone bisync`) reuses storage you already pay for, with version
  history. There's no official Linux client, so `rclone bisync` runs on a timer instead of live, and the macOS client
  can keep files online-only, which an agent reads as missing.

The steps below use Syncthing, the simplest fit for a folder that agents write.

1. Install Syncthing on each machine and run it as a service of your user, so it starts at login and keeps running.
2. Share the vault folder from the first machine to the others. **You accept each new device and folder** in the other
   machine's Syncthing, since pairing is a trust decision.
3. Turn on file versioning for the folder on at least one always-on machine.
4. Add an ignore list before the first sync, in `.stignore` at the top of the folder:

   ```text
   node_modules
   .next
   .turbo
   .cache
   target
   .venv
   __pycache__
   ```

   A pattern without a leading `/` matches at any depth. Agents build apps inside synced folders, and one dependency
   tree is hundreds of thousands of files that sync slowly, churn every backup and can be rebuilt from the lock file.
   The same names go in your backup excludes ([Backups](backups.md#what-not-to-back-up)).
5. Add the check in [step 4](#4-check-that-sync-is-working) in the same change.

## 2. Skills and instructions: one-way, from one source

Skills and shared instructions change less often, and when two machines edit them at once you want one clear answer.
So they're edited in one place and published to the rest. Fleetlayer's file-sync plugin (`fleetlayer-file-sync`,
bundled with every release) does that:

- **The source node watches a folder** and publishes a new version when its contents change (checked every
  `every_minutes`, 5 by default).
- **Each version is sealed** to each target node's own key before it leaves the source, so the control plane relays
  it without being able to read it, and doesn't store it.
- **Each target makes its folder match**: every file written, and files the plugin wrote for an earlier version
  removed. A file it didn't write is never touched. It keeps its record of what it wrote in `.fleetlayer-files.json`
  in that folder.
- **Text files only**, at most 1 MB each and 20 MB a set. Files and folders whose names start with `.` are left out.

Pick the source for each set: the machine where you and your agents edit skills. Make it an always-on machine if you
can (see "When it doesn't work"). Here the source is `server` and the target is `laptop`.

### On the source node

Add the plugin to the source's `~/.config/fleetlayer/node.yaml`:

```yaml
plugins:
  - name: file-sync
    command: fleetlayer-file-sync
    grants: [sync.publish]
    config:
      publish:
        - { set: skills, dir: ~/.claude/skills, to: [laptop], every_minutes: 5 }
        - { set: instructions, dir: ~/fleet/instructions, to: [laptop] }
```

- `set` names the set. Its bundle is `file-sync/<set>`, which is how access rules name it.
- `dir` is the folder to publish. `~` is the node user's home.
- `to` lists the target nodes by name.
- The `sync.publish` grant lets the plugin publish through the node at all.

### On each target node

```yaml
plugins:
  - name: file-sync
    command: fleetlayer-file-sync
    config:
      apply:
        - { set: skills, dir: ~/.claude/skills }
        - { set: instructions, dir: ~/fleet/instructions }
```

The target's `dir` can differ from the source's. Restart each node so it reads the change (`fleetlayer-node install`
rewrites the service and restarts it, on Linux and macOS), then check that `fleetlayer status` lists `file-sync` with
the role `sync_source` on the source and `sync_target` on each target.

### Remove hand-made copies once

A target never overwrites a file it didn't write. If `laptop` already has its own `~/.claude/skills/review/SKILL.md`,
the published one is skipped, and the node's log says `leaving … alone: it wasn't written by fleetlayer-file-sync`.
Before the first sync, move each target's existing copies aside (or delete the ones the source has too), so the
published versions land.

### Only the source may send

A target writes whatever arrives for that set into the folder you named, so only the source should be allowed to send
it. In your access policy ([Access control](../access-control.md)), forbid every other node:

```yaml
version: 2
default: allow
deny:
  - who: "*"
    except: "-@server"          # the source node
    can: [sync]
    to: ["sync:file-sync/*@*"]
```

A publish is checked as the source node (`-@server`) doing `sync` to `sync:file-sync/<set>@<target>`. Check the rule
before you apply it:

```sh
fleetlayer-control acl explain --from=-@server --can sync --to sync:file-sync/skills@laptop --policy policy.yaml
fleetlayer-control acl explain --from=-@laptop --can sync --to sync:file-sync/skills@server --policy policy.yaml
fleetlayer-control acl apply policy.yaml
```

The first should allow, the second deny.

**You approve the first sync to each target.** Unless your policy turns asking off (`ask: []`), Fleetlayer asks before
a node's first `sync` to each target. List the waiting requests and approve them:

```sh
fleetlayer-control approvals list
fleetlayer-control approvals approve <id>
```

From v0.3.9 (on the source, the targets and the control plane), the source sends within about 15
seconds of your approval, and the approval renews each time it's used, so it lapses only after 30 days without a
sync. A target that hasn't got the latest version is retried with backoff (a minute, doubling to an hour). A refusal
by your access rules, or a request you denied, waits for the next version instead. Upgrade targets before sources.

## 3. Per-machine instructions

Some instructions hold on one machine only: this one has no browser, that one can't use Touch ID, this is how to reach
the others. Keep them in one small file per machine that is never synced, and have each machine's entry point pull in
both the shared file and its own. With Claude Code, `~/.claude/CLAUDE.md` on each machine is just two imports:

```text
@~/fleet/instructions/AGENTS.md
@~/.claude/CLAUDE.this-machine.md
```

The shared file arrives through the `instructions` set; `CLAUDE.this-machine.md` is written by hand on that machine
and stays there. If you sync `~/.claude` with a two-way tool instead, put `CLAUDE.this-machine.md` in its ignore list.
What goes in each file: [Instructions that follow your agents to every machine](agent-instructions.md).

## 4. Check that sync is working

A sync tool that has lost its connection keeps running, so "is the process up" tells you nothing. Check the work: have
one machine write a timestamp into the synced folder, and have another machine check that it arrived recently. Then
push the result as a heartbeat, so a stall raises an alert ([Monitoring](monitoring.md)).

For the instructions set, add a file next to `AGENTS.md` on the source, rewritten every hour by a timer:

```sh
date +%s > ~/fleet/instructions/sync-check.txt
```

On each target, an hourly job checks its age and reports it:

```sh
#!/bin/sh
token=$(cat ~/.config/fleetlayer/instructions-heartbeat-token)
url="http://fleetlayer.localhost:7680/api/v1/endpoints/fleetlayer_instructions/external"
age=$(( $(date +%s) - $(cat ~/fleet/instructions/sync-check.txt 2>/dev/null || echo 0) ))
if [ "$age" -lt 7200 ]; then
  curl -fsS -X POST -H "Authorization: Bearer $token" "$url?success=true"
else
  curl -fsS -X POST -H "Authorization: Bearer $token" "$url?success=false&error=instructions+not+synced+for+${age}s"
fi
```

Name the heartbeat in the target's gateway.yaml, or let Gatus take it on a server (both in [Monitoring](monitoring.md)):

```yaml
heartbeats:
  instructions: { every: 3600, token_file: ~/.config/fleetlayer/instructions-heartbeat-token }
```

Do the same for the vault: each machine writes its own file (`sync-check/laptop.txt`) and checks the others'. One
check covers one path, so give each set and each pair of machines you depend on its own.

For the file-sync plugin, Fleetlayer also reports on itself (from v0.3.9). Each bundle and target has a
`sync.status` reading (its state, the error, and what to do about it, such as the approve command to run), and each
set a `file-sync.set` reading (its files, what was skipped and why, its version):

```sh
fleetlayer readings sync.status file-sync.set
```

The node's own heartbeat fails when a sync has waited on a person (an approval, a changed key, an access rule) for
over an hour, so an alert reaches you without a check of your own.

## Who writes where

- **The vault:** anyone, anywhere. Two-way tools handle edits to different files well. Two edits to the same file
  before they meet give a conflict copy (Syncthing), or the newer edit wins
  (LiveSync, for the same lines). Agents keep conflicts rare by writing
  to their own folders: each agent's scratchpad is named after its role, and pages go in the agent's own space.
- **Published sets:** only at the source. An edit made on a target is overwritten by the next version and never
  travels back. Put that in your shared instructions, so an agent on `laptop` that wants to change a skill asks a
  session on the source machine to do it (`fleetlayer msg send --to skills@server "…"`) instead of editing its copy.
- **Per-machine files:** only on that machine.

## Other ways to do it

- **git** (a private repo, with a timer that commits and pulls) gives you full history, diffs and review, and suits
  skills and instructions well if you already review changes that way. It isn't live, a merge conflict stops the timer
  until someone resolves it, and sign-ins must never be committed.
- **rsync on a timer** is a fine one-way copy between machines that reach each other over SSH. It has no record of
  what it wrote, so `--delete` removes files made by hand on the target, and without `--delete` removed skills linger.
- **A cloud drive** works for a vault if you already pay for one; see the list in step 1 for its limits on Linux and
  with online-only files.
- **Syncthing for everything** is simple and many people use it for `~/.claude` too. You trade the file-sync plugin's
  single source and "never touches files it didn't write" for edits from anywhere and conflict files to clean up.

## When it doesn't work

- **A target never gets a skill.** Look in the source node's log (`journalctl --user -u fleetlayer-node` on Linux,
  `~/Library/Logs/fleetlayer-node.log` on macOS; `fleetlayer status --json` shows each plugin's last 50 log lines).
  The publish result names each target with an error: offline, no key yet, a changed key, or refused by your access
  rules. On the target, `leaving … alone` means a hand-made file is in the way.
- **Nothing publishes at all.** One file over 1 MB, or a set over 20 MB, stops the whole set, and the source logs
  `reading <dir>: … is over 1 MB` every interval. Move large files out of the set.
- **A file is missing on the target.** Files that aren't valid UTF-8 text (images, binaries) are skipped, and anything
  whose name starts with `.` is left out. A symlink that points inside the folder arrives as the same symlink; one that
  points outside it, to nothing, or to a hidden file is skipped. So a skills folder made of links into a repository
  publishes nothing: publish the folder the links point to. The `file-sync.set` reading lists what was skipped and why
  (older releases skip every symlink without saying so).
- **A script in a skill won't run on the target:** the target runs a release older than v0.3.9, which
  writes plain files and drops the executable bit. Upgrade it, or have the skill call its scripts through their
  interpreter (`sh script.sh`, `python3 tool.py`).
- **Sync stopped and nobody noticed:** on releases up to v0.3.8, an approval held 30 days whether used or not, and a
  refused version waited for the folder to change. Upgrade the source, the targets and the control plane; the
  `sync.status` reading then says what each target is waiting for.
- **A target waits while its source is offline.** Bundles are sealed to each target and never stored, so a target
  that comes back gets the latest version only once the source is linked too (from v0.3.9, the
  control plane asks the source to resend whenever either side links). Keep the source on an always-on machine.
- **Something that watches a synced folder misses changes.** Filesystem events (inotify) see only writes made through
  the local kernel, so a folder written over a network share, or shared into a VM, never fires an event for writes made
  on the other side. Anything that reacts to new files there has to poll; [Pages](pages.md#when-it-doesnt-work) does.
- **A dropped share looks like an empty folder.** A sync target or a service reading a folder that has gone away sees
  no files rather than an error. Keep a marker file in the folder and fail the check when it's missing, as pages does
  with `.fleetlayer-space`.
- **Project memories don't show up on the other machine.** Claude Code names each project's folder after its absolute
  path, so `/Users/you/code/app` on a Mac is `-Users-you-code-app` and `/home/you/code/app` on Linux is
  `-home-you-code-app`. Keep projects at the same path on every machine, or sync the memories through one shared folder
  and link each machine's `memory/` to it.
