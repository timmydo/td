# td-mta storage

[POSTGRES.md](POSTGRES.md) owns durable storage, configuration, credentials,
transaction authorization, database concurrency and recovery. PostgreSQL is
the sole mail authority. There is no embedded database, database-directory
lock, permanent body-file tree, local device registry or alternative backend.
[FORMAT.md](FORMAT.md) defines transient application keys and row values;
those encodings do not define a second on-disk database format.

## Durable values and snapshots

PostgreSQL stores immutable message, upload and transmitted-copy bytes with
metadata and SHA-256 digests. It also stores mailboxes, memberships, keywords,
threading, submissions, recipients, leases and retained changes. The private
runtime role and schema-qualified queries enforce the authority boundaries
specified in POSTGRES.md. Owner administration uses separate credentials.

An admitted read retains its original account, store epoch, configuration
selection, deadline and database snapshot. Ordinary body access verifies the
complete length and digest through `VerifiedSource` before granting a
`PinnedBlob`. Body-bearing Email/get may instead qualify the versioned MIME
manifest and use `VerifiedChunkSource`, which checks each requested chunk's
length, ordinal and SHA-256 before exposing any bytes. Unrequested chunks need
not be read or verified; raw downloads and integrity audits still verify the
whole body. POSTGRES.md specifies qualification and fallback. Both private
pin constructors retain the original snapshot, sticky failures and deadline.
MIME processing cannot extend that lifetime or combine snapshots. Shared MIME
code does not open a database or grant storage authority.

Qualified derived search fields, like stored header summaries, may answer
metadata searches without opening a body pin. Their body identity and field
checksum bind them to the authenticated snapshot; they do not prove the
current integrity of unread body chunks. Missing or invalid evidence falls
back to verified body interpretation under the same method work budget.

Device authentication and publication checks belong to the PostgreSQL
transaction paths. Passive request/result structures in `store_fs::mutations`
convey no authentication or mutation authority. PostgreSQL commit uncertainty
must not be reported as a confirmed successful mutation or blindly replayed.

## Local runtime and response scratch

`serve` uses separate operator-provisioned `--runtime` and `--scratch` roots.
The runtime root holds its cooperative process lock and control endpoint.
The scratch root holds disposable request files, never authoritative mail.
Different processes use different roots while sharing the database.

The safe std filesystem adapter requires an operator-controlled stable
namespace, caller-owned private directories and regular private files with
one hard link. It retains the cooperative lock and checks file identity.
It does not defend against another process with the same filesystem authority
replacing paths. Symlinks, unexpected entries, wrong ownership or permissions
refuse admission. No new syscall or unsafe surface is needed.

`RequestSpool` accepts only LOCK and request-00 through request-31 directories,
each containing at most one response file. Cold cleanup validates the entire
closed namespace before discarding abandoned files. Cleanup permits an
owner-only subset of the normal mode left by a restrictive creation umask.
The process must be quiescent and own the lock before recovery. A live
response retains its file identity and slot until cleanup succeeds; failed
cleanup conservatively retires the slot.

`ResponseCoordinator` consumes a cold empty RequestSpool and owns only its
process-local response-byte ledger. Occupied or retired slots refuse
construction. Response retention preserves the original work and deadline,
reserves the whole file allowance, and accounts for prepaid response output
and auxiliary request indexes. It conveys no durable-mail quota or device
authority. There are no native database-view or external-sort hooks on this
owner. [ADMISSION.md](ADMISSION.md) owns generic work and retention rules.

Storage workers serialize the short in-memory response-ledger section on its
mutex instead of treating momentary contention as exhausted capacity. No
filesystem, database or peer operation holds that guard; cleanup unlinks before
acquiring it. Admission freshly checks the original deadline after acquisition
and before reserving space. Poisoned accounting refuses; actual quota and cell
exhaustion still refuse without starting a method. This is ordinary worker
mutex waiting, not an interruptible wait or a host-scheduler deadline guarantee.


## Qualification and operations

The native gate runs real PostgreSQL fixtures with the checkout's td-mail
and fetch test executables, and a separate real serving-process fixture.
Protocol-only tests use bounded memory sources or disposable response files.
No embedded-database dependency or its allocation/backup probes remain.
[CONFORMANCE.md](CONFORMANCE.md) states the scope of the PostgreSQL evidence.

Backup, restore and server resource management use PostgreSQL facilities;
there is no td-mta local-store backup or SQLite import command. Deployment
and startup are documented in [DEPLOYMENT.md](DEPLOYMENT.md) and
[CONFIG.md](CONFIG.md). Database transactions remain atomic; replacement of
the unused old implementation requires no production cutover protocol.
