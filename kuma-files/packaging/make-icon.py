#!/usr/bin/env python3
"""Generates kuma-files.png, the Koguma app icon.

The shell's dock and launcher render SVG icons tinted with the theme
text color (gpui discards an SVG's own colors), so the app icon ships
as a 256x256 PNG instead: raster icons decode through the true-color
path. Drawn at 4x and downscaled for smooth edges. Run from this
directory:

    python3 make-icon.py
"""

from PIL import Image, ImageDraw

# house stroke color, reads on the dark dock and light menu accents
STROKE = (0xC6, 0xCA, 0xD0, 0xFF)

SCALE = 4  # 48-unit design grid, drawn at 192, downscaled to 48... then to 256
SIZE = 1024  # canvas: 48 * ~21.33, gives a crisp 256 after downscale


def s(v: float) -> float:
    return v * SIZE / 48.0


img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# ears
d.ellipse(
    [s(13.5 - 6.2), s(13 - 6.2), s(13.5 + 6.2), s(13 + 6.2)],
    outline=STROKE,
    width=round(s(3)),
)
d.ellipse(
    [s(34.5 - 6.2), s(13 - 6.2), s(34.5 + 6.2), s(13 + 6.2)],
    outline=STROKE,
    width=round(s(3)),
)
# head
d.ellipse(
    [s(24 - 15), s(26.5 - 13.5), s(24 + 15), s(26.5 + 13.5)],
    outline=STROKE,
    width=round(s(3)),
)
# eyes
d.ellipse([s(18.5 - 1.9), s(24 - 1.9), s(18.5 + 1.9), s(24 + 1.9)], fill=STROKE)
d.ellipse([s(29.5 - 1.9), s(24 - 1.9), s(29.5 + 1.9), s(24 + 1.9)], fill=STROKE)
# nose
d.ellipse([s(24 - 2.4), s(30 - 1.8), s(24 + 2.4), s(30 + 1.8)], fill=STROKE)
# mouth: line with round caps (caps drawn as circles)
d.line([s(24), s(32), s(24), s(35.4)], fill=STROKE, width=round(s(2.4)))
d.ellipse([s(24 - 1.2), s(35.4 - 1.2), s(24 + 1.2), s(35.4 + 1.2)], fill=STROKE)

img = img.resize((256, 256), Image.LANCZOS)
img.save("kuma-files.png")
print("kuma-files.png written")
