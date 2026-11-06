# Signed ISO downloads

`td-net release` can sign, verify and upload a whole ISO with the release
key authorized by an explicitly supplied public root from [RELEASES.md](RELEASES.md).
This authenticates downloaded bytes before writing media. Firmware and the
installer do not consume this signature; it does not enable Secure Boot.
The signer asserts the profile and source commit. These commands do not
inspect ISO contents, establish reproducibility or test bootability.

## Commands

```text
td-net release sign-iso ROOT RELEASE_PRIVATE system-x86-64 COMMIT td.iso SIGNATURE_DIRECTORY
td-net release verify-iso ROOT system-x86-64 SIGNATURE td.iso
td-net release upload-iso-s3 ROOT system-x86-64 SIGNATURE td.iso ENDPOINT auto td-releases CREDENTIALS PROFILE --dry-run
```

Signing prints the created signature path. Verification prints the source
commit, SHA-256, size and basename. Supply an independently trusted root:
a self-signed root downloaded beside the ISO does not establish its identity.
Verify its fingerprint through a trusted channel before accepting it.
Verification is stateless: it checks the supplied root and record's expiry,
not whether this is the newest release. It does not learn root rotations or
persist rollback floors. Root rotation requires supplying the new trusted
root through the existing release trust process or an independent channel.

Remove `--dry-run` to upload. The uploader validates everything before making
requests, uploads `iso/ISO_SHA256/BASENAME`, then `roots/ROOT_VERSION`, then
`iso/signatures/RECORD_SHA256.sig`. All three are immutable, using the
conditional upload and reuse rules in [S3.md](S3.md). A failed upload can leave
unreferenced objects; retry resumes by reusing matching remote objects.
There is no mutable latest-ISO pointer or remote deletion. Operators publish
links to the exact image and signature. Expiry refuses verification; it does
not delete files from storage or invalidate already written media.

Records expire after 30 days, clipped to root expiry. Re-signing after time
passes produces a new record address and can reuse the same ISO object.
Keep old records until their consumers no longer need them. Configure any
bucket lifecycle separately, ensuring it does not remove active releases.

Private keys must be owned regular files with no group/other permissions.
Images must be nonempty regular files, at most four GiB; their ASCII basename
starts with an alphanumeric character, contains only letters, digits, `-`,
`_`, `.`, ends in `.iso`, and is at most 128 bytes. Verification requires the
same basename. Hashing streams within a one-hour operation deadline.
Keep source files and ancestor directories stable during the operation:
opened descriptors prevent path replacement from redirecting an upload but
do not protect against another process modifying an open file in place.
No private signing key is needed by the uploader or verifier.

## Record format

The UTF-8 record is at most 16 KiB and contains these newline-terminated
lines, with no extra fields:

```text
td-release-iso-v1
ROOT_VERSION
ISSUED_UNIX_SECONDS
EXPIRES_UNIX_SECONDS
PROFILE
SOURCE_COMMIT
ISO_BASENAME
BYTE_LENGTH
SHA256
ED25519_SIGNATURE
```

The Ed25519 signature covers every preceding line including its newline.
Numbers are canonical decimal; commit, hash and signature are lowercase hex
of 20, 32 and 64 bytes respectively. The marker separates this assertion from
other release messages. Existing release lifetime rules reject future-issued records; both root
and record must be fresh before and after hashing.
