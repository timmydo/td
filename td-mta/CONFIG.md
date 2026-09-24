# td-mta configuration

## Scope

This document owns configuration syntax and its bounded parsing helpers.
`config::syntax` implements framing and statement decoding; `config::stream`
drives a trusted reader through EOF. The resource stanza schema below
additionally builds checked resource plans. [SCHEMA.md](SCHEMA.md) specifies
the remaining complete operator schema and
snapshot partitions. Its loader, protected file access, effective output and
CLI remain M04b2c3/M04b3/M05/M19 work as assigned in IMPLEMENTATION.md.
A syntactically accepted statement is not a valid service configuration.
DESIGN.md §6 owns the administration contract; RESOURCES.md owns the aggregate
memory budget.

## Physical input and limits

Input is UTF-8, with LF or CRLF line endings, including mixed endings. A final
unterminated line is allowed. A bare CR is invalid. Empty input is
syntactically valid; the schema must still require its version and mandatory
settings. A UTF-8 BOM prefix is refused. Outside quoted strings, whitespace
means ASCII space or horizontal tab. Every line, including comments, must be
valid UTF-8 and contain no Unicode control character except horizontal tab.
Also reject Unicode line/paragraph separators U+2028/U+2029 and directional
controls U+061C, U+200E/U+200F, U+202A..U+202E and U+2066..U+2069, including
in comments, so visual line/direction changes cannot disguise settings. Quoted
strings also reject raw horizontal tabs. Other Unicode text is retained
without normalization; field-specific validation follows syntax decoding.

All ceilings apply together:

| Quantity | Inclusive ceiling |
| --- | ---: |
| Entire physical input, including comments and line endings | 2,097,152 bytes |
| Physical lines, including empty/comment lines | 65,536 |
| One physical line, including its LF/CRLF if present | 8,192 bytes |
| One decoded string, including a section label | 4,096 UTF-8 bytes |
| One section name or assignment key | 64 ASCII bytes |
| Unsigned integer | 18,446,744,073,709,551,615 |

A terminal LF ends the preceding line; it does not add another empty line.
The first byte exceeding a ceiling refuses the input. A heavily escaped
4,096-byte decoded string may not fit a physical line with its key and quotes;
the line ceiling still applies. These are parser ceilings, not permission to
exceed the snapshot arena or descriptor counts. The future schema may impose
smaller field limits.

## Literal grammar

Each physical line has exactly one of these forms, surrounded by optional
space/tab:

```text
# comment
[section]
[section "label"]
key = "text"
key = 123
key = true
key = false
```

These are syntax examples, not a service configuration or a list of supported
fields. Blank lines are accepted. Names match `[a-z][a-z0-9_]*`. No whitespace
is permitted between `[` and the section name. A label requires at least one
space/tab after the name. Space/tab before `]` is optional. A comment begins
at `#` outside a string, after a complete statement or where a blank line
could occur; no separating whitespace is required. Non-comment text after a
complete statement is an error. There are no inline statements or semicolons.

Strings use double quotes. Only `\"` and `\\` are escapes; all other backslash
sequences are errors. `#`, `=`, `[` and `]` inside quotes are literal text.
Empty strings and empty labels are syntactically valid. There are no single
quotes, multiline strings, Unicode escape sequences, substitutions, or escape
sequences for controls. Represent Unicode directly as UTF-8.

Integers are canonical unsigned decimal: `0` or a nonzero digit followed by
digits. Leading zeros, signs, separators, fractions, exponents and overflow
are refused. Boolean literals are exactly `true` and `false`. Other bare
words are refused. There are no lists, nested tables, includes, environment
expansion, executable hooks or network input mechanisms.

Syntax decoding preserves statement order and does not track the current
section. The future schema builder owns section association, permitted labels,
duplicate and unknown fields/sections, version checks, required values,
reference resolution and listener/resource policy. It must validate the whole
candidate through confirmed EOF before publication; successfully parsing a
prefix cannot install a configuration.

## Bounded streaming interface

`Framer::new` borrows at least 8,192 initialized bytes and uses only that
prefix. It allocates nothing. `feed` consumes input through at most one LF
and returns the consumed prefix length. The caller retains the remainder,
processes `line()`, then calls `advance()` before feeding more. When a line
is ready, `feed` returns zero without consuming more input. `line()` exposes
the one-based physical line number and bytes including the line ending.

After the caller has fed all bytes from the actual input stream, `finish()`
marks EOF and exposes a final unterminated line if any. It is idempotent;
nonempty input after it is an invalid-state error. Process and advance any
remaining line before `is_finished()` becomes true. Calling `advance()` with
no ready line is an error. Framing failures are sticky: subsequent operations
return the first diagnostic and `is_finished()` stays false. The helper
cannot establish that an external reader really reached EOF.

`parse_line` checks one physical line and uses caller-owned decoded-string
scratch. It checks the line limit independently; aggregate byte/line limits
are the framer's responsibility. Section/key names borrow the raw line and
text values/labels borrow scratch. The builder must copy retained values into
the candidate's bounded arena before either buffer is reused. Only one decoded
string is live per statement. Insufficient scratch returns `config_capacity`;
an empty string requires no scratch bytes. No partial statement is returned
on error. Scratch and frame tails are not erased when their visible length
changes, so they are private storage and must never be dumped as diagnostics.

The control worker's existing 64 KiB configuration scratch is divided into 16
KiB input, 8 KiB physical line, 4 KiB decoded string and 36 KiB parser/builder
state. Construction of the candidate uses its already budgeted snapshot; there
is no whole-file input copy or allocated syntax tree. This increment does not
instantiate the control worker, open files, read credentials, or prove
ownership/permissions. M05 supplies the trusted filesystem boundary.

## Reader completion and statement dispatch

M04b2c2 implements `config::stream::read` over a trusted `std::io::Read`
adapter and a statement handler. It uses the first 28 KiB of caller-owned
scratch: 16 KiB input, 8 KiB physical line and 4 KiB decoded string. This is
within the 64 KiB control-worker partition above, leaving 36 KiB for builder
state. Additional supplied scratch is untouched. Insufficient scratch refuses
before calling the reader or handler. The driver allocates nothing; trusted
reader and handler implementations own their own allocation/blocking behavior.

Read each nonempty chunk completely through the framer, parse each complete
line, and pass nonempty statements in order to the handler. Comments and blank
lines count as physical lines but do not call the handler. Borrowed statements
are valid only during that call; retained values must be copied into bounded
candidate storage. The handler must stage changes only. The caller of `read`
must discard that entire candidate whenever `read` returns Err, including
failures after the last successful callback. Successful prefix callbacks never
authorize publication, file writes or listener changes.

Only `Ok(0)` from a read into the nonempty input buffer marks EOF. Short reads
continue; errors, including `UnexpectedEof` and `WouldBlock`, refuse. At EOF,
parse and dispatch any final unterminated line and require the framer to finish
before returning a Summary with consumed bytes, physical lines and nonempty
statement count. Empty input completes syntactically, leaving mandatory schema
requirements to the loader. Summary is an observation, not a publication token
or a proof of schema validity, file identity, permissions or immutable contents.
M05 must supply the actual trusted regular-file adapter; this helper relies on
its reader's truthful EOF and does not open paths itself.

At most 32 Interrupted read errors are retried across the entire operation;
progress does not reset that allowance. The 33rd refuses. Combined with the
input ceiling, there are at most 2097185 reader calls, even with one-byte
progress. This bounds retries/work, not elapsed time inside a blocking reader.
All other read errors stop immediately. A reader reporting more bytes than
its supplied buffer holds is refused. No handler or reader is called after a
failure; syntactic diagnostics retain their original source locations.

Fixed driver codes are `config_stream_capacity`, `config_stream_read`,
`config_stream_interrupted_limit`, `config_stream_invalid_read_count`,
`config_stream_invariant` and `config_stream_handler`. Syntax refusals use the
existing syntax codes. Read failures retain only the fixed ErrorKind, dropping
custom I/O error text. Display reports only the fixed read-error code;
Debug or typed matching additionally reveals its ErrorKind. Default driver
Display/Debug does not print handler errors. The error source chain is empty;
explicit enum matching retains the typed handler error for the trusted schema
caller. This also avoids duplicating a syntax diagnostic in chained reports.
The entire scratch partition, not only unused tails, retains bytes after
success or failure. Keep it private and
never dump it as a diagnostic. Inline secrets are not supported; this driver
does not read protected credential files or claim zeroization.

## Diagnostics and disclosure

`Diagnostic` contains a fixed `Code` and one-based line/byte-column location.
Columns count UTF-8 bytes, not displayed characters. End-of-line errors may
point one byte past the content. Input ceilings point to the first refused
byte; line-limit errors can therefore report column 8,193. UTF-8 validation
precedes syntax decoding and reports the start of the invalid sequence.
A string-length refusal at an escape points to its backslash. Validation
order determines which single error is returned. Locations are intentionally
disclosed; DESIGN.md requires credentials in separate protected files.

Stable syntax codes are `config_capacity`, `config_invalid_state`,
`config_input_too_large`, `config_too_many_lines`, `config_line_too_long`,
`config_invalid_utf8`, `config_control_character`, `config_expected_name`,
`config_name_too_long`, `config_expected_equals`, `config_expected_value`,
`config_invalid_integer`, `config_invalid_escape`,
`config_unterminated_string`, `config_string_too_long`,
`config_expected_bracket`, `config_expected_space`, and
`config_trailing_data`. They are library diagnostics; CLI JSON and exit-code
wiring is still planned.

Display/debug diagnostics never include source bytes. `Statement` and `Value`
implement redacted `Debug`, including names, labels, integers and booleans.
Their typed accessors/patterns intentionally expose data to the trusted
builder; redacted debug output is not an authorization boundary. Never
interpolate a supplied key, raw input line, wrong-type value or referenced
secret into a normal event. Typed numeric resource diagnostics have the
explicit disclosure contract below.

## Resource stanza schema

M04b2a implements resource settings as a separate typed builder. It cannot
validate an entire configuration. The outer snapshot builder must recognize
unlabelled singleton `[limits]`, `[disk]`, `[work]`, `[network]` sections,
reject labels, and establish the schema version and actual EOF. Each may
appear at most once, in any order, and may be omitted to retain defaults.
Within one section, each allowed key occurs at most once. Unknown keys fail;
values must be integer literals, including when a boolean or quoted string
contains something that resembles a number. No unit suffix is accepted.

Fields, default values and range limits derive from the existing declarations
in `Limits`, `DiskLimits`, `WorkLimits` and `NetworkLimits`; they are the same
inputs consumed by RESOURCES.md and ADMISSION.md's planners. The supported v1
key vocabulary is listed below and pinned by an independent test oracle.
Adding a declaration must reconcile that vocabulary and this schema. The
Limits declaration separates configurable values from fixed storage and
execution constants. Fixed journal/frame sizes/counts and the single outbound
worker are not operator keys; spelling them in a file is an unknown-field
error even when the supplied value equals the constant.

### `[limits]`

`smtp_sessions`, `smtp_per_peer`, `https_connections`, `tls_handshakes`,
`event_streams`, `body_jobs`, `storage_views`, `message_bytes`,
`header_bytes`, `mime_depth`, `mime_parts`, `smtp_recipients`, `json_bytes`,
`json_methods`, `json_depth`, `json_tokens`, `objects_per_method`,
`query_page`, `index_cache_bytes`, `journal_bytes`, `upload_disk_bytes`,
`upload_expiry_seconds`, `queue_disk_bytes`, `queue_submissions`,
`sort_disk_bytes`, `log_file_bytes`, `retained_logs`, `memory_budget_bytes`.

### `[disk]`

`body_bytes`, `body_files`, `live_metadata_bytes`, `checkpoint_bytes`,
`response_bytes`, `response_total_bytes`, `cache_bytes`, `cold_bytes`,
`free_bytes`, `free_inodes`.

### `[work]`

`foreground_seconds`, `foreground_io_bytes`, `foreground_records`,
`changes_seconds`, `changes_io_bytes`, `changes_records`, `request_seconds`,
`commit_seconds`, `commit_io_bytes`, `commit_records`, `checkpoint_seconds`,
`checkpoint_io_bytes`, `gc_drain_seconds`, `gc_seconds`, `gc_io_bytes`,
`gc_records`, `gc_unlinks`, `backup_seconds`, `backup_io_bytes`,
`admission_seconds`.

### `[network]`

`handshake_seconds`, `dns_seconds`, `dial_seconds`, `header_idle_seconds`,
`header_seconds`, `body_idle_seconds`, `response_idle_seconds`,
`keepalive_seconds`, `event_stall_seconds`, `smtp_idle_seconds`,
`smtp_data_seconds`, `command_seconds`, `data_init_seconds`,
`data_block_seconds`, `final_reply_seconds`, `migration_idle_seconds`,
`migration_minimum_seconds`, `transfer_minimum_seconds`,
`transfer_base_seconds`, `minimum_rate`.

Quantities ending in `_bytes` are bytes and those ending in `_seconds` are
seconds. Counts/depths are integers; `minimum_rate` is bytes per second. No
unit conversion or multiplication is performed by the stanza decoder.

### Builder and validation

The outer builder calls `begin` for each resource section and supplies its
current typed section explicitly to every `assign`. `Section::ALL` and
`Section::from_name` enumerate/recognize the fixed names. This helper keeps no
second current-section selection. Assignments before `begin` fail. The caller
must route only assignments from the matching current resource stanza. It
passes the assignment's key location; range errors point there, not inside the
value. `begin` rejects repeated sections even if no assignments occurred or
another section intervened. `assign` rejects repeated fields, retaining the
first location for diagnostic context. The first error poisons the candidate;
later `begin`, `assign` and `finish` return it. The outer builder must
likewise discard the whole candidate if any other check fails.

`finish(view_mode)` consumes the builder and runs, in order, the existing
memory plan, disk/work plan and timeout plan. The selected view mode is an
explicit typed argument from the future whole-configuration schema; this
helper cannot infer whether online background operations are enabled. Resource
integers are converted from u64 to usize with a checked conversion;
disk/work/network values remain u64. No integer is truncated. Ranges and
cross-field capacity rules are applied by the planners before `Validated` is
returned. `Validated` contains only the three immutable plans and grants no
filesystem, credential or listener authority.

Resource errors use fixed codes and optional source locations. Duplicate
errors include the original location; range errors identify the fixed schema
field and its assignment when available, using `config_out_of_range`.
Type/width errors also retain the recognized static field name.
Cross-field/budget errors do not blame a single line and have no location.
Typed planner errors are available as the error source; their names/rules are
compiled strings, never supplied key strings or wrong-type values.
Successfully decoded resource integers and computed totals may appear in the
typed planner error, its Debug, or its error source (including a rejected
memory budget). These are numeric administrative diagnostics, never credential
data or raw input echoes. The builder holds at most 64 location slots per
resource section and is bounded to 4 KiB by its size test, inside the existing
36 KiB builder scratch. It allocates no additional storage.

Resource codes are `config_duplicate_section`, `config_duplicate_field`,
`config_unknown_field`, `config_expected_integer`, `config_integer_width`,
`config_no_resource_section`, `config_resource_schema_capacity`,
`config_out_of_range`, `config_resource_plan`, `config_admission_plan`, and
`config_timeout_plan`. These supplement the syntax codes above; CLI output
remains unimplemented.

## Local recipient routing

M04b2b implements a typed routing candidate and immutable lookup view in
`config::routing`. It does not implement the whole stanza dispatcher, SMTP
commands, live reload, authentication or mailbox creation. M04b2c3 must bind
these target routing stanzas to that candidate:

```text
[account "0123456789abcdef0123456789abcdef"]
[domain "example.test"]
[alias "me@example.test"]
account = "0123456789abcdef0123456789abcdef"
```

Account labels and alias targets are canonical AccountId strings, independent
of mail paths. The sole account stanza declares its stable ID. Domain labels
are served DNS names; alias labels are full addresses, and each alias requires
exactly one `account` assignment. No folder/forwarding/catch-all fields exist.
The outer schema rejects absent labels, duplicate or unknown fields and any
second account stanza. Other account/identity fields are specified in SCHEMA.md; these
examples are a routing fragment, not a complete runnable configuration.
The helper accepts typed AccountId values and does not parse those labels.

Exactly one account and at least one served domain are required before a
routing view exists. Account declaration may follow aliases. Every alias must
reference the sole account and a declared domain. Conflicting or redundant
canonical domain/alias declarations are errors, including duplicate aliases
targeting the same account. Ordinary local-part case remains significant;
domain case does not. No routing decision uses a display name or filesystem
path. Alias acceptance does not authorize outbound sending.

### Address spelling and canonical keys

Configured aliases use ASCII SMTP mailbox spelling with a DNS domain, without
angle brackets, source routes, comments or header display-name syntax. Quoted
local parts are decoded before comparison; equivalent spellings within the
length bounds share a key. Local parts retain case except reserved postmaster.
This follows the comparison rules in
[RFC 5321 §4.1.2](https://datatracker.ietf.org/doc/html/rfc5321#section-4.1.2);
this helper is not a complete SMTP command/path parser.

The td-mta limits are 254 bytes for the full supplied address and 64 bytes for
its serialized local part, including any quotes/escapes. The decoded local
part must be nonempty. Unquoted local parts are nonempty dot-separated atoms;
quoted local parts allow printable ASCII and quoted pairs for printable ASCII.
Controls, non-ASCII local parts, empty atoms and malformed quoting are refused.
No trimming or Unicode normalization is performed.

Served domains are nonempty ASCII labels of 1..63 bytes, containing letters,
digits and interior hyphens, separated by dots. Reject empty labels, trailing
dots, leading/trailing hyphens, underscores, address literals and all-digit
final labels. ASCII-fold
to lowercase. The served-domain ceiling is 243 bytes so its mandatory
`postmaster@DOMAIN` address fits the existing 254-byte envelope bound. These
are local configuration constraints, not DNS resolution or ownership proof.

The temporary lookup key is decoded local bytes, one NUL separator and the
folded domain. Persistent cells store the local bytes and a domain index. Input local parts cannot contain NUL, so the separator is
unambiguous even when a quoted local part contains `@`. This byte key is
private metadata, not an SMTP address or a string to display verbatim.
Reserved postmaster local parts are folded before duplicate detection too.
Keep the original accepted SMTP envelope separately when receipt is implemented;
the routing key does not replace that inspection record.

### Postmaster and refusal behavior

Every served domain routes any case of postmaster to the sole account, and
the domainless `Postmaster` form does so as well. These routes are implicit
and cannot be disabled. An explicit postmaster alias is permitted but must
pass the same domain/account checks; it adds no forwarding authority.
Quoted equivalent local forms with a served domain receive the same reserved
handling. The domainless exception is only the unquoted `Postmaster` token
(case-insensitive), as in RFC 5321 §4.1.1.3; quoted domainless forms do not
resolve. No other domainless address resolves. The command parser owns
angle-bracket removal and validates the full SMTP command before lookup.

All other addresses require an exact canonical alias. A `+` is an ordinary
literal local-part character: it matches only when that exact alias exists.
No implicit tag stripping, catch-all, forwarding or nonlocal delivery occurs.
`resolve` returns no route for unsupported, malformed or unconfigured spellings;
this is not an SMTP syntax verdict. The protocol layer owns its reply codes.
Lookup returns only AccountId. Inbox provisioning, durable delivery, envelope
preservation and pinning this configuration for a transaction remain later
milestones; v1 delivery still files once in the account's Inbox.

### Storage and construction

The caller supplies initialized storage: at most 256 domain cells of 16 bytes,
4096 alias cells of 32 bytes, and 320 KiB routing text. The text partition is
within the existing 512 KiB snapshot text/secret arena, leaving 192 KiB for
other configured text and decoded credential material. It is not an extra
allocation. The 4 KiB domain table comes from the snapshot's 384 KiB descriptor
region; the alias table uses its existing 128 KiB reservation. Smaller caller
regions are allowed (including zero alias cells), but at least one domain
cell and one text byte are required. No operation grows these regions.
The conservative text bound is 256 × 243 + 4096 × 64 = 324352 bytes, below
320 KiB. All count and string limits therefore fit the full reservation;
smaller supplied regions can exhaust text before cell counts.

Each domain is stored once. An alias cell holds an eight-byte local-text
reference, its 16-byte target AccountId, four-byte line, two-byte column and
one-byte domain index, with padding within 32 bytes. A domain cell holds an
eight-byte text reference, line, column, declaration flag and original index,
within 16 bytes. Cells have opaque fields and an EMPTY initializer. Only the
builder's used prefixes are live; reusing an arena cannot reactivate old
cells. The constructor checks layout and storage ceilings before mutation.
Text references stay private to the builder/view and cannot cross arenas.

Construction interns canonical domains with a bounded linear search of at
most 256 cells, including domains first mentioned by aliases. A pending
reference consumes a domain cell but does not make the domain served: an
explicit domain declaration is still required. Aliases and declarations can
arrive in either order. Repeated domain declarations fail immediately;
repeated alias keys fail at finish. No operation allocates.

The first insertion failure poisons the candidate; further methods and finish
return it. Finishing consumes the builder, validates references and targets,
sorts the domain table in place, remaps alias indices through 256 bytes of
stack scratch, then sorts aliases by domain index and local bytes and checks
duplicates. No view is returned on failure. Immutable borrows of the used
regions exclude rebuilding in the same storage. Runtime generation lifetime
and publication remain M19.

Lookup uses at most 254 bytes of stack key scratch plus checked binary
searches of domains and aliases; it neither allocates nor scans the full
alias table. Private offsets are checked before reads. Lookup does not mutate
counters or invoke files/network services. No normal Debug output exposes
routing text. Fixed errors carry source locations without supplied names or
addresses. Domain duplicates report the earlier declaration; alias duplicates
report the lower source coordinate as the previous site. If a typed caller
supplies coincident coordinates, both reported sites are equal. Private text
offsets break alias sorting ties after source coordinates without extra cell
metadata; the parser has no includes or synthetic macro locations. Missing
mandatory declarations have no source location. Buffer tails are private and
not erased; no zeroization claim is made.

Stable routing codes are `config_route_capacity`, `config_route_invalid_domain`,
`config_route_invalid_address`, `config_route_second_account`,
`config_route_missing_account`, `config_route_missing_domain`,
`config_route_duplicate_domain`, `config_route_duplicate_alias`,
`config_route_unknown_domain`, `config_route_unknown_account`, and
`config_route_invariant`.

## Visible identity preimages

`config::identity` supplies the bounded canonical encoder specified in
[API.md](API.md), preserving all visible identity properties for the eventual
configuration fingerprint. It borrows validated text and requires sorted,
unique identity IDs. This helper does not bind identity stanzas, materialize
defaults, authorize sending or hash/publish state. The complete schema loader
remains M04b2c3; the crypto provider and JMAP integration remain M07/M15.

## Common scalar values

M04b2c3a1 implements `config::values`, the shared scalar grammar for later
stanza builders. It validates profile names, configured DNS names, derived
certificate names and lexical absolute paths against SCHEMA.md. It does no
filesystem access, DNS lookup, reference resolution or authorization. Profile
names are at most 64 bytes, ordinary endpoint/routing DNS names 243 bytes,
derived certificate names 253 bytes, and paths 4095 UTF-8 bytes. The source
parser still owns control-character restrictions; path validation alone is
not proof of safe operator-file bytes or protected ancestors.

`paths_overlap` first validates both paths, then compares whole components
including equality; `/a` overlaps `/a/b`, not `/a-other`. Root `/` overlaps
any absolute path. Symlink aliases and descriptor identity remain M05 checks.

DNS helpers only validate; they do not return folded storage. `config::text`
owns shared lowercase copying, and callers must fold
names before canonical storage or compare with ASCII case folding. Both DNS
ceilings intentionally return `config_value_dns_name`; the outer loader adds
static field/role context when reporting a complete schema error.

`mailbox_key` applies the same 243-byte domain ceiling to every configured
mailbox, including identities, reply-to/BCC entries and ACME contacts. It
reuses the routing mailbox grammar and writes into a caller's
254-byte array. The returned prefix is decoded local bytes, a NUL separator,
and the lowercase ASCII domain. Local case is preserved, including postmaster.
Only inbound routing applies its reserved case-insensitive postmaster rule;
from-address authorization must not inherit that exception. This key is not an
SMTP command/path parser, a header address parser, or permission to send. A
syntactically valid star local part remains representable here; the identity
builder separately rejects wildcard sending identities as SCHEMA.md requires.
Failed parsing may leave partial bytes; only a successful returned prefix is
valid, and no operation scrubs the caller's backing storage.

The helpers allocate nothing and return fixed codes `config_value_profile_name`,
`config_value_dns_name`, `config_value_absolute_path` and
`config_value_mailbox`. `Code::at` attaches a caller-supplied physical location:
the whole loader uses assignment `value_location`, or the labelled section's
location for label failures. It does not fabricate an internal byte offset.
This lower-level diagnostic does not carry a field name; the whole loader
wraps it with a closed static field/role identifier to satisfy SCHEMA.md.
Diagnostics contain no input text and have an empty error-source chain.
Unknown fields, duplicate references and snapshot text ownership remain
later M04b2c3 work; these helpers cannot produce a complete
validated configuration.

## Numeric endpoints and HTTPS values

M04b2c3a2 implements `config::endpoint`. These are bounded, allocation-free
value helpers, not a resolver, listener, TLS verifier or complete schema loader.
They use the common DNS grammar above and SCHEMA.md's HTTPS authority profile.
They do not normalize arbitrary URLs or interpret them through another URL
library. Fixed error codes are `config_endpoint_origin`, `config_endpoint_uri`,
`config_endpoint_address` and `config_endpoint_prefix`. The whole loader adds
static field/role context and physical locations; this mapping remains
unimplemented until M04b2c3d. Errors have no source chain.

`HttpsUri::parse` accepts at most 4096 ASCII bytes: lowercase `https://`, a
243-byte DNS host, optional shortest decimal port 1..65535, and a bounded
RFC 3986 path/query with checked percent triplets. It refuses userinfo,
fragments, backslashes, controls, spaces, non-ASCII URI text, address literals,
and the hex-number final labels excluded by SCHEMA.md. The original URI and
host spelling remain borrowed unchanged; `same_origin` compares host with ASCII
case folding and effective port (443 when absent). Preserve `raw()` for consumers
such as JWS. `write_request_target` preserves path/query bytes and supplies `/`
for an empty path, including before a query. It never percent-decodes or changes
escape case, and retains literal or percent-encoded dot segments verbatim.
No path authorization follows from this helper. Same-origin validation is a comparison helper, not proof of a CA's
authority or permission to follow an endpoint.

`Origin::parse` further allows only an absent path or one terminal `/`, with
no query. Its canonical writer emits lowercase host, omits port 443 and omits
the terminal slash. `HttpsUri::origin` exposes its origin, and both types
share `Origin::same_origin` for comparisons. The host accessor still returns the original borrowed case.
Both writers append to `bounded::TextBuffer` atomically: a capacity failure
restores visible length, not overwritten tail bytes. Their private formatting
implementations do bounded work and allocate nothing.

`numeric_endpoint` parses at most 53 bytes into a std `SocketAddr`, rejects
zero/noncanonical ports, mapped IPv6 and every zone spelling (including `%0`).
IPv6 uses brackets. No bind-family policy or network action follows from that
value; SCHEMA.md's IPv6 runtime adapter gate remains in force.

`Prefix::parse` accepts at most 49 bytes of numeric CIDR text, requires zero
host bits and shortest decimal lengths 1..32 or 1..128, and refuses mapped
IPv6 prefixes. It retains a binary network and prefix length; equivalent IPv6
spellings compare equal. Membership maps mapped-IPv6 socket peers to IPv4
before checking, so only an IPv4 prefix can admit them. Receipt storage must
still retain the original socket peer. No prefix alone proves gateway authority;
current policy, TLS identity and local-recipient checks remain required.

URI/origin/prefix Debug output is redacted. Explicit value accessors and std
socket-address results expose their values to trusted callers; they are not
safe logging substitutes. Persistent compact cells, combined arena ownership,
reference binding and runtime publication remain later M04 work.


## Bounded configuration text storage

M04b2c3a3 implements `config::text` as a caller-backed byte builder and
immutable borrowed view. The caller supplies at most 192 KiB for non-routing
text and decoded credentials, within the existing 512 KiB snapshot text
reservation. Routing retains its separate 320 KiB partition. Smaller regions,
including empty storage, are valid; no operation allocates or grows them.
This helper does not own an entire snapshot, read files, check the schema or
publish configuration.

An append either copies the whole value and advances the written prefix, or
leaves both the prefix and all backing bytes unchanged. Empty values consume
no text bytes. Raw append accepts arbitrary bytes; `text` separately checks
UTF-8 for the selected value. `append_dns` and `append_certificate_name` use
the shared 243/253-byte grammars and copy lowercase ASCII, without retaining
a second canonical string. Raw append performs no normalization or secret
validation.

Offsets and lengths are private checked `u32` pairs, eight bytes total.
Public opaque handles add a process-local ownership ticket, for a total of
16 bytes. These handles are transient API references, not the compact fields
of persisted/configured descriptor cells. The remaining typed snapshot
builders must keep eight-byte spans private under their enclosing arena
owner; they must not charge 16-byte handles as eight-byte fields or serialize
the process-local ticket. Typed cell construction and whole-snapshot ownership
are implemented for account/identity/address tables by M04b2c3b1 below.
`text.rs` now has config-private span conversion and reads. Each typed table
must bind to its arena owner and verify that owner before converting handles
or reading compact spans. Raw span fields remain unavailable to external
callers; combined snapshot ownership remains M04b2c3d.

A constructor makes one atomic attempt to claim a never-reused ownership
number; it returns `config_text_contended` if another constructor races it.
The single control worker constructs candidates; another caller must treat
contention as a retryable operational condition, not an invalid configuration.
It may retry in a later bounded step. Unit tests that construct builders share
`text::TEST_CONSTRUCTION_LOCK`; separate integration-test processes have
independent issuers. Never reset the issuer to isolate a test. Exhaustion
refuses new builders before counter wrap. Relaxed atomics provide uniqueness only, not
synchronization of text access. Every read checks owner identity and the
written prefix before exposing bytes, including empty values. Dropping and
rebuilding on the same backing memory does not revive old handles. Private
range checks also reject overflow and slices beyond the written prefix.

`freeze` consumes the builder and borrows only its written prefix immutably.
There is no reset or hidden allocation, and no lifetime can outlast the
caller's backing storage. Builder/view/handle Debug output is redacted.
Errors are fixed `config_text_capacity`, `config_text_reference`,
`config_text_utf8`, `config_text_dns_name`, `config_text_owner_exhausted` or
`config_text_contended`, with empty source chains. `config_text_dns_name`
serves both DNS-host and derived certificate-name validation. Static schema
field/role and source-location wrapping remain the complete loader's responsibility.
Explicit read accessors reveal bytes to trusted callers; neither dropping nor
freezing scrubs storage. Protected secret loading and erasure belong to the
owning candidate/runtime work, not this borrowed helper.


## Structural sending-identity records

M04b2c3b1 implements `config::identities` for complete typed account,
identity and address inputs. The whole-loader dispatcher still owns
section names, field types, duplicate fields, required fields and actual
reader EOF. These records grant no SMTP/JMAP sending permission and do
not materialize signatures or produce the identity preimage.

The caller provides 1..64 opaque identity cells of at most 128 bytes,
0..2048 opaque address cells of at most 32 bytes, and the shared
non-routing text builder. Compile-time checks pin the layouts and
address-index sentinel. Identity layout reserves two additional
eight-byte signature spans inside the existing 128-byte ceiling for
M04b3. Construction checks caller capacities against the existing 8/64
KiB metadata reservations. There is no second text arena or address
vector. The enclosing table binds to the text builder's process-local
owner; every mutation verifies that owner. Only config-private checked
conversion strips a handle into an eight-byte span. Completed records
likewise require a matching arena. `view_live` borrows a live text
builder immutably, allowing path inspection before protected loading;
when those borrowed values are no longer used, appends can continue.
`view` accepts a frozen text view. Each compact read rechecks the table
owner inside `text.rs`; spans and their fields remain unavailable to
public callers.

Account input validates the case-sensitive visible ASCII username
excluding colon, its 254-byte ceiling, and the 4096-byte display-name
ceiling. Require exactly one account. Identity inputs carry explicit
defaults for name/list selectors and optional absolute signature paths.
Mailboxes use the shared syntax and size limits while preserving their
configured visible spelling. Only sending identities reject a decoded
local part exactly `*`; reply-to/BCC entries use ordinary mailbox
syntax. A signature path is retained verbatim: existence, ownership,
permissions, content and materialization belong to M04b3/M05. An absent
path stays distinguishable from a pending file reference.

Address rows can precede their identity. The builder interns at most 64
identity IDs, including undeclared forward targets, and maintains two
bounded linked lists per identity. Each list has at most 16 rows.
Address cells retain name/email spans, a next index, name presence and
source coordinates. Null names and empty names remain distinct;
duplicate visible addresses remain separate ordered rows. Identity
sorting moves list heads with the identity; address indices and list
order do not change. A true selector with no rows produces an empty
iterator; a false selector produces null and rejects rows.

Finalization requires a declared account, at least one identity, no
undeclared forward targets, matching account references and consistent
list selectors. It sorts identities by raw ID and returns structural
records retaining a private exclusive borrow of identity cells. Public
views borrow those records read-only. M04b3 consumes the records and
fills the two reserved signature spans after protected reads; it must
not mutate a published snapshot or an outstanding read view. Duplicate
account/identity declarations format both current and prior source
locations. Capacity errors caused while undeclared forward targets
occupy slots also identify an earlier unresolved reference. Fixed error
codes use the `config_identity_` prefix and reveal no supplied text. The
typed builder attaches stanza coordinates; the whole dispatcher must
supply field-specific validation and error context where available. This
is not EOF or protected-file validation.

The first mutation failure is sticky; later mutations and finalization
return it. A multi-field operation may have appended an earlier field
before text capacity runs out. Those bytes remain charged, but no
successful records can be returned from that failed builder.
Whole-loader failure must discard its entire candidate. An individual
text append retains its own atomic failure contract.
Builders/views/input wrappers redact Debug output and do not scrub
backing bytes; explicit account/identity/address accessors reveal values
to trusted callers.

Views provide checked index access and bounded, fallible, fused address
iterators. They do not promise an exact item count on an internal error.
Only the used cell prefixes are live. M04b3 must build the existing
encoder's borrowed arrays in its reserved stack workspace after
protected signature loading; this increment adds no duplicate array or
preimage buffer. Domain policies are described below; combined ownership
and publication remain M04b2c3d/M19.


## Domain policies bound to local routing

M04b2c3b2 implements `config::policy`, a typed wrapper that owns a fresh
routing builder and its policy cells. It forwards account/alias
declarations and binds each domain policy to the exact routing insertion
index. No API accepts an unrelated routing table or a caller-supplied
domain index. Aliases can still precede domain declarations. Routing
keeps each canonical domain name once; policy cells do not duplicate
those names into non-routing text.

The caller supplies routing's existing text/domain/alias regions, at
most 256 policy cells, and the shared non-routing text builder. The
policy-cell layout is checked at compile time against its 64-byte
ceiling, within the existing 16 KiB metadata reservation. Constructor
checks refuse empty or oversized policy-cell regions, then reset each
cell's current-build presence flag. Old field bytes may remain in unused
cells; only a successful current declaration marks a cell present. A
smaller policy region can exhaust before routing does; that failure
invalidates the candidate.

Complete typed policy inputs carry optional MX hostname, preference,
MTA-STS mode, max age and optional certificate profile. `Input::default`
supplies SCHEMA.md's absent MX, preference 10, mode off, age 86400 and
absent certificate. Explicit MX names use the shared DNS grammar and
lowercase copying. Preference must fit 0..65535; max age must fit
0..31557600, including while mode is off. A syntactically valid
certificate profile is required exactly when mode is testing, enforce or
none, and forbidden when off. None and off remain distinct. The codes
`config_policy_certificate_required`,
`config_policy_certificate_forbidden` and `config_policy_profile`
distinguish missing/forbidden references from profile-name syntax for
field diagnostics. Named `DEFAULT_PREFERENCE` and
`DEFAULT_MAX_AGE_SECONDS` constants are shared by input defaults and
empty cells. Profile existence, certificate verification, port/SNI
compatibility and direct versus gateway MX classification belong to
M04b2c3c3/c4b and M07/M18.

Finalization first validates local routing, then confirms a policy for
every routing domain. It validates and stores one canonical global
server hostname for all default MX values. The whole dispatcher can
stage the at-most-243-byte hostname in its existing builder workspace
until this handoff; other server consumers use the resulting shared
value. M04b2c3d still owns the `[server].hostname` field binding: it
passes that assignment's location and wraps DNS/text errors with its
static server-field context, distinct from a per-domain `mx_host` call.
This helper owns the canonical storage, not the operator field's schema
dispatch. No per-domain default hostname copy is retained. Explicit MX
provenance remains in each policy even when its name equals the global
hostname: later listener checks must preserve SCHEMA.md's
explicit-versus-default distinction.

Routing sorting retains each domain's original insertion index.
Immutable policy access uses that mapping, so policy associations
survive sorting and forward alias interning. The records enclose their
routing table rather than accepting one at read time.
`Routing::domain_name` provides checked canonical name access; the
insertion-index bridge stays private to configuration code.
Local-recipient lookup retains the routing module's existing behavior.

Each operation touching non-routing text verifies its arena owner.
Records accept only an owner-matching live borrowed or frozen text view,
and every compact read rechecks ownership inside `text.rs`. Live views
permit later protected-input appends after their borrowed values are no
longer used. A first mutation failure is sticky across
domain/account/alias calls and finalization. Finalization errors return
no records. Fixed `config_policy_` codes, nested fixed routing codes and
source coordinates never echo supplied values; duplicate declarations
retain both coordinates. Debug output for policy
inputs/builders/records/views is redacted. Explicit accessors remain
trusted-caller data.

This increment does not parse complete configuration files, resolve
certificate references, generate DNS or MTA-STS bodies/IDs, serve HTTPS,
or publish a runtime snapshot. Those consumers must pass the remaining
schema, protected-input and provider checks before any externally
visible action.


## Structural resolver and relay records

M04b2c3c1 implements `config::outbound` for complete typed inputs. It retains
one to four explicitly configured numeric DNS destinations in stanza order,
with unique profile names and binary endpoint values. SCHEMA.md's destination
restrictions supplement the general socket parser; no ambient resolver lookup
or reachability check occurs. Exactly one relay is required, with lowercase
DNS hostname, port 1..65535, mandatory password path and optional CA path.
Relay usernames preserve case and printable ASCII spaces/colons, unlike
account usernames; the limit is 254 bytes. Transport is either implicit TLS
or required STARTTLS. `Transport::name` supplies the canonical schema spelling.
No plaintext/downgrade option exists.

Resolver and relay cells are inline in their records, with a compile-time
1 KiB ceiling charged to global settings/headroom. The builder has a separate
1 KiB ceiling within existing builder workspace. SCHEMA.md budgets the pending
relay stanza's retained inputs within that same workspace. Every string uses
the shared non-routing text builder. Constructors bind its owner, mutations
verify it, and compact reads recheck it inside the text module. Live views
allow path inspection before later protected-input appends; frozen views
require the same owner. Resolved protected inputs and runtime publication
remain later integrations.

The first mutation error poisons subsequent calls and finalization. Field
validation precedes text writes; a later capacity failure may retain earlier
appended fields, but the failed builder can never produce records. Missing
resolvers or relay refuse finalization. Duplicate declarations/endpoints
retain both coordinates, including when the table is already full. Checking
at most four existing cells preserves that more specific diagnostic before
reporting capacity. Successful resolver and relay views retain their stanza
coordinates for downstream diagnostics.

Errors have fixed `config_outbound_` codes and an empty source chain. Distinct
`config_outbound_password_path` and `config_outbound_ca_path` codes identify
lexical path failures. M04b2c3d adds static field context and assignment
coordinates; these typed helpers receive stanza locations. Inputs, builders,
records and views redact Debug output. Explicit accessors expose data only to
trusted callers. No helper reads a protected file, resolves DNS, authenticates
to a relay or verifies a TLS peer; these records are structural candidates.


## Structural gateway policy records

M04b2c3c2 implements `config::gateway`, using at most 16 caller-owned gateway
cells of 256 bytes and 128 peer-prefix cells of 32 bytes, within the existing
4 KiB reservations for each. Compile-time layout checks enforce those ceilings.
Smaller regions, including zero cells for a configuration with no gateways,
are accepted; overflow refuses without growing either region. Text uses the
existing non-routing arena. The builder interns at most 16 profile names,
including forward peer targets, and retains first-reference/declaration
coordinates. Each newly interned cell is fully initialized for this build;
only used prefixes are readable when storage is reused.

Complete gateway inputs validate the lexical private-CA path and current leaf
SHA-256 pin, plus an optional different rotation pin. Pins decode exactly 64
lowercase hexadecimal characters into 32 bytes; their type does not assert
that any certificate has been hashed or verified. No protected file is opened.
Profile names are stored once. Each gateway retains at most eight indices into
the common prefix table, preserving peer stanza order and gateway association.
CIDRs use the shared binary prefix parser; equivalent IPv6 spellings within a
policy are duplicates with both coordinates. Overlapping prefixes are allowed,
and different policies may use the same prefix. Unused peer-index entries hold
an out-of-range sentinel, so an internal count error cannot silently select
another policy's first prefix. Public access still checks the used count.

Finalization rejects any undeclared forward target. Declared, unused policies
may have no peers; the listener graph in M04b2c3c4 must require at least one
peer for each consumed gateway. Indexed gateway/peer access is checked and
returns no item past the used prefix. Neither pin equality nor prefix
membership creates gateway authority: current configured policy, private
chain trust, validity, client-auth usage and leaf-pin verification are all
required by M07/M12 before a verified gateway identity exists.

Mutations bind to the shared text owner, and immutable live/frozen views
require that same owner. Each compact text read rechecks it. Any mutation
failure is sticky, including partial text exhaustion; subsequent calls and
finalization return the first error. Profile syntax is checked first, then a
repeated gateway declaration, then its fields. Peer input checks the profile
before the CIDR. Duplicate-prefix checks precede capacity checks to retain the
previous entry's coordinate even when a table is full. A new forward target
may consume its bounded name/cell before discovering peer capacity exhaustion;
the entire failed candidate must be discarded. Fixed `config_gateway_` codes carry
coordinates, duplicate prior coordinates and no input bytes, with an empty
source chain. Inputs/builders/records/views and pins redact Debug output;
explicit accessors are trusted data. Backing bytes are not scrubbed. Whole
stanza dispatch, field diagnostics, EOF, protected inputs and runtime
publication remain later milestones.


## Structural certificate and ACME records

M04b2c3c3 implements `config::certificate` for complete typed certificate
profiles and optional ACME settings. The caller supplies 1..16 cells of at
most 128 bytes in the existing 2 KiB profile reservation. The complete
borrowed records header, including the ACME singleton, fits 128 bytes of
global settings/headroom; the builder fits 256 bytes of existing builder
workspace. These are compile-time ceilings. All text uses the shared 192 KiB
region. Records hold references to operator paths, never raw keys, chains,
parsed roots or provider objects; those belong to the separate
certificate-generation budget and protected/provider validation.

A typed profile input is either ACME or files with both chain and key paths.
Paths receive lexical validation only. The future M04b2c3d whole stanza
dispatcher must reject missing or forbidden operator fields before
constructing this typed input. In files mode, missing chain/key fields use
`config_certificate_chain_required` / `config_certificate_key_required`; in
ACME mode, supplied chain/key fields use
`config_certificate_chain_forbidden` / `config_certificate_key_forbidden`.
These four dispatcher codes are specified here but remain unimplemented
until M04b2c3d; it must test every field-presence combination for both
modes. It may never discard a forbidden field during conversion to
`ProfileInput::Acme`. Names are unique profile labels and retain declaration
order. Mode names are the canonical `acme` and `files` spellings. Every
newly used cell is fully written, and unused cells are not exposed after
storage reuse.

ACME settings may precede profile declarations. They require a valid bounded
HTTPS directory URI, SMTP mailbox contact, explicit accepted terms and an
optional lexical CA path. Keep directory URI and contact spelling unchanged;
`HttpsUri` supplies the configured origin. M18 owns mailto percent encoding,
same-origin enforcement for operational URLs, account creation and issuance.
No automatic terms-link fetch or network request occurs here. Finalization
requires at least one profile and an ACME section exactly when an ACME-mode
profile exists. Missing settings report the first ACME profile; an unused
ACME section reports its own coordinate. Files-only profiles need no ACME
settings. With zero profiles, the missing-profile error takes precedence
even when an ACME section exists. Profile consumption, required-name sets,
SNI conflicts and the ACME HTTP-01 requirement use M04b2c3c4b below; c4a
owns listener role fields and the HTTP-01 bind port.

Profile checks run in this order: name syntax, duplicate declaration, path
syntax, then available capacity, before any text writes. This retains the
prior declaration coordinate for a duplicate even on a full table.

Mutations verify arena ownership and retain the first error. A later text
capacity failure may leave earlier appended fields charged, but finalization
cannot return records. Immutable live/frozen views require matching owners;
every compact text read checks ownership again. Indexed access returns no
item past the used prefix. Stanza coordinates remain available in successful
views. Fixed `config_certificate_` codes distinguish chain, key and CA path
errors, directory/contact errors, terms refusal, duplicates and missing
references. They carry no raw values and have an empty source chain. Debug
output is redacted; explicit view accessors expose trusted data. No helper
establishes EOF, trusted file ownership, usable certificate material or
runtime authority.

## Structural listener records

M04b2c3c4a implements `config::listener` for complete typed listener inputs.
The five kinds are direct SMTP, gateway SMTP, HTTPS, HTTP-01 and an explicit
plaintext loopback SMTP fixture. Required and forbidden fields follow
SCHEMA.md's role matrix; a missing field and a supplied forbidden field have
distinct fixed codes and a static field identifier. Direct/gateway server
names receive shared DNS validation and lowercase storage. Certificate and
gateway references receive profile-name validation only; their existence and
compatibility use M04b2c3c4b below. The source dispatcher still owns
unknown fields, duplicates within a stanza, scalar types and reader EOF.

Each bind is a parsed numeric socket endpoint. Loopback fixtures require
127.0.0.0/8 or ::1, and HTTP-01 requires port 80. Duplicate listener names
and conflicting binds retain both declaration coordinates. On the same
address family and port, identical addresses or either unspecified address
conflict. Separate IPv4 and IPv6 binds are structurally distinct. This does
not establish IPV6_V6ONLY: runtime must still refuse IPv6 startup until M11
supplies its audited socket adapter, as SCHEMA.md requires. No socket is
opened here.

SMTP roles require nonzero session and per-peer counts; per-peer cannot
exceed the listener session limit. Checked integer conversion prevents
truncation. Finalization checks each against the supplied checked
ResourcePlan, sums all SMTP session limits within the global pool, and
requires at least one SMTP and one HTTPS listener. A sum overrun reports
SessionLimit and the first listener whose addition exceeded the pool. The
explicit loopback fixture counts toward both the SMTP role requirement and
the shared pool; SMTP plus HTTP-01 still fails the HTTPS requirement.
Non-SMTP roles cannot carry those fields. HTTPS and HTTP-01 retain their
existing shared runtime pools; no per-listener pool is allocated.
JMAP-origin ports, ACME HTTP-01 availability, certificate consumers, SNI
names, gateway peer requirements and MX classification use c4b below.

The caller supplies 1..16 cells of at most 128 bytes, within the existing 2
KiB listener partition; the builder fits 128 bytes of builder workspace. The
complete borrowed records header fits 128 bytes of global headroom. These
layout ceilings are checked at compile time. Text uses the shared 192 KiB
region. A pending listener's label, server name, certificate/gateway labels
and bind text total at most 488 bytes, within the existing shared
pending-stanza reservation. Every used cell is fully written; reused cells
beyond the current prefix remain inaccessible. Records bind to the text
owner, and each compact read verifies it through the shared helper. Live
views permit later protected-input appends after borrowed text is released.

Profile/duplicate checks precede cell-capacity refusal and remaining field
validation, which precedes text writes. A capacity error does not imply that
remaining endpoint or role fields have been validated. A later text capacity
error can retain charged partial bytes; all mutation failures are sticky and
prevent finalization. Fixed `config_listener_` codes and Field names
disclose only static schema context and source coordinates, with no
error-source chain. Inputs/builders/records/ views redact Debug output.
Trusted accessors expose declared settings and coordinates; these records
grant no socket, TLS, gateway or SMTP authority.


## Structural reference graph

M04b2c3c4b implements `config::graph::bind`. It consumes the completed
listener, certificate, gateway and domain-policy records, a validated JMAP
origin, its source coordinate and caller-owned binding cells. All four
tables must belong to the supplied shared text arena. This is a structural
graph only: the whole loader must still establish actual EOF and every
mandatory section; protected material, provider verification, DNS
reachability, socket startup and publication remain later work.

Check every listener certificate reference and every gateway reference.
Used gateway policies require at least one allowed peer prefix; unused
staged gateway policies may have none. Every HTTPS listener port matches the
JMAP origin port. Any ACME profile requires an HTTP-01 listener, whose port
80 is already enforced by the listener helper. MTA-STS in testing, enforce
or none mode requires origin port 443 and its selected certificate profile.
The enabled policy hosts use every HTTPS listener's SNI table. If a policy
host equals the JMAP host, its profile must equal each HTTPS listener's
primary profile. Different HTTPS listeners may select different primary
profiles when this creates no conflict within either listener's SNI table.

Without direct SMTP ingress every domain needs an explicit upstream MX.
A default MX must match a direct listener server name. An explicit MX equal
to any hostname advertised here also needs a matching direct listener:
global hostname, direct/gateway server name, JMAP origin or any enabled
MTA-STS host. This prevents gateway-only or HTTPS-only names from being
classified as upstream merely because the MX is explicit. Other explicit
targets remain unverified upstream names. This classification performs no DNS lookup and
creates no gateway trust or forwarding authority.

Derive certificate names from direct/gateway SMTP server names, each HTTPS
listener's JMAP host and each enabled MTA-STS host. Retain lowercase names,
deduplicate within each profile and enforce 32 names per profile, 512 binding
cells overall. At most 272 names can be derived from the current 16
listeners and 256 domains; a compile-time check keeps that maximum within
the conservative 512-cell reservation. Tests fill all 272 derived entries
across 16 profiles and refuse a caller region one cell smaller. Every
declared certificate profile must have a consumer.
The caller may supply fewer cells and receives a capacity error when they
fill. Names are added in listener declaration order then canonical domain
order; a duplicate retains its first consumer's coordinate. Binding views
resolve the profile through the enclosing records, never an unrelated table.
The graph also retains read-only access to the enclosed routing table.

Each binding cell fits 32 bytes in the existing 16 KiB partition. The whole
borrowed graph header, including its four enclosed table headers, fits 1 KiB
of global headroom; it replaces those headers rather than duplicating them.
Canonical origin and required names use the existing shared 192 KiB text.
Required-name copies are charged here even when their consumer spelling is
already stored; there is no unaccounted string pool.
A 253-byte name scratch and 257-byte canonical-origin scratch live in the
existing control-worker stack allowance; no heap allocation is introduced.
M04b2c3d stages the server origin (at most 258 raw bytes, including an
optional final slash) and its field coordinate within the existing builder
workspace, then passes it here for canonical storage. Default port 443 and
the optional slash are omitted and the host is folded; the directory URI's
separate preserved spelling remains unchanged.

Check order is binding-region size, all table owners, listener references
and ports/peers, ACME HTTP-01, each domain's MX then MTA-STS port/reference/
SNI rules, and unused profiles. All run before graph text or binding writes.
Then store the canonical origin and derive names, checking per-profile and
caller-region capacity before each name append. Missing references therefore
precede name-capacity failures; MTA-STS port errors precede its certificate
lookup. An unused profile cannot consume graph text or binding cells.

An error consumes the input records and returns no partial graph. Earlier
text or binding writes may remain charged in the discarded candidate; no
rollback, reusable authority or replacement allocation is implied. Used
binding cells are fully initialized, and only their current prefix is
exposed after reuse. Live/frozen graph views check the arena owner; every
compact read checks it again. Fixed `config_graph_` codes carry only source
coordinates, with a related-setting coordinate where applicable. Display,
Debug and the empty error-source chain reveal no operator values; trusted
accessors expose settings explicitly. The whole dispatcher supplies any
additional static stanza/field context from those coordinates.


## Global options and lexical roots

M04b2c3d1 implements `config::globals` for the remaining typed global
options. Its server input carries online-background planning and optional
IPv4/IPv6 publication hints. The whole dispatcher separately requires and
stages hostname and JMAP origin for their single canonical copies in the
domain-policy and graph records. A successful global-options helper alone
therefore does not establish a complete server stanza, EOF or configuration.

Online-background defaults true and selects the existing OnlineBackground
plan; false selects ForegroundOnly. Numeric address hints are parsed in
their requested family, bounded to 15 IPv4 or 45 IPv6 source bytes. Reject
hostnames, zones, brackets and socket-address syntax. Preserve the binary
address without DNS discovery, public-reachability inference or interface
configuration. Reject unspecified and multicast addresses, IPv4 limited
broadcast, mapped IPv6 and obsolete IPv4-compatible IPv6 spellings. Private
and loopback addresses remain allowed for explicit fixtures, including ::1;
acceptance does not prove public reachability. Destination and bind helpers
retain their own role-specific rules.

Roots default to `/var/lib/td-mta`, `/run/td-mta` and `/var/log/td-mta`.
Each supplied path receives lexical validation, then all three pairs must
be disjoint by slash-delimited components. Equality, either nesting
direction and root `/` overlap fail; `/data` and `/data-other` do not overlap.
Validate every root and all pairs before appending any path. When only one
side of an overlap is supplied, identify that operator field and name the
default root as the related field; when both are supplied, use the fixed
runtime/data, logs/data, logs/runtime pair order. Defaults are
applied only to omitted fields, never to invalid supplied text. They are
appended once when an explicit paths stanza completes or, for an absent
stanza, at finalization. Trusted ancestor and resolved-descriptor checks
remain M05. No path is opened here.

Logging accepts only `info`, `warning` or `error`, defaulting to Info. The
helper accepts an omitted severity for an explicit empty logging section,
retaining its coordinate and duplicate detection. One default constant is
used for absent sections and omitted fields. No raw input or credential
logging switch is introduced. Within this helper, singleton duplication
precedes its own field parsing and retains both declaration coordinates.
The whole dispatcher must reject duplicate singleton headers before staging
any fields, especially hostname/origin, and owns unknown fields, repeated
fields, labels, types and all mandatory server fields. Finalization requires
the server-options input and installs absent path/logging defaults.

All path bytes use the shared non-routing arena; a builder fits 256 bytes
of existing workspace, and records fit 128 bytes of global headroom.
Compile-time guards enforce those ceilings. Server flags, numeric hints,
severity and coordinates are inline. Paths retain compact owner-checked
references. Every mutation verifies the arena owner and makes its first
error sticky; consuming finalization refuses that error. Partial text
exhaustion can charge earlier roots but cannot return records. Live/frozen
views accept only the matching owner and check each compact read again.
Fixed `config_globals_` errors retain only static field names, a related
root field for overlap and source coordinates, with an empty source chain.
Inputs, settings, builders and views redact Debug output. Trusted accessors
expose values explicitly; nothing grants file or publication authority.
