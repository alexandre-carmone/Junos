#!/usr/bin/env python3
"""
Download DSS2 thumbnails for Messier + famous NGC/IC DSOs and generate
16×16 grayscale pixel-art sprites embedded in stars-web/src/dso_sprites.rs.

Usage:
    python3 scripts/gen_dso_sprites.py

Requirements:
    pip install Pillow numpy

The script caches downloaded images in scripts/dso_sprite_cache/ so
re-running is fast. Delete the cache directory to force a re-download.
"""

import io
import os
import re
import sys
import time
import urllib.request
from pathlib import Path

try:
    import numpy as np
    from PIL import Image
except ImportError:
    print("ERROR: Missing dependencies. Run: pip install Pillow numpy")
    sys.exit(1)

# ── Paths ─────────────────────────────────────────────────────────────────────
ROOT       = Path(__file__).parent.parent
CACHE_DIR  = Path(__file__).parent / "dso_sprite_cache"
OUTPUT_RS  = ROOT / "stars-web" / "src" / "dso_sprites.rs"
CATALOG_RS = ROOT / "stars-web" / "src" / "dso_catalog.rs"

CACHE_DIR.mkdir(exist_ok=True)

SPRITE_SIZE   = 16   # output sprite resolution (NxN pixels)
DOWNLOAD_SIZE = 64   # download at higher res then downscale for better AA

# ── Notable DSOs to generate sprites for ─────────────────────────────────────
# All Messier objects are included automatically (parsed from catalog).
# Add extra famous objects here:
EXTRA_NAMES = {
    "NGC 869",   "NGC 884",    # Double Cluster (Perseus)
    "NGC 7293",               # Helix Nebula
    "NGC 4594",               # Sombrero Galaxy
    "NGC 5128",               # Centaurus A
    "NGC 2237",               # Rosette Nebula
    "NGC 7000",               # North America Nebula
    "NGC 6992",               # Eastern Veil Nebula
    "NGC 1499",               # California Nebula
    "NGC 2070",               # Tarantula Nebula
    "NGC 3372",               # Eta Carinae Nebula
    "IC 434",                 # Horsehead Nebula region
    "IC 1805",                # Heart Nebula
    "IC 1848",                # Soul Nebula
    "IC 5070",                # Pelican Nebula
    "NGC 2392",               # Eskimo Nebula
    "NGC 3242",               # Ghost of Jupiter
    "NGC 7331",               # Deer Lick Group
    "NGC 891",                # Edge-on galaxy
    "NGC 4565",               # Needle Galaxy
    "NGC 253",                # Sculptor Galaxy
    "NGC 300",                # Sculptor Dwarf
    "NGC 55",                 # Sculptor Group
    "NGC 1097",               # Barred spiral
    "NGC 1316",               # Fornax A
    "NGC 1365",               # Great Barred Spiral
    "NGC 2903",               # Barred spiral Leo
    "NGC 3031",               # Bode's Galaxy (M81 companion check)
    "NGC 4631",               # Whale Galaxy
    "NGC 4656",               # Hockey Stick
    "NGC 5195",               # M51 companion
    "NGC 6302",               # Bug Nebula
    "NGC 6543",               # Cat's Eye Nebula
    "NGC 7009",               # Saturn Nebula
    "NGC 7662",               # Blue Snowball
    "NGC 2244",               # Rosette cluster
    "NGC 6888",               # Crescent Nebula
    "NGC 6960",               # Western Veil
    "NGC 7027",               # Planetary nebula
}


def is_notable(name: str) -> bool:
    if re.match(r'^M\d+$', name):
        return True
    return name in EXTRA_NAMES


# ── Parse positions from catalog ──────────────────────────────────────────────
LINE_RE = re.compile(
    r'ra_deg:\s*(-?[\d.]+)_f32,\s*dec_deg:\s*(-?[\d.]+)_f32,\s*name:\s*"([^"]+)"'
    r'.*?size_arcmin:\s*([\d.]+)_f32'
)

catalog: dict[str, tuple[float, float, float]] = {}
with open(CATALOG_RS, encoding="utf-8") as f:
    for line in f:
        m = LINE_RE.search(line)
        if m:
            ra   = float(m.group(1))
            dec  = float(m.group(2))
            name = m.group(3)
            size = float(m.group(4))
            catalog[name] = (ra, dec, size)

print(f"Parsed {len(catalog)} DSOs from catalog.")
notable = {name: vals for name, vals in catalog.items() if is_notable(name)}
print(f"Targeting {len(notable)} notable DSOs.")


# ── Image download ────────────────────────────────────────────────────────────
def skyview_url(ra: float, dec: float, size_deg: float, pixels: int) -> str:
    return (
        f"https://skyview.gsfc.nasa.gov/cgi-bin/images?"
        f"Survey=DSS2+Red"
        f"&Position={ra:.6f}%2C{dec:.6f}"
        f"&Size={size_deg:.5f}"
        f"&Pixels={pixels}"
        f"&Return=PNG"
        f"&Scaling=Log"
    )


def fetch_png(name: str, ra: float, dec: float, size_arcmin: float) -> bytes | None:
    safe_name = re.sub(r'[^A-Za-z0-9]', '_', name)
    cache_path = CACHE_DIR / f"{safe_name}.png"
    if cache_path.exists():
        return cache_path.read_bytes()

    # Field size: 1.5× major axis, clamped between 0.15° and 12°
    field_deg = max(min(size_arcmin * 1.5 / 60.0, 12.0), 0.15)
    url = skyview_url(ra, dec, field_deg, DOWNLOAD_SIZE)

    for attempt in range(3):
        try:
            req = urllib.request.Request(
                url,
                headers={"User-Agent": "stars-dso-sprite-gen/1.0"},
            )
            with urllib.request.urlopen(req, timeout=45) as resp:
                data = resp.read()
            # Validate: must be a PNG
            if not data[:4] == b'\x89PNG':
                print(f"  Not a PNG response for {name} (attempt {attempt+1})")
                time.sleep(2.0)
                continue
            cache_path.write_bytes(data)
            time.sleep(0.4)   # be polite to the server
            return data
        except Exception as exc:
            print(f"  Error fetching {name} (attempt {attempt+1}/3): {exc}")
            time.sleep(2.5)
    return None


# ── Image → 16×16 sprite ──────────────────────────────────────────────────────
def to_sprite(data: bytes) -> list[int] | None:
    try:
        img = Image.open(io.BytesIO(data)).convert("L")

        # Log-stretch to handle the enormous dynamic range of astronomical images
        arr = np.array(img, dtype=np.float32)
        arr = np.log1p(arr)

        # Normalize to 0–1
        lo, hi = float(arr.min()), float(arr.max())
        if hi > lo:
            arr = (arr - lo) / (hi - lo)
        else:
            arr[:] = 0.0

        # Gamma < 1 brightens the faint outer structure
        arr = np.power(arr, 0.55)

        # Convert back to PIL for high-quality downscale
        stretched = Image.fromarray((arr * 255).clip(0, 255).astype(np.uint8), mode="L")
        sprite_img = stretched.resize((SPRITE_SIZE, SPRITE_SIZE), Image.LANCZOS)

        return list(sprite_img.getdata())
    except Exception as exc:
        print(f"  Processing error: {exc}")
        return None


# ── Main download loop ────────────────────────────────────────────────────────
sprites: dict[str, list[int]] = {}

for idx, (name, (ra, dec, size_arcmin)) in enumerate(
    sorted(notable.items()), start=1
):
    print(f"[{idx:3d}/{len(notable)}] {name:<15} ", end="", flush=True)

    data = fetch_png(name, ra, dec, size_arcmin)
    if data is None:
        print("FAILED (download)")
        continue

    pixels = to_sprite(data)
    if pixels is None or len(pixels) != SPRITE_SIZE * SPRITE_SIZE:
        print("FAILED (processing)")
        continue

    sprites[name] = pixels
    print("ok")

print(f"\nGenerated {len(sprites)} / {len(notable)} sprites.")

if not sprites:
    print("No sprites generated — nothing to write.")
    sys.exit(1)


# ── Rust source generation ────────────────────────────────────────────────────
def rust_ident(name: str) -> str:
    """Convert 'M31' → 'SPRITE_M31', 'NGC 7293' → 'SPRITE_NGC_7293'."""
    return "SPRITE_" + re.sub(r'[^A-Z0-9]', '_', name.upper()).strip("_")


lines: list[str] = [
    "//! Pixel-art sprites for notable deep-sky objects.",
    "//!",
    "//! Auto-generated by `scripts/gen_dso_sprites.py` — do not edit by hand.",
    "//! Each sprite is a 16×16 grayscale image stored row-major (top→bottom,",
    "//! left→right). Values 0–255: 0 = dark background, 255 = peak brightness.",
    "",
    "/// Return the 16×16 grayscale sprite for a DSO by catalog name, if available.",
    "#[allow(clippy::match_like_matches_macro)]",
    "pub fn get_sprite(name: &str) -> Option<&'static [u8; 256]> {",
    "    match name {",
]

for name in sorted(sprites):
    ident = rust_ident(name)
    lines.append(f'        "{name}" => Some(&{ident}),')

lines += [
    "        _ => None,",
    "    }",
    "}",
    "",
]

for name in sorted(sprites):
    ident = rust_ident(name)
    vals  = ", ".join(str(v) for v in sprites[name])
    lines.append(f"static {ident}: [u8; 256] = [{vals}];")
    lines.append("")

OUTPUT_RS.write_text("\n".join(lines) + "\n", encoding="utf-8")
print(f"Written {OUTPUT_RS}")
