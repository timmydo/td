# Submission and relay contract

This document is normative for M16/M17 and administration. FORMAT.md owns
bytes; this document owns allowed states and transitions. It does not enable
a listener, queue worker or JMAP capability. All times in persisted rows are
signed UTC milliseconds. Attempt deadlines use the monotonic Clock adapter.

## 1. Creating responsibility

Authorize the account, identity, From/Sender and envelope before publishing
the transmitted blob. Reject malformed addresses, ambiguous multiple
Sender/From fields, empty recipient sets and unsupported envelope parameters.
V1 advertises an empty submissionExtensions map and maxDelayedSend zero.
Null/omitted envelope is derived according to RFC 8621 §7: Sender, otherwise
From, with the identity address substituted when the derived sender is not
allowed; deduplicated To/Cc/Bcc recipients. Explicit envelope addresses must
also pass authorization. Preserve local-part case; compare domains after
ASCII case folding. Deduplicate by that pair, preserving first appearance.
Do not silently discard recipient parameters or accept FUTURERELEASE.

The queue stores the immutable, Bcc-stripped transmitted message separately
from the user's email. Its blob, submission, every recipient row and the
submission-created CHANGE become visible in one durable transaction. Reserve
the entire frame before creating this responsibility. A configured recipient
limit must fit that transaction; the format ceiling is 1000. Generated IDs
come from Entropy, with collision checks against live objects and retained
history. Do not deliberately reuse IDs; random 128-bit identifiers are not
a mathematical proof against collisions over unbounded deleted history.

sendAt is the creation time and expiresAt is exactly five days later by
checked addition. Reject an unrepresentable timestamp before publication.
Durable creation or a successful update (including cancellation) qualifies
for onSuccessUpdateEmail and onSuccessDestroyEmail. Apply these hooks to
every successful create/update/destroy item, never only to creation; v1
refuses client destroys, so none of those items qualify. Process their combined implicit Email/set once after
all EmailSubmission/set operations, with its separate response under the same
call ID. Its failure does not roll back a successfully created submission.
Deleting the visible Email never cancels delivery or changes historical
emailId/threadId in the submission.

## 2. Recipient state invariants

The row codec checks field widths and basic combinations. The transaction
validator additionally enforces this table. Reply fields hold actual SMTP
replies from the latest attempt that produced an applicable reply, normalized
by RFC 8621 §7; they are not local error messages. Keep previous replies while
a new attempt has not produced an applicable reply. A new RCPT reply replaces
rcptReply and clears dataReply; a new final DATA reply then fills dataReply.
A MAIL refusal clears rcptReply and fills dataReply for its attempted batch;
a DATA-command refusal fills dataReply for its accepted RCPT subset. Only
negative MAIL/DATA-command replies use that message-level fallback, never a
positive MAIL reply or interim 354. Reset the local
diagnostic on dispatch. These historical strings alone never prove a current
attempt succeeded: the worker must collect and fence its own RCPT/DATA result.
attempt, attemptCount and lastAttemptAt are either all absent/zero or all present/nonzero.

| State | Phase | Next attempt | Meaning |
| --- | --- | --- | --- |
| Queued | None | Present | Never attempted; no replies, uncertainty or failure reason |
| InFlight | Prepared, Body or AcceptancePossible | Absent | One fenced worker owns this attempt |
| RetryWait | Final | Present | No attempt may have accepted; uncertainty false |
| Accepted | Final | Absent | Genuine final positive DATA reply durably recorded for this recipient |
| Failed | None if never attempted, otherwise Final | Absent | Definitive failure/expiry, with no earlier uncertainty |
| Canceled | None if never attempted, otherwise Final | Absent | Guaranteed no delivery, uncertainty false, reason Canceled |
| OutcomeUnknown | AcceptancePossible or Final | Present while retry eligible, otherwise absent | Acceptance may have occurred; uncertainty true |

Accepted requires positive RCPT and positive final DATA replies, reason None.
Failed requires a permanent SMTP refusal or Expired; route/authentication
failures cannot directly produce it. RetryWait requires at least one attempt
and a temporary/network/TLS/authentication/protocol reason, with uncertainty
false. Unknown uses Uncertain, or the later failure/expiry reason (even while
retry eligible). Any unsuccessful attempt with an earlier uncertainty latch
returns OutcomeUnknown, never RetryWait/Failed. InFlight
starts with reason None. The uncertainty bit is latched once any attempt may
have accepted; later definitive failures cannot clear it. Accepted may retain
that bit to expose an earlier duplicate risk. All state changes produce an
EmailSubmission-updated CHANGE in the same frame, even when only local
diagnostics change. Mailbox/Email/Thread changes are added only when their
objects actually change (for example a local failure notice).

## 3. Attempt and restart transitions

The single route dispatches at most one SMTP transaction at a time. Select
at most 100 eligible recipients for one attempt, in ascending ordinal order;
a submission with more recipients is dispatched in batches. All recipients
in that batch receive the same new random AttemptId and checked increment
of their individual attemptCount. Exhaustion pauses dispatch and requires
operator intervention; it cannot wrap or discard responsibility.

| Event | Required durable transition / restart interpretation |
| --- | --- |
| Dispatch due Queued/RetryWait/retryable OutcomeUnknown | Persist InFlight/Prepared, timestamp and attempt ID before connecting or issuing SMTP |
| RCPT 4xx/5xx | Save actual reply; temporary becomes RetryWait, permanent becomes Failed, except uncertainty keeps OutcomeUnknown; exclude from DATA |
| Positive RCPT then 354 | Persist InFlight/Body for exactly the accepted recipient subset before writing message body |
| Before terminator | Persist InFlight/AcceptancePossible before handing any DATA terminator bytes to a TLS/plaintext transport buffer |
| Positive final DATA | Atomically persist Accepted for every recipient covered by DATA before reporting local relay success |
| Final DATA 4xx/5xx | Save reply for that subset; RetryWait/Failed respectively, or OutcomeUnknown if an earlier attempt was uncertain |
| Disconnect/protocol failure before AcceptancePossible | Current attempt is safe to retry; abort/fence before RetryWait if certain, otherwise OutcomeUnknown |
| Disconnect/protocol failure after AcceptancePossible | OutcomeUnknown, uncertainty true, retry policy applies |
| Restart in Prepared/Body | No terminator could have been emitted; end attempt and schedule RetryWait (or OutcomeUnknown if already uncertain) |
| Restart in AcceptancePossible | OutcomeUnknown, uncertainty true, schedule retry |
| Restart in Accepted | Never dispatch this recipient again |
| Expiry before any further dispatch | Failed/Expired if certainty permits; otherwise OutcomeUnknown/Expired with no next attempt |

A MAIL/DATA-command 4xx/5xx that definitively refuses this message follows
temporary/permanent refusal for the attempted/accepted subset respectively;
retain its actual reply in dataReply as specified above, so a pre-body
message refusal can be projected without inventing an SMTP response.
Authentication refusal, bad certificates and DNS failures instead pause/back
off the route, retaining every pending recipient. No SMTP command is sent on
a failed TLS channel. Never send a DATA body to a subset not recorded in its
attempt. Close the connection after a partial/invalid reply that cannot be
classified safely. SMTP parser limits must refuse overlong replies, not turn
a truncated prefix into a successful status.

Journal reservations cover the whole selected batch and its CHANGE. The
maximum encoded recipient row is 9019 bytes; a PUT adds 12 operation-header
bytes and a 20-byte key. A 100-row update is 905100 bytes before frame and
submission/CHANGE overhead, below the 1 MiB frame ceiling. RESOURCES.md
accounts separately for retaining distinct replies while the remote peer
is slow; the shared writer frame cannot be held across a network round trip.
Before committing AcceptancePossible, hold both its frame reservation and
a separate worst-case final-outcome reservation for the selected DATA subset,
including submission/CHANGE overhead. Only the phase reservation is consumed
by that commit. Keep the final reservation and refresh expected sequence under
the writer until result commit; Conflict cannot release this reservation.
Its deadline covers the remaining bounded SMTP attempt and commit allowance.
If these reservations cannot be obtained, abort before handing off terminator
bytes; do not create an uncertain outcome merely because admission is full.
Sync/I/O failure after handoff remains inherently uncertain despite capacity.

If a journal write/sync fails, stop the writer. A complete replayable frame
may exist even when the caller saw an error. Recovery decides the state;
the worker must not append an alternative result or continue transmitting
after losing its durable phase fence. A crash between remote acceptance and
local durability is inherently ambiguous. Retries use identical transmitted
bytes/Message-ID, without assuming provider deduplication.

## 4. Retry, cancellation and retention

After failed attempt n (n starts at 1), nominal delay is
min(300000 * 2^(min(n-1, 4)), 3600000) milliseconds. Sample uniformly from
the integer interval [max(300000, nominal * 9 / 10),
min(3600000, nominal * 11 / 10)] using Entropy; rejection sampling has a
limit of 32 entropy samples and failure pauses dispatch. Clipping the interval
before sampling avoids concentrating probability at either bound. Use checked
arithmetic. Entropy failure pauses dispatch; it does not discard or immediately retry a recipient. No free-text
SMTP retry hints are interpreted in v1. The persisted next-attempt UTC time
is bounded by expiry; if no eligible time precedes expiry, expire instead.

Route backoff uses the same bounds, shared across recipients; bad credentials
or certificate verification require a successful explicit configuration/
certificate reload before dispatch resumes. Reconstruct a conservative
route delay from retained attempt/retry rows during the bounded startup
scan; configuration repair releases the pause but not the minimum interval.
After restart or a wall-clock discontinuity, require five monotonic minutes
before redispatching an attempted recipient. Newly queued messages may be
sent immediately on a healthy route. Evaluate expiry before dispatch; once
expired, a clock moving backward cannot revive a terminal row. M02c3 owns
scan/scheduler work ceilings. Persisting/rebuilding an index cannot itself
change these authoritative times.

A UTC discontinuity exceeding five minutes relative to monotonic elapsed
time pauses new dispatch and expiry before any more terminal transitions.
At startup, any already-overdue pending submission is held for clock
validation, not immediately expired. An administrator must confirm the UTC
clock through a named queue clock-confirm operation after at least five
minutes of stable UTC/monotonic progress; until then health exposes the hold.
This is deliberately conservative for a restart after a long outage: no
durable row/format is added, and a restart repeats the validation if items
remain overdue. Non-overdue items may dispatch after the normal restart guard.
After confirmation, process at most one 100-recipient expiry batch per second,
yielding to interactive work; expiry is not one unbounded catch-up sweep.

Manual retry may move an eligible pending recipient earlier only after the
route and recipient minimum intervals; it cannot resend Accepted, bypass
expiry, resume a permanent refusal, or erase uncertainty. A new deliberate
submission is required to resend a terminal failure. Terminal OutcomeUnknown
requires operator acknowledgement before explicit deletion, with duplicate
risk visible; it is not silently retried after expiry.

Cancellation is submission-wide and atomic. Serialize against the worker;
fence and close any Prepared/Body attempt before committing cancellation.
Refuse if any recipient is Accepted, has uncertainty, or is currently in
AcceptancePossible. An already wholly Canceled submission is an idempotent
success. Before fencing or changing rows, compute and reserve the complete
cancellation frame, retaining actual replies. Refuse with cannotUnsend (CLI
limit reason) if it cannot fit the byte/operation ceiling or obtain capacity;
never split cancellation or discard replies to fit it. Thus up to 1000 fresh
queued recipients can cancel together, but a large attempted submission with
long reply history may require explicit refusal. Failed recipients with no
uncertainty can become Canceled together
with all remaining recipients: the guarantee is that no recipient received
the message. Neither CLI nor JMAP offers partial cancellation in v1. A later
worker result with an old attempt/fence is rejected before writing state.

completedAt is set once every recipient has no future dispatch obligation;
it is a wall-clock observation and can precede sendAt after a clock step.
Retain accepted/canceled submission records and their blobs for at least
30 days after completion by default. Failed and terminal OutcomeUnknown
records remain until explicit administrator deletion; storing a notice does
not resolve them. Ordinary retention never deletes pending responsibility.
Manual JMAP destruction is forbidden for all submission records; record
destruction does not mean cancellation (RFC 8621 §7.5).

When a submission becomes terminal with any Failed/OutcomeUnknown recipient,
set notification Pending in the same transaction. A local account-only
failure email and notification Stored + notificationEmail are committed in
one later frame; retry Pending on local storage failure. Its Email ID is
historical after user deletion, so a deleted notice is not generated again.
Never notify an external inbound reverse path. Administrative acknowledgement
and deletion are explicit, journaled actions subject to the same worker fence.

## 5. Standard JMAP projection

Expose precisely RFC 8621 §7 fields; internal states/reasons belong to CLI
and logs. identityId/emailId/threadId and sendAt never change with retries.
Envelope addresses preserve the committed order; parameters are null.

| Property | V1 projection |
| --- | --- |
| undoStatus | final if any recipient is Accepted; canceled if all are Canceled; pending otherwise (a cancellation attempt can return cannotUnsend) |
| deliveryStatus | A map for recipients with actual applicable SMTP replies; empty object before any are known (v1 supports this property) |
| smtpReply | Latest applicable RCPT reply, except a later message refusal (MAIL, DATA command or final DATA) uses dataReply; no invented reply for local/DNS/TLS/expiry errors |
| delivered | Accepted -> unknown; any uncertain recipient -> unknown; retryable/active certain recipient -> queued; certain Failed/Canceled -> no, when a reply entry exists |
| displayed | unknown for every entry |
| dsnBlobIds, mdnBlobIds | Empty arrays; v1 does not correlate received reports, which remain ordinary email |

An accepted smart-host transfer never proves final mailbox delivery and
never yields delivered=yes. A retry before RCPT preserves the last actual
reply and its map entry; attempt state still advances through CHANGE. A
positive RCPT reply only permitted a DATA attempt: it can remain the last
actual reply when a later local expiry makes delivered=no.
For local expiry/cancellation with a previous actual reply, preserve that
reply and project the terminal status; local cause remains in diagnostics.

RFC 8621 §7 permits undoStatus to remain pending even when cancellation can
be refused; final is evidence of relay to at least one recipient. Its
deliveryStatus definition covers known recipient status, and smtpReply is a
non-nullable String. A local failure with no applicable SMTP reply is visible
through the retained queue record/notice, without synthesizing an SMTP reply.

Only pending -> canceled is a client-writeable undoStatus transition (and
idempotent canceled -> canceled succeeds). Unsupported updates receive
invalidProperties; a refused pending cancellation receives cannotUnsend.
Known destroy IDs receive forbidden; unknown IDs receive notFound. Standard
ifInState processing precedes object operations. Creation errors use the
standard missing Email/Identity invalidProperties, invalidEmail with all
identified invalid Email properties (including ambiguous From/Sender),
tooLarge with maxSize,
tooManyRecipients with maxRecipients, noRecipients, invalidRecipients with
its address list, forbiddenMailFrom, forbiddenFrom and forbiddenToSend where
applicable; storage/admission errors follow API.md.

Queries implement identityIds/emailIds/threadIds/undoStatus, after inclusive
and before exclusive against immutable sendAt. Support both sentAt (the
comparator spelling printed in RFC 8621 §7.3) and sendAt as aliases for that
timestamp, plus required emailId/threadId sorts; ascending raw SubmissionId
breaks ties. Do not invent a sentAt object property.

## 6. Required implementation evidence

M16/M17 fixtures must cover every transition and crash row above, partial
RCPT subsets, distinct long replies, all-1000-recipient batch progress,
cancellation-frame fit/refusal, retained final-outcome reservations, clock
validation and bounded expiry catch-up, successful cancellation filing hooks,
earlier uncertainty followed by refusal/expiry/acceptance, writer sync failure,
lost final replies, cancel-versus-terminator fencing, old worker results,
clock steps, route repair, retained replies after pre-RCPT failure, lost JMAP
replies and separate filing failure.
Each test observes recovered rows, transmitted bytes and the exact JMAP
projection. M22 uses td-mail for creation and paginated lost-response lookup.
These are acceptance requirements, not claims of passing protocol tests today.

Source: [JMAP Mail, RFC 8621 §§7–7.5](https://www.rfc-editor.org/rfc/rfc8621.html#section-7).
