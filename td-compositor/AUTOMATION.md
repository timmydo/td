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

## Opt-in keyboard control

Append `--input-control enabled` to the headless command to grant its private
`td-control` endpoint one synthetic keyboard. Ordinary `run` sessions and
headless sessions without that exact option refuse keyboard requests. Public
Wayland clients gain no control endpoint or input grant. No hardware device
is acquired, and a trusted-attention-enabled runtime refuses synthetic input
even if incorrectly wired to this adapter.

The `td-ctl` request vocabulary adds:

```text
key <time-ms> <1-247> <down|up>
release-keys <time-ms>
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

The keyboard belongs to the headless process generation, not an individual
one-request control connection. Held keys persist across those connections.
`release-keys` releases all its depressed keys using normal device-removal
cleanup, including consumed shortcuts, but retains Caps/Num lock toggles and
overlay visibility. Repeating it is harmless. The owner closes stdin to end
the whole generation, which disconnects clients and discards all held state;
there is no detached input controller that outlives its session. Callers
sharing the endpoint share this keyboard and must serialize their scenarios.

`ok` means input was routed or an idempotent no-op, not that the focused
application processed it or presented another frame. Delivery failure can
follow mutation; requests are not rollback transactions, and an unavailable
or lost reply must not be blindly retried as exactly-once input. Use
`release-keys` to recover depressed state or dispose of the session. The
existing request-size and whole-conversation deadline bounds remain in force.
Pointer injection, pixel capture and application/output observation fences
are still separate increments.

## Planned control and observation increments

These are the next implementation requirements, not available commands:

1. Extend the opt-in keyboard with complete pointer/button/wheel reports
   through normal shared seat and compositor-binding paths. Do not mutate
   editor state or bypass compositor routing. Release automation-owned held
   state when its controlling generation ends. Automation must never
   manufacture a physical-origin witness, enter/confirm trusted attention,
   or authorize secret release.
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
