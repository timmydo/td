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
selection, deadline and database snapshot. Body access verifies the complete
length and digest before granting a `PinnedBlob`; subsequent bounded reads
retain that same snapshot. A failed pin stays failed. MIME processing cannot
extend the original lifetime or use a later snapshot to complete an earlier
response. The PostgreSQL body adapter implements `VerifiedSource`; the shared
MIME code does not open a database or grant storage authority.

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
