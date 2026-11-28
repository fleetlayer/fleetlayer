# The dashboard

Status: built (28 November 2026): machines, sessions, events, approvals and admin windows, readings and a graph of the
fleet; people, invites and teams; and the access rules in force (read here, changed in the policy file). Sign-in
providers and editing rules from the page come next.

The dashboard is Fleetlayer's web page for people: one place to see every machine and what it runs, every agent
session and whether it needs you, what arrived and who it reached, and what's waiting for your approval. It's live:
each page refreshes itself every five seconds.

It shows only what the control plane's API gives it, with a token of its own, and does only what the API does. So
everything on it can also be done with the `fleetlayer` command, the API, or an agent: the dashboard is for having a
look around, not the only way in. The same dashboard works with a control plane you run yourself and with a hosted one.

## The pages

- **Overview**: four numbers (machines online, sessions, what's waiting on you, problems), a list of what needs you
  (an agent waiting at a prompt, a request to approve, an open admin window, an offline machine, a failing plugin),
  every machine with its nodes, sessions and plugins, the newest events and the sessions working now.
- **Machines**: each machine and its nodes (one per Unix user), each node's version, when it was last heard from, its
  plugins and their state, its problems, and the tools it shares and with whom.
- **Sessions**: every running session, grouped by whether it's waiting on you, working, idle or done, with its latest
  one-line summary; filter by node.
- **Events**: the newest hundred events (webhooks, CI results, schedules) and which sessions each reached.
- **Access**: requests waiting for a person's approval, with Approve and Deny, and the windowed roles (`docs/access-control.md`,
  "Windows"): which are open and until when, with Close now, and the recent windows.
- **Graph**: the control plane, each machine with its nodes and their plugins, which nodes are linked, and which
  shared tool each node may use.
- **Readings**: the newest of every reading (Claude account usage, token health, `node.status`, `plugin.status`...).
- **People**: everyone in the fleet and their role, inviting someone (a link that works once, for a week, which they
  open to make their passkey), making someone an admin or a member, removing them, and the invites not used yet.
- **Teams**: groups of people the access rules can name (`team:design`), each inside another if you like: making one,
  putting people in it and taking them off. Someone in a team through their sign-in provider's groups is marked so.
- **Rules**: the access policy in force, as sentences (who may never, may only, may do what to what), the agent roles
  and which of them are off until a window is opened, and the policy file itself. Rules are changed in that file, kept
  in git where the agents it limits can't change it, and applied with `fleetlayer-control acl apply`.

People, Teams and Rules show to the fleet's admins, and to anyone the policy allows `admin` on `service:dashboard`
(which is for using the dashboard); only the fleet's admins change people. The control plane checks that on every
read and change, and records each change, with a role's old and new value, as the person who made it; refused and
failed attempts are recorded too. Every change grants or takes away rights, so each asks for your passkey again when
you last used it more than ten minutes ago, and the control plane enforces that itself: the sign-in service tells it
each time someone uses their passkey (over its own token), so neither a session left open in a browser nor the
dashboard's token can skip it. An invite's link is shown once, on the page, never in an address. One person makes at
most 20 invites an hour.

## Try it

`fleetlayer-dashboard demo` serves a made-up fleet at `http://127.0.0.1:7690/`, with no sign-in (so only on this
machine). Nothing you click changes anything.

## Set it up

On a laptop, `fleetlayer up` already runs it, at `http://dashboard.localhost:7680`, with a login key
(`~/.local/state/fleetlayer/dashboard/login`) and an admin token of its own. It runs where the control plane does, so a
machine that joins a fleet elsewhere doesn't get one.

Anywhere else, it's a plugin of a node, like pages: run it on the machine where your gateway and sign-in service run (or anywhere
that reaches the control plane).

1. **A token for it** on the control plane: `fleetlayer-control token add dashboard --scope delegate`. With it, each
   signed-in person decides requests and closes windows as themselves: the control plane checks they're one of the
   tenant's admins and records the decision as theirs ([Approving from a deck or an inbox](access-control.md#approving-from-a-deck-or-an-inbox)).
   With `--scope read`, people see requests and decide them elsewhere. Save it in a file only the dashboard's Unix user
   reads, and run the dashboard under a Unix user of its own, never one whose node runs agents.
2. **A sign-in client** in the sign-in service's config (`docs/plugins/signin.md`):

   ```yaml
   clients:
     - name: dashboard
       redirect_uris: [https://dashboard.example.com/_/auth/callback]
       post_logout_redirect_uris: [https://dashboard.example.com/_/signed-out]
       backchannel_logout_uri: https://dashboard.example.com/_/auth/backchannel-logout
       secret_file: ~/.config/fleetlayer/signin-clients/dashboard
   ```

3. **The plugin** in `node.yaml`, with the `access.check` grant so it can ask what each person may do:

   ```yaml
   plugins:
     - name: dashboard
       command: fleetlayer-dashboard
       grants: [access.check]
       config:
         listen: 127.0.0.1:0
         base_url: https://dashboard.example.com
         control:
           url: https://control.example.com
           token_file: ~/.config/fleetlayer/dashboard/control-token
         signin:
           issuer: https://signin.example.com
           secret_file: ~/.config/fleetlayer/signin-clients/dashboard
         heartbeat:
           url: https://status.example.com/api/v1/endpoints/dashboard/external
           token_file: ~/.config/fleetlayer/dashboard/heartbeat-token
   ```

   The node tells the gateway where it listens, so it's at `dashboard.<your domain>` with no route to add.

4. **Who may use it**, in the policy. Admins may sign in to every service from the start. For anyone else:

   ```yaml
   rules:
     - who: team:engineering
       can: [sign_in]
       to: [service:dashboard]      # look
     - who: person:alex
       can: [admin]
       to: [service:dashboard]      # approve requests and close windows
   ```

Without the sign-in service, `login_file:` (a file with a random key of at least 32 characters) lets in whoever has the
key. Its holder decides through an admin token (`--scope admin`), as `fleetlayer up` sets up on a laptop, and can't open
role windows from the dashboard: that needs a person's passkey. Both can be set while you move from one to the other.

| Setting | What it is |
|---|---|
| `listen` | Where it listens (`127.0.0.1:0` picks a free port and tells the gateway). |
| `base_url` | The address people open. Required with `signin`. Plain `http://` only on `localhost`. |
| `control` | `url` (the control plane's API) and `token_file`. |
| `signin` | `issuer`, `client` (default `dashboard`, also `service:<client>` in the policy) and `secret_file`. |
| `login_file` | A login key, for a dashboard without the sign-in service. |
| `trusted_proxies` | Addresses whose `X-Forwarded-For` is believed, for the login key's lockout. |
| `heartbeat` | `url` and `token_file`: pushed every minute (below). |

## Who sees what

- Signing in checks `sign_in` on `service:dashboard`, and every page checks it again (remembered for a minute), so
  taking someone's access away works within a minute rather than when their session ends.
- Approving, denying and closing a window need `admin` on `service:dashboard`, a form token from the same session,
  and, for a signed-in person, the dashboard's delegate token: the control plane checks them and records every
  decision as theirs (`person:sam (via dashboard)`).
- **Opening a role window** takes the person's passkey. Approve with passkey goes to the sign-in service's step-up
  page for that request, which comes back to the dashboard to confirm; the control plane accepts it within five
  minutes, once, and only from that person. The login key's holder can't open windows from the dashboard.
- When the access check can't be made (the control plane is down), the dashboard shows nothing rather than guessing.
- A session lasts 12 hours, in memory: restarting the dashboard signs everyone out. Signing out at the sign-in
  service ends it here too (back-channel logout).

## What it reads

Its calls to the control plane, made at most once every three seconds however many people are looking:

| Call | For |
|---|---|
| `GET /v1/fleet` | Nodes (linked or not, version, last seen) and sessions with their summaries, and the handoffs that link them (`handoffFrom`, `handoffTo`). |
| `GET /v1/handoffs` | The newest 200 handoffs; `?since=<cursor>` the feed after it, oldest first, with `nextCursor` (`?since=` from the start). `/v1/handoffs/<id>`: one, with its note. |
| `GET /v1/events` | The newest hundred events and their deliveries. |
| `GET /v1/approvals` | Requests waiting for a person. |
| `GET /v1/windows` | Windowed roles and recent windows. |
| `GET /v1/readings` | Every reading's newest value; `?kind=a,b` for some kinds only. |
| `GET /v1/tools` | The tools each node shares (names, descriptions, `allow:` patterns, folder names; never paths). |
| `GET /v1/token` | What its token is (scope) and which tenant it shows. |

And for the People, Teams and Rules pages, as the signed-in person (`Fleetlayer-Person`, with the delegate token):
`GET /v1/people`, `POST /v1/people/<action>` (`invite`, `invite_revoke`, `member_role`, `member_remove`, `team_add`,
`team_remove`, `team_member`) and `GET /v1/policy`. They answer only for the tenant's admins, or the admin token.

`/v1/readings` and `/v1/tools` are new with the dashboard; against an older control plane those parts are just empty.
Each node publishes a **`node.status`** reading every minute while it's linked: its health (the same judgement as its
heartbeat) and each plugin's name, state, version, roles, restarts and problem, never a plugin's output, command line or
settings. A node older than 0.4 doesn't, so the dashboard shows its plugins as unknown.

## Monitoring

- `/healthz` answers 200 while the fleet can be read with the dashboard's token, and 503 with the reason when it
  can't (the control plane is down, or the token was removed).
- The heartbeat pushes the same result every minute, so a monitor alerts when the dashboard stops (no push) and when it
  runs but shows nothing (a failed push).
- The node shows the same problem in `fleetlayer status` and its own health, through the plugin's status report.

## Moving it

It keeps no state of its own: its settings are its section of `node.yaml` and its two secret files (the control plane
token and the sign-in client secret). `fleetlayer move dashboard <machine>` copies them, or do it by hand: stop it,
copy the section and the files, start it on the new node, and repoint the gateway if its address changed. Signed-in
people sign in again.
