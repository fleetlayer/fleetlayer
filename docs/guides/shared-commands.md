# Run approved commands on another machine

Some commands only work on one machine. A deploy tool that asks for a touch of the security key plugged into your
laptop, a CLI signed in as you, a script that shows a dialog on your desktop. Your agents run in a VM or on a server,
where none of that exists. You don't want to give them a shell on your laptop either.

This guide lets the laptop's node run a few commands you declare, each with typed parameters, for the agents you
pick. An agent elsewhere asks with `fleetlayer run deploy@laptop --app web`. The laptop checks every argument against
its type, the access policy decides (and can put each run in front of you first), and only then does the command run,
as you, on your laptop, with its output streaming back to the agent. Nothing else can be run that way: no shell, no
other program, no arguments you didn't allow.

## How it works

- **The laptop's node declares commands** in its `node.yaml`: a name, the program and its arguments as a list, and a
  type for each parameter (a choice from a list, short text, a value matching a pattern, or a file).
- **An agent lists and runs them:** `fleetlayer commands` says which ones it may run and what each takes;
  `fleetlayer run <name>@<node> --<param> <value> …` runs one. A file parameter takes a path on the agent's machine:
  the CLI sends the file's bytes and SHA-256.
- **The laptop checks the arguments first.** Anything undeclared, missing or off its type is refused. It then asks the
  control plane about this exact run: the verb `run` on `command:<name>@<node>`, decided by your
  [access policy](../access-control.md), recorded in the audit log with the program and arguments (files by name, size
  and hash, never their contents), and held for your approval when an `ask:` rule matches. By default every run asks.
- **Then it runs**, by direct exec (an argument list, never a shell, so a value can't become a second command). A file
  is written to a new private folder the laptop chooses and removed after. Output streams back line by line, with any
  secret delivered to the laptop's node redacted. The agent can stop it with Ctrl-C; the timeout stops it too.

The laptop has to be awake and online, and the command runs as the laptop node's Unix user: you.

## 1. Declare the commands

Add a `commands:` section to `~/.config/fleetlayer/node.yaml` on the machine that runs them. The worked example is
[Rack Gateway](https://github.com/DocSpring/rack-gateway), which runs commands on Convox racks and approves deploys
with a security key touch and its own review dialog. Two commands: run a script on a rack, and approve a deploy.

```yaml
commands:
  - name: rack-gateway-run
    description: Run a Ruby script in an app's web service on a rack (rails runner; a key touch per run)
    run: [/opt/homebrew/bin/rack-gateway, run, --rack, "{rack}", --app, "{app}",
          --script, "{script}", --justification, "{justification}", web, rails, runner]
    params:
      rack: { type: choice, values: [staging, production] }
      app: { type: choice, values: [web-app, api] }
      script: { type: file, extension: .rb, max_bytes: 65536, description: "the script to run" }
      justification: { type: text, max_length: 300, description: "why, for the rack's audit log" }
    who: ["*@agents"]                 # which sessions may run it: <tab>@<node> patterns. Nobody when empty.
    timeout: 30m

  - name: rack-gateway-approve
    description: Approve a deploy of one commit (a key touch)
    run: [/opt/homebrew/bin/rack-gateway, deploy-approval, approve,
          --rack, "{rack}", --app, "{app}", --commit, "{commit}"]
    params:
      rack: { type: choice, values: [staging, production] }
      app: { type: choice, values: [web-app, api] }
      commit: { type: pattern, pattern: "[0-9a-f]{40}", description: "the full commit SHA" }
    who: ["release@agents"]
    timeout: 10m
```

Restart the node (`fleetlayer-node install` again, or restart its service) and check `fleetlayer status`. A command
with a mistake in it stops the node at start with the reason.

What each setting does:

| Setting | |
|---|---|
| `name` | 1 to 64 letters, digits, `-` and `_`. Agents run it as `<name>@<node>`. |
| `description` | One line agents read in `fleetlayer commands`. |
| `run` | The program and its arguments, one list item per argument. `{param}` is replaced by that parameter's value, inside an item too (`--app={app}`); it never splits into more arguments. `{{` and `}}` are literal braces. The first item is fixed: no parameter in the program. Give the program's full path: launchd's `PATH` on a Mac is short. |
| `params` | Each parameter used in `run`, with its type (below). Every one must be used, and `why` and `help` are taken by the CLI. |
| `who` | `<tab>@<node>` patterns that may run it. Nobody when it's left out. The access policy decides as well. |
| `timeout` | `90s`, `30m`, `2h` (default `1h`, at most `24h`). It's then stopped: SIGTERM to its whole process group, SIGKILL five seconds later. |
| `env` | Names of the node's environment variables the program also gets. It starts with only `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TZ` and the locale, plus `FLEETLAYER_CALLER` (`<tab>@<node>` of the caller). Values never come from the caller. |
| `cwd` | The folder it runs in. Default: the run's own private folder. |
| `max_output` | The most output sent back, in bytes (default 4 MiB). Past it, the agent gets a note and the program runs on. |
| `approval_wait` | How long a run waits for your approval before the agent is told to try again later (default `10m`, at most `1h`). |

Parameter types:

| Type | Takes | Settings |
|---|---|---|
| `choice` | one of its `values`, exactly | `values` |
| `text` | plain text: no control characters (newlines included), nothing that hides itself, not starting with `-` | `max_length` (default 200, at most 4096), `allow_dash` |
| `pattern` | text the regular expression matches whole (it's anchored for you) | `pattern`, `max_length` |
| `file` | a file from the caller's machine, written to a new folder and given to the program as its path | `max_bytes` (default 1 MiB, at most 16 MiB), `extension` |

Any parameter but a file can have a `default`; the others must be given. Text can't start with `-` unless you set
`allow_dash: true`, because many programs read such a value as an option of their own: a justification of
`--rack=production` would otherwise move the run to another rack.

## 2. Let the agents run them

Two things must both allow a run: the command's own `who:` list, and the access policy. With no policy of your own,
the built-in one lets agents in the tenant run a command and asks you before every run. Your policy can be narrower:

```yaml
version: 2
default: deny
allow:
  - { who: "release@agents", can: run, to: "command:rack-gateway-approve@laptop" }
  - { who: "*@agents", can: run, to: "command:rack-gateway-run@laptop" }
ask:
  - { who: "*", can: run, to: "command:*", for: once }   # each run, with exactly the arguments you saw
```

An approval request shows you the program and the arguments it will run, with each file by name, size and SHA-256:

```
rack-gateway-run on laptop for ci@agents:
/opt/homebrew/bin/rack-gateway run --rack staging --app web-app --script '<file fix.rb (812 bytes, sha256 9f2c…)>' --justification 'backfill the missing invoice rows' web rails runner
```

`for: once` approves exactly that run; anything else asks again. A longer hold (`for: 1h`) approves every run of
that command for an hour whatever its arguments, and `acl apply` warns about it. For Rack Gateway, the key touch and
its own dialog are a second approval on the same machine, so you may choose `ask: []` for these commands and approve
on the laptop alone. That's your call: the Fleetlayer approval is what you can answer from your phone.

## 3. Run one

On the agents' machine:

```sh
fleetlayer commands laptop
fleetlayer run rack-gateway-run@laptop --rack staging --app web-app --script fix.rb \
  --justification "backfill the missing invoice rows"
```

`fleetlayer commands` lists each command the session may run, what each parameter takes and whether each run asks
you. `fleetlayer run` prints the program's output as it arrives (its stdout to stdout, its stderr to stderr) and
leaves with the program's exit code. Its own codes: 125 when the run was refused or couldn't start, 124 when it was
stopped at its timeout, 130 when it was cancelled. While it waits for your approval it says so, with the request ID;
if you don't answer within `approval_wait`, it's refused, and the agent runs it again once you have.

## 4. What it guards against

- **Injection:** the program is fixed, arguments go to it as a list (no shell), and a parameter's value is always one
  argument, however it's written.
- **Option smuggling:** text can't start with `-` unless you allow it; a choice or a pattern takes only what you wrote.
- **Paths:** a file parameter is never a path the agent chooses. The laptop writes the bytes it received to
  `~/.local/state/fleetlayer/commands/<run>/<sha256><extension>`: a new folder (0700) and a new file (0600), never
  through a link, read back and checked against the hash before the program starts, and removed when the run ends.
  The file's name from the agent is only checked against `extension` and shown to you.
- **Size:** each file has its `max_bytes` (the CLI refuses a bigger one before sending it, and the laptop refuses it
  again); the whole request has a limit from those; output stops being sent past `max_output`.
- **What you approve is what runs:** the approval and the audit entry are bound to a hash of the arguments and the
  files' hashes, and the laptop runs exactly what it checked.
- **What it can't:** the program runs as you. Anything it does (a script on a rack, a deploy) is as powerful as the
  program and its arguments allow, so declare the narrowest commands that do the job, and keep `who:` to the agents
  that need each one. Other processes of the laptop's own user can read the run's folder while it runs: they're you.

## 5. Monitoring

- **The node's health** fails (and its heartbeat turns red) when a command's program isn't there or can't be run: an
  upgrade moved it, or it isn't on the node's `PATH`. That catches the command that would fail every run before
  anyone needs it.
- **A `command.runs` reading** per command (`fleetlayer readings command.runs`): runs now, runs, refusals and failures
  since the node started, and the last run's caller, time and exit code.
- **The audit log** on the control plane has every decision, with the program and arguments of each run.

## Other uses

The same pattern fits anything that must run on one machine: `brew upgrade` on the laptop, a script that needs the
desktop session, a CLI only you are signed in to. Declare it with the narrowest parameters that do the job.
