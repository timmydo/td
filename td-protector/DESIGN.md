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
any field, or a value outside these bounds refuses the token. A
`td-protector` token with a protected role and the `td-upgrade` and
`td-request` types are the protected tier's ("Protected roles"); other
token types are ignored.

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

### Keyslot test area

`luks2::test_area` is `read` and what a keyslot test against a detached
copy of the header needs: the area's length, two copies of `hdr_size`
and `config.keyslots_size`, the latter at most `MAX_KEYSLOTS_SIZE`
(128 MiB, cryptsetup's own largest), and each keyslot's cost. A cost is
the keyslot's PBKDF2-SHA256 iterations, at most
`MAX_KEYSLOT_ITERATIONS` (100000; td forces 1000 on every keyslot it
adds), and those of the one digest naming it, at most
`MAX_DIGEST_ITERATIONS` (4000000; cryptsetup benchmarks a digest's
count for about an eighth of a second at formatting). Any other KDF,
Argon2 among them, whose memory and time td does not run, or a count
over its ceiling, or no single digest, has no cost, and the test is
refused before cryptsetup starts.

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
`open --test-passphrase` on one keyslot, and the same against a
detached header, `--header FILE` before the keyslot, which the storage
worker's disk proof runs on a private copy of the header area; and the
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

`run_bounded` is `run` for a child that holds a secret past its caller's
control: it keeps no output, the child's standard output being null,
and polls the child's exit every 10 ms until it exits, a deadline passes
or a caller's `abandoned` says the caller is gone. Past either of those
it kills the child and reaps it before returning `Expired` or
`Abandoned`, so no child outlives the run with its input, however long
a lock, an I/O or a key derivation would keep it. Its refusals before a
child starts are `run`'s. td-install's storage worker runs its keyslot
test through it (td-install/DESIGN.md, "Storage operation worker").

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
compiled by path at the crate root beside a mount of the engine's
SHA-256, as td-secret's store crypto compiles it (td-fido/DESIGN.md, "Shared HMAC-SHA256"),
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
unmeasured, all-zero PCR 4, 7 or 9, as release must; 8c's seal takes
`observe_pin`, the tpm-pin counterpart of `observe`, which refuses an
unmeasured PCR before it seals, as the device-bound seal does.

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
check are current ("PIN and tpm-pin policy"), as are, from 8a's last
commit, the lockout token's encryption, the codecs of the tokens 8a
names and the reader's protected bounds below, and, from 8c's first
commit, the plans on protected headers and the runner's `token import
--token-replace`, and from its second the protected release with the
lockout's take, proof, counter and reset, which td-boot's installed
selector runs on a protected header from its third, and the deployment
initramfs's PolicyPCR-only check of tpm-pin tokens, and, from 8e, the
update-retry token's codec, reader bound and tag (`update_retry`) and a
selector update's pre-seal and token steps (below), and, from 8f's first
commit, the reader's upgrade view, the upgrade's plans and the runner's
`reencrypt` shapes (below), which nothing runs until the selector's
upgrade boot. Before item 9 only a header root wrote through the worker,
or one forged on the disk, reaches them. The lockout's clear and the
secure-boot and config tokens are not current.

**Lockout.** `lockout::seal(volume_key, uuid, l, name)` and `open`
implement ENCRYPTION.md's lockout token: HKDF-SHA256 over the volume key
with an empty salt and the info `td/disk-protector/lockout/v1`, a zero
byte, the UUID and the nonce, 64 bytes split into `k_enc` and `k_mac`;
the ciphertext is `L` XOR `k_enc`; the tag is HMAC-SHA256 under `k_mac`
over the nonce, ciphertext and Name. The HKDF is td-fido's
`hkdf_expand` from the same `hmac.rs` the PIN's HMAC compiles. `seal`
draws the nonce from `/dev/random`, as a secret is drawn, since two
writes under one volume key sharing a nonce would share a key stream;
the UUID is the header's text as 16 bytes (`lockout::volume_uuid`,
8-4-4-4-12 hexadecimal digits of either case), and an empty volume key
is refused. `open` takes this TPM's storage-primary Name and verifies
the tag in constant time before anything else (`OpenError::Tag`), then
requires the token's Name to be that one (`OtherPrimary`); only then
does it decrypt, into the buffer td-tpm's `LockoutAuth::from_bytes`
zeroes, whose refusal of an authorization that trims to nothing is
`Invalid`. `k_enc` and `k_mac` are zeroed on drop. The token carries no
secret in clear, so its codec keeps nothing to zero. The take, the
proof and the reset are the protected release's (below), over td-tpm's
lockout commands (td-tpm/DESIGN.md "Lockout hierarchy") with the
parameters `DA_MAX_TRIES` 32, `DA_RECOVERY_SECS` 600 and
`DA_LOCKOUT_RECOVERY_SECS` 86400 (`lockout::DA_PARAMETERS`, pinned by
test); none matches parameters in place of a proof. The clear lands with
the `tpm-clear` request.

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
 "primary":"HEX","nonce":"HEX","ciphertext":"HEX","tag":"HEX",
 "proven":"1"}
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
`pin-change` or `tpm-clear`; `account` is `1000`; `proven` is `0`
until a take's LockReset or a proof under the lockout token succeeds
and `1` after (ENCRYPTION.md "Lockout"), outside the tag, since
whoever can write the header can remove the token anyway; marking a
token over a wrong `L` proven costs only one refused proof per boot,
bounded by the lockout hierarchy's own recovery. `seal` writes `0`,
and nothing else is admitted. `salt`, `x`, `y`,
`nonce`, `ciphertext` and `tag` are 32 bytes, and `credential` is 1 to
255 bytes. A tpm-pin token's `public` and `private` are 1 to 128 and 1
to 256 bytes, tighter than the device-bound token's 256 and 512 so that
the largest protected header leaves 1 KiB of the JSON area spare
("Headers"). td creates the object: its public area is exactly 78
bytes, which `protected_auth_policy` enforces, and its private area
follows the TCG reference layout every commercial TPM derives from, an
integrity HMAC (2 + 32), an IV (2 + 16) and the encrypted sensitive area
(106), 158 bytes, which swtpm 0.10.1 also writes (the emulator test pins
both and encodes the token). A TPM whose object exceeds either bound is
refused at the seal, before the cap and before any keyslot is added or
header written, and is a TPM unusable for td (ENCRYPTION.md
"Re-encrypting upgrade"): 8c's seal pins that ordering. The secure-boot
token's are sized for an RSA-2048 key, at most 320 and 256 bytes (its
TPM2B_PUBLIC holds 316 bytes; its private area 236 before the IV), the
sizes the header measurement takes and 8i's codec pins against the
emulator. `keyslots` names at most one keyslot; the lockout,
`td-upgrade` and `td-request` tokens name none, a tpm-pin token or the
marker naming none is an orphan, and a fido2 one is pending or an
orphan. `protected::Token` carries the tokens 8a
names: `encode` writes the compact JSON in the order above and refuses
a value outside these bounds, and `decode` (4096 bytes at most) and
`from_json` admit exactly the format, each key once in any order,
refusing an unknown, missing or repeated key, another type, role,
account, `pcrs`, shape or operation, a keyslot text that is not a
canonical number up to 31, more than one keyslot or one where none
belongs, and uppercase, odd or wrongly sized hexadecimal. No field
carries a version: the role, the type and the `/v1` derivation domains
carry it. A role td does not know yet, such as 8i's, falls to the
device-bound codec, which refuses it. The `td-upgrade` and
`td-request` types are their own, so that a selector from before
increment 8 ignores them as foreign tokens; their empty `keyslots`, the
lockout's, the secure-boot, config and update-retry tokens' make none of
them an orphan. `config::tag(volume_key, uuid, bytes)` is
ENCRYPTION.md's configuration tag ("Firmware authentication"), checked
in constant time; the secure-boot and config tokens land with 8i.
`update_retry::token(volume_key, uuid, kernel, initrd)` and `verifies`
are ENCRYPTION.md's retry tag ("Selector update"): a 32-byte key
HKDF-SHA256 derives from the volume key with an empty salt and the info
`td/disk-protector/update-retry/v1`, a zero byte and the UUID; `pair`
is the SHA-256 of the staged kernel's SHA-256 followed by the staged
initramfs's, and `tag` HMAC-SHA256 under the key over `pair`. `verifies`
requires both the pair and the tag, each compared in constant time; an
empty volume key verifies nothing, and the key is zeroed on drop.
`tests/token_vectors.py` computes both independently.

**Headers.** The reader returns the protected tier's tokens with their
numbers in `Header::protected` (and `Metadata::protected`, so a plan's
confirmation compares them too), and `Header::kind` classifies a header
as device-bound (first-boot or device-bound td tokens, or none),
protected (a tpm-pin token, a fido2 token naming a keyslot, the
recovery-key marker, the lockout, an update-retry or a `td-request`
token) or upgrading
(a `td-upgrade` token beside either kind). A keyslot-less fido2 token
alone leaves a header device-bound, since a pending fido2 token without
a `td-upgrade` token is an orphan (phase 1 interrupted before its last
write). One holding device-bound td tokens, orphans included, beside
protected ones without a `td-upgrade` token refuses. A device-bound
header keeps the four-token bound for its own td tokens. A protected or
upgrading header admits at most fourteen tokens of td's types, orphans
included: at most two tpm-pin, four fido2, one recovery-key marker, one
lockout, one secure-boot, one config, one update-retry, one
`td-upgrade`, one `td-request` and one first-boot or device-bound during
U0; the reader enforces each class's bound, the device-bound one's
beside a `td-upgrade` token included, which bounds the sum. `decode`
keeps its 4096-byte bound per token, which the largest tokens fit (a
tpm-pin token at its bounds is 1038 bytes, a fido2 one 824).

The device-bound planner plans only device-bound headers
(`Refusal::NotDeviceBound`), so before 8c and 8f a selector boots a
protected or upgrading header with no plan: a device-bound token
releases and the volume opens after its keyslot's test, every token and
keyslot left as it is. In a device-bound header it treats the protected
tier's tokens as it treats foreign ones, their keyslots and token
numbers taken and never removed; the pending tokens an interrupted
phase 1 leaves are removed by 8f's planner (`upgrade::pending_orphans`).

Measured with the pinned cryptsetup 2.8.8 on header files the test
builds (`tests/header_fit.rs`, ENCRYPTION.md "Re-encrypting upgrade"),
the largest protected header, keyslot 0 and six more, four fido2
tokens with 255-byte credentials, two tpm-pin tokens, the marker, the
lockout, `td-upgrade` and `td-request` tokens at their bounds, and the
secure-boot, config (two tags) and update-retry tokens at theirs, holds
10360 bytes of JSON of the 12288; the largest U2, every token naming no
keyslot and `reencrypt --init-only` adding its keyslots, segments and
digest, 9696 bytes with journal resilience and 9732 with checksum. At
the device-bound sealed-area bounds each tpm-pin token would be 768
bytes longer and the protected header would hold 11896, under 1 KiB
spare. The U2 headers are measured at `reencrypt --init-only`. A
reencryption interrupted part-way splits its first segment in two and a
multi-terabyte volume writes longer offsets and sizes: probed by hand
on a 20 GiB file with no tokens, an offline reencryption interrupted
after its first GiB grew the JSON from 1976 bytes to 2114, and 14-digit
byte counts add a few bytes per field, which the 2.5 KiB margin absorbs.
The pinned sizes hold for the test's fixture, a 20 MiB file, and 8f's
planner bounds each plan's size from the header's own before writing
("Upgrade plans"). The protected
header, 8e's update-retry token included and before 8i's tokens are
added, and a U2 header holding
only what U2 holds are read back through `luks2::read` and agree with
cryptsetup's own metadata dump.

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
fido2 token without a `td-upgrade` token is an orphan. The upgrade
planner bounds each plan's resulting JSON size and refuses a plan whose
header could outgrow the 12 KiB area, before writing.

Since 8c, `protected_plan` is that planner for protected headers, pure
as `transition` is. `check` admits a header only when it is protected
(`luks2::Kind`; device-bound headers are `transition`'s and upgrading
ones 8f's), of one shape, the FIDO2-primary shape being a header with a
fido2-primary token and the TPM-primary shape any other, with no
fido2-primary token beside a tpm-pin or lockout token (`MixedShape`), a
TPM-primary header holding a marker that names a keyslot
(`NoRecoveryMarker`), and no keyslot named by two tokens of any type
but two tpm-pin tokens (`SharedKeyslot`). `plan` takes the protector
whose keyslot's test passed this boot (`Opened`: a tpm-pin or fido2
token, or the recovery key on the marker's keyslot), which the release
ran and the plan does not repeat, and the confirmed reseal's new
tpm-pin token, if any. Four choices go beyond ENCRYPTION.md's text, each
failing closed:

- Only a release by the shape's primary protector (tpm-pin in the TPM
  shape, fido2-primary in the FIDO2 shape) or a confirmed reseal plans.
  "Every tpm-pin token but the one that released or was resealed" is
  read after a tpm-pin release: a fido2-recovery or recovery-key release
  without a reseal plans nothing, orphans included, as the device-bound
  recovery without a reseal does, so a boot through a security key, a
  locked-out TPM's included, never retires the tpm-pin protector.
- An orphan keyslot is killed only beside a marker naming the recovery
  keyslot, never the opener's, the marker's or one a protected or
  foreign token names; without a marker, in the FIDO2 shape, no keyslot
  is an orphan, since keyslot 0 could still hold the recovery key. An
  orphan token is a tpm-pin, fido2 or marker token naming no keyslot;
  a lockout or `td-request` token names none by its format.
- A reseal whose token would exceed the two-token bound first retires
  every tpm-pin keyslot and token, keyslot then token, under the
  opener, as the device-bound planner retires early at its bound; it
  then adds at the lowest free keyslot from 1, imports at the lowest
  free number, which cryptsetup chooses, tests, and retires the old
  protectors under the new keyslot's key.
- The lockout token's writes are the planner's too: `lockout_import`
  imports it as the number the header's lockout token holds, with
  `--token-replace`, or else as the lowest number no token holds,
  naming it explicitly so that the take's later writes can name it;
  `lockout_proven` replaces it there with `proven` set, and
  `lockout_remove` removes it. Only a header `check` admits in the TPM
  shape takes one; a FIDO2-shape header is refused with `NoLockout`.
- A selector update's token steps are the planner's too, since 8e:
  `predicted_import` imports the pre-seal's object as a tpm-pin token
  naming the keyslot that released, at the lowest free number, only on a
  TPM-shape header whose one tpm-pin token names that keyslot
  (`NotPredicted` otherwise, as a cut commit leaves two); `retry_write`
  writes the `update-retry` token in place of the header's own, with
  `--token-replace`, or as the lowest free number; `retry_remove`
  removes it; `update_retry` reads it.

Every kill carries the key of a keyslot that remains, and `Plan::confirm`
compares cryptsetup's own metadata with the header the plan was
computed from, as `transition`'s does; the release confirms once, before
its first write (`protected_plan::confirm`). A step whose header would
not fit is refused by cryptsetup's own commit, which leaves the header
unchanged: the plans stay within the reader's per-class bounds, which
the header measurement below covers with 1 KiB to spare.

**Upgrade plans.** Since 8f, `upgrade` is the upgrade's planner, pure
as `transition` is, over `luks2::read_upgrade`'s view: the header,
its `Volume` and the length of the used copy's JSON text, up to its NUL
padding. `Volume` holds the data segment's identity, whether a
mandatory requirement starting `online-reencrypt` is present (cryptsetup
2.8.8 writes `online-reencrypt-v2`), the keyslots of type `luks2`, and
the `mode` of each keyslot of type `reencrypt`, a reencryption's own.
`luks2::upgrade_metadata` reads the same from cryptsetup's dump, so a
plan's confirmation compares them too. The identity
(`luks2::segment_digest`) is the SHA-256 over the `digest` text, a zero
byte and the `salt` text, in the base64 cryptsetup writes, of the one
digest naming segment 0, or none when no single digest does; phase 1
writes it as the `td-upgrade` token's `from`. Measured with the pinned
cryptsetup on a file, an initialized reencryption keeps the old digest
on segment 0, and a finished one leaves one keyslot, the rewrapped
recovery key at the lowest number free at its start (keyslot 1), one
segment and a new digest. `check` reads the state from the header
alone: U2 while the requirement is present, then U0 or U1 while the
identity equals `from`, U0 when a device-bound td token names a
keyslot, U3 once it differs and no marker exists, and from the marker
U4, or U5 when every fido2 token names a keyslot and, in the TPM shape,
a tpm-pin token names one and a lockout token exists. `needs` says what
the boot gathers before the cap, every pending fido2 token, the TPM
shape's tpm-pin protector when none names a keyslot, and the recovery
key, and whether a take follows (`lockout`), for which the selector
reads the TPM before the cap even with nothing to seal, in each state
before U5, which gathers nothing. The selector passes a TPM it found
unusable as `Tpm::Unusable` whatever it was to seal. `plan` returns one
state's steps; the selector runs them, reads the header again and plans
the next state. Each plan is ENCRYPTION.md's step 3 for its state, every
step authorized by the recovery key and tested first on the recovery
keyslot, and its `Step`s name the key each one's standard input or key
file carries (`Key`): the recovery key, a pending token's passphrase or
the new tpm-pin secret. `cancel` is U0's cancellation, the `td-upgrade`
token's removal then the pending tokens'. It asks only what makes a
header U0 (no reencryption, the identity unchanged, a device-bound
token naming a keyslot, and no protected token but the pending fido2
ones and `td-upgrade`, so the reader admits what it leaves), not
`check`'s other rules, since its removals lose nothing: an upgrading
header `check` refuses for a token of another type, one a foreign
token joined after phase 1, can still leave the upgrade before its
first commit. `pending_orphans` removes the pending tokens from a
device-bound header. Phase 1's two functions, which 8f's
storage-operation commit adds, are `phase_one_fits` and `phase_one`.
`phase_one_fits` takes the view, the shape and the new key count: it
refuses a header that is not converged (device-bound, keyslot 0 and
exactly one other keyslot, which the one device-bound token names, every
keyslot `luks2`, no reencryption, a data segment identity, and no other
token but pending fido2 ones, `Refusal::NotConverged`), and one too
large. Its size bound
is every later plan's sum taken at once: the view's JSON length grown
by phase 1's imports, each new pending token at its largest (a 255-byte
credential), and by everything the upgrade boot will add, the
reencryption, a keyslot per new key, the marker and, in the TPM shape,
a keyslot and a token at its largest for the tpm-pin protector and the
lockout token, never shrunk by what any of them removes, must leave
the area's last byte (`TooLarge` otherwise), so no state's plan the
boot reaches is refused for size. The worker calls it before its first
`10`, with the count alone, and ends with `21` on either refusal.
`phase_one` takes the re-read view after `13`, the shape and the new
keys' pending fido2 tokens, makes the same checks, which the earlier
call already passed unless the header changed (`28`), then plans each
orphan pending token's removal, each new pending token's import and
last the `td-upgrade` token's import with the shape and the view's
identity as `from`. The worker writes the selector after the plan's
removals and before its first import, and confirms the plan once,
before its first write. `header_fit` builds the largest header
`phase_one_fits` admits with the pinned cryptsetup, a retry over two
orphan pending tokens adding two new keys at their largest in the TPM
shape beside a device-bound token at its bounds, and requires the
bound to leave 1 KiB of the area, so a retry is never refused for size:
the header measured 4317 bytes and its bound 10097. Phase 1's unit
tests pin the bound's exact sum, since the model's sizes sit below
its constants, and that it covers every header the upgrade boot then
reads. `on_mains` and
`resilience` are the power and resilience rules over what the selector
reads from `/sys`. Choices beyond ENCRYPTION.md's text, each failing
closed:

- A token of another type refuses the upgrading header: the
  reencryption rewraps the recovery keyslot alone and wipes the others,
  so a foreign protector would lose its keyslot. So does a keyslot not
  of type `luks2` outside a reencryption, an update-retry or request
  token, a token of the other shape, the FIDO2 shape without a
  fido2-primary token, before the marker any protected token but a
  pending fido2 one and `td-upgrade`, a device-bound token once the
  identity changed, a device-bound token naming keyslot 0, a marker
  naming none, a second tpm-pin token, from U4 a keyslot two tokens
  name, and in U2 a reencryption keyslot whose `mode` is not
  `reencrypt`: resuming a forged `decrypt` would leave the volume in
  clear.
- Only U0 waits for mains power, as ENCRYPTION.md's step 1 says. U1 also
  starts the whole-volume rewrite, but after U0's commit the boot has
  already asked for the recovery key and holds no device-bound
  protector to boot by instead: waiting would only defer the rewrite to
  a later recovery-key boot, so U1 starts on battery with the warning a
  resumption gives.
- U0's plan ends at its own commits, the kills and removals after which
  nothing cancels. U1's plan repeats them for what a cut left, every
  keyslot but 0 and every device-bound token, orphans included, then
  re-encrypts, so no keyslot cryptsetup would wipe remains.
- U3 marks the one `luks2` keyslot no token names; two such keyslots
  refuse, since either could be the recovery key's.
- U4 kills every keyslot no token names but the marker's, removes a
  tpm-pin token naming none, adds each pending token's keyslot at the
  lowest free number from 1, then the tpm-pin protector's. Each
  protector is added, tested, and only then named by its token, the
  fido2 one replaced in place: a fido2 token naming a keyslot is never
  tested again by the upgrade, so a keyslot no test proved stays an
  orphan the next boot kills and adds again. (A protected plan's
  reseal imports before its test, since every boot's release tests its
  tpm-pin keyslot.) Its last step is
  `TakeLockout`, run by the selector with the token `lockout_import`
  places at the lowest free number, when the TPM shape lacks a lockout
  token and the TPM is usable; otherwise the `td-upgrade` token's
  removal, since without a take nothing further makes the header U5.
- A gathered tpm-pin protector or passphrase a state does not need is
  ignored; one it needs and lacks refuses the plan (`MissingFido2`,
  `NoTpmPin`).
- U5's plan is the `td-upgrade` token's removal alone, tested by nothing:
  the protected release already opened the volume.
- The planner trusts `from`. Phase 1 writes it from `read_upgrade`'s view
  alone. A `from` that never matched the volume reads as U3 at once and
  ends without a rotation, but no header holding the device-bound
  protector reads so (a device-bound token refuses U3), and removing that
  protector by hand needs no less than writing the header; what such a
  writer achieves, a volume protected under its old key, is the
  device-bound root's inheritance ENCRYPTION.md's Scope already
  concedes.
- cryptsetup 2.8.8 finishes a reencryption by destroying the old key's
  keyslots, each its own header commit, before the commit that drops its
  requirement (`reencrypt_teardown_ok`), wiping each keyslot's area
  before its commit. A cut from the first wipe to the last commit
  leaves the requirement with no usable keyslot for the old key, which
  no resumption opens
  (ENCRYPTION.md "Threat and limits"): U2's plan is the resumption
  alone, so such a boot fails at it and writes nothing.

Each plan's header must fit the 12 KiB area, judged before its first
step as the view's JSON length grown by what the plan adds and never
shrunk by what it removes: a token's compact text and six bytes for its
number and separators, a replaced token's growth, `KEYSLOT_JSON` (320
bytes; an added keyslot measured 305, 307 at a two-digit number) per
keyslot, `REENCRYPT_JSON` (1536; an initialization measured 1208 bytes
with journal resilience and 1244 with checksum, and a split segment
138 more, above) for the reencryption, and a lockout token at its
largest for the take. The text must leave at least the area's last
byte for its NUL, as cryptsetup and the reader require, so a plan whose
bound reaches 12288 is refused. A removal alone always fits.

**Runner.** The runner gains `reencrypt` in two spelled shapes, current
since 8f: `reencrypt_args`, the upgrade's initialization and run
(`--batch-mode --active-name NAME --key-slot N --key-file=-`, then
`--use-random`, formatting's cipher, `--new-key-size 512`, its sector
size and PBKDF2, then `--resilience checksum --resilience-hash sha256`
or `--resilience journal`), and `reencrypt_resume_args` (`--resume-only
--batch-mode --active-name NAME --key-file=-`), naming no keyslot, since
cryptsetup then holds one for each volume key and tries the key on both.
Both read the recovery key on standard input. cryptsetup 2.8.8 sizes the
new key by `--new-key-size`, reading `--key-size` only for a header
without keyslots, and a test ties each value to formatting's. It loads
an online reencryption by the active mapping's name and uses the device
operand only in messages, so the selector must hold `NAME` over the
partition it read. Progress is not in these shapes: with
`--batch-mode` cryptsetup prints none, and `--progress-json` streams
past `run`'s output bound over a long reencryption, so the selector's
commit adds it beside a run that reports progress as it reads it
(ENCRYPTION.md asks at least every percent). `token import` with and without
`--token-replace` for a token id, the second current since 8c
(`token_replace_args`), is cryptsetup's one header commit replacing the
token in place. Its
`luksKillSlot` floor stays 32 bytes, which a FIDO2 passphrase and the
recovery key's 48 digits meet.

**Release.** Since 8c the protected path is its own module,
`protected_release`, beside the device-bound `release::release`, which
td-boot keeps for device-bound and upgrading headers; `release` refuses
a header whose `Kind` is not protected. Its `Released` names the method
(tpm-pin, fido2-primary, fido2-recovery or recovery key) that the
admission record follows, `Method::admits` the two primary ones; FIDO2
comes through `protected_release::Fido`, which td-boot's worker
implements and tests script, and every console entry through `Prompt`,
one run of secret-line each. The `release::Tpm` trait gains the
protected tier's operations as default methods over td-tpm's salted
sessions (`dictionary_attack`, `primary_name`, `seal_pin`, `unseal_pin`
and `lockout` with a `LockoutCommand`), so the release tests stand a
model in for the sessions while the chain check, the PCR reads and the
cap stay on the scripted wire; an emulator test runs the defaults
against swtpm. `release` runs, before the cap, the chain check, the
warning, the TPM PIN, another way and the verified reseal, then the cap
and the released keyslot's test; `finish`, after the selector opened
the mapping with the volume key, confirms cryptsetup's metadata once
before the first write and runs the plan, the take or proof, the
counter and the reset. `policy_pcr_alone` is the deployment
initramfs's post-cap check. Choices beyond ENCRYPTION.md's text, each
failing closed:

- A reseal passes the storage-primary Name every tpm-pin token records
  to `seal_pin`'s `expected`, and is not offered when the tokens record
  different Names or the chain check found another primary
  (`ChainError::OtherPrimary`): a primary that appeared since the first
  seal never receives a new authValue and secret ("Bus"). A cleared or
  replaced TPM, whose primary is new, therefore gets no reseal at boot;
  a TPM that refuses the Load under the recorded primary does. With no
  tpm-pin token the seal is a first one and passes none.
- After the cap, before a plan adds its keyslot, every seal, a first
  one included, requires its primary's Name to be the one a lockout
  token on the header read at release records under a tag that
  verifies under the volume key. Without one, or under another Name,
  the object is discarded before its import and the boot continues by
  the method that released it; a first take's own token anchors the
  next boot's seal, not this one's. So no reseal is offered before the
  cap on a header with no lockout token (`NO_ANCHOR`): its seal could
  only be discarded and would send the new PIN's authValue for nothing.
- A TPM-primary header with no tpm-pin token naming a keyslot prints
  the changed-chain warning before any prompt, after the line saying it
  holds none: a cut update, a TPM clear or a rewritten header removed
  them, and the warning is what stands between a disk-writing
  interposer and the owner's key (ENCRYPTION.md "Threat and limits").
- A take never replaces a lockout token whose tag verifies but names
  another primary: that TPM, cleared, replaced or intercepted, gets no
  lockout step at boot (ENCRYPTION.md "Lockout").
- A keyslot test that fails after the cap discards a confirmed reseal
  before another way is asked, so a key that opens nothing cannot
  confirm one that the next key's release installs.
- With a confirmed reseal the take or proof runs before the reseal's
  plan, which runs only once it succeeded, so a failed one discards the
  object before its import; without one the plan runs first. The lockout
  token's writes are the planner's, at the numbers it names.
- The counter is read once the release's test passed, before a take's
  own reset would clear it, and printed after the take or proof, in the
  TPM-primary shape only, where it counts td's PINs.
- The TPM is reached after the cap only when this boot's own cap closed
  PCR 12. An already-closed cap, or a TPM without a SHA-256 bank, after a
  security-key or recovery-key release opens the volume with the plan
  but no lockout step; after a TPM PIN release or a verified reseal, any
  cap error halts, every secret zeroed.
- A SetPrimaryPolicy refused as a wrong authorization (`AuthFail`)
  removes an unproven token and keeps a proven one. One refused with
  `Lockout` checked nothing and keeps either. Its success proves
  possession and marks the token proven before DictionaryAttackParameters,
  whose refusal keeps it. A proof refused for another primary, or
  failing any other way, keeps the token and sends nothing more that
  boot.
- A header the planner refuses after a release opens the volume with no
  write and nothing sent to the lockout hierarchy.
- A sealed object over the token's bounds is refused at its seal, before
  the cap, with no keyslot added and no tpm-pin token imported; the
  boot's lockout step runs as without a reseal.
- A selector update's pre-seal (ENCRYPTION.md "Selector update", since
  8e): after a TPM PIN release and before the cap, `release` calls the
  caller's `PreSeal` hook, and a `Predicted` it returns (PCRs 4, 7 and
  9) is sealed from the released secret under the typed PIN with a
  fresh `/dev/random` salt, to the token's own selection at the
  predicted values and a literal-zero PCR 12, under the primary the
  token records (`expected`). Its chain is not running, so no unseal
  verifies it; its bounds, its public area's policy digest and the
  primary's Name are checked instead, and any refusal keeps no object.
  `pin_loop` returns the PIN with the secret for this, dropped once the
  hook has run. The object is discarded if the released keyslot's test
  fails, and `finish` hands it on in `Finished::predicted`, naming the
  keyslot that released, unless it refused every header change this
  boot, and only beside a lockout this boot took or proved whose token
  on the header read at release verifies for the object's primary
  under the volume key (`protected_plan::anchors`), the reseal's
  anchor; `predicted_import` requires that anchor again
  (`NotAnchored`). `write_tokens` runs the commit's token steps,
  imports and removals only, after one confirmation of cryptsetup's
  metadata against the header the caller read afresh.

Its bounds grow by the FIDO2 attempts and the entries, which are the
person's, one seal and one verifying unseal per reseal, and the
lockout's commands: a take's three calls (td-tpm's take, which sends
HierarchyChangeAuth and SetPrimaryPolicy, then
DictionaryAttackParameters and LockReset) or a proof's two
(SetPrimaryPolicy, DictionaryAttackParameters), then at most one
counter reset and, from its increment, one requested clear.

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
four device-bound td tokens, orphans included, among 32. A protected
token encodes in at most 1038 bytes (a tpm-pin token's sealed areas at
most 128 and 256 bytes, a fido2 token's credential at most 255), and a
header carries at most two tpm-pin, four fido2 and one each of the
marker, lockout, `td-upgrade` and `td-request` tokens. The lockout's
HKDF expands two blocks. A plan has at most 39
steps: two tests, a kill per keyslot from 1 to 31, a removal per td
token, an add and an import. The runner's inputs are at most one page.
A protected plan has at most 48 steps: a kill per keyslot but the
opener's, a removal per td token, an add, an import and a test. An
upgrade plan has at most 50: a test, a kill per keyslot but the
recovery keyslot, a removal per orphan tpm-pin token, an add, an import
and a test per pending fido2 token and for the tpm-pin protector, and
the take or the `td-upgrade` token's removal.
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
the order tokens are tried in. No gate test reads a header cryptsetup
wrote: that needs the source-built cryptsetup and its kernel crypto
interfaces, which the host gate does not provide; the protected tier's
header measurement below does, ignored by default. Increment 5's encrypted-installation oracle
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

Protected-token tests pin each 8a token's exact encoding for fixed
values and the largest encoding of each kind (tpm-pin 1038 bytes, fido2
824, marker 63, lockout 376, `td-upgrade` 125, `td-request` 60) as
literals that `tests/token_vectors.py`, a stdlib-only host tool and no
build input, computes independently (Python's `json`, `hmac` and
`hashlib`), round trip them, and refuse every key, type, role, keyslot,
value and length rule above, a constructed repeated key and oversized
text, and an encoding outside the bounds. Lockout tests pin the
ciphertext, tag and token text of a fixed volume key, UUID, nonce,
authorization and Name against the same tool's HKDF and HMAC, open it
back, and refuse another volume key or UUID, a flipped byte of the
nonce, ciphertext, tag or Name (the Name under the tag, so moving it to
this TPM's fails the tag), another primary's Name, an empty volume key,
and a verifying tag over an authorization of zeros; the nonce is one
exact read of an injected source, and UUID texts of either case parse
while malformed ones refuse. Reader tests read protected, U0, U5 and
interrupted-phase-1 headers, with two tpm-pin tokens on one keyslot and
a foreign token's keyslot, compare `metadata`, and refuse each class
over its bound, every mixture of a device-bound token or orphan with a
protected one, two device-bound tokens beside `td-upgrade`, a
malformed protected token, a lockout naming a keyslot, a role td does
not know yet and a token naming an absent keyslot. Upgrade-view tests
read cryptsetup 2.8.8's own dumps of a U1 header, of it after
`reencrypt --init-only` and of a header after a whole reencryption, pin
the data segment's identity against Python's hashlib, read the same
view from a header area with its text's length, and leave the identity
empty for no digest, another segment, a missing digest or salt and two
digests naming segment 0, reading the requirement from its versions and
refusing a malformed one. Planner tests refuse
protected and upgrading headers and leave pending fido2 tokens beside a
device-bound header with their numbers taken. Upgrade-plan tests, over a
model of cryptsetup's commits whose reencryption rewraps the recovery
key at the lowest free keyslot, refuses one whose unlocked keyslot a
token names (cryptsetup would move the token), needs a keyslot of each
key to resume and wipes every old one at its end, and whose every state
the reader admits as cryptsetup's metadata, drive the upgrade from U0 in
both shapes and resilience modes (which differ in the model only in
their arguments), cut after every step, after the reencryption's
unbound keyslot and after its initialization, and the boot that resumes
it cut the same way, and require one final header: the shape's
protectors and the
marker only, every keyslot named, the volume key rotated, the recovery
key, each fido2 passphrase and the tpm-pin secret opening theirs, the
lockout token in the TPM shape, no `td-upgrade` token, and
`protected_plan::check` admitting it. They pin the states a cut leaves
in the table's order with what each gathers, U5's plan needing no
recovery key, the U0 to U3 and U4 plans and their arguments, an
unbound keyslot killed in U1, a cut in cryptsetup's last commits
planning only the resumption, a TPM unusable from the start or from
any cut ending the upgrade without adding a tpm-pin or lockout token
and one unusable at the take planning none, the mains and resilience
rules, a cancellation at every cut, of a U0 header `check` refuses, and
an interrupted phase 1 leaving a device-bound header without pending
tokens, no cancellation after U0, every refusal above with forged
headers, a reencryption in another mode, a plan that would not fit by
one byte, a secret not gathered, no free token number, metadata that
disagrees in the data segment or the requirement, and the lockout
import's state. Every keyslot from 1 taken is the planner's refusal
too, but a U4 header cannot reach it: a keyslot no token names is an
orphan its plan frees. Protected-plan tests, over
a model of cryptsetup's commits whose every state the reader's bounds
must admit, pin a tpm-pin release retiring a superseded protector, an
orphan keyslot and an orphan token; two tpm-pin tokens on one keyslot
keeping it for the one that released; a fido2-recovery or recovery-key
release planning nothing; the reseal after each, and beside two tpm-pin
tokens retiring both first; no reseal after a tpm-pin release or in
the FIDO2 shape; a fido2-primary release removing only orphans, and no
keyslot as an orphan without a marker; forged headers refused before
any step (a tpm-pin token on a fido2 or the recovery keyslot, a fido2 or
foreign token on a tpm-pin keyslot, a fido2-primary token beside a
tpm-pin or lockout token, the TPM shape without a marker or with one
naming none, an upgrading and a device-bound header), each also
refusing the lockout import; an opened token of another kind or naming
no keyslot; metadata that disagrees; no free keyslot or token number and
an object over the token's bounds; the lockout token's import, proof
mark and removal at the numbers the planner names; and every plan, a
tpm-pin release and the reseal beside one and two old protectors, cut
after each of its steps, converging over the next boots on one header.
Protected-release tests run `release` and `finish` over the scripted
wire TPM with a model of the salted sessions, a scripted FIDO2 worker,
scripted secret-line entries, and a cryptsetup model that requires PCR
12 capped before every command and every state to pass the reader's
bounds; the model refuses a lockout command before a keyslot's test has
passed and a seal after the cap. They pin a PIN release and its take
(token on the header first, then the change, the parameters and the
reset, then proven) and the next boot's proof (SetPrimaryPolicy before
DictionaryAttackParameters, no reset, no write); a codec-refused entry
costing no TPM command; wrong PINs counted and the counter printed
after the test; a lockout turning to another way and that release's
reset, and a lockout answered at the Unseal; a changed chain, a fresh
TPM (a new primary) and another primary each asking no PIN and warning
before any prompt and offering no reseal, and a Load refused under the
recorded primary offering one; no SHA-256 bank asking no PIN and taking
nothing; an unmeasured PCR 4 or 9 offering no reseal; each
FIDO2 role and the recovery key in both shapes, with no reseal, take or
write in the FIDO2 shape; a wrong recovery key asking again after the
cap; the reseal confirmed (verified before the cap, requiring the
recorded Name, planned after the take, the next boot taking the new
PIN), declined, and failing at its seal, its verification and the
token's bounds; the pre-cap lockout rule; a refused take discarding the
reseal and its unproven token proven on the next boot; a verified
token's L reused and a forged one replaced; a refused proof removing an
unproven token and keeping a proven one, a locked lockout hierarchy
keeping an unproven one, a refused DictionaryAttackParameters after a
proof keeping it proven, and an interposed take; a seal's keyslot
added only under the anchored primary (an interposer's primary taking
nothing, a lockout token whose tag fails anchoring nothing); a failed
keyslot test discarding the pending reseal; a header with no lockout
token offering no reseal and its take anchoring the next boot's; no
tpm-pin token warning before any prompt; no TPM device; end of input and failed applets; every cut of the reseal with
its take and of a PIN release's plan converging; forged headers (a
foreign policy, a tpm-pin token on the fido2 or recovery keyslot, a
lockout token whose tag fails, metadata over the bounds, a mixed shape,
an upgrading header) releasing nothing unverified and writing nothing;
the post-cap check's PolicyPCR alone; that every secret is dropped and
none reaches a console line. td-fido's `hkdf_expand`
is pinned to RFC 5869's three SHA-256 cases at their whole lengths,
with the info split at every offset, one block agreeing with `hkdf`, and
an output over 8160 bytes refused untouched.

The header measurement (`tests/header_fit.rs`) runs the pinned
cryptsetup on 20 MiB files with td's own `luksFormat`, `luksAddKey` and
`token import` argument lists and pins the JSON sizes above, each at
least 1 KiB under the 12 KiB area, the two copies equal. It also holds
the upgrade's growth bounds to what cryptsetup writes, and runs the
upgrade from a device-bound header with phase 1's tokens to a protected
one: each state's plan confirmed against cryptsetup's metadata and run
through the runner's arguments, the TPM shape in both resilience modes,
uncut and interrupted after the initialization, and the FIDO2 shape
interrupted so. A file has no mapping, so the reencryption runs as the
runner spells it with `--init-only` in place of `--active-name
td-selector`, and resumes offline (`--force-offline-reencrypt
--disable-locks`); the online shape is first run by the selector's
oracle. The result is a header of the shape the protected planner
admits, its volume key rotated, each protector opening its keyslot and
a wrong key refused. It also emulates a cut in cryptsetup's last
commits, keyslot 0 killed after an initialization, which no resumption
opens:

```
TD_TEST_CRYPTSETUP=/absolute/path/to/cryptsetup cargo test --frozen --manifest-path td-protector/Cargo.toml --test header_fit -- --ignored
```

The keyslot test (`tests/keyslot_test.rs`) runs the same cryptsetup
on a 20 MiB volume formatted the same way, keyslot 0 with its
recovery-key marker and keyslot 26 with a fido2 token, whose second
copy is one sequence number behind, or damaged. `detached_test_args`
over a copy of `test_area`'s bytes, through `run_bounded`, exits 0 for
each keyslot's key and `EXIT_BAD_PASSPHRASE` for a wrong one; the
volume's SHA-256 is unchanged after each, and the copy's second header
is not, which is cryptsetup's repair landing there. Naming the volume
as `--header` changes it, for a wrong key too:

```
TD_TEST_CRYPTSETUP=/absolute/path/to/cryptsetup cargo test --frozen --manifest-path td-protector/Cargo.toml --test keyslot_test -- --ignored
```

Both need the host kernel's AF_ALG hash and skcipher interfaces and no
privilege; a regular file serves as the data device, with no loop. The
cryptsetup is the `cryptsetup-x86-64` recipe's output,
`bin/cryptsetup.static` under the path `td-recipe-eval build-run
cryptsetup-x86-64` prints.
