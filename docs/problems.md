# The problems we solve

Draft, November 2026. Everyone is building an agent setup for one project. This is about running many: agents for
personal errands, email triage, product features, infrastructure, background workers, each with its own machine or
Unix user and its own permissions, while you still see and steer all of them from your own terminal and phone. Once an
agent can set the whole thing up, adding a new agent, or a whole isolated worker with its own permissions, takes
minutes.

It's built for people whose work and life share the same agents. A solo founder with a family, say: a company's
agents (features, infrastructure, support, finances) and the family's (personal errands, email, home infrastructure)
on one system, with a partner who signs in to their own space, and an agent like finances that is deliberately allowed
to see both sides. Few products try to serve personal and professional use at once; most make you choose, or run two
of everything.

One driver runs through all of it: **no vendor lock-in**. You can keep using Anthropic's and OpenAI's tooling and
built-in features (connectors, artifacts, remote control, their apps). When you outgrow them and hit their limits, you
don't have to give anything up: each problem below has an answer that works with any agent, any model and any vendor,
on machines you control.

Three pieces, each solving its own set of problems:

- **Fleetlayer**: connects the agent sessions you already run, on every machine, into one fleet you control.
- **A server setup** (`fleetlayer/server-iac-example`, planned): the always-on machines that fleet runs on, as code
  agents can read and change.
- **Quietdeck**: one person's control deck built on top. Mentioned here, not required.

Marked **(planned)** where it isn't built yet.

## Fleetlayer

### Seeing the fleet

1. **You can't see what every agent is doing across machines.** Each session manager shows its own machine. One view
   lists every machine and session, with what each is doing now.
2. **You lose track of what an agent did and why.** Summaries of each session, written as it works and kept as plain
   files on the machine, with a copy you can search across the fleet; a catch-up for any session on request.
3. **You can't watch or step into a session on another machine.** Watch or drive any session's terminal from anywhere,
   whatever runs it (herdr now; tmux, T3 Code and others as plugins), under the access policy.
4. **Every isolated agent needs its own phone pairing** (a relay, ports, firewall rules, an invite per Unix user).
   One pairing reaches every session on every node, new ones included. **(planned)**

### Agents working together

5. **Agents on different machines can't talk to each other.** Messages between sessions, on one machine or across
   machines, delivered through the agent's own channel and labelled with who sent them.
6. **A typo in a session's name looks like an outage.** A wrong name says so and suggests the right one; a session
   that's stopped says it's stopped; a unique name works without saying which machine.
7. **Webhooks wake a fresh agent with no context.** Events are routed to the session that owns the work: a CI failure
   goes to the agent that wrote the branch.
8. **Routine actions burn tokens and wait for an agent.** A runner carries out typed actions on your own machine
   without AI, and wakes an agent only when a decision needs judgment.
9. **A tool that lives on one machine is stuck there** (your signed-in browser on a laptop, a database on a server).
   Shared tools: an agent anywhere uses an MCP server running on another machine, limited to the networks, callers and
   functions you allow, cleaned up when it disconnects.
10. **Copying skills, settings and secrets to every machine by hand.** Sync bundles, sealed to each machine's own key;
    the control plane relays them and can't read them.

### Claude accounts

11. **One Claude subscription isn't enough, and switching accounts by hand doesn't scale.** Usage across all your
    accounts, shared by every session on every machine, so the account rotator moves sessions before an account runs
    out.
12. **Claude sign-ins break when shared across machines and sessions** (a refresh token used twice signs everyone
    out; each machine needs its own sign-in per account, every month). Year-long setup tokens made once per account
    and sent to every machine sealed; the sign-ins live only on a machine you control.
13. **Signing in to many accounts on a server with no browser.** A sign-in page you open from your own browser: you
    sign in and paste the code; the server does the rest, warns before anything expires and checks every token works.
14. **A sign-in hub on your laptop stops everything when you close the lid.** It runs on an always-on machine.

### Trust and control

15. **An agent that reads strangers' text (email, social media, public forms) can be talked into anything.** Its
    messages arrive labelled as outside data, it gets its own Unix user and node, and the policy limits whom it may
    message, what it may read and how much it may send.
16. **All agents can do everything, or you approve every step by hand.** An access policy (groups, deny, only, allow,
    rate limits) checked on every action, with approvals and justifications where you want them: on by default, off
    for a fleet that runs autonomously on purpose.
17. **You can't tell afterwards who did what.** A tamper-evident audit log of every access decision, checked on a
    schedule.
18. **An agent blocked by a rule fails silently.** Every refusal says which rule and why, and how to report it if it
    gets in the way of the job.
19a. **Every small service grows its own login** (a key here, a passkey there, a token for the next one), and none of
    them know your groups or spaces. One sign-in for every service you run, through Fleetlayer, with your company's
    provider (Google Workspace, Okta and others) or a simple built-in directory, and permissions from the same access
    policy as your agents. **(planned)**
19. **Your credentials end up on someone else's server.** The control plane holds no credentials; secrets cross it
    only as ciphertext for one machine.

### Pages

- **Agents' pages for people (screenshots, reports, mock-ups) are locked to one vendor and one account.** Pages are
  plain folders in your synced vault, written by any agent or model; Fleetlayer hosts whatever is there, with access
  per space. **(planned)**

### Many people

- **Every person who joins needs their own servers and setup.** One deployment serves a team or a household: each
  person signs in separately, brings their own accounts and tools, and owns their agents, spaces and data, private by
  default and shared on purpose. **(planned)**

### Running it

20. **Background things stop working and nobody notices.** Every daemon, sync and scheduled job reports a heartbeat
    that fails when it stops doing its job, not only when it stops running.
21. **Adopting a new tool means abandoning the ones you use.** Fleetlayer attaches to your session manager without
    taking it over; your apps and terminals keep working beside it. Plugins are separate processes in any language,
    speaking the Agent Client Protocol.
21a. **Starting needs a server, or outgrowing a laptop means starting over.** Start with everything on your Mac and
    move any one piece (the control plane, sign-in, the Claude hub, pages) to a home server or a VPS when you outgrow
    it, by asking an agent: each piece keeps its state in one place and has a tested move. `fleetlayer up` starts
    everything on a laptop at `<name>.localhost` with a status page, and `fleetlayer move <piece> <machine>` moves
    one with its keys and data, tested for each piece ([Moving a piece](guides/moving.md))
22. **Keeping every machine up to date.** Releases with notes, signed builds and self-update.

## The server setup (planned: `fleetlayer/server-iac-example`)

1. **Agents that matter run on a laptop that sleeps.** A recipe for always-on machines: a Proxmox host with VMs and
   containers, from a Mac mini or a rented server up to a cluster, in the cloud or on premises.
2. **Setting up a server by hand can't be repeated, reviewed or handed to an agent.** Everything as code in git
   (Terraform, Ansible, k3s, Argo CD): an agent makes a change as a pull request, and a rebuilt machine comes back the
   same.
3. **Adding an agent safely is slow.** A new agent is a new Unix user (or VM) with its own node, home, credentials and
   access rules, from one change.
4. **Internal pages end up on the public internet, or unreachable from your phone.** Private HTTPS names that work at
   home and away (Tailscale, a reverse proxy with a wildcard certificate), with a firewall that admits only the proxy.
5. **Agents need credentials, and without a secrets manager they're pasted into chat.** Service accounts per agent in
   a secrets manager, read at deploy time into files only that agent's user can read.
6. **Monitoring that nobody acts on.** Uptime checks and heartbeats defined in git beside each service, alerts to a
   room an agent listens to, and an agent that fixes what it can.
7. **Losing a machine loses everything.** Backups by default, with what's deliberately left out written down.
8. **Big builds overload small machines.** A build server agents use for compiles and tests.
9. **Choosing tools takes weeks of reading.** Recommended tools, each with an honest comparison against the
   alternatives (Gatus vs Uptime Kuma and the rest).
10. **The goal:** one coding agent on your laptop, given these docs and SSH to a brand-new server, sets the whole thing
    up in a couple of hours.

## Quietdeck (built on Fleetlayer; mentioned, not required)

1. **Agents propose actions and you have no good place to approve them.** A deck of cards: each says what an agent
   wants to do and why, with buttons to approve, change or decline, and rules that decide routine cards for you.
2. **Email triage either does too much on its own or asks too much.** An agent reads mail and posts cards with
   suggested actions; you decide in batches, and its report goes at the top of your inbox.
3. **You can't see what was done automatically.** A timeline of automatic actions, and threads you can watch.
4. **Your phone buzzes for everything.** Only urgent events are pushed (a disk running out); everything else waits in
   the deck.
5. **Agents step on each other over shared things.** Leases: a temporary Gmail filter while an agent fixes CI so the
   failure emails stay out of your inbox, and build slots so big compiles take turns on a machine.
6. **Usage across accounts and tools isn't visible at a glance.** A usage screen and a menu bar app.
