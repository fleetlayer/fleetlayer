# The node's link to the control plane

Each node keeps **one outbound WebSocket** to the control plane at `/v1/link`, so a node works behind NAT with no
port open on its machine. The node authenticates with its token as a bearer token (`Authorization: Bearer fln_…`);
the control plane stores only the token's SHA-256 hash. The link carries **JSON-RPC 2.0**, one message per text frame,
with requests in both directions (the same peer as plugins use, `fleetlayer-plugin`). The control plane pings each node
with `_fleetlayer.dev/ping` every 30 seconds and drops the link after three missed pings. The node drops a link it
has heard nothing on (no ping, no other frame) for 100 seconds, since a path that died without closing (a laptop that
slept, a NAT that forgot the connection) never says so, and it reconnects with backoff from 1 second up to a minute.

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
| `link/session_start` | request | `{node, tab?, cwd?, harness?, prompt?, resume?, model?, effort?, caller, justification?}` → `{node, tab, address, sessionId, promptError}`: start or resume a session on `node` (`fleetlayer session start\|resume --on`, or a plugin's `_fleetlayer.dev/session/start`). `caller` is one of the node's sessions (its tab), `-` for its shell, or `plugin:<name>` for one of its plugins; anything else is refused. Checked as `start` on `<tab>@<node>` (`<sessionId>@<node>` for a resume without a tab; asked by default for 10 minutes), with the approval bound to the whole spec (node, tab, folder, harness, model, effort, resumed session, and the prompt's hash), which the person is shown. A resume is also checked as `start` and `read` on the session it resumes, under every name it goes by. The policy's start limits (`starts_per_hour`, `starts_per_day`) are checked before anything is asked, then the start is counted; a refusal is audited, and a start the node refused or never got doesn't count. The prompt arrives rendered by the calling node; the control plane adds the outside-data notice when the caller reads untrusted input, then `node` gets `link/session_serve`. |
| `link/transcript_search` | request | `{node, text, limit?, perSession?, caller}` → `{node, hits, searched, partial?}`: every transcript on `node` searched for `text` (`fleetlayer transcript-search --on`), 10 a minute per caller; `node` gets `link/transcript_search_serve` `{text, limit: 500, perSession?, caller}`. Only hits in sessions the caller may read without asking come back, checked under every name the session goes by (ended sessions under the tabs they had), then the caller's limit applies; how many were left out isn't said. The control plane keeps none of it. |
| `link/session_handoff` | request | `{id, from, note?, node, tab, cwd, harness?, prompt?, model?, effort?, caller, justification?}` → `{handoff, node, tab, address, sessionId, promptError}`: start a session on `node` that takes over `from`'s work (`fleetlayer session handoff`). Checked as `start` on `<tab>@<node>`, and `read` on `from` when the caller isn't that session. The control plane keeps the note and the link (`/v1/fleet`: `handoffFrom`, `handoffTo`), then starts it as `link/session_start` does; a start that fails keeps nothing. |
| `link/handoff_get` | request | `{id, caller, justification?}` → `{id, from, fromSessionId?, to, toSessionId?, note?, by, createdAt}`: one handoff, for the session it went to or a caller with `read` on `from`. |
| `link/session_close`, `link/session_answer`, `link/session_command` | request | `{node, session, action: "close"} \| {node, session, action: "answer", prompt, answers} \| {node, session, action: "command", command}`, plus `caller` and `justification?` → `{node, sessionId, tab, done}` (`closed`, `answered`, `typed`). Checked as `close`, `answer` or `command` on `<session>@<node>` (asked by default for 10 minutes); each command is its own approval by default (`for: once`, bound to the command); what's about to be typed is written to the audit log before the node is asked, then its outcome. Then `node` gets `link/session_act_serve`, which also carries `action: "say"` for a person's `POST /v1/sessions/say`: a node can't ask for that one. |
| `link/tool_call` | request | `{streamId, targets, argsHash, detail?, justification?, peek?}` → `{decision, approval?, reason}`: the node serving a tool stream with `check_calls` asks about one call (the MCP gate, [access-control.md](access-control.md#the-mcp-gate)). The caller and tool are the stream's, as recorded at `link/tool_open`, and only the node serving it may ask. Each target is decided as `call` on `tool:<tool>/<target>@<node>`, audited, and filed for approval by an `ask:` rule (`for: once` binds the approval to `argsHash`; `detail` is shown to the person, never logged). `decision` is `allowed`, `denied` or `pending` (with `approval`). With `peek`, it only reports that request's status (`pending`, `ready` once approved, `denied`), recording nothing. |
| `link/tools` | notification | `{tools: [{name, description?, allow, notOffered?, folders?: [{name, upload?, delete?, trusted?}]}]}`: the tools this node shares, replacing its earlier list; sent on linking and when an opt-in starts or ends. Names, descriptions and folder names only, never paths or secrets. The control plane keeps it in memory, as a cache nodes rebuild. |
| `link/me` | request | `{caller}` → `{address, untrusted, justify, message, messageRefused, tools, pages, start, unknown, subscriptions, windows}`: what one of this node's sessions may use (`fleetlayer me`). Each item is decided as the request would be, without asking: no approval request filed or renewed, no audit entry, no event. Anything a rule refuses is left out, so a restricted agent learns neither the tool nor the rule's name. Tools and pages say `allowed` (with `until` when an approval holds), `ask`, `pending` (with `request`) or `refused`. `start` lists the nodes it may start sessions on, decided for a new tab (`start` on `-@<node>`, the node's own address): its own node always, others `allowed` or `ask`. Nodes that don't send `link/tools` are listed in `unknown`. `windows` lists the windowed roles it's one of the agents of or may ask for, with `state` (`open`, `pending`, `off`) and `endsAt`. |
| `link/role_activate` | request | `{role, for, caller, justification?}` → `{role, state, request?, endsAt?, message}`: ask for a role's window (`fleetlayer role activate`). Only the role's own agents and its `asked_by:` may; files an approval (verb `activate`, target `window:<role>`). `state` is `pending` or `open`. |
| `link/role_deactivate` | request | `{role, caller}` → `{role, state, message}`: close a role's window now; `state` is `closing` or `off` (none was open). |
| `link/role_windows` | request | `{caller}` → `{windows: [{role, state, endsAt?, id?, longestMs, member}]}` (`fleetlayer role list`). |
| `link/window` | notification, control plane → node | `{id, role, open, endsAt, member}`: a window opened or closed. A member node (one of the role's) closes its sessions, removes what was delivered to it and runs its `on_end` hook at the end, and keeps `endsAt` on disk to close the window by itself; a node watching the role (`windows:` in its config, sent as `windows` in `link/hello`) runs its hooks. Sent again when a node links, for open windows and closed ones it hasn't confirmed (however long ago), and once a minute to linked nodes that haven't confirmed. |
| `link/window_closed` | request | `{id, problem?}` → `{}`: a node a window concerns (one of the role's, or one watching it) confirms it closed the window, or says what didn't work (kept, and shown in `/healthz`, until a clean confirmation). |
| `link/summary` | notification | `{event}`: a session summary from the node's summaries plugin (its `_fleetlayer.dev/summary/publish`), stored by the control plane. |
| `link/summaries_search` | request | `{q, limit?, session?, mentions?}` → `{summaries}`: summaries matching `q` (20 by default, 200 at most), one session's timeline with `session`, or those that mention a name with `mentions`. Only sessions the node may read, and its own, come back. |
| `link/tool_open` | request | `{tool, node, caller, justification?}` → `{streamId}`: open a stream to a tool `node` shares (`fleetlayer mcp`). Checked as `use_tool` on `tool:<tool>@<node>`; then `node` gets `link/tool_serve`. Both sides connect to `/v1/stream/<streamId>` with their node token; a stream nobody opens within a minute is forgotten. |
| `link/command_open` | request | `{command, node, caller, justification?}` → `{streamId}`: open a stream to run a command `node` shares (`fleetlayer run`). Refused here only when `run` on `command:<name>@<node>` is denied outright; the run itself is decided with `link/command_call`. Then `node` gets `link/command_serve`. |
| `link/command_call` | request | `{streamId, argsHash, detail?, audit?, justification?, peek?}` → `{decision, approval?, reason}`: the node serving a command stream asks about one run, as `link/tool_call` does for a tool call. Decided as `run` on `command:<name>@<node>`, audited with `audit` (the program and arguments, files by name, size and hash), and asked `for: once` by default. |
| `link/terminal_open` | request | `{session, node, caller, mode, cols, rows, justification?}` → `{streamId}`: open a stream to a session's terminal on `node`. Checked as `read` on `<session>@<node>` for `observe`, `attach` for `input` and `control`; then `node` gets `link/terminal_serve`. |
| `link/transcript_read` | request | `{node, session, caller, justification?}` plus the range fields of `_fleetlayer.dev/transcript/read` (protocol section 10) → `{sessionId, format, size, offset, next, data, cut?, skipped?}`. Checked as `read` on `<session>@<node>`; then `node` gets `link/transcript_serve`. The control plane passes the bytes on and keeps none. |
| `link/sync_publish` | request | `{bundle, version, copies: [{node, sealed}]}` → `{results: [{target, ok, error?}]}`: a bundle sealed to each target's key (protocol section 8). Each copy is checked as `sync` on `sync:<bundle>@<target>` for `-@<node>`, then sent to the target as `link/sync_deliver`. A target that's offline is noted, and gets the bundle when it's back (`link/sync_wanted`). The control plane keeps no copy. |
| `link/readings` | notification | `{readings}`: readings the node's plugins published (protocol section 9). |
| `link/reading_subscribe` | request | `{kinds}` → `{readings}`: the reading kinds this node's plugins want, replacing its earlier list, answered with the newest of each it may read. |
| `link/reading_latest` | request | `{kinds}` → `{readings}`: the newest readings of `kinds` this node may read, without changing its subscription (`fleetlayer readings`). |

A session is the node's registry entry: `{sessionId, manager, harnessPlugin, cwd, title, updatedAt, meta, ended}`,
where `meta` is the `fleetlayer.dev/session` object from protocol section 4.

## Control plane → node

| Method | Params → result |
|---|---|
| `link/deliver` | `{name, deliveryId, kind, subkind?, untrusted, from, text}` → `{via}`. The node resolves `name` (a tab or a session ID) among its own sessions and delivers as protocol section 5 says. |
| `link/pages_serve` | `{streamId, space, name, new?, date?, caller}` → `{}`. The node runs its pages plugin's `receive` command on stream `streamId` (`docs/plugins/pages.md`, "From another machine"). |
| `link/session_serve` | `{tab?, cwd?, harness?, prompt?, resume?, caller}` → `{node, tab, address, sessionId, promptError}`. The node starts the session as [protocol.md](protocol.md#starting-sessions) describes, within two and a half minutes. |
| `link/session_act_serve` | `{session, action, prompt?, answers?, command?, text?, caller}` → `{node, sessionId, tab, done}`. The node closes the session, answers its prompt after checking on screen that the same prompt shows (`-32015` when not), types an allowed slash command (`command`), or types what a person said (`say`). |
| `link/tool_serve` | `{streamId, tool, caller}` → `{}`. The node starts the shared tool for `caller` and connects it to stream `streamId`. |
| `link/command_serve` | `{streamId, command, caller, justification?}` → `{}`. The node takes one run of a shared command from stream `streamId` (`docs/guides/shared-commands.md`). |
| `link/terminal_serve` | `{streamId, session, mode, cols, rows, caller}` → `{}`. The node streams the session's terminal into stream `streamId` (protocol section 4, "Terminals"). |
| `link/transcript_serve` | `{session, caller}` plus the range fields → `{sessionId, format, size, offset, next, data, cut?, skipped?}`: part of one of its sessions' transcripts, for a caller. |
| `link/transcript_search_serve` | `{text, limit, perSession?, caller}` → `{node, hits, searched, partial?}`: search its transcripts for a caller (see `link/transcript_search`). |
| `link/catchup` | `{sessionId}` → the session's catch-up from its summaries plugin, for `POST /v1/sessions/<id>/catchup`. |
| `link/sync_deliver` | `{bundle, version, from, sealed}` → `{applied}`, or `{reason}` when it was deliberately not applied, or `{error}`: a sealed bundle for this node, applied by its `sync_target` plugins. |
| `link/sync_wanted` | notification, `{bundle, node}`: a target that missed a bundle is back, or its sync was just approved: publish the bundle again for it. |
| `link/reading_update` | notification, `{readings}`: new readings of the kinds this node subscribed to, from any node it may read. |
| `link/moved` | notification, `{url, proof}`: the control plane has moved to this `ws://` or `wss://` link URL (`fleetlayer move control`). It's sent as the old copy stops. The node sends its token there only after `GET <web address>/v1/move-proof?nonce=<proof>` answers 200 (the new copy holds the old one's folder), never follows `wss://` down to `ws://`, keeps its old address as the fallback, and saves the new one as `control.url` in node.yaml once that answer says `"confirmed": true` (the move passed its health check). If the move isn't confirmed within an hour, the node stops trying the new address. |

## Access control

The tenant's access policy decides each request, as [access-control.md](access-control.md) describes: who is a
`<tab>@<node>` (a sender with no session is `-@<node>`, a plugin `plugin:<name>@<node>`), each method's verb and
target are given in the tables above, and deny wins. `link/send` is decided as `message` on the target session, and a
session may always answer a session that messaged it in the last 24 hours (the reply window). `link/fleet` shows a
node its own sessions and those its `-@<node>` may `read`. A tenant with no policy of its own uses the built-in
default, which is a version 2 policy too. A tenant still on the older version 1 rules (ordered first-match rules on
`<tab>@<node>` patterns, managed with `fleetlayer-control acl list|add|remove|default|check`) decides `link/send` by
those rules, and `link/fleet` shows the sessions the node may message. A node may subscribe and unsubscribe only its
own sessions. Every refusal is also published as an `access.denied` event on the `access` channel (kept in the event
log; subscribe a session or set the channel's fallback to hear about them).

## Routing

- **Messages** between nodes: the control plane forwards `link/send` to the target node as `link/deliver`. A target
  node that isn't linked answers at once with "node X is offline". Nothing is queued.
- **Events** (webhooks, plugins' events): stored, then delivered to every subscription on the channel (whole-channel
  subscriptions and those to the event's subject), or to the channel's fallback when none match. Each event is kept
  with its delivery results. A repeated `dedupeKey` on the same channel goes nowhere.
- **Webhooks** arrive on the control plane's second listener, `POST /hooks/<endpoint id>/<secret>`, the only one that
  needs to face the internet. GitHub webhooks get the subject `<owner/repo>@<branch>`. Alertmanager's (known by its
  body, or `?kind=alertmanager`) become one `alertmanager.alert` event per alert, `[FIRING] HighMemory (warning,
  web): Memory above 90%`, with the alert's `alertname` as subject (another label with `?subject_label=`, or a fixed
  `?subject=`) and a dedupe key from its fingerprint, status and start, so a repeated notification goes nowhere.
  Anything else uses its `summary`/`text`/`message` field, or `?subject=` and `?kind=` in the URL.
- **Missed events:** every event is kept with its delivery results, so a session that was offline reads what it missed
  with `fleetlayer events` (`link/events`).
