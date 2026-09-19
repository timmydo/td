"""Offline fixture generator: python3 portable_vector.py /path/to/libcrypto.so.

Uses host OpenSSL EVP for AEAD and Python hashlib/hmac for RFC 5869.
Neither this program nor its host library is a target build/test dependency.
The checked-in Rust fixture was generated using OpenSSL 3.5.7.
All keys and plaintext here are deliberately public synthetic test values.
Compare stdout with the hex literal in the Rust test
independent_openssl_and_python_envelope_vector; generation is not gated.
"""

import ctypes as c
import hashlib
import hmac
import struct
import sys

lib = c.CDLL(sys.argv[1])
ptr = c.c_void_p
integer = c.c_int
length = c.POINTER(integer)


def bind(name, result, arguments):
    fn = getattr(lib, name)
    fn.restype = result
    fn.argtypes = arguments
    return fn


new = bind("EVP_CIPHER_CTX_new", ptr, [])
free = bind("EVP_CIPHER_CTX_free", None, [ptr])
cipher = bind("EVP_chacha20_poly1305", ptr, [])
init = bind("EVP_EncryptInit_ex", integer, [ptr, ptr, ptr, ptr, ptr])
update = bind("EVP_EncryptUpdate", integer, [ptr, ptr, length, ptr, integer])
finish = bind("EVP_EncryptFinal_ex", integer, [ptr, ptr, length])
ctrl = bind("EVP_CIPHER_CTX_ctrl", integer, [ptr, integer, integer, ptr])


def require(success):
    if not success:
        raise RuntimeError("OpenSSL fixture operation failed")


def seal(key, nonce, aad, plain):
    ctx = new()
    require(ctx)
    try:
        require(init(ctx, cipher(), None, key, nonce) == 1)
        size = integer()
        require(update(ctx, None, c.byref(size), aad, len(aad)) == 1)
        out = c.create_string_buffer(len(plain) + 16)
        require(update(ctx, out, c.byref(size), plain, len(plain)) == 1)
        used = size.value
        require(finish(ctx, c.byref(out, used), c.byref(size)) == 1)
        used += size.value
        tag = c.create_string_buffer(16)
        require(ctrl(ctx, 0x10, 16, tag) == 1)  # EVP_CTRL_AEAD_GET_TAG
        return out.raw[:used] + tag.raw
    finally:
        free(ctx)


def hkdf(key, salt, info):
    prk = hmac.digest(salt, key, hashlib.sha256)
    return hmac.digest(prk, info + b"\x01", hashlib.sha256)


def field(data):
    return struct.pack(">I", len(data)) + data


# Standard P-256 generator G and its inverse, independent of td code.
x = bytes.fromhex("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296")
y = int("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5", 16)
p = 0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF


def cose(backup):
    ordinate = (p - y if backup else y).to_bytes(32, "big")
    return bytes.fromhex("a5010203262001215820") + x + bytes.fromhex("225820") + ordinate


vault_id = bytes(range(32))
master = bytes(range(32, 64))
header = b"TDVAULT2" + vault_id + struct.pack(">QB", 1, 2)
for index, (credential, role, salt, secret) in enumerate([
    (b"backup", 2, bytes([0xB2]) * 32, bytes([0x22]) * 32),
    (b"primary", 1, bytes([0xA1]) * 32, bytes([0x11]) * 32),
]):
    metadata = bytes([role]) + struct.pack(">H", len(credential)) + credential + cose(role == 2) + salt
    nonce = bytes(range(64 + index * 12, 76 + index * 12))
    aad = b"td-secret/portable/slot/v2\x00" + vault_id + metadata
    key = hkdf(secret, vault_id, b"td-secret/portable/wrap/v2")
    header += metadata + nonce + seal(key, nonce, aad, master)

plain = struct.pack(">I", 1) + bytes([0x33]) * 16 + struct.pack(">Q", 1)
plain += field(b"Email/Personal") + field(b"username: alice\npassword: example\r\n")
nonce = bytes(range(88, 100))
header += nonce + struct.pack(">I", len(plain) + 16)
domain = b"td-secret/portable/body/v2\x00"
envelope = header + seal(hkdf(master, vault_id, domain), nonce, domain + header, plain)
print(envelope.hex())
