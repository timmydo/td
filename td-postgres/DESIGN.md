# td-postgres: a synchronous PostgreSQL client

td-postgres is td's own PostgreSQL client: frontend/backend protocol 3.0
over a blocking stream, SCRAM-SHA-256 login, the simple and extended
queries, and the handful of types td-mta stores. td-mta, whose workers
are threads, uses it in place of the async driver and external closure
it first admitted: a std-only client whose every bound is the caller's.

The crate depends on std and on td-alloc, td-encoding, td-fs and
td-primitives. It forbids `unsafe`. Its cryptography is td-primitives'
SHA-256, HMAC, PBKDF2, entropy and constant-time comparison. It connects
only to a Unix-domain socket and carries no TLS.

## What td-mta uses

The client covers exactly this surface, read from td-mta's sources
(`td-mta/src/postgres_*.rs`, `postgres_probe.rs` and its modules), and
nothing past it. td-mta reaches it through `postgres_probe::Client`, a
per-worker adapter that holds the worker's deadline, makes a deadline or
broken session sticky for the worker, and copies owned results through
td-alloc.

| Concern | What td-mta does |
| --- | --- |
| Endpoint | A private or group-shared Unix-domain socket whose ownership td-mta checks first (`td_fs::check_server_socket`), through `connect_unix_through` with td-mta's stream filter. No TCP, no TLS, no channel binding |
| Login | SCRAM-SHA-256 only. td-mta's input filter (`postgres_probe/framing.rs`) refuses any other authentication request, AuthenticationOk without the SASL exchange, and any ReadyForQuery before authentication. Passwords are generated hex tokens, at most 4096 bytes |
| Startup parameters | user, dbname, password, `application_name = td-mta-postgres`, and `options = -c statement_timeout=5000 -c lock_timeout=1000 -c idle_in_transaction_session_timeout=10000 -c search_path=pg_catalog` |
| Simple query | `batch`: BEGIN with isolation and `SET LOCAL` lines, LISTEN and NOTIFY, a few SET and DML statements; rows, as from `SELECT pg_advisory_xact_lock(...)`, are discarded. The connection probe reads its five text settings with `query` |
| Extended query | `query`, `query_one`, `query_opt` and `execute`. No explicit `prepare`, no statement reuse, no portals or row streaming, no pipelining |
| Transactions | `BEGIN ISOLATION LEVEL ...` through `batch`, then `commit` or `rollback`; no savepoints. Every COMMIT error where an outcome matters, `RolledBack` included, is taken as indeterminate and resolved later through durable receipts (POSTGRES.md section 3) |
| Advisory locks | SQL functions only: `pg_try_advisory_lock_shared`, `pg_try_advisory_xact_lock[_shared]` (results `bool`), `pg_advisory_xact_lock` (in a batch) |
| Notifications | `LISTEN td_mta_relay` on a retained read session; a NotificationResponse on that channel with an empty payload wakes the relay; an idle session is drained without a query to find a server-ended connection |
| Not used | COPY, cancellation, function calls, replication, large objects, statement metadata |

Parameter types, each with its Rust form: `int2` (`i16`), `int4`
(`i32`), `int8` (`i64`), `bool`, `bytea` (`&[u8]`, `Vec<u8>`), `text`
(`&str`, `String`), any of those as `Option` for NULL, and the arrays
`bytea[]` (`Vec<&[u8]>`, `Vec<Vec<u8>>`, `Vec<Option<&[u8]>>`), `int8[]`
(`Vec<i64>`, `Vec<Option<i64>>`), `text[]` (`Vec<String>`, `Vec<&str>`)
and `bool[]` (`Vec<bool>`). There is no `int4[]`: td-mta sends small
integer arrays as `int8[]` and lets the column's assignment cast narrow
them.

Result types: `int2`, `int4`, `int8`, `bool`, `bytea`, `text` and
`name` (as `&str` or `String`), each optionally NULL, and the arrays
`bytea[]` and `text[]`. Columns are read by index (about 740 sites) and
by name (about 110). `timestamptz`, `interval` and `oid` appear only
inside SQL (`lease_until > clock_timestamp()`, `oid::bigint`) and never
cross the wire; `uuid`, `jsonb` and every other type are absent.

Errors: td-mta maps every client error to its own opaque kinds and reads
one field, the SQLSTATE, once: `55P03` (`lock_not_available`) in
`postgres_route_owners.rs`. Messages are never kept.

Deadlines: connection at most ten seconds (`MAX_DURATION`), whole
operations ten, thirty for stored bodies, up to an hour for owner audits,
each an `Instant` passed to every call; server-side statement,
lock and idle-in-transaction timeouts from the startup options and
`SET LOCAL`. Bounds: every backend message at most 128 KiB, and
per-connection and per-turn input bytes, message counts and output bytes
enforced by td-mta's stream-level filter before the client reads.

## Surface

- `connect_unix(path, config, limits, deadline)` opens a session;
  `connect_unix_through(path, wrap, ...)` runs it over the `Stream` that
  `wrap` makes of the connected `Socket`, so a caller's own filter sees
  the startup and login too; `startup(stream, ...)` starts one over any
  `Stream`.
- `Client::batch(sql, deadline)`: the simple query, several statements,
  rows discarded.
- `Client::query`, `query_one`, `query_opt`, `execute`: the extended query,
  one statement with `&[&dyn Encode]` parameters.
- `Client::commit(deadline) -> Result<(), CommitError>` and
  `Client::rollback(deadline)`.
- `Client::wait(until)`, `take_notification`, `dropped_notifications`.
- `Client::transaction_status`, `is_broken`, `limits`, `set_limits`,
  `close`.
- `Rows`, `Row` (the one row of `query_one`/`query_opt`) and `RowRef`:
  `get::<T>(index)`, `get_named::<T>(name)`, `raw(index)`.
- `Encode`, `Typed`, `Element` and `Decode<'a>`, implemented for the types
  above; `Array<'a>` reads `bytea[]`, `text[]`, `int8[]` and `bool[]` in
  place; `oid` names the type OIDs.

Every parameter names its own type, so a value the server would
otherwise infer as another type (`name`, `varchar`) is sent as `text`
and the SQL casts where it must. Owned results are read borrowed, `&str`
or `&[u8]`, then copied through td-alloc, and arrays as `Array`
iterations. td-mta's `Stream` is its filter over the `Socket`.

## Stream

`Stream` is `read(buffer, deadline)` and `write_all(bytes, deadline)`.
A deadline already past makes `read` take only what is ready, which is
how `wait` polls. `Socket` implements it for Unix sockets with
`SO_RCVTIMEO`/`SO_SNDTIMEO` and a nonblocking poll; no `poll(2)` and no
`unsafe`. The socket is dialed by `td_fs::unix_connect_until` within the
connection's deadline. No SSLRequest is sent: the session is plaintext
over the local socket, whose access the caller decides.

## Startup and SCRAM

The startup message carries `user`, `database`, `application_name` when
not empty, `client_encoding = UTF8`, then the caller's `parameters`.
Those may not repeat or name the client's own, `replication` or a
`_pq_.` protocol option, and the message is at most 10,000 bytes, the
server's own ceiling.

The one accepted login is SCRAM-SHA-256 without channel binding (GS2
header `n,,`, so a server offering SCRAM-SHA-256-PLUS learns that the
client does not bind). The order is fixed: AuthenticationSASL naming
SCRAM-SHA-256, SASLContinue, SASLFinal, AuthenticationOk, then
ParameterStatus and BackendKeyData to ReadyForQuery. Any other request,
cleartext and MD5 included, an AuthenticationOk before SASLFinal is
verified, a SASL message out of turn, or a server signature that does
not match is `Authentication`. The server-first message must extend
this client's 24-character random nonce with printable characters, give
a canonical base64 salt of 1 to 128 bytes and a canonical iteration
count no greater than `Limits::scram_iterations` (more is `Limit`), and
carry no other attribute. The server-final must be exactly `v=` and the
signature, compared in constant time; `e=` is refused. SCRAM's own
username is empty, as PostgreSQL ignores it for the startup's.

Passwords are ASCII without NUL. SASLprep (RFC 4013) maps nothing in
ASCII, and PostgreSQL uses a password SASLprep refuses (one with control
characters) as it is, so every ASCII password reaches the same verifier
here as in libpq. Non-ASCII passwords would need SASLprep and are
refused before connecting.

The server must report `client_encoding` as `UTF8` before its first
ReadyForQuery, which must be idle, and may never report another value:
text values are taken as UTF-8. Before AuthenticationOk only a
NoticeResponse may come between the login's messages; a parameter report
or notification there is `Protocol`.

## Queries

`batch` sends one Query and reads each statement's RowDescription,
DataRows, CommandComplete or EmptyQueryResponse up to ReadyForQuery.
Rows are counted against `Limits::rows` and dropped. A ReadyForQuery
with neither a completion nor an error before it is `Protocol`.

The extended query is one write and one round trip: Parse into the
unnamed statement with every parameter's type OID, Bind with every
parameter and every result column in binary, Describe the portal,
Execute it for all rows, Sync. The answer must be ParseComplete,
BindComplete, RowDescription (every column binary) or NoData, DataRows
of the described width, CommandComplete or EmptyQueryResponse, then
ReadyForQuery. Naming the parameter types means the server checks each
value against its declared type instead of inferring one; a statement
whose SQL needs another type says so with a cast. `execute` returns the
count of an INSERT, UPDATE, DELETE, MERGE, SELECT, FETCH, MOVE or COPY
tag, and 0 for any other command.

After an ErrorResponse only ReadyForQuery may follow. Its SQLSTATE and
severity are kept; its message, detail and hint are not, since they can
quote the statement's data. An ERROR leaves the session usable; a FATAL
or PANIC breaks it.

## Bounds

`Limits` holds every bound, and nothing defaults:

- `message_bytes`, 256 bytes to 1 GiB: the largest backend message
  body, so excluding the type byte and the four-byte length (td-mta's
  128 KiB frame cap, which counts the length, is a `message_bytes` four
  less). A length past it is refused from the five-byte header, before
  any body is buffered; the read buffer grows to fit a message and no
  further.
- `request_bytes`: the largest request written; a larger one is refused
  before anything is sent.
- `messages`: the backend messages one operation, the startup or one
  idle wait may read, every notice, parameter report and notification
  included, so a flooding server ends in `Limit` rather than at the
  deadline.
- `rows`, `columns` (at most 1664, PostgreSQL's own) and `result_bytes`:
  a query's rows, a result's columns, and the bytes a kept result holds,
  names and values plus 16 bytes a column and 8 a value.
- `notifications`: those held for the caller; later ones are counted in
  `dropped_notifications` and dropped, as are any whose copy td-alloc
  refuses and any with a channel over 63 bytes or a payload over 7999,
  more than a default-built PostgreSQL sends, so held notifications stay
  small. Notifications are hints; td-mta's correctness never rests on
  them (POSTGRES.md section 2).
- `scram_iterations`: the PBKDF2 work the server may ask for.

An operation may thus read up to `messages` times `message_bytes`, most
of it discarded. A total byte budget per connection or per turn, such
as td-mta's stream filter keeps, stays a wrapper of the caller's around
its `Stream`: `connect_unix_through` puts it around the Unix socket
before the startup message, so every byte the session moves passes
through it.

Every server length, count and terminator is checked against the bytes
present, every message must be consumed exactly, negative counts and
lengths other than NULL's -1 are refused, and a message this client
never asks for (COPY, function call, portal suspension, protocol
negotiation) is `Protocol`. All growth goes through td-alloc; a refused
reservation is `Allocation`. Production code holds no `unwrap`,
`expect`, panic or panicking index.

## Errors and the unknown COMMIT

`Error` says why an operation failed. A failure before the request is
written (`Invalid`, `Allocation` or `Limit` while building it, a
deadline already past) leaves the client as it was. So does a server
ERROR, read to its ReadyForQuery, and `RowCount` from `query_one` or
`query_opt`. Any other failure after writing, with the answer not read
to its ReadyForQuery, leaves the protocol's state unknown, so the client
is broken: every later call is `Broken`, and the caller drops it. A
write that fails because the server ended the session reports the FATAL
or PANIC the server sent before closing as `Db`, when it is already
readable, and `Io` otherwise; only what is ready is read. A COMMIT so
refused is still `OutcomeUnknown`: a failed write does not prove the
server read none of it.

`commit` is COMMIT through the simple query, with an outcome of its own:

- `Ok(())`: CommandComplete `COMMIT`, then ReadyForQuery idle.
- `RolledBack(Some(error))`: an ERROR with no CommandComplete before
  it, read to a ReadyForQuery that is idle, or failed, in which case the
  client is broken so that the failed block ends with the connection.
  This rests on PostgreSQL's implementation, not on the protocol: it
  raises errors at COMMIT before the commit record, and one after it is
  a PANIC. A caller that resolves every COMMIT error from durable state,
  as td-mta does today, may go on treating this as unknown.
- `RolledBack(None)`: the tag `ROLLBACK`, which a failed transaction's
  COMMIT returns.
- `NotSent(error)`: nothing was written: the client was broken, the
  deadline had passed, no transaction was open, or the request could not
  be built. An open transaction then ends with its connection, uncommitted.
- `OutcomeUnknown(error)`: COMMIT was written and no complete answer up
  to ReadyForQuery came back: the connection closed or failed, the
  deadline passed, a limit was exceeded, a FATAL arrived (a terminated
  backend may already have committed), or the answer broke the protocol.
  A CommandComplete `COMMIT` with no ReadyForQuery after it is still
  unknown, as is one followed by an ERROR, and any answer that leaves a
  transaction live. The client is broken; the caller resolves the outcome from
  durable state on another connection.

A statement in autocommit has the same uncertainty and no type of its
own: a broken client after `execute` says only that its effect is not
known.

## Notifications and idle connections

NotificationResponse, NoticeResponse and ParameterStatus may arrive
between any two messages; each is handled where it arrives and counted.
`wait(until)` reads only these, returning once a notification is held or
`until` passes; with `until` already past it reads what is ready and
does not wait, which is how a caller finds a connection the server
ended. A partial message left at `until` stays buffered and is resumed.
An idle connection's FATAL is `Db` and breaks the client; any other
message is `Protocol`.

## Termination

`close` writes Terminate. Dropping a client that is not broken writes it
too, waiting at most 100 ms.

## Not supported

TCP, TLS of any form, channel binding (SCRAM-SHA-256-PLUS), every login but
SCRAM-SHA-256, SASLprep of non-ASCII passwords, named prepared
statements, portals and row-at-a-time fetch, pipelining, COPY,
cancellation requests, function calls, protocol 3.2, the text result
format in the extended query, and every type not listed above.

## Tests

Unit tests run against byte fixtures and a scripted in-memory stream,
and the connect forms against a Unix listener in a `td_fs::TempDir`:

- framing of every backend message at every fragmentation, lengths
  below four or above the limit refused from the header, and every
  message refused with a byte added or cut;
- hostile values: negative counts and lengths, values longer than their
  message, unknown tags, statuses and formats, SQLSTATEs malformed or
  repeated, severities a notice would have;
- SCRAM: RFC 7677's exchange, PostgreSQL's empty-user form (from Python's
  `hashlib` and `hmac`), and each hostile server-first and server-final
  refused;
- startup: trust, cleartext, MD5, GSS, a PLUS-only offer, a missing or
  forged proof, AuthenticationOk out of turn, a non-UTF8 encoding, and a
  Unix login carried, or cut short, by a caller's wrapper;
- queries: rows read and typed, errors that keep or end the session,
  every protocol-order violation, each limit, a stalled server's
  deadline;
- COMMIT: committed, rolled back both ways, never sent, and unknown for
  every truncated or hostile answer;
- notifications held, bounded and resumed across a deadline;
- td-alloc refusals stepped from zero through a query and a held
  notification.

`tests/postgres.rs` runs the client against a real PostgreSQL 17 server
on a disposable cluster in a `td_fs::TempDir`, found as td-mta's native
fixtures find theirs: the first `initdb` on `PATH`, with `postgres` and
`pg_ctl` beside it, each tool run with only `PATH` and `LC_ALL=C`. It
covers SCRAM login over the Unix socket, directly and through a
counting wrapper, a wrong password, cleartext and trust refused by the
client, every
type both ways (arrays with NULLs, `name`, the extremes of each integer,
all 256 byte values), the extended and simple queries, commit and
rollback, a failed transaction's COMMIT, a deferred constraint failing
at COMMIT, SQLSTATE errors including `55P03`, a startup
`statement_timeout`, a client deadline, a COMMIT whose deadline passes
while a deferred trigger runs (unknown, and the server then commits) and
one whose backend is terminated (unknown, FATAL `57P01`), LISTEN and
NOTIFY, an ended idle session found by a poll, a query and a COMMIT
written to a session the server already ended (its FATAL reported, the
COMMIT unknown), and each result limit. The test is ignored in ordinary
runs, since the sandbox gate has no PostgreSQL; the host preflight runs
it through `test-args = "--include-ignored"`.
