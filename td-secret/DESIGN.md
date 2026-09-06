# Local credential manager

## Increment (a): file-backed master

This is a dependency-free Rust implementation of APPLICATIONS.md §W.4.
The initial backend is authorized by ordinary uid ownership. It does not
claim hardware protection, user authentication, elevation, memory locking,
or guaranteed erasure of Rust/compiler copies of secret buffers. Filling
owned buffers with zero is best effort. The unconfined user can read the
master; jailed applications cannot traverse or mount the store. No server
or external synchronization exists.

`/var/lib/td/secrets/<uid>` is a mode-0700 directory, with regular mode-0600
single-link files owned by uid. Directory traversal pins every component
and rejects symlinks. Reads reject wrong ownership, mode, type, links and
oversized input. The store lock serializes each complete read or update;
contention fails closed. Publication uses a random exclusive temporary,
file fsync, rename and directory fsync. An existing malformed master is
never replaced, and a missing master in a nonempty store is an error.

The `master` file contains exactly 32 bytes from the kernel random source.
HKDF-SHA256 (RFC 5869) extracts with salt `td-secret/store/v1`, then expands
one block with the authenticated application name as info. The derived
32-byte key stays in td-owned code. Each `APP.NAME` record has this layout:

| Bytes | Meaning |
|---|---|
| 0..8 | ASCII `TDSEC001` |
| 8..20 | Fresh random 96-bit nonce |
| 20..end-16 | ChaCha20 ciphertext, 1..4096 bytes |
| last 16 | Poly1305 authentication tag |

AEAD follows RFC 8439. The associated data is UTF-8
`td-secret/record/v1/APP/APP.NAME`. Both names contain 1..64 ASCII letters,
digits, hyphens or underscores, so neither the filename nor associated data
has ambiguous boundaries. Authentication precedes decryption. Nonces are
independent per write; this small local store does not approach the random
96-bit nonce collision budget. Rename provides crash atomicity, not rollback
protection against a writer able to replace the store or disk state.

## Credential interface

The activated desktop portal serves `td.Secret1` version 1:

- `Retrieve(s name) -> (h credential, s receipt)` performs a broker
  `GetConnectionCredentials` lookup for the original unique sender. Only an
  authenticated application at uid 1000 is currently admitted. No caller
  supplies its application name; `FLATPAK_ID` is not consulted by the portal.
- The credential descriptor names an already-unlinked regular file in the
  runtime filesystem. It is reopened read-only before transfer. The master
  and derived keys never cross D-Bus. Secret bytes never enter log messages,
  method arguments, process arguments, or persistent temporary plaintext.
- `Received(s receipt)` consumes the token only on the original connection.
  It grants no authority. The td-owned client calls it after reading the
  bounded file and requires the exact reply before handing bytes to tmc.
- Pending lookups plus unacknowledged deliveries are capped at 16; each owner
  may have four pending lookups and deliveries combined. Lookups and receipts expire after 20 seconds,
  with the service's ten-second audit retiring expired entries. Disconnect
  notification also retires that caller's entries. Replies from a peer other
  than the broker cannot resolve a pending identity.

The service refuses incoming descriptors. Its ancillary reader closes every
received fd and preserves the count for decoding and InvalidArgs replies.
The helper negotiates descriptor transfer, reads one bounded frame at a time,
owns every installed descriptor through rejection, and accepts only the
reply from the broker-resolved activated portal name. It validates file type,
unlinked status and length, and holds one 20-second exchange deadline.
The transport surface and source confinement are recorded in UNSAFE.md §15.

## Writers and migration

Firstboot creates the per-user store and `mail/main` placeholder. It imports
the former provisioner's `password` file before publishing portal-mode
configuration and deleting the old file. Existing stored bytes survive every
boot. A conflict between a stored credential and a legacy file is refused;
neither is silently discarded. This migration supports the exact previous
provisioner's path and main account, with explicit refusal for renamed
accounts, custom password-file paths, multiline strings and ambiguous
credential sources.
A mail refusal leaves its source data intact and does not prevent news
configuration from being provisioned.

`td-secret set APP/NAME` reads credential bytes from stdin. Application and
entry names are parsed separately and never interpreted as paths. This is
the interim console operation authorized in §W.4, with no consent UI. The
target replacement binds a secure-attention token touch to one typed request
and one credential descriptor through td-authd. Once a hardware backend is
selected it must fail closed; missing TPM or token cannot select a plaintext
master or an unauthenticated write path.

## Evidence

Tests include RFC HKDF and AEAD vectors, Poly1305, bytewise ciphertext/tag
tampering, identity substitution, file metadata, reopen and migration
idempotence, broker identity refusals, descriptor ownership and live transfer.
The image requires both the unconfined probe's exact credential refusal
and the supervised mail receipt marker. This composes the
provisioner, persistent store, tmc configuration, jailed helper, broker-fixed
identity, authenticated decryption, descriptor transport and acknowledgement.
It is not evidence for TPM sealing, FIDO2 release or elevation.
