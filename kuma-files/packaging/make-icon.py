#!/usr/bin/env python3
"""Generates kuma-files.png, the Koguma app icon.

A bear-cub folder: solid filled folder in the kumaOS accent green,
two round ears peeking over the top edge, dot eyes and a nose on the
front in accent_text. Filled shapes, not outlines: an app icon needs
visual mass to read at 24px in the dock.

The shell's dock and launcher render SVG icons tinted with the theme
text color (gpui discards an SVG's own colors), so the icon ships as a
256x256 PNG: raster icons decode through the true-color path. Drawn at
4x and downscaled for smooth edges. Run from this directory:

    python3 make-icon.py
"""

from PIL import Image, ImageDraw, ImageColor

ACCENT = ImageColor.getcolor("#8be4d2", "RGBA")  # kumaOS accent
ACCENT_TEXT = ImageColor.getcolor("#0d1211", "RGBA")  # text on accent
PANEL_BG = ImageColor.getcolor("#151e1c", "RGBA")

# the tab sits a step darker than the body, so the ear in front of it
# keeps a visible edge
def mix(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(4))

TAB = mix(ACCENT, PANEL_BG, 0.38)

SIZE = 1024  # design grid is 48 units; drawn at 4x+ then downscaled


def s(v: float) -> float:
    return v * SIZE / 48.0


img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# folder tab (drawn first, behind everything): top corners rounded only
d.rounded_rectangle(
    [s(6), s(11), s(18), s(18.5)],
    radius=s(2.5),
    fill=TAB,
)
# ears peek over the body edge; the left one overlaps the tab, so the
# darker tab keeps its silhouette readable
d.ellipse([s(14.5 - 4.4), s(13.8 - 4.4), s(14.5 + 4.4), s(13.8 + 4.4)], fill=ACCENT)
d.ellipse([s(33.5 - 4.4), s(13.8 - 4.4), s(33.5 + 4.4), s(13.8 + 4.4)], fill=ACCENT)
# folder body
d.rounded_rectangle(
    [s(5.5), s(16), s(42.5), s(40.5)],
    radius=s(4.5),
    fill=ACCENT,
)
# face: two dot eyes and a nose, nothing that can mush at 24px
d.ellipse([s(18.5 - 1.9), s(26.5 - 1.9), s(18.5 + 1.9), s(26.5 + 1.9)], fill=ACCENT_TEXT)
d.ellipse([s(29.5 - 1.9), s(26.5 - 1.9), s(29.5 + 1.9), s(26.5 + 1.9)], fill=ACCENT_TEXT)
d.ellipse([s(24 - 2.2), s(31.5 - 1.6), s(24 + 2.2), s(31.5 + 1.6)], fill=ACCENT_TEXT)

img = img.resize((256, 256), Image.LANCZOS)
img.save("kuma-files.png")
print("kuma-files.png written")
