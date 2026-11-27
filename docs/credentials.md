# Credentials: how Fleetlayer keeps secrets

Agents need credentials to do real work: a Claude sign-in, a GitHub token, cookies for a site, a cloud API key. Each
one is a risk wherever it sits, because anything that can read it can use it. This page says how Fleetlayer handles
them: what works today, checked against the code, then what's still coming, and last what none of it covers.

Sections that start *In the next release* are on the main branch and ship in the release after v0.3.12.

## The principles

- **Off by default.** Most people start with a token in their shell, and Fleetlayer never forces more. It says what's
  at risk in plain words, and one request to an agent sets the rest up: the skills carry the steps, so nobody has to
  learn a secrets system first.
- **Roles, not sessions.** Sessions start, end and get renamed all day, so nothing is configured per session. Access
  belongs to a **role**: a Unix user with its own node, because that's the boundary the machine enforces. A handful of
  roles covers most fleets (everyday work, untrusted input, a trusted runner, infrastructure), and a new session in a
  role has that role's secrets with no setup. Which session used a secret is still recorded in the audit log.
- **The control plane holds no credentials.** Secrets live on machines you control and travel only sealed to the key
  of the machine that needs them.

## How it works today

### The control plane never holds your credentials

The control plane connects your machines, but it never sees the credentials agents use. Claude sign-ins, cookies and
API tokens live only on machines you control. When one machine needs a credential another holds, it travels as a
**sealed bundle**:

- Every node has its own key pair ([age](https://age-encryption.org) X25519). The private key never leaves the node's
  config folder, which only its Unix user can read.
- The sending node encrypts the bundle to each target node's public key before anything leaves the machine. The
  control plane relays ciphertext it can't read, and **never stores it**.
- The sender pins each target's key the first time it seals to it, and refuses a changed key until a person removes
  the old pin. A control plane that lied about a node's key couldn't read what's sent to it after that. The first
  seal trusts the key it's given, so check a new node's key fingerprint (`keyFingerprint` in
  `fleetlayer status --json`) before trusting it with secrets.
- By default, the first bundle from each source to each target waits for a person's approval.
- Status flows back to the sender ("applied", or the error). Content never flows back.

What the control plane does store: hashes of the tokens nodes and webhooks use to reach it, and the signing secret
for each GitHub webhook you give one, which it needs to check GitHub's signature. That signing secret lets someone
forge a webhook delivery to your fleet, and nothing more. The mechanism is in
[sync and sealed secrets](protocol.md#8-sync-and-sealed-secrets).

### One shape for every secret

Claude setup tokens, browser cookies and 1Password items go the same way:

1. **A source** holds the secret: the Claude hub, the browser on your laptop, a password manager.
2. **A grant** says which role gets it, never which session (see the principles above).
3. **Sealed delivery**, as above. Your [access policy](access-control.md) decides which nodes may receive which
   bundles (`sync`).
4. **A writer** on the target puts it where tools look: an env file, a file, a keychain item. Each file is mode 0600,
   replaced in one rename, and never written over a file a person made.
5. **Readings** say when each secret last arrived and when it expires, never its value. The control plane's audit log
   records every delivery, never the value.

From the next release, every plugin that writes secrets on a node also lists them in the node's **registry**
(`~/.local/state/fleetlayer/secrets/`, readable only by the node's user), by reference: `op://vault/item/field`,
`cookie://x/ct0`, `claude://account/2`. That's how the node knows what to scrub, and what `fleetlayer secrets run` may
fill in.

### Claude sign-ins: year-long setup tokens, delivered sealed

A normal Claude sign-in can't be shared: each refresh revokes the copy on every other machine. So the Claude accounts
plugin keeps one hub on an always-on machine you control. The hub holds each account's sign-in and a setup token
(`claude setup-token`: it lasts a year and can only run Claude). It sends the setup tokens to every machine as a
sealed bundle. Each machine writes them into account slots, readable only by its user, and checks that each one
works. Sessions everywhere use the same accounts without anyone copying a sign-in folder.
[Many Claude accounts on many machines](guides/claude-accounts.md) covers the setup.

### Signed-in browser cookies, only where you send them

Some tools act as you on a website with your browser's cookies. `fleetlayer-cookies` reads the cookies of the sites
you list (and only the cookie names you list, if you list them) on the machine with your browser. It sends them sealed
to the machines you name, which write them where the tool looks. When an agent only needs to look at a page or click
through it, [your shared browser](guides/browser.md) is better: the cookies never leave your laptop.
[fleetlayer-cookies](plugins/cookies.md) has the details.

### Secrets from 1Password, granted per role

*In the next release.* `fleetlayer-1password` delivers 1Password items to the roles you grant them to. It runs under a
Unix user of its own and holds the 1Password service-account token there, so no agent holds a token that opens a whole
vault. A grant names items (`op://github/deploy-bot/token`) and the nodes of a role, so vaults become a way to
organise items, not a way to control access, and nobody makes a vault or a token per agent. Each node gets exactly its
items, sealed to its key, again within minutes whenever one changes in 1Password, and loses them (files included) when
a grant is taken away. [fleetlayer-1password](plugins/1password.md) covers the setup.

### Using a secret without seeing it

*In the next release.* Agents use a delivered secret by its reference, so the value never appears in their commands or
their context:

```sh
fleetlayer secrets list                     # what this role has, by reference
GITHUB_TOKEN=op://github/deploy-bot/token fleetlayer secrets run -- gh api user
fleetlayer secrets run --env-file .env -- terraform plan    # lines like NAME=op://vault/item/field
```

`fleetlayer secrets run` replaces `op run`: every environment variable and `--env-file` line whose value is a
delivered reference gets the value, a reference the role wasn't granted is refused with a message saying so, and the
command's output comes back with every delivered secret redacted (`--no-masking` passes it through untouched).
`fleetlayer secrets grants` shows who may read what across the fleet: each source publishes its grants (role, nodes,
references) as a reading. `fleetlayer secrets forget` removes everything delivered to this node; the next delivery
puts it back.

### Scrubbing what leaves a node

*In the next release.* The node knows exactly which secrets it delivered, so it removes exact matches, with no
guessing, from everything it sends the control plane: messages, events, readings, session titles, summaries and
catch-ups. A match is replaced by `[REDACTED <reference>]`, such as `[REDACTED op://github/deploy-bot/token]`. It
matches the forms a value takes once something has encoded it:

- the value itself, and without a trailing newline;
- JSON-escaped, once and twice (a value in a JSON string, and in JSON carried inside a JSON string, as transcripts
  and tool results do);
- URL-encoded;
- base64, standard and URL-safe, wherever it starts in a longer encoded text;
- for values of 32 characters or more, a long prefix (half the value, at least 16 characters), so a token cut short
  on screen is still caught.

Values shorter than 8 characters aren't matched (too many ordinary strings would look like them); the
`secrets.delivered` reading lists them under `tooShort`.

**A match means the secret has already leaked**: it was on a session's screen and went to the model's API before the
node caught it on the way out. So the node also logs it (by reference and form, never the value), publishes a
`secrets.redacted` reading and an event on the `secrets` channel for whichever agent watches for it, and fails its
heartbeat for a day with a message that says to rotate the secret. Rotate it: redaction keeps the value out of the
fleet view, other agents' messages and the control plane, but it can't take it back from the model provider or the
session's own transcript.

Transcripts are copied off machines for search and backups, so scrub the copy before it leaves:

```sh
fleetlayer secrets scrub /path/to/the/copy            # in place; --check only reports
fleetlayer secrets scrub --gitleaks /path/to/the/copy  # also what gitleaks finds, as [REDACTED gitleaks:<rule>]
```

It refuses the live folders (`~/.claude/projects`, `~/.codex/sessions`): rewriting a file a running session appends
to loses its lines. [gitleaks](https://github.com/gitleaks/gitleaks) (MIT) catches secrets nobody delivered, such as
a key pasted into a chat; it's optional, and has to be on `PATH`.

A shared MCP server's calls and results are scrubbed too, from v0.3.13: the node sharing the tool redacts the secrets
delivered to it from every line in both directions, before a call reaches the server and before a result reaches the
agent ([the MCP gate](access-control.md#the-mcp-gate)). A session's transcript read from another
machine (`_fleetlayer.dev/transcript/read`, which herdr-fleet uses) is scrubbed as plain text on the machine it's on,
before it's encoded, and so is a terminal's scrollback (`terminal.scrollback`, which a watcher may ask for). Not
scrubbed yet: the frames of terminal attach (`fleetlayer attach`), which carry the terminal's raw bytes, where a value
can be split between chunks, and show a person their own session; nor pages and apps.

| Reading | Key | Data |
|---|---|---|
| `secrets.delivered` | node | `{references, redacted, tooShort, errors}`: what the node redacts now (references only) |
| `secrets.redacted` | reference | `{reference, forms, hits, firstAt, lastAt, lastContext, action}` |
| `secrets.grants` | `<source>/<role>` | `{source, role, nodes, items: [{ref, access}]}` |

### Tools that hold their own sign-in

An MCP server with its own sign-in (a signed-in browser, a service's API) can run on one machine and be shared with
the fleet as a tool. Agents elsewhere call its tools through Fleetlayer and never see the token behind them. The
machine sharing it decides who may call it and which of its tools, refused calls never reach it, and every call is
logged with its caller ([tools at the level of their own tools](access-control.md#tools-at-the-level-of-their-own-tools)).

An MCP gateway that holds sign-ins for many services (Executor, for example) works the same way, behind
[the MCP gate](access-control.md#the-mcp-gate): its token stays with a node of its own Unix user, your policy decides
each call by what it does (`tools.google.gmail.send`), calls that change things wait for your approval, every call is
audited with its caller, and a delivered secret in a call or a result is redacted. [Executor behind
Fleetlayer](guides/executor.md) sets it up.

### Network access per role

*In the next release.* A role's agents reach only the hosts on its list, through a proxy that reads the host name and
never decrypts the traffic. A host that isn't on the list is refused with a message that tells the agent how to ask.
The request goes to a person through the same approvals as everything else, and once approved it stays allowed. It
starts in a mode that only logs, so a week of real traffic builds the list before anything is blocked, and it keeps
restricted roles off your local network and tailnet by default. Linux only for now. [Network access per
role](egress.md) has the details, and [the guide](guides/egress.md) sets it up.

### Trust is per Unix user

A machine can only enforce boundaries between Unix users. Every process of one user can read that user's files and
act as any of its sessions, so Fleetlayer draws the line there too:

- **Agents that read strangers' text** (email, web pages, issues) run as their own Unix user, with their own node.
  They can't read a trusted user's node token, its key or the credentials delivered to it.
  [An agent that reads strangers' text](guides/untrusted-input.md) sets that up.
- **Anything trusted more than its neighbours** (a runner of approved actions, a service with wider rights) gets its
  own user and node too.
- **The access policy decides what each node's agents may do** across the fleet: whom they may message, which tools
  they may call, what needs a person's approval. Every decision is written to a tamper-evident audit log before the
  action happens ([Access control](access-control.md)).

### An admin role that's off until you open it

*In the next release.* An agent that can change everything sits in a role that's switched off: every request by it or
to it is refused, and nothing is delivered to it. It asks for a window (`fleetlayer role activate admin --for 1h`),
you approve, and its rules apply until the time is up. Then its sessions are closed, what was delivered to it is
removed, a hook takes away its sudo rule, and its node confirms; a window that didn't close cleanly shows as a health
problem. Every action in the window is audited with the window's ID. The rules are in [the admin
window](access-control.md#the-admin-window-a-role-thats-off-until-you-open-it), and [the
guide](guides/admin-window.md) sets one up.

### Webhooks don't leak their own secret

A webhook address carries a secret in its path, so anyone who has it can send events. The control plane stores only
its hash, and removes the secret from everything it delivers, even when the sender echoes the address back (GitHub's
ping does). With a GitHub signing secret set, a delivery without a valid signature is refused. A leaked address can
be replaced without changing anything else ([CI results to the agent that pushed](guides/ci-results.md)).

## What's coming

These are being designed or built now. Until each one ships, treat it as a plan, not a promise.

### Short-lived credentials minted per use

Where a service can mint short-lived credentials (a GitHub App's installation tokens, scoped cloud tokens), a broker
holding the root credential will mint one per use instead of delivering a long-lived token. Storing a secret from an
agent (`fleetlayer secrets put`, by grant and approval) is planned too.

## What none of this covers

- **Anything an agent can read, it can send.** Scrubbing catches accidents, and network rules narrow where data can
  go, but an allowed host is still a way out (a public gist, a push to a public repository). Short-lived, narrowly
  scoped tokens limit what a leaked one can do.
- **Within one Unix user, sessions trust each other.** Which session used a secret is recorded, but nothing can stop
  one session in a role reading what another in the same role can.
- **The model sees what the agent reads.** A secret printed in a terminal has gone to the model's provider. Treat it
  as exposed, and rotate it.
