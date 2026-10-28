# td-luks: td's LUKS2 volume tool

td-luks replaces cryptsetup on td's boot and installation paths. It is
a dependency-free Rust program that reads and writes standard LUKS2
headers and activates them through the kernel's device mapper and
dm-crypt. It implements only the one LUKS2 profile td formats
(td-install/ENCRYPTION.md "Device-bound formatting") and the operations
td-protector's runner performs on it, and it refuses everything else.

The boot and installation paths use td-luks. "Increments" records the
implementation boundaries. Source citations describe the protocol;
compatibility claims are checked against the pinned cryptsetup 2.8.8
binary. The tests below state what is checked so far.

## Why

The replaced cryptsetup was built with its kernel crypto backend, so LUKS2's PBKDF2
and keyslot encryption run through AF_ALG sockets. Linux 7.2 deprecates
AF_ALG (`Documentation/crypto/userspace-if.rst`): every
`CONFIG_CRYPTO_USER_API_*` option is now marked deprecated, the
interface "regularly has vulnerabilities", and the kernel recommends
moving programs to userspace crypto and turning the options off. td previously compiled the hash and skcipher interfaces for cryptsetup
alone, exposing them to every unjailed process.

The cutover removes:

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
- **Digest assignment:** every keyslot belongs to exactly one digest.
  An unbound keyslot retains its digest with an empty `segments` list;
  a keyslot with no digest makes the whole header invalid.
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
  which is the only `unsafe` code ("Device mapper"). Its library keeps
  `forbid`; the binary and manifest use `deny` with one scoped allowance.

## Commands

The command line is td-luks's own, one verb per operation td-protector's
runner performs, with the profile compiled in. Each cryptsetup argument
list in `td-protector/src/luks.rs` has exactly one replacement, and
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
- no argv element or environment variable carries key material; keys
  arrive on stdin or through the named descriptor/volume-key file.
  td-luks ignores its environment.

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
  stricter. A keyslot with no digest invalidates the whole header.
  Killing the last bound keyslot by its own key leaves a header no key
  opens, as with cryptsetup, and so
  does a crash after that kill's wipe ("Commits").
- **`kill-unbound`:** the key must open a bound keyslot. It removes each
  unbound keyslot, meaning one whose digest names no segment. Their
  digests stay,
  with empty `keyslots`, as cryptsetup leaves them. It refuses a header
  that carries the reencryption requirement. After bound-key
  authentication, it first wipes and syncs all free keyslot-area spans,
  even when no unbound slot remains ("Start and end").
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

A keyslot no digest names is a failure (status 1) for `test` and
`add-key` whatever the key, never a wrong key. cryptsetup 2.8.8 exits 1
for `open --test-passphrase --key-slot` and `luksAddKey --key-slot` on
such a header too, refusing the whole header ("Keyslot N is not assigned
to exactly 1 digest"); the profile check refuses it on load and commit.
The interop tests pin both refusals. They also construct the proposed
end-cleanup metadata with distinct old and new volume keys and verify
cryptsetup accepts it before cleanup and after each old keyslot removal,
without repairing either copy. This checks headers and keyslots only;
actual reencryption and its interrupted states remain increment D's oracle.

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
   `flock` it, blocking for header and mapping-name locks. The UUID
   reencryption lock uses a nonblocking exclusive flock, as stock resume
   does, and fails promptly if another process owns it.
3. Check that the path still names the locked inode; if it does not,
   cryptsetup unlinked the file meanwhile, so start again.
4. On release, take `LOCK_EX | LOCK_NB` and unlink the file if it is
   still the same inode.

Reencryption acquires its UUID lock before the exclusive header lock,
then the top, overlay and hotzone name locks in that order. An initial
shared header read discovers the UUID; after locking, the same held device
must still carry it. Initial discovery for open, start and resume uses a
nonblocking shared lock, so another reencryption's exclusive header lock
cannot queue a second operation behind its full run. Other header reads
retain their blocking locks. Ordinary activation holds the header lock through
CREATE. Stock init-only takes the header lock rather than the UUID lock.
The runtime oracle pins stock resume's nonblocking exclusion.

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
UNSAFE.md §24 records its boundary:

- **Requests:** `DM_VERSION`, `DM_DEV_CREATE`, `DM_TABLE_LOAD`,
  `DM_TABLE_CLEAR`, `DM_DEV_SUSPEND` (suspend and resume),
  `DM_DEV_REMOVE`, `DM_DEV_STATUS`, `DM_TABLE_STATUS` and
  `DM_TABLE_DEPS`, each value-pinned, each through a `struct dm_ioctl`
  buffer td-luks owns, with bounded output. The allocation is 64 KiB
  and eight-byte aligned; fixed-header replies may report 305 bytes
  despite the 312-byte padded input structure. ABI 4.20 or newer is
  required for secure-data. Interrupted ioctls are not blindly retried.
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

Activation requires a block device with a positive, whole-4096-byte
data segment. The header lock stays held through key verification and
activation; the table names the opened descriptor's major/minor, never
a separately resolved pathname. An explicit slot must be bound to
segment 0; a supplied 64-byte volume key must match that segment's digest.

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
mapping has that name and successfully claiming the kernel name with
CREATE, since a stale symlink left by a crash could name a reused
`dm-N`. Losing CREATE never unlinks an entry. td-luks also serializes
open/publication and close/removal under an exclusive per-name lock,
`/run/cryptsetup/LN_td-luks-mapping-<name>`, using the same inode/retry
and release protocol as its device locks. Activation takes its header
lock first, then this name lock. Different volume descriptors do not
bypass name exclusion. Stock cryptsetup does not take this extra lock;
CREATE remains the cross-tool name-claim boundary. `close` removes the entry with the device. Names contain only ASCII
letters, digits, `_`, `-`, `.`, or `+`, and exclude `.`, `..` and
`control`; both kernel name and UUID field limits apply. An existing
regular file is refused. The devtmpfs node must have the created
mapping's device number before its symlink is published. Publication
failure removes the created mapping. Ordinary `close` requires a LUKS2
UUID and refuses a mapper entry naming another device.

### Reencryption devices

Reencryption stacks devices as cryptsetup does:

- `<name>-overlay` and `<name>-hotzone-forward`, with `CRYPT-SUBDEV-`
  UUIDs, under the active mapping, which is reloaded as a linear target
  over the overlay;
- temporary dm-crypt devices `temporary-cryptsetup-<pid>-<n>`, through
  which the data is read and rewritten.

`close` takes the UUID lock and removes at most the two expected
subdevices found through `DM_TABLE_DEPS`, checking their exact names,
volume-bound UUIDs and device identities. It also checks the two expected
helper names for unreferenced orphans from partial setup or final teardown,
admitting them bottom-up only when their dependencies reach that same
backing device or an already verified helper. Foreign UUIDs are retained. It removes top, overlay, then
hotzone. It refuses active reencryption rather than tearing down its stack.
Temporary maps use deferred removal (ABI 4.27 or newer) while an O_DIRECT
File holds each open, so process exit also releases them. A crash between
CREATE and deferred removal may leave an orphan; a subsequent authenticated
run removes only td-luks temporary UUIDs bound to that volume and raw
backing device. Unrelated maps are never cleanup candidates.

Opening an interrupted volume requires no existing raw-device holders.
Starting or resuming online requires the exact authenticated top table to
be the sole raw-device holder. An interrupted retained stack must first be
closed, or lost on reboot; busy helpers cause refusal. A failure during a
hotzone leaves its helper suspended and metadata recoverable. The program
does not guess which queued application I/O can safely resume on an error.

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
  holds it in one buffer. td-luks caps each newly marked hotzone at
  1 MiB. Recovery must stream larger stock hotzones within a bounded
  working buffer; the metadata validator does not impose td's creation
  limit on stock's recorded hotzone.
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
not increment C. That open verifies both volume keys and the reencryption
digest, syncs the authenticated metadata, runs crash recovery, which writes
data, and loads a multi-segment table. Explicit-slot and raw-volume-key
opens are refused during reencryption. No runtime command ships until E.

### Metadata and digest admission

The library's `reencrypt::profile` validates the forward in-place layout
against the opened device's data length. It accepts one to three active
segments followed by the fixed full-volume previous/final backups, with
distinct old/new digests and one private reencryption digest. Offsets and
IVs must cover the data contiguously. At most one finite dirty hotzone
may lie between the new prefix and old suffix; its resilience bytes must
fit the nonoverlapping area. The clean all-new layout with the requirement
still present is refused because of the stock resume failure below.

Normal keyslots, digests, tokens and configuration retain the ordinary
profile's checks. The validator applies those checks to a private clone
with the reencryption-only fields normalized, after validating those
fields separately. It does not change the input or perform I/O.

`reencrypt::digest` encodes version 2 with the fixed backup profile into
a bounded, zeroed-on-drop buffer: 274 bytes for checksum and 263 for
journal. The caller must validate the metadata separately and authenticate
the resulting digest before trusting recovery parameters. D1 provides
validation and digest calculation; ordinary activation and mutations
continue to refuse the reencryption requirement.

### Metadata transitions

D2 adds the header protocol without enabling reencryption commands or
production data writes. `reencrypt::load` preserves binary identity and
validates the layout against the held device size. `unlock` opens both
keys from their enabled keyslots and authenticates the v2 digest before
returning them. Wrong passphrases and mismatched verification metadata
remain distinct failures.

`plan_start` authenticates the selected ordinary bound slot, generates
a fresh volume key, seals its new keyslot under the same passphrase,
allocates the largest free resilience span and prepares the atomic start
below. Tokens naming the selected slot follow its replacement; other old
slots remain assigned until cleanup. A priority-zero authorizing slot is
refused so automatic resume does not depend on a disabled slot.

`mark_hotzone` and `finish_hotzone` are pure JSON transitions. Marking
requires a clean prefix and a whole-sector hotzone of at most 1 MiB that
fits the resilience area. Completion requires one dirty hotzone and goes
directly to ordinary metadata when it reaches the end. `reencrypt::commit`
validates the in-progress or final ordinary profile and uses the existing
writer's sequence and copy ordering. The caller must hold the locks,
persist resilience before committing dirty metadata, and sync rewritten
data before committing completion. These library APIs do not establish
that data ordering themselves.

`fixtures/metadata.rs` constructs 22 states under checksum and journal
resilience, using dm-crypt for old/new ciphertext, and asks stock 2.8.8 to
repair/resume each and verify all plaintext. `fixtures/METADATA.md` records
that oracle and its boundary. Rust tests separately interrupt every
start and final header write/sync and check the surviving header.

### Runtime ordering

D3b adds the command controller and device stack in `bin/reencrypt*.rs`.
After authenticating keys and metadata it syncs the held metadata device
before recovery writes or resilience reuse: a previous process may have
left its last checksummed header visible but not durable. Start verifies
the existing mapping's old key; resume verifies its clean old/new segments.
Temporary old/new crypt maps expose aligned O_DIRECT plaintext I/O through
safe std; raw O_DIRECT I/O reads ciphertext without copying volume keys
into user-space cipher schedules. Plaintext uses a zeroed-on-drop 4096-byte
aligned buffer; resilience for a new hotzone is bounded to 1 MiB.

The top mapping becomes linear over an overlay. For each hotzone, load
its new-key crypt target over the hotzone helper between any new prefix
and old suffix, suspend overlay then hotzone, and resume the overlay.
I/O for that zone stays blocked while other zones can continue. Capture
old ciphertext or hashes, sync resilience and commit dirty metadata,
rewrite through old/new crypt maps and sync the data, then commit clean
or final metadata before releasing the hotzone. Completion reloads the
top as one new-key crypt target, removes helpers and temporary maps, and
runs authenticated free-space/old-slot cleanup before reporting success.

Recovery streams the validated recorded hotzone, including a stock zone
larger than td's creation limit. Journal recovery restores each old
ciphertext sector before reading through the old map and writing through
the new one. Checksum recovery rewrites sectors whose raw ciphertext still
matches the saved hash; a mismatch means already rewritten, relying on
the selected atomic-sector assumption. It syncs data before advancing the
header. Final-zone recovery performs cleanup before publishing its mapping.

`fixtures/reencryption.rs` drives the actual binary in both directions,
including stock prefixes, concurrent I/O and live process interruption;
`fixtures/REENCRYPTION.md` states its procedure and limits. This does not
claim arbitrary process-killed live stacks resume without closing them.

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
  while retaining the old key's digest with `segments: []` and its
  keyslots unchanged. The old key's keyslots stay, unbound but each
  assigned to exactly one digest. After authenticating the new bound
  key, sync the final metadata, wipe and sync every free span of the
  keyslots area, then wipe
  and remove the old keyslots in one commit. The old digest stays
  with an empty `keyslots` list after the last removal.

Journal resilience retains old-key ciphertext in its area. Dropping the
reencryption keyslot from JSON does not erase those bytes, so completion
of the final metadata commit alone does not establish old-key revocation.
Free spans are derived from the surviving normal keyslot areas, not from
the deleted reencryption slot: an interrupted U3 cleanup can repeat the
wipe even when no unbound slot remains. This cleanup must finish before
ENCRYPTION.md's old-key-uselessness claim applies. Do not wipe resilience
before the final metadata commit: a crash could still select the dirty
header, whose recovery requires those bytes. `kill-unbound` implements
this post-commit cleanup using a reusable 64 KiB random buffer, deriving
free spans from the validated fixed-size normal keyslot areas. An initial
sync makes the authenticated final header durable before any erasure:
a previous process may have stopped after writing its checksum but before
syncing it. Failure of that barrier leaves the journal untouched. The header,
occupied slots and data segment stay untouched until the free-space sync
succeeds. Its fault-injection tests interrupt free-space writes, the sync
and old-slot retirement, then repeat cleanup and verify the journal is
erased and the new bound key still opens. They also cover cleanup with
no unbound slots and refusal of a wrong key or in-progress header. A
separate volatile/durable fixture stops finalization before its first sync,
persists cleanup's first erase out of order and then simulates power loss;
the surviving dirty header must never have lost its journal.
D2's metadata APIs alone do not perform cleanup.

The last hotzone stays marked `in-reencryption` through its data sync.
There is no intervening commit with one all-new active segment while the
reencryption requirement remains: cryptsetup 2.8.8 mistakes that clean
layout for offset zero, rewrites the new ciphertext as though it used the
old key, and returns success with corrupted plaintext. B4b's replayed
stock write prefixes reproduce this under checksum and journal resilience.
The final commit above goes directly from the last dirty hotzone to the
final segments without the requirement; a torn commit selects either a
recoverable last hotzone or the finished volume.

cryptsetup 2.8.8 also destroys the old keyslots before dropping the
requirement, and a power cut between them leaves a header no resumption
opens (ENCRYPTION.md "Re-encrypting upgrade"). With td-luks's order,
the intended invariant is that every commit leaves a header both
programs open or resume. Increment D must prove this for every
intermediate header against the pinned binary before shipping. A crash
during the end leaves unbound keyslots and no requirement.
`kill-unbound`, run by the selector before it chooses the recovery
keyslot in U1 and U3, removes them. It also removes the stray keyslot a
crash in cryptsetup's own start leaves.

The atomic E cutover amends ENCRYPTION.md's U1 and U3 definitions, the
selector's recovery-keyslot rule, post-commit free-area cleanup and the gap
paragraph together with its runner. Until then the shipped selector still
runs cryptsetup. These amendments require the D interop evidence that stock
accepts every intermediate header.

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
  each captured write boundary under both resilience modes, before old
  keyslot erasure. Each replay prefix is checked with stock cryptsetup:
  recoverable states must restore the known plaintext; the all-new layout
  still carrying the requirement is retained as negative evidence and
  must never be emitted by td-luks. A recorded procedure regenerates them
  in a qemu guest. They keep the interoperability
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
  - **B4a.** Ordinary cryptsetup fixtures: fresh format, added slot and
    token, killed slot and unbound slot, captured in QEMU by
    `fixtures/generate.rs`; `fixtures/README.md` records the procedure.
    `tests/fixtures.rs` consumes the committed answers without AF_ALG.
  - **B4b.** Reencryption fixtures under both resilience modes:
    `fixtures/generate_reencrypt.rs` captures stock block writes and
    validates every retained prefix in QEMU; `tests/reencryption_fixtures.rs`
    checks their headers, keyslots, ciphertext and resilience bytes
    offline. `fixtures/reencrypt/README.md` records the replay boundary
    and the stock end-state failure retained as negative evidence.
- **C.** The device-mapper surface: `open`, `open-volume-key`, `close`
  and `status` for headers without the reencryption requirement, with
  UNSAFE.md's section and the interop oracle for activation. The binary
  uses `bin/dm.rs` and `bin/dm_sys.rs`; `tests/activation.rs` and
  `tests/confinement.rs` hold authorization and the syscall boundary.
  `fixtures/activation.rs` runs the pinned stock comparison in QEMU;
  `fixtures/ACTIVATION.md` records its scope and procedure.
- **D.** Reencryption: `reencrypt`, `reencrypt-resume` and `open` on a
  header mid-reencryption, in both directions, with the ordered start
  and end and the ENCRYPTION.md amendments above.
  - **D1.** Read-only layout admission and the v2 verification digest
    (`src/reencrypt.rs`, `src/reencrypt_profile.rs`). Offline tests match
    the digest for all 106 stock prefixes, accept the 92 recoverable
    layouts and refuse the 14 unsafe all-new layouts, with malformed
    layout cases. No reencryption command or data rewriting is enabled.
  - **D2.** Authenticated header loading, atomic start and hotzone/end
    metadata transitions (`src/reencrypt_metadata.rs` and
    `src/reencrypt_transition.rs`), with commit fault injection and
    the pinned stock metadata recovery oracle. Reencryption commands
    and production data rewriting remain disabled.
  - **D3a.** Repeatable authenticated free-keyslot-space erasure before
    `kill-unbound` retires old normal slots, with journal cleanup and
    interruption tests. This establishes the cleanup operation required
    by the runtime; production data reencryption remains disabled.
  - **D3b.** Device stacks, authenticated recovery, online data rewriting,
    UUID exclusion, deferred temporary-map cleanup and the bidirectional
    runtime oracle. U1/U3 selector changes join E's atomic runner cutover.
- **E.** The cutover, in one landing (AGENTS.md principle 4):
  - td-protector's runner and every caller switch to td-luks;
  - the image ships `/bin/td-luks` where it ships cryptsetup;
  - the recipes are deleted: cryptsetup, libdevmapper, json-c, popt,
    util-linux-libs, the `linux-source` 4.14.67 seed, and their pins and
    watches;
  - the kernel recipe drops `CONFIG_CRYPTO_USER_API_*` and asserts it
    absent;
  - ENCRYPTION.md increment 3, td-install/DESIGN.md D6 and D7,
    td-protector/DESIGN.md "LUKS runner", UNSAFE.md, UPDATING.md
    and AGENTS.md are amended to match.
