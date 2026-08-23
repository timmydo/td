# td-profiler — continuous system profiling

This file is the normative specification for `td-profiler`. Where the code,
recipes, image, and this document disagree, one of them is a bug.

## 1. Product contract

A running td deployment continuously produces bounded, local files that show
where every observed user-mode process spends CPU time. The files are useful
without a hosted service or an interactive profiler UI: an authorized human
or AI agent can read process summaries, function hot spots, folded stacks, and
the provenance of the executable and symbols used to resolve each address.

Sampling is statistical. A short-lived task may execute between samples, and
the absence of samples does not prove that a task used no CPU. `td-profiler`
therefore reports tasks observed through perf task records separately from
tasks which received samples. It never turns an unresolved or truncated stack
into an apparent complete stack.

The first implementation measures on-CPU user time. Scheduler records bound
off-CPU intervals and say whether a switch-out was preemptive, but without
wakeup events they cannot divide an interval into sleeping and runnable time
or identify a wait cause. Wall time, blocked time, allocation profiling, and
traced arguments are different measurements and are not inferred from CPU
samples.

## 2. Build-time observation contract

Frame pointers are a property of the shipped target platform, not an opt-in
debug profile. Every source-built user-mode object which can execute in the
shipped system uses them, including:

- the native compiler, libc, source-built libraries, and userland closure;
- the shipped stage2 compiler, in-tree standard library, and Cargo;
- target-built td control-plane programs and Rust userland;
- registry dependency crates, build-script C/C++ objects, and direct `rustc`
  recipes.

The policy covers the whole compiled closure because one omitted frame pointer
can truncate every caller above it. Optimization, link-time optimization, and
panic strategy remain independent choices. Bootstrap seeds, build-only
intermediates which cannot execute in the shipped system, the kernel and
firmware are separate contracts. Marked foreign application payloads cannot
acquire frame pointers after the fact; §7 defines their explicit fallback.
Implementation increment 1 makes this a checked current claim.

The compiler policy applies to compiler-generated code. Hand-written assembly
which changes the frame chain must either preserve the architecture's frame
pointer convention or appear in a reviewed exception roster. An exception is
a named coverage boundary, not permission to certify the containing object as
fully unwindable. Every split output records the conservative transitive union
of applicable roster entries at `lib/debug/.td-assembly-exception`; this
includes glibc startup and libgcc in downstream static executables, plus the
Rust runtime and package-specific assembly where applicable. Offline analysis
therefore does not need the recipe checkout to discover a boundary.

Rust compilation emits line-table debug information and ordinary symbols.
Every varying build path is remapped before it reaches either artifact. The
canonical roots are `/td-build` for package source, `/td-build-root` for other
build scratch, and `/td-cargo` for Cargo's working state; vendor source is
mapped below `/td-cargo/vendor`. Timestamps, archive ordering, and other build
identity inputs remain pinned by the normal recipe reproducibility contract.

A recipe links each user-mode ELF with a deterministic GNU build ID, using the
linker's SHA-1 build-id form. This is a pair check over linked bytes, not object
provenance or a statement of cryptographic trust. Every ELF gets a debug
companion: the recipe installs the runtime normally and places the companion
at the same relative path below `lib/debug` in that output. For example:

```text
/td/store/<output>/bin/td-svc
/td/store/<output>/lib/debug/bin/td-svc.debug
```

Shared objects use the same rule (`lib/X` and `lib/debug/lib/X.debug`). A
runtime ELF is stripped only after the companion has been created. The recipe
requires the same `.note.gnu.build-id` bytes in both files and refuses a
missing or duplicate note. Both files live in one content-addressed output, so
garbage collection, deployment selection, and rollback cannot separate code
from its symbols. Debug files are not separate downloads and require no
td-operated server.

Stripping removes the full ordinary symbol table and non-allocated debug
payload from the runtime after both have been copied to the companion. Dynamic
symbols and allocated sections remain available to the loader. An allocated
section is part of the runtime mapping even when its name begins `.debug_`;
rustc's `.debug_gdb_scripts` registration section is the known example and
remains in both files. Pair validation distinguishes it by `SHF_ALLOC` rather
than by a name allowlist.

Companions carry ordinary symbols plus line tables, not full variable and type
debugging information. The image recipe records their total bytes and enforces
a literal compiled ceiling kept outside the measuring code. The source-built
toolchain and shipped deployment have independent ceilings: four GiB for the
LLVM/rustc-dominated toolchain and one GiB for the image, which deliberately
excludes that build-only toolchain. Changing either is reviewed with the
corresponding size report. This keeps the always-available data useful for
function and source-line attribution without turning every deployment into a
full debugger SDK.

The deployment bundle records `deployment/debug-size` beside `root.erofs`.
It remains a derived build report rather than a boot payload: the exact
three-payload `td-deployment-v1` manifest stays readable by selectors already
installed before profiling, while the manifest's `root.erofs` digest binds the
debug companions whose size the report measures. The source-built Rust
toolchain records its separately dominated subtotal at `share/td/debug-size`
in that output. Both reports are stable `key=value` files and enforce their
scope's compiled ceiling before their containing outputs can be committed.

Debug information is expected to change output hashes. Reproducibility means
that the same declared inputs produce the same runtime and debug bytes; it
does not mean that enabling observability preserves an older store path. The
double-build check remains the oracle. Increment 1 copies the same `td-boot`
source below two different source and build roots, canonicalizes each with the
shared direct-rustc policy, and performs two independent links and companion
transforms. It compares both the installed runtime and companion byte for byte;
the normal closure checks remain the wider reproducibility backstop.

## 3. Collection boundary

One system service opens Linux perf events system-wide, once per online CPU,
from the deployment's outer PID namespace. It requests software CPU-clock
samples and `PERF_RECORD_FORK`, `EXIT`, `COMM`, `MMAP2`, and context-switch
records. `sample_id_all`, `use_clockid`, and `CLOCK_MONOTONIC` are pinned so
every non-sample record carries PID, TID, time, and CPU on the same clock as
startup fences and capture manifests. Samples exclude kernel and hypervisor
execution and include those fields, instruction pointer, and a user call
chain.

The metadata and sampling event for one CPU redirect into one ring with
`PERF_EVENT_IOC_SET_OUTPUT`, so kernel ring order breaks same-CPU timestamp
ties. Rings merge by monotonic time, then CPU number and per-ring sequence.
Equal-time records on different CPUs which would impose conflicting state on
one task mark that task interval ambiguous; CPU-number order is serialization,
not invented causality.

Collection from outside application jails is deliberate. It requires no jail
API or relaxation: an application neither opens the events nor reads another
process's profile through td-profiler. The collector uses `perf_event_open`,
read-only startup `/proc` metadata, and memory-mapped perf ring buffers. It
does not use ptrace, BPF, a kernel module, shell, an external library, or a
socket.

Both perf events pin the same non-sample trailer fields:
`PERF_SAMPLE_IDENTIFIER`, `TID`, `TIME`, and `CPU`. The sample event additionally
requests `IP` and `CALLCHAIN`, which affect only `PERF_RECORD_SAMPLE`. The
identifier and fixed trailer subset make records from the shared ring
unambiguous without guessing which event emitted them.

The first version pins `CONFIG_HOTPLUG_CPU=n`. Runtime CPU topology is therefore
immutable: supporting hotplug later requires a design amendment and a new
loss-aware event source, not polling the current mask. The daemon recovers or
reclaims old capture directories before collection, then starts the privileged
observation sequence. It builds the immutable object inventory in §4, reads the
online CPU mask, and opens disabled metadata and CPU-clock events for exactly
that set. It redirects each sample event into its CPU's metadata ring, enables
metadata, then synthesizes the already-running task and mapping state from
`/proc`.

Each synthetic task snapshot has a before-read monotonic fence and the task's
kernel start time. Real metadata at or after that fence is applied over the
snapshot in the merge order above. Repeated mapping records are idempotent; an
exec record clears the generation and its following mapping records rebuild it.
Before enabling sampling, the daemon drains every perf ring through an end
fence and rereads the online mask. A topology disagreement or any startup ring
loss invalidates the entire baseline, closes every event, and retries the whole
sequence without publishing a sample. Three failed attempts exit nonzero so
`td-svc` applies its ordinary failure backoff. A vanished task, changed start
time, inconsistent map read, or exec ambiguity invalidates only that task's
baseline and is recorded rather than guessed.

Later mapping records are attributed by MMAP2 device and inode against the
startup inventory; no untrusted path or build ID selects an object. Because
post-startup attribution does not need cross-uid `/proc/PID/maps`, the daemon
permanently drops supplementary groups, gid, and uid to the dedicated
`profiler` account before enabling and consuming samples. After the drop it
reads indexed runtime/debug store objects and writes only beneath its capture
root. Event file descriptors are never accepted from clients, and there is no
control socket in the first version. The `UNSAFE.md` roster and confinement
tests pin every syscall and path class used for event setup, rings, files, and
the credential drop.

A failed CPU event, malformed ring record, lost-record counter, permission
failure, or symbol mismatch is durable output, not a reason to invent data or
silently omit a CPU.

The image pins `kernel.perf_event_paranoid` to at least 1, so the unprivileged
process cannot open another system-wide event. The service uses
`restart=on-failure`; it has no planned exit or reconfiguration path. An online
mask change despite the kernel pin is a conformance failure: the daemon closes
the capture with each CPU's last known coverage end and exits nonzero. Sampling
defaults to 99 Hz per online CPU so periodic work is less likely to phase-lock
with the profiler. The rate and fixed CPU mask are recorded in every capture
manifest.

## 4. Address ownership and symbols

Every sample address is first normalized to an object-relative address using
the mapping records captured from perf. Mapping state is time-indexed by task
start time and exec generation: a later `exec`, replacement mapping, PID reuse,
or library relocation must not rewrite the meaning of an earlier sample.

The image contains a deterministic deployment object index listing exact
runtime and debug store paths plus the expected GNU build ID and provenance.
During privileged startup the daemon stats those exact paths through the outer
mount and builds a same-boot device/inode inventory. MMAP2 records request
device, inode, and inode generation rather than their build-ID union. Only a
device/inode match against this inventory can attribute a store object; an
available inode generation is also required to agree. The immutable EROFS
deployment does not reuse an inode during that boot. The image and daemon both
assert that the indexed store is on the deployment's read-only EROFS mount;
collection refuses store attribution if that check fails. The path string
reported by a task is display-only: it is never resolved against the outer
root, because `/app/X` or `/usr/X` can name different bytes inside an
application mount namespace.

After identity selects an exact runtime object, its build ID is checked against
the index and companion before symbols are used. A build ID never selects or
authenticates an object. Function names come from the full symbol table; source
file and line come from the remapped line table when supported. The report
retains the runtime store path, object-relative address, device/inode identity,
and build ID even when a friendly name was found, so resolution can be audited
and improved without collecting the workload again.

The symbolizer never opens a path through a sampled process's mutable root as
trusted metadata. Store paths are immutable. The first version never copies a
non-store mapping into a capture; it labels the captured display path and
identity and leaves the object unresolved. Anonymous and JIT mappings are
identified as such rather than assigned a neighboring symbol.

## 5. Capture files

Captures live under `/var/lib/td-profiler/captures`. The service writes a new
directory with a `.partial` suffix, and readers ignore `.partial` and
`.quarantine` directories.
Publication writes and fsyncs every file including the manifest, fsyncs the
partial directory, atomically renames it, and then fsyncs the parent `captures`
directory. A completed capture contains:

```text
manifest.json       schema, deployment, interval, rate, CPUs, loss, errors
processes.jsonl     one process/image generation and its accounted samples
hotspots.jsonl      sorted process/object/function aggregates
stacks.jsonl        sorted stack aggregates with state, reason, and count
stacks.folded       deterministic folded stacks with integer sample counts
samples.bin         versioned raw records needed for re-symbolization
```

`stacks.jsonl` is the structured source for per-stack state. The folded form
prepends one reserved synthetic frame such as
`[td:truncated:foreign-no-frame-pointer]`; escaping rules prevent a program
symbol from entering the `[td:*]` namespace. Lost records which produced no
stack appear in manifest counters rather than as fabricated folded samples.

JSON uses UTF-8, integer nanoseconds and counts, fixed field order, and one
object per line where applicable. Summaries sort by descending sample count,
then stable identity fields. A path or name has a human-readable string field
only when its bytes are valid UTF-8 and always has a lowercase hexadecimal
`*_bytes` field; consumers use the latter as identity. Invalid bytes are never
placed in a JSON string or replaced. The binary stream has an explicit magic,
endian marker, schema version, record lengths, and reserved fields that must be
zero. Unknown record kinds are skipped by length and reported.

`manifest.json` records at least:

- schema version and profiler build store path;
- deployment identity and boot ID;
- monotonic start/end and wall-clock display time;
- configured and effective sample rates and the CPUs covered;
- sample, task, mapping, context-switch, lost, corrupt, and unresolved counts;
- every collection or symbolization error with its CPU and time range;
- runtime/debug build identities and store paths used by the report.

The raw records are the durable evidence. Summary generation is deterministic
for an identical raw stream and symbol closure, but captures of a live system
are observations and are not reproducible recipe outputs. No profile is ever
an undeclared build input. An optimization based on a capture is reviewed as a
source or recipe change; profile-guided optimization would need a separately
declared, pinned input policy before it could enter the artifact graph.

## 6. Retention and confidentiality

Capture contents expose workload shape, executable names, and code addresses.
The capture root and each capture directory are owned by
`profiler:profiler-read`, mode `02750`; files are `0640`. Setgid inheritance
keeps every file in `profiler-read` even after the daemon drops all
supplementary groups. No interactive account joins that group by default. A
local human or AI analysis process receives read-only membership through the
system's explicit account configuration, not through this daemon. Captures are
not placed in service logs or sent over the network; td-profiler does not
upload, serve, or redact them.

The daemon rotates fixed-duration captures and enforces both a byte ceiling
and a count ceiling across completed, partial, and quarantine directories.
Before starting a capture it reserves that capture's compiled maximum size,
pruning oldest completed captures but never the sole completed one. Rotation
first disables each CPU's sample event, records its monotonic coverage end,
snapshots every ring head after disable, and drains through those heads before
publishing. The next capture carries mapping state forward and records its own
per-CPU coverage start, so the manifest never implies coverage during the
bounded rotation pause.

The raw stream has its own byte budget below the reservation, and report files
have separately reserved worst-case budgets derived from bounded record counts
and field lengths. Reaching the raw budget rotates immediately instead of
waiting for the duration; no write may exceed its reserved portion. If a
single record cannot fit, the daemon records an oversize/lost count, closes the
capture, and starts a new one rather than overrunning `/var`.

The atomic partial-directory name encodes validated boot ID, PID, kernel start
time, and sequence before any contents are created. At startup the daemon
deletes a partial only when those fields prove that the recorded boot differs,
or that PID/start-time no longer names a live process. A live owner makes a
second daemon refuse to start. A malformed name is renamed with a
`.quarantine` suffix; readers ignore both suffixes. Partial and quarantine
directories count toward the byte ceiling. A quarantined tree is reclaimed by
descriptor-relative traversal which refuses mount crossings and unlinks
symlinks rather than following them, then fsyncs the capture root. Failure to
validate or reclaim it stops new collection before the filesystem is filled
and emits its exact path in a console/service diagnostic. Exact defaults belong
beside the implementation and are pinned by tests. `/var` is persistent, so
completed captures survive reboot and deployment rollback; their manifests
identify the deployment that produced them.

## 7. Coverage boundaries

Frame-pointer call chains are the fast, always-on path for source-built td
user mode. Coverage degrades explicitly:

- a marked foreign payload or executable bootstrap seed may lack frame pointers
  or debug information;
- reviewed hand-written assembly may not preserve a frame chain;
- an anonymous/JIT mapping may have no stable object;
- a deleted or replaced non-store mapping may be unavailable at report time;
- kernel permission or ring loss may remove records.

The leaf instruction pointer is still attributed when possible. Each stack
records whether it is complete, truncated, unresolved, or lost, with a reason.
DWARF stack unwinding is not performed in the sampling path. Adding it later
must remain bounded, run outside the sampled process, and preserve the raw
addresses so a bad unwind cannot destroy evidence.

The phrase "every process" therefore means every user task observable by the
outer system-wide perf boundary, with coverage and loss stated. It does not
mean every instruction is traced or that foreign bytes acquire metadata td
did not build.

## 8. Landing sequence and acceptance

The implementation lands in three independently green increments after this
design:

1. Apply the target-wide frame-pointer and deterministic debug-companion policy
   to every shipped source-built user-mode build path. Tests pin flags, path
   remaps, GNU build IDs, companion layout and pair verification, assembly
   exceptions, and double-build identity. A literal debug-size ceiling is
   reviewed outside the measuring code. The shipped image and the separately
   retained source-built toolchain each report against their own ceiling, so
   rustc and LLVM growth cannot silently relax the deployment budget.
2. Add the dependency-free `td-profiler` collector and offline report command.
   Amend `UNSAFE.md`, confine the syscall roster, and register the standalone
   lock and affected checks. Unit tests cover CPU-set parsing, perf record
   decoding, startup fences, cross-CPU ordering, mapping generations, loss
   propagation, raw format compatibility, symbol aggregation, and hostile
   lengths without requiring host perf permission.
3. Add the recipe, kernel requirements, dedicated account, service, persistent
   capture directory, rotation policy, deployment object index, and boot/image
   assertions. Account-roster tests pin `profiler`, the members-capable
   `profiler-read` group, setgid directory modes, and the
   `perf_event_paranoid` value. They also pin `CONFIG_HOTPLUG_CPU=n`, prove the
   running online mask is fixed, prove the indexed store is the read-only EROFS
   deployment mount, and prove indexed companions remain readable after the
   credential drop. Tests distinguish whole-baseline startup loss from a
   per-task snapshot failure and pin the three-attempt nonzero failure path.
   The system test proves the daemon produces a complete manifest and
   AI-readable summaries for a known CPU workload, or a precise
   unsupported-permission diagnostic on a host where collection cannot be
   exercised.

A release is conforming only when the built kernel exposes the required perf
event ABI, the collector remains outside jails, every shipped source-built
user-mode ELF has a verified companion, capture loss is visible, and the image
contains no profiler socket, uploader, socket syscall, inherited socket
descriptor, or automatic export path. An explicitly authorized reader remains
responsible for what it does with the files it can read.
