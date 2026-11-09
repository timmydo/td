#!/usr/bin/env python3
"""Writes handshake_keys.txt: the private keys handshake_peer_tests.rs signs
TLS handshakes with, and their public halves, from Python's big integers and
hashlib, independent of td-primitives. The test recomputes each public half
with its own arithmetic and requires these.

Rows, hex, big-endian unless noted:
  RSA n e d p q dp dq qinv     a 2048-bit key from two seeded 1024-bit primes
  P256|P384|P521 d x y         a scalar and its point
  ED25519 seed public          RFC 8032 encodings (little-endian)

Run: python3 -I handshake_keys.py > handshake_keys.txt
These keys sign test handshakes and nothing else.
"""

import hashlib
import random

CURVES = {
    "P256": (
        2**256 - 2**224 + 2**192 + 2**96 - 1,
        0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551,
        0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
        0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5,
        32,
    ),
    "P384": (
        2**384 - 2**128 - 2**96 + 2**32 - 1,
        0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFC7634D81F4372DDF581A0DB248B0A77AECEC196ACCC52973,
        0xAA87CA22BE8B05378EB1C71EF320AD746E1D3B628BA79B9859F741E082542A385502F25DBF55296C3A545E3872760AB7,
        0x3617DE4A96262C6F5D9E98BF9292DC29F8F41DBD289A147CE9DA3113B5F0B8C00A60B1CE1D7E819D7A431D7C90EA0E5F,
        48,
    ),
    "P521": (
        2**521 - 1,
        0x01FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA51868783BF2F966B7FCC0148F709A5D03BB5C9B8899C47AEBB6FB71E91386409,
        0x00C6858E06B70404E9CD9E3ECB662395B4429C648139053FB521F828AF606B4D3DBAA14B5E77EFE75928FE1DC127A2FFA8DE3348B3C1856A429BF97E7E31C2E5BD66,
        0x011839296A789A3BC0045C8A5FB42C7D1BD998F54449579B446817AFBD17273E662C97EE72995EF42640C550B9013FAD0761353C7086A272C24088BE94769FD16650,
        66,
    ),
}


def ec_multiply(curve, k):
    p, _, gx, gy, _ = curve
    result, addend = None, (gx, gy)
    while k:
        if k & 1:
            result = ec_add(p, result, addend)
        addend = ec_add(p, addend, addend)
        k >>= 1
    return result


def ec_add(p, a, b):
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


# Miller-Rabin witnesses: the first 40 primes, so the keys depend only on
# the candidates getrandbits draws.
WITNESSES = (
    2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71,
    73, 79, 83, 89, 97, 101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151,
    157, 163, 167, 173,
)


def probable_prime(n):
    if n % 2 == 0:
        return False
    for small in (3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37):
        if n % small == 0:
            return n == small
    d, s = n - 1, 0
    while d % 2 == 0:
        d, s = d // 2, s + 1
    for a in WITNESSES:
        x = pow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(s - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def prime(bits, rng, e):
    while True:
        candidate = rng.getrandbits(bits) | (3 << (bits - 2)) | 1
        if (candidate - 1) % e and probable_prime(candidate):
            return candidate


ED_P = 2**255 - 19
ED_L = 2**252 + 27742317777372353535851937790883648493
ED_D = -121665 * pow(121666, -1, ED_P) % ED_P
ED_BY = 4 * pow(5, -1, ED_P) % ED_P


def ed_x(y, sign):
    xx = (y * y - 1) * pow(ED_D * y * y + 1, -1, ED_P) % ED_P
    x = pow(xx, (ED_P + 3) // 8, ED_P)
    if (x * x - xx) % ED_P:
        x = x * pow(2, (ED_P - 1) // 4, ED_P) % ED_P
    if x % 2 != sign:
        x = ED_P - x
    return x


def ed_add(a, b):
    (x1, y1), (x2, y2) = a, b
    t = ED_D * x1 * x2 * y1 * y2 % ED_P
    x = (x1 * y2 + x2 * y1) * pow(1 + t, -1, ED_P) % ED_P
    y = (y1 * y2 + x1 * x2) * pow(1 - t, -1, ED_P) % ED_P
    return (x, y)


def ed_multiply(k, point):
    result = (0, 1)
    while k:
        if k & 1:
            result = ed_add(result, point)
        point = ed_add(point, point)
        k >>= 1
    return result


def ed_public(seed):
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    x, y = ed_multiply(a, (ed_x(ED_BY, 0), ED_BY))
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


def hexed(value, size):
    return value.to_bytes(size, "big").hex()


def main():
    rng = random.Random(0x7464_6873)  # "tdhs"
    e = 65537
    while True:
        p, q = prime(1024, rng, e), prime(1024, rng, e)
        n = p * q
        if p != q and n.bit_length() == 2048:
            break
    if p < q:
        p, q = q, p
    d = pow(e, -1, (p - 1) * (q - 1))
    row = [n, e, d, p, q, d % (p - 1), d % (q - 1), pow(q, -1, p)]
    sizes = [256, 3, 256, 128, 128, 128, 128, 128]
    print("RSA", " ".join(hexed(v, s) for v, s in zip(row, sizes)))
    for name, curve in CURVES.items():
        size, order = curve[4], curve[1]
        d = int.from_bytes(hashlib.sha512(b"td handshake " + name.encode()).digest(), "big")
        d = d % (order - 1) + 1
        x, y = ec_multiply(curve, d)
        print(name, hexed(d, size), hexed(x, size), hexed(y, size))
    seed = hashlib.sha256(b"td handshake ED25519").digest()
    print("ED25519", seed.hex(), ed_public(seed).hex())


main()
