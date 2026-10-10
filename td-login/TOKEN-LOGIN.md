# Login keys and session lock

This is the normative target for td's login-key tier. td-authd,
td-secret, td-compositor and td-login implement it together; this
document owns the tier's rules, and each component document states the
amendments its own contract needs. **Only the record codec, the record
store, the consent descriptions, the CTAP login primitives, the root
worker and td-authd's supervision of it and the compositor's client of
that supervision are implemented**, inert: td-secret's private
`login-operation` worker unlocks, enrolls, adds and removes keys against
the record ("Placement"), and td-authd starts and drives it on the
paired requests `1b` and `1c`. The compositor's key-management screen
sends `1b` for a first enrollment, an addition and a removal from the
key list of td-authd's `1a` answer, each of which a production td-authd
refuses before starting anything; without a list it refuses removal
itself. Only its lock surface's chord sends an unlock's `1b`, and only
while the state is enrolled; production locks only at a generation's
start, on `Super+l`, on the attention menu's `L`, on a lid close and on
a resume (increment 4's C7, C9 and C10, below). Its PIN field sends `1c`
only when root asks for a PIN at a presented PIN step, which only that
unlock reaches. So nothing in production starts the worker or uses its
`login_record` and `login_store` modules ("The login record"), its login
identify, PIN-retry, assertion and creation steps ("Token profile"), or
td-authd's login consent operations, step admission and supervision
(`td-authd/DESIGN.md`, "Login-key operation supervision"), except that
unlock on a machine that holds a record, which nothing in production
writes. The worker reads both retained deployments' tier markers before
a write that would leave a record ("Versions"), which production
td-authd still refuses before starting it. Every deployment built from
increment 4's C10b onward carries the marker ("Deployments"), so it
reads the record versions of each retained deployment that carries it;
one built before C10b carries none and reads no version. Its
`qemu-secret` guests, which have no volume and so take both retained
deployments to read this build's version in the marker's place, run it
over UHID virtual keys through every case increment 2 lists, including
power cuts inside its writes on a disposable disk ("Evidence"):
increment 2 is complete. Of increment 3, the compositor's exclusion of a
security key's own keyboard from secure attention has landed, and it is
live: it narrows the existing attention selections and confirmation and
needs no record (`td-compositor/DESIGN.md`, "Physical secure
attention"). The private client's login-key operations have landed too
(`td-compositor/DESIGN.md`, "Login-key operations"): the `K` screen,
each operation's descriptions checked step by step, its commit and the
failure texts ("Failure texts"). Production reaches only their refusals.
The PIN field has landed as well ("PIN entry, presence and retries"),
reached only by that unlock, and so has the lock surface with its login
unlock ("Session lock"), which C7 made live. Increment 3 is complete;
its desktop guest moved to increment 4 as `login-desktop`. Increment 4
is specified as twelve commits, C1 to C11 and C10b ("Increments"), all
of which have landed, so increment 4 is complete: firstboot ensures the login
directory at every boot, the live medium's included, through the shared
login-state predicate, and rootcheck reports it on a marker of its own;
td-authd answers request `1a` with the login state, through that
predicate and the read-only `inspect-login` helper; the compositor asks
it and uses its key list for `D`; the tier marker's grammar, placement
and readers are specified ("Deployments") and implemented: the worker
reads its write version from the retained deployments' markers, and on
an enrolled or unavailable machine td-authd's request 19 refuses a
queued deployment that cannot read the record, which the compositor
shows. Every deployment built from C10b, the commit after C10 that
completes increment 4's enforcement, carries the marker, since the
marker claims that a deployment honours the record at every entry point.
On an enrolled or unavailable machine, request 19 therefore admits a
marked deployment that reads the record's version (any marked one while
the record cannot be read) and refuses one built before C10b, which
carries none; nothing enrolls before increment 5. td-login's interactive
`login` and `login-primary` refuse the console there, returning the line
to root and parking with one fixed line (`THREAT-MODEL.md` §3), while
the image logs in only through `login-primary`; and firstboot's
boot-time SSH render takes the enforced form there ("SSH"). The
compositor starts every generation locked there too ("Session lock"):
its first frame is the lock surface, with the answer's hostname and
username above the state's rows, and on it the chord sends an unlock's
`1b` only while the state is enrolled. The `login-desktop` guest shows
that start and the unlock through the production authority and worker in
QEMU ("Evidence"). `Super+l`, the attention menu's `L`, a lid close and
a resume lock an enrolled or unavailable session too, and a lock while
an attention lifetime is open ends it as Escape does ("Session lock").
The `qemu-login-system` guest shows on the full system image the three
refusals, the locked start, and an enrolled session's `Super+l`, `L` and
killed-compositor locks. It does not show a lid, `Super+l` or `L` on an
unavailable session, or request 19's admission while the state could
not be read; and after QEMU's S3, whose wake leaves the virtio-gpu card
dead, it shows only that the session was locked when the first
post-wake input was routed, in the same generation, not the lock
surface ("Evidence").
Those three refusals, the locked start and those four locks are all that
act on the state, and only where a record or an invalid directory
exists, but for increment 4's live exceptions ("Increments"): on every
paired machine `Super+l` is consumed and the menu shows `L`, which
answers `NO LOGIN KEYS ENROLLED` while unenrolled. Of increment 5, A1
to A3 have landed and are live on every machine without acting on the
state: `sshd`'s unit stops its whole leaf (`stop=leaf`,
`td-svc/DESIGN.md` §4), so a stop, restart or shutdown of `sshd` ends
its OpenSSH sessions; at every boot firstboot publishes its SSH render
by rename and writes the cutover record (`/run/td-login-cutover`,
`td-authd/DESIGN.md` amendment 7) naming the form it rendered; and
td-authd checks that record at every generation's first `1a` and after
every login operation. Where the reduced state differs it cuts over:
`td-firstboot render-ssh-policy`, the greeter's line handed back,
`sshd` and `greeter` restarted and the record written, or on failure
the guarded reboot. On a stock machine the record names the unenrolled
state, so the check changes nothing; only damage to the directory or
the record within a boot cuts over. The compositor polls `1a` while a
revocation is pending and shows a failure's notice on its lock surface
and attention screen. Nothing else below is implemented. Until
the increments at the end land, `THREAT-MODEL.md` §3 is the complete
current behaviour: the installed account logs in automatically, and a
machine with neither a record nor an invalid directory, as every stock
machine is, never locks. No document, UI or release note may describe
this tier as available before its acceptance evidence exists.

**Enrollment requires §L.1 elevation.** Enrolling a key refuses every
interactive login, and there is no `su` and root has no login at all,
so an enrolled machine would otherwise have no administrative path. The
activation increment therefore lands only after the `APPLICATIONS.md`
§L.1 consent-only elevation workstream, whose "Elevation increments" L1
to L7, all landed, made `deploy-rollback`, `set-hostname` and
`deploy-publish` approval-key operations and then deleted `su` and
locked root ("Retiring the escape hatch"). Enrollment does not exist on
any build without that elevation. Root login is never re-enabled as an
administrative path.

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

**Firstboot** ensures the directory on every boot before any consumer
runs. It is one stage-1 init step, `td-firstboot ensure-login-directory
/sysroot`, run once `@var` is mounted at `/sysroot/var` and before
`render-primary-sshd` (SSH), on the live medium too. It is not fatal:
init continues whatever it reports. It creates only what is absent,
`/var/lib/td` as root:root mode 0755 and `/var/lib/td/login` as root:root
mode 0700, and never changes the owner or mode of anything that exists,
a parent included. In a valid directory it unlinks every entry whose
name begins with `tmp-` (below) and nothing else; it never creates or
removes a record. It walks the path as the record store does
(`td-secret/DESIGN.md`, "Login record store") and refuses rather than
repairs invalid existing metadata: for that, or a `tmp-` entry it cannot
unlink, it prints one console line, `td-firstboot: login directory
refused: ` and the reason, and exits nonzero, which init's `|| :`
ignores. It creates only in a parent that root owns and that no group
or other may write, so nothing else can put a directory under the name
between its `mkdir` and its open, and refuses otherwise. It then opens
the new directory without following a link, requires that descriptor
to show an empty directory with two links, or the one Btrfs reports for
every directory, and only then chowns and chmods it through that
descriptor and syncs it and its parent; anything else is refused and
left as it is.

The login state is **unenrolled** only when the directory is valid and the
record name is absent, **enrolled** when a valid record is present, and
**unavailable** otherwise. The first test is a directory-and-name check,
the same predicate in firstboot, td-login and td-authd, one shared
std-only module (`td-secret/src/login_state.rs`, increment 4) that takes
the root it reads under, and needs no helper; only a name that exists is
parsed, so nothing about the record's bytes can make an unenrolled
machine look enrolled or the reverse (`td-authd/DESIGN.md`, login-state
amendment). Unavailable has three typed
causes, each with its screen text:

- a damaged directory (a wrong owner, group or mode, a non-directory or
  nothing at that path, or a symbolic link at or above it):
  `LOGIN KEY STATE UNAVAILABLE: DIRECTORY DAMAGED`;
- a damaged record (the name exists, but is not a regular single-link
  root:root mode-0600 file (an inode with no links is the race below), or its bytes are truncated, malformed, of an
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
getInfo options carry `alwaysUv` with the value true; one reporting
false is admitted. Planned with `td-install/ENCRYPTION.md` increment 8
(8b): admission also requires getInfo's versions to include `FIDO_2_1`,
since CTAP 2.0's hmac-secret returns the same output with or without the
PIN, so a stolen key would unlock without it; and the probe step adds
one assertion with presence and no PIN, which must find no credential or
return an output different from the repeat's, or the key is refused as
unable to require its PIN. A key with built-in UV, such as a biometric
key, is used with its PIN only: td never requests built-in UV. A key
without a configured PIN is refused before any creation, with an
instruction to set one using another tool, and a key that cannot hold a
PIN is refused as unsupported; td never sets, changes or resets a PIN or
a key. Every operation requires exactly one connected FIDO device and
refuses none or several before asking for a PIN, so the add and
enrollment flows tell the person to remove the authorizing or previous
key before inserting the next.

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
independent `td-fido/tests/login_ctap_vectors.py`:

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
`0e`; nothing is retried, and the compositor re-reads the state through
`1a`, as after every login operation (increment 4's C3). Any other kind
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

The lock exists only for an enrolled or unavailable account; on an
unenrolled one the triggers below change nothing
(`td-compositor/DESIGN.md` says what each shows). The compositor learns
the login state as `td-authd/DESIGN.md` specifies. Enrolled and
unavailable both start locked; unavailable shows its cause's text ("The
login record") and offers no unlock. The live
medium never locks: td-authd answers it unenrolled (`td-authd/DESIGN.md`,
amendment 1). Only the compositor's paired profile locks; the direct
development profile leaves `Super+l` as it is.

The session locks at every compositor generation start (boot, and any
compositor or authority restart or crash), on `Super+l`, on the attention
screen's `L`, on a lid close and on resume from any suspend, however it
began, that lasted longer than the compositor's two-second threshold plus
the kernel's one-second RTC granularity for measuring sleep on x86
without a TSC that runs in S3. A shorter suspend can go unnoticed. The
compositor's detection of each trigger is in its design; it only
detects a suspend and never starts or delays one. td has
no suspend initiator and this tier adds none. Any initiator that lands
later must, for an enrolled account, obtain the lock surface's
presentation receipt before writing `/sys/power/state` and refuse to
suspend when locking fails. Hibernation stays disabled.

Locking closes overlays, withdraws focus and grabs, and paints the lock
surface over the whole output. Until that paint has a presentation
receipt, no client receives input. A lock that comes while an attention
lifetime is open, from a lid close or a resume (the attention screen
reads `Super+l` as its own key, on the menu as `L`, and `L` is itself
the lifetime's selection, which drains as Escape's does), ends
that lifetime first. Before its operation's commit it cancels the
operation under the existing pre-commit cancellation rules. After the
commit the screen drains as Escape's does and the result is not shown:
root finishes a committed operation whatever the screen does. In a
login-unlock lifetime, already on the lock surface, the lock counts as
Escape, so even a committed unlock leaves the session locked. No key
started a lid's or a resume's close, so input then settles by time
rather than discarding each device's first report: keys typed after
the lock and past its 100 ms window are taken, the unlock chord whole.
A resume is found by the batch of reports that follows it and locks
before that batch is routed, so that batch's own keys, stamped before
the lock, are dropped (`td-compositor/DESIGN.md`, "Physical secure
attention").

While locked:

- The output shows only the lock surface: hostname, username and the
  state's rows below. No client pixel, cursor, title or workspace bar is
  shown.
- Applications keep running. They receive no focus and no input from any
  source; new windows map behind the lock. Frame callbacks are not
  throttled. Audio and network continue.
- Ordinary bindings (terminal, launcher, help, workspaces) are suppressed.
  Control-socket and automation requests that inject input or capture output
  refuse; bridge clipboard snapshots already require keyboard focus.
- Queued credential writes and installations stay unselectable; their own
  intake deadlines apply.

The lock protects the interactive surfaces, not running processes, as
`td-install/ENCRYPTION.md` says of any locked running machine.

The lock surface's rows are chrome rows, as the failure texts are
("Failure texts"): the hostname, the username, then the state's rows
below. Both names are the current `1a` answer's
(`td-authd/DESIGN.md`, amendment 1), drawn uppercase, and an answer
without a hostname leaves out its row. Every row but the hostname fits
45 columns whole. A hostname of up to 63 bytes has no space to wrap at,
so it breaks at the output's last column instead
(`td-compositor/DESIGN.md`, "Session lock and login-key entry"). The
surface gives no recovery hint (Recovery).

| When | Shows |
| --- | --- |
| enrolled, attention closed | `LOCKED`, then `PRESS CTRL+ALT+ESC TO UNLOCK` |
| unavailable, attention closed | `LOCKED`, then its cause's two rows |
| unenrolled, attention closed | `LOCKED`, then `NO LOGIN KEYS ENROLLED` |
| the chord, enrolled, until the unlock's first prompt | the attention screen's `PREPARING REQUEST`, never its menu |
| the chord, unavailable | the attention screen with the cause's two rows; no `1b` |
| the chord, unenrolled | the attention screen with `NO LOGIN KEYS ENROLLED`; no `1b` |

Only a generation's start, `Super+l`, `L`, a lid close and a resume
lock. A `1a` answer never locks an unlocked session, even one whose
state turns unavailable; it changes only what the next lock shows and
whether one is offered. While locked, the rows follow each `1a` answer.
A state that could not be read resolves through the compositor's
polling (`td-authd/DESIGN.md`, amendment 1), and enrolled brings back
the unlock. A locked session
whose state becomes unenrolled stays locked for the rest of its
generation, since nothing on the lock surface unlocks it; the next
generation, at a reboot or a compositor or authority restart, starts
unlocked, as every unenrolled one does.

On the lock surface Ctrl+Alt+Esc opens a login-unlock attention lifetime
directly, without the menu. The prompt presents the unlock description; the
worker identifies the connected key, which must already be the only one
connected (the worker does not wait for it); the next presented step
names its fingerprint and remaining PIN attempts and opens the PIN field;
after the PIN comes the touch. The worker compares the record with its
baseline before identify and again before its success. td-authd reports
success only after the worker's success frame and observed exit, and the
compositor then drains held input and restores the ordinary screen and
focus. No key started that close, so input settles by time rather than
discarding each device's first report, and the first key typed past its
100 ms window reaches the session (`td-compositor/DESIGN.md`, "Physical
secure attention"). Only that success for the unlock the compositor
committed, with no Escape since, unlocks: an Escape that came first,
even after the commit, keeps the session locked. A failure shows its
typed reason on the trusted screen; Escape returns to the lock surface.
One unlock is allowed per attention lifetime. Unlocking starts no
process, switches no credential and releases no secret.

## Locked boot

With an enrolled account, td-svc's order is unchanged: the compositor and
authority pair starts and the first frame is the lock surface. The human
session units (the terminal and the applications) start behind the lock as
they do today, and their windows map behind it. Running the account's
processes before authentication discloses nothing on an unencrypted disk
and grants no interactive access, and it keeps boot health, which requires
the terminal, unchanged. The follow-up that releases the secret store at
login, and the protected tier's "one fresh session" rule, must revisit
this and may hold session start until the first unlock. The one
planned exception to a locked start is the protected tier's admitted
first generation, which `td-install/ENCRYPTION.md`, "Verified account
handoff", owns.

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
names that file (`td-firstboot/src/ssh_policy.rs`). Root has no SSH
login in either form: both carry `PermitRootLogin no` (`APPLICATIONS.md`
§L.1, L7), and root's shadow field is `!`. This is that rule's one
statement; other documents point here.

The persistent root-owned `/etc/ssh/authorized_keys`, empty on a fresh
install, admits no account on a td image: root is refused, every
account but the primary is locked, and the primary's `Match` block names
its own file. It remains as the carrier of the QEMU root-refusal
fixture's key, and the enforced form's `AllowUsers` is defence in depth
over it. A deployment older than L7 reads it as root's
(`APPLICATIONS.md` §L.1, "The rollback window"), which is why firstboot
warns while it holds a key.

In the enrolled and unavailable states the rendered policy takes its
**enforced form**, which admits no account but the primary. SSH then
opens no session without a login key except that self-test. Only a
verifiably unenrolled state renders the ordinary policy. At every boot
stage-1 init's `td-firstboot render-primary-sshd /sysroot`, right after
the directory step ("The login record") and before `sshd` starts,
renders the form that matches the login state, decided by the
directory-and-name check alone, with no helper: a valid directory
without the record name renders the ordinary policy, and anything else
the enforced form. The ordinary policy is byte for byte the one rendered
before this tier but for L7's root line, `PermitRootLogin no` where it
was `PermitRootLogin prohibit-password`. The cutover renders it within a
boot. The QEMU root-refusal fixture (`APPLICATIONS.md` §L.1, "Retiring
the escape hatch") runs on unenrolled images, and root is refused in
both forms.

The two forms differ in one place. The enforced form adds the line
`AllowUsers NAME` right after `PermitRootLogin no`, still in the global
section before the primary's `Match User NAME` block, NAME being the
admitted primary account; every other byte is the same. The shared
account validation admits only a lowercase letter
followed by lowercase letters, digits, `_` and `-`, so NAME carries none
of OpenSSH's pattern characters and `AllowUsers` names that one account.
Firstboot reads the accounts first, whose failure still stops boot, then
the state under the same root it renders under, for UID 1000's record
with root:root as the owner. The predicate answers every failure to read
as unavailable, never as unenrolled, so a read that fails renders the
enforced form and nothing falls back to the ordinary one. The realized
OpenSSH recipe test checks both forms with the built `sshd -T`, and the
stock image's boot health proves root's seeded key refused
(`TD-ROOT-SSH-REFUSED`, `THREAT-MODEL.md` §8).

## Cutover

Whenever the login state moves between unenrolled and enforced (enrolled
or unavailable), by an operation or otherwise, td revokes every
interactive session that the other state allowed: a serial session the
greeter started before enrollment would otherwise keep a UID-1000 shell on
`ttyS0`, and an SSH session the other policy admitted would outlive
it. td-authd renders the SSH policy for the new
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

The restarted greeter re-enters `login-primary`, which refuses and
returns the terminal to root (`THREAT-MODEL.md` §3). The greeter's
`tty=` containment covers its leader and every process whose controlling
terminal is that device (`td-svc/DESIGN.md`, "Stopping"). A process that
holds a descriptor to the line without it being its controlling
terminal, because it opened the line that way or called `setsid()` and
kept an inherited descriptor, survives, and the hand-back does not take
its descriptor away: a chown revokes no open file. Such a descriptor
exists through an act of the account, or a compromise, while the line is
still the account's. That is before enrollment, and also after it until
the refusal's hand-back runs, because the node stays the account's own
until the restarted greeter's refusal takes it for root. Revocation
should therefore return the line to root:root with the pinned mode
before it restarts the greeter, which narrows that window to the time
between the state change and the revocation. Increment 5 owns that step;
the refusal's hand-back alone does not close the window. `sshd`'s unit
stops its whole service leaf (`stop=leaf`, `td-svc/DESIGN.md`), so
OpenSSH sessions in their own process groups end too.

Removing the last key reverses the cutover, so console login and the
ordinary SSH policy return at once. The graphical session that performed
the change stays unlocked; td-authd's next login-state answer makes the
lock available or unavailable at once.

## Deployments

A deployment carries the tier when its uncompressed `initramfs.cpio`
holds the fixed tier marker naming the record versions it reads. The
marker is the archive member `etc/td-login-tier`, a regular file whose
whole content is `td-login-tier-v1`, then at least one version, each
one the build's record codec reads (`READS`), as a space and a decimal
number from 1 to 255 without leading zeros, strictly increasing, then
one newline: an increment-4 build's is `td-login-tier-v1 1\n`. The
deployment ID covers it, being the SHA-256 of the manifest, which holds
the initramfs's SHA-256.

The marker claims that the deployment honours the record at every entry
point: the console refusal (increment 4's C5), the enforced SSH form
(C6), the locked start (C7), and the session lock on `Super+l`, `L`, lid
close and resume (C9, C10). Enrollment trusts `current` and `previous`
on that claim, so a deployment that carried the marker but not that
enforcement would be a rollback target that boots open on an enrolled
machine. Deployments therefore carry it only from C10b, the commit after
C10, and every deployment built from C10b onward carries it; the readers
below landed earlier, in C4, and read no version from a td-built
deployment built before C10b.

Since C10b the system recipe writes it into the deployment phase's
`gen_init_cpio` spec only, beside `dir /etc 0755 0 0`, as `file
/etc/td-login-tier` with mode 0444 and owner 0:0, its content a constant
of the recipe; the selector initramfs carries none. A recipe test reads
`td-secret/src/login_record.rs`'s `READS` and requires the constant to
list exactly those versions in the grammar above, so a codec that reads
a new version cannot ship a marker that omits it.

The marker is not in the manifest. td-boot's `parse_manifest` admits
exactly the four-line `td-deployment-v1` form, and an installed
selector, which nothing updates but its owner's consented selector
update (planned, `td-install/ENCRYPTION.md` increment 8), would refuse
every deployment carrying a fifth line or a new manifest version. The
initramfs is already covered, and a member that nothing at boot reads
changes no boot.

A reader opens the deployment's `manifest` and `initramfs.cpio` through
a held directory descriptor, each with `O_NOFOLLOW | O_NONBLOCK` (as
td-authd opens a queued manifest, through the descriptor's
`/proc/self/fd` path), and requires each to be a regular file of the
manifest's owner rule: owned by the requester for a queued update, by
root for a retained deployment. The manifest keeps its 4096-byte bound
and the archive takes td-update's 512 MiB initramfs staging bound
(`td-install/DESIGN.md`), checked before reading; a size change while
reading refuses. The archive is streamed once, hashed as it is parsed,
and the read gives up after its caller's budget, reading no version: two
seconds for request 19's queued update and ten for each retained
deployment the worker reads. Request 19 is answered synchronously on the
paired channel, after amendment 1's fresh read with its two-second
helper, and the compositor waits five seconds for any answer, so the
helper and the marker read together stay under that receive and an
archive too large or slow to hash in time is refused with `99 01` rather
than ending the paired generation; a td-built initramfs hashes in a
fraction of the budget. The worker reads two deployments, at most twenty
seconds, well inside its 120-second operation ceiling. As for any
synchronous filesystem read, neither budget can interrupt a stalled
filesystem. A queued update's source is any requester-owned directory,
which today includes one reached through `/proc/<pid>/root` inside the
requester's own mount namespace. td ships no FUSE, so nothing the
requester can mount there stalls a read today, but a FUSE mount could
hold request 19 past its budget, and the intake's own capture already
opens the source and reads its manifest on the serve loop. Before td
supports FUSE, the intake must refuse a source reached through `/proc`
at capture, which a path prefix cannot do since intermediate links are
followed, so by a link-free walk (each component opened `O_NOFOLLOW`
through the last) or `openat2`, or move capture's open and reads and
request 19's marker read off the serve loop together. The manifest's
SHA-256 must equal the deployment ID naming it and the archive's the
manifest's `initramfs.cpio` entry, each over the bytes parsed. The newc
reader is bounded: magic `070701` only; each member's name field of 2 to
4096 bytes counting its terminating NUL, which must be its last byte and
its only NUL; at most 65536 members before `TRAILER!!!`, the trailer not
counted; every other member's data skipped unbuffered; the marker the
one member named exactly `etc/td-login-tier`, a regular file with one
link and at most 256 bytes; and nothing after `TRAILER!!!` but NUL
padding.

- A **queued update** is read through the directory File td-authd
  holds for request 19, against the ID it was admitted with.
- A **retained deployment** is read through a held descriptor of the
  read-only `/run/td-volume/td`: the `boot/current` or `boot/previous`
  selector is read once and must be exactly `../deployments/` and 64
  lowercase hex digits, and `deployments/<id>` is then opened by that
  name, with `O_DIRECTORY | O_NOFOLLOW`, and checked against that id.
  Deployment directories are named by their content, so a selector that
  flips meanwhile cannot mix two deployments' files into one read.

A deployment whose files are missing, of the wrong type, owner or size,
do not verify or parse, whose marker is absent, duplicated, not a
regular file or malformed, or that cannot be read in time reads no
record version, as a deployment without the tier.

`td-secret/src/login_tier.rs` implements this reading (increment 4's
C4; `td-secret/DESIGN.md`, "Tier marker reader"), with the budget a
parameter of each reader: td-authd's `MARKER_GIVE_UP` of two seconds
and the worker's `RETAINED_GIVE_UP` of ten. Its tests build their own
archives; the recipe's writing is C10b's, which a recipe test and the
build's archive check hold to the placement above.

A deployment without the marker ignores the record, so booting it
restores automatic login. Therefore:

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
username and the hostname (`td-authd/DESIGN.md`, amendment 1).
Increment 4's C3 implements it. What `D` shows without a list, on a
machine with no enrolled record, is `td-compositor/DESIGN.md`'s
("Login-key operations"); its digits and request are tested against
lists the tests supply, through root's answer.

**First enrollment** applies only to an unenrolled account and is
authorized by the physical selection alone: no key exists yet, and a person
at the automatically logged-in session holds the account under §L.1's
scope. The person chooses:

- `2`, two keys: the primary, then a backup created with the primary
  excluded. td asks for a separate physical key but cannot prove distinct
  hardware.
- `1`, one key.

Either choice first shows an enrollment disclosure, confirmed by the
root-drawn approval key under `td-compositor/DESIGN.md`, "Elevation
consent", never by Enter. It states that from then on the
machine boots and locks to a screen only an enrolled key and its PIN
open; that open console and SSH sessions end now, and SSH for any
account but the primary is refused from then on (root SSH already is,
since §L.1's L7); that administration then needs a login; and that a PIN
set on another keyboard layout may not be typable here. The one-key
choice adds that losing this key, or blocking its PIN, leaves no way to
log in, locally or over SSH; that recovery then needs someone who can
start other code on this machine (a firmware boot menu or UEFI shell
suffices on an unencrypted disk), the same access that bypasses the
lock; and that a backup key can be added later.

Every key is created, proved, repeated and probed before anything is
published; publication then makes the cutover above.

**Adding a key** (`A`) refuses at eight keys before any token. One
enrolled key and its PIN authorize it with an authorize-phase assertion,
or, on a planned protected volume, a disk proof (below). At a connect
step the person then removes that key and connects only the new one,
which is created with every enrolled credential excluded, proved,
repeated and probed, and the record is published against the unchanged
baseline.

**Removing keys** (`D`) takes a nonempty set selected by digit. The
description names each selected key by its position in the record's
canonical slot order, which the digits show, and its fingerprint, so two
keys that share a fingerprint stay distinct. One enrolled key and its
PIN authorize it, or on a planned protected volume a disk proof
(below), and that key may be in the set. Leaving exactly one
key requires the one-key disclosure. Removing every key is refused on a
planned protected volume, where the machine never logs in without a key;
elsewhere it requires a disclosure that the machine will log in without
a key and that open SSH sessions end now, confirmed the same way; the
record is then unlinked and the cutover restores console login and the
ordinary SSH policy.

**On a planned protected volume** (`td-install/ENCRYPTION.md` increment
8; td-authd's `/var/lib/td/login/protected` marker present), this is the
one place the rule is stated. Removing every key is refused, as above.
A first enrollment, an addition, and a removal of keys the person no
longer holds may each be authorized, instead of an enrolled key's
assertion, by a disk proof made fresh inside that operation from an
unlocked session. Either proof must open a keyslot, since a header
token alone is something anyone who can write the disk can add:

- the volume's 48-digit recovery key, typed into the compositor's PIN
  field on the operation's step, a secure-attention surface no client
  draws, and tested by the worker with `cryptsetup open
  --test-passphrase` on the keyslot the volume's `recovery-key` marker
  names, the key on standard input; or
- an authorize-phase assertion from a disk security key, with its PIN
  and touch, whose client data is the disk domain's over this
  operation's canonical description, requesting hmac-secret under UV:
  the worker verifies the signature under the header token's public
  key, derives that token's passphrase from the output
  (ENCRYPTION.md, "FIDO2 protectors"), and tests it with `cryptsetup
  open --test-passphrase` on the keyslot the token names.

The recovery key is not hardware-backed, and is acceptable here because
its holder already has equal power offline: from the live medium it
opens the volume and can rewrite the login record ("Recovery"). The
step's description names which proof it asks for and the operation it
authorizes; the operation's physical selection and consent are the ones
above; and one proof authorizes that one operation, with nothing
remembered after it. A session admitted at boot does not authorize it:
admission only unlocks the first frame.

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
is deliberate. td ships no privileged recovery environment, and its live
medium is none: the live session is unprivileged (`APPLICATIONS.md`
§L.1, Recovery). On an unencrypted volume, anyone who can start an
external recovery environment on the machine (another OS, through a
firmware boot menu or a UEFI shell) can mount `@var` and delete
`/var/lib/td/login/1000`, which restores automatic login. The
same access repairs the unavailable state. A damaged record is removed
the same way, which also restores automatic login, or replaced by an
intact copy of the same machine's record. A damaged directory is
removed, or restored as a root:root mode-0700 directory, and the next
boot's firstboot recreates or accepts it. That is the owner's recovery
and the bypass named in Scope. On a device-bound volume such an
environment cannot unseal, because the TPM releases the volume only to
td's own selector (`td-install/ENCRYPTION.md`); the volume's recovery
key opens it there for the same repair. On a planned protected
volume, a boot admitted by its disk PIN or a primary disk token starts
unlocked, a recovery boot starts locked (`td-install/ENCRYPTION.md`,
"Verified account handoff"), and lost login keys are replaced with a
disk proof ("Enrollment, addition and removal").

A firmware supervisor password and boot-order lock narrow the physical
path on hardware that has them; td makes no claim about them. The record
is not backed up, and a key is not a backup of anything.

## Administration

A key and its PIN are required only to unlock and to add or remove keys,
which is how recovery policy changes. Ordinary elevation is the §L.1
consent-only mechanism, which an enrolled machine always has
("Enrollment requires §L.1 elevation", above, owns that prerequisite).
As on every td machine, `su` no longer exists and root's shadow field
is locked (§L.1's L6 and L7), interactive root login is refused
(`THREAT-MODEL.md` §3) and root SSH is refused (SSH). Root remains
reachable only through root-owned services and physical access, and root
can remove or rewrite the record, so a change made as root bypasses the
key requirement. Software in an unlocked session acts as UID 1000 with
all of that account's data, which needs no root (§L.1 scope).

## Other contracts

**Disk encryption.** The device-bound tier and this one each keep their own
scope; `td-install/ENCRYPTION.md`, "Device-bound default", owns the rule
that their combination is not lost-laptop protection. Login records are
not disk protectors and never become ones. The protected tier's plan
(`td-install/ENCRYPTION.md` increment 8, "FIDO2 protectors") keeps its
disk credentials separate: created for the disk, held in the volume's
LUKS2 header rather than the record, with their own client-data domain
and salts. One physical key may hold a login credential and a disk
credential, and enrolling either never creates or changes the other;
that tier's shape minimums govern disk protectors.

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

Twelve `qemu-secret` login guests run without a TPM. Eleven of them run
the root worker's own operation over the production discovery, Session and HID worker with
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

The twelfth, `login-desktop` (`td-secret/DESIGN.md`, "Login desktop
guest"), pairs the production compositor and td-authd over a record
that guest's worker enrolled, through a UHID key and a UHID keyboard,
and the host checks the display through QMP `screendump` at each step
the guest names, the guest acting only on the host's answer. After a
screen of a colour no compositor paints, every capture shows only that
colour, black or the lock surface's own pixels until the exact lock
surface with the hostname and username rows appears, at boot and again
after the pair restarts; a client window
mapped behind the lock leaves the whole frame the lock surface's, and
while the generation is locked every capture, between steps included,
holds only those colours. The chord opens the unlock: a wrong PIN typed
into the field, its empty field and its four masks seen, ends it with
`WRONG PIN`, still locked; a key not in the record
ends it with `THIS KEY IS NOT ENROLLED HERE` and no PIN step; the
enrolled key's PIN and a slow touch, the touch request seen, unlock to
the desktop with the client's window on glass. A damaged directory
starts locked with its cause, and the chord shows the cause with no
`1b`: td-authd reaps no child and has none, after the chord and after
Escape, where during the unlock before it the worker was its only
child. Once the worker removes the record, a generation starts unlocked,
with no lock pixel in any capture. The captures are samples, so a frame
shown for less than a capture's interval can be missed; the
compositor's host tests hold every frame handed to the output. Escape
after an unlock's commit is not timed in the guest, since the commit
round follows the touch and the success follows the commit at once;
the compositor's host tests cover it.

A thirteenth TPM-free guest, `login-powercut` (`td-secret/DESIGN.md`,
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
temporary cleanup landed with increment 4's C1 and is tested on the
host (`td-firstboot/src/login_directory.rs`).

The stock VM stays unenrolled: no recipe or firstboot path writes a record,
and its valid, empty directory is decided unenrolled without any helper,
so this tier leaves the existing serial-console and SSH oracles unchanged.
`APPLICATIONS.md` §L.1's L7, which locks root and refuses root SSH in
both forms, changed them on its own account.

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
holds. No production path reads the login state until increment 4, whose
C1 makes firstboot ensure the directory, so its absence, itself a
damaged directory, never meets a production reader. From then on, that
exclusion apart, behaviour changes only when a record or an invalid
directory exists, which no production path creates, and the state is
decided by the directory-and-name check before any helper runs, so no
helper failure can change an unenrolled machine. Increment 4 has
exactly three exceptions, live on every machine the paired compositor
runs, record or not: `Super+l` is consumed, and does nothing while
unenrolled; the attention menu always shows `L: LOCK SCREEN` below `K`,
which answers `NO LOGIN KEYS ENROLLED` while unenrolled; and `D` without
a key list answers `NO LOGIN KEYS ENROLLED` where it answered `NOT
AVAILABLE IN THIS BUILD`. Lid-switch admission and resume sampling are
no exceptions: they run only while the state is enrolled or unavailable
(`td-compositor/DESIGN.md`, items 6 and 7). Each lists what it proves
and the oracle that shows it.

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
     worker with a seeded record, with framebuffer bitmap checks: moved
     to increment 4 as `login-desktop`.
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
     `D` was refused locally until increment 4's C3 supplied the key
     list; root refuses its removal until increment 5.
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
     operation that would reach a PIN step.
   - The lock surface has landed (`td-compositor/DESIGN.md`, "The lock
     surface"), inert: the only setter of its state is compiled into
     tests alone, so nothing in production can lock. Locked, the
     output shows only `LOCKED` and `PRESS CTRL+ALT+ESC TO UNLOCK`, no
     client is focused or given input, and no ordinary binding runs. Its
     chord, read under the selection rules so that a security key's own
     keyboard cannot use it, opens one attention lifetime straight into
     a login unlock, `1b 07` with no menu, which carries the chained
     unlock with its PIN field; only root's `06` for that unlock's
     committed step, with no Escape since, leaves the lock surface and
     closes attention once held input is released, `SESSION UNLOCKED`
     shown meanwhile. Host and device-dispatcher tests drive the whole
     unlock through the device dispatcher; a failure's text and Escape
     before and after the commit, still locked; and forged and
     out-of-order `06`s, which end the paired generation without
     unlocking. The next generation starts locked only from increment
     4's login state at Prepare; until then every generation starts
     unlocked. This build's root answers an unlock `NO LOGIN KEYS
     ENROLLED`, or `DIRECTORY DAMAGED` while the directory is missing.
   - Increment 3 is complete. Its desktop guest moved by decision to
     increment 4's `login-desktop`: without a fixture entry, which this
     build deliberately lacks, no guest reaches the lock surface before
     increment 4's locked boot. The hardware record of an OTP touch
     stays owed with the T430s evidence.
4. **Locked boot and session lock:** request `1a` with the login state,
   the enrolled key list, the primary username and the hostname;
   firstboot's directory and temporaries; the unavailable state;
   firstboot's boot-time render of the enforced SSH form for the enrolled
   and unavailable states; td-login's console refusal; removal of
   `build_autologin`'s `login -f` fallback; the tier marker and
   td-authd's update-consent refusal; the locked start; `Super+l`,
   attention `L`, lid close and resume detection; and unlock end to end.
   It lands as these commits after this amendment's documentation-only
   C0, each one green and independently landable:
   - C1, landed: the shared login-state predicate ("The login record"),
     with the root as a parameter, which td-secret's record store now
     walks through; `td-firstboot ensure-login-directory /sysroot` in
     stage-1 init before `render-primary-sshd`; and rootcheck's report
     of `/var/lib/td/login` as a root:root mode-0700 directory, through
     the read-only `td-firstboot check-login-directory /`, as
     `TD-LOGIN-DIRECTORY-OK`, a marker of its own outside the flag boot
     health reads, so the oracles that require it (`qemu-boot-system`,
     `qemu-boot-session`, `qemu-install-system` and
     `qemu-boot-encrypted`) see the directory while a damaged one still
     boots healthy. It changes every boot, so its landing runs `check
     integration` by hand.
   - C2, landed: the read-only `td-secret inspect-login --uid 1000`
     helper (`td-secret/DESIGN.md`, "Read-only enrollment-state
     inspection"), sharing inspect-store's admission and reply code and
     reading through the record store, which nothing runs yet; host
     tests only, since no guest reaches it before C3.
   - C3, landed: `1a` end to end (`td-authd/DESIGN.md`, amendment 1):
     root's predicate, the helper under its two-second deadline, the
     cache and the `9a` answer; the compositor's `1a` at connect, after
     Prepare, after every login operation and every 250 ms while the
     state could not be read, feeding the key list `D` uses. Root still
     answers a removal `9b 00`, and nothing locks yet. Host tests cover
     each state, the helper missing, slow, malformed and answering, the
     cache, the helper's pause while unreadable, the live boot, the
     hostname rules and the exact bytes on root's side, and every
     malformed `9a`, the connect-time and polled `1a` and `D` over a
     list or none on the compositor's; `qemu-secret`'s
     `supervise-login` case also reads `1a` through the production
     helper.
   - C4, landed: the tier marker's readers, not its writing (C10b):
     `login_tier.rs`, the marker's grammar and bounded newc reader
     ("Deployments"); the worker's read sets taken from the retained
     deployments' markers in place of its empty `UNMARKED`
     (`td-secret/DESIGN.md`, "Login-key worker"); and request 19's
     refusal (`td-authd/DESIGN.md`, amendment 8), shown as `UPDATE
     CANNOT READ LOGIN KEYS`. No deployment carried the marker until
     C10b, so between C4 and C10b the worker read no version and request
     19 on an enrolled or unavailable machine refused every td-built
     update; nothing enrolls before increment 5, so neither refusal cost
     anything. Host tests cover the grammar, every reader bound, the
     digest chain, the read sets over fixture volumes, request 19 on
     each state and marker and past its two-second budget, all over
     archives the tests build, and the compositor's text; the refusal
     on a full system is `qemu-login-system`'s (C11). No boot archive
     changes.
   - C5, landed: td-login's console refusal through the shared predicate
     (`THREAT-MODEL.md` §3), compiled into td-login as its second
     reviewed `#[path]` and staged by its recipe: interactive `login` and
     `login-primary`, for a caller root in some uid column, read the state
     on the real root before anything else that could start a session and,
     unless it is unenrolled, hand the line back to root:root with
     `TTY_MODE`, write the one fixed line and park; and the deletion of
     `build_autologin`'s `login -f` branch, a non-primary autologin
     account now refusing the build and `system_def_is_self_consistent`
     requiring the autologin account to be the primary account. Host tests
     cover the decision on each state and cause, a spawned gate on each
     refusing state that writes the line last within a 30-second deadline
     and is still alive after a further grace period, an unenrolled one
     returning, the hand-back's order (owner first, nothing written
     before a refused chown, a wrong read-back refused), the grant's
     writes and the recipe's staging; the refusal on a full system is
     `qemu-login-system`'s (C11). It changes every boot's greeter path,
     so its landing runs `check integration` by hand.
   - C6, landed: `render-primary-sshd`'s enforced form ("SSH"), its
     unenrolled output byte-identical to the one before it. The
     predicate was already td-firstboot's reviewed `#[path]` since C1, and
     its recipe already staged it. Host tests hold the ordinary form to a
     literal copy of the policy before it for several names, the enforced
     form's exact bytes, the ordinary form chosen only by an unenrolled
     state, and the enforced form over temporary roots that are enrolled
     (the record name in any shape), damaged in each way the predicate
     names (a wrong mode or owner, missing, a file, a link at or above
     it, a relative root) and unreadable (an unsearchable parent); the
     admitted name is the one the shared account validation admits; and
     the recipe tests keep the single render right after the directory
     step and check both forms with the built `sshd -T`. The enforced
     form on a full system is `qemu-login-system`'s (C11). It changes
     the boot path's td-firstboot, so its landing runs `check
     integration` by hand.
   - C7, landed, the activating commit: the compositor's locked start
     (`td-compositor/DESIGN.md`, "The lock surface"). In the paired
     profile, the `1a` answer at connect locks an enrolled or
     unavailable session in the generation's first paint, which takes
     the place of its first repaint, before any input reader or client;
     that answer alone decides, never a later poll's, so a state that
     could not be read at connect starts locked even if a poll reads it
     unenrolled first. Every generation, a restarted compositor's
     included, starts locked on that rule; the live medium's root
     answers unenrolled and the direct development profile has no
     authority, so neither locks.
     The lock surface draws the answer's hostname and username above the
     state's rows and follows each later answer. On it the chord sends
     `1b` only while the state is enrolled; unavailable shows its cause's
     rows and unenrolled `NO LOGIN KEYS ENROLLED`, sending nothing.
     `Scene::lock` lost its test-only gate, its one production caller is
     that first paint, and the amended source pin holds it; the live
     entries for `Super+l` and `L` stay test-only until C9. Session units
     start behind the lock, as "Locked boot" says. Host tests cover the
     start decision per state, unenrolled and no authority, every frame
     a recording output is handed, relock per generation, the rows' exact
     positions and a 63-byte hostname's wrapping, the chord on each
     state, a state turned unenrolled while locked, and the pin. No
     oracle reaches the locked path, since the stock image is
     unenrolled: `login-desktop` (C8) and `qemu-login-system` (C11) do.
   - C8, landed: the `login-desktop` guest (below, and "Evidence"), a
     twelfth `qemu-secret` login case that `td-recipe-eval qemu-secret
     --case login-desktop` runs alone, whose host answers the display
     checks the guest asks for over ttyS0 (`td-secret/DESIGN.md`,
     "Login desktop guest").
   - C9, landed: `Super+l`, the attention screen's `L`, locking an open
     lifetime ("Session lock") and the help sheet's row
     (`td-compositor/DESIGN.md`, item 5). The runtime's and the evdev
     adapter's live entries lost their test-only gates, and the amended
     source pin holds that they, beside the first paint, are the only
     production locks: the adapter's entry needs the paired profile and is
     reached only from the bindings' decision for `Super+l` and `L`.
     `Super+l` is read before the sheet's and launcher's capture, always
     consumed, and locks only an enrolled or unavailable session; the
     direct profile leaves it the client's. `L: LOCK SCREEN` sits below
     `K` on the menu alone, where the menu's last row moved to 564 on
     1280x800 (§L.1's `B` and `H` rows later moved it to 636) and every
     other screen's stays at 528. A lock while a
     lifetime is open ends it through Escape's drain. Host tests cover
     `Super+l` on each state and profile, under the sheet and launcher
     and through the device dispatcher; `L` on each state, its refusal
     elsewhere than the menu and from a security key's own keyboard; a
     lock before and after an unlock's commit, with a key held; the
     menu's rows at 1280x800, 800x600 and 320x200; the sheet's row in
     the paired profile alone; and the pin. `qemu-login-system` (C11)
     drives `Super+l` and `L` on a full system.
   - C10, landed: a lid close and a resume (`td-compositor/DESIGN.md`,
     items 6 and 7), each locking through the evdev adapter's suspend
     entry, which needs the paired profile and an enrolled or
     unavailable last `1a` answer and otherwise does nothing; the
     amended source pin holds that these two, beside the first paint,
     `Super+l` and `L`, are the only production locks. A switch-only
     lid node declaring `SW_LID` in sysfs is read by its own reader,
     which acts on a close and reads nothing else, and only where the
     answer at connect is enrolled or unavailable; otherwise, on an
     unenrolled machine, with no answer and in the direct profile, it is
     not opened at all, where it was opened and ignored before, and it
     never reaches the ordinary readers. Resume is a gap of more than two
     seconds between how far `/proc/uptime` and `Instant` advanced,
     checked before each input batch is routed and before a reader's
     teardown releases what it held, before each repaint, focus change
     and input delivery that could reach a client, which wait while a
     lock is owed, and at least once a second by a monitor thread, only
     while the last answer locks. The monitor runs for the generation on
     every paired machine, enrolled or not, and samples nothing while
     the last answer is unenrolled. Both need no new
     syscall, request or `unsafe`. A lid close or a resume during an
     open lifetime ends it as Escape does, so an unlock, enrollment,
     addition or removal is cancelled before its commit and drains
     without its result after it. Host tests replay `SW_LID` events and
     clock gaps through the adapter: the lid on each state and profile,
     its admission and classification, its reports kept from every
     client, gaps over and under two seconds, discarded and unreadable
     samples, the state rule, a batch, a repaint, focus and input held
     until the lock, a lost device's releases held until it, a refused
     lock retaken, the monitor's period and its lock with every reader
     gone, both triggers during an unlock and a removal before and after
     the commit, and the pin. A real lid and
     a real suspend are the hardware evidence's ("Evidence"); QEMU's
     S3 is `qemu-login-system`'s (C11).
   - C10b, landed, "login: deployments carry the tier marker": the
     system recipe writes the marker, listing the build's `READS`, into
     the deployment initramfs as "Deployments" places it, so every
     deployment from C10b onward carries it; a recipe test holds the
     constant to `login_record.rs`, the spec and the write before the
     pack, and the build's archive check finds it in the deployment
     initramfs and not the selector's. It follows C10 because the
     marker claims the deployment honours the record at every entry
     point, which holds only once C5, C6, C7, C9 and C10 have landed: a
     deployment built between C4 and C10 that carried it would pass
     increment 5's enrollment check as `current` or `previous` and,
     rolled back to, boot open. From it, on an enrolled or unavailable
     machine, request 19 admits a marked deployment that reads the
     record's version (any marked one while the record cannot be read),
     and the worker reads the record versions of each retained
     deployment that carries the marker. Every deployment ID changes,
     since the initramfs does, so its landing runs `check integration`
     by hand.
   - C11, landed, completing increment 4: the `qemu-login-system`
     guest (below; `td-secret/DESIGN.md`, "Login system guest"), which
     depends on C10b's marked deployments: they are what its install
     and refusal cases tell apart. `td-recipe-eval qemu-login-system`
     boots the test-only system image once per phase on one disposable
     volume whose files are guest root's, as an installation's are, so
     the worker's production read of the retained deployments' markers
     sees this build's version. It changes no production behaviour:
     the guest, its host oracle, a direct q35 boot with S3 for it, and
     in the test-only image the fixture unit's longer readiness bound
     and a reboot, not a power off, to end these boots, since the q35
     chipset QEMU resets at the wake no longer powers down.

   C7 lands only after the update-consent refusal (C4), the console
   refusal (C5) and the enforced SSH render (C6), so no build locks the
   screen while an update, the console or SSH could pass it. C10b landed
   only after C10, so no deployment carries the marker before it
   honours the record at every entry point. Each commit
   before C7 changes behaviour only where a record or an invalid
   directory exists, apart from C1's directory itself and C3's text for
   `D` without a list (above). Fixtures seed records through the worker
   with simulated root acknowledgements, as `login_vm.rs` does
   ("Evidence"), with both retained deployments taken to read this
   build's version, since production td-authd refuses every write until
   increment 5.
   - `login-desktop` (C8), increment 3's desktop guest: a diskless,
     TPM-free `qemu-secret` login case in the desktop harness without
     its TPM measurement, with a UHID keyboard and a UHID key, and a
     record the worker enrolled before the paired generation starts.
     The first frame is the lock surface; the chord, the PIN typed on
     the keyboard and the key's touch unlock it through the production
     authority and worker; a wrong PIN and a key not in the record stay
     locked; a damaged directory locks with its cause and its chord
     sends nothing; a restarted pair locks again, and an unenrolled one
     does not. The host checks the framebuffer at each step with QMP
     `screendump`: the lock surface's rows, the PIN step's prompt and
     the field's masks, no client pixel while locked, and the desktop
     once unlocked. Escape after the commit is the compositor's host
     tests' ("Evidence").
   - `qemu-login-system` (C11): a full-system, TPM-free guest on a
     disposable volume: a cold boot whose first frame is the lock surface,
     whose serial greeter prints the exact refusal line and starts no
     shell, which boot health still accepts, and which each seeded key
     unlocks; `Super+l` and `L`, unlock, relock after killing the
     compositor; suspend to RAM in a q35 machine with S3 enabled
     (`ICH9-LPC.disable_s3=0`), held suspended at least 10 seconds and
     woken over QMP `system_wakeup`, whose first post-wake input, the
     chord, in one HID report, starts the boot's one login worker since
     before the suspend under the same compositor and authority, which
     the chord does only on a locked session: the session was locked
     when the first post-wake input was routed, in the same generation.
     After the display's first change following the wake it shows only
     lock pixels or QEMU's inactive output, which it must show: Linux 7.1.4's virtio-gpu has no freeze or restore, so
     the card the wake resets stays dead and the lock surface itself is
     not seen; the unavailable state, with
     its screen text and the enforced SSH form, for a wrong-mode,
     wrong-owner and non-directory `/var/lib/td/login` and for a record
     with the wrong mode, two links, truncated bytes or an unknown version,
     each repaired by the documented recovery and followed by a normal
     boot; with the `inspect-login` helper made to fail, an unenrolled
     image that boots unlocked with the ordinary SSH policy, and a seeded
     record that shows `STATE COULD NOT BE READ` and resolves to enrolled
     once the helper answers; refusal to install a marker-less deployment
     and a marked deployment that does not read the record's version,
     and admission of a copy of the booted, C10b-marked deployment to
     its prompt, which is cancelled. The could-not-be-read case restarts
     the enrolled boot's pair with the helper failing, rather than
     booting with it.
   - Both guests run by hand, on KVM only, in neither `check` nor
     `check integration`.
   - Compositor input tests replay recorded `SW_LID` events and clock gaps
     through the adapter; QEMU has no lid.

   Shared code lives in one source file each, compiled by its other
   consumers through a reviewed `#[path]` as td-authd already compiles
   `td-firstboot/src/principals.rs`, and staged by each consumer's target
   recipe at the same relative path (the compositor's under `auth/`,
   behind its `cfg_attr` pair, as it stages `td-authd/src/consent.rs`):
   - `td-secret/src/login_state.rs`, the predicate, compiled by
     td-firstboot, td-login (`THREAT-MODEL.md` §3) and td-authd;
   - `td-secret/src/login_tier.rs`, std-only: the marker's grammar and
     the bounded newc reader with its file checks ("Deployments"); the
     worker's own module, and `#[path]` in td-authd. It hashes with the
     `engine/src/sha256.rs` copy each already compiles, so
     `TARGET_INCLUDED_ENGINE_SOURCES` (`td-install/DESIGN.md` §7) gains
     no source;
   - `td-firstboot/src/hostname.rs`, `Hostname::parse`: in td-authd for
     the hostname it sends, and in the compositor for its `9a`
     admission;
   - `td-authd/src/primary_account.rs`'s `validate_name`: in the
     compositor for its `9a` admission, the rest of that file under a
     reasoned `dead_code` allowance, as td-secret compiles the shared
     hash.

**Prerequisite:** the §L.1 elevation workstream through its L7
(`APPLICATIONS.md` §L.1, "Elevation increments"), which has landed, as
"Enrollment requires §L.1 elevation" above requires.

5. **Activation:** enrollment UI and automatic-login cutover, the
   in-boot SSH render (`td-firstboot render-ssh-policy`), the `stop=leaf`
   key and `sshd`'s use of it (`td-svc/DESIGN.md`), the `K` screens, the
   disclosures with their approval key, addition and removal, the
   deployment-marker check on enrollment, and revocation with its reboot
   guard, which returns the greeter's line to root:root with the pinned
   mode before it restarts the greeter ("Cutover"). It lands only after
   the prerequisite, and its oracle uses the §L.1 operations; this
   activates the tier.
   - Full-system QEMU: both enrollment choices through the attention
     screen; enrollment in a boot whose serial session was logged in and
     runs a process that ignores TERM, and in which a primary SSH
     session is open, followed by observation that both sessions and
     every process in their containments are gone and the greeter
     refuses; a root SSH attempt, refused before enrollment (§L.1's L7)
     and after; a cold reboot that is locked with the enforced SSH form;
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

   Increment 5 is specified as five commits, A1 to A5, in landing
   order. Only A5 admits a write. Before it, a cutover arises only when
   the login state changes within a boot, which in production only
   damage to the directory or the record does. A1's `sshd` stop and A2's
   atomic publication and record are live on every machine and change
   no admitted login. A1 and A2 each add their live piece to AGENTS.md
   principle 7's list and to this document's opening when they land.
   - A1, landed: td-svc `stop=leaf` (`td-svc/DESIGN.md`):
     - the key, and its validation: `cgroup=service`, no `tty=`, not
       `pair-exec`;
     - the leader's launch behind the pair's start gate, so every
       process of an instance starts in the leaf;
     - a requested stop, restart or shutdown that TERMs the leaf's
       members, kills the whole leaf at the KILL deadline and waits for
       it to empty, through the pair's `cgroup.kill` and
       `cgroup.events` reader; the stop path opens the controls at the
       stop, since a leaf whose leader restarted in place is not empty;
     - a leader crash, which restarts the leader in place;
     - the shipped `sshd` unit, which sets it.

     Stopping `sshd` now ends the OpenSSH sessions in its leaf, as
     shutdown already did by killing everything. A machine where
     `sshd`'s placement fails does not start it, as for a pair. The
     evidence is table and supervisor fixtures, host tests of the
     trampoline's grant, and a guest whose leader forks a session child
     in its own session before anything else and holds it through a
     crash and ends it at a restart.
   - A2, landed: firstboot's two renders:
     - Stage-1's `td-firstboot render-primary-sshd /sysroot` publishes
       `/sysroot/run/td-sshd.conf` itself, a root:root mode-0600
       temporary renamed into place, in place of the shell's truncating
       redirect. It then writes the volatile record
       `/sysroot/run/td-login-cutover` for the form it rendered.
       `td-authd/DESIGN.md`'s amendment 7 gives the record's bytes,
       writers and reader once.
     - The new fixed verb `td-firstboot render-ssh-policy` takes no
       operand. It renders under `/` for a running boot, publishes the
       policy the same way, and prints the form it published. It writes
       no record; A3's cutover runs it.

     A2 amends `THREAT-MODEL.md` §1's render and serialization
     statements to match.
   - A3, landed: td-authd's revocation and reboot guard, amendment 7 as stated
     there: the check and when it runs, the cutover beside the operation
     slot, the line's hand-back, `9a`'s revocation byte, the reboot and
     the guard. The compositor decodes the byte, polls `1a` every 250 ms
     while it reads `01`, and shows `02`'s and `03`'s texts. A recipe
     test pins the line constant to the greeter unit's `tty=`. On a
     stock machine the boot's record names the unenrolled state, so the
     check changes nothing.

     Host child fixtures with a scripted td-svc cover the host-fixture
     items of the failure injection listed above. Its guest items, the
     authority killed between publication and restart and the cut
     followed by a boot, are A5's. `qemu-login-system`'s seed phase ends
     its serial session with a cutover rather than its own `stop
     greeter`: the greeter is stopped only while the unenrolled screens
     need the line the host answers on, then started again, and after
     seeding a restarted pair finds the record and the state different.
     The cutover ends the logged-in serial session and an open SSH
     session, and leaves the enforced form, the record naming it and a
     greeter that refuses.
   - A4, landed: the disclosures' approval key, inert in production:
     - Only a disclosure step carries the key. That is the first step
       of a first enrollment (its first `connect`), and the first step
       of a removal that leaves at most one key (`identify`).
       Root draws it beside the operation's nonce and puts it on its own
       first step; the worker repeats that description exactly.
     - Both are presented before any token I/O, and each disclosure
       belongs to the whole operation. So one key confirms the
       operation, and no later step carries one.
     - The key's two bytes are drawn as an elevation's are and are the
       description's last, after the step's (`td-authd/DESIGN.md`,
       "Elevation operations"). The decoder requires them on such a step
       and refuses them on any other.
     - The step's rows end with the disclosure ("Enrollment, addition
       and removal") and then the key rows.
     - The compositor sends that step's presentation receipt only once
       the key is typed under its "Elevation consent" rules. The receipt
       carries the exact description, so it carries the key. Enter and
       a wrong digit act as they do on an elevation's prompt.
     - Root waits for that receipt, and the worker for root's
       acknowledgement, until the operation's deadline rather than the
       usual three and five seconds. A disclosing operation's ceiling
       adds a reading allowance, the same for root, the worker and the
       compositor (`td-authd/DESIGN.md`, amendment 6).
     - The widest login value is unchanged: the authorize step of an
       eight-key removal, which carries no key (`td-authd/DESIGN.md`,
       "Immutable consent description prerequisite").
     - A disclosure prompt an output cannot hold whole is refused with
       a notice before any token I/O; which outputs hold each is
       `td-compositor/DESIGN.md`'s ("Elevation consent").
     - The evidence is literal vectors, host tests (among them a key
       typed after ten seconds still proceeds, and a receipt at root's
       deadline, defence in depth behind the compositor's own deadline,
       ends as TIMEOUT) and device-dispatcher tests, as for L3's key.
   - A5, activation:
     - `login::WRITES` becomes true, so production admits enrollment,
       addition and removal.
     - The documents' statements of what is implemented follow. None
       describes the tier as usable before the hardware evidence below.
     - Its oracle is this item's full-system QEMU list, plus the
       guest-side failure injection. These run as further
       `qemu-login-system` phases that drive the attention screen over
       QMP.

   A5 lands only after A3 and A4, so no build admits a write whose
   cutover or disclosure is missing.

Hardware evidence on the T430s follows increment 5 and precedes any claim
that the tier is usable. Follow-ups: store release at login, idle-timeout
lock, a suspend initiator, built-in UV and PIN setup.
