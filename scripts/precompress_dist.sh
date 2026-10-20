#!/usr/bin/env bash
# Write pre-compressed siblings (<file>.gz, and <file>.br when brotli is on
# PATH) next to the frontend's static files, so junos-server can serve them
# as-is (`ServeDir::precompressed_*`) instead of compressing a 7 MB wasm on
# every page load. Must run at build time: the Nix store is read-only.
#
#   scripts/precompress_dist.sh [dist-dir]      (default: junos-web/dist)
#
# Idempotent: a sibling newer than its source is kept. `tycho.bin` is skipped
# (Tycho-2 packed floats compress by 0.5 %); JPEGs are never touched.
set -euo pipefail

dist="${1:-$(cd "$(dirname "$0")/.." && pwd)/junos-web/dist}"
[ -d "$dist" ] || { echo "precompress_dist: no such dir: $dist" >&2; exit 1; }

have_brotli=0
command -v brotli >/dev/null 2>&1 && have_brotli=1

n=0
while IFS= read -r -d '' f; do
    case "$(basename "$f")" in tycho.bin) continue ;; esac
    if [ ! "$f.gz" -nt "$f" ]; then
        gzip -9 -k -f "$f"
        n=$((n + 1))
    fi
    if [ "$have_brotli" = 1 ] && [ ! "$f.br" -nt "$f" ]; then
        brotli -9 -k -f "$f"
        n=$((n + 1))
    fi
done < <(find "$dist" -maxdepth 2 -type f \
    \( -name '*.wasm' -o -name '*.js' -o -name '*.css' -o -name '*.html' -o -name '*.bin' \) \
    -print0)

echo "precompress_dist: $n file(s) written in $dist (brotli: $have_brotli)"
