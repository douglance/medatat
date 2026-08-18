#!/usr/bin/env python3
"""Generates packaging/medatat.png, the application icon.

Committed alongside the PNG it produces, so the asset is reproducible rather than a
binary of unknown provenance. Pure standard library: no Pillow, no build-time download.

The mark is a rounded card with three field rows -- a form, which is what the
application is. It is deliberately plain. It exists because AppImage refuses to build
without an Icon entry and because a desktop entry without one shows a broken image in
every menu; it is not a brand.
"""
import struct, zlib

SIZE, SS = 256, 4          # final size, and supersampling factor for smooth edges
W = SIZE * SS
BG   = (0x1B, 0x4F, 0x5A)  # card
ROW  = (0xE8, 0xF1, 0xF2)  # field rows
ROW2 = (0x5F, 0xA8, 0xB0)  # the filled row, to read as data rather than blank lines


def rounded(x, y, x0, y0, x1, y1, r):
    """True when (x, y) is inside the rounded rectangle."""
    if x < x0 or x > x1 or y < y0 or y > y1:
        return False
    for cx, cy in ((x0 + r, y0 + r), (x1 - r, y0 + r), (x0 + r, y1 - r), (x1 - r, y1 - r)):
        # Only the corner quadrants are curved; the straight edges pass through.
        if (x < x0 + r or x > x1 - r) and (y < y0 + r or y > y1 - r):
            if (x - cx) ** 2 + (y - cy) ** 2 <= r * r:
                return True
            continue
    return not ((x < x0 + r or x > x1 - r) and (y < y0 + r or y > y1 - r))


def sample(x, y):
    """Colour at a supersampled pixel, or None for transparent."""
    s = SS
    if not rounded(x, y, 20 * s, 12 * s, 236 * s, 244 * s, 44 * s):
        return None
    rows = ((88, 172, ROW2), (132, 196, ROW), (176, 150, ROW))
    for top, right, colour in rows:
        if rounded(x, y, 56 * s, top * s, right * s, (top + 20) * s, 10 * s):
            return colour
    return BG


def main():
    # Supersample, then box-filter down. Doing it this way keeps the corners and row ends
    # from looking like staircases at 32x32, which is the size that actually gets seen.
    hi = [[sample(x, y) for x in range(W)] for y in range(W)]
    raw = bytearray()
    for y in range(SIZE):
        raw.append(0)  # PNG filter type 0 for this scanline
        for x in range(SIZE):
            r = g = b = a = 0
            for dy in range(SS):
                for dx in range(SS):
                    px = hi[y * SS + dy][x * SS + dx]
                    if px is not None:
                        r += px[0]; g += px[1]; b += px[2]; a += 255
            n = SS * SS
            if a == 0:
                raw += b"\x00\x00\x00\x00"
            else:
                # Un-premultiply: average the colour over covered samples only, so a
                # partly-covered edge pixel keeps its hue instead of fading toward black.
                cov = a // 255
                raw += bytes((r // cov, g // cov, b // cov, a // n))

    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
           + chunk(b"IEND", b""))
    with open("packaging/medatat.png", "wb") as f:
        f.write(png)
    print(f"wrote packaging/medatat.png ({len(png)} bytes, {SIZE}x{SIZE})")

    # Windows .ico. Since Vista an ICO entry may hold a PNG verbatim, so this is a
    # container around the same bytes rather than a re-encode -- no ImageMagick, and
    # nothing to drift out of sync with the PNG.
    ico = (struct.pack("<HHH", 0, 1, 1)                    # reserved, type 1 = icon, count
           + struct.pack("<BBBBHHII",
                         0, 0,        # 0 means 256 in both width and height
                         0, 0,        # palette, reserved
                         1, 32,       # colour planes, bits per pixel
                         len(png), 22)  # payload size, offset past this 22-byte header
           + png)
    with open("packaging/medatat.ico", "wb") as f:
        f.write(ico)
    print(f"wrote packaging/medatat.ico ({len(ico)} bytes)")


if __name__ == "__main__":
    main()
