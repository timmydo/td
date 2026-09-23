# td-mta: personal mail service

## 1. Status and scope

This is the normative target for a new service, not a claim that it exists.
`IMPLEMENTATION.md` divides it into independently reviewable increments.
An incomplete increment must not advertise capabilities it cannot provide.
The v1 release requires the acceptance evidence in section 15.

The M01 library skeleton provides typed local IDs, configuration versioning and
checked resource planning only. [RESOURCES.md](RESOURCES.md) records its initial
byte ledger; [CONFORMANCE.md](CONFORMANCE.md) inventories the unimplemented JMAP
contract and current client calls. There are no protocol handlers or listeners.
The M02a/M02b format module adds checked scalar/key/row codecs and literal
format fixtures. [FORMAT.md](FORMAT.md) fixes their byte layout and the
container registry; persistence and the remaining M02 contracts are not
implemented by those codecs. [WIRE.md](WIRE.md) pins implemented wire-ID and
MIME-part locator codecs separately from the future protocol handlers.
[API.md](API.md) defines the compiling M02c2 adapter contracts and implemented
state codecs; [QUEUE.md](QUEUE.md) freezes future queue/restart/JMAP semantics.

The initial deployment is one person's approximately 1 GB of mail, multiple
domains, and explicit aliases on each domain pointing into one account's
mailbox store. Account IDs remain explicit in every storage and authorization
interface; v1 need not provide shared accounts or delegated access.
Configuration validation rejects a second account and any alias targeting it.

The service receives Internet SMTP for local recipients, stores and serves
mail through JMAP, and submits outgoing messages through a configured smart
host. It also supports receiving from an upstream gateway MX. It ships as
one executable with all executable dependencies statically linked against
musl for x86-64 Linux. Files for configuration, secrets, trust, and mail remain
external. Data formats and interfaces must permit a future aarch64 build.

Included in v1:

- SMTP reception, STARTTLS, durable local delivery, and explicit aliases.
- JMAP Core, Mail, and Submission sufficient for their advertised contracts
  and the existing td-mail client, including attachment upload/download,
  MIME construction, text search, threading, and submission reconciliation.
- Durable smart-host queue, retry, cancellation, and failure visibility.
- HTTPS with per-device application passwords and automatic ACME renewal.
- File configuration, inspectable mail storage, bounded rolling logs,
  machine-readable administration, migration, backup, and recovery tools.

Deferred: SPF, DKIM, DMARC verification and catch-all delivery. Excluded from
v1: IMAP, POP, public SMTP submission, direct-to-recipient-MX outbound delivery,
external forwarding, plus addressing, Sieve, vacation replies, spam scoring,
antivirus, calendars, contacts, webmail, clustering, and attachment-content
indexing. MIME attachment metadata remains searchable where JMAP requires it.
An inbound message is not authenticated merely because its transport uses TLS.
V1 must report sender authentication as not evaluated, never as passed.

## 2. Invariants and trust boundaries

1. Final SMTP acceptance and successful JMAP writes follow durable publication
   of the data and metadata needed to recover that operation.
2. Only configured recipients receive inbound mail. Neither Internet clients
   nor a trusted gateway can turn the receiving listener into an open relay.
3. Every JMAP object, blob, identity, and submission is scoped to an authorized
   account. Possession of an opaque ID does not authorize its use.
4. A submission accepted by JMAP remains queryable after restart, including
   while its smart host is unavailable. Queue state is not held only in RAM.
5. Request processing has fixed memory, connection, descriptor, disk, and work
   limits. Exceeding a limit produces a defined refusal, never silent loss.
6. Production td-owned code follows the repository's panic/indexing rules.
   Checked arithmetic, bounded nesting, and explicit error handling extend
   the rule to lengths, counters, conversions, locks, and thread creation.
7. Cryptographic primitives and TLS come from the narrowly reviewed provider;
   application protocols and storage remain std-only. No shell subprocesses
   implement mail, certificates, configuration, or administration.
8. Protocol tests run offline against local fixtures. Neither Migadu nor a
   public CA nor the deployment host is a test endpoint.

Adversaries include unauthenticated SMTP/HTTPS peers, malicious message and
MIME contents, authenticated clients sending malformed requests, forged
gateway identity, and hostile files outside the service's private roots.
Root, the kernel, and a compromised service UID are outside the confidentiality
boundary. Disk corruption, full disks, process termination, power loss, clock
changes, and third-party outages are explicit failure inputs.

Trust is not inherited from message headers. `Received`, `Authentication-Results`,
HTTP forwarding headers, and a claimed EHLO name never establish peer identity.
Email and logs are untrusted data for an AI operator, never instructions.

## 3. Code and dependency boundaries

Use `td-mta/` for a dependency-free library: bounded data structures, parsers,
state machines, storage, JMAP methods, queue policy, config, and administration.
Use a small `td-mta-runtime/` package to link that library with TLS/crypto and
provide the installed binary named `td-mta`. This is a compile-time boundary;
the operator installs one executable, with no td-net helper requirement.

The runtime package is a proposed named exception to the current std-only
roster. Before creating its manifest, amend AGENTS.md and both lock/gate paths
atomically. Do not hide the package from testing or weaken the rules for other
crates. The user has approved a minimal TLS/cryptography dependency category;
the implementation must still record the exact pinned transitive closure,
features, licenses, native build inputs, and rationale in its landing.

Prefer rustls with its ring provider, reusing compatible versions already
reviewed in `net/Cargo.lock`, and the existing root-certificate data where
appropriate. Do not inherit the entire td-net dependency set or enable a second
crypto provider. No async runtime, general web framework, database, mail parser,
serialization framework, or ACME framework is part of this exception. The
first dependency increment must demonstrate static musl linking and bounded
TLS behavior before later tasks depend on its API. Exact versions belong in
the lock and dependency review, not in this design's prose.

The portable musl artifact has a separate, host-only build manifest: pin Rust
and its target standard library, the musl C compiler/linker and sysroot needed
by the crypto provider, and their source/artifact checksums and provenance.
Provision them before offline builds; ambient host tools cannot silently fill
missing inputs. M03 owns these exact pins and a clean build fixture. This
portability workflow is outside td's source-built glibc target artifact graph;
it must not import host-built outputs into that graph or claim its provenance.

Core adapters provide typed errors and caller-owned buffers for transport,
clock, entropy, digest/signature operations, and fault-injected persistence.
Network workers cannot bypass the store's commit API. A crypto adapter also
serves future DKIM verification without implementing new crypto primitives.

No new unsafe surface is authorized by this document. Prefer safe std APIs.
If platform work requires unsafe, its increment must first read and amend
UNSAFE.md and this design, name each operation, and add confinement tests.
This includes any test allocator hook needing an unsafe implementation.

## 4. Deployment and transport

### 4.1 Direct MX

Publish MX records for every served domain pointing to a configured hostname,
for example `mx.example.net`, with A/AAAA records for reachable addresses.
Internet SMTP uses port 25 with STARTTLS; port 25 is not a plaintext fallback
after trying port 465. The receiving listener offers TLS 1.2/1.3 with the
provider's reviewed cipher defaults. It accepts plaintext delivery for peers
that do not negotiate TLS, as selected by the operator for compatibility.
Once STARTTLS begins, a failed handshake closes the connection; it never
continues that session in plaintext. Reset SMTP state after successful TLS.

Port 443 serves JMAP HTTPS and configured MTA-STS policy hosts. Port 80 serves
ACME HTTP-01 challenges and
may redirect ordinary GET requests to a fixed configured HTTPS origin; it
never accepts credentials or JMAP writes. URLs in JMAP discovery are generated
from configured origins, never an untrusted Host header.

Support serving a static MTA-STS policy from the configured
`mta-sts.DOMAIN` HTTPS names. `dns-plan` emits the necessary MX, address, and
`_mta-sts` TXT records and policy text; it does not edit DNS. Start in testing
mode, then allow the operator to select enforcement after certificate/DNS
validation. This is policy publication for senders that support MTA-STS,
not outbound MX lookup or a claim that all inbound sessions use TLS.

### 4.2 Gateway MX

In gateway mode public MX records point to another server. The gateway owns
Internet acceptance/retry and sends to a dedicated td-mta listener. The
deployment restricts that listener using firewall rules AND explicit peer
address admission in td-mta. Require STARTTLS with verified client certificates
for a network gateway; a private VPN can additionally protect the route.
Plaintext is allowed only in an explicitly configured loopback fixture mode.
Trusted peer admission never permits nonlocal recipients.

Configure the gateway listener's server hostname explicitly. The upstream
gateway verifies that name and certificate chain; certificate provisioning and
renewal cover it even when this host is not the public MX. An operator-provided
certificate/private CA is permitted for a private gateway, with the same name
and expiry checks and an explicit operator renewal responsibility.

Gateway client authentication uses a dedicated configured private trust root
and exact allowed certificate identities mapped to gateway IDs. Verify chain,
validity, client-auth usage and identity in addition to peer address admission.
The bundled outbound public roots never authorize gateway clients. A valid
client certificate for another identity is insufficient. Bound trust reload
and define revocation/rotation fixtures before enabling the listener.

The authenticated identity is the gateway, not the original sender. Record
both the socket peer and configured gateway ID. V1 does not implement PROXY
protocol, XCLIENT, or trust arbitrary original-IP headers. Future SPF checks
must not mistake the gateway's address for the originating sender's address.
Gateway-provided authentication results require a separately specified trust
contract before they influence policy.

Direct and gateway listeners have separate limits, credentials, and admission
policies. A deployment may enable either or both for migration. A gateway must
share the valid-recipient policy or validate recipients before accepting mail;
otherwise its accepted unknown-recipient mail can generate backscatter. This
service's gateway support is a receiving role, not a general forwarding MTA.

### 4.3 Smart host and local conformance fixtures

The initial provider is `smtp.migadu.com`, port 465, implicit TLS, and mailbox
password authentication, matching Migadu's published settings. Provider MTA
software/version is unverified. Do not infer it from unrelated deployments or
make compatibility depend on a banner. No live SMTP probing is needed.

Support configured implicit TLS and required STARTTLS submission endpoints.
Verify certificate chains, validity, and the configured hostname; send SNI.
Never send credentials before verified TLS or downgrade after a TLS failure.
Support advertised AUTH PLAIN and LOGIN with bounded challenge/reply handling.
Do not advertise or use an AUTH mechanism merely because a banner suggests it.
The operator configures permitted From/envelope identities; aliases accepted
inbound are not automatically authorized outbound by Migadu.

The default local fixture offers implicit TLS, AUTH, EHLO extensions, envelope
validation, DATA, and a durable captured message. Its profile models published
Migadu settings, not an asserted clone of Migadu's private configuration.
Scripted peers cover refusal, retry, TLS, and ambiguous outcomes (section 15).
An independently implemented local MTA may be an additional interoperability
oracle after its test-only source pin is reviewed; it is not a runtime input
or a prerequisite silently downloaded by the test suite.

## 5. Resource and execution model

Targets for the default personal profile are RSS below 64 MiB idle and below
128 MiB under the workload in section 15. These are unverified release gates,
not measurements. Record RSS and cgroup memory separately: filesystem page
cache can affect the latter.

Use a fixed set of long-lived workers, preallocated connection slots, and
bounded queues of slot IDs. Allocate and touch application arenas before
opening listeners. Do not spawn a thread per connection/request or load the
mailbox's full metadata/content into RAM. Use bounded I/O chunks, sorted metadata files, and
disposable disk indexes with a fixed cache. No unbounded mmap or memory-sized-to-mail
strategy is permitted. Maintenance shares an explicit budget with live work.

Event-source streams hold connection slots but no storage read view between
events. They use bounded state notifications and a dedicated fixed execution
budget or nonblocking scheduling; they cannot occupy all workers that process
ordinary requests, commits or health checks. M02 pins that scheduling choice.
Safe std supports a bounded round-robin scan of nonblocking sockets with a
deadline-based timed wait; it does not expose poll/epoll. If M02 instead chooses
fixed blocking workers, reserve event workers separately and count all stacks.
An OS readiness adapter requires the explicit unsafe review from section 3.
Read slots are acquired per store operation with a bounded fair wait queue;
exhaustion returns a retryable HTTP 503 before response headers or the mapped
JMAP method error. A streaming response that has begun cannot fabricate an
error object inside its body: finish from its admitted view or terminate it.
An online backup uses one read slot; interactive work shares the other. This
intentional concurrency limit preserves the memory budget.

Initial default ceilings (validated together at startup):

| Resource | Default limit |
| --- | --- |
| Simultaneous inbound SMTP sessions | 8, at most 2 per peer address |
| Active HTTPS connections | 8, with a separate cap of 2 TLS handshakes |
| Event-source connections within the HTTPS pool | 2 |
| Concurrent smart-host deliveries | 1 |
| Concurrent body/search jobs | 2 |
| Raw message or uploaded blob | 32 MiB, streamed |
| Aggregate header bytes / MIME nesting / MIME parts | 256 KiB / 32 / 1024 |
| SMTP recipients per transaction | 100 |
| JMAP JSON request / methods per request | 1 MiB / 16 |
| JMAP object IDs per get/set / query page | 256 / 256 |
| JSON nesting / parser tokens per request | 32 / 32768 |
| Combined resident index cache | 8 MiB |
| Storage read views | 2, each with a 4 MiB journal arena plus bounded descriptors |
| Unattached upload storage | 128 MiB per account, expiry after 24 hours |
| Pending outbound storage | 256 MiB and 1000 submissions |
| Maintenance sort scratch on disk | 64 MiB per account |
| Active log plus retained generations | 8 MiB each, 4 retained |

Values are operator-adjustable within compiled maxima and an explicit startup
memory budget. SMTP command/path/line limits also obey their protocol minimums;
the resource table does not replace RFC limits. The implementation must add an
arena ledger with byte counts, worker stack sizes, scratch reservations, and
TLS headroom before committing a default profile. A larger configured pool
cannot silently retain the default memory claim.

The M01 ledger reserves 62874880 bytes under the default 64 MiB budget,
including planned stack, TLS, reload and process allowances. It is not RSS
evidence. M02/M07 must fit concrete structures and measured provider use within
those reservations or amend the ledger before enabling service admission.

The no-allocation contract covers td-owned hot processing: SMTP parsing and
streaming, MIME scanning, HTTP/JSON parsing, JMAP evaluation/serialization,
store commits, queue dispatch, and structured logging after slot admission.
Use reusable arenas, borrowed views, bounded formatting, and fallible capacity
checks. Allocating on every admitted request and calling it admission work is
not an exemption. Any std/platform allocation unavoidable in an I/O adapter is
named, bounded, and measured alongside TLS; it must not grow with message or
mailbox size. Configuration reload, startup, certificate rotation, and offline
migration are cold paths, still bounded and accounted for at peak overlap.

TLS library allocations are a separate measured budget, including handshake
records, certificate chains, session caches, and concurrent old/new certificate
generations. Application allocation counters alone do not establish whole-
process bounds. Disable unneeded resumption caches and 0-RTT.

All network operations have idle and total deadlines; slow progress must not
reset the total deadline forever. Bound keepalive requests and fairness between
SMTP and JMAP so unauthenticated connections cannot starve an authenticated
mail reader or health checks. Resolve configured outbound hostnames using a
bounded std-only DNS adapter with explicit UDP/TCP deadlines and A/AAAA/CNAME
limits; do not hide uninterruptible resolver work in unbounded threads.
Never perform recipient-MX lookups for outbound mail. Saturation produces SMTP
temporary failure, HTTP/JMAP limit responses, or bounded retry, as appropriate.

No design can promise survival of arbitrary dependency panic, kernel OOM kill,
or storage failure. Do not use catch_unwind as storage recovery. An unexpected
worker failure makes health fail and the service stop accepting new mutations;
restart recovers committed state. A supervisor restarts the process. Release
tests cover both orderly shutdown and abrupt termination.

## 6. Configuration and local administration

Use a documented small line/stanza configuration grammar with versioned schema,
quoted strings and integer/boolean values. It resembles td's existing service
files but is not advertised as general TOML. Reject duplicate/unknown keys,
invalid UTF-8, embedded NUL, conflicting aliases, dangling account references,
and incompatible listener policies. No includes, environment expansion, shell
evaluation, arbitrary hooks, or network-loaded configuration.

Separate operator configuration (`/etc/td-mta/`) from service-managed data
(`/var/lib/td-mta/`), runtime control (`/run/td-mta/`), and logs
(`/var/log/td-mta/`). All paths can be configured absolutely for other distros.
Secret values come from protected files, not command-line arguments or logs.
Check file type, ownership, permissions, and trusted ancestors before use;
the service does not support a data root with an untrusted concurrent writer.
Create secret/mail files as 0600 and private directories as 0700, without a
permissive creation window. Never derive a filesystem pathname from a mailbox
name, address, attachment filename, or arbitrary client ID.

Planned commands, with stable JSON output and exit codes:

| Command | Contract |
| --- | --- |
| `config check` | Offline syntax, references, permission, and resource validation |
| `config show --redacted` | Effective values, defaults, and configuration generation |
| `serve` | Foreground service, no daemonization |
| `status --json`, `doctor --json` | Health, bounds, certificates, storage, queue; no mutation |
| `dns-plan` | Expected records and MTA-STS policy; no DNS changes |
| `reload` | Validate complete candidate, then atomically install or retain old config |
| `queue list`, `queue inspect ID` | Paginated status and redacted reasons |
| `queue retry ID`, `queue cancel ID` | Named, journaled operation; never repeat accepted recipients |
| `device create`, `device revoke ID` | Local credential administration |
| `store layout`, `store inspect`, `store journal`, `store export` | Bounded read-only decoding/export under local administrator authority |
| `store verify`, `store repair` | Read-only verification; explicit offline repair with a manifest |
| `backup`, `restore`, `migrate` | Bounded, resumable tools using the storage contract |

Online mutations go through a private Unix control socket and the single
writer. The socket directory's permissions establish administrator authority;
there is no public administration HTTP API. Offline mutating commands require
the exclusive store lock. Readiness checks do not contact Migadu or a CA.
`doctor` network probes, if later added, require an explicit opt-in and cannot
send email. Configuration checks never expose supplied secrets in errors.

Listener address changes, data-root changes, and pool sizing require restart.
Aliases, device revocations, and validated certificates can reload. Revocation
is rechecked before a mutation commits; a connection is not permanent authority.
Reload has bounded coexistence memory and records the effective generation.

## 7. Identity, passwords, and TLS lifecycle

Generate application passwords with at least 256 bits of provider entropy,
display once to the local administrator, and store only a versioned salted
verifier. Use the provider for digests and constant-time verification.
High-entropy generated tokens do not need a human-password derivation scheme;
v1 does not accept operator-chosen weak passwords. A device record binds an
account, label, stable ID, creation/revocation state, and verifier. Labels are
untrusted display strings. Bound authentication work globally and by peer.

Accept HTTP Basic authentication only over verified server HTTPS from clients;
td-mta presents the certificate and the client verifies it. Credentials do not
grant local OS login or td elevation. Service-owned ACME keys and smart-host
secrets are unattended server credentials, distinct from td's human-session
credential portal. A later td image integration must specify provisioning
under that platform's credential policy rather than using its su escape hatch.

ACME runs inside the executable using HTTPS, JWS, and the crypto provider.
Start with HTTP-01 on port 80; DNS-01 and TLS-ALPN-01 are deferred. The operator
controls the deployment host's ports and DNS; this development host is not
configured as the mail server. Certificate identifiers include the MX/JMAP
hostname, configured gateway listener names selected for ACME, and enabled
MTA-STS policy hostnames, not automatically every alias. ACME-managed gateway
names require reachable HTTP-01 validation; private names use provisioned
certificates rather than starting an order that cannot be validated.

Persist the ACME account key and resumable order state privately. Bound
directory/nonces/order responses and certificate chain sizes. Validate nonce,
URL, challenge, CSR, key, identifier set, and returned chain. JWS/CSR encoding
is bounded std code; signing/key generation is provider code. CA endpoints
must be HTTPS; follow only validated directory/order endpoints, with no
credential-bearing redirect to arbitrary hosts or protocol downgrade.

Schedule renewal early based on actual certificate lifetime, with jitter and
bounded exponential retry; do not hardcode a 90-day lifetime. Honor CA retry
instructions within explicit limits. Test clock jumps and suspend/resume.
Write a new certificate generation, sync, validate key/chain/names, then
atomically select it. Existing sessions finish on their bounded old generation.
A failed renewal retains the old certificate and exposes a health error.
Never silently replace an expired/missing public certificate with self-signed
TLS. During initial issuance, serve ACME and local diagnostics; do not expose
JMAP or accept new mail until a usable certificate exists. After expiry, refuse
new TLS service and temporarily refuse inbound mail rather than quietly hiding
loss of the deployment's advertised transport security.

Use bundled reviewed public roots for outbound TLS, with an explicit protected
CA-file override for local tests/private gateways. A root-data update is a
reviewed artifact update. A test trust override must not disable verification.

## 8. On-disk store and crash consistency

[STORAGE.md](STORAGE.md) is the normative physical storage specification.
Read it before implementing persistence, queries, queues, migration or backup.
It owns the directory/file layout, authoritative row model, binary container
rules, transaction publication, checkpoint/replay, bounded read views,
reclamation and inspection commands.

The chosen representation is ordinary immutable `.eml` files plus compact
binary metadata inspected through td-mta commands. Folder names, membership,
keywords, JMAP IDs/history and submission outcomes are authoritative metadata;
message bytes alone cannot reconstruct them. Parsed headers, offsets and search
indexes are disposable caches. Live metadata changes use the transaction API;
direct file editing is unsupported. No database dependency is added.

One sorted metadata checkpoint plus a bounded recent-change journal forms the
current account state. The single writer pauses new mutation admission during
checkpointing; readers pin a checkpoint and an exact committed journal prefix.
This is a small purpose-built storage engine with explicit recovery obligations,
not a claim that filesystem rename alone supplies multi-object transactions.
Large message bytes are streamed and never included in metadata checkpoints.

The core publication rule remains: sync new immutable bytes and their published
directory entries, then append/sync the complete metadata transaction, then
expose it and acknowledge success. Preserve queued transmitted bytes independently
of the visible email's lifetime. A failed sync stops the writer for recovery.
Storage corruption is distinct from a rebuildable index failure. Account backups
pin one exact view; capturing all service secrets/configuration initially requires
a stopped whole-service backup.

## 9. SMTP receiving and message representation

Implement a bounded SMTP state machine with EHLO/HELO, MAIL, RCPT, DATA, RSET,
NOOP, QUIT, SIZE, 8BITMIME, STARTTLS, and enhanced status codes. Advertise
PIPELINING only after its ordered parsing and error paths are tested.
Do not advertise SMTPUTF8, CHUNKING, BINARYMIME, DSN, or AUTH in v1 reception.
Local configured addresses use ASCII domains/local-parts; domains compare
case-insensitively, while ordinary local-parts match aliases case-sensitively.
Reserve case-insensitive `postmaster` at every served domain and the SMTP
domainless Postmaster form, routing both to the sole account. Configuration
must validate that route; it cannot disable it or redirect it outside the
account. Support VRFY with the standard non-enumerating 252 response.
Unicode message display names and MIME content remain supported.

Reject unknown/nonlocal recipients at RCPT. Resolve accepted aliases to stable
account IDs and deduplicate delivery to the same account within one transaction;
retain the accepted envelope recipients for inspection. Delivery decisions are
pinned for that transaction rather than changing halfway through a reload.
Every v1 inbound delivery files once in the account's Inbox, including when
several accepted aliases match. Aliases select accounts, not folders; alias
names remain envelope metadata. Provision exactly one Inbox before receipt
and forbid its deletion or role reassignment while receiving is enabled.
V1 has one account, so its final DATA success is one store transaction. Extending
to multiple recipient accounts requires a durable delivery manifest before
acknowledgement; independent partial writes cannot masquerade as atomic success.

Parse command/data boundaries across arbitrary network chunks. Strict CRLF,
dot-stuffing and termination rules prevent SMTP smuggling. Never interpret DATA
bytes as commands after a framing/size failure; drain only within a bounded
policy or close. Preserve original message content apart from SMTP transport
decoding and required locally generated trace fields. Do not rewrite content
just because MIME parsing is incomplete or a character set is unsupported.

Store envelope information separately from message headers. Add a truthful
Received trace and handle Return-Path according to final-delivery semantics;
do not trust an incoming Return-Path as the SMTP reverse path. Accept null
reverse paths for delivery-status messages. Permanent recipient refusals occur
before acceptance; temporary resource/storage trouble returns 4xx. After final
acceptance, ordinary local delivery must not depend on a later fallible task.

MIME scanning is incremental, with bounded header unfolding, part descriptors,
nesting, transfer decoding and work. Support multipart alternatives/mixed,
base64, quoted-printable, encoded header words, common address/date forms,
UTF-8/ASCII and a documented initial legacy-charset set. Unrenderable text is
reported through JMAP's encoding/problem semantics and raw bytes remain
downloadable; raw storage does not depend on rendering success. Pathological
messages hitting processing limits expose a typed error without killing the
service or hiding an accepted raw message. Define exact MIME/property coverage
and fixtures before claiming the Mail capability.

## 10. HTTP and JMAP

Implement bounded HTTP/1.1 for discovery, authenticated method calls, uploads,
and downloads. Reject conflicting Content-Length/Transfer-Encoding, ambiguous
header syntax, oversized chunks, invalid request targets, and request smuggling
patterns. Bound headers independently of bodies. Stream chunked/content-length
uploads and downloads. No request compression, arbitrary proxy routing, or
cross-origin browser access by default. Protocol errors terminate the connection
when its framing cannot be trusted.

RFC 8620 and RFC 8621 define wire semantics. Maintain a capability/method/property
coverage table with normative section references and local acceptance fixtures.
Capability advertisement is a release gate: a td-mail-only happy path is not
proof of conformance. Unsupported optional capabilities are omitted; unsupported
methods/properties get the specified errors rather than fabricated empty success.
Publish truthful resource limits and read-only identity properties. Include
the standard session eventSourceUrl and a bounded authenticated event-source
endpoint, with coalesced account state, type filtering, ping and closeafter
behavior. Polling also works. External push-service registration is refused
by explicit policy using the standard method error; v1 never POSTs to a
client-supplied callback URL. The conformance table must pin that refusal
and the associated get/set behavior rather than silently omit Core methods.

The compatibility baseline includes the current td-mail JMAP implementation,
particularly its existing sending flow:

- Discovery, HTTP Basic, account/capability and URL templates.
- Mailbox get/set; Email query/get/set; Thread get; filtering, pagination,
  date ordering, text search, headers, body values and attachment download.
- Identity/get, attachment blob upload, structured Email/set creation.
- In one request, creation ID `#draft` used by EmailSubmission/set, followed by
  onSuccessUpdateEmail and its implicit Email/set response under the submission
  call ID. td-mail already checks this response for filing failures.
- EmailSubmission/query by identityIds/after/before and EmailSubmission/get,
  used to reconcile a lost sending response. Preserve message IDs and filing
  metadata used in the client's subsequent reconciliation.

The repository baseline was inspected at
`248054be6` (including td-mail sending commit `c2c343bf3`). Implementers recheck
current client calls before freezing the wire fixtures. Existing td-mail mock
tests remain unit/integration coverage, but v1 additionally runs the real client
against the real service through its actual td-fetch transport.

JMAP changes, state preconditions, partial set results, creation/result
references, and account isolation must be explicit. A multi-method request is
not one global transaction: earlier successful methods remain successful if
later methods fail. Thread assignment is deterministic and persisted using
Message-ID, References and In-Reply-To; duplicate or cyclic headers cannot
merge accounts or cause unbounded work. An existing Email's threadId is
immutable. New arrivals may join one existing thread, but never merge or
renumber existing Email threadIds. STORAGE section 3 owns authoritative lookup
and deterministic conflict rules; M02 freezes their byte encodings/fixtures.

Text search streams decoded searchable data with bounded working memory and
uses disk indexes for candidate selection where available. Queries have stable
ordering with an ID tie-break and a committed snapshot. Do not silently truncate
results or report an exact total before computing it. Exhausted work/time
budgets return the appropriate explicit error; attachment binary contents are
not decoded as documents. Index build, rebuild and query must obey section 5.

## 11. Submission and retry semantics

[QUEUE.md](QUEUE.md) owns the exact transition, cancellation, retry and standard
JMAP projection contract; the following summarizes its service behavior.

Authorize identity, header From/Sender and envelope sender/recipients before
creating a submission. Bound recipients, sizes, header injection and recipient
expansion. Serialize structured JMAP messages into immutable MIME, including
attachments and reply references. Remove Bcc headers from the transmitted copy
while retaining Bcc envelope recipients and the user's sent-copy metadata.
Pin the transmitted blob and envelope at submission; later email edits or
deletion do not alter an accepted queued message.

Creating a JMAP submission means accepting responsibility into the local durable
queue, not proving remote delivery. Commit queue intent and the submission's
queryable record before acknowledging creation. Apply onSuccessUpdateEmail
according to the JMAP method's success semantics, and report its own result;
filing in Sent is not proof of smart-host or final-recipient delivery.
The queue retains its blob even if the client removes the corresponding email.
V1 does not offer FUTURERELEASE: sendAt is the submission creation time and
never moves with retries or eventual relay acceptance. Identity/time queries
therefore find the record in td-mail's original reconciliation interval even
during a prolonged relay outage.

Internal states are queued, in-flight, retry-wait, accepted-by-relay,
permanent-failure, canceled, and outcome-unknown. Persist per-recipient states,
attempt IDs, last bounded diagnostic, next-attempt time and expiry. Document
their mapping to standard JMAP undoStatus/deliveryStatus without adding made-up
standard fields or claiming accepted-by-relay means delivered-to-recipient.
Local CLI/logs may expose the richer internal state.

Before sending bytes, persist the attempt and recipient set. SMTP 4xx, connect
failure, and timeout schedule retry. SMTP 5xx marks the relevant recipients
permanently failed. If some RCPT commands succeed and others fail, DATA covers
only those with successful RCPT replies. A successful RCPT reply alone never
marks a message delivered or releases its queue reference: responsibility
transfers only after the relay's final positive reply following DATA. A failed
DATA leaves those recipients pending or failed according to that final reply.
Never resend recipients whose final DATA acceptance is durably recorded.
Refused authentication or bad certificates pause the affected route,
retain pending mail, and expose an actionable error instead of a tight loop.

Smart-host DNS failures, including timeout, SERVFAIL and NXDOMAIN, are route
transport failures. Apply bounded exponential route backoff and health/log
diagnostics; no DNS response by itself permanently fails a recipient. Pending
mail remains queued subject to its normal expiry. Manual retry cannot bypass
the route's concurrency or minimum retry interval.

Default automatic retry starts at five minutes, doubles to an hourly cap with
jitter, and expires after five days. V1 ignores free-text server retry hints.
Persist UTC scheduling data; use monotonic time within a running attempt and
bound catch-up after clock changes/restart. No sleeping thread per queue item.
Use a disk due-time index and fixed queue window, not an in-memory full queue.

A disconnect after the DATA terminator, or a crash after remote acceptance but
before its durable local record, leaves an uncertain outcome. Preserve the same
transmitted Message-ID and body and retry under the normal policy, recording
the duplicate risk. SMTP cannot guarantee exactly-once delivery. Do not use
Message-ID as a provider deduplication promise. An in-flight recovered attempt
is uncertain until its recorded phase proves that message acceptance could
not have occurred.

Local retry/cancel operations are serialized with the worker. Cancellation
is successful only before an irrevocable remote attempt; never report a
message recalled after it may have been accepted. Keep final submission records
for at least 30 days by default and do not automatically expire unresolved
failures. Expired/permanent failures retain inspectable metadata and notify
the account through a locally generated failure message plus health/log state;
never send unsolicited error mail to an unverified inbound reverse path.
If failure notification cannot be committed, retain a pending notification
record and retry locally. Explicit deletion/retention operations reclaim data.

## 12. Logs and observability

Emit versioned JSON Lines with bounded event sizes, fixed event codes, severity,
UTC time, process boot ID, config generation, connection/request ID, transaction
sequence and submission ID when relevant. Escape/control-character encode all
peer strings. Structured fields have documented meaning and stable enum values.
Default logs omit credentials, Authorization/AUTH contents, message bodies,
subjects, and recipient addresses; authorized inspection can retrieve details.

One writer rotates by size using rename, retaining the configured generations.
Bound log buffering; a flooded peer cannot block durable mail storage behind
logging. Report suppressed-event counts and log-write failures in health plus
a rate-limited stderr fallback. Logs are diagnostic evidence, not the journal
or an acceptance condition. The supervisor must not rotate the same files.

Expose counters for accepted/refused mail, TLS/plain sessions, active slots,
limit refusals, queue age/depth, unknown outcomes, disk headroom, dropped logs,
index lag, authentication failures and certificate expiry/renewal. `status`
distinguishes serving, degraded, recovering and refusing mutations. Readiness
depends on local storage/config/listeners, not a healthy smart host or CA.
Agent-facing output clearly separates facts, recommended actions, and untrusted
data. No API executes instructions obtained from messages or logs.

## 13. Migration and operational lifecycle

Migrate from Stalwart 0.15.2 through its authenticated JMAP interface and raw
blob downloads, not by reverse-engineering its database. Preserve raw message
bytes, folder names/hierarchy, membership, and received dates; preserve ordinary
keywords/read state when available at little extra cost. Stable remote IDs are
recorded for resumability, not reused as local pathnames. No identities, rules,
contacts, calendars, account passwords, or server configuration are migrated.

Stream a paginated export into an archive containing raw files and a versioned
manifest with source account/object IDs, byte counts, digests and mailbox
mapping. Resume by source instance/account/object ID plus verification, not
Message-ID or byte digest alone: identical messages can be distinct objects.
Repeated imports must not duplicate a previously mapped object; preserve a
single source email's multiple folder memberships. Do not log access tokens.

A bulk copy while Stalwart is live is provisional. Final cutover requires a
documented quiescent interval: freeze client writes and receipt on the source,
reconcile the final source state, verify per-folder/object counts and digests,
then switch service/DNS. Upstream SMTP senders queue during temporary refusal.
Account for DNS cache overlap by keeping the old endpoint able to temporarily
refuse, or explicitly deliver to the new gateway listener after cutover; do
not leave two independent writable mail stores. Never silently skip failed or
oversize imports. The importer emits a bounded paginated exception report.

Retain an untouched source backup and tested archive. Rollback before new mail
or writes reach td-mta can restore the old endpoint directly; rollback after
that point must export and reconcile td-mta's new mail and changes first.
Changing DNS back alone loses work and is not a rollback procedure.

Run in the foreground as a dedicated unprivileged service identity. Deployment
provisions directories and grants only low-port binding through its supervisor
or a reviewed launcher. Do not run the protocol engine as root or introduce
setuid behavior. Inherited-listener support, if selected, must validate socket
types, addresses and counts, and obey the unsafe contract. Systemd/td-svc
examples are packaging tasks, not executable dependencies. SIGTERM draining
and abrupt-death recovery both need tests; signal integration cannot silently
add an unsafe surface. Persistent state is outside the executable's directory.

This standalone artifact does not automatically enter td's system image.
Future image integration follows the source-bootstrap, profiling, service,
credential and packaging contracts in the repository. A host musl build is
useful for other distros but is not proof of a source-bootstrapped td artifact.

## 14. Implementation ownership

The implementation plan is the handoff contract. Tasks must name dependencies,
owned modules, typed interfaces, explicit exclusions, and executable acceptance
criteria. Freeze formats and API shapes before parallel consumers implement
them. Each independently landable increment stays green under DEVELOPMENT.md.
An unresolved design decision is written down before implementing dependent
code; it is not delegated to whichever worker arrives first.

The plan's initial schema/format/dependency tasks resolve exact encodings,
provider features, memory ledger and standard capability tables. They may
refine this document with measured evidence, but cannot silently relax its
durability, authentication, allocation, or no-live-service-test boundaries.

## 15. Acceptance evidence

V1 is ready only with reproducible evidence for all of the following:

- Static-musl executable inspection (no PT_INTERP or DT_NEEDED), operation in
  a clean Linux environment without td helpers, and host-independent data
  encoding fixtures. Build/test the core on aarch64 when tooling is available;
  td-owned x86-only assembly and native-usize serialization are prohibited.
  Provider assembly must have a supported implementation for each target; its
  presence alone does not prohibit reviewed per-architecture acceleration.
- Syntax/confinement lint coverage and malformed-input property/fuzz fixtures
  for SMTP, HTTP, JSON, MIME, config, journal, DNS and certificates. Fuzzing
  tooling is development-only and separately pinned/approved if external.
- Allocation instrumentation of admitted hot operations, including error and
  logging paths; separate TLS/cold-path measurements. Pool exhaustion refuses
  predictably, and a long run shows no resident growth with delivered count.
- A deterministic 1 GiB corpus of 10000 messages with mixed folders, MIME,
  Unicode, attachments, duplicate Message-IDs and distinct identical messages.
  Measure idle and peak RSS after restart and during 4 concurrent SMTP streams,
  2 JMAP readers, one bounded search, queue retry and certificate rotation.
  Repeat resource tests with many small messages and a full queue. Record
  compiler/profile, kernel, filesystem, corpus seed, limits and page-cache
  accounting. Corpus bytes and object count are separate scaling dimensions.
- Kill/fault injection at each publication/sync boundary for inbound receipt,
  JMAP mutation, submission creation/attempt, checkpointing, renewal, migration
  and restore. Verify acknowledgements against recovered state and prove no
  committed object is reclaimed. Include disk full, short I/O and failed sync.
- Real td-mail + real td-fetch + real td-mta: receive/read/search/move/flag/
  delete/thread/download; send with attachments and Bcc; query a submission
  after a lost HTTP response; inspect the local SMTP capture and Sent filing.
- Local SMTP scripts: implicit TLS, required STARTTLS, untrusted/expired/wrong-
  name certificates, missing STARTTLS, bad AUTH, multiline/fragmented replies,
  SIZE/8BITMIME differences, 421/450/451/452/550, mixed recipient outcomes,
  disconnects before and after DATA, restart, duplicate-risk recording,
  cancellation races, route pause, retry expiry and local failure notification.
- Direct and gateway reception: nonlocal recipients denied in both modes,
  peer/certificate admission, forged headers ignored, SMTP smuggling refused,
  slow-client fairness, and fresh TLS use after certificate rotation.
- Local ACME protocol fixture: initial issuance, nonce retry, renewal, rate
  limits, malformed/mismatched certificates, interrupted publication, expired
  certificate behavior and clock changes. A mocked signer alone is insufficient.
- A Stalwart 0.15.2 compatibility fixture or sanitized captured JMAP exchanges,
  a restartable export/import oracle, and count/hash/folder verification. Exact
  source data/version must be identified; a generic mock cannot be labelled
  proof against that release. No production mailbox is modified by tests.
- All integration endpoints resolve within the isolated fixture network, with
  egress denied. The fixture rejects configured public provider names before
  connecting. No live Migadu/CA/DNS/deployment-host dependency.

## 16. References

- [SMTP and responsibility transfer, RFC 5321](https://www.rfc-editor.org/rfc/rfc5321.html)
- [STARTTLS, RFC 3207](https://www.rfc-editor.org/rfc/rfc3207.html)
- [Submission TLS, RFC 8314](https://www.rfc-editor.org/rfc/rfc8314.html)
- [JMAP Core, RFC 8620](https://www.rfc-editor.org/rfc/rfc8620.html)
- [JMAP Mail and Submission, RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html)
- [Internet message format, RFC 5322](https://www.rfc-editor.org/rfc/rfc5322.html)
- [MIME, RFC 2045](https://www.rfc-editor.org/rfc/rfc2045.html)
- [HTTP/1.1 framing, RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html)
- [ACME, RFC 8555](https://www.rfc-editor.org/rfc/rfc8555.html)
- [MTA-STS, RFC 8461](https://www.rfc-editor.org/rfc/rfc8461.html)
- [Migadu's documented client settings](https://www.migadu.com/guides/)
- [Let's Encrypt challenge types](https://letsencrypt.org/docs/challenge-types/)

Migadu settings are documentation-derived, not tested against its live server.
Implementation fixtures cite the additional extension RFCs they exercise.
