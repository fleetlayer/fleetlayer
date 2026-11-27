# Guides to write

Every setup step, footgun or design reason others will hit goes here in the same change: what it is, how it was
solved, and where the knowledge lives now. These become Fleetlayer's guides. They cover the whole journey of running a
fleet of agents, not only Fleetlayer itself: Claude Code's permissions and hooks, accounts and sign-ins, session
managers, machines that sleep.

- **A GitHub organization can't be created from the CLI or the API on github.com.** `POST /admin/organizations`
  exists only on GitHub Enterprise Server (it answers 404 on github.com, whatever the token's scopes). An agent can
  create repos with `gh repo create <org>/<name>`, but a person makes the organization once in the web UI
  (github.com/account/organizations/new). Where it matters: any guide that has an agent set up a new project's org.
- **On macOS the node must run as a per-user LaunchAgent.** Writing Claude Code's keychain item needs the login
  keychain unlocked: a LaunchDaemon, or an SSH session on a Mac whose keychain is locked, fails with "User
  interaction is not allowed". Where it lives: `docs/plugins/claude-auth-manager.md`.
- **Driving your own signed-in browser from an agent on another machine.** Written: `docs/guides/browser.md`. Playwright
  MCP's extension mode, with its server on the laptop (the extension only talks to loopback). Interim transport: the laptop dials out over SSH with
  a reverse stream-local forward, a socket-activated launchd agent starts one `@playwright/mcp --extension
  --session-name <name>` per connection, and each connection opens with `<name>\t<token>\n`. The homeserver session is
  building it and will send the full setup (sshd Match block, launchd agents, probe, off switch, test results) for
  this guide. Later: Fleetlayer's "tools on another machine" replaces the transport with the same contract.
- **Claude Code sessions in bypass mode hold every Fleetlayer message for approval** unless
  `~/.claude/settings.json` has `"crossSessionInbound": "accept"`. That setting also accepts unattested messages from
  any process of the same user that can read the session's key file: the guide says so and leaves the choice to the
  user. Where it lives: `plugins/claude-code/src/main.rs`.
- **Access control for a fleet with an agent you trust less.** A new control plane allows every message. Before a node
  that reads untrusted input joins (an email triage agent), add rules: `fleetlayer-control acl add --from
  '*@triage-node' --to 'inbox@main' allow`, then `--from '*@triage-node' --to '*' deny`, or set `acl default deny`
  and list what's allowed. A node sees only the sessions it may message, and subscribes only its own. Where it
  lives: `crates/fleetlayer-control/src/acl.rs`. Written: `docs/guides/untrusted-input.md` (version 2 policy, tested with `acl test`
  against v0.3.7).
- **A browser MCP server started by launchd can't find Chrome's profile.** macOS doesn't let a launchd-spawned process
  read Chrome's profile folders, so Playwright MCP's extension mode fails with "Playwright Extension not found". Fix:
  set `PLAYWRIGHT_MCP_EXECUTABLE_PATH` to Chrome's binary and pass `--profile-dir-name Default`. Six sessions at once
  each got their own tab group with no cross-talk; navigation over an SSH tunnel takes 0.5-1 s. Found by the
  homeserver session, 26 Nov 2026. Fleetlayer's node will hit the same thing when it starts the server.
- **Stopping test processes with `pkill -f` can kill your own shell**: the pattern matches the shell's own command
  line. Use a bracket (`pkill -f 'bin/[f]leetlayer-node'`), and never `pkill -x fleetlayer-node` on a machine where the
  node runs as a service.
- **macOS Local Network privacy can block the node's first links.** On macOS 15+, a launchd-started binary that isn't
  Apple's dialling a LAN address gets "No route to host (os error 65)" until Local Network access is allowed. The
  node retries with backoff, so it usually links once the person allows the prompt. If a fresh install keeps failing
  that way, check System Settings > Privacy & Security > Local Network. Seen installing the first Mac node, 26 Nov 2026.
- **The browser over an SSH bridge (the interim before Fleetlayer's shared tools).** From the homeserver session's
  build, 26 Nov 2026: OpenSSH's `AllowTcpForwarding no` also refuses Unix-socket remote forwards, so use
  `AllowTcpForwarding remote` with `PermitListen none`; a socket created in a setgid directory inherits its group;
  macOS TCC stops a launchd process reading Chrome's profile (set `PLAYWRIGHT_MCP_EXECUTABLE_PATH`); each connection
  opens with a `name<TAB>token` line. Sources: the homeserver repo's `ansible/roles/agents_vm/tasks/browser_tunnel.yml`,
  `scripts/browser-mcp/`, and `docs/services/agents-vm.md` ("Browser tunnel").
- **After upgrading by hand, rerun `fleetlayer-node install` if the service runs a versioned path.** Since 26 Nov the
  installer writes the `PATH` symlink (`~/.local/bin/fleetlayer-node`) when it points at the running binary, so
  swapping symlinks reaches the service; older installs wrote the versioned path and kept running the old binary.
- **Proxies close idle WebSockets.** Traefik 3.4's default entrypoint idleTimeout is 180 s, and a shared tool's stream
  (a browser session between tasks) can sit idle much longer. Fleetlayer's nodes ping their stream sockets every 30 s
  and the link every 30 s. If you put another proxy in front of a control plane, check its idle timeout is longer than
  30 s. Found when a 5-minute idle browser stream was dropped, 26 Nov 2026.
- **Don't let a node summarise another user's sessions.** Archive or RAG syncs can mirror one Unix user's Claude
  transcripts into another's `~/.claude/projects`. The summaries plugin now skips sessions whose recorded cwd is
  outside its own user's home; any tool that reads transcripts needs the same check.
- **launchctl bootstrap right after bootout can fail** with "Bootstrap failed: 5: Input/output error": bootout returns
  before launchd has unloaded the service. `fleetlayer-node install` now waits until `launchctl print` no longer finds
  it, then bootstraps, retrying once. Found by the homeserver session installing the Mac node (1 in 3 runs).
- **Test nodes on a machine where people work must set `typing_fallback: false`.** A test node with only the herdr
  plugin types messages into the real tab when it can't use Claude Code's socket, so a test message lands as the
  person's input in the middle of their work. Happened once while testing access policy v2, 26 Nov 2026.
- **Sharing a browser MCP server safely (Playwright MCP 0.0.86, extension mode).** Found by the homeserver session,
  26 Nov 2026: `browser_run_code_unsafe` is always on (deny it with `tools:`); file arguments are confined to the
  client's MCP roots or the server's working directory, so pass `--ignore-roots` and give it an empty `cwd` (a
  launchd service's cwd is `/`); `browser_close` only disconnects in extension mode and leaves tabs in a grey group, so
  clean up with `browser_tabs {action: close, index: 0}` repeated one at a time (pipelined calls race and leave tabs).
  Where it lives: docs/getting-started.md, docs/access-control.md.
- **A LaunchAgent can't find `claude`.** launchd gives agents a PATH without `~/.local/bin`, where Claude Code's
  installer puts it, so anything a node runs that calls `claude` fails with "No such file or directory". The
  claude-auth-manager plugin looks on PATH and then in the installers' places (`~/.local/bin`, `~/.claude/local`,
  Homebrew), and takes `claude:` in its config. Other plugins that run `claude` need the same. Found writing the
  token checks, 26 Nov 2026.
- **Running `claude -p` from a token slot without touching the person's setup.** Set
  `CLAUDE_SECURESTORAGE_CONFIG_DIR` to the slot (that's where the credentials are read from, on macOS and Linux) and
  `CLAUDE_CONFIG_DIR` to a scratch directory, so Claude Code leaves its config files there and not in the slot; add
  `--setting-sources project` from an empty working directory (no user settings, so no plugins or hooks),
  `--strict-mcp-config` and `--no-session-persistence`; remove `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` and the
  other overriding variables. Tested with Claude Code 2.1.332, 26 Nov 2026; it lives in claude-auth-manager's
  `health.rs`.
- **One coordinator per user for the Claude account rotator.** `coordinator.sock` in `~/.claude/account-rotator` is
  served by whichever process holds `coordinator.lock`: the rotator's own coordinator, Quietdeck's node, or
  claude-auth-manager. Where claude-auth-manager runs, Quietdeck's node must have its Claude part off
  (`usage: { claude: false }` in its node.yaml, since quietdeck 37c7e31; it keeps Codex), or the plugin waits (its
  `claude.coordinator` reading says so); a rotator's own coordinator is asked to step down. Sessions need rotator
  0.7.6 or later to leave the plugin's coordinator alone. 26 Nov 2026.
- **Syncing a checkout to a build server with `rsync -a` can make cargo skip a changed file.** It copies the local
  modification time, which can be older than the server's last build, so the build uses stale code and reports
  errors about things that exist. `scripts/on-dev` now syncs by content (`--checksum`) with fresh times on changed
  files. Found 26 Nov 2026.
- **Signing Claude Code in on a machine with no browser.** `claude auth login --claudeai` prints a URL and reads a
  pasted code from stdin (a pipe is enough); `claude setup-token` needs a pseudo-terminal and prints the token on
  screen. Tested with 2.1.332, 26 Nov 2026; details in docs/plugins/claude-auth-manager.md.
- **Running the Claude sign-in hub.** An always-on machine (never the laptop), its own Unix user as a hardened system
  service, its page behind a private HTTPS proxy with `trusted_proxies` set (else one person's typos lock everyone out),
  a firewall rule so only the proxy reaches its port, an uptime check on `/healthz` plus its heartbeat, its folder left
  out of backups on purpose. How it went is in docs/plugins/claude-auth-manager.md ("The hub").
- **Watching and driving any session's terminal from anywhere.** `fleetlayer attach <tab>@<node>` (Ctrl-] leaves,
  `--observe` only watches). herdr's `terminal session observe|control` is the stream; the access policy's `read` and
  `attach` decide who. A shell inside a herdr pane carries `HERDR_SOCKET_PATH`, which beats `HERDR_SESSION`: unset it
  to reach a named test session. Design: docs/protocol.md ("Terminals").
- **Replacing Claude Code's built-in "dangerous command" prompts with your own hooks, so unattended agents never
  block** (Claude Code 2.1.332, Linux).
  - The trap: Claude Code's own Bash check, "Dangerous rm operation on possibly-empty variable path" (also `rmdir`,
    and variables inside command substitutions or derived ones), asks even under `--dangerously-skip-permissions`, and
    "cannot be auto-allowed by permission rules". An agent nobody watches waits forever (a worker agent sat all night
    on `rm -f $C/$ID.json`, `$C` from a sourced file, in a forked subagent).
  - The switch: `CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1` lifts those prompts (found by reading the
    binary; no public docs). It works only in bypass mode (`--dangerously-skip-permissions` or `defaultMode:
    bypassPermissions`), outside child sessions (`CLAUDE_CODE_CHILD_SESSION`), with no `ask` or `deny` rules touching
    removals, no `PermissionRequest` hook and no managed settings; otherwise it logs that it "has no effect". Set it in
    the real environment where `claude` starts (the session manager's service unit), not settings.json's `env` (the
    binary drops it from children's environment), check with `/proc/<pid>/environ`, and restart each session.
  - The guard that makes that safe: a `PreToolUse` hook on Bash (exit 2, the reason on stderr) refuses any `rm` or
    `rmdir` whose target uses a variable unless every variable is a `${NAME:?}` guard, set to a literal earlier in the
    same command, or `HOME`/`TMPDIR`; command substitutions and unquoted variables always; `${C:-/tmp}` isn't a guard.
    Don't confine recursive deletes to the session's directory and `/tmp`: that refused a checked literal path to a
    build folder in another repo that the person wanted gone. What works: refuse the shapes that go wrong without
    anyone meaning it (a path from a command substitution; an unquoted variable; a variable that may be empty, with no
    `${VAR:?}` guard and not set earlier in the same command; a relative path after a `cd` the hook can't follow;
    `xargs rm -r`), plus a floor nobody means (`/`, the home directory, any top-level directory, or a glob of one:
    `~/*`, `/*`), and let every other literal or guarded path run. Over `ssh`, refuse variables in a recursive delete
    (they expand on the other host) and apply the same floor. The refusal gives the fix ("Rewrite it with a
    guard on every variable, e.g. "${C:?}"/"${ID:?}".json, or use a literal path"), so the agent retries at once.
    About 18 ms a call (`python3 -I`, standard library only).
  - The lesson: the hook must be at least as strict as Claude Code's prompt on the same command shapes, or the looser
    cases fall through to the prompt and hang. A hook must answer fast and never ask. Pair the switch with the hook, so
    nothing is deleted unchecked.
  - Where it lives now: the author's home-server setup (the guard script and the session manager's unit).
- **Setup tokens can't use claude.ai features.** A `claude setup-token` token has only the inference scope: claude.ai
  connectors disappear, and very likely artifacts, Remote Control and cloud sessions. Fine for a fleet with its own
  replacements; say so before people switch. Where it lives: docs/plugins/claude-auth-manager.md ("What a setup token
  can't do").
- **Reading a token off a terminal UI.** Read the screen through a terminal emulator (`vt100`), take the token from
  one line only, and test it with one request before keeping it: joining lines once glued the next line of text onto
  every token, and all of them failed. Where it lives: plugins/claude-auth-manager/src/hub.rs (`find_token`).
- **Claude Code's keychain items on macOS.** `Claude Code-credentials` for the default sign-in, and
  `Claude Code-credentials-<first 8 hex of sha256(NFC config dir)>` for others, account `$USER`, written with
  `security -i` and `add-generic-password -U`. Overwrite in place: deleting first leaves a moment where sessions see
  no sign-in. Where it lives: docs/plugins/claude-auth-manager.md ("The tokens bundle").
- **Why claude.ai connectors (and other account-bound features) break down with several accounts and several
  machines**, and what replaces them.
  - **Several Claude accounts.** Connectors belong to a claude.ai account: each account has its own set, set up by
    hand, and each connector holds one identity (one Google account per Gmail, Drive or Calendar connector; switching
    means disconnecting and connecting again). A session an account rotator moves to another account gets that
    account's connectors, or none, in the middle of its work. Claude in Chrome answers only the account its extension
    is signed in to, so a moved session gets "signed in to a different account", and `/login` doesn't fix it.
  - **Several machines.** A full claude.ai sign-in has an access token of about 8 hours and a single-use refresh token
    that rotates, with a hard 30-day lifetime that refreshes don't extend. Two holders of one sign-in (machines,
    copied `.credentials.json`, many sessions on one machine) knock each other out: the loser gets `invalid_grant`,
    sometimes writes blank tokens over the winner's, and every session asks for `/login` (claude-code issues #21765,
    #24317, #88583, #108871). So every machine needs its own sign-in for every account, redone monthly: accounts ×
    machines sign-ins. The same race signs remote OAuth MCP servers out daily when 40+ sessions share one token entry.
  - **What works instead:** `claude setup-token` per account (a year, no refresh, no race), shared to every machine
    sealed (claude-auth-manager); the price is no connectors, artifacts or Remote Control. Replace them: one process
    that holds the MCP sign-ins for every session (an MCP gateway), a shared signed-in browser over Fleetlayer's
    tools (Playwright MCP) instead of Claude in Chrome, a phone relay for remote control, and your own page or
    document tooling for artifacts.
  - Where it lives now: docs/plugins/claude-auth-manager.md ("What a setup token can't do", "Token facts"), the
    author's research notes on Claude OAuth tokens and account rotation.
- **Recommended tools, with "X vs Y and why we chose X".** A series of short comparisons, each fair to the tool not
  chosen: what it does well, and why the pick fits a fleet that agents run from config in git.
  - **Gatus vs Uptime Kuma for uptime checks and heartbeats.** Gatus keeps every check in config files: they live in
    the infrastructure repo, go through review, are deployed with everything else, and an agent adds a check in the
    same change that adds a service (which "everything in the background is monitored" needs). Uptime Kuma is good
    and friendly, but its checks live in its own database behind a UI, so automating it means its API or editing the
    database directly, both worse than a file in git. It may move towards agent access (an API, MCP), so the guide
    shouldn't rule it out; it's a fine choice for someone who wants a UI first. Where it lives now: the author's
    home-server setup (Gatus external endpoints for heartbeats, alerts to a chat room an agent listens to).
- **Showing every account's usage in a menu bar app (CodexBar) without signing each machine in.** CodexBar reads
  several Claude accounts from an external program that prints claude-swap's account list (`--list --json`, schema
  v1); `fleetlayer-claude-accounts` prints the hub's accounts and usage that way, from readings. Footguns: CodexBar
  rejects the whole list over one malformed `resetsAt` (so normalise times, drop bad ones); the list needs
  `activeAccountNumber` present (null) and `supportsAccountSwitching: false` when the program can't switch, or
  CodexBar offers a switch that can't work; a GUI app on macOS has no `XDG_RUNTIME_DIR`, so the program must find the
  node's socket the same way the node does. Where it lives now: docs/plugins/claude-auth-manager.md ("Every account
  in a menu bar").
- **A new binary in a release isn't on PATH after a self-update.** The updating node links only the binaries the
  *old* version knew, so a binary added in the new release stayed unlinked. Fixed: on start, a node running from the
  installer's release folder links any of its release's binaries that have no link in `~/.local/bin` yet, and never
  touches an existing file or link there. A node installed some other way (a package, Ansible, `/opt`) does nothing:
  whatever installed it puts its binaries on PATH, and lists each binary by name, so a new binary has to be added
  there too. Where it lives now: crates/fleetlayer-node/src/update.rs (`link_missing`).
  - **claude-swap vs the original Claude account rotator vs our fork (and claude-auth-manager).** Four ways to run
    several Claude subscriptions:
    - claude-swap (realiti4/claude-swap, MIT, ~5k stars): a command line that keeps several accounts and switches the
      active one (`--switch-to`), with a JSON account list that CodexBar reads. It reads usage for setup-token
      accounts from the rate-limit headers of a one-token request (`usage.headerProbe`, `usage.fableProbe` for the
      Fable week). One active account at a time; verify how it handles many sessions at once when writing.
    - claude-account-rotator (ivokund, the original Claude Code mod): shows each account's usage under the prompt and
      moves a session to another account before the one it's on runs out. Each session polls on its own: fine for a
      few sessions.
    - The fork (ndbroadbent/claude-account-rotator): one coordinator per machine polls every account for all sessions
      (no polling storm at 40-60 sessions), handles Claude in Chrome's one-account limit, and had a menu bar app, now
      retired in favour of CodexBar.
    - Fleetlayer's claude-auth-manager: the sign-ins live on one always-on hub; every machine gets year-long setup
      tokens sealed; usage is shared fleet-wide as readings; the fork's coordinator runs on each node in borrow mode;
      CodexBar shows every account through `fleetlayer-claude-accounts`, which speaks claude-swap's list format.
    The pick depends on scale: one machine and a few sessions, claude-swap or the original; many sessions on one
    machine, the fork; many machines, claude-auth-manager. Where it lives now: docs/plugins/claude-auth-manager.md,
    the fork's README, the codexbar brief.
- **A web page for secrets that ships inside a Rust binary.** The hub's page (sign-in links and codes pass through
  it) embeds its HTML, CSS and JS with `include_str!` and serves them from its own routes, so the CSP can be
  `script-src 'self'; style-src 'self'` with no `unsafe-inline`. Footguns: inline `style=""` attributes are blocked
  too (set CSS custom properties from JS through `el.style.setProperty`, which CSP allows); a link to an outside
  sign-in page leaks the page's address unless `Referrer-Policy: no-referrer`; a native `<dialog>` closed with Escape
  fires `cancel`, which must also stop the server-side flow or the program keeps running until its timeout. Where it
  lives: `plugins/claude-auth-manager/src/hub.rs` (`security_headers`, `ASSETS`) and `web/`.
- **Claude's sign-in codes have a `#` in the middle, and copying from the browser can cut them short.** `claude auth
  login` says so and waits for the code again; `claude setup-token` fails outright and needs a new link. The hub's
  page refuses a code without a `#` before sending it, so a setup token isn't lost to a short copy. Where it lives:
  `plugins/claude-auth-manager/web/app.js` (`sendCode`).
- **Screenshots of a web UI from a small headless VM.** Use a Playwright browser already on the machine
  (`playwright-core` + `chromium_headless_shell`, pointing `executablePath` at it when versions differ) on
  localhost, with a small mock of the API that serves the real page files, so every state (failures, a code cut
  short, done) can be shown without real sign-ins and nothing opens to the network.
- **A Claude setup token belongs to whoever the browser was signed in as, and nothing checks it.** Making a token for
  account 2 while claude.ai is signed in as account 1 gives account 1's token, and every "does it work" check
  passes. Compare organizations: the profile's at sign-in, and the `anthropic-organization-id` header on one tiny
  `/v1/messages` request made with the token (`anthropic-beta: oauth-2025-04-20`). Where it lives:
  `plugins/claude-auth-manager/src/hub.rs` (`token_org`, `wrong_account`).
- **Filesystem events miss writes made on the other side of a share.** inotify sees only changes made through the
  local kernel: a vault shared into a VM over virtiofs (often re-exported through FUSE), an SMB or NFS mount, or a
  folder another machine writes over a network share never fires an event for writes made elsewhere. A service that
  reacts to new files (pages' live updates) has to poll: stat the few files of an open page every second and a
  folder's list every few seconds, which costs nothing at this scale. Where it lives: `plugins/pages/src/web.rs`
  (`watch`), `docs/plugins/pages.md` ("Live").
- **A dropped share looks like an empty folder.** When a synced or mounted folder goes away, a service reading it sees
  no files rather than an error, and happily serves an empty list. Put a marker file in the folder when you set it up
  (`.fleetlayer-space` for pages) and fail the health check and heartbeat when it's missing. Where it lives:
  `docs/plugins/pages.md` ("Monitoring").
- **Pages for people instead of vendor artifacts.** Claude's artifacts (and other vendors' hosted previews) open only
  for the account that published them, which breaks with several accounts or several models. Agents write a folder
  (`index.md` and `img/`) into their space of a synced vault, the pages service hosts it, and an agent you trust less
  gets only its own space's folder mounted. Where it lives: `docs/plugins/pages.md`; the setup guide (a vault share
  of its own, mounts per agent user) comes with the first deploy.
- **A sandboxed frame without `allow-same-origin` loses the sign-in cookie.** A mock-up shown in `<iframe sandbox>`
  gets an opaque origin, so its own CSS and images are requested cross-site and a SameSite=Strict session cookie
  isn't sent: they fail to load behind a login. Use `sandbox="allow-same-origin"` with no `allow-scripts` (and a
  `script-src 'none'` policy on the file) so nothing can use that origin. Where it lives: `plugins/pages/src/web.rs`.
- **One sign-in for a household's and a company's services, with agents that work for both.** A person is one
  account across every tenant they belong to; each tenant admits people and agents and decides with its own policy;
  a shared machine belongs to a team or tenant so it stays when the person who set it up leaves. The guide walks a
  company tenant and a family tenant through it, including an agent admitted to both (finances). Footguns: tenant IDs
  are part of every audit entry's hash, so a tenant is renamed by its slug and never by its ID; a command that
  changes memberships queues its audit entries for the running control plane instead of writing them itself (it
  doesn't hold the audit key, and two writers would race for the chain's next position). Where it lives:
  `docs/sso.md`, `crates/fleetlayer-control/src/people.rs`.
- **Backups.** Written: docs/guides/backups.md (restic to Backblaze B2 from one backup host, one repository per
  source, separate accounts per owner, retention, heartbeats, a scheduled restore check, disaster recovery, what to
  leave out, agents and backups). Example taken from the author's home server.
- **`node_modules` never goes into a synced or backed-up share.** Agents build apps in folders that are synced and
  backed up; a dependency tree is hundreds of thousands of files that sync slowly, churn backups and can be rebuilt
  from the lock file. Exclude it on every share (Syncthing `.stignore`, the backup excludes), with the usual build
  caches (`.next`, `.turbo`, `.cache`, `target`, `.venv`, `__pycache__`). Rule from Nathan, 27 Nov 2026. Where it
  lives: `docs/plugins/apps.md` ("Configuration"), the apps skill, the deploy notes.
- **Secure cookies are refused on plain-HTTP subdomains of localhost.** A browser treats `http://localhost` as secure
  enough for a `Secure` cookie but not `http://app.apps.localhost`, so a sign-in that sets `__Host-` cookies works in
  production (HTTPS) and silently fails on a laptop. Pages drops `Secure` and the prefix when its address is plain
  HTTP, and allows plain HTTP only for localhost. Where it lives: `plugins/pages/src/apps.rs` (`cookies`).
- **A wildcard certificate covers one level.** `*.home.example` doesn't cover `app.apps.home.example`: per-app
  subdomains need a wildcard for their own domain, or names one level under an existing wildcard
  (`app--space.home.example`). Where it lives: `docs/plugins/apps.md`.
- **Agent-written apps and a strict content policy.** Agents often write inline `<script>` blocks and load libraries
  from CDNs; an app origin's policy that allows only its own script files blocks both, so the service lists them as
  problems on the space's page and the apps skill tells agents up front (files only, bundle libraries). Where it
  lives: `plugins/pages/src/apps.rs` (`check_html`), `plugins/pages/apps-skill/SKILL.md`.
- **Isolated apps on sibling subdomains.** Each app on its own origin keeps scripts apart, but siblings share a site:
  one can set a cookie for the parent domain and shadow another's. `__Host-` cookies (host-only, Secure, Path=/)
  close that. Sign-in to many apps without registering each one: the pages site brokers OIDC-shaped one-time codes
  bound to each app's host and a state value. Where it lives: `docs/plugins/apps.md` ("Sign-in").
- **Messaging between agent sessions after a restart.** Claude Code's cross-session names are random (`<project>-3d`)
  and all change when a machine restarts, so sessions lose each other. Address sessions by their session manager's
  tab name instead (`fleetlayer msg send --to "<tab>@<node>"`), which is stable. Where it lives: AGENTS.md (Git).
- **`*.localhost` gives every laptop service its own address without TLS or root.** Browsers send any
  `<name>.localhost` to the loopback address and treat it as a secure context, so passkeys and sign-in work over plain
  HTTP. Programs outside the browser ask the system resolver, which differs: Debian 13 answers, Ubuntu 22.04 (and its
  curl 7.81) doesn't. So services talk to each other by backend address, and tests use
  `curl --connect-to ::127.0.0.1:7680`. Where it lives: `docs/guides/laptop.md` (Advanced).
- **A plugin announces its address after binding, so nobody picks ports.** Laptop configs say `listen: 127.0.0.1:0`;
  the plugin sends `_fleetlayer.dev/service/listening` with the port it got, the node forgets it when the plugin exits,
  and the gateway follows. On a server whose proxy runs elsewhere, use fixed ports instead. Where it lives:
  `docs/protocol.md` section 10.
- **The monitor shouldn't be a child of what it monitors.** The laptop gateway runs as its own user service beside
  the node rather than as a node plugin, so a dead node still shows on the status page and still alerts (a desktop
  notification at once, the session message when the node is back). launchd/systemd restart the gateway, and the
  node's health includes the gateway's `/healthz`. Where it lives: `crates/fleetlayer-gateway/src/main.rs`.
- **Alerts an agent can act on: one per change, one message per round.** Stopping the control plane also drops the
  node's link, so the gateway sends both changes as one message; a crash the service manager fixes within seconds is
  no alert; a message the node refuses (no such session) is shown on the status page instead of retried forever.
  Where it lives: `crates/fleetlayer-gateway/src/monitor.rs`.
- **Testing user services as another Unix user.** `sudo -iu <user> systemctl --user …` fails with "Failed to connect
  to bus" unless `XDG_RUNTIME_DIR=/run/user/$(id -u)` is set, and the user's services stop at logout unless
  `loginctl enable-linger <user>`. Found testing `fleetlayer up` on a Linux build server, 27 Nov 2026.
- **`fleetlayer-control` admin commands used `./fleetlayer-control.db` unless told otherwise** (fixed in v0.3.8). Run
  from the wrong folder, `node add` or `acl apply` quietly made a new, empty database. Since v0.3.8, with no `--db` it
  uses `./fleetlayer-control.db` if that exists, else the one `fleetlayer up` makes
  (`~/.local/state/fleetlayer/control/control.db`), and admin commands refuse to create a database at a path nobody
  named. Where it lives: `crates/fleetlayer-control/src/main.rs`; the guides leave out `--db` on a laptop.
