# Guides to write

Every setup step, footgun or design reason others will hit goes here in the same change: what it is, how it was
solved, and where the knowledge lives now. These become Fleetlayer's guides.

- **A GitHub organization can't be created from the CLI or the API on github.com.** `POST /admin/organizations`
  exists only on GitHub Enterprise Server (it answers 404 on github.com, whatever the token's scopes). An agent can
  create repos with `gh repo create <org>/<name>`, but a person makes the organization once in the web UI
  (github.com/account/organizations/new). Where it matters: any guide that has an agent set up a new project's org.
- **On macOS the node must run as a per-user LaunchAgent.** Writing Claude Code's keychain item needs the login
  keychain unlocked: a LaunchDaemon, or an SSH session on a Mac whose keychain is locked, fails with "User
  interaction is not allowed". Where it lives: `docs/plugins/claude-auth-manager.md`.
- **Driving your own signed-in browser from an agent on another machine.** Playwright MCP's extension mode, with its
  server on the laptop (the extension only talks to loopback). Interim transport: the laptop dials out over SSH with
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
  lives: `crates/fleetlayer-control/src/acl.rs`.
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
