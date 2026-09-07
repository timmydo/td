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
The status text stays empty; workspace chrome remains. No synthetic input is
enabled in this increment. Existing layout commands work on `td-control`.
Normal `run` startup, deployment authority and physical secure attention are
unchanged. All production code remains std-only with no new syscall surface.

## Planned control and observation increments

These are the next implementation requirements, not available commands:

1. Add separately enabled synthetic input and capture capabilities to the
   existing `td-ctl` channel, absent by default. A public Wayland socket does
   not grant either. Automation must never manufacture a physical-origin
   witness, enter/confirm trusted attention, or authorize secret release.
2. Route typed keys and complete pointer/button/wheel reports through the
   normal shared logical-seat and compositor-binding paths. Do not mutate
   editor state or call a shortcut that bypasses compositor routing. Release
   automation-owned held state when its controlling generation ends.
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
