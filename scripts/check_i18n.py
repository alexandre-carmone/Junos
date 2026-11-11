#!/usr/bin/env python3
"""Check that every key of junos-web's `translations!` macro is in both
en.json and fr.json: a key missing from one language panics the app the
first time that language is used. Exits 1 listing the missing keys.

Usage: python3 scripts/check_i18n.py
"""
import json
import re
import sys
from pathlib import Path

I18N = Path(__file__).resolve().parent.parent / "junos-web" / "src" / "i18n"


def macro_ids(source: str) -> list[str]:
    # The invocation, not the `macro_rules!` definition above it.
    m = re.search(r"^translations!\s*\{(.*?)^\}", source, re.S | re.M)
    if not m:
        sys.exit("check_i18n: no translations! invocation in i18n/mod.rs")
    body = re.sub(r"//[^\n]*", "", m.group(1))
    return re.findall(r"[A-Za-z_][A-Za-z0-9_]*", body)


def main() -> int:
    ids = macro_ids((I18N / "mod.rs").read_text(encoding="utf-8"))
    failed = False
    for lang in ("en", "fr"):
        keys = json.loads((I18N / f"{lang}.json").read_text(encoding="utf-8")).keys()
        missing = sorted(set(ids) - set(keys))
        if missing:
            failed = True
            print(f"{lang}.json lacks {len(missing)} key(s): {', '.join(missing)}")
    if not failed:
        print(f"check_i18n: {len(ids)} keys, all in en.json and fr.json")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
