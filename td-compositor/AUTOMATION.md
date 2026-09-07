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
TD-COMPOSITOR-HEADLESS-READY version=1 width=800 height=600 scale=1
```

It follows the initial successful output paint, both listener binds, keymap
preparation, and successful listener/lifetime worker creation. It promises
server startup, not that any application has mapped or completed an action.
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
key <time-ms> <1-247> <down|up>
release-keys <time-ms>
pointer <time-ms> <x> <y> <buttons> <vertical> <horizontal>
release-input <time-ms>
```

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

`ok` means input was routed or an idempotent no-op, not that the focused
application processed it or presented another frame. Delivery failure can
follow mutation; requests are not rollback transactions, and an unavailable
or lost reply must not be blindly retried as exactly-once input. Use
`release-input` to recover depressed state or dispose of the session. The
existing request-size and whole-conversation deadline bounds remain in force.
Pixel capture and application/output observation fences remain separate
increments.

## Planned control and observation increments

These are the next implementation requirements, not available commands:

1. Maintain the input boundary: do not mutate editor state or bypass
   compositor routing. Automation must never manufacture a physical-origin
   witness, enter/confirm trusted attention, or authorize secret release.
2. Add a separately enabled capture capability on `td-ctl`, absent by default.
   A public Wayland socket grants neither synthetic input nor capture.
3. Report monotonic session/action/commit/output identities with bounded
   observation. Distinguish accepted input, a client's subsequent commit and
   completed output. A compositor sync cannot prove an application processed
   input. Queued output is not presented output.
4. Add bounded public-scene captures through the control grant. Capture must
   exclude the private attention screen (`Scene::render_display` is not a
   capture API). Correlate captured pixels with completed output; fail closed
   where that cannot be proved. No raw private-screen backing file is exposed.
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
