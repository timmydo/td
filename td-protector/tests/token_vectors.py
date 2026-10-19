"""Independent vectors for td-protector's protected-tier tokens; stdlib only.

Usage: python3 -I token_vectors.py
No network or td implementation imports. Not a build/test/runtime
dependency: src/protected.rs, src/lockout.rs, src/update_retry.rs and
src/request.rs pin the lines it prints
as literals. Every input below is a public fixture.

Lockout token (td-install/ENCRYPTION.md, "PIN and dictionary-attack
policy"): 64 bytes of RFC 5869 HKDF-SHA256 with the volume key as input
keying material, an empty salt and the info b"td/disk-protector/lockout/v1",
one zero byte, the volume's 16-byte UUID and the nonce; k_enc is the first
32 bytes and k_mac the last. The ciphertext is L XOR k_enc and the tag
HMAC-SHA256 under k_mac over the nonce, the ciphertext and the Name;
"proven", "0" or "1", lies outside it and a fresh token carries "0".

Update-retry token (td-install/ENCRYPTION.md, "Selector update"): the key
is 32 bytes of the same HKDF with the info
b"td/disk-protector/update-retry/v1", one zero byte and the UUID; "pair" is
the SHA-256 of the staged kernel's SHA-256 followed by the staged
initramfs's, and "tag" HMAC-SHA256 under the key over "pair".

Request token (td-install/ENCRYPTION.md, "Request tag"): the key is 32
bytes of the same HKDF with the info b"td/disk-protector/request/v1", one
zero byte, the UUID and the request's nonce; "tag" is HMAC-SHA256 under it
over one length byte and the operation's name, the nonce, the lockout
token's nonce (32 zero bytes without one) and the SHA-256 over every
tpm-pin token's public area in token-number order, each preceded by its
length as two big-endian bytes.

Token text: compact JSON, keys in td-protector/DESIGN.md's order, every
byte field lowercase hexadecimal. Printed are the fixture tokens' exact
texts and the lengths of each kind's largest encoding.
"""
import hashlib
import hmac
import json

DOMAIN = b"td/disk-protector/lockout/v1"


def hkdf(ikm, salt, info, length):
    prk = hmac.new(salt, ikm, hashlib.sha256).digest()
    block, out = b"", b""
    counter = 1
    while len(out) < length:
        block = hmac.new(prk, block + info + bytes([counter]), hashlib.sha256).digest()
        out += block
        counter += 1
    return out[:length]


def text(pairs):
    return json.dumps(dict(pairs), separators=(",", ":"))


def slots(slot):
    return [] if slot is None else [str(slot)]


def tpm_pin(slot, pcrs, primary, salt, public, private):
    return text([
        ("type", "td-protector"), ("keyslots", slots(slot)), ("role", "tpm-pin"),
        ("account", "1000"), ("pcrs", pcrs), ("primary", primary.hex()),
        ("salt", salt.hex()), ("public", public.hex()), ("private", private.hex()),
    ])


def fido2(slot, role, credential, salt, x, y):
    return text([
        ("type", "td-protector"), ("keyslots", slots(slot)), ("role", role),
        ("account", "1000"), ("credential", credential.hex()), ("salt", salt.hex()),
        ("x", x.hex()), ("y", y.hex()),
    ])


def marker(slot):
    return text([("type", "td-protector"), ("keyslots", slots(slot)), ("role", "recovery-key")])


def lockout(primary, nonce, ciphertext, tag, proven):
    return text([
        ("type", "td-protector"), ("keyslots", []), ("role", "lockout"),
        ("primary", primary.hex()), ("nonce", nonce.hex()),
        ("ciphertext", ciphertext.hex()), ("tag", tag.hex()),
        ("proven", "1" if proven else "0"),
    ])


def update_retry(pair, tag):
    return text([
        ("type", "td-protector"), ("keyslots", []), ("role", "update-retry"),
        ("pair", pair.hex()), ("tag", tag.hex()),
    ])


def upgrade(shape, source):
    return text([("type", "td-upgrade"), ("keyslots", []), ("shape", shape), ("from", source.hex())])


def request(operation, nonce, tag):
    return text([
        ("type", "td-request"), ("keyslots", []), ("operation", operation),
        ("nonce", nonce.hex()), ("tag", tag.hex()),
    ])


REQUEST_DOMAIN = b"td/disk-protector/request/v1"


def request_tag(volume_key, uuid, operation, nonce, lockout_nonce, publics):
    key = hkdf(volume_key, b"", REQUEST_DOMAIN + b"\x00" + uuid + nonce, 32)
    pins = hashlib.sha256(b"".join(len(p).to_bytes(2, "big") + p for p in publics)).digest()
    name = operation.encode()
    return hmac.new(key, bytes([len(name)]) + name + nonce + lockout_nonce + pins, hashlib.sha256).digest()


volume_key = bytes(range(64))
uuid = bytes.fromhex("0f0e0d0c0b0a49088706050403020100")
nonce = bytes(range(0xA0, 0xC0))
auth = bytes(range(0xC0, 0xE0))
primary = bytes.fromhex("000b") + b"\x11" * 32
okm = hkdf(volume_key, b"", DOMAIN + b"\x00" + uuid + nonce, 64)
ciphertext = bytes(a ^ k for a, k in zip(auth, okm[:32]))
tag = hmac.new(okm[32:], nonce + ciphertext + primary, hashlib.sha256).digest()
print(f"lockout ciphertext {ciphertext.hex()}")
print(f"lockout tag {tag.hex()}")

RETRY_DOMAIN = b"td/disk-protector/update-retry/v1"
kernel = bytes(range(0x10, 0x30))
initrd = bytes(range(0x30, 0x50))
retry_key = hkdf(volume_key, b"", RETRY_DOMAIN + b"\x00" + uuid, 32)
pair = hashlib.sha256(kernel + initrd).digest()
retry_tag = hmac.new(retry_key, pair, hashlib.sha256).digest()
print(f"update-retry pair {pair.hex()}")
print(f"update-retry tag {retry_tag.hex()}")

request_nonce = bytes(range(0x80, 0xA0))
publics = [bytes.fromhex("0008abcd"), bytes.fromhex("0006a1b2c3")]
request_tag_value = request_tag(volume_key, uuid, "pin-change", request_nonce, nonce, publics)
bare_tag = request_tag(volume_key, uuid, "tpm-reenroll", request_nonce, b"\x00" * 32, [])
print(f"request tag {request_tag_value.hex()}")
print(f"request bare tag {bare_tag.hex()}")

fixtures = [
    ("tpm-pin", tpm_pin(2, "4,9", primary, b"\x22" * 32, bytes.fromhex("0008abcd"), bytes.fromhex("0102fe"))),
    ("fido2", fido2(3, "fido2-primary", b"\x33" * 16, b"\x44" * 32, b"\x55" * 32, b"\x66" * 32)),
    ("pending", fido2(None, "fido2-recovery", b"\x33" * 16, b"\x44" * 32, b"\x55" * 32, b"\x66" * 32)),
    ("marker", marker(0)),
    ("lockout", lockout(primary, nonce, ciphertext, tag, False)),
    ("upgrade", upgrade("tpm", b"\x77" * 32)),
    ("request", request("pin-change", request_nonce, request_tag_value)),
    ("update-retry", update_retry(pair, retry_tag)),
]
for name, token in fixtures:
    print(f"token {name} {token}")

largest = [
    ("tpm-pin", tpm_pin(31, "4,7,9", b"\xff" * 34, b"\xff" * 32, b"\xff" * 128, b"\xff" * 256)),
    ("fido2", fido2(31, "fido2-recovery", b"\xff" * 255, b"\xff" * 32, b"\xff" * 32, b"\xff" * 32)),
    ("marker", marker(31)),
    ("lockout", lockout(b"\xff" * 34, b"\xff" * 32, b"\xff" * 32, b"\xff" * 32, True)),
    ("upgrade", upgrade("fido2", b"\xff" * 32)),
    ("request", request("tpm-reenroll", b"\xff" * 32, b"\xff" * 32)),
    ("update-retry", update_retry(b"\xff" * 32, b"\xff" * 32)),
]
for name, token in largest:
    print(f"largest {name} {len(token)}")
