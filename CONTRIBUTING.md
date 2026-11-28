# Contributing to Fleetlayer

Thanks for helping. Most changes to Fleetlayer are made by agents directed by people, and these notes are written for
both.

- **Read [AGENTS.md](AGENTS.md) first.** It holds the rules the code follows (plugins are separate processes, the
  control plane holds no credentials, every component can move, everything in the background is monitored) and the
  rules for borrowed code.
- **A plugin before a core change.** A plugin needs no fork and no pull request: see [the plugin
  protocol](docs/protocol.md). Change the core when a plugin can't do it.
- **One change per pull request,** saying what problem it solves and how it was tested. CI runs `cargo fmt --all
  --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
- **Docs in the same change.** `docs/` is the website (fleetlayer.ai). A new doc starts with a `# Title` line and gets
  an entry in `site/site.config.mjs`. A setup step or footgun others will hit goes into `docs/guides-to-write.md`.
- **Public-safe.** No hostnames, tokens, private repository names, customer data or anything from private notes.
- **No GPL or AGPL code,** not even a snippet. Apache 2.0 and MIT code can be borrowed with its notice kept and an
  entry in [NOTICE](NOTICE).

## Add your plugin to the directory

A plugin of your own needs no fork of Fleetlayer and no change here: it's a separate program the node starts. To list
it in the [plugin directory](https://fleetlayer.ai/docs/directory/), open a pull request that adds one file,
`plugins-directory/<name>.yaml`. [plugins-directory/README.md](plugins-directory/README.md) lists the fields, and CI
checks them. Any open-source or source-available licence is listed; copyleft and proprietary ones are marked on the page.

## Licence

Fleetlayer is licensed under the [Functional Source License (FSL-1.1-ALv2)](LICENSE), and contributions are accepted
under the same licence.

To run your own build of Fleetlayer while a change is in review (or forever), see [Make it
yours](https://fleetlayer.ai/docs/guides/make-it-yours/).
