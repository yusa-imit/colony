#!/usr/bin/env bash
# Prints the release notes for a version: its section of CHANGELOG.md, then install instructions for the
# archives the release workflow builds. Fails when CHANGELOG.md has no section for the version.
#
#   .github/scripts/release-notes.sh 0.1.1
set -euo pipefail

version=$1

# The section starts at "## [<version>]" and ends before the next "## " heading.
section=$(awk -v v="[$version]" '
  /^## / { if (found) exit; if ($2 == v) { found = 1; next } }
  found { print }
' CHANGELOG.md | sed -e '/./,$!d')

if [ -z "$section" ]; then
  echo "CHANGELOG.md has no section for $version (expected a heading \"## [$version] - <date>\")" >&2
  exit 1
fi

printf '%s\n' "$section"
cat <<EOF

### Install

| Platform | Archive |
|---|---|
| macOS, Apple Silicon | \`colony-$version-aarch64-apple-darwin.tar.gz\` |
| macOS, Intel | \`colony-$version-x86_64-apple-darwin.tar.gz\` |
| Linux x86-64 (static) | \`colony-$version-x86_64-unknown-linux-musl.tar.gz\` |
| Linux arm64 (static) | \`colony-$version-aarch64-unknown-linux-musl.tar.gz\` |

\`\`\`sh
shasum -a 256 -c colony-$version-<target>.tar.gz.sha256    # or: sha256sum -c
tar -xzf colony-$version-<target>.tar.gz
xattr -d com.apple.quarantine colony 2>/dev/null           # macOS: the binary is not notarized
mv colony ~/.local/bin/
colony --version
\`\`\`

colony needs \`claude\` (signed in) and \`git\` at run time. Usage: [README](https://github.com/yusa-imit/colony/blob/v$version/README.md).
EOF
