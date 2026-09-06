# Disk encryption and session unlock

This is the normative target for the `disk-encryption-rolling` workstream.
It complements [deployment installation](DESIGN.md) and
[protected elevation](../APPLICATIONS.md#l1-elevation--consent-without-a-secret).
The current image remains unencrypted and auto-logs in. No increment may
describe enrollment, disk confidentiality, or protected login as shipped
until its complete boot and recovery path passes the acceptance tests below.

## Scope

Protect private data when a laptop is lost while powered off or locked:
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
separate work. Initial support is for fresh installs; no in-place conversion
or erasure claim about historical plaintext copies.

## Authentication and recovery

Default unlock is **TPM 2.0 plus PIN**. The PIN authorizes a hardware-held
secret with persistent dictionary-attack protection, not a short LUKS
passphrase susceptible to offline guessing. Release also requires an
approved measured boot state. TPM possession alone never logs a person in.
An enrolled **FIDO2 token plus PIN** is an alternative primary method and
the recovery method when the TPM is lost or replaced. Require the token's
`hmac-secret` capability and user verification; touch alone is insufficient.
These are alternative protectors, not a requirement to present both devices.

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

Authenticate the initial boot code and measure the selector, its initramfs
and command line before selector-stage release. That trusted selector
authenticates the selected deployment after opening the volume; it cannot
require a measurement of unreadable deployment bytes to unlock that volume.
Measure the verified deployment and its boot arguments before second-stage
release or handoff, with a policy that covers both stages explicitly.
Firmware/key provisioning and TPM policy-authorized updates must preserve
both `current` and the approved `previous` fallback. Exact-PCR enrollment
without an update/recovery policy cannot ship as the default.

The selector must unlock before reading a deployment. Its dm-crypt mapping
does not survive `kexec`: the deployment initramfs must create it again.
Before enabling encrypted boot, specify and prove either re-release under
the second kernel's policy or a bounded volatile key handoff to the verified
deployment. That protocol must preserve a single user interaction, verify
the original signed deployment bytes, and retire intermediate secrets. A
plaintext key on the command line or in a stored initramfs is forbidden.
This handoff is an explicit implementation gate, not existing machinery.

The authenticated user gets an ordinary session. Later elevation uses the
existing operation-to-principal policy and secure-attention path: one
request, one explicit approval, one broker-performed operation. Applications
receive no general root process or reusable authority. Protected consent is
the normal interaction; changing unlock credentials or recovery policy
requires fresh hardware-backed PIN verification bound to that operation.
No client surface, synthetic input, remote-control interface, or untrusted
same-uid process may impersonate the trusted UI or approve a request.

## Independently landable increments

1. This design and atomic reconciliation of the authentication contracts.
2. Built-in device mapper, dm-crypt and AES-XTS support in the target kernel,
   with checks against the realized kernel. This alone unlocks nothing.
3. Review the bounded LUKS2 userspace implementation and dependency closure.
   Source-built cryptsetup is the preferred candidate, but adding it or its
   dependencies needs explicit principle-2 sign-off and an amendment to
   DESIGN.md D6. This document grants neither. Do not replace that review
   with a new cryptographic disk format. New Rust syscall surfaces amend
   UNSAFE.md with their component contract in the same increment.
4. Add installer formatting, protector metadata and crash-safe enrollment;
   preserve file-image testing and the single deployment publisher. Replace
   the retained plaintext scratch-image path for private material, account
   for header space, and identify backing devices without `/dev/vda` pins.
5. In successive increments, add authenticated/measured boot, TPM PIN
   release and update policies, FIDO2 primary/recovery, and the verified
   `kexec`/account handoff. Exercise them together before activation.
6. Activate the encrypted profile only with trusted login/lock and operation
   consent, retiring auto-login and the administrative escape hatch
   atomically in that profile.

## Acceptance evidence

Use disposable QEMU disks and the existing pinned TPM emulator oracle; no
test touches an operator's disk or enrolls their hardware. Require a real
encrypted read/write roundtrip and reboot persistence; wrong PIN, missing
token and changed boot measurements must refuse release. Verify PIN retry
limits across process restart, a different TPM, primary-token loss, and
recovery with the original TPM unavailable. Exercise both primary methods,
interrupted formatting/enrollment, key replacement, update, failed-candidate
rollback, and the complete selector-to-session path with one interaction.
Record ciphertext/header checks and absence of plaintext secrets in scratch
artifacts. Prove that recovery cannot silently log in, lock cannot be bypassed,
and injected input or a replayed approval cannot authorize an operation.
Hardware FIDO2 interoperability evidence is required in addition to mocks.
