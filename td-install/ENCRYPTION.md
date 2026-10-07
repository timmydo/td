# Disk encryption and session unlock

This is the normative target for the `disk-encryption-rolling` workstream.
It complements [deployment installation](DESIGN.md) and
[protected elevation](../APPLICATIONS.md#l1-elevation--consent-without-a-secret).
It defines two tiers over one volume format. The **device-bound** tier is
the target default installation: it encrypts storage with no interaction
at boot and binds it to this machine's TPM and boot chain. The
**protected** tier is the lost-laptop target: hardware-backed PIN unlock
and authenticated login. A device-bound volume upgrades to the protected
tier by replacing protectors and its volume key, never by reinstalling.

The live installer installs device-bound storage on a machine with a
usable TPM 2.0 and a keyboard console, and unencrypted storage otherwise
("Activation"); the installed account logs in automatically either way.
Device binding protects a disk read away from its machine, not a lost
one. No increment may describe enrollment, disk confidentiality beyond
that, or protected login as shipped until its complete boot and recovery
path passes the acceptance tests below.
The planned TPM-free login-key tier in
[td-login/TOKEN-LOGIN.md](../td-login/TOKEN-LOGIN.md) is an authentication
tier over either storage state, not a third storage tier: it replaces
automatic login with a FIDO2 key and PIN at boot and lock. Its scope,
including what it leaves unprotected, is stated there.

## Scope

The protected tier protects private data when a laptop is lost while
powered off or locked:
the finder may copy its disk, boot another OS, or try the login screen.
Trust the kernel, firmware and td's isolated authentication UI. Invasive
hardware attacks, memory extraction, prior compromise, and an attacker at
an already-unlocked desktop are outside this scope. A locked running or
suspended machine still holds storage keys in RAM and relies on session
isolation; it does not regain the powered-off confidentiality boundary.

Use LUKS2 over kernel dm-crypt, initially AES-XTS, around the whole td
Btrfs volume: deployments and `@var`, including homes, logs and credentials.
The EFI partition and partition/enrollment metadata remain readable. Keep
deployment signatures: encryption does not authenticate mutable sectors or
prevent replay. Authenticated sectors and general anti-rollback storage are
separate work. Encryption applies only to fresh installs; no plaintext
volume is converted in place, and there is no erasure claim about
historical plaintext copies. Recipe-built file images, including the stock
VM, stay unencrypted: a build cannot hold a volume key without placing it
in a store output.

## Device-bound default

The device-bound tier has a narrower scope. It protects storage read away
from its machine: a removed drive or a copied disk image. Erasing the LUKS2
header and keyslots makes what the installed system wrote unreadable on a
disposed disk; it does not erase what the disk held before installation
("Device-bound formatting"). On the same machine it
binds release to td's selector image, its initramfs and its load options;
code that runs before the selector, including option ROMs and firmware
drivers, is not covered. It does **not** protect a lost or stolen machine:
whoever powers it on reaches the automatic-login desktop exactly as on an
unencrypted install, or, once the planned login-key tier exists and keys
are enrolled, its lock screen. No installer text, document or claim may
describe it, alone or combined with login keys, as lost-laptop
protection: with login keys the volume key is still released with no user
action, so a locked machine holds it in RAM while exposing its devices and
every surface reachable before unlock.

TPM possession here releases storage and nothing else. It is device binding
in the sense of AGENTS.md principle 7, not authentication: the session is
still admitted by the existing, disclosed automatic login (or, once it
exists, by the planned login-key tier), and this tier neither changes nor
strengthens either. Root on the booted system holds the volume key and can
change any protector, so this tier makes no authorization claim about
protector changes. Its own protector changes happen only in the selector,
as described below, and never through root's administrative path on the
booted system (APPLICATIONS.md §L.1).

The installer generates the volume key on the machine and formats the
volume with a **first-boot protector** and a **recovery key**. Neither the
volume key, a protector secret nor the recovery key may enter a recipe,
store output, log, argv, environment variable, kernel command line or
persistent temporary file. Keyslots for these high-entropy secrets use
PBKDF2 at minimal cost, not memory-hard Argon2.

Every protector is a random secret sealed by the TPM, with dictionary-attack
exemption, under a PolicyPCR over the SHA-256 bank. Every policy requires
PCR 12 at its reset value of zero. Sealed protector blobs live in td-typed
LUKS2 tokens, each naming its keyslot. Only td's bounded Rust reader parses
them before the cap; cryptsetup writes them only after it, through LUKS2's
own crash-safe header commits, and the ESP never changes. The selector
releases in this order, and no step may move earlier:

1. td's bounded reader loads the sealed blobs from the header copy that
   cryptsetup will use, chosen by the same checksum and sequence rule, and
   refuses copies that disagree; no C parser has run. Where that copy
   fails td's own checks, td refuses rather than fall back. cryptsetup
   also falls back from a copy whose JSON fails its wider validation, so
   td reads the copy cryptsetup will use or refuses, with one gap
   ([td-protector](../td-protector/DESIGN.md) "LUKS2 tokens"): from a copy
   failing only cryptsetup's validation, the released secret opens the
   volume only if its keyslot is in cryptsetup's copy, and otherwise the
   boot reaches recovery. No header change follows unless cryptsetup's
   own metadata agrees with td's copy (td-protector "Transitions").
2. The selector tries every td token, up to a fixed bound, device-bound
   tokens first.
3. Only when the first-boot protector alone releases, it seals the
   device-bound protector described below to the observed PCR 4 and PCR 9
   values and a literal-zero PCR 12, then unseals that new protector once
   to verify it.
4. Whether or not any unseal succeeded, it extends PCR 12 with a fixed td
   release-cap event and requires an exact readback, as the PCR 11
   measurement does. A PCR 12 already non-zero (td-protector's
   `AlreadyClosed`) means nothing could release; the installed selector
   enters the recovery flow, which then offers no reseal and runs no
   plan. A TPM with no SHA-256 PCR bank allocated (`NoSha256Bank`) can
   meet no protector's SHA-256 PolicyPCR, and its banks change only by a
   platform-authorized allocation effective at the next reset, so when
   nothing released and no protector was sealed this boot the installed
   selector enters recovery for it as it does for `AlreadyClosed`, with
   no reseal and no plan. A release or seal this boot shows the bank
   existed, so the cap reporting none then is a contradiction that halts
   as an uncertain cap does. The first PCR 12 read, which cannot move
   it, is tried once more when it fails; the extension and the readback
   never are.
   An uncertain or mismatched extension zeroes every released secret,
   refuses boot and halts on the console, so that only a platform reset
   leaves it: exiting init would panic, and `panic=-1` would reboot into
   the same refusal without end. A TPM that fails every command
   therefore stops the selector until it is disabled in firmware, when
   the no-device rule below applies.
5. Only after the cap does cryptsetup parse the header and the selector
   run td-protector's transition plan, which owns the commit order and
   what is superseded or orphaned ("Transitions"). No destroy precedes a
   verified add or a verified released keyslot. After step 3 the new
   protector's keyslot and token are committed and tested before the
   first-boot keyslot and token are destroyed. When a device-bound
   protector released, its keyslot is first tested with its secret, and
   only then are a leftover first-boot or superseded keyslot and token
   and any orphan removed. Then the selector opens the volume.

When the cap runs, it closes release until the next platform reset on
every later path, including refusal, recovery and `kexec`. No other td
component extends PCR 12. The installed selector caps PCR 12 when its
volume is encrypted; on an unencrypted volume it makes no TPM contact,
which increment 7's activation ("Activation") left unchanged, so the
residual below for a not-yet-booted disk's first-boot protector stays.
The live selector's cap is MEDIA.md's ("Live boot"): it caps whenever a
TPM device is present, proceeds when PCR 12 is already closed or the TPM
has no SHA-256 bank, and skips the cap without a device.

Without a TPM device (`/dev/tpmrm0` absent) nothing can release or be
capped. An installed selector booting a td LUKS2 volume waits for the
device up to td-boot's TPM wait, a constant of 10 seconds
(`selector_release::TPM_WAIT`) that the implementer may tune and a test
pins, whatever the header holds: a header corrupted or stripped of its
td tokens by someone who can write the disk must not shorten the wait
and so widen the late-TPM residual below. If the device appears, the
release order runs. If it does not, the selector skips the cap, its
console says that PCR 12 stays open to a TPM exposed later, and it
enters the recovery flow, which offers no reseal. These are the paths the
cap does not close, and their residuals are disclosed. A TPM that the kernel
exposes only after that wait, by a late driver probe, sees PCR 12 still
zero, so on that boot root can release the disk's first-boot and
device-bound protectors; the deployment initramfs's check ("Boot and
authority boundaries") catches it only when the TPM is visible by then.
A not-yet-booted encrypted disk's first-boot protector likewise remains
releasable to root on the same machine while it runs an unencrypted td
install, or a live session whose selector found no TPM device that the
kernel exposes later; its device-bound protectors are not, since neither
boot measures that disk's selector initramfs into PCR 9.

The first-boot protector's policy names PCR 12 alone. PCR 4 holds the
firmware's measurement of the selector EFI image (`EFI/BOOT/BOOTX64.EFI`),
whose built-in command line names its initrd. PCR 9 holds the EFI stub's
measurements of any firmware load options and of the prepared selector
initramfs (`EFI/BOOT/INITRD`). The selector refuses to seal while either
PCR is unmeasured and enters recovery instead. An interrupted first boot
keeps the first-boot keyslot and repeats the transition, discarding any
orphan (td-protector "Transitions"); cryptsetup leaves a token whose
keyslot was destroyed naming no keyslot, and td's reader reports it
without releasing it. Until the installed selector's first boot,
release is not bound to the boot chain: any OS that leaves PCR 12 at zero,
other than td's live medium, can unseal the first-boot protector. The first
boot also adopts that boot's load options, so a one-time firmware boot-menu
entry can send later ordinary boots to recovery. The review discloses both.

The policy is exact PolicyPCR without PolicyAuthorize, so it never names a
predicted value. A change to the selector image, its initramfs or its load
options, a cleared or replaced TPM, or a firmware change that alters PCR 4
or PCR 9 refuses release. Firmware code and configuration PCRs are excluded
so that most firmware updates are intended to keep releasing.

When release fails, the selector enters a recovery flow on its console.
It never falls back to plaintext, retries with weaker policy, or skips
the volume. That console is the serial console, which the selector's
built-in command line makes `/dev/console` (DESIGN.md "Full-system
volume consumers"), and beside it the screen's virtual terminal
("Keyboard console"): the same prompt on both, read from whichever
completes an entry first. The selector reads the recovery key through
td-init's secret-line applet with echo off, so its digits are never
echoed; a console server or BMC recorder on the serial line may still
log what is typed, which the review discloses. The applet, not the
selector, prints the prompt, and only after echo is off: it turns echo
off with `TCSETS` and then discards pending input with `TCFLSH`
(UNSAFE.md §3), so anything typed before the prompt is discarded rather
than kept or joined to the entry. It reads one whole canonical record:
an entry is bounded at 256 bytes, and a longer record, or one holding a
newline before its end, is refused whole, consuming nothing after it; a
record ended by `^D` rather than a newline ends input and is refused.
Each entry is tried on keyslot 0 alone, and a wrong one prompts again,
without a limit: a 128-bit key needs no retry bound. End of input
refuses boot and halts instead ("Selector release"). After the cap, a
correct recovery key opens the volume; when this boot's own cap closed
PCR 12 and a reseal can run ("Selector release"), the selector then
offers, with an explicit console confirmation, to seal a device-bound
protector to the observed PCR 4 and PCR 9 values and a literal-zero PCR
12, commit its keyslot and token, and only then destroy every other td
keyslot and token, a surviving first-boot one included (td-protector
"Transitions"). Its release is first proven on the next boot, and the
recovery keyslot remains, so a failure returns to recovery. Recovery
without that confirmation boots once and runs no plan, leaving the
header, orphans included, unchanged. Nothing reseals automatically,
because that would adopt a changed boot chain without its owner's
decision. The live medium can open the volume with the recovery key for
data access.

The recovery key is 128 random bits read from `/dev/random`, encoded in
grouped decimal digits with a check digit per group so that entry does not
depend on the keyboard layout ([td-protector](../td-protector/DESIGN.md)
"Recovery key" owns the encoding). Its keyslot passphrase is the 48 digits
without separators, so stock cryptsetup opens the volume with it from any
medium when given the 48 digits alone; only td's own entry tolerates
separators between groups. The installer displays it once on its completion
screen, requires it to be typed back, and stores no copy. Completion waits
for the type-back, and so does the disk's partition table: until the key
is confirmed the disk carries none, so firmware boots nothing from it. If
the installer is lost before it, the installation is withdrawn like any
failure after layout (DESIGN.md "Device-bound formatting"), because
recovery cannot be declined in this tier.

The installer seals the first-boot protector on the live medium under
td-protector's first-boot policy. Sealing reads no PCR, and TPM2_Create
does not evaluate the policy, so the seal does not depend on PCR 12's
value at that moment. The installer never unseals that protector. On a
machine with a TPM device that has a SHA-256 bank, the only one the
installer seals on, the live selector has already capped PCR 12
(MEDIA.md "Live boot"); increment 5's encrypted-installation oracle
(below) boots its own test initramfs, not the live selector. After
sealing and before declaring success, the installer verifies that the
recovery keyslot and the first-boot keyslot each open the volume
(`--test-passphrase`), the latter with the secret it still holds. It then
reads the token back from the header through td's reader and runs
td-protector's `verify_first_boot_object` on it: the sealed object's
public authPolicy equals the PCR-12-at-zero policy computed in a trial
session, and the TPM loads the sealed object under the storage primary.
The first-boot protector's first TPM release is therefore on the
installed first boot, and the typed-back recovery key covers its failure.
The review discloses that the recovery key is the only way back if the
TPM, firmware measurements or boot chain change.

td has no selector-update operation. Increment 8 specifies one, with its
crash-safe commit of both ESP files and its protector transition
("Selector update"); until it lands, and on a device-bound volume after
it, a changed selector reaches recovery and its confirmed reseal.

TPM bus interposition is an invasive hardware attack, outside Scope; this
tier's unseal sessions need not be salted or encrypted. The protector
formats and policies are disk-specific: td-protector
([td-protector/DESIGN.md](../td-protector/DESIGN.md)) carries the
policies, protector secret and release cap that the installer and
selector share. It runs over the td-tpm client crate shared with
td-secret, but not over td-secret's application-secret formats.

Without a usable TPM 2.0, the installer offers no device-bound volume and no
passphrase substitute. A usable TPM has a SHA-256 PCR bank, and the live
boot shows PCR 4 and PCR 9 measured. The review discloses that storage will
be unencrypted, and installation proceeds only under that disclosed plan.
The service chooses for itself, with no operand: device-bound only when
this probe passes and the live system shows a keyboard console,
unencrypted with the disclosure otherwise, and it refuses neither
("Activation").

Upgrading to the protected tier re-encrypts the volume to a fresh volume
key, drops the device-bound protector and keeps the recovery key as the
recovery protector, its keyslot rewrapped ("Re-encrypting upgrade",
increment 8's target). The rotation is required, not a precaution: the
device-bound volume key was released to TPM possession alone on every
boot, so anyone who held the machine and its TPM before the upgrade
could have taken it, and no new protector would protect a key already
taken. Re-encryption defeats retained copies of the old key and header
for what the volume holds afterwards, not old ciphertext an SSD keeps in
stale flash pages; it does not remove persistence left by anyone who was
root on the device-bound system, which with automatic login and physical
update installation (`I`, which installs a locally built system) is
anyone at the keyboard. Such prior compromise is outside Scope, so a
protected-tier claim on an upgraded volume is no stronger than the
device-bound system's integrity before the upgrade.

## Device-bound formatting

Increment 5 formats device-bound volumes, and increment 7 makes them the
default: `td-install serve` formats one exactly when its plan names
device-bound storage, which follows its own probes ("Activation"). No
review, wizard page, request or operand selects it: storage policy is not
a caller-selectable flag. Increment 7 deleted the storage operand that
reached this path before it.

The volume is formatted with exactly these parameters, the device being a
loop over the volume's extent (DESIGN.md "Device-bound formatting"):

```text
cryptsetup luksFormat --batch-mode --type luks2 --cipher aes-xts-plain64
  --key-size 512 --sector-size 4096 --hash sha256 --pbkdf pbkdf2
  --pbkdf-force-iterations 1000 --use-random --luks2-metadata-size 16384
  --luks2-keyslots-size 16744448 --offset 32768 --uuid PLAN-UUID
  --label td-system --key-slot 0 --key-file=- DEVICE
```

Each LUKS2 header copy is 16 KiB and the keyslots area fills the rest of
the first 16 MiB, where the data segment starts (`--offset` counts
512-byte sectors): cryptsetup's default layout, stated so that no version
change moves it. The segment's size is dynamic, the rest of the
partition, which the layout ends on whole 4 KiB sectors (DESIGN.md "Disk
layout") so that stock cryptsetup opens the partition itself. Volume fit
and the minimum volume size subtract those 16 MiB from the partition's
length. The protected-tier upgrade re-encrypts online with checksum
resilience, whose hotzone lives in the keyslots area, so the layout keeps
no data-shift reserve. The LUKS2 UUID is the plan's volume UUID, which the
Btrfs filesystem inside and the prepared selector also carry, and the
label is `td-system`. The volume key is generated inside cryptsetup from
`/dev/random` and never leaves it.

Keyslot 0 holds the recovery key and keyslot 1 the first-boot protector,
added with the same PBKDF parameters. Token 0 carries the sealed
first-boot protector, naming keyslot 1, in td-protector's token format
("LUKS2 tokens"). Key material reaches cryptsetup only through
descriptors, never argv or the environment; DESIGN.md "Device-bound
formatting" owns the mechanism.

Formatting erases nothing beyond what it writes. `luksFormat` writes the
16 MiB header area and leaves the data segment as it found it; mkfs.btrfs
and td-boot then write, through the mapping, only the blocks the new
filesystem uses. Every other block of the segment keeps what the disk held
before, in plaintext. An attacker with the disk reads those earlier
contents wherever the installed system has not yet overwritten them, which
just after installation is nearly the whole volume, and can tell such
blocks from ones written since, so learns roughly how much the system has
written. The tier protects what is written after installation, not what
the disk held before it. td performs no whole-disk overwrite, discard or
drive sanitize; erasing a disk's earlier contents is its owner's separate
step before installing. The ESP and the partition table are plaintext on
every tier.

The table is written last and only after the recovery key is typed back
(DESIGN.md "Device-bound formatting"). A crash, power cut or lost
installer before then leaves a disk with no partition table, which
firmware does not boot. It still holds the ESP, with the kernel and the
prepared selector, and the volume: its header with keyslot 0, opened by
the unconfirmed recovery key, and keyslot 1 with token 0, whose protector
only this TPM releases; inside, the published deployment and the staged
settings, account and trusted key, none of them secret. A withdrawal also
zeroes the volume's first 16 MiB, so neither keyslot opens; a crash leaves
no withdrawal behind it, and a new installation over the disk replaces
both.

After the confirmation the table goes down backup first, then the primary
with the protective MBR, each synced. A crash between the two leaves the
backup GPT alone, with LBA 0 zeroed and so no protective MBR. Whether
firmware then boots is its own: one that falls back to a valid backup
header may boot the installation, one that wants the protective MBR or a
valid primary treats the disk as unpartitioned. Either way the recovery
key was already typed back, so a boot that reaches recovery is one its
owner can open; nothing completes the installation, which a new one
over the disk replaces.

## Selector release

This section, the release order and the handoff ("Boot and authority
boundaries") are increment 6's target. The installed selector's half is
current: td-boot runs the release order through td-protector's release
orchestration ([td-protector](../td-protector/DESIGN.md) "Release
orchestration"), opens the volume and hands its key to td-kexec's
`--fds-key` mode, and the live selector caps (MEDIA.md "Live boot"). So
is the deployment initramfs's unlock ("Boot and authority boundaries"),
which takes that key and opens the volume again; both initramfs carry
cryptsetup (DESIGN.md D6). So is the recovery flow with its confirmed
reseal (below), through td-init's secret-line applet, which the
selector initramfs alone links (UNSAFE.md §3). So is the running
system's half: its `install`, `update`, `rollback` and `success`, so
boot acknowledgement, updates and rollback, bind the `td-system`
mapping the deployment initramfs opened by its held descriptor, as they
bind an unencrypted volume's partition, and refuse a volume without it
(DESIGN.md "Full-system volume consumers"): the running system never
opens or unlocks the volume, runs no cryptsetup and reaches no TPM for
it.

The installed selector acts on what discovery finds under its configured
UUID (DESIGN.md "Full-system volume consumers"): a Btrfs volume boots as
it does today, with no TPM contact, and a td LUKS2 volume runs the
release order on its pinned partition. A mapping of the volume already
active before the release refuses boot and halts. Release and the cap,
with any recovery prompt, run before any mount or deployment selection,
so one volume key serves `current` and `previous` alike and selection,
fallback and boot attempts are unchanged. The PCR 11 measurement still
follows selection, over a command line the handoff leaves unchanged; the
handoff's archive lies outside its event. The released secret opens the
mapping `td-selector` through td-protector's runner, by standard input
and the partition's held descriptor; td-boot admits it by discovery's
mapping rule, requiring its device-mapper name to be `td-selector`,
holds it and the partition, and mounts and selects through it. Nothing
in the selector closes the mapping: it stays open until
`kexec_file_load` returns, since the kernel reads the deployment's
payloads through it. It does not survive `kexec`; the deployment
initramfs opens its own, `td-system` (td-boot's `VOLUME_MAPPING_NAME`),
and discovery
admits either by its `dm/uuid` and `slaves/`, whatever it is named.

After release and the cap, the selector obtains the volume key from
cryptsetup for the handoff without the key touching a block device or a
persistent file: `luksDump --dump-volume-key --batch-mode
--volume-key-file FILE --key-file=- PARTITION`, the released secret on
standard input, writes it into a fresh mode-0700 directory,
`/run/td-boot-volume-key`, in the selector's RAM-backed initramfs root,
on which nothing is mounted. td-boot reads exactly 64 bytes into a buffer
zeroed on drop, then unlinks the file and removes the directory at once,
whether or not the read succeeded, and drops the protector secret. The
mechanism was verified against the source-built static cryptsetup 2.8.8
on a file image formatted with the parameters above, since the verifying
user could not attach a loop device, and named through a held
`/proc/PID/fd/N` as td-boot names it: `tools_write_mk` opens the file
`O_CREAT|O_EXCL|O_WRONLY` at mode 0400 and writes the raw key, so an
existing name, a dangling symlink, and `/proc/self/fd/N` or `/dev/fd/N`
naming a pipe or a file are all refused (`Cannot open keyfile`) and no
descriptor-only route exists; `--batch-mode` skips its confirmation;
standard output then carries the header summary and `Key stored to
file`, never the key, which the hex dump prints only without a file, so
td-protector's runner starts a `luksDump` only in the two shapes td
builds, refusing an unfiled dump, its `--dump-master-key` and
`--master-key-file` aliases and `--unbound`;
`--key-slot` does not restrict which keyslot unlocks it; a wrong key
writes no file; and the file's 64 bytes equal the hex dump. Unlinking
retires the key from that filesystem only; its freed pages are a
residual like the memfd's.

A release that ends in recovery prints its reason and runs the recovery
flow ("Device-bound default"), still before any mount. Each entry is one
run of `/bin/secret-line` with td-boot's prompt as its operand; td-boot
reads the line from the applet's standard output, a std pipe, into a
buffer zeroed on drop, and never prints a prompt itself. td-protector's
recovery-key codec parses the line, admitting the 48 digits with spaces
or hyphens between groups. An entry the codec refuses (a wrong digit
count, a group above 65535 or with a wrong check digit, any other byte)
prompts again, naming the group or byte and never the digits, without
reaching cryptsetup, as does a record the applet refuses as over-long or
holding a newline (its status 4). An admitted key is tried with `open
--test-passphrase --key-slot 0`, its 48 digits on standard input; a
wrong one prompts again, without a limit. Only cryptsetup's
wrong-passphrase status is a wrong key: 2, which cryptsetup 2.8.8's
`translate_errno` makes of the keyslot code's `-EPERM`. Any other
failure of that test (a missing keyslot 0 or bad arguments, exit 1; a
wrong device, 4; a cryptsetup that cannot start or a signal) fails the
boot as a failed open does (below), since prompting again could never
succeed. End of input (status 3: `^D`, or a hang-up, which fails with
status 1 instead where the hung-up line then refuses its restore) and
any other failure of the applet refuse boot and halt as a failed cap
does: a console that answers end of input at once would make a repeating
prompt spin, and the selector never boots without a key that opened
keyslot 0, nor exits init. A platform reset returns to the same
recovery.

Once keyslot 0 opened, the reseal is offered where it can run: this
boot's own cap closed PCR 12, td read the header, the release did not
find PCR 4 or PCR 9 unmeasured, and td-protector's planner admits a
reseal over the header (keyslot 0 present and named by no td token, no
keyslot shared, a free keyslot and token number). The console first
says that confirming binds release to this boot chain and retires every
other td protector, then asks through a second run of secret-line,
whose flush discards anything typed before the question:
exactly `reseal` confirms, and every other answer, an empty one, end of
input and a failed run included, declines. Confirmed, td-protector's
`release::reseal` seals a fresh `/dev/random` secret to the observed PCR
4 and PCR 9 values and a literal-zero PCR 12 and runs the planner's
reseal plan once cryptsetup's metadata confirms the header, the recovery
key authorizing the add and any orphan's kill, printing each commit as
the release does. The seal is not verified by an unseal, which the
closed cap refuses: the plan's test of the new keyslot with its secret
is this boot's evidence, and the TPM release is first proven on the
next boot. An unmeasured PCR, a failed seal or a plan that does not run
leaves the header unchanged. A failed step stops the plan; keyslot 0 is
never touched, the boot goes on with the recovery key, and the next
boot either releases the new protector, whose plan removes what is left
over, or reaches recovery again. Whatever the reseal did, and when it is
declined or not offered, the mapping then opens on keyslot 0 alone
(`open --key-slot 0`) and the volume key is taken as above, both with
the recovery key; `luksDump`'s `--key-slot` cannot restrict which
keyslot opens, so the dump tries the key as cryptsetup chooses. A failed
open or volume-key dump after keyslot 0 opened fails the boot as on the
released path: td-boot exits, init's exit panics the kernel, `panic=-1`
reboots, and the next boot prompts again. A completed reseal persists
across that reboot, since it ran before the open, so the next boot may
release its protector instead. The recovery key, its passphrase text,
each entry and the new protector's secret are zeroed when dropped, on
every path.

An unencrypted volume's boot is behaviour-identical to one before this
tier, not byte-identical: cryptsetup enters both initramfs and the boot
binaries change, but an unencrypted volume's boot makes no TPM contact
and takes the same steps, beyond `mount-root`'s removal of a key member
and an `initrd.image` that are not there. On a machine with a TPM, the
live selector's cap is the one change a live boot shows.

## Keyboard console

This section is increment 7's, and current: it gives the selector's
recovery flow a screen and keyboard beside the serial line, which stays.
What the installer does with it is "Activation", also current.

The built-in command line (DESIGN.md "Full-system volume consumers")
names `console=tty0` before `console=ttyS0,115200`. Linux writes its
messages to every console the command line names and makes the last one
`/dev/console`, so kernel messages reach both the foreground virtual
terminal (VT) and the serial line, while `/dev/console`, and with it the
standard streams of both initramfs' init, stays `ttyS0`: serial
diagnostics and the oracles' console capture and typing are unchanged.
The prefix appears once per kernel entry, so the selector-to-deployment
`kexec` carries `console=tty0 `, 13 bytes with its separator, twice,
inside the command-line budget DESIGN.md states.

The kernel carries the firmware framebuffer: `CONFIG_SYSFB_SIMPLEFB`
presents a UEFI GOP framebuffer whose pixel format simple-framebuffer can
describe as a `simple-framebuffer` device, and `CONFIG_DRM_SIMPLEDRM`
drives it with DRM's fbdev emulation, so fbcon draws the VT on any such
display before, or without, a native driver. Both are pinned and guarded
over the resolved configuration as the other console and display symbols
are (the linux-x86-64 recipe); Linux 7.1.4's `DRM_EFIDRM` and
`DRM_VESADRM` require `SYSFB_SIMPLEFB` off, so neither is built, and
the legacy `FB_EFI` and `FB_VESA`, which would take a mode
simple-framebuffer cannot describe, stay off; the recipe refuses any of
the four set. A native
driver takes the display over. In Linux 7.1.4 `sysfb_init` is a device
initcall in `drivers/firmware`, linked after the GPU drivers, and
virtio-gpu (as virtio-vga) and i915 remove a conflicting firmware
framebuffer and disable sysfb when they probe its device; so on a
machine they drive, simpledrm never binds, fbcon draws on the native
driver's framebuffer and that card is `card0`. A native driver whose
probe is deferred past that point removes simpledrm's device when it
binds, and fbcon moves to it, but its card may then not be `card0`:
DRM takes the lowest free minor, and simpledrm's is freed only once
nothing holds it open. On a
GOP display td has no driver for, simpledrm's card is `card0`
(td-compositor/DESIGN.md says what the compositor does with it). A GOP
mode simple-framebuffer cannot describe gets no framebuffer and no
fbcon.

The VT uses the kernel's built-in keymap, `defkeymap.map`, which is US,
whatever layout the installed system uses: nothing in the selector loads
a keymap. The kernel reads key positions, not legends (PS/2 scan codes
and USB HID usages both name positions), and the keymap turns the top
row's keys, unshifted, into `1` to `0` from left to right. So a keyboard
whose top row carries those digits in that order, shifted or not (AZERTY
shifts them), enters the recovery key's digits whatever its printed
layout. The selector does not change Num Lock. The kernel starts the VT
with it as the boot parameters report it, which on td's firmware entry,
the EFI stub's zeroed parameters, is off; with it off the keypad's digit
keys send cursor-key escape sequences, which td-protector's codec
refuses by byte, prompting again, and pressing Num Lock makes them
digits. The reseal answer is read under the same keymap, so on a
keyboard whose letters sit elsewhere (AZERTY's A and Q) `reseal` is
typed at its US key positions, which td-boot's mirrored line before the
question says; a mistyped answer declines, the safe answer, which leaves
the header unchanged.

Which terminal `/dev/console` is cannot be learned from its descriptor:
`fstat` on any open of `/dev/console` reports the console device, 5:1,
never the terminal behind it, and `/dev/tty1` is 4:1. Both td-boot and
secret-line therefore read `/sys/class/tty/console/active`, up to 4096
bytes, one sysfs page, whose last space-separated entry names the
terminal `/dev/console` is (Linux 7.1.4's `show_cons_active` lists the
enabled consoles with `/dev/console`'s last). An entry `tty1`, or
`tty0`, which names the foreground VT and so tty1 here, means the VT is
`/dev/console`; any other name means it is not. Both initramfs mount
sysfs before td-boot runs, and no ioctl is needed for this.

td-boot writes each console line to standard error as today and, in both
initramfs, the same bytes to `/dev/tty1`, opened once, write-only,
`O_NOCTTY` and `O_NONBLOCK`: in the selector's `on-volume boot` and
`live-boot`, and in the deployment initramfs's `on-volume mount-root`,
`on-volume mount-var`, `root-loop`, `live-root` and `live-seed`, the
verbs those inits run. No other run mirrors, so the running system's VT,
which the compositor's display covers, gets nothing from td-boot. A line
is mirrored whole, after its standard-error write. There is no mirror
when `/dev/console` is the VT, or when `active` cannot be read or
parsed, so no line is written twice to one terminal. Nothing waits on
the mirror: a mirrored write that would block or is short (a VT stopped
by Scroll Lock or `^S`) is abandoned for that line, and the next line is
tried afresh, so a stopped VT misses lines only while it is stopped; any
other failed write that is not a hard error skips its line the same way. A
mirror that cannot be opened, or whose write fails with a hard error
(`EIO`, `ENXIO` or `EBADF`), is said once on standard error and dropped
for the rest of the run. td-boot's own standard-error writes stay
blocking, as today, so a serial line held by flow control
(XOFF from a console server) holds td-boot at its next line, as it does
today, and so holds the VT's prompt too, which td-boot has not yet
reached: that step is not bounded. Nothing td-boot prints carries a
secret, mirrored or not: the recovery key reaches it only through
secret-line's pipe, and protector secrets and the volume key only
through descriptors. Before its first recovery prompt td-boot prints one
line naming the entry: the 48 digits on the keyboard's top row,
unshifted, spaces or hyphens between groups optional, keypad digits only
with Num Lock on. Before the reseal question it says that the answer is
typed at its US key positions. The prompts themselves, `td recovery
key: ` and the reseal question's, are unchanged.

The kernel keeps writing its own messages to both consoles at the default
console loglevel, so a late one (a USB hot-plug, a driver probe) can
print over a prompt on either line. Nothing re-prints the prompt: the
entry being typed is unaffected, since the line discipline holds it and
echo is off, and no message the kernel prints carries what was typed.

td-init's secret-line keeps its one operand and takes no device operand.
It opens two fixed lines, each read-write, `O_NOCTTY` and `O_NONBLOCK`:
`/dev/console`, which the command line makes the serial console, and
`/dev/tty1`, the first VT. That is the foreground VT, since nothing in
either initramfs switches VTs; `/dev/tty0` would name whichever VT is
foreground when opened, so the applet names tty1 to fix the identity.
Its node is devtmpfs's: both initramfs mount devtmpfs on `/dev` first,
which shadows
the cpio's own `/dev`, and the VT driver registers `tty1` whenever
`CONFIG_VT` is set, with or without a display or keyboard, so the
selector cpio needs none, and the increment's check requires the node in
the booted selector. When `active` says `/dev/console` is the VT (a
firmware load option naming `console=tty0` last does that), the two
names are one terminal and the applet opens `/dev/console` alone as its
one line; so it does when `active` cannot be read or parsed, which is
today's single line. Termios is saved once per terminal, before anything
changes it, so a restore never writes back settings the applet itself
silenced.

No step waits on one line's output while the other could be read, so a
line stopped by flow control (a VT stopped by Scroll Lock or `^S`, a
serial line held by XOFF from a console server) cannot stop entry on the
other, and no path halts on a transient stop. Increment 6's draining
`TCSETSF` was replaced, with one line or two, by a plain `TCSETS` with
its readback and a non-draining input flush, `TCFLSH` with `TCIFLUSH`.
For each line, serial first, echo-off is increment 6's patch applied
with `TCSETS` and read back, then that flush, which discards what was
typed before it, under either setting; what arrives after it is not
echoed and is read. Only once every line it kept is silent and flushed
does the applet write the prompt to each, so a key typed on a line
before that line's flush is discarded. A line that cannot be opened, or
whose settings cannot be read, is skipped untouched; one whose settings
cannot be set or read back as computed is skipped with its settings
restored and its input flushed; a note naming either goes to the line
that remains. A VT that opens but has no display or keyboard is kept; it
never completes an entry. With no line left the applet fails with status
1, which td-boot treats as any other failure of the applet: it refuses
boot and halts.

Prompt and note bytes are queued per line and written through the
non-blocking descriptors. A write that would block (`EAGAIN`) or is
interrupted (`EINTR`) leaves the line's queue as it was, and a short
write leaves the rest from its offset; either way the line stays
readable and its queue is finished when `poll(2)` reports `POLLOUT` for
it. A line is dropped only on a hard error: a write failing with `EIO`
(which a hung-up terminal also returns), `ENXIO` or `EBADF`, or `poll`
reporting `POLLERR` or `POLLNVAL` for it without `POLLHUP` (a hung-up
terminal reports `POLLERR` beside its `POLLHUP`, and is read, as below).
Any other write error, or a write that takes nothing, abandons that
line's queue and keeps the line readable, so it neither drops the line
nor spins on `POLLOUT`. A dropped line is restored and flushed as below,
at once and best effort, since a line gone with `EIO` may refuse its
restore, which the note then names; a note is queued on the line that
remains, before its prompt if none of the prompt was written, else on a
line of its own with the prompt again, and with no line left the applet
fails as above. Only the restores of the lines kept, and their flushes,
decide whether the entry stands. A stopped line therefore only delays
its own prompt, which appears when the line resumes, while what is typed
on it is still read.

The applet waits in that one `poll(2)` loop, with no timeout, asking
`POLLIN` of every line and `POLLOUT` of every line with queued bytes. A
canonical line is readable only once a whole record is queued or it hung
up (Linux 7.1.4's n_tty reports input only up to its canonical head), so
the wait ends at the first line that completes a record, by newline or
`^D`; a `POLLHUP` is read, so a hang-up is end of input as today rather
than a dropped line. When both lines are readable at once, the serial
line is read. Queued prompt bytes still unwritten when a record arrives
are discarded with the wait. That line's one record is read and judged
exactly as today, per line: the 256-byte bound, a longer record or one
holding a newline refused whole (status 4), and end of input at `^D` or
a hang-up (status 3; 1 where the hung-up line then refuses its restore,
as a `vhangup`ed terminal answers `EIO`, while a pseudo-terminal whose
master closed may still take it). Then every line is restored with
`TCSETS` and read back exactly, and every line, the one read included,
is then flushed with `TCIFLUSH`, which discards what was typed there
with echo off, partial or whole, and on the line read anything after its
record (a second record from a pasted key's CR LF, or keys typed after
the entry), so no unechoed digits stay queued for a later reader on any
line. A restore and a flush are each attempted whatever the other did,
here and wherever a line is dropped or skipped, and either failing fails
the run. Each line then gets one attempt at its newline, non-blocking:
echo is back on and the newline is cosmetic, so a line that is stopped
or takes it short is not waited on, since waiting there would hold an
entry already made on the other line. Only once every restore and flush
succeeded is the line written to the pipe. The reseal question runs the
same way, on both lines.

On one line this changed increment 6's applet only in what it waits in.
That applet wrote the prompt with a blocking write, so a line held by
XOFF waited there for XON before anything was read. In the loop a lone
stopped line still waits, now in `poll(2)`, until it resumes or a
record arrives, and keys typed meanwhile are read; and the trailing
newline, which then blocked with echo back on, is attempted once. Its
guarantees stay: echo off before the prompt, input before the flush
discarded, the same record rules and statuses, and the restore on every
path. No path, on one line or two, halts on a transient XOFF.

End of input on either line ends the entry and halts the boot, as today,
and halting keeps a line that answers end of input at once from spinning
the prompt. It cannot cut short a person typing on the other line,
because an idle line never reads end of input: the kernel starts its
serial lines with modem control off (`CLOCAL`), so a serial line with
nothing attached delivers no byte and no hang-up and its read blocks,
and a VT with no keyboard blocks the same way. End of input therefore
comes only from a `^D` someone typed or a line that hung up.

`poll(2)` is td-init's eleventh syscall and `TCFLSH` the ioctl request
that replaced `TCSETSF`, which left the roster with no caller, so it
stays at six (UNSAFE.md §3). These commits change what a boot shows,
never what it does: an unencrypted volume's boot still makes no TPM
contact and takes the same steps.

## Activation

This section is increment 7's, and current.

`serve` takes no storage operand. Once its admission checks pass and
before its greeting, it runs two probes, each once, and records both
outcomes in every plan (INSTALLER.md "Storage choice"):

- the TPM probe: PCR_Read of PCRs 4 and 9 in the SHA-256 bank answers
  that bank, and neither value is zero. It records rather than refuses,
  and runs under a 3-second deadline, so that a slow TPM cannot outlast
  the installer's ten-second greeting wait: a read still unanswered then
  is abandoned and recorded as not passed, and that service makes no
  further TPM use, since its plans are unencrypted.
- the keyboard-console probe of the running live system, through sysfs:
  some `/sys/class/vtconsole/vtcon*` whose `name` reads exactly
  `(M) frame buffer device` and a newline and whose `bind` reads `1` and
  a newline (fbcon is bound to the VT; Linux 7.1.4's `vt.c` prints `(M)`
  for every driver `do_register_con_driver` registers, fbcon built in
  included, and `(S)` only for the boot console, such as `(S) dummy
  device`), and some
  `/sys/class/input/input*` whose `capabilities/key` bitmap sets
  `KEY_ENTER` (28) and `KEY_1` to `KEY_0` (2 to 11), the keys an entry
  needs. That bitmap is Linux 7.1.4's `input_print_bitmap` on a 64-bit
  kernel: one to twelve words (`KEY_MAX` is 767), each 1 to 16 lowercase
  hexadecimal digits without `0x` and, as `%lx` writes them, without a
  leading zero digit, separated by single spaces and ended by one
  newline; the most significant word comes first, leading zero words
  are omitted, a later zero word is written `0`, the last word holds
  bits 0 to 63, and an empty bitmap is `0` alone. The needed keys must
  all be set on one device. Each attribute is read up to 4096 bytes,
  one sysfs page; one that is missing, longer, unreadable or outside
  those grammars is malformed and counts as absent, never present, and
  a listing that fails, or that holds more than 64 `vtconsole` or 4096
  `input` entries, fails the probe. Power and sleep buttons, lid
  switches and the PC speaker advertise none of the needed keys, so
  they never pass it.

Every plan of that service names device-bound storage when both probes
passed and unencrypted storage otherwise. Neither outcome refuses a
start, a proposal or an execution: a machine without a usable TPM, or
without a keyboard console, installs unencrypted under the disclosed
plan, and one with both installs device-bound. Execution does not probe
again; the TPM's state may change before execution, which seals under
its own policy, as today.

The keyboard-console probe is a proxy for the selector. The live medium
boots the kernel an installed selector runs, with the same built-in
command line, on the same hardware, and every driver is built in, so a
framebuffer console and a keyboard the live system has at the probe are
ones the selector will find. It sees only what is attached then. A
display or keyboard removed or changed between installation and a later
recovery (a USB keyboard unplugged, a display moved to a GPU with
neither UEFI GOP nor a td driver) leaves a recovery the keyboard console
cannot take; the serial console remains where the machine has one, and
the live medium with the recovery key opens the volume for data access.
The review discloses that limit. A keyboard attached only after the
probe is not seen, and the plan stays unencrypted. The keyboard half
reads advertised keys, not a keyboard a person types on: a HID device
that advertises a keyboard's keys without being one, such as a security
token's one-time-password interface, a wireless receiver with no
keyboard paired or a barcode scanner, passes it, and the disclosed
limit then applies.

Automatic login remains and stays disclosed, and td-authd passes no
storage operand: there is none. What remains deferred is hardware the
probe refuses and so installs unencrypted: a display with neither a UEFI
GOP framebuffer simple-framebuffer can describe nor a td driver, and a
keyboard td's kernel has no driver for (USB keyboards on ports served by
OHCI or UHCI controllers, companions included, and I2C-HID and Bluetooth
keyboards); and a keymap
other than US. Activation leaves the selectors as they are: the
installed selector on an unencrypted volume makes no TPM contact, and
the live selector caps as MEDIA.md says.

## Authentication and recovery

This section and the next govern the protected tier except where they
name the device-bound tier; "Protected tier" below is increment 8's
specification of how, none of it current. Protected unlock is **TPM 2.0
plus PIN**. The PIN authorizes a hardware-held secret with persistent
dictionary-attack protection, not a short LUKS passphrase susceptible to
offline guessing. Release also requires an approved measured boot state.
In this tier TPM possession alone never logs a person in. An enrolled
**FIDO2 token plus PIN** is an alternative primary method and the
recovery method when the TPM is lost or replaced. Require the token's
`hmac-secret` capability and user verification; touch alone is
insufficient. These are alternative protectors, not a requirement to
present both devices. The recovery key remains a recovery protector
beside them; "Protectors and shapes" states the minimums every shape
keeps.

Enrollment generates a random volume key on the machine, binds protectors
to an explicit account, and verifies primary and recovery unlock before
declaring success. Recovery must not require the failed TPM or its old
PCR state. It enters a
trusted recovery flow, not an automatic desktop login; replacing a protector
requires fresh authentication. Never silently create a plaintext fallback,
clear a TPM, reset a token, or discard the last working protector.

Keep the PIN, volume key and authentication proofs out of recipes, store
outputs, logs, argv, environment variables and persistent temporary files.
Public enrollment metadata and wrapped key material may remain outside the
encrypted volume so that unlock is not circular. Treat them as untrusted
bounded input. Protect enrollment updates against interruption, and document
that old disk/header backups can retain revoked protectors.

One verified primary authentication unlocks storage and admits its enrolled
account to one fresh session. Carry account identity and a non-replayable
authentication result across boot stages; a mounted disk is not login proof.
Resume and screen unlock require authentication again, without admitting a
new desktop or revealing the old one first. No disk swap or hibernation is
enabled by this workstream until encrypted resume and key lifetime are
specified and tested. Existing TPM application-secret enrollment is a
different format and policy; it is not a disk protector or recovery path.

## Boot and authority boundaries

In the protected tier, firmware measures the selector EFI image into PCR
4, and the EFI stub its initramfs and any load options into PCR 9,
before selector-stage release, which requires those exact values and PCR
12 at zero ("Protected release"). Firmware-enforced authentication of
that entry, UEFI Secure Boot under the machine's own keys with the
selector's initramfs inside the signed image, is the optional "Firmware
authentication" increment; it does not gate activation. Without it the
tier authenticates no pre-selector code, as the device-bound tier does
not: a boot chain changed by someone holding the machine is refused by
release, not by firmware, and a counterfeit selector can still record
the PIN ("Protected release", "Threat and limits"). The trusted selector
authenticates the selected deployment after opening the volume; it
cannot require a measurement of unreadable deployment bytes to unlock
that volume. It then measures the verified deployment and its boot
arguments into PCR 11 before the handoff, its prepared selector carrying
the measurement policy ("Selector update"), and hands the volume key
off: there is no second-stage release, so no policy binds PCR 11, and
deployment updates and rollbacks never change release values. `current`
and the approved `previous` fallback are therefore both preserved by
construction. Its update policy is td's own selector update, which the
installed selector predicts and seals ahead before its cap, and its
recovery policy for any other change is an enrolled FIDO2 token with its
PIN, or the recovery key, and a confirmed selector-stage reseal
("Changed boot chains and updates"), so its exact-PCR enrollment ships
with both. The device-bound tier releases on exact PolicyPCR without
PolicyAuthorize too. Its update and recovery policy is the recovery key
with a confirmed selector-stage reseal. It releases only at the selector
stage, so deployment updates never change its release values.

The [selector measurement prerequisite](DESIGN.md#selector-deployment-measurement-prerequisite)
now records the verified deployment and exact handoff arguments in PCR 11
when configured in the selector itself. The full-system QEMU oracle verifies
that PCR after kexec. Its direct-kernel entry remains host-trusted; it does
not establish authenticated firmware entry, authorize updates, or enable any
disk/application protector. PCR 11 alone cannot satisfy this boot contract.

The selector must unlock before reading a deployment. Its dm-crypt mapping
does not survive `kexec`: the deployment initramfs must create it again.
Before enabling encrypted boot, specify and prove either re-release under
the second kernel's policy or a bounded volatile key handoff to the verified
deployment. That protocol must preserve a single user interaction, verify
the original signed deployment bytes, and retire intermediate secrets. A
plaintext key on the command line or in a stored initramfs is forbidden.
This handoff is an explicit implementation gate, not existing machinery.

The device-bound tier uses a bounded volatile handoff, which the protected
tier adopts, adding the admission record beside the key ("Verified
account handoff"). After authenticating the selected deployment and
making its configured PCR 11 measurement, the selector copies the
verified deployment initramfs into an unlinked memfd, verifies that copy
against the manifest, appends one 4-byte-aligned cpio archive holding
only the volume key (and, on a protected volume, the admission record),
seals the
memfd and passes it to `kexec_file_load`. The appended archive is outside
the signed manifest and the measured event. Its single member's name and
the 64-byte key length are td-boot protocol constants and a permanent v1
contract: an installed selector may never be updated (increment 8's
selector update is a consented operation, not an automatic one), so it
hands every later deployment the same format, and a later selector keeps
it. A deployment from before
increment 6 on an encrypted volume has no key reader; it fails closed.
Its initramfs finds no Btrfs volume under the handed-off UUID, or
refuses the LUKS2 one as not yet supported, so its init fails and
`panic=-1` reboots. Such a deployment can be selected only as a pending
update, whose attempt record the selector consumes on each boot it
selects it; once the record is exhausted the selector selects
`previous`, which carries no record. A deployment without one was
acknowledged after booting on this volume, so it read the key, or was
installed together with the selector from its own image, so it is as
new as that selector. An acknowledged deployment's own refusals repeat
on every boot (below).

The member is `td-volume-key-v1` at the root of the rootfs, a regular
file of mode 0400 owned by root holding exactly the 64 key bytes; a later
format takes a new name beside it. td-kexec builds the memfd as
`td-kexec --fds-key DIGEST /proc/PID/fd/N CMDLINE`: the kernel and the
verified initramfs on descriptors 0 and 1, as `--fds`, `DIGEST` the
initramfs digest td-boot verified from the manifest, and the key on a
pipe the caller holds, named as td-install names one to cryptsetup. The
caller writes all 64 bytes and closes every write end of that pipe
before it starts td-kexec; td-boot does so after its PCR 11 measurement
with td-protector's `KeyFile`, the runner's key-file pipe, and holds the
read end until td-kexec returns. td-kexec opens the name anew without blocking,
refuses anything but a pipe, and requires exactly 64 bytes and end of
file: a read that would wait, because a write end is still open, refuses
rather than hang the selector. td-kexec refuses a `CMDLINE` naming
`retain_initrd` or `keepinitrd` (quotes removed, `-` and `_` alike, as
the kernel parses them), which would keep the initramfs and so the key
readable at `/sys/firmware/initrd`. It creates the memfd with
`MFD_NOEXEC_SEAL`, so the copy can never be executed, and copies the
initramfs into it with positioned reads, hashing what it writes; it
reads the key only after the copy's digest equals `DIGEST`, so a
mismatch refuses with no key read. It then writes NUL padding to a
4-byte boundary and the archive, the key straight from its one buffer
between the archive's header and trailer, zeroes that buffer, and adds
the four seals before `kexec_file_load` takes the memfd and the
unchanged command line. It writes the memfd through a second open of it,
closed before the seals, because `kexec_file_load` refuses an initramfs
open for writing (Linux 7.1.4 `kernel_read_file`'s `deny_write_access`),
which memfd_create's own descriptor does not count as; a test-only read
lease, which the kernel refuses on the same write count, shows no writer
left. The initramfs, and the copy with the archive, are bounded by
`kexec_file_load`'s 4 GiB.

Once the key is in the memfd, any later failure (writing the archive's
trailer, the length check, the seals, `kexec_file_load`, or `reboot`
returning) leaves it there until td-kexec exits, and the memfd's pages
are then freed without being zeroed. A `kexec_file_load` that fails with
ENOMEM is retried once after td-kexec drops the clean page cache, which
leaves the memfd alone; the failed attempt's buffers are freed without
being zeroed. A loaded kexec image whose `reboot` fails keeps its copy
of the key in the staged segments. Each is a copy in memory of the kind
described below, within the memory-extraction residue that Scope
excludes. The retry reads the kernel again from the device after
td-boot's hash, and the initramfs too except under `--fds-key`, whose
memfd is not dropped: the payload-reread residual `MEDIA.md` "Live
boot" already states.

The deployment initramfs reads the key from its RAM-backed root, opens
the volume by descriptor and removes the key file before starting the
system. When its kernel could not unpack an initramfs, the
`CONFIG_BLK_DEV_RAM` fallback writes the whole initrd, archive and key
included, to `/initrd.image` in that root, so the key reader must remove
that file too before starting the system; DESIGN.md "Full-system volume
consumers" owns how it finds the partition and the active mapping.
`td-boot on-volume mount-root` takes the key first, before it reads the
command line or looks for the volume: it inspects the member without
following a link and opens nothing but a regular file, then requires,
on the open descriptor, the file it inspected, mode 0400, uid 0 and
exactly 64 bytes, read into a buffer zeroed on drop with end of file
after them. It then removes the member and `/initrd.image`, each if
present, whether or not the member was there or passed, so every later
path, refusal included, finds neither; a member that fails a check, or
a removal that fails, refuses. No other operation reads or removes
them. It refuses a key on an unencrypted volume and an encrypted volume
without a key, and an encrypted volume whose mapping is already active,
since the selector's does not survive `kexec`. Each refusal is an
ordinary `on-volume` error: td-boot exits non-zero, the deployment
`/init` exits under `set -e`, the kernel panics on init's exit and
`panic=-1` reboots at once; nothing starts the system. As for an
unencrypted volume that fails to mount, the reboot spends a pending
deployment's boot attempts, so the selector falls back to `previous`
once they are gone. An acknowledged deployment has no countdown, so a
refusal that repeats on every boot, such as a key handed for an
unencrypted volume, a malformed member or a key cryptsetup rejects
against the header's digest, reboots without end, each cycle running
the selector's release again, until the volume or the selector is
repaired from the live medium.

Then, as defence in depth and before cryptsetup runs, it attempts to
unseal each td token the selector would try (td-protector's release
candidates: none naming keyslot 0, at most four), since the selector's
cap must already be closed. It reads the header after the selector's
step-5 transitions, so its tokens are the header's tokens now. Without
a TPM device (`/dev/tpmrm0` absent, with no wait) it attempts nothing,
so a TPM that appears later goes unchecked ("Device-bound default"). A
header td's reader refuses gives the check no tokens, so it attempts
nothing either and the volume opens. A policy
refusal and a load refusal (td-protector "Unseal outcomes") release
nothing: the closed cap causes the first, and a cleared or different
TPM, or an owner hierarchy given a password or disabled since
installation, the second for tokens a recovery boot kept. Nor does a
TPM without a SHA-256 PCR bank (`NoSha256Bank`), whose selector reached
recovery for the same reason: no SHA-256 PolicyPCR can be met, so a
device-bound token's policy read sends nothing to unseal. Any release,
or any
other outcome, a transport error, a device that will not open or a
command the TPM did not answer included, zeroes what it released and
the key and halts on the console, as the selector's failed cap does; it
never exits init, and tries no later token. That attempt is the
installed path's evidence of an unseal after the cap. Only then does
td-protector's runner run `/bin/cryptsetup open --type luks2
--volume-key-file /proc/<pid>/fd/K /proc/<pid>/fd/N td-system`: the key
on a `KeyFile` pipe, nothing on standard input, the partition named by
its held descriptor. cryptsetup 2.8.8 reads the header's volume-key size
from that pipe and activates only a key matching the header's digest of
the data segment. The key is zeroed once it is in the pipe, before
cryptsetup runs; a mapping left open by a later refusal ends with the
reboot. Neither stage writes the key to any block device. Removing the
file retires the key from the filesystem only: copies remain in the
selector's memfd pages, the freed pages of its volume-key file and of
its `KeyFile` pipe's buffer, the `kexec` segments, the second kernel's
freed initrd region and the deployment initramfs's key pipe's freed
pages, which memory
extraction, outside Scope, could read. td-kexec's memfd and sealing
syscalls are recorded in UNSAFE.md §1.

The authenticated user gets an ordinary session. Later elevation uses the
existing operation-to-principal policy and secure-attention path: one
request, one explicit approval, one broker-performed operation. Applications
receive no general root process or reusable authority. Protected consent is
the normal interaction; changing unlock credentials or recovery policy
requires fresh hardware-backed PIN verification bound to that operation.
No client surface, synthetic input, remote-control interface, or untrusted
same-uid process may impersonate the trusted UI or approve a request.

## Protected tier

This section is increment 8's target and none of it is current; item 9
activates it. It specifies the protected tier that "Authentication and
recovery" and "Boot and authority boundaries" require, over the
device-bound tier's volume format, release order, PCR 12 cap and
volatile handoff, which it keeps. td-protector carries its policies,
token formats and planner (td-protector/DESIGN.md, "Protected roles
(planned)"), and td-tpm the commands it adds (td-tpm/DESIGN.md,
"Planned: protected-tier commands"). The storage operations here that
run on the booted system are refused by td-authd until item 9; root can
run their worker directly, which is how the oracles reach them. The
selector's half is not unreachable before item 9: from 8c on, the
installed selector parses protected headers, and anyone who can write
the disk can forge one. Every sub-increment's selector code therefore
fails closed on a forged header: it releases only through a protector
it verified, writes an admission record only after such a release,
and kills a keyslot or removes a token only by the planner's rules.

### Protectors and shapes

A protected volume's protectors are keyslots of four kinds:

- **tpm-pin**: a 32-byte `/dev/random` secret sealed by the TPM under
  exact PolicyPCR over the SHA-256 bank (PCR 4 and PCR 9 at their
  expected values, PCR 7 too when the measured boot shows Secure Boot
  enabled, "Firmware authentication", and PCR 12 at its literal reset
  value of zero), then PolicyAuthValue, then
  PolicyCommandCode(Unseal). Its authValue derives from a PIN ("PIN and
  dictionary-attack policy"), and its noDA attribute is clear, so a
  wrong PIN counts against the TPM's lockout.
- **fido2-primary** and **fido2-recovery**: a passphrase derived from an
  enrolled FIDO2 credential's hmac-secret output under user
  verification ("FIDO2 protectors"). No TPM takes part.
- **recovery key**: the device-bound tier's 48-digit recovery key, kept
  through the upgrade and rewrapped by its reencryption, its keyslot
  named by a `recovery-key` marker token.

Every td token names the account it admits, the primary account at UID
1000, the one principal td-login/TOKEN-LOGIN.md's tier has. A volume
has one of two shapes, and this is the one place their minimums are
stated:

- **TPM primary**: one tpm-pin protector, the recovery key, and zero to
  four fido2-recovery tokens.
- **FIDO2 primary**: one to three fido2-primary and zero to three
  fido2-recovery tokens, at most four FIDO2 tokens, and either at least
  two FIDO2 tokens or the recovery key, so that one lost token is never
  the last way in (AGENTS.md principle 7).

The recovery key is removable only in the FIDO2 shape while two FIDO2
tokens remain ("Protector management"). The enrollment screens recommend
a FIDO2 recovery token beside the recovery key, and say that td asks for
separate keys but cannot prove distinct hardware; exclusion lists keep
the credentials distinct. Anyone holding an enrolled token and its PIN,
or the recovery key, holds full authority over the volume's protectors,
as an enrolled login key does over the login record. A release by a
primary protector (tpm-pin, fido2-primary) admits one session ("Verified
account handoff"); a release by a fido2-recovery token or the recovery
key unlocks storage and admits none.

### PIN and dictionary-attack policy

The TPM PIN is 6 to 63 bytes, each printable ASCII from 0x20 to 0x7e,
space included, taken exactly as typed. Both paths that read it go by
key position on the US layout: the selector's consoles under the
kernel's built-in keymap ("Keyboard console") and the compositor's PIN
field under its `us` keymap (td-compositor/DESIGN.md, "The PIN field"),
so the same keys give the same PIN whatever their caps show; the
enrollment and change screens say so, and that a PIN of digits alone
avoids the question. td-protector's PIN codec refuses any other entry
before the TPM sees it, so a malformed entry costs no attempt. A FIDO2
PIN is the token's own, under TOKEN-LOGIN.md's 4-to-63-byte
printable-ASCII profile; td never sets, changes or resets one.

A tpm-pin object's authValue is HMAC-SHA256 keyed with its 32-byte salt
over `td/disk-protector/pin/v1`, one zero byte and the PIN. The salt is
random per sealed object and public in its token, so two objects sealed
under one PIN have unrelated authValues; it slows no guess, which the
TPM's lockout bounds.

**Bus.** Every command a PIN or td's lockout authorization authorizes,
and every Unseal, runs in a session salted to td's storage primary:
td-tpm encrypts the session salt to the primary's ECC P-256 key (an
ephemeral ECDH key and KDFe), so the session key is unknown to anyone
recording the bus, and sets the session's encrypt and decrypt
attributes with AES-128 in CFB mode, so Unseal's response, the
protector secret, and a new authorization sent to the TPM cross it
encrypted. The command HMAC is keyed with that session key and the
authValue, so a recording gives no offline test of the PIN. The
primary's Name is recorded in each tpm-pin and lockout token when it is
first written and must match at every later use: trust on first use. An
interposer present at that first seal defeats it; one that appears
later is refused as a changed TPM. td-tpm takes P-256 and AES from
td-fido by path ("FIDO2 protectors"); the kernel's own TPM bus
protection (`TCG_TPM2_HMAC`) stays off as pinned.

**Lockout.** The TPM's dictionary-attack lockout is the only guess
limit: td keeps no counter of its own, and the TPM keeps its counter in
non-volatile state. td sets maxTries 32, recoveryTime 600 seconds and
lockoutRecovery 86400 seconds, and holds the lockout hierarchy's
authorization, without which these bound nothing: anyone who boots
another system on the machine could reset the counter
(TPM2_DictionaryAttackLockReset) after each lockout and return to the
genuine prompt. td keeps it as a `lockout` token, encrypted under the
volume key:

- The authorization `L` is 32 bytes from `/dev/random`. A write draws a
  fresh 32-byte nonce and derives 64 bytes, `k_enc` then `k_mac`, by
  HKDF-SHA256 with the volume key as input keying material, an empty
  salt and the info `td/disk-protector/lockout/v1`, a zero byte, the
  volume's 16-byte UUID and the nonce. The token holds the nonce, `L`
  XORed with `k_enc`, the storage primary's Name, and a tag,
  HMAC-SHA256 under `k_mac` over the nonce, that ciphertext and the
  Name; td-protector verifies the tag in constant time before it uses
  `L`. td reads it only in the selector, after the cap, with the volume
  key in hand; td's code on the running system and in the deployment
  initramfs never decrypts it, though root on the running system,
  which can read the volume key from the device-mapper table, could.
- **Taking it** runs after the cap and only after the released
  keyslot's test passed, so a boot that has proven nothing never changes
  TPM state irreversibly; the lockout hierarchy binds no PCR. With
  `TPM_PT_PERMANENT`'s `lockoutAuthSet` clear, td first imports the
  token with a fresh `L`, replacing any token present, then sends
  TPM2_HierarchyChangeAuth for `TPM_RH_LOCKOUT` from the empty
  authorization to `L`, TPM2_DictionaryAttackParameters with the three
  values under `L`, and TPM2_DictionaryAttackLockReset under `L`, whose
  success proves possession. A crash before the change leaves a token
  the next take replaces; one after it leaves the token holding the `L`
  the TPM now has.
- **Proving it**, with `lockoutAuthSet` set, needs a token whose tag
  verifies under this volume key and whose Name is this TPM's primary:
  td sends TPM2_DictionaryAttackParameters with the three values under
  `L`, which also repairs them. Anything else is a TPM td cannot prove:
  `lockoutAuthSet` set without a token (another system, such as a
  Windows installation, provisioned it), a tag that fails, another
  primary, or a refused proof. td never matches parameters instead. It
  refuses the TPM-primary shape on such a TPM, naming the reason, and
  sends nothing further to its lockout hierarchy that boot; a refused
  proof costs that hierarchy's one attempt, which the TPM's own
  lockoutRecovery bounds. The FIDO2 shape and the recovery key remain,
  and a TPM clear from firmware makes the TPM eligible again. A second
  td installation on the same TPM, another disk or a reinstallation,
  therefore finds the lockout taken by the first and is refused the
  TPM shape until the TPM is cleared; the screens say so.
- Before it seals a new tpm-pin object, before the cap, the selector
  reads `TPM_PT_PERMANENT`, which needs no authorization: with
  `lockoutAuthSet` set and no lockout token it asks no new PIN and seals
  nothing. Otherwise it seals and verifies before the cap, and a take or
  proof that then fails after the cap discards the object before its
  import.
- **Counter.** After any release whose keyslot test passed, a non-zero
  `TPM_PT_LOCKOUT_COUNTER` is printed (`td: the TPM counted N wrong PINs
  since its last reset`), a sign of guessing when the owner made no
  mistakes. After a fido2 or recovery-key release, td then resets the
  counter under `L`, so a person locked out recovers with a token or the
  recovery key; a PIN release leaves it to decay.
- **Clearing.** A TPM clear requested from the running system
  ("Protector management") is TPM2_Clear under `L`, run by the selector
  after a release.

A wrong PIN fails with `TPM_RC_AUTH_FAIL` on Unseal's session (0x98e), a
locked-out TPM with `TPM_RC_LOCKOUT` (0x921). The TPM compares a policy
session's digest with the object's authPolicy before it checks the
session's HMAC, so a changed chain and a closed cap are refused without
consuming an attempt; the emulator tests pin that order against the
pinned swtpm. Disclosed: after 32 wrong PINs one attempt returns every
ten minutes the TPM is powered; a TPM may count a power loss without an
orderly shutdown as a failure, which shows as fewer attempts; and a
firmware TPM (AMD fTPM, Intel PTT) keeps its counter in the platform's
SPI flash, so someone who can rewrite that flash can roll the counter
back and take 32 more guesses each time. A discrete TPM resists that
attack, and the enrollment screen says which kind the machine has.

### Protected release

The installed selector reads a protected header by its tokens: tpm-pin,
fido2 or recovery-key tokens and no first-boot or device-bound one,
td-protector refusing any mixture outside the upgrade's states
("Re-encrypting upgrade"). Its release order keeps the device-bound
order's rules (no C parser before the cap, the cap after every attempt,
the plan only after the cap, the TPM wait) and replaces steps 2 and 3:

1. td's reader loads the header, as step 1.
2. Before the cap, the release loop runs on the consoles, each entry
   one run of secret-line, read and zeroed as the recovery key is
   ("Selector release"), with no limit on entries; end of input or a
   failed applet halts as there.
   - **Chain check.** In the TPM-primary shape td-protector first checks
     that the tpm-pin object can release here, asking for nothing and
     costing no attempt: the policy digest over the PCRs the token
     names as they read now, a literal-zero PCR 12, PolicyAuthValue and
     Unseal must equal the sealed public area's authPolicy; the storage
     primary's Name must equal the token's; and the TPM must load the
     object, which verifies its private area. With two tpm-pin tokens
     (an update's pre-seal, "Selector update") either may pass.
   - **Warning.** When no tpm-pin object passes although the TPM loads
     one, before any other prompt the console prints `td: WARNING: this
     machine's boot chain is not the one td sealed. Firmware, its
     settings or option ROMs, Secure Boot, or td's selector changed. If
     you did not change them, someone may have: a security key or the
     recovery key used now unlocks the disk for whatever is running.`
     A Load refusal or another primary prints that the TPM was cleared,
     replaced or is being intercepted. No TPM device, no SHA-256 bank
     or a lockout each print their own line. The warning is the genuine
     selector's: a counterfeit selector prints none ("Threat and
     limits").
   - **TPM PIN.** When an object passes and the TPM is not locked out,
     the selector prints how many attempts remain
     (`TPM_PT_MAX_AUTH_FAIL` less `TPM_PT_LOCKOUT_COUNTER`) and prompts
     `td PIN (empty for another way): `. An entry the codec refuses
     prompts again; an admitted one runs Unseal, and a wrong PIN
     prompts again with the new count. In the FIDO2-primary shape the
     loop starts at the next prompt.
   - **Another way.** The prompt is `td: connect one security key and
     press Enter, or type the recovery key: `. An empty entry starts
     the selector's FIDO2 worker ("FIDO2 protectors"), which requires
     exactly one connected FIDO device, identifies its credential among
     the header's fido2 tokens by a silent assertion, reads its PIN
     retries, and prompts `td security key PIN (N attempts left): `;
     after the PIN it prints `td: touch your security key`.
     TOKEN-LOGIN.md's PIN statuses and a key not enrolled here are
     console lines that return to this prompt. Any other entry goes to
     the recovery-key codec, whose refusals prompt again; an admitted
     key is held for its test after the cap, and ends TPM attempts for
     this boot.
   - **Reseal.** When a FIDO2 token or the recovery key was chosen in
     the TPM-primary shape and no tpm-pin object passed (a changed
     chain, a cleared or different TPM, or none on the header), and the
     TPM has a SHA-256 bank with PCR 4 and PCR 9 measured and may be
     sealed to (the pre-cap rule in "Lockout"), the console warns that
     confirming binds release to this boot chain and retires every
     other tpm-pin protector, and asks, as the device-bound reseal does;
     exactly `reseal` confirms. Confirmed, it prompts `td new PIN: ` and
     `td new PIN again: ` until two entries the codec admits are equal,
     seals a fresh secret to the observed values and a literal-zero PCR
     12, and unseals it once with that PIN, requiring the same secret:
     PCR 12 is still zero, so unlike the device-bound reseal this one is
     verified before the cap. A failed seal or verification says so and
     keeps nothing. The FIDO2-primary shape has no tpm-pin protector and
     offers none.
   - **Requests.** After a PIN release, a requested PIN change
     ("Protector management") or else a staged selector update's
     pre-seal ("Selector update") runs here, before the cap; never both
     in one boot, so a staged update waits for the next PIN boot.
3. The cap runs as step 4, whenever a TPM device exists. A FIDO2 token
   and the recovery key release without a TPM, so a selector that found
   none still boots; the cap is then skipped with its console line, and
   PCR 12 stays open to a TPM the kernel exposes later, whose tpm-pin
   protector still needs the PIN.
4. After the cap the released keyslot is tested with its secret: the
   recovery key on the keyslot its marker names, where a wrong key
   prompts again for a security key or the recovery key, since neither
   needs the TPM. The selector then opens the mapping and takes the
   volume key with the released secret, as the device-bound order
   does, and under Secure Boot verifies `TDCONFIG` against the volume
   before its trusted key selects or verifies anything ("Firmware
   authentication"). The plan then runs as step 5: a confirmed
   reseal's keyslot is added under the released secret, its token
   imported and its keyslot tested, then every other tpm-pin keyslot
   and token is retired, and orphans are removed under the protected
   rules (td-protector, "Protected roles"). The lockout take or proof,
   the counter, a requested TPM clear, and a staged update's
   authentication, import and commit follow in that order, each needing
   the volume key or the open volume. The selector then selects, writes
   the admission record ("Verified account handoff"), and hands off as
   today.

The deployment initramfs's post-cap check ("Boot and authority
boundaries") tries each tpm-pin token by PolicyPCR alone, which the
closed cap refuses with `TPM_RC_VALUE` before any authorization: it
never sends Unseal or a PIN, and a PolicyPCR that passes halts as a
release does. A fido2 token and the recovery key release nothing
without a person, so the check attempts neither.

**Threat and limits.** A finder of the powered-off machine, or of its
disk, needs the PIN with this TPM on this boot chain, or an enrolled
token with its PIN, or the recovery key: the TPM gives 32 guesses and
then one per ten minutes, subject to the firmware-TPM rollback above,
and a FIDO2 token its own retry counter. Without "Firmware
authentication", nothing authenticates the selector before it runs.
Anyone who can write the ESP while the machine is away from its owner
can replace `BOOTX64.EFI` or `INITRD` with a look-alike prompt. PCR
binding makes the TPM refuse release to that code, but cannot stop it
recording the PIN, and a second visit can restore the genuine selector
and type the recorded PIN: TPM plus PIN without Secure Boot does not
withstand an attacker with two visits. Secure Boot with a firmware setup
password closes that path; Secure Boot alone does not ("Firmware
authentication"). Nor does the tier protect a running or suspended
machine, whose key is in RAM (Scope), a PIN observed as it is typed, or
a TPM bus interposer present at the first seal.

### FIDO2 protectors

A disk credential is created on TOKEN-LOGIN.md's token profile ("Token
profile": PORTABLE.md's creation codec, ES256, `rk=false`, hmac-secret,
a configured client PIN, `alwaysUv` not true, no built-in UV requested,
signed backup flags and enterprise attestation refused, and getInfo's
versions including `FIDO_2_1`) with relying party `td.invalid` and the
labels `td disk` for the relying party's name and the user's name and
display name. CTAP 2.0's hmac-secret holds one secret per credential, so
an assertion without the PIN returns the same output as one with it,
and a stolen key would open the disk without its PIN; CTAP 2.1 keeps the
two apart. The profile therefore requires `FIDO_2_1`, and creation's
probe adds one assertion with presence and no PIN, which must either
find no credential or return an output different from the repeat's, or
the key is refused as unable to require its PIN. Its creation excludes
every credential the header's fido2 tokens and the operation already
name; it is proved, repeated and probed as TOKEN-LOGIN.md's new key is,
each with its PIN and touch, the repeat reproducing the proof's output
exactly. A credential ID longer than 255 bytes is refused before any
PIN, which bounds the header ("Re-encrypting upgrade"). Login, notebook
and application-store credentials are separate credentials on the same
relying party: each domain has its own client-data domain and salt, and
one physical key may hold any of them.

A fido2 token carries the credential ID, a random 32-byte hmac-secret
salt, the credential's P-256 public key and its role. Its keyslot
passphrase is 32 bytes of HKDF-SHA256 with the hmac-secret output for
that salt as input keying material, an empty salt, and an info string
of `td/disk-protector/fido2/v1`, a zero byte, the volume's 16-byte UUID
and the u32-length-prefixed credential ID; it reaches cryptsetup as a
key file, never argv.

Every assertion's client-data hash is SHA-256 over
`td/disk-protector/operation/v1`, a zero byte, a phase byte, the
u32-length-prefixed canonical description of what it authorizes, and
32 fresh kernel-random bytes. The phases are unlock 1, create 2, prove
3, repeat 4, probe 5 and authorize 6, the last for "Protector
management". td verifies the relying-party hash, UP and UV set, and
the signature over the authenticator data and that hash under the
token's public key with td-fido's software P-256, so FIDO2 release
needs no TPM; then it decrypts the hmac-secret output. The signature
binds the assertion to this operation; the keyslot test after the cap
proves the output.

The selector's FIDO2 client is td-boot's `fido-worker` verb, which
td-boot starts from `/proc/self/exe` as td-secret starts its
`hid-worker`, under td-secret's root admission ("USB token transport":
at most 256 fixed `/dev/hidrawN` names, a root-owned mode-0600
character device, USB HID bus metadata and the FIDO report descriptor
read from sysfs) with its two-minute absolute deadline and independent
watchdog. devtmpfs creates the nodes and the kernel already builds USB,
xHCI, EHCI, HID, hidraw and USB HID in, so the selector loads no module
and needs no new binary under D6. UHID is never on the boot path. The
CTAP code td-secret, td-boot and td-tpm then share (report framing,
CBOR, the CTAP codecs, the PIN protocols, hmac-secret, P-256, AES and
hidraw admission) moves into one std-only sibling crate, `td-fido`,
which forbids `unsafe` (AGENTS.md principle 2); what needs `unsafe`
stays in td-secret.

**Threat and limits.** FIDO2 release binds no measurement, and the
salt is public in the header. Presenting a token and typing its PIN
into an altered boot chain gives the volume away at once: whatever runs
reads the salt, asks the token for the output under that PIN, and
derives the passphrase. Because a TPM-primary machine's ordinary boots
and td's own selector updates ask only for the PIN ("Changed boot
chains and updates"), a request for a token or the recovery key there
is exceptional, and the warning above precedes it; the owner should
present one only knowing why the chain changed. A FIDO2-primary volume
presents its token at every boot and so does not withstand a boot chain
altered between boots; it suits a machine without a usable TPM, and its
enrollment screen says so. A lost token is revoked by removing its
keyslot ("Protector management"); an old header copy still holds it.

### Changed boot chains and updates

Release values change only with the selector or the firmware:
deployment updates, rollback and the `previous` fallback never change
them ("Boot and authority boundaries"). td's own selector updates are
predicted and sealed ahead ("Selector update"), so the boot after one
asks only for the PIN. Everything else that moves PCR 4, PCR 7 or PCR 9
fails the chain check before any PIN is asked and shows the warning: a
firmware update td does not perform, changed firmware settings, option
ROMs or Secure Boot state, a load option, a mispredicted update, and a
cleared or replaced TPM. The owner then releases with an enrolled FIDO2
token or the recovery key and confirms the reseal ("Protected release"),
and later boots take the PIN again. There is no PolicyAuthorize, signing
key or NV counter; every seal is exact PolicyPCR over values the genuine
selector observed or computed.

**Threat and limits.** A changed chain cannot obtain release without a
token or the recovery key; a cleared TPM costs the tpm-pin protector,
never the volume. A firmware update needs one of them at the next boot,
so one must stay reachable.

### Selector update

`td-install storage-operation`'s `selector-update` replaces the ESP's
`EFI/BOOT/BOOTX64.EFI` and `EFI/BOOT/INITRD` with the running
deployment's kernel and a selector prepared from the running root's
stock template (`/lib/td-boot/selector-initramfs.cpio`) by
`prepare-selector`'s rules, with the installation's trusted key as the
volume holds it and the volume UUID of `td.volume=`; on a protected
volume, and for the upgrade, the prepared selector also carries
`etc/td/boot-measurement` (DESIGN.md "Selector deployment measurement
prerequisite"). Under Secure Boot the pair is the signed image and
`TDCONFIG` instead ("Firmware authentication"). It runs only from a
`current` deployment that was acknowledged, with no pending attempt, so
the kernel and template come from a deployment known to boot; both are
read from the verified deployment through the held volume and checked
against its manifest.

td's bounded FAT32 code, the std-only sibling crate `td-fat` that
td-install and td-boot share, admits the held disk's ESP, the td
layout's first GPT partition, only when its `EFI/BOOT` directory's first
sector holds every entry the update touches, as td's layout writes
them, and its free clusters hold the new files beside the old ones;
otherwise nothing is written. Clusters allocated in the FATs but
reachable from no directory entry, an earlier interrupted update's, are
freed first. Each commit is one write of that directory sector, synced,
after the new data is written and chained in both FATs and synced; the
old chains are freed after it. The commit assumes the device writes one
logical sector whole on power loss. NVMe's AWUPF covers at least one
block; SATA and eMMC devices state no such unit, so the assumption is
unproven there, and evidence for a device class is a power-cut series
on it (`qemu` cannot provide one) or its documented atomic write unit.
The review discloses it.

The update takes one of two forms:

- **Direct**, on a device-bound volume, during the upgrade, and on a
  FIDO2-primary volume, where release binds nothing td could predict:
  the running system writes the new pair and commits it, both entries'
  first cluster and size in the one sector write. The next boot of a
  device-bound volume reaches recovery and its confirmed reseal.
- **Staged**, on a TPM-primary volume, in three phases:
  1. **Stage.** The running system writes the new pair as
     `BOOTX64.NEW` and `INITRD.NEW` and adds both entries in one sector
     write, after removing any earlier staged entries. The consent
     screen says the next boot asks for the PIN and completes the
     update.
  2. **Pre-seal**, at the next boot, before the cap, by the installed
     selector after a PIN release. It reads each staged file once with
     `td-fat`, computing in that one pass the digests every later step
     uses: `BOOTX64.NEW`'s SHA-256 and Authenticode SHA-256 and
     `INITRD.NEW`'s SHA-256. It authenticates nothing yet: before the
     cap it can read no deployment. It predicts the new chain's PCR 4
     and PCR 9 from the event log ("Prediction" below) and seals the
     secret this boot released, with the PIN typed this boot under a
     fresh salt, to the predicted values and a literal-zero PCR 12. The
     object, which would share the released keyslot, stays in RAM: it is
     inert until the commit imports it, and it cannot be verified, since
     its chain is not running.
  3. **Commit**, in the same boot after the cap and before any import.
     With the volume open, the selector authenticates the staged pair
     against the volume's `current` deployment, whose manifest it
     verifies as it does to boot it: `BOOTX64.NEW`'s SHA-256 must equal
     that manifest's `bzImage`; and, after it hashes `root.erofs` once
     against the manifest, attaches it with td-init's `losetup` applet
     (UNSAFE.md §3) and mounts it read-only, `INITRD.NEW` must be that
     root's `/lib/td-boot/selector-initramfs.cpio` followed by exactly
     the archive `prepare-selector` appends with this selector's own
     trusted key, volume UUID and measurement policy (the same
     `engine/src/cpio.rs` writer): the SHA-256 of those bytes must equal
     the digest the pre-seal took, so what was predicted is what was
     authenticated. Only then does it import the predicted object as a
     second tpm-pin token naming the same keyslot, and commit: one
     sector write points `BOOTX64.EFI` and `INITRD` at the first
     clusters and sizes the pre-seal read and hashed, never at entries
     it reads again, and deletes the staged entries; then the old chains
     are freed.

  Any authenticity failure (another kernel, another template, another
  appended archive, a deployment that is not `current`) discards the
  object and deletes the staged update in one sector write, with a
  console line naming the check; an older signed selector therefore
  never installs. A failed prediction with every authenticity check
  passed keeps the staged update and retries the pre-seal at the next
  PIN boot, recording the retry in an `update-retry` token that binds
  the staged pair's digests under a tag HKDF-SHA256 derives from the
  volume key (info `td/disk-protector/update-retry/v1`, a zero byte and
  the volume UUID), so it cannot be forged on the ESP or the header.
  When the retry's prediction fails again, its authenticity checks pass
  and that token verifies for the same pair, the console offers to
  complete the update anyway, saying that the machine restarts at once
  and the next boot will show the chain warning once and ask for a
  security key or the recovery key, which is expected; exactly `update`
  commits without a predicted object and reboots immediately, so the
  warning boot happens while the owner is present, and any other answer
  deletes the staged update. The commit and the deletion remove the
  retry token. Someone with firmware access can still force the two
  prediction failures (a one-time boot entry that fails and returns logs
  a refused event), which costs the owner one token or recovery-key boot
  they are present for. The boot after a commit releases with the
  predicted object and the PIN, and its plan retires the other tpm-pin
  token; its keyslot stays, named by the one that released. A
  misprediction fails the chain check, shows the warning, and needs a
  token or the recovery key and a reseal, which retires both objects.

  **Prediction.** The selector mounts securityfs and reads the TCG
  event log as Linux presents it, the firmware's crypto-agile log with
  the final-events table appended. It requires the Spec ID event to
  list SHA-256, takes each event's SHA-256 digest, skips `EV_NO_ACTION`
  events, which extend nothing, and replays PCR 4, PCR 7 and PCR 9 from
  zero. PCR 9 then takes td's own extensions, which follow
  ExitBootServices and so are in no firmware log: under Secure Boot the
  configuration event ("Firmware authentication"), whose digest the
  selector recorded when it extended it. Each replay must equal the
  PCR as read. It refuses a log whose events may not recur at the next
  boot: an `EV_EFI_ACTION` "Returning from EFI Application from Boot
  Option" (a boot attempt that failed or returned), more than one
  `EV_EFI_BOOT_SERVICES_APPLICATION` in PCR 4 (a boot menu, shell or
  one-time boot entry that ran before the selector), or any PCR 4 event
  but the calling action, separators and that one application. It
  requires that application's digest to be the Authenticode SHA-256 of
  the running `BOOTX64.EFI` and exactly one PCR 9 event, the EFI stub's
  initrd measurement, to be the running `INITRD`'s SHA-256. Replaying
  with the staged files' digests in those events gives the predicted
  PCR 4 and PCR 9; PCR 7 keeps its value.

| Interrupted | ESP and header after | Next boot |
| --- | --- | --- |
| Staging, before its sector write | old pair, leaked clusters | ordinary; the next update reclaims |
| After staging | old pair, staged entries | pre-seal and commit |
| Pre-seal, or after the cap before the import | unchanged | the same again |
| After the import, before the commit | old pair, two tpm-pin tokens | old object releases; plan retires the predicted token; pre-seal again |
| After the commit, before freeing | new pair, leaked clusters | predicted object releases; the next update reclaims |
| Deleting a staged update | old pair, with or without staged entries | ordinary, or the same deletion again |
| Direct, before its sector write | old pair, leaked clusters | ordinary; the next update reclaims |
| Direct, after it | new pair | the changed chain's path |

No previous selector is kept; a selector that does not boot is repaired
from the live medium, by hand.

**Threat and limits.** Staging needs root on the running system and
physical consent. Anyone who can write the ESP can also stage files, but
the predicted object leaves RAM only after the pair is authenticated
against the volume's `current` deployment, so a forged staging gains no
seal and no installation, and an older signed selector is refused.
Between the import and the next boot the old chain's object stays
valid, which lets that genuine chain boot once more.

### Verified account handoff

One primary authentication unlocks storage and admits its account to
one fresh session. Every protected boot's selector writes an admission
record after its release, once its PCR 11 measurement of the selected
deployment has succeeded. The record is 46 bytes: `TDADMIT1` (8 bytes),
version 1 (1), the big-endian u32 UID the released token names (4), the
method (1: 0 for no admission, after a fido2-recovery or recovery-key
release and in every upgrade boot; 1 tpm-pin; 2 fido2-primary), and the
selected deployment's 32-byte manifest ID (32). td-kexec carries it
beside the key, on a second pipe, as a second archive member
`td-admit-v1`, a regular file of mode 0400 owned by root with exactly
those bytes, under the rules the key member has (`--fds-key-admit DIGEST
KEY RECORD CMDLINE`, the record written and its write end closed before
td-kexec starts).

The deployment initramfs never carries it to disk. `mount-root` handles
the key as today and leaves the record; a new verb, `td-boot admit
/sysroot/run`, which the deployment init runs right after it mounts the
system's `/run` tmpfs, inspects the member as `mount-root` inspects the
key, writes it to a fresh root:root mode-0700 `/sysroot/run/td-admit` as
`v1`, a single-link root:root mode-0600 file created exclusively, and
removes the member, whether or not it passed. A failure writes nothing
and is no refusal: a lost record costs a login, not a boot. A deployment
from before the verb leaves the member in its initramfs root, which
`switch_root` frees.

td-authd reads it at a paired generation's Prepare, before the
compositor's first `1a`. It opens `/run/td-admit/v1` without following
links, requires the file's ownership, mode, single link and exact size
on a `/run` that is tmpfs, reads it and unlinks it, then creates
`/run/td-admit/consumed` exclusively (`O_CREAT|O_EXCL`, mode 0600)
before judging it; an existing marker discards the record, so a boot
admits at most once, and a record copied back after consumption admits
nothing. Any valid record makes td-authd create the persistent marker
`/var/lib/td/login/protected` if absent; with it present, an unenrolled
login state locks as an enrolled one does (td-authd/DESIGN.md, amendment
9). A record with method 1 or 2 admits the generation only when its UID
is the primary account's, its manifest ID is the running deployment's
(`/run/td-deployment`), and the deployment kernel's `CLOCK_BOOTTIME`,
which starts at the `kexec` just after the selector wrote the record and
so measures the record's age, reads at most 300 seconds. The first `1a`
answer then says so, and the compositor's first paint is the session
rather than the lock surface (td-compositor/DESIGN.md, "The lock
surface"). Every later generation, `Super+l`, a lid close and a resume
lock as TOKEN-LOGIN.md specifies, and unlock needs a login key.

**Login keys.** Admission unlocks one first frame; it authorizes no
login-key operation. Replacing lost login keys on a protected machine
takes a fresh proof at that moment, the recovery key or a disk security
key, under the rule TOKEN-LOGIN.md "Enrollment, addition and removal"
states. Upgrade's first phase requires an enrolled login state, and the
last login key cannot be removed on a protected machine, so a recovery
boot never finds an unlocked session.

**Threat and limits.** The record is the non-replayable authentication
result: it exists only in RAM, crosses only the sealed memfd and a
root-only tmpfs, admits once per boot, and binds the account, the
measured deployment and the boot's first minutes. A recovery release
admits nothing, so recovery never logs in. Root on the running system is
trusted and could forge a record or remove the markers; a compromised
deployment is outside Scope. The recovery key's holder can repair the
login state from the live medium, as on a device-bound volume
(TOKEN-LOGIN.md, "Recovery"); it is a storage credential with full
authority, which is why it is never typed at a login prompt.

### Re-encrypting upgrade

A device-bound volume becomes protected in two phases: a running-system
phase that gathers what needs the person's tokens and writes no secret,
and an upgrade boot of the selector that rotates the volume key. The
rotation is required ("Device-bound default").

**Phase 1** is `td-install storage-operation`'s `upgrade`, under
td-authd's physical consent. It requires a converged device-bound header
(keyslot 0 and one device-bound protector), no reencryption in progress,
an acknowledged `current`, and an enrolled login state (TOKEN-LOGIN.md).
The person chooses the shape, TPM primary being the default; the
FIDO2-primary choice discloses both trade-offs: an altered selector can
take the volume from a single presentation of the key, while a firmware
TPM's guess counter rolls back with its flash ("Lockout"). The TPM shape
is offered only when a TPM with a SHA-256 bank is present and its
`TPM_PT_PERMANENT`, read without authorization, shows `lockoutAuthSet`
clear, since a device-bound volume holds no lockout token that could
prove a set one; otherwise the screen says why and offers the FIDO2
shape. Each FIDO2 credential is created, proved, repeated and probed
("FIDO2 protectors") through the compositor's PIN field. The worker then
writes, each a key-less header commit: the selector update in its direct
form, so the selector that reads what follows understands it; each fido2
token with empty `keyslots`, a pending token; and last the `td-upgrade`
token, a token type of its own holding only the shape and `from`, the
SHA-256 of the data segment's digest and salt as they are now
(td-protector, "Protected roles"). A pending fido2 token is pending only
beside a `td-upgrade` token; without one it is an orphan the next plan
removes, so a crash inside phase 1 leaves a device-bound volume that
boots through recovery and its confirmed reseal, the selector having
changed, and the person runs phase 1 again. Before consenting, the
review discloses the upgrade boot's duration (about one read and one
write of the whole volume, two writes with journal resilience), that it
needs mains power, that the machine is not usable during it, that the
recovery key is needed at that boot and stays the recovery protector,
and that the tokens and the new PIN are asked for then.
`storage-operation`'s `upgrade-cancel` removes the `td-upgrade` token
while the header is in state U0.

**The upgrade boot** is the selector's flow while the header carries a
`td-upgrade` token. Its state is read from the header alone:

| State | Header |
| --- | --- |
| U0 | device-bound token and keyslot, keyslot 0, pending tokens, `td-upgrade`; digest equals `from` |
| U1 | no device-bound token or keyslot; digest equals `from`; no reencryption |
| U2 | cryptsetup's online-reencryption requirement present |
| U3 | digest differs from `from`; no requirement; no `recovery-key` marker |
| U4 | marker present; some pending token or the tpm-pin token (TPM shape) or the lockout token (TPM shape) missing |
| U5 | marker, every fido2 token with a keyslot, and in the TPM shape the tpm-pin and lockout tokens, unless the TPM became unusable (below) |

1. **Before the cap.** In U0, the selector first requires mains power:
   a `/sys/class/power_supply` entry of type `Mains` online, or no
   `Battery` entry. Without it the console says the upgrade waits for
   mains power, and the boot runs the device-bound recovery flow with
   nothing upgraded. In U0 an empty entry at the first prompt asks
   whether to cancel, and exactly `cancel` confirms: the boot runs the
   device-bound recovery flow, and its plan removes the `td-upgrade`
   token, after which the pending tokens are orphans it removes too.
   After U0 nothing cancels, and the prompts say so. The selector then
   gathers what the state still needs: for each pending fido2 token in
   turn, its key alone, its PIN and a touch, the assertion verified
   under the token's public key and the passphrase derived; in the TPM
   shape without a tpm-pin token, the new PIN twice (the pre-cap rule
   in "Lockout" applying), a fresh secret sealed to the observed PCR 4
   and PCR 9 (and PCR 7 when measured on) and a literal-zero PCR 12,
   and one Unseal verifying it; and the recovery key. In U5 it gathers
   nothing and runs the protected release instead.
2. **The cap.**
3. **After the cap**, every step authorized by the recovery key, tested
   first on the recovery keyslot: keyslot 0 in U0 and U1; in U2 by
   `reencrypt --resume-only` itself, since cryptsetup then holds a
   keyslot for each volume key; in U3 the one `luks2` keyslot no token
   names, since reencryption may have moved it; from U4 the keyslot the
   marker names. The steps run in order
   from the state found, each a commit the console reports:
   - U0 to U1: kill the device-bound keyslots, then remove the
     first-boot and device-bound tokens; a token naming a killed
     keyslot is an orphan the step removes.
   - U1 to U3: open the mapping `td-selector` with the recovery key and
     re-encrypt it online: `cryptsetup reencrypt` on the held partition
     with `--active-name td-selector`, `--key-slot` naming the recovery
     keyslot, `--use-random`, formatting's cipher, key size, sector size
     and PBKDF2 parameters, the recovery key on standard input
     (td-protector's runner spells the list), and `--resilience
     checksum` when the disk's `queue/atomic_write_unit_max_bytes` is
     at least 4096, otherwise `--resilience journal`. Progress is
     reported at least every percent. In U2 the step is `reencrypt
     --resume-only` with the same key, before anything else and before
     any handoff, since the handoff carries one volume key; a resumption
     on battery warns and continues. td-boot admits the mapping by
     discovery's walk only once reencryption has ended and cryptsetup
     has removed its helper devices.
   - U3 to U4: import the `recovery-key` marker naming the recovery
     keyslot.
   - U4 to U5: kill any `luks2` keyslot no token names but the marked
     one, an add an earlier attempt left; then for each pending fido2
     token add its keyslot and replace the token with one naming it
     (`token import --token-replace`), testing each; in the TPM shape
     add the tpm-pin keyslot, import its token and test it, then take
     the lockout ("Lockout"). When the TPM has become unusable for td
     since phase 1 (absent, without a SHA-256 bank, or with a lockout
     authorization td cannot prove), the console says so and U4 ends
     without a tpm-pin or lockout token: the volume completes as a
     TPM-primary volume with no tpm-pin protector, as after a TPM clear,
     whose boots ask for a token or the recovery key and offer the
     reseal once the TPM is eligible again.
   - U5 to protected: remove the `td-upgrade` token.

   The selector keeps the mapping it opened, takes the volume key with
   the recovery key and hands off with an admission record of method 0.
   A boot that finds U5, which a crash just before the last step leaves,
   releases as a protected boot, with the PIN or a primary token, and
   its plan removes the `td-upgrade` token, so the upgrade's end needs
   no recovery key.

| Interrupted | Next boot |
| --- | --- |
| Phase 1, before `td-upgrade` | device-bound recovery and reseal; pending tokens removed; phase 1 again |
| Upgrade boot, before the first commit | U0 again, cancel still offered |
| U0 to U1, between kill and removal | U1 after the orphan's removal |
| U1, U2 | reencryption starts or resumes with the recovery key |
| U3 | marker import |
| U4, after an add before its token | that keyslot killed as an orphan, the add repeated; a lost tpm-pin secret means the PIN is asked again before the cap |
| U4, inside the lockout take | the take's own crash rule ("Lockout") |
| U5 | protected release; `td-upgrade` removed |

Both resilience modes keep their hotzone in the keyslots area, so the
upgrade needs no free space on the volume ("Device-bound formatting");
checksum resilience relies on whole atomic writes of the 4096-byte
sector, which only a device stating such a unit provides, and journal
resilience writes the data twice. The header's 12 KiB JSON area must
hold the largest state the upgrade or a protected volume reaches: four
fido2 tokens at the 255-byte credential bound, the `td-upgrade` token,
the marker, two tpm-pin tokens, the lockout, Secure Boot, config and
update-retry tokens, a request, their keyslots, and in U2 cryptsetup's
reencryption keyslot, segments and second digest. It is estimated at
about 9.5 KiB. 8a's codec tests and 8f's planner tests build each worst
case with the pinned cryptsetup on a header file, the U2 one by
`reencrypt --init-only`, and require at least 1 KiB to spare; if the
measurement leaves less, the credential bound or the token counts shrink
in that commit, amending this section. The planner computes each step's
resulting JSON size and refuses a step whose header would not fit,
before writing.

**Threat and limits.** Rotation makes the old volume key, and any copy
of it taken while the volume was device-bound, useless for what is
written afterwards and for the logical blocks reencryption rewrites. It
does not reach old ciphertext an SSD keeps in stale flash pages after
remapping: someone holding the old key and the flash itself may still
read data as it was before the upgrade. The recovery key opens the new
key as before. The upgrade inherits whatever the device-bound system's
root left behind (Scope).

### Protector management

On a protected volume `td-install storage-operation` adds a FIDO2
token, removes FIDO2 tokens, removes the recovery key, requests a TPM
PIN change, and requests a TPM clear, each under td-authd's physical
consent. Addition and removal are authorized either by a fresh
authorize-phase assertion over the operation's canonical description
from an enrolled FIDO2 token that remains afterwards, whose derived
passphrase is the key `luksAddKey` and `luksKillSlot` are given, or by
the recovery key typed in the PIN field, which cryptsetup tests as the
same key; the authorization is also the cryptsetup authority. An
addition creates the new credential with every enrolled one excluded. A
removal refuses to leave a shape below its minimums ("Protectors and
shapes"). These, and the keyslot test of TOKEN-LOGIN.md's disk proof,
are the running system's only key-bearing cryptsetup commands
(DESIGN.md "Full-system volume consumers").

The PIN change and the TPM clear need the TPM before the cap or the
lockout authorization, which only the selector has, so the running
system imports a key-less request token and the selector carries it out:

- **PIN change.** At the next PIN boot, after the release and before the
  cap, the selector says a PIN change was requested and prompts `td new
  PIN (empty to cancel): `; an empty entry declines, and the plan
  removes the request after the cap, since anyone who can write the disk
  can plant one. Otherwise it asks the new PIN again, seals a fresh
  secret under it to the observed values and verifies it by one Unseal,
  and, with a Secure Boot db key present, changes that key's
  authorization from the old PIN to the new (TPM2_ObjectChangeAuth).
  After the cap it adds the new keyslot under the old secret, imports
  and tests the new token, retires the old tpm-pin keyslot and token,
  replaces the db key's blob, and removes the request last. A crash
  before the new token leaves the old PIN working and the request in
  place, which repeats; one after it leaves both PINs working until a
  boot's plan retires the protector that did not release. The old PIN is
  this operation's fresh authentication.
- **TPM clear.** After a release and the cap, the console says that
  clearing the TPM destroys every object under its storage hierarchy:
  the tpm-pin protector, the Secure Boot db key and td-secret's TPM
  objects; exactly `clear` confirms, and TPM2_Clear runs under `L`;
  any other answer declines and the plan removes the request, which
  anyone who can write the disk could have planted. The
  plan then retires the tpm-pin keyslot and token and the lockout token
  and removes the request. The next boot asks for a token or the
  recovery key and offers the reseal, which takes the lockout anew.

**Threat and limits.** A removed protector stays in old header copies;
TPM2_ObjectChangeAuth leaves the db key's old blob valid under the old
PIN wherever a copy of it survives.

### Firmware authentication (optional)

UEFI Secure Boot under keys the machine generates is an optional
strengthening and does not gate item 9. It is offered only in the
TPM-primary shape.

- **Image.** Authenticode on `BOOTX64.EFI` alone would leave the
  external `INITRD`, which holds the selector and its PIN prompt,
  unauthenticated: the EFI stub loads and measures it but verifies
  nothing. Under Secure Boot the selector is instead one signed PE, the
  sealed selector image: a second build of td's kernel with the stock
  selector initramfs built in (`INITRAMFS_SOURCE`) and a built-in
  command line that firmware cannot change (`CMDLINE_OVERRIDE`) and
  that names `noinitrd`. `CMDLINE_OVERRIDE` alone is not enough: in
  Linux 7.1.4 `efi_load_initrd` (`efi-stub-helper.c` in
  `drivers/firmware/efi/libstub`) loads an initrd offered through the
  `LINUX_EFI_INITRD_MEDIA_GUID` LoadFile2 device path whatever the
  command line says, and the kernel unpacks an external initramfs over
  the built-in one, where it can replace `/init`. The x86 stub parses
  `CONFIG_CMDLINE` with `efi_parse_options` before it loads an initrd
  (`x86-stub.c`), and `noinitrd` there sets `efi_noinitrd`, which makes
  `efi_load_initrd` return before either the LoadFile2 or the
  `initrd=` path; the firmware's load options are never parsed. The
  recipe's configuration check pins all three, and the image carries
  the kernel and every line of selector code under firmware's check.
  The deployment's root image ships the unsigned sealed image as
  `/lib/td-boot/selector.efi`, beside the template. The recipe graph
  builds it, a second kernel compile; a third-party stub that reads
  embedded sections is excluded by D6, and relinking per installation
  would need a toolchain on the running system.
- **Configuration.** The per-installation values `prepare-selector`
  appends today (trusted key, volume UUID, measurement policy) become
  the data file `EFI/BOOT/TDCONFIG`, written by the same
  `engine/src/cpio.rs` writer as a cpio archive. The selector reads it
  once, extends its SHA-256 into PCR 9 with TPM2_PCR_Extend before
  parsing it, and keeps that digest for prediction ("Selector update",
  "Prediction"); tpm-pin seals bind it through PCR 9. A replaced
  `TDCONFIG` changes PCR 9, so the PIN releases nothing, but a token or
  the recovery key still releases after the warning, and the attacker's
  trusted key would then select an attacker-signed deployment. So the
  volume authenticates it: a `config` token holds one or two
  HMAC-SHA256 tags over `TDCONFIG`'s bytes under a key HKDF-SHA256
  derives from the volume key with the info `td/disk-protector/config/v1`,
  a zero byte and the volume UUID. After every release and the volume
  key's retrieval, before the trusted key selects or verifies anything,
  the selector requires `TDCONFIG`'s tag to be in the token, compared in
  constant time; otherwise it zeroes the key and halts, saying the
  ESP's configuration is not this volume's. A staged update's
  `TDCONFIG.NEW` must equal what this selector writes from its own
  authenticated values; when its bytes differ, the commit adds its tag
  to the token before the sector write and the next boot's plan drops
  the old one, so a crash between them leaves both pairs valid. While
  both tags are valid, a one-boot rollback to the old `TDCONFIG` is
  possible, only when the trusted key or measurement policy changed,
  and only through a token or recovery-key boot, since PCR 9 refuses
  the PIN.
- **Enrollment** is `storage-operation`'s `secure-boot-enroll`, with
  the PIN in the PIN field, in two steps around one boot, because only a
  selector holding the volume key can write the first `config` tag:
  1. The running system requires the firmware in setup mode
     (`SetupMode` 1, no PK). It refuses when the event log's PCR 2
     records an option ROM driver (`EV_EFI_BOOT_SERVICES_DRIVER` or
     `EV_EFI_RUNTIME_SERVICES_DRIVER`), which a td-only db would stop
     from loading. It creates the db key, signs the root's
     `/lib/td-boot/selector.efi` with it and stages the signed image and
     `TDCONFIG.NEW` as a staged selector update. The next PIN boot
     authenticates them as any staged update (the image by its
     Authenticode SHA-256 against the root's unsigned image,
     `TDCONFIG.NEW` against this selector's own values), predicts PCR 4
     from the image and PCR 9 without the stub's initrd event and with
     the configuration extension, writes the first `config` tag, and
     commits; the selector-update sector write renames `INITRD`'s entry
     to `TDCONFIG`. Secure Boot is still off, so the signed image boots
     as an unsigned one would and the PIN releases.
  2. The running system then mounts efivarfs with td-init's `mount` for
     the operation alone and, accepting whatever KEK and db the
     firmware kept through its PK clear, replaces both whole with
     non-append writes: db (signed by KEK), then KEK (signed by PK),
     then PK (self-signed), whose creation ends setup mode. dbx is left
     as the firmware has it. It reads back db, KEK and PK as exactly
     td's lists and `SetupMode` 0. The next boot, with Secure Boot on,
     moves PCR 7, shows the warning, and needs a token or the recovery
     key and a reseal, which binds PCR 7; the consent screen says so.
  td enrolls no Microsoft or vendor certificate, so firmware dbx
  updates signed by them no longer apply.
- **Keys.** PK and KEK are RSA-2048 keys created in the TPM, used to
  sign their enrollment payloads, flushed, and never stored: they are
  discarded after enrollment. The db key is an RSA-2048 signing key
  under the storage primary (fixedTPM, fixedParent, sensitiveDataOrigin,
  sign, adminWithPolicy; userWithAuth and noDA clear) whose policy is
  PolicyOR of PolicyAuthValue then PolicyCommandCode(Sign) and
  PolicyAuthValue then PolicyCommandCode(ObjectChangeAuth), its
  authValue derived from the TPM PIN under its own salt; a PIN change
  rebinds it. Its blob lives in the `secure-boot` token, so the selector
  can rebind it before the cap, and its certificate in
  `/var/lib/td/secure-boot`, root-only on the volume. td writes the
  self-signed X.509 v3 certificates, the EFI signature lists and
  authenticated variable payloads in DER itself, the TPM signing each
  SHA-256 digest (TPM2_Sign, RSASSA); no key is downloaded, published
  or held anywhere else (principle 5).
- **PCR 7.** A tpm-pin seal names PCR 7 exactly when the event log,
  replayed to the PCR 7 value read, records `SecureBoot` enabled; no
  header field decides it. Turning Secure Boot off moves PCR 7, which
  fails the chain check with the warning.
- **Updates.** A staged selector update signs the new image with the db
  key, the PIN in the PIN field authorizing TPM2_Sign, and stages it
  with `TDCONFIG.NEW`. The commit authenticates the image by its
  Authenticode SHA-256 against `current`'s `/lib/td-boot/selector.efi`
  and `TDCONFIG.NEW` against this selector's own authenticated values,
  as "Selector update" does the unsigned pair. Authenticode's
  hash leaves out the certificate table, so the pre-seal predicts PCR 4
  from the image as for an unsigned one, PCR 9 by replacing the
  configuration extension's digest, and keeps PCR 7, whose authority
  event names the same db certificate.
- **Kernel.** `EFIVAR_FS` is built in and mounted by nothing but
  enrollment; lockdown stays off (no lockdown LSM) and `KEXEC_SIG` stays
  off, since td-boot authenticates deployments itself.

**Threat and limits.** With Secure Boot and a firmware setup password,
firmware runs only the td-signed image, which holds every line of the
selector and loads no initramfs from outside it, and `TDCONFIG` is
authenticated by the volume before it selects anything, so a PIN prompt
on that machine is td's. Without the password, anyone holding the
machine can turn Secure Boot off; PCR 7 then refuses release and the
genuine selector warns, but a look-alike prompt can again record the
PIN, as without Secure Boot. A cleared TPM loses the db key: the signed
image keeps booting, but no new one can be signed until Secure Boot is
reset in setup and enrolled again.

### Unsafe and syscall surfaces

No piece of increment 8 adds an `unsafe` surface. td-tpm's new commands
are bytes over its safe file I/O on `/dev/tpmrm0`, its session
cryptography td-fido's safe code. The FIDO2 boot client reads and writes
`/dev/hidrawN` with std file I/O and reads the report descriptor from
sysfs, so it needs no `HIDIOCGRDESC` or other hidraw ioctl; `td-fido`
and `td-fat` forbid `unsafe`. PIN entry is the existing secret-line
applet with new prompt operands. The admission record travels through
td-kexec's existing memfd, pipe and sealing calls (UNSAFE.md §1). The
ESP commits are positioned writes and `fsync` on the held disk; the
event log, power supplies and the disk's atomic write unit are sysfs and
securityfs reads; securityfs and efivarfs are mounted by td-init's
existing `mount` applet, which takes the filesystem type as an operand,
and the commit's read of `root.erofs` uses its existing `losetup` applet
(UNSAFE.md §3), and enrollment only creates variables, which needs no
`FS_IOC_SETFLAGS`. The upgrade and management are cryptsetup children of
td-protector's runner. UNSAFE.md records this plan beside td-boot's
entry; a commit that finds it needs a syscall amends UNSAFE.md in that
commit.

## Independently landable increments

1. This design, its device-bound amendment, and atomic reconciliation of the
   authentication contracts.
2. Built-in device mapper, dm-crypt and AES-XTS support in the target kernel,
   with checks against the realized kernel. This alone unlocks nothing.
3. Source-built cryptsetup. The repository owner granted principle-2
   sign-off for a static cryptsetup using its kernel crypto backend and
   internal Argon2, with this closure: LVM2's device-mapper library alone,
   json-c, popt, and util-linux's libuuid and libblkid already built for
   btrfs-progs. No OpenSSL, libgcrypt, udev, libssh, external token loader or
   token plugin enters; any other library needs its own sign-off. LUKS2
   re-encryption stays enabled. The kernel gains the AF_ALG hash and
   skcipher interfaces and the HMAC, SHA-256 and XTS-AES algorithms that
   backend uses; td-jail's socket-family filter already keeps confined
   applications from AF_ALG. The same increment amends DESIGN.md D6 to make
   cryptsetup its boot-path exception; the build-time binding D7 requires
   lands with its first exec. Do not replace this with a new cryptographic
   disk format.
   New Rust syscall surfaces amend UNSAFE.md with their component contract
   in the same increment.
4. Share td-secret's dependency-free TPM 2.0 client with the disk
   protector: seal to observed PCR 4 and PCR 9 values plus a literal-zero
   PCR 12, unseal under that policy, and extend and read back the release
   cap. Application secret formats and policy do not change. td-tpm and
   td-protector carry this increment, and the selector's PCR 11
   measurement runs over the same client; the LUKS2 token format is
   increments 5 and 6.
5. Add installer formatting, the first-boot protector, the recovery key,
   the no-TPM refusal and crash-safe enrollment, reachable then only
   through the service's storage operand, which increment 7 deleted
   ("Device-bound formatting"); preserve
   file-image testing and the single deployment publisher. The encrypted
   path keeps no plaintext scratch image, accounts for header and
   re-encryption space, and identifies backing devices without `/dev/vda`
   pins. Its commits, in order: the recovery-key codec; the token codec
   and bounded LUKS2 header reader in td-protector; the plan and protocol
   records (INSTALLER.md); the TPM probe; formatting with D6's cryptsetup
   binding, whose image check names the binary and its debug companion;
   the completion page's display and type-back; and the
   encrypted-installation oracle.
6. Add selector release, the PCR 12 cap in the installed and live
   selectors (amending MEDIA.md), the first-boot transition, the
   recovery flow with its confirmed reseal, the volatile `kexec` handoff and
   deployment-initramfs unlock, extending D6's binding to both initramfs
   ("Selector release"). Exercise them together before activation. Its
   commits, in order: this specification; td-protector's cryptsetup
   runner, moved from td-install, and its pure transition planner;
   td-protector's release orchestration; the live selector's cap;
   td-kexec's sealed-memfd key handoff; td-init's console secret-line
   applet; td-boot's LUKS2 volume and mapping discovery;
   deployment-initramfs unlock with D6's deployment half; selector
   release, the first-boot transition and the handoff with D6's selector
   half; the recovery flow with its confirmed reseal; and the
   `qemu-boot-encrypted` oracle.
7. Activate the device-bound tier as the installer default on machines with
   a usable TPM 2.0 whose live system shows a keyboard console ("Keyboard
   console", "Activation"), amending INSTALLER.md's disclosures in the
   same landing. A machine without both installs unencrypted, disclosed.
   Automatic login remains and stays disclosed. Its commits, in order:
   this specification; the kernel's firmware framebuffer and the
   `console=tty0` prefix; td-init's secret-line on both consoles with
   td-boot's mirrored lines, carrying UNSAFE.md §3's amendment, which
   then becomes current; the installer's keyboard-console probe, inert:
   recorded in the plan and shown on the review, not yet acting; and the
   activation, which deletes the storage operand, makes the default,
   changes the review, td-authd's consent summary and INSTALLER.md's
   disclosures, and adds its oracle legs.
8. In successive increments, add TPM PIN release, FIDO2 primary and
   recovery, the verified account handoff, the predicted selector
   update, the re-encrypting upgrade and protector management, exercise
   them together before activation, and offer firmware authentication
   ("Protected tier"). Each sub-increment below is independently
   landable and each commit green. None is reachable through td's own
   operations before item 9: td-authd refuses every storage operation in
   a production build, so only root running the worker directly, as the
   oracles do, writes a protected or upgrading header. From 8c on, the
   installed selector's protected paths are reachable through a forged
   header by anyone who can write the disk, so 8c and every later
   selector commit test that forged headers fail closed. Selectors from
   8a to 8e read a `td-upgrade` token and its pending tokens through
   td-protector's reader and boot such a header as a device-bound one,
   leaving them untouched, so a later selector can still upgrade it. A
   commit that changes the shipped selector or deployment initramfs runs
   `check integration` by hand, and `qemu-boot-encrypted` keeps passing
   unchanged on device-bound headers. Its commits, in order:
   - **8a, primitives.** This specification; the `td-fido` crate, moved
     out of td-secret with no behaviour change, td-secret its first
     consumer; td-tpm's salted sessions (ECDH to the storage primary
     through td-fido's P-256, AES-128-CFB parameter encryption) and HMAC
     policy sessions with PolicyAuthValue, sealing with an authValue
     and noDA clear, Unseal's typed `TPM_RC_AUTH_FAIL` and
     `TPM_RC_LOCKOUT`, and GetCapability of the dictionary-attack
     properties; td-tpm's lockout commands (TPM2_HierarchyChangeAuth,
     TPM2_DictionaryAttackParameters, TPM2_DictionaryAttackLockReset and
     TPM2_Clear); td-protector's PIN codec, authValue derivation,
     tpm-pin policy and chain check; td-protector's protected token
     codecs (tpm-pin, fido2, `recovery-key`, `lockout`, `td-upgrade`,
     `td-request`), the lockout token's encryption, and the reader's
     protected bounds, with the real-cryptsetup header measurements.
     All library code, called by nothing in production.
   - **8b, FIDO2 boot client.** td-fido's disk-protector flows
     (identify, the UV hmac-secret assertion with signature
     verification, passphrase derivation, and creation with proof,
     repeat and the no-PIN probe) and its `FIDO_2_1` admission; the same
     admission and probe in td-secret's login-key creation
     (TOKEN-LOGIN.md, "Token profile"); td-boot's `fido-worker` verb over
     root hidraw admission, linked into td-boot and run by nothing yet.
   - **8c, protected selector release.** td-protector's planner rules
     for protected headers (pure), forged headers among their tests;
     its protected release orchestration: the chain check and warning,
     the TPM PIN, the security key, the recovery key, the verified
     pre-cap reseal and the plan, over a `release::Fido` interface; the
     lockout take, proof, counter and reset after the cap; td-boot's
     prompts and wiring through secret-line and the worker, with the
     deployment initramfs's PolicyPCR-only check of tpm-pin tokens.
   - **8d, verified account handoff.** td-kexec's `--fds-key-admit` and
     second member; td-boot's admission record in the selector and its
     `admit` verb with the deployment init's line; then, as one commit
     since the paired peers ship atomically, still `TDLA003`: td-authd's
     consumption with its consumed and protected markers, amendment 9's
     admission byte in `9a`, an unenrolled protected machine locking,
     and the compositor's admitted first paint; then TOKEN-LOGIN.md's
     protected-volume rule: the disk proof for login-key operations and
     the last-key refusal (td-authd and td-secret's login operation).
   - **8e, predicted selector update.** A documentation commit fixing
     td-authd's storage-operation wire (amendment 10) and the
     compositor's storage screen before their code; the `td-fat` crate
     (bounded FAT32 reader, reclaim, staging and the one-sector
     commits); td-boot's event-log reader, its refusal rules and PCR 4
     and PCR 9 prediction, with the kernel's `SECURITYFS` pin;
     td-init's `losetup` applet linked into the selector initramfs, with
     its D6 binding and the image check that requires it;
     `storage-operation`'s `selector-update` in both forms, with
     td-authd's supervision, which refuses it in production; the
     selector's pre-seal, its authentication against `current`'s
     `bzImage` and `root.erofs`, its retry token, deletion and immediate
     reboot after `update`, and the commit.
   - **8f, re-encrypting upgrade.** td-protector's upgrade planner over
     the states U0 to U5 and the runner's `reencrypt` (initialization in
     both resilience modes and `--resume-only`) and `token import`
     shapes; the `upgrade` and `upgrade-cancel` operations, with phase
     1's `TPM_PT_PERMANENT` check; the selector's upgrade boot with its
     power check and U4's exit for a TPM that became unusable.
   - **8g, protector management.** `storage-operation`'s FIDO2 token
     addition and removal and recovery-key removal; the PIN-change
     request and its selector half; the TPM-clear request and its
     selector half.
   - **8h, the combined oracle.** The host's virtual FIDO2 authenticator
     over QEMU `usb-redir` ("Acceptance evidence"); then
     `qemu-boot-protected` with every leg below. The hardware evidence
     follows it and precedes item 9.
   - **8i, firmware authentication (optional).** The sealed selector
     image's recipe (the second kernel build with the selector initramfs
     built in, `CMDLINE_OVERRIDE` and `noinitrd`, its configuration
     check, and `/lib/td-boot/selector.efi` in the root image) and the
     kernel's `EFIVAR_FS` pin; the selector's `TDCONFIG` reading, PCR 9
     extension and recorded event, and the `config` token and its check
     after release; td-tpm's RSA signing keys, TPM2_Sign, PolicyOR and
     TPM2_ObjectChangeAuth; td-install's DER, X.509, signature-list,
     authenticated-variable and Authenticode writers (pure);
     `secure-boot-enroll` in its two steps, with its option-ROM refusal
     and readback, PCR 7 from the measured state, signing in the staged
     update, and the db key's rebinding in the PIN change; and its
     oracle legs. It may land before or after item 9, which it does not
     gate; the owner may defer or drop it.
9. Activate the protected tier only with trusted login/lock
   (td-login/TOKEN-LOGIN.md) and operation consent, with no automatic login
   in that profile, and only after `su` and root's empty shadow field have
   retired as APPLICATIONS.md §L.1, "Retiring the escape hatch",
   specifies (its L6 and L7). Activation lifts td-authd's refusal of
   increment 8's storage operations and adds their disclosures; it
   requires 8a to 8h and their hardware evidence, not 8i.

## Acceptance evidence

Use disposable QEMU disks and the existing pinned TPM emulator oracle; no
test touches an operator's disk or enrolls their hardware.

For the device-bound tier, install under UEFI firmware that measures into
the emulated TPM; the firmware oracle must attach that TPM. Require a real
encrypted read/write roundtrip and reboot persistence with no interaction,
and a first-boot transition interrupted between its keyslot, token, test
and destroy steps (td-protector "Transitions") that completes on the
next boot. A changed selector image,
initramfs or load option and a cleared or different TPM must refuse release
and reach the recovery flow, where the recovery key opens the volume and a
confirmed reseal restores unattended boot. An unseal after the PCR 12 cap
and the live medium booted on the installed machine must both refuse
release. Install without a TPM and show the unencrypted disclosure in the
consented plan. Record ciphertext/header checks and the absence of the
volume key, protector secrets and recovery key from the ESP, logs, scratch
artifacts and command lines.

Increment 5's encrypted-installation oracle, `td-recipe-eval
qemu-install-encrypted --tpm /absolute/path/to/swtpm`, is separate from
the integration tier. Like `qemu-secret-system` it needs an explicit
swtpm path (td-secret/DESIGN.md "TPM validation" says which builds
serve); without one it is an unprovisioned host gap, not a usage error.
Its guest drives the service, with the swtpm, a display device and the
PS/2 keyboard attached so that its plans are device-bound, onto a
disposable disk whose two table ranges hold a valid GPT the host seeded,
as both of its peers: it fetches the recovery key once, requires a
second ask refused as sent and a mistyped type-back refused as a
mismatch with the phase continuing, and types the key back. Reaching the
phase is the evidence of verifying boot's checks, the token read back
from the header cryptsetup wrote through td's reader and the TPM's
verification of the sealed object among them; in the phase both of the
disk's table ranges, which held the seeded table when the service
started, must read zero. After completion it opens the volume with the
recovery key, where a mistyped key fails, and requires the published
deployment and the settings inside; the host independently parses both
LUKS2 header copies and, through td-engine's GPT reader, the table,
whose volume partition must be whole 4 KiB encryption sectors, and
checks the ciphertext (no Btrfs superblock or staged plaintext in the
data segment, which starts zeroed, so it claims no erasure). The
recovery key must appear nowhere on the whole disk (ESP, header and
ciphertext alike, read after the clean page cache is dropped), nowhere
in the opened volume's plaintext, on the console or in any
`/proc/*/cmdline` sampled while the service ran, and the service leaves
no workspace. The protector secret never leaves td-install, so the
oracle cannot look for it; it requires instead that every sampled
cryptsetup command line is exactly one of td-install's documented
argument lists, that its environment is empty, and that `luksFormat` and
`luksAddKey` were among those sampled, the two invocations the host
requires to have been seen. Sampling may still miss a short-lived
process, so it is not proof that no other process carried a secret. It
derives the destination's partition devices from the kernel's block
inventory, never from `/dev/vda` literals. A leg cut off in the
recovery-key phase leaves both table ranges zero; increment 7's legs
follow below. The installed system is not booted: its release is
increment 6's oracle.

Increment 6's oracle, `td-recipe-eval qemu-boot-encrypted --tpm
/absolute/path/to/swtpm`, is likewise outside the integration tier and
an unprovisioned host gap without `--tpm`. Its legs are the device-bound
requirements above, except the no-TPM disclosure, which `qemu-boot-live`
proves by installing (`qemu-install-encrypted`'s no-TPM leg reviews and
withdraws); `qemu-boot-live`, which attaches no TPM, also keeps proving
the live selector's skip. A fresh swtpm state stands in for a cleared or
different TPM: a cleared TPM's new storage primary seed has the same
effect on these policies, since no protector sealed under the old
primary loads. There is no test-only selector build: the oracle boots
the selector td ships. The unseal after the cap is the deployment
initramfs's defence-in-depth attempt ("Boot and authority boundaries").

It installs as increment 5's installed leg does, with one difference:
after the guest's checks it writes the recovery key to a virtio serial
port that only the host reads, never to the console. Firmware writes its
own console to every ISA serial port, so the channel is not one. The
host keeps the key in memory and deletes the port's file. Every later
boot runs the same firmware code from a fresh copy of the variable
template. Unless a leg says otherwise, it runs under the installed
machine's swtpm state with only the installed disk, or a copy of it,
attached. The exceptions are these: the live leg attaches its medium
over USB; the fresh-TPM leg runs under a new swtpm state; the
header-state and inspection guests boot a fixture ISO beside the disk;
and the inspection guests attach no TPM.

- **First boot.** It must print the first-boot token's release, the cap,
  the six transition commits in plan order, and the volume opened with
  keyslot 2 as the `td-selector` mapping. After the selection, the
  deployment initramfs's post-cap check must report a policy refusal of
  the device-bound token, and the system must acknowledge boot success.
  The account's serial shell writes a file into its home.
- **Second boot.** It releases the device-bound token with no
  interaction and tests keyslot 2. The shell reads the file back from
  the same home.
- **Host check after the second boot.** The host parses both header
  copies: keyslots 0 and 2, and one device-bound token, 1, on keyslot 2.
  The data segment must hold neither a plaintext marker nor the file's
  contents.
- **Interrupted transition.** Four legs boot copies of the disk as
  installed. The host kills qemu when the console reports the keyslot,
  token, test and destroy commits (steps 2 to 5), polling the console
  every 5 ms in these boots. The header is read as cryptsetup and td's
  reader read it: the newer checksum-valid copy, with a torn copy or one
  a write behind reported. Each cut must leave exactly its own commit's
  state (the token and test commits leave the same header). The guest
  keeps committing until the kill lands, so a cut that leaves a later
  state interrupted nothing. It is a miss, never a pass: the leg tries
  again on a fresh copy, up to three attempts in all, and fails if
  none lands. The next boot must finish a transition and bring the
  system up with one device-bound token on a keyslot other than 0.
- **Live medium.** The production live medium boots on the installed
  machine, with the same swtpm and the installed disk attached. It must
  cap PCR 12 before booting its own deployment, with no release line,
  and leave both header copies byte for byte unchanged.
- **Constructed header states.** A guest booted with the recovery key in
  its initramfs uses cryptsetup on the converged volume to build three
  states:
  - an orphan keyslot 1;
  - a superseded device-bound token 2 (the same sealed object) on its
    own keyslot 3;
  - a first-boot token 0 whose keyslot cryptsetup then killed, leaving
    it naming none.
  The next boot must release tokens 1 and 2, test keyslot 2, retire
  exactly those three states in any order and converge, and bring the
  system up.
- **Changed chain and fresh TPM.** These legs boot copies of the
  converged disk:
  - `\EFI\BOOT\BOOTX64.EFI` with its PE time stamp moved, found
    through the ESP's FAT;
  - `\EFI\BOOT\INITRD` with one cpio mtime digit moved;
  - a firmware boot entry carrying a load option, which the selector
    kernel's printed command line must show (and every other leg's
    must not);
  - a fresh swtpm state.
  Each must refuse the device-bound token (by policy, or at load for the
  fresh TPM), close the cap, release nothing and reach the recovery
  flow. The host answers its secret-line prompts over the serial
  console, three key prompts in turn:
  - an entry the recovery-key codec refuses, which prompts again;
  - a well-formed wrong key (one group's value moved and its Damm check
    digit made again), which keyslot 0 refuses, prompting again;
  - the recovery key, which opens keyslot 0.
  The console must never show the key. Then comes the reseal warning
  and question. The selector-image leg declines: the system boots once
  through keyslot 0 with the header unchanged. The other three confirm:
  - the reseal's commits run;
  - it reports its new keyslot;
  - the header holds keyslot 0 and one new device-bound protector;
  - the deployment initramfs refuses that protector after the cap;
  - the system boots;
  - the next boot of the same changed chain or TPM releases the new
    protector with no interaction.
- **Inspection.** A guest booted with the recovery key and every
  console the host kept, the installation's included, takes the volume
  key with `luksDump --dump-volume-key` into RAM. It requires the volume
  key (raw and hexadecimal) and the recovery key nowhere: not on the
  whole disk (read after the clean page cache is dropped), not in the
  read-only opened volume's plaintext, and not in those consoles. The
  volume key never leaves that guest's RAM. The interrupted,
  header-state and changed legs run the same guest, without the
  consoles, over each disk copy they booted, a missed cut's included,
  before deleting it.

Every console is captured whole: the host creates a private file empty,
QEMU writes every console byte to it with no cap, and the host reads,
removes and scans it as raw bytes. That creation and the absent cap make
the capture whole; the boot loop's bounded text tail, read from the same
file, serves only diagnostics and the leg's line checks, and requiring
the capture to hold that tail is a sanity guard rather than proof. The
final inspection guest's own console is checked only for the recovery
key, since the volume key never leaves that guest's RAM for the host to
look for.
Each capture is checked for the recovery key before the host keeps it:
its digits alone, and its groups joined by hyphens or by spaces. Each
medium that carried the key is deleted when its boot ends, and the
host zeroes the buffers it built that medium from. The ISO writer's own
copies and the host's process memory are not otherwise cleared.

The oracle can search only for keys it holds: the recovery key, and the
volume key it takes inside the inspection guest. The protector secrets
never leave td-boot inside the selector, so it cannot search for them
on the disk or in any console. Increment 5's text says the same of
td-install's. And it builds no test-only selector, so it cannot sample
the selector's cryptsetup command lines as increment 5 samples
td-install's: neither the protector's `open` and transition commands
nor the recovery flow's `cryptsetup open` with the recovery key.
Instead, td-protector's cryptsetup runner tests pin that each argument
list is spelled exactly, that a child gets no environment, and that no
key appears in argv.

Increment 7's evidence ("Keyboard console", "Activation") extends both
oracles. `qemu-boot-live`, which attaches no TPM, keeps proving the live
selector's skip, and its review must name unencrypted storage for want
of a TPM, with the keyboard console found (`disclosure=no-tpm`); its
consent prompt must show exactly `UNENCRYPTED STORAGE, AUTOMATIC LOGIN`
among its rows, and it installs and boots. It is the evidence that a
machine without a TPM installs unencrypted with that disclosure in the
plan consented to; `qemu-install-encrypted`'s no-TPM leg only reviews
and withdraws. td-init's tests drive the two-line order through scripted
lines (the first to complete is read, a partial entry on the other is
discarded, a skipped line, no line, end of input on either, a line whose
prompt write would block or is short finished on `POLLOUT` while the
other reads, a hard write error dropping a line, one terminal under two
names saved and restored once), and the keyboard-console probe's tests
run over fixture sysfs trees (a bound and an unbound fbcon, a dummy
console alone, key bitmaps with and without each needed bit, interior
zero words, over-long and malformed attributes).

- **Default wizard.** `qemu-boot-encrypted` gains a leg that boots the
  production live medium over USB with the swtpm, a display device and
  the PS/2 keyboard attached, and drives td-setup with physical keys as
  `qemu-boot-live` does. The review must name device-bound storage, its
  evidence line the disclosure set its page shows
  (`disclosure=device-bound`; td-setup's review tests pin each set's
  text), and td-authd's consent summary the same storage. It first opens
  the prompt and presses Escape on it with the target unwritten, and
  requires the screen closed and td-setup back at its settings with the
  target still unwritten: Escape before commit declines. It then reviews
  again and consents, and after Enter presses nothing: the compositor's
  returning notice must show in the attention screen's notice row, pixel
  for pixel, with no installed notice before it, and the screen must
  then close by itself within 30 seconds (td-compositor/DESIGN.md
  "Physical installation confirmation"). The completion page must then
  show the key. The host learns the key only from that page's pixels, by
  QMP `screendump`, never from a console or an evidence line. Its
  reference glyphs are the ten digits drawn by td-ui's own rasterizer
  (`Face` over the `jetbrains-mono-nerd-font` recipe's face, in the
  style and cell size the page draws the key in), as `qemu-boot-live`
  already draws the status bar's expected text (`update::BarText`); each
  digit cell must match exactly one of them, the row found by its seven
  hyphen cells at either scale td-setup may draw. It types the key back
  with physical keys. A misread fails closed: a cell matching no digit,
  or more than one, fails the leg before anything is typed, and a wrong
  digit is refused by td-setup's group check or by the service as 17,
  recovery key mismatch, so the leg fails and the installation never
  completes on a misread key. The installed disk then boots through
  firmware twice with nothing typed, as "First boot" and "Second boot"
  require.
- **Storage choice without the operand.** `qemu-install-encrypted`'s
  guest drives the service with no operand. With the swtpm and a display
  device attached its plans are device-bound and its legs run as above.
  Its no-TPM leg requires an unencrypted review naming the missing TPM in
  place of the operand's refusal. Two legs with the swtpm require an
  unencrypted review naming the missing keyboard console, and install
  unencrypted: one with no display device (`-vga none` and no other, for
  QEMU's default VGA would give OVMF a GOP and simpledrm a framebuffer),
  so that no framebuffer console binds, and one with a display device
  but no keyboard, its
  machine `q35,i8042=off` (which the host QEMU 10.2.1 accepts) with no
  USB or virtio keyboard attached. The guest records every review's
  storage and probe findings on the console, and the host requires
  exactly the expected one; it then reads an unencrypted leg's image
  itself: the installer's GPT, and a Btrfs volume with the reviewed
  identity and no LUKS2 header. The no-TPM leg withdraws its review and
  leaves its disk unchanged.
- **Recovery on the VT.** The changed-initramfs leg attaches a display
  device and answers its three secret-line prompts and the reseal
  question on the VT, through QMP key events to the PS/2 keyboard
  (`input-send-event`), typing nothing on the serial line. Its serial
  capture must hold the entry line and every prompt, which shows the
  serial line was offered each, and never the key. The other
  changed-chain legs answer on the serial line with the VT idle, as now.
- **Prefix.** Every existing `qemu-boot-encrypted` leg keeps passing
  with `console=tty0` in the built-in prefix, its serial capture and
  typing unchanged.

For the protected tier, require a real encrypted read/write roundtrip and
reboot persistence; wrong PIN, missing token and changed boot measurements
must refuse release. Verify PIN retry limits across process restart, a
different TPM, primary-token loss, and recovery with the original TPM
unavailable. Exercise both primary methods, interrupted
formatting/enrollment, key replacement, update, failed-candidate rollback,
and the complete selector-to-session path with one interaction. Record
ciphertext/header checks and absence of plaintext secrets in scratch
artifacts. Prove that recovery cannot silently log in, lock cannot be
bypassed, and injected input or a replayed approval cannot authorize an
operation. Hardware FIDO2 interoperability evidence is required in addition
to mocks.

Increment 8's evidence, by sub-increment:

- **8a.** td-secret's existing tests and `qemu-secret` guests pass
  unchanged over `td-fido`. td-tpm unit tests pin, against independent
  vectors (`td-tpm/tests/session_vectors.py`, a host fixture tool like
  td-secret's), the salted session's ECDH, KDFe and session key, its
  AES-128-CFB parameter encryption both ways, the HMAC session's command
  and response bytes, the PolicyAuthValue digest literal, authValue
  trimming, each new command's bytes, and the typed `TPM_RC_AUTH_FAIL`,
  `TPM_RC_LOCKOUT` and refused lockout-authorization replies.
  td-protector's pin the tpm-pin policy digest for fixed PCR values, the
  PIN codec's bounds (5, 6, 63 and 64 bytes, 0x1f and 0x7f refused, a
  space admitted), the authValue and lockout-token derivations against
  independent vectors, every protected token's exact encoding and
  refusals (a lockout tag under another volume key, another primary's
  Name), and, using the pinned cryptsetup on header files built in the
  test, the largest protected and upgrading headers, the U2 one by
  `reencrypt --init-only`, each with at least 1 KiB of the JSON area
  spare. Ignored emulator tests against the pinned swtpm: seal and
  unseal with the right PIN in a salted session; a wrong PIN refused
  with 0x98e and the counter raised, still raised after a swtpm restart;
  lockout after 32 wrong PINs with 0x921, the right PIN refused while
  locked out; a changed PCR 4 and a closed PCR 12 each refused without
  the counter moving; a fresh TPM state refusing at Load; another
  primary's Name refused before Unseal; and the lockout commands' take,
  proof, reset and clear, with a proof under a wrong authorization
  refused.
- **8b.** td-fido's tests drive the disk flows against the virtual
  authenticator with independent literal vectors
  (`td-fido/tests/disk_vectors.py`): the client-data hashes of every
  phase, the passphrase derivation, a signature under another key, an
  unenrolled credential, each PIN status, and a credential ID of 256
  bytes refused. The virtual authenticator gains a CTAP 2.0 mode whose
  hmac-secret has one secret: a key reporting only `FIDO_2_0` is
  refused before any PIN, and one claiming `FIDO_2_1` whose no-PIN
  probe returns the UV output is refused at creation, for disk and
  login credentials alike. td-boot's tests drive the worker over a
  scripted transport, its deadline and its refusal of none or two
  devices.
- **8c.** Release tests over the scripted TPM, a scripted FIDO2 worker
  and the scripted cryptsetup, which requires PCR 12 capped before every
  command: a PIN release; a codec-refused entry costing no TPM command;
  wrong PINs and their counts; lockout turning to another way; a changed
  chain, a fresh TPM and another primary each asking no PIN and
  printing the warning before any other prompt; each FIDO2 role and the
  recovery key releasing, a wrong recovery key prompting again after the
  cap; the reseal declined, confirmed, verified before the cap, and
  failing at its seal and verification; the pre-cap lockout rule
  refusing a seal; the take, proof, refusal of a TPM td cannot prove,
  counter report and reset, each after the keyslot test and never
  before; a FIDO2-primary header offering no reseal; no TPM device;
  every plan step interrupted, with the next boot completing it; and
  forged headers (a tpm-pin object under a policy that is not td's, a
  token naming another token's keyslot, a lockout token whose tag fails,
  bounds exceeded, mixtures outside the upgrade's states) each releasing
  nothing it did not verify and writing nothing outside the planner's
  rules. td-boot's tests hold the prompts and that no secret reaches a
  console line.
- **8d.** td-kexec's tests: the second member's exact 46 bytes after the
  key's, both pipes' refusals as the key's, and none without the flag.
  td-boot's: the record written after every protected release and after
  PCR 11, with method 0 after recovery; `admit`'s file checks, its
  exclusive create and its removal of the member on every path.
  td-authd's: the consumption order (unlinked, consumed marker created,
  then judged), a second record in one boot discarded, each refusal
  (UID, manifest ID, age, unenrolled, method 0), the protected marker
  making an unenrolled state lock, the admission byte only in the
  generation's first answer, a login-key operation on a protected
  machine authorized by a fresh recovery key or disk-token assertion
  whose derived passphrase opens its keyslot, and refused with a wrong
  one, a stale one, none (an admitted session included) or a validly
  signed assertion from a fido2 token added to the header without a
  keyslot it opens, and the last-key removal refused there. The
  compositor's: an admitted first paint unlocked, every other generation
  locked, and `Super+l` after admission locking.
- **8e.** FAT tests over images built in the test: admission of td's
  layout and refusal of a foreign `EFI/BOOT` directory or too little
  space; staging and both commits cut before, inside and after each of
  their writes, each leaving exactly the old pair, the old pair with
  complete staged entries, or the new pair, and the next update
  reclaiming leaked clusters; and an independent FAT reader agreeing.
  Prediction tests over event logs recorded from OVMF and from the
  hardware machines: the replay's equality with the PCRs, the
  substitution, and refusal of a log that does not replay, of none or
  two matching events, of a "Returning from EFI Application" action or
  a second boot application, of a log without SHA-256, and of a
  truncated or oversized log, with `EV_NO_ACTION` events skipped and the
  final-events table included. Commit tests: a staged `INITRD.NEW`
  holding another template with this selector's exact appended archive,
  another kernel, and a pair from `previous` each delete the staged
  update after the cap with no object imported; a prediction failure
  with an authentic pair keeps it and writes the retry token, and only
  the retry's failure with that token verifying offers `update`, which
  reboots at once; a retry token forged, copied from another volume or
  naming another pair offers nothing.
- **8f.** Planner tests drive the upgrade from each state U0 to U5 to
  every cut in the table, crashes inside reencryption in both resilience
  modes among them, and require one final header: tpm-pin or
  fido2-primary, fido2 and recovery-key protectors only, the lockout
  token in the TPM shape, no `td-upgrade` token, and the recovery key
  opening it. A cancelled upgrade and an interrupted phase 1 each leave
  a device-bound header without pending tokens. U5 completes with a PIN
  and no recovery key. A missing mains supply in U0 upgrades nothing.
  Phase 1 refuses the TPM shape on a TPM whose `lockoutAuthSet` is set;
  a TPM that becomes unusable before U4 ends the upgrade without a
  tpm-pin protector.
- **8g.** Each operation's authorization (a stale assertion, another
  operation's description, a token not enrolled, a wrong recovery key),
  the minimums, a PIN change and a TPM clear each cut at every commit,
  and each declined (an empty new PIN, an answer other than `clear`)
  removing its request.
- **8h.** `td-recipe-eval qemu-boot-protected --tpm
  /absolute/path/to/swtpm`, outside the integration tier and an
  unprovisioned host gap without `--tpm`, as `qemu-boot-encrypted` is.
  Its FIDO2 tokens are host-side virtual authenticators attached through
  QEMU's `usb-redir` device to a socket the host serves, speaking the
  usbredir protocol for one USB HID FIDO device each, so the shipped
  selector and the running system see them as hardware keys behind
  xHCI; their CTAP state machine is the virtual authenticator
  (td-secret/DESIGN.md, "Virtual authenticator", then td-fido's), with
  its test-only ES256 signer and persistent state, which the host
  evaluator compiles as a host check source. A host QEMU built without
  usbredir is an unprovisioned host gap (exit 69). Because the host
  holds each virtual credential's secrets, it can compute every
  hmac-secret output and derived passphrase, and it searches for them
  as it searches for the recovery key. It installs device-bound as
  `qemu-boot-encrypted` does, seeds an enrolled login record as
  `login-desktop` does, attaches a display, and runs these legs:
  - **Upgrade, TPM primary.** Phase 1 as root over the serial shell
    with token A as recovery; the upgrade boot with A, the PIN twice and
    the recovery key; reencryption's progress, with journal resilience
    on the virtual disk, which states no atomic write unit; then the
    host parses both header copies (tpm-pin, fido2, recovery-key and
    lockout tokens and their keyslots, no device-bound token, no
    `td-upgrade` token), requires the recovery key still to open the
    volume, and requires every data-segment sector to differ from its
    copy taken before the upgrade. The inspection guest takes the new
    volume key, which must differ from the old one it took before.
  - **Interrupted upgrade.** Copies of the disk cut at each row of the
    upgrade's crash table, inside reencryption at three points; each
    next boot converges to the same header shape and the system boots,
    the U5 copy with the PIN alone.
  - **PIN boot and one interaction.** The PIN alone releases; the first
    frame (QMP `screendump`) is the session, not the lock surface;
    `Super+l` then locks. The record copied back into `/run/td-admit`
    by root within 300 seconds after consumption, followed by a restart
    of the paired pair, leaves the lock surface.
  - **Wrong PIN and lockout.** Wrong PINs lower the console's count; 32
    lock the TPM out; the lockout persists across a QEMU and swtpm
    restart; token A then releases as recovery, the first frame is the
    lock surface, and the console reports the count and resets it, so
    the next boot takes the PIN.
  - **Predicted update.** `selector-update` from an acknowledged
    deployment; the next boot asks for the PIN, pre-seals and commits;
    the boot after it asks for the PIN alone and retires the old
    object. A staged `INITRD.NEW` built from another template with the
    correct appended archive is deleted after the cap and the next boot
    asks for the PIN on the old chain.
  - **Changed chain, fresh TPM, no TPM.** A changed `INITRD`, a changed
    `BOOTX64.EFI` and a load option each print the warning before any
    prompt and ask no PIN; token A releases, the reseal is confirmed
    with a new PIN and verified before the cap, and the next boot takes
    that PIN. The recovery key does the same. A fresh swtpm state does
    the same and the reseal takes its lockout. With no TPM attached
    token A releases and the console says the cap was skipped.
  - **TPM td cannot prove.** The host sets the lockout authorization of
    a fresh swtpm state to a value of its own, with td-tpm's command
    compiled as a host check source; the reseal is refused before any
    new PIN, naming the reason, and token A and the recovery key still
    boot.
  - **Tokens.** An unenrolled token C is refused as not enrolled; two
    keys at once and none are refused; a no-PIN assertion opens nothing.
  - **FIDO2 primary.** A second installation upgraded with A as primary
    and B as recovery: A's boot is admitted and B's is not.
  - **Recovery on an unenrolled login state.** With the protected marker
    present, root removes the login record; a recovery-key boot starts
    locked; the next PIN boot's first frame is the session, where a key
    is enrolled with the recovery key typed as the disk proof, and a
    second attempt without a proof is refused.
  - **Management.** The recovery key, authorizing, adds token B; B,
    authorizing, adds C and removes A; A is then refused and the header
    holds no keyslot for it; a PIN change requested on the running
    system takes effect at the next boot, the old PIN refused after it;
    a TPM clear requested and confirmed leaves the next boot asking for
    a token; a removal below the minimum is refused.
  - **Rollback.** A pending deployment that fails its attempts falls
    back to `previous` with no new PIN or token asked.
  - **Inspection.** Neither PIN, any derived passphrase or hmac-secret
    output, the lockout authorization, the old or new volume key nor the
    recovery key appears on the whole disk, the ESP, the opened volume's
    plaintext, any console or any sampled `/proc/*/cmdline`, and the
    swtpm channel's recorded traffic carries no protector secret in
    clear; the record appears on no disk.
  The hardware evidence follows on a discrete TPM 2.0 machine (not the
  T430s, whose TPM is 1.2), a firmware-TPM machine, and a machine whose
  TPM a Windows installation provisioned (refused until cleared, then
  admitted), with two FIDO2 keys of different vendors reporting
  `FIDO_2_1` with hmac-secret and a client PIN, and one CTAP 2.0-only
  key, which must be refused: an upgrade, PIN boots, a predicted
  update, a wrong PIN's count, each key's boot and the first frame,
  recorded in that commit's message before item 9 lands. The
  single-sector ESP commit stays a disclosed assumption on SATA and eMMC
  devices unless that evidence includes a power-cut series on one.
- **8i.** OVMF's Secure Boot build with SMM (`TD_QEMU_EFI_CODE` naming
  it; its absence an unprovisioned host gap): enrollment's first step
  as a predicted staged update that writes the first `config` tag and
  boots with the PIN alone, then its second from setup mode with KEK
  and db left populated, replaced whole and read back;
  refusal with an option ROM in PCR 2's log (QEMU's `romfile` on a PCI
  device); the sealed image booting with Secure Boot on after a reseal
  naming PCR 7; an unsigned image refused by firmware with no td line on
  the console; a replaced `TDCONFIG` booting the genuine selector, which
  warns, releases nothing to the PIN, and after a token release halts
  before selecting; a boot entry whose load options name `initrd=` an
  initramfs whose `/init` writes a marker to the console, which never
  appears while the built-in `/init`'s line does;
  Secure Boot turned off refusing the tpm-pin protector; a staged signed
  update predicted and released with the PIN alone; and a PIN change
  after which the db key signs only under the new PIN.
