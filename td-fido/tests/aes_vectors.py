"""Public fixture generator: python3 aes_vectors.py /path/to/libcrypto.so.

Prints key, IV, plaintext and ciphertext hex for comparison with aes_vectors.txt.
OpenSSL 3.5.7 EVP generated the committed fixture. No padding is used.
Neither this script nor its host library is a build or test dependency.
"""

import ctypes as c
import hashlib
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
cipher = bind("EVP_aes_256_cbc", ptr, [])
init = bind("EVP_EncryptInit_ex", integer, [ptr, ptr, ptr, ptr, ptr])
padding = bind("EVP_CIPHER_CTX_set_padding", integer, [ptr, integer])
update = bind("EVP_EncryptUpdate", integer, [ptr, ptr, length, ptr, integer])
finish = bind("EVP_EncryptFinal_ex", integer, [ptr, ptr, length])


def require(success):
    if not success:
        raise RuntimeError("OpenSSL fixture operation failed")


def encrypt(key, iv, plain):
    ctx = new()
    require(ctx)
    try:
        require(init(ctx, cipher(), None, key, iv) == 1)
        require(padding(ctx, 0) == 1)
        out = c.create_string_buffer(len(plain) + 16)
        size = integer()
        require(update(ctx, out, c.byref(size), plain, len(plain)) == 1)
        used = size.value
        require(finish(ctx, c.byref(out, used), c.byref(size)) == 1)
        used += size.value
        require(used == len(plain))
        return out.raw[:used]
    finally:
        free(ctx)


print("# Public OpenSSL 3.5.7 AES-256-CBC fixtures: key iv plaintext ciphertext")
for blocks in range(1, 9):
    seed = b"td-secret/aes-fixture/v1/" + bytes([blocks])
    key = hashlib.sha256(seed + b"key").digest()
    iv = bytes(16) if blocks <= 4 else hashlib.sha256(seed + b"iv").digest()[:16]
    plain = b"".join(hashlib.sha256(seed + bytes([i])).digest() for i in range(4))
    plain = plain[:blocks * 16]
    print(key.hex(), iv.hex(), plain.hex(), encrypt(key, iv, plain).hex())

for byte in [0, 255]:
    key, iv, plain = bytes([byte]) * 32, bytes([byte]) * 16, bytes([byte]) * 128
    print(key.hex(), iv.hex(), plain.hex(), encrypt(key, iv, plain).hex())
