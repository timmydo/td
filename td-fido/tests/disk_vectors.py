"""Optional public disk-protector vector generator; Python stdlib only.

Usage: python3 disk_vectors.py > disk_vectors.txt
Independent of td: hashlib and hmac, with HKDF (RFC 5869) written here.
No network. Not a build/test/runtime dependency. Every value below is a
deterministic PUBLIC fixture.
Byte layout: td-install/ENCRYPTION.md, "FIDO2 protectors".
"""
import hashlib
import hmac


def sha(data):
    return hashlib.sha256(data).digest()


def hkdf(ikm, salt, info, length):
    prk = hmac.digest(salt or bytes(32), ikm, "sha256")
    out, block, counter = b"", b"", 1
    while len(out) < length:
        block = hmac.digest(prk, block + info + bytes([counter]), "sha256")
        out += block
        counter += 1
    return out[:length]


def u32(data):
    return len(data).to_bytes(4, "big") + data


rows = []
description = b"td disk vector: unlock volume 00112233-4455-6677-8899-aabbccddeeff"
random = sha(b"disk-vectors/random")
rows.append(("hash", "description", description))
rows.append(("hash", "random", random))
for name, phase in [("unlock", 1), ("create", 2), ("prove", 3), ("repeat", 4),
                    ("probe", 5), ("authorize", 6)]:
    client = sha(b"td/disk-protector/operation/v1\0" + bytes([phase]) + u32(description) + random)
    rows.append(("hash", name, client))
rows.append(("hash", "empty", sha(b"td/disk-protector/operation/v1\0\x01" + u32(b"") + random)))

output = sha(b"disk-vectors/output")
volume = bytes.fromhex("00112233445566778899aabbccddeeff")
rows.append(("passphrase", "output", output))
rows.append(("passphrase", "volume", volume))
for name, credential in [("one", b"\x07"), ("short", b"disk-credential"),
                         ("longest", bytes(i % 251 for i in range(255)))]:
    info = b"td/disk-protector/fido2/v1\0" + volume + u32(credential)
    rows.append(("passphrase", "credential_" + name, credential))
    rows.append(("passphrase", "key_" + name, hkdf(output, b"", info, 32)))

print("# Public disk-protector vectors; disk_vectors.py, Python stdlib only")
for group, name, value in rows:
    print(group, name, value.hex())
