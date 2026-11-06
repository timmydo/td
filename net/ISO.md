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
Verification is stateless: it checks the supplied root and record's signed policy,
not whether this is the newest release. It does not learn root rotations or
persist rollback floors. Root rotation requires supplying the new trusted
root through the existing release trust process or an independent channel.

Remove `--dry-run` to upload. The uploader validates everything before making
requests, uploads `iso/ISO_SHA256/BASENAME`, then `roots/ROOT_VERSION`, then
`iso/signatures/RECORD_SHA256.sig`. All three are immutable, using the
conditional upload and reuse rules in [S3.md](S3.md). A failed upload can leave
unreferenced objects; retry resumes by reusing matching remote objects.
There is no mutable latest-ISO pointer or remote deletion. Operators publish
links to the exact image and signature. Legacy expiry refuses verification;
it does not delete files or invalidate already written media.

A non-expiring root requires zero record expiry: verification needs no
periodic re-signing. Legacy expiring roots retain their 30-day maximum,
clipped to root expiry. Old records remain valid according to their own
signed policy. Configure storage retirement separately, retaining active
releases; deletion does not revoke cached authentic bytes.

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
other release messages. Under a zero-expiry root, record expiry must also be
zero and wall-clock time does not gate admission. Legacy policy checks future
issue times and expiry before and after hashing.

## Download listing

`iso-index` generates a standalone `index.html`, styled like `www/`, from an
explicit UTF-8 catalog. It does not scan a bucket or infer releases from file
names. The catalog begins with `td-iso-catalog-v1`; every following line has
four tab-separated fields: public root path, profile, signature path, ISO
path. Paths are relative to the catalog's directory unless absolute. No blank
rows, comments or control characters are allowed. The maximum is 128 entries
and one MiB of catalog text. A header-only catalog produces an empty listing.

```text
td-net release iso-index CATALOG index.html
td-net release upload-iso-index-s3 CATALOG index.html ENDPOINT auto td-releases CREDENTIALS PROFILE --dry-run
```

Both commands verify the complete local ISO bytes, signatures and root policy.
Invalid or missing entries, and expired legacy entries, refuse the entire
operation. Non-expiring entries display "No expiry". Retire obsolete entries
explicitly in the catalog; no recurring signature renewal is needed.
For one ISO/profile select one signature; duplicate entries and conflicting
roots for one epoch refuse. Multiple root epochs and profiles are supported.
Keep the complete catalog: uploading it replaces the listing, it does not
merge it with the previous page. Entries sort by profile, filename and digest.

The upload command writes the local preview even in dry-run but makes no
network requests in that mode. Without dry-run it uploads/reuses every listed
ISO, root and signature before publishing `index.html` last, with
`Content-Type: text/html; charset=utf-8` and `Cache-Control: no-store`.
Publication uses the original page's ETag (or absence) as a write condition;
a concurrent change refuses unless exact readback equals the intended page.
A failed operation may leave reusable immutable files, but does not publish
an incomplete listing. No remote files are deleted. No signing key is needed.
Local inputs and output directories must remain stable during the command.

Configure the public serving layer to rewrite `/` to `/index.html`, preserve
all other paths, and honor the cache header. Links are root-relative. The
page embeds its stylesheet and uses no JavaScript or external assets. Its
colors, typography and layout follow `www/style.css`; its assets live under
`net/` so the networking recipe remains self-contained.

The page is unsigned navigation, not a release trust anchor or latest-release
proof. It displays signature expiry in UTC, but cannot remove entries as time
passes. Regenerate and publish after catalog changes; clients still verify
against an independently trusted root and current time. This command does
not publish system-update heads or change their trust rules.
