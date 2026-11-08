"""Edge values no corpus reaches, derived here with Python's big integers so that
tests/vectors.rs has an answer this crate did not compute.

    python3 -I edge_vectors.py > edge_vectors.txt

Four groups:

  p256-small-x, p256-small-y: on-curve points whose x or y is small enough that
  the same residue plus p still fits in 256 bits, which only a range check can
  refuse. They come from a search over small coordinates, with the cubic's root
  found by polynomial gcd. p384-small-x and p384-small-y are the same on P-384.

  p256-sha384: an ECDSA signature over a digest longer than the order, made here
  by textbook affine arithmetic, a pairing no Wycheproof file holds.

  rsa2049-*: an RSA key of 2049 bits, the width where a PSS encoded message is one
  byte shorter than the modulus. One valid signature, one whose recovered value
  is otherwise valid but carries a nonzero leading byte, one with a flipped bit
  in H, and two whose salts are a byte short and a byte long, which only a verifier
  fixing the salt at the hash's length refuses. The key is generated from a fixed seed
  and its private half is discarded: it signs nothing else.

  rsa8192-*: the same at the top of the accepted range, where a public certificate
  to test against is hard to come by, with a PKCS #1 and a PSS signature.

  p521-*: P-521's small coordinates as above, signatures over SHA-256 and SHA-384,
  pairings Wycheproof holds only with SHA-512, and one over a 66-byte digest, whose
  528 bits pass the order's 521 so that only the leftmost count.
"""

import hashlib
import random

P = 2**256 - 2**224 + 2**192 + 2**96 - 1
B = 0x5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b


def poly_mod_cubic(a, f):
    """Reduce a polynomial (low first) by a monic cubic, over F_p."""
    a = list(a)
    while len(a) > 3:
        lead = a.pop()
        for i in range(3):
            a[len(a) - 3 + i] = (a[len(a) - 3 + i] - lead * f[i]) % P
    return a + [0] * (3 - len(a))


def poly_mul(a, b, f):
    out = [0] * (len(a) + len(b) - 1)
    for i, x in enumerate(a):
        for j, y in enumerate(b):
            out[i + j] = (out[i + j] + x * y) % P
    return poly_mod_cubic(out, f)


def power_of_x(shift, exponent, f):
    result, base = [1, 0, 0], [shift % P, 1, 0]
    while exponent:
        if exponent & 1:
            result = poly_mul(result, base, f)
        base = poly_mul(base, base, f)
        exponent >>= 1
    return result


def poly_gcd(a, b):
    def trim(p):
        while p and p[-1] == 0:
            p = p[:-1]
        return p

    a, b = trim(list(a)), trim(list(b))
    while b:
        inverse = pow(b[-1], P - 2, P)
        while len(a) >= len(b):
            factor = a[-1] * inverse % P
            offset = len(a) - len(b)
            for i, c in enumerate(b):
                a[offset + i] = (a[offset + i] - factor * c) % P
            a = trim(a[:-1]) if a[-1] == 0 else a
        a, b = b, trim(a)
    return a


def linear_root_of_cubic(f):
    """A root in F_p of x^3 + f[2] x^2 + f[1] x + f[0], or None."""
    rng = random.Random(5)
    full = f + [1]
    frobenius = power_of_x(0, P, f)
    split = poly_gcd(full, [(frobenius[0]) % P, (frobenius[1] - 1) % P, frobenius[2]])
    if len(split) <= 1:
        return None
    while len(split) > 2:
        shift = rng.randrange(P)
        half = power_of_x(shift, (P - 1) // 2, f)
        half[0] = (half[0] - 1) % P
        factor = poly_gcd(split, half)
        if 1 < len(factor) < len(split):
            split = factor
    return -split[0] * pow(split[1], P - 2, P) % P


def on_curve(x, y):
    return (y * y - (x * x * x - 3 * x + B)) % P == 0


def hex_of(value, length):
    return value.to_bytes(length, "big").hex()


def small_coordinate_rows(name, width):
    # A small x: try x = 1, 2, ... until the cubic's value is a square.
    x = 1
    while True:
        right = (x * x * x - 3 * x + B) % P
        # p = 3 mod 4, so this is the root when one exists.
        y = pow(right, (P + 1) // 4, P)
        if y * y % P == right:
            break
        x += 1
    assert on_curve(x, y) and x + P < 2 ** (8 * width)
    print(f"{name}-small-x {hex_of(x, width)} {hex_of(y, width)} {hex_of(x + P, width)}")

    # A small y: solve x^3 - 3x + (b - y^2) for x.
    y = 1
    while True:
        root = linear_root_of_cubic([(B - y * y) % P, P - 3, 0])
        if root is not None:
            break
        y += 1
    assert on_curve(root, y) and y + P < 2 ** (8 * width)
    print(f"{name}-small-y {hex_of(root, width)} {hex_of(y, width)} {hex_of(y + P, width)}")


def p384_small_coordinate_rows():
    global P, B
    kept = P, B
    P = 2**384 - 2**128 - 2**96 + 2**32 - 1
    B = 0xb3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef
    gx = 0xaa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7
    gy = 0x3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f
    assert on_curve(gx, gy)
    small_coordinate_rows("p384", 48)
    P, B = kept


def textbook_signature(p, n, g, width, d, k, e):
    """An ECDSA signature by d with nonce k over the reduced digest e, by affine
    arithmetic on y^2 = x^3 - 3x + b mod p: the uncompressed key and DER."""

    def add(a, b):
        if a is None:
            return b
        if b is None:
            return a
        if a[0] == b[0]:
            if (a[1] + b[1]) % p == 0:
                return None
            slope = (3 * a[0] * a[0] - 3) * pow(2 * a[1], p - 2, p) % p
        else:
            slope = (b[1] - a[1]) * pow(b[0] - a[0], p - 2, p) % p
        x = (slope * slope - a[0] - b[0]) % p
        return x, (slope * (a[0] - x) - a[1]) % p

    def times(scalar, point):
        result = None
        while scalar:
            if scalar & 1:
                result = add(result, point)
            point = add(point, point)
            scalar >>= 1
        return result

    public = times(d, g)
    r = times(k, g)[0] % n
    s = pow(k, n - 2, n) * (e + r * d) % n
    assert r and s

    def length(size):
        return bytes([size]) if size < 0x80 else bytes([0x81, size])

    def integer(value):
        body = value.to_bytes(width, "big").lstrip(b"\0")
        if body[0] & 0x80:
            body = b"\0" + body
        return b"\x02" + length(len(body)) + body

    body = integer(r) + integer(s)
    der = b"\x30" + length(len(body)) + body
    point = b"\x04" + public[0].to_bytes(width, "big") + public[1].to_bytes(width, "big")
    return point.hex(), der.hex()


def p256_sha384_row():
    n = 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551
    gx = 0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296
    gy = 0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5

    # Fixed values: this key and nonce sign this one message and nothing else.
    d = int.from_bytes(hashlib.sha256(b"td edge p256 key").digest(), "big") % n
    k = int.from_bytes(hashlib.sha256(b"td edge p256 nonce").digest(), "big") % n
    message = b"td-primitives P-256 with SHA-384"
    # FIPS 186-5: the leftmost 256 bits of the 384-bit digest.
    e = int.from_bytes(hashlib.sha384(message).digest()[:32], "big") % n
    point, der = textbook_signature(P, n, (gx, gy), 32, d, k, e)
    print(f"p256-sha384 {point} {message.hex()} {der}")


def p521_rows():
    global P, B
    kept = P, B
    P = 2**521 - 1
    B = int(
        "0051953eb9618e1c9a1f929a21a0b68540eea2da725b99b315f3b8b489918ef109e1"
        "56193951ec7e937b1652c0bd3bb1bf073573df883d2c34f1ef451fd46b503f00",
        16,
    )
    n = int(
        "01ff" + "ffffffff" * 7 + "fffffffa"
        "51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e91386409",
        16,
    )
    gx = int(
        "00c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d3dba"
        "a14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5bd66",
        16,
    )
    gy = int(
        "011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e662c"
        "97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd16650",
        16,
    )
    assert on_curve(gx, gy) and n.bit_length() == 521
    # n G is the identity, which a signature by n - 1 with nonce 1 shows: its
    # key is -G, whose x is G's.
    point, _ = textbook_signature(P, n, (gx, gy), 66, n - 1, 1, 1)
    assert point == "04" + hex_of(gx, 66) + hex_of(P - gy, 66)
    small_coordinate_rows("p521", 66)

    def scalar(label):
        wide = hashlib.sha512(label + b" 1").digest() + hashlib.sha512(label + b" 2").digest()
        return int.from_bytes(wide, "big") % n

    d = scalar(b"td edge p521 key")
    for name, digest in (("sha256", hashlib.sha256), ("sha384", hashlib.sha384)):
        message = f"td-primitives P-521 with {name.upper()}".encode()
        # Shorter than the order: the whole digest is the integer.
        e = int.from_bytes(digest(message).digest(), "big")
        point, der = textbook_signature(P, n, (gx, gy), 66, d, scalar(name.encode()), e)
        print(f"p521-{name} {point} {message.hex()} {der}")

    # 66 bytes are 528 bits, past the order's 521: FIPS 186-5 keeps the leftmost.
    digest = hashlib.sha512(b"td edge p521 long digest").digest() + bytes([0xa5, 0x7f])
    e = (int.from_bytes(digest, "big") >> 7) % n
    point, der = textbook_signature(P, n, (gx, gy), 66, d, scalar(b"long"), e)
    print(f"p521-digest66 {point} {digest.hex()} {der}")
    P, B = kept


def is_prime(n, rng):
    if n % 2 == 0:
        return n == 2
    d, r = n - 1, 0
    while d % 2 == 0:
        d //= 2
        r += 1
    for _ in range(40):
        x = pow(rng.randrange(2, n - 1), d, n)
        if x in (1, n - 1):
            continue
        for _ in range(r - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def prime_of(bits, rng):
    while True:
        # 1.25 to 1.5 of 2^(bits-1), so the product can sit high in 2049 bits.
        candidate = rng.getrandbits(bits - 3) | (0b101 << (bits - 3)) | 1
        if is_prime(candidate, rng):
            return candidate


def mgf1(seed, length):
    out = b""
    counter = 0
    while len(out) < length:
        out += hashlib.sha256(seed + counter.to_bytes(4, "big")).digest()
        counter += 1
    return out[:length]


def pss_encode(message, em_bits, salt):
    """RFC 8017 9.1.1 for SHA-256, with whatever salt the caller gives."""
    em_len = (em_bits + 7) // 8
    m_hash = hashlib.sha256(message).digest()
    h = hashlib.sha256(bytes(8) + m_hash + salt).digest()
    db = bytes(em_len - len(salt) - 32 - 2) + b"\x01" + salt
    masked = bytes(a ^ b for a, b in zip(db, mgf1(h, len(db))))
    masked = bytes([masked[0] & (0xff >> (8 * em_len - em_bits))]) + masked[1:]
    return masked + h + b"\xbc"


def rsa_rows():
    rng = random.Random(2049)
    e = 65537
    while True:
        p, q = prime_of(1025, rng), prime_of(1025, rng)
        n = p * q
        if n.bit_length() != 2049 or p == q:
            continue
        phi = (p - 1) * (q - 1)
        if phi % e == 0:
            continue
        # Room above the encoded message for a nonzero leading byte.
        if n <= (1 << 2048) + (1 << 2047):
            continue
        break
    d = pow(e, -1, phi)
    message = b"td-primitives 2049-bit PSS edge"
    em = pss_encode(message, 2048, bytes(range(32)))
    assert len(em) == 256
    value = int.from_bytes(em, "big")
    assert value < 1 << 2048

    def signature(of):
        assert of < n
        return hex_of(pow(of, d, n), 257)

    print(f"rsa2049-key {hex_of(n, 257)} {e:06x}")
    print(f"rsa2049-message {message.hex()}")
    print(f"rsa2049-valid {signature(value)}")
    print(f"rsa2049-lead-byte {signature(value | (1 << 2048))}")
    print(f"rsa2049-flipped {signature(value ^ (1 << 10))}")
    for name, salt_len in (("short-salt", 31), ("long-salt", 33)):
        wrong = pss_encode(message, 2048, bytes(range(salt_len)))
        print(f"rsa2049-{name} {signature(int.from_bytes(wrong, 'big'))}")


def top_of_range_rows():
    rng = random.Random(8192)
    e = 65537
    while True:
        p, q = prime_of(4096, rng), prime_of(4096, rng)
        n = p * q
        phi = (p - 1) * (q - 1)
        if n.bit_length() == 8191:
            # 1.25 to 1.5 squared of 2^8190: push into the top bit of 8192.
            q = prime_of(4097, rng)
            n = p * q
            phi = (p - 1) * (q - 1)
        if n.bit_length() == 8192 and phi % e != 0:
            break
    d = pow(e, -1, phi)
    message = b"td-primitives 8192-bit edge"
    digest_info = bytes.fromhex("3031300d060960864801650304020105000420")
    tail = digest_info + hashlib.sha256(message).digest()
    pkcs1 = b"\x00\x01" + b"\xff" * (1024 - len(tail) - 3) + b"\x00" + tail
    pss = pss_encode(message, 8191, bytes(range(32)))
    assert len(pkcs1) == 1024 and len(pss) == 1024

    print(f"rsa8192-key {hex_of(n, 1024)} {e:06x}")
    print(f"rsa8192-message {message.hex()}")
    print(f"rsa8192-pkcs1 {hex_of(pow(int.from_bytes(pkcs1, 'big'), d, n), 1024)}")
    print(f"rsa8192-pss {hex_of(pow(int.from_bytes(pss, 'big'), d, n), 1024)}")


def main():
    small_coordinate_rows("p256", 32)
    p384_small_coordinate_rows()
    p256_sha384_row()
    rsa_rows()
    top_of_range_rows()
    p521_rows()


if __name__ == "__main__":
    main()
