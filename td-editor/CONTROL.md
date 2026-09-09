# Control protocol reference

The experimental `--window --control-socket PATH` endpoint implements the
query and revision-checked editing subset below. It is off by default;
scratch preview and replay do not accept the option. Remote Close Tab, Quit and
close-dialog Cancel/Discard/Save/path, conflict Cancel/Reload/Save As and file
Open/Save/Save As are connected, including ordinary Open/Save As/Dictionary
path answers. Decoded keys drive ordinary editing and menu/Find/Replace/
numeric/command input under the stricter input contract below. Decoded pointer
press/move/release share native hit testing, and normalized wheel deltas use
the native scrolling path.
`prompt-state` reads full text entries, target validity and native feedback
without changing any input or dialog state. `prompt-answer` supplies pinned
non-file entry/actions without requiring focus.
Remote Check Spelling admission and
bounded job outcomes are implemented below. The complete version-1 target is
specified in
[DESIGN.md](DESIGN.md#test-and-control-architecture).

`control` has no listener, thread, filesystem access, clock or Wayland access.
The separate `control_socket` library publishes a private Unix listener
under the contract below.
`control_worker` owns that listener on a bounded transport thread and hands
typed requests to the native window's UI thread.

## Framing

One frame is a four-byte unsigned big-endian length followed by that many
payload bytes. Length excludes the header and must be 1..=1,048,576 bytes,
for requests and responses. There is no newline terminator. `frame` checks
the bound before allocating its output. `Decoder` accumulates one frame,
accepts arbitrary header/body fragmentation, and exposes its payload only
after all declared bytes arrive. A zero length is `protocol`; a length over
the ceiling is `limit`. Incomplete `finish` is `protocol`.

Any decoder refusal permanently poisons it and drops partial payload storage.
More input cannot revive it. Bytes beyond the declared body, if supplied to
that decoder, are refused rather than treated as a second request. Completion
does not prove peer EOF or that no later bytes will arrive: the worker
must dispatch at most once per connection and close it after the response.
`finish` transfers the completed buffer; it performs no read or EOF check.
Empty input chunks make no progress. The decoder allocates at most one MiB
of payload storage, only after validating the length.
The full declared storage is allocated on header completion, before body
bytes arrive. The worker must budget each admitted connection against that
one-MiB allocation, not just against bytes received so far.

The worker below owns deadlines, connection limits, request/response queues
and cancellation. The framing library alone claims none of those safeguards.
Private socket publication is implemented separately below. Replay retains
its consecutive-frame stdin/stdout runner; the one-frame decoder serves the
one-request-per-connection worker.

## Requests

Payloads are ASCII tab-separated records. Literal control bytes other than
field-separating Tab, DEL and non-ASCII payload bytes are refused. Decimal
fields contain one or more digits only, except signed wheel deltas below.
Leading zeros are accepted; signs, spaces, exponents and overflow are
refused. IDs/revisions fit `u64`; offsets
and limits additionally fit the host's `usize`. Missing/extra fields, unknown
versions and names are `protocol`. Parsing uses a bounded field iterator, not
a vector proportional to the number of Tab bytes in an untrusted payload.
Envelope validation and error-ID recovery are shared with replay, while the
two adapters retain distinct command allowlists.

In the examples below, field spaces denote literal Tab separators.

| Payload fields | Meaning |
| --- | --- |
| `1 ID state` | Snapshot the current controller. |
| `1 ID clipboard-state` | Inspect bounded native clipboard and transfer metadata without mutation. |
| `1 ID prompt-state` | Inspect native text-entry state and existing dialog IDs without mutation. |
| `1 ID prompt-answer TAB REVISION INPUT_GENERATION KIND ANSWER [HEX_ENTRY]` | Answer one pinned non-file text prompt; details below. |
| `1 ID new` | Create and activate an empty tab; return its stable ID. |
| `1 ID open HEX_PATH` | Queue ordinary file Open and return a background-job ID. |
| `1 ID save TAB REVISION` | Queue a revision-pinned save to the associated file. |
| `1 ID save-as TAB REVISION HEX_PATH` | Queue a revision-pinned save to a new destination. |
| `1 ID close-tab TAB REVISION` | Start ordinary active-tab close, without approving discard. |
| `1 ID quit` | Start ordinary whole-window close, without approving discard. |
| `1 ID dialog-answer DIALOG TAB REVISION ANSWER` | Use close `discard`/`save`, or `cancel` for a live close, conflict or path flow. |
| `1 ID dialog-answer DIALOG TAB REVISION path HEX_PATH` | Supply a literal path for the live Open, Save As or Dictionary prompt. |
| `1 ID dialog-answer DIALOG TAB REVISION reload` | Request Reload from a live conflict; dirty text requires a second answer. |
| `1 ID dialog-answer DIALOG TAB REVISION discard-reload` | Confirm the conflict's second question before replacing unsaved text. |
| `1 ID dialog-answer DIALOG TAB REVISION save-as HEX_PATH` | Save the conflict target to a new literal destination. |
| `1 ID key TAB REVISION GENERATION HEX_CHORD` | Deliver one decoded native chord with an exact input-context fence. |
| `1 ID pointer TAB REVISION GENERATION PHASE X Y EXTEND` | Deliver a bounded pointer press/move/release with the same input-context fence. |
| `1 ID wheel TAB REVISION GENERATION ROWS COLUMNS` | Deliver one bounded normalized wheel frame with an input-context fence. |
| `1 ID text TAB REVISION OFFSET LIMIT` | Read a scalar-aligned UTF-8 page. |
| `1 ID spelling-results TAB REVISION SCAN OFFSET LIMIT` | Read native spelling status and a scan-pinned range page. |
| `1 ID wait-frame GENERATION` | Wait for a main-surface callback at or beyond this native redraw generation. |
| `1 ID check-spelling TAB REVISION` | Admit an on-demand whole-document spelling job for the active tab. |

Tab creation, file jobs and the editing subset are specified below. `load`,
direct Reload, raw physical-input simulation and other dialog answers remain
refused; this parser is not a route into replay's broader command set.
`Request::response` borrows `&Controller`, so it cannot dispatch an edit or
change selection, views, history or generation. It returns `unavailable`
for creation, file jobs, closing, dialog, editing, spelling and frame requests.
`Request::execute` admits edits; the native adapter dispatches creation,
file jobs and close/dialog actions and supplies its job/spelling/frame state.

An error response is `1 ID error CODE HEX_DIAGNOSTIC`. A recoverable request
ID is echoed even if the command name is missing or later arguments are
invalid. Failures before a valid ID is parsed use zero: frame-size, ASCII
and version checks precede ID parsing. Replay uses this same envelope helper.
Zero is also a valid caller ID; it carries no authorization meaning.
Currently the diagnostic is the stable code's own ASCII bytes encoded as
lowercase hex. Empty byte strings use `-`; nonempty hex has two lowercase
digits per byte. `hex`/`unhex` and the frame/page limits are shared with
replay, whose old public helper names remain re-exports, not duplicate codecs.
Replay also uses the bounded response-frame encoder.

## Clipboard inspection

`1 ID clipboard-state` is a native-only read-only query with no arguments.
It is available without keyboard/pointer readiness, focus or non-modal UI.
The controller-only adapter returns `unavailable`. Success is `1 ID ok`
followed by these fields, in order:

| Field | Meaning |
| --- | --- |
| `input-generation=N` | Existing input-context fence, not a clipboard revision or transfer receipt. |
| `device=0\|1` | A native data-device object is retained. |
| `focus=0\|1` | Current native keyboard focus. |
| `selection=none\|unsupported\|utf8\|plain` | No selection ID, no usable supported offer, preferred UTF-8 MIME, or UTF-8 plain-text fallback. |
| `source-bytes=N\|-` | Byte count of the locally retained immutable Copy/Cut snapshot, or no snapshot. |
| `incoming=0\|1` | An incoming transfer owner is retained. |
| `outgoing=0\|1` | An outgoing transfer owner is retained. |

Selection classification uses the same current, non-retired offer and MIME
preference as Paste, including accepted ASCII case variants. `unsupported`
also covers a selection ID without a retained usable offer. Source retention
does not acknowledge compositor ownership; a writer can remain after its
source is retired. Transfer flags do not promise progress, successful EOF,
a live peer or an unexpired deadline. There is no transfer identity, byte
progress, target, terminal history, payload, native object ID or MIME string
in this snapshot. Fields describe the adapter at query processing time and
may change immediately afterward; the input token is not a clipboard-state
version and does not reserve an offer or authorize an action.

The query does not read clipboard bytes, issue protocol requests, poll or
advance a transfer/file/spelling job, validate a target, clear feedback,
cancel input/Paste, change a generation or request a frame. It uses the
existing bounded control-worker query budget. Fatal adapter state and
exhausted frame/input counters retain normal fail-stop behavior; shutdown
ends socket service. Device, focus and supported-offer flags alone do not
make Copy/Paste admissible: ordinary native serial, modal and target rules
remain unchanged. The response is bounded metadata, not clipboard access.

## Prompt inspection

`1 ID prompt-state` is a native-only read-only query, available without
keyboard/pointer readiness, focus or visible prompt geometry. It does not
type, dismiss, answer, validate an entered value, cancel Paste/repeat/drag,
advance a generation, poll a job or request a frame. The controller-only
adapter returns `unavailable`; the native worker uses its existing bounded
query budget. Fatal adapter state and exhausted frame/input counters retain
their ordinary fail-stop behavior. Worker shutdown ends socket service.

Success is `1 ID ok` followed by these fields, in order:

| Field | Meaning |
| --- | --- |
| `input-generation=N` | Exact current input-context fence, not a frame acknowledgement. |
| `prompt=KIND` | Retained text-entry kind below; `none` means no entry, not no modal. |
| `target=TAB,REVISION` or `-` | Current validated prompt owner, or no valid target. |
| `target-error=CODE` or `-` | Ordinary model refusal when a retained prompt target is invalid. |
| `field=text\|replacement\|-` | Selected field within the retained entry; `replacement` occurs only in Replace. |
| `text=HEX` | Entire literal primary entry, including a value clipped in the UI. |
| `replacement=HEX` | Entire Replace replacement field; otherwise empty. |
| `refused=0\|1\|-` | Prior numeric/command refusal flag; otherwise not applicable. |
| `status=HEX` | Replace's stored human-readable result/status; otherwise empty. |
| `notice=HEX` | Last stored native feedback, possibly hidden by a modal. |
| `key-ready=0\|1` | Existing decoded-key availability, not target/value validation. |
| `dialog-last=N` | Existing shared close/conflict/path ID counter. |
| `dialog=...` | Existing close/conflict/path identity/phase/answers from `state`. |
| `completion=idle\|pending\|ready\|error` | Path-completion state; idle without a path entry. |

Completion adds state-specific fields after `completion`: pending includes
`completion-id=N`, a checked scan ID, not answer authority. Error includes
`completion-error=HEX`, a UTF-8 diagnostic limited to 160 characters. Ready
includes `completion-count=N`, `completion-skipped=N` (non-UTF-8 basenames),
`completion-selected=INDEX|-`, then up to three repeated
`completion-item=INDEX,HEX` fields for the currently displayed page. Indices
are zero-based sorted match indices; values are full literal UTF-8 paths,
not escaped display labels. Each path is at most 4096 bytes and the list at
most 128 paths; only the current three-row page is returned. These bounded
fields fit the existing one-MiB frame. Inspecting them never starts or polls
a scan. Physical Tab/Shift+Tab/Up/Down uses the path entry's keyboard path;
semantic dialog answers still supply explicit OS-byte paths and do not
gain a completion mutation verb or physical clipboard authority.

Kinds are `none`, `path-open`, `path-save-as`, `path-dictionary`,
`close-save-as`, `find-forward`, `find-backward`, `replace`, `go-to-line`,
`fill-column`, and `command`. `close-save-as` is Save As answered through
the close dialog's ID rather than an independent path ID, including after
a save conflict during close. Other ordinary paths always have their own
revision-bound identity; opening one without a valid tab is refused.

An async save conflict can cover a retained Find/Replace/numeric/command
entry. The snapshot still exposes that retained entry, not the surface
currently receiving input. Path entry takes precedence; otherwise a live
close/conflict/Reload question outranks non-file entry for input. Consult
`dialog` for this question authority; `key-ready=0` alone also covers loss
of focus/readiness. Menus and questions with no retained entry report
`none`. `state` supplies coarse modal flags. Inspection creates no new
prompt IDs and grants no answer authority by itself.

Each of `text`, `replacement`, `status` and `notice` is UTF-8 encoded as
lowercase hex, with `-` for empty. Each is limited to 8,192 original bytes;
the whole query returns `limit` before hex encoding if any exceeds the
ceiling. Values are never truncated. Four bounded hex fields plus fixed
metadata fit the existing one-MiB response bound without paging. Existing
entry limits remain smaller or equal; this query does not enlarge them.
Current producers fit below the snapshot ceiling (stored notices are capped
at 512 characters); the outer bound also protects against future producer
growth or internal misuse.

Target checks use the existing owner/revision/selection/setting rules of
the prompt. A stale retained entry remains inspectable: `target=-` and its
`target-error` describe refusal without discarding text. A valid target does
not mean the typed number or command is valid, and `refused=0` means only
that no prior refusal is retained. `status` and `notice` are diagnostic prose,
not stable machine enums, completion receipts, or claims about visible pixels.
Readiness and all fields are captured in one UI-thread observation. Paths
include their ordinary live dialog fields in that same snapshot; inspection
does not bypass the explicit ID/tab/revision checks for an answer. Other
text prompts can use the explicit pinned answers below. Decoded keys retain
their real-input readiness policy.

This explicitly opted-in private endpoint already grants document read/write
access. Prompt entries and feedback may also contain user text and file paths;
do not publish them as non-sensitive diagnostics. `Request` Debug contains
only the query operation, never a response or prompt value.

## Non-file prompt answers

`prompt-answer` acts only on the currently open `find-forward`,
`find-backward`, `replace`, `go-to-line`, `fill-column`, or `command`
prompt. It cannot open a prompt. Use `prompt-state` to read its exact kind,
valid tab/revision and current nonzero `input-generation`, then echo all
four values. This input-context token is the live-instance fence: opening,
closing, entry changes and every accepted answer advance it; caret-only
blinks do not. Reopening the same kind on the same revision cannot accept
an old answer. There is no additional non-file dialog ID counter.

This is trusted semantic dialog control, available without keyboard/pointer
readiness, focus or visible geometry. It does not synthesize a physical key,
held state, repeat, serial or focus event. The file/close/conflict protocols
are unchanged: these answers cannot reach their handlers or approve discard.
Menus, close/quit, path/conflict/reload state, a closed window, fatal adapter
state or an active native serial refuse `unavailable`.
The menu/quit exclusions also defend the ordinary prompt-exclusivity
invariant; Find opening clears any retained numeric entry.

| Answer | Effect |
| --- | --- |
| `entry HEX_ENTRY` | Replace the entire active entry field, without submitting. Empty `-` clears it. |
| `submit` | Ordinary Return: Find, numeric apply, exact named command, or Replace Find Next. |
| `cancel` | Close this prompt only; completed Replace edits remain undoable. Unlike global Escape, it does not cancel an unrelated spelling scan. |
| `complete` | Command only: ordinary bounded prefix completion. |
| `next-field` | Replace only: toggle Find/With. |
| `replace-one` | Replace only: replace the selected exact match. |
| `replace-all` | Replace only: ordinary atomic whole-document replacement. |

Non-entry answers take no extra argument. Unknown kinds/answers are
`invalid-argument`; missing/extra fields are `protocol`. Entry wire decoding
requires valid UTF-8 and at most 4,096 bytes before native admission.
The native adapter also validates constructed requests: Find/Replace allow
up to 4,096 UTF-8 bytes without control characters; numbers allow at most
20 ASCII digits; command entry allows at most 64 ASCII lowercase letters or
hyphens. Empty entries are allowed. Length excess is `limit`, invalid entry
characters are `invalid-argument`. The entire entry is checked before any
clearing/typing; there is no silent truncation or partial acceptance.
Numeric range/line existence and exact command names are checked only when
submitted, using the ordinary prompt's correction/refusal behavior.

After global health checks, native admission checks excluded modal/serial
state, nonzero/exact input token, matching live kind and the prompt's own
target validity, exact requested tab/revision, answer-kind/entry validity,
then controller generation headroom. Zero input token is `invalid-argument`.
Native prompt target validation retains
its own model refusal code. Token or requested tab/revision mismatch returns
`stale-revision`; a missing/different kind or inapplicable answer returns
`unavailable`. Existing selection/setting target checks apply even to
Cancel and entry changes. A stale retained prompt is inspectable but cannot
be answered by weakening its target checks; physical/ready decoded Cancel
remains available. Every refusal before admission preserves entry, feedback,
jobs, generations, drag/repeat/Paste and document state.

Every admitted answer stops drag/repeat and cancels prior Paste, then uses
only the pinned ordinary prompt handler. Entry replacement uses its existing
clear-and-literal-character rules, without a generation per character.
Observers run afterward and the input token advances even for an unchanged
entry or refused numeric/command submission. Success is `1 ID ok` with an
empty body: delivery, not proof of a model edit, match or job completion.
Query `prompt-state`, document state/text and ordinary job fields afterward.
Named commands still use the exact built-in action list, never evaluation;
native jobs they initiate do not acquire a remote job receipt. A native
handler error latches the existing fatal-input state and refuses later work.
Numeric/command Cancel also skips global Escape's search-wrap cancellation;
ordinary post-action observation still invalidates stale search state.
Debug records kinds and entry byte lengths, never entry contents.

## Tab creation

`1 ID new` has no arguments. The native adapter dispatches the same
`Event::New` as the File > New menu and replay, returning `1 ID ok TAB`
with the newly allocated stable tab ID. The tab becomes active and is empty,
clean and unnamed, with revision zero, an empty selection at byte zero,
empty Undo/Redo history and ordinary new-document defaults. Existing tabs,
their file associations, dirty text, selections, history and spelling
reports remain intact. A scan on an existing tab may keep running.
No filesystem operation, background job or prompt is created.

This command does not target existing text, so it has no expected tab or
revision. It is not idempotent: each admitted request creates a separate tab,
even when the caller reuses a request ID. Request IDs are correlation values,
not deduplication tokens. The returned tab ID belongs to that admission;
another input can select or create a different tab before a subsequent
state query. Do not blindly retry a lost reply.

The same closed/quitting/modal and transport-liveness guards as remote
edits apply before dispatch. The ordinary 64-tab ceiling and checked tab
and controller counters refuse before input/view changes. These admission
refusals preserve all state and do not cancel a pending Paste. New consumes
the same tab budget as a pending Open; taking the last slot can make that
Open refuse when its read completes, without creating a file-backed tab.
Success resets controller
input, cancels native Paste/repeat/drag/wrap context as applicable, and
invalidates the native frame. The reply is not presentation evidence;
use state and `wait-frame` separately. The immutable controller-only
response API returns `unavailable`; creation is a native action, while
headless tests can dispatch `Event::New` directly or use replay `new`.

## File Open jobs

`1 ID open HEX_PATH` submits ordinary file Open through the existing single
file worker. The path is 1..=4096 literal OS bytes, encoded in lowercase hex,
with no NUL. It need not be UTF-8. Relative paths use the editor's working
directory; no shell, tilde or environment expansion occurs. Oversize encoding
is `limit`, malformed hex/arity is `protocol`, and empty/NUL paths are
`invalid-argument`, before native admission. Request debug formatting exposes
only the path byte count, not its contents. Ordinary native file notices
retain their existing escaped-path diagnostics.

Closed/quitting windows, every native modal and open menus refuse
`unavailable`. A file session is required, and any pending file operation
refuses another Open without consuming a job ID. Controller-counter and job
capacity/exhaustion checks precede admission. These refusals preserve the
native snapshot and notice. Admission cancels native repeat, drag and Paste
through the shared cleanup paths, invalidates a frame and returns
`1 ID pending JOB`. It does not wait for file I/O or select a tab yet.
Here `pending` acknowledges a job ID, not a promise that its row is still
pending: a synchronous submission failure may already be terminal.
The file worker's ordinary publication, duplicate-association and text-codec
rules are unchanged; Open grants no write, reload or overwrite authority.

Open shares the checked job-ID sequence and 64-row history with spelling.
Its row is `job=JOB,open,TAB,REVISION,0,STATUS,CODE`. A pending row has zero
tab/revision. Successful completion records `complete,-` with the nonzero
tab ID and revision selected or created by that exact Open, captured before
any later native/control action. Opening an associated file selects its
existing tab without replacing text, history or disk baseline. A missing
file creates the ordinary dirty, empty new-file tab without writing a file.
An invalid document, exhausted model capacity, worker failure or failed
submission records `error,unavailable` with zero tab/revision; the native
notice retains the more specific diagnostic. This subset does not yet expose
typed file-error subcategories. No human diagnostic is parsed to classify
success: the single in-flight worker result and synchronous controller
admission determine it. Defensive impossible-target failures may report
`missing-tab` or `protocol`; those are invariant errors, not file-error
subcategories. Successful ordinary Open always selects a nonzero live tab.

Open rows are historical, not a statement about the current active tab or
revision. Later selection, edits, closure and spelling work cannot retarget
them. A completed row is not presentation evidence; use `wait-frame`
separately. No path, title or document bytes are retained in job rows. The
same eviction and transport-lifetime rules as spelling apply. In particular,
disconnect or a lost reply cannot cancel an admitted Open, and clients must
not blindly retry it. Native Open/dictionary/Save operations do not create
remote job rows unless their path is answered remotely as specified below.

## Save and Save As jobs

`1 ID save TAB REVISION` saves the associated file. A tab without a file
association refuses `invalid-argument`; use `save-as` with an explicit path.
`1 ID save-as TAB REVISION HEX_PATH` uses the same bounded literal OS-byte
path grammar as Open. It requires a new, unassociated destination under the
ordinary file adapter's policy. An existing file is never force-overwritten.
Request Debug redacts the path. These direct remote commands do not create
or answer path prompts; ordinary keyboard Save/Save As prompts are unchanged.

Admission first applies the shared closed/quitting/modal/menu guard, then
requires a file session with no pending file work. It validates the target
revision, then requires the active tab and (for plain Save) an association.
Wrong/missing/stale targets use the ordinary `invalid-argument`, `missing-tab`
and `stale-revision` codes. Counter/capacity checks precede job reservation
and native input cleanup. These refusals preserve the native snapshot and
notice. Successful admission returns `1 ID pending JOB`; like Open, that
acknowledges an ID even if submission already failed. The endpoint's trusted
remote authority does not require physical focus or prompt visibility.

Save admission reserves the one global file-operation slot and stores an
editor-identity-bound tab/revision point plus optional path, not text bytes.
The next ordinary file poll is the handoff boundary: it revalidates that
point before capturing a snapshot or submitting anything to the worker.
Any intervening edit, including edit followed by Undo to identical text,
fails the job `stale-revision` without submitting a write. A vanished target
or different editor is respectively `missing-tab` or `invalid-argument` at
that boundary. No new file/dictionary operation or close can enter while
this slot is queued/in flight. This is one queued/in-flight file operation
per window, a stricter bound than the version-1 per-tab ceiling.

After handoff, the existing Save path owns an immutable encoded snapshot
and opaque saved-state token. Later edits cannot change its bytes. Ordinary
file publication, conflict detection, destination reservation, cleanup and
controller acknowledgement are unchanged; remote control never constructs
a saved-state token or dispatches `Event::Saved`. Changing the active tab
does not retarget an admitted save. Newer edits remain dirty after the
earlier snapshot succeeds. Socket disconnect, loss of a reply or cancellation
of a close request cannot undo an accepted write.

Rows are `job=JOB,save,TAB,REQUEST_REVISION,0,STATUS,CODE` or the same with
kind `save-as`. They retain the requested tab/revision for both pending and
terminal outcomes, never the current active tab or latest revision. A
`complete,-` row means ordinary publication and saved-state acknowledgement
succeeded for the requested snapshot, not that later edits are saved or a
frame was presented. No path, text or native diagnostic enters the row.

Other file/submission/acknowledgement failures use `error,unavailable`;
the native notice preserves the specific file diagnostic. An error alone
does not prove that disk is unchanged: publication or cleanup may have
partially succeeded, so inspect the native warning and destination before
retrying. Only a revision/identity rejection before handoff proves that this
job submitted no write. Terminal rows, eviction and transport ambiguity use
the shared job contract. Ordinary conflict dialogs may appear after Save
fails; remote conflict answers below retain the ordinary Reload/Save As
policy. Close-driven Save/path answers share these jobs. No force answer or
conflict bypass is added.

## Close requests and dialog answers

Close Tab uses the native file-window close coordinator. It requires the
active tab and its current revision, and refuses during any existing modal,
while closed/quitting, without a file session, or while file work is busy.
These native guards precede missing-tab/revision/active-tab checks. Missing,
stale or inactive targets use `missing-tab`, `stale-revision` or
`invalid-argument`. The checked dialog counter and room for two possible
controller dispatches are admitted before native input changes; exhaustion
is `exhausted`. Validation refusals preserve the snapshot and visible notice.
The two-dispatch reservation covers pointer cancellation and clean completion;
it may conservatively refuse near counter exhaustion even for a dirty tab.

`quit` has no arguments and uses the same guarded coordinator with window
scope. It pins the current set of tabs/revisions at admission and asks about
each dirty target; it never grants discard itself. There is no expected
active tab because the requested scope is the whole window. New/removed or
changed tabs invalidate the existing coordinator rather than expanding its
approval. Close Tab and Quit remain unavailable in scratch preview.

A clean scope closes immediately with `1 ID ok closed`. Dirty text is not
removed: success is `1 ID ok dialog DIALOG`, and state supplies its live
question. Successful close admission shares ordinary pointer/repeat reset,
then cancels native Paste and updates search/spelling/frame state through
the common control boundary. There is no file read/write or save job here.
Closing the last tab exits the window; shutdown can drop the reply before
delivery. Connection EOF is not a receipt or persistence proof. Normal
transport liveness/deadline rules still apply, and a lost reply must not be
blindly retried.

Native state adds `dialog-last=N`, initially zero, and exactly one
`dialog=ID,SCOPE,PHASE,TAB,REVISION,ANSWERS` field, or `dialog=-` when neither
close, conflict nor identified path flow exists. Close `SCOPE` is `close-tab`
or `close-window`; conflict/path fields are defined below. Close `PHASE` is
`question`, `path` (Save As entry during closing), or `saving`. The answers
are `cancel+discard+save` for a question, `cancel+path` for path entry, and
`cancel` while saving. An invalid coordinator reports `invalid,-,-,-` after
scope and offers no remote answer.
This covers stale pinned targets and a defensive no-question fallback;
ordinary completion removes a fully answered coordinator synchronously,
before the next control request. The coarse native file-busy flag remains
available even for an invalid coordinator. Non-close path prompts have their
own IDs and cannot be answered with this close token.

Each admitted ordinary or remote close request, new conflict question and
non-close path prompt consumes a new nonzero window-local checked `u64`
dialog ID, including clean immediate closes. They share this counter. The
ID is retained across that coordinator's questions and Save phases, never
reset or reused in that window. Dialog, transport request, spelling job/scan,
tab and frame IDs have separate namespaces; their numeric values may coincide.
Repeated window-manager close while the coordinator is already live keeps
its ID. Cancellation and a fresh close get
a new ID even for identical tab text/revision. IDs are not durable across
window/process restarts. No dialog history, path or entry text is retained
or exposed by these fields.

An answer must match the nonzero live ID and the coordinator's current
question tab and revision. A missing/closed coordinator is `unavailable`;
zero/wrong ID or wrong question tab is `invalid-argument`; wrong revision
or a coordinator invalidated by later text is `stale-revision`. A vanished
pinned tab may report `missing-tab`. The existing coordinator revalidates
every pinned model point, not just the named tab. Repeated answers cannot
approve the next window-close target. Missing fields are `protocol`;
unsupported/case-mismatched answers are `invalid-argument`. Fields are decoded
left to right, so an invalid answer
precedes a trailing-field error; an otherwise valid answer with extra fields
is `protocol`. No force/discard bypass exists.

As explicitly approved for automation, trusted remote answers do not require
keyboard focus, a synchronized keymap/seat, or a fully visible prompt.
Physical Save/Discard still require their existing input and 272x160-pixel
visibility conditions. This does not fabricate input state or a physical
serial. The remote authority is the private control endpoint plus an explicit
answer bound to the live dialog, tab and revision. This includes a dialog
opened by a human through ordinary input or the window manager; the client
need not have opened it. Save and close-driven path answers use this same
approved policy; they do not fabricate key events or physical serials.

`discard` is accepted only in the question phase, with no pending file work,
and prechecks room for its possible controller dispatch. It invokes the same
coordinator approval and completion path as physical Ctrl+D. Tab close removes
only that tab and its file association. Window-close approvals stay deferred:
all text/tabs remain until all dirty documents have decisions; Cancel drops
all approvals. `cancel` is also allowed during close-driven Save As or Save;
it drops the close coordinator and its path entry, but never cancels or rolls
back an accepted write. Completed saves stay saved and later edits stay dirty.
Neither answer alters the physical confirmation checks.

Cancel/Discard success is `1 ID ok` plus a trailing Tab. It acknowledges that
explicit choice, not file persistence or presentation; state may now name
the next question or no dialog. Queries are immutable. Rare admitted coordinator
completion failures report their error and retain unapproved text, but may
clear the failed dialog and update its native diagnostic; they are not
side-effect-free validation refusals.

`dialog-answer DIALOG TAB REVISION save` is accepted only in the question
phase with no pending file work. An associated target queues the same remote
Save described above and returns `1 ID pending JOB`. An untitled target
instead opens the ordinary close-driven path entry and returns
`1 ID ok dialog DIALOG` with Tab separators and no trailing Tab, retaining
the same coordinator ID without reserving
a job. The target may be inactive during a whole-window close; only the
coordinator's current question can authorize it, never the selected tab.

`dialog-answer DIALOG TAB REVISION path HEX_PATH` is accepted only for that
coordinator's live Save path entry. The explicit literal OS-byte path replaces
any partially typed entry and follows Open/Save As's 1..4096-byte, non-NUL
hex encoding. Path bytes are redacted in Debug and never serialized in state.
It queues Save As and returns `1 ID pending JOB`. Busy or exhausted-counter
refusals retain the exact path entry and close plan, without reserving a job.
Neither Save in path/saving phases nor Path in question/saving phases is
accepted; these phase refusals are `unavailable`. Other native path prompts
require their own live ID as specified below, never this close token.

Both save paths reserve capacity/counters before admission and recheck the
owner-bound revision at handoff; subsequent writes use immutable snapshots.
Accepted jobs use the same `save`/`save-as` historical rows as direct commands.
Completion advances the existing close plan only while it remains live;
file failure cancels closing and retains tabs, with the ordinary diagnostic
and possible conflict prompt. Cancel drops the plan, not the accepted job:
without an edit the queued write still proceeds; an edit before handoff makes
it stale, while an edit after handoff remains unsaved. Successful earlier
saves are not rolled back. A final save can exit the window before its job
outcome is queried; a pending reply or connection EOF is not persistence proof.

## Conflict answers and Reload jobs

An ordinary Save conflict opens `dialog=ID,conflict,question,TAB,REVISION,
cancel+reload+save-as` (one comma-separated field, without the shown line
break). This uses a fresh ID after any failed close Save; answering the
conflict never resumes that cancelled close plan. If the shared dialog
counter is exhausted, no conflict question is created: preserve text and the
old association, retain the failed Save job outcome, and show an exhaustion
diagnostic. IDs are not reused. An idle conflict replaced by native Close
gets a fresh Close ID. Native or remote cancellation followed by another
Save conflict likewise gets a fresh ID, even for identical text/revision.

Conflict answers require the nonzero live ID and the pinned tab/revision,
including for an inactive target. The opaque coordinator revalidates editor
identity and current revision. Missing flow is `unavailable`, wrong ID/tab
is `invalid-argument`, and stale text is `stale-revision`; missing targets
are `missing-tab`. Invalid coordinator state offers no answers and reports
`ID,conflict,invalid,-,-,-`. The approved trusted semantic-answer policy also
applies here: no physical focus, keymap or visibility requirement is added.
Physical non-Cancel answers keep all their existing input/visibility guards.

`reload` is allowed only in the first conflict question. For dirty text it
starts no read and returns `1 ID ok dialog DIALOG`; state changes to
`ID,conflict,discard,TAB,REVISION,cancel+discard-reload`. Only an explicit
`discard-reload` in that second phase can authorize replacing unsaved text.
An initial discard-reload, repeated Reload in the discard phase, or Save As
there is `unavailable`. Neither a `discard` close answer nor a force answer
can substitute for this authorization. For clean text the first Reload can
start the read immediately without a discard question.

Before consuming a Reload permit, reserve the bounded job ID and room for
input cleanup. Busy/closed/counter/history refusals preserve the question
and unconsumed permit. A submitted read returns `1 ID pending JOB` and state
changes to `ID,conflict,reloading,TAB,REVISION,cancel`. The ordinary file
coordinator checks the single-use permit before I/O and again during model
replacement. It alone adopts the candidate baseline after successful model
admission. Missing, invalid, changed-target or failed reads retain text,
history, saved state and the old association. There is no force overwrite.

Reload job rows are `job=JOB,reload,TAB,REVISION,0,STATUS,CODE`, using the
same shared 64-row bound and checked job counter. Tab/revision identify the
requested pre-replacement snapshot, not a later active tab or output revision.
`complete,-` means the candidate was accepted by the ordinary model/file
path. Query fresh state/text for the new revision. Completion replaces text,
resets selection/undo history and preserves tab identity, active-tab choice
and formatting/view preferences under the ordinary Reload contract.
I/O, submission and model-admission failures are `error,unavailable`, with
the specific diagnostic retained natively. A defensive permit-creation
failure may retain its model error code or `protocol`. Like other jobs,
`pending` acknowledges admission even if startup already failed.

`cancel` is allowed in any conflict phase and returns `1 ID ok` with an empty
trailing field. During Reload it drops the permit, not the read syscall;
the eventual result cannot replace text or adopt its baseline. A remotely
started Reload becomes `cancelled,-` immediately, including cancellation by
physical Escape/C-g. Its terminal row is not rewritten when the worker
finishes. The file-busy flag remains set until that read finishes; edits may
resume meanwhile, but unrelated file work/close still refuses while busy.
Cancelling a question starts no job and preserves the original file notice.
An unreachable registry-correlation failure may report an error after Cancel
has already dropped the permit; that defensive error cannot undo cancellation.

`save-as HEX_PATH` is allowed only in the first conflict question. It directly
queues the same revision-pinned Save As job for that target, then dismisses
the conflict. It does not open a path prompt. It preserves the external file
and refuses existing/reserved destinations under ordinary file policy.
Paths share the 1..4096-byte non-NUL OS-byte hex grammar and Debug/state
privacy. Counter/busy refusals retain the question. Failed accepted jobs
retain text and do not resume a previous close plan. Native Save As still
opens its ordinary keyboard path prompt; it can be answered with its fresh
path ID under the following contract.

## Ordinary path answers and Dictionary jobs

Opening a native Open, Save As or Dictionary path entry also mints a fresh
checked dialog ID and captures an editor-bound tab/revision point. State
reports `ID,SCOPE,path,TAB,REVISION,cancel+path`, with scope `path-open`,
`path-save-as` or `path-dictionary`. Save As reached through a conflict gets
a fresh path ID, and its target can remain inactive. Close-driven Save As
instead retains its existing close ID and close scope. Typing, empty Return
and input loss keep the current ID. Cancelling and reopening never reuse it.
Counter exhaustion refuses new path creation with a visible diagnostic and
retains documents; native input cleanup may already have run. Native conflict
Save As dismisses its conflict before attempting path creation, so exhaustion
also loses that question; a later Save can raise the conflict again. Associated
native Save needs no path prompt and consumes no path ID.

`dialog-answer DIALOG TAB REVISION cancel` dismisses only that live prompt
and returns `1 ID ok` with a trailing empty field. `path HEX_PATH` replaces
any partially typed entry with the explicit literal OS-byte path. It shares
the existing 1..4096 non-NUL-byte hex grammar and never exposes the path or
entry text in Debug/state. Wrong ID/tab is `invalid-argument`; wrong or
changed revision is `stale-revision`, a missing target is `missing-tab`, and
an editor-identity mismatch is `invalid-argument`. State reports
`ID,SCOPE,invalid,-,-,-` for an invalidated point and offers no answers.
Missing/closed/unidentified prompts and other answer kinds are `unavailable`.
These trusted semantic answers do not require physical focus, input readiness
or visibility. Native path editing retains its ordinary input behavior.

Before queueing work, the existing file-busy, controller-counter and job
capacity guards run. A refused answer retains the exact prompt, ID, entry
and history. An accepted Path returns `1 ID pending JOB` and drops the entry,
even if startup already failed; native diagnostics retain the failure detail.
Open uses the same Open worker/job as a direct command; Save As uses the
same queued revision recheck and immutable snapshot job. Their existing
target, new-destination and publication semantics do not change. Open and
Dictionary points authorize the answer, not later worker results: subsequent
edits or tab changes cannot retarget the supplied path, but do not cancel
an already accepted read. The Save As job independently rechecks its queued
point at handoff. A lost reply or disconnect is not job cancellation.

Dictionary Path uses the ordinary bounded read/parser and window-wide
dictionary replacement. Its job is `job=JOB,dictionary,0,0,0,STATUS,CODE`:
the three zero fields indicate a global operation, not a document/scan.
It shares the checked job counter and 64-row history. `complete,-` means
the validated dictionary was installed; `error,unavailable` preserves the
specific native diagnostic and the old dictionary/results. Successful
replacement clears old marks/scans across tabs without starting a new scan,
changing text or editing history. The global file slot remains busy until
completion. No cancellation RPC or download/bundled dictionary is added.
Terminal rows are historical; later replacements do not rewrite them.
Native keyboard Return still submits through its ordinary path without a
remote job row. This endpoint now answers all file/close/conflict dialogs;
other UI modals accept decoded keys as specified next. `prompt-state`
complements their coarse flags with exact read-only entry inspection.

## Decoded keys

`1 ID key TAB REVISION GENERATION HEX_CHORD` delivers one logical chord to
the same native handler as a translated key press. `GENERATION` must be the
exact current `input-generation` from state, not controller generation,
`window-generation` or a frame callback. It fences native prompt, menu and
selection context as well as the named active tab/revision. Zero is
`invalid-argument`; any different generation is `stale-revision`.
Input invalidations are conservative: resize, notice changes or other input
can make a queued key stale even when document bytes did not change. Query
again after an explicit refusal; never blindly retry a lost reply. An old
Return cannot select a newly opened menu or answer a later prompt merely
because its text revision is unchanged.
Every admitted key invalidates this generation, even when ignored, so obtain
fresh state before the next key. Prefer semantic commands where available.

The input counter starts at one and advances on every native redraw
invalidation except a clock-only caret visibility transition. Idle caret
blinking therefore does not stale a queued input request. Clock processing
still fences clipboard completion and any other input/UI change normally;
text, selection, layout, menu, prompt, notice and job-related damage are not
exempt. The checked counter never wraps or reuses a value. Exhaustion of
either input or redraw generation poisons both fences and stops the window.
The counters may happen to be numerically equal, but are not interchangeable.
This removes blink-driven starvation, not contention from real context
changes, and supplies no bounded latency or general admission guarantee.
Frame submission/callback snapshots and `wait-frame` keep using the separate
redraw generation, including caret-only redraws.

The chord is 1..=32 UTF-8 bytes in lowercase hex, with no control characters.
Use names such as `Return`, `Tab`, `Space`, `C-x`, `C-S-s`, or a single
printable Unicode scalar, following the existing logical key vocabulary.
Empty/control text is `invalid-argument`, oversize is `limit`, invalid UTF-8
is `invalid-text`, and malformed hex/arity/numbers are `protocol`. Debug
reports only the byte count. The same bounds apply to constructed requests.
No evdev code, XKB mask, physical serial, held-key state or repeat timer is
supplied or invented. Each request is a fresh non-repeat delivery; Emacs
prefixes remain live across admitted keys under their ordinary rules.

Unlike trusted semantic edits/dialog answers, decoded keys require a
configured window, actual keyboard device and validated map, focus and
synchronized modifiers. They cannot enter during a physical serial's dynamic
scope. Native state exposes `key-ready=0|1` for this availability guard; it is
not a reservation or a guarantee of target/counter admission. Closed/quitting
windows and every file path, close, conflict or pending-Reload flow refuse
`unavailable`, even for Escape. Use explicit `dialog-answer` with its live
ID/tab/revision instead. Synthetic Save or Close may open those flows, but
another key cannot approve them. Menus, Find, Replace, numeric and command
prompts do accept keys; their ordinary target and visibility checks remain.
The endpoint cannot create focus, synchronize a seat or enlarge a window.

Availability runs first, then the generation fence, missing/stale tab checks,
active-tab check (`invalid-argument`), chord validation and a conservative
eight-dispatch controller-counter reservation. Admission refusals preserve
UI, prompt entry, pending Paste, repeat and history. After admission, cancel
the prior pointer gesture/repeat/Paste through shared cleanup, then call the
native chord handler. Observe search/spelling/job invalidation afterward;
do not cancel a new Paste started by that chord. Native typing invokes Auto
Fill and ordinary undo grouping; it is not semantic Insert. A new scan or
file operation started by a key follows native behavior and creates no
remote job row. Use semantic file/spelling commands for job receipts.

Success is `1 ID ok` with an empty trailing field: the chord was delivered,
not necessarily bound, effective, saved or presented. Ordinary ignored keys,
unknown bindings and native command refusals retain their existing visible
feedback; they do not turn this reply into an operation-completion receipt.
A native adapter error may queue `unavailable` after delivery, then terminates
the window with the same nonzero-error shutdown as physical input. The error
is latched before any further control admission or drawing: a partially sent
Wayland message must not be followed by more output on a damaged stream.
Shutdown may drop that error reply; its delivery is not guaranteed. Prior
cleanup or effects are not rolled back. For ordinary delivery, query state/text,
dialog/job fields and `wait-frame` separately. Closing the final clean tab
can end the window before reply delivery, under the existing EOF ambiguity.

Copy/Cut cannot obtain clipboard ownership from a decoded key: their shared
native path still requires a current physical press serial. Menu activation
does not fabricate one either. Paste can request only an already advertised,
supported compositor selection under the existing focused data-device policy;
it revalidates its captured document/selection at completion. The private
endpoint has the editor's existing authority, not a route to other windows
or clipboard offers the compositor did not supply. No new clipboard RPC is
added. Replay keeps its separate, controller-only `key TAB REV HEX_CHORD`
grammar; its keys do not perform native file or clipboard I/O.

## Decoded pointer input

`1 ID pointer TAB REVISION GENERATION PHASE X Y EXTEND` supplies one native
pointer action. The phase is exactly `press`, `move` or `release`; `EXTEND`
is exactly `0|1` and affects only a text press, like Shift-click. Coordinates
are unsigned integer surface pixels, not buffer pixels, text offsets or
24.8 wire integers. Each is 0..=8,388,607, admitting an exact checked 24.8
conversion. Negative syntax is `protocol`; a representable decimal outside
the coordinate range is `invalid-argument`. Existing decimal overflow is
`protocol`. Bad phase is `invalid-argument`; bad flag/arity is `protocol`.
Programmatically constructed requests get the same coordinate bound.
No pointer-enter/leave, button identity, physical serial, axis frame or
compositor cursor-warp request can be supplied. Normalized wheel frames use
the separate operation below.

Availability requires configuration, an actual pointer device and a current
compositor enter for this surface, no closed/modal state and no dynamic
physical activation serial. Unlike keys, keyboard focus/map readiness is
unnecessary: ordinary pointer selection already works without them. State's
`pointer-ready=0|1` exposes availability, not counter/target admission.
Menus accept hover/activation; all other native modals refuse `unavailable`,
including file/close/conflict flows. Pointer coordinates never approve a
discard or bypass an explicit dialog answer. A tab close mark starts the
ordinary close coordinator, including for an inactive dirty tab.

A menu press can enter Find/Replace/numeric/command entry, where pointer
actions no longer work. `prompt-state` plus pinned `prompt-answer` can
complete or dismiss a valid prompt without keyboard readiness. Physical
input and ready decoded keys retain their ordinary prompt behavior.
If a pending file Open changes the active tab after a prompt opens, its
target becomes stale. Strict remote Cancel also refuses it; a pointer-only
client needs keyboard readiness restored to dismiss that retained entry.
Do not enter these prompts during pending file work when no keyboard path
is available. A fresh input token does not bypass the stale target fence.

The nonzero exact `input-generation` fence and active tab/revision admission
match decoded keys. The named active tab is the input context, not necessarily
the tab hit by a tab-strip click. Hit testing resolves that tab under the
same fence. Counter admission conservatively reserves eight possible dispatches
before input cleanup. Refused requests preserve selection, modal state, Paste,
repeat and any live gesture. Oversized surfaces/coordinates never bypass the
ordinary geometry and document bounds; out-of-text drag motion clamps through
the shared controller. Fractional physical coordinates retain their existing
strict midpoint rules in the common native path.

A text press captures a separate editor-bound remote drag point only when
the controller actually starts a drag. A tab/menu/chrome press does not create
one. State exposes `pointer-drag=TAB,REVISION` for a still-valid active point
or `-`; this never reports a physical gesture. Move/Release can continue only
that remote point, revalidating its owner/revision and controller drag target.
A changed point refuses with its model error, and a lost controller gesture
is `unavailable`; a fresh Press can replace it. Release ends the remote drag.
Without an owned remote gesture, Move may hover a menu and Release is ignored;
neither may continue an existing physical drag. Each request still needs fresh
input-generation admission; the gesture is not a generation bypass.

Remote input cancels prior physical drag/wheel/repeat/Paste context, including
the adapter's remembered held flag. It never changes the physical pointer's
remembered coordinates or enter serial, claims a physical button press, or
moves the compositor's visible cursor. Every later real
pointer event or decoded native key cancels a remote gesture. Semantic edits
and modal transitions use their existing input cancellation. A lost socket
reply/disconnect does not synthesize Release or Undo; an admitted gesture is
window state, not connection-owned state. A later remote Press, key, semantic
edit or physical input can end it. No timer, worker or new gesture allocation
loop is introduced.

Every admitted phase, even an ignored Move/Release, cancels a Paste that was
already pending. It is an explicit remote input boundary, not a physical
button-release acknowledgement. Activate a menu item with a single Press;
omit redundant Move/Release phases after the menu consumes its click.
Pointer and native input share menu hover, hit testing, selection/drag and
close routing. Copy/Cut through a menu still requires an actual physical
serial. Paste can use only an existing compositor offer under the ordinary
focused data-device policy and remains live after the action that starts it.
Post-action search/spelling/job observation does not cancel that new
transfer. As with decoded keys, `1 ID ok` with a
trailing empty field means delivery, not an effective click, file completion
or presentation. Native adapter errors are fatal under the same contract;
an error reply may be lost during shutdown. Native pointer-started file/scan
actions create no remote job receipt. This is decoded editor input, not raw
Wayland injection or control of another application.

## Decoded wheel input

`1 ID wheel TAB REVISION GENERATION ROWS COLUMNS` delivers one normalized
scroll frame through the same handler as the physical wheel decoder. Rows
are visual document rows; columns are monospaced layout cells. Positive
values move down/right, negative values up/left. These are not wheel notches,
pixels or Wayland fixed-point distances. Physical discrete notches decode
to three units; physical smooth-distance accumulation remains native-only.
Each remote request is already one complete frame, with no retained axis
fractions, source metadata, stop events or partial frame state.

`ROWS` and `COLUMNS` are the sole signed-decimal request fields: an optional
single minus followed by one or more digits, with no plus, whitespace or
fraction. Leading zeros and negative zero are accepted. Each magnitude must
be at most 16,777,216, the native normalized clamp. A representable decimal
magnitude beyond the bound is `invalid-argument`; malformed syntax or `u64`
magnitude overflow is `protocol`. Constructed requests get the same bound.
Both axes may be nonzero in one frame. Ordinary viewport bounds clamp the
result; soft wrapping continues to suppress horizontal scrolling.

Availability requires `pointer-ready=1` and no menu. All native modals,
including menus, refuse wheel delivery with `unavailable`; no keyboard focus
is required. State exposes `wheel-ready=0|1` before target/counter validation.
The nonzero exact `input-generation`, current active tab/revision and eight
possible controller dispatches are checked before any cleanup. Caret-only
blinks do not invalidate the request; viewport, selection, layout and other
input changes do. The active tab is the scroll target, not a tab under the
physical cursor. There is no cursor warp or synthetic physical serial.

Native admission checks readiness, nonzero/current generation, model
revision, active tab, delta bounds and counter headroom in that order.
An inactive tab with a stale revision returns `stale-revision`; with its
current revision it returns `invalid-argument`, matching decoded keys and
pointers. Wire bounds are checked at parsing first; a constructed request
that is both stale and out of range gets the earlier native guard's error.

Every admitted frame, even `(0,0)` or an edge-clamped no-op, is an input
boundary: cancel remote/physical drags, repeat, pending Paste and accumulated
physical wheel context before delivery. Preserve physical coordinates and
compositor enter. Refusals preserve them all. No remote fraction can combine
with later physical input. Scrolling changes view only, not document bytes,
selection, revision, dirty state or undo history. Observe ordinary search,
spelling and job state afterwards and invalidate the input token, including
on a no-op. `1 ID ok` plus a trailing empty field acknowledges delivery, not
an effective scroll or presentation; use the redraw frame contract for the
latter. A lost reply is not safe to retry blindly. No background job is made.

## Revision-checked editing

All requests below name a stable tab ID and its expected text revision.
Arguments are tab-separated, and selection endpoints are directed UTF-8
byte offsets, not scalar indices or a sorted range.

| Payload fields | Meaning |
| --- | --- |
| `1 ID select-tab TAB REVISION` | Activate that tab after checking its revision. |
| `1 ID select-range TAB REVISION ANCHOR CARET` | Set the active tab's selection to these scalar-aligned byte offsets. |
| `1 ID insert TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET HEX_TEXT` | Replace the expected selection with decoded text. |
| `1 ID delete TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET` | Delete the expected selection, or the following scalar when empty. |
| `1 ID undo TAB REVISION` | Undo one transaction in the active tab. |
| `1 ID redo TAB REVISION` | Redo one transaction in the active tab. |
| `1 ID fill-paragraph TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET` | Fill the paragraph at the expected caret through the ordinary controller. |
| `1 ID set-auto-fill TAB REVISION ENABLED` | Set this tab's Auto Fill mode; `ENABLED` is exactly `0` or `1`. |
| `1 ID set-fill-column TAB REVISION COLUMN` | Set this tab's fill column, 20..=240. |
| `1 ID go-to-line TAB REVISION LINE` | Collapse selection at the start of the one-based logical line. |
| `1 ID set-key-profile TAB REVISION PROFILE` | Set the whole window's key profile to `windows` or `emacs`. |
| `1 ID set-line-numbers TAB REVISION ENABLED` | Set the whole window's logical-line gutter; exactly `0` or `1`, default `1`. |
| `1 ID find TAB REVISION EXPECTED_ANCHOR EXPECTED_CARET HEX_NEEDLE BACKWARD WRAP` | Find a literal match from the expected selection; flags are exactly `0` or `1`. |
| `1 ID replace TAB REVISION HEX_NEEDLE HEX_REPLACEMENT` | Replace all nonoverlapping literal matches in the whole active document. |

Success is `1 ID ok` followed by one trailing Tab (an empty body).
It means controller admission completed, not saved bytes, client receipt,
a frame callback or physical presentation. Query state afterward to obtain
the current revision/selection; another input may intervene before that query.
No history to undo/redo and no text-changing work follow the existing model's
no-op rules; an admitted command can advance controller generation without
advancing text revision or adding history.

The native adapter first refuses with `unavailable` while closed, quitting,
or any path, close, conflict, pending-Reload, menu, Find, numeric, command or
Replace modal is present. It neither dismisses nor answers the modal, and a
refusal does not replace the visible notice. Queries remain available.
Outside modals, a missing tab is `missing-tab`, a differing text revision is
`stale-revision`, and all operations other than `select-tab` require that tab
to be active (`invalid-argument` otherwise). Insert/Delete/Fill/Find additionally
require the exact expected directed selection (`invalid-argument` on mismatch).
These checks run on the UI thread immediately before dispatch. A selection
that moved away and back without text edits passes the value check; Undo
cannot resurrect an old text revision. Select Range deliberately specifies
its destination, rather than capturing an earlier selection.

Find is case-sensitive and literal, not a regular expression. Forward search
starts at the selection's sorted end; backward search ends at its sorted
start. With `WRAP=1`, a failed directional search retries the whole document
from the appropriate end; it can select the same occurrence again. With
`WRAP=0`, it never wraps. A match selects its half-open UTF-8 byte range in
forward direction. Native Find reports `no-match` only after admission and
preserves all state. This is distinct from a native modal/closed-window
`unavailable` refusal, so a client need not infer absence from a racy state
query. The controller retains its existing `Unavailable` result; the native
adapter classifies that result only after its own admission guard.
The needle accepts 1..=4096 decoded UTF-8 bytes, including literal newlines;
it is not normalized. Empty needles are `invalid-argument`, oversize is
`limit`, invalid UTF-8 is `invalid-text`, and malformed hex/flags are
`protocol`. The decoded-size bounds also apply to directly constructed
requests, not just wire parsing. Wire size, hex, UTF-8 and flags are parsed
before UI admission; empty needles and unsupported replacement text are
model validation after tab/revision/selection guards. For example, an empty
needle on a missing tab is `missing-tab`, not `invalid-argument`.

Replace uses the same literal needle bound and accepts 0..=262,144 raw
decoded replacement bytes before ordinary insertion normalization (CRLF to
LF). It replaces every nonoverlapping match, regardless of selection, with
one ordinary undo transaction and the model's document/history budgets.
It does not invoke typing's Auto Fill. A text-changing replacement places
the caret at EOF; Undo restores the previous directed selection. Empty `-`
deletes matches. No matches, or an identical resulting document, follow the
model's admitted no-op rules: no matches leave selection intact, while an
identical replacement still moves the caret to EOF without a text revision
or history entry. Neither command changes the native Find/Replace
entry history or creates a prompt. Remote Find's explicit wrap flag is
independent of the interactive end-then-repeat-to-wrap gesture; it does not
seed or advance F3 history. Both paths dispatch the same model commands.
Native prompts additionally manage their own wrap gesture: a remote no-match
Replace All leaves an existing F3 boundary intact because its revision and
selection are unchanged, unlike submission through the native Replace prompt.
Selection/revision-changing remote commands clear that boundary immediately;
moving away and back cannot revive it.

The input bound is not an output/change-size bound. Replacement may expand
to the model's 16-MiB document ceiling, with additional aggregate text/history
limits checked before mutation. Like native Replace All, admitted Find and
replacement run synchronously on the UI thread; the transport deadline
cannot preempt an already executing model operation or roll it back.

Insert accepts at most 262,144 raw decoded UTF-8 bytes before normalization.
Oversize is `limit`, malformed hex is `protocol`, and invalid UTF-8 or
unsupported text is `invalid-text`. CRLF normalizes to LF; existing BOM and
line-ending flags are unchanged. An empty `-` replacement deletes a nonempty
selection. Insert never invokes typing's Auto Fill. Each text-changing
Insert/Delete/Fill is one ordinary undo transaction, with the model's
document/history budgets and no-op rules. Invalid boundaries return
`invalid-position`, exhausted budgets return `limit`, and counter exhaustion
returns `exhausted`, without changing model, input or view state.
No intermediate Select is dispatched to implement an insertion.

Accepted operations use the existing `Controller::dispatch` path, reset its
prefix/mark/drag, cancel native repeat/wheel/drag and pending Paste, observe
search-wrap and spelling invalidation immediately, and request redraw.
Cancelling an in-flight Paste emits a visible cancellation notice; other
accepted edits do not replace an existing notice.
Selection-only changes retain spelling marks; edits and Undo clear stale
marks without starting a scan. Semantic commands do not require keyboard
focus or a physical-input serial; they cannot acquire clipboard ownership.
Ordinary file jobs may remain active while editing, as with keyboard input;
this subset neither initiates file I/O nor acknowledges a save. An edit before
a queued remote Save's handoff invalidates that job, as specified under
Save and Save As jobs above.

Mode setters and Go To Line share that modal, active-tab, revision and input
invalidation policy. They do not change text, text revision, dirty state or
undo/redo history, and retain spelling results. Profile selection affects
the whole window but still requires a live active tab/revision as its
admission guard; it cancels the old key prefix through ordinary dispatch.
Repeated setters are allowed: even an already-current value advances the
ordinary controller/native generation, cancels pending Paste with its visible
notice, and requests redraw. Admission is an explicit input boundary, not
a value-change-only notification. Flags other than literal `0|1` are `protocol`.
Unknown/case-mismatched profiles are parse-time `invalid-argument` before
tab/revision/modal admission. Column bounds are checked by the model after
those guards; an out-of-range column on a missing tab is `missing-tab`, not
`invalid-argument`. Go To Line counts LF-delimited logical lines in normalized
document text (also returned by `text`), not on-disk CRLF offsets or wrapped
display rows: line zero is `invalid-argument`, past the last logical
line is `invalid-position`, and a trailing LF introduces an empty final line
at EOF. No expected old mode/selection is needed for these explicit setters
and absolute navigation. These are not typing events and never run Auto Fill.

One accepted connection dispatches at most once. `Job::respond_with` checks
its deadline/liveness immediately before invoking the UI handler, in addition
to queue admission and reply checks. A job already expired or cancelled there
does not execute. Expiry, transport failure or disconnect during/after
execution cannot roll back an accepted command; losing its reply means the
outcome is unknown to the client. Peer disappearance is not synchronously
detectable and the worker does not read further input after the first frame.
Request IDs correlate replies only: there is no cross-connection deduplication,
retry guarantee or cancellation RPC. Do not blindly retry a mutation after a
missing reply; inspect fresh state/text first. A deadline bounds transport
admission and output, not the duration of a synchronous admitted model command.

## Text response

Success is `1 ID ok NEXT HEX_TEXT`. `TAB` may name an inactive tab. Missing
tabs return `missing-tab`; a changed revision returns `stale-revision`, even
if Undo restored the same bytes. Revision is checked when answering, not just
when decoding, so queue time cannot silently retarget a page.

`OFFSET` must be a UTF-8 scalar boundary in `0..=text.len()`. Otherwise the
answer is `invalid-position`. `LIMIT` is 4..=262,144 bytes; other values
return `invalid-argument`. The page ends at the last scalar boundary not
beyond `OFFSET + LIMIT` or EOF. The four-byte minimum guarantees progress
unless already at EOF. At EOF return the same offset with `-`; no extra
terminator or BOM is inserted. Bytes are normalized document text, not the
encoded on-disk representation. Paging pins every request's revision; it is
not an unbounded snapshot retained across connections.

The largest text response uses at most 524,288 hex digits plus a bounded
header/next offset, within the frame ceiling. Serialization never copies
the whole document to produce a page.

## Spelling results

This native-only read query names any open tab (including inactive tabs),
its expected text revision and a scan ID. `SCAN=0` discovers the current
status/ID and requires `OFFSET=0`. Later pages must pin the returned nonzero
scan ID as well as the revision. `OFFSET` is a zero-based range index, not a
document byte offset. `LIMIT` is 1..=256 ranges. Queries never start a scan,
load a dictionary, advance scanning or publish partial marks. Use ordinary
F7 or remote `check-spelling` to admit a scan.

Success is `1 ID ok TAB REVISION SCAN STATUS NEXT TOTAL CHECKED UNKNOWN
SKIPPED CAPPED` followed by one or more range fields. All separators,
including the line break shown here, are single Tabs on the wire.

- `STATUS` is `no-dictionary`, `not-checked`, `checking` or `complete`.
- `SCAN` is zero for the first two statuses. Each admitted native scan gets
  a window-lifetime, checked, strictly increasing nonzero `u64` ID, retained
  on completion. Cancel, edit, recheck, tab close or dictionary replacement
  invalidates the affected scan/report; IDs are never reused, even when a
  replacement dictionary has identical bytes. Counter exhaustion returns
  `exhausted` from the ordinary Check Spelling action, retaining prior valid
  work. IDs are not durable across editor processes/socket lifetimes.
- `NEXT` is `OFFSET` plus the number of returned ranges. `TOTAL` is the
  number of stored marks, not the number of unknown words. At `OFFSET=TOTAL`
  the page is empty. No status before completion exposes partial results:
  `NEXT` and `TOTAL` are zero and its four count/cap fields are `-`.
- Completed counts are unsigned decimals; `CAPPED` is `0|1`. Counts cover
  the whole scan, including words omitted from the shared 10,000-mark budget.
  A completed scan with no unknown words has numeric count fields with
  `UNKNOWN=0` and is distinct from an unchecked document.
- Each range is one `START,END` field, in ascending document order, with
  scalar-aligned, half-open UTF-8 byte offsets into the normalized text.
  An empty page has one `-` field instead. No word text is copied.

Validation order is missing tab/revision (`missing-tab`/`stale-revision`),
limit or zero-scan/nonzero-offset misuse (`invalid-argument`), nonzero scan
mismatch (`stale-revision`), then offset beyond stored marks
(`invalid-position`). This includes cancelled/replaced reports and edits
followed by Undo; old pages never become valid again. A fresh zero-ID query
can discover a newer scan, but cannot continue an older page sequence.
Pending scans accept only offset zero. Status and pages are borrowed on the
UI thread without changing selection, generation, notices, modals or history;
queries remain available during modals. Existing whole-request transport
bounds apply. Each page is below 16 KiB under the document/range ceilings,
and serialization neither copies the whole document nor clones its marks.

## Spelling job admission and outcomes

`check-spelling` is native-only and uses the same action as F7. It takes a
stable active tab and expected text revision, not an expected selection.
Native closed/quitting/modal guards run first, then missing-tab, revision
and active-tab checks. Refusals use the same codes as editing and do not
allocate a job, change UI state or evict history. Job-counter exhaustion is
`exhausted`; a full history with no terminal record to evict is `limit`.
The latter is a defensive registry bound, not a normally reachable wire
outcome under the current one-scan policy.
Neither refuses ordinary F7 or poisons the window's frame counter.

Admission returns `1 ID pending JOB`, with a new nonzero window-local `u64`
job ID. This ID is distinct from the caller's request ID, spelling scan ID,
tab/revision and frame generation. It never wraps or resets in that window.
`pending` acknowledges the job, not that it is still running when received:
startup can already have completed with an error. Accepted admission invokes
the ordinary F7 input/Paste/repeat/drag/wrap cancellation and visible feedback,
even if no dictionary is loaded. Unlike remote text editing, ordinary F7
silently cancels an incoming Paste and clears an existing notice on
successful scan startup; remote Check Spelling intentionally does the same.
No dictionary produces a terminal
`error,unavailable` job with scan zero and the usual visible dictionary
notice. Other startup errors retain their stable code. These are accepted
job outcomes, not side-effect-free admission refusals.

The job uses the current explicitly loaded local dictionary. It does not
open a file, select a dictionary, download data, change text/revision/history,
or start interactive checking. One bounded spelling step runs per ordinary
outer turn; no partial marks/counts appear. A recheck replaces the existing
scan just like F7. Socket I/O stays on the transport worker. The transport's
five-second deadline and liveness checks govern admission/reply, not the
accepted background job's lifetime. Disconnect after admission cannot undo
or cancel the job; a lost reply is ambiguous and must not be blindly retried.

Native state adds `job-last=N`, the largest admitted ID (zero initially),
followed by job fields in increasing job-ID order. Spelling rows are
`job=JOB,spelling,TAB,REVISION,SCAN,STATUS,CODE`; Open, Save/Save As, Reload
and Dictionary rows are defined above.
Scan zero means spelling startup produced no scan. Status
is `pending`, `complete`, `cancelled`, or `error`; code is `-` except for a
stable error code with `error`. Pending/complete spelling rows name the
real nonzero scan ID, which can be passed to `spelling-results` with that
tab/revision.
A matching completed report completes the job. While pending, changed text
is `error,stale-revision` and a closed tab is `error,missing-tab`; explicit
scan cancellation, recheck or dictionary replacement is `cancelled`.
Cancellation does not restart work. A low-level scan failure that removes
the scan is also observed as `cancelled`; its native notice remains the
more specific diagnostic.

Terminal rows are historical outcomes. Later edits, Undo, tab closure or
dictionary replacement do not rewrite them or promise their marks still
exist. `spelling-results` independently validates the current report.
At most 64 job rows may be retained. Admission at capacity evicts the oldest
terminal row, never pending work; no wall-clock expiry or text payload is
stored. A requested ID at or below `job-last` absent from the rows has been
evicted: its outcome is unknown, not failed or never run. No eviction occurs
on a refused admission. The window's one-scan policy ordinarily leaves at
most one pending spelling job. Observation runs after native events, after
the outer-turn spelling step and after accepted remote actions; state
serialization itself only borrows validated job records.

## Frame acknowledgement

Native `state` supplies `window-generation=N`, a checked redraw-invalidation
counter starting at one for the initial pending frame. Main-surface redraw
requests advance it, including native notices, menus, dialogs, spelling and
caret changes. Conservative invalidations and no-op edits may also advance
it; multiple invalidations can coalesce into one submitted frame, so values
may be skipped in the submitted/completed streams. This is separate from
the controller's `generation` and is not a version of every native job flag.
It is local to one window lifetime, not durable across endpoint restarts.
Exhaustion of either input or redraw generation poisons both fences,
including `wait-frame`, and stops the window. Multiple invalidations in one
clock turn may advance the redraw counter more than once.

`wait-frame GENERATION` requires `1..=window-generation` at UI admission.
Zero or a future value is `invalid-argument`. The request has no side effects,
does not request a redraw or dismiss a modal, and does not require focus.
If a matching callback already completed, respond immediately. Otherwise
retain the job until a main-surface frame of at least that generation receives
its `wl_callback.done`, or the existing request deadline/cancellation wins.
There is no intermediate `pending` response and no new timeout clock.
Transport expiry closes the connection without a guaranteed error frame;
an occluded/unconfigured window need not produce a matching callback at all.

Success is `1 ID ok GENERATION,CONTROLLER,TAB,REVISION,WIDTH,HEIGHT,SCALE`.
The seven numbers form one comma-separated field after the ordinary Tab
header. They describe the immutable snapshot used to paint and submit the
acknowledged buffer, not the controller when the callback or reply arrives.
`TAB=0,REVISION=0` means the rendered snapshot had no active document.
Other fields retain their existing units: UTF-8 document revision, controller
generation and buffer pixel geometry/integer scale. A newer completed frame
may satisfy an older request, and edits can intervene before the reply.
Check the returned tab/revision when testing a particular edit.

Native state appends `frame-submitted` and `frame-completed`, each either
`-` (none) or the same seven-number snapshot. A sent commit records submitted
state; only the matching outstanding main-surface callback records completed
state. `wl_buffer.release` controls buffer reuse independently and never
satisfies a wait. No response claims physical scanout, visibility, screenshot
capture, compositor decorations or the separate pointer-cursor surface.

At most eight jobs may be held, sharing the worker's existing eight live
connection slots. Each outer turn inspects every held job once, drops dead
jobs, and shares a two-action budget between ready held replies and newly
admitted requests. Held replies have priority; unresolved holds consume no
reply budget, so other admitted requests and Wayland dispatch continue.
Eight holds can occupy all transport slots until completion/expiry; this
endpoint promises bounded resource use, not per-client fairness. Shutdown or
control disable drops all held jobs. If the defensive held-capacity check
refuses admission, the live request receives `limit`; it is not held beyond
the cap. Socket I/O remains on the worker.

Native generation exhaustion poisons the window and terminates the event
loop with `exhausted`, instead of wrapping, continuing redraws or reporting a
false acknowledgement. The last admitted input may already have changed the
model; this terminal display failure is not an undo/rollback or persistence
guarantee. Subsequent control admission and drawing refuse the poisoned
state, and ordinary endpoint cleanup cancels pending waits.

## Controller state response

Success begins `1 ID ok` followed by these tab-separated fields, in order:

1. `active=TAB_OR_0`, `keys=windows|emacs`, `prefix=0|1`.
2. One `tab=ID,REV,DIRTY,BYTES,ANCHOR,CARET,AUTO_FILL,FILL_COLUMN,BOM,ENDING`
   for each open tab in ascending ID order. Flags are `0|1`; `ENDING` is
   `lf|crlf`. Selection endpoints are directed UTF-8 byte offsets.
3. `generation=N`, `window=WIDTH,HEIGHT,SCALE`, `focus=0|1`,
   `line-numbers=0|1` (window-wide, on by default).
4. One `view=ID,ROW,COLUMN,COLUMNS,ROWS,WRAP,AFFINITY,DESIRED_COLUMN` per tab
   in ascending ID order. `AFFINITY` is `upstream|downstream`; an absent
   desired column is `-`. View coordinates are the existing controller's
   zero-based visual row/column origin, not document byte offsets.

There are at most 64 tab/view pairs. No text bytes, file path, title, dictionary,
pending dialog/job or spelling range is serialized by this controller-only
snapshot. Its generation means local UI state, **not** a submitted buffer,
frame callback or scanout. The native extension below adds its own redraw
generations/snapshots, coarse flags, bounded spelling/file outcomes and close
and conflict/path dialog identity/answers. Decoded key and pointer admission
are connected, including normalized wheel frames. The separate `prompt-state`
query exposes native entry values and feedback under its contract above;
`prompt-answer` connects pinned non-file entry/actions independently of
physical readiness.

## Experimental native adapter

`--control-socket PATH` may appear once after `--window`, before the literal
`--` delimiter. Its next argument is one literal OS-byte pathname, not shell
text. It must satisfy the complete private-socket contract below. The caller
creates the private parent; no directory or endpoint is discovered, adopted
or repaired automatically. Giving access to the endpoint grants read/write
access to every tab's current in-memory text, including unsaved and inactive
tabs within the implemented operation subset above.

Startup binds before opening document/dictionary files or connecting to
Wayland, so an invalid endpoint fails startup without those operations.
The worker starts only after file preparation and window construction.
An earlier startup failure drops the socket and attempts checked cleanup;
no response is available during that interval. Pathname existence or a
successful connect is therefore not a readiness signal. A successful state
response proves the UI is answering, not that a frame has been presented.
Startup cleanup is best-effort; an abrupt death can leave a stale socket,
which the caller must inspect and remove explicitly before retrying.

The adapter shares at most two live actions between held frame replies and
new job admission at the end of each outer event-loop turn, after file/timer
processing and the single spelling step. It does not multiply that allowance
for each decoded Wayland event. Each worker poll may discard its existing
bounded prefix of expired jobs. While control is enabled,
the ordinary receive wait is capped at ten milliseconds, including idle time.
This opt-in latency tradeoff can wake an otherwise idle window 100 times per
second; the default window adds no control polling. All socket reads and
writes stay on the worker. Two maximum pages can allocate about one MiB of
hex response text per turn; this is a byte/work bound, not a real-time latency
guarantee. Edits may scan bounded whole documents and recompute layout;
there is no event-loop latency ceiling. State/text serialization only borrows
the controller immutably.
Queries neither answer nor dismiss a modal, move selection, start I/O, mark
a document saved nor invoke an edit.

Native `text` responses are exactly the shared response above. Native `state`
appends these tab-separated fields to successful controller snapshots, in
this order. Error responses are unchanged:

| Field | Value |
| --- | --- |
| `adapter=native-prompt-answer` | Explicit implemented adapter identity. |
| `key-ready=0\|1` | Native key availability, before target/generation/counter validation. |
| `pointer-ready=0\|1` | Native pointer availability, before target/generation/counter validation. |
| `wheel-ready=0\|1` | Native wheel availability; pointer readiness with no menu. |
| `pointer-drag=TAB,REVISION` or `-` | Valid remote drag, never a physical gesture. |
| `native=...` | Configured, file session present, file job busy, quitting. |
| `modal=...` | Path entry, close question, conflict question, pending Reload, menu, Find, numeric entry, command entry, Replace. |
| `spelling=...` | Selected dictionary entry count or `-`, scan running. |
| `job-last=N` | Largest admitted remote background-job ID, or zero. |
| `job=...` (repeated) | Up to 64 ordered spelling/Open/Save/Save As/Reload/Dictionary rows. |
| `dialog-last=N` | Largest admitted close/conflict/path ID, or zero. |
| `dialog=...` | Live close/conflict/path scope/phase/target/allowed answers, or `-`. |
| `input-generation=N` | Current native input-context generation; excludes caret-only ticks. |
| `window-generation=N` | Current native redraw-invalidation generation. |
| `frame-submitted=...` | Last submitted snapshot, or `-`. |
| `frame-completed=...` | Last callback-completed snapshot, or `-`. |

Boolean flags are `0|1`; the dictionary field is an entry count or `-`.
The native/modal/spelling flags remain coarse presence information, not
dialog IDs, allowed answers or spelling ranges. Separate dialog fields expose
close/conflict coordinators and path prompts; job rows expose spelling and
file outcomes. Other modal IDs remain absent, including menus, Find,
Replace, numeric and command entry. No path or entry text
is disclosed by these added fields. Query `text` separately for document
bytes and `prompt-state` for entries/feedback. Controller generation does
not cover native-only modal/job changes, and is not a submitted or
callback-completed frame generation. Clients must not use it as a native
snapshot version or presentation fence. Use
`input-generation` for decoded keys, pointers, wheel frames and non-file
prompt answers, and the separate `window-generation` with `wait-frame`
under the frame contract above.

On ordinary window exit, stop and join the worker and explicitly attempt
checked cleanup; a shutdown error contributes to nonzero exit status. Drop
also cancels the worker on other unwind/owner-drop paths. Incomplete or
nonreading peers cannot keep shutdown waiting for their request deadline.
If the transport thread fails while the editor is live, disable control and
show a retained diagnostic without discarding documents or stopping editing.
Any cleanup failure remains recorded for eventual nonzero exit status.
Invalid response frames report a notice; ordinary expired/disconnected reply
admission is silent and cannot overwrite a user-facing notice. A missing
mutation reply does not undo its already admitted edit.
No automatic rebinding or restart occurs. A reply that was queued but not
delivered is not a claim of client receipt.

Tests use actual private pathname sockets with the native window fixture,
exercise state/text, revision-checked edits and out-of-subset refusals,
preserve modal state, reject stale pages after edit/Undo, answer Wayland
pings with requests outstanding, and
check startup-failure cleanup, timed shutdown with partial/nonreading peers,
and retained cleanup errors without deleting replacement names. A separate
source confinement assertion pins the two-action per-turn budget. Held-frame
socket tests count ready replies across turns, exercise old callbacks after
newer edits, distinguish callback from release, preserve modal reads, service
pings, expire a wait without a callback, and cancel holds on shutdown.
Editing tests compare shared state/history with replay, native keyboard
edits in both profiles and rendered pixels; they cover expected-selection
races, expired jobs, pending-Paste/repeat cancellation, unchanged disk bytes,
and spelling invalidation without rescanning. These are fake-compositor and
local kernel tests, not a live independent-compositor or td-jail/td-mail oracle.
Spelling queries additionally cover native no-dictionary status, complete
range pages matching the window's marks and modal-preserving reads.

## Conformance

Shared control-library tests pin exact request/error/text payloads, matching
state/text responses through replay, immutable controller state, maximum
pages and 64-tab output,
stale-after-Undo rejection, every frame split, single-byte delivery, premature
EOF, zero/oversized/trailing frames, poisoned decoder behavior and arbitrary
byte input both as raw headers and as correctly framed payloads. Invalid
envelopes/read-only command refusals are compared with replay, separately
from the intentionally different mutation allowlists/selection guards.
Spelling-page tests cover cross-tab pending/report isolation, closed targets,
hidden partial results, range/mark limits, scan-ID reuse refusal after recheck
or dictionary replacement, and exhaustion that preserves prior valid work.
These library tests need no display, socket, dictionary file or external process.

## Private socket publication prerequisite

`control_socket::Socket::bind` opens a Linux Unix-domain listener only when
explicitly called. It does not accept a CLI option, create a worker, decode a
request, read editor state or send a response. `accept` and each accepted
stream are nonblocking; connection limits, deadlines, cancellation and UI
integration are separate worker/adapter responsibilities. No new raw syscall
surface or dependency is used. The filesystem operations use safe `std` and
procfs, which must be mounted at `/proc`. Like the existing editor transport,
the guard in `src/sys.rs` requires Linux x86-64 at compile time; these open
flags name that ABI, not all Unix platforms or Linux architectures.

The socket pathname must be absolute, non-NUL and at most 107 bytes, with a
nonempty basename of at most 80 bytes. Trailing slash, `.`/`..` basenames and
parent `..` traversal are refused. Interior `.` and repeated separators have
ordinary `Path` normalization; there is no shell expansion or canonicalization
through symlinks. Non-UTF-8 names remain literal OS bytes. The basename cap
keeps the internal descriptor-relative bind name within Linux's pathname
socket limit independently of the current descriptor number.

The final parent must already exist, belong to the calling UID and have mode
exactly 0700, including no special bits. The binder neither creates nor
chmods directories. Every ancestor, including root, must be owned by root or
the caller and must not be group/other-writable unless sticky. This admits
ordinary `/tmp` while protecting caller/root-owned path components from
rename by another UID. An otherwise private parent inside a non-sticky
shared writable ancestor is refused.

Ownership is interpreted in the editor's current user namespace. Unmapped
ancestor owners are not trusted: the overflow UID (commonly 65534) is not an
alias for root. A rootless container must provide a path whose complete
ancestry satisfies these checks; the optional endpoint otherwise refuses.
Before walking the path, read `/proc/sys/kernel/overflowuid` with an 11-byte
limit plus one byte for overflow detection. Require a decimal `u32`, with at
most one final LF. Missing, malformed or oversized data refuses publication.
If the configured overflow value equals the caller UID or zero, refuse:
otherwise an unmapped owner could be numerically indistinguishable from a
trusted owner. This includes callers legitimately mapped to the overflow
number; the adapter cannot distinguish the two cases from stat ownership.
No fixed overflow number is assumed and no sysctl is changed. Concurrent
changes to that global sysctl require the already-excluded privileged host
authority. Linux's [user namespace contract](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)
specifies this substitution for stat and process-status ownership.
Do not relax the predicate based on environment variables or test execution.
The crate declares `trusted-test-root = true` for the cargo gate, which gives
its tests a private caller-owned filesystem root and `/tmp` through the
existing capped runner. See DEVELOPMENT.md's trusted-root fixture contract;
this changes the test environment, not production socket admission.

Read the UID from a bounded `/proc/self/status` read (64 KiB plus one byte
for overflow detection). Require exactly one complete `Uid:` record with
four equal representable `u32` values: real, effective, saved and filesystem
UID. Missing/duplicate/malformed/mixed records refuse. UID zero is allowed
when all slots agree; this is not a sandbox for root. Environment variables
are never evidence of identity.

Walk directory components from an opened root using `O_PATH | O_DIRECTORY |
O_NOFOLLOW`, one component at a time through `/proc/self/fd/N`. These kernel
descriptor links are intentional; no user-supplied symlink component is
followed. Retain the final directory descriptor. Bind through its pinned
descriptor path rather than reopening the user's absolute path. Refuse every
existing final name without connecting to it: regular files, directories,
symlinks, live listeners and stale sockets all remain untouched. A concurrent
binder is still subject to the kernel's exclusive pathname creation.
Socket address diagnostics report the internal `/proc/self/fd/N/name` bind
address, not the requested pathname. A peer must connect using the requested
path it was given, not reuse `peer_addr` as a path in its own descriptor table.
Callers retain the requested path for diagnostics; the guard does not store
another copy.

Binding briefly creates a listening socket with umask-derived permissions
before mode 0600 is set. The already-private parent contains that interval
to the same caller/root trust boundary; changing process-global umask would
race unrelated file operations and is not used.

Open the new filesystem socket inode with `O_PATH | O_NOFOLLOW`, require a
socket owned by the caller, and pin that descriptor too. Set mode 0600
through its kernel descriptor path and read back mode/owner. Rewalk the
visible parent after setup, rechecking trust, private mode and identity;
also verify the named socket still matches the pinned inode. This catches
parent/name changes during publication. The filesystem socket inode is
distinct from the listener's socket descriptor inode; cleanup pins the former.

Explicit `close` makes one checked cleanup attempt and reports errors. Drop
attempts the same cleanup best-effort. Before unlinking the original basename
inside the pinned parent, require socket type and matching device/inode.
A missing name is already clean only after rechecking that the procfs parent
descriptor path remains accessible; this is an accessibility check, not
detection of a renamed or removed parent. A real procfs link still addresses
that pinned inode after either operation. This also handles a name removed
between the identity check and unlink. A replacement file/socket is left alone.
Renaming the parent does not redirect cleanup into a replacement directory.
Keeping the socket inode open prevents inode-number reuse from fooling the
comparison. If binding succeeds but inode ownership cannot be established,
the binder does not unlink an unverified name; a stale endpoint may remain
for caller inspection. Later setup failures attempt normal checked cleanup.

Identity checks and pathname operations are separate kernel operations, not
an atomic compare-and-unlink primitive. These rules protect against other UIDs
under the admitted permissions; they are not isolation from root or another
process running as the same UID. Such a process can race bind-to-pin or
check-to-unlink, rename names, or change permissions, and already has the
endpoint's authority. The caller must keep this path private. Sharing it
across a jail boundary remains a separate explicit grant.

Kernel tests connect through the requested pathname, exchange bounded bytes,
check nonblocking/close-on-exec flags and mode/owner, preserve every existing
endpoint class, reject symlinked/unowned/nonprivate/untrusted ancestors, and
exercise replacement-name and renamed-parent cleanup. A deterministic
publication hook proves a replaced visible parent refuses and only the
pinned candidate is cleaned. These are transport-ownership tests, not a
native editor-control endpoint or an adversarial same-UID race proof.

## Bounded worker

`control_worker::Worker::start` takes an already admitted `Socket`. One named
thread owns the listener and every accepted connection; no socket I/O occurs
in `try_request` or `Job::respond`. The thread has no editor reference or
model lock. The native adapter above owns UI dispatch and command-line opt-in.

Admit at most eight connections. When all slots are occupied, leave further
clients in the kernel listener backlog; do not create descriptors, threads or
request allocations for them. The application does not promise a backlog
connection deadline or backlog size. The five-second whole-request deadline
starts when the worker accepts, never renews on progress, and includes header,
body, UI queue/response time and output. At or after expiry close silently.
Every loop checks each live connection, allowing at most one 16-KiB read or
write for it, then parks for at most ten milliseconds with live connections,
or 100 milliseconds when idle. UI replies, abandoned jobs and shutdown unpark
the worker early. A ready reply attempts its first write in the same step;
a refusal after a read waits until the next step to preserve the I/O bound.
This deliberately paces output near 1.6 MiB/s per connection and a maximum
hex-encoded text page near 330 milliseconds, excluding scheduling delays.
Idle polling trades up to 100 milliseconds of acceptance latency for fewer
wakeups; blocking accept would require a separate reliable shutdown wakeup.
Scheduling delay and kernel execution are not hard real-time guarantees; the
worker never deliberately blocks on socket I/O.

The endpoint is not an availability boundary against the same UID or root.
Such a process can continually occupy all eight slots, keeping legitimate
clients in the backlog. Deadlines bound each admitted connection, not a peer's
share of future admissions. Full UI admission closes without an error frame.

One complete request dispatches at most once, without requiring write EOF.
The shared decoder rejects zero/oversized frames and extra bytes delivered
in the same read. After completion no further request bytes are read: later
bytes never execute a second command. A premature EOF or decoder refusal
queues a framed error with ID zero; a parsed request refusal uses its recovered
ID. Transport failures, deadline expiry or exhausted UI admission close the
connection without a guaranteed error frame. Successful output closes after
its one complete frame, without waiting for the peer to close.

Only the query/edit subset above passes `Request::parse`. Eight typed jobs
fit in the UI queue; submission is nonblocking and a full/disconnected queue
closes that connection. Each job has a one-element response channel and a
liveness token combining the acceptance deadline and connection lifetime.
`try_request` never waits and makes at most eight receives per call, returning
the first live job. After eight expired entries it returns no job; remaining
arrivals or worker disconnection are observed on a later poll. A disconnected
worker reports an error once that bounded expired prefix has been drained.
Dropping a job without a response disconnects its response channel before
waking the worker to close the client.

The native caller consumes `Job::respond_with`, which checks liveness before
passing the borrowed request to its query/edit handler. The handler applies
the modal/target guards above and returns one payload. The underlying
`Job::respond` checks liveness and frame limits before copying/queuing it;
success means queued, not delivered or rendered. It does not validate the
caller's response fields. A disconnected or expired job returns false; invalid
payload size returns the shared frame error. Closing a connection cancels its
queued/held job; a deadline never reserves an old document snapshot. This
transport API does not replace live UI modal/ID/revision admission.

Request payload allocation is at most one MiB per reading connection. Parsing
retains a fixed-size descriptor plus at most 256 KiB of decoded Insert text
and drops the wire payload when moving to UI wait. Parsing may briefly hold
both. UI insertion clones that bounded text for the owned controller command;
the model separately normalizes/adopts it within its existing edit budgets.
Job debug output reports byte counts, never inserted text.
Each response channel/connection owns at most one framed reply
of one MiB plus four bytes. A UI reply may briefly coexist with its original
request allocation during handoff; there are at most eight such connections
and eight queued jobs. One reusable 16-KiB scratch buffer serves the thread.
These bounds exclude caller-owned response inputs/retained jobs, allocator
overhead and kernel socket buffers; there is no unbounded worker byte queue.

Explicit `close` and Drop set the stop flag, unpark and join the worker. It
invalidates all live jobs, closes clients, and uses the socket's checked
cleanup. Explicit close reports worker/cleanup failure; Drop is best-effort.
No detached thread survives a completed join. Interrupted or aborted accepts
and temporary descriptor/memory/buffer exhaustion retry after the normal park;
other listener errors stop the worker and socket Drop attempts cleanup. A
deadline representation overflow also stops it rather than accepting without
a deadline. Shutdown is not a wall-clock promise about filesystem operations
or host scheduling.

Deterministic connection tests inject time and cover bytewise input, complete
and truncated frames, refusal IDs, late extra requests, full queues, abandoned
jobs, invalid response sizes, and expiry while reading, awaiting the UI or
blocked on output when the kernel buffer fills. A separate pre-output expiry
test is independent of socket-buffer tuning. Tests also cover large draining
replies, text request admission, retry classification, idle pacing and explicit
thread-error propagation. Real Unix-socket worker tests cover a framed round trip,
admission backpressure with continued client progress, shutdown cancellation
and owned endpoint removal. These need no Wayland server and do not prove
native editor control or frame synchronization.
