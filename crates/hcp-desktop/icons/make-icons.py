#!/usr/bin/env python3
"""Draws HCP's icons from the hexagon in favicon.svg, using only the standard library.

    python3 crates/hcp-desktop/icons/make-icons.py

Writes, next to this script:
  icon.png       1024x1024 app icon: the violet-to-cyan hexagon on a dark rounded square
  tray.png       44x44 menu-bar icon: a black hexagon outline, used as a template image
                 (macOS recolours it for light and dark menu bars)
"""

import math
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))

# favicon.svg: a hexagon on a 64-unit grid, stroke 4, violet (265,90%,65%) to cyan (190,95%,55%).
HEXAGON = [(32, 4), (58, 18), (58, 46), (32, 60), (6, 46), (6, 18)]
VIOLET = (0.6085, 0.415, 0.9585)
CYAN = (0.1775, 0.8061, 0.9725)
BACKGROUND = (0.0784, 0.0824, 0.1020)


def png(path, width, height, pixels):
    """Writes 8-bit RGBA rows (lists of (r, g, b, a) floats in 0..1)."""
    raw = bytearray()
    for row in pixels:
        raw.append(0)
        for r, g, b, a in row:
            raw += bytes(max(0, min(255, round(v * 255))) for v in (r, g, b, a))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    with open(path, "wb") as out:
        out.write(b"\x89PNG\r\n\x1a\n")
        out.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)))
        out.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        out.write(chunk(b"IEND", b""))


def segment_distance(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def hexagon_distance(px, py, points):
    return min(
        segment_distance(px, py, *points[i], *points[(i + 1) % len(points)]) for i in range(len(points))
    )


def rounded_rect_distance(px, py, x0, y0, x1, y1, radius):
    """Signed distance: negative inside."""
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    hx, hy = (x1 - x0) / 2 - radius, (y1 - y0) / 2 - radius
    qx, qy = abs(px - cx) - hx, abs(py - cy) - hy
    outside = math.hypot(max(qx, 0), max(qy, 0))
    return outside + min(max(qx, qy), 0) - radius


def over(top, bottom):
    """Composites `top` over `bottom`, both (r, g, b, a) with straight alpha."""
    ta, ba = top[3], bottom[3]
    a = ta + ba * (1 - ta)
    if a == 0:
        return (0, 0, 0, 0)
    return tuple((top[i] * ta + bottom[i] * ba * (1 - ta)) / a for i in range(3)) + (a,)


def app_icon(size=1024):
    # macOS icon grid: an 824px rounded square centred on the 1024 canvas.
    inset, radius = size * 100 / 1024, size * 185 / 1024
    scale = size * 0.62 / 64
    offset = (size - 64 * scale) / 2
    points = [(offset + x * scale, offset + y * scale) for x, y in HEXAGON]
    half_stroke = 4.6 * scale / 2
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            back = max(0.0, min(1.0, 0.5 - rounded_rect_distance(px, py, inset, inset, size - inset, size - inset, radius)))
            pixel = BACKGROUND + (back,)
            d = hexagon_distance(px, py, points) - half_stroke
            ink = max(0.0, min(1.0, 0.5 - d))
            if ink > 0:
                t = max(0.0, min(1.0, ((px - offset) + (py - offset)) / (2 * 64 * scale)))
                colour = tuple(VIOLET[i] + (CYAN[i] - VIOLET[i]) * t for i in range(3))
                pixel = over(colour + (ink,), pixel)
            row.append(pixel)
        rows.append(row)
    png(os.path.join(HERE, "icon.png"), size, size, rows)


def tray_icon(size=44):
    scale = size * 0.86 / 64
    offset = (size - 64 * scale) / 2
    points = [(offset + x * scale, offset + y * scale) for x, y in HEXAGON]
    half_stroke = 7 * scale / 2
    rows = []
    for y in range(size):
        rows.append([
            (0, 0, 0, max(0.0, min(1.0, 0.5 - (hexagon_distance(x + 0.5, y + 0.5, points) - half_stroke))))
            for x in range(size)
        ])
    png(os.path.join(HERE, "tray.png"), size, size, rows)


if __name__ == "__main__":
    app_icon()
    tray_icon()
    print("wrote icon.png and tray.png in", HERE)
