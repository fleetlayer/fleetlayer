# Releasing

A version tag (`v0.4.0`) starts `.github/workflows/release.yml`: it builds every binary for Linux and macOS, signs and
notarises the macOS ones, attests each tarball, and publishes them with `SHA256SUMS` as a GitHub release. A tag with a
`-` (`v0.4.0-rc.1`) makes a prerelease that `install.sh` and the nodes' own update skip.

## macOS: signed and notarised

Each macOS binary is signed with a Developer ID Application certificate, with the hardened runtime, Apple's timestamp
and an identifier that never changes (`dev.fleetlayer.node` for `fleetlayer-node`, `dev.fleetlayer.cli` for
`fleetlayer`, `dev.fleetlayer.<rest>` for `fleetlayer-<rest>`). Apple then notarises them.
`scripts/release/sign-macos.sh` does both, and fails the build if Apple or Gatekeeper refuses a binary.

Why it matters:

- **Permissions survive upgrades,** together with a path that doesn't change. macOS remembers a permission (Files
  and Folders, Local Network) for a command-line program by its path, and checks the program found there against the
  signature it saw when the person answered. For a signed program that check is the team and identifier, the same in
  every release, so a new build at the same path keeps the permission. For an unsigned one it's the exact bytes, so
  no new build ever does. Signing isn't enough by itself: a signed release installed in a new folder was asked again,
  and a different signed build put at a path that already had the permission wasn't (both tested, 28 Nov 2026). So
  from v0.3.13 the installer and the node's own update keep every binary at one path, `~/.local/lib/fleetlayer/`,
  and replace the files there (`crates/fleetlayer-node/src/update.rs`).
- **A tarball downloaded in a browser runs.** Without notarisation macOS refuses it ("Apple could not verify...").

What to know:

- **Nothing is stapled.** Only apps, disk images and installer packages can carry their notarisation ticket. For a bare
  binary, macOS looks the ticket up online, by the binary's code hash, the first time it checks one that carries the
  downloaded mark. `install.sh` and the node's own update fetch with curl and HTTPS, which don't set that mark, so they
  work offline too. Only a tarball downloaded in a browser, first run with no network, is refused until the Mac is
  online.
- **No entitlements.** The hardened runtime restricts loading other people's libraries, generating code and
  debugging. The binaries do none of those. Starting other programs isn't restricted, so the node runs its plugins and
  tools as before, and the plugins still use the keychain through `/usr/bin/security`.
- **Never change an identifier.** It's half of what macOS stores with a permission. A renamed identifier means every
  person is asked again. A new binary gets its identifier from the rule in `sign-macos.sh`.
- **Signatures outlive the certificate.** They're timestamped, so a release signed before the certificate expires
  stays valid. A release after it fails in the signing step until the secrets hold a new certificate. A new
  certificate from the same team keeps everyone's permissions.

To check a release on a Mac:

```sh
codesign -dv --verbose=2 ~/.local/bin/fleetlayer-node    # Authority=Developer ID Application..., flags=...(runtime)
codesign -dr - ~/.local/bin/fleetlayer-node              # identifier "dev.fleetlayer.node" and ... your team
spctl --assess --type open --context context:primary-signature -v ~/.local/bin/fleetlayer-node
# source=Notarized Developer ID
```

## The secrets

All optional. Without the `CODESIGN_*` secrets the macOS step warns and leaves the binaries unsigned, so a fork's
release still builds; with them and without the `APPLE_NOTARY_*` ones it signs and warns that it didn't notarise.

| Secret | What it is |
| --- | --- |
| `CODESIGN_CERTIFICATE` | A Developer ID Application certificate and its private key, exported as a `.p12` and base64-encoded |
| `CODESIGN_CERTIFICATE_PASSWORD` | The `.p12`'s password |
| `CODESIGN_IDENTITY` | The certificate's name: `Developer ID Application: <name> (<team ID>)` |
| `APPLE_NOTARY_KEY` | An App Store Connect API key (a Team Key with the Developer role), the `.p8` file's text |
| `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_ISSUER_ID` | The two IDs App Store Connect shows beside the key |

Both come from a paid Apple Developer account. Only its Account Holder can create a Developer ID certificate
(developer.apple.com > Certificates; pick the G2 Sub-CA), and Apple lets you download the `.p8` once (App Store
Connect > Users and Access > Integrations). Keep both in a password manager and set the secrets straight from it,
so they never sit in a file or a shell history:

```sh
<read the .p12 from the password manager> | base64 | gh secret set CODESIGN_CERTIFICATE --repo <owner>/<repo>
```

If you make the `.p12` with OpenSSL 3, add `-legacy`: macOS's keychain can't import its default format.

The script runs on any Mac with the same variables in the environment, against a folder of built binaries, which is
the quickest way to check new credentials before a tag:

```sh
scripts/release/sign-macos.sh <folder> fleetlayer fleetlayer-node
```

It makes a temporary keychain for the certificate and removes it when it's done.

## A fork's releases

A fork that signs with its own Developer ID is a different team, so macOS treats its binaries as different programs
from the official ones: someone who moves between the two is asked for each permission again.
