#!/usr/bin/env bash
# Build the junos-web Arch package from the LOCAL git tree (no release tag
# needed) inside an Arch Linux Docker container. Produces a .pkg.tar.zst under
# packaging/arch/.
#
#   ./packaging/arch/build-local.sh            # x86_64 (native on this host)
#   ./packaging/arch/build-local.sh arm64      # aarch64 (needs qemu binfmt; slow)
#
# For a real aarch64/Raspberry Pi build, prefer running `makepkg -si` directly
# on an Arch Linux ARM machine — see README.md.
set -euo pipefail

arch="${1:-amd64}"                       # amd64 | arm64
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
pkgver="$(bash -c "source '$here/PKGBUILD'; echo \$pkgver")"
prefix="ekos-web-rust-${pkgver}"         # must match _srcdir in PKGBUILD
tarball="$here/junos-web-${pkgver}.tar.gz"   # must match source=() filename

case "$arch" in
  amd64) image="archlinux:latest"; platform="linux/amd64" ;;
  arm64) image="lopsided/archlinux-arm:latest"; platform="linux/arm64" ;;
  *) echo "unknown arch: $arch (use amd64 or arm64)" >&2; exit 1 ;;
esac

echo ">> packing working tree (HEAD) -> $(basename "$tarball")"
git -C "$repo" archive --prefix="${prefix}/" -o "$tarball" HEAD

echo ">> building in $image ($platform)"
docker run --rm --platform "$platform" -v "$here":/build "$image" bash -c '
  set -euo pipefail
  pacman -Syu --noconfirm --needed base-devel git rust rust-wasm trunk tailwindcss
  useradd -m builder
  chown -R builder /build
  su builder -c "cd /build && makepkg -f --skipinteg --noconfirm"
'
echo ">> done. Package(s):"
ls -1 "$here"/*.pkg.tar.zst
