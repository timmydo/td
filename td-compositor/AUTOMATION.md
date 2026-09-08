# Native compositor automation

This is the normative development/test automation contract, complementing
[DESIGN.md](DESIGN.md). td-compositor is the primary integration platform for
td-editor and future td UI clients. Optional disposable Weston checks remain
independent interoperability evidence, not a dependency of native tests or
of the shipped editor.

## Disposable headless sessions

Implemented entry point:

```text
td-compositor headless --session-dir NEW_ABSOLUTE_PATH --width 800 --height 600
```

The caller supplies an existing parent and a directory name which does not
exist, even as an empty directory or symlink. The compositor creates that
directory at mode 0700. It binds `wayland-0` and `td-control` at mode 0600
inside it; existing endpoints are never replaced. Paths are explicit and do
not come from the caller's desktop environment. Clients use the absolute
`NEW_ABSOLUTE_PATH/wayland-0` as their display. Client-owned buffers and
readiness files belong in a separate caller-owned directory.

The caller retains a pipe to compositor stdin. EOF requests normal session
termination; any byte is an error, not an input command. Parent exit closes
the pipe only when no inherited duplicate writer remains, so harnesses must
keep that descriptor private. Stdout and stderr must be drained or redirected
to caller-owned logs. The one stdout readiness record is:

```text
TD-COMPOSITOR-HEADLESS-READY version=2 session=0123456789abcdef0123456789abcdef width=800 height=600 scale=1
```

It follows the initial successful output paint, both listener binds, keymap
preparation, and successful listener/lifetime worker creation. It promises
server startup, not that any application has mapped or completed an action.
The session field is a fresh 128-bit nonce, encoded as exactly 32 lowercase
hexadecimal digits. Startup reads 16 bytes from `/dev/urandom` before creating
the directory and refuses on entropy-source failure; it has no clock/PID
fallback. Reusing a pathname starts another identity. Nonces distinguish
process lifetimes probabilistically, not by a persistent global sequence,
and are identifiers rather than authorization secrets.
Listener retirement fails the session, including an unexpected worker unwind
in a development build: a completion guard reports abandonment even while
other lifecycle senders remain live. An abort still ends the whole process.
The main entry point returns directly to process exit; blocking
Wayland/client/control workers are process-lifetime
workers, not joined library tasks. EOF and ordinary startup/runtime errors
remove only the session's recorded endpoints and its empty directory. Cleanup
checks recorded inode/type identities, preserves replacements and continues
cleaning other owned endpoints. A replaced directory forbids any traversal
through its new identity. Cleanup runs once, never recursively removes client
files, and reports failure with nonzero exit. SIGKILL or a process abort
cannot run cleanup: the harness owns the outer temporary root,
retains exact child handles, bounds startup/exit, kills and reaps only those
children on failure, and removes its own residual fixture files afterward.
The same-UID caller and parent directory are trusted; inode checks prevent
accidental replacement deletion, not adversarial same-UID pathname races.

The output is tightly packed XRGB8888, scale 1, normal transform, with
explicit nonzero dimensions up to 16,384 per axis and 32 MiB of frame bytes
(8,388,608 four-byte pixels).
Other scales, output hotplug and resizing are not implemented. Its ordinary
file backing is unlinked before publication. The same production Framebuffer,
Runtime, Scene, public Wayland dispatch, configure and seat workers, shared
memory ingestion, buffer release and frame-callback code serve the session.
This proves software composition, not GPU execution or physical presentation.

Headless startup opens no framebuffer/DRM/evdev device, starts no launcher,
private portal listener, root authority channel, VM bridge, or status sampler.
The status text stays empty; workspace chrome remains. Synthetic input is
disabled by default. Existing layout commands work on `td-control`.
Normal `run` startup, deployment authority and physical secure attention are
unchanged. All production code remains std-only with no new syscall surface.

## Opt-in input control

Append `--input-control enabled` to the headless command to grant its private
`td-control` endpoint one synthetic seat with keyboard and pointer devices.
Ordinary `run` sessions and headless sessions without that exact option refuse
input requests. Public Wayland clients gain no control endpoint or input
grant. No hardware device is acquired. A trusted-attention-enabled runtime
refuses synthetic input even if incorrectly wired to this adapter.

The `td-ctl` request vocabulary adds:

```text
key <session> <time-ms> <1-247> <down|up>
release-keys <session> <time-ms>
pointer <session> <time-ms> <x> <y> <buttons> <vertical> <horizontal>
release-input <session> <time-ms>
```

`session` is the exact 32-lowercase-hex nonce from readiness. Every synthetic
input request requires it. The runtime checks the expected identity before
touching the seat, so a controller holding the previous generation's nonce
cannot send input after the owner reuses a socket path. Non-headless runtimes
refuse even if an input adapter is incorrectly supplied. This is a generation
guard, not additional authority; the nonce is public to the session owner.
Legacy nonce-less input requests are refused, not inferred from the endpoint.
Ordinary layout commands retain their existing unguarded vocabulary.

`time-ms` is an explicit unsigned decimal u32 Wayland timestamp, including
zero and wraparound; it is not a physical monotonic-clock witness. Codes are
Linux evdev codes, not XKB's code-plus-eight or Unicode text. Both fields
reject signs and overflow. The existing US keymap determines text. Duplicate
downs and unmatched ups are idempotent; repeat is client-owned, with no
synthetic repeat request. Keys pass through the same logical binding policy
and normal Runtime keyboard/modifier delivery as evdev, including consumed
compositor workspace chords and the help overlay. Launcher opening and
process-launch chords report unavailable; they never start a process.
Ctrl+Alt+Esc is ordinary untrusted input here, never secure attention.

The seat belongs to the headless process generation, not an individual
one-request control connection. Held keys and buttons persist across them.
`release-keys` releases all its depressed keys using normal device-removal
cleanup, including consumed shortcuts, but retains Caps/Num lock toggles and
overlay visibility. It does not release pointer buttons. `release-input`
releases both devices, including owed client button releases and compositor
drag cleanup. Repeating either release command is harmless. The owner closes
stdin to end the whole generation, disconnecting clients and discarding all
held state. No detached input controller outlives its session. Callers
sharing the endpoint share this seat and must serialize their scenarios.

Every `pointer` request is one complete absolute report: position, entire held
button mask and wheel deltas. Coordinates are unsigned decimal output pixels,
including compositor chrome, from zero through width/height minus one. The
wire ceiling is 16,383 per axis; coordinates outside the current output are
refused before mutation, not clamped. Exact pixel/extent fractions feed the
normal absolute-device placement path, with no intermediate quantization.
The unsigned decimal button mask is 0..255: bits 0..7 correspond to evdev
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA, BTN_FORWARD, BTN_BACK
and BTN_TASK (272..279). Changes follow ascending button-code order.
An unchanged mask does not repeat a press; two separate reports express a
click. Missing fields and overflow are refused, never supplied from another
caller's partial report. The keyboard and pointer use separate device ids
under the same binding state, so keyboard-only cleanup preserves pointer
button ownership. Releasing Alt still cancels an Alt-held compositor drag
through normal modifier handling.

Wheel fields are signed decimal detents in -120..120, without a plus sign:
vertical positive is away from the operator, horizontal positive is right.
The shared evdev scroll conversion produces Wayland's signs/units. Unlike a
held button mask, a nonzero wheel delta is a new action on every request.
Overlay filtering, hit testing, client grabs, Alt drags, focus, and workspace
gestures all follow the production pointer report path. Each control report
flushes deferred cursor paint; a failed delivery retains device bookkeeping
for cleanup and still attempts the pending paint. This is software output
work, not evidence that a client processed its input or drew another frame.

Successful input now replies with `ok\n` followed by one receipt:

```text
td-action-v1 session=SESSION action=ACTION\n
```

The CLI validates and prints the receipt, including a match against the
request's expected session. ACTION is a positive canonical decimal u64,
with no sign or leading zeros and at most 20 digits. Its per-session counter
starts at zero internally. The first successful seat request gets one;
every later successful request gets the next number, including an accepted
idempotent no-op or release of already-released input. Number capacity is
checked before routing, and exhaustion refuses without touching the seat.
Publication follows complete successful routing under the same runtime lock.
Rejected or failed requests neither publish a receipt nor advance this
counter. Other control orders and output paints do not advance it.

The input CLI uses a 1,024-byte reply ceiling and one five-second post-connect
read/write deadline, as observation does. Missing, truncated, oversized,
malformed and wrong-session receipts publish no successful CLI stdout.
Error/refusal exit codes remain unchanged. There is no compatibility success
for an old server's bare `ok`, which provides no session-bound receipt.

A receipt means input was routed or an idempotent no-op, not that the focused
application processed it or presented another frame. Delivery failure can
follow mutation; requests are not rollback transactions, and an unavailable
or lost reply must not be blindly retried as exactly-once input. Use
`release-input` to recover depressed state or dispose of the session. The
existing request-size and whole-conversation deadline bounds remain in force.
Receipts identify successful routing, not every partial state change and not
an idempotency key that the server remembers for a retry. Callers still
serialize their scenarios. Client-commit observations and processing fences
remain separate increments; completed-output observation is specified below.

## Opt-in public capture

Append `--capture-control enabled` to grant capture on the private headless
control endpoint. This is independent of `--input-control enabled`; either,
both or neither may be enabled. Ordinary `run` sessions cannot enable capture.
The grant must exist at startup, never be obtained through a control request.
The CLI command is `td-ctl --socket PATH capture`. It writes binary PPM to
stdout on success; the caller may redirect that stream to its own file.
The compositor never accepts a capture output pathname.

Capture requests perform a fresh production paint under the runtime lock.
They refuse trusted-attention-enabled runtimes, any visible private attention
scene, incomplete compound updates, failed paint, and queued rather than
completed submission. Before releasing pixels, the backend verifies its
completed byte image equals a separate public `Scene::render` result. The
comparison never uses `Scene::render_display`. This also refuses a previously
private frame merely relabeled as a public scene, or bytes made uncertain by
a partial write. The response is built before the lock is released, so later
scene changes cannot alter it while the client reads. This proves completed
headless software output, not GPU execution, scanout timing or physical glass.

Wire success is `ok\n` followed by exactly:

```text
P6\n
# td-output-v1 session=SESSION output=OUTPUT\n
WIDTH HEIGHT\n
255\n
```

The mandatory PPM comment names the ready record's session and this capture's
completed-output number, using the grammar below. Unstamped legacy captures
are refused by the CLI; this is an atomic development-protocol cutover.
The header is followed by width × height × 3 RGB bytes, with no row padding,
alpha, trailing bytes or newline. Header line endings are single LF bytes;
the notation above spells them explicitly. Errors retain the ordinary
`error ...\n` or `unavailable ...\n` framing and CLI exit codes 2 or 1.
The CLI validates framing, dimensions and exact byte count before publishing
any stdout bytes. It accepts at most 24 MiB of pixels plus 128 framing bytes,
derived from the existing 32 MiB XRGB frame ceiling, and uses fallible buffer
reservation. Its capture exchange has one five-second read/write deadline
after connection, not a fresh budget per read; connect retains the existing
Unix control client's blocking behavior. Server conversation deadlines also
cover capture generation and writes. A timeout or truncated response fails;
it is never a partial successful image.

Capture deliberately causes a paint. It is not a passive query of a historical
frame, and a fresh capture alone does not prove that a client processed an
earlier input event or committed new content.

## Session and completed-output observation

The same startup capture grant enables `td-ctl --socket PATH observe`.
Without that grant, or in ordinary `run`, both observation and capture
refuse. Trusted-attention-enabled runtimes and visible private attention
also refuse both paths. Observation takes a snapshot under the runtime lock
without painting, dispatching input, flushing pending work or consuming a
number. Wire success is `ok\n` followed by one fixed-order record; the CLI
validates the full reply and prints only the record:

```text
td-output-v1 session=SESSION output=OUTPUT current=yes\n
```

SESSION has the readiness nonce's exact 32-lowercase-hex grammar. OUTPUT is
an unsigned decimal u64 with no sign or leading zeros and at most 20
digits; zero is spelled `0`. It increases
once for each immediately completed production software paint in this
headless runtime, including captures and paints whose pixels are unchanged.
It starts at zero before any paint; startup's successful paint establishes
one. Pending, failed and queued submissions do not advance it. A compound
update advances it only when its final paint completes. Exhaustion refuses
the next paint before touching the backend; it never wraps or aliases an
earlier number. Other runtime profiles do not maintain this counter.

`current=yes` means that the latest requested submission completed and no
known paint or compound update is pending at the snapshot. Otherwise the
record says `current=no`, retaining the historical completed number. Zero
is valid only with `current=no`, and never in a capture. This flag does not
run the capture's independent public-byte comparison. Failed writes retain
the historical number without claiming those bytes remain current.

Capture embeds the number assigned to its own fresh completed paint and
constructs the validated pixel snapshot under that same lock. A harness
must compare the returned session with the ready record before correlating
numbers. Compare numbers only within one session; a new session restarts
numbering. Synthetic input takes its own expected-session guard and returns
numbered receipts, but this output query does not contain action state or
provide a historical-frame lookup or wait-for-number command.

Observation replies are limited to 1,024 bytes including wire status, with
the same five-second post-connect read/write deadline as capture. Malformed,
oversized, truncated or expired replies publish no successful CLI output.
The server checks its conversation deadline before each write; lock waits
and synchronous painting are not preempted by that check. The client's
deadline bounds waiting for a response, not the runtime's rendering work.
Neither a number increase nor `current=yes` proves that an application
processed input. Accepted-input receipts are separate from output numbers.
Applications can lag while compositor-only paint continues.

### Applied client commits

With the capture grant enabled, `observe-client <session> <@id>` takes a
passive snapshot for the live client owning that public `layout` window
handle. The session argument has the same exact nonce syntax as input; a
stale session, missing window or retired connection returns `unavailable`
and no record. Malformed requests and absent capture grant return `error`.
The ordinary deployment channel and trusted-attention runtimes refuse it.
The CLI validates the expected session and window, exact field order,
canonical unsigned decimal counters and final newline before printing the
body. The wire response includes the status line shown here:

```text
ok
td-client-v1 session=0123456789abcdef0123456789abcdef window=@12 client=5 commit=8 output=19 current=yes
```

`client` is a nonzero connection identity, never reused within the process.
Allocation refuses before wrapping; the session nonce separates processes.
Connection-ID exhaustion retires the accept worker and, through the owned
worker lifecycle, ends the disposable session. It cannot recover by retrying
another connection, so the listener does not advertise reusable capacity.
`commit` starts at zero and advances once per successful applied surface
transaction by that client, including empty/no-op commits and the initial
empty commit that solicits configure. It is a client-wide publication
counter, not a document revision or an exact count of `wl_surface.commit`
requests. Synchronized
subsurface commits only cache pending state and do not advance it. A parent
commit that applies several cached children advances once; a transition out
of effective synchronization applies its cached subtree and advances once,
even if that transaction is empty. Destruction, capture, ordinary control
and other clients' transactions do not advance this client's counter.

Capacity is checked before admission or scene mutation. An exhausted-counter
refusal preserves the previous valid observation until normal disconnect
cleanup; it did not begin applying a new scene transaction.
Transaction application and
compound settlement must both succeed before publishing the next number
under the runtime lock. An admitted attempt's failure leaves the historical
number and an invalid observation, even if scene state was partially changed
or a later capture successfully paints it. Normal protocol handling
disconnects failed clients.
An outbound notification failure after publication does not revoke a scene
update: publication precedes flushing deferred Wayland events, so this is
not evidence that the client received a callback or buffer release.
Only one small record per live registered headless client is retained;
unregistration removes it. No event history or disconnected-client journal
is accumulated. Ordinary runtimes do not allocate observation records.

`output` is the latest completed output number in the same locked snapshot,
not a claim that the client caused that output. `current=yes` requires a
successful client publication and the same settled/presented condition as
`observe`; a zero commit or output can only be `current=no`. Pending, failed
or queued paints and compound transactions cannot claim current output.
Neither observation query causes a paint, input event or client transaction.
The reply is bounded to 1,024 bytes and the same five-second post-connect
CLI conversation budget as other small automation observations.

For correlated evidence, observe a client, capture, then observe it again.
Require the expected session/window/client, a positive unchanged commit,
`current=yes` at both observations, and the output-number relation
`first_output < capture_output <= second_output`. Otherwise retry within a
test deadline or fail.
This binds pixels to an applied-publication interval; it does not establish
which input caused it, nor cover all state changes outside surface commits.
Native editor tests must also assert the expected editor remote state and
inspect the captured pixels. Input receipts, client publication, completed
output and editor semantic state remain distinct pieces of evidence.

## Opt-in clipboard transfer holds

Append `--clipboard-control enabled` to headless startup to enable a bounded
transfer scheduling control. It is independent of input and capture; neither
of those grants implies it. Ordinary `run` sessions cannot enable it, and a
trusted-attention-enabled or private-attention-visible runtime refuses every
clipboard command. The public Wayland socket acquires no new requests or
authority. This is a deterministic test barrier, not clipboard injection or a
clipboard manager. It reads no payload bytes and never manufactures an input
serial, offer, selection, source, or receive request.

The private control endpoint and `td-ctl` accept:

```text
clipboard-arm <session> <@receiver-window>
clipboard-status <session> <hold>
clipboard-release <session> <hold>
clipboard-drop <session> <hold>
```

Session syntax is the readiness record's exact 32-lowercase-hex nonce. Hold
identities are positive canonical decimal u64 values. Arming requires the
named live window itself to own keyboard focus and a current client-owned
selection; compositor-owned VM selections are excluded. It pins that surface
and source identity, allocates the next checked hold number, and starts one
absolute ten-second monotonic deadline. The deadline does not restart when
the transfer arrives or status is queried. Exhaustion refuses without wrapping.
Only one armed/held record may exist; a new arm refuses while it is active.
A later arm replaces the one terminal record. There is no history or queue.

The next matching receive which passes the production offer, selection and
focus checks moves its exact owned descriptor into the hold instead of
queuing `wl_data_source.send`. No descriptor is duplicated and no bytes are
read or buffered. A receive with stale source/focus does not consume the arm.
While held, another receive for the same receiver/source is refused and its
descriptor closed; unrelated receives retain normal handling. No source
send is queued until explicit release. Release rechecks live window, focus,
selection and deadline under the same runtime lock, then moves the descriptor
through the existing bounded source-delivery queue. `released` means queue
admission, not source receipt, EOF, successful paste, or application mutation.
Queue refusal consumes/closes the descriptor and retains `failed` status;
it cannot be retried. Source-side Wayland dispatch still validates the source
object before sending its event.

Selection revision changes and every surface-focus transition invalidate
active holds, including transitions between surfaces owned by one client
which do not change its selection offer. An ABA focus return cannot revive
one. Either participating
client's removal also invalidates it. A headless lifecycle tick checks expiry,
live window/focus/selection and attention every 50 ms while this grant is
enabled. It takes the runtime lock but never waits on transfer I/O. The tick
interval is not a real-time cleanup guarantee: scheduling and runtime-lock
work can delay it. Commands and receive interception also check expiry before
using an active hold. Without this grant, the original blocking lifecycle wait
is unchanged. Session exit closes any retained endpoint with the process.

Drop closes a held descriptor or disarms an arm. Expiry and invalidation also
close it; they never release it automatically. Closing an unwritten endpoint
can produce ordinary EOF at the receiver, not a special cancellation event.
To test editor cancellation, first observe `held` and editor `incoming=1`,
send the native cancel action, and observe editor `incoming=0` before release.
The source may then encounter a closed receiver normally. Large payloads and
sleeps are not substitutes for this barrier.

Success is `ok\n` followed by exactly one record in this field order:

```text
td-clipboard-v1 session=0123456789abcdef0123456789abcdef hold=1 window=@12 state=held
```

States are `armed`, `held`, `released`, `dropped`, `expired`, `invalidated`,
or `failed`. Arm returns `armed`, release `released`, and drop `dropped`.
If a deadline and invalid authority are observed together, expiry takes
precedence; both states close the descriptor and forbid release. These are
test scheduling results, not an attention audit trail.
Status accepts the latest hold identity, including its terminal state. It
does not forward or read data, but may discard an expired/invalid active hold.
Repeated release/drop of a terminal hold refuses. Wrong session or hold
identity refuses before altering that record, including deadline cleanup;
the independent lifecycle tick may still expire it. All unavailable grants,
stale identities and invalid transitions use `unavailable`; malformed syntax
uses `error`. The CLI validates exact framing, canonical identities, expected
session and hold (or receiver window for arm), and the command's allowed
success state before printing. Replies have a 1,024-byte ceiling and one
five-second post-connect read/write deadline. A lost response is not an
idempotency receipt: do not blindly repeat arm/release/drop. Controllers must
serialize their scenario; the one endpoint and hold belong to the session,
not an individual control connection.

## Planned control and observation increments

These are the next implementation requirements, not available commands:

1. Maintain the input boundary: do not mutate editor state or bypass
   compositor routing. Automation must never manufacture a physical-origin
   witness, enter/confirm trusted attention, or authorize secret release.
2. Preserve separate input and capture grants. A public Wayland socket grants
   neither capability.
3. Preserve the implemented session, input, client-publication and
   completed-output identities. Distinguish accepted input, an applied
   client transaction and
   completed output. A compositor sync cannot prove an application processed
   input. Queued output is not presented output.
4. Preserve capture correlation and the public-scene comparison and
   fail-closed completion proof as observation grows. No raw private-screen
   backing file is exposed.
5. Run td-editor's native key-profile, selection, menu, wheel and inter-client
   clipboard scenarios in disposable td-compositor processes. Combine real
   routed input and output evidence with editor remote exact-state assertions.
   Retain optional Weston tests to catch common client/server assumptions.

Required first-increment proofs run in ordinary `cargo test`: a built
compositor starts without hardware, a separate built native client maps and
completes its real configure/release/frame handshake, control commands change
its visibility, and owner EOF terminates the compositor and client connection.
Refusal, permissions, startup rollback, input-channel misuse and preservation
of unexpected client files are separate regressions. Test deadlines bound
failure; protocol observations, not sleeps, establish success.
