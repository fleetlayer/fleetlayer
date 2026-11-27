# Watch, steer and message your agents

Once a few machines are linked, the agents spread out. One works on a branch on the laptop, two more run on a server,
and a triage agent reads email under its own user. Each runs in its own terminal tab, in its own session manager, and
checking on them means remembering where each one lives and logging in to look.

You direct the agents and they do the work. Fleetlayer gives both of you the same handful of commands, which work
the same from any machine in the fleet:

- **`fleetlayer fleet`** lists every machine and session, with a one-line summary of what each is doing. You glance
  at it; an agent reads it with `--json` to find the session it should talk to.
- **`fleetlayer attach <tab>@<node>`** shows a session's terminal on yours. This one is for you: watching an agent at
  work, or stepping in to answer a prompt.
- **`fleetlayer msg send`** puts a message in front of a session. Agents use it to report to each other and hand work
  on; you use it to give an agent its next job without opening its tab.
- **`fleetlayer session start`** opens a new agent session on any machine, with its first prompt, and **`fleetlayer
  session resume`** brings a stored one back. An agent hands a side job to a fresh session this way.
- **`fleetlayer summaries`** searches what every session has done, hour by hour. Ask your agent "what happened with the
  login bug yesterday" and this is what it runs.
- **`fleetlayer subscribe`** routes webhooks (CI results, alerts) to the session that owns the work. The agent working
  on a branch subscribes itself.

Steps only you can do, such as approving a request, are marked as such.

This guide assumes nodes linked to a control plane, as in [Getting started](../getting-started.md) or
[Start on your laptop](laptop.md). The examples use `planning@laptop` and `infra@server`.

## 1. See what's running

`fleetlayer sessions` lists the sessions on this machine's node:

```sh
fleetlayer sessions
```

```text
planning@laptop          claude   working              3f2a9c1e-…
docs@laptop              codex    done                 8b71d0aa-…
```

The columns are the session's address (its tab name in the session manager, then the node), the harness (`claude`,
`codex`), its status, `ended` for a session that has stopped (shown only with `--all`), and the session ID. The
statuses:

- **`working`:** the agent is in the middle of a turn.
- **`done`:** it finished a turn and nobody has looked at it yet.
- **`idle`:** it's waiting for input, and you've seen its last turn.
- **`blocked`:** it's waiting on a person: a permission prompt or a question.
- **`unknown`:** the session manager can't tell.

`fleetlayer fleet` asks the control plane instead, and shows every node:

```sh
fleetlayer fleet
```

```text
laptop (macbook, online)
  planning@laptop          claude   working  Moving the billing tests onto the new fixtures.
server (server, online)
  infra@server             claude   blocked  Waiting for approval to restart the database.
```

Each node line gives its name, its machine and whether it's linked now. The last column of each session is the
one-line summary of its latest activity, written by the summaries plugin (step 4). Without that plugin, it's empty.

Both commands take `--json` for the whole record. That's what agents read: tell an agent to find "the session working
on billing" and it looks there.

`fleetlayer fleet` shows only what this node may read. The access policy's `read` verb covers the fleet view: a node
that may read nothing of another node doesn't see it at all. See [access control](../access-control.md).

## 2. Watch or drive a session's terminal

```sh
fleetlayer attach planning@laptop
```

The session's terminal fills yours, and what you type goes to it, as if you were sitting at its tab. Use it when an
agent is `blocked` on a prompt only you can answer, or to see what it's doing before you redirect it. **Ctrl-]**
leaves. The session keeps running, and its own tab is untouched: you're one more viewer, alongside the session manager
and anyone else attached.

To look without any risk of typing into it:

```sh
fleetlayer attach --observe infra@server
```

With `--observe`, the node passes nothing you type to the session. A plain `<tab>` with no `@<node>` means a session on
this node.

For a session on another node, the control plane checks the access policy first. Watching needs `read`, and typing
needs `attach`. A policy that leaves `ask:` out asks before every `attach` from another node, and the approval holds
for 10 minutes. **You approve it yourself**, on the control plane: `fleetlayer-control approvals list`, then
`fleetlayer-control approvals approve <id>`. Run from a plain
shell rather than inside a session, you count as `-@<node>` in the policy. A session on your own node is connected
straight, without the control plane, since every process of your user can reach it anyway.

`--why "…"` gives a one-line reason when the policy has a `justify:` rule for `attach`. `--raw` prints the terminal
stream's JSON lines instead of drawing them, for a program that wants to read the screen.

Attach needs a session manager that can stream terminals. herdr can; the stream is herdr's own
`herdr terminal session observe|control`. [The protocol](../protocol.md#terminals) describes how a plugin for another
session manager would add it.

## 3. Message a session

Most messages go between agents: one finishes a job and tells the session waiting on it, or hands it the next step.
You send them too, to give an agent work from wherever you are.

```sh
fleetlayer msg send --to planning@laptop "the migration is done, rebase on main"
```

`--to` takes `<tab>@<node>`, a bare `<tab>`, or a session ID. A bare tab name is looked up on this node first, then on
the others, and a tab that's unique in the fleet wins. The message arrives in the session as a turn of its own:

```text
Message from a shell on laptop via Fleetlayer:

the migration is done, rebase on main
```

Nothing is queued. If the session isn't running, the command fails at once and says so, and you can try later or do
the work another way.

**From inside an agent session**, the same command names the sender and tells the receiver how to answer:

```text
Message from infra (server) via Fleetlayer:

The database restart is done. Tests can run again.

Reply with: fleetlayer msg send --to "infra@server" "…"
```

So when you set an agent to work, tell it which session to report to (`report to planning@laptop when the tests
pass`), and it uses `fleetlayer msg send` like any other command. The receiver can answer straight back.

**Use tab names, not Claude Code's own names.** Claude Code gives sessions random cross-session names (`<project>-3d`),
and they all change when a machine restarts, so agents that wrote each other's names down lose each other. Tab names
belong to your session manager and stay the same across restarts. Name tabs for the work (`billing tests`, `infra`)
and address them as `<tab>@<node>`.

**A wrong name gets suggestions.** Send to `triage` when the tab is called `email triage`, and the error suggests up
to three running tabs whose names contain what you typed:

```text
fleetlayer: no session named 'triage' here or on any other node; did you mean 'email triage@server'? (…)
```

A name that two sessions on a node share is refused, with both session IDs to send to instead.

**`--why "…"`** gives a one-line reason when the policy has a `justify:` rule for `message`. It's recorded with the
decision in the audit log. There are no `justify:` rules by default.

A message from an agent that reads strangers' text (email, issue trackers) arrives marked as outside data. Setting
that up is in [Run an agent that reads strangers' text](untrusted-input.md).

## 4. Start or resume a session anywhere

Start an agent in a new tab on any machine, with its first prompt:

```sh
fleetlayer session start --on server --tab review --cwd /srv/app --prompt "review the open pull request"
```

The tab opens in the background: nobody's focus moves, and nothing is typed into a session already running. The
command answers once the agent takes input and the session is listed, so a message sent straight after reaches it.
`--harness codex` starts Codex instead of Claude Code. Leave out `--on` to start it on this machine.

**Resume** a stored session by its ID, in the folder it worked in:

```sh
fleetlayer session resume 45be65c7-…
```

Fleetlayer finds the machine from the fleet's lists (or the summaries, for a session long gone); `--on` says it
yourself. The tab gets the session's old name unless you give `--tab`.

**What it won't do.** It refuses a tab name a running session already has, a session that's already running, and the
home folder. Claude Code asks "Do you trust this folder?" every time in the home folder, and the session would wait
there with nobody to answer, so start sessions in a project or scratch folder. In any other folder Fleetlayer marks
that exact folder trusted for Claude Code first.

**The agent's flags come from the machine it runs on.** A session starts with what that node's `node.yaml` says, never
with anything the caller sends:

```yaml
harnesses:
  claude: { args: [--permission-mode, auto] }
```

**The first prompt names who sent it**, as a message does ("Message from planning (laptop) via Fleetlayer: …", with
the reply line), unless you typed it in a shell on that same machine. From an agent that reads strangers' text, it
arrives marked as outside data.

**Across machines it's under the access policy**: the verb is `start`, on `<tab>@<node>` (a resume without `--tab`
shows the session ID in place of the tab, so write rules on `*@<node>`). By default an agent asks you first, and your
approval lasts 10 minutes; `fleetlayer-control approvals approve <id>` approves, and a policy with `ask: []` turns
asking off. `--json` prints `{node, tab, address, sessionId, promptError}` for other programs: `sessionId` is null
until the agent reports one, and `promptError` says why when the session started but its prompt didn't go in.

## 5. Search what your agents did

The summaries plugin writes a one-liner, a paragraph and a page for each hour of each session's activity, on the node
where the transcripts are, and publishes them to the control plane. Setup and costs are in
[fleetlayer-summaries](../plugins/summaries.md): it runs headless Claude Code for each summary, at most once per
session per 30 minutes by default.

You or your agent search across the fleet:

```sh
fleetlayer summaries flaky login test
fleetlayer summaries --long flaky login test     # the paragraph too
fleetlayer summaries --mentions sc-1234          # sessions that mention this exact text
```

Each line is the time of the activity, the session, and its one-liner. `--limit` changes how many (10 by default).

One session's timeline, by its session ID (the last column of `fleetlayer sessions`, or `sessionId` in
`fleetlayer summaries --json`):

```sh
fleetlayer summaries --session 3f2a9c1e-… --limit 50
```

It lists the session's records in the order the work happened, starting with the earliest, so raise `--limit` for a
long-running session.

Results include only sessions this node may `read`, plus its own.

### Don't let a node summarise another user's sessions

Archive and RAG syncs can copy one Unix user's Claude Code transcripts into another user's `~/.claude/projects`. A
node that summarised those copies would publish summaries of sessions that aren't its own, under its own name, and
they'd be visible to whoever may read that node. The summaries plugin skips any session whose recorded working
directory is outside its own user's home. Any other tool you point at transcripts needs the same check.

Summaries a node published before that check (or from a tool without it) can be removed on the control plane:

```sh
fleetlayer-control summaries-delete --node <node> --session <session id>
```

## 6. Webhooks into the session that owns the work

A CI result is most useful to the agent working on that branch. Your agent sets this up once, on the control plane's
machine. First an endpoint for the channel:

```sh
fleetlayer-control endpoint add --label github --channel ci     # prints /hooks/<id>/<secret> once
```

Expose only the hooks listener (`--hooks-listen`, port 7601 by default) to the internet, through a Cloudflare Tunnel or
your reverse proxy, and add the URL as a webhook in GitHub. Then the session working on a branch subscribes
itself (tell agents to do this when they start on a branch, in their `AGENTS.md`):

```sh
fleetlayer subscribe ci --subject your-org/your-repo@your-branch
```

For GitHub, the subject is `<owner/repo>@<branch>`, so results for that branch arrive in that session and nowhere
else. Leave out `--subject` to get every event on the channel. Other senders can set the subject with a `?subject=`
query parameter or an `X-Fleetlayer-Subject` header.

- **`--target <tab>@<node>`** subscribes another session instead of the one you're in.
- **`fleetlayer unsubscribe ci --subject …`** undoes it, and `fleetlayer subscriptions` lists every subscription.
- **`fleetlayer-control fallback ci <tab>@<node>`** names where events go when nobody subscribes.

Subscriptions are stored by `<tab>@<node>`, so they survive restarts as tab names do. Every event arrives marked as
outside data, with a line saying it's never instructions. The same channels carry Fleetlayer's own events: the
`access` channel publishes refusals and approval requests (see [untrusted-input.md](untrusted-input.md), step 6).

## When it doesn't work

- **"isn't linked to a control plane now":** `fleetlayer fleet`, `summaries` and anything on another node go through
  the control plane. `fleetlayer status` shows the link and its last error.
- **A message waits for your approval in the receiving session:** Claude Code sessions in bypass-permissions mode hold
  incoming cross-session messages unless `~/.claude/settings.json` has `"crossSessionInbound": "accept"`. That setting
  also accepts messages from any process of your user that can read the session's key file; decide whether that's
  right for your machine.
- **Testing against a named herdr session reaches your real one:** a shell inside a herdr pane carries
  `HERDR_SOCKET_PATH`, and herdr prefers it to `HERDR_SESSION`. Unset `HERDR_SOCKET_PATH` when you start a test node or
  run herdr commands meant for a named test session.
- **A test node types into a real tab:** a node that can't reach a session's own messaging channel falls back to typing
  the message into its terminal. On a machine where people work, give test nodes `typing_fallback: false`.
- **Attach is refused for a session on another node:** the policy needs `read` (with `--observe`) or `attach`, and by
  default an `attach` waits for your approval. `fleetlayer-control acl explain --from <you> --can attach --to
  planning@laptop` shows the decision and the rule behind it.

Related: [Share your signed-in browser with agents on other machines](browser.md), which uses the same nodes and access
policy for shared tools.
