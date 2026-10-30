#!/usr/bin/env bash
# Packages a release build as dist/colony-<version>-<target>.tar.gz (the binary and its license) with a
# SHA-256 file next to it, in the format `shasum -a 256 -c` and `sha256sum -c` read.
#
#   .github/scripts/package.sh 0.1.1 aarch64-apple-darwin
set -euo pipefail

version=$1
target=$2
name="colony-$version-$target"
bin="target/$target/release/colony"

[ -x "$bin" ] || { echo "no release build at $bin" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$bin" LICENSE "$stage/"

mkdir -p dist
# COPYFILE_DISABLE keeps macOS tar from adding AppleDouble (._*) entries.
COPYFILE_DISABLE=1 tar -C "$stage" -czf "dist/$name.tar.gz" colony LICENSE
cd dist
if command -v sha256sum > /dev/null; then
  sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else
  shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
fi
cat "$name.tar.gz.sha256"
