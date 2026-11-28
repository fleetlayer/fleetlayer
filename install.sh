#!/bin/sh
# Install Fleetlayer's binaries for this user from the latest GitHub release:
#   curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
# They go into ~/.local/lib/fleetlayer/, with symlinks in ~/.local/bin, and each release is kept as it was unpacked
# in ~/.local/lib/fleetlayer/versions/<version>/ (the newest two). The binaries keep the same paths from one release
# to the next, which is what lets macOS keep the permissions you gave them (docs/releasing.md). A node installed this
# way updates itself from later releases, the same way. Set FLEETLAYER_VERSION=v0.2.0 to pick a release.
set -eu
repo=fleetlayer/fleetlayer
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "fleetlayer: no release is built for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
version=${FLEETLAYER_VERSION:-$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)}
[ -n "$version" ] || { echo "fleetlayer: couldn't find the latest release" >&2; exit 1; }
name="fleetlayer-$version-$target"
base="https://github.com/$repo/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $name..."
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
expected=$(grep "  $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
[ -n "$expected" ] && [ "$actual" = "$expected" ] || { echo "fleetlayer: checksum mismatch for $name.tar.gz" >&2; exit 1; }
tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
lib="$HOME/.local/lib/fleetlayer"
binaries="fleetlayer fleetlayer-node fleetlayer-herdr fleetlayer-rex fleetlayer-tmux fleetlayer-claude-code fleetlayer-summaries fleetlayer-file-sync fleetlayer-claude-auth-manager fleetlayer-claude-accounts fleetlayer-pages fleetlayer-control fleetlayer-gateway fleetlayer-signin fleetlayer-shared-folders fleetlayer-scheduler fleetlayer-leases fleetlayer-cookies fleetlayer-1password fleetlayer-egress fleetlayer-herdr-fleet fleetlayer-dashboard fleetlayer-matrix"
mkdir -p "$lib/versions" "$HOME/.local/bin"
[ -f "$tmp/$name/fleetlayer-node" ] || { echo "fleetlayer: $name.tar.gz has no fleetlayer-node" >&2; exit 1; }
rm -rf "${lib:?}/versions/$version"
mv "$tmp/$name" "$lib/versions/$version"
# Each binary is copied beside the live one and renamed over it: the path never changes and no file is half-written.
for b in $binaries; do
  # This script may be newer than the release it installs: a binary the release doesn't have yet is skipped.
  [ -f "$lib/versions/$version/$b" ] || continue
  cp -p "$lib/versions/$version/$b" "$lib/.$b.new"
  mv -f "$lib/.$b.new" "$lib/$b"
  ln -sfn "$lib/$b" "$HOME/.local/bin/$b"
done
rm -rf "$lib/.skills.new" "$lib/.skills.old"
if [ -d "$lib/versions/$version/skills" ]; then cp -R "$lib/versions/$version/skills" "$lib/.skills.new"; fi
if [ -e "$lib/skills" ]; then mv "$lib/skills" "$lib/.skills.old"; fi
if [ -d "$lib/.skills.new" ]; then mv "$lib/.skills.new" "$lib/skills"; fi
rm -rf "$lib/.skills.old"
printf '%s\n' "$version" > "$lib/.VERSION.new" && mv -f "$lib/.VERSION.new" "$lib/VERSION"
# Release folders from the layout before v0.3.13 (one folder per release) move under versions/.
for old in "$lib"/*/; do
  old=${old%/}
  case "$(basename "$old")" in versions | skills) continue ;; esac
  if [ ! -L "$old" ] && [ -f "$old/fleetlayer-node" ]; then
    rm -rf "$lib/versions/$(basename "$old")"
    mv "$old" "$lib/versions/"
  fi
done
# Keep this release and the newest other one. (Release tags are plain names, so listing them with ls is safe.)
# shellcheck disable=SC2010
ls -1t "$lib/versions" | grep -vx "$version" | tail -n +2 | while IFS= read -r v; do rm -rf "${lib:?}/versions/$v"; done
echo "Installed Fleetlayer $version in $lib, linked from ~/.local/bin."
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add ~/.local/bin to your PATH." ;; esac
echo "Next: write ~/.config/fleetlayer/node.yaml (see docs/getting-started.md), then run: fleetlayer-node install"
