# td-tpm: the shared TPM 2.0 client

td-tpm is td's shared TPM 2.0 client. AGENTS.md principle 2 puts code
two crates need in one sibling crate, so td-secret's credential stores
and the disk protector of `td-install/ENCRYPTION.md` increment 4 run
over this crate instead of each carrying a copy. td-boot's selector
PCR 11 measurement reads, extends and reads back through `read_pcr` and
`extend_pcr`; the client's unit tests and td-boot's pin those exact
command bytes. It is pure `std`, depends on no crate, compiles the
engine's SHA-256 as shared source, and forbids `unsafe`; it adds no
syscall surface to `UNSAFE.md`. The device is opened through safe file
I/O.

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
  copy, and are zeroed on every return path; the payload
  `unseal_object` returns is the caller's to zero. A refused command is
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
  as it is marshaled. `unseal_object` validates the stored public area,
  loads the object under the same primary, checks its Name, unseals in a
  real policy session and returns the payload for the caller to zero.
- **PCR read and extend.** `read_pcrs` returns the selected values in
  ascending order after checking the bank, bitmap and count; it does not
  judge the values. `read_pcr` reads one. `extend_pcr` extends one with
  a SHA-256 event digest and accepts only the exact empty
  password-session reply. Both take a `u8` index and refuse one outside
  the `PcrSelection` range.

## What belongs to consumers

Envelope formats, what a payload contains, which PCRs a policy selects,
whether an unmeasured or nonzero PCR is acceptable, the order of release
and capping steps, and every recovery decision belong to the consumer
and its normative document. td-secret's `TDTPM001` and `TDBOUND1`
envelopes, its owner-UID payload prefix and its all-zero PCR refusal are
specified in `td-secret/DESIGN.md`; the disk protector's tokens, PCR 12
cap and recovery flow in `td-install/ENCRYPTION.md`.

Sessions are neither salted nor parameter-encrypted. Physical TPM-bus
interposition is outside every current consumer's boundary.

## Bounds

Commands and replies are at most 4096 bytes (`MAX_PACKET`); a reply's
size field must equal its length. Every reply field is read through a
bounded reader that refuses truncation and trailing bytes. Sealed
payloads hold 1 to 128 bytes, TPM2B_SENSITIVE_DATA's guaranteed
capacity. Production code returns errors; it does not panic.

## Evidence

Unit tests pin the policy digest literal, selection marshalling, the
single-PCR read and extend command bytes, malformed and refused replies,
handle ownership across failed flushes, primary personalization,
payload and public-area bounds before any TPM I/O, and, against a
scripted TPM that evaluates the policy itself, seal then unseal through
the public API (including a policy over a PCR value supplied by the
caller rather than read), an Unseal refused for a mismatched session
policy, and extend then read. td-secret's tests pin
the complete seal and unseal command stream against a scripted TPM and
its persisted envelope bytes, and its pinned-emulator and QEMU guest
oracles exercise this client against a TPM.
