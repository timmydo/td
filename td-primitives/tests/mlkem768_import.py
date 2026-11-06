"""ML-KEM-768 rows from NIST's ACVP-Server, for tests/ssh_vectors.rs.

    python3 -I mlkem768_import.py KEYGEN.json ENCAPDECAP.json > ssh/mlkem768_acvp.txt

The two arguments are `internalProjection.json` from gen-val/json-files's
ML-KEM-keyGen-FIPS203 and ML-KEM-encapDecap-FIPS203. Every check row is kept. Of the
known answers, the first few of each function are: the rest run the same code on
other random inputs, and a row costs up to 7 KiB.
"""

import json
import sys

KEYGEN_ROWS = 4
ENCAPS_ROWS = 4
DECAPS_ROWS = 6
HEADER = """\
# NIST ACVP-Server gen-val/json-files ML-KEM-keyGen-FIPS203 and
# ML-KEM-encapDecap-FIPS203 internalProjection.json, vsId 42, ML-KEM-768 rows only:
# keygen d z ek dk | encaps ek m c k | decaps dk c k reason |
# ekcheck ek pass reason | dkcheck dk pass reason."""


def groups(path, function=None):
    with open(path, encoding="utf-8") as file:
        document = json.load(file)
    for group in document["testGroups"]:
        if group["parameterSet"] == "ML-KEM-768" and group.get("function") == function:
            yield from group["tests"]


def spaced(reason):
    return reason.replace(" ", "-")


def verdict(test):
    return "1" if test["testPassed"] else "0"


def main():
    keygen, encap = sys.argv[1:]
    print(HEADER)
    for test in list(groups(keygen))[:KEYGEN_ROWS]:
        print("keygen", test["d"].lower(), test["z"].lower(), test["ek"].lower(), test["dk"].lower())
    for test in list(groups(encap, "encapsulation"))[:ENCAPS_ROWS]:
        print("encaps", test["ek"].lower(), test["m"].lower(), test["c"].lower(), test["k"].lower())
    for test in list(groups(encap, "decapsulation"))[:DECAPS_ROWS]:
        print("decaps", test["dk"].lower(), test["c"].lower(), test["k"].lower(), spaced(test["reason"]))
    for test in groups(encap, "decapsulationKeyCheck"):
        print("dkcheck", test["dk"].lower(), verdict(test), spaced(test["reason"]))
    for test in groups(encap, "encapsulationKeyCheck"):
        print("ekcheck", test["ek"].lower(), verdict(test), spaced(test["reason"]))


if __name__ == "__main__":
    main()
