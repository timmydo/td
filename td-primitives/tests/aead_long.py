"""Known answers for AEAD records of real length, from a naive reference that
shares nothing with this crate: a table-driven AES, bit-at-a-time GHASH, textbook
ChaCha20 and big-integer Poly1305. Wycheproof's longest message is 513 bytes,
which a truncated block counter or length field survives.

    python3 -I aead_long.py > aead_long.txt

Each row is `algorithm seed aad_length length tag sha256(ciphertext)`. Key, nonce,
AAD and message expand from the seed, as in digest_vectors.py. Lengths cover TLS
records and pass 2^16 counter blocks, past which a 16-bit counter would wrap.
"""

import hashlib

MASK32 = 0xffffffff


def expand(seed, length):
    out = bytearray()
    state = seed
    for _ in range(length):
        state = (state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        out.append(state >> 56)
    return bytes(out)


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
    state = [a ^ b for a, b in zip(block, schedule[0])]
    for round_key in schedule[1:-1]:
        s = [SBOX[b] for b in state]
        s = [
            s[0], s[5], s[10], s[15], s[4], s[9], s[14], s[3],
            s[8], s[13], s[2], s[7], s[12], s[1], s[6], s[11],
        ]
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
    s = [SBOX[b] for b in state]
    s = [
        s[0], s[5], s[10], s[15], s[4], s[9], s[14], s[3],
        s[8], s[13], s[2], s[7], s[12], s[1], s[6], s[11],
    ]
    return bytes(x ^ y for x, y in zip(s, schedule[-1]))


def gf_multiply(x, h):
    """SP 800-38D algorithm 1, with bit 0 of the spec the top of the integer."""
    z, v = 0, h
    for i in range(127, -1, -1):
        if (x >> i) & 1:
            z ^= v
        v = (v >> 1) ^ (0xe1 << 120) if v & 1 else v >> 1
    return z


def ghash(h, parts):
    y = 0
    for part in parts:
        for at in range(0, len(part), 16):
            block = part[at : at + 16].ljust(16, b"\0")
            y = gf_multiply(y ^ int.from_bytes(block, "big"), h)
    return y


def aes_gcm(key, nonce, aad, message):
    schedule = expand_key(key)
    h = int.from_bytes(aes_block(schedule, bytes(16)), "big")
    out = bytearray(len(message))
    for index in range(0, (len(message) + 15) // 16):
        counter = nonce + (index + 2).to_bytes(4, "big")
        stream = aes_block(schedule, counter)
        at = index * 16
        for offset, byte in enumerate(message[at : at + 16]):
            out[at + offset] = byte ^ stream[offset]
    lengths = (len(aad) * 8).to_bytes(8, "big") + (len(message) * 8).to_bytes(8, "big")
    y = ghash(h, [aad, bytes(out), lengths])
    mask = int.from_bytes(aes_block(schedule, nonce + (1).to_bytes(4, "big")), "big")
    return bytes(out), (y ^ mask).to_bytes(16, "big")


def rotate(x, count):
    return ((x << count) | (x >> (32 - count))) & MASK32


def quarter(s, a, b, c, d):
    s[a] = (s[a] + s[b]) & MASK32
    s[d] = rotate(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & MASK32
    s[b] = rotate(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b]) & MASK32
    s[d] = rotate(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & MASK32
    s[b] = rotate(s[b] ^ s[c], 7)


def chacha_block(key_words, counter, nonce_words):
    start = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574]
    start += key_words + [counter] + nonce_words
    s = list(start)
    for _ in range(10):
        quarter(s, 0, 4, 8, 12)
        quarter(s, 1, 5, 9, 13)
        quarter(s, 2, 6, 10, 14)
        quarter(s, 3, 7, 11, 15)
        quarter(s, 0, 5, 10, 15)
        quarter(s, 1, 6, 11, 12)
        quarter(s, 2, 7, 8, 13)
        quarter(s, 3, 4, 9, 14)
    return b"".join(((a + b) & MASK32).to_bytes(4, "little") for a, b in zip(s, start))


def poly1305(key, parts):
    r = int.from_bytes(key[:16], "little") & 0x0ffffffc0ffffffc0ffffffc0fffffff
    s = int.from_bytes(key[16:32], "little")
    p = (1 << 130) - 5
    h = 0
    for part in parts:
        for at in range(0, len(part), 16):
            block = part[at : at + 16].ljust(16, b"\0")
            h = (h + int.from_bytes(block, "little") + (1 << 128)) * r % p
    return ((h + s) % (1 << 128)).to_bytes(16, "little")


def chacha20_poly1305(key, nonce, aad, message):
    words = lambda data: [
        int.from_bytes(data[i : i + 4], "little") for i in range(0, len(data), 4)
    ]
    key_words, nonce_words = words(key), words(nonce)
    out = bytearray(len(message))
    for index in range(0, (len(message) + 63) // 64):
        stream = chacha_block(key_words, index + 1, nonce_words)
        at = index * 64
        for offset, byte in enumerate(message[at : at + 64]):
            out[at + offset] = byte ^ stream[offset]
    lengths = len(aad).to_bytes(8, "little") + len(message).to_bytes(8, "little")
    tag = poly1305(chacha_block(key_words, 0, nonce_words), [aad, bytes(out), lengths])
    return bytes(out), tag


def main():
    two16_aes = (1 << 16) * 16 + 100
    two16_chacha = (1 << 16) * 64 + 100
    cases = [
        ("aes128", 16, aes_gcm, (4096, 16384, 16640, two16_aes)),
        ("aes256", 32, aes_gcm, (4096, 16384, 16640, two16_aes)),
        ("chacha", 32, chacha20_poly1305, (4096, 16384, 16640, two16_chacha)),
    ]
    seed = 7000
    for name, key_len, cipher, lengths in cases:
        for length in lengths:
            aad_length = 70000 if length == 16640 else 13
            key = expand(seed, key_len)
            nonce = expand(seed + 1, 12)
            aad = expand(seed + 2, aad_length)
            message = expand(seed + 3, length)
            sealed, tag = cipher(key, nonce, aad, message)
            digest = hashlib.sha256(sealed).hexdigest()
            print(f"{name} {seed} {aad_length} {length} {tag.hex()} {digest}")
            seed += 4


if __name__ == "__main__":
    main()
