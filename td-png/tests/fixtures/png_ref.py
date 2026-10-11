#!/usr/bin/env python3
"""Independent PNG decoder and fixture writer, written from the PNG
specification (W3C, third edition) and not from td-png's Rust, as the
oracle for td-png's decoder.

    python3 png_ref.py FILE.png...     print each file's RGBA hash
    python3 png_ref.py make DIR        write the synthetic suite into DIR

Decoding prints `NAME WxH rgba 0x...`, the FNV-1a-64 hash of the decoded
eight-bit RGBA bytes (rows top to bottom, alpha straight). Inflation is
Python's zlib, a second inflater beside the engine's; the chunk walk,
filters, Adam7 and expansion are written here from the specification.

The expansion is the contract td-png shares, so the hash is exact:
a sample of depth 1, 2 or 4 becomes `v * 255 / (2**d - 1)` (exact for
those depths); a 16-bit sample `(v * 255 + 32767) // 65535`; grey is
replicated to R, G and B; a `tRNS` key, compared at the image's own
depth before any scaling and its bits above that depth masked off, makes
its pixels' alpha 0; a palette entry's
alpha is its `tRNS` byte, else 255. Every other ancillary chunk is
skipped.

`make` writes each colour type at each depth the specification allows,
plain and Adam7-interlaced, at 13x11 (so every pass has pixels and a
sub-byte row ends mid-byte); the five filter types in turn by row; the
image data split over three IDAT chunks; a gAMA and a tEXt chunk before
it to be skipped; `tRNS` variants for grey, RGB and palette (an 8-bit
key with bits above the depth set, and beside each 16-bit key a near
miss with the same eight-bit value); and a 1x1
and a 3x1 interlaced frame, whose later passes are empty. Pixels come
from a fixed LCG and zlib level 9, so the bytes are reproducible with a
given zlib (1.3.1 wrote the committed ones).
"""

import os
import struct
import sys
import zlib

SIGNATURE = b"\x89PNG\r\n\x1a\n"
ADAM7 = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
]
CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}


def fnv1a64(data):
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


# ------------------------------------------------------------------ decode


def chunks(data):
    if not data.startswith(SIGNATURE):
        raise ValueError("not PNG")
    pos = len(SIGNATURE)
    while True:
        length, kind = struct.unpack(">I4s", data[pos : pos + 8])
        body = data[pos + 8 : pos + 8 + length]
        (crc,) = struct.unpack(">I", data[pos + 8 + length : pos + 12 + length])
        if zlib.crc32(kind + body) != crc:
            raise ValueError("crc " + kind.decode())
        yield kind, body
        if kind == b"IEND":
            return
        pos += 12 + length


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def unfilter_rows(raw, pos, rows, rowlen, bpp):
    prior = bytearray(rowlen)
    out = []
    for _ in range(rows):
        kind = raw[pos]
        line = bytearray(raw[pos + 1 : pos + 1 + rowlen])
        pos += 1 + rowlen
        for i in range(rowlen):
            a = line[i - bpp] if i >= bpp else 0
            b = prior[i]
            c = prior[i - bpp] if i >= bpp else 0
            if kind == 0:
                p = 0
            elif kind == 1:
                p = a
            elif kind == 2:
                p = b
            elif kind == 3:
                p = (a + b) // 2
            elif kind == 4:
                p = paeth(a, b, c)
            else:
                raise ValueError("filter %d" % kind)
            line[i] = (line[i] + p) & 0xFF
        out.append(line)
        prior = line
    return out, pos


def samples(line, count, depth):
    if depth == 16:
        return [(line[2 * i] << 8) | line[2 * i + 1] for i in range(count)]
    if depth == 8:
        return list(line[:count])
    out = []
    per = 8 // depth
    mask = (1 << depth) - 1
    for i in range(count):
        byte = line[i // per]
        shift = 8 - depth * (i % per + 1)
        out.append((byte >> shift) & mask)
    return out


def scale(v, depth):
    if depth == 16:
        return (v * 255 + 32767) // 65535
    if depth == 8:
        return v
    return v * 255 // ((1 << depth) - 1)


def decode(data):
    width = height = depth = colour = interlace = None
    palette = None
    key = None
    alphas = None
    idat = b""
    for kind, body in chunks(data):
        if kind == b"IHDR":
            width, height, depth, colour, _, _, interlace = struct.unpack(">IIBBBBB", body)
        elif kind == b"PLTE":
            palette = [tuple(body[i : i + 3]) for i in range(0, len(body), 3)]
        elif kind == b"tRNS":
            if colour == 3:
                alphas = list(body)
            else:
                # Bits above the depth are masked off (PNG 11.3.1.1).
                mask = 0xFFFF if depth == 16 else (1 << depth) - 1
                key = tuple(v & mask for v in struct.unpack(">%dH" % (len(body) // 2), body))
        elif kind == b"IDAT":
            idat += body
    raw = zlib.decompress(idat)
    channels = CHANNELS[colour]
    bpp = max(1, channels * depth // 8)
    rgba = bytearray(width * height * 4)
    passes = ADAM7 if interlace else [(0, 0, 1, 1)]
    pos = 0
    for x0, y0, dx, dy in passes:
        w = (width - x0 + dx - 1) // dx if width > x0 else 0
        h = (height - y0 + dy - 1) // dy if height > y0 else 0
        if w == 0 or h == 0:
            continue
        rowlen = (w * channels * depth + 7) // 8
        lines, pos = unfilter_rows(raw, pos, h, rowlen, bpp)
        for r, line in enumerate(lines):
            vals = samples(line, w * channels, depth)
            for c in range(w):
                s = vals[c * channels : (c + 1) * channels]
                if colour == 0:
                    g = scale(s[0], depth)
                    a = 0 if key is not None and key[0] == s[0] else 255
                    px = (g, g, g, a)
                elif colour == 2:
                    a = 0 if key is not None and tuple(key) == tuple(s) else 255
                    px = (scale(s[0], depth), scale(s[1], depth), scale(s[2], depth), a)
                elif colour == 3:
                    r_, g_, b_ = palette[s[0]]
                    a = alphas[s[0]] if alphas is not None and s[0] < len(alphas) else 255
                    px = (r_, g_, b_, a)
                elif colour == 4:
                    g = scale(s[0], depth)
                    px = (g, g, g, scale(s[1], depth))
                else:
                    px = tuple(scale(v, depth) for v in s)
                at = ((y0 + r * dy) * width + x0 + c * dx) * 4
                rgba[at : at + 4] = bytes(px)
    if pos != len(raw):
        raise ValueError("image data length")
    return width, height, bytes(rgba)


# ------------------------------------------------------------------ make


def lcg(state):
    state = (state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
    return state, state >> 33


def chunk(kind, body):
    return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))


def pack_row(vals, depth):
    if depth == 16:
        return b"".join(struct.pack(">H", v) for v in vals)
    if depth == 8:
        return bytes(vals)
    per = 8 // depth
    out = bytearray((len(vals) * depth + 7) // 8)
    for i, v in enumerate(vals):
        out[i // per] |= v << (8 - depth * (i % per + 1))
    return bytes(out)


def filter_row(kind, line, prior, bpp):
    out = bytearray(len(line))
    for i in range(len(line)):
        a = line[i - bpp] if i >= bpp else 0
        b = prior[i]
        c = prior[i - bpp] if i >= bpp else 0
        p = [0, a, b, (a + b) // 2, paeth(a, b, c)][kind]
        out[i] = (line[i] - p) & 0xFF
    return bytes([kind]) + bytes(out)


def make_png(width, height, depth, colour, interlace, seed, palette=None, trns=None, pick=None):
    channels = CHANNELS[colour]
    top = (1 << depth) - 1
    if colour == 3:
        top = len(palette) - 1
    state = seed
    grid = []
    for _ in range(height):
        row = []
        for _ in range(width):
            px = []
            for _ in range(channels):
                state, r = lcg(state)
                px.append(r % (top + 1))
            row.append(px)
        grid.append(row)
    if pick is not None:
        # The tRNS key appears at a few known places.
        for (x, y) in [(0, 0), (5, 3), (12, 10)]:
            if x < width and y < height:
                grid[y][x] = list(pick)
        if depth == 16:
            # A near miss: the same eight-bit value, a different sixteen.
            grid[0][1] = [v ^ 1 for v in pick]
    bpp = max(1, channels * depth // 8)
    passes = ADAM7 if interlace else [(0, 0, 1, 1)]
    raw = bytearray()
    n = 0
    for x0, y0, dx, dy in passes:
        w = (width - x0 + dx - 1) // dx if width > x0 else 0
        h = (height - y0 + dy - 1) // dy if height > y0 else 0
        if w == 0 or h == 0:
            continue
        rowlen = (w * channels * depth + 7) // 8
        prior = bytes(rowlen)
        for r in range(h):
            vals = []
            for c in range(w):
                vals.extend(grid[y0 + r * dy][x0 + c * dx])
            line = pack_row(vals, depth)
            raw += filter_row(n % 5, line, prior, bpp)
            prior = line
            n += 1
    data = zlib.compress(bytes(raw), 9)
    third = max(1, len(data) // 3)
    parts = [data[:third], data[third : 2 * third], data[2 * third :]]
    out = SIGNATURE + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, depth, colour, 0, 0, interlace))
    out += chunk(b"gAMA", struct.pack(">I", 45455))
    out += chunk(b"tEXt", b"Comment\x00td-png fixture")
    if palette is not None:
        out += chunk(b"PLTE", b"".join(bytes(p) for p in palette))
    if trns is not None:
        out += chunk(b"tRNS", trns)
    for part in parts:
        out += chunk(b"IDAT", part)
    return out + chunk(b"IEND", b"")


def suite():
    palette16 = [((i * 37) % 256, (i * 91) % 256, (i * 151) % 256) for i in range(16)]
    palette256 = [((i * 37) % 256, (i * 91) % 256, (i * 151) % 256) for i in range(256)]
    cases = []
    combos = [
        ("g1", 1, 0), ("g2", 2, 0), ("g4", 4, 0), ("g8", 8, 0), ("g16", 16, 0),
        ("rgb8", 8, 2), ("rgb16", 16, 2),
        ("p1", 1, 3), ("p2", 2, 3), ("p4", 4, 3), ("p8", 8, 3),
        ("ga8", 8, 4), ("ga16", 16, 4),
        ("rgba8", 8, 6), ("rgba16", 16, 6),
    ]
    for seed, (name, depth, colour) in enumerate(combos, start=1):
        pal = None
        if colour == 3:
            pal = palette16[: min(16, 1 << depth)] if depth < 8 else palette256
        for interlace in (0, 1):
            suffix = "-i" if interlace else ""
            cases.append((name + suffix, make_png(13, 11, depth, colour, interlace, seed * 2 + interlace, palette=pal)))
    cases.append(("g8-t", make_png(13, 11, 8, 0, 0, 101, trns=struct.pack(">H", 77), pick=[77])))
    cases.append(("g16-t", make_png(13, 11, 16, 0, 1, 102, trns=struct.pack(">H", 4660), pick=[4660])))
    cases.append(("g8-tm", make_png(13, 11, 8, 0, 0, 109, trns=struct.pack(">H", 0x014D), pick=[0x4D])))
    cases.append(("g2-t", make_png(13, 11, 2, 0, 0, 103, trns=struct.pack(">H", 2), pick=[2])))
    cases.append(("rgb8-t", make_png(13, 11, 8, 2, 0, 104, trns=struct.pack(">HHH", 10, 20, 30), pick=[10, 20, 30])))
    cases.append(("rgb16-t", make_png(13, 11, 16, 2, 1, 105, trns=struct.pack(">HHH", 1000, 2000, 3000), pick=[1000, 2000, 3000])))
    cases.append(("p4-t", make_png(13, 11, 4, 3, 0, 106, palette=palette16, trns=bytes([0, 64, 128, 192, 255, 7]))))
    cases.append(("rgba8-1x1-i", make_png(1, 1, 8, 6, 1, 107)))
    cases.append(("g1-3x1-i", make_png(3, 1, 1, 0, 1, 108)))
    return cases


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "make":
        for name, data in suite():
            with open(os.path.join(sys.argv[2], name + ".png"), "wb") as f:
                f.write(data)
        return
    for path in sys.argv[1:]:
        with open(path, "rb") as f:
            w, h, rgba = decode(f.read())
        name = os.path.basename(path).removesuffix(".png")
        print("%s %dx%d rgba 0x%016x" % (name, w, h, fnv1a64(rgba)))


if __name__ == "__main__":
    main()
