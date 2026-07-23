#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
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
    uv run scripts/prefetch_dso_tiles.py                  # all 7960 objects
    uv run scripts/prefetch_dso_tiles.py --status         # coverage report, no downloads
    uv run scripts/prefetch_dso_tiles.py --limit 50       # smoke test
    uv run scripts/prefetch_dso_tiles.py --workers 8
    uv run scripts/prefetch_dso_tiles.py --only M31,M42,"NGC 7000"

Resumable: an object whose tile is already on disk and non-empty is skipped, so
re-running after an interruption costs nothing. The index is rewritten from
whatever is on disk at the end of every run (and periodically during it), so a
partial run still yields a usable index.

Because existing tiles are skipped regardless of their pixel size, changing
TILE_PX and re-running leaves a *mix* of resolutions on disk. `--status` breaks
the cache down by dimension so that stays visible; `--force` refetches
everything at the current TILE_PX.

Output layout (under --out, default `.cache/dso_tiles/`):
    index.json         [{ name, path, ra, dec, fov }, …]  ra/dec J2000 deg
    <slug>.jpg         one cutout per object
"""

from __future__ import annotations

import argparse
import json
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
HIPS2FITS_URL = "https://alaskybis.u-strasbg.fr/hips-image-services/hips2fits"
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

KIND_NAMES = [
    "Galaxy", "OpenCluster", "GlobularCluster", "Nebula",
    "PlanetaryNebula", "SupernovaRemnant", "GalaxyCluster",
]


# ── dso.bin reader ───────────────────────────────────────────────────────────
# Format mirrors junos-web/src/dso_catalog.rs / gen_dso_catalog.py:
#   [u32] n_objects
#   per object: ra,dec,mag,size,size_minor,pa (6×f32), kind(u8),
#               name_len(u8), name, aliases…, fr_names…

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
        name = r.string()
        r.name_list()  # common_names — unused here
        r.name_list()  # fr_names — unused here
        objects.append({
            "name": name,
            "ra": ra,
            "dec": dec,
            "mag": mag,
            "size_arcmin": size,
            "kind": KIND_NAMES[kind] if kind < len(KIND_NAMES) else "?",
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


# ── download ─────────────────────────────────────────────────────────────────

def tile_url(ra: float, dec: float, fov: float) -> str:
    q = urllib.parse.urlencode({
        "hips": HIPS,
        "width": TILE_PX,
        "height": TILE_PX,
        "fov": f"{fov:.6f}",
        "projection": "TAN",
        "coordsys": "icrs",
        "ra": f"{ra:.6f}",
        "dec": f"{dec:.6f}",
        "format": "jpg",
    })
    return f"{HIPS2FITS_URL}?{q}"


def fetch(url: str, timeout: float, retries: int, delay: float) -> bytes | None:
    """GET with bounded retries. Returns None once the budget is spent."""
    for attempt in range(retries + 1):

        time.sleep(1)
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "junos-web/prefetch"})
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                if resp.status != 200:
                    raise urllib.error.HTTPError(url, resp.status, "bad status", resp.headers, None)
                data = resp.read()
            if len(data) < 1024:
                raise ValueError(f"suspiciously small response ({len(data)} bytes)")
            return data
        except Exception as e:  # noqa: BLE001 — any failure is retryable here
            if attempt == retries:
                print(f"    give up: {e}", file=sys.stderr)
                return None
            # Back off; hips2fits throttles under load.
            time.sleep(delay * (2 ** attempt))
    return None


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
    args = ap.parse_args()

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

    def write_index() -> int:
        index = [
            {
                "name": e["name"],
                "path": f"{e['slug']}.jpg",
                "ra": round(float(e["ra"]), 6),
                "dec": round(float(e["dec"]), 6),
                "fov": round(e["fov"], 6),
            }
            for e in planned if have(e)
        ]
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
        # `have()` will not correct on its own.
        dims: dict[str, int] = {}
        for e in present:
            d = jpeg_dims(tile_path(e))
            dims[f"{d[0]}x{d[1]}" if d else "unreadable"] = \
                dims.get(f"{d[0]}x{d[1]}" if d else "unreadable", 0) + 1
        for dim, count in sorted(dims.items(), key=lambda kv: -kv[1]):
            flag = "" if dim == f"{TILE_PX}x{TILE_PX}" else f"  (not current TILE_PX={TILE_PX}; --force to refetch)"
            print(f"  {dim:>11}: {count:,}{flag}")

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

    if args.index_only:
        print(f"index.json rebuilt: {write_index()} tiles")
        return 0

    todo = [e for e in planned if args.force or not have(e)]
    skipped = len(planned) - len(todo)
    print(f"to fetch: {len(todo)}  (already on disk: {skipped})")
    if not todo:
        print(f"index.json: {write_index()} tiles")
        return 0

    print(f"~{human_bytes(len(todo) * mean_bytes)} estimated, "
          f"{args.workers} workers → {args.out}")

    done = 0
    failed: list[str] = []
    lock = threading.Lock()
    start = time.time()

    def work(entry: dict) -> None:
        nonlocal done
        data = fetch(tile_url(entry["ra"], entry["dec"], entry["fov"]),
                     args.timeout, args.retries, args.delay)
        with lock:
            done += 1
            n = done
        if data is None:
            with lock:
                failed.append(entry["name"])
            return
        # Write via a temp file so an interrupted run never leaves a partial
        # JPEG that `have()` would later mistake for a complete tile.
        tmp = tile_path(entry) + ".part"
        with open(tmp, "wb") as f:
            f.write(data)
        os.replace(tmp, tile_path(entry))
        if n % 25 == 0 or n == len(todo):
            rate = n / max(time.time() - start, 1e-6)
            eta = (len(todo) - n) / rate if rate > 0 else 0
            print(f"  [{n}/{len(todo)}] {entry['name']:<14} "
                  f"{rate * 60:.0f}/min  ETA {eta / 60:.0f} min")
        if n % 250 == 0:
            with lock:
                write_index()

    try:
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            list(pool.map(work, todo))
    except KeyboardInterrupt:
        print("\ninterrupted — writing index for what landed", file=sys.stderr)

    total = write_index()
    print(f"\nindex.json: {total} tiles in {args.out}")
    if failed:
        print(f"{len(failed)} failed (re-run to retry): {', '.join(failed[:10])}"
              + (" …" if len(failed) > 10 else ""), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
