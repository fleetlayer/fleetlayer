# The node's link to the control plane

Status: draft 1 (26 November 2026), as implemented in `fleetlayer-node` and `fleetlayer-control`.

Each node keeps **one outbound WebSocket** to the control plane at `/v1/link`, so a node works behind NAT with no
port open on its machine. The node authenticates with its token as a bearer token (`Authorization: Bearer fln_…`);
the control plane stores only the token's SHA-256 hash. The link carries **JSON-RPC 2.0**, one message per text frame,
with requests in both directions (the same peer as plugins use, `fleetlayer-plugin`). Each side pings the other with
`_fleetlayer.dev/ping` every 30 seconds and drops the link after three missed pings; the node reconnects with backoff
up to a minute.

## Node → control plane

| Method | Kind | Params → result |
|---|---|---|
| `link/hello` | request, first | `{protocol, version, machine, user, publicKey?, windows?}` → `{node}` (the name the token belongs to). A protocol the control plane doesn't speak is refused. `windows` names the roles whose windows the node watches (its config's `windows:`). |
| `link/sessions` | notification | `{sessions: [...]}`: every session on the node, replacing what the control plane had |
| `link/session` | notification | `{session}`: one session changed |
| `link/session_gone` | notification | `{sessionId}` |
| `link/send` | request | `{to: "<name>@<node>", deliveryId, from, text}` → `{via}`. The sending node renders `text` (header and reply line). |
| `link/publish` | request | an event (protocol section 6) → `{duplicate, deliveries: [{target, ok, error?}]}` |
| `link/subscribe`, `link/unsubscribe` | request | `{channel, subject?, target: "<name>@<node>"}` |
| `link/subscriptions` | request | `{}` → `{subscriptions}` |
| `link/fleet` | request | `{}` → `{nodes: [{name, machine, online, ...}], sessions: [{node, ...}]}` |
| `link/access_check` | request | `{tenant, who, can, to}` → `{allowed, rules, reason}`, for a plugin's `_fleetlayer.dev/access/check`. Refused unless the node's `-@<node>` is a member of `tenant`; decided and audited in that tenant ([`sso.md`](sso.md)). |
| `link/pages_spaces` | notification | `{spaces: [name]}`: the pages spaces this node's plugins host, replacing its earlier list; sent on linking and on every change. A space another linked node already hosts stays with that node, and the control plane logs an error. |
| `link/pages_open` | request | `{space, name, caller, new?, date?, justification?}` → `{streamId}`: push a page (`fleetlayer pages push`). Checked as `write` on `page:<space>/<name>` (agents are asked by default, approved per space); then the space's node gets `link/pages_serve`. |
| `link/session_start` | request | `{node, tab?, cwd?, harness?, prompt?, resume?, caller, justification?}` → `{node, tab, address, sessionId, promptError}`: start or resume a session on `node` (`fleetlayer session start\|resume --on`). Checked as `start` on `<tab>@<node>` (`<sessionId>@<node>` for a resume without a tab; asked by default for 10 minutes). The prompt arrives rendered by the calling node; the control plane adds the outside-data notice when the caller reads untrusted input, then `node` gets `link/session_serve`. |
| `link/session_close`, `link/session_answer` | request | `{node, session, action: "close"} \| {node, session, action: "answer", prompt, answers}`, plus `caller` and `justification?` → `{node, sessionId, tab, done}`. Checked as `close` or `answer` on `<session>@<node>` (asked by default for 10 minutes); an answer that went in is written to the audit log with its picks. Then `node` gets `link/session_act_serve`. |
| `link/tools` | notification | `{tools: [{name, description?, allow, notOffered?, folders?: [{name, upload?, delete?, trusted?}]}]}`: the tools this node shares, replacing its earlier list; sent on linking and when an opt-in starts or ends. Names, descriptions and folder names only, never paths or secrets. The control plane keeps it in memory, as a cache nodes rebuild. From v0.3.9. |
| `link/me` | request | `{caller}` → `{address, untrusted, justify, message, messageRefused, tools, pages, start, unknown, subscriptions, windows}`: what one of this node's sessions may use (`fleetlayer me`). Each item is decided as the request would be, without asking: no approval request filed or renewed, no audit entry, no event. Anything a rule refuses is left out, so a restricted agent learns neither the tool nor the rule's name. Tools and pages say `allowed` (with `until` when an approval holds), `ask`, `pending` (with `request`) or `refused`. `start` lists the nodes it may start sessions on, decided for a new tab (`start` on `-@<node>`, the node's own address): its own node always, others `allowed` or `ask`. Nodes older than v0.3.9, which don't send `link/tools`, are listed in `unknown`. `windows` lists the windowed roles it's one of the agents of or may ask for, with `state` (`open`, `pending`, `off`) and `endsAt`. From v0.3.9. |
| `link/role_activate` | request | `{role, for, caller, justification?}` → `{role, state, request?, endsAt?, message}`: ask for a role's window (`fleetlayer role activate`). Only the role's own agents and its `asked_by:` may; files an approval (verb `activate`, target `window:<role>`). `state` is `pending` or `open`. |
| `link/role_deactivate` | request | `{role, caller}` → `{role, state, message}`: close a role's window now; `state` is `closing` or `off` (none was open). |
| `link/role_windows` | request | `{caller}` → `{windows: [{role, state, endsAt?, id?, longestMs, member}]}` (`fleetlayer role list`). |
| `link/window` | notification, control plane → node | `{id, role, open, endsAt, member}`: a window opened or closed. A member node (one of the role's) closes its sessions, removes what was delivered to it and runs its `on_end` hook at the end, and keeps `endsAt` on disk to close the window by itself; a node watching the role (`windows:` in its config, sent as `windows` in `link/hello`) runs its hooks. Sent again when a node links, for open windows and closed ones it hasn't confirmed. |
| `link/window_closed` | request | `{id, problem?}` → `{}`: a member node confirms it closed a window, or says what didn't work (kept, and shown in `/healthz`, until a clean confirmation). |

A session is the node's registry entry: `{sessionId, manager, harnessPlugin, cwd, title, updatedAt, meta, ended}`,
where `meta` is the `fleetlayer.dev/session` object from protocol section 4.

## Control plane → node

| Method | Params → result |
|---|---|
| `link/deliver` | `{name, deliveryId, kind, subkind?, untrusted, from, text}` → `{via}`. The node resolves `name` (a tab or a session ID) among its own sessions and delivers as protocol section 5 says. |
| `link/pages_serve` | `{streamId, space, name, new?, date?, caller}` → `{}`. The node runs its pages plugin's `receive` command on stream `streamId` (`docs/plugins/pages.md`, "From another machine"). |
| `link/session_serve` | `{tab?, cwd?, harness?, prompt?, resume?, caller}` → `{node, tab, address, sessionId, promptError}`. The node starts the session as [protocol.md](protocol.md#starting-sessions) describes, within two and a half minutes. |
| `link/session_act_serve` | `{session, action, prompt?, answers?, caller}` → `{node, sessionId, tab, done}`. The node closes the session, or answers its prompt after checking on screen that the same prompt shows (`-32015` when not). |

## Access control

Ordered rules on `<tab>@<node>` patterns (`*` matches any run of characters; a sender with no session is `-@<node>`),
first match wins, then the tenant's default (`allow` on a new control plane). A plugin sending a message is
`plugin:<name>@<node>`, so a rule can give a runner that carries out only approved actions more reach than the agent on
the same node. A session may always answer a session that messaged it in the last 24 hours (the reply window), so an
agent limited to a short list can still reply to whoever wrote. They decide `link/send`, and which
sessions `link/fleet` shows a node (those its `-@<node>` may message, plus its own). A node may subscribe and
unsubscribe only its own sessions. Every refusal is also published as an `access.denied` event on the `access`
channel (kept in the event log; subscribe a session or set the channel's fallback to hear about them). Managed with `fleetlayer-control acl list|add|remove|default|check`.

## Routing

- **Messages** between nodes: the control plane forwards `link/send` to the target node as `link/deliver`. A target
  node that isn't linked answers at once with "node X is offline". Nothing is queued.
- **Events** (webhooks, plugins' events): stored, then delivered to every subscription on the channel (whole-channel
  subscriptions and those to the event's subject), or to the channel's fallback when none match. Each event is kept
  with its delivery results. A repeated `dedupeKey` on the same channel goes nowhere.
- **Webhooks** arrive on the control plane's second listener, `POST /hooks/<endpoint id>/<secret>`, the only one that
  needs to face the internet. GitHub webhooks get the subject `<owner/repo>@<branch>`; anything else uses its
  `summary`/`text`/`message` field, or `?subject=` and `?kind=` in the URL.
