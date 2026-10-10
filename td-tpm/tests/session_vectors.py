"""Optional public session-vector generator; Python stdlib only.

Usage: python3 session_vectors.py > session_vectors.txt
Independent of td: hashlib, hmac, integer P-256 arithmetic and a table-free
AES-128 written here from TPM 2.0 Part 1 (11.4.10 KDFa and KDFe, 19.6
session keys and HMACs, 21 CFB parameter encryption), SEC 1 ECDH and
FIPS 197. No network. Not a build/test/runtime dependency. Every scalar,
nonce and authValue below is a deterministic PUBLIC fixture. The script
checks its AES against FIPS 197 C.1 and SP 800-38A F.3.13 before it
prints anything. Each output line is `name hex`.
"""
import hashlib
import hmac
import struct

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


def multiply(k, point):
    out = None
    while k:
        if k & 1:
            out = add(out, point)
        point = add(point, point)
        k >>= 1
    return out


def scalar(label):
    raw = int.from_bytes(sha(b"td-tpm/session-vector/" + label), "big")
    return raw % (N - 1) + 1


def sha(data):
    return hashlib.sha256(data).digest()


def be32(value):
    return struct.pack(">I", value)


def be16(value):
    return struct.pack(">H", value)


def blob(data):
    return be16(len(data)) + data


# AES-128 encryption, FIPS 197, computed without lookup tables.
def xtime(a):
    a <<= 1
    return (a ^ 0x11B) & 0xFF if a & 0x100 else a


def gmul(a, b):
    out = 0
    while b:
        if b & 1:
            out ^= a
        a = xtime(a)
        b >>= 1
    return out


def sbox(x):
    inverse = 0
    for candidate in range(1, 256):
        if x and gmul(x, candidate) == 1:
            inverse = candidate
    out = 0x63
    for shift in range(5):
        out ^= ((inverse << shift) | (inverse >> (8 - shift))) & 0xFF
    return out


SBOX = [sbox(x) for x in range(256)]


def expand(key):
    words = [list(key[i:i + 4]) for i in range(0, 16, 4)]
    rcon = 1
    for i in range(4, 44):
        temp = list(words[i - 1])
        if i % 4 == 0:
            temp = [SBOX[b] for b in temp[1:] + temp[:1]]
            temp[0] ^= rcon
            rcon = xtime(rcon)
        words.append([a ^ b for a, b in zip(words[i - 4], temp)])
    return [sum(words[r * 4:r * 4 + 4], []) for r in range(11)]


def aes128(key, block):
    rounds = expand(key)
    state = [a ^ b for a, b in zip(block, rounds[0])]
    for r in range(1, 11):
        state = [SBOX[b] for b in state]
        state = [state[(i + 4 * (i % 4)) % 16] for i in range(16)]
        if r != 10:
            mixed = []
            for c in range(4):
                a = state[4 * c:4 * c + 4]
                mixed += [
                    gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3],
                    a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3],
                    a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3),
                    gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2),
                ]
            state = mixed
        state = [a ^ b for a, b in zip(state, rounds[r])]
    return bytes(state)


def cfb(key, iv, data, encrypt):
    out = b""
    feedback = iv
    for at in range(0, len(data), 16):
        chunk = data[at:at + 16]
        pad = aes128(key, feedback)
        done = bytes(a ^ b for a, b in zip(chunk, pad))
        out += done
        feedback = done if encrypt else chunk
    return out


assert aes128(bytes(range(16)), bytes.fromhex(
    "00112233445566778899aabbccddeeff")).hex() == "69c4e0d86a7b0430d8cdb78070b4c55a"
SP800_PLAIN = bytes.fromhex(
    "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51"
    "30c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710")
SP800_CIPHER = bytes.fromhex(
    "3b3fd92eb72dad20333449f8e83cfb4ac8a64537a0b3a93fcde3cdad9f1ce58b"
    "26751f67a3cbb140b1808cf187a4f4dfc04b05357c5d1c0eeac4c66f9ff7f2e6")
SP800_KEY = bytes.fromhex("2b7e151628aed2a6abf7158809cf4f3c")
assert cfb(SP800_KEY, bytes(range(16)), SP800_PLAIN, True) == SP800_CIPHER
assert cfb(SP800_KEY, bytes(range(16)), SP800_CIPHER, False) == SP800_PLAIN


# TPM 2.0 Part 1 11.4.10.2 and 11.4.10.3, SHA-256, 256 bits: one block,
# the label with its terminating zero octet.
def kdfa(key, label, context_u, context_v):
    return hmac.new(key, be32(1) + label + b"\0" + context_u + context_v + be32(256),
                    hashlib.sha256).digest()


def kdfe(z, label, party_u, party_v):
    return sha(be32(1) + z + label + b"\0" + party_u + party_v)


def trim(auth):
    return auth.rstrip(b"\0")


def session_hmac(key, auth, p_hash, newer, older, attributes):
    return hmac.new(key + trim(auth), p_hash + newer + older + bytes([attributes]),
                    hashlib.sha256).digest()


def cfb_first(key, auth, newer, older, parameters, encrypt):
    derived = kdfa(key + trim(auth), b"CFB", newer, older)
    size = int.from_bytes(parameters[:2], "big")
    body = cfb(derived[:16], derived[16:], parameters[2:2 + size], encrypt)
    return parameters[:2] + body + parameters[2 + size:]


SESSIONS, NO_SESSIONS = 0x8002, 0x8001
CREATE, UNSEAL, START = 0x153, 0x15E, 0x176
POLICY_AUTH_VALUE, POLICY_CC, POLICY_PCR = 0x16B, 0x16C, 0x17F
GET_CAPABILITY = 0x17A
NULL = 0x40000007
PARENT, OBJECT, SESSION = 0x80000000, 0x80000001, 0x03000000


def command(code, handles, area, parameters):
    body = be32(code) + b"".join(be32(h) for h in handles)
    tag = SESSIONS if area is not None else NO_SESSIONS
    if area is not None:
        body += be32(len(area)) + area
    body += parameters
    return be16(tag) + be32(6 + len(body)) + body


def session_command(code, handles, names, key, auth, nonce_tpm, nonce_caller,
                    attributes, parameters):
    if attributes & 0x20:
        parameters = cfb_first(key, auth, nonce_caller, nonce_tpm, parameters, True)
    cp_hash = sha(be32(code) + b"".join(names) + parameters)
    mac = session_hmac(key, auth, cp_hash, nonce_caller, nonce_tpm, attributes)
    area = be32(SESSION) + blob(nonce_caller) + bytes([attributes]) + blob(mac)
    return command(code, handles, area, parameters)


def session_response(code, key, auth, nonce_tpm, nonce_caller, attributes, parameters):
    if attributes & 0x40:
        parameters = cfb_first(key, auth, nonce_tpm, nonce_caller, parameters, True)
    rp_hash = sha(be32(0) + be32(code) + parameters)
    mac = session_hmac(key, auth, rp_hash, nonce_tpm, nonce_caller, attributes)
    body = be32(len(parameters)) + parameters + blob(nonce_tpm) + bytes([attributes]) + blob(mac)
    return be16(SESSIONS) + be32(10 + len(body)) + be32(0) + body


def out(name, data):
    print(name, data.hex())


print("# td-tpm session vectors; generated by session_vectors.py")
primary = multiply(scalar(b"primary"), G)
ephemeral_scalar = scalar(b"ephemeral")
ephemeral = multiply(ephemeral_scalar, G)
assert on_curve(primary) and on_curve(ephemeral)
px, py = (c.to_bytes(32, "big") for c in primary)
ex, ey = (c.to_bytes(32, "big") for c in ephemeral)
z = multiply(ephemeral_scalar, primary)[0].to_bytes(32, "big")
salt = kdfe(z, b"SECRET", ex, px)
out("ephemeral_scalar", ephemeral_scalar.to_bytes(32, "big"))
out("primary_x", px)
out("primary_y", py)
out("ephemeral_x", ex)
out("ephemeral_y", ey)
out("z", z)
out("salt", salt)

# The storage primary's public area, as td-tpm's template returns it.
template = (be16(0x23) + be16(0x0B) + be32(0x30472) + blob(b"")
            + b"".join(be16(v) for v in (6, 128, 0x43, 0x10, 3, 0x10)))
primary_public = template + blob(px) + blob(py)
primary_name = be16(0x0B) + sha(primary_public)
out("primary_public", primary_public)
out("primary_name", primary_name)

# A salted policy session and a salted HMAC session to the primary.
symmetric = be16(6) + be16(128) + be16(0x43) + be16(0x0B)
for kind, label in ((1, b"policy"), (0, b"hmac")):
    nonce_caller = sha(b"td-tpm/session-vector/" + label + b"/caller")
    nonce_tpm = sha(b"td-tpm/session-vector/" + label + b"/tpm")
    parameters = (blob(nonce_caller) + blob(blob(ex) + blob(ey)) + bytes([kind])
                  + symmetric)
    out(label.decode() + "_start", command(START, [PARENT, NULL], None, parameters))
    out(label.decode() + "_nonce_tpm", nonce_tpm)
    out(label.decode() + "_key", kdfa(salt, b"ATH", nonce_tpm, nonce_caller))
policy_key = kdfa(salt, b"ATH", sha(b"td-tpm/session-vector/policy/tpm"),
                  sha(b"td-tpm/session-vector/policy/caller"))
hmac_key = kdfa(salt, b"ATH", sha(b"td-tpm/session-vector/hmac/tpm"),
                sha(b"td-tpm/session-vector/hmac/caller"))

# The PIN-derived authValue with its trailing zeros, which the TPM removes.
auth = sha(b"td-tpm/session-vector/auth")[:29] + b"\0\0\0"
out("auth", auth)
out("auth_trimmed", trim(auth))

# PolicyPCR over PCRs 4, 9 and 12, PolicyAuthValue, PolicyCommandCode(Unseal).
selection = be32(1) + be16(0x0B) + bytes([3, 0x10, 0x12, 0])
pcr_digest = sha(b"\x44" * 32 + b"\x49" * 32 + b"\0" * 32)
digest = sha(b"\0" * 32 + be32(POLICY_PCR) + selection + pcr_digest)
digest = sha(digest + be32(POLICY_AUTH_VALUE))
digest = sha(digest + be32(POLICY_CC) + be32(UNSEAL))
out("pcr_digest", pcr_digest)
out("auth_policy", digest)

# CFB both ways under the policy session's value.
caller = sha(b"td-tpm/session-vector/cfb/caller")
tpm = sha(b"td-tpm/session-vector/cfb/tpm")
plain = sha(b"td-tpm/session-vector/cfb/plain") + b"tail"
derived = kdfa(policy_key + trim(auth), b"CFB", caller, tpm)
out("cfb_caller", caller)
out("cfb_tpm", tpm)
out("cfb_plain", plain)
out("cfb_command_key", derived)
out("cfb_command", cfb(derived[:16], derived[16:], plain, True))
derived = kdfa(policy_key + trim(auth), b"CFB", tpm, caller)
out("cfb_response_key", derived)
out("cfb_response", cfb(derived[:16], derived[16:], plain, True))

# Create under the primary in the salted HMAC session, its sensitive area
# encrypted (decrypt), then a reply.
payload = sha(b"td-tpm/session-vector/payload")
public = (be16(8) + be16(0x0B) + be32(0x92) + blob(digest) + be16(0x10) + blob(b""))
sensitive = blob(trim(auth)) + blob(payload)
parameters = blob(sensitive) + blob(public) + blob(b"") + be32(0)
nonce_caller = sha(b"td-tpm/session-vector/create/caller")
out("payload", payload)
out("create_caller", nonce_caller)
out("create", session_command(CREATE, [PARENT], [primary_name], hmac_key, b"",
                              sha(b"td-tpm/session-vector/hmac/tpm"), nonce_caller,
                              0x20, parameters))
# The TPM returns the template with unique set to its own digest.
object_public = public[:-2] + blob(sha(b"td-tpm/session-vector/unique"))
create_reply = blob(b"vector-private") + blob(object_public)
create_tpm = sha(b"td-tpm/session-vector/create/tpm")
out("create_reply_parameters", create_reply)
out("create_reply_tpm", create_tpm)
out("create_reply", session_response(CREATE, hmac_key, b"", create_tpm, nonce_caller,
                                     0x20, create_reply))

# Unseal of that object in the salted policy session with the authValue,
# its reply encrypted (encrypt).
object_name = be16(0x0B) + sha(object_public)
nonce_caller = sha(b"td-tpm/session-vector/unseal/caller")
out("object_template", public)
out("object_public", object_public)
out("unseal_caller", nonce_caller)
out("unseal", session_command(UNSEAL, [OBJECT], [object_name], policy_key, auth,
                              sha(b"td-tpm/session-vector/policy/tpm"), nonce_caller,
                              0x40, b""))
unseal_tpm = sha(b"td-tpm/session-vector/unseal/tpm")
out("unseal_reply_tpm", unseal_tpm)
out("unseal_reply", session_response(UNSEAL, policy_key, auth, unseal_tpm, nonce_caller,
                                     0x40, blob(payload)))

# The commands that carry no session.
out("policy_auth_value", command(POLICY_AUTH_VALUE, [SESSION], None, b""))
out("capability_permanent", command(GET_CAPABILITY, [], None,
                                    be32(6) + be32(0x200) + be32(1)))
out("capability_lockout", command(GET_CAPABILITY, [], None,
                                  be32(6) + be32(0x20E) + be32(4)))
