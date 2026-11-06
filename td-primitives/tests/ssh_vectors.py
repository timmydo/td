"""Answers for the corpora under ssh/, from a reference that shares nothing with this
crate: Python's hashlib and hmac, a table-driven AES, textbook ChaCha20, big-integer
Poly1305, and a Blowfish whose tables are computed from pi here.

    python3 -I ssh_vectors.py keccak > ssh/keccak.txt
    python3 -I ssh_vectors.py hmac_sha1 > ssh/hmac_sha1.txt
    python3 -I ssh_vectors.py aes256ctr > ssh/aes256ctr.txt
    python3 -I ssh_vectors.py chachapoly > ssh/chachapoly.txt
    python3 -I ssh_vectors.py poly1305 ssh/poly1305.txt > new.txt
    python3 -I ssh_vectors.py bcrypt_layout ssh/bcrypt_pbkdf.txt > ssh/bcrypt_layout.txt

The first four write their whole file. `poly1305` keeps each row's key and message,
which came at random, and answers again. `bcrypt_layout` first holds its Blowfish to
the OpenBSD rows it is given, and takes over a minute, as one output block costs
67,000 enciphers at each round.
"""

import hashlib
import hmac
import sys

MASK32 = 0xffffffff


def message(length):
    return bytes((i * 17 + length) & 0xff for i in range(length))


def lengths():
    edges = []
    for block in (16, 32, 48, 56, 64, 72, 80, 96, 104, 112, 120, 128, 136, 144, 168, 192, 200, 256, 272, 512):
        edges += [block - 1, block, block + 1]
    return [0, 1, *sorted(set(edges)), 1000]


def keccak():
    print("# Generator: Python 3.12 hashlib over OpenSSL 3.5.7.")
    print("# algorithm message-length digest; message[i] = (i * 17 + length) & 0xff.")
    for name in ("sha3_256", "sha3_512"):
        for length in lengths():
            print(name, length, hashlib.new(name, message(length)).hexdigest())
    for name in ("shake128", "shake256"):
        for length in lengths():
            print(name, length, hashlib.new(name, message(length)).hexdigest(64))
        for length in (0, 137, 168, 1000):
            print(name, length, hashlib.new(name, message(length)).hexdigest(600))


def hmac_sha1():
    print("# Generator: Python 3.12 hmac over OpenSSL 3.5.7.")
    print("# key-length message-length mac; key[i] = (i * 31 + key-length + 7) & 0xff,")
    print("# message[i] = (i * 17 + length) & 0xff.")
    for key_length in (0, 1, 19, 20, 21, 63, 64, 65, 100, 200):
        key = bytes((i * 31 + key_length + 7) & 0xff for i in range(key_length))
        for length in (0, 1, 55, 56, 63, 64, 65, 119, 120, 128, 1000):
            mac = hmac.new(key, message(length), "sha1").hexdigest()
            print(key_length, length, mac)


def build_sbox():
    def multiply(a, b):
        product = 0
        while b:
            if b & 1:
                product ^= a
            a = ((a << 1) ^ (0x1b if a & 0x80 else 0)) & 0xff
            b >>= 1
        return product

    box = []
    for x in range(256):
        inverse = next((y for y in range(256) if multiply(x, y) == 1), 0)
        value = inverse
        for shift in range(1, 5):
            value ^= ((inverse << shift) | (inverse >> (8 - shift))) & 0xff
        box.append(value ^ 0x63)
    return box, [multiply(x, 2) for x in range(256)], [multiply(x, 3) for x in range(256)]


SBOX, TWO, THREE = build_sbox()


def expand_key(key):
    nk = len(key) // 4
    rounds = nk + 6
    words = [list(key[i * 4 : i * 4 + 4]) for i in range(nk)]
    rcon = 1
    for i in range(nk, 4 * (rounds + 1)):
        word = list(words[i - 1])
        if i % nk == 0:
            word = word[1:] + word[:1]
            word = [SBOX[b] for b in word]
            word[0] ^= rcon
            rcon = TWO[rcon]
        elif nk > 6 and i % nk == 4:
            word = [SBOX[b] for b in word]
        words.append([a ^ b for a, b in zip(words[i - nk], word)])
    return [sum(words[i * 4 : i * 4 + 4], []) for i in range(rounds + 1)]


def aes_block(schedule, block):
    shifted = (0, 5, 10, 15, 4, 9, 14, 3, 8, 13, 2, 7, 12, 1, 6, 11)
    state = [a ^ b for a, b in zip(block, schedule[0])]
    for round_key in schedule[1:-1]:
        s = [SBOX[state[at]] for at in shifted]
        mixed = []
        for c in range(0, 16, 4):
            a, b, d, e = s[c], s[c + 1], s[c + 2], s[c + 3]
            mixed += [
                TWO[a] ^ THREE[b] ^ d ^ e,
                a ^ TWO[b] ^ THREE[d] ^ e,
                a ^ b ^ TWO[d] ^ THREE[e],
                THREE[a] ^ b ^ d ^ TWO[e],
            ]
        state = [x ^ y for x, y in zip(mixed, round_key)]
    s = [SBOX[state[at]] for at in shifted]
    return bytes(x ^ y for x, y in zip(s, schedule[-1]))


def aes256ctr():
    print("# Generator: OpenSSL 3.5.7 enc -aes-256-ctr over zeros, including IVs that carry")
    print("# across the 64-bit boundary and wrap all 128 bits. The last 20 rows carry across")
    print("# 32, 64 and 128 bits inside either backend's batch. A table AES in ssh_vectors.py")
    print("# gives every row too.")
    print('# key iv keystream; "-" is empty.')
    key = bytes.fromhex("593a4af0e330ad3482f7d8c6ec8a618ad0792756ed190525da9e7e4b9684bdb6")
    schedule = expand_key(key)
    first = (0, 1, 15, 16, 17, 31, 32, 33, 48, 100)
    later = (127, 128, 129, 300, 1000)
    plan = [(iv, first) for iv in (
        "0b238d97521beb8c9ba8a495df538a3c",
        "0000000000000000ffffffffffffffff",
        "ffffffffffffffffffffffffffffffff",
        "00000000000000000000000000000000",
    )] + [(iv, later) for iv in (
        "0b238d97521beb8c9ba8a495df538a3c",
        "000000000000000000000000fffffffd",
        "0000000000000000fffffffffffffffd",
        "fffffffffffffffffffffffffffffffd",
    )]
    for iv, sizes in plan:
        counter = int(iv, 16)
        for size in sizes:
            stream = bytearray()
            at = counter
            while len(stream) < size:
                stream += aes_block(schedule, at.to_bytes(16, "big"))
                at = (at + 1) % (1 << 128)
            print(key.hex(), iv, stream[:size].hex() or "-")


def rotl(x, n):
    return ((x << n) & MASK32) | (x >> (32 - n))


def chacha_block(key, counter, nonce):
    words = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574]
    words += [int.from_bytes(key[i : i + 4], "little") for i in range(0, 32, 4)]
    words += [counter & MASK32, counter >> 32]
    words += [int.from_bytes(nonce[i : i + 4], "little") for i in range(0, 8, 4)]
    x = list(words)

    def quarter(a, b, c, d):
        x[a] = (x[a] + x[b]) & MASK32
        x[d] = rotl(x[d] ^ x[a], 16)
        x[c] = (x[c] + x[d]) & MASK32
        x[b] = rotl(x[b] ^ x[c], 12)
        x[a] = (x[a] + x[b]) & MASK32
        x[d] = rotl(x[d] ^ x[a], 8)
        x[c] = (x[c] + x[d]) & MASK32
        x[b] = rotl(x[b] ^ x[c], 7)

    for _ in range(10):
        quarter(0, 4, 8, 12)
        quarter(1, 5, 9, 13)
        quarter(2, 6, 10, 14)
        quarter(3, 7, 11, 15)
        quarter(0, 5, 10, 15)
        quarter(1, 6, 11, 12)
        quarter(2, 7, 8, 13)
        quarter(3, 4, 9, 14)
    return b"".join(((a + b) & MASK32).to_bytes(4, "little") for a, b in zip(x, words))


def chacha_xor(key, counter, nonce, data):
    out = bytearray()
    for at in range(0, len(data), 64):
        pad = chacha_block(key, counter + at // 64, nonce)
        out += bytes(a ^ b for a, b in zip(data[at : at + 64], pad))
    return bytes(out)


def poly1305_tag(key, data):
    r = int.from_bytes(key[:16], "little") & 0x0ffffffc0ffffffc0ffffffc0fffffff
    s = int.from_bytes(key[16:32], "little")
    p = (1 << 130) - 5
    h = 0
    for at in range(0, len(data), 16):
        block = data[at : at + 16]
        h = (h + int.from_bytes(block + b"\x01", "little")) * r % p
    return ((h + s) % (1 << 128)).to_bytes(16, "little")


def chachapoly():
    print("# Generator: chacha20-poly1305@openssh.com framing composed from OpenSSL 3.5.7:")
    print("# enc -chacha20 for the length (K_1, counter 0), the Poly1305 key (K_2, counter 0)")
    print("# and the payload (K_2, counter 1), each with IV = counter-low-LE32 ||")
    print("# counter-high-LE32 || BE64(seq); then mac poly1305 over encrypted length and")
    print("# ciphertext. Framing checked by eye against OpenSSH cipher-chachapoly.c.")
    print("# key sequence payload-length sealed-packet tag;")
    print("# plaintext = BE32(payload-length) || payload, payload[i] = (i * 17 + length) & 0xff.")
    key = bytes.fromhex(KEY_64)
    main, length_key = key[:32], key[32:]
    plan = [(0x12345678, size) for size in (
        0, 1, 2, 3, 4, 15, 16, 17, 28, 55, 56, 60, 63, 64, 65, 111, 112, 124, 127, 128,
        129, 135, 136, 137, 191, 192, 193, 252, 255, 256, 257, 316, 319, 320, 321, 512,
        1024, 2000,
    )]
    for seq in (0, 1, 3, 255, 256, 0xdeadbeef, MASK32):
        plan += [(seq, 32), (seq, 300)]
    for seq, size in plan:
        nonce = seq.to_bytes(8, "big")
        head = chacha_xor(length_key, 0, nonce, size.to_bytes(4, "big"))
        body = chacha_xor(main, 1, nonce, message(size))
        tag = poly1305_tag(chacha_block(main, 0, nonce)[:32], head + body)
        print(key.hex(), seq, size, (head + body).hex(), tag.hex())


def poly1305(path):
    with open(path, encoding="utf-8") as file:
        for line in file:
            line = line.rstrip("\n")
            if line.startswith("#"):
                print(line)
                continue
            key, data = line.split(" ")[:2]
            data_bytes = b"" if data == "-" else bytes.fromhex(data)
            print(key, data, poly1305_tag(bytes.fromhex(key), data_bytes).hex())


def pi_words(count):
    """The fractional hex digits of pi, by Machin's formula over big integers."""
    bits = 32 * count + 64
    one = 1 << bits

    def arctan_inverse(x):
        total = term = one // x
        n, sign = 3, -1
        while term:
            term //= x * x
            total += sign * (term // n)
            n += 2
            sign = -sign
        return total

    pi = 4 * (4 * arctan_inverse(5) - arctan_inverse(239))
    fraction = pi - (3 << bits)
    return [(fraction >> (bits - 32 * (i + 1))) & MASK32 for i in range(count)]


PI = pi_words(18 + 4 * 256)


class Blowfish:
    def __init__(self):
        self.p = PI[:18]
        self.s = [PI[18 + 256 * i : 18 + 256 * (i + 1)] for i in range(4)]

    def encipher(self, left, right):
        p = self.p
        s0, s1, s2, s3 = self.s
        for key in p[:16]:
            left ^= key
            right ^= (((s0[left >> 24] + s1[(left >> 16) & 0xff]) & MASK32) ^ s2[(left >> 8) & 0xff]) + s3[left & 0xff] & MASK32
            left, right = right, left
        return right ^ p[17], left ^ p[16]

    def expand(self, salt, key):
        at = 0
        for i in range(18):
            word = 0
            for _ in range(4):
                word = (word << 8) | key[at]
                at = (at + 1) % len(key)
            self.p[i] ^= word
        salt_at = 0

        def salt_word():
            nonlocal salt_at
            word = 0
            for _ in range(4):
                word = (word << 8) | (salt[salt_at] if salt else 0)
                salt_at = (salt_at + 1) % max(len(salt), 1)
            return word

        left = right = 0
        slots = [(self.p, i) for i in range(18)]
        for box in self.s:
            slots += [(box, i) for i in range(256)]
        for at in range(0, len(slots), 2):
            left ^= salt_word()
            right ^= salt_word()
            left, right = self.encipher(left, right)
            table, index = slots[at]
            table[index] = left
            table, index = slots[at + 1]
            table[index] = right


def bcrypt_hash(sha2pass, sha2salt):
    state = Blowfish()
    state.expand(sha2salt, sha2pass)
    for _ in range(64):
        state.expand(b"", sha2salt)
        state.expand(b"", sha2pass)
    magic = b"OxychromaticBlowfishSwatDynamite"
    words = [int.from_bytes(magic[i : i + 4], "big") for i in range(0, 32, 4)]
    for _ in range(64):
        for i in range(0, 8, 2):
            words[i], words[i + 1] = state.encipher(words[i], words[i + 1])
    return b"".join(word.to_bytes(4, "little") for word in words)


LANES = {}


def lane(password, salt, rounds, count):
    """OpenBSD's outer loop for one output block, which a length does not change."""
    known = LANES.get((password, salt, rounds, count))
    if known is not None:
        return known
    sha2pass = hashlib.sha512(password).digest()
    block = bcrypt_hash(sha2pass, hashlib.sha512(salt + count.to_bytes(4, "big")).digest())
    out = block
    for _ in range(rounds - 1):
        block = bcrypt_hash(sha2pass, hashlib.sha512(block).digest())
        out = bytes(a ^ b for a, b in zip(out, block))
    LANES[(password, salt, rounds, count)] = out
    return out


def bcrypt_pbkdf(password, salt, rounds, length):
    """As OpenBSD's bcrypt_pbkdf.c lays its output out, loop for loop."""
    key = bytearray(length)
    stride = (length + 31) // 32
    amount = (length + stride - 1) // stride
    left, count = length, 1
    while left > 0:
        out = lane(password, salt, rounds, count)
        amount = min(amount, left)
        written = 0
        for i in range(amount):
            dest = i * stride + (count - 1)
            if dest >= length:
                break
            key[dest] = out[i]
            written = i + 1
        left -= written
        count += 1
    return bytes(key)


def bcrypt_layout(path):
    with open(path, encoding="utf-8") as file:
        rows = [line.split() for line in file if not line.startswith("#")]
    for password, salt, rounds, want in rows:
        got = bcrypt_pbkdf(bytes.fromhex(password), bytes.fromhex(salt), int(rounds), len(want) // 2)
        if got.hex() != want:
            raise SystemExit(f"the reference disagrees with OpenBSD: {password} {salt}")
    print("# Generator: the Blowfish and output loop of ssh_vectors.py, written from OpenBSD's")
    print("# bcrypt_pbkdf.c and held to its nine regress rows before these. Lengths are")
    print("# whole outputs, so that each block's place in them is what is checked: no second")
    print("# implementation of the longer ones is in reach here. Password and salt are hex.")
    print("# password salt rounds output")
    password, salt = b"pass", b"salt"
    for rounds, length in (
        (1, 1), (1, 31), (1, 32), (1, 33), (1, 48), (1, 64), (1, 65), (1, 1023), (1, 1024),
        (2, 48), (2, 65),
    ):
        print(password.hex(), salt.hex(), rounds, bcrypt_pbkdf(password, salt, rounds, length).hex())


KEY_64 = (
    "5b64cb3ed118ed9b25ae60b3d8b2a98649524fd5a9961b37a30a6ed3c7392eb1"
    "1fa47cf3a4d0d833610c4df5451bd4a76b6b7eb2c2099810e4baafd227171728"
)


def main():
    name, *rest = sys.argv[1:]
    {
        "keccak": keccak,
        "hmac_sha1": hmac_sha1,
        "aes256ctr": aes256ctr,
        "chachapoly": chachapoly,
        "poly1305": poly1305,
        "bcrypt_layout": bcrypt_layout,
    }[name](*rest)


if __name__ == "__main__":
    main()
