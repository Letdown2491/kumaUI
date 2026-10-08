#!/usr/bin/env python3
"""Generates kuma-term.png, the Higuma app icon.

A bear-cub terminal: solid rounded screen in the kumaOS accent green,
two round ears peeking over the top edge (Koguma's sibling), dot eyes
and a prompt chevron for a mouth in accent_text. The chevron reads as
both the `>` prompt and a grin. Filled shapes, not outlines: an app
icon needs visual mass to read at 24px in the dock.

Like Koguma's icon, this ships as a 256x256 PNG: raster icons decode
through the true-color path, while the shell tints SVG icons with the
theme text color. Drawn at 4x and downscaled for smooth edges. Run
from this directory:

    python3 make-icon.py
"""

from PIL import Image, ImageDraw, ImageColor

ACCENT = ImageColor.getcolor("#8be4d2", "RGBA")  # kumaOS accent
ACCENT_TEXT = ImageColor.getcolor("#0d1211", "RGBA")  # text on accent

SIZE = 1024  # design grid is 48 units; drawn at 4x+ then downscaled


def s(v: float) -> float:
    return v * SIZE / 48.0


img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# ears peek over the body edge, same placement as Koguma's
d.ellipse([s(14.5 - 4.4), s(13.8 - 4.4), s(14.5 + 4.4), s(13.8 + 4.4)], fill=ACCENT)
d.ellipse([s(33.5 - 4.4), s(13.8 - 4.4), s(33.5 + 4.4), s(13.8 + 4.4)], fill=ACCENT)
# terminal body: a rounded square screen, squarer than Koguma's folder
d.rounded_rectangle(
    [s(8), s(15.5), s(40), s(40.5)],
    radius=s(4.5),
    fill=ACCENT,
)
# eyes: the cub's dots
d.ellipse([s(17.5 - 1.9), s(25.5 - 1.9), s(17.5 + 1.9), s(25.5 + 1.9)], fill=ACCENT_TEXT)
d.ellipse([s(30.5 - 1.9), s(25.5 - 1.9), s(30.5 + 1.9), s(25.5 + 1.9)], fill=ACCENT_TEXT)
# the prompt chevron as the mouth: `>` centered under the eyes
d.line(
    [s(20.5), s(30.5), s(24.5), s(34.0), s(20.5), s(37.5)],
    fill=ACCENT_TEXT,
    width=round(s(2.6)),
    joint="curve",
)
# round the chevron's caps so it matches the dot eyes at small sizes
r = s(2.6) / 2
d.ellipse([s(20.5) - r, s(30.5) - r, s(20.5) + r, s(30.5) + r], fill=ACCENT_TEXT)
d.ellipse([s(20.5) - r, s(37.5) - r, s(20.5) + r, s(37.5) + r], fill=ACCENT_TEXT)
d.ellipse([s(24.5) - r, s(34.0) - r, s(24.5) + r, s(34.0) + r], fill=ACCENT_TEXT)

img = img.resize((256, 256), Image.LANCZOS)
img.save("kuma-term.png")
print("kuma-term.png written")
