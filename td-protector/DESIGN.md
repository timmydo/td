# td-protector: the device-bound disk protector

td-protector is the disk-protector policy of
[ENCRYPTION.md](../td-install/ENCRYPTION.md)'s device-bound tier, carried
for both of its consumers: the installer, which probes for a usable TPM
with the observed-policy read before a device-bound service starts and
seals and verifies the first-boot protector (increment 5), and the
selector, which releases, seals
the device-bound protector and caps PCR 12 (increment 6). It runs over the
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
unchanged, and the pure transition planner ("Transitions"); the release
orchestration that runs ENCRYPTION.md's release order is its target,
not yet implemented.

## Policies

Every policy is exact PolicyPCR over the SHA-256 bank then
PolicyCommandCode(Unseal), td-tpm's `PcrPolicy`, with no PolicyAuthorize.

- **First boot.** `first_boot_policy` selects PCR 12 alone at its literal
  reset value of zero. It reads nothing from the TPM, so the installer can
  seal it from the live medium, whose selector has, from increment 6,
  already capped PCR 12.
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

This is increment 6's target; `unseal` does not yet type its errors.
Its `UnsealError` separates the refusals that release nothing from
everything else, so that the deployment initramfs's post-cap check
(ENCRYPTION.md "Boot and authority boundaries") halts on no expected
refusal:

- `PolicyRefused`: only `TPM_RC_VALUE` on PolicyPCR's first parameter
  (0x1c4), PCRs that differ from the policy's, and `TPM_RC_POLICY_FAIL`
  on Unseal's first session (0x99d).
- `LoadRefused`: TPM2_Load's integrity, seed and handle failures, the
  answer of a cleared or different TPM whose storage primary did not
  create the sealed object. The commit that adds it pins those codes.
- `Other`: a transport error, a reply that does not answer the command,
  and every other response code.

## Release cap

`cap` reads PCR 12 and requires zero, extends it once with `cap_event()`,
the SHA-256 of `td/disk-protector/release-cap/v1`, and requires the exact
readback `SHA256(zero32 || cap_event())`. It never retries. Its error is a
typed `CapError`, each displayed with PCR 12 context:

- `AlreadyClosed`: PCR 12 read non-zero, so nothing was extended. Every
  protector policy requires PCR 12 at zero, so no TPM release is possible
  this boot.
- `Uncertain`: a PCR 12 read or the extension failed in transport or was
  refused, so the cap's state is unknown: a failed prior read leaves PCR
  12 unmoved but unverified, and a failed extension or readback may or
  may not have moved it. Either way release cannot be shown closed.
- `Mismatch`: the readback differs from the expected value.

The caller's contract follows ENCRYPTION.md's release order. The selector
tries every td token, up to the fixed bound; when the first-boot
protector alone released, it performs the first-boot transition's seal
and verification unseal; then it caps exactly once, whether or not any
unseal succeeded, before cryptsetup parses the header. ENCRYPTION.md's
release order (step 4) says what each `CapError` leads to. No other td
component extends PCR 12.

## Installer check

`verify_first_boot_object` runs the first-boot policy in a TPM trial
session, which td-tpm refuses unless the TPM's PolicyGetDigest equals the
local digest, then requires the sealed object's public area to carry that
digest as its authPolicy with the fixed sealed attributes. It then loads
the public and private pair under the storage primary with td-tpm's
`load_and_flush`, so the TPM verifies the private area's integrity, and
flushes both. Nothing is unsealed: the installer never unseals the
first-boot protector, and from increment 6 the live selector has already
capped PCR 12.

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
first byte, such as a window onto the installer's claim or the selector's
opened partition, and returns the used copy's sequence number, size,
UUID, label, keyslot numbers, td tokens and orphans with their token
numbers, and each token of another type with its number and the
keyslots it names. It runs no cryptsetup and needs no privilege.
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
material to standard output (`luksDump --dump-volume-key`, say) may run
through `run`. `metadata` runs `luksDump --dump-json-metadata` and
returns its output instead, the header's public JSON.

`run` succeeds only on exit 0 and otherwise names the program, the verb
and the exit status, never an argument. `mapping` asks `status NAME` of
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
remains; `luksDump --dump-json-metadata`; `open`, `close`, `status`;
and `open --test-passphrase` on one keyslot. Given a key, cryptsetup
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
orchestration, increment 6's target, executes the plan through the
runner after the cap. `transition::classify` names the transition the
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
  rule; the protected-tier upgrade, which re-encrypts online, must run
  no plan while one exists.

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

## Evidence

Unit tests over a scripted TPM that evaluates PolicyPCR and the Unseal
authPolicy pin the cap event and first-boot policy digest literals; seal
then unseal under both policies; that the first-boot policy reads nothing
and the observed policy reads exactly PCRs 4 and 9; refusal of an
unmeasured PCR 4 or 9; that after the cap both policies are refused at
PolicyPCR with no Unseal sent; each `CapError`, with no extension after a
non-zero prior and no retry after a refused extension or a lost reply at
any of the three exchanges; the installer check's command stream and its
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
token types name, and a read error. Runner tests pin every command's
arguments word for word, that a key file is a close-on-exec descriptor
holding exactly the secret, the one-page input bound, a stand-in
cryptsetup's cleared environment and inputs with no key in its argv, a
relative program refused, and `status` exits 0, 4 and others. Planner
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
31-byte key before any child starts. No test reads a header cryptsetup wrote: that needs the source-built
cryptsetup and its kernel crypto interfaces, which the host
gate does not provide, so increment 5's encrypted-installation oracle
(`qemu-install-encrypted`) reads one in the guest: the installer's
verifying-boot check runs this reader and `verify_first_boot_object` on
the header cryptsetup wrote, under the pinned swtpm, and the oracle
requires the recovery-key phase that check gates. An ignored oracle runs the protector lifecycle against
the pinned swtpm under td-secret's convention (`td-secret/DESIGN.md`, "TPM
validation"):

```
TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --frozen --manifest-path td-protector/Cargo.toml emulator_ -- --ignored
```

A normal cargo pass with it ignored is not TPM integration evidence.
