"""P-256 ECDSA signatures that take RFC 6979's retry, made here with Python's
hmac and big integers so that ec.rs's unit test has an answer this crate did
not compute.

    python3 -I ecdsa_retry.py > ecdsa_retry.txt

No reachable key and message make a P-256 nonce candidate fall outside
[1, n): the chance is about 2^-32 a candidate. The test therefore replaces
the first candidates, the DRBG stepping past each as it would past a
rejected one, and the signature must come from the candidate after them, by
RFC 6979 3.2 step h.3: K = HMAC_K(V || 0x00), V = HMAC_K(V), then the next
V. Each row is `CASE KEY REPLACED SIGNATURE`, the message is "sample", and
REPLACED is the candidates put in place of the DRBG's first ones, comma
separated:

  out-of-range: n, 0 and 2^256 - 1, so the fourth block signs.

  s-zero: a candidate k0 for which s is zero, by choosing the key as
  -e / r0 mod n, where r0 is k0 G's x. The second block signs. r = 0 needs
  a k with k G's x a multiple of n, which nobody can find, so no row has it.

  s-zero-then-out-of-range: k0 then n, against the s-zero key; the third
  block signs.
"""

import hashlib
import hmac

P = 2**256 - 2**224 + 2**192 + 2**96 - 1
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551
G = (
    0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
    0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5,
)


def add(a, b):
    if a is None:
        return b
    if b is None:
        return a
    if a[0] == b[0] and (a[1] + b[1]) % P == 0:
        return None
    if a == b:
        slope = (3 * a[0] * a[0] - 3) * pow(2 * a[1], -1, P) % P
    else:
        slope = (b[1] - a[1]) * pow(b[0] - a[0], -1, P) % P
    x = (slope * slope - a[0] - b[0]) % P
    return (x, (slope * (a[0] - x) - a[1]) % P)


def multiply(k, point=G):
    result = None
    while k:
        if k & 1:
            result = add(result, point)
        point = add(point, point)
        k >>= 1
    return result


def mac(key, data):
    return hmac.new(key, data, hashlib.sha256).digest()


def blocks(d, e):
    """RFC 6979 3.2's candidates for SHA-256 and a 256-bit order, without end."""
    x, h = d.to_bytes(32, "big"), (e % N).to_bytes(32, "big")
    v, k = b"\x01" * 32, b"\x00" * 32
    k = mac(k, v + b"\x00" + x + h)
    v = mac(k, v)
    k = mac(k, v + b"\x01" + x + h)
    v = mac(k, v)
    while True:
        v = mac(k, v)
        yield v
        # h.3, whether or not the candidate was taken.
        k = mac(k, v + b"\x00")
        v = mac(k, v)


def der_integer(value):
    body = value.to_bytes(32, "big").lstrip(b"\x00") or b"\x00"
    if body[0] & 0x80:
        body = b"\x00" + body
    return bytes([0x02, len(body)]) + body


def sign(d, e, replaced):
    for i, block in enumerate(blocks(d, e)):
        if i < len(replaced):
            block = replaced[i]
        k = int.from_bytes(block, "big")
        if not 1 <= k < N:
            continue
        r = multiply(k)[0] % N
        s = pow(k, -1, N) * (e + r * d) % N
        if r == 0 or s == 0:
            continue
        assert i == len(replaced)
        body = der_integer(r) + der_integer(s)
        return bytes([0x30, len(body)]) + body


def main():
    e = int.from_bytes(hashlib.sha256(b"sample").digest(), "big")
    # RFC 6979 A.2.5's key.
    rfc_key = 0xC9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721
    # This DRBG and signer give that appendix's k and r first.
    first = next(blocks(rfc_key, e)).hex()
    assert first == "a6e3c57dd01abe90086538398355dd4c3b17aa873382b0f24d6129493d8aad60"
    assert sign(rfc_key, e, [])[5:37].hex() == (
        "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716"
    )
    k0 = int.from_bytes(hashlib.sha256(b"td s-zero nonce").digest(), "big") % N
    r0 = multiply(k0)[0] % N
    zero_key = -e * pow(r0, -1, N) % N
    assert (e + r0 * zero_key) % N == 0
    n_bytes, k0_bytes = N.to_bytes(32, "big"), k0.to_bytes(32, "big")
    cases = [
        ("out-of-range", rfc_key, [n_bytes, bytes(32), b"\xff" * 32]),
        ("s-zero", zero_key, [k0_bytes]),
        ("s-zero-then-out-of-range", zero_key, [k0_bytes, n_bytes]),
    ]
    for name, d, replaced in cases:
        signature = sign(d, e, replaced)
        print(
            name,
            d.to_bytes(32, "big").hex(),
            ",".join(r.hex() for r in replaced),
            signature.hex(),
        )


main()
