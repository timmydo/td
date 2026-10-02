# td-mta configuration

## Scope

This document owns configuration syntax and its bounded parsing helpers.
`config::syntax` implements framing and statement decoding; `config::stream`
drives a trusted reader through EOF. The resource stanza schema below
additionally builds checked resource plans. [SCHEMA.md](SCHEMA.md) specifies
the complete operator schema and snapshot partitions. `config::load::read`
owns whole-reader structural loading, with the portable stack qualification
below. `config::material` decodes bounded signature and relay-password bytes.
Protected file access, effective output and CLI remain M04b3/M05/M19 work
as assigned in IMPLEMENTATION.md.
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

The default `memory_budget_bytes` is 100663296 (96 MiB). It funds the checked
RESOURCES.md planning ledger, not a measured RSS limit. A smaller explicit
budget is accepted only when the complete configured plan fits it; changing
the budget alone does not change connection counts or the RSS release targets.

### `[disk]`

`body_bytes`, `body_files`, `live_metadata_bytes`, `checkpoint_bytes`,
`response_bytes`, `response_total_bytes`, `cache_bytes`, `cold_bytes`.
These are logical quotas; physical free-space settings are not supported.

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
address-index sentinel. Identity layout includes two resolved eight-byte
signature spans inside the existing 128-byte ceiling; M04b3b2a fills them
only while consuming an unpublished candidate. Construction checks caller
capacities against the existing 8/64
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
views borrow those records read-only. Records may be consumed into the sealed
candidate described below.
M04b3b2a consumes exclusive candidate access and fills those spans after
content decoding. M05 still owes protected-file checks before publication.
It cannot mutate a published snapshot or an outstanding read view. Duplicate
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
region. A pending gateway listener's label, kind, server name,
certificate/gateway labels and bind text total at most 500 bytes, within the existing shared
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

## Pending source stanzas

M04b2c3d2a implements `config::stanza` as the bounded staging part of the
source dispatcher. Its catalog recognizes the fifteen non-resource section
names and delegates the four resource names to the existing resource schema.
Known names, field names, scalar classes, unconditional required fields and
label classes are static. Resource stanzas are refused by the pending-buffer
API so the dispatcher must send them directly to `config::resources`; their
larger field set never acquires a second pending array. Before that handoff,
the dispatcher must explicitly reject a label on each resource header:
the resource builder has no label parameter and cannot perform that check.

A Pending value holds exactly one non-resource stanza. `begin` is permitted
initially or after a successful `finish`; it cannot overwrite an unfinished
stanza. It requires or forbids a label according to section, bounds label
length by its class and copies it. Label content is not yet an ID, profile,
DNS or mailbox proof; typed handoff must validate that content. Assignments
accept only known fields of the catalog's scalar class. They retain both key
and value coordinates, detect duplicates before type checks, and preserve
omitted, false and empty-text distinctions. Text is copied before the physical
line/decoded-string buffer can be reused, and each text value retains the
4096-byte syntax ceiling. No default is applied here.

`finish` checks every unconditional required field, returning a borrowed
stanza with explicit label/field access. An accessor key outside that
section's catalog returns Invariant, so a dispatcher typo cannot masquerade
as an omitted field and select a default. Certificate mode-specific fields,
listener role-specific fields and other semantic/range/reference checks
remain typed dispatch and the existing helpers. A pending stanza is neither
complete configuration validity nor EOF evidence. The whole dispatcher must
still own version=1, singleton-header rejection before staging, typed
handoffs and failure propagation; the whole loader must own actual EOF.

After successful finish, the next begin reuses storage, clears all field
presence and exposes only its new text prefix. Rust borrowing prevents reuse
while a stanza view exists. Calling assign or finish in the wrong phase, or
any label/field/type/presence/capacity failure, retains the first error and
prevents future reuse; discard the candidate. The outer dispatcher must also
discard on a typed handoff/reader error after this helper succeeded. There
is no clear-error or partial-publication API.

The full Pending representation fits the existing shared 13 KiB workspace:
12672 text bytes, eight compact typed cells and header/coordinates. The
largest valid identity needs 12604 text bytes, including both raw IDs,
display name, email and two signature paths; all three root paths need
12285. Maximum counts do not authorize oversized individual values. Invalid
semantic text can exhaust the shared region before later semantic checks;
a capacity failure is not evidence of field validity. Appends check the
entire range before modifying bytes or the used prefix. No heap storage,
whole-file AST or second alias-text arena is introduced. M04b2c3d4a guards
combined named loader state; target peak-stack qualification remains d4b.

Fixed `config_stanza_` errors disclose only static section/field context and
source coordinates. Unknown raw names are never echoed. Duplicate fields
report both key locations; type and text-length errors report the value
location; missing fields report the section location. A begin call made
before finishing reports the incoming section at its supplied coordinate.
Pending buffers, stanza views and entries redact Debug output, and errors
have an empty source chain. Explicit accessors expose trusted pending values. This helper performs no files,
network, crypto or runtime publication.


## Typed source dispatch

M04b2c3d2b implements `config::dispatch`. It accepts borrowed syntax
statements, copies one pending stanza and sends completed inputs to every
typed builder above. The first nonempty statement must be integer
`version = 1`; reject other root fields, repeated version and unknown
sections. All ten singleton headers are checked before staging their fields,
including server hostname/origin. Resource headers explicitly forbid labels
and use their existing direct scalar builder. Other labels receive their
role's ID, profile, DNS or mailbox validation during typed handoff.

The pending catalog enforces unconditional fields, scalar classes and
unknown/duplicate fields. Handoff preserves omitted/empty/false values and
passes every declared optional field to its semantic helper. Certificate
mode conversion first enforces all four prescribed chain/key required and
forbidden codes; it cannot discard forbidden raw fields when constructing a
typed variant. Listener helpers receive all five role-sensitive fields.
References may precede declarations; resolver order remains source order.

`finish_stanzas` consumes the dispatcher and closes supplied statements: it
requires version/server, finishes globals and resource planners using the
selected view mode, then identities, routing/policies, outbound settings,
certificates, gateway peers, listeners and the certificate/name graph. It
returns borrowed `Parsed` records and a matching read-only text view. This
method does not operate a reader and is explicitly not evidence of actual
EOF. M04b2c3d4a calls it only after its owned reader operation reaches
EOF. Protected files, TLS/provider readiness and runtime publication remain
later stages. Public structural records cannot authorize any of them.

The caller supplies every table, both text arenas and one Pending. Dispatch
uses no heap allocation or whole-file AST. It stages only the bounded server
hostname/origin and coordinates until their final canonical copies are
written by policy/graph finalization. A compile-time guard covers the
dispatcher and Pending within the existing 36 KiB workspace; this is not a
peak-stack proof including called frames. The separate 28 KiB stream region
is combined with loader state guards in d4a; peak call-frame measurement
remains d4b. Private owned storage and sealed table headers are described below.

The first accept error is sticky and consuming finish refuses it. Partial
table/text writes remain private and charged until the caller discards the
failed candidate; no rollback or partial result is exposed. Tests retain an
independent previous structural configuration across failed replacements;
active-generation publication tests remain M19.

Errors preserve typed helper causes, including duplicate/related coordinates,
and add static section/field context with value coordinates during handoff
where the failing field is identifiable. Label and declaration errors retain
stanza coordinates. Cross-reference/finalizer errors retain their existing
record coordinates; discarded stanzas are not retained as a source map.
Unknown input names and supplied values are never echoed, error source chains
are empty, and builders/tables/results redact Debug. Explicit record accessors
expose values for trusted consumers.


## Preallocated configuration storage

M04b2c3d3a implements `config::storage::Storage`. Its cold `try_new` creates
two text regions and ten typed table regions at the existing compiled count
ceilings. All vectors are private. Compile-time checks also require every
compiled count/layout to fit its partition; unused space within a partition
is allowed. Each allocation checks count multiplication
and its SCHEMA partition before calling `try_reserve_exact`, checks returned
capacity against that same partition, then initializes cells with their
small empty value. A returned capacity smaller than requested also fails;
initialization cannot silently trigger another allocation. Spare capacity
within a partition is allowed and counted, but only the requested cells are
initialized or lent to builders. Failure drops
partially constructed regions and returns a fixed region/code diagnostic.
There is no large aggregate stack temporary, byte reinterpretation or unsafe.

`allocated_bytes` reports actual vector payload capacities plus the owner
headers. Every region retains its existing ceiling; the owner fits 1 KiB of
global headroom. Device cells remain reserved and unimplemented. This empty
owner count excludes borrowed Parsed headers/plans and allocator overhead; it
does not predict RSS. `Candidate::allocated_bytes` additionally counts the
completed sealed headers and owned resource plans as described below.
An allocator returning vector capacity above its region partition is refused; this post-allocation
check is not a bound on allocator-internal transient storage. The partition
ledger and later whole-process qualification retain their existing roles.

`builder` lends all regions exclusively to the existing typed dispatcher and
uses caller-owned Pending scratch. Building, finalizing and querying retain
those allocations. A completed Parsed result still borrows the storage; drop
all builders/results/views before rebuilding or moving its owner. Failed
candidates expose no records; a fresh build obtains a new text ownership
ticket and initializes its used prefixes, so old rows/bytes cannot be
observed through the new result. Unused backing bytes are private, not
scrubbed or exposed as diagnostics. This is storage reuse after discarding
a failed candidate, not recovery of that failed builder.

Tests check all table counts, capacity accounting, allocation/layout refusal,
unchanged allocation addresses across success/failure/reuse, shrinking every
typed table and text prefix, moving unused storage, and an independent prior configuration
remaining readable after replacement failure. Pointer/capacity checks establish
region reuse; they do not replace later whole-process allocation measurement.
The following section specifies sealed headers and movable candidates; d4 owns
actual reader EOF, concurrent workspace and call-frame checks. No service,
protected-file, provider or publication authority is introduced.


## Owning structural candidates

M04b2c3d3b adds `storage::Candidate` and private sealed headers.
`Storage::build_stanzas` consumes its preallocated owner and borrows caller
Pending scratch. The supplied callback receives an owned restricted
`Statements` sink with only `accept`; it cannot replace or extract the
underlying dispatcher. This preserves the association between the builder,
its storage, and the headers later sealed from it. The callback must propagate
its input errors. A recorded accept failure takes precedence over the
callback error and retains the first typed configuration diagnostic. With no
recorded accept failure, the callback error remains Input. Ignored accept
failures still poison finalization.

After callback success, the owner finalizes all supplied statements and
consumes the borrowed Parsed result. Routing, identities, policies, listeners,
certificates, gateways and bindings retain private used counts and inline
settings; text retains its original owner ticket and used prefix. Global and
outbound records and resource plans move directly into the header aggregate.
Every table borrow ends before the backing owner moves. Header constructors
and checked reopening stay private to configuration modules; there is no
public detached-header or arbitrary rebinding API. No self-reference, unsafe
conversion, new allocation or complete snapshot copy is introduced.

Identity, outbound and global accessors return read-only views borrowing the
candidate. `with_graph` reconstructs only the bounded borrowed graph header
on the stack, validates its text owner, and invokes a caller callback; a
graph view cannot outlive that
callback. The same closure can use local routes and nested graph accessors.
Compiler rejection examples pin graph-view escape, consuming an owner while
an identity view remains in use, and the private statement sink. Each requires
its specific Rust error code so an unrelated linker failure cannot pass.
Every reopened slice checks its
sealed count, and text-backed views continue to enforce the original owner.

A callback or configuration failure returns `Failed<E>`, containing the
preallocated storage and its error, with no validated headers. `into_parts`
recovers that storage for another build. Input errors have redacted Debug,
Display and source chains; matching the public Input variant explicitly
exposes the trusted caller's original error. Configuration errors retain their
fixed diagnostics. Returning the storage inline has one documented
`result_large_err` Clippy allowance; boxing the failure would allocate on
the error path. Generic input error storage belongs to the caller's bounded
loader/workspace contract, not the snapshot payload ledger.

`Candidate::into_storage` consumes all completed headers and recovers the
regions. Rust borrowing prevents this while a read view exists. Reuse creates
a new text ticket and new used prefixes; old private bytes remain unscrubbed
as specified above. Tests move completed candidates, compare allocation
addresses, rebuild with smaller configurations, exercise input/finalizer and
sticky errors, reject internal owner/count mismatch, and retain an independent
prior candidate across replacement failure. No old generation is mutated.

Complete sealed headers fit 4 KiB; the graph subheader fits 1 KiB and identity
subheader 128 bytes, within global headroom. Resource plans independently fit
their 4 KiB partition. Candidate inline ownership plus headers fits global
headroom plus the plans partition. `allocated_bytes` counts the actual table
capacities plus the whole candidate header once, replacing the small empty
Storage owner's count. Existing table partition ceilings continue to apply.
Allocator overhead, provider material and peak call frames remain separate
measurements; no RSS claim follows from these object-layout guards.

A Candidate proves structural closure of the statements actually submitted,
not reader EOF, protected-file trust, resolved signatures, certificate
validity or runtime publication. M04b2c3d4a owns the reader operation and
returns its Loaded wrapper only after actual EOF. M04b3 consumes exclusive candidate
access for protected signature/credential material before publication; no
mutable cells or loose headers are exposed by the current public API.


## Whole-reader structural loading

M04b2c3d4a adds `config::load::read`. It consumes preallocated Storage,
borrows Pending and the 28 KiB stream scratch, and owns the complete reader
operation. The restricted sink drives the dispatcher; stream success requires
an actual `Read::read` result of zero. Only then does structural finalization
close required sections, references and resource plans. There is no API to
supply an unrelated stream Summary as completion evidence.

Success returns `Loaded`, a private wrapper around the structural Candidate.
It lends read-only candidate access; consuming it can erase the wrapper or
recover storage. Callers cannot construct a Loaded from a statement-only
Candidate. This certifies EOF from the same trusted reader operation, not
filesystem ownership, protected-input validity, TLS material or publication.
Reader adapters remain responsible for truthful EOF, bounded allocation and
blocking behavior. This increment opens no files or sockets.

Failure returns the reusable storage carrier. The first recorded dispatcher
failure remains Configuration, preserving its typed cause and source context;
this takes precedence over a wrapping reader callback error. A stream error
without a dispatcher failure remains Input and can be inspected as its precise
syntax/read/capacity/interruption variant. Input(Handler) cannot originate
from this loader: its only handler is the dispatcher, whose failures become
Configuration. The carrier's generic formatting
continues to redact input errors. The caller's prior candidate is independent
and remains readable during replacement and after a failed load.

Tests cover fragmented input and an unterminated final line, actual EOF,
late read failure, late syntax/handler/finalizer refusal, scratch shortage
before reader I/O, and exhausted domain descriptors and shared text. Each
failure can return storage for a valid retry. A compiler-code-pinned negative
example prevents external construction of Loaded. No runtime generation is
published; M19 retains that responsibility.

A compile-time sum of the named Pending, dispatcher Builder, borrowed Parsed,
Loaded, Failure, stream Summary, Framer and Statement representations fits
36 KiB. This deliberately counts both build and completed representations,
including inline global settings and resource plans. The separate stream
scratch remains 28 KiB within the existing 64 KiB parser reservation. This is
an object-layout guard, not compiler peak-stack evidence: parameter moves,
initialization temporaries, nested helper frames, trusted reader frames and
provider stack use need separate execution evidence. M04b2c3d4b supplies
the structural-loader check below; later protected/provider paths must
qualify their additional frames before service integration.


### Portable structural-loader stack qualification

`tests/config_stack.rs` exercises the production library as an integration test.
Generic loader/dispatcher/stream functions are compiled in that test crate,
with its fixture readers and inlining decisions. The installed executable
has no service caller yet; this is evidence for the test compilation only.
The portable command in td-crypto/PORTABLE.md builds it with the same pinned
release musl compiler, target, frame-pointer flags and Cargo graph as the
installed binary. Its qualification case is ignored by ordinary host tests;
it refuses non-release or non-x86-64-musl execution. The portable runtime
selects that exact case in its own process with a 30-second deadline and
requires exactly `1 passed; 0 failed`. A nonignored host case runs the same
eight scenario bodies on the ordinary harness stack to catch fixture drift;
it makes no target stack claim.

The case requests a 160 KiB worker stack, allowing for musl's additional
mapping overhead. Before loading, a bounded 1 MiB read of `/proc/self/smaps`
locates a live local's writable private mapping. Its whole extent must be at
most 176 KiB, with an adjacent lower no-access guard of at least 4 KiB and
without the `gd` grow-down flag. Failure to establish that evidence fails
qualification. A host unit case checks rejection of oversized/growing regions,
missing flags and missing, short or noncontiguous guards. No unsafe stack
inspection or custom allocator is introduced. The runner shows successful
captured output and relays `config_stack_mapping_bytes` into the invoking
build log; the artifact receipt does not retain this runtime measurement.

On that worker, the fixture constructs storage and Pending, then exercises
fragmented and final-line loading, EOF, late I/O/syntax/schema/reference
refusals, old/replacement coexistence, returned-storage reuse, short scratch,
and descriptor/text exhaustion. Maximum identity/path fields, a permuted full
alias table and full domain table, gateway/MTA-STS/resource stanzas,
ACME/HTTP-01, loopback listeners, logging and identity addresses exercise the
larger and alternative structural paths. A production compile-time guard
requires the borrowed Identity/Address layouts and list ranges to fit the
separate 80 KiB reservation on every compiled target. Existing
compiled 36 KiB workspace guards remain in force.

This establishes a point-in-time executable stack ceiling for these
test-compiled loader paths on the qualified artifact, not an exact high-water measurement or a
whole-process RSS bound. Fixture construction and mapping inspection allocate;
this is not the service allocation test. The reader is an injected bounded
slice reader. M04b3/M05/M07/M19 must requalify protected finalization, real reader
adapters, providers and runtime integration with the same total reservations.
Compiler/profile/target or loader-path changes require a manual rerun of this
portable check; ordinary `ready` does not enforce it. Every new compiled
instance of `read`/`build_stanzas` needs qualification, even when using the
same reader type. Five scenario bodies intentionally repeat the loader unit
cases to exercise an external compilation; changes to their coverage must
update both suites.


## Operator-file content decoding

M04b3a adds `config::material::read`, an allocation-free content driver over
an injected `Read`. `Kind::Signature` accepts at most 16384 bytes of UTF-8
without NUL; it preserves every byte, including newlines, markup and any BOM.
Empty signatures are valid. `Kind::RelayPassword` implements SCHEMA.md's
printable-ASCII password format and optional single terminal LF/CRLF. The
kinds share no expansion, normalization, filesystem or provider behavior.

The caller lends `Kind::scratch_bytes()` bytes: 16385 for signatures and
1027 for passwords, including the over-limit observation byte. Short scratch
refuses before reader I/O. Larger slices have only that prefix used. Before
reading, initialize the admitted prefix to zero once, so a faulty reader that
reports unwritten bytes cannot return a prior password as signature text.
This is buffer initialization, not a secure-erasure guarantee. Every
successful value requires an actual nonempty-window EOF read, even when the
raw size is exactly its limit. Oversize input fails after at most limit+1
bytes. Invalid `Read` counts refuse before slicing. The total Interrupted
retry allowance is the stream driver's 32, without resetting on progress;
other I/O errors fail immediately and retain only `ErrorKind`. Late errors
never return a partial value. The trusted reader owns blocking, allocation
behavior, truthful EOF/counts and writing every byte it reports; this helper
supplies no wall-clock deadline.

`Value` privately carries its kind and borrowed text. Its explicit `text()`
accessor is for trusted finalization; Debug is redacted and there is no
Display. Fixed `config_material_*` diagnostics carry no path, file bytes or
arbitrary I/O error payload/source chain. Scratch can retain input on success
or failure; neither this helper nor the existing text arena promises secure
erasure. Later integration must account for the whole credential lifetime.

This is content validity and same-read EOF evidence only. No protected-file
ownership, permissions, ancestor trust, runtime authentication or publication
proof follows. M04b3's finalizer must consume a structural candidate, use M05
trusted file handles, append decoded text within the remaining 192 KiB arena,
and discard candidate authority on any late error. The decoder's maximum
scratch is sized to reuse the existing 28 KiB stream region after structural
EOF; compile-time checks pin both kinds' fit. M04b3b must implement that reuse.
No second concurrent buffer is budgeted.
New compiled finalizer/reader instances still need CONFIG.md's target stack
qualification before service use. Provider inputs remain M07's separate
certificate-generation ledger; this helper does not load keys or trust stores.

## Referenced-file inventory

M04b3b1 adds `config::inputs::Cursor` over an immutable structural Candidate.
It stores a process-local text-owner ticket and a bounded scan position,
without borrowing the candidate between calls. `visit_next` requires the
same owner on every call, including after moving the candidate. Rebuilding
reused storage issues a new ticket and refuses an old cursor before invoking
its callback. This is request provenance only, not a file-trust credential.

Each successful step invokes its callback at most once with a borrowed
Reference and typed Target. The callback's reference cannot escape, while
its result may borrow caller-owned scratch. This permits later exclusive
finalization to read one input, release path borrows and then append the
decoded bytes within the same arena. That mutation is not implemented here.
Callback allocation, blocking, its own frames and side effects remain
trusted caller responsibilities. Inventory frames remain live during a
callback, including reopened graph Records for certificate/gateway inputs.
M04b3b2's stack qualification must include those frames together with the
callback and finalizer. No file is opened and no content is loaded by the
inventory itself.

Scanning is bounded by 179 slots: two per identity, relay password/CA,
ACME CA, two per certificate and one per gateway. This is a conservative
ceiling; some slots are mutually exclusive. Skip absent optional references,
preserving distinct requests when several roles share a path. Identity order
is ascending raw ID, text then HTML; then relay password/CA, ACME CA, profile
chain/key pairs and gateway CAs in their stored index order. Indices identify
this candidate only. ACME profiles do not yield managed key/chain requests.
SCHEMA.md owns each raw cap and the M05 ownership/mode/ancestor requirements.

Exhaustion remains None on repeated calls for the same owner; it proves only
that the inventory was visited. Any callback, invariant or owner failure
makes the cursor terminal, with subsequent calls returning FailedCursor.
The cursor is terminal during dispatch too: catching a callback unwind does
not allow continuation past the skipped input.
Fixed `config_inputs_*` errors and Debug output contain no paths or arbitrary
callback payload/source chains. Trusted callers can inspect the typed
Callback variant explicitly. A structural candidate need not prove source
EOF; finalization must still consume `load::Loaded`. The inventory grants no
permission, resolved-input, provider or runtime publication authority.


## Resolved configuration text

M04b3b2a adds `config::materialize::read_text`. It consumes `load::Loaded`,
retaining the requirement for source EOF from one reader operation, and
returns `ResolvedText` only after every configured signature and the relay
password has decoded through EOF. A statement-only Candidate cannot create
that wrapper. Provider key/chain/CA requests are skipped; their loading and
validation remain M07. This is text materialization, not a fully finalized
configuration or permission to publish, authenticate, or serve requests.

The caller supplies the stream scratch after source EOF and an opener that
returns an owned reader. Require at least the signature decoder's 16385
bytes before any opener call. Traverse the owner-bound input cursor, open
one text reference, decode into scratch, release the path borrow, then
append the decoded bytes to the existing non-routing text arena. The arena
retains its owner ticket and already initialized prefix. No new arena,
resizable collection or concurrent read buffer is created. Opener/reader
allocations, truthful read results, blocking and later M05 descriptor trust
remain trusted adapter obligations. M05 must check every operator input's
ownership, modes and secret/public inode separation before any content can
leave the trusted configuration worker; checking only text references is
insufficient.

Two resolved spans occupy the previously reserved sixteen bytes per identity
cell, with two completion flags inside the existing 128-byte ceiling.
Refuse a duplicate store and verify that every configured signature path
has completed before returning ResolvedText, independently of cursor
exhaustion. Absent signature paths resolve to empty text; present empty files also yield empty text without erasing the
original path's presence. Signature bytes remain exact. Relay password bytes
use the decoder's specified line-ending rule and remain behind a private
handle with an explicit trusted accessor. All Debug/error formatting stays
redacted; typed input errors are available only through explicit inspection,
with no automatic source chain. Failure retains an optional typed Target
for explicit diagnostics without its path; whole-stage failures have none.
M05/M07 must consume this content stage into a configuration wrapper carrying
all descriptor, inode-separation and provider checks before publication.
SMTP/JMAP/relay consumers must receive that validated runtime configuration,
never ResolvedText directly. Its accessors serve trusted configuration work.

Any opener, content, inventory or aggregate text-capacity failure consumes
the candidate's headers and returns reusable Storage. Its private backing
can retain bytes. A bounded guard fills the decoder's 16385-byte scratch
window with zero on success, error or stack unwind; bytes beyond that
window remain untouched. Short scratch fails before modification or I/O.
This observable scratch hygiene is not a secure-erasure guarantee: stored
credential bytes and copies outside this window still have their normal
lifetimes. A separate active generation is untouched. A panic in trusted
callback code unwinds exclusive ownership and cannot return a partial ResolvedText. This
helper introduces no production panics or recovery mechanism.

The resolved owner reports actual allocated table capacities plus its full
inline header, replacing Candidate's inline size once. All cells retain
their existing partition limits; the small resolved owner fits global/plan
headroom. Individual signature/password limits do not guarantee aggregate
fit in the 192 KiB text arena. Identity preimage encoding has its own
192 KiB ceiling. Existing structural stack qualification does not cover this
compiled finalizer. The combined fixture below covers its test instances;
M04b3b2b2b/M05/M07 must qualify the complete production
reader/finalizer/provider path before service use. Protected-input
integration, effective output and runtime publication remain pending;
M04b3b2b1's identity assembly is specified below.


## Identity preimage assembly

M04b3b2b1 adds `config::preimage::write` over ResolvedText. First assemble
all address views and list ranges in fixed arrays; then assemble identity
views borrowing the completed address array and the immutable text arena.
There is no self-referential owner, growing collection or retained preimage.
The existing 80 KiB view reservation covers 2048 Address entries, 64 Identity
entries and 128 fixed list ranges. Compile-time layout checks include all
three arrays. The loader's workspace guard refers to this same sum.

Preserve ascending raw identity IDs, each address list's declaration order,
null versus empty lists, and absent versus empty address names. Take exact
resolved text/HTML signatures from the content stage; absent files remain
empty. The relay password, signature paths and other configuration fields
are excluded by field from this visible-identity representation. M05 must
reject secret/public input-file aliasing before publication; this content
stage alone cannot prevent a signature from containing credential bytes.

Invoke the existing versioned `identity::write_preimage` only after view
assembly succeeds. It validates the whole immutable representation, including
its independent 192 KiB encoded-size ceiling, before the first sink call.
A candidate fitting the text arena can still fail that ceiling because the
preimage includes IDs and framing. The outer error wrapper formats fixed
codes only and exposes no arbitrary sink source chain. Explicit typed
inspection retains the underlying validation or sink error.

Sink failure can leave a prefix and must discard that output/digest. Sink
allocation, blocking and panic behavior remain trusted caller obligations.
The writer is for private configuration work until M05/M07 complete; it
confers no file trust, authentication, digest or publication authority.
The local td-crypto streaming Digest can consume the writer later without
retaining a second 192 KiB buffer. Actual combined reader/materializer/view
stack qualification for the test instances is described below; installed
service integration must be qualified separately. This object-layout guard
does not measure compiler frames or total stack use.


## Combined configuration stack qualification

M04b3b2b2a extends the existing portable integration executable with a
separate `portable_materialized_stack` case. It runs structural loading,
text materialization and visible-identity preimage writing on one worker,
reusing snapshot storage and caller scratch between fixtures. The original
structural-only case retains its independent 176 KiB ceiling.

The new worker requests 240 KiB. The same bounded `/proc/self/smaps` reader
requires its entire private writable stack mapping to fit 256 KiB, with
an adjacent lower no-access guard of at least 4 KiB and no grow-down flag.
The portable runner requires exactly one positive measurement under
`config_materialized_stack_mapping_bytes` and a successful one-test summary.
The actual mapping is relayed to the build log. This is a checked ceiling,
not a stack high-water measurement or an additional ledger reservation.

The combined worker also runs the complete structural-loader scenario set.
Materialization fixtures exercise fragmented structural and text readers,
maximum pending fields and routing tables, all 64 identities and 2048 address
entries, gateway/ACME/relay-CA inventory, both maximum-size signatures, text-arena overflow,
and the independent preimage ceiling. They also exercise sink refusal,
late password-reader failure, decoder-scratch clearing and returned-storage
reuse. The ordinary host test executes the same fixtures to catch drift;
its harness stack is not target evidence.

This is manual point-in-time qualification of these generic reader, opener
and sink instances in the integration test's pinned release-musl compilation.
Fixture construction and mapping inspection allocate; no hot-path heap,
provider-allocation or process-RSS bound is established. Protected-file
opening, provider adapters and installed runtime callers must requalify
their complete compiled paths within the existing worker reservation before
service use. This test does not confer configuration publication authority.

Rerun the portable command manually after compiler, profile, target or
compiled-path changes, including the loader, inventory, material decoder,
materializer, preimage assembly and identity encoder. `ready` does not enforce
this evidence freshness. Each new compiled reader/opener/sink instance needs
its own qualification; a prior artifact does not qualify later code.
