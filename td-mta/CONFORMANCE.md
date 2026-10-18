# JMAP implementation inventory

The supported client is td-mail. Its current call inventory below defines
required JMAP behavior; generic client interoperability and unused RFC methods,
properties and argument combinations are outside the implementation scope.
The broader tables remain a protocol reference, not an implementation backlog
or a gate on the td-mail milestone. Existing implemented behavior stays tested.

The initial read milestone uses real td-mail over real td-fetch and td-mta: authenticate,
list mailboxes and messages, read plain and HTML mail, follow threads, and
download original messages and attachments. Search, flags, moves, deletion and
sending follow the client's actual calls; they need not block the initial
read-mail fixture. Authorization, durable outcomes, bounded work and honest
errors remain mandatory in every supported path. Do not report absent body
content as successful empty data to make a client test pass.

The mailbox-access profile advertises Core and the td-mail Mail profile.
Authenticated
Session discovery, bounded Core and Mail read batches, upload/download, the
single-account Blob/copy outcome and PushSubscription policy are active.
Email/set read/unread, star patches and single-destination mailbox replacement
and permanent deletion are active. Structured text/attachment creation and
Submission are active for the supported sending flow described below. The native td-mail fixture sets and clears
both flags over HTTPS and checks persisted keyword rows after each result.
It creates archive/trash folders through td-mail, refuses a duplicate name,
then archives, trashes and restores messages using those returned folders,
and checks native membership after each operation. Revoked credentials
leave queued moves unapplied; restarting td-mail with replacement credentials
replays its durable flag and archive intentions. The creation profile is
name-only root folders. Singleton deletion removes empty roleless leaves;
hierarchy creation and rename remain absent.
EventSource emits pings only; Mail StateChange integration remains absent and
is not used by td-mail. Activation follows
the supported td-mail workflow tests, not every reference-table row.
M22/M24 still own real-client and complete release coverage. API.md and
QUEUE.md fix state/error and submission policies.
[POLICY.md](POLICY.md) fixes interpretation, query and thread choices;
[CASES.md](CASES.md) maps every surface below to stable acceptance case IDs.

Normative references: [RFC 8620](https://www.rfc-editor.org/rfc/rfc8620.html)
and [RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html). Section numbers in
the tables refer to those RFCs. Property names are case-sensitive wire names.
Required properties include nullable and read-only properties; a nullable
type does not permit hiding an unimplemented non-null case behind `null`.

## Capabilities and session

V1 targets `urn:ietf:params:jmap:core`, `urn:ietf:params:jmap:mail` and
`urn:ietf:params:jmap:submission` as the wire vocabulary used by td-mail.
Enable each surface only with tested support for its intended td-mail flows;
this is a td-mail compatibility profile, not a claim of full RFC conformance. VacationResponse and vendor extensions are omitted. The Core
recommended minimum limits are recommendations; publish our actual bounded
limits, including 32 MiB uploads and 256-object get/set windows.

| Surface | Required inventory | Reference | Owner / fixture obligation |
| --- | --- | --- | --- |
| Discovery/authentication | HTTPS Session resource and well-known discovery; authenticated, fixed-origin URLs | 8620 §§2, 2.2, 8.1-8.3 | M13: unauthenticated and cross-origin cases |
| Session | `capabilities`, `accounts`, `primaryAccounts`, `username`, `apiUrl`, `downloadUrl`, `uploadUrl`, `eventSourceUrl`, `state` | 8620 §2 | M13: complete discovery object |
| Account | `name`, `isPersonal`, `isReadOnly`, `accountCapabilities` | 8620 §2 | M13: sole account and denied foreign IDs |
| Core limits | `maxSizeUpload`, `maxConcurrentUpload`, `maxSizeRequest`, `maxConcurrentRequests`, `maxCallsInRequest`, `maxObjectsInGet`, `maxObjectsInSet`, `collationAlgorithms` | 8620 §2 | M13: published limit equals enforced limit |
| Mail account capability | `maxMailboxesPerEmail`, `maxMailboxDepth`, `maxSizeMailboxName`, `maxSizeAttachmentsPerEmail`, `emailQuerySortOptions`, `mayCreateTopLevelMailbox` | 8621 §1.3.1 | M14/M15: names permit at least 100 UTF-8 bytes; decoded attachment and encoded message limits remain distinct |
| Submission account capability | `maxDelayedSend`, `submissionExtensions` | 8621 §1.3.2 | M16/M17: delayed send disabled; truthful extension policy |
| Request/response envelope | `using`, ordered `methodCalls`/`methodResponses`, invocation IDs, `createdIds`, `sessionState` | 8620 §§3.2-3.4 | M13: ordered partial success and capability opt-in |
| Shared method behavior | account authorization, defaults, unknown arguments, creation/result references, error types, state checks, set patches and per-object outcomes | 8620 §§3.5-3.10, 5.1-5.6 | M13-M16: earlier successes survive later errors |

POLICY.md pins supported collation/search behavior and parser/query limits;
ADMISSION.md and RESOURCES.md bound storage, work and resident capacity.
Unsupported optional behavior gets the standard error where permitted;
unimplemented calls receive explicit errors. Unused generic behavior is not
a release requirement for this client-specific profile.
[WIRE.md](WIRE.md) fixes the wire-ID mapping: generated IDs use type letters
plus canonical storage hex; MIME parts use distinct checked locators. The
parser separately accepts the full generic RFC 8620 §1.2 Id grammar, so valid
absent IDs receive the proper notFound outcome. Implemented codecs do not
provide endpoint or authorization evidence.

## Methods and HTTP endpoints

For implemented methods, preserve the wire response members and semantics
used by td-mail. The full RFC method inventory below is reference material. Shared `/get`, `/changes`, `/set`, `/copy`, `/query` and
`/queryChanges` arguments/results follow RFC 8620 §5, with each type's stated
exceptions. Tests must cover omitted arguments, nonexistent IDs, invalid
properties, conditional states, boundary limits and unknown account IDs.

| Method / endpoint | Reference | Owner | Required acceptance case |
| --- | --- | --- | --- |
| `Core/echo` | 8620 §4 | M13 | Arbitrary valid arguments returned within request/output bounds |
| Upload HTTP endpoint | 8620 §6.1 | M13/M15 | Streaming quota, content type, account, blob ID and size response |
| Download HTTP endpoint | 8620 §6.2 | M13/M14 | Account authorization, URL templates, bytes and media/filename handling |
| `Blob/copy` | 8620 §6.3 | M15 | Distinct-account copy contract: same accessible account is `invalidArguments`; unavailable destination/source is `accountNotFound`/`fromAccountNotFound`; successful cross-account copy is unreachable in v1 |
| `PushSubscription/get` | 8620 §7.2.1 | M13 | Empty actual registration set; requested absent IDs reported correctly; explicit `url` or `keys` projection gets method-level `forbidden`, even for an empty set |
| `PushSubscription/set` | 8620 §7.2.2 | M13 | Registration creation refused as `forbidden`; no outbound URL request; absent update/destroy IDs use `notFound` |
| EventSource HTTP endpoint | 8620 §7.3 | M13 | StateChange event, type filtering, ping, closeafter, authenticated reconnect; no held read view |
| `Mailbox/get` | 8621 §2.1 | M14 | Full/default and projected properties |
| `Mailbox/changes` | 8621 §2.2 | M14 | Created/updated/destroyed IDs and `updatedProperties` |
| `Mailbox/query` | 8621 §2.3 | M14 | Tree options, parent/name/role/subscription filtering |
| `Mailbox/queryChanges` | 8621 §2.4 | M14 | Correct delta or permitted `cannotCalculateChanges` |
| `Mailbox/set` | 8621 §2.5 | M15 | Cycles, role constraints, child/email deletion errors and `onDestroyRemoveEmails` |
| `Thread/get` | 8621 §3.1 | M14 | Live email IDs in required order, immutable grouping |
| `Thread/changes` | 8621 §3.2 | M14 | Membership changes produce the right changed thread IDs |
| `Email/get` | 8621 §4.2 | M14 | Header forms, part projection and body-value flags/byte limits |
| `Email/changes` | 8621 §4.3 | M14 | Retention floor, atomic state, bounded pages |
| `Email/query` | 8621 §4.4 | M14 | Filter operators, declared sort, anchors/positions, totals and thread collapse |
| `Email/queryChanges` | 8621 §4.5 | M14 | Stable state/delta with collapse, or permitted `cannotCalculateChanges` |
| `Email/set` | 8621 §4.6 | M15 | Structured creation, immutable-property refusal, keyword/membership patches, destroy |
| `Email/copy` | 8621 §4.7 | M15 | Same-account invalidArguments and unavailable-account errors; successful cross-account copy is unreachable in v1 |
| `Email/import` | 8621 §4.8 | M15 | Raw authorized blob, folders, dates, keywords and malformed input |
| `Email/parse` | 8621 §4.9 | M14/M15 | Authorized uploaded/part blobs, `parsed`, `notParsable`, `notFound` |
| `SearchSnippet/get` | 8621 §5.1 | M14 | Actual matches, escaping/highlighting, absent email IDs |
| `Identity/get` | 8621 §6.1 | M15 | Complete configured identity values |
| `Identity/changes` | 8621 §6.2 | M15 | Configuration changes reflected in identity state |
| `Identity/set` | 8621 §6.3 | M15 | Read-only policy using specified refusal, missing-ID and state errors |
| `EmailSubmission/get` | 8621 §7.1 | M16 | Queue state mapped to every standard property |
| `EmailSubmission/changes` | 8621 §7.2 | M16 | Retry/completion/cancel changes after restart |
| `EmailSubmission/query` | 8621 §7.3 | M16 | Identity/time and other standard filters, sort/pagination |
| `EmailSubmission/queryChanges` | 8621 §7.4 | M16 | Correct changes or permitted `cannotCalculateChanges` |
| `EmailSubmission/set` | 8621 §7.5 | M16 | Durable creation, cancel, `onSuccessUpdateEmail`/`onSuccessDestroyEmail`, implicit Email/set response |

Generic query support includes `position`, `anchor`, `anchorOffset`, `limit`,
`calculateTotal`, filters and comparators. Responses include the appropriate
state, IDs, position, change-calculation capability and requested total.
Changes include `hasMoreChanges` and precise state boundaries; expired history
does not yield fabricated empty success. QueryChanges can explicitly refuse
an uncomputable delta as the standard allows, independently of normal query.

PushSubscription methods are credential-scoped, with no `accountId` argument
or response member. `/get` has no response `state`; omitted/null `properties`
excludes `url` and `keys`. `/set` has no `ifInState`, `oldState` or `newState`.
M13 fixtures must verify these exceptions to the shared envelopes. Requested
absent IDs use the generic JMAP ID syntax and are echoed as absent; they need
not match the encoding of locally generated object IDs.

## Object/property inventory

| Type or property group | Required members | Reference / owner |
| --- | --- | --- |
| Mailbox | `id`, `name`, `parentId`, `role`, `sortOrder`, `totalEmails`, `unreadEmails`, `totalThreads`, `unreadThreads`, `myRights`, `isSubscribed` | 8621 §2 / M14-M15 |
| MailboxRights | `mayReadItems`, `mayAddItems`, `mayRemoveItems`, `maySetSeen`, `maySetKeywords`, `mayCreateChild`, `mayRename`, `mayDelete`, `maySubmit` | 8621 §2 / M14-M15 |
| Thread | `id`, `emailIds` | 8621 §3 / M14 |
| Email metadata | `id`, `blobId`, `threadId`, `mailboxIds`, `keywords`, `size`, `receivedAt` | 8621 §4.1.1 / M14-M15 |
| Email headers | `headers`; parameterized `header:NAME`, parsed form and `:all`; `messageId`, `inReplyTo`, `references`, `sender`, `from`, `to`, `cc`, `bcc`, `replyTo`, `subject`, `sentAt` | 8621 §§4.1.2-4.1.3 / M06/M14-M15 |
| Header forms | Raw, Text, Addresses, GroupedAddresses, MessageIds, Date, URLs; validate allowed form/header combinations | 8621 §4.1.2 / M06 |
| EmailHeader / EmailAddress / EmailAddressGroup | `name`, `value` / `name`, `email` / `name`, `addresses` | 8621 §§4.1.2-4.1.3 / M06 |
| Email body aggregate | `bodyStructure`, `bodyValues`, `textBody`, `htmlBody`, `attachments`, `hasAttachment`, `preview` | 8621 §4.1.4 / M06/M14 |
| EmailBodyPart | `partId`, `blobId`, `size`, `headers`, parameterized header properties, `name`, `type`, `charset`, `disposition`, `cid`, `language`, `location`, `subParts` | 8621 §4.1.4 / M06/M14-M15 |
| EmailBodyValue | `value`, `isEncodingProblem`, `isTruncated` | 8621 §4.1.4 / M06/M14 |
| SearchSnippet | `emailId`, `subject`, `preview` | 8621 §5 / M14 |
| Identity | `id`, `name`, `email`, `replyTo`, `bcc`, `textSignature`, `htmlSignature`, `mayDelete` | 8621 §6 / M15 |
| EmailSubmission | `id`, `identityId`, `emailId`, `threadId`, `envelope`, `sendAt`, `undoStatus`, `deliveryStatus`, `dsnBlobIds`, `mdnBlobIds` | 8621 §7 / M16-M17 |
| Envelope / Address | `mailFrom`, `rcptTo` / `email`, `parameters` | 8621 §7 / M16-M17 |
| DeliveryStatus | `smtpReply`, `delivered`, `displayed` | 8621 §7 / M16-M17 |
| PushSubscription | `id`, `deviceClientId`, `url`, `keys`, `verificationCode`, `expires`, `types`; no accepted registrations in v1 | 8620 §7.2 / M13 |
| StateChange | `@type`, `changed` account/type state map | 8620 §7.1 / M13-M16 |

Email/get and Email/parse also cover `bodyProperties`, `fetchTextBodyValues`,
`fetchHTMLBodyValues`, `fetchAllBodyValues`, `maxBodyValueBytes`; Email/parse
observes its explicit exclusions from stored Email metadata. Email/import
preserves `blobId`, `mailboxIds`, `keywords`, `receivedAt` under state checks.
Each nullable/default/read-only rule needs its own oracle in the owning task.

Filters to inventory in M02's wire fixtures:

- Mailbox: `parentId`, `name`, `role`, `hasAnyRole`, `isSubscribed`, plus
  `sortAsTree` and `filterAsTree` options (8621 §2.3).
- Email: `inMailbox`, `inMailboxOtherThan`, `before`, `after`, `minSize`,
  `maxSize`, `allInThreadHaveKeyword`, `someInThreadHaveKeyword`,
  `noneInThreadHaveKeyword`, `hasKeyword`, `notKeyword`, `hasAttachment`,
  `text`, `from`, `to`, `cc`, `bcc`, `subject`, `body`, `header` (8621 §4.4.1).
- Submission: `identityIds`, `emailIds`, `threadIds`, `undoStatus`, `before`,
  `after` (8621 §7.3).
- Shared filter operators: AND/OR/NOT with bounded nesting (8620 §5.5).

Required sort comparators are Mailbox `sortOrder` and `name`, Email
`receivedAt`, and EmailSubmission `emailId`, `threadId` and `sentAt` (8621
§§2.3, 4.4.2, 7.3). The last spelling is exactly as printed in §7.3, although
the submission object property is `sendAt`; QUEUE.md supports both comparator
spellings against immutable sendAt, without adding an object property. POLICY.md
selects additional comparators and collation behavior, with standard
`unsupportedFilter`/`unsupportedSort`
errors (8620 §5.5). ReceivedAt ascending/descending with an ID tie break is
also required by td-mail. Comparators and query forms unused by td-mail do
not gate the client milestone.

## Deliberate v1 interpretation limits

POLICY.md specifies these choices and their independent CASES.md oracles:

- Support the required receivedAt sort and recommended size sort. The other
  RFC 8621 section 4.4.2 recommended Email comparators are deferred to keep
  comparator/index work small for the initial client. Return unsupportedSort,
  and do not advertise them. This is a deliberate choice against those SHOULD
  recommendations, not a claim that they are implemented.
- Search has simple lowercase matching, with no canonical-equivalence or
  full-case-folding promise. Header Text's mandatory NFC and bodyValues'
  original decoded scalar spelling remain distinct. Fixtures pin both.
- Charset aliases and HTML named entities use the finite POLICY.md sets.
  Unknown labels/references follow its explicit fallback. Original bodies
  remain downloadable; no full charset registry or HTML renderer is claimed.
- Accepted mail may exceed interpretation limits. Default Email/get batches
  can fail, while explicit stored-metadata/top-level-header projections and
  raw download remain available as applicable. M14/M22 must implement td-mail's
  bounded isolation/placeholder recovery and show content-search failure
  honestly. Third-party MUAs without that recovery may not display such mail.
  SearchSnippet uses its standard per-email null/null fallback.
- Email/copy cannot succeed with one account: RFC 8620 requires a distinct
  source and destination. Test invalidArguments/fromAccountNotFound/
  accountNotFound; never invent a same-account successful copy or run an
  implicit destroy. Blob/copy's separate contract remains supported.

Read the exact policies before interpreting a missing feature as success.
Unused methods in the reference inventory may remain unimplemented.

## Current td-mail call inventory

Rechecked against `td-mail/src/jmap/client.rs` and `types.rs` at
`2a8165330de469758b0771ca841a9fea5afafd58`. Paths below refer to the same checkout, not a frozen external client.
Recheck them when implementing M14-M16 and M22.

| Client source | Request/response behavior | Acceptance owner |
| --- | --- | --- |
| `td-mail/src/jmap/client.rs`: `discover`; `types.rs`: `JmapSession` | HTTPS Basic, fixed-origin discovery, mail account, capability maps, upload/download templates, upload size and submission availability | M13/M22 |
| `get_mailboxes`, `create_mailbox`, `delete_mailbox` | Mailbox/get and Mailbox/set create/destroy | M14-M15/M22 |
| `query_emails`, `query_emails_uncollapsed` | Email/query: inMailbox, AND, text, after/before, receivedAt descending, collapseThreads false, position/limit | M14/M22 |
| `get_emails`, `get_emails_with_extra_properties`, `get_emails_for_rules` | Email/get projection, header properties, text/HTML body-value requests; rule reads omit bodies | M06/M14/M22 |
| `get_email_for_reply` | Email/get messageId, references, replyTo and body context | M06/M14/M22 |
| `mark_emails_read`, `mark_email_read`, `mark_email_unread`, `set_email_flagged`, `move_email`, `destroy_emails` | Email/set patches for keywords/mailboxIds and destroy; per-object results | M15/M22 |
| `get_threads`, `query_thread_emails` | Thread/get membership followed by Email/get batches of 50 | M14/M22 |
| `get_email_raw`, `download_blob` | Email/get blobId and template download; attachments use part blob IDs | M06/M14/M22 |
| `get_identities`, `upload_blob` | Identity/get; upload HTTP request with MIME type and bounded response blobId | M13/M15/M22 |
| `submit_email` | Email/set create under `draft`, then EmailSubmission/set referencing `#draft`; `#submission` success patch removes $draft and files Sent; client checks the implicit Email/set result under call ID 1 separately | M15-M16/M22 |
| `submissions_between`, `get_email_filing` | IdentityIds/after/before submission query, submission get id/emailId, then Email/get id/keywords/mailboxIds/messageId for lost-response reconciliation | M16/M22 |
| `td-mail/src/submit.rs`, `compose.rs` | Structured MIME fields/bodyValues/attachments, Bcc and stable Message-ID for reconciliation | M06/M15-M16/M22 |

The normal Email/get projection includes id/threadId/from/to/cc/subject,
receivedAt/preview/textBody/htmlBody/bodyValues/keywords/mailboxIds/attachments.
Rules and replies add messageId/references/replyTo and requested custom header
properties. Preserve omission/null distinctions and body decoding diagnostics.

Existing tests in `td-mail/tests/cli_integration.rs` cover connect/mailboxes,
query/date filters, read/unread, triage/move/delete, download and offline replay.
`test_send_draft_submits_through_jmap_and_retires_the_draft` covers identity,
upload/attachments, creation/submission, filing failure, lost replies, duplicate
Message-ID copies, and explicit refusal. These use mocks and are evidence of
client expectations only. M22 repeats the flows against real td-mta/td-fetch.

### Compatibility gaps discovered by M01

`query_thread_emails` uses Thread/get membership followed by Email/get
batches of 50. The real-server fixture must still prove a thread larger than
256 messages. Audit other client get/set batches against the advertised
256-object limit before activating those flows.

`submissions_between` completes a bounded window of at most 4096 submissions
using explicit 256-ID query pages, stable query state and totals, and 50-ID
get batches. Missing, malformed, duplicate or changing results preserve an
unknown send outcome. Filing reads use 50-ID batches and require complete
coverage. The client mock lost-reply fixture places the sought submission
beyond 300 preceding matches. M16/M22 must repeat this against real td-mta
before activation. The fallback Drafts/Sent Email/query path uses the same complete bounded
pagination and gathers both recorded folders before considering draft
destruction. Its lost-reply fixture places a sent copy behind 300 newer
messages while a matching draft remains in the recorded Drafts folder.

POLICY.md adds a third client acceptance gap: Email/get's method-level failure
on one opaque message must not hide a whole listing. M14/M22 add bounded
metadata fallback, per-ID isolation and raw download/export, without inventing
empty successful body data. A failing body search remains a visible failure.

## Release evidence

For each table row, the owning increment records the fixture/test and its
positive, refusal and restart observations. CASES.md supplies a traceable case
ID for each property group and exceptional wire mapping; later implementation
must split grouped rows when their coverage differs. Capability enablement
requires the corresponding supported client flows, including refusal paths.
Unused reference rows do not gate enablement. External
push registration refusal, read-only identities and uncomputable query changes
are real policy/error tests, never missing handlers disguised as conformance.

## Core Blob/copy worker coverage

The retained Core dispatcher implements RFC 8620 §6.3 for the single-account
profile. It validates the required `fromAccountId`, `accountId` and `blobIds`,
rejects unknown arguments, and walks the blob array under the retained-source
work and deadline bounds. Generic valid foreign IDs remain valid inputs.
The get/set object limit does not apply to Blob/copy: §6.3 defines no such
method limit or requestTooLarge error. Duplicates do not allocate an index.
After validation, unavailable destination and source accounts receive
`accountNotFound` and `fromAccountNotFound`, respectively; destination takes
precedence when both are unavailable. Copying within the sole accessible
account is `invalidArguments`: this profile interprets §6.3's distinct-account
wording consistently with §5.4's explicit rule and Cyrus's copy parser. No distinct accessible pair exists in this profile, so no successful
copy or per-blob mutation is reachable. Tests cover result references,
capability opt-in, phase expiry and continued later calls. The storage-independent worker
is integrated into the live Core runtime described below.

## Core runtime evidence

The HTTPS fixture exercises discovery metadata against the prepared Session,
origin-form and absolute-form authority, fixed/chunked Core requests, ordered
responses/references/problems, upload publication and raw downloads. Event
coverage includes requested pings, ping=0 silence, unknown reconnect IDs,
connection-limit refusal, simultaneous API traffic, revocation and shutdown.
The passive EventSource tests exercise StateChange coalescing, filtering,
last-event-ID replay avoidance and closeafter=state. Core exposes no stateful
account data type. The live stream still produces pings only; this read
profile does not claim Mail StateChange events. The real-client fixture below
qualifies the request-driven td-mail flow independently of passive SSE tests.
td-mail does not consume EventSource; it refreshes on its configured idle-sync
interval (60 seconds by default) and on manual refresh. Live Mail events
remain deferred.

## Mailbox query availability

The PostgreSQL dispatcher currently returns unknownMethod for Mailbox/query
and Mailbox/queryChanges. The removed local-store query tests do not qualify
this backend. Mailbox/get supplies the mailbox discovery used by td-mail.

## Native real-client gate

The native crypto-cargo wrapper builds this checkout's td-mail and td-net
fetch test executables. It requires the PostgreSQL read/download, plain-text
send and actual foreground-service fixtures to pass. Set
`TD_MTA_TEST_POSTGRES_BIN` to an absolute directory containing PostgreSQL 17
`initdb`, `postgres`, `pg_ctl` and `psql`. Each fixture owns its disposable
cluster and process cleanup; it contacts no public endpoint.

The private CA enters only the existing fetch test process. This establishes
client/server compatibility for the flows described below, not production
root-set, managed-provider, maximum-load or full release qualification.

## Structured Email creation evidence

The PostgreSQL real-client send fixture below exercises text and attachment
draft creation, submission history and success filing through the prepared
HTTPS backend. The removed local-store draft/device-policy fixtures are not
qualification for this backend. The foreground service advertises Submission and runs relay scheduling for
relay profiles.
Shared creation-ID and response-envelope tests continue to cover bounded
encoding, seed replacement, partial results and output failures. Attachment
limits and native sending evidence are described below.

## Prepared PostgreSQL real-client fixture

The ignored `real_postgres_td_mail_reads_and_downloads` fixture requires
local PostgreSQL 17 tools plus the native-built td-mail and isolated td-fetch
test executables. It creates an empty database, publishes database profiles
and TLS materials, delivers a multipart message under the cloud profile and
serves the same account through the desktop profile's prepared HTTPS backend.
The fixture drives discovery, mailbox listing, Email/query and Email/get,
body display, explicit Thread/get, read/unread and star/unstar updates,
root-folder creation and duplicate refusal, empty-folder deletion and
protected/nonempty/parent/missing-folder refusals, moves between folders,
permanent Email deletion,
decoded attachment download and raw-message download with the real client. Authentication and API/download work run on the fixed HTTPS
workers. Its private CA enters only the existing test-only fetch process.

This PostgreSQL fixture is part of the native gate. It does not qualify
cloud-provider failover or managed-database deployment.
The separate send and executable-service cases cover their named flows. Process and file cleanup belong to
the fixture; no public endpoint is contacted.

The separate `real_postgres_http_drafts_create_read_and_settle` fixture drives
the prepared HTTP body adapter with the td-mail structured plain-text shape.
It checks creation, stale-state refusal, body retrieval, raw download and
delayed-COMMIT receipt settlement. It does not exercise td-mail sending or
advertise Submission.

The separate `real_postgres_td_mail_sends_plain_text` fixture uses the prepared
Submission Session and the real td-mail/fetch processes. It covers text and attachment
sending, identity discovery, folder creation, draft/submission/filing replies,
stored message retrieval and retained-send reconciliation after failed local
draft retirement. The second client attempt must return the original
submission ID with exactly one queued submission. Binary and forwarded-message
attachments retain their octets through upload, composition, stored retrieval
and download, including a UTF-8 filename. Expired and other-device upload
leases cannot publish a draft. This prepared fixture does not deliver via SMTP;
the foreground-service fixture below covers executable SMTP delivery.


The prepared PostgreSQL HTTP fixture also receives length-delimited and
chunked uploads, downloads their blobs under current device authorization,
refuses malformed/oversized input, and settles a delayed COMMIT without a
second publication. A separate real HTTPS test proves an uncertain upload
closes only its exchange without a final receipt and releases its body slot
for another request. The native sending fixture above qualifies td-mail attachment composition.


The native PostgreSQL foreground-service fixture uploads a binary attachment and submits a structured draft
and EmailSubmission through the executable's HTTPS endpoint to its separate
relay profile. Its local TLS smart host refuses the first MAIL with 451;
the fixture restarts the relay, delivers fresh mail during the retry hold,
delivers a verified eight-bit fixture source using 8BITMIME and checks route
backoff reset, then observes the original message delivered after the real five-minute
startup guard and persisted retry deadline. It checks durable recipient
states and actual DATA receipt, including the binary attachment encoding. It confirms the stable clock through local
control, then kills the process during a later MAIL and checks that a new
owner recovers Prepared after the abandoned lease expires. That interrupted
message has expired meanwhile; recovery records a non-dispatchable expiry-wait
marker while terminal expiry remains held for clock confirmation. This qualifies the bounded plain-text path,
not maximum-size throughput or every SMTP failure interleaving.


## Local delivery-failure notices

The foreground PostgreSQL fixture covers terminal expiry and permanent MAIL
rejection producing unread Inbox notices. Email/get returns their decoded
plain-text report, including local expiry reasons and actual retained SMTP
replies. An injected local storage refusal leaves notification Pending and
creates no partial Email. A deferred COMMIT longer than the notice deadline
exercises an uncertain response; restart observes one Stored notice. Deleting
that Email through JMAP and restarting does not regenerate it or replace its
historical notificationEmail ID. Unit cases cover uncertain outcome wording,
peer-control sanitization, all 1000 maximum-size recipient details and finite
database framing allowances. This is local account mail, never an outbound
bounce or a claim that SMTP relay acceptance proves mailbox delivery.

The live PostgreSQL HTTP adapter supports singleton submission cancellation
through `EmailSubmission/set` (`undoStatus: "canceled"`). It refuses the whole
update if any recipient is accepted, uncertain or past the durable
AcceptancePossible boundary. Native fixtures cover separate frontend/relay
processes, queued and active cancellation, repeat/restart, lost COMMIT
settlement and continued service. This is cancellation of eligible remaining
work, not recall of delivered mail; mixed/multiple updates remain refused.

Structured attachment creation accepts at most 64 local blob IDs, each with
an attachment disposition, a nonempty filename of at most 255 UTF-8 bytes,
a media type of at most 127 bytes, and an optional description of at most
1024 UTF-8 bytes. Multipart sources and message subtypes other than rfc822
and global are refused. Forwarded message/rfc822 bytes must have SMTP-safe
CRLF lines (at most 998 content octets, no NUL); they are preserved exactly.
Ordinary attachment octets use base64. Aggregate decoded input and encoded
message each obey the selected message-size ceiling, capped at 32 MiB.
MIME-part locators must first be downloaded/uploaded, as td-mail does; they
are not local upload IDs. HTML draft composition remains outside this flow.
