# Credentials: how Fleetlayer keeps secrets

Agents need credentials to do real work: a Claude sign-in, a GitHub token, cookies for a site, a cloud API key. Each
one is a risk wherever it sits, because anything that can read it can use it. This page says how Fleetlayer handles
them. The first part covers what works today, checked against the code. The second covers what's being built, and is
marked as coming.

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
  the old pin. A control plane that lied about a node's key still couldn't read what's sent to it.
- By default, the first bundle from each source to each target waits for a person's approval.
- Status flows back to the sender ("applied", or the error). Content never flows back.

What the control plane does store: hashes of the tokens nodes and webhooks use to reach it, and the signing secret
for each GitHub webhook you give one, which it needs to check GitHub's signature. That signing secret lets someone
forge a webhook delivery to your fleet, and nothing more. The mechanism is in
[sync and sealed secrets](protocol.md#8-sync-and-sealed-secrets).

### Claude sign-ins: year-long setup tokens, delivered sealed

A normal Claude sign-in can't be shared: each refresh revokes the copy on every other machine. So the Claude accounts
plugin keeps one hub on an always-on machine you control. The hub holds each account's sign-in and a setup token
(`claude setup-token`: it lasts a year and can only run Claude). It sends the setup tokens to every machine as a
sealed bundle. Each machine writes them into account slots, readable only by its user, and checks that each one
works. Sessions everywhere use the same accounts without anyone copying a sign-in folder.
[Many Claude accounts on many machines](guides/claude-accounts.md) covers the setup.

### Signed-in browser cookies, only where you send them

Some tools act as you on a website with your browser's cookies. `fleetlayer-cookies` reads the cookies of the sites
you list (and only the cookie names you list, if you list them) on the machine with your browser. It sends them sealed to the machines
you name, which write them where the tool looks. When an agent only needs to look at a page or click through it,
[your shared browser](guides/browser.md) is better: the cookies never leave your laptop.
[fleetlayer-cookies](plugins/cookies.md) has the details.

### Tools that hold their own sign-in

An MCP server with its own sign-in (a signed-in browser, a service's API) can run on one machine and be shared with
the fleet as a tool. Agents elsewhere call its tools through Fleetlayer and never see the token behind them. The
machine sharing it decides who may call it and which of its tools, refused calls never reach it, and every call is
logged with its caller ([tools at the level of their own tools](access-control.md#tools-at-the-level-of-their-own-tools)).

An MCP gateway that holds sign-ins for many services (Executor, for example) works the same way: agents call tools
and never hold the services' tokens.

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

### Webhooks don't leak their own secret

A webhook address carries a secret in its path, so anyone who has it can send events. The control plane stores only
its hash, and removes the secret from everything it delivers, even when the sender echoes the address back (GitHub's
ping does). With a GitHub signing secret set, a delivery without a valid signature is refused. A leaked address can
be replaced without changing anything else ([CI results to the agent that pushed](guides/ci-results.md)).

### An admin role that's off until you open it

An agent that can change everything sits in a role that's switched off: every request by it or to it is refused, and
nothing is delivered to it. It asks for a window (`fleetlayer role activate admin --for 1h`), you approve, and its
rules apply until the time is up. Then its sessions are closed, what was delivered to it is removed, a hook takes away
its sudo rule, and its node confirms; a window that didn't close cleanly shows as a health problem. Every action in
the window is audited with the window's ID. [The admin window](access-control.md#the-admin-window-a-role-thats-off-until-you-open-it),
and [the guide](guides/admin-window.md).

## What's coming

These are being designed or built now. Until each one ships, treat it as a plan, not a promise.

The principles they share:

- **Off by default.** Most people start with a token in their shell, and Fleetlayer never forces more. It says what's
  at risk in plain words, and one request to an agent sets the rest up.
- **An agent sets it up.** The skills carry the steps, so nobody has to learn a secrets system first.
- **Roles, not sessions.** Sessions start, end and get renamed all day, so nothing is configured per session. Access
  belongs to a **role**: a Unix user with its own node, because that's the boundary the machine enforces. A handful
  of roles covers most fleets (everyday work, untrusted input, a trusted runner, infrastructure). A new session in a
  role gets that role's secrets with no setup. Which session used a secret is still recorded in the audit log.

### Secrets granted per role, from your password manager

A secrets plugin reads items from 1Password, and later other secret stores, and delivers each one sealed to the
roles your policy grants it to, the same way Claude tokens travel today. The plugin holds the password manager's
credential under its own Unix user, so no agent holds a token that opens a whole vault. Grants are per item, so
vaults become a way to organise items, not a way to control access. Delivered secrets are written for the role's
user only, and their freshness shows as a reading, never their value.

Where a service can mint short-lived credentials (a GitHub App's installation tokens, scoped cloud tokens), the plan
prefers minting one per use over delivering a long-lived token.

### Scrubbing what leaves a node

A node knows exactly which secrets it delivered, so it can remove exact matches from everything that leaves it:
messages, events, session summaries, tool calls and their results, and copies of transcripts before they're synced
or backed up. It will match encoded forms too (JSON-escaped, base64, URL-encoded), with a general secret scanner for
what it didn't deliver. A match also raises an alert to rotate that secret, because by then the value has already
been on a screen and sent to a model.

### Network access per role

Built, and new: [Network access per role](egress.md), with [a guide](guides/egress.md). A role's agents reach only
the hosts on its list, through a proxy that reads the host name and never decrypts the traffic. A host that isn't on
the list is refused with a message that tells the agent how to ask. The request goes to a person through the same
approvals as everything else, and once approved it stays allowed. It starts in a mode that only logs, so a week of
real traffic builds the list before anything is blocked, and it keeps restricted roles off your local network and
tailnet by default. Linux only for now.

## What none of this covers

- **Anything an agent can read, it can send.** Scrubbing catches accidents, and network rules narrow where data can
  go, but an allowed host is still a way out (a public gist, a push to a public repository). Short-lived, narrowly
  scoped tokens limit what a leaked one can do.
- **Within one Unix user, sessions trust each other.** Which session used a secret is recorded, but nothing can stop
  one session in a role reading what another in the same role can.
- **The model sees what the agent reads.** A secret printed in a terminal has gone to the model's provider. Treat it
  as exposed, and rotate it.
