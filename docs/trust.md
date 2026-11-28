# Installing and trusting Fleetlayer

Fleetlayer runs next to your agents, as you, on every machine you give it, and some of its plugins hold your
sign-ins. That's a lot to trust a young project with. This page says plainly what installing it means, how to check
what you install, what each piece can read, and where the boundaries are.

## What you're trusting

- **A young project.** Fleetlayer is early, built in public, and much of it is written by AI agents under one
  person's direction, with tests and reviews but without a long track record. Expect breaking changes between
  releases, and read the [changelog](changelog.md) before upgrading.
- **The release pipeline.** Binaries are built by GitHub Actions from the tagged source, in the public repository.
  Whoever can push a release tag there can ship code to every machine that updates itself (below).
- **Source you can read.** Everything is in [the repository](https://github.com/fleetlayer/fleetlayer), under
  FSL-1.1-ALv2: you may read, run, change and build it for anything except a competing product, and each release
  becomes Apache 2.0 two years after it ships.

## `curl … | sh`, and how to check a release first

The installer is a short shell script. It needs no root and writes only to `~/.local/lib/fleetlayer/` and
`~/.local/bin`. It downloads the release for your platform, checks it against the release's `SHA256SUMS`, and puts
the binaries in place. Read it before you run it:

```sh
curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh -o install.sh
less install.sh
sh install.sh                                  # FLEETLAYER_VERSION=v0.4.1 sh install.sh pins a release
```

The checksum only catches a damaged download, since it comes from the same release as the tarball. To check that a
tarball was built by Fleetlayer's own release workflow from the tagged source, download it from the
[releases page](https://github.com/fleetlayer/fleetlayer/releases) and verify its build attestation:

```sh
gh attestation verify fleetlayer-v0.4.1-aarch64-apple-darwin.tar.gz --repo fleetlayer/fleetlayer
```

The macOS binaries are signed with a Developer ID and notarised by Apple; `codesign` and `spctl` show it
([Releases and updates](releasing.md#check-a-release)). Or build from source: [Make it yours](guides/make-it-yours.md).

## Self-update is on by default

A node the installer put in place checks for a new release every 6 hours and installs it by itself, after checking
it against `SHA256SUMS`. It doesn't yet verify the attestation or a signature with a key it trusts, so it trusts the
GitHub release as published. To choose the moment yourself, in each machine's `~/.config/fleetlayer/node.yaml`:

```yaml
update:
  mode: notify      # notify: only log that a release is out. off: don't check. auto: install it (the default).
```

A node installed any other way (a package, Ansible, your own build) never updates itself; it only logs that a release
is out.

## What each piece can read and do

Every process runs as the Unix user that started it, so it can read whatever that user can. The node and its plugins
run as you unless you give them a user of their own.

| Piece | Reads or holds | Can do |
|---|---|---|
| `fleetlayer-node` | Its key and token (`~/.config/fleetlayer/`), the session list from your session manager | Start, message and close sessions it's allowed to; deliver sealed secrets to plugins |
| Control plane | Session names and status, summaries, who messaged whom and when, handoff notes, the audit log, and event bodies: webhook payloads, Matrix room messages, and session status with a blocked prompt's text (kept 7 days; other channels until pruned). The text of messages between sessions passes through and isn't stored. No credentials: hashes of tokens, and one GitHub webhook signing secret | Decide every request by the access policy; relay ciphertext it can't read |
| Claude Code plugin | Claude Code's session registry and transcripts on its machine; adds hooks to `~/.claude/settings.json` | Type messages into sessions; answer a blocked prompt when allowed |
| Summaries | Transcripts on its machine | Send them to Claude with your own accounts, to write the summaries |
| Claude accounts | Your Claude sign-ins and setup tokens (on the hub); a setup token per account (on each node) | Run Claude as you; move sessions between your accounts |
| Shared browser | Your signed-in Chrome on your laptop | Act as you on any site you're signed in to, for the agents you allow |
| Shared folders | The folders you list (screenshots, downloads) | Upload or delete there only where you allow it |
| **Cookies** | Your browser's cookie store and the key that decrypts it | Send the cookies of the sites you list to other machines, where anything running as that user can act as you on those sites. This widens your attack surface more than anything else here: send only what a tool needs, and prefer the shared browser |
| 1Password | A service-account token, under a Unix user of its own | Deliver the items you grant to each role |
| Egress | Each restricted role's connections | Refuse hosts off the role's list; its firewall rules need root once, to set up |
| Sign-in | Passkeys, the token-signing key | Sign people in to your services, by the access policy |

Plugins you install from others run with the same rights as yours; their directory entries say what each can read
and do ([plugin directory](directory.md)).

## The trust boundary is a Unix user

A machine can only keep apart what runs as different Unix users. Every process of one user can read that user's
files, its node's token and key, and act as any of its sessions. So:

- **Agents that read strangers' text** (email, the web, issues) run as their own user with their own node and limited
  rules ([An agent that reads strangers' text](guides/untrusted-input.md)).
- **Anything you trust more than the agents beside it** (a runner of approved actions, the 1Password plugin) gets its
  own user too.
- **The control plane can't read your secrets.** They travel sealed to each machine's key, and a sender pins each
  machine's key the first time, so a control plane that lied about a key still couldn't read what's sent
  ([Credentials](credentials.md)).
- **Within one user, sessions trust each other.** Which session did something is recorded, but nothing stops one
  session in a role reading what another in the same role can.

Related: [Access control](access-control.md), [Credentials](credentials.md), [Requirements, and
uninstalling](requirements.md).
