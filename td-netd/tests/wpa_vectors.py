"""WPA2-PSK handshake vectors for td-netd's src/wpa/tests.rs.

An authenticator written apart from td-netd's supplicant: hashlib and hmac
for the PSK, PRF and MICs, and the `cryptography` package's RFC 3394 key
wrap for the key data. It prints the frames the authenticator sends and the
exact replies a correct station returns, as Rust constants.

Run: guix shell python python-cryptography -- python3 -I wpa_vectors.py
"""

import hashlib
import hmac

from cryptography.hazmat.primitives.keywrap import aes_key_wrap

PASSPHRASE = b"correct horse battery"
SSID = b"td-test"
AA = bytes.fromhex("020000000001")
SPA = bytes.fromhex("020000000002")
ANONCE = bytes(range(0x10, 0x30))
SNONCE = bytes(range(0x40, 0x60))
GTK = bytes(range(0xa0, 0xb0))
GTK2 = bytes(range(0xc0, 0xd0))
# The AP's beacon RSN element: CCMP group and pairwise, PSK, and
# capabilities a real AP sets (16 PTKSA replay counters).
AP_RSN = bytes.fromhex("30140100000fac040100000fac040100000fac020c00")
STATION_RSN = bytes.fromhex("30140100000fac040100000fac040100000fac020000")

PAIRWISE, INSTALL, ACK, MIC, SECURE, ENCRYPTED = 8, 64, 128, 256, 512, 4096
VERSION = 2


def prf(key, label, data, length):
    out = b""
    i = 0
    while len(out) < length:
        out += hmac.new(key, label + b"\0" + data + bytes([i]), "sha1").digest()
        i += 1
    return out[:length]


def key_frame(protocol, info, replay, nonce, rsc, data, kck):
    body = (
        bytes([2])
        + info.to_bytes(2, "big")
        + (16 if info & ACK and info & PAIRWISE else 0).to_bytes(2, "big")
        + replay.to_bytes(8, "big")
        + nonce
        + bytes(16)
        + rsc
        + bytes(8)
        + bytes(16)
        + len(data).to_bytes(2, "big")
        + data
    )
    frame = bytes([protocol, 3]) + len(body).to_bytes(2, "big") + body
    if kck is not None:
        mic = hmac.new(kck, frame, "sha1").digest()[:16]
        frame = frame[:81] + mic + frame[97:]
    return frame


def reply(protocol, info, replay, nonce, data, kck):
    # A station's replies carry key length zero (12.7.6.3, 12.7.6.5).
    body = (
        bytes([2])
        + info.to_bytes(2, "big")
        + bytes(2)
        + replay.to_bytes(8, "big")
        + nonce
        + bytes(16 + 8 + 8 + 16)
        + len(data).to_bytes(2, "big")
        + data
    )
    frame = bytes([protocol, 3]) + len(body).to_bytes(2, "big") + body
    mic = hmac.new(kck, frame, "sha1").digest()[:16]
    return frame[:81] + mic + frame[97:]


def wrap_key_data(kek, data):
    if len(data) % 8 or len(data) < 16:
        data += b"\xdd"
        data += bytes(-len(data) % 8)
    return aes_key_wrap(kek, data)


def gtk_kde(index, gtk):
    body = bytes([0x00, 0x0F, 0xAC, 1, index, 0]) + gtk
    return bytes([0xDD, len(body)]) + body


def rust(name, data):
    print(f'const {name}: &str = "{data.hex()}";')


pmk = hashlib.pbkdf2_hmac("sha1", PASSPHRASE, SSID, 4096, 32)
data = min(AA, SPA) + max(AA, SPA) + min(ANONCE, SNONCE) + max(ANONCE, SNONCE)
ptk = prf(pmk, b"Pairwise key expansion", data, 48)
kck, kek, tk = ptk[:16], ptk[16:32], ptk[32:]

m1 = key_frame(2, VERSION | PAIRWISE | ACK, 1, ANONCE, bytes(8), b"", None)
m2 = reply(2, VERSION | PAIRWISE | MIC, 1, SNONCE, STATION_RSN, kck)
rsc = bytes.fromhex("0500000000000000")
m3 = key_frame(
    2,
    VERSION | PAIRWISE | INSTALL | ACK | MIC | SECURE | ENCRYPTED,
    2,
    ANONCE,
    rsc,
    wrap_key_data(kek, AP_RSN + gtk_kde(1, GTK)),
    kck,
)
m4 = reply(2, VERSION | PAIRWISE | MIC | SECURE, 2, bytes(32), b"", kck)
g1 = key_frame(
    2,
    VERSION | ACK | MIC | SECURE | ENCRYPTED,
    3,
    bytes(32),
    bytes.fromhex("0900000000000000"),
    wrap_key_data(kek, gtk_kde(2, GTK2)),
    kck,
)
g2 = reply(2, VERSION | MIC | SECURE, 3, bytes(32), b"", kck)

rust("PMK", pmk)
rust("PTK", ptk)
rust("M1", m1)
rust("M2", m2)
rust("M3", m3)
rust("M4", m4)
rust("G1", g1)
rust("G2", g2)
rust("PRF_PREFIX_HI_THERE", prf(bytes([0x0B]) * 20, b"prefix", b"Hi There", 64))
