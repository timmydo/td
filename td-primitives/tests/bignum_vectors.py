"""bignum.rs's Montgomery arithmetic at the widths the curves use, answered by
Python's big integers so that the unit test has an answer this crate did not
compute.

    python3 -I bignum_vectors.py > bignum_vectors.txt

The moduli are P-256's, P-384's and P-521's field primes and group orders, at
4, 6 and 9 limbs. Each opens with `modulus NAME LIMBS HEX`, then lists its
operands, `operand NAME INDEX HEX`, and then rows `NAME OP A B RESULT`, where
A and B index the operands (B is `-` for one-operand rows) and R = 2^(64 LIMBS).
Every value is big-endian hex of 8 LIMBS bytes. The rows state what each
function does to its raw limbs, Montgomery form included:

  mul(a, b)    = a b / R mod m          every pair of operands
  add(a, b)    = a + b mod m            every pair
  sub(a, b)    = a - b mod m            every pair
  enter(a)     = a R mod m              every operand and the values at and
                                        past m: m, m + 1 and R - 1
  leave(a)     = a / R mod m            every operand
  invert(a)    = (a / R)^(m - 2) R      every operand, 0 included
  pow(a, e)    = (a / R)^e R mod m      every operand, against the exponents
                                        0, 1, 2, m - 2 and m - 1, which are
                                        operands too

The operands are 0, 1, 2, (m - 1) / 2, (m + 1) / 2, m - 2^(64 (LIMBS - 1)),
m - 2, m - 1 and two drawn from SHA-512 below m. The values past m, which only
enter takes, follow as indexes 10, 11 and 12.
"""

import hashlib

MODULI = [
    ("P256-p", 4, 2**256 - 2**224 + 2**192 + 2**96 - 1),
    ("P256-n", 4, 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551),
    ("P384-p", 6, 2**384 - 2**128 - 2**96 + 2**32 - 1),
    (
        "P384-n",
        6,
        0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFC7634D81F4372DDF581A0DB248B0A77AECEC196ACCC52973,
    ),
    ("P521-p", 9, 2**521 - 1),
    (
        "P521-n",
        9,
        int(
            "1ff" + "ffffffff" * 7 + "fffffffa51868783bf2f966b7fcc0148f709a5d0"
            "3bb5c9b8899c47aebb6fb71e91386409",
            16,
        ),
    ),
]
# FIPS 186-4 D.1.2.5's decimal n, against a slip in the hex.
assert MODULI[5][2] == int(
    "686479766013060971498190079908139321726943530014330540939446345918554318339765"
    "5394245057746333217197532963996371363321113864768612440380340372808892707005449"
)


def is_probable_prime(m):
    for a in [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37]:
        d, s = m - 1, 0
        while d % 2 == 0:
            d, s = d // 2, s + 1
        x = pow(a, d, m)
        if x in (1, m - 1):
            continue
        for _ in range(s - 1):
            x = x * x % m
            if x == m - 1:
                break
        else:
            return False
    return True


def drawn(name, i, m):
    seed = hashlib.sha512(f"{name} {i}".encode()).digest()
    return int.from_bytes(seed + hashlib.sha512(seed).digest(), "big") % m


def main():
    for name, limbs, m in MODULI:
        assert is_probable_prime(m)
        r = 2 ** (64 * limbs)
        r_inverse = pow(r, -1, m)
        width = 16 * limbs
        hx = lambda value: format(value, f"0{width}x")
        operands = [
            0,
            1,
            2,
            (m - 1) // 2,
            (m + 1) // 2,
            m - 2 ** (64 * (limbs - 1)),
            m - 2,
            m - 1,
            drawn(name, 0, m),
            drawn(name, 1, m),
        ]
        past = [m, m + 1, r - 1]
        print("modulus", name, limbs, hx(m))
        for i, value in enumerate(operands + past):
            print("operand", name, i, hx(value))
        pairs = range(len(operands))
        for i in pairs:
            for j in pairs:
                a, b = operands[i], operands[j]
                print(name, "mul", i, j, hx(a * b * r_inverse % m))
                print(name, "add", i, j, hx((a + b) % m))
                print(name, "sub", i, j, hx((a - b) % m))
        for i, a in enumerate(operands + past):
            print(name, "enter", i, "-", hx(a * r % m))
        for i, a in enumerate(operands):
            print(name, "leave", i, "-", hx(a * r_inverse % m))
            plain = a * r_inverse % m
            print(name, "invert", i, "-", hx(pow(plain, m - 2, m) * r % m))
            for j in [0, 1, 2, 6, 7]:
                print(name, "pow", i, j, hx(pow(plain, operands[j], m) * r % m))


main()
