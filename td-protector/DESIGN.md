# td-protector: the device-bound disk protector

td-protector is the disk-protector policy of
[ENCRYPTION.md](../td-install/ENCRYPTION.md)'s device-bound tier, carried
for both of its consumers: the installer, which seals and verifies the
first-boot protector (increment 5), and the selector, which releases, seals
the device-bound protector and caps PCR 12 (increment 6). It runs over the
shared TPM 2.0 client [td-tpm](../td-tpm/DESIGN.md) and owns only what
ENCRYPTION.md makes disk-specific: which PCRs a protector names, the
release cap, and the protector secret. It is pure `std`, depends only on
td-tpm, forbids `unsafe` and adds no syscall surface to `UNSAFE.md`.

Nothing here is a persisted volume format. The td-typed LUKS2 tokens that
carry sealed protectors, their bounded reader, keyslot naming, the
first-boot transition and the recovery flow belong to increments 5 and 6,
which amend this document with the token format they add.

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

## Release cap

`cap` reads PCR 12 and requires zero, extends it once with `cap_event()`,
the SHA-256 of `td/disk-protector/release-cap/v1`, and requires the exact
readback `SHA256(zero32 || cap_event())`. It never retries. Its error is a
typed `CapError`, each displayed with PCR 12 context:

- `AlreadyClosed`: PCR 12 read non-zero, so nothing was extended. Every
  protector policy requires PCR 12 at zero, so no TPM release is possible
  this boot. Increment 6 decides whether that routes to recovery.
- `Uncertain`: a PCR 12 read or the extension failed in transport or was
  refused, so the cap's state is unknown: a failed prior read leaves PCR
  12 unmoved but unverified, and a failed extension or readback may or
  may not have moved it. Either way release cannot be shown closed.
- `Mismatch`: the readback differs from the expected value.

The caller's contract follows ENCRYPTION.md's release order. The selector
tries every td token, up to the fixed bound; when the first-boot
protector alone released, it performs the first-boot transition's seal
and verification unseal; then it caps exactly once, whether or not any
unseal succeeded, before cryptsetup parses the header. An `Uncertain` or
`Mismatch` cap zeroes every released secret, refuses boot and requires a
platform reset. No other td component extends PCR 12.

## Installer check

`verify_first_boot_object` runs the first-boot policy in a TPM trial
session, which td-tpm refuses unless the TPM's PolicyGetDigest equals the
local digest, then requires the sealed object's public area to carry that
digest as its authPolicy with the fixed sealed attributes. It then loads
the public and private pair under the storage primary with td-tpm's
`load_and_flush`, so the TPM verifies the private area's integrity, and
flushes both. Nothing is unsealed: the live selector has already capped
PCR 12.

## Bounds

Every TPM exchange is td-tpm's: bounded commands and replies, no retry or
fallback, and errors rather than panics. This crate adds fixed selections
and a 32-byte payload length.

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
one exact read from `/dev/random`, through an injectable source path. An
ignored oracle runs the same lifecycle against the pinned swtpm under
td-secret's convention (`td-secret/DESIGN.md`, "TPM validation"):

```
TD_TEST_SWTPM=/absolute/path/to/swtpm cargo test --frozen --manifest-path td-protector/Cargo.toml emulator_ -- --ignored
```

A normal cargo pass with it ignored is not TPM integration evidence.
