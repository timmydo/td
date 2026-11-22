# Website passkeys

This library stores device-bound ES256 website credentials for td-pass.
Enrollment takes an explicit loss decision, a PIN and the storage
primary Name established by td's proof of lockout ownership. Losing or
clearing that TPM loses access to every credential. The trusted UI must
present this consequence and a cancel choice before enrollment. Opening
an existing profile never enrolls or replaces it. A YubiKey's private
credentials stay on that key.

td-tpm seals a random 256-bit encryption key under the primary and the
salted PIN. Its DA-protected policy permits only Unseal. Each operation
that uses the TPM first checks its dictionary-attack configuration, and
reads the admitted primary, through td-tpm's shared `device_pin`
(td-tpm/DESIGN.md, "What belongs to consumers"), which the disk PIN and
td-secret's notebook keys of this computer's TPM also apply. A
protected caller supplies the private, owner-only directory by file
descriptor. The vault locks an independent description of the directory
for its lifetime. A file named passkeys contains the version, PIN salt,
primary Name, sealed public and private areas, nonce, ciphertext and tag.
ChaCha20-Poly1305 authenticates the header. Writes use a new random
nonce and a synced atomic replacement. An uncertain publication requires
reopening. An old copy of the entire file can restore an old snapshot
under the original TPM and PIN.

An unlocked session holds at most 256 records. Each binds the signing
scalar and random credential ID to the RP and user handle. The list
operation exposes account metadata. Registration obeys the exclude list;
a new discoverable credential replaces an existing discoverable one for
the same RP and user. The signing path checks the RP, allow list and
selected account again. Both operations require trusted user presence,
plus verification for discoverable registration and whenever the request
requires it. Firefox can preflight a known handle with the hash of empty
client data under that exact RP. That response has both flags clear and
omits the user handle. Signing a website challenge requires presence. The trusted caller owns
the protected process, request presentation, approval binding and session
revocation. Dropping the vault clears its plaintext and master key.
Authenticator data uses a zero signature counter and no attestation.

The unit test verifies a fresh assertion with the registration public key.
With swtpm 0.10.1 on PATH, run the persistent-store scenario with
`cargo test --manifest-path td-passkeys/Cargo.toml -- --include-ignored`.
It covers enrollment refusal, the PIN retry counter, encrypted storage,
reopening, site sign-in, deletion and corruption. It uses no hidraw
device.

The device transport accepts the unnumbered 64-byte FIDO HID reports.
Every ceremony receives a private transaction identifier. Cancellation,
resynchronization or an absolute deadline invalidates the identifier.
Only parsed website operations reach a worker. The transport handles
initialization, echo and authenticator information locally. Its UHID
descriptor lives with the root session owner. Only the kernel virtual
device carrying its random identity can be published to Firefox.

The guest-tests feature builds a separate ignored case,
`tests::kernel_passkey_registration_and_sign_in`, for a disposable KVM
guest with td's kernel, a fresh emulated TPM and the explicit
`td.passkey-guest=1` boot option. It takes that test TPM's lockout
hierarchy. A hidraw client running under workload UID and GID 65536 performs registration, a handle check and
sign-in through the kernel's UHID device, reopening the TPM vault between
operations and verifying the signature independently. This protocol
client supplies the test approvals.

The worker distinguishes its private control endpoint from the HID
interface. Control frames select a backend, approve one ticket, submit a
protected PIN, select a site-matched account, cancel a request, or lock
the vault. Only explicit acceptance at registration can enroll the
TPM vault. A website assertion checks the accepted ticket again. The
worker clears the input containing a PIN. A hardware operation pins its
attachment from the revision of the hotplug list shown to the human, checks for cancellation and uses a separate HID worker under
the system operation lock. The guest worker case drives the installed
td-pass worker entry with test approvals and an unprivileged hidraw
client.


The worker services HID and control traffic while TPM work and durable
writes are pending. The private controller polls for verification before
selecting an account or committing an operation. A cancel, lock or the
original HID deadline invalidates the job. Publication checks that event
after syncing the temporary file, immediately before its rename. A
cancellation after this commit point can suppress the reply to Firefox
even when the credential is already durable.

The physical seat can start a two-minute vault management session on the
private worker socket. The worker releases its browser probe cache. The
vault requires a fresh TPM PIN and verifies that the credential ID in a
removal matches the last metadata row it returned. The browser gets no
management commands through HID. Lock, closing the screen, generation
loss and expiry revoke management. Deletion checks its cancellation
after syncing the temporary file and before publication.
