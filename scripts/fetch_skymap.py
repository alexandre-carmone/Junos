#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow>=10"]
# ///
"""Mirror the CDS `P/DSS2/color` HiPS survey so the planetarium has a full-sky
image without internet, plus the Mellinger Milky Way panorama.

A HiPS (Hierarchical Progressive Survey) is the whole sky cut into HEALPix
cells, one 512 × 512 JPEG per cell, at every order from 0 (12 tiles of ~58°)
to 9 (3.1 M tiles of ~0.11°, 0.8″/px for DSS2). The tree is mirrored as-is —
`properties` plus `Norder{k}/Dir{(n // 10000) * 10000}/Npix{n}.jpg` — so any
HiPS client, `rsync` or `wget -r` understands the result, and `junos-server`
serves it at `/api/skymap/*`. The browser then streams only the tiles under
its view at the order matching its zoom, never the whole mirror.

Usage:
    uv run scripts/fetch_skymap.py                    # orders 0..7, ~262k tiles, 13–20 GB
    uv run scripts/fetch_skymap.py --max-order 3      # smoke test: 1,020 tiles, ~25 MB
    uv run scripts/fetch_skymap.py --status           # per-order coverage, no downloads
    uv run scripts/fetch_skymap.py --allsky           # Milky Way panorama (allsky.jpg + allsky_small.jpg)
    uv run scripts/fetch_skymap.py --min-order 8 --max-order 9 --cone 83.82 -5.39 3   # Orion at full res
    uv run scripts/fetch_skymap.py --prune-above 7    # delete Norder8+ and rewrite info.json
    uv run scripts/fetch_skymap.py --info-only        # rebuild info.json from disk
    uv run scripts/fetch_skymap.py --selftest         # HEALPix round-trips + shared test vectors

Sizes (from CDS's `hips_estsize`): order 6 ≈ 3.3 GB, 7 ≈ 13–20 GB, 8 ≈ 53 GB,
9 ≈ 210 GB. A full-sky order 8 or 9 is discouraged — use `--cone` to get the
fine orders only around the targets you image.

Resumable: an existing tile (> MIN_TILE_BYTES) is skipped, every write goes
through `.part` + `os.replace`, and Ctrl-C still writes `info.json`. Tiles are
static files on CDS's Apache servers (strong ETags, 2015 `Last-Modified`), so
no per-request pause is needed; `--pace 0.2 --workers 4` is the gentle setting.
A 404 is final (the survey has no such tile); 429 / 5xx / timeouts back off
exponentially and move to the next mirror.

Output layout (under --out, default `.cache/skymap/`):
    properties                      CDS's survey description, copied verbatim
    Norder{k}/Dir{d}/Npix{n}.jpg    the tiles, exact HiPS layout
    info.json                       what the mirror holds — served at /api/skymap/info
    allsky.jpg                      ALLSKY_W × ALLSKY_W/2 Milky Way panorama (+ allsky_small.jpg)

The HEALPix helpers below (nested scheme, healpix_cxx formulas) are also
implemented in Rust (`junos-web/src/healpix.rs`) and WGSL
(`junos-web/src/components/sky/shaders/skymap.wgsl`); `--selftest` prints the
vectors those two assert, keep the three in step.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import os
import shutil
import struct
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
ROOT_DIR = os.path.dirname(SCRIPT_DIR)
# Honour SKYMAP_DIR so a single env var configures both this script and
# junos-server (which reads the same var); --out still overrides it.
DEFAULT_OUT = os.environ.get("SKYMAP_DIR") or os.path.join(ROOT_DIR, ".cache", "skymap")

# ── survey ───────────────────────────────────────────────────────────────────
SURVEY_ID = "CDS/P/DSS2/color"
SURVEY_PATH = "DSS/DSSColor"
# Same HiPS collection on three hosts; a tile a throttled mirror refuses is
# retried against the next one.
MIRRORS = [
    "https://alasky.cds.unistra.fr",
    "https://alaskybis.cds.unistra.fr",
    "https://alasky.u-strasbg.fr",
]
TILE_WIDTH = 512
DEFAULT_MAX_ORDER = 7
SURVEY_MAX_ORDER = 9            # overridden by `properties` (hips_order)
# Every DSS2 colour tile is a real JPEG far above this; the floor only catches
# an empty or truncated file.
MIN_TILE_BYTES = 1024
USER_AGENT = "junos-web/fetch_skymap"
PROPERTIES_FILE = "properties"
INFO_FILE = "info.json"
# An order counts as complete (and `max_order` may reach it) from this
# fraction of its 12·4^k tiles.
COMPLETE_FRACTION = 0.99
ATTRIBUTION = "Digitized Sky Survey - STScI/NASA, Colored & Healpixed by CDS"
ATTRIBUTION_URL = "http://archive.stsci.edu/dss/copyright.html"
SURVEY_TITLE = "DSS colored"

# ── all-sky Milky Way panorama ───────────────────────────────────────────────
# Mellinger's optical panorama is the usual planetarium backdrop; DSS2 colour
# ("CDS/P/DSS2/color") works too but is noisier and shows plate seams at
# this scale.
HIPS2FITS_MIRRORS = [
    "https://alasky.u-strasbg.fr/hips-image-services/hips2fits",
    "https://alaskybis.cds.unistra.fr/hips-image-services/hips2fits",
    "https://alasky.cds.unistra.fr/hips-image-services/hips2fits",
]
ALLSKY_HIPS = "CDS/P/Mellinger/color"
ALLSKY_FILE = "allsky.jpg"
ALLSKY_SMALL_FILE = "allsky_small.jpg"
ALLSKY_W = 4096          # height is always ALLSKY_W / 2 (plate carrée)
ALLSKY_SMALL_W = 2048    # phones / adapters with a 2048 px texture limit
# Orientation of the delivered image — the sky shader relies on this:
#   plate carrée centred on RA 0h / Dec 0°, north up, east LEFT (the sky as
#   seen from inside), so RA increases leftward from the centre column:
#   u = 0.5 - ra/360 (wrapping), v = (90 - dec)/180.
ALLSKY_CENTER_RA = 0.0


# ── geometry ─────────────────────────────────────────────────────────────────

def ntiles(order: int) -> int:
    return 12 << (2 * order)


def tile_rel(order: int, npix: int) -> str:
    return f"Norder{order}/Dir{(npix // 10000) * 10000}/Npix{npix}.jpg"


def tile_url(mirror: str, order: int, npix: int) -> str:
    return f"{mirror}/{SURVEY_PATH}/{tile_rel(order, npix)}"


# ── HEALPix, nested scheme (healpix_cxx formulas) ────────────────────────────
# Only `--cone` needs these. phi = RA, theta = 90° − Dec, nside = 2^order.

def _spread_bits(v: int) -> int:
    """Bit i of v → bit 2i (Morton interleave, 16-bit input)."""
    v &= 0xFFFF
    v = (v | (v << 8)) & 0x00FF00FF
    v = (v | (v << 4)) & 0x0F0F0F0F
    v = (v | (v << 2)) & 0x33333333
    v = (v | (v << 1)) & 0x55555555
    return v


def _compress_bits(v: int) -> int:
    """Inverse of _spread_bits: keeps the even bits, packs them down."""
    v &= 0x55555555
    v = (v | (v >> 1)) & 0x33333333
    v = (v | (v >> 2)) & 0x0F0F0F0F
    v = (v | (v >> 4)) & 0x00FF00FF
    v = (v | (v >> 8)) & 0x0000FFFF
    return v


def ang2pix_nest(order: int, ra_deg: float, dec_deg: float) -> int:
    nside = 1 << order
    z = math.sin(math.radians(dec_deg))
    za = abs(z)
    tt = (ra_deg % 360.0) / 90.0                       # in [0, 4)
    if za <= 2.0 / 3.0:                                # equatorial belt
        temp1 = nside * (0.5 + tt)
        temp2 = nside * z * 0.75
        jp = int(math.floor(temp1 - temp2))
        jm = int(math.floor(temp1 + temp2))
        ifp = jp >> order
        ifm = jm >> order
        if ifp == ifm:
            face = (ifp & 3) + 4
        elif ifp < ifm:
            face = ifp & 3
        else:
            face = (ifm & 3) + 8
        ix = jm & (nside - 1)
        iy = nside - (jp & (nside - 1)) - 1
    else:                                              # polar caps
        ntt = min(int(tt), 3)
        tp = tt - ntt
        tmp = nside * math.sqrt(3.0 * (1.0 - za))
        jp = min(int(tp * tmp), nside - 1)
        jm = min(int((1.0 - tp) * tmp), nside - 1)
        if z >= 0:
            face = ntt
            ix = nside - jm - 1
            iy = nside - jp - 1
        else:
            face = ntt + 8
            ix = jp
            iy = jm
    return (face << (2 * order)) | _spread_bits(ix) | (_spread_bits(iy) << 1)


_JRLL = [2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4]
_JPLL = [1, 3, 5, 7, 0, 2, 4, 6, 1, 3, 5, 7]


def pix2ang_nest(order: int, ipix: int) -> tuple[float, float]:
    """Cell centre as (RA, Dec) in degrees."""
    nside = 1 << order
    face = ipix >> (2 * order)
    p = ipix & (nside * nside - 1)
    ix = _compress_bits(p)
    iy = _compress_bits(p >> 1)
    jr = _JRLL[face] * nside - ix - iy - 1
    if jr < nside:
        nr = jr
        z = 1.0 - nr * nr / (3.0 * nside * nside)
        kshift = 0
    elif jr > 3 * nside:
        nr = 4 * nside - jr
        z = nr * nr / (3.0 * nside * nside) - 1.0
        kshift = 0
    else:
        nr = nside
        z = (2 * nside - jr) * 2.0 / (3.0 * nside)
        kshift = (jr - nside) & 1
    jp = (_JPLL[face] * nr + ix - iy + 1 + kshift) // 2
    if jp > 4 * nr:
        jp -= 4 * nr
    if jp < 1:
        jp += 4 * nr
    ra = (jp - (kshift + 1) * 0.5) * (90.0 / nr)
    dec = math.degrees(math.asin(max(-1.0, min(1.0, z))))
    return ra % 360.0, dec


def _angsep(ra1: float, dec1: float, ra2: float, dec2: float) -> float:
    """Great-circle separation in degrees (haversine)."""
    r1, d1, r2, d2 = map(math.radians, (ra1, dec1, ra2, dec2))
    a = math.sin((d2 - d1) / 2) ** 2 + math.cos(d1) * math.cos(d2) * math.sin((r2 - r1) / 2) ** 2
    return math.degrees(2 * math.asin(min(1.0, math.sqrt(a))))


# Side of an order-0 cell, degrees: sqrt(4π/12) rad. A cell's farthest vertex
# is within one side of its centre, so one side is a safe pruning margin.
ORDER0_SIDE_DEG = math.degrees(math.sqrt(4 * math.pi / 12))


def cone_pixels(order: int, ra: float, dec: float, radius_deg: float) -> set[int]:
    """Cells of `order` whose bounding circle meets the cone — recursive
    descent from the 12 base cells, so order 9 costs milliseconds, not a scan
    of 3 M cells."""
    cells = list(range(12))
    for o in range(0, order + 1):
        margin = ORDER0_SIDE_DEG / (1 << o)
        keep = [c for c in cells if _angsep(*pix2ang_nest(o, c), ra, dec) <= radius_deg + margin]
        if o == order:
            return set(keep)
        cells = [(c << 2) | q for c in keep for q in range(4)]
    return set(cells)


def selftest() -> int:
    """Round-trips and the vectors the Rust / WGSL ports assert."""
    bad = 0
    for order in range(0, 5):
        for p in range(ntiles(order)):
            ra, dec = pix2ang_nest(order, p)
            q = ang2pix_nest(order, ra, dec)
            if q != p:
                bad += 1
                if bad <= 5:
                    print(f"  round-trip failed: order {order} pix {p} → ({ra:.6f}, {dec:.6f}) → {q}")
    # Base-cell faces: north polar cap 0–3 above +41.8°, south 8–11 below.
    for ra in (10.0, 100.0, 190.0, 280.0):
        if not 0 <= ang2pix_nest(0, ra, 60.0) <= 3:
            bad += 1
            print(f"  face check failed at ({ra}, 60)")
        if not 8 <= ang2pix_nest(0, ra, -60.0) <= 11:
            bad += 1
            print(f"  face check failed at ({ra}, -60)")
    if ang2pix_nest(0, 0.0, 0.0) != 4:
        bad += 1
        print("  (0, 0) should fall in base cell 4")
    # Cone coverage contains the cells of sampled points.
    for order, ra, dec, r in ((3, 83.82, -5.39, 3.0), (7, 10.68, 41.27, 1.0), (9, 0.5, 89.0, 2.0)):
        cells = cone_pixels(order, ra, dec, r)
        for k in range(50):
            a = k * 2 * math.pi / 50
            pr = ra + r * 0.9 * math.cos(a) / max(1e-6, math.cos(math.radians(dec)))
            pd = max(-89.99, min(89.99, dec + r * 0.9 * math.sin(a)))
            if _angsep(ra, dec, pr, pd) > r:
                continue  # the flat offset overshoots near the pole: not a cone point
            if ang2pix_nest(order, pr, pd) not in cells:
                bad += 1
                print(f"  cone miss: order {order} point ({pr:.4f}, {pd:.4f})")
                break
    print("Test vectors (RA, Dec degrees) — assert these in junos-web/src/healpix.rs:")
    for order, p in ((3, 0), (3, 767), (7, 196607), (0, 4), (1, 47)):
        ra, dec = pix2ang_nest(order, p)
        print(f"  pix2ang_nest({order}, {p}) = ({ra:.6f}, {dec:.6f})")
    for order, ra, dec in ((3, 83.82, -5.39), (7, 10.68, 41.27), (5, 0.0, -89.5)):
        print(f"  ang2pix_nest({order}, {ra}, {dec}) = {ang2pix_nest(order, ra, dec)}")
    print("selftest: " + ("OK" if bad == 0 else f"{bad} failure(s)"))
    return 0 if bad == 0 else 1


# ── network ──────────────────────────────────────────────────────────────────

class Final(Exception):
    """A failure that no retry or mirror will fix (404)."""


def fetch_one(url: str, timeout: float, retries: int, delay: float,
              min_bytes: int = MIN_TILE_BYTES) -> bytes | None:
    """GET one URL with bounded retries. None once the budget is spent;
    raises Final on a 404."""
    for attempt in range(retries + 1):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                data = resp.read()
            if len(data) < min_bytes:
                raise ValueError(f"suspiciously small response ({len(data)} bytes)")
            return data
        except urllib.error.HTTPError as e:
            if e.code == 404:
                raise Final(f"404 {url}") from None
            err: Exception = e
        except Exception as e:  # noqa: BLE001 — timeouts, resets, short reads: retry
            err = e
        if attempt == retries:
            print(f"    give up on {url}: {err}", file=sys.stderr)
            return None
        time.sleep(delay * (2 ** attempt))
    return None


_tls = threading.local()


def fetch_tile(order: int, npix: int, mirrors: list[str],
               timeout: float, retries: int, delay: float) -> bytes | None:
    """Try the mirrors starting from this thread's preferred one, which
    rotates on failure so a dead host isn't retried for every tile."""
    start = getattr(_tls, "mirror", 0)
    for i in range(len(mirrors)):
        m = mirrors[(start + i) % len(mirrors)]
        try:
            data = fetch_one(tile_url(m, order, npix), timeout, retries, delay)
        except Final:
            return None
        if data is not None:
            _tls.mirror = (start + i) % len(mirrors)
            return data
    _tls.mirror = (start + 1) % len(mirrors)
    return None


def parse_properties(text: str) -> dict[str, str]:
    props: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        props[k.strip()] = v.strip()
    return props


def fetch_properties(out: str, mirrors: list[str], timeout: float, retries: int, delay: float) -> dict[str, str]:
    """Download CDS's `properties` once (kept verbatim for HiPS clients); fall
    back to the copy on disk, then to the constants above."""
    path = os.path.join(out, PROPERTIES_FILE)
    for m in mirrors:
        try:
            data = fetch_one(f"{m}/{SURVEY_PATH}/{PROPERTIES_FILE}", timeout, retries, delay, min_bytes=16)
        except Final:
            data = None
        if data is not None:
            write_atomic(path, data)
            break
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return parse_properties(f.read())
    except OSError:
        return {}


# ── disk ─────────────────────────────────────────────────────────────────────

def write_atomic(path: str, data: bytes) -> None:
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    tmp = f"{path}.{os.getpid()}.part"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)


def have(path: str) -> bool:
    try:
        return os.path.getsize(path) > MIN_TILE_BYTES
    except OSError:
        return False


def scan(out: str) -> dict[int, tuple[int, int]]:
    """{order: (tile count, bytes)} from the Norder*/Dir* tree."""
    orders: dict[int, tuple[int, int]] = {}
    try:
        top = list(os.scandir(out))
    except OSError:
        return orders
    for e in top:
        if not (e.is_dir() and e.name.startswith("Norder") and e.name[6:].isdigit()):
            continue
        order = int(e.name[6:])
        n = size = 0
        for d in os.scandir(e.path):
            if not (d.is_dir() and d.name.startswith("Dir")):
                continue
            for t in os.scandir(d.path):
                if t.name.startswith("Npix") and t.name.endswith(".jpg"):
                    try:
                        s = t.stat().st_size
                    except OSError:
                        continue
                    if s > MIN_TILE_BYTES:
                        n += 1
                        size += s
        orders[order] = (n, size)
    return orders


def compute_max_order(orders: dict[int, tuple[int, int]]) -> int | None:
    """Highest k such that every order 0..k is ≥ COMPLETE_FRACTION full —
    the zoom the client may rely on everywhere."""
    best = None
    k = 0
    while k in orders and orders[k][0] >= COMPLETE_FRACTION * ntiles(k):
        best = k
        k += 1
    return best


def build_info(out: str, props: dict[str, str]) -> dict:
    orders = scan(out)
    return {
        "survey": props.get("creator_did", "ivo://" + SURVEY_ID).removeprefix("ivo://"),
        "title": props.get("obs_title", SURVEY_TITLE),
        "frame": props.get("hips_frame", "equatorial"),
        "tile_width": int(props.get("hips_tile_width", TILE_WIDTH)),
        "tile_format": "jpg",
        "survey_max_order": int(props.get("hips_order", SURVEY_MAX_ORDER)),
        "max_order": compute_max_order(orders),
        "orders": {str(k): orders[k][0] for k in sorted(orders)},
        "bytes": sum(b for _, b in orders.values()),
        "allsky": os.path.isfile(os.path.join(out, ALLSKY_SMALL_FILE)),
        "attribution": props.get("obs_copyright", ATTRIBUTION).strip('"'),
        "attribution_url": props.get("obs_copyright_url", ATTRIBUTION_URL),
        "updated": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }


def write_info(out: str, props: dict[str, str]) -> dict:
    info = build_info(out, props)
    write_atomic(os.path.join(out, INFO_FILE), (json.dumps(info, indent=1) + "\n").encode())
    return info


def read_properties(out: str) -> dict[str, str]:
    try:
        with open(os.path.join(out, PROPERTIES_FILE), encoding="utf-8", errors="replace") as f:
            return parse_properties(f.read())
    except OSError:
        return {}


def prune_above(out: str, n: int) -> None:
    for k in sorted(scan(out)):
        if k > n:
            p = os.path.join(out, f"Norder{k}")
            print(f"removing {p}")
            shutil.rmtree(p, ignore_errors=True)


def jpeg_dims(path: str) -> tuple[int, int] | None:
    """(width, height) read from a JPEG's SOF marker, or None if unreadable.
    Hand-rolled so `--status` doesn't need Pillow."""
    try:
        with open(path, "rb") as f:
            d = f.read(256 * 1024)
    except OSError:
        return None
    if not d.startswith(b"\xff\xd8"):
        return None
    i = 2
    while i + 9 < len(d):
        if d[i] != 0xFF:
            i += 1
            continue
        marker = d[i + 1]
        if marker in (0xC0, 0xC1, 0xC2):
            h, w = struct.unpack(">HH", d[i + 5:i + 9])
            return w, h
        if marker == 0xD8 or 0xD0 <= marker <= 0xD7:
            i += 2
            continue
        seg = struct.unpack(">H", d[i + 2:i + 4])[0]
        i += 2 + seg
    return None


def human_bytes(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:,.1f} {unit}" if unit != "B" else f"{n:,.0f} B"
        n /= 1024
    return f"{n:,.1f} TB"


# ── status ───────────────────────────────────────────────────────────────────

def status(out: str) -> None:
    props = read_properties(out)
    orders = scan(out)
    print(f"sky map: {out}")
    print(f"survey:  {props.get('creator_did', SURVEY_ID)}  (hips_order {props.get('hips_order', '?')}, "
          f"tile {props.get('hips_tile_width', '?')} px)")
    if not orders:
        print("  no tiles — run `uv run scripts/fetch_skymap.py --max-order 3` for a start")
    total_n = total_b = 0
    for k in sorted(orders):
        n, b = orders[k]
        want = ntiles(k)
        pct = 100.0 * n / want
        print(f"  order {k}: {n:>9,} / {want:>9,} tiles ({pct:5.1f} %)  {human_bytes(b):>10}  missing {want - n:,}")
        total_n += n
        total_b += b
    print(f"  total:   {total_n:,} tiles, {human_bytes(total_b)}")
    mo = compute_max_order(orders)
    print(f"  max_order (every order complete up to): {mo if mo is not None else 'none'}")
    for name in (ALLSKY_FILE, ALLSKY_SMALL_FILE):
        p = os.path.join(out, name)
        dims = jpeg_dims(p)
        print(f"  {name}: " + (f"{dims[0]}x{dims[1]}" if dims else "missing"))
    info_path = os.path.join(out, INFO_FILE)
    try:
        with open(info_path, encoding="utf-8") as f:
            info = json.load(f)
        stale = info.get("orders") != {str(k): orders[k][0] for k in sorted(orders)}
        print(f"  info.json: {'STALE — run --info-only' if stale else 'in sync'}")
    except (OSError, ValueError):
        print("  info.json: missing — run --info-only")


# ── all-sky panorama ─────────────────────────────────────────────────────────

def allsky_url(base: str, width: int) -> str:
    q = urllib.parse.urlencode({
        "hips": ALLSKY_HIPS,
        "width": width,
        "height": width // 2,
        "fov": "360",
        "projection": "CAR",
        "coordsys": "icrs",
        "ra": f"{ALLSKY_CENTER_RA:.1f}",
        "dec": "0.0",
        "format": "jpg",
    })
    return f"{base}?{q}"


def fetch_allsky(out_dir: str, width: int, timeout: float, retries: int, delay: float) -> bool:
    """Download `allsky.jpg` at `width` px and derive `allsky_small.jpg` from it."""
    from PIL import Image
    data = None
    for i, base in enumerate(HIPS2FITS_MIRRORS):
        try:
            data = fetch_one(allsky_url(base, width), timeout, retries, delay)
        except Final:
            data = None
        if data is not None:
            break
        if i + 1 < len(HIPS2FITS_MIRRORS):
            print(f"    mirror {base} failed, trying next", file=sys.stderr)
    if data is None:
        return False
    os.makedirs(out_dir, exist_ok=True)
    big = os.path.join(out_dir, ALLSKY_FILE)
    write_atomic(big, data)
    with Image.open(big) as im:
        w, h = im.size
        print(f"allsky: {w}x{h} → {big}")
        small = im.convert("RGB").resize((ALLSKY_SMALL_W, ALLSKY_SMALL_W // 2), Image.LANCZOS)
        small_path = os.path.join(out_dir, ALLSKY_SMALL_FILE)
        small.save(small_path + ".part", "JPEG", quality=88, optimize=True)
        os.replace(small_path + ".part", small_path)
        print(f"allsky: {ALLSKY_SMALL_W}x{ALLSKY_SMALL_W // 2} → {small_path}")
    return True


# ── work ─────────────────────────────────────────────────────────────────────

def plan(args: argparse.Namespace) -> list[tuple[int, int]]:
    """(order, npix) pairs to fetch: orders min..max, cone-filtered, minus
    what is on disk (unless --force), truncated by --limit."""
    todo: list[tuple[int, int]] = []
    for order in range(args.min_order, args.max_order + 1):
        if args.cone:
            ra, dec, r = args.cone
            pixels: list[int] = sorted(cone_pixels(order, ra, dec, r))
        else:
            pixels = list(range(ntiles(order)))
        for n in pixels:
            if args.force or not have(os.path.join(args.out, tile_rel(order, n))):
                todo.append((order, n))
            if args.limit and len(todo) >= args.limit:
                return todo
    return todo


def run(todo: list[tuple[int, int]], args: argparse.Namespace, props: dict[str, str]) -> int:
    lock = threading.Lock()
    done = failed = written = 0
    t0 = time.monotonic()
    last_print = last_info = t0
    failures: list[tuple[int, int]] = []
    stop = threading.Event()

    def progress(force: bool = False) -> None:
        nonlocal last_print
        now = time.monotonic()
        if not force and now - last_print < 10 and (done + failed) % 500:
            return
        last_print = now
        el = max(1e-6, now - t0)
        rate = (done + failed) / el
        left = len(todo) - done - failed
        eta = left / rate if rate > 0 else float("inf")
        print(f"  {done + failed:,}/{len(todo):,} tiles  {human_bytes(written)}  "
              f"{rate:.1f}/s  ETA {eta / 60:.0f} min  ({failed} failed)", flush=True)

    def work(item: tuple[int, int]) -> None:
        nonlocal done, failed, written, last_info
        if stop.is_set():
            return
        order, n = item
        data = fetch_tile(order, n, args.mirror, args.timeout, args.retries, args.delay)
        if args.pace:
            time.sleep(args.pace)
        with lock:
            if data is None:
                failed += 1
                failures.append(item)
            else:
                write_atomic(os.path.join(args.out, tile_rel(order, n)), data)
                done += 1
                written += len(data)
            progress()
            if time.monotonic() - last_info > 60:
                last_info = time.monotonic()
                write_info(args.out, props)

    print(f"fetching {len(todo):,} tiles into {args.out} with {args.workers} workers")
    pool = ThreadPoolExecutor(max_workers=args.workers)
    try:
        list(pool.map(work, todo))
    except KeyboardInterrupt:
        print("\ninterrupted — finishing in-flight tiles, re-run to resume", file=sys.stderr)
        stop.set()
        pool.shutdown(wait=True, cancel_futures=True)
    else:
        pool.shutdown(wait=True)
    progress(force=True)
    info = write_info(args.out, props)
    print(f"info.json: max_order {info['max_order']}, {human_bytes(info['bytes'])} on disk")
    if failures:
        print(f"{len(failures)} tile(s) failed, first: " +
              ", ".join(f"{o}/{n}" for o, n in failures[:10]), file=sys.stderr)
        return 1
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", default=DEFAULT_OUT, help=f"mirror directory (default {DEFAULT_OUT})")
    ap.add_argument("--max-order", type=int, default=DEFAULT_MAX_ORDER)
    ap.add_argument("--min-order", type=int, default=0)
    ap.add_argument("--cone", nargs=3, type=float, metavar=("RA", "DEC", "RADIUS_DEG"),
                    help="only the tiles within this cone (ICRS degrees)")
    ap.add_argument("--workers", type=int, default=8)
    ap.add_argument("--pace", type=float, default=0.0, help="seconds each worker sleeps between tiles")
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--retries", type=int, default=3)
    ap.add_argument("--delay", type=float, default=1.0, help="backoff base, doubled per retry")
    ap.add_argument("--limit", type=int, default=0, help="stop after this many tiles (smoke test)")
    ap.add_argument("--force", action="store_true", help="refetch tiles already on disk")
    ap.add_argument("--mirror", action="append", help="HiPS mirror base URL (repeatable; replaces the built-in list)")
    ap.add_argument("--status", action="store_true")
    ap.add_argument("--info-only", action="store_true", help="rebuild info.json from disk")
    ap.add_argument("--allsky", action="store_true", help="fetch the Milky Way panorama")
    ap.add_argument("--allsky-px", type=int, default=ALLSKY_W)
    ap.add_argument("--prune-above", type=int, metavar="N", help="delete orders above N")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()
    if not args.mirror:
        args.mirror = MIRRORS

    if args.selftest:
        return selftest()
    if args.status:
        status(args.out)
        return 0
    if args.prune_above is not None:
        prune_above(args.out, args.prune_above)
        write_info(args.out, read_properties(args.out))
        return 0
    if args.info_only:
        info = write_info(args.out, read_properties(args.out))
        print(f"info.json: max_order {info['max_order']}, orders {info['orders']}")
        return 0
    if args.allsky:
        os.makedirs(args.out, exist_ok=True)
        ok = fetch_allsky(args.out, args.allsky_px, args.timeout, args.retries, args.delay)
        write_info(args.out, read_properties(args.out))
        return 0 if ok else 1

    if not 0 <= args.min_order <= args.max_order <= 12:
        ap.error("need 0 <= --min-order <= --max-order <= 12")
    os.makedirs(args.out, exist_ok=True)
    props = fetch_properties(args.out, args.mirror, args.timeout, args.retries, args.delay)
    survey_max = int(props.get("hips_order", SURVEY_MAX_ORDER))
    if args.max_order > survey_max:
        print(f"survey stops at order {survey_max}; clamping --max-order", file=sys.stderr)
        args.max_order = survey_max
    if args.max_order >= 8 and not args.cone:
        print(f"note: a full-sky order {args.max_order} is {'~53 GB' if args.max_order == 8 else '~210 GB'}; "
              "consider --cone for the fine orders", file=sys.stderr)
    todo = plan(args)
    if not todo:
        print("nothing to fetch — every planned tile is on disk")
        info = write_info(args.out, props)
        print(f"info.json: max_order {info['max_order']}")
        return 0
    return run(todo, args, props)


if __name__ == "__main__":
    sys.exit(main())
