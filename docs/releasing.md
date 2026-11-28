# Releases and updates

Fleetlayer ships one release per set of features: a version tag (`v0.4.0`) with release notes, never every build. Each
release can need a backup or a look at its notes before you roll it out, so read them first; the
[changelog](changelog.md) sums each one up. A tag outside that rhythm is for an urgent security fix or a bug that
breaks someone's work, and its notes say which.

Each release on [GitHub](https://github.com/fleetlayer/fleetlayer/releases) has a tarball of every binary for each
platform (Linux and macOS, x86-64 and ARM), a `SHA256SUMS` file, and a build attestation for each tarball. A tag with
a `-` (`v0.4.0-rc.1`) is a prerelease: the installer and the nodes' own updates skip it.

## How nodes update themselves

A node installed by the installer checks for a new release every 6 hours, downloads the tarball for its platform,
checks it against the release's `SHA256SUMS`, puts the new binaries in place and restarts into them. A node installed
any other way (a package, Ansible, a build of your own) only logs that a release is out, since whatever installed it
should update it. Choose per machine in `node.yaml`:

```yaml
update:
  mode: auto        # auto: install it. notify: only log that it's out. off: don't check.
  every_hours: 6
```

Set `mode: notify` on a machine where you want to choose the moment. Upgrading by hand, going back to the previous
release, and where the files live are in [Set up a node that keeps running](guides/node-services.md#5-updates).

## Check a release

The installer and the node check each tarball's SHA-256 against `SHA256SUMS`, which catches a damaged download. Both
come from the same release, though, so to check that a tarball was built by Fleetlayer's own release workflow from
the tagged source, verify its build attestation with GitHub's CLI:

```sh
gh attestation verify fleetlayer-v0.4.0-aarch64-apple-darwin.tar.gz --repo fleetlayer/fleetlayer
```

It prints the workflow and the commit that built it, or fails.

On a Mac, also check the signature of the installed binaries:

```sh
codesign -dv --verbose=2 ~/.local/bin/fleetlayer-node    # Authority=Developer ID Application..., flags=...(runtime)
codesign -dr - ~/.local/bin/fleetlayer-node              # identifier "dev.fleetlayer.node"
spctl --assess --type open --context context:primary-signature -v ~/.local/bin/fleetlayer-node
# source=Notarized Developer ID
```

## macOS: signed and notarised

Each macOS binary is signed with Fleetlayer's Developer ID, with the hardened runtime and an identifier that never
changes (`dev.fleetlayer.node` for `fleetlayer-node`, `dev.fleetlayer.cli` for `fleetlayer`, `dev.fleetlayer.<rest>`
for `fleetlayer-<rest>`), and notarised by Apple. That does two things for you:

- **Permissions you give survive upgrades.** macOS remembers a permission (Files and Folders, Local Network) for a
  command-line program by its path, and checks the program there against what it saw when you answered: for a signed
  program, the team and identifier, which stay the same in every release. So from v0.3.13 the installer and the
  node's own update keep every binary at one path, `~/.local/lib/fleetlayer/`, and replace the files there. Moving to
  that layout is a new path, so macOS asks once more, then never again for an upgrade.
- **A tarball downloaded in a browser runs.** Without notarisation, macOS refuses it ("Apple could not verify...").

Two things to know:

- **The first run of a browser download needs the network.** A bare binary can't carry its notarisation ticket, so
  macOS looks it up online the first time it runs a downloaded one. The installer and the node's own update download
  in a way that doesn't need that check, so they work offline too.
- **No special rights.** The binaries ask for no entitlements: they don't load others' libraries, generate code or
  allow debugging. Starting other programs isn't restricted, so the node runs its plugins and tools as usual.

## Signing your own releases

A fork's release workflow builds without any of this: without the `CODESIGN_*` secrets, the macOS step warns and
leaves the binaries unsigned; with them and without the `APPLE_NOTARY_*` ones, it signs and warns that it didn't
notarise.

| Secret | What it is |
| --- | --- |
| `CODESIGN_CERTIFICATE` | A Developer ID Application certificate and its private key, exported as a `.p12` and base64-encoded |
| `CODESIGN_CERTIFICATE_PASSWORD` | The `.p12`'s password |
| `CODESIGN_IDENTITY` | The certificate's name: `Developer ID Application: <name> (<team ID>)` |
| `APPLE_NOTARY_KEY` | An App Store Connect API key (a Team Key with the Developer role), the `.p8` file's text |
| `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID` | The two IDs App Store Connect shows beside the key |

Both come from a paid Apple Developer account. Only its Account Holder can create a Developer ID certificate
(developer.apple.com > Certificates; pick the G2 Sub-CA), and Apple lets you download the `.p8` once (App Store
Connect > Users and Access > Integrations). Keep both in a password manager and set the secrets straight from it, so
they never sit in a file or a shell history:

```sh
<read the .p12 from the password manager> | base64 | gh secret set CODESIGN_CERTIFICATE --repo <owner>/<repo>
```

If you make the `.p12` with OpenSSL 3, add `-legacy`: macOS's keychain can't import its default format. To try new
credentials before a tag, run `scripts/release/sign-macos.sh <folder> fleetlayer fleetlayer-node` on a Mac with the
same variables set, against a folder of built binaries. It uses a temporary keychain and removes it afterwards.

What to keep in mind:

- **Your releases are different programs to macOS.** Your Developer ID is a different team, so someone who moves
  between your builds and the official ones is asked for each permission again.
- **Never change an identifier.** It's half of what macOS stores with a permission, so renaming one means everyone is
  asked again. A new binary gets its identifier from the rule in `sign-macos.sh`.
- **Signatures outlive the certificate.** They're timestamped, so a release signed before the certificate expires
  stays valid. After it expires, the signing step fails until the secrets hold a new certificate, and a new certificate
  from the same team keeps everyone's permissions.
