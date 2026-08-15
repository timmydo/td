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

- No partitioner and no installer. A machine is only ever reached by QEMU
  `-kernel`, which is a development harness, not a way to boot hardware.
- No bootloader, and nothing that writes an ESP.
- No **authenticity**. Manifest hashes prove integrity and transaction
  completeness. They prove nothing about who produced the bundle: anyone who
  can write the volume can write a manifest that matches their own payloads.
- No update channel.

Two pieces landed toward it and are not yet consumed by anything:
`engine/src/gpt.rs` + `engine/src/fat.rs` (with `engine/src/crc32.rs`), and
`engine/src/ed25519.rs` + `engine/src/sha512.rs` (verify-only, `aa347e60`).

## 3. Hard invariants

**D1. There is exactly ONE bundle writer.** `td-boot install <device>
<mountpoint> <source>` is it. `td-install` formats a disk and then
*delegates* the publish; it does not learn to write a deployment directory,
update a selector, or account for attempts. The transactional publish is the
part of this path where a partial write is a machine that does not boot, and
two implementations of it would be two chances to get the rename order wrong
— with the second one exercised only by installs, which are the rarest
operation on the path and the one nobody watches.

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
a key other than the pinned one, or a manifest that does not verify, each
refuse the deployment. There is no permissive mode and no flag to add one.

**D3. The signature is DETACHED and the id is unchanged by it.** The
deployment id stays `sha256(manifest)`. The signature lives beside the
manifest as `manifest.sig`, and is not an input to the id. This is what lets
a bundle be re-signed under a new key — key rotation, or a bundle promoted
from a test key to a release key — **without changing its identity**, so a
machine that already has that deployment installed still recognises it as the
same one. Folding the signature into the id would make every re-signing a
different deployment, and rollback would stop finding what it rolled back to.

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

```text
LBA 0            protective MBR
LBA 1            primary GPT header
LBA 2..33        primary partition entry array (128 x 128 bytes)
LBA 2048..       partition 1: EFI System Partition, FAT32
                 partition 2: td, Btrfs
last-33..last-1  backup entry array
last             backup GPT header
```

Partitions start at 2048 sectors (1 MiB) and are megabyte-aligned, which is
what every current partitioner does and what keeps writes off the wrong side
of an erase block on flash media.

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

Generation reuses the existing tool — `td-subst keygen PRIV PUB` produces a
pkcs8 private half and a raw 32-byte public half. The private half is
**never committed**. `tests/td-deploy.pub` is the committed default public
key, and it is exactly that: a default for development and for the crate's
own tests.

**td-boot's pinned public key is a declared build input**, not a constant in
its source. The recipe takes it and pins it into the binary at build time.
That is what makes the trust root a *build* parameter: rotating it is a
rebuild rather than a source edit, a release build can pin a key that never
appears in the repository, and — the property that pays for the extra recipe
parameter — the boot oracle can pin a key it generated seconds earlier.

Verification is `engine/src/ed25519.rs`, which is verify-only and strict: it
refuses `S >= L`, non-canonical encodings, keys outside the prime-order
subgroup, and small-order `R`. It reaches SHA-512 as `crate::sha512`, so a
consumer must declare **both** modules at its crate root; they are a pair.

Signing is host-side in `td-net`, where `ring` already signs, and emits
`manifest.sig` beside the manifest (D3, D4).

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

Each entry stores the full consumer list the router prints in its note; the
column above shows only the target half that distinguishes these six.

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
3. **Signing, host-side and target-side, in one landing** (§6, D2): `td-net`
   emits `manifest.sig`; `td-boot` verifies fail-closed against a build-input
   public key; the system oracle signs the bundle it stages and gains the
   wrong-key negative control. Nothing installs yet, and nothing needs to for
   this to be tested.
4. **`td-install`**, a standalone crate outside the workspace (D9): GPT +
   FAT32 ESP + Btrfs volume onto a device or a regular file,
   `#[path]`-including `gpt.rs`/`fat.rs`/`crc32.rs` and `protocol.rs`, and
   delegating the publish to `td-boot install` (D1). Carries the
   `mkfs.btrfs` build-time binding (D7). Registering a new crate is three
   touch points that must land with it: the cargo-test gate's one-package
   `Cargo.lock` assertion plus its clippy and test lines
   (`builder/src/gate_defs/325-cargo-test.rs`), a route and assertions in
   `builder/src/affected.rs`, and the workspace `exclude` list.
5. **The EFI-stub kernel** (§5): `CONFIG_EFI`, `CONFIG_EFI_STUB`,
   `CONFIG_CMDLINE` in `linux-x86-64.rs`, having first confirmed what
   `CONFIG_EFI` drags in on the pinned tree.
6. **The OVMF oracle** (§8), beside the `-kernel` one, not replacing it.
7. **`td-update` and its local channel**: fetch a signed bundle, verify it,
   delegate the publish (D1 again), and roll back on a failed boot. This is
   where the update channel that was its own workstream rejoins this one.

Items 5 and 6 depend on nothing above them and may land whenever they fit.

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
