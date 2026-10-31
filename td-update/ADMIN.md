# Trusted-main remote administration

The designated admin SSH key authorizes a fixed policy: build and install
`main` from `https://github.com/timmydo/td.git`, and reboot after successful
installation unless `--no-reboot` was requested. There is no post-build
consent prompt or unattended-approval deadline. Whoever can publish to that
upstream branch can publish the next system. HTTPS authenticates the source
server; this policy adds no commit-signature claim.

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
`update --no-reboot` installs without rebooting. `rollback [--no-reboot]`
restores the exact current/previous selector pair through td-boot and also
reboots by default; a changed pair refuses inside the boot transaction. `status` returns the latest
job's state (or `status ID` a retained job); `logs [ID]` returns the last 64 KiB of its build log; `cancel` requests
cancellation before installation; `reboot` requests orderly reboot when no
admin update is active. Repeated updates while a job runs return its status
and do not change its recorded reboot choice. A lost submission reply is
resolved with `status`, not an assumed failure.

The dedicated account is UID/GID 986, home `/var/lib/td-admin`, shell
`/bin/td-admin`. Its non-password shadow marker cannot authenticate through
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

## Source and build boundary

The root service owns `/var/lib/td-admin/repository.git`. It fetches exactly
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
