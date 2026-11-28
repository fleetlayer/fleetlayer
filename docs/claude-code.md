# What Fleetlayer adds beyond Claude Code

Claude Code keeps adding features that used to need something like Fleetlayer: sessions that message each other,
Remote Control from your phone, sessions in the cloud, scheduled routines and artifacts. If they cover what you do,
use them. This page says what each one does, where its edges are, and what Fleetlayer adds past them. Fleetlayer works
alongside all of them: nothing here is either/or.

## You probably don't need Fleetlayer if

- you use one Claude account, on one machine;
- your agents are all Claude Code, and you're happy with Anthropic's cloud for anything that runs while you're away;
- you don't need rules about which agent may do what, or a record of what they did.

## You probably do if

- your agents run on more than one machine (a laptop, a home server, a VPS) or under more than one Claude account;
- you run Codex or other harnesses next to Claude Code;
- an agent reads email, issues or the web, and you want it kept away from the others' rights and secrets;
- you want your agents, your secrets and the record of what they did on machines you control.

## Feature by feature

### Sessions messaging each other

**Claude Code:** [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging) lets one session
message another. Between sessions on one machine it uses a local socket; to sessions on other machines or in the cloud
it goes through Anthropic's servers and needs a claude.ai sign-in (not an API key or a setup token).
[Agent teams](https://code.claude.com/docs/en/agent-teams) are a lead session with teammates, experimental and off by
default.

**Fleetlayer adds:** messages between any two sessions on any of your machines, under any account and any sign-in
(setup tokens included), and between Claude Code and Codex. They go through your own control plane, every message is
checked against your access policy, and an agent that reads strangers' text has its messages marked as outside data.
[Watch, steer and message](guides/day-to-day.md).

### Remote Control and your phone

**Claude Code:** [Remote Control](https://code.claude.com/docs/en/remote-control) lets claude.ai or the Claude app
drive a session that keeps running on your machine. It needs a claude.ai subscription sign-in; API keys and setup
tokens aren't supported.

**Fleetlayer adds:** every session on every machine in one list, with what each is doing and whether it's waiting on
you; `fleetlayer attach` to watch or drive any session's terminal; blocked prompts answered from anywhere; and one
herdr phone relay for every machine ([Every machine on your phone](guides/phone.md)). It works with setup tokens.
Codex sessions get all of it except blocked prompts, which Fleetlayer reads only from Claude Code.

### Sessions in the cloud, and scheduled routines

**Claude Code:** [Claude Code on the web](https://code.claude.com/docs/en/claude-code-on-the-web) runs sessions on a
VM Anthropic manages (or an environment you host), from a GitHub repository; it can't reach your local files.
[Routines](https://code.claude.com/docs/en/routines) are saved prompts that run as cloud sessions on a schedule, an API
call or a GitHub event, at most hourly, in a research preview.

**Fleetlayer adds:** the same kind of always-on work on machines you choose: a home server, a VPS, a VM, with your own
files, databases, tools and sign-ins there. The [scheduler](plugins/scheduler.md) sends a session a prompt on any
schedule, [webhooks](guides/ci-results.md) reach the session that pushed the commit, and sessions can be started,
resumed and handed off on any machine.

### Artifacts and pages

**Claude Code:** [artifacts](https://code.claude.com/docs/en/artifacts) are claude.ai pages published from a session.
They're private to the account that made them until shared, and publishing needs a claude.ai sign-in.

**Fleetlayer adds:** [pages and apps](guides/pages.md) any agent can write, Claude or Codex or a shell script, under
any account, as plain folders you keep, served behind your own sign-in to the people your policy allows.

### Your browser

**Claude Code:** [Claude in Chrome](https://code.claude.com/docs/en/chrome) drives your own signed-in Chrome. It needs
the extension signed in to the same organisation as Claude Code, and a claude.ai sign-in; setup tokens turn it off.

**Fleetlayer adds:** your signed-in Chrome on your laptop, used by agents on any machine, under any account and any
harness, each in its own tab group, with the policy deciding which agents may ([Share your browser](guides/browser.md)).

### Accounts and usage

**Claude Code:** `/usage` shows the current account's usage, computed from this machine's history: usage from other
devices isn't included ([costs](https://code.claude.com/docs/en/costs)). Team and Enterprise admins get reports per
person.

**Fleetlayer adds:** your own accounts on your own machines: year-long setup tokens delivered to every machine, every
account's usage visible to every session, and a rotator that moves sessions off an account that has no usage left
([Many Claude accounts](guides/claude-accounts.md)).

### Rules, approvals and audit

**Claude Code:** [permission rules](https://code.claude.com/docs/en/permissions) (allow, ask, deny) and
[hooks](https://code.claude.com/docs/en/hooks) apply on the machine where a session runs, and
[managed settings](https://code.claude.com/docs/en/managed-settings) let an organisation push them to each machine.
They govern what one session may do with its tools.

**Fleetlayer adds:** one [access policy](access-control.md) across every machine and harness, about what agents may do
to each other and to shared things: whom an agent may message, which shared tools and which of their calls it may use,
which secrets it receives, what waits for your approval. Every decision goes in a tamper-evident audit log. Both work
together: Claude Code's permissions inside a session, Fleetlayer's policy between sessions and machines.

### Secrets, network and shared tools

**Claude Code:** not its job. Credentials are whatever the session's environment holds.

**Fleetlayer adds:** [secrets per role](credentials.md) from 1Password, delivered sealed and scrubbed from what leaves
a machine; [network access per role](guides/egress.md); tools that live on one machine (a browser, your screenshots,
a deploy command that needs your security key) shared with agents elsewhere, with the token staying where it is.

## Self-hosted

Everything Fleetlayer runs is on machines you choose, and it holds no credentials of its own. It doesn't replace
Claude Code: it connects the Claude Code and Codex sessions you already run, and leaves Anthropic's features there for
you to use when they suit you.

Related: [Fleetlayer and OpenClaw](openclaw.md), [How Fleetlayer fits together](concepts.md).
