#!/usr/bin/env python3
"""Plate-solve all Stellarium nebula images to get accurate WCS corner coordinates.

Uses existing nebulae.json as initial RA/Dec/scale hints, runs astrometry.net's
solve-field on each PNG, reads the resulting WCS, and writes accurate corners
to nebulae.json.

Usage:
    python3 scripts/platesolve_nebulae.py [--workers N] [--timeout S] [--skip-solved]

Requirements:
    - astrometry.net (solve-field in PATH)
    - astropy (pip install astropy)
    - Existing nebulae.json and nebulae/ images from download_nebulae.py
"""

import argparse
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from pathlib import Path

try:
    from astropy.wcs import WCS
    from astropy.io import fits
    import numpy as np
except ImportError:
    sys.exit("ERROR: astropy required — pip install astropy")

SCRIPT_DIR = Path(__file__).parent
ROOT_DIR = SCRIPT_DIR.parent
IMG_DIR = ROOT_DIR / "stars-web" / "public" / "nebulae"
JSON_PATH = ROOT_DIR / "stars-web" / "public" / "nebulae.json"
SOLVED_DIR = SCRIPT_DIR / ".nebulae_solved"  # cache of solved WCS files


def corner_size_deg(corners: list) -> float:
    """Estimate the image diagonal in degrees from the 4 corner coordinates."""
    ras = [c[0] for c in corners]
    decs = [c[1] for c in corners]
    # Width in RA (accounting for cos(dec))
    mid_dec = sum(decs) / 4.0
    cos_dec = math.cos(math.radians(mid_dec))
    dra = (max(ras) - min(ras)) * cos_dec
    ddec = max(decs) - min(decs)
    return math.sqrt(dra**2 + ddec**2)


def center_from_corners(corners: list) -> tuple:
    """Return (ra_deg, dec_deg) center from 4 corners."""
    ra_avg = sum(c[0] for c in corners) / 4.0
    dec_avg = sum(c[1] for c in corners) / 4.0
    return ra_avg, dec_avg


def wcs_corners(wcs: WCS, w: int, h: int) -> list:
    """Return [[ra,dec] x 4] corners in BL, BR, TR, TL order (texture UV space).

    Canvas2D affine mapping uses:
      pts[0] = BL → UV (0,0)
      pts[1] = BR → UV (1,0)
      pts[2] = TR → UV (1,1) (unused in render, kept for completeness)
      pts[3] = TL → UV (0,1)

    Image pixel (0,0) is top-left, so:
      BL = pixel (0, h)
      BR = pixel (w, h)
      TR = pixel (w, 0)
      TL = pixel (0, 0)
    """
    # pixel_to_world uses 0-based (x, y) where x=col, y=row
    pixels = np.array([
        [0,   h],   # BL
        [w,   h],   # BR
        [w,   0],   # TR
        [0,   0],   # TL
    ], dtype=float)
    sky = wcs.all_pix2world(pixels, 0)  # shape (4, 2): [[ra,dec], ...]
    result = []
    for ra, dec in sky:
        if ra < 0:
            ra += 360.0
        result.append([round(float(ra), 5), round(float(dec), 5)])
    return result


def solve_image(task: dict) -> dict:
    """Run solve-field on one image. Returns updated record dict or None on failure.

    task = {
      'name': str,
      'path': str,       # relative path, e.g. "nebulae/m42.png"
      'corners': [...],  # existing corners (used as hint)
      'img_path': str,   # absolute path to PNG
      'solved_dir': str, # directory for caching .wcs files
    }
    """
    name = task["name"]
    img_path = Path(task["img_path"])
    solved_dir = Path(task["solved_dir"])
    corners = task["corners"]
    timeout = task.get("timeout", 120)

    stem = img_path.stem
    cached_wcs = solved_dir / f"{stem}.wcs"

    wcs = None
    if cached_wcs.exists():
        try:
            with fits.open(cached_wcs) as hdul:
                wcs = WCS(hdul[0].header)
        except Exception:
            cached_wcs.unlink(missing_ok=True)

    if wcs is None:
        # Compute hint
        ra, dec = center_from_corners(corners)
        diag = corner_size_deg(corners)
        scale_low = diag * 0.3
        scale_high = diag * 3.0
        scale_low = max(0.01, scale_low)
        scale_high = min(180.0, scale_high)

        with tempfile.TemporaryDirectory() as tmpdir:
            tmp = Path(tmpdir)
            cmd = [
                "solve-field",
                "--no-plots",
                "--no-verify",
                "--crpix-center",
                "--new-fits", "none",
                "--wcs", str(tmp / f"{stem}.wcs"),
                "--ra", str(ra),
                "--dec", str(dec),
                "--radius", str(max(diag * 2, 5.0)),
                "--scale-units", "degwidth",
                "--scale-low", str(scale_low),
                "--scale-high", str(scale_high),
                "--downsample", "4",
                "--dir", str(tmp),
                str(img_path),
            ]
            try:
                result = subprocess.run(
                    cmd,
                    capture_output=True,
                    text=True,
                    timeout=timeout,
                )
            except subprocess.TimeoutExpired:
                return {"name": name, "status": "timeout"}
            except Exception as e:
                return {"name": name, "status": f"error: {e}"}

            wcs_file = tmp / f"{stem}.wcs"
            if not wcs_file.exists():
                return {"name": name, "status": "failed"}

            # Cache the WCS
            solved_dir.mkdir(parents=True, exist_ok=True)
            shutil.copy(wcs_file, cached_wcs)

            try:
                with fits.open(wcs_file) as hdul:
                    wcs = WCS(hdul[0].header)
            except Exception as e:
                return {"name": name, "status": f"wcs-parse: {e}"}

    # Get image dimensions from the PNG
    try:
        from PIL import Image as PILImage
        with PILImage.open(img_path) as im:
            w, h = im.size
    except Exception:
        # Fallback: assume square from wcs NAXIS
        try:
            naxis1 = wcs.pixel_shape[1] if wcs.pixel_shape else 512
            naxis2 = wcs.pixel_shape[0] if wcs.pixel_shape else 512
            w, h = naxis1, naxis2
        except Exception:
            w, h = 512, 512

    try:
        new_corners = wcs_corners(wcs, w, h)
    except Exception as e:
        return {"name": name, "status": f"corners: {e}"}

    return {
        "name": name,
        "path": task["path"],
        "corners": new_corners,
        "status": "ok",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workers", type=int, default=4,
                        help="Parallel solver processes (default: 4)")
    parser.add_argument("--timeout", type=int, default=120,
                        help="Timeout per image in seconds (default: 120)")
    parser.add_argument("--skip-solved", action="store_true",
                        help="Skip images already in .nebulae_solved/ cache")
    parser.add_argument("--filter", type=str, default=None,
                        help="Only process images whose name contains this substring")
    args = parser.parse_args()

    if not JSON_PATH.exists():
        sys.exit(f"ERROR: {JSON_PATH} not found — run download_nebulae.py first")

    with open(JSON_PATH) as f:
        records = json.load(f)

    print(f"Loaded {len(records)} entries from nebulae.json")

    SOLVED_DIR.mkdir(parents=True, exist_ok=True)

    # Deduplicate by path (multiple names can share an image)
    path_to_records: dict = {}
    for rec in records:
        p = rec["path"]
        path_to_records.setdefault(p, []).append(rec)

    tasks = []
    for path, recs in path_to_records.items():
        img_path = ROOT_DIR / "stars-web" / "public" / path
        if not img_path.exists():
            continue
        stem = img_path.stem
        if args.filter and args.filter.lower() not in path.lower():
            continue
        if args.skip_solved and (SOLVED_DIR / f"{stem}.wcs").exists():
            continue
        tasks.append({
            "name": recs[0]["name"],
            "path": path,
            "corners": recs[0]["corners"],
            "img_path": str(img_path),
            "solved_dir": str(SOLVED_DIR),
            "timeout": args.timeout,
        })

    print(f"Tasks to solve: {len(tasks)}")
    if not tasks:
        print("Nothing to do.")
        return

    # Map path → new corners for solved images
    path_to_new_corners: dict = {}
    failed = []
    ok_count = 0

    start = time.time()
    with ProcessPoolExecutor(max_workers=args.workers) as executor:
        futures = {executor.submit(solve_image, t): t for t in tasks}
        done = 0
        for fut in as_completed(futures):
            done += 1
            task = futures[fut]
            try:
                res = fut.result()
            except Exception as e:
                res = {"name": task["name"], "status": f"exception: {e}"}

            status = res.get("status", "?")
            elapsed = time.time() - start
            avg = elapsed / done
            remaining = avg * (len(tasks) - done)
            print(
                f"[{done}/{len(tasks)}] {task['path']:40s}  {status}"
                f"  ETA {remaining:.0f}s",
                flush=True,
            )
            if status == "ok":
                path_to_new_corners[task["path"]] = res["corners"]
                ok_count += 1
            else:
                failed.append((task["path"], status))

    # Update records with new corners where solved
    updated = 0
    for rec in records:
        if rec["path"] in path_to_new_corners:
            rec["corners"] = path_to_new_corners[rec["path"]]
            updated += 1

    print(f"\nSolved: {ok_count}  Failed: {len(failed)}  Updated records: {updated}")
    if failed:
        print("Failed images:")
        for p, reason in failed:
            print(f"  {p}: {reason}")

    with open(JSON_PATH, "w") as f:
        json.dump(records, f, separators=(",", ":"))
    size_kb = JSON_PATH.stat().st_size / 1024
    print(f"Written {JSON_PATH} ({size_kb:.1f} KB)")
    print("Next: cd stars-web && trunk build")


if __name__ == "__main__":
    main()
