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

DNS helpers only validate; they do not return folded storage. The later
snapshot text builder owns shared lowercase copying, and callers must fold
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
Unknown fields, duplicate references, URI/bind/CIDR parsing and snapshot text
ownership remain later M04b2c3 work; these helpers cannot produce a complete
validated configuration.
