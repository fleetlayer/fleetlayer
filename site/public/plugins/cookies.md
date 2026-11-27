# fleetlayer-cookies

Command-line tools that act as you on a website (an X client, `yt-dlp` for a site you pay for, a script that calls a
dashboard's internal API) usually take your browser's cookies. On your laptop they read Chrome's cookie store directly.
An agent on a server has no browser signed in as you, so those tools don't work there.

`fleetlayer-cookies` fixes that. On the machine with your browser, it reads the cookies of the sites you list and
sends them, encrypted to each target machine's key, whenever they change. On each target it writes them where the tool
looks: a `cookies.txt`, a JSON file, or an env file such as `AUTH_TOKEN=…` and `CT0=…`. It ships with Fleetlayer as
`fleetlayer-cookies`.

> **Whoever can read those files can act as you on those sites**, until the cookies expire or you sign out. Send them
> only to machines whose agents should be signed in as you, and only the cookies a tool needs (`names:`). When an agent
> only needs to look at a page or click through it, give it [your browser](https://fleetlayer.ai/guides/browser.md) instead: nothing is
> copied, and the cookies never leave your laptop.

## What it reads

Chrome, Chromium, Brave and Microsoft Edge, on macOS and Linux.

- **macOS:** the browser encrypts cookies with a key it keeps in your login Keychain ("Chrome Safe Storage"). The first
  time the plugin reads it, macOS asks whether `fleetlayer-cookies` may use that item: type your login password and
  choose **Always Allow**. Release builds are signed with a fixed identity (`dev.fleetlayer.cookies`), so the answer
  holds across upgrades. A build you made yourself asks again after every rebuild. If you click **Deny**, the plugin
  reports it and doesn't ask again until it restarts (`fleetlayer status` shows it as failing).
- **Linux:** it uses the browser's fixed key, or the key in your desktop keyring through `secret-tool` (libsecret) when
  the browser keeps one there. KWallet isn't supported yet.
- **Windows** isn't supported. Chrome's app-bound encryption there lets only Chrome itself open its cookies.

The browser can stay open: the plugin copies the cookie database before reading it.

## Configuration

On the machine with the browser (the **source**), in `node.yaml`:

```yaml
plugins:
  - name: cookies
    command: fleetlayer-cookies
    grants: [sync.publish, reading.publish]
    config:
      share:
        - set: x                        # the bundle's name: cookies/x
          domains: [x.com]              # these hosts and their subdomains
          names: [auth_token, ct0]      # only these cookies (leave it out for all of the domains' cookies)
          to: [server]                  # target nodes
```

| Field | Default | What it is |
|---|---|---|
| `set` | | A name for this group of cookies (`a-z`, `0-9`, `-`, `_`); targets ask for it by name |
| `domains` | | Host names, each covering its subdomains. Never a wildcard, a bare top-level domain or a public suffix (`co.uk`, `github.io`) |
| `names` | all | Only these cookies. Prefer a short list: everything sent is something a target can use |
| `to` | | The nodes to send to |
| `browser` | `chrome` | `chrome`, `chromium`, `brave` or `edge` |
| `profile` | `Default` | The browser profile's folder name (`Profile 1`, …), as `chrome://version` shows it |
| `user_data_dir` | the browser's usual one | The browser's user data folder |
| `cookies_file` | | The cookie database itself, instead of `browser`, `profile` and `user_data_dir` |
| `require` | `names` | Cookies that must be there and unexpired. When one isn't (you've signed out), nothing is sent and the plugin reports it |
| `every_minutes` | 5 | How often it reads the cookies; it sends only when they changed |
| `resend_hours` | 12 | It sends again after this long even when nothing changed, so targets can tell it's alive |

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
| `stale_hours` | 48 | Report a problem when nothing has arrived for this long |

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

## Approving it

A source's first bundle to each target waits for your approval, like any sync (the default access policy's `ask:`
rule for `sync`; [access control](https://fleetlayer.ai/access-control.md)). `fleetlayer readings sync.status` shows the waiting target
and the command that approves it. Each delivery is checked against your policy and written to the control plane's
audit log (the bundle, `cookies/<set>`, and the target), and the source's log names the domains and targets of every
version it sends. Cookie values never appear in logs, messages, readings or the plugin's state files.

The control plane relays each bundle as ciphertext sealed to the target node's key and never stores it. A target that
was offline gets the latest version when it's back, while the source is online.

## Monitoring

The plugin reports whether it does its job (`fleetlayer status`, the `plugin.status` reading, the node's heartbeat):

- **Source:** it can't read the browser (Keychain denied, no cookie database), or a required cookie is missing or
  expired, which usually means you've signed out of the site. The message says what to do.
- **Target:** a required cookie has expired or wasn't sent, a file couldn't be written, or nothing has arrived for
  `stale_hours` (the source machine is asleep or offline, or its sync to this node isn't approved).

Readings, which never hold values:

| Kind | Key | Data |
|---|---|---|
| `cookies.share` | set | `{set, domains, to, browser, cookies, earliestExpiry, version, publishedAt, readAt, problem}` |
| `cookies.receive` | set | `{set, version, appliedAt, readAt, cookies, earliestExpiry, files, problem}` |

`earliestExpiry` is over the required cookies. `fleetlayer readings cookies.receive` shows every target's sets.

## The bundle

`cookies/<set>`, one item, `cookies.json`: `{set, domains, readAt, cookies: [{domain, name, value, path, expires,
secure, httpOnly, sameSite}]}`, where `domain` is the browser's host key (`.x.com` covers subdomains, `x.com` doesn't)
and `expires` is Unix seconds, null for a session cookie.
