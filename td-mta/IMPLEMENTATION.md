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
local path the mail crate's only direct dependency, including dev/build scope;
keep the shared crate independent of mail types. Both crates carry standalone
locks and join the discovered host/sandbox test and Clippy roster. No external
backend or new primitive implementation is introduced in this increment.

**Acceptance:** consumer compilation proves shared trait identity and error
conversion, including propagation of a partial-fill failure through a test
caller using `?`. This is interface evidence, not production token handling.
Dependency confinement rejects another direct mail dependency. The gate
discovers both crates and the dependent mail tests when td-crypto changes.
No test-only fake is described as a cryptographic implementation.

### M03b1 — Private backend admission and offline gates

Add Rustls, aws-lc-rs and roots only inside td-crypto. Keep every public type,
error and configuration opaque to upstream types. Pin a minimal compatible
closure with one AWS-LC version pair and explicit features. Record licenses
and why each dependency is present. M03b2 records all portable native
compiler/assembler/generator inputs. td-mta's lock necessarily includes this transitive closure, while its
manifest still names only td-crypto. Amend AGENTS.md and the common host/gate
checks atomically to admit these exact named closures and preserve the std-only
rules for other crates. Do not exclude either crate from tests or Clippy.

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
changes, an extra direct mail dependency or an unapproved crypto dependency
fail the gate. Public-API confinement rejects upstream type/re-export leaks.
Offline source provisioning is reproducible and has no host libssl/td-net
runtime dependency. Pin DESIGN section 3's portability toolchain manifest,
including target std, C compiler/linker/sysroot and checksum/provenance evidence.
The clean fixture builds without ambient undeclared inputs. Establish bounded
TLS smoke behavior before any service consumer depends on the backend.

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
- **M04c3a:** checked physical-space arithmetic in `admission/space.rs`,
  including concurrent-probe correction and checkpoint completion capacity.
- **M04c3b1:** fixed logical quota groups and linear effect tickets in
  `admission/quota.rs` and `admission/logical.rs`; no physical I/O permission.
- **M04c3b2:** coordinator-owned selected/journal scalar ledger and derived
  checkpoint capacity in `admission/writer.rs`, including reserved candidate
  frames, dedicated append tickets and simulated writer-barrier transitions.
  Rollover preserves outstanding leases and stays closed for physical admission.
  M08 supplies trusted selected/committed state and exact metadata accounting.
- **M04c3b3a:** bounded filesystem registry and probe observations consumed
  once in `admission/filesystems.rs`; adapter identity and actual probes remain M05.
  This matches probe data without installing any physical reservation.
- **M04c3b3b:** implemented writer preparation with same-cell physical
  reservations,
  fresh probe assessment, checked extensions and atomic effect/cancel updates.
  Every filesystem delta is prevalidated before any logical installation.
  Existing leases remain bounded by 64 combined 128-byte records.
  `admission/coordinator.rs` starts closed until baseline checkpoint capacity
  passes fresh probes; ordinary extensions and one-shot journal appends use
  the same atomic logical/physical accounting. No platform I/O is implemented.
- **M04c3b3c1:** implemented a checked registry-wide probe epoch and atomic
  invalidation in
  `admission/filesystems.rs`. Earlier tickets and already matched samples
  refuse at the same Tick; no capacity or identity changes. The registry
  fence alone does not implement checkpoint publication or reopening.
- **M04c3b3c2:** implemented protected-to-building capacity transfer, overlapping
  checkpoint quota, closed writer transitions and post-fence probe reopening in
  `admission/coordinator/checkpoint.rs`. One fixed attempt record is independent
  of client lease saturation. Partial/unselected output keeps its charges. M05
  supplies descriptor-backed probes and cleanup proof; M08 owns persistence,
  exact metadata state and view/writer pin authority.
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
Implement ADMISSION.md's checked u64 disk/work configuration and filesystem
reservation coordinator. This does not implement a platform space probe;
M05 supplies that reviewed boundary and its fault-injected fake.

**Acceptance:** oversized/malformed/duplicate/unknown config fails with location
and stable codes; no secret is echoed. Resource overflow, exhausted slots and
log truncation are deterministic. Inject hostile strings to verify JSON escaping.
Bounded structures never increase capacity after construction. No networking,
store mutations, live reload or filesystem log rotation in this task.

## M05 — Immutable blobs and journal commit/replay

ADMISSION.md's physical completion reserves and free-space probe are required
before write admission. Any unsafe platform probe follows the separate
UNSAFE.md amendment/confinement workflow; no new surface is preauthorized.

**Depends on:** M02/M04/M07. **Own:** storage I/O adapter, blob files, journal codec,
store locking and initial replay; no search index or protocol endpoints.

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

Implement bounded header unfolding, encoded words, address/date parsing,
multipart scanning, transfer decoding and part offsets. Implement documented
charset coverage and error/opaque-body representation. Add deterministic MIME
serialization for structured outgoing email, including attachment streaming,
boundary generation through Entropy, reply headers and Bcc separation.
Provision UNICODE.md's approved checksummed sources before offline tests; add
the std-only generator, committed compact tables, complete Unicode license,
reproducibility gate and every official NFC vector. Implement the fixed-memory
fast/resident-replay algorithm with charged work. Search case mappings
come from the same pin. No allocating library or ambient Unicode version may
replace these contracts. Record static table size within process headroom.

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
with resumption disabled. Opaque TLS sessions, gateway authorization, native
allocation and service resource qualification remain pending; these fixtures
do not complete M07 or enable service.

The future TLS contract is specified in td-crypto/TLS.md, including explicit
algorithm sets, local P-256 PEM identity admission, trust/SNI/time policy,
bounded record progress and failure/close behavior. Concrete compiling TLS
interfaces and their qualification are still pending. Implement direct
Crypto/Entropy operations and incoming/outgoing TLS
inside td-crypto. Rustls provider/configuration/verifier/key types never leave
that crate. Implement the conformance, explicit-provider confinement, algorithm
baseline, key compatibility and native allocation/failure qualification in
`td-crypto/DESIGN.md`, retaining its independent fixtures for F04.

The mail adapters implement TlsFactory/TlsTransport through that facade. They
own implicit-TLS and STARTTLS handoff, socket/deadline progress, generation/slot
leases and gateway allowlist authorization. td-crypto owns chain/time/name and
client-certificate verification. Preserve SNI, private CA override, bounded
certificate generations and verified peer semantics. Disable unneeded
resumption/early data. No authentication credentials reach a peer before
verified TLS.

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
- **M07b3 — explicit TLS configuration:** compile opaque shareable handles,
  fixed algorithm inventories, SNI/ALPN selection and supplied-clock bridge.
  Test excluded algorithms, no global provider, mandatory gateway client
  certificates and independent resumption refusal through this layer.
- **M07c — opaque sessions:** implement and pin concrete public signatures
  for TLS.md's record/plaintext/output/status/close operations. Confinement
  tests cover the complete resolved public API. Local peers exercise one-byte
  fragmentation, short buffers, backpressure, truncation, exact authenticated
  evidence and permanent retirement after returned errors/Rust unwinds.
- **M07d — mail transport:** implement existing ports through that facade,
  with socket/lease/deadline handling, implicit client TLS, STARTTLS transition
  fixtures and gateway pin plus address authorization. No provider type or
  diagnostic reaches td-mta. Use td-mail-compatible local HTTPS/SMTP fixtures.
- **M07e — resource/service admission:** qualify complete generation overlap,
  session/handshake peaks, worker entropy and Rust/native stack/allocation/RSS
  on the portable artifact before activating the adapters. Amend the checked
  ledger if measurements cannot fit; never infer a bound from buffer limits.

**Acceptance:** shared backend tests exercise known-answer/independent crypto
oracles, malformed keys, explicit TLS policy and upstream API confinement.
Mail integration fixtures cover valid/untrusted/expired/wrong-name chains,
mTLS admission, fragmented records, STARTTLS reset, handshake saturation,
wrong keys and returned/fatal failure boundaries. Test M02 digest-bearing
fixture inputs through the real provider; fake output is not a digest oracle.
Fit cold/session/handshake generations, native allocations and Rust/C peak
stack within the service ledger and measure whole-process RSS. Warm workers
before admission and report secret-erasure limits. Any allocator hook first
requires an UNSAFE.md amendment. No live CA/provider contact, ignored certificate
errors, hidden second backend or additional direct mail dependency.

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
