# td-tpm: the shared TPM 2.0 client

td-tpm is td's shared TPM 2.0 client. AGENTS.md principle 2 puts code
two crates need in one sibling crate, so td-secret's credential stores
and td-protector, the disk protector of `td-install/ENCRYPTION.md`
increment 4, run over this crate instead of each carrying a copy.
td-boot's selector PCR 11 measurement reads, extends and reads back
through `read_pcr` and `extend_pcr`; the client's unit tests and
td-boot's pin those exact command bytes. It is pure `std`, depends on
no crate, compiles the engine's SHA-256 as shared source, and forbids
`unsafe`; it adds no syscall surface to `UNSAFE.md`. The device is
opened through safe file I/O.

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
  32-byte caller nonce from `/dev/urandom`. A command is sized and
  checked against `MAX_PACKET` before its parameters are copied. Buffers
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

## Planned: protected-tier commands

Nothing here is current. `td-install/ENCRYPTION.md` increment 8 (8a,
and 8i for signing) adds, still as bytes over the same safe file I/O and
with no syscall surface. The crate then depends on one sibling,
`td-fido = { path = "../td-fido" }`, for P-256 and AES, and on no other
crate; "It is pure `std`, depends on no crate" above then reads "on
td-fido alone".

- **Salted sessions.** A policy session may be salted to the storage
  primary: td-tpm draws an ephemeral P-256 key, sends its public point
  as TPM2_StartAuthSession's `encryptedSalt`, derives the salt by KDFe
  from the shared point with the primary's public point, and derives
  the session key by KDFa from the salt and both nonces. td-fido's AES
  gains the AES-128 key schedule for it. Such a session sets
  `decrypt` and `encrypt` with AES-128 in CFB mode, so a command's first
  sensitive parameter and a response's are encrypted under keys KDFa
  derives from the session key and authValue. The primary's Name is
  returned for the consumer to compare with the one it recorded.
- **HMAC policy sessions.** A policy session may carry PolicyAuthValue,
  after which `call` computes the command HMAC, HMAC-SHA256 keyed with
  the session key followed by the object's authValue with its trailing
  zero bytes removed, over cpHash, the two nonces and the session
  attributes, and verifies the response HMAC over rpHash before it
  trusts or decrypts a reply. The authValue is the caller's zeroing
  owner, borrowed for the command.
- **Sealing with an authValue.** `seal_object` takes an optional
  authValue for TPM2B_SENSITIVE_CREATE's userAuth, sent encrypted in a
  salted session, and the consumer's attributes, so a consumer can seal
  with noDA clear.
- **Typed authorization refusals.** `UnsealError`'s refusal names
  `TPM_RC_AUTH_FAIL` on a session (0x98e for session 1) and
  `TPM_RC_LOCKOUT` (0x921) as their own kinds.
- **Dictionary-attack state.** TPM2_GetCapability of
  `TPM_CAP_TPM_PROPERTIES` for `TPM_PT_PERMANENT`,
  `TPM_PT_LOCKOUT_COUNTER`, `TPM_PT_MAX_AUTH_FAIL`,
  `TPM_PT_LOCKOUT_INTERVAL` and `TPM_PT_LOCKOUT_RECOVERY`, each reply
  checked for exactly the properties asked.
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

Sessions are neither salted nor parameter-encrypted. Physical TPM-bus
interposition is outside every current consumer's boundary; the planned
protected tier salts its sessions (above, and `td-install/ENCRYPTION.md`,
"PIN and dictionary-attack policy").

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
