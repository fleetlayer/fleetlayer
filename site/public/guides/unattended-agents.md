# Agents that run while you're away

You leave a Claude Code session working on a server overnight. In the morning it has done ten minutes of work and then
spent seven hours waiting on a question nobody was there to answer. Bypass mode was on, so you didn't expect any
questions at all.

Bypass mode skips most prompts. A few still get through, and a session nobody watches waits on them forever. This guide
covers the ones the author has hit running a fleet of unattended sessions, and how to get past each one:

- **Messages from other sessions** are held for approval in bypass mode, until you change one setting.
- **Deleting a path built from a variable** asks even in bypass mode. An undocumented switch lifts it, and a hook of
  your own takes over the check.
- **A node started by launchd can't find `claude`**, because launchd's PATH is short.
- **Sessions lose each other's names after a restart**, so they need a name that stays put.

Everything about Claude Code here was tested on Claude Code 2.1.332 on Linux. Two of the fixes rely on behaviour
Anthropic doesn't document, and a later version can change or remove it. Check again after you upgrade.

## 1. Run in bypass mode

Start each session with `claude --dangerously-skip-permissions`, or set it as the default in `~/.claude/settings.json`:

```json
{
  "permissions": {
    "defaultMode": "bypassPermissions"
  }
}
```

Bypass mode lets the agent run any tool without asking. Use it only for agents you'd trust with your user account, on
a machine where that's acceptable. An agent that reads strangers' text (email, issues, web pages) needs more than
this: see [Run an agent that reads strangers' text](https://fleetlayer.ai/guides/untrusted-input.md).

## 2. Let messages from other sessions through

Fleetlayer delivers a message to a Claude Code session through Claude Code's own messaging socket, so the agent sees
who wrote and nothing is typed into your terminal. In bypass mode, Claude Code holds every message that arrives this way
until you approve it in the session's tab. An unattended session never sees them.

To deliver them straight away, add this to `~/.claude/settings.json` on the machine that runs the sessions:

```json
{
  "crossSessionInbound": "accept"
}
```

Know what you're accepting. With this setting, Claude Code also accepts unattested messages from any process of your
user that can read the session's key file (it sits beside the session's registration in `~/.claude/sessions/`). On a
machine where everything running as your user is yours, that changes little. Where other programs run as your user and
you'd rather they couldn't put words in an agent's mouth, leave it off and approve messages by hand, or move those
programs to a user of their own. The choice is yours.

## 3. The prompt bypass mode can't skip

Claude Code checks Bash commands for one shape on its own: `rm` or `rmdir` on a path built from a variable that might be
empty. That includes variables inside command substitutions and variables derived from other ones. The prompt reads
"Dangerous rm operation on possibly-empty variable path". It asks even under `--dangerously-skip-permissions`, and it
"cannot be auto-allowed by permission rules".

The check has a good reason: `rm -rf "$DIR/"*` with `DIR` empty deletes far more than anyone meant. It also catches
plenty of safe commands. In the author's fleet, a worker agent sat all night on `rm -f $C/$ID.json`, where `$C` came
from a sourced file, inside a forked subagent.

There are two parts to the fix. A switch turns Claude Code's check off, and a hook of yours puts a check back. Do both,
or neither: the switch alone means nothing checks deletes at all.

### The switch

`CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1` lifts these prompts. It isn't documented anywhere; it was found
by reading the Claude Code binary. It takes effect only when all of these hold:

- The session runs in bypass mode (`--dangerously-skip-permissions`, or `defaultMode: bypassPermissions`).
- The session isn't a child session (Claude Code marks those with `CLAUDE_CODE_CHILD_SESSION`).
- No `ask` or `deny` permission rule touches removals.
- No `PermissionRequest` hook is configured.
- No managed settings are in force.

When one of them fails, Claude Code logs that the variable "has no effect" and keeps asking.

Set it in the real environment of the process that starts `claude`, which for unattended sessions is usually the
session manager's service unit. For a systemd user service:

```ini
[Service]
Environment=CLAUDE_CODE_EVEN_MORE_DANGEROUSLY_SKIP_PERMISSIONS=1
```

Setting it in the `env` block of `settings.json` doesn't work: Claude Code drops it from the environment it gives
children. Restart every session after the change, then check that a running session really has it:

```sh
tr '\0' '\n' < /proc/<pid>/environ | grep EVEN_MORE
```

### The guard hook

With the switch on, a `PreToolUse` hook on Bash takes over the check. It reads the command Claude Code is about to run,
and when it refuses, it exits with status 2 and writes the reason to stderr. Claude Code shows that reason to the agent,
which can retry with a fixed command.

The rules that work in the author's setup refuse the shapes that go wrong without anyone meaning it:

- A path that comes from a command substitution (`rm -rf "$(pwd)/build"`).
- An unquoted variable (`rm -f $C/x`).
- A variable that may be empty. A variable counts as safe when it's a `${NAME:?}` guard, when it's set to a literal
  earlier in the same command, or when it's `HOME` or `TMPDIR`. `${C:-/tmp}` isn't a guard: it hides the empty case
  instead of stopping on it.
- A relative path after a `cd` the hook can't follow.
- `xargs rm -r`, which takes its paths from another command.
- A floor nobody means: `/`, the home directory, any top-level directory, or a glob of one (`/*`, `~/*`).
- Over `ssh`, any variable in a recursive delete (it expands on the other host), and the same floor.

Every other literal or guarded path runs. Don't go further and confine recursive deletes to the session's own folder
and `/tmp`. The author tried that, and it refused a checked literal path to a build folder in another repository that
the person wanted gone.

Two rules matter more than the rest:

- **The hook must be at least as strict as Claude Code's check** on the same command shapes. With the switch on, the
  hook is the only check, so a looser hook lets through deletes Claude Code would have stopped. Without the switch (an
  upgrade that removed it, or a condition above that stopped holding), the cases the hook lets through fall back to
  Claude Code's prompt, and the session hangs again.
- **The hook answers fast and never asks.** A hook that waits for input is the same problem in a new place. The author's
  takes about 18 ms a call, run with `python3 -I` and only the standard library.

Make the refusal say how to fix the command, for example: Rewrite it with a guard on every variable, e.g.
`"${C:?}"/"${ID:?}".json`, or use a literal path. The agent then retries at once instead of giving up.

Here is a skeleton to start from. It covers command substitutions, unquoted variables, unguarded variables, `xargs rm`
and the floor. It doesn't follow `cd` or look inside `ssh` commands, and it splits commands with regular expressions
rather than a shell parser. Treat it as a starting point and test it against the commands your agents really run before
you rely on it.

```python
#!/usr/bin/env python3
# A starting point, not a finished guard: test it against the commands your agents really run.
import json
import re
import sys

ALWAYS_SET = {"HOME", "TMPDIR"}
VAR = re.compile(r"\$(?:\{([A-Za-z_]\w*)([^}]*)\}|([A-Za-z_]\w*))")
RM = re.compile(r"(?:^|[\s;&|(])(rm|rmdir)\s+([^;&|\n]*)")
FLOOR = re.compile(r"""^["']?(/|~|\$HOME|\$\{HOME\}|/[^/\s"']+)/?\*?["']?$""")
FIX = 'Rewrite it with a guard on every variable, e.g. "${C:?}"/"${ID:?}".json, or use a literal path.'


def refuse(why):
    print(f"Refused by the rm guard: {why} {FIX}", file=sys.stderr)
    sys.exit(2)


def set_earlier(cmd, name, before):
    return re.search(rf"(?:^|[\s;&]){name}=[^\s;&|$`]+", cmd[:before]) is not None


def in_double_quotes(text, pos):
    return text[:pos].count('"') % 2 == 1


def check(cmd):
    if re.search(r"\bxargs\b[^;&|]*\brm\b", cmd):
        refuse("xargs rm takes its paths from another command.")
    for m in RM.finditer(cmd):
        args = m.group(2)
        if "$(" in args or "`" in args:
            refuse("a path comes from a command substitution.")
        for word in args.split():
            if not word.startswith("-") and FLOOR.match(word):
                refuse(f"{word} is a top-level or home directory.")
        for v in VAR.finditer(args):
            name = v.group(1) or v.group(3)
            if not in_double_quotes(args, v.start()):
                refuse(f"${name} is unquoted.")
            guarded = v.group(1) and v.group(2).startswith(":?")
            if not (guarded or name in ALWAYS_SET or set_earlier(cmd, name, m.start())):
                refuse(f"${name} may be empty.")


event = json.load(sys.stdin)
if event.get("tool_name") == "Bash":
    check(event.get("tool_input", {}).get("command", ""))
sys.exit(0)
```

Save it as `~/.claude/hooks/rm-guard.py` and register it in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "python3 -I ~/.claude/hooks/rm-guard.py" }]
      }
    ]
  }
}
```

It refuses `rm -f $C/$ID.json` and `rm -f "$C/$ID.json"`, and lets `rm -f "${C:?}/${ID:?}.json"` and
`D=/tmp/x; rm -rf "$D"` run.

## 4. Give the node a PATH that has `claude`

On macOS, `fleetlayer-node install` runs the node as a launchd agent. launchd gives its agents a short PATH without
`~/.local/bin`, which is where Claude Code's installer puts `claude`. Anything the node runs that calls `claude` then
fails with "No such file or directory".

`fleetlayer-node install` writes the PATH of the shell you run it from into the service file (the launchd agent on
macOS, the systemd user service on Linux). Run it from a shell where `which claude` finds Claude Code, and run it again
whenever you move `claude` or change your PATH:

```sh
which claude              # should print a path, such as ~/.local/bin/claude
fleetlayer-node install
```

The claude-auth-manager plugin looks further by itself: on PATH, then in the installers' usual places
(`~/.local/bin`, `~/.claude/local`, and Homebrew's `/opt/homebrew/bin` and `/usr/local/bin`). You can also name the
binary in its config:

```yaml
plugins:
  - name: claude-auth-manager
    command: fleetlayer-claude-auth-manager
    config:
      claude: ~/.local/bin/claude
```

A plugin or script of your own that runs `claude` needs the same care.

## 5. Address sessions by a name that survives a restart

Claude Code gives each session a random name for its own messaging between sessions, such as `<project>-3d`. Every name
changes when the machine restarts, so agents that wrote down each other's names can't find each other in the morning.

Address sessions by their session manager's tab name instead, which you choose and which stays the same. Fleetlayer
names every session `<tab>@<node>`:

```sh
fleetlayer fleet                                         # every node and session, from the control plane
fleetlayer msg send --to "planning@server" "the migration is done"
```

`--to` also takes a bare `<tab>` for a session on the same node, or a session ID. Sent from inside an agent session,
the message names the sending session, so the receiver knows who wrote and how to reply. A send to a session that isn't
running fails at once and isn't kept for later, so an agent learns straight away that it needs another plan.

Put the tab names your agents use in your project's instructions (an `AGENTS.md`, for example), so a session started
after a restart knows them too.

## When it doesn't work

- **A session still waits on a message from another session:** `crossSessionInbound` is missing from the settings file
  on the machine that runs the session, or the session started before you added it.
- **The "possibly-empty variable" prompt still appears:** one of the switch's conditions failed. Look for the "has no
  effect" line in Claude Code's log, check `/proc/<pid>/environ` for the variable, and check that the prompt didn't
  come from a child session. Then restart the session.
- **The agent keeps retrying a delete the hook refuses:** the refusal doesn't say how to fix the command. Make the
  message name the rewrite it should use.
- **The hook lets through a delete Claude Code would have asked about:** it's looser than Claude Code's check. Add the
  shape to the hook. Without the switch, that same gap shows up as a hang instead.
- **The node's plugins fail with "No such file or directory":** the service's PATH doesn't include `claude`. Run
  `fleetlayer-node install` again from a shell that finds it.
- **`fleetlayer msg send` says the session isn't running:** check the name with `fleetlayer fleet`. The tab may have
  been renamed, or the session may have ended.
