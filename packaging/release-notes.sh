#!/usr/bin/env bash
# Print the CHANGELOG.md section for one version, without its heading.
#   packaging/release-notes.sh 0.1.2
set -euo pipefail
version="${1#v}"
file="${2:-CHANGELOG.md}"
awk -v v="$version" '
  /^## / { if (found) exit; found = ($2 == v) ; next }
  found { print }
' "$file" | sed -e '/./,$!d' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}'
