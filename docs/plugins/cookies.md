# fleetlayer-cookies

Command-line tools that act as you on a website (an X client, `yt-dlp` for a site you pay for, a script that calls a
dashboard's internal API) usually take your browser's cookies. On your laptop they read Chrome's cookie store directly.
An agent on a server has no browser signed in as you, so those tools don't work there.

`fleetlayer-cookies` fixes that. On the machine with your browser, it reads the cookies of the sites you list and
sends them, encrypted to each target machine's key: from your running Chrome itself, with your approval for each read
(the default on macOS), or from Chrome's cookie file whenever they change (the default on Linux). On each target it writes them where the tool
looks: a `cookies.txt`, a JSON file, or an env file such as `AUTH_TOKEN=…` and `CT0=…`. It ships with Fleetlayer as
`fleetlayer-cookies`.

> **Whoever can read those files can act as you on those sites**, until the cookies expire or you sign out. Send them
> only to machines whose agents should be signed in as you, and only the cookies a tool needs (`names:`). When an agent
> only needs to look at a page or click through it, give it [your browser](../guides/browser.md) instead: nothing is
> copied, and the cookies never leave your laptop.

## Where the cookies come from

A share's `source:` says how the plugin gets them:

- **`browser`, the default on macOS:** it asks your running, signed-in Chrome itself, through the Playwright MCP
  extension that [your shared browser](../guides/browser.md) already uses. It needs no Keychain item and no privacy
  grant. It reads only when a target needs fresh cookies: its first delivery, when a target asks
  (`fleetlayer-cookies request`), or every `refresh_days`. **Each read waits for your approval**, which says which
  cookies, which sites, which target and why. It returns only the cookies you name.
- **`file`, the default on Linux:** it reads Chrome's cookie database and decrypts it (Chrome, Chromium, Brave and
  Edge). On Linux that needs nothing from you. It checks every few minutes and sends whenever the cookies change, with
  no approval per read.

Why not the file on macOS: Chrome keeps its key in your Keychain, and since macOS 27 reading Chrome's folder needs
Full Disk Access, which opens every file you have to the program that holds it. The `browser` source needs neither.
You can still choose `source: file` on a Mac if you want it, and give those two grants. The steps are under
[the file source on macOS](#the-file-source-on-macos).

### How a browser read works

1. A target is due: its first delivery, a `fleetlayer-cookies request <set> --why "…"` run there (the request travels in
   its `cookies.receive` reading), or `refresh_days` since the last read.
2. The source asks the control plane for approval, as verb `sync` on `sync:cookies-from-browser/<set>@<target>`, with
   a reason such as "read auth_token, ct0 for x.com from Chrome on laptop and send them to server, for it asked: bird
   says signed out". Fleetlayer's default asks for every read (`for: once`). It waits until you approve, and does
   nothing if you refuse.
3. Once you approve, it starts its own Playwright MCP with the `storage` capability and calls `browser_cookie_list`
   for each domain. That opens one blank tab in a tab group named "fleetlayer cookies", which it closes, then it stops
   the process. Every cookie of the listed domains reaches the plugin's process on your machine, which keeps only the
   named ones. Only those leave the machine, sealed to the target.
4. The delivery is an ordinary sync of `cookies/<set>`: approved once per target, then checked and audited each time.

**Keep that ask in your own policy.** A fleet that turns approvals off (`ask: []`, or asks only for some things) drops
Fleetlayer's default rule too. If you still want to approve each browser read, list it:

```yaml
ask:
  - { who: "*", can: [sync], to: ["sync:cookies-from-browser/*"], for: once }
```

The storage capability stays the plugin's own. The node refuses to share any tool started with it (`--caps storage`
or `PLAYWRIGHT_MCP_CAPS`): through the shared browser tool, every agent allowed the browser could read and change every
cookie.

`browser_cookie_list` gives each cookie's name, value, domain and path, but not its expiry or flags. So a
browser-read cookie has no expiry: the target can tell a cookie is missing, not that it's about to expire. When a tool
says you're signed out, the target asks for a fresh read.

### The file source on macOS

Two grants, once each, both for `fleetlayer-cookies` alone (never the node, so no other plugin or tool gets them).
Release builds are signed with a fixed identity (`dev.fleetlayer.cookies`), so both answers hold across upgrades.

1. **The browser's key.** The first time the plugin reads "Chrome Safe Storage" from your login Keychain, macOS asks:
   type your login password and choose **Always Allow**. If you click **Deny**, the plugin reports it and doesn't ask
   again until it restarts.
2. **The cookie file.** Since macOS 27, reading Chrome's folder is blocked without asking ("Data Access Blocked"). Open
   System Settings, Privacy & Security, **Full Disk Access**, click **+**, press Cmd-Shift-G, paste
   `~/.local/lib/fleetlayer/fleetlayer-cookies` and turn it on. That gives this program every file you have, so weigh
   it against the browser source.

The grant goes to `fleetlayer-cookies` alone because the plugin starts itself again as its own *responsible process*.
Otherwise macOS would hold the node responsible for what it reads. `fleetlayer-cookies responsible` checks this: it
should say "is its own responsible process".

On Linux the file source uses the browser's fixed key, or the key in your desktop keyring through `secret-tool`
(libsecret). KWallet isn't supported yet. Windows isn't supported: Chrome's app-bound encryption there lets only Chrome
open its cookies. The browser can stay open while it reads; the plugin copies the database first.

## Configuration

On the machine with the browser (the **source**), in `node.yaml`:

```yaml
plugins:
  - name: cookies
    command: fleetlayer-cookies
    grants: [sync.publish, reading.publish, reading.subscribe, access.check]
    config:
      share:
        - set: x                        # the bundle's name: cookies/x
          domains: [x.com]              # these hosts and their subdomains
          names: [auth_token, ct0]      # only these cookies
          to: [server]                  # target nodes
```

| Field | Default | What it is |
|---|---|---|
| `set` | | A name for this group of cookies (`a-z`, `0-9`, `-`, `_`); targets ask for it by name |
| `domains` | | Host names, each covering its subdomains. Never a wildcard, a bare top-level domain or a public suffix (`co.uk`, `github.io`) |
| `names` | all (`file`); required (`browser`) | Only these cookies. Keep the list short: a target can use everything it's sent |
| `to` | | The nodes to send to |
| `source` | `browser` on macOS, `file` elsewhere | Where the cookies come from (above) |
| `refresh_days` | 14 | `browser`: read again after this long unasked (each read approved) |
| `playwright` | the [browser guide](../guides/browser.md)'s install | `browser`: `{node, cli, token_file, chrome}`, how to start Playwright MCP (0.0.86 or later). Defaults: `/opt/homebrew/bin/node` if it exists, else `node` on the `PATH`; `~/.local/share/browser-mcp/node_modules/@playwright/mcp/cli.js`; `~/.config/browser-mcp/extension-token`; Chrome's usual path on macOS, none elsewhere |
| `browser` | `chrome` | `file`: `chrome`, `chromium`, `brave` or `edge` (`browser` reads Chrome) |
| `profile` | `Default` | The browser profile's folder name (`Profile 1`, …), as `chrome://version` shows it |
| `user_data_dir` | the browser's usual one | The browser's user data folder |
| `cookies_file` | | The cookie database itself, instead of `browser`, `profile` and `user_data_dir` |
| `require` | `names` | Cookies that must be there and unexpired. When one isn't (you've signed out), nothing is sent and the plugin reports it |
| `every_minutes` | 5 | `file`: how often it reads the cookies; it sends only when they changed |
| `resend_hours` | 12 | `file`: it sends again after this long even when nothing changed, so targets can tell it's alive |

Only the source's own config decides which sites leave the machine. A target can't ask for anything else, and the
plugin never reads another domain's cookies.

On each **target**:

```yaml
plugins:
  - name: cookies
    command: fleetlayer-cookies
    grants: [reading.publish]
    config:
      receive:
        - set: x
          write:
            - { format: env, path: ~/.config/fleetlayer/cookies/x.env, values: { AUTH_TOKEN: auth_token, CT0: ct0 } }
            - { format: netscape, path: ~/.config/fleetlayer/cookies/x.txt }
```

| Field | Default | What it is |
|---|---|---|
| `set` | | The source's set name |
| `write` | | The files to write, each with a `format` and an absolute `path` (`~` is the node user's home) |
| `require` | | More cookies that must be there and unexpired. The ones an `env` file names always are |
| `stale_hours` | 48 | Report a problem when nothing has arrived for this long (from a `browser` source: its `refresh_days` and two days more) |
| `from` | the first sender | The source's node. Any node can run a plugin called `cookies` and publish a set of the same name, so only one node's bundles are applied: this one, or else the first node that sent the set (remembered in `~/.local/state/fleetlayer/cookies/sources.json`). Others are refused and show as a problem for a day |

Formats:

| `format` | What it writes | For |
|---|---|---|
| `netscape` | A Netscape `cookies.txt`, one cookie a line (no `#HttpOnly_` prefixes, which some readers drop) | `curl -b`, `wget --load-cookies`, `yt-dlp --cookies`, `gallery-dl --cookies` |
| `json` | Playwright's `storageState` shape: `{"cookies": [...], "origins": []}` | `browser.newContext({storageState})`, scripts |
| `env` | `VAR=value` lines, one per entry of `values` (variable → cookie name), values single-quoted where a shell needs it | tools that take cookie values by environment variable or flag |

Each file is written mode 0600 (a missing folder is made 0700), replaced in one rename. A file that's already there and
wasn't written by this plugin is never overwritten: move it away and the next bundle writes it. When several of a set's
domains have a cookie of the same name, an `env` value takes the one that expires last.

## Example: an X client on a server

bird (`@steipete/bird` on npm) reads `AUTH_TOKEN` and `CT0` from its environment (or `--auth-token` and
`--ct0`). With the configuration above, an agent on `server` runs:

```sh
set -a; . ~/.config/fleetlayer/cookies/x.env; set +a
bird whoami
```

When bird says it's signed out, ask for a fresh read, which waits for your approval:

```sh
fleetlayer-cookies request x --why "bird whoami says signed out"
```

## Approving it

With the `browser` source, every read of your browser waits for your approval first (above). A source's first bundle to each target waits for your approval, like any sync (the default access policy's `ask:`
rule for `sync`; [access control](../access-control.md)). `fleetlayer readings sync.status` shows the waiting target
and the command that approves it. Each delivery is checked against your policy and written to the control plane's
audit log (the bundle, `cookies/<set>`, and the target), and the source's log names the domains and targets of every
version it sends. Cookie values never appear in logs, messages, readings or the plugin's state files. Each target also lists the values it
received in the node's secrets registry, as `cookie://<set>/<name>`, so the node redacts them from everything it sends
the control plane ([scrubbing](../credentials.md#scrubbing-what-leaves-a-node)), and `fleetlayer secrets forget` (or a
role's window ending) removes every file the plugin wrote.

The control plane relays each bundle as ciphertext sealed to the target node's key and never stores it. A target that
was offline gets the latest version when it's back, while the source is online.

## Monitoring

The plugin reports whether it does its job (`fleetlayer status`, the `plugin.status` reading, the node's heartbeat):

- **Source:** it can't read the browser (Chrome not running or the extension missing for `browser`; the Keychain
  refused, macOS blocking the cookie file or no cookie database for `file`), or a required cookie is missing or
  expired, which usually means you've signed out of the site. The message says what to do. A browser read waiting for
  your approval isn't a problem: the reading shows it (`waitingFor`).
- **Target:** a required cookie has expired or wasn't sent, a file couldn't be written, or nothing has arrived for
  `stale_hours` (the source machine is asleep or offline, or its sync to this node isn't approved).

Readings, which never hold values:

| Kind | Key | Data |
|---|---|---|
| `cookies.share` | set | `file`: `{set, domains, to, browser, cookies, earliestExpiry, version, publishedAt, readAt, problem}`; `browser`: `{set, source, domains, names, to, targets: {<node>: {deliveredAt, waitingFor}}, readAt, problem}` |
| `cookies.receive` | set | `{set, version, appliedAt, readAt, cookies, earliestExpiry, files, problem, requestedAt, requestWhy}` |

`earliestExpiry` is over the required cookies. `fleetlayer readings cookies.receive` shows every target's sets.

## The bundle

`cookies/<set>`, one item, `cookies.json`: `{set, domains, readAt, cookies: [{domain, name, value, path, expires,
secure, httpOnly, sameSite}]}`, where `domain` is the browser's host key (`.x.com` covers subdomains, `x.com` doesn't)
and `expires` is Unix seconds, null for a session cookie (and for every cookie read through the browser). A `browser`
source adds `refreshDays`.
