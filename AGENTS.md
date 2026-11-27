# Fleetlayer

Fleetlayer is open source under Apache 2.0, built in public: everything in this repo is public, so no hostnames,
tokens, private repo names, customer data or anything from Nathan's private notes goes into it.

## Rules that shape the design

- **Never replace or hide a session manager.** An adapter attaches to herdr (or T3 Code, Cursor...) without taking
  it over: no exclusive locks, no moving sessions somewhere only Fleetlayer reaches. The person's own apps and
  terminals keep working alongside it.
- **Plugins are separate processes.** Adapters and other plugins are executables the node starts and talks to over
  JSON-RPC on stdio (`docs/protocol.md`). Never compile a plugin into the node, never load dynamic libraries.
- **The control plane holds no credentials.** Claude sign-ins and tokens, and any other secret, live only on machines
  the person controls. Secrets cross the control plane only as ciphertext sealed to a node's key, and are never
  stored there. It keeps only hashes of the tokens that reach it, plus one exception: a GitHub webhook's signing
  secret, which it needs to check a delivery's signature and which can do no more than forge a delivery. Don't add
  another exception without asking Nathan.
- **Build on ACP, don't fork it.** Extensions use ACP's own rules: `_meta` keys and `_`-prefixed methods under the
  `fleetlayer.dev/` namespace. Never add fields at the root of an ACP type.
- **Simple first, depth when it's needed.** Nobody should have to learn what SSO, OpenID, tenants or teams are to get
  started. One person starts with one tenant, one team and the built-in sign-in, created for them and never named in
  the interface. The moment they need a second team, a second tenant or their company's sign-in provider, it's already
  there and just works. In pages, the hub and the docs: plain words, the enterprise features summed up in one line
  ("everything a company needs: single sign-on, teams, separate organisations, audit"), and their settings under an
  **Advanced** section, collapsed by default. Never show an empty "Tenants" or "Identity providers" screen to someone
  who has one of each.
- **Every component can move.** Someone can start with everything on their Mac (control plane, node, sign-in, the
  Claude hub, pages, apps) and later move any one piece to a home server or a VPS by asking an agent. So each
  component keeps all its state in one directory and its settings in one file, never assumes a hostname or that it
  shares a machine with another component, and has a documented, tested move (stop, copy the state, start elsewhere,
  repoint the address) that keeps identities and keys (a node keeps its key; tokens sealed to it stay readable).
  Secrets move only sealed or through the secrets manager.
- **The node is the trust boundary, so trust needs its own Unix user.** Every process of a node's Unix user can read
  its token and claim to be any of its sessions or plugins: the control plane can't tell them apart. Anything the
  policy trusts more than the agents beside it (a runner of approved actions, a service with wider grants) runs under a
  different Unix user, with its own node, whose token and key the agents can't read. Never give a plugin on an
  untrusted agent's node rights its agent mustn't have.
- **Everything in the background is monitored.** Every daemon, sync or scheduled job gets a health check or heartbeat
  that catches real failure (it stopped doing its job, not only that it stopped running), in the same change.

## Code from other projects

- Apache 2.0 and MIT code can be borrowed: keep its copyright notice, add it to `NOTICE`, and mark files you change.
- GPL and AGPL code never goes in, not even a snippet. `herdr-mobile-relay` and Agent Hub are AGPL: don't read their
  code while working on Fleetlayer.
- MPL-2.0 libraries may be dependencies, used unmodified and listed in `NOTICE` (Nathan, 27 Nov 2026, for
  webauthn-rs). Never copy MPL code into Fleetlayer's own files.
- Check a project's licence before taking anything from it, and ask Nathan about any other copyleft licence.

## Builds and tests run on a build server

Rust builds, tests and clippy run on a build server, never on a laptop or the small agents VM. Use
`scripts/on-dev check|clippy|test|build|fmt` (cargo arguments after the command), and `scripts/on-dev lock` after
changing dependencies. The server's SSH host goes in `.dev-host` (gitignored) or `$FLEETLAYER_DEV_HOST`.

## Guides

Fleetlayer is "a small set of components you can run, and a large set of guides about how to plug them all together
with what you already use". Every setup step or footgun others will hit goes into `docs/guides-to-write.md` in the
same change: what it is, how it was solved, and where the knowledge lives now. That includes what Nathan runs into
outside Fleetlayer while running his fleet (Claude Code permissions and hooks, accounts, session managers): the
guides cover the whole journey. When another agent fixes such a problem, add the entry here even if the fix lives
elsewhere, without private names or hostnames.

The docs in `docs/` are the source of fleetlayer.ai (`site/`). A new doc starts with a `# Title` line (no
frontmatter) and gets an entry in `site/site.config.mjs`; links between docs stay relative `.md` paths.

## Skills: what agents are told

Agents on a Fleetlayer machine learn what it gives them from skills: the core one in `skills/fleetlayer/`, one per
shared tool that isn't a plugin (`skills/fleetlayer-browser/`), and one beside each plugin an agent acts through
(`plugins/<name>/skill/SKILL.md`, `plugins/pages/apps-skill/`). A change to a command, a plugin's tools or a message an
agent sees (a refusal, an outside-data line) that changes what an agent should do updates its skill in the same
commit. Check every command and flag against the code before writing it down. A test in `crates/fleetlayer-cli`
(`skill_commands.rs`) fails when a skill names a `fleetlayer` command or flag that doesn't exist. A skill's folder
name doesn't matter: its `name:` does, unique across the repo (`scripts/collect-skills`, run in CI, puts each in the
release's `skills/<name>/`, and the node installs them on every machine).

## Releases

Deployers get one release per set of features: a version tag (`v0.2.0`) with release notes, not every build. Each
release can mean a backup and a rollout for them. A tag outside that rhythm only for an urgent security fix or a bug
that breaks someone's work, and say which it is.

## Git

Fleetlayer is built semi-autonomously: agent sessions commit, push to main, release and deploy as they go, without
asking Nathan first (unlike projects where he reviews each change). Before pushing, tell the other sessions working in
this repo what you're pushing and which files, and wait for them if they're mid-change in the same files. Each
session works in its own git worktree (`git worktree add ../fleetlayer-<tab> main`), never in another session's
checkout. To reach another session, use its herdr tab name, which survives restarts: `fleetlayer msg send --to "<tab>@<node>"`.
Claude Code's own session names (`fleetlayer-3d`) are random and change whenever the machine restarts.

After pushing, run `fleetlayer subscribe ci --commit` in your checkout (with a CLI older than v0.3.9:
`fleetlayer subscribe ci --subject fleetlayer/fleetlayer@$(git rev-parse HEAD)`). Each workflow's result for your
commit, success or failure, arrives as a message within seconds of the run ending, and the subscription ends by
itself. Don't poll. If no result arrives within the usual CI time plus five minutes, check once with `gh run watch <id>
--interval 60` (never shorter), and never loop over `gh run list`: every session shares one GitHub API limit (5,000
calls an hour), and short intervals across many sessions used it up on 27 Nov 2026. A "cancelled" result means
another session pushed on top of yours: subscribe to the newer commit (`git fetch`, then `fleetlayer subscribe ci
--commit <newer sha>`) to hear the run that includes your change. Releases still follow the rhythm below. Quote commit hashes only from git output, after committing.
