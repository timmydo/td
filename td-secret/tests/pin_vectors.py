"""Optional public transcript generator; Python stdlib + OpenSSL 3.5.7 only.

Usage: python3 pin_vectors.py /path/to/libcrypto.so
No network or td implementation imports. Not a build/test/runtime dependency.
All private scalars, PINs and tokens below are deterministic PUBLIC fixtures.
"""
import ctypes as c
import hashlib
import hmac
import sys

lib = c.CDLL(sys.argv[1])
ptr, integer = c.c_void_p, c.c_int


def bind(name, result, args):
    fn = getattr(lib, name)
    fn.restype, fn.argtypes = result, args
    return fn


def check(value):
    if not value:
        raise RuntimeError("OpenSSL fixture operation failed")
    return value


bn_from = bind("BN_bin2bn", ptr, [ptr, integer, ptr])
bn_new = bind("BN_new", ptr, [])
bn_bytes = bind("BN_bn2binpad", integer, [ptr, ptr, integer])
bn_free = bind("BN_free", None, [ptr])
ctx_new = bind("BN_CTX_new", ptr, [])
ctx_free = bind("BN_CTX_free", None, [ptr])
nid = bind("OBJ_txt2nid", integer, [c.c_char_p])(b"prime256v1")
group_new = bind("EC_GROUP_new_by_curve_name", ptr, [integer])
group_free = bind("EC_GROUP_free", None, [ptr])
point_new = bind("EC_POINT_new", ptr, [ptr])
point_free = bind("EC_POINT_free", None, [ptr])
mul = bind("EC_POINT_mul", integer, [ptr, ptr, ptr, ptr, ptr, ptr])
coords = bind("EC_POINT_get_affine_coordinates", integer, [ptr, ptr, ptr, ptr, ptr])
key_new = bind("EC_KEY_new_by_curve_name", ptr, [integer])
key_set = bind("EC_KEY_set_public_key", integer, [ptr, ptr])
key_free = bind("EC_KEY_free", None, [ptr])
sig_new = bind("ECDSA_SIG_new", ptr, [])
sig_set = bind("ECDSA_SIG_set0", integer, [ptr, ptr, ptr])
sig_free = bind("ECDSA_SIG_free", None, [ptr])
verify = bind("ECDSA_do_verify", integer, [ptr, integer, ptr, ptr])
aes_new = bind("EVP_CIPHER_CTX_new", ptr, [])
aes_free = bind("EVP_CIPHER_CTX_free", None, [ptr])
aes_cipher = bind("EVP_aes_256_cbc", ptr, [])
aes_init = bind("EVP_EncryptInit_ex", integer, [ptr, ptr, ptr, ptr, ptr])
aes_pad = bind("EVP_CIPHER_CTX_set_padding", integer, [ptr, integer])
aes_update = bind("EVP_EncryptUpdate", integer, [ptr, ptr, c.POINTER(integer), ptr, integer])
aes_finish = bind("EVP_EncryptFinal_ex", integer, [ptr, ptr, c.POINTER(integer)])
group, ctx = check(group_new(nid)), check(ctx_new())
N = int("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551", 16)


def bn(n):
    return check(bn_from(n.to_bytes(32, "big"), 32, None))


def multiple(n):
    point, scalar = check(point_new(group)), bn(n)
    try:
        check(mul(group, point, scalar, None, None, ctx) == 1)
        return point
    except BaseException:
        point_free(point)
        raise
    finally:
        bn_free(scalar)


def xy(n):
    point, x, y = multiple(n), check(bn_new()), check(bn_new())
    try:
        check(coords(group, point, x, y, ctx) == 1)
        out = []
        for value in [x, y]:
            buf = c.create_string_buffer(32)
            check(bn_bytes(value, buf, 32) == 32)
            out.append(buf.raw)
        return out
    finally:
        point_free(point)
        bn_free(x)
        bn_free(y)


def aes(key, iv, data):
    context = check(aes_new())
    try:
        check(aes_init(context, aes_cipher(), None, key, iv) == 1)
        check(aes_pad(context, 0) == 1)
        out, size = c.create_string_buffer(len(data) + 16), integer()
        check(aes_update(context, out, c.byref(size), data, len(data)) == 1)
        used = size.value
        check(aes_finish(context, c.byref(out, used), c.byref(size)) == 1)
        check(used + size.value == len(data))
        return out.raw[:len(data)]
    finally:
        aes_free(context)


def sha(data):
    return hashlib.sha256(data).digest()


def mac(key, data):
    return hmac.digest(key, data, "sha256")


def hkdf(shared, info):
    return mac(mac(bytes(32), shared), info + b"\1")


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
        return head(0, value) if value >= 0 else head(1, -1-value)
    if isinstance(value, (bytes, str)):
        data = value.encode() if isinstance(value, str) else value
        return head(3 if isinstance(value, str) else 2, len(data)) + data
    if isinstance(value, list):
        return head(4, len(value)) + b"".join(map(cbor, value))
    if isinstance(value, dict):
        pairs = sorted((cbor(k), cbor(v)) for k, v in value.items())
        return head(5, len(pairs)) + b"".join(k+v for k, v in pairs)
    raise ValueError("unsupported CBOR fixture type")


def cose(n):
    x, y = xy(n)
    return {1: 2, 3: -25, -1: 1, -2: x, -3: y}


def derint(value):
    data = value.to_bytes(32, "big").lstrip(b"\0") or b"\0"
    if data[0] & 128:
        data = b"\0" + data
    return b"\2" + bytes([len(data)]) + data


def sign(private, digest):
    # Deterministic PUBLIC fixture nonce, independently verified by OpenSSL.
    k = int.from_bytes(sha(b"public-pin-signature" + digest), "big") % (N-1) + 1
    r = int.from_bytes(xy(k)[0], "big") % N
    s = (int.from_bytes(digest, "big") + r * private) * pow(k, -1, N) % N
    check(r and s)
    point, key, sig = multiple(private), check(key_new(nid)), check(sig_new())
    try:
        check(key_set(key, point) == 1)
        check(sig_set(sig, bn(r), bn(s)) == 1)
        check(verify(digest, 32, sig, key) == 1)
    finally:
        point_free(point)
        key_free(key)
        sig_free(sig)
    body = derint(r) + derint(s)
    return b"\x30" + bytes([len(body)]) + body


print("# Public complete PIN/hmac-secret transcripts; pin_vectors.py, OpenSSL 3.5.7")
try:
    for protocol, permissions, token_size in [(1, False, 16), (1, True, 32), (2, False, 32), (2, True, 32)]:
        label = f"p{protocol}-{'scoped' if permissions else 'legacy'}"
        seed = label.encode()
        scalar = int.from_bytes(sha(seed + b"scalar"), "big") % (N-1) + 1
        peer = int.from_bytes(sha(seed + b"peer"), "big") % (N-1) + 1
        signing = int.from_bytes(sha(seed + b"credential"), "big") % (N-1) + 1
        shared = xy(scalar * peer % N)[0]
        aes_key = sha(shared) if protocol == 1 else hkdf(shared, b"CTAP2 AES key")
        hmac_key = sha(shared) if protocol == 1 else hkdf(shared, b"CTAP2 HMAC key")
        pin = b" 1234 printable PIN! "
        token = sha(seed + b"token")[:token_size]
        salt, challenge, output = [sha(seed + name) for name in [b"salt", b"challenge", b"output"]]
        iv_pin, iv_salt, iv_token, iv_output = [sha(seed + name)[:16] for name in [b"iv-pin", b"iv-salt", b"iv-token", b"iv-output"]]
        def enc(data, iv):
            return aes(aes_key, bytes(16), data) if protocol == 1 else iv + aes(aes_key, iv, data)
        def auth(key, data):
            return mac(key, data)[:16 if protocol == 1 else 32]
        salt_enc = enc(salt, iv_salt)
        ext = {1: cose(scalar), 2: salt_enc, 3: auth(hmac_key, salt_enc)}
        if protocol == 2:
            ext[4] = 2
        get = {1: "td.invalid", 2: challenge, 3: [{"id": b"fixture-id", "type": "public-key"}],
               4: {"hmac-secret": ext}, 5: {"up": True}, 6: auth(token, challenge), 7: protocol}
        client = {1: protocol, 2: 9 if permissions else 5, 3: cose(scalar), 6: enc(sha(pin)[:16], iv_pin)}
        if permissions:
            client.update({9: 2, 10: "td.invalid"})
        info = {1: ["FIDO_2_1"], 2: ["hmac-secret"], 3: bytes(16),
                4: {"clientPin": True, "pinUvAuthToken": permissions}, 6: [protocol]}
        x, y = xy(signing)
        rows = dict(info=b"\0"+cbor(info), scalar=scalar.to_bytes(32, "big"), pin=pin,
                    key_response=b"\0"+cbor({1: cose(peer)}), key_request=b"\6"+cbor({1: protocol, 2: 2}),
                    pin_request=b"\6"+cbor(client), pin_response=b"\0"+cbor({2: enc(token, iv_token)}),
                    assertion=b"\2"+cbor(get), iv_pin=iv_pin, iv_salt=iv_salt,
                    salt=salt, challenge=challenge, output=output, x=x, y=y,
                    shared=shared, aes=aes_key, hmac=hmac_key)
        encrypted = enc(output, iv_output)
        for name, flags, extension, rp in [
                ("response", 0x85, {"hmac-secret": encrypted}, "td.invalid"),
                ("no_uv", 0x81, {"hmac-secret": encrypted}, "td.invalid"),
                ("no_up", 0x84, {"hmac-secret": encrypted}, "td.invalid"),
                ("missing", 0x05, None, "td.invalid"),
                ("wrong_extension", 0x85, {"other": encrypted}, "td.invalid"),
                ("short_output", 0x85, {"hmac-secret": encrypted[:-16]}, "td.invalid"),
                ("wrong_rp", 0x85, {"hmac-secret": encrypted}, "example.com")]:
            data = sha(rp.encode()) + bytes([flags]) + (7).to_bytes(4, "big")
            if extension is not None:
                data += cbor(extension)
            rows[name] = b"\0" + cbor({1: {"id": b"fixture-id", "type": "public-key"},
                                        2: data, 3: sign(signing, sha(data + challenge))})
        # Enrollment uses a separate PIN transaction from its subsequent proof.
        create_scalar = int.from_bytes(sha(seed + b"create-scalar"), "big") % (N-1) + 1
        create_peer = int.from_bytes(sha(seed + b"create-peer"), "big") % (N-1) + 1
        create_shared = xy(create_scalar * create_peer % N)[0]
        create_aes = sha(create_shared) if protocol == 1 else hkdf(create_shared, b"CTAP2 AES key")
        create_token = sha(seed + b"create-token")[:token_size]
        create_challenge, user = sha(seed + b"create-challenge"), sha(seed + b"user")
        create_iv_pin, create_iv_token = sha(seed + b"create-iv-pin")[:16], sha(seed + b"create-iv-token")[:16]
        def create_enc(data, iv):
            return aes(create_aes, bytes(16), data) if protocol == 1 else iv + aes(create_aes, iv, data)
        create_client = {1: protocol, 2: 9 if permissions else 5, 3: cose(create_scalar),
                         6: create_enc(sha(pin)[:16], create_iv_pin)}
        if permissions:
            create_client.update({9: 1, 10: "td.invalid"})
        make = {1: create_challenge, 2: {"id": "td.invalid", "name": "td personal vault"},
                3: {"id": user, "name": "td personal vault", "displayName": "td personal vault"},
                4: [{"alg": -7, "type": "public-key"}], 6: {"hmac-secret": True},
                7: {"rk": False}, 8: auth(create_token, create_challenge), 9: protocol}
        credential_id = b"enrolled-" + seed
        public = cose(signing)
        public[3] = -7
        data = sha(b"td.invalid") + b"\xc5" + (0).to_bytes(4, "big") + bytes(16)
        data += len(credential_id).to_bytes(2, "big") + credential_id + cbor(public) + cbor({"hmac-secret": True})
        rows.update(create_scalar=create_scalar.to_bytes(32, "big"), create_iv_pin=create_iv_pin,
                    create_challenge=create_challenge, user=user, credential_id=credential_id, cose=cbor(public),
                    create_key_response=b"\0"+cbor({1: cose(create_peer)}),
                    create_pin_request=b"\6"+cbor(create_client),
                    create_pin_response=b"\0"+cbor({2: create_enc(create_token, create_iv_token)}),
                    make_request=b"\1"+cbor(make),
                    make_none=b"\0"+cbor({1: "none", 2: data}),
                    make_packed=b"\0"+cbor({1: "packed", 2: data,
                        3: {"alg": -7, "sig": sign(signing, sha(data + create_challenge))}}))
        make[5] = [{"id": b"prior-primary", "type": "public-key"},
                   {"id": b"prior-backup", "type": "public-key"}]
        rows["make_excluded"] = b"\1"+cbor(make)
        get[3] = [{"id": credential_id, "type": "public-key"}]
        rows["enroll_assertion"] = b"\2"+cbor(get)
        for name, flags, extension in [
                ("enroll_response", 0x85, {"hmac-secret": encrypted}),
                ("enroll_be", 0x8d, {"hmac-secret": encrypted}),
                ("enroll_bs", 0x9d, {"hmac-secret": encrypted}),
                ("enroll_no_uv", 0x81, {"hmac-secret": encrypted}),
                ("enroll_missing", 0x05, None),
                ("enroll_short", 0x85, {"hmac-secret": encrypted[:-16]})]:
            data = sha(b"td.invalid") + bytes([flags]) + (7).to_bytes(4, "big")
            if extension is not None:
                data += cbor(extension)
            rows[name] = b"\0"+cbor({1: {"id": credential_id, "type": "public-key"},
                                      2: data, 3: sign(signing, sha(data + challenge))})
        for key, value in rows.items():
            print(label, key, value.hex())
finally:
    ctx_free(ctx)
    group_free(group)
