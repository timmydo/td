# td-alloc

td-alloc owns fallible growth of td's std collections. It is std-only and
forbids unsafe code. Cargo consumers depend on this sibling crate; the
control plane and direct-rustc consumers mount the same source. A program
must use one instance for code that shares a reservation scope.

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
