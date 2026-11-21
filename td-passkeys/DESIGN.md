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
that uses the TPM first checks its dictionary-attack configuration. A
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
