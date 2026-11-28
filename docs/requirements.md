# Requirements, and uninstalling

What you need before you install Fleetlayer, what works with it today, and how to take it off a machine again.

## What you need

- **macOS or Linux,** on x86-64 or ARM (Apple silicon, a Raspberry Pi 4 or newer with a 64-bit OS, an ARM VPS).
  There's no Windows build, and running it under WSL hasn't been tested.
- **A session manager** for the agents, so Fleetlayer can see, message, start and close their sessions:
  - [herdr](https://herdr.dev): the one Fleetlayer is built and tested with most, the release from herdr.dev
    included (0.9.5 tested);
  - [Rex](rex.md), Superlogical's terminal and session manager;
  - tmux, through [the tmux plugin](plugins/tmux.md) (v0.4.2).

  Fleetlayer works with your session manager: it attaches to it and never takes it over, so your own terminals and apps
  keep working beside it.
- **An outbound connection** from each machine to the control plane: a WebSocket over TLS, or plain on a private
  network. Each node keeps one outbound connection, so it works behind NAT with no port opened on its machine. The
  control plane itself needs one address its nodes can reach: on one laptop that's `localhost`; across machines, a
  private network (Tailscale, a LAN) or a public address behind TLS.
- **`curl` and `tar`** for the installer. Nothing else: each release is a set of single binaries, with no runtime to
  install.

The node is small: it runs comfortably beside the agents on a VM with 2 cores and 4 GB of memory. The agents and
their builds need far more than Fleetlayer does.

## Agents (harnesses) that work with it

| Harness | Today |
|---|---|
| **Claude Code** | Everything: listed with its status, messages through Claude Code's own channel, blocked prompts read from its hooks and answered from anywhere, sessions started, resumed and closed remotely, summaries, and [your own Claude accounts on every machine](guides/claude-accounts.md). |
| **Codex** | Listed with its status, messages typed in by the session manager, sessions started, resumed and closed remotely, summaries, skills installed for it. |
| **pi, OpenCode** | Listed with their status, messages through the agent's own API (shown in its tab), permission prompts and questions seen from anywhere, transcripts, sessions started and resumed remotely. |
| **Gemini CLI, Cursor's CLI** | Listed with their status, messages typed in by the session manager (not yet into Gemini CLI where herdr doesn't recognise it), permission prompts seen from anywhere (Gemini CLI), transcripts, sessions started and resumed remotely. |
| **Aider** | Not yet: an Aider in a session manager's pane can still be typed into by hand. |

What each harness gets, and how, is in [Harnesses](harnesses.md).

Any agent that can run a shell command can also use Fleetlayer itself: `fleetlayer msg send`, `fleetlayer mcp
browser@laptop`, `fleetlayer secrets run`, pages. The table is about what Fleetlayer can see and do *to* a session.

## Installing

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
```

It downloads the latest release for your platform, checks it against the release's `SHA256SUMS`, and puts the
binaries in `~/.local/lib/fleetlayer/` with links in `~/.local/bin`. It needs no root and changes nothing outside
your home folder. What that trusts, and how to check a release yourself first, is in
[Installing and trusting Fleetlayer](trust.md). Then [Getting started](getting-started.md).

## Uninstalling

Fleetlayer keeps everything in a few folders in your home, so taking it off a machine is stopping its services and
removing those folders. Have your agent do it, or follow these steps on each machine.

1. **Take its hooks out of Claude Code** (on a machine with the Claude Code plugin). Set `hooks: false` in the
   plugin's `config:` in `~/.config/fleetlayer/node.yaml` and restart the node once: the plugin removes its hooks from
   `~/.claude/settings.json` and leaves every other setting alone. If you skip this, the leftover hooks do nothing
   (each exits at once when the plugin isn't there), but they stay in the file until you remove the entries whose
   command contains `fleetlayer-claude-code`. The pi, OpenCode, Gemini CLI and Cursor plugins take theirs out with
   `install: false` in their `config:` the same way (otherwise delete `fleetlayer.ts` in `~/.pi/agent/extensions`,
   `fleetlayer.js` in `~/.config/opencode/plugins`, and the hooks whose command contains `fleetlayer-harness` in
   `~/.gemini/settings.json` and `~/.cursor/hooks.json`).

2. **Stop the services.** On the machine you ran `fleetlayer up` on, `fleetlayer down` stops the node, the control
   plane and the gateway and removes their service files. On a machine with only a node, `fleetlayer-node uninstall`
   does the same for the node. Both leave every file in place.

3. **Revoke the machine** (if the control plane carries on without it). On the control plane:
   `fleetlayer-control node remove <name>`. Its token stops working at once.

4. **Remove the files:**

   ```sh
   rm -rf ~/.local/lib/fleetlayer ~/.config/fleetlayer ~/.local/state/fleetlayer ~/.local/share/fleetlayer
   rm -f ~/.local/bin/fleetlayer ~/.local/bin/fleetlayer-*
   ```

   `~/.config/fleetlayer` holds the node's key and token, and the control plane's folder holds its database and audit
   log. Back up anything you want to keep first ([Backups](guides/backups.md)).

5. **Remove the skills it installed.** The node marks each skill folder it writes with a `.fleetlayer` file, so you
   can remove exactly those:

   ```sh
   for d in ~/.claude/skills/* ~/.agents/skills/*; do [ -f "$d/.fleetlayer" ] && rm -rf "$d"; done
   ```

6. **Your own content stays.** Pages and apps your agents wrote are plain folders (`~/Fleetlayer/` after `fleetlayer
   up`); keep or delete them as you like.

On a Mac, also remove the logs (`rm -f ~/Library/Logs/fleetlayer-*.log`), the `fleetlayer-node` rows under System
Settings > Privacy & Security (Files and Folders, Full Disk Access) if you gave it any, and, if you used the cookies
plugin, its access to "Chrome Safe Storage" in Keychain Access.

Related: [Getting started](getting-started.md), [Set up a node that keeps running](guides/node-services.md).
