#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow>=10"]
# ///
"""Pre-download one hips2fits survey cutout per DSO so the Framing Assistant
works without internet.

Reads `junos-web/public/dso.bin` (the same catalog the WASM client loads) and
fetches a square TAN cutout centred on every object, sized from its apparent
diameter. Output goes to a server-side cache directory — **not** into
`junos-web/public/`, so these hundreds of megabytes stay out of git.
`junos-server` serves the directory at `/api/dso_tiles/…`; the Framing
Assistant uses a tile when it covers the requested mosaic and otherwise falls
back to the live `/api/skysurvey` proxy.

Usage:
    uv run scripts/prefetch_dso_tiles.py                  # every catalog object (+ thumbnails)
    uv run scripts/prefetch_dso_tiles.py --status         # coverage report, no downloads
    uv run scripts/prefetch_dso_tiles.py --limit 50       # smoke test
    uv run scripts/prefetch_dso_tiles.py --workers 8
    uv run scripts/prefetch_dso_tiles.py --only M31,M42,"NGC 7000"
    uv run scripts/prefetch_dso_tiles.py --thumbs         # (re)build thumbs/ from the tiles on disk
    uv run scripts/prefetch_dso_tiles.py --allsky         # fetch the Milky Way panorama (allsky.jpg)

Besides the full-size tiles the Framing Assistant composites, the planetarium
draws two things from this cache:

  * `thumbs/<slug>.jpg` — a THUMB_PX square per tile, written by `--thumbs`
    (and automatically at the end of a fetch run). The sky draws these as
    additive sprites at each object's true size and orientation, so the
    thumbnail bakes in what the shader would otherwise need: the sky
    background level is subtracted and the edges fade to black.
  * `allsky.jpg` / `allsky_small.jpg` — one equirectangular (plate carrée)
    J2000 panorama of the whole sky, the Milky Way background. Fetched by
    `--allsky` from ALLSKY_HIPS.

Nebulae are not all DSS2 (`is_nebula`): one of LARGE_NEBULA_ARCMIN or more
is cut from NSNS_HIPS, a narrowband survey, when it covers the whole tile; a
smaller one gets the best-framing Hubble image on AstroPix warped onto its DSS2
tile, and a sprite made from that image alone over a tighter field (the index's
`thumb_fov`). `sources.json` records what each tile holds, so a later run
upgrades an existing DSS2 cache in place and only redoes what changed; the
AstroPix lookups, including the misses, are cached under `nasa/`.

Resumable: an object whose tile is already on disk, non-empty and from the
right source is skipped, so re-running after an interruption costs nothing. The index is rewritten from
whatever is on disk at the end of every run (and periodically during it), so a
partial run still yields a usable index.

Because existing tiles are skipped regardless of their pixel size, changing
TILE_PX and re-running leaves a *mix* of resolutions on disk. `--status` breaks
the cache down by dimension so that stays visible; `--force` refetches
everything at the current TILE_PX.

Output layout (under --out, default `.cache/dso_tiles/`):
    index.json         [{ name, path, ra, dec, fov, thumb_fov? }, …]  ra/dec J2000 deg
    <slug>.jpg         one cutout per object
    thumbs/<slug>.jpg  THUMB_PX square sprite of the same cutout
    sources.json       { slug: { base, nsns?, nasa? } } for the tiles that aren't plain DSS2
    nasa/<slug>.json   AstroPix lookup: AVM coordinates, credit, licence ({"id": null}: none)
    nasa/<slug>.jpg    the Hubble image as downloaded
    allsky.jpg         ALLSKY_W × ALLSKY_W/2 Milky Way panorama (+ allsky_small.jpg)
"""

from __future__ import annotations

import argparse
import html
import io
import json
import math
import os
import re
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
DSO_BIN = os.path.join(ROOT_DIR, "junos-web", "public", "dso.bin")
# Honour DSO_TILE_DIR so a single env var configures both this prefetcher and
# junos-server (which reads the same var); --out still overrides it.
DEFAULT_OUT = os.environ.get("DSO_TILE_DIR") or os.path.join(ROOT_DIR, ".cache", "dso_tiles")

# Mirror junos-server/src/skysurvey.rs so cached tiles and live cutouts come
# from the same survey — otherwise the preview would change appearance
# depending on whether it was served from cache.
#
# hips2fits is the same API across every CDS host, so these are drop-in
# fallbacks: a tile that a throttled/unavailable mirror refuses is retried
# against the next one. They all draw from the same CDS HiPS collection, so the
# "same survey" invariant above still holds whichever mirror serves the tile.
# The first entry matches junos-server; keep them in sync.
HIPS2FITS_MIRRORS = [
    #"https://alaskybis.u-strasbg.fr/hips-image-services/hips2fits",
    "https://alasky.u-strasbg.fr/hips-image-services/hips2fits",
    "https://alaskybis.cds.unistra.fr/hips-image-services/hips2fits",
    "https://alasky.cds.unistra.fr/hips-image-services/hips2fits",
]
HIPS = "CDS/P/DSS2/color"

TILE_PX = 1024 * 4
# Apparent-size multiplier: a tile should hold the object plus enough sky
# around it to frame a mosaic that overshoots the object itself.
SIZE_MARGIN = 2.0
# Objects with unknown/small sizes still get a usable field; the ceiling keeps
# the resolution of the biggest tiles (3 deg over 1024 px ~ 10.5"/px) tolerable
# and stays inside the server's 10 deg hips2fits clamp.
FOV_MIN_DEG = 1.0
FOV_MAX_DEG = 20.0

# ── planetarium sprites (thumbs/) ────────────────────────────────────────────
THUMB_DIR = "thumbs"
THUMB_PX = 512
THUMB_QUALITY = 85
# Luminance percentile taken as the tile's sky background; everything at or
# below it becomes black so the sprite adds nothing where there is no object.
THUMB_BLACK_PCT = 0.10
# Radial fade: full image out to this fraction of the half-side, black at the
# edge, so adjacent sprites overlap without visible squares.
THUMB_VIGNETTE_START = 0.72

# ── all-sky Milky Way panorama ───────────────────────────────────────────────
# Mellinger's optical panorama is the usual planetarium backdrop; DSS2 colour
# ("CDS/P/DSS2/color") works too but is noisier and shows plate seams at
# this scale.
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

KIND_NAMES = [
    "Galaxy", "OpenCluster", "GlobularCluster", "Nebula",
    "PlanetaryNebula", "SupernovaRemnant", "GalaxyCluster", "DarkNebula",
]

# ── nebula sources ───────────────────────────────────────────────────────────
# Nebulae get better pictures than DSS2's plates. From LARGE_NEBULA_ARCMIN up
# the tile comes from the Northern Sky Narrowband Survey (H-alpha + continuum,
# stars partly subtracted, ~6"/px, CC BY-NC-SA) wherever it covers the whole
# tile; below, a Hubble image found on AstroPix — whose pages carry each
# image's AVM sky coordinates — is warped onto the DSS2 tile. Everything else,
# and any nebula neither source covers, stays DSS2. What each tile holds is
# recorded in SOURCES_FILE so a re-run only redoes what changed.
NEBULA_KINDS = {"Nebula", "PlanetaryNebula", "SupernovaRemnant"}
LARGE_NEBULA_ARCMIN = 10.0
NSNS_HIPS = "simg.de/P/NSNS/DR0_2/hbr8"
NSNS_ARCSEC_PX = 6.4        # HiPS order 6 × 512 px tiles: finer tiles add nothing
NSNS_PROBE_PX = 256         # PNG coverage probe — transparent where NSNS has no data
SOURCES_FILE = "sources.json"

ASTROPIX = "https://www.astropix.org"
NASA_DIR = "nasa"           # <slug>.jpg (the image as downloaded) + <slug>.json (AVM)
# AstroPix tags every search result: keep Hubble visible-light observations,
# drop composites with other missions and illustrations.
NASA_REQUIRE = {"hubble", "observation", "optical", "image_coordinate_complete"}
NASA_REJECT = {"collage", "mission_graphics", "multi-mission", "x-ray", "radio", "single_channel"}
NASA_MAX_CANDIDATES = 10    # image pages read per object
NASA_MAX_PX = 6000          # never download the multi-hundred-MB originals
# A Hubble picture only shows in the planetarium if the sprite is not the whole
# ≥ FOV_MIN_DEG tile, so these objects get a tighter one (index `thumb_fov`).
NASA_THUMB_FOV_MIN = 2.0 / 60.0


# ── dso.bin reader ───────────────────────────────────────────────────────────
# Format mirrors junos-web/src/dso_catalog.rs / gen_dso_catalog.py:
#   [u32] n_objects
#   per object: ra,dec,mag,size,size_minor,pa (6×f32), kind(u8),
#               name_len(u8), name, aliases…, fr_names…, ids…

class _Reader:
    def __init__(self, buf: bytes):
        self.buf = buf
        self.pos = 0

    def take(self, n: int) -> bytes:
        if self.pos + n > len(self.buf):
            raise EOFError("dso.bin truncated")
        out = self.buf[self.pos:self.pos + n]
        self.pos += n
        return out

    def u8(self) -> int:
        return self.take(1)[0]

    def u32(self) -> int:
        return struct.unpack("<I", self.take(4))[0]

    def string(self) -> str:
        return self.take(self.u8()).decode("utf-8", "replace")

    def name_list(self) -> list[str]:
        return [self.string() for _ in range(self.u8())]


def read_dso_bin(path: str) -> list[dict]:
    with open(path, "rb") as f:
        r = _Reader(f.read())
    n = r.u32()
    objects = []
    for _ in range(n):
        ra, dec, mag, size, size_minor, pa = struct.unpack("<ffffff", r.take(24))
        kind = r.u8()
        r.take(4)  # emission signature (u8) + constellation (3 × ASCII) — unused here
        name = r.string()
        r.name_list()  # common_names — unused here
        r.name_list()  # fr_names — unused here
        ids = r.name_list()
        objects.append({
            "name": name,
            "ra": ra,
            "dec": dec,
            "mag": mag,
            "size_arcmin": size,
            "kind": KIND_NAMES[kind] if kind < len(KIND_NAMES) else "?",
            # OpenNGC's "Cl+N" (M42, IC 1805, IC 1396…) is typed as a cluster,
            # but keeps the Sharpless / LBN designation of its nebula.
            "nebulous": any(i.startswith(("Sh2-", "LBN ")) for i in ids),
        })
    if r.pos != len(r.buf):
        print(f"warning: {len(r.buf) - r.pos} trailing bytes in dso.bin", file=sys.stderr)
    return objects


# ── tile geometry ────────────────────────────────────────────────────────────

def tile_fov_deg(size_arcmin: float) -> float:
    """Square field for an object of the given apparent major axis."""
    if not size_arcmin or size_arcmin <= 0:
        return FOV_MIN_DEG
    fov = size_arcmin / 60.0 * SIZE_MARGIN
    return min(max(fov, FOV_MIN_DEG), FOV_MAX_DEG)


def is_nebula(entry: dict) -> bool:
    return entry["kind"] in NEBULA_KINDS or (entry["kind"] == "OpenCluster" and entry["nebulous"])


def wants_nsns(entry: dict) -> bool:
    return is_nebula(entry) and entry["size_arcmin"] >= LARGE_NEBULA_ARCMIN


def wants_nasa(entry: dict) -> bool:
    return is_nebula(entry) and 0 < entry["size_arcmin"] < LARGE_NEBULA_ARCMIN


def nsns_px(fov: float) -> int:
    """NSNS tile side: twice the survey's sampling, within [1024, TILE_PX]."""
    return min(TILE_PX, max(1024, round(fov * 3600.0 / NSNS_ARCSEC_PX * 2)))


def nasa_thumb_fov(entry: dict) -> float:
    return min(entry["fov"], max(entry["size_arcmin"] / 60.0 * SIZE_MARGIN, NASA_THUMB_FOV_MIN))


# ── sky geometry (gnomonic / AVM) ────────────────────────────────────────────

def gnomonic(ra: float, dec: float, ra0: float, dec0: float) -> tuple[float, float]:
    """(ra, dec) → standard coordinates (xi east, eta north) about (ra0, dec0), all degrees."""
    a, d, a0, d0 = map(math.radians, (ra, dec, ra0, dec0))
    cos_c = math.sin(d0) * math.sin(d) + math.cos(d0) * math.cos(d) * math.cos(a - a0)
    xi = math.cos(d) * math.sin(a - a0) / cos_c
    eta = (math.cos(d0) * math.sin(d) - math.sin(d0) * math.cos(d) * math.cos(a - a0)) / cos_c
    return math.degrees(xi), math.degrees(eta)


def inv_gnomonic(xi: float, eta: float, ra0: float, dec0: float) -> tuple[float, float]:
    x, y, d0 = math.radians(xi), math.radians(eta), math.radians(dec0)
    rho = math.hypot(x, y)
    if rho == 0.0:
        return ra0, dec0
    c = math.atan(rho)
    dec = math.asin(math.cos(c) * math.sin(d0) + y * math.sin(c) * math.cos(d0) / rho)
    ra = ra0 + math.degrees(math.atan2(x * math.sin(c),
                                       rho * math.cos(d0) * math.cos(c) - y * math.sin(d0) * math.sin(c)))
    return ra % 360.0, math.degrees(dec)


class AvmWcs:
    """The TAN WCS an AVM block describes, for an image of `width` × `height`.

    AVM follows FITS: ReferencePixel is 1-based with the origin at the bottom
    left, at ReferenceDimension's scale. `sky_to_pix` returns PIL coordinates
    (top-left origin, pixel centres at +0.5) in the image actually in hand.
    """

    def __init__(self, avm: dict, width: int, height: int):
        self.ra0, self.dec0 = avm["ReferenceValue"]
        self.ref_w, self.ref_h = avm["ReferenceDimension"]
        self.px0, self.py0 = avm["ReferencePixel"]
        c1, c2 = avm["Scale"]
        rot = math.radians(avm["Rotation"])
        # CROTA2 convention → CD matrix, then its inverse.
        cd = (c1 * math.cos(rot), -c2 * math.sin(rot), c1 * math.sin(rot), c2 * math.cos(rot))
        det = cd[0] * cd[3] - cd[1] * cd[2]
        self.inv = (cd[3] / det, -cd[1] / det, -cd[2] / det, cd[0] / det)
        self.k = width / self.ref_w
        self.deg_px = abs(c1) / self.k

    def sky_to_pix(self, ra: float, dec: float) -> tuple[float, float]:
        xi, eta = gnomonic(ra, dec, self.ra0, self.dec0)
        px = self.px0 + self.inv[0] * xi + self.inv[1] * eta
        py = self.py0 + self.inv[2] * xi + self.inv[3] * eta
        return (px - 0.5) * self.k, (self.ref_h - (py - 0.5)) * self.k


def slug(name: str) -> str:
    """"NGC 1023" → "ngc1023". Stable and filesystem-safe."""
    return re.sub(r"[^a-z0-9]+", "", name.lower()) or "unnamed"


def jpeg_dims(path: str) -> tuple[int, int] | None:
    """(width, height) read from a JPEG's SOF marker, or None if unreadable.

    Hand-rolled to keep this script dependency-free — Pillow would be one line
    but pulls a wheel in for what is a 20-byte header walk.
    """
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
        # SOF0/1/2 carry the frame dimensions; everything else is skipped by
        # its own length field.
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


# ── Hubble overlay ───────────────────────────────────────────────────────────

def edge_mask(size: tuple[int, int]) -> "Image.Image":
    """`L` alpha for a press image: opaque inside, fading to 0 over the outer
    ~10 % so its frame doesn't show. Its black sky is not masked out — press
    images clip the sky to black right up to the object, and holes there read
    as a ragged ring; `overlay_avm` matches the sky level instead."""
    from PIL import Image, ImageDraw, ImageFilter
    w, h = size
    k = min(1.0, 256 / max(w, h))
    sw, sh = max(1, round(w * k)), max(1, round(h * k))
    r = max(1, round(min(sw, sh) * 0.05))
    m = Image.new("L", (sw, sh), 0)
    ImageDraw.Draw(m).rectangle((r, r, sw - 1 - r, sh - 1 - r), fill=255)
    return m.filter(ImageFilter.GaussianBlur(r / 2)).resize(size, Image.BILINEAR)


def sky_level(im: "Image.Image") -> list[int]:
    """Per-channel background: the THUMB_BLACK_PCT luminance percentile."""
    hist = im.histogram()
    out = []
    for ch in range(3):
        h = hist[ch * 256:(ch + 1) * 256]
        target, acc, level = sum(h) * THUMB_BLACK_PCT, 0, 0
        for v, cnt in enumerate(h):
            acc += cnt
            if acc >= target:
                level = v
                break
        out.append(min(level, 200))
    return out


def overlay_avm(base: "Image.Image", ra0: float, dec0: float, fov: float,
                src: "Image.Image", avm: dict) -> "Image.Image":
    """Warp `src` (with its AVM WCS) onto `base`, a north-up east-left TAN
    square of side `fov` degrees centred on (ra0, dec0).

    The press image's sky level is mapped onto the base's (DSS2's sky is grey,
    a press image's near black) before it replaces the base inside its
    footprint; on a black base its sky simply goes to black.
    """
    from PIL import Image
    n = base.width
    s_t = fov / n
    wcs = AvmWcs(avm, src.width, src.height)
    # Bring the source near the target sampling first: the affine warp does
    # not filter, and a Hubble frame can be 20x finer than a tile.
    if wcs.deg_px < s_t / 1.5:
        f = s_t / wcs.deg_px
        src = src.resize((max(1, round(src.width / f)), max(1, round(src.height / f))), Image.LANCZOS)
        wcs = AvmWcs(avm, src.width, src.height)

    lut = []
    for to, frm in zip(sky_level(base), sky_level(src)):
        lut += [round(to + max(0, v - frm) * (255 - to) / (255 - frm)) for v in range(256)]
    rgba = src.point(lut)
    rgba.putalpha(edge_mask(src.size))

    # Base pixel → sky → source pixel. A press image spans a few arcminutes,
    # so the map is affine to well under a pixel: fit it on three points.
    def to_src(x: float, y: float) -> tuple[float, float]:
        ra, dec = inv_gnomonic(-(x - n / 2) * s_t, -(y - n / 2) * s_t, ra0, dec0)
        return wcs.sky_to_pix(ra, dec)

    c, d = n / 2, n / 8
    x0, y0 = to_src(c, c)
    x1, y1 = to_src(c + d, c)
    x2, y2 = to_src(c, c + d)
    a, b = (x1 - x0) / d, (x2 - x0) / d
    e, f = (y1 - y0) / d, (y2 - y0) / d
    warped = rgba.transform((n, n), Image.AFFINE, (a, b, x0 - a * c - b * c, e, f, y0 - e * c - f * c),
                            resample=Image.BICUBIC, fillcolor=(0, 0, 0, 0))
    return Image.composite(warped.convert("RGB"), base, warped.getchannel("A"))


# ── thumbnails ───────────────────────────────────────────────────────────────

_vignette_cache: dict[int, "Image.Image"] = {}


def vignette_mask(px: int) -> "Image.Image":
    """`L` mask: 255 inside THUMB_VIGNETTE_START, smoothly down to 0 at the
    inscribed circle's edge. Built once per size — the same for every tile."""
    from PIL import Image
    if px in _vignette_cache:
        return _vignette_cache[px]
    half = px / 2.0
    start = THUMB_VIGNETTE_START
    data = bytearray(px * px)
    i = 0
    for y in range(px):
        dy = (y + 0.5 - half) / half
        for x in range(px):
            dx = (x + 0.5 - half) / half
            r = (dx * dx + dy * dy) ** 0.5
            t = (r - start) / (1.0 - start)
            if t <= 0.0:
                v = 255
            elif t >= 1.0:
                v = 0
            else:
                # smoothstep — no visible ring where the fade starts
                v = int(round(255.0 * (1.0 - t * t * (3.0 - 2.0 * t))))
            data[i] = v
            i += 1
    mask = Image.frombytes("L", (px, px), bytes(data))
    _vignette_cache[px] = mask
    return mask


def make_thumb(src: str, dst: str, nasa: tuple | None = None) -> bool:
    """Write the planetarium sprite for one tile. False if the JPEG is unreadable.

    `nasa` = (ra, dec, thumb fov, image path, avm): the sprite is then the
    Hubble image alone, over black, covering only the thumb fov around the
    object — DSS2's bloated glow around a small nebula would ring it.
    """
    from PIL import Image
    try:
        if nasa:
            ra, dec, thumb_fov, img_path, avm = nasa
            src = img_path
            with Image.open(img_path) as hs:
                im = overlay_avm(Image.new("RGB", (THUMB_PX, THUMB_PX)), ra, dec, thumb_fov,
                                 hs.convert("RGB"), avm)
        else:
            with Image.open(src) as im:
                im = im.convert("RGB").resize((THUMB_PX, THUMB_PX), Image.LANCZOS)
    except (OSError, ValueError) as e:
        print(f"    thumb: cannot read {src}: {e}", file=sys.stderr)
        return False

    # Black level: the sky background of a DSS cutout is a grey of 20-50/255.
    # Drawn additively that would add a grey disc around every object, so
    # take a low luminance percentile as the floor and stretch from there.
    hist = im.convert("L").histogram()
    total = sum(hist)
    target = total * THUMB_BLACK_PCT
    acc = 0
    floor = 0
    for v, n in enumerate(hist):
        acc += n
        if acc >= target:
            floor = v
            break
    floor = min(floor, 200)
    if floor > 0:
        scale = 255.0 / (255.0 - floor)
        lut = [max(0, min(255, int(round((v - floor) * scale)))) for v in range(256)]
        im = im.point(lut * 3)

    black = Image.new("RGB", (THUMB_PX, THUMB_PX), (0, 0, 0))
    out = Image.composite(im, black, vignette_mask(THUMB_PX))
    tmp = dst + ".part"
    out.save(tmp, "JPEG", quality=THUMB_QUALITY, optimize=True)
    os.replace(tmp, dst)
    return True


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
        data = fetch_one(allsky_url(base, width), timeout, retries, delay)
        if data is not None:
            break
        if i + 1 < len(HIPS2FITS_MIRRORS):
            print(f"    mirror {base} failed, trying next", file=sys.stderr)
    if data is None:
        return False
    big = os.path.join(out_dir, ALLSKY_FILE)
    tmp = big + ".part"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, big)
    with Image.open(big) as im:
        w, h = im.size
        print(f"allsky: {w}x{h} → {big}")
        small = im.convert("RGB").resize((ALLSKY_SMALL_W, ALLSKY_SMALL_W // 2), Image.LANCZOS)
        small_path = os.path.join(out_dir, ALLSKY_SMALL_FILE)
        small.save(small_path + ".part", "JPEG", quality=88, optimize=True)
        os.replace(small_path + ".part", small_path)
        print(f"allsky: {ALLSKY_SMALL_W}x{ALLSKY_SMALL_W // 2} → {small_path}")
    return True


# ── download ─────────────────────────────────────────────────────────────────

def tile_url(base: str, ra: float, dec: float, fov: float,
             hips: str = HIPS, px: int = TILE_PX, fmt: str = "jpg") -> str:
    q = urllib.parse.urlencode({
        "hips": hips,
        "width": px,
        "height": px,
        "fov": f"{fov:.6f}",
        "projection": "TAN",
        "coordsys": "icrs",
        "ra": f"{ra:.6f}",
        "dec": f"{dec:.6f}",
        "format": fmt,
    })
    return f"{base}?{q}"


def fetch_one(url: str, timeout: float, retries: int, delay: float,
              min_bytes: int = 1024) -> bytes | None:
    """GET one URL with bounded retries. Returns None once the budget is spent."""
    for attempt in range(retries + 1):

        time.sleep(1)
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "junos-web/prefetch"})
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                if resp.status != 200:
                    raise urllib.error.HTTPError(url, resp.status, "bad status", resp.headers, None)
                data = resp.read()
            if len(data) < min_bytes:
                raise ValueError(f"suspiciously small response ({len(data)} bytes)")
            return data
        except Exception as e:  # noqa: BLE001 — any failure is retryable here
            if attempt == retries:
                print(f"    give up: {e}", file=sys.stderr)
                return None
            # Back off; hips2fits throttles under load.
            time.sleep(delay * (2 ** attempt))
    return None


def fetch(ra: float, dec: float, fov: float,
          timeout: float, retries: int, delay: float,
          hips: str = HIPS, px: int = TILE_PX, fmt: str = "jpg", min_bytes: int = 1024) -> bytes | None:
    """Try each mirror in turn; a tile only counts as failed once every mirror
    has spent its retry budget."""
    for i, base in enumerate(HIPS2FITS_MIRRORS):
        data = fetch_one(tile_url(base, ra, dec, fov, hips, px, fmt), timeout, retries, delay, min_bytes)
        if data is not None:
            return data
        if i + 1 < len(HIPS2FITS_MIRRORS):
            print(f"    mirror {base} failed, trying next", file=sys.stderr)
    return None


def nsns_covered(ra: float, dec: float, fov: float,
                 timeout: float, retries: int, delay: float) -> bool | None:
    """Whether NSNS has data over the whole tile (None: could not tell).

    hips2fits paints uncovered sky white in a JPEG — indistinguishable from a
    saturated core — but transparent in a PNG, so a small PNG decides."""
    from PIL import Image
    data = fetch(ra, dec, fov, timeout, retries, delay,
                 hips=NSNS_HIPS, px=NSNS_PROBE_PX, fmt="png", min_bytes=64)
    if data is None:
        return None
    with Image.open(io.BytesIO(data)) as im:
        if "A" not in im.getbands():
            return True
        return im.getchannel("A").getextrema()[0] > 0


# ── AstroPix (Hubble images with AVM coordinates) ────────────────────────────
# AstroPix has no API; its search and image pages are plain HTML whose AVM
# fields carry RDFa `property` attributes, which is what these patterns key on.
_AP_ITEM = re.compile(r"<div class='element-item ([^']*)'([^>]*)>")
_AP_ATTR = re.compile(r"data-(release-date|url)='([^']*)'")
_AP_AVM = re.compile(r"<dd property='avm:Spatial\.(\w+)'>\s*(.*?)\s*</dd>", re.S)
_AP_CREDIT = re.compile(r"<dd property='photoshop:Credit'>\s*(.*?)\s*</dd>", re.S)
_AP_POLICY = re.compile(r"Image Use Policy:\s*(.*?)\s*</p>", re.S)
_AP_SIZE = re.compile(r'href="([^"]+?_\d+\.jpg)">\s*(\d+) x (\d+)')
_AP_ORIGINAL = re.compile(r'Full Size Image\s*\((\d+) x (\d+)\)\s*<br>\s*<a href="([^"]+?_original\.jpg)"')


def _ap_text(s: str) -> str:
    return " ".join(html.unescape(re.sub(r"<[^>]+>", " ", s)).split())


def astropix_search(ra: float, dec: float, radius: float,
                    timeout: float, retries: int, delay: float) -> list[tuple[str, str]] | None:
    """[(image page path, release date)] of the usable Hubble images within
    `radius` degrees, newest first. None if AstroPix could not be reached."""
    q = urllib.parse.urlencode({"ra": f"{ra:.5f}", "dec": f"{dec:.5f}", "radius": f"{radius:.4f}"})
    data = fetch_one(f"{ASTROPIX}/search?{q}", timeout, retries, delay)
    if data is None:
        return None
    out = []
    for m in _AP_ITEM.finditer(data.decode("utf-8", "replace")):
        tags = {t.removeprefix("navigator_param_") for t in m.group(1).split()}
        attrs = dict(_AP_ATTR.findall(m.group(2)))
        if NASA_REQUIRE <= tags and not tags & NASA_REJECT and "url" in attrs:
            out.append((attrs["url"], attrs.get("release-date", "")))
    out.sort(key=lambda t: t[1], reverse=True)
    return out


def astropix_image(path: str, timeout: float, retries: int, delay: float) -> dict | None | bool:
    """The AVM spatial block, credit and JPEG sizes of one AstroPix image page.
    None if it lacks a full TAN J2000 solution, False if it is unreachable."""
    data = fetch_one(f"{ASTROPIX}{path}", timeout, retries, delay)
    if data is None:
        return False
    page = data.decode("utf-8", "replace")
    raw = {k: _ap_text(v) for k, v in _AP_AVM.findall(page)}
    try:
        avm = {
            "ReferenceValue": [float(v) for v in raw["ReferenceValue"].split(",")],
            "ReferenceDimension": [float(v) for v in raw["ReferenceDimension"].split(",")],
            "ReferencePixel": [float(v) for v in raw["ReferencePixel"].split(",")],
            "Scale": [float(v) for v in raw["Scale"].split(",")],
            "Rotation": float(raw["Rotation"]),
        }
    except (KeyError, ValueError):
        return None
    if (raw.get("CoordsystemProjection", "TAN") != "TAN"
            or raw.get("CoordinateFrame", "ICRS") not in ("ICRS", "FK5")
            or raw.get("Quality", "Full") != "Full"
            or any(len(avm[k]) != 2 for k in ("ReferenceValue", "ReferenceDimension", "ReferencePixel", "Scale"))
            or avm["Scale"][0] == 0 or avm["Scale"][1] == 0):
        return None
    sizes = [(int(w), int(h), url) for url, w, h in _AP_SIZE.findall(page)]
    if (o := _AP_ORIGINAL.search(page)):
        sizes.append((int(o.group(1)), int(o.group(2)), o.group(3)))
    if not sizes:
        return None
    credit = _AP_CREDIT.search(page)
    policy = _AP_POLICY.search(page)
    return {
        "id": path.removeprefix("/image/"),
        "page": f"{ASTROPIX}{path}",
        "credit": _ap_text(credit.group(1)) if credit else "",
        "policy": _ap_text(policy.group(1)) if policy else "",
        "avm": avm,
        "sizes": sorted(sizes),
    }


def nasa_score(rec: dict, entry: dict, thumb_fov: float) -> float | None:
    """How well an image frames the object: the share of the sprite it covers,
    or None if it misses much of the object. Catalog sizes are major axes, and
    press frames are often cropped tight, so 70 % of it around the centre will do."""
    avm = rec["avm"]
    w, h = avm["ReferenceDimension"]
    wcs = AvmWcs(avm, int(w), int(h))
    x, y = wcs.sky_to_pix(entry["ra"], entry["dec"])
    r = 0.7 * entry["size_arcmin"] / 120.0 / wcs.deg_px
    if not (r <= x <= w - r and r <= y <= h - r):
        return None
    side = min(w * abs(avm["Scale"][0]), h * abs(avm["Scale"][1]))
    return min(1.0, side / thumb_fov)


def nasa_lookup(entry: dict, nasa_dir: str,
                timeout: float, retries: int, delay: float) -> dict | None | bool:
    """The Hubble image for a small nebula, found once then cached in
    `nasa/<slug>.json` (+ `.jpg`). Returns the record, None when there is no
    usable image, False when AstroPix could not be reached (retry later)."""
    from PIL import Image
    meta_path = os.path.join(nasa_dir, f"{entry['slug']}.json")
    img_path = os.path.join(nasa_dir, f"{entry['slug']}.jpg")
    try:
        with open(meta_path) as f:
            rec = json.load(f)
        if not rec.get("id") or os.path.exists(img_path):
            return rec if rec.get("id") else None
    except (OSError, ValueError):
        rec = None

    if rec is None:
        thumb_fov = nasa_thumb_fov(entry)
        hits = astropix_search(entry["ra"], entry["dec"], max(0.05, entry["size_arcmin"] / 60.0),
                               timeout, retries, delay)
        if hits is None:
            return False
        best, best_score = None, 0.0
        for path, _ in hits[:NASA_MAX_CANDIDATES]:
            cand = astropix_image(path, timeout, retries, delay)
            if cand is False:
                return False
            score = nasa_score(cand, entry, thumb_fov) if cand else None
            # Newest first, so a tie keeps the more recent processing.
            if score is not None and score > best_score + 0.05:
                best, best_score = cand, score
        os.makedirs(nasa_dir, exist_ok=True)
        if best is None:
            with open(meta_path, "w") as f:
                json.dump({"id": None}, f)
            return None
        # Smallest JPEG fine enough for the sprite, else the largest allowed.
        ref_w = best["avm"]["ReferenceDimension"][0]
        want = thumb_fov / THUMB_PX / 1.2
        allowed = [s for s in best["sizes"] if s[0] <= NASA_MAX_PX] or best["sizes"][:1]
        fine = [s for s in allowed if abs(best["avm"]["Scale"][0]) * ref_w / s[0] <= want]
        best["image"] = (fine[0] if fine else allowed[-1])[2]
        del best["sizes"]
        rec = best

    data = fetch_one(rec["image"], timeout, retries, delay)
    if data is None:
        return False
    try:
        with Image.open(io.BytesIO(data)) as im:
            im.verify()
    except (OSError, ValueError):
        return False
    with open(img_path + ".part", "wb") as f:
        f.write(data)
    os.replace(img_path + ".part", img_path)
    with open(meta_path + ".part", "w") as f:
        json.dump(rec, f, indent=1)
    os.replace(meta_path + ".part", meta_path)
    return rec


def overlay_tile(data: bytes, entry: dict, nasa: dict, nasa_dir: str) -> bytes:
    """The tile JPEG with the object's Hubble image warped in."""
    from PIL import Image
    with Image.open(io.BytesIO(data)) as im, \
            Image.open(os.path.join(nasa_dir, f"{entry['slug']}.jpg")) as hs:
        out = overlay_avm(im.convert("RGB"), entry["ra"], entry["dec"], entry["fov"],
                          hs.convert("RGB"), nasa["avm"])
    buf = io.BytesIO()
    out.save(buf, "JPEG", quality=90)
    return buf.getvalue()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", default=DEFAULT_OUT, help=f"cache dir (default: {DEFAULT_OUT})")
    ap.add_argument("--limit", type=int, help="stop after N objects (smoke test)")
    ap.add_argument("--only", help="comma-separated catalog names to fetch, e.g. M31,\"NGC 7000\"")
    ap.add_argument("--workers", type=int, default=4, help="parallel downloads (default: 4)")
    ap.add_argument("--timeout", type=float, default=300.0, help="per-request timeout in seconds")
    ap.add_argument("--retries", type=int, default=2, help="retries per tile")
    ap.add_argument("--delay", type=float, default=1.0, help="base backoff between retries")
    ap.add_argument("--force", action="store_true", help="refetch tiles that already exist")
    ap.add_argument("--index-only", action="store_true", help="rebuild index.json from files on disk, download nothing")
    ap.add_argument("--status", action="store_true", help="report cache coverage and exit, downloading nothing")
    ap.add_argument("--thumbs", action="store_true", help="build thumbs/ for the tiles on disk, download nothing")
    ap.add_argument("--allsky", action="store_true", help="fetch the Milky Way panorama (allsky.jpg), nothing else")
    ap.add_argument("--allsky-px", type=int, default=ALLSKY_W, help=f"panorama width in px (default: {ALLSKY_W})")
    args = ap.parse_args()

    if args.allsky:
        os.makedirs(args.out, exist_ok=True)
        ok = fetch_allsky(args.out, args.allsky_px, args.timeout, args.retries, args.delay)
        return 0 if ok else 1

    objects = read_dso_bin(DSO_BIN)
    print(f"catalog: {len(objects)} objects from {DSO_BIN}")

    if args.only:
        wanted = {n.strip().lower() for n in args.only.split(",") if n.strip()}
        objects = [o for o in objects if o["name"].lower() in wanted]
        missing = wanted - {o["name"].lower() for o in objects}
        for m in sorted(missing):
            print(f"warning: {m!r} not in catalog", file=sys.stderr)
    if args.limit:
        objects = objects[:args.limit]

    os.makedirs(args.out, exist_ok=True)

    # Two objects can slug identically (they shouldn't, but the catalog is
    # generated upstream) — keep the first and warn rather than silently
    # overwriting one tile with another object's sky.
    seen: dict[str, str] = {}
    planned = []
    for o in objects:
        s = slug(o["name"])
        if s in seen:
            print(f"warning: {o['name']!r} and {seen[s]!r} share slug {s!r}; skipping the former", file=sys.stderr)
            continue
        seen[s] = o["name"]
        planned.append({**o, "slug": s, "fov": tile_fov_deg(o["size_arcmin"])})

    def tile_path(entry: dict) -> str:
        return os.path.join(args.out, f"{entry['slug']}.jpg")

    def have(entry: dict) -> bool:
        p = tile_path(entry)
        return os.path.exists(p) and os.path.getsize(p) > 1024

    thumb_dir = os.path.join(args.out, THUMB_DIR)

    def thumb_path(entry: dict) -> str:
        return os.path.join(thumb_dir, f"{entry['slug']}.jpg")

    def have_thumb(entry: dict) -> bool:
        p = thumb_path(entry)
        return os.path.exists(p) and os.path.getsize(p) > 256

    # What each tile on disk was made from: {slug: {"base": "dss2" | "nsns",
    # "nsns": False once NSNS turned out not to cover it, "nasa": AstroPix id
    # of the image warped in}}. A tile absent from it is a plain DSS2 cutout.
    sources_path = os.path.join(args.out, SOURCES_FILE)
    try:
        with open(sources_path) as f:
            sources: dict[str, dict] = json.load(f)
    except (OSError, ValueError):
        sources = {}
    nasa_dir = os.path.join(args.out, NASA_DIR)

    def write_sources() -> None:
        tmp = sources_path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(sources, f, indent=0, sort_keys=True)
        os.replace(tmp, sources_path)

    def nasa_cached(entry: dict) -> dict | None:
        """The cached AstroPix lookup ({"id": None} when nothing fits), or None if never looked up."""
        try:
            with open(os.path.join(nasa_dir, f"{entry['slug']}.json")) as f:
                return json.load(f)
        except (OSError, ValueError):
            return None

    def nasa_args(entry: dict) -> tuple | None:
        """`make_thumb`'s Hubble parameters if one is baked into the tile."""
        nid = sources.get(entry["slug"], {}).get("nasa")
        rec = nasa_cached(entry) if nid else None
        if not rec or rec.get("id") != nid:
            return None
        return (entry["ra"], entry["dec"], nasa_thumb_fov(entry),
                os.path.join(nasa_dir, f"{entry['slug']}.jpg"), rec["avm"])

    def needs_work(entry: dict) -> bool:
        if args.force or not have(entry):
            return True
        m = sources.get(entry["slug"], {})
        if wants_nsns(entry) and m.get("base") != "nsns" and m.get("nsns") is not False:
            return True
        if m.get("base") == "nsns" and not wants_nsns(entry):
            return True
        if wants_nasa(entry):
            rec = nasa_cached(entry)
            return rec is None or (rec.get("id") is not None and m.get("nasa") != rec["id"])
        return False

    def write_thumbs(force: bool) -> int:
        """Build every missing sprite from the tiles on disk. Returns how many were written."""
        todo_t = [e for e in planned if have(e) and (force or not have_thumb(e))]
        if not todo_t:
            return 0
        os.makedirs(thumb_dir, exist_ok=True)
        print(f"thumbs: {len(todo_t)} to build → {thumb_dir}")
        n_done = 0
        t_lock = threading.Lock()
        t_start = time.time()

        def one(entry: dict) -> None:
            nonlocal n_done
            make_thumb(tile_path(entry), thumb_path(entry), nasa_args(entry))
            with t_lock:
                n_done += 1
                n = n_done
            if n % 200 == 0 or n == len(todo_t):
                rate = n / max(time.time() - t_start, 1e-6)
                print(f"  thumbs [{n}/{len(todo_t)}] {rate:.0f}/s")

        with ThreadPoolExecutor(max_workers=max(1, args.workers)) as pool:
            list(pool.map(one, todo_t))
        return len(todo_t)

    def write_index() -> int:
        index = []
        for e in planned:
            if not have(e):
                continue
            item = {
                "name": e["name"],
                "path": f"{e['slug']}.jpg",
                "ra": round(float(e["ra"]), 6),
                "dec": round(float(e["dec"]), 6),
                "fov": round(e["fov"], 6),
            }
            # The sprite of a tile with a Hubble image covers less than the tile.
            if sources.get(e["slug"], {}).get("nasa"):
                item["thumb_fov"] = round(nasa_thumb_fov(e), 6)
            index.append(item)
        index.sort(key=lambda e: e["name"])
        tmp = os.path.join(args.out, "index.json.tmp")
        with open(tmp, "w") as f:
            json.dump(index, f, separators=(",", ":"))
        os.replace(tmp, os.path.join(args.out, "index.json"))
        return len(index)

    present = [e for e in planned if have(e)]
    sizes = [os.path.getsize(tile_path(e)) for e in present]
    # Estimate from what this cache actually holds rather than a hardcoded
    # constant — tile weight moves by ~15x with TILE_PX, so a fixed guess goes
    # badly wrong the moment TILE_PX changes. Fall back to a rough
    # bytes-per-pixel figure for DSS2 colour JPEGs when the cache is empty.
    mean_bytes = (sum(sizes) / len(sizes)) if sizes else TILE_PX * TILE_PX * 0.13

    if args.status:
        n_have, n_all = len(present), len(planned)
        pct = (n_have / n_all * 100.0) if n_all else 0.0
        print(f"cache:   {args.out}")
        print(f"tiles:   {n_have:,} / {n_all:,} ({pct:.1f}%)  {human_bytes(sum(sizes))}")
        print(f"missing: {n_all - n_have:,}  (~{human_bytes((n_all - n_have) * mean_bytes)} to fetch)")
        if sizes:
            print(f"mean:    {human_bytes(mean_bytes)}/tile")

        # Resolution mix — the tell that TILE_PX changed mid-cache, which
        # `have()` will not correct on its own. NSNS tiles are smaller on purpose.
        dims: dict[str, list[int]] = {}
        for e in present:
            d = jpeg_dims(tile_path(e))
            key = f"{d[0]}x{d[1]}" if d else "unreadable"
            px = nsns_px(e["fov"]) if sources.get(e["slug"], {}).get("base") == "nsns" else TILE_PX
            tally = dims.setdefault(key, [0, 0])
            tally[0] += 1
            tally[1] += key != f"{px}x{px}"
        for dim, (count, off) in sorted(dims.items(), key=lambda kv: -kv[1][0]):
            flag = f"  ({off:,} not at the current size; --force to refetch)" if off else ""
            print(f"  {dim:>11}: {count:,}{flag}")

        n_nsns = sum(1 for e in present if sources.get(e["slug"], {}).get("base") == "nsns")
        n_nasa = sum(1 for e in present if sources.get(e["slug"], {}).get("nasa"))
        n_redo = sum(1 for e in present if needs_work(e))
        print(f"nebulae: {n_nsns:,} NSNS tiles, {n_nasa:,} with a Hubble image"
              + (f"  ({n_redo:,} to redo — run a fetch)" if n_redo else ""))

        n_thumbs = sum(1 for e in present if have_thumb(e))
        print(f"thumbs:  {n_thumbs:,} / {n_have:,}"
              + ("" if n_thumbs == n_have else "  (run --thumbs)"))
        for name in (ALLSKY_FILE, ALLSKY_SMALL_FILE):
            p = os.path.join(args.out, name)
            if os.path.exists(p):
                d = jpeg_dims(p)
                print(f"allsky:  {name} {d[0]}x{d[1]}" if d else f"allsky:  {name} unreadable")
            else:
                print(f"allsky:  {name} absent (run --allsky)")

        # A stale index is invisible to the server, which trusts it verbatim.
        idx_path = os.path.join(args.out, "index.json")
        try:
            with open(idx_path) as f:
                n_idx = len(json.load(f))
            state = "in sync" if n_idx == n_have else f"STALE — run --index-only ({n_have:,} on disk)"
            print(f"index:   {n_idx:,} entries, {state}")
        except (OSError, ValueError):
            print(f"index:   absent — run --index-only ({n_have:,} tiles on disk are unused without it)")
        return 0

    if args.thumbs:
        n = write_thumbs(args.force)
        print(f"thumbs: {n} written, {sum(1 for e in planned if have_thumb(e)):,} on disk")
        return 0

    if args.index_only:
        print(f"index.json rebuilt: {write_index()} tiles")
        return 0

    todo = [e for e in planned if needs_work(e)]
    skipped = len(planned) - len(todo)
    print(f"to fetch: {len(todo)}  (up to date on disk: {skipped})")
    if not todo:
        print(f"index.json: {write_index()} tiles")
        write_thumbs(False)
        return 0

    print(f"~{human_bytes(len(todo) * mean_bytes)} estimated, "
          f"{args.workers} workers → {args.out}")

    done = 0
    failed: list[str] = []
    lock = threading.Lock()
    start = time.time()
    net = (args.timeout, args.retries, args.delay)

    def process(entry: dict) -> bool:
        """Bring one tile up to date: its base cutout (NSNS or DSS2) and, for
        a small nebula, its Hubble image. False if something has to be retried."""
        s = entry["slug"]
        with lock:
            m = dict(sources.get(s, {}))
        fresh = (args.force or not have(entry)
                 or (m.get("base") == "nsns" and not wants_nsns(entry)))
        if fresh:
            m.pop("nasa", None)
        ra, dec, fov = entry["ra"], entry["dec"], entry["fov"]
        data = None

        if (wants_nsns(entry) and (fresh or m.get("base") != "nsns")
                and (args.force or m.get("nsns") is not False)):
            covered = nsns_covered(ra, dec, fov, *net)
            if covered is None:
                return False
            if covered:
                data = fetch(ra, dec, fov, *net, hips=NSNS_HIPS, px=nsns_px(fov))
                if data is None:
                    return False
                m["base"] = "nsns"
                m.pop("nsns", None)
            else:
                m["nsns"] = False

        nasa = nasa_lookup(entry, nasa_dir, *net) if wants_nasa(entry) else None
        lookup_failed = nasa is False
        if lookup_failed:
            nasa = None
        # A new tile, or one baked with another Hubble image: start again
        # from a clean DSS2 cutout.
        if data is None and (fresh or (nasa and m.get("nasa") not in (None, nasa["id"]))):
            data = fetch(ra, dec, fov, *net)
            if data is None:
                return False
            m["base"] = "dss2"
            m.pop("nasa", None)
        if nasa and m.get("nasa") != nasa["id"]:
            if data is None:
                with open(tile_path(entry), "rb") as f:
                    data = f.read()
            data = overlay_tile(data, entry, nasa, nasa_dir)
            m["nasa"] = nasa["id"]

        if data is not None:
            # Write via a temp file so an interrupted run never leaves a partial
            # JPEG that `have()` would later mistake for a complete tile.
            tmp = tile_path(entry) + ".part"
            with open(tmp, "wb") as f:
                f.write(data)
            os.replace(tmp, tile_path(entry))
            # The sprite of the old tile is stale; the end-of-run pass rebuilds it.
            try:
                os.remove(thumb_path(entry))
            except FileNotFoundError:
                pass
        with lock:
            if m in ({}, {"base": "dss2"}):
                sources.pop(s, None)
            else:
                sources[s] = m
        return not lookup_failed

    def work(entry: dict) -> None:
        nonlocal done
        ok = process(entry)
        with lock:
            done += 1
            n = done
            if not ok:
                failed.append(entry["name"])
        if n % 25 == 0 or n == len(todo):
            rate = n / max(time.time() - start, 1e-6)
            eta = (len(todo) - n) / rate if rate > 0 else 0
            print(f"  [{n}/{len(todo)}] {entry['name']:<14} "
                  f"{rate * 60:.0f}/min  ETA {eta / 60:.0f} min")
        if n % 250 == 0:
            with lock:
                write_sources()
                write_index()

    try:
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            list(pool.map(work, todo))
    except KeyboardInterrupt:
        print("\ninterrupted — writing index for what landed", file=sys.stderr)

    write_sources()
    total = write_index()
    print(f"\nindex.json: {total} tiles in {args.out}")
    write_thumbs(False)
    if failed:
        print(f"{len(failed)} failed (re-run to retry): {', '.join(failed[:10])}"
              + (" …" if len(failed) > 10 else ""), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
