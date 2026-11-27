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
| `fleetlayer-folders` | `plugins/folders/skill/` | Your screenshots and downloads: the latest screenshot in one call, uploads and deletes where allowed, the refusals only you can fix |
| `fleetlayer-claude-accounts` | `plugins/claude-auth-manager/skill/` | Setup-token sessions: never `/login`, what's missing and what replaces it, reading usage |
| `pages` | `plugins/pages/skill/` | Pages for you to look at: reports, screenshots, mock-ups |
| `apps` | `plugins/pages/apps-skill/` | Small interactive apps behind your sign-in |

Install all of them on every machine, not only on the machines that run each plugin: the browser and the folders are
used from *other* machines than the one that shares them. A skill for something your fleet doesn't have costs nothing;
the agent checks `fleetlayer me` first.

## Install the skills

Claude Code reads skills from `~/.claude/skills/<name>/SKILL.md`, Codex from `~/.agents/skills/<name>/SKILL.md`. From
a checkout of the release you run:

```sh
git clone --depth 1 --branch v0.3.9 https://github.com/fleetlayer/fleetlayer /tmp/fleetlayer-src
cd /tmp/fleetlayer-src
for d in skills/* plugins/folders/skill plugins/claude-auth-manager/skill plugins/pages/skill plugins/pages/apps-skill; do
  name=$(sed -n 's/^name: //p' "$d/SKILL.md" | head -1)
  for root in ~/.claude/skills ~/.agents/skills; do
    mkdir -p "$root/$name" && cp "$d/SKILL.md" "$root/$name/"
  done
done
```

Copy the files, don't link them into a checkout: the [files plugin](sync.md) never follows a link that points outside
the folder it syncs, so a linked skill wouldn't reach your other machines. If you sync `~/.claude/skills` from one
machine with the files plugin, installing them there once is enough.

Update them with each Fleetlayer release, since they describe that release's commands. Fleetlayer checks every
command a skill names against its own command line in its tests, so a skill never tells an agent to run something
that doesn't exist.

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
  folders@laptop: your first use asks your person
    Folders: screenshots (read), downloads (read, upload)
    Add it: claude mcp add folders -- fleetlayer mcp folders@laptop
Pages space personal (on home): allowed
    Push a page: fleetlayer pages push <folder> --space personal
```

It asks nothing and records nothing: no approval request is filed and nothing counts as a first use. What your access
policy refuses the session isn't listed at all, so an agent you trust less (one that reads strangers' email) doesn't
learn what else is in the fleet. The descriptions come from each tool's `description:` line in the sharing machine's
`node.yaml`; give every tool one, written for an agent choosing between them.
