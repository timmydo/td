# Bounded header lexical primitives

This std-only crate owns resident lexical syntax, not protocol or admission
policy. It has no dependencies, clock, crypto, I/O or source ownership.
The enclosing grammar supplies a complete immutable field slice, its exact
start offset, and permission for the lexical token at that location.

cfws scans optional comments and folding whitespace, leaves the first
non-CFWS byte untouched, and returns original top-level comment extents.
Comments nest to depth 32 including the outer pair. Escapes suppress delimiter
interpretation; UTF-8 is validated without decoding display words. SP/HTAB,
CRLF folds and bare-LF folds followed by SP/HTAB are admitted. Literal and escaped obsolete ASCII
controls are preserved; literal NUL, invalid/truncated UTF-8, unfinished
escapes, unclosed comments and nonfold line endings inside comments fail.
Outside comments, a nonfold ending is left for the enclosing grammar.

delimited validates one raw quoted string or domain literal, including its
delimiters in the returned extent. It preserves escapes and folds with the
same UTF-8/obsolete-control rules. A literal opening square bracket inside
a domain literal is invalid. Trailing source is left to the caller.

Neither helper discovers raw-message field boundaries, selects fields,
decodes encoded words or charsets, unquotes output, normalizes Unicode,
assembles addresses/parameters, creates protocol output or grants identity.
Provisional comment events retire on any later failure; lexical Complete
validates only the helper's scope, not its enclosing field.

Work::charge runs before each access or transition. Charge reports visits
and records separately, including lead-byte rereads and failed fold lookahead.
Each poll performs at most 32 transitions, 160 source-byte visits and 32
record charges, emits no output and retains no growing buffer or collection.
The caller implements the bounded callback and binds current clock,
cancellation and aggregate/job budgets to it. No replacement callback can
revive a failed cursor; Error<E> retains the original Copy error. Cached
completion performs no new work. Live final admission belongs to the caller.
Generic cursor size depends on E; with td-mta's fixed errors it remains within
64 bytes. No Clone or Copy implementation permits duplicating live lexical
state. Constructor allocation and complete-field authorization are external.

Mail wrappers preserve their public errors and original budget/clock binding.
The source grammars migrate atomically: no old parser remains alongside the
shared implementation. Shared fixtures exercise lexical/refusal contracts;
mail tests and allocation probes retain consumer composition qualification.
These bounds do not establish total worker/native memory or scheduler latency.
