#!/usr/bin/env python3
"""Writes canonical_keys.txt: certificate public keys built with Python's
big integers, independent of td-primitives, and whether td-crypto admits
each as a trust anchor or identity key.

Each row is `ALGORITHM FORM VERDICT GROUP KEY`: ALGORITHM is P256, P384,
P521, RSA or Ed25519; VERDICT is `accept` or `refuse`; accepted EC rows sharing a
GROUP are one point, so their canonical forms must be equal; KEY is the
SubjectPublicKeyInfo bit string's contents in hex.

Run: python3 -I canonical_keys.py > canonical_keys.txt
"""

CURVES = {
    "P256": (
        2**256 - 2**224 + 2**192 + 2**96 - 1,
        0x5AC635D8AA3A93E7B3EBBD55769886BC651D06B0CC53B0F63BCE3C3E27D2604B,
        (
            0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
            0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5,
        ),
        32,
    ),
    "P384": (
        2**384 - 2**128 - 2**96 + 2**32 - 1,
        0xB3312FA7E23EE7E4988E056BE3F82D19181D9C6EFE8141120314088F5013875AC656398D8A2ED19D2A85C8EDD3EC2AEF,
        (
            0xAA87CA22BE8B05378EB1C71EF320AD746E1D3B628BA79B9859F741E082542A385502F25DBF55296C3A545E3872760AB7,
            0x3617DE4A96262C6F5D9E98BF9292DC29F8F41DBD289A147CE9DA3113B5F0B8C00A60B1CE1D7E819D7A431D7C90EA0E5F,
        ),
        48,
    ),
    "P521": (
        2**521 - 1,
        0x0051953EB9618E1C9A1F929A21A0B68540EEA2DA725B99B315F3B8B489918EF109E156193951EC7E937B1652C0BD3BB1BF073573DF883D2C34F1EF451FD46B503F00,
        (
            0x00C6858E06B70404E9CD9E3ECB662395B4429C648139053FB521F828AF606B4D3DBAA14B5E77EFE75928FE1DC127A2FFA8DE3348B3C1856A429BF97E7E31C2E5BD66,
            0x011839296A789A3BC0045C8A5FB42C7D1BD998F54449579B446817AFBD17273E662C97EE72995EF42640C550B9013FAD0761353C7086A272C24088BE94769FD16650,
        ),
        66,
    ),
}


def add(curve, a, b):
    p = curve[0]
    if a is None:
        return b
    if b is None:
        return a
    if a[0] == b[0] and (a[1] + b[1]) % p == 0:
        return None
    if a == b:
        slope = (3 * a[0] * a[0] - 3) * pow(2 * a[1], -1, p) % p
    else:
        slope = (b[1] - a[1]) * pow(b[0] - a[0], -1, p) % p
    x = (slope * slope - a[0] - b[0]) % p
    return (x, (slope * (a[0] - x) - a[1]) % p)


def multiply(curve, k):
    result, addend = None, curve[2]
    while k:
        if k & 1:
            result = add(curve, result, addend)
        addend = add(curve, addend, addend)
        k >>= 1
    return result


def on_curve(curve, x, y):
    p, b = curve[0], curve[1]
    return (y * y - (x * x * x - 3 * x + b)) % p == 0


def is_square(curve, value):
    p = curve[0]
    return value % p == 0 or pow(value, (p - 1) // 2, p) == 1


def rows():
    out = []
    for name, curve in CURVES.items():
        p, b, _, size = curve
        enc = lambda v: v.to_bytes(size, "big")
        for group, k in enumerate([7, 0x1234567]):
            x, y = multiply(curve, k)
            assert on_curve(curve, x, y)
            tag = f"{name}-{group}"
            out.append((name, "uncompressed", "accept", tag, b"\x04" + enc(x) + enc(y)))
            out.append((name, "compressed", "accept", tag, bytes([2 | (y & 1)]) + enc(x)))
            out.append((name, "hybrid", "refuse", "-", bytes([6 | (y & 1)]) + enc(x) + enc(y)))
            out.append((name, "other-y", "accept", f"{tag}-neg", bytes([2 | (~y & 1)]) + enc(x)))
        x, y = multiply(curve, 7)
        out.append((name, "off-curve", "refuse", "-", b"\x04" + enc(x) + enc((y + 1) % p)))
        out.append((name, "coordinate-p", "refuse", "-", b"\x04" + enc(p) + enc(y)))
        # The smallest x whose right-hand side has no square root.
        bad = next(x for x in range(1, 1000) if not is_square(curve, x * x * x - 3 * x + b))
        out.append((name, "compressed-off-curve", "refuse", "-", b"\x02" + enc(bad)))
        out.append((name, "infinity", "refuse", "-", b"\x00"))
        out.append((name, "truncated", "refuse", "-", b"\x04" + enc(x) + enc(y)[:-1]))
    return out


def der_integer(value):
    body = value.to_bytes((value.bit_length() + 8) // 8, "big")
    return der(0x02, body)


def der(tag, body):
    if len(body) < 128:
        length = bytes([len(body)])
    else:
        raw = len(body).to_bytes((len(body).bit_length() + 7) // 8, "big")
        length = bytes([0x80 | len(raw)]) + raw
    return bytes([tag]) + length + body


def rsa(modulus, exponent):
    return der(0x30, der_integer(modulus) + der_integer(exponent))


def rsa_rows():
    # Odd moduli of an exact bit length; admission checks shape, not primes.
    n = lambda bits: 2 ** (bits - 1) + 2**64 + 1
    return [
        ("RSA", "2048-bit", "accept", "-", rsa(n(2048), 65537)),
        ("RSA", "8192-bit", "accept", "-", rsa(n(8192), 65537)),
        ("RSA", "e-3", "accept", "-", rsa(n(2048), 3)),
        ("RSA", "e-33-bit", "accept", "-", rsa(n(2048), 2**32 + 1)),
        ("RSA", "e-1", "refuse", "-", rsa(n(2048), 1)),
        ("RSA", "e-even", "refuse", "-", rsa(n(2048), 65536)),
        ("RSA", "e-34-bit", "refuse", "-", rsa(n(2048), 2**33 + 1)),
        ("RSA", "even-modulus", "refuse", "-", rsa(n(2048) + 1, 65537)),
        ("RSA", "2047-bit", "refuse", "-", rsa(n(2047), 65537)),
        ("RSA", "8193-bit", "refuse", "-", rsa(n(8193), 65537)),
    ]


ED_P = 2**255 - 19
ED_D = -121665 * pow(121666, -1, ED_P) % ED_P
ED_L = 2**252 + 27742317777372353535851937790883648493
ED_IDENTITY = (0, 1)


def ed_add(a, b):
    # Complete for edwards25519: a = -1 and d is not a square.
    p = ED_P
    t = ED_D * a[0] * b[0] * a[1] * b[1] % p
    x = (a[0] * b[1] + b[0] * a[1]) * pow(1 + t, -1, p) % p
    y = (a[1] * b[1] + a[0] * b[0]) * pow(1 - t, -1, p) % p
    return (x, y)


def ed_multiply(point, k):
    result = ED_IDENTITY
    while k:
        if k & 1:
            result = ed_add(result, point)
        point = ed_add(point, point)
        k >>= 1
    return result


def ed_x(y):
    """The even x of y, or None when y is on no point."""
    p = ED_P
    square = (y * y - 1) * pow(ED_D * y * y + 1, -1, p) % p
    x = pow(square, (p + 3) // 8, p)
    if x * x % p != square:
        x = x * pow(2, (p - 1) // 4, p) % p
    if x * x % p != square:
        return None
    return p - x if x & 1 else x


def ed_encode(point, sign=None):
    x, y = point
    bit = (x & 1) if sign is None else sign
    return (y | bit << 255).to_bytes(32, "little")


def ed_order(point):
    for order in [1, 2, 4, 8]:
        if ed_multiply(point, order) == ED_IDENTITY:
            return order
    return None


def ed25519_rows():
    base = (ed_x(4 * pow(5, -1, ED_P) % ED_P), 4 * pow(5, -1, ED_P) % ED_P)
    assert ed_multiply(base, ED_L) == ED_IDENTITY
    # A point of order 8: the torsion part of the first y on the curve that has one.
    torsion = None
    for y in range(2, 100):
        x = ed_x(y)
        if x is not None and ed_order(ed_multiply((x, y), ED_L)) == 8:
            torsion = ed_multiply((x, y), ED_L)
            break
    small = [ed_multiply(torsion, i) for i in range(8)]
    assert len(set(small)) == 8
    out = []
    for i, point in enumerate(small):
        out.append(("Ed25519", f"order-{ed_order(point)}-{i}", "refuse", "-", ed_encode(point)))
    # The six other spellings of those points, which decoding refuses anyway:
    # x = 0 with its sign bit set, and y at or past p that reduces to one.
    for y, sign, form in [
        (1, 1, "identity-negative-zero"),
        (ED_P - 1, 1, "order-2-negative-zero"),
        (ED_P, 0, "y-p"),
        (ED_P, 1, "y-p-signed"),
        (ED_P + 1, 0, "y-p-plus-1"),
        (ED_P + 1, 1, "y-p-plus-1-signed"),
    ]:
        out.append(("Ed25519", form, "refuse", "-", (y | sign << 255).to_bytes(32, "little")))
    key = ed_multiply(base, 0x1234567)
    out.append(("Ed25519", "prime-order", "accept", "-", ed_encode(key)))
    # A key with a torsion part is not small: nobody forges for it who does not
    # hold its discrete log, so admission takes it.
    mixed = ed_add(key, small[1])
    assert ed_order(mixed) is None
    out.append(("Ed25519", "mixed-order", "accept", "-", ed_encode(mixed)))
    # Admission compares the key and does not decode it; verification refuses
    # a key on no point.
    off = next(y for y in range(2, 100) if ed_x(y) is None)
    out.append(("Ed25519", "no-point", "accept", "-", off.to_bytes(32, "little")))
    out.append(("Ed25519", "truncated", "refuse", "-", ed_encode(key)[:31]))
    return out


for row in rows() + rsa_rows() + ed25519_rows():
    print(" ".join(row[:4]), row[4].hex())
