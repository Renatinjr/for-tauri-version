"""Generate the 1024x1024 source icon, with no external dependencies.

`pnpm tauri icon tools/icon-source.png` turns this into the .ico/.icns/.png set that the
bundler wants. Kept in the repo so the icon is reproducible rather than a binary somebody
once dragged in.

K-player is strictly monochrome, so this is a white play mark on a black rounded tile —
the same motif as the Android launcher icon and banner, drawn from the same numbers.
"""

import math
import os
import struct
import zlib

SIZE = 1024
BLACK = (0x00, 0x00, 0x00)
WHITE = (0xFF, 0xFF, 0xFF)

# The rounded tile, in 1024-unit coordinates.
INSET = 96
RADIUS = 224

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "icon-source.png")


def chunk(tag, data):
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def render():
    # Alpha, not colour: the tile has to be transparent outside its rounded corners or it
    # shows as a black square on a dark taskbar.
    px = [[(0, 0, 0, 0) for _ in range(SIZE)] for _ in range(SIZE)]

    x0, y0 = INSET, INSET
    x1, y1 = SIZE - INSET, SIZE - INSET

    def rounded_tile_alpha(x, y):
        """Coverage of the rounded rectangle at a pixel centre, 0..1, softened at the edge."""
        cx = min(max(x, x0 + RADIUS), x1 - RADIUS)
        cy = min(max(y, y0 + RADIUS), y1 - RADIUS)
        # Inside the straight part of either axis: a plain rectangle test.
        if x0 <= x <= x1 and y0 <= y <= y1 and (cx == x or cy == y):
            return 1.0
        distance = math.hypot(x - cx, y - cy)
        # One pixel of feathering, so the corners are not stair-stepped.
        return max(0.0, min(1.0, RADIUS + 0.5 - distance))

    # Play triangle, apex right: (400,300) - (720,512) - (400,724).
    tri_x0, tri_top, tri_bottom = 400, 300, 724
    tri_span = 320
    tri_cy = (tri_top + tri_bottom) / 2
    tri_half = (tri_bottom - tri_top) / 2

    for y in range(SIZE):
        row = px[y]
        for x in range(SIZE):
            tile = rounded_tile_alpha(x + 0.5, y + 0.5)
            if tile <= 0:
                continue

            colour = BLACK
            if tri_top <= y < tri_bottom and x >= tri_x0:
                t = abs(y + 0.5 - tri_cy) / tri_half
                if x < tri_x0 + tri_span * (1 - t):
                    colour = WHITE

            row[x] = (colour[0], colour[1], colour[2], round(255 * tile))

    return px


def main():
    px = render()
    raw = b"".join(
        b"\x00" + b"".join(struct.pack("4B", *px[y][x]) for x in range(SIZE))
        for y in range(SIZE)
    )
    png = (
        b"\x89PNG\r\n\x1a\n"
        # Colour type 6: truecolour with alpha.
        + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(OUT, "wb") as handle:
        handle.write(png)
    print(f"wrote {OUT} ({len(png)} bytes)")


if __name__ == "__main__":
    main()
