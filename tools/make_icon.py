"""Generate the 1024x1024 source icon, with no external dependencies.

`pnpm tauri icon tools/icon-source.png` turns this into the .ico/.icns/.png set that the
bundler wants. Kept in the repo so the icon is reproducible rather than a binary somebody
once dragged in.

The motif matches the Android player's leanback banner: a screen outline with a play
triangle, in the same palette.
"""

import os
import struct
import zlib

SIZE = 1024
BG = (0x10, 0x14, 0x18)
SCREEN = (0x1C, 0x24, 0x2C)
ACCENT = (0x4F, 0xC3, 0xF7)

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "icon-source.png")


def chunk(tag, data):
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def render():
    s = SIZE / 1024.0
    px = [[BG for _ in range(SIZE)] for _ in range(SIZE)]

    def rect(x0, y0, x1, y1, colour):
        for y in range(max(0, round(y0 * s)), min(SIZE, round(y1 * s))):
            row = px[y]
            for x in range(max(0, round(x0 * s)), min(SIZE, round(x1 * s))):
                row[x] = colour

    # Screen body and stand, in 1024-unit coordinates.
    rect(160, 200, 864, 660, SCREEN)
    rect(460, 664, 564, 700, SCREEN)
    rect(360, 700, 664, 736, SCREEN)

    # Play triangle, apex right: (420,300) - (700,430) - (420,560).
    y0, y1 = round(300 * s), round(560 * s)
    cy = (y0 + y1) / 2
    half = (y1 - y0) / 2
    x_start = round(420 * s)
    span = 280 * s
    for y in range(max(0, y0), min(SIZE, y1)):
        t = abs(y - cy) / half
        x_end = round(x_start + span * (1 - t))
        row = px[y]
        for x in range(x_start, min(SIZE, x_end)):
            row[x] = ACCENT

    return px


def main():
    px = render()
    raw = b"".join(
        b"\x00" + b"".join(struct.pack("3B", *px[y][x]) for x in range(SIZE))
        for y in range(SIZE)
    )
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(OUT, "wb") as handle:
        handle.write(png)
    print(f"wrote {OUT} ({len(png)} bytes)")


if __name__ == "__main__":
    main()
