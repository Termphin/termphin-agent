#!/usr/bin/env bash
# Cross-compiles static musl binaries for x86_64/aarch64 Linux and a
# x86_64-pc-windows-gnu binary into dist/, with a SHA-256 manifest. The Linux
# images are pinned by digest for fully reproducible builds. The Windows
# build uses a pinned base image too, but installs mingw-w64 and the Rust
# target at build time from Debian's and rustup's own repositories - not
# byte-reproducible the way the Linux builds are, since those packages can
# change over time even for the same image digest.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"
X86_IMAGE="messense/rust-musl-cross@sha256:ce75e9174325d4fbb3de85c309e2d7ca29f7500169bc4b5d2c611ff7e86d549a"
ARM_IMAGE="messense/rust-musl-cross@sha256:ecae5dd62d1c938c14f8071d36c16fa699860aace03bfb5284fb1216474d2643"
WINDOWS_IMAGE="rust@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97"

mkdir -p "$DIST"

docker run --rm -v "$ROOT:/home/rust/src" -w /home/rust/src "$X86_IMAGE" \
  cargo build --release --target x86_64-unknown-linux-musl
docker run --rm -v "$ROOT:/home/rust/src" -w /home/rust/src "$ARM_IMAGE" \
  cargo build --release --target aarch64-unknown-linux-musl
docker run --rm -v "$ROOT:/src" -w /src "$WINDOWS_IMAGE" bash -c '
  apt-get update -qq && apt-get install -y -qq gcc-mingw-w64-x86-64 >/dev/null
  rustup target add x86_64-pc-windows-gnu
  cargo build --release --target x86_64-pc-windows-gnu
'

cp "$ROOT/target/x86_64-unknown-linux-musl/release/termphin-agent" \
  "$DIST/termphin-agent-x86_64"
cp "$ROOT/target/aarch64-unknown-linux-musl/release/termphin-agent" \
  "$DIST/termphin-agent-aarch64"
cp "$ROOT/target/x86_64-pc-windows-gnu/release/termphin-agent.exe" \
  "$DIST/termphin-agent-windows-x86_64.exe"

"$ROOT/scripts/make_manifest.sh" "$DIST"
