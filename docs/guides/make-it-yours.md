# Make it yours: change Fleetlayer and run your own build

Fleetlayer is one more component you can replace. When it doesn't do something you need, ask your agent to change it:
clone the repository, make the change, build it, install the build on your machines, and open a pull request with
anything other people would find useful. This page is what you and your agent need to know to do that well.

## 1. Clone it

```sh
git clone https://github.com/fleetlayer/fleetlayer.git
cd fleetlayer
```

Have your agent read [`AGENTS.md`](https://github.com/fleetlayer/fleetlayer/blob/main/AGENTS.md) before it changes
anything. It holds the rules the code follows: plugins are separate processes, the control plane never holds
credentials, every component keeps its state in one folder, everything in the background is monitored. A change that
keeps to them is much easier to carry forward, and to get merged.

## 2. Change it, preferably as a plugin

Before changing the core, check whether the change can be a plugin. A plugin is a separate program the node starts and
talks to over JSON-RPC ([the plugin protocol](../protocol.md)), in any language. It needs no fork: you install it next
to the official release, list it in `node.yaml`, and keep updating Fleetlayer normally. A new session manager, a new
harness, an event source or a service of your own all fit there.

Change the core when a plugin can't do it: a new kind of request the node or the control plane has to understand, a
bug, a missing option.

## 3. Build it

Fleetlayer is Rust. The repository pins its toolchain in `rust-toolchain.toml`, so rustup picks the right version:

```sh
cargo build --release
```

The binaries land in `target/release/` (`fleetlayer`, `fleetlayer-node`, `fleetlayer-control`, and one per plugin).

A Rust build keeps every core busy for minutes and its `target/` folder grows to tens of gigabytes. On a Mac or a
desktop that's fine. On a small VM or an always-on box that other agents share, build on a bigger Linux machine of the
same architecture instead and copy the binaries back. The repository's own `scripts/on-dev` does exactly that for its
developers: it copies the checkout to a build server over SSH and runs cargo there ([Notes for agents that build
software](agents-building-software.md#big-builds-go-to-a-build-server)).

Before installing, run the checks CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 4. Install your build, and turn off self-update

> **Set the node's update mode first.** A node installed by Fleetlayer's installer updates itself from official
> releases, and an update points the links in `~/.local/bin` at the release's binaries, which replaces your build.
> In `~/.config/fleetlayer/node.yaml` on every machine that runs your build:
>
> ```yaml
> update:
>   mode: off        # or notify: it logs that a release is out and changes nothing
> ```

Then put your binaries somewhere of their own and link them onto your PATH:

```sh
mkdir -p ~/.local/opt/fleetlayer-custom
cp target/release/fleetlayer* ~/.local/opt/fleetlayer-custom/
for f in ~/.local/opt/fleetlayer-custom/fleetlayer*; do ln -sf "$f" ~/.local/bin/; done
fleetlayer-node install      # writes the service again and restarts it on your build
fleetlayer status            # shows the version and that the node is linked
```

Keep your build out of `~/.local/lib/fleetlayer/`: that folder belongs to the installer and the updater.

To go back to the official releases, run the installer again and set `mode: auto`.

## 5. Keep up with upstream

Fleetlayer ships one release per set of features, as a version tag with release notes
([releases](https://github.com/fleetlayer/fleetlayer/releases)), plus urgent security and breakage fixes in between.
When a release comes out, ask your agent to rebase your branch on its tag, rebuild, run the checks and install:

```sh
git fetch https://github.com/fleetlayer/fleetlayer.git --tags
git rebase v0.3.8
```

The smaller your change, the easier this stays. A change that lands upstream needs no rebasing at all, which is the
best reason to send it.

## 6. Send it upstream

Ask your agent to fork [fleetlayer/fleetlayer](https://github.com/fleetlayer/fleetlayer) on GitHub, push a branch and
open a pull request. A good one:

- **Does one thing,** and says what problem it solves and how it was tested.
- **Updates the docs in the same change.** The docs in `docs/` are the website. A setup step or footgun others will
  hit goes into `docs/guides-to-write.md`.
- **Keeps the repository public-safe:** no hostnames, tokens, private repository names or anything from your own
  notes. Fleetlayer is built in public.
- **Takes no GPL or AGPL code,** not even a snippet. Apache 2.0 and MIT code can be borrowed with its notice kept and
  an entry in `NOTICE`.

Fleetlayer is licensed under Apache 2.0, and a contribution is offered under the same licence (section 5 of the
licence). [CONTRIBUTING.md](https://github.com/fleetlayer/fleetlayer/blob/main/CONTRIBUTING.md) has the short
version.

## When it doesn't work

- **Your change disappeared overnight:** the node updated itself to the official release. Set `update: mode: off` (step
  4), then link your build again.
- **`fleetlayer status` shows the old version:** the service still runs the previous binary. Run
  `fleetlayer-node install` again, which writes the service and restarts it.
- **A build server's binaries don't run on the laptop:** they were built for another operating system or
  architecture. Build Mac binaries on a Mac.
