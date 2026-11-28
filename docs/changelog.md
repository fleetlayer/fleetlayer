# Changelog

What changed in each release of Fleetlayer, newest first. The full release notes are on
[GitHub](https://github.com/fleetlayer/fleetlayer/releases). The rest of these docs describe how things work in the
latest release.


## Upgrading from an older release

Most releases need only the installer or a node's own update. These need more, once:

- **From v0.3.12 or earlier to anything newer:** each release used to live in a folder of its own. The first newer node
  to start moves itself to one fixed path (`~/.local/lib/fleetlayer/`) and restarts once. On a Mac, that move is a new
  path, so macOS asks once more for the permissions you gave the node (Files and Folders, Local Network); after that,
  upgrades keep them. Before this, macOS asked again after every upgrade.
- **From v0.3.8 or earlier:** sync approvals held for 30 days whether used or not, and a refused version waited for
  the folder to change. Upgrade the source, the targets and the control plane together. Targets older than v0.3.9 write
  synced files without their executable bit, so scripts in a synced skill won't run there until the target is upgraded.
- **A node older than v0.3.8** doesn't relink `fleetlayer-control`, `fleetlayer-gateway` and `fleetlayer-pages` when it
  updates itself. Rerun the installer once on a machine that runs those.
- **A node installed before 26 November 2026** wrote its versioned path into its service file, so it kept running the
  old binary after updating. Rerun the installer, then `fleetlayer-node install`.

## v0.4.2 (29 November 2026)

- **Upgrading:** back up the control plane's database (migrations 0031 to 0033 run on start). The first start
  deletes events older than 30 days (session status older than 7), in batches; set `event_days` in `control.yaml` to
  keep more.
- **Upgrading:** only the control plane publishes on the `session.status` and `access` channels now; nodes, plugins
  and webhooks are refused there. A read token reads every event body through the new events feed, so give one only to
  a service you'd trust with all of them.
- **Upgrading:** point liveness probes at the new `GET /livez`, which never touches the database; `/healthz` reports a
  busy database as degraded and fails only when it stays busy for over a minute. Before moving the Claude hub, upgrade
  its machine and let it publish once.
- **Upgrading:** a start's approval covers exactly that start (another folder, model or prompt asks again), and names
  with control or hidden characters are refused.
- **More agents** (`fleetlayer-harness`): pi, OpenCode, Gemini CLI and Cursor's CLI are listed, messaged, read and
  started like Claude Code, with transcripts and resume ([Harnesses](harnesses.md)). **tmux** (`fleetlayer-tmux`):
  agents in tmux panes are sessions like herdr's and Rex's, and `fleetlayer up` finds tmux by itself.
- **Presence** (`fleetlayer-presence`): whether you're at your desk, on your phone, away or asleep, so agents can skip
  background work nobody would read, with who sees it decided by the policy.
- **Events you can watch and read:** a `session.status` event each time a session starts working, goes idle,
  finishes, is blocked or goes away; `fleetlayer watch` tells your session once when something happens; and services
  read `GET /v1/events`. Plugins can subscribe to events (`event.subscribe`) and start sessions (`session.start`),
  each start decided by the policy, with `starts_per_hour` and `starts_per_day` limits.
- **A moved Claude hub keeps its identity:** nodes trust the hub's signing key, which moves with it, so `fleetlayer
  move hub` needs no manual step; `fleetlayer secrets unpin <bundle>` resets a wrong pin.
- Approval requests in plain words, shown first by the dashboard and `approvals list`. Leases count slots per value of
  a payload field (`per:`). GitHub events keep a commit status's context and state and a pull request's number.
- Fixes: sessions start on stock herdr; a link that died silently reconnects after 100 s; `fleetlayer up` on a node
  joined through a tunnel starts no control plane; reading and typing in Rex sessions through herdr-fleet; `~` in
  `claude_dir`; Matrix `history` honours its limit; `fleetlayer move` carries Matrix, 1Password, egress and dashboard
  state; no macOS prompt at every browser connection or cookie read; tmux before 3.3; fewer database writes on a slow
  disk.

## v0.4.1 (28 November 2026)

- **Upgrading:** migration 0030 runs on start, so back up the control plane's database first.
- **Upgrading:** `fleetlayer tools call` takes its JSON arguments as `--params '{…}'`. In v0.4.0 the `--json` flag
  clashed with the global flag and every call failed.
- **Upgrading:** for the new Services tab, give the sign-in plugin `grants: [reading.publish]` so its clients appear,
  and set `public_scheme: https` in `gateway.yaml` where a TLS proxy fronts the gateway.
- **Services in the dashboard:** a Services tab lists every web service the fleet runs, with links and health, plus
  outside links you add. Each viewer sees only what they may use.
- `fleetlayer session command <session> /compact` (or `/clear`) types an allowed slash command into an idle session.
  Each command is its own approval, asked once by default.
- A deck or the dashboard can type a signed-in person's one-line answer into a session as their own input (`say`). It
  needs that person's own token, is audited and rate-limited, and no agent or service can call it alone.
- Smaller fixes: a command whose output is closed early (`fleetlayer … | head`) exits quietly, and a build host can be
  limited to light commands such as `fmt`, `check` and `clippy`.

## v0.4.0 (28 November 2026)

- **Upgrading:** back up the control plane's database (migrations 0025 to 0029 run on start), and upgrade the control
  plane and every node together.
- **Upgrading:** sign-in has no admin pages any more. Deploy the dashboard in the same rollout, with a `delegate` token
  (`fleetlayer-control token add dashboard --scope delegate`) under a Unix user no agent can read, and set
  `dashboard:` in the sign-in settings.
- **Upgrading:** the control plane answers only `/v1/*`, `/healthz` and `/hooks/*`, so a proxy can send everything
  else on the same host to the dashboard.
- **Upgrading:** the dashboard reads `/v1/readings` and `/v1/tools`, which are new in v0.4.0. Against an older control
  plane, the parts that use them stay empty.
- **Upgrading:** a node older than v0.4.0 doesn't publish the `node.status` reading, so the dashboard shows its
  plugins as unknown until it's upgraded.
- **The dashboard** (`fleetlayer-dashboard`): one page for machines, sessions, events, access, readings, people, teams
  and rules. It acts as the signed-in person, and changing people asks for a passkey used in the last 10 minutes.
- **Leases** (`fleetlayer-leases`) and **shared commands**: `fleetlayer lease acquire|release|list` for build slots
  and approved claims, and `fleetlayer run <name>@<node>` to call a command another machine declares, decided by the
  access policy.
- **Channels and Matrix** (`fleetlayer-matrix`), **cookies through the browser** on macOS, **session handoffs**
  (`fleetlayer session handoff`) and **transcript search across nodes** (`fleetlayer transcript-search`).

## v0.3.16 (28 November 2026)

- Reading a huge transcript works: a line longer than a read comes back cut with a marker instead of an empty read.
  `fleetlayer transcript --types assistant,user` keeps only the conversation, and `--tail` returns the newest lines.
- A shared tool reached through `fleetlayer mcp` survives its machine restarting or sleeping, so a session no longer
  has to restart to get the shared browser back.
- **Upgrading:** the node serving the transcript and the calling machines need this version. Sessions pick up the
  bridge fix at their next start.

## v0.3.15 (28 November 2026)

- `fleetlayer transcript <tab>@<node>` works for a session on another node. The control plane refused it as an unknown
  method before.
- The Claude accounts plugin takes over a stale coordinator lock left by its previous process after a restart.
- Only the control plane and the nodes running the Claude accounts plugin need this release.

## v0.3.14 (28 November 2026)

- `fleetlayer transcript <session>` reads another session's transcript on any node the access policy lets you read,
  with delivered secrets redacted. Both nodes need v0.3.13 or later.
- Shared tools really start lazily: an idle session no longer starts its own Playwright process on the machine sharing
  the browser.

## v0.3.13 (28 November 2026)

- **Upgrading:** back up the control plane's database (migrations 0019 to 0024 run on start), and upgrade the control
  plane and every node together.
- **Upgrading:** binaries now live at one path, `~/.local/lib/fleetlayer/<binary>`, with the last two versions kept for
  going back. An old install moves itself the first time the new node starts.
- **Upgrading:** the control plane has a settings file, `control.yaml`. Run `fleetlayer up` once to create it, or pass
  `--config`. Shared tools now start at a session's first call; set `lazy: false` to keep the old behaviour.
- **Secrets per role** (`fleetlayer-1password`) and **network access per role** (`fleetlayer-egress`, Linux), both off
  until you turn them on. A node also scrubs every secret delivered to it from what it sends the rest of the fleet.
- **The MCP gate** checks every call to a fronted MCP server against the access policy, and **admin windows** give a
  role no rights until you start a timed window with `fleetlayer role activate`. The control plane and the
  gate's machine both need v0.3.13 or later.
- `fleetlayer move <piece> <machine>` moves any component to another machine and keeps its identities and keys.
- Every herdr on your phone (`herdr-fleet`), and sign-in is no longer experimental.

## v0.3.12 (27 November 2026)

- **Upgrading:** migration 0018 runs on start, so back up the control plane's database first.
- **Upgrading:** the control plane can read its policy files itself (`serve --policy-file`), reloading them when they
  change. If you load the policy with an init container, see "The access policy" in `deploy/README.md` to switch.
- **fleetlayer-scheduler:** a new plugin that sends a message to a session on a schedule. It needs
  `grants: [message.send]` in `node.yaml`.
- **fleetlayer-cookies:** a new plugin that shares a site's cookies from a browser on one machine with command-line
  tools on another, sealed to each target.
- Sign-in in front of any web app through forward-auth, and teams from your sign-in provider's groups (both
  experimental).
- `fleetlayer-control endpoint remove` and `endpoint rotate` for webhook endpoints, for example after a URL leaks.

## v0.3.11 (27 November 2026)

- **Upgrading:** the claude-code plugin now adds hooks to each user's `~/.claude/settings.json` and restores them every
  5 minutes. If a configuration tool manages that file, add the same entries there or set the plugin's `hooks: false`.
- **Upgrading:** two new policy verbs, `answer` and `close`, are asked by default. Grant them only to the one service
  that should answer prompts or close sessions, never on an untrusted agent's node.
- **Upgrading:** add `backchannel_logout_uri` to the hub's and pages' clients in the sign-in settings.
- **Blocked prompts as data:** a Claude Code session waiting on a question, permission prompt or plan approval shows
  it in the fleet API, and `fleetlayer session answer` types the answer in. `fleetlayer session close` is new too.
- Signing out reaches every service through OpenID Connect back-channel logout, with a sign-out-everywhere button.
- Webhook events never carry the endpoint's secret path. If an earlier version delivered a ping to an agent, rotate
  the endpoint's path.

## v0.3.10 (27 November 2026)

- The control plane no longer refuses requests while a read-only command such as `audit verify` reads its database.
  Released out of rhythm because this refused people's messages.
- Summaries stopped by a usage limit retry by themselves on another account.
- **Upgrading:** point the scheduled audit check at the new `GET /v1/audit/verify` with its own admin token, and point
  uptime checks at `/healthz?strict`.
- Plugins can report a problem, which shows in `fleetlayer status` and turns the node's health red.
- **Upgrading:** replacing a node's binaries closes open shared-tool connections, so sessions using the shared browser
  need an `/mcp` reconnect.

## v0.3.9 (27 November 2026)

- **Upgrading:** two plugins are renamed with no compatibility: `files` is now `file-sync` and `folders` is now
  `shared-folders`. Update `node.yaml`, policy rules and MCP configs with each node's upgrade.
- **Upgrading:** upgrade the control plane and sync targets before sync sources. Agent permission flags for sessions
  Fleetlayer starts come only from the target node's `harnesses:` settings.
- **Start and resume sessions anywhere** with `fleetlayer session start --on <node>` and `fleetlayer session resume`,
  and see what a session may use with `fleetlayer me`.
- **CI results without polling:** `fleetlayer subscribe ci --commit` after pushing.
- **Shared folders** (`fleetlayer-shared-folders`), **Rex support** (`fleetlayer-rex`) and **skills for agents**, which
  the node installs for Claude Code and Codex.
- The shared browser's `browser_close` closes your tabs.
- **Sign in with Fleetlayer:** the experimental `fleetlayer-signin`, an OpenID Connect provider with passkeys.
- macOS builds are signed and notarised. Smaller fixes: shared-tool calls time out instead of hanging, and the
  control plane exits within seconds on `SIGTERM`.

## v0.3.8 (27 November 2026)

- **Upgrading:** back up the control plane's database and upgrade it first. Migration 0014 runs and admits every
  enrolled node to a tenant, so nothing that worked stops.
- **Start on your laptop:** `fleetlayer up` installs and starts the control plane, a node and the gateway as user
  services, and `fleetlayer down` stops them.
- **fleetlayer-gateway:** every service at `http://<name>.localhost:7680` on this machine, with a status page.
- **Access control by tenant:** policies can name people, teams, roles, services, pages and apps. A fleet with no
  policy now asks before sensitive actions, as documented.
- **Apps in pages:** HTML and JavaScript on subdomains of their own, signed in through pages.
- Smaller fixes: a self-update repoints every binary, and heartbeat `token_file` paths with `~` work. Prerelease tags
  (with a `-`) are skipped by `install.sh` and auto-update.

## v0.3.7 (27 November 2026)

- **Pages** (`fleetlayer-pages`) ships in the release archives, the node image and `install.sh`. Pages are plain folders
  in a synced vault, live within a second of a file changing.
- Pages' `/healthz` and heartbeat fail when a space's folder can't be read, so a dropped share doesn't look empty.
- A node's last seen time in the control plane is refreshed after each answered ping.
- A first guide on backups.

## v0.3.6 (27 November 2026)

- **Upgrading:** back up the control plane's database and upgrade it first. Access decisions don't change.
- **People, tenants and teams** in the control plane, the first step of Fleetlayer sign-in. A `default` tenant with an
  `everyone` team is added and every existing node is admitted to it.
- Invites and recovery invites, and nodes owned by a person, a team or a tenant.
- New `fleetlayer-control` commands: `person`, `invite`, `team`, `tenant`, `agent` and `node add --owner`. They log to
  stderr, so printed tokens and codes stay separate from log lines.
- A first version of the pages plugin, in the source only.

## v0.3.5 (27 November 2026)

- **Upgrading:** upgrade the node that runs the Claude sign-in hub. Other nodes take only smaller fixes.
- The hub's page is redesigned: accounts as cards with their state, and sign-in and setup-token flows in step-by-step
  dialogs that can be cancelled and resumed.
- The hub checks that a setup token belongs to the account it's for, refuses one for a different account, and drops
  one later found to be wrong.
- The hub's page sends a strict Content-Security-Policy and refuses to be framed.

## v0.3.4 (27 November 2026)

- **Upgrading:** upgrade the control plane first, then nodes.
- `fleetlayer readings <kinds>` prints the newest readings of those kinds this node may read.
- `fleetlayer-claude-accounts`, a new binary, lists every account the hub manages with its usage windows, in a format
  CodexBar can read. It needs a control plane and hub of v0.3.4 or later.
- A self-installed node links binaries new in a release into `~/.local/bin` when it starts.

## v0.3.3 (26 November 2026)

- Writing a Claude token into a slot keeps the MCP servers' sign-ins stored beside it. MCP servers whose sign-ins were
  wiped by v0.3.1 or v0.3.2 need signing in again once.
- A node applies its last Claude account bundle again at every start, so a changed slot layout takes effect.
- Upgrade the nodes that run the Claude accounts plugin. The control plane is unchanged.

## v0.3.2 (26 November 2026)

- An urgent fix: setup tokens the hub read no longer carry the next line of the screen, which made them fail on every
  node.
- The hub checks each new token with one small request before keeping it.
- **Upgrading:** if v0.3.1 stored tokens, strip the trailing `Storethistokensecurely` from `hub.json` and
  `tokens.json`, or make each setup token again.

## v0.3.1 (26 November 2026)

- The hub's setup-token flow submits. Released out of rhythm because the broken flow blocked work.
- **Upgrading:** the hub now manages every Claude sign-in, and a node replaces a sign-in made by hand. Set
  `default_slot: false` on a node to leave `~/.claude` alone.
- Accounts number from 1 in the hub, matching every machine, and a Delete button signs an account out.
- A wrong session name gets its own error with suggestions, and a bare tab name resolves across the fleet when it's
  unique. Upgrade the control plane and nodes together for this.

## v0.3.0 (26 November 2026)

- **Upgrading:** back up the control plane's database. Migration 0012 adds the readings table.
- **Readings:** plugins publish small, newest-wins facts and subscribe to other nodes' readings, decided by the policy
  verbs `publish` and `subscribe`.
- The Claude accounts plugin writes setup tokens into account slots on each node and shares usage across the fleet.
- **The sign-in hub** runs on an always-on machine: you do every Claude sign-in from your own browser in its page, and
  it publishes the tokens to your nodes sealed. Setup is in `docs/plugins/claude-auth-manager.md`.

## v0.2.1 (26 November 2026)

- **Upgrading:** a security fix for nodes that share tools. Shared tools now run in an empty directory of their own,
  never the node's.
- `on_close` calls run one at a time, with `repeat:` until the first error, so a shared browser's tabs all close.
- The browser example in the getting-started guide shows the safe setup.

## v0.2.0 (26 November 2026)

- **Access control, version 2:** policy files compile to Cedar policies, so a deny always wins. Reading is a
  permission, and approvals are on by default (`ask: []` turns them off).
- A tamper-evident audit log of every decision, with `audit verify`, and `acl apply`, `explain`, `test` and `suggest`.
- Messages from agents that read untrusted input arrive labelled as outside data, and messages are rate-limited.
- **Shared tools:** per-tool allow and deny rules, tools gated by network, and `on_close:` calls.
- **Sealed sync** between nodes with a key per node, and the first part of the Claude accounts plugin.
- Summaries for other programs, a summaries feed and read-scoped API tokens. Self-update from releases and
  `install.sh`.

## v0.1.0 (26 November 2026)

- The first release: the node and the `fleetlayer` command line, the control plane with enrolment, node links, the
  fleet view and messages between machines.
- The herdr and Claude Code plugins, shared tools on another machine, and session summaries.
- Linux and macOS binaries from GitHub releases, with checksums and attestations.
