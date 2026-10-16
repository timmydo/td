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
(1088 input frames, 1 MiB input, 128 KiB output), including schema ledger
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
publication must freshly fence the selection and current device. JMAP bindings
also share the bounded visible Identity catalog, including public text/HTML
signature bytes. No relay credentials or signature source paths enter the
binding; diagnostic formatting redacts the visible catalog.

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

The closed mutation worker admits 1088 input frames, one MiB total input,
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
worker admits 40 MiB output and the mutation worker's one MiB/1088-frame input
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
one-MiB output ceiling for bounded names and a one-MiB/1088-frame input budget.
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
receipts, bootstrap receipts and reserved import tables are read-only.
Upload leases require SELECT and INSERT only; UPDATE and DELETE refuse.
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
GRANT INSERT ON td_mta.leases TO td_mta_runtime;
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

## 56. Qualification on each runtime connection

The internal database request distinguishes preparation from runtime mode.
Preparation remains the default of the protected bootstrap parser and all
operator commands. Runtime mode authenticates TLS/SCRAM, checks primary and
durability settings, acquires the shared migration lock, validates the current
schema and checks the dedicated runtime role/table privileges before admitting
an operation's SQL. A new connection repeats all of those checks; a previous
cold receipt never substitutes for them. The lock and connection remain held
through the operation. Mail helpers still enforce their own epoch, current
configuration, device, snapshot and publication fences.

Qualification failure closes the connection before the application callback
runs, with separate redacted schema/role/privilege errors. It performs no
reconnect, publication retry or deadline renewal. An operation trying to
resolve an earlier uncertain publication still retains that uncertainty when
qualification refuses; refusal does not prove the earlier operation rolled
back. Schema initialization and grants remain explicit owner operations.

Runtime connections use at least 1024 frames to include qualification and
the operation in one budget; mutation workers admit 1088 frames. Larger existing
frame budgets, byte budgets, per-frame/authentication caps and output budgets
are unchanged; qualification does not replenish any allowance. The original
absolute deadline covers connection setup, qualification and application work.
No runtime pool, listener or automatic retry is activated by this increment.
The real PostgreSQL fixture loads both profiles and exercises read/unread/star
mutations, batch receipts and lost-COMMIT handling through the dedicated
runtime login; privilege changes are refused on the next connection.

## 57. Request credential ownership

`http_auth::parse_basic` separates bounded HTTP Basic decoding and exact
configured-username matching from storage authentication. Its owned
`BasicCredentials` retains at most 352 decoded bytes, has no Debug or Clone
implementation, and clears its decoded buffer on drop. It contains presented
bytes only, not a device ID, cached verifier result or authorization grant.
The existing local `basic` adapter uses this same parser and still checks the
local registry before returning a device ID.

At PostgreSQL cutover the bounded HTTPS request worker may retain this owner
for that request only, borrowing its password for fresh primary snapshot and
publication checks. It must not cache the owner across requests or replace
those checks with the result of earlier authentication. TLS, configured-origin
and attempt admission precede parsing. Invalid password syntax still goes to
the backend's credential check; HTTP decoding is not app-password validation.
No PostgreSQL listener or alternative production backend is activated here.

## 58. Mailbox projection from the admitted read snapshot

`postgres_read::Snapshot::mailbox_listing` reads the bounded mailbox hierarchy
and counts through the same authenticated connection and repeatable-read
snapshot as Email metadata and bodies. Its original policy/device admission,
account state, deadline and connection budgets remain in force. It reuses the
closed mailbox query and validators of the separate mailbox observation;
errors retire the view. At most 256 mailboxes escape, with overflow refused.

`mailbox_response::encode_object` projects a checked passive Mailbox row and
snapshot-bound counts without opening storage. Both the local adapter and
PostgreSQL use this encoder and the shared `MailboxCounts` value. Counts must
match the supplied account/epoch/state identity and mailbox ID. This catches
accidental cross-view substitution; passive values are not authorization.
The caller retains and checks the original admitted view before publishing
output. The local wrapper still checks its snapshot before and after encoding.
This prepares existing JMAP output for cutover; it activates no listener.

## 59. Stored Email projection on the admitted snapshot

The stored-field encoder consumes `email_response::Source`, implemented by
the existing local view and PostgreSQL's admitted `Snapshot`. Both adapters
share the passive metadata and value-window types; the native adapter remains
a forwarding wrapper over its existing reads. PostgreSQL maps its body
manifest length to stored size and uses its existing snapshot queries for
mailbox memberships and keywords. No MIME parsing or body reads are added.

The encoder checks the retained view around every bounded sink callback,
keeps each emission at 512 bytes or less, and pages sets in windows of at
most 256 values. Metadata and every window must name the requested Email;
windows must match the metadata snapshot identity. Output stays provisional
until successful completion and the enclosing request's final checks.
PostgreSQL read failures retain their typed errors; sink/format errors use
`postgres_read::Error::Output` and do not imply database publication.
The same-snapshot fixture exercises stored output across a concurrent Email
removal, a later absent object, multi-page keywords and a refusing output sink.
Service admission remains part of the atomic cutover.

## 60. Worker-owned read views and fixed admission slots

`postgres_read::ReadSlots` reserves 1..256 process-local read connections.
Acquisition uses at most eight atomic compare/exchange attempts and returns
Busy on exhaustion or contention, without a queue, connection attempt or
retry. The service must size this pool from its admitted resources and keep
health/reconnect and settlement capacity separate. This is a read-slot
primitive, not the full service's outage or connection-pool activation.

`Snapshot::open_runtime` requires Runtime connection mode and acquires a
slot before connecting. It returns an owned, thread-confined snapshot after
primary/schema/privilege qualification and fresh profile/device admission.
The connection drops before the slot is returned. Failed opening releases
both. Neither the deadline, protocol budgets, snapshot identity nor sticky
read failure can be renewed by retaining or borrowing this owner. The worker
must drop it when the bounded method finishes; server idle-transaction expiry
and local checks remain additional limits. No live view crosses worker threads.

The existing `with_snapshot` preparation helper uses the same implementation
with one local slot and still bounds custody to its callback. Its caller
continues to own outer worker admission. This preserves preparation fixtures
without allowing owner credentials through the runtime constructor. The real
read fixture checks exhaustion before a second connection, server-side closure
on drop, slot recovery after failed authentication and owner-mode refusal.
No serving listener is activated by these ownership changes.

## 61. Verified body custody for existing MIME readers

`Snapshot::with_pinned_body` lends the existing opaque `PinnedBlob` to a
bounded callback only after PostgreSQL's full length/chunk/digest check.
It retains the same snapshot worker, body manifest, Blob ID and original
finite deadline. The internal source boundary is crate-private; external
callers cannot turn an arbitrary `BlobReader` or passive metadata into a pin.
Reads and explicit freshness checks preserve sticky failures, and callback
completion checks the pin again. No callback result grants output after
expiry, connection loss or a swallowed pin failure.

`postgres_body::with_pinned_blob` provides the same custody for downloads.
It retains the existing account-reference/upload-lease authorization and
fresh primary reauthorization after verification, before lending the pin.
The compatibility `with_blob` entry delegates to it. The download fixture
checks pin identity/digest alongside its original empty, multi-chunk and
maximum-body replay and cross-device/revocation refusals.

The shared pin still lives at its existing `store_fs` API path during
preparation, so root-header, MIME structure, body-value and attachment
readers need no parser changes or replacement authority interface. Its local
variant retains the same incremental SQLite handle, verification clock and
drop custody. The PostgreSQL variant borrows the verified chunk reader;
it adds no body copy, renewed work meter or deadline. Both representations
are preparation plumbing, not a selectable production backend. The atomic
cutover removes the local variant and relocates the surviving boundary.

The real read fixture collects root headers through the existing MIME
reader after another connection deletes the source Email/body. It checks
pin identity/digest, original body bytes, sticky expiry even when swallowed
by the callback, and refusal to grant a pin for corrupt bytes. The existing
MIME/allocation fixtures continue to qualify the local representation.

## 62. Shared verified Email MIME projection

`jmap_request::dispatch::write_verified_email` runs the existing Email/get
header, preview, body-list, body-value and attachment-locator encoders over
a verified pin supplied by either storage adapter. It emits comma-prefixed
members beside the caller's stored fields and framing, with the same bounded
scratch, 4096-byte buffered output and original work meter. It checks source
custody before entry and after final flush; MIME readers retain the pin
through their own bounded turns. Message size is checked before projection.
The shared boundary reconciles the original meter after every projection
result, preserving deadline/capacity classification through nested MIME
errors. There is no new HTML/MIME feature or alternate parser.

`EmailProjection` retains at most 64 bounded header-property names and the
existing field selections. Inclusion uses the original property parser and
work allowance; `stored()` supplies the same stored-field projection used
by the local dispatcher. The trusted `EmailOutput` owner must enforce its
original response admission/deadline and retain provisional bytes. A failed
projection cannot publish its partial output. PostgreSQL integrity verification
continues to use the original body worker's finite connection budgets before
entry; projection does not renew them or reverify/copy the whole body.

The local dispatcher performs its existing funded verification then invokes
this shared projection. The PostgreSQL snapshot fixture invokes it after a
concurrent source deletion and checks subject/address, decoded preview/text,
body lists and locator output, plus a refusing response sink. Full native
validation still exercises the real td-mail client against the active local
service. PostgreSQL protocol/service activation remains the atomic cutover.

## 63. HTTPS authentication off the network reactor

The active HTTPS adapter now hands admitted credential work to one fixed
authentication worker through a bounded backend interface. This prepares
PostgreSQL lookup without putting connection, authentication or SQL work on
the network reactor. Origin/rate admission precedes the handoff; lookup and
completion retain the request's original finite deadline. Client-owned
credentials survive the initial device observation for subsequent fresh
storage authorization. Ordinary responses and EventSource clear them before
output; other client retirement also clears them.

The current service still supplies its local device registry. This increment
activates no database-backed listener and grants no authority from parsing
alone. The cutover must supply runtime-qualified primary lookup, fresh
method authorization and service readiness/backoff, including its independent
health/settlement connection capacity. DESIGN.md and RESOURCES.md own the
worker lifecycle and active resource boundaries.

## 64. Request credentials at the body-worker boundary

Upload, download and API requests now borrow the original bounded Basic
credential owner for the synchronous backend call. The initial device ID
remains an observation; PostgreSQL consumers must freshly authenticate the
presented bytes under their operation's original deadline and policy fence.
The handoff copies no secret and allocates no credential storage. Completion
clears the owner before close, drain or refusal handling, including failed
backend calls. EventSource retains its existing credential-free contract.

The active local backend still uses its device registry guard. The HTTPS
fixture checks the presented password against the observed device on the
body worker for all three operations, including its second-device requests.
This handoff prepares PostgreSQL operation adapters without activating them.

## 65. PostgreSQL HTTPS authentication adapter

The prepared `postgres_http_auth::Authentication` implements the fixed HTTPS
worker's credential interface. It resolves the protected bootstrap hostname
anew on each lookup, forces runtime mode on every connection, and checks the
selected profile, store epoch and device under one fresh primary snapshot.
Only transport failures try another bounded resolved address. The connection
and snapshot close before returning a device observation; later operations
still require their own fresh authorization and publication fence.

One atomic permit bounds concurrent lookup, including DNS; contention returns
Busy without a queue. The original HTTPS deadline converts through the same
RuntimeClock origin without a fresh allowance. A ten-second lookup ceiling
and five-second DNS ceiling only shorten it. Results are refused after expiry.
Malformed and inactive/wrong credentials return no device; configuration,
schema, privilege and connection failures remain service errors, not invalid
password observations. Errors carry no endpoint, secret or provider text.

This adapter remains inactive until the atomic service cutover supplies its
bootstrap, binding and shared clock. Service readiness/backoff and independent
health/settlement capacity remain cutover obligations. The real PostgreSQL
read fixture checks valid, malformed, wrong and revoked credentials, expired
deadlines, rejection of owner credentials and subsequent successful lookup.
A held table lock checks concurrent Busy refusal, finite lock failure and
in-flight deadline expiry, followed by successful lookup after release.

## 66. Response retention without a mail database

The prepared `ResponseCoordinator` owns one cold, empty RequestSpool and a
process-local response quota ledger. Construction consumes the spool and
refuses occupied or retired slots; it opens no mail database and grants no
mail authority. Its only reservation surface is response bytes. PostgreSQL
remains responsible for durable quotas; the local ledger cannot reserve
bodies, uploads or delivery responsibility.

Both the active local StoreCoordinator and this prepared owner use the same
retention implementation: full-cap reservation before file creation, original
work/deadline accounting, prepaid mutation output, and unlink-before-credit
cleanup. Failed cleanup retains the full charge and retires the slot; ledger
failure stops admission. The prepared owner refuses the legacy native-view
and external-sort hooks. PostgreSQL method adapters must supply their own
authenticated snapshots and bounded queries rather than accidentally opening
a local view.

The existing Core dispatcher runs against this owner without constructing a
mail database. Fixtures cover complete echo output, quota saturation, prepaid
output, successful cleanup, replacement-file uncertainty and sticky clock
failure. Production still supplies the local coordinator until atomic cutover;
the temporary owner enum and legacy hooks are preparation plumbing.

## 67. Prepared PostgreSQL Mailbox/get dispatch

`dispatch::run_with_postgres_read` accepts a freshly authenticated runtime
snapshot and a response retention owner. Core/echo and connected Mail methods
use the existing whole-request admission, argument parser, result references,
method-error handling and retained response encoder. Unconnected methods return
unknownMethod at this preparation boundary; they never fall through to the
local database. Production remains on the existing backend until cutover.

Mailbox/get obtains bounded names, hierarchy and counts from that snapshot,
uses its identity for state, and uses the shared object encoder. Explicit
IDs retain request order and duplicate/notFound behavior; null IDs select
the bounded mailbox list. Snapshot checks precede and follow the operation
and the batch. All response bytes remain provisional until those checks
succeed. Read-only fixtures advertise read-only rights; the prepared HTTP
flag endpoint in §78 advertises seen/keyword rights explicitly. The temporary
read seam and native path are removed at cutover.

The dispatcher converts its execution tick using the response owner's
RuntimeClock and tightens both the snapshot and SQL worker deadlines before
any dispatch. This can only shorten the original allowance; a subsequent
call cannot extend it or clear a terminal error. Nested mailbox expiry keeps
its executionDeadline classification.

Losing the snapshot is terminal for this prepared read batch, including
execution-phase expiry: the final snapshot check discards the entire retained
result and returns the deadline error. Argument and unsupported-method errors
still produce per-method replies while the snapshot remains usable. The
runtime HTTP adapter must settle such terminal refusal without publishing
any prefix of the discarded batch.

## 68. Prepared PostgreSQL Email/get dispatch

The prepared read dispatcher also connects Email/get to its authenticated
snapshot. The existing argument parser, selections, ID planning, result
references, response framing and notFound handling remain shared with the
local adapter. Stored fields use the shared Email encoder; MIME and header
fields use the existing verified-pin encoder. No new client features or
interpretation rules are introduced.

Null IDs use one bounded PostgreSQL query page; an account larger than the
method object allowance refuses rather than silently truncating. Requested
IDs retain their order and deduplication. Body verification prepays the
existing per-method byte/record allowance before obtaining the PostgreSQL
pin. The snapshot additionally retains its connection budget and shortened
execution deadline. A projection/output refusal does not poison a healthy
snapshot; body integrity or connection failure remains terminal. Provisional
bytes are published only after both projection and final snapshot checks.

## 69. Prepared PostgreSQL Email/query dispatch

Email/query reuses the existing td-mail argument parser and result framing
with the PostgreSQL snapshot query. The supported inMailbox filter,
receivedAt ordering with ascending-ID ties, position, bounded limit and
optional total retain their existing semantics. Query state comes from the
same admitted snapshot as the returned IDs; query changes remain unsupported.

The PostgreSQL path requires no local external-sort files: database query
limits and the original snapshot worker budget bound reads, and the shared
response owner bounds retained JSON. Final snapshot and execution checks
precede publication. A result reference into the query IDs feeds the same
snapshot's Email/get without client-visible storage-specific handling.

## 70. Exact worker expiry and transport ticks

The PostgreSQL worker's original Instant deadline remains authoritative
through connection, qualification and query polling. Its TLS transport uses
whole-millisecond ticks, so the initial remaining allowance rounds upward
when deriving that secondary cap. Rounding downward could expire TLS first
by a fraction of a millisecond, losing the worker's deadline classification
in a generic driver error. This does not extend the worker allowance: each
poll still checks the exact original Instant, and phase tightening can only
shorten it. An unrepresentable transport tick is refused.

## 71. Shared HTTPS API framing

The existing HTTPS API backend now delegates bounded body reception, JSON
validation, response preflight and final retained-response encoding to a
shared exchange helper. The backend still reserves storage before body
reception and supplies the same fresh authorization at the receive boundary
and before dispatch. All exchange, transfer, idle and execution deadlines
remain anchored to their existing start points.

This prepares the PostgreSQL HTTP backend to reuse the exact request and
response path. The helper owns no database authority and selects no backend;
production service construction remains unchanged pending atomic cutover.

## 72. Reserved fresh runtime reads

`Snapshot::open_resolved` reserves a process-local read slot before starting
DNS, then resolves the protected bootstrap endpoint afresh and forces runtime
connection qualification. The same slot and original finite allowance cover
resolution, all attempted addresses, authentication and snapshot ownership.
Only transport connection failures advance to another resolved address;
policy, credential, schema and database failures have no fallback.

The selected bootstrap profile must match the prepared binding. Resolution
has a five-second cap within the original request allowance; the existing
ten-second worker ceiling can only shorten that allowance. No connection or
DNS queue is created, and all failed paths release their slot after closing
owned work. Health and settlement retain independent admission capacity at
runtime integration. This entry prepares HTTP workers; it activates no role.

## 73. Prepared HTTP read backend

`postgres_http_api::Service` connects the fixed HTTPS body-worker interface
to the shared bounded HTTP framing and PostgreSQL read dispatcher. It remains
inert until the atomic cutover. Its prepared Session, profile binding and
resource plan must agree; HTTPS, response retention and dispatch share one
RuntimeClock. It implements Core/echo, Mailbox/get, Email/query and Email/get;
uploads, mutations and EventSource remain unsupported here.

Original credentials stay borrowed through body preparation and fresh primary
snapshot authentication. The observed device ID must match their syntax, but
only the fresh snapshot grants read authority. One reserved read slot covers
fresh DNS, runtime-role qualification and the whole batch under the original
execution deadline. Final snapshot validation precedes response output; the
connection and read slot drop before headers, while temporary response quota
remains charged through output and cleanup. No database work runs on the
network reactor, no success escapes a failed snapshot, and uncertain response
cleanup stops admission. No new listener or production command activates it.

Shared HTTP framing keeps the execution deadline strictly before the outer
response deadline, including short original request allowances. It caps
execution one clock tick before the outer limit without extending either
budget. This preserves response retention's phase-ordering contract; the
one-tick separation does not promise transmission time after expiry.
## 74. Prepared HTTP downloads

The prepared HTTP backend serves raw blobs and direct MIME-part locators
through the existing shared download encoder. Target account and observed
device must match before connection admission. It reserves two read slots
before fresh DNS: one for the verified body snapshot, one for fresh primary
reauthorization before the callback. Both runtime connections qualify the
database role. The snapshot, MIME processing and transmission share the
original deadline capped at ten seconds; no reconnect renews that budget.

Whole-body digest verification and account-reference/upload-lease policy
remain `postgres_body::with_pinned_blob` responsibilities. Forged locators
are checked against the verified MIME tree and decoded length is counted
before headers. A single foreground work meter covers interpretation and
replay; no full-message resident buffer is introduced. Both connection
reservations survive callback output and are released after their sockets
drop, including failed writes. A pool with fewer than two free slots refuses
immediately. Uploads and production activation remain unimplemented.

## 75. Prepared PostgreSQL Thread/get dispatch

The prepared read dispatcher accepts td-mail's explicit Thread/get IDs using
its existing argument validation, projection, result references and response
encoder. Ordered membership windows come from the same authenticated
PostgreSQL snapshot. Missing or empty threads are notFound; state and final
snapshot checks bind the complete retained result to that view. The original
execution deadline and output allowance cover every page.

Omitted or null IDs are refused as invalidArguments on this prepared path;
td-mail supplies explicit IDs, and account-wide thread enumeration is outside
this increment. Production remains on the local backend until atomic cutover.
The real-client PostgreSQL fixture also retrieves the delivered message by
its thread ID and checks the returned ID and body.

## 76. Prepared PostgreSQL object changes

Mailbox/changes, Email/changes and Thread/changes use the existing
transaction-aligned history merger on the authenticated batch snapshot.
The shared argument parser, state validation, page encoding and errors stay
unchanged. The PostgreSQL reader does not open another connection or refresh
its snapshot between methods. Each history poll charges the original HTTP
response work allowance and checks the execution deadline; the existing
4096-poll history bound also applies.

A foreign, stale or wrong-kind state returns cannotCalculateChanges without
poisoning a healthy snapshot, so later methods can continue. Database,
integrity, work and deadline failures remain terminal for this prepared read
batch. Final snapshot checks precede publication. Production activation and
mutation coordination remain separate.

## 77. Read boundaries for prepared method coordination

A successful Snapshot::finish_method consumes and closes the PostgreSQL
connection before returning a BetweenMethods reservation. No body pin can
survive that transition. The connection slot remains reserved until the next
method reopens it or drops the reservation; this is conservative capacity
accounting, not a retained database transaction.

Reopening performs fresh DNS, runtime-role qualification and full device
secret authentication against the original profile/account binding. It
captures a new account state. The original deadline is capped once at read
admission and never extended by closing or reopening. The reservation stores
no password and refuses a different device ID. Every failed reopen releases
its capacity. A failed snapshot cannot produce a reservation.

This supplies a boundary for future mixed read/write batches. Existing
read-only HTTP dispatch retains its single snapshot; no mutation endpoint
or production PostgreSQL service is activated here.

## 78. Prepared singleton flag requests

The prepared HTTPS backend admits td-mail's standalone Email/set keyword
updates through the shared bounded request parser and response reservation.
It closes its authenticated read snapshot before mutation and reserves a
connection slot before fresh DNS. Runtime connections check the selected
profile, account, device secret and optional Email state under transaction
locks. Read-only batches retain their existing single snapshot. Mixed batches
cannot invoke this mutation adapter. Draft creation remains outside this
prepared endpoint; §80 connects mailbox moves and §81 connects deletion.

One process-lifetime Pending owner serializes flag mutations without waiting.
An uncertain COMMIT retains the exact Change and refuses new flag writes
with a nonfatal busy response while reads continue;
no new operation ID or transparent replay replaces it. Known outcomes survive
late local deadline expiry. The owner contains no password. Production
startup, receipt settlement/reclamation and restart recovery remain cutover
requirements; this adapter is not a production serving entry point.

The real td-mail PostgreSQL fixture now marks read, stars, marks unread and
unstars the delivered message, querying fresh state after every result. It
also forces a COMMIT error and verifies a later write refuses without losing
the original operation or stopping reads. Mailbox rights advertise seen and
keyword updates; §80 adds membership rights while structural rights stay false.

## 79. Prepared flag receipt settlement

Pending::settle inspects the exact retained Change under runtime-role
qualification and the existing account lock that excludes a still-running
COMMIT. It uses a separately supplied settlement connection allowance, fresh
DNS and one original deadline capped at ten seconds. It borrows no app
password and does not authorize or retry a mutation.

A confirmed receipt returns its operation ID and complete original outcome,
then releases the flag-write fence. Absence, deadline, DNS or database failure
preserves the original Change and fence. A confirmed success survives a late
local deadline poll. The caller must retain the returned outcome as required
by its own recovery workflow. Production scheduling, restart persistence and
receipt reclamation remain cutover requirements.

The real-client fixture checks absence does not clear uncertainty. A second
prepared owner loses a COMMIT acknowledgement to a deadline and later settles
the committed receipt while both foreground read slots are held, proving that
settlement has separate connection admission.


## 80. Prepared moves between existing mailboxes

The singleton Email/set adapter also accepts td-mail's mailboxIds replacement.
It forwards the one selected mailbox to the existing bounded PostgreSQL batch
transaction. Device/profile/state fences, mailbox existence checks, membership
replacement, Email and affected Mailbox history, and the original receipt
remain atomic. Unknown destinations are per-object invalidProperties, and
membership changes retain the original Email, Thread and body identities.

Prepared HTTP mailbox rights advertise add/remove alongside seen/keyword
updates. Section 82 connects root-folder creation. Rename, folder deletion and
submission remain unsupported.
The same uncertainty owner and settlement mechanism cover combined flag/move
requests without new retry identities. The real-client fixture prepares an
Archive folder through the existing storage helper, moves Inbox to Archive
and back, checks database membership and unchanged flags, and queries the
same Email/body ID after each move. This does not activate production serving.


## 81. Prepared Email deletion

The singleton Email/set endpoint connects td-mail's destroy action to the
existing PostgreSQL deletion transaction. It retains distinct update and
deletion operation types through publication and receipt settlement; a
historical receipt resolves only its original operation. Both share the same
nonwaiting mutation owner and connection admission. Authentication, selected
profile, expected state, per-object absence, history, last-owner body removal
and quota release remain governed by the existing transaction.

Both protocol completions carry only the original epoch/sequence, without a
fabricated read history floor. Typed output vectors reserve their bounded
capacity before effects. Deletion prepays work for bounded keyword,
membership, Thread-owner and blob-owner observations, validates result kinds
before releasing the pending operation, and preserves known COMMIT results
through late local expiry.

The real td-mail fixture delivers a second message and permanently deletes it,
checks authoritative Email and quota state, and confirms the first message
still reads and downloads. A delayed deletion COMMIT is settled through its
original receipt and releases the final body quota. Draft creation and
production PostgreSQL activation remain separate.


## 82. Prepared root-folder creation

Singleton Mailbox/set now connects td-mail's name-only root creation to the
existing PostgreSQL transaction. Preparation spends the original HTTP work
meter for name validation and NFC; publication prepays bounded row work,
reserves result capacity and a database slot before DNS, and rechecks runtime
role, profile, account, device and expected state under the original deadline.
Mixed method batches never enter this mutation path or native storage.

A dedicated bounded owner retains one exact Creation through uncertain COMMIT.
It blocks further folder creation without stopping reads or independent Email
writes. Receipt settlement uses separately admitted capacity and never retries
creation with a new identity. Absence and errors preserve the owner; confirmed
outcomes survive local expiry. The encompassing prepared mutation owner keeps
both operation families across requests. Restart persistence, settlement
scheduling and receipt reclamation remain required before production cutover.

Mailbox completion exposes only the original epoch and sequence, without a
fabricated snapshot identity. The real td-mail fixture creates Archive, rejects
its duplicate, lists it and moves mail through it. A delayed creation COMMIT
proves exact receipt settlement and one durable folder. Hierarchy, renaming
and folder deletion remain outside this td-mail milestone.


## 83. Caller-funded draft preparation

The retained plain-text draft helper has an entry point that borrows the
original work meter and runtime clock. Composition, body hashing, retained
thread anchors and request hashing spend that same allowance; no preparation
step renews an exhausted budget. A clock wrapper checks the shorter absolute
preparation deadline on every composition turn. Both entry points retain the
ten-second phase and selected message-size ceilings. The cold convenience
entry point constructs its own configured allowance and delegates to the same
implementation.

The caller still owns memory admission for the bounded retained body, fresh
device authorization and publication capacity. This helper does not activate
HTTP creation. The PostgreSQL draft fixture checks exhausted record/output
budgets and expired original/phase deadlines refuse before publication, then
publishes a caller-funded draft through the existing receipt, body, thread and
history checks.


## 84. Prepared HTTP plain-text drafts

The singleton Email/set service now accepts td-mail's existing structured
plain-text draft shape. It captures authenticated Email state, closes the
read transaction before composition, and supplies that state to the shared
response adapter without opening a native view. Publication rechecks device,
profile, epoch, mailbox and optional sequence. Completion carries the actual
before epoch/sequence and known durable result through response generation.

The prepared owner admits one composed or uncertain draft body, bounded by the
selected message ceiling and the helper's 32-MiB cap. It is held before input
parsing or composition; the original request meter funds decoding, composition,
hashing and prepaid publication work. A foreground database slot is reserved
before DNS. No app password survives the synchronous call. Other mutation
families retain their independent bounded owners. Runtime memory-plan
qualification and durable restart ownership remain production cutover work.

Uncertain COMMIT retains the original Draft and blocks new draft creation.
Settlement uses separately admitted database capacity to inspect that exact
receipt; absence or error preserves the owner. It does not publish again.
The HTTP fixture creates and retrieves a draft, refuses a stale state without
effects, downloads its immutable message and settles a delayed commit. This
activates no production service, attachment upload or submission capability.


## 85. Caller-funded submission input

Submission input preparation can borrow the original HTTP work meter and
runtime clock. Before database entry it charges admission work and tightens
the SQL deadline to the earlier caller or original meter deadline. The same
allowance covers body verification, headers, implicit-envelope derivation,
blind-recipient filtering and the retained outgoing copy. A phase clock keeps
local processing inside the SQL preparation deadline, without extending the
original meter. The cold convenience entry point delegates to this path.

The returned input still records the submitting profile's route and grants no
queue or delivery responsibility. Connection and retained-memory admission,
publication work, fresh device authorization and receipt ownership remain the
runtime caller's obligations. The local PostgreSQL fixture verifies exhausted
work and both deadline refusals, then checks caller-funded input through the
existing privacy, source-state, route and queue-publication evidence.


## 86. td-mail submission request decoding

The inert submission_request decoder borrows the parsed JSON arena and spends
its original work meter under a non-extending phase deadline. It decodes only
td-mail's emailId/identityId creation object and success-filing patch: remove
$draft, optionally add the Sent membership and remove the original membership.
Both mailbox operands survive decoding; a later publisher must not interpret
that patch as permission to replace an unrelated current mailbox.

Literal and request-local Email references remain distinct. A valid opaque ID
is retained without claiming it names a local object. The decoder neither
resolves creation IDs nor authorizes account, identity or mailbox access.
Call-level account/state checks, creation-key binding, successful-submission
selection, publication and the separate implicit Email/set response remain
handler work. Unsupported fields and incomplete filing shapes are explicit
refusals. No Submission capability or serving path is enabled by this decoder.

## 87. Retained success filing

The inert postgres_email_set::filing helper clears $draft and optionally
removes one exact source membership and adds one destination. It preserves
other memberships and keywords. Missing source membership or destination
refuses the whole patch without clearing $draft; a concurrent unrelated move
is never replaced. Clearing an unseen draft updates Mailbox state/history
for every membership whose unread counts change, including retained folders
and an already-present destination. A seen clear-only patch changes only
Email history. An already-cleared Email records a no-op receipt without
advancing its state.

Filing shares the bounded Email receipt table, account serialization, fresh
device authorization and policy fencing. Its distinct request digest binds
both mailbox operands. Retaining the exact Filing permits historical recovery
after an uncertain commit, even when authorization subsequently changes.
Publication is a separate transaction from submission: refusal or rollback
never retracts the durable queue entry. The caller must select only known
successful submissions and report the independent implicit Email/set result.
This helper does not activate Submission or the HTTP sending flow.

The local PostgreSQL submission fixture proves rollback leaves the queue and
Email intact, exact receipt recovery after delayed COMMIT, preservation of an
unrelated membership, missing-source refusal, clear-only/no-op behavior, and
digest-conflict detection. Caller resource admission and bounded receipt
reclamation remain activation obligations.

## 88. Caller-funded submission publication preparation

The inert postgres_submissions::prepare_with_meter helper uses the request's
original work meter and runtime clock for body and request hashing. Each body
chunk, fixed request field and length-delimited envelope string is charged
before hashing, with both the original deadline and a bounded absolute phase
deadline enforced. The cold helper creates a configured allowance and
delegates. Prepared input remains shared immutable storage; this step does
not copy the outgoing body or charge a second output allocation.

The local submission publication fixture checks exhausted records, a body
larger than remaining I/O, original and phase deadline refusals, and continued
consumption of an already-used meter. Its successful result feeds the existing
concurrent publication, receipt, routing and failure evidence. This preparation
does not acquire database capacity, publish mail or enable Submission; those
remain caller obligations, alongside retained-memory admission.

## 89. Fixed-worker submission ownership

The inert postgres_http_submissions adapter admits one active or uncertain
submission per retained owner. It acquires the owner and foreground connection
allowance before DNS, source reading or body preparation. Fresh Runtime
qualification, account/device checks, original request work, and a shared
absolute deadline cover authenticated preparation and queue publication.
Transport retries retain the same prepared operation and submitting route;
other failures do not fall back to another profile.

An uncertain COMMIT retains the exact immutable submission and fences another
submission. The owner stores no application password. Its settlement method
uses independent connection capacity and fresh resolution to inspect the
historical receipt. Confirmed outcomes release the owner; absence and errors
retain it. One outgoing body is bounded by preparation's 32 MiB cap, alongside
the temporary verified source during preparation. The HTTP mutation owner
holds this submission owner separately from drafts and Email mutations.

The local PostgreSQL fixture uses the restricted Runtime login to check
publication, the submitting route, prepublication budget/device/deadline
refusals, rollback, delayed COMMIT fencing, and exact receipt settlement while
foreground capacity is occupied. JMAP dispatch, implicit success filing,
durable startup ownership and capability activation remain separate work.

## 90. Prepared PostgreSQL Identity/get

Cold JMAP-profile preparation retains the existing bounded visible Identity
catalog beside the immutable selected binding. Other role profiles do not
allocate this catalog. The same resolved PostgreSQL identity/signature text
drives the existing canonical visible-state digest and object encoder; no
local-file fallback or second representation is introduced.

Prepared read dispatch serves Identity/get through its freshly authenticated
PostgreSQL snapshot and checks that snapshot before publishing the result.
It does not open a local SQLite view. Account, epoch, selected configuration
and deadline remain bound to that snapshot, while identity state depends on
the visible configuration digest rather than the mail sequence. Catalog
handles alone confer no live authorization.

The read-dispatch fixture explicitly admits Submission to test identity
listing, selected/missing IDs, result-reference lookup, signature bytes and
foreign-account refusal alongside the normal Mail methods. HTTPS capability
admission and the Session remain unchanged: this increment does not advertise
Submission before sending and reconciliation are connected.

## 91. Fixed-worker success filing

The prepared HTTPS Email mutation owner accepts the exact success-filing
operation alongside flags, moves and deletion. The caller supplies a known
successful submission's Email and optional source/destination pair. Filing
uses its own Email transaction and response completion; no filing error
replaces or rolls back the queue outcome.

The original request meter funds bounded membership inspection and history
updates before effects. Fresh Runtime qualification, account/device checks,
the current selected policy and the original deadline apply as for other
Email writes. Uncertain filing retains its exact receipt operation in the
same owner and fences subsequent Email mutations. Independent settlement
capacity can recover it without obtaining another application password.

The local PostgreSQL fixture verifies exhausted work, transaction rollback
without queue or draft loss, delayed-COMMIT fencing, settlement with occupied
foreground capacity, the final membership/draft state, and missing-source
refusal. This adapter does not yet connect the JMAP implicit response or
advertise Submission.

## 92. Prepared submission reconciliation reads

The authenticated PostgreSQL Snapshot exposes historical submission identity
metadata and bounded identity/time query pages for td-mail reconciliation.
Metadata retains the original Email, Thread and sending Identity IDs and
sendAt even if the current Email has been deleted; those IDs confer no body
access. Each row's changed sequence must belong to the pinned account view.

Query windows use inclusive after and exclusive before against immutable
sendAt, either date direction and ascending raw SubmissionId ties. Pages have
at most 1024 returned IDs plus one lookahead, an optional same-snapshot total,
and account/epoch/sequence/interpretation-bound Submission query state. The
original read slot, SQL worker, authenticated snapshot and absolute deadline
remain in force; a failure poisons the view. No JMAP dispatch, capability,
recipient status projection or production PostgreSQL cutover is activated.

## 93. Prepared td-mail submission-get projection

Prepared PostgreSQL read dispatch accepts EmailSubmission/get only with
Submission in the admitted request's using list. Its implemented shape is
td-mail's explicit IDs and explicit id/emailId properties; id is always
returned. Missing and opaque foreign IDs appear in notFound, duplicate IDs
are coalesced, foreign accounts are refused, and bounded result references
retain their normal request semantics. Omitted/null IDs or properties and
other properties are explicitly invalidArguments, not a partial full-object
projection. Enumeration and recipient/status properties remain unconnected.

The projection uses the same fresh authenticated Snapshot and original
response/work allowance, returns its Submission data state and historical
Email IDs, and checks the view before publishing. HTTPS capability admission
and Session advertising remain unchanged; this is not sending activation.

## 94. Prepared td-mail submission query

Prepared PostgreSQL read dispatch accepts EmailSubmission/query with
Submission in using. Its bounded filter is one identityIds entry and
mandatory after/before UTC whole-second dates, inclusive/exclusive against
sendAt. Opaque foreign identities match nothing. Unsupported filter forms,
sorts, malformed dates and negative positions refuse explicitly. Sorting
supports sendAt in either direction; ascending raw SubmissionId breaks ties.

Pages respect the configured query bound and the storage maximum of 1024,
return the pinned Submission query state and optional same-snapshot total,
and support result references into subsequent explicit Submission/get calls.
The original snapshot, SQL deadline and response meter cover every page;
publication checks the snapshot again. This preparation does not advertise
Submission or activate sending, generic query filters or queryChanges.

## 95. Prepared Email reconciliation windows

PostgreSQL Email/query supports td-mail's fallback filing search: inMailbox,
after and before in one condition or a flat AND of conditions. Date bounds
are strict UTC whole-second values, inclusive after and exclusive before
against receivedAt. Repeated date bounds intersect; foreign mailbox IDs and
empty time windows match nothing. Distinct local mailbox conditions refuse rather
than guessing their multi-membership intersection. Unknown filters, nested
operators and non-AND operators refuse; malformed shapes and dates refuse.
The active SQLite dispatcher retains its mailbox-only filter support.

The same snapshot filters both the optional total and ordered page, preserving
account isolation, receivedAt/EmailId ordering, lookahead and the original
SQL deadline. The date decoder is shared with submission queries, including
its numeric-position checks. This is an inert PostgreSQL read preparation;
it does not enable Submission or the production backend cutover.

## 96. Prepared singleton submission mutation

Prepared dispatch accepts one EmailSubmission/set creation with an optional
matching onSuccessUpdateEmail patch. It resolves literal Email IDs or
request-local creation IDs without granting Email access; the fixed worker
freshly authenticates and authorizes the account, source and identity and
preserves the submitting profile's route. Mixed draft/send batches, updates,
cancellation and destroy remain unconnected, as do capability advertising
and production PostgreSQL selection.

Response body and implicit-frame space are reserved before effects, under
the original response and mutation meter. Known creation publishes its
Submission ID and state independently of the later filing result. Filing
uses the same meter and execution deadline and gets a separate Email/set
response (or method error) under the original call ID. Filing refusal or
uncertainty never rolls back the submission or changes its known result.
Outer response I/O, deadline or exhausted original work can still prevent a
reply; retained operations and td-mail reconciliation remain necessary.
Indeterminate creation keeps its native fence and returns serverPartialFail
without invoking filing. Initial creation mappings remain fallback-only;
new successful request mappings shadow them.

## 97. Prepared td-mail draft-and-submit batch

Prepared dispatch accepts td-mail's ordered singleton literal Email/set
creation then EmailSubmission/set request. A noncreation first call refuses
before effects. The batch uses one response owner, creation map, work
meter and execution deadline. The second call resolves the first call's
successful draft ID. Draft publication advances the state used by a later
submission refusal; successful submission and filing retain their own
transaction states. Each worker freshly authorizes its operation. These
calls are independent transactions: a failed submission leaves its draft,
and failed filing leaves a successful submission. This does not advertise
Submission or activate the production PostgreSQL backend.

## 98. Prepared HTTP submission dispatch

The inert PostgreSQL HTTP adapter admits the Submission capability and
routes a singleton EmailSubmission/set or td-mail's draft-and-submit batch
to the prepared mutation dispatcher. It releases its fresh authenticated
snapshot before mutations, retains the original response owner and deadline,
and uses the shared pending owners for draft, submission and filing. Read
requests can use Identity/get and submission reconciliation through the same
admission path. Session metadata and the active SQLite HTTP adapter remain
unchanged; production activation still requires atomic backend cutover.

## 99. Prepared plain-text sending Session and real-client qualification

A separate Session constructor describes the prepared PostgreSQL td-mail
sending profile: Submission appears in global and account capabilities and
primaryAccounts, delayed sending is zero and submission extensions are
empty. The active mail-access constructor stays unchanged. Capability bytes
participate in the Session state digest. The constructor itself grants no
authority and activates no production listener.

The real-client sending fixture exercises discovery, Identity/get, Sent
folder selection/creation, plain-text draft creation, durable submission,
independent success filing and message retrieval through the native td-mail
and isolated fetch processes. A deliberately failed local draft retirement
leaves td-mail's send record in place; its next attempt reconciles through
submission query/get and retires the draft without adding a submission.
Queue rows retain the desktop submitting profile's smarthost route. No SMTP
relay, attachments, cloud-provider failover or production cutover is claimed.


## 100. Caller-funded upload preparation

`postgres_uploads::prepare_with_meter` retains completed ingress using the
caller's original work meter and clock. It prepays retained body bytes,
charges bounded hashing and copying separately, and charges request-digest
input and a final deadline check. Exhausted work remains stopped across
retries, including a later empty body. The supplied preparation deadline can
only shorten the meter's original deadline; no database work or new allowance
is introduced. The cold convenience wrapper constructs its bounded meter and
uses the same implementation. Memory/concurrency admission, HTTPS upload
receipt ownership and production activation remain separate work.


## 101. Fixed-worker upload ownership

The inert `postgres_http_uploads` owner publishes completed, caller-admitted
upload bytes under the original meter and deadline. One process-lifetime
pending owner excludes concurrent preparation and retains at most one bounded
immutable body on uncertain COMMIT. Publication rechecks current device and
profile policy. Known outcomes release the owner; uncertainty keeps the exact
operation and refuses another upload. No password survives the call.

Receipt settlement uses fresh runtime-qualified connections and separate
connection capacity. A confirmed receipt releases the fence and returns the
original operation/outcome. Absence, timeout or connection failure preserves
it; settlement does not replay or authorize publication. The runtime caller
still owns ingress memory, response retention and startup/recovery scheduling.
No HTTPS upload endpoint or production backend is activated by this owner.

The runtime privilege roster permits INSERT on upload leases for publication,
beside their existing SELECT permission. UPDATE and DELETE remain refused,
including column-only UPDATE grants. Provisioning grants must match this
roster before the runtime connection qualifies; no owner credential fallback
is used for an upload.


## 102. Prepared HTTPS upload receipt

The prepared PostgreSQL HTTPS adapter receives length-delimited or chunked
upload bytes on the fixed body worker. One ingress permit excludes another
body or preparation on that pending owner; an uncertain upload refuses before
100 Continue or body allocation. The original request meter prepays bounded
ingress and response storage, charges wire/decoded input, and carries through
preparation/publication. Body idle, transfer and original deadlines never
extend each other. No SQL transaction spans peer input. Publication rechecks
current device/profile policy before creating the lease and receipt.

Known outcomes return the bounded upload JSON and admit authenticated raw
blob download. An uncertain COMMIT returns an internal uncertain response
whose bytes cannot be serialized. The HTTPS owner retires only that exchange
without final success or temporary-failure output; the retained operation
continues to fence uploads until settlement. Reads and the service remain
available. SQLite confirmed responses retain their existing behavior.
This remains prepared integration: production cutover, attachment composition,
and resource/recovery activation are separate requirements.


## 103. Caller-funded receiving preparation

`postgres_receiving::prepare_with_meter` prepares completed DATA under the
connection owner's original meter and runtime clock. Header filtering,
identifier copies, body hashing and request-digest construction share that
allowance. A bounded phase clock cannot extend its deadline. The cold owner
entry point wraps the same path with profile work limits and a bounded body
plus 1 MiB metadata output allowance; it does not admit network peers.

Before metadata validation and copying, preparation prepays four 64 KiB
metadata passes and two 64 KiB retained metadata extents plus trace scratch.
Line discovery prepays at most 1001 bytes and a record per turn, retaining
no unused credit; a missing newline cannot cause a whole-body scan. Existing
header and output accounting remains separate. Exhaustion stays sticky across
retries. The caller still owns body and scratch memory admission, gateway and
recipient authority, publication and durable uncertain-operation custody.
The real PostgreSQL receiving fixture exercises shared-meter exhaustion,
expiry and successful publication with unchanged provenance and threading.


## 104. Fixed-worker receiving publication ownership

`postgres_smtp_receiving` publishes completed, caller-admitted DATA with one
retained operation and the original work meter. It validates the receiving
profile and configured header/recipient bounds, reserves a foreground
connection before resolution, and uses runtime-qualified primary connections.
A ten-second phase and original deadline cover preparation and publication;
publication is prepaid using the final filtered body's extent and recipient
count. Known outcomes
survive later time/accounting refusal. Connection failures before publication
may try the next bounded resolved address; unknown COMMIT is never replayed.

The fixed owner retains uncertain Delivery bytes, IDs and provenance and
refuses another preparation. Settlement uses separate connection capacity and
fresh resolution, excludes the old publisher through the account lock and
clears only a confirmed receipt. An absent receipt or failed observation keeps
the operation fenced. Passive SMTP receipt facts confer no peer, gateway or
recipient authority: the trusted connection owner still owes those checks.

This remains prepared integration. SMTP ingress/transport wiring, durable
restart custody, maintenance and production backend cutover are not activated.
The PostgreSQL runtime-role fixture covers known success, rollback, delayed
COMMIT, refusal while pending, historical settlement, absent-receipt retention
and stale-policy refusal.


## 105. SMTP worker storage boundary

The fixed SMTP receiving worker accesses storage through `smtp_storage`.
The boundary admits a job from the trusted connection's session, account,
configuration generation and observed peer/TLS facts. Jobs own DATA writes,
preparation and publication; the worker retains the existing routing, TLS,
deadline, retry and shutdown state machine. A known durable result remains
separate from cleanup/accounting stop flags; uncertain publication still
retires the active service under the existing production policy.

The native adapter preserves current coordinator readiness, spool capacity,
authorization guards, contention mappings and completion behavior. Production
continues to construct only that adapter. This is a storage interface for the
PostgreSQL cutover, not a new SMTP feature or an activated PostgreSQL listener.
Existing real SMTP/TLS, storage refusal and shutdown fixtures exercise the
same production adapter through the new boundary.


## 106. Fresh receiving readiness

The prepared SMTP owner checks readiness through a bounded, runtime-qualified
primary connection before reporting that its selected receiving profile and
Inbox are available. The check validates schema epoch and selection, then
holds the account share lock while checking sequence/floor coherence and one
Inbox with a valid ID and change sequence. Publication uses the same Inbox
validator under its account write lock. Readiness writes no mail or receipt
and does not replace fresh publication authorization or reserve quota.

The owner refuses readiness while an uncertain delivery is retained, reserves
foreground capacity before fresh resolution and retains one ten-second phase
under the caller's deadline. Errors remain typed; no local fallback or listener
activation is introduced. The runtime-role PostgreSQL fixture covers success,
missing Inbox, incoherent Inbox history, stale selection and pending delivery,
including unchanged account state and absent publication effects.

## 107. Prepared SMTP DATA storage

`postgres_smtp_storage` implements the fixed worker's storage boundary for
direct SMTP. Its constructor pins the receiving owner, one shared runtime
clock and the local policy generation. Begin checks the routed account,
generation and configured message/header/recipient bounds, takes one ingress
permit and observes fresh receiving readiness before admitting DATA. The
permit and resident buffer survive worker turns without a database transaction
or mutex guard spanning peer input. Dropping the job releases that capacity.

The original absolute deadline and profile work allowance cover bounded DATA
copies, receipt metadata, preparation and publication. Admission prepays one
incoming buffer of at most 32 MiB; preparation may retain one additional
filtered body under the same meter. Each decoded line is at most 1000 bytes.
A failed write remains failed, and publication can be attempted only once.
The trusted connection owner still supplies routing and observed peer/TLS
facts; no gateway authority is inferred from message content.

Known publication yields the account, epoch and sequence required for SMTP's
final success reply. Unknown COMMIT retains the original operation in the
publication owner, stops this backend and produces no final SMTP receipt.
Historical settlement does not automatically restart this backend. Production
still constructs only native storage; aggregate runtime admission, durable
restart custody and atomic backend cutover remain separate work. The runtime
PostgreSQL fixture drives the SMTP session through admission, DATA and commit,
including discarded ingress, sticky refusal, generation/clock mismatch,
single ingress capacity, success, delayed COMMIT and historical settlement.

## 108. Real SMTP worker qualification

The explicit PostgreSQL fixture runs the production SMTP receiving worker and
prepared PostgreSQL storage adapter together on an ephemeral TCP listener.
Its database-loaded profile supplies routing, resource limits and retained
TLS policy. The runtime database role supplies readiness and publication.
The fixture checks the SMTP greeting, envelope admission, DATA and final
success reply against durable receiving receipts and Email rows. A known
database rollback produces 451, preserves the ready worker and permits a
later successful transaction on that connection. Shutdown discards unfinished
DATA, joins the workers and leaves no additional message or receipt.

This qualifies the plaintext direct-SMTP transport path in the prepared
runtime. It does not activate a production PostgreSQL listener, qualify a new
STARTTLS path or replace the existing native TLS and shutdown fixtures.

## 109. PostgreSQL receiving over STARTTLS

The explicit STARTTLS worker fixture upgrades the same real SMTP listener
using td-crypto's verified localhost client and the fixture's private trust
root. It checks that STARTTLS refuses an active envelope, then upgrades
after RSET and requires a fresh EHLO before MAIL. Both committed
messages retain the negotiated TLS version and post-upgrade EHLO/reverse
path in PostgreSQL; the plaintext variant checks Plain receipt provenance.
The same rollback/recovery and open-DATA shutdown cases run over both
transports. Abrupt shutdown may end encrypted input without close_notify;
the fixture distinguishes that closure from a timeout and accepts no final
message receipt for unfinished DATA.

The test uses bounded TLS wire buffers and per-I/O waits, verifies the server
name and does not change production trust, client authentication or gateway
policy. This is prepared PostgreSQL integration evidence; production backend
selection and deployment qualification remain unchanged.


## 110. Durable outgoing DATA phases

Schema 19 retains one phase receipt per account, attempt and target phase.
Fresh initialization permits at most 1024 inbound protocol frames for the
complete migration ledger; byte and individual-frame ceilings stay unchanged.
The prepared phase publisher checks current configuration, account and route
ownership in their existing lock order, then the exact active attempt's boot,
claim, destination and fencing generation. Body publication accepts one
ordered subset of at most 100 original attempt ordinals with bounded positive
RCPT replies. It clears older DATA replies for that subset. AcceptancePossible
requires exactly the entire stored Body subset and preserves those RCPT
replies. Other prepared recipients remain untouched. Neither transition
retires the attempt, invents a remote result or clears prior uncertainty.

Recipient, submission and account sequences, change history and the digest-
bound receipt commit atomically. A second request for the same phase with
different input refuses. Historical receipt inspection precedes fresh policy
checks and excludes an in-progress publisher through the account lock;
unknown COMMIT requires closing transport, not transmitting on guessed state.
Receipt storage has a 2048-per-account ceiling. Runtime retention/reclamation
and final-outcome capacity admission remain prerequisites for activation.

These are durable facts, not continuing permission to transmit. The runtime
must retain current ownership and its monotonic transport fence, and reserve
final-outcome capacity before allowing a terminator. This increment enables no
SMTP relay. PostgreSQL fixtures cover invalid subsets/replies, stale ownership,
known rollback, phase ordering, exact Body membership, retained replies,
100 distinct maximum-length replies, conflicting replay, delayed COMMIT
and historical settlement after a policy
change. Schema upgrade tests include the version-18 boundary.


## 111. Durable positive SMTP outcomes

Schema 20 adds acceptance receipts, bounded to 1024 per account. The prepared
attempt publisher records a genuine positive final DATA reply for exactly the
AcceptancePossible subset under the same current policy, boot, claim and route
fence. It preserves positive RCPT replies, attempt history and any earlier
uncertainty latch. A positive result records smart-host handoff, not final
mailbox delivery. Passing the message expiry time cannot discard a received
positive reply; current owner and clock checks still apply.

Accepted rows, reply text, submission/account change history and one digest-
bound receipt commit atomically. The attempt retires only when no recipient
remains InFlight under it. If the complete ordinal set has no future delivery
obligation, completion time is published once; terminal failures retain their
failure-notice obligation. Historical replay neither advances time nor changes
replies. Unknown COMMIT settles through the original receipt without inventing
a second remote result. No connection or SMTP relay is activated here.

The PostgreSQL fixture covers ordering and exact-subset refusal, maximum-length
final replies, rollback, delayed COMMIT, duplicate-risk preservation, attempt
retirement, exclusion of Accepted recipients from subsequent dispatch, and
final-batch completion after expiry. Negative outcomes, final-outcome capacity
admission and runtime recovery/scheduling remain separate prerequisites.


## 112. Durable final DATA refusals

Schema 21 adds at most 1024 digest-bound final-refusal receipts per account.
The prepared publisher accepts one actual 4xx/5xx final DATA reply for the
exact AcceptancePossible subset under its current boot, claim and route
fence. It preserves positive RCPT history and earlier uncertainty. A definite
negative final reply does not introduce uncertainty merely because the
current attempt had reached AcceptancePossible.

Temporary refusals schedule each recipient using the shared bounded jitter
policy; earlier uncertainty yields OutcomeUnknown, otherwise RetryWait.
A retry that cannot precede expiry refuses with ExpiryHold for the separate
clock-confirmed expiry path. Permanent refusals terminate retries, retaining
OutcomeUnknown for prior uncertainty and Failed otherwise; a genuine refusal
can be retained after expiry. Result rows, submission completion and pending
failure notice, attempt retirement, history and receipt commit together.
Historical replay preserves the original random retry times.

The runtime-role fixture exercises ordering, exact subsets, maximum replies,
near-expiry hold, rollback, delayed COMMIT settlement, conflicting replay,
retry bounds, uncertainty preservation and final completion after expiry.
This prepares final DATA results only. RCPT/MAIL/DATA-command refusals,
transport failures, retry dispatch, capacity reservation and production
activation remain separate work.


## 113. Durable negative RCPT subsets

Schema 22 adds at most 1024 RCPT-result receipts per account. The prepared
publisher retains one ordered set of negative RCPT replies per attempt, at
most 100 distinct 4096-byte replies. Each selected recipient must still be
Prepared under the exact current attempt fence. Its new RCPT reply replaces
prior RCPT history and clears prior DATA history. Temporary/permanent states,
retry jitter, uncertainty preservation, expiry hold and completion follow the
final-refusal rules above, independently for each reply.

Unselected Prepared recipients remain in the active attempt and can enter
Body as the accepted RCPT subset. An all-refused batch retires the attempt;
a partially refused batch retires only after the remaining recipients finish.
One receipt binds the complete refused subset, so the worker must collect its
negative RCPT replies before publication instead of publishing one per call.
Historical replay preserves retry times and never repeats account changes.

The runtime-role fixture checks mixed temporary/permanent replies with and
without prior uncertainty, retained-reply replacement, a partially accepted
batch proceeding through DATA, 100 distinct maximum-length refusals,
rollback, delayed COMMIT settlement, conflicting replay, attempt retirement
and final completion. MAIL and DATA-command refusals, transport failures,
retry dispatch and final-outcome capacity admission remain separate work.

## 114. Durable MAIL and DATA-command refusals

Schema 23 adds at most 1024 command-refusal receipts per account. A negative
MAIL reply covers the complete original Prepared batch, clears prior RCPT
history and stores the real refusal as DATA history. It cannot follow a
partially settled RCPT batch. A negative DATA-command reply instead binds the
accepted RCPT subset and its actual positive replies; it replaces both prior
reply fields without claiming that Body or AcceptancePossible occurred.
Other Prepared recipients remain owned until separately settled. Body and
DATA-command refusal are mutually exclusive for the entire attempt, even
after the Body recipients finish; outstanding RCPT refusals may still settle.

Both operations reuse the current policy/route/attempt fence, temporary retry
jitter and expiry hold, prior uncertainty latch, terminal completion and
failure-notice handling. Exact retained input binds the receipt, including
positive RCPT text. Neither command refusal introduces uncertainty or permits
another SMTP transmission. Unknown COMMIT settles historically.

The PostgreSQL fixture exercises temporary and permanent outcomes for both
commands, maximum reply sizes, stale owners, MAIL subset refusal, old-reply
replacement, partial RCPT settlement, rollback, expiry holds, permanent
results after expiry, delayed COMMIT, conflicting replay and completion.
Transport failures, retry dispatch, capacity reservation and production relay
activation remain separate work.


## 115. Current-owner transport interruption

The attempt recovery publisher also prepares a domain-separated current-owner
interruption. Unlike abandoned-attempt recovery, it requires the exact current
generation, boot and owner operation recorded in the active attempt. The caller
must close or fence transport before publication and cannot resume it from a
receipt. Unknown COMMIT settles the retained operation historically.

All still-InFlight recipients end together. Prepared and Body become RetryWait
unless earlier uncertainty is latched; AcceptancePossible latches uncertainty
and becomes retryable OutcomeUnknown. Genuine replies remain unchanged, and
already settled RCPT recipients are excluded. Bounded jitter, near-expiry hold,
account history and attempt retirement reuse the existing recovery transaction
and its bounded receipts. This operation does not pause a route, certify TLS
or credentials, reserve final-outcome capacity, or activate production relay.

The PostgreSQL fixture covers each durable phase, mixed remaining Prepared
recipients, earlier uncertainty, stale owners, refusal of abandoned recovery
for the current owner, retained replies, exact replay and retry timestamps,
rollback, expiry hold, delayed COMMIT and exclusion of settled RCPT results.


## 116. Conservative runtime queue clock guard

The prepared `queue_clock::Guard` pairs database UTC with samples from one
process-local monotonic domain. Startup permits fresh dispatch but holds
attempted recipients for five monotonic minutes. Expiry remains held until
an explicitly authorized queue clock-confirm operation observes at least five
stable minutes. The caller must check database due times, ownership and route
health separately; a status snapshot is neither a permit nor historical
transport authority.

A cumulative UTC/monotonic discrepancy greater than five minutes in either
direction pauses all new dispatch and expiry, resets the stable interval and
revokes confirmation. Individual sample-to-sample discontinuities are checked separately so that a
jump cannot hide by canceling prior drift. Smaller observations do not move
the comparison anchor.
A database primary change forces the same hold even when its clock matches.
Elapsed time alone never clears these holds. Invalid UTC or monotonic regression
poisons that guard until runtime reconstruction, which repeats startup holds.

This bounded, allocation-free guard supplies scheduling and health decisions
only. Wiring database sample acquisition, failover detection, authenticated
clock-confirm, bounded expiry and the production scheduler remains activation
work. Existing prepared storage publishers do not imply those runtime checks.


## 117. Prepared due-retry publication

The attempt publisher has a separate domain-separated retry preparation.
It selects at most 100 due RetryWait or retryable OutcomeUnknown recipients
from one supplied submission, in ordinal order. It excludes fresh Queued
work, Accepted, Failed, terminal OutcomeUnknown and future retry timestamps.
The first-attempt preparation continues to select only fresh Queued work.

The transaction validates final phase, attempt identity/count, uncertainty
and retry reason, a due timestamp at least five minutes after the previous
attempt, and checked count increment. InFlight/Prepared publication preserves
uncertainty and genuine prior replies, replaces attempt identity and timestamp,
and clears only next-attempt time, failure reason and local diagnostic.
Immutable transmitted content and envelope remain unchanged. Active-route
exclusion, current-owner fencing, history and receipt replay use the same
atomic publisher as first attempts; receipt replay never increments twice.

This is a prepared storage operation, not a retry scheduler. Runtime callers
supply the persistent queue clock guard; publication enforces it along with
shared route backoff and repair generation. Current transport authority and
bounded due scanning remain separate runtime requirements.
No runtime route or retry loop is activated here. The PostgreSQL fixture seeds
elapsed due times and covers both states, bounded batches, terminal/future
exclusion, overflow, minimum interval, expiry, rollback, unknown COMMIT,
historical replay and the separate fresh-recipient path.


## 118. Clock-confirmed expiry admission

The queue clock guard admits one expiry batch per monotonic second, only
while explicitly confirmed. Each batch remains limited to 100 recipients by
the storage publisher. A failed or abandoned attempt consumes its admission;
there is no refund or catch-up burst. Runtime expiry workers must share one
guard for their admitted queue and preserve the database-wide owner fences.

The retained token fixes a UTC cutoff and a nonrenewable ten-second maximum
publication deadline. The publisher must check it again before commit with
fresh paired database UTC and local monotonic time. Clock holds, confirmation
replacement, backward UTC and elapsed deadlines permanently retire the token.
A process-unique checked confirmation identity prevents transfer to another
guard; these ephemeral tokens and identities are never persisted. Every
observed UTC regression advances a checked expiry generation, including
subthreshold steps seen by another user of the shared guard. Recovery of UTC
does not restore an older token; it cannot erase the intervening observation. The token
is not a live transport permission. Database expiry mutation and runtime
scheduler/authenticated confirmation wiring remain separate work.


## 119. Prepared queue expiry storage

Schema version 24 adds retained expiry-operation receipts and one database-wide
expiry progress row. A receipt binds an account/operation to its request digest,
submission, one-to-100 recipient count, fixed nonnegative UTC cutoff, publication
time and old/new account sequence. It deliberately has no foreign key to the
submission, so later retention cannot erase historical settlement evidence.
The publisher must cap retained receipts and verify exact sequence adjacency.

The progress row starts with no publication time. Its singleton key and nullable
nonnegative timestamp prepare serialization of expiry batches across accounts
and instances. A future publisher must lock this row after configuration,
account and route-owner locks, require at least one database UTC second since
the previous committed batch, and update it atomically with recipients, history
and receipt. Missing progress is an integrity failure, never permission to reset
the rate limit. Local monotonic admission remains separately mandatory; the
persisted timestamp is neither clock confirmation nor transport authority.

Both tables are private to the schema owner at initialization and require the
mail runtime's ordinary DML grants. Upgrade preserves the epoch and earlier
material, rolls back all new DDL on obstruction, and includes the prior version
23 in the migration fixture. This schema preparation expires no recipients and
starts no scheduler; the fenced publisher and runtime wiring remain work.


## 120. Fenced clock-confirmed expiry publisher

The prepared expiry publisher binds a random operation to one submission,
route-owner claim and generation. Historical receipt inspection precedes
fresh admission: a completed operation settles after clock holds or owner
replacement without expiring more recipients. Changed digests conflict.
Uncertain COMMIT results require settlement of that same retained operation.

New publication requires current policy and unexpired ownership under the
configuration/account/route locks. Database UTC is paired with the supplied
runtime clock's monotonic domain and admitted by its shared confirmed guard.
The database-wide progress row serializes publishers and requires a UTC
second since its previous publication sample. That persisted bound supplements
the guard's monotonic interval; neither enables catch-up or bypasses explicit
clock confirmation. The retained cutoff never advances during the operation.

For an overdue submission, one transaction selects at most 100 pending rows
in ordinal order. Fresh Queued becomes Failed/Expired with no attempt phase.
Attempted certain rows become Failed/Expired/Final; an existing uncertainty
latch or AcceptancePossible instead becomes terminal OutcomeUnknown/Expired.
No retry time remains. Actual replies, attempt history, transmitted bytes and
envelope survive. Terminal rows are excluded. Exact ordinal coverage is
checked before updating completion; the final batch sets completedAt and
Pending failure notice together. Retired attempts, account sequence, CHANGE,
receipt and global progress commit with those recipient changes. Each account
retains at most 1024 expiry receipts until a separately defined reclamation.

The publisher rechecks ownership and the nonrenewable clock token before
COMMIT; late clock failure rolls back staged changes. A COMMIT handed to the
server is inherently indeterminate on connection/deadline failure and settles
through the receipt, never a replacement result. Runtime callers must close
and fence the route's transport first, including an interrupted current-owner
attempt, share the process guard and detect primary changes. This API starts
no transport, scan, scheduler or administrator clock-confirm endpoint.


## 121. Prepared shared route health storage

Schema version 25 prepares route health keyed by account, route name and
immutable destination fingerprint, plus independently retained operation
receipts. Separate destination keys keep a changed endpoint from inheriting
or erasing the prior endpoint's backoff. Each health revision is nonzero;
receipt revisions increase and the future publisher must check exact adjacency.
Receipts capture the resulting state and one of failure, repair or success,
so later state changes cannot rewrite an earlier operation's settlement.

The stored consecutive failure count is bounded by u32. A zero count is the
healthy state, with no failure/retry timestamp or repair latch. A nonzero count
retains failure UTC and a next-attempt time between five minutes and one hour
later. Reasons are None, Network, Tls, Auth or Protocol. A repair latch, when
present, names a nonzero configuration-selection generation and is permitted
only for TLS/authentication failures. Explicit repair may clear that latch
while preserving the failure count and minimum retry time; successful transport
may reset the count without resetting the increasing revision. No stored
field or receipt itself authorizes a send or proves a successful transport.

The future publisher must compare current policy and ownership, use bounded
shared retry jitter with checked count/revision increments, retain repair
holds across ownership changes, and cap health rows and receipts per account.
Clock guards and due times remain separate dispatch requirements. Both tables
use the runtime's ordinary mail DML grant set and private owner initialization.
This schema preparation changes no dispatch behavior or runtime admission.


## 122. Account-bootstrap mutation budget

Account creation uses the existing bounded mutation connection allowance:
1088 input frames, one MiB aggregate input and 128 KiB output, with unchanged
per-frame, authentication and deadline limits. Its schema-ledger inspection
and bootstrap writes share that allowance. The ordinary 128-frame probe
allowance no longer fits this operation at schema version 25; exhausting it
while reading COMMIT must remain indeterminate and settle the retained creation.
This changes no transaction or replay semantics. Read-only account inspection
retains the ordinary probe allowance.


## 123. Shared route failure publication and dispatch checks

The route-health publisher records Network, TLS, authentication or protocol
failure for a retained attempt of the exact current owner, route and destination.
The attempt ID is the operation key: preparing it again cannot double-count a
failure or replace its classification. The request digest binds the claim,
fence, route, attempt and failure. Historical settlement precedes current-owner
checks and remains available after takeover or subsequent health changes.

Under the current configuration/account/route lock, publication checks attempt
identity and time, requires the attempt to remain active, refuses reports
from before the latest route failure, and
increments checked failure count and revision. Shared bounded retry jitter
sets the next time. TLS/authentication failures also latch the selected
configuration generation; ordinary later failure reports cannot clear that
hold. Health and receipt publish atomically, with 1024-row per-account caps,
and recheck current ownership before COMMIT. Unknown COMMIT must settle this
same attempt operation. Route health changes no recipient or account sequence;
recipient settlement remains a separate fenced operation after transport closes.
Publish a fresh route failure before that settlement retires the attempt. This
prevents delayed reports for retired attempts from undoing a later success;
the single active attempt per route keeps that ordering across new dispatch.

Fresh and retry attempt publication now inspect durable route health under
those same locks. Backward database time, an outstanding repair latch or a
future shared retry time refuses new dispatch. Receipt replay remains historical
and does not acquire new transport authority. The local queue-clock guard and
recipient due times remain separate requirements. No scheduler or transport is
activated; explicit configuration repair and successful-route reset are the
next publication operations, and no timer silently clears a repair latch.


## 124. Explicit shared route repair

A prepared route-repair operation binds a random retained operation ID to its
current owner, route, destination and selected configuration generation. It
requires a TLS/authentication repair latch from a strictly older selection.
Reselecting validated configuration and resolving a new profile still does
not clear the latch: the explicit repair publication is separately required.
The same selection cannot clear its own failure, and a route without a latch
refuses a new repair. Historical replay can settle after owner release.

Under the configuration/account/route locks, repair increments the checked
health revision and clears only repair_generation. Failure count, reason,
failure timestamp and next-attempt timestamp remain byte-for-byte unchanged.
Dispatch therefore remains blocked until the existing shared minimum interval
and the separate recipient/clock guards permit it. Receipt action 2 captures
that repaired state atomically, with the shared 1024-receipt account limit.
Backward UTC, stale ownership, overflow and write failure refuse publication;
unknown COMMIT settles the retained operation without applying another repair.

This is an explicit prepared storage operation, not an authenticated operator
endpoint or a configuration-load side effect. Runtime integration must bind
operator authorization to this operation. Successful-route counter reset is
a separate operation; repair does not claim that transport has succeeded.


## 125. Shared route success reset

The success publisher requires a retired attempt of the exact current owner,
route and destination, plus its durable positive final-DATA acceptance receipt.
A Prepared/Body/AcceptancePossible attempt or merely asserted transport
success cannot reset health. The attempt ID is the shared operation key;
failure and success classifications cannot both publish for one attempt.
Fresh failure publication requires an active attempt, so a delayed unreported
failure from an older retired attempt cannot undo this reset.
Historical settlement remains available after owner release.

Under current policy/account/route locks, success refuses a repair latch or
an attempt prepared before the latest route failure. It increments the checked
revision and resets the consecutive count, reason and failure/retry times to
the healthy state. Action-3 settlement captures that state in the same commit,
with the shared health/receipt caps and a final owner/time check. Recipient
results and account sequence are unchanged. Unknown COMMIT settles the original
operation. A successful SMTP handoff does not prove final mailbox delivery;
this operation starts no scheduler or transport and cannot authorize a send.


## 126. Prepared whole-submission cancellation settlement

Schema version 26 adds independently retained cancellation receipts keyed by
account and operation. Each binds a request digest and submission identity,
records the whole recipient count (1 through 1000), the changed count, original
and resulting account sequences, and the resulting completedAt observation.
A no-change result preserves the sequence and records zero changed recipients;
a changing result advances it. The publisher must enforce exact checked
adjacency, full ordinal coverage and the all-or-refuse cancellation rules.
The completion timestamp is signed like the submission's existing timestamp:
canceling a completed failure must preserve its exact historical observation.

The receipt references its account but deliberately not the live submission,
so eventual authorized submission deletion cannot erase settlement history.
Ordinary runtime mail DML grants include this table; owner initialization and
migration remain private. This increment provides no cancellation API or
transport fence. The future publisher must lock current policy/account/route,
reject any Accepted, uncertain or AcceptancePossible recipient, reserve the
whole mutation before fencing, preserve replies and existing notice/completion
state, and atomically publish canceled recipients, retired attempts, CHANGE
and settlement. Already wholly canceled submissions are idempotent successes.

## 127. Whole-submission cancellation publication

A prepared cancellation binds a random operation ID to its current route
owner, generation and submission. Historical settlement precedes live policy
and owner checks, survives submission deletion, and never invokes transport
closure. New publication holds policy/account/route locks through reservation,
transport closure and commit. The supplied closure must close/fence the owned
transport locally within the original deadline, with no database work or SMTP
round trips. This
internal route-owner entry point does not establish client or operator
authorization.

Before invoking that closure, publication validates the complete ordinal group
with the shared row codec and queue-state validator. Accepted, uncertain or
AcceptancePossible recipients refuse. It bounds aggregate text before fetching
rows, reserves and encodes the prospective whole-group rows, and checks the
transaction byte and operation ceilings. The conservative allowance is 128
bytes per recipient beyond its text plus 4096 bytes for submission, change,
account, attempt retirement and settlement. A thousand fresh recipients fit;
large retained replies may refuse without closing transport. This is bounded
logical transaction capacity and staging memory, not a reservation of physical
PostgreSQL disk or WAL space. The connection has two MiB aggregate input and
2048 frames, unchanged per-frame/authentication limits and 128 KiB output.

Publication changes the whole group to Canceled, retires active attempts,
advances the account sequence once and records CHANGE and settlement in the
same transaction. Actual replies, attempt history, diagnostic text, existing
completion time and failure-notice state are preserved. Cancellation creates
no notice. A wholly canceled group retains a no-change receipt without closing
transport or advancing sequence. Ownership and deadline are checked again
before COMMIT; unknown COMMIT resolves the same operation. Stale attempt
publication cannot cross into AcceptancePossible after retirement. No HTTP
endpoint, scheduler or transport is activated by this increment.

## 128. Conditional whole-submission cancellation

Cancellation preparation can bind the caller's expected store epoch and
account sequence. An epoch mismatch refuses preparation; a fresh publication
checks the sequence under the account lock before group reservation, transport
closure or writes. This check also applies to an already-canceled no-op.
The optional sequence is included in the retained request digest, with absence
distinct from zero. Unconditional operations retain their existing digest.

Historical receipt settlement still precedes live-state checks: a committed
conditional cancellation replays after its own sequence advance or later
account changes. Its receipt must record the original expected sequence.
This is the storage precondition needed for ifInState, not client credential
authorization or an activated EmailSubmission/set update endpoint.


## 129. Device-authorized cancellation publication

Client cancellation preparation binds a selected JMAP profile and device ID
alongside the relay owner's claim, submission and optional account state.
Both profiles must belong to the same account, epoch and selected configuration.
A relay-only profile cannot supply client authority. The request digest binds
that client identity; no password or authentication result lives in the
prepared operation. Internal operations retain their prior digest format.

Fresh client publication requires that device's app password and rechecks its
account-bound verifier and revocation under the account lock, before group
reservation, local transport closure or writes. Device revocation takes the
same lock, so it cannot race a previously cached authentication into a write.
Missing, malformed, incorrect and other-device credentials refuse identically.
The internal publication entry point cannot publish a fresh device-bound
request without credentials; the authenticated entry point refuses preparations
that lack client identity. Existing policy, ownership and state fences remain.

Known receipt replay still settles without live device authority, including
after revocation: it performs no new cancellation. These are trusted worker
APIs whose retained prepared handles are not client-supplied operation IDs;
an HTTP adapter must authenticate the request before exposing settlement.
The supplied credential is decoded into the existing zero-on-drop secret
owner for this call only. No HTTP dispatch, credential format, administration
policy, scheduler or transport is activated or changed.

## 130. Bounded client cancellation owner

The fixed HTTPS worker's prepared cancellation adapter owns at most one active
or uncertain operation. It validates its loaded JMAP profile, account, device
credential shape and optional epoch before admission. The trusted relay owner
supplies the current claim and generation; the publisher checks current device
authority, selected configuration, account state and route ownership under its
existing locks. No request can choose another profile's database or route.

The worker preserves the request/meter's original absolute deadline, with a
ten-second database phase and a five-second resolver phase. Before database
work it charges each attempted address for the receipt/publication connection
ceilings (three MiB plus 192 KiB of protocol I/O and 8192 records, including
runtime admission and whole-group validation). A transport fallback spends
another charge, never refreshes the deadline or substitutes another operation.
The local abort callback must be idempotent: a connection failure after local
closure but before COMMIT can retry the same operation. It still performs no
SQL or SMTP round trips and is invoked only after complete reservation.

Known success or refusal releases the prepared operation; unknown COMMIT
retains it and refuses another cancellation. Independent settlement capacity
only inspects the exact historical receipt. Absence, deadline and errors keep
it retained. Confirmation releases it without credentials or a transport
callback, including after device revocation. Neither password nor callback
survives the foreground call. These are internal owner APIs, not a public
settlement endpoint; production dispatch, relay-worker ownership/restart
custody and atomic backend cutover remain separate work.

## 131. Borrowed cancellation patch decoding

The shared submission request decoder accepts exactly one update property,
`undoStatus: "canceled"`. Empty, non-object, multi-property, partial-recipient,
nested, read-only and other undoStatus updates refuse. Decoding borrows the
already bounded JSON arena and charges the original work meter and deadline;
it allocates no owned request or authority token.

This proves only patch syntax. The selected device, current account state,
whole-group eligibility, transaction reservation and transport fence remain
the authenticated publisher's responsibility. A repeated canceled update is
syntactically valid; only publication can establish its no-change result.
Dispatch and error projection remain separate from this input boundary.

## 132. Prepared JMAP cancellation dispatch

The prepared singleton EmailSubmission/set path can receive a trusted
cancellation adapter alongside its creation and filing adapters. A request
selects one creation or one whole-submission update; mixed operations,
multiple updates and cancellation filing hooks refuse before effects. The
ordinary HTTP adapter supplies no cancellation owner yet, so this adds no
route-worker control or production endpoint activation.

Cancellation validates account and scoped ifInState before object work, then
uses the bounded patch decoder. Unsupported patches become invalidProperties;
well-formed unknown external IDs become notFound. A valid local cancellation
passes the original execution deadline, meter, authenticated device and state
precondition to the trusted adapter. That adapter must own the current relay
claim and local transport fence, recheck current authority/state through the
publisher, and retain unknown COMMIT exactly as the bounded worker requires.

Dispatch reserves its complete response storage before invoking the owner.
Known changed and no-change outcomes carry the receipt's actual old/new
sequences, validate the full changed count and checked sequence adjacency,
and remain retainable after the execution phase expires. CannotUnsend,
transaction capacity and fence refusal project as cannotUnsend; missing and
authorization failures project as notFound and forbidden. Work admission and
connection errors remain method-level failures with no proven mutation.
Unknown COMMIT or an inconsistent claimed success becomes serverPartialFail,
never a fabricated refusal or success. A current state mismatch remains a
method-level stateMismatch. Cancellation creates no creation ID or implicit
Email/set response in this increment.


## 133. Real PostgreSQL cancellation dispatch qualification

The real database scenario now drives validated JMAP requests through response
preflight, the retained-response coordinator, prepared dispatch, the concrete
route adapter and authenticated cancellation publication. It checks current
ifInState, unsupported patches, Accepted-recipient refusal before fencing,
whole-group success and idempotent no-change responses. Wire old/new states
are checked against the actual committed sequences; calls retain escaped IDs,
produce no creation IDs or implicit filing response, and release response
ownership after encoding in small chunks.

A delayed COMMIT forces serverPartialFail while the worker retains the exact
operation. Later independent receipt inspection confirms its original state
and single sequence advance. No uncertain response becomes a guessed refusal
or success. Private response trees use the shared TempDir guard. This qualifies
the prepared internal path, not production HTTP/relay ownership or cutover.


## 134. Bounded route candidate discovery

The prepared route-owner scanner returns at most 64 due submission hints per
page, separately for fresh and retry recipients. It holds the current policy,
account and route-owner locks while checking ownership, route health and the
absence of an active route attempt. Candidates match the immutable route and
destination, have a live uncompleted submission, and have eligible recipient
states due at the database observation time. Each hint carries the minimum
due time among those recipients. No attempt, receipt or sequence is written.

Pages order by due time then submission ID. An opaque continuation binds the
last returned key to the same claim, generation and candidate kind; one extra
row establishes whether more candidates existed at that observation. Pages
are independent bounded reads, not one stable snapshot. A scheduling pass
must restart from the beginning to discover newly due or moved earlier work.
The original absolute connection deadline and existing protocol ceilings
bound each scan; database query work can still grow with the stored queue.

Hints reserve nothing and grant no permission to transmit. Full recipient
validation, queue-clock admission, current policy, route health and ownership
remain required by the attempt owner and publication path. The final owner
check also refuses a scan that outlives its lease. No scheduler, SMTP driver
or production backend is activated by this API. The real PostgreSQL scenario
covers multi-page discovery, eligibility filtering, separate retry discovery,
stale generations, cursor-kind binding, active-attempt refusal and unchanged
account state during read-only scans.


## 135. Expiry discovery during delivery pauses

The route candidate scanner also supplies expiry hints ordered by submission
expiry time and ID under the same page, cursor, policy and ownership bounds.
It selects uncompleted expired submissions matching the immutable route and
destination that still have Queued, InFlight or Deferred recipients, or
OutcomeUnknown recipients with a retained retry time. Terminal unknown,
Accepted and Canceled recipients do not make a submission expiry-eligible.
Recipient retry times and send times do not postpone expiry discovery.

Expiry discovery bypasses delivery backoff, repair pauses and active-attempt
refusal: those delivery conditions must not hide expired responsibility.
This differs from fresh/retry discovery only; the read writes no state and
still refuses a stale owner. An expired InFlight hint grants no authority to
change its recipients or continue its transport. The expiry publisher still
requires closed/fenced transport, confirmed queue-clock admission, current
ownership and policy, exact recipient validation and its global rate limit.
In particular a forward database clock jump may produce a hint while the
clock guard correctly refuses expiry publication.

The real PostgreSQL scenario covers expired hints during route backoff,
repair and active attempts; expiry despite a future retry time; supported
recipient states; and exclusion of terminal unknown, Accepted, Canceled,
completed, live and destination-mismatched submissions. Pagination retains
its independent-observation semantics and must restart for each scheduling
pass. No background scheduler or transport is activated.

## 136. Verified active-attempt transmitted bytes

The prepared attempt API can admit its immutable transmitted body through a
bounded read-only repeatable-read snapshot. The opaque Dispatch binds the
account, route, destination, owner, generation, attempt and request digest.
Fresh primary admission checks current policy, unexpired route ownership and
that exact active attempt before snapshot work. The snapshot binds the live
submission and blob manifest; the shared blob reader verifies contiguous
chunks, total length and SHA-256 before exposing any bytes to the callback.

A second fresh primary admission after verification refuses cancellation,
retirement, ownership loss or policy change observed before that check. Both
admissions release account/route locks before replay. After the second
admission, a fresh check of the original snapshot and deadline precedes the
callback, so snapshot loss during admission cannot enter it. The callback retains one
immutable snapshot under the original absolute deadline, even if later
cancellation retires the attempt. It receives passive metadata and a bounded
BlobReader, not transport authority: current transport ownership and durable
Body/AcceptancePossible phase publications remain prerequisites for SMTP.

The caller reserves connection and body-work capacity. The existing ten-second
connection limit, 32 MiB body limit, bounded chunk batches and fixed protocol
budgets cover this prepared operation. No deadline or snapshot is renewed.
The authenticated device-body path shares manifest decoding without changing
its authorization or upload-lease checks. No delivery scheduler is activated.

The real PostgreSQL scenario proves digest-checked replay, size/deadline
refusal, unpublished/retired attempt refusal and corruption rejection before
callback admission. Cancellation through an independent connection inside the
callback proves that replay holds no account lock, while the admitted snapshot
continues to return the same bytes within its deadline. A controlled source
fault during admission checks that deadline or connection loss suppresses the
callback before it can act.

## 137. Bounded initial attempt envelopes

The prepared attempt API reads an initial envelope only while every retained
batch ordinal remains InFlight in Prepared for that exact active attempt.
Current policy, account and route-owner locks cover the complete bounded read;
route identity, destination, live due submission, account sequences and final
lease validity are checked before returning. At most 100 recipients are copied
with fallible reservations in the receipt's original ordinal order. Strict
shared mailbox syntax validates recipient addresses and any nonempty reverse
path; an empty reverse path remains representable. The uncertainty latch is
returned unchanged as passive per-recipient metadata.

Partial recipient results or later DATA phases refuse reconstruction. The
worker must retain its admitted envelope for the existing transport; these
values never authorize a new or repeated SMTP transaction. Queue-clock and
transport admission, phase publication, interruption recovery and cancellation
remain separate boundaries. No lock survives the call and no account sequence
or attempt state changes. Body and envelope reads share exact active-attempt
receipt validation, including its request digest and owner identity.

Real PostgreSQL cases cover the 100-recipient bound, stable ordinal/address
mapping, read-only state, unpublished and retired attempts, empty and malformed
reverse paths, malformed recipients, inconsistent Prepared fields, partial RCPT
refusal and Body-phase refusal. No scheduler or SMTP transport is activated.

## 138. Mandatory clock admission for attempt publication

Fresh and retry publication now require the caller's persistent process queue
clock guard and monotonic adapter. Under current policy and the route-owner
lock, publication pairs database UTC with the adapter's monotonic tick and
checks the applicable guard status before selecting recipients. The locked
database sample reaches the guard before owner-clock or lease refusal, so a
failed ownership check cannot hide a discontinuity from the persistent hold.
Fresh work is
allowed at startup; retries require five stable monotonic minutes. A clock
discontinuity or primary-change hold continues to refuse dispatch until the
existing explicit confirmation rules are satisfied. The adapter's UTC field
is never used for queue decisions.

After staging all recipient, submission, change and attempt rows, publication
rechecks unexpired ownership, database time and the same guard immediately
before issuing COMMIT. A backward time step within the transaction or elapsed
submission expiry refuses publication. Failure rolls back the whole staged
change. Guard access uses try_lock, so contention or poisoning refuses rather
than waiting outside the connection deadline. No guard lock spans SQL awaits.

Historical receipt settlement precedes clock admission and still grants no
permission to transmit. An unknown COMMIT retains the same attempt identity;
SMTP admission must independently revalidate current runtime ownership and
clock state. There is no unguarded publication entry point. Runtime callers
must share one guard across dispatch and expiry and notify it of primary
changes; this increment activates no scheduler or administrator endpoint.

Real PostgreSQL cases prove startup retry refusal, discontinuity holds,
clock-adapter failure, guard contention, holds despite owner-clock/lease
refusal followed by restored ownership, and rollback when a discontinuity is
observed only after the recipient updates. Historical settlement succeeds
without consulting a failed clock. Existing publication fixtures explicitly
simulate the stable interval while retaining their storage/receipt assertions.

## 139. Bounded relay attempt publication custody

The prepared relay dispatch worker owns one active or uncertain publication at
an admitted primary address. It requires a Runtime-mode connection, shares the
caller's persistent queue guard and runtime monotonic clock, and reserves one
connection slot before preparing an attempt. Each operation prepays two MiB
plus 192 KiB of protocol I/O and 4096 records for receipt inspection, publication,
runtime admission and bounded recipient staging. The earliest input deadline,
work-meter deadline or ten-second phase limit covers all connection work.
There is no waiter queue, address fallback or hidden DNS operation.

Known success transfers the opaque Dispatch and passive receipt to the caller;
known refusal releases the prepared identity. Unknown COMMIT retains exactly
that Dispatch and refuses another publication. Independent settlement capacity
only inspects the retained receipt: absence and errors keep it, and a matching
receipt releases custody without changing account state. Settlement never
republishes, constructs a replacement attempt or authorizes SMTP. Recovery and
current transport admission remain separate operations.

The trusted caller selects the primary and owns primary-change notification,
process-lifetime pending custody, runtime clock/guard sharing and scheduler
admission. No client-controlled address, credential or route enters this API;
the supplied Claim binds current policy and the prepared route. Connection
credentials are borrowed for each call and are not retained with the attempt.
Production scheduler wiring and backend cutover remain separate work.

The real PostgreSQL fixture checks preparation-mode refusal, exhausted work,
original deadlines, startup retry hold, fresh publication and envelope access,
unknown-COMMIT blocking, retained absence/deadline/capacity failures, and
historical settlement without a second account-sequence increment.

## 140. Bounded relay phase publication custody

The prepared dispatch worker uses one shared pending-operation slot for
attempt preparation and durable Body, AcceptancePossible or result changes.
An uncertain commit of either kind blocks both kinds of publication. The
retained operation includes its exact attempt, phase, subset, replies and
digest; settlement only inspects that operation's receipt. Absence, admission
refusal and database errors retain it. A matching receipt releases custody
and returns a tagged historical result without a second state mutation.

Phase publication and settlement prepay three MiB plus 128 KiB of I/O and
4096 records, covering the two bounded connections, their protocol input,
the batch writer's one-MiB output ceiling, receipt output and staging.
Attempt preparation retains its two-MiB-plus-192-KiB allowance. Both use the
same Runtime-only connection admission, independently reserved settlement
slots and earliest original job/meter/ten-second deadline. No credential or
new worker is retained in pending state.

The caller supplies actual SMTP evidence from its current attempt and closes
transport when a phase COMMIT is unknown. Known publication and recovered
receipts remain passive: neither authorizes more SMTP output, renews a lease
or acquires the separate final-outcome capacity required before terminator
handoff. Current transport/ownership fencing and runtime activation remain
separate work. No SQL, schema or persisted phase semantics change here.

## 141. Shared account sequence admission

Every state-changing account publisher uses one shared next-sequence check
inside its existing account transaction. After acquiring the account row in
the established lock order, it rereads the exact sequence and floor under
that lock, refuses a stale observation or invalid floor, and computes the
next unsigned 64-bit value with checked arithmetic. Exhaustion refuses the
whole mutation; no signed PostgreSQL integer conversion or wrap is allowed.
No-op mailbox/Email updates and already-completed cancellation preserve their
sequence. Historical receipt validation does not allocate another sequence.

The additional bounded query uses the same connection, original deadline and
protocol ceilings. It commits nothing by itself and retains no lock beyond
the caller's transaction. This consolidates the admission point needed for
durable result reservations described below; it does not enable SMTP
terminator handoff. Route revisions, configuration generations and other
independent counters retain their existing checked arithmetic.

The shared account-sequence query adds a bounded round trip to a changing
publication. Mutation workers allow 1088 frames, including fixed headroom for
statement-close and COMMIT responses, so the maximum Email deletion batch fits; aggregate bytes, per-frame/authentication limits, output
allowances and the original deadline remain unchanged.

## 142. Durable logical final-result capacity

Schema version 27 adds private final-result reservations keyed by account and
attempt, with the exact ordered DATA subset (at most 100 ordinals). Its foreign
key to the immutable attempt binds the submission, route, destination and
owner fence; the schema ledger binds the store epoch. Runtime mail DML grants
include the new table. Upgrade refuses any old InFlight/AcceptancePossible
recipient: resolve those attempts using the old binary before upgrading. It
does not manufacture capacity promises for previously published phases.

AcceptancePossible now allocates one future account-sequence increment and
one slot in each alternative acceptance, refusal, recovery and expiry receipt
quota, atomically with the phase change. All thirteen account-sequence writers
preserve the aggregate outstanding claims under the account row lock. The
shared query reads claims with sequence/floor in one round trip. Every writer
of those four receipt classes admits actual receipts plus outstanding claims
against its existing 1024-row ceiling. Claims are bounded by that same ceiling;
all counts use bounded indexed account scans. These are credits for a future
increment, not preassigned sequence values.

A final positive/negative DATA result validates and consumes precisely its
reserved subset in the same transaction as recipient state, CHANGE and receipt.
Current-owner interruption and later-owner recovery consume the exact remaining
AcceptancePossible subset while recovering the whole bounded attempt. Expiry
prioritizes one reserved subset and expires it completely in one transaction,
even when its ordinals straddle an ordinary 100-row sweep boundary. Ordinary
pending recipients wait for later sweeps. Prepared recipients outside the DATA
subset have no remote-acceptance obligation and no reservation promise.

Consumption occurs before shared sequence/receipt admission inside the same
transaction. A conflict, expiry hold, clock refusal or failed commit rolls it
back. Unknown COMMIT settles the retained operation; historical replay never
allocates or consumes a second claim. Lease expiry or owner takeover alone
cannot release capacity. Cancellation already refuses AcceptancePossible rows.
The attempt foreign key also prevents removal of an outstanding claim's owner
record. Physical disk/WAL availability and database connectivity are not
reserved, and synchronous failure after remote acceptance remains uncertain.

This is durable logical capacity only. The relay must still retain independent
local connection/work/reply capacity, its original attempt deadline and live
ownership before terminator handoff. Passive phase receipts remain historical
evidence; BodySent still exposes no terminator API, and no scheduler is enabled.

## 143. Retained local final-result work

The prepared relay publication worker can reserve one process-local connection
slot and prepay the existing phase I/O/record allowance for a named attempt's
final positive or negative DATA result. Its non-cloneable FinalWork retains
that slot until consumption or drop. Consumption accepts only that attempt's
phase 4/5 Change, uses the already held slot and allowance, and cannot renew
the reservation's original job/meter/ten-second deadline. A held slot reduces
ordinary publication capacity, so publishing AcceptancePossible while holding
it requires another ordinary slot. Settlement still uses independent capacity.

The reservation borrows its original worker, connection configuration and
pending-operation owner. An unresolved operation refuses reservation or
consumption. Publication uses the same bounded native phase transaction and
transfers an uncertain COMMIT request into Pending for historical settlement.
No second pending queue is introduced. Abandoning an unused local allowance
releases its connection slot but does not refund charged work or remove any
durable reservation.

This reserves connection/work capacity only. Bounded reply storage and native
publication allocations remain separate, as do durable logical claims and live
ownership. FinalWork does not authorize SMTP output, and a returned receipt
may be historical. BodySent remains closed; this increment enables no scheduler
or terminator handoff.

## 144. Preallocated final-reply evidence

An admitted relay worker can prepare a non-cloneable FinalReply before
AcceptancePossible for an exact ordered subset of 1..100 recipients. It owns
one 4096-byte reply buffer per recipient and both possible final-phase digest
prefixes. Consuming it accepts only a normalized genuine 2xx, 4xx or 5xx final
DATA reply, fills those existing buffers, and returns the ordinary immutable
phase Change. Invalid ordinal sets, oversized/control-bearing replies and
non-final reply classes refuse. It grants neither publication nor SMTP authority.

Change now shares its private immutable fields through Arc. Native receipt
inspection and publication clone that owner rather than allocate copies of the
reply/ordinal vectors. The request digest format stays identical to ordinary
accepted/refused construction, so either form settles or replays the other's
receipt. DATA-command refusal still completes its private fields before
exposing a Change; no shared fields can be mutated afterward.

FinalReply finishing uses retained string capacities and inline SHA-256 states.
It does not allocate new reply buffers or a new Change owner. Native database
connections, commit markers and bounded SQL rows remain separate allocations;
this does not promise allocation-free database publication or physical disk/WAL
availability. The local FinalWork allowance, durable logical claim and live
ownership are still separate prerequisites. BodySent remains closed and no
scheduler or terminator path is enabled.

## 145. New AcceptancePossible publication

The native phase API and bounded publication worker expose publish_new_possible
for phase 3 only. It refuses every existing matching receipt, both during the
initial lookup and again under the publication transaction's account locks.
This closes the race where a receipt appears between those checks. Ordinary
publish and inspect retain historical idempotent settlement behavior. Wrong
phases refuse, and differing request digests retain their existing conflict
behavior. The actual new transaction uses the same policy/owner checks, exact
subset validation, durable capacity claim and synchronous COMMIT path.

The worker retains uncertain COMMIT in its existing Pending slot, using the
same original deadlines and bounded work admission. Settling that request
returns only historical evidence; a later new-publication call still refuses
its receipt. No retry silently turns settlement into new publication.

Known new publication is a prerequisite for future handoff construction, not
SMTP permission by itself. Independent local work/reply capacity and current
ownership remain required. No caller-provided Outcome can authorize output;
BodySent remains closed and no scheduler or terminator path is enabled.

## 146. Bound final-result custody

The publication worker can reserve a non-cloneable FinalPlan that binds one
exact attempt, owner/generation and ordered recipient subset to both FinalReply
storage and FinalWork capacity. Its consuming publish_possible operation uses
the new-only phase-3 path, retaining the local owner only after a known new
commit. Existing receipts refuse; uncertain COMMIT transfers the exact phase
request to Pending and releases local capacity. Settlement cannot manufacture
a FinalResult. A second ordinary connection slot is needed during publication.

FinalResult privately retains the original request, reply storage, work and
passive receipt. Its consuming record operation accepts only actual normalized
final DATA reply text; it constructs and publishes the result for the retained
subset without a caller-supplied replacement Change, fresh slot or work charge.
The original job/meter/ten-second reservation deadline caps phase publication
and result consumption; later admission cannot extend it. Expiry after a known
phase commit refuses the local owner and leaves durable recovery responsible
for the unresolved phase. Dropping either owner releases local capacity only.

This connects capacity and known publication, not transport authority. Live
owner/queue-clock admission and custody of the matching SMTP body transaction
remain required before terminator handoff. Passive outcomes cannot construct
these private owners. BodySent remains closed and no scheduler is enabled.

## 147. Queue-clock observation for new phase publication

The bounded worker's new AcceptancePossible path observes authoritative
PostgreSQL UTC through the process's persistent queue-clock Guard. It pairs
each observation with the original runtime monotonic clock and ignores the
adapter's wall time. It checks under the owner lock on admission and again
immediately before COMMIT, requiring current ownership and an unexpired
submission at the second check. A backward transaction-time observation also
refuses. The same database-time admission helper serves initial attempt
publication; its fresh/retry policy is unchanged.

A clock hold, invalid sample or busy guard refuses the new phase. The SQL
transaction rolls back recipients, sequence, receipt and durable capacity
claim, but the process-local clock observation survives. Database time is
observed before owner-clock/lease refusal, so a failed transaction cannot hide
a discontinuity. Unknown COMMIT still transfers to Pending. Historical receipt
inspection and ordinary publication remain historical paths, and duplicate
new requests still refuse before obtaining any continuing authority.

This is publication admission, not a live transport grant. The low-level
unobserved phase API remains storage machinery; known new publication alone
never permits SMTP output. Current ownership/clock validation and matching
transport custody at actual handoff remain outstanding; BodySent stays closed.

## 148. Live AcceptancePossible state checks

A retained FinalResult can recheck its exact phase request through a bounded
native transaction. It requires current policy and owner, the persistent queue
clock, healthy/due route state, the exact receipt, an active matching attempt,
an uncompleted/unexpired submission, the entire still-InFlight phase-3 subset,
and its exact durable final-result reservation. Remaining Prepared recipients
outside that subset are permitted; no other outside phase is. A historical receipt alone is
insufficient. Account/configuration/owner locks end before the call returns;
the operation changes no recipient, receipt, reservation or account sequence.

The worker uses another ordinary connection slot and prepaid bounded work,
while retaining its final-publication slot. Pending operations refuse the
check. The original final-work deadline caps the operation. A final owner/clock
observation also checks submission expiry and time regression. Its returned
local limit starts before that database observation and subtracts one
millisecond for timestamp rounding, then caps remaining lease/submission time
by the original deadline. Database response latency cannot extend that limit.
An expired response refuses rather than returning an already elapsed bound.

The result is a passive observation, not a transferable SMTP grant. A paused
worker or policy/takeover change can still race a later write as section 4
explains. The future transport owner must call this path internally, retain the
matching body transaction and final-result capacity, and abort on refusal;
caller-supplied receipts or Instants cannot authorize terminator output.
BodySent remains closed and no scheduler is enabled.

## 149. Retained final-publication failures

FinalResult recording returns a non-cloneable FinalFailure when publication
fails. Once genuine normalized final reply text has been prepared, that failure
retains the exact immutable Change and original FinalWork slot/deadline. A
known database refusal no longer discards the actual reply or local capacity.
Invalid reply preparation has no retained final evidence. Debug output exposes
only the error, not reply text or connection credentials.

An explicit retry prepays another bounded phase I/O/record allowance while
using the held slot. It cannot replace the attempt, subset or reply, and its
deadline can only shorten the original work limit. Budget, deadline and Pending
refusals return the still-retained failure owner. There is no automatic retry
or new job lifetime. Unknown COMMIT also keeps the exact request in Pending;
that operation must settle historically before another publication can proceed.

Success releases the local owner. Explicit drop abandons local custody and
releases its slot; it neither consumes nor deletes the durable reservation.
The bounded runtime must retain or settle its actual reply before dropping
that responsibility, and recovery remains conservative after a crash. The
low-level FinalWork consuming API remains storage machinery; future handoff
uses the retained FinalResult failure path. No terminator API is opened here.

## 150. Final DATA transport handoff

The prepared PostgreSQL handoff consumes a non-cloneable FinalResult and the
matching completed BodySent. Only known new AcceptancePossible publication
can construct that result owner. The trusted runtime remains responsible for
the immutable body source, actual 354, durable Body fence and matching attempt
and recipient subset; BodySent alone proves encoding/flush, not those facts.
This opens the previously closed terminator boundary through this owner only;
no scheduler or production PostgreSQL backend is activated.

BodySent retains its original clock and deadline. Handoff requires identity
with the worker's runtime clock, keeps that body deadline and the original
final-work Instant cap, and borrows separate wire and database work meters
through completion. The command adapter privately frames exactly `.CRLF` into
three reserved bytes and reuses the existing bounded write/flush/reply parser.
Its public command enum still exposes no terminator or raw command.

Before each terminator write or flush turn, the owner internally rechecks its
exact live AcceptancePossible state. Each check charges bounded database work
and uses a second ordinary slot while final-publication capacity stays held.
The returned lease/submission cap can only shorten the retained local limit;
original monotonic deadlines are checked before and after transport turns.
A refused check aborts transport. Database locks never span network work, and
section 4's pause/takeover and already-buffered-byte race remains explicit.

After flush, only bounded reply reading remains. A complete actual 2xx, 4xx
or 5xx reply is published through retained FinalResult capacity before reporting
completion. Once the command owner returns a complete reply, later deadline
expiry is handled by retained publication refusal; it cannot discard the reply. No later live-state check substitutes for collecting that reply.
Non-final, malformed, truncated or out-of-budget replies refuse without an
invented result. Publication failure returns FinalFailure with the exact
reply and held slot, plus the closed transport buffers; unknown COMMIT also
remains in Pending. Completion is a passive durable receipt and closed buffers,
never a reusable session. Callers must retain/settle failed publication custody.

Real paired-TLS tests cover exact terminator framing, fragmented final replies,
publication ordering and retention, non-final/malformed/truncated refusal,
live admission refusal, original work expiry, clock mismatch and exhausted
wire capacity. The existing real PostgreSQL worker fixture independently
qualifies native live checks and retained final-result publication/retry.

## 151. Native final-handoff integration fixture

The real PostgreSQL queue fixture drives fresh attempts through the public
prepared relay components on paired verified TLS connections. It publishes
Prepared before connecting, reads the matching envelope, performs EHLO and
PLAIN authentication, and sends actual MAIL/RCPT/DATA commands. Actual positive
RCPT evidence supplies the Body transition after 354. The immutable stored
transmitted body is verified and streamed through BodyExchange under the same
runtime clock and original attempt deadline.

Final capacity is reserved before body handoff. Known new AcceptancePossible
publication supplies FinalResult; the public final exchange sends the terminator
and publishes the actual 250. The peer checks that PostgreSQL still records
phase 3 when the terminator arrives, before sending its reply. The fixture
requires exactly one terminator and inspects the matching durable final receipt.
A second transaction injects a final-receipt INSERT refusal, retains the exact
reply owner, removes the fixture fault and explicitly retries that publication.

This qualifies composition of native phase/result ownership and real SMTP TLS
transport. Fixture policy and credentials are prepared explicitly, and address
selection uses a local socket pair. It does not qualify resolver/scheduler
activation or switch the production backend. Existing isolated component tests
cover the broader malformed reply, live refusal and uncertainty cases.

## 152. Live Body storage validation

The prepared phase API can publish a known new Body transition with the same
persistent queue-clock observation used for AcceptancePossible: admission and
pre-COMMIT checks both use database UTC paired with the runtime monotonic
adapter. Existing receipts refuse the new-only path. Ordinary publication and
inspection remain historical settlement, including after later phases.

Live Body validation shares the bounded current-state reader with
AcceptancePossible. It requires the exact receipt, active attempt and selected
InFlight Body subset under current policy, owner, route-health and clock checks.
The retained positive RCPT text must equal each selected recipient's stored
reply; data replies are absent and changed sequences equal the Body receipt.
Prepared recipients outside the selected subset remain permitted. Durable final
capacity is required only for AcceptancePossible, where it is reserved.

The returned local deadline conservatively bounds the current owner lease and
submission expiry without extending the input limit. All locks end before
return; validation neither mutates queue state nor grants lasting SMTP
authority. The trusted caller still owns actual 354, the matching immutable
source, original transport deadlines and validation before body output. This
storage increment does not wire a body transport owner or activate a scheduler.
Section 4's pause/takeover and already-buffered-byte limitation remains.

The real PostgreSQL fixture exercises both phase readers and clock-observed
publication paths, including clock jumps before and after staging, adapter
failure, persistent holds, wrong-phase and duplicate refusal, missing receipts,
changed policy/owner/health/expiry, and exact Body RCPT mismatch. Historical
Body receipt replay and final-capacity checks retain their existing coverage.

## 153. Bounded Body publication ownership

The prepared dispatch worker constructs a non-cloneable BodyResult only after
known new clock-observed Body publication. Its immutable change binds the exact
attempt, owner generation, subset and actual RCPT replies. Publication prepays
the bounded phase allowance and acquires one ordinary slot; the owner retains
the original earliest job/meter/ten-second Instant limit, not that slot.
Unknown COMMIT keeps the exact change in the shared Pending barrier and returns
no BodyResult. Historical settlement cannot reconstruct one, and duplicate
new-only publication refuses even after settlement or explicit owner drop.

Each current-state check independently prepays the phase allowance, acquires an
ordinary slot, checks Pending and uses the native live Body reader. The shared
admission helper also serves FinalResult without changing its retained final
capacity. No fresh meter or later lease renewal can extend the original owner
limit. A later phase refuses Body validation. Dropping an owner releases local
custody only; it does not alter durable phase or recovery state.

The real PostgreSQL worker cases cover mode, budget, slot and deadline refusal,
exact charging, known new ownership, original expiry, unknown COMMIT retention,
historical settlement and duplicate refusal. The paired SMTP TLS fixture uses
BodyResult after actual 354 and validates it before the immutable body transfer;
after AcceptancePossible it requires Body validation to refuse. This is an
initial caller check, not yet a body transport owner enforcing every write or
flush. Actual source matching, repeated transport fencing and scheduler/backend
activation remain separate work.

## 154. Final capacity derived from Body custody

BodyResult can be consumed into a non-cloneable BodyPlan that pairs its exact
Body operation with final-result capacity. The phase-3 change and preallocated
final reply derive internally from that Body change's claim, generation,
attempt and ordered recipient subset; the caller supplies no replacement
identity or subset. Reservation uses the same worker, original input deadline
and earliest retained Body limit, further shortened by reservation admission.

BodyPlan keeps one ordinary slot for final publication while every live Body
check acquires another and prepays its own bounded work. Its checks use the
combined limit, so a new meter cannot extend either original Body ownership or
a shorter reservation bound. Drop or refusal releases only local capacity.
After the matching body is transferred, consuming the plan publishes known new
AcceptancePossible through the existing FinalPlan path. Uncertainty remains in
Pending and yields no FinalResult; historical settlement grants no new owner.

The caller still owns actual 354, immutable source matching, body completion
and checks before each transport write or flush. This binds the two storage
owners; it does not yet implement the body transport wrapper or activate the
scheduler. Existing low-level trusted APIs remain available as storage and
protocol machinery.

The real PostgreSQL worker fixture checks held-slot exclusion, inherited Body
expiry despite a fresh meter, refusal to publish after expiry, and slot release.
The native paired-TLS fixture derives final capacity from BodyResult, refuses
live checks while the remaining ordinary slot is occupied, then checks Body,
streams its immutable bytes and completes the same retained final handoff.

## 155. Native Body transport handoff

The prepared PostgreSQL body wrapper consumes BodyPlan, an authenticated
session and the matching admitted immutable source. It constructs the bounded
body encoder with the owner's runtime clock and original deadline, borrowing
separate wire and database meters. The trusted caller still owns actual 354,
compatible MAIL mode and source/session matching. This activates no scheduler
or production backend.

Before each body write or flush turn, the wrapper internally checks the exact
live Body state through the reserved plan. Every check charges the bounded
phase allowance and acquires a second ordinary slot while final capacity stays
held. The returned lease/submission bound only shortens its local cap; original
Instant limits are checked before and after every transfer turn. Refusal closes
the transport and releases local ownership. Locks do not span network work;
section 4's pause/takeover and buffered-byte limitation remains unchanged.

After all declared bytes are encoded and flushed, the wrapper internally
publishes known new AcceptancePossible using the matching reserved plan.
Publication refusal closes the transport; unknown COMMIT remains in Pending
and yields no final owner. Known success returns a non-separable completed-body
and FinalResult pair. That pair can start the existing final handoff or close
its transport; callers cannot substitute a different body or result owner.
No terminator is emitted by the body wrapper itself.

Paired-TLS tests check admission before every write/flush, exact transparency
without a terminator, refusal before any output, shortened expiry, malformed
body input and failed possible publication. The full native PostgreSQL/TLS
fixture now uses the wrapper through actual final-result publication and
retained-reply retry. These are bounded correctness cases, not maximum-size
throughput qualification. Full live checks currently open bounded database
connections and reread the selected replies per write/flush; their cumulative
cost must fit the original meter and deadline. Large-body throughput and any
future admission batching require explicit qualification before activation;
no allowance or deadline is silently enlarged here.

## 156. Observed Prepared-envelope admission

The native observed-envelope reader shares the untouched Prepared validation
with the passive envelope reader. Retried Prepared rows retain the previous
attempt's replies as required by section 117; neither reader treats those
historical replies as progress in the new attempt. It additionally observes database UTC through
the caller's shared queue-clock guard before reading the envelope and again
under the final owner check. Clock failure, a discontinuity or a retained hold
refuses; failed transactions do not roll back the process guard. Fresh route
health admission refuses backoff and repair holds.

The returned Instant is the minimum of the caller's original limit and the
remaining owner lease/submission lifetime measured from the start of the final
owner query, discarding one millisecond for database timestamp rounding. A
backwards final timestamp, exhausted lifetime or expired returned bound refuses.
The reader changes no durable queue state, and all database locks end before
return. The envelope and deadline are passive observations: historical receipts
cannot reconstruct transport ownership. A bounded Prepared worker owner and
SMTP envelope orchestration remain separate work; no scheduler is activated.

Real PostgreSQL cases cover both clock observations, persistent holds, failed
clock adapters, guard contention, route backoff and repair, original deadline
and submission-expiry caps, unchanged account sequence, malformed envelope
rows, partial RCPT publication, advancement to Body and valid retry envelopes
that retain previous replies.

## 157. Bounded Prepared publication custody

The worker can publish a Prepared attempt through a new-only path and retain a
non-cloneable PreparedResult only after a known new commit. Both the initial
receipt lookup and the locked recheck refuse historical replay on this path.
The original historical publication API remains passive. Unknown COMMIT keeps
the exact Dispatch in the shared Pending barrier and returns no owner;
settlement cannot construct one.

PreparedResult retains the worker, exact publication and earliest original
job/meter/ten-second Instant cap. It exposes passive identity/outcome references
and a bounded observed-envelope read. Each read checks Pending and original
expiry, prepays the attempt allowance and acquires an ordinary slot, then uses
the native observed reader with the retained cap. A new meter or lease renewal
cannot extend the original bound. No slot or database lock remains held after
the read. Drop releases local custody only.

The returned envelope and conservative deadline are still passive evidence.
SMTP envelope orchestration must retain its minimum bound and carry the
original job limit into later Body custody; this increment does not compose
that transport owner or activate a scheduler. Trusted low-level phase APIs
remain separate.

Native worker cases cover runtime mode, budgets, slots, exact read charging,
original expiry despite a fresh meter, Pending exclusion, uncertain commit
retention and historical settlement. Storage cases require new-only replay to
refuse while historical publication still returns the exact receipt.

## 158. Prepared-to-Body deadline and identity continuity

A PreparedResult can be consumed after actual 354 to publish Body for its own
claim, generation and attempt. The caller supplies only the actual accepted
RCPT replies; the native phase transaction still validates their ordered
subset against the original batch. The worker clamps publication itself to
the retained Prepared Instant limit and original input deadline, and any new
BodyResult retains that same combined bound. Entering DATA cannot refresh the
job allowance. The final reservation and body/final transport wrappers already
carry that bound onward.

Known new Body publication returns BodyResult and the matching passive
Dispatch for immutable body reading. An error returns no Body owner; uncertain
COMMIT retains the exact phase operation in Pending. The caller still owns
actual SMTP evidence, the matching source/session and minimum observed lease
bounds during earlier envelope transport. This does not implement that
transport orchestration or activate a scheduler.

The native TLS fixture uses this handoff after actual 354 and checks that Body
admission never exceeds the earlier Prepared bound even when the supplied
wire/input deadline is longer. It then transfers the verified immutable body
and exercises actual final publication and retained-reply retry. Worker tests
also require the handoff to refuse an expired Prepared owner despite a fresh
meter.

## 159. Retained initial envelope and minimum observed cap

Consuming PreparedResult through initial envelope admission produces a
non-cloneable EnvelopePlan. It pairs that exact bounded envelope with the
Prepared owner and clamps the owner's limit to the returned native lease,
submission and work bound. The public envelope view is borrowed passive data;
callers cannot replace the stored envelope or construct a plan from a receipt.

A subsequent untouched-envelope check charges and reads through the same
native path, requires the complete envelope to equal the retained one, and
only shortens the stored limit. A new meter or renewed lease cannot enlarge
that minimum. Consuming the plan into Body carries this bound through the
existing Prepared handoff and final-result chain.

These checks apply while all original recipients remain untouched Prepared.
They do not reconstruct an envelope after partial RCPT publication, implement
SMTP command sequencing or prove actual 354. Those transport duties remain
with the caller until native envelope orchestration is composed. No scheduler
or production backend is activated.

Native worker tests cover identity substitution refusal and retention of the
original view, a shortened observation followed by a longer fresh meter,
capacity/slot/Pending refusal and expiry. The real SMTP/TLS fixture uses the
retained envelope for MAIL/RCPT and consumes the plan after actual 354 before
running the existing Body and final-result wrappers.

## 160. Reserved negative envelope-command reply storage

CommandReply reserves bounded normalized-reply cells and hash contexts before
MAIL or DATA-command transmission. The MAIL reservation fixes the complete
Prepared ordinal batch; the DATA reservation fixes the actually accepted
RCPT subset and retains its positive replies. Finishing consumes only an
actual normalized 4xx/5xx reply, fills the existing cells and preserves the
existing command-receipt digest. Positive or interim replies cannot become
negative evidence. Identity, command stage and subset cannot be replaced at
finish. At most 100 recipients and 4096 bytes per reply remain the bounds.

These objects contain passive evidence storage, not live Prepared custody,
transport permission, database capacity or a publication retry owner. They
are preparation for envelope orchestration: callers still reserve database
work and retain the resulting Change across failed publication. Native SQL
allocation remains separate. No scheduler or backend is activated.

The native command fixture exercises both reply classes at both command
stages through these reservations, with maximum-sized distinct RCPT replies
and message refusals, partial DATA subsets, stale ownership, transaction
failure and uncertain COMMIT. Inspection through the legacy constructors
must find the same receipt, pinning digest compatibility.

## 161. Bound MAIL evidence and retained publication failures

EnvelopePlan can reserve a non-cloneable MailPlan before MAIL transmission.
The reservation derives the complete Prepared ordinal batch, attempt and
owner generation from retained publication custody. It pairs the envelope
with CommandReply storage and exposes the same bounded untouched-envelope
checks. A normalized actual positive MAIL reply consumes the reservation
back into the original envelope owner without extending its deadline.

A genuine normalized negative MAIL reply first fills the retained evidence,
then attempts ordinary bounded native publication. Every admission or database
failure after preparation returns CommandFailure holding the exact immutable
Change, original worker and minimum deadline. Invalid reply preparation has
no retained evidence. Debug output exposes the error only. Explicit retry
prepays fresh work and acquires an ordinary slot; an admitted shorter meter
shortens the retained limit even if publication fails. No deadline can renew.
Unknown COMMIT also remains in the worker's existing Pending owner and must
settle before another publication attempt. The failed owner adds no second
pending queue or held connection slot.

These owners do not send SMTP or prove that supplied replies came from their
session. The caller still owns actual MAIL sequencing, fresh checks before
output, compatible body mode and connection closure on refusal. Dropping
CommandFailure abandons local evidence, not durable queue responsibility;
the runtime must retain or settle it. No scheduler or backend is activated.

The native SMTP/TLS fixture consumes actual positive MAIL before its existing
Body/final path. A real negative MAIL reply survives a transaction refusal,
empty retry budget and occupied slots before successful publication with the
same receipt. A separately shortened owner retains an actual negative reply
after expiry and refuses revival by a fresh meter.

## 162. Bound DATA-command result alternatives

EnvelopePlan can reserve a non-cloneable DataPlan for a nonempty ordered
subset of its admitted recipients and their actual positive RCPT replies.
Recipients outside the retained envelope refuse before DATA. Construction
prepares both the exact Body request and negative DATA-command reply storage
under the retained claim, generation and attempt; no later recipient list or
RCPT text can replace them. The existing untouched-envelope checks retain the
minimum original bound through this owner.

Only an actual normalized 354 consumes the plan into new clock-observed Body
publication. The private Prepared handoff uses the already retained Change,
preserving all existing deadline, novelty, Pending and live-phase checks.
A genuine normalized 4xx/5xx instead fills the reserved refusal evidence and
uses CommandFailure to retain it across failed publication or retry. Both
paths keep the original identity and deadline; neither admits new SMTP work
from a historical receipt.

This owner still relies on the caller for actual DATA sequencing, matching
session, fresh pre-output checks and connection closure on refusal. Negative
RCPT publication ordering remains an envelope-orchestration responsibility;
untouched-envelope checks cannot reconstruct a partially progressed batch.
No scheduler or production backend is activated.

The native SMTP/TLS fixture publishes Body from actual 354 through DataPlan,
retains an actual negative DATA reply across database refusal and retry,
refuses a recipient outside the retained envelope before DATA, and rejects
an actual non-354 reply without publishing Body.

## 163. Reserved per-recipient SMTP evidence

RcptReplies reserves one maximum normalized reply cell for each ordered
recipient in an initial attempt batch, at most 100. It retains the exact
claim, generation, attempt and ordinal sequence. Recording accepts only the
next ordinal and an actual normalized 2xx/4xx/5xx reply, moving a reserved cell
into positive or negative storage without a new reply allocation. Invalid
input leaves the next ordinal and all earlier evidence unchanged.

Consuming finish closes collection and reports whether every planned RCPT
received a reply. It exposes retained positive evidence and, when any negative
replies were collected, one immutable ordinary RCPT-refusal Change with the
existing digest encoding. A partial collection is usable only after the
caller has fenced its interrupted session; publication cannot be followed by
more RCPT commands for that attempt because there is only one refusal receipt.
No positive-RCPT persistence, transport permission, database admission, or
phase owner is created by this passive collector. The orchestration of
interruption and positive-reply persistence remains separate work.

The native PostgreSQL fixture uses the collector for mixed and all-negative
batches, including 100 maximum-size distinct replies, and compares publication
receipts against the existing constructor. It exercises invalid input without
losing earlier evidence, partial collections, all-positive collections, and
continues the positive subset through Body using its retained actual text.

## 164. Positive RCPT evidence on current-owner interruption

Current-owner recovery can retain an ordered subset of actual positive RCPT
replies that has not yet reached Body. interrupted_with_rcpt binds at most
100 normalized 4096-byte replies to the exact retained recovery operation;
a domain-separated digest includes their ordinals and text. The existing
no-evidence interruption and abandoned-recovery encodings stay unchanged.

Publication requires every supplied recipient to remain InFlight/Prepared
under the current attempt and owner fence. A foreign, settled, Body or
AcceptancePossible recipient refuses the whole operation. Within the existing
atomic recovery transaction, supplied positive replies replace prior RCPT
history and clear prior DATA history before the ordinary interruption state
transition. Other recipients retain their recorded evidence, and earlier
uncertainty remains latched. This cannot establish Body or authorize new SMTP
output: the caller must first close or fence the interrupted session.

The publisher uses the existing bounded batch client so 100 maximum-size
reply parameters fit within its one-MiB output allowance. Input remains one
MiB/1088 frames and the original ten-second deadline cannot renew. Existing
replay, expiry hold, receipt capacity, sequence and unknown-COMMIT rules apply.

The real PostgreSQL fixture covers partial evidence with untouched history,
refusal of replacing Body/Possible replies, maximum-size old and new reply
cells, stale/foreign/invalid input, expiry hold, rollback, delayed-COMMIT
settlement, exact replay and preservation of earlier uncertainty.

## 165. Bounded recovery publication and settlement custody

Dispatches accepts a retained recovery operation only after the caller has
closed or fenced the affected transport. It prepays ordinary PHASE_IO work
and 4096 records under the supplied job/meter deadline, acquires an ordinary
slot, and publishes through the native recovery transaction. Known success
returns only a passive receipt. A known failure releases Pending; the caller
retains an operation clone for explicit retry.

Unknown COMMIT leaves the exact recovery operation in the same single Pending
owner used by dispatch and phase publication. All new dispatch, phase,
recovery and final-capacity work stays blocked. Settlement prepays fresh work
and uses the caller's independent settlement slots to inspect that exact
receipt; absence or error retains custody, and only a found receipt clears it.
Settlement never republishes or creates continuing SMTP authority. This is a
bounded settlement job, not a renewal of any expired transport owner.
No production scheduler or backend is activated.

The real PostgreSQL fixture checks runtime mode, budget/deadline/slot refusal,
exact work charging, known rollback and retry, delayed COMMIT, blocked new
work, missing-receipt retention and historical settlement while ordinary
capacity is occupied. It verifies preserved positive replies, attempt
retirement, and an unchanged account sequence during receipt inspection.

## 166. RCPT collection bound to admitted envelope custody

EnvelopePlan reserves RcptPlan from its own complete admitted ordinal batch,
claim, generation and attempt. The plan exposes only the next retained
recipient, records ordered actual replies through RcptReplies, and performs
fresh untouched-envelope checks while retaining the original minimum cap.
Actual command sequencing, session matching and pre-output checks remain
caller responsibilities.

Finishing pairs passive evidence with that same envelope owner. Only a
complete collection with at least one accepted recipient can reserve DATA;
it derives both DATA alternatives from retained positive replies and returns
any exact negative-RCPT Change alongside the DataPlan. The original cap follows
DATA custody. A planning failure retains the complete RcptResult so invalid
partial/all-negative choices or allocation failures do not discard replies.

After closing or fencing the matching session, consuming interruption planning
instead returns the exact negative refusal and, when recipients remain, a
current-owner recovery request carrying retained positive replies. A complete
all-refused batch needs only its refusal, which retires the attempt. Partial
collections cannot create DATA custody. These are passive settlement requests,
not a renewal of transport ownership.

The caller publishes negative RCPT evidence before Body, DATA refusal or
interruption recovery, stops on unknown COMMIT, and retains actual DATA replies
across any failed negative-RCPT publication. Untouched-envelope current checks
remain invalid after partial publication. These planning helpers do not perform
SMTP orchestration or activate a production scheduler/backend.

The paired SMTP/TLS fixture uses the bound collector and retained positives
for DATA, exercises mixed acceptance/refusal through final handoff, settles an
all-refused batch without DATA, and interrupts after one positive reply while
preserving its evidence. Failed DATA planning retains both partial and
all-negative results for their correct settlement.

## 167. Negative DATA custody across RCPT-refusal publication

RcptDataPlan can capture an actual negative DATA reply into its preallocated
command cells before either database operation is admitted. Private fields
prevent substituting either half of the pair; explicit into_parts retains the
existing caller-managed path and its evidence/order obligations. The caller fences
the matching SMTP session first. It then publishes any negative RCPT subset
before the negative DATA result. Both exact operations stay together on a
known failure; a successful RCPT publication is removed from the retained
sequence. Unknown COMMIT stays in the existing Pending owner and blocks retry
until inspection settles it. Receipt replay can only continue passive result
publication, never SMTP output.

Retries retain the original identity, actual replies and minimum admitted
limit across both steps. An empty budget, occupied slot or expired deadline
preserves that custody and cannot renew the cap. This is not post-expiry
settlement or general SMTP orchestration; no backend is activated. Existing
low-level paths retain their explicit caller responsibilities.

The native paired TLS fixture captures a mixed RCPT/negative-DATA exchange,
forces rollback at either publication, checks intermediate state and retained
identity, refuses retry with an empty budget, and then verifies both exact
results and attempt retirement.

## 168. Passive command evidence after bounded retry custody

CommandFailure and RcptCommandFailure may consume their retained actual
negative command evidence into immutable passive Change requests after the
matching SMTP session is closed or fenced. The paired form preserves any
unpublished negative RCPT step before its exact DATA-command result. Invalid
input that never created retained evidence yields None. Extraction allocates
no reply storage and carries no transport owner, connection permit or deadline.

Original command retries still cannot renew their earliest admitted cap.
Separate settlement explicitly admits fresh bounded database work through the
same Dispatches/Pending owner and publishes the preceding refusal first. An
unresolved COMMIT continues to block publication until exact receipt inspection;
extraction neither clears Pending nor declares an uncertain operation absent.
Native publication still verifies current durable ownership and phase, or an
exact retained receipt. Expired route ownership, replacement, cancellation and
receipt-retention expiry are not bypassed. This separates a local SMTP work
cap from passive database settlement; it never revives SMTP output permission.

The real TLS/PostgreSQL fixture verifies expired MAIL refusal and mixed DATA
refusals whose preceding RCPT step either failed or committed. Normal retries
remain expired despite a fresh meter; extracted evidence then settles through
fresh database admission and produces exact stored replies and retired attempts.
A delayed RCPT COMMIT case additionally extracts the pair while Pending is
occupied, verifies that command publication stays blocked, inspects the exact
receipt through an independent settlement slot, and only then replays the
passive refusal and publishes the retained DATA result.

## 169. Interrupted Body publication with exact RCPT evidence

A separately domain-separated interrupted_with_body_rcpt request handles a
fenced session whose Body publication may have completed. It retains the same
bounded ordered actual-positive subset as interrupted_with_rcpt. Under current
ownership and recipient locks, each supplied member must be Prepared or Body.
Prepared members receive the retained actual RCPT and clear prior DATA;
already-Body members must already contain byte-identical RCPT evidence and no
DATA reply, and are not rewritten. Supplied AcceptancePossible, mismatching,
foreign or settled members refuse. Existing interrupted_with_rcpt remains
Prepared-only; its digest and ordinary recovery encodings are unchanged.

The request then applies ordinary interrupted recovery atomically with its
receipt, preserving uncertainty and existing retry/expiry rules. It does not
infer a missing COMMIT: the bounded worker still blocks on Pending until exact
receipt inspection. This supplies passive evidence settlement before or after
Body without manufacturing a transport owner from history.

Native fixtures cover an entirely Body positive subset, mixed Body/Prepared
members, mismatching replies, AcceptancePossible refusal, expiry hold, rollback
of replies and sequence, delayed COMMIT inspection/replay, and attempt retirement.

## 170. Positive DATA handoff retains failed publication evidence

RcptDataPlan reserves its boxed DATA owner before the command. After actual
normalized 354, its bound handoff publishes any negative RCPT subset first and
then only a new observed Body transition, with the original minimum cap. Only
known new publication before that cap yields BodyResult and the matching
immutable-body request. Historical receipts never create output custody.

Every failure retains the exact plan and its actual positive replies, plus any
RCPT refusal not known committed. The caller closes or fences the SMTP session.
The failure exposes no transport retry or owner extraction; it can only plan
passive interruption using interrupted_with_body_rcpt. Allocation or validation
failure preserves that custody. The recovery handles Prepared members or exact
already-Body evidence after a known or uncertain publication; Pending still
blocks all publication until exact unknown-COMMIT inspection. Negative RCPT
settlement precedes recovery. No result reconstruction permits output.

The paired TLS/PostgreSQL fixture uses this handoff for mixed successful DATA,
forces known failure at either publication and delayed Body COMMIT, then fences
the session and verifies retained actual replies and complete retirement through
passive recovery. Production scheduling and backend activation remain deferred.

## 171. Passive final DATA evidence after local reservation expiry

FinalFailure may consume its retained genuine final DATA Change after transport
has closed. Extraction moves existing immutable evidence and releases the held
local connection permit; it neither releases the durable final-result claim nor
clears Pending. Invalid reply input that never created retained evidence yields
None. The value conveys no SMTP session, output owner, work allowance or deadline.

Bounded retry still refuses renewal of its original cap. Separate passive
publication must prepay fresh ordinary work and obtain a slot through the same
Dispatches/Pending owner. Uncertain COMMIT blocks until independent receipt
inspection, and durable ownership, phase, receipt and final-capacity checks still
apply. A retained receipt is historical only. Confirmed result publication or
ordinary durable recovery consumes the final claim, never evidence extraction.

The native SMTP/TLS fixture forces final-result rollback, expires its retained
retry cap, verifies that extraction frees the local slot while preserving exact
evidence, and settles through fresh admission. A delayed-COMMIT retry additionally
keeps Pending occupied across extraction and requires independent inspection
before exact historical replay.
