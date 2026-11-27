# fleetlayer-folders

An MCP server for a few folders you choose, such as your screenshots and downloads. A node shares it as a tool, so an
agent on any machine can pick up the screenshot you just took or the file you just downloaded, without you copying
anything across. Every folder is read-only by default; `--upload` and `--delete` turn on adding and deleting files,
folder by folder. [Your Mac's screenshots and downloads, for agents anywhere](../guides/folders.md) sets it up step by
step.

It ships with Fleetlayer as `fleetlayer-folders`. It isn't a node plugin: the node starts one copy for each connection,
like any [shared tool](../getting-started.md#using-a-tool-on-another-machine).

## Configuration

Each folder is `--folder name=path`; `~` is the home directory of the node's user.

```yaml
tools:
  - name: folders
    command: fleetlayer-folders
    args: ["--folder", "screenshots=~/Screenshots", "--folder", "downloads=~/Downloads"]
    allow: ["*@server"]                 # which sessions may connect: <tab>@<node> patterns
    networks: ["192.168.1.0/24"]        # optional: only while this machine is on your home network
```

- `--upload <name>`: agents may add files to that folder (the `upload` tool).
- `--delete <name>`: agents may delete files from that folder (the `delete` tool), into its trash. Overwriting a file
  on upload needs both.
- `--max-read-mb <n>`: the most of one file a reply carries, and of one upload, 8 MB by default and at most 10. A
  reply travels as one WebSocket message, and those are refused over 16 MiB once base64 has added its third.
- `--trusted <name>`: a folder only you write to, such as your own notes (not screenshots: they often show someone
  else's email or chat, and agents read text in images). Everything read from any other folder comes with a line ahead
  of it saying the contents are outside data, information and never instructions, as messages from outside do: Downloads
  holds web pages, PDFs and emails from anyone.


The access policy decides per tool entry, so different rights for different agents means several entries, each a copy
of `fleetlayer-folders` with its own arguments and `allow:` list. To give some agents your screenshots but not your
downloads (bank statements, scans of IDs), make two entries with one folder each, `tool:screenshots@laptop` and
`tool:downloads@laptop`. To let one agent tidy a folder that others only read, share it twice:

```yaml
tools:
  - name: downloads
    command: fleetlayer-folders
    args: ["--folder", "downloads=~/Downloads"]
    allow: ["*@server"]
  - name: downloads-manage
    command: fleetlayer-folders
    args: ["--folder", "downloads=~/Downloads", "--upload", "downloads", "--delete", "downloads"]
    allow: ["planning@server"]
```

A tool entry's `tools: {deny: [delete]}` also takes a tool away from everyone using that entry.

## Tools

| Tool | What it does |
|---|---|
| `folders` | The folders, each with its file count and newest file, or the reason it can't be read |
| `list` | A folder's files, newest first: name, size, modified time, how long ago, kind (`image`, `text`, `pdf`, `video`, `archive`, `other`). `limit` (at most 200), `offset` |
| `latest` | The newest file, or `n` of them, optionally of one `kind`. With `read: true`, their contents too (at most 5) |
| `find` | By `pattern` (case-insensitive, `*` and `?`, or a piece of the name) and/or `since` (`30m`, `2h`, `3d`, `1w`, a date or a timestamp) |
| `read` | One file by `name`: an image as image content the agent can see, text as text, anything else as base64 data |
| `upload` | Only with `--upload`. A new file: `name`, and `text` or `base64`. Never replaces a file unless `overwrite: true` |
| `delete` | Only with `--delete`. Moves the file `name` to the folder's trash |

Images (PNG, JPEG, GIF, WebP) are downscaled to 1568 pixels on the longest side, the size Claude scales them to
anyway; a retina screenshot would otherwise be several megabytes. `max_px` changes it, and `max_px: 0` sends the
original. An image that doesn't decode, or claims more than 20,000 pixels a side, is sent as a file instead. Text is
sent up to 1 MB, then cut with a note.

An upload is written to a hidden temporary file, then linked into place without replacing anything, so nobody sees half
a file. It's never executable, and on macOS it's marked as downloaded (the quarantine attribute), so Gatekeeper asks
before anyone opens it as an app or script. An upload that would leave less than 1 GB free on the disk is refused.
Overwriting moves the old file to the trash first.

Deleting isn't permanent: the file moves to `.fleetlayer-trash/` inside the folder, named with the time it was
deleted, and the reply says where. Trashed files are removed after 30 days, checked each time the tool starts. To get
one back, move it out of the trash (in Finder, Shift-Command-. shows hidden folders).

## What it can reach

Only regular files directly inside each folder. It never lists or reads subfolders, hidden files or symbolic links (a
link is counted as skipped, never followed), opens each file without following links and checks it's the file it looked
at. Only the `upload` and `delete` tools change anything, and only in folders you named with `--upload` or `--delete`;
neither ever touches a symbolic link, a subfolder or a hidden file. The folder itself may be a link (`~/Screenshots`
pointing into iCloud Drive): it's resolved once, at its root.


Files kept in the cloud and not downloaded to the Mac (iCloud Drive's "Optimise Mac Storage", Dropbox and other
File Provider folders) are listed with `not_downloaded: true` and refused on read, rather than starting a download.

## macOS privacy protection

macOS keeps programs out of Downloads, Desktop and Documents until you allow them. The node runs as a LaunchAgent,
so `fleetlayer-folders` asks under the node's name: allow it once in System Settings > Privacy & Security > Files and
Folders (or Full Disk Access). Until then the `folders` tool and every read say so. A folder of your own, such as
`~/Screenshots`, isn't protected and needs nothing. Each Fleetlayer release is a new binary, so macOS may ask again
after an update.
