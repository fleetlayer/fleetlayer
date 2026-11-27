#!/bin/bash
# Sign the macOS release binaries with a Developer ID and have Apple notarise them.
#
#   scripts/release/sign-macos.sh <folder> <binary>...
#
# release.yml runs this on the macOS builds, before the tarball is made. A Developer ID signature gives each binary
# an identity that stays the same from one release to the next (the team plus the identifier set here), so macOS keeps
# a permission the person granted (Files and Folders, Local Network, the keychain) across upgrades. An unsigned binary
# is known to macOS only by its path and its bytes, and both change with every release.
#
# Secrets, all optional (docs/releasing.md):
#   CODESIGN_CERTIFICATE            the Developer ID Application certificate and its key, a .p12, base64-encoded
#   CODESIGN_CERTIFICATE_PASSWORD   the .p12's password
#   CODESIGN_IDENTITY               the certificate's name, "Developer ID Application: <name> (<team ID>)"
#   APPLE_NOTARY_KEY                an App Store Connect API key, the .p8 file's text
#   APPLE_NOTARY_KEY_ID, APPLE_NOTARY_ISSUER_ID
#
# Without the CODESIGN_* secrets (a fork, a pull request) it warns and leaves the binaries as the linker made them
# (ad-hoc signed on Apple silicon). With them but without the APPLE_NOTARY_* secrets it signs and warns that it
# didn't notarise. With both, anything Apple refuses fails the build.
#
# A bare binary can't carry its notarisation ticket (only apps, disk images and installer packages can be stapled), so
# macOS looks the ticket up online, by the binary's code hash, when it first checks one that was downloaded in a
# browser. install.sh and the node's own update fetch with curl and HTTPS, which don't mark files as downloaded, so
# they never need that check.
set -euo pipefail

dir=${1:?usage: sign-macos.sh <folder> <binary>...}
shift
[ $# -gt 0 ] || { echo "usage: sign-macos.sh <folder> <binary>..." >&2; exit 2; }

warn() { echo "::warning::$*"; }
fail() { echo "::error::$*"; exit 1; }

if [ -z "${CODESIGN_CERTIFICATE:-}" ] || [ -z "${CODESIGN_CERTIFICATE_PASSWORD:-}" ] || [ -z "${CODESIGN_IDENTITY:-}" ]; then
  warn "The CODESIGN_* secrets aren't set here, so the macOS binaries aren't signed or notarised."
  exit 0
fi

# The identifier is half of what macOS remembers a permission by, so it must never change: "fleetlayer-node" is
# dev.fleetlayer.node in every release, and the "fleetlayer" command is dev.fleetlayer.cli.
identifier() {
  case "$1" in
    fleetlayer) echo dev.fleetlayer.cli ;;
    fleetlayer-*) echo "dev.fleetlayer.${1#fleetlayer-}" ;;
    *) fail "No identifier rule for $1" ;;
  esac
}

tmp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/sign-macos.XXXXXX")
keychain="$tmp/signing.keychain-db"
keychain_password=$(openssl rand -hex 24)
# The user's keychain search list, to put back afterwards (the temporary keychain has to be on it for codesign to
# build the certificate chain)
previous_keychains=()
while IFS= read -r line; do
  line=${line#"${line%%[![:space:]]*}"}
  line=${line#\"}
  previous_keychains+=("${line%\"}")
done < <(security list-keychains -d user)

cleanup() {
  if [ ${#previous_keychains[@]} -gt 0 ]; then
    security list-keychains -d user -s "${previous_keychains[@]}" || true
  fi
  security delete-keychain "$keychain" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

printf '%s' "$CODESIGN_CERTIFICATE" | base64 --decode > "$tmp/certificate.p12"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 3600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$tmp/certificate.p12" -k "$keychain" -P "$CODESIGN_CERTIFICATE_PASSWORD" -f pkcs12 -T /usr/bin/codesign > /dev/null
rm -f "$tmp/certificate.p12"
# Lets codesign use the key without a dialog nobody can answer
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" > /dev/null
security list-keychains -d user -s "$keychain" ${previous_keychains[@]+"${previous_keychains[@]}"}

for b in "$@"; do
  [ -f "$dir/$b" ] || fail "No $dir/$b to sign"
  id=$(identifier "$b")
  # --options runtime: the hardened runtime, which Apple requires before it notarises anything. These binaries need
  # no entitlements: they load no libraries of their own and generate no code, and starting other programs (the
  # node's plugins and tools) isn't restricted.
  # --timestamp: Apple's timestamp, so the signature stays valid after the certificate expires.
  codesign --force --sign "$CODESIGN_IDENTITY" --keychain "$keychain" --options runtime --timestamp --identifier "$id" "$dir/$b"

  codesign --verify --strict --verbose=2 "$dir/$b"
  details=$(codesign --display --verbose=2 "$dir/$b" 2>&1)
  grep -q '^CodeDirectory .*flags=0x[0-9a-f]*([^)]*runtime' <<< "$details" || fail "$b isn't signed with the hardened runtime: $details"
  grep -q "^Identifier=$id\$" <<< "$details" || fail "$b's identifier isn't $id: $details"
  grep -q '^Timestamp=' <<< "$details" || fail "$b has no secure timestamp: $details"
  # What macOS stores with a permission: the same line in every release, or people are asked again
  requirement=$(codesign --display --requirements - "$dir/$b" 2>/dev/null)
  grep -q "identifier \"$id\" and anchor apple generic" <<< "$requirement" || fail "$b's designated requirement isn't a Developer ID one: $requirement"
  echo "Signed $b as $id"
done

if [ -z "${APPLE_NOTARY_KEY:-}" ] || [ -z "${APPLE_NOTARY_KEY_ID:-}" ] || [ -z "${APPLE_NOTARY_ISSUER_ID:-}" ]; then
  warn "The APPLE_NOTARY_* secrets aren't set here, so the macOS binaries are signed but not notarised."
  exit 0
fi

mkdir "$tmp/notarize"
for b in "$@"; do cp "$dir/$b" "$tmp/notarize/"; done
ditto -c -k --keepParent "$tmp/notarize" "$tmp/notarize.zip"

printf '%s\n' "$APPLE_NOTARY_KEY" > "$tmp/notary_key.p8"
auth=(--key "$tmp/notary_key.p8" --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID")

# notarytool's exit status alone doesn't say whether Apple accepted the binaries: the submission's status does
set +e
xcrun notarytool submit "$tmp/notarize.zip" "${auth[@]}" --wait --timeout 1h --output-format json > "$tmp/submission.json"
submit_status=$?
set -e
cat "$tmp/submission.json"; echo
id=$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$tmp/submission.json" | head -1)
status=$(sed -n 's/.*"status" *: *"\([^"]*\)".*/\1/p' "$tmp/submission.json" | head -1)
if [ -n "$id" ]; then
  # Apple's report: lists any problems, and warnings even when it accepts the binaries
  xcrun notarytool log "$id" "${auth[@]}" || true
fi
[ "$status" = Accepted ] || fail "Apple didn't notarise the binaries (notarytool exited $submit_status, status: ${status:-none}). Its report above says why."

# Gatekeeper's own answer, looked up online. The ticket can take a moment to be published after Apple accepts.
for b in "$@"; do
  for attempt in 1 2 3 4 5 6; do
    assessment=$(spctl --assess --type open --context context:primary-signature -v "$dir/$b" 2>&1) && accepted=1 || accepted=0
    if [ "$accepted" = 1 ] && grep -q 'source=Notarized Developer ID' <<< "$assessment"; then
      break
    fi
    [ "$attempt" -lt 6 ] || fail "Gatekeeper doesn't see $b as notarised: $assessment"
    sleep 20
  done
  echo "Notarised $b: $(tail -1 <<< "$assessment")"
done
