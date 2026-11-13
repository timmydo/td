# td-alloc

td-alloc owns fallible growth and budgeted storage. It depends only on std.
Cargo consumers use `src/owned.rs`, whose private raw allocation boundary is
recorded in UNSAFE.md section 28. The control plane and consumers needing only reservation helpers
mount the safe-only helpers in `src/lib.rs`. Direct-rustc consumers of
td-json compile the owned crate root, just as Cargo does. A program uses one instance for code sharing a
reservation scope.

## Allocation contract

`reserve` and `reserve_exact` take additional elements relative to the
vector's length. They check existing capacity before reserving. `push`,
`extend_from_slice`, `resize`, `filled`, `copy`, and `with_capacity` reserve
before any operation that can grow storage. A failed reservation preserves
the vector's existing elements. `push` consumes its argument even on
failure; constructing it and running its destructor remain the caller's
responsibility. Copy and repeated-fill operations require `Copy`, so an
element's `Clone` implementation cannot allocate behind the helper.

`reserve_map` and `insert` provide fallible hash-map growth; `push_back`
provides fallible deque growth. Hashing/equality and element destruction
remain caller-owned code. Insertion reserves conservatively even when the
key could already exist.

`string`, `push_str`, and `reserve_string` similarly preserve valid UTF-8
and existing contents on refusal. `format(format_args!(...))` writes into
fallibly grown storage and distinguishes a storage failure from a Display
failure. A Display implementation is arbitrary code: its own allocations
are outside the guarantee. Use primitives and allocation-free error types
when formatting on an error path. Formatting a result with ordinary
`format!` or `to_string` outside this API reintroduces an allocation.

`Error` has no heap storage. Capacity overflow and allocator refusal are
one allocation error; input size, CPU and retained-memory limits belong to
the consumer and retain its domain-specific errors. Errors must be
propagated, or handled with a pre-existing/static fallback. Retrying the
same request without releasing resources or changing admission is not a
recovery policy.

The helpers retain std collection types for interoperability. Returning a
Vec is not an assurance about later calls on it: unchecked growth, cloning,
collecting, formatting, boxing or conversion to reference-counted storage
must still be reviewed and guarded. There is no global allocator override,
catch-unwind recovery from OOM, or claim to prevent a kernel/cgroup OOM kill.
Allocator success need not mean physical pages have been committed.

## Scoped admission and failure tests

`with_reservation_limit(n, f)` admits at most n capacity-growing reservation
attempts through this instance's helpers, on the current thread, during f.
It needs no heap allocation. Existing capacity and zero-sized element
storage cost no reservation. Nested scopes cannot increase an outer
allowance; consumed admissions remain charged when the nested scope exits,
including during unwinding. Threads have independent scopes. A reservation
that reaches std and fails still consumes an admitted attempt.

This is an operation count, not a byte limit or a live-memory measurement.
It does not intercept std allocations or follow newly spawned workers.
Tests step the allowance from zero until a representative operation
succeeds, require each earlier refusal to return the domain's resource
error, and verify that a later unrestricted operation still succeeds.
Actual capacity overflow is tested separately through std's fallible API.
These tests complement source review; they cannot find an allocation that
bypasses these helpers.

## Adoption and enforcement

Picture decoding, raw DEFLATE decoding, picture fitting and td-charset's
whole-input `decode` use the shared reservation path. Their codecs retain
input/output/work bounds. The raw inflater's legacy String-returning
wrappers and image encoders are outside the allocation-free error
guarantee. General UI, fetch, navigation and other existing subsystem
allocations remain migration debt.

JMAP request byte and token backing uses owned child budgets under one
HTTPS worker-pool budget. Parsing, validation, response emission and recovered
buffers retain admission until their final owners drop. JMAP dispatch-owned
call/reference/ID plans, creation overlays, mutation plans and response
buffers, Email/get header selections and MIME projection scratch share that
same child and its explicit method allowance. Search/draft composition,
transport/library internals and response files
retain separate limits; this is not a whole-request allocation guarantee. The request storage module and
the allocation-free HTTP date formatter have additional source guards that
reject recognized allocation operations and standard heap collection names.
Migrated leaf JMAP method modules forbid standard heap owners and recognized
allocating constructors/conversions. Their fixed-buffer growth methods and
budgeted reservations retain operation counts in the debt inventory.
Mixed adapter modules remain covered by the debt ratchet alone.

The repository allocation guard is a source-level ratchet over production
Rust. It excludes recognized test-only code and literals/comments, scans
new files too, and compares each file's recognized allocation operations
with a committed debt inventory. Direct std reservation calls also count,
so new code uses the shared admission path. New operation categories or
increases require migrating to td-alloc or a specifically reviewed debt amendment.
Removed debt must be removed from the inventory. Inspect it with
`td-builder gate-crates allocations`; produce a candidate inventory with
`td-builder gate-crates allocation-inventory`. Review changes before replacing
the committed inventory; generation is not approval. The allocator
implementation's reserved growth operations are recorded explicitly too;
it has no directory-wide exemption. The host guard itself retains the
control plane's allocating source scanner, filesystem reads, ordered maps
and String diagnostics, recorded in that same inventory. It is an explicit
bootstrap debt admission, not an allocation-safe runtime boundary; migrating
it requires migrating that whole scanner operation and its error paths.

The guard is deliberately not a type-system proof: aliases, custom methods,
macro expansion, callbacks, trait implementations and dependency internals
can hide allocations. Counts per file and category also cannot distinguish
one removed site from one added site of the same category. Its inventory
can contain false positives such as nonallocating custom `push` methods. Reviewing the call graph and testing
failures remain required. A baseline is a record of debt, not permission
to add unchecked allocation to existing code.

Migration proceeds at operation boundaries: parsers and decoded buffers,
then rendering, networking and storage. Replace an operation's diagnostics
and fallback paths along with its main allocations. Shared ownership that
requires a newly allocated Arc/Box cannot be made fallible by a Vec helper;
transfer ownership or reserve reusable storage, or design and audit that
surface separately under UNSAFE.md. The stable std allocator API does not
currently provide a safe fallible replacement for every collection.


## Owned byte budgets

`Budget` is an explicitly passed, movable shared identity. Creating it and
its children is fallible; `try_clone` shares the identity without allocating
and refuses reference-count overflow. There is no implicit thread-local
budget, reset operation or unlimited constructor. A chain has at most 16
nodes, including the root, bounding admission, rollback and destruction.
Each budget charges its own control block; a child also charges that block
to every ancestor. Its limit includes this metadata. The control block
stays charged while any handle, descendant or storage owner remains alive.

`charge` admits bytes against every ancestor before storage is allocated.
Concurrent partial admission can conservatively refuse another operation;
it cannot permit oversubscription. A failed ancestor admission rolls back
all descendant counters. `used` includes reservations currently in progress.
The non-cloneable `Charge` returns its bytes on drop; consumers of external
resources must release those resources before dropping their charge. Forgetting
an owner leaks its admission too and cannot replenish a budget.

`Charge::shrink_to` returns only the excess reservation to every ancestor,
without releasing or reacquiring the retained bytes. It never allocates.
Equal sizes and zero are allowed; a larger requested size returns `Limit`
without changing the charge. Release the corresponding resources before
shrinking: changing accounting does not resize or free storage. A zero-byte
charge still retains its budget identity and metadata until dropped.

The limit bounds the requested layouts of participating heap allocations,
including spare capacity and budget metadata. It excludes allocator-internal
metadata, fragmentation, stack storage, OS page commitment, mappings and
allocations outside the API. It is not an RSS or cgroup guarantee. Shared
library internals and user-supplied constructors/destructors remain outside
this contract. A container of owned strings charges its element array;
each string's storage needs its own budgeted owner. Service policy retains
separate input, work, concurrency, descriptor and disk limits.

## Owned storage and arenas

`BudgetVec` admits an exact requested layout before allocating and retains
its charge alongside the vector. A grow admits the whole replacement while
the old capacity remains charged, moves initialized values, then frees the
old allocation before returning its charge. Failed admission/allocation
preserves the old values and capacity. Amortized growth clips spare capacity
to currently available headroom
across every ancestor and the layout limit. A stable tight cap therefore
does not force repeated one-element reallocations and quadratic copying.
Headroom is only a sizing hint: concurrent admission can change it, and
the complete replacement charge remains authoritative. If that admission
fails, growth may fall back to the exact requested size once. It does not
retry an allocator refusal. Clear/truncate retain storage and therefore
its charge.
Zero-sized element storage costs no bytes or allocator attempt.

Only borrowed slices escape. There is no mutable std collection access,
infallible Clone, raw ownership extraction, or implicit budget transfer.
`Buffer` freezes length/capacity for queued parser backing; moving or
recovering its `BudgetVec` retains admission. `BudgetString` preserves UTF-8
and offers fallible append and constant-time string borrowing. Copy/fill
operations do not invoke a custom Clone implementation.

`Arena<T>` owns typed values with stable, bounds-checked indices. It grows
fallibly from the same budget and destroys values on drop. References borrow
the arena and exclude simultaneous growth; addresses are not stable across
growth. Indices are offsets local to one arena, not globally branded handles.
No raw heterogeneous bump allocator or unrun-destructor policy is implied.

Budget exhaustion, size/reference-count overflow and allocator refusal are
separate allocation-free errors. Callers use existing/static refusal paths
and reserve completion capacity before durable effects. The operation-count
failure-injection scope also covers both raw allocations in this boundary;
it remains thread-local test admission, not byte accounting.

Every migrated application scenario needs before/after performance evidence
under the same build profile and workload, covering representative latency,
throughput and retained/peak memory. Primitive microbenchmarks supplement the
scenario measurement; they do not establish whole-service performance.
