#!/bin/sh
# Installs termphin-agent where the Termphin app keeps it, or finds the copy
# the app already installed, and puts `termphin` on PATH.
#
#   curl -fsSL https://github.com/Termphin/termphin-agent/releases/latest/download/install.sh | sh
#
# TERMPHIN_BASE_URL points it at another copy of the release assets.

set -eu

base_url="${TERMPHIN_BASE_URL:-https://github.com/Termphin/termphin-agent/releases/latest/download}"
lib_dir="$HOME/.local/lib/termphin"
agent="$lib_dir/termphin-agent"
bin_dir="$HOME/.local/bin"
link="$bin_dir/termphin"

say() { printf '%s\n' "$*"; }
fail() {
  printf 'termphin install: %s\n' "$*" >&2
  exit 1
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    fail "needs curl or wget"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

installed_version() {
  "$agent" version --machine 2>/dev/null | sed -n 's/^version=//p'
}

install_agent() {
  case "$(uname -s)" in
    Linux) prefix="" ;;
    Darwin) prefix="macos-" ;;
    *) fail "no build for $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "no build for $(uname -m)" ;;
  esac
  asset="termphin-agent-$prefix$arch"
  key="$(printf '%s' "$prefix" | tr -d '-')${prefix:+_}$arch"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  fetch "$base_url/manifest.properties" "$tmp/manifest" || fail "could not download the release manifest"
  fetch "$base_url/$asset" "$tmp/agent" || fail "could not download $asset"

  expected="$(sed -n "s/^$key\.sha256=//p" "$tmp/manifest")"
  [ -n "$expected" ] || fail "the manifest has no checksum for $asset"
  [ "$(sha256 "$tmp/agent")" = "$expected" ] || fail "$asset does not match its checksum"
  version="$(sed -n 's/^version=//p' "$tmp/manifest")"
  protocol="$(sed -n 's/^protocol=//p' "$tmp/manifest")"

  mkdir -p "$lib_dir"
  chmod 700 "$lib_dir"
  # Staged beside the target, so the rename that puts it in place is atomic.
  cp "$tmp/agent" "$lib_dir/.termphin-agent.tmp"
  chmod 700 "$lib_dir/.termphin-agent.tmp"
  mv -f "$lib_dir/.termphin-agent.tmp" "$agent"
  # The app reads this to decide whether its own copy is current, so it has
  # to describe the binary exactly.
  printf 'version=%s\nprotocol=%s\nsha256=%s\n' "$version" "$protocol" "$expected" >"$lib_dir/.agent.meta.tmp"
  mv -f "$lib_dir/.agent.meta.tmp" "$lib_dir/agent.meta"
  say "installed termphin-agent $version"
}

profile_file() {
  case "$(basename "${SHELL:-sh}")" in
    zsh) printf '%s' "$HOME/.zprofile" ;;
    bash)
      if [ "$(uname -s)" = Darwin ]; then
        printf '%s' "$HOME/.bash_profile"
      else
        printf '%s' "$HOME/.bashrc"
      fi
      ;;
    *) printf '%s' "$HOME/.profile" ;;
  esac
}

if [ -n "$(installed_version)" ]; then
  say "termphin-agent $(installed_version) is already installed"
else
  install_agent
fi

mkdir -p "$bin_dir"
if [ -e "$link" ] && [ ! -L "$link" ]; then
  fail "$link exists and is not a link to termphin-agent - move it away and run this again"
fi
ln -sf "$agent" "$link"

case ":$PATH:" in
  *":$bin_dir:"*)
    say "run: termphin"
    ;;
  *)
    profile="$(profile_file)"
    line='export PATH="$HOME/.local/bin:$PATH"'
    if ! grep -qsF "$line" "$profile"; then
      printf '\n%s\n' "$line" >>"$profile"
      say "added ~/.local/bin to PATH in $profile"
    fi
    say "open a new terminal, then run: termphin"
    ;;
esac
