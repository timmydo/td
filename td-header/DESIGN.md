# Bounded header lexical primitives

This std-only crate owns resident lexical syntax, not protocol or admission
policy. It has no dependencies, clock, crypto, I/O or source ownership. The
enclosing grammar supplies a complete immutable field-value or
parameter-name slice, any required exact start offset, and permission for
the lexical token at that location.

cfws scans optional comments and folding whitespace, leaves the first
non-CFWS byte untouched, and returns original top-level comment extents.
Comments nest to depth 32 including the outer pair. Escapes suppress
delimiter interpretation; UTF-8 is validated without decoding display words.
SP/HTAB, CRLF folds and bare-LF folds followed by SP/HTAB are admitted.
Literal and escaped obsolete ASCII controls are preserved; literal NUL,
invalid/truncated UTF-8, unfinished escapes, unclosed comments and nonfold
line endings inside comments fail. Outside comments, a nonfold ending is
left for the enclosing grammar.

delimited validates one raw quoted string or domain literal, including its
delimiters in the returned extent. It preserves escapes and folds with the
same UTF-8/obsolete-control rules. A literal opening square bracket inside a
domain literal is invalid. Trailing source is left to the caller.

mime_token_octet supplies the shared ASCII MIME-token octet predicate used
by the classifier and enclosing MIME field grammar. It grants no placement
or complete-token authority.

mime_attribute classifies one complete parameter-name slice. It preserves a
base-name extent and ordinary, single extended, numbered section or
malformed form. Ordinary names use ASCII MIME-token syntax; extended bases
additionally exclude apostrophe and percent. Section numbers use checked u64
arithmetic, start with a nonzero digit or the single digit zero, and may end
in one star for encoding. It validates spelling only: no gaps/duplicate
checks, segment assembly, charset/language or percent decoding, selection or
fallback occurs. Malformed form retains the base extent for the caller's
candidate rejection. No name is normalized or granted filesystem/path
authority.

The classifier scans at most 32 octets per turn with 32 source visits and 32
records; EOF is a charged transition. These bounds fit the shared maximum
below. It owns no buffer or collection. Generic state size depends on E;
with the fixed mail error, its bound is recorded below. Shared work failure
remains sticky across replacement callbacks; completion is cached and inert.
Explicit check_work calls the caller's live admission with a zero-count
charge; a refusal retires cached classification and latches in the same
owner. The enclosing caller owns complete-field validation and binds fresh
admission.

None of these helpers discovers raw-message field boundaries, selects
fields, decodes encoded words or charsets, unquotes output, normalizes
Unicode, assembles addresses/parameters, creates protocol output or grants
identity. Provisional comment events retire on any later failure; lexical
Complete validates only the helper's scope, not its enclosing field.

Work::charge runs before each access or transition. Charge reports visits
and records separately, including lead-byte rereads and failed fold
lookahead. Each poll performs at most 32 transitions, 160 source-byte visits
and 32 record charges, emits no output bytes and retains no growing buffer
or collection. The caller implements the bounded callback and binds current
clock, cancellation and aggregate/job budgets to it. No replacement callback
can revive a failed cursor; Error<E> retains the original Copy error. Cached
completion performs no new work. Live final admission belongs to the caller.
Generic cursor size depends on E; with td-mta's fixed errors CFWS and
delimited cursors remain within 64 bytes, and the parameter-name cursor
within 128 bytes. No Clone or Copy implementation permits duplicating live
lexical state. Constructor allocation and complete-field authorization are
external.

Mail wrappers preserve their public errors and original budget/clock
binding. The source grammars migrate atomically: no old parser remains
alongside the shared implementation. Shared fixtures exercise
lexical/refusal contracts; mail tests and allocation probes retain consumer
composition qualification. These bounds do not establish total worker/native
memory or scheduler latency.
