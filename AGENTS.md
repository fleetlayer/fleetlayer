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
  stored there.
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

## Releases

Deployers get one release per set of features: a version tag (`v0.2.0`) with release notes, not every build. Each
release can mean a backup and a rollout for them. A tag outside that rhythm only for an urgent security fix or a bug
that breaks someone's work, and say which it is.

## Git

Fleetlayer is built semi-autonomously: agent sessions commit, push to main, release and deploy as they go, without
asking Nathan first (unlike projects where he reviews each change). Before pushing, tell the other sessions working in
this repo what you're pushing and which files, and wait for them if they're mid-change in the same files. Each
session works in its own git worktree (`git worktree add ../fleetlayer-<tab> main`), never in another session's
checkout. Releases still follow the rhythm below. Quote commit hashes only from git output, after committing.
