#!/usr/bin/env bash
# Prepare a release: move CHANGELOG.md's Unreleased notes under the new
# version and bump the crates to it. Commit the result, then push a tag.
#   packaging/release.sh 0.1.2
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: packaging/release.sh <version>, e.g. 0.1.2}"
version="${version#v}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $version" >&2; exit 1; }
old=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ "$old" != "$version" ] || { echo "Cargo.toml is already at $version" >&2; exit 1; }
grep -q "^## $version " CHANGELOG.md && { echo "CHANGELOG.md already has $version" >&2; exit 1; }

notes=$(awk '/^## /{ if (u) exit; u = ($2 == "Unreleased"); next } u' CHANGELOG.md | grep -v '^[[:space:]]*$' || true)
[ -n "$notes" ] || { echo "nothing under ## Unreleased in CHANGELOG.md" >&2; exit 1; }

# A fresh Unreleased section above the new version.
today=$(date -u +%Y-%m-%d)
awk -v v="$version" -v d="$today" '
  !done && /^## Unreleased/ { print; print ""; print "## " v " - " d; done = 1; next }
  { print }
' CHANGELOG.md > CHANGELOG.md.tmp && mv CHANGELOG.md.tmp CHANGELOG.md

# The workspace version and the pinned hooktop-core dependency.
sed -i.bak \
  -e "s/^version = \"$old\"/version = \"$version\"/" \
  -e "s/hooktop-core = { path = \"crates\/hooktop-core\", version = \"=$old\" }/hooktop-core = { path = \"crates\/hooktop-core\", version = \"=$version\" }/" \
  Cargo.toml && rm Cargo.toml.bak
cargo update --workspace --quiet

echo "hooktop $old -> $version. Release notes:"
echo
packaging/release-notes.sh "$version"
echo
echo "Next: git commit -am \"chore: release $version\" && git push"
echo "Then: create the v$version tag (git tag v$version && git push origin v$version, or a GitHub release)."
