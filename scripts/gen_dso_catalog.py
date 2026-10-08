#!/usr/bin/env python3
"""Generate junos-web/public/dso.bin, the planetarium's deep-sky catalog.

The base is OpenNGC: NGC.csv (NGC, IC, Messier) and addendum.csv (M45, the
Hyades, the Coathanger, the Horsehead, the Magellanic Clouds, …). On top of
it come the catalogs imagers reach for once the NGC runs out:

  * Sharpless HII regions (Sh2)             VizieR VII/20
  * van den Bergh reflection nebulae (vdB)  VizieR VII/21
  * Barnard dark nebulae (B)                VizieR VII/220A
  * Lynds dark nebulae (LDN, opacity >= 5)  VizieR VII/7A
  * Lynds bright nebulae (LBN, class <= 3,  VizieR VII/9
    not inside a larger nebula)
  * Abell planetary nebulae                 SIMBAD (PN A66)
  * Hickson compact groups (HCG)            VizieR VII/213
  * Arp peculiar galaxies                   VizieR VII/192
  * Abell galaxy clusters (distance <= 3)   VizieR VII/110A
  * Open clusters outside the NGC/IC        VizieR B/ocl (Collinder, Melotte,
    Stock, Trumpler, Berkeley, King, …)
  * Globular clusters outside the NGC/IC    VizieR VII/202 (Palomar, Terzan, …)

and, for the constellation of each object, Roman's boundary table (VI/42).

plus `dso_extra.csv`: curated EN/FR common names for any designation, and
exotic objects no bulk catalog covers (quasars, protoplanetary nebulae, faint
supernova remnants, Local Group dwarfs, …), placed by CDS Sesame or by the
coordinates given there. A designation named in that file is kept even when
its catalog's cut above would drop it.

An object listed in several catalogs is emitted once. SIMBAD's
cross-identifications and OpenNGC's own identifier column merge e.g. Sh2-117
into NGC 7000; failing that, an object sitting on an earlier one of the same
nature and size is merged by position (a vdB nebula is filed under its
illuminating star in SIMBAD, so only its position ties it to an NGC/IC
entry). The merged designations become the object's searchable `ids`.

Common names come from OpenNGC, from SIMBAD's NAME identifiers of extended
objects, from `dso_extra.csv` (the only source for most Sharpless, Barnard and
Lynds nicknames) and, in French, from Wikidata.

Every download is cached and committed (openngc.csv, openngc_addendum.csv,
wikidata_fr_dso.json, dso_sources/), so a rebuild is offline and reproducible.
`--refresh` refetches the VizieR, SIMBAD and Sesame sources.

Each object also gets its IAU constellation, looked up from its position in
Roman's table of the 1875 boundaries (VizieR VI/42), and an emission-line
signature: how much of it shows in Hα, OIII, SII and broadband (stars,
galaxies, reflection), 0–3 each — what is worth a filter. It is derived from
the kind and the evidence the catalogs give (OpenNGC's HII/EmN/RfN/Neb types
and Hubble type, SIMBAD's object type, Sharpless vs van den Bergh), and
`dso_extra.csv` overrides it for the objects that deserve better.

Fields emitted per object:
  ra_deg, dec_deg (J2000), name, kind, lines (emission signature), con (IAU
  abbreviation), mag (99 = unknown), size_arcmin (major), size_minor_arcmin,
  pa_deg, common_names (English), fr_names (French), ids (other designations,
  searchable but not displayed)

Usage:
    python3 scripts/gen_dso_catalog.py
    python3 scripts/gen_dso_catalog.py --refresh
"""

import argparse
import csv
import io
import json
import math
import os
import re
import struct
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))

OPENGC_URL = (
    "https://github.com/mattiaverga/OpenNGC/raw/master/database_files/NGC.csv"
)
OPENGC_ADDENDUM_URL = (
    "https://github.com/mattiaverga/OpenNGC/raw/master/database_files/addendum.csv"
)
CACHE = os.path.join(HERE, "openngc.csv")
ADDENDUM_CACHE = os.path.join(HERE, "openngc_addendum.csv")
OUT = os.path.join(HERE, "..", "junos-web", "public", "dso.bin")
WIKIDATA_FR_CACHE = os.path.join(HERE, "wikidata_fr_dso.json")
WIKIDATA_SPARQL_URL = "https://query.wikidata.org/sparql"
SOURCES_DIR = os.path.join(HERE, "dso_sources")
EXTRA_CSV = os.path.join(HERE, "dso_extra.csv")

VIZIER_URL = "https://vizier.cds.unistra.fr/viz-bin/asu-tsv"
SIMBAD_TAP_URL = "https://simbad.cds.unistra.fr/simbad/sim-tap/sync"
SESAME_URL = "https://cds.unistra.fr/cgi-bin/nph-sesame/-oxpI/S?"
USER_AGENT = "junos-dso-catalog-gen/2.0 (junos-web)"

# Set by --refresh: refetch the VizieR / SIMBAD / Sesame caches.
REFRESH = False

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
    "DrkN":   "DarkNebula",    # addendum only (B 33, the Coalsack)
}
# Only the addendum's asterisms are worth keeping (the Coathanger, the
# Double Cluster); NGC.csv's are mostly a few stars around a misidentified
# NGC number.
ADDENDUM_TYPE_MAP = {**TYPE_MAP, "*Ass": "OpenCluster"}

# Object types to skip entirely (stars, duplicates, non-existent, etc.)
SKIP_TYPES = {"*", "**", "*Ass", "Dup", "NonEx", "Other", "Nova"}

# Magnitude cut-off: objects fainter than this are dropped *unless* they are
# in the Messier catalog or have a large apparent size (>= MIN_SIZE_ARCMIN).
MAG_LIMIT = 14.0
MIN_SIZE_ARCMIN = 1.0  # keep anything with major axis >= 1 arcmin regardless of mag

MISSING_MAG = 99.0  # sentinel for "magnitude unknown"

# Cuts for the big catalogs, chosen to keep what is photographable.
LDN_MIN_OPACITY = 5   # Lynds' opacity class, 1 (faint) … 6 (darkest)
LBN_MAX_BRIGHT = 3    # Lynds' brightness class, 1 (brightest) … 6
ACO_MAX_DCLASS = 3    # Abell distance class, 0 (nearest) … 6
# Open-cluster catalogs kept from B/ocl; the rest (FSR, ASCC, ESO, Loden,
# Teutsch, …) are mostly survey detections no one goes looking for.
OCL_CATALOGS = {
    "Collinder", "Melotte", "Trumpler", "Stock", "King", "Berkeley", "Czernik",
    "Dolidze", "Harvard", "Basel", "Roslund", "Markarian", "Pismis", "Ruprecht",
    "Haffner", "Bochum", "Hogg", "Westerlund", "Tombaugh", "Biurakan", "Lynga",
    "Platais", "Alessi", "Waterloo", "Stephenson", "Blanco", "Mayer", "Danks",
    "Moffat",
}
# Short forms searched alongside the full catalog name ("Cr 399").
CATALOG_ABBREV = {
    "Collinder": "Cr", "Melotte": "Mel", "Trumpler": "Tr", "Berkeley": "Be",
    "Ruprecht": "Ru", "Czernik": "Cz", "Palomar": "Pal", "B": "Barnard",
    "HCG": "Hickson",
}

# Positional merge: an object absorbs a later one of the same group whose
# centre lies within 30% of the larger size and whose size is within this
# factor of its own (catalog sizes of one nebula easily differ 4×: M17 is
# 12.6' in OpenNGC, 60' as Sh2-45).
MERGE_MAX_SIZE_RATIO = 5.0
# Kind groups for the positional merge.
MERGE_GROUP = {
    "Nebula": "bright", "SupernovaRemnant": "bright", "PlanetaryNebula": "bright",
    "DarkNebula": "dark",
    "OpenCluster": "cluster", "GlobularCluster": "cluster",
    "Galaxy": "galaxy",
    "GalaxyCluster": "group",
}
# SIMBAD object types that are extended objects (as opposed to the star a
# reflection nebula is filed under) — only their NAME identifiers are used.
EXTENDED_OTYPES = {
    "HII", "RNe", "DNe", "MoC", "Cld", "SNR", "PN", "OpC", "Cl*", "GlC", "ClG",
    "GrG", "CGG", "SCG", "G", "GiG", "GiC", "GiP", "IG", "PaG", "ISM", "EmO",
    "bub", "SFR", "glb", "sh", "As*", "St*", "C?*", "LSB", "BiC", "SBG", "H2G",
    "Sy1", "Sy2", "SyG", "AGN", "LIN", "rG", "QSO", "BLL", "Bla", "ERO",
}
# A SIMBAD NAME is taken as a common name only when it reads like one: it
# ends with one of these nouns ("Parrot's Head", "Kutner's Cloud").
NAME_NOUNS = {
    "Nebula", "Cloud", "Loop", "Arc", "Ring", "Cluster", "Galaxy", "Group",
    "Quintet", "Sextet", "Septet", "Object", "Head", "Trunk", "Streamer",
    "Bubble", "Shell", "Filament", "Lane", "River", "Cave", "Rift", "Pillars",
    "Triplet", "Trio", "Chain",
}

# SIMBAD object type → DsoType, for objects placed by Sesame.
OTYPE_KIND = {
    "ClG": "GalaxyCluster", "GrG": "GalaxyCluster", "CGG": "GalaxyCluster",
    "SCG": "GalaxyCluster",
    "OpC": "OpenCluster", "Cl*": "OpenCluster", "As*": "OpenCluster",
    "GlC": "GlobularCluster",
    "PN": "PlanetaryNebula", "PN?": "PlanetaryNebula", "pA*": "PlanetaryNebula",
    "post-AGB*": "PlanetaryNebula",
    "SNR": "SupernovaRemnant", "SNR?": "SupernovaRemnant",
    "DNe": "DarkNebula", "MoC": "DarkNebula", "Cld": "DarkNebula",
    "glb": "DarkNebula",
    "HII": "Nebula", "RNe": "Nebula", "EmO": "Nebula", "ISM": "Nebula",
    "bub": "Nebula", "HH": "Nebula", "SFR": "Nebula",
}

KINDS = (
    "Galaxy", "OpenCluster", "GlobularCluster", "Nebula", "PlanetaryNebula",
    "SupernovaRemnant", "GalaxyCluster", "DarkNebula",
)

# ── Emission-line signature ──────────────────────────────────────────────────
# (Hα, OIII, SII, broadband), each 0 = nothing, 1 = optional, 2 = significant,
# 3 = dominant. Packed into one byte, two bits per band in that order.
LINE_BANDS = ("Ha", "OIII", "SII", "RGB")
SIG_BROADBAND = (0, 0, 0, 3)          # stars, galaxies, reflection, dust
SIG_SPIRAL = (1, 0, 0, 3)             # Hα lifts the HII regions of a spiral
SIG_EMISSION = (3, 1, 2, 0)           # HII region
SIG_CLUSTER_NEBULA = (3, 1, 2, 2)     # cluster in its HII region (Cl+N)
SIG_MIXED = (3, 1, 1, 2)              # emission and reflection together
SIG_BUBBLE = (3, 3, 1, 0)             # Wolf-Rayet shell: OIII as strong as Hα
SIG_PLANETARY = (2, 3, 1, 1)
SIG_SNR = (3, 2, 2, 0)
SIG_NEBULA = (2, 0, 0, 2)             # bright nebula of unknown nature
# A spiral or irregular Hubble type (not a lenticular S0): HII regions.
SPIRAL_RE = re.compile(r"^(S(AB|A|B)?[a-dm]|I)")
SPIRAL_MIN_ARCMIN = 5.0

# Evidence for the signature, per catalog: OpenNGC's type, SIMBAD's object
# type. "emn" emission, "rfn" reflection, "bub" Wolf-Rayet shell, "neb"
# bright nebula of unknown nature, "cln" cluster with nebula.
NGC_TYPE_HINT = {"HII": "emn", "EmN": "emn", "RfN": "rfn", "Neb": "neb", "Cl+N": "cln"}
OTYPE_HINT = {
    "HII": "emn", "EmO": "emn", "SFR": "emn", "RNe": "rfn", "bub": "bub", "WR*": "bub",
}


def otype_hints(otype, default=None):
    h = OTYPE_HINT.get(otype, default)
    return {h} if h else set()


# ── download + cache helpers ─────────────────────────────────────────────────

def http_get(url, data=None, timeout=300):
    req = urllib.request.Request(url, data=data, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.read().decode("utf-8")


def cached_source(fname, fetch):
    """Text of `dso_sources/<fname>`, fetched (and saved) when missing or on --refresh."""
    path = os.path.join(SOURCES_DIR, fname)
    if os.path.exists(path) and not REFRESH:
        with open(path, encoding="utf-8") as f:
            return f.read()
    print(f"  Fetching {fname} ...")
    text = fetch()
    os.makedirs(SOURCES_DIR, exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    return text


def vizier(table, columns, fname):
    """Rows of a VizieR table as dicts. Positions come as `_RAJ2000`/`_DEJ2000`
    (VizieR's own J2000 conversion of the catalog's epoch), in degrees."""
    def fetch():
        query = urllib.parse.urlencode({
            "-source": table,
            "-out": ",".join(columns),
            "-out.max": "unlimited",
            "-oc.form": "d",
        })
        text = http_get(f"{VIZIER_URL}?{query}")
        # Drop VizieR's comment preamble (it carries a timestamp) so the
        # cached file only changes when the data does.
        return "\n".join(l for l in text.splitlines() if l.strip() and not l.startswith("#")) + "\n"

    lines = cached_source(fname, fetch).splitlines()
    header = lines[0].split("\t")
    # lines[1] is units, lines[2] the dashes under the header.
    return [dict(zip(header, (c.strip() for c in line.split("\t")))) for line in lines[3:]]


def simbad(adql, fname):
    """Rows of a SIMBAD TAP query as dicts (string values, quotes stripped)."""
    def fetch():
        data = urllib.parse.urlencode({
            "request": "doQuery", "lang": "adql", "format": "tsv", "query": adql,
        }).encode("utf-8")
        return http_get(SIMBAD_TAP_URL, data=data)

    reader = csv.DictReader(io.StringIO(cached_source(fname, fetch)), delimiter="\t")
    return list(reader)


def sesame(names):
    """{name: {ra, dec, otype, oid, aliases} | None} via CDS Sesame (SIMBAD
    only), cached in dso_sources/sesame.json. Sesame resolves loose names
    ("Hoag's Object", "QSO B0957+561") that SIMBAD's TAP would need verbatim."""
    path = os.path.join(SOURCES_DIR, "sesame.json")
    cache = {}
    if os.path.exists(path) and not REFRESH:
        with open(path, encoding="utf-8") as f:
            cache = json.load(f)
    missing = [n for n in names if n not in cache]
    for name in missing:
        print(f"  Resolving {name!r} with Sesame ...")
        try:
            root = ET.fromstring(http_get(SESAME_URL + urllib.parse.quote(name), timeout=60))
        except Exception as e:
            print(f"  ! Sesame failed for {name!r}: {e}")
            continue
        res = root.find("./Target/Resolver")
        if res is None or res.find("jradeg") is None:
            cache[name] = None
            continue
        cache[name] = {
            "ra": float(res.findtext("jradeg")),
            "dec": float(res.findtext("jdedeg")),
            "otype": res.findtext("otype") or "",
            "oid": int(res.findtext("oid") or 0),
            "oname": res.findtext("oname") or "",
            "aliases": [a.text for a in res.findall("alias") if a.text],
        }
    # Keep only what dso_extra.csv still asks for.
    kept = {n: cache[n] for n in names if n in cache}
    if missing or kept.keys() != cache.keys():
        os.makedirs(SOURCES_DIR, exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            json.dump(kept, f, ensure_ascii=False, indent=1, sort_keys=True)
    return {n: kept.get(n) for n in names}


def load_csv():
    if os.path.exists(CACHE):
        print(f"  Using cached {CACHE}")
        return open(CACHE, encoding="utf-8")

    print(f"  Downloading {OPENGC_URL} ...")
    data = http_get(OPENGC_URL)
    with open(CACHE, "w", encoding="utf-8") as f:
        f.write(data)
    print(f"  Saved to {CACHE} ({len(data):,} bytes)")
    return io.StringIO(data)


def load_addendum_csv():
    if os.path.exists(ADDENDUM_CACHE):
        print(f"  Using cached {ADDENDUM_CACHE}")
        return open(ADDENDUM_CACHE, encoding="utf-8")

    print(f"  Downloading {OPENGC_ADDENDUM_URL} ...")
    data = http_get(OPENGC_ADDENDUM_URL)
    with open(ADDENDUM_CACHE, "w", encoding="utf-8") as f:
        f.write(data)
    return io.StringIO(data)


# ── parsing helpers ──────────────────────────────────────────────────────────

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
    s = (s or "").strip()
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


# addendum.csv keys ("Mel022", "Cl399", "ESO351-030") → catalog names.
ADDENDUM_PREFIX = {
    "B": "B", "Cl": "Collinder", "H": "Harvard", "HCG": "HCG", "Mel": "Melotte",
    "MWSC": "MWSC", "PGC": "PGC", "UGC": "UGC",
}


def addendum_name(row):
    if row.get("M", "").strip():
        return make_name(row)
    raw = row["Name"].strip()
    m = re.match(r"^ESO(\d+)-0*(\d+)$", raw)
    if m:
        return f"ESO {int(m[1])}-{int(m[2])}"
    m = re.match(r"^([A-Za-z]+?)0*(\d+)$", raw)
    if not m:
        return raw
    prefix, num = m[1], int(m[2])
    if prefix == "C":
        # Caldwell objects outside the NGC/IC: name them by their own
        # catalog when they have one (C9 is Sh2-155, C41 Melotte 25).
        for ident in split_list(row.get("Identifiers")):
            c = canon(ident)
            if c and (c.startswith("Sh2-") or c.startswith("Melotte ")):
                return c
        return f"Caldwell {num}"
    return f"{ADDENDUM_PREFIX.get(prefix, prefix)} {num}"


def addendum_own_desig(raw):
    """The addendum row's own designation ("Mel022" → "Melotte 22"), for
    rows displayed under another name (M45)."""
    m = re.match(r"^([A-Za-z]+?)0*(\d+)$", raw)
    if m and m[1] in ADDENDUM_PREFIX:
        return f"{ADDENDUM_PREFIX[m[1]]} {int(m[2])}"
    return None


def split_list(s):
    return [x.strip() for x in (s or "").split(",") if x.strip()]


def clean_common_name(s):
    """OpenNGC and SIMBAD sometimes write "the War and Peace Nebula"."""
    s = s.strip()
    return s[4:] if s[:4].lower() == "the " else s


# Designation → canonical form. Canonical forms are both the merge keys and
# what gets displayed / searched, so "SH  2-155" (SIMBAD), "S 155" (LBN's
# cross-reference) and "Sh2-155" all land on one entry.
_CANON = [
    (re.compile(r"^M\s*0*(\d+)$"), lambda m: f"M{int(m[1])}"),
    (re.compile(r"^NGC\s*0*(\d+)$"), lambda m: f"NGC {int(m[1])}"),
    (re.compile(r"^IC\s*0*(\d+)$"), lambda m: f"IC {int(m[1])}"),
    (re.compile(r"^S[Hh]\s*2\s*-\s*0*(\d+)$"), lambda m: f"Sh2-{int(m[1])}"),
    (re.compile(r"^S\s+0*(\d+)$"), lambda m: f"Sh2-{int(m[1])}"),  # LBN's cross-reference
    (re.compile(r"^LBN\s*0*(\d+)$"), lambda m: f"LBN {int(m[1])}"),
    (re.compile(r"^LDN\s*0*(\d+)$"), lambda m: f"LDN {int(m[1])}"),
    (re.compile(r"^(?:Barnard|B)\s*0*(\d+)$"), lambda m: f"B {int(m[1])}"),
    (re.compile(r"^VDB\s*0*(\d+)$", re.I), lambda m: f"vdB {int(m[1])}"),
    (re.compile(r"^PN\s+A66\s*0*(\d+)$"), lambda m: f"Abell {int(m[1])}"),
    (re.compile(r"^HCG\s*0*(\d+)$"), lambda m: f"HCG {int(m[1])}"),
    (re.compile(r"^(?:APG|Arp)\s*0*(\d+)$", re.I), lambda m: f"Arp {int(m[1])}"),
    (re.compile(r"^ACO\s*0*(\d+)$"), lambda m: f"ACO {int(m[1])}"),
    (re.compile(r"^Ced\s*0*(\d+)([a-z]?)$"), lambda m: f"Ced {int(m[1])}{m[2]}"),
    (re.compile(r"^UGC\s*0*(\d+)$"), lambda m: f"UGC {int(m[1])}"),
    (re.compile(r"^(?:Cl\s+)?(Collinder|Melotte|Mel|Cr)\s*0*(\d+)$"),
     lambda m: f"{ {'Mel': 'Melotte', 'Cr': 'Collinder'}.get(m[1], m[1]) } {int(m[2])}"),
    (re.compile(r"^(?:Cl\s+)?([A-Z][a-z]+)\s+0*(\d+)$"),
     lambda m: f"{m[1]} {int(m[2])}" if m[1] in OCL_CATALOGS else None),
]


def canon(s):
    s = (s or "").strip()
    for rx, fmt in _CANON:
        m = rx.match(s)
        if m:
            return fmt(m)
    return None


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


def _sparql_query(query):
    """POST a SPARQL query to Wikidata, return parsed JSON bindings list."""
    data = urllib.parse.urlencode({"query": query, "format": "json"}).encode("utf-8")
    req = urllib.request.Request(
        WIKIDATA_SPARQL_URL,
        data=data,
        headers={
            "User-Agent": USER_AGENT,
            "Accept": "application/sparql-results+json",
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    with urllib.request.urlopen(req, timeout=300) as resp:
        return json.loads(resp.read().decode("utf-8"))["results"]["bindings"]


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


# ── the merged catalog ───────────────────────────────────────────────────────

def add_unique(lst, values):
    seen = {v.lower() for v in lst}
    for v in values:
        if v and v.lower() not in seen:
            lst.append(v)
            seen.add(v.lower())


def angular_sep_arcmin(ra1, dec1, ra2, dec2):
    r1, d1, r2, d2 = map(math.radians, (ra1, dec1, ra2, dec2))
    c = math.sin(d1) * math.sin(d2) + math.cos(d1) * math.cos(d2) * math.cos(r1 - r2)
    return math.degrees(math.acos(max(-1.0, min(1.0, c)))) * 60.0


class Catalog:
    """Objects plus the indexes the merge needs: designation → object, and a
    1° grid for the positional fallback."""

    def __init__(self):
        self.objects = []
        self.by_desig = {}
        self.grid = {}
        self.stats = {}

    def lookup(self, desig):
        return self.by_desig.get(desig) if desig else None

    def register(self, obj, desig, searchable=True):
        """Tie `desig` to `obj`; a searchable one also lands in its `ids`."""
        if not desig:
            return
        self.by_desig.setdefault(desig, obj)
        if searchable and desig != obj["name"]:
            add_unique(obj["ids"], [desig])

    def add(self, obj, desig_keys=(), source=""):
        obj.setdefault("common_names", [])
        obj.setdefault("fr_names", [])
        obj.setdefault("ids", [])
        self.objects.append(obj)
        self.register(obj, obj["name"])
        for d in desig_keys:
            self.register(obj, d)
        cell = (int(obj["dec_deg"] + 90), int(obj["ra_deg"]) % 360)
        self.grid.setdefault(cell, []).append(obj)
        self.stats[source] = self.stats.get(source, 0) + 1
        return obj

    def near(self, ra, dec, kind, size):
        """An earlier object of the same group on top of (ra, dec) with a
        comparable size, or None."""
        group = MERGE_GROUP[kind]
        best, best_sep = None, None
        dec_cell = int(dec + 90)
        # Cells within reach of the largest object that could still match
        # (3× this one's size); RA cells shrink towards the poles.
        reach = 1 + int(max(2.0, 0.3 * MERGE_MAX_SIZE_RATIO * (size or 0.0)) / 60.0)
        ra_span = min(180, int(reach / max(0.05, math.cos(math.radians(dec)))) + 1)
        for dc in range(-reach, reach + 1):
            for rc in range(-ra_span, ra_span + 1):
                for o in self.grid.get((dec_cell + dc, (int(ra) + rc) % 360), ()):
                    if group not in o.get("merge_groups", {MERGE_GROUP[o["kind"]]}):
                        continue
                    s1, s2 = size or 0.0, o["size_arcmin"] or 0.0
                    if s1 > 0 and s2 > 0 and max(s1, s2) / min(s1, s2) > MERGE_MAX_SIZE_RATIO:
                        continue
                    radius = max(2.0, 0.3 * max(s1, s2))
                    sep = angular_sep_arcmin(ra, dec, o["ra_deg"], o["dec_deg"])
                    if sep <= radius and (best_sep is None or sep < best_sep):
                        best, best_sep = o, sep
        return best

    def inside_larger(self, ra, dec, kind, size):
        """Whether (ra, dec) falls inside an earlier, at least twice larger
        object of the same group — a fragment of it (LBN cuts NGC 7000 into
        a dozen pieces)."""
        group = MERGE_GROUP[kind]
        reach = 6  # degrees: the largest nebulae (Barnard's Loop) are ~10° across
        ra_span = min(180, int(reach / max(0.05, math.cos(math.radians(dec)))) + 1)
        for dc in range(-reach, reach + 1):
            for rc in range(-ra_span, ra_span + 1):
                for o in self.grid.get((int(dec + 90) + dc, (int(ra) + rc) % 360), ()):
                    if group not in o.get("merge_groups", {MERGE_GROUP[o["kind"]]}):
                        continue
                    if o["size_arcmin"] < 2.0 * (size or 0.0):
                        continue
                    if angular_sep_arcmin(ra, dec, o["ra_deg"], o["dec_deg"]) <= 0.5 * o["size_arcmin"]:
                        return True
        return False

    def merge_or_add(self, entry, desig, xids=(), names=(), source="", positional=True):
        """Fold catalog `entry` (designation `desig`) into the object it
        duplicates, or add it. `xids` are canonical designations of the same
        object elsewhere (SIMBAD / catalog cross-references)."""
        target = self.lookup(desig)
        if target is None:
            for x in xids:
                target = self.lookup(x)
                if target is not None:
                    break
        if target is None and positional:
            target = self.near(entry["ra_deg"], entry["dec_deg"], entry["kind"], entry["size_arcmin"])
        if target is not None:
            self.register(target, desig)
            for x in xids:
                self.register(target, x)
            if not target["size_arcmin"] and entry["size_arcmin"]:
                for k in ("size_arcmin", "size_minor_arcmin", "pa_deg"):
                    target[k] = entry[k]
            if target["mag"] >= MISSING_MAG and entry["mag"] < MISSING_MAG:
                target["mag"] = entry["mag"]
            add_unique(target.setdefault("simbad_names", []), names)
            target.setdefault("hints", set()).update(entry.get("hints", ()))
            self.stats[source + " (merged)"] = self.stats.get(source + " (merged)", 0) + 1
            return target
        entry["simbad_names"] = list(names)
        return self.add(entry, [desig, *xids], source)

    def apply_simbad_names(self):
        """SIMBAD's names, as a last resort for objects nothing else names
        (it has "Bermuda Cluster" for NGC 7000) and only when no other object
        goes by them (it also calls Ced 201 the "Cave Nebula")."""
        taken = {n.lower() for o in self.objects for n in o["common_names"]}
        for o in self.objects:
            names = [n for n in o.pop("simbad_names", []) if n.lower() not in taken]
            if not o["common_names"]:
                add_unique(o["common_names"], names)
                taken.update(n.lower() for n in names)


def new_entry(name, ra, dec, kind, mag=MISSING_MAG, size=0.0, size_minor=0.0, pa=0.0, hints=()):
    return {
        "name": name, "ra_deg": ra, "dec_deg": dec, "kind": kind,
        "mag": MISSING_MAG if mag is None else mag,
        "size_arcmin": size or 0.0, "size_minor_arcmin": size_minor or 0.0,
        "pa_deg": pa or 0.0, "hints": set(hints),
    }


# ── OpenNGC ──────────────────────────────────────────────────────────────────

def load_openngc(cat):
    print("Loading OpenNGC catalog...")
    skipped_type = skipped_coords = skipped_faint = 0
    dups = []  # (dup designation, primary designation)

    for fh, type_map, addendum in ((load_csv(), TYPE_MAP, False),
                                   (load_addendum_csv(), ADDENDUM_TYPE_MAP, True)):
        for row in csv.DictReader(fh, delimiter=";"):
            obj_type = row.get("Type", "").strip()

            if obj_type == "Dup":
                # A duplicate entry: keep its designation as an id of the
                # primary (IC 2118 → NGC 1909, the Witch Head).
                own = make_name({"Name": row["Name"]})
                primary = (f"M{int(row['M'])}" if row.get("M", "").strip()
                           else f"NGC {int(row['NGC'])}" if row.get("NGC", "").strip().isdigit()
                           else f"IC {int(row['IC'])}" if row.get("IC", "").strip().isdigit()
                           else None)
                if addendum and row["Name"].startswith("M"):
                    own = f"M{int(row['Name'][1:])}"
                if primary:
                    dups.append((own, primary))
                skipped_type += 1
                continue

            # Skip non-DSO types
            if obj_type not in type_map or (obj_type in SKIP_TYPES and not addendum):
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

            # Magnitude / size filter (the addendum is hand-picked: keep it all)
            if not is_messier and not addendum:
                if mag > MAG_LIMIT and (maj is None or maj < MIN_SIZE_ARCMIN):
                    skipped_faint += 1
                    continue

            name = addendum_name(row) if addendum else make_name(row)
            hints = {NGC_TYPE_HINT[obj_type]} if obj_type in NGC_TYPE_HINT else set()
            if SPIRAL_RE.match(row.get("Hubble", "").strip()):
                hints.add("spiral")
            obj = new_entry(name, ra, dec, type_map[obj_type], mag, maj, minor, pa, hints)
            # OpenNGC's constellation, to check ours against.
            obj["ngc_const"] = row.get("Const", "").strip()
            if obj_type == "Cl+N":
                # Filed as a cluster (M42, M17, IC 1396), but the Sharpless
                # region on top of it is the same object.
                obj["merge_groups"] = {"cluster", "bright"}

            # OpenNGC "Common names" is comma-separated; preserve every alias so
            # search can match "Orion Nebula" as well as "Great Orion Nebula".
            obj["common_names"] = []
            add_unique(obj["common_names"], [clean_common_name(n) for n in split_list(row.get("Common names"))])
            cat.add(obj, source="OpenNGC addendum" if addendum else "OpenNGC")

            # A Messier object is named "M42" — keep its NGC/IC number searchable.
            raw = row["Name"].strip()
            own = addendum_own_desig(raw) if addendum else make_name({"Name": raw})
            if own and own != name:
                cat.register(obj, own)
            # Identifiers: Caldwell and LBN numbers are searchable; the rest
            # (UGC, PGC, MWSC, …) only serve to recognise a duplicate.
            for ident in split_list(row.get("Identifiers")):
                m = re.match(r"^C\s*0*(\d+)$", ident)
                if m:
                    cat.register(obj, f"Caldwell {int(m[1])}")
                    cat.register(obj, f"C {int(m[1])}")
                    continue
                c = canon(ident)
                if c:
                    cat.register(obj, c, searchable=not c.startswith("UGC "))
            if addendum:
                m = re.match(r"^C0*(\d+)$", raw)
                if m:
                    cat.register(obj, f"Caldwell {int(m[1])}")
                    cat.register(obj, f"C {int(m[1])}")
        fh.close()

    for own, primary in dups:
        target = cat.lookup(primary)
        if target is not None:
            cat.register(target, own)

    print(f"Skipped (type):  {skipped_type:,}")
    print(f"Skipped (coord): {skipped_coords:,}")
    print(f"Skipped (faint): {skipped_faint:,}")


# ── SIMBAD cross-identifications ─────────────────────────────────────────────

# Identifier prefixes worth fetching on the other side of a cross-match.
XID_OTHER = [
    "NAME %", "NGC %", "IC %", "M %", "SH %", "LBN %", "LDN %", "Barnard %",
    "VDB %", "PN A66 %", "HCG %", "APG %", "ACO %", "Ced %", "UGC %",
    "Cl Collinder %", "Cl Melotte %", "Cl Trumpler %", "Cl Stock %",
]


def simbad_xids(key, patterns):
    """{canonical designation: {"otype": str, "xids": set, "names": list}} for
    every SIMBAD object with an identifier matching one of `patterns`."""
    where_self = " OR ".join(f"i1.id LIKE '{p}'" for p in patterns)
    where_other = " OR ".join(f"i2.id LIKE '{p}'" for p in XID_OTHER)
    rows = simbad(
        "SELECT i1.id AS cat_id, b.otype AS otype, i2.id AS other "
        "FROM ident AS i1 JOIN ident AS i2 ON i2.oidref = i1.oidref "
        "JOIN basic AS b ON b.oid = i1.oidref "
        f"WHERE ({where_self}) AND ({where_other})",
        f"simbad_xid_{key}.tsv",
    )
    out = {}
    for r in rows:
        own = canon(r["cat_id"])
        if not own:
            continue
        e = out.setdefault(own, {"otype": r["otype"], "xids": set(), "names": []})
        other = r["other"]
        if other.startswith("NAME "):
            n = clean_common_name(other[5:])
            if r["otype"] in EXTENDED_OTYPES and plausible_common_name(n):
                add_unique(e["names"], [n])
            continue
        c = canon(other)
        if c and c != own:
            e["xids"].add(c)
    return out


# Constellation and Bayer-letter abbreviations mark SIMBAD's shorthand names
# ("Per Cluster", "lam Ori Molecular Ring"). Leo is also a word.
ABBREV_WORDS = set("""
And Ant Aps Aqr Aql Ara Ari Aur Boo Cae Cam Cnc CVn CMa CMi Cap Car Cas Cen
Cep Cet Cha Cir Col Com CrA CrB Crv Crt Cru Cyg Del Dor Dra Equ Eri For Gem
Gru Her Hor Hya Hyi Ind Lac LMi Lep Lib Lup Lyn Lyr Men Mic Mon Mus Nor Oct
Oph Ori Pav Peg Per Phe Pic PsA Psc Pup Pyx Ret Sge Sgr Sco Scl Sct Ser Sex
Tau Tel TrA Tri UMa UMi Vel Vir Vol Vul
alf bet gam del eps zet eta tet iot kap lam mu nu ksi omi pi rho sig tau ups
phi chi psi ome
""".split())


def plausible_common_name(n):
    words = n.split()
    return (
        n[:1].isupper()
        and not any(ch.isdigit() for ch in n)
        and n != n.upper()
        and len(words) >= 2
        and words[-1] in NAME_NOUNS
        and not any(w in ABBREV_WORDS for w in words)
        and not PSEUDO_NAME_RE.match(n)
    )


def xid_info(xids, desig):
    e = xids.get(desig)
    if not e:
        return [], [], ""
    # Sorted: set order changes from run to run (string hashing), and the
    # first cross-id found decides which object absorbs this one.
    return sorted(e["xids"]), e["names"], e["otype"]


def nebula_kind(default, otype):
    """A nebula catalog's default kind, unless SIMBAD knows better."""
    return {"PN": "PlanetaryNebula", "SNR": "SupernovaRemnant"}.get(otype, default)


# ── the bulk catalogs ────────────────────────────────────────────────────────

def load_harris_gcs(cat):
    rows = vizier("VII/202/catalog", ["_RAJ2000", "_DEJ2000", "ID", "Name", "Vt", "Rh"],
                  "vizier_harris_gc.tsv")
    for r in rows:
        ident = r["ID"]
        if re.match(r"^(NGC|IC)\s", ident):
            continue  # already in OpenNGC
        m = re.match(r"^(Pal|Djorg|Rup|Ton)\s+(\d+)$", ident)
        if m:
            full = {"Pal": "Palomar", "Djorg": "Djorgovski", "Rup": "Ruprecht",
                    "Ton": "Tonantzintla"}[m[1]]
            name, ids = f"{full} {m[2]}", [ident]
        elif ident == "1636-283":
            name, ids = "ESO 452-11", [ident]
        elif ident == "Arp 2":
            # Not Arp's atlas entry 2 (a galaxy): SIMBAD's "Cl Arp 2".
            name, ids = "Cl Arp 2", []
        else:
            name, ids = ident, []
        rh = parse_float(r["Rh"], 0.0)
        e = new_entry(name, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "GlobularCluster",
                      parse_float(r["Vt"], MISSING_MAG), 4.0 * rh if rh else 2.0)
        obj = cat.merge_or_add(e, name, source="Harris globular clusters", positional=False)
        for i in ids:
            cat.register(obj, i)


def load_open_clusters(cat):
    rows = vizier("B/ocl/clusters", ["_RAJ2000", "_DEJ2000", "Cluster", "Diam"],
                  "vizier_dias_ocl.tsv")
    xids = simbad_xids("ocl", [f"Cl {c} %" for c in sorted(OCL_CATALOGS)])
    for r in rows:
        m = re.match(r"^([A-Za-z]+)[ _](\d+)$", r["Cluster"])
        if not m or m[1] not in OCL_CATALOGS:
            continue
        desig = f"{m[1]} {int(m[2])}"
        x, names, _ = xid_info(xids, desig)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "OpenCluster",
                      size=parse_float(r["Diam"], 0.0))
        obj = cat.merge_or_add(e, desig, x, names, source="Open clusters (B/ocl)")
        if m[1] in CATALOG_ABBREV:
            cat.register(obj, f"{CATALOG_ABBREV[m[1]]} {int(m[2])}")


def load_sharpless(cat):
    rows = vizier("VII/20/catalog", ["_RAJ2000", "_DEJ2000", "Sh2", "Diam"], "vizier_sh2.tsv")
    xids = simbad_xids("sh2", ["SH %"])
    for r in rows:
        if not r["Sh2"].isdigit():
            continue
        desig = f"Sh2-{int(r['Sh2'])}"
        x, names, otype = xid_info(xids, desig)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]),
                      nebula_kind("Nebula", otype), size=parse_float(r["Diam"], 0.0),
                      hints=otype_hints(otype, "emn"))  # Sharpless: HII regions
        cat.merge_or_add(e, desig, x, names, source="Sharpless")


def load_vdb(cat):
    rows = vizier("VII/21/catalog", ["_RAJ2000", "_DEJ2000", "VdB", "BRadMax", "RRadMax"],
                  "vizier_vdb.tsv")
    # SIMBAD files most vdB numbers under the illuminating star, so its
    # cross-ids (and star names) are not the nebula's: position only.
    for r in rows:
        if not r["VdB"].isdigit():
            continue
        desig = f"vdB {int(r['VdB'])}"
        radius = max(parse_float(r["BRadMax"], 0.0), parse_float(r["RRadMax"], 0.0))
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "Nebula",
                      size=2.0 * radius, hints={"rfn"})
        cat.merge_or_add(e, desig, source="van den Bergh")


def load_abell_pn(cat):
    rows = simbad(
        "SELECT i.id AS ident, b.ra AS ra_deg, b.dec AS dec_deg, b.otype AS otype, "
        "b.galdim_majaxis AS maj_axis, b.galdim_minaxis AS min_axis, b.galdim_angle AS pos_angle "
        "FROM ident AS i JOIN basic AS b ON b.oid = i.oidref WHERE i.id LIKE 'PN A66 %'",
        "simbad_abell_pn.tsv",
    )
    xids = simbad_xids("a66", ["PN A66 %"])
    for r in rows:
        desig = canon(r["ident"])
        if not desig or not r["ra_deg"]:
            continue
        x, names, otype = xid_info(xids, desig)
        e = new_entry(desig, float(r["ra_deg"]), float(r["dec_deg"]),
                      nebula_kind("PlanetaryNebula", otype),
                      size=parse_float(r["maj_axis"], 1.0), size_minor=parse_float(r["min_axis"], 0.0),
                      pa=parse_float(r["pos_angle"], 0.0))
        cat.merge_or_add(e, desig, x, names, source="Abell planetary nebulae")


def load_barnard(cat):
    rows = vizier("VII/220A/barnard", ["_RAJ2000", "_DEJ2000", "Barn", "Diam"],
                  "vizier_barnard.tsv")
    xids = simbad_xids("barnard", ["Barnard %"])
    for r in rows:
        m = re.match(r"^(\d+)$", r["Barn"])
        if not m:
            continue  # "59a"-style sub-clouds
        desig = f"B {int(m[1])}"
        x, names, _ = xid_info(xids, desig)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "DarkNebula",
                      size=parse_float(r["Diam"], 0.0))
        obj = cat.merge_or_add(e, desig, x, names, source="Barnard")
        cat.register(obj, f"Barnard {int(m[1])}")


def load_ldn(cat, forced):
    rows = vizier("VII/7A/ldn", ["_RAJ2000", "_DEJ2000", "LDN", "Area", "Opacity", "Barn"],
                  "vizier_ldn.tsv")
    xids = simbad_xids("ldn", ["LDN %"])
    for r in rows:
        if not r["LDN"].isdigit():
            continue  # unnumbered sub-clouds
        desig = f"LDN {int(r['LDN'])}"
        barnard = [f"B {int(b)}" for b in r["Barn"].split() if b.isdigit()]
        known = any(cat.lookup(b) for b in barnard)
        if int(r["Opacity"] if r["Opacity"].isdigit() else 0) < LDN_MIN_OPACITY and desig not in forced and not known:
            continue
        x, names, _ = xid_info(xids, desig)
        area = parse_float(r["Area"], 0.0)  # deg²; size = diameter of the same area
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "DarkNebula",
                      size=2.0 * math.sqrt(area / math.pi) * 60.0)
        cat.merge_or_add(e, desig, [*barnard, *x], names, source="Lynds dark nebulae")


def load_lbn(cat, forced):
    rows = vizier("VII/9/catalog", ["_RAJ2000", "_DEJ2000", "Seq", "Diam1", "Diam2", "Bright", "Name"],
                  "vizier_lbn.tsv")
    xids = simbad_xids("lbn", ["LBN %"])
    for r in rows:
        if not r["Seq"].isdigit():
            continue
        desig = f"LBN {int(r['Seq'])}"
        # LBN's own cross-reference: "S 155" (Sharpless), "C 214" (Cederblad), NGC/IC.
        ref = r["Name"].strip()
        ref = re.sub(r"^C\s+", "Ced ", ref)
        own_xid = [c for c in [canon(ref)] if c]
        x, names, otype = xid_info(xids, desig)
        known = cat.lookup(desig) or any(cat.lookup(c) for c in [*own_xid, *x])
        if int(r["Bright"] if r["Bright"].isdigit() else 9) > LBN_MAX_BRIGHT and desig not in forced and not known:
            continue
        d1, d2 = parse_float(r["Diam1"], 0.0), parse_float(r["Diam2"], 0.0)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]),
                      nebula_kind("Nebula", otype), size=max(d1, d2), size_minor=min(d1, d2),
                      hints=otype_hints(otype, "neb"))
        # A fragment of a larger nebula is dropped — unless it is that
        # nebula under another number, which the merge below folds in.
        if (not known and desig not in forced
                and cat.near(e["ra_deg"], e["dec_deg"], e["kind"], e["size_arcmin"]) is None
                and cat.inside_larger(e["ra_deg"], e["dec_deg"], e["kind"], e["size_arcmin"])):
            continue
        cat.merge_or_add(e, desig, [*own_xid, *x], names, source="Lynds bright nebulae")


def load_hickson(cat):
    rows = vizier("VII/213/groups", ["_RAJ2000", "_DEJ2000", "HCG", "AngSize", "Totmag"],
                  "vizier_hcg.tsv")
    xids = simbad_xids("hcg", ["HCG %"])
    for r in rows:
        if not r["HCG"].isdigit():
            continue
        desig = f"HCG {int(r['HCG'])}"
        x, names, _ = xid_info(xids, desig)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "GalaxyCluster",
                      parse_float(r["Totmag"], MISSING_MAG), parse_float(r["AngSize"], 0.0))
        obj = cat.merge_or_add(e, desig, x, names, source="Hickson groups", positional=False)
        cat.register(obj, f"Hickson {int(r['HCG'])}")


def load_arp(cat):
    rows = vizier("VII/192/arpord", ["_RAJ2000", "_DEJ2000", "Arp", "Name", "Size"],
                  "vizier_arp.tsv")
    for r in rows:
        if not r["Arp"].isdigit():
            continue
        desig = f"Arp {int(r['Arp'])}"
        # "NGC 4038 + 39", "MESSIER 65 + 66", "UGC 01810 + 13": the first one.
        m = re.match(r"^(NGC|IC|UGC|MESSIER)\s*0*(\d+)", r["Name"])
        principal = (canon(f"{'M' if m[1] == 'MESSIER' else m[1]} {m[2]}") if m
                     else "HCG 92" if r["Name"].startswith("Stephan") else None)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "Galaxy",
                      size=parse_float(r["Size"], 0.0))
        obj = cat.merge_or_add(e, desig, [principal] if principal else [],
                               source="Arp peculiar galaxies", positional=False)
        if principal and principal != obj["name"]:
            cat.register(obj, principal)


def load_abell_clusters(cat, forced):
    cols = ["_RAJ2000", "_DEJ2000", "ACO", "Dclass", "z", "m10"]
    rows = (vizier("VII/110A/table3", cols, "vizier_aco_north.tsv")
            + vizier("VII/110A/table4", cols, "vizier_aco_south.tsv"))
    xids = simbad_xids("aco", ["ACO %"])
    for r in rows:
        if not r["ACO"].isdigit():
            continue
        n = int(r["ACO"])
        desig = f"ACO {n}"
        if int(r["Dclass"] if r["Dclass"].isdigit() else 9) > ACO_MAX_DCLASS and desig not in forced:
            continue
        # Abell radius is 1.72'/z; draw the core, half of it. Without a
        # redshift, estimate it from the 10th-brightest member (Coma: m10
        # 13.5, z 0.023).
        z = parse_float(r["z"], 0.0)
        if not z:
            m10 = parse_float(r["m10"], 0.0)
            z = 10 ** (0.2 * m10 - 4.34) if m10 else 0.05
        x, names, _ = xid_info(xids, desig)
        e = new_entry(desig, float(r["_RAJ2000"]), float(r["_DEJ2000"]), "GalaxyCluster",
                      size=min(1.72 / z, 300.0))
        cat.merge_or_add(e, desig, x, names, source="Abell galaxy clusters", positional=False)


def name_abell_clusters(cat):
    """Abell galaxy clusters go by "Abell 1656" — except where an Abell
    planetary nebula already holds the number (both catalogs start at 1)."""
    for o in cat.objects:
        m = re.match(r"^ACO (\d+)$", o["name"])
        if not m:
            continue
        abell = f"Abell {m[1]}"
        if cat.lookup(abell) is None:
            o["name"] = abell
            cat.register(o, abell, searchable=False)
            cat.register(o, f"ACO {m[1]}")


# ── dso_extra.csv ────────────────────────────────────────────────────────────

def read_extra():
    rows = []
    with open(EXTRA_CSV, encoding="utf-8") as f:
        lines = [l for l in f if l.strip() and not l.lstrip().startswith("#")]
    for r in csv.DictReader(lines, delimiter=";"):
        rows.append({k: (v or "").strip() for k, v in r.items()})
    return rows


def parse_size(s):
    """'60' / '60x10' / '60x10@45' → (major, minor, pa) arcmin/deg."""
    m = re.match(r"^([\d.]+)(?:x([\d.]+))?(?:@([\d.]+))?$", s)
    if not m:
        return None
    return float(m[1]), float(m[2] or 0.0), float(m[3] or 0.0)


def parse_lines(s, name):
    """'Ha3 OIII2 SII1 RGB2' → (3, 2, 1, 2); a band left out is 0."""
    sig = dict.fromkeys(LINE_BANDS, 0)
    for tok in s.split():
        m = re.match(r"^(Ha|OIII|SII|RGB)([0-3])$", tok)
        if not m:
            raise SystemExit(f"dso_extra.csv: {name}: bad lines token {tok!r}")
        sig[m[1]] = int(m[2])
    return tuple(sig[b] for b in LINE_BANDS)


def apply_extra(cat, rows):
    to_resolve = [r["resolve"] for r in rows
                  if r["resolve"] and not re.match(r"^[\d.]+\s+[-+]?[\d.]+$", r["resolve"])]
    resolved = sesame(to_resolve)

    oids = sorted({v["oid"] for v in resolved.values() if v})
    dims = {}
    if oids:
        dim_rows = simbad(
            "SELECT b.oid AS oid, b.galdim_majaxis AS maj_axis, b.galdim_minaxis AS min_axis, "
            "b.galdim_angle AS pos_angle, f.V AS vmag FROM basic AS b "
            "LEFT JOIN allfluxes AS f ON f.oidref = b.oid "
            f"WHERE b.oid IN ({','.join(str(o) for o in oids)})",
            "simbad_extra_dims.tsv",
        )
        dims = {int(r["oid"]): r for r in dim_rows}

    for r in rows:
        name = r["name"]
        obj = cat.lookup(canon(name) or name) or cat.lookup(name)
        if obj is None and r["resolve"]:
            coords = re.match(r"^([\d.]+)\s+([-+]?[\d.]+)$", r["resolve"])
            if coords:
                ra, dec, otype, aliases, d = float(coords[1]), float(coords[2]), "", [], {}
            else:
                hit = resolved.get(r["resolve"])
                if not hit:
                    print(f"  ! {name}: Sesame could not resolve {r['resolve']!r}")
                    continue
                ra, dec, otype, aliases = hit["ra"], hit["dec"], hit["otype"], hit["aliases"]
                d = dims.get(hit["oid"], {})
            kind = r["kind"] or OTYPE_KIND.get(otype, "Galaxy")
            size = parse_size(r["size"]) or (parse_float(d.get("maj_axis"), 1.0),
                                             parse_float(d.get("min_axis"), 0.0),
                                             parse_float(d.get("pos_angle"), 0.0))
            mag = parse_float(r["mag"], None)
            if mag is None and kind in ("Galaxy", "GlobularCluster"):
                mag = parse_float(d.get("vmag"), None)
            e = new_entry(name, ra, dec, kind, mag, *size, hints=otype_hints(otype))
            xids = [c for c in (canon(a) for a in aliases) if c and c != canon(name)]
            obj = cat.merge_or_add(e, canon(name) or name, xids, source="dso_extra.csv",
                                   positional=False)
            if obj is not e:
                print(f"  note: {name} merged into {obj['name']}")
        elif obj is None:
            print(f"  ! {name}: not in any catalog and no 'resolve' given — skipped")
            continue

        if r["kind"]:
            if r["kind"] not in KINDS:
                raise SystemExit(f"dso_extra.csv: {name}: unknown kind {r['kind']!r}")
            obj["kind"] = r["kind"]
        if parse_size(r["size"]):
            obj["size_arcmin"], obj["size_minor_arcmin"], obj["pa_deg"] = parse_size(r["size"])
        if r["mag"]:
            obj["mag"] = float(r["mag"])
        if r.get("lines"):
            obj["lines"] = parse_lines(r["lines"], name)

        # Curated names go first: they are the ones to display. A leading
        # '=' drops the names the catalogs gave instead.
        for col, key in (("en", "common_names"), ("fr", "fr_names")):
            replace = r[col].startswith("=")
            names = [n.strip() for n in r[col].lstrip("=").split("|") if n.strip()]
            lower = {n.lower() for n in names}
            kept = [] if replace else [n for n in obj[key] if n.lower() not in lower]
            obj[key] = names + kept
        for i in (x.strip() for x in r["ids"].split("|")):
            if i:
                cat.register(obj, i)


def forced_designations(rows):
    """Designations dso_extra.csv names: kept even below their catalog's cut."""
    out = set()
    for r in rows:
        out.add(canon(r["name"]) or r["name"])
        out.update(canon(i) or i for i in r["ids"].split("|") if i.strip())
    return out


# ── constellation and emission signature ─────────────────────────────────────

B1875_JD = 2405889.258550475


def precess_from_j2000(ra, dec, jd):
    """J2000 RA/Dec (deg) to the mean equinox of `jd`: IAU 1976 (Lieske), as
    `precess_j2000_to_jnow` in coords.rs."""
    t = (jd - 2451545.0) / 36525.0
    arcsec = lambda x: math.radians(x / 3600.0)
    zeta = arcsec((0.017998 * t + 0.30188) * t * t + 2306.2181 * t)
    z = arcsec((0.018203 * t + 1.09468) * t * t + 2306.2181 * t)
    theta = arcsec((-0.041833 * t - 0.42665) * t * t + 2004.3109 * t)
    ra0, dec0 = math.radians(ra), math.radians(dec)
    a = math.cos(dec0) * math.sin(ra0 + zeta)
    b = math.cos(theta) * math.cos(dec0) * math.cos(ra0 + zeta) - math.sin(theta) * math.sin(dec0)
    c = math.sin(theta) * math.cos(dec0) * math.cos(ra0 + zeta) + math.cos(theta) * math.sin(dec0)
    return math.degrees(math.atan2(a, b) + z) % 360.0, math.degrees(math.asin(max(-1.0, min(1.0, c))))


def load_constellation_bounds():
    """Roman (1987): the 1875 boundaries as RA strips, southern edge first
    by declination — the first strip a position falls in is its constellation."""
    rows = vizier("VI/42/data", ["RA_low", "RA_up", "DE_low", "const"], "vizier_cst_bounds.tsv")
    return [(float(r["RA_low"]), float(r["RA_up"]), float(r["DE_low"]), r["const"]) for r in rows]


def constellation(ra, dec, bounds):
    ra1875, dec1875 = precess_from_j2000(ra, dec, B1875_JD)
    ra_h = ra1875 / 15.0
    for lo, hi, de_lo, con in bounds:
        if dec1875 >= de_lo and lo <= ra_h < hi:
            return con
    return "Oct"  # below the last strip: the south pole


def line_signature(o):
    """(Hα, OIII, SII, broadband) for an object; see SIG_*."""
    if "lines" in o:
        return o["lines"]
    h = o.get("hints", set())
    kind = o["kind"]
    if kind == "Galaxy":
        return SIG_SPIRAL if "spiral" in h and o["size_arcmin"] >= SPIRAL_MIN_ARCMIN else SIG_BROADBAND
    if kind == "PlanetaryNebula":
        return SIG_PLANETARY
    if kind == "SupernovaRemnant":
        return SIG_SNR
    if kind == "OpenCluster":
        return SIG_CLUSTER_NEBULA if h & {"cln", "emn", "bub"} else SIG_BROADBAND
    if kind != "Nebula":
        return SIG_BROADBAND
    if "bub" in h:
        return SIG_BUBBLE
    if "emn" in h and "rfn" in h:
        return SIG_MIXED
    if "emn" in h:
        return SIG_EMISSION
    if "rfn" in h:
        return SIG_BROADBAND
    return SIG_NEBULA


def pack_lines(sig):
    return sum(v << (2 * i) for i, v in enumerate(sig))


# ── main ─────────────────────────────────────────────────────────────────────

def main():
    global REFRESH
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--refresh", action="store_true",
                    help="refetch the VizieR / SIMBAD / Sesame sources")
    REFRESH = ap.parse_args().refresh

    cat = Catalog()
    extra = read_extra()
    forced = forced_designations(extra)

    load_openngc(cat)
    print("Loading the extra catalogs...")
    load_harris_gcs(cat)
    load_open_clusters(cat)
    load_abell_pn(cat)   # before Sharpless: the Medusa is "Abell 21", not "Sh2-274"
    load_sharpless(cat)
    load_vdb(cat)
    load_barnard(cat)
    load_ldn(cat, forced)
    load_lbn(cat, forced)
    load_hickson(cat)
    load_arp(cat)
    load_abell_clusters(cat, forced)
    name_abell_clusters(cat)
    print("Applying dso_extra.csv...")
    apply_extra(cat, extra)
    cat.apply_simbad_names()

    objects = cat.objects

    # Searchable short forms ("Cr 399", "Barnard 33") of every designation.
    for o in objects:
        alts = []
        for d in [o["name"], *o["ids"]]:
            m = re.match(r"^([A-Za-z]+) (\d+)$", d)
            if m and m[1] in CATALOG_ABBREV:
                alts.append(f"{CATALOG_ABBREV[m[1]]} {m[2]}")
        add_unique(o["ids"], [a for a in alts if a != o["name"]])

    # Sort: Messier first (by number), then by the magnitude the sky culls
    # by — the catalog magnitude, else a stand-in from the size (mirrors
    # `Dso::vis_mag` in dso_catalog.rs). The order is the sky's label
    # priority: of two colliding labels, the earlier object keeps its own.
    def vis_mag(o):
        if o["mag"] < 90.0:
            return o["mag"]
        if o["size_arcmin"] > 0:
            return min(16.0, max(4.0, 15.0 - 2.5 * math.log10(o["size_arcmin"])))
        return 16.0

    def sort_key(o):
        if o["name"].startswith("M") and o["name"][1:].isdigit():
            return (0, int(o["name"][1:]))
        return (1, vis_mag(o))

    objects.sort(key=sort_key)

    # Names must stay unique once squashed to [a-z0-9] — that is the search
    # key and the DSO tile file name (prefetch_dso_tiles.py `slug`).
    seen = {}
    unique = []
    for o in objects:
        key = re.sub(r"[^a-z0-9]+", "", o["name"].lower())
        if key in seen:
            print(f"  ! {o['name']!r} collides with {seen[key]!r}; dropped")
            continue
        seen[key] = o["name"]
        unique.append(o)
    objects = unique

    # Attach French aliases from Wikidata (keyed by our make_name() output).
    fr_map = fetch_wikidata_french_labels(
        [o["name"] for o in objects if re.match(r"^(M\d|NGC |IC )", o["name"])]
    )
    fr_hits = 0
    for o in objects:
        fr_aliases = fr_map.get(o["name"], [])
        # Drop FR aliases that duplicate an existing English common name.
        existing = {n.lower() for n in o["common_names"]}
        add_unique(o["fr_names"], [a for a in fr_aliases if a.lower() not in existing])
        if o["fr_names"]:
            fr_hits += 1

    print()
    for source, n in cat.stats.items():
        print(f"  {source:<36} {n:>6,}")
    print(f"Objects kept:    {len(objects):,}")
    print(f"Objects with EN names: {sum(1 for o in objects if o['common_names']):,}")
    print(f"Objects with FR names: {fr_hits:,}")

    # Constellation, checked against OpenNGC's where it has one: a mismatch
    # is an object on a boundary, placed by slightly different coordinates.
    bounds = load_constellation_bounds()
    mismatches = []
    for o in objects:
        o["con"] = constellation(o["ra_deg"], o["dec_deg"], bounds)
        ngc = {"Se1": "Ser", "Se2": "Ser"}.get(o.get("ngc_const", ""), o.get("ngc_const", ""))
        if ngc and ngc.lower() != o["con"].lower():
            mismatches.append(f"{o['name']} ({ngc} → {o['con']})")
    print(f"Constellations differing from OpenNGC: {len(mismatches)}"
          + (f" — {', '.join(mismatches[:30])}" if mismatches else ""))

    sigs = {}
    for o in objects:
        sig = line_signature(o)
        sigs[sig] = sigs.get(sig, 0) + 1
        o["lines_byte"] = pack_lines(sig)
    print("Emission signatures (Ha, OIII, SII, RGB):")
    for sig, n in sorted(sigs.items(), key=lambda kv: -kv[1]):
        print(f"  {sig}  {n:>6,}")

    # --- Emit binary ---
    # Format (little-endian):
    #   [u32] n_objects
    #   Objects: ra(f32) dec(f32) mag(f32) size_arcmin(f32) size_minor(f32) pa_deg(f32)
    #            kind(u8) lines(u8) con(3 × ASCII) name_len(u8) name(utf8)
    #            aliases_count(u8) [ alias_len(u8) alias(utf8) ] × aliases_count
    #            fr_names_count(u8) [ fr_len(u8) fr(utf8) ] × fr_names_count
    #            ids_count(u8) [ id_len(u8) id(utf8) ] × ids_count
    # kind codes: 0=Galaxy 1=OpenCluster 2=GlobularCluster 3=Nebula
    #             4=PlanetaryNebula 5=SupernovaRemnant 6=GalaxyCluster
    #             7=DarkNebula
    # lines: Hα | OIII << 2 | SII << 4 | broadband << 6, each 0–3
    # con: IAU abbreviation ("And", "CVn")
    KIND_CODE = {k: i for i, k in enumerate(KINDS)}

    def name_list(names):
        # u8 length fields cap at 255. Skip any entry that won't fit and cap
        # the list at 255 entries so the wire format stays uncorrupted.
        enc_list = []
        for n in names:
            enc = n.encode("utf-8")
            if 0 < len(enc) <= 255:
                enc_list.append(enc)
            if len(enc_list) == 255:
                break
        out = bytes([len(enc_list)])
        for enc in enc_list:
            out += bytes([len(enc)]) + enc
        return out

    buf = bytearray()
    buf += struct.pack('<I', len(objects))
    for o in objects:
        name_enc = o['name'].encode('utf-8')
        buf += struct.pack('<ffffff',
                           o['ra_deg'], o['dec_deg'], o['mag'],
                           o['size_arcmin'], o['size_minor_arcmin'], o['pa_deg'])
        buf += bytes([KIND_CODE[o['kind']], o['lines_byte']]) + o['con'].encode('ascii')
        buf += bytes([len(name_enc)]) + name_enc
        buf += name_list(o['common_names'])
        buf += name_list(o['fr_names'])
        buf += name_list([i for i in o['ids'] if i != o['name']])

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, 'wb') as f:
        f.write(buf)
    print(f"Wrote {OUT} ({len(buf):,} bytes)")


if __name__ == "__main__":
    main()
