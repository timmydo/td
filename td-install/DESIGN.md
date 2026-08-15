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

`engine/src/gpt.rs` + `engine/src/fat.rs` (with `engine/src/crc32.rs`)
landed toward it and are not yet consumed by anything.
`engine/src/ed25519.rs` + `engine/src/sha512.rs` (verify-only, `aa347e60`)
were in that list until §10 item 5: td-boot compiles them in now, though
nothing on the boot path calls them until item 6 flips the policy.

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

### Where the trusted public key lives — OPEN

This is not settled, and it is recorded here rather than decided quietly
because a first answer turned out to be unbuildable.

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
driver loop is `while (!message && len)`. And td's deployment initramfs is an
**initrd** (`CONFIG_INITRAMFS_SOURCE=""`, qemu `-initrd`), so it takes
`:726-733` rather than the `panic_show_mem` the built-in archive gets at
`:714-716`; with `CONFIG_BLK_DEV_RAM` off — allnoconfig leaves it off and the
recipe's delta list does not add it — that arm is one
`printk(KERN_EMERG "Initramfs unpacking failed: %s\n")` and **the boot
continues**, base archive extracted and the key absent. That is precisely the
"a missing key becomes a runtime branch" hazard named three paragraphs above
— whose tempting branch is the fail-open D2 forbids — arriving through the
very mechanism meant to avoid it. So the harness's own padding is the entire
defence, and item 6's fail-closed flip is what turns a keyless boot from a
silent one into a refusal.

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
   REACH a machine, and it needs no answer to the open question below.
5. **The verifier reaches the target** (§6): td-boot `#[path]`-includes
   `ed25519.rs` and `sha512.rs`, its recipe stages them, and it gains a hex
   decoder, a trusted-key reader, and `authenticate_manifest` — reached by
   one new verb, `td-boot authenticate <deployment-directory>
   <trusted-key>`, which answers authenticity and nothing else. The BOOT
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
   deployment whose manifest does not verify, and the system oracle signs the
   bundle it stages and gains a wrong-key negative control. This is the
   increment that decides the OPEN question in §6 — where the trusted key
   lives — because it is the first one whose ANSWER has to reach a running
   machine. It was scoped with item 3 as a single landing and split three
   times: as that question turned out to be unsettled, as carrying the file
   turned out to be its own increment, and as getting the verifier into the
   target binary turned out to be separable from deciding what it trusts.
   Each earlier half needs no answer to the question; this one cannot
   proceed without one.
7. **`td-install`**, a standalone crate outside the workspace (D9): GPT +
   FAT32 ESP + Btrfs volume onto a device or a regular file,
   `#[path]`-including `gpt.rs`/`fat.rs`/`crc32.rs` and `protocol.rs`, and
   delegating the publish to `td-boot install` (D1). Carries the
   `mkfs.btrfs` build-time binding (D7). Registering a new crate is three
   touch points that must land with it: the cargo-test gate's one-package
   `Cargo.lock` assertion plus its clippy and test lines
   (`builder/src/gate_defs/325-cargo-test.rs`), a route and assertions in
   `builder/src/affected.rs`, and the workspace `exclude` list.
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
