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
and sections 16, 19, 21, 25 and 27's material/mail/device/configuration and
account-bootstrap tables,
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
adds src/postgres_configuration.sql; version 6 adds src/postgres_accounts.sql.
New initialization applies all six in
one transaction. PUBLIC grants are revoked; inherited
schema/relation grants to other roles refuse initialization and roll back.
Explicit runtime grants and active configuration selection remain
preparation work.

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
permits 256 input frames for the six migrations and their ledger writes;
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
count plus one (currently seven rows); it requires
exactly the six ordered migration versions, their compiled DDL checksums and
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
material, mail, device, immutable configuration and account-bootstrap
receipt relations, but no active configuration pointer, runtime role or
listener. The matching `td-mta
postgres schema-inspect --connection-file PATH` checks the ledger under its
shared migration lock. `td-mta postgres schema-upgrade --connection-file
PATH` applies every missing migration through version 6, or reports an
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
