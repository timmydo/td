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

The current image remains unencrypted and auto-logs in. No increment may
describe enrollment, disk confidentiality, or protected login as shipped
until its complete boot and recovery path passes the acceptance tests below.
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
header and keyslots makes a disposed disk unreadable. On the same machine it
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
as described below, and never through the administrative escape hatch.

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
   boot reaches recovery.
2. The selector tries every td token, up to a fixed bound, device-bound
   tokens first.
3. Only when the first-boot protector alone releases, it seals the
   device-bound protector described below to the observed PCR 4 and PCR 9
   values and a literal-zero PCR 12, then unseals that new protector once
   to verify it.
4. Whether or not any unseal succeeded, it extends PCR 12 with a fixed td
   release-cap event and requires an exact readback, as the PCR 11
   measurement does. A failed or uncertain extension zeroes the released
   secret, refuses boot and requires a platform reset.
5. Only after the cap does cryptsetup parse the header. After step 3 it
   commits the new protector's keyslot and token, verifies that the keyslot
   opens, and only then destroys the first-boot keyslot and token. When a
   device-bound protector released, it only removes a leftover first-boot
   or superseded keyslot and token. Then it opens the volume.

The cap closes release until the next platform reset on every later path,
including refusal, recovery and `kexec`. The installed selector and the live
selector both cap PCR 12; the live selector does so unconditionally at boot.
No other td component extends PCR 12.

The first-boot protector's policy names PCR 12 alone. PCR 4 holds the
firmware's measurement of the selector EFI image (`EFI/BOOT/BOOTX64.EFI`),
whose built-in command line names its initrd. PCR 9 holds the EFI stub's
measurements of any firmware load options and of the prepared selector
initramfs (`EFI/BOOT/INITRD`). The selector refuses to seal while either
PCR is unmeasured and enters recovery instead. An interrupted first boot
keeps the first-boot keyslot and repeats the transition, discarding any
orphaned td keyslot or token; cryptsetup leaves a token whose keyslot was
destroyed naming no keyslot, and td's reader reports it without releasing
it. Until the installed selector's first boot,
release is not bound to the boot chain: any OS that leaves PCR 12 at zero,
other than td's live medium, can unseal the first-boot protector. The first
boot also adopts that boot's load options, so a one-time firmware boot-menu
entry can send later ordinary boots to recovery. The review discloses both.

The policy is exact PolicyPCR without PolicyAuthorize, so it never names a
predicted value. A change to the selector image, its initramfs or its load
options, a cleared or replaced TPM, or a firmware change that alters PCR 4
or PCR 9 refuses release. Firmware code and configuration PCRs are excluded
so that most firmware updates are intended to keep releasing.

When release fails, the selector enters a recovery flow on its console. It
never falls back to plaintext, retries with weaker policy, or skips the
volume. After the cap, a correct recovery key opens the volume; the
selector then offers, with an explicit console confirmation, to seal a
device-bound protector to the observed PCR 4 and PCR 9 values and a
literal-zero PCR 12, commit its keyslot and token, and only then destroy
the old device-bound keyslot and token. Its release is first proven on the
next boot, and the recovery keyslot remains, so a failure returns to
recovery. Recovery without that confirmation boots once and leaves the
protectors unchanged. Nothing reseals automatically, because that would
adopt a changed boot chain without its owner's decision. The live medium
can open the volume with the recovery key for data access.

The recovery key is 128 random bits read from `/dev/random`, encoded in
grouped decimal digits with a check digit per group so that entry does not
depend on the keyboard layout ([td-protector](../td-protector/DESIGN.md)
"Recovery key" owns the encoding). Its keyslot passphrase is the 48 digits
without separators, so stock cryptsetup opens the volume with it from any
medium when given the 48 digits alone; only td's own entry tolerates
separators between groups. The installer displays it once on its completion
screen, requires it to be typed back, and stores no copy. Completion waits
for the type-back. If the installer is lost before it, the installation is
withdrawn like any failure after layout (DESIGN.md "Device-bound
formatting"), because recovery cannot be declined in this tier.

The installer seals the first-boot protector on the live medium under
td-protector's first-boot policy. Sealing reads no PCR, and TPM2_Create
does not evaluate the policy, so the seal does not depend on PCR 12's
value at that moment. The installer never unseals that protector. Until
increment 6 adds the live selector's cap, a live boot leaves PCR 12 at
zero; only the test-only reach of increment 5 (below) runs there. After
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

td has no selector-update operation. One that is added must specify a
crash-safe protector transition for both ESP files before it ships; until
then a changed selector reaches recovery and its confirmed reseal.

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
Before activation, the service started with the device-bound operand
probes for a usable TPM and refuses to start without one; it never falls
back to an unencrypted volume. The default wizard's unencrypted disclosure
changes only at activation (increment 7).

Upgrading to the protected tier enrolls and verifies its protectors, then
re-encrypts the volume online to a fresh volume key, keeping only those
protectors' keyslots and so dropping the device-bound protector and
recovery key. Re-encryption defeats retained copies of the old key and
header; it does not remove persistence left by anyone who was root on the
device-bound system, which with automatic login and `su` is anyone at the
keyboard. Such prior compromise is outside Scope, so a protected-tier
claim on an upgraded volume is no stronger than the device-bound system's
integrity before the upgrade.

## Device-bound formatting

Increment 5 formats device-bound volumes without activating them. Until
increment 7, `td-install serve` formats one only when its caller passes the
control-plane storage operand (INSTALLER.md "Installation service core").
td-authd never passes it; the encrypted-installation oracle does. No
review, wizard page or request selects it: storage policy is not a
caller-selectable flag. The default installation stays unencrypted, and no
text may describe disk confidentiality as shipped. Increment 7 deletes the
operand in the landing that makes the tier the default.

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
change moves it. Volume fit and the minimum volume size subtract those
16 MiB. The protected-tier upgrade re-encrypts online with checksum
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

## Authentication and recovery

This section and the next govern the protected tier except where they name
the device-bound tier. Protected unlock is **TPM 2.0 plus PIN**. The PIN
authorizes a hardware-held secret with persistent dictionary-attack
protection, not a short LUKS passphrase susceptible to offline guessing.
Release also requires an approved measured boot state. In this tier TPM
possession alone never logs a person in. An enrolled **FIDO2 token plus
PIN** is an alternative primary method and the recovery method when the TPM
is lost or replaced. Require the token's `hmac-secret` capability and user
verification; touch alone is insufficient. These are alternative
protectors, not a requirement to present both devices.

Enrollment generates a random volume key on the machine, binds protectors
to an explicit account, and verifies primary and separately stored recovery
token unlock before declaring success. A FIDO2 primary needs a second token.
Recovery must not require the failed TPM or its old PCR state. It enters a
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

In the protected tier, authenticate the initial boot code and measure the
selector, its initramfs and command line before selector-stage release.
That trusted selector authenticates the selected deployment after opening
the volume; it cannot require a measurement of unreadable deployment bytes
to unlock that volume. Measure the verified deployment and its boot
arguments before second-stage release or handoff, with a policy that covers
both stages explicitly. Firmware/key provisioning and TPM policy-authorized
updates must preserve both `current` and the approved `previous` fallback.
Exact-PCR enrollment without an update/recovery policy cannot ship as the
default. The device-bound tier is the stated exception: it authenticates no
pre-selector code and releases on exact PolicyPCR without PolicyAuthorize.
Its update and recovery policy is the recovery key with a confirmed
selector-stage reseal. It releases only at the selector stage, so
deployment updates never change its release values.

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
tier may adopt. After authenticating the selected deployment and making its
configured PCR 11 measurement, the selector copies the verified
deployment initramfs into an unlinked memfd, verifies that copy against the
manifest, appends one 4-byte-aligned cpio archive holding only the volume
key, seals the
memfd and passes it to `kexec_file_load`. The appended archive is outside
the signed manifest and the measured event. The deployment initramfs reads
the key from its RAM-backed root, opens the volume through a descriptor,
and removes the key file before starting the system. It refuses a key on an
unencrypted volume and an encrypted volume without a key. Neither stage
writes the key to any block device. Removing the file retires the key from
the filesystem only: copies remain in the selector's memfd pages, the
`kexec` segments and the second kernel's freed initrd region, which memory
extraction, outside Scope, could read. The memfd and sealing syscalls amend
UNSAFE.md in the increment that adds them.

The authenticated user gets an ordinary session. Later elevation uses the
existing operation-to-principal policy and secure-attention path: one
request, one explicit approval, one broker-performed operation. Applications
receive no general root process or reusable authority. Protected consent is
the normal interaction; changing unlock credentials or recovery policy
requires fresh hardware-backed PIN verification bound to that operation.
No client surface, synthetic input, remote-control interface, or untrusted
same-uid process may impersonate the trusted UI or approve a request.

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
   the no-TPM refusal and crash-safe enrollment, reachable only through the
   service's storage operand ("Device-bound formatting"); preserve
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
   deployment-initramfs unlock, extending D6's binding to both initramfs.
   Exercise them together before activation.
7. Activate the device-bound tier as the installer default on machines with
   a usable TPM 2.0 whose selector console accepts keyboard input, amending
   INSTALLER.md's disclosures in the same landing. A platform without such a
   console is not activated until the selector gains one. Automatic login
   remains and stays disclosed.
8. In successive increments, add authenticated firmware entry, TPM PIN
   release and update policies, FIDO2 primary/recovery, the verified account
   handoff and the re-encrypting upgrade. Exercise them together before
   activation.
9. Activate the protected tier only with trusted login/lock
   (td-login/TOKEN-LOGIN.md) and operation consent, with no automatic login
   in that profile, and only after `su` and root's empty shadow field have
   retired as APPLICATIONS.md §L.1, "Retiring the escape hatch",
   specifies.

## Acceptance evidence

Use disposable QEMU disks and the existing pinned TPM emulator oracle; no
test touches an operator's disk or enrolls their hardware.

For the device-bound tier, install under UEFI firmware that measures into
the emulated TPM; the firmware oracle must attach that TPM. Require a real
encrypted read/write roundtrip and reboot persistence with no interaction,
and a first-boot transition interrupted between its token, keyslot and
destroy commits that completes on the next boot. A changed selector image,
initramfs or load option and a cleared or different TPM must refuse release
and reach the recovery flow, where the recovery key opens the volume and a
confirmed reseal restores unattended boot. An unseal after the PCR 12 cap
and the live medium booted on the installed machine must both refuse
release. Install without a TPM and show the unencrypted disclosure in the
consented plan. Record ciphertext/header checks and the absence of the
volume key, protector secrets and recovery key from the ESP, logs, scratch
artifacts and command lines.

Increment 5's encrypted-installation oracle is separate from the
always-run integration tier and provisioned like `qemu-secret-system`,
with an explicit swtpm path; without one it is an unprovisioned skip. Its
guest drives the service with the device-bound operand onto a disposable
disk, as both of its peers, typing the displayed recovery key back. It
then opens the volume with that recovery key and checks the ciphertext
(no Btrfs superblock or staged plaintext in the data segment), the token
read back through td's reader, and the published deployment. It checks
that no recovery key or protector secret appears on the ESP, in the
workspace or in any `/proc/*/cmdline` sampled while cryptsetup runs. It
derives the destination's partition devices from the kernel's block
inventory, never from `/dev/vda` literals. A no-TPM leg requires the
operand's refusal with the disk unchanged.

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