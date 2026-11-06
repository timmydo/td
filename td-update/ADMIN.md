# Signed-release and trusted-main remote administration

The designated admin SSH key authorizes two fixed update paths. `update`
acquires a release under the machine's separately provisioned publisher
policy; `update-from-source` builds and installs `main` from
`https://github.com/timmydo/td.git`. Both reboot after successful installation
unless `--no-reboot` was requested. Neither needs a further physical consent
prompt. Download failures never select the source path automatically.

Whoever controls a trusted release key can authorize bytes for that policy's
profile; a signed source commit is the publisher's assertion, not a proof of
reproducibility or Git ancestry. The source path separately trusts upstream
main over HTTPS and adds no commit-signature claim. Neither path admits a
caller-selected source, command or deployment pathname.

This is an explicit alternative to the local checkout's per-deployment
secure-attention approval, not a remembered approval from that path. The
local `td-update build` and consent-bound developer installation remain
available for arbitrary local source. The remote interface never accepts a
repository, revision, build command, deployment pathname or privileged shell.

## Initial key enrollment

On the installed primary account, run `td-admin enroll-key KEY.pub` with
one plain Ed25519 public key. This queues a settings request through the
existing UID-1000 intake. Press Ctrl+Alt+Escape, then H, verify the full
public key and its grant, and confirm the presented prompt. The new
`enroll-admin-key` elevation-table operation grants this initial enrollment
to the primary account. The fixed root helper validates the key with the
installed ssh-keygen and publishes the root-owned, mode-0644 key file.
There is no private-key upload and no SSH restart is needed.

Enrollment is initial-only: an existing key file, including an empty or
invalid one, refuses replacement. The helper serializes enrollment and
atomically creates the destination without replacing it. It cannot be used
as a key-rotation or recovery shortcut. Physical access to trusted recovery
media remains the recovery path for a lost administrator key. The admin
SSH command vocabulary cannot enroll or replace keys.

## Interface and identities

`ssh td-admin@HOST update` submits a job and returns its ID immediately.
`update --no-reboot` installs without rebooting.
`update-from-source [--no-reboot]` explicitly selects a local source build. `rollback [--no-reboot]`
restores the exact current/previous selector pair through td-boot and also
reboots by default; a changed pair refuses inside the boot transaction. `status` returns the latest
job's state (or `status ID` a retained job); `logs [ID]` returns the last 64 KiB of its build log; `cancel` requests
cancellation before installation; `reboot` requests orderly reboot when no
admin update is active. Repeated updates while a job runs return its status
and do not change its recorded reboot choice. A lost submission reply is
resolved with `status`, not an assumed failure.

The dedicated account is UID/GID 986, home `/var/lib/td-admin`, shell
`/bin/td-admin`. Its non-password marker in `/etc/passwd` cannot authenticate through
td-login. OpenSSH accepts only Ed25519 public-key authentication, from the
root-owned `/var/lib/td-admin/authorized_keys`. Its Match block forces
`/bin/td-admin ssh`, disables PTYs and inherits disabled forwarding and user
startup files. The shell accepts that exact `-c` invocation and parses
`SSH_ORIGINAL_COMMAND` as the fixed command vocabulary; it never evaluates
shell syntax. SFTP, another executable and extra arguments cannot bypass it.
This explicitly remains available in both login-key policy forms. Root SSH
stays disabled and the primary account's ordinary keys confer no admin grant.

The system service's socket is `/run/td-admin/control`, root:986 mode 0660,
inside root:986 mode 0710 `/run/td-admin`. The immutable account table gives
no other account membership in this group. Directory and socket permissions
are the authority boundary: the dedicated SSH process and root can connect;
the primary account and the builder cannot. Possession or deliberate
transfer of a connected descriptor is delegation of this administrative
capability. It is not a per-command cryptographic assertion.

`td-admin` is an argv-zero dispatch of the installed `td-update` executable.
No internal root or builder verb is exposed by the SSH parser. Neither
executable is setuid. The root service is a `stop=leaf` td-svc daemon, so its
requested stops drain its children. On an unexpected leader exit, the new
leader drains every helper subgroup before accepting commands.

## Release policy and download boundary

The desktop image bundles the official public root (epoch 2, no expiry,
SHA-256 `08f59be2c8edcc7b0d2e91305b726784cd326a41e663c2ec47984d95b40d801e`)
and `https://releases.tdlinux.org`. At service startup, after draining old
workers, td-admin initializes `system-x86-64` policy only if the entire
`/var/lib/td-admin/releases` path is absent. It verifies the bundled root
through the installed td-release, without a network request. Trust comes
from the installed image, not from a key downloaded beside an update.
No private signing key is bundled. Existing policy, including a partial,
invalid or custom one, is never replaced. An initialization error is reported
without withholding the source-update or rollback service.

Initial explicit publisher provisioning remains a root-only recovery/setup operation:

```text
td-admin configure-release URL ROOT_DOCUMENT PROFILE
```

Both paths serialize with the root-owned enrollment lease, prepare a mode-0700
`.releases-initial` directory beside the destination, and publish it only
after successful initialization and endpoint persistence. Interrupted staging
may be reclaimed under that lease; an existing destination is never reclaimed.
The completed `/var/lib/td-admin/releases` is root:root mode 0700. The
installed `/bin/td-release init` initializes `state`; `endpoint` holds the
fixed repository URL. The root document is public metadata selected
out of band; no private signing key goes onto the machine. The profile is
`system-x86-64` or `server-x86-64`. An existing policy directory refuses
replacement, including a partial failed setup; trusted recovery must inspect
and remove an incomplete initial setup before retrying. This operation is
not available to the remote admin key or ordinary session. Hosting and
publisher-key rotation follow `net/RELEASES.md`; the SSH caller cannot change
trust, repository, profile or version floors.

A source-only td-admin predating downloadable releases interprets SSH
`update` as a trusted-main source build. With an already enrolled admin key,
that command can install a main revision containing this bootstrap; its first
boot initializes the publisher, and subsequent `update` requests download
images. A transitional client where `update` already means download must use
`update-from-source` explicitly if it lacks usable publisher policy. Very old
systems without td-admin or an enrolled admin key require local installation
or key enrollment first. This migration still depends on the old image being
able to build current main; it is not a remote trust-bypass command.

An unconfigured `update`, or one without a valid available release, fails
and names `td-admin update-from-source` (preserving `--no-reboot`) as an
explicit alternative. Status and logs retain the failure. There is no
silent build, downgrade, expired-metadata override or reboot after refusal.

The root service drains the build cgroup, gives UID 985 a public snapshot
of its trusted state, and runs the installed downloader under that identity.
The worker retains verified objects in
`/var/lib/td-admin-build/release-cache`; its completed candidate is always
`/var/lib/td-admin-build/download`. It cannot write the privileged replay
state. A failed job may retain verified cache objects for the next request. The
cache retains at most two GiB of object payload bytes, evicting least recently
used objects under its lease; metadata and installation staging are separate.
The root download helper reads and validates the root-private deployment ID
in `/run/td-deployment` before switching identity, and passes that ID to the
worker. The downloader seeds missing requested chunks from that deployment
under the read-only volume's `td/deployments` directory.
This is the running image, independent of selectors changed by an earlier
`--no-reboot` update. Its bytes are untrusted cache candidates checked against
the new signed index, never an alternate release authorization.

After draining the worker, including failure or cancellation, root runs the
installed metadata learner over its fixed `release-state.metadata` evidence.
It authenticates signatures and transitions, never copies worker counters,
and persists learned revocations/floors even without payloads. Retained
evidence is learned before a retry replaces the worker snapshot, including
a retry after service restart. Invalid/incomplete evidence is ignored;
protected-state or persistence errors prevent further work. Expired signed
observations can raise floors but cannot authorize installation.

After successful acquisition and metadata learning, the service runs the
installed offline verifier over that fixed candidate using root's own
policy state. The verifier authenticates root rotations, signatures,
the signed expiry policy, profile, monotonic versions, the manifest and every complete
payload; only its bounded `COMMIT DEPLOYMENT_ID` receipt is authoritative.
The source-build worker's receipt is not used by this path. The existing
root-private installer then pins, copies and rehashes exactly that deployment
before entering td-boot's serialized transaction. No downloaded executable
runs as a verifier or installer. Replay state advances independently of boot
selectors; cancellation, installation failure and local rollback do not reset
it. Non-expiring roots impose no clock deadline on admission and require no
publisher renewal. Legacy expiring policy is checked at admission; neither
policy disables an installed current or previous deployment.

Release sequences and the source path's Git ancestry are separate histories.
An explicit switch between delivery paths can select different commits;
there is no claim of global commit ordering across that operator choice.

## Source and build boundary

The existing source builder targets `system-x86-64`; it is not a
profile-preserving fallback for a server. The stock `server-x86-64` profile
ships neither td-admin nor its service or administrator accounts. Server
profile support in the release transport and host-side boot oracle does
not provide installed-server remote updates: servers retain the replacement
and reprovisioning workflow in `td-svc/DESIGN.md` §10.

The explicit `update-from-source` path owns `/var/lib/td-admin/repository.git`. It fetches exactly
`refs/heads/main` from the compiled HTTPS URL, records its full commit ID,
and requires it to descend from the last accepted commit. A rewritten or
rewound branch refuses; there is no force flag. Git uses no ambient
configuration, hooks, templates, replacement objects, submodule recursion
or lazy fetching. The repository is readable by the builder but writable
only by root. A job keeps its selected commit even if upstream advances.

The service runs the installed builder entry point through td-login's
existing `exec-service-as` credential switch as `td-admin-build`, UID/GID
985, with its own mode-0700 home `/var/lib/td-admin-build`. The account has
`!td-service` and `/bin/false` and admits no SSH or human login. It checks out
the selected commit into a fresh private clone, verifies HEAD, then uses
td-update's normal source build and deployment assembly. No source code is
executed as root; source-built control-plane programs remain host control
plane under AGENTS.md's build model.

Every helper waits on a pipe start gate. The parent places its PID in the
owned subgroup before releasing the gate; EOF refuses execution, and the
helper verifies its membership. The builder uses
`/sys/fs/cgroup/system/td-admin/build`, root Git uses the sibling `git`,
and installation, rollback and reboot use `operation`. Before reuse and
at service startup all relevant groups are killed and drained. After a
helper completes or is cancelled, its direct child is killed and reaped
before the final descendant drain, preventing a late join or fork.
Build output is accepted only after that drain. The builder has no delegated writable host cgroup. A group
that cannot be drained refuses installation. Cancellation supervises both Git and build helpers and kills their owned
groups, never a command-line match. A stalled fetch remains cancellable.
Requested service replacement also drains the full td-svc leaf. The separate identity and empty group
prevent an earlier build from modifying a later build's source or output.

Only a bounded receipt under that worker's deployment directory is accepted.
The existing td-update installation mechanism pins the source, copies the
fixed manifest and three payloads into root-private staging, validates their
digests, and invokes td-boot's serialized on-volume publication. It checks
the result against the exact manifest digest. It does not run an installer
from the newly built source or accept a path from the SSH requester.

## Job lifetime

Root persists a monotonically increasing job ID, status and logs under
`/var/lib/td-admin`. Status publication writes, syncs and renames a file,
then syncs its directory. Queuing is recorded before execution. Each status
records the selected commit, deployment when known, and reboot choice.
Only one job runs. SSH and terminal lifetimes do not own it.

Cancellation and the transition to installation are serialized under one
lock. Cancellation before that transition prevents publication; afterward
it is refused and the operation must finish. A failed fetch, build,
validation or publication never requests reboot. Successful installation
requests `/bin/td-svc reboot` unless suppressed; the supervisor owns ordered
shutdown. A failure to request reboot does not undo an installed deployment.

A service or machine restart never automatically retries an interrupted
job. Status retains the previous report and explicitly says it is no longer
running. An installation interrupted after publication but before its
receipt remains uncertain; the boot transaction's recovery rules apply.
A later explicit update is a new request. The admin job lock is independent
of td-boot's writer lock, which serializes this path against local developer
installation and rollback.

A source build embeds its selected commit into the deployment. A release
publisher must prepare the same boot metadata before signing if it wants the
boot display to identify that revision; the signed index alone does not
rewrite image contents. Once such a deployment boots, `BUILD_ID` in `/etc/os-release` is the full commit, whose
short form the startup menu's label shows (`td-install/DESIGN.md`, "Installed
startup menu"). `--no-reboot` therefore leaves the running system reporting the
commit it booted from until the next boot. A rollback reports the commit of the
deployment it restores, or none if that deployment predates this.

## Installed-machine oracle

`td-recipe-eval qemu-admin-update` exercises the installed service, its
restricted SSH command, unprivileged downloader, root-owned replay state,
publication and reboot on a disposable desktop VM. The ordinary primary
account hosts an untrusted loopback repository using a target-built fixture;
only its ephemeral, separately provisioned admin SSH key grants update
authority. Expiry, truncated HTTP/resumption, root rotation/rollback refusal and
failed-boot rollback are required assertions. The test provisions policy
out of band and does not grant an ordinary account a policy-setting command.
The fixture executable and keys are absent from distribution images.

For an operator-run public-hosting check, use:

```text
td-recipe-eval qemu-admin-update --public BASE_DEPLOYMENT ROOT HTTPS_URL COMMIT DEPLOYMENT_ID
```

This boots the supplied older deployment on a disposable volume, provisions
only the explicitly trusted public root and an ephemeral SSH administrator
key, and runs the installed downloader against that HTTPS repository. It
requires the expected signed commit/deployment, nonzero local byte reuse,
a second update with zero new object bytes, a healthy boot reporting the
expected BUILD_ID, and explicit rollback to the original deployment. The
public head must remain on the selected release during the check. The
ordinary loopback oracle separately exercises hostile responses, key rotation
and automatic failed-boot rollback; this operator mode does not change public
metadata, enroll a production machine or exercise destructive failure cases
against a public repository. It uses the current checkout's selector and host
harness with the supplied base's userland and payloads. Logs survive under the
runner's `public-admin-update` directory; the disk and disposable SSH key are
removed when the run ends.
