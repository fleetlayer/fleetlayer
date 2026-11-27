# Plugin directory

Every plugin, tool and service that works with Fleetlayer, including the ones that ship with it. The site builds
[fleetlayer.ai/directory](https://fleetlayer.ai/directory/) from these files.

## Add yours

A plugin needs no fork of Fleetlayer: it's a separate program the node starts ([the plugin protocol](../docs/protocol.md)).
Listing it is a pull request that adds one file here, `<name>.yaml`. Copy an existing entry and change it.

| Field | Required | What it is |
|---|---|---|
| `name` | yes | Unique, lower case with hyphens; what `node.yaml` calls it |
| `summary` | yes | One line: what it does for the person |
| `kind` | yes | `plugin` (the node starts it), `tool` (a command line) or `service` (a program of its own) |
| `roles` | plugins | `session_manager`, `harness`, `event_source`, `action`, `sync_target`, `sync_source`, `observer` |
| `ships_with_fleetlayer` | yes | `true` only for what's in every release's archive |
| `status` | yes | `stable`, `beta` or `experimental` |
| `licence` | yes | An SPDX identifier (`Apache-2.0`, `MIT`). Copyleft and proprietary licences are listed, and marked |
| `maintainer` | yes | Who looks after it |
| `source` | yes | The repository (or folder) URL; it must be reachable |
| `install` | yes | `bundled`, or one of `{archive: <url>}`, `{cargo: <crate>}`, `{npm: <package>}`, `{command: "<shell>"}` |
| `command` | yes | The program the node or the person runs |
| `platforms` | yes | Any of `linux`, `macos`, `windows` |
| `min_fleetlayer` | yes | The oldest Fleetlayer release it works with (`0.3.4`) |
| `protocol` | plugins | The plugin protocol version it speaks (Fleetlayer's is `1`) |
| `needs` | no | Other software or Fleetlayer components it needs |
| `grants` | no | What `node.yaml` must grant it (`sync.publish`, `reading.publish`...) |
| `config` | no | The `node.yaml` section to start from |
| `security` | no | What it can read or do, plainly |
| `docs` | no | A page on fleetlayer.ai (`plugins/pages`), or a URL |
| `description` | no | Markdown: more on what it does and how to judge it |

`cd site && npm run check-directory` checks every entry (add `-- --network` to check the source URLs too).
