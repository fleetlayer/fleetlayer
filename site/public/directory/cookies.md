# cookies

Shares a site's cookies from the signed-in browser on your laptop with command-line tools on other machines, encrypted in transit.

| | |
|---|---|
| Kind | Plugin |
| Roles | Sync source, Sync target |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/cookies |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-cookies` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.12 and later |
| Plugin protocol | version 1 |
| Grants | `sync.publish`, `reading.publish` |

## What it does

Reads Chrome, Chromium, Brave or Edge cookies (macOS and Linux) for the domains you list, and sends them whenever
they change. Each target writes a Netscape `cookies.txt`, a Playwright `storageState` JSON file or an env file of
named values, mode 0600. Expired or missing cookies and a source that can't read the browser show as a failing
plugin. Bundles are named `cookies/<set>`.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: cookies
    command: fleetlayer-cookies
    grants: [sync.publish, reading.publish]
    config:
      share:                        # on the machine with the browser
        - { set: x, domains: [x.com], names: [auth_token, ct0], to: [server] }
      receive:                      # on each target
        - set: x
          write:
            - { format: env, path: ~/.config/fleetlayer/cookies/x.env, values: { AUTH_TOKEN: auth_token, CT0: ct0 } }
```

## What it can read and do

Whoever can read a target's files can act as you on the shared sites until the cookies expire. Only the source's own list of domains leaves it, sealed to each target node's key; the control plane relays ciphertext and never stores it. Values never appear in logs or readings. On macOS it reads the browser's key from your Keychain, once you allow it.

More: https://fleetlayer.ai/plugins/cookies.md.
