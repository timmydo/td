# Bounded canonical composition

This std-only crate owns the deterministic canonical ordering/composition
engine. It owns no source storage, Unicode tables, decoding, filtering,
clock, I/O, work allowance or retained output. The caller supplies one
immutable deterministic scalar source, its pinned Unicode decomposition,
combining-class and composition rules, and a live original admission context.
This is a conditional normalization engine, not a validator of arbitrary
caller-supplied Unicode tables or checkpoint implementations.

Source is a Copy pure checkpoint. Copies contain source identity, exact
progress, decoder/decomposition state and pending scalars only; they contain
no admission allowance, credit or live output owner. Equality compares exact
replay progress including pending decomposition, never a completed-prefix
scan. Each Cell/Yield/End reproduces the same event and next checkpoint.
Reader callbacks admit all source/decomposition/classification work before
access. The engine calls Admission::step before each fixed transition; the
caller binds current cancellation/time and its original aggregate/job quotas.
The source quantum is fixed in 1..=32; larger or zero quanta refuse. A poll
performs at most that many transitions and emits at most one provisional
scalar. Callback work is bounded and charged by the caller, not inferred by
the generic engine. compose, at and is_encoding_problem perform fixed-bounded
work covered by the enclosing transition's Admission::step. turns runs once
per active poll before admission and returns the same fixed quantum for every
checkpoint without source access. Only Reader callbacks receive live context.
Copies of checkpoints never copy the context.

Scratch has 256 scalar/class cells and 256 u32 class counts, exactly 3072
bytes on qualified targets. The caller allocates/touches it before admission
and lends it exclusively to one non-Clone/non-Copy Cursor. Cursor state holds
four pure checkpoints and fixed scalar/class positions, flags and bitmaps;
its size depends on Source and Source::Error. Its bound is qualified by each
consumer within that consumer's parser reservation. Scratch is separate from
those checkpoints and cannot be simultaneously borrowed by two live cursors.

Segments with at most 256 nonstarters use stable bounded insertion sorting.
A longer segment retains class counts and presence bits, then replays the
exact unfinished segment once per present nonzero class. Replay restores
pending decomposition and decoded-source progress without reparsing finished
prefixes. Stable equal-class order, canonical blocking, composition and
adjacent starter composition follow the caller's canonical Unicode rules.
The u8 class domain permits at most 255 occupied nonzero classes and
510 ordered passes plus initial scan; a consumer can pin a smaller Unicode
class inventory, as mail does. Overflow is an algorithm path, not a larger
allocation or arbitrary truncation.
Every scan/insertion/ordering/emission/composition transition is admitted.
Source errors, count/position overflow and inconsistent replay refuse.

Scalar events remain provisional until the enclosing operation drains the
whole source and obtains fresh admission. Complete is cached and inert.
check calls caller-supplied live admission, including after Complete; refusal
is sticky, suppresses inspection and prevents later scratch release. poll,
check and into_scratch preserve the original refusal without new work.
Inspection returns only borrowed passive checkpoints and phase diagnostics;
it is neither a restoration mechanism nor output/source validity authority.
Phase diagnostics have no stability contract.
into_scratch requires healthy Done; Done may coincide with the final Scalar,
whose output admission/copy still belongs to the caller before publication.
Dropping a cursor releases its exclusive borrow but proves no erasure.

Standalone fixtures qualify stable ordering, equal-class blocking, short and
long segment replay, one/32-transition quanta, the three-cell fixture admission sweep,
cached/fresh refusal, workspace layout and live ownership. Mail keeps its
pinned Unicode/source fixtures, decomposition checkpoint adversaries,
aggregate/job deadlines, allocation intervals and existing resource ceilings.
No generic engine fixture claims full Unicode database or whole-worker,
native/RSS or retained-output qualification.
