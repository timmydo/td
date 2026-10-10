# td-tpm: the shared TPM 2.0 client

td-tpm is td's shared TPM 2.0 client. AGENTS.md principle 2 puts code
two crates need in one sibling crate, so td-secret's credential stores
and td-protector, the disk protector of `td-install/ENCRYPTION.md`
increment 4, run over this crate instead of each carrying a copy.
td-boot's selector PCR 11 measurement reads, extends and reads back
through `read_pcr` and `extend_pcr`; the client's unit tests and
td-boot's pin those exact command bytes. It is pure `std`, depends on
one sibling crate, `td-fido = { path = "../td-fido" }`, for the P-256,
AES and HMAC-SHA256 of its salted sessions, compiles the engine's
SHA-256 as shared source, and forbids `unsafe`; it adds no syscall
surface to `UNSAFE.md`. The device is opened through safe file I/O.

## API boundary

The crate owns the protocol and nothing a consumer persists or decides:

- **Transport.** `Transport` exchanges one complete command for one
  reply. `Device` opens `/dev/tpmrm0` without following links and
  refuses anything but a character device; the kernel resource manager
  owns the connection lifetime. `Device::open_io` is the same open
  keeping the OS error kind, which td-boot reports. Tests substitute
  scripted transports.
- **Commands.** `Client::call` marshals one command with at most one
  session: an empty password session or a policy session with a fresh
  32-byte caller nonce from `/dev/urandom`. A command in a salted
  session takes its own path ("Salted and PIN-authorized sessions"). A
  command is sized and checked against `MAX_PACKET` before its
  parameters are copied. Buffers
  that carry secrets (the sealed sensitive area, the Create parameters,
  `call`'s command and reply buffers, and the Unseal reply parameters)
  are allocated once at their final size, so no reallocation leaves a
  copy, and are zeroed on every return path with `zero`, which keeps the
  stores observable through `black_box` so they are not elided before
  the buffer is freed. This is best effort in safe Rust. The payload
  `unseal_object` returns is the caller's to zero. `zero` is public, as
  is `equal`, which compares two fixed-length secrets with no early exit
  at the first difference (best effort, as `zero` is), so td-boot,
  td-protector and td-secret share one copy of what they use. A refused
  command is
  an error naming its code and response code; there is no retry or
  fallback.
- **Handles.** Returned handles are owned by the client until `flush`,
  until Unseal consumes a policy session, or until drop, which flushes
  every handle still owned. A flush the TPM does not confirm leaves the
  handle owned, so drop tries again.
- **Storage primary.** `storage_primary` creates the deterministic ECC
  P-256 restricted decryption primary under an empty-authorization owner
  hierarchy, optionally personalized by a 32-byte `unique.x`. The
  returned template, point extents, creation ticket and Name are
  checked. `bound_storage_primary` retires the unpersonalized primary
  and refuses a personalized one with the same Name.
- **PCR policy.** `PcrSelection` is one SHA-256 bank selecting one to
  eight of PCRs 0 through 15, marshaled as TPML_PCR_SELECTION with a
  three-byte bitmap. That range, with at most eight selected, is the
  client's bound on the selection it marshals and the PCR_Read reply it
  accepts; which PCRs to use remains consumer policy. `policy_digest` is
  PolicyPCR over the selection and composite digest, then
  PolicyCommandCode(Unseal), from the zero digest. `policy_session`
  builds that policy in a trial or real session and refuses a TPM
  PolicyGetDigest that differs from the local digest.
- **Seal and unseal.** `seal_object` creates a keyed-hash object with
  `SEALED_ATTRIBUTES` (fixedTPM, fixedParent, adminWithPolicy, noDA;
  userWithAuth clear), the policy digest as its sole authorization, and
  a 1 to 128-byte payload. The caller's payload buffer is zeroed as soon
  as it is marshaled. `unseal_object` validates the stored public area's
  format, loads the object under the same primary, checks its Name,
  unseals in a real policy session and returns the payload for the
  caller to zero. Whether the object's authPolicy is the session's
  policy is the TPM's to answer (Unseal's `TPM_RC_POLICY_FAIL`), not a
  local check, so that its `UnsealError` can carry, as a `Refusal`, the
  command and response code of the TPM refusal that ended the unseal,
  when one did; a consumer types its refusals from that
  (td-protector/DESIGN.md "Unseal outcomes").
  `load_and_flush` loads a public and private pair under that primary,
  checks the Name and flushes the object and the primary, so the TPM
  verifies the private area without an unseal; the public area's format
  is the caller's to check. `load` is its Load alone, under a parent
  the caller created, the handle owned until `flush` or drop, for a
  caller that must tell which command failed: td-protector's chain
  check (td-protector/DESIGN.md "PIN and tpm-pin policy").
  `last_refusal` is the `Refusal` of the last command the client sent
  if the TPM refused it, and `None` after a command it answered, a
  transport error or a malformed reply; every command, a flush included,
  resets it, so a caller reads it straight after the failed call.
- **PCR read and extend.** `read_pcrs` returns the selected values in
  ascending order after checking the bank, bitmap and count; it does not
  judge the values. `read_pcr` reads one. `read_pcrs_typed` and
  `read_pcr_typed` return a `PcrReadError` that separates
  `NoSha256Bank`, the answer of a TPM with no SHA-256 bank allocated
  (the selection returned with no PCR selected, as the reference
  implementation filters an unallocated bank, or no selection, and no
  values), from every other failure. `extend_pcr` extends one with
  a SHA-256 event digest and accepts only the exact empty
  password-session reply. Both take a `u8` index and refuse one outside
  the `PcrSelection` range.

## Salted and PIN-authorized sessions

`td-install/ENCRYPTION.md` increment 8a adds these for the tpm-pin
protector ("PIN and dictionary-attack policy" there); nothing in
production calls them before item 9. They are bytes over the same safe
file I/O, `session.rs` and `auth.rs`; TPM 2.0 Part 1 specifies each
derivation.

- **Salted sessions.** A session is salted to the unpersonalized
  storage primary: td-tpm draws an ephemeral P-256 scalar from
  `/dev/random`, which blocks until the kernel's generator is
  initialized, as early boot needs (the scalar hides the salt, so unlike
  a nonce it is a secret), and sends its public point as
  TPM2_StartAuthSession's `encryptedSalt`, with the primary as tpmKey
  and `TPM_RH_NULL` as bind. The salt is KDFe with SHA-256 over Z, the
  x-coordinate of the ECDH point td-fido's `SecretScalar::agree`
  computes, the label `SECRET` with its zero octet, the ephemeral x as
  PartyUInfo and the primary's x, as CreatePrimary's checked outPublic
  returned it, as PartyVInfo. The session key is KDFa (HMAC-SHA256) over
  the salt with `ATH`, nonceTPM then nonceCaller. The session's
  symmetric definition is AES-128 in CFB mode and its hash SHA-256;
  td-fido's `fido_aes` carries AES-128 and CFB beside its AES-256-CBC.
  A started session's handle must be the class its kind takes, 2 for
  an HMAC session and 3 for a policy or trial session; another is
  refused and stays owned for drop's flush.
- **Session commands.** A command in a salted session carries one
  authorization: a fresh 32-byte nonceCaller from `/dev/urandom`, the
  attributes, and the HMAC, HMAC-SHA256 keyed with the session key
  followed by the authValue with its trailing zero octets removed
  (`trim_auth`, as the TPM stores an authValue), over cpHash (the
  command code, the handles' Names, and the parameter area as sent),
  nonceCaller, nonceTPM and the attributes. With `decrypt` the data of
  the first command parameter, a TPM2B, is encrypted with AES-128-CFB
  under the key and IV KDFa derives from that same session value with
  `CFB`, nonceCaller then nonceTPM; with `encrypt` the first response
  parameter's, nonceTPM then nonceCaller. The reply's HMAC, over rpHash
  (success, the command code and the parameter area as received), the
  new nonceTPM, nonceCaller and the echoed attributes, is compared with
  `equal` before anything in the reply is trusted or decrypted, and the
  new nonceTPM replaces the old. Every session command td sends clears
  continueSession, so the TPM ends the session with the command; td
  stops owning it only once a success reply has verified, so a refused,
  malformed or unverified reply leaves it owned for drop's flush. The session key,
  salt and derived keys are zeroed on drop or once used; the authValue
  stays the caller's to zero, borrowed for the command.
- **PolicyAuthValue.** `auth_policy_digest` and `PcrPolicy::auth_digest`
  are PolicyPCR, then PolicyAuthValue, then PolicyCommandCode(Unseal),
  from the zero digest: the tpm-pin policy. `policy_digest` is
  unchanged.
- **Sealing with an authValue.** `seal_with_auth` seals a 1 to 128-byte
  payload under `auth_digest` beneath the unpersonalized storage
  primary, with an authValue of at most 32 bytes (`MAX_AUTH_VALUE`, the
  nameAlg's digest size) that is not empty once trimmed, trimmed, as
  TPM2B_SENSITIVE_CREATE's userAuth, and the consumer's attributes,
  `SEALED_ATTRIBUTES` or `DA_SEALED_ATTRIBUTES` (noDA clear, so a wrong
  authValue counts against the TPM's lockout). Anything else is
  refused, and the payload zeroed, before any TPM I/O. A trial session
  first checks the TPM's own PolicyGetDigest. It creates the primary
  and, given the Name a consumer recorded (`expected_primary`, which a
  re-seal under an existing token passes), refuses another before any
  session is salted to it, so a primary that appeared after enrollment
  never receives the new authValue and payload to test PINs against
  offline; a first seal passes none and records the Name returned. The
  Create runs in an HMAC session salted to the primary, authorizing the
  primary (whose authValue is empty) with `decrypt`, so the authValue
  and payload cross the bus encrypted; the
  payload buffer is zeroed as it is marshaled, as `seal_object` does. It
  returns the sealed pair and the primary's Name, `AuthSealed`, for the
  consumer to record. The authValue path is its own method rather than a
  new `seal_object` signature, so the PCR-only path and its consumers
  are unchanged.
- **Unsealing with an authValue.** `unseal_with_auth` takes the policy,
  the recorded primary Name, the authValue and the pair. Before any I/O
  it refuses an authValue over 32 bytes or empty once trimmed, and a
  public area outside the
  two sealed formats (`validate_auth_sealed_public` checks one against a
  policy for a consumer). It creates the primary and refuses one whose
  Name is not the recorded one before Load, so no authValue is ever used
  under another primary; loads the object; starts a policy session
  salted to the primary; runs PolicyPCR, PolicyAuthValue and
  PolicyCommandCode(Unseal), checked against PolicyGetDigest; and sends
  Unseal with `encrypt` alone, since Unseal has no command parameter, so
  the payload crosses the bus encrypted. The caller zeroes the payload.
- **Typed authorization refusals.** `Refusal::authorization` and
  `UnsealError::authorization` name `TPM_RC_AUTH_FAIL` on session 1
  (`RC_AUTH_FAIL`, 0x98e; td sends one session), a wrong authValue the
  TPM counted, as `AuthRefusal::AuthFail`, and `TPM_RC_LOCKOUT`
  (`RC_LOCKOUT`, 0x921) as `AuthRefusal::Lockout`. Every other code is
  none, `TPM_RC_BAD_AUTH` (0x9a2) included, which a noDA object's wrong
  authValue returns uncounted. The TPM compares a policy session's
  digest with the object's authPolicy before it checks the HMAC, so a
  session that reaches Unseal over another chain is `TPM_RC_POLICY_FAIL`
  (0x99d) and costs no attempt. An error response carries no session
  HMAC, so an interposer can forge either kind: a typed refusal is
  grounds to stop, never proof the PIN was wrong or the TPM locked.
- **Dictionary-attack state.** `dictionary_attack` reads
  TPM2_GetCapability of `TPM_CAP_TPM_PROPERTIES` twice, for
  `TPM_PT_PERMANENT` alone and for the four from
  `TPM_PT_LOCKOUT_COUNTER` (`TPM_PT_MAX_AUTH_FAIL`,
  `TPM_PT_LOCKOUT_INTERVAL`, `TPM_PT_LOCKOUT_RECOVERY`), each reply
  holding exactly the properties asked for, in order, whatever its
  moreData; it returns TPMA_PERMANENT's lockoutAuthSet and inLockout and
  the four values. It needs no authorization, and its replies carry no
  HMAC: what it reports can deny service or mislead a display, never
  prove the TPM's state.

## Planned: protected-tier commands

Nothing here is current. `td-install/ENCRYPTION.md` increment 8 (8a's
next commit, and 8i for signing) adds, still as bytes over the same safe
file I/O, in the sessions above, and with no syscall surface:

- **Lockout hierarchy.** TPM2_HierarchyChangeAuth of `TPM_RH_LOCKOUT`
  from the empty authorization to a caller's value,
  TPM2_DictionaryAttackParameters, TPM2_DictionaryAttackLockReset and
  TPM2_Clear under it, each in a salted HMAC session, the new
  authorization sent encrypted.
- **Signing keys (8i).** Creating an RSA-2048 signing key under the
  storage primary with a policy and an authValue, transient or kept by
  the caller; TPM2_Sign with RSASSA over a caller's SHA-256 digest;
  PolicyOR; and TPM2_ObjectChangeAuth, which returns a new private area
  under a new authValue.

Which PCRs, PINs, parameters, Names and keys these serve stays the
consumer's.

## What belongs to consumers

Envelope formats, what a payload contains, which PCRs a policy selects,
whether an unmeasured or nonzero PCR is acceptable, the order of release
and capping steps, and every recovery decision belong to the consumer
and its normative document. td-secret's `TDTPM001` and `TDBOUND1`
envelopes, its owner-UID payload prefix and its all-zero PCR refusal are
specified in `td-secret/DESIGN.md`; the disk protector's policies,
secret and PCR 12 cap in `td-protector/DESIGN.md`, and its tokens and
recovery flow in `td-install/ENCRYPTION.md`.

The sessions of `seal_object`, `unseal_object` and `load_and_flush`
are neither salted nor parameter-encrypted: physical TPM-bus
interposition is outside every current consumer's boundary. The
protected tier's PIN path salts its sessions (above, and
`td-install/ENCRYPTION.md`, "PIN and dictionary-attack policy").

## Bounds

Commands and replies are at most 4096 bytes (`MAX_PACKET`); a reply's
size field must equal its length. Every reply field is read through a
bounded reader that refuses truncation and trailing bytes. Sealed
payloads hold 1 to 128 bytes, TPM2B_SENSITIVE_DATA's guaranteed
capacity. Production code returns errors; it does not panic.

## Evidence

Unit tests pin the policy digest literal, selection marshalling, the
single-PCR read and extend command bytes, malformed and refused replies,
an absent SHA-256 bank typed apart from another bank or a malformed reply,
handle ownership across failed flushes, primary personalization,
payload and public-area bounds before any TPM I/O, and, against a
scripted TPM that evaluates the policy itself, seal then unseal through
the public API (including a policy over a PCR value supplied by the
caller rather than read), an Unseal refused for a mismatched session
policy, `unseal_object`'s refusal carried for PolicyPCR, Unseal and
Load, none for a public area outside the format (refused before any
I/O) or a transport error, `load_and_flush` leaving no handle and
refusing a private area the TPM will not load, `load` owning its handle
and `last_refusal` naming a refused Load until the next command and
nothing after a transport error, and extend then read. td-secret's tests pin
the complete seal and unseal command stream against a scripted TPM and
its persisted envelope bytes, and its pinned-emulator and QEMU guest
oracles exercise this client against a TPM.

The salted and PIN-authorized sessions' unit tests pin, against
independent vectors (`tests/session_vectors.txt`, printed by
`tests/session_vectors.py`, a stdlib-only host fixture tool with integer
P-256 and a table-free AES-128 it checks against FIPS 197 and SP 800-38A
before printing), the salted session's ECDH point, KDFe salt, session
key and StartAuthSession bytes for an HMAC and a policy session;
AES-128-CFB parameter encryption both ways, each with its own KDFa key;
the whole Create command in the salted HMAC session, its sensitive area
encrypted, and its reply; the whole Unseal command with the authValue
and its encrypted reply, a reply changed at any byte, under another
authValue or with other attributes refused; the PolicyAuthValue digest
literal and command bytes; authValue trimming; GetCapability's two
commands and its exact-properties check; and, against a scripted TPM,
the typed `TPM_RC_AUTH_FAIL` and `TPM_RC_LOCKOUT` replies at Unseal,
another primary's Name refused before Load at unseal and before any
salted session at seal, an authValue empty once trimmed refused before
I/O, each session kind's handle class, and a session left owned after a
forged or malformed success reply. td-fido's tests pin AES-128
against FIPS 197 and CFB against SP 800-38A.

Ignored oracles against the pinned swtpm 0.10.1 (`emulator_tests.rs`:
`TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --manifest-path
td-tpm/Cargo.toml emulator_ -- --ignored`) seal and unseal with the
right PIN in a salted session; refuse a wrong PIN with 0x98e and raise
the counter, which an orderly restart keeps, while a noDA object's wrong
PIN is 0x9a2 and uncounted; lock out after 32 wrong PINs with 0x921,
refusing the right PIN too, the DA parameters set by a raw
TPM2_DictionaryAttackParameters under swtpm's empty lockoutAuth until
the lockout commands are the client's; refuse a changed PCR 4 and a
closed PCR 12 at PolicyPCR and, in a session over the PCRs as they read,
at Unseal's policy check, without the counter moving; refuse a fresh TPM
state at Load; refuse another primary's Name before Load; re-seal under
the recorded primary's Name; and refuse, under a fresh TPM, a re-seal
recording the first TPM's primary before its Create is sent.
