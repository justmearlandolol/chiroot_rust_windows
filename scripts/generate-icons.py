#!/usr/bin/env python3
"""Generate the Windows icon set used by the Chiroot executable.

The only input is the supplied `favicon.png` artwork at the repository root.
Nothing is redesigned: the image is simply resampled to the sizes Windows and
Tauri expect.

This script runs inside GitHub Actions (see .github/workflows/build-windows.yml)
because the Windows build is never done locally.

Requirements: Pillow (`pip install pillow`).
"""

from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "favicon.png"
ICON_DIR = ROOT / "icons"

# Sizes embedded in the .ico. Windows picks the best one for the taskbar,
# Alt+Tab, Explorer and the executable properties dialog.
ICO_SIZES = [(16, 16), (20, 20), (24, 24), (32, 32), (40, 40), (48, 48), (64, 64), (128, 128), (256, 256)]

# Sizes referenced from tauri.conf.json > bundle.icon.
PNG_SIZES = {"32x32.png": 32, "128x128.png": 128, "128x128@2x.png": 256}


def main() -> int:
    if not SOURCE.is_file():
        print(f"error: missing required asset: {SOURCE}", file=sys.stderr)
        return 1

    ICON_DIR.mkdir(parents=True, exist_ok=True)
    source = Image.open(SOURCE).convert("RGBA")

    for name, size in PNG_SIZES.items():
        source.resize((size, size), Image.LANCZOS).save(ICON_DIR / name)

    source.save(ICON_DIR / "icon.png")
    source.save(ICON_DIR / "icon.ico", sizes=ICO_SIZES)

    generated = sorted(path.name for path in ICON_DIR.iterdir())
    print(f"Generated icons from {SOURCE.name} ({source.width}x{source.height}):")
    for name in generated:
        print(f"  icons/{name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
