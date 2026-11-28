# Tell your agents about Fleetlayer

An agent on a Fleetlayer machine can message other sessions, start or resume sessions on any machine, use your
signed-in browser or your laptop's screenshots, and push pages for you to look at. It does none of this unless it
knows it can. Without being told, it asks you to copy a screenshot across, or says it can't reach the session next
door. And when a message arrives marked as outside data, or a call waits for your approval, it should know what that
means without you explaining it each time.

Two things fix that, and both are small:

- **Skills**, which an agent reads when the task calls for them: how to message, start sessions, use a shared tool,
  and what each refusal or mark means.
- **A few lines in your global instructions**, loaded in every session, that say the machine is on Fleetlayer and
  name the one command that lists what's available.

## The block for your global instructions

Paste this into the global instructions every agent loads on every machine: `~/.claude/CLAUDE.md` for Claude Code,
`~/.codex/AGENTS.md` for Codex (or `$CODEX_HOME/AGENTS.md`). Keep it this short, since it's read in every session.

```markdown
## Fleetlayer
This machine is on Fleetlayer: every agent session on every machine is `<tab>@<node>`, and `fleetlayer me` lists
what you may use from here (sessions to message, shared tools such as a signed-in browser or a laptop's folders,
your pages space). Read the `fleetlayer` skill before messaging, starting or resuming sessions, using a shared tool,
or when a message says it carries outside data or a call waits for approval.
```

If you keep one shared global file and a file per machine ([Instructions that follow your agents to every
machine](agent-instructions.md)), this block goes in the shared one: it's true everywhere.

## The skills

| Skill | Where it lives in the repo | What it's for |
|---|---|---|
| `fleetlayer` | `skills/fleetlayer/` | Addresses, messages and replies, who's running and what they did (summaries), starting and resuming sessions, shared tools, readings, outside-data lines, what to do when a call is held or refused |
| `fleetlayer-browser` | `skills/fleetlayer-browser/` | Using your signed-in Chrome from any machine: adding it, its own tab group, what to ask you before doing |
| `fleetlayer-shared-folders` | `plugins/shared-folders/skill/` | Your screenshots and downloads: the latest screenshot in one call, uploads and deletes where allowed, the refusals only you can fix |
| `fleetlayer-claude-accounts` | `plugins/claude-auth-manager/skill/` | Setup-token sessions: never `/login`, what's missing and what replaces it, reading usage |
| `pages` | `plugins/pages/skill/` | Pages for you to look at: reports, screenshots, mock-ups |
| `apps` | `plugins/pages/apps-skill/` | Small interactive apps behind your sign-in |
| `fleetlayer-signin` | `plugins/signin/skill/` | Running the sign-in service: putting a service behind it, inviting people and taking their access away, adding Google Workspace or another OpenID Connect provider, rotating keys |
| `fleetlayer-leases` | `plugins/leases/skill/` | `fleetlayer lease`: a slot on a shared build server before a heavy build or test run, or a claim you approve once, such as a mail filter while an agent watches CI |
| `fleetlayer-scheduler` | `plugins/scheduler/skill/` | Messages to a session on a schedule (every few hours, a cron expression) or once at a set time, and checking the entries |
| `fleetlayer-matrix` | `plugins/matrix/skill/` | Hearing a Matrix room's messages (an alerts room, a team room) as events, and answering in the room as its bot |
| `fleetlayer-cookies` | `plugins/cookies/skill/` | Using a site as you from the command line on a machine without your browser, with cookies copied from your signed-in browser |
| `fleetlayer-1password` | `plugins/1password/skill/` | 1Password items granted to the machine's role (`op://` references) without a 1Password token, and setting the plugin up |
| `fleetlayer-egress` | `plugins/egress/skill/` | Network access rules: what a refused connection looks like, asking for a host with a reason, and setting the rules up per role |

Install all of them on every machine, not only on the machines that run each plugin: the browser and the folders are
used from *other* machines than the one that shares them. A skill for something your fleet doesn't have costs nothing;
the agent checks `fleetlayer me` first.

## Install the skills

The node installs them itself. Each release carries them, and when the node starts it copies every skill
into Claude Code's folder (`~/.claude/skills/<name>/`, if you use Claude Code) and Codex's (`~/.agents/skills/<name>/`,
if you use Codex), on every machine. A self-update restarts the node, so the new release's skills arrive with it.

- **Your own skills are safe.** The node marks each folder it writes with a `.fleetlayer` file and only ever replaces
  or removes folders with that file. If you already have a skill of the same name (a `pages` skill you installed by
  hand, say), it's left alone, and `fleetlayer status` says so:

  ```text
  skill skipped: /home/you/.claude/skills/pages is someone's own, so Fleetlayer's isn't installed there. To use
  Fleetlayer's, move it away and restart the node
  ```

- **It never writes through a symbolic link.** If `~/.claude`, `~/.claude/skills` or `~/.agents/skills` is a link
  (into a dotfiles repo, say), the node writes nothing there and `fleetlayer status` says why: copy the skills by hand
  (below) into the real folder.
- **To install none**, put `skills: false` in the node's `node.yaml`.
- **A node not installed from a release** (built from source, a package) has no skills to copy, and `fleetlayer
  status` says that too. Copy them by hand from a checkout of the release you run:

  ```sh
  git clone --depth 1 --branch <release> https://github.com/fleetlayer/fleetlayer /tmp/fleetlayer-src
  /tmp/fleetlayer-src/scripts/collect-skills /tmp/fleetlayer-skills
  cp -R /tmp/fleetlayer-skills/* ~/.claude/skills/    # and ~/.agents/skills/ for Codex
  ```

If you sync `~/.claude/skills` between machines with the [file-sync plugin](sync.md), let each node install its own copy
and leave Fleetlayer's skill folders out of the synced set. The file-sync plugin never overwrites a file it didn't
write, so the two don't fight, but the copies would only be duplicates.

Fleetlayer checks every command a skill names against its own command line in its tests, so a skill never tells an
agent to run something that doesn't exist.

## What the agent sees

`fleetlayer me`, run in a session:

```text
You are planning@server (claude), on node server.
Messages: you may message all 12 running sessions on other nodes (`fleetlayer fleet` lists them).
Your messages arrive as from you.
Shared tools:
  browser@laptop: allowed (approved until 2026-12-27)
    Your Chrome, signed in to your sites
    Add it: claude mcp add browser -- fleetlayer mcp browser@laptop
  shared-folders@laptop: your first use asks your person
    Folders: screenshots (read), downloads (read, upload)
    Add it: claude mcp add shared-folders -- fleetlayer mcp shared-folders@laptop
Pages space personal (on home): allowed
    Push a page: fleetlayer pages push <folder> --space personal
Start or resume sessions on: home (asks your person first), laptop (asks your person first), server (this node) (`fleetlayer session start --on <node>`).
```

It asks nothing and records nothing: no approval request is filed and nothing counts as a first use. What your access
policy refuses the session isn't listed at all, so an agent you trust less (one that reads strangers' email) doesn't
learn what else is in the fleet. The descriptions come from each tool's `description:` line in the sharing machine's
`node.yaml`; give every tool one, written for an agent choosing between them.
