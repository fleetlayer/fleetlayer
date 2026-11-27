# Executor behind Fleetlayer

[Executor](https://executor.sh) (MIT) holds the sign-ins for your remote services (Gmail, Calendar, your issue
tracker, Slack) and gives agents one MCP server for all of them. That makes its token a key to everything: any agent
that can read it can send email as you. This guide puts Executor behind a Fleetlayer node, the MCP gate, so that:

- the token lives under a Unix user of its own, and no agent can read it or go around the gate;
- what each role may call is in your policy file (allow, ask, deny, per Executor path such as
  `tools.google.gmail.send`);
- calls that change things wait for your approval, and the approval holds for exactly the call you saw;
- every call is in the audit log with the agent that made it;
- a delivered secret in a call or its result is redacted before it goes on.

And you sign in to Executor's dashboard with Fleetlayer's sign-in, so there's no separate password.

It takes an agent about ten minutes. You'll need Fleetlayer v0.3.13 or later on the control plane and on the gate's
machine.

## 1. A Unix user and a node for the gate

The gate is trusted more than the agents beside it (it holds the token), so it runs as its own Unix user with its
own node ([why](../access-control.md#who-and-what)). On the always-on machine:

```sh
sudo useradd --create-home --shell /usr/sbin/nologin fleetlayer-gate
fleetlayer-control node add gate        # on the control plane: prints the node's token once
```

Install the node service for that user as for any machine ([Add a machine](node-services.md)), with the token in
`~fleetlayer-gate/.config/fleetlayer/token` (mode 0600, owned by `fleetlayer-gate`). On a laptop, use a second macOS
account the same way.

## 2. Executor's token, where only the gate can read it

Make a token for the gate in Executor (its **Tokens** page, or `executor login` on a self-hosted one) and write it
to the gate user's home:

```sh
sudo -u fleetlayer-gate sh -c 'umask 077; mkdir -p ~/.config/executor; cat > ~/.config/executor/token'
# paste the token, then Ctrl-D
```

Then revoke every token your agents held for Executor until now, and remove them from their machines: a token left
in an agent's home is a way around the gate.

## 3. Share Executor from the gate's node

In `~fleetlayer-gate/.config/fleetlayer/node.yaml`:

```yaml
tools:
  - name: executor
    description: "Executor: Gmail, Calendar, the issue tracker and Slack, behind the gate"
    url: https://executor.example.com/mcp
    bearer_file: ~/.config/executor/token
    allow: ["*@agents", "*@laptop"]           # which nodes' agents may connect at all
    check_calls: true                         # check every call with the policy
    call_targets:
      call: ["/calls/*/path"]                 # a call is named by the Executor paths it calls
```

Restart the gate's node. Within five minutes it has checked Executor with the token (initialize, list its tools);
`fleetlayer status` on that machine, and its heartbeat, say so if the token is refused or Executor is down.

## 4. Rules for each role

Executor's MCP server has a few tools of its own: `search`, `describe` and `skills` (finding tools), `call` (calling
them by path), `execute` (a JavaScript program over all of them) and `resume` (answering a pause inside Executor).
With the `call_targets` above, each call is named `tool:executor/<tool>@gate`, and a `call` is named by each path in
it: `tool:executor/call/tools.google.gmail.send@gate`. In your policy file:

```yaml
deny:
  # A program can reach any path, built at run time, so the gate can't check it call by call.
  - who: "*"
    can: call
    to: "tool:executor/execute@gate"

only:
  # The agent that reads email may search and read it, nothing else.
  - who: "*@triage"
    can: call
    to: ["tool:executor/search@gate", "tool:executor/describe@gate", "tool:executor/skills@gate",
         "tool:executor/call/tools.google.gmail.search@gate", "tool:executor/call/tools.google.gmail.readThread@gate"]

ask:
  # Anything that sends, creates or deletes waits for you, once, with the arguments you saw.
  - who: "*"
    can: call
    to: ["tool:executor/call/tools.*.send*@gate", "tool:executor/call/tools.*.create*@gate",
         "tool:executor/call/tools.*.delete*@gate", "tool:executor/call/tools.*.draft*@gate"]
    for: once
```

Apply it (`fleetlayer-control acl apply`, or let the control plane read the file itself), and check it with
`fleetlayer-control acl explain --from inbox@triage --can call --to tool:executor/call/tools.google.gmail.send@gate`.
Paths are whatever Executor calls them: `search` lists them. Match by app (`tools.google.*`) and by verb in the name,
and keep a test file (`acl test`) with the calls that matter, so a renamed path shows up in CI rather than in your
inbox.

**About `execute`.** Denying it costs agents little: what a program does, `call` does in a few calls, each checked.
If you'd rather keep it, ask for it `for: once`: the approval shows the program and the paths written in it, and you
read it before it runs. Allowing it outright is allowing everything the token can do, so do that only for a role
you'd trust with the token itself.

**Executor's own approvals** still apply inside Executor (its automatic, model, browser or native modes). The gate
comes first: a call it refuses never reaches Executor.

## 5. Agents use the gate

On each agent's machine, replace Executor's MCP server with the gate:

```sh
claude mcp remove executor
claude mcp add executor -- fleetlayer mcp executor@gate
```

`fleetlayer me` lists it for every session allowed to use it. An agent sees the same tools as before. A call that
waits for you takes longer and then goes through; one you haven't answered within two minutes comes back with the
request ID, and the agent makes the same call again once you've approved it (the `fleetlayer` skill tells agents
this). You see each request as an `access.ask` event (the Inbox, a phone push, whoever subscribes to `access`) and
with `fleetlayer-control approvals list`, which shows the call and its arguments.

## 6. Sign in to Executor with Fleetlayer

Self-hosted Executor signs people in with any OpenID Connect provider, and Fleetlayer's sign-in is one. In the
sign-in plugin's settings, register Executor as a client
([how](sign-in.md)):

```yaml
clients:
  - name: executor
    redirect_uris: [https://executor.example.com/api/auth/callback/sso]
    secret_file: ~/.config/fleetlayer/signin-clients/executor
```

Executor creates an account for a new person only from a verified email at a domain you allow, so each person who
signs in needs an email in Fleetlayer: one their upstream provider verified (Google Workspace, your company's OIDC),
or one an operator sets with `fleetlayer-control person email <person id> <address>`. Then set four variables on
Executor's container and restart it:

```sh
SSO_DISCOVERY_URL=https://signin.example.com/.well-known/openid-configuration
SSO_CLIENT_ID=executor
SSO_CLIENT_SECRET=<the contents of the secret file>
SSO_ALLOWED_DOMAINS=example.com
```

Executor's login page then has **Continue with SSO**. The discovery address must be the final HTTPS address
(Executor follows no redirects). An existing password account links to SSO from **Account → Security → Connect SSO**.
Who may sign in to Executor at all is a `sign_in` rule on `service:executor`, as for any service.

## Moving it

The gate is a node like any other: its state is the node's directory and the token file. Move it as
[a node moves](../concepts.md#start-small-move-pieces-later), keep the token file mode 0600 under the new user, and repoint nothing: agents name
`executor@gate`, wherever `gate` runs.

## When something's wrong

- **Every call is refused with "couldn't check this call with the control plane"**: the gate's node isn't linked.
  The gate fails closed: no answer is no call.
- **"this call call has nothing at /calls/*/path"**: the agent sent a `call` without a path. The gate refuses what it
  can't name rather than letting it through as less than it is.
- **The node's health says the server behind executor isn't working**: Executor is down, or refused the token (it
  was revoked or expired). Make a new one (step 2).
- **A call you approved asks again**: an approval `for: once` holds for one call with the same arguments. An agent
  that changed one argument made a different call.
