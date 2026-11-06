# Public release operations

The public mirror serves signed static artifacts. The local publisher
repository, public root documents, signing keys and upload credentials
live outside the source checkout. Back up the repository after publication;
keep the root private key offline and the online release key private.
A bucket backup alone does not restore signing authority or establish a
trusted local publication history.

## Cadence

- Renew and upload the selected head every 12 hours. Heads last at most
  24 hours, clipped to image-index and root expiry. Treat any nonzero exit
  as an operator incident; inspect the local log and public head expiry.
- Review source changes and publish a tested image at least weekly, with
  earlier releases for security fixes. This is a manual release decision.
  An image index lasts at most 30 days. A renewal job does not rebuild,
  select new code or keep an expired image eligible indefinitely.
- Review root and installer-signature expiry during the weekly release
  review. Rotate the root/key chain before expiry; system-head renewal
  does not renew installer signatures.
- Run remote cleanup first as a dry run during the weekly review. Delete
  only in an exclusive publisher maintenance window. The same command
  reclaims its exact local candidates after remote deletion; retain the
  latest repository backup.

## Publish a new system image

Use the clean-checkout build and `publish-next` workflow in
[RELEASES.md](RELEASES.md#local-operator-workflow). Record source revision,
deployment ID and signed index hash together. Run the adversarial installed
admin oracle on the candidate. Explicitly promote the tested index, then
upload it with `release upload-s3`; discovery is uploaded last.

Run the public HTTPS installed-admin oracle from a retained older deployment
as described in [ADMIN.md](../td-update/ADMIN.md#installed-machine-oracle).
Retain its logs with the publication record: installed-deployment seeding,
new object bytes, repeat-cache behavior, healthy boot identity and rollback.
Boot/rollback evidence belongs to that exact published deployment, not to
whatever source happens to be newest later.

## Renewal job

Configure the host's scheduler to execute this argv every 12 hours, using a
reviewed installed td-net binary outside a changing development worktree:

```text
td-net release renew-upload-s3 REPOSITORY ROOT RELEASE_PRIVATE system-x86-64 ENDPOINT auto td-releases CREDENTIALS td-releases
```

Arguments contain paths to secrets, never secret values. The scheduler must
use the publisher account, preserve a private log, prevent overlapping jobs
and report failures. Keep the host clock synchronized. This document does
not install a scheduler or promise unattended monitoring.

An interrupted upload can be retried; matching immutable objects are reused.
A newer local head after failure is expected. An expired public head can be
repaired by renewal while the selected index and root remain valid. If the
index expired, test and publish a fresh index explicitly. If the root expired,
follow the key-rotation procedure; never bypass client freshness checks.

## Collection window

Pause scheduled renewals and all other publishers for the whole window.
Use the same endpoint, bucket, protected credentials and complete local
repository used for publication:

```text
td-net release gc-s3 REPOSITORY ROOT ENDPOINT auto td-releases CREDENTIALS td-releases
td-net release gc-s3 REPOSITORY ROOT ENDPOINT auto td-releases CREDENTIALS td-releases --delete --exclusive-publisher
```

Review the dry-run output before the deletion command. Unknown remote
indexes or different heads require reconciliation, not forced deletion.
Do not run a separate local GC on the published repository: remote cleanup
removes only its fixed candidate set locally after confirming remote deletion
or absence. Resume renewal
when the window closes. Current/previous installed deployments are outside
this cleanup, and shared chunks survive as long as retained indexes use them.
[S3.md](S3.md#release-renewal-and-remote-collection) specifies the bounds and
concurrency assumptions. ISO retention is a separate explicit catalog
operation; this collector never removes installer downloads.
