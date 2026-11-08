"""Signatures td-tls-provider's and td-net's tests check against, made here by
textbook big-integer arithmetic, so that their tables meet answers they did not
compute.

    python3 -I signature_answers.py > signature_answers.txt

One row for each distinct check in td-tls-provider/src/lib.rs, as `VERIFIER public-key
message signature` in hex, the key as a TLS peer presents it: an EC point, a raw
Ed25519 key or a DER RSAPublicKey. td-crypto's differential tests read the ECDSA, Ed25519
and RSA rows too: four of the curve and hash pairs are ones AWS-LC will not sign with.

`REFUSED_` rows are signatures no verifier may take: ECDSA r or s of 0 or n or more, or
not strict DER; RSA moduli outside 2048 to 8192 bits and exponents that are 1, even or
over 33 bits; Ed25519 S of L or more. A second RSA_PKCS1_SHA256 row has the largest
exponent taken, 33 bits. SMALL_ORDER_ED25519 is a signature under the identity key,
which verifies for any message whose R is rB and S is r.

One OpenPGP row for net/src/openpgp.rs, `PGP_SHORT_MPI n e data subpackets value`: a
v4 binary signature whose value has a leading zero octet, which its MPI drops, as
about one real signature in 256 does.

Keys come from fixed seeds and their private halves are discarded: they sign these
messages and nothing else.
"""

import hashlib
import random
from math import gcd

P256 = {
    "p": 2**256 - 2**224 + 2**192 + 2**96 - 1,
    "n": 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551,
    "gx": 0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296,
    "gy": 0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5,
    "len": 32,
}
P384 = {
    "p": 2**384 - 2**128 - 2**96 + 2**32 - 1,
    "n": 0xffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973,
    "gx": 0xaa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7,
    "gy": 0x3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f,
    "len": 48,
}
P521 = {
    "p": 2**521 - 1,
    "n": 0x01fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e91386409,
    "gx": 0x00c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d3dbaa14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5bd66,
    "gy": 0x011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e662c97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd16650,
    "len": 66,
}
INFO = {
    "sha256": bytes.fromhex("3031300d060960864801650304020105000420"),
    "sha384": bytes.fromhex("3041300d060960864801650304020205000430"),
    "sha512": bytes.fromhex("3051300d060960864801650304020305000440"),
}


def times(scalar, curve):
    p = curve["p"]

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

    point, result = (curve["gx"], curve["gy"]), None
    while scalar:
        if scalar & 1:
            result = add(result, point)
        point = add(point, point)
        scalar >>= 1
    return result


def der(tag, body):
    if len(body) < 0x80:
        length = bytes([len(body)])
    elif len(body) < 0x100:
        length = bytes([0x81, len(body)])
    else:
        length = bytes([0x82, len(body) >> 8, len(body) & 0xff])
    return bytes([tag]) + length + body


def integer(value):
    body = value.to_bytes(value.bit_length() // 8 + 1, "big")
    return der(2, body)


def ecdsa_row(name, curve, hash_name, label):
    n, size = curve["n"], curve["len"]
    seed = hashlib.sha512(b"td tls verifier key " + label).digest()
    d = int.from_bytes(seed, "big") % n
    seed = hashlib.sha512(b"td tls verifier nonce " + name.encode()).digest()
    k = int.from_bytes(seed, "big") % n
    message = b"td verifier table " + name.encode()
    digest = hashlib.new(hash_name, message).digest()
    # FIPS 186-5: the leftmost bits of the digest, as many as the order has.
    e = int.from_bytes(digest[:size], "big") % n
    public = times(d, curve)
    r = times(k, curve)[0] % n
    s = pow(k, n - 2, n) * (e + r * d) % n
    assert r and s
    point = b"\x04" + public[0].to_bytes(size, "big") + public[1].to_bytes(size, "big")
    signature = der(0x30, integer(r) + integer(s))
    print(f"{name} {point.hex()} {message.hex()} {signature.hex()}")
    return point, message, r, s


def ecdsa_refusals(point, message, r, s):
    # Each a signature no verifier may take, with its row's key and message.
    n = P256["n"]
    padded = der(2, b"\x00" + integer(r)[2:])
    body = integer(r) + integer(s)
    for name, signature in [
        ("R_ZERO", der(0x30, integer(0) + integer(s))),
        ("S_ZERO", der(0x30, integer(r) + integer(0))),
        ("R_IS_N", der(0x30, integer(n) + integer(s))),
        ("R_PLUS_N", der(0x30, integer(r + n) + integer(s))),
        ("S_PLUS_N", der(0x30, integer(r) + integer(s + n))),
        ("R_NOT_MINIMAL", der(0x30, padded + integer(s))),
        ("LENGTH_NOT_MINIMAL", bytes([0x30, 0x81, len(body)]) + body),
        ("TRAILING_BYTE", der(0x30, body) + b"\x00"),
    ]:
        row = f"REFUSED_ECDSA_P256_SHA256_{name}"
        print(f"{row} {point.hex()} {message.hex()} {signature.hex()}")


def pkcs1_signature(n, d, message):
    size = (n.bit_length() + 7) // 8
    tail = INFO["sha256"] + hashlib.sha256(message).digest()
    em = b"\x00\x01" + b"\xff" * (size - len(tail) - 3) + b"\x00" + tail
    return pow(int.from_bytes(em, "big"), d, n).to_bytes(size, "big"), em


def multi_prime(count, rng, e):
    # Squarefree n of 512-bit primes: verification reads only n and e.
    while True:
        primes = [prime_of(512, rng) for _ in range(count)]
        n, lam = 1, 1
        for p in primes:
            n *= p
            lam = lam * (p - 1) // gcd(lam, p - 1)
        if len(set(primes)) == count and gcd(e, lam) == 1:
            return n, pow(e, -1, lam)


def rsa_edges(n, p, q):
    # Moduli outside 2048..8192 bits and exponents outside odd 3..2^33.
    rng = random.Random(8192)
    message = b"td verifier table RSA_PKCS1_SHA256 edge"
    phi = (p - 1) * (q - 1)
    rows = []
    for name, count in (("1024_BIT", 2), ("OVER_8192_BIT", 17)):
        modulus, d = multi_prime(count, rng, 65537)
        assert modulus.bit_length() in (1024, *range(8193, 8705))
        signature, _ = pkcs1_signature(modulus, d, message)
        rows.append(
            (f"REFUSED_RSA_PKCS1_SHA256_{name}", modulus, 65537, message, signature)
        )
    _, em = pkcs1_signature(n, 1, message)
    rows.append(("REFUSED_RSA_PKCS1_SHA256_E_1", n, 1, message, em))
    rows.append(even_exponent_row(message))
    # The largest exponents allowed and not: 34 bits refused, 33 taken.
    for name, e in (
        ("REFUSED_RSA_PKCS1_SHA256_E_34_BIT", 2**33 + 17),
        ("RSA_PKCS1_SHA256", 2**32 + 15),
    ):
        assert all(e % f for f in range(2, 1 << 17)) and gcd(e, phi) == 1
        signature, _ = pkcs1_signature(n, pow(e, -1, phi), message)
        rows.append((name, n, e, message, signature))
    for name, modulus, e, signed, signature in rows:
        public = der(0x30, integer(modulus) + integer(e))
        print(f"{name} {public.hex()} {signed.hex()} {signature.hex()}")


def even_exponent_row(message):
    # A 2048-bit key whose signature is one under e = 65538, so only the
    # parity check refuses it: primes 3 mod 4 with 32769 invertible, a
    # message whose padding is a square mod each, its root through the
    # CRT, then that root's 32769th root.
    rng = random.Random(65538)
    while True:
        p, q = prime_of(1024, rng), prime_of(1024, rng)
        lam = (p - 1) * (q - 1) // gcd(p - 1, q - 1)
        if p % 4 == 3 and q % 4 == 3 and gcd(32769, lam) == 1:
            break
    n = p * q
    assert n.bit_length() == 2048
    for counter in range(1000):
        signed = message + b" " + str(counter).encode()
        _, em = pkcs1_signature(n, 1, signed)
        value = int.from_bytes(em, "big")
        if all(pow(value, (f - 1) // 2, f) == 1 for f in (p, q)):
            break
    else:
        raise AssertionError("no padding is a square")
    rp, rq = pow(value, (p + 1) // 4, p), pow(value, (q + 1) // 4, q)
    root = (rp * q * pow(q, -1, p) + rq * p * pow(p, -1, q)) % n
    signature = pow(root, pow(32769, -1, lam), n)
    assert pow(signature, 65538, n) == value
    name = "REFUSED_RSA_PKCS1_SHA256_E_EVEN"
    return (name, n, 65538, signed, signature.to_bytes(256, "big"))


def is_prime(n, rng):
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
        candidate = rng.getrandbits(bits) | (0b11 << (bits - 2)) | 1
        if is_prime(candidate, rng):
            return candidate


def mgf1(hash_name, seed, length):
    out, counter = b"", 0
    while len(out) < length:
        out += hashlib.new(hash_name, seed + counter.to_bytes(4, "big")).digest()
        counter += 1
    return out[:length]


def pss(hash_name, message, em_bits):
    size = hashlib.new(hash_name).digest_size
    em_len = (em_bits + 7) // 8
    salt = hashlib.new(hash_name, b"td tls verifier salt" + message).digest()
    h = hashlib.new(
        hash_name, bytes(8) + hashlib.new(hash_name, message).digest() + salt
    ).digest()
    db = bytes(em_len - size - size - 2) + b"\x01" + salt
    masked = bytes(a ^ b for a, b in zip(db, mgf1(hash_name, h, len(db))))
    masked = bytes([masked[0] & (0xff >> (8 * em_len - em_bits))]) + masked[1:]
    return masked + h + b"\xbc"


def rsa_rows():
    rng = random.Random(2048)
    e = 65537
    while True:
        p, q = prime_of(1024, rng), prime_of(1024, rng)
        n = p * q
        if n.bit_length() == 2048 and (p - 1) * (q - 1) % e != 0:
            break
    d = pow(e, -1, (p - 1) * (q - 1))
    public = der(0x30, integer(n) + integer(e))
    for hash_name in ("sha256", "sha384", "sha512"):
        name = f"RSA_PKCS1_{hash_name.upper()}"
        message = b"td verifier table " + name.encode()
        tail = INFO[hash_name] + hashlib.new(hash_name, message).digest()
        em = b"\x00\x01" + b"\xff" * (256 - len(tail) - 3) + b"\x00" + tail
        signature = pow(int.from_bytes(em, "big"), d, n).to_bytes(256, "big")
        print(f"{name} {public.hex()} {message.hex()} {signature.hex()}")
    for hash_name in ("sha256", "sha384", "sha512"):
        name = f"RSA_PSS_{hash_name.upper()}"
        message = b"td verifier table " + name.encode()
        em = pss(hash_name, message, 2047)
        signature = pow(int.from_bytes(em, "big"), d, n).to_bytes(256, "big")
        print(f"{name} {public.hex()} {message.hex()} {signature.hex()}")
    pgp_row(n, e, d)
    rsa_edges(n, p, q)


def ed25519_row():
    q = 2**255 - 19
    order = 2**252 + 27742317777372353535851937790883648493
    d = -121665 * pow(121666, q - 2, q) % q
    root = pow(2, (q - 1) // 4, q)

    def recover_x(y):
        square = (y * y - 1) * pow(d * y * y + 1, q - 2, q) % q
        x = pow(square, (q + 3) // 8, q)
        if (x * x - square) % q:
            x = x * root % q
        return q - x if x % 2 else x

    def add(a, b):
        cross = d * a[0] * b[0] * a[1] * b[1] % q
        x = (a[0] * b[1] + b[0] * a[1]) * pow(1 + cross, q - 2, q) % q
        y = (a[1] * b[1] + a[0] * b[0]) * pow(1 - cross, q - 2, q) % q
        return x, y

    def times(scalar, point):
        result = (0, 1)
        while scalar:
            if scalar & 1:
                result = add(result, point)
            point = add(point, point)
            scalar >>= 1
        return result

    def encode(point):
        return (point[1] | (point[0] & 1) << 255).to_bytes(32, "little")

    y = 4 * pow(5, q - 2, q) % q
    base = (recover_x(y), y)
    # RFC 8032 5.1.6.
    expanded = hashlib.sha512(hashlib.sha256(b"td tls verifier ed25519").digest()).digest()
    a = int.from_bytes(expanded[:32], "little") & (2**254 - 8) | 2**254
    public = encode(times(a, base))
    message = b"td verifier table ED25519"
    r = int.from_bytes(hashlib.sha512(expanded[32:] + message).digest(), "little") % order
    big_r = encode(times(r, base))
    k = int.from_bytes(hashlib.sha512(big_r + public + message).digest(), "little") % order
    signature = big_r + ((r + k * a) % order).to_bytes(32, "little")
    print(f"ED25519 {public.hex()} {message.hex()} {signature.hex()}")
    # S of L or more is refused, though S - L would verify.
    s_plus_l = (r + k * a) % order + order
    signature = big_r + s_plus_l.to_bytes(32, "little")
    print(f"REFUSED_ED25519_S_PLUS_L {public.hex()} {message.hex()} {signature.hex()}")
    # The identity, of order one: R = rB, S = r verifies any message under it.
    identity = encode((0, 1))
    message = b"td verifier table SMALL_ORDER_ED25519"
    r = int.from_bytes(hashlib.sha512(message).digest(), "little") % order
    signature = encode(times(r, base)) + r.to_bytes(32, "little")
    print(f"SMALL_ORDER_ED25519 {identity.hex()} {message.hex()} {signature.hex()}")


def pgp_row(n, e, d):
    # The creation time the fixture's Flathub key signed at, so its key dates fit.
    subpackets = bytes([5, 2]) + (1_792_503_025).to_bytes(4, "big")
    head = bytes([4, 0, 1, 8]) + len(subpackets).to_bytes(2, "big") + subpackets
    trailer = bytes([4, 0xff]) + len(head).to_bytes(4, "big")
    for counter in range(100_000):
        data = b"td short mpi %d" % counter
        tail = INFO["sha256"] + hashlib.sha256(data + head + trailer).digest()
        em = b"\x00\x01" + b"\xff" * (256 - len(tail) - 3) + b"\x00" + tail
        signature = pow(int.from_bytes(em, "big"), d, n).to_bytes(256, "big")
        if signature[0] == 0 and signature[1] != 0:
            value = signature[1:]
            print(
                f"PGP_SHORT_MPI {n.to_bytes(256, 'big').hex()} {e:06x} {data.hex()} "
                f"{subpackets.hex()} {value.hex()}"
            )
            return
    raise AssertionError("no signature with one leading zero")


def main():
    p256 = ecdsa_row("ECDSA_P256_SHA256", P256, "sha256", b"p256")
    ecdsa_row("ECDSA_P256_SHA384", P256, "sha384", b"p256")
    ecdsa_row("ECDSA_P384_SHA256", P384, "sha256", b"p384")
    ecdsa_row("ECDSA_P384_SHA384", P384, "sha384", b"p384")
    ecdsa_row("ECDSA_P256_SHA512", P256, "sha512", b"p256")
    ecdsa_row("ECDSA_P384_SHA512", P384, "sha512", b"p384")
    ecdsa_row("ECDSA_P521_SHA256", P521, "sha256", b"p521")
    ecdsa_row("ECDSA_P521_SHA384", P521, "sha384", b"p521")
    ecdsa_row("ECDSA_P521_SHA512", P521, "sha512", b"p521")
    ed25519_row()
    rsa_rows()
    ecdsa_refusals(*p256)


if __name__ == "__main__":
    main()
