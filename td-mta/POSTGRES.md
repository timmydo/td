# PostgreSQL storage and concurrency contract

## Status and precedence

Serving and administration use PostgreSQL as the sole durable authority.
Receive, JMAP and relay profiles are active, with account-scoped authorization
and several cooperating processes sharing one writable database. A cloud VM
can receive and relay mail while a local process serves JMAP or relays through
a configured smart host. This remains a bounded personal service, not a general
multi-tenant platform. Peer-to-peer replication, an offline writable mail store
and read-replica routing are outside its scope.

PostgreSQL stores message and attachment bytes, metadata, configuration,
device verifiers, TLS/ACME keys and smart-host credentials. The embedded
backend, local configuration authority and file-backed device registry are
removed. [STORAGE.md](STORAGE.md) owns local runtime and response scratch;
[CONFORMANCE.md](CONFORMANCE.md) owns implemented td-mail flows and evidence.

Database deployment targets include independently installed PostgreSQL,
Azure Database for PostgreSQL Flexible Server and Amazon RDS for PostgreSQL.
Provider qualification is separate from the native PostgreSQL fixtures;
Aurora and database proxies require their own qualification. See
[DEPLOYMENT.md](DEPLOYMENT.md) for provisioning and
[PRODUCTION.md](PRODUCTION.md) for the current host's operations.

The admitted driver is tokio-postgres 0.7.18 with default features disabled:
no Tokio runtime or network/time driver is selected. Its private closure
provides SCRAM crypto and string preparation; TLS remains the td-crypto
boundary. Serving and owner commands use this driver; the connection probe
in section 8 is a diagnostic, not an alternative backend. No server package
is admitted by the driver dependency; source-build rules still apply.

### Contract navigation

The sections below retain their existing numbers for code and document
references. Use these current entry points rather than treating implementation
sequence as a backlog:

| Concern | Contract |
| --- | --- |
| Database authority, schema and migration barrier | Sections 1–3 below |
| Account administration and identities | [Account administration workflow](#account-administration-workflow) |
| Account-scoped HTTP authentication | [HTTPS account selection](#https-account-selection) |
| Incoming mail across accounts | [Transactional incoming delivery](#transactional-incoming-delivery-across-accounts) |
| Relay scheduling and retained sessions | [Foreground relay scheduling](#foreground-relay-scheduling-across-accounts) |
| Durable list summaries | [Persistent common-header summaries](#persistent-common-header-summaries-schema-33) |
| MIME evidence and selective body verification | [Persistent MIME interpretation](#persistent-mime-interpretation-schema-34) |
| Whole-body audit and progress | [Scheduled integrity scrubbing](#scheduled-whole-body-integrity-scrubbing) |
| Backup, restore and deployment procedures | [Deployment](DEPLOYMENT.md) and [production operations](PRODUCTION.md) |

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
not required for the initial runtime integration. Resolve the configured writer endpoint
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
Incompatible upgrades require all instances to drain and stop; no mixed-version
deployment is promised initially. Serving database connections acquire a shared
session-level advisory lock on the fixed migration key before checking schema
compatibility and retain it until closure. Retained Body/Envelope validators
(sections 180 and 187) instead take the same key's shared transaction-level
lock on each turn, fully revalidate schema/epoch and Runtime access before
application reads, and acknowledge rollback before returning. The fixed HTTPS
authentication worker, HTTPS API read workers and foreground relay idle reader acquire the
session lock at connection admission,
qualifies the schema and runtime role once, and retains that lock until closure.
Their jobs still check epoch, maintenance and selected configuration;
authentication observes credentials and idle reads observe route ownership
in fresh snapshots, acknowledging rollback before reuse. Body/Envelope
validators retain their per-turn qualification and release their migration
locks between turns.
The migrator takes the conflicting exclusive lock with a finite
deadline and refuses while a serving session or validation turn holds a shared
lock. After migration, a returning old process must reacquire the appropriate
shared lock and validate the new schema before application SQL; stale pools
cannot bypass the barrier. This requires direct sessions, not
transaction-pooling proxies.

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
The one concurrent delivery limit per account/route pair is database-wide.
Route pauses, backoff and repair generations share that account/route scope.
Each foreground process has one outbound worker across its configured accounts;
different processes may deliver different accounts concurrently.

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
2. PostgreSQL bootstrap/schema/storage foundations with real local database
   tests.
3. Replace active configuration, credentials, body/metadata storage, runtime
   coordinators and operational commands with PostgreSQL. This service has
   no production deployment to preserve: replacement may span independently
   tested commits and need not be an atomic landing. Remove SQLite code,
   dependencies/pins, private device files, exclusive-store assumptions and
   obsolete SQLite qualification as their consumers are replaced. Do not
   introduce a selectable production dual backend or migrate existing data.
   Update the routed contracts to describe each implemented replacement.
   Prior references below to an runtime integration do not require preserving the
   SQLite executable or completing all operational tooling before wiring
   PostgreSQL serving.
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

The private async adapter belongs to one worker. Reads during driver polls
remain nonblocking: the pinned driver polls reads before flushing writes, so
blocking there could prevent the request from reaching PostgreSQL. Socket
writes block only under the current operation deadline. After a complete
poll without socket progress or a driver channel wakeup, the worker waits
for socket readability with a deadline-bounded peek that consumes no bytes.
The adapter restores nonblocking mode before the next driver poll. Idle
health checks never wait for readability. One private synchronized owner
shares this socket between the adapter and its worker; no cloned socket,
Tokio runtime, network/time driver, background task or concurrent query is
introduced. Connect, TLS handshake, each retained job and teardown use their
own existing deadline, without renewal. Cleanup waits at most its existing
100-ms allowance. All loop turns and final success check the original
deadline. Native crypto work remains subject to TLS.md's between-call
deadline limitations.

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
timeout and a one-second lock timeout. Worker teardown releases the client
and drives pending responses, PostgreSQL Terminate and TLS shutdown for at
most 100 milliseconds within the original deadline. Failed or expired
connections still abort; cleanup never changes a known operation outcome or
retries a write. Terminate ends any remaining read transaction without a
separate SQL round trip. The probe writes no mail or configuration. This does not validate a schema, enforce a
supported-version range, or establish replication/failover durability.
Provider diagnostics and credentials never enter the fixed error enum.
The worker drives `poll_message` and drops asynchronous notices without
invoking the driver Future implementation, which logs raw server text.

The ignored `postgres_probe` integration fixture requires an independently
installed PostgreSQL 17 on `PATH`: the directory of the first `initdb`
there, which must also hold `postgres`, `pg_ctl` and `psql`. No variable
names it. It initializes only a new private disposable cluster, creates
its own test certificate and password, and checks verified
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
runtime integration work.

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
and total 1 MiB input bounds, authentication checks and terminal budget
refusal are unchanged. Schema operations permit 128 KiB of post-TLS output,
including startup/authentication and statement names. Driver statement names
grow with a process-wide counter; schema initialization must still fit after
long owner audit runs. Probe output remains bounded to 64 KiB. Fixed SQL and typed
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
owner. Serving connections retain their own shared lock throughout their
lifetime and validate the complete serving schema, except for the closed
Body-validation turns specified in sections 2 and 180.

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
probe operations retain 64 KiB and schema operations permit 128 KiB.
Each operation owns a cold,
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

Schema version 3 adds explicit mail relations. Schema initialization, inspection,
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
adapter. The file registry is removed. Credential types and limits stay in
the private shared implementation; PostgreSQL exposes Creation::password()
and its own error type. Registry publication, duplicate-ID refusal, capacity,
revocation and current-policy locking belong to PostgreSQL transactions.

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
credential check. Sections 193 through 195 add explicit registry inspection,
revocation and creation commands. Runtime HTTP authentication and admitted
mail snapshots/transactions use this database registry; there is no local
file credential authority.

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
foreground service composes this adapter into its database profile loader
before admitting listeners. Database version references are the runtime
material source; the old file-configuration serving path is removed.

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
or native-allocation qualification. Runtime privileges and service activation are enforced by the runtime
boundaries described below.

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
one, and the receipt. No existing
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

Section 196 exposes initialization and retained receipt inspection as
explicit administrative commands.

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
Configuration/profile admission and runtime credentials remain runtime integration work.

The result owns at most 256 mailbox rows, in binary ID order, with account
sequence/floor and total/unread Email and Thread counts from the same snapshot.
Unread excludes both seen and draft Emails; a Thread is unread when at least
one member in that mailbox is unread. A closed query limits returned rows to
257 to detect overflow without returning a partial listing. The mailbox-only
transport allows 512 frames within the ordinary one MiB input and 128 KiB
individual-frame ceilings; other operations retain their previous budgets.
Counts are grouped across memberships of the bounded selected mailbox set,
with account-scoped Email, Thread and keyword joins. Empty mailboxes retain
zero counts. This avoids requiring a separate aggregate execution for each
mailbox without retaining per-email rows in the caller. Database aggregate
work is subject to the statement and whole-operation limits, not a promise
of bounded server scan cardinality.

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
and bounded runtime pools are still runtime integration work.

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

`with_interpreted_body` and the qualified `with_projected_body` path, whose
callbacks build provisional output and cannot publish or admit new work,
group connection-health polling across
resident pin checks and cached body reads. They share one counter: a full
driver check occurs at least every 64 checkpoints. Every checkpoint still
checks sticky failure and the original body/worker deadlines immediately.
Cache misses check connection health before and after SQL; verification and
callback completion retain unconditional checks. An unknown disconnect may
therefore permit bounded provisional work, but never successful publication.
Ordinary verified pins, downloads and relay sources retain unconditional
connection checks on every read.

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
administration activation. Section 197 supplies a fixed public encoding
for retaining the same change across process restart; callers must persist
it before publication. Encoding support does not activate administration.

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
binding; diagnostic formatting redacts the visible catalog. A relay binding
also retains the process-local relay policy identity from this exact prepared
TLS generation (section 183); it contains no credentials or TLS lease.

Before returning, a final primary observation rejects a selection already
superseded during preparation. This does not freeze future changes: runtime
startup/publication must still fence policy afresh and admit only implemented
roles. The original at-most-ten-second deadline covers source verification,
repeated profile preparation, account inspection and final observation.
The source candidate is discarded before the final check; retained text and
TLS objects belong to the caller's cold control worker. Reusable connection
pools, publication, live refresh and full resource qualification remain
runtime integration work.


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
mapping remain runtime integration work. All calls retain finite worker/deadline limits.
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
Production JMAP dispatch and serving retention remain runtime integration work; this API
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
reclamation or serving. Those remain prerequisites for the runtime integration.

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
submission, runtime admission and the runtime integration remain work.


## 38. Authenticated change-history pages

`postgres_changes` reads detached Mailbox/Thread/Email/Submission change
pages under one authenticated primary repeatable-read snapshot. It checks
the selected JMAP profile generation/version, epoch and current device,
then binds the account sequence and retained history floor. State tokens
convey no authority. Wrong account/epoch, future or pruned state refuses.

The existing `jmap_changes` transaction-aligned merger now accepts a small
history-source interface. PostgreSQL seeks the native account/kind/sequence/operation index
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
admission remain part of the runtime integration.


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

Email/get prefetches passive metadata in windows of at most sixteen requested
IDs using one typed, account-scoped query. Its explicit seventeen-row ceiling
and strict returned-ID ordering reject duplicates or unexpected rows. Every
returned row passes the same manifest, chunk-count, referenced-thread and
sequence validator as a single lookup, and must match the exact snapshot
identity. A fixed window retains requested IDs and explicit absent results;
only complete successful validation replaces the old window. Request order,
duplicate handling and notFound assembly remain in the JMAP layer.

The snapshot also retains its existing last successful metadata record for
unprefetched single reads. Both caches still check the original connection,
deadline and sticky failure on every access and cannot cross a snapshot or
method reopening. Prefetching does not authorize body output: body callbacks
still verify the same snapshot's complete bytes. For selected mailboxIds and
keywords, two additional typed, account-scoped batch queries prefetch bounded
prefixes for the same IDs. Each lateral subquery aggregates at most nine
values and their validation fields into one protocol row. This avoids
spending one protocol frame per value and then repeating it during fallback.
Sets shorter than nine are complete; a nine-value prefix is conservatively
marked has_more. Cached reads can return a short page, and requests at or
beyond its tail continue through the existing paginated reader. No returned
prefix value is reread. Unselected sets issue no query. Every retained value,
reference and change sequence receives the same validation as an individual
read; unknown IDs, duplicate/unsorted values, unequal arrays and excess rows
fail the snapshot. Empty sets are explicit and do not prove Email existence.
Cached pages preserve exclusive-after ordering and has_more, and retain the
same connection, deadline and sticky-error checks. The value window is cleared
when metadata is replaced and never crosses a snapshot. The nine-value and
sixteen-ID limits bound only the optimization, not mailbox capabilities. Metadata, membership, keyword and chunk
reads send fixed parameter types with the query, avoiding a separate
prepare/describe exchange without changing row validation or limits.

Every method and final callback completion checks the original connection and
deadline. A failed operation retires the whole view, including when its caller
swallows the error. Missing Email metadata remains a normal absent result.
Concurrent writes or retirement cannot splice new metadata or body bytes into
an admitted read. Current profile/device authority is checked at admission;
revocation does not extend or invalidate an already admitted finite snapshot.
The existing read-worker byte/frame caps apply across all methods and bodies,
without replenishment. This snapshot grants no independent listener or publication authority.

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
whole view. The PostgreSQL Email/query dispatcher uses this read boundary.

## 41. Snapshot-bound thread member windows

`postgres_read::Snapshot::thread_emails` reads one thread's members in
ascending received-date and Email-ID order, strictly after an optional
coordinate. Shared passive `ports::ThreadEmail` and `ThreadEmails` values
carry no storage authority. The window ceiling remains 256 members plus one validated overflow row.

An absent stored thread returns `None`; an empty stored thread returns an
empty window, leaving the existing JMAP rule that hides empty threads to the
protocol adapter. Thread and returned member change sequences must not exceed
the snapshot. All reads, including repeated windows after concurrent deletion,
retain the same account state, worker budget and deadline. Invalid limits or
other read errors retire the view. This supplies storage for td-mail's
Thread/get without activating PostgreSQL production dispatch.

## 42. Retained root-folder creation

Schema version 12 adds `mailbox_create_receipts`. The
`postgres_mailbox_set` adapter prepares at most 256 root-folder names,
using the shared bounded NFC/control/noncharacter validator. Invalid names become the existing empty-name marker before
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
expiry/reclamation remains part of runtime integration work. Folder transactions have a
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
operation receipts remains runtime integration work.

## 45. Shared receiving header preparation

The storage-independent `receiving_headers` module owns the existing bounded
root-header retention, Return-Path removal, supplied trace insertion and
Message-ID/References/In-Reply-To selection used by receiving. Its
64 KiB output turns, source bounds, deadline checks and original work-meter
charges are unchanged. The module grants no recipient, gateway or publication
authority and performs no database access. PostgreSQL receiving owns publication and runtime authorization.

## 46. Retained SMTP receiving publication

Schema version 13 adds `receiving_receipts`. The owner-side `postgres_receiving`
adapter prepares completed decoded DATA outside its write transaction. The
trusted connection owner supplies envelope/socket facts and must already have
admitted the peer, gateway and recipient routes under the selected profile.
Passive receipt facts confer no authority. The receiving service supplies this trusted connection owner.

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
ingress reservations, retention/reclamation and activation remain runtime integration work.

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
ownership and activation remain runtime integration work.

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

This path preserves near-expiry/overdue responsibility using the QUEUE.md
expiry-wait marker and retires the active attempt without declaring terminal
failure. Clock-confirmed expiry remains a
separate activation prerequisite. A backwards last-attempt timestamp refuses.
Runtime clock discontinuity validation and the five-minute monotonic restart
guard still precede any actual retry. Receipt storage has a 1024-per-account
cap. No SMTP, scheduler or retry dispatcher is activated by this worker.

## 55. Runtime database credential preparation

`postgres runtime-check --connection-file PATH` checks the current schema and
one dedicated runtime login's effective data privileges. It closes the
connection before returning and neither grants privileges nor activates a
profile. Startup/reconnect integration remains runtime integration work; a successful
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
can grant the current schema privileges atomically as follows:

```sql
BEGIN;
GRANT USAGE ON SCHEMA td_mta TO td_mta_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA td_mta
    TO td_mta_runtime;
REVOKE INSERT, UPDATE, DELETE ON
    td_mta.schema_migrations, td_mta.runtime_barrier,
    td_mta.config_materials,
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
retain their separate credential. Drain/stop serving instances before changing
their runtime role or privilege contract; reconnect requalifies it. Before a
schema upgrade, drain/stop all
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
cold receipt never substitutes for them. Retained HTTPS authentication and foreground relay idle readers hold
that qualification and migration lock for their finite connection lifetimes,
with fresh job fences as specified under Retained HTTPS authentication
sessions. Other operation-owned connections hold the lock through their
operation. BodyChecks instead authenticates at open and performs
this qualification inside every closed validation transaction (sections 2 and
180), before any Body queries. No prior turn supplies cached qualification.
Mail helpers still enforce their own epoch, current configuration, device,
snapshot and publication fences.

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

`http_auth::parse_basic` separates bounded HTTP Basic decoding from storage
authentication. It retains the exact visible-ASCII username (1..254 bytes,
excluding colon) and at most 97 password bytes. Its owned
`BasicCredentials` retains at most 352 decoded bytes, has no Debug or Clone
implementation, and clears its decoded buffer on drop. It contains presented
bytes only, not a device ID, cached verifier result or authorization grant.
`postgres_http_auth::Authentication` matches the username case-sensitively
against a bounded registry of at most 16 account bindings, then checks the
password using that account's fresh primary snapshot. It returns a principal
containing both AccountId and DeviceId; a device ID alone cannot identify an
account because device IDs are unique only within their account.

At PostgreSQL runtime integration the bounded HTTPS request worker may retain this owner
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
Service admission remains part of the runtime integration.

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

The shared `store_fs::PinnedBlob` borrows a verified PostgreSQL chunk reader.
Root-header, MIME structure, body-value and attachment readers retain that
borrow through their original request scope. There is no local-database
variant, body copy, renewed work meter or renewed deadline. Unit MIME tests
use a bounded memory implementation of the same verified-source interface.

The real read fixture collects root headers through the existing MIME
reader after another connection deletes the source Email/body. It checks
pin identity/digest, original body bytes, sticky expiry even when swallowed
by the callback, and refusal to grant a pin for corrupt bytes. The shared MIME fixtures use bounded memory sources; real PostgreSQL
fixtures separately qualify database custody.

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
service. PostgreSQL protocol/service activation remains the runtime integration.

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

The active PostgreSQL backend authenticates at this operation boundary. The
HTTPS framing fixture checks the presented password against the observed
device on the body worker for all three operations, including second-device
requests; PostgreSQL fixtures separately prove database revocation fences.

## 65. PostgreSQL HTTPS authentication adapter

`postgres_http_auth::Authentication` implements the fixed HTTPS worker's
credential interface. Its bounded registry selects an account binding, and
fresh snapshots check configuration, store epoch, maintenance and device.
The current connection lifecycle, connect-time qualification, idle recovery
and finite reuse bounds are specified under Retained HTTPS authentication
sessions. DNS runs on connection admission rather than on every lookup.
Later operations require their own fresh authorization and publication fence.

One nonblocking mutex bounds concurrent lookup, including DNS; contention
returns Busy without a queue. The original HTTPS deadline converts through
the same RuntimeClock origin without a fresh allowance. A ten-second lookup
ceiling and five-second DNS ceiling only shorten it. Results are refused
after expiry. Malformed and inactive/wrong credentials return no principal;
configuration, schema, privilege and connection failures remain service
errors, not invalid-password observations. Errors carry no endpoint, secret
or provider text. The native fixture checks these distinctions, revocation,
lock contention, deadlines and subsequent successful lookup.

## 66. Response retention without a mail database

`ResponseCoordinator` owns one cold, empty RequestSpool and a process-local
response quota ledger. Construction consumes the spool and refuses occupied
or retired slots; it opens no mail database and grants no mail authority.
Its only reservation surface is response bytes. PostgreSQL remains
responsible for durable quotas; the local ledger cannot reserve bodies,
uploads or delivery responsibility.

Retention reserves the full cap before file creation, preserves original
work/deadline accounting and prepaid mutation output, and unlinks before
returning quota credit. Failed cleanup retains the full charge and retires
the slot; ledger failure stops admission. Legacy native-view and external-sort
hooks are removed. PostgreSQL method adapters supply their authenticated
snapshots and bounded queries.

The active service supplies this coordinator to the dispatcher. Fixtures
cover complete echo output, quota saturation, prepaid output, successful
cleanup, replacement-file uncertainty and sticky clock failure.

## 67. Prepared PostgreSQL Mailbox/get dispatch

`dispatch::run_with_postgres_read` accepts a freshly authenticated runtime
snapshot and a response retention owner. Core/echo and connected Mail methods
use the existing whole-request admission, argument parser, result references,
method-error handling and retained response encoder. Unconnected methods return
unknownMethod at this preparation boundary; they never fall through to the
local database. The active service uses this backend.

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
the active receive/JMAP service constructs these owners.

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

## 73. HTTP read backend

`postgres_http_api::Service` connects the fixed HTTPS body-worker interface
to the shared bounded HTTP framing and PostgreSQL read dispatcher. Its Session, profile binding and
resource plan must agree; HTTPS, response retention and dispatch share one
RuntimeClock. Read dispatch includes Core, mailbox, email, thread and change
methods. Uploads and mutations use the separate publication adapters below;
their write connections are not retained by the read cache.

Original credentials stay borrowed through body preparation and fresh primary
snapshot authentication. The observed device ID must match their syntax, but
only the fresh snapshot grants read authority. Each fixed HTTPS body worker
retains at most one read connection for its last account and configured origin, holding one existing
read permit. Connection admission performs DNS, schema and runtime-role
qualification and retains the shared migration lock. Each job begins a fresh
read-only repeatable-read transaction, checks epoch, maintenance, selected
configuration and the complete presented credential, and acknowledges ROLLBACK
before output. Only the clean connection and its permit survive; no snapshot,
body pin or authority does. The standalone backend entry still closes its
connection after the job.

API read sessions admit at most eight jobs within sixty seconds, each under
the original ten-second execution deadline. Per-job wire allowances remain
256 MiB input, 512 KiB output and 8192 frames; finite connection totals are
eight times those limits, with no parser/authentication reset. These are wire
budgets, not resident allocations. Owning-thread maintenance retires exhausted
or near-expiry sessions between jobs and during bounded HTTP input/output
turns. Slow progressing peers cannot retain an expired idle database session.
The local hook issues no SQL; active database jobs keep their original finite
deadline and release their transaction before response output. Account or origin changes, uploads,
downloads and mutations discard the retained reader before acquiring other
storage capacity. Observable idle disconnects reconnect before job submission;
failures after submission retire the connection without replaying the job.
Worker exit closes retained resources on the same thread before releasing
their read permits. Account count does not multiply retained sessions.

Final snapshot validation and rollback precede response output; temporary
response quota remains charged through output and cleanup. No database work runs on the
network reactor, no success escapes a failed snapshot, and uncertain response
cleanup stops admission.

Shared HTTP framing keeps the execution deadline strictly before the outer
response deadline, including short original request allowances. It caps
execution one clock tick before the outer limit without extending either
budget. This preserves response retention's phase-ordering contract; the
one-tick separation does not promise transmission time after expiry.
### Worker-local verified header projections

Each fixed HTTPS body worker may retain serialized root-header members for
header-only Email/get selections, separately from its finite database session.
The cache survives connection rotation, API mutations and API/download switches;
account/origin replacement, uploads and worker teardown drop it. Its immutable
Service binding fixes profile, configuration and interpretation limits.

Every lookup follows fresh transaction authentication and current Email and
blob metadata validation. The key contains account, epoch, blob ID, length,
full SHA-256 digest and the exact ordered header property spellings, including
modifiers and case. Email IDs, thread IDs, flags, memberships, dates and other
stored fields are never served from this cache. Removed Email objects remain
notFound; cached headers grant neither account access nor body-pin authority.

A hit reuses historical interpretation of the same immutable blob. It does not
re-read chunks or detect subsequent corruption of an unrequested tail. Full
body projections, previews, searches, raw downloads and attachment downloads
still verify current requested bytes. Qualified MIME projections may use
chunk verification as specified below; searches and downloads retain full
verification. Header-cache misses preserve whole-body precharge and verification. Hits charge bounded lookup and copy/output work
and check the original snapshot, retention and method deadlines. Cache output
remains provisional until normal final snapshot validation and response
publication.

Only a successful projection whose outer pin verification also completes may
stage an entry. Staged entries are invisible to lookups until the enclosing
read transaction completes acknowledged rollback and final checks. Failure
or unwinding discards pending entries. No transaction, credential, snapshot or
body pin is retained. Earlier independently verified projections can survive
an ordinary later per-method JMAP refusal if the enclosing read job succeeds.

The configured index_cache_bytes budget is divided across the actual bounded
HTTPS body-worker count. Each share includes entry-vector capacity, exact
property signatures, resident and staged fragments, active capture, and old
plus replacement allocation during capture growth. At most 256 entries and
64 KiB per fragment are retained. FIFO eviction, allocation failure, oversized
fragments or a tiny configured budget merely bypass caching. The existing
memory reservation is unchanged; this is a bounded optional optimization,
not a persistent database index or a latency guarantee.

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
this increment. The active receive/JMAP service uses the PostgreSQL backend.
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
receipt reclamation remain runtime integration requirements.

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
updates. Section 82 connects root-folder creation. Rename and
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

A dedicated bounded owner retains one exact Change through uncertain COMMIT.
It blocks further folder creation or deletion without stopping reads or independent Email
writes. Receipt settlement uses separately admitted capacity and never retries
creation with a new identity. Absence and errors preserve the owner; confirmed
outcomes survive local expiry. The encompassing prepared mutation owner keeps
both operation families across requests. Restart persistence, settlement
scheduling and receipt reclamation remain required before runtime integration.

Mailbox completion exposes only the original epoch and sequence, without a
fabricated snapshot identity. The real td-mail fixture creates Archive, rejects
its duplicate, lists it and moves mail through it. A delayed creation COMMIT
proves exact receipt settlement and one durable folder. Hierarchy creation and renaming remain outside this td-mail milestone.
Singleton deletion uses the same owner as described below.


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
qualification and durable restart ownership remain runtime integration work.

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
Account, epoch, selected configuration
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
recipient status projection or PostgreSQL service activation is activated.

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

The same snapshot filters both the optional total and ordered page, preserving
account isolation, receivedAt/EmailId ordering, lookahead and the original
SQL deadline. The date decoder is shared with submission queries, including
its numeric-position checks. This is an inert PostgreSQL read preparation;
it does not enable Submission or the runtime integration.

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

The PostgreSQL HTTP adapter can admit the Submission capability and
routes a singleton EmailSubmission/set or td-mail's draft-and-submit batch
to the prepared mutation dispatcher. It releases its fresh authenticated
snapshot before mutations, retains the original response owner and deadline,
and uses the shared pending owners for draft, submission and filing. Read
requests can use Identity/get and submission reconciliation through the same
admission path. The active receive/JMAP service does not yet advertise
Submission; the explicit real-client sending fixture qualifies this adapter.

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
relay, attachments, cloud-provider failover or runtime integration is claimed.


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
available. Attachment composition and complete recovery qualification remain
separate requirements.


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
restart custody, maintenance and runtime integration are not activated.
The PostgreSQL runtime-role fixture covers known success, rollback, delayed
COMMIT, refusal while pending, historical settlement, absent-receipt retention
and stale-policy refusal.


## 105. SMTP worker storage boundary

SMTP receiving accesses storage through `smtp_storage`. The fixed worker
admits a job from the trusted connection's session, account, configuration
generation and observed peer/TLS facts, and performs preparation and
publication. The network loop copies one decoded DATA line into admitted
memory per connection turn, under the same slot ownership and original
deadline. `Job::write` is bounded and performs no database, filesystem or
peer I/O; it does not wait on another slot's publication. Routing, TLS,
retry, fail-stop and shutdown behavior retain their existing fences. A known durable result remains
separate from cleanup/accounting stop flags; uncertain publication still
retires the active service under the existing production policy.

The foreground service constructs the PostgreSQL adapter behind this
interface. Its readiness, ingress capacity, authorization fences, contention
mappings and completion behavior are exercised by the PostgreSQL worker and
executable fixtures. The old local storage adapter is removed.


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
generation and configured message/header/recipient bounds, takes one of four
ingress permits and observes fresh receiving readiness before admitting DATA.
The permit and resident buffer survive worker turns without a database transaction
or mutex guard spanning peer input. Dropping the job releases that capacity.
The shared bound is independent of listener and account counts. Exhaustion
retries before 354 under the original queued admission deadline. A sender
waiting in DATA does not hold another sender's permit or the storage worker.

The original absolute deadline and profile work allowance cover bounded DATA
copies, receipt metadata, preparation and publication. Admission prepays one
incoming buffer of at most 32 MiB per admitted job; preparation may retain one
additional filtered body under the same meter. The single storage worker and
publication owner serialize preparation and publication. Four full-capacity
incoming buffers plus that filtered body have a 160-MiB payload ceiling;
allocator capacity and native protocol buffers are additional. This finite
bound is not qualification of the provisional 128-MiB workload RSS goal.
Each decoded line is at most 1000 bytes. The network loop performs its
bounded memory copy and refuses further copies after service failure. A
failed write remains failed, and publication can be attempted only once.
The trusted connection owner still supplies routing and observed peer/TLS
facts; no gateway authority is inferred from message content.

Known publication yields the account, epoch and sequence required for SMTP's
final success reply. Unknown COMMIT retains the original operation in the
publication owner, stops this backend and produces no final SMTP receipt.
Already admitted jobs refuse further writes, preparation and publication;
the receiving worker retires every uncommitted sibling without publication.
Historical settlement does not automatically restart this backend. The
foreground service constructs this PostgreSQL storage adapter; durable restart
custody for uncertain operations remains a separate requirement. The runtime
PostgreSQL fixture drives the SMTP session through admission, DATA and commit,
including discarded ingress, sticky refusal, generation/clock mismatch,
four-job mixed-peer ingress capacity, two-job same-peer capacity,
release after refusal/discard, success, delayed
COMMIT, fencing of admitted siblings and historical settlement.

## 108. Real SMTP worker qualification

The explicit PostgreSQL fixture runs the production SMTP receiving worker and
prepared PostgreSQL storage adapter together on an ephemeral TCP listener.
Its database-loaded profile supplies routing, resource limits and retained
TLS policy. The runtime database role supplies readiness and publication.
The fixture checks the SMTP greeting, envelope admission, DATA and final
success reply against durable receiving receipts and Email rows. A known
database rollback produces 451, preserves the ready worker and permits a
later successful transaction on that connection. A partial DATA sender stays
paused until another connection has received final success, with exactly that
second delivery durable before the paused sender completes. Shutdown discards
two unfinished same-peer DATA jobs, joins the workers and leaves no additional message
or receipt. A third same-peer DATA attempt receives 451 without 354 while
the two unfinished jobs remain admitted. These concurrency regressions run
over plaintext and STARTTLS. Another
regression holds one admitted job immediately before publication and proves
that a second job consumes its DATA line before that gate is released. No
receipt exists for either message until publication proceeds; both final
successes must correspond to durable receipts.

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
A retry that cannot precede expiry records expires_at as the nonterminal
expiry-wait marker for the separate clock-confirmed expiry path. Permanent refusals terminate retries, retaining
OutcomeUnknown for prior uncertainty and Failed otherwise; a genuine refusal
can be retained after expiry. Result rows, submission completion and pending
failure notice, attempt retirement, history and receipt commit together.
Historical replay preserves the original random retry times.

The runtime-role fixture exercises ordering, exact subsets, maximum replies,
expiry-wait marker, rollback, delayed COMMIT settlement, conflicting replay,
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
already settled RCPT recipients are excluded. Bounded jitter, expiry-wait marker,
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
the single active attempt per account/route keeps that ordering across new dispatch.

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
custody and runtime integration remain separate work.

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
Production scheduler wiring and runtime integration remain separate work.

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
The retained positive RCPT text must match each selected recipient's stored
reply through the SHA-256 witness specified in section 179; data replies are
absent and changed sequences equal the Body receipt.
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

Each current-state check independently prepays the Body-check allowance in
section 179, acquires an ordinary slot, checks Pending and uses the native live
Body reader. The shared
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
live Body state through the reserved plan. Section 181's retained owner
prepays its finite check allowance and holds a second ordinary slot while
final capacity stays held; individual turns spend prepaid credits. The returned
lease/submission bound only shortens its local cap. Original Instant limits
are checked before and after every transfer turn. Refusal closes
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
throughput qualification. Full live checks reread selected reply witnesses
per write/flush through the prepaid retained connection in section 181. Their
cumulative cost must fit its finite allowance and the original deadline.
Large-body throughput requires explicit qualification before activation.

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
owner used by dispatch, phase and route-failure publication (section 184).
All new dispatch, phase, recovery, route-failure and final-capacity work
stays blocked. Settlement prepays fresh work
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

## 172. Bound native MAIL exchange

postgres_relay_mail binds a reserved MailPlan to one authenticated relay
Session for the duration of MAIL. Before command construction it requires the
session's policy identity to match the TLS generation prepared with the
attempt's private selected-profile binding (section 183). The adapter selects
the exact admitted reverse path, runtime clock and original deadline. SevenBit
frames MAIL FROM; EightBit frames
BODY=8BITMIME only when the session retained the negotiated capability. It
borrows the caller's original wire and database meters through the exchange.

Every write and flush turn first rechecks the exact untouched Prepared envelope
through the original owner. Its local cap may only shorten. Database locks
never span transport calls. Direct low-level plans close each observation
connection; configured attempts retain the admitted validator in section 188.
Each turn uses the existing bounded
command parser and wire budget; before/after transport checks remain in force.
The pause/takeover and already-buffered-byte boundary in section 4 is unchanged.

An actual normalized 2xx reply returns the same session, original envelope and
MAIL mode together. Only the internal RCPT adapter can split this owner;
external callers continue through the typed RCPT entry or close transport.
Fresh checks remain required before each command.
An actual 4xx/5xx reply enters the preallocated negative MAIL publication before
any further database admission. Known failure retains exact CommandFailure;
unknown COMMIT also stays in Pending. Rejection, protocol/admission failure and
publication failure close transport and return its buffers. Other reply classes
refuse without invented evidence. No returned durable receipt is a live owner.

The real PostgreSQL/TLS fixture exercises exact sender framing, positive MAIL,
durable negative MAIL, negative-publication rollback and retained retry, occupied
connection slots and changed envelope identity before output, expired local custody, unadvertised EightBit,
and a non-final reply class. Interrupted Prepared attempts are recovered only
after transport closes. This adapter does not activate production dispatch or
claim complete envelope orchestration or maximum-message throughput.

## 173. Ordered RCPT exchange from successful MAIL

postgres_relay_rcpt begins only from the bound MAIL Accepted owner. It reserves
all maximum reply cells before the first RCPT, derives each address and ordinal
from the admitted batch, and keeps the same session and MAIL mode. Every command
borrows fresh scratch cells and the caller's wire/database meters. A returned
Cursor permits only the next exact recipient; no separate session, recipient
address or replacement identity enters this path. Original clock/deadline and
the shortest admitted local cap survive between commands.

Every write/flush turn rechecks the untouched Prepared envelope before output.
No negative RCPT publication occurs while collection continues. Complete actual
2xx/4xx/5xx replies returned by the command parser enter reserved ordered storage before checking whether the
local cap permits continuation. A wire, current-state, deadline or parser
failure closes transport and finishes the earlier collection into passive
RcptResult evidence; a finish-time crypto/integrity error is reported separately
instead of inventing evidence. Explicit Cursor/Exchange closure follows the
same fence-before-interruption order. A failed initial reservation closes MAIL
transport without inventing any RCPT reply.

An actual 421 is retained and closes immediately, even on the last RCPT
after earlier positive replies; it never returns another command or DATA
owner (RFC 5321 sections 3.8 and 4.2.2). Complete all-negative collection
closes automatically and returns passive
results for ordinary negative publication, with no DATA session. A complete
batch with positives retains its exact RcptResult, same session and mode; the
internal DATA stage must reserve its alternatives and check authority before
output. Its split is crate-private; external callers use the typed DATA
continuation. Completed.close fences and retains evidence for passive interruption. Fresh passive settlement must
publish collected negatives before recovery through the same Pending owner.
Neither receipt nor collected reply evidence reconstructs output authority.

Native PostgreSQL/TLS cases cover mixed and all-negative batches, closure after
a positive or negative first reply, invalid next reply, occupied slots and
changed envelope identity before the next command, and original cap expiry
between commands, plus intermediate/final 421 shutdown. They verify exact
commands, no further output on admission
refusal, retained reply texts and fenced recovery. Production dispatch, DATA
orchestration and maximum-message throughput remain separate work.

## 174. Bound DATA exchange after complete ordered RCPT

postgres_relay_data begins only from postgres_relay_rcpt::Completed, preserving
the same authenticated session and MAIL mode. It reserves the bound DATA
alternatives before command output and keeps the original runtime clock,
deadline and shrinking cap. Each write/flush turn rechecks the untouched
Prepared envelope; preceding negative RCPT replies remain unpublished until
actual DATA completion. Existing bounded parser and work-meter checks apply.

An actual complete 354 returned by the command parser enters ordered negative
RCPT publication and new observed Body publication through RcptDataPlan. Only
known new observed Body before the retained cap returns BodyResult, matching
Dispatch, same session and mode. Historical receipts or uncertain commits never
construct this handoff. Its consuming reserve_body operation binds final
capacity before BodyReady begins transfer with the same session and MAIL mode.
BodyReady binds matching immutable source admission as specified in section 182.
No body bytes or terminator are sent by this command adapter.

An actual negative DATA reply fences the socket before settlement while keeping
the parser's existing normalized reply storage readable. A crate-private abort
on command Reply provides this ordering without a reply copy. Ordered refusal
publication retains the exact RcptCommandFailure on failure. Other reply classes,
wire failure, stale authority, deadline or capacity refusal close transport and
retain the bound RCPT plan as passive RcptBodyFailure for interruption planning.
Initial reservation failure closes and preserves RcptFailure. No refusal returns
a live session; unknown COMMIT remains in Pending, and passive settlement uses
its existing ordered publication and independent inspection rules.

Native PostgreSQL/TLS fixtures drive actual bound MAIL and mixed RCPT into DATA.
They cover known new Body with matching final-capacity reservation, permanent
and 421 temporary DATA rejection, rollback at negative RCPT/Body/DATA publication,
slot refusal before DATA output, and an invalid positive DATA status. Each path
verifies exact reply persistence and retirement after transport closure. Shared
RCPT fixture drivers are used by both adapters. Full body/final transfer remains
separately qualified by its existing native fixture; production activation and
maximum-message throughput are not claimed here.

## 175. Bound envelope through immutable body and final settlement

The native final-handoff fixture now drives the successful transfer, final
publication rollback, expired final custody, and unknown final COMMIT cases
through the bound MAIL, ordered RCPT and DATA adapters. The same authenticated
session passes through their typed completions into the existing immutable
body reader and body/final transport adapters. The actual DATA completion's
MAIL mode is carried into body encoding. Exact wire commands and positive
replies are checked before body transfer; matching attempt identity, the
shrinking Prepared cap, final-capacity admission, Possible before terminator,
and exact final settlement remain asserted by the existing fixture.

These four cases cover the composed small-message path, including passive
final evidence extraction and independent unknown-commit inspection. Existing
negative-envelope and interrupted-publication cases retain their focused
low-level paths. This test composition adds no production dispatcher, cached
authority, output-owner reconstruction, allocation policy or deadline change.
The maximum-message throughput limitation in section 155 still applies.

## 176. Passive complete reply evidence on post-read refusal

A command turn that fully parses and finishes a normalized reply before its
post-operation clock or meter check fails still refuses the operation. The
command Refusal closes transport and releases native TLS ownership first,
then retains a borrowed view of the existing normalized output cell as passive
evidence.
Its reply accessor supplies the actual code and text; it has no session or
continuation operation. Incomplete, malformed, truncated and trailing-byte
input cannot populate that evidence, and pre-operation or write/flush failures
retain no invented reply. Existing pre/post checks and bounded work charges
remain in force.

Bound MAIL and DATA adapters capture complete negative replies from this
closed refusal through their existing retained publication owners. Ordered
RCPT captures any complete valid recipient result before returning fenced
interruption custody. The final adapter records complete 2xx/4xx/5xx evidence
through FinalResult. Publication still uses the original allowance and deadline;
expired or otherwise refused publication preserves its existing failure owner,
and unknown COMMIT stays in Pending. Positive MAIL and DATA replies from an
expired operation never construct a live envelope or Body handoff.

The paired TLS regression cases expire the command clock after the read that
finishes positive, negative and DATA-continuation replies, and distinguish
partial and trailing-byte input. This extends evidence retention at the parser
boundary without granting output authority or changing production activation.

## 177. DATA custody through final reservation and body start

Completed no longer splits live Body, session and MAIL mode into independent
parts. Its reserve_body operation reserves final capacity against that exact
BodyResult and returns BodyReady with the same session and mode. A refused
reservation closes transport and returns the existing body-transfer Refusal;
it grants no body or terminator output. Ordinary interrupted-Body recovery still
owns durable retirement after that closure.

BodyReady exposes the matching passive Dispatch for immutable body admission
and the existing live phase check. Its consuming with_source verifies the exact
Dispatch and supplies a callback-scoped BodySourceReady; that owner's begin
passes its private BodyPlan, session, mode and verified source to the adapter. Original shrinking
limits, per-write checks, Possible publication before terminator and final
evidence custody remain unchanged. Explicit buffer extraction closes the
session and drops local reservation custody. No historical receipt constructs
either live owner.

The native composed success, final rollback, expiry and unknown-COMMIT cases
use this handoff. A new mixed-RCPT case occupies both local slots after DATA,
then verifies failed final reservation closes transport without body output,
preserves Body state and permits fenced recovery. Section 182 binds the
immutable-source boundary and its separate connection/body-work admission.
The maximum-message throughput limitation in section 155 remains explicit.
Production dispatch remains inactive.

## 178. TLS-sized body output cells

The bounded body adapter accumulates 16384 encoded bytes before a full-cell
write, matching the existing TLS plaintext ceiling. Its source cell remains
4096 bytes and each codec poll remains limited to 256 transitions. Exact
partial-write offsets, pre/post deadlines and work charges remain unchanged.
Every write attempt and flush still performs its existing fresh live Body
check. Full cells can require up to fourfold fewer successful writes than
4096-byte cells; Pending/partial writes and flushes prevent a universal
fourfold check-count claim.

Paired TLS tests cross CRLF over the input boundary and a dot-stuffed line
start over the output boundary, retaining a partial final cell. The native
composed relay fixture transfers a 64000-byte immutable body, checks exact
dot transparency and its sole terminator, and persists the actual final reply
under the original ten-second cap and existing meters. This is a bounded
larger-message qualification, not a maximum-32-MiB throughput claim. Full
per-write validation and evidence reads remain as in section 155; section 181
binds the checks to one prepaid connection.

## 179. Compact live Body reply witnesses

The live Body reader projects the SHA-256 of each RCPT reply's UTF-8 bytes and
whether DATA text is present. Each selected row must have a non-null 32-byte
digest equal to the digest of its retained, validated positive reply and no
DATA text. Equality now relies on SHA-256 collision resistance, consistent
with existing stored-body and operation-receipt digests. The original request
digest and receipt representation do not change. PostgreSQL's pg_catalog.sha256
and pg_catalog.convert_to supply the witness; no extension is required.

The query retains exact sorted subset membership, phase, publication receipt,
active attempt, encoded membership, sequence/floor, reason, retry, policy,
owner, health, queue-clock and expiry checks. Unselected Prepared rows retain
their existing handling. AcceptancePossible keeps its full positive reply
validation and final-capacity checks.

Standalone dispatch checks open fresh qualified connections; retained body
transfer uses section 181's prepaid profile. Body acknowledges transaction
rollback before returning (section 180); Possible releases locks by connection
closure. Standalone Body uses at most 1 MiB input, 1088 frames
and 64 KiB output, including authentication and runtime qualification. The
frame allowance is the same in preparation mode. Dispatch prepays 1 MiB plus
128 KiB and 4096 records: both protocol byte ceilings plus the existing 64 KiB
margin. Publication, final reservation and Possible checks keep their original
allowances. No counter is replenished and no deadline or slot rule changes.

Native cases check 100 distinct maximum-size replies, including multibyte text
and HTAB, under runtime qualification; a one-byte change, null reply and
unexpected maximum DATA text refuse. Exact Body-check budget exhaustion is
tested independently of publication charges. Existing state, policy, ownership,
clock and deadline corruption cases remain. This reduces response volume and
per-check charging; finite record and turn limits still preclude a
maximum-message throughput claim.

## 180. Closed Body validation turns

`BodyChecks` binds one authenticated PostgreSQL connection to the exact Body
change, clock guard and original absolute deadline. It is non-cloneable and
thread-bound because its worker waker targets the opening thread. It exposes
only passive validation; it owns no SMTP authority, connection permit or work
meter. Trusted callers retain connection capacity and admission. Standalone
dispatch checks open one such connection per check; the body transport uses
the prepaid retained owner in section 181. Production dispatch remains inactive.

Each turn begins a READ COMMITTED transaction and obtains the migration key's
shared transaction-scoped lock before inspecting the full schema ledger and
store epoch. Runtime mode repeats the dedicated role and privilege checks on
every turn. It then takes the original policy/account locks and performs all
existing Body checks from section 179. The ordinary owner begin path retains
its session-lock behavior; this dedicated path never recursively acquires that
session lock. Successful validation awaits ROLLBACK completion, releasing the
migration and row locks, and checks its resulting limit again before return.
A refused check, failed cleanup, expired deadline or broken connection closes
the worker permanently; a later call returns the retained error without SQL.

The standalone 1 MiB input, 1088 frames, 64 KiB output and ten-second cap span
the entire connection, including all turns. The retained transfer profile in
section 181 separately prepays its larger finite connection budget. A successful rollback replenishes no
counter and extends no deadline. A returned limit can only tighten the next
turn's cap. Runtime authentication is connection-bound; schema, durability,
role, privileges and live dispatch conditions are rechecked each turn.
The schema, role, policy, owner, health, receipt and recipient reads use
explicit PostgreSQL parameter types so the driver can send each query in
one round trip without separately preparing and closing a statement. SQL
ordering, lock lifetimes and fresh observations are unchanged; no result
is cached between validation turns. The same single-round-trip calls cover
primary read snapshot selection and account state, device-token reads, body
reauthorization, Email/query pagination/counts and thread membership windows.
Their nullable parameters retain explicit bytea or bigint types; predicates,
ordering, result-cardinality checks and authorization boundaries are unchanged.

HTTPS uses up to three body workers, each retaining its own request snapshot,
credentials and original deadline. The worker count reserves two read slots
per concurrent exchange for verification plus fresh authorization. Session
API concurrency advertises this same configured worker count; upload
concurrency remains one because the native ingress buffer is shared. A
separate upload permit refuses overlapping uploads with 429 before backend
admission. When at least two workers exist, transfers may occupy all but
one; the final capacity remains available for API work. The default three
workers allow two downloads alongside an API request. No read
result or authority crosses workers. Existing pending-publication guards
still serialize or refuse conflicting mutations; parallel readers do not
promise parallel writes or change unknown-COMMIT settlement.

Native fixtures verify two checks on one backend, idle state after rollback,
and an independent connection acquiring exclusive migration and conflicting
row locks between turns. Policy, runtime-role, privilege and reply changes refuse the
next check and remain terminal after repair. Repetition exhausts the original
protocol allowance; time expiry retains the original cap. Terminating the
backend after the final live clock observation but before rollback produces
no successful observation. This qualifies a reusable validation boundary,
not a scheduler or maximum-body throughput.

## 181. Prepaid Body-transfer validation connection

Starting the bound Body transport consumes BodyPlan into a private retained
owner. Its admission uses the verified source's actual length L, bounded by
32 MiB, rather than the caller's maximum. A valid CRLF source expands to at
most E = L + floor(L/3) bytes under dot transparency. The finite check count
is 2 * ceil(E/16384) + 1 flush + 8 additional I/O attempts. Each cell
reserves plaintext acceptance and a separate TLS-record tail drain: the
16 KiB socket write cap is smaller than a full encrypted record. The extra
attempts cover further partial or Pending writes/flushes, not retries or
guaranteed progress.
Every attempted write or flush consumes a check, including those that make
no progress. Unused allowance is not returned.

Before connecting, admission prepays that count times 1 MiB plus 128 KiB I/O
and 4096 records. Checked arithmetic derives the same count's cumulative
PostgreSQL input (1 MiB each), output (64 KiB each) and frames (1088 each).
A 32 MiB source admits at most 5471 checks: 6154.875 MiB I/O and 22409216
records. These are finite work counters, not resident-memory reservations.
One ordinary ReadPermit covers the retained validator separately from final
capacity. The original Body/meter cap only tightens; no connection, turn or
window renews it.

Each turn additionally admits at most 1 MiB input, 64 KiB output and 1088
frames, with authentication included in the first window. Available idle
traffic drains against the preceding window before the next is granted.
Only acknowledged rollback permits a later window, and the finite source
budget bounds grants independently of the dispatch check counter. Parser
state and authentication survive boundaries. Aggregate counters only
decrease, and protocol failure permanently refuses further grants.

Each write or flush spends a credit, checks Pending and the original
meter/deadline, then performs the full section 180 transaction with fresh
qualification and live fences. Exhaustion refuses before SQL or new SMTP
output; no hidden reconnect, counter replenishment or deadline extension is
available. Failure closes SMTP and drops the validator before releasing its
permit. On body completion that order precedes Possible publication, leaving
the second slot available beside the final reservation. The earliest limit
carries into Possible and final custody. Standalone Body/Possible profiles
are unchanged.

Native fixtures preserve the two-slot pool, 64000-byte transfers and the same
idle validator across checks. A near-worst-case 1000002-byte dot/CRLF source
exercises 1333336 encoded bytes under the original ten-second job limit, with
initial database budgets covering all phases and no replenishment. Separate
144000-byte cases deliberately lack the complete prepaid admission: they
stop before body output with Capacity, Body durable, Pending empty and slots
returned. One byte or record short of admission refuses before connecting.
The source snapshot separately retains its own migration lock while reading.
This qualifies bounded transfers, not maximum-32-MiB throughput, arbitrary
backpressure or production activation.

## 182. Bound immutable source and explicit source admission

BodyReady.with_source selects only its private Dispatch through the existing
verified attempt-body reader. Its callback receives BodySourceReady, which
privately joins the verified reader to the same actual DATA session, MAIL
mode and final reservation. Its begin accepts no replacement source. The
callback drives source-scoped Exchange/Progress to Ready and returns unit or
a transfer Refusal. Scoped wrappers privately borrow a completion sink owned
by with_source; Ready stores the final handoff there and exposes no final
operation or completion owner to the callback. Only successful callback and
post-callback source validation release Completed for final SMTP. The lower trusted body adapter retains its explicit matching-input
contract for focused interruption fixtures.

Before any source connection, admission checks Pending, Runtime mode and the
original shrinking limit, prepays 131 MiB I/O and 32768 records, and acquires
two slots from a distinct body pool. Aliasing the ordinary pool refuses. The
snapshot profile allows 128 MiB input and 512 KiB output; each of two fresh
admission profiles allows 1 MiB input and 64 KiB output. The work charge covers
130.625 MiB of protocol ceilings plus 384 KiB margin, not resident memory.
Two slots cover the second admission overlapping the immutable snapshot.
Both remain held until the reader returns and destroys its connections. The
ordinary two-slot pool still covers final custody and retained Body validation.
No budget or deadline replenishes. Source admission shortens the retained
BodyPlan limit, so a shorter source allowance also bounds final custody.

The wrapper retains BodyReady outside verification until callback entry, and
retains the callback result and private completion sink outside the reader's
post-callback source check.
Failure before entry closes the original session. Failure after completion
closes Completed and returns its buffers without a terminator; durable
Possible remains conservative. An already returned transfer Refusal keeps its
original error and buffers even if the later source check fails. Callback
success without stored completion refuses. A callback error after Ready
explicitly closes the stored completion and returns its actual buffers.
Source read failures do not invent an uncertain publication or replace Pending.

Native composed fixtures use the bound source path with exact admitted work.
They cover one-byte/record-short admission, a one-slot pool, aliasing ordinary
slots, a source size refusal, snapshot termination after body completion, and a
shorter source deadline that remains binding before final SMTP. A transfer
refusal followed by snapshot loss retains the transfer error and buffers.
Callback success without Ready refuses; callback error after Ready closes the
private final handoff and returns its buffers. Refusals close the session, preserve the expected Body/Possible phase and send
no terminator. Source slots are reusable before final exchange or recovery.
The 64000-byte success still uses the original ordinary message allowance;
source work is explicitly separate. This adds no production dispatcher or
maximum-message throughput claim.

## 183. Selected TLS provenance before MAIL

Selected-profile loading records the exact process-local relay TlsPolicyId
from the TLS generation prepared by that same load. It inspects only the
reserved owner's ID and borrowed payload, without publishing or exposing its
TLS lease. Non-relay bindings have no relay policy identity. Binding clones carry this passive
identity through Claim, Dispatch and MailPlan; no database digest, receipt,
schema or persisted generation contains it.

Before constructing MAIL, postgres_relay_mail compares Session.policy_id()
to the plan's private expected identity. Mismatch closes the authenticated
transport and returns buffers with Forbidden before any MAIL output or new
database operation. The caller cannot supply a substitute expected ID.
Generation identities are process-wide and never reused in that process;
matching host, port, route or credentials cannot substitute for this match.
The session still proves TLS and AUTH against its retained policy. Original
selection, ownership, route-health, envelope, clock and work checks remain
mandatory; local policy identity supplies no renewed queue authority.

Successful MAIL and RCPT owners no longer expose public split operations.
Only their internal next-stage adapters separate fields; external callers
continue through bound RCPT/DATA or close and retain passive evidence. Body
mode remains the caller's existing explicit SevenBit/EightBit choice, with
negotiated 8BITMIME and encoder validation unchanged.

Native composed fixtures publish and use the exact TLS generation from the
PostgreSQL profile load. Same-profile transfers still succeed. Separately
prepared same-destination generations, including rotated credentials, are
authenticated to the fixture but refuse before MAIL; durable Prepared and
Pending remain unchanged and buffers return. Existing envelope/deadline
refusals remain covered. Compile-fail examples pin external split refusal.
This is preparation for the eventual runtime; no relay scheduling or backend
activation is enabled.

## 184. Ordered retirement of a failed Prepared attempt

PreparedResult.into_failed consumes pre-MAIL authority into a non-cloneable
FailedPrepared continuation. The caller first closes all failed transport and
classifies the actual remote failure. Local capacity, cancellation and invalid
configuration are not automatically remote Network or Auth failures. This
owner derives both the route-health change and one interrupted-recovery
request from its private Dispatch; it accepts no replacement claim, fence,
attempt or recovery operation. Preparation failures retain passive custody.
Both requests exist before either publication starts.

Each step runs at most one bounded publication or exact receipt inspection.
Health must complete before recovery retires recipients. A known failure
retains that publication stage for explicit retry. An unknown health or
recovery COMMIT moves the exact request into the existing Pending barrier
and switches the continuation permanently to receipt inspection for that
stage. Missing receipts and errors never permit republication. A validated
health receipt advances to recovery; only validated recovery completion
returns the combined passive outcome. Repeated completed steps do no I/O.

The continuation never accepts public Settlement data as authority. It
inspects its own retained request and matches an occupied Pending slot by
private request identity, not merely attempt ID; another operation refuses
Busy. If ordinary Pending.settle already found a receipt and cleared the
slot, the continuation reinstalls its exact request and inspects it again.
Empty Pending proves neither success nor rollback. Inspection and matching
slot clearance occur under the existing Pending mutex. No second pending
queue is introduced.

After transport is fenced, passive steps may receive fresh bounded deadlines
and work even when the original SMTP cap has expired. Runtime admission,
the supplied and meter deadlines, the ten-second connection ceiling and
ordinary ownership/selection/expiry fences still apply. Publication uses
ordinary capacity; inspection uses a separately reserved settlement pool,
which must not alias the ordinary pool. No Prepared owner, session or
transport permission can be reconstructed from this continuation.

Native PostgreSQL/TLS cases cover actual AUTH refusal and its repair hold,
Network backoff, TLS repair hold, settlement after SMTP expiry, known health
and recovery rollback, delayed commits and missing receipts, generic
settlement followed by exact reinspection, budget/deadline/slot refusal,
ordinary-slot occupancy during inspection, unrelated Pending preservation,
and recovery expiry-wait marker. They assert health before retirement, one health
count and one recovery sequence change, retained Prepared on early failures,
and candidate refusal by backoff/repair after retirement. This activates
no scheduler or PostgreSQL backend and changes no schema.

## 185. Configured relay setup bound to Prepared custody

The inert postgres_relay_connect adapter consumes one PreparedResult and a
PublishedProfile. Cold Prepared.publish constructs that bundle from its own
Binding, resolved configuration and successfully published TLS generation.
Publication in another generation domain refuses and returns the original
preparation. The retired generation remains an explicit cold-disposal result.
No caller can assemble a bundle from separately loaded credentials or policies.

Before network setup, reserve the existing MailPlan and its negative-reply
storage. Compare its private policy identity to the bundled relay policy
before DNS, TCP or credential output. Resolve only the configured relay name
through the configured numeric DNS servers, dial bounded returned addresses,
and use the matched policy's endpoint and TLS mode. Required STARTTLS keeps
the plaintext greeting, EHLO offer and upgrade on the same socket, discards
plaintext capabilities, and requires a fresh verified TLS EHLO before AUTH.
Implicit TLS also requires verified TLS, greeting and EHLO before AUTH.

All setup uses the original worker RuntimeClock and the minimum of the
Prepared limit, original deadline and remaining wire-meter deadline. Instant
limits convert down to whole milliseconds, never extending them. The wire
deadline first tightens retained Prepared custody before database admission;
MAIL entry likewise tightens it before its native check. Fresh native
authority checks run before resolution, after resolution before dialing,
before AUTH and before entering MAIL. No database connection or lock survives
an authority check into DNS, dialing, TLS or AUTH work. After the final MAIL
entry observation, section 188 admits the retained envelope validator; its
transactions and locks still end before every SMTP transport call. A fence is an observation, not a claim
that remote SMTP output and database revocation are atomic.

Successful setup returns opaque ConnectedPrepared. Its session and MailPlan
cannot be split; only bound MAIL or explicit closure consumes it. MAIL entry
retains the existing refusal and any command evidence plus passive recovery
identity. Every setup refusal closes transport and returns reusable buffers.
A private Dispatch retained before consuming admission produces only closed
interruption recovery; failed recovery preparation returns that same passive
owner. An isolated complete actual 535 AUTH reply instead creates the ordered
FailedPrepared Auth continuation. Local budget, invalid configuration,
unsupported capabilities and ambiguous network/TLS errors do not infer a
remote health classification. There is no automatic retry or alternate AUTH.

Native fixtures exercise configured DNS and actual TCP in both TLS modes,
then the existing 64,000-byte bound envelope/source/final chain. Separate
cases prove foreign-domain publication refusal, policy mismatch and empty
work before DNS/TCP, ownership expiry before AUTH with no credential output,
blocked SQL under shorter setup and MAIL-entry wire deadlines, and actual
AUTH refusal followed by health-before-recovery settlement.
Setup adds no scheduler or backend activation. The source-sized Body allowance
and ten-second native job cap still require qualification for maximum-sized
messages and backpressure before the relay runtime can be called usable.


## 186. One bounded configured relay attempt

The inert postgres_relay_attempt driver consumes ConnectedPrepared and runs
bound MAIL, ordered RCPT, DATA, immutable source transfer, Possible and final
publication on the caller's admitted worker. The same wire/database meters
and reusable command/body cells span the attempt. Source work and its separate
two-slot pool remain explicit. Source completion stays inside the existing
private sink until snapshot postvalidation releases final authority.

Only durable actual MAIL, DATA or final results return Completed. All other
stops close transport and return the existing phase-specific evidence owner
plus passive interruption custody retained from the connector's private
Dispatch. No caller can substitute a session, attempt or live phase owner.
The driver neither settles failures nor invents remote-health classifications.

Stopped is a trusted runtime handoff, not an automatic recovery instruction.
Preserve and publish actual negative/final evidence and inspect any unknown
Pending first. RCPT uses its positive-reply-aware interruption conversion;
DATA planning and Body failures use their corresponding evidence-aware
conversions. Discard generic fallback if settlement retires the attempt.
Otherwise it addresses only unresolved durable state. Durable Possible stays
uncertain even when source postvalidation fails before a terminator is sent.

Pending commands sleep at most five milliseconds under the captured upper
cap. Body transfer waits only when a Write/Flush turn still has Write/Flush
work afterward; pure source/codec turns and completed writes leading back
to encoding proceed immediately. This phase hint is crate-private and
supplies no authority. The next existing phase advance owns any tighter
deadline and exact refusal evidence. The driver adds no fresh budgets, renewed deadlines, connection
pool, thread, scheduler, retry or backend activation. Source-sized Body
admission retains the maximum-message/backpressure limitations in section 181.

Native configured fixtures exercise both TLS modes with one wire meter from
setup through final delivery: 64,000-byte mixed-recipient success, a
1,000,002-byte near-worst-case dot/CRLF source, prepaid
144,000-byte Body refusal, mixed RCPT interruption preserving the unanswered
recipient, immutable-reader failure after Possible with no terminator, and
an unknown final commit retaining actual reply evidence for inspection.
They assert exact durable recipient outcomes, empty Pending after settlement,
and returned ordinary/source pool capacity. These cases qualify the bounded
driver, not maximum-sized messages or arbitrary peer backpressure.

Successful MAIL/RCPT/DATA command writes, completed flushes and partial
reply reads immediately continue through the existing bounded advance
operation. A private per-turn progress hint resets before each advance and
is set only for validated positive-length writes or reads, or a completed
flush. Pending writes, flushes and reads retain their existing polling waits.
Every advance keeps its before/after deadline checks, per-turn work charge,
one TLS operation and at most 512 parsed bytes. Fresh authority checks
before every attempted command write/flush remain unchanged. The hint grants
no transport or publication authority and cannot carry progress into an
idle turn. Removing reply-read waits alone does not establish full-recipient
throughput; section 188 qualifies the combined retained-validation path.


## 187. Retained untouched-envelope observations

The passive EnvelopeChecks owner authenticates one thread-bound connection
for observations of an exact active Prepared attempt. Each check starts a
new READ COMMITTED transaction, obtains the transaction-scoped migration
lock, inspects schema and store epoch, requalifies Runtime role/privileges,
and locks selected policy and account state. It then runs the same complete
envelope-validation implementation as standalone observations: live route
ownership, queue clock, health, active receipt, submission and all selected
recipient fields, followed by a fresh expiry observation.

A successful turn acknowledges ROLLBACK and checks the resulting conservative
local deadline before returning. The returned envelope is fresh passive
evidence; its caller must compare it with the admitted envelope before any
SMTP action. A valid address change is observable, not cached or silently
accepted as transport authority. No transaction, migration lock or row lock
spans returned control. Authentication persists, but authority does not.

The original deadline only shrinks. Input bytes, protocol frames and output
bytes in standalone EnvelopeChecks::open are cumulative over the connection:
1 MiB, 1088 frames and 64 KiB, respectively. No successful turn renews them.
The configured bound profile in section 188 prepays finite per-turn windows
within a separate aggregate ceiling. Validation, protocol, rollback
or expiry failure closes permanently, including after later database repair.
The caller owns work admission and connection capacity. This passive API
neither supplies SMTP authority nor adds a scheduler or activates PostgreSQL.

Native tests exercise 100 selected Prepared recipients, backend reuse, idle
ROLLBACK state, independently obtainable conflicting locks, selected-policy,
role, privilege, recipient-phase and owner-generation fences, fresh address
observations, cumulative exhaustion, original expiry, and backend termination
after the last live clock observation but before acknowledged rollback.


## 188. Prepaid configured envelope validation

ConnectedPrepared::begin_mail first tightens its deadline and performs the
existing fresh standalone observation. It then prepays one finite envelope
validation profile and reserves one ordinary ReadPermit before entering MAIL.
The thread-bound validator follows successful MAIL, ordered RCPT and DATA
custody. Direct low-level plans retain their standalone observation behavior;
all configured attempts use this retained path. Authentication persists, but
every attempted command write or flush still runs the complete fresh checks
from section 187, compares every admitted envelope field and acknowledges
ROLLBACK before transport. No transaction or database lock spans SMTP I/O.

For N selected recipients, admission covers 2*(N+2)+8 checks: write and flush
for MAIL, each RCPT and DATA, plus eight additional partial/Pending attempts.
N is checked against the existing 1..=100 attempt bound. Each check has at most
1 MiB protocol input, 64 KiB output and 1088 frames. The first includes
connection authentication; available idle traffic consumes the previous
window before the next one starts. Only acknowledged successful rollback
closes a window. Aggregate counters and the finite turn count never refill.
These are the same checked budget and protocol-window implementation used by
Body validation, with independently derived envelope and source profiles.

Prepayment charges each check 1 MiB+128 KiB I/O and 4096 records. At 100
recipients this is 212 turns, 238.5 MiB I/O and 868352 records; the protocol
ceilings are 212 MiB input, 13.25 MiB output and 230656 frames. These are
cumulative work bounds, not retained allocations. Each attempted check still
checks Pending and both original and current meter deadlines, consumes one
credit and only tightens the local cap. Failure permanently closes the
retained validation state; it cannot reconnect or fall back to standalone
checks. No deadline or meter is renewed.

The worker is destroyed before its ordinary permit is released. Closure
precedes negative command publication, actual 354 publication, Body/source
admission and every passive failure/evidence handoff. Successful positive
RCPT custody alone may carry the validator into DATA. Partial RCPT results,
421, explicit close, planning failures and failed DATA custody retain evidence
without retaining the validation connection or permit. Existing final-result
and source reservations remain independent.

Native fixtures exercise repeated backend identity and idle ROLLBACK after
MAIL and RCPT, independent conflicting migration/row locks, exact one-byte
and one-record-short prepayment refusal before MAIL output, and DATA outcomes
while another ordinary slot is held. Both slots must be reusable while failed
publication evidence is still retained. The configured full-attempt fixture
also exercises the 100-recipient boundary and maximal normalized RCPT text
under the original cap. Relay process scheduling remains unimplemented.

## 189. Productive command output pacing

The configured driver also skips its fixed polling delay after a command
write reports validated progress or a flush completes. The next bounded
advance still performs its fresh database authority check before any further
write or flush. Partial writes consume the existing finite validation-turn
allowance; no-progress attempts still wait. Every turn retains the original
meter and before/after deadline checks, with no additional transport call.
At 100 recipients this removes the fixed waits after the 204 ordinary
MAIL/RCPT/DATA write and flush turns. It does not extend the ten-second cap
or change final DATA terminator custody or pacing.

The paired TLS test drives successful command output, an idle reply turn,
a productive partial reply and another idle turn, proving that the hint
resets rather than carrying output or read progress into an idle operation.


## 190. Admitted route-candidate discovery

Dispatches::candidates wraps the existing route scanner in the same bounded
worker admission as attempt publication. Only Runtime connections enter;
Pending must be empty and its nonblocking operation guard spans the read.
Each page prepays 1 MiB+128 KiB protocol I/O and 4096 records, reserves one
ordinary ReadPermit, and uses the minimum of the supplied deadline, current
meter deadline and ten-second operation cap. Slot or work exhaustion refuses
without waiting, and successful reads must still finish before that cap.
The database connection closes before the slot returns. No read installs,
clears or settles a Pending operation.

Fresh, retry and expiry pages retain the scanner's existing policy, ownership,
route-health, active-attempt, ordering and cursor rules. Pages contain at most
64 passive hints, not reservations or SMTP authority. A new scheduling pass
still restarts pagination. Candidate timestamps do not update or bypass the
persistent queue-clock guard; Prepared/expiry publication must independently
recheck the clock, current policy, owner and exact durable state. No scheduler,
transport or production backend is activated.

The native dispatch fixture compares admitted pages with the standalone
scanner and checks one-byte/one-record-short and nonrenewing exact budgets,
expired input/meter deadlines, preparation-mode refusal, stale ownership,
cursor binding, slot exhaustion/reclamation and unchanged account state.
An existing delayed Prepared COMMIT verifies all three scan kinds refuse
while the exact unresolved operation remains in Pending.


## 191. Admitted route acquisition and retained uncertainty

Dispatches::acquire_route admits one exact prepared Claim through the worker's
Runtime-only connection, original input/meter deadline and ten-second cap.
It prepays 2 MiB+192 KiB I/O and 4096 records for receipt inspection and
possible publication, reserving one ordinary slot across both sequential
connections. Admission refusal installs no Pending operation. The claim then
moves into the shared Pending guard before database work. Known success and
definite failure clear custody; an unknown COMMIT retains the exact claim.
No further scan, attempt, phase or acquisition can pass that barrier.

Pending::identity replaces its attempt-only accessor with a closed identity
sum: Attempt, RouteClaim or Expiry. Existing attempt publication, recovery and route
failure identities are unchanged. Pending::settle uses independently admitted
capacity to inspect the retained claim receipt, with no acquisition replay.
Absence and errors preserve custody; a matching receipt clears it and returns
Settlement::RouteClaim. The grant is historical evidence only, even when
returned by a known acquisition. Fresh current-owner and attempt admission
must still precede SMTP, and an expired or released receipt cannot revive an
owner. Known committed results are not converted into deadline refusals.

The native route-owner fixture exercises short and nonrenewing budgets,
expired input/meter deadlines, mode refusal, ordinary/settlement slot pressure,
known stale receipt replay, definite busy refusal and delayed acquisition
COMMIT. All candidate kinds and further worker publication refuse while the
claim remains unresolved. Removing the receipt temporarily proves absence
cannot clear Pending; restoration permits inspection-only settlement. These
cases preserve account sequence and leave both slot pools reusable.

## 192. Admitted expiry and retained uncertainty

`Dispatches::expire` brings one prepared queue-expiry operation under the
relay worker's existing Runtime, original deadline, work-meter, ordinary-slot
and Pending admission. The caller must close its route's SMTP transport
before expiry, including the current owner's attempt. This worker entry point
activates no scheduler and grants no transport authority.

Admission prepays the existing receipt inspection and possible publication
connection ceilings. The shared Pending slot takes the exact owned Expiry
before SQL; its public identity is `Identity::Expiry(SubmissionExpiryId)`.
Known publication or historical replay returns the outcome and clears
custody; definite failure clears custody; an unknown COMMIT retains the
operation and excludes every further worker operation. Independent settlement
capacity can inspect only that exact receipt. Absence and errors retain the
barrier; `Settlement::Expiry` clears it only on a matching historical outcome.
Settlement never retries publication or changes recipient state.

The underlying expiry transaction retains fresh policy and owner fencing,
explicit queue-clock confirmation after the real five-minute stability hold,
process-local and database-wide spacing, at most 100 recipients, current
attempt reservation handling, uncertainty preservation and one account
sequence change. Admission does not confirm or replace the shared clock
guard. Every expiry clock-guard access uses nonblocking lock acquisition;
contention refuses as Clock(Invalid) without waiting for another holder. Historical receipt replay may settle during a new clock hold because
it cannot perform another expiry mutation.

The native submission fixture starts a real runtime clock and database-UTC
guard during setup, waits out any remaining hold while renewing the current route lease,
then explicitly confirms
it before the expiry exercise. It drives the existing delayed-COMMIT expiry
through this worker, including absent-receipt retention, separate settlement
capacity, blocked worker operations, pre-SQL admission refusals, unconfirmed
clock refusal, historical replay, nonrenewing budgets and reclaimed slots.
The existing recipient-state and sequence assertions remain in force.

## 193. Bounded administrative device listing

`postgres_devices::list` reads one account's complete device registry,
including revoked rows, under a fresh repeatable-read, read-only snapshot.
It validates the primary, current schema and expected store epoch while
retaining the session migration lock, distinguishes an absent account from
an empty registry, and acknowledges rollback before returning a successful
result. Every path closes the connection. No account
write fence is taken and no mail sequence, credential or receipt is changed.

The query selects only ID, label, creation time and revocation time, ordered
by ID. It never requests passwords, salts, verifiers or request digests.
At most 257 rows are fetched so a registry exceeding the existing 256-device
ceiling is refused without a truncated success. IDs, labels and nonnegative
timestamps are validated before any result is returned. The returned public
metadata is historical and cannot authorize authentication or a mail operation.

The explicit preparation command is:

```text
td-mta postgres device-list --connection-file PATH --epoch HEX --account ID
```

It uses the protected connection-file loader, verified TLS/SCRAM, fresh
bounded DNS resolution and the existing shared ten-second connection/operation
deadline. Database credentials must have the required schema/account and
metadata-column read privileges. The command grants none. The syntactic
bootstrap profile placeholder selects no running profile. JSON output binds
the expected epoch and account and contains the four public fields for each
device, with escaped labels and nullable revoked_at. Errors report fixed
categories without connection inputs or provider diagnostics. This supplies
inspection after lost creation output; it neither recovers a password nor
creates, revokes, authenticates or activates a credential or service.

The native fixture exercises the API and actual command with a role lacking
salt/verifier read privileges, Unicode and quoted labels, revoked entries,
empty and foreign accounts, stale epoch, malformed metadata, exact capacity
and overflow refusal. Device enumeration leaves account sequences unchanged.

## 194. Explicit administrative device revocation

`td-mta postgres device-revoke --connection-file PATH --epoch HEX
--account ID --device ID` publishes one owner-side revocation in the
preparation database. The protected connection input, fresh DNS resolution,
verified TLS/SCRAM and overall ten-second worker deadline are the same as
other explicit administration commands. The committed revocation is visible
to the active PostgreSQL authentication and operation fences.

The command samples checked UTC milliseconds and prepares one random
revocation operation before address attempts. It retains that exact value
through transport fallback. The existing account write fence, expected
epoch and immutable receipt rules in section 21 apply. Success names the
public epoch, account, device and current invocation's operation ID, with
`recorded` or `already-revoked`. The latter does not claim that the current
operation has a durable receipt: the prior revocation remains unchanged.
No password, verifier, digest or database credential is printed.

A possible COMMIT without a known response reports the fixed error
`indeterminate-inspect-device` and does not retry or claim success. The
operator can inspect the named device with `device-list`; absence of a
revocation in a snapshot does not settle an outstanding COMMIT. A later
explicit revoke of that same device is safe: the account fence serializes
with the original operation and either records a revocation or observes
the retained prior one. It cannot identify the original operation's receipt
and never creates a replacement credential. Process loss has no automatic
retry or on-disk operation recovery. No creation command is introduced.

## 195. Explicit administrative device creation

`td-mta postgres device-create --connection-file PATH --epoch HEX
--account ID --label LABEL` creates one owner-side preparation credential.
The protected connection input, fresh DNS, TLS/SCRAM, ten-second worker and
expected epoch rules match other explicit preparation commands. The label
retains the registry's 128-byte UTF-8 limit and control-character refusal.
The command takes checked UTC milliseconds and prepares one Creation before
address attempts. The existing 256-record ceiling includes revoked devices.

Only confirmed creation success prints the password, once, in the local JSON
receipt together with its public device identity. The guarded password is
written directly in its fixed hex-dot-hex encoding, without a temporary
String. The database receives only its salted verifier. No database password,
salt, verifier or request digest is printed. Output remains the operator's
local credential receipt and must be retained before leaving the terminal.

A failed publication prints only the fixed error category and public epoch,
account and generated device ID. A possible COMMIT without a known response
reports `indeterminate-inspect-and-revoke-device`; the command does not retry,
print the password or generate a replacement credential. Inspect and revoke
that named device using sections 193 and 194. A read showing no record does
not settle an outstanding transaction. Process or output loss after success
can leave an undisclosed credential: inspect the account registry and revoke
it before explicitly creating another. The label is not a unique operation
identity, and rerunning creation is never recovery of the original password.
No on-disk recovery, serving admission or file-registry change is introduced.

## 196. Explicit administrative account bootstrap

`td-mta postgres account-init --connection-file PATH --epoch HEX --account
ID` prepares one random Inbox identity before bounded address attempts and
uses section 27's atomic account bootstrap. A confirmed publication prints
its public epoch/account/Inbox receipt. The protected connection input,
fresh DNS, TLS/SCRAM and ten-second operation deadline match other explicit
preparation commands. Initialization neither activates account policy nor
admits a serving backend.

A publication failure reports only a fixed error and the attempted public
tuple. An outstanding COMMIT reports `indeterminate-inspect-account` and
stops without retrying. `td-mta postgres account-inspect --connection-file
PATH --epoch HEX --account ID` reads the retained bootstrap receipt, not
current Inbox contents, sequence or serving authority. Absence cannot settle
an outstanding original transaction; let it finish before interpreting a
missing receipt. A stored receipt can be compared with the attempted tuple.

A fresh initialization invocation generates a different Inbox identity and
conflicts with a preexisting bootstrap receipt. An account lacking a receipt
also refuses initialization. Neither command resets, repairs or adopts an
existing account. Lost process output is recovered by inspecting the named
account's retained receipt, not by blindly repeating initialization. Account
retirement, import and serving configuration remain separate operations.

## 197. Process-independent selection request custody

`postgres_selection::Change::encode` produces exactly 89 public bytes:
format version 1, 16-byte epoch, 16-byte operation ID, eight-byte big-endian
expected generation, 16-byte configuration version and 32-byte source digest.
`Change::decode` requires that exact length/version, refuses an exhausted
generation and recomputes the domain-separated request digest internally.
It does not retain caller-supplied request-digest bytes. Persist the encoded
change before first publication and retain the same bytes across uncertainty.
The encoding contains neither configuration source nor credential material.

These are custody bytes, not an authenticated credential or a success receipt.
Changing the operation ID asks for another operation; changing any request
field changes its derived digest. Owner database credentials, stored receipt
comparison, expected generation and current source/material validation keep
their existing roles. Decoding cannot establish publication or authorize
service. An already recorded operation still resolves after a newer selection
or material expiry, while a changed request using its ID conflicts. Missing
receipt observations do not authorize a replacement operation.

This supplies the bounded recovery representation used by section 198’s
explicit preparation commands. It does not select production PostgreSQL
serving. The real PostgreSQL fixture reconstructs an uncertain
operation before settlement, rejects altered custody against its receipt and
reconstructs historical requests after newer selection and material expiry.

## 198. Explicit selection request preparation and settlement commands

`td-mta postgres config-selection-prepare --connection-file PATH --epoch HEX
--version HEX --generation N` rereads and validates the immutable source
version, prepares one random operation and prints section 197's public request
as 178 hexadecimal characters in JSON. Preparation performs no selection
write. Retain that request before any publication; the expected generation is
explicit, with zero meaning the initially unselected database.

`td-mta postgres config-select --connection-file PATH --request HEX` decodes
and publishes exactly that request. `td-mta postgres config-selection-inspect
--connection-file PATH --request HEX` resolves its retained receipt under the
existing policy-row lock. The public request may be supplied as an argument;
it contains no password, source text or private material. Malformed length,
non-hex input and duplicate or mixed operation arguments refuse. The protected
connection file and ordinary bounded DNS/TLS/SCRAM rules still apply.

A recorded result prints the operation, historical generation and version.
Inspection can return `recorded:false`; that observation is not permission to
replace the operation. Publication uncertainty reports
`indeterminate-retain-request` and stops without automatic retry. Resolve or
explicitly retry the original retained request, never prepare a replacement
from an absence observation. Historical receipts remain resolvable after a
newer selection or expired material, while new publication revalidates the
version and compares the expected generation. An altered request using a
recorded operation ID conflicts.

These commands operate on the preparation database. They do not activate a
listener, runtime generation or PostgreSQL production backend. Receipt success
does not assert that its version is still selected or that current materials
are usable. The finite 1024-receipt ceiling and lack of pruning remain exact;
these commands add no receipt-retention or automatic deployment policy.

## 199. Historical account usage audit

`postgres_usage_audit::inspect` compares the five stored account-usage counters
with durable rows in one finite, read-only REPEATABLE READ primary snapshot.
It checks the expected epoch under the shared schema barrier, captures the
account sequence/floor, distinguishes a missing account from a missing usage
row, and acknowledges ROLLBACK before the final deadline check. The returned
identity and counts are historical observations, not quota admission or policy
and device authorization. No account row lock, write or repair is performed.

Body bytes and blob count include every live blob manifest, including a
zero-length blob. Upload bytes count each manifest with an existing lease;
expired but unreclaimed leases still count. Queue bytes count each manifest
referenced as a transmitted blob by a submission, once even if several
submissions reference it. Queue submission count includes every durable
submission, including completed ones retained in the database. Reserved blob
IDs without a live manifest contribute nothing. These are the same durable
categories used by publication accounting and the initial usage migration.

A mismatch in any counter, missing usage row, malformed sequence or negative
result refuses the audit. Aggregate overflow or query failure also refuses;
there is no truncation or inferred correction. The normal finite probe and
statement bounds apply, as detailed in RESOURCES. A large account that cannot
be checked within those bounds has no successful audit result.

This is one component for future semantic verification. It does not validate
body digests, metadata references, mailbox graphs, operation receipts,
credentials, configuration or delivery outcomes, and cannot authorize restore
or serving. The native fixture deliberately demonstrates that matching counters
can coexist with a damaged body. It also checks account isolation, zero-length
manifests, expired leases, shared queue references and refusal without repair.

## 200. Explicit historical usage inspection command

`postgres usage-inspect --connection-file PATH --epoch HEX --account ID`
loads the protected owner-side connection file and invokes section 199's
read-only counter audit for one explicit account and store epoch. It shares
one DNS/connection/operation deadline across resolved-address attempts; only
transport refusal permits another address. No profile is selected or served.

Successful schema-1 JSON reports the account, epoch, snapshot sequence and
history floor, plus `body_bytes`, `blob_count`, `upload_bytes`, `queue_bytes`
and `queue_submissions`. `counters_consistent:true` describes that historical
snapshot only. `semantic_verified:false` explicitly preserves the narrower
scope: this command checks no body digests, metadata graph, retained receipts,
configuration, credentials or restore safety. Success supplies no quota or
serving authority and does not imply the counters remain current.

Missing accounts, wrong epochs, absent or inconsistent counters, database
failures and deadlines refuse with fixed error codes. Duplicate, missing and
unknown arguments refuse before loading the connection file. There is no
repair, force or all-accounts option. The command never prints credentials,
body bytes or native database error text, and never changes counters or
account sequence. The native process fixture checks JSON values, fixed
refusals, malformed options and persistence of deliberately corrupted usage.

## 201. Historical single-blob integrity audit

`postgres_blob_audit::inspect` verifies one explicitly named account/blob in
one finite read-only primary REPEATABLE READ snapshot under the expected store
epoch and shared migration lock. The caller supplies trusted administrative
connection access, not a device password. The preparation command in
section 208 exposes this verification component, never a download or device
authorization path; it returns only
public manifest metadata and the snapshot account sequence/floor.

The existing body reader checks manifest bounds, changed sequence, exact
chunk count, ordered contiguous ordinals, chunk extents and the complete
SHA-256, including the empty body digest. It uses the existing 32-MiB body
ceiling and eight-chunk batches, not a message-sized buffer. The original
connection budgets and deadline cover metadata, all body reads and an
acknowledged ROLLBACK; a final deadline check precedes success. Missing rows,
wrong epochs, corrupt manifests/bytes, database failures and timeouts refuse.
Failure closes the private worker and never repairs any row.

A successful result certifies only this body's historical snapshot. It does
not check counters, owning Email/submission/lease references, other metadata,
configuration, credentials or retained receipts. It provides no current quota
or serving authority, complete-store certificate or restore approval. The
native PostgreSQL fixture checks corrupt bytes and extents, ordinal gaps,
manifest/sequence inconsistency, empty bodies, account isolation and absence
of device credentials while pinning the narrower verification scope.

## 202. Historical canonical metadata audit

`postgres_metadata_audit::inspect` runs the shared `metadata_sweep` against
all eleven canonical PostgreSQL metadata tables in one owner-side primary
REPEATABLE READ, READ ONLY transaction. The expected epoch and shared
migration lock precede the snapshot. The caller supplies trusted UTC and
explicit row/parent-read limits; no device credential or serving profile is
required. The private adapter cannot escape the worker or expose history
reads. It refuses `next_change`; this is no change-history certificate.

Account-scoped keyset reads return one row, in decoded `Key::compare` order.
Compound text and import keys use their logical SQL column tuples and C
collation, not transient length-prefixed encodings. PostgreSQL fields become
shared typed rows and pass their canonical encoder/decoder and key checks.
SMTP origins additionally read at most 1001 ordered receipt recipients,
refusing count disagreement, ordinal gaps, excess or malformed receipt data.
Receipt rows on a non-SMTP Email also refuse. Every canonical changed sequence
must be no later than the captured account sequence; sequences retain all
64 bits. Parent lookups stay in the same snapshot and account.

The existing shared checks enforce local row rules, direct owning references,
rooted mailbox chains and exact submission recipient coverage. Their report
retains the snapshot identity and counts. It does not add policy checks that
those shared checks lack (for example uniqueness of mailbox roles), audit
usage counters, body bytes, credentials, configuration, retained operation or
delivery receipts, or change history. No whole-store, restore, serving or
current authority follows. Independently collected body/counter reports must
not be presented as one consistent account snapshot.

A dedicated preparation-only audit worker derives checked cumulative wire,
frame and command allowances from the caller's logical bounds. It reserves
`5 * rows + parent_reads + 64` turns for the three passes, direct references,
parent walks, schema checks, table-end probes and acknowledged ROLLBACK.
Each turn admits at most 1 MiB input, 64 KiB output and 1088 frames, including
at most 1001 SMTP receipt rows. Authentication shares the first window.
Only completion admits the next window; idle bytes remain charged to the
previous window, cumulative counters never replenish, and failures are
terminal. Counters allocate no proportional memory. The existing 128 KiB
frame, SCRAM sequence, row decoder and fixed scratch bounds remain in force.
Overflow or a counter that does not fit the host refuses before connection.

The caller supplies one deadline no more than an hour away. Connection and
authentication still have at most ten seconds, individual SQL statements
five seconds and locks one second. The original audit deadline covers every
turn and acknowledged ROLLBACK and is checked after worker destruction.
No turn, address attempt or logical allowance renews time. Runtime workers
retain their ordinary bounds and cannot use this owner path. Connection
failure during metadata reads retains its connection classification rather
than being reported as canonical metadata corruption. A large account may
refuse without a partial success report.
Failures close the connection and perform no repair. Native PostgreSQL
coverage exercises every table, compound cursor order, malformed rows,
mailbox cycles, recipient coverage, SMTP receipt framing, caller bounds,
account isolation and the explicit body/counter exclusions.

## 203. One-snapshot canonical account audit

`postgres_account_audit::inspect` composes section 202's shared canonical
metadata sweep, section 199's five usage-counter comparisons and section
201's body verifier for every live blob manifest. They use the same primary
REPEATABLE READ, READ ONLY transaction, expected epoch and shared migration
lock. No independent audit reports are joined. A private metadata helper
begins and retains that transaction; only the outer audit acknowledges
ROLLBACK and checks the final original deadline after dropping its worker.

After canonical metadata passes, the same transaction rejects duplicate
non-null mailbox roles within this account. A single EXISTS query groups
roles under C collation and returns one boolean; unassigned roles and other
accounts do not conflict. This adds role uniqueness, not registry membership
or full mailbox-policy validation, and retains no client-side role collection.

The caller supplies trusted UTC, metadata row/parent limits and aggregate
body count/byte limits. Body enumeration is account-scoped, ascending blob
ID keyset pagination with one manifest at a time. It includes manifests with
no Email, lease or submission owner and checks zero-length body digests.
Every manifest passes the existing extent, changed-sequence and actual chunk
count checks before the shared streaming verifier checks all bytes. Captured
identity must match on every body. Aggregate limits are checked before each
body is read. Verified body totals must agree with both metadata blob count
and the audited usage totals before a report can succeed.

The report contains historical canonical metadata, role uniqueness, counters
and verified body totals. Role registry membership, other mailbox policy,
configuration, credentials, retained operation/delivery receipts and change
history remain uncertified. This is not a complete PostgreSQL semantic audit,
restore approval, delivery permission or service admission. Failure returns
no partial report and performs no repair. The audit worker adds
`2 * max_blobs + ceil(max_bytes / 524288)` turns to the metadata allowance:
one manifest per blob, one independent rounding allowance per blob and
streamed eight-chunk reads. Final enumeration and usage/role queries fit the
fixed reserve. Body buffers retain at most eight 64 KiB chunks regardless of
the account size. The same per-turn and cumulative limits apply. Native
coverage checks each component's refusal, unowned
and empty bodies, aggregate limits and a concurrent-change oracle: while the
metadata scan is blocked after snapshot capture, another session commits
body corruption, incorrect usage, a duplicate mailbox role and a new sequence.
The in-flight audit sees only its old consistent snapshot; fresh audits refuse
the new damage. Null roles and duplicates in another account do not conflict.

## 204. Explicit bounded account audit command

`postgres account-audit --connection-file PATH --epoch HEX --account ID
--max-rows N --max-parent-reads N --max-blobs N --max-bytes N
[--timeout-seconds N]` invokes section
203's composed historical audit. Every bound is required, unsigned decimal
with at most 20 ASCII digits and checked u64 range; zero is allowed. Duplicate,
unknown, incomplete and malformed options refuse before opening the protected
connection file. There is no profile, all-accounts, force or repair option.
The command samples trusted process wall time once for metadata policy and
shares one DNS/connection/operation deadline across address attempts. Only
transport refusal permits another address, with no renewed deadline.

Successful schema-1 JSON reports account, epoch, sequence and history floor,
metadata row count, verified blob/byte totals and the five usage counters.
`canonical_metadata_verified`, `mailbox_roles_unique`, `counters_consistent`
and `body_digests_verified` are true for that historical snapshot only;
`semantic_verified:false` preserves all section 203 exclusions. The report
never approves restore or serving and grants no current authority. This
command defaults to ten seconds. Optional `--timeout-seconds` accepts one
unsigned decimal value from 1 through 3600; duplicates and values outside
that range refuse before opening files. The explicit owner timeout does not
change runtime service deadlines, per-statement bounds or the ten-second
connection/authentication ceiling.

Fixed error codes distinguish missing accounts, epochs, aggregate body
limits, metadata checks, usage integrity, body checks and database failures.
No SQL error text, credentials or body content is printed. The process
fixture verifies JSON values, absent-account and epoch refusals, decimal and
aggregate bounds, counter/body corruption and preservation of damaged rows.

## 205. Runtime maintenance refusal foundation

Migration 28 adds the owner-controlled `runtime_barrier` singleton, initially
unblocked. The dedicated runtime role has SELECT only on this table, and the
exact privilege qualification rejects broader column/table write authority.
After role and privilege checks, common runtime qualification requires exactly
one `(singleton=true, blocked=false)` row. A blocked or missing singleton
refuses with the distinct maintenance error; query/type failures also refuse.

This check applies to each new ordinary Runtime connection and retained
validators' runtime qualification. HTTPS authentication performs a fresh
maintenance fence in each job snapshot. Neither check revokes already-admitted
network I/O or make a previously captured snapshot see newer rows. Existing
operations retain their original finite limits and migration locks. Owner
Preparation schema inspection remains available while the barrier is set;
`runtime-check` remains a runtime qualification and refuses maintenance.

Runtime qualification performs no entry, release, epoch renewal, delivery
quarantine or recovery approval. Sections 206 and 207 specify owner entry.
Owner transitions hold the exclusive migration lock and
require external fencing of old instances and SMTP egress; shared locks prevent
them from racing qualified database operations. A database flag cannot establish
external fencing or decide duplicate-delivery risk. Recovery must not clear
this barrier until its separately specified quarantine, credential/configuration
and semantic-state requirements are satisfied.

Prior migration bytes/checksums remain unchanged. Upgrade is atomic, preserves
store epoch and existing state, and initializes the singleton only in the new
migration. Native coverage includes upgrade from version 27 with obstruction
rollback, blocked/missing runtime refusal, owner inspection and rejection of
runtime write privilege. Fixture-only SQL toggles do not implement the future
owner recovery transition protocol.

## 206. Monotone owner maintenance entry

`postgres_maintenance` adds bounded owner inspection and one-way barrier
entry. The caller must provide the expected store epoch and explicitly
attest that all old instances and SMTP egress are externally stopped/fenced.
The typed attestation records a caller obligation; PostgreSQL cannot prove
it. Section 207 exposes explicit preparation commands; production startup
is unchanged.

Entry forces READ COMMITTED and synchronous commit, acquires the exclusive
transaction migration lock before checking the complete current ledger, and
requires the authenticated session user to own the schema, ledger and barrier
tables. A runtime credential cannot enter maintenance even if somebody has
incorrectly granted it barrier UPDATE. Any shared or exclusive migration-lock
holder makes entry refuse busy; it does not wait for or cancel live work.
The singleton must exist and be well-formed. Entry never inserts missing
state or changes the epoch, mail, configuration, credentials or receipts.

Maintenance entry only transitions false to true. At the same epoch, observing
true is already success and performs no UPDATE. Unacknowledged COMMIT is
indeterminate, not rollback. Resolve it by inspection or repetition using the
same expected epoch. Inspection holds a shared migration lock, so an original
in-flight entry remains busy until it commits or rolls back. A false
observation permits only the same monotone entry, not runtime activation.
An acknowledged COMMIT remains known even if completing it crosses the
caller deadline; a read/no-op result requires the final deadline check.

The explicit recovery-release protocol below renews the store epoch while
clearing the barrier and quarantining pending deliveries. It requires
restored devices revoked and external configuration/fencing review; semantic
verification and subsequent profile/credential activation remain explicit. This
prevents an old entry retry from reblocking a reopened store. Direct owner
SQL that clears the barrier without renewal violates this protocol. Barrier
entry alone supplies no epoch renewal, delivery quarantine, resend decision,
backup/restore verification or authority to open service.

Native tests cover exact ownership, extra runtime grants, wrong epochs,
shared/exclusive lock contention, unchanged mail and ledger state, repeated
entry, missing state, a deferred-COMMIT timeout resolved at the
same epoch, and stale retry refusal after a fixture-simulated epoch change.

## 207. Explicit owner maintenance commands

`postgres maintenance-inspect --connection-file PATH --epoch HEX` observes
the owner barrier. `postgres maintenance-enter` requires the same arguments
and the explicit `--instances-and-smtp-fenced` flag. The flag attests that
all old instances and SMTP egress are externally stopped/fenced; it neither
performs nor proves fencing. Duplicate, unknown and missing arguments refuse
before opening the protected connection file. Inspection rejects that flag.
Neither maintenance command has a force, profile selector or epoch-generation
option. Recovery release is a separate owner command described below.

Both commands use section 206's owner and expected-epoch checks, the existing
protected bootstrap loader and one original DNS/connection deadline. Only a
transport failure before any COMMIT attempt permits another resolved address;
indeterminate entry is not silently replayed. Retain the exact epoch after
uncertainty and inspect or repeat at that epoch as section 206 specifies.

Schema-1 JSON reports the expected epoch, historical blocked state and
`recovery_verified:false`. Errors use fixed codes without SQL, credentials or
body contents; operation failures preserve the expected epoch in output.
`indeterminate-inspect-maintenance` requires explicit outcome resolution.
A false observation does not authorize opening service, and successful entry
neither rotates an epoch nor quarantines deliveries or validates recovery.
The commands expose preparation only and leave production startup unchanged.

The native process fixture covers strict fencing arguments, owner refusal,
epoch mismatch, initial/entered/repeated observations, preserved ledger epoch
and refusal without repair after a missing singleton. Section 206's API
fixture separately covers lost COMMIT acknowledgement and lock contention.

## 208. Explicit historical single-blob audit command

`postgres blob-audit --connection-file PATH --epoch HEX --account ID
--blob ID` invokes section 201's existing bounded verifier. All four flags
are required exactly once. Unknown, duplicate, missing and malformed
arguments refuse before opening the protected connection file. The command
has no repair, download or profile option.

Address resolution and body verification share the original ten-second
deadline. Only pre-connection transport failure permits another resolved
address; database or body failures do not renew work. The existing 32-MiB
body ceiling, bounded chunk batches and worker byte/frame ceilings apply.

Success JSON schema 1 identifies the epoch, account, blob, captured account
sequence/history floor, verified byte count and manifest creation time.
`body_digest_verified:true` describes that body's historical snapshot;
`semantic_verified:false` excludes counters, references, other metadata,
credentials, configuration and restore approval. No body bytes are emitted.
Missing account or blob returns the same fixed `blob-not-found` code.
Other failures retain fixed codes without SQL, database messages or secrets.
No invocation changes rows or grants device or serving authority.

The native command fixture checks exact report fields, account isolation,
missing rows, wrong epochs, corrupt and empty body digests, argument refusal
and absence of repair or credential output.

## 209. Explicit historical canonical metadata audit command

`postgres metadata-audit --connection-file PATH --epoch HEX --account ID
--max-rows N --max-parent-reads N [--timeout-seconds N]` exposes section
202's canonical metadata verifier independently of body and usage checks.
Connection, epoch, account and both logical bounds are required exactly once.
The optional timeout follows section 204's 1 through 3600 second range and
ten-second default. Limits use unsigned ASCII decimal with at most twenty
digits
and must fit u64; zero is valid and can cause a bounded scan refusal. Unknown,
duplicate, missing and malformed arguments refuse before protected-file
access. Body-limit flags belong to account-audit and are refused here.

One trusted UTC sample supplies lease-reference policy. DNS and verification
share the original selected deadline; DNS retains a five-second ceiling.
Logical limits select section 202's finite cumulative worker allowance;
no operation renews wire or time budgets. Only pre-connection transport
failures permit
address fallback. Failure emits fixed categories without SQL or secrets and
never changes a row or returns a partial success report.

Success JSON schema 1 identifies epoch, account, sequence, history floor,
canonical row count and mailbox count. `canonical_metadata_verified:true`
means the shared canonical row/reference/forest/recipient checks passed in
one historical snapshot. `body_digests_verified:false` and
`semantic_verified:false` preserve the narrower scope: body bytes, counters,
mailbox-role uniqueness/registry, credentials, configuration, retained history
and operation/delivery receipts are not certified. No serving or restore
authority follows, and separate reports cannot be combined into one snapshot.

The native command fixture checks report fields, account/epoch refusal,
row/parent bounds, malformed mailbox refusal without repair, strict arguments
and successful canonical checks despite invalid body digests, inconsistent
usage counters and duplicate mailbox roles. Those exclusions are deliberate;
the composed account-audit command supplies the broader checks in section 203.

## 210. Historical canonical retained-history audit

`postgres_history_audit::inspect` checks all physically retained change
rows for one explicit epoch/account in one primary REPEATABLE READ,
READ ONLY snapshot under the shared migration lock. It captures the account
endpoint and history floor before scanning, requires floor <= endpoint and
accepts an explicit total row limit. Zero is valid only when no rows remain.
There is no command, device authorization or serving activation in this
increment.

The audit and authenticated change reader share one canonical row decoder:
exact eight-byte nonzero unsigned sequence, operation 0..4095, supported
Mailbox/Thread/Email/EmailSubmission kind, Created/Updated/Destroyed action
and exact sixteen-byte object ID. Identity changes refuse. Both consumers
check strict cursor order and the captured committed endpoint. Account
keyset batches contain at most 128 rows, never a complete retained history.

Rows at or below the floor may await physical reclamation and are valid.
The audit validates them too and reports their count separately. A success
reports only the captured identity and total/retired row counts after
acknowledged ROLLBACK and a final original-deadline check. It does not prove
that events are complete, actions match live objects, operation ordinals are
contiguous, transaction history can reconstruct current state, or retained
receipts are valid. No body, metadata, credential, restore or serving
certificate follows. Separate reports are separate snapshots.

The existing body-worker wire/frame ceilings and ten-second deadline cover
the entire scan; a caller row allowance does not renew them. Exhaustion,
malformed rows, future or zero sequences, wrong epoch, missing account or
database failure refuse without repair or a partial report. Native fixtures
cover multiple batches, exact row bounds, retired rows, account isolation,
corrupt sequences, deliberate event-completeness exclusions and a concurrent
commit after snapshot capture. The authenticated changes fixture qualifies
the shared decoder through normal transaction-aligned paging.

## 211. PostgreSQL foreground service

The active serve command loads and validates the selected database profile
and uses PostgreSQL SMTP storage, HTTPS authentication and JMAP adapters.
CONFIG.md owns syntax and local runtime/scratch placement. Receive and JMAP
roles are supported, including a JMAP-only instance and a separate relay.
The embedded backend, local-store commands and file-backed device registry
are removed. This is not a live migration or a complete release qualification
claim.

## 212. Foreground relay scheduling

A relay profile runs one fixed SMTP worker and one lease-renewal worker.
It pins the startup primary address and published configuration/TLS generation;
there is no transparent primary fallback or configuration reload. Every SMTP
phase retains the existing current-policy, route-generation and attempt fences.
Renewal failure stops the service, and the original bounded SMTP attempt closes
before joining workers and releasing the lease. Busy ownership is standby.

The worker reserves two ordinary database slots, two independent body-source
slots, one separate settlement slot, and two reusable TLS wire buffers. It polls bounded candidate pages, alternating
fresh and retry work, and admits at most one attempt per turn. The existing
100-recipient batch and ten-second initial admission cap remain in force; a
120-second outer budget cannot extend that shorter cap. Wire, database and
source meters each have bounded 1-GiB I/O/output and two-million-record budgets;
these are work ceilings, not resident allocations. Exhaustion refuses work.

Passive active-attempt discovery is fenced to the current account, route and
destination and reads at most two rows, refusing multiple active owners.
Restart recovery uses the previous generation; locally abandoned Prepared
attempts use current-owner interruption after transport closure. Possible
acceptance remains uncertain. Known MAIL/DATA/final refusal and RCPT evidence
are settled before generic recovery. Unknown COMMIT retains the exact Pending
owner and blocks subsequent work until its receipt is observed. No absent
receipt authorizes replay. A requested stop may abandon unresolved custody;
durable phase recovery on restart remains conservative.

The process starts the five-minute retry guard from database UTC and its one
monotonic clock. New mail may dispatch while retries wait. Expiry scans precede
dispatch; the private queue clock confirmation operation in CONFIG.md enables
at most one 100-recipient expiry batch per second. Overdue work remains held
until confirmation. Authentication and TLS failures publish a route pause;
network failures, including DNS idle timeout while the original cap remains
live, publish route backoff. A local exhausted cap does not diagnose the route.
Durably accepted final DATA resets consecutive route failures. Success and
repair retain exact operations and inspect unknown COMMIT before further work.
After selecting a repaired configuration and restarting, an explicit private
`queue-route-repair` request releases the configuration-bound pause only if
the database confirms a newer selected generation. Mere restart never repairs.
The worker scans the verified immutable transmitted source under its original
Prepared cap to select SevenBit or EightBit before MAIL; DATA requires the
matching EHLO capability. This scan uses fixed 4096-byte scratch and prepays
three maximum-body extents plus database validation overhead, with charged
records per read. There is no complete-message allocation.


## 213. Local terminal-delivery notices

The shared foreground relay worker attempts at most one pending local notice
per account every five seconds, independently of route backoff, repair holds and expiry clock
confirmation. Only already completed submissions with notification Pending
qualify. The current selected relay profile and live route lease authorize
its account; an older submission destination does not strand local notices
when the smart host changes. No notice enters the outgoing submission queue.

The publisher holds the configuration, account and route fences while it
reads up to 1000 terminal recipients in pages of 100 and builds one plain-text
report with the shared compose_text MIME writer. The report identifies the
submission and original Email, gives each recipient's outcome/reason, and
includes retained RCPT/message replies and diagnostics. Unknown outcomes say
that delivery may have occurred. Peer control characters are replaced with
question marks in the text body; no peer text becomes a header. The generated
message uses a local system sender, its own Message-ID and a fresh thread.

One transaction writes the immutable blob, unread Inbox Email, membership,
thread/anchor, quota charge and CHANGE rows for Email, Thread, Mailbox and
Submission, then changes Pending to Stored with its historical Email ID.
It preserves completedAt, recipients, their replies and uncertainty. Account
sequence admission preserves outstanding final-result reservations. The
existing shared body/blob quotas apply; missing Inbox, quota/storage failures
or bounded-work refusal leave Pending for a later turn and do not stop SMTP
scheduling. Errors are redacted fixed enums in the service log.

Stored is the durable receipt. Every subsequent scan first obtains the same
account lock, so it waits for any older commit/rollback before selecting
Pending again. A crash or lost COMMIT response therefore cannot create a
second notice. The marker retains the Email ID after JMAP deletion, so missing
Email bytes are not a reason to republish. No extra receipt table, schema
upgrade or file-backed continuation is introduced.

Each turn uses one ordinary database slot and a five-second absolute deadline.
Raw report text is capped at 10 MiB and the MIME message at 16 MiB; each SQL
page holds at most 100 rows of schema-bounded recipient fields. The private
wire adapter bounds aggregate input at 12 MiB/4096 frames, retains the existing
128-KiB per-frame/authentication limits, and bounds output at 20 MiB. Composition
has one 128-MiB I/O, 64-MiB output and two-million-record meter, never refreshed
inside a turn. These are work ceilings; the report/message and bounded SQL
page are additional transient memory, released before the next SMTP attempt.

Lease renewal permits an eight-second database lock/statement wait inside
its original ten-second client deadline, so a five-second notice turn can
outlast the ordinary one-second lock timeout. It still checks the current
owner and unexpired lease under the acquired locks before extending it;
waiting never authorizes renewal of an expired or replaced owner. Storage
that exceeds this tolerance still stops the service under the existing
lease-loss policy.

After a notice error, including an indeterminate COMMIT, the scheduling
worker also performs
that bounded renewal before ordinary queue reads. Its account lock waits
behind the notice transaction (COMMIT or rollback) and establishes
settlement while rechecking
ownership. A pending COMMIT therefore cannot trip the next candidate
read's shorter lock timeout. Failure to establish current ownership still
stops the service; the next notice scan uses the durable Stored marker.

## 214. Live JMAP cancellation across processes

The HTTP service supplies an authenticated cancellation adapter for a
singleton `EmailSubmission/set` update with `undoStatus: "canceled"`.
A JMAP-only profile can cancel account submissions while the relay runs in
another process or is offline. The frontend preparation binds the account,
selected configuration, device, submission, optional state and random
operation. It does not manufacture a relay claim or acquire its lease.
The existing internal route-owner API retains its owner and local-close
checks.

Frontend publication holds the selected-policy and account locks, verifies
the current credential, and uses the same complete-group reservation,
recipient validation, attempt retirement, change record and receipt as the
route-owner API. It can cancel old route destinations belonging to the same
account. Any Accepted, uncertain or AcceptancePossible recipient refuses the
whole operation as cannotUnsend. It never promises recall of delivered mail.
An eligible Prepared or Body attempt becomes inactive in that transaction.
Every later relay phase/terminator admission uses that same account fence,
so a remote worker cannot publish AcceptancePossible after cancellation.
The worker closes on its next failed admission; already buffered partial
body bytes are not a completed SMTP delivery. No cross-process socket
control or new database request queue is needed.

The scheduler treats a missing fenced phase publication as a retired
attempt after settling any unknown receipt, rather than retrying it forever.
The unchanged recovery path likewise tolerates retired attempts. A canceled
attempt does not clear route failures as a successful delivery.

The HTTPS worker retains one unknown operation. Before another cancellation
it charges and inspects that receipt under the original request/meter
limits. Receipt inspection takes the account lock behind the unknown COMMIT:
Stored confirms success; absence proves rollback and releases the frontend
pending slot. Restart needs no local journal: durable canceled state is an
idempotent update, and a rolled-back request can be issued again. No password
or transport callback is retained. Existing route-owner callers retain their
historical settlement contract.

Native qualification covers both prepared-owner and frontend JMAP dispatch,
state mismatch, cannotUnsend, a thousand recipients, repeated cancellation,
and lost COMMIT response settlement. The executable fixture uses separate
JMAP/relay processes, cancels queued work with the relay offline and an
active attempt before MAIL's reply, checks transport closure before another
command, refuses accepted mail, and includes restart and account auditing.


## 215. Structured attachment sending

The live draft owner accepts td-mail's uploaded attachment descriptors and
composes multipart/mixed with its existing text part. Each source is read
through the verified PostgreSQL blob reader under the selected account and
current device. An Email/submission reference or that device's unexpired
upload lease is required. The source bytes, length and digest are checked
before composition. Publication rechecks each source manifest and access
under the account lock with fresh device/configuration authorization; a
lease that expired during composition cannot authorize publication.

A retained Draft includes only the composed body and bounded source manifests,
whose IDs, lengths and digests enter the operation digest. Unknown COMMIT
uses the existing exact draft receipt and pending owner; successful receipt
lookup remains historical evidence, independent of later source expiry.
The input uploads retain their ordinary lease/cleanup lifecycle. Draft
publication creates one new composed blob, not references to mutable parts.

There are at most 64 attachments, with filename/type/description byte caps
of 255/127/1024. Header controls and MIME parameter injection are refused.
Names use RFC 2231 UTF-8 continuations; descriptions use the shared encoded
header writer. Ordinary payloads and message/global use the shared base64
encoder. message/rfc822 retains identity 8bit encoding and exact bytes,
and its enclosing multipart explicitly declares 8bit,
requiring CRLF, no NUL, lines no longer than 998 content octets, and no
collision with the random 128-bit enclosing boundary. It is not silently
rewritten or base64-encoded: the message/global allowance in
[RFC 6532 section 3.5](https://www.rfc-editor.org/rfc/rfc6532.html#section-3.5)
does not change message/rfc822's restrictions. Other message subtypes and
multipart attachment types are refused.

One original ten-second-or-shorter request deadline covers resolution,
source verification, composition and publication. Each body read reserves
two foreground connection permits and prepays its bounded verification and
replay work before I/O, including fallback addresses. Aggregate decoded
source bytes and composed bytes each stay within the selected message
ceiling (at most 32 MiB); raw sources are dropped before publication.
RESOURCES.md records this additional transient memory and work admission.

The native td-mail send fixture uploads binary and forwarded-message files,
composes/sends them, reconciles a retained send without duplication, and
compares downloaded attachment bytes and UTF-8 names. It refuses another
device's upload and a lease expired after preparation without publishing an
Email. The foreground-service fixture uploads through HTTPS and verifies
binary attachment DATA at its local TLS smart host. Unit cases cover empty
parts, invalid metadata, source-line/boundary refusals and exact output caps.


## Empty folder deletion

The `postgres_mailbox_set` transaction also accepts one prepared deletion.
A distinct request-digest domain binds the target (or a valid foreign ID's
absent local target), device, selected binding and optional expected state.
It shares creation's account lock, current-device authorization, sequence
checks, bounded receipt allowance and uncertain-COMMIT owner. The existing
`mailbox_create_receipts` table stores both operations without a schema change:
17-byte result tags 4 through 8 represent destroyed, notFound, forbidden,
mailboxHasChild and mailboxHasEmail. Created results retain tags 1 through 3.

Only an empty, roleless leaf may be removed. Any role is protected; children
and message memberships refuse deletion. A successful deletion removes the
folder, adds one Mailbox destroyed history row and advances the account
sequence once in the same transaction as its receipt. Refusals retain the
old state. Replay and inspection return the exact historical result, without
performing a second deletion or granting fresh authorization.

The HTTP profile accepts a pure singleton destroy with optional ifInState.
Mixed create/destroy, nonempty update, multiple destroy IDs and
onDestroyRemoveEmails=true refuse before publication. Response capacity is
reserved before mutation. Only a confirmed commit emits destroyed; uncertainty
retains the prepared operation and fences subsequent folder writes until
receipt settlement. The real-client fixture covers deletion, missing and
foreign IDs, protected Inbox, nonempty folders and parents. Native transaction
checks cover stale state, revoked authorization and exact receipt replay; a
delayed deletion COMMIT exercises owner settlement and one history row.


## Application diagnostic retention (schema 29)

`postgres_logs.sql` adds `application_log_state` and `application_logs`.
Both require runtime SELECT/INSERT/UPDATE/DELETE, without ownership, DDL,
TRUNCATE or grant options; existing owner/runtime separation is unchanged.
Stop all service instances for the explicit schema upgrade, revoke runtime
schema/table/column privileges as required by `schema-upgrade`, upgrade with
the owner connection, reapply the exact runtime grants (including these two
tables), run `runtime-check`, and restart on the matching executable. Never
edit an existing migration checksum. Mail tables and retained mail are unchanged.

OBSERVABILITY.md owns the 30-day age policy, fixed ring, rate/byte limits,
loss counters and failure isolation. These records use asynchronous commit,
may be lost or overwritten, and are never a source of delivery, recovery or
publication authority. Log inspection is bounded and epoch-qualified:

```text
td-mta postgres logs --connection-file /private/owner-connection --epoch HEX --limit 100
td-mta logging-status --runtime /private/runtime
```

Use the last returned `before` ID for the next page. The log relations have
a physical high-water stop in addition to live-row bounds; PostgreSQL WAL,
archive/replication retention, backups and whole-volume headroom still require
normal operational limits and monitoring. A stopped diagnostic sink must not
be treated as a reason to resend mail.

## 8.121 Native ACME state and certificate pairs

Schema 30 adds `acme_profiles`, sixteen fixed slots containing bounded private
account/order state and one active certificate/key pair per named profile.
Runtime qualification grants ordinary data access to this table; immutable
operator configuration/material tables retain their existing read-only grants.
[ACME.md](ACME.md) specifies the implemented single-endpoint routing restriction,
lease/revision protocol, endpoint policy and retry/retention bounds.

Claims check the schema/store epoch and serialize only short slot/ownership
transactions. Every save matches the random owner, expected revision and
unexpired database lease. Publication changes the active pair and pending state
in the same statement. Uncertain outcomes are reloaded on the next ownership
turn. No CA connection runs inside a database transaction. Managed material
loaders fetch and pin the complete active pair once per profile and verify its
settings digest against the configured directory, contact and exact name set.
Different chain/key reads cannot observe different renewals. No arbitrary file
path, material version, configuration selection or mail-delivery authority is
created by this mapping.

## 8.122 Restored-store delivery quarantine (schema 31)

`td-mta postgres recovery-release` is an explicit owner operation after
`maintenance-enter`. Keep old instances stopped and SMTP egress fenced
throughout the recovery. The flags attest operator actions; they do not
establish a network fence. This command never starts a service.

Retain a fresh random 16-byte new epoch before publication, alongside the
restored epoch. First inspect the restored accounts, bodies, counters and
configuration, and revoke **every** restored device with `device-revoke`.
The release refuses any still-active device, missing maintenance barrier,
wrong epoch, invalid account floor, inconsistent pending submission or
relation exceeding `--max-rows` (1 through 1,000,000). The bound applies to
each rewritten mail/queue relation; the command retains its ten-second
deadline. Failure before COMMIT rolls the entire operation back.

```text
td-mta postgres recovery-release --connection-file /private/owner-connection \
  --epoch OLD_HEX --new-epoch FRESH_HEX --max-rows 100000 \
  --instances-and-smtp-fenced --configuration-reviewed
```

Release changes Queued, InFlight, RetryWait and retryable OutcomeUnknown
recipients into terminal OutcomeUnknown with explicit uncertainty and the Restored reason (schema 31).
A backup may predate every attempt: this reason also permits an unattempted
recipient with phase None, without inventing attempt history. Release clears
retry times, completes
those submissions and requests only local failure notices. Accepted, failed,
canceled and already-terminal unknown results stay intact. Replies and attempt history
remain available; active attempts, final reservations and route ownership are
retired. Bodies and usage counters remain unchanged. Account history floors
advance to current sequences, preventing access to pre-recovery incremental
history under the new epoch. The ledger epoch changes and the maintenance
barrier releases in the same transaction.

Retain both epochs after an indeterminate result. Wait for the operation to
settle, then inspect the schema and maintenance barrier. A new epoch with an
open barrier identifies committed release; the old epoch with a closed
barrier permits retry with the **same** new epoch. A busy migration lock is
not a rollback. Any other state requires investigation. Never substitute a
new epoch to hide uncertainty.

Configuration encodings are epoch-bound: existing selected profiles fail to
load after release. Import the reviewed private configuration sources as a
new version under the fresh epoch, then explicitly select it using the next
selection generation. Retain old configuration versions and receipts. Create
fresh device credentials and verify mailbox reads before opening ingress or
SMTP egress. Separately review database/bootstrap credentials, relay secrets,
ACME account state and all changes or deliveries after the backup boundary.
Quarantined submissions never retry automatically; deliberately submitting a
new copy requires accepting duplicate-delivery risk.

A release receipt reports quarantine counts, not full semantic integrity or
successful service activation. The isolated restore fixture covers the
archive round trip, rollback and ambiguous commit, epoch change and pending
recipient quarantine. Hosted activation and off-host backup durability remain
separate operational checks.

## HTTPS account selection

The fixed HTTPS authentication worker returns an account/device principal.
The network reactor selects a backend by that authenticated account, and
retains the principal until the request ends. Backend registration is bounded
to 16 distinct account IDs. Session discovery, upload, download, JMAP API and
event snapshot dispatch all use that selection. URL or method argument account
IDs never select authority: the selected account-scoped backend retains its
existing target-account checks, fresh device checks and publication fences.
Credentials remain request-owned and zeroed on drop; no worker pool, read
capacity, body-operation concurrency or authentication-rate allowance is
multiplied by the number of accounts.

Cold preparation constructs a bounded binding registry, verifies every
configured account's bootstrap, and prepares only that account's sending
identities and visible catalog in its binding. One final selection observation
fences the entire registry. Publication retains the bindings with the same
configuration and TLS generation; an absent account never falls back to the
first binding. The compatibility binding accessor is for common profile facts,
not request dispatch. Extracting a singleton binding refuses multiple accounts.

JMAP profiles register one Session and backend per account. All backends
share read slots, pending-mutation ownership, response admission and body
operation capacity. Each Session names only its authenticated account. Three
MiB of startup metadata reservation covers the aggregate catalog preparation,
bindings and up to sixteen Session documents. All serving roles use this
bounded account registry.

### Retained HTTPS authentication sessions

The fixed authentication worker retains at most one PostgreSQL session across
requests, shared by every configured account and origin. Its adapter binds to
the first calling thread, uses a nonblocking mutex with no waiter queue, and
refuses another thread. Credentials and account/device authority remain
request-owned; the retained session caches neither authentication results nor
configuration or maintenance observations. Full schema and Runtime-role
qualification belongs to connection admission, protected by the shared
session migration lock held until that connection closes.

Connection admission acquires the shared session migration lock once, inspects
the complete schema ledger and qualifies primary/durability, Runtime-role,
exact privileges and maintenance. Schema and role administration stops all
serving instances first; it must not modify the protected contract behind a
live connection. PostgreSQL continues to enforce each statement's privileges.
The closed SQL surface cannot exploit newly added privileges. These checks
repeat on every reconnect, including recovery after an idle disconnect.

Each job begins a new READ ONLY REPEATABLE READ transaction. One typed query
checks the pinned store epoch and current maintenance barrier; selected
configuration, presented device and account snapshot remain freshly observed.
The migration lock predates every job snapshot, preventing an upgrade from
crossing connection qualification. Success and wrong-password refusal both
require acknowledged ROLLBACK and final deadline/window checks. No transaction
or account lock survives between jobs; the one shared migration lock does.
Wrong-password refusals retain the clean connection without caching an
authentication result. SQL failures, failed cleanup, expired jobs and framing
failures retire it. Before submitting a later job, the adapter polls the clean
idle driver without queuing SQL. An already-observable idle failure permits
one fresh connection under the original deadline. No failure after job
submission retries that job; a disconnect racing the idle check can still
fail the request.

A session admits at most 64 jobs within sixty seconds of creation. Each job
keeps its own original deadline, capped at ten seconds including initial DNS,
TCP, TLS and SCRAM. Before starting a later job whose complete allowance would
outlive the session, the adapter closes the old connection and performs fresh
DNS/TLS/SCRAM admission. The existing fixed authentication worker checks local
idle state once per second, outside client-slot locks, and retires a session
when fewer than ten seconds of its original lifetime remain or its job count
is exhausted. This maintenance performs no authorization query, driver health
poll or new task. A shutdown guard closes retained resources on that same
worker before it exits, including early failure paths.

Only a clean idle session whose job completed with acknowledged rollback
and final checks may use a separate termination allowance. Consuming
retirement bounds that allowance by the caller deadline, original session
expiry and the existing 100-ms driver cleanup ceiling. It cannot perform
another job, renew TLS or replenish byte/frame windows. Failed or unfinished
jobs retain their original deadline and sticky failure behavior. Cleanup is
best effort if the peer fails or scheduling crosses the immutable TLS expiry;
no cleanup outcome changes an already completed authentication result.

Authentication and the first job share the initial one-MiB input, 1088-frame
and 64-KiB output window. A completed job admits another such window while the
sixty-four-MiB input, 69632-frame and four-MiB output session totals only
decrease. Per-frame and SCRAM limits remain unchanged. Available idle messages
consume the preceding window before the next starts; parser state, connection
authentication and sticky failures are never reset. Windows are work limits,
not resident allocations. The existing one-authentication-worker capacity
covers the retained socket and TLS buffers, independently of account count.

API reads, downloads, post-verification reauthorization and mutation connections
retain their existing admission and teardown paths. This increment changes no
write retry, uncertain-COMMIT or receipt-settlement behavior. Native fixtures
check 64 jobs on one backend, rotation, idle migration-lock retention and
release on close, wrong-thread refusal, fresh policy/epoch/maintenance checks,
statement privilege enforcement, cold ledger/excess-privilege qualification,
credential refusals and idle disconnect recovery. A focused fixture reports cold and warm authentication times; those
measurements do not establish end-to-end HTTP or download performance.


## Transactional incoming delivery across accounts

An accepted SMTP envelope retains each address's pinned local account. The
fixed receiving worker groups those recipients by account, preserving accepted
spellings and filing aliases only once per account. Unknown or external
recipients are refused before DATA; no SMTP AUTH or forwarding authority is
introduced. Startup and DATA admission inspect the configured Inboxes in one
bounded connection and transaction, holding the selection and sorted account
locks. Account count does not multiply authentication connections or extend
admission deadlines.

`postgres_receiving::Fanout` retains one operation, shared prepared DATA and
header-anchor storage, and private provenance for at most sixteen accounts.
Preparation validates the complete recipient partition, sorts account IDs and
binds every account and its encoded provenance into one request digest. Each
account receives only its own recipient metadata. Original body preparation,
work meter and deadlines remain shared; SQL publication charges body I/O for
every target before opening the transaction.

Publication locks the current selection, then every account in ID order before
reading receipts or writing any email. It uses the existing account write path
for quotas, Inbox, thread, body, history and receiving receipts, and commits
once after every target succeeds. A quota or Inbox refusal rolls everything
back. Receipt lookup holds all account locks in the same order and requires
either all matching receipts or none; a subset or digest mismatch refuses.
Historical complete receipts remain settlement evidence after selection changes.
The owner retains the entire operation after an uncertain COMMIT and stops new
SMTP publication until settlement. Final protocol success verifies that the
returned account set exactly covers the accepted envelope.

The closed fanout driver admits 40 MiB outgoing bytes, 1 MiB incoming bytes
and 1088 response frames per target account, for at most sixteen accounts.
These are cumulative transport limits, not resident allocations; the existing
per-frame, authentication, TLS and ten-second connection limits remain intact.
Receipt inspection uses the same bounded account-count allowance. The DATA
worker's output meter includes the separate 2-MiB fanout metadata reservation,
including when the configured message ceiling is smaller than that metadata.


## Foreground relay scheduling across accounts

One foreground worker rotates through at most sixteen configured accounts.
A separate expiry cursor retains an account’s turn across local or database
rate-limit refusals, advancing after an admitted attempt or no due work.
Accounts without a live lease are skipped. This prevents a busy account from
repeatedly consuming the shared expiry allowance ahead of later accounts.
Each turn settles prior custody, checks periodic local failure notices and
explicit repair work, checks expiry and interrupted attempts, and selects at
most one SMTP attempt. Fresh/retry selection alternates independently for each
account. Busy leases and paused routes advance the cursor. A complete empty
pass backs off its following pause from 250 ms through 500 ms and one second
to a two-second cap. Any observed candidate, interrupted attempt, pending
settlement, explicit repair, published notice or notice failure restores the
250-ms pause for that pass. This is a polling hint, never cached queue absence
or delivery authority. Fresh/retry alternation can require two idle passes
to discover newly queued work; database work and other accounts add latency.
Lease renewal keeps its independent five-second schedule, and shutdown still
checks at most every 50 ms during a pause. An SMTP attempt retains its existing 120-second
bound, so another account may wait behind a slow peer. This is bounded rotation,
not a per-account latency guarantee. Unsettled publication prevents rotation.

After settlement, a turn without pending custody or an explicit repair request
first makes one owner-qualified absence read. It checks unfinished submissions
and active attempts for that account, route and destination, plus completed
pending failure notices anywhere in the account. An empty result replaces the
separate notice, expiry, recovery and candidate reads for that turn only. Its
fresh database UTC still feeds the queue-clock guard and explicit confirmation;
the empty expiry turn advances and fresh/retry selection alternates. No absence
survives into the next turn. A positive result uses the existing fenced paths
and does not itself mark the round active: delayed or paused work must not pin
polling at its shortest interval. Notice scheduling is unchanged until an actual
notice check occurs. New work committed after the hint waits for another turn.

When every configured account has a renewed owner lease and its absence read
finds no unfinished submissions, attempts or pending notices, the next pause
uses a thirty-second fallback instead of the ordinary two-second cap. The
retained reader autocommits `LISTEN td_mta_relay` before its first absence read.
Submission publication executes `NOTIFY td_mta_relay` inside the same transaction
as its rows and receipt, before the existing COMMIT outcome marker. PostgreSQL
delivers the hint only on commit. A notification failure never permits an
acknowledgement without durable publication, and uncertain COMMIT retains the
existing settlement rules. No schema migration or new privileges are required.

The channel has an empty payload: every subscribed relay profile may wake,
then uses its own bounded account rotation and current authorization. Driver
polls coalesce matching notifications into one boolean, including those read
during SQL. Unrelated channels and nonempty payloads are ignored. Existing
finite input frame/byte budgets still apply; no notification queue is allocated.
An idle pause blocks on the existing socket in at most fifty-millisecond slices
and issues no SQL. Stop, repair and queue-clock confirmation requests interrupt
it. A disconnect, missing subscription or finite session retirement also ends
the pause; replacement subscribes before scanning again. Only a complete
rotation on one subscription admits the extended pause, so a reconnect between
account scans cannot hide work from an earlier account behind that pause. A complete lost hint
is recovered by the thirty-second fallback, even without another submission.
The database scan remains authoritative and no hint authorizes delivery.
An observed notification or subscription-recovery wake resets the ordinary
backoff to 250 ms without changing fresh/retry alternation. A fresh arrival
during a retry turn therefore avoids retaining a two-second empty-queue delay
before the next fresh turn. Empty socket slices do not reset the backoff.

Any delayed retry, paused or unfinished submission, active attempt, pending
notice, Busy read or account without a renewed lease keeps the original
250-ms-to-two-second polling schedule. Thus notifications do not postpone
scheduled retry/expiry work or owner acquisition. Lease renewal remains on its
independent five-second schedule. A notification storm can cause extra scans,
but each round retains its bounded account work and protocol input limits.

The process retains one claim operation, boot identity and owner generation per
account instead of acquiring and releasing on each turn. Historical acquisition
receipts require fresh owner observation and successful renewal before dispatch.
Newly acquired accounts enter a bounded registry and wait for renewal acknowledgement.
The renewal worker snapshots that registry without holding its mutex over SQL,
then renews all registered leases in one transaction every five seconds or when
new claims arrive. It locks configuration, sorted accounts and then owner rows,
samples database UTC after locks, and validates all generations before updating.
A stale member rolls the batch back. All I/O shares one ten-second deadline;
uncertain renewal stops the process. Lock contention on one account can therefore
affect this process's availability. It cannot grant stale transport authority.

The foreground worker retains one qualified idle-reader connection across
account turns and empty-round pauses. It holds one existing ordinary read
permit until connection close. The shared finite read-session implementation
also backs HTTPS authentication: at most 64 jobs in sixty seconds, immutable
TLS lifetime, finite cumulative/per-job wire windows, one owning thread and
ten-second job deadlines. Connection admission qualifies schema and runtime
role once and holds the migration lock until close. Each idle job begins a
fresh READ COMMITTED transaction, checks epoch and maintenance, locks current
configuration/account/owner rows, and acknowledges ROLLBACK before returning.
No account lock, transaction or absence observation survives the job. Idle
checks retain no authority to send. Failure retires the session; an observable
idle disconnect can reconnect before a job, never replay an admitted one.

A lock timeout on the idle job's configuration/account lock is a Busy
observation, not evidence of work or permission to send. The failed read
retires its session and releases its permit. The foreground worker defers
that account until a later bounded scheduling round and yields any owned
expiry turn so other accounts can progress, with the same empty-round backoff; it records `relay_poll_deferred` with `storage_busy`. Other idle
failures still stop the worker and record `relay_step_failed` at `idle_poll`.
Lease-renewal errors remain fail-stop and are recorded at `lease_renewal`;
no renewal, publication or uncertain COMMIT is replayed by this policy.

The worker closes this reader before pending settlement, explicit repair,
positive-hint processing, delivery custody and final lease release. Rotation,
shutdown and error cleanup close the connection before releasing its permit;
clean idle cleanup has the existing bounded termination allowance. Each loop
also retires exhausted or near-expiry idle sessions even when no account owns
a usable lease; this local maintenance issues no SQL. Account
count does not multiply retained sessions or extend any allowance.

The standalone absence helper, initial foreground renewal and final release
consume callback-scoped clients. Periodic renewal retains one qualified mutation
session on the renewal thread, sharing the SMTP mutation lifecycle but not its
publication wire allowance. Its sixteen-turn, sixty-second lifetime has finite
cumulative bounds and the existing per-job limits: 128 KiB output, 1 MiB input
and 1088 input frames. No account or transaction is retained between renewals;
the migration lock remains held until connection close. Each new READ COMMITTED
transaction checks epoch and maintenance before the existing fresh configuration,
sorted account and owner checks. Lock and statement timeouts are transaction-local
and remain eight seconds within the original ten-second operation deadline.

Observable idle failure or exhausted lifetime permits replacement only before
SQL admission, under the same deadline. Any admitted error retires the connection
and preserves the worker's fail-stop path; no renewal is replayed. COMMIT is
marked immediately before its attempt. An unknown outcome remains Indeterminate;
a positively acknowledged COMMIT survives later cleanup failure, which retires
the session. Exhausted, retired or near-expiry sessions close after the job,
before the renewal worker waits again. Shutdown closes the thread-owned session
before joining. Initial
renewal is deliberately one-shot so a retained session never crosses threads.
Preparation connections still inspect the schema at transaction entry.

Registry acknowledgements match the exact claim operation and generation.
Published slots are never replaced while the worker runs. One fresh database
UTC sample initializes the shared queue-clock guard; historical receipt time
and later account acquisition never reset it. Renewal timestamps do not feed
that guard out of order. Existing five-minute restart/retry and explicit expiry
confirmation rules continue across the entire process.

An explicit route-repair request remains pending until every configured account
has been visited. An account without a repair hold completes a fresh observation;
a held account still requires the existing newer-configuration and receipt
fences. A busy account leaves the request pending, and a refusal is visible.
A request serial prevents completion of an earlier refused request from finishing
a newer request. No timer clears a repair hold.

Shutdown closes foreground custody and joins renewal before one bounded
multi-account release transaction. Expiring leases remain the fallback if cleanup
cannot complete. Claim receipt counts do not grow with idle scheduling turns.
Account-scoped route ownership and health match the existing database keys;
there is no schema migration or global smart-host concurrency promise.

## Account administration workflow

`postgres account-list --connection-file PATH [--epoch HEX]` reports the
selected generation/version and account, receiving-alias and sending-identity
records. Account IDs remain stable when the name or login username changes.
The record `section` distinguishes account, alias, identity and identity_address;
`id` is the stanza label. Names and addresses are JSON-escaped. This is a
configuration observation, not proof that a process serves that generation.
Current listing and editing revalidate referenced materials; expired or invalid
certificates can prevent them, as for config-check.

The following owner-side commands replace manual source decoding and SQL:

```text
td-mta postgres account-create --connection-file PATH --address EMAIL \
  [--name NAME] [--username USER] --operation-file PATH
td-mta postgres account-update --connection-file PATH --account ID \
  [--name NAME] [--username USER] [--outbound-profile NAME] --operation-file PATH
td-mta postgres account-alias-add --connection-file PATH --account ID \
  --address EMAIL --operation-file PATH
td-mta postgres account-alias-remove --connection-file PATH --account ID \
  --address EMAIL --operation-file PATH
td-mta postgres account-apply --connection-file PATH --operation-file PATH
td-mta postgres account-operation-inspect --connection-file PATH --operation-file PATH
```

Create/update/alias commands accept an optional `--epoch`; otherwise they
inspect the database epoch before preparing the operation. Create defaults
name and username to the address. The address's domain must already be
configured. It generates an account, Inbox and sending identity, and adds an
explicit receiving alias. Other folders can be created by the client. An
alias alone never grants permission to send from that address. Update changes
only supplied name, username and outbound-profile fields, not aliases, identities, credentials,
account IDs or stored messages. No destructive account deletion is offered.
These commands retain the existing 16-account bound.

The operation file must not exist and must have a checked private mode-0700
parent with trusted ancestors. It is created exclusively as mode 0600, then
both file and parent are synced **before any database mutation**. A bounded,
versioned, SHA-256-checked record retains the exact account/Inbox IDs, source
bytes, configuration version and compare-and-swap selection request. It
contains configuration material references, not resolved material secrets.
The checksum detects corruption, not a malicious operator; PostgreSQL owner
credentials remain the authority. Source input remains bounded to 2 MiB;
operation framing allows another 2048 bytes. No subprocess implements editing
or SQL, and unrelated source bytes/comments are retained.

Each command validates the complete edited configuration, bootstraps a new
account if required, stores the immutable version, and selects it against the
original observed generation. A first-to-second-account change makes the old
singleton postmaster explicit in every profile where it was implicit. Every
profile still passes the ordinary routing, identity, material and quota checks.
Concurrent changes return a conflict rather than silently rebasing.

These steps are resumable, not one cross-operation transaction. Failure can
leave bootstrapped account storage or an unselected configuration version;
it cannot partially select a configuration. Keep the operation file and use
`account-apply` after interruption or an uncertain result. It reuses the exact
bootstrap and version IDs, syncs the loaded file and parent before writes,
and resolves the selection receipt before current
material validation. Never remove the file and retry create with fresh IDs to
resolve uncertainty. A missing receipt is not proof an earlier request cannot
still commit. A known generation conflict requires a newly prepared edit of
current configuration; retain the original for inspection.

Successful selection fences old instances; restart **every affected mail
instance** before considering the account available. Output always says
`profile_activated:false` and marks receipts historical. Reapplying an old
successful operation reports its original receipt without replacing a newer
selection. It does not prove that version is still selected. Credentials use
the existing `device-create`, `device-list` and `device-revoke` commands; a
username change does not revoke them.

Native tests exercise executable create/list/alias commands, protected
operation permissions, first-to-second postmaster preservation, exact replay,
conflicting edits, and selection commit uncertainty. They verify unrelated
accounts and identities survive and account storage is not duplicated.

### Sending identity administration

`postgres account-identity-add --connection-file PATH --account ID --address
EMAIL [--name NAME] --operation-file PATH` creates a fresh sending identity;
name defaults to the address. `account-list` reports its generated identity ID.
`account-identity-update` takes `--account ID --identity ID` and at least one
of `--address EMAIL` or `--name NAME`; an empty name is allowed. It preserves
the identity ID, omitted fields, signatures and Reply-To/Bcc lists.
`account-identity-remove` takes `--account ID --identity ID` and removes that
identity and all of its address-list entries. Update/remove require the
identity to belong to the named account. All three commands require the same
connection and operation-file options and accept optional `--epoch`.

These commands use the same retained-operation, validation, CAS, replay and
restart contract above. They never add or remove receiving aliases, create
account storage or provision credentials. A sending address need not be a
receiving alias or use a locally received domain; the operator is responsible
for authorizing it with the outbound provider. Normal mailbox syntax and the
aggregate 64-identity limit still apply. An account may become receive-only
when its last identity is removed, but the configuration must retain at least
one identity globally. Removal prevents future submissions after activation;
it does not cancel already accepted messages or rewrite their envelopes.

## Legacy message storage (schema 32)

Owner-authorized migration may preserve raw messages up to 64 MiB. Schema 32
widens only the immutable blob length and chunk ordinal constraints, retaining
64 KiB chunks and at most 1024 chunks. Earlier migration checksums are unchanged;
existing databases require the explicit owner schema upgrade. Ordinary SMTP,
upload, draft and submission preparation keep their 32 MiB admission limits.
Account quotas continue to charge every imported byte.

The owner import connection admits at most 72 MiB outgoing bytes and 4096 input
frames. Input bytes remain 1 MiB, or 4 MiB for the guarded new-only import.
Stored-body verification and replay allow 256 MiB incoming bytes with the
existing 8192-frame and 512 KiB outgoing limits. Raw JMAP downloads use the
64 MiB stored-body ceiling; configured network admission limits do not hide
successfully imported bodies. Authentication, per-frame bounds, deadlines and
bounded chunk buffers remain in force. Email body/header projections and text
search also use the stored-body ceiling rather than the current incoming
message ceiling. MIME header, part, depth, work and output bounds remain
independent and may still refuse an interpretation.

Full stored-body verification and raw download have an absolute 30-second
ceiling, including replay: the 64 MiB debug fixture exhausted ten seconds after
verification alone consumed eight. Connection establishment and fresh device
reauthorization retain their ten-second caps, shortened by the original body
deadline. Ordinary JMAP snapshot methods retain their existing ten-second cap.
Progress never renews an admitted operation's deadline.

An owner Email import's write connection admits ten seconds plus up to twenty
more in proportion to the body's share of the 64 MiB ceiling
(`postgres_probe::import_duration`), reaching the same thirty seconds at the
ceiling. The 64 MiB publication took about two seconds on an idle host and
exceeded the former ten-second cap under heavy load. Receipt inspection,
connection establishment and mailbox publication keep ten seconds, and a
caller deadline above the scaled allowance is refused before any work.


## Per-account submission routes

An account's optional `outbound_profile` selects the route of a configured
profile with the relay role. Complete candidate preparation refuses an unknown
profile or one without relay capability. The account field belongs to shared
configuration and is identical across profiles. Omitting it preserves the
submitting profile's route. This selects only a named route and its destination
evidence; no relay credentials are copied into the account or JMAP binding.

Each binding retains separate submission and worker routes. New authenticated
submissions bind and persist the account-selected route under the existing
configuration/device fences. A relay worker retains its own profile's route,
credentials and TLS policy, including when the same process also serves JMAP.
Changing the account selection does not rewrite previously queued routes;
retain workers for those routes until their accepted mail is settled. Native
route ownership, destination matching and account isolation remain in force.
An account route never grants a sending identity or unauthenticated relay.

`postgres account-update --outbound-profile NAME` changes the selection through
the retained operation workflow. An explicitly empty NAME removes the override
and restores inheritance. `account-list` includes any explicit selection. All
mail instances must restart after configuration publication, as for other
account edits. These commands do not configure provider credentials or DNS.


## JMAP origin aliases

Immutable configuration may declare `server.jmap_origin_aliases` beside the
primary origin, using CONFIG.md's bounded explicit allowlist. Candidate
preparation validates certificate coverage for every allowed JMAP hostname.
The runtime prepares a private Session and PostgreSQL API backend for each
account/origin pair, all sharing the same account binding, admission pools,
credentials and persistent authority. URL and session-state selection remain
consistent across discovery and API requests. Origin selection never selects
another account or grants relay permission. No schema migration is required;
configuration import/check/select and service restart activate the change.


## Retained SMTP worker session

The fixed non-TLS SMTP storage worker retains at most one PostgreSQL connection
for admission and publication, shared across its configured account registry.
The backend mutex refuses another thread and never queues a waiter. Jobs carry
resident DATA and a backend reference, not the connection. Startup readiness
uses the one-shot path before worker startup. Idle maintenance and shutdown
retire the connection on the owning worker; its read-slot permit is released
only after disposal. Independent pending-receipt settlement retains its own
one-shot capacity.

Admission fully qualifies schema, Runtime privileges and the shared session
migration lock once. Each SQL job begins a fresh READ COMMITTED transaction,
checks epoch and the current maintenance barrier, and uses the existing current
configuration, account and Inbox checks. Publication preserves sorted account
locks, receipt matching before stale-selection rejection, quota enforcement and
synchronous COMMIT. It performs no separate pre-publication receipt connection:
matching receipts are checked inside the publication transaction. No transaction
or account/policy lock survives between jobs or while DATA arrives.

A session lasts at most 60 seconds and 16 SQL jobs, counting readiness and
publication separately. Every job retains its original deadline and ten-second
ceiling. Each job admits only its actual account count: input is one MiB,
output 40 MiB and frame count 1088 per account. Session totals are sixteen times
those allowances at the configured maximum account count. Qualification shares
the first window; idle bytes retain their preceding charge, fragmented framing
and sticky failures survive windows, and cumulative budgets never replenish.
These wire-work allowances do not allocate equivalent resident buffers.

Only an observed idle failure before starting a new job may replace the
connection transparently. Once a job starts, failure does not replay its SQL.
COMMIT attempted without acknowledged success remains Indeterminate, retains
Pending custody, stops the backend and emits no final 250. A confirmed COMMIT
or matching historical receipt remains success even if later rollback,
deadline, framing completion or retirement fails; such failure discards the
connection separately. Ordinary refused jobs conservatively retire it. The
next job reuses a connection only after acknowledged transaction termination
and successful deadline and protocol-window checks. Expiry maintenance and
clean idle retirement use the existing bounded termination allowance; an
unfinished or failed job never gains time through cleanup.


## Retained HTTPS download authorization

Each fixed HTTPS body worker keeps either its API read cache or one download
authorization cache, never both. Account or origin changes, uploads and worker
exit discard that state. Standalone download callers keep the one-shot path.
A download still opens a fresh repeatable-read body snapshot, authenticates and
verifies its complete digest, then uses a separate READ COMMITTED transaction
for current configuration, account, credential and upload-expiry checks. Only
this second connection is retained. Its acknowledged ROLLBACK precedes output;
no snapshot, row lock or authorization result survives a request.

The authorization session is Runtime-qualified once under the shared migration
lock. Each turn checks epoch and maintenance before the existing selection-lock,
account-lock, credential and database-clock checks. The ordinary retained-read
profile bounds it to 64 turns and an immutable 60-second lifetime, with 1 MiB
input, 64 KiB output and 1088 frames per turn. Existing cumulative wire ceilings
remain independent limits. Initial qualification consumes the first window.
Idle disconnect replacement occurs only before body verification; an admitted
failed authorization is not replayed. Every failed download retires its cache.

Reserve two read permits before DNS: one for the body and one for the retained
check, or two transient permits for a standalone call. A retained connection
must cover the original body deadline before verification starts; otherwise it
is retired first. Verification and replay retain their 30-second bound, while
the fresh authorization turn is capped at ten seconds and the original body
deadline. Cache borrows end before output. Sender maintenance can retire the
idle checker during slow response writes, and fixed-worker maintenance retires
it while no request is active. A worker retains at most one idle permit.

## Bulk body verification

The verified body reader hashes all chunks with td-crypto's opaque
`BulkSha256`, backed by the existing admitted AWS-LC SHA-256 implementation.
It retains one digest context only for the verification pass, feeding the
existing 64 KiB chunks from batches of at most eight. The account, immutable
snapshot, ordinal/extent checks, final constant-time digest comparison and
original deadline remain unchanged. Error or unwind retires the digest and
refuses the verified pin. Empty bodies still require the SHA-256 empty digest.
The worker checks its original deadline around each bounded chunk batch;
native hashing is not independently cancellable. Native context allocation
belongs to the existing process/provider allowance, bounded by active workers.
This path is not an allocation-free primitive. Other digest callers retain
the existing owned implementation and storage bytes are unchanged.


## Persistent common-header summaries (schema 33)

`blob_summaries` holds rebuildable From, To, Cc and Subject JSON values, in
that fixed order, independently of mutable Email metadata or request property
order. Each account/blob has at most one row, bound to body length, complete
body digest and interpretation version 1. Version 1 uses the existing shared
header parser with a 256-KiB header ceiling, its aggregate interpretation
limits, four fields of at most 16 KiB each, and finite preparation work.
Changing these interpretation semantics requires a version bump. Different
configured header ceilings, unsupported properties, absent rows, incompatible
versions, digest/length mismatches or malformed JSON use verified parsing.

The shared blob publication boundary derives these values from its already
owned immutable bytes and stores them in the same transaction as the blob.
SMTP, drafts, imports and local notices therefore retain the original durable
publication boundary. Parsing examines only the bounded header prefix; it does
not decode attachments or compute previews. An unrenderable/oversized summary
is optional and is omitted, leaving the original message available through
existing behavior. Publication deadlines still apply to this additional work;
summary SQL failure cannot leave a partially committed delivery.

An authenticated read fetches at most sixteen summary rows for the current
metadata window, under the original snapshot, account and deadline. Only
header-only selections of these four properties use them. All Email existence,
flags, thread IDs, dates and memberships still come from that snapshot. JSON
values pass bounded framing and root-kind validation before output; final
snapshot validation and response publication remain unchanged. `summary_reuse`
counts successful persisted summary uses in the existing performance report.
No body chunks are read or body I/O precharged for a summary hit.

These rows are historical interpretation evidence, like the existing memory
cache, not current body integrity evidence. They neither create a body pin nor
detect subsequent corruption of an unrequested attachment. Full body, preview,
search and download paths retain current verification; qualified body-bearing
Email/get may use the chunk-verification contract below. Account/blob foreign
keys and cascading deletion remove summaries when the authoritative blob is
removed. There is no external cache service, credential cache or shared mutable
response object.

`postgres summary-backfill --connection-file OWNER --epoch E --account A`
rebuilds one finite page of Email-referenced blobs, including replacing
existing summaries. It can repair malformed derived rows as well as fill gaps. `--limit N`
selects 1..16 blobs (default 16); `--after BLOB` resumes after the previously
reported cursor. One page verifies at most 64 MiB of full bodies, retains one
bounded header prefix at a time, and writes at most 256 KiB of derived values
within the ordinary ten-second owner job. Reports contain `scanned`, `written`,
`skipped`, `verified_bytes`, `after` and `done`, never header contents. Skipped
unrenderable messages advance the cursor without fabricating a summary. Only
known COMMIT returns success and an advanceable cursor; failures preserve the
caller's previous cursor, and an unknown COMMIT is reported explicitly. Replaying
the same page is safe because these are replaceable derived rows, not mail
publication or outbound effects. Run pages serially, retaining each successful
cursor, until `done`; a later version rebuild starts again without `--after`.
New delivery does not wait for this administrative backfill.

Schema upgrade remains explicit: stop serving instances, use the owner schema
upgrade workflow, grant the runtime role the new table's ordinary mail access,
and restart the upgraded binary. Existing messages remain readable before
backfill. Backups include derived rows, but the original messages remain the
source for rebuilding them. A rollback binary must understand schema 33; do
not restart a schema-32 binary against the upgraded ledger.

## Persistent MIME interpretation (schema 34)

`blob_interpretations` holds optional, rebuildable MIME evidence per account
and blob. Version 1 fixes the parser limits at 256 KiB of aggregate headers,
1024 parts and depth 32. Its explicit 56-byte part records preserve preorder,
parent, entity/body/type extents, decoded size, media, encoding and diagnostics.
A domain-separated SHA-256 binds the table, aggregate header count and one
SHA-256 per 64-KiB body chunk to the whole-body digest and length. This is
publication evidence, not a MAC or independent protection against a database
writer replacing both data and evidence. Semantic parser changes require an
interpretation version bump.

Delivery, upload, draft, submission and import preparation derive evidence
from their owned immutable bytes before the publication transaction. SMTP
preparation remains serialized with publication. Locally composed failure
notices derive their evidence within their existing bounded transaction.
Evidence insertion shares the blob's durable transaction; optional parsing
failure omits the row, while SQL failure refuses publication. The original job
deadline still governs preparation. No extra background worker is started.

Metered publication prepays a finite child allowance from the original job
before parsing, hashing or encoding. Each dimension reserves at most half of
the remaining credit, further capped by input-size bounds; unused or failed
cache work is not refunded. This leaves credit for publication without giving
the cache new admission. Insufficient credit omits optional evidence and
preserves the verified-parser fallback. Standalone import, notice and owner
backfill preparation has a finite 512-MiB I/O, 64-million-record and 128-MiB
decoded-output allowance per message under its original deadline. Decoded-size
work is distinct from the much smaller retained evidence and page-output caps.

For a body-bearing Email/get, the original authenticated snapshot loads at
most one interpretation row at a time. Matching version, limits, length and
digest, bounded format, checksum and tree extents are checked before use.
Missing or incompatible evidence falls back to the existing MIME traversal.
A hit admits the private chunk-verifying pin. Each cache miss fetches only
the chunks intersecting the requested span, at most eight 64-KiB chunks per
batch, and verifies every returned ordinal, length and SHA-256 before exposing
any bytes. The complete manifest binds these hashes to the selected blob's
length and whole-body digest. Empty bodies still require the empty SHA-256.
This trusts publication-derived interpretation evidence, not a freshly
recomputed whole-body hash: corruption in an unrequested attachment may remain
undetected by a text projection. The manifest checksum is not a MAC.

The parser rereads entity headers at stored extents and checks their endpoints
and aggregate count. Current headers supply names, MIME labels and dispositions;
cached descriptors skip body traversal and encoded-size counting. Requested
text and previews pass through the same chunk-verifying reader. Header or
chunk inconsistency after qualification fails closed, with no retry under a
new snapshot. Missing, malformed or incompatible evidence takes the original
full-body verification and parsing path. The bounded chunk cache, original
work precharges (including conservative full-body I/O credit), deadline,
authorization and final response checks remain in force. No selected-read
result is promoted into the fully verified header cache.

`structure_reuse` counts successful qualified table reuse. `chunk_verify`
measures requested batch SQL, framing and hash verification, distinct from
`body_verify` for complete-body reads. Raw and attachment downloads, search
and integrity auditing retain full-body verification. Periodic integrity
scrubbing is an independently activated operator schedule described below.

`summary-backfill --structure` additionally rebuilds MIME evidence under the
same owner authority, snapshot, cursor and known-COMMIT rules as summary
backfill. It streams one verified body through bounded header, part and chunk
buffers; it does not allocate a whole-message buffer. The existing 64-MiB
verification and 256-KiB derived-output page caps cover both kinds of evidence.
Reports add `structures_written` and `structures_skipped`; a skipped MIME
interpretation does not prevent rebuilding a valid header summary, or vice
versa. Reduce the page limit if parsing reaches the original finite deadline.
New messages require no scheduled backfill. Cascading blob deletion removes
both kinds of evidence, and neither changes account sequence or Email history.
Schema-33 binaries cannot serve schema 34; use the explicit fenced upgrade
procedure and retain a consistent pre-upgrade backup for recovery.

## Scheduled whole-body integrity scrubbing

`postgres blob-scrub-page` is an owner-only, read-only primary audit page.
It takes `--connection-file`, `--epoch`, optional `--limit` (default 128,
maximum 256), and optional `--after ACCOUNT:BLOB --through ACCOUNT:BLOB`.
Both key components are canonical hexadecimal IDs. A continuation requires
its retained upper key. The first page captures the greatest `(account,id)`
blob key in its repeatable-read snapshot. Each page holds the migration lock,
checks the actual schema owner and epoch, verifies manifests, chunk layout
and the original whole-body SHA-256, and ends with known ROLLBACK before
returning progress. It reads all stored blobs, including unreferenced uploads;
it does not rely on summaries or MIME interpretation evidence.

A page attempts at most 64 MiB of valid advertised body lengths, streams one
body at a time, and retains the original ten-second operation deadline and
bounded connection framing. Corrupt metadata or bytes increment a bounded
failure count and advance the cursor; only the first failed identity is
retained. An operational, deadline or transport failure returns no publishable
cursor. A replay is read-only. These are historical per-page observations,
not one database-wide snapshot: concurrent deletion may remove a body before
its page, and insertion behind a cursor belongs to the next cycle. No repair,
mail deletion, metadata-integrity certificate or credential authority follows.

`postgres blob-scrub` adds `--state-dir` and runs one page with locally retained
progress. The pre-created private directory is locked and dedicated to this
operation. It contains only `LOCK`, `state.json` and interrupted `partial`;
files obey the protected operator-input contract. State is bounded to 8 KiB,
versioned and epoch-bound, published by private create, file sync, rename and
directory sync. A crash may replay a page but cannot skip one. Restored epochs
require a fresh directory; old progress is not reused. Unknown entries,
symlinks, malformed state and concurrent owners refuse. This state is an
operator progress receipt, not authoritative mail data.

A completed cycle waits 86,400 seconds before the next cycle. Each page
records totals, the cursor, first corrupt identity and operational failure.
Completed failure counts remain visible throughout the next cycle and clear
only after a complete clean cycle. Failures return nonzero and structured
JSON to the supervisor; progress also has structured JSON. The status command
`postgres blob-scrub-status --state-dir PATH --epoch HEX` needs no database
credentials. It waits at most fifteen seconds for an overlapping bounded page
to release the state lock. It fails on known corruption, the last operational failure,
missing completion, completion older than 172,800 seconds or backward wall
clock observations. Every invocation, including status and waiting, persists
the latest observed second before reporting freshness; a lower observation
refuses rather than making a stale cycle appear fresh again. It never claims integrity of future writes or replaces
backup/restore drills. These operator reports use the supervisor journal and
private progress file, not the database application-log table.

The installer operations generator can schedule one page per minute and a
separate status check every fifteen minutes on systemd or Shepherd. Each
invocation is bounded, the directory lock excludes overlap, and a failed page
is retried on the next tick. Schedules require explicit generation and
activation; normal serving does not enable background scrubbing implicitly.
