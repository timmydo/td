# td-protector: the device-bound disk protector

td-protector is the disk-protector policy of
[ENCRYPTION.md](../td-install/ENCRYPTION.md)'s device-bound tier, carried
for both of its consumers: the installer, which probes for a usable TPM
with the observed-policy read before a device-bound service starts and
seals and verifies the first-boot protector (increment 5), and the
selector, which releases, seals
the device-bound protector and caps PCR 12 (increment 6), and whose
volume discovery already identifies a td LUKS2 volume through the header
identity below (td-install/DESIGN.md "Read-only volume discovery
primitive"). It runs over the
shared TPM 2.0 client [td-tpm](../td-tpm/DESIGN.md) and owns only what
ENCRYPTION.md makes disk-specific: which PCRs a protector names, the
release cap, and the protector secret. It is pure `std`, depends only on
td-tpm and the std-only td-json, forbids `unsafe` and adds no syscall
surface to `UNSAFE.md`: its cryptsetup runner's pipes and children are
std's.

Increment 5 adds the two persisted formats the installer writes and the
selector reads: the recovery key's encoding and the td LUKS2 token, with
the bounded reader that finds tokens in a LUKS2 header before the cap.
Increment 6 adds what the installer and the selector share for keyslot
changes: the cryptsetup runner ("Cryptsetup runner"), moved from
td-install with DESIGN.md "Device-bound formatting"'s descriptor rules
unchanged, and the pure transition planner ("Transitions"); then the
release orchestration that runs ENCRYPTION.md's release order ("Release
orchestration"), which the installed selector, td-boot, calls.

## Policies

Every policy is exact PolicyPCR over the SHA-256 bank then
PolicyCommandCode(Unseal), td-tpm's `PcrPolicy`, with no PolicyAuthorize.

- **First boot.** `first_boot_policy` selects PCR 12 alone at its literal
  reset value of zero. It reads nothing from the TPM, so the installer can
  seal it from the live medium, whose selector has already capped PCR 12.
- **Device bound.** `observed_policy` reads PCR 4 (the selector EFI image)
  and PCR 9 (load options and the selector initramfs) in one PCR_Read and
  refuses either one at all zeros, as unmeasured. Its composite is those two
  observed values and a literal-zero PCR 12. PCR 12 is never read: the
  policy names the reset value every protector requires, not the value at
  seal time. The first-boot transition seals before the cap, but the
  confirmed recovery reseal runs after it, when PCR 12 is no longer zero,
  and a read value there would seal a protector that never releases.

`DEVICE_BOUND_PCRS` and `FIRST_BOOT_PCRS` name the selections. The
first-boot policy digest and the cap event are pinned by literal tests.

## Protector secret

`Secret` holds 32 bytes read from `/dev/random` in one heap allocation.
`/dev/random` blocks until the kernel CSPRNG is initialized, which
`/dev/urandom` does not: the selector generates protectors in early boot,
and a weak secret behind a minimal-cost PBKDF2 keyslot would bypass the
TPM. td-tpm's session nonces are not secrets and stay on `/dev/urandom`.
`Secret` is neither `Debug`, `Display` nor `Clone`; `expose` lends the
bytes to a keyslot operation, which must not copy them into argv, the
environment, a log, a store output or a persistent file. `seal` copies the
secret into the one payload buffer td-tpm zeroes as it is marshaled;
`unseal` moves td-tpm's returned payload into a `Secret`, refusing any
payload that is not exactly 32 bytes. The secret on drop, the returned
payload and the seal copy are zeroed with td-tpm's `zero`, which keeps the
stores observable through `black_box`; this is best effort in safe Rust.

Each TPM operation runs on its own client. `seal` and `unseal` consume
their `td_tpm::Client`, so every transient handle and session is flushed
when the operation ends, whether or not it succeeded. Callers open a fresh
client (`Device::open`) for each operation, including the cap and the
installer check.

Protectors are sealed under td-tpm's unpersonalized owner storage primary
with `SEALED_ATTRIBUTES` (fixedTPM, fixedParent, noDA, policy-only). Unlike
td-secret's stores, a protector carries no personalization binding: each
protector opens only its own keyslot, so substituting another sealed
protector of the same machine gains nothing that protector does not already
grant, and ENCRYPTION.md asks for none.

## Unseal outcomes

`unseal`'s `UnsealError` separates the refusals that release nothing
from everything else, so that the deployment initramfs's post-cap check
(ENCRYPTION.md "Boot and authority boundaries") halts on no expected
refusal. It types td-tpm's `UnsealError`, which carries the command and
response code of the TPM refusal that ended the unseal, if one did:

- `PolicyRefused`: only `TPM_RC_VALUE` on PolicyPCR's first parameter
  (0x1c4, `POLICY_PCR_REFUSED`), PCRs that differ from the policy's,
  and `TPM_RC_POLICY_FAIL` on Unseal's first session (0x99d,
  `UNSEAL_POLICY_REFUSED`).
- `LoadRefused` (`load_refused`): a refusal before the sealed object is
  loaded. Without a parent there is no Load, and without a loaded object
  no Unseal, so none of these can indicate a release:
  - TPM2_Load's `TPM_RC_INTEGRITY` (`RC_FMT1` 0x080 + 0x01f = 0x09f) at
    any handle or parameter position: 0x09f, on the parent handle
    0x19f, on `inPrivate` 0x1df, on `inPublic` 0x2df (TPM 2.0 Part 2
    6.6: `TPM_RC_P` 0x040, handle and parameter numbers in 0xf00). It
    is the answer of a cleared or different TPM whose storage primary
    did not create the sealed object: the private area's integrity
    HMAC is keyed from the parent's seed, so a new seed fails there
    before anything else is checked. swtpm 0.10.1 answers 0x1df, which
    the emulator oracle pins. The code on a session position (0x89f
    and up) is `Other`.
  - CreatePrimary of the owner storage primary refused by the owner
    hierarchy's state (`HIERARCHY_REFUSED`): `TPM_RC_BAD_AUTH`
    (0x080 + 0x022) on session 1 (`TPM_RC_S` 0x800 + `TPM_RC_1`
    0x100), 0x9a2, an owner password set since installation; and
    `TPM_RC_HIERARCHY` (0x080 + 0x005) on handle 1, 0x185, the storage
    hierarchy disabled. Either would otherwise make every recovery
    boot's deployment check halt.
- `NoSha256Bank`: `release_policy`'s PCR_Read of PCRs 4 and 9 found no
  SHA-256 bank (td-tpm's `PcrReadError::NoSha256Bank`, kept typed
  through `release_policy` and `unseal_token`), so a device-bound token
  sends nothing to unseal: no SHA-256 PolicyPCR can be met without the
  bank, which only a platform-authorized allocation at the next reset
  restores. The selector's release logs it as any refusal; the
  deployment initramfs's check proceeds on it, so a machine whose bank
  was deallocated after installation boots with its recovery key.
- `Other`: a transport error, a reply that does not answer the command,
  every other response code, and a payload other than 32 bytes.

So that a changed boot chain and a closed cap are the TPM's answers and
not a local mismatch, td-tpm's `unseal_object` checks the sealed public
area's format but leaves comparing its authPolicy to the TPM, and a
device-bound token is unsealed under `release_policy`: PCRs 4 and 9 as
they read now, unmeasured or not, and a literal-zero PCR 12. Before the
cap a changed chain passes PolicyPCR and is refused at Unseal (0x99d);
after it PolicyPCR refuses (0x1c4). `observe` is the seal's read, which
refuses an unmeasured PCR 4 or 9 as `ObserveError::Unmeasured`;
`observed_policy` is the same with its refusal as text.

## Release cap

`cap` reads PCR 12 and requires zero, extends it once with `cap_event()`,
the SHA-256 of `td/disk-protector/release-cap/v1`, and requires the exact
readback `SHA256(zero32 || cap_event())`. Only a failed prior read is
tried once more, since a read cannot move PCR 12; the extension and the
readback are never retried. Its error is a typed `CapError`, each
displayed with PCR 12 context:

- `AlreadyClosed`: PCR 12 read non-zero, so nothing was extended. Every
  protector policy requires PCR 12 at zero, so no TPM release is possible
  this boot.
- `NoSha256Bank`: the prior read found no SHA-256 PCR bank allocated
  (td-tpm's typed `PcrReadError::NoSha256Bank`), so nothing was
  extended. Every protector policy is a SHA-256 PolicyPCR, which such a
  TPM cannot satisfy, and a bank allocation takes platform authorization
  and a reset to apply, so no TPM release is possible this boot.
- `Uncertain`: a PCR 12 read (both tries of the prior one) or the
  extension failed in transport or was refused, so the cap's state is
  unknown: a failed prior read leaves PCR 12 unmoved but unverified, and
  a failed extension or readback may or may not have moved it. Either
  way release cannot be shown closed.
- `Mismatch`: the readback differs from the expected value.

The caller's contract follows ENCRYPTION.md's release order. The selector
tries every td token, up to the fixed bound; when the first-boot
protector alone released, it performs the first-boot transition's seal
and verification unseal; then it caps exactly once, whether or not any
unseal succeeded, before cryptsetup parses the header. ENCRYPTION.md's
release order (step 4) says what each `CapError` leads to, and
`release::release` is that caller ("Release orchestration"). The live
selector, which releases nothing, caps once before it reads its medium
(MEDIA.md "Live boot"). No other td component extends PCR 12.

## Installer check

`verify_first_boot_object` runs the first-boot policy in a TPM trial
session, which td-tpm refuses unless the TPM's PolicyGetDigest equals the
local digest, then requires the sealed object's public area to carry that
digest as its authPolicy with the fixed sealed attributes. It then loads
the public and private pair under the storage primary with td-tpm's
`load_and_flush`, so the TPM verifies the private area's integrity, and
flushes both. Nothing is unsealed: the installer never unseals the
first-boot protector, and the live selector has already capped PCR 12.

## Recovery key

A recovery key is 16 bytes from `/dev/random`, read as `Secret` is. They
are eight big-endian 16-bit values, each written as five decimal digits,
`00000` to `65535`, followed by one Damm check digit over those five: the
standard order-10 quasigroup table, starting from zero. That makes eight
groups of six digits, 48 digits and 128 bits. Damm detects every
single-digit substitution and every adjacent transposition within a group.

The keyslot passphrase is the 48 ASCII digits with no separator. Stock
cryptsetup derives the keyslot key from exactly the bytes it is given,
so whoever opens the volume without td types the 48 digits alone; only
td's own entry tolerates the separators below. The display form joins
the groups with hyphens. Entry admits spaces and hyphens only between
groups and around the whole, and refuses, naming the group where it can,
a byte other than a digit, space or hyphen (a tab or line terminator
included, so a caller strips a line's terminator first), a separator
ending a run of digits that is not whole groups, a digit count other
than 48, a value above 65535 and a wrong check digit. A key, its
passphrase and its display form are zeroed on drop and are neither
`Debug`, `Display` nor `Clone`.

`recovery::RecoveryKey` carries it, held as its 48 passphrase digits so
that producing either text cannot fail. `generate` reads `/dev/random`
through the same injectable reader as `Secret` and encodes the 16 bytes,
zeroing them; `parse` scans at most 256 bytes and returns an `EntryError`
naming the group, or the one-based byte offset of a refused byte, never
the digits; a separator after a wrong number of digits names the group
the run began in. `check_partial` judges an entry still being typed, for
the installer's type-back feedback as each key is pressed: it refuses
as `parse` does, checking each group once it is whole, counts more than
48 digits as `Length`, and otherwise answers how many digits there are,
so a short entry is incomplete rather than refused. It returns no
digits. `passphrase` and `display` return a `RecoveryText`,
one allocation at exactly its 48- or 55-byte length, for the keyslot
operation or the completion screen; UI code draws it from the borrow and
never copies it into an ordinary `String` or other buffer that is not
zeroed. `matches` compares two keys over every digit, for the
installer's type-back. Whoever holds the entered bytes zeroes them.

## LUKS2 tokens

A td token is a LUKS2 token object with exactly these five keys:

```text
{"type":"td-protector","keyslots":["N"],"role":"ROLE",
 "public":"HEX","private":"HEX"}
```

`keyslots` holds one keyslot number, 0 to 31, as LUKS2 writes it: a
decimal string without leading zeros. `role` is `first-boot` or
`device-bound`. `public` and `private` are the sealed object's
TPM2B_PUBLIC and TPM2B_PRIVATE contents in lowercase hexadecimal, bounded
to 256 and 512 bytes. A missing, extra or duplicated key, another type in
any field, or a value outside these bounds refuses the token. Other token
types are ignored.

A td token whose `keyslots` array is empty is an orphan. cryptsetup
strips a destroyed keyslot from every token that names it and keeps the
token, so a transition interrupted between destroying a keyslot and its
token leaves one. An orphan must otherwise be the format; it is never
released, it counts toward the four-token bound, and the reader reports
its token number and role so that the transition removes it
("Transitions"). cryptsetup's own token validation also admits a token
naming several keyslots; td writes none, and a td token naming more than
one refuses the header.

The reader reads the header copy cryptsetup will use, by cryptsetup's own
rule, before any C parser runs. A copy is valid when its binary header has
its position's magic, version 2, its own offset in `hdr_offset`, a
`hdr_size` from 16 KiB to 4 MiB and, for the secondary, an offset equal to
that size, and when the SHA-256 over the binary header with its checksum
field zeroed and the whole JSON area equals the stored checksum; a copy
the medium ends inside is invalid, and any other read error refuses. The
secondary is read at the primary's `hdr_size` when the primary is valid,
and otherwise at the first of cryptsetup's fixed secondary offsets that
holds a valid copy. Of two valid copies the higher `seqid` is used. Copies
with equal `seqid` must agree in `hdr_size`, label, checksum algorithm,
UUID, subsystem and JSON area, or the reader refuses them as disagreeing;
agreeing, the primary is used. A checksum algorithm other than `sha256`
refuses the header rather than the copy, so that td never picks a
different copy than cryptsetup would.

td parses only a used copy of the 16 KiB it formats (ENCRYPTION.md
"Device-bound formatting"): any other `hdr_size` refuses the header
before its JSON reaches td-json, which therefore never sees more than the
12 KiB JSON area from a header (cryptsetup's metadata dump, below, is
bounded apart). That area must hold one JSON object followed only by NUL
bytes, parsing without a duplicate key. Its `tokens` and `keyslots`
objects are keyed by numbers 0 to 31, each token names keyslots that
exist (cryptsetup's own token rule), and at most four td tokens, orphans
included, are present: a first-boot and a device-bound protector, one
superseded and one an interrupted transition left. "Transitions" says
how a plan that adds stays within that bound.

td's checks of the used copy refuse rather than fall back to the other
copy. cryptsetup also discards a checksum-valid copy whose JSON fails its
own validation (`LUKS2_hdr_validate`) and falls back to the other copy,
and that validation is wider than td's. So td reads the copy cryptsetup
will use or refuses, with one gap: a copy that passes td's checks and
fails only cryptsetup's is one td reads and cryptsetup does not use. The
secret td releases from it opens the volume only if its keyslot is also in
cryptsetup's copy; otherwise the boot reaches recovery. No header change
rests on that copy alone: a transition runs only once cryptsetup's own
metadata agrees with it ("Transitions").

`token::Token` carries one td token: `encode` writes the compact JSON
`cryptsetup token import` takes, keys in the order above, `decode` and
`from_json` admit it, and `orphan_from_json` admits an orphan.
`luks2::read` takes any `Read + Seek` whose offset zero is the volume's
first byte, such as a window onto the installer's claim or the
selector's opened partition, and returns the used copy's sequence
number, size,
UUID, label, keyslot numbers, td tokens and orphans with their token
numbers, and each token of another type with its number and the
keyslots it names. It runs no cryptsetup and needs no privilege.
Volume discovery checks identity only, so that another disk's header
cannot stop a boot (td-install/DESIGN.md "Read-only volume discovery
primitive"). `luks2::binary_claim` is its pre-check over one 4096-byte
binary header: the label and UUID, each up to its first NUL, of a
header carrying its position's magic and version 2, or nothing; it
verifies no checksum. `luks2::identity` chooses and verifies the copy
exactly as `read` does, refusing what that choice refuses (no valid
copy, copies disagreeing at one sequence number, a checksum algorithm
other than SHA-256), and returns its sequence number, size, UUID and
label without parsing its JSON, so it admits any valid size and any
tokens; `read` refuses the rest on the volume chosen.
`luks2::metadata` reads the same keyslots and tokens, under the same
token rules, from the JSON `luksDump --dump-json-metadata` prints for
the copy cryptsetup uses: at most `MAX_METADATA_JSON`, 128 KiB, which
holds cryptsetup's indented rendering of a 12 KiB area, parsed by
td-json.

## Cryptsetup runner

`cryptsetup::Cryptsetup` runs one cryptsetup command per child, for the
installer's formatting and the selector's transitions alike. Its
program is an absolute path, checked at each run rather than looked up,
and its environment is cleared. Standard input is a std pipe filled
whole before the child starts, so it is held to one page
(`MAX_PIPE_INPUT`, 4096 bytes, the least any pipe holds) and more is
refused rather than risk a write that blocks: a passphrase or secret of
at most 48 bytes, or a token's JSON under `token::MAX_TOKEN_JSON`. A
second secret, the new key `luksAddKey` reads as its key file, is a
`KeyFile`: a std pipe filled the same way whose write end is closed and
whose read end this process keeps, close-on-exec, naming it to the child
as `/proc/<own pid>/fd/N`, so cryptsetup opens the pipe anew and reads
exactly the secret and end of file. That name opens only for a process
the kernel lets trace the caller, which must therefore stay dumpable and
run cryptsetup under its own uid. No argv element or environment
variable carries key material, and a descriptor path names no secret.
The runner keeps no copy of a secret: it writes from the caller's borrow
into the pipe. A child's standard output is read up to `MAX_OUTPUT`, 128
KiB, beyond which the run fails; `run` copies it to standard error,
which the child inherits, and zeroes it, so no verb that prints key
material to standard output may run through `run`. `luksDump
--dump-volume-key` prints the volume key in hex there unless it is given
`--volume-key-file`, cryptsetup 2.8.8 also takes `--dump-master-key` and
`--master-key-file` as aliases, and `--unbound` prints an unbound key, so
the runner starts a `luksDump` only in exactly the two shapes below,
with operands that cannot be read as options, and refuses any other
before a child starts. `metadata` runs `luksDump --dump-json-metadata` and
returns its output instead, the header's public JSON.

`run` succeeds only on exit 0 and otherwise names the program, the verb
and the exit status, never an argument; `exit_code` reads that status
back from the error, so a caller can tell `EXIT_BAD_PASSPHRASE`, 2
(cryptsetup 2.8.8's `translate_errno` of a wrong passphrase's
`-EPERM`), from every other failure. `mapping` asks `status NAME` of
device-mapper and classifies its exit alone: 0 active, 4 inactive
(cryptsetup 2.8.8's `action_status` returns `-ENODEV` for an inactive
name, which `translate_errno` makes 4), and any other exit an error
meaning neither; no node under `/dev/mapper` is consulted.

The module spells every command's arguments: `luksFormat` with
ENCRYPTION.md's parameters, the UUID and the label; `luksAddKey` of a
new keyslot, another keyslot's key on standard input authorizing it,
with the format's PBKDF2 parameters; `token import`, at a given number
or the lowest free one; `token remove`; `luksKillSlot --batch-mode
--key-file=-`, whose standard input carries the key of a keyslot that
remains; `luksDump --dump-json-metadata`; `luksDump --dump-volume-key
--batch-mode --volume-key-file FILE --key-file=-`, for the selector's
handoff, which cryptsetup 2.8.8 writes to a new FILE it creates
`O_CREAT|O_EXCL` at mode 0400 and names on standard output without the
key (ENCRYPTION.md "Selector release" says what was verified); `open`,
`close`, `status`; `open --key-slot N`, the passphrase tried on that
keyslot alone, with which the selector's recovery flow opens keyslot 0;
`open --test-passphrase` on one keyslot; and the
deployment initramfs's `open --volume-key-file KEY-FILE`, the handed-off
volume key a `KeyFile` with nothing on standard input, which cryptsetup
2.8.8 reads at the header's volume-key size and activates only when it
matches the header's digest of the data segment (`_verify_key`), trying
no token or keyslot. Given a key, cryptsetup
2.8.8's `luksKillSlot` destroys a keyslot only once that key opens
another; given an empty standard input, or one it fails to read, it
ignores the failed read (`-EPIPE`) and destroys the keyslot unasked. So
`run` refuses a `luksKillSlot` whose input is shorter than
`MIN_KILL_KEY`, 32 bytes, td's shortest secret, before any child
starts.

## Transitions

The planner is pure: from the header the reader returned and the td
tokens whose secrets released, it computes one transition's ordered
cryptsetup steps, running no cryptsetup and reaching no TPM. The release
orchestration executes the plan through the runner after the cap
("Release orchestration"). `transition::classify` names the transition the
released tokens call for, and `transition::plan` its steps, each a
runner command with the key of a named keyslot, the new token's JSON or
nothing on standard input. A plan adds the new protector at the lowest
free keyslot from 1 and imports its token at the lowest free token
number, which cryptsetup chooses; every kill carries the key of a
keyslot that remains: the released or recovery keyslot's before the
add, the new one's after its test. A plan whose import would find every
token number of any type taken, after the orphans and any early
retirements are removed, refuses before its first step, as one with no
free keyslot does.

`plan` returns a `Plan`, which carries the reader's view of the
keyslots and tokens of every type it was computed from. Before the
plan's first step the executor runs `luksDump --dump-json-metadata`
through the runner and hands its output to `Plan::confirm`, which
returns the steps only when `luks2::metadata` reads from it exactly
that view; the executor runs no other steps. Otherwise no plan runs that
boot and the volume still opens: a copy td read and cryptsetup rejects
("LUKS2 tokens") could otherwise make td kill a keyslot that is an
orphan in td's copy and a foreign token's in cryptsetup's.

- Keyslot 0 is always the recovery keyslot. No plan adds or kills it or
  names it in a token. A td token naming it is never released and
  suppresses every plan while it remains: no transition, reseal or
  orphan removal runs, though other td tokens still release and the
  volume opens. Its repair is removing that token from the live medium
  (`cryptsetup token remove`), which the review discloses.
- An orphan is any keyslot other than 0 that no token of any type
  names, or any td token whose `keyslots` array is empty. Keyslots that
  other token types name, such as systemd-cryptenroll's, are left
  alone, so the reader also reports which keyslots those tokens name. A
  passphrase keyslot added by hand without a token is therefore removed
  at the next boot that runs a plan; the tier's only passphrase is the
  recovery key in keyslot 0, and the review discloses this.
- A superseded token is every device-bound token other than the
  lowest-numbered one that released. When no device-bound token
  released, nothing is superseded.
- No destroy precedes a verified add or a verified released keyslot. A
  plan first runs `open --test-passphrase` on the keyslot the released
  secret's token names, with that secret, unless the recovery key in
  keyslot 0 opened the volume. It then removes every orphan.
- A new protector then commits in this order: add its keyslot, import
  its token, `open --test-passphrase` on that keyslot with its secret,
  `luksKillSlot` each old keyslot, then `token remove` each old token.
  Should the add exceed the four-token bound, the plan first retires,
  keyslot then token, every td token that did not release this boot:
  keyslot 0 or the tested keyslot opens the volume. An
  interruption at any step keeps keyslot 0 and whichever protector
  opened the volume, or the new one once its token is committed; what it
  leaves over is an orphan, a superseded token or a leftover first-boot
  one, which the next boot's plan removes.
- A power cut inside `luksKillSlot` can leave a dead keyslot: its area
  wiped, its JSON and any token naming it intact, so the reader sees it
  as before. Kills retire only keyslots no protector that stays needs,
  and each later kill of it is authorized by another keyslot's key, so a
  dead keyslot is retired again by the next plan. A dead keyslot the
  released token names fails the plan's first test, or the volume's open
  when the plan is empty: the executor then classifies again without
  that token, falling back to the next released one and, when none is
  left, to recovery. It never halts on it.
- A LUKS2 reencryption keyslot names no token and is an orphan by this
  rule; the protected-tier upgrade, which re-encrypts online, runs only
  its own planner while one or its `td-upgrade` token exists ("Protected
  roles (planned)").

What is old depends on the transition. The first-boot transition retires
the first-boot keyslot and token. When a device-bound protector released
there is no new protector, and the plan retires a leftover first-boot
token and every superseded one, with their keyslots. The confirmed
recovery reseal retires every td keyslot and token except the new one, a
surviving first-boot one included. Recovery without that confirmation,
recovery without a TPM device and the `AlreadyClosed` path run no plan:
the header, orphans included, is unchanged. Tests drive each plan to
every interruption point and require the next boot's plan to complete
it.

## Release orchestration

`release::release` runs ENCRYPTION.md's release order, steps 2 to 5,
for the installed selector, td-boot, which then opens the volume with a
`Released` secret, runs its recovery flow on `Recovery` and refuses
boot and halts on `Halt` (ENCRYPTION.md "Selector release"). It takes the result of `luks2::read` on the volume (step 1,
which td-boot runs first, since a header carrying a td token decides
its TPM wait), the name cryptsetup opens the partition by, the TPM as a
`release::Tpm` (`DeviceTpm` opens `/dev/tpmrm0`; `None` when no device
appeared within td-boot's wait, whose constant is td-boot's), the
cryptsetup commands as a `release::Runner` (the runner's
`Cryptsetup`), and a console that receives one line per decision and
per transition commit. Every TPM operation opens its own client;
nothing reaches cryptsetup before the cap.

1. Without a TPM nothing is tried or capped: `Recovery { NoTpm }`.
2. It tries every td token up to `MAX_ATTEMPTS`, the reader's four,
   device-bound tokens first and each role by number, through
   `unseal_token`: the first-boot policy for a first-boot token,
   `release_policy` for a device-bound one. A token naming keyslot 0 is
   never tried; `release::candidates` is that order and filter, which
   the deployment initramfs's post-cap check shares. Every refusal
   releases nothing; the console names its `UnsealError`.
3. When only first-boot tokens released, it reads PCRs 4 and 9 with
   `observe`, seals a fresh `/dev/random` secret to them and a
   literal-zero PCR 12, and unseals it once, requiring the same secret.
4. It caps PCR 12 on a fresh client whatever came before. `AlreadyClosed`
   is `Recovery { AlreadyClosed }`. `NoSha256Bank` is
   `Recovery { NoSha256Bank }`, with no reseal offered, only when nothing
   released and no protector was sealed this boot; otherwise a SHA-256
   PolicyPCR that did shows the bank existed, and the contradiction is
   `Halt` with an `Uncertain` reason, since recovery would leave PCR 12
   open on a TPM that may have it. `Uncertain` and `Mismatch` are
   `Halt`, and a client that cannot be opened is `Uncertain`. Neither
   carries a secret: every released and new secret is dropped, which
   zeroes it, before the outcome returns.
5. After the cap a header td refused is `Recovery { Header }`, a step 3
   that refused an unmeasured PCR or failed is `Recovery { Unmeasured }`
   or `Recovery { Transition }`, and nothing released is `Recovery {
   NothingReleased }`. Otherwise it classifies the released tokens and
   plans `Keep`, or `FirstBoot` with the new protector, confirms the
   plan against `luksDump --dump-json-metadata` and runs its steps: a
   released or new keyslot's key on standard input, the new token's
   JSON, or nothing, and the new secret as `luksAddKey`'s key file. A
   plan the planner refuses, metadata that cannot be read or disagrees,
   or an empty plan runs no step but a test of the released keyslot, so
   the keyslot `Released` names was tested this boot.

A failed test of the released keyslot, the plan's first step or that
stand-in, falls back: classification runs again without that token, and
when none is left the outcome is `Recovery { NoKeyslotOpens }`, never a
halt. A fall-back that leaves only first-boot tokens is `Recovery {
FirstBootFallback }`: a device-bound token released, so step 3 sealed
nothing, and opening with the first-boot protector alone would leave
release bound to PCR 12 alone on every later boot. The confirmed reseal
then retires both. Any other failed step stops the plan; the volume opens with the
released secret, or with the new protector's once its keyslot's test
passed, and the next boot's plan removes what is left over. `Released {
keyslot, secret }` is the only outcome holding a secret; the selector
opens with it and drops it. `Recovery`'s `reseal_offerable` is
ENCRYPTION.md's condition for offering the confirmed reseal: this
boot's own cap closed PCR 12, td read the header, and the reseal can
run. It is false without a TPM, for `AlreadyClosed` and for a refused
header; for `Unmeasured`, whose PCR refuses the reseal's seal too; and
wherever the planner refuses a `Reseal` plan over the header (a td
token naming keyslot 0, a shared keyslot, no free keyslot or token
number), which a placeholder sealed object tries before the outcome
returns.

`release::reseal` is the confirmed recovery reseal, which td-boot's
recovery flow runs after the cap, once the recovery key opened keyslot
0 and its owner confirmed. It takes the header `release` was given, the
partition's name, the parsed `RecoveryKey`, the TPM and the runner. It
seals a fresh secret to `observe`'s PCR 4 and PCR 9 and a literal-zero
PCR 12, the seal step 3 also makes, but sends no verifying unseal, which
the closed cap would refuse. It then plans `Transition::Reseal`,
confirms the plan against `luksDump --dump-json-metadata` and runs its
steps through the same executor, reporting each commit: the recovery
passphrase is keyslot 0's key, authorizing the add and any orphan's
kill, and the new secret is the added keyslot's. `Resealed::Complete`
names the new keyslot when every step ran; `Stopped` says whether the
new keyslot's test passed before a failed step stopped the plan, which
the next boot's plan completes or that boot's recovery repeats; and
`NotRun` (an unmeasured PCR, a failed seal or read, a plan refused,
metadata that fails or disagrees) leaves the header unchanged. Keyslot
0 is never touched, the new secret is zeroed before it returns, and the
caller opens the volume with the recovery key whatever it says. The
plan's test of the new keyslot is the only verification this boot can
make; its TPM release is first proven on the next boot.

Two choices go beyond ENCRYPTION.md's text. A step 3 seal or
verification that fails for a reason other than an unmeasured PCR also
ends in recovery rather than opening with the first-boot secret, so the
device never keeps releasing to the first-boot protector alone without
its owner seeing it. And a header td refuses is still capped, with
nothing tried: the cap closes release on an encrypted volume whatever
its header holds.

## PIN and tpm-pin policy

ENCRYPTION.md increment 8a's first td-protector commit: library code
that nothing in production calls until 8c, in the `pin` module, over
td-tpm's existing commands.

**PIN.** `pin::Pin` holds 6 to 63 bytes, each 0x20 to 0x7e, as typed;
`parse` refuses any other entry, naming the byte offset or the length
and never a byte, and is the only constructor. It judges the length
first, then each byte, and names a refused byte by its one-based
offset, as the recovery-key entry does; it copies the entry into one
allocation of exactly its length, and the caller zeroes its own. It is
zeroed on drop and neither `Debug`, `Display` nor `Clone`.
`auth_value(salt)` is HMAC-SHA256 keyed with the 32-byte salt over
`td/disk-protector/pin/v1`, a zero byte and the PIN, returned whole in
`AuthValue`, a zeroing owner that is neither `Debug`, `Display` nor
`Clone`. Removing an authValue's trailing zero bytes where TPM 2.0 does
is td-tpm's (td-tpm/DESIGN.md, "HMAC policy sessions"), not the
derivation's. The HMAC is td-fido's `hmac_sha256`, its `hmac.rs`
compiled by path beside a mount of the engine's SHA-256, as td-secret's
store crypto compiles it (td-fido/DESIGN.md, "Shared HMAC-SHA256"),
rather than a dependency on the td-fido crate, so the td-boot and
td-install recipes, which stage td-protector file by file, stage that
one file and build no td-fido rlib. It streams the parts, so the PIN is
copied into no hash input buffer.

**tpm-pin policy.** `PinPolicy` is PolicyPCR over the SHA-256 bank
selecting PCRs 4, 9 and 12 (and 7 when the seal names it), with the
composite of the expected PCR 4, PCR 9 (and PCR 7) and a literal-zero
PCR 12, then PolicyAuthValue (`POLICY_AUTH_VALUE`, 0x16b), then
PolicyCommandCode(Unseal), each extended from the zero digest as TPM
2.0 Part 3 specifies; `PinPcrs` names the token's two selections,
`4,9` and `4,7,9`. td-protector computes the digest itself: td-tpm's
`policy_digest` has no PolicyAuthValue step. When td-tpm's session
commit, which runs PolicyAuthValue in a session, lands, td-protector's
`POLICY_AUTH_VALUE` and digest builder give way to td-tpm's, and these
literal tests stay as the cross-check. The sealed object has
`PROTECTED_ATTRIBUTES` (0x92): fixedTPM, fixedParent and
adminWithPolicy, with userWithAuth and noDA clear, and the authValue
`auth_value` gives. Whether a new seal names PCR 7 is td-boot's reading
of the measured boot (ENCRYPTION.md, "Firmware authentication"), never a
header field. `PinPolicy::new` and `release_pin_policy` admit an
unmeasured, all-zero PCR 4, 7 or 9, as release must; 8c's seal therefore
owes a tpm-pin counterpart of `observe` that refuses an unmeasured PCR
before it seals, as the device-bound seal does.

**Chain check.** `chain_check` takes its own client, the token's
`PinPcrs`, the storage primary's 34-byte Name the token recorded and
the sealed pair. It refuses a public area other than a SHA-256
keyed-hash object with `PROTECTED_ATTRIBUTES`, a 32-byte authPolicy, no
scheme and a 32-byte unique digest before any command
(`NotProtected`); reads the token's PCRs in one PCR_Read, measured or
not, PCR 12 entering as the literal zero and never read (`NoSha256Bank`
typed as td-tpm types it); creates the storage primary
(`PrimaryRefused` when the TPM refuses CreatePrimary, such as an owner
authorization another system set, 0x9a2); refuses another Name, after
flushing the primary, without loading (`OtherPrimary`); loads the object
under that primary with td-tpm's `load`, sending no authorization
(`LoadRefused` when the TPM refuses the Load, such as TPM_RC_INTEGRITY
for a private area that does not verify); flushes the object and the
primary; and only then compares the digest over the PCRs as they read
now with the sealed authPolicy (`ChangedChain`). Each refusal keeps its
command and response code, read from td-tpm's `last_refusal` straight
after the refused command; a transport error, a malformed reply and a
failed flush, also one after a Load that succeeded, are `Other` and
never read as a refusal. The Load runs whatever the digest, so that
ENCRYPTION.md's warning can tell a changed chain the TPM still loads
from a cleared, replaced or intercepted TPM; which of `PrimaryRefused`,
`LoadRefused` and `Other` leads to which line is 8c's. `Ok` answers
that the PIN may be asked.

## Protected roles (planned)

ENCRYPTION.md increment 8 adds the
protected tier's roles here ("Protected tier" there owns the flows and
the shapes; this section owns the formats, policies and plan rules),
each landing in the sub-increment ENCRYPTION.md names.

The PIN codec, the authValue and the tpm-pin policy with its chain
check are current ("PIN and tpm-pin policy"); the rest of this section
is not.

**Lockout.** `lockout::seal(volume_key, uuid, l, name)` and `open`
implement ENCRYPTION.md's lockout token: HKDF-SHA256 over the volume key
with the info `td/disk-protector/lockout/v1`, a zero byte, the UUID and
the nonce, 64 bytes split into `k_enc` and `k_mac`; the ciphertext is
`L` XOR `k_enc`; the tag is HMAC-SHA256 under `k_mac` over the nonce,
ciphertext and Name, compared in constant time. `take`, `prove`,
`reset` and `clear` are the sequences that section gives, the
parameters `DA_MAX_TRIES` 32, `DA_RECOVERY_SECS` 600 and
`DA_LOCKOUT_RECOVERY_SECS` 86400 pinned by test; none matches
parameters in place of a proof.

**Tokens.** Each protected token is a LUKS2 token object with exactly
these keys, in this order, every text a canonical decimal or lowercase
hexadecimal string as the device-bound format's are:

```text
{"type":"td-protector","keyslots":["N"],"role":"tpm-pin",
 "account":"1000","pcrs":"4,9","primary":"HEX","salt":"HEX",
 "public":"HEX","private":"HEX"}
{"type":"td-protector","keyslots":["N"],"role":"fido2-primary",
 "account":"1000","credential":"HEX","salt":"HEX","x":"HEX","y":"HEX"}
{"type":"td-protector","keyslots":["N"],"role":"recovery-key"}
{"type":"td-protector","keyslots":[],"role":"lockout",
 "primary":"HEX","nonce":"HEX","ciphertext":"HEX","tag":"HEX"}
{"type":"td-protector","keyslots":[],"role":"secure-boot",
 "salt":"HEX","public":"HEX","private":"HEX"}
{"type":"td-protector","keyslots":[],"role":"config","tags":["HEX"]}
{"type":"td-protector","keyslots":[],"role":"update-retry",
 "pair":"HEX","tag":"HEX"}
{"type":"td-upgrade","keyslots":[],"shape":"tpm","from":"HEX"}
{"type":"td-request","keyslots":[],"operation":"pin-change"}
```

`pcrs` is `4,9` or `4,7,9`; `primary` is the storage primary's 34-byte
Name; `role` of a FIDO2 token is `fido2-primary` or `fido2-recovery`,
and a pending one has empty `keyslots`; `shape` is `tpm` or `fido2`;
`from` is 32 bytes; `tags` holds one or two distinct 32-byte HMAC-SHA256
tags; `pair` is the SHA-256 over the staged pair's two digests and `tag`
its 32-byte HMAC-SHA256 under the update-retry key; `operation` is
`pin-change` or `tpm-clear`; `account` is `1000`. `salt`, `x`, `y`,
`nonce`, `ciphertext` and `tag` are 32 bytes, `public` and `private`
keep the device-bound bounds (the secure-boot token's sized for an
RSA-2048 key), and `credential` is 1 to 255 bytes. The `td-upgrade` and
`td-request` types are their own, so that a selector from before
increment 8 ignores them as foreign tokens; their empty `keyslots`, the
lockout's, the secure-boot, config and update-retry tokens' make none of
them an orphan. `config::tag(volume_key, uuid, bytes)` is
ENCRYPTION.md's configuration tag ("Firmware authentication"), checked
in constant time; the secure-boot and config tokens land with 8i.

**Headers.** The reader classifies a header as device-bound (first-boot
or device-bound td tokens), protected (tpm-pin, fido2 or recovery-key
tokens) or upgrading (a `td-upgrade` token beside either kind); one
holding both kinds without a `td-upgrade` token refuses, as does a
second `td-upgrade` or `td-request` token. A device-bound header keeps
the four-token bound for its own td tokens. A protected or upgrading
header admits at most fourteen tokens of td's types, orphans included:
at most two tpm-pin, four fido2, one recovery-key marker, one lockout,
one secure-boot, one config, one update-retry, one `td-upgrade`, one
`td-request` and one first-boot or device-bound during U0. `decode`
keeps its 4096-byte bound per token, which the largest fido2 and
secure-boot tokens fit.

**Plans on protected headers.** The recovery keyslot is the one the
`recovery-key` marker names; an orphan is any keyslot no token of any
type names and any td token naming a keyslot that does not exist. Two
tpm-pin tokens may name one keyslot, an update's pre-seal; after a
release, the plan retires every tpm-pin token but the one that released
or was resealed this boot, and every tpm-pin keyslot no remaining token
names. A boot's plan never retires a fido2 token, its keyslot or the
recovery keyslot: only "Protector management" removes one, under the
shape minimums. The reseal's plan adds its keyslot under the released
secret, imports its token, tests it, then retires every other tpm-pin
keyslot and token. While a `td-upgrade` token remains, only the upgrade
planner runs, over ENCRYPTION.md's states U0 to U5, read from the header
alone: the data segment's digest against `from`, cryptsetup's
online-reencryption requirement, and which tokens exist. A pending
fido2 token without a `td-upgrade` token is an orphan. The planner
computes each step's resulting JSON size and refuses a step whose header
would not fit the 12 KiB area, before writing.

**Runner.** The runner gains `reencrypt` in three spelled shapes, the
upgrade's initialization with `--resilience checksum` or `--resilience
journal` (ENCRYPTION.md's argument list) and `--resume-only`, each with
`--active-name td-selector` and the key on standard input, and `token
import` with and without `--token-replace` for a token id. Its
`luksKillSlot` floor stays 32 bytes, which a FIDO2 passphrase and the
recovery key's 48 digits meet.

**Release.** `release::release` gains the protected path: its
`Released` names the method (tpm-pin, fido2-primary, fido2-recovery or
recovery key) that the admission record follows; FIDO2 comes through a
`release::Fido` interface that td-boot's worker implements and tests
script; the chain check, warning and verified reseal run before the cap
and the lockout's take, proof and reset after the keyslot test, as
ENCRYPTION.md's "Protected release" says. Its bounds grow by the FIDO2
attempts, which are the person's, one seal and one verifying unseal per
reseal, and the lockout's four commands.

## Bounds

Every TPM exchange is td-tpm's: bounded commands and replies, no retry or
fallback, and errors rather than panics. This crate adds fixed selections
and a 32-byte payload length. The header reader reads and hashes a 4 KiB
binary header per candidate and, where that is valid, its whole area: at
most 4 MiB for the primary, then at most 4 MiB for the secondary or,
when the primary is invalid, the nine scan offsets' areas, under 8 MiB
together: under 12 MiB in all, hashed twice at most (the checksum, then
the JSON area's own digest). It holds at most one area, up to 4 MiB, at
a time, and keeps of
each verified copy only its binary header, its JSON area's SHA-256 and,
at 16 KiB, its 12 KiB JSON area. td-json parses at most 12 KiB of a
header, 128 KiB of a metadata dump, and bounds
nesting. `decode` admits a token text of at most 4096 bytes; a token from
either path has only bounded fields and encodes in under 1700 bytes. Its
sealed areas are at most 256 and 512 bytes, and a header carries at most
four td tokens, orphans included, among 32. A plan has at most 39
steps: two tests, a kill per keyslot from 1 to 31, a removal per td
token, an add and an import. The runner's inputs are at most one page.
A PIN is at most 63 bytes, and a chain check sends one PCR_Read, one
CreatePrimary, one Load and two flushes. A release tries at most four
tokens, seals at most one protector, caps once, and runs at most one
plan per released token, each of whose fall-backs follows a failed
first step. A reseal seals one protector and runs at most one plan.

## Evidence

Unit tests over a scripted TPM that evaluates PolicyPCR and the Unseal
authPolicy pin the cap event and first-boot policy digest literals; seal
then unseal under both policies; that the first-boot policy reads nothing
and the observed policy reads exactly PCRs 4 and 9; refusal of an
unmeasured PCR 4 or 9; that after the cap both policies are refused at
PolicyPCR with no Unseal sent; each `CapError`, with no extension after a
non-zero prior or a TPM answering without a SHA-256 bank, one retry of a
lost prior read only, and no retry after a refused extension or a lost
extension or readback reply; the installer check's command stream and its
refusal of a private area the TPM will not load; refusal of unsealed
payloads other than 32 bytes, including 31 and 33; and that secrets are
one exact read from `/dev/random`, through an injectable source path.
Recovery-key tests pin the Damm table and its published example (572
checks to 4) and encodings computed by an independent implementation,
check for every value from 0 to 65535 that each single-digit substitution
and adjacent transposition in its group fails the check, and cover round
trips through the passphrase and display form, the boundary values 0 and
65535, a value above 65535 refused as such, separators between and inside
groups, every refusal and its group, and the injected random source. Token
tests pin the exact encoding and round trip it at both bounds, and refuse
unknown, missing and repeated keys, other types, keyslot texts that are
not canonical numbers up to 31, unknown roles, uppercase, odd and
oversized hexadecimal, and oversized text; an orphan is admitted only
with an empty `keyslots` array and an otherwise valid format. Header
tests build both copies in the test, checksummed with td-tpm's SHA-256,
and cover agreeing copies, either sequence number winning, copies that
disagree at one sequence number in their JSON or label, a primary invalid
by magic, version, either size bound, own offset, JSON bytes or stored
checksum falling back to the secondary, the scan finding a secondary at
64 KiB and refusing its size, a secondary not at its own size, a
truncated medium, another checksum algorithm, sizes td does not format
(20 and 32 KiB), a checksum-valid 4 MiB primary filled by one JSON string
refused by its size without parsing, malformed and padded JSON areas, a
duplicate key, non-canonical slot numbers, tokens naming absent
keyslots, a malformed td token, a td token naming two keyslots, other
token types ignored, orphans reported beside a valid token and counted
toward the four-token bound, a malformed orphan, the keyslots other
token types name, and a read error; `binary_claim` reports either
position's label and UUID, also from a copy whose checksum fails, and
nothing for the other position's magic, versions other than 2, zeros or
a truncated header; `identity` chooses the copy as `read` does, admits
JSON, tokens and a size `read` refuses, and refuses a header with no
valid copy or with disagreeing copies. Runner tests pin every command's
arguments word for word, that a key file is a close-on-exec descriptor
holding exactly the secret, the one-page input bound, a stand-in
cryptsetup's cleared environment and inputs with no key in its argv, a
relative program refused, every other luksDump shape (a key dump without
a file, the master-key aliases, `--unbound`) refused
before any child starts, `status` exits 0, 4 and others, and a failed
run's exit code read back, none for a child that never ran. Planner
tests model cryptsetup's header commits, a kill stripping its keyslot
from the tokens naming it, and pin the plans of the first-boot
transition, each of its interrupted states, a device-bound release with
superseded, leftover and orphaned tokens and a foreign token's keyslot,
the reseal, the four-token bound and a header whose 31 other token
numbers are foreign; drive the first-boot transition and the reseal to
every cut, between steps or inside a kill, of one boot and of the boot
that resumes it and require the same final header; fall back from a
dead released keyslot; refuse each `Refusal`, metadata that disagrees
in a keyslot or a token of any type among them; and, over 400 generated
headers of keyslots 0 to 5, some dead, with up to four tokens of every
kind, on three machines, cut the first boot everywhere and require of
every later boot no refusal but the header's own, no command
cryptsetup would refuse (each test with the key that opens its keyslot),
the four-token bound after every step, and an empty plan within three
boots. The runner tests also refuse a `luksKillSlot` with an empty or
31-byte key before any child starts. Unseal-outcome tests pin the
refusal codes against Part 2's encodings, type each on its own command
and positions only and a transport error as `Other`, and, through the
scripted TPM, a changed chain refused at Unseal, a cleared TPM refused
at Load, each load refusal answered with nothing unsealed, and an
unmeasured PCR answered by the TPM. Release tests run the orchestration over the scripted TPM and
a scripted cryptsetup that keeps keyslot keys and tokens, reads the new
key from its descriptor, enforces the kill floor and authorization, and
requires PCR 12 capped before every command, with no TPM command after
the first; every test also requires that no secret but the returned one
outlives the release. They cover a device-bound release retiring a
leftover first-boot protector and an orphan keyslot, the first-boot
transition (seal, verification, then the cap last, each commit
reported) and the next boot keeping its protector, nothing released,
`AlreadyClosed` and a TPM without a SHA-256 bank with no plan, that
TPM halting when a protector released before its cap, `Uncertain` and
`Mismatch` halting with
or without a new protector, an unmeasured PCR 4 or 9, a cleared TPM,
metadata that disagrees or fails, a dead released keyslot falling back
to the next token and to recovery, a dead device-bound keyslot beside a
live first-boot one reaching recovery on every boot until a confirmed
reseal (`release::reseal`) converges, the reseal after a changed
selector image (no unseal and no extension, an orphan killed with
keyslot 0's key, every other td keyslot and token retired, keyslot 0
kept, the recovery key on no step keyslot 0 does not authorize, each
commit reported, and the next boot releasing the new protector), a
reseal failing at each of its steps (the metadata dump, the add, the
import, the test, a kill and a removal) followed by the boot that
releases the new protector or reaches recovery and reseals again, an
unmeasured PCR 4 or 9 running no reseal and offering none, recovery
offering no reseal over a header with a td token naming keyslot 0 or
no free keyslot, a td token naming keyslot 0 never
tried and suppressing the plan, a failed import or kill and the boot
that completes it, no TPM device, a refused header still capped, and
the order tokens are tried in. No test reads a header cryptsetup wrote: that needs the source-built
cryptsetup and its kernel crypto interfaces, which the host
gate does not provide, so increment 5's encrypted-installation oracle
(`qemu-install-encrypted`) reads one in the guest: the installer's
verifying-boot check runs this reader and `verify_first_boot_object` on
the header cryptsetup wrote, under the pinned swtpm, and the oracle
requires the recovery-key phase that check gates. Increment 6's oracle
(`qemu-boot-encrypted`) boots the shipped selector over real headers.
The host parses each header the selector's transitions leave: after a
whole first boot, after a power cut on each transition commit and the
boot that completes it, and after constructed orphan and superseded
states that the next plan retires. Ignored oracles run the protector lifecycle and
the release order against the pinned swtpm under td-secret's convention
(`td-secret/DESIGN.md`, "TPM validation"): a first boot's transition
over the scripted cryptsetup, the TPM's PolicyPCR refusal after the
cap, the next boot's release after a restart, a changed selector image
refused at Unseal, and a fresh TPM state's Load refusal (0x1df):

```
TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --frozen --manifest-path td-protector/Cargo.toml emulator_ -- --ignored
```

PIN tests pin the codec's bounds (0, 5, 6, 63 and 64 bytes, the length
judged before the bytes, every byte value at one offset, 0x1f and 0x7f
refused, a space admitted at either end and inside, a multi-byte
character refused at its first byte) and its refusals' text, which
names no byte; the authValue for three PINs (6 bytes, one with a space,
and the 63 printable bytes from 0x20) and a second salt; and the
tpm-pin policy's selection, composite and digest for PCRs 4, 9 and 12
and for PCRs 4, 7, 9 and 12 at fixed values, as literals that
`tests/pin_vectors.py`, a stdlib-only host tool and no build input,
computes independently (Python's `hmac`, and the policy formulas from
TPM 2.0 Part 3). Through the scripted TPM the chain check passes for
both selections, also after the cap, with its exact command stream; is
`ChangedChain` for each changed PCR the token names, for a 4,7,9 check
of a 4,9 object, and not for a changed PCR 7 a 4,9 token does not name;
`OtherPrimary` with no Load; `LoadRefused` with Load's 0x1df for a
cleared TPM and a private area that does not verify; `PrimaryRefused`
for CreatePrimary refused with 0x9a2 and 0x185, with no Load;
`Other` for a flush refused after a successful Load and for a lost
reply to CreatePrimary or the Load; `NoSha256Bank` after the read
alone; and `NotProtected` with no command for noDA or userWithAuth set,
the device-bound attributes, adminWithPolicy clear, another type, name
algorithm or scheme, a short authPolicy, a trailing byte and a truncated
area. The ignored emulator test has the pinned swtpm compute both
policy digests in trial sessions, equal to the literals; with PCRs 4, 7
and 9 extended and PCR 12 still at reset, a real policy session's
PolicyPCR with an empty pcrDigest, where the TPM takes the composite of
its own PCRs, then PolicyAuthValue and PolicyCommandCode, gives
`release_pin_policy`'s digest for both selections, which ties the
literal-zero PCR 12 to the TPM's own reading. It then runs the chain
check over an object the test creates with `PROTECTED_ATTRIBUTES` and
an authValue: it passes, a 4,7,9 check and a changed PCR 4 are
`ChangedChain`, a corrupted private area is a Load `LoadRefused`, and a
fresh TPM state is `OtherPrimary`.

A normal cargo pass with it ignored is not TPM integration evidence.
