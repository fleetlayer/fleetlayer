# Access control: running agents with least privilege

Status (26 November 2026): version 2's first phase is built: the Cedar engine and the policy file (groups, deny,
only, allow, `except`, default), `acl apply`, `explain`, `test`, `show`, `log` and `unapply`, read rules on the fleet
view, summaries and subscriptions, untrusted marking, rate limits, audit rules, and a check that every request the
control plane serves is classified; `acl suggest`; per-tool rules for shared MCP servers. The tamper-evident audit log
(HMAC chain, optional anchors, counted repeats, `audit verify` and `audit tail`). Approvals, on by default (`approvals list|approve|deny`, `GET /v1/approvals`, `POST /v1/approvals/<id>`
with an admin token). Justifications (`justify:` rules; `--why` on `fleetlayer msg send` and `fleetlayer mcp`, or
`FLEETLAYER_WHY`). All of version 2's design is built. Without a version 2 policy, version 1's ordered rules still decide.

Agents act on what they read. An agent that reads email, issue trackers, web pages or public submissions can be
steered by whoever wrote them (prompt injection), and every other agent it can reach inherits that risk. Fleetlayer's
access control decides, for every agent on every machine:

- whom it may **message**;
- which **tools** another machine shares it may use (a browser signed in as you, a database shell);
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
| A sync bundle | `sync:<bundle>@<node>` | `sync:files/skills@laptop` |
| A channel | `channel:<name>` | `channel:ci` |
| A group | `group:<name>` | `group:untrusted-input` |

`*` matches any run of characters. A node is one machine, or one Unix user on a shared machine: the node is the trust
boundary, because every process of that user can claim to be any of its sessions. Give an agent you trust less its
own Unix user and its own node.

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

Verbs: `message`, `use_tool`, `sync`, `read` (the fleet view, summaries, catch-ups; a node that may read nothing of
another, not even `-@<node>`, doesn't see that node in the fleet view at all), `subscribe`, `publish`, and
`start` (start, resume, close or interrupt a session on another node), and `attach` (type into a session's
terminal; watching it is `read`).

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
justify:                       # the agent must say why, in one line, and it goes in the audit log
  - who: "*"
    can: [use_tool, sync, start]
```

Approvals are on by default: a policy that leaves `ask:` out asks before `start` and `attach` (an approval holds 10
minutes) and
before each agent's first `use_tool` or `sync` to a target (it holds 30 days), the safe start for a fleet you don't know
yet. A fleet of autonomous agents turns it off with `ask: []` (and
`justify: []`). An `allow` rule doesn't skip an ask: to stop asking for something, list `ask:` without it (`ask: []`
turns asking off). An `ask` match refuses the request with a request ID and publishes an `access.ask` event on the `access` channel (for
an Inbox card, a phone push, or whoever subscribes); once the person approves it, the agent tries again and the same
agent, verb and target pass without asking again for `for`. A refusal stands for an hour. A justification is one line of plain text, 16 to 1000 characters, with no
hidden characters; a request that needs one and has none is refused with a message saying so. Agents give it with
`--why "…"` on `fleetlayer msg send` and `fleetlayer mcp`, or in `FLEETLAYER_WHY` (handy in an MCP server's
configuration), and it's recorded with the decision in the audit log. There are no `justify:` rules by default.

The list of verbs that can change things is exhaustive in the code, and a test checks every request the control plane
handles maps to a verb: something unlisted fails closed.

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

## Commands

| Command | What it does |
|---|---|
| `acl apply <file> [--check]` | Make the stored policy match the file; `--check` prints the difference without applying |
| `acl explain --from <who> --can <verb> --to <what>` | The decision, and the rules that made it |
| `acl test <tests.yaml>` | Check expected decisions (`{from, can, to, expect: allow\|deny}`) against a policy file, offline; exits non-zero on a mismatch |
| `acl suggest [--days 7]` | The smallest `only` rules that allow what each agent actually did, from the message log |
| `acl log [--limit 30]` | Recent refusals, and what audit rules would have refused |

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
