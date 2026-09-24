# td-mta configuration

## Scope

This document owns the configuration syntax. `config::syntax` implements
bounded framing and statement decoding only. The typed field schema,
defaults, snapshot builder, reference/resource validation, protected file
access, effective output, and CLI remain M04b2/M04b3/M05/M19 work as assigned
in IMPLEMENTATION.md. A syntactically accepted statement is not a valid
service configuration. DESIGN.md §6 owns the administration contract;
RESOURCES.md owns the aggregate memory budget.

## Physical input and limits

Input is UTF-8, with LF or CRLF line endings, including mixed endings. A final
unterminated line is allowed. A bare CR is invalid. Empty input is syntactically
valid; the schema must still require its version and mandatory settings.
A UTF-8 BOM prefix is refused. Outside quoted strings, whitespace means ASCII space
or horizontal tab. Every line, including comments, must be valid UTF-8 and
contain no Unicode control character except horizontal tab. Also reject
Unicode line/paragraph separators U+2028/U+2029 and directional controls
U+061C, U+200E/U+200F, U+202A..U+202E and U+2066..U+2069, including in
comments, so visual line/direction changes cannot disguise settings. Quoted
strings also reject raw horizontal tabs. Other Unicode text is retained
without normalization;
field-specific validation follows syntax decoding.

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

Strings use double quotes. Only `\"` and `\\` are escapes; all other
backslash sequences are errors. `#`, `=`, `[` and `]` inside quotes are literal
text. Empty strings and empty labels are syntactically valid. There are no
single quotes, multiline strings, Unicode escape sequences, substitutions,
or escape sequences for controls. Represent Unicode directly as UTF-8.

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

The control worker's existing 64 KiB configuration scratch is divided into
16 KiB input, 8 KiB physical line, 4 KiB decoded string and 36 KiB parser/builder
state. Construction of the candidate uses its already budgeted snapshot;
there is no whole-file input copy or allocated syntax tree. This increment
does not instantiate the control worker, open files, read credentials, or
prove ownership/permissions. M05 supplies the trusted filesystem boundary.

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
`config_expected_bracket`, `config_expected_space`, and `config_trailing_data`.
They are library diagnostics; CLI JSON and exit-code wiring is still planned.

Display/debug diagnostics never include source bytes. `Statement` and `Value`
implement redacted `Debug`, including names, labels, integers and booleans.
Their typed accessors/patterns intentionally expose data to the trusted builder;
redacted debug output is not an authorization boundary. Never interpolate a
rejected key, value, input line or referenced secret into a normal event.
