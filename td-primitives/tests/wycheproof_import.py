"""Convert Wycheproof JSON into the flat rows tests/vectors.rs reads.

Wycheproof (https://github.com/C2SP/wycheproof, Apache-2.0) is the input;
THIRD-PARTY.md names the commit. Run from this directory, with the JSON files
in one directory, none of it executed:

    python3 -I wycheproof_import.py DIR

Each row is space-separated, hex fields with "-" for an empty one, ending in
Wycheproof's verdict (valid, invalid or acceptable). A group the primitive's
API cannot express, such as a 128-bit GCM nonce, is dropped and counted here,
never in the test: the printed counts are the coverage statement.
"""

import json
import os
import sys


def field(value):
    return value if value else "-"


def aead(directory, name, key_bits):
    with open(os.path.join(directory, name)) as handle:
        data = json.load(handle)
    rows, dropped = [], 0
    for group in data["testGroups"]:
        usable = (
            group["keySize"] in key_bits
            and group["ivSize"] == 96
            and group["tagSize"] == 128
        )
        for test in group["tests"]:
            if not usable:
                dropped += 1
                continue
            rows.append(
                " ".join(
                    field(test[k]) for k in ("key", "iv", "aad", "msg", "ct", "tag")
                )
                + " "
                + test["result"]
            )
    return rows, dropped


def write(out, rows):
    with open(out, "w") as handle:
        handle.write("\n".join(rows) + "\n")


def main():
    directory = sys.argv[1]
    for name, out, bits in (
        ("aes_gcm_test.json", "wycheproof/aes_gcm.txt", (128, 256)),
        ("chacha20_poly1305_test.json", "wycheproof/chacha20_poly1305.txt", (256,)),
    ):
        rows, dropped = aead(directory, name, bits)
        write(out, rows)
        print(f"{out}: {len(rows)} rows, {dropped} dropped")


if __name__ == "__main__":
    main()
