"""Independent vectors for td-protector's tpm-pin primitives; stdlib only.

Usage: python3 -I pin_vectors.py
No network or td implementation imports. Not a build/test/runtime
dependency: src/pin.rs pins the lines it prints as literals. Every input
below is a public fixture.

authValue: HMAC-SHA256 keyed with the 32-byte salt over
b"td/disk-protector/pin/v1", one zero byte and the PIN (RFC 2104, by
Python's hmac module).

tpm-pin policy: TPM 2.0 Part 3's policy updates from the zero digest,
  PolicyPCR:         H(old || TPM_CC_PolicyPCR || TPML_PCR_SELECTION || pcrDigest)
  PolicyAuthValue:   H(old || TPM_CC_PolicyAuthValue)
  PolicyCommandCode: H(old || TPM_CC_PolicyCommandCode || TPM_CC_Unseal)
where pcrDigest is H over the selected values in ascending PCR order and
PCR 12 is the literal zero.
"""
import hashlib
import hmac
import struct

DOMAIN = b"td/disk-protector/pin/v1"
CC_POLICY_PCR = 0x17F
CC_POLICY_AUTH_VALUE = 0x16B
CC_POLICY_COMMAND_CODE = 0x16C
CC_UNSEAL = 0x15E
ALG_SHA256 = 0x000B


def sha256(data):
    return hashlib.sha256(data).digest()


def auth_value(salt, pin):
    return hmac.new(salt, DOMAIN + b"\x00" + pin, hashlib.sha256).digest()


def selection(pcrs):
    mask = 0
    for pcr in pcrs:
        mask |= 1 << pcr
    # TPML_PCR_SELECTION: count 1, SHA-256, sizeofSelect 3, the bitmap.
    return struct.pack(">IHB", 1, ALG_SHA256, 3) + mask.to_bytes(3, "little")


def policy(values):
    pcrs = sorted(values)
    composite = sha256(b"".join(values[pcr] for pcr in pcrs))
    digest = bytes(32)
    digest = sha256(
        digest + struct.pack(">I", CC_POLICY_PCR) + selection(pcrs) + composite
    )
    digest = sha256(digest + struct.pack(">I", CC_POLICY_AUTH_VALUE))
    digest = sha256(digest + struct.pack(">II", CC_POLICY_COMMAND_CODE, CC_UNSEAL))
    return composite, digest


salt = bytes(range(32))
for name, pin in [
    ("digits", b"123456"),
    ("space", b"pass phrase"),
    ("printable", bytes(range(0x20, 0x5F))),
]:
    assert 6 <= len(pin) <= 63
    print(f"auth {name} {auth_value(salt, pin).hex()}")
print(f"auth other-salt {auth_value(bytes([0xA5]) * 32, b'123456').hex()}")

zero = bytes(32)
selector = {4: b"\x44" * 32, 9: b"\x49" * 32, 12: zero}
secure_boot = dict(selector)
secure_boot[7] = b"\x47" * 32
for name, values in [("selector", selector), ("secure-boot", secure_boot)]:
    composite, digest = policy(values)
    print(f"selection {name} {selection(sorted(values)).hex()}")
    print(f"composite {name} {composite.hex()}")
    print(f"policy {name} {digest.hex()}")
