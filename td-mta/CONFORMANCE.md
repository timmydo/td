# JMAP implementation inventory

This is the M01 coverage inventory for td-mta. Every endpoint, method and
property below is **unimplemented**. The library skeleton advertises no JMAP
capability. Tables name acceptance obligations and owning tasks, not passing
tests. M02 freezes wire/error fixtures; M13-M17 implement them; M22/M24 prove
real-client compatibility and standards coverage before release.

Normative references: [RFC 8620](https://www.rfc-editor.org/rfc/rfc8620.html)
and [RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html). Section numbers in
the tables refer to those RFCs. Property names are case-sensitive wire names.
Required properties include nullable and read-only properties; a nullable
type does not permit hiding an unimplemented non-null case behind `null`.

## Capabilities and session

V1 targets `urn:ietf:params:jmap:core`, `urn:ietf:params:jmap:mail` and
`urn:ietf:params:jmap:submission`. Their full associated contracts gate
advertisement. VacationResponse and vendor extensions are omitted. The Core
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

M02 must pin supported collation/search behavior and all variable-length
limits. Unsupported optional behavior gets the standard error where permitted;
mandatory properties/methods cannot disappear behind a client-only subset.
[WIRE.md](WIRE.md) fixes the wire-ID mapping: generated IDs use type letters
plus canonical storage hex; MIME parts use distinct checked locators. The
parser separately accepts the full generic RFC 8620 §1.2 Id grammar, so valid
absent IDs receive the proper notFound outcome. Implemented codecs do not
provide endpoint or authorization evidence.

## Methods and HTTP endpoints

All listed methods require their standard response members as well as the
properties below. Shared `/get`, `/changes`, `/set`, `/copy`, `/query` and
`/queryChanges` arguments/results follow RFC 8620 §5, with each type's stated
exceptions. Tests must cover omitted arguments, nonexistent IDs, invalid
properties, conditional states, boundary limits and unknown account IDs.

| Method / endpoint | Reference | Owner | Required acceptance case |
| --- | --- | --- | --- |
| `Core/echo` | 8620 §4 | M13 | Arbitrary valid arguments returned within request/output bounds |
| Upload HTTP endpoint | 8620 §6.1 | M13/M15 | Streaming quota, content type, account, blob ID and size response |
| Download HTTP endpoint | 8620 §6.2 | M13/M14 | Account authorization, URL templates, bytes and media/filename handling |
| `Blob/copy` | 8620 §6.3 | M15 | Authorized blob copying and defined errors for inaccessible source/destination |
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
| `Email/copy` | 8621 §4.7 | M15 | Copy permissions, source errors and destroy-original semantics |
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
the submission object property is `sendAt`; M02 must resolve that discrepancy
in its wire fixtures before implementation. M02 selects additional comparators
and collation behavior, with standard `unsupportedFilter`/`unsupportedSort`
errors (8620 §5.5). ReceivedAt ascending/descending with an ID tie break is
also required by td-mail. Required standards behavior remains a release gate
even where the current client does not exercise it.

## Current td-mail call inventory

Inspected at repository commit `51e843f78` (full baseline recorded in the M01
commit). Paths below refer to the same checkout, not a frozen external client.
Recheck them when implementing M14-M16 and M22.

| Client source | Request/response behavior | Acceptance owner |
| --- | --- | --- |
| `td-mail/src/jmap/client.rs`: `discover`; `types.rs`: `JmapSession` | HTTPS Basic, fixed-origin discovery, mail account, capability maps, upload/download templates, upload size and submission availability | M13/M22 |
| `get_mailboxes`, `create_mailbox`, `delete_mailbox` | Mailbox/get and Mailbox/set create/destroy | M14-M15/M22 |
| `query_emails`, `query_emails_uncollapsed` | Email/query: inMailbox, AND, text, after/before, receivedAt descending, collapseThreads false, position/limit | M14/M22 |
| `get_emails`, `get_emails_with_extra_properties`, `get_emails_for_rules` | Email/get projection, header properties, text/HTML body-value requests; rule reads omit bodies | M06/M14/M22 |
| `get_email_for_reply` | Email/get messageId, references, replyTo and body context | M06/M14/M22 |
| `mark_emails_read`, `mark_email_read`, `mark_email_unread`, `set_email_flagged`, `move_email`, `destroy_emails` | Email/set patches for keywords/mailboxIds and destroy; per-object results | M15/M22 |
| `get_threads`, `query_thread_emails` | Thread/get; existing nonstandard inThread query noted below | M14/M22 |
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

`query_thread_emails` sends `Email/query` with `inThread`, which is not a
standard RFC 8621 filter, and requests 500 results. The default td-mta query
page is 256. Before declaring compatibility, M14/M22 must replace this client
path with Thread/get plus bounded Email/get batches, or another standard
sequence preserving ordering/completeness. V1 must not silently implement an
unadvertised extension or truncate a thread to make this call pass. Also audit
all client get/set batches against the advertised 256-object limit. M01 records
the mismatch; it does not change client behavior or widen the server policy.

`submissions_between` omits query `limit`/`position`/`calculateTotal` and reads
only one submission page. M16/M22 must make lost-response reconciliation page
through the complete bounded time window, with bounded get batches and query
state handling, before absence is meaningful. An omitted match must not become
evidence that a send never happened. Include a fixture with more than 256
matching submissions and the sought submission beyond the first page.

## Release evidence

For each table row, the owning increment records the fixture/test and its
positive, refusal and restart observations. M02 supplies a traceable case ID
for each property group and exceptional wire mapping; later implementation
must split grouped rows when their coverage differs. Capability enablement
requires every mandatory row, not just the current client inventory. External
push registration refusal, read-only identities and uncomputable query changes
are real policy/error tests, never missing handlers disguised as conformance.
