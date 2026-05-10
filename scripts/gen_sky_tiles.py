#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "numpy>=1.26",
#     "Pillow>=10.0",
#     "healpy>=1.16",
# ]
# ///
"""Generate the precomputed sky-background tile pyramid.

Reads `junos-web/public/nebulae.json` + `nebulae/*.png` (already plate-solved
by `scripts/platesolve_nebulae.py`) and rasterizes them into HEALPix nested
tiles at one or more levels.

Step 1 of the migration plan ships nebulae only at level 3 (Nside=8, 768
tiles). Faint stars and DSO outlines are added in later steps.

Usage:
    uv run scripts/gen_sky_tiles.py                 # default: level 3 only
    uv run scripts/gen_sky_tiles.py --levels 0-4
    uv run scripts/gen_sky_tiles.py --tile-px 256 --format png

Output layout (under `junos-web/public/sky_tiles/`):
    manifest.json          { format, tile_px, levels: [{ nside, ipix_present }] }
    L{level}/{ipix}.png    one file per non-empty tile

Tiles are HEALPix NESTED ordering, J2000 frame. Each tile is rasterised on a
TAN (gnomonic) projection centred on the HEALPix cell's centre direction.
The tile's pixel scale is chosen so the cell's diagonal fits within the tile
with a small overlap (configurable via `--overlap`).
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from dataclasses import dataclass
from pathlib import Path

import healpy as hp
import numpy as np
from PIL import Image

# ── paths ────────────────────────────────────────────────────────────────────
ROOT = Path(__file__).resolve().parent.parent
PUBLIC = ROOT / "junos-web" / "public"
NEBULAE_JSON = PUBLIC / "nebulae.json"
NEBULAE_DIR = PUBLIC / "nebulae"
OUT_ROOT = PUBLIC / "sky_tiles"


# ── HEALPix tile geometry ────────────────────────────────────────────────────
@dataclass
class TileGeom:
    """Local TAN projection for one HEALPix cell.

    Coordinates: pixel (i, j) in [0, tile_px) with origin at top-left,
    j increasing downward (image convention). World: (ra_rad, dec_rad).
    """

    tile_px: int
    ra0: float       # tile centre RA (rad)
    dec0: float      # tile centre Dec (rad)
    scale: float     # rad/pixel along the central tangent

    def world_to_pix(self, ra: np.ndarray, dec: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        """TAN forward projection. Inputs are in radians, broadcastable."""
        sin_d, cos_d = np.sin(dec), np.cos(dec)
        sin_d0, cos_d0 = math.sin(self.dec0), math.cos(self.dec0)
        cos_dra = np.cos(ra - self.ra0)
        sin_dra = np.sin(ra - self.ra0)
        # Standard TAN (gnomonic), Calabretta & Greisen 2002 eq. (54-55)
        denom = sin_d * sin_d0 + cos_d * cos_d0 * cos_dra
        # Negative or zero denom = behind the tangent point.
        with np.errstate(divide="ignore", invalid="ignore"):
            x = cos_d * sin_dra / denom
            y = (sin_d * cos_d0 - cos_d * sin_d0 * cos_dra) / denom
        i = self.tile_px / 2 + x / self.scale
        j = self.tile_px / 2 - y / self.scale
        # Mark behind-tangent points as NaN so callers can mask them.
        bad = denom <= 0
        i = np.where(bad, np.nan, i)
        j = np.where(bad, np.nan, j)
        return i, j


def tile_geom_for(nside: int, ipix: int, tile_px: int, overlap: float) -> TileGeom:
    """Build a TAN projection sized to fit one HEALPix cell + overlap.

    The cell radius (centre→corner) at Nside is roughly sqrt(3)/Nside rad for
    base pixels; healpy's `max_pixrad` returns the exact maximum angular
    distance from centre to any boundary point.
    """
    theta, phi = hp.pix2ang(nside, ipix, nest=True)  # colatitude, longitude (rad)
    dec0 = math.pi / 2 - theta
    ra0 = phi
    radius = hp.max_pixrad(nside)  # rad
    # Pick scale so the diameter (2 * radius * (1+overlap)) covers the tile.
    scale = (2.0 * radius * (1.0 + overlap)) / tile_px
    return TileGeom(tile_px=tile_px, ra0=ra0, dec0=dec0, scale=scale)


# ── nebula warp ──────────────────────────────────────────────────────────────
@dataclass
class Nebula:
    name: str
    path: Path
    corners_radec: np.ndarray  # shape (4, 2), order BL, BR, TR, TL (deg)


def load_nebulae() -> list[Nebula]:
    with NEBULAE_JSON.open() as f:
        raw = json.load(f)
    out: list[Nebula] = []
    for entry in raw:
        png = PUBLIC / entry["path"]
        if not png.exists():
            print(f"  skip {entry['name']}: missing {png}", file=sys.stderr)
            continue
        corners = np.array(entry["corners"], dtype=np.float64)  # (4, 2) deg
        if corners.shape != (4, 2):
            continue
        out.append(Nebula(name=entry["name"], path=png, corners_radec=corners))
    return out


def warp_nebula_into_tile(
    canvas: Image.Image,
    nebula: Nebula,
    geom: TileGeom,
) -> bool:
    """Project the nebula's 4 sky corners into tile pixels and composite.

    Returns True if any of the four corners landed inside the tile (with a
    1-tile-side margin of slack to catch nebulae extending across the edge).
    """
    ras = np.deg2rad(nebula.corners_radec[:, 0])
    decs = np.deg2rad(nebula.corners_radec[:, 1])
    i, j = geom.world_to_pix(ras, decs)
    if not np.all(np.isfinite(i)) or not np.all(np.isfinite(j)):
        return False

    margin = geom.tile_px  # generous: skip only nebulae far outside the tile
    inside = (
        (i > -margin).any()
        and (i < geom.tile_px + margin).any()
        and (j > -margin).any()
        and (j < geom.tile_px + margin).any()
    )
    if not inside:
        return False
    # Cheap reject: AABB of the projected corners has no overlap with tile box.
    if i.max() < 0 or i.min() > geom.tile_px:
        return False
    if j.max() < 0 or j.min() > geom.tile_px:
        return False

    src = Image.open(nebula.path).convert("RGBA")
    sw, sh = src.size

    # corners_radec order: BL, BR, TR, TL
    # PIL PERSPECTIVE expects mapping from output coords → input coords. We
    # build it by solving the 8-coefficient transform that sends the four
    # tile-pixel corners back to the source image corners.
    src_corners = np.array(
        [[0, sh], [sw, sh], [sw, 0], [0, 0]],
        dtype=np.float64,
    )  # BL, BR, TR, TL in image pixels (origin top-left, y down)
    dst_corners = np.column_stack([i, j])  # (4, 2) tile pixels

    coeffs = _perspective_coeffs(dst_corners, src_corners)
    warped = src.transform(
        (geom.tile_px, geom.tile_px),
        Image.PERSPECTIVE,
        coeffs,
        Image.BILINEAR,
    )
    canvas.alpha_composite(warped)
    return True


def _perspective_coeffs(dst: np.ndarray, src: np.ndarray) -> tuple[float, ...]:
    """Solve for the 8 PERSPECTIVE coefficients PIL wants.

    PIL maps output (x, y) → input ((ax+by+c)/(gx+hy+1), (dx+ey+f)/(gx+hy+1)).
    Given 4 (dst, src) point pairs this is a linear system.
    """
    matrix = []
    for (x, y), (u, v) in zip(dst, src):
        matrix.append([x, y, 1, 0, 0, 0, -u * x, -u * y])
        matrix.append([0, 0, 0, x, y, 1, -v * x, -v * y])
    a = np.asarray(matrix, dtype=np.float64)
    b = src.flatten()
    coeffs, *_ = np.linalg.lstsq(a, b, rcond=None)
    return tuple(coeffs.tolist())


# ── tile generation ──────────────────────────────────────────────────────────
def gen_level(
    level: int,
    tile_px: int,
    overlap: float,
    nebulae: list[Nebula],
    out_dir: Path,
    fmt: str,
) -> list[int]:
    """Generate every tile at this level. Returns the sorted list of non-empty ipix."""
    nside = 1 << level
    npix = 12 * nside * nside
    out_dir.mkdir(parents=True, exist_ok=True)
    present: list[int] = []

    for ipix in range(npix):
        geom = tile_geom_for(nside, ipix, tile_px, overlap)
        canvas = Image.new("RGBA", (tile_px, tile_px), (0, 0, 0, 0))
        for neb in nebulae:
            warp_nebula_into_tile(canvas, neb, geom)
        # Authoritative empty check: did anything actually land on the canvas?
        alpha_min, alpha_max = canvas.getchannel("A").getextrema()
        if alpha_max == 0:
            continue

        out_path = out_dir / f"{ipix}.{fmt}"
        if fmt == "png":
            canvas.save(out_path, format="PNG", optimize=True)
        else:
            raise SystemExit(f"unsupported format: {fmt}")
        present.append(ipix)

        if len(present) % 16 == 0:
            print(f"  L{level}: {len(present)} tiles written ({ipix + 1}/{npix} scanned)")

    print(f"  L{level}: {len(present)} non-empty tiles out of {npix}")
    return present


def parse_levels(spec: str) -> list[int]:
    if "-" in spec:
        lo, hi = spec.split("-", 1)
        return list(range(int(lo), int(hi) + 1))
    return [int(p) for p in spec.split(",") if p]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--levels", default="3", help="HEALPix level(s), e.g. '3' or '0-4' or '2,3'")
    ap.add_argument("--tile-px", type=int, default=256)
    ap.add_argument("--overlap", type=float, default=0.05, help="Fractional overlap added to each tile's FOV")
    ap.add_argument("--format", choices=["png"], default="png")
    args = ap.parse_args()

    if not NEBULAE_JSON.exists():
        raise SystemExit(f"missing {NEBULAE_JSON} — run scripts/download_nebulae.py first")

    print(f"loading nebulae index from {NEBULAE_JSON}")
    nebulae = load_nebulae()
    print(f"  {len(nebulae)} nebulae loaded")

    levels = parse_levels(args.levels)
    OUT_ROOT.mkdir(parents=True, exist_ok=True)

    manifest_levels = []
    for level in levels:
        nside = 1 << level
        out_dir = OUT_ROOT / f"L{level}"
        print(f"\nlevel {level} (Nside={nside}, {12 * nside * nside} cells)")
        present = gen_level(level, args.tile_px, args.overlap, nebulae, out_dir, args.format)
        manifest_levels.append({"nside": nside, "ipix_present": present})

    manifest = {
        "format": args.format,
        "tile_px": args.tile_px,
        "frame": "J2000",
        "ordering": "nested",
        "levels": manifest_levels,
    }
    manifest_path = OUT_ROOT / "manifest.json"
    with manifest_path.open("w") as f:
        json.dump(manifest, f, indent=2)
    print(f"\nwrote {manifest_path}")


if __name__ == "__main__":
    main()
