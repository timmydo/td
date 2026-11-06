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

Follow [RELEASES.md](RELEASES.md#local-operator-workflow) from a clean reviewed
checkout. Rebuild the builder, evaluator and network tool before every image:
the evaluator embeds some target sources, so an old executable can build old
code despite an updated checkout. Record the commit, deployment ID and index
hash together. Run the installed admin oracle, explicitly promote the tested
index and upload it; discovery is published last.

Then run the [public HTTPS oracle](../td-update/ADMIN.md#installed-machine-oracle)
from a compatible older deployment. Retain its transfer, healthy-boot and
rollback logs with the publication record. Never attribute a boot result to
another source revision or deployment.

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
