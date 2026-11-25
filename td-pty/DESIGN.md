# td-pty

td-pty is td's PTY: the `/dev/ptmx` device, a child started on its slave,
the threads that carry the child's output, keyboard input and exit, and a
foreground relay over this process's own terminal. It depends on nothing
but `std`. td-term runs its shell through it, td-sandbox its relay and
workspace entry, and td-init's secret-line and boot-menu tests and td-sh's
job-control test open real terminals with it, so none of them stages the
UI toolkit for a terminal device.

`td-term/DESIGN.md` is the behavioural contract for the terminal these
pieces serve: its bounds (§2: the keyboard-queue and PTY-output ceilings)
and its threads' lifecycle (§4: a reader or writer is never joined on a
teardown path). The raw surface is `UNSAFE.md` §29's.

## Interface

- `Pty` (`open` unlocks `/dev/ptmx` opened without acquiring it, `master`,
  `into_master`, `peer` for the slave by descriptor, `window`, and
  `resize`, which publishes a grid and reads it back before trusting it),
  `WindowSize` and `window_size`, `grid_size` and `grid_for_tile`,
  `ChildCommand` and `spawn` (a caller-composed command on the slave, the
  environment cleared and set to the caller's list of OS-string pairs, a
  program named without a slash found on that list's `PATH` by std's rule,
  leading a new session whose controlling terminal is the slave exactly
  when `leads_session` is set), the threads `spawn_reader`, `spawn_writer`
  and `spawn_waiter` (named `pty-output`, `pty-input` and `pty-child`,
  each reporting its own ending on the caller's channel as `Output` or
  `Waited` and running the caller's `notify` after each send), `Input`,
  the bounded queue the loop fills and the writer drains (`new`, `push`,
  `close`), over `InputQueue`, which admits a sequence whole or drops it
  whole and marks the drop for the caller's bell, `write_input`, the
  `DEV_PTMX`, `READ_CHUNK`, `MAX_OUTPUT_BYTES`, `MAX_OUTPUT_CHUNKS` and
  `MAX_INPUT_BYTES` constants, and `selftest`. `Input::take_for_test` is
  test support, public because a consumer's tests are another crate and
  hidden from the crate's documentation. Which account, environment and
  command a terminal runs is its caller's policy, never this crate's.

## Invariants

- `unsafe` is confined to the private `sys` module under `UNSAFE.md` §29:
  three function-scoped allowances, one syscall instruction, one adoption
  site for the slave and one pre-exec hook; the crate root denies it and
  is the module's only caller. The queue module has no raw boundary.
- The master is opened with `O_NOCTTY`; the slave is taken from it by
  descriptor, never by `/dev/pts` name; every published size is read back
  before it is trusted; a child's environment is exactly its caller's
  list, and it leads a session on the slave exactly when it asks.
- A keyboard sequence is admitted whole or dropped whole, and the
  writer never holds the queue's lock across a write.
- Production code has no `unwrap`, `expect`, panics or panicking indexing.
  Its allocations are recorded debt (`td-alloc/DESIGN.md`); migrating
  them to td-alloc adds a dependency, which amends this list, the
  confinement test and `UNSAFE.md` §29 together.
- The manifest and lock name no dependency; `tests/confinement.rs` pins
  that, the source inventory, the raw layer's fingerprint and every call
  site.

## Foreground terminal relay

`RelayTerminal`, used by td-sandbox, owns a borrowed-terminal duplicate,
the exact original Linux termios bytes and the calling thread's signal mask.
It enters raw byte mode with VMIN=0/VTIME=1, reads at most one timeout per
poll, and restores both on return/drop. The caller spawns children before
entering the guard, so they do not inherit its blocked termination signals;
subsequent reader threads live only as long as that frontend process.
HUP/INT/QUIT/TERM/TSTP are blocked and synchronously consumed, never handled
by a Rust signal handler. Suspension requests terminate the relay; SIGKILL
cannot restore attributes. The safe API exposes enter/read/window/signal and
restore, not arbitrary ioctl requests or signal sets. UNSAFE.md section 29
records its requests and syscalls; the complete raw-source fingerprint,
call inventory, and a PTY restoration test pin the implementation.

The relay terminal guard cannot cross threads: its signal mask belongs to
the entering thread. Restoration attempts both termios and the signal mask,
even if the terminal has disappeared.
