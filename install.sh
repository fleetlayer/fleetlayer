#!/bin/sh
# Install Fleetlayer's binaries for this user from the latest GitHub release:
#   curl -fsSL https://raw.githubusercontent.com/fleetlayer/fleetlayer/main/install.sh | sh
# They go into ~/.local/lib/fleetlayer/<version>/, with symlinks in ~/.local/bin. A node installed this way updates
# itself from later releases. Set FLEETLAYER_VERSION=v0.2.0 to pick a release.
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
mkdir -p "$lib" "$HOME/.local/bin"
rm -rf "${lib:?}/$version"
mv "$tmp/$name" "$lib/$version"
for b in fleetlayer fleetlayer-node fleetlayer-herdr fleetlayer-claude-code fleetlayer-summaries fleetlayer-files fleetlayer-claude-auth-manager fleetlayer-claude-accounts fleetlayer-pages fleetlayer-control fleetlayer-gateway; do
  ln -sfn "$lib/$version/$b" "$HOME/.local/bin/$b"
done
echo "Installed Fleetlayer $version in $lib/$version, linked from ~/.local/bin."
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add ~/.local/bin to your PATH." ;; esac
echo "Next: write ~/.config/fleetlayer/node.yaml (see docs/getting-started.md), then run: fleetlayer-node install"
