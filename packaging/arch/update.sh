#!/usr/bin/env bash
# Install / update junos-web on Arch (or Arch Linux ARM) from the latest
# GitHub Release — no custom repo needed.
#
#   ./update.sh            # update to the latest release for this arch
#   ./update.sh --force    # reinstall even if already up to date
#
# It picks the release asset matching `uname -m` (x86_64 or aarch64), so the
# same script works on the desktop and on the Raspberry Pi. Install it system
# wide for a short command:
#
#   sudo install -Dm755 update.sh /usr/local/bin/junos-web-update
#   junos-web-update
set -euo pipefail

repo="alexandre-carmone/ekos-web-rust"
pkgname="junos-web"
arch="$(uname -m)"
force=0
[ "${1:-}" = "--force" ] && force=1

api="https://api.github.com/repos/${repo}/releases/latest"

echo ">> querying latest release of ${repo}"
release_json="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api")"

tag="$(printf '%s' "$release_json" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
[ -n "$tag" ] || { echo "!! could not read latest release tag" >&2; exit 1; }

# Asset URL for a package built for this architecture (.pkg.tar.zst or .xz).
url="$(printf '%s' "$release_json" \
  | grep -oE 'https://[^"]*'"${arch}"'\.pkg\.tar\.[a-z]+' | head -1)"
[ -n "$url" ] || { echo "!! no ${arch} package in release ${tag}" >&2; exit 1; }

installed="$(pacman -Q "$pkgname" 2>/dev/null | awk '{print $2}' || true)"
echo ">> latest release: ${tag}   installed: ${installed:-<none>}"

# Release tags are MAJOR.MINOR.PATCH; installed version is pkgver-pkgrel. Compare
# on the leading pkgver only — skip if it already matches (unless --force).
if [ "$force" -eq 0 ] && [ -n "$installed" ] && [ "${installed%%-*}" = "$tag" ]; then
  echo ">> already up to date (${installed}); use --force to reinstall."
  exit 0
fi

tmp="$(mktemp --suffix=.pkg.tar)"
trap 'rm -f "$tmp"' EXIT
echo ">> downloading $(basename "$url")"
curl -fL# -o "$tmp" "$url"

echo ">> installing via pacman -U"
sudo pacman -U --noconfirm "$tmp"
echo ">> done. Restart the service if running:  sudo systemctl restart ${pkgname}"
