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
| `link/publish` | request | an event (protocol section 6), plus `caller: "plugin:<name>"` when one of the node's plugins sent it → `{duplicate, deliveries: [{target, ok, error?}]}`. Checked as `publish` on `channel:<channel>`, for `plugin:<name>@<node>` or else `-@<node>`. |
| `link/events` | request | `{target, channel?, all?, peek?, limit?}` → `{events: [{id, channel, kind, subject, summary, body, source, at, delivered, error}]}`: the events whose delivery to `target` (one of the node's own sessions) failed since it last asked, oldest first, at most 7 days back; `all` adds those delivered; the mark moves to the newest shown unless `peek` (`fleetlayer events`). |
| `link/subscribe`, `link/unsubscribe` | request | `{channel, subject?, target: "<name>@<node>"}` |
| `link/subscriptions` | request | `{}` → `{subscriptions}` |
| `link/fleet` | request | `{}` → `{nodes: [{name, machine, online, ...}], sessions: [{node, ...}]}` |
| `link/access_check` | request | `{tenant, who, can, to}` → `{allowed, rules, reason}`, for a plugin's `_fleetlayer.dev/access/check`. Refused unless the node's `-@<node>` is a member of `tenant`; decided and audited in that tenant ([`sso.md`](sso.md)). |
| `link/pages_spaces` | notification | `{spaces: [name]}`: the pages spaces this node's plugins host, replacing its earlier list; sent on linking and on every change. A space another linked node already hosts stays with that node, and the control plane logs an error. |
| `link/pages_open` | request | `{space, name, caller, new?, date?, justification?}` → `{streamId}`: push a page (`fleetlayer pages push`). Checked as `write` on `page:<space>/<name>` (agents are asked by default, approved per space); then the space's node gets `link/pages_serve`. |
| `link/session_start` | request | `{node, tab?, cwd?, harness?, prompt?, resume?, model?, effort?, caller, justification?}` → `{node, tab, address, sessionId, promptError}`: start or resume a session on `node` (`fleetlayer session start\|resume --on`). Checked as `start` on `<tab>@<node>` (`<sessionId>@<node>` for a resume without a tab; asked by default for 10 minutes). The prompt arrives rendered by the calling node; the control plane adds the outside-data notice when the caller reads untrusted input, then `node` gets `link/session_serve`. |
| `link/transcript_search` | request | `{node, text, limit?, perSession?, caller}` → `{node, hits, searched, partial?}`: every transcript on `node` searched for `text` (`fleetlayer transcript-search --on`), 10 a minute per caller; `node` gets `link/transcript_search_serve` `{text, limit: 500, perSession?, caller}`. Only hits in sessions the caller may read without asking come back, checked under every name the session goes by (ended sessions under the tabs they had), then the caller's limit applies; how many were left out isn't said. The control plane keeps none of it. |
| `link/session_handoff` | request | `{id, from, note?, node, tab, cwd, harness?, prompt?, model?, effort?, caller, justification?}` → `{handoff, node, tab, address, sessionId, promptError}`: start a session on `node` that takes over `from`'s work (`fleetlayer session handoff`). Checked as `start` on `<tab>@<node>`, and `read` on `from` when the caller isn't that session. The control plane keeps the note and the link (`/v1/fleet`: `handoffFrom`, `handoffTo`), then starts it as `link/session_start` does; a start that fails keeps nothing. |
| `link/handoff_get` | request | `{id, caller, justification?}` → `{id, from, fromSessionId?, to, toSessionId?, note?, by, createdAt}`: one handoff, for the session it went to or a caller with `read` on `from`. |
| `link/session_close`, `link/session_answer`, `link/session_command` | request | `{node, session, action: "close"} \| {node, session, action: "answer", prompt, answers} \| {node, session, action: "command", command}`, plus `caller` and `justification?` → `{node, sessionId, tab, done}` (`closed`, `answered`, `typed`). Checked as `close`, `answer` or `command` on `<session>@<node>` (asked by default for 10 minutes); each command is its own approval by default (`for: once`, bound to the command); what's about to be typed is written to the audit log before the node is asked, then its outcome. Then `node` gets `link/session_act_serve`, which also carries `action: "say"` for a person's `POST /v1/sessions/say`: a node can't ask for that one. |
| `link/tool_call` | request | `{streamId, targets, argsHash, detail?, justification?, peek?}` → `{decision, approval?, reason}`: the node serving a tool stream with `check_calls` asks about one call (the MCP gate, [access-control.md](access-control.md#the-mcp-gate)). The caller and tool are the stream's, as recorded at `link/tool_open`, and only the node serving it may ask. Each target is decided as `call` on `tool:<tool>/<target>@<node>`, audited, and filed for approval by an `ask:` rule (`for: once` binds the approval to `argsHash`; `detail` is shown to the person, never logged). `decision` is `allowed`, `denied` or `pending` (with `approval`). With `peek`, it only reports that request's status (`pending`, `ready` once approved, `denied`), recording nothing. From v0.3.13. |
| `link/tools` | notification | `{tools: [{name, description?, allow, notOffered?, folders?: [{name, upload?, delete?, trusted?}]}]}`: the tools this node shares, replacing its earlier list; sent on linking and when an opt-in starts or ends. Names, descriptions and folder names only, never paths or secrets. The control plane keeps it in memory, as a cache nodes rebuild. From v0.3.9. |
| `link/me` | request | `{caller}` → `{address, untrusted, justify, message, messageRefused, tools, pages, start, unknown, subscriptions, windows}`: what one of this node's sessions may use (`fleetlayer me`). Each item is decided as the request would be, without asking: no approval request filed or renewed, no audit entry, no event. Anything a rule refuses is left out, so a restricted agent learns neither the tool nor the rule's name. Tools and pages say `allowed` (with `until` when an approval holds), `ask`, `pending` (with `request`) or `refused`. `start` lists the nodes it may start sessions on, decided for a new tab (`start` on `-@<node>`, the node's own address): its own node always, others `allowed` or `ask`. Nodes older than v0.3.9, which don't send `link/tools`, are listed in `unknown`. `windows` lists the windowed roles it's one of the agents of or may ask for, with `state` (`open`, `pending`, `off`) and `endsAt`. From v0.3.9. |
| `link/role_activate` | request | `{role, for, caller, justification?}` → `{role, state, request?, endsAt?, message}`: ask for a role's window (`fleetlayer role activate`). Only the role's own agents and its `asked_by:` may; files an approval (verb `activate`, target `window:<role>`). `state` is `pending` or `open`. |
| `link/role_deactivate` | request | `{role, caller}` → `{role, state, message}`: close a role's window now; `state` is `closing` or `off` (none was open). |
| `link/role_windows` | request | `{caller}` → `{windows: [{role, state, endsAt?, id?, longestMs, member}]}` (`fleetlayer role list`). |
| `link/window` | notification, control plane → node | `{id, role, open, endsAt, member}`: a window opened or closed. A member node (one of the role's) closes its sessions, removes what was delivered to it and runs its `on_end` hook at the end, and keeps `endsAt` on disk to close the window by itself; a node watching the role (`windows:` in its config, sent as `windows` in `link/hello`) runs its hooks. Sent again when a node links, for open windows and closed ones it hasn't confirmed (however long ago), and once a minute to linked nodes that haven't confirmed. |
| `link/window_closed` | request | `{id, problem?}` → `{}`: a node a window concerns (one of the role's, or one watching it) confirms it closed the window, or says what didn't work (kept, and shown in `/healthz`, until a clean confirmation). |

A session is the node's registry entry: `{sessionId, manager, harnessPlugin, cwd, title, updatedAt, meta, ended}`,
where `meta` is the `fleetlayer.dev/session` object from protocol section 4.

## Control plane → node

| Method | Params → result |
|---|---|
| `link/deliver` | `{name, deliveryId, kind, subkind?, untrusted, from, text}` → `{via}`. The node resolves `name` (a tab or a session ID) among its own sessions and delivers as protocol section 5 says. |
| `link/pages_serve` | `{streamId, space, name, new?, date?, caller}` → `{}`. The node runs its pages plugin's `receive` command on stream `streamId` (`docs/plugins/pages.md`, "From another machine"). |
| `link/session_serve` | `{tab?, cwd?, harness?, prompt?, resume?, caller}` → `{node, tab, address, sessionId, promptError}`. The node starts the session as [protocol.md](protocol.md#starting-sessions) describes, within two and a half minutes. |
| `link/session_act_serve` | `{session, action, prompt?, answers?, caller}` → `{node, sessionId, tab, done}`. The node closes the session, or answers its prompt after checking on screen that the same prompt shows (`-32015` when not). |
| `link/moved` | notification, `{url, proof}`: the control plane has moved to this `ws://` or `wss://` link URL (`fleetlayer move control`). It's sent as the old copy stops. The node sends its token there only after `GET <web address>/v1/move-proof?nonce=<proof>` answers 200 (the new copy holds the old one's folder), never follows `wss://` down to `ws://`, keeps its old address as the fallback, and saves the new one as `control.url` in node.yaml once that answer says `"confirmed": true` (the move passed its health check). If the move isn't confirmed within an hour, the node stops trying the new address. From v0.3.13. |

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
  needs to face the internet. GitHub webhooks get the subject `<owner/repo>@<branch>`. Alertmanager's (known by its
  body, or `?kind=alertmanager`) become one `alertmanager.alert` event per alert, `[FIRING] HighMemory (warning,
  talira): Memory above 90%`, with the alert's `alertname` as subject (another label with `?subject_label=`, or a fixed
  `?subject=`) and a dedupe key from its fingerprint, status and start, so a repeated notification goes nowhere.
  Anything else uses its `summary`/`text`/`message` field, or `?subject=` and `?kind=` in the URL.
- **Missed events:** every event is kept with its delivery results, so a session that was offline reads what it missed
  with `fleetlayer events` (`link/events`).
