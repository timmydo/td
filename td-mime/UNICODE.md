# Unicode data and bounded normalization

This upstream data was approved for td-mta and moves with its parser into
td-mime. It adds no Cargo
crate, runtime file dependency, network fetch during a build, or Unicode
library.
M06m supplies committed inputs and cold verification tooling; M06n generates
compact tables reproducibly. M06o adds fixed runtime lookups and algorithmic
Hangul. M06p supplies bounded NFC over resident valid UTF-8; M06t adds
resident unstructured-header decoding before NFC. Structured header parsing
and protocol integration remain open.

## Inputs

Use Unicode 17.0.0. Each data URL is the exact filename below appended to
`https://www.unicode.org/Public/17.0.0/ucd/`. Fetch only during explicit
source provisioning, verify size and SHA-256 before use, then build/test
offline. The license URL is `https://www.unicode.org/license.txt`; its digest
pins the notice even if that URL later changes. Do not silently accept a newer
notice.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| UnicodeData.txt | 2198209 | `2e1efc1dcb59c575eedf5ccae60f95229f706ee6d031835247d843c11d96470c` |
| DerivedNormalizationProps.txt | 1377582 | `71fd6a206a2c0cdd41feb6b7f656aa31091db45e9cedc926985d718397f9e488` |
| NormalizationTest.txt | 2827429 | `5019ffd530751a741900c849c0e010332f142a3612234639bd200b82138a87db` |
| license.txt | 1995 | `e7a93b009565cfce55919a381437ac4db883e9da2126fa28b91d12732bc53d96` |

Retain the complete Unicode License V3 notice with generated tables and test
material, including in distributed documentation. Its copyright line is
`Copyright © 1991-2026 Unicode, Inc.`; this is separate from td's MIT
license. The reviewed checkout provisions exact copies under unicode/17.0.0.
The ordinary td-mime test suite checks every file's regular type, exact size,
complete SHA-256 and UTF-8 before returning any corpus. The license is
verified against its full approved digest. Tests do not consume Markdown;
review checks this pin table against the compiled declarations. Final input
symlinks are refused; ordinary parent-path resolution remains operator
controlled. No ambient cache or network fallback is used. The three large
upstream corpora use compact Git binary summaries; their approved pins and
byte-verifying tests are the review boundary. The license remains a normal
text diff. No data is edited or normalized in transit.

The cold examples/unicode_inputs.rs tool uses the same verifier. Build
the cold examples with the committed lock:

```text
cargo build --frozen --manifest-path td-mime/Cargo.toml --examples
```

The current x86-64 GNU host with the default target directory can
independently verify a supplied folder:

```text
td-mime/target/debug/examples/unicode_inputs td-mime/unicode/17.0.0
```

Verification failure exits unsuccessfully before printing any results. On
success it reports each filename, byte count and digest; output errors also
fail the command and may leave partial output. It writes no source files.
These source copies and tool are outside the service executable and its
memory plan.
Input verification and table generation do not establish NFC conformance.

## Generated tables

The cold examples/unicode_generate.rs tool first verifies the complete corpus,
then emits deterministic Rust source on stdout. It does not write a source
file itself. Capture a candidate from the repository root:

```text
td-mime/target/debug/examples/unicode_generate td-mime/unicode/17.0.0 > td-mime/unicode/tables.rs.candidate
```

Check the command succeeds and review the candidate before replacing
src/unicode_tables.rs. Shell redirection creates or truncates the candidate
before verification; any failure can leave it empty or partly written. Keep
candidates outside src so they cannot enter portable source staging.

The generator runs offline and does not invoke a formatter. The emitted
source already matches the repository's default formatting. The ordinary
test suite compares fresh output byte for byte with the committed file,
compiles the tables, and checks ordering, scalar values, offsets, class
count and payload size. The full license notice is retained as comments in
the generated source.

The current tables contain 2081 decomposition entries (u32 scalar, u16
offset, u8 length), 3450 u32 decomposed scalars, 403 combining-class ranges
(u32 start, u32 end, u8 class), 961 composition triples (three u32 scalars)
and 1488 simple lowercase pairs (two u32 scalars). Their compiled array
payload totals 58720 bytes on the initial target layout. Slice descriptors,
code and mapped-page rounding are separate. M06o links these tables into the
library through checked fixed lookups and implements algorithmic Hangul.
Their static payload fits within the existing process allowance in
../td-mta/RESOURCES.md.

Decomposition entries are fully recursively expanded, without canonical
reordering. The normalizer must still order combining classes and compose.
Expansion uses an explicit cold work stack with cycle detection and rejects
more than four output scalars. Composition pairs come from the original
two-scalar mappings before expansion, with Full_Composition_Exclusion applied.
The source interpretation follows Unicode 17's UAX #15 revision 57 and UAX #44
revision 36. No ambient Rust or host Unicode tables are consulted.

Generate sorted compact arrays for canonical decomposition, nonzero canonical
combining class, canonical composition, and simple lowercase mapping.
UnicodeData's compatibility decompositions are excluded. Composition excludes
every Full_Composition_Exclusion from DerivedNormalizationProps. Hangul
decomposition/composition is algorithmic. Absent entries mean identity or
combining class zero as appropriate, including scalars unassigned in 17.0.0.
Reject surrogate mapping targets, duplicate/unsorted source records, malformed
ranges and invalid decomposition graphs. Validate the source's First/Last
range pairs, including its three category-Cs surrogate ranges, but omit those
ranges from scalar tables. Their presence in UnicodeData is valid; no Rust
char or decomposition output may represent a surrogate.

Pin generation to numeric scalar order, exact formatting and explicit integer
widths. Check recursive canonical decomposition is acyclic and expands to at
most four scalars per input scalar (Hangul needs at most three). Checked table
offsets/lengths and ordinary safe lookups apply in generated code too. The
source has 55 distinct nonzero combining classes; verify that count during
generation as an additional bound on the replay policy below. No build script
downloads or regenerates tables on an ordinary cargo build. Commit generated
tables and compare fresh offline generator output byte for byte in the owning
gate. Runtime integration must retain the static payload accounting above
within the existing process allowance in ../td-mta/RESOURCES.md.

Use simple lowercase from this pin for ../td-mta/POLICY.md's default comparison/search.
Do not substitute Rust's evolving Unicode tables or claim full case folding,
locale collation, or the IANA i;unicode-casemap algorithm.

## Normalization

Implement NFC as specified by Unicode 17.0's normalization algorithm:
recursive canonical decomposition, stable combining-class ordering, and
canonical composition with blocking and Hangul rules. Do not insert CGJ or
remove marks to make input fit a fixed segment; that would change the
requested text. Output may stream into an admitted response or message writer.
No complete normalized header string or message-sized buffer is required.

Normalization reads resident, immutable header bytes or a bounded request/
configuration string through a restartable decoding cursor. Nested-message
headers have already been gathered into the header arena; NFC never restarts
an entire nested body pipeline. A cursor checkpoint records the source offset,
encoded-word/charset/UTF-8 state and pending canonical decomposition (at most
four scalars). Each checkpoint is at most 256 bytes. It includes state inside
an encoded word, so resuming does not rescan all earlier words or segments.

../td-mta/RESOURCES.md partitions the existing 32 KiB conversion region: NFC uses a 2
KiB fast segment (256 eight-byte scalar/class cells), 1 KiB class counts and 1
KiB for four cursor checkpoints. At overflow, keep a checkpoint at segment
start and replay from the resident source, without a growing segment, disk
file, sort lease or allocation. End/source checkpoints let the outer cursor
resume once the segment is emitted. All source replay and scalar work are
charged, even when reads are from the header arena rather than disk.

Canonical ordering segments end before the next class-zero scalar. A segment
contains a possible starter followed by nonstarters, or only initial
nonstarters before the first starter. Carry the possible starter separately.
Count occupied nonzero classes, then replay that segment once per occupied
class in increasing order, preserving source order within a class. The pinned
data permits at most 55 occupied classes. Replay starts at the segment's exact
checkpoint, never at the beginning of the complete header.

A first ordered traversal computes the composed starter and whether any marks
remain unconsumed. If any remain, emit that starter and repeat the same
composition decisions to emit those marks in order. If none remain, retain the
starter: the next class-zero scalar may compose with it under the normal
table/Hangul rules. This includes non-Hangul pairs such as U+0B47/U+0B3E. If
it cannot compose, emit the held starter before processing the new one.
Unconsumed marks block composition with a following class-zero scalar. A
leading nonstarter-only segment has no starter to emit and needs just one
ordered traversal. Flush retained state at EOF. Ordering boundaries and
composition/output boundaries are distinct. The fast and replay paths must
produce identical octets for every split point.

At most 110 ordered replay passes visit a segment, plus its initial scan.
Source checkpoints make the whole header's work proportional to its own
length times this fixed bound; adjacent segments do not repeatedly scan a
prefix. At header_bytes=1 MiB this spans at most 111 MiB of raw input
extents and four times as many decomposed scalars. Charged byte visits
additionally include bounded decoder lookahead, recognizer scans and
charset-byte visits. These are resident-source visits; replay performs no
disk reads. Encoded-word/charset decoding cannot produce more scalars than
input octets under ../td-mta/POLICY.md's charset set. No extra disk reserve is
required, and a busy external sort cannot block NFC.

In addition to ../td-mta/ADMISSION.md's enclosing deadline/work budgets, one email's
aggregate header projection permits at most 16 MiB of source visits and
16000000 scalar/decoder steps; repeated visits count again. This fixed
deterministic ceiling bounds one hostile combining run. Ordinary maximal-size
ASCII headers do not need replay and fit it. On exhaustion return the
interpretation-limit outcome in ../td-mta/POLICY.md, never unnormalized success or a
malformed-Unicode classification. M06 must measure actual steps with resumable
chunks of at most 256 scalar/ decoder transitions. Raw download,
header-independent projections and header-independent queries remain
available. No runtime timing claim is made.

## Current resident implementation

M06p implements the ordering/composition algorithm above for immutable
valid UTF-8 strings. Its fixed 3072-byte scratch and at most 1024 bytes of
cursor plus aggregate budget fit the existing 4 KiB reservation. Private
source checkpoints now fit 256 bytes each, including the M06t header-
decoder state and pending canonical expansion. ../td-mta/API.md sections 1.23 and
1.27 define exact charging and ownership. Valid-UTF8 turns have at most 32
state transitions and 128 charged steps; unstructured-header turns have one
transition and at most 228 steps. Phrase decoding adds a 231-step bound;
all fit the 256-step ceiling. One job-meter record prepays 16 internal
steps using private cursor credit; individual steps still debit the header
budget. The cursor's output charge method lets the caller debit the same
meter before serialization and check its deadline after a turn. External
clock/cancellation checks belong to the caller. On any error the entire
provisional property must be discarded.

Ordinary tests run all five NFC equations for each of the 20034 official
vectors, including idempotence, plus every scalar outside Part 1 as an
identity case. Independent fixtures exercise 255/256/257-cell
boundaries, long equal-class runs, all 55 occupied classes in descending
order, pending decomposition restoration, leading marks and class-zero
composition. A 1 MiB ASCII source fits the aggregate ceiling without replay;
an exact source-byte oracle proves the long prefix is visited once even
when its tail replays. The ASCII and hostile-replay fixtures use the default
foreground job limits; hostile replay reaches InterpretationLimit with job
records remaining. Deadline faults cover resumed scanning, insertion,
composition replay and output replay. A supplied Tick is fixed throughout a
pure poll: expiry during the turn is observed by the caller's post-turn
zero-byte output charge or the next poll, not an internal clock sample.
Source/step
limits and aggregate retirement are checked. The isolated Rust allocation
probe covers fast and replay paths using fixed scratch.

Both entry points retain resident immutable source for the cursor's lifetime.
M06t composes unstructured-header unfolding, word/charset decoding, malformed
replacement and scalar filtering before NFC. A private charging adapter
retains both live budgets across candidate scans, word decoding and replay.
Header checkpoints include a checked deterministic turn ordinal and immutable
source identity, so even positions inside words compare without prefix scans.
The before-EOF checkpoint ends replay; the consumed state is kept separately
for resumption. Diagnostics accumulate across every pass.

Additional literal oracles cover composition across encoded words and folds,
Hangul, replacement before NFC, overflow spanning words and pending canonical
decomposition, exact prefix visit counts, shared source/step limits and
progressed deadline failures. A maximal one-MiB ASCII header fits default
budgets and retains exact source accounting. Isolated allocation intervals
cover the complete decoding/normalization fast and replay paths. Structured
headers, source gathering, outer property failure publication and worker
scheduling remain open; this does not qualify their stacks or the service RSS.

M06am adds validated phrase names as a third resident source. It preserves
whole-field placement context and binds the proof to its exact phrase range.
Unquoting, sole-quoted trimming, conditional encoded-word gap suppression and
scalar filtering precede NFC. A checked turn ordinal plus field/range identity
identifies deterministic decoder progress; pending decomposition remains part
of each source checkpoint. The same source/cursor/scratch ceilings hold.
Every decoding prepass and replay spends the aggregate budget before work;
phrase polls stay within 231 steps and 15 job records. Initial grammar proof
validation is separately caller-charged; complete header-form aggregate
admission remains part of the future owner composition.

Literal and encoded-name oracles cover canonical/Hangul composition,
noncharacter filtering, overflow across words and pending decomposition, and
prefixes that are not revisited with their hostile tail. One MiB of ASCII
fits the default budgets; its decoder visits each byte four times plus two
initial peeks. Shared budget exhaustion and progressed deadline/output refusal
remain terminal. The allocation probe composes validation, decoding and NFC
without allocating after admission. Comment fallback and whole-field/JMAP
publication remain open, as do worker-stack and service RSS qualification.

M06an adds selected fallback comments through an exact whole-comment proof.
Quoted-pair decoding, unfolding, explicit grammatical whitespace policy and
scalar filtering precede NFC. Original comment, quoted-pair and LWS boundaries
govern encoded-word placement; decoded characters never become new syntax.
Source pointer/length plus checked turn identity and pending decomposition
make Copy checkpoints exact without scanning prefixes. Nested comment
parentheses are ordinary normalized display text after outer-parenthesis
removal.

The existing source/cursor/scratch ceilings hold. Comment polls fit 231
aggregate steps and 15 job records, with all conversion and replay charged.
Tests cover canonical/Hangul composition, NUL removal between base and accent,
encoding diagnostics, multi-class overflow and bounded prefix cost. Allocation
intervals compose proof validation and normalization, including refusal.
Initial grammar and complete field admission remain caller-owned; whole-field
response publication and worker/RSS qualification remain open.

M06bz extracts this same engine into std-only td-nfc; the old mail engine is
removed in the same landing. The mail crate retained all Unicode 17 tables, source
validation/decoding/filtering, canonical decomposition and class lookups. Pure
Copy source checkpoints retain exact decoder/pending-decomposition identity;
separate generic admission borrows the original mail budgets/credit and uses
the real Tick supplied for that poll. Source callbacks and engine
transitions preserve exact charges,
one/32-transition quanta and the existing source/cursor/scratch ceilings.
Standalone engine correctness is conditional on caller-supplied deterministic
canonical source/tables; its API owns no ambient Unicode version. The official
vectors and all adversarial mail replay/budget/allocation fixtures remain the
consumer oracle. Passive borrowed phase/checkpoint inspection is not a restore
API or output validity proof. Parameter checkpoint composition remains open.

M06ca composes original-source MIME parameter display with the same td-nfc
engine and Unicode 17 tables. Private pure checkpoints retain nested lexical,
family, octet, charset/word and filtered-scalar progress plus exact pending
decomposition. Their larger separate source is qualified within existing
parser/conversion reservations; the existing header source enum is unchanged.
The live owner retains original aggregate/job allowances and credit across
every replay and output/final check, using the real Tick supplied for each
call. NFC applies after complete
candidate validation and existing ordinary compatibility/extended filtering;
completed normalized prefixes are never reconstructed to find a checkpoint.
Typed refusals stay sticky and discard provisional property output. Filename
precedence/retention and whole-worker/native/RSS qualification remain open.

## Acceptance evidence owned by M06

Run the complete pinned NormalizationTest file's NFC equations, normalization
idempotence and the specified identity cases outside its listed repertoire.
Include canonical exclusions, decomposed/composed Hangul, leading marks,
equal-class blocking, supplementary scalars, invalid UTF-8 replacement before
NFC, empty input and unassigned scalars. Do not compare against the same
generator as the sole oracle: the official vectors supply expected values.

Run fast/replay boundary cases at 255/256/257 cells; descending classes, long
equal-class runs and all occupied classes; segmentation at every input byte;
work/cancellation failures before and during replay. Verify no admitted
allocation, no partial successful property, no CGJ insertion, no prefix
rescanning, exact cursor restoration and fair worker yielding. Check
tampered/missing inputs, generator reproducibility and the compiled
static-table footprint. This document and the downloaded research inputs are
not evidence of those tests.

## Pinned license notice

The complete notice is retained here so offline provisioning remains possible
if the upstream license URL changes. The fenced UTF-8 text, with LF line
endings and one final LF, has the license digest above. The committed
license.txt copy matches those bytes. Retain it alongside generated material.

```text
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2026 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```
