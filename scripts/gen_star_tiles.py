#!/usr/bin/env python3
"""Generate junos-web/public/tycho.bin, the planetarium's deep star layer.

The base star catalog, junos.bin (~329 000 stars, complete to V ~10, with
names and the constellation figures), stays as it is. This script adds what
Tycho-2 (Høg et al. 2000, CDS I/259: 2.5 million stars, ~90% complete to
V 11.5) has on top of it: every Tycho-2 star not already in junos.bin.

The sky loads tycho.bin only once the view is zoomed in past junos.bin's
depth, and then only draws the tiles around the view — so the file is cut
into tiles: 5° declination bands, each split into RA cells of roughly 5° on
the sky (fewer near the poles).

Format (little-endian):
  "TYC2"                      magic
  u32 n_bands, u32 n_tiles
  f32 band_deg                declination height of a band (5)
  f32 mag0, f32 mag_step      V = mag0 + q_mag * mag_step
  f32 bv_step                 B-V = q_bv * bv_step
  u32 × n_bands               RA cells in each band, from the south pole up
  u32 × n_tiles               stars in each tile (band-major, RA-minor)
  then the stars, tile after tile, brightest first, 6 bytes each:
    u16 ra   position across the tile's RA span, in 1/65536ths
    u16 dec  position across the band, in 1/65536ths
    u8  q_mag
    i8  q_bv
Positions are ICRS / J2000, like junos.bin.

The Tycho-2 files (~160 MB) are cached in .cache/tycho2/ (gitignored).

Usage:
    python3 scripts/gen_star_tiles.py
"""

import gzip
import math
import os
import struct
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.join(HERE, "..")
BASE = os.path.join(ROOT, "junos-web", "public", "junos.bin")
OUT = os.path.join(ROOT, "junos-web", "public", "tycho.bin")
CACHE_DIR = os.path.join(ROOT, ".cache", "tycho2")
CDS_URL = "https://cdsarc.cds.unistra.fr/ftp/I/259/"
FILES = [f"tyc2.dat.{i:02d}.gz" for i in range(20)] + ["suppl_1.dat.gz"]

BAND_DEG = 5.0
CELL_DEG = 5.0          # target RA cell width, on the sky
MAG0, MAG_STEP = 4.0, 0.05
BV_STEP = 0.02
DEFAULT_BV = 0.6        # colour of a star with only one Tycho magnitude

# A Tycho-2 star is junos.bin's own when one of its stars lies this close and
# is no more than MATCH_DMAG fainter. junos.bin lists a close pair as one star
# of their combined magnitude, so a component Tycho-2 resolves can be much
# fainter than its base match; a base star much fainter is another star.
MATCH_ARCSEC = 5.0
MATCH_DMAG = 1.0


def fetch(name):
    path = os.path.join(CACHE_DIR, name)
    if os.path.exists(path):
        return path
    os.makedirs(CACHE_DIR, exist_ok=True)
    print(f"  Downloading {name} ...")
    req = urllib.request.Request(CDS_URL + name, headers={"User-Agent": "junos-star-tiles/1.0"})
    with urllib.request.urlopen(req, timeout=600) as resp, open(path + ".part", "wb") as f:
        while chunk := resp.read(1 << 20):
            f.write(chunk)
    os.replace(path + ".part", path)
    return path


def num(s):
    s = s.strip()
    return float(s) if s else None


def johnson(bt, vt):
    """Tycho BT/VT → Johnson (V, B-V), per the Tycho-2 ReadMe note 7."""
    if bt is not None and vt is not None:
        return vt - 0.090 * (bt - vt), 0.850 * (bt - vt)
    if vt is not None:
        return vt, DEFAULT_BV
    return bt - 0.6, DEFAULT_BV  # BT only: typical BT-VT of a field star


def read_tycho():
    """(ra, dec, V, B-V) for every Tycho-2 star, main catalogue + supplement 1."""
    for name in FILES:
        path = fetch(name)
        suppl = name.startswith("suppl")
        with gzip.open(path, "rt", encoding="ascii") as f:
            for line in f:
                if suppl:
                    ra, dec = num(line[15:27]), num(line[28:40])
                    bt, vt = num(line[83:89]), num(line[96:102])
                else:
                    if line[13] == "X":  # no mean position: the observed one
                        ra, dec = num(line[152:164]), num(line[165:177])
                    else:
                        ra, dec = num(line[15:27]), num(line[28:40])
                    bt, vt = num(line[110:116]), num(line[123:129])
                if ra is None or dec is None or (bt is None and vt is None):
                    continue
                v, bv = johnson(bt, vt)
                yield ra, dec, v, bv


def read_base():
    """(ra, dec, mag) of junos.bin's stars (format: see junos-web/src/catalog.rs)."""
    with open(BASE, "rb") as f:
        buf = f.read()
    n, _, _ = struct.unpack_from("<III", buf, 0)
    pos = 12
    out = []
    for _ in range(n):
        ra, dec, mag, _bv = struct.unpack_from("<ffff", buf, pos)
        pos += 16
        pos += 1 + buf[pos]  # constellation
        pos += 1 + buf[pos]  # name
        out.append((ra, dec, mag))
    return out


# Duplicate check on a grid of MATCH_ARCSEC-sized cells: dec rows, and RA
# cells sized for the row's own declination so they stay square on the sky.
CELL = MATCH_ARCSEC / 3600.0


def row_of(dec):
    return int(math.floor((dec + 90.0) / CELL))


def col_of(ra, row):
    cos_dec = max(math.cos(math.radians(-90.0 + (row + 0.5) * CELL)), 1e-6)
    return int(math.floor(ra * cos_dec / CELL))


def build_base_grid(base):
    grid = {}
    for ra, dec, mag in base:
        r = row_of(dec)
        grid.setdefault((r, col_of(ra, r)), []).append((ra, dec, mag))
    return grid


def in_base(grid, ra, dec, v):
    r0 = row_of(dec)
    cos_dec = math.cos(math.radians(dec))
    for r in (r0 - 1, r0, r0 + 1):
        c0 = col_of(ra, r)
        for c in (c0 - 1, c0, c0 + 1):
            for bra, bdec, bmag in grid.get((r, c), ()):
                if bmag - v > MATCH_DMAG:
                    continue
                dra = (bra - ra + 180.0) % 360.0 - 180.0
                d = math.hypot(dra * cos_dec, bdec - dec) * 3600.0
                if d <= MATCH_ARCSEC:
                    return True
    return False


def band_cells():
    """RA cells per declination band, ~CELL_DEG wide on the sky."""
    n_bands = int(round(180.0 / BAND_DEG))
    cells = []
    for b in range(n_bands):
        mid = -90.0 + (b + 0.5) * BAND_DEG
        cells.append(max(1, int(round(360.0 * math.cos(math.radians(mid)) / CELL_DEG))))
    return cells


def main():
    print("Reading junos.bin ...")
    base = read_base()
    grid = build_base_grid(base)
    print(f"  {len(base):,} base stars")

    cells = band_cells()
    first_tile = [0]
    for n in cells:
        first_tile.append(first_tile[-1] + n)
    tiles = [[] for _ in range(first_tile[-1])]

    print("Reading Tycho-2 ...")
    total = dup = 0
    for ra, dec, v, bv in read_tycho():
        total += 1
        ra %= 360.0
        if in_base(grid, ra, dec, v):
            dup += 1
            continue
        b = min(int((dec + 90.0) / BAND_DEG), len(cells) - 1)
        n_ra = cells[b]
        width = 360.0 / n_ra
        c = min(int(ra / width), n_ra - 1)
        q_ra = min(int((ra - c * width) / width * 65536.0), 65535)
        q_dec = min(max(int((dec - (-90.0 + b * BAND_DEG)) / BAND_DEG * 65536.0), 0), 65535)
        q_mag = min(max(int(round((v - MAG0) / MAG_STEP)), 0), 255)
        q_bv = min(max(int(round(bv / BV_STEP)), -128), 127)
        tiles[first_tile[b] + c].append((q_mag, q_ra, q_dec, q_bv))
        if total % 500_000 == 0:
            print(f"  {total:,} read, {dup:,} already in junos.bin")

    kept = total - dup
    print(f"  {total:,} Tycho-2 stars, {dup:,} already in junos.bin, {kept:,} added")

    buf = bytearray(b"TYC2")
    buf += struct.pack("<II", len(cells), len(tiles))
    buf += struct.pack("<ffff", BAND_DEG, MAG0, MAG_STEP, BV_STEP)
    buf += struct.pack(f"<{len(cells)}I", *cells)
    buf += struct.pack(f"<{len(tiles)}I", *(len(t) for t in tiles))
    for t in tiles:
        t.sort()  # brightest first (then by position, for a stable file)
        for q_mag, q_ra, q_dec, q_bv in t:
            buf += struct.pack("<HHBb", q_ra, q_dec, q_mag, q_bv)

    with open(OUT, "wb") as f:
        f.write(buf)
    busiest = max(len(t) for t in tiles)
    print(f"Wrote {OUT} ({len(buf):,} bytes, {len(tiles):,} tiles, busiest {busiest:,} stars)")


if __name__ == "__main__":
    main()
