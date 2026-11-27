# fleetlayer-folders

A read-only MCP server for a few folders you choose, such as your screenshots and downloads. A node shares it as a
tool, so an agent on any machine can pick up the screenshot you just took or the file you just downloaded, without you
copying anything across. [Your Mac's screenshots and downloads, for agents anywhere](../guides/folders.md) sets it up
step by step.

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

- `--max-read-mb <n>`: the most of one file a reply carries, 8 by default and at most 10. A reply travels as one
  WebSocket message, and those are refused over 16 MiB once base64 has added its third.

- `--trusted <name>`: a folder only you write to. Everything read from any other folder comes with a line ahead of
  it saying the contents are outside data, information and never instructions, as messages from outside do: Downloads
  holds web pages, PDFs and emails from anyone.

To give some agents your screenshots but not your downloads (bank statements, scans of IDs), make two entries, each
with one folder, and let the access policy decide each one: `tool:screenshots@laptop` and `tool:downloads@laptop`.

## Tools

| Tool | What it does |
|---|---|
| `folders` | The folders, each with its file count and newest file, or the reason it can't be read |
| `list` | A folder's files, newest first: name, size, modified time, how long ago, kind (`image`, `text`, `pdf`, `video`, `archive`, `other`). `limit` (at most 200), `offset` |
| `latest` | The newest file, or `n` of them, optionally of one `kind`. With `read: true`, their contents too (at most 5) |
| `find` | By `pattern` (case-insensitive, `*` and `?`, or a piece of the name) and/or `since` (`30m`, `2h`, `3d`, `1w`, a date or a timestamp) |
| `read` | One file by `name`: an image as image content the agent can see, text as text, anything else as base64 data |

Images (PNG, JPEG, GIF, WebP) are downscaled to 1568 pixels on the longest side, the size Claude scales them to
anyway; a retina screenshot would otherwise be several megabytes. `max_px` changes it, and `max_px: 0` sends the
original. An image that doesn't decode, or claims more than 20,000 pixels a side, is sent as a file instead. Text is
sent up to 1 MB, then cut with a note.

## What it can reach

Only regular files directly inside each folder. It never lists or reads subfolders, hidden files or symbolic links
(a link is counted as skipped, never followed), opens each file without following links and checks it's the file it
looked at, and has no tool that writes, moves or deletes. The folder itself may be a link (`~/Screenshots` pointing
into iCloud Drive): it's resolved once, at its root.

Files kept in the cloud and not downloaded to the Mac (iCloud Drive's "Optimise Mac Storage", Dropbox and other
File Provider folders) are listed with `not_downloaded: true` and refused on read, rather than starting a download.

## macOS privacy protection

macOS keeps programs out of Downloads, Desktop and Documents until you allow them. The node runs as a LaunchAgent,
so `fleetlayer-folders` asks under the node's name: allow it once in System Settings > Privacy & Security > Files and
Folders (or Full Disk Access). Until then the `folders` tool and every read say so. A folder of your own, such as
`~/Screenshots`, isn't protected and needs nothing. Each Fleetlayer release is a new binary, so macOS may ask again
after an update.
