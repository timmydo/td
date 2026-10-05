"""Optional public login-record vector generator; Python stdlib only.

Usage: python3 login_record_vectors.py > login_record_vectors.txt
Independent of td: hashlib, hmac and integer P-256 arithmetic written here.
No network. Not a build/test/runtime dependency. Every scalar, salt,
hmac-secret output and random value below is a deterministic PUBLIC fixture.
Byte layout: td-login/TOKEN-LOGIN.md, "The login record".
"""
import hashlib
import hmac

P = 2**256 - 2**224 + 2**192 + 2**96 - 1
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551
B = 0x5AC635D8AA3A93E7B3EBBD55769886BC651D06B0CC53B0F63BCE3C3E27D2604B
G = (0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
     0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5)


def on_curve(point):
    x, y = point
    return (y * y - (x * x * x - 3 * x + B)) % P == 0


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


def multiply(k):
    result, addend = None, G
    while k:
        if k & 1:
            result = add(result, addend)
        addend = add(addend, addend)
        k >>= 1
    if result is None or not on_curve(result):
        raise RuntimeError("fixture point")
    return result


def sha(data):
    return hashlib.sha256(data).digest()


def hkdf(ikm, salt, info):
    # RFC 5869 extract and one expand block.
    prk = hmac.digest(salt, ikm, "sha256")
    return hmac.digest(prk, info + b"\1", "sha256")


def u16(n):
    return n.to_bytes(2, "big")


def u32(n):
    return n.to_bytes(4, "big")


MAGIC, VERSION, UID = b"TDLOGREC", 1, 1000
VERIFIER = b"td-login/verifier/v1\0"
OPERATION = b"td-login/operation/v1\0"
PHASES = [("identify", 1), ("authorize", 2), ("create", 3), ("prove", 4),
          ("repeat", 5), ("probe", 6), ("unlock", 7)]

record_id = sha(b"login-record-fixture/record-id")
# Credentials are unsorted here; the record holds them in byte order.
credentials = [b"login-fixture-credential-backup",
               b"\x01" * 64,
               b"A"]
slots = []
for index, credential in enumerate(credentials):
    seed = b"login-record-fixture/" + bytes([index])
    scalar = int.from_bytes(sha(seed + b"/scalar"), "big") % (N - 1) + 1
    x, y = multiply(scalar)
    salt, output = sha(seed + b"/salt"), sha(seed + b"/output")
    info = VERIFIER + u32(UID) + record_id + u32(len(credential)) + credential
    verifier = hkdf(output, b"", info)
    slots.append(dict(credential=credential, x=x.to_bytes(32, "big"),
                      y=y.to_bytes(32, "big"), salt=salt, output=output,
                      verifier=verifier, fingerprint=sha(credential)[:4]))

record = MAGIC + bytes([VERSION]) + u32(UID) + record_id + bytes([len(slots)])
for slot in sorted(slots, key=lambda s: s["credential"]):
    record += u16(len(slot["credential"])) + slot["credential"]
    record += slot["x"] + slot["y"] + slot["salt"] + slot["verifier"]

print("# Public login record, verifier and client-data vectors; login_record_vectors.py")
print("uid", u32(UID).hex())
print("id", record_id.hex())
for index, slot in enumerate(slots):
    for name in ["credential", "x", "y", "salt", "output", "verifier", "fingerprint"]:
        print(f"slot{index}_{name}", slot[name].hex())
print("record", record.hex())
print("digest", sha(record).hex())

description = "Unlock this td session with an enrolled login key.".encode()
random = sha(b"login-record-fixture/random")
print("description", description.hex())
print("random", random.hex())
for name, byte in PHASES:
    data = OPERATION + bytes([byte]) + u32(len(description)) + description
    if name == "authorize":
        data += u32(32) + record_id + u32(32) + sha(record)
    print(f"hash_{name}", sha(data + random).hex())
