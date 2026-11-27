# Access control: running agents with least privilege

Status (26 November 2026): version 2's first phase is built: the Cedar engine and the policy file (groups, deny,
only, allow, `except`, default), `acl apply`, `explain`, `test`, `show`, `log` and `unapply`, read rules on the fleet
view, summaries and subscriptions, untrusted marking, rate limits, audit rules, and a check that every request the
control plane serves is classified; `acl suggest`; per-tool rules for shared MCP servers. The tamper-evident audit log
(HMAC chain, optional anchors, counted repeats, `audit verify` and `audit tail`). Approvals, on by default (`approvals list|approve|deny`, `GET /v1/approvals`, `POST /v1/approvals/<id>`
with an admin token). Justifications (`justify:` rules; `--why` on `fleetlayer msg send` and `fleetlayer mcp`, or
`FLEETLAYER_WHY`). Grants with an expiry (`until:`) and roles that are off until a window is approved (the admin
window, 28 November 2026). All of version 2's design is built. Without a version 2 policy, version 1's ordered rules still decide.
From v0.3.13, the MCP gate: a node in front of any MCP server, local or remote, checking each tool call against the
policy (verb `call`), with approvals `for: once` bound to the call's arguments.

Agents act on what they read. An agent that reads email, issue trackers, web pages or public submissions can be
steered by whoever wrote them (prompt injection), and every other agent it can reach inherits that risk. Fleetlayer's
access control decides, for every agent on every machine:

- whom it may **message**;
- which **tools** another machine shares it may use (a browser signed in as you, a database shell), and which of their
  **calls** (send an email, read a calendar), with the server's credentials kept where the agent can't read them;
- which **sync bundles** it may send (secrets, skills, AGENTS.md files that other machines apply);
- what it may **read**: the fleet's sessions, their summaries and catch-ups;
- which channels it may **subscribe** to and **publish** events on.

It's enforced in the control plane, so an agent can't route around it, and the rules live in a file in git that the
agents they limit can't change.

## Principles

1. **Deny wins.** A rule that forbids something can't be undone by a broader rule that allows it, whatever order
   they're in. (Ordered first-match rules make "allow `*@server`" quietly allow `tool:browser@server` too.)
2. **The person decides, the agents report.** Rules come from a file a person commits. An agent that hits a rule gets a
   refusal that says so, every refusal is recorded, and the agent's job is to tell its person, not to work around it.
3. **Least privilege is learned, then enforced.** New rules can run in audit mode first: they record what they would
   refuse without refusing it. `fleetlayer-control acl suggest` writes the smallest rules that allow what agents
   actually did.
4. **Untrusted input stays marked.** A message from an agent in an untrusted group arrives labelled as outside data,
   so the agent that receives it treats it as it would treat the email itself.
5. **Every decision can be explained and tested.** `acl explain` names the rules behind a decision; `acl test` checks
   a file of expected decisions, in CI, before a change ships.

## Who and what

| Kind | Written as | Example |
|---|---|---|
| A session | `<tab>@<node>` | `billing@laptop` |
| A shell or script | `-@<node>` | `-@server` |
| A plugin (a runner that carries out approved actions) | `plugin:<name>@<node>` | `plugin:runner@triage` |
| A shared tool | `tool:<name>@<node>` | `tool:browser@laptop` |
| A sync bundle | `sync:<bundle>@<node>` | `sync:file-sync/skills@laptop` |
| A channel | `channel:<name>` | `channel:ci` |
| A group | `group:<name>` | `group:untrusted-input` |
| A person, a team, the admins | `person:<handle>`, `team:<name>`, `role:admin` | `person:sam` |
| A web service, or someone's data in it | `service:<name>[/<owner>/<path>]` | `service:quietdeck/sam/inbox` |
| An internet host a role may connect to | `host:<name>[:<port>]` | `host:api.github.com`, `host:*.pastebin.com` |
| A page, or an app, in a space | `page:<space>/<path>`, `app:<space>/<name>` | `page:handbook/*` |
| What someone owns | `owned-by:person:<handle>`, `owned-by:team:<name>`; `owner` in `to` | `owned-by:team:support` |
| An agent from outside this tenant | `outside` | |

A session also answers to its session ID (`<id>@<node>`), and a caller may name it either way. Its rules hold under
both names: a request is checked against the rules for its tab name and for its ID, and a refusal under either
refuses. Approvals and reasons are asked once, under the tab name.

`*` matches any run of characters. A node is one machine, or one Unix user on a shared machine: the node is the trust
boundary, because every process of that user can claim to be any of its sessions. Give an agent you trust less its
own Unix user and its own node ([Run an agent that reads strangers' text](guides/untrusted-input.md)).

The same goes the other way: a plugin is only as trusted as the agents on its node. A process running as that Unix
user can read the node's token, link as the node and send requests as any of its sessions or plugins
(`plugin:runner@<node>` included). So a rule that gives a plugin more than its node's agents (a runner allowed to
start sessions elsewhere) holds only if the plugin runs under a different Unix user, with its own node, whose token
the agents can't read. Put a runner of approved actions beside an agent that reads untrusted input on a node of its
own, and name that node in the rule.

A service that isn't on any agent's Unix user (a deck in a container, a bot) sends messages as `service:<name>` with a
`send` token (`fleetlayer-control token add <name> --scope send`, then `POST /v1/messages`). It's a member of the
token's tenant, rules name it like anyone else (`who: service:deck`), and its messages are delivered as from that
service, marked untrusted only if `untrusted:` lists it.

For the same reason, a node always sees, messages and drives its own sessions with no policy check: a message to a
session on the same node is delivered straight to it, and so is `fleetlayer attach`. `message`, `read` and `attach`
are checked when the session is on another node.

A pattern matches people only if it says so (`person:…`, `team:…`, `role:…`, or a group that names them): a bare `*`
in `who` is every agent. People have their own default, `default_people: own`: anything to what they own, read what
their teams own, nothing else until a rule shares it; the tenant's admins also sign in to and administer its
services. Each request is decided in the tenant that owns what it
touches, membership first; between agents, in every tenant either one is a member of. The verbs `sign_in`, `admin`
and `write` are for services and pages, and `admin` and `write` ask first by default. All of it, with
`owners:` and `agents_elsewhere`: [`docs/sso.md`](sso.md).

## The policy file

```yaml
version: 2
default: allow                 # what happens when no rule applies: allow (a fleet of one person) or deny

groups:
  untrusted-input: ["*@triage", "*@scraper"]    # agents that read text strangers wrote
  people: ["*@laptop", "*@server"]

# Forbidden, whatever any other rule says.
deny:
  - who: group:untrusted-input
    can: [use_tool, sync]

# These agents may do these things only to these targets (and reply within 24 hours to whoever wrote to them).
only:
  - who: "*@scraper"
    can: [message]
    to: ["research@laptop", "-@server"]
  - who: group:untrusted-input
    can: [read]
    to: []                     # no fleet view, no summaries, no catch-ups of other sessions

# Allowed when the default is deny (and not forbidden above).
allow:
  - who: group:people
    can: [message, use_tool, read]
    to: ["*"]

# Messages from these agents are delivered as outside data.
untrusted: [group:untrusted-input]

# How much an agent may send.
limits:
  - who: group:untrusted-input
    messages_per_hour: 60
    distinct_recipients_per_hour: 10

# Rules that record what they would refuse, without refusing it. Promote them once the log is quiet.
audit:
  only:
    - who: "*@server"
      can: [message]
      to: ["*@server", "planning@laptop"]
```

Verbs: `message`, `use_tool`, `call` (one tool call through [the MCP gate](#the-mcp-gate)), `sync`, `read` (the fleet view, summaries, catch-ups; a node that may read nothing of
another, not even `-@<node>`, doesn't see that node in the fleet view at all), `subscribe`, `publish`, and
`start` (start, resume or interrupt a session on another node), `close` (close one), `answer` (answer the prompt a
session is blocked on: a question, a plan, or a permission prompt, which approves what it asks), and `attach` (type
into a session's terminal; watching it is `read`), and `connect` (reach an internet host, `host:<name>[:<port>]`, asked by
the [egress plugin](egress.md) for a role's node: `-@<node>`). Give an overseer that answers for the person `answer` by name, never
through a broad rule: `allow: [{ who: "plugin:runner@<node>", can: answer }]` with `answer` denied to everyone else.

## Asking the person: approval and justification

Some actions are fine when the person agrees, and wrong when an agent decides alone: starting a session on another
machine, using a signed-in browser on a bank's site, sending a secret. Two tools, both borrowed from
[Rack Gateway](https://github.com/DocSpring/rack-gateway)'s MFA step-up and command justifications:

```yaml
ask:                           # allowed once the person approves; an approval holds for `for`
  - who: group:agents
    can: [start]
    to: ["*"]
    for: 10m
  - who: "*"
    can: [call]
    to: ["tool:executor/call/tools.*.send*@gate"]
    for: once                  # one call, with exactly the arguments the person saw
justify:                       # the agent must say why, in one line, and it goes in the audit log
  - who: "*"
    can: [use_tool, sync, start]
```

Approvals are on by default: a policy that leaves `ask:` out asks before `start`, `attach`, `answer` and `close` (an
approval holds 10 minutes) and
before each agent's first `use_tool`, `sync`, `admin`, `write` or `connect` to a target (it holds 30 days; one for
`use_tool`, `sync` or `connect` renews while it's used, so it lapses only after 30 days unused), the safe start for a
fleet you don't know yet. That holds with no policy applied too: a tenant with neither a policy nor version 1 rules is
decided by the built-in default (`default: allow` and these asks), which `acl show` prints. A fleet of autonomous agents turns it off with `ask: []` (and
`justify: []`). An `allow` rule doesn't skip an ask: to stop asking for something, list `ask:` without it (`ask: []`
turns asking off). An `ask` match refuses the request with a request ID and publishes an `access.ask` event on the `access` channel (for
an Inbox card, a phone push, or whoever subscribes); once the person approves it, the agent tries again and the same
agent, verb and target pass without asking again for `for`; `for: once` allows one use, and for a tool call through
the MCP gate only with the same arguments. A refusal stands for an hour. A justification is one line of plain text, 16 to 1000 characters, with no
hidden characters; a request that needs one and has none is refused with a message saying so. Agents give it with
`--why "…"` on `fleetlayer msg send` and `fleetlayer mcp`, or in `FLEETLAYER_WHY` (handy in an MCP server's
configuration), and it's recorded with the decision in the audit log. There are no `justify:` rules by default.

The list of verbs that can change things is exhaustive in the code, and a test checks every request the control plane
handles maps to a verb: something unlisted fails closed.

## Grants with an expiry

Any `allow`, `deny` or `only` rule can say when it stops applying, so a grant made for a job ends with the job instead
of living on because nobody remembered to remove it:

```yaml
allow:
  - who: "*@ops"
    can: use_tool
    to: "tool:db@server"
    until: 2026-12-31            # the start of that day, UTC; or a timestamp: 2026-12-31T18:00:00+13:00
    why: the migration to the new database
```

After it, the rule is as if it weren't there. A `justify:` rule can have one too.

## The admin window: a role that's off until you open it

Some work needs an agent that can change everything: install packages, edit system files, rotate production keys. Keep
that agent in a role of its own (a Unix user with its own node) and mark the role `activation: window`. It then has no
rights at all until the person opens a window for it, for a set time:

```yaml
roles:
  admin:
    who: "*@server-admin"        # the role's agents: everything on its node
    activation: window           # off until a window is approved (the default is `always`)
    longest: 1d                  # the longest window the person may approve (default 1d)
    asked_by: "*@laptop"         # who else may ask for a window, or close it early (its own agents always may)
    why: changes the server itself

allow:                           # what the role may do while its window is open
  - who: "*@server-admin"
    can: [use_tool, sync, message]
```

- **Off:** every request by the role's agents, or to them, is refused, whatever the rules say: no messages, no shared
  tools (its own included), no secrets or bundles delivered to it, no sessions started on it from elsewhere, nothing it
  reads (the fleet view, summaries, readings), and its sessions don't show in anyone else's fleet view. The refusal
  says how to ask. This is the part that's enforced: it's decided in the control plane, which the role can't change.
- **Asking:** one of its agents runs `fleetlayer role activate admin --for 1h --why "upgrade postgres to 17"`. That
  files an ordinary approval (verb `activate`, target `window:admin`), so it reaches the person like every other
  approval, with the reason. A request for longer than `longest:` is refused, and so is a request from anyone the role
  doesn't name.
- **Open:** once approved, the role's rules decide as for any agent, and its `ask:` rules don't ask again: the window
  is the approval. Every decision in the window is written to the audit log with the window's ID. Bundles held back
  while it was off are sent at once. `fleetlayer me` and `fleetlayer role list` show when it ends.
- **The end** (its time, `fleetlayer role deactivate admin`, `fleetlayer-control role deactivate admin` or `DELETE
  /v1/windows/admin`): the role is off again at once, and its node closes every session it has, removes everything
  delivered to it (`fleetlayer secrets forget`, [Credentials](credentials.md#using-a-secret-without-seeing-it)), runs its `on_end` hook, and confirms. The node
  keeps the window's end on disk and closes it by itself at that time, linked to the control plane or not, and after a
  restart.
- **Hooks**, in the node's config, run with `FLEETLAYER_WINDOW_ROLE`, `FLEETLAYER_WINDOW_ID`,
  `FLEETLAYER_WINDOW_ENDS_AT` and `FLEETLAYER_WINDOW_STATE` (`open` or `closed`). On the role's own node `on_end` runs
  at the end. A node of a more trusted Unix user can watch the role and run `on_start` and `on_end` both: that's how a
  sudo rule is added for the window and removed after it, since the role's own user must never be able to add it
  itself. Hooks must be safe to run twice. Every node a window concerns confirms its close, the watching ones too, and
  the nodes a window opened on are recorded, so a policy change mid-window (another node, or a shorter `longest:`,
  which ends it at once) still reaches them.

  ```yaml
  windows:
    - role: admin
      on_start: [sudo, -n, /usr/local/sbin/admin-window, open]
      on_end: [sudo, -n, /usr/local/sbin/admin-window, close]
  ```

- **Who approved** is kept with the approval and written in the window's audit entry; a window closed before the
  control plane's loop saw it open is still recorded, audited and closed.
- **Watched:** opening and closing are published as `access.window.opened` and `access.window.closed` on the `access`
  channel. A window whose nodes (the role's, and those watching it) haven't confirmed closing it within five minutes of its end, or that didn't close
  cleanly (a hook failed, a session wouldn't close), shows in the control plane's `/healthz` and in the node's health
  and heartbeat until it's fixed.

Prefer credentials whose lifetime is the window, so the end is real even for a copy the agent kept: AWS STS sessions
(`aws sts assume-role --duration-seconds 3600`, up to the role's maximum session duration), GitHub App installation
tokens (an hour), short-lived database users. A static key the role saw could have been copied: rotate the important
ones on a schedule. The whole setup, sudo rule included: [An admin role that's off until you open it](guides/admin-window.md).

### Approving from a deck or an inbox

Whatever shows the person approvals (a deck, an inbox, a phone app) can decide them too, and show windows: a card for
each request, a card for each open window with its end time, and a Close button.

**Its token.** Give it an `approve` token, never the admin token:

```sh
fleetlayer-control token add deck --scope approve --owner sam
```

An approve token reads approvals and windows, approves and refuses requests, and closes windows. Nothing else: no
fleet view, no events, no policy. It decides as its owner, who must be one of the tenant's admins when it's used (a
token whose owner left or stopped being an admin is refused), and each decision is recorded as `person:sam (token
deck)`, with the approval and in the audit log. Making and removing tokens is recorded in the audit log too.

**A passkey to open a window.** Approving a role window (a request with verb `activate` on `window:<role>`) with an
approve token needs the owner to confirm that one request with their passkey first, at the sign-in service's
`/stepup?tenant=<slug>&approval=<id>&return=<url>` page ([the sign-in service](plugins/signin.md), "Confirming a
request with a passkey"). The sign-in service reports it to the control plane,
which accepts the approval within 5 minutes, once; without it the approval is refused with a 403 that says so. The
control plane enforces this, not the deck. Refusing a window, closing one and every other approval need no passkey.
The admin token needs none either: it's the operator's, and stays on the server.

**The API:**

- **The request** is an ordinary approval with verb `activate` and target `window:<role>`. `GET /v1/approvals` lists it
  (`{"id": "ap_…", "from": "ops@server-admin", "verb": "activate", "to": "window:admin", "status": "pending", …}`), the
  `access.ask` event announces it, and `POST /v1/approvals/<id>` `{"approve": true, "for": "1h"}` (admin or approve
  token) opens the window. `for` can shorten it; the window never runs past the role's `longest:`. The reason the agent
  gave is in the audit log (`asked for admin's window for 1h; why: …`).
- **`GET /v1/windows`** (read, admin or approve token): `{"roles": [{"role", "state", "endsAt"?, "id"?, "longestMs",
  "member"}], "windows": [{"id", "role", "requestedBy", "openedAt", "endsAt", "closedAt", "closedBy", "unconfirmed",
  "problem"}]}`. `state` is `open`, `pending` (a request waiting, `id` is its approval) or `off`. `windows` lists the 50
  most recent, newest first: `closedAt` is null while open, `unconfirmed` lists the nodes (the role's, and those
  watching it) that haven't confirmed closing it, and `problem` says what went wrong if one reported a problem. Times
  are milliseconds since the epoch.
- **`DELETE /v1/windows/<role>`** (admin or approve token): close it now. `{"role", "closed"}`, with `closed` false
  when none was open. A pending request for the role is withdrawn too.
- **Events** on the `access` channel: `access.window.opened` and `access.window.closed`, with subject
  `window:<role>`, body `{"id", "role", "endsAt"}` and dedupe key `<kind>:<id>`. Subscribe to the channel to update a
  card without polling.
- **For the sign-in service** (its own token): `POST /v1/signin/approval_view {tenant, approval}` → the approval
  (`{id, from, verb, to, detail, status, holdMs, createdAt}`, 404 when missing or another tenant's), and `POST
  /v1/signin/stepup {tenant, person, approval}` → `{approval, person, expiresAt}`, 403 with the reason when the person
  isn't one of the tenant's admins or the request isn't a pending role window.

## Roles that never ask

Approvals are on by default, but a role whose work never needs the person (a homelab agent, a CI runner) shouldn't
ask: list `ask:` with that role left out, through `except:`.

```yaml
ask:
  - who: "*"
    except: ["*@homeserver", "*@ci"]    # these roles get on with it
    can: [start, attach, answer, close]
    for: 10m
  - who: "*"
    except: ["*@homeserver", "*@ci"]
    can: [use_tool, sync, admin, write]
    for: 30d
```

Their `deny` and `only` rules still hold; only the asking stops. Keep asking for roles that touch production or
money, and give the rest standing approval.

## The audit log

Every decision (allowed, denied, asked, approved) is written before the action happens, and if it can't be written,
the action is refused: the control plane fails closed. Entries are chained, each with an HMAC over its content and the
previous entry's hash, so any changed, reordered or removed entry before the newest is detected. Optionally, every
hour an anchor (the latest entry's position and hash, and the previous anchor's hash) is written outside the database
(a file on another machine, a git repository, or write-once storage such as S3 Object Lock), which also catches
deleting the newest entries. `fleetlayer-control audit verify` checks the chain and any anchors. Repeats of the
same denial within a minute are written as one entry with a count, so a looping agent can't flood the log. This
follows Rack Gateway's audit log, built for SOC 2.

## Tools at the level of their own tools

A shared tool is often an MCP server with many tools of its own. The machine sharing it can allow or deny them one by
one, since the node starts the server and sees every `tools/call`. A refused call is answered with an MCP error result
that names the rule and never reaches the server; refused tools are left out of `tools/list`; every call is logged on
the sharing node with its caller:

```yaml
- name: browser
  allow: ["*@server"]
  tools:
    deny: [browser_run_code_unsafe]     # Playwright MCP: arbitrary code in the server's process, always on
```

A tool runs in an empty directory of its own unless `cwd:` says otherwise: MCP servers often confine file access
(uploads) to their working directory, and the node's own (`/` under launchd) would expose every file. For Playwright
MCP, also pass `--ignore-roots`, so a caller can't widen that with its own MCP roots.

Decision order: a `deny` match refuses; then an `only` rule for that agent and verb refuses anything outside its list
(a reply within 24 hours to a session that wrote first is always inside); then an `allow` match allows; then the
default. Rate limits apply to whatever is allowed.

## The MCP gate

A node can stand in front of any MCP server an agent uses, a program or a remote server such as
[Executor](guides/executor.md), and check every tool call before the server sees it. The server's credentials stay
with the node that fronts it, under a Unix user of its own, so the agents that use it never hold them and can't go
around the gate.

```yaml
# node.yaml of the gate's own node (its own Unix user)
tools:
  - name: executor
    url: https://executor.example/mcp      # or command: + args: for a program
    bearer_file: ~/.config/executor/token    # read when a connection opens; header_files: for other headers
    allow: ["*@agents", "*@laptop"]
    check_calls: true
    call_targets:                            # argument rules: more of the call's name, from its arguments
      call: ["/calls/*/path"]
```

With `check_calls: true`, the node asks the control plane about each `tools/call` (`link/tool_call`). A call is
named `tool:<tool>/<target>@<node>`: the MCP tool's name, and with `call_targets`, one more part for each value a JSON
pointer picks out of the arguments (`*` for each item of a list, or the value itself when it isn't one). Executor's
`call` with two calls in it is two targets, `tool:executor/call/tools.google.gmail.search@gate` and
`tool:executor/call/tools.google.gmail.send@gate`, and both must be allowed. A call whose arguments lack what a rule
names, or hold something other than a short string there, is refused by the node: it never passes as less than it
is. The caller is the one the control plane recorded when the stream opened, not whatever the node says.

Each target is decided as verb `call` by the same policy file, so per-role rules are the usual ones (a role is a Unix
user with its own node, `*@<node>`):

```yaml
deny:
  - who: "*"
    can: call
    to: "tool:executor/execute@*"             # programs: see below
only:
  - who: "*@triage"
    can: call
    to: ["tool:executor/search@*", "tool:executor/describe@*", "tool:executor/call/tools.google.gmail.search@*"]
ask:
  - who: "*"
    can: call
    to: ["tool:executor/call/tools.*.send*@*", "tool:executor/call/tools.*.create*@*", "tool:executor/call/tools.*.delete*@*"]
    for: once
```

- **Allowed** calls go to the server; **denied** ones are answered by the node with an MCP error result naming the
  rule.
- **Asked** calls wait: the person gets an `access.ask` event (and `fleetlayer-control approvals list` shows the call,
  its arguments scrubbed of delivered secrets and cut short at 4,000 characters), and the node holds the call for up to
  `approval_wait` seconds (120 by default) while it looks at the request every five seconds without writing to the
  audit log. Approved in time, the call goes through and the agent never noticed; otherwise the agent is told the
  request ID and makes the same call again once it's approved. An approval `for: once` is spent by that call and holds
  only for those arguments.
- **Every** decision is in the audit log with its caller and the full target. Arguments are not: the log says who
  called what, the server's own logs say with what.
- A delivered secret in a call's arguments or its result is redacted by the node's scrubber
  ([credentials](credentials.md)) before it goes on, and raises the alert that says to rotate it.
- The node checks a remote server every five minutes with its own credentials (initialize, list its tools), and a
  failure, a revoked token included, fails the node's health and its heartbeat.
- Only one JSON-RPC message per line passes, for every shared MCP server (gated or not, from v0.3.13): a batch or a
  line that isn't JSON is refused, so nothing reaches the server that the checks didn't read.
- Calls run concurrently: one waiting for the person doesn't hold up the others. A call still being checked when the
  agent disconnects is dropped.

**Programs can't be checked call by call.** Executor's `execute` runs a JavaScript program that can reach any path,
built at run time, so its target is `execute` alone. The node lists the paths written out in the program for the
person to read, but binds nothing to them. Choose one: deny `execute` and let agents use `call` (every path checked);
ask for it `for: once` (the person reads the program); or allow it only for a role you'd let call anything. The same
holds for any server with a run-code tool (Playwright's `browser_run_code_unsafe`).

The gate is off unless a tool says `check_calls: true`. Without it, a shared tool works as before: `use_tool` decides
who may connect, and the node's own `tools:` list filters by name.

## Commands

| Command | What it does |
|---|---|
| `acl apply <file> [--check]` | Make the stored policy match the file; `--check` prints the difference without applying |
| `acl explain --from <who> --can <verb> --to <what>` | The decision in a tenant (`--tenant`), and the rules that made it |
| `acl test <tests.yaml>` | Check expected decisions (`{from, can, to, expect: allow\|deny}`, with `role` and `teams` for a person) against a policy file, offline; exits non-zero on a mismatch |
| `acl suggest [--days 7]` | The smallest `only` rules that allow what each agent actually did, from the message log |
| `acl log [--limit 30]` | Recent refusals, and what audit rules would have refused |
| `role list` | Roles with windows, whether each is open, and recent windows (who asked, when it closed, nodes that haven't confirmed) |
| `role activate <role> --for 1h` | Open a role's window now, as the person (no approval request) |
| `role deactivate <role>` | Close it now |

On a server, let the control plane apply the policy itself instead of running `acl apply`: `fleetlayer-control serve
--policy-file <path>` (or `FLEETLAYER_CONTROL_POLICY_FILE`) applies the file at start and again within 10 seconds of
any change, with no restart. A broken file is refused, the stored policy keeps deciding, and `/healthz` says so until
it's fixed. Keep the file in git, where the agents it limits can't change it, and check each change in CI with
`acl apply <file> --check` and `acl test`. Several tenants, and the move from an init container, are in
[the deploy notes](https://github.com/fleetlayer/fleetlayer/blob/main/deploy/README.md#the-access-policy).

## How it's built

The decision engine is [Cedar](https://www.cedarpolicy.com) (Apache-2.0, from AWS, with a formally verified core):
the YAML file compiles to Cedar policies. `deny` and `only` become `forbid` policies, `allow` and the default become
`permit` policies, and Cedar's rule that a `forbid` always beats a `permit` gives principle 1. Group membership is
worked out from the patterns when a request arrives. Cedar names the policies behind each decision, which `acl
explain` shows with the YAML rule each came from. Teams that outgrow the YAML can add raw Cedar policies in a
`cedar:` block of the same file.

Every refusal is published as an `access.denied` event on the `access` channel, and every decision an audit rule would
have refused as `access.audit`.

## Cases other teams will meet

- **An agent that reads untrusted input** (email, tickets, the web, public forms): put it in an untrusted group; deny
  tools and sync; mark its messages untrusted; limit its rate; give it its own Unix user and node.
- **A runner that carries out only approved actions** next to an agent that proposes them: the runner is a plugin
  (`plugin:runner@node`) and gets the reach; the agent gets little.
- **A shared, signed-in browser:** allow `use_tool` on `tool:browser@laptop` to named sessions only, never to an
  untrusted group. The machine sharing it keeps its own allow list too: both must agree.
- **An MCP server holding sign-ins for many services** (Executor, a company API gateway): front it with a node of
  its own Unix user holding the token, `check_calls: true`, deny run-code tools, `ask ... for: once` on what sends or
  deletes, and `only` rules per role ([Executor behind Fleetlayer](guides/executor.md)).
- **Secrets and skills sync:** only the hub that holds a secret may `sync` it. Nothing else may send bundles a
  machine applies, or it could overwrite them.
- **People, tenants and teams** (people signing in to services, a company and a household on one deployment, agents
  admitted to several tenants): `docs/sso.md`.
- **A team sharing one control plane:** each person's machines in a group; people message across groups, agents stay
  inside their person's group unless allowed.
- **Rolling out least privilege on a fleet that already runs:** write the rules under `audit:`, run `acl suggest`,
  watch `acl log` for a week, then promote them.
- **An agent renamed:** rules name tabs, and anyone with the node's user can rename a tab. Put rules on nodes (or
  groups of nodes) for anything that matters; tab rules are for convenience inside a trusted node.
