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
  arm64) image="menci/archlinuxarm:latest"; platform="linux/arm64" ;;
  *) echo "unknown arch: $arch (use amd64 or arm64)" >&2; exit 1 ;;
esac

echo ">> packing working tree (HEAD) -> $(basename "$tarball")"
git -C "$repo" archive --prefix="${prefix}/" -o "$tarball" HEAD

echo ">> building in $image ($platform)"
# Only base-devel (for makepkg) + sudo are pre-installed. `makepkg -s` then reads
# depends/makedepends straight from the PKGBUILD and installs them via pacman —
# so the package list lives in ONE place (the PKGBUILD), not duplicated here.
docker run --rm --platform "$platform" -v "$here":/build "$image" bash -c '
  set -euo pipefail
  # pacman 7 downloads via a landlock+alpm-user sandbox that cannot initialize
  # under qemu emulation ("switching to sandbox user alpm failed"). Disable it —
  # emulation-only; a native Pi build (makepkg -si) does not need this.
  sed -i "/^\[options\]/a DisableSandbox" /etc/pacman.conf
  pacman -Syu --noconfirm --needed base-devel sudo
  useradd -m builder
  echo "builder ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/builder
  chown -R builder /build
  su builder -c "cd /build && makepkg -sf --skipinteg --noconfirm"
'
echo ">> done. Package(s):"
# PKGEXT varies by image (.zst on Arch x86, .xz on the ALARM image), so match any.
ls -1 "$here"/*.pkg.tar.* 2>/dev/null || echo "  (no package produced — check build output above)"
