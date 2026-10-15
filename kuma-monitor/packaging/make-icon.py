#!/usr/bin/env python3
"""Generates kuma-monitor.png, the Kamuy app icon.

Koguma and Higuma's sibling, by the same recipe and palette: the
family ears at the same two spots peeking over the body's edge, a
face of dark dot eyes and a nose on the accent body. The body is
the difference: a crescent bowl, the moon the watching spirit
inhabits, horns sweeping down on either side and a hollow under
the face. Filled shapes, not outlines: an app icon needs visual
mass to read at 24px in the dock.

Like the other icons this ships as a 256x256 PNG: raster icons
decode through the true-color path, while the shell tints SVG
icons with the theme text color. Drawn at 4x and box-downscaled
for smooth edges. Pure stdlib (no PIL on the host or in the build
container): the PNG encoder is thirty lines of struct and zlib.
Run from this directory:

    python3 make-icon.py
"""

import struct
import zlib

ACCENT = (0x8B, 0xE4, 0xD2, 0xFF)  # kumaOS accent
PANEL_BG = (0x15, 0x1E, 0x1C, 0xFF)  # the dark features
CLEAR = (0, 0, 0, 0)

SIZE = 1024  # design grid is 48 units, drawn at 4x then downscaled
SCALE = SIZE / 48.0


def s(v: float) -> float:
    return v * SCALE


# the geometry, on the 48 grid
MOON = (24.0, 25.0, 15.5)  # the crescent's outer circle
BITE = (24.0, 29.5, 14.0)  # the circle cut away, leaving the bowl
EARS = [(14.5, 13.8, 4.4), (33.5, 13.8, 4.4)]  # the family placement
EYES = [(18.5, 13.0, 1.9), (29.5, 13.0, 1.9)]
NOSE = (24.0, 14.2, 2.2, 1.3)  # center x, y, radii x and y


def in_circle(cx: float, cy: float, r: float, x: float, y: float) -> bool:
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def pixel(x: float, y: float) -> tuple:
    for ex, ey, er in EYES:
        if in_circle(ex, ey, er, x, y):
            return PANEL_BG
    nx, ny, nrx, nry = NOSE
    if ((x - nx) / nrx) ** 2 + ((y - ny) / nry) ** 2 <= 1.0:
        return PANEL_BG
    # ears union the crescent; the bite is carved from the moon only
    for ex, ey, er in EARS:
        if in_circle(ex, ey, er, x, y):
            return ACCENT
    mx, my, mr = MOON
    bx, by, br = BITE
    if in_circle(mx, my, mr, x, y) and not in_circle(bx, by, br, x, y):
        return ACCENT
    return CLEAR


# draw at 4x, premultiplied, then box-downscale to 256
hi = [
    [pixel((x + 0.5) / SCALE, (y + 0.5) / SCALE) for x in range(SIZE)]
    for y in range(SIZE)
]
BOX = int(SIZE / 256)
lo = []
for y in range(256):
    row = bytearray()
    for x in range(256):
        r = g = b = a = 0
        for dy in range(BOX):
            for dx in range(BOX):
                pr, pg, pb, pa = hi[y * BOX + dy][x * BOX + dx]
                m = pa / 255.0
                r += pr * m
                g += pg * m
                b += pb * m
                a += pa
        n = BOX * BOX
        if a:
            r = round(r / a * 255)
            g = round(g / a * 255)
            b = round(b / a * 255)
        row += bytes((int(r), int(g), int(b), int(round(a / n))))
    lo.append(row)


def write_png(path: str, rows: list[bytearray]) -> None:
    raw = b"".join(b"\x00" + bytes(row) for row in rows)

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", 256, 256, 8, 6, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", header))
        f.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        f.write(chunk(b"IEND", b""))


write_png("kuma-monitor.png", lo)
print("kuma-monitor.png written")
