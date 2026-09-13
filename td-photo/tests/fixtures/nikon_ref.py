"""Independent reference decoder for the first ROWS rows of a Nikon
lossless-compressed NEF, transcribed from dcraw's nikon_load_raw. Prints
the byte extent of the bitstream those rows consume, an FNV-1a-64 hash over
the decoded samples as little-endian u16 bytes, and the first samples of
each row, so the Rust decoder can be pinned against something that is not
itself."""
import struct
import sys

path = sys.argv[1]
ROWS = int(sys.argv[2]) if len(sys.argv) > 2 else 2
d = open(path, 'rb').read()
E = '<'


def u16(o):
    return struct.unpack_from(E + 'H', d, o)[0]


def u32(o):
    return struct.unpack_from(E + 'I', d, o)[0]


TYPES = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8, 13: 4}


def entries(off, base=0):
    n = u16(off)
    out = {}
    for i in range(n):
        e = off + 2 + i * 12
        tag = u16(e)
        typ = u16(e + 2)
        cnt = u32(e + 4)
        sz = TYPES.get(typ, 1) * cnt
        vo = e + 8 if sz <= 4 else u32(e + 8) + base
        out[tag] = (typ, cnt, vo)
    return out


ifd0 = entries(u32(4))
sub1 = entries(u32(ifd0[330][2] + 4))
width = u32(sub1[256][2])
height = u32(sub1[257][2])
bps = u16(sub1[258][2])
data_offset = u32(sub1[273][2])
data_len = u32(sub1[279][2])
exif = entries(u32(ifd0[34665][2]))
mn_off = exif[37500][2]
base = mn_off + 10
nk = entries(u32(base + 4) + base, base=base)
meta = nk[0x96][2]

trees = [
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 5, 4, 3, 6, 2, 7, 1, 0, 8, 9, 11, 10, 12],
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0x39, 0x5a, 0x38, 0x27, 0x16, 5, 4, 3, 2, 1, 0, 11, 12, 12],
    [0, 1, 4, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 4, 6, 3, 7, 2, 8, 1, 9, 0, 10, 11, 12],
    [0, 1, 4, 3, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 5, 6, 4, 7, 8, 3, 9, 2, 1, 0, 10, 11, 12, 13, 14],
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 8, 0x5c, 0x4b, 0x3a, 0x29, 7, 6, 5, 4, 3, 2, 1, 0, 13, 14],
    [0, 1, 4, 2, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 7, 6, 8, 5, 9, 4, 10, 3, 11, 12, 2, 0, 1, 13, 14],
]

ver0 = d[meta]
ver1 = d[meta + 1]
p = meta + 2
if ver0 == 0x49 or ver1 == 0x58:
    p += 2110
tree = 0
if ver0 == 0x46:
    tree = 2
if bps == 14:
    tree += 3
vpred = [[u16(p), u16(p + 2)], [u16(p + 4), u16(p + 6)]]
p += 8
maxv = (1 << bps) & 0x7fff
csize = u16(p)
p += 2
curve = list(range(1 << 16))
split = 0
step = maxv // (csize - 1) if csize > 1 else 0
if ver0 == 0x44 and ver1 == 0x20 and step > 0:
    raise SystemExit('lossy type 2 not handled by this reference')
elif ver0 != 0x46 and csize <= 0x4001:
    curve = [u16(p + 2 * i) for i in range(csize)]
    maxv = csize
while curve[maxv - 2] == curve[maxv - 1]:
    maxv -= 1
print('ver', hex(ver0), hex(ver1), 'tree', tree, 'vpred', vpred, 'csize', csize, 'max', maxv,
      'size', width, height, 'data', data_offset, data_len)

# make_decoder_ref
t = trees[tree]
count = [0] + t[:16]
mx = 16
while mx and not count[mx]:
    mx -= 1
huff = [0] * (1 << mx)
h = 0
src = 16
for ln in range(1, mx + 1):
    for i in range(count[ln]):
        sym = t[src]
        src += 1
        for j in range(1 << (mx - ln)):
            if h < (1 << mx):
                huff[h] = (ln << 8) | sym
                h += 1

# bit reader (MSB first, no FF stuffing)
pos = data_offset
end = data_offset + data_len
bitbuf = 0
vbits = 0


def fill(n):
    global bitbuf, vbits, pos
    while vbits < n:
        c = d[pos] if pos < end else 0
        pos += 1
        bitbuf = ((bitbuf << 8) | c) & 0xFFFFFFFFFFFFFFFF
        vbits += 8


def getbits(n):
    global bitbuf, vbits
    if n == 0:
        return 0
    fill(n)
    c = (bitbuf >> (vbits - n)) & ((1 << n) - 1)
    vbits -= n
    return c


def gethuff():
    global vbits
    fill(mx)
    c = (bitbuf >> (vbits - mx)) & ((1 << mx) - 1)
    e = huff[c]
    vbits -= e >> 8
    return e & 0xFF


FNV_OFFSET = 0xcbf29ce484222325
FNV_PRIME = 0x100000001b3
hsh = FNV_OFFSET
hpred = [0, 0]
out_first = []
errors = 0
for row in range(ROWS):
    first = []
    for col in range(width):
        i = gethuff()
        ln = i & 15
        shl = i >> 4
        if ln == 0:
            diff = 0
        else:
            diff = ((getbits(ln - shl) << 1) + 1) << shl >> 1
            if (diff & (1 << (ln - 1))) == 0:
                diff -= (1 << ln) - (0 if shl else 1)
        if col < 2:
            vpred[row & 1][col] = (vpred[row & 1][col] + diff) & 0xFFFF
            hpred[col] = vpred[row & 1][col]
        else:
            hpred[col & 1] = (hpred[col & 1] + diff) & 0xFFFF
        v = hpred[col & 1]
        if v >= maxv:
            errors += 1
        vv = v if v < 0x8000 else 0
        vv = min(vv, 0x3fff)
        s = curve[vv]
        if col < 8:
            first.append(s)
        for b in (s & 0xFF, s >> 8):
            hsh ^= b
            hsh = (hsh * FNV_PRIME) & 0xFFFFFFFFFFFFFFFF
    out_first.append(first)
consumed_bits = (pos - data_offset) * 8 - vbits
print('rows', ROWS, 'consumed_bits', consumed_bits, 'consumed_bytes', (consumed_bits + 7) // 8,
      'errors', errors)
print('fnv1a64 0x%016x' % hsh)
for r, f in enumerate(out_first):
    print('row', r, 'first', f)
