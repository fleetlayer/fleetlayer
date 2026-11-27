# shared-folders

Lets agents on other machines read, and if you allow it upload to or delete from, folders you choose such as screenshots.

| | |
|---|---|
| Kind | Shared tool |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/shared-folders |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-shared-folders` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.9 and later |

## What it does

An MCP server the node shares with agents anywhere (`fleetlayer mcp shared-folders@<node>`): list a folder newest first,
the latest files, find by name or time, and read a file. Images come back as images the agent can see, downscaled
to 1568 pixels; text as text; anything else as base64, up to 8 MB. With --upload and --delete, agents can add files and move them to the trash.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
tools:
  - name: shared-folders
    command: fleetlayer-shared-folders
    args: ["--folder", "screenshots=~/Screenshots", "--folder", "downloads=~/Downloads"]
    allow: ["*@server"]
```

## What it can read and do

Only files directly inside the folders you name: no subfolders, hidden files or symbolic links. Read-only unless a folder is given --upload (new files, never executable, quarantined on macOS, refused under 1 GB free) or --delete (into a trash kept 30 days). Whoever may use the tool can read everything in those folders, so share Downloads (strangers' files, private documents) as its own tool when it needs stricter access than your screenshots, and give writing rights through a separate entry for named sessions.

More: https://fleetlayer.ai/plugins/shared-folders.md.
