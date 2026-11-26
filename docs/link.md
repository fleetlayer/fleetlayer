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
| `link/hello` | request, first | `{protocol, version, machine, user}` → `{node}` (the name the token belongs to). A protocol the control plane doesn't speak is refused. |
| `link/sessions` | notification | `{sessions: [...]}`: every session on the node, replacing what the control plane had |
| `link/session` | notification | `{session}`: one session changed |
| `link/session_gone` | notification | `{sessionId}` |
| `link/send` | request | `{to: "<name>@<node>", deliveryId, from, text}` → `{via}`. The sending node renders `text` (header and reply line). |
| `link/publish` | request | an event (protocol section 6) → `{duplicate, deliveries: [{target, ok, error?}]}` |
| `link/subscribe`, `link/unsubscribe` | request | `{channel, subject?, target: "<name>@<node>"}` |
| `link/subscriptions` | request | `{}` → `{subscriptions}` |
| `link/fleet` | request | `{}` → `{nodes: [{name, machine, online, ...}], sessions: [{node, ...}]}` |

A session is the node's registry entry: `{sessionId, manager, harnessPlugin, cwd, title, updatedAt, meta, ended}`,
where `meta` is the `fleetlayer.dev/session` object from protocol section 4.

## Control plane → node

| Method | Params → result |
|---|---|
| `link/deliver` | `{name, deliveryId, kind, subkind?, untrusted, from, text}` → `{via}`. The node resolves `name` (a tab or a session ID) among its own sessions and delivers as protocol section 5 says. |

## Access control

Ordered rules on `<tab>@<node>` patterns (`*` matches any run of characters; a sender with no session is `-@<node>`),
first match wins, then the tenant's default (`allow` on a new control plane). A plugin sending a message is
`plugin:<name>@<node>`, so a rule can give a runner that carries out only approved actions more reach than the agent on
the same node. A session may always answer a session that messaged it in the last 24 hours (the reply window), so an
agent limited to a short list can still reply to whoever wrote. They decide `link/send`, and which
sessions `link/fleet` shows a node (those its `-@<node>` may message, plus its own). A node may subscribe and
unsubscribe only its own sessions. Managed with `fleetlayer-control acl list|add|remove|default|check`.

## Routing

- **Messages** between nodes: the control plane forwards `link/send` to the target node as `link/deliver`. A target
  node that isn't linked answers at once with "node X is offline". Nothing is queued.
- **Events** (webhooks, plugins' events): stored, then delivered to every subscription on the channel (whole-channel
  subscriptions and those to the event's subject), or to the channel's fallback when none match. Each event is kept
  with its delivery results. A repeated `dedupeKey` on the same channel goes nowhere.
- **Webhooks** arrive on the control plane's second listener, `POST /hooks/<endpoint id>/<secret>`, the only one that
  needs to face the internet. GitHub webhooks get the subject `<owner/repo>@<branch>`; anything else uses its
  `summary`/`text`/`message` field, or `?subject=` and `?kind=` in the URL.
