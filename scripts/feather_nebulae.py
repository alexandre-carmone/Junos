#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "numpy>=1.26",
#     "Pillow>=10.0",
# ]
# ///
"""Bake a soft alpha channel into every nebula thumbnail in-place.

The Stellarium nebula set ships as RGB PNGs with pure-black backgrounds.
Blitted naively over the planetarium sky they appear as visible rectangles
(black corners darken stars under them). This script rewrites each PNG to
RGBA where alpha is derived from luminance (black = transparent, bright =
opaque) plus a radial vignette so even nebulae filling the frame fade out
toward the corners.

Idempotent: re-runs re-derive alpha from RGB (transparent corners are still
RGB=0). Safe to rerun any time the curve below is tuned.

Usage:
    uv run scripts/feather_nebulae.py                   # all nebulae
    uv run scripts/feather_nebulae.py m42 m31 ngc7000   # only these
    uv run scripts/feather_nebulae.py --dry-run         # don't write
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
NEBULAE_DIR = ROOT / "rekos-wasm" / "public" / "nebulae"

# Tunables — chosen to clear the typical Stellarium black background while
# keeping faint nebulosity visible. Adjust if specific objects look wrong.
BLACK_FLOOR = 4.0 / 255.0       # luma at/below this is fully transparent
WHITE_CEILING = 64.0 / 255.0    # luma at/above this is fully opaque
VIGNETTE_INNER = 0.92           # normalized radius where vignette starts
VIGNETTE_OUTER = 1.00           # normalized radius where vignette reaches 0


def smoothstep(edge0: float, edge1: float, x: np.ndarray) -> np.ndarray:
    t = np.clip((x - edge0) / (edge1 - edge0), 0.0, 1.0)
    return t * t * (3.0 - 2.0 * t)


def feather_one(path: Path, dry_run: bool = False) -> tuple[bool, str]:
    """Returns (changed, message)."""
    img = Image.open(path)
    if img.mode not in ("RGB", "RGBA", "L", "P"):
        return False, f"skip {path.name}: mode {img.mode}"

    rgb = np.asarray(img.convert("RGB"), dtype=np.float32) / 255.0  # (H, W, 3)
    h, w, _ = rgb.shape

    luma = 0.2126 * rgb[..., 0] + 0.7152 * rgb[..., 1] + 0.0722 * rgb[..., 2]
    alpha = smoothstep(BLACK_FLOOR, WHITE_CEILING, luma)

    # Radial vignette in the inscribed-circle frame: distance from centre,
    # normalized so the corners sit at r ≈ √2 / √2 = 1 (we treat the
    # bounding circle of the inscribed square as r = 1, so the corners are
    # past 1 and fully faded — exactly what we want).
    yy, xx = np.indices((h, w), dtype=np.float32)
    cx, cy = (w - 1) / 2.0, (h - 1) / 2.0
    rx = (xx - cx) / (w / 2.0)
    ry = (yy - cy) / (h / 2.0)
    r = np.sqrt(rx * rx + ry * ry)
    vignette = 1.0 - smoothstep(VIGNETTE_INNER, VIGNETTE_OUTER, r)
    alpha *= vignette

    out = np.empty((h, w, 4), dtype=np.uint8)
    out[..., :3] = (rgb * 255.0 + 0.5).astype(np.uint8)
    out[..., 3] = (np.clip(alpha, 0.0, 1.0) * 255.0 + 0.5).astype(np.uint8)

    if dry_run:
        return False, (
            f"dry  {path.name}  alpha min/max/mean = "
            f"{out[..., 3].min()}/{out[..., 3].max()}/{out[..., 3].mean():.1f}"
        )
    Image.fromarray(out, mode="RGBA").save(path, format="PNG", optimize=True)
    return True, (
        f"wrote {path.name}  alpha min/max/mean = "
        f"{out[..., 3].min()}/{out[..., 3].max()}/{out[..., 3].mean():.1f}"
    )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("names", nargs="*", help="basename(s) without extension; default = all PNGs")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    if not NEBULAE_DIR.is_dir():
        raise SystemExit(f"missing {NEBULAE_DIR}")

    if args.names:
        targets = []
        for n in args.names:
            matches = sorted(NEBULAE_DIR.glob(f"{n}*.png"))
            if not matches:
                print(f"  no match for {n}", file=sys.stderr)
            targets.extend(matches)
    else:
        targets = sorted(NEBULAE_DIR.glob("*.png"))

    print(f"feathering {len(targets)} PNG(s) under {NEBULAE_DIR}")
    changed = 0
    for p in targets:
        ok, msg = feather_one(p, dry_run=args.dry_run)
        if ok:
            changed += 1
        if args.dry_run or len(targets) <= 20 or ok:
            print(f"  {msg}")
    print(f"{'(dry-run) ' if args.dry_run else ''}done — {changed}/{len(targets)} written")


if __name__ == "__main__":
    main()
