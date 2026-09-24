# Protocol acceptance case registry

M02 assigns these stable case IDs and independent oracles. They are a handoff,
not executable tests or a conformance claim. M06/M13-M17 add actual fixtures
and map each case to a test; M22 runs the applicable cases through real td-mail,
td-fetch and td-mta; M24 audits every mandatory CONFORMANCE.md row. No network
test contacts a real provider, public CA or production mail store.

Use literal expected bytes/JSON or independently specified records. Do not
produce expected answers with the parser, query, codec or state machine under
test. The existing format-v1 fixtures and wire/sync tests retain their own
byte-level evidence; this registry adds endpoint and interpretation evidence.

## Shared fixture conventions

Example shorthand A/M/E/T/B/I/S expands in the harness to canonical local wire
IDs with the appropriate prefix and fixed distinct 16-byte values. Also test
valid foreign-format IDs, wrong local type, malformed generic IDs and an ID
belonging to a different account. Use an injected clock, sequence, epoch and
entropy stream; expected states follow API.md/POLICY.md's literal encodings.
Do not treat the shorthand itself as an accepted alternate ID spelling.

Each standard method row below expands into named positive/default, explicit
projection, invalid-argument, absent-ID, unauthorized-account and boundary
cases where applicable. Each property in its CONFORMANCE.md group gets an
individual assertion, including omitted versus null, read-only and empty
values. An inapplicable dimension must have a reason; a method-level green
cannot stand for an untested property. Mutation cases additionally compare
raw files/metadata after restart and fault injection. Method responses retain
call IDs and ordered implicit responses exactly as ADMISSION.md defines.

## Interpretation cases and literal oracles

Rows containing exact inputs/results are literal oracles. Other rows name
coverage obligations whose owning milestone supplies the executable corpus.
Strings use JSON escapes for raw CR/LF or non-ASCII characters; decode them
to form fixture bytes. Expected strings are independent literals.

| ID | Input / independent expected result | Owner |
| --- | --- | --- |
| H01 | `Subject: first\r\nSUBJECT: =?UTF-8?Q?Cafe=CC=81?=\r\n\r\n`: header:subject is ` =?UTF-8?Q?Cafe=CC=81?=`; header:subject:asText is `Caf\u00e9`; :asText:all is `["first","Caf\u00e9"]` | M06/M14 |
| H02 | `X-A: a\r\n\tb\u0000c\r\n\r\n`: Raw value ` a\r\n\tbc`, Text `a\tbc`; absent single field null, absent :all [] | M06/M14 |
| H03 | `Subject: x=?UTF-8?Q?Cafe=CC=81?=y\r\n\r\n`: bad placement leaves Text `x=?UTF-8?Q?Cafe=CC=81?=y` | M06/M14 |
| H04 | Message-ID `<a@example.test> garbage` has MessageIds null; complete `<a@example.test> (x) <b@example.test>` gives `["a@example.test","b@example.test"]`; header:From:asDate is invalidArguments | M06/M14 |
| H05 | Address/group/quoted-pair/comment fallback, UTF-8/EAI, RFC 2047 placement, Date numeric/obsolete zones and -0000, List URL parse failures; assert every header form and whitelist combination | M06/M14-M15 |
| H06 | Input scalars U+0065 U+0301 normalize to U+00E9; U+1100 U+1161 U+11A8 normalize to U+AC01; complete UNICODE.md official vectors and replay failures are separate required tests | M06 |
| M01 | base64 `TQ==` -> hex 4d, `TWE=` -> 4d61, `TWFu` -> 4d616e, `TQ` -> 4d with problem, `T!Q==Z` -> 4d with problem, `T` -> empty with problem | M06/M14 |
| M02 | QP `a=20\r\nb \t\r\nc=0A=QZ=` -> bytes `a \r\nb\r\nc\n=QZ` with problem; split every byte and test arbitrarily long literal trailing whitespace | M06/M14 |
| M03 | Body `\u00e9\r\nx` in UTF-8: downloaded bytes c3a90d0a78, body value `\u00e9\nx`; maxBodyValueBytes 1 -> empty/truncated, 2 -> `\u00e9`/truncated, 0 -> full/not truncated | M06/M14 |
| M04 | windows-1252 bytes 80 81 -> U+20AC U+FFFD/problem; ISO-8859-1 80 -> U+0080/no charset problem; unknown charset plus UTF-8 bytes -> decoded fallback/problem | M06 |
| M05 | Multipart delimiter CRLF excluded from leaf download, root final CRLF retained; preamble/epilogue, empty parts, missing close, bad opening, duplicate fields, boundary prefixes and nested depth/part/header limits | M06/M14 |
| M06 | RFC 8621 §4.1.4 A..K example: textBody A,B,C,D,K; htmlBody A,E,K; attachments C,F,G,H,J; multipart blobId/partId null; message/rfc822 is a leaf | M06/M14 |
| M07 | Authorized p1/p2 parse/download and exact decoded inner octets; identity/base64/QP attached messages, forged extents/tags, expired/foreign roots, pinned deletion, sixth/seventh source stage | M06/M14-M15 |
| M08 | RFC 2231 UTF-8 continued filename, ordinary/name fallback, duplicate/gapped/bad-percent candidates; every EmailBodyPart property/default and bodyProperties projection | M06/M14-M15 |
| M09 | Structured text+HTML+attachments, Bcc, empty bodies, Unicode, identities, header injection, mutually exclusive body properties, boundary collision/retry and encoded-size refusal; raw stored and captured transmitted bytes differ only as specified | M06/M15-M17 |
| M10 | Received MIME overrun: raw/metadata reads still work, requested interpretation fails; parse gives notParsable, missing parent notFound, I/O fault method error; no fabricated partial tree | M06/M14 |
| T01 | Commit E1 in T1 with `<a@x>`, E2 in T2 with `<b@x>`; new References `<a@x> <b@x>` joins T2, neither existing assignment changes | M08 |
| T02 | Two live anchors for `<a@x>` select smaller raw Email ID; delete that email then retry, survivor wins; no surviving anchor -> fresh thread | M08 |
| T03 | First valid Message-ID versus last-field messageId projection; own-ID duplicate fallback; malformed field skipped, oversize first ID no anchor; last-32 References followed by last-32 In-Reply-To, case-exact and UTF-8 matching, self/cycles and late connecting arrival | M06/M08 |
| T04 | Restart/cache removal preserves thread assignment and order; failed authoritative lookup refuses creation; last-email deletion removes live thread but historical submissions retain their recorded ID | M08/M16 |
| Q01 | E1/E2 have equal receivedAt and E1 raw ID < E2; ascending and descending date sorts both retain E1,E2 tie order; before excludes equality, after includes it; maxSize excludes equality, minSize includes it | M14 |
| Q02 | From contains Alice, Subject contains Report: text `alice report` matches; subject `alice report` does not. Subject `Quarterly REPORT` matches quoted phrase `"quarterly report"`; `report quarterly` is a two-token match | M14 |
| Q03 | HTML `<head>secret</head><p title="note">hello &amp; world</p><script>hidden</script>` searches hello/world/note but not secret/hidden; bodyValues still contains all original markup | M06/M14 |
| Q04 | Independent AND/OR/NOT, all/some/none thread keyword predicates across folders, inMailboxOtherThan, header existence/value, attachments, all mailbox and submission filters; invalid versus unsupported syntax and complexity limits | M14/M16 |
| Q05 | Default Unicode simple-lowercase comparison, explicit octet/ASCII collations, ignored numeric collation, unsupported sort, duplicate comparator ties; query option advertisement equals implementation | M13/M14/M16 |
| Q06 | Tree order/filter ancestors; collapse before paging/total, absent anchor, negative positions/offsets, zero/default/oversize limit, one stable pinned view during concurrent mutations | M14/M16 |
| Q07 | Identical query/view/version -> equal state and IDs; account mutation or interpretation-version change changes queryState; queryChanges cannotCalculateChanges and canCalculateChanges false | M14/M16 |
| Q08 | Cache absent/stale, sort quota or work exhaustion never yields partial success/false total; discarded response tail and resource error preserve earlier methods | M13/M14 |
| Q09 | Subject `a < b & c` matching b -> `a &lt; <mark>b</mark> &amp; c`; snippets have no id/state; preview <=255 UTF-8 bytes after escaping/marking and no broken scalar/entity/tag; no positive text -> null/null | M14 |

## Endpoint and method coverage

Every row inherits the shared dimensions above. RFC numbers refer to the
standards linked by CONFORMANCE.md; test owners must link their executable
tests here without relabeling this unimplemented registry as passing evidence.

| ID | Surface / additional independent oracle | Standard / owner |
| --- | --- | --- |
| C01 | Discovery, auth, full Session/Account/capabilities; truthful all Core limits, sole account, revocation, fixed origins, HTTP smuggling and oversized framing | 8620 §§2,8 / M13 |
| C02 | Core/echo, complete envelope/defaults, capability opt-in, unknown method/capability, escaped call IDs, ordered responses and sessionState; I-JSON rejects surrogate/noncharacter strings, raw mail projects noncharacters with replacement while downloads preserve bytes | 8620 §§3,4; 7493 §2.1 / M06/M13 |
| C03 | Creation IDs, seeded/final createdIds, result-reference wildcard/flattening/name mismatch, invalidArguments versus invalidResultReference; first matching call ID including implicit response collision | 8620 §§3.3,3.4,3.7 / M13 |
| C04 | Request spool pressure before effects, later failure after earlier mutation, partial Set, known committed creations and implicit hooks after partial failure, physical spool fault closes response | 8620 §3.6.2 / M13-M16 |
| C05 | Upload response accountId/blobId/type/size, streaming/account/device/quota/expiry errors and downloaded exact bytes, safe filename/media headers and range policy | 8620 §§6.1,6.2 / M13-M15 |
| C06 | Blob/copy inaccessible source/destination, same-account allowed behavior, creation mapping and lease/quota/refusal | 8620 §6.3 / M15 |
| C07 | PushSubscription/get empty set/absent generic IDs, no accountId/state and default exclusion of url/keys; explicit sensitive properties forbidden | 8620 §7.2.1 / M13 |
| C08 | PushSubscription/set creation forbidden, absent update/destroy notFound, no URL contact and no standard account/state fields | 8620 §7.2.2 / M13 |
| C09 | EventSource authenticated StateChange type map, ping/closeafter/reconnect, two streams without view pins, fairness/output-stall/lifetime limits | 8620 §7.3 / M13 |
| B01 | Mailbox/get every property/right, default/projected values and correct counts after memberships/keywords/thread changes | 8621 §2.1 / M14 |
| B02 | Mailbox/changes exact atomic pages and updatedProperties; expired/foreign/future states cannotCalculateChanges | 8621 §2.2 / M14 |
| B03 | Mailbox/query all filters/tree/sorts and Mailbox/queryChanges refusal | 8621 §§2.3,2.4 / M14 |
| B04 | Mailbox/set create/rename/move/subscription, roles, Net-Unicode/size, hierarchy cycles/depth, mailboxHasChild/mailboxHasEmail and onDestroyRemoveEmails | 8621 §2.5 / M15 |
| R01 | Thread/get all live member IDs oldest first; Thread/changes created/updated/destroyed, empty-thread removal and state limits | 8621 §§3.1,3.2 / M14 |
| E01 | Email/get every stored/header/body property, defaults, all header forms and projections, bodyProperties/value flags; explicit metadata-only retrieval of opaque mail | 8621 §4.2 / M14 |
| E02 | Email/changes pages cannot split a committed frame, no-op/retention/restore states and indirect membership/keyword changes | 8621 §4.3 / M14 |
| E03 | Email/query all filters, supported/unsupported comparators, Q01-Q08; queryChanges refusal | 8621 §§4.4,4.5 / M14 |
| E04 | Email/set full structured create, immutable field errors, patch paths, keyword/membership edits/destroy, no mailbox, reserved queue blob lifetime, atomic per-object writes | 8621 §4.6 / M15 |
| E05 | Email/copy same account invalidArguments, unavailable source fromAccountNotFound, unavailable destination accountNotFound; no mutation or destroy-original hook can succeed in this single-account deployment | 8621 §4.7 / M15 |
| E06 | Email/import authorized raw blobs, receivedAt/keywords/shared memberships, malformed source, state/quota/expiry errors, raw bytes retained | 8621 §4.8 / M15 |
| E07 | Email/parse whole/part uploads, null id/mailboxIds/keywords/receivedAt/threadId policy, blobId/size, parsed/notParsable/notFound, defaults and all body options | 8621 §4.9 / M14-M15 |
| E08 | SearchSnippet/get actual matching subject/body and notFound, Q09, no state/id fields | 8621 §5.1 / M14 |
| I01 | Identity/get every configured field/default, Identity/changes equal/new configuration epoch, Identity/set read-only and missing-ID/state errors | 8621 §§6.1-6.3 / M15 |
| S01 | EmailSubmission/get every property/envelope/DeliveryStatus, absent replies omitted, uncertainty unknown, real SMTP reply retention, historical IDs and empty DSN/MDN arrays | 8621 §7.1 / M16-M17 |
| S02 | EmailSubmission/changes all attempt/retry/cancel/completion mutations with exact durable states | 8621 §7.2 / M16 |
| S03 | EmailSubmission/query every filter, sendAt/sentAt comparator equivalence and Q01/Q06/Q07; queryChanges refusal | 8621 §§7.3,7.4 / M16 |
| S04 | EmailSubmission/set authorized durable creation, #references, cancel races/oversize refusal, implicit Email/set and filing failure, delayed-send refusal, lost response after commit | 8621 §7.5 / M16 |
| S05 | QUEUE.md's complete state/phase/crash matrix, mixed RCPT results, provider pause/backoff, expiry/clock jumps, acceptance fence, uncertain outcomes and no known-accepted resend | 5321 / M16-M17 |
| X01 | Current td-mail compose/upload/send_draft/read/reply/flag/move/delete/download path through real binaries; captured bytes and restarted store agree; one opaque email cannot hide the rest of a listing, and its raw download/export remains available | M22 |
| X02 | Replace client inThread/500-result path with standard bounded Thread/get + Email/get, complete a thread larger than 256 emails | M14/M22 |
| X03 | More than 256 matching submissions; sought lost-response submission is on a later page; full paging/state handling before deciding absence | M16/M22 |

C05 range policy is full-body downloads only in v1: a Range request may be
ignored with a correct complete 200 response; never return an invented 206.
E07 returns threadId null, performing no speculative store assignment.
There is one account; cross-account cases assert the appropriate standard
authorization/no-such-account error, not a second supported configuration.

## Release mapping

M22 records the exact td-mail commit and production executable paths used;
mocks remain unit evidence. M24 records each ID, test command, independent
oracle, positive/refusal/restart result and outstanding mandatory members.
Missing tooling or capabilities fails that release evidence rather than being
silently skipped. Structural fixture presence, JSON validity and documentation
link checks do not prove an endpoint implements any case.

## Additional literal edge cases

These extend the existing case IDs, without substituting for their other
obligations. JSON spelling below denotes decoded text unless called raw bytes.

- **M02:** QP bytes `ab= \t\r\ncd` decode to `abcd` without an encoding
  problem. `ab=\ncd` gives the same bytes with a problem. `ab=` and
  `ab= \t` at part EOF both decode to `ab` with a problem. A part ending
  `ab=\r\n--X` has the boundary CRLF excluded from its extent; the resulting
  `ab=` still decodes to `ab`.
- **M05:** raw bytes
  `Content-Type: multipart/mixed; boundary=X\r\n\r\n--Xjunk\r\n\r\nA\r\n--X--\r\n`
  have one identity leaf of exactly hex 41. Bare-LF spelling of this whole
  fixture gives the same leaf byte, removing one delimiter LF. In contrast,
  a leaf `A\r--X\rZ` before the actual CRLF delimiter retains all those
  bytes: bare CR never begins a delimiter. If an outer X contains a child
  declaring multipart boundary Xy, `--Xy` matches the outer X first; that
  child has no opening boundary and the complete parse is notParsable.
- **M06:** alternative(mixed(html X, plain Y)), with both leaves lacking
  disposition/filename, gives textBody=[X], htmlBody=[X], attachments=[Y],
  hasAttachment=true. The body-list algorithm and the final attachment
  definition are separate steps; the suggested algorithm's intermediate
  attachment accumulator is not authoritative.
- **T03:** field 1 has a complete valid first ID of 1005 bytes; field 2 is
  `<small@x>`. Store no anchor, never small@x. A new non-reply message with
  `<a@x>` joins the smallest live a@x anchor's thread after reply candidates
  miss. In one ordered creation frame, E2 referencing E1's newly inserted
  anchor joins E1's thread; E1 cannot see E2 before its own creation.
- **Q04:** AND([])=true, OR([])=false, NOT([])=true; empty quoted phrase
  matches the empty token set. Unquoted `O'Brien` is one literal token.
  An unmatched opening quote in `"draft` is literal. A complete quoted phrase
  with an unescaped backslash is unsupportedFilter, while an unquoted path
  containing a backslash is literal. A missing field never satisfies
  header:[name,value], even if value is empty; header:[name] tests existence.
- **Q05:** a header Text value U+00E9 matches operand U+00E9 but not
  U+0065 U+0301; a body value U+0065 U+0301 matches that decomposed operand
  but not U+00E9. Unknown explicit string collation gives unsupportedSort;
  that same collation on receivedAt is ignored. U+212A lowercases to U+006B
  without treating its three UTF-8 bytes as three matcher characters.
- **Q06:** request limit 1000 under query_page=256 returns at most 256 IDs
  and response limit=256. Omitted limit also reports 256. Explicit limit=0
  returns [] but calculateTotal=true still reports the exact result count.
- **Q03/Q09:** `<span>Hel</span>lo&nbsp;world &rsquo;` extracts
  `Hello world &rsquo;`. The unsupported named entity remains literal; snippet
  escaping consequently represents its ampersand as `&amp;`. This is a known
  heuristic limit, not a claim of a complete HTML named-entity decoder.
- **E05:** fromAccountId=A, accountId=A produces invalidArguments and no
  implicit Email/set. A valid unavailable source ID with accountId=A gives
  fromAccountNotFound; an unavailable destination gives accountNotFound.
- **E06:** top Received trace dated 2024-01-02 followed by an older trace
  claiming 2024-02-01 defaults receivedAt to 2024-01-02. If the top trace's
  timestamp is malformed, the next valid trace wins. No valid trace uses
  the injected import clock; an explicit receivedAt overrides all traces.
