#!/usr/bin/env python3
"""Download Stellarium nebulae images and generate the runtime index.

Usage:
    python3 scripts/download_nebulae.py

Output:
  - stars-web/public/nebulae/*.png   (downloaded images)
  - stars-web/public/nebulae.json    (runtime index, fetched by WASM at startup)

Images are fetched from the Stellarium GitHub repository.
Position metadata (sky corners) is read from textures.json.
The JSON is never compiled into the WASM binary — it is served statically
and loaded at runtime by the frontend.
"""

import json
import os
import re
import time
import urllib.request

TEXTURES_URL = (
    "https://raw.githubusercontent.com/Stellarium/stellarium/master"
    "/nebulae/default/textures.json"
)
IMAGE_BASE_URL = (
    "https://raw.githubusercontent.com/Stellarium/stellarium/master"
    "/nebulae/default/"
)

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
ROOT_DIR = os.path.dirname(SCRIPT_DIR)
OUTPUT_IMG_DIR = os.path.join(ROOT_DIR, "stars-web", "public", "nebulae")
JSON_OUTPUT = os.path.join(ROOT_DIR, "stars-web", "public", "nebulae.json")


def parse_names(filename: str) -> list:
    """Extract catalog designations from a Stellarium image filename.

    Name format must match gen_dso_catalog.py exactly so lookups succeed:
      Messier  → "M42"
      NGC      → "NGC 1023"   (space, no leading zeros)
      IC       → "IC 434"     (space, no leading zeros)
    """
    stem = re.sub(r"\.(png|jpg|webp)$", "", filename, flags=re.IGNORECASE)
    names = []

    # Messier: m31, m31_2, m1dumont, m31-ha, etc.
    m = re.match(r"^m(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"M{m.group(1)}")

    # NGC: n1023, n1023h, ngc1023
    m = re.match(r"^n(?:gc)?(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"NGC {int(m.group(1))}")

    # IC: ic434, ic1805
    m = re.match(r"^ic(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"IC {int(m.group(1))}")

    # Abell: abell31, abell85
    m = re.match(r"^abell(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"Abell{m.group(1)}")

    # LBN: lbn437
    m = re.match(r"^lbn(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"LBN{m.group(1)}")

    # LDN: ldn1251
    m = re.match(r"^ldn(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"LDN{m.group(1)}")

    # Sharpless: sh2-1
    m = re.match(r"^sh2-(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"Sh2-{m.group(1)}")

    # VdB: vdb123
    m = re.match(r"^vdb(\d+)", stem, re.IGNORECASE)
    if m:
        names.append(f"VdB{m.group(1)}")

    # PGC: p6830
    m = re.match(r"^p(\d{4,})$", stem, re.IGNORECASE)
    if m:
        names.append(f"PGC{m.group(1)}")

    # UGC: u9792
    m = re.match(r"^u(\d+)$", stem, re.IGNORECASE)
    if m:
        names.append(f"UGC{m.group(1)}")

    return names


def main():
    os.makedirs(OUTPUT_IMG_DIR, exist_ok=True)

    print("Fetching textures.json ...")
    with urllib.request.urlopen(TEXTURES_URL) as r:
        raw = r.read()
    # Stellarium's textures.json contains literal tab characters inside strings.
    text = raw.decode("utf-8", errors="replace")
    data = json.loads(text, strict=False)

    entries = data.get("subTiles", [])
    print(f"Found {len(entries)} texture entries")

    records = []  # JSON output: list of {name, path, corners}

    for entry in entries:
        filename = entry.get("imageUrl", "")
        if not filename.lower().endswith(".png"):
            continue

        world_tiles = entry.get("worldCoords", [])
        if not world_tiles:
            continue

        world = world_tiles[0]
        if len(world) < 4:
            continue

        # Normalize RA (some entries use negative RA values)
        corners = []
        for ra, dec in world:
            if ra < 0:
                ra += 360.0
            corners.append([round(float(ra), 5), round(float(dec), 5)])

        names = parse_names(filename)
        if not names:
            continue

        # Download image if not already cached
        url = IMAGE_BASE_URL + filename
        dest = os.path.join(OUTPUT_IMG_DIR, filename)
        if not os.path.exists(dest):
            try:
                print(f"  {filename} ...", end=" ", flush=True)
                urllib.request.urlretrieve(url, dest)
                print("OK")
                time.sleep(0.05)
            except Exception as e:
                print(f"FAILED: {e}")
                continue

        path = f"nebulae/{filename}"
        for name in names:
            records.append({"name": name, "path": path, "corners": corners})

    print(f"\nWriting {JSON_OUTPUT} ({len(records)} entries) ...")
    with open(JSON_OUTPUT, "w") as f:
        json.dump(records, f, separators=(",", ":"))

    size_kb = os.path.getsize(JSON_OUTPUT) / 1024
    print(f"Done: {size_kb:.1f} KB  ({len(records)} entries)")
    print("Next: cd stars-web && trunk build")


if __name__ == "__main__":
    main()
