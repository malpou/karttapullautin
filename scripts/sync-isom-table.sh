#!/usr/bin/env bash
# Refresh the vendored isom-maplibre symbol table (ADR 0006) from one commit of
# https://github.com/MetsaApp/isom-maplibre and record that commit in the README.
# The build reads only the vendored copy; this script is the only thing that fetches.
#
# Usage: scripts/sync-isom-table.sh <40-character commit SHA>
set -euo pipefail

repo=MetsaApp/isom-maplibre
sha=${1:-}
if [[ ! $sha =~ ^[0-9a-f]{40}$ ]]; then
    echo "usage: $0 <40-character commit SHA of $repo>" >&2
    exit 2
fi

dir="$(cd "$(dirname "$0")/.." && pwd)/vendor/isom-maplibre"
mkdir -p "$dir"
for file in isom.yaml isom.schema.json LICENSE; do
    curl -fsSL "https://raw.githubusercontent.com/$repo/$sha/$file" -o "$dir/$file.tmp"
    mv "$dir/$file.tmp" "$dir/$file"
done

cat >"$dir/README.md" <<EOF
# Vendored isom-maplibre symbol table

\`isom.yaml\` and \`isom.schema.json\` are copied unchanged from
[$repo](https://github.com/$repo) (MIT, see \`LICENSE\`) at commit
[\`$sha\`](https://github.com/$repo/tree/$sha).

\`build.rs\` generates \`IsomCode\` and \`IsomTable\` (\`src/isom.rs\`) from \`isom.yaml\`
at build time, offline; a test validates \`isom.yaml\` against \`isom.schema.json\`.
Never edit these files by hand: refresh them with

    scripts/sync-isom-table.sh <commit SHA>

which re-fetches all three files at that commit and rewrites this README.
EOF
echo "vendor/isom-maplibre synced to $repo@$sha"
