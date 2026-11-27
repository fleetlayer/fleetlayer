# pages

Hosts the pages your agents write (screenshots, reports, mock-ups) as plain folders, behind one sign-in.

| | |
|---|---|
| Kind | Plugin |
| Roles | None (a service the node runs) |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/pages |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-pages` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.6 and later |
| Plugin protocol | version 1 |
| Needs | a synced folder (Syncthing or any sync); an HTTPS proxy on a server |

## What it does

A page is a folder with `index.md` and its images, written by any agent; there's no publish step. The same plugin
hosts apps. `fleetlayer up` starts it on a laptop with one space.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: pages
    command: fleetlayer-pages
    config:
      listen: 127.0.0.1:0
      base_url: https://pages.example
      spaces:
        personal: { dir: /srv/pages/personal, apps: /srv/apps/personal }
      login_file: /etc/fleetlayer-pages/login
```

## What it can read and do

Serves every file in its spaces' folders to whoever signs in. Pages run no scripts. Apps (interactive pages) run agents' JavaScript, each on a subdomain of its own with a strict content policy; an app from an untrusted space can't reach other sites and opens behind a notice.

More: https://fleetlayer.ai/plugins/pages.md.
