#!/usr/bin/env bash
# Cross-compiles static musl binaries for x86_64 and aarch64 into dist/, with a
# SHA-256 manifest. The images are pinned by digest, so the same source and the
# same script produce the same checksums on any host with docker.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"
X86_IMAGE="messense/rust-musl-cross@sha256:ce75e9174325d4fbb3de85c309e2d7ca29f7500169bc4b5d2c611ff7e86d549a"
ARM_IMAGE="messense/rust-musl-cross@sha256:ecae5dd62d1c938c14f8071d36c16fa699860aace03bfb5284fb1216474d2643"

mkdir -p "$DIST"

docker run --rm -v "$ROOT:/home/rust/src" -w /home/rust/src "$X86_IMAGE" \
  cargo build --release --target x86_64-unknown-linux-musl
docker run --rm -v "$ROOT:/home/rust/src" -w /home/rust/src "$ARM_IMAGE" \
  cargo build --release --target aarch64-unknown-linux-musl

cp "$ROOT/target/x86_64-unknown-linux-musl/release/termphin-agent" \
  "$DIST/termphin-agent-x86_64"
cp "$ROOT/target/aarch64-unknown-linux-musl/release/termphin-agent" \
  "$DIST/termphin-agent-aarch64"

VERSION="$("$DIST/termphin-agent-x86_64" version --machine | awk -F= '$1 == "version" { print $2 }')"
PROTOCOL="$("$DIST/termphin-agent-x86_64" version --machine | awk -F= '$1 == "protocol" { print $2 }')"
X86_SHA="$(sha256sum "$DIST/termphin-agent-x86_64" | cut -d' ' -f1)"
ARM_SHA="$(sha256sum "$DIST/termphin-agent-aarch64" | cut -d' ' -f1)"

cat > "$DIST/manifest.properties" <<EOF
version=$VERSION
protocol=$PROTOCOL
x86_64.file=termphin-agent-x86_64
x86_64.sha256=$X86_SHA
aarch64.file=termphin-agent-aarch64
aarch64.sha256=$ARM_SHA
EOF

printf 'Built termphin-agent %s (protocol %s)\n' "$VERSION" "$PROTOCOL"
printf '  x86_64  %s\n' "$X86_SHA"
printf '  aarch64 %s\n' "$ARM_SHA"
