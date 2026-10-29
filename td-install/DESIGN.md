# td-install — the deployment path

[INSTALLER.md](INSTALLER.md) specifies the offline graphical installer and
its hybrid installation media. Its v1 deliberately defers the
encrypted profile in [ENCRYPTION.md](ENCRYPTION.md). The contracts below
continue to own disk layout, deployment publication and payload checks.

This file is the normative specification for td's **deployment path**: how a
disk is laid out, how a deployment bundle is published onto it and verified,
and how a machine moves from one deployment to the next. Where the code and
this document disagree, one of them is a bug.

It is deliberately wider than one crate. The path runs through `td-install`
(the installer), the on-disk protocol and transactional publish in
`td-boot`, and the update path in `td-update`. Those were three workstreams
and are now one, with one owner and one branch, because every coordination
point between them was a place where two agents could each write half of a
mechanism. Several invariants below are not checkable by any compiler or
lint; they are recorded here because the failure modes they prevent are
silent.

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
Data a deployment must declare to td's own readers therefore travels in
a covered payload, never a new manifest line: the login-key tier marker
is a member of `initramfs.cpio` (`td-login/TOKEN-LOGIN.md`,
"Deployments").

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

What was **missing** when this workstream began; each has since landed, the
installer image in INSTALLER.md and the rest in the sections below:

- No installer on any IMAGE. `td-install` writes the layout and a recipe now
  links it statically for the target, but nothing packs it anywhere a person
  could run it, so a machine is still only ever reached by QEMU `-kernel` —
  a development harness, not a way to boot hardware.
- No bootloader, and nothing that writes an ESP.
- No update channel.

`engine/src/gpt.rs` + `engine/src/fat.rs` (with `engine/src/crc32.rs`)
landed toward it and were consumed by nothing until §10 item 7a, which is
the installer that writes what they produce — and by a TARGET binary since
7b's recipe, which is what makes them shipped code rather than host code
that happens to compile.

Manifest hashes prove integrity and transaction completeness, not who
produced a bundle; §6 is why td adds nothing to them.

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
is about: `td-boot install <device> <mountpoint> <source>` mounts and
then publishes, and the verb 7c adds publishes into a volume root that is
already writable. The second exists because staging needs no mount at all and a
regular-file destination has no partition device to mount anyway (D9); the
first is what a running machine and the update path use, and what `format
--publish` runs on a loop over the disk it holds (§5 "Publishing through a
loop over the claim"). Neither reimplements the other — `install` calls the
same function once its mount has succeeded.

Consequently `td-install` shares `td-boot/src/protocol.rs`'s
`DEPLOYMENTS_DIR`, `SELECTOR_PREFIX` and `BOOT_DIR` through the same `#[path]`
include, rather than restating the layout. A layout stated twice is a layout
that can disagree with itself, and the disagreement surfaces at the first
boot after an install rather than at build time.

**D2. Verify fail-closed.** Every reader of a deployment — the selector
choosing a slot, the publisher, the installer's source checks and the
update helper — hashes each payload it uses against the manifest and
refuses on any mismatch, missing payload or malformed manifest. There is
no permissive mode and no flag to add one.

**D3. The id is the manifest's hash.** The deployment id is
`sha256(manifest)`, recomputed at every verify, so it is a claim about
content rather than a name someone chose. Nothing beside the manifest is
an input to it, and a deployment directory holds the manifest and its
three payloads and nothing else td reads.

**D4. No secret enters the deployment path.** A deployment is built
reproducibly from the recipe graph and published as built; no key is
generated, retained or consulted to build, install, update or boot it
(§6).

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
reintroduce one. D7's `mkfs.btrfs`, run at INSTALL time and on a LIVE boot,
whose deployment initramfs formats its volatile volume with it (MEDIA.md
"Live boot"), is td's own Rust from the `td-btrfs` recipe, not a third-party
program. LUKS2 formatting and activation use the static Rust `td-luks`
program (ENCRYPTION.md and td-luks/DESIGN.md). The verified root and both
selector and deployment initramfs bind `/bin/td-luks` to its exact store
payload; the root also carries its debug companion. Image construction
checks these bindings. Keys travel only through descriptors. The
selector opens the volume and exports its handoff key through td-luks;
the deployment initramfs verifies and activates that volume key.
The live selector, the same stock
initramfs, carries it and never runs it. The installed selector's
recovery flow reads the recovery key through td-init's `secret-line`
(td-boot's `SECRET_LINE_APPLET`), as its protected release reads every
PIN, answer and key there, a td-init applet like the rest; its FIDO2
worker, td-boot's own `fido-worker` verb started as `/proc/self/exe`,
needs `/proc` mounted and takes its operation lock under the root-owned
mode-0755 `/run` the selector initramfs carries, and the
image build refuses a selector initramfs whose `/bin/secret-line` does
not link td-init, and a deployment initramfs or root that carries it.
`losetup` and `mknod` are in both initramfs: the deployment binds its
verified root with them, and the installed selector's staged-update
commit (ENCRYPTION.md "Selector update", from 8e) binds `current`'s
`root.erofs` read-only to read its template, making `/dev/loop0` first
when devtmpfs has none; so the image build refuses either initramfs
whose `/bin/losetup` or `/bin/mknod` does not link td-init. The selector
initramfs alone links td-init's `reboot` (td-boot's `REBOOT_APPLET`),
which the installed selector runs once `update` completed a staged
update without a predicted protector; the image build refuses a selector
initramfs whose `/bin/reboot` does not link td-init and a deployment
initramfs that carries it.

**D7. `mkfs.btrfs` is bound at build time.** `td-install` execs the
shipped `mkfs.btrfs`, td's std-only Rust formatter from the `td-btrfs`
recipe ([td-btrfs/DESIGN.md](../td-btrfs/DESIGN.md)), to create the volume.
The cost is real and is the `losetup` lesson: an install would depend on
another program existing at a path, with nothing tying the two together,
so dropping it from the image would break installs with **no build-time
complaint**. So the landing that execs it also carries the binding
`td-boot/src/protocol.rs` already models for applets — a named constant that
the recipe-side image check consumes, so an image that does not provide
`mkfs.btrfs` **reds the build** instead of failing an install on a machine
someone is standing in front of. The binding lands *with* the exec, not
after it. The live boot's exec is bound by the image's shape check instead:
it refuses a deployment initramfs without `/bin/mkfs.btrfs` and its store
payload, and a selector initramfs with either. It is the static binary built
under the shipped target profile, so its debug companion ships in the image
with it.

**D8. One recorded `unsafe` surface.** Everything here is ordinary file
I/O: partition tables and filesystems are bytes at offsets, and efivarfs is a
filesystem. The sibling crates the binary links for device-bound storage,
td-protector, td-tpm with its td-fido, and td-json, forbid `unsafe` and
reach the TPM through td-tpm's safe file I/O on `/dev/tpmrm0` (D10
records that open and td-protector's `/dev/random` read). The pipes that
carry key material to td-luks are std's, created by td-protector's
runner, and the device-mapper requests td-luks's own.
The one exception is publication onto a disk this process holds, which
binds a loop device over the volume with two value-pinned requests
(`UNSAFE.md` §21, §5 "Publishing through a loop over the claim"). Any
further syscall is an amendment to `UNSAFE.md` and is reviewed as one —
not a thing to discover in a diff.

**D9. The installer writes to a device OR to a regular file, and the file
case is what the oracle exercises.** One code path, two destinations. This is
what makes the installer testable headlessly at all: the oracle hands it a
file, then boots that file under OVMF. An installer whose tested path and
shipped path differ is an installer tested somewhere other than where it
runs.

The optional volume timezone is validated against the deployment's offline
catalog before destination access. It seeds only `@var/lib/td/timezone`,
mode 0644, under mode-0755 parents. The subvolume root is always mode 0755,
including when no zone is selected. The deployment-owned optional persistent
link at `/etc/timezone` points to that file; no selection leaves it
absent. INSTALLER.md owns the catalog grammar and consumer boundaries.

An optional hostname is parsed into the shared canonical name type before
any destination access and seeds `@var/lib/td/hostname` with the same
readable file and directory modes. The persistent `/etc/hostname` link,
boot-time default initialization, kernel activation and QEMU evidence are
specified in INSTALLER.md. No user-selected value enters generated shell.

An optional `--username NAME VERIFIED-ROOT TD-FIRSTBOOT` binds a selected name
to the caller-authenticated stable deployment and an absolute source-built
validator. Complete account admission must succeed before volume destination
access or scratch removal. The formatter seeds `@var/lib/td/username` with the
same modes and a final newline. INSTALLER.md owns early account publication,
read-only projection, home preparation and recovery; the numeric UID/GID and
authentication fields remain unchanged. Layout callers must perform that
preflight before their first destructive command. This interface is a
caller-bound primitive, not a user-selectable validator or disk authorization.

`layout-preview` uses the same layout calculation without a destination
operand or any filesystem access. Its geometry-only contract and output
schema live in INSTALLER.md; it does not admit a disk or authorize writes.

**D10. Every filesystem call goes through a choke point that names its
path.** `io::Error` carries an errno and no filename, and one command line
here names as many as five paths — so `No such file or directory` alone is
a diagnostic an operator cannot act on. This crate's SHIPPED source reaches
the filesystem only through `mod paths`, whose wrappers each pair the
operation with the path they were given; td-fs's `src/real_file.rs`, which
the crate compiles in, is the second such point and already named its own, and
`engine/src/sha256.rs`'s `sha256_file` is the third: the crate compiles
that file for its digests, never calls the helper (a test pins that), and
the helper names its path for every consumer. ONE
wrapper is exempt from the naming and is named in the test that enforces
it: `metadata_if_present` discards its error, because its caller asks only
whether two files are the same and an unreadable path is not one of them.

Two opens lie outside the three, the lint below and `compiled_files()`,
because they are not this crate's source but siblings the binary links as
rlibs. The service's TPM probe, which every `serve` runs once admitted
(INSTALLER.md "Storage choice"), and a device-bound execution's seal and
check (D8) call td-tpm's `Device::open`, which opens the fixed path
`/dev/tpmrm0` read-write with `O_NOFOLLOW`, refuses anything but a
character device, and names the TPM resource manager in its error. The
execution draws the recovery key and the protector secret through
td-protector, which reads the fixed path `/dev/random` and names it in
its error. Those two device opens are the documented exceptions;
td-protector's td-luks runner also execs the absolute td-luks path
td-install gives it, naming that program in its errors, which is an exec
of a path this crate chose rather than an open. td-tpm's salted
sessions and its lockout authorization read `/dev/random` and td-fido's
hidraw code opens its
devices, but the device-bound path calls neither. Nothing else reaches
another path through td-tpm, td-fido, td-protector or td-json, and a
further path a linked sibling opens for this binary is an amendment here,
as a further syscall is to D8.

The COMPILER holds the first half of that. `clippy.toml` disallows every
path-taking entry point into the filesystem and `Cargo.toml` denies the
lint, so a call outside the three choke points fails CLIPPY — which is a
gate leg and not `cargo build`, and not the recipe that compiles the
shipped binary with rustc directly. The four scoped `#[allow]`s are the
whole of the exception, and a test reads where they sit. What is left for
a test is whether a wrapper puts its path in the message, which clippy has
no opinion about. A source SCAN held the first half before this and lost
nine rounds of review to spellings it did not model — `clippy.toml` is the
roster now, and a new verb is a new wrapper rather than a call at the site
that wants one.

Two edges of that are worth stating because neither is visible from the
code they constrain. The deny reaches every file this crate COMPILES,
`engine/src/gpt.rs` and the other five included ones among them, so an
ordinary `std::fs::read` added there fails td-install's clippy while the
engine's own stays green — and the advice in the message, to use `mod
paths`, cannot be followed from a crate that has no such module. And an
entry that does not RESOLVE is inert: clippy warns `does not refer to a
reachable function` and exits 0, so a typo removes a roster line without
removing a test's count of it.

One operation is beyond the roster and is a constraint on a WRAPPER
instead. Reading a directory fails twice: once opening it, which
`std::fs::read_dir` reports, and once per entry, since each `ReadDir`
`next()` is a fallible `readdir` whose `io::Error` names nothing. The
roster cannot hold the second — `std::fs::ReadDir::next` does not resolve
as a path, so an entry for it would warn once and refuse nothing. A
`paths::read_dir` must therefore CONSUME the iterator and name the
directory on each entry's error rather than hand a bare `ReadDir` back.
`paths::read_dir_bounded` consumes that iterator for block inventory, names
opening and per-entry errors, and refuses an excess entry instead of
truncating the observation. `paths::read_bounded` similarly names read
errors and rejects attributes exceeding their byte limit. The
keyboard-console probe (ENCRYPTION.md "Activation") reads sysfs through
the same two wrappers: its listings of `/sys/class/vtconsole` and
`/sys/class/input` and each attribute, one page at most, opened
close-on-exec by std and closed when read.

### Carried Wi-Fi networks

A live execution (`serve`) also carries the Wi-Fi networks the live
session joined, so the person does not type a passphrase again on the
installed system. After the settings are admitted and before staging, it
reads td-netd's `var/lib/td-netd/wifi` under the root it serves: a
missing directory, a missing file or an empty one carries nothing. Only
a real regular file (td-fs's `open_real_file`, no symlink followed) of
mode 0600 or stricter and at most 4096 bytes, in a directory that is
not a symlink and that no one but its owner writes, both owned by root,
whose bytes are only td-netd's alphabet (lowercase hex, `open`, spaces
and newlines), is carried. `var` and `var/lib` are resolved as the root
has them, root's on the live system. The alphabet is a check on bytes,
not a parse: td-netd parses the copy on the installed system as it
parses its own file, dropping any line that does not decode. Anything
else is left behind with a `td-install serve:` line on standard error
and the installation continues, since Wi-Fi is a convenience the
installed system can configure again; once read, a failure to stage the
copy fails the installation as staging the settings does. The bytes are
read into one allocation, held zeroed-on-drop and staged as
`@var/lib/td-netd/wifi`, mode 0600, in `@var/lib/td-netd` at 0700, both
root's, modes set because `--rootdir` copies them. A network joined
after the execution read the file is not carried. The wizard never sees
the networks and nothing about them crosses the installation protocol;
the review's details say that networks joined in the session are
copied, with their keys, and that on unencrypted storage the disk holds
those keys readably. The `volume` and `format` commands carry none.
Reinstall keeps the installed `@var`, and so its own saved networks, and
carries none from the live session.

### VM installations

`td-recipe-eval bundle --installation --out DIR` creates a private VM
installation. It builds the system, provisions the selector with the
volume identity and publishes the initial deployment onto a fresh
volume. The existing builder `userns-private` operation maps the seed's
owner to numeric root while `mkfs.btrfs` reads it; no host privilege or
first-boot ownership repair is required. Each installation receives a
random version-4 UUID from the kernel random device, as `td-install
new-volume-uuid` draws one, and 2 TiB of sparse virtual capacity for
local builds.

The installation output directory is pinned by descriptor, caller-owned 0700
and empty apart from its private publication lease; the disk is 0600.
The output's parent must already exist. One nonblocking lease covers path
settlement, warming, building and publication. Publication rechecks emptiness,
refuses existing destination files or a moved directory, and syncs its
completed files and directory entries. This mode requires explicit
`--out`, refuses `--force`, uses a distinct marker that demo replacement does
not recognize, and generates a launcher that persists guest writes by
default. It is not a redistributable release template. The default
`bundle` remains a disposable demo.

VM provisioning does not install an EFI boot stub, provide disk
encryption, or authorize successor activation. The named, consent-bound
update operation below authorizes successor activation.

### Offline reinstall

`INSTALLER.md` "Reinstall while keeping personal files" defines the live
preserve mode. It is separate from formatting and preserves the existing
GPT, filesystem UUID and writable state. Its first increment refuses
encryption. It replaces both boot selections and the ESP's kernel and
selector, and requires explicit physical consent to reinstall. An
interrupted reset may require another USB boot; keeping personal files is
not a claim of atomic activation across Btrfs and FAT.

`td-boot reinstall DEVICE MOUNTPOINT SOURCE` is its root mechanism.
It verifies the complete source before a writable mount and takes the
normal transaction locks. It uses the one bundle writer,
replacing a same-ID destination through the existing installation temporary
namespace, then selects the verified replacement in both slots and clears its
attempt budget. Ordinary install retains its existing predecessor checks.
Only the live installer binds this mechanism to physical consent and
refreshes the selector; the command alone is not a complete reinstall
interface.

### Privileged local installation mechanism

`td-update apply-operation DEPLOYMENT-ID` is an internal root-only
mechanism for the stock VM's installation authority. Its stdin is a held
source directory descriptor owned by the configured requester (UID 1000),
not a pathname. It is not an elevation command or a user consent flow.
The authority must admit and present the exact manifest ID before invoking
it. `./update` (or `./update install`) builds the current checkout and queues
that operation through td-authd as `deploy-publish`, which the principal
table must grant. Physical I selects it, and Enter, pressed first after
complete presentation, confirms it, as specified in
td-compositor/DESIGN.md, "Elevation consent".

The helper reads a bounded manifest through the supplied directory,
compares its SHA-256 with the approved ID, and pins all three regular
payload files, which must also belong to the requester. Symlinks and
special files refuse. The staging bounds are 256 MiB for the kernel,
512 MiB for the initramfs, and 128 GiB for the root image. A size change
during copying refuses; content changes remain subject to the payload
hashes.

A fresh root-private directory beneath td-update's own state,
`/var/lib/td-update`, receives copies of those four files, each synced
at mode 0600. Every ancestor of that state must be a real root-owned
directory with no group or other write; the state itself is created
0700 if absent and its mode then set exactly, and a symlink or any other
owner or mode refuses. `td-boot validate-source STAGE` then checks every
staged payload against the staged manifest, and its stdout must be
exactly the approved ID and a newline. Only then does `td-boot install`
verify all payloads again and enter its existing transaction, publishing current/previous on the
running volume UUID through the private `/run/td-update` mountpoint. No
requester-selected command, device, mountpoint or file name crosses this
interface. This device selection is the current stock VM profile, not a
hardware installer discovery policy.

Ordinary completion or refusal removes the helper's private staging.
Process death can retain a root-private `.update-*` orphan but cannot
publish an unverified deployment; the boot transaction owns crash recovery
once installation begins. There is no implicit reboot in this mechanism.


## 4. Disk layout

Published VM volumes additionally carry `td/source/repository.bundle` and
`td/source/revision`, with `td/source/upstream` naming
`https://github.com/timmydo/td.git` and branch `main` by default. Explicit
`--source-origin` and `--source-branch` publishing options override these;
older source companions may omit the upstream file. These
are source companion data, outside the deployment
and target artifact graph. The VM producer exports only the selected Git
HEAD's reachable history, never host Git configuration, hooks, untracked files
or unrelated refs. It requires a clean, complete checkout before preparation
and checks the same clean commit again during export and before publication.
The operator must keep the checkout stable throughout the build; these checks
do not lock out a concurrent editor or establish a snapshot of build reads.

The source files have mode 0644 and their volume directories have mode 0755,
independent of the publisher's umask. They count against the volume payload
budget. The published disk checksum covers these bytes; the deployment
manifest does not. Boot never executes them. The unprivileged `release-source`
oneshot clones the bundle into the human's persistent `~/src/td` if absent,
where ordinary Git owns subsequent edits and pulls. It preserves an existing
checkout and never makes console startup depend on cloning. The initializer
and native build command are specified in `td-update/DESIGN.md`.
Update activation preserves this initial source companion and the user's
checkout. Nothing may treat this mutable volume metadata as boot input.

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

Partition 2 ends at the last usable LBA, except on a device-bound plan:
there it ends on the last usable LBA that leaves its length whole 4 KiB
encryption sectors, at most 3.5 KiB short of the last usable LBA on a
512-byte disk and nowhere short on a 4Kn one. The LUKS2 data segment is
the rest of the partition, and dm-crypt maps a 4 KiB-sector segment only
in whole sectors, so stock cryptsetup opening the partition from any
medium must find it whole. Its 1 MiB-aligned start is already a whole
encryption sector. The unencrypted layout is unchanged by this rule.

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

td-recipe-eval's `server-image` writes this layout into an image file
on the build host, at a 512-byte sector, with the same partition
names, ESP size, alignment and EFI paths from `td-boot/src/protocol.rs`
(td-svc/DESIGN.md §10, "Profiles"). It does not run `td-install`: the
host composes the volume as its other persistent volumes are composed,
owned by guest root, and the image is never an installation target.

## 5. Firmware entry

### Installed startup menu

After unlocking and mounting the volume read-only, the installed selector
shows the retained `current` and distinct `previous` deployments. Each
entry requires a well-formed manifest and an initramfs matching it. Full
kernel and root
verification remains mandatory on the selected boot path; showing a label
is not a claim that those larger payloads have already been read.
The writer locks remain held across the menu and activation.

The default countdown is 15 seconds. A console input byte stops it;
Up/Down or 1/2 chooses and Enter boots. With no input, normal attempt
consumption and automatic rollback remain authoritative. Exhausted current
attempts make a successful previous the displayed default. A pending
previous is offered only with a successful current fallback. An explicit
retry resets the chosen pending deployment's three-attempt budget and
consumes its first attempt before boot.

An explicit previous choice becomes current, so older systems' success
acknowledgements still refer to the running deployment. A successful choice
is published as current before the displaced verified current is
retained as previous. A pending choice instead preserves the successful
current as previous before activation. These publication orders preserve the known-good
fallback when one exists. Manual transaction errors refuse rather than silently
boot a different entry. Verification is repeated after the writable
transaction before handing descriptor-bound payloads to kexec.

`td-init boot-menu` owns the fixed keyboard and serial consoles through
its existing termios boundary (UNSAFE.md section 3). The selector alone
links `/bin/boot-menu`; neither the deployment initramfs nor root farm
does. If neither console is usable before interaction, it reports automatic
selection. Losing all input after stopping the countdown fails the menu.

Labels show UTC commit date, 12-character Git hash and UTC build date,
with `+dirty` for an uncommitted checkout. An older deployment without
metadata explicitly shows unknown dates/hash and a short deployment ID.
`engine/src/boot_info.rs` defines a canonical 512-byte final newc appendix
containing public `td-build-info`. The reader accepts only the exact
canonical archive, from the already hash-verified initramfs descriptor.
No inode timestamp or current checkout supplies an old system's label.

ISO and installed-update assembly add that data to a private copy of the
recipe initramfs and regenerate the four-line manifest. The recipe outputs
and source-bootstrap graph stay unchanged. Build date means deployment
assembly date; identical source assembled on different dates can have
distinct deployment IDs. Git identity is captured before the build and
checked again afterward; a changed HEAD or clean/dirty state refuses
assembly. A dirty label does not identify uncommitted bytes by Git hash;
payload hashes still bind them.

The menu lives in the firmware-loaded selector. Ordinary deployment updates
retain the installed selector. Existing machines receive this menu through
the preserving reinstall path, which refreshes the selector, or the
separate selector-update protocol for protected machines.


### Read-only volume discovery primitive

`td-boot volume [UUID]` prints one canonical lowercase filesystem UUID and
one `/dev` path, or two for an encrypted volume (below), separated by
spaces and terminated by a newline. Without an argument it requires
exactly one visible td volume, a `td-system` Btrfs filesystem or a
`td-system` LUKS2 header (below). With a UUID it requires
exactly one matching volume. The two kinds are counted together: multiple
eligible volumes, including two devices carrying a cloned UUID and a
LUKS2 partition beside a Btrfs one with its UUID, refuse rather than
selecting by enumeration order. For an encrypted volume it reopens and
pins the partition as `on-volume` does and adds a third field, the
`/dev/dm-N` node of its active `td-system` mapping, admitted as the
running system's operations admit it; with no such mapping it refuses
as they do ("Full-system volume consumers"). UUIDs and labels identify
bytes; they do not vouch for a deployment or authorize a write.

The resolver refuses more than 4096 `/sys/class/block` entries and probes only
direct virtio, SCSI/SATA and NVMe disk/partition names (`vd*`, `sd*`, and
`nvme*n*`, with their numeric partition suffixes), and `ram<N>`, where a live
boot keeps its volatile volume (MEDIA.md "Live boot"). An installed system's
unused RAM disk carries no td volume and is ignored like any other disk.
Loop, device-mapper, mdraid and optical devices are not candidates; a
device-mapper node is admitted only as the active mapping of a selected
LUKS2 partition ("Full-system volume consumers").
Install media are found by a separate descriptor probe, not this one. Each
candidate's bounded sysfs device number must match its real block node
before and after a read-only `O_NOFOLLOW|O_NONBLOCK` open, including inode
identity across the open. The same device and inode checks follow the read.
Unreadable candidates or malformed sysfs values refuse; small devices cannot
contain a primary superblock and are skipped.

One 4096-byte primary superblock at byte 65536 is read through the held
descriptor. The layout is the kernel's `btrfs_super_block`, as
[`td-btrfs/src/format.rs`](../td-btrfs/src/format.rs) writes it.
Non-Btrfs and differently labelled volumes are ignored. Matching labels
require CRC32C, the primary offset, a nonzero UUID, one device, and ordinary
superblock flags. Unsupported checksums, seed/metadump/changing/error flags,
and corrupt matching superblocks refuse. This is a narrow identity probe,
not a complete filesystem validator; the kernel still validates mounts.

The same held descriptor is probed for a td LUKS2 header
([ENCRYPTION.md](ENCRYPTION.md) "Device-bound formatting"). Each 4096-byte
binary header copy at offset zero and at td-luks's secondary offsets
(16 KiB to 4 MiB, those the device holds) whose position's magic and
version 2 carry the `td-system` label is a claim, read through
td-protector's `luks2::binary_claim`, which verifies no checksum.
Discovery checks identity only. A claim naming the volume sought, or any
claim when no UUID is given, hands the device to td-protector's
`luks2::identity` (td-protector/DESIGN.md "LUKS2 tokens"): the copy
td-luks would use, chosen by magic, version, offsets, checksum and
sequence number, with no JSON parsed. Its label must be `td-system`
(another label is not a td volume) and its UUID canonical lowercase text
and, when one is sought, that volume's (otherwise the device is passed
over). A device whose claims name only other volumes is passed over
without that check, malformed or not, so another disk's header, an old
drive's or a newer td format's, cannot stop this volume's boot; a
wanted claim that `identity` refuses, or a non-canonical UUID, refuses.
Discovery reads no tokens: the installed selector reads them with
td-protector's full reader, on the selected volume alone, through the
held descriptor (ENCRYPTION.md "Selector release"). A
header read that fails, a bad sector or a
short read, before any wanted claim leaves the device no LUKS2
candidate, so the Btrfs probe alone decides it as before. LUKS1, another
version or another label is ignored like a foreign Btrfs. A device
carrying both a td LUKS2 header and a td Btrfs superblock is ambiguous
and refuses. Each device passed over by these rules is reported on
standard error once per resolution, one whole line per note, at most 64.

No match is polled against a thirty-second wall-clock deadline checked
between scans. One scan can overrun it: block I/O may wait despite
`O_NONBLOCK`. A missing candidate node
or sysfs value invalidates the entire partial scan and retries within that
deadline, allowing cold-boot devtmpfs publication to finish. An ambiguity or
other error is not retried. A successful scan describes currently visible
devices, not devices that might appear after it, and closes its descriptors before
returning. The caller must keep the topology stable and revalidate identity
at a later use. Root-owned devtmpfs/sysfs and their ancestors are trusted;
this is neither exclusive device admission nor protection from hostile
hotplug or a privileged namespace writer.

### Block destination formatting guard

All raw formatting commands open their destination read-write with Linux
`O_EXCL` (0x80) through safe `OpenOptionsExt`. The formatting wrapper
refuses platforms other than x86-64 Linux before opening; another target
architecture needs its flag mapping reviewed. On a block device, the held
File owns a kernel claim until it closes: a mounted partition or another
incompatible claim makes the open fail with EBUSY before any destination
write. There is no retry or forced unmount. Without `O_CREAT`, Linux leaves
ordinary existing image-file opens unchanged; source-built image production
continues through the same wrapper.

This guard does not implement the installation service's exclusive
admission. Unclaimed raw I/O can still race a claim, image files have no
block claim, and each command releases its descriptor when it finishes,
except that `format --publish` keeps it until its loop clears.
Media/backing-device exclusion, stable identity, the service's held
operation descriptors through execution and trusted destructive consent
remain required; `td-install serve` holds the claim through review only.

The internal `FormatDestination` owns one File and a diagnostic label. Both
formatting bodies borrow that File; neither reopens its label. A
same-process coordinator can retain the object through layout and volume
formatting. `prepare_volume` validates caller-bound programs and account
selection before the destination is acquired or changed; its consumed result
feeds volume formatting. Source paths and contents must remain stable under
the caller's control. This preparation does not establish payload fit,
scratch capacity, disk eligibility, a reviewed installation plan or consent.

The pathname CLI commands each construct and release their own object;
format retains one through both raw writes. A service holding a separate
block claim still cannot invoke them: the distinct holder would conflict.
There is no descriptor-number CLI, claim transfer through /proc, or
cross-process descriptor protocol. The service must own its formatting
object in the process performing writes. `td-install serve` takes the same
read-write exclusive claim at propose and holds that File until the review
ends; its execution will wrap that File, in that process. Deployment
publication under that claim goes through a loop over the held File
("Publishing through a loop over the claim" below), which the QEMU fixture
exercises. The separate pathname-based partition refresh and mount commands
cannot run while this exclusive claim is held and remain only for the
mounted route, after the claim is released.

A regular-file regression replaces the destination name after opening,
then formats both layout and volume through the retained object. It checks
the original's table and cleared volume edges and requires the replacement
bytes to survive both phases. The populated ESP test also replaces the
name, then independently reads the FAT directories, payload bytes and
cluster padding from the retained inode. This proves pathname independence,
not block
exclusivity against raw I/O or hostile hotplug.

The QEMU fixture attempts all three raw commands while the just-formatted
partition is mounted and requires the destination-open EBUSY diagnostic
and unchanged first 64 KiB after each attempt before continuing with
partition refresh and normal publication. This bounds the preservation
check to the protective MBR and primary GPT metadata, not the entire disk.

### Advisory destination probes

`td-install destinations` uses the same x86-64 Linux claim wrapper with
write access disabled. INSTALLER.md's advisory destination discovery
contract owns the bounded metadata filter, temporary claims, output schema
and limitations. Only ResourceBusy (Linux EBUSY, retained as ErrorKind by
the path wrapper) excludes a probed disk; other failures refuse discovery.
No device bytes are written. This does not replace the formatter's
read-write claim or establish the installation service's retained authority.
`td-install candidate-record` shares that read-only observation. INSTALLER.md
owns its binary output contract and its lack of retained authority.
`td-install observe-plan` separately takes a read-write exclusive claim,
compares a bounded reviewed plan with two complete inventories and the
opened block device, then releases the claim after its observation report.
Read-write access mirrors formatter admission and refuses devices where the
destructive path could not obtain its required claim.
The report grants no authority to a later formatter invocation.

### Prepared volume image admission

The volume formatter retains its exclusively created scratch-image inode
while the source-built mkfs child runs. That formatter must write in place
and leave the pre-sized regular file at exactly the planned length; the
source-built td-install-test recipe exercises this contract. After that
child succeeds, the installer opens that same path through the existing
real-regular-file reader and requires
both the original device/inode pair and the exact planned volume length.
The original descriptor stays open through this comparison, preventing inode
reuse from making a replacement look unchanged. The admitted read descriptor
then supplies every sparse-copy pass; its pathname is not reopened.

Missing, symlinked, non-regular, replaced, shortened or grown output refuses
before clearing either destination edge or copying a filesystem byte. This
protects the volume command's destination at that boundary; it cannot undo a
preceding layout command. Staging changes remain in the caller's scratch.
Admission checks the output file's identity and extent, not Btrfs semantic
validity. The declared mkfs and caller-controlled scratch remain trusted,
and the caller must prevent concurrent source mutation during copying. No
claim transfer, filesystem snapshot or destructive authorization is added.

The internal `PreparedVolumeImage` owns the admitted read descriptor and
its exact length. Staging takes a planned length and a held destination
only for the scratch-alias check; it neither reads nor writes GPT and can
finish before layout starts. Its consuming write step first refuses a
source descriptor that aliases the destination inode, independently of the
destination passed during staging. It then rereads the current
GPT through the held destination, checks that the actual volume extent
fits the disk and has exactly the prepared length, and rechecks the held
source length before any edge clearing. Sparse copying and all existing
sync barriers then use those same descriptors. A removed or replaced
scratch pathname cannot redirect the copy. The scratch artifact remains
caller-owned after completion.

The existing volume CLI uses these same preparation and write steps with the
length read from its current GPT. The internal split itself supplies no
service activation, immutable installation plan or capacity admission. The
combined formatter below uses it before layout; a service must additionally
admit disk eligibility. The image alone binds a length, not an offset, disk
identity or destructive consent; its destination check reads the current
table and does not check it against a reviewed plan. Source bytes and
destination topology must remain stable while copying, and a later refusal
cannot undo layout already performed.

### Coordinated raw formatting

`td-install format EFI-KERNEL SELECTOR-INITRAMFS VOLUME-ARGUMENTS` accepts
the same ordered options and operands as `volume`, after the two required
boot files. Option-like boot names refuse with an ordering diagnostic; a
relative filename starting with a dash must use a ./ prefix. It prepares
settings and account validation, opens the destination once, and
builds the GPT/FAT metadata and holds both EFI inputs before staging the
Btrfs image. The image length comes from the new layout, so no existing GPT
is required. All scratch writes, optional staged deployment publication,
mkfs output admission and scratch-image sync finish before the first
destination write. Insufficient staging space or a failed mkfs therefore
refuses before erasing the destination; this performs the real staging work
rather than promising space from a free-byte estimate.

`PreparedLayout` owns the generated metadata, placements and opened EFI
files. Its consuming writer rechecks destination geometry and the held EFI
file lengths before invalidating the old GPT. The combined coordinator
also checks the held Btrfs image before starting layout, and the volume
writer retains its independent GPT-extent and source checks before copying.
The same `FormatDestination` stays open throughout preparation and both
raw writes. The label and input pathnames are never reopened for writing
or copying. The individual layout and volume commands retain their narrow
interfaces and share these implementations.

The combined command reports only the volume writer's three byte counts
(offset, length, copied bytes), after both phases and their sync barriers
succeed. It does not report an intermediate layout success. Existing
write ordering is unchanged: GPT publication precedes volume copying, so
this is not an atomic whole-disk transaction or rollback guarantee. A
write, read, sync or final report failure after erasure can leave a changed
disk. Scratch artifacts remain owned by the caller.

This is a raw formatter, not the installation service. Callers still bind
trusted source-built programs, verified deployment bytes, stable input
contents and topology, and scratch outside the destination. Same-sized
concurrent source mutation remains outside the contract. Boot-file
admission checks type, bounds and FAT fit, not signatures or PE validity.
No disk eligibility, media exclusion, reviewed identity, trusted consent,
mount claim handoff or complete-installation success is supplied. The
seed-only form still publishes the deployment later on the mounted disk;
its later capacity or publication failures cannot preserve old contents.

### Selector identity preparation

`td-install prepare-selector TEMPLATE VOLUME-UUID OUTPUT`
creates the private selector copy described in INSTALLER.md. It appends
the validated volume identity, using `engine/src/cpio.rs`, and leaves
the verified base template's bytes unchanged. The appended archive is
built by `td-boot/src/selector_appendix.rs`, which the storage worker's
selector update and the installed selector's staged-update commit
compile too, so the commit rebuilds exactly the bytes the worker staged
(ENCRYPTION.md "Selector update"). The shared
EFI copier pins the input and rejects size changes; exclusive output
creation cannot overwrite an existing destination. This preparation
belongs before raw formatting and supplies no destructive authority.
The live QEMU installer uses `new-volume-uuid` to draw one version-4 identity
through the formatter's existing random-device reader before preparing the
copy. Its live configuration carries no destination UUID, and its ISO
selector template carries none, as the stock selector does not. The host
checks the reported identity
against the private image's primary Btrfs fsid and refuses reuse across
installations, then requires the same binding on detached boots. The direct
application-evidence boot uses the first installed volume's observed UUID;
other host-only boot oracles retain their existing provisioned selectors.
INSTALLER.md owns the generator's output and future-plan boundary.

The system's root image carries the stock selector at
`/lib/td-boot/selector-initramfs.cpio` (td-boot's root-relative
`SELECTOR_TEMPLATE_PATH`), `td-install` at `/bin/td-install` and the
static `td-btrfs` the deployment initramfs already carries, linked at
`/bin/mkfs.btrfs` (the name D7's `MKFS_BTRFS` gives), and the static
`td-luks` with its debug companion, linked at `/bin/td-luks` (D6),
so a live installer finds them in the root image `live-root` hashed
against the medium's manifest rather than on the medium's unverified
ESP. The image build refuses a root whose
`/bin/mkfs.btrfs` or `/bin/td-luks` does not link exactly that
executable static binary.
That template carries no volume identity, and `prepare-selector` appends
one, so it is that command's template as shipped.

### Refreshing partitions after formatting

`td-init reread-partitions DEVICE` is the explicit kernel refresh between
whole-disk formatting and using the new partition nodes. It accepts one
absolute real block-device path and issues the value-pinned BLKRRPART
request through td-init's existing syscall boundary (UNSAFE.md §3). Linux
requires CAP_SYS_ADMIN, rejects a partition operand, and refuses a disk
whose partitions remain open. A scan error stops the operation; no unmount,
force, retry or regular-file fallback occurs. It emits no stdout on success.

The formatter's existing regular-file and block-device path remains one
implementation with no raw syscall. A caller needing only a completed disk
image still needs no kernel refresh. A live coordinator wanting to mount a
new partition must sync its table first, invoke the refresh, and validate the
result against its plan. The applet does not parse GPT, prove complete
partition publication, authorize erasure or provide exclusive admission.
Its read-only descriptor stays held across the request; namespace and device
stability remain caller obligations. The kernel may block during the scan,
so the caller owns the operation's outer deadline.

All three formatter commands sync their destination before returning success;
the diagnostic relies on those explicit barriers before requesting a reread.
The QEMU diagnostic invokes it after layout and volume formatting, resolves
the preselected UUID on the expected new partition, mounts that Btrfs volume,
and requires EBUSY from a reread while it is mounted. It unmounts and requires
a final successful reread before reporting installation success. Both optical
and USB installation legs require this evidence before direct publication
onto the mounted destination, as described below.

### Seed-only initialization and mounted publication

`td-install volume [--uuid UUID] [--timezone IANA-ID] DESTINATION MKFS
SCRATCH --seed`
formats the same Btrfs image and destination region as bare `volume`, but
initializes the publication directories without copying a deployment into
scratch. The existing publishing form (`... SCRATCH TD-BOOT DEPLOYMENT`)
remains available for unmounted image production; the two forms cannot
be combined. Bare formatting remains unprovisioned.

Directory modes are 0755. This mode does not verify a deployment, create
`current` or `previous`, or claim a bootable installation. The
coordinator must verify its stable source before erasure, then complete
verified publication through `td-boot install DEVICE MOUNTPOINT SOURCE`.
That command remains the single transaction writer and rechecks copied
payloads. The formatter itself gains no dependency or syscall for this;
the loop publication below is a separate route with its own surface.

The diagnostic ISO's interrupted-publication leg uses this sequence after
preflight and layout. After seed-only formatting it requires empty staged
boot, deployment and incoming directories and only the selected timezone
under `@var`, refreshes and checks the partition devices, deletes its entire
owned scratch directory, then invokes `td-boot install` on the resolved
partition. Its ordinary legs publish through the loop below instead, before
the same checks. Either way publication streams from read-only media onto
Btrfs. The host requires the direct
publication marker as well as successful detached boots from the expected
deployment. Deployment-sized staging copies no longer consume guest RAM;
the sparse formatter image still needs metadata space and scans the logical
volume size. This small fixture alone does not establish full-system RAM
or capacity bounds; the full-system diagnostic in §8 supplies a separate
2 GiB guest execution check. Production capacity admission remains open.

### Publishing through a loop over the claim

`td-install format EFI-KERNEL SELECTOR-INITRAMFS [OPTIONS] DESTINATION MKFS
SCRATCH --seed --publish TD-BOOT DEPLOYMENT MOUNTPOINT` formats
exactly as the seed-only form does, then publishes the deployment before it
releases the destination. Only `format` accepts `--publish`, only directly
after `--seed`, and its three operands must be absolute: td-boot
requires two of them, and a bare program name would resolve through `PATH`.

After the volume copy and its sync barriers, the formatter rereads the
volume extent from the table it just wrote through the held destination and
binds a loop device over exactly that byte range, with the destination's
logical sector size as the loop's block size. It then runs `TD-BOOT install
/dev/loopN MOUNTPOINT DEPLOYMENT`. The loop's backing file is the
destination's own open file, so the exclusive claim, which refuses a mount
of any partition of the disk, lasts while the loop is bound, and nothing
names the disk by path. The kernel's partition table is neither read nor
refreshed; it may still describe the disk as it was. td-boot mounts,
publishes and unmounts as it does on any device, and its stdout, the
deployment id, is copied to stderr. The formatter then closes the loop's
last opener and requires the kernel to have cleared it within a second
before it syncs the destination. The byte report is written only after
publication succeeds and the loop has cleared, so a report means a
published disk and no loop left behind.

Before the destination is opened, the formatter requires the deployment to
be a directory and the mountpoint an empty one. td-boot takes those two in
the other order, so a swap is the likely mistake, and without the check it
would surface only after the disk was written.

The loop is bound in one `LOOP_CONFIGURE` call and checked through sysfs
before use, including that it is backed by the destination's own file;
UNSAFE.md §21 owns that surface, its layout and its read-back.
It is not read-only, not partition-scanned and not direct-I/O. The formatter
then holds it only through a read-only descriptor, because the pinned kernel
refuses to mount a device another descriptor holds open for writing. A
regular-file destination takes the same path, a loop over a file, so D9's
single implementation holds. Binding needs privilege, so the regular-file
tests reach only the refusal, and no report is written. The QEMU fixture is
the kernel evidence: its ordinary legs publish this way on virtio and NVMe
targets at 512-byte and 4Kn sectors and on AHCI at 512-byte, including the
reinstall over an interrupted publication, and require that no loop device
is left bound afterwards. Its interrupted-publication leg keeps the mounted
route, whose publisher it can stop on its own.

A failure after the first write leaves the disk as the raw formatter does: a
seed-only volume with no selector, or a partial publication whose recovery
td-boot's transaction owns. The live execution adds one state: a fully
written installation whose verification failed, with its table withdrawn
("Executing a consented installation"). Nothing retries. If td-boot leaves
the loop mounted, the loop and the claim's file stay bound until it is
unmounted, and the formatter reports the loop still bound rather than
success. The residual is the claim's: a non-exclusive raw writer, or a
privileged process that opens or detaches the bound loop, is not excluded.
The caller still binds stable trusted sources, an absolute td-boot and a
mountpoint nothing else uses. This is the one-process publication the
installation service's execution needs; it does not activate that execution.

### Device-bound formatting

A plan naming device-bound storage, which a service's own probes choose
(INSTALLER.md "Storage choice"), is executed as "Executing a consented
installation" says, except as follows. [ENCRYPTION.md](ENCRYPTION.md)
"Device-bound formatting" owns the volume format and its parameters.

No plaintext volume image exists. The settings and account are staged as a
directory tree in the workspace before the first write, as on the
unencrypted path. The only secret among them is the carried Wi-Fi networks'
PMKs ("Carried Wi-Fi networks"), in the root-only workspace and then inside
the encrypted volume. The scratch image and its admission ("Prepared volume
image admission") remain only for the unencrypted path. The layout's writes
stop short of the table: the old table is invalidated and the ESP written,
each synced, as on the unencrypted path, but neither copy of the new table
is written until the recovery key is acknowledged (below). A refusal by the
layout's recheck before its first write leaves the disk untouched. The
formatter then binds the loop over the layout's volume extent, which no
table on the disk yet places, as "Publishing through a loop over the claim"
does. The loop covers the whole partition, which the layout ends on whole 4
KiB encryption sectors ("Disk layout"); an extent that is not whole sectors
is refused before the first write rather than shortened, because a format
shorter than the partition would be a volume no mapping of the partition
opens. Then, on that loop:

1. draws the recovery key and the first-boot protector secret from
   `/dev/random` through td-protector;
2. runs `format` with ENCRYPTION.md's parameters;
3. adds keyslot 1 with `add-key 0 1 DEVICE NEW-FD`, the compiled PBKDF
   parameters and the recovery passphrase authorizing it;
4. seals the protector under the first-boot policy on a fresh TPM client
   and imports token 0, naming keyslot 1, with `token-import 0 DEVICE`, its JSON first held to td-protector's 4096-byte
   `MAX_TOKEN_JSON`;
5. opens the mapping `/dev/mapper/td-install-TAG`, the workspace
   directory's own name, with the recovery passphrase;
6. runs `mkfs.btrfs` on the mapping, with the loop's length less 16 MiB
   as `--byte-count`, the plan's UUID, the label, `--rootdir` over the
   staged tree and the `@var` subvolume, as on the unencrypted path;
7. runs `td-boot install` on the mapping, then closes it whether or not
   opening, mkfs or td-boot failed, since an open that failed may still
   have loaded its table: so the loop can clear and no volume key stays
   live. Only device-mapper's answer counts, never a node under
   `/dev/mapper`: `status NAME` is asked before each close and after the
   last, exit 0 meaning active and 4 inactive (cryptsetup 2.8.8's
   `action_status` returns `-ENODEV` for an inactive name, which
   `translate_errno` makes 4), and any other exit is taken as active. An
   inactive mapping needs no close. A close is asked up to three times, a
   second after each that fails. A mapping not inactive after the last is
   a write failure whose error says so: its volume key stays in the
   kernel until the live session ends.

Verifying boot then also requires, while the loop is bound, that
`test N DEVICE` succeeds for keyslot 0 with the
recovery passphrase and for keyslot 1 with the protector secret, and that
the header, read back through the claim by td-protector's reader from a
valid primary copy no older than the secondary, carries the plan's UUID,
the label, keyslots 0 and 1 alone and exactly the token imported as token
0 with no orphan, which `verify_first_boot_object` on a fresh TPM client
accepts. The reader reports neither whether both copies were valid nor
the data segment's cipher, sector size or offset; the keyslot tests authenticate the keys and td-luks's compiled profile
validates the cipher, sector size and offset. These
checks report the verifying boot phase, and their failure is
verification failed. The protector secret is then zeroed and the loop
released as before, and the boot artifacts are read back as on the
unencrypted path, except that both of the table's ranges must still read
zero. Nothing names a partition of the destination: td-luks,
mkfs.btrfs and td-boot see only the loop and the mapping.

The execution then removes its workspace and holds the claim while the
service holds the recovery key for its installer to show and acknowledge
(INSTALLER.md "Device-bound records"): it hands the key's 48 digits to
the service, keeps no copy, and waits. Only the acknowledgement makes
the disk bootable: the execution then writes the table, backup then
primary, each synced, and reads both back as verifying boot does, and
only then is the installation complete. Deferring the table rather than
token 0 keeps everything verifying boot checks in the volume and its
header settled before the phase: a disk without a table is one firmware
skips, while one without the token would still boot to recovery. A
failure to write the table is write failed and one to read it back
verification failed, each withdrawing the installation. An installer
lost first, before or during the phase, answers the execution lost: it
withdraws the installation and fails as recovery unconfirmed.
ENCRYPTION.md "Device-bound formatting" says what a crash before the
acknowledgement leaves on the disk, no table, so nothing firmware boots,
and what one between the backup and primary writes leaves: the backup
GPT alone, without a protective MBR, which firmware may or may not boot,
with the key already acknowledged.

Every td-luks child runs through td-protector's runner, which
the selector shares (td-protector/DESIGN.md "LUKS runner"), and
execs `/bin/td-luks` from the verified root with a cleared
environment, its output captured, copied to standard error and zeroed,
and its error inherited. Key material reaches it only by descriptor. The first secret travels on
standard input as `--key-file=-`, written whole and then closed. The
second, the new key `add-key` takes as its key file, travels through a
pipe created with std: the runner writes the secret, closes the write end,
keeps the read end, close-on-exec, and names it to the child as
`/proc/<td-install pid>/fd/N`, so td-luks opens the pipe anew and reads
exactly the secret and end of file. Standard input carries the
passphrase, at most 48 bytes, or the token's JSON, and a pipe is filled
whole before its child starts, so its input is held to one page, the
least any pipe holds, and more is refused rather than risk a write that
blocks. That `/proc` name opens only for a process the kernel lets trace
td-install: td-install stays dumpable and runs td-luks under its own
uid, and a `PR_SET_DUMPABLE(0)` would break `add-key`. No argv element
or environment variable carries key material; a descriptor path names no
secret. This adds no `unsafe` and no syscall to UNSAFE.md: pipes and
children are std's and the device-mapper requests are td-luks's own.

The cost of formatting in place is the refusal order. A failing mkfs,
which the unencrypted path meets before the first write, is here a write
failure after layout. On this path every failure after the first write,
and the loss of the installer before its acknowledgement, withdraws the
installation: both of the table's ranges and the volume's first 16 MiB
are zeroed through the claim, so no keyslot of an unfinished installation
stays openable, then synced, each attempted whatever became of the
others. The outcome keeps its stage's code: write failed while writing or
writing the table, verification failed in the checks or reading the table
back, and recovery unconfirmed for a lost installer. A withdrawal that
itself fails keeps that code too, as at verifying boot ("Executing a
consented installation"): its error names each part that failed, and the
disk may keep what that part did not zero.

`format` does not erase the data segment, and nothing here does:
ENCRYPTION.md "Device-bound formatting" says what of the disk's earlier
plaintext an attacker with the disk still reads.

### Payload fit

A review is presented only for a disk that holds what installing its
source writes. After validating the source under the claim, and with
the plan's volume identity in hand, the service sizes the source's
`bzImage`, `initramfs.cpio` and `root.erofs` under td-boot's source
naming, each a real regular file, and the selector that preparing the
stock template with that identity yields.

The system volume must hold one copy of the three payloads and 1 GiB, the
share td-boot's `MIN_VOLUME_BYTES` reserves for Btrfs metadata and `@var`;
a disk short of that is refused as insufficient space. That is what
installing writes. Retaining further deployments through updates is
`MIN_VOLUME_BYTES`'s floor, which the layout enforces, and beyond it an
update that does not fit fails while staging without changing what boots
(td-boot/src/protocol.rs). The stock deployment is about 3 GB, so a
three-copy rule would refuse the stock 6 GiB install target.

The kernel must be within the execution's 1..=256 MiB bound, and the ESP's
FAT, built from the kernel's and selector's lengths alone, must hold both.
The ESP is the same 512 MiB on every disk, its FAT capacity differing only
by a sector size's reserved sectors, so either failure is the source's to
remedy and refuses as source unavailable, as does a payload, key or template
that cannot be sized. Any refusal releases the claim.

The sizes are read from the source's files, which carry no length in the
manifest. The execution asks the volume fit again of the source as it is
before anything is staged, and refuses as insufficient space with the disk
untouched. Past that point the payloads td-boot publishes are bound only
by their digests; a source that changes size afterwards meets ENOSPC
during publication.

A device-bound plan subtracts from its partition, which the layout
already ends on whole 4 KiB encryption sectors, the 16 MiB LUKS2 header
and keyslots area (ENCRYPTION.md "Device-bound formatting"), for this
fit and for the layout's minimum volume size, which the volume inside
the header must still hold.

Scratch is not sized here. The execution's workspace under `/run` holds
the kernel copy, the selector and a sparse volume image whose blocks are
mkfs's metadata and the staged settings; a device-bound execution holds no
volume image. A shortage there is met before the first write: as
insufficient space where this process meets it, and as write failed where
mkfs.btrfs reports only its status, with the disk untouched either way.

### Executing a consented installation

On trusted live media the wizard's final review provides the human
confirmation, for erase and preserve alike. Root acknowledges its exact
Execute through the private lifecycle channel; no secure attention prompt
is involved. This does not change installed-system update authorization.


`td-install serve`'s execution installs onto the disk the service holds,
on the service's own thread, once consent arrives and the held disk has
rechecked. Its inputs are serve's four operands and the plan, never the
installer's: the deployment directory, `td-boot` and
`td-firstboot` as bound, and from the verified root its stock selector
template (`SELECTOR_TEMPLATE_PATH`) and `bin/mkfs.btrfs` (D7), and the
running system's time zone catalog, re-read from the read-only root the
settings were checked against. The root is `/` because td-authd binds the
verified root there; the execution does not assert it. A live boot's
init records the deployment its root was verified as in
`/run/td-deployment`, and the execution reads it too. It also reads the
live session's saved Wi-Fi networks under that root, which are the
session's mutable state rather than the deployment's, under their own
rules ("Carried Wi-Fi networks").

Before the destination is touched it checks the source. The booted record
must name the plan's deployment, since the account and catalog checks, the
selector template and `mkfs.btrfs` are all that root's. The source's
manifest must hash to the plan's deployment digest, which source
validation produced; td-boot's manifest has a fixed line order, so its
`bzImage` entry is then the digest the ESP kernel's bytes must have.
td-boot's source naming applies to the kernel: the deployment's `bzImage`
or the medium's `bzimage`, a real regular file, never both as two files.
The kernel is copied into the workspace, which only root can write, and
the formatter hashes that copy through the descriptor it pinned and
rewinds it, so the bytes checked are the bytes copied; a mismatch refuses
before the first write. The selector is the template with the plan's
volume identity appended (`prepare-selector`). The
review's volume fit, with the layout's own refusal of a disk too small for
it, is asked again of the source's payloads before anything is staged.

The execution works in a fresh root-only directory under `/run`, named by
the first eight bytes of the plan's nonce: the kernel copy, the prepared
selector, the formatter's scratch and td-boot's mountpoint. A directory of
that name already present refuses and is left alone. It formats the held
claim directly, the same `format` body without reopening anything, with
the plan's time zone, hostname and account and publication through a loop
over the claim (above). The service is told writing filesystems once the
layout's and the volume's rechecks have passed, immediately before the
first byte is written, and publishing the deployment as publication
starts. The account and regional settings are staged into the volume
image before any write, so no separate settings phase is reported.
The selector, like the kernel, is hashed through the descriptor its copy
reads before the first write: its digest is taken from the private copy
prepared above, which only this execution writes.

Verifying boot follows: td-boot's output must be one deployment id, and
the plan's, and the boot artifacts are read back off the disk through the
claim against what was written: both copies of the table byte for byte,
and with them the ESP's entry; the ESP's filesystem metadata byte for
byte, the directories firmware resolves the files by among it; the ESP's
kernel hashing to the manifest's `bzImage` digest and its initramfs to
the prepared selector's, each with its last cluster's rest zeroed. Read
through the claim, this checks what was placed where, not the medium: the
reads may be served from the kernel's cache, and durability rests on the
syncs before them. The deployment's payloads on the volume are td-boot's,
verified by digest as it publishes them. Complete in INSTALLER.md's sense
follows only this. An installation that fails verifying boot is withdrawn
from firmware: its table is invalidated again, as the layout does before
it writes, and the outcome is verification failed.

Every failure is written to standard error and maps to one protocol failure,
by the stage that met it. A source or layout check, preparing the selector
from the template, td-boot's output, the published id and
the read-back of the boot artifacts are verification failed; the time zone,
hostname, account or seed operands are settings failed; a disk too small for
the layout or a deployment the volume cannot hold, or a workspace that
cannot be created, is insufficient space; staging the volume image in
scratch is write failed; and an error this process meets as a full
filesystem or exceeded quota before the first write is insufficient space
(mkfs.btrfs reports only its exit status, so its own shortage is write
failed). After the first write every failure until verifying boot is write
failed, and the disk may be incomplete. The failed status carries no phase,
so a client cannot tell the two write failures apart and treats either as a
disk that may be incomplete. A verification failure at verifying boot leaves
a disk fully written but with its table withdrawn, which firmware does not
boot; if withdrawing it also fails, the error says so, and the disk may keep
a valid copy of the table, which firmware may boot from: each copy is zeroed
whether or not the other could be, and no order avoids that. The workspace is removed
whatever the outcome, each part attempted even where another fails, and also
on drop, which covers a partial creation and an unwinding panic. The shipped
binary aborts on panic, and a killed process drops nothing, so there the
workspace stays in `/run` until reboot; a later review draws a fresh nonce
and so a fresh name. Its mountpoint is removed only if empty, so a volume
td-boot left mounted is never walked; that, like any other removal failure,
is reported and leaves the outcome as it was.

Production constructs this execution and `run_serve` opens the consent
channel, so it runs once td-authd answers a review with consent. The kernel
check binds the ESP to the approved manifest; the remaining payloads
are td-boot's to verify as it publishes them. The execution's own path past
the loop attach, which unprivileged tests cannot reach, is
`qemu-install-system`'s: the guest drives the service as root and boots the
disk it installed.

### Full-system volume consumers

`td-boot on-volume OPERATION ARGS...` supplies the device operand to
`boot`, `install`, `update`, `rollback`, `success`, `mount-root` or `mount-var`.
The operation keeps the existing verification, transaction locking and
publication semantics. Other verbs and nested `on-volume` are refused.

`rollback` names the pair it acts on: `td-boot rollback DEVICE
MOUNTPOINT CURRENT PREVIOUS`, and `on-volume rollback MOUNTPOINT
CURRENT PREVIOUS`, two different canonical deployment IDs. Under the
transaction lock, before any change, a stale selector temporary's
reaping included, it requires `current` to name CURRENT and `previous`
to name PREVIOUS, and refuses otherwise, so a rollback approved for one
pair never acts on another (td-authd/DESIGN.md, "Elevation
operations"). It then makes `previous`, which must be verified and not
pending, current, leaving `previous` naming it, and prints its ID. There
is no form without the pair. Its callers are td-authd's
`deploy-rollback` and the boot-health pass, which names the deployment
it just installed and the running one. The automatic rollback of an
exhausted candidate is `select_boot_deployment`'s, not this verb's.
Explicit-device verbs remain low-level interfaces for formatting and
controlled diagnostics; the full-system profile uses `on-volume` throughout.
The explicit mount forms are `td-boot mount-root DEVICE MOUNTPOINT` and
`td-boot mount-var DEVICE MOUNTPOINT`; both paths must be absolute.

Boot volume-binding and selection diagnostics are formatted completely
before td-boot passes their bytes to `write_all`. This prevents formatting
from splitting a record into separate writes, while preserving diagnostic
text, ordering and I/O error propagation. Short writes can still require
multiple calls, and this is not an atomic serial-console guarantee. The
QEMU oracle continues to reject incomplete or interleaved binding evidence;
a later deployment marker cannot substitute for the selector's binding.

For `boot`, the expected UUID comes from the selector's own
`/etc/td/volume-uuid`, a real regular file containing exactly the canonical
UUID and one newline. Missing or malformed configuration refuses. The
selector does not discover an arbitrary sole td disk. It rejects any
existing `td.volume=` command-line token and appends its configured UUID
before the ordinary verified kexec builds and measures the final handoff.
Other operations require exactly one canonical `td.volume=` token from
`/proc/cmdline`. Thus deployment mounts, boot acknowledgement and update
installation retain the identity chosen by the selector.

VM provisioning puts the same UUID in the selector and Btrfs formatter.
Host-only provisioning profiles draw a random one per run, as VM
installations do; those profiles' alternate disk fixtures retain that
run's UUID and must not be attached together. Live installation
fixtures instead generate a fresh UUID inside the guest for every install
and destructive reinstall. An installed volume retains its UUID across
updates. Selector configuration is boot-source data, outside deployments
and the writable volume's authority; this is not Secure Boot authentication
of that source.

After a complete uniqueness scan, `on-volume` reopens and validates the
selected node and UUID, retaining its read-only descriptor through every
mount, child command and transaction. Mounts use `/proc/PID/fd/N` naming
that held parent descriptor, so replacing a devtmpfs pathname cannot
redirect the mount. Procfs, sysfs, root and the kernel remain trusted.
This is not exclusive physical-device admission or protection against
hot removal, device-number reuse or changes by a privileged kernel/device
administrator. Discovery precedes the existing transaction locks; a
concurrent superblock change can cause a conservative refusal.

Btrfs may retain the descriptor pathname after its owner exits. A stale
update mount is therefore verified by mounting the expected held device
read-only at a fresh private directory under the transaction lock and
comparing the mounted top-level filesystem device identities. The existing
mount-table check still requires one top-level Btrfs mount with exactly the
required writable/restriction flags before recovery unmounts it. A mismatch
or failed probe/unmount refuses. Cleanup never recursively removes a probe
path; process death may leave a probe mount until reboot. Reused PID/fd
names do not authenticate a stale mount.

The selector and deployment initramfs mount sysfs before discovery.
`on-volume mount-root` uses `ro,nodev,nosuid,noexec`; `mount-var` uses
`rw,nodev,nosuid,subvol=@var`. The verified EROFS root and existing persistent
state setup are unchanged. The full-system oracle moves an installed
pending candidate to `/dev/vdb` behind a blank disk, then requires its
UUID evidence, persistent state, greeter and successful acknowledgement.

Encrypted volumes are increment 6's target ([ENCRYPTION.md](ENCRYPTION.md)
"Selector release"). Discovery identifies a LUKS2 partition whose header,
read by td-protector's bounded reader, carries the expected UUID and the
`td-system` label ("Read-only volume discovery primitive"), and
`on-volume` pins it as it pins a Btrfs partition. That partition and the
dm-crypt mapping over it are one volume, not two devices with one UUID:
a mapping is admitted only when its `/sys/block/dm-N/dm/uuid` is
`CRYPT-LUKS2-<the UUID's 32 hex digits>-<name>`, `<name>` being its
`dm/name`, and its `slaves/` names exactly the pinned partition, and it
is found that way, never by a `/dev/mapper` name; the scan never counts
a `dm-N` node as a candidate of its own. A mapping claiming the UUID
whose name disagrees or whose `slaves/` names anything else, and a
second such mapping, refuse. `dm/uuid` is read up to 129 bytes, the
kernel's 128 characters and a newline. A `dm-N` or attribute that
vanishes during the walk makes it incomplete, and the walk is repeated
for up to five seconds. An admitted mapping's one slave must have, in
its sysfs `dev`, the device number of the held partition descriptor, so
a partition name the kernel gave another device since the pin is not
taken for it. The mapping is then opened through `/dev/dm-N` under the
same device-number and inode checks and must carry the td Btrfs
superblock with the volume's UUID; its `dm/uuid`, `dm/name`, `slaves/`
and `dev` are then read again and must still describe the opened node,
whose descriptor is held for the consumer. The installed selector's
`on-volume boot` of an encrypted volume runs the release order on the
pinned partition, before any mount
(ENCRYPTION.md "Selector release"); a mapping already active then
refuses boot. The released secret, or the recovery key on keyslot 0
when the release ends in the recovery flow, opens the mapping
`td-selector`; on a protected header (ENCRYPTION.md "Protected
release"), td-protector's protected release instead, its entries
through the same `secret-line` and its security keys through td-boot's
FIDO2 client, one client per Enter, opens it on the released keyslot
alone, takes the volume key, then runs td-protector's plan and lockout
steps; a device-bound or refused header keeps the device-bound order,
and from 8f an upgrading one runs td-protector's upgrade boot
(ENCRYPTION.md "Re-encrypting upgrade") or, in U5, the protected
release; the upgrade's no-mains and cancelled boots keep the
device-bound order without a reseal, the cancelled one's cancellation
after its open, and a device-bound header's open then removes pending
orphans. td-luks names the partition by its held descriptor,
`/proc/<pid>/fd/N`; the mapping is then admitted by this walk, under
that name only, held with the partition, and the boot's mounts name it
as `/proc/<pid>/fd/M`. It does not survive `kexec`.

The deployment initramfs opens its own mapping with the handed-off key
([ENCRYPTION.md](ENCRYPTION.md) "Boot and authority boundaries" owns the
key, the refusals and the post-cap check): `on-volume mount-root` runs
td-luks on the partition by its held descriptor, `/proc/<pid>/fd/N`,
under the fixed name `td-system` (td-boot's `VOLUME_MAPPING_NAME`), so
its `dm/uuid` is `CRYPT-LUKS2-<hex>-td-system`; it then admits the
mapping by this walk, requires that name, and mounts the held mapping
node, `/proc/<pid>/fd/M`, as it mounts a Btrfs partition, reporting the
partition as the volume. It refuses a mapping already active before it
opens one. `on-volume mount-var` requires the active `td-system`
mapping `mount-root` opened, admitted the same way, and mounts it;
neither reads the key again. A failure after the open leaves the
mapping until the reboot that refusal causes.

The running system's `install`, `update`, `rollback` and `success` bind
the same mapping. When discovery finds the handed-off UUID's td LUKS2
partition with its admitted `td-system` mapping active, `on-volume`
holds the mapping's descriptor as the operation's device,
`/proc/<pid>/fd/M`, exactly as it holds an unencrypted volume's Btrfs
partition, reports the partition as the volume, and runs the
operation unchanged: its mounts, transaction lock, stale-mount probe and
publication reach the Btrfs inside through that descriptor. Admission
is the walk above, so a mapping over another device, or whose Btrfs
carries another UUID or none, refuses. The running system never opens
or unlocks the volume: it runs no td-luks, reaches no TPM and reads
no key, so a LUKS2 partition with no active mapping, or with an active
mapping under another name, refuses. The planned exception is
ENCRYPTION.md increment 8's `td-install storage-operation` ("Storage
operation worker" below): the
upgrade's first phase reads one TPM capability, it and the requests
import key-less tokens, protector management adds and kills keyslots
under a disk proof's secret, a FIDO2 passphrase or the recovery key, a
request reads the volume key through td-luks's volume-key dump under
that secret to tag its token, a login-key disk proof tests one of them
on its keyslot, the selector update writes the ESP, and Secure
Boot enrollment signs with a TPM-held key and writes EFI variables;
none of them opens the volume, and only a request reads its key.
Device-mapper refuses, or defers
until the last close, removing a mapping that is open, so the held
descriptor keeps the mapping for the operation; root reloading its
table is a privileged device administrator's change, outside this
boundary as above. `td-boot volume` pins an encrypted volume the same
way and prints the mapping node after the partition, refusing as these
operations do ("Read-only volume discovery primitive").

No consumer refuses an encrypted volume as such; each requires its own
mapping state. The selector's `boot` requires no active mapping before
its release (one halts it) and admits only the `td-selector` mapping it
opens; `mount-root` requires no active mapping and admits only the
`td-system` mapping it opens; `mount-var`, the running system's
operations and `td-boot volume` require the active admitted `td-system`
mapping and refuse without it. An unencrypted volume's boot and
operations take the same steps as before.

The native installation oracle provisions its chosen UUID into both the
formatter and the selector before writing the disk. Its tiny selector
uses the same `on-volume boot` entry, including the selector-owned UUID
configuration and verified handoff. The oracle requires the exact planned
UUID on both sides of kexec and after disk reordering. Installation target
admission remains the fixture's private serial convention. Neither path
activates a physical-device installer, machine settings or destructive
user consent.

`td-install volume [--uuid UUID] [--timezone IANA-ID] DESTINATION MKFS SCRATCH
[TD-BOOT DEPLOYMENT | --seed]` accepts a preselected filesystem identity.
The optional pair appears immediately after `volume`; UUID is exactly
36 ASCII bytes of nonzero, canonical lowercase UUID text. Invalid input
refuses during argument parsing, before destination or scratch access.
The formatter passes the supplied value unchanged to `mkfs.btrfs --uuid`.
Omitting it retains the low-level operation's fresh random UUID behavior.
This does not change GPT disk/partition GUID generation or the output's
three numeric fields.

A live-profile producer must choose the per-installation UUID and provision
that same value into the fixed selector before creating the ESP. The
formatter cannot inspect or repair a mismatched selector. A UUID identifies
the filesystem; it provides no authentication, uniqueness proof or erase
consent. The caller owns unique provisioning, stable device topology and
exclusive admission. Reusing a UUID on two attached volumes deliberately
causes boot discovery to refuse. The live diagnostic generates a fresh UUID
per installation with `new-volume-uuid`, including separate optical/USB
destinations and destructive reinstalls.

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
third-party bootloader exists on the path. `linux-x86-64.rs` enables both,
with their ACPI prerequisite confirmed in Linux 7.2.13's `arch/x86/Kconfig`.
The deprecated handover entry points and mixed 32-bit firmware mode are
explicitly disabled, as is the EFI variable filesystem. EFI runtime-map
support follows KEXEC_CORE; other symbols under the EFI menu retain
upstream defaults. Producer and consumer recipes use `AssertEfiApplication`
to require MZ/PE signatures, x86-64 PE32+ EFI application identity, bounded
headers/sections and a nonzero file-backed executable entry point. The Rust
reader admits at most 96 sections, 64 KiB of headers and a 256 MiB file. It
is a format check, not a complete PE loader or signature verifier.
The signed selector packaging and full system firmware oracle remain
required. The stub has a built-in command line naming
`initrd=/EFI/BOOT/INITRD`, `console=tty0`, `console=ttyS0,115200`,
`rdinit=/init`, and `panic=-1`. Linux makes the last `console=`
`/dev/console`, so kernel messages reach both the VT and the serial
line while `/dev/console` stays `ttyS0` (ENCRYPTION.md "Keyboard
console"). `CONFIG_CMDLINE_OVERRIDE` stays
off so firmware, direct-kernel tests and kexec can supplement those
defaults. `INITRD` is
an 8.3 name on the same FAT filesystem as `BOOTX64.EFI`. Linux's EFI
loader converts these forward slashes to FAT separators; forward slashes
also satisfy td-boot's unquoted printable command-line grammar.

These are v1 QEMU product defaults, including serial diagnostics and immediate
panic reboot. They apply to direct boots and kexec too; live installation
media override the panic policy from their early initramfs entry and live
kexec handoff, as specified in MEDIA.md "Live boot diagnostics". Audit policy remains
with the caller: non-audit direct-kernel tests pass `audit=0`, while the audit
oracle passes `audit=1`. Linux permanently disables audit initialization on
any earlier `audit=0`; a later `audit=1` cannot restore it. The built-in
prefix therefore contains no audit token. Firmware boots without an explicit
policy retain Linux's initialized but disabled audit default; initialization
can still permit unconditional seccomp diagnostics. The selector's
keyboard console is no longer deferred: increment 7 specifies it
(ENCRYPTION.md "Keyboard console"). The kernel carries the firmware
framebuffer (simpledrm over sysfb's simple-framebuffer) and the prefix's
`console=tty0`, so a UEFI GOP display shows the kernel's messages even
where td has no driver for it. td-boot's lines are mirrored to the VT
and the recovery prompt is offered on both consoles; an installer that
encrypts only where the live system shows a keyboard console
("Activation") remains increment 7's target. What stays deferred is a
display with neither a UEFI GOP framebuffer nor a td driver, a keyboard
td's kernel has no driver for (USB keyboards on ports served by OHCI or
UHCI controllers, companions included, and I2C-HID and Bluetooth
keyboards), keymaps other than US on the VT, and a console policy for the running system's own text greeter, which stays
on `ttyS0`, so a machine without a serial port has none.
Each kernel entry prepends the built-in prefix; selector-to-deployment
kexec therefore adds a second copy. The current short profile arguments
fit, but this consumes part of both td-boot's 2048-byte command-line bound
and the kernel's 2048-byte buffer: each copy is 79 bytes, 13 of them
`console=tty0 `. Future argument expansion must reserve room for that
second prefix rather than relying on kernel truncation.

The x86 kernel recipe discards the inherited EFI initrd configuration-table
entry before parsing tables on a kexec boot. Its one-shot loader allocation
can have been reused for the incoming kernel; reading and clearing that
old table can corrupt the new kernel. The selected initramfs arrives through
the new kernel's `boot_params`. Table invalidation also runs without an
SMBIOS pointer. Normal firmware entry still consumes its initrd table,
and EFI runtime services remain enabled across the selector handoff.

Firmware passes **no command line** on the removable-media path, so the
stub's must be built in (`CONFIG_CMDLINE`). That costs nothing under
this design and is the reason the design is shaped this way: the kernel
on the ESP is a *fixed* stub whose only job is to reach `td-boot` on the
Btrfs volume, and the per-deployment command line is the one `td-boot`
already builds for its kexec. The ESP therefore never changes when a
deployment does, which is D5 restated as a property of the boot flow
rather than as a rule. A boot entry naming the stub can still add load
options, which the stub appends to the built-in line; `qemu-boot-live`
gives a firmware-booted live medium its test tokens that way.

`td-install layout DESTINATION [EFI-KERNEL SELECTOR-INITRAMFS]` can
populate both fixed boot files while creating the GPT and ESP. Supply both
inputs or neither; the latter retains the empty-ESP layout operation. The
inputs are nonempty real regular files, each bounded at 256 MiB, opened
once before erasure, and must fit together with FAT metadata. An input
with the destination's device/inode identity is refused. This detects
regular-image aliases; it does not detect files on a partition of a block
destination. The caller must keep sources off the destination disk and
supply stable, validated boot artifacts. This low-level writer does not
authenticate a selector or verify PE format. It detects source length
changes during streaming, but does not snapshot concurrent content writes.

The old GPT is invalidated and synced first. Reserved sectors, both FATs
and all directory clusters are cleared before writing metadata. File data
is streamed and final-cluster padding is zeroed; unallocated clusters are
not erased. All ESP writes are synced before the backup and then primary
GPT are published with their existing barriers. A streaming failure leaves
no published GPT. This is a destructive layout primitive, not the complete
installation transaction: the Btrfs volume still needs formatting and
verified deployment publication. There is no new consent or device
admission interface here.

### Selector deployment measurement prerequisite

The selector can require one SHA-256 PCR 11 extension before its verified
kexec handoff. Its own rootfs must contain the regular, bounded file
`/etc/td/boot-measurement` with exactly `td-selector-pcr11-v1` plus a newline.
Absent configuration preserves the unenrolled development profile; malformed
or unreadable configuration refuses boot. Neither the deployment volume nor
a kernel command-line token enables or disables this policy. The stock
selector has no such file. The full-system secret QEMU oracle provisions it
in its private selector.

The version-1 event is the concatenation of `td/selector-deployment/v1`,
one zero byte, the 64 lowercase ASCII bytes of the verified manifest ID,
a big-endian u32 byte count, and the exact final kexec command line without
its terminating NUL. The ID binds kernel, initramfs and root payload hashes;
the command line includes the chosen deployment and any bookkeeping warning.
Whitespace and argument order are significant. All boot decisions, including
read-only recovery, pass through this same handoff with already verified
kernel/initramfs descriptors. No payload pathname is reopened
for measurement or kexec.

The selector reads PCR 11 through root's `/dev/tpmrm0` and requires zero,
extends the event's SHA-256 digest once, then requires exact readback of
`SHA256(zero32 || event_digest)`. It runs over the shared td-tpm client
([td-tpm/DESIGN.md](../td-tpm/DESIGN.md)): commands select only PCR 11 in
the SHA-256 bank, and a reply naming another bank or PCR, or anything but
the exact empty extension reply, is refused. An unsupported TPM, malformed
reply, nonzero initial PCR, uncertain extension or readback mismatch
refuses kexec on an unencrypted boot; an encrypted one prints the fixed
`TD-BOOT-UNMEASURED` line and boots, a protected release with a method-0
record and, from 8f's upgrade-boot commit, a device-bound one with none
(ENCRYPTION.md "Verified account handoff").
It never resets a PCR or retries an extension. A failed handoff requires a
cold boot; it cannot measure another candidate on the already-used PCR.
The ordinary verified selection and fallback happen before measurement.

This follows TPM2 PCR_Read/PCR_Extend's wire operations in the
[TCG TPM command specification](https://trustedcomputinggroup.org/wp-content/uploads/Trusted-Platform-Module-2.0-Library-Part-3_Commands-V185-RC4_12Dec2025.pdf).
It is a td selector event, not a firmware TCG event log or an attestation.
The QEMU host still supplies the first kernel and selector. Authentication
and measurement of that entry and its policy remain prerequisites
for using PCR 11 in a shipped release policy. Root and the running kernel
remain trusted. This adds no key release, enrollment or authorization for a
changed deployment. Update authorization for approved current/previous
states remains governed by [ENCRYPTION.md](ENCRYPTION.md).

### Storage operation worker

Fixed by [ENCRYPTION.md](ENCRYPTION.md) increment 8e's documentation commit
before its code, the upgrade and its cancellation by 8f's, and protector
management by 8g's. `storage_operation.rs` implements the selector update,
the disk proof's keyslot test, the upgrade's phase 1 and its cancellation,
and protector management's addition, removal, recovery-key removal and
requests; the upgrade boot is the selector's. td-authd refuses every storage
operation in a production build until that document's item 9, and starts a
disk proof's worker only in builds that decode the proof, test builds until
then. The worker compiles td-secret's endpoint (`frames.rs`, whose shared
secret frame `16` it reads into clearing storage) and memory rules
(`memory.rs`) and td-authd's consent descriptions by `#[path]`, admits and
writes the ESP through `td-fat`, and asks a disk security key through
td-fido's `fido_disk::authorize` over root-admitted devices, each HID
session's worker this program's own `fido-worker` verb, started from
`/proc/self/exe` as td-boot's selector starts its own; td-install never
builds or dispatches the desktop verb. Discovery takes the one `td.volume=`
UUID of `/proc/cmdline`, asks `td-boot volume` for its partition and holds
that partition open; the volume's files are read through the running
system's `/run/td-volume`, and `current`'s manifest is read there by the
worker itself and must hash to the ID it serves. It starts no td-boot child
for this: root's kill on a cancellation or timeout reaches the worker alone
(neither td-authd nor td-install has a process-group kill), so an unbounded
child, such as `validate-source` hashing `root.erofs`, would outlive it; and
the update reads no payload but the kernel, which the worker hashes itself.
The ESP is the held partition's disk's first GPT partition, `td-esp` of the
EFI System type, as td's layout writes it. `td-install storage-operation
--uid 1000` is the root worker td-authd's amendment 10 supervises
(td-authd/DESIGN.md, "Login keys and session lock"), the one running-system
process that runs a key-bearing td-luks command or writes the ESP
("Full-system volume consumers"). It reaches the command line only through
`main`, accepts no other operand, and keeps the login worker's startup,
unnamed socketpair, descriptor inventory and bounded framing
(td-secret/DESIGN.md, "Login-key worker"): it requires no active swap and a
zero core-dump soft limit before its baseline, since it may hold a recovery
key, a PIN, a derived passphrase or a volume key, and reads every frame into
clearing storage.

Its frames are the login worker's: it sends `18` its baseline, `10` and
`12` invitations, `14` success and `15` a failure; root sends the
description, `11` and `13` acknowledgements and `16` a secret after a
secret step's acknowledgement, `16` and 4 to 63 bytes as a PIN frame is,
or, after a removal's choose step, `16` and one ASCII digit, the chosen
position. The baseline, sent before any presentation, token or ESP I/O,
is `18`, then:

- the volume byte, from discovery's held partition and td-protector's
  reader over its LUKS2 header: `00` no encrypted td volume, `01`
  device-bound, `02` upgrading (a `td-upgrade` token), `03` protected
  with the TPM-primary shape, `04` protected with the FIDO2-primary
  shape, `05` a header the reader refuses;
- `01` when a `recovery-key` marker names a keyslot, else `00`;
- the fido2 token count, 0 to 4, then each token's role (`01` primary,
  `02` recovery), `01` when it names a keyslot, else `00`, and its
  fingerprint, the first four bytes of SHA-256 over its credential ID,
  in token-number order;
- the count of tpm-pin tokens naming a keyslot, 0 to 2, `01` when a
  lockout token is on the header, else `00`, and the request byte, `00`
  with no `td-request` token or `01` to `03` for its operation
  (`pin-change`, `tpm-clear`, `tpm-reenroll`);
- `00` and the running deployment's 32-byte manifest ID when it is
  `current`, acknowledged and with no pending attempt, or `01` alone.

Root's first frame is a TDCONS02 description: tag 16 for a selector
update, tag 17's review for an upgrade, tag 18 for its cancellation,
tags 13 to 15 at their proof's first step for a disk proof, or tags 19
to 24 at their first step for protector management.
The worker requires it to be what its baseline admits, as root does,
or fails as INTERNAL before any I/O, and repeats it exactly as its
first `10`, a selector update's after the reads below, so the approval's
time holds no I/O.

- **Selector update** (tag 16). The form must be the one the volume byte
  gives and the ID the baseline's. Before its first `10` it reads
  `current`'s manifest from the held volume, requires it to hash to that ID,
  and hashes the deployment's `bzImage` against that manifest (`23`
  otherwise). The manifest names no selector template; the template is
  verified through `root.erofs`, as the selector's commit verifies it: the
  baseline requires the running deployment to be that `current`, whose root
  is the `root.erofs` the deployment initramfs hashed against this manifest
  and bound read-only, so the worker reads the template from that running
  root's `/lib/td-boot/selector-initramfs.cpio`. It prepares the selector in
  memory by `prepare-selector`'s rules with the volume UUID of `td.volume=`
  and, for a protected volume, `etc/td/boot-measurement`, and admits the ESP
  with `td-fat` (`24` otherwise), reclaiming nothing yet. After the
  presentation's `11` it sends `12` only with 120 seconds of its deadline
  left (`08` otherwise); after root's `13`, before any write, it re-reads
  the header, the running deployment's standing and the ESP's `EFI/BOOT`
  sector, failing as `28` if the header changed since the baseline, the
  deployment is no longer an acknowledged `current` with no pending attempt,
  the sector changed since its admission, or td-fat's `fits`, which writes
  nothing, now refuses the pair (the ESP's free clusters changed). Then it
  stages the pair or writes it directly (ENCRYPTION.md, "Selector update")
  and sends `14`. It never opens the volume or reads its key.
- **Upgrade** (tag 17; ENCRYPTION.md "Re-encrypting upgrade", phase 1).
  The volume byte must be `01` and the ID the baseline's. Before its
  first `10` the worker reads the header again through td-protector's
  `read_upgrade` (`20` if it is gone or the reader now refuses it, `28`
  if it is not the header the baseline read) and requires, through
  td-protector's `upgrade::phase_one_fits` with the shape and count,
  that the header the upgrade will grow fits its area and that it is
  converged (`21` otherwise): of kind device-bound, keyslot 0 and
  exactly one other keyslot, which the one device-bound token names,
  every keyslot `luks2`, no reencryption, a data segment identity, and
  no token but that
  device-bound token and pending fido2 tokens a crashed phase 1 left,
  whose removal its commit round begins with; a token of another type is
  refused, since the reencryption would wipe its keyslot. For the TPM
  shape it then opens the TPM, reads its PCRs, requiring a SHA-256 bank,
  and reads `TPM_PT_PERMANENT` without authorization through td-tpm's
  `dictionary_attack`, requiring `lockoutAuthSet` clear (`27` otherwise,
  its detail `01` no TPM or no answer, `02` no SHA-256 bank, `03` the
  lockout authorization set), since a device-bound volume holds no
  lockout token that could prove a set one; this is the upgrade's one
  TPM read, and no TPM write follows. Then it makes the selector
  update's reads and checks (`23`, `24`) for the direct form, preparing
  the selector with `etc/td/boot-measurement`, as ENCRYPTION.md's
  selector update says for the upgrade. After the review's `11` it runs
  each new key's ceremony: its connect step, after whose `11` the worker
  waits within the deadline for exactly one connected key that is not
  the previous ceremony's (`05` for several), then td-fido's
  `fido_disk::create_stepped` over root-admitted devices: creation,
  proof, repeat, identify probe and no-PIN probe, each phase's client
  data hashing that step's own description, which the worker presents
  (`10`) before the key needs it and root admits; the exclusion list is
  the credentials of the operation's earlier keys and of the header's
  fido2 tokens that name a keyslot, none on a converged header: a
  crashed phase 1's pending tokens are on their way out, and excluding
  them would refuse the very key the person runs phase 1 again with. A
  credential ID over 255 bytes is refused before the proof's PIN. The
  derived passphrase is zeroed unused: phase 1 writes no secret, and the
  upgrade boot derives it again. After the last step's `11` it sends
  `12` only with 120 seconds of its deadline left (`08` otherwise);
  after root's `13`, before any write, it re-reads the header, the
  standing and the ESP as the selector update does (`28`), and
  td-protector's `upgrade::phase_one` plans the header commits from the
  re-read view (`28` should it refuse what `phase_one_fits` admitted,
  since the header changed), confirmed against td-luks's own metadata
  (`Plan::confirm`; `28` when they disagree), so the imports land where
  td's copy of the header expects and `from` is the identity td-luks
  also reads (`0f` should that read fail). Its writes, in order, each a
  key-less header commit through td-protector's runner but the
  selector's: the removal of any pending token a crashed phase 1 left;
  the selector in its direct form, so the selector that reads what
  follows understands it; each new key's pending fido2 token (`keyslots`
  empty, the role the shape gives, its credential, salt and public key),
  in ceremony order; and last the `td-upgrade` token with the shape and
  `from`, the re-read header's data segment identity
  (`luks2::segment_digest`). Then it sends `14`. A failure after the
  first write is uncertain to root; what it leaves is a device-bound
  volume, whose pending tokens without a `td-upgrade` token are orphans,
  or one in U0. A device-bound volume whose selector phase 1 wrote boots
  a selector carrying `etc/td/boot-measurement` (ENCRYPTION.md,
  "Verified account handoff", for its rule on a failed measurement).
- **Cancellation** (tag 18). The volume byte must be `02`. Before its
  first `10` it reads the header through `read_upgrade` (`20` if it is
  gone or refused, `28` if it is not the baseline's) and plans
  td-protector's `upgrade::cancel`, which
  admits only U0 (`21` otherwise). After the presentation's `11` it
  sends `12` with 120 seconds left (`08` otherwise); after root's `13`
  it re-reads the header, failing as `28` if it changed, and runs the
  plan, confirmed against td-luks's own metadata (`Plan::confirm`;
  `28` when they disagree): the `td-upgrade` token's removal, then each
  pending token's, each a key-less header commit. Then it sends `14`.
- **Protector management** (tags 19 to 24; ENCRYPTION.md "Protector
  management"). The volume byte must be `03` or `04`, and root's first
  description exactly the one its baseline gives (consent's
  `Protected::admits`), or the worker fails as INTERNAL before any I/O.
  - *Before the first `10`.* A request checks, over the header area
    `test_area` reads, that every keyslot has a cost
    (`TestArea::all_costed`; `0f` otherwise), since its dump below
    tries every keyslot.
  - *The choose step.* A removal first presents the choose step and
    takes its `16`, the chosen position. That position must name a
    removable token by the worker's own baseline, and the next
    description must carry it (INTERNAL otherwise).
  - *The proof.* A disk proof's, below, with its kinds, except that:
    - a disk key is identified among the FIDO2 protectors, the fido2
      tokens naming a keyslot, so an orphan's key is `04`;
    - a removal's disk key may not be the chosen token's (`04` detail
      `01`, before its PIN), so the authorizing token remains;
    - the proof's secret is kept, zeroed on drop, for the writes;
    - a request keeps the proof's header copy past the keyslot test,
      for its dump.
  - *An addition's new key.* Its ceremony runs as the upgrade's does,
    through `fido_disk::create_stepped` for the chosen role, with every
    fido2 token's credential excluded (`11`). Its connect step waits for
    one key that is not the one the proof used, so the authorizing key
    must be removed first. The derived passphrase is kept in clearing
    storage for its keyslot.
  - *A request's volume key.* The worker reads it from that header copy
    through the runner's existing dump shape, the selector's
    (ENCRYPTION.md "Selector release"): `td-luks volume-key COPY FILE`,
    with the proof's secret on standard input.
    - It runs bounded, as the keyslot test does, within the proof's 120
      seconds and before `12`.
    - FILE is a new name in the worker's mode-0700 directory under
      `/run`, beside the copy, created exclusively at mode 0400.
    - td-luks tries bound candidate keyslots, not just the tested one;
      the check before the first `10` bounds every admitted slot's cost.
    - The worker reads exactly 64 bytes, the header's volume-key size,
      into clearing storage, then unlinks FILE and the copy whether or
      not the read succeeded. The test having passed, the dump's
      deadline is `08`, as the test's, and any other failure `0f`. A
      killed worker's FILE goes with its directory at the next worker's
      start.
    - The copied area's keyslots must each have a cost too, since the
      header may have changed between the check before the first `10`
      and the copy (`0f` otherwise).
    - The key reaches no frame, string, error or log, and is zeroed
      once the tag is computed or at any end.
  - *The commit round.* After the last step's `11` the worker sends
    `12` only with 120 seconds of its deadline left (`08` otherwise).
    After root's `13`, before any write, it re-reads the header (`28`
    if it is not the baseline's) and plans with td-protector's `manage`
    planners from the re-read view. The plan is confirmed against
    td-luks's own metadata (`Plan::confirm`); `28` when they
    disagree, or when a planner refuses what the baseline admitted,
    since the header changed. Its writes, through td-protector's
    runner, are then:

    | Operation | Writes, in order |
    | --- | --- |
    | Addition | the new keyslot, added under the proof's secret with the new key's derived passphrase; its fido2 token, naming it; the new keyslot's test on a fresh copy (`0e` should it refuse) |
    | Removal | the chosen token's keyslot, killed under the proof's secret; then the token |
    | Recovery-key removal | the marker's keyslot, killed under the disk key's passphrase; then the marker |
    | Request | a pending `td-request` token's removal; then the new request, key-less, with a 32-byte nonce from `/dev/random` and its tag from td-protector's `request::tag` under the dumped key over the re-read header (ENCRYPTION.md "Request tag") |

    Then it sends `14`. A failure after the first write is uncertain to
    root; what it leaves is ENCRYPTION.md's rule for a cut.
- **Disk proof** (tags 13 to 15). Root's first description must be the
  proof's first step for the worker's account, one its own baseline
  offers (consent's `proof_start`: a `recovery-key` marker, or a fido2
  token), or the worker fails as INTERNAL before presenting it; it then
  presents that step, waiting for its approving Enter where the step
  discloses, and narrows its deadline to the description's ceiling.
  With the recovery key: after the recovery-key step's `11` it takes
  one `16`, parses it with td-protector's recovery-key codec (`25` on a
  refusal, before any td-luks), re-reads the header, failing as `28`
  if it changed since the baseline (or `20` if it is gone or the reader
  now refuses it), and tests the keyslot the marker names, the key on
  standard input (`26` when it refuses, td-luks's wrong-passphrase
  exit; any other failure `0f`). The entry frame, the codec's key and
  its passphrase are each zeroed on drop, and the passphrase reaches
  td-luks only through its standard-input pipe: no copy enters a
  string, an error or a log. With a disk key: it identifies the one
  connected key among the header's fido2 tokens whose public key td-fido
  admits, as the selector's release does, skipping any other, presents
  the disk
  authorize step with that token's fingerprint and the key's PIN
  retries, takes the PIN, runs the authorize-phase assertion whose
  client data is the disk domain's over that exact step (ENCRYPTION.md,
  "FIDO2 protectors"; the identify's binds the identify step),
  verifies its signature under the token's public key, derives the
  passphrase, re-reads the header as the recovery key does (`28` or
  `20`) and tests it on the keyslot the token names (`26` when it
  refuses, or when the token names none), the passphrase on standard
  input. The key's sessions live one transport lifetime at most, inside
  the operation's ceiling.

  A keyslot test uses a private snapshot of the admitted header area.
  td-luks tests are read-only, including with stale or damaged secondary
  metadata; the snapshot also keeps the bounded proof tied to its view. The node `td-boot
  volume` named must still be the held partition (the same `st_rdev`).
  td-protector's `test_area` then reads the used copy through the held
  descriptor, which must equal the presented header (`28` otherwise),
  and the tested keyslot's cost, whose bounds refuse Argon2 and any
  PBKDF2 count over its ceiling (`0f`, before td-luks starts). The
  worker copies exactly that area, both copies and the keyslots area,
  into a file of its own, mode 0600, in a directory it makes for it
  under `/run`, mode 0700, and reads the copy again, which must equal
  what it read (`28` otherwise). `td-luks test N DEVICE COPY` reads only the copy, which is removed afterwards, after its
  volume-key dump for a request. A worker killed
  meanwhile leaves its copy behind, so each worker's start removes the
  copies dead workers left: real directories root owns, named exactly
  for a pid no live process has, a live worker's left alone. The
  child runs through the runner's `run_bounded`: it is killed and
  reaped at the operation's deadline (`08`) or as soon as root's end of
  the socket closes, which a second handle on it reads for every 10 ms,
  so a cancellation, which closes root's end, ends it. Root gives a
  proof's storage worker, and protector management's, one second after
  closing its channel before it kills it (td-authd/DESIGN.md amendment
  10), so the copy is removed; until item 9 only a test build's root
  starts this path.

  Success sends `14` with no commit round, and
  the worker exits; the secret, output and passphrase are zeroed first.
  A `28` here, like a selector update's, precedes any write, and the
  worker exits successfully after it.

Its failure kinds are td-secret's login worker's where they mean the
same (`01` to `08` for a disk key's PIN, presence, devices and time,
`04` a key that is no header token's, for protector management no FIDO2
protector's, detail `01` a removal's chosen token's, `06` a new key td's
creation refuses, with its detail, `06` detail `06` among them for a
probe that does not select the new credential, `11` a new key already
holding one of the excluded credentials, `0e` an uncertain write, `0f`,
a repeat that does not reproduce the proof and a credential ID over 255
bytes included, and `10`), and its own:

| Byte | Kind |
| --- | --- |
| `20` | VOLUME: no encrypted td volume, or a header td's reader refuses |
| `21` | STATE: a volume state the operation does not take, such as an unencrypted or upgrading volume for a selector update, a header not converged for an upgrade, one past U0 for a cancellation, or a proof the header has no protector for; for protector management, root's details apply (td-authd/DESIGN.md amendment 10): `01` a role the shape does not take or has no room for, `02` a removal below the shape's minimums, `03` a request without the protector it acts on, and `00` otherwise |
| `22` | NOT CURRENT: the running deployment is not an acknowledged `current` with no pending attempt |
| `23` | DEPLOYMENT: the kernel or the template does not match `current`'s manifest |
| `24` | ESP: the ESP is not td's layout or lacks the space ("Selector update") |
| `25` | KEY MISTYPED: the recovery-key codec refused the entry |
| `26` | WRONG KEY: the keyslot refused the recovery key or the derived passphrase |
| `27` | TPM: the TPM shape needs a TPM with a SHA-256 bank whose `lockoutAuthSet` is clear; detail `01` no TPM, `02` no SHA-256 bank, `03` lockout authorization set |
| `28` | CHANGED: the header, the deployment's standing or the ESP changed since it was read; sent before any write, so definite |

After root's `13` any outcome but `14` with a successful exit is
uncertain to root, as a login write's is, with one exception: `28`,
which the worker sends only from its checks before its first write, is
definite, and root forwards it as `91 0d`. Host tests drive each
form over a header and an ESP the test builds, every kind at its point,
the commit margin and the change check, including space taken after the
presentation, and the production host's standing, template and
verified kernel over a scratch root. The upgrade's drive phase 1 in
both shapes, with and without new keys and over a crashed phase 1's
orphans, through a scripted `Keys` whose creation presents each step:
the review and each ceremony's steps in order, td-fido's hashes bound
to what was presented, each key's exclusion list, every read before the
review, and the commits in order around the selector's direct write;
and every kind at its point: `21` for a header not converged, each
`27` detail, `20` and `28` at the view, the selector's reads, a
description the baseline does not admit, a ceremony's `11`, `06` detail
`06`, `01`, `0f` and `05`, a change after `13`, metadata that disagrees,
and a failed commit. The cancellation's drive its removals in order, a
header past U0, a volume that is not upgrading and a change after
`13`. The production host's TPM read composes td-protector's
`release_policy` and td-tpm's `dictionary_attack`, each tested in its
crate. The disk proof's drive each
proof to success and to every kind at its point: the codec's refusal,
a keyslot that refuses, a header changed, gone or refused before the
test, a token naming no keyslot, a description the baseline does not
offer, a token whose point td-fido refuses, skipped, and the disk key's
refusals and statuses through a scripted `Keys`; td-fido's own tests
drive `authorize` against its virtual authenticator. A source test holds
the secret's path free of copies. The production host's keyslot test
runs a scripted td-luks over a scratch partition whose second copy is
stale: the repair it makes lands in the copy and the partition is
unchanged; a header other than the presented one, an Argon2 keyslot and
a node that is not the held one run nothing; and a stalled td-luks
is killed and reaped at the deadline and when root hangs up. A start's
sweep removes only dead workers' copies root owns. td-protector's
`keyslot_test`, run as `header_fit` is, holds td-luks itself to the copy: over a td-formatted volume whose second copy
is stale or damaged, the exact invocation leaves the volume's SHA-256
unchanged for a right key and a wrong one (exit 2, `26`), and the copy is unchanged too.

Protector management's drive each operation over protected headers the
test builds, in both shapes, through a scripted `Keys` and td-luks:
each proof and its kinds, a removal's choose step and its digit, a
removal's disk key that is the chosen token's (`04` detail `01`), an
addition's ceremony with every credential excluded, a description the
baseline does not give, a change after `13`, metadata that disagrees and
a cut at each write; and each request's dump, a header holding a keyslot
without a cost (`0f` before any step), a dump that fails or whose file
is not 64 bytes (`0f`), FILE and the copy removed on every path, a
pending request replaced, and the imported tag recomputed through
td-protector's `request::tag`, which `tests/token_vectors.py`'s vectors
pin. td-protector's `keyslot_test` also runs the exact dump against the
built td-luks over a header copy of a volume it formats: the file
reproduces the key the volume itself dumps, through either keyslot, and
a name that exists is refused.

## 6. No deployment signatures

td does not sign deployments, and nothing on the boot, install or update
path authenticates one. A deployment is its manifest and three payloads;
every reader checks the payloads against the manifest (D2) and the id is
the manifest's hash (D3). What decides which deployment a machine runs is
who can write its volume, and for a person's update, rollback or install,
their consent at secure attention or in the live installer.

A per-machine signing key was tried and removed because it guarded
nothing. A machine must hold its own private key to sign its own
updates, which td builds locally from git (AGENTS.md principles 5 and 6),
so the key lives where the deployments do:

| Who | Unencrypted volume | Device-bound encrypted volume |
|---|---|---|
| Offline, writes the volume | reads the key and signs | cannot write the volume |
| Offline, replaces the ESP selector and its key | not stopped | the TPM seal over PCR 4 and PCR 9 refuses release |
| Root on the running system | holds the key | holds the key |

The signature therefore stopped nothing that disk encryption, the TPM
seal and the manifest hashes do not, and a key carried in the selector
it vouched for, or on install media beside the deployment it signed,
vouched only for itself. So:

- Offline protection is device-bound encryption and its TPM seal over
  the selector (ENCRYPTION.md). An unencrypted installation has no tamper
  protection: anyone who can write its disk chooses what it boots.
- Root on a running system can write the volume and so choose what boots
  next; consent governs what a person's session asks root to do, not
  what a compromised root can do.
- Verified boot, if it becomes a goal, needs both halves a local key
  cannot give: firmware that refuses an unsigned selector (Secure Boot
  with td's own keys enrolled, the kernel and selector built into one
  signed EFI image, and the firmware locked), and a deployment-signing
  key the disk does not hold, such as a FIDO2 key exercised at the
  consent prompt. That is a separate design.
- Installations made before this change keep a selector that requires a
  signature and so refuses an unsigned update, falling back to
  `previous`. They are reinstalled from current media; no update path
  replaces their selector, and a staged selector update cannot either:
  a key-carrying selector's commit rebuilds its appendix with that key,
  so a staged keyless `INITRD.NEW` does not match and is deleted
  (ENCRYPTION.md "Selector update").

Volumes may still hold `td/trusted.pub` and a `manifest.sig` beside a
published manifest from that time. td never reads, requires or removes
them. A source directory holding a `manifest.sig` is accepted; the file
is ignored and never published.

### Validating a source

`td-boot validate-source DEPLOYMENT` is a read-only preflight for an
absolute deployment directory. It uses the publisher's bundle verifier:
read the bounded manifest, parse it, then open and stream-hash `bzImage`,
`initramfs.cpio` and `root.erofs`. A source may spell the kernel
`bzimage`, as a mounted install medium does (MEDIA.md); a directory
holding both spellings as different files refuses. Symlink and
special-file payloads refuse. Stdout is exactly the manifest's deployment
ID and a newline after every payload succeeds; validation failure emits
no ID. The operation creates no staging tree, lock, selector or mount and
has no destination argument.

Validation keeps the opened payload files for the duration of the command,
then closes them. Its result is not a snapshot or authorization for a later
write: the caller must keep source bytes and their namespace stable. The
native ISO fixture normally supplies read-only media under a fixed test
topology; its writable-source claim test retains read-only payload mounts
and never publishes. Volume publication still checks every copied payload.
Preflight does not validate the separate firmware selector, its UUID,
scratch capacity, destination eligibility or destructive consent.
Those remain installation-coordinator responsibilities.
`td-install observe-source-plan` holds the reviewed destination's exclusive
claim while this preflight compares the canonical manifest ID with the
bounded plan. Its report names the disk identity checked at claim time and
again after hashing, and the deployment validated while that claim
remains open. After source
validation, it rechecks the held descriptor's capacity and compares a
fresh, bracketed sysfs inventory with the plan before reporting success.
The descriptor's device number is reasserted but cannot change for that
opened inode. Removal or a changed disk sequence observed at that point
refuses; unrelated inventory changes only matter if they occur between the
two recheck scans. The claim itself does not prevent physical hotplug
during hashing, and an intervening change restored before recheck is
undetectable.
The child closes
its source descriptors and the destination claim closes before return, so
the report cannot authorize a later formatting step. The service must keep
its own source and destination lifetimes through consent and writes.

`td-boot install` publishes exactly `bzImage`, `initramfs.cpio`,
`root.erofs` and `manifest`. When a directory with the bundle's id is
already published, it is read as a published deployment: if its manifest
hashes to that id and its payloads match, publication is a no-op;
otherwise it refuses ("existing deployment … does not verify as …").

## 7. Engine sources compiled into target binaries

`builder/src/affected.rs` routes a changed file to the checks that can catch
a break in it. Most `engine/src/*` sources reach only the two control-plane
bins. A few are `#[path]`-included into **target** binaries as well, and
which ones is written down in `TARGET_INCLUDED_ENGINE_SOURCES`:

| source | target consumer |
|---|---|
| `principals.rs` | td-firstboot identity parsing and reservations |
| `sha256.rs` | td-boot, td-update, td-install live ESP kernel check and its td-tpm rlib, td-authd, td-secret, td-firstboot, td-compositor corpus verifier, td-ui terminal corpus |
| `crc32.rs` | td-install (via gpt) |
| `gpt.rs` | td-install |
| `cpio.rs` | td-install selector identity preparation, td-boot's selector appendix |
| `fat.rs` | td-install |

Each entry stores the full consumer list the router prints in its note; the
column above shows only the target half that distinguishes them.

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

What being in the table does NOT mean is that some gate builds the
consumer; the note makes a consumer visible at the routing decision, and
building it belongs to that consumer.

A target consumer whose source is *missing* from the table is the failure
that matters: a target binary whose engine sources nothing checks.

### The other shared source: `td-boot/src/protocol.rs`

Not an engine source, and so not in the table, but the same shape: it is
`#[path]`-included by `td-install`, `recipes/src/recipes/system-x86-64.rs`
and `recipes/src/bin/td_recipe_eval/checks/qemu_boot.rs`, and routes with
td-boot's other sources to every gate that compiles one of them.

### Shared hostname grammar

`td-firstboot/src/hostname.rs` is included by `td-install` for option
validation and by the system recipe's tests to validate its default. The
target recipes stage that same dependency-free source. Crate-wide reader
discovery tracks the include, including the conservative firstboot reader
cluster; the installer does not carry a separately maintained parser.

## 8. Oracles

The existing QEMU system oracle boots with `-kernel` and stays. It is the
fast path and it tests the deployment machinery, not the firmware.

A **second** oracle boots the real thing: `-M q35` with an OVMF pflash
code/vars pair, the vars a writable per-run copy. It needs no new host
dependency — the qemu that `find_qemu()` already locates ships
`edk2-x86_64-code.fd` and `edk2-i386-vars.fd` under its own `share/qemu` — so
the firmware is located relative to that binary and its absence is a loud
failure, as `find_qemu`'s is. This bundling is host-package-specific;
matching distro OVMF paths are also tried;
`TD_QEMU_EFI_CODE` and `TD_QEMU_EFI_VARS` can select an explicit pair
(both or neither). The pair must have Secure Boot disabled and emit the
EDK II BdsDxe disk-lookup diagnostic used by the negative control. The tool
checks bounded regular files; it cannot infer those firmware policies from
the bytes. Firmware is solely a host test input.

The initial `td-recipe-eval qemu-boot-uefi [linux-x86-64]` increment
proves the firmware entry and initrd handoff with the recipe's tiny
td-sh initramfs. It builds and runs the source-built `td-install` on
an exclusively created private 6 GiB sparse disk, populating the ESP
from the kernel recipe. It starts q35, on KVM alone unless
`TD_QEMU_ACCEL=tcg` (`checks/accel.rs`), with cold per-run variables
and the disk writable, and requires the real userspace marker without
`-kernel`, `-initrd` or `-append`. The same disk must pass a second cold
boot with another fresh variables copy and the disk read-only. A second
disk laid out with an empty ESP must remain without that marker for the
bounded negative observation and emit BdsDxe's `Not Found` refusal for
the `UEFI Misc Device` boot attempt. The positive leg runs first to
reject a broken firmware pair promptly. The pair-or-none layout
interface makes the negative control an empty ESP, rather than an ESP
containing only INITRD. No disk destination is accepted from the
operator. Only the installed disk is attached. This proves the
installer's EFI file path with a tiny diagnostic initramfs; it does not
format Btrfs, run installation inside a guest, or boot a system
deployment. The full deployment oracle described below remains required.

`td-recipe-eval qemu-install` adds a native guest installation fixture,
[td-install-qemu-test](../td-install-qemu-test/DESIGN.md). Its source-built
PID 1 runs td-install format, which publishes the deployment through
a loop over its own claim, on an exclusively created QEMU virtio or AHCI
target. The same private ISO is exercised as optical and
USB media. After installation, the host detaches media and cold-boots only
the destination, requiring verified selector kexec, a read from the
installed EROFS payload, persistent Btrfs state across two boots, and
successful deployment acknowledgement. Before layout, the live fixture
validates the source's manifest and all three payloads. A negative ISO
retains the valid manifest but substitutes corrupt `root.erofs` bytes.
Both optical and USB boots must report the payload hash mismatch without
an installation-success marker. The host seeds each refusal target with
nonzero canaries and compares its complete byte length and SHA-256
before and after QEMU exits. A refusal after changing the GPT, ESP,
volume or sparse gaps fails the oracle. Unit tests exercise all
three payloads, including missing files and symlinks. This is source
preflight, not complete device, space or firmware-selector admission.

The fixture accepts no operator destination and is absent from system and
installer profiles. Its serial-based installation target identification is
diagnostic scaffolding, not production consent. Installed boots exercise
the read-only resolver above, including changed disk ordering and duplicate
identity refusal before selection. Linux mounts its ISO payload files
read-only before installation; the live initramfs holds tools. Machine
settings and the production installer are INSTALLER.md's.

The small fixture also interrupts the real publisher while its staged kernel
is nonempty and incomplete. It kills/reaps its own child, proves there are
no current/previous selectors, syncs the partial data, then reports byte
counts so the host can cut VM power. A detached firmware boot must refuse
both absent selectors without selecting a deployment. Explicit whole-disk
reinstallation from the normal ISO must restore a verified boot and
fresh state. Both media attachments exercise this sequence. The observer
has no production pause hook; this is a controlled publisher interruption
followed by a VM power cut, not a model of every storage power-loss window.
The same oracle boots valid media against undersized and read-only private
targets. Specific layout refusals and whole-target digests establish that
neither case changed target bytes or reached partition refresh/publication.
Both media attachments run these cases.
QEMU block-backend write protection is not exclusive device admission.
Additional cases exercise real scratch exhaustion before formatting writes
and refusal to format mounted, writable USB source media. The writable
source test requires an exclusive open to succeed after source unmounts
and preserves both complete images. The
[fixture design](../td-install-qemu-test/DESIGN.md) owns the complete boot
roster and the exact evidence for these cases.

The successful small installation matrix covers both 512-byte and
4096-byte logical sectors. The guest reports the target geometry read
from sysfs; the host requires it to match the requested QEMU geometry.
The installed target and duplicate/reordered companions retain that
geometry through firmware boots. This tests GPT/FAT32 layout and
detached verified boots on 4Kn media; it is not a physical-device or
512e performance claim. Refusal and interruption cases remain at 512
bytes. Eight positive boots exercise AHCI targets at 512-byte geometry
through both media attachments, including duplicate identity refusal and
reordered detached boots on that same bus. The small matrix includes
sixteen NVMe boots at 512-byte and 4Kn geometry through
both media attachments. Each NVMe controller has one namespace; the
second detached boot must move from nvme0n1p2 to nvme1n1p2. The kernel
pins its PCI NVMe driver and core built-in, enables message-signaled
interrupts and disables multipath. MSI/MSI-X is selected for the whole PCI
profile; NVMe can also fall back to INTx. The virtio/AHCI desktop matrix
therefore validates the kernel change as well. This tests SATA and NVMe paths
under QEMU, not physical-device or ThinkPad compatibility.

`td-recipe-eval qemu-install-system [system-x86-64]` uses the same live
fixture to install the built system deployment and its verified selector.
The same ISO installs through optical and USB media onto both virtio and
AHCI targets with 512-byte sectors. Each of the four installations cold-
boots twice with the ISO detached; the second boot places a blank disk
on the same bus before the installed disk. Target and decoy retain their
bus throughout; the host requires vda2/vdb2 for virtio and sda2/sdb2 for
AHCI. Each boot must bind the planned UUID before selecting the exact
deployment, reach the greeter, prove immutable root/configuration
and writable owned state, complete compositor modesetting and page
flips, and acknowledge deployment health. First-boot identity must
become stable on the second boot, with the same SSH host-key
fingerprint, while all four independently installed machines must have
pairwise different fingerprints. Missing bookkeeping and exhausted
attempts refuse. The host terminates QEMU immediately after the health
marker, then starts a new firmware process for the next boot. This
exercises durability before acknowledgement across abrupt termination;
it does not exercise the installed session's orderly shutdown path.

After both cold boots validate, remove that installation's private target
and decoy; only bounded captured identity evidence remains for later
comparisons. Removal errors fail the oracle. The shared ISO and source
stay until the matrix ends, and the scratch guard also cleans up on error.

All twelve boots run with 2 GiB RAM and networking disabled. Private target
capacity is at least 6 GiB, or the payload size plus 2 GiB and partition
headroom rounded up to the partition alignment, whichever is larger. This
is diagnostic sizing, not admission of an operator-owned disk. The default
per-boot deadline is 900 seconds. The host prints each installation and
cold boot duration from its QEMU runner. A positive `TD_QEMU_BOOT_TIMEOUT_SECS`
overrides it. The diagnostic accepts no destination operand and does not
enter the shipped closure.
It retains the stock system's account defaults: username/PIN selection,
production consent and hardware support remain separate milestones.

`td-recipe-eval qemu-install-encrypted --tpm /absolute/path/to/swtpm` is
"Device-bound formatting"'s oracle, outside the integration tier
(ENCRYPTION.md "Acceptance evidence"); without `--tpm` it is an
unprovisioned host gap (exit 69). It builds what `qemu-install-system`
builds and boots the same live fixture through optical firmware media
onto a fresh 512-byte virtio target, each
leg from its own ISO and firmware variables, the emulated TPM a tpm-tis
device on a fresh swtpm state behind a private socket. A no-TPM leg
requires an unencrypted review, withdrawn with the whole target
unchanged, and two legs with the TPM and no display device, or no
keyboard, an unencrypted review installed as GPT and a Btrfs volume with
no LUKS2 header (ENCRYPTION.md "Activation"). The installed
leg's and the cut leg's targets start with a valid GPT the host wrote
into the two table ranges alone. The installed leg drives the
device-bound service through the recovery-key phase as both of its
peers, then opens the volume with the key it sent; the guest ends
each encrypted leg with a bare `TD-INSTALL-ENCRYPTED-END` line, on which
the host stops, so the records before it are whole. The host then reads
the image: the table through td-engine's GPT reader (both copies, CRCs
and bounds), the ESP of the fixed layout and the volume after it on
whole 4 KiB encryption sectors, both LUKS2 header copies by
their own parse (magic, version, 16 KiB size, offsets, SHA-256
checksums, UUID, label, keyslots 0 and 1 at PBKDF2-SHA256/1000 over a
512-bit key, one first-boot td-protector token on keyslot 1, the
AES-XTS segment at 16 MiB in 4 KiB sectors, the layout sizes), and the
data segment: written where Btrfs keeps its first two superblocks, with
no Btrfs magic, hostname, time zone or manifest header at any offset.
The data segment starts zeroed, so this shows the filesystem only inside
the mapping and claims no erasure. A cut leg powers the guest off in the
phase after the key was sent: the host finds the seeded table's ranges
zero and the header with both keyslots. Every console the host saves or
prints has key-shaped text redacted, after the check that refuses it.
This oracle does not boot the installed system; `qemu-boot-encrypted`
does. td-install-qemu-test/DESIGN.md "Device-bound installation" owns
the guest's side.

`td-recipe-eval qemu-boot-encrypted --tpm /absolute/path/to/swtpm` is
selector release's oracle (ENCRYPTION.md "Acceptance evidence", increment
6). It sits outside the integration tier, and without `--tpm` it is an
unprovisioned host gap (exit 69). It installs as the installed leg above
does, from the `install-encrypted-key` phase, which writes the recovery
key to a virtio serial port backed by a mode-0600 host file. The host
reads that file once and removes it. It then boots the installed disk,
or a copy of it, through the same firmware code (ENCRYPTION.md names
the legs that change the TPM state or the attachments), under
`TD_QEMU_BOOT_TIMEOUT_SECS` (1800 seconds by default) for each boot. The
legs, in order:

1. first boot;
2. second boot;
3. four power cuts on transition commits, each on a copy of the disk
   as installed, a cut that lands past its own commit tried again;
4. the live medium on the installed machine;
5. constructed header states;
6. a changed selector image, initramfs or load option, and a fresh TPM,
   each on a copy of the converged disk. Each reaches the recovery flow,
   where the host types the key at its prompts over the serial console.
   Three of them confirm a reseal, which the next boot releases;
7. a final inspection for the volume and recovery keys.

The guest legs that need the recovery key (`encrypted-headers` and
`encrypted-inspect`) get it as an initramfs file. The host deletes their
media when each boot ends. Each disk copy is searched by
`encrypted-inspect` before it is deleted. Every boot's console is
captured whole to a private file and scanned as raw bytes.

ENCRYPTION.md increment 7 changed both oracles ("Acceptance evidence"):
the guest's service runs with no storage operand, for there is none, so
its no-TPM leg expects an unencrypted review rather than a refusal, and
legs without a display device or without a keyboard expect one too;
`qemu-boot-encrypted` gained the default wizard's leg, first, on its own
machine, and its changed-initramfs leg answers recovery on the VT
through QMP key events with nothing typed on the serial line.

The negative controls are payloads that do not match their manifests,
as above: a verifier that returns success passes none of them.

Both halves of the disk are already validated against independent
implementations: `sfdisk --verify` and `fsck.vfat` read what `gpt.rs` and
`fat.rs` write, and EDK II under OVMF has parsed the GPT, mounted the ESP,
and read a file back byte-exactly. Those checks stay; the oracle is what
makes them a *boot* rather than a parse.

## 9. What this path does not do

Recorded so it is not re-litigated, and so nothing here is mistaken for a
stronger claim than it is:

- **No Secure Boot and no deployment authenticity.** Nothing
  authenticates the selector or a deployment (§6); D2's payload check is
  integrity, not authenticity. Chaining to the platform's own trust root
  (shim, MOK, a signed PE) is a separate trust policy increment with its
  own key management. ENCRYPTION.md increment 8 plans it as an optional
  increment: a signed image holding the selector's initramfs, and a
  TPM-held db key ("Firmware authentication").
- **No measured boot / TPM.** Nothing is sealed to PCRs.
- **No ESP redundancy.** One ESP. A machine whose ESP is destroyed is
  recovered by reinstalling it, not by a second copy.
- **No A/B partition scheme.** Deployments are directories on one volume with
  `current`/`previous` selectors, which is already transactional. A/B
  partitioning solves a problem td does not have.
- **No lost-laptop encryption.** [ENCRYPTION.md](ENCRYPTION.md) specifies the
  device-bound default, current on machines with a usable TPM 2.0 and a
  keyboard console, which protects a disk read away from its machine but
  not a lost one, and the lost-laptop tier, authentication policy and
  activation gates, which remain targets.

## 10. Sequence

Ordered by dependency, not by size. Each is one landing with its own tests.

1. **This file.** Docs-only.
2. **The `affected.rs` routing table** (§7). Mechanism only; no behaviour
   change, and the entries land with it.
3. **Host-side signing** — withdrawn (§6).
4. **Publishing a signature** — withdrawn (§6).
5. **The verifier in td-boot** — withdrawn (§6).
6. **Fail-closed signature verification** — withdrawn (§6). What remains
   of it is the engine's newc cpio writer (`engine/src/cpio.rs`), which
   the selector appendix uses, and `run_boot` re-verifying the chosen
   deployment under the mount whose handles go to kexec.
7. **`td-install`**, a standalone crate outside the workspace (D9): GPT +
   FAT32 ESP + Btrfs volume onto a device or a regular file, sharing the
   GPT, FAT, CPIO, checksum, boot protocol, real-file and hostname
   implementations through `#[path]` includes, and delegating publication to
   td-boot (D1). Carries the `mkfs.btrfs` build-time binding (D7).
   Registering a new crate no longer has touch points:
   `builder/src/affected.rs` discovers every `td-*/Cargo.toml` at the repo
   root and derives the lock roster, the routes and the cargo commands, and
   the cargo-test gate (`builder/src/gate_defs/325-cargo-test.rs`) asks it
   for the same list. Commit the crate's `Cargo.lock` and declare any
   non-default gating in its own `[package.metadata.td-gate]`; the workspace
   `exclude` list is gone.

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
   through a mount; its own paragraphs are below the 7b ones.

   7b is itself two commits, and the RECIPE is the first of them. D7's binding
   is a build-time complaint about a missing program, so something has to
   build td-install with its formatter (now `td-btrfs`) declared beside it —
   and today nothing builds td-install at all. There is also no install image
   to pack it into, so the roster check `td-boot/src/protocol.rs` already
   models for td-init applets has no image to read; what it reads instead is
   td-install's own declared inputs. The recipe is worth landing on its own
   for the reason item 5 was split off: a `#[path]`-including crate that
   only ever compiles on the host has not been shown to compile for the
   target, and the recipe is also
   the only place a test can EXEC `mkfs.btrfs`, since no host is required to
   have one. `recipe-checks` joins td-install's route with that recipe.

   **The formatter itself uses no partition device.** `mkfs.btrfs` writes a scratch image sized to the volume
   partition, and td-install copies that image into the partition through the
   whole-disk descriptor it already holds — which is exactly how it writes the
   ESP today. A kernel reread is not needed for formatting, because it does
   not answer the question for BOTH destinations:
   a regular file has no partition device to rescan, so the scratch-image path
   has to exist regardless, and once it exists the ioctl buys a second code
   path for the one destination the tests cannot reach. D9 settles it — an
   installer whose tested path and shipped path differ is an installer tested
   somewhere other than where it runs — and D8 survived intact, until the
   loop publication below gave it one recorded surface.
   A live coordinator can separately refresh partitions through td-init, as
   specified in §5, before direct publication; the formatter remains one path.

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
   labels. mkfs.btrfs zeroes both ends of its device, so on a fresh image both
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
   including the stdout of EVERY child this path runs — `mkfs.btrfs`, whose
   report names the new filesystem, and since 7c `td-boot publish`, whose is
   the deployment id. Both are things a caller might want and neither is a byte
   offset, so the rule is about the CHANNEL rather than about how interesting
   the bytes are.

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
   it. That loop was later adopted for PUBLICATION, not formatting: a
   process holding the disk's exclusive claim cannot mount a partition of
   it, so §5 "Publishing through a loop over the claim" binds one over the
   held descriptor with `LOOP_CONFIGURE`, and `td-install` joined the roster
   for that step alone (UNSAFE.md §21). The formatter still writes no
   partition device.

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
   the sparse copy is about to write. Nothing mounts, nothing loops (the
   later loop publication is a separate route), no partition device is
   needed, `UNSAFE.md` is untouched, and the oracle
   exercises the shipped code path rather than a cousin of it — which is D9,
   and the same argument that settled the `BLKRRPART` question above. It is
   also verifiable offline, which is what makes it a check rather than a
   hope: `td-btrfs list` lists the directory entries of an unmounted image,
   so the recipe can assert `td/deployments/<id>` and the selector are really
   in the filesystem on the destination. Measured on a real image before this
   was written down — including that `--rootdir` preserves a SYMLINK as one
   (listed `l … -> TARGET`), which the selector is and without which the
   assertion would be about a file of the same name.

   That recipe check is 7c's THIRD commit, and it is what makes the first two
   more than a claim: they substitute a shell script for `td-boot` on both
   sides, so a renamed verb or a reordered argument would leave every cargo
   test green and break every install. Here both programs are the ones that
   ship, out of `/td/store`, and the bundle's payloads are verified, so one
   that does not match its manifest is a failed build. Its negative pass
   publishes a tampered kernel and requires td-boot's `bzImage hash
   mismatch` with no selector left on the volume.

   The FIXTURE is the payload-matched set td-boot's own tests carry, shared
   from `td-boot/src/fixture.rs` by `#[path]`, the way `protocol.rs` already
   is. The recipe COMPUTES the manifest from those payloads rather than
   copying one, so a drifted payload changes the deployment id instead of
   leaving a fixture nobody rechecked.

   Two things about the check are worth writing down because they were
   measured rather than assumed. `--rootdir` preserves a SYMLINK as one, which
   the selector has to be — a regular file of the same name boots nothing.
   And the volume holds TWO symlinks, `current` and `previous`, each listed
   twice as a DIR_ITEM and a DIR_INDEX, so a `case` glob asking for `type
   SYMLINK` before `name: current` can be satisfied by `previous`'s entry. The
   exact test is therefore done on the STAGING TREE, which is an ordinary
   directory while the check runs: `-L`, plus the link TARGET read back, which
   is the only assertion that ties the selector to this deployment rather than
   to any deployment. The image is then asked only that the entries reached
   it, the deployment id being conclusive on its own since a 64-character hex
   name appears nowhere else in a filesystem tree.

   The deployment source has to exist when `volume` runs, since the publish
   happens before the filesystem does — so it is an argument to that verb
   rather than a later step, and an install with no bundle to publish is
   the same command without it. Two arguments or none, never one: the
   publish needs a writer and a bundle. And a bundle published this way is
   subject to the copy's ordering above: it is inside the image, so it
   lands before the superblock does, and an interrupted install leaves no
   filesystem rather than a filesystem with a half-written deployment in it.
8. **The EFI-stub kernel** (§5): the EFI/EFI_STUB configuration and realized
   PE format checks are present. The pinned tree requires ACPI, already
   enabled for TPM discovery. Signed selector packaging belongs to Secure
   Boot (§9).
9. **The OVMF oracle** (§8), beside the `-kernel` one, not replacing it.
10. **`td-update` and its local channel**: take a bundle, verify it,
   delegate the publish (D1 again), and roll back on a failed boot. This is
   where the update channel that was its own workstream rejoins this one.

   **Most of item 10 is already built, and the first thing this item needs is
   for that to be written down** — otherwise its obvious-looking scope is a
   rebuild of `td-boot`. Everything after "verify it" exists and is driven
   from the shipped image today. `install_deployment` IS the update
   transaction: it opens the bundle, verifies it, publishes it, and
   activates it against a known-good fallback.
   It is already idempotent — republishing the id that is `current` retains
   it and repairs `previous` rather than activating anything — and it already
   refuses the cases that would leave a machine with nowhere to go: activation
   while `current` is pending without a verified fallback, retaining a pending
   deployment as the fallback, replacing an invalid fallback with a pending
   one. Rollback is a verb (`td-boot rollback`), acknowledgement is a verb
   (`td-boot success`), the three durable attempts and the automatic demotion
   of an exhausted candidate are in `select_boot_deployment`, and
   `/etc/inittab` on the shipped image already runs `td-boot success /dev/vda
   /run/td-update "$deployment"` after a good boot. That mountpoint is named
   for this item and predates it.

   So the update path is a CHANNEL front end, not a second publisher. What it
   adds over `td-boot install` is where a bundle comes from and which one to
   take.

   **It is a VERB, `td-boot update`, and not the separate `td-update` program
   this item first specified.** The reversal is recorded rather than quietly
   applied, because the earlier text said "it execs the one writer" and that
   is no longer what happens — D1 is served by a CALL now, which is strictly
   more direct. td-boot already carries `install`, `publish`, `success` and
   `rollback`: four verbs that are the update transaction rather than the
   boot, so the binary is already the deployment tool that also boots, and
   `update` is consistent with what it is. A separate crate would have needed
   a manifest and lock, two `affected.rs` rosters, a target recipe and image
   inclusion before any behaviour landed, plus a third `#[path]` copy of both
   `protocol.rs` and the real-file rule now in td-fs — and would still have
   had to exec td-boot, putting the channel logic and the publish logic in two
   processes with argv between them.

   What that costs is real and worth naming: the SELECTOR initramfs packs
   td-boot, so it now carries channel code it never reaches. That is dead
   weight on the boot path rather than live surface — `run_boot` has no path
   to it — and the same is already true of the other four verbs. If the
   selector ever needs to shrink, splitting the update verbs out is the move,
   and this paragraph is what to re-read.

   **The real-file rule.** Every reader of a manifest, payload or channel
   entry takes it through one rule: a regular file, not a symlink, of at
   most its bound, typed by `lstat` BEFORE the open (opening a FIFO
   read-only blocks until a writer appears, so a check after it never
   runs), its `st_dev`/`st_ino` pair compared across the open, and the
   bound applied to what is READ, one byte past the limit refusing, not
   only to what `stat` claimed. `/proc/self/status` is that disagreement
   without a race to arrange — a regular file whose reported length is
   zero and whose contents are not — and is what the test uses.

   **10b made that rule ONE implementation.** It had been written three
   times, in td-boot, td-install and td-net, one of them saying outright
   that it was "mirrored". That is exactly the shape `protocol.rs` exists to
   prevent, "a thing spelled in both crates is a thing they can come to
   disagree about", and it is not a hypothetical: the td-install copy
   diverged three ways on the day it was written (type checked after the
   open, no identity pin across the open, an oversized read truncated rather
   than refused), all found by review rather than by anything that runs. So
   the pair moved to `td-boot/src/realfile.rs`, which all three
   `#[path]`-include as they already do `protocol.rs`; it has since become
   `td-fs/src/real_file.rs`, one file of the crate td's other programs take
   their file helpers from.

   It carries the redundant `#[path]` `fixture.rs` carries, for the same
   reason: `affected.rs`'s staging guard reads `#[path]` attributes and is
   blind to a plain `mod`, so spelling it that way is what makes BOTH target
   recipes staging the file a checked property rather than something to
   remember. Removing either stage reds `self_test_passes_against_repo` naming
   the file and the recipe, instead of failing the target build an hour later
   inside `recipe-checks`.

   td-net also `#[path]`-includes the `td-fs/` tree, and no gate builds
   td-net from source — the recipe-graph warm does, which is a chain target
   — so that tree has a ROUTING rule of its own in
   `builder/src/affected.rs` that selects those targets and td-net's own
   suite; td-boot's sources, `protocol.rs` included, fall to the td-boot
   rule.

   **10c closed the residual race, once, for all three.** The type is settled
   by `lstat` and the open follows it, so the entry can be replaced in between
   — and the identity pin catches that only for a swap the open SURVIVES.
   Two do not. A symlink is followed and the target read, which the pin then
   notices after the fact rather than preventing; and a FIFO BLOCKS the open,
   where no check on the far side of a blocking call ever runs. The second is
   the one that matters: td-boot reads a manifest on the boot path, so it is a
   machine that hangs with nothing printed rather than one that refuses.

   So the open carries `O_NOFOLLOW | O_NONBLOCK`. Neither flag changes anything
   for a regular file — reads ignore `O_NONBLOCK` — which is why the identity
   pin still does the work for a regular-for-regular swap; the flags handle
   only the two cases it cannot.

   The `lstat` stays in front of the open rather than being replaced by it,
   and what it buys is narrower than it looks: it keeps this off device nodes
   in the ORDINARY case, some of them acting merely on being opened, but a
   node swapped in after it is still opened and the pin only notices
   afterwards. So that race is NOT closed, and saying otherwise would be the
   claim this file exists to avoid. Closing it needs `O_PATH | O_NOFOLLOW` —
   which opens without acting on the file — an `fstat` to type it, and a
   reopen through `/proc/self/fd/N`. That is refused here for one reason: it
   would put a `/proc` dependency on the BOOT path, where td-boot reads a
   manifest from an initramfs that need not have mounted one, and a reader
   that cannot read is worse than a residual race an attacker must already
   have write access to the directory to reach.

   Both constants are spelled by value, since these crates have no libc, and
   pinned in two directions. By what they DO: a symlink opened with
   `O_NOFOLLOW` must fail AND a regular file must not, which is what separates
   it from the `O_DIRECTORY` one bit away, and a writerless FIFO opened with
   `O_NONBLOCK` must return at once. And by WHERE: the target is pinned whole
   at compile time to x86_64-linux, in the shape `td-svc` and `td-init`
   already use, because a wrong number here is SILENT rather than loud —
   `build_open_how` masks flag bits it does not recognise instead of refusing
   them, so a wrong `O_NOFOLLOW` is an open that quietly follows symlinks
   again. arm64 is the trap that makes the whole-target form the right one: it
   overrides the generic header, where `0o400000` is `O_LARGEFILE`.

   The post-`lstat` open is its own function so those flags are tested THROUGH
   the production path rather than through an `OpenOptions` a test built
   itself — handed metadata deliberately made stale, which is the race
   arranged rather than won. The symlink case points back at the file that was
   typed, same device and inode, so the pin would accept it and `O_NOFOLLOW`
   is the only thing that refuses. What those tests still do not pin is
   EXACTNESS: each flag is checked as sufficient, so a superset would pass.

   **10e — the boot oracle's update runs three times.** The
   `td.deploy=install` pass first updates over an EMPTY channel, which must
   succeed and name nothing, then over the real one, which must install
   and echo the marker; the rollback pass (§11) then reinstalls from that
   same channel. Both channels, `td/incoming` and `td/incoming-idle`, are
   seeded 0755, and the fixture volume is built root-owned through
   `builder userns-private`, since 10f refuses a channel that is not
   uid 0 with no group or other write.

   **10g — the oracle drives the VERB.** Those passes went through `install`
   with a bundle path until `td-boot update` existed; they take a CHANNEL now,
   which is what an actual machine's timer runs. The gate refuses a script
   whose update chain names the candidate at all: `update` would look for
   `<candidate>/candidate` one level too deep, find nothing, and read every
   tick as nothing to do — a silent no-op leaving every other marker green.
   That tripwire is scoped to the chain rather than asked of the whole script,
   because it is a substring test for a common English word over 10 KB that
   also carries five probe farms and the configured hostname: unscoped, any
   innocent later use of the word false-REDS it, and the message points here.

   The IDLE pass is the one this verb needed and `install` could not have. An
   up-to-date machine is in that state on almost every tick, so a verb that
   errored there would fail a machine continuously and no gate could see it.
   Both halves are asserted, and only the second separates idle from an update
   that quietly installed something: exit 0 AND stdout empty. The harness
   stages `DEPLOY_IDLE_CHANNEL` as a second, empty directory rather than
   emptying the real channel, which has to keep its candidate for the pass that
   follows.

   Empty directories DO survive `mkfs.btrfs --rootdir` and `td-btrfs restore` —
   checked rather than assumed, since both this fixture and the installer's own
   `VOLUME_CHANNEL_DIR` depend on it. `td-install-test` asserts it on the real
   restored image now, rather than leaving it to a check somebody once ran by
   hand: `td-install`'s own tests read the STAGING tree, before a filesystem
   exists, so nothing else looks at the channel after both have had it. It is
   asserted with `[ -d ]` plus `ls -A`, whose exit status is CHECKED, because
   `$(…)` captures stdout alone — a busybox whose `ls` did not take `-A` would
   print its usage to stderr and hand back the empty string, and an emptiness
   test that reads that as "empty" passes whatever is in the directory. `rmdir`
   would say all three things in one call and is not in that recipe's declared
   applet set, which is the trap `cmp` is already noted for one line above it.

   EACH PASS IS HELD TO BOTH HALVES OF ITS CONTRACT, which the first version of
   this item got right for the idle pass and wrong for the real one. That pass
   asserted only that `update` exited 0 and never looked at what it had
   redirected — and `update` exits 0 printing nothing whenever a channel holds
   no candidate, so ANY wrong channel there printed the install marker and the
   boot-success marker with every gate assertion green. It is the exact failure
   the paragraph above says this item keeps circling, reintroduced by the commit
   that added the idle pass to catch it. The real pass now requires a non-empty
   id as the idle pass requires an empty one.

   The gate pins the real pass's channel as a whole argument, not by
   substring: `VOLUME_CHANNEL_DIR` is a strict PREFIX of `DEPLOY_IDLE_CHANNEL`
   (`td/incoming` of `td/incoming-idle`), so a substring test for the channel
   is satisfied by the idle pass and says nothing about the real one.

   **10f — the channel is the volume's and root's.** With no key, what
   keeps an unattended updater from installing whatever anyone offers is
   where the channel may be. `update` takes the volume's read-only view as
   an argument and requires it to BE a mount point, through
   `is_mount_point` — `mountpoint(1)`'s test, a device number that differs
   from its parent's, and no `/proc` at all: `/init` creates that
   directory before moving the volume onto it, so a move that never
   happened leaves an ordinary directory on the `/run` tmpfs. The channel
   must be absolute and a real directory; it is resolved, and the resolved
   path must lie on that volume ("update channel … is not on the volume
   …"). Every directory from the volume root down to the channel must be
   a root-owned real directory that no group or other may write, except
   that any directory above the channel may be group- or other-writable if
   sticky; the channel itself may not ("update channel …: … must be
   root-owned and writable by no one else"). The installer pins the
   volume root it stages to 0755, so no umask leaves it writable to
   others. Every tick checks this before looking
   for a candidate, idle ticks included, so a channel that has become
   unsafe is reported on a tick with nothing to install. A candidate that
   is there must be a root-owned real directory no one else may write
   ("update candidate … must be root-owned and writable by no one
   else"), holding a manifest, kernel, initramfs and root that are each a
   root-owned regular file no one else may write ("update candidate file
   … must be a root-owned regular file writable by no one else");
   otherwise its owner could rewrite it between the decision and the
   publish. The candidate is looked up once, so one removed before
   that lookup is nothing offered, not an error; one removed after it
   fails the tick at its files.
   Resolving first matters because `Path::starts_with` is lexical:
   `<volume>/../x` is inside the volume to it and outside to the kernel.
   The volume is NAMED rather than derived: nothing distinguishes a
   mounted volume from an ordinary directory of the same name except being
   told which to expect. A bind mount of a directory on the same
   filesystem shares its parent's device number, `is_mount_point`'s
   documented gap; a btrfs subvolume boundary reads as a mount point,
   unreachable here because `/run` is a tmpfs.

   **The LOCAL channel** is a directory holding AT MOST ONE bundle, under the
   fixed name `protocol::CHANNEL_CANDIDATE`. Local is the whole of it for now:
   no network, no fetch, no protocol. A fetching update is a separate
   decision with its own trust argument, not an increment of this one. The
   channel lies on the volume and only root writes it (10f), so a bundle
   arrives there by root copying it in; `VOLUME_CHANNEL_DIR` is the
   volume's own.

   **One fixed name rather than a search, because td cannot order two
   bundles.** A manifest is a header and a list of payload digests; it carries
   no version, no timestamp and nothing else that says which of two is later,
   and the deployment id is a hash, which sorts meaninglessly. Anything that
   picked the "newest" would be guessing from mtimes an attacker supplies or
   from names nobody validates. So WHICH bundle is the producer's decision,
   expressed by what it leaves at that name. Giving td a real notion of newer
   means a version field inside the manifest, which is a change to the
   deployment contract and its own landing.

   **How a producer publishes into the channel**, stated because an earlier
   draft of this paragraph said "replacement is `rename` — atomic" and that is
   not implementable. `rename(2)` refuses a destination directory that is not
   EMPTY, so it can put a FIRST candidate in place atomically and cannot
   replace an existing one. The protocol is therefore: stage the bundle under
   any other name INSIDE the channel, remove the old candidate if there is
   one, then rename the staged one over it.

   That leaves a window in which the candidate is absent, and the window is
   affordable for exactly the reason the verb exists — a timer. A tick landing
   in it reads "nothing offered", exits 0, and the next tick installs. What
   stage-then-rename does still buy is the atomicity that matters: no reader
   ever sees a HALF-WRITTEN bundle, only a whole one or none. A tick that
   catches the removal mid-read fails loudly rather than installing something
   partial, because every payload must match the manifest it is read
   with.

   Closing the window properly needs an atomic exchange — `renameat2`'s
   `RENAME_EXCHANGE`, a syscall, so an `UNSAFE.md` amendment — or a `candidate`
   SYMLINK swung between generation directories, which rename does replace
   atomically. The second is cheap and is refused for now only because it
   contradicts the real-directory rule below; if the window ever matters, that
   is the trade to revisit.

   **What `update` does that `install` does not** is all about being run by a
   TIMER rather than a person, and each difference is the same judgement: an
   updater nobody is watching must not decide for itself that something
   missing is fine.

   Its channel is required ABSOLUTE: nothing sets a timer's working
   directory, so a relative path is a different directory depending on who
   started it. And it is held to 10f's rule on every tick.

   A missing CHANNEL is an error and a missing CANDIDATE is not. That is the
   one distinction the verb exists to draw: pointing a timer at a typo would
   otherwise answer "nothing to do" forever, and the report arrives only when
   an update is finally urgent. An absent candidate, by contrast, is the
   ordinary state of a machine that is up to date, so it exits 0 with stdout
   EMPTY.

   What that empty stdout distinguishes is narrower than an earlier draft
   claimed, and the difference is worth being exact about: it separates "the
   channel offered nothing" from "the channel offered something", NOT
   "installed" from "already installed". NOTHING REMOVES THE CANDIDATE after a
   successful install — no code does it and no document assigned it — so a
   machine that is up to date keeps re-deciding on the same bundle and printing
   its id every tick. That is harmless because the publish is idempotent and
   because a repeat costs almost nothing (below), but a caller that read a
   printed id as "something changed" would be wrong every tick after the first.

   Consuming the candidate is the obvious refinement and is deliberately not
   taken here, because it decides something this item has not: whether the
   channel is the updater's to mutate, and the updater holds the volume
   read-only.

   The candidate must be a real DIRECTORY, `require_real_directory`'s rule
   everywhere else in td-boot. A symlink would make the channel's contents
   depend on something outside it whose path nothing checks.

   That rule holds against a producer that got it wrong and NOT against one
   RACING the call, which is a residual worth stating rather than leaving to be
   discovered. The candidate is typed before the volume is locked and mounted
   and is opened by pathname afterwards, so a writer who has the channel —
   root, by 10f — can swap a directory in between; `O_NOFOLLOW` binds only
   the final component of each file it then opens, not the directory
   above them. What bounds the
   damage is that whatever is read must still match its manifest, so the
   most a racing writer achieves is installing a DIFFERENT whole bundle —
   which is also what it achieves by simply writing that bundle to the
   candidate, which it is entitled to do.
   Closing it needs the directory-handle route (`O_PATH` plus `openat`), which
   `std` does not expose and which item 10c already declined on the boot path.

   A REPEAT tick copies nothing: `publish_bundle` short-circuits an id
   already published, so a tick that finds the candidate it installed last
   time costs reading and hashing it and a directory sync rather than a copy
   of the image.

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
an install onto a regular file that then boots under OVMF, and an update
that installs a second deployment and rolls back to the first.

The second is BUILT, inside `qemu-boot-system`'s install boot, which is
the only place a running td machine exists to drive it from. The
rollback pass runs after the install one and ends
where it began — it rolls back to the deployment that is RUNNING, requires the
id it lands on to be exactly that one, then reinstalls from the same channel
and requires that to name what the first install named. Restoring is not
tidiness: the boots after this one expect the candidate to be current, so a
pass that left the volume rolled back would fail them and blame selection.
What the restore buys beyond that is the thing worth having — a rolled-back
volume is one an update can still proceed from, which is the real sequence an
operator meets, an update that boots badly followed by another attempt.
`SYSTEM_DEPLOY_ROLLBACK_MARKER` is the only report that any of it happened,
precisely because the pass is a round trip.

Between those two halves is the branch that makes the pass mean anything, and
it was missing from the first version of it. Every other branch reads a
PRINTED id, and a rollback that printed the right id without rewriting
`current` satisfies all of them: the reinstall then finds its own id already
current, takes `install_deployment`'s idempotent branch, prints that id, and
the comparison after it agrees too — a round trip that went green having
rolled nothing back, which is exactly the silent no-op this item keeps
producing. So the pass also runs `td-boot success` on the deployment it
claims to have rolled back to, which refuses unless that id IS current. It is
the one branch observing an EFFECT rather than a report, and on a deployment
already marked successful it returns before doing anything else, so it stays
an observation.

Two clocks run over that loop and both had to move. `svc_timeouts::BOOTSUCCESS`
is td-svc's backstop on the unit; `shared::DEFAULT_BOOT_TIMEOUT_SECS` is the
host's ceiling on the whole boot, and it is declared in `ladder` rather than
beside the oracle so the guest side can assert the relation between them. A
host that gives up first turns a diagnosable unhealthy boot into a bare
timeout carrying no guest-side reason, so the ceiling must outlast the
clamped iteration count times what an iteration may cost — which is a test
now, because the first version of this pass raised the guest's per-iteration
budget past the host's entire ceiling and nothing noticed.

What the pass costs is worth stating precisely, because the fixture's size
budget turns on it and a wrong version of this sentence was written first.
The REINSTALL copies nothing: the deployment is already published, so
`publish_bundle` finds its destination and takes the existing-bundle path.
`create_persistent_volume_layout` sizes the volume for three deployment-sized
copies and that is still right. What the pass adds is deployment-sized READS.

Two limits, both found in review of it and neither closed:

- The RETRY loop no longer covers this pass. Reaching `current = candidate`
  used to imply the boot was healthy and about to exit; now a rollback branch
  can fail with the candidate current and the running deployment not, and
  every later iteration then fails at the `td-boot success` that guards the
  chain — nine iterations reporting that the running deployment is not
  current rather than what actually broke. A transient failure in this pass
  is therefore a hard boot failure, where the loop exists to absorb one.
- The REINSTALL's effect is not observed in the pass, where the rollback's
  now is. A reinstall that printed the right id without activating it would
  pass here and surface three boots later, as the healthy-candidate boot
  selecting the initial deployment instead of the candidate — caught, but
  attributed to selection. The rollback half could be closed with `td-boot
  success` because that verb refuses unless the id it is given is current and
  does nothing else to a deployment already marked successful; the reinstall
  half has no such observer, since `success` on the CANDIDATE would clear the
  attempt state the boots after this one are written around.

The first is item 9 and is still owed.
