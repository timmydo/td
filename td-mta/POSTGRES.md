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
section 14's preparation command; no serving path calls it. Its current server contract is
PostgreSQL major 17, qualified locally on 17.10. Other majors refuse until
separately qualified. It creates only the first preparation ledger, not a
complete mail/configuration schema or runtime role. Serving still cannot
select PostgreSQL. The owner credential is separate from the future runtime
credential; no owner credential is installed into a serving pool here.

The operation verifies the same writable-primary/durability settings as the
probe, begins one transaction, sets synchronous_commit=on, and takes an
exclusive transaction-level try-lock on the bigint migration key
0x74646d7461000001. Conflict returns Busy without waiting. It refuses any
existing td_mta namespace, even an empty one. The committed SQL in
src/postgres_schema.sql creates that schema and its schema_migrations ledger.
Version 1 records the exact DDL's SHA-256 and a fresh 128-bit store epoch in
one row with fixed length constraints. PUBLIC grants are revoked; inherited
schema/table grants to other roles refuse initialization and roll back.
Explicit runtime grants and application tables are later preparation work.

The client shares the original ten-second worker limit, configured
statement/lock timeouts and protocol byte/frame caps. Fixed SQL and typed
parameters are private to the crate; arbitrary driver execution is not a
public API. Errors before COMMIT drop the session and leave no committed
namespace. Once COMMIT may have been sent, a lost response or deadline is
Indeterminate, never a safe-to-retry refusal. A known successful commit
retains its receipt even if completing that poll crosses the deadline.
There is no automatic replay or uncertainty resolver in this preparation
API. An operator must resolve database state before another initialization.

`postgres_schema::inspect` takes the conflicting shared session-level
try-lock before reading the ledger. It requires exactly one bounded row,
version 1, the compiled DDL checksum and a 16-byte epoch, and checks its final
deadline before returning. It closes the session and lock before returning;
the receipt grants no authority for later SQL on another connection. This
checks the recorded ledger, not every catalog object or evidence against a
malicious schema owner. The future serving connection must retain its own
shared lock throughout its lifetime and validate the complete serving schema.

The real PostgreSQL schema fixture checks existing-namespace refusal,
rollback after inherited-grant refusal, shared/exclusive migration conflicts,
initialization/inspection, restart persistence and version/checksum refusal.
A second fixture delays COMMIT with a disposable deferred trigger, observes
Indeterminate at the client deadline, then inspects the committed ledger
after the original session ends and verifies that another initialization
refuses. A database commit can outlive the client deadline and disconnect;
the configured statement timeout does not establish rollback. Neither fixture
uses an existing database. They require the section 8 tools, including psql.
The advisory-lock lifetime/conflict behavior follows PostgreSQL's
[advisory-lock functions](https://www.postgresql.org/docs/17/functions-admin.html#FUNCTIONS-ADVISORY-LOCKS).

## 14. Operator schema preparation

`td-mta postgres schema-init --connection-file PATH` explicitly initializes
section 13's preparation ledger using a separately provisioned schema-owner
login. It refuses every existing td_mta namespace. It creates no mail store,
application configuration, runtime role or listener. The matching
`td-mta postgres schema-inspect --connection-file PATH` checks the ledger
under its shared migration lock. Neither command takes a profile: the
namespace and epoch are deployment-wide.

Both commands use the same protected bootstrap, verified TLS/SCRAM, fresh
bounded DNS lookup and shared ten-second network deadline as the probe.
They try another resolved address only on a definite transport refusal before
COMMIT; the schema worker converts every connection loss after COMMIT may
have been sent to Indeterminate. No schema error, migration conflict or
uncertain commit is retried. Exit codes remain zero for success, one for
failure and two for invalid arguments. Provider text and supplied arguments
never enter the fixed JSON diagnostics.

Success returns schema_version and store_epoch with ledger_checked true,
serving_schema_checked false and profile_checked false. An
indeterminate-do-not-retry result means the original backend may still commit;
wait for that operation to settle and inspect database state before another
explicit initialization. An immediate missing-ledger inspection is not proof
of rollback. A failed stdout write can also lose a successful receipt; do
not interpret a missing command response as permission to replay.

These commands qualify only the preparation ledger, not complete schema
integrity or operational mail service. Future application schema migrations
and profile publication remain separate explicit operations.
