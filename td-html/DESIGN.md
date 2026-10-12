# HTML text rendering and bounded references

The existing resident renderer owns its parsed tree and formatted text. Its
plain and rich output share rendering and preserve the existing layout,
link/image and named-reference conventions. Those whole-document APIs allocate;
they are not mail-service memory-admission interfaces.

`reference::Reference` is a separate bounded streaming interface for decoding
one character reference after the caller has consumed its ampersand. The
resident renderer uses this same decoder; the old resident reference scanner
has been removed. Named-reference lookup and numeric codepoint replacement
share the existing table and implementation.

A Reference contains at most 128 bytes, allocates nothing and retains no
source, clock, work meter or authority. It may be cloned as pure parser
progress. The caller owns source identity, ordered input, work/output admission
and final publication. One poll consumes at most one input scalar or emits
at most one output scalar. Resolution performs at most one table lookup over
at most 32 name bytes over at most 512 table entries per turn; suffix flushing emits one byte per turn.
Long numeric runs saturate the codepoint accumulator without retaining digits.
Unknown names flush their bounded literal prefix and leave following source
characters to the caller. A single reference takes linear work in its input
plus at most a fixed number of name-resolution and flush turns.

Names::Html uses the renderer's complete existing table. Names::Basic admits
only amp, lt, gt, quot, apos and nbsp with the same legacy flags. Names are
case-sensitive. Numeric references use the renderer's HTML replacement for
zero, surrogate/out-of-range values and C1 codepoints. The semicolon may be
omitted after numeric references or a legacy named reference. In Attribute
context, a missing-semicolon legacy name is not decoded when its following
character is ASCII alphanumeric or equals; Text preserves the renderer's
existing longest-legacy-prefix behavior.

The caller retains an unconsumed input/last pair. A consumed last scalar closes
input; pending reference output can be drained with None. A nonfinal None
requests input unless the decoder already has output to drain. Complete can
carry the final output scalar, which must be handled before completion; cached
Complete consumes and emits nothing. Input beyond the reference is left
unconsumed. State refusal is terminal and is not an alternative interpretation
of malformed source; malformed references are ordinary literal text or the
specified replacement character.

This interface does not tokenize tags, sanitize markup, extract searchable
mail text or provide native mailbox access. A composing extraction cursor
must retain those policies and fund every decoder turn under its own original
admission. No td-mta dependency or capability is introduced by this interface.
