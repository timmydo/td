# Unicode data and bounded normalization

This is td-mta's approved upstream data dependency. It adds no Cargo crate,
runtime file dependency, network fetch during a build, or Unicode library. M06
owns the std-only generator, generated Rust tables and normalizer. This
contract pins their inputs; none of that implementation is claimed here.

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
`Copyright © 1991-2026 Unicode, Inc.`; this is separate from td's MIT license.
M06 must add checked provisioning and regeneration commands plus offline gate
inputs. A developer's ambient cache, Python Unicode version, or network access
is not a substitute for those pinned inputs. Until that integration exists,
these are frozen source declarations, not an executable provisioning claim.

## Generated tables

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
gate. Their static bytes fit the existing process allowance in RESOURCES.md;
M06 records actual size before enabling an implementation.

Use simple lowercase from this pin for POLICY.md's default comparison/search.
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

RESOURCES.md partitions the existing 32 KiB conversion region: NFC uses a 2
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
Source checkpoints make the whole header's work proportional to its own length
times this fixed bound; adjacent segments do not repeatedly scan a prefix. At
header_bytes=1 MiB this is at most 111 MiB of source bytes and four times as
many decomposed scalars, before cursor bookkeeping. These are resident-source
visits, not disk reads. Encoded-word/charset decoding cannot produce more
scalars than input octets under POLICY.md's charset set. No extra disk reserve
is required, and a busy external sort cannot block NFC.

In addition to ADMISSION.md's enclosing deadline/work budgets, one email's
aggregate header projection permits at most 16 MiB of source visits and
16000000 scalar/decoder steps; repeated visits count again. This fixed
deterministic ceiling bounds one hostile combining run. Ordinary maximal-size
ASCII headers do not need replay and fit it. On exhaustion return the
interpretation-limit outcome in POLICY.md, never unnormalized success or a
malformed-Unicode classification. M06 must measure actual steps with resumable
chunks of at most 256 scalar/ decoder transitions. Raw download,
header-independent projections and header-independent queries remain
available. No runtime timing claim is made.

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
endings and one final LF, has the license digest above. M06 may extract it as
license.txt and must retain it alongside generated material.

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
