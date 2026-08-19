#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="${1:-$ROOT/dist}"

declare -A PLATFORMS=(
  [x86_64]=termphin-agent-x86_64
  [aarch64]=termphin-agent-aarch64
  [windows_x86_64]=termphin-agent-windows-x86_64.exe
  [macos_x86_64]=termphin-agent-macos-x86_64
  [macos_aarch64]=termphin-agent-macos-aarch64
)

runnable=""
for key in x86_64 macos_x86_64 aarch64 macos_aarch64; do
  candidate="$DIST/${PLATFORMS[$key]}"
  if [[ -x "$candidate" ]] && "$candidate" version --machine >/dev/null 2>&1; then
    runnable="$candidate"
    break
  fi
done
if [[ -z "$runnable" ]]; then
  echo "no runnable termphin-agent binary found in $DIST" >&2
  exit 1
fi

version="$("$runnable" version --machine | awk -F= '$1 == "version" { print $2 }')"
protocol="$("$runnable" version --machine | awk -F= '$1 == "protocol" { print $2 }')"

{
  echo "version=$version"
  echo "protocol=$protocol"
  for key in "${!PLATFORMS[@]}"; do
    file="${PLATFORMS[$key]}"
    path="$DIST/$file"
    [[ -f "$path" ]] || continue
    sha="$(sha256sum "$path" | cut -d' ' -f1)"
    echo "$key.file=$file"
    echo "$key.sha256=$sha"
  done
} > "$DIST/manifest.properties"

echo "Wrote $DIST/manifest.properties (version=$version protocol=$protocol)"
cat "$DIST/manifest.properties"
