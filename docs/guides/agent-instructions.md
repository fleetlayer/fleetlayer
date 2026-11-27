# Instructions that follow your agents to every machine

Every agent starts each session from its instructions: a global file for everything it does, and the project's own
`AGENTS.md` or `CLAUDE.md`. With one machine, one global file is enough. With a laptop, a server and a VM for agents,
the same file is mostly right everywhere and slightly wrong on each: the server has no browser, the VM can't use Touch
ID, the laptop reaches the others differently. Agents either get told things that aren't true where they run, or you
keep several copies that drift apart.

This guide covers three habits the author's fleet runs on, each small, each written into the global instructions so
every agent follows it without being reminded:

- **One shared global file, plus one file per machine** for what holds only there.
- **Two scratchpads per agent:** one that's wiped, one that's kept.
- **The agent creates the secrets item before you paste a secret in**, so your part takes seconds.

## 1. A shared global file and a per-machine file

Keep the global instructions in one place and sync them to every machine (see [Keep notes, skills and instructions
the same on every machine](sync.md)). Next to them, each machine has its own file that the sync leaves out. For Claude
Code, that's `~/.claude/CLAUDE.md` everywhere, and `~/.claude/CLAUDE.this-machine.md` on each machine. The global file
pulls the machine's file in with Claude Code's import syntax, at the end:

```markdown
# This machine

Instructions that hold only on the machine you're running on:

@~/.claude/CLAUDE.this-machine.md
```

Leave the per-machine file out of the sync. With Syncthing, that's a line in the folder's `.stignore`:

```text
CLAUDE.this-machine.md
```

What goes in the per-machine file is whatever an agent would otherwise get wrong there:

- **What the machine can't do:** no browser, no Touch ID or keychain, no GUI; a small machine where builds go
  elsewhere ([Notes for agents that build software](agents-building-software.md)).
- **How to reach the rest of the fleet from here:** the node's name, which sessions are on this machine and which are
  reached with `fleetlayer msg send --to "<tab>@<node>"`.
- **Where things live here:** the notes vault, the shared folders, the secrets the machine may read.
- **Who to ask for what only a person can do:** on a server with no screen, a sign-in or a key touch becomes a task
  for the person, or a message to a session on their laptop.

Write the global file so it never names one machine. When a rule holds only somewhere, it goes in that machine's file.

The shared file is also where every agent learns it's on Fleetlayer and what it may use from here:
[Tell your agents about Fleetlayer](agents-know-fleetlayer.md) has the block to paste and the skills to install.

## 2. Two scratchpads per agent

Agents produce two kinds of files that aren't part of a project: intermediate files (a downloaded release, a test
database, screenshots before they're picked) and working notes worth keeping (an investigation log, research, a
checklist for a long job). Mixing them loses one or the other.

- **The wiped one:** `/tmp`, or the session's own scratch folder if the harness gives one. Anything there may be gone
  after a reboot, and that's fine.
- **The kept one:** a folder in your synced notes vault, named after the agent's role (`Agents/triage/`,
  `Agents/docs/`), or after its session's name when it has no role. It syncs to your other machines and your phone,
  survives reboots, and is there for the next session that takes the role over. Agents create it when they first need
  it.

Put both in the global instructions, with the rule for choosing: intermediate files go in the wiped one; anything the
agent, its successor or you would want next week goes in the kept one. Decisions and anything the team needs still
belong where the team looks (the repository, the issue tracker, the pull request), not in an agent's scratchpad.

## 3. Secrets you paste in: the agent makes the item first

Some secrets only a person can get: a token generated on a vendor's page, an API key from a dashboard. The slow part is
rarely the copy. It's opening the secrets manager, choosing the vault, creating the item, picking its type, naming it
and finding the right field, and then telling the agent where it went.

Have the agent do all of that first, with an empty field, and then tell you the item's name. With 1Password's command
line:

```sh
op item create --vault agents --category "API Credential" \
  --title cloudflare-site-deploy-token 'credential[concealed]=' \
  'notesPlain=Deploy token for the site (GitHub Actions secret CLOUDFLARE_API_TOKEN). Scope: Workers deploys only.'
```

A new item sits at the top of the vault's list, so you open it, press Edit and paste. The vault, the name and the field
are exactly what the agent's code reads, and the notes say what it's for and how narrow it should be.

The rules to give agents:

- **Create the item, then ask.** Never ask the person to create it, and never ask for the secret in chat.
- **Kebab-case names** that say what and where (`cloudflare-site-deploy-token`, not `token2`).
- **The right vault:** the one the agent's service account can read, so it can use the secret afterwards without
  asking again.
- **Ask once, with everything:** where to get the secret, what scope to give it, and the item's name.

When the agent's own credentials can make the secret (a token with permission to create narrower tokens), it should
make it itself, scoped as tightly as the job allows, and save it straight into the item without printing it.

## When it doesn't work

- **An agent on the server tries to open a browser or asks for Touch ID:** the per-machine file is missing that
  machine's limits, or the global file states something only true on the laptop. Move the rule into the right file.
- **A change to one machine's file appears on all of them:** the sync isn't leaving it out. Check the ignore list, and
  that the file's name matches it exactly.
- **Notes an agent needed are gone after a reboot:** it wrote them to the wiped scratchpad. Say in the global
  instructions which kind of file goes where.
