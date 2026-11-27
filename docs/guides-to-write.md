# Guides to write

Every setup step, footgun or design reason others will hit goes here in the same change: what it is, how it was
solved, and where the knowledge lives now. These become Fleetlayer's guides. They cover the whole journey of running a
fleet of agents, not only Fleetlayer itself: Claude Code's permissions and hooks, accounts and sign-ins, session
managers, machines that sleep.

- **Agents don't know what Fleetlayer gives them.** Without being told, an agent doesn't know it can message other
  sessions, start or resume them, use a shared browser or a laptop's folders, or what an outside-data line or a held
  call means. Solved with skills (`skills/fleetlayer/`, `skills/fleetlayer-browser/`, `plugins/folders/skill/`,
  `plugins/claude-auth-manager/skill/`, the pages and apps skills) and a short block for the person's global
  instructions. Claude Code reads skills from `~/.claude/skills/<name>/`, Codex from `~/.agents/skills/<name>/`, and
  Codex's global instructions are `~/.codex/AGENTS.md` (`$CODEX_HOME`). `fleetlayer me` (v0.3.9) lists what a
  session may use, so neither the skills nor the instructions keep a list that goes stale. Written:
  `docs/guides/agents-know-fleetlayer.md`. The node installs the skills itself from v0.3.9 (`crates/fleetlayer-node/src/skills.rs`,
  release tarballs' `skills/` from `scripts/collect-skills`), never over a folder a person made.
- **A GitHub organization can't be created from the CLI or the API on github.com.** `POST /admin/organizations`
  exists only on GitHub Enterprise Server (it answers 404 on github.com, whatever the token's scopes). An agent can
  create repos with `gh repo create <org>/<name>`, but a person makes the organization once in the web UI
  (github.com/account/organizations/new). Where it matters: any guide that has an agent set up a new project's org.
  Written: `docs/guides/agents-building-software.md`.
- **On macOS the node must run as a per-user LaunchAgent.** Writing Claude Code's keychain item needs the login
  keychain unlocked: a LaunchDaemon, or an SSH session on a Mac whose keychain is locked, fails with "User
  interaction is not allowed". Where it lives: `docs/plugins/claude-auth-manager.md`.
  Written: `docs/guides/node-services.md`, `docs/guides/claude-accounts.md`.
- **Driving your own signed-in browser from an agent on another machine.** Written: `docs/guides/browser.md`. Playwright
  MCP's extension mode, with its server on the laptop (the extension only talks to loopback). Interim transport: the laptop dials out over SSH with
  a reverse stream-local forward, a socket-activated launchd agent starts one `@playwright/mcp --extension
  --session-name <name>` per connection, and each connection opens with `<name>\t<token>\n`. The homeserver session is
  building it and will send the full setup (sshd Match block, launchd agents, probe, off switch, test results) for
  this guide. Later: Fleetlayer's "tools on another machine" replaces the transport with the same contract.
- **Claude Code sessions in bypass mode hold every Fleetlayer message for approval** unless
  `~/.claude/settings.json` has `"crossSessionInbound": "accept"`. That setting also accepts unattested messages from
  any process of the same user that can read the session's key file: the guide says so and leaves the choice to the
  user. Where it lives: `plugins/claude-code/src/main.rs`. Written: `docs/guides/unattended-agents.md`.
- **Access control for a fleet with an agent you trust less.** A new control plane allows every message. Before a node
  that reads untrusted input joins (an email triage agent), add rules: `fleetlayer-control acl add --from
  '*@triage-node' --to 'inbox@main' allow`, then `--from '*@triage-node' --to '*' deny`, or set `acl default deny`
  and list what's allowed. A node sees only the sessions it may message, and subscribes only its own. Where it
  lives: `crates/fleetlayer-control/src/acl.rs`.
  Written: `docs/guides/untrusted-input.md` (version 2 policy, tested with `acl test`
  against v0.3.7).
- **A browser MCP server started by launchd can't find Chrome's profile.** macOS doesn't let a launchd-spawned process
  read Chrome's profile folders, so Playwright MCP's extension mode fails with "Playwright Extension not found". Fix:
  set `PLAYWRIGHT_MCP_EXECUTABLE_PATH` to Chrome's binary and pass `--profile-dir-name Default`. Six sessions at once
  each got their own tab group with no cross-talk; navigation over an SSH tunnel takes 0.5-1 s. Found by the
  homeserver session, 26 Nov 2026. Fleetlayer's node will hit the same thing when it starts the server.
- **Stopping test processes with `pkill -f` can kill your own shell**: the pattern matches the shell's own command
  line. Use a bracket (`pkill -f 'bin/[f]leetlayer-node'`), and never `pkill -x fleetlayer-node` on a machine where the
  node runs as a service. Written: `docs/guides/node-services.md`, `docs/guides/agents-building-software.md`.
- **macOS Local Network privacy can block the node's first links.** On macOS 15+, a launchd-started binary that isn't
  Apple's dialling a LAN address gets "No route to host (os error 65)" until Local Network access is allowed. The
  node retries with backoff, so it usually links once the person allows the prompt. If a fresh install keeps failing
  that way, check System Settings > Privacy & Security > Local Network. Seen installing the first Mac node, 26 Nov 2026.
  Written: `docs/guides/node-services.md`.
- **macOS asks for the same permission again after every upgrade of an unsigned program.** macOS stores a privacy
  answer (Files and Folders, Local Network) for an unsigned or ad-hoc signed binary by its path and code hash, and
  each release has a new path (`~/.local/lib/fleetlayer/<version>/`) and new bytes. The prompt names the program
  launchd started (the node), not the tool it runs, and the tool's call hangs until someone answers. Solved by signing
  the macOS release binaries with a Developer ID: macOS then stores the designated requirement (team plus identifier),
  which is the same in every release. Three things make it work: an explicit `--identifier` per binary (the linker's
  default, `fleetlayer_node-<hash>`, changes every build), the hardened runtime (Apple won't notarise without it; Rust
  binaries that start other programs need no entitlements), and `--timestamp`. Bare binaries can't be stapled, so
  notarise a zip of them and let macOS look the ticket up online; curl and the node's update don't mark files as
  downloaded, so they never need the lookup. In CI: a temporary keychain, `security set-key-partition-list` or
  codesign waits for a dialog, and check the submission's status, not notarytool's exit code. The first signed
  release still asks once, since the old answer belonged to a path. Where it lives: `scripts/release/sign-macos.sh`,
  `docs/releasing.md`. Written: `docs/guides/folders.md` (section 3).
- **Testing a release's binaries beside a running node.** Before v0.3.9 a node ignores `FLEETLAYER_INSTANCE`, and any
  node started by hand binds the default socket: it removes the running node's socket file and takes its place, and
  when it stops, the real node is still running with no socket (`fleetlayer status` says it can't reach the node)
  until its service is restarted. Seen while smoke-testing signed binaries, 27 Nov 2026. Fixed in v0.3.9: a node
  claims its sockets before any plugin starts and refuses to start when another node answers there ("another node is
  already running on <socket> (pid N): stop it first, or use FLEETLAYER_INSTANCE for a second one"), replaces only a
  stale socket nothing answers on, and on exit removes a socket file only if it's still the one it created. A test
  copy uses `FLEETLAYER_INSTANCE` (`docs/guides/laptop.md`, Advanced; `docs/guides/node-services.md`). Where it lives:
  `crates/fleetlayer-node/src/socket.rs`.
- **The browser over an SSH bridge (the interim before Fleetlayer's shared tools).** From the homeserver session's
  build, 26 Nov 2026: OpenSSH's `AllowTcpForwarding no` also refuses Unix-socket remote forwards, so use
  `AllowTcpForwarding remote` with `PermitListen none`; a socket created in a setgid directory inherits its group;
  macOS TCC stops a launchd process reading Chrome's profile (set `PLAYWRIGHT_MCP_EXECUTABLE_PATH`); each connection
  opens with a `name<TAB>token` line. Sources: the homeserver repo's `ansible/roles/agents_vm/tasks/browser_tunnel.yml`,
  `scripts/browser-mcp/`, and `docs/services/agents-vm.md` ("Browser tunnel").
- **After upgrading by hand, rerun `fleetlayer-node install` if the service runs a versioned path.** Since 26 Nov the
  installer writes the `PATH` symlink (`~/.local/bin/fleetlayer-node`) when it points at the running binary, so
  swapping symlinks reaches the service; older installs wrote the versioned path and kept running the old binary.
  Written: `docs/guides/node-services.md`.
- **Proxies close idle WebSockets.** Traefik 3.4's default entrypoint idleTimeout is 180 s, and a shared tool's stream
  (a browser session between tasks) can sit idle much longer. Fleetlayer's nodes ping their stream sockets every 30 s,
  and the control plane pings each node's link every 30 s. If you put another proxy in front of a control plane, check its idle timeout is longer than
  30 s. Found when a 5-minute idle browser stream was dropped, 26 Nov 2026. Written: `docs/guides/monitoring.md`.
- **Don't let a node summarise another user's sessions.** Archive or RAG syncs can mirror one Unix user's Claude
  transcripts into another's `~/.claude/projects`. The summaries plugin now skips sessions whose recorded cwd is
  outside its own user's home; any tool that reads transcripts needs the same check.
  Written: `docs/guides/day-to-day.md`.
- **launchctl bootstrap right after bootout can fail** with "Bootstrap failed: 5: Input/output error": bootout returns
  before launchd has unloaded the service. `fleetlayer-node install` now waits until `launchctl print` no longer finds
  it, then bootstraps, retrying once. Found by the homeserver session installing the Mac node (1 in 3 runs).
  Written: `docs/guides/node-services.md`.
- **Test nodes on a machine where people work must set `typing_fallback: false`.** A test node with only the herdr
  plugin types messages into the real tab when it can't use Claude Code's socket, so a test message lands as the
  person's input in the middle of their work. Happened once while testing access policy v2, 26 Nov 2026.
  Written: `docs/guides/node-services.md`.
- **Sharing a browser MCP server safely (Playwright MCP 0.0.86, extension mode).** Found by the homeserver session,
  26 Nov 2026: `browser_run_code_unsafe` is always on (deny it with `tools:`); file arguments are confined to the
  client's MCP roots or the server's working directory, so pass `--ignore-roots` and give it an empty `cwd` (a
  launchd service's cwd is `/`); `browser_close` only disconnects in extension mode and leaves tabs in a grey group, so
  clean up with `browser_tabs {action: close, index: 0}` repeated one at a time (pipelined calls race and leave tabs).
  Where it lives: docs/getting-started.md, docs/access-control.md. Written: `docs/guides/browser.md`.
- **A LaunchAgent can't find `claude`.** launchd gives agents a PATH without `~/.local/bin`, where Claude Code's
  installer puts it, so anything a node runs that calls `claude` fails with "No such file or directory". The
  claude-auth-manager plugin looks on PATH and then in the installers' places (`~/.local/bin`, `~/.claude/local`,
  Homebrew), and takes `claude:` in its config. Other plugins that run `claude` need the same. Found writing the
  token checks, 26 Nov 2026. Written: `docs/guides/unattended-agents.md`.
- **Running `claude -p` from a token slot without touching the person's setup.** Set
  `CLAUDE_SECURESTORAGE_CONFIG_DIR` to the slot (that's where the credentials are read from, on macOS and Linux) and
  `CLAUDE_CONFIG_DIR` to a scratch directory, so Claude Code leaves its config files there and not in the slot; add
  `--setting-sources project` from an empty working directory (no user settings, so no plugins or hooks),
  `--strict-mcp-config` and `--no-session-persistence`; remove `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` and the
  other overriding variables. Tested with Claude Code 2.1.332, 26 Nov 2026; it lives in claude-auth-manager's
  `health.rs`. Written: `docs/guides/claude-accounts.md`.
- **One coordinator per user for the Claude account rotator.** `coordinator.sock` in `~/.claude/account-rotator` is
  served by whichever process holds `coordinator.lock`: the rotator's own coordinator or claude-auth-manager. A
  rotator's own coordinator is asked to step down (the `claude.coordinator` reading says who holds the lock). Quietdeck
  used to take it too; it now ignores `usage.claude` and never takes the lock. Sessions need rotator
  0.7.6 or later to leave the plugin's coordinator alone. 26 Nov 2026. Written: `docs/guides/claude-accounts.md`.
- **Syncing a checkout to a build server with `rsync -a` can make cargo skip a changed file.** It copies the local
  modification time, which can be older than the server's last build, so the build uses stale code and reports
  errors about things that exist. `scripts/on-dev` now syncs by content (`--checksum`) with fresh times on changed
  files. Found 26 Nov 2026. Written: `docs/guides/agents-building-software.md`.
- **Signing Claude Code in on a machine with no browser.** `claude auth login --claudeai` prints a URL and reads a
  pasted code from stdin (a pipe is enough); `claude setup-token` needs a pseudo-terminal and prints the token on
  screen. Tested with 2.1.332, 26 Nov 2026; details in docs/plugins/claude-auth-manager.md.
  Written: `docs/guides/claude-accounts.md`.
- **Running the Claude sign-in hub.** An always-on machine (never the laptop), its own Unix user as a hardened system
  service, its page behind a private HTTPS proxy with `trusted_proxies` set (else one person's typos lock everyone out),
  a firewall rule so only the proxy reaches its port, an uptime check on `/healthz` plus its heartbeat, its folder left
  out of backups on purpose. How it went is in docs/plugins/claude-auth-manager.md ("The hub").
  Written: `docs/guides/claude-accounts.md`.
- **Watching and driving any session's terminal from anywhere.** `fleetlayer attach <tab>@<node>` (Ctrl-] leaves,
  `--observe` only watches). herdr's `terminal session observe|control` is the stream; the access policy's `read` and
  `attach` decide who. A shell inside a herdr pane carries `HERDR_SOCKET_PATH`, which beats `HERDR_SESSION`: unset it
  to reach a named test session. Design: docs/protocol.md ("Terminals"). Written: `docs/guides/day-to-day.md`.
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
  Written: `docs/guides/unattended-agents.md`.
- **Setup tokens can't use claude.ai features.** A `claude setup-token` token has only the inference scope: claude.ai
  connectors disappear, and very likely artifacts, Remote Control and cloud sessions. Fine for a fleet with its own
  replacements; say so before people switch. Where it lives: docs/plugins/claude-auth-manager.md ("What a setup token
  can't do"). Written: `docs/guides/claude-accounts.md`.
- **Reading a token off a terminal UI.** Read the screen through a terminal emulator (`vt100`), take the token from
  one line only, and test it with one request before keeping it: joining lines once glued the next line of text onto
  every token, and all of them failed. Where it lives: plugins/claude-auth-manager/src/hub.rs (`find_token`).
  Written: `docs/guides/claude-accounts.md`.
- **Claude Code's keychain items on macOS.** `Claude Code-credentials` for the default sign-in, and
  `Claude Code-credentials-<first 8 hex of sha256(NFC config dir)>` for others, account `$USER`, written with
  `security -i` and `add-generic-password -U`. Overwrite in place: deleting first leaves a moment where sessions see
  no sign-in. Where it lives: docs/plugins/claude-auth-manager.md ("The tokens bundle").
  Written: `docs/guides/claude-accounts.md`.
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
    author's research notes on Claude OAuth tokens and account rotation. Written: `docs/guides/claude-accounts.md`.
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
  in a menu bar"). Written: `docs/guides/claude-accounts.md`.
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
      Fable week). It switches the machine's active account, and also runs accounts side by side (`cswap run 2`).
    - claude-account-rotator (ivokund, the original Claude Code mod): shows each account's usage under the prompt and
      moves a session to another account before the one it's on runs out. Sessions share their readings and check each
      account at most every 5 minutes between them (per its README).
    - The fork (ndbroadbent/claude-account-rotator): one coordinator per machine polls every account for all sessions
      (no polling storm at 40-60 sessions), handles Claude in Chrome's one-account limit, and had a menu bar app, now
      retired in favour of CodexBar.
    - Fleetlayer's claude-auth-manager: the sign-ins live on one always-on hub; every machine gets year-long setup
      tokens sealed; usage is shared fleet-wide as readings; the fork's coordinator runs on each node in borrow mode;
      CodexBar shows every account through `fleetlayer-claude-accounts`, which speaks claude-swap's list format.
    The pick depends on scale: one machine and a few sessions, claude-swap or the original; many sessions on one
    machine, the fork; many machines, claude-auth-manager. Where it lives now: docs/plugins/claude-auth-manager.md,
    the fork's README, the codexbar brief. Written: `docs/guides/node-services.md`.
- **A web page for secrets that ships inside a Rust binary.** The hub's page (sign-in links and codes pass through
  it) embeds its HTML, CSS and JS with `include_str!` and serves them from its own routes, so the CSP can be
  `script-src 'self'; style-src 'self'` with no `unsafe-inline`. Footguns: inline `style=""` attributes are blocked
  too (set CSS custom properties from JS through `el.style.setProperty`, which CSP allows); a link to an outside
  sign-in page leaks the page's address unless `Referrer-Policy: no-referrer`; a native `<dialog>` closed with Escape
  fires `cancel`, which must also stop the server-side flow or the program keeps running until its timeout. Where it
  lives: `plugins/claude-auth-manager/src/hub.rs` (`security_headers`, `ASSETS`) and `web/`.
  Written: `docs/guides/agents-building-software.md`.
- **Claude's sign-in codes have a `#` in the middle, and copying from the browser can cut them short.** `claude auth
  login` says so and waits for the code again; `claude setup-token` fails outright and needs a new link. The hub's
  page refuses a code without a `#` before sending it, so a setup token isn't lost to a short copy. Where it lives:
  `plugins/claude-auth-manager/web/app.js` (`sendCode`). Written: `docs/guides/claude-accounts.md`.
- **Screenshots of a web UI from a small headless VM.** Use a Playwright browser already on the machine
  (`playwright-core` + `chromium_headless_shell`, pointing `executablePath` at it when versions differ) on
  localhost, with a small mock of the API that serves the real page files, so every state (failures, a code cut
  short, done) can be shown without real sign-ins and nothing opens to the network.
  Written: `docs/guides/agents-building-software.md`.
- **A Claude setup token belongs to whoever the browser was signed in as, and nothing checks it.** Making a token for
  account 2 while claude.ai is signed in as account 1 gives account 1's token, and every "does it work" check
  passes. Compare organizations: the profile's at sign-in, and the `anthropic-organization-id` header on one tiny
  `/v1/messages` request made with the token (`anthropic-beta: oauth-2025-04-20`). Where it lives:
  `plugins/claude-auth-manager/src/hub.rs` (`token_org`, `wrong_account`). Written: `docs/guides/claude-accounts.md`.
- **Filesystem events miss writes made on the other side of a share.** inotify sees only changes made through the
  local kernel: a vault shared into a VM over virtiofs (often re-exported through FUSE), an SMB or NFS mount, or a
  folder another machine writes over a network share never fires an event for writes made elsewhere. A service that
  reacts to new files (pages' live updates) has to poll: stat the few files of an open page every second and a
  folder's list every few seconds, which costs nothing at this scale. Where it lives: `plugins/pages/src/web.rs`
  (`watch`), `docs/plugins/pages.md` ("Live"). Written: `docs/guides/pages.md`.
- **A dropped share looks like an empty folder.** When a synced or mounted folder goes away, a service reading it sees
  no files rather than an error, and happily serves an empty list. Put a marker file in the folder when you set it up
  (`.fleetlayer-space` for pages) and fail the health check and heartbeat when it's missing. Where it lives:
  `docs/plugins/pages.md` ("Monitoring"). Written: `docs/guides/pages.md`.
- **Pages for people instead of vendor artifacts.** Claude's artifacts (and other vendors' hosted previews) open only
  for the account that published them, which breaks with several accounts or several models. Agents write a folder
  (`index.md` and `img/`) into their space of a synced vault, the pages service hosts it, and an agent you trust less
  gets only its own space's folder mounted. Where it lives: `docs/plugins/pages.md`; the setup guide (a vault share
  of its own, mounts per agent user) comes with the first deploy. Written: `docs/guides/pages.md`.
- **A sandboxed frame without `allow-same-origin` loses the sign-in cookie.** A mock-up shown in `<iframe sandbox>`
  gets an opaque origin, so its own CSS and images are requested cross-site and a SameSite=Strict session cookie
  isn't sent: they fail to load behind a login. Use `sandbox="allow-same-origin"` with no `allow-scripts` (and a
  `script-src 'none'` policy on the file) so nothing can use that origin. Where it lives: `plugins/pages/src/web.rs`.
  Written: `docs/guides/pages.md`.
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
  lives: `docs/plugins/apps.md` ("Configuration"), the apps skill, the deploy notes. Written: `docs/guides/pages.md`.
- **Secure cookies are refused on plain-HTTP subdomains of localhost.** A browser treats `http://localhost` as secure
  enough for a `Secure` cookie but not `http://app.apps.localhost`, so a sign-in that sets `__Host-` cookies works in
  production (HTTPS) and silently fails on a laptop. Pages drops `Secure` and the prefix when its address is plain
  HTTP, and allows plain HTTP only for localhost. Where it lives: `plugins/pages/src/apps.rs` (`cookies`).
  Written: `docs/guides/pages.md`.
- **A wildcard certificate covers one level.** `*.home.example` doesn't cover `app.apps.home.example`: per-app
  subdomains need a wildcard for their own domain, or names one level under an existing wildcard
  (`app--space.home.example`). Where it lives: `docs/plugins/apps.md`. Written: `docs/guides/pages.md`.
- **Agent-written apps and a strict content policy.** Agents often write inline `<script>` blocks and load libraries
  from CDNs; an app origin's policy that allows only its own script files blocks both, so the service lists them as
  problems on the space's page and the apps skill tells agents up front (files only, bundle libraries). Where it
  lives: `plugins/pages/src/apps.rs` (`check_html`), `plugins/pages/apps-skill/SKILL.md`.
  Written: `docs/guides/pages.md`.
- **Isolated apps on sibling subdomains.** Each app on its own origin keeps scripts apart, but siblings share a site:
  one can set a cookie for the parent domain and shadow another's. `__Host-` cookies (host-only, Secure, Path=/)
  close that. Sign-in to many apps without registering each one: the pages site brokers OIDC-shaped one-time codes
  bound to each app's host and a state value. Where it lives: `docs/plugins/apps.md` ("Sign-in").
  Written: `docs/guides/pages.md`.
- **Messaging between agent sessions after a restart.** Claude Code's cross-session names are random (`<project>-3d`)
  and all change when a machine restarts, so sessions lose each other. Address sessions by their session manager's
  tab name instead (`fleetlayer msg send --to "<tab>@<node>"`), which is stable. Where it lives: AGENTS.md (Git).
  Written: `docs/guides/unattended-agents.md`, `docs/guides/day-to-day.md`.
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
  Written: `docs/guides/monitoring.md`.
- **Alerts an agent can act on: one per change, one message per round.** Stopping the control plane also drops the
  node's link, so the gateway sends both changes as one message; a crash the service manager fixes within seconds is
  no alert; a message the node refuses (no such session) is shown on the status page instead of retried forever.
  Where it lives: `crates/fleetlayer-gateway/src/monitor.rs`. Written: `docs/guides/monitoring.md`.
- **Testing user services as another Unix user.** `sudo -iu <user> systemctl --user …` fails with "Failed to connect
  to bus" unless `XDG_RUNTIME_DIR=/run/user/$(id -u)` is set, and the user's services stop at logout unless
  `loginctl enable-linger <user>`. Found testing `fleetlayer up` on a Linux build server, 27 Nov 2026.
  Written: `docs/guides/node-services.md`.
- **A shared tool call that hangs forever.** An agent using a shared browser (`fleetlayer mcp browser@<mac>`) waited
  ten minutes, until it was restarted, after the Mac's node restarted mid-call. Claude Code waits a very long time for
  an MCP tool call by default (its `MCP_TOOL_TIMEOUT` setting, in milliseconds, shortens that), and nothing on the way
  noticed the other end was gone: a ping into a half-open TCP connection succeeds. Now the limits are Fleetlayer's: a
  stream with nothing received for 90 s (three pings) is closed by the nodes and by the control plane's relay, which
  also closes one leg the moment the other closes; `fleetlayer mcp` gives the tool's first answer 30 s; and each call
  gets a JSON-RPC error after `--call-timeout` (120 s): "<tool>@<node> didn't answer within 120s (the machine may be
  asleep, or its node restarting)", with later calls still working. Found 27 Nov 2026. Where it lives:
  `crates/fleetlayer-cli/src/mcp_bridge.rs`, `IDLE` in `crates/fleetlayer-node/src/tools.rs` and
  `crates/fleetlayer-control/src/streams.rs`.
- **A user service's PATH isn't your shell's.** A PATH copied from the shell into a launchd plist or systemd unit can
  hold `~/.local/bin`, `$HOME/...` or `./node_modules/.bin`: the shell expands and resolves those, launchd, systemd
  and execvp don't, and the service runs in another folder. So a tool the shell found (herdr in `~/.local/bin`) was
  "No such file or directory" for the service. Fixed when writing the unit: expand a leading `~` and `$HOME` against
  the account's home, drop relative, empty and repeated entries; and `fleetlayer up` writes the absolute path of the
  programs plugins run (herdr's `program:`, summaries' `claude:`). Found by the Mac test, 27 Nov 2026. Where it lives:
  `service_path` in `crates/fleetlayer-protocol/src/local.rs`.
- **A second copy of a user service under the same account.** A test copy with only its own `HOME` still replaces the
  real one: launchd labels and systemd unit names are per account, and the service manager starts the job with the
  account's real `HOME`, so it reads the real config. And systemd reads user units only from the account's own
  `~/.config/systemd/user`, so a unit file under another `HOME` isn't found ("Unit file … does not exist"). Solved
  with `FLEETLAYER_INSTANCE`: its own labels and unit names, `HOME` written into the unit, `systemctl --user link`
  for the file, its own node socket under `$XDG_RUNTIME_DIR`, and its own ports by default (7690, 7700). It also means testing
  `fleetlayer up` on a Mac needs no throwaway macOS user. Where it lives: `crates/fleetlayer-protocol/src/local.rs`,
  `docs/guides/laptop.md` (Advanced).
- **`fleetlayer-control` admin commands used `./fleetlayer-control.db` unless told otherwise** (fixed in v0.3.8). Run
  from the wrong folder, `node add` or `acl apply` quietly made a new, empty database. Since v0.3.8, with no `--db` it
  uses `./fleetlayer-control.db` if that exists, else the one `fleetlayer up` makes
  (`~/.local/state/fleetlayer/control/control.db`), and admin commands refuse to create a database at a path nobody
  named. Where it lives: `crates/fleetlayer-control/src/main.rs`; the guides leave out `--db` on a laptop.
- **Moving sign-in without breaking passkeys.** A passkey belongs to the domain it was made for (the relying party
  ID), not to the machine. Make passkeys for a registrable domain you keep (`example.com`, or a tailnet's
  `tail1234.ts.net`) and sign-in can move between machines and addresses under it; made on `localhost` or one
  machine's name, they stop working after a move and everyone registers again. The move guide says this up front, in
  the same words as the sign-in page's warning. Where it lives: `docs/sso.md` ("Moving it").
- **An agent no tenant admitted can't do anything.** Since decisions moved to the tenants that own what's touched,
  every request is decided in the tenants either agent is a member of, and an agent no tenant admitted is refused
  ("neither … is a member of any tenant"). Upgrading keeps working: on upgrading, every node no tenant admits (and
  no team or tenant owns) has its agents admitted to the first tenant, recorded in its audit log, and `node add`
  admits a new node's agents to the tenant it's run for (the first, unless `--tenant`) unless `--no-admit`. A node added with `--no-admit`, or a
  person's node after they leave every tenant, needs `fleetlayer-control agent admit "*@<node>"` in some tenant first.
  `acl explain --tenant <slug>` says which. Where it lives: `docs/sso.md` ("On the control plane").
- **A shared tool has two gates: the node's `allow` list and the control plane's policy.** The sharing node's
  `tools: allow:` decides who it will serve at all; the policy decides per caller on top. A list written for remote
  callers only (`['*@agents']`) refused the sharing machine's own sessions, which looked like a policy denial. Prefer
  `allow: ['*']` on the node and let the policy be the one place that says who (deny untrusted-input agents there).
  The error names the node's list ("may not use browser on <node>") when it's the node refusing.

## Pushing pages to a service that only reads its folders

- **What:** a pages service set up to read its spaces through a read-only view (a read-only bind mount, so the web
  service can never change what agents wrote) can't receive `fleetlayer pages push`, because the receiver runs as the
  same user.
- **How it was solved:** push checks that it can write before asking for any file and answers "this space doesn't
  accept pushes". The deployment gives the service a writable view of only the spaces that accept pushes, and keeps
  the rest read-only.
- **Where it lives:** `docs/plugins/pages.md`, "From another machine".

- **Claude Code's folder trust prompt blocks a session you start for an agent.** Claude Code 2.1.332 asks "Do you trust
  this folder?" even in a folder whose parent is trusted, and never remembers trust for the home folder, so an
  unattended session started there waits forever. Before starting a session, mark the exact folder trusted
  (`projects.<absolute path>.hasTrustDialogAccepted: true` in `~/.claude.json`, written atomically), and start
  sessions in a project or scratch folder, never the home folder. `fleetlayer session start|resume` does this:
  the Claude Code plugin marks the folder (`_fleetlayer.dev/harness/prepare`) and the node refuses the home folder.
  Where it lives: `docs/guides/day-to-day.md` ("Start or resume a session anywhere"), `docs/protocol.md` ("Starting
  sessions").
- **An agent started for someone must never take a flag from the caller.** A session another machine starts gets its
  permission mode and other flags only from the node it runs on (`harnesses:` in `node.yaml`); the caller sends no
  arguments, and a session ID that looks like a flag is refused. Otherwise anyone allowed to start a session could
  start one with `--dangerously-skip-permissions`. Where it lives: `crates/fleetlayer-node/src/start.rs`,
  `docs/guides/day-to-day.md`.
- **Per-machine instructions for agents.** One shared global instructions file for every machine, plus one file per
  machine (`CLAUDE.this-machine.md`, left out of the sync and pulled in from the global one) for what holds only there:
  no browser, no Touch ID, how to reach the other machines. From Quietdeck's list, 27 Nov 2026.
  Written: `docs/guides/agent-instructions.md`.
- **Two scratchpads per agent.** A wiped one (`/tmp` or the session's scratchpad) for intermediate files, and a kept,
  synced one in the notes vault, named after the agent's role, for working notes worth keeping.
  Written: `docs/guides/agent-instructions.md`.
- **Secrets the person pastes in: create the stub item first.** The agent creates the secrets-manager item (right
  vault, kebab-case name, the field its code reads, empty) and names it, so the person only opens it and pastes.
  Written: `docs/guides/agent-instructions.md`.
- **Continuous deployment without races.** CI builds the image and commits its digest to the infra repo with a deploy
  key; production only moves forward (an ancestry check before each push); main's runs queue; migrations may only add
  during a rolling deploy; Argo CD backs up before and checks after. Why not an image updater (build order isn't commit
  order). Where it lives: Quietdeck's `deploy/README.md` ("Continuous deployment"). Written: `docs/guides/continuous-deployment.md`.
- **A server VM for agents.** A login user without sudo, lingering, Tailscale SSH, herdr, the node. Plus dev servers on
  a remote machine: Vite refuses host names it doesn't know (403), and `crypto.randomUUID` exists only on HTTPS pages
  or localhost; use the tailnet's full name and allow it. Written: `docs/guides/agents-vm.md`.
- **Syncing notes, skills and instructions between machines.** Written: `docs/guides/sync.md`.
- **Never type a message into a terminal whose agent has ended.** A session manager's typing fallback is for a running
  agent with no messaging channel. Once the agent's process is gone, its pane may be a plain shell, and typed text runs
  as commands. Fleetlayer types only on "no channel" (the harness sees a live process without a usable socket), never
  on "gone". Found by Quietdeck's delivery (9f31dd5); Fleetlayer: crates/fleetlayer-node/src/deliver.rs and the
  claude-code plugin.
- **herdr starts a new agent without your usual flags.** `herdr tab create --agent claude` runs plain `claude`, and
  herdr's `[session.resume_extra_args]` (e.g. `claude = ["--dangerously-skip-permissions"]`) applies only when it
  resumes a session, never to a new one. Pass the flags after `--`: `herdr tab create --label "<tab>" --agent claude --
  --dangerously-skip-permissions --name "<tab>" "<first prompt>"`, and put that in the agents' global instructions so
  an agent that starts a session for you uses it. Found 27 Nov 2026, when a new tab came up in the wrong mode.
- **A sync that stops quietly.** Three ways a one-way sync looked fine while it wasn't: an approval that expired after a
  month of daily use, a version refused once and never sent again, and a target that came back while the source was
  asleep and waited for the next change. Fixed in the node: approvals for `sync` and `use_tool` renew while used, the
  source's node retries targets with backoff and right after an approval, and the control plane asks the source to
  resend when either side links. Each target's state is a `sync.status` reading saying what to do, and a refusal
  lasting an hour fails the heartbeat. Also: a skills folder full of symlinks into a repo syncs nothing for those
  skills, since links that point outside the folder are never followed (the `files.set` reading lists them). Where it
  lives: `docs/protocol.md` section 8, `plugins/files/src/main.rs`. To write into: `docs/guides/sync.md`.
- **A plugin is only as trusted as the agents on its node.** Anything running as the node's Unix user can read its
  token and act as any of its sessions or plugins, so a rule letting `plugin:runner@<untrusted node>` start sessions
  elsewhere lets a prompt injection in that agent's input do it too. Run anything trusted more than its node's agents
  under its own Unix user and node. Found by homeserver and Quietdeck reviewing the triage setup before the
  session-start release. Where it lives: AGENTS.md, docs/access-control.md.
  sessions in a project or scratch folder, never the home folder. Where it lives: Quietdeck's session start
  (control.rs `trust_in`); Fleetlayer's `fleetlayer session start` (planned).
- **One sign-in for self-hosted services, with passkeys.** Point a service's OpenID Connect settings at the sign-in
  plugin, register it as a client with its exact return address, and give people `sign_in` on `service:<name>` in
  the policy; without that rule a person reaches nothing, by design. A service that keeps no secret (a browser app)
  is a public client, and PKCE is required of every client either way. Where it lives: `docs/plugins/signin.md`.

## Signing a web app in with OpenID Connect: three browser and server footguns

- **What:** wiring pages to Fleetlayer's sign-in turned up three problems any self-hosted app hits:
  1. A SameSite=Strict session cookie set on the callback isn't sent with the redirect that follows, because the whole
     redirect chain counts as the identity provider's cross-site navigation, so the person lands on the login page
     again.
  2. A `form-action 'self'` CSP blocks the sign-out form's redirect to the provider's logout, and nothing happens.
  3. On a laptop, the server can't resolve `signin.localhost`: browsers treat every `*.localhost` name as this machine,
     but glibc without systemd-resolved doesn't.
- **How it was solved:** (1) and (2): answer with a tiny page that moves on (`<meta http-equiv="refresh">`) instead
  of a redirect. (3): the server's HTTP client resolves `*.localhost` to 127.0.0.1 itself. Server-to-server addresses
  can be 127.0.0.1 directly where a setting allows it.
- **Where it lives:** `plugins/pages/src/web.rs` (`auth_callback`, `logout`) and `oidc.rs` (`client_for`);
  `docs/plugins/pages.md`, "Sign-in".

- **A server can't reach `*.localhost`.** Browsers resolve every `*.localhost` name to this machine themselves, but
  a server's own resolver often doesn't (glibc on Linux without systemd-resolved), so a service that calls another
  by its `*.localhost` address (pages fetching the sign-in service's discovery document) fails with a DNS error. Give
  server-to-server calls `127.0.0.1:<port>` (the sign-in service's `control_plane`), or resolve `*.localhost` to
  loopback in the client, as pages does. Found by the pages tab testing against a real sign-in service. Where it
  lives: `docs/plugins/signin.md` (Settings) and pages' HTTP client.
- **Many agents share one GitHub API limit.** 5,000 requests an hour per user, shared by every session using that
  user's token; `gh run watch` polls every 3 s by default (about 1,200 an hour per watcher), so a few agents watching
  CI exhaust it. Use `--interval 60`, no loops over `gh run list`, `gh api rate_limit` to check, webhooks instead of
  polling. Found 27 Nov 2026; in AGENTS.md (e3473b6). Written: `docs/guides/agents-building-software.md`.
- **A server in a container that ignores SIGTERM.** A process running as PID 1 gets no default action for SIGTERM,
  so without its own handler it keeps running until Kubernetes kills it when the grace period ends (30 s by default),
  and every node sees 503s while the new pod waits. The control plane now handles SIGTERM and SIGINT: it stops taking
  requests, closes node links and tool streams with close code 1012 ("service restart") so nodes reconnect at once,
  closes the database (SQLite checkpoints its write-ahead log), and exits within a few seconds. The same applies to any
  service someone writes for their fleet. Tested by `crates/fleetlayer-control/tests/shutdown.rs`. To write: a
  section in the guide on running the control plane on Kubernetes.
- **CI results for a shared branch reach every session on it.** When agents all push to main, a webhook subscription
  to `<repo>@main` hands each agent everyone's results. Solved with commit subscriptions (`fleetlayer subscribe ci
  --commit` after pushing): GitHub's workflow runs match `<repo>@<sha>` too, the control plane tracks each commit's runs
  (from their `requested` events) and ends the subscription once none is pending, and results that finished before
  the agent subscribed are sent at once. Footguns: GitHub's workflow run payload has no job list (the message names
  the workflow; the agent runs `gh run view --log-failed`), and a workflow chained by `on: workflow_run` starts after
  the others finished. Written: `docs/guides/ci-results.md`.
- **CircleCI results the same way.** CircleCI's webhooks (`workflow-completed`, `job-completed`) have their own payload:
  project slug, branch, `pipeline.vcs.revision` for the commit, and a signature in `circleci-signature` (`v1=<hex
  HMAC-SHA256>`). Not parsed yet: add it to the control plane's hooks once the GitHub path has run for a while, mapping
  to the same `<repo>@<branch>` and `<repo>@<sha>` subjects so `--commit` works unchanged.
- **Agents elsewhere reading your Mac's screenshots and downloads.** `fleetlayer-folders`, a read-only MCP server shared
  as a node tool (`fleetlayer mcp folders@<mac>`). Footguns: macOS privacy protection (TCC) keeps the node's
  LaunchAgent, and every tool it starts, out of ~/Downloads, ~/Desktop and ~/Documents until its person allows the node
  once under Files and Folders (or Full Disk Access); a folder of your own (~/Screenshots, set in Shift-Command-5 >
  Options) needs nothing. Confirmed on a real Mac (27 Nov 2026): the prompt names fleetlayer-node by its real versioned
  path (the responsible process launchd started, not the tool), the tool call blocks until it's answered, and the grant
  is stored keyed by that path (unsigned binary), so each upgrade to a new versioned folder prompts again. A stable real
  path for the node, replaced in place, or a code-signing identity would keep the grant. iCloud/Dropbox placeholders are
  refused rather than downloaded. A reply is one WebSocket message (16 MiB limit), so reads are capped at 8-10 MB and
  images downscaled to 1568 px. Downloads holds strangers' files and private documents: decode images with limits, and
  share it as its own tool when it needs stricter access than screenshots. Writing (`--upload`, `--delete` per folder):
  the policy decides per tool entry, not per MCP tool, so per-agent write rights mean sharing the folder twice (a read
  entry for many, a manage entry for a named session). Uploads are linked into place without clobbering, never
  executable, quarantined on macOS, refused under 1 GB free; deletes go to a hidden trash purged after 30 days, judged
  by the time in the trashed name (a rename keeps the old modified time). Written: `docs/guides/folders.md`,
  `docs/plugins/folders.md`.
- **A plugin that runs but can't do its job looked healthy.** A session manager whose program isn't on the service's
  PATH (herdr in `~/.local/bin`, which launchd doesn't expand) answered every `session/list` with an error, yet
  `fleetlayer status` said "running" and the heartbeat stayed green. Since then a failing `session/list` shows the
  plugin as `failing` with the reason, turns the node's health red until a list works again (retried every 30
  seconds), and herdr's error says to give the plugin `config: { program: <full path> }`. Where it lives:
  `crates/fleetlayer-node/src/plugins.rs`, `plugins/herdr/src/herdr.rs`.
- **herdr can't start an agent with a multi-line first prompt.** `herdr tab create --agent claude -- … "<prompt>"`
  breaks when the prompt has newlines. Start the agent with no prompt, wait for it to take input (`--agent` waits),
  then type the prompt with `herdr agent prompt <pane> "<text>"`, which keeps newlines in one turn. Fleetlayer's herdr
  plugin does it this way, so `fleetlayer session start --prompt` takes any text (a prompt from another node always has
  several lines: the sender's header, then the text). Where it lives: `plugins/herdr/src/main.rs` (`SESSION_OPEN`),
  test `the_first_prompt_never_goes_on_the_command_line`.
- **Choosing a Claude account by its week alone keeps choosing one that's at its 5-hour limit.** The "soonest weekly
  reset" rule picked the same account for every summary while its 5-hour window was full, and each call failed with
  "You've hit your 5-hour limit". Skip an account whose 5-hour window is full until it resets, and remember a usage
  limit answer so the next call goes elsewhere. Any program that picks accounts for headless calls needs both. Where it
  lives: `plugins/summaries/src/summaries/accounts.rs`, `docs/plugins/summaries.md`.
