"""Optional public login CTAP vector generator; Python stdlib only.

Usage: python3 login_ctap_vectors.py > login_ctap_vectors.txt
Independent of td: hashlib, hmac and a CBOR encoder written here. No
network. Not a build/test/runtime dependency. Every ID, hash and token
below is a deterministic PUBLIC fixture. The creation rows reuse
pin_vectors.py's seed derivations, recomputed here, so they replace only
the portable display labels in that file's make requests.
Byte layout: td-login/TOKEN-LOGIN.md, "Token profile".
"""
import hashlib
import hmac


def sha(data):
    return hashlib.sha256(data).digest()


def head(major, n):
    if n < 24:
        return bytes([(major << 5) | n])
    for code, size in [(24, 1), (25, 2), (26, 4), (27, 8)]:
        if n < 1 << (8 * size):
            return bytes([(major << 5) | code]) + n.to_bytes(size, "big")
    raise ValueError("CBOR integer overflow")


def cbor(value):
    if isinstance(value, bool):
        return bytes([0xf5 if value else 0xf4])
    if isinstance(value, int):
        return head(0, value) if value >= 0 else head(1, -1 - value)
    if isinstance(value, (bytes, str)):
        data = value.encode() if isinstance(value, str) else value
        return head(3 if isinstance(value, str) else 2, len(data)) + data
    if isinstance(value, list):
        return head(4, len(value)) + b"".join(map(cbor, value))
    if isinstance(value, dict):
        pairs = sorted((cbor(k), cbor(v)) for k, v in value.items())
        return head(5, len(pairs)) + b"".join(k + v for k, v in pairs)
    raise ValueError("unsupported CBOR fixture type")


def descriptor(credential):
    return {"id": credential, "type": "public-key"}


rows = []
# Identify: a silent getAssertion, up=false, no PIN and no extension.
ids = {"a": b"login-key-a", "b": b"login-key-b", "c": b"login-key-c", "z": b"login-key-z"}
client = sha(b"login-ctap/identify")
rows.append(("identify", "hash", client))
for name in ["a", "b", "c", "ab", "abc"]:
    request = {1: "td.invalid", 2: client, 3: [descriptor(ids[i]) for i in name], 5: {"up": False}}
    rows.append(("identify", "request_" + name, b"\x02" + cbor(request)))
signature = b"\x30\x06\x02\x01\x01\x02\x01\x02"  # Never verified: identify selects only.
for name, chosen, flags in [("select_b", "b", 0x00), ("select_c", "c", 0x00),
                            ("select_omitted", None, 0x00), ("select_outside", "z", 0x00),
                            ("select_up", "b", 0x01), ("select_uv", "b", 0x04)]:
    data = sha(b"td.invalid") + bytes([flags]) + (9).to_bytes(4, "big")
    response = {2: data, 3: signature}
    if chosen is not None:
        response[1] = descriptor(ids[chosen])
    rows.append(("identify", name, b"\0" + cbor(response)))

# getPINRetries (clientPIN 0x01) and its pinRetries/powerCycleState reply.
for name, reply in [("eight", {3: 8}), ("five", {3: 5, 4: False}),
                    ("zero", {3: 0}), ("power", {3: 3, 4: True})]:
    rows.append(("retries", name, b"\0" + cbor(reply)))

for protocol, permissions, token_size in [(1, False, 16), (1, True, 32), (2, False, 32), (2, True, 32)]:
    label = f"p{protocol}-{'scoped' if permissions else 'legacy'}"
    seed = label.encode()
    rows.append((label, "retries_request", b"\x06" + cbor({1: protocol, 2: 1})))
    # pin_vectors.py's creation token, challenge and user handle for this label.
    create_token = sha(seed + b"create-token")[:token_size]
    create_challenge, user = sha(seed + b"create-challenge"), sha(seed + b"user")
    auth = hmac.digest(create_token, create_challenge, "sha256")[:16 if protocol == 1 else 32]
    make = {1: create_challenge, 2: {"id": "td.invalid", "name": "td login"},
            3: {"id": user, "name": "td login", "displayName": "td login"},
            4: [{"alg": -7, "type": "public-key"}], 6: {"hmac-secret": True},
            7: {"rk": False}, 8: auth, 9: protocol}
    rows.append((label, "login_make_request", b"\x01" + cbor(make)))
    make[5] = [descriptor(b"prior-primary"), descriptor(b"prior-backup")]
    rows.append((label, "login_make_excluded", b"\x01" + cbor(make)))

print("# Public login CTAP vectors; login_ctap_vectors.py, Python stdlib only")
for label, name, value in rows:
    print(label, name, value.hex())
