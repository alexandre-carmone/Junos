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
import json
import math
import os
import re
import struct
import urllib.parse
import urllib.request

OPENGC_URL = (
    "https://github.com/mattiaverga/OpenNGC/raw/master/database_files/NGC.csv"
)
CACHE = os.path.join(os.path.dirname(__file__), "openngc.csv")
OUT = os.path.join(
    os.path.dirname(__file__), "..", "rekos-wasm", "public", "dso.bin"
)
WIKIDATA_FR_CACHE = os.path.join(os.path.dirname(__file__), "wikidata_fr_dso.json")
WIKIDATA_SPARQL_URL = "https://query.wikidata.org/sparql"

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


def _sparql_query(query):
    """POST a SPARQL query to Wikidata, return parsed JSON bindings list."""
    data = urllib.parse.urlencode({"query": query, "format": "json"}).encode("utf-8")
    req = urllib.request.Request(
        WIKIDATA_SPARQL_URL,
        data=data,
        headers={
            "User-Agent": "stars-dso-catalog-gen/1.0 (rekos-web)",
            "Accept": "application/sparql-results+json",
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    with urllib.request.urlopen(req, timeout=300) as resp:
        return json.loads(resp.read().decode("utf-8"))["results"]["bindings"]


QID_RE = re.compile(r"^Q\d+$")
# Match P528 catalog codes shaped like "NGC 224" / "NGC0224" / "IC 1" /
# "M 31" / "M31". The leading prefix tells us the catalog without needing
# to know Wikidata's catalog-item QIDs (which differ between catalogs and
# change over time).
CATCODE_RE = re.compile(r"^\s*(NGC|IC|M)\s*0*(\d+)\s*$", re.IGNORECASE)
# Pseudo-name pattern: any string that is just a catalog designation
# (possibly with a sub-component letter/dash and possibly a French
# conjunction joining a second designation). These show up as Wikidata FR
# labels for items with no real common name and add no search value.
DESIG_TOKEN = r"(?:NGC|IC|M|PGC|UGC|Gum|Sh2|HD|HIP|Caldwell|Mel|Stock|Cr|Tr|Abell|HCG)\s*\d+[\dA-Za-z\-]*"
PSEUDO_NAME_RE = re.compile(
    rf"^\s*{DESIG_TOKEN}(?:\s*(?:et|and|/|,)\s*{DESIG_TOKEN})*\s*$",
    re.IGNORECASE,
)


def _catcode_to_name(code):
    """Convert a Wikidata P528 catCode to our `make_name()` form, or None."""
    m = CATCODE_RE.match(code)
    if not m:
        return None
    prefix = m.group(1).upper()
    num = m.group(2)
    if prefix == "M":
        return f"M{int(num)}"
    return f"{prefix} {int(num)}"


def _candidate_catcodes(name):
    """All P528 catCode forms a Wikidata item might use for our display name.

    Generates the spaced form ("NGC 224") and the zero-padded form
    ("NGC0224") so VALUES queries hit either. Messier objects use both
    "M31" and "M 31" in the wild.
    """
    if name.startswith("M") and name[1:].isdigit():
        n = int(name[1:])
        return [f"M{n}", f"M {n}"]
    if name.startswith("NGC ") or name.startswith("IC "):
        prefix, num = name.split(" ", 1)
        if num.isdigit():
            n = int(num)
            width = 4 if prefix == "NGC" else 4
            return [f"{prefix} {n}", f"{prefix}{n:0{width}d}", f"{prefix} {n:0{width}d}"]
    return []


def fetch_wikidata_french_labels(object_names):
    """Return {dso_name: [fr_alias, ...]} keyed by our `make_name()` output.

    Uses a cached JSON file when present so reruns and offline builds work.
    Sends targeted VALUES queries in batches so the public WDQS endpoint
    stays well under its 60-second result-set timeout.
    """
    if os.path.exists(WIKIDATA_FR_CACHE):
        print(f"  Using cached Wikidata FR labels {WIKIDATA_FR_CACHE}")
        with open(WIKIDATA_FR_CACHE, encoding="utf-8") as f:
            return json.load(f)

    print("  Fetching French DSO labels from Wikidata...")
    # Reverse map: catCode → our display name. Multiple codes can map to
    # the same name; that's fine, the matcher just deduplicates aliases.
    code_to_name = {}
    for name in object_names:
        for code in _candidate_catcodes(name):
            code_to_name[code] = name

    all_codes = sorted(code_to_name.keys())
    fr_map = {}
    BATCH = 400
    for i in range(0, len(all_codes), BATCH):
        chunk = all_codes[i : i + BATCH]
        values = " ".join(f'"{c}"' for c in chunk)
        # P31/P279* wd:Q6999 (astronomical object) excludes the long tail of
        # unrelated Wikidata items that happen to share a P528 string —
        # without it M20 picks up "Parti démocrate", M25 picks up a Beethoven
        # sonata, etc.
        query = f"""
SELECT ?catCode ?frLabel ?enLabel WHERE {{
  VALUES ?catCode {{ {values} }}
  ?item wdt:P528 ?catCode.
  ?item wdt:P31/wdt:P279* wd:Q6999.
  ?item rdfs:label ?frLabel. FILTER(LANG(?frLabel) = "fr")
  OPTIONAL {{ ?item rdfs:label ?enLabel. FILTER(LANG(?enLabel) = "en") }}
}}
"""
        try:
            rows = _sparql_query(query)
        except Exception as e:
            print(f"  ! batch {i // BATCH} failed: {e}")
            continue
        for row in rows:
            fr = row.get("frLabel", {}).get("value")
            en = row.get("enLabel", {}).get("value", "")
            code = row.get("catCode", {}).get("value", "")
            name = code_to_name.get(code)
            if not fr or not name:
                continue
            if fr == en or fr == name or QID_RE.match(fr):
                continue
            # Drop FR labels that are themselves catalog designations
            # (e.g. Wikidata returns "IC 2169" as the FR label of IC 447,
            # or "NGC 858-1" / "PGC 70934" — none of these are real names).
            if PSEUDO_NAME_RE.match(fr):
                continue
            fr_map.setdefault(name, [])
            if fr not in fr_map[name]:
                fr_map[name].append(fr)
        print(f"  batch {i // BATCH + 1}/{(len(all_codes) + BATCH - 1) // BATCH}: {len(fr_map)} named so far")

    with open(WIKIDATA_FR_CACHE, "w", encoding="utf-8") as f:
        json.dump(fr_map, f, ensure_ascii=False, indent=2, sort_keys=True)
    print(f"  Cached to {WIKIDATA_FR_CACHE}")
    return fr_map


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

    # Attach French aliases from Wikidata (keyed by our make_name() output).
    fr_map = fetch_wikidata_french_labels([o["name"] for o in objects])
    fr_hits = 0
    for o in objects:
        fr_aliases = fr_map.get(o["name"], [])
        # Drop FR aliases that duplicate an existing English common name.
        existing = set(o["common_names"])
        fr_aliases = [a for a in fr_aliases if a not in existing]
        o["fr_names"] = fr_aliases
        if fr_aliases:
            fr_hits += 1
    print(f"Objects with FR names: {fr_hits:,}")

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
    #            fr_names_count(u8) [ fr_len(u8) fr(utf8) ] × fr_names_count
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

        # French aliases — same length-prefixed shape as common_names.
        fr_enc = []
        for fr in o.get('fr_names', []):
            enc = fr.encode('utf-8')
            if 0 < len(enc) <= 255:
                fr_enc.append(enc)
            if len(fr_enc) == 255:
                break
        buf += bytes([len(fr_enc)])
        for enc in fr_enc:
            buf += bytes([len(enc)]) + enc

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, 'wb') as f:
        f.write(buf)
    print(f"Wrote {OUT} ({len(buf):,} bytes)")


if __name__ == "__main__":
    main()
