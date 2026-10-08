#!/usr/bin/env python3
"""Generates kuma-term.png, the Higuma app icon.

Koguma's sibling, by the same recipe and palette but with the color
balance inverted so the two never read as the same icon: Koguma is a
green folder with dark features, Higuma is a green bear looking into
a dark terminal screen. The screen is an inset panel on the body, and
the face lives on the screen: dot eyes and a prompt, `>_`, drawn in
the accent green. Filled shapes, not outlines: an app icon needs
visual mass to read at 24px in the dock.

Like Koguma's icon, this ships as a 256x256 PNG: raster icons decode
through the true-color path, while the shell tints SVG icons with the
theme text color. Drawn at 4x and downscaled for smooth edges. Run
from this directory:

    python3 make-icon.py
"""

from PIL import Image, ImageDraw, ImageColor

ACCENT = ImageColor.getcolor("#8be4d2", "RGBA")  # kumaOS accent
PANEL_BG = ImageColor.getcolor("#151e1c", "RGBA")  # the dark screen

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
# the dark inset screen: the identity. Koguma's features are dark dots
# on green; Higuma's face is green marks on a dark display.
d.rounded_rectangle(
    [s(11.5), s(19), s(36.5), s(37)],
    radius=s(3.0),
    fill=PANEL_BG,
)
# eyes: accent dots on the dark screen
d.ellipse([s(17 - 1.7), s(23 - 1.7), s(17 + 1.7), s(23 + 1.7)], fill=ACCENT)
d.ellipse([s(31 - 1.7), s(23 - 1.7), s(31 + 1.7), s(23 + 1.7)], fill=ACCENT)
# the prompt as the mouth: `>_` centered, chevron then cursor
d.line(
    [s(18.5), s(27), s(22), s(30.25), s(18.5), s(33.5)],
    fill=ACCENT,
    width=round(s(2.4)),
    joint="curve",
)
r = s(2.4) / 2
for x, y in [(18.5, 27), (18.5, 33.5), (22, 30.25)]:
    d.ellipse([s(x) - r, s(y) - r, s(x) + r, s(y) + r], fill=ACCENT)
# the cursor underscore
d.rounded_rectangle(
    [s(25.5), s(31.9), s(30.5), s(34.3)],
    radius=s(1.0),
    fill=ACCENT,
)

img = img.resize((256, 256), Image.LANCZOS)
img.save("kuma-term.png")
print("kuma-term.png written")
