#!/usr/bin/env python3
"""Generate stars-web/src/catalog.rs from the AT-HYG stellar database.

Downloads athyg_33_reduced_m10.csv.gz from Codeberg (or uses a local cache),
filters to mag <= 6.5, extracts B-V/BT-VT color index, proper/Bayer names,
and constellation codes, then emits a Rust source file with colored star data.

AT-HYG combines Tycho-2, Gaia DR3, HYG (Hipparcos + Yale + Gliese).
Source: https://codeberg.org/astronexus/athyg

Constellation stick figures are downloaded from Stellarium's western sky culture
(constellationship.fab), which provides the canonical IAU-compatible HIP ID pairs.

Usage:
    python3 scripts/gen_catalog.py
"""

import csv
import gzip
import io
import json
import os
import struct
import urllib.request

# AT-HYG m10 subset: all stars to mag 10, covers our 6.5 limit
# Files are stored in Git LFS on Codeberg; we resolve via the LFS pointer.
ATHYG_RAW_URL = "https://codeberg.org/astronexus/athyg/raw/branch/main/data/subsets/athyg_33_reduced_m10.csv.gz"
ATHYG_LFS_BATCH_URL = "https://codeberg.org/astronexus/athyg.git/info/lfs/objects/batch"
ATHYG_LFS_OBJECT_URL = (
    "https://codeberg.org/astronexus/athyg.git/info/lfs/objects/{oid}"
)
ATHYG_CACHE = os.path.join(os.path.dirname(__file__), "athyg_33_reduced_m10.csv.gz")

CONSTSHIP_URL = "https://raw.githubusercontent.com/Stellarium/stellarium/master/skycultures/modern_st/index.json"
CONSTSHIP_CACHE = os.path.join(os.path.dirname(__file__), "constellationship.json")

OUT = os.path.join(os.path.dirname(__file__), "..", "stars-web", "public", "stars.bin")
MAG_LIMIT = 11.5

# Greek letter abbreviation → Unicode symbol
GREEK = {
    "Alp": "\u03b1",
    "Bet": "\u03b2",
    "Gam": "\u03b3",
    "Del": "\u03b4",
    "Eps": "\u03b5",
    "Zet": "\u03b6",
    "Eta": "\u03b7",
    "The": "\u03b8",
    "Iot": "\u03b9",
    "Kap": "\u03ba",
    "Lam": "\u03bb",
    "Mu": "\u03bc",
    "Nu": "\u03bd",
    "Xi": "\u03be",
    "Omi": "\u03bf",
    "Pi": "\u03c0",
    "Rho": "\u03c1",
    "Sig": "\u03c3",
    "Tau": "\u03c4",
    "Ups": "\u03c5",
    "Phi": "\u03c6",
    "Chi": "\u03c7",
    "Psi": "\u03c8",
    "Ome": "\u03c9",
}

# Superscript digit mapping for Bayer suffixes like "Alp-1" → "α¹"
SUPERSCRIPT = {
    "1": "\u00b9",
    "2": "\u00b2",
    "3": "\u00b3",
    "4": "\u2074",
    "5": "\u2075",
    "6": "\u2076",
    "7": "\u2077",
    "8": "\u2078",
    "9": "\u2079",
}


def bayer_name(bayer_str, con_str):
    """Convert AT-HYG bayer field (e.g. 'Alp', 'Bet-1') + constellation to a display name."""
    if not bayer_str or not con_str:
        return None

    parts = bayer_str.split("-", 1)
    abbrev = parts[0].strip()
    suffix = parts[1].strip() if len(parts) > 1 else ""

    greek = GREEK.get(abbrev)
    if not greek:
        return None

    result = greek
    if suffix:
        sup = SUPERSCRIPT.get(suffix, suffix)
        result += sup

    result += " " + con_str
    return result


def resolve_lfs_url(raw_url):
    """If raw_url returns a Git LFS pointer, resolve it to the actual download URL.

    Returns the URL to download the actual file content.
    """
    req = urllib.request.Request(
        raw_url, headers={"User-Agent": "stars-catalog-gen/1.0"}
    )
    resp = urllib.request.urlopen(req, timeout=30)
    data = resp.read()

    # Check if this is an LFS pointer (starts with the LFS version line)
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        # Not a text file — it's the real binary content already
        return raw_url, data

    if not text.startswith("version https://git-lfs.github.com/spec/v1"):
        return raw_url, data

    # Parse LFS pointer
    oid = None
    for line in text.splitlines():
        if line.startswith("oid sha256:"):
            oid = line.split(":", 1)[1].strip()
            break

    if not oid:
        raise ValueError(f"Could not parse LFS pointer OID from: {text!r}")

    lfs_url = ATHYG_LFS_OBJECT_URL.format(oid=oid)
    print(f"  Resolved LFS pointer → {lfs_url}")
    return lfs_url, None


def download_cached_gz(raw_url, cache_path, label):
    """Download a (possibly LFS-backed) gzip file, cache it, return decompressed text."""
    if os.path.exists(cache_path):
        print(f"  Using cached {cache_path}")
        with gzip.open(cache_path, "rt", encoding="utf-8") as f:
            return f.read()

    print(f"  Resolving {raw_url} ...")
    lfs_url, initial_data = resolve_lfs_url(raw_url)

    if initial_data is None:
        print(f"  Downloading {lfs_url} ...")
        req = urllib.request.Request(
            lfs_url, headers={"User-Agent": "stars-catalog-gen/1.0"}
        )
        resp = urllib.request.urlopen(req, timeout=600)
        gz_data = resp.read()
    else:
        gz_data = initial_data

    with open(cache_path, "wb") as f:
        f.write(gz_data)
    print(f"  Saved {label} to {cache_path} ({len(gz_data)} bytes compressed)")

    return gzip.decompress(gz_data).decode("utf-8")


def download_cached(url, cache_path, label):
    """Download url to cache_path if not already cached; return text content."""
    if os.path.exists(cache_path):
        print(f"  Using cached {cache_path}")
        with open(cache_path, encoding="utf-8") as f:
            return f.read()

    print(f"  Downloading {url} ...")
    req = urllib.request.Request(url, headers={"User-Agent": "stars-catalog-gen/1.0"})
    resp = urllib.request.urlopen(req, timeout=300)
    data = resp.read().decode("utf-8")

    with open(cache_path, "w", encoding="utf-8") as f:
        f.write(data)
    print(f"  Saved {label} to {cache_path} ({len(data)} bytes)")
    return data


def load_athyg():
    """Download or load cached AT-HYG CSV, return file handle."""
    data = download_cached_gz(ATHYG_RAW_URL, ATHYG_CACHE, "AT-HYG database")
    return io.StringIO(data)


def load_constellations():
    """Download Stellarium's modern_st/index.json and parse into dicts of HIP pairs and names.

    The JSON has a 'constellations' list; each entry has:
      - 'id': e.g. "CON modern_st And"
      - 'lines': list of polylines, each a list of HIP IDs (connected in order)
      - 'common_name': { 'native': 'Andromeda', 'english': '...' }

    Returns:
      pairs_dict: dict mapping abbr → list of (hip1, hip2) tuples
      names_dict: dict mapping abbr → native name string
      hip_sets:   dict mapping abbr → set of HIP IDs used in the figure
    """
    data = download_cached(CONSTSHIP_URL, CONSTSHIP_CACHE, "constellationship.json")
    obj = json.loads(data)

    pairs_dict = {}
    names_dict = {}
    hip_sets = {}
    for entry in obj.get("constellations", []):
        # id looks like "CON modern_st And"
        con_id = entry.get("id", "")
        parts = con_id.split()
        abbr = parts[-1] if parts else ""
        if not abbr:
            continue

        native = entry.get("common_name", {}).get("native", abbr)
        names_dict[abbr] = native

        pairs = []
        hips = set()
        for polyline in entry.get("lines", []):
            hips.update(polyline)
            for i in range(len(polyline) - 1):
                pairs.append((polyline[i], polyline[i + 1]))

        if pairs:
            pairs_dict[abbr] = pairs
            hip_sets[abbr] = hips

    print(f"  Loaded {len(pairs_dict)} constellations from index.json")
    return pairs_dict, names_dict, hip_sets


def circular_mean_ra(ra_list):
    """Circular mean of RA values in degrees (handles 0/360 wrap)."""
    import math

    sin_sum = sum(math.sin(math.radians(r)) for r in ra_list)
    cos_sum = sum(math.cos(math.radians(r)) for r in ra_list)
    return math.degrees(math.atan2(sin_sum, cos_sum)) % 360.0


def main():
    print("Loading constellation lines...")
    constellations, con_names, con_hip_sets = load_constellations()

    print("Loading AT-HYG database...")
    fh = load_athyg()
    reader = csv.DictReader(fh)

    # (ra_deg, dec_deg, mag, bv, hip, con, name)
    stars = []

    for row in reader:
        # Magnitude filter (also implicitly skips Sol at mag=-26.7)
        try:
            mag = float(row["mag"])
        except (ValueError, KeyError, TypeError):
            continue
        if mag > MAG_LIMIT:
            continue

        # RA/Dec (ra is in hours in AT-HYG, same as HYG)
        try:
            ra_deg = float(row["ra"]) * 15.0
            dec_deg = float(row["dec"])
        except (ValueError, KeyError, TypeError):
            continue

        # B-V / BT-VT color index (default 0.0 = white if missing)
        try:
            bv = float(row.get("ci", ""))
        except (ValueError, TypeError):
            bv = 0.0

        # HIP ID
        hip_str = row.get("hip", "").strip()
        try:
            hip = int(hip_str) if hip_str else None
        except ValueError:
            hip = None

        # Constellation
        con = row.get("con", "").strip() or None

        # Name: proper > Bayer+con > None
        proper = row.get("proper", "").strip()
        bayer_str = row.get("bayer", "").strip()

        if proper:
            name = proper
        elif bayer_str and con:
            name = bayer_name(bayer_str, con)
        else:
            name = None

        stars.append((ra_deg, dec_deg, mag, bv, hip, con, name))

    fh.close()

    # Sort by magnitude (brightest first)
    stars.sort(key=lambda s: s[2])

    print(f"Total: {len(stars)} stars (mag <= {MAG_LIMIT}, sorted by magnitude)")

    # Build HIP -> index mapping and HIP -> (ra_deg, dec_deg) mapping
    hip_to_idx = {}
    hip_to_radec = {}
    for idx, (ra, dec, _, _, hip, _, _) in enumerate(stars):
        if hip:
            hip_to_idx[hip] = idx
            hip_to_radec[hip] = (ra, dec)

    # Build constellation line segments resolved to star indices
    segments = []
    for _name, pairs in constellations.items():
        for h1, h2 in pairs:
            i1 = hip_to_idx.get(h1)
            i2 = hip_to_idx.get(h2)
            if (
                i1 is not None
                and i2 is not None
                and (i1, i2) not in segments
                and (i2, i1) not in segments
            ):
                segments.append((i1, i2))

    print(f"Constellation segments: {len(segments)}")

    # Compute constellation centroids (circular mean RA, mean Dec) from stick-figure stars
    centers = []  # (abbr, native_name, center_ra_deg, center_dec_deg)
    for abbr, hip_set in con_hip_sets.items():
        ra_list = []
        dec_list = []
        for hip in hip_set:
            if hip in hip_to_radec:
                r, d = hip_to_radec[hip]
                ra_list.append(r)
                dec_list.append(d)
        if not ra_list:
            continue
        cra = circular_mean_ra(ra_list)
        cdec = sum(dec_list) / len(dec_list)
        native = con_names.get(abbr, abbr)
        centers.append((abbr, native, cra, cdec))

    centers.sort(key=lambda c: c[0])

    # --- Emit binary ---
    # Format (little-endian):
    #   [u32] n_stars, [u32] n_segments, [u32] n_centers
    #   Stars:    ra(f32) dec(f32) mag(f32) bv(f32) con_len(u8) con(utf8) name_len(u8) name(utf8)
    #   Segments: idx_a(u16) idx_b(u16)
    #   Centers:  abbr_len(u8) abbr(utf8) name_len(u8) name(utf8) ra(f32) dec(f32)
    buf = bytearray()
    buf += struct.pack('<III', len(stars), len(segments), len(centers))
    for ra, dec, mag, bv, _hip, con, name in stars:
        buf += struct.pack('<ffff', ra, dec, mag, bv)
        for s in (con, name):
            if s:
                enc = s.encode('utf-8')
                buf += bytes([len(enc)]) + enc
            else:
                buf += bytes([0])
    for i1, i2 in segments:
        buf += struct.pack('<HH', i1, i2)
    for abbr, native, cra, cdec in centers:
        for s in (abbr, native):
            enc = s.encode('utf-8')
            buf += bytes([len(enc)]) + enc
        buf += struct.pack('<ff', cra, cdec)

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, 'wb') as f:
        f.write(buf)
    print(f"Wrote {OUT} ({len(buf):,} bytes)")


if __name__ == "__main__":
    main()
