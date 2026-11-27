# Network access per role

Status (28 November 2026): new, Linux only. `fleetlayer-egress` ships with Fleetlayer from the release after v0.3.12.

An agent that can read a secret can send it anywhere it can reach. Egress control narrows "anywhere" to the hosts a
role's work needs: a model API, GitHub, the package registries, the docs it reads. It works like Little Snitch for a
fleet of agents. The first connection to a new host gets a decision, and that decision is remembered. The rules belong
to a **role** on every machine, not to an app on one Mac. The question goes to your phone or your control plane
instead of a dialog on a screen nobody's watching, and the agent says why it needs the host.

It's off by default. Most fleets start without it, and an agent sets it up when you ask
([the guide](guides/egress.md)).

## How it works

A role is a Unix user with its own node ([Credentials](credentials.md#whats-coming)). For each restricted role:

1. **A firewall rule per Unix user.** nftables matches the role's users (`meta skuid`). Their connections to ports 443
   and 80 are redirected to the egress proxy, their DNS questions to its resolver, and once the role is enforced,
   everything else they send off the machine is refused: other ports, the LAN, the tailnet, UDP. Other users, yours
   included, are untouched.
2. **A proxy that reads host names and never decrypts.** For HTTPS it reads the server name the client puts in its
   TLS ClientHello, then passes the bytes on untouched. For plain HTTP it reads the `Host` header, and for tools that
   use `HTTPS_PROXY` it reads the `CONNECT` line. It connects to the host by name, resolving the name itself, so a
   client can't name an allowed host and reach a different address. A restricted role is also kept off private
   addresses (the LAN, the tailnet, cloud metadata at 169.254.169.254) unless you named that host exactly or listed
   the range.
3. **A resolver that refuses names off the list.** The role's DNS questions are logged, and names it may not reach
   get `REFUSED`, which closes DNS as a way to send data out in the names it looks up.
4. **An audit line per connection:** user, role, host, port, decision, bytes each way, and how long it lasted.
5. **Asking for a host.** A refused agent is told how to ask. Its request goes through Fleetlayer's
   [approvals](access-control.md#asking-the-person-approval-and-justification) as the role's node, with the agent's
   one-line reason. Once a person approves it, the host stays allowed for that role.

The proxy runs as a node plugin under a Unix user outside every restricted role (a user of its own is best), so the
agents it restricts can't stop or replace it. A small probe on each restricted role's node checks that the rules
really hold, and reports a problem the moment they don't.

## Log mode first

A role starts in `log` mode: nothing is refused, and what would be refused is recorded as `would_deny`. After a week
of real work, `fleetlayer-egress suggest` writes the list that work needed, as settings to review and merge. Then
switch the role to `enforce`.

## The settings

One file on the proxy's machine, `~/.config/fleetlayer/egress.yaml` under the proxy's user. It's read again within
five seconds of a change, so host lists need no restart. Only new roles, new users and new ports need the firewall
regenerated.

```yaml
mode: log                     # log (the default) or enforce, for every role without its own
ports: { proxy: 3128, tls: 3129, http: 3130, dns: 3153 }   # the defaults, on 127.0.0.1 and ::1
dns_upstream: [1.1.1.1:53]    # default: the nameservers in /etc/resolv.conf
keep_days: 30                 # audit log kept (default 30)
roles:
  triage:
    users: [triage]           # the role's Unix users
    node: server-triage       # the role's node: its agents ask for hosts as -@server-triage
    mode: enforce             # overrides the file's mode
    presets: [anthropic, github]
    hosts:
      - api.example.com       # that name, on 443 and 80
      - "*.docs.example.org"  # any name under it (not docs.example.org itself)
      - github.com:22         # another port: say which
      - fleet.example.net     # your control plane, if the role's node reaches it through here
    networks: []              # private ranges the role may reach (10.5.0.0/16): none by default
    loopback: true            # its own test servers on this machine (default true)
```

A host without a port allows 443 and 80. An entry with `*.` matches names under it at any depth. A private address
is reached only through a host named exactly (not by `*.`), a host a person approved by name, or a range in
`networks`. A name with no dot (`intranet`) isn't accepted: write the full name.

### Presets

`fleetlayer-egress presets` lists them:

| Preset | Hosts |
|---|---|
| `anthropic` | api.anthropic.com, claude.ai, console.anthropic.com, statsig.anthropic.com, mcp-proxy.anthropic.com |
| `openai` | api.openai.com, chatgpt.com, auth.openai.com |
| `google-ai` | generativelanguage.googleapis.com |
| `github` | github.com, \*.github.com, \*.githubusercontent.com, ghcr.io, \*.ghcr.io |
| `npm` | registry.npmjs.org, registry.yarnpkg.com, nodejs.org |
| `pypi` | pypi.org, files.pythonhosted.org |
| `crates` | crates.io, index.crates.io, static.crates.io, static.rust-lang.org, sh.rustup.rs |
| `go` | proxy.golang.org, sum.golang.org, go.dev |
| `rubygems` | rubygems.org, index.rubygems.org |
| `docs` | docs.rs, doc.rust-lang.org, developer.mozilla.org, docs.python.org, docs.github.com, docs.anthropic.com, docs.claude.com, pkg.go.dev |

Hosts change. If a tool needs one the preset lacks, the log shows it, and `suggest` proposes it.

## Asking for a host, and approving it

When a connection is refused, the agent sees:

- through `HTTPS_PROXY`: a `403` that says why and how to ask (`X-Fleetlayer-Egress: refused`);
- for HTTPS without a proxy: the TLS alert `access denied` (curl prints `tlsv1 alert access denied`);
- for a name: a DNS `REFUSED` (`Could not resolve host`).

`fleetlayer-egress why <host>` shows the agent its own recent decisions for a host, with the reason. To ask:

```sh
fleetlayer-egress ask api.vendor.io --why "the vendor's API for the invoice import"
fleetlayer-egress ask api.vendor.io --why "…" --wait 10    # wait up to ten minutes for the answer
```

The proxy asks the control plane whether `-@<role's node>` may `connect` to `host:api.vendor.io`. The policy decides
like any other request:

- Fleetlayer's default asks first ([access control](access-control.md#asking-the-person-approval-and-justification)):
  the request is published as `access.ask` on the `access` channel for your Inbox or phone, and you approve it with
  `fleetlayer-control approvals approve <id>`. The approval holds 30 days and renews while the host is used, so it
  lapses only after 30 days unused. The proxy also remembers it, so an approved host keeps working while the control
  plane is out of reach.
- A fleet with `ask: []` (no approvals) allows any host an agent asks for. Every request and its reason are still in
  the audit log. To keep asking for hosts while turning the rest off, list only these:
  `ask: [{ who: "*", can: connect, to: "host:*", for: 30d }]`.
- `deny` rules forbid hosts outright, and deny always wins, even over an approval:
  `deny: [{ who: "-@server-triage", can: connect, to: ["host:*.pastebin.com", "host:gist.github.com"] }]`.

A role whose only job is keeping the lists (an "allowlist keeper") subscribes to the `access` channel and approves or
refuses requests with an admin token (`POST /v1/approvals/<id>`). Give it nothing else.

The proxy checks a waiting request every minute for the first hour, then every ten minutes for a day. An approved host
in use is checked again once a day, which renews its approval and drops it if a person revoked it.

## Commands

For agents, on any restricted machine (they reach the proxy at `127.0.0.1:3128`, or `--proxy`/`FLEETLAYER_EGRESS_PROXY`):

| Command | What it does |
|---|---|
| `fleetlayer-egress status` | Your role, its mode, presets, hosts, approved hosts and requests waiting |
| `fleetlayer-egress ask <host> --why "…" [--wait <minutes>]` | Ask for a host (exit 0 allowed, 3 still waiting, 1 refused) |
| `fleetlayer-egress why [<host>]` | Your recent decisions, newest first |
| `fleetlayer-egress env` | The `HTTPS_PROXY` lines for a shell profile |

For the person, or the agent that sets it up, on the proxy's machine:

| Command | What it does |
|---|---|
| `fleetlayer-egress check [file]` | Read the settings and say what each role allows, with warnings |
| `fleetlayer-egress log [--days 1] [--role r] [--refused] [--limit 50] [--json]` | The audit log |
| `fleetlayer-egress suggest [--days 7] [--role r]` | The hosts a week of the log asks for, as settings to review |
| `fleetlayer-egress firewall [--apply \| --remove \| --unit]` | Print the nftables rules, apply or remove them (as root), or print a systemd unit that applies them at boot |
| `fleetlayer-egress presets` | The presets and their hosts |
| `fleetlayer-egress probe` | Run the probe's checks once, as the current user |

## The plugin

On the proxy's node (a Unix user outside every restricted role):

```yaml
plugins:
  - name: egress
    command: fleetlayer-egress
    grants: [access.check]      # to ask the control plane for hosts
    config:                     # all optional
      settings: ~/.config/fleetlayer/egress.yaml
      state_dir: ~/.local/state/fleetlayer/egress
```

On each restricted role's own node, the probe:

```yaml
plugins:
  - name: egress-probe
    command: fleetlayer-egress
    config:
      probe:
        allowed: api.anthropic.com   # a host on the role's list (default)
        blocked: example.com         # a host not on it (default)
        direct: 1.1.1.1:853          # an address that answers on a port the proxy never sees (default)
        every: 300                   # seconds (default)
```

## Watching it

Both plugins report through `plugin.status`, so the node's health check and heartbeat turn red, and your
[monitoring](guides/monitoring.md) hears about it:

- **The proxy** reports a settings file that doesn't parse (the last good settings keep deciding), DNS questions the
  upstream resolvers stopped answering, and running as a user that's in a restricted role.
- **The probe** checks the rules from the restricted role's side, as its user: the proxy answers and knows the role;
  an allowed host gets through; and once enforced, a blocked host is refused, its name doesn't resolve, and a direct
  connection past the proxy fails. It catches a firewall that was never applied or was lost at a reboot, a proxy
  that stopped, and anything that took the proxy's place and lets everything through.

## What it doesn't cover

- **An allowed host is still a way out.** A public gist, a push to a public repository, a paste in an issue: without
  decrypting there's no filtering by path, and Fleetlayer doesn't decrypt. Short-lived, narrowly scoped tokens are the
  defence ([Credentials](credentials.md)).
- **Root, and the services it runs, aren't restricted.** A restricted user mustn't have sudo, and mustn't be in the
  `docker` group: Docker's daemon runs as root, so containers it starts reach anywhere.
- **The shared browser** goes anywhere its person can. Its MCP calls pass through the sharing node's tool rules
  instead ([access control](access-control.md#tools-at-the-level-of-their-own-tools)).
- **Other ports go through `CONNECT` only.** Git over SSH, a database on 5432: list the host with its port, and point
  the tool at the proxy (for SSH, `ProxyCommand nc -X connect -x 127.0.0.1:3128 %h %p`), or use HTTPS where you can.
- **QUIC (HTTP/3 over UDP) is refused once enforced.** Browsers and clients fall back to TCP by themselves.
- **Encrypted Client Hello** hides the real server name behind a public one, so such a connection is decided by the
  public name, which won't be on your list. Clients fall back where it's refused.
- **One restricted user could take the proxy's port while the proxy is down.** The probe notices (a blocked host gets
  through), and reports it.
- **Linux only for now.** On a Mac, run the agents in a Linux VM ([guides to come](https://github.com/fleetlayer/fleetlayer/blob/main/docs/guides-to-write.md)).

## Prior art

[Little Snitch](https://www.obdev.at/products/littlesnitch/) and [LuLu](https://objective-see.org/products/lulu.html)
for "ask once, remember", [OpenSnitch](https://github.com/evilsocket/opensnitch) for a per-process firewall on Linux,
Stripe's [smokescreen](https://github.com/stripe/smokescreen) for a CONNECT proxy with per-role host lists, and
Claude Code's own sandbox, which allows network access by domain through a local proxy. Fleetlayer borrows ideas, not
code: LuLu and OpenSnitch are GPL.
