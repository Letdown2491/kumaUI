#!/usr/bin/env python3
"""Generates kuma-term.png from kuma-term.svg.

The SVG beside this script is the source of truth: a lavender bear with
ears behind a navy terminal window (white prompt chevron, teal caret).
This script rasterizes it to the 256x256 PNG that build.sh installs
into hicolor. Raster, not svg: the shell tints SVG icons with the theme
text color (gpui discards an SVG's own colors), so raster icons decode
through the true-color path.

Run from this directory:

    python3 make-icon.py
"""

import subprocess
from pathlib import Path

here = Path(__file__).parent

subprocess.run(
    [
        "magick",
        "-background", "none",
        str(here / "kuma-term.svg"),
        "-resize", "256x256",
        "-strip", "-depth", "8",
        str(here / "kuma-term.png"),
    ],
    check=True,
)
print("kuma-term.png written")
