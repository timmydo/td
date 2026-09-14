#!/usr/bin/env python3
"""Independent baseline JPEG decoder, written from ITU-T T.81 and not from
td-photo's Rust, as the oracle for td-photo's preview decoder.

    python3 jpeg_ref.py FILE.jpg [SCALE]   SCALE in 8 (1/1), 4 (1/2), 2 (1/4), 1 (1/8)

Prints the image geometry, the FNV-1a-64 hash of every dequantised
coefficient in decode order (each as a little-endian i32; an integer-exact check of the entropy decoder
and quantisation), and the FNV-1a-64 hash of the decoded RGB bytes.  With
`--ppm OUT` it also writes the decoded image.  With `--table` it prints the
IDCT constants td-photo's Rust must carry as literals.

Arithmetic contract shared with the Rust decoder, so the pixel hash is
exact and not a tolerance:
  * the inverse DCT is separable float64, rows then columns, each pass
    out[x] = sum_u T[u][x] * in[u] in ascending u, with T[u][x] =
    (c(u)/2) * cos((2x+1) u pi / (2N)) for an N-point (N = 8, 4, 2, 1)
    reduced transform over the first N coefficients of each axis;
  * a sample is floor(v + 128 + 0.5) clamped to 0..255;
  * chroma is replicated (no interpolation) to the luma grid;
  * R = Y + 1.402 (Cr-128), G = Y - 0.344136 (Cb-128) - 0.714136 (Cr-128),
    B = Y + 1.772 (Cb-128), each floor(x + 0.5) clamped to 0..255.
"""
import math
import struct
import sys

ZIGZAG = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
]


def idct_table(n):
    """T[u][x] for the n-point reduced inverse DCT."""
    table = []
    for u in range(n):
        cu = (1.0 / math.sqrt(2.0)) if u == 0 else 1.0
        row = []
        for x in range(n):
            row.append((cu / 2.0) * math.cos((2 * x + 1) * u * math.pi / (2 * n)))
        table.append(row)
    return table


def fnv1a64(data):
    h = 0xcbf29ce484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001b3) & 0xffffffffffffffff
    return h


class Huffman:
    def __init__(self, counts, symbols):
        self.lookup = {}  # (length, code) -> symbol
        code = 0
        k = 0
        for length in range(1, 17):
            for _ in range(counts[length - 1]):
                if code >= 1 << length:
                    raise ValueError("over-full huffman table")
                self.lookup[(length, code)] = symbols[k]
                k += 1
                code += 1
            if counts[length - 1] and code >= 1 << length:
                raise ValueError("huffman code of all ones")
            code <<= 1


class Bits:
    """MSB-first reader over entropy-coded data with 0xFF00 unstuffing;
    a marker ends the data (zeros are supplied past it)."""

    def __init__(self, data, pos):
        self.data = data
        self.pos = pos
        self.acc = 0
        self.n = 0
        self.marker = None

    def _byte(self):
        if self.marker is not None or self.pos >= len(self.data):
            return 0
        b = self.data[self.pos]
        if b == 0xFF:
            nxt = self.data[self.pos + 1] if self.pos + 1 < len(self.data) else 0
            if nxt == 0x00:
                self.pos += 2
                return 0xFF
            self.marker = nxt
            return 0
        self.pos += 1
        return b

    def bit(self):
        if self.n == 0:
            self.acc = self._byte()
            self.n = 8
        self.n -= 1
        return (self.acc >> self.n) & 1

    def bits(self, count):
        v = 0
        for _ in range(count):
            v = (v << 1) | self.bit()
        return v

    def symbol(self, table):
        code = 0
        for length in range(1, 17):
            code = (code << 1) | self.bit()
            sym = table.lookup.get((length, code))
            if sym is not None:
                return sym
        raise ValueError("bad huffman code")

    def restart(self, expected):
        """Byte-align, consume the RSTn marker (n = expected), continue.
        Only the padding of the last byte may remain: an unread byte before
        the marker is a malformed interval, and so is a marker out of
        sequence."""
        self.n = 0
        if self.marker is None:
            # The marker was not yet met: it must be next in the data.
            while self.pos + 1 < len(self.data) and self.data[self.pos] == 0xFF and self.data[self.pos + 1] == 0xFF:
                self.pos += 1
            if self.pos + 1 < len(self.data) and self.data[self.pos] == 0xFF:
                self.marker = self.data[self.pos + 1]
                self.pos += 2
        else:
            self.pos += 2
        if self.marker != 0xD0 + expected:
            raise ValueError("expected restart marker %d" % expected)
        self.marker = None

    def finish(self):
        """After the last MCU: only padding remains and EOI follows."""
        p = self.pos
        while p + 1 < len(self.data) and self.data[p] == 0xFF and self.data[p + 1] == 0xFF:
            p += 1
        if not (p + 1 < len(self.data) and self.data[p] == 0xFF and self.data[p + 1] == 0xD9):
            raise ValueError("expected EOI after the scan")
        return p


def extend(v, t):
    if t == 0:
        return 0
    return v if v >= (1 << (t - 1)) else v - (1 << t) + 1


def decode(data, scale=8):
    if data[:2] != b"\xff\xd8":
        raise ValueError("no SOI")
    pos = 2
    qt = {}
    dc = {}
    ac = {}
    frame = None
    restart_interval = 0
    coefficient_bytes = bytearray()
    while pos < len(data):
        if data[pos] != 0xFF:
            raise ValueError("expected marker at %d" % pos)
        pos += 1
        marker = data[pos]
        pos += 1
        while marker == 0xFF:  # fill bytes before a marker
            marker = data[pos]
            pos += 1
        if marker == 0xD8 or 0xD0 <= marker <= 0xD7 or marker == 0x01:
            continue
        if marker == 0xD9:
            break
        (length,) = struct.unpack(">H", data[pos:pos + 2])
        seg = data[pos + 2:pos + length]
        pos += length
        if marker == 0xDB:
            i = 0
            while i < len(seg):
                pq, tq = seg[i] >> 4, seg[i] & 15
                i += 1
                if pq == 0:
                    vals = list(seg[i:i + 64])
                    i += 64
                else:
                    vals = list(struct.unpack(">64H", seg[i:i + 128]))
                    i += 128
                qt[tq] = vals  # zigzag order
        elif marker == 0xC4:
            i = 0
            while i < len(seg):
                tc, th = seg[i] >> 4, seg[i] & 15
                counts = list(seg[i + 1:i + 17])
                total = sum(counts)
                symbols = list(seg[i + 17:i + 17 + total])
                i += 17 + total
                (dc if tc == 0 else ac)[th] = Huffman(counts, symbols)
        elif marker in (0xC0, 0xC1):
            if frame is not None:
                raise ValueError("second frame")
            p, y, x, nf = seg[0], struct.unpack(">H", seg[1:3])[0], struct.unpack(">H", seg[3:5])[0], seg[5]
            if p != 8:
                raise ValueError("precision %d" % p)
            comps = []
            for k in range(nf):
                cid, hv, tq = seg[6 + 3 * k], seg[7 + 3 * k], seg[8 + 3 * k]
                comps.append({"id": cid, "h": hv >> 4, "v": hv & 15, "tq": tq})
            frame = {"width": x, "height": y, "comps": comps}
        elif marker in (0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCC, 0xCD, 0xCE, 0xCF, 0xDE, 0xDF):
            raise ValueError("unsupported marker %02x" % marker)
        elif marker == 0xDD:
            (restart_interval,) = struct.unpack(">H", seg[0:2])
        elif marker == 0xDA:
            if frame is None:
                raise ValueError("SOS before SOF")
            ns = seg[0]
            scan = []
            for k in range(ns):
                cs, t = seg[1 + 2 * k], seg[2 + 2 * k]
                comp = next(c for c in frame["comps"] if c["id"] == cs)
                if frame["comps"].index(comp) != k:
                    raise ValueError("scan order differs from the frame")
                scan.append((comp, dc[t >> 4], ac[t & 15]))
            if ns != len(frame["comps"]):
                raise ValueError("multi-scan baseline is not supported")
            pos = entropy_decode(data, pos, frame, scan, qt, restart_interval, scale, coefficient_bytes)
        # APPn, COM and the rest are skipped.
    if frame is None or "planes" not in frame:
        raise ValueError("no image")
    return frame, bytes(coefficient_bytes)


def entropy_decode(data, pos, frame, scan, qt, restart_interval, scale, coefficient_bytes):
    comps = frame["comps"]
    hmax = max(c["h"] for c in comps)
    vmax = max(c["v"] for c in comps)
    width, height = frame["width"], frame["height"]
    n = scale  # output samples per block axis
    tables = idct_table(n)
    single = len(scan) == 1
    if single:
        comp = scan[0][0]
        cw = -(-width * comp["h"] // hmax)
        ch = -(-height * comp["v"] // vmax)
        mcus_x = -(-cw // 8)
        mcus_y = -(-ch // 8)
    else:
        mcus_x = -(-width // (8 * hmax))
        mcus_y = -(-height // (8 * vmax))
    planes = {}
    for comp, _, _ in scan:
        bh = (comp["h"] if not single else 1)
        bv = (comp["v"] if not single else 1)
        pw = mcus_x * bh * n
        ph = mcus_y * bv * n
        planes[comp["id"]] = {"w": pw, "h": ph, "px": bytearray(pw * ph)}
    bits = Bits(data, pos)
    pred = {comp["id"]: 0 for comp, _, _ in scan}
    mcu_count = 0
    intervals = 0
    for my in range(mcus_y):
        for mx in range(mcus_x):
            if restart_interval and mcu_count and mcu_count % restart_interval == 0:
                bits.restart(intervals % 8)
                intervals += 1
                for k in pred:
                    pred[k] = 0
            mcu_count += 1
            for comp, dct, act in scan:
                bh = (comp["h"] if not single else 1)
                bv = (comp["v"] if not single else 1)
                q = qt[comp["tq"]]
                for by in range(bv):
                    for bx in range(bh):
                        coef = [0] * 64  # natural order
                        t = bits.symbol(dct)
                        if t > 11:
                            raise ValueError("dc category past 11")
                        diff = extend(bits.bits(t), t)
                        pred[comp["id"]] += diff
                        if not -32768 <= pred[comp["id"]] <= 32767:
                            raise ValueError("dc predictor out of range")
                        coef[0] = pred[comp["id"]] * q[0]
                        k = 1
                        while k < 64:
                            rs = bits.symbol(act)
                            r, s = rs >> 4, rs & 15
                            if s == 0:
                                if r == 15:
                                    if k > 48:
                                        raise ValueError("run past block")
                                    k += 16
                                    continue
                                if r == 0:
                                    break
                                raise ValueError("reserved ac symbol")
                            if s > 10:
                                raise ValueError("ac size past 10")
                            k += r
                            if k > 63:
                                raise ValueError("run past block")
                            coef[ZIGZAG[k]] = extend(bits.bits(s), s) * q[k]
                            k += 1
                        for c in coef:
                            coefficient_bytes += struct.pack("<i", c)
                        block = idct(coef, n, tables)
                        plane = planes[comp["id"]]
                        ox = (mx * bh + bx) * n
                        oy = (my * bv + by) * n
                        for yy in range(n):
                            row = (oy + yy) * plane["w"] + ox
                            plane["px"][row:row + n] = bytes(block[yy * n:(yy + 1) * n])
    frame["planes"] = planes
    frame["hmax"], frame["vmax"], frame["scale"] = hmax, vmax, n
    # The scan is complete: only padding may remain, and EOI follows.
    return bits.finish()


def idct(coef, n, t):
    # rows: for each of the first n rows v, transform the first n coefficients.
    tmp = [0.0] * (n * n)
    for v in range(n):
        for x in range(n):
            s = 0.0
            for u in range(n):
                s += t[u][x] * coef[v * 8 + u]
            tmp[v * n + x] = s
    out = [0] * (n * n)
    for x in range(n):
        for y in range(n):
            s = 0.0
            for v in range(n):
                s += t[v][y] * tmp[v * n + x]
            val = math.floor(s + 128.0 + 0.5)
            out[y * n + x] = 0 if val < 0 else (255 if val > 255 else val)
    return out


def to_rgb(frame):
    planes = frame["planes"]
    comps = frame["comps"]
    hmax, vmax, n = frame["hmax"], frame["vmax"], frame["scale"]
    # Output geometry at this scale: ceil(width * n / 8) by ceil(height * n / 8).
    ow = -(-frame["width"] * n // 8)
    oh = -(-frame["height"] * n // 8)
    out = bytearray(ow * oh * 3)
    if len(comps) == 1:
        p = planes[comps[0]["id"]]
        for y in range(oh):
            for x in range(ow):
                v = p["px"][y * p["w"] + x]
                i = (y * ow + x) * 3
                out[i] = out[i + 1] = out[i + 2] = v
        return ow, oh, bytes(out)
    py, pb, pr = (planes[c["id"]] for c in comps[:3])
    fy = (hmax // comps[0]["h"], vmax // comps[0]["v"])
    fb = (hmax // comps[1]["h"], vmax // comps[1]["v"])
    fr = (hmax // comps[2]["h"], vmax // comps[2]["v"])
    for y in range(oh):
        for x in range(ow):
            yy = py["px"][(y // fy[1]) * py["w"] + x // fy[0]]
            cb = pb["px"][(y // fb[1]) * pb["w"] + x // fb[0]]
            cr = pr["px"][(y // fr[1]) * pr["w"] + x // fr[0]]
            r = math.floor(yy + 1.402 * (cr - 128) + 0.5)
            g = math.floor(yy - 0.344136 * (cb - 128) - 0.714136 * (cr - 128) + 0.5)
            b = math.floor(yy + 1.772 * (cb - 128) + 0.5)
            i = (y * ow + x) * 3
            out[i] = min(255, max(0, r))
            out[i + 1] = min(255, max(0, g))
            out[i + 2] = min(255, max(0, b))
    return ow, oh, bytes(out)


def main():
    args = sys.argv[1:]
    if args and args[0] == "--table":
        for n in (8, 4, 2, 1):
            print("N =", n)
            for row in idct_table(n):
                print("    [" + ", ".join(repr(v) for v in row) + "],")
        return
    ppm = None
    if "--ppm" in args:
        i = args.index("--ppm")
        ppm = args[i + 1]
        del args[i:i + 2]
    path = args[0]
    scale = int(args[1]) if len(args) > 1 else 8
    data = open(path, "rb").read()
    frame, coefficient_bytes = decode(data, scale)
    ow, oh, rgb = to_rgb(frame)
    comps = ",".join("%d:%dx%d" % (c["id"], c["h"], c["v"]) for c in frame["comps"])
    print("frame %dx%d comps %s scale %d/8 -> %dx%d" % (frame["width"], frame["height"], comps, scale, ow, oh))
    print("coefficients: %d blocks, fnv1a64 0x%016x" % (len(coefficient_bytes) // 256, fnv1a64(coefficient_bytes)))
    print("rgb: fnv1a64 0x%016x" % fnv1a64(rgb))
    if ppm:
        with open(ppm, "wb") as f:
            f.write(b"P6\n%d %d\n255\n" % (ow, oh))
            f.write(rgb)


if __name__ == "__main__":
    main()
