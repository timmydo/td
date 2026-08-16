# td-install — the deployment path

This file is the normative specification for td's **deployment path**: how a
disk is laid out, how a deployment bundle is published onto it, how that
bundle's authenticity is established, and how a machine moves from one
deployment to the next. Where the code and this document disagree, one of
them is a bug.

It is deliberately wider than one crate. The path runs through `td-install`
(the installer), the signing half in `td-net`, the on-disk protocol and
transactional publish in `td-boot`, and the update channel that will become
`td-update`. Those were three workstreams and are now one, with one owner and
one branch, because every coordination point between them was a place where
two agents could each write half of a mechanism. Several invariants below are
not checkable by any compiler or lint; they are recorded here because the
failure modes they prevent are silent.

## 1. Why this file exists

Two landed commits — `1dd502f6` (the GPT writer) and `44a8ce74` (the FAT32
ESP writer) — cite `plan.txt` as their normative roadmap. **`plan.txt` is not
in git.** It is untracked in one working copy, which means it is invisible in
the integrator's clone and in every fresh worktree: a reader following those
citations finds nothing, and the specification the work was built against
cannot be read by the person reviewing it. That is the whole reason this file
is the first increment of the workstream rather than a later tidy-up.

The roadmap now lives here. `plan.txt` steps 1–6 are built and are recorded
as §2. Step 7 — "add partitioning, UEFI/bootloader, firmware, and broader
hardware config" — is this workstream, and §10 is what replaces it. Future
commits on this path cite this file. The two already-landed ones cannot be
amended; they are on `main`.

## 2. What already holds

The deployment **bundle** is the reproducible artifact and already exists:

```text
deployment/
|-- bzImage
|-- initramfs.cpio
|-- root.erofs
`-- manifest
```

The manifest is strict, versioned ASCII: header `td-deployment-v1`, then
exactly three `<64 lowercase hex>  <label>` lines for `bzImage`,
`initramfs.cpio` and `root.erofs`, a trailing newline, and nothing else
(`td-boot/src/main.rs`, `parse_manifest`). It is bounded at 4096 bytes.

The **deployment id is the sha256 of the manifest bytes** — computed at
publish and recomputed at every verify, which is what makes the id a claim
about content rather than a name someone chose.

On the persistent volume:

```text
/td/deployments/<deployment-id>/{bzImage,initramfs.cpio,root.erofs,manifest}
/td/boot/current    -> ../deployments/<id>
/td/boot/previous   -> ../deployments/<id>
/td/boot/attempts/
@var                (Btrfs subvolume, mounted read-write)
```

`td-boot` already publishes transactionally (`install_deployment` →
`publish_bundle`): stage into a temporary directory, verify every payload,
flush, atomically rename to the deployment id, then update `previous` and
`current`. It consumes a boot attempt before kexec and rolls back to
`previous` when attempts are exhausted. `/` is a read-only EROFS image
attached to a loop device; `@var` carries mutable state; `/run` and `/tmp`
are tmpfs.

What is **missing**, and what this workstream is:

- No installer on any IMAGE. `td-install` writes the layout and a recipe now
  links it statically for the target, but nothing packs it anywhere a person
  could run it, so a machine is still only ever reached by QEMU `-kernel` —
  a development harness, not a way to boot hardware.
- No bootloader, and nothing that writes an ESP.
- No **authenticity**. Manifest hashes prove integrity and transaction
  completeness. They prove nothing about who produced the bundle: anyone who
  can write the volume can write a manifest that matches their own payloads.
- No update channel.

`engine/src/gpt.rs` + `engine/src/fat.rs` (with `engine/src/crc32.rs`)
landed toward it and were consumed by nothing until §10 item 7a, which is
the installer that writes what they produce — and by a TARGET binary since
7b's recipe, which is what makes them shipped code rather than host code
that happens to compile.
`engine/src/ed25519.rs` + `engine/src/sha512.rs` (verify-only, `aa347e60`)
were in that list until §10 item 5: td-boot compiles them in, and since item
6's flip the boot path CALLS them — no slot is selected whose manifest does
not verify under the trust root in the selector's own rootfs.

## 3. Hard invariants

**D1. There is exactly ONE bundle writer.** `td-boot`'s
`install_deployment` is it. `td-install` formats a disk and then *delegates*
the publish; it does not learn to write a deployment directory, update a
selector, or account for attempts. The transactional publish is the part of
this path where a partial write is a machine that does not boot, and two
implementations of it would be two chances to get the rename order wrong —
with the second one exercised only by installs, which are the rarest
operation on the path and the one nobody watches.

That writer has TWO ways in and one body, which is the distinction the rule
is about: `td-boot install <device> <mountpoint> <source>` mounts and then
publishes, and the verb 7c adds publishes into a volume root that is already
writable. The second exists because `td-install` cannot mount (D8) and a
regular-file destination has no partition device to mount anyway (D9); the
first is what a running machine and the update path use. Neither reimplements
the other — `install` calls the same function once its mount has succeeded.

Consequently `td-install` shares `td-boot/src/protocol.rs`'s
`DEPLOYMENTS_DIR`, `SELECTOR_PREFIX` and `BOOT_DIR` through the same `#[path]`
include, rather than restating the layout. A layout stated twice is a layout
that can disagree with itself, and the disagreement surfaces at the first
boot after an install rather than at build time.

**D2. Sign before install, and verify fail-closed.** The signing half lands
*first*, before `td-install` exists. This ordering is not stylistic. The
system oracle already stages bundles, so signing and verification are
testable today with no new boot path at all; whereas an installer written
first would be a bundle publisher that must then be retrofitted, and — worse
— an installer that stages an *unsigned* bundle produces a machine that fails
closed on its first boot. The first thing the installer ever does would be
the thing that bricks it.

Fail-closed means: a missing signature, a malformed signature, a signature by
a key other than the trusted one, or a manifest that does not verify, each
refuse the deployment. There is no permissive mode and no flag to add one.
"Trusted" rather than "pinned": §6 settled on a key read at runtime from the
selector initramfs, so there is no build-time pin to be other than.

**D3. The signature is DETACHED and the id is unchanged by it.** The
deployment id stays `sha256(manifest)`. The signature lives beside the
manifest as `manifest.sig`, and is not an input to the id. This is what lets
a bundle be re-signed under a new key — key rotation, or a bundle promoted
from a test key to a release key — **without changing its identity**, so a
machine that already has that deployment installed still recognises it as the
same one. Folding the signature into the id would make every re-signing a
different deployment, and rollback would stop finding what it rolled back to.

Item 6's flip gave that a consequence worth writing down. A re-sign now decides
whether the deployment still BOOTS, and `install` cannot check the replacement:
it runs on the real root, which per §6 has no trust root. Publishing already
refuses to remove a signature (`SignatureWithdrawn`); it cannot refuse to
replace one with a signature under the wrong key, because telling those apart
is the thing it has no key for. Mostly this is absorbed by the mechanism that
exists for it — `current` fails to authenticate and the boot rolls back to
`previous`. What it is not absorbed by is the case where both slots select the
SAME deployment, which is exactly the state a freshly installed machine is in:
there the re-sign costs the machine its boot, and nothing before the next
power-on says so. So `td-boot install` warns on stderr when a re-sign replaces
the signature of a slot that selects it — a warning rather than a refusal
because refusing is D3's feature, and rather than silence because the next
report is a machine that does not come back. The real answer is a trust root on
the installing rootfs, which is item 7's to settle.

**D4. Signing happens outside the derivation.** A signing key inside a build
would break both reproducibility (the output depends on a secret) and offline
purity (the key is an undeclared input). Signing is a host-side `td-net`
operation over an already-built bundle, exactly as `td-subst sign` is over
narinfos. A private key never enters the target graph, never enters a recipe,
and never appears in a store path.

**D5. The ESP is not the system.** The EFI System Partition holds a *fixed*
boot stub and nothing per-deployment. It is FAT32 — the one filesystem in td
that is not td's choice, because UEFI 2.10 §13.3 requires firmware to be able
to read it — and it is therefore the least trustworthy surface on the disk:
firmware writes to it, other operating systems' installers write to it, and
it has no checksums. Everything that decides *what runs* lives on the Btrfs
volume behind `td-boot`'s verification. The ESP's job is to reach `td-boot`,
and updating a deployment must never require writing to the ESP.

**D6. Nothing on the BOOT path is a third-party program.** That is already
true — `td-boot` reaches only `td-init` applets, and `losetup` moved into
`td-init` precisely so that no absolute path to a foreign multicall sits
between a machine and its root filesystem. This workstream does not
reintroduce one. The single deliberate exception is at INSTALL time, D7.

**D7. `mkfs.btrfs` is an approved install-time exception, bound at build
time.** `td-install` execs the shipped, source-built `btrfs-progs` to create
the volume; writing a Btrfs formatter in Rust is not a thing this workstream
is going to do well, and a wrong one produces a volume that mounts and then
loses data. The cost is real and is the `losetup` lesson: an install would
depend on a third-party program existing at a path, with nothing tying the
two together, so dropping it from the image would break installs with **no
build-time complaint**. So the landing that execs it also carries the binding
`td-boot/src/protocol.rs` already models for applets — a named constant that
the recipe-side image check consumes, so an image that does not provide
`mkfs.btrfs` **reds the build** instead of failing an install on a machine
someone is standing in front of. The binding lands *with* the exec, not
after it.

**D8. No new `unsafe`.** Everything here is ordinary file I/O: partition
tables and filesystems are bytes at offsets, and efivarfs is a filesystem.
`UNSAFE.md`'s roster is unchanged by this workstream. If some later increment
appears to need a syscall, that is an amendment to `UNSAFE.md` and is
reviewed as one — not a thing to discover in a diff.

**D9. The installer writes to a device OR to a regular file, and the file
case is what the oracle exercises.** One code path, two destinations. This is
what makes the installer testable headlessly at all: the oracle hands it a
file, then boots that file under OVMF. An installer whose tested path and
shipped path differ is an installer tested somewhere other than where it
runs.

## 4. Disk layout

Positions are in SECTORS of the destination's own logical size, not in
512-byte units. The entry array is 16 KiB and the first partition starts at
1 MiB whatever that size is, so the LBA numbers below differ between a 512e
and a 4Kn disk while the byte offsets do not:

```text
LBA 0                    protective MBR
LBA 1                    primary GPT header
LBA 2..                  primary partition entry array (128 x 128 bytes;
                           16 KiB, so 32 LBAs at 512 and 4 at 4096)
1 MiB..                  partition 1: EFI System Partition, FAT32
                         partition 2: td, Btrfs
last - (1 + array)..     backup entry array
last                     backup GPT header
```

Partitions start at 1 MiB and are megabyte-aligned, which is what every
current partitioner does and what keeps writes off the wrong side of an erase
block on flash media. At 512 bytes that is LBA 2048 and at 4096 it is LBA
256; `td-install` derives it by dividing rather than naming either, because a
4Kn disk laid out in 512-byte sectors has every LBA off by a factor of eight
and firmware reads that as no table at all.

**The ESP is at least 33 MiB.** This is not a round number chosen for
comfort: `engine/src/fat.rs` refuses to format a volume below **66599
sectors — 32.52 MiB**, because FAT32 is *defined* as at least 65525 clusters
and a volume below that line is FAT16 however its BPB is labelled. A reader
that counts clusters rather than trusting the label — which firmware does —
reads it as FAT16 and the mount fails. A 32 MiB ESP cannot work at any
cluster size. The shipped default is larger than the floor, because the ESP
holds a kernel.

The Btrfs partition takes the remainder and carries `@var` plus
`/td/deployments`. It is the only partition that grows.

## 5. Firmware entry

td boots by the **removable-media path**: `\EFI\BOOT\BOOTX64.EFI` on the ESP,
which every UEFI implementation boots when no NVRAM boot entry names anything
else. That path needs no `efivarfs` write at all, which is why it comes
first: NVRAM is per-machine mutable state that a reinstall cannot rely on and
a virtual machine may not persist, and firmware that has lost its boot
entries is firmware that still boots removable media. Writing
`Boot0000`/`BootOrder` through efivarfs is a later increment and an
optimisation, not a prerequisite.

`BOOTX64.EFI` is an **EFI-stub kernel**: `CONFIG_EFI` + `CONFIG_EFI_STUB`, so
the kernel image is itself a PE executable the firmware can load, and no
third-party bootloader exists on the path. td's kernel does not have this
today — `linux-x86-64.rs` builds from `allnoconfig`, so both are off, and on
x86 `CONFIG_EFI` pulls ACPI in; that dependency is to be confirmed against
the pinned tree rather than taken from this file.

Firmware passes **no command line**, so the stub's must be built in
(`CONFIG_CMDLINE`). That costs nothing under this design and is the reason
the design is shaped this way: the kernel on the ESP is a *fixed* stub whose
only job is to reach `td-boot` on the Btrfs volume, and the per-deployment
command line is the one `td-boot` already builds for its kexec. The ESP
therefore never changes when a deployment does, which is D5 restated as a
property of the boot flow rather than as a rule.

## 6. Signing and keys

**A dedicated deployment-signing key**, separate from the `td-subst` cache
key. They are different trust domains: `td-subst`'s key says "this store path
came from this builder", and the deployment key says "this is a system you
may boot". Sharing one key would mean a compromise of the substituter's
signing key is also a compromise of every machine's boot chain, and the two
have different lifetimes and different exposure.

Signing is host-side in `td-net`, where `ring` already signs: **`td-deploy`**,
its own applet rather than a verb on `td-subst`, because the two are the
different trust domains named above and one tool serving both invites one key
serving both. `td-deploy sign MANIFEST PRIV OUT` signs the manifest's exact
bytes — no canonicalisation, since the verifier hashes that same file to
derive the id — and writes the detached signature (D3, D4).

What it will sign is bounded by td-boot's OWN contract rather than by a
restatement of it: `MANIFEST_HEADER`, `MANIFEST_NAME`, `MAX_MANIFEST_BYTES`
and `MANIFEST_SIG_NAME` live in `td-boot/src/protocol.rs`, which `td-net`
`#[path]`-includes exactly as the recipes do. So the signer refuses a
manifest larger than the verifier will read, and one that does not carry the
verifier's header — a signature that could never be checked fails on the
builder rather than on a machine. The header check is also the only thing
separating the two signing domains at the *message* level, since neither tool
tags what it signs; without it a deployment key would sign any blob handed to
it. It reads the manifest the way td-boot does, too — `symlink_metadata`
first, then an open whose device and inode must still match — because a
signer that accepts what the target refuses produces a bundle that fails at
boot instead of at signing.

`td-deploy keygen PRIV PUB` produces a pkcs8 private half and a raw 32-byte
public half. The private half is created **`0600` and exclusively**: a
signing key any local reader can copy forges whatever it authorises, and a
second `keygen` over an existing path must be an error rather than silently
destroying a key machines are still pinned to. **No private key is ever
committed**, which is the repository's existing practice: `tests/td-subst.pub`
is committed and its private half never was.

Verification is `engine/src/ed25519.rs`, which is verify-only and strict: it
refuses `S >= L`, non-canonical encodings, keys outside the prime-order
subgroup, and small-order `R`. It reaches SHA-512 as `crate::sha512`, so a
consumer must declare **both** modules at its crate root; they are a pair.

### Where the trusted public key lives — SETTLED

**In the SELECTOR initramfs** (`boot/selector-initramfs.cpio`), in a cpio
archive the harness appends, at `TRUSTED_KEY_PATH` — `etc/td/deployment.pub`,
declared in `td-boot/src/protocol.rs` so the writer and the reader cannot
disagree about the spelling.

*Which* initramfs is the whole of the answer and the sentence below that says
only "the initramfs the firmware loads" was not specific enough to prevent a
wrong implementation of itself. td builds two: the selector, which firmware
loads and in whose rootfs `td-boot boot` runs when it selects, verifies and
kexecs; and the deployment's own, which is a payload the manifest hashes and
which lives on the Btrfs volume. Only the first is right. A key in the second
is inside the artifact being authenticated — a hostile update source supplies
bundle, key and signature together and self-authenticates, which is exactly
the Btrfs-volume weakening the first bullet below forbids by name. It also
breaks D3: the key would be hashed into the deployment's initramfs, so
rotating it would rename the deployment.

The reasoning that got here is kept below rather than replaced by the answer,
because a first answer turned out to be unbuildable and that is the
load-bearing part of the record.

The intent was that td-boot's key be pinned at build time. It cannot be a key
the test harness generates: every recipe embeds its sources with
`include_str!`, which Rust resolves when *td-recipe-eval itself* compiles, and
recipes are content-addressed targets with no per-run parameter. So a
build-time pin can only carry a key that exists before the build — and since
no private key may be committed, nothing at test time holds the matching half
to sign a bundle with. **Build-pinning and "no committed private key" cannot
both hold.** The manifest changes with every system build, so its signature
must be produced per build; a committed *fixture* signature cannot stand in.

What follows is that the trusted key is read at runtime. The important part
is WHERE, and the distinction is not "compiled in vs. read from a file":

- A key on the **Btrfs volume**, beside the deployments it authorises, is a
  real weakening. Anyone who can write a forged deployment can write the
  matching public key next to it, and the signature degrades to an integrity
  check the manifest hashes already provide. Do not put it there.
- A key travelling in the **same artifact as td-boot** — the initramfs the
  firmware loads — moves the trust boundary hardly at all. Substituting it
  requires writing that artifact, which is what patching a compiled-in
  constant would require too.

Three differences survive even then, and each is a thing to get right rather
than a reason to prefer the constant: something must WRITE the key, and any
path that writes a trust root can be induced to write the wrong one; a
missing key becomes a runtime branch, where the tempting branch is the
fail-open D2 forbids, so absence must be a refusal; and the trust root
becomes per-machine state that the reproducible artifact does not record.

Against the threat this signature exists for — a hostile or compromised
update source — the two are equivalent: that attacker supplies bytes and
touches neither the ESP nor the key. Against a local-disk attacker td has no
defence either way today (§9: no Secure Boot, no TPM, no encryption), so
"pinned at build time" buys less here than the phrase suggests.

The mechanism that makes a per-run key workable is verified: Linux accepts
**concatenated cpio archives** for initramfs, so a harness can append a
one-file archive carrying that run's public key without any recipe being
parameterized. Checked against the pinned tree, `linux-7.1.4`,
`init/initramfs.c` — `unpack_to_rootfs`'s driver loop restarts the state
machine at `Start` on each `070701` magic and skips NUL padding between
archives, and the compressed path does the same in `flush_buffer`. Two
consequences: the appended archive must begin **4-byte aligned** (the branch
is guarded by `!(this_header & 3)`), and a later archive's file replaces an
earlier one's (`clean_path` plus `O_TRUNC`), which is what lets a key be
appended rather than reserved.

What a *misaligned* appendix costs was stated wrongly here at first, and the
correction matters because the whole appeal of the mechanism was that the
kernel would catch the mistake loudly. It does not, twice over. `do_reset`
eats the NUL run and errors `broken padding` (`:324-331`) — not `invalid
magic`, which is unreachable here, because that error sets `message` and the
driver loop is `while (!message && len)`. And td's selector initramfs — the
one this appendix rides — is an
**initrd** (`CONFIG_INITRAMFS_SOURCE=""`, qemu `-initrd`), so it takes
`:726-733` rather than the `panic_show_mem` the built-in archive gets at
`:714-716`; with `CONFIG_BLK_DEV_RAM` off — allnoconfig leaves it off and the
recipe's delta list does not add it — that arm is one
`printk(KERN_EMERG "Initramfs unpacking failed: %s\n")` and **the boot
continues**, base archive extracted and the key absent. That is precisely the
"a missing key becomes a runtime branch" hazard named three paragraphs above
— whose tempting branch is the fail-open D2 forbids — arriving through the
very mechanism meant to avoid it. So the harness's own padding is the entire
defence, and item 6's fail-closed flip is what turned a keyless boot from a
silent one into a refusal: `run_boot` reads the trust root before it touches
the volume, so the missing key is what gets named.

One tension in the list above is worth naming rather than leaving to be
noticed: D5 calls the ESP the least trustworthy surface on the disk —
firmware writes it, other OS installers write it, nothing checksums it — and
the second bullet nonetheless prefers it to the Btrfs volume. Both hold. D5
is about the ESP relative to a volume td controls and verifies, and the
bullet is about the ESP relative to a *compiled-in constant in a binary that
already lives on the ESP*. An attacker who can rewrite the key file there can
rewrite td-boot itself, so pinning buys nothing against them. What D5 does
rule out is treating the ESP as a place where a key could be *safely* left
for something else to trust; the key is only as good as the binary beside it,
and that is the whole of the claim.

**What is built.** The system oracle generates a throwaway ed25519 keypair per
run from `/dev/urandom` (`RunTrust`). The public half is appended to a *copy*
of the verified selector initramfs — the copy is what boots, so the recipe
output and its own manifest stay intact — and the private half signs the
manifest of **every** deployment the run stages onto the volume: the seed that
`current` and `previous` point at, and every candidate. Signing only candidates
would leave the ordinary boot path unsigned, and fail-closed verification would
then refuse the deployment nearly every mode boots. Nothing is committed and
nothing is stored; the private half is dropped when the run ends.

This is what dissolves the contradiction recorded above as unbuildable.
"Build-pinned" and "no committed private key" cannot both hold — and neither
does. The key is per-run.

D3 survives because the key is not in any deployment: the deployment id stays
`sha256(manifest)`, re-signing under another key changes only `manifest.sig`,
and `run_system` — which recreates its fixture and requires the ids to be
unchanged — keeps working. A key hashed into a deployment's initramfs would
break all three at once.

Two things the appendix must carry that are easy to omit, both because the
kernel declines in silence rather than complaining. It emits its own `etc` and
`etc/td` directory entries — derived from `TRUSTED_KEY_PATH` rather than
written beside it — since a missing parent is `filp_open` returning 0
(`init/initramfs.c:385-387`), and a parent that exists but is not a directory
is ENOTDIR through the same path. And it is padded to a 4-byte boundary at the
join, which per the correction above nothing downstream would report.

### The signature file

`manifest.sig`, beside `manifest` in the deployment directory
(`MANIFEST_SIG_NAME` in `td-boot/src/protocol.rs`). Its contents are the
detached ed25519 signature as **lowercase hex with a trailing newline** — 129
bytes. The verifier must tolerate the newline; `from_hex` trims. This is
recorded because it is the wire format between the two halves, and a format
that lives only in the signer's code is one the verifier can disagree with.

### What the publish path carries

`td-boot install` carries the detached signature. Two things used to stop it
reaching a machine at all, and both are closed:

- `publish_bundle` copied exactly `bzImage`, `initramfs.cpio`, `root.erofs`
  and `manifest`, so a `manifest.sig` beside the manifest was silently
  dropped. It is staged with the rest now, and the staged directory is read
  back for it — the id is the manifest's hash and is structurally blind to a
  detached file.
- Publishing early-returned when the destination's deployment id matched.
  Since re-signing deliberately does not change the id (D3), a rotated key
  could never update an installed signature. The id still says the PAYLOADS
  agree — it is the hash of the manifest naming their digests — so the
  signature is what is left to compare, and an already-published deployment
  is now one of four things:

  | destination | source | outcome |
  |---|---|---|
  | same id, same signature | signed | no-op, as before |
  | same id, no signature | signed | signature added |
  | same id, different signature | signed | signature replaced in place |
  | same id, signed | unsigned | **refused** |
  | different id | — | refused, as before |

  Five rows for five inputs, spelled out rather than folded: adding a
  signature to an already-installed unsigned deployment is what a first
  signed release does to a running machine, and it has its own test.

  Replacing in place is sound only because the ids match, which means the
  manifest bytes match, which means the payloads just verified are the same
  contents; nothing changes but which key vouches for the deployment. It goes
  through a temporary and a rename, because a truncated signature verifies as
  nothing and would strand a machine on a deployment it cannot authenticate.

  The withdrawal case is refused rather than performed because removing a
  signature is a downgrade, and quietly doing nothing would ignore what the
  caller asked for. Neither is a decision an installer should make silently.

  Its cost is worth stating rather than discovering: the refusal comes from
  `publish_bundle`, which `install_deployment` calls BEFORE any selector
  work, so it fails the whole activation and not just the signature carry.
  An operator holding an unsigned but byte-identical copy of an installed
  signed deployment cannot use it to re-point `current`; the recovery is to
  obtain the signature or remove the deployment directory. Note the
  asymmetry with the row above it, which silently ADDS a signature and
  proceeds. Both are deliberate — one direction gains authenticity and the
  other loses it — but only one of them takes an unrelated operation down
  with it.

The signature is **optional** at this stage: nothing verifies it yet, so
requiring one would make every existing bundle uninstallable for no gain.
What is NOT optional is a signature that exists and is malformed — a
symlink, a directory, or larger than `MAX_SIGNATURE_BYTES` is a refusal, not
a bundle read as unsigned. Downgrading a bad signature to "no signature" is
precisely the fail-open D2 forbids, and the verifying half must not inherit
it from the reader.

### Signing cannot be done by the verifier — what tests may assume

`engine/src/ed25519.rs` exposes `verify` and no signer — only that
function plus `PUBLIC_KEY_LEN` and `SIGNATURE_LEN`. There is no signing in
THAT FILE and there must not be: **td-boot `#[path]`-includes it** (§10
item 5), so anything added there lands in the boot binary, whose job is to
refuse what does not verify. A signer on the boot path would be a crypto
surface serving no boot-time purpose. This paragraph stood in the future
tense for exactly one commit — the rule was written down before the
include landed, on the grounds that afterwards the cost of having got it
wrong is a boot binary carrying a signer.

`engine/src/ed25519_sign.rs` is therefore a SEPARATE module, and the
separation is the whole design: td-boot does not include it, so it cannot
reach the boot path. It has exactly one caller — the recipe-check oracle,
which must sign a manifest whose digests change with every build and so
cannot use a committed fixture signature. It is hand-rolled rather than
`ring` because the check crate may carry no external dependency and the
host signer is unreachable from a gate: no recipe builds td-net, and
nothing puts it on a check's PATH.

Neither half of the split is enforced by the compiler, so both are
asserted against the tree by `builder/src/affected.rs`'s table test — the
same place `TARGET_INCLUDED_ENGINE_SOURCES` membership is written out by
hand. No file under `td-boot/src` may name `ed25519_sign`, and
`ed25519.rs` may contain none of `fn sign`, `fn public_key` or `SEED_LEN`
— what a signer needs rather than what one might be called. Without that,
a later "just reuse the verifier's file, it already has `Point`" refactor
lands a signer in the boot binary with every gate green.

That is a new crypto surface and is recorded as one. It signs THROWAWAY
per-run test keys only; `td-deploy` remains the signer for anything that
authorises a real deployment. Its nonce is RFC 8032's deterministic one,
so there is no RNG to misuse, and it makes no side-channel claim — note
that it drives `ed25519.rs`'s deliberately variable-time `scalar_mul` with
a SECRET scalar, the one caller for which that function's "every input is
public" does not hold. Acceptable here because the keys are throwaway and
the sandbox is the attacker's absence, and recorded at both ends.

Correctness is pinned three ways, because a signer cannot check its own
work — and **where each runs differs**, which matters more than the count:

| pin | runs in the gate? |
|---|---|
| RFC 8032 §7.1's five vectors (the standard's own) | yes — `cargo test --workspace` |
| round-trip through `ed25519::verify` | yes — same |
| differential against `ring` (`net/src/ed25519_cross.rs`) | **no** |

The differential is the one that matters most — a signer agreeing with
td's own verifier and with nothing else would pass every test in the
engine and still emit signatures no other implementation accepts — and it
is precisely the one no gate runs, because nothing builds td-net from
source (§7). It is a developer and prep-time check, run with
`CC=<cc> cargo test --manifest-path net/Cargo.toml`. The RFC vectors are
the standard's own and carry the gated half on their own, which is why
this is a stated gap rather than a blocker; the same gap already applies
to the verifier and is recorded in §7.

The consequence lands on tests. td-boot's own tests **cannot produce a
signature**, so the positive path is exercised with committed fixtures — a
public key, a canonical manifest, and its detached signature, generated once
by `td-deploy`. No private key is committed, exactly as `tests/td-subst.pub`
already establishes.

What makes the triple self-consistent is the SIGNATURE over the manifest
bytes, and nothing else. An earlier draft of this paragraph said the fixture
manifest names the digests of payload bytes the test writes; it does not,
and it does not need to — authenticity is decided over the manifest's bytes
and the digests it names are never resolved on this path. `td-boot/tests/README`
is the normative note on the fixtures and says so.

Every NEGATIVE assertion — wrong key, tampered manifest, absent signature,
truncated signature, signature over a different manifest — needs no signer
at all, and those are the fail-closed ones that matter. The oracle is the
other half: it signs per run with a throwaway key rather than a fixture,
using `ed25519_sign.rs` above. (An earlier draft of this section said the
oracle "has `td-deploy` available". It does not, and that is the whole
reason the module above exists — no recipe builds td-net and nothing puts
it on a check's PATH.)

## 7. Engine sources compiled into target binaries

`builder/src/affected.rs` routes a changed file to the checks that can catch
a break in it. Most `engine/src/*` sources reach only the two control-plane
bins. A few are `#[path]`-included into **target** binaries as well, and
which ones is written down in `TARGET_INCLUDED_ENGINE_SOURCES`:

| source | target consumer |
|---|---|
| `sha256.rs` | td-boot, td-compositor corpus verifier |
| `crc32.rs` | td-install (via gpt) |
| `gpt.rs` | td-install |
| `fat.rs` | td-install |
| `ed25519.rs` | td-boot, td-net's `cfg(test)` ring differential |
| `sha512.rs` | td-boot (pair with ed25519), td-net's ring differential |
| `ed25519_sign.rs` | td-net's `cfg(test)` ring differential ONLY — never td-boot |

Each entry stores the full consumer list the router prints in its note; the
column above shows only the target half that distinguishes these seven.

The last row is the one that is a declaration in both directions: being in
this table records that a `#[path]` include exists, and its note records
where the include may NOT go. §6 has the assertion that holds it shut.

**The table is a declaration, not what selects a gate.** `recipe-checks` is
itself a build gate and the engine rule adds every build gate, so *all* of
`engine/src/*` already routes to it — `engine/src/json.rs` included. The
`p == "engine/src/sha256.rs"` equality this replaces was therefore dead for
routing and live only for the explanatory note, and the assertion that
claimed to pin it (`assert_target!("engine/src/sha256.rs", "recipe-checks")`)
passed for any engine source at all. What the table buys is that the set is
written down once, named in the note, and pinned per entry — against the
note, which is the only rendered thing that distinguishes a target-included
source, and against the filesystem, so a renamed entry cannot quietly stop
matching. The explicit `recipe-checks` selection stays beside it so the
requirement does not rest on `recipe-checks` remaining a build gate.

It follows that populating the table ahead of its consumers costs **nothing**
— `add_target` dedups, and a table path and a non-table path render the same
target set, differing only in order. That is why all six land at once rather
than one per increment: the alternative is editing `affected.rs` in three
later landings, which is the coordination point the single-owner change
exists to dissolve.

What being in the table does NOT mean is that some gate builds the consumer.
Nothing builds td-net from source (`aa347e60`), so a change to `ed25519.rs`
runs its ring differential — the primary correctness pin for that verifier —
**nowhere**. Naming td-net in the note does not close that; it makes it
visible at the routing decision, and closing it belongs to td-net.

A target consumer whose source is *missing* from the table is the failure
that matters: a target binary whose engine sources nothing checks.

### The other shared source: `td-boot/src/protocol.rs`

Not an engine source, and so not in the table, but the same shape: it is
`#[path]`-included by `recipes/src/recipes/system-x86-64.rs`,
`recipes/src/bin/td_recipe_eval/checks/qemu_boot.rs` and — since the manifest
header and size bound moved into it — `net/src/main.rs`. It gets its own
routing arm ahead of the `td-boot/*` glob, because the glob selects nothing
that compiles td-net: no gate builds td-net from source, the recipe-graph
warm does, and that is a chain target. The arm is `cargo-test` + `check` +
`recipe-checks` + the chain, and what pins it is the contrast — the
assertions require `td-boot/src/main.rs` to select none of the three
bootstrap targets, so the rule has to be about this FILE rather than about
the crate.

## 8. Oracles

The existing QEMU system oracle boots with `-kernel` and stays. It is the
fast path and it tests the deployment machinery, not the firmware.

A **second** oracle boots the real thing: `-M q35` with an OVMF pflash
code/vars pair, the vars a writable per-run copy. It needs no new host
dependency — the qemu that `find_qemu()` already locates ships
`edk2-x86_64-code.fd` and `edk2-i386-vars.fd` under its own `share/qemu` — so
the firmware is located relative to that binary and its absence is a loud
failure, as `find_qemu`'s is.

The oracle signs with a **per-run throwaway key**: generate a keypair, sign
the staged bundle, build `td-boot` pinned to that run's public key, boot, and
require the machine to reach its target. This exercises the entire
keygen → sign → pin → verify chain every run, and it cannot rot into a
fixture that passes without testing anything. Its **negative control** is a
second fresh key: a bundle signed by a key the build did not pin must fail
closed. A signing test without that control passes just as well against a
verifier that returns `true`.

Both halves of the disk are already validated against independent
implementations: `sfdisk --verify` and `fsck.vfat` read what `gpt.rs` and
`fat.rs` write, and EDK II under OVMF has parsed the GPT, mounted the ESP,
and read a file back byte-exactly. Those checks stay; the oracle is what
makes them a *boot* rather than a parse.

## 9. What this path does not do

Recorded so it is not re-litigated, and so nothing here is mistaken for a
stronger claim than it is:

- **No Secure Boot.** Verification is td's own signature check inside
  `td-boot`, after firmware has already run the stub. Chaining to the
  platform's own trust root (shim, MOK, a signed PE) is a separate trust
  policy increment with its own key management, and D2's fail-closed check is
  not a substitute for it.
- **No measured boot / TPM.** Nothing is sealed to PCRs.
- **No ESP redundancy.** One ESP. A machine whose ESP is destroyed is
  recovered by reinstalling it, not by a second copy.
- **No A/B partition scheme.** Deployments are directories on one volume with
  `current`/`previous` selectors, which is already transactional. A/B
  partitioning solves a problem td does not have.
- **The disk is not encrypted.**

## 10. Sequence

Ordered by dependency, not by size. Each is one landing with its own tests.

1. **This file.** Docs-only.
2. **The `affected.rs` routing table** (§7). Mechanism only; no behaviour
   change, and the entries land with it.
3. **Signing, host-side** (§6, D2, D3, D4): `td-deploy sign` emits a detached
   `manifest.sig`. Nothing consumes it yet — the same shape `aa347e60` used
   to land the verifier, and it is what lets the target half land against a
   signer that already exists.
4. **Publishing carries the signature** (D1, D3): `td-boot install` stages a
   `manifest.sig` beside the manifest and treats a changed signature on an
   already-published deployment as an update rather than a no-op. Still
   nothing that verifies — this is the half that makes a signature able to
   REACH a machine, and it needs no answer to the §6 key question above.
5. **The verifier reaches the target** (§6): td-boot `#[path]`-includes
   `ed25519.rs` and `sha512.rs`, its recipe stages them, and it gains a hex
   decoder, a trusted-key reader, and `authenticate_manifest` — reached by
   one new verb, `td-boot authenticate <deployment-directory>
   [trusted-key]`, which answers authenticity and nothing else. (The key
   became optional with item 6's provisioning half, defaulting to
   `TRUSTED_KEY_PATH`; it was mandatory when this landed.) The BOOT
   path is untouched, which is the shape items 3 and 4 landed in; what this
   settles is that the target-static build carries those sources, separately
   from the boot-path change that depends on it.

   The verb is what makes that true rather than a convenience. rustc drops
   dead code, so declaring the modules and calling nothing compiles the
   verifier and then discards it: measured on the recipe's own layout, the
   binary was 24576 bytes SMALLER with the functions unreferenced. A
   `#[path]` include with no caller proves the sources TYPE-CHECK and
   nothing more.

   The positive case is a committed fixture triple (`td-boot/tests/`), since
   td-boot has the verifier and deliberately not the signer; every negative
   needs no signer at all.
6. **Verification, target-side, fail-closed** (§6, D2): `td-boot` refuses a
   deployment whose manifest does not verify. This is the increment that
   needed §6's key question answered — where the trusted key lives — because
   it is the first one whose ANSWER has to reach a running machine. It was
   scoped with item 3 as a single landing and has been split five times: as
   that question turned out to be unsettled, as carrying the file turned out
   to be its own increment, as getting the verifier into the target binary
   turned out to be separable from deciding what it trusts, and then twice
   more inside the answer itself.

   All three halves are done. First a newc cpio WRITER in the engine
   (`engine/src/cpio.rs`), since the mechanism is an appended archive and the
   tree had no writer at all. Then PROVISIONING: the oracle's `RunTrust` puts
   a per-run public key in the selector initramfs it boots and signs every
   deployment it stages with the private half. Both are host-side and change
   no boot path, which is what made them separable from the flip.

   Then the POLICY, deliberately last, because until td-boot refuses there is
   nothing to distinguish a run whose key arrived from one whose key did not
   — which per §6's alignment correction is also what a misaligned appendix
   silently produces. `run_boot` reads the trust root from the rootfs it is
   running in BEFORE it touches the volume, so a selector provisioned without
   one names the missing key rather than failing about whatever the first
   slot happens to be wrong about; and every slot the BOOT DECISION considers
   — `select_boot_deployment`, its verified-previous fallback, and both
   read-only paths — is authenticated before its payload digests are read.

   Authentication is a property of SELECTION and could not be anything else,
   which is what the answer to §6's key question forces: only the selector's
   rootfs has the key, so the td-boot that runs `root-loop` after kexec and
   the one that runs `success` after switch_root cannot check a signature.
   Nothing is lost, because the deployment id IS the manifest hash — a
   downstream reader holding `<id>/manifest` is holding the bytes the
   selector authenticated or it errors.

   What does NOT authenticate, and must not: `install`, which asks whether
   the fallback slot is intact from the real root where no trust root exists,
   and `verify`, the operator's diagnostic, which runs in the same place. The
   two questions are separate functions — `verify_slot` for state,
   `authenticated_slot` for the boot decision — because a key threaded
   through the shared one would turn every install into a hard failure.

   The manifest is read ONCE and the signature checked over the bytes the
   payload digests are then parsed out of. Reading it again to parse would
   leave a window in which a writer could serve a signed manifest to the
   check and a different one to the parse, and an attacker who can write the
   volume is the entire reason any of this is signed.

   `run_boot` re-verifies the chosen deployment under the mount whose handles
   go to kexec, and does that through the authenticated reader too. The id
   came from an authenticated selection, so the hashes alone already bound
   it; going through the authenticated reader makes "everything handed to
   kexec was authenticated" a property of the call rather than of a trace
   back to the id's origin, which is the half a later edit breaks silently.
   It is asserted over the SOURCE, because `run_boot` needs a block device, a
   mount and a working kexec, and no unit test has any of the three. That
   assertion pins the WHOLE trust-root call and not just the function's name,
   which is the sharpest thing review found here: `read_boot_trust_root(
   mountpoint)` reads the key off the Btrfs volume — the surface the first
   bullet of §6 rules out by name, since anyone who can write a forged
   deployment can write the matching public key beside it — and it passed
   every test in the crate. It is the most damaging single edit possible in
   that file, it is one word, and nothing saw it.

   Its negative controls are the point of the increment rather than its trim:
   a deployment signed under another key, an unsigned one, a manifest that
   does not hash to its directory name, and a rootfs with no trust root at
   all. Each was verified to red by removing the guard it names. The
   wrong-key control is a whole SECOND key rather than the trusted one with a
   bit flipped: a public key is a compressed curve point, so a flipped bit is
   not reliably a point at all, and a control built on one proves that a
   malformed key is refused rather than that somebody else's signature is.

   What is NOT proven is a real boot. The oracle for that is
   `qemu-boot-system`, which wants a warm store — a cold one means building
   the whole ladder. What stands in for it is that nothing can boot an
   unprovisioned selector: every selector that reaches qemu comes from
   `provision_selector`, and `VerifiedSelector`'s path is private, so the
   store output cannot be reached around it.
7. **`td-install`**, a standalone crate outside the workspace (D9): GPT +
   FAT32 ESP + Btrfs volume onto a device or a regular file,
   `#[path]`-including `gpt.rs`/`fat.rs`/`crc32.rs` and `protocol.rs`, and
   delegating the publish to `td-boot install` (D1). Carries the
   `mkfs.btrfs` build-time binding (D7). Registering a new crate is three
   touch points that must land with it: the cargo-test gate's one-package
   `Cargo.lock` assertion plus its clippy and test lines
   (`builder/src/gate_defs/325-cargo-test.rs`), a route and assertions in
   `builder/src/affected.rs`, and the workspace `exclude` list.

   Split in three, because the volume and the publish each bring a
   dependency the layout does not. **7a, the LAYOUT**, is landed: the crate,
   its three registration points, and a `layout` verb that writes the
   protective-MBR GPT and formats the ESP. That is what finally gives
   `gpt.rs` and `fat.rs` a consumer — §2 listed them as landed toward this
   and used by nothing.

   Two things in it are worth finding here rather than in the code. The
   disk LAYOUT constants live in `td-boot/src/protocol.rs` with the rest of
   the on-disk shape, for D1's reason: `td-install` writes the partitions
   and td-boot reads what is inside them, and a layout stated twice can
   disagree with itself at the first boot after an install. And the ESP's
   metadata region is ZEROED before it is formatted, because `fat.rs` states
   that precondition and cannot check it — it emits only what must be
   non-zero, so over a disk that already held a filesystem the bytes past
   the live FAT prefix read as ALLOCATED clusters. Zeroing the whole ESP
   would also satisfy it, at half a gigabyte of writes per install; the
   reserved sectors, both FATs and the root cluster are enough, and past
   them every cluster reads free.

   Neither the size nor the sector size is assumed from which destination it
   is. Both are asked: the size by seeking to the end, which answers for a
   device as well as a file and keeps D8 intact where `BLKGETSIZE64` would
   not, and the sector size from sysfs by the opened file's device NUMBER —
   `losetup`'s argument, since a path can name a different device than the
   descriptor is open on. A 4Kn disk laid out in 512-byte sectors has every
   LBA off by eight, which firmware reads as no table at all.

   **7b, the VOLUME**, is landed: `td-install volume` formats the volume
   partition and leaves it holding the read-write `@var` subvolume, with the
   `mkfs.btrfs` binding D7 requires beside the exec that needs it. **7c**
   delegates the publish to `td-boot`, into the staging tree rather than
   through a mount, and settles the install-time trust root; its own
   paragraphs are below the 7b ones.

   7b is itself two commits, and the RECIPE is the first of them. D7's binding
   is a build-time complaint about a missing program, so something has to
   build td-install with `btrfs-progs` declared beside it — and today nothing
   builds td-install at all. There is also no install image to pack it into,
   so the roster check `td-boot/src/protocol.rs` already models for td-init
   applets has no image to read; what it reads instead is td-install's own
   declared inputs. The recipe is worth landing on its own for the reason item
   5 was split off: a `#[path]`-including crate that only ever compiles on the
   host has not been shown to compile for the target, and the recipe is also
   the only place a test can EXEC `mkfs.btrfs`, since no host is required to
   have one. `recipe-checks` joins td-install's route with that recipe.

   **SETTLED, and the granted permission is not spent: 7b uses no partition
   device.** `mkfs.btrfs` writes a scratch image sized to the volume
   partition, and td-install copies that image into the partition through the
   whole-disk descriptor it already holds — which is exactly how it writes the
   ESP today. `BLKRRPART` was authorised as an `UNSAFE.md` amendment and is
   not needed, because it does not answer the question for BOTH destinations:
   a regular file has no partition device to rescan, so the scratch-image path
   has to exist regardless, and once it exists the ioctl buys a second code
   path for the one destination the tests cannot reach. D9 settles it — an
   installer whose tested path and shipped path differ is an installer tested
   somewhere other than where it runs — and D8 survives intact.

   Three properties of that copy belong here rather than only in the code. It
   writes only the chunks of the image that are not entirely zero, because a
   freshly made Btrfs is nearly all hole and copying the holes would be a
   write of the whole volume. That skip has a consequence: bytes the image
   leaves as holes are bytes the destination KEEPS, so a reinstall over
   another filesystem would keep that filesystem's superblock — mkfs erases
   those signatures by writing zeros, which in a sparse image are
   indistinguishable from the holes around them. So `PARTITION_ALIGN_BYTES` at
   EACH END of the volume are zeroed before the copy, the same bounded-prefix
   argument the ESP's metadata region already makes, doubled.

   Both ends, because one is not enough and a bounded prefix alone would be a
   claim this file made and the disk did not keep. The front holds XFS at 0,
   ext* at 1 KiB and Btrfs at 64 KiB; the TAIL holds MD RAID 0.90 metadata in
   the last 64 KiB, MD RAID 1.0 eight kilobytes from the end, and ZFS's L2/L3
   labels. btrfs-progs wipes both ends of its device, so on a fresh image both
   wipes are holes — and it is exactly those holes the copy discards. Installing
   over a former mdadm member with 1.0 metadata therefore left `blkid` reporting
   both `btrfs` and `linux_raid_member`, which is a disk mdadm may assemble and
   a `LABEL=` that resolves ambiguously. Btrfs's own superblock MIRRORS need no
   such care: they sit at fixed offsets and the new mkfs writes each one the
   volume is large enough to hold, so a stale mirror is always overwritten by
   its replacement.

   The copy is also ORDERED, for the same reason the primary table is written
   last: a filesystem has a commit point too, and Btrfs's is the superblock
   64 KiB in — inside the FIRST chunk. So every other chunk is written and
   synced, and that one goes last. An interrupted `volume` then leaves nothing
   at the offset a mount reads: a partition that holds no filesystem, which is
   what it is. Written first, the same interruption leaves a superblock every
   prober calls valid over chunks that are still the previous install's, which
   mounts as `open_ctree failed` and reads as corruption rather than as an
   install that did not finish. This is also what makes zeroing the FRONT
   load-bearing rather than merely tidy — without the deferral the copy
   rewrites that whole chunk immediately, and with it those zeros are what
   stands in the superblock's place meanwhile. The zeroing is synced before
   the copy begins, or the barrier orders nothing: a loss could persist new
   blocks while the zero over the old superblock was still page cache.

   The deferral covers the PRIMARY superblock and deliberately not the
   mirrors. A mirror 64 MiB in is written in the first pass, and holding it
   back would not buy the same property — the bounded zeroing does not reach
   that far, so what stands there in the meantime is the previous install's
   mirror rather than nothing. The residual is therefore that `btrfs rescue
   super-recover`, asked to try, can promote a mirror of an interrupted
   install; every path that MOUNTS reads the primary, which is absent. Making
   that residual go away means zeroing the whole region, which is the write of
   the whole volume the sparse copy exists to avoid.

   The cost is a read of the whole image per install — memory bandwidth over a
   sparse file, no I/O — which is the price of not adding a syscall. If that
   ever matters, `SEEK_DATA` is the fix and is an amendment then.

   **Both verbs report one line of whitespace-separated BYTE OFFSETS on stdout
   and nothing else** — `layout` the ESP and the volume, `volume` the volume's
   offset, length and the bytes actually written. Bytes rather than the LBAs
   the layout works in, and the same unit for both, because two verbs of one
   program reporting the same-shaped line in different units is a caller
   reading 2048 where the ESP is at 1048576, with nothing on either line to say
   which it got. The destination is NOT echoed back: a caller knows what it
   passed, and a path is the one value here that can carry a space — which
   shifts every field read by position — or a newline, which would break the
   one-line promise outright. Everything a person reads goes to stderr,
   including the output of the one child process this path runs.

   **The problem it answers: writing a partition table does not make Linux
   reread one.** On a block device the kernel keeps serving the partition
   layout it already has, so `/dev/sda1` may not exist after a layout on a
   blank disk, and may still have the OLD bounds after a reinstall. Nothing in
   7a needs it — the layout writes offsets on the whole-disk descriptor it
   opened — but formatting the volume and publishing into it both want the
   kernel to agree about where that partition is. Neither is visible in the
   regular-file tests and neither will be: a file has no partition devices at
   all, which is the one place D9's single code path genuinely stops, and the
   decision above is what stops it stopping.

   The two rejected candidates, so a later reader does not re-derive them. The
   conventional mechanism is the `BLKRRPART` ioctl (`_IO(0x12, 95)`,
   `block/ioctl.c`), which needs `CAP_SYS_ADMIN`, must be issued on the whole
   disk rather than a partition, and returns EBUSY from
   `disk_scan_partitions` if ANY partition is currently open — so a reinstall
   onto a disk with something mounted cannot rescan at all. The owner
   AUTHORISED that amendment; it is declined rather than unavailable, because
   it would make `td-install` a ninth entry on `UNSAFE.md`'s roster to serve
   the one destination the tests cannot reach. A loop device over the
   partition's byte range is the other, and it needs `LOOP_SET_STATUS64` for
   the offset — another ioctl, so it trades the surface rather than avoiding
   it.

   One tempting way out does not exist, and is written down because it reads
   like it should: the kernel does NOT rescan when the last writable
   descriptor on a whole-disk device is closed. `GD_NEED_PART_SCAN` is set in
   four places — a disk appearing (`add_disk_final`), removable media
   actually changing (`disk_check_media_change`), `disk_scan_partitions`
   itself, and nbd — and `bdev_release` is not among them. It syncs and
   flushes media-change events and nothing else. The rescan on open fires
   only if that flag is already set.

   **7c, the PUBLISH: into the staging tree, before the filesystem exists.**
   D1 gives the whole transactional publish to `td-boot`, and D8 leaves
   `td-install` unable to mount anything — so the question 7c answers is not
   who writes a deployment but WHERE `td-boot` writes it, given a destination
   whose volume partition has no name a mount could take.

   The answer follows 7b's rather than fighting it. `td-boot install <device>
   <mountpoint> <source>` is two things in a trench coat: mount this device,
   then publish into the result. Only the second half is D1's one bundle
   writer, and it is already a function of a plain PATH —
   `install_deployment(root, source)` requires `root/td`, `root/<BOOT_DIR>`
   and `root/<DEPLOYMENTS_DIR>` to be real directories and after that does
   ordinary file operations and renames, with nothing in it that knows it is
   on Btrfs. So the inner half is exposed as its own verb, `install` is
   refactored to call it once it has mounted, and there remains exactly ONE
   implementation of the publish — strictly less duplication than today, not
   more.

   `td-install` then stages the deployment into the SAME `--rootdir` tree that
   already carries `@var`, and mkfs bakes the published result into the image
   the sparse copy is about to write. Nothing mounts, nothing loops, no
   partition device is needed, `UNSAFE.md` is untouched, and the oracle
   exercises the shipped code path rather than a cousin of it — which is D9,
   and the same argument that settled the `BLKRRPART` question above. It is
   also verifiable offline, which is what makes it a check rather than a
   hope: `btrfs inspect-internal dump-tree -t fs` lists the directory entries
   of an unmounted image, so the recipe can assert `td/deployments/<id>` and
   the selector are really in the filesystem on the destination. Measured on
   a real image before this was written down.

   **The install-time trust root is settled here, because it is item 7's to
   settle** — `warn_unverifiable_resign` says so in as many words. Today
   `install` runs with no trust root at all (§6: the real root has none), so
   it publishes what it is given and warns that nothing present can check it;
   the first thing that actually verifies the bundle is the next boot, and a
   bundle signed with the wrong key is discovered by a machine that does not
   come back. That is the wrong end of the operation to learn it at.

   An INSTALLER is not the real root, though, and that is the distinction the
   warning was waiting for. It runs from installation media that td built,
   which can carry a trust root exactly as the selector initramfs does, so
   the publishing verb takes an optional trusted key — the spelling
   `td-boot authenticate <deployment-directory> [trusted-key]` already uses —
   and REFUSES a bundle that does not verify under it. Fail-closed where a
   key is supplied, and today's warn-and-publish only where one is not, which
   keeps a rootfs with no trust root behaving as D3 requires while making the
   installer the first thing on the path to check rather than the last.

   Two consequences worth stating rather than discovering. The deployment
   source has to exist when `volume` runs, since the publish now happens
   before the filesystem does — so it is an argument to that verb rather than
   a later step, and an install with no bundle to publish is the same command
   without it. And a bundle published this way is subject to the copy's
   ordering above: it is inside the image, so it lands before the superblock
   does, and an interrupted install leaves no filesystem rather than a
   filesystem with a half-written deployment in it.
8. **The EFI-stub kernel** (§5): `CONFIG_EFI`, `CONFIG_EFI_STUB`,
   `CONFIG_CMDLINE` in `linux-x86-64.rs`, having first confirmed what
   `CONFIG_EFI` drags in on the pinned tree.
9. **The OVMF oracle** (§8), beside the `-kernel` one, not replacing it.
10. **`td-update` and its local channel**: fetch a signed bundle, verify it,
   delegate the publish (D1 again), and roll back on a failed boot. This is
   where the update channel that was its own workstream rejoins this one.

Items 8 and 9 depend on nothing above them and may land whenever they fit.

## 11. Validation

The single pass/fail command is unchanged:

```text
cargo run --release --manifest-path builder/Cargo.toml -- check
```

During development, `td-builder affected-checks --run`; before pushing,
`td-builder ready`, which is `affected-checks --committed-only --run` plus
the per-commit review record.

Beyond the existing deployment/persistence/rollback oracles, this path adds:
an install onto a regular file that then boots under OVMF; a bundle signed by
an unpinned key that must be refused; and an update that installs a second
deployment and rolls back to the first.
