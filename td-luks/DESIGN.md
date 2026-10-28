# td-luks: td's LUKS2 volume tool

td-luks replaces cryptsetup on td's boot and installation paths. It is
a dependency-free Rust program that reads and writes standard LUKS2
headers and activates them through the kernel's device mapper and
dm-crypt. It implements only the one LUKS2 profile td formats
(td-install/ENCRYPTION.md "Device-bound formatting") and the operations
td-protector's runner performs on it, and it refuses everything else.

This document is the design. It is planned and not yet implemented:
"Increments" says which parts exist. Until the cutover lands,
cryptsetup 2.8.8 remains td's LUKS2 program and ENCRYPTION.md's
increment 3 sign-off still governs it. Facts stated here about
cryptsetup are those of its 2.8.8 source.

## Why

cryptsetup is built with its kernel crypto backend, so LUKS2's PBKDF2
and keyslot encryption run through AF_ALG sockets. Linux 7.2 deprecates
AF_ALG (`Documentation/crypto/userspace-if.rst`): every
`CONFIG_CRYPTO_USER_API_*` option is now marked deprecated, the
interface "regularly has vulnerabilities", and the kernel recommends
moving programs to userspace crypto and turning the options off. td
compiles the hash and skcipher interfaces in for cryptsetup alone, which
leaves any unjailed process on a td system able to reach them.

The cutover therefore removes:

- the AF_ALG interfaces from td's kernel;
- cryptsetup and the four C libraries it alone needs: LVM2's
  device-mapper library, json-c, popt, and util-linux's libuuid and
  libblkid;
- the frozen `linux-source` 4.14.67 header seed, whose only consumers are
  those libraries, and the json-c hold;
- the last C program on td's boot path (td-install/DESIGN.md D6).

td-luks is not a new disk format. Volumes stay standard LUKS2, so stock
cryptsetup opens, inspects and repairs them, and resumes their
reencryption.

## The profile

td-luks formats exactly the profile ENCRYPTION.md fixes:

- **Volume:** aes-xts-plain64 with a 512-bit volume key and 4096-byte
  sectors.
- **IV unit:** the IV counts 512-byte units, as for every LUKS2 volume.
  cryptsetup sets `iv_large_sectors` only in plain mode, so td-luks
  never sets it.
- **Keyslots:** PBKDF2-HMAC-SHA256 with 1000 iterations and a 32-byte
  salt, then LUKS1-style AF splitting with 4000 stripes and SHA-256. The
  area is encrypted with the segment's cipher, aes-xts-plain64 with a
  512-bit key, in 512-byte sectors, the IV counting from 0 at the area's
  offset. A keyslot's split key is 256000 bytes (500 sectors) in a
  258048-byte area.
- **Digests:** PBKDF2-HMAC-SHA256 with 1000 iterations, a 32-byte salt
  and a 32-byte result, for the volume key, a reencryption's new key and
  the reencryption digest. 1000 is cryptsetup's own count under
  `--pbkdf-force-iterations`, since that option skips the digest
  benchmark. On read, td-luks accepts any count up to its reader's
  bound, 4,000,000 (`header::MAX_DIGEST_ITERATIONS`, which
  td-protector re-exports), because stock cryptsetup may have written
  the header.
- **Layout:** each header copy is 16 KiB, with a SHA-256 checksum. The
  keyslots area is 16744448 bytes. The data segment starts at 16 MiB and
  its size is `dynamic`.
- **Persistent flags:** none. A header carrying `config.flags` (stock
  cryptsetup's `--persistent` activation flags) is refused, since those
  flags change the table.
- **Keyslots and tokens:** keyslots 0 to 31, with td's token types
  (td-protector/DESIGN.md "LUKS2 tokens"). Foreign token types are kept
  verbatim.
- **Reencryption:** only in place, forward, with checksum (SHA-256) or
  journal resilience, and the same profile on both sides ("Reencryption").

td-luks refuses a header outside the profile before writing anything.
Among those are LUKS1, Argon2, integrity, a detached header other than
"Detached test", other ciphers, key sizes, header sizes or area sizes,
and an unknown mandatory requirement.

## Crates

td-luks is a library and a program:

- **The library** forbids `unsafe`. It holds the format-level header
  reader, which moves here from `td-protector/src/luks2.rs` and gains
  segments, keyslot areas and the reencryption metadata. It also holds
  the writer and the keyslot cryptography.
- **td-protector** depends on that library and keeps its token semantics
  layered on it: td's token types, their bounds and the mixing rules. So
  the format is read one way, and td-luks does not depend on
  td-protector or td-tpm.
- **The program** adds the command line and the device-mapper module,
  which is the only `unsafe` code ("Device mapper").

## Commands

The command line is td-luks's own, one verb per operation td-protector's
runner performs, with the profile compiled in. Each cryptsetup argument
list in `td-protector/src/cryptsetup.rs` has exactly one replacement, and
td-luks takes no option that would select a different profile.

| cryptsetup 2.8.8 form td uses | td-luks |
|---|---|
| `luksFormat` (the profile, `--uuid`, `--label`, keyslot 0) | `format UUID LABEL DEVICE` |
| `luksAddKey --key-slot BY --new-key-slot N` | `add-key BY N DEVICE NEW-KEY-FILE` |
| `token import [--token-id N]` | `token-import [N] DEVICE` |
| `token import --token-id N --token-replace` | `token-replace N DEVICE` |
| `token remove --token-id N` | `token-remove N DEVICE` |
| `luksKillSlot DEVICE SLOT` | `kill-slot SLOT DEVICE` |
| `luksDump --dump-json-metadata` | `metadata DEVICE` |
| `luksDump --dump-volume-key --volume-key-file F` | `volume-key DEVICE FILE` |
| `open [--key-slot N]` | `open [SLOT] DEVICE NAME` |
| `open --volume-key-file F` | `open-volume-key DEVICE NAME KEY-FILE` |
| `open --test-passphrase --key-slot N [--header H]` | `test SLOT DEVICE [HEADER]` |
| `close` / `status` | `close NAME` / `status NAME` |
| `reencrypt` (the profile, `--resilience R`) | `reencrypt SLOT {checksum\|journal} DEVICE NAME` |
| `reencrypt --resume-only` | `reencrypt-resume DEVICE NAME` |
| (none: see "Reencryption") | `kill-unbound DEVICE` |

The runner's contract with its child does not change:

- the first secret arrives on standard input;
- a second key arrives as a descriptor the runner names
  `/proc/<pid>/fd/N`;
- `volume-key` creates its file with `O_EXCL` and mode 0400;
- no argv element, environment variable or file td-luks reads carries
  key material, and td-luks ignores its environment.

The key on standard input authorizes each verb as cryptsetup's would:

- **`add-key`:** the key must open keyslot BY, and BY must be bound to
  the volume's segment. An unbound BY is a failure (status 1), as
  cryptsetup 2.8.8's `luksAddKey` reports it ("No usable keyslot is
  available"), not a wrong key.
- **`test`:** the key must open SLOT. An unbound SLOT is tested against
  its own digest, as cryptsetup 2.8.8's `open --test-passphrase
  --key-slot` tests it: status 0 for its key, 2 for another. The
  interop tests pin both behaviours.
- **`kill-slot`:** the key must open an active keyslot other than SLOT,
  whatever its priority, as cryptsetup's `verify_keyslot` tries each by
  number, or SLOT itself when it is the last keyslot. A shorter key
  than td's shortest secret is refused before anything is read;
  cryptsetup instead kills unasked on empty input. When SLOT is
  unbound and no bound keyslot remains, td-luks still needs SLOT's own
  key, where cryptsetup kills unverified; this is deliberately
  stricter, and a keyslot with no digest at all, which no key can be
  checked against, is then refused. Killing the last bound keyslot by
  its own key leaves a header no key opens, as with cryptsetup, and so
  does a crash after that kill's wipe ("Commits").
- **`kill-unbound`:** the key must open a bound keyslot. It removes each
  unbound keyslot, meaning one with no digest or whose digest names no
  segment (cryptsetup's `LUKS2_keyslot_unbound`). Their digests stay,
  with empty `keyslots`, as cryptsetup leaves them. It refuses a header
  that carries the reencryption requirement.
- **`open` with no slot:** tries each bound keyslot whose priority is
  not 0: priority 2 first, then priority 1, each in keyslot-number
  order. cryptsetup goes in JSON order within a priority, so only which
  keyslot opens can differ, never whether one does.

Exit statuses:

| Status | Meaning |
|---|---|
| 0 | success |
| 2 | a key that opens no candidate keyslot (cryptsetup's wrong-passphrase status) |
| 4 | `status` finds no mapping |
| 1 | any other failure, with a message on standard error |

Reencryption prints one `--progress-json`-shaped line a second,
`{"device_bytes":"N","device_size":"M"}` with decimal strings, so the
selector's progress reader stays as it is.

## Header writes

The writer emits complete LUKS2 headers. Each binary header holds the
magic, version, header size, sequence id, label, checksum algorithm,
salt, UUID, subsystem and offset, with the SHA-256 checksum computed
over the copy with its checksum field zeroed. The JSON area follows
cryptsetup's schema for keyslots, digests, segments, config and tokens,
NUL-padded to the area. Numbers that LUKS2 writes as strings (offsets,
sizes) are written as strings.

A commit rewrites the label and UUID it read and an empty subsystem,
so td-luks refuses, when it loads a header and so for reads and
mutations alike, a header whose label or UUID field is not UTF-8 up to
a NUL followed only by NUL bytes, or whose subsystem is not empty. td
formats no subsystem, and refusing keeps every commit's binary fields
byte for byte what was read. The
profile also refuses a second digest naming segment 0, which only a
reencryption writes.

The JSON text need not match cryptsetup's byte for byte. It must be a
header that cryptsetup 2.8.8 and the reader both accept with the same
meaning, and the oracle tests compare meaning, not bytes. The one
byte-exact structure is the reencryption digest's input ("Reencryption").

### Locking

td-luks follows cryptsetup's whole lock-file protocol
(`utils_device_locking.c`), so td-luks and stock cryptsetup exclude each
other. The locks are:

- `/run/cryptsetup/L_<major>:<minor>` for a block device: exclusive for
  a mutation, shared for a read;
- a regular file, as tests use, locked as itself with the same modes,
  with no resource file and no check, as cryptsetup locks one;
- the reencryption lock `LN_LUKS2-reencryption-<uuid>`, while a
  reencryption runs and when `open` activates a header mid-reencryption.

Each lock is taken as cryptsetup takes it:

1. Create `/run/cryptsetup` if it is missing, requesting mode 0700,
   subject to umask, refusing it
   when it is a symlink or no directory.
2. Open the lock file with `O_CREAT | O_NOFOLLOW`, requesting mode 0644
   subject to umask, then
   `flock` it, blocking.
3. Check that the path still names the locked inode; if it does not,
   cryptsetup unlinked the file meanwhile, so start again.
4. On release, take `LOCK_EX | LOCK_NB` and unlink the file if it is
   still the same inode.

A device is opened once, before locking, and the same descriptor serves
all header I/O. A replaced regular-file path cannot redirect an operation
to an inode its lock does not cover. The program reads stdin through a
duplicated descriptor directly into a preallocated zeroing buffer, with
no buffered-stdio or growing-buffer copies of secrets.

A header file in "Detached test" is only read and takes no lock.

cryptsetup opens the lock file relative to the directory's descriptor;
std has no `openat`, so td-luks opens it by path once the directory is
known to be no symlink. Only root can create or swap that directory, and
root needs no race to defeat a lock. The lock code belongs to the
program (`bin/lock.rs`, outside the `src/` the boot recipes stage file
by file), not the library they compile.

### Commits

Every mutation is one commit:

1. Read and validate the header under the lock.
2. Build the new JSON with sequence id + 1.
3. Write the keyslot-area changes and sync them.
4. Write the copy the reader did not use, then the one it used. For
   each copy, write the binary header without its checksum, then the
   JSON, then the binary header with its checksum, then `fsync`, as
   cryptsetup's `hdr_write_disk` does. cryptsetup always writes the
   primary first, which is safe for it because it repairs a bad copy
   on every load; td-luks repairs nothing on read ("No repair on
   read"), so when only one copy is valid it must be the last one
   overwritten. When both are valid at one sequence id, the order is
   immaterial.

A crash at any point leaves at least one valid copy, carrying either the
old JSON or the new, and the reader's validity and sequence rule picks
it. Step 3 runs before the header changes. So a crash after a kill's
wipe leaves the old header naming a keyslot whose area no longer
decrypts: that keyslot opens nothing, and a repeated kill removes it.
Fault-injection tests stop the writer after each write.

### Format and kill

`format` zeroes both header copies' areas, fills the keyslots area with
random bytes, then commits the header with keyslot 0, as cryptsetup's
`LUKS2_wipe_header_areas` does. It leaves the data segment as it found
it (ENCRYPTION.md "Device-bound formatting").

A killed keyslot's area is overwritten with random bytes and synced
before the commit that removes the keyslot. That commit also strips the
keyslot from every token, leaving any token that named only that keyslot
as an orphan with an empty `keyslots` list, as cryptsetup does.

New keyslots take the lowest free 4096-aligned span of the keyslots area
that fits, cryptsetup's first fit. Reencryption's own keyslot takes the
largest free span instead ("Reencryption").

### No repair on read

cryptsetup rewrites a bad or older header copy when it loads a header.
td-luks repairs only by committing: a read never writes, and the next
mutation writes both copies anyway. In particular, `test` with a
detached HEADER never writes HEADER or DEVICE.

## Cryptography

td-luks performs all of its cryptography in userspace, except the
encryption of volume data. Each primitive lives once, in the shared
crate for it (AGENTS.md "Avoid external dependencies"):

- **Hashing and PBKDF2:** SHA-256 and HMAC-SHA256 come from td-fido.
  PBKDF2-HMAC-SHA256 joins them there, pinned to RFC 7914 §11's vectors.
  td-netd's PBKDF2-HMAC-SHA1 (`td-netd/src/sha1.rs`) stays separate,
  since td-fido has no SHA-1.
- **AF splitter:** lives in td-luks, its only consumer, and is pinned to
  cryptsetup 2.8.8 fixtures.
- **Keyslot areas (AES-XTS):** td-fido's `fido_aes` gains AES-256-XTS
  over its existing table-free round function, pinned to IEEE 1619's
  vectors. The S-box is computed arithmetically with masked operations,
  so the cipher has no secret-indexed table and no branch on secret
  data. td-fido/DESIGN.md is amended for PBKDF2 and XTS in the
  increment that adds them.
- **Volume data:** encrypted only by the kernel's dm-crypt, never in
  software, including the data reencryption rewrites.
- **Randomness:** the volume key, salts and wipe bytes are read from
  `/dev/random`, which blocks only until the kernel's generator is
  initialized.

### Cost

One unlock attempt decrypts a 256000-byte split key per keyslot it
tries. On one thread of a Ryzen 7 5700G, td-fido's round function
decrypts one such key in about 185 ms at `opt-level = 3` and 290 ms at
`opt-level = "s"`; the consuming build's level is the one that counts.
At td's 1000 iterations, PBKDF2 (about 1.2 ms) and the AF merge are
negligible beside that.

The ceiling is two seconds per `open` or `test`, whatever the number of
keyslots tried. A protected header has up to about seven candidates:
about 1.3 s at `opt-level = 3` but 2.1 s at `"s"`. So td-luks builds
at `opt-level = 3`, in its manifest and its recipe alike. Increment B
records the T14s figure here. A bitsliced round function in td-fido is
the remedy if the T14s exceeds the ceiling.

The ceiling covers td's own profile. A header stock cryptsetup wrote may
carry a digest benchmarked to millions of iterations. At the reader's
4,000,000 bound one digest check takes 4 to 5 s, and an attempt checks
the digest once per candidate keyslot. That is a rescue path, not a
boot path, so it is accepted and not bounded by the ceiling.

## Device mapper

Activation, deactivation, status and reencryption's devices go through
`/dev/mapper/control` ioctls. This is td-luks's one `unsafe` surface, and
UNSAFE.md gains its section in increment C:

- **Requests:** `DM_VERSION`, `DM_DEV_CREATE`, `DM_TABLE_LOAD`,
  `DM_TABLE_CLEAR`, `DM_DEV_SUSPEND` (suspend and resume),
  `DM_DEV_REMOVE`, `DM_DEV_STATUS`, `DM_TABLE_STATUS` and
  `DM_TABLE_DEPS`, each value-pinned, each through a `struct dm_ioctl`
  buffer td-luks owns, with bounded output.
- **Secure data:** every request whose input or output carries a key
  sets `DM_SECURE_DATA_FLAG`, so the kernel wipes its copy of the
  buffer. That includes `DM_TABLE_STATUS` with the table flag, whose
  output holds the key.
- **Failed loads:** a table that fails to load or resume is cleared with
  `DM_TABLE_CLEAR` and its device removed, so no key stays in an
  inactive table.
- **Suspend flags:** suspends take the fixed flags reencryption needs,
  `DM_NOFLUSH_FLAG | DM_SKIP_LOCKFS_FLAG`.

The callers, td-boot, td-install, td-authd and td-protector, keep
forbidding `unsafe`: they run td-luks as a child, as they run cryptsetup
today.

### Activation

td-luks activates a mapping exactly as cryptsetup 2.8.8 does:

- the dm UUID is `CRYPT-LUKS2-<32 hex>-<name>`;
- the table is one crypt target over the whole data segment:
  `aes-xts-plain64 <128 hex key> 0 <major:minor> 32768 1 sector_size:4096`;
- the hex volume key sits in the table, as in cryptsetup's keyring-less
  build;
- `iv_large_sectors`, discard flags and every other optional parameter
  are absent.

td-boot's admission (td-boot/src/volume.rs) reads only `dm/uuid`,
`dm/name` and `slaves/`. The table's exactness is held by the interop
oracle, which compares `DM_TABLE_STATUS` output for a volume each
program activates.

### `/dev/mapper` entries

td has no udev. Today libdevmapper, built without udev
synchronization, creates `/dev/mapper/<name>` as a block node with
`mknod`. td-luks instead creates it as a symlink to `../dm-N`. That
avoids a `mknod` surface, and every current consumer follows links:
td-btrfs's target open and td-boot's update locks use `fs::metadata`,
and td-boot's admission uses sysfs and `/dev/dm-N`.

`open` replaces an existing entry only after checking that no live
mapping has that name, since a stale symlink left by a crash could name
a reused `dm-N`. `close` removes the entry with the device.

### Reencryption devices

Reencryption stacks devices as cryptsetup does:

- `<name>-overlay` and `<name>-hotzone-forward`, with `CRYPT-SUBDEV-`
  UUIDs, under the active mapping, which is reloaded as a linear target
  over the overlay;
- temporary dm-crypt devices `temporary-cryptsetup-<pid>-<n>`, through
  which the data is read and rewritten.

`close` removes leftover subdevices it finds through `DM_TABLE_DEPS`.

## Reencryption

Online reencryption keeps cryptsetup 2.8.8's on-disk protocol, so that
either program resumes what the other interrupted. Increment D builds it
from these parts:

- **Requirement:** exactly `online-reencrypt-v2` in
  `config.requirements.mandatory`.
- **Reencrypt keyslot:** type `reencrypt`, mode `reencrypt`, direction
  `forward`, `key_size` 1, priority 0, so no open tries it. Its area
  takes the largest free span of the
  keyslots area and holds the resilience metadata.
- **Resilience:** checksum resilience stores a SHA-256 of each 4096-byte
  block of the hotzone in that area, so the hotzone is at most the
  area's size / 32 × 4096 bytes. Journal resilience stores the hotzone's
  data there. The hotzone's length is not part of the on-disk protocol:
  a resume reads it from the segments. cryptsetup caps it at 1 GiB and
  holds it in one buffer. td-luks sets its own smaller limit, which
  increment D fixes from the selector's memory at boot.
- **Reencryption digest:** a PBKDF2 digest over a binary blob, matched
  byte for byte (cryptsetup's `luks2_reencrypt_digest.c`). The blob
  holds:
  - the bytes 0x76 and 0x30 + version;
  - the old key, then the new key;
  - the reencrypt keyslot's mode, direction, and area type, offset,
    size, hash and sector size, with integers big-endian;
  - the `backup-previous` and `backup-final` segments.
- **Segments:** the in-progress layout with its backup segments.
- **Devices:** those in "Reencryption devices".

Start, every resume point and the end must work in both directions:
td-luks to cryptsetup and cryptsetup to td-luks. The interop oracle
holds that for every resume point.

Opening a header that carries the requirement is part of this increment,
not increment C. That open verifies the reencryption digest, runs crash
recovery, which writes data, and loads a multi-segment table. Until
increment D lands, `open` refuses such a header.

### Start and end

td-luks orders its own commits so that a crash leaves a state
ENCRYPTION.md's upgrade machine resolves:

- **Start:** the new key's keyslot, its digest, the reencrypt keyslot,
  the segments and the requirement are committed together. A crash
  leaves either U1 unchanged or a resumable U2, never a stray keyslot.
  (cryptsetup's tool commits the new keyslot and its token reassignment
  first, so a crash can leave an extra unbound keyslot holding the new
  key.)
- **End:** one commit finalizes the segments and removes the
  requirement, the reencrypt keyslot and its digest, the backup segments
  and the old key's digest. The old key's keyslots stay, unbound. Then
  each is wiped and removed in its own commit.

cryptsetup 2.8.8 instead destroys the old keyslots before dropping the
requirement, and a power cut between them leaves a header no resumption
opens (ENCRYPTION.md "Re-encrypting upgrade"). With td-luks's order,
every commit leaves a header both programs open or resume. A crash
during the end leaves unbound keyslots and no requirement.
`kill-unbound`, run by the selector before it chooses the recovery
keyslot in U1 and U3, removes them. It also removes the stray keyslot a
crash in cryptsetup's own start leaves.

Increment D amends ENCRYPTION.md's U1 and U3 definitions, the selector's
recovery-keyslot rule and the gap paragraph to match. It may do so only
after the interop oracle shows that cryptsetup 2.8.8 accepts every
intermediate header.

## Detached test

`test SLOT DEVICE HEADER` reads the header from the file HEADER instead
of DEVICE. It covers the storage worker's bounded test of a detached
header copy (td-install/src/storage_operation.rs). It is the only
detached form, and it writes nothing.

## Tests

- **Unit tests:** PBKDF2, XTS and AF vectors; header serialization round
  trips through the reader; every mutation's fault-injection points;
  authorization of each verb; refusal of every header outside the
  profile. These run on ordinary files without privilege.
- **Host interop tests:** `tests/cryptsetup_interop.rs`, ignored by
  default, runs the pinned cryptsetup found first on `PATH` against
  regular files in both directions: cryptsetup dumps, opens and leaves
  unrepaired what td-luks writes, and td-luks opens what cryptsetup
  writes with td-protector's own format arguments, to the same volume
  key. They need the host's AF_ALG and leave with cryptsetup. One of
  them captured the keyslot known answer `src/keyslot.rs` pins in the
  ordinary unit tests: a cryptsetup keyslot's first area sector, its
  plaintext, and its digest of the dumped volume key.
- **Committed fixtures:** headers and keyslot areas that cryptsetup
  2.8.8 produced with known keys. The set covers a fresh format, added
  keyslots and tokens, a killed keyslot, and reencryption interrupted at
  each resume point under both resilience modes. A recorded procedure
  regenerates them in a qemu guest. They keep the interoperability
  evidence after cryptsetup leaves the tree.
- **Interop oracle:** a qemu guest with both programs runs each verb
  with td-luks and checks the result with cryptsetup, and the other way
  round. It covers activation tables, locking and reencryption with
  interruption and resumption at each step. It runs until the cutover,
  which retires it in favour of the fixtures and the existing encrypted
  install and boot oracles.
- **Existing tests:** td-protector's ignored host tests
  (`tests/header_fit.rs`, `tests/keyslot_test.rs`) move to td-luks, which
  needs no AF_ALG.

## Increments

Each increment is one landable commit, or a short series:

- **A.** This design. Status: landed.
- **B.** Libraries and the verbs that need no device mapper:
  - **B1.** In td-fido: PBKDF2 and XTS. Status: landed.
  - **B2.** The reader moved from td-protector, with td-protector
    switched onto it (`src/header.rs`). Status: landed.
  - **B3a.** The writer library: header commits through the `Medium`
    trait (`src/write.rs`), the AF splitter and keyslot cryptography
    (`src/keyslot.rs`), and the operations behind `format`, `add-key`,
    `kill-slot`, `kill-unbound`, `token-*`, `metadata`, `volume-key`
    and `test`, with the profile check, fault-injection tests, the
    reader's format-only refusal tests moved from td-protector, and
    ignored interop tests against the pinned cryptsetup
    (`tests/cryptsetup_interop.rs`). Status: landed.
  - **B3b.** The `td-luks` program (`bin/main.rs`): the verbs that
    need no device mapper, their exit statuses and key input over that
    library, `format`'s `O_EXCL` open of a block device, and
    cryptsetup's lock protocol (`bin/lock.rs`, "Locking"), with CLI
    tests (`tests/cli.rs`) and the program in the interop tests. No
    recipe ships it until E. Status: landed.
  - **B4.** Next: the cryptsetup fixtures ("Tests").
- **C.** The device-mapper surface: `open`, `open-volume-key`, `close`
  and `status` for headers without the reencryption requirement, with
  UNSAFE.md's section and the interop oracle for activation.
- **D.** Reencryption: `reencrypt`, `reencrypt-resume` and `open` on a
  header mid-reencryption, in both directions, with the ordered start
  and end and the ENCRYPTION.md amendments above.
- **E.** The cutover, in one landing (AGENTS.md principle 4):
  - td-protector's runner and every caller switch to td-luks;
  - the image ships `/bin/td-luks` where it ships cryptsetup;
  - the recipes are deleted: cryptsetup, libdevmapper, json-c, popt,
    util-linux-libs, the `linux-source` 4.14.67 seed, and their pins and
    watches;
  - the kernel recipe drops `CONFIG_CRYPTO_USER_API_*` and asserts it
    absent;
  - ENCRYPTION.md increment 3, td-install/DESIGN.md D6 and D7,
    td-protector/DESIGN.md "Cryptsetup runner", UNSAFE.md, UPDATING.md
    and AGENTS.md are amended to match.
