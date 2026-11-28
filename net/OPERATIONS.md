# Public release operations

Publish signed static artifacts on demand. No renewal timer or periodic
signing job is required. A non-expiring root policy keeps its selected image
and installer signatures valid while the publisher is offline. Signatures
establish authenticity, not that a release is newest or free of security bugs.

Keep the publisher's local repository, signing keys and upload credentials
outside the checkout. The bucket holds the release history and the ISO
catalog; the local repository is a working copy each run brings up to it
([Pull release history](S3.md#pull-release-history)), so any host holding
those keys and credentials can publish. Keep the root private key offline.
Retain
the complete root rotation chain. Review
source changes regularly and publish tested images when useful or required
for security fixes; missing a release date does not disable downloads.

## Publishing

From a clean, reviewed checkout, run:

```text
./publish
./publish --test
./publish --config /absolute/path/to/publish.json
```

`./publish` publishes the commit **currently checked out**. It does not fetch,
reset, or select a Git branch: update your checkout to the intended reviewed
main revision first. The launcher rebuilds the host builder and evaluator,
and the Rust command resolves a freshly built td-net through the existing
provisioned host toolchain, pulls the bucket's release history into the
local repository, warms pinned inputs, builds the ISO and
retains the exact deployment it carries, warms every source and
build-root pin and uploads them to the [source mirror](MIRROR.md), signs
the image and ISO, checks that the bucket's ISO catalog can list it,
promotes and uploads the update, lists the ISO in that catalog trimmed to
the newest releases and republishes the download page, and then deletes
what that trim retired ([Storage retention](#storage-retention)). A pin it cannot warm
stops the run, naming it, before anything is signed or promoted; rerun
once its upstream serves it. All three host tools come from that
checkout; target builds retain their normal pinned input rules.
Only the desktop `system-x86-64` profile is supported by this convenience
command. The lower-level [release commands](RELEASES.md#local-operator-workflow)
remain available for other operator workflows.

Tests are opt-in: `--test` runs `td-builder check` and the installed
`qemu-admin-update` oracle before promotion. Missing test prerequisites or a
failed test stop publication. Without that flag, no test pass is claimed.
Signature, manifest, full-payload and ISO verification always run; they cannot
be disabled. Tests are separate from those integrity checks. The public HTTPS
oracle and an interactive `./test-iso ISO` installation remain separate checks;
this command does not claim a tested installation or public guest upgrade.

### Per-user setup

Run as your ordinary build user, including `timmy`, from any clone. No paths
under another account are compiled in and no sudo is needed for publishing.
The host still needs the ordinary td build prerequisites; `--test` also needs
its check environment, QEMU and KVM access.

The default configuration is `~/.config/td-publisher/publish.json` and default
state is `~/.local/share/td-publisher`. Absolute `XDG_CONFIG_HOME` and
`XDG_DATA_HOME` override those bases. For example:

```json
{
  "endpoint": "https://ACCOUNT_ID.r2.cloudflarestorage.com",
  "bucket": "td-releases",
  "region": "auto",
  "profile": "td-releases",
  "root": "~/.local/share/td-publisher/root-v2",
  "release-key": "~/.local/share/td-publisher/signing/release-v1.key",
  "credentials": "r2-credentials",
  "retain": 5
}
```

The required settings are `endpoint` and `bucket`. Region defaults to `auto`
and credential profile to `td-releases`. `retain`, a whole number from 1 to
128, is how many ISO releases of each profile stay listed and stored; it
defaults to 5. Give every publishing host the same `retain`: retention
counts the shared catalog, so a host with a lower one retires and deletes
releases the others keep. Optional `repository`, `catalog`
and `artifacts` paths default to `repository`, `iso-catalog.tsv` and `releases`
under the data directory. `catalog` names a host's pre-bucket local
catalog, read only to seed a bucket that holds none and then set aside
(below). The default public root is `root` in that directory;
the default private release key is `signing/release-v1.key` there. Credentials
default to `r2-credentials` beside the configuration. Relative configured paths
resolve against the configuration directory, regardless of the calling clone;
`~/` resolves against the invoking user's home. No shell expansion runs.
Unknown or duplicate settings refuse. Use UTF-8 paths without control characters.

Create private directories (`0700`) and install the release key and
[AWS-style credentials file](S3.md#configuration) owned by the invoking user
with mode `0600`. The offline root private key is **not needed**. Keep it
in the offline backup. Do not place private files in the checkout or in the
served repository.

To publish from another host or account, give it the current public root,
release key and credentials; earlier root epochs, the release history and
the ISO catalog come from the bucket. The copy must be owned by the
publishing user and accessible to them; another checkout alone does not
transfer these files. The release repository need not be copied: a run
creates it if absent and pulls the bucket's history into it. Keep a
retiring host's repository until its own `gc-s3` has collected the objects
it staged, which no other repository can ([Storage
retention](#storage-retention)). Publish from one host at a time.

The bucket's catalog ([Download listing](ISO.md#download-listing))
replaced each host's `iso-catalog.tsv` and `retired-isos.tsv`; each
release's digest record, still written to its artifact directory as
`source-digests`, is uploaded beside it. Each run calls
`import-iso-catalog-s3` after its first pull, before the build. While the
bucket holds no catalog, that seeds it from this host's `catalog` file,
its `retired-isos.tsv` and the records beside its ISOs, uploading a
last listed ISO if the bucket lacks it, and then sets both files aside as
`NAME.imported`, so they are never read again. So the first run after
upgrading must be on the host that published last. If another host
imported first, a host whose local listing the bucket's catalog lacks
refuses rather than lose those releases; so does a stale file whose
releases the bucket has since retired. If this host published last and
the bucket's catalog holds nothing newer than the import, delete `publisher/iso-catalog` with the S3
console and rerun on this host; otherwise move this host's file aside and
republish what it listed. A run whose bucket holds no catalog and whose
host has no local one refuses before the build; to recover a lost bucket
catalog, upload the newest run's `iso-catalog` artifact back to
`publisher/iso-catalog` with the console. For an intentionally new, empty
publisher, provision its root first and create a header-only local
catalog, which seeds an empty one:

```text
td-iso-catalog-v1
```

### Retries and records

State outside the clone holds one retained ISO/deployment per source commit
and public root. Rerunning from another clone at the same commit reuses those
bytes, verifies their saved hashes and embedded source identity, and resumes
uploads. Tests run again when `--test` is supplied. A partial build without its
completed hash record refuses with its location; preserve or remove that
incomplete ISO and deployment before retrying. Upload failures retain reusable
signed objects. A different configuration or a superseding selected release
refuses reuse rather than silently changing the publication target.

A workflow lock serializes these commands sharing a repository. Each run
pulls the bucket's release history when it starts and again before it
stages the index. An index another host staged meanwhile is numbered
above. A release another host selected meanwhile refuses the run, since
this commit would supersede it; the error names the release's
`selected-head-before` file, and removing it (allowed while this run has
staged nothing) publishes this commit over it anyway, keeping the build.
Nothing excludes two hosts or lower-level operator commands running at
once; use one publisher at a time. The bucket's head and ISO catalog are
replaced only on the version a run read, and the page on the version read
before that catalog write, so the slower of two concurrent runs refuses
there rather than overwriting the other; rerun it to continue from the
bucket's state. That does not make concurrent runs safe: retention by
one can still collect a source another has uploaded but not yet listed. Local selected heads can advance before upload,
and the bucket's catalog records the intended listing before the page
upload, so a lost page is rebuilt by a rerun. A failed run is not a
completed publication: retry it to reconcile a lost reply. Update
discovery and the ISO listing are separate remote writes; a failure can
leave the update available before the new ISO is listed.

Each artifact directory records the configuration, complete payload/ISO hashes,
index hash, ISO signature path, original and selected signed heads, and
successful completion.
`tests-passed` exists only after both requested test commands pass. Keep these
records; `iso-catalog` and `index.html` there are the catalog and page as
that run wrote them.
Only `./publish`'s retention step deletes remote files; nothing schedules
periodic signing.

For the separate public hosting check, run the
[public HTTPS oracle](../td-update/ADMIN.md#installed-machine-oracle) from a
compatible older deployment. Retain its transfer, healthy-boot and rollback
logs with the publication record. Never attribute a boot result to another
source revision or deployment.

New roots default to non-expiring policy:

```text
td-net release root VERSION ROOT_PRIVATE RELEASE_PUBLIC PREVIOUS_ROOT_PRIVATE ROOT
```

For the initial root, use the same private root key in both positions.
Rotation advances exactly one version and requires the previous root key.
A compromised release key is revoked by a new root delegating to a different
release key. Clients must learn that rotation; there is no clock-driven
revocation fallback. No root or release key needs periodic use merely to
keep an existing release available.

Clients that predate this policy must first upgrade through a still-valid
legacy release, current trusted media or the explicit source-build path.
They cannot interpret zero expiry. Publish current installation media as
part of that migration. An existing immutable root is never edited.

## Storage retention

`./publish` keeps the newest `retain` ISO rows of each profile (default
5) listed, newest meaning latest in the bucket's catalog, which is the
order they were published from any host. It also keeps the source mirror
digests the listed releases pin. Listing a release marks older rows
`retired` in the same catalog write that lists it, and only after the
page no longer links them does `retire-s3` delete each retired ISO and
its signature record and every `sources/` digest that no listed release
pins (`s3 sources-gc`'s rule, `MIRROR.md`); the [download
listing](ISO.md#download-listing) gives the details. A retired row whose
image a listed row also names (one image signed twice) is only unlisted
and never deleted; a retired row listed again goes back to the end. A
failed deletion leaves its row retired in the bucket and the run exits
non-zero after publishing; rerun `./publish`, from any host, to finish
it. Retirement stops when another host changes the catalog under it, and
mirror cleanup runs only once the bucket holds the catalog it computed.

Each release's digest record lives in the bucket at
`publisher/source-digests/SHA256`, named by its listed row, with a header
and count so a truncated copy is refused. Until every listed release has
one (a release listed before records existed has none), mirror cleanup
waits and says so; a malformed one stops it. `s3 sources-gc`'s rule
refuses a keep set that names nothing the mirror holds. These deletions
run with `--exclusive-publisher`, which is this command's own rule of one
publisher at a time. Digest records, roots, heads and the update
repository are left in place; the update repository is the collector's,
below.

Some failures recur on every run until resolved by hand. A listed row
no longer verifies (an expired legacy record, an image removed or altered
by hand), so every listing refuses: unlist it (every such row in one
call) with `td-net release unlist-iso-s3 ROOT OUTPUT`, the bucket settings
and each row's signature SHA-256,
which is also how to withdraw a bad installer from the page. A retired
row's record no longer verifies: delete its image and then its signature
record with the S3 console; a row whose record is gone counts as
finished, so the next run drops it. A remote file's upload
metadata does not match: the deletion refuses rather than guess;
reconcile or delete the object yourself. Local artifact directories are
retry records only; retirement reads nothing from them.

Update-image storage is collected separately, by age rather than count.
Run remote cleanup as a dry run before deciding to delete. Pause all other
publishers, including those on other hosts, for the entire deletion window:

```text
td-net release gc-s3 REPOSITORY ROOT ENDPOINT auto td-releases CREDENTIALS td-releases
td-net release gc-s3 REPOSITORY ROOT ENDPOINT auto td-releases CREDENTIALS td-releases --delete --exclusive-publisher
```

Selected indexes remain indefinitely. Older non-expiring indexes remain at
least 30 days after issue plus seven days' grace, and all indexes remain for
seven days after non-expiring head promotion or upload. Shared objects survive
while any retained index needs them; unreachable objects must also be over
seven days old. Thus image retirement is a storage operation, never a timed
failure of signature verification. A suspended client may need the current
release if its old objects were collected.

The command reclaims exactly its planned local candidates after confirming
remote deletion or absence. Unknown remote indexes or differing heads
refuse it, so run `release pull-s3` first, as `./publish` does. A pulled
repository holds only the objects its host staged, and only those are
candidates; another host's unreachable objects wait for that host's
collection. Do not run standalone local GC on a published repository: its
later cutoff could delete history still remote. [S3.md](S3.md#remote-collection) defines
the bounds and exclusivity assumptions. This collector never deletes
installer downloads or mirrored sources; `./publish`'s retention does.
