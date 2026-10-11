"""Independent FAT32 reader for td-fat's test images; Python stdlib only.

Usage: python3 -I fat_reader.py DIR > fat_reader.txt

DIR holds the images `cargo test -- --ignored write_reader_images` writes
(TD_FAT_IMAGES=DIR), one `NAME.img` per scenario, each a whole FAT32
volume. Written from Microsoft's FAT32 specification (fatgen103) alone,
not from td-fat or td's writer: it parses the BPB, both FATs and every
directory, follows each chain, and reports what a reader sees. No network.
Not a build/test/runtime dependency. Each output line is space-separated:

  image NAME DIGEST          SHA-256 over every non-zero 512-byte unit,
                            each preceded by its index as 8 big-endian bytes
  geometry NAME BPS SPC CLUSTERS
  file NAME PATH SIZE SHA256
  free NAME N               clusters free in both FATs
  lost NAME N               allocated clusters no entry reaches (bad excluded)
  fsinfo NAME FREE HINT     FSInfo's free count and next-free hint

The reader refuses (and prints nothing for that image) a volume whose two
FATs disagree on a reachable cluster, a chain that loops, leaves the
volume or does not match its file's size, or two entries sharing a
cluster.
"""
import hashlib
import os
import struct
import sys

UNIT = 512
EOC_MIN = 0x0FFFFFF8
BAD = 0x0FFFFFF7
MASK = 0x0FFFFFFF


class Refused(Exception):
    pass


def sparse_digest(data):
    h = hashlib.sha256()
    zero = bytes(UNIT)
    for index in range(0, len(data) // UNIT):
        unit = data[index * UNIT:(index + 1) * UNIT]
        if unit != zero:
            h.update(struct.pack(">Q", index))
            h.update(unit)
    return h.hexdigest()


def read_volume(name, data):
    bps, spc, reserved, nfats = struct.unpack_from("<HBHB", data, 11)
    tot32 = struct.unpack_from("<I", data, 32)[0]
    fatsz = struct.unpack_from("<I", data, 36)[0]
    root = struct.unpack_from("<I", data, 44)[0]
    fsinfo = struct.unpack_from("<H", data, 48)[0]
    if data[510:512] != b"\x55\xaa" or nfats != 2:
        raise Refused("boot sector")
    data_start = (reserved + nfats * fatsz) * bps
    clusters = (tot32 - reserved - nfats * fatsz) // spc
    if clusters < 65525:
        raise Refused("not FAT32")
    cluster_bytes = bps * spc

    def fat(which, c):
        at = (reserved + which * fatsz) * bps + 4 * c
        return struct.unpack_from("<I", data, at)[0] & MASK

    def cluster(c):
        at = data_start + (c - 2) * cluster_bytes
        return data[at:at + cluster_bytes]

    owner = {}

    def chain(first, who):
        out = []
        c = first
        while True:
            if c < 2 or c > clusters + 1:
                raise Refused("chain leaves the volume")
            if c in owner:
                raise Refused("cluster %d shared by %s and %s" % (c, owner[c], who))
            if fat(0, c) != fat(1, c):
                raise Refused("FATs disagree at %d" % c)
            owner[c] = who
            out.append(c)
            nxt = fat(0, c)
            if nxt >= EOC_MIN:
                return out
            if nxt in (0, 1, BAD):
                raise Refused("chain reaches a free or bad cluster")
            c = nxt

    files = []

    def walk(first, path, depth):
        if depth > 16:
            raise Refused("too deep")
        raw = b"".join(cluster(c) for c in chain(first, path or "/"))
        for at in range(0, len(raw), 32):
            entry = raw[at:at + 32]
            lead = entry[0]
            if lead == 0x00:
                break
            if lead == 0xE5:
                continue
            attr = entry[11]
            if attr & 0x0F == 0x0F or attr & 0x08:
                continue
            name = entry[0:8].rstrip(b" ").decode("ascii")
            ext = entry[8:11].rstrip(b" ").decode("ascii")
            if name in (".", ".."):
                continue
            full = path + "/" + name + ("." + ext if ext else "")
            hi, = struct.unpack_from("<H", entry, 20)
            lo, = struct.unpack_from("<H", entry, 26)
            size, = struct.unpack_from("<I", entry, 28)
            first_cluster = (hi << 16) | lo
            if attr & 0x10:
                walk(first_cluster, full, depth + 1)
                continue
            if size == 0:
                if first_cluster != 0:
                    raise Refused("an empty file owns a cluster")
                files.append((full, 0, hashlib.sha256(b"").hexdigest()))
                continue
            owned = chain(first_cluster, full)
            if len(owned) != (size + cluster_bytes - 1) // cluster_bytes:
                raise Refused("chain length does not match %s" % full)
            body = b"".join(cluster(c) for c in owned)[:size]
            files.append((full, size, hashlib.sha256(body).hexdigest()))

    walk(root, "", 0)
    free = 0
    lost = 0
    for c in range(2, clusters + 2):
        a, b = fat(0, c), fat(1, c)
        if a == 0 and b == 0:
            free += 1
        elif c not in owner and a != BAD and b != BAD:
            lost += 1
    info = data[fsinfo * bps:(fsinfo + 1) * bps]
    if info[0:4] != b"RRaA" or info[484:488] != b"rrAa":
        raise Refused("FSInfo signatures")
    info_free, info_hint = struct.unpack_from("<II", info, 488)
    out = ["geometry %s %d %d %d" % (name, bps, spc, clusters)]
    for path, size, digest in sorted(files):
        out.append("file %s %s %d %s" % (name, path, size, digest))
    out.append("free %s %d" % (name, free))
    out.append("lost %s %d" % (name, lost))
    out.append("fsinfo %s %d %d" % (name, info_free, info_hint))
    return out


def main():
    directory = sys.argv[1]
    print("# td-fat reader vectors; generated by fat_reader.py")
    for entry in sorted(os.listdir(directory)):
        if not entry.endswith(".img"):
            continue
        name = entry[:-4]
        with open(os.path.join(directory, entry), "rb") as f:
            data = f.read()
        print("image %s %s" % (name, sparse_digest(data)))
        try:
            for line in read_volume(name, data):
                print(line)
        except Refused as why:
            print("refused %s %s" % (name, str(why).replace(" ", "-")))


if __name__ == "__main__":
    main()
