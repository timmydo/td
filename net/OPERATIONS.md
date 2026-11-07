# Public release operations

Publish signed static artifacts on demand. No renewal timer or periodic
signing job is required. A non-expiring root policy keeps its selected image
and installer signatures valid while the publisher is offline. Signatures
establish authenticity, not that a release is newest or free of security bugs.

Keep the local publisher repository, signing keys and upload credentials
outside the checkout. Back up the repository after publication and keep the
root private key offline. Retain the complete root rotation chain. Review
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
provisioned host toolchain, warms pinned inputs, builds the ISO and
retains the exact deployment it carries, signs both, promotes the update,
uploads it, and publishes the complete ISO listing. All three host tools come
from that checkout; target builds retain their normal pinned input rules.
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
  "credentials": "r2-credentials"
}
```

The required settings are `endpoint` and `bucket`. Region defaults to `auto`
and credential profile to `td-releases`. Optional `repository`, `catalog`
and `artifacts` paths default to `repository`, `iso-catalog.tsv` and `releases`
under the data directory. The default public root is `root` in that directory;
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

When moving the existing publisher to Timmy, copy its **complete** repository,
public root chain, release key, credentials and ISO catalog with every file
that catalog references. Rewrite catalog paths that name the old account.
The copy must be owned by Timmy and accessible to him; another checkout alone
does not transfer these files. Stop publication from the old account before
switching. Do not start a fresh repository against an existing bucket: it
would lose sequence and retention history. The command refuses a missing
repository or catalog. For an intentionally new, empty publisher, provision
its root/repository first and explicitly create a header-only catalog:

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

A workflow lock serializes these commands sharing a repository. It does not
coordinate separate copies on other hosts or lower-level operator commands;
use one publisher at a time. Local selected heads can advance before upload,
and the local catalog records the intended listing before the page upload.
A failed run is not a completed publication: retry it to reconcile a lost
reply. Update discovery and the ISO listing are separate remote writes; a
failure can leave the update available before the new ISO is listed.

Each artifact directory records the configuration, complete payload/ISO hashes,
index hash, ISO signature path, original and selected signed heads, and
successful completion.
`tests-passed` exists only after both requested test commands pass. Keep these
records and back up the publisher repository and catalog after publication.
No command here deletes remote files or schedules periodic signing.

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
remote deletion or absence. Do not run standalone local GC on this published
repository: losing local history still present remotely blocks safe remote
collection. Unknown remote indexes or differing heads require reconciliation.
Keep the latest repository backup. [S3.md](S3.md#remote-collection) defines
the bounds and exclusivity assumptions. ISO retirement is an explicit
catalog operation; this collector never deletes installer downloads.
