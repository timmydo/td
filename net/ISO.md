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
There is no mutable latest-ISO pointer. Operators publish links to the
exact image and signature. Legacy expiry refuses verification;
it does not delete files or invalidate already written media.

A non-expiring root requires zero record expiry: verification needs no
periodic re-signing. Legacy expiring roots retain their 30-day maximum,
clipped to root expiry. Old records remain valid according to their own
signed policy. Retirement (below) deletes storage, retaining active
releases; deletion does not revoke cached authentic bytes.

```text
td-net release retire-iso-s3 ROOT system-x86-64 SIGNATURE ENDPOINT auto td-releases CREDENTIALS PROFILE
td-net release retire-iso-s3 ROOT system-x86-64 SIGNATURE ENDPOINT auto td-releases CREDENTIALS PROFILE --delete --exclusive-publisher
```

Retirement verifies the signature record against ROOT, derives the two
keys `upload-iso-s3` wrote from the record alone (no ISO bytes are
needed), and deletes `iso/ISO_SHA256/BASENAME` and then
`iso/signatures/RECORD_SHA256.sig`, each only if its upload metadata
matches the record's length and SHA-256, with `If-Match` on the observed
ETag. Without both flags it only reports `would-delete` or `absent`. An
absent file counts as retired, so a rerun after a lost reply succeeds.
Roots are never deleted. A record that no longer verifies (an expired
legacy root) cannot be retired this way. Unlist the entry first
(`unlist-iso-s3`, below): the page must not link to deleted files. The image key
is the ISO's digest and basename, so the same image signed under two
roots or profiles shares it: retiring one record deletes the image the
other still lists. `retire-s3` never deletes for a row whose image key a
listed row still names.

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

The bucket holds the listing's source: an explicit catalog at
`publisher/iso-catalog`, which every publisher continues, so no host keeps
its own. It is UTF-8 text beginning with `td-iso-catalog-v2`; every
following line has five tab-separated fields: `listed` or `retired`, root
epoch, profile, the signature record's SHA-256 (its key is
`iso/signatures/SHA256.sig`), and the SHA-256 of the release's
source-digest record at `publisher/source-digests/SHA256`, or `-` for a
release published before records existed. Listed rows keep publication
order, which retention counts by; a signature appears at most once; no
blank rows, comments or other fields are allowed; and the text is at most
one MiB and 1024 rows. The catalog is public, like every other key, and is
written with `no-store`, only on the ETag it was read at (`If-None-Match:
*` when absent), so a concurrent publisher's change refuses rather than
being overwritten.

```text
td-net release list-iso-s3 ROOT PROFILE SIGNATURE ISO DIGESTS RETAIN OUTPUT ENDPOINT auto td-releases CREDENTIALS PROFILE [--check]
td-net release retire-s3 ROOT OUTPUT ENDPOINT auto td-releases CREDENTIALS PROFILE [--delete --exclusive-publisher]
td-net release import-iso-catalog-s3 ROOT CATALOG RETIRED OUTPUT ENDPOINT auto td-releases CREDENTIALS PROFILE
td-net release unlist-iso-s3 ROOT OUTPUT ENDPOINT auto td-releases CREDENTIALS PROFILE SIGNATURE_SHA256...
```

`list-iso-s3` verifies the local ISO against its signature under ROOT and
the digest record's header, count and digests, then reads the catalog. It
lists the signature after the listed rows unless it is listed already (a
retry), moving a retired copy back to the end, and then keeps the newest
RETAIN (1 to 128) listed rows of each profile, marking older ones
`retired`; the release being listed must be among those kept. Each other
listed row's signature record is fetched and verified under its epoch's
root, whose chain from epoch one ends at ROOT (`pull-s3`'s chain, so the
bucket must not hold a newer epoch), and its image must be present with
the record's length and SHA-256 upload metadata. The page is then
rendered from those entries and this release's: legacy entries that have
expired, a missing record or image, one image listed twice for a profile
(the same image signed again while its earlier row is kept: unlist one)
and a page over its bounds refuse the whole operation. With `--check` it
stops there and writes nothing, so a refusal comes before promotion.
Otherwise it uploads the ISO, root, signature and digest record
(immutable, as `upload-iso-s3` does), reads the page's ETag, writes the
catalog and then `index.html` on that ETag, with `Content-Type:
text/html; charset=utf-8` and `Cache-Control: no-store`. A page another
host wrote since the catalog was read therefore refuses this one, rather
than being replaced by a page of an older catalog; rerun. The page is a
function of the catalog, so a lost page write is rebuilt by rerunning.
OUTPUT, a protected directory, receives the catalog and page as written.
No signing key is needed.

`unlist-iso-s3` marks the rows named by their signature records' SHA-256
`retired` in one write and republishes the page without them, whether or
not their records still verify; name every broken row at once, since the
rest must verify. It withdraws a bad installer, resolves a listed row that
no longer verifies (an expired legacy record, an image removed by hand)
and so blocks every listing, and makes room for an image signed again.
`retire-s3` then deletes its files.

`retire-s3` reads the catalog and, for each retired row, deletes the image
and then the signature record (`retire-iso-s3`'s rule, with the same
metadata and `If-Match` checks), or only drops the row when a listed row
names the same image key (one image signed twice; its signature record
stays, unreferenced). A row whose record is already gone is finished,
since the image goes first. Before each deletion it rereads the catalog
and stops if another host changed it. It then writes the catalog without
the finished rows and, only once the bucket holds exactly that catalog,
deletes every `sources/` digest no listed row's digest record pins (`s3
sources-gc`, `MIRROR.md`); until every listed row has a record, that
waits and says so. Without `--delete --exclusive-publisher` it only
reports what it would delete. A failure leaves its row retired for the
next run and exits non-zero.

`import-iso-catalog-s3` seeds a bucket that holds no catalog from a host's
pre-bucket `td-iso-catalog-v1` file: four tab-separated fields per row
(public root path, profile, signature path, ISO path, relative to the
file's directory unless absolute), with the pending rows of RETIRED
(`td-iso-retired-v1`, the same fields; a missing file is none) as retired
rows and the `source-digests` record beside each listed ISO uploaded as its
digest record. Each listed row's root must be the bucket chain's root for
its epoch and its record must verify. The last listed row, if the bucket
lacks it (a publish interrupted before its upload), is uploaded from its
local ISO; an earlier one the bucket lacks refuses, as a stale file. A pending
row that cannot be read or verified is skipped with a message, leaving its
files to delete by hand. After the catalog is written, both files are set
aside as `NAME.imported`, so they are never read again. A header-only file
seeds an empty catalog, which is how a new publisher starts. Where the
bucket already holds a catalog, a local file whose listed signatures it
holds is set aside, and one it does not cover refuses: another host
imported first ([OPERATIONS.md](OPERATIONS.md#per-user-setup) says how to
recover). With no bucket catalog and no local file it refuses.

Entries sort by profile, then descending signed issue time, with filename
and digest breaking ties deterministically. The first entry per profile is
featured as the latest listed release; remaining entries appear under a
collapsed, keyboard-accessible Previous versions disclosure. Each entry
shows its signed publication time and "No expiry" for non-expiring
records; no recurring signature renewal is needed. This ordering describes
the catalog, not freshness beyond it or source ancestry.

Configure the public serving layer to rewrite `/` to `/index.html`, preserve
all other paths, and honor the cache header. Links are root-relative. The
page embeds its stylesheet and uses no JavaScript or external assets. Its
colors, typography and layout follow `www/style.css`; its assets live under
`net/` so the networking recipe remains self-contained.

The page is unsigned navigation, not a release trust anchor or latest-release
proof. It displays signature expiry in UTC, but cannot remove entries as time
passes. Clients still verify against an independently trusted root and
current time. These commands do not publish system-update heads or change
their trust rules.
