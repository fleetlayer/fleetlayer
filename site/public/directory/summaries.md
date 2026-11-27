# summaries

Writes a short summary of each agent session as it works, as Markdown files, and publishes them for search.

| | |
|---|---|
| Kind | Plugin |
| Roles | Observer |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/summaries |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-summaries` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.1.0 and later |
| Plugin protocol | version 1 |
| Needs | Claude Code (it runs claude -p for each summary) |
| Grants | `sessions.watch`, `summary.publish` |

## What it does

The one-line summaries in `fleetlayer fleet` and the results of `fleetlayer summaries` come from this plugin. Each
summary is also a plain Markdown file on the machine, for your own notes or search.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
plugins:
  - name: summaries
    command: fleetlayer-summaries
    grants: [sessions.watch, summary.publish]
    config:
      publish: full        # full | sentence | none
```

## What it can read and do

Reads the Claude Code transcripts of its own Unix user's sessions (never sessions whose working directory is outside its home), and sends summaries to the control plane, where whoever may read this node can search them. Spends Claude tokens: at most one summary per session per 30 minutes by default.

More: https://fleetlayer.ai/plugins/summaries.md.
