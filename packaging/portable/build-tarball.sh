#!/usr/bin/env bash
# Build a *portable* Linux release tarball from the local tree:
#
#     junos-server (binary) + junos-web/dist/ (compiled WASM frontend)
#     + a sample systemd unit + run notes.
#
# Runs on any glibc Linux and is arch-agnostic — the produced tarball is named
# after `uname -m` (x86_64 or aarch64), so the same script builds the Raspberry
# Pi artifact when run on an aarch64 host/runner.
#
#   ./packaging/portable/build-tarball.sh
#
# Assumes rustup (with the wasm32-unknown-unknown target) and `trunk` are on
# PATH. The arch-correct Tailwind standalone binary is fetched automatically if
# junos-web/bin/tailwindcss is missing (Trunk's pre_build hook needs it).
#
# Env overrides:
#   VERSION   version label baked into the tarball name (default: git describe)
#   OUT_DIR   where the tarball lands           (default: <repo>/dist-artifacts)
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
cd "$repo"

_twver="3.4.17"
arch="$(uname -m)"                                   # x86_64 | aarch64
ver="${VERSION:-$(git describe --tags --always --dirty)}"
name="junos-web-${ver}-${arch}-linux"
out="${OUT_DIR:-$repo/dist-artifacts}"

# ── Tailwind standalone (arch-correct) ──────────────────────────────────────
if [ ! -x junos-web/bin/tailwindcss ]; then
  case "$arch" in
    x86_64)          tw="tailwindcss-linux-x64" ;;
    aarch64|arm64)   tw="tailwindcss-linux-arm64" ;;
    *) echo "unsupported arch for Tailwind: $arch" >&2; exit 1 ;;
  esac
  echo ">> fetching Tailwind $_twver ($tw)"
  mkdir -p junos-web/bin
  curl -fsSLo junos-web/bin/tailwindcss \
    "https://github.com/tailwindlabs/tailwindcss/releases/download/v${_twver}/${tw}"
  chmod +x junos-web/bin/tailwindcss
fi

# ── Build ───────────────────────────────────────────────────────────────────
echo ">> building frontend (trunk, release)"
( cd junos-web && trunk build --release )

echo ">> building junos-server (release)"
cargo build --release -p junos-server

# ── Stage + pack ────────────────────────────────────────────────────────────
work="$(mktemp -d)"
stage="$work/$name"
mkdir -p "$stage"
install -Dm755 target/release/junos-server            "$stage/junos-server"
cp -a           junos-web/dist                          "$stage/dist"
install -Dm644  packaging/arch/junos-web.service        "$stage/junos-web.service"
install -Dm644  packaging/portable/README.txt           "$stage/README.txt"

mkdir -p "$out"
tar -C "$work" -czf "$out/$name.tar.gz" "$name"
( cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )
rm -rf "$work"

echo ">> wrote $out/$name.tar.gz"
ls -l "$out/$name.tar.gz"
