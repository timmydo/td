# td-mta implementation handoff

## How to use this plan

Read `DESIGN.md`, root `AGENTS.md`, and `DEVELOPMENT.md` before work.
Crypto/backend tasks also read `td-crypto/DESIGN.md` and `td-crypto/TLS.md`.
Service-facing tasks also read `RESOURCES.md` and `ADMISSION.md`, including
their named milestone-specific evidence. Message/query tasks read `POLICY.md`,
`UNICODE.md` and `CASES.md`. Storage tasks read `STORAGE.md`.
This plan
describes future implementation; none of its tasks are complete merely because
this file exists. Its scope is the personal service specified in the design.

Give a model one numbered task and its prerequisite commits, not the entire
backlog as an instruction to implement everything. Each task produces one
independently green increment unless its description explicitly splits a
design checkpoint from code. If a task cannot fit a reviewable commit, split
it along the named interface before starting and update this plan. Never let
several workers independently invent a shared on-disk format or wire mapping.

The task owner must deliver production code, relevant tests, documentation
updates, and the review/ready/push record required by DEVELOPMENT.md. It must
not substitute a mock-only green result for an integration acceptance criterion.
No deployment to this development host, real email, live Migadu connection,
public ACME request, or production Stalwart operation is authorized by a task.
External test tools and source fixtures need the repository's dependency/pin
review; no task may download an unpinned container image to finish its checks.

Every task handoff uses this template:

```text
Task: Mxx, exact title
Base: prerequisite commit IDs and current origin/main
Read: DESIGN.md sections and code named by the task
Own: exact files/modules; do not edit another worker's files
Inputs: frozen interfaces and fixture versions
Output: observable behavior and interface introduced
Exclusions: features explicitly outside this task
Tests: commands plus required positive and failure observations
Evidence: failing regression before fix where applicable, checks after
Submission: one green reviewed commit, ready, pushed branch
Open issue: stop dependent implementation and record the exact ambiguity
```

## Milestones and dependency order

| Milestone | Tasks | Result |
| --- | --- | --- |
| Contracts | M01-M03 | Frozen formats, budgets, gate/dependency boundary |
| Foundations | M04-M09 | Bounded parsers, durable storage, TLS, DNS |
| Receive | M10-M12 | Local durable SMTP and gateway reception |
| Serve mail | M13-M17 | Real JMAP read/write and submission queue |
| Operate | M18-M21 | Automatic certificates, administration, backup/migration |
| Release | M22-M25 | Real client/provider fixtures, memory/crash gates, artifact |
| Later | F01-F04 | Sender verification, catch-all and td-owned crypto |

Dependency edges:

```text
M01 -> M02
M02 -> M03, M04 foundations
M03 -> M04b2c3d4b -> M04b3
M04 -> M06
M04 + M07 -> M05
M05 + M06 -> M08
M03 + M04 -> M07
M04 + M07 -> M09
M04 + M06 + M08 -> M10
M07 + M09 + M10 -> M11 -> M12
M04 + M07 + M09 -> M13
M06 + M08 + M13 -> M14 -> M15
M08 + M15 -> M16
M07 + M09 + M16 -> M17
M07 + M09 + M13 -> M18
M04 + M08 + M12 + M17 + M18 -> M19
M08 + M19 -> M20
M09 + M14 + M20 -> M21
M12 + M15 + M17 + M18 + M19 -> M22
M20 + M21 + M22 -> M23 -> M24 -> M25
```

Parallel work becomes useful after M02. For example, MIME M06, TLS M07 and DNS
M09 have separate ownership once their inputs exist. Integration owners alone
edit shared runtime wiring. A dependency denotes committed behavior, not a
concurrent branch's promise. Smaller models should not be assigned M02, M05,
M08 or M16 without an experienced reviewer checking transaction semantics.

## Interfaces to freeze before parallel implementation

These names describe responsibilities. M02 freezes the nondeterministic/store
adapter signatures and ownership/error rules in ports.rs, plus format/state
codecs and the normative protocol contracts. The owning milestone below
introduces each deterministic parser, buffer or dispatcher API before its
dependent milestones begin. For example, M06 consumes M04's committed arena
API, then M10 consumes M06's committed MIME API. Avoid empty placeholder traits
for these internal algorithms; only nondeterministic or external boundaries
need adapters. The dependency edges are the shared-interface handoff gate.

| Boundary | Inputs / outputs | Owner |
| --- | --- | --- |
| `Limits`, `ResourcePlan` | Valid config -> checked arena/slot/stack ledger | M01/M04 |
| `Clock`; shared `Entropy`, `Digest`, `Crypto` | Injected time, randomness, digest/sign; future verification | M02/M03a/M07 |
| `WireBuffer`, `Arena`, `SlotId` | Caller-owned capacity -> borrowed views / limit errors | M04 |
| `Store`, `ReadView`, `Transaction` | Bounded operations -> committed IDs and sequence | M05/M08 |
| `BlobReader`, `BlobWriter` | Chunks and offsets -> immutable published blob | M05 |
| `MimeCursor`, `MessageView` | Raw reader + scratch -> bounded part/header views | M06 |
| `TlsTransport`, `Resolver` | Fixed endpoint + deadline -> verified bounded stream | M07/M09 |
| `SmtpSession` | Peer, input chunk, limits -> reply / store request | M10 |
| `JmapRequest`, `MethodResult` | Account, tokens, read view -> streaming responses | M13/M14 |
| `Submission`, `RecipientAttempt` | Immutable envelope/blob -> journaled state machine | M16 |
| `EventSink`, `HealthSnapshot` | Typed event -> bounded log/status output | M04/M19 |

## M01 — Service skeleton, limits, and conformance inventory

**Depends on:** nothing. **Read:** DESIGN sections 1-5, 10, 15; current td-mail
JMAP client, session types, submit code and integration tests.

Create std-only `td-mta` library manifest/lock and deny-lint configuration, with
test discovery and offline gate coverage. Define typed IDs, limit/error enums,
config version, and the default resource ledger. Record bytes for every slot,
arena, scratch page and stack; don't allocate the whole message limit per slot.
Compile a table of all standard JMAP methods/properties required by advertised
capabilities and all methods currently used by td-mail. Separate mandatory
requirements from optional extension support and record RFC section references.

**Acceptance:** crate tests and strict Clippy run in both host and sandbox gate
rosters; overflow/zero/inconsistent budgets refuse. The client inventory covers
upload, structured creation, submission success filing and lost-response query.
No network listeners or capabilities are enabled by this skeleton.

M01's initial ledger is in RESOURCES.md; CONFORMANCE.md carries the complete
method/property inventory, client call sites and discovered compatibility gaps.

## M02 — Freeze store, queue, and protocol contracts

M02 is split at the format/API boundaries into independently reviewed
commits. Consumers of M02 wait for all parts; early contracts do not authorize
writing production mail or advertising capabilities.

- **M02a — Scalars, keys and framing registry:** checked borrowed codecs,
  table/key tags, exact container offsets and sequence exhaustion. Tests pin
  canonical scalar/key bytes, malformed input and cross-type import identity.
- **M02b — Rows and golden fixtures:** complete value field registry, bounded
  row codecs, full row/container byte examples and digest-provider test inputs.
  Implemented in `format/row.rs` and `tests/format_rows.rs`.
- **M02c — Runtime and protocol contracts:** continues in three bounded
  increments. **M02c1** freezes generic/local wire IDs and checked MIME-part
  locator codecs with literal fixtures. **M02c2** freezes queue transitions,
  restart/JMAP mappings, synchronization states and compiling adapter/store
  APIs in `ports.rs`/`sync.rs`, API.md and QUEUE.md. **M02c3** is completed
  in three independently reviewed parts: **M02c3a** accounts for transaction/
  reply staging and fixed worker/pool ownership; **M02c3b** freezes disk/work/
  maintenance budgets and request/result retention in ADMISSION.md; **M02c3c**
  freezes thread/search/MIME policies in POLICY.md, approved data and bounded
  NFC in UNICODE.md, and the traceable wire fixture inventory in CASES.md.
  None relaxes the M02 dependency gate for protocol consumers.

**Depends on:** M01. **Own:** module interfaces, format specification and golden
fixture descriptions. **Read:** DESIGN sections 8-11 and standards inventory.

Complete STORAGE.md's numeric field/table registry and exact byte offsets,
with golden encodings for every row, journal frame, manifest and CURRENT.
Preserve its chosen sorted-checkpoint/bounded-journal model, byte/operation
ceilings, endian rules and authority distinctions. Pin sequence exhaustion,
crash points and the source-key encoding; do not reopen the storage-engine
choice in a consumer task. Pin immutable thread assignment/anchor lookup, duplicated Message-ID handling,
and multi-mailbox membership. Freeze typed MIME part blob locators, checked
streaming decode and parent pin/reuse rules. Define the submission state transition table and
its exact standard JMAP field mapping, including uncertain outcomes and partial
recipient results. Freeze the adapter and store APIs in compiling modules.

Specify receivedAt sorting/text semantics, MIME/property/charset coverage,
initial retry/cancel behavior, error mapping, and bounded per-operation work.
Choose the minimum worker layout and TLS/cold-path budget within M01's ledger.
Pin event-stream scheduling, read-slot wait/error mappings, maintenance work
budgets and journal reservation transfer across the checkpoint commit barrier.
This is the main design review checkpoint; protocol consumers wait for it.

**Acceptance:** byte-level format examples round-trip through tiny encoders/
decoders where provided; every acknowledgement maps to a sync boundary; every
queue transition has restart and JMAP meaning. No open-ended TODO serves as a
contract. Complex codecs belong to their following task, not this checkpoint.

## M03 — Local crypto boundary, backend and gate integration

**Depends on:** M02. **Own:** td-crypto, td-mta's direct dependency and error
adaptation, eventual binary/build wiring and explicit dependency gate changes.
The following increments land separately; M03 is complete only after M03b2.

### M03a — Shared contracts and direct dependency

Create the local td-crypto crate by moving the existing nondeterministic
Crypto/Entropy/Digest ports into it, with its own fixed error enum. Re-export
the traits from mail ports and provide a total error conversion. Make the
local path the sole crypto dependency, including dev/build scope; keep the
shared crate independent of mail types. Both crates carry standalone locks and
join the discovered host/sandbox test and Clippy roster. No external backend
or new primitive implementation is introduced in this increment.

**Acceptance:** consumer compilation proves shared trait identity and error
conversion, including propagation of a partial-fill failure through a test
caller using `?`. This is interface evidence, not production token handling.
Dependency confinement rejects unreviewed direct mail dependencies. The gate
discovers both crates and the dependent mail tests when td-crypto changes. No
test-only fake is described as a cryptographic implementation.

### M03b1 — Private backend admission and offline gates

Add Rustls, aws-lc-rs and roots only inside td-crypto. Keep every public type,
error and configuration opaque to upstream types. Pin a minimal compatible
closure with one AWS-LC version pair and explicit features. Record licenses
and why each dependency is present. M03b2 records all portable native
compiler/assembler/generator inputs. td-mta's lock necessarily includes this
transitive closure, while its manifest names td-crypto as its sole crypto
provider. Amend AGENTS.md and the common host/gate checks atomically to admit
these exact named closures and preserve the std-only rules for other crates.
Do not exclude either crate from tests or Clippy.

Implement the source/cc build controls in `td-crypto/DESIGN.md`; M03b2
supplies their decoy checks. Pin the required root Cargo config and require
ancestor config.toml files to match that pin. Reject legacy config files and
automatic build.rs inputs. Guard the locks and the actual host
feature/build graph, including inactive entries and duplicate native versions.
M03b2 applies this policy to the portable target build as well.

**Acceptance:** both crates pass the same derived host/sandbox test and Clippy
commands using only verified vendored sources. Mutations to manifest, lock,
features or other roster consumers fail. Explicit-provider construction and a
native SHA-256 known answer establish that the selected backend builds; they
are not full algorithm, TLS or resource qualification. Record inactive locked
packages separately from the active graph.

### M03b2 — Portable artifact and backend boundary qualification

This milestone lands in independently checked increments:

- **M03b2a:** pinned x86-64 musl header preparation in the builder, with no
  upstream scripts, byte agreement with upstream `install-headers`, retained
  licensing and refusal of modified archives or cached output. This prepares
  one native input and does not complete M03.
- **M03b2b:** prepare the pinned upstream Rust host/target components and
  retain the declared td GNU recipe outputs in verified local caches. This
  prepares build inputs only; no portable artifact is qualified yet.
- **M03b2c:** implemented the explicit isolated build command, packaging binary,
  source/cc decoys, missing-input refusals and static ELF/clean-runtime checks
  in td-crypto/PORTABLE.md. It is separately provisioned from ordinary host
  Cargo preflights; API/TLS qualification below still gates M03 completion.
- **M03b2d1:** implemented compiler-resolved public API confinement in the
  portable command, including conditional-export and backend-type mutations.
- **M03b2d2:** implemented local TLS 1.2/1.3 handshakes, bidirectional data/
  closure, certificate-signature verification and malformed/tampered-record
  refusals with bounded fixture work in the
  clean static runtime. PORTABLE.md defines the smoke and its limits.
  All increments are required before M03 completes.

**Depends on:** M03b1. Prepare inputs in M03b2a/b, qualify the isolated
artifact and source/cc decoys in M03b2c, then complete API confinement and
TLS smoke in M03b2d before any service consumer depends on the backend.
Clear ambient native flags/tool search paths. The host admission wrapper is
initially x86-64 GNU/musl only; qualify any additional build host explicitly.
Its exact manifests declare no extra test arguments or trusted-test-root;
revisit wrapper dispatch if later manifest pins add those fields.

Provide the installed binary name from td-mta itself; no separate runtime
package. Backend construction and smoke fixtures belong inside td-crypto;
mail wiring uses only its public facade. Complete key/digest and TLS consumer
adapters remain M07. Do not advertise serving mail from this build increment.

**Acceptance:** static x86-64 musl smoke executable runs in a clean fixture;
ELF inspection finds no interpreter or dynamic dependencies. Lock/feature
changes, an unreviewed direct mail dependency or an unapproved crypto
dependency fail the gate. Public-API confinement rejects upstream type
or re-export leaks. Offline source provisioning is reproducible and has no host
libssl/td-net runtime dependency. Pin DESIGN section 3's portability toolchain
manifest, including target std, C compiler/linker/sysroot and
checksum/provenance evidence. The clean fixture builds without ambient
undeclared inputs. Establish bounded TLS smoke behavior before any service
consumer depends on the backend.

## M04 — Bounded primitives, configuration, and event records

**Depends on:** M02 for foundations; M03 for target stack qualification
(M04b2c3d4b) before M04b3. **Own:** `bounded`, `ownership`, `config`, `observability`,
`limits` and `admission` modules.

Split at these concrete boundaries before dependent milestones start:

- **M04a1:** caller-owned byte arenas, wire buffers and atomic bounded text
  formatting in `bounded.rs`; fixed capacity and explicit work/ownership.
- **M04a2:** `ownership.rs` fixed queues and checked reusable slots, including
  stale completions, cross-pool tokens, saturation and generation exhaustion.
- **M04b1:** implemented bounded physical-line framing and literal stanza
  syntax in `config/syntax.rs`; [CONFIG.md](CONFIG.md) owns the grammar,
  limits and redacted diagnostics. This accepts statements, not effective
  configurations. No file access or schema validation is implemented.
- **M04b2a:** implemented integer resource stanza decoding in
  `config/resources.rs`, reusing the existing Limits/DiskLimits/WorkLimits/
  NetworkLimits declarations. Reject duplicate sections/fields, unknown keys
  and wrong types; consume candidates through memory, admission and timeout
  planners. This is resource validation only, not a full configuration check.
- **M04b2b:** implemented typed bounded account/domain/alias candidates and
  immutable local-recipient lookup in `config/routing.rs`. CONFIG.md owns the
  target stanza binding, canonical keys, reserved postmaster behavior and
  byte layout/partition ceilings. Stanza dispatch and complete snapshot
  integration remain M04b2c3; this view has no store/protocol authority.
- **M04b2c1:** implemented the canonical visible-identity preimage encoder in
  `config/identity.rs`, including sorted unique IDs, every visible property,
  nullable ordered addresses and checked representation bounds. API.md owns
  its exact encoding. It streams to a caller sink after whole-input validation;
  it is not a full identity loader, digest provider or publication path.
- **M04b2c2:** implemented the bounded reader/statement driver in
  `config/stream.rs`. Reuse parser scratch, require actual reader EOF and a
  finished framer, bound interrupted reads, and reject read/handler errors.
  No schema, file trust or publication authority follows from its Summary.
- **M04b2c3:** SCHEMA.md specifies the complete operator schema and the
  remaining snapshot partition. Its implementation is split below; none of
  these loaders is implemented by the schema document.
  - **M04b2c3a1:** implemented common scalar values in `config/values.rs`:
    profile/DNS/certificate-name/path checks, component-wise root overlap and
    a shared mailbox key preserving sender local case. Routing alone retains
    its postmaster folding. Fixed diagnostics attach parser source locations.
    No URI/network-value parser or owning snapshot is implemented here.
  - **M04b2c3a2:** implemented `config/endpoint.rs`: numeric sockets/CIDRs,
    bounded HTTPS URIs/origins, preserved URI spelling and atomic canonical
    origin/request-target output. Shared DNS syntax plus the HTTPS numeric-host
    restriction, strict port/zone rules and mapped-peer membership are tested.
    These are value helpers only, with no network or provider authority.
  - **M04b2c3a3:** implemented caller-backed non-routing text in
    `config/text.rs`: private eight-byte spans, opaque owner-checked handles,
    checked written-prefix access, lowercase DNS/certificate copying and an
    immutable borrowed view. The 192 KiB ceiling complements routing's 320 KiB
    reservation. No owning snapshot, file/network I/O or publication is added.
    Identity/domain cells are implemented below; network cells remain c.
    The whole loader's d increment owns combined storage and static
    field/source-location diagnostic wrapping.
  - **M04b2c3b1:** implemented typed account/identity/address candidates in
    `config/identities.rs`: compact cells, shared arena ownership checks,
    forward references, ordered lists, null/empty distinctions, raw visible
    strings, signature-file references and sorted unique IDs. Fits the 8/64 KiB
    identity/address reservations, including the two resolved signature spans.
    Live borrowed text views allow protected loading to inspect paths and then
    append without freezing early. Stanza dispatch/EOF remain d; protected
    signature materialization and preimage invocation remain M04b3, using its
    80 KiB borrowed-view reservation within the control-worker stack.
  - **M04b2c3b2:** implemented `config/policy.rs` in the existing 16 KiB
    partition. The builder owns routing and preserves policy association across
    forward aliases and domain sorting. Canonical MX defaults share one global
    hostname; mode/age/certificate syntax and provenance are retained for c's
    listener and certificate graph. No DNS/policy publication is enabled.
  - **M04b2c3c1:** implemented typed resolver/relay records in
    `config/outbound.rs`: one to four numeric resolvers retain fallback order,
    with unique profile names and binary endpoints. Exactly one relay has a
    canonical DNS host, nonzero port, printable username, mandatory password
    path, optional CA path and an explicit TLS transport choice. Shared text
    owner checks protect live/frozen views; sticky failures prevent incomplete
    records. Inline metadata fits 1 KiB of global settings/headroom, with no
    extra text arena. File loading, DNS, authentication and TLS remain later
    integrations; stanza dispatch/EOF remain d.
  - **M04b2c3c2:** implemented `config/gateway.rs`: unique profile records,
    private CA paths, strict current/next leaf pins, ordered forward peer rows,
    binary duplicate rejection and per-policy/global prefix ceilings. Caller
    cells fit the existing 4 KiB gateway and 4 KiB prefix reservations. Owner
    checks and sticky failure protect live/frozen views and reused backing.
    Staged policies may have no peers; c4 validates consumers and requires peers
    for used gateways. No TLS peer authorization before M07/M12.
  - **M04b2c3c3:** implemented `config/certificate.rs`: bounded unique
    profiles, typed ACME/files modes, lexical material paths, accepted terms,
    preserved directory/contact spelling and ACME presence exactly when used.
    Cells fit the existing 2 KiB profile reservation; small ACME globals and
    builders fit existing headroom/workspace. Owner checks and sticky failure
    protect live/frozen views. M04b2c3d must reject mode-specific required and
    forbidden operator fields before constructing typed inputs. Provider verification
    remains M03/M07; managed issuance and policy publication remain M18.
  - **M04b2c3c4a:** implemented `config/listener.rs`: role-specific required
    and forbidden fields, loopback-only plaintext fixture, HTTP-01 port, checked
    SMTP pool totals and same-family bind conflicts. Require SMTP and HTTPS
    roles. Caller cells fit the 2 KiB listener reservation; owner-checked views
    retain declared fields, references and coordinates for the graph below.
    No socket is opened; IPv6 startup still needs M11's audited socket policy.
  - **M04b2c3c4b:** implemented `config/graph.rs`: consumes the listener,
    certificate, gateway and domain tables as one closed structural graph.
    Checks origin ports, profile consumption and required names, used gateway
    peers, ACME HTTP-01, MTA-STS SNI conflicts and local/upstream MX rules.
    Caller bindings fit the existing 16 KiB partition; canonical origin and
    deduplicated names share non-routing text. Tests exercise direct, gateway,
    combined and fixture ingress entirely offline. No provider or network
    authority; whole-file dispatch and actual EOF remain d.
  - **M04b2c3d1:** implemented `config/globals.rs`: server planning flag and
    explicit numeric address hints, lexical disjoint roots/defaults and fixed
    logging severity. Inline metadata and owner-bound path references fit
    existing workspace/headroom. Hostname/origin staging, raw stanza validation
    and whole-file EOF remain the dispatcher; no files or network operations.
  - **M04b2c3d2a:** implemented `config/stanza.rs`: static section/field/type
    catalog and one reusable pending non-resource stanza. Copy decoded text,
    retain presence and key/value coordinates, refuse unknown/duplicate/type
    and unconditional missing-field errors. Complete representation fits
    13 KiB; resource stanzas still use their existing direct builder. This is
    staging, not label semantics, a whole parser, EOF or runtime authority.
  - **M04b2c3d2b:** implemented `config/dispatch.rs`: connect the reusable
    stanza buffer to typed builders with strict
    version, label content, scalar semantics and conditional required/forbidden
    rules. Reject duplicate singleton headers before any field staging,
    including hostname/origin. Reject labels on resource sections before
    invoking their direct resource builder.
    Reuse one pending variant within SCHEMA.md's 13 KiB reservation; avoid
    a whole-file AST or duplicate alias arena. Before typed certificate
    conversion, enforce CONFIG.md's files-mode required chain/key and
    ACME-mode forbidden chain/key codes. Test the full presence matrix for
    both modes and every unknown/duplicate/type/missing/forbidden refusal.
    `finish_stanzas` closes resources and references over supplied statements
    only; its borrowed result does not prove reader EOF or confer authority.
    Typed handoff errors preserve helper causes and add static source context.
    Builder plus Pending fits 36 KiB; d4 still measures concurrent call frames.
  - **M04b2c3d3a:** implemented `config/storage.rs`: cold fallible allocation
    of private typed tables and both text arenas at the compiled maxima.
    Check requested and returned capacities against each implemented region
    partition;
    initialize in place without a megabyte stack temporary. Subsequent
    borrowed dispatcher builds reuse allocations after success or failure.
    Results retain exclusive storage borrowing; no owned validated snapshot
    or whole-reader authority is claimed.
  - **M04b2c3d3b:** implemented sealed per-table headers and
    `storage::Candidate`. A restricted statement sink lends only `accept`,
    preventing callbacks from replacing the builder/backing association.
    Success consumes every borrowed record before moving the owner; failure
    returns reusable storage without validated headers. Direct read-only
    identity/global/outbound views and scoped graph access borrow the owner.
    Aggregate header/candidate and existing per-partition guards account for
    payload memory; reader EOF and peak call-frame proof remain d4.
    Consume borrowed builders before moving their enclosing storage; keep
    backing tables, used prefixes and text owner together. No public loose
    rebinding, self-reference, extra snapshot copy or unsafe conversion.
    Prove every concrete snapshot partition fits its existing reservation.
  - **M04b2c3d4a:** implemented `config/load.rs`: one call consumes private
    storage and drives the restricted dispatcher sink through actual reader
    EOF before structural finalization. Private Loaded distinguishes this
    result from supplied-statement candidates; it confers no protected-file
    or runtime authority. Typed stream/schema errors return reusable storage.
    Complete fixtures cover fragmented input, final lines, late I/O/syntax/
    handler/reference failures, exhausted descriptors/text, and the unchanged
    caller-held prior candidate. Named concurrent state representations fit
    36 KiB beside the 28 KiB stream region; no new allocation is introduced.
  - **M04b2c3d4b:** implemented point-in-time integration qualification
    on the pinned release musl target, including initialization, nested
    helper/fixture-reader frames, failure/reuse, near-maximum pending text and full table cases.
    CONFIG.md defines the checked non-growing guarded stack ceiling. Later
    compiled instances, adapters/providers/finalization must be qualified.
    Ordinary host gates exercise fixture behavior but not the target ceiling.
    Preserve the 36 KiB workspace and existing 256 KiB
    control-worker stack reservations, including 80 KiB for later borrowed
    identity views. M03 provides the portable toolchain. Object-layout guards
    alone are not peak-stack proof. All combined snapshot/scratch proofs
    precede M04b3/M19 consumption; M19 tests active generation publication.
- **M04b3:** protected-file reference requirements and redacted effective
  configuration library output. Actual trusted file opening and permission
  evidence use M05 adapters; no successful full `config check` before that
  integration. Consume a structural candidate, resolve signatures/credentials
  within its remaining text capacity, materialize identities with bounded
  injected protected-input fixtures, then encode from temporary borrowed views.
  Any late failure drops the candidate. M19 owns atomic runtime publication.
  - **M04b3a:** implemented bounded content decoding for signature and relay
    password inputs through injected readers. Exact EOF, byte ceilings,
    bounded Interrupted retries, fixed redacted failures and borrowed output;
    SCHEMA.md specifies password-file bytes. Sized to reuse stream scratch.
    This adds no filesystem trust, candidate finalization or CLI success.
  - **M04b3b1:** implemented bounded referenced-file inventory, role-specific
    raw caps and private-mode requirements. Owner-bound cursor callbacks
    release candidate borrows between requests and refuse stale reuse;
    requests establish no file trust. SCHEMA.md defines M05's checks.
  - **M04b3b2a:** implemented exclusive text-input materialization from Loaded,
    stream-scratch reuse, resolved signature spans and private relay-password
    handle within the existing text arena. Independent signature-completion
    flags refuse omissions/duplicates; the decoder window clears on return or
    unwind. Failures return header-free storage with typed input context.
    This content-only stage establishes no protected-file or runtime authority.
  - **M04b3b2b1:** implemented fixed-array visible-identity assembly and
    streaming preimage encoding from resolved text, preserving list/name
    presence and declaration order within the 80 KiB view reservation.
    Validate the separate preimage ceiling before output; redact sink errors.
  - **M04b3b2b2a:** qualified the test-compiled structural/text/preimage path
    within a guarded 256 KiB portable worker mapping, including full tables,
    maximum signatures, independent limits, late errors and storage reuse.
    CONFIG.md scopes the evidence; this is not heap/RSS qualification.
  - **M04b3b2b2b:** remaining M05 adapter integration, M07 provider validation
    and streamed redacted effective output. Requalify actual production
    reader/finalizer/provider instances and combined memory before service use.
- **M04c1:** checked u64 disk/work settings and capacity-derived maintenance
  validation in `admission.rs`; configuration uses this committed plan.
- **M04c2:** charged work meters in `admission/work.rs` and checked
  network/attempt deadline budgets in `admission/timers.rs`; consumers own
  actual state transitions, idle resets and scheduling enforcement.
- **M04c3b1:** fixed logical quota groups and linear effect tickets in
  `admission/quota.rs` and `admission/logical.rs`; no physical I/O permission.
- **M04c3b2:** coordinator-owned selected/journal scalar ledger and derived
  checkpoint capacity in `admission/writer.rs`, including reserved candidate
  frames, dedicated append tickets and simulated writer-barrier transitions.
  Rollover preserves outstanding leases and stays closed for reconciliation.
  M08 supplies trusted selected/committed state and exact metadata accounting.
- **M04c3 runtime integration:** pending logical building/retention quota,
  cleanup accounting and post-checkpoint reconciliation under M08. Use the
  existing bounded logical ledger. No physical-space registry or probe gate.
- **M04d1:** implemented typed bounded event and explicit inspection encoders
  in `observability.rs`, including stable JSON Lines fields, redaction by
  default event shape, bounded UTF-8 truncation and ASCII JSON escaping.
  [OBSERVABILITY.md](OBSERVABILITY.md) owns the versioned record schema.
- **M04d2:** implemented bounded event queue/loss counters in
  `observability/queue.rs` and typed status snapshots in `observability/health.rs`.
  Local readiness is independent of relay/CA health. Unknown metrics and
  unsupported inode probes remain explicit. Full status encoding fits 4 KiB;
  a minimal unavailable frame fits main's 2 KiB framing turn. Runtime
  sink/rotation, cached publication, synchronized aggregation and rate-limited
  fallback remain M19.

All parts gate M04 consumers. Individual helper modules are not a running
allocator, scheduler, admission coordinator or service.
Checked local/wire identifiers already exist from M01/M02; M04a2 owns the
additional runtime slot tokens, and M04b2b/M04b2c3 validate configured references.

Implement reusable buffers/arenas, bounded formatting, fixed-capacity queues,
and checked identifiers. Implement the documented stanza grammar, immutable
effective configuration, alias/identity resolution and secret-file references.
Define typed JSON log/status encoders with maximum sizes and redaction. Add
config check and redacted effective-config library operations; later CLI wiring
uses these exact functions.
Implement ADMISSION.md's checked u64 disk/work configuration and logical
reservations. M05/M08 supply I/O error handling and durable effect accounting.

**Acceptance:** oversized/malformed/duplicate/unknown config fails with location
and stable codes; no secret is echoed. Resource overflow, exhausted slots and
log truncation are deterministic. Inject hostile strings to verify JSON escaping.
Bounded structures never increase capacity after construction. No networking,
store mutations, live reload or filesystem log rotation in this task.

## M05 — Immutable blobs and journal commit/replay

ADMISSION.md's logical quotas and failure/recovery contract apply before
write admission. Use std filesystem APIs and STORAGE.md's stable-path
deployment assumptions. Physical free-space admission is not part of v1.

**Depends on:** M02/M04/M07. **Own:** storage I/O adapter, blob files, journal codec,
store locking and initial replay; no search index or protocol endpoints.

Split this milestone before persistence consumers begin. Pure container codecs
need committed M07a1/M07a2 digest primitives/oracles and M07a4
Crypto::sha256/equal_digest factory operations; they do not depend on TLS
service activation. Each part lands independently:

- **M05a1 — fixed identity and journal headers:** exact FORMAT, CURRENT and
  journal-header codecs through the injected Crypto adapter. Check fixed extent,
  checksums, magic/schema/flags and nonzero generation/segment numbers before
  returning typed fields. Encoders leave caller output unchanged on returned
  errors. Use the existing literal fixtures, every truncated prefix, changed
  bytes, rehashed invalid fields and injected digest failures. This implements
  only these fixed containers, not referent validation, replay or disk I/O.
- **M05a2 — checkpoint containers:** bounded table header/record and manifest
  codecs, full row validation, digest coverage and cross-file identity/length
  bindings. Stream tables; keep only bounded manifest descriptors. Exercise
  duplicate/out-of-order keys, mismatched descriptors and selected history.
  - **M05a2a — table header and record codecs:** exact fixed table headers and
    individually checksummed borrowed records. Bound prefix lengths before
    hashing, validate full local key/row grammar and checkpoint sequence, and
    stage digest work before changing output. Test literal empty/populated
    tables, malformed/rehashed fields, arithmetic exhaustion, every truncated
    record prefix and failures in all three streaming digest updates. These
    codecs do not verify a whole table or select any persistent state.
  - **M05a2b — table validation and manifests:** bounded sorted-record traversal,
    exact count/extent and whole-file digest, manifest descriptor codecs and
    cross-file account/epoch/generation/sequence/hash checks. Carry failure
    state through streaming completion; never accept a valid header as proof
    that the table payload is complete. Persistence consumers wait for this.
    - **M05a2b1 — streamed table validation:** share bounded prefix framing,
      consume exact individual records with sticky failures, verify unsigned
      key order and declared count/extent, and compute the supplied-stream digest. Retain only the
      prior key, digest and counters; caller owns the record buffer. Completion
      describes the supplied stream, not filesystem EOF or manifest selection.
    - **M05a2b2 — manifest codec:** bounded borrowed manifest decoding and
      atomic encoding, all eleven table descriptors and at most 64 contiguous
      history descriptors. Validate local structural constraints; no path from
      disk input or selected-file authority follows from a parsed descriptor.
    - **M05a2b3 — checkpoint bindings:** match CURRENT, store identity, manifest,
      validated table summaries and journal headers. Require the caller's
      expected account/table, hash the entire manifest including its footer,
      and keep partial header checks distinct from full-file verification.
      History frame validation follows M05a3, and M05d validates the complete
      selected graph with I/O.
      Persistence consumers require all parts.
- **M05a3 — transaction frames:** exact bounded operation/header/footer codecs
  and validated sequential frame iteration. Validate header digest before using
  lengths; distinguish a short physical tail from full-length corrupt data.
  Preserve operation ordinals and return no partial successful frame.
  - **M05a3a — header and operation codecs:** fixed checked frame headers,
    exact bounded operation prefixes and borrowed locally valid PUT/DELETE/
    CHANGE values. Preserve output on encode failure; keep known Identity wire
    syntax separate from its v1 transaction prohibition. These pieces grant
    no full-frame validation or recovery authority.
  - **M05a3b — complete frames and sequential validation:** validate complete
    footer coverage, count, operation grammar and continuity before exposing
    a frame. Preserve ordinals, reject v1 Identity changes and carry failure
    through bounded sequential completion. Distinguish missing supplied header
    or body bytes from invalid complete contents: classify by supplied extent
    versus the checked header length, never by a row/operation error kind.
    Seal caller-built payloads in place with fixed header/footer scratch and
    preserve bytes on any returned error.
    Hash complete supplied journals and enforce byte and operation caps.
    Physical EOF remains with M05d;
    complete final-view transaction/reference rules remain with M08.
  - **M05a3c — selected journal summaries:** bind complete history streams to
    the indexed manifest range, extent and digest. Match active stream identity
    and the caller's pinned committed offset/sequence. Preserve independent
    checks without claiming graph completeness, physical EOF or pin ownership.
- **M05b — private storage adapter:** trusted roots, generated paths, exclusive
  lock, short/failing I/O, sync and non-replacing publication. Specify any new
  syscall surface under UNSAFE.md before introducing it. Own the deterministic
  fault-I/O model and process-death lock tests.
  - **M05b1 — canonical names:** fixed-buffer relative paths from typed account/
    blob IDs, positive canonical generation/segment numbers and known table
    names. Validate directory-entry blob names against namespace and shard.
    No filesystem access or authority follows from these names.
  - **M05b2 — std filesystem boundary:** generated paths, startup permission
    checks, cooperative writer lock and durable publication under STORAGE.md.
    - **M05b2a — lookup/root checks:** implemented bounded std path lookup,
      pre-existing symlink/type refusal, retained File metadata and mode/owner
      checks. Supervisor identity and stable roots/mounts are deployment
      preconditions. Tests distinguish retained-file identity from later
      pathname lookup and cover maximum-path allocation on host/musl.
    - **M05b2b — persistent LOCK:** implemented std `File::try_lock` with
      explicit contention, private empty single-link regular-file policy,
      opened identity validation and file/root-directory sync. `LockedRoot`
      consumes PrivateRoot and retains the lock without exposing clone/unlock.
      Tests cover independent opens, independent-process contention, process
      death, policy refusals and permission errors (where the harness identity
      cannot bypass modes). Process tests bound their ready wait and support a
      single CPU. The inode survives release/restart; no store
      recovery or service activation follows from owning this lock alone.
    - **M05b2c — durable file operations:** implement exclusive temporary
      creation, bounded reads/writes, immutable publication via `hard_link`,
      same-directory CURRENT replacement via `rename`, and explicit file and
      directory sync. Keep fixed path buffers and extend allocation/error tests.
      Checkpoint directories use exclusive creation and gain authority only
      through CURRENT. Same-filesystem publication is required; propagate
      cross-device errors. Inject failure before/after every operation and
      distinguish proven no effect, partial private output and uncertain commit.
      - **M05b2c1 — private temporary output:** implemented typed exclusive
        creation below pre-existing private account directories, lifetime-bound
        LOCK ownership, bounded sequential writes and consuming file/parent
        sync. Errors retain partial-output accounting; Drop never unlinks.
        Completed private output has bounded caller-buffer reads. Real-file
        tests and injected short/write/sync errors cover these primitives.
        The dedicated allocation probe covers successful/failed operations at
        short and maximum root paths on the pinned host/musl builds. Logical
        admission, publication and cleanup remain separate
        work.
      - **M05b2c2 — exclusive directories:** implemented one-level creation of
        the accounts entry and typed account/checkpoint directories, shared
        private-parent checks,
        umask normalization and new-directory/parent sync. Existing entries
        refuse untouched. Uncertain/created failures retain accounting; tests
        inject before/after creation and each sync. Allocation probes include
        successful and refused directory operations. Directory creation does
        not select a checkpoint or activate serving.
      - **M05b2c3 — immutable blob publication:** implemented consuming,
        account-bound, non-replacing hard links, destination-parent sync,
        temporary unlink and temporary-parent sync. Explicit failure stages
        preserve possible effects; no implicit rollback/adoption or writable
        result handle. Source identity/length/private policy are checked.
        Host/musl allocation probes cover success, collisions and errors at
        every mutation/sync boundary. Transaction/admission coupling remains
        M05c; CURRENT replacement and recovery remain separate work.
      - **M05b2c4 — fresh metadata publication:** implemented a closed typed
        destination for tables, manifests and initial journal segments through
        the same non-replacing publication sequence. Names remain bound to the
        temporary file's account; CURRENT is not expressible. Missing parents
        refuse. Tests cover all table tags and unchanged collisions; allocation
        probes cover each metadata role at maximum generation/root lengths.
        Format validation, selected reachability and append authority remain
        caller/recovery responsibilities. CURRENT replacement follows.
      - **M05b2c5 — CURRENT replacement:** implemented fixed encoded intent
        with expected absence/previous selector, same-account/epoch advancing
        generations, private exact-length/EOF checks, typed same-directory
        temporary output, file/parent sync, rename and final directory sync.
        Errors retain private/uncertain-selection effects. Fault tests pin
        sync handles, ordering and old/new bytes at every sync/rename boundary;
        allocation probes cover initialization/replacement, absence/stale
        refusal and sync/rename faults at maximum roots. Durable graph
        validation, actual barriers, reservation reconciliation and startup
        orphan cleanup remain required before activation.
    - **M05b2d — input files:** std type/link/owner/mode checks under SCHEMA.md,
      bounded reading through EOF and opened-file identity checks. Deployment
      uses the data-root owner as its trusted expected service identity.
      Configuration replacement during reload may refuse/retry; no hostile
      namespace writer or effective-UID verification claim.
      - **M05b2d1 — selected-store inputs:** implemented a typed private-file
        reader for FORMAT, CURRENT, tables, manifests, journals and blobs.
        Enforce private ancestor/file policy, identity and byte ceilings;
        bound each caller-buffer read, retire errors, require full extent and
        observed physical EOF for completion. Tests cover truncated/growing
        files, stale size, partial/failed reads, roles, modes, links and limits;
        the allocation interval covers success and refusal/error paths.
        Parser/digest/selection binding remains M05d. Operator-config/secrets
        loading remains separate; M05d4a below implements concurrent journal
        prefix I/O without claiming frame validation or a real read-view pin.
    - **M05b2e — recovery evidence:** local temporary-folder process tests
      exercise the common std API on the host. XFS deployment crash/power-loss
      qualification is release evidence; no xfsprogs prerequisite for ordinary
      tests and no runtime filesystem whitelist/profile probe.
- **M05c — immutable blobs:** admitted streamed temporary bodies, inline SHA-256,
  exact size accounting and durable non-replacing publication. Couple effect
  tickets and logical completion budgets; inject failures at each filesystem step.
- **M05d — selected-store recovery:** validate CURRENT's complete selected graph,
  replay contiguous frames, fence corruption and repair only incomplete EOF
  tails under the lock. Do not scan for a newer unselected generation or magic.
  - **M05d1 — selection loading:** implemented private FORMAT/CURRENT and named
    manifest reads through complete-extent/EOF evidence, then container and
    account/epoch/generation/digest binding. Fixed caller scratch and per-file
    read-attempt limits; errors identify the failed stage. Literal-file tests
    cover identity/checksum/size failures and missing selection without scans;
    the allocation probe covers success and missing-account refusal at both
    root bounds. Whole table/journal validation, replay, orphan accounting and
    final-view invariants remain required before activating the store.
  - **M05d2 — selected table input:** implemented header/extent binding before
    row reads, incremental prefix-bounded records with sticky failure, and
    completion through physical EOF plus whole-table manifest digest. Caller
    scratch and per-record read-call bounds prevent whole-table buffering.
    Tests exercise every table tag, literal input, multi-record scratch reuse,
    valid mismatched headers, shared prefix/body attempt limits, corrupt input,
    changed final extents and digest mismatch;
    allocation probes cover table reads/completion and refusal at both roots.
    Rows remain provisional. History/active journal reads, replay and complete
    graph/final-view validation follow before store activation.
  - **M05d3 — retained-history input:** implemented exact selected segment
    extents and header identity, incremental checksummed frame reads with a
    shared prefix/body attempt allowance, sticky failure, and completion
    against physical EOF and selected history sequence/size/digest. Literal
    tests cover multiple frames, valid identity mismatches, corrupt lengths,
    headers/footers, truncation, changed extents and selected digest refusal.
    The existing allocation interval covers frame reading and completion at
    both root bounds. Shared exact reads replace the metadata/table copies.
    ReadView change iteration still needs a separate cursor that skips PUT
    bodies. Active prefixes, active-tail repair, replay and whole-graph/final-view
    invariants remain required before activation.
  - **M05d4a — captured prefix I/O:** implemented journal-only PrefixReader
    and distinct CompletePrefix. Private-file checks permit same-inode
    monotonic growth within the opening ceiling; reads expose only the
    supplied captured bytes. Completion accepts later appends but refuses a
    missing prefix, without claiming physical EOF. Tests cover open-time
    growth/shrink/replacement, suffix exclusion, limits, truncation and sticky
    errors; the existing allocation interval covers both root bounds. Real
    pin ownership, active header/frame/selection validation and repair remain
    separate. Whole-file readers retain their exact extent/EOF contract.
  - **M05d4b — active-prefix validation:** implemented supplied active identity
    and captured sequence/offset checks before I/O, checked frame streaming
    through caller scratch, and completion against the selected active summary.
    Shared framing preserves retained history's physical EOF and active input's
    distinct prefix evidence. Tests cover empty/populated prefixes with later
    appends, invalid views/headers, cut frames, corrupt footers, physical
    truncation and final sequence mismatch. Both adapters reuse one admitted
    frame arena in the allocation fixture. Real pins/history retention,
    ReadView's separate change cursor, tail repair and final-view checks remain.
  - **M05d5a — stopped active scan:** implemented read-only whole-file scanning
    with selected header identity, bounded provisional frames and physical EOF
    completion. ScannedJournal retains the last valid boundary and incomplete
    tail extent without repair. Short journal headers, complete corrupt frames,
    gaps and impossible declared budgets refuse. Tests cover every short final
    header/body/footer length, complete corruption, short-tail gaps, exhausted
    sequence/byte/operation budgets, the physical format cap, read-budget
    failure and changed extents. Allocation probes reuse the existing arena at
    both roots. Live journal readers retain strict short-frame refusal.
  - **M05d5b — explicit tail repair:** implemented consuming repair of a scanned
    incomplete suffix. Recheck exact CURRENT and retained journal identity/
    extent, truncate with std set_len, sync_all, then confirm retained-file EOF.
    Errors retain attempted/truncated/synced effect stages; no automatic retry
    or rollback. Tests cover stale selectors, changed/replaced/nonprivate-file
    refusal and errors before/after effects. Allocation probes share the
    existing arena at both root bounds. Finish graph/replay and reference
    validation before opening pins or mutation admission.
  - **M05d6 — referenced blob integrity:** implemented supplied account/ID/row
    input with admitted length, exact private file size, incremental SHA-256
    and physical EOF/digest completion. One caller-buffer read is at most
    64 KiB; I/O/hash errors retire input. Tests cover known digest, both blob
    kinds, empty/large files, limits, corruption, changed extents and provider
    errors. Allocation probes cover success/refusal at both root bounds.
    Final owning-reference resolution and actual pins remain coordinator work.
  - **M05d7 — bounded replay overlay:** implemented immutable supplied frame
    bytes and caller-owned operation cells, complete contiguous frame validation,
    in-place descriptor sorting and latest-key lookup/iteration with tombstones.
    All operations count toward the cap; CHANGE records never become rows.
    Limits::plan checks the 32-byte compiled cell budget. Tests cover independent
    last-write comparison, sequence/ordinal ties, corruption/truncation, full
    operation capacity and scratch reuse; allocation instrumentation covers
    8192 operations and lookups/refusals.
  - **M05d8 — active-overlay loading:** implemented selected prefix loading into
    caller frame/cell arenas, early header binding, one shared 128-read budget
    and final sequence/offset binding. Refuse impossible cell capacity before I/O;
    exact per-operation capacity is checked during decode. Classify buffer-size
    refusal as InvalidInput. Retain the consumed prefix descriptor
    alongside the borrowed overlay. Tests cover suffix growth, identity and
    admission refusal, corruption/cut frames, endpoint mismatch and exact read
    budget/error boundaries; allocation probes cover short/maximum roots.
  - **M05d9 — provisional table replay:** implemented a streaming sorted merge
    of supplied checkpoint records and latest overlay entries. Bind table header
    account/epoch/base, validate input order/count/extent, retain unchanged row
    sequences and replace/drop rows according to PUT/DELETE. Callback output
    remains provisional; input or sink failure retires the merge. Tests compare
    an independent last-write map, check independent pre-output count/payload
    ceilings, wrong inputs/identities and sink errors;
    allocation probes include full-capacity overlay draining. File/digest
    bindings are connected by M05d10 below.
  - **M05d10 — selected table replay:** implemented conversion of fresh TableInput
    into a failure-sticky replay bound to LoadedOverlay's supplied view. Advance
    performs one record/merge step; finish verifies table checksum/EOF/selection
    before residual overlay callbacks. Tests also prove successful PUT output
    both before a checkpoint row and during the final drain. Completion retains
    the selected table and
    borrows the loaded prefix. Tests cover unchanged/deleted/empty tables, input
    freshness, generation mismatch, input/sink errors and late digest/extent
    failure; allocation probes reuse existing caller arenas at both root bounds.
    Checkpoint publication, final reference validation and live pins remain
    integration work.
  - **M05d11 — selected point lookup:** implemented a complete selected-table
    replay that retains only the matching row in a separate caller buffer.
    Completion exposes the final row/sequence or absence only after digest/EOF
    and residual-overlay validation. Tests cover unchanged/replaced/inserted/
    deleted/absent rows, variable and zero-byte values, wrong keys, short result
    buffers, early finish and late corruption. The allocation probe exercises
    lookup success/refusal at both root bounds. The ledger separately charges
    maximum record and result buffers. Whole-graph reference validation and
    runtime pin ownership remain coordinator work.
  - **M05d12 — selected ordered scan:** implemented a complete selected replay
    retaining only the first final row strictly after an encoded cursor, or
    the first row when no cursor is supplied. Completion returns key/value/
    sequence or proven exhaustion. Tests cover canonical encoded ordering,
    replacements/deletions/residual rows, exclusive cursors, malformed cursors,
    short output, incomplete consumption and late corruption; allocation probes
    cover both root bounds. Existing scratch partitions fund both outputs.
    Indexed iteration, whole-graph recovery and live pins remain separate work.
  - **M05d13 — mailbox parent chains:** implemented fixed-state cycle/missing-
    target validation over one supplied ReadView, one get per advance. Bind
    exact view identity before/after reads, validate rows/sequence ceilings,
    retire on errors or explicit lookup-budget exhaustion, and expose
    completion only at a root. Tests compare exhaustive small functional
    graphs against an independent visited-set oracle, plus long chains,
    changed views, malformed rows and sticky errors. The allocation probe
    covers success/cycle/missing/limit cases. Actual view integration and the
    all-mailbox/whole-graph coordinator remain separate work.
  - **M05d14a — incremental frame validation:** implemented supplied-byte
    verification with exact header, one operation per push and exact footer.
    Validate sequence/local grammar, preserve stored ordinals and enforce
    count/extent/digest before completion, with sticky push errors. Differential
    tests cover literal and maximum frames, malformed counts/bytes, Identity
    CHANGE and provider faults. Existing allocation instrumentation covers
    success and failure. This retains only digest/counters, enabling future
    retained-change input without another frame arena. File I/O, journal
    binding, change collection and ReadView integration remain separate work.
  - **M05d14b — checked frame changes:** implemented caller-slot collection of
    CHANGE type/ID/action and stored ordinal. Validate every supplied operation
    and expose the compact sequence only after consuming frame completion.
    Short capacity and malformed input retire the collector; row-only frames
    need no change slots. Preserve duplicates and all v1 actions/types. Tests
    compare mixed and maximum frames against the complete decoder, prove
    copied ownership and exercise missing operations/corrupt footer/sticky
    failure. Separate per-view change scratch permits interleaved row lookups;
    the existing allocation probe covers bounded collection after preparation.
    Physical history input, cursor policy and real ReadView pins remain future.
  - **M05d14c — incremental journal changes:** implemented checked journal
    headers and exclusive pending frames over the change collector. Validate
    sequence and aggregate budgets before operations, hash all supplied bytes
    including footer checksums, and return the existing journal Summary only
    after every begun frame finishes. Abandonment and all errors retire the
    parent. Tests compare whole-frame summaries, retained changes, empty and
    exhausted bases, journal caps, frame errors and crypto faults. The existing
    allocation interval exercises completion and abandonment. File input,
    selected history/prefix binding and ReadView cursor activation follow.
  - **M05d14d — selected history changes:** implemented immutable selected-file
    opening and complete-frame iteration borrowing per-call operation scratch
    and retaining compact change slots. Check exact extent/header/aggregate limits, bound operation
    framing and share a finite read-call budget. Consume each frame checksum
    before retaining changes; recover full scratch capacity for the next frame.
    Selected digest/EOF completion is separate from provisional frame access.
    Tests cover repeated/max frames, changed bytes, short cells, read budgets,
    I/O errors, buffer reuse and final selection. Existing allocation
    instrumentation covers both path bounds. Live cursor/floor policy and
    read-view pins remain future.
  - **M05d14e — captured active changes:** implemented selected-prefix changes
    over the same operation input as immutable history. Validate active identity
    and byte admission, retain one checked frame of compact changes and finish
    only at the captured sequence/offset. Prefix completion permits append growth
    and refuses shrinkage or partial frames. Tests cover complete/incomplete
    later appends, empty prefixes, short slots, I/O faults, mismatched views and
    endpoint mismatch. Existing allocation instrumentation covers both root
    bounds. Live cursor, history-floor and pin integration remain future.
  - **M05d14f — supplied change cursor:** implemented fixed-kind, captured-view
    policy over checked compact frames. Validate floor/endpoint/caller cursor,
    request the exact next sequence, retain frame identity and slot position,
    and return ordered matching records or explicit frame boundaries. All
    errors are terminal; completion requires the endpoint boundary. Tests cover
    maximum frames, partial cursors, filtering/actions/duplicates, empty ranges,
    view changes and frame substitution. Allocation instrumentation covers
    draining and refusal. File location, real pins and serving remain future.
  - **M05d14g — selected change routing:** implemented retained coverage checks
    and bounded source choice over selected metadata/captured view fields. Map
    an exact required sequence to a history descriptor or the active prefix,
    refusing missing history, future frames and changed identity. Tests cover
    all 64 descriptors, segment/floor/checkpoint/endpoint boundaries, missing
    coverage and invalid views. Existing allocation instrumentation covers both
    source types and below-floor/changed-view refusal. Physical frame location
    and real pins remain future.
  - **M05d14h — bounded selected frame location:** implemented opening a routed
    source and locating the target with at most one frame per advance. Hide
    earlier frames, bind supplied view identity and refuse excess sequences.
    Continue within that segment, then require its selected completion before
    returning full reusable slots. Tests cover exact read positions, locating
    before the floor, both source types, append growth, premature finish, view/
    checksum/I/O failures and endpoint refusal. Existing allocation probes cover
    slot transfer and empty-prefix reclaim. Segment transitions/live pins remain.
  - **M05d14i — sequential selected change scan:** implemented cursor/locator
    coordination with separate bounded opening, locating, draining and completion
    phases. Verify/close each source before transferring slots; retain caller
    continuation during internal work and retire the entire scan on errors.
    Tests cover source transitions, partial cursors, filtering, late corruption,
    exact continuation, I/O refusal and exhausted ranges without I/O. Existing
    allocation probes cover source transfer/reclaim. Runtime ReadView integration
    and early-page validity/pins remain future.
  - **M05d15 — direct final-row references:** implemented supplied-source
    validation with at most two typed targets and one ReadView get per advance.
    Check identity around lookups, source/target sequence ceilings, blob kinds,
    recipient ordinal bounds and lease account/expiry; omit historical IDs.
    Tests cover every table, missing/malformed targets, view movement, time
    boundary, incomplete/terminal state and fixed lookup counts. Existing
    allocation instrumentation covers the helper. Whole-graph enumeration,
    physical blob verification, parent cycles and real pins remain separate.
  - **M05d16 — direct-reference sweep:** implemented bounded enumeration over a
    supplied ReadView, one next plus at most two direct-reference gets per step.
    Check table/key progression, identity around next and finite row admission;
    detach source keys before value reuse and retain fixed per-table counts.
    Tests cover populated/empty scans, logical work bounds, exact/short/zero row
    budgets, order/type/sequence faults, lookup failures and changed-view
    precedence. Allocation probes reuse fixed fixtures. Full physical graph,
    blob bytes, parent cycles, aggregate invariants and runtime pins remain.
  - **M05d17 — exact recipient coverage:** implemented ordered submission and
    recipient validation with one next per advance, exact 0..count ordinals,
    independent stream order, identity/sequence/local checks and a finite total
    row budget. Tests cover an empty view, multiple/maximum groups, missing/extra/orphan
    rows, duplicate keys, lookup/identity faults, malformed sources and terminal
    states. Allocation probes cover fixed populated/empty fixtures. Queue state
    policy, physical view completeness and actual pins remain separate.
  - **M05d18 — all-mailbox parent validation:** implemented ordered enumeration
    plus one incremental ParentWalk at a time. Each step performs one next or
    get, with separate finite row/total-get allowances and view checks. Tests
    cover rooted/shared chains, empty views, late cycles/missing parents,
    exhausted work, corrupt/moved lookups and duplicate/descending enumeration.
    Allocation probes cover populated/empty fixtures. Physical completeness,
    configured depth policy and runtime pins remain separate.
  - **M05d19 — final blob file sweep:** implemented ordered final blob rows
    connected to private-file opening, bounded chunk hashing and digest/EOF completion.
    Separate row/total-byte admission and full identity checks precede success;
    reuse value scratch after detaching scalar source data. Tests cover empty
    files/views, cumulative limits, late corruption, missing files, source/order
    faults and view movement through every phase. Existing allocation probes
    cover fixed prepared files. Physical view completeness and actual pins remain.
  - **M05d20 — complete selected-table replay:** implemented all-table
    sequencing with byte/final-row budgets and one reusable record buffer.
    Reclaim scratch only after selected digest/EOF and residual merge completion;
    count every final row and retire failures. Tests cover all selected files,
    deletion overlays, exact/short admission, missing late files and changed
    final extents. Allocation probes check both root bounds. Retained history,
    cross-row/blob checks, actual view pins and activation remain separate.
  - **M05d21 — complete retained-history validation:** implemented sequencing
    over every selected descriptor with aggregate byte/frame admission and one
    reusable change-slot slice. Each step opens, reads one frame or verifies a
    selected segment; completion returns exact selection/counts and slots.
    Tests cover empty/multiple/maximal history, admission/capacity errors,
    missing late segments and selected digest failure. Existing allocation
    probes cover slot reclamation at both roots; the multi-segment unit test
    covers reuse. Full graph/pins/activation remain.
  - **M05d22 — stopped validation ownership:** implemented a consuming owner for
    LockedRoot with read-only selection, overlay, table and history operations.
    Borrowed metadata/inputs prevent thaw while live; consuming return restores
    mutation access without lock reacquisition. Compile-fail tests enforce the
    transfer/return lifetimes; file tests cover retained lock and validation.
    Trusted filesystem authority remains required; serving view pins, full
    selected-graph validation and activation remain separate.
  - **M05d23 — owned selected physical-file validation:** implemented a coordinator
    tying the captured active prefix to the stopped owner, admitting table and
    history budgets before I/O and reclaiming one record buffer between sweeps.
    Finish retains owner/overlay/selection with complete physical summaries and
    returns scratch only after both sweeps succeed. Tests cover phase ordering,
    buffer identity, foreign owners, early admission, late faults and retirement.
    Current selection loading, final row/blob rules and activation remain separate.
  - **M05d24 — offline validation ReadView:** implemented get/next through complete
    table replay and a retained fixed-kind change scan over CheckedFiles. Admit
    all selected source sizes and per-call work, check monotonic deadlines and
    retire all errors. Shared scratch supports interleaved row/change calls.
    Tests cover present/deleted rows, ordered EOF, history/active continuations,
    short capacity, budgets, clock/cursor errors and late refusal. Full logical
    validation, runtime leases and service activation remain separate.
  - **M05d25 — final queue consistency:** implemented current recipient
    phase/retry/uncertainty/reason and required reply-code checks in the existing
    recipient sweep. Exact groups now require correct completedAt/notification
    state and submission-wide cancellation, retaining only scalar flags. Tests
    exercise all states, definitive/uncertain results, historical replies,
    retryable unknowns, group combinations and sticky sweep failures. Transition
    history, worker fences and complete logical graph integration remain separate.
  - **M05d26 — owned final-data validation:** implemented ordered composition
    of direct references, recipient queue consistency, mailbox parent walks and
    final blob files over one offline ValidationView. Bracket every step and
    consuming completion with the deadline, compare counts against physical
    replay and retain stopped ownership independently of reusable scratch.
    Real selected-file tests cover populated/empty success and errors in every
    phase, budgets, sticky failure and deadlines after blob reads/completion.
    Recovery repair/accounting, mutation policy and activation remain separate.
  - **M05d27 — stopped journal capture:** implemented a read-only wrapper that
    derives committed prefix identity/history floor from a completed bounded
    scan and loads that same prefix into caller overlay storage. Keep repair
    capability private, report incomplete tails, require End plus physical EOF
    and release frame scratch at finish. Tests cover empty/complete/partial
    input, corruption, capacity, premature finish, late size changes, history
    floor derivation, unchanged tail bytes and downstream physical validation.
    Compare reloaded prefix digests and test same-size valid replacement refusal
    and dropping the capture before using the handed-off overlay.
    Compile-fail cases pin owner retention and absence of a repair method.
  - **M05d28 — stopped account verification:** implemented one bounded library
    entry point from actual CURRENT selection through capture, overlay, physical
    files and final-data checks. Apply one monotonic deadline across all stages
    and nested readers; return owner-bound summaries after releasing all scratch
    and input descriptors. Tests cover actual selection, empty/tail/blob cases,
    phase failures, admission, clock failures/regressions and final deadline.
    Preserve tails without repair. CLI/JSON, complete mutation policy, recovery
    accounting and activation remain separate.
  - **M05d29 — composed verification allocation probe:** exercise the complete
    stopped-account path inside the existing Rust allocation counter, at short
    and maximum roots with empty/tail and populated-blob fixtures. Keep fixture
    setup, buffer allocation and corruption writes outside eight intervals;
    require 16 valid, unchanged paired snapshots for success and errors.
    No instrumentation surface, allocator policy or dependency is added. Native
    allocation, maximum datasets, worker-stack and RSS qualification remain.
- **M05e — serialized commit publication:** connect reservations, complete frame
  append/sync and atomic sequence/offset visibility. Failed sync stops writes;
  all crash boundaries preserve acknowledged state. M08 supplies the complete
  object transaction/reference rules before protocol mutations are enabled.
  - **M05e1 — bounded frame append:** implemented a consuming complete-scan to
    immutable-frame append path using std APIs. Recheck CURRENT and inode/length,
    validate sequence/checksum/local operations and cumulative journal caps,
    bound each write and the call count, then require file sync plus exact EOF
    before returning durable endpoint evidence. Failures retire output and leave
    uncertain effects for recovery. Tests cover partial writes, sync/confirmation
    faults, pre-write refusal, premature finish, replay, large frames and caps.
    Runtime reservations, final transaction policy and visibility remain external.
  - **M05e2 — append admission integration:** implemented the reservation-bound
    adapter using the existing writer ledger and exact frame ticket. Check prior
    journal counters and actual frame bounds before any write. Hold the ledger
    exclusively, stop admission and retain busy charges on uncertainty or drop,
    and reconcile actual durable frame bytes/operations before returning evidence.
    Tests cover conservative-to-actual accounting, independent reservations,
    constructor refusal, every append fault, every abandonment phase and failed
    bookkeeping. Account/generation ownership, work/deadline orchestration and
    final mutation policy remain caller responsibilities.
  - **M05e2b — append allocation probe:** implemented sixteen observation
    intervals over short/maximum roots and success, short-write, failure and
    abandonment cases. Measure reservation creation, std append and exact
    reconciliation/disposal with every allocator counter unchanged. Keep scan,
    frame construction and fixture lifecycle cold; worker stack, maximum data
    and whole-service RSS qualification remain separate.
  - **M05e2c — reusable reconciled boundary:** implemented consuming successor
    append from ReconciledAppend, sharing the original construction and binding
    checks. Keep prior sequence/count/extent evidence and compare CURRENT and
    reopened inode/extent before each append without replaying the whole journal.
    Constructor refusal consumes the owner and needs a later rescan; uncertainty
    retains stopped admission and busy charges. Tests cover repeated successors,
    replay/accounting, changed selection/extent/inode and partial second writes.
    Live views and publication remain separate.
  - **M05e3a — owned verified store:** implemented a consuming verification
    transition that retains the locked store and copied CURRENT, view
    identity and journal summary without caller scratch or a writable root
    accessor. Refuse an incomplete tail without repair; errors release
    ownership, requiring a fresh lock and verification on retry. Consuming
    return discards this proof before restoring offline operations. This
    prepares the owner for publication; reader leases, serialized live
    append and atomic visibility remain separate.
  - **M05e3b — scoped journal publication:** implemented an owning scope with
    startup digest/extent/ledger rechecks, one serialized writer and a
    separate publication mutex. Bounded borrowed identity pins retain the
    fixed selected namespace. Publish only after sync and reconciliation;
    failures after reservation permanently retire writes, including a late
    deadline after publication. Serving query I/O, scratch leases and live
    checkpoint/retention transitions remain separate.
  - **M05e3c — publication allocation probe:** implemented sixteen observation
    intervals across short/maximum roots, repeated public commit calls, reader
    capture/drop, capacity and lock contention, constructor refusal and
    deadlines before admission or after write/sync/reconciliation/publication.
    Require unchanged Rust allocator counters; setup, session startup and
    teardown remain cold. Small frames qualify these paths; maximum data,
    native allocation, worker stacks and whole-service RSS remain separate.
  - **M05e3d — pinned query scopes:** implemented exclusive pin borrowing
    with caller selection/replay/record/change scratch. Reload the unchanged
    selection, validate its captured prefix and physical tables/history,
    then lend the bounded ReadView implementation under one deadline. Later
    appended bytes stay invisible to old pins, ignored reader errors still
    fail the scope, and failed preparation releases scratch for reuse. Only
    the owned session permits prefix queries alongside its serialized
    writer; standalone append retains stopped-store exclusion. Runtime pool
    leases and performance, allocation and whole-worker stack qualification
    remain pending.
  - **M05e3e — pinned-read allocation probe:** implemented sixteen intervals
    across short/maximum roots and successful queries, append during an old
    read scope, early/late deadlines, work/scratch refusal and ignored query
    error. Include selection/replay/file validation, get/next/changes,
    temporary cleanup and pin disposal; require every Rust allocator counter
    unchanged. Fixtures, arena creation and session startup/teardown stay cold.
    Small fixtures do not qualify maximum data, native allocation or worker RSS.
  - **M05e3f — admitted read scratch pool:** implemented exact backing checks
    and startup touching for fixed slots. Capture pairs a moved partition with
    a committed pin; refusal/drop returns resources without holding slot locks
    across I/O, publication locking or callbacks. Scoped thread handoff and
    retries reuse the same backing. Capacity, contention, poison and mismatched
    admission refuse explicitly. Existing memory reservations cover the
    bookkeeping; pool allocation and whole-worker qualification remain open.
  - **M05e3g — pooled-read allocation probe:** implemented sixteen measured
    intervals at short/maximum roots using full preallocated read backing.
    Cover queries, callback append, deadline/work refusal, full pools and
    slot/publication contention or capacity. Retry the old reader, read a
    fresh pin and drop both leases inside measurement; require every Rust
    allocator counter unchanged. Setup, startup, poison/unwind paths, native
    allocation and whole-worker/RSS qualification remain separate.
  - **M05e4a — pinned immutable body reads:** implemented captured BlobRow
    lookup and a bounded sequential digest input borrowing one pooled view.
    Consuming EOF/extent/digest completion returns a bounded random reader of
    the same file. Preserve one deadline/watermark and terminal body errors
    across all stages; retain the view pin until body drop. Existing cursor
    memory covers state. Authorization, worker integration, MIME locators and
    combined allocation/stack qualification remain separate.
  - **M05e4b — pinned-body allocation probe:** implemented twenty intervals
    across short/maximum roots, normal/empty bodies, delete-after-capture,
    missing rows, byte caps, checksum/truncation failures and late body-step
    deadlines. Include complete public pool/body paths, metadata reuse and
    owner disposal; require forty valid snapshots with unchanged Rust counters.
    Startup/teardown, maximum bodies, native allocation and full-worker/RSS
    qualification remain separate.
  - **M05e5 — explicit journal write stop:** implemented an irreversible
    atomic request with nonblocking writer-idle confirmation. Check it
    before admission, around append work and at publication completion.
    Preserve in-flight uncertainty and old read pins; cover idle, contended,
    poisoned and every commit clock boundary. Health/queue policy and
    runtime integration remain separate.
  - **M05e3 — committed visibility:** runtime integration remains pending.
    Connect pooled queries to worker ownership and queue admission; retain
    generation/history ownership across live checkpoint and retention changes.
    Keep the durable append/publication ordering and fault oracles before
    exposing any SMTP/JMAP acknowledgment path.

Implement exclusive store access, generated private paths, streamed temporary
blobs, digesting through the adapter, file/directory sync and journal commit.
Use STORAGE.md publication order, admission reservations and complete-frame
versus incomplete-tail rules. Do not deduplicate bodies or pack MIME into a
metadata value.
Implement bounded sequential replay with incomplete-tail recovery and explicit
interior-corruption refusal. Expose committed visibility only after durability.
The mail core accepts the shared Crypto interface; its deterministic fake tests
check ordering/failure behavior, not digest correctness. M05 owns td-mta
integration tests using M07's real provider through the td-crypto facade for
every SHA-256-bearing golden frame/table/blob. These introduce no direct
external dev-dependency.
Provide deterministic failure injection before/after each filesystem operation.

**Acceptance:** two writers cannot open the store; partial/short writes and
failed sync never return committed success. Every crash boundary preserves
previous commits, and uncommitted orphan data is distinguishable from missing
committed data. Malicious IDs cannot escape the root. Tiny configured limits
exercise oversize behavior without large allocations. Test full-length bad-checksum final frames without silently truncating them.
Test ENOSPC/EIO and
lock release on process death. Never claim process-kill tests alone prove
power-loss ordering; include the fault I/O model and later VM evidence.

## M06 — Streaming message and MIME representation

**Depends on:** M02/M04. **Own:** address/header/date/MIME codecs and test corpus.

Initial independently landable increments:

- **M06a — stable base64 transfer decoder:** implemented fixed copied state,
  caller-buffer progress and at most 256 transitions per turn. Charge all
  source visits/output, preserve exact malformed-input octets and
  diagnostics, and retire work failures. Tests cover M01 vectors, every
  fixture split, small output, all tail pad-bit values, charged
  backpressure, checkpoint replay and deadlines. An isolated allocation
  interval covers basic successful/malformed/refused paths. Nested source
  ownership, Unicode and protocol integration remain open.

- **M06b — bounded transfer input:** implemented a live body-reader borrow
  and checked encoded extent with caller source backing. Refill or decode
  one bounded turn under shared monotonic/deadline/work checks; initial
  identity/base64 support is extended to QP by M06l. Preserve terminal
  errors/completion and diagnose source-contract failures. Cover exact
  ranges, short reads, backpressure, clock faults, work refusal and a real
  pinned-body path; qualify core allocations with a synthetic source. Nested
  source chains and protocol integration remain open.

- **M06c — raw header scanner:** implemented fixed state and source extents,
  256-transition turns, field work charging and exact body/header boundaries.
  Handle split CRLF, bare LF/CR, folds, obsolete name whitespace, malformed
  lines and EOF without a line buffer. Enforce the supplied header allowance
  only for recognized headers; ambiguous names remain work-bounded. Cover
  fragmented fixtures, bounds, failures and an isolated allocation interval.
  Collection, aggregate MIME admission and normalized header forms remain open.

- **M06d — bounded charset decoding:** implemented exact label aliases and
  fixed-state scalar decoding for UTF-8/ASCII/Latin-1/Windows-1252. Preserve
  maximal invalid subparts across fragments, charge lookahead/replay/scalars,
  retain EOF, and retire work failures. Cover all byte values, exhaustive
  two-byte UTF-8 inputs, boundary/malformed fixtures, checkpoint work and
  allocation counters. Encoded words, NFC and protocol projection remain open.

- **M06e — transfer source checkpoints:** implemented eight private slots in
  caller-owned stage backing, cleared for each exclusive reader binding.
  Retain exact encoded/decoded cursors, decoder/pending-output state and
  completion; restore by discarding/refilling bytes at the saved cursor.
  Charge checkpoint records and fresh pre/post clock checks, retain live
  deadline/work state, and prevent cross-source or failed-owner revival.
  Cover unread input, partial decoding, completion, rebinding, bad slots,
  clock/work failure and allocation counters. Nested chains remain open.

- **M06f — bounded header unfolding:** implemented byte-preserving removal of
  CRLF/bare-LF folds before SP/HTAB, retaining bare CR and nonfold bytes.
  Fixed copied state, 256-transition turns and source/output charges support
  fragments and backpressure without line buffers. Cover every fixture split,
  short output, replay charges, sticky refusal and allocation counters.
  Charset/form composition, encoded words, NFC and protocol output remain open.

- **M06g — resident Raw header projection:** implemented immutable-source
  scalar iteration, maximal-subpart UTF-8 replacement before NUL removal,
  I-JSON noncharacter replacement, and preservation of folds/other text.
  Charge decode/filter/replay work; retain sticky failure and stable completion.
  Cover the H02 scanner-to-Raw fixture, all 66 noncharacters, boundary neighbors,
  invalid fragments separated by NUL, copied-state replay and allocation.
  Header selection, collection and JSON/JMAP integration remain open.

- **M06h — bounded body charset selection:** implemented direct label selection
  and complete-body UTF-8 prescan for absent/ASCII labels. Preserve original
  labels externally, report promotion/unknown/malformed diagnostics and charge
  scan/replay work. Cover all split points, late malformed suffixes, scalar
  boundaries, copied-state work, failure retirement and allocation counters.
  MIME parameter binding and protocol output remain open; M06i owns source replay.

- **M06i — owned transfer-to-charset replay:** implemented one immutable source
  borrow through optional complete prescan, checkpoint rewind and scalar
  decoding. Share work/clock state, preserve transfer diagnostics, and retire
  every failed operation. Cover exact ranges, short reads, direct/heuristic
  labels, identity/base64, clock boundaries, replay I/O failure and allocation.
  Nested sources, body-value response ownership and JMAP remain open.

- **M06j — plain body-value filtering:** implemented CRLF conversion, I-JSON
  noncharacter replacement and exact UTF-8 scalar-prefix byte caps. Continue
  validation after truncation, preserve NUL/other text, and retire failures.
  Cover pending CR/EOF, boundary caps, post-cap diagnostics, work/replay,
  a composed transfer-text fixture and allocation counters. HTML truncation,
  response ownership and JMAP output remain open.

- **M06k — bounded quoted-printable cursor:** implemented stable escapes,
  soft/hard line endings, literal trailing-whitespace removal and exact
  source-position replay for interior runs. Fixed copied state and charged
  256-transition turns avoid growing buffers. Cover malformed fixtures,
  fragment/output boundaries, long runs, replay/refusal and allocation.
  M06l supplies raw-source ownership; nested checkpoints remain open.

- **M06l — owned QP input and text replay:** bind QP rewind positions to the
  retained immutable raw extent; reuse resident runs or discard stale source
  buffers and yield before charged refills. Save/restore the complete decoder state in existing private
  slots. Compose with charset prescan under the same source, clock and meter.
  Cover exact ranges, small buffers, every-turn checkpoints, rewind I/O and
  clock failures and allocation. Nested decoded-source chains remain open.

- **M06m — checked offline Unicode inputs:** provision the four previously
  approved exact source/license files in the checkout. Cold std-based tooling
  verifies every size/hash/type and UTF-8 before returning a complete corpus.
  Ordinary tests verify the actual data and reject missing/tampered inputs;
  a built example provides explicit verification. No runtime data or new
  Cargo dependency is added. Generation and normalization remain open.

- **M06n — reproducible Unicode tables:** implement offline source parsing,
  checked First/Last ranges, iterative canonical expansion, composition
  exclusions, class ranges and simple lowercase mappings. Emit licensed Rust
  tables with stable formatting and verify exact regeneration, malformed
  inputs, scalar/offset bounds and the 58720-byte compiled array payload.
  M06o supplies runtime lookup and Hangul; M06p owns resident NFC equations.

- **M06o — fixed Unicode lookups:** link the generated tables through checked
  searches, fixed canonical decomposition, combining classes, simple
  lowercase and pair composition. Implement algorithmic Hangul and verify
  every official Hangul syllable vector. Cover all table entries, boundaries,
  exclusions and allocation-free copied results. M06p supplies resident NFC.

- **M06p — bounded resident NFC:** normalize immutable valid UTF-8 with fixed
  256-cell ordering, charged class replay, canonical/Hangul blocking and exact
  private source checkpoints. Borrow scratch and both live work budgets;
  retain failures across polls and aggregate interpretation-limit failures
  across projections. Prepay job records in 16-step groups, retain exact
  header-step accounting and charge streamed output through the same meter.
  Verify all official NFC equations, idempotence,
  occupied classes, fast/replay boundaries, pending decomposition, maximal
  ASCII work, deadlines and allocation-free turns. Decoded-header adapters,
  malformed-byte replacement and worker/protocol integration remain open.

- **M06q — bounded encoded-word candidate syntax:** recognize whole admitted
  tokens with known charsets, B/Q encoding, a 75-byte ceiling and Text/Phrase/
  Comment grammar. Retain optional language qualifiers without allocation.
  Precharge bounded scans; distinguish nonwords from recognized malformed
  payloads. Verify exact boundaries, context restrictions and meter failures.
  Lexical placement, payload decoding, adjacent whitespace suppression and
  normalized-header composition remain open.

- **M06r — bounded encoded-word payload decoding:** compose Q/Base64 payload
  recovery with independent per-word charset state, encoded-control removal
  and I-JSON noncharacter replacement. Keep a fixed copied cursor, charge
  payload/charset visits and replay, and latch work failures. Verify literal
  malformed-input oracles, per-word separation, intermediate checkpoints,
  deadline/exhaustion behavior and allocation-free polls. Header placement,
  whitespace suppression and normalized-header composition remain open.

- **M06s — resident unstructured header decoding:** compose unfolding, initial
  SP removal, whole-token encoded-word placement, conditional adjacent-word
  whitespace suppression, UTF-8 replacement and scalar filtering. Retain raw
  offsets for bounded candidate/whitespace replay without copying the header.
  Verify long literals/gaps, default-budget maximal ASCII, copied states,
  work/deadline failures and allocation-free turns. NFC composition and
  structured header-form authorization/parsing remain open.

- **M06t — normalized unstructured headers:** connect resident decoding to
  canonical normalization with exact word/charset/decomposition checkpoints,
  private aggregate charging and bounded header turns. Keep all source state
  within the four 256-byte checkpoints and the existing 4 KiB NFC region.
  Verify cross-word composition, overflow replay without prefix rescans,
  filtering, limits, deadlines, maximal ASCII and allocation-free output
  charging. Structured forms and protocol/worker integration remain open.

- **M06u — header property selection:** parse parameterized header keys and
  convenience aliases with exact form/occurrence semantics and RFC field
  restrictions. Preserve request capitalization in borrowed state. Verify
  bounded long-name scans, work refusal, syntax/form errors and allocation
  independence. Field collection, structured values and JMAP wiring remain
  open; a permitted Text form alone does not authorize unstructured decoding.

- **M06v — resident header occurrence traversal:** combine the raw scanner
  with bounded case-insensitive name comparison. Stream all occurrences or
  retain only the last, returning unchanged absolute value extents. Require
  complete source and provisional-result retirement. Verify duplicates,
  absence, long names, malformed body boundaries, limits and allocation-free
  traversal. Source capture, value projection and protocol wiring remain open.

- **M06w — structured comment/whitespace cursor:** scan optional CFWS only at
  caller-authorized grammar positions. Preserve top-level raw comment extents,
  bound nesting/UTF-8/fold lookahead, and distinguish malformed syntax from
  nesting/work refusal. Verify escaped delimiters, obsolete controls, Unicode,
  long comments, EOF and allocation-free turns. Value grammars, comment text
  decoding and structured-form publication remain open.

- **M06x — resident date-time interpretation:** compose CFWS with bounded
  lexical/grammar state and validate the complete field before returning
  Gregorian components and a known/unknown offset. Cover obsolete years,
  zones and adjacency, weekday agreement, clock/day ranges, malformed tails,
  long inputs, sticky resource refusals and allocation-free polling. UTC
  conversion, RFC 3339 rendering and JMAP projection remain separate.

- **M06y — ordinary date projection:** validate copied components and render
  ordinary dates into fixed caller storage, normalizing known offsets to Z
  while preserving unknown -00:00. Bound month/year carries and output work;
  retain an explicit unverified outcome for leap-second input. Cover date
  boundaries, large/nonhour offsets, hostile components, capacity/work
  refusals and allocation-free formatting. M06ab qualifies pinned leap
  insertions; JMAP form publication remains open.

- **M06z — delimited structured tokens:** validate raw quoted strings and
  domain literals with fixed escape/fold/UTF-8 state. Preserve complete token
  extents, obey enclosing field boundaries and retain work failures. Cover
  escaped delimiters, obsolete controls, long tokens, precise charges and
  allocation-free polling. Unquoting/display decoding, complete address and
  MessageIds forms, and publication remain follow-on work.

- **M06aa — resident MessageIds lists:** compose CFWS and delimited tokens
  with bounded atom/grammar state. Return provisional raw segments omitting
  grammatical CFWS and outer angles; authorize obsolete discarded phrases
  only for References/In-Reply-To. Cover complete-field rejection, Unicode,
  quoted data, long atoms, nesting/work refusal and allocation-free parsing.
  M06ac supplies unfolding and scalar projection; response publication and
  method integration remain open.

- **M06ab — pinned positive leap seconds:** add the operator-approved IANA
  source with exact offline verification and deterministic static generation.
  Qualify listed UTC 23:59:60 values after offset normalization with fixed
  work and no allocation; retain explicit unverified outcomes for all other
  leap components. Cover all insertions, source corruption, malformed rows,
  numeric calendar conversion, search bounds and capacity/work refusal.
  JSON/JMAP publication and complete worker composition remain open.

- **M06ac — validated MessageIds text:** finish complete syntax validation
  before replaying the same field under the live meter. Unfold raw parts,
  decode UTF-8 through a fixed one-byte handoff and replace noncharacters
  without NFC, unquoting or encoded-word interpretation. Cover malformed
  tails, long/folded/obsolete/Unicode text, exact charges, sticky failures
  and allocation-free polling. State fits 384 bytes. JSON serialization,
  response admission and complete worker qualification remain open.

- **M06ad — resident URLs form:** validate a complete RFC 2369 list before
  charged replay without prefix recovery. Remove outer comments/brackets
  and internal whitespace; check generic ASCII URI syntax and preserve
  spelling. Handle List-Post NO as an explicitly authorized empty list.
  Cover percent escapes, authority/path/query/fragment, bounded IPv6 and
  scalar IPvFuture, malformed tails, long data and allocation-free refusal.
  No URL fetching, scheme execution or response publication is authorized.
  JSON serialization and complete worker qualification remain open.

- **M06ae — address recovery boundaries:** partition raw field values only
  at commas/semicolons outside quotes, comments, angles and domain literals.
  Retain empty items and separator identities for later group grammar;
  unmatched constructs consume and flag the remaining field tail. Bound
  nesting and byte steps without recursion or strings. Cover escapes,
  raw invalid bytes, long input, exact charges and allocation-free refusal.
  Address/group grammar, fallback projection and display-name NFC remain
  follow-on work; boundary completion grants no SMTP or JMAP authority.

- **M06af — single addr-spec grammar:** share the identifier parser's
  local/domain states with a private bare mode requiring full-candidate
  EOF. Preserve public MessageIds list semantics and raw addr-spec parts
  without synthetic delimiters or a copied address. Cover obsolete CFWS,
  quoting, UTF-8, malformed tails, exact work and allocation-free refusal.
  Complete mailbox/group parsing, raw fallback/display projection and NFC
  remain follow-on work; SMTP envelope validation remains separate.

- **M06ag — resident phrase tokens:** share fixed word/CFWS grammar through
  a private purpose without changing MessageIds or addr-spec behavior.
  Retain token kinds and exact leading/trailing gaps for later display-name
  decoding and encoded-word placement. Cover adjacent/quoted/obsolete words,
  malformed tails, Unicode, long input, exact work and allocation-free
  refusal. Complete mailbox/group assembly, display decoding/NFC and
  publication remain follow-on work.

- **M06ah — resident single mailbox:** compose shared structural tokens,
  phrase and addr-spec grammar with exact obsolete route validation.
  Return only final raw name/address extents after complete-item success;
  choose the first immediately trailing comment when no phrase exists.
  Cover protected delimiters, route slots, name precedence, malformed tails,
  long Unicode, precise work and allocation-free refusal. List/group
  recovery, display/address projection, NFC and publication remain open.

- **M06ai — resident address/group assembly:** retain raw boundary colon
  metadata and compose phrase/mailbox parsing with deterministic item
  fallback. Preserve named groups and consecutive unnamed runs; ignore
  empty slots, close missing semicolons at EOF and treat stray semicolons
  as recovery boundaries. Keep all events provisional until field completion.
  Cover group/name transitions, protected colons, malformed tails, long
  input and allocation-free refusal. Text/NFC projection, JSON response
  storage and composed worker qualification remain open.

- **M06aj — address text projection:** reuse the MessageIds conversion
  engine through private parsed/fallback purposes. Validate parsed addr-spec
  before output; trim/unfold/repair fallback text without address grammar.
  Preserve spelling and literal controls without NFC or encoded-word
  interpretation. Cover malformed tails, invalid UTF-8, noncharacters,
  exact work, long input and allocation-free refusal. Display-name decoding,
  NFC, JSON output and complete worker qualification remain open.

- **M06ak — validated phrase replay:** consume whole-phrase success into
  an opaque source-bound proof, then provide a small Copy token replayer.
  Preserve exact token/gap extents; every replay charges the live external
  meter without copying budgets. Cover grammar agreement, checkpoint suffixes,
  work refusal and allocation-free traversal. Encoded-word placement, name
  scalar projection, NFC integration and publication remain follow-on work.

- **M06al — phrase display-name scalars:** bind the completed phrase proof
  to the exact range of its whole field. Use actual neighboring bytes for
  encoded-word placement, unquote/unfold literals and trim a sole quoted word.
  Preserve semantic CFWS and conditional adjacent-word whitespace suppression.
  Share one state slot between replay and word decoding; retain private token
  checkpoints and live work charging. Cover literals, encoded text, context,
  copies, exact work and allocation-free refusal. Comment names, NFC and
  JMAP publication remain open.

- **M06am — normalized phrase names:** connect the validated phrase scalar
  source to NFC with exact field/range/turn checkpoint identity. Retain live
  aggregate charging and existing scratch/cursor ceilings. Cover canonical
  and Hangul composition, hostile overflow, decoder/decomposition restoration,
  exact prefix visits, maximal ASCII, terminal refusal and allocation-free
  composition. Comment fallback, whole-field publication and worker resource
  qualification remain open.

- **M06an — normalized fallback comment names:** validate the separately
  selected exact comment into an opaque proof; decode original placement,
  nested/escaped text and explicit whitespace/filtering policy. Compose its
  Copy scalar source with NFC under the existing live budgets and fixed
  source/cursor/scratch ceilings. Cover copied suffixes, hostile normalization
  replay, terminal refusal and allocation-free composition. Whole-field
  admission/publication and worker resource qualification remain open.

- **M06ao — normalized scalar JSON output:** borrow the selected NFC cursor
  and serialize one quoted JSON string using fixed escaped-byte staging.
  Precharge output once before fragmentable copying, retain sticky refusal
  and final deadline checks, and expose only provisional output. Cover all
  scalars, one-byte sinks, exact charges, replay and allocation-free operation.
  Field admission, retained response ownership and publication remain open.

- **M06ap — Raw/address JSON strings:** extend the bounded serializer with
  borrowed Raw and parsed/fallback address sources and the live job meter.
  Preserve identity without NFC; retain decoder work and charge escaped JSON
  separately. Cover tiny output slices, repairs, exact layered charges and
  allocation-free refusal. Whole-field/list assembly and publication remain
  open.

- **M06aq — reusable private JSON framing:** extract the fixed framing state
  behind a private scalar/charge seam while retaining the public borrowed API.
  Let an owner retain parser/frame state by value and borrow disjoint fields
  per poll; require stable source/meter identity and whole-property retirement.
  Preserve bounds, exact charges, fragmentation and failure behavior. NFC
  workspace ownership and complete field publication remain separate.

- **M06ar — aggregate header selection:** thread typed live email budget
  charging through raw scanning and field-name comparison. Precharge actual
  visits and bounded transitions, retain private step credit, and preserve
  ordinary job-only entry points. Cover exact charges, long fields, refusal
  before child work, repeated projections and allocation-free aggregate
  exhaustion. Remaining form grammar/conversion and publication stay open.

- **M06as — aggregate Raw conversion and JSON:** own Raw decoding and private
  prepaid credit while borrowing the live email/job budgets. Precharge byte
  visits and transitions, preserve identity, and reuse bounded JSON framing.
  Cover exact costs, repairs, aggregate/job/output refusal, non-copyable state
  and allocation-free fragmented output. Whole-field ownership and publication
  remain separate.

- **M06at — provisional Raw property values:** compose aggregate selection,
  conversion and JSON into last/all values with null/empty-array absence.
  Move the same live budget borrows through successful per-value handoffs;
  stage only bounded punctuation and retain terminal whole-property failure.
  Cover exact layered charges, tiny sinks, late refusal and allocation-free
  assembly. Other forms and unpublished response-spool retention remain open.

- **M06au — provisional Text property values:** share the value coordinator
  through private statically selected projections, retaining Raw semantics.
  Admit only unstructured Subject/Comments with charged name comparisons;
  other Text grammars remain unsupported, including Keywords and MIME fields.
  Move NFC scratch and live budgets into each normalized Text source and back
  only after successful completion. Cover words/folds, scratch overflow/reuse,
  exact composed charges, late failures and allocation-free output. Other
  parsed forms and unpublished response retention remain open.

- **M06av — budgeted resident Date parsing:** reuse the Date and CFWS parsers
  through private charged work, retaining the plain APIs' exact behavior. Own
  original job/email borrows and non-copyable credit; distinguish aggregate
  exhaustion from malformed dates. Pin EOF, fold and revisit costs, fairness,
  pre-access refusal, field reuse and allocation-free long-comment parsing.
  Aggregate date formatting and complete Date property JSON remain open.

- **M06aw — budgeted Date formatting:** route the existing checked formatter
  through the same job/email budgets, preserving ordinary and pinned leap
  outcomes. Prepay bounded interpretation, charge output exactly once and
  preserve untouched buffers on refusal. Cover exact costs, repeated calls
  and allocation-free formatting. Complete Date property JSON remains open.

- **M06ax — provisional Date property values:** compose selection, budgeted
  parsing and formatting through the shared value core. Stage quoted dates or
  null in 27 inline bytes; retain a separate unverified-leap diagnostic across
  selected values. Cover last/all, exact charges, resource/late refusal,
  handoff guards and allocation-free output. Other structured forms and
  unpublished response retention remain open.

- **M06ay — budgeted resident MessageIds parsing:** retain original job/email
  budgets across CFWS, delimited tokens and shared identifier grammar. Share
  Raw/Date parser charging without changing plain APIs; propagate typed
  aggregate refusal. Cover exact costs, every partial budget, provisional
  event retirement, field-credit isolation and allocation-free long input.
  Aggregate conversion and complete MessageIds property JSON remain open.

- **M06az — budgeted validated MessageIds conversion:** carry original budgets
  through whole-field validation, replay, unfolding and scalar conversion.
  Bound private unfolding turns and preserve intermediate output charges;
  make unfolder failures typed and charge before input access. Cover partial
  budgets, provisional retirement, exact field credit and allocation-free
  long conversion. Complete MessageIds property JSON remains open.

- **M06ba — provisional MessageIds property values:** compose charged field
  mode selection, whole-field conversion and nested JSON arrays with original
  budgets. Permit only checked malformed validation to become null; preserve
  identifier identity, escaping and diagnostic scope. Cover exact composed
  charges, handoff guards, late refusal and allocation-free short drains.
  Other structured forms and unpublished response retention remain open.

- **M06bb — budgeted resident URLs:** retain original job/email budgets across
  whole-field validation, URI parsing and replay. Share the MessageIds
  conversion charging policy; admit lookahead before access. Pin exact
  two-pass/IPv6 costs, partial cutoffs, field credit and allocation-free long
  input. URL property JSON and unpublished response retention remain open.

- **M06bc — provisional URLs property values:** share inline list/string
  framing with MessageIds through static adapters. Charge List-Post mode
  selection, preserve URI spelling and checked malformed-only null handoff.
  Cover exact composed charges, per-turn limits, partial resources, late
  refusal and allocation-free drains. Other structured forms and unpublished
  response retention remain open.

- **M06bd — budgeted resident address/group parsing:** thread original job
  and email budgets through boundaries, CFWS, phrase/mailbox/route grammar
  and raw recovery. Preserve typed resource failure and provisional extents.
  Pin EOF, private credit, partial cutoffs, late group refusal and fixed-state
  allocation evidence. Text conversion and property composition remain open.

- **M06be — budgeted resident address text:** reuse private conversion
  ownership for Parsed addr-spec and raw Fallback text. Charge validation,
  replay, trimming, unfolding and scalar output to original budgets; preserve
  identity spelling and selected encoding diagnostics. Cover partial limits,
  private credit and allocation-free long input. Property JSON remains open.

- **M06bf — selected display-name normalization:** validate one admitted
  phrase/comment extent under original budgets, consume its source-bound
  proof and hand the same budgets/scratch to NFC. Preserve typed failures,
  private credit and selected diagnostics. Cover exact composed costs,
  partial limits, overflow replay and allocation-free handoff. Name JSON
  and address-property composition remain open.

- **M06bg — budgeted address/name JSON strings:** borrow original-budget
  address and selected-name owners into the existing fixed JSON frame.
  Charge exact output before staging; preserve identity, NFC, diagnostics and
  typed refusal. Cover composed costs, short drains, partial resources and
  allocation-free long input. Complete address properties remain open.

- **M06bh — provisional Addresses property values:** flatten groups into
  name/email objects while retaining one mailbox and the suspended parser.
  Consume completed name/address children back into original budgets and
  scratch, preserving field credit and selected diagnostics. Pin composed
  costs, partial limits, late failure and allocation-free one-byte drains.
  Grouped values and unpublished response retention remain open.

- **M06bi — provisional GroupedAddresses property values:** share static
  address composition while retaining named, empty and unnamed groups.
  Normalize selected group names through the same reusable child/scratch;
  preserve original budgets and flat-form semantics. Pin group transitions,
  composed costs, partial limits, late failure and allocation-free drains.
  Remaining Text grammars and unpublished response retention remain open.

- **M06bj — additional unstructured Text fields:** admit Content-Description
  and user-defined X- fields under their MIME encoded-word rules. Prepay
  bounded name/prefix comparisons, preserve existing Subject/Comments costs,
  and keep other grammars explicit refusals. Cover Email/BodyPart contexts,
  selection/diagnostics, admission limits and allocation-free overflow replay.
  Structured Text grammars and unpublished response retention remain open.

- **M06bk — shared bounded JSON string framing:** move generic scalar escaping,
  quote staging, backpressure and refusal latching into std-only td-json.
  Keep mail decoding/NFC, budgets, deadlines and error adaptation local. Share
  escaping with the existing Json writer, pin the local source graph and
  portable staging, and preserve public mail behavior and memory ceilings.
  Library tests cover scalar and framing policy; mail allocation intervals
  exercise the shared implementation. No new external dependency is added.

- **M06bl — bounded header-form dispatcher:** select one existing coordinator
  from the authorized property form and retain it inline. Preserve original
  budgets, scratch, diagnostics and typed refusal without another parser or
  dispatch charge. Pin all seven forms, final admission and partial-output
  retirement, including allocation-free construction and short drains.
  Structured Text grammars and response-spool publication remain open.

- **M06bm — structured Text placement:** admit Keywords and List-Id with
  fixed lexical quote/comment/identifier state in the resident scalar decoder.
  Preserve Text punctuation while recognizing only original phrase/comment
  encoded words, charging comparisons and lexical transitions. Reuse NFC,
  overflow replay, JSON framing and the original email/job meters. Cover
  one-byte drains, grammar-specific alphabets, malformed/escaped placement,
  diagnostics, scratch reuse, fixed-state replay and allocation-free output.
  MIME parameter Text grammars, part traversal and response retention remain
  open; this decoder is no structured-creation or list-identity authority.

- **M06bn — MIME parameter-field Text:** admit Content-Type and
  Content-Disposition with original comment-only encoded-word placement.
  Preserve ordinary/extended parameter spelling and quoted syntax before
  filtering and NFC; do not apply filename compatibility or RFC 2231
  decoding to Text. Reuse bounded lexical state, charged NFC replay and
  shared JSON framing. Cover short drains, quote/comment recovery, syntax
  injection from decoded words, aggregate refusal and allocation-free output.
  Derived parameter metadata, MIME part traversal and retention remain open.

- **M06bo — bounded MIME field syntax:** compose existing CFWS and
  quoted-string validators for Content-Type, Content-Disposition and
  Content-Transfer-Encoding. Return raw type/token/parameter extents in wire
  order with fixed state; retain duplicate and extended spelling for later
  metadata selection. Require complete suffix validation before accepting
  provisional events. Carry original job/email budgets, charged source
  revisits and sticky syntax/depth/resource failures. Cover long input,
  escaped/folded values, every partial admission and allocation-free turns.
  RFC 2231 assembly, derived metadata and part traversal remain open.

- **M06bp — shared bounded header lexing:** move CFWS and delimited-token
  state into std-only td-header with caller-owned visit/record admission.
  Preserve mail errors, complete-field slicing, deadlines, original budgets,
  charged rereads, per-turn ceilings and fixed memory through thin adapters.
  Remove the old grammar implementations atomically; retain mail composition
  and allocation fixtures and add independent shared-crate lexical/refusal
  cases. Pin the local manifest/lock and source staging without widening the
  private crypto graph. Metadata assembly and part traversal remain open.

- **M06bq — first-valid MIME metadata fields:** compose one raw-header scan
  with the complete resident MIME field grammar. Retain one validated
  Field/Head per kind, skip malformed occurrences and ignore later duplicates
  after success. Apply normal/digest-child type defaults only after the whole
  section completes. Retire all selection on raw-header/depth/job/aggregate
  refusal; carry original budgets and fixed state through final admission.
  Cover defaults, source extents, prefix/EOF, long Unicode and all partial
  budgets, cached completion, compile-fail ownership and allocation-free
  constructor/drain/refusal. Case/value projection, RFC 2231 assembly and
  MIME part traversal remain open.

- **M06br — shared MIME parameter-name classification:** put bounded RFC
  2231 suffix spelling in std-only td-header and bind it through a thin mail
  facade to original job/email work. Retain ordinary/extended/section or
  malformed form with raw base extents; check numeric overflow and leading
  zeros without a section table. Cover exact cost, fixed turns, long input,
  replacement-callback refusal, aggregate cuts, final deadline, ownership
  and allocation-free construction/drain. Full candidate validation, value
  assembly and MIME part traversal remain open.

- **M06bs — shared MIME parameter octets:** validate complete token/quoted
  spelling and project bounded ordinary/extended octet events through
  std-only td-header. Remove quotes/pairs and logical folds, distinguish
  charset/language/data, require prefix delimiters and complete percent
  triplets, and preserve decoded NUL/non-UTF-8 for later conversion. Bind
  original work through a thin mail facade, share zero-count admission with
  the name classifier, and share passive language grammar with encoded
  words. Cover complete/malformed values, all decoded octets, long fixed
  turns, every job/aggregate refusal, sticky final checks,
  ownership and allocation-free construction/drain. Numbered candidate
  replay, display conversion, boundaries and MIME traversal remain open.

- **M06bt — shared unquote/fold reader:** atomically replace the MIME
  value, phrase and comment projection grammars with stateless std-only
  td-header::projection. Preserve escaped fold provenance, exact source
  positions, distinct invariant errors and each callback's EOF admission.
  Leave original placement, work/failure ownership and normalization in
  mail. Pin maximal six reads, all octets and every refusal in shared
  fixtures; pin mail EOF differences and retain placement, fixed-turn and
  allocation qualification. Complete parameter-family selection, derived
  metadata and MIME traversal remain open.

- **M06bu — complete MIME parameter families:** select the first ordinary
  value or a complete preferred extended family with fixed-state charged
  field replay. Reject duplicate/mixed singles, gaps, duplicate indices,
  malformed names and incomplete percent escapes without selecting subsets.
  Validate every value before publishing passive source plans; distinguish
  extended rejection from absence and preserve fatal work/nesting/field
  refusals. Qualify fixed turns, original budgets, final admission, live
  ownership and allocation-free construction/replay. Derived octet drain,
  charset/display/boundary projection and MIME traversal remain open.

- **M06bv — validated MIME parameter octet replay:** validate a complete
  family before emitting any provisional octets; reuse one inline selector
  for exact-value or per-index field replay under the original allowances.
  Preserve role ordering, raw plans and rejected-family diagnostics, and
  refuse replay failure without another fallback. Keep word placement bound
  to original spelling. Qualify mixed sections, invalid later values, every
  validation/replay budget cut, final admission, Unicode fixed turns and
  allocation-free construction/drain. Charset/display/boundary projection,
  MIME traversal and full-worker qualification remain open.

- **M06bw — literal MIME parameter scalars:** share the existing charset
  alias table with a bounded incremental label matcher; compose validated
  Name/Filename replay with one decoder across mixed sections. Recover
  unknown/empty declared labels explicitly, retain native unlabelled UTF-8
  display conversion and distinguish charset/family diagnostics. Preserve
  scalar controls/noncharacters and original-source word placement for later
  display filtering/NFC. Qualify aliases, split and malformed data, all work
  cuts, fixed turns, live ownership and allocation-free conversion. Complete
  filename compatibility/display, boundaries, traversal and worker resource
  qualification remain open.

- **M06bx — MIME parameter display scalars:** bind ordinary filename/name
  compatibility to the original selected raw quoted value; recognize whole
  contiguous words at actual unescaped LWS/quote boundaries. Preserve literal
  extended/numbered spelling, apply display scalar filtering and retain
  separate charset/family diagnostics. Keep one inline active decoder and
  original allowance/credit through handoff and charged completion. Qualify
  escaped placement, all budget cuts, maximal recognition, long conversion,
  live ownership and allocation-free intervals. NFC, cross-field filename
  precedence, output retention, boundaries, traversal and complete-worker
  resource qualification remain open.

- **M06by — shared charged character projection:** extract strict logical
  UTF-8 assembly into td-header; migrate phrase, comment and ordinary MIME
  display consumers atomically. Retain exact source/EOF and local verification
  charges, first-octet escape provenance and existing placement machines.
  Centralize all mail noncharacter predicates while keeping caller-specific
  control/output rules. Qualify UTF-8 widths/errors, every callback refusal,
  existing work/placement/NFC fixtures and Rust allocation intervals. NFC
  engine extraction, parameter checkpoints, retention and traversal remain
  open.

- **M06bz — shared bounded canonical composition:** extract the existing NFC
  engine into td-nfc and migrate mail atomically. Keep pure checkpoints/table
  policy separate from original live admission, preserving source/cursor/scratch
  ceilings, work counts, sticky refusal and exact unfinished-segment replay.
  Admit the std-only local crate in manifest/lock/staging/reader closure and
  pin its package-only graph. Preserve all mail Unicode/checkpoint/allocation
  qualification and add standalone engine contracts. Parameter checkpoints/NFC,
  retention, selected boundary metadata and traversal remain open.

- **M06ca — original-source parameter NFC:** add opaque pure lexical and
  private composite checkpoints; bind them to shared normalization under the
  original work/header/credit and scratch owners, using the supplied Tick
  for each call. Preserve whole-family
  validation, compatibility placement, scalar filtering and sticky refusal.
  Qualify the larger separate source within existing parser/conversion
  reservations without widening header-source ceilings. Filename precedence,
  retention, selected boundary metadata and traversal remain open.

- **M06cb — retained filename precedence:** use exact first-valid selected
  disposition/type fields and complete extended/ordinary candidate rules.
  Treat selected empty output as present. Normalize into caller-reserved
  backing, charge each scalar before copying and retire on capacity/work/
  interpretation failure. Qualify original cuts, sticky retirement and
  fresh final admission. Selected boundary/charset metadata, traversal and
  complete worker/native/RSS qualification remain open.

- **M06cc — selected protocol parameters:** retain selected boundary/charset
  ASCII bytes with exact spelling and fixed shared grammar classification.
  Preserve complete extended-family choice without ordinary retry after
  protocol invalidity; distinguish absent, invalid, known and unknown labels.
  Fund source replay, qualifier/alias work and output through original owners.
  Qualify bounded turns, sticky refusal and fresh passive handoff. Multipart
  traversal, body defaults/recovery and complete worker/RSS remain open.

- **M06cd — resident MIME delimiter events:** extract pure bounded line-prefix
  matching into td-header, compose original-job body scanning and exact
  accepted-ending extents in mail, and preserve deterministic prefix/suffix
  recovery. Qualify turn seams, checked offsets and sticky/fresh refusal.
  Parent-first child clipping, explicit-frame traversal, transfer sizing and
  descriptor/publication authority remain open.

- **M06ce — private polling composition:** separate protocol/delimiter
  progress from exclusive public budget/output owners and share metadata
  context polling. Preserve original admission, prepaid credit, retained
  output and sticky retirement. Extract only pure detached delimiter state
  to td-header. Qualify public equivalence and movable private progress;
  explicit-frame traversal and simultaneous memory qualification remain open.

- **M06cf — complete resident structure:** compose fixed-frame traversal,
  parent-first clipping, root/child aggregate header counts, preorder passive
  cells and exact transfer sizes. Qualify outer-prefix precedence, same-length
  suspended boundaries, digest defaults, no implicit attached recursion,
  missing-close diagnostics, structural/original-work cuts and fresh handoff.
  Keep cells hidden until complete success. Retained part metadata, JSON/body
  lists, locator issuance and streaming/worker/native/RSS remain open.

- **M06cg — retained selected part headers:** compose first-valid selection,
  lowercase type/disposition, exact charset and NFC filename into separate
  caller windows. Serialize after traversal, preserve original owners and
  selected-empty/family diagnostics, and hide the complete result until every
  phase succeeds. Qualify aggregate/job cuts, capacities, defaults, duplicates,
  fresh handoff and Rust allocations. Content-ID/language/location, body lists,
  JSON, locators and streaming/worker/native/RSS remain open.

- **M06ch — iterative resident body lists:** classify complete retained heads,
  consume complete preorder evidence with fixed frames, apply scoped alternative
  fallback/related/name rules and derive attachments from final membership.
  Qualify the RFC A..K oracle, nested disabled channels, structural/capacity/work
  cuts, fresh original handoff and Rust allocation. Automatic composition,
  remaining part headers, JSON, locators and streaming/worker/native/RSS remain
  open.

- **M06ci — shared resident extents:** extract passive absolute slice
  mapping into std-only td-header and atomically migrate header
  selection/value composition, metadata, part headers and traversal.
  Preserve consumer error mapping and original source/budget owners. Shared
  literal bounds fixtures and the consumer suite qualify the migration;
  automatic coordination and worker/native/RSS remain open.

- **M06cj — Content-Language values:** compose shared CFWS/tag spelling into
  bounded complete-value list syntax, preserving ordered original-case tag
  extents. Bind original job/email budgets, sticky refusal and fresh handoff.
  Qualify literal RFC lists, malformed tails, all allowance/deadline cuts and
  Rust allocation. Part-header selection/retention, JSON and worker/native/RSS
  remain open.

- **M06ck — Content-ID values:** reuse strict identifier syntax and full-value
  validation/replay through a private single-identifier purpose. Bind original
  owners and retain conversion policy, work and provisional-event retirement.
  Qualify literal single/multiple identifiers, malformed tails, owner handoff,
  allowance/deadline cuts and Rust allocation. First-valid selection, retained
  metadata/JSON and worker/native/RSS remain open.

- **M06cl — shared URI syntax:** atomically move the existing URI
  validator to std-only td-header with generic admission. Keep mail's list
  grammar, original owners, exact charges and typed error mapping. Shared
  literal syntax and sticky/prepaid refusal fixtures plus the consumer suites
  qualify the move. Relative Content-Location values, selected metadata and
  worker/native/RSS remain open.

- **M06cm — shared URI references:** add fixed-state relative/empty reference
  spelling and first-segment scheme/colon disambiguation. Preserve default
  scheme-required mail URL behavior and original bounds. Qualify literal RFC
  examples, malformed percent/authority/colon tails and sticky refusal.
  Content-Location field decoding/selection, retained metadata and
  worker/native/RSS remain open.

- **M06cn — shared URI unfolding:** remove wire SP/HTAB and accepted folds
  with one admitted octet or EOF per turn. Preserve literal octets and source
  offsets; reject nonfold endings and incomplete folds. Shared fixtures pin
  exact costs, callback cuts and sticky/fresh refusal. Warm/measured Rust
  allocation intervals cover empty, long, folded word, malformed and refused
  preprocessing. Content-Location CFWS/encoded-word composition, retained
  metadata and worker/native/RSS remain open.

- **M06co — private encoded-word progress:** split fixed transfer/charset
  state from its borrowed Word without changing public behavior or charges.
  The owner supplies the same immutable recognized logical payload each turn;
  shape mismatch is a sticky admitted invariant error. Paired relocated views
  pin event, diagnostic and exact-work correspondence; fixtures cover shape,
  first-work and expired admission refusal. Existing public allocation and
  composition suites retain their bounds. Content-Location fixed scratch,
  placement/CFWS policy, retained metadata and worker/native/RSS remain open.

- **M06cp — selected URI encoded words:** compose shared wire unfolding,
  complete recognition and private payload-free decoding through fixed
  75-octet movable scratch. Relative metadata preserves borrowed word shape
  and language without self references. Unknown/oversized candidates request
  whole literal replay; malformed late folds fail before scalar emission.
  Original job/header owners and consuming fresh handoff govern provisional
  scalars. Whole Content-Location CFWS/placement, URI validation, retained
  metadata and worker/native/RSS remain open.

- **M06cq — selected literal URI references:** validate complete unfolded
  spelling before projecting literal ASCII octets and source-slice offsets.
  Replay retains original job/header owners and fresh consuming handoff.
  Empty references stay valid at this layer; word placement, CFWS, complete
  field grammar and retained metadata remain external. Fixtures pin complete
  malformed-tail rejection, every allowance/deadline cut, provenance and
  exact successful costs. Whole Content-Location composition remains open.

- **M06cr — shared URI spelling selection:** select original slice offsets
  under explicit leading/terminal CFWS permission. Fixed exclusive child
  state funds every probe/replay and EOF; optional grammar recovery never
  hides work/internal failure. POLICY.md freezes ambiguous parentheses
  handling without claiming complete Content-Location validity. Fixtures
  cover partial/failed suffixes, leading errors, every callback cut, fresh
  retirement and long whitespace/comment replay; allocation intervals cover
  both completion and refusal. Original mail-owner composition, whole-field
  presence/word placement, retained metadata and worker/native/RSS remain
  open.

- **M06cs — original-owner URI spelling selection:** compose the shared
  boundary selector with the same Meter/HeaderBudget and fresh consuming
  handoff of owners plus offsets. Fixtures pin exact source ranges, complete
  refusal at every header/job/deadline cut, cached/fresh retirement and
  bounded four-byte-comment turns. Empty/invalid URI spelling remains a
  later phase's decision; word placement, complete Content-Location
  projection and retained metadata remain open.

- **M06ct — literal URI field-value pipeline:** compose CFWS selection and
  complete URI/fold validation before source replay, using exclusive child
  state and fresh transfer of original allowances. Rebase emitted offsets
  into the supplied complete field-value slice. Fixtures pin malformed
  whole-field rejection before output, exact combined costs, owner identity,
  every allowance/deadline cut and fresh final retirement. Literal mode is
  explicitly caller-selected; encoded-word dispatch/placement, retained
  metadata and whole Content-Location activation remain open.

- **M06cu — resident CID/language field selection:** select first-valid
  complete fields through raw scanning and exclusive existing grammars under
  original job/header owners. Skip malformed occurrences without renewed
  credit or partial success; retire all fields after structural/source/work
  refusal. Hide passive absolute extents until complete section success and
  qualify duplicates, source boundaries, original consuming handoff and
  per-turn/full-allowance/deadline cuts. Retained label values, location,
  traversal coordination, JSON and worker/native/RSS remain open.

- **M06cv — passive labels in retained part headers:** add first-valid raw
  CID/language extents to completed Views after an exclusive selector phase
  under original job/header owners. Require the same section/body boundary
  and hide all prior projections until label success. Preserve original
  Scratch and backing; labels introduce no string/list arena. Qualify late
  label refusal, original handoff and allocation/resource/deadline checks.
  Label-value/JSON projection, location dispatch, traversal integration and
  worker/native/RSS qualification remain open.

- **M06cw — selected Content-ID JSON string:** bind the existing complete CID
  projector to shared std-only string framing with original job/header
  owners. Preserve literal identifier spelling and conversion diagnostics;
  serialize into caller output without a retained string. Qualify short
  drains, exact serialization charges, whole-value/refusal retirement and
  original consuming handoff. Language arrays, missing-field property
  mapping, location, part/response composition and worker/native/RSS remain
  open.

- **M06cx — shared bounded string arrays:** extract array punctuation and
  nested string framing into std-only td-json, atomically migrating existing
  MessageIds/URLs property coordinators. Keep complete grammar validation,
  modes, null mapping, original budgets, diagnostics and typed error contexts
  in mail. Qualify short drains, protocol/source/admission refusal, original
  consuming handoff and existing allocation/resource ceilings. Language JSON
  and MIME metadata/response publication remain subsequent work.

- **M06cy — selected Content-Language JSON:** bind original-owner language
  parsing to shared bounded string-array framing. Serialize charged literal
  tag replay with original order/case/duplicates and shared prepaid credit.
  Retire provisional tags on malformed tails and fresh refusal; consume only
  complete drained framing with the original owners. Qualify tiny/empty
  windows, exact costs, every resource/deadline cut, original handoff and
  measured long-list/comment/refusal allocation paths. Selection/null mapping,
  retained metadata/response publication and worker/native/RSS remain open.

- **M06cz — retained selected label JSON:** collect selected CID and language
  JSON directly into separate caller-reserved backing. Keep original owners
  exclusive across child handoff and expose only the healthy complete pair
  through consuming finish. Qualify exact capacities, absence, diagnostics,
  later-fragment retirement, original resource/deadline cuts and allocation.
  Full retained part-header composition, source/selection authorization,
  location dispatch and response publication remain subsequent work.

- **M06da — composed part-header label JSON:** bind complete authorized
  entity part-header selection to retained CID/language JSON in separate
  caller windows. Map selected absolute fields only into the same entity
  within its recognized header section. Retain original job/header/scratch
  owners exclusively, hide all metadata until whole completion and retire
  every View on any refusal. Qualify body isolation, first-valid selection,
  exact windows, source bounds, every resource/deadline cut, original reuse
  and warm/measured allocation. Location placement, traversal and response
  publication remain subsequent work.

- **M06db — selected URI encoded-word runs:** extract passive bounded word
  framing into shared std-only td-header and use its common 75-octet ceiling.
  Validate complete folds and classify every word before scalar replay under
  original owners. Preserve decoded literal spelling and combined diagnostics;
  unknown, oversized or mixed runs request whole-run fallback. Qualify every
  resource/deadline cut, exact word ceilings, late malformed tails and measured
  long-run/refusal allocation. Complete Content-Location placement, retained
  labels and traversal/response publication remain subsequent work.

- **M06dc — complete authorized URI field projection:** compose existing
  source-relative CFWS selection, whole encoded-word runs and literal URI
  fallback under exclusive original owners. Decode only the complete selected
  spelling; preserve decoded labels and whole literal fallback without extra
  output charge. Retire metadata on any refusal and freshly restore original
  owners at healthy completion. Qualify public-child costs, every resource and
  deadline cut, exact grants, source bounds and long/fault allocation paths.
  Field discovery/presence, retained location JSON and traversal/response
  metadata publication remain subsequent work.

- **M06dd — complete authorized location JSON:** bind shared string framing
  to the complete field cursor under original owner and character contracts.
  Charge exact serialized bytes beside scalar output without renewed credit,
  and hide metadata through whole drain and fresh final admission. Qualify
  short/empty output, exact original costs and grants, prefix deadlines,
  typed source/framing faults and long/escape/refusal allocation. First-valid
  location discovery/retention and traversal/response integration remain next.

- **M06de — shared retained JSON windows and optional location retention:**
  extract fixed backing/prefix bookkeeping into std-only td-json and migrate
  paired-label retention atomically. Retain selected location JSON under the
  same original owner/cost contracts, keeping absence and empty presence
  distinct. Qualify exact backing, all original cuts, source/owner identity,
  whole visibility, relocation, fresh retirement and measured long/fault
  allocation. First-valid Content-Location discovery/source-bound retention
  and existing traversal/response metadata integration remain next.

- **M06df — resident first-valid location discovery:** bind the existing raw
  header scanner and authorized complete location field to original owners.
  Skip only malformed needed occurrences, retain first empty references,
  ignore later duplicate values and require the complete raw boundary before
  passive extents/diagnostics become visible. Consuming malformed discard
  freshly admits unchanged owners; nesting, job, interpretation and internal
  refusals stay fatal. Independent scanner/field cost cuts, progress deadlines,
  original source/owner reuse and long duplicate allocation qualify the
  fixed collector. M06dg supplies source-bound retained composition; MIME
  integration remains open.

- **M06dg — source-bound selected location JSON:** bind first-valid resident
  discovery and optional retained JSON to one immutable Input and original
  Meter/HeaderBudget. Replay only that Input's selected extent; expose whole
  selection and borrowed JSON with correlated diagnostics. Qualify public
  child costs, every original resource/capacity cut, progress-prefix
  admission, original-owner reuse and measured zero Rust allocation.
  M06dh supplies retained MIME part metadata; traversal response integration
  remains next.

- **M06dh — retained location in complete part metadata:** atomically extend
  composed header/label backing and View with a separate location window
  and completed source-bound fragment. Keep one immutable Entity and
  exclusive header, label and location owners; correlate complete header
  boundaries and hide all groups on any refusal. Scratch remains parked
  during location. Qualify three-child prefix/cost/reference, all original
  grants and location capacity/presence/repair/nesting cases, triple-owner
  reuse and measured zero Rust allocation within the existing 6 KiB cursor
  and 16 KiB parser region. Traversal response integration remains next.

- **M06di — traversal default context:** retain immediate digest-child
  context independently of selected type in the existing descriptor flags.
  Preserve the combined 64-byte descriptor/body-list reservation and
  original output costs. Qualify completed traversal-to-retained-metadata
  handoff, nested missing/malformed/explicit types, absolute source bounds,
  original owners, nested immediate-child context, after-append problem
  preservation and measured allocation-free context retention. Expose
  parse-problem flags separately from context evidence.
  Automatic traversal/metadata response composition remains next.

- **M06dj — bound traversal/part metadata:** keep one original resident
  source and original job/header owners across complete traversal and
  serial retained part projections. Map only checked completed descriptor
  extents/context, correlate raw body starts, and retire the whole binding
  on child refusal. Keep it Abandoned until healthy consumption, including
  after safe forgetting. Qualify public-child costs and owner reuse,
  every traversal/part prefix, late whole-binding admission, descriptor
  correlation and measured zero Rust allocation. Automatic whole-tree
  body-list/response composition and authenticated locators remain next.

- **M06dk — bound compact classification:** freshly consume original
  part metadata and original ordinal beside its passive body-list Node,
  spending fixed comparisons and complete node output under original
  allowances. Preserve retirement on classification refusal; hand serial
  complete caller nodes and original owners to existing body-list
  selection. Qualify exact costs, all five resource cuts, both
  normal/digest progress prefixes and nested preorder/list output with
  zero measured Rust allocation. Whole-part visit enforcement, response
  serialization and authenticated locators remain next boundaries.

- **M06dl — complete ordered classification:** consume original
  Structure into private ordered construction with admitted fixed Node
  backing. Choose every original ordinal once, consume
  metadata/classification before slot writes and retain original owners.
  Return per-part metadata for one-pass response work, then freshly
  expose only whole complete nodes. Qualify independent
  costs/owners/preorder at three bases, every child/whole prefix, fresh
  complete-owner finish, capacity/extra/forget refusal and measured zero
  Rust allocation. Source-bound body-list and response work remain next.

- **M06dm — original-source body-list selection:** consume private Classified
  into the existing iterative selector under original job/header owners,
  retaining original immutable source/base and complete parts/nodes. Fresh
  constructor admission precedes local limits. Whole selection alone creates
  private Selected; fresh consuming handoff restores original owners.
  Qualify direct-selector turns/costs and literal nested fallback, three source
  bases, original source/cell/budget identity, every progress prefix, late
  complete-owner refusal, three job cuts, all four output capacities and
  measured zero Rust allocation. Response serialization and authenticated
  locator composition remain next.

- **M06dn — original-source response metadata replay:** consume private
  Selected with original source/base/header limit, cells, lists and
  allowances. Freshly admit next-part construction; visit each ordinal once
  through existing bound metadata without reclassification or another node
  charge. Only freshly consumed metadata advances visitation; refusal and
  safe forgetting retire the whole owner. Fresh complete replay produces
  private Projected. Qualify direct metadata costs/turns and literal labels
  at three bases, original owner identity, every child/whole prefix,
  constructor and completed-owner refusal, original work/header cuts,
  extra/forgotten parts, capacity and measured zero Rust allocation.
  JSON structure framing, authenticated locators and publication remain next.

- **M06do — original replay metadata member framing:** consume only a live
  complete replay Part, retaining its abandonment latch until fresh whole
  JSON finish. Frame selected scalars with shared fixed string framing and
  copy privately retained label/location JSON. Charge every wire byte of the
  new fragment under original work/header owners, including metadata copies.
  Apply final charset mapping and unsigned-size bounds; preserve descriptor
  size and decimal preorder IDs. Pin short/empty and wide drains, all UTF-8
  widths, measured per-turn limits, every digest-child prefix and original
  resource cutoff, exclusive constructors and measured zero Rust allocation.
  The fixed fragment omits braces, locators, subparts and property selection;
  body-list/tree composition and whole-job publication remain next.

- **M06dp — fixed original replay fragment retention:** bind private fresh
  framing of a live response Part to a separately reserved fixed window.
  Retain only reported bytes with shared Window; add no second wire charge
  or allowance. Fresh admission precedes capacity, and only complete fresh
  retention consumption advances the parent. Pin exact-fit and every-shorter
  capacities including escaped scalars and long raw runs, bare-framer byte/
  cost parity, caller owner/backing identity, every digest-child progress
  prefix, stale/premature/forgotten refusal, three constructor/exclusivity
  doctests and seven measured zero-allocation trials. Whole body-list/tree
  composition, locators and publication remain subsequent work.

- **M06dq — whole original fragment collection:** consume original selected
  ownership and exactly one distinct fixed cell/window per ordinal. Bind
  private metadata/retention phases to that cell; only fresh complete child
  consumption stores it and clears whole abandonment. Create exclusive
  Serialized after all original ordinals; passive fragments/visitation
  cannot substitute. Pin original backing, selection, preorder and owners,
  every first-child prefix and all five quota cutoffs, capacity, reused/extra
  cells, stale whole completion, seven exclusivity/constructor doctests and
  seven additional measured Rust-allocation intervals. Body-list/tree
  composition, locators and publication remain subsequent work.

- **M06dr — original body metadata member composition:** consume exclusive
  Serialized and emit original bodyStructure or selected text/html/
  attachment members. Use fixed iterative parent ordinals, original
  depth-one root and unchanged retained fragments. Fund every generated
  wire byte and bounded control in original owners. Pin literal tree/list
  output, exact final-byte completion, small output windows, every
  progress prefix, all five quota cutoffs and depth 64; add five exclusive/
  passive-input doctests and eight measured allocation intervals. Complete
  Composed proves emission; fixed retention, authenticated locators,
  request property selection and publication remain subsequent work.

- **M06ds — fixed whole body metadata member retention:** consume original
  Serialized to privately construct fresh composition in one separately
  admitted fixed window. Preserve exact generated byte/cost parity and
  final-byte completion; only fresh complete retention yields exclusive
  Retained. Pin every shorter capacity, original backing/owner identity,
  every prefix, deadline priority and step/record/wire refusal cutoffs and zero-source/I/O debit; add six
  exclusivity/constructor doctests and eight measured allocation trials.
  Authenticated locators, request selection and publication remain later.

- **M06dt — original provisional direct-leaf locator candidates:** consume
  whole Retained and map one original ordinal per funded turn into caller
  slots. Normalize encoded extents to original source base, preserve all
  transfer tags and multipart nulls, and emit canonical p1 candidate bytes.
  Pin literal tags/extents, decoded-size distinction, zero bodies/final
  CRLF, near-u64 coordinates, original owners/backing, every prefix,
  constructor/refusal ordering and exact quota errors. Add five exclusive/
  passive-input doctests and eight measured allocation trials. Candidates
  remain untrusted until exact source identity and live authorized parent
  access bind original Mapped; nested contexts and publication follow.

- **M06du — complete resident source identity and actual parent pin:**
  consume original Mapped and complete PinnedBlob; match ID/length then
  hash all original bytes in funded 4 KiB turns against its authoritative
  digest. Keep the actual pin through fresh Bound completion/release,
  with actual-clock post-work precedence and terminal digest errors.
  Root access authorization remains external before pin opening. Pin
  literal whole-source mismatch, original backing and actual-clock
  refusal; allocation/service qualification, issuance and publication
  remain later.

- **M06dv — original source-binding Rust allocation qualification:**
  compile real filesystem pin and original MIME owners in one test-only
  source graph, preserving private owner construction. Measure eight
  intervals from binding construction through release/refusal teardown;
  filesystem setup, complete pin verification and original mapping stay
  cold. Cover healthy zero/near-u64 bases, partial/completed/Bound expiry,
  same-length source mismatch, original I/O refusal and constructor
  expiry. Only these eight intervals are qualified; API §1.127 lists
  remaining unmeasured source-binding cases. Descriptor teardown is
  counted, enclosing pooled-view/scratch-lease release is outside.
  Existing 65 metadata/locator trials remain separate. Native,
  whole-service resource qualification and authenticated publication
  remain later.

- **M06dw — original source-bound part-member emission:** consume only
  original Bound and select one original ordinal after fresh admission.
  Emit retained metadata plus canonical direct-leaf blobId or multipart
  null in funded 64-byte turns while keeping the actual descriptor.
  Retain exclusive Member completion through fresh final release of
  original Bound. Pin literal bytes/costs, final-byte completion, every
  funded prefix, quota cutoffs, actual-clock post-copy refusal, original
  backing/custody and passive-owner compile failures. Add eight measured
  Rust allocation intervals; whole tree/list composition, current access
  policy, request selection and authenticated publication remain later.

- **M06dx — whole source-bound part-member retention:** consume original
  Bound into a fresh member emitter with a fixed caller window. Advance
  only reported bytes, preserve fresh deadline-before-capacity order,
  sticky refusal and post-work actual-pin fences. Retain whole bytes and
  exclusive Member through fresh original Bound release. Check fragment
  length plus 81 for caller reservation. Pin byte/cost/backing parity,
  exact fit, every shorter capacity and funded prefix, final checks,
  constructor/refusal ordering, quota refusal and compile guards. Add
  eight enumerated Rust allocation intervals with actual-pin-clock
  expiry, leaving the variants listed in API §1.129 unmeasured; whole
  collection and composition, request selection and current
  authorization follow.

- **M06dy — whole original source-bound member collection:** consume
  original Bound and exact fixed cells, hand off one original ordinal
  exclusively into fresh whole retention. Keep abandonment/refusal
  sticky; require fresh complete child acceptance with exactly one
  original step and two records (fresh control plus explicit acceptance),
  and actual pin fence even on quota failure.
  Restore original Bound only after installing its whole member. Mint
  exclusive Serialized only after every ordinal, preserving pin custody
  through fresh release. Pin bytes/backing/costs, cell cardinality/reuse,
  abandonment, premature completion, deadlines and quota precedence.
  Qualify eight enumerated Rust allocation intervals; composition,
  request selection and current authorized publication remain later.

- **M06dz — original source-bound whole tree/list emission:** consume
  original complete Serialized and preserve actual pin custody through
  bounded Structure/Lists emission. Share one checked Frame with the
  existing metadata composer, deleting its old inline mechanism while
  preserving its API/bytes/costs. Fund original output/interpretation
  work and directly sample the actual pin after every turn, including
  quota refusal. Keep exclusive Composed through fresh collection
  release. Pin literal tree/list bytes, original owner/backing custody,
  selected prefix/final expiry, cutoff and post-turn precedence cases.
  Qualify eight enumerated Rust allocation intervals; whole retention,
  request property selection and current publication remain later.

- **M06ea — whole source-bound tree/list fixed retention:** create a
  private fresh composer only from original complete collection, holding
  original descriptor and backing while using a separately admitted
  shared td-json Window. Advance only successful generated bytes without
  extra funding; admit fresh deadlines before capacity, keep partial
  refusal sticky and require fresh complete Retained/final release.
  Pin parity/backing, selected shorter root windows, exact/spare capacity,
  original funding, both deadline domains and final owner boundaries.
  Qualify eight enumerated Rust allocation intervals; complete admission,
  request property selection and authenticated publication follow.

- **M06eb — requested source-bound body-list member selection:** keep
  original complete collection and actual descriptor while selecting
  textBody/htmlBody/attachments/hasAttachment through one shared Frame.
  Preserve existing ALL bytes, turns and funding; frame selected keys in
  canonical order and finish on the final selected copy. Freshly admit
  empty selection, then complete without bytes/work; keep final owner
  admissions fresh. Carry selection through exclusive Composed release.
  Pin all sixteen subsets, original custody, ALL parity/funding,
  deadlines, quotas and final-copy sticky pin expiry. Qualify eight
  enumerated allocation intervals; selected retention, broader request
  routing/bodyProperties and authenticated publication follow.

- **M06ec — whole requested source-bound list retention:** use shared
  fixed windows, preserving original collection, selection and actual
  pin through fresh complete owners. Empty selection accepts zero
  capacity. Pin per-turn five-cost/byte parity, exact-fit/short capacity
  and sticky freshness boundaries; add eight units, six compile-fail
  guards and eight scoped Rust allocation intervals. Wider routing,
  full admission and authorized publication remain later (API §1.134).

- **M06ed — requested source-bound tree/list property routing:** emit
  a pure five-key caller selection through one shared bounded Frame,
  preserving source collection and actual pin through fresh exclusive
  release. Keep single-mode legacy bytes/turns/funding, canonical
  tree-then-list keys, empty/inert completion and sticky fences. Pin
  all 32 SIMPLE subsets plus representative digest/bases, six compile
  guards and eight scoped allocation intervals (API §1.135). Whole
  requested retention, per-part properties, request JSON decoding,
  full admission and authenticated publication follow.

- **M06ee — whole requested source-bound tree/list retention:** retain
  successful selected output from its first byte in the shared Window,
  holding original collection, requested Properties and actual pin
  through fresh complete owners/release. Pin NONE zero capacity,
  final-copy exact fit, meaningful short capacities, byte/five-cost
  parity, both deadline domains and sticky hiding. Qualify eight Rust
  allocation intervals and six compile-fail guards (API §1.136).
  Per-part properties, JSON decoding, full admission and current
  authenticated publication remain later.

- **M06ef — original source-bound part-field selection:** select ten
  existing metadata/locator fields under original Bound and actual pin.
  Preserve ALL bytes/turns/five fees and freshly complete NONE without
  work; fund a fixed bounded metadata index for partial selections.
  Preserve canonical commas, strings/arrays and blob wire/null. Keep
  selection through fresh exclusive Member and original Bound release.
  Pin all 1024 masks in the small generated-member index and exercise
  representative source-bound selections, funding, freshness and original
  custody. Add six compile-fail guards and eight scoped Rust allocation
  intervals (API §1.137). Whole selected retention/collection/tree,
  headers/subParts, request parsing and current publication follow.

- **M06eg — whole selected source-bound part members:** retain from byte
  zero in the shared fixed window. Pure ten-field Properties configure a
  fresh original-bound emitter; advanced and emitted-only owners cannot
  reconstruct the prefix. Exact/spare capacity, meaningful short cuts,
  per-turn bytes/five costs, original descriptor custody, NONE zero
  capacity, both freshness domains, premature finish, sticky work
  refusal and six ownership/constructor guards qualify this increment.
  Eight Rust allocation intervals cover the API §1.138 scope. No
  complete BodyPart/JMAP objects, headers/header:*/subParts, request
  defaults/parsing, whole collection/tree integration, full admission or
  current authenticated publication is claimed.

Implement other header-form composition, encoded words, address/date
parsing, multipart scanning, transfer decoding and part offsets. Implement
documented charset coverage and error/opaque-body representation. Add
deterministic MIME serialization for structured outgoing email, including
attachment streaming, boundary generation through Entropy, reply headers and
Bcc separation. Connect structured-header cursors to the resident NFC
algorithm while retaining its fixed memory and charged replay contract.
Search case mappings come from the same pin. No allocating
library or ambient Unicode version may replace these contracts. Retain
static table accounting within the process allowance.

**Acceptance:** fragmented input, nested multiparts, malformed encodings, huge
headers, cyclic-looking boundary data and unsupported charsets do not panic or
grow working memory. Part downloads match original bytes/decoded content as
specified; forged locators and parent-deletion/reuse races follow STORAGE §3.1. Round-trip fixtures prove From/To/Cc/Bcc and attachment behavior.
Raw-message retention does not depend on rendering success. Do not reuse an
allocating client parser merely because it is already std-only.
CASES.md H01-H06/M01-M10 and UNICODE.md's adversarial replay/failure cases are
required independent oracles, including exact malformed-transfer blob bytes.

## M07 — Shared TLS implementation and mail transport adapters

**Depends on:** M03/M04. **Own:** td-crypto's private TLS/crypto backend and public
opaque APIs; td-mta's transport, policy-generation and resource integration.

**Partial implementation:** M07a1 supplies opaque, fallible streaming SHA-256
inside td-crypto, with terminal failure state. Its direct operation now uses
the owned fixed-state SHA-256 primitive brought forward from F04, without
heap allocation or provider calls. Known-answer, differential and failure tests
run in the portable harness. This leaves the remaining native operations
unchanged. M07a2
compares the real facade with existing mail-format container, cross-file,
blob and import snapshot digest fixtures on the host and portable artifact.
This is hash-coverage qualification, not a production container verifier.
M07a3 adds a worker-local entropy handle with nonempty cold initialization,
fixed returned-error handling and explicit native-abort limits. M07a4 supplies
the concrete Crypto factory, fixed-size provider comparison and opaque P-256
key generation/loading/signing, with bounded PKCS#8 admission and independent
test-only ES256 verification. M07a5 qualifies mandatory client certificates
on local TLS 1.2/1.3 peers, including exact refusal cases and full handshakes
with resumption disabled. Opaque TLS sessions and bounded mail record progress
are now implemented. Gateway authorization, native allocation and service
resource qualification remain pending; these fixtures do not complete M07
or enable service.

The TLS contract is specified in td-crypto/TLS.md, including explicit
algorithm sets, local P-256 PEM identity admission, trust/SNI/time policy,
bounded record progress and failure/close behavior. Material admission and
immutable client/server configurations and socket-free client/server progress
are implemented. M07d2 composes record progress; admitted mail adapters and
complete resource qualification remain pending. Direct Crypto/Entropy
operations are implemented inside td-crypto. Rustls provider/configuration/verifier/key types never leave
that crate. Implement the conformance, explicit-provider confinement, algorithm
baseline, key compatibility and native allocation/failure qualification in
`td-crypto/DESIGN.md`, retaining its independent fixtures for F04.

The mail adapters implement a staged factory and TlsTransport through that
facade. They own implicit-TLS and STARTTLS handoff, socket/deadline
progress, generation/slot leases and gateway allowlist authorization.
td-crypto owns chain/time/name and client-certificate verification. Preserve
SNI, private CA override, bounded certificate generations and verified peer
semantics. Disable unneeded resumption/early data. No authentication
credentials reach a peer before verified TLS.

Implement the remaining work as independently reviewable increments:

- **M07b1 — material syntax:** implemented bounded PEM/base64 decoding into
  caller buffers and strict key/chain envelopes. Reject malformed input, wrong labels, extra
  keys, noncanonical padding and byte/count overflow. Decoded DER is not a
  verified identity. The P-256 PEM loader delegates to existing key validation;
  borrowed certificate readers validate all envelopes before publication.
  Host and portable tests cover independent decoding, exact limits and retry.
  No listener/config publication in this increment.
- **M07b2 — identity admission:** local key/leaf agreement, chain consistency,
  dates, SAN coverage and usage checks; explicit private/public trust handling.
  Use local generated fixtures for wrong keys, missing names, stale clocks,
  expired/misordered/unsupported chains and trust replacement. Keep service
  loading disabled until full generation allocation qualification.
  - **M07b2a — local identity:** implemented owned ServerIdentity admission,
    P-256 key/leaf agreement, bounded certificate metadata, supplied-chain
    signatures/constraints, validity and SAN/usage checks. Includes fixed TLS
    errors and the exact classical certificate-algorithm inventory. Generated
    fixtures cover RSA issuers and malformed/refused material. Inspection
    exposes public certificate/name data only. This grants no remote trust
    and enables no serving path; absent chain-tail issuers remain unchecked.
  - **M07b2b — trust stores:** implemented opaque explicit private CA bundles
    and pinned public-root selection with complete replacement, no skipped
    malformed roots and no implicit OS/network source. Initial private anchors
    refuse EKU/path-length/name constraints as specified in td-crypto/DESIGN.md.
    Generated fixtures qualify replacement, usage, limits and atomic refusal.
    Configuration-role enforcement remains M07b3; no peer is authenticated by
    material construction alone.
- **M07b3 — explicit TLS configuration:** compile opaque shareable handles,
  fixed algorithm inventories, SNI/ALPN selection and supplied-clock bridge.
  Test excluded algorithms, no global provider, mandatory gateway client
  certificates and independent resumption refusal through this layer.
  - **M07b3a — algorithm provider:** implemented private exact suite/group
    lists and complete certificate/handshake identifier mapping checks, with
    excluded native algorithms refused. Portable inventory, drift and local positive/
    negative fixtures qualify this component without enabling service.
  - **M07b3b — local signing bridge:** implemented private retained-identity
    signing through the existing owned key lifecycle. Portable fixtures cover
    canonical DER, hash-once signing, shared retirement and local handshakes.
  - **M07b3c — configuration handles:** implemented immutable role selection,
    bounded identity routing, ALPN, supplied clock and disabled resumption.
    - **M07b3c1 — outbound configuration:** implemented owned ClientConfig,
      fixed HTTP/SMTP protocol selection, bounded verifier and shared injected
      clock with callback-unwind retirement. Portable local peers qualify
      independent client resumption refusal, trust/name/ALPN policy and time
      failure through incoming post-handshake tickets. Sessions remain separate.
    - **M07b3c2 — server configuration:** implemented immutable protocol/name
      selection, bounded shared identities and mandatory private-client
      verification. Portable peers qualify routing, disabled resumption and
      mutual authentication. M07c still enforces raw SNI, selected-material
      lifetime and complete clock/error fences before public session success.
- **M07c — opaque sessions:** implement and pin concrete public signatures
  for TLS.md's record/plaintext/output/status/close operations. Confinement
  tests cover the complete resolved public API. Local peers exercise one-byte
  fragmentation, short buffers, backpressure, truncation, exact authenticated
  evidence and permanent retirement after returned errors/Rust unwinds.
  - **M07c1 — client session:** implemented socket-free client progress with
    whole-record bounds, per-connection sticky clock failure, consuming unwind
    boundaries, fixed errors/evidence and TLS-version-specific close behavior.
    Local peers exercise tiny bounded pipes, simultaneous writes, KeyUpdate,
    post-handshake ticket time failures and exact reassembly limits. Portable
    qualification covers the same cases. This does not enable mail transport.
  - **M07c2 — server session:** implemented bounded raw ClientHello/SNI checks
    before backend loss, including fragmented/retry hellos; selected-identity
    health/date fences before signing and after Finished; mandatory-client
    leaf evidence and shared lifecycle fences through the common facade.
    Public-pair tiny pipes and local mutual peers qualify progress, exact
    evidence and typed refusal. Raw parser state stays below 1 KiB; complete
    generation/session/native accounting remains M07e.
- **M07d — mail transport:** implement existing ports through that facade,
  with socket/lease/deadline handling, implicit client TLS, STARTTLS transition
  fixtures and gateway pin plus address authorization. No provider type or
  diagnostic reaches td-mta. Use td-mail-compatible local HTTPS/SMTP fixtures.
  - **M07d1 — socket/time foundations:** implemented an exclusively owned
    nonblocking TCP adapter with 16 KiB data I/O calls, TCP_NODELAY, fixed sticky
    errors, separate read/write closure and captured socket peer. Implemented
    one runtime monotonic origin and checked injected UTC conversion for TLS.
    Local socket and fault/time fixtures cover their contracts. This adds no
    listeners, dialer, slot admission or TLS/STARTTLS policy integration.
  - **M07d2 — TLS record pump:** implemented shared sessions with two reserved
    record/tail buffers, bounded transport progress and fixed deadlines.
    Public-facade TLS 1.3 fixtures cover partial I/O, simultaneous full chunks,
    flush backpressure, error/close semantics, publication deadline fences and
    TCP loopback. Consuming connection/refusal returns recover both buffers
    for pool reuse, including constructor failure before admission.
    Sealed borrowed/owned array storage supports complete connection movement
    between workers; owned buffers are allocated before admission and returned
    explicitly. Runtime leases and pool return queues remain M07d3.
    The portable harness executes all eleven pump and five clock/TCP cases
    from a separately selected static musl library test artifact.
    No mail authorization is constructed from raw TLS evidence.
    Gateway process fixtures below cover TLS 1.2/1.3 through mail transport;
    complete protocol and resource qualification remain below.
  - **M07d3 — admitted upgrades:** bind immutable policy generations and slot
    leases, reserve before STARTTLS replies, reject plaintext tails and map
    verified leaf evidence plus actual peer address to gateway authorization.
    Complete local HTTPS/SMTP transition fixtures before enabling service.
    - **M07d3a — handshake capacity:** implemented a fixed atomic bitmap and
      linear movable permits over the validated global count. One reservation
      attempt never waits/spins; Drop returns capacity. Host and portable cases
      cover all capacities, saturation/reuse, overlapping reservation/release
      and refusal cleanup.
      This reserves only the count; it does not couple session buffers, policy
      generations or protocol transitions and does not enable service.
    - **M07d3b — policy and resource binding:**
      - **M07d3b1 — immutable gateway policy:** implemented cold private-CA
        admission, mandatory client-auth server configuration and owned
        pin/CIDR constraints. Canonical fingerprints compare configured client
        authority independently of PEM/order/spelling and server renewal.
        Matching supplied values is a predicate, never an admission proof.
        Host and portable fixtures cover equivalence/change, maximum bounds,
        pin/address refusal and required client authentication. Generation
        publication and successful authenticated transport remain below.
      - **M07d3b2 — generation and resource coupling:**
        - **M07d3b2a — retained ownership:** implemented two-slot generation
          ownership with detached reservation before cold boxed construction,
          process-unique IDs, stale/foreign publication refusal and movable
          shared leases. Explicit startup has no Default; publication returns
          a must-use retired owner for control-worker disposal. Current,
          reserved, candidate and retired payloads share capacity; final drop
          destroys the boxed payload before releasing its reservation. Host
          and portable cases exercise limits, failure, concurrency and drop
          order. This generic owner does not validate TLS policy or bytes.
        - **M07d3b2b — admitted factory:** couple the global handshake permit,
          reserved wire/session storage and immutable authorized generation.
          Refusal returns all owners; hold the permit across queued/Pending
          progress and release only on completion or teardown. Apply current
          policy revocation using canonical comparison plus listener binding.
          - **Policy compilation and preparation:** implemented complete
            TLS content compilation from a closed, text-resolved configuration
            inside a reserved generation. Bounded readers admit every local
            identity and explicit trust input; immutable tables select listener,
            relay and ACME roles. IDs resolve only within their generation.
            Detached session preparation retains the global handshake permit,
            generation and caller-owned wire buffers before worker-native
            construction; refusal/teardown recover both buffers. Exact gateway
            comparison covers canonical policy and the full listener binding.
            This is not current-generation authority or a mutation fence.
            **Client startup/recovery:** implemented explicit prepare_clients
            for relay and optional ACME trust without opening server/gateway
            material. A coverage tag distinguishes it from complete compilation;
            no server policy can be resolved. Both modes share one two-slot
            generation domain. Local fixtures cover absent/expired identities,
            strict client trust refusal and an old outbound session completing
            across publication of a complete table. M18/M19 must still integrate
            issuance, health gates, expiry and atomic runtime publication before
            service; constructors alone never issue certificates.
          - **Socket handoff:** implemented consuming TCP handoff with fixed
            deadlines, plaintext-tail refusal and exact buffer recovery. The
            bounded pump retains generation/count owners, publishes role-checked
            mail evidence only after handshake completion, and releases count
            capacity on completion/abort. Gateway evidence additionally matches
            the actual socket peer and configured leaf pins. A supplied-current
            comparison aborts on canonical policy/binding changes; runtime
            serialization and durable mutation fencing remain M11/M13. Host and
            portable cases cover deadlines, refusal, encrypted delivery, close,
            retention, clock failure and missing-client-certificate denial.
            Private process-peer fixtures now qualify positive gateway mTLS
            under TLS 1.2/1.3 with current/next pins, wrong verified leaf pins
            and actual peer CIDR refusal. Exact STARTTLS transitions remain
            M07d3c.
    - **M07d3c — protocol integration:** complete STARTTLS flush/tail handling,
      parser/EHLO/auth reset and local HTTPS/SMTP transitions. Gateway mTLS
      transport evidence is covered by the process fixtures above.
      - **Control framing:** implemented borrowed 512-byte strict-CRLF line
        framing and a 16 KiB aggregate multiline reply reader. Exact consumed
        prefixes preserve tails; reply codes agree across continuation lines,
        bare final codes are accepted, and framing/capacity failure is terminal.
        EHLO extension syntax excludes the initial greeting. Boundary, malformed,
        fragmented, aggregate-cap and tail fixtures run on host and musl.
        The parser does not own sockets, deadlines or command state. STARTTLS
        inbound reply ownership is described next; complete reset/dispatch and
        outbound protocol drivers remain pending.
      - **Inbound reply ownership:** implemented ServerStartTls consuming the
        prepared session and socket before 220. Validate the framed bare command,
        tail, receiving role and deadlines. One bounded operation per advance
        writes/drains the complete reply before TLS handoff; refusal/cancellation
        recover both wire reservations and release native/generation/count owners.
        Direct local and private gateway process fixtures qualify the boundary,
        including TLS 1.2/1.3 pin acceptance/refusal. Full command sequencing,
        SMTP state reset and actual-current fencing remain protocol integration
        work.
      - **Outbound reply ownership:** implemented EhloReader/StartTlsOffer and
        ClientStartTls for required-STARTTLS relay policies. Consume the complete
        advertisement and prepared session, write/flush STARTTLS, then require
        a complete bounded 220 without buffered tail before native handoff.
        Two borrowed 512-byte reservations retain fragmented input and reply
        state; refusal/cancellation return native wire buffers and permits.
        Local fixtures cover both owners through verified TLS, malformed/tailed
        replies and admission cleanup. Full greeting/EHLO/AUTH dispatch and
        post-TLS state reset remain M17; M07e owns whole-session accounting.
- **M07e — resource/service admission:** qualify complete generation overlap,
  session/handshake peaks, worker entropy and Rust/native stack/allocation/RSS
  on the portable artifact before activating the adapters. Amend the checked
  ledger if measurements cannot fit; never infer a bound from buffer limits.
  Account for retained peer state, decoded-message expansion and separate
  header/body polling calls before service admission.
  - **M07e1 — representative worker stack:** a portable-only release fixture
    runs twenty-five policy/transport scenarios on one guarded non-growing mapping
    capped at the existing 256 KiB worker allowance. It covers representative
    construction/refusal, retained generations, TLS/STARTTLS and gateway paths.
    RESOURCES.md scopes the evidence; complete allocation/native/RSS accounting,
    maximum inputs, CPU paths and production caller stacks remain pending.
  - **M07e2a — Rust allocation probe foundation:** a separate test executable
    forwards System allocation through the approved UNSAFE.md T1 boundary.
    Checked counters and positive controls qualify the probe before measuring
    repeated digest and SMTP success/refusal paths. Native heap accounting,
    TLS generation/session measurements and whole-service RSS remain pending.
  - **M07e2b — bounded native bookkeeping:** a test-only fixed address/size
    table tracks ownership and coherent requested-byte snapshots without heap
    storage or pointer access. Collision, saturation, resize ownership and
    concurrent tests exercise the table; snapshots are coherent through its
    shared guard. Deletion closes probe-chain gaps without accumulating
    tombstones; model traces cover wrapped and full tables. Libc interception
    remains pending.
  - **M07e2c — native allocator diagnostic foundation:** a separate musl
    executable wraps six libc entry points only at its final link. Positive
    controls, recursion suppression and symbol confinement precede any TLS
    memory interpretation. Counts remain diagnostic, not whole-native/RSS
    qualification; generation/session limits and admission remain pending.

  - **M07e3a — outbound lifecycle observations:** separate Rust and native
    processes record representative public-root relay/ACME generations,
    overlapping owners, two pending clients and repeated buffer reuse.
    Reservation and capacity refusal require unchanged counters. Requested
    live bytes and lifetime peaks remain diagnostics; server/maximal inputs,
    completed handshakes, record traffic and RSS qualification remain pending.

  - **M07e3b — local handshake/record observations:** separate Rust and native
    processes share synthetic certificates with existing transport fixtures,
    complete an admitted local TLS 1.3 pair and verify repeated 16 KiB traffic
    both ways. Warm retention is checked while allocation calls remain visible.
    Other versions, mTLS, adversarial/maximal inputs and RSS remain pending.

  - **M07e3c — entropy worker lifetime observations:** separate counter
    processes observe eight test workers before RNG use, after warming one
    then all workers, after repeated fills, and after explicit joins and
    scope teardown. Warm fills require unchanged counters. Shared provider
    retention, stack mappings and whole-process RSS remain separate; this
    diagnostic does not activate production workers or amend the ledger.

  - **M07e3d — fragmented handshake refusal observations:** both counter
    domains exercise the existing 16 KiB/4 KiB fragment reassembly boundaries,
    distinguish decoding refusal from one-byte-over-limit capacity refusal,
    and check retention across repeated construction/refusal. Valid maximal
    chains, complete concurrent sessions and RSS remain unqualified.

  - **M07e3e — large admitted local chain observations:** reuse the local
    handshake/record scenario with a signed three-certificate chain above
    60 KiB of PEM and within the 64 KiB loader ceiling. Both counter domains
    observe completed authentication and repeated records; maximal profile
    counts, remote chains, other TLS versions and whole-service bounds remain.

  - **M07e3f — sampled process RSS observations:** a separate unwrapped test
    executable runs the five existing scenarios, each in a fresh process.
    A fixed rollup reader and positive resident-allocation control qualify
    ordered KiB samples. They include fixture/observer overhead and do not
    establish transient peaks, whole-service bounds or per-session charges.

  - **M07e4a — established output allowance:** the shared facade permanently
    retires its larger handshake ciphertext allowance only after Finished and
    complete local flight drain. Both roles then cap pending ciphertext at
    two wire reservations, preserving read/write progress and terminal
    capacity refusal. This narrows one queue bound; retained and temporary
    memory, complete generations and total session admission remain pending.

  - **M07e4b — endpoint and wire-buffer release observations:** split the local
    and large-chain teardown checkpoints into client release, remaining-server
    release and one checkpoint dropping all four returned buffers. Both counter
    domains require the exact requested wire bytes to disappear; native
    tracking also checks four blocks. RSS samples the same checkpoints without
    asserting allocator release. Shared state, fixed objects and transient peaks
    remain separate.

  - **M07e4c — sixteen-profile generation observations:** compile sixteen large
    files identities into SMTP and HTTPS/MTA-STS policies with explicit relay
    trust. Separate candidate/publication/old/current release checkpoints,
    unchanged third-generation refusal counters and repeated replacement
    retention checks run in both counter domains and unwrapped RSS. Maximum
    trust/name/listener combinations and aggregate admission remain pending.

  - **M07e4d — large remote-chain observations:** an unwrapped controller owns
    synthetic material and a private test peer. Separate Rust/native/RSS
    processes authenticate TLS 1.2/1.3 chains above 63 KiB DER and verify repeated
    records. Endpoint and wire-buffer release remain separate. The peer enables
    TLS 1.3 tickets; complete transient, concurrency and session admission work
    remains pending.

  - **M07e4e — large incoming-ticket observations:** the remote-chain fixture
    additionally processes one 65000-byte TLS 1.3 ticket in each observation
    domain. Separate socket-free cases qualify accepted single/paired tickets
    and terminal capacity refusal of a larger combined flight after Finished.
    These fixed cases leave complete simultaneous-traffic and service memory
    admission pending; resumption remains disabled.

  - **M07e4f — large generation routing observations:** combine sixteen
    certificate profiles, sixteen listeners and 256 MTA-STS domains with long
    derived names. Separate Rust/native/RSS processes observe the same
    two-generation and repeated-replacement lifecycle as the smaller case.
    Sixteen-HTTPS, mixed trust/gateway inputs and complete session coexistence
    remain pending; the fixture alone does not prove the generation allowance.

  - **M07e4g — shared HTTPS identity views:** the cold compiler shares narrowed
    certificate-name lists by profile and JMAP-primary role across listeners in
    one generation. Listener bindings and native configurations stay distinct.
    Real TLS fixtures cover equal/different primary selections and SMTP-name
    exclusion. Both allocation probes check the large-routing candidate's
    retained requested bytes against 1 MiB; full generation/session admission
    and allocator/RSS bounds remain pending.

  - **M07e4h — gateway trust generation observations:** a separate scenario
    constructs sixteen large profiles, fifteen gateway policies with full
    128-anchor private bundles, one HTTPS listener and relay trust. Fresh
    Rust/native/RSS processes cover the two-generation lifecycle, unchanged
    third-slot refusal and stable replacement. Its explicit fifteen-session,
    128 MiB configuration is fixture-only. Retained trust duplication still
    requires accounting or reduction before service activation.

  - **M07e4i — decoded certificate-list refusal observations:** fresh
    Rust/native/RSS processes feed unexpected TLS 1.2 certificate lists with
    21800 empty entries through the client facade. Two fragmentation sizes,
    terminal refusal and stable repeated teardown qualify decoded allocation
    separately from bounded wire storage. Encrypted TLS 1.3 and concurrent
    session accounting remain pending.

  - **M07e4j — revised TLS planning ledger:** retain default connection counts
    with a 96 MiB planning budget. Reserve 512 KiB per TLS session, 4 MiB per
    admitted handshake, 8 MiB per certificate generation and one 4 MiB
    established-processing allowance for the main thread. Generation,
    remote-client and decoded-list allocation fixtures check their measured
    requested-byte costs against these entries; keep the narrower routing
    regression guard. The allowances do not prove
    arbitrary-input, simultaneous-traffic, allocator or RSS bounds, and do not
    activate serving.

**Acceptance:** shared backend tests exercise known-answer/independent crypto
oracles, malformed keys, explicit TLS policy and upstream API confinement.
Mail integration fixtures cover valid/untrusted/expired/wrong-name chains,
mTLS admission, fragmented records, STARTTLS reset, handshake saturation,
wrong keys and returned/fatal failure boundaries. Test M02 digest-bearing
fixture inputs through the real provider; fake output is not a digest oracle.
Fit cold/session/handshake generations, native allocations and Rust/C peak
stack within the service ledger and measure whole-process RSS. Warm workers
before admission and report secret-erasure limits. Any allocator hook first
requires an UNSAFE.md amendment. No live CA/provider contact, ignored
certificate errors, hidden second backend or unreviewed direct mail
dependency.

## M08 — Store objects, indexes, checkpoints and reclamation

**Depends on:** M05/M06. **Own:** object transactions, read views, index cache,
checkpoint publication, change history and garbage collection.

Implement mailbox hierarchy/membership/keywords, immutable email objects,
thread assignments and authoritative anchors, account state and retained changes. Implement STORAGE.md's sorted flat tables,
fixed journal arenas/descriptors, exact-prefix read views and streaming merge.
Pause new mutations during checkpointing; publish table/manifest/journal pairs
through CURRENT in the specified order. Sparse/secondary disk indexes remain
rebuildable. Bound retired-generation pins and enforce history floors, disk
reservations and checkpoint overlap budgets. Implement body reclamation only
inside the specified exclusive maintenance window, including queue/lease roots.
Enforce ADMISSION.md's corrected checkpoint record-overhead reserve, separate
closed-journal/history charges, background-view arbitration, startup orphan
cleanup and quota-derived maintenance work bounds. Prove a complete GC pass
at configured caps across bounded candidate windows and a validated scheduling
cursor; a scan that always restarts without progress is not reclamation support.

**Acceptance:** remove indexes, rebuild and compare object IDs, bytes, folder
membership and states. Readers never observe half a transaction or an index
ahead of commit. Kill at every checkpoint/reclaim boundary. Old state tokens
produce explicit resync errors; restored epochs cannot alias previous states.
Scale many small objects without mailbox-sized RAM. Exercise both journal
byte/operation ceilings, read views spanning commit/checkpoint, pin exhaustion,
queue references after visible email deletion, and orphan cleanup after a failed
publication. Inspect identical logical views before and after checkpointing.
Exercise repeated operations on one key in a frame, reserved streaming work
finishing across checkpoint, physical orphan scans and abandoned sort cleanup.
Do not implement JMAP here.

## M09 — Bounded DNS and outbound HTTPS transport

**Depends on:** M04/M07 for HTTPS; DNS core can start after M04. **Own:** resolver
and bounded outbound HTTP transport shared by ACME and migration.

Implement A/AAAA/CNAME resolution through configured resolvers, bounded DNS
compression/name parsing, cache TTLs, query entropy, UDP truncation/TCP fallback,
timeouts and response-source/question validation. Read only documented resolver
configuration: SCHEMA.md's explicit numeric resolver endpoints, not ambient
resolv.conf/NSS modules or uncancelable per-request threads. ACME operational
URLs remain on the directory origin; offline migration has one bounded explicit
source-origin endpoint slot.
Implement HTTPS requests/streaming responses with capped headers, framing,
redirect policy, fixed trust settings, and total deadlines.

**Acceptance:** local DNS/HTTPS fixtures cover compressed-name loops, malformed
lengths, unrelated replies, TCP fallback, chain limits, expiration, slow peers,
HTTP ambiguity and connection cleanup. Provider/domain failures are typed and
bounded. No MX resolution or SPF/TXT evaluator in v1. Resolver endpoints are
explicit runtime data, never permission to use public DNS during automated tests.

## M10 — SMTP core with durable local delivery

**Depends on:** M04/M06/M08. **Own:** SMTP parser/session state and store adapter.

Implement the DESIGN section 9 command/extension set as a pure state machine
fed bounded chunks and a transport-independent peer context. Resolve aliases,
validate recipients early, stream DATA, generate trace metadata, commit delivery
and return final acceptance only after store success. Deduplicate multiple
aliases to the same account within one SMTP transaction.

**Acceptance:** transcript fixtures split at every framing boundary; exercise
invalid sequencing, RSET, null sender, 8-bit content, unknown/nonlocal recipients, case-sensitive ordinary aliases, domainless and mixed-case
Postmaster, non-enumerating VRFY, oversize mail, dot-stuffing, bare-LF smuggling and resource exhaustion. The
acceptance transcript's 250 maps to recovered durable mail. No general relay,
public AUTH, DSN advertisement, or port binding in this task.

## M11 — Direct receiving runtime and admission limits

**Depends on:** M07/M09/M10. **Own:** runtime listeners, fixed workers/slots,
timeouts, startup/shutdown state and SMTP TLS wiring.

Bind configured test/high ports first, attach the M10 engine to TLS/plain
transports and impose peer/global fairness limits. Reset state after STARTTLS,
bound pending handshakes and drain/close saturated connections correctly.
Allocate/touch application buffers before admission. Add observable readiness
and worker-failure handling without yet claiming complete operational tooling.

**Acceptance:** real local SMTP clients deliver and retrieve persisted raw mail;
TLS failure never resumes plaintext in the same session. Slow peers cannot
exhaust all future slots indefinitely; overload gets a temporary response when
possible. Kill/restart preserves accepted delivery. No root execution required
for the tests and no production port configuration changes.

## M12 — Gateway receiving policy

**Depends on:** M11. **Own:** gateway configuration and peer admission adapter.

Add a distinct listener policy with explicit address allowlists and verified
client certificate identities from a gateway-specific private trust root; map
admitted peers to fixed gateway IDs. Public outbound roots cannot authorize peers.
Enforce identical local-recipient rules after peer admission. Record gateway
identity separately from untrusted headers, and expose the expected upstream
recipient-policy requirements in configuration output.

**Acceptance:** unlisted peers, forged identities, bad certificates, plaintext
network clients and nonlocal recipients fail. Accepted gateway delivery uses
the same durable path as direct SMTP. Direct mode still accepts eligible
plaintext peers. No PROXY protocol, XCLIENT, trusted Authentication-Results,
upstream forwarding daemon or original-IP reconstruction.
Include a gateway-only listener with a distinct server hostname and verify
the upstream peer's hostname/chain checks against its provisioned certificate.

## M13 — HTTPS, authentication and JMAP Core

Use ADMISSION.md's exact bounded response spool, creation-ID map, result
reference evaluation and per-method capacity preflight. Required tests include
query/mutation/reference ordering and failure after an earlier committed method.

**Depends on:** M02/M04/M07/M09. **Own:** HTTP server framing, bounded JSON token
parser/writer, auth/device verification, discovery and core dispatcher.

Implement strict HTTP framing, streaming upload/download plumbing, request
tokens in reusable arenas, method ordering/errors, creation/result references,
account authorization and limits. Implement generated app-password verifiers
and revocation checks with fixed auth-work limits. Discovery publishes fixed
configured URLs and only completed capabilities. Include required JMAP Core
session/push behavior according to the M01 table; do not omit mandatory fields
by calling the implementation a client-specific subset.

**Acceptance:** HTTP smuggling variants, invalid JSON, excessive depth/tokens,
cross-account IDs/blobs, bad auth and revoked credentials fail predictably.
Streaming data cannot bypass upload limits. Tests prove auth is HTTPS-only,
device verifiers contain no reusable plaintext secret, and error output leaks
no credentials. Unimplemented mail capabilities remain absent until M14/M15.
Two event streams must not pin storage views or starve ordinary method workers;
read-slot saturation has the bounded retry/error behavior frozen in M02.

## M14 — JMAP mail reads, changes, and queries

**Depends on:** M06/M08/M13. **Own:** mailbox/email/thread read methods, query
evaluation, change states, property projection and search snippets as required.

Implement the M01 required method/property matrix using bounded read views.
Cover td-mail's actual query filters and raw/custom header/body requests, stable
pagination/date ordering, decoded text search, mailbox counts, threads, blob
downloads and state changes. Build additional disk indexes only when measured
search work needs them; maintain bounded fallback with explicit timeout errors.
Implement POLICY.md's exact search scope, supported sort/collation choices,
query-state interpretation version and CASES.md's property-level oracles.
Add td-mail's bounded metadata fallback/per-ID isolation for Email/get
interpretation failures; one opaque message must not hide an entire listing.

**Acceptance:** compare results to independently specified corpus expectations,
not a test that calls the same filter implementation. Query total, order,
membership, thread and state remain coherent across concurrent writes. Real
td-mail reads can begin against a seeded local store. No invented empty
responses to required unsupported methods; finish or withhold the capability.

## M15 — JMAP mail writes and structured message creation

**Depends on:** M14. **Own:** Mailbox/set, Email/set/import/copy and associated
required write semantics, upload lifetime and attachment linkage.

Implement mailbox create/rename/move/delete, keyword and membership changes,
email destruction, structured MIME construction, upload attachment references,
state preconditions and per-object errors. Email/copy has only the standard
single-account refusal outcomes frozen in POLICY.md; do not invent a successful
same-account copy. Protect immutable submitted blobs
even when email objects are changed/deleted. Read-only identity config supplies
Identity/get and required refusal semantics; local management edits identities.

**Acceptance:** actual td-mail operations manage a real store; restart preserves
results. A multi-method request preserves earlier successes on later failure.
Concurrent/expired uploads, quota exhaustion, invalid parent cycles and foreign
blob references fail without partial success claims. Canonical MIME fixtures
cover Bcc, Unicode and attachments. Do not initiate remote delivery here.

## M16 — Durable JMAP submission state machine

**Depends on:** M08/M15. **Own:** immutable submission intent, journal transitions,
recipient states, JMAP submission methods, success filing and reconciliation.

Implement DESIGN section 11 with a fake deterministic transport boundary first.
Persist submission creation before success, process #creation references and
onSuccessUpdateEmail, and support query/get/changes/state checks from M01.
Map internal states exactly as frozen in M02. Serialize cancellation/retry
against attempts. Preserve unresolved/final record retention and failure
notification intent. No network smart-host worker in this increment.

**Acceptance:** lose the HTTP response after a committed create, reconnect and
find the submission using td-mail's identity/time query while the relay remains
unavailable; sendAt stays at creation time across retry/restart. No duplicate is caused
by the service replaying a transaction. Sent filing is reported separately and
never claims relay acceptance. Kill between intent, attempt and status writes;
recover uncertainty honestly. Attempt cancellation races have one valid result.

## M17 — Smart-host SMTP client and queue worker

**Depends on:** M07/M09/M16. **Own:** SMTP client engine, due-index scheduler and
local scripted peer fixture; wire the queue into the runtime.

Implement verified implicit TLS/required STARTTLS, bounded EHLO/AUTH PLAIN/LOGIN,
capability-based transfer, envelope/DATA dot-stuffing and response parsing.
Implement partial-recipient handling, retry/backoff/expiry, route pause,
uncertain attempts and local failure notifications. Include a default fixture
profile matching Migadu's documented port-465 behavior, with test credentials.
DNS timeout/SERVFAIL/NXDOMAIN back off the route without immediately failing
recipients; normal queue expiry and minimum retry intervals still apply.

**Acceptance:** run every DESIGN section 15 SMTP fault case; verify exact
captured bytes and per-recipient attempt counts. Successful RCPT followed by
failed DATA remains pending/failed as appropriate, never delivered. Never
retransmit recipients whose final DATA acceptance is durably known. Resume
pending delivery after process restart and provider recovery. Configuring
smtp.migadu.com in the isolated
fixture is explicitly refused before a connection. Fixture certificates still
require correct hostname validation; mapping to loopback cannot skip it.

## M18 — ACME issuance, renewal and MTA-STS publication

**Depends on:** M07/M09/M13. **Own:** ACME state machine, bounded JWS/CSR encoders,
certificate generations, HTTP-01 responder and static MTA-STS host routing.

Implement directory/account/order/authz/challenge/finalization/certificate
flows through existing adapters. Persist keys/orders, validate returned names
and keys, schedule renewal by actual lifetime, and atomically install a checked
generation. Serve only exact live challenge tokens; bound token expiry and
hostnames. Generate DNS/policy output without editing external DNS.

**Acceptance:** use a local ACME server fixture with real signature/challenge
verification and a local CA key. Cover initial issuance, badNonce, retry hints,
rate limits, invalid SAN/key/chain, restart, renewal overlap and expiration.
Prove old sessions survive a valid rotation and new sessions see the new cert.
Cover a distinct ACME-managed gateway hostname and provisioned private names.
No public CA accounts created and no actual deployment DNS/ports changed.

## M19 — Administration, logs, health and reload

**Depends on:** M04/M08/M12/M17/M18. **Own:** CLI/control socket, rolling-file
sink, runtime health/status aggregation and config generation lifecycle.

Wire the runtime administration subset of DESIGN section 6 through a private
control socket or exclusive offline lock: config check/show, serve, status,
doctor, dns-plan, reload, queue/device operations, and store layout/inspect/
journal/export. Implement versioned JSON output, pagination, queue
inspection/operations, storage layout/record/journal inspection and raw export,
device creation/revocation, doctor, redacted config and
atomic reload. Add size-based log rotation, suppression counters and fallback
diagnostics. Ensure a restart-required change does not partly apply.
M20 owns verify/repair/backup/restore; M21 owns migrate. Those commands remain
unavailable until their owning increment lands; M19 supplies shared CLI wiring.

**Acceptance:** unauthorized socket access fails by filesystem policy; stale
IDs and concurrent controls cannot duplicate delivery. Health reports disk,
renewal, queue and dropped-log failures even with a broken log sink. Live
credential revocation affects pending mutations. Peer floods cannot grow logs
or memory without bound. No public administrative API or shell hooks.

## M20 — Offline verification, repair, backup and restore

**Depends on:** M08/M19. **Own:** storage inspection/repair tools and consistent
backup manifests; reuse the store's codecs and locking rather than bypassing it.

Implement read-only verify, explicit repair planning/apply while stopped,
generation/prefix-pinned account backup and restore to a new root, following
STORAGE.md. Snapshot service-wide credentials/configuration only while stopped
until a separate consistency protocol exists. Preserve a pre-repair copy
or manifest sufficient to audit changes. Rebuild indexes independently. Change
store epoch on restore and refuse incompatible formats without altering them.

**Acceptance:** backup while receiving and compacting, restore elsewhere, then
verify every committed blob/reference/folder/queue record. Detect interior
corruption and missing blobs. Repair never invents accepted contents or drops
corrupt committed records silently. Live and offline writers cannot race.
Exported secrets require explicit selection and protected destination files.

## M21 — Stalwart 0.15.2 export/import and cutover runbook

**Depends on:** M09/M14/M20. **Own:** migration CLI, archive schema and operator
runbook; use the shared store transaction API and bounded HTTPS transport.

Paginate JMAP folders/emails, download raw blobs, build a digest/count manifest,
and import under stable source-object mappings. Preserve hierarchy, memberships,
dates and available ordinary keywords; do not migrate server settings or rules.
Implement repeat/resume and final reconciliation, including source deletions
and moves during a provisional copy. Report every unrepresentable object.

**Acceptance:** local version-identified Stalwart 0.15.2 or sanitized real
exchange fixtures prove compatibility. Repeat interrupted export/import without
duplicating identical messages with different source IDs or losing shared
membership. Verify a 1 GiB archive. Test the quiescent final pass and rollback
with post-cutover new mail. No test connects to the user's production Stalwart.
Do not label a generic server mock a verified 0.15.2 migration test.

## M22 — Real td-mail end-to-end acceptance suite

**Depends on:** M12/M15/M17/M18/M19. **Own:** integration harness and minimal
existing-client test seams only if needed; do not reimplement client sending.

Build/run real td-mail, td-fetch and td-mta binaries in isolated temporary
roots with test credentials/certificates and local endpoints. Exercise existing
NDJSON commands including `send_draft`, and assert actual SMTP fixture capture
and persisted mailbox state. Audit current td-mail calls again when starting
this task; earlier plans inspected commit c2c343bf3's sending implementation.

**Acceptance:** receive/read/search/move/flag/delete/thread/download and
compose/send with attachments/Bcc work end to end. Drop the JMAP response after
submission commit and prove the client's reconciliation avoids blind resend.
Simulate relay outage/recovery, restart, auth revocation and expired certs.
Failure messages/status are observable. No MockJmapServer or MockFetchSocket
stands in for a production binary in this suite. Public egress is denied.

## M23 — Memory, allocation, fairness and disk-pressure gates

**Depends on:** M20/M21/M22. **Own:** resource measurement harness, deterministic
corpus generators, allocator evidence and limit/refusal regression cases.

Run the DESIGN section 15 workload and many-small-message variant on release
builds. Measure RSS separately from page cache/cgroup accounting, record the
whole pool ledger, and detect allocations in successful AND failed admitted
hot operations. Name bounded std/TLS/cold-path allocations explicitly. Any
unsafe instrumentation needs the repository's documented test surface.

**Acceptance:** <64 MiB idle, <128 MiB workload RSS under the default profile,
or a reviewed design amendment supported by measurements before release.
Repeated ingestion, queries, retries, renewals and reloads do not grow retained
RAM. Saturation yields defined refusals and reserves progress for health and
existing accepted work. Disk full and checkpoint pressure cannot corrupt mail.
Fix concrete hot-path violations in their owning modules, with focused tests.

## M24 — Crash matrix and protocol robustness release gate

**Depends on:** M23. **Own:** consolidated crash/fault corpus and conformance
report; production fixes remain within their owner modules.

Execute the frozen transaction crash matrix with real binaries, deterministic
I/O failures, and disposable filesystem/VM power-loss tests. Cover accepted
incoming mail, JMAP writes/submission, partial recipients, renewal, snapshots,
backup/import and repair. Run malformed-input campaigns over every parser and
verify mandatory JMAP coverage against the M01 table, including event/state
behavior required by the chosen standards baseline.

**Acceptance:** every externally acknowledged mutation is recovered; every
unknown remote delivery remains explicitly uncertain; no test silently skips a
missing capability/tool. Record tested filesystem/kernel/toolchain versions.
The gate includes forbidden-relay, cross-account, Bcc and credential-redaction
oracles. Resolve actual blockers before calling this release-ready.

## M25 — Portable deployment artifact and operator documentation

**Depends on:** M24. **Own:** reproducible musl build/release instructions,
artifact verification, clean-host smoke test and supervisor examples.

Package the one executable and its debug companion as required by the applicable
profiling policy. Document state/config paths, low-port binding without a root
protocol worker, service identity, startup/renewal/restart behavior and limits.
Provide systemd and td-svc examples only after reading their normative contracts;
the executable cannot require either supervisor. Include direct/gateway DNS,
Migadu identity/auth configuration, local test instructions and the migration
runbook. td image recipe/integration is a separate future change.

**Acceptance:** the exact installed executable passes ELF static-link checks
and runs on a clean non-td Linux fixture with no td helper installed. It loads
provided cert/config/data files and uses no shared libssl. Copy state between
fixtures and recover it. Signals/restarts and permission refusals are tested.
Report source/bootstrap provenance accurately. Document x86-64 support and the
remaining aarch64 validation work without introducing architecture assumptions.

## Later increments

These do not block v1 and must not be advertised before their own acceptance.

**F01 — SPF.** Add bounded DNS TXT/MX evaluation with RFC lookup/recursion/work
limits, cache and timeout policy, and a complete pass/fail/softfail/neutral/
none/temperror/permerror result model. Direct mode evaluates the real peer;
gateway mode needs the explicit trusted-origin contract first. Authentication
results must distinguish checks from disposition. Local DNS fixtures only.

**F02 — DKIM and DMARC.** Extend the crypto port with RSA-SHA256 and Ed25519-SHA256
verification; use streaming body/header canonicalization, bounded signature/DNS
processing and exact standards fixtures.
Define alignment and organizational-domain rules using a reviewed data strategy
and applicable standard versions; do not casually add a dependency or hostname
heuristic. Start with observed results before policy enforcement. Forwarded
mail and malformed signatures must not become implicit success. Outbound
signing remains the smart host's configured responsibility unless separately
requested. Report support does not imply DMARC report generation.

**F03 — Catch-all.** Add an explicit per-domain fallback only after exact aliases,
with predictable quota/admission behavior and tests for rejected domains. No
plus addressing, external forwarding, or regex address rewriting rides with it.

**F04 — Owned cryptographic primitives inside td-crypto.** The facade crate
exists from M03a. Direct streaming SHA-256 is brought forward to satisfy v1
allocation constraints; the rest of AWS-LC replacement remains future work
and does not block v1. Its normative contracts live in td-crypto/DESIGN.md.
Mail code and its direct dependency do not change with backend selection.

1. Follow td-crypto/DESIGN.md's primitive inventory and review requirements.
   Inventory the entire configured TLS backend, plus DKIM operations if F02
   has landed. Specify algorithms, secret lifetimes/erasure limits,
   constant-time and failure contracts, ECDSA nonces, entropy and optimization
   barriers. New unsafe surfaces need the normal UNSAFE.md amendment first.
2. Implement independently reviewed primitive increments privately in td-crypto,
   with pinned/approved known-answer, adversarial and differential inputs.
   Require independent cryptographic and exact-artifact side-channel review,
   repeated when compiler/flags/target change. Tests alone are insufficient.
3. Implement its private Rustls CryptoProvider bridge as well as the direct
   operations, except the separately qualified direct SHA-256 cutover. Other
   candidate paths stay test-only until cutover; there is no public
   feature/configuration selector or new mail dependency. The reviewed test
   closure may contain both implementations for qualification.
4. Run shared conformance, complete M07 mail/TLS integration and bidirectional
   persisted-key/rollback fixtures. Requalify resources and static x86-64 musl
   linkage; record aarch64 validation status. Atomically select the qualified
   backend and remove obsolete AWS-LC dependencies/native build inputs from all
   affected locks/build wiring. Rustls remains private inside td-crypto; this
   replacement alone does not make its entire closure std-only. Algorithm,
   trust or format changes are separate explicit compatibility work.

## Completion report for each milestone

Record which tasks/commits are complete, which capability/resource claims now
have evidence, the exact binaries/fixtures tested, and what remains unavailable.
Avoid dates or completion marks copied ahead of implementation. The final v1
report must link the conformance table, memory measurements, crash results,
real td-mail integration evidence and tested migration/restore procedure.
