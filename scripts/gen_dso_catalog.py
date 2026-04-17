#!/usr/bin/env python3
"""Generate rekos-wasm/public/dso.bin from the OpenNGC catalog.

Downloads NGC.csv from the OpenNGC GitHub repository (or uses a local cache),
filters to visually interesting deep-sky objects, and emits the binary blob
consumed by `rekos-wasm/src/dso_catalog.rs`.

Fields emitted per object:
  ra_deg, dec_deg, name, kind, mag, size_arcmin (major), size_minor_arcmin, pa_deg,
  common_names (zero or more human-readable aliases from the OpenNGC
  "Common names" column, e.g. "Andromeda Galaxy" for M31)

Usage:
    python3 scripts/gen_dso_catalog.py
"""

import csv
import io
import math
import os
import struct
import urllib.request

OPENGC_URL = (
    "https://github.com/mattiaverga/OpenNGC/raw/master/database_files/NGC.csv"
)
CACHE = os.path.join(os.path.dirname(__file__), "openngc.csv")
OUT = os.path.join(
    os.path.dirname(__file__), "..", "rekos-wasm", "public", "dso.bin"
)

# OpenNGC type codes → DsoType variant
TYPE_MAP = {
    "G":      "Galaxy",
    "GPair":  "Galaxy",
    "GTrpl":  "Galaxy",
    "GGroup": "GalaxyCluster",
    "OCl":    "OpenCluster",
    "GCl":    "GlobularCluster",
    "Neb":    "Nebula",
    "HII":    "Nebula",
    "EmN":    "Nebula",
    "RfN":    "Nebula",
    "PN":     "PlanetaryNebula",
    "SNR":    "SupernovaRemnant",
    "Cl+N":   "OpenCluster",   # cluster + nebula → cluster is the dominant visual feature
}

# Object types to skip entirely (stars, duplicates, non-existent, etc.)
SKIP_TYPES = {"*", "**", "*Ass", "Dup", "NonEx", "Other", "Nova"}

# Magnitude cut-off: objects fainter than this are dropped *unless* they are
# in the Messier catalog or have a large apparent size (>= MIN_SIZE_ARCMIN).
MAG_LIMIT = 14.0
MIN_SIZE_ARCMIN = 1.0  # keep anything with major axis >= 1 arcmin regardless of mag

MISSING_MAG = 99.0  # sentinel for "magnitude unknown"


def load_csv():
    if os.path.exists(CACHE):
        print(f"  Using cached {CACHE}")
        return open(CACHE, encoding="utf-8")

    print(f"  Downloading {OPENGC_URL} ...")
    req = urllib.request.Request(
        OPENGC_URL, headers={"User-Agent": "stars-dso-catalog-gen/1.0"}
    )
    resp = urllib.request.urlopen(req, timeout=300)
    data = resp.read().decode("utf-8")
    with open(CACHE, "w", encoding="utf-8") as f:
        f.write(data)
    print(f"  Saved to {CACHE} ({len(data):,} bytes)")
    return io.StringIO(data)


def parse_ra(s):
    """'HH:MM:SS.ss' → degrees (float). Returns None on failure."""
    s = s.strip()
    if not s:
        return None
    try:
        parts = s.split(":")
        h, m, sec = float(parts[0]), float(parts[1]), float(parts[2])
        return (h + m / 60.0 + sec / 3600.0) * 15.0
    except Exception:
        return None


def parse_dec(s):
    """'±DD:MM:SS.s' → degrees (float). Returns None on failure."""
    s = s.strip()
    if not s:
        return None
    try:
        sign = -1.0 if s.startswith("-") else 1.0
        s = s.lstrip("+-")
        parts = s.split(":")
        d, m, sec = float(parts[0]), float(parts[1]), float(parts[2])
        return sign * (d + m / 60.0 + sec / 3600.0)
    except Exception:
        return None


def parse_float(s, default=None):
    s = s.strip()
    if not s:
        return default
    try:
        return float(s)
    except ValueError:
        return default


def make_name(row):
    """Return the best display name: Messier ID if available, else NGC/IC."""
    m = row.get("M", "").strip()
    if m:
        try:
            return f"M{int(m)}"
        except ValueError:
            pass
    # Name is like "NGC0001" or "IC0001" — strip prefix and leading zeros
    raw = row["Name"].strip()
    if raw.startswith("NGC"):
        num = raw[3:].lstrip("0") or "0"
        return f"NGC {num}"
    if raw.startswith("IC"):
        num = raw[2:].lstrip("0") or "0"
        return f"IC {num}"
    return raw


def main():
    print("Loading OpenNGC catalog...")
    fh = load_csv()
    reader = csv.DictReader(fh, delimiter=";")

    objects = []
    skipped_type = 0
    skipped_coords = 0
    skipped_faint = 0

    for row in reader:
        obj_type = row.get("Type", "").strip()

        # Skip non-DSO types
        if obj_type in SKIP_TYPES or obj_type not in TYPE_MAP:
            skipped_type += 1
            continue

        # Parse coordinates
        ra = parse_ra(row.get("RA", ""))
        dec = parse_dec(row.get("Dec", ""))
        if ra is None or dec is None:
            skipped_coords += 1
            continue

        # Parse optional fields
        maj = parse_float(row.get("MajAx", ""), default=0.0)
        minor = parse_float(row.get("MinAx", ""), default=0.0)
        pa = parse_float(row.get("PosAng", ""), default=0.0)

        # Magnitude: prefer V-Mag, fall back to B-Mag
        mag = parse_float(row.get("V-Mag", ""), default=None)
        if mag is None:
            mag = parse_float(row.get("B-Mag", ""), default=MISSING_MAG)

        is_messier = bool(row.get("M", "").strip())

        # Magnitude / size filter
        if not is_messier:
            if mag > MAG_LIMIT and (maj is None or maj < MIN_SIZE_ARCMIN):
                skipped_faint += 1
                continue

        kind = TYPE_MAP[obj_type]
        name = make_name(row)

        # OpenNGC "Common names" is comma-separated; preserve every alias so
        # search can match "Orion Nebula" as well as "Great Orion Nebula".
        raw_common = (row.get("Common names") or "").strip()
        common_names = [n.strip() for n in raw_common.split(",") if n.strip()]

        objects.append({
            "ra_deg": ra,
            "dec_deg": dec,
            "name": name,
            "kind": kind,
            "mag": mag if mag is not None else MISSING_MAG,
            "size_arcmin": maj if maj else 0.0,
            "size_minor_arcmin": minor if minor else 0.0,
            "pa_deg": pa if pa else 0.0,
            "common_names": common_names,
        })

    fh.close()

    # Sort: Messier first (by number), then by magnitude
    def sort_key(o):
        if o["name"].startswith("M") and o["name"][1:].isdigit():
            return (0, int(o["name"][1:]))
        return (1, o["mag"])

    objects.sort(key=sort_key)

    print(f"Objects kept:    {len(objects):,}")
    print(f"Skipped (type):  {skipped_type:,}")
    print(f"Skipped (coord): {skipped_coords:,}")
    print(f"Skipped (faint): {skipped_faint:,}")

    # --- Emit binary ---
    # Format (little-endian):
    #   [u32] n_objects
    #   Objects: ra(f32) dec(f32) mag(f32) size_arcmin(f32) size_minor(f32) pa_deg(f32)
    #            kind(u8) name_len(u8) name(utf8)
    #            aliases_count(u8) [ alias_len(u8) alias(utf8) ] × aliases_count
    # kind codes: 0=Galaxy 1=OpenCluster 2=GlobularCluster 3=Nebula
    #             4=PlanetaryNebula 5=SupernovaRemnant 6=GalaxyCluster
    KIND_CODE = {
        "Galaxy": 0, "OpenCluster": 1, "GlobularCluster": 2,
        "Nebula": 3, "PlanetaryNebula": 4, "SupernovaRemnant": 5, "GalaxyCluster": 6,
    }
    buf = bytearray()
    buf += struct.pack('<I', len(objects))
    for o in objects:
        name_enc = o['name'].encode('utf-8')
        buf += struct.pack('<ffffff',
                           o['ra_deg'], o['dec_deg'], o['mag'],
                           o['size_arcmin'], o['size_minor_arcmin'], o['pa_deg'])
        buf += bytes([KIND_CODE[o['kind']], len(name_enc)]) + name_enc

        # u8 length fields cap at 255. Skip any alias that won't fit and cap
        # the list at 255 entries so the wire format stays uncorrupted.
        aliases_enc = []
        for alias in o['common_names']:
            enc = alias.encode('utf-8')
            if 0 < len(enc) <= 255:
                aliases_enc.append(enc)
            if len(aliases_enc) == 255:
                break
        buf += bytes([len(aliases_enc)])
        for enc in aliases_enc:
            buf += bytes([len(enc)]) + enc

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, 'wb') as f:
        f.write(buf)
    print(f"Wrote {OUT} ({len(buf):,} bytes)")


if __name__ == "__main__":
    main()
