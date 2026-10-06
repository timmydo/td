# Login keys and session lock

This is the normative target for td's login-key tier. td-authd, td-secret,
td-compositor and td-login implement it together; this document owns the
tier's rules, and each component document states the amendments its own
contract needs. **Only the record codec, the record store, the consent
descriptions, the CTAP login primitives, the root worker and td-authd's
supervision of it and the compositor's client of that supervision are
implemented**, inert: td-secret's private `login-operation` worker
unlocks, enrolls, adds and removes keys against the record
("Placement"), and td-authd starts and drives it on the paired requests
`1b` and `1c`. The compositor's key-management screen sends `1b` for a
first enrollment and an addition, which a production td-authd refuses
before starting anything; it refuses removal itself, having no key list
yet, and nothing sends an unlock's `1b`. Its PIN field sends `1c` only
when root asks for a PIN at a presented PIN step, which no production
operation reaches. So nothing
in production starts the worker or uses its `login_record` and
`login_store` modules ("The login record"), its login identify,
PIN-retry, assertion and creation steps ("Token profile"), or td-authd's
login consent operations, step admission and supervision
(`td-authd/DESIGN.md`, "Login-key operation supervision"). No
deployment carries the tier marker yet, so the worker reads no record
version for either retained deployment and
refuses every write that would leave a record ("Versions"). Its
`qemu-secret` guests, standing in for that marker, run it over UHID
virtual keys through every case increment 2 lists, including power cuts
inside its writes on a disposable disk ("Evidence"): increment 2 is
complete. Of increment 3, the compositor's exclusion of a security
key's own keyboard from secure attention has landed, and it is live: it
narrows the existing attention selections and confirmation and needs no
record (`td-compositor/DESIGN.md`, "Physical secure attention"). The
private client's login-key operations have landed too
(`td-compositor/DESIGN.md`, "Login-key operations"): the `K` screen,
each operation's descriptions checked step by step, its commit and the
failure texts ("Failure texts"). Production reaches only their refusals.
The PIN field has landed as well ("PIN entry, presence and retries"),
inert, since production never reaches a PIN step. The lock surface has
not landed, and nothing else below is implemented.
Until the increments at the end land, `THREAT-MODEL.md` §3 is the
complete current behaviour: the installed account logs in automatically
and the session never locks. No document, UI or release
note may describe this tier as available before its acceptance evidence
exists.

**Enrollment requires §L.1 elevation.** Enrolling a key refuses every
interactive login, and on a fresh install `su` cannot elevate from a
session and root has no SSH key, so an enrolled machine would otherwise
have no administrative path. The activation increment therefore lands only
after the `APPLICATIONS.md` §L.1 consent-only elevation increment, which
also retires `su` and root's empty shadow field ("Retiring the escape
hatch"). Enrollment does not exist on any build without that elevation.
Root login is never re-enabled as an administrative path.

## The tier

Installation stays passwordless (`td-install/INSTALLER.md`). After logging
in, the account holder may enroll removable FIDO2 keys, each used with its
own PIN. Enrolling the first key turns automatic login off for that
account: from then on every boot, every manual lock, every lid close and
every resume from a suspend longer than about three seconds stops at a
compositor-owned lock screen that one enrolled key plus its PIN opens.
Removing the last key turns automatic login back on.

Under AGENTS.md principle 7 the PIN authorizes a hardware-held credential,
so this is user authentication, not device binding. The tier is TPM-free:
no TPM, PCR or sealed state takes part, so it serves machines with TPM 1.2
or no TPM, such as the ThinkPad T430s. Only the configured human, UID 1000,
can enroll; root and service accounts have no login keys.

## Scope

The tier defends against a person at a booted td machine whose session is
locked, who lacks an enrolled key and its PIN, and who uses the
interactive surfaces td offers: the lock screen, the secure-attention
screen, td-login's interactive console paths and SSH. On an enrolled
machine none of these opens a session for that person without a key (the
SSH self-test key is readable only by the session's own account), and
administration is §L.1 elevation from an unlocked session.

It does **not** protect:

- **A powered-off machine whose disk is not encrypted.** Anyone who can
  start other code on it reads and writes the disk and can delete the
  login record (Recovery). The cheapest route usually needs no prepared
  medium: a firmware boot menu or UEFI shell that boots another OS or
  starts td's selector with added load options. Disk encryption is
  `td-install/ENCRYPTION.md`'s separate work, which also owns how the
  tiers combine ("Device-bound default").
- Changes to firmware settings, boot order or load options, attached boot
  media, DMA, memory extraction, kernel bugs in surfaces reachable from a
  locked machine, and malicious input devices (`td-compositor/DESIGN.md`,
  "Physical secure attention").
- Software in an unlocked session, and a person at an unlocked session who
  arrived before the lock (`APPLICATIONS.md` §L.1 scope). Idle-timeout
  locking is a follow-up.
- Root, which can remove or rewrite the record, and anyone holding an
  enrolled key together with its PIN.

## Placement

**td-authd** (root) alone starts a login-key operation and alone decides
that a session unlocks; locking needs no authority. It supervises one
fixed root worker, `/bin/td-secret login-operation --uid 1000`, launched
like the existing unlock worker. Its requests, deadlines, step admission
and revocation are specified as planned changes in `td-authd/DESIGN.md`,
"Login keys and session lock".

**td-secret**'s worker owns all CTAP2 traffic. It reuses `PORTABLE.md`'s
PIN-authorized hmac-secret flow, transaction runner and P-256 verification
over the root-only USB transport, each token session serialized by
`/run/td-fido/operation.lock` (`td-secret/DESIGN.md`, "USB token
transport"); td-authd's single operation slot, not that lock, covers the
gaps between one operation's sessions ("The login record", below). That
is the exclusive mediation §L.1 requires: raw token nodes
stay root-only, td-owned workers serialize, and every assertion is bound to
one presented operation. The worker verifies assertions and publishes the
record; td-authd trusts its success frame only together with its observed
successful exit. Before any presentation or token I/O the worker reports
the state it read: unenrolled, or the enrolled record's slot count and
fingerprints in canonical order, which is the baseline td-authd's step
admission uses. An unavailable state reports its cause instead and the
operation ends. Every failure reaches td-authd as a typed kind, never as
text; `td-secret/DESIGN.md`, "Login-key worker", gives the frames.

**td-login** never talks CTAP2, verifies an assertion or parses the
record. It is the credential-switch program, single-threaded with three
confined syscalls; a device protocol would widen that surface for nothing.
It only refuses interactive logins when a record may exist
(`THREAT-MODEL.md` §3).

**td-compositor** (UID 993) owns the lock surface, every presentation and
the PIN field. As for §W.4 operations, root cannot observe its framebuffer
and trusts the paired peer's receipts. Its amendments are in
`td-compositor/DESIGN.md`, "Session lock and login-key entry".

There is no public intake. No application, human-UID process, control
socket, automation session or VM bridge can start a login-key operation or
an unlock. Only physical secure-attention input does: a selection on the
attention screen, or the chord itself on the lock surface. Locking is the
exception: any input source may lock, since a lock grants nothing.

## The login record

The record is `/var/lib/td/login/1000`: a regular, single-link,
root:root mode-0600 file in the root:root mode-0700 directory
`/var/lib/td/login` on the persistent `@var` volume. Only the root login
worker publishes or removes a record.

Firstboot ensures the directory on every boot before any consumer runs. It
never creates or removes a record, and it removes leftover temporaries
(below). It refuses rather than repairs invalid existing metadata.

The login state is **unenrolled** only when the directory is valid and the
record name is absent, **enrolled** when a valid record is present, and
**unavailable** otherwise. The first test is a directory-and-name check,
the same predicate in firstboot, td-login and td-authd, and needs no
helper; only a name that exists is parsed, so nothing about the record's
bytes can make an unenrolled machine look enrolled or the reverse
(`td-authd/DESIGN.md`, login-state amendment). Unavailable has three typed
causes, each with its screen text:

- a damaged directory (a wrong owner, group or mode, a non-directory or
  nothing at that path, or a symbolic link at or above it):
  `LOGIN KEY STATE UNAVAILABLE: DIRECTORY DAMAGED`;
- a damaged record (the name exists, but is not a regular single-link
  root:root mode-0600 file, or its bytes are truncated, malformed, of an
  unknown version or for another UID): `LOGIN KEY STATE UNAVAILABLE:
  RECORD DAMAGED`;
- a state that could not be read: td-authd's read-only helper did not
  answer in time, or a read met an I/O error, other than the damage
  above, or saw the record change while reading it: `LOGIN KEY STATE
  UNAVAILABLE: STATE COULD NOT BE READ`. It is transient: the compositor
  asks again until it resolves.

Unavailable keeps every refusal of the enrolled state in force (td-login's,
SSH's enforced form, the update-consent rule) and the compositor stays
locked with no unlock. It is not boot-fatal and does not fail boot health:
firstboot reports a damaged directory on the console and continues.
Rolling back cannot repair it, because both deployments share `@var`;
recovery is physical (Recovery). Stopping boot instead would also take
down boot health and the diagnostics that make the state repairable.

The record's presence is the only enrollment state. Present means enrolled
and automatic login off; there is no second flag, `/etc/autologin` edit or
shadow change. Its contents, with exact bytes (below) pinned against
independent literal vectors (`td-secret/tests/login_record_vectors.py`),
are a magic and version, the UID, a random 32-byte record ID fixed at
first enrollment, and one through eight
slots in canonical credential-ID order. Each slot holds a bounded nonempty
credential ID, the canonical P-256 public key, a random 32-byte salt and a
32-byte verifier. Unknown versions, out-of-range counts, duplicate
credentials and trailing bytes refuse. Keys have no roles and the record
holds no recovery-policy byte: the slot count is the state, as in
`PORTABLE.md`. No counter, AAGUID, label or timestamp is stored.

**Bytes.** Integers are unsigned big-endian. Version 1 is:

| Field | Encoding |
| --- | --- |
| Magic | Eight bytes `TDLOGREC` |
| Version | u8, 1 |
| UID | u32, which must equal the expected UID |
| Record ID | 32 random bytes |
| Slot count | u8, one through eight |
| Slots | Repeated below, strictly increasing by credential bytes |

Each slot is a u16 credential length (1 through 1024, td-secret's CTAP
credential bound), the credential ID, the public key as its 32-byte x and
32-byte y coordinates, canonical field elements on P-256, the 32-byte salt
and the 32-byte verifier. Raw coordinates rather than `PORTABLE.md`'s COSE
form: the record stores one key type and no algorithm choice. The largest
record is 46 + 8 * 1154 = 9278 bytes, and a longer file refuses before
parsing. Every field has exactly one encoding, so re-encoding a decoded
record reproduces the bytes read.

**Versions.** A deployment that carries the tier lists, in its tier marker
(Deployments), the record versions it reads. Enrollment and every mutation
write the lowest version that the writer and both retained deployments
read, and refuse when there is none. Together with td-authd's
update-consent rule (Deployments), rolling back to `previous` therefore
never meets an unreadable record.

The verifier is one 32-byte block of HKDF-SHA256 with the slot's
hmac-secret output as input keying material, an empty salt, and an info
string of `td-login/verifier/v1`, a zero byte, the u32 UID, the record ID
and the u32-length-prefixed credential ID. It is compared in constant
time. A login needs both checks:

- the signature over a fresh root-generated challenge proves the key took
  part now, so no recorded assertion verifies twice;
- the verifier binds the hmac-secret output of a signed UV assertion, so a
  key that does not reproduce it is refused, and a later store release can
  derive from the same output under a different label.

A copy of the record reveals public keys, credential IDs, salts and
verifiers. None of them produces an assertion or the hmac-secret output;
the verifier is one-way and domain-separated from any future wrapping key.
Credential IDs are public metadata: someone holding a key can learn that
it is enrolled here. The record is not authenticated against a writer.
Root or anyone who can write the disk can replace or remove it, and an old
copy in a snapshot, backup or restored `@var` re-admits the keys it lists.

Publication writes an exclusive (`O_EXCL`) mode-0600 temporary file in
the same directory, named `tmp-` and 32 lowercase hex digits from 16
random bytes, fsyncs it, renames it over the record and fsyncs the
directory. Removing the last key unlinks the record and fsyncs the
directory. Readers look up only the exact record name and ignore every
other entry. Before each publication or removal the worker, and firstboot
at boot, unlink every entry whose name begins with `tmp-` and no other,
so `cutover-reboot` (`td-authd/DESIGN.md`) is never touched; a prefixed
entry that cannot be unlinked, such as a directory, refuses the write. A
power cut during first enrollment therefore leaves the machine unenrolled
or completely enrolled, never locked without a record. The worker
compares the record's bytes with the baseline it presented before any
token I/O and again before publication, refusing a change: the baseline
is the record's absence or the SHA-256 of its exact bytes, and the record
must read as that again before the write starts and after the temporary
is fsynced; an unavailable state never matches. A failure before the
rename or unlink is attempted changes nothing at the record name; the
worker removes its own temporary best effort, and the next write's
cleanup removes any it leaves. Once it is attempted, any failure,
including the rename or unlink itself, the directory fsync or the
closing check that the name holds the published file or is gone, is
uncertain. An uncertain outcome
is re-read and reported, never retried. Once td-authd acknowledges a
write's commit round, the write may have begun: any outcome but the
success frame and a successful exit, a typed failure included, is
uncertain to td-authd, which re-reads the login state. The worker
starts that round only with five seconds of its deadline left, and
refuses before writing if they are gone when the acknowledgement
arrives; a success td-authd has not seen by its own deadline is
uncertain to it. The store takes no lock of its own: only the worker writes,
td-authd's single operation slot runs one worker at a time, and td-svc
kills a worker whose td-authd has died before it starts another
td-authd. `/run/td-fido/operation.lock` serializes each of the worker's
token sessions, not the operation (`td-secret/DESIGN.md`, "Login-key
worker").

## Token profile

Admission, PIN protocols, the 4-to-63-byte printable-ASCII PIN profile,
ES256, `rk=false`, refusal of signed backup flags and of enterprise
attestation follow `PORTABLE.md`'s implemented PIN, creation and proof
flows, including its fixed relying party `td.invalid`; login credentials
are separated from notebook and application-store credentials by
credential, challenge domain and salt. Creation uses fixed login display
labels and requests no credProtect. Admission also refuses a key whose
getInfo options carry `alwaysUv` with the value true; one reporting false
is admitted. A key with built-in UV, such as a biometric key, is used with
its PIN only: td never requests built-in UV. A key without a configured PIN
is refused before any creation, with an instruction to set one using
another tool, and a key that cannot hold a PIN is refused as unsupported;
td never sets, changes or resets a PIN or a key. Every
operation requires exactly one connected FIDO device and refuses none or
several before asking for a PIN, so the add and enrollment flows tell the
person to remove the authorizing or previous key before inserting the
next.

Every client-data hash is SHA-256 over `td-login/operation/v1`, a zero
byte, a phase byte, the u32-length-prefixed complete canonical description
and 32 fresh kernel-random bytes. The description is that of the step
the assertion answers, so a PIN step's hash is computed once
getPINRetries has answered and binds the count it presents. The
authorize phase also includes, before the random bytes, the
length-prefixed record ID and the length-prefixed
SHA-256 digest of the exact record bytes being changed, so the signature
itself binds an addition or removal to that record; the worker's baseline
comparison is a second check, not the binding. Length prefixes are u32.
The identify, authorize, create, prove, repeat, probe and unlock phases
are distinct, with phase bytes 1 through 7 in that order. Byte 8 is
reserved for the consent description's connect step (`td-authd/DESIGN.md`):
a presentation that sends no assertion and so has no client-data phase.
No phase may use it.

- **Identify.** Before any PIN, a silent getAssertion (`up=false`, no PIN
  and no extension) over the enrolled credential IDs, in batches no larger
  than the key's advertised list limit (one when absent), selects the slot.
  Only `CTAP2_ERR_NO_CREDENTIALS` for every batch means the key is not
  enrolled here; any other status fails the operation. The silent response
  selects only; it authenticates nothing, and the fingerprint the next
  step shows is that selection, not proof of which key is connected. This
  probe is new work: `PORTABLE.md`'s flows do not probe.
- **Unlock and authorize.** One credential, its salt, the PIN, required
  presence and UV. Verify the RP hash, UP, UV and the signature over
  authenticator data and the hash with the slot's key, then decrypt the
  hmac-secret output, derive the verifier and compare in constant time. The
  output is retired immediately; nothing is released. Counters are checked
  structurally only and not persisted, so clone detection is not claimed.
- **New key.** It begins at a connect step: the screen asks the person to
  connect the new key alone, removing the authorizing or previous key, and
  no assertion is sent until exactly one device is present, whose PIN
  retries the create step then shows. The worker waits for that, within
  the deadline, while no device or only the authorizing or previous
  key's device is connected, and refuses several. Creation's exclusion list holds
  every enrolled credential and every key already proved in this
  operation that the connected key could identify (Wire choices, below),
  and must fit the key's advertised list and size limits whole, as
  `PORTABLE.md`'s creation codec requires; a key that cannot take it is
  refused before any PIN. Then a proof, then a repeat assertion on a new
  channel that must reproduce the identical secret, each with its own PIN
  entry and touch as in `PORTABLE.md`. Right after the repeat, while that
  key is still the connected device, the worker runs the identify probe
  for the new credential and refuses the operation unless the probe finds
  it, so a credential that a later unlock could not select never enters
  the record. The probe names the excluded credentials too, in record
  order and batched as an unlock's identify is, so an addition's key
  meets exactly the batches an unlock of the new record sends. A
  two-key enrollment's primary is probed before the backup's credential
  exists, so its probe cannot include it; and no test key yet mishandles
  a foreign ID in an allow list, so that refusal path is untested. Each
  slot's salt is fresh. In a two-key enrollment the
  backup is kept distinct from the primary by this exclusion list and the
  record's refusal of duplicate credentials, not by td-authd's step
  admission, which sees only the current key's credential and an empty
  baseline.

**Wire choices.** These bytes are fixed in td-secret
(`td-secret/DESIGN.md`, "Login CTAP primitives") and pinned against the
independent `td-secret/tests/login_ctap_vectors.py`:

- Creation's labels are `td login` for the relying party's name and the
  user's name and display name, where the notebook uses `td personal
  vault`; the rest of the request is the notebook's. Requesting no
  credProtect, td admits a key that advertises it: a key whose default
  policy hides the credential from a silent assertion fails the new-key
  probe instead.
- Admission refuses, typed and in this order: a key without the
  `hmac-secret` extension, including one whose getInfo omits extensions
  or lists none; one whose `alwaysUv` is true; one that cannot hold a PIN,
  whose getInfo omits options or the `clientPin` option; and one with no
  PIN set (`clientPin` false), which another tool can set. Every other
  getInfo refusal is the notebook's and untyped.
- A key can identify a credential when its ID is within the key's
  `maxCredentialIdLength` and a silent request naming it alone is within
  the key's message size; every PIN request naming it is larger. Any
  other credential cannot be one this key enrolled or could unlock, since
  the new-key probe identifies it on the same key: identify does not send
  it, and creation leaves it out of the exclusion list.
- The list limit is getInfo's `maxCredentialCountInList`, clamped to
  eight, and one when absent. The exclusion list that remains must fit
  it, and with its exclusions the creation request the message size;
  otherwise creation refuses as the list being too small.
- Identify sends getAssertion `{1: "td.invalid", 2: hash, 3: [{"id",
  "type": "public-key"}...], 5: {"up": false}}` with one client-data hash,
  of the identify or probe phase, for every batch. Batches take the
  identifiable IDs in record order, each as many as fit both the list
  limit and the message size. A NO_CREDENTIALS answer must have no body.
  A selection must name one ID of its batch, which a one-ID batch may
  omit; UP, UV and AT must be clear, as for a silent answer; the
  signature is not checked.
- Each PIN step sends getKeyAgreement, then getPINRetries `{1: protocol,
  2: 1}` with the operation's PIN protocol, then opens the prompt. The
  reply's `pinRetries` (key 3, 0 to 255) is what td shows. Zero is
  reported as PIN blocked, whatever `powerCycleState` says; otherwise
  `powerCycleState` (key 4) true is reported as PIN auth blocked. Neither
  gets a PIN step.

Nothing is retried automatically, and cancellation kills and reaps the
worker as `td-secret/DESIGN.md` specifies. Deadlines are td-authd's.

## PIN entry, presence and retries

A PIN is typed only into the compositor's PIN field, inside an attention
lifetime the person opened with a physical Ctrl+Alt+Esc, including on the
lock surface; never into a client window, a terminal, td-pass, the VM
bridge or a console. The field's input rules, including its keymap, are
`td-compositor/DESIGN.md`'s; the enrollment screens disclose that a PIN set
on another keyboard layout may not be typable there. The PIN crosses the
paired channel to td-authd and the worker's private socket to the worker,
in clearing owners and bounded frames, once per CTAP transaction, and is
retired after its PIN-token request. It never enters a log, argv, the
environment, a file, the framebuffer, an automation capture or a
diagnostic.

**PIN memory.** The compositor holds a PIN only in its field's fixed
64-byte buffer, never in a growable string or vector and never in
formatted output, and zeroes that buffer on every way out: as the PIN is
copied into the `1c` request, on Escape, on its own cancellation, at the
attempt's deadline, at any end root reports, on a protocol violation,
when the send fails and when the buffer is dropped. The `1c` request is
written from a fixed buffer of its own, zeroed as soon as the write
returns, whether it succeeded or not, before root's answer is awaited.
The field opens only after the compositor checks its own process as
td-secret's worker checks itself: no active swap in `/proc/swaps` and a
zero core-dump soft limit in `/proc/self/limits`. A refusal cancels the
operation before any field opens. A zero `RLIMIT_CORE` does not stop a
core dump piped to a `core_pattern` handler, and td-secret's worker
shares that gap; neither check reads `core_pattern`. The keycodes of the
presses that type a PIN still pass through the input reader's ordinary
buffers, as any key press does, and each byte the keymap gives one
passes, unzeroed, through the input thread's stack as the key it hands
the field (`FieldKey::Byte` inside the dispatcher's decision).

The PIN field is drawn beneath the presented step's prompt, which stays
on the screen exactly as presented, so the person sees which operation
and which key, its fingerprint and remaining attempts, the PIN is for
while typing it. Its rows are in the prompt's font; the memory check's
refusal is a chrome row ("Failure texts" gives its width):

| When | Shows |
| --- | --- |
| root asks for the presented step's PIN | beneath the prompt, `ENTER THE PIN FOR THIS KEY`, then one mask per byte typed |
| the PIN was sent | beneath the prompt, `TOUCH YOUR KEY` |
| the memory check refuses | `PIN ENTRY NEEDS NO SWAP AND NO CORE DUMPS` |

The field never shows a typed character: it is drawn from a count. It
takes only presses made after its own paint was on glass. Where the band
the prompt leaves free beneath it cannot hold the field, the PIN step's
prompt is not presented and the operation fails (`THE OPERATION
FAILED`) before any receipt and before any PIN reaches the key, rather
than move, shrink or cover the prompt.

Every creation, proof, repeat, authorization and unlock assertion requires
user presence; after the PIN is submitted the prompt says TOUCH YOUR KEY.
Only identify and probe are silent.

The authenticator's own retry counter is the only guess limit. td keeps no
counter of its own: one on an unencrypted disk could be reset by anyone
who can write it and would only add lockout. Before every PIN step td
queries getPINRetries and shows `N PIN ATTEMPTS LEFT ON THIS KEY` as an
untrusted device claim. A key that reports none left gets no PIN step: the
worker reports it as PIN blocked, and td-authd refuses a PIN step whose
retries are zero. Typed CTAP statuses, never diagnostic text, select what
follows:

| status | td shows | then |
| --- | --- | --- |
| PIN invalid | WRONG PIN | the operation ends |
| PIN auth blocked | REMOVE AND REINSERT THIS KEY | no PIN until power-cycled |
| PIN blocked | PIN BLOCKED; USE ANOTHER KEY | only a reset, which erases it |
| any other | THE OPERATION FAILED | no retry |

A reset erases every credential on the key, its login credential
included. td never infers a remaining count from a status. Every new
attempt is a new operation from a new chord with a new nonce. A blocked
key stays in the record until another key removes it.

## Failure texts

An operation that ends without success shows one text on the trusted
screen, chosen by td-authd's typed kind and detail
(`td-authd/DESIGN.md`, "Login-key operation supervision"), never by
diagnostic text. These texts are the compositor's chrome rows, not a
trusted prompt's: its 5x7 font from 24 pixels in, doubled only on an
output of at least 800x600 as a prompt's is, so even the smallest output
a prompt takes, 320 pixels wide, gives a row 45 columns. Every text here
fits that whole, the longer ones as two rows, split here by `/`;
the compositor wraps any wider chrome row at a space rather than
clip it (`td-compositor/DESIGN.md`, "Login-key operations"). The PIN
table above uses the first three and `0f`'s.

| Kind | Detail | Shows |
| --- | --- | --- |
| `01` WRONG PIN | any | `WRONG PIN`, without the count |
| `02` PIN AUTH BLOCKED | | `REMOVE AND REINSERT THIS KEY` |
| `03` PIN BLOCKED | | `PIN BLOCKED; USE ANOTHER KEY` |
| `04` NOT ENROLLED | | `THIS KEY IS NOT ENROLLED HERE` |
| `05` ONE KEY | | `CONNECT EXACTLY ONE KEY` |
| `06` KEY REFUSED | `01` | `KEY HAS NO HMAC-SECRET` |
| | `02` | `KEY ALWAYS REQUIRES UV` |
| | `03` | `KEY CANNOT HOLD A PIN` |
| | `04` | `SET A PIN WITH ANOTHER TOOL` |
| | `05` | `KEY CANNOT LIST ENROLLED KEYS` |
| | `06` | `KEY CANNOT BE SELECTED` |
| `07` DENIED | | `TOUCH DENIED` |
| `08` TIMEOUT | | `TIMED OUT` |
| `09` NO RECORD | | `NO LOGIN KEYS ENROLLED` |
| `0a` DIRECTORY DAMAGED | | `LOGIN KEY STATE UNAVAILABLE:` / `DIRECTORY DAMAGED` |
| `0b` RECORD DAMAGED | | `LOGIN KEY STATE UNAVAILABLE:` / `RECORD DAMAGED` |
| `0c` STATE COULD NOT BE READ | | `LOGIN KEY STATE UNAVAILABLE:` / `STATE COULD NOT BE READ` |
| `0d` RECORD CHANGED | | `KEYS CHANGED; NOTHING WRITTEN` |
| `0e` UNCERTAIN | `01` to `05` | `RESULT UNCERTAIN` |
| `0f` FAILED, `10` INTERNAL | | `THE OPERATION FAILED` |
| `11` EXCLUDED | | `KEY ALREADY ENROLLED` |
| `12` VERSION | | `A RETAINED SYSTEM CANNOT READ KEYS` |
| `80` cancelled | | nothing |
| `81` eight keys | | `EIGHT KEYS ALREADY ENROLLED` |
| `82` already enrolled | | `LOGIN KEYS ALREADY ENROLLED` |
| `83` slots not the record's | | `KEY LIST CHANGED; REOPEN` |

Kinds `0a` to `0c` show the unavailable state's own texts ("The login
record"), split after the colon. A cancellation shows nothing new: the
person chose it, or the compositor did and said why, withdrawing any
prompt: `TIMED OUT` when the attempt's own deadline passed, `THE
OPERATION FAILED` when a presentation did (a PIN step with no room for
its field is not presented), or the paint that opens a PIN field or
shows its touch request, and the memory check's text when
it refused a PIN field ("PIN entry, presence and retries"). An end
td-authd
reports uncertain (`91 0e`, only after a write's acknowledged commit
round) shows `RESULT UNCERTAIN` above its kind's rows, or alone for kind
`0e`; nothing is retried, and from increment 4 the screen re-reads the
state through `1a`. Any other kind
or detail, or an end td-authd could not have reported, is a protocol
violation that ends the paired generation: an uncertain end before such
a commit or a certain one after it; kind `0e` in a certain end; `80`
before the compositor's own `15`; `81`, `82` or `83` except for an
addition, a first enrollment or a removal respectively, before any
description (td-authd refuses those at the worker's baseline); and an
end without a description once one was given, or after a commit with
another than the committed step's. td-authd's refusal in this build
(`9b 00`), and a selection the compositor cannot yet serve, show `NOT
AVAILABLE IN THIS BUILD`. Success shows `SESSION UNLOCKED`, `LOGIN KEYS
ENROLLED`, `LOGIN KEY ADDED` or `LOGIN KEYS REMOVED`.

## Session lock

The lock exists only for an enrolled account; on an unenrolled one the
triggers below change nothing (`td-compositor/DESIGN.md` says what each
shows). The compositor learns the login state as `td-authd/DESIGN.md`
specifies. Enrolled and unavailable both start locked; unavailable shows
its cause's text ("The login record") and offers no unlock.

The session locks at every compositor generation start (boot, and any
compositor or authority restart or crash), on `Super+l`, on the attention
screen's `L`, on a lid close and on resume from any suspend, however it
began, that lasted longer than the compositor's two-second threshold plus
the kernel's one-second RTC granularity for measuring sleep on x86
without a TSC that runs in S3. A shorter suspend can go unnoticed. The
compositor's detection of each trigger is in its design. td has
no suspend initiator and this tier adds none. Any initiator that lands
later must, for an enrolled account, obtain the lock surface's
presentation receipt before writing `/sys/power/state` and refuse to
suspend when locking fails. Hibernation stays disabled.

Locking cancels an open attention lifetime under the existing pre-commit
cancellation rules, closes overlays, withdraws focus and grabs, and paints
the lock surface over the whole output. Until that paint has a
presentation receipt, no client receives input.

While locked:

- The output shows only the lock surface: hostname, username, `LOCKED` and
  `PRESS CTRL+ALT+ESC TO UNLOCK`. No client pixel, cursor, title or
  workspace bar is shown.
- Applications keep running. They receive no focus and no input from any
  source; new windows map behind the lock. Frame callbacks may be
  throttled. Audio and network continue.
- Ordinary bindings (terminal, launcher, help, workspaces) are suppressed.
  Control-socket and automation requests that inject input or capture output
  refuse; bridge clipboard snapshots already require keyboard focus.
- Queued credential writes and installations stay unselectable; their own
  intake deadlines apply.

The lock protects the interactive surfaces, not running processes, as
`td-install/ENCRYPTION.md` says of any locked running machine.

On the lock surface Ctrl+Alt+Esc opens a login-unlock attention lifetime
directly, without the menu. The prompt presents the unlock description; the
worker identifies the connected key, which must already be the only one
connected (the worker does not wait for it); the next presented step
names its fingerprint and remaining PIN attempts and opens the PIN field;
after the PIN comes the touch. The worker compares the record with its
baseline before identify and again before its success. td-authd reports
success only after the worker's success frame and observed exit, and the
compositor then drains held input and restores the ordinary screen and
focus. A failure shows its typed reason on the trusted screen; Escape
returns to the lock surface. One
unlock is allowed per attention lifetime. Unlocking starts no process,
switches no credential and releases no secret.

## Locked boot

With an enrolled account, td-svc's order is unchanged: the compositor and
authority pair starts and the first frame is the lock surface. The human
session units (the terminal and the applications) start behind the lock as
they do today, and their windows map behind it. Running the account's
processes before authentication discloses nothing on an unencrypted disk
and grants no interactive access, and it keeps boot health, which requires
the terminal, unchanged. The follow-up that releases the secret store at
login, and the protected tier's "one fresh session" rule, must revisit
this and may hold session start until the first unlock.

The serial greeter's console login, and every other interactive td-login
path, is refused as `THREAT-MODEL.md` §3 specifies.

## SSH

The primary account logs in over SSH only with the boot self-test's
volatile key, from 127.0.0.1: the root-owned
`/run/td-ssh-selftest-authorized_keys` line is `restrict,from="127.0.0.1"`,
and its private key is readable by UID 1000 itself, so it grants nothing
the session lacks and nothing off the machine. `/etc/bootsuccess`
generates both on every boot (`build_bootsuccess` in
`recipes/src/recipes/system-x86-64.rs`), and the policy's `Match` block
names that file (`td-firstboot/src/ssh_policy.rs`). Root and non-primary
accounts use the persistent root-owned `/etc/ssh/authorized_keys`, empty
on a fresh install.

In the enrolled and unavailable states the rendered policy takes its
**enforced form**: `PermitRootLogin no`, and no account but the primary is
admitted. SSH then opens no session without a login key except that
self-test. Only a verifiably unenrolled state renders the ordinary policy.
Firstboot renders the form that matches the login state at every boot,
before `sshd` starts; the cutover renders it within a boot. The QEMU
persistent-administrator fixture therefore runs only on unenrolled images.

## Cutover

Whenever the login state moves between unenrolled and enforced (enrolled
or unavailable), by an operation or otherwise, td revokes every
interactive session that the other state allowed: a serial session the
greeter started before enrollment would otherwise keep a UID-1000 shell on
`ttyS0`, and an SSH session for root or another account would outlive the
policy that admitted it. td-authd renders the SSH policy for the new
state, restarts `sshd` and `greeter` through td-svc, and reports
completion only after observing that each old containment is empty and a
new instance runs; its exact mechanism, deadline, reconciliation, notice
and failure handling are `td-authd/DESIGN.md`'s revocation amendment.

Revocation is reconciled from durable state, not from the operation that
changed it. Firstboot renders the policy for the login state at every boot
before `sshd` and the greeter start, and td-authd repeats the check after
every login operation, including a failed or uncertain one, at every
Prepare, and when it starts. A worker or authority death between
publication and restart therefore still converges. A revocation that
cannot be observed complete is a failure that ends in one orderly reboot,
whose boot ordering enforces the boundary; td-authd's guard keeps a
persistent failure from looping.

The restarted greeter re-enters `login-primary`, which refuses and returns
the terminal to root (`THREAT-MODEL.md` §3). The greeter's `tty=`
containment covers its leader and every process whose controlling terminal
is that device (`td-svc/DESIGN.md`, "Stopping"). A process that holds a
descriptor to the line without it being its controlling terminal, because
it opened the line that way or called `setsid()` and kept an inherited
descriptor, survives; it exists only through the account's own earlier
act, or a compromise, before enrollment. `sshd`'s unit stops its whole
service leaf (`stop=leaf`, `td-svc/DESIGN.md`), so OpenSSH sessions in
their own process groups end too.

Removing the last key reverses the cutover, so console login and the
ordinary SSH policy return at once. The graphical session that performed
the change stays unlocked; td-authd's next login-state answer makes the
lock available or unavailable at once.

## Deployments

A deployment carries the tier when its manifest holds a fixed tier marker,
covered by the deployment ID, naming the record versions it reads. A
deployment without the marker ignores the record, so booting it restores
automatic login. Therefore:

- enrollment refuses unless both `current` and `previous` carry the marker
  and meet the record-version rule ("The login record", Versions);
- on an enrolled or unavailable machine, td-authd's update consent keeps
  a deployment that cannot read the record from becoming `current` or
  `previous` through td's own path; its update-consent amendment in
  `td-authd/DESIGN.md` owns the exact rule.

A deployment that carries the marker never stops honouring the record.

## Enrollment, addition and removal

Key management opens with `K` on the attention screen of an unlocked
session. The screen numbers enrolled keys 1 to N in the record's
canonical slot order and shows each one's fingerprint, the first four
bytes of the credential ID's SHA-256 as in `PORTABLE.md`, and the cap of
eight. The configured human is the only principal; td-authd refuses any
other.

That list is td-authd's: its enrolled `1a` answer carries the slot
count and each slot's fingerprint in canonical order, as `inspect-login`
reports them, and every `1a` answer carries the validated primary
username (`td-authd/DESIGN.md`, amendment 1). Increment 4 implements
it. Until then the compositor has no list, so `D` shows `NOT AVAILABLE
IN THIS BUILD` and sends nothing; its digits and request are tested
against a list the tests supply.

**First enrollment** applies only to an unenrolled account and is
authorized by the physical selection alone: no key exists yet, and a person
at the automatically logged-in session holds the account under §L.1's
scope. The person chooses:

- `2`, two keys: the primary, then a backup created with the primary
  excluded. td asks for a separate physical key but cannot prove distinct
  hardware.
- `1`, one key.

Either choice first shows an enrollment disclosure, confirmed by a fresh
physical Enter under `td-compositor/DESIGN.md`, "Physical installation
confirmation". It states that from then on the machine boots and locks to
a screen only an enrolled key and its PIN open; that open console and SSH
sessions end now, and root and non-primary SSH, including persistent
administrator keys, are refused from then on; that administration then
needs a login; and that a PIN set on another keyboard layout may not be
typable here. The one-key choice adds that losing this key, or blocking
its PIN, leaves no way to log in, locally or over SSH; that recovery then
needs someone who can start other code on this machine (a firmware boot
menu or UEFI shell suffices on an unencrypted disk), the same access that
bypasses the lock; and that a backup key can be added later.

Every key is created, proved, repeated and probed before anything is
published; publication then makes the cutover above.

**Adding a key** (`A`) refuses at eight keys before any token. One enrolled
key and its PIN authorize it with an authorize-phase assertion. At a
connect step the person then removes that key and connects only the new
one, which is created with every enrolled credential excluded, proved,
repeated and probed, and the record is published against the unchanged
baseline.

**Removing keys** (`D`) takes a nonempty set selected by digit. The
description names each selected key by its position in the record's
canonical slot order, which the digits show, and its fingerprint, so two
keys that share a fingerprint stay distinct. One
enrolled key and its PIN authorize it, and that key may be in the set.
Leaving exactly one key requires the one-key disclosure. Removing every key
requires a disclosure that the machine will log in without a key, confirmed
the same way; the record is then unlinked and the cutover restores console
login and the ordinary SSH policy.

Every enrolled key with its PIN holds full authority: whoever holds one key
and its PIN can remove the others and add their own. The remedies are to
keep each key apart from its PIN and to remove a lost key promptly with
another. There is no other recovery-policy setting; adding and removing
keys is how it changes. After an uncertain result the screen re-reads the
state and retries nothing. These requirements bind td's operations; root
can rewrite the record regardless.

## Recovery

With one key lost and another enrolled, unlock with the other, remove the
lost key and add a replacement.

With every key lost or every PIN blocked, no td path logs in, locally or
over SSH, and §L.1 elevation needs an unlocked session. There is no
password, recovery code or fallback; recovery is physical only, and that
is deliberate. On an unencrypted volume, anyone who can start other code
on the machine (a firmware boot menu, a UEFI shell, another OS, or td's
live medium, whose session offers a terminal) can mount `@var` and delete
`/var/lib/td/login/1000`, which restores automatic login. The same access
repairs the unavailable state. A damaged record is removed the same way,
which also restores automatic login, or replaced by an intact copy of the
same machine's record. A damaged directory is removed, or restored as a
root:root mode-0700 directory, and the next boot's firstboot recreates or
accepts it. That is the owner's recovery and the bypass named in Scope.
On a device-bound volume the live medium cannot unseal, because it caps
PCR 12; the volume's recovery key opens it from the live medium for the
same repair. The future protected tier has its own recovery.

A firmware supervisor password and boot-order lock narrow the physical
path on hardware that has them; td makes no claim about them. The record
is not backed up, and a key is not a backup of anything.

## Administration

A key and its PIN are required only to unlock and to add or remove keys,
which is how recovery policy changes. Ordinary elevation is the §L.1
consent-only mechanism, which an enrolled machine always has ("Enrollment
requires §L.1 elevation", above, owns that prerequisite). On such a
machine `su` is no administrative path and root has no empty shadow
field, interactive root login is refused
(`THREAT-MODEL.md` §3) and root SSH is refused (SSH). Root remains
reachable only through root-owned services and physical access, and root
can remove or rewrite the record, so a change made as root bypasses the
key requirement. Software in an unlocked session acts as UID 1000 with all
of that account's data, which needs no root (§L.1 scope).

## Other contracts

**Disk encryption.** The device-bound tier and this one each keep their own
scope; `td-install/ENCRYPTION.md`, "Device-bound default", owns the rule
that their combination is not lost-laptop protection. Login records are
not disk protectors and never become ones; activating the protected tier
decides how its FIDO2 protectors relate to these keys, and its stricter
second-token rule governs disk protectors.

**Secret store.** Application-store enrollment, release and writes
(`APPLICATIONS.md` §W.4) are unchanged and use their own TPM-bound
credentials. Logging in releases nothing. Releasing the store at login is
a follow-up, specified separately. It must account for the slot salts being
public on an unencrypted disk: anyone who later holds a key and its PIN can
recompute a wrapping key derived only from the hmac-secret output, and
then open any earlier copy of the store, so it needs rotation or a further
secret.

**Portable notebook.** td-pass credentials are separate. One physical key
can hold login, notebook and application-store credentials; each
enrollment creates its own.

## Evidence

The test-only virtual authenticator, created through UHID (`CONFIG_UHID` is
already pinned) and absent from the shipping image, needs no TPM. It
implements:

- getInfo advertising clientPIN set, pinUvAuthToken, hmac-secret and a list
  limit, with variants carrying `alwaysUv` true and false, credProtect, and
  a list limit too small for the exclusion list;
- clientPIN getPINRetries, getKeyAgreement,
  getPinUvAuthTokenUsingPinWithPermissions and legacy getPinToken, for both
  PIN protocols;
- makeCredential with exclusion lists, and getAssertion in silent and
  PIN-authorized modes with distinct UV and non-UV hmac-secret outputs;
- a retry counter starting at eight, the three-consecutive-failure block
  until simulated reinsertion, and a blocked state at zero;
- scripted presence delay and denial, keepalives, a wrong secret, a
  signature over other data and a replay over an earlier assertion's
  client-data hash, and a configuration changed between operations.

It signs with test-only ECDSA over td-secret's private P-256 arithmetic;
host tests check its signatures and PIN-protocol messages against the
existing committed independent vectors. Its state persists on the
disposable disk, saved before each reply that changed it, so a key
keeps its credentials across cold boots, hard kills included; a damaged
state file is refused by type, never loaded. The
authenticator logic is a test-only core (`td-secret/DESIGN.md`,
"Virtual authenticator") that host tests drive in process and that a
test-only UHID binding presents in a guest as a hidraw FIDO device
speaking CTAPHID, with keepalives within the 100 ms ceiling while the
key works, UPNEEDED during a scripted touch ("UHID binding").

Eleven `qemu-secret` guests, none with a TPM, run the root worker's own
operation over the production discovery, Session and HID worker with
its operation lock, against those devices, with simulated root
acknowledgements and both retained deployments taken to read this
build's record version (`td-secret/DESIGN.md`, "Login-key worker
guests"): unlock with each key, wrong PIN with a falling count, PIN AUTH
BLOCKED cleared only by reinsertion, PIN BLOCKED, a key not in the
record, no key and two keys; one-key enrollment whose record a fresh
worker then unlocks, and removal of the last key; two-key enrollment
across a device swap; addition across a swap and removal of the
authorizing key; additions to eight across swaps, each key then
unlocking, and a ninth refused before any token; a slow touch held by
keepalives; a credProtect default refused at the probe; `alwaysUv` true
refused before any PIN at enrollment and at an enrolled key's unlock,
and false admitted; presence denied at a
creation and at an unlock, each DENIED after its PIN; a list too small
for the exclusions refused before any PIN; a tampered verifier and a
tampered public key, each written to the production record, a
signature over other data and a replayed assertion, the current data
signed over an earlier unlock's client-data hash, each FAILED, the last
because each challenge is fresh; a record changed after the
baseline, before any token I/O and at the commit round, RECORD CHANGED
with no write attempted; and an incompatible record version refused
before any token, while removing every key under no shared version
still unlinks the record. Host tests of the worker's operation over
in-process virtual keys cover the same cases but the keepalives, which
only a HID device sends.

A twelfth TPM-free guest, `login-powercut` (`td-secret/DESIGN.md`,
"Login power-cut guests"), boots twelve times on one disposable disk
whose Btrfs `@var` holds the record and both persistent keys. Ten boots
each make one write through the worker and are killed by the host, a
SIGKILL of QEMU, at a store stage inside it: first enrollment at the
temporary's creation, its sync and the rename, an addition at the
write, just before the rename and after the directory sync, and the
removal of every key before the unlink, after it and after the
directory sync; one more after an enrollment's success. Each next boot
finds the old record or the whole new one, as that stage requires,
never an unavailable state; an unlink done or not; a synced temporary
holding the whole new record, ignored by the read and removed by the
next write; and a record written in an earlier boot unlocked by each
persisted key it lists, a key it does not list being NOT ENROLLED. A
cut during first enrollment therefore left the machine unenrolled or
completely enrolled. Btrfs is mounted so that only a sync makes a
change durable within a boot, and the cuts just after the rename and
the unlink, before the directory sync, must lose them, while the same
writes cut after that sync must keep them: that is how a missing
directory sync would be caught. Those two controls rest on stated
premises, that nothing syncs `@var` between the change and the kill,
which the fixture enforces for its own keys, and that the pinned
kernel's rename and unlink do not sync the Btrfs log, so a red there
after a kernel bump is a premise to investigate before a store
regression. The temporary's sync is tested too, since the synced
temporary must survive whole; what cannot be shown on Btrfs is that a
published record would be torn without it. A QEMU kill keeps the host's
page cache, so these guests prove crash consistency of the store's
syncs, not that a sync reached stable media: a missing write-cache
flush, torn or reordered sectors and host power loss are not covered,
and I/O in flight at the kill may land or not. Firstboot's own
temporary cleanup is increment 4's.

The stock VM stays unenrolled: no recipe or firstboot path writes a record,
and its valid, empty directory is decided unenrolled without any helper,
so this tier leaves the existing serial-console and SSH oracles unchanged.
The §L.1 increment's retirement of root's empty shadow field may change
them on its own account.

QEMU proves protocol composition, td's state machine, the refusal paths,
the trusted input and display path, relock on crash and resume, console
and SSH revocation and publication. It does not prove a USB controller,
YubiKey firmware interoperability (PIN protocol, credential-ID length, list
limit, silent assertion, UV-protected hmac-secret), real touch timing
within the deadlines, a real retry counter, a real lid switch, real
laptop suspend or a real OTP interface. Before the tier is described as
usable, record a ThinkPad T430s with two YubiKeys (model and firmware),
exercised on both its xHCI and EHCI ports, including a lid close, a
suspend and resume, and a touch of the OTP interface while the attention
screen is open. No test enrolls, resets or changes the PIN of an
operator's device.

## Increments

Each is independently landable. None is expected to need new `unsafe`;
one that turns out to amends `UNSAFE.md` in the same landing. Increments
2 to 4 land inert, except increment 3's exclusion of a security key's
own keyboard from secure attention, which is live on every machine with
or without a record: it denies a security key's own keyboard every
selection and confirmation, and the only selection it newly admits is
another keyboard's fresh press of a key the security key's keyboard
holds. No production path reads the login state until increment 4, which
also makes firstboot ensure the directory, so its absence, itself a
damaged directory, never meets a production reader. From then on, that
exclusion apart, behaviour changes only when a record or an invalid
directory exists, which no production path creates, and the state is
decided by the directory-and-name check before any helper runs, so no
helper failure can change an unenrolled machine. Each lists what it
proves and the oracle that shows it.

1. **This amendment.** Documentation only.
2. **Protector backend, virtual authenticator and authority:** the record
   codec and versioning, the root `login-operation` worker for enroll, add,
   remove and unlock, the identify and new-key probes, the consent
   operations, and td-authd's supervision with requests `1b` and `1c`,
   which refuse enrollment, addition and removal in production.
   - Host tests: literal vectors for the record, verifier, client-data
     hashes (including the authorize binding) and consent descriptions;
     every codec refusal; the virtual authenticator against the committed
     vectors; host child fixtures and the root session fixture driving
     `1b` and `1c` through the real paired Session.
   - `qemu-secret`-style worker guests with simulated acknowledgements: both
     enrollment choices, addition to eight and refusal of a ninth, removal,
     unlock with each key; wrong PIN with a falling count, auth-blocked,
     blocked, unenrolled key, no key, two keys, denied presence, `alwaysUv`
     true refused and false admitted, credProtect and too-small list
     refusals, tampered verifier or public key, replayed assertion (a
     signature over an earlier unlock's challenge), changed baseline,
     incompatible record version; guest power cuts before and
     after publication leaving one whole record and no unenrolled machine
     locked; leftover temporaries ignored and removed. Twelve guests
     covering every case above have landed ("Evidence"), the power cuts
     and temporaries among them.
   - td-authd's supervision with `1b` and `1c` has landed: host child
     fixtures over scripted workers, and the root session fixture, the
     `supervise-login` authority case of `qemu-secret`, against the
     production worker's refusals. This increment is complete.
3. **Compositor trusted PIN entry and lock surface**, driven by fixtures
   and not yet activated by a record; the compositor sends `1b` and `1c`.
   - Native compositor and device-dispatcher tests: lock rendering excludes
     client pixels and cursors; the PIN field takes only physical keys,
     handles Shift, and refuses injected, automation, control and bridge
     input; the OTP-keyboard exclusion from every attention selection,
     confirmation and the PIN field over hand-built sysfs trees (UHID devices
     have no USB parent, so the guest cannot show it); the K, A, D, 1 and 2
     selections against a scripted authority, and their refusal by the
     production one; the chained login lifetime.
   - A desktop guest with a UHID keyboard drives PIN entry against the
     worker with a seeded record, with framebuffer bitmap checks.
   - The OTP-keyboard exclusion has landed for the existing selections and
     Enter confirmation: host tests over hand-built sysfs trees (a
     composite key, a plain keyboard, a keyboard behind a hub beside a key, nodes
     with no USB parent, a FIDO page named only by extended usage ranges,
     truncated, oversized, long-item and empty descriptors)
     and device-dispatcher tests. The PIN field and the `K` selections
     must read through the same exclusion when they land. The hardware
     record of an OTP touch on the attention screen ("Evidence") is still
     owed.
   - The private client's login-key operations have landed, with the
     `K` screen's `1`, `2`, `A` and `D` selections read through that
     exclusion and one operation per attention lifetime: `1b` for each
     operation; each description admitted by consent's `login_start` and
     `login_successor` after its predecessor's receipt, with the
     addition's authorizing key never named by its new key; the commit
     only after the last step; the operation's ceiling as the attempt's
     lifetime; and the failure texts. Host tests drive every operation's
     status sequence, its refusals, cancellation at each stage and every
     end kind against a scripted authority, and device-dispatcher tests
     the selections, their refusal outside attention and from an
     excluded keyboard. Production root refuses `1`, `2` and `A`, and
     `D` is refused locally until increment 4's key list.
   - The PIN field has landed (`td-compositor/DESIGN.md`, "The PIN
     field"): root's `0c` for the presented PIN step opens it once the
     compositor's own memory check passes; it takes fresh presses from
     devices secure attention reads, made after its own paint was on
     glass, through the `us` keymap with Shift, 4 to 63 printable bytes,
     shows only masks beneath the step's prompt, which stays as
     presented, and Enter sends `1c` from a buffer zeroed as soon as it
     is written, with Escape before the final check sending nothing; a PIN
     step with no room for its field is never presented; the client
     admits a PIN step's successor or commit only after its own `1c`.
     Host tests drive the
     whole status sequence of every operation with its `0c` and `1c`,
     the zeroing on each way out, the deadline while typing, the memory
     check's refusal and a wrong PIN against a scripted authority, and
     device-dispatcher tests the keymap, repeats, the bounds and the
     excluded keyboard. It is inert: production root refuses every
     operation that would reach a PIN step. The lock surface follows.
4. **Locked boot and session lock:** request `1a` and login state at
   Prepare, with the enrolled key list and the primary username,
   `Super+l`, attention `L`, lid close, resume detection, unlock
   end to end, firstboot's directory and temporaries, the unavailable
   state, firstboot's boot-time render of the enforced SSH form for the
   enrolled and unavailable states, td-login's console refusal with
   terminal return and greeter hold, removal of `build_autologin`'s
   `login -f` fallback, the tier marker and td-authd's update-consent
   refusal, all atomically. Fixtures seed records through the backend.
   - Full-system QEMU on a disposable volume: a cold boot whose first frame
     is the lock surface, whose serial greeter prints the exact refusal line
     and starts no shell, and which each seeded key unlocks; `Super+l` and
     `L`, unlock, relock after killing the compositor; suspend to RAM in
     a machine with S3 enabled, held suspended at least 10 seconds and
     woken over QMP `system_wakeup`, whose first routed input after resume
     reaches the lock surface and no client; the unavailable state, with
     its screen text and the enforced SSH form, for a wrong-mode,
     wrong-owner and non-directory `/var/lib/td/login` and for a record
     with the wrong mode, two links, truncated bytes or an unknown version,
     each repaired by the documented recovery and followed by a normal
     boot; with the `inspect-login` helper made to fail, an unenrolled
     image that boots unlocked with the ordinary SSH policy, and a seeded
     record that shows `STATE COULD NOT BE READ` and resolves to enrolled
     once the helper answers; refusal to install a marker-less deployment
     and a marked deployment that does not read the record's version.
   - Compositor input tests replay recorded `SW_LID` events and clock gaps
     through the adapter; QEMU has no lid.
   - `system_def_is_self_consistent` requires the autologin account to be
     the primary account.

**Prerequisite:** the §L.1 elevation increment lands next, as "Enrollment
requires §L.1 elevation" above requires.

5. **Activation:** enrollment UI and automatic-login cutover, the
   in-boot SSH render (`td-firstboot render-ssh-policy`), the `stop=leaf`
   key and `sshd`'s use of it (`td-svc/DESIGN.md`), the `K` screens, the
   disclosures, addition and removal, the deployment-marker check on
   enrollment, and revocation with its reboot guard. It lands only after
   the prerequisite, and its oracle uses the §L.1 operations; this
   activates the tier.
   - Full-system QEMU: both enrollment choices through the attention
     screen; enrollment in a boot whose serial session was logged in and
     runs a process that ignores TERM, and in which a root SSH session is
     open, followed by observation that both sessions and every process in
     their containments are gone, the greeter refuses and root SSH is
     refused; a cold reboot that is locked with the enforced SSH form;
     additions to eight; removal disclosures; removal of every key
     followed by immediate serial login and an unlocked next boot;
     enrollment refused while `previous` lacks the marker.
   - Failure injection: the authority killed between publication and
     restart, after which the next generation's check completes the
     revocation; a scripted td-svc that answers once transiently and then
     succeeds, one that refuses, and one whose containment never empties,
     the last two ending in the reboot request, a failure at the first
     check of the boot after an automatic reboot, which holds, and an
     unwritable reboot guard, which also holds (host child fixtures); an
     absent `/run/td-login-cutover`; an unenrolled image whose
     `inspect-login` helper is made to fail, which triggers no cutover;
     and in the guest a
     cut followed by a boot whose firstboot renders the enforced form
     before `sshd` and the greeter start.

Hardware evidence on the T430s follows increment 5 and precedes any claim
that the tier is usable. Follow-ups: store release at login, idle-timeout
lock, a suspend initiator, built-in UV and PIN setup.
