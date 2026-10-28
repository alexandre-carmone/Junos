#!/usr/bin/env python3
"""Pack the offline sky map into plain tar archives and pull them back on
another machine — a copy of the mirror served from *your* server, instead
of a second 13–20 GB crawl of CDS.

The archives sit beside the tiles, in `<skymap>/archive/` by default, which
junos-server serves as-is at `/api/skymap/archive/*` (plain files, Range
requests honoured), so a second install needs only:

    python3 scripts/pack_skymap.py pull http://observatory:8080/api/skymap/archive

Layout — one uncompressed `.tar` per HiPS directory (`Norder{k}/Dir{d}`,
≤ 10,000 JPEGs, ~1 GB at order 7; JPEGs don't compress, and one file per
Dir keeps each download resumable and a partial mirror — `--max-order`,
a `--cone` run — a handful of archives), plus `meta.tar` (`properties`,
`allsky.jpg`, `allsky_small.jpg`) and `manifest.json`:

    archive/manifest.json          what is in here: sizes, sha256, the source info.json
    archive/meta.tar               properties + the Milky Way panorama
    archive/Norder0_Dir0.tar       …
    archive/Norder7_Dir190000.tar  members are `Norder7/Dir190000/Npix*.jpg`

so `tar -xf Norder7_Dir0.tar -C .cache/skymap` restores the tree by hand too.

Incremental: `pack` fingerprints each Dir on disk (tile count, bytes,
newest mtime) and rewrites only the archives whose Dir changed since the
manifest was written — after a `--cone` fetch at order 8 that is a few
archives, not the 24 GB. Archives of Dirs no longer on disk (`--prune-above`)
are deleted. `--force` rewrites everything.

`pull` reads a manifest from a URL or a directory (a USB stick), skips the
archives whose Dir is already complete locally, downloads the rest with
HTTP resume into `<skymap>/.pull/`, verifies the sha256, extracts the
members (names checked against the HiPS layout, nothing else is written),
deletes the tar and finally rewrites `info.json` from what is on disk — so
`--max-order 5` yields an honest `max_order: 5`.

Usage:
    python3 scripts/pack_skymap.py pack                       # (re)build the archives
    python3 scripts/pack_skymap.py pack --max-order 6         # only up to order 6
    python3 scripts/pack_skymap.py status                     # what pack would rewrite
    python3 scripts/pack_skymap.py pull URL_OR_DIR            # into SKYMAP_DIR / .cache/skymap
    python3 scripts/pack_skymap.py pull URL --max-order 6 --insecure   # self-signed :8443

Plain python3, standard library only; it imports `fetch_skymap.py` (same
directory) for the tree scan and `info.json` so the two never disagree.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import ssl
import sys
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, SCRIPT_DIR)
import fetch_skymap as fs  # noqa: E402  (stdlib-only at import time; pillow is lazy)

ARCHIVE_SUBDIR = "archive"
MANIFEST_FILE = "manifest.json"
META_TAR = "meta.tar"
META_MEMBERS = [fs.PROPERTIES_FILE, fs.ALLSKY_FILE, fs.ALLSKY_SMALL_FILE]
MANIFEST_FORMAT = 1
USER_AGENT = "junos-web/pack_skymap"
PULL_SUBDIR = ".pull"
CHUNK = 1 << 20

TAR_NAME_RE = re.compile(r"^Norder(\d+)_Dir(\d+)\.tar$")
TILE_MEMBER_RE = re.compile(r"^Norder(\d+)/Dir(\d+)/Npix(\d+)\.jpg$")


# ── disk ─────────────────────────────────────────────────────────────────────

def default_skymap() -> str:
    return fs.DEFAULT_OUT


def list_dirs(skymap: str) -> list[tuple[int, int, str]]:
    """Every `Norder{k}/Dir{d}` on disk holding at least one tile, as
    (order, dir, path), sorted."""
    out: list[tuple[int, int, str]] = []
    try:
        top = list(os.scandir(skymap))
    except OSError:
        return out
    for e in top:
        if not (e.is_dir() and e.name.startswith("Norder") and e.name[6:].isdigit()):
            continue
        order = int(e.name[6:])
        for d in os.scandir(e.path):
            if d.is_dir() and d.name.startswith("Dir") and d.name[3:].isdigit():
                out.append((order, int(d.name[3:]), d.path))
    out.sort()
    return out


def dir_tiles(path: str) -> list[tuple[int, str, int, int]]:
    """(npix, path, bytes, mtime_ns) of each real tile in a Dir, by npix."""
    tiles = []
    try:
        entries = os.scandir(path)
    except OSError:
        return tiles
    for t in entries:
        if not (t.name.startswith("Npix") and t.name.endswith(".jpg") and t.name[4:-4].isdigit()):
            continue
        try:
            st = t.stat()
        except OSError:
            continue
        if st.st_size > fs.MIN_TILE_BYTES:
            tiles.append((int(t.name[4:-4]), t.path, st.st_size, st.st_mtime_ns))
    tiles.sort()
    return tiles


def fingerprint(files: list[tuple[int, str, int, int]]) -> str:
    """Cheap change detector for a Dir: count, bytes and the newest mtime —
    a refetched tile moves the mtime, a new one the count."""
    n = len(files)
    b = sum(f[2] for f in files)
    m = max((f[3] for f in files), default=0)
    return f"{n}:{b}:{m}"


def meta_files(skymap: str) -> list[tuple[int, str, int, int]]:
    out = []
    for i, name in enumerate(META_MEMBERS):
        p = os.path.join(skymap, name)
        try:
            st = os.stat(p)
        except OSError:
            continue
        out.append((i, p, st.st_size, st.st_mtime_ns))
    return out


def read_manifest(archive_dir: str) -> dict | None:
    try:
        with open(os.path.join(archive_dir, MANIFEST_FILE), encoding="utf-8") as f:
            m = json.load(f)
    except (OSError, ValueError):
        return None
    return m if isinstance(m, dict) and isinstance(m.get("archives"), list) else None


def read_info(skymap: str) -> dict | None:
    try:
        with open(os.path.join(skymap, fs.INFO_FILE), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


class HashingWriter:
    """File-like that sha256s everything written through it."""

    def __init__(self, f):
        self.f = f
        self.h = hashlib.sha256()
        self.n = 0

    def write(self, b):
        self.h.update(b)
        self.n += len(b)
        return self.f.write(b)

    def tell(self):
        return self.n

    def flush(self):
        self.f.flush()

    def close(self):
        self.f.close()


def _clean(ti: tarfile.TarInfo) -> tarfile.TarInfo:
    ti.uid = ti.gid = 0
    ti.uname = ti.gname = ""
    ti.mode = 0o644
    return ti


def write_tar(path: str, members: list[tuple[str, str]]) -> tuple[int, str]:
    """Write `members` ((arcname, source path), already sorted) to `path`
    through `.part` + `os.replace`; returns (size, sha256)."""
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    tmp = f"{path}.{os.getpid()}.part"
    try:
        with open(tmp, "wb") as raw:
            hw = HashingWriter(raw)
            with tarfile.open(fileobj=hw, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for arcname, src in members:
                    tar.add(src, arcname=arcname, recursive=False, filter=_clean)
            hw.flush()
        os.replace(tmp, path)
    except BaseException:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise
    return hw.n, hw.h.hexdigest()


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(CHUNK):
            h.update(chunk)
    return h.hexdigest()


# ── pack ─────────────────────────────────────────────────────────────────────

def plan_groups(skymap: str, max_order: int | None) -> list[dict]:
    """What the archive set should hold, from disk: the meta group first,
    then one group per Dir. Each carries its sorted members and fingerprint."""
    groups: list[dict] = []
    meta = meta_files(skymap)
    if meta:
        groups.append({
            "file": META_TAR, "kind": "meta",
            "members": [(os.path.basename(p), p) for _, p, _, _ in meta],
            "count": len(meta), "bytes": sum(f[2] for f in meta), "fingerprint": fingerprint(meta),
        })
    for order, d, path in list_dirs(skymap):
        if max_order is not None and order > max_order:
            continue
        tiles = dir_tiles(path)
        if not tiles:
            continue
        groups.append({
            "file": f"Norder{order}_Dir{d}.tar", "kind": "tiles", "order": order, "dir": d,
            "members": [(fs.tile_rel(order, npix), p) for npix, p, _, _ in tiles],
            "count": len(tiles), "bytes": sum(t[2] for t in tiles), "fingerprint": fingerprint(tiles),
        })
    return groups


def up_to_date(group: dict, prev: dict | None, archive_dir: str) -> bool:
    if not prev or prev.get("fingerprint") != group["fingerprint"]:
        return False
    try:
        return os.path.getsize(os.path.join(archive_dir, group["file"])) == prev.get("size")
    except OSError:
        return False


def entry_for(group: dict, size: int, digest: str) -> dict:
    e = {"file": group["file"], "kind": group["kind"]}
    if group["kind"] == "tiles":
        e["order"] = group["order"]
        e["dir"] = group["dir"]
        e["tiles"] = group["count"]
    else:
        e["members"] = [a for a, _ in group["members"]]
    e.update({"bytes": group["bytes"], "size": size, "sha256": digest, "fingerprint": group["fingerprint"]})
    return e


def write_manifest(archive_dir: str, skymap: str, entries: list[dict]) -> dict:
    manifest = {
        "format": MANIFEST_FORMAT,
        "survey": fs.SURVEY_ID,
        "created": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "archives": entries,
        "total_size": sum(e["size"] for e in entries),
        "info": read_info(skymap),
    }
    fs.write_atomic(os.path.join(archive_dir, MANIFEST_FILE), (json.dumps(manifest, indent=1) + "\n").encode())
    return manifest


def pack(skymap: str, archive_dir: str, max_order: int | None, force: bool) -> int:
    if not os.path.isdir(skymap):
        print(f"no sky map at {skymap} — run `uv run scripts/fetch_skymap.py` first", file=sys.stderr)
        return 1
    groups = plan_groups(skymap, max_order)
    if not groups:
        print(f"nothing to pack in {skymap}", file=sys.stderr)
        return 1
    os.makedirs(archive_dir, exist_ok=True)
    prev = read_manifest(archive_dir)
    prev_by_file = {e["file"]: e for e in (prev or {}).get("archives", []) if isinstance(e, dict) and "file" in e}
    wanted = {g["file"] for g in groups}

    # Stale archives: Dirs gone from disk or above --max-order.
    for name in sorted(os.listdir(archive_dir)):
        if name in wanted or not (name == META_TAR or TAR_NAME_RE.match(name)):
            continue
        print(f"removing stale {name}")
        os.remove(os.path.join(archive_dir, name))

    # `ready` holds the entries the manifest may claim right now: the kept
    # archives, then each new one as it lands — the manifest is rewritten
    # after every archive so a Ctrl-C leaves a usable, if smaller, set.
    todo = [g for g in groups if force or not up_to_date(g, prev_by_file.get(g["file"]), archive_dir)]
    ready = {g["file"]: prev_by_file[g["file"]] for g in groups if g not in todo}
    print(f"sky map: {skymap}\narchive: {archive_dir}\n"
          f"{len(groups)} archives, {len(todo)} to {'re' if prev else ''}write "
          f"({fs.human_bytes(sum(g['bytes'] for g in todo))})")
    t0 = time.monotonic()
    done_bytes = 0
    for g in todo:
        t1 = time.monotonic()
        size, digest = write_tar(os.path.join(archive_dir, g["file"]), g["members"])
        done_bytes += size
        el = time.monotonic() - t1
        print(f"  wrote {g['file']:<26} {g['count']:>6,} files {fs.human_bytes(size):>10}"
              f"  {el:5.1f} s", flush=True)
        ready[g["file"]] = entry_for(g, size, digest)
        write_manifest(archive_dir, skymap, [ready[h["file"]] for h in groups if h["file"] in ready])
    entries = [ready[g["file"]] for g in groups]
    manifest = write_manifest(archive_dir, skymap, entries)
    el = time.monotonic() - t0
    print(f"done: {len(entries)} archives, {fs.human_bytes(manifest['total_size'])} total, "
          f"{fs.human_bytes(done_bytes)} written in {el:.0f} s → {os.path.join(archive_dir, MANIFEST_FILE)}")
    return 0


def status(skymap: str, archive_dir: str, max_order: int | None) -> int:
    groups = plan_groups(skymap, max_order)
    prev = read_manifest(archive_dir)
    prev_by_file = {e["file"]: e for e in (prev or {}).get("archives", []) if isinstance(e, dict) and "file" in e}
    print(f"sky map: {skymap}\narchive: {archive_dir}" + ("" if prev else "  (no manifest yet)"))
    stale = missing = 0
    for g in groups:
        p = prev_by_file.get(g["file"])
        if p is None:
            state, missing = "missing", missing + 1
        elif up_to_date(g, p, archive_dir):
            state = "ok"
        else:
            state, stale = "stale", stale + 1
        print(f"  {g['file']:<26} {g['count']:>6,} files {fs.human_bytes(g['bytes']):>10}  {state}")
    wanted = {g["file"] for g in groups}
    extra = [n for n in prev_by_file if n not in wanted]
    for n in extra:
        print(f"  {n:<26} {'':>6} {'':>10}  stale (gone from disk)")
    print(f"  {len(groups)} archives: {stale} stale, {missing} missing, {len(extra)} to remove"
          + (f"; manifest {prev['created']}, {fs.human_bytes(prev.get('total_size', 0))}" if prev else ""))
    return 0


# ── pull ─────────────────────────────────────────────────────────────────────

class Source:
    """A manifest and its archives, at a URL or in a directory."""

    def __init__(self, base: str, insecure: bool, timeout: float):
        self.local = os.path.isdir(base)
        self.base = os.path.abspath(base) if self.local else base.rstrip("/")
        self.timeout = timeout
        self.ctx = None
        if insecure:
            self.ctx = ssl.create_default_context()
            self.ctx.check_hostname = False
            self.ctx.verify_mode = ssl.CERT_NONE

    def _url(self, name: str) -> str:
        return os.path.join(self.base, name) if self.local else f"{self.base}/{urllib.parse.quote(name)}"

    def manifest(self) -> dict:
        if self.local:
            with open(self._url(MANIFEST_FILE), encoding="utf-8") as f:
                return json.load(f)
        req = urllib.request.Request(self._url(MANIFEST_FILE), headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(req, timeout=self.timeout, context=self.ctx) as r:
            return json.loads(r.read().decode("utf-8"))

    def download(self, name: str, dest: str, expect_size: int, progress) -> None:
        """Fetch `name` to `dest`, resuming a partial `dest` (HTTP Range)."""
        if self.local:
            shutil.copyfile(self._url(name), dest)
            progress(expect_size)
            return
        have = os.path.getsize(dest) if os.path.exists(dest) else 0
        if have >= expect_size:
            if have > expect_size:
                os.remove(dest)
                have = 0
            else:
                progress(have)
                return
        headers = {"User-Agent": USER_AGENT}
        if have:
            headers["Range"] = f"bytes={have}-"
        req = urllib.request.Request(self._url(name), headers=headers)
        try:
            resp = urllib.request.urlopen(req, timeout=self.timeout, context=self.ctx)
        except urllib.error.HTTPError as e:
            if e.code == 416:          # the server thinks we have it all
                return
            raise
        with resp:
            if have and resp.status != 206:
                have = 0               # no Range support: start over
            mode = "ab" if have else "wb"
            with open(dest, mode) as f:
                while chunk := resp.read(CHUNK):
                    f.write(chunk)
                    have += len(chunk)
                    progress(have)


def local_complete(skymap: str, entry: dict) -> bool:
    if entry.get("kind") == "meta":
        return all(os.path.isfile(os.path.join(skymap, m)) for m in entry.get("members", META_MEMBERS))
    path = os.path.join(skymap, f"Norder{entry['order']}", f"Dir{entry['dir']}")
    return len(dir_tiles(path)) >= int(entry.get("tiles", 0))


def member_ok(entry: dict, name: str) -> bool:
    if entry.get("kind") == "meta":
        return name in META_MEMBERS
    m = TILE_MEMBER_RE.match(name)
    return bool(m) and int(m.group(1)) == entry["order"] and int(m.group(2)) == entry["dir"] \
        and (int(m.group(3)) // 10000) * 10000 == entry["dir"]


def extract(tar_path: str, skymap: str, entry: dict) -> int:
    """Write the members into the tree, each through `.part` + replace;
    anything not named like a tile (or the meta files) is skipped."""
    n = 0
    with tarfile.open(tar_path, mode="r:") as tar:
        for ti in tar:
            if not ti.isfile() or not member_ok(entry, ti.name):
                continue
            src = tar.extractfile(ti)
            if src is None:
                continue
            fs.write_atomic(os.path.join(skymap, ti.name), src.read())
            n += 1
    return n


def pull(source: str, skymap: str, max_order: int | None, force: bool, insecure: bool, timeout: float) -> int:
    src = Source(source, insecure, timeout)
    try:
        manifest = src.manifest()
    except (OSError, ValueError, urllib.error.URLError) as e:
        print(f"cannot read {MANIFEST_FILE} at {source}: {e}", file=sys.stderr)
        return 1
    if manifest.get("format") != MANIFEST_FORMAT:
        print(f"unknown manifest format {manifest.get('format')!r}", file=sys.stderr)
        return 1
    entries = [e for e in manifest.get("archives", [])
               if e.get("kind") == "meta" or max_order is None or int(e.get("order", 99)) <= max_order]
    os.makedirs(skymap, exist_ok=True)
    pull_dir = os.path.join(skymap, PULL_SUBDIR)
    os.makedirs(pull_dir, exist_ok=True)
    todo = [e for e in entries if force or not local_complete(skymap, e)]
    total = sum(int(e["size"]) for e in todo)
    print(f"source:  {src.base}\nsky map: {skymap}\n"
          f"{len(entries)} archives, {len(todo)} to pull ({fs.human_bytes(total)}); manifest {manifest.get('created')}")
    tty = sys.stderr.isatty()
    t0 = time.monotonic()
    pulled = 0
    for i, e in enumerate(todo, 1):
        name, size = e["file"], int(e["size"])
        dest = os.path.join(pull_dir, name + ".part")
        label = f"  [{i}/{len(todo)}] {name:<26} {fs.human_bytes(size):>10}"
        last = [0.0]

        def progress(have: int, label=label, size=size, last=last) -> None:
            now = time.monotonic()
            if tty and (now - last[0] > 0.2 or have >= size):
                last[0] = now
                sys.stderr.write(f"\r{label}  {100.0 * have / max(size, 1):5.1f} %")
                sys.stderr.flush()

        for attempt in (1, 2):
            try:
                src.download(name, dest, size, progress)
            except (OSError, urllib.error.URLError) as err:
                print(f"\n{name}: download failed ({err}); kept {dest} for resume", file=sys.stderr)
                return 1
            if os.path.getsize(dest) == size and sha256_file(dest) == e["sha256"]:
                break
            os.remove(dest)
            if attempt == 2:
                print(f"\n{name}: checksum mismatch twice, giving up", file=sys.stderr)
                return 1
            print(f"\n{name}: checksum mismatch, refetching", file=sys.stderr)
        n = extract(dest, skymap, e)
        os.remove(dest)
        pulled += size
        if tty:
            sys.stderr.write("\r")
        print(f"{label}  {n:>6,} files  ok", flush=True)
    try:
        os.rmdir(pull_dir)
    except OSError:
        pass
    info = fs.write_info(skymap, fs.read_properties(skymap))
    el = time.monotonic() - t0
    print(f"done: {fs.human_bytes(pulled)} pulled in {el:.0f} s; info.json: max_order {info['max_order']}, "
          f"orders {info['orders']}")
    return 0


# ── cli ──────────────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd")

    def common(p: argparse.ArgumentParser, out: bool) -> None:
        p.add_argument("--skymap", default=default_skymap(),
                       help=f"the mirror directory (default {default_skymap()}; SKYMAP_DIR honoured)")
        if out:
            p.add_argument("--out", help=f"archive directory (default <skymap>/{ARCHIVE_SUBDIR})")
        p.add_argument("--max-order", type=int, help="ignore orders above N")

    p = sub.add_parser("pack", help="write / refresh the archives from the mirror")
    common(p, True)
    p.add_argument("--force", action="store_true", help="rewrite every archive")
    p = sub.add_parser("status", help="which archives pack would rewrite")
    common(p, True)
    p = sub.add_parser("pull", help="fetch a packed mirror into the local tree")
    p.add_argument("source", help="URL of the archive directory (…/api/skymap/archive) or a local path")
    common(p, False)
    p.add_argument("--force", action="store_true", help="refetch archives whose Dir is already complete")
    p.add_argument("--insecure", action="store_true", help="skip TLS verification (the server's self-signed cert)")
    p.add_argument("--timeout", type=float, default=60.0)
    args = ap.parse_args()
    if args.cmd is None:
        args.cmd = "pack"
        args.skymap, args.out, args.max_order, args.force = default_skymap(), None, None, False

    skymap = os.path.abspath(args.skymap)
    if args.cmd == "pull":
        return pull(args.source, skymap, args.max_order, args.force, args.insecure, args.timeout)
    archive_dir = os.path.abspath(args.out) if args.out else os.path.join(skymap, ARCHIVE_SUBDIR)
    if args.cmd == "status":
        return status(skymap, archive_dir, args.max_order)
    return pack(skymap, archive_dir, args.max_order, args.force)


if __name__ == "__main__":
    sys.exit(main())
