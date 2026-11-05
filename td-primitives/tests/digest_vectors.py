"""Digest and HMAC answers from Python's hashlib and hmac, an implementation
independent of this crate, for tests/vectors.rs.

    python3 -I digest_vectors.py > digest_vectors.txt

Messages and keys are not stored: both sides expand a seed with the same
generator, so the file holds one line per case and stays small. Lengths cover
every block boundary of the 64- and 128-byte block sizes and their padding.
"""

import hashlib
import hmac

ALGORITHMS = ("sha1", "sha256", "sha384", "sha512")


def expand(seed, length):
    out = bytearray()
    state = seed
    for _ in range(length):
        state = (state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        out.append(state >> 56)
    return bytes(out)


def main():
    seed = 1
    for algorithm in ALGORITHMS:
        for length in list(range(0, 260)) + [1000, 4096, 100000]:
            message = expand(seed, length)
            digest = hashlib.new(algorithm, message).hexdigest()
            print(f"{algorithm} {seed} {length} {digest}")
            seed += 1
    for algorithm in ("sha256", "sha384", "sha512"):
        for key_length in (0, 1, 20, 32, 63, 64, 65, 127, 128, 129, 300):
            for length in (0, 1, 55, 64, 111, 128, 1000):
                key = expand(seed, key_length)
                message = expand(seed + 1, length)
                mac = hmac.new(key, message, algorithm).hexdigest()
                print(f"hmac-{algorithm} {seed} {key_length} {length} {mac}")
                seed += 2


if __name__ == "__main__":
    main()
