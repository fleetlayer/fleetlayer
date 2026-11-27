# file-sync

Keeps a folder (skills, AGENTS.md files) the same on other machines, encrypted in transit.

| | |
|---|---|
| Kind | Plugin |
| Roles | Sync source, Sync target |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/file-sync |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-file-sync` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.2.0 and later |
| Plugin protocol | version 1 |
| Grants | `sync.publish` |

## What it does

The source publishes a new version whenever the folder changes. Each target makes its folder match: files written,
and files this plugin wrote for an earlier version removed. Bundles are named `file-sync/<set>`.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: file-sync
    command: fleetlayer-file-sync
    grants: [sync.publish]          # on the source node
    config:
      publish:                      # on the source node
        - { set: skills, dir: ~/.claude/skills, to: [server, laptop], every_minutes: 5 }
      apply:                        # on each target node
        - { set: skills, dir: ~/.claude/skills }
```

## What it can read and do

A target writes whatever the source publishes into the folder you name, so only the source node should be allowed to send its bundles (your access policy). It never touches files it didn't write. Text files only, at most 1 MB each and 20 MB a set.
