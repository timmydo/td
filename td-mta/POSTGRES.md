# PostgreSQL replacement contract

## Status and precedence

This is the agreed replacement target, not implemented PostgreSQL support.
The current executable and STORAGE.md still describe SQLite. This document
supersedes the single-process, file-configuration and SQLite choices for the
replacement workstream; it does not relax the current implementation's
invariants before cutover. This precedence applies to all routed mail
documents, including CONFIG, RESOURCES, API and FORMAT. Protocol scope remains
the flows used by td-mail.

The deployment is one personal account with several cooperating td-mta
processes: a cloud VM can receive and relay mail while a local machine serves
JMAP or relays through a configured smart host. Processes connect to one
writable PostgreSQL database. No peer-to-peer replication, offline writable
mail store, read-replica routing or general multi-tenant service is required.
PostgreSQL is the sole authority for message and attachment bytes, metadata,
configuration, device verifiers, TLS/ACME keys and smart-host credentials.

Support independently installed PostgreSQL, Azure Database for PostgreSQL
Flexible Server and Amazon RDS for PostgreSQL. Aurora and database proxies
need separate qualification; AWS support does not imply every PostgreSQL-
compatible offering. Installation/supervision of a local PostgreSQL server is
a later convenience. It is not a prerequisite for connecting to an existing
installation. The user approved tokio-postgres and its transitive dependencies. The
initial admission pins 0.7.18 with default features disabled: no Tokio runtime
or network/time driver is selected. The lock includes private SCRAM crypto
and string preparation; TLS remains the td-crypto boundary. This increment
provides the inert connection probe in section 8, with no selectable backend.
No server package is admitted here; source-build rules still apply.

## 1. Bootstrap and profiles

Normal startup needs a connection specification and a small `--profile NAME`
selector. The named database profile selects account, receiving/JMAP/relay
roles, listener addresses, public URLs, limits, certificate references and
routes. For example, cloud and desktop profiles can share the same account
while enabling different listeners. Several instances may select one profile;
each process generates its own unpredictable boot identity. A profile is
configuration, not an authorization credential or a singleton lock.

Keep only unavoidable bootstrap inputs outside the database: endpoint,
database and login credentials, database TLS trust, profile selector and
host-local runtime/scratch/log placement. Supply secret-bearing connection
specifications through a protected file or inherited input, never require
passwords in argv. Reject unknown connection options and redact the complete
specification and provider errors. A database row must not override its own
bootstrap endpoint or trust roots. Defaults permit the simple connection plus
profile launch without a second full MTA configuration file.

Network connections require certificate-chain and endpoint-name verification;
there is no opportunistic TLS fallback. Local TCP follows the same rule.
A protected Unix-domain socket can be a separately explicit local transport,
with authenticated database login and trusted socket-directory ownership.
It does not silently replace a failed TLS connection. Initial authentication
is PostgreSQL password/SCRAM over verified TLS; cloud identity-token login is
not required for the first cutover. Resolve the configured writer endpoint
again on reconnect, within bounded DNS and connection deadlines.

Startup connects, verifies writable-primary status and supported schema,
loads and validates a complete profile/configuration generation, checks
required secrets and certificates, and initializes bounded pools before
opening service admission. An unavailable database never starts an offline
service. Unsupported schema or invalid configuration fails startup with a
redacted actionable error. Serving does not create or upgrade a schema.

## 2. Schema, configuration and trust

Use an application-owned schema with parameterized, closed SQL behind typed
storage interfaces. Runtime credentials receive the required data privileges,
not schema ownership or DDL privileges. Explicit initialization/migration uses
a separate owner credential, an exclusive database migration lock, and a
version/checksum ledger. Initialize only when the application namespace is
absent; refuse even an existing empty namespace and never replace a store.
Incompatible upgrades require all instances to drain and stop; no mixed-version deployment is promised initially. Every
serving database connection must acquire a shared session-level advisory
lock on the fixed migration key before checking schema compatibility, and
retain it until closure. The migrator takes the conflicting exclusive lock
with a finite deadline and refuses while any serving connection remains.
After migration, a returning old process must reacquire that shared lock and
validate the new schema before any application SQL; stale pools cannot bypass
the barrier. This requires direct sessions, not transaction-pooling proxies.

Configuration has immutable validated generations and one transactional
active-generation pointer. Shared domains, aliases, identities and routes
are distinct from per-profile role/listener selections. All references and
secret versions must validate before a generation becomes active. Disabled
or unsupported roles cannot become active merely because a row names them.
A failed candidate leaves the previous generation intact. Instances observe
generations by bounded polling; notifications may accelerate observation but
are never the sole correctness mechanism. Restart-only changes report that
status rather than pretending the new listeners or pool sizes are active.

Device verifiers and revocations live in the database. Authorization-sensitive
writes lock/check the current account policy, device and identity versions
in their publication transaction. Revocation and policy changes take
conflicting locks in the same documented order. Thus revocation committed
before a write's authorization point prevents that write; a write serialized
before revocation may complete. A cached configuration or authenticated
connection cannot bypass this check. Reads authenticate against current
primary state before opening an admitted snapshot; an already admitted,
bounded read may finish under that snapshot after revocation.

TLS private keys, ACME account/order state and relay secrets are versioned
database values with restricted privileges and redacted diagnostics. Device
passwords remain generated high-entropy tokens with only salted verifiers
stored. Database administrators, backup readers and a compromised authorized
MTA process are within the trusted boundary for secrets they can read. Storing
an encryption key alongside ciphertext would not remove that trust. Provider
storage encryption and protected backups are operational requirements, not
end-to-end encryption claims. Bootstrap credentials remain separately
recoverable. These unattended service credentials grant no td human login or
elevation; future td installation still follows its credential policy.

An administrator uses explicit database-authorized configuration/device
operations. No public administration HTTP endpoint is introduced. The local
control socket still controls that process's status and shutdown; it cannot
stand in for a database-wide exclusion lock. Certificate publication and
renewal use database ownership/version fences so two instances cannot publish
conflicting generations. HTTP-01 routing must reach every selected challenge
serving instance before ACME activation; shared rows alone do not prove that.

## 3. Bodies, metadata and publication

Keep exact immutable message/upload/transmission bytes in bounded `bytea`
chunk rows keyed by account, blob and ordinal, with a manifest holding length,
digest and chunk count. Start with a 64 KiB chunk ceiling and retain the
32 MiB message ceiling. Validate contiguous ordinals, exact total length and
SHA-256 before publication. No SQL operation returns a whole message value;
read ranges in bounded batches with caller-owned scratch. Use ordinary logged
tables, foreign keys, uniqueness and checked scalar constraints. PostgreSQL
large objects, external object storage and filesystem body authority are not
part of this contract.

Private local ingress files may hold unacknowledged bytes. They are disposable
on process loss. Stream prepared chunks into a finite database transaction;
body manifest, chunks, metadata, references, quota effects, history and
operation receipt become visible together at COMMIT. Do not hold that write
transaction while an SMTP or HTTP peer uploads bytes. An aborted upload or
transaction creates no visible blob and no delivery responsibility.

Serialize account mutations with a locked account state row. Allocate the
next committed account sequence there, in the same transaction as object
changes and retained history; a standalone SQL sequence does not establish
commit order. Preserve store epoch, object identity, immutable thread
assignment, reference integrity, retained change floors and queue invariants.
JMAP ifInState is checked under that lock. Use a single documented lock order
for policy, account, route and attempt rows to avoid lock-order inversions.
Bound contention with lock/statement/transaction deadlines and bounded retry
for definite rollback only. Never hold account locks across SMTP round trips.

Read operations use finite read-only repeatable-read snapshots on the writer
endpoint, binding state tokens and object/body reads to the same view.
Snapshot deadlines include MIME work and response production; slow clients
cannot indefinitely prevent vacuum/reclamation. The effective transfer
deadline is the minimum of the HTTP allowance and the remaining snapshot
lifetime. At cutover, replace ADMISSION's existing transfer formula with this
combined cap and reject inconsistent timer configurations: idle-transaction
limits must cover admitted gaps between database reads within that cap.
A minimum HTTP rate never extends a database snapshot lifetime. Qualify this
cap against real td-mail downloads before activation. A failed snapshot cannot be
transparently replaced halfway through a response. End incomplete output
rather than splice a different snapshot into it. Download digest verification
and byte replay retain the same immutable blob and snapshot.

Every potentially ambiguous publication has a random operation ID and a
request digest, recorded transactionally with its result. On lost COMMIT
response, discard the connection and resolve that operation on the primary.
Reexecution uses the same ID under a unique-key/transaction fence, so a still
running original cannot race an absence check into duplicate publication.
A different digest for the same ID refuses. Bound receipt retention and retry
lifetime together; after expiry an unresolved operation is not replayable.
This is internal recovery, not a new JMAP idempotency promise. A client which
loses a response still uses td-mail's submission reconciliation flow.

Require logged tables, fsync=on and full_page_writes=on, verified at startup
and reconnect before admission. Set synchronous_commit=on for each publication
transaction; database defaults cannot weaken it. Deployment administration
must preserve these settings while serving. Acknowledgements follow confirmed
COMMIT or its durable receipt. SMTP never
returns final 250 for locally staged bytes. Known success remains success
if later accounting, cleanup or response transmission fails. Uncertain
publication is not a definite refusal and is never blindly repeated.
PostgreSQL settings alone do not prove a failover target contains every
acknowledged commit: the deployment must qualify its replication/durability
policy. Promotion of a lagging replica or point-in-time restore is recovery,
not transparent continuation under the same store epoch.

## 4. Distributed delivery ownership

Outgoing submissions select the submitting profile's named route at enqueue.
`[relay] route = "personal-smarthost"` gives that route a stable name; omission
uses the profile name. Profiles may share a route, but one configuration
version must give each route identical host, port, transport, authentication
identity and credential/trust references. Each current profile selects one
outgoing route and, when its relay role is enabled, serves that route.
The name is not a machine identity: any instance serving the same route can
compete for delivery ownership. With no eligible worker, queued mail waits.

The queued route name and destination fingerprint remain fixed across retries.
The fingerprint covers canonical host, port, transport and authentication
identity. Credential and trust versions may rotate through validated
configuration; destination changes do not silently redirect existing mail.
A worker must match both the stored name and destination under fresh policy
and ownership fences. Rerouting queued responsibility needs a separate
explicit operation; no automatic fallback to another profile's route exists.


Preserve QUEUE.md's immutable outgoing bytes/envelope, per-recipient states,
uncertainty latch, retry limits, cancellation and retained failure notices.
Replace its process-local worker fence with durable route/attempt ownership.
The default one concurrent delivery per route is database-wide, not one per
process. Route pauses, backoff and repair generations are shared too.

A short transaction locks a due route and eligible recipient batch, installs
an owner boot ID, fresh attempt ID, increasing fencing generation and database
lease expiry, and commits Prepared before any SMTP work. Bounded due scans
may use FOR UPDATE SKIP LOCKED for competing workers; ordinary mail reads
must not skip locked rows. Heartbeats renew only the exact current unexpired
owner/fence. Every phase or result update compares that fence under lock;
stale workers cannot overwrite a replacement's result or renew an old claim.

Use database time for cross-instance lease eligibility and local monotonic
deadlines for network attempts. Clock jumps/failover trigger conservative
clock validation and the existing retry guard; elapsed wall time alone never
proves a remote attempt harmless. Exact lease/heartbeat/attempt limits and
clock tests are part of worker activation, not arbitrary per-host tuning.

Persist Body before body transmission and AcceptancePossible before any
terminator byte enters the transport buffer. If a phase commit is unknown,
stop SMTP output and resolve its receipt; never assume the phase did not
commit. Loss of database connectivity or renewal authority stops new SMTP
work and closes that worker's transport. A replacement can reclaim expired
Prepared/Body work as retryable, preserving any prior uncertainty; a durable
AcceptancePossible phase is always recovered as OutcomeUnknown.

A lease cannot revoke bytes already buffered in a remote SMTP connection.
A paused worker can resume after takeover, including between a final local
check and a write. Database fencing protects database state, not exactly-once
SMTP. Persisting AcceptancePossible before handoff ensures such a race keeps
its duplicate risk visible. Retry preserves transmitted bytes/Message-ID and
never assumes remote deduplication. Already durably Accepted recipients are
never selected again.

Cancellation locks and fences the same route/attempt state. It can guarantee
no delivery only while no recipient is accepted/uncertain/AcceptancePossible.
An old Prepared/Body worker cannot cross into AcceptancePossible after that
fence, so sending a partial body cannot turn into a committed delivery.
A worker already granted AcceptancePossible makes cancellation refuse, even
if its lease has expired. Shutdown closes owned transports before voluntarily
releasing claims, drains bounded result commits, and leaves unresolved phases
for recovery. It never resets all claims just because one process stopped.

## 5. Outages and resource limits

Connection pools, waiting jobs, result rows/bytes and retry queues have fixed
configured maxima. Reserve capacity for health/reconnect and delivery result
settlement separately from interactive reads and maintenance. Database-side
statement, lock and idle-transaction timeouts supplement socket and whole-job
deadlines. Bound driver buffering and server error/notice retention. No
unbounded task-per-request or transparent driver retry of writes is allowed.

Logical account/blob/upload/queue quotas are transactional database counters
and reservations shared by all instances. Process-local counters only bound
that process's concurrency and scratch. Reservation reclaim uses durable
ownership and fences: expiry cannot free capacity while an old holder can
still publish against it. Logical quotas do not reserve server disk/WAL space;
server resource exhaustion produces bounded temporary failure. SQLite heap,
VM-fuel, WAL-file and checkpoint ceilings do not describe PostgreSQL limits.
Server memory, vacuum, WAL growth and backups belong to database operations;
MTA RSS and pool/resource observations remain separate measurements.

On a connection blip, mark affected roles unready, stop new durable work and
reconnect with bounded exponential backoff and jitter. No offline acceptance,
local writable fallback, stale credential authentication or unbounded RAM
queue is allowed. Return SMTP temporary errors or HTTP/JMAP temporary errors
where framing and known outcomes permit; close an already started response
on loss of its snapshot. Never turn a lost mutation receipt into a safe-retry
claim. Other healthy instances can continue. Reconnect verifies primary,
epoch, schema, configuration and unresolved operation state before reopening
admission; it does not run whole-database recovery on every connection.

## 6. Backup, restore and qualification

Backups include bodies, metadata, history, profiles, secrets and queue state
from one consistent database backup/PITR boundary. PostgreSQL/provider tooling
owns physical backup and recovery. The MTA verifies semantic state separately;
it does not copy server data files from a client machine. Protect backups as
credentials and mail, and retain bootstrap access independently.

Restore requires an explicit maintenance barrier: fence/stop all old
instances and SMTP egress before activating the restored database, generate
a fresh store epoch and invalidate old claims/caches/state tokens. Reconcile
potential deliveries after the backup boundary; missing accepted records
cannot justify automatic resend. Quarantine restored outstanding submissions
until this risk is resolved or the operator explicitly accepts duplicate risk.
A restored credential/config snapshot may resurrect revocations or old keys;
validate/rotate them before opening service. Never run old and restored stores
as independent writable authorities for the same deployment.

The first release must demonstrate two real instances sharing a local
PostgreSQL fixture: receive on one, read/download/flag on the other with
real td-mail; concurrent mutation/state/quotas; revocation across instances;
worker death/takeover and stale phase/result refusal; lost COMMIT receipts;
database restart/connection interruption; orderly one-instance shutdown;
restore with epoch and delivery quarantine. Local protocol fixtures remain
offline. Provider qualification is a separate opt-in operator exercise on
empty disposable databases: TLS/name refusal, reconnect through failover,
role privileges and acknowledgement durability. Never silently use a live
mailbox or claim cloud qualification from a local mock.

## 7. Landing sequence and removal boundary

1. This contract and dependency/driver evaluation, including TLS and bounded
   I/O integration. Obtain explicit approval before adding a driver closure.
2. Inert PostgreSQL bootstrap/schema/storage foundations with real local
   database tests. Keep the current executable working while replacements
   are prepared; no selectable production dual backend is introduced.
3. One atomic production cutover of configuration, credentials, body/metadata
   storage, runtime coordinators and operational commands. Remove SQLite code,
   dependency/pins, private device files, exclusive-store assumptions and
   obsolete SQLite qualification in the same landing. Update STORAGE,
   CONFIG/SCHEMA, ADMISSION/RESOURCES, QUEUE, FORMAT, API and conformance documents to
   the actual implemented interfaces. Existing data need not be migrated.
4. Activate distributed relay and complete td-mail sending on the replacement
   backend under the ownership contract above. Incomplete roles stay refused.
5. Add one-way Stalwart import, preserving raw bytes, folder membership,
   received dates and flags with durable source-ID mappings and resumability.
   DESIGN section 13's source freeze, verification and rollback rules remain.
   Local PostgreSQL installation/supervision can follow independently.

Do not claim PostgreSQL support, automatic delivery takeover or cloud
qualification until their named integration evidence passes. Freeze exact SQL,
supported server versions, driver pins and command syntax in implementation
increments rather than implying this contract already supplies them.

## References

- [PostgreSQL commit durability](https://www.postgresql.org/docs/18/runtime-config-wal.html)
- [PostgreSQL row locking and SKIP LOCKED](https://www.postgresql.org/docs/18/sql-select.html#SQL-FOR-UPDATE-SHARE)
- [Azure PostgreSQL TLS](https://learn.microsoft.com/en-us/azure/postgresql/security/security-tls-how-to-connect)
- [Azure PostgreSQL high availability](https://learn.microsoft.com/en-us/azure/postgresql/flexible-server/concepts-high-availability)
- [RDS PostgreSQL TLS](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.SSL.html)
- [RDS failover and endpoint changes](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/Concepts.MultiAZ.Failover.html)

## 8. Implemented connection probe

`postgres_probe::probe` is an inert worker entry point, not service startup
or a storage backend. Its caller supplies an admitted resolved address,
server DNS name, borrowed credentials, trust store and absolute Instant
deadline at most ten seconds away. It does not resolve DNS, read a connection
file, load a profile, migrate a schema or retry. User/database names are
nonempty, NUL-free and at most 256 bytes; passwords are 1..4096 bytes.
The TLS facade validates the at-most-253-byte server name before dialing.

One worker drives connect and a fixed query through tokio-postgres with
TLS required. The driver sends SSLRequest; td-crypto's outbound Postgres
selector verifies the certificate chain and server name before startup
credentials enter the stream. There is no plaintext fallback, direct-TLS
ALPN, client certificate or channel-binding claim. The plaintext framing
guard requires the SASL challenge/final/OK sequence and refuses MD5,
cleartext-password, trust and other authentication methods. The pinned
driver selects SCRAM-SHA-256 and verifies its server signature; its
100,000-iteration ceiling bounds the server-directed password work.

The private async adapter is used only by this periodically polled worker,
which parks at most one millisecond between turns. It uses no Tokio runtime,
network/time driver, background task or concurrent query. Driver channel
wakeups unpark that same thread. All loop turns and final success check the
original deadline; TCP connect and TLS retain finite bounds. Native crypto
work remains subject to TLS.md's between-call deadline limitations.

Before driver intake, cap backend frames at 128 KiB including their length
word, 128 frames and one MiB total for the entire connection. Fragmented
headers and authentication codes retain nine bytes of framing state. A
refusal aborts the transport. Plaintext output has a 64 KiB connection cap;
TLS retains its existing two wire buffers. These are input/work bounds, not
a whole-process RSS qualification or a serving pool reservation. The future
storage worker must define operation-specific budgets and pool accounting.

The sole query uses pg_catalog-qualified functions to inspect server version,
recovery/read-only state, fsync and full_page_writes. It refuses recovery,
read-only sessions or disabled durability settings and returns only the
numeric version. Startup options fix search_path, a five-second statement
timeout and a one-second lock timeout. Success drops the connection; no mail
or configuration is written. This does not validate a schema, enforce a
supported-version range, or establish replication/failover durability.
Provider diagnostics and credentials never enter the fixed error enum.
The worker drives `poll_message` and drops asynchronous notices without
invoking the driver Future implementation, which logs raw server text.

The ignored `postgres_probe` integration fixture requires an explicitly
provided independently installed PostgreSQL bin directory through
`TD_MTA_TEST_POSTGRES_BIN`. It initializes only a new private disposable
cluster, creates its own test certificate and password, and checks verified
TLS/SCRAM, wrong name/trust/password, expired deadline, TLS refusal,
cleartext/trust authentication refusal, read-only refusal and reconnect after
server restart. It never connects to
an operator's existing database. This is host qualification, not a shipped
server package or evidence for Azure/RDS. Framing refusal tests run offline
in the ordinary unit suite.
The ordinary suite also checks expiry while a peer stalls SSLRequest.
The framing boundary follows the
[PostgreSQL message formats](https://www.postgresql.org/docs/17/protocol-message-formats.html).


## 9. Implemented bootstrap decoding

`postgres_bootstrap::Bootstrap::parse` is a pure parser for a protected
connection specification and profile name. It performs no file access,
environment lookup, DNS, connection or service activation. Sections 10 and 11
provide separate resolver and protected-file workers; loading a database
profile remains startup work. Never put its secret-bearing input in argv or
diagnostics.
Neither the parsed value nor the connection request implements Debug.

The initial syntax is one `postgresql://USER:PASSWORD@HOST[:PORT]/DATABASE`
URI (`postgres://` is also accepted). Credentials and database are explicit;
there are no environment defaults, password files, service-name expansion,
multiple hosts or Unix-socket fallback. HOST is an ASCII DNS name, including
localhost, authenticated by the TLS facade; literal IPs, numeric address
shorthand, numeric final labels and encoded hosts are refused. The default port is 5432. Reconnect must resolve HOST again.

The optional query accepts `sslmode=verify-full` and an absolute
`sslrootcert` path, each at most once. Verification is mandatory even without
sslmode. An absent root path means the public trust store; an explicit path
replaces it. Unknown parameters, duplicate parameters and weaker TLS modes
are refused, rather than forwarded to the driver. A root path here is only a
name; its subsequent reader must enforce the protected-file policy.

Input is at most 16 KiB of printable ASCII without whitespace or fragments.
Percent escapes decode once, preserving literal plus signs. Encode URI
separators in credentials/database and spaces or non-ASCII data. User and
database are nonempty UTF-8 text without controls, at most 256 decoded bytes.
Passwords are 1..4096 non-NUL decoded bytes; the trust path is nonempty UTF-8
without controls, at most 4096 bytes. Profiles are 1..64 ASCII letters,
digits, dots, underscores or hyphens, starting with a letter or digit.
All parse failures return fixed categories without the input or credentials.
This decoder does not claim cloud connectivity or a supported schema.

## 10. Implemented resolver worker

`postgres_resolver::resolve` accepts the parsed bootstrap, the caller's
trusted absolute td-mta executable path and an absolute deadline at most five seconds
away. It starts that executable in private resolver mode with a cleared
environment, hostname-only stdin, bounded stdout and discarded stderr.
No connection URI, credentials, profile or trust bytes enter the child.
Each invocation performs a fresh system resolver lookup; any system DNS
cache retains its own TTL policy. This is not database connection retry or
service activation.

The child uses the platform resolver and may consult trusted host resolver
configuration. Blocking libc/NSS resolution stays outside service workers.
The parent polls child completion, kills and reaps it on deadline or error,
and admits only a complete versioned response with at most sixteen unique
IP addresses and 1024 bytes. IPv6 scope IDs are unsupported. The child
checks the same DNS-name policy as bootstrap and caps result traversal at
128 addresses. It writes only after complete validation, within pipe
capacity; no streaming partial result is accepted.

Before reading stdin, the dedicated child starts a watchdog that exits its
whole process after five seconds. This requests expiry after parent loss
and during blocked input/read/lookup; native exit cleanup may delay it. The internal entry point always
exits; it cannot leave a watchdog behind in a serving process. Ordinary
shutdown still owns and reaps an in-flight helper. Native process creation,
scheduling, kill/reaping, native exit cleanup and filesystem/libc calls retain the platform's
between-call limitations; this is not a hard real-time or resolver RSS
claim. The child uses no shell, external resolver program or new dependency.
The eventual serving pool must separately bound concurrent resolutions.

Unit process fixtures qualify parent cancellation and independent child
expiry. An explicitly ignored host integration fixture invokes the built td-mta
binary and resolves localhost twice, without external DNS or an operator
database. It requires the host name-service configuration; hermetic suites
still run the resolver deadline/argument checks without that dependency. Cloud DNS
failover and writer reconnection remain separate qualification work.

## 11. Implemented protected bootstrap loading

`postgres_input::Loaded::open` reads a connection file and constructs its
TLS trust store without DNS, database access or service admission. It takes
the file path and profile, never a password argument. The operator probe in
section 12 selects it; serving does not. The file's parent must satisfy the
existing `PrivateRoot` contract:
mode 0700, non-root owner, absolute normalized path within `MAX_ROOT_BYTES`,
no symlinks and only root/owner ancestors without shared write permissions.
The supervisor must run as that dedicated owner; std does not verify the
process UID. Namespace writers and mount authority are trusted, paths remain
stable, and checks do not defend against hostile pathname races. Opening
this directory acquires no store lock and creates no local database.

The existing `operator_files::Inputs` boundary checks the connection as a
regular secret input (0400 or 0600) and binds all input ownership to that
parent. The URI is at most 16 KiB, optionally followed by one LF or CRLF.
Other whitespace, comments, extra lines and invalid UTF-8 refuse. The reader
retains at most 16 KiB plus three bytes of source scratch before parsing;
parsed credentials are separate bounded allocations. It makes no secure
memory-erasure claim.

An explicit sslrootcert is opened as a public input under the same owner
and ancestor policy, including secret/public inode-alias refusal, and read
with 128 KiB plus one byte of scratch. `TrustStore::from_pem` enforces its
complete private-CA bundle contract. Explicit trust replaces public roots;
missing, unreadable, oversized or invalid explicit trust never falls back.
Without sslrootcert, only the facade's compiled public roots are loaded.
Every failure is a fixed category, without file paths, file bytes or provider
messages. Reads are bounded in bytes, not wall time: operator-provisioned
regular files are cold trusted I/O, outside the network deadline.

Unit tests cover line endings, decoding and bounded reads. The ignored
`protected_files_enforce_modes_aliases_and_explicit_trust` host fixture
requires a private TMPDIR under trusted ancestors and exercises actual file
modes, symlinks, secret/public hard-link aliasing, trust replacement, missing
trust and size refusal. It uses only a disposable TempDir and generated CA;
ordinary sandbox tests do not assume host directory ownership.


## 12. Operator PostgreSQL probe

`td-mta postgres probe --connection-file PATH --profile NAME` runs the
protected loader, owned resolver helper and TLS/SCRAM probe together. Run
as the connection directory's dedicated owner. It writes one bounded JSON
result to stdout and exits zero on success, one on qualification failure,
or two for invalid arguments. Passwords/URIs are file contents, never required
arguments. Diagnostics include no input path, endpoint, credentials or raw
provider text. The selected profile is syntax-checked only; it is not loaded
from the database.

After cold file loading, DNS receives at most five seconds and the whole
network operation receives ten seconds. Resolved addresses are attempted
in order with the same absolute deadline. Only TCP/transport refusal permits
another address; authentication, TLS-policy, settings or deadline errors
stop the operation. No attempt renews its budget. As in section 8, driver
connection/authentication failures are deliberately coarse. A slow first
address may exhaust the budget before later addresses can be attempted.

Success reports the server version and explicitly sets schema_checked and
profile_checked false. It proves only the section 8 connection/settings
checks at that instant, not service readiness, replication durability or
cloud failover qualification. It writes no mail/configuration, installs no
schema and never opens serving listeners. The separate ignored
`real_postgres_probe_command_loads_resolves_and_authenticates` fixture
requires the section 8 PostgreSQL tools plus a private TMPDIR under trusted
ancestors. It uses a fresh disposable database and tests the actual command,
private trust, localhost resolution, wrong credentials, read-only refusal,
restart/reconnect and connection-file mode refusal.

## 13. Prepared schema ledger

`postgres_schema::initialize` is an explicit schema-owner operation used by
section 14's preparation command; no serving path calls it. Its current
server contract is PostgreSQL major 17, qualified locally on 17.10. Other
majors refuse until separately qualified. It creates the preparation ledger
and sections 16, 19, 21, 25, 27, 30 and 32's material/mail/device/configuration,
account-bootstrap, passive selection and flag receipt tables,
not a complete
serving schema or runtime role. Serving still cannot select
PostgreSQL. The owner credential is separate from the future runtime
credential; no owner credential is installed into a serving pool here.

The operation verifies the same writable-primary/durability settings as the
probe, begins one transaction, sets synchronous_commit=on, and takes an
exclusive transaction-level try-lock on the bigint migration key
0x74646d7461000001. Conflict returns Busy without waiting. It refuses any
existing td_mta namespace, even an empty one. The committed SQL in
src/postgres_schema.sql creates that schema and its schema_migrations
ledger. Each migration records its exact DDL's SHA-256 and the same 128-bit
store epoch in a ledger row. Version 1's committed SQL and checksum remain
fixed; version 2 adds src/postgres_materials.sql and version 3 adds
src/postgres_mail.sql; version 4 adds src/postgres_devices.sql, and version 5
adds src/postgres_configuration.sql; version 6 adds src/postgres_accounts.sql,
version 7 adds src/postgres_selection.sql, and version 8 adds
src/postgres_flags.sql. Version 9 adds src/postgres_flag_batches.sql.
Version 10 adds src/postgres_uploads.sql; version 11 adds
src/postgres_drafts.sql; version 12 adds src/postgres_mailbox_create.sql, and version 13 adds
src/postgres_receiving.sql; version 14 adds src/postgres_submissions.sql;
version 15 adds src/postgres_submission_routes.sql; version 16 adds
src/postgres_route_owners.sql; version 17 adds src/postgres_attempts.sql;
version 18 adds src/postgres_attempt_recovery.sql.
New initialization applies all eighteen in
one transaction. PUBLIC grants are revoked; inherited
schema/relation grants to other roles refuse initialization and roll back.
Explicit runtime grants and configuration-driven service admission remain
cutover work.

`postgres_schema::upgrade` uses an explicit owner credential and the same
exclusive transaction migration lock. It validates all existing checksums
and the common epoch before applying every missing migration. A
compatible current ledger returns its receipt without another write.
Unknown, missing, changed or inconsistent ledger history refuses. Migration
preserves the epoch and commits the DDL and ledger row together. A failed
migration rolls back, leaving the prior version intact; an old version
cannot serve current operations. Post-COMMIT uncertainty has the same no-replay
rule as initialization.

The client shares the original ten-second worker limit, configured
statement/lock timeouts and protocol byte caps. Owner initialization/upgrade
permits 512 input frames for the twelve migrations and their ledger writes;
probe, inspection and material operations retain 128. The per-frame 128 KiB
and total 1 MiB input bounds, 64 KiB schema output bound, authentication
checks and terminal budget refusal are unchanged. Fixed SQL and typed
parameters are private to the crate; arbitrary driver execution is not a
public API. Initialization errors before COMMIT drop the session and leave
no committed namespace; migration errors preserve the previous schema. Once
COMMIT may have been sent, a lost response or deadline is Indeterminate,
never a safe-to-retry refusal. A known successful commit retains its receipt
even if completing that poll crosses the deadline. There is no automatic
replay or uncertainty resolver in this preparation API. An operator must
resolve database state before another initialization.

`postgres_schema::inspect` takes the conflicting shared session-level
try-lock before reading the ledger. Its bound is the compiled migration
count plus one (currently thirteen rows); it requires
exactly the twelve ordered migration versions, their compiled DDL checksums and
one common 16-byte epoch. It checks its final deadline before returning. It
closes the session and lock before returning; the receipt grants no
authority for later SQL on another connection. This checks the recorded
ledger, not every catalog object or evidence against a malicious schema
owner. The future serving connection must retain its own shared lock
throughout its lifetime and validate the complete serving schema.

The real PostgreSQL schema fixture checks existing-namespace refusal,
rollback after inherited-grant refusal, shared/exclusive migration
conflicts, initialization/inspection, restart persistence and
version/checksum refusal. A second fixture delays COMMIT with a disposable
deferred trigger, observes Indeterminate at the client deadline, then
inspects the committed ledger after the original session ends and verifies
that another initialization refuses. A database commit can outlive the
client deadline and disconnect; the configured statement timeout does not
establish rollback. Neither fixture uses an existing database. They require
the section 8 tools, including psql. The advisory-lock lifetime/conflict
behavior follows PostgreSQL's [advisory-lock
functions](https://www.postgresql.org/docs/17/functions-admin.html#FUNCTIONS-ADVISORY-LOCKS).

## 14. Operator schema preparation

`td-mta postgres schema-init --connection-file PATH` explicitly initializes
section 13's preparation ledger using a separately provisioned schema-owner
login. It refuses every existing td_mta namespace. It creates the prepared
material, mail, device, immutable configuration, account-bootstrap and
selection and flag receipt relations with an initially empty passive configuration
pointer, but no runtime role or listener. The matching `td-mta
postgres schema-inspect --connection-file PATH` checks the ledger under its
shared migration lock. `td-mta postgres schema-upgrade --connection-file
PATH` applies every missing migration through version 9, or reports an
already current ledger. None of these commands takes a profile: the
namespace and epoch are deployment-wide.

These commands use the same protected bootstrap, verified TLS/SCRAM, fresh
bounded DNS lookup and shared ten-second network deadline as the probe. They
try another resolved address only on a definite transport refusal before
COMMIT; the schema worker converts every connection loss after COMMIT may
have been sent to Indeterminate. No schema error, migration conflict or
uncertain commit is retried. Exit codes remain zero for success, one for
failure and two for invalid arguments. Provider text and supplied arguments
never enter the fixed JSON diagnostics.

Success returns schema_version and store_epoch with ledger_checked true,
serving_schema_checked false and profile_checked false. An
indeterminate-do-not-retry result means the original backend may still
commit; wait for that operation to settle and inspect database state before
another explicit initialization. An immediate missing-ledger inspection is
not proof of rollback. A failed stdout write can also lose a successful
receipt; do not interpret a missing command response as permission to
replay.

These commands qualify only the preparation ledger, not complete schema
integrity or operational mail service. Further schema migrations require
explicit owner upgrade; profile publication remains separate work.

## 15. Prepared instance role validation

`config::profile::Roles` provides the closed, nonempty role set for future
profile rows: receiving is bit 1, JMAP bit 2 and relay bit 4. Unknown bits
refuse decoding. These values are structural declarations, never proof of
runtime support or authorization. No database profile is stored or activated
by this preparation API.

`config::listener::Builder::finish_for_roles` requires SMTP listeners
exactly when receiving is selected and HTTPS listeners exactly when JMAP is
selected. A relay-only profile has no inbound listeners; HTTP-01 requires
receiving or JMAP. Each profile retains the existing listener syntax,
duplicate/bind checks and aggregate SMTP session limits. A disabled role
cannot be enabled by merely adding its listener. Sticky builder failures
remain terminal.

`config::load::read_profile` now carries explicit roles through whole-reader
EOF, stanza finalization, listener validation and graph closure. Its
PreparedProfile retains the selected roles and provides read-only candidate
inspection or storage reclamation. It cannot be passed as the legacy Loaded
input to text/provider materialization or serving. No command selects it.
The separate selected-profile content path is specified in section 18.
Its owner is included in the compile-time workspace-state bound; the existing
portable stack fixture qualifies the legacy loader, not this new entry point.

The input is an already composed selected profile, not a database generation
format. It retains existing account, identity, route, resource and relay
requirements. A nonreceiving profile requires explicit domain MX names but
does not require local SMTP listeners for those shared names; a receiving
profile retains the local-MX/direct-listener check. Policy serving remains
part of the HTTPS/JMAP listener role: an enabled MTA-STS policy in a composed
profile without that role refuses. Certificate consumers, names, origin
ports, ACME HTTP-01 and all other reference checks remain enforced. A
relay-only profile has no inbound listeners and must omit unused certificates.

The existing file loader still requires both receiving and JMAP as before.
Future database generation validation must distinguish shared policy from
per-profile policy serving, compose each selected profile, validate global
receiving/policy coverage across profiles, resolve versioned database
materials, verify implemented capabilities, and validate the entire
generation before publication. This selected-profile structural API does
not prove those deployment-wide properties.

## 16. Prepared versioned configuration material

Schema version 2 adds owner-private config_materials manifests and
config_material_chunks. `postgres_materials::insert` appends one explicit
128-bit version and its immutable raw bytes in one durable transaction.
`fresh_version` supplies random identifiers; a duplicate primary key refuses
without replacing bytes. The caller retains its version before entry so a
lost commit receipt does not force another identity. There is no update,
delete, activation or public administration endpoint here. No service path
consumes these records yet.

The closed kinds and raw input ceilings are relay password (1026 bytes),
certificate chain (64 KiB), private key (16 KiB), explicit trust bundle (128
KiB), and text/HTML signature (16 KiB each). Only signatures may be empty.
These match existing raw input limits, including optional password line
endings. Storage validates size and kind, not text/PEM syntax, key validity,
certificate names or cross-references. Complete generation admission must
still run those checks before selecting a value. A stored reference cannot
substitute one kind for another.

Each manifest stores kind, length, SHA-256 and chunk count; each chunk holds
at most 64 KiB with an ordinal. Reads select one exact version and expected
kind under a read-only repeatable-read snapshot. The worker requires exact
contiguous ordinals, full non-final chunks, exact total length/count and a
matching digest before exposing any bytes. It reads at most two manifest
rows and three chunk rows to detect unexpected extras. SQL constraints bound
individual rows; the typed writer/read verifier establish aggregate shape.

Every operation qualifies the primary and obtains the shared session schema
lock, validates the complete current ledger and checks the caller's expected
store epoch on that same connection. It retains the session through all SQL
and byte verification. Writes use synchronous_commit=on; the manifest and
chunks commit together. Failures before COMMIT roll back. Once COMMIT may
have been sent, a lost response is Indeterminate. After the original backend
settles, read the retained version and compare the intended bytes; absence
while that backend may still commit is not proof of rollback. There is no
automatic replay, uncertainty resolver or permission to pick a fresh version
and duplicate an uncertain write.

Material operations retain the ten-second worker deadline, five-second
statement timeout, one-second lock timeout, 128-KiB backend-frame bound,
128-frame count and one-MiB input cap. Their post-TLS output cap is 256 KiB
to carry a maximal trust bundle plus fixed SQL/authentication overhead;
probe and schema operations retain 64 KiB. Each operation owns a cold,
fallibly reserved copy of at most 128 KiB; driver encodings/row buffers and
TLS/provider allocations are additional bounded inputs, not a complete RSS
qualification. The returned Material owner intentionally has no Debug
implementation, and errors contain no input bytes or SQL/provider
diagnostics. Dropping memory does not claim secure erasure. Database owners,
backups and authorized processes remain inside the service-secret trust
boundary of section 2; these values confer no td human login or elevation
authority.

Real PostgreSQL fixtures cover version-1 migration with preserved epoch,
rollback after inherited grants, migration-lock refusal, all raw input
limits, empty signatures, duplicate immutability, kind/epoch refusal,
restart persistence, chunk corruption/missing data, mid-write rollback and
lost-COMMIT resolution using the original version. They use only disposable
local clusters and do not qualify Azure/RDS or configuration activation.

## 17. Operator material preparation

`td-mta postgres material-version` generates one random canonical 128-bit
version without opening files, contacting a database or reserving an ID.
Retain it before importing. Explicit import and comparison use:

```text
td-mta postgres material-import --connection-file PATH --input-file PATH \
    --kind KIND --epoch HEX --version HEX
td-mta postgres material-check --connection-file PATH --input-file PATH \
    --kind KIND --epoch HEX --version HEX
```

Both IDs must be exactly 32 lowercase hex characters. Obtain the epoch from
schema-inspect. Kinds are relay-password, certificate-chain, private-key,
trust-bundle, text-signature and html-signature. These owner-authorized
preparation commands store or compare raw bytes only; they neither validate
text/PEM contents nor activate a configuration. No profile selector applies.
There is no secret-bearing argument or command that prints stored bytes.

The protected loader opens connection, optional explicit database trust and
material in one owner/inode inventory. Passwords and private keys require
secret-file permissions; chains, trust and signatures use the public-input
policy. Secret/public inode aliases refuse even across different paths.
The connection directory's checked owner binds every input. Each input is
read under its existing raw-byte ceiling before DNS; trusted regular-file
I/O is bounded in bytes, not wall time. Raw material owners have no Debug
representation; memory cleanup makes no secure-erasure claim.

Network work shares the existing ten-second deadline and five-second DNS
allowance, with address retry only for definite transport refusal before
any possible COMMIT. Import appends the caller's retained version, refusing
replacement. Check validates the stored manifest/chunks and compares exact
bytes with the protected input, without outputting either value. A lost
COMMIT response reports indeterminate-do-not-retry. Wait for the original
backend to settle, then check that same epoch/version/kind/input. Missing
while the original backend could still commit is not rollback evidence.
Keep the intended input available until resolution; do not generate another
version and replay. A lost stdout receipt likewise does not authorize replay.

Success JSON echoes only canonical IDs and explicitly reports
content_validated=false and profile_activated=false. Errors use fixed
categories without supplied paths, bytes or provider text. Exit codes are
zero for success, one for refusal/failure and two for invalid arguments.
The local PostgreSQL fixture covers the actual CLI, maximum and empty inputs,
protected modes and aliases, kind/epoch/mismatch refusal, duplicate
immutability and delayed-COMMIT resolution by the retained version.

## 18. Prepared profile content validation

`config::materialize::read_profile_text` consumes PreparedProfile and decodes
its relay password and signatures through the same bounded decoder as the
legacy loader. It preserves the explicit role set in ResolvedProfile.
Failures return reusable storage without validated headers; decoder scratch
is cleared under the existing hygiene contract. The wrapper has redacted
Debug output and no conversion into the legacy owned ResolvedText admission
input. Its owner size remains inside the existing configuration-state bound;
allocated_bytes accounts for that owner plus the existing backing regions,
not provider memory or total process RSS.

`TlsPolicies::prepare_profile` compiles the profile's TLS material through
the existing provider validator: selected inbound identities, required
certificate names, relay trust and optional ACME/gateway material. A
relay-only profile compiles no inbound identity. Client policy preparation
does not enable relay delivery on a profile whose relay role is disabled.
Provider failure drops the uncommitted construction reservation and cannot
publish a new generation. Existing callers and legacy preparation retain
their own input types and behavior.

These APIs accept trusted reader callbacks, not database references or
filesystem trust proofs. The future database generation loader must resolve
exact immutable material versions, bound aggregate work, validate the whole
shared/profile generation and admit implemented runtime capabilities before
publication. No serving path or operator command selects these APIs here.
Host tests cover every role combination, malformed text, failed readers,
wrong certificate names, invalid PEM and reservation recovery. They do not
extend the existing portable-stack or aggregate native-memory qualification
to these new entry points.

## 19. Prepared mail relations

Schema version 3 adds explicit mail relations, with no serving adapter
or mail write command yet. Existing SQLite tables remain production
authority until the atomic cutover. Schema initialization, inspection,
upgrade and material operations now require a UTF8 database (a different
encoding reports the fixed unsupported-version-or-checksum refusal);
text columns use the built-in C collation so byte ordering does not
depend on the installation's locale. Versions 1 and 2's SQL and
checksums are unchanged. Upgrade from either preserves the epoch and
existing material rows in the same transaction as version 3's DDL.

The account row holds the committed sequence and retained history floor as
fixed eight-byte big-endian values, preserving the full unsigned range and
lexicographic order. The floor cannot exceed the sequence. Future mutations
must lock this row and allocate their sequence transactionally; there is no
SQL sequence. Blob IDs remain reserved separately from live manifests.
Manifests contain length, SHA-256, creation time, changed sequence and exact
ceiling-divided chunk count. Bodies have no whole-message column. Chunk rows
are account/blob scoped, ordinals 0 through 511, and one through 65,536 bytes.
Empty blobs have zero chunks; manifests retain the 32 MiB ceiling. Deleting
a manifest cascades its chunks but retains its reserved ID.

The remaining relations preserve mailbox, immutable thread assignment,
message receipt provenance, membership, keyword, thread anchor, submission,
recipient, upload lease, import marker and change-history shapes. Foreign
keys include account identity; historical submission/import
identities intentionally do not require live objects. Text bounds measure
UTF-8 bytes. Existing deferred references permit one transaction to build a
complete graph, and native indexes support account-scoped keyset reads.
All tables are ordinary logged tables. Existing owner-only migration ACL
checks also cover these tables and indexes.

These are row-shape and reference constraints, not a complete publication
validator. Future typed transactions must verify contiguous chunks, exact
length and digest, mailbox graphs, immutable fields, recipient/receipt
coverage, retained history, authorization, quotas and operation receipts
before COMMIT. Upload leases are not delivery ownership: distributed route
and attempt fences, policy/device/configuration tables, runtime privileges,
and connection pools remain separate work before serving activation. No
SQLite data migration or Stalwart importer is introduced.

Disposable PostgreSQL tests exercise initial creation and upgrades, logged
relations, body bounds, deferred same-account references, empty/maximum
manifests, unsigned sequence ordering, bytewise text ordering, failed graph
rollback, chunk cascade and retained blob IDs. They do not claim the future
publication checks or cloud-provider qualification.

## 20. Shared app-password credentials

The private device_credentials module owns the existing v1 app-password
format and salted verifier construction for reuse by the PostgreSQL device
adapter. The current file registry now calls that same implementation;
there is no second verifier format or new authentication authority.
DeviceStore's public password/error types and limits remain re-exported
from store_fs. Registry publication, duplicate-ID refusal, capacity,
revocation and current-policy locking remain at the storage boundary.

Generation yields an opaque password separately from ID, salt and verifier.
Parsing yields a private, non-Debug presented credential so a future database
lookup can select its ID before comparing the account-bound verifier. A
successful comparison alone grants no authority: callers must still check
current account/device policy and revocation at the transaction/snapshot
boundary. Password and decoded-secret owners retain their existing drop-time
buffer clearing; this is not a whole-process memory-erasure claim. The hash
domain, lowercase token encoding and account/ID/salt binding are unchanged,
with a fixed golden vector and malformed-token/entropy-failure tests.

## 21. Prepared PostgreSQL device registry

Version 4 adds an owner-private devices table. The postgres_devices module
provides explicit owner-side preparation and publication APIs, plus a cold
credential check. No serving or command path calls them yet. Runtime device
privileges, HTTP authentication and authorization inside admitted mail
snapshots/transactions remain cutover work. The existing file registry
remains production authority until then.

Prepare Creation once before publishing: it owns the password separately
from the ID, salt, verifier, label, timestamp and request digest. Only the
verifier enters SQL. The device ID is the creation operation identity;
its digest binds the epoch, account and complete creation request. An
existing matching receipt confirms the original creation without another
write, including if the device has since been revoked. A mismatched digest
refuses. Creation retains the existing 256-device account ceiling, including
revoked records. There is no delete/reuse API. Labels retain the existing
UTF-8 byte/control-character rules. Audit timestamps are nonnegative signed
64-bit UTC milliseconds; timestamp overflow refuses before database work.

Prepare Revocation once with a random operation ID and a digest binding the
epoch, account, device, operation and requested timestamp. First revocation
stores that receipt with the revoked timestamp. The same operation confirms
Recorded; conflicting reuse refuses. A different operation observing an
already revoked device returns AlreadyRevoked without writing or replacing
the original receipt. Revocation operation IDs are unique within an account. Neither
preparation API contacts a database, and retained values do not implement
on-disk recovery for an operator command. A lost process must not invent a
replacement creation to resolve missing output; later administration must
inspect/revoke an undisclosed credential instead.

Every writer takes the shared session migration lock and validates primary,
schema and expected epoch. It explicitly begins a read-committed,
synchronous-commit transaction with a ten-second idle-transaction timeout
and locks the account row before selecting or changing device rows. An
inherited repeatable-read snapshot could otherwise retain a stale capacity
count after waiting for that lock. Creation
checks capacity while retaining that same fence. This serializes competing
instances and supplies the future authorization-write fence: policy-aware
mail mutations must take their configuration/policy locks first, then the
account row, recheck current device/identity policy there, and retain the
fence through COMMIT. Revocation does not alter the mail change sequence.
No account lock spans peer I/O, and this module admits no mail operation.

A possible COMMIT handoff followed by connection/deadline failure returns
Indeterminate. Retain the original prepared value. Repeating it takes the
same account lock and resolves its durable receipt, so it cannot race an
unsettled original into a duplicate or replace a revoked credential. A
definite failure of a retry does not settle the original uncertainty;
retain that unresolved status until its receipt is observed. A
known receipt is success even if the final local poll crosses the deadline.
Receipts live with the bounded, undeleted device records; no shorter receipt
expiry permits identity reuse. Account/schema restoration still requires a
fresh epoch under the deployment-wide recovery barrier.

check uses one fresh read-only repeatable-read primary snapshot and current
revocation state, preserving dummy verifier work for unknown IDs. It closes
that snapshot before return: a successful result grants no authority to a
later mail read or write, and is never a credential cache. Future serving
admission must perform credential/policy checks inside the same retained
snapshot or publication transaction as its mail operation. A malformed token
can be denied without database access; database failures never authenticate.
Read snapshots also set the ten-second idle-transaction timeout.
All operations retain the ten-second worker, statement/lock deadlines and
128-frame/1 MiB input bounds. Provider errors and secret values remain
redacted.

## 22. Selected-profile database material adapter

The inert postgres_profile_materials loader connects the selected-profile
text and TLS consumers to immutable PostgreSQL material versions. The
existing selected-profile grammar still calls these references file paths;
this adapter accepts only `/postgres-material/` followed by one canonical
32-character lowercase version ID. It performs no filesystem opening, URI
resolution, alias lookup or fallback. Text and TLS target kinds determine the
expected stored kind; sharing a version between incompatible roles refuses.
Every read checks the configured epoch, primary, schema, complete chunks and
digest before returning owned bytes to the existing decoder or TLS provider.

Use one loader for the text and TLS stages of one selected profile. All reads
share its original absolute deadline, at most 256 requests and an 8 MiB
aggregate reservation charged at each requested kind's maximum raw length.
Failure poisons the loader. These are cold-operation bounds, not aggregate
native-allocation qualification. The adapter does not cache versions or
weaken the material reader's per-connection limits. It provides no stronger
secret-memory erasure guarantee than the existing material consumer.

Managed ACME requests have no explicit immutable version and refuse here;
future ACME publication must supply its own fenced database mapping. The
adapter does not admit a complete shared/profile generation, select an active
pointer, grant runtime database privileges or open service admission. The
legacy filesystem loader remains production authority until the atomic
cutover. Database version references are used only through this explicit
prepared-profile adapter, never through the legacy serving path.

## 23. Shared/profile source composition

The inert config::composition boundary separates shared account, domain,
alias, identity and identity-address stanzas from profile server, resolver,
logging, relay, certificate/ACME, gateway/peer, listener and resource stanzas.
The shared source owns the single version-1 root declaration. A profile must
begin with a section and cannot add or override shared policy. Neither source
may supply paths: host-local storage/runtime/log placement remains bootstrap
input. This establishes source ownership, not implemented distributed route
selection or database generation publication.

Source checks both borrowed byte slices through the existing bounded syntax
reader and refuses a combined size above the existing 2 MiB source ceiling,
including its inserted LF. It copies no source bytes. Its composed reader
adds that LF even when the shared source ends without a newline, so a final
comment or assignment cannot consume the first profile section. Callers must
then drive load::read_profile through EOF; that stage applies aggregate line
limits and every existing field, role and structural reference check.
Partition diagnostics identify the shared or profile input without values.

Composition grants no material, complete-generation or serving authority.
Every profile still needs the database material adapter, provider checks,
generation-wide coverage and implemented-role admission before publication.
The existing selected-profile restrictions on MTA-STS/local MX, managed ACME
and unused certificates remain; composition does not weaken them to accept
a cross-profile policy it cannot yet validate.

## 24. Complete source candidate validation

The inert postgres_configuration preparer checks one shared source and
1–16 named profiles before producing an owned Candidate. Profile names use
the existing lowercase ASCII identifier grammar and must be unique. Their
canonical order is bytewise name order. The total shared/profile source,
profile-name bytes and composition separators cannot exceed 2 MiB.

For every profile, preparation composes the same shared policy, drives the
structural loader through EOF with its explicit roles, resolves text inputs
from the expected PostgreSQL epoch, and compiles every configured TLS policy
through the provider. One absolute deadline covers the whole operation,
including its database reads, within the existing ten-second cold-worker
ceiling. Each profile retains the material adapter's request/byte bounds.
Failure returns no candidate. The shared source partition prevents profiles
from replacing account, domain, alias or identity policy; existing per-profile
MX/STS/role rules remain unchanged. This does not expand implemented runtime
roles or supply the outstanding distributed ACME/route policy.

Candidate retains raw sources and material references, never resolved
password/key bytes or compiled TLS policies. Its SHA-256 digest binds the
expected epoch, shared source, profile count and every canonical name, role
set and profile source using fixed-width length framing. There is no Debug
or automatic serialization of its source bytes. Explicit borrowed access is
for the future owner-side database publisher and loader.

No configuration row, active pointer, device policy or listener changes here.
The candidate is not serving authority and its checks describe preparation
time only: a later startup/load must revalidate current database material,
certificate validity, schema/epoch and implemented-role admission. Database
persistence and atomic generation activation remain separate boundaries.
These cold input/count bounds are not native-allocation qualification.

## 25. Immutable configuration source versions

Schema version 5 appends owner-private config_versions and
config_version_chunks tables. Earlier DDL/checksums remain unchanged.
The postgres_configuration_store boundary accepts only a validated Candidate
and a retained ConfigurationVersion ID. It serializes exactly the existing
canonical digest input, including domain and epoch, into 64 KiB chunks.
The 2 MiB source bound plus 1 KiB framing allowance permits at most 33 chunks.
No resolved password, key or provider policy is serialized.

Publication takes the shared schema lock, checks the epoch, and inserts the
manifest/chunks in one explicit READ COMMITTED transaction with synchronous
commit enabled and a ten-second idle-transaction timeout. The version's
primary key serializes competing publishers. A retained ID with the same
length, digest and chunk count resolves its durable receipt; differing bytes
conflict. Any failure after COMMIT may have been handed off is Indeterminate.
Keep the original ID and candidate; another failed attempt does not settle
an earlier uncertain outcome. No active pointer or service admission changes.

Reading uses one bounded primary REPEATABLE READ snapshot. It checks manifest
bounds, exact chunk count/order/length, and the digest before decoding. The
length-framed decoder rejects truncation, trailing bytes, noncanonical names,
ordering, roles and source limits. After closing that snapshot it reruns the
complete source preparer, including current material and provider checks,
under the original absolute deadline. A stored digest is not permission to
skip validation. Immutable material IDs and epoch checks bind those later
reads; owner corruption fails closed. Successful publication receipts assume
the owner preserves immutable rows and do not revalidate historical content.

Only this closed configuration worker has 4 MiB aggregate input/output and
512 input frames, enough for a full candidate. Per-frame 128 KiB and the
SCRAM sequence remain unchanged, as do default, material and schema worker
budgets. This is a finite cold operation, not persistent service transport
or native-allocation qualification. Runtime privileges, activation and the
atomic SQLite/file-authority replacement remain separate requirements.

## 26. Operator configuration preparation

`td-mta postgres config-version` generates a fresh ID to retain before any
publication attempt. `config-import --connection-file PATH --epoch HEX
--version HEX --shared-file PATH --instance NAME ROLES PATH` validates and
publishes one immutable source version. Repeat --instance for every named
profile, up to 16. ROLES is a comma-separated unique selection of receive,
jmap and relay; it declares structural intent, not implemented capabilities.
All profile sources refer to already imported immutable material IDs using
section 22's reference syntax. No source version becomes active here.

One protected input inventory binds the connection, database trust, shared
source and all profile files to the bootstrap owner. Sources are public
operator inputs under the existing stable trusted-namespace contract; they
cannot alias the secret connection file, including through hard links.
Inventory names/counts are checked before files are opened. Source/name bytes
and inserted separators share the preparer's 2 MiB ceiling while loading;
an extra byte or a later file exceeding the remaining allowance refuses.
Sources are not printed. Complete candidate validation and publication share
one absolute ten-second DNS/network deadline after cold file loading.

`config-check --connection-file PATH --epoch HEX --version HEX` loads the
exact stored version and repeats current material/provider validation. It
needs no local source files and never falls back to them. Both commands emit
bounded JSON with epoch, version, digest and profile count on success, and
explicitly report profile_activated=false. Errors expose fixed categories,
not paths, credentials, source values or provider text. Exit status is zero
for success, one for validation/database/input refusal and two for arguments.
Only definite transport refusal permits another resolved address, under the
original deadline. Indeterminate publication stops and reports
indeterminate-retain-version. Keep the same ID and source set, wait for the
original operation to settle, then check/retry that retained version. A
failed check (including changed or missing material) is not evidence that a
previous publication rolled back. Never allocate another ID to hide an
uncertain result. These owner commands do not supply runtime privileges,
listener activation or PostgreSQL serving admission.

## 27. Durable account bootstrap

Schema version 6 adds owner-private account_bootstraps receipts. The inert
postgres_accounts boundary prepares one retained epoch/account/Inbox tuple,
then atomically inserts account sequence one and floor zero, the shared
INITIAL_INBOX contents, its Mailbox Created history at sequence one/operation
one, and the receipt. The default row is shared with the existing SQLite
bootstrap so both produce the same logical initial state. No existing
account is reset, populated from configuration or granted serving authority.

Publication holds the shared migration lock and verifies the epoch before
one explicit READ COMMITTED transaction with synchronous commit enabled and
a ten-second idle-transaction timeout. Insertion of the account primary key
serializes competing creations. A matching receipt resolves the retained
creation without touching later sequence, mailbox or history changes. A
different Inbox conflicts; an existing account without a receipt refuses
rather than attempting repair. The receipt binds account/Inbox for that
epoch and remains for the account's lifetime, independent of current mailbox
contents and retained history. It deliberately has no mailbox foreign key.
Account retirement/receipt removal is not implemented by this boundary.

As with device/configuration publication, an error after COMMIT may have
been handed off is Indeterminate. Keep the original Creation, let the
original attempt settle, then inspect/retry that same tuple. Inspection
uses one finite primary read-only REPEATABLE READ snapshot; absence while
the original is still running cannot settle uncertainty. Inspection checks
its final deadline before returning. A retried publication that finds a
matching retained receipt succeeds even if its last poll crosses the local
deadline.

These explicit owner operations neither activate account policy nor bypass
future account limits, authorization, quota or mutation fences. Configuration
activation must separately bind its declared accounts to existing prepared
accounts before serving. Existing unreceipted accounts are not adopted, and
this is not a SQLite or Stalwart migration interface. The real PostgreSQL
fixture checks exact initial rows, idempotency after later changes, conflicts,
rollback after partial writes, uncertain commits and competing bootstraps.

## 28. Authenticated mailbox observations

`postgres_mailboxes::list` is an inert worker-side storage operation, not a
selected runtime backend. It takes the expected epoch/account and app password,
verifies the primary/schema barrier, and begins one read-only repeatable-read
transaction. The shared device verifier runs inside that transaction, before
account state and mailbox contents are read. A revoked or wrong-account token
returns no mailbox data. A revocation after snapshot admission may allow that
bounded observation to finish; its returned device ID grants no later authority.
Configuration/profile admission and runtime credentials remain cutover work.

The result owns at most 256 mailbox rows, in binary ID order, with account
sequence/floor and total/unread Email and Thread counts from the same snapshot.
Unread excludes both seen and draft Emails; a Thread is unread when at least
one member in that mailbox is unread. A closed query limits returned rows to
257 to detect overflow without returning a partial listing. The mailbox-only
transport allows 512 frames within the ordinary one MiB input and 128 KiB
individual-frame ceilings; other operations retain their previous budgets.
Database aggregate work is subject to the statement and whole-operation
limits, not a promise of bounded server scan cardinality.

Rows pass shared mailbox-format validation, sequence checks and bounded parent
hierarchy validation. Count inputs must have valid referenced Emails/Threads
and change positions no newer than the captured sequence. Missing/cyclic
parents, future changes, invalid fields or capacity exhaustion fail the entire
observation. Retained strings use fallible bounded allocation. The absolute
caller deadline (at most ten seconds) covers connection, authentication, SQL
and decoding, and is checked again before any result escapes. The connection
and snapshot close before return; results cannot be extended into a later
snapshot or used as a mutation authorization.

## 29. Verified Email body snapshots

`postgres_body::with_email` prepares the message-reading boundary without
selecting a production backend. Its expected epoch, account and Email ID do
not grant authority: it verifies the primary/schema barrier and app password
inside one read-only repeatable-read transaction before resolving that Email's
body. Only an Email in that account admits a body; upload leases and standalone
transmission blobs remain outside this operation. Configuration/role admission
and bounded runtime pools are still cutover work.

Before callback admission it validates the captured account sequence/floor,
Email/Thread/Blob references and change positions, length and chunk count. It
reads contiguous 64 KiB chunks in batches of at most eight, checks exact lengths
and ordinals, and verifies the entire SHA-256 before admitting the callback.
Later reads rely on PostgreSQL preserving those bytes in the same snapshot;
chunks are not independently hashed again on replay. Storage/server corruption
after verification is outside that guarantee. Empty bodies still require the
correct empty digest. The caller
may impose a smaller size ceiling within the existing 32 MiB maximum.

The synchronous BlobReader callback retains that exact database snapshot and
migration lock. Reads reuse at most eight cached chunk rows, so small MIME
reads do not refetch a whole database chunk every time. Cache misses use the
same connection and snapshot; there is no reconnect or replacement view.
Invalid reads, database failure and expiry are sticky, including when a
callback attempts to swallow the error. The original deadline covers setup,
verification, callback work and final return. As with the current verified
blob callback, callers must bound their own CPU and I/O by that deadline.

A private connection worker can drive successive closed queries synchronously;
it never replenishes deadlines or transport budgets between queries. Existing
probe/material/configuration/schema/mailbox operations retain their original
budgets and mutation-outcome semantics. Body operations have a 128 MiB total
input budget, 8192 frames, 512 KiB output and the unchanged 128 KiB individual
frame/authentication limits. The total includes initial verification and later
reads, so sufficiently many cache misses refuse rather than consume unlimited
wire or driver memory. Cache-hit and callback-return checks drain available
connection messages without a SQL round trip and retain terminal failures; a
queued notice cannot conceal a subsequent connection error. The worker tracks
actual socket byte progress beneath TLS: consuming an encrypted header can
return Pending before yielding plaintext, so it retries immediately after
that progress and parks only when the socket made none. The same rule drives
connection setup and query polling, without extending any allowance. Database query
failures report I/O failure; corruption remains a row-shape or digest failure.
This is a finite cold operation, not a reusable pool or
an assertion that future HTTP transfer limits are already qualified.

## 30. Retained configuration selection

Schema seven adds a singleton `config_selection` row and immutable
`config_selection_receipts`. The initial row has generation zero and no
version. `postgres_selection` supplies owner-side preparation, publication,
receipt inspection and selected-source reads. This is still inert storage
preparation: it opens no listener and supplies no runtime policy, role or
device authorization. Production startup and policy fencing remain cutover
work.

A retained change binds a random operation ID to the store epoch, expected
full-u64 generation, immutable version ID and validated source digest.
Publication first resolves an existing receipt under a shared singleton lock
that excludes an earlier publication still committing, then rereads and revalidates
all profiles and materials under its original absolute deadline. The write
transaction locks the singleton before any future account/device locks,
checks the receipt again after waiting, compares the generation and stored
source digest, and increments the generation with checked arithmetic.
Pointer and receipt commit together with explicit synchronous commit and
read-committed isolation. Competing changes from the same generation cannot
both publish. A failed candidate or definite rollback leaves the pointer
unchanged.

The original ten-second operation ceiling covers every connection, validation
and transaction; none renews it. A lost COMMIT response is indeterminate.
The publication transaction uses the bounded mutation transport profile
(1024 input frames, 1 MiB input, 128 KiB output), including schema ledger
qualification and COMMIT. Receipt inspection and passive observation retain
the smaller probe profile.
Keep the exact retained change and resolve/retry it, never generate another
ID from an absence observation. Lock/connection failures while resolving
leave any previous uncertainty outstanding; they are not proof of rollback.
The shared resolution lock is released before material validation. Historical
receipts resolve their original
selection even after subsequent changes or material expiry; receipt success
does not assert that version is still selected. At most 1024 receipts can be
published, checked under the same singleton lock. There is no receipt expiry
or pruning API yet; capacity refusal preserves existing outcomes. This finite
preparation limit needs an explicit retention/retry policy before runtime
administration activation. Changes currently survive only in the caller’s
retained typed object; a durable encoding is required before administration
can recover that object across process restart.

Selected-source reads capture a passive generation/version, then verify the
immutable source bytes and all profiles/materials. A concurrent publication
can supersede that generation before return. Consumers must fence current
policy at runtime admission/publication rather than treat the returned
candidate as authority. Store-epoch checks apply to each connection, including
receipt resolution after restore. Schema migration keeps earlier checksums
unchanged and initializes selection without choosing an existing version.

## 31. Selected-profile runtime preparation

`postgres_runtime_profile::load` is a cold bridge from selected database
sources to an owned resolved profile and caller-reserved TLS generation.
It retains a passive binding of epoch, selection generation/version, account,
profile name and roles. It does not open listeners, publish a runtime
configuration, enable relay/ACME workers or provide device authorization.
No conversion to the legacy file-serving input is introduced.

Loading captures the selected version and revalidates every profile/material,
then resolves the requested profile using the exact same composition, text
material and TLS preparation path as candidate validation. It requires the
configured account's durable bootstrap receipt. All material comes from the
database under the original deadline; there is no filesystem fallback.
The caller's TLS generation reservation remains in its original domain and
is released on failure. The returned resolved text contains secrets and has
no diagnostic dump. Its explicit binding retains passive identifiers, limits
and the selected sending identity IDs/mailboxes. At most 64 mailboxes of at
most 254 bytes are copied from the validated configuration; binding clones
share that immutable catalog. Diagnostic formatting redacts the mailboxes.
The header and SMTP recipient limits come from the same selected profile.
Identity lookup is configuration evidence, not sending authority: submission
publication must freshly fence the selection and current device. No relay
credentials or signature material enter the binding.

Before returning, a final primary observation rejects a selection already
superseded during preparation. This does not freeze future changes: runtime
startup/publication must still fence policy afresh and admit only implemented
roles. The original at-most-ten-second deadline covers source verification,
repeated profile preparation, account inspection and final observation.
The source candidate is discarded before the final check; retained text and
TLS objects belong to the caller's cold control worker. Reusable connection
pools, publication, live refresh and full resource qualification remain
cutover work.


## 32. Retained single-Email flag writes

Schema version 8 adds owner-private flag_receipts. The cold postgres_email_set
boundary assigns optional seen/starred values to one existing Email. It is
not wired to production JMAP. Section 33 adds separate batch entry points;
section 43 adds single-destination mailbox replacement.
Preparation binds an unpredictable operation ID and canonical digest to the
profile binding, account, device, Email, optional expected sequence and flag
assignments. The retained object contains no app password.

Fresh writes take the shared migration lock, then a shared configuration
selection row lock, then the account write lock, and authenticate the device
inside that read-committed transaction. Selection generation/version and JMAP
role must still match the prepared profile. Account locking also serializes
revocation. The expected state is compared under the same lock. Changed flags,
Email history, Mailbox history for seen changes (including drafts), account
sequence and receipt commit atomically with synchronous_commit=on. Star-only
changes do not change Mailbox or Thread history. No-ops retain a receipt without
advancing sequence. Full unsigned 64-bit state is preserved; overflow refuses.
At most 256 memberships are admitted for a seen change.

Owner-side receipt inspection takes an account shared lock before looking for
the result, excluding an earlier writer's still-pending COMMIT. Historical
receipts resolve even after policy changes or revocation; this does not
authorize another mutation. The API is not a client authentication endpoint.
A missing receipt never authorizes a new operation identity after uncertainty.
COMMIT loss returns Indeterminate and requires retaining the original Change.
At 1024 receipts per account fresh operations refuse; there is no pruning or
unbounded recovery promise. Runtime retention/retry policy and bulk JMAP
mapping remain cutover work. All calls retain finite worker/deadline limits.
An inspection that crosses its deadline refuses absence/errors, but preserves
a confirmed historical receipt as known success.


## 33. Bounded flag batches

Schema 9 adds per-Email results to the shared flag receipt relation; prior
single-Email receipts retain their digest and state and gain one Updated
result. Existing DDL remains unchanged. The single-Email API preserves its
request digest framing, missing-Email refusal and result shape.

postgres_email_set::prepare_batch accepts at most 256 unique Email IDs in request
order, with optional seen/star assignments and one optional expected state.
The batch has one retained random operation and a separate digest domain.
inspect_batch and publish_batch return per-Email Updated or NotFound results
in request order, plus the original and resulting account sequence. Empty
batches, missing Emails and already-satisfied assignments do not advance the
sequence; their result is still retained. State mismatch refuses the whole
batch. Single and batch entry points reject each other's prepared operations.

One transaction and the existing policy/account/device fences cover the whole
batch. Set-oriented array queries load at most 256 Email rows and 257 distinct
Mailbox rows including the overflow sentinel. At most 256 affected Mailboxes
are admitted. Keyword writes and history use bounded arrays rather than a
round trip per Email. A changed batch advances state exactly once. Each changed
Email and each affected Mailbox receives one history entry, even when several
Emails share a Mailbox; Thread history is unchanged. Receipt results, history,
flags and state commit together. The 1024-receipt account limit covers both
single and batch operations. Lost COMMIT recovery retains the full result.

The closed mutation worker admits 1024 input frames, one MiB total input,
128 KiB per frame and 128 KiB output. Both single-Email and batch publication
use this worker. Other workers retain their own bounds;
SCRAM qualification and the original ten-second maximum deadline are unchanged.
The finite larger frame count covers the two result sets and protocol overhead.
Production JMAP dispatch and serving retention remain cutover work; this API
is not a public authentication endpoint.

## 34. Shared logical quota policy

Every profile in a complete PostgreSQL configuration candidate must agree on
five logical account limits: `disk.body_bytes`, `disk.blob_count`,
`limits.upload_disk_bytes`, `limits.queue_disk_bytes` and
`limits.queue_submissions`. Defaults participate in the comparison. A mismatch
refuses the whole candidate, including when reading a retained version or
preparing a selected runtime profile. These remain existing profile fields;
operators changing a shared limit update all profiles in the same version.
Other process-local resource settings may differ between profiles.

The candidate and selected-profile binding expose the validated common limits
as passive values. Canonical source bytes already bind every explicit setting,
so neither the version encoding nor the schema changes. A historical selection
receipt remains historical success; it does not bypass revalidation for new
runtime preparation. Lowering limits is configuration policy, not deletion or
capacity reclamation. Runtime admission must compare durable usage and pending
reservations against the selected limits before accepting more work.

This closes conflicting instance policy before shared accounting is activated.
It does not yet implement database quota counters, reservations, publication,
reclamation or serving. Those remain prerequisites for the atomic cutover.

## 35. Retained upload publication and shared usage

Schema version 10 adds `account_usage` and `upload_receipts`. Migration derives
one usage row per account from existing bodies, leases and submissions in the
same transaction as its ledger entry. Expired leases and completed submissions
remain charged while retained; a body shared by submissions is charged once
for queue bytes. New account bootstrap writes an all-zero usage row atomically.
Every future body/reference mutation must adjust these counters under the
account lock in the same transaction. Owner-side direct SQL is outside this
application-maintained accounting contract.

`postgres_uploads` is a cold, bounded publication API for completed upload
bytes. Preparation checks the selected profile's message ceiling (at most
32 MiB), copies and hashes the body, allocates one Blob ID and operation ID,
and binds the request digest to the epoch, account, configuration selection,
profile, device, Blob ID, exact length/digest and configured upload lifetime.
Clones share the single immutable body allocation. The preparation deadline is
at most ten seconds. No password is retained or body exposed through Debug.
This owner API does not admit remote ingress or reserve runtime memory slots.

Publication qualifies a primary, holds the schema migration lock, then takes
selection FOR SHARE and account FOR UPDATE in that order. It rechecks the
selected generation/version and current device credentials, enforces all five
shared quotas, and publishes body manifest/chunks, a Both-use upload lease,
quota charges, the next full-u64 account sequence and the retained outcome in
one synchronous transaction. It creates no Email or Thread. Blob IDs remain
permanently reserved. Chunks are at most 64 KiB, sent in batches of eight;
no database parameter carries a whole maximum-size message. The closed upload
worker admits 40 MiB output and the mutation worker's one MiB/1024-frame input
budget, including the same authentication/per-frame checks. Its original
ten-second deadline covers the complete publication operation.

Lease creation and expiry use the database clock and the profile's configured
upload expiry interval. Receipts retain Blob ID, length, timestamps and old/new
state. Inspection takes account FOR SHARE before reading, excluding an earlier
publication still committing. Repeated publication resolves the same operation
before new policy/device admission; a matching historical receipt is known
success, not current permission to download or import. A mismatched digest or
malformed receipt refuses. Lost COMMIT response is Indeterminate: retain the
same prepared upload and inspect/retry it after the original settles. The
1024-receipt account bound is preparation-only until runtime retention and
retry lifetimes are defined together.

Body and upload byte charges and the blob count advance atomically, with no
process-local fallback. Lowered limits refuse additional publication when any
current usage exceeds its cap. Logical counters do not reserve server disk.
Runtime ingress reservations, cleanup with fenced reclaim, HTTPS download
dispatch, SMTP publication and serving activation
remain cutover work. This API does not make the SQLite/file-backed server use PostgreSQL.

## 36. Authenticated blob downloads

`postgres_body::with_blob` shares the Email reader's bounded chunk verifier
and replay cache. Its read-only repeatable-read snapshot binds the account
state, manifest, chunk count and bytes. It accepts a current JMAP profile and
device credential. An Email or transmitted-submission reference in that
account admits the body; otherwise the snapshot must contain an unexpired
upload lease owned by that device. A Blob ID or upload receipt alone grants
no access. Lease expiry uses the database clock.

Before calling the consumer, the complete body must pass SHA-256 verification.
A separate primary transaction then rechecks the store epoch, current profile
selection and device credential under selection/account shared locks, in that
order. It checks the retained upload expiry against current database time.
Revocation or policy replacement during verification therefore refuses the
callback. Once admitted, a callback can finish after revocation or retirement
within its original deadline; it retains the verified snapshot throughout.
No read retries onto a different snapshot after output begins.

Both connections share the original at-most-ten-second operation deadline;
each retains its existing finite frame and byte budgets. Callback errors,
swallowed reader errors, expired deadlines and failed snapshot connections
cannot become success. This cold boundary does not yet connect production
HTTPS dispatch or replace runtime connection and worker admission.


## 37. Retained td-mail draft creation

Schema version 11 adds bounded historical `draft_receipts` and an index for
Email/Thread identity collision checks against retained history. Each account
retains at most 1024 draft receipts until runtime retry and retention policy
are activated. Explicit PUBLIC revocation also applies when the operator has
permissive default table privileges.

`postgres_drafts` prepares the plain-text structured draft used by td-mail's
Email/set creation. It reuses the existing composer, selected profile work
limits and at-most-32-MiB message limit. Composition and SHA-256 finish before
opening the publication transaction. The immutable prepared object retains
body bytes, operation/object IDs and a digest binding the selected policy,
account, device, mailbox, optional expected state and exact body. No app
password is retained. Preparation and publication each have an original
at-most-ten-second deadline; retry never extends an in-flight operation.

Publication follows selection FOR SHARE then account FOR UPDATE, rechecks
current policy and device authorization, and requires the selected mailbox
and optional account state. It publishes body chunks, Email, one membership,
$draft/$seen keywords, thread anchor, quota counters, account sequence and
receipt in one synchronous transaction. Body and blob-count usage increase;
no upload lease or queue charge is created. Upload and draft publication
share the same private chunk writer and quota calculation. Thread selection
checks nearest References, then In-Reply-To, then own Message-ID, with at
most 32 entries per reference list and lowest Email ID for an anchor tie.
Existing threads are never merged. The one new account sequence records
Email creation, Thread creation/update and Mailbox update.

Receipt inspection excludes a still-committing operation with an account
shared lock. Matching historical success survives policy replacement and
credential revocation without authorizing a new write. Lost COMMIT response
requires retaining and resolving that same operation. Failure before commit
rolls back bodies, ID reservation, metadata, usage and receipt together.
This remains a cold storage API: HTTP dispatch, sender-identity policy,
submission, runtime admission and the atomic production cutover remain work.


## 38. Authenticated change-history pages

`postgres_changes` reads detached Mailbox/Thread/Email/Submission change
pages under one authenticated primary repeatable-read snapshot. It checks
the selected JMAP profile generation/version, epoch and current device,
then binds the account sequence and retained history floor. State tokens
convey no authority. Wrong account/epoch, future or pruned state refuses.

The existing `jmap_changes` transaction-aligned merger now accepts a small
history-source interface; the SQLite source keeps its existing checks and
semantics. PostgreSQL seeks the native account/kind/sequence/operation index
in batches of at most 128 rows. Each row validates ordering, sequence,
operation, action and object identity. The merger preserves whole sequence
groups, folds repeated object changes and rejects a first group exceeding
the requested page limit. At most 1024 final changes plus one bounded
transaction group are retained. An empty kind advances to the pinned account
sequence without scanning unrelated kinds.

All polls, including cached rows and completion, check the original
at-most-ten-second deadline and terminal connection state. This cold API
uses the existing read worker's 128 MiB/8192-frame input and 512 KiB output
ceilings without replenishment between batches. The merger gets at most
4096 polls, leaving room below the wire-frame ceiling for setup and batch
replies. If that work bound prevents completion, return cannotCalculateChanges
so td-mail refreshes its state instead of retrying the same folding history
forever. Other connection/deadline failures still refuse the whole page;
no partial results escape or resume on a different snapshot.
Authorization and policy are admitted at snapshot start; the finite read
may finish after concurrent revocation. HTTP dispatch and runtime connection
admission remain part of the production cutover.


## 39. Shared Email read snapshots

`postgres_read::with_snapshot` admits the selected JMAP profile and current
device, then retains one primary repeatable-read snapshot across Email
metadata, mailbox membership, keyword windows and verified body callbacks.
The common private snapshot admission helper also serves change-history
reads. All results bind the same account sequence and history floor. The
handle and connection cannot escape the outer callback; returned passive
values confer no authority on later operations.

Email metadata reads validate the manifest, chunk count and referenced thread
without reading MIME body bytes. Membership and keyword windows are ordered,
exclusive-after, and limited to 256 values plus one validated overflow row.
An empty window alone does not prove Email existence. Membership reads check
the referenced mailbox and both change sequences; keyword values use the
existing local syntax validator. Body callbacks reuse the same complete
SHA-256 verifier and chunk replay cache as existing downloads, on this
snapshot's connection and original deadline.

Every method and final callback completion checks the original connection and
deadline. A failed operation retires the whole view, including when its caller
swallows the error. Missing Email metadata remains a normal absent result.
Concurrent writes or retirement cannot splice new metadata or body bytes into
an admitted read. Current profile/device authority is checked at admission;
revocation does not extend or invalidate an already admitted finite snapshot.
The existing read-worker byte/frame caps apply across all methods and bodies,
without replenishment. This API adds no runtime listener, pool or backend
selection; production dispatch and atomic SQLite removal remain pending.

## 40. Snapshot-bound Email query pages

`postgres_read::Snapshot::query_emails` supplies the existing td-mail inbox
query shapes: all mail or one mailbox, received date ascending/descending,
nonnegative position, a bounded page and optional total. Equal dates always
sort by ascending Email ID, preserving the existing query interpretation.
A foreign wire mailbox can be represented by the explicit empty scope.
No text-search, thread collapse or new client feature is added here.

The page and its query state use the same authenticated repeatable-read
snapshot as metadata and body reads. Closed SQL filters by account and
mailbox before ordering; optional totals count the same filtered relation.
The page ceiling is 1024 IDs plus one validated overflow row. Zero limit is
valid; a position above PostgreSQL's signed row-count range returns an empty
page while retaining the requested position and optional total. Returned rows,
including overflow, validate ordering and change sequences. Failed query
operations retire the view even if callers swallow the error.

Server statement limits and the original worker deadline bound sorting,
counting and offset work; worker byte/frame limits remain shared across the
whole view. No retained cursor or backend activation is introduced. Production
JMAP dispatch and the atomic replacement remain pending.

## 41. Snapshot-bound thread member windows

`postgres_read::Snapshot::thread_emails` reads one thread's members in
ascending received-date and Email-ID order, strictly after an optional
coordinate. Shared passive `ports::ThreadEmail` and `ThreadEmails` values
also serve the existing SQLite reader; its algorithm is unchanged. The
window ceiling remains 256 members plus one validated overflow row.

An absent stored thread returns `None`; an empty stored thread returns an
empty window, leaving the existing JMAP rule that hides empty threads to the
protocol adapter. Thread and returned member change sequences must not exceed
the snapshot. All reads, including repeated windows after concurrent deletion,
retain the same account state, worker budget and deadline. Invalid limits or
other read errors retire the view. This supplies storage for td-mail's
Thread/get without activating PostgreSQL production dispatch.

## 42. Retained root-folder creation

Schema version 12 adds `mailbox_create_receipts`. The
`postgres_mailbox_create` adapter prepares at most 256 root-folder names,
using the same bounded NFC/control/noncharacter validator as the existing
SQLite adapter. Invalid names become the existing empty-name marker before
retaining and hashing the canonical request. Preparation retains random
operation/object IDs and no device password. It runs outside the transaction
under the selected profile's work limits and original deadline.

Publication fences selected policy, account state and current device in the
same order as flag writes. A locked account serializes exact root-name
uniqueness; existing names and later duplicates in the batch report
AlreadyExists, while invalid names report InvalidName. Successful rows use
no parent or role, sort order zero and subscribed true. IDs must not collide
with current mailboxes or retained mailbox history. All created rows,
Mailbox/created history, one account-sequence increment and ordered results
commit atomically. A batch with no created rows retains an unchanged state.
The same account lock enforces the listing limit of 256 total mailboxes.
A batch exceeding that limit refuses without retaining a receipt or changing
any mailbox. No generic hierarchy, rename or destroy flow is introduced.

The complete receipt binds the canonical request and generated IDs and is
checked before reauthorizing a fresh write. Owner-side inspection waits
behind account publication; lost COMMIT response remains indeterminate until
resolved with that same operation. Each account retains at most 1024 receipts;
expiry/reclamation remains part of cutover work. Folder transactions have a
one-MiB output ceiling for bounded names and a one-MiB/1024-frame input budget.
Preparation, publication and replay do not enable PostgreSQL serving.


## 43. Atomic Email mailbox replacement

The existing `postgres_email_set::Update` now accepts an optional existing
account mailbox alongside seen/star assignments. A batch replaces each
successful Email's memberships with that one target. Missing Emails return
NotFound before destination validation; missing destinations return
InvalidMailbox and leave that Email's flags and memberships unchanged.
Already-satisfied moves are no-ops. Removing other memberships while retaining
an existing target preserves the target membership row and its sequence.

Batch digest domain v2 includes target presence and ID; existing single-Email
flag preparation is unchanged. The same schema-9 receipt format admits the
new per-object result byte, validated against the prepared move request.
No schema migration or production dispatcher change is needed.

The account lock serializes moves, flags and account history. Set-oriented
queries inspect at most 256 memberships plus an overflow sentinel per moved
Email, returning only bounded aggregates. At most 256 affected Mailboxes are
returned across the batch. SQL work remains subject to the original deadline.
Mailbox history covers removed/added memberships and all previous memberships
whose unread count changes, deduplicated across Emails. Retained targets need
no Mailbox history unless seen changes. Changed Emails each get one history
entry, Thread history is unchanged, and the account advances once. All rows,
history and the ordered receipt commit together; uncertainty resolves through
the original operation. Existing mutation-worker wire bounds are unchanged.


## 44. Retained Email deletion

`postgres_email_set` owns the single-Email flag, batch update and deletion
adapters. `deletion::prepare` accepts at most 256 unique Email IDs and retains
one opaque operation with a separate digest domain and no password. Its
inspect/publish entry points return ordered Destroyed/NotFound results; empty
or entirely missing batches retain a no-op receipt. The existing flag_receipts
relation now covers all three modes under one 1024-receipt account limit.
Receipt result decoding rejects values from another operation mode. No schema
migration is needed, and production serving remains unchanged.

The common transaction fences policy, account state and current device before
fresh deletion. Bounded Email, affected-Mailbox and Thread result sets each
admit at most 256 rows. Per-Email keyword inspection admits 256 plus one
sentinel, and multiple thread anchors refuse. SQL work remains deadline-bound.
Each removed Email receives destroyed history, each affected Mailbox updated
history, and each touched Thread updated or destroyed history according to
remaining Emails. One account sequence covers the complete batch.

Memberships, keywords, anchors and receipt-recipient rows are removed with the
Email. A body retires only after its last Email disappears and no retained
lease or submission owns it, including expired leases and completed
submissions. Removing a body cascades its chunks and decrements body-byte and
blob counters in the same transaction; upload/queue counters remain charged
for their surviving owners. Counter underflow or inconsistency refuses the
entire transaction. Blob identity tombstones remain, preventing ID reuse.
Existing repeatable-read snapshots retain deleted metadata and body bytes.

Lost COMMIT response remains indeterminate until the same retained operation
resolves. Historical receipts remain inspectable after later policy change,
revocation, object deletion or body retirement. This does not grant a fresh
write or a new body read. Reclamation of retained leases, submissions and
operation receipts remains cutover work.

## 45. Shared receiving header preparation

The storage-independent `receiving_headers` module owns the existing bounded
root-header retention, Return-Path removal, supplied trace insertion and
Message-ID/References/In-Reply-To selection used by receiving. The SQLite
delivery adapter now consumes it through an explicit error mapping. Its
64 KiB output turns, source bounds, deadline checks and original work-meter
charges are unchanged. The module grants no recipient, gateway or publication
authority and performs no database access. PostgreSQL receiving publication
and runtime integration remain cutover work.

## 46. Retained SMTP receiving publication

Schema version 13 adds `receiving_receipts`. The owner-side `postgres_receiving`
adapter prepares completed decoded DATA outside its write transaction. The
trusted connection owner supplies envelope/socket facts and must already have
admitted the peer, gateway and recipient routes under the selected profile.
Passive receipt facts confer no authority. This increment opens no listener and
does not replace the production SQLite receiving path.

Preparation validates bounded SMTP metadata, complete CRLF lines of at most
1000 bytes, the selected message ceiling and a root-header ceiling of at most
1 MiB. It uses the shared receiving header and trace formatter: prepend the
server Return-Path/Received fields, strip supplied Return-Path fields, retain
root identifiers and keep body bytes unchanged. Original work/deadline limits
cover filtering and identifier replay. Input reserves the trace allowance
within the message ceiling. Prepared bytes, IDs, provenance and the request
digest remain immutable across retries; no network input occurs under a lock.

Publication takes the configuration share lock before the account write lock,
rechecks epoch and selected profile generation, and requires its receiving
role. It resolves exactly one Inbox, selects the thread using the shared draft
anchor lookup, and publishes body chunks, SMTP provenance and ordered envelope
recipients, one unread/unstarred Email, membership, anchor, history, shared
quota effects and the operation receipt in one synchronous transaction.
Each delivery advances account state once; repeated envelope recipients still
produce one Email. Body and blob quotas are charged once. Retained receipts
are capped at 1024 per account and survive deletion of the delivered Email.

Lost COMMIT responses are indeterminate. Inspect/replay waits behind the prior
account publisher and uses the retained operation/digest; known success stays
success after policy replacement. This is internal delivery recovery, not
cross-session SMTP deduplication. Runtime connection/gateway authorization,
ingress reservations, retention/reclamation and activation remain cutover work.

## 47. Verified submission input preparation

The inert postgres_submission_input worker freezes td-mail's implicit
envelope and outgoing copy from one authenticated, verified Email snapshot.
It selects the sending mailbox only through the immutable profile binding,
checks optional account state, and retains the original Email/blob/thread
identity. No database write, queue reservation or SMTP responsibility occurs.

A single original deadline and work meter cover header collection, envelope
derivation and transmitted copying; the whole-body integrity pass is prepaid
before the reader starts. Shared MIME scanning locates the bounded root header
section. Existing submission_envelope and transmitted_headers enforce the
configured author identity and recipient ceiling and strip root Bcc/Resent-Bcc.
All remaining body bytes are copied without interpretation. Reads and copying
use 4096-byte turns; header scratch is at most the configured header ceiling
plus two separator bytes. The retained copy is at most 32 MiB and clones
share it; at most 1000 copied recipient mailboxes are retained. Original
stored bytes remain unchanged.

The result contains passive policy/source evidence and no password or live
snapshot. It grants no continuing device authority. Publication must
recheck the source identity, optional state, selected generation and device
under the account transaction, enforce shared quotas, and atomically publish
the outgoing blob, submission, recipients, history and operation receipt.
Deletion, revocation or a configuration change after preparation can therefore
refuse publication. Section 48 supplies the owner-side enqueue transaction;
runtime memory admission remains separate work.


## 48. Retained submission publication

Schema version 14 adds owner-private submission_receipts. The inert
postgres_submissions worker assigns random operation, submission and blob IDs
to a verified prepared input, retaining its bytes and a domain-separated
request digest across retries. No client endpoint or relay worker is enabled.

Publication locks configuration before the account, then freshly checks the
selected generation/version, JMAP role, device and optional state. The source
Email must still reference the verified blob/thread and manifest. It charges
one outgoing body/blob, queue bytes and one submission against shared quotas.
A database UTC timestamp supplies sendAt and checked five-day expiresAt.
Outgoing chunks, submission, ordered initially Queued recipients, account
sequence, submission-created history, usage and receipt commit together.
The visible Email and its blind-recipient headers remain unchanged.

Receipts are bounded to 1024 per account and retained independently of the
source Email or queued submission. Concurrent publication of one retained
operation serializes through the account row. Lost COMMIT responses remain
indeterminate until primary receipt inspection resolves them; a digest
mismatch refuses replay. Historical success survives later revocation, source
deletion or configuration replacement. Deleting the source after enqueueing
does not cancel delivery or retire its separately owned outgoing body.
Runtime admission, receipt reclamation, dispatch, fenced multi-instance queue
ownership and activation remain cutover work.

## 49. Selected named-route evidence

`postgres_routes` builds bounded, redacted route evidence from a validated
profile. Whole-generation preparation rejects conflicting definitions of a
shared name; profile loading retains the selected route in its binding.
`serves_route` is only passive eligibility evidence and requires the relay
role plus matching name and destination. It grants no lease or continuing
policy authority. Section 50 supplies queue persistence; worker ownership remains subsequent
work. The production single-process backend is unchanged.

## 50. Submission route persistence

Schema version 15 adds owner-private `submission_routes`, one account-bound
mapping per queued submission. New enqueue transactions write the submitting
profile, selected route name and destination fingerprint atomically with
outgoing bytes, recipients, quota charges, history and the operation receipt.
The retained request digest explicitly binds the route and destination.
Failures while inserting either route or receipt roll back the entire enqueue.

The mapping has no foreign key to the current configuration: removal or
replacement of a profile cannot rewrite an already queued destination.
Source Email deletion leaves it intact; submission retirement cascades its
mapping. Migration preserves older inert queue fixtures without inventing a
route. A queue row lacking a mapping is ineligible for dispatch and requires
explicit repair; workers must never infer a default from their own profile.
This increment still enables no worker or client endpoint.

## 51. Route-scoped outgoing body snapshots

The inert `postgres_delivery_body::with_submission` worker requires a selected
relay role and opens a finite read-only primary snapshot. It rechecks epoch
and configuration selection, then joins the queued submission's stored route
name and destination to the worker's selected route. Missing mappings, wrong
routes and different destinations cannot expose an outgoing body. Account
state, changed sequences, manifest and chunk coverage must be valid.

The shared body verifier checks the entire bounded outgoing blob before the
callback receives its reader. Verification and replay share the original
snapshot and deadline. No Email or device credential is needed: queued
responsibility survives source deletion and device revocation. The database
credential and selected relay profile authorize this internal read. A read
is passive evidence, including after a policy change during its snapshot; it
grants no attempt ownership or right to transmit. Fenced claim and phase
publication remain required before SMTP work.

## 52. Inert route-owner coordination

Schema version 16 introduces `route_owners` and retained
`route_owner_receipts`. `postgres_route_owners` coordinates one owner per
account and route name, even across destination changes. Fresh claims
require a selected relay role. Acquisition locks configuration, account and
route in that order, rejects an unexpired owner, and increments a nonzero
64-bit fencing generation without wrapping. A retained random operation and
digest make uncertain acquisition inspectable and replayable without a new
claim. The serialized receipt recheck precedes current-policy rejection,
so a concurrently committed acquisition remains resolvable after selection
changes. Route rows and acquisition receipts each have a 1024-per-account cap.

A fixed 30-second database-time lease binds the process boot identity, claim
operation, route destination and generation. Renewal checks every field,
current selected policy and unexpired ownership under lock. Release compares
the same owner and fence but can clear an expired lease or removed policy;
its caller must first close its SMTP transport. Release retains the fencing
counter. Historical acquisition receipts are never live authority: `observe`
checks current policy, identity, fence and database expiry. Unknown renewal
or release responses remain indeterminate; fresh primary observation resolves
whether that owner remains live. Internal coordination does not advance JMAP
state or mutate recipient responsibility.

Database time earlier than the last recorded owner clock refuses. This is a
local row check, not complete failover/clock-jump qualification. Runtime
heartbeat scheduling, monotonic transport deadlines and conservative clock
validation remain activation work. No lease enables SMTP by itself; atomic
recipient Prepared publication and every subsequent phase still need the
same route-owner fence. Takeover recovery must preserve AcceptancePossible
uncertainty before any replacement attempt. These workers remain inert.


## 53. Inert first-attempt publication

Schema 17 retains `submission_attempts`: one historical receipt per random
attempt, with a unique active attempt per account and route name. The inert
`postgres_attempts` worker rechecks selected policy, route destination and
current unexpired owner fence under configuration/account/route locks. It
selects at most 100 due, never-attempted Queued recipients from one supplied
submission in ascending ordinal order. Expired, completed, mismatched and
already attempted responsibility cannot enter this first-attempt path.

Recipient InFlight/Prepared state, attempt identity, timestamp and count,
submission/account sequence, change history and the retained attempt commit
atomically. Receipt resolution precedes fresh policy checks; lost COMMIT
responses resolve with the same retained Dispatch. Historical outcomes do
not authorize transport. At most 1024 attempt receipts are retained per
account; reclamation remains later maintenance work.

An active attempt blocks another batch even after owner replacement. This
increment never clears that block, resumes retries or opens SMTP. Fenced
phase/result publication and conservative takeover recovery must resolve it
before another attempt. Clock validation and monotonic retry guards remain
runtime activation requirements. The supplied submission is selected by a
future bounded scheduler; no unbounded due-queue scan is introduced here.


## 54. Inert abandoned-attempt recovery

Schema 18 adds retained `attempt_recovery_receipts`. A current route owner
may recover an active attempt only from a strictly older fencing generation
with the same route and destination. Account/route locks exclude stale phase
writes. Up to 100 remaining InFlight recipients resolve atomically with
submission/account history, active-attempt retirement and one receipt.
Historical replay precedes current policy and ownership checks.

Prepared and Body recover with Network reason, preserving prior uncertainty.
AcceptancePossible always becomes uncertain OutcomeUnknown with Uncertain
reason. No recovery produces Accepted or erases replies, attempt count or
attempt identity. Retry times use the queue contract's clipped uniform jitter
with at most 32 entropy draws; failure rolls back. Recipient updates use one
bounded SQL array batch rather than one protocol exchange per recipient.

This path handles only work with room for a retry before expiry. It refuses
near-expiry/overdue responsibility as ExpiryHold without clearing the active
attempt or declaring terminal failure. Clock-confirmed expiry remains a
separate activation prerequisite. A backwards last-attempt timestamp refuses.
Runtime clock discontinuity validation and the five-minute monotonic restart
guard still precede any actual retry. Receipt storage has a 1024-per-account
cap. No SMTP, scheduler or retry dispatcher is activated by this worker.

## 55. Runtime database credential preparation

`postgres runtime-check --connection-file PATH` checks the current schema and
one dedicated runtime login's effective data privileges. It closes the
connection before returning and neither grants privileges nor activates a
profile. Startup/reconnect integration remains cutover work; a successful
cold check is not authority for a later connection.

The initial runtime login must be separate from the owner, have no role
memberships, and have none of SUPERUSER, CREATEDB, CREATEROLE, REPLICATION or
BYPASSRLS. It must not own the database or application objects or have database CREATE or
application-schema CREATE privileges. This dedicated-login policy also
refuses membership that currently disables inheritance or SET ROLE, rather
than depending on those options remaining unchanged. Database TEMP privileges
are outside this application-schema boundary. Provider administration and
arbitrary externally installed SECURITY DEFINER functions are not audited by
this check; database administrators remain trusted.

The exact table privilege roster lives in `postgres_runtime_access::TABLES`.
All current tables require SELECT. Mail data and receipt tables require
INSERT, UPDATE and DELETE, while schema, immutable configuration, selection
receipts, bootstrap receipts and reserved import/lease tables are read-only.
The selection singleton permits only column-level UPDATE on `singleton`,
which allows FOR SHARE policy fencing without changing its generation/version.
Application tables must be ordinary logged tables without row security.
Extra/missing tables, grant options, TRUNCATE, REFERENCES, TRIGGER and MAINTAIN
privileges refuse. Column grants are checked as well as table grants.

Provision the dedicated login through PostgreSQL/provider administration;
keep its password out of argv. For a fresh `td_mta_runtime` login, the owner
can grant the schema-18 privileges atomically as follows:

```sql
BEGIN;
GRANT USAGE ON SCHEMA td_mta TO td_mta_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA td_mta
    TO td_mta_runtime;
REVOKE INSERT, UPDATE, DELETE ON
    td_mta.schema_migrations, td_mta.config_materials,
    td_mta.config_material_chunks, td_mta.config_versions,
    td_mta.config_version_chunks, td_mta.config_selection,
    td_mta.config_selection_receipts, td_mta.account_bootstraps,
    td_mta.leases, td_mta.imports FROM td_mta_runtime;
GRANT UPDATE (singleton) ON td_mta.config_selection TO td_mta_runtime;
COMMIT;
```

This is a fresh-role setup, not repair of inherited or preexisting grants.
Validate with the runtime connection specification afterward. Owner commands
retain their separate credential. Before a schema upgrade, drain/stop all
instances and revoke runtime schema/table/column grants; the existing private
migration ACL check still applies. Reapply the new version's exact grants and
recheck before restart. Do not install default privileges on future tables.

The cold check uses the existing bounded listing transport (512 frames,
1 MiB input, 64 KiB output), retaining the original ten-second maximum
operation deadline and the shared migration lock through inspection.
