"""Convert Wycheproof JSON into the flat rows tests/vectors.rs reads.

Wycheproof (https://github.com/C2SP/wycheproof, Apache-2.0) is the input;
THIRD-PARTY.md names the commit. Run from this directory, with the JSON files
in one directory, none of it executed:

    python3 -I wycheproof_import.py DIR

Rows are space-separated, hex fields with "-" for an empty one, ending in
Wycheproof's verdict (valid, invalid or acceptable). "K" rows name the key the
"T" rows after them use. A group the primitive's API cannot express, such as a
128-bit GCM nonce, is dropped and counted here, never in the test: the printed
counts are the coverage statement.
"""

import json
import os
import sys


def field(value):
    return value if value else "-"


def load(directory, name):
    with open(os.path.join(directory, name + "_test.json")) as handle:
        return json.load(handle)


def write(out, rows):
    with open(out, "w") as handle:
        handle.write("\n".join(rows) + "\n")


def report(out, rows, dropped):
    tests = [row for row in rows if not row.startswith("K ")]
    verdicts = {}
    for row in tests:
        verdict = row.rsplit(" ", 1)[1]
        verdicts[verdict] = verdicts.get(verdict, 0) + 1
    print(f"{out}: {len(tests)} tests {sorted(verdicts.items())}, {dropped} dropped")


def aead(directory, name, key_bits):
    rows, dropped = [], 0
    for group in load(directory, name)["testGroups"]:
        usable = (
            group["keySize"] in key_bits
            and group["ivSize"] == 96
            and group["tagSize"] == 128
        )
        for test in group["tests"]:
            if not usable:
                dropped += 1
                continue
            fields = [field(test[k]) for k in ("key", "iv", "aad", "msg", "ct", "tag")]
            rows.append(" ".join(fields + [test["result"]]))
    return rows, dropped


def key_exchange(directory, name):
    rows = []
    for group in load(directory, name)["testGroups"]:
        for test in group["tests"]:
            fields = [field(test[k]) for k in ("private", "public", "shared")]
            rows.append(" ".join(fields + [test["result"]]))
    return rows, 0


def signature(directory, name, key_of, label):
    rows = []
    for group in load(directory, name)["testGroups"]:
        rows.append("K " + label + " " + key_of(group))
        for test in group["tests"]:
            rows.append(
                f"T {field(test['msg'])} {field(test['sig'])} {test['result']}"
            )
    return rows, 0


def ed25519_key(group):
    return group["publicKey"]["pk"]


def ecdsa_key(group):
    return group["publicKey"]["uncompressed"]


def rsa_key(group):
    return group["publicKey"]["modulus"] + " " + group["publicKey"]["publicExponent"]


def compressed(uncompressed):
    """SEC 1's compressed form of an uncompressed point: x, tagged by y's parity."""
    width = (len(uncompressed) - 2) // 4
    x, y = uncompressed[2 : 2 + 2 * width], uncompressed[2 + 2 * width :]
    return ("03" if int(y[-1], 16) & 1 else "02") + x


def points(directory, curves):
    """Rows of curve, encoded point, its validated uncompressed form or "-", verdict.

    Every distinct ECDSA key goes in under both encodings, its uncompressed form
    being Wycheproof's. From the ECDH point files come the valid uncompressed keys
    and every invalid point: off the curve, out of range, empty, or an x with no
    square root, which the wrong-curve compressed rows are too, being points on
    the twist. The one compressed key each file takes is dropped, as the file
    does not state its uncompressed form.
    """
    rows, seen, dropped = [], set(), 0

    def add(curve, encoded, result, verdict):
        if (curve, encoded) not in seen:
            seen.add((curve, encoded))
            rows.append(f"{curve} {field(encoded)} {result} {verdict}")

    for curve, bits, ecdsa_names in curves:
        for name in ecdsa_names:
            for group in load(directory, name)["testGroups"]:
                key = group["publicKey"]["uncompressed"]
                add(curve, key, key, "valid")
                add(curve, compressed(key), key, "valid")
        for group in load(directory, f"ecdh_secp{bits}r1_ecpoint")["testGroups"]:
            for test in group["tests"]:
                public = test["public"]
                if test["result"] == "valid" and public.startswith("04"):
                    add(curve, public, public, "valid")
                elif test["result"] == "invalid":
                    add(curve, public, "-", "invalid")
                else:
                    dropped += 1
    return rows, dropped


def ec_points(directory, _name):
    return points(
        directory,
        [
            ("p256", 256, ["ecdsa_secp256r1_sha256", "ecdsa_secp256r1_sha512"]),
            (
                "p384",
                384,
                ["ecdsa_secp384r1_sha256", "ecdsa_secp384r1_sha384", "ecdsa_secp384r1_sha512"],
            ),
            ("p521", 521, ["ecdsa_secp521r1_sha512"]),
        ],
    )


def main():
    directory = sys.argv[1]
    os.makedirs("wycheproof", exist_ok=True)
    outputs = [
        ("wycheproof/aes_gcm.txt", [("aes_gcm", aead, ((128, 256),))]),
        ("wycheproof/chacha20_poly1305.txt", [("chacha20_poly1305", aead, ((256,),))]),
        ("wycheproof/x25519.txt", [("x25519", key_exchange, ())]),
        ("wycheproof/ecdh_p256.txt", [("ecdh_secp256r1_ecpoint", key_exchange, ())]),
        ("wycheproof/ecdh_p384.txt", [("ecdh_secp384r1_ecpoint", key_exchange, ())]),
        ("wycheproof/ed25519.txt", [("ed25519", signature, (ed25519_key, "ed25519"))]),
        (
            "wycheproof/ecdsa.txt",
            [
                ("ecdsa_secp256r1_sha256", signature, (ecdsa_key, "p256 sha256")),
                ("ecdsa_secp384r1_sha256", signature, (ecdsa_key, "p384 sha256")),
                ("ecdsa_secp384r1_sha384", signature, (ecdsa_key, "p384 sha384")),
                ("ecdsa_secp256r1_sha512", signature, (ecdsa_key, "p256 sha512")),
                ("ecdsa_secp384r1_sha512", signature, (ecdsa_key, "p384 sha512")),
                ("ecdsa_secp521r1_sha512", signature, (ecdsa_key, "p521 sha512")),
            ],
        ),
        ("wycheproof/ec_points.txt", [(None, ec_points, ())]),
        (
            "wycheproof/rsa_pkcs1.txt",
            [
                ("rsa_signature_2048_sha256", signature, (rsa_key, "sha256")),
                ("rsa_signature_2048_sha384", signature, (rsa_key, "sha384")),
                ("rsa_signature_2048_sha512", signature, (rsa_key, "sha512")),
                ("rsa_signature_3072_sha256", signature, (rsa_key, "sha256")),
                ("rsa_signature_4096_sha512", signature, (rsa_key, "sha512")),
            ],
        ),
        (
            "wycheproof/rsa_pss.txt",
            [
                ("rsa_pss_2048_sha256_mgf1_32", signature, (rsa_key, "sha256")),
                ("rsa_pss_2048_sha384_mgf1_48", signature, (rsa_key, "sha384")),
                ("rsa_pss_3072_sha256_mgf1_32", signature, (rsa_key, "sha256")),
                ("rsa_pss_4096_sha512_mgf1_64", signature, (rsa_key, "sha512")),
            ],
        ),
    ]
    for out, sources in outputs:
        rows, dropped = [], 0
        for name, convert, arguments in sources:
            more, gone = convert(directory, name, *arguments)
            rows += more
            dropped += gone
        write(out, rows)
        report(out, rows, dropped)


if __name__ == "__main__":
    main()
