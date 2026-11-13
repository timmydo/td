# td-sandbox

## Contract

`td-sandbox` manages named development sandboxes for the primary human.
Each sandbox runs as a workload account of its own, `tdwUID` in the
ledger's sandbox and workspace range (98304–131071), with a private home
at `/var/lib/td/workloads/UID` (WORKLOADS.md step 7; td-authd/DESIGN.md,
"Workload account ledger"). td-sandbox itself creates no account, gains no
privilege and starts no jail: every change to a sandbox's account, grants
or network, and every shell or command in one, is a request to root's
workspace launch intake at `/run/td-authd/1000/workspace`
(td-authd/DESIGN.md, "Workspace launch intake"), which admits the primary
human's peers alone. td-jail's workspace kind owns namespace, mount,
identity, capability, seccomp and descendant confinement; APPLICATIONS.md
section C is normative for that boundary. The frontend forbids unsafe code
and depends only on td-fs and td-ui. Its executable is the workspace's
immutable entry; the target recipe stages engine's Flathub declarations
(`flathub_app.rs`) and the ledger's names (`workload_accounts.rs`) too.

Sandboxes run only on td, where the intake runs: on any other host every
command but `help`, `runtime` and `stop`, which touch only the human's own
records, fails naming the missing intake. `./install-apps` no longer
installs td-sandbox and has no way to retire a program: a
`~/.local/bin/td-sandbox` an earlier `./install-apps` left on a development
host is stale, no longer updated, and should be removed by hand.

```
td-sandbox create tools
td-sandbox share tools ~/src/project --write
td-sandbox network tools host
td-sandbox runtime tools freedesktop-platform-25-08
td-sandbox shell tools
td-sandbox run tools -- command argument
td-sandbox stop tools
td-sandbox adopt tools
td-sandbox delete tools
```

## Who keeps what

Root keeps each sandbox's account, grants and network: the ledger row
`sandbox NAME UID` (`/var/lib/td/accounts.tsv`), the grant table's rows
for it (`/var/lib/td/grants.tsv`), `directory ro|rw PATH` relative to the
human's home and `network host`, no network row being off, and the
account's home. td-sandbox keeps none of them: `inspect`, `list` and
`adopt` ask root.

The human keeps one record per sandbox under
`~/.local/share/td-sandbox/.profiles/NAME`, private: `record`, `lock` and,
while a session runs, `control`. The record is exact: the line
`td-sandbox-profile-v1`, an optional `runtime=RUNTIME`, then an optional
`old-home=PATH` naming the home of the profile it was imported from. A
name is the ledger's: 1–64 lowercase ASCII letters, digits, `.`, `_` or
`-`, beginning with a letter. One session or change per name holds the
record's kernel file lock.

A sandbox is **held** when its account's home holds files or the grant
table holds a row of its name; root answers `status NAME` with `absent`
(no ledger row), `free UID` or `held UID`. Root never creates over a held
sandbox, so a lost record never costs a sandbox its files or grants.

- `create NAME` writes the record, then asks root to `create`. Root
  refuses `exists` for a held sandbox, and otherwise allocates the
  account through td-firstboot's `allocate-workload`, the one path to a
  sandbox's ledger row, or returns the one the ledger holds: a free
  sandbox, a deleted one among them, is created again with nothing lost.
  A refusal removes the record; over a held sandbox td-sandbox says to
  `adopt` or `delete` it. A record already here is refused, naming
  `inspect` and `delete`. It prints `Created NAME as tdwUID; network off`.
- `adopt NAME` writes a record for a held sandbox that has none: one whose
  record was lost, removed or restored from before it was made. It is
  refused for a free or absent one, which `create` makes.
- `share NAME PATH --read|--write` resolves PATH and asks root to grant it
  `ro` or `rw` by its path relative to the canonical home; a directory
  that is not below the home cannot be shared. Root's writer refuses what
  it refuses for any grant (WORKLOADS.md, "Grants"), by its reason.
  `unshare NAME PATH` revokes, taking a path that is gone, or the
  `~/PATH` form `inspect` shows.
- `network NAME off|host` sets the network row.
- `runtime NAME RUNTIME|native` changes the record alone.
- `delete NAME` asks root to `delete`, then removes the record. Root
  refuses while a shell or command of the sandbox runs or its grants are
  being read or changed; the client's lock refuses while its own session
  does. A record that cannot be read, a crash's half-written one, is
  deleted all the same, and a held sandbox without a record is deleted at
  root.
- `list` prints each record's name, account and network; `inspect` the
  account, its home, network, runtime, each grant as `Read: ~/PATH` or
  `Write: ~/PATH`, and an imported sandbox's old home.

Grants change what the next launch publishes; a running session keeps its
views until it ends. Root's grant verbs, `status`, launches, creation and
deletion of one sandbox are ordered in memory: a creation or deletion runs
alone on it. Request `2c` through the compositor runs in another root
process and is not ordered with them, so a grant it makes while a
deletion runs can leave a row on the deleted name; the sandbox is then
held, and creating it again is refused `exists` rather than inheriting
the row, until it is deleted again.
## Deletion

Root deletes a sandbox in one order: it clears every grant row the name
holds, directories, sockets and network, through the grant table's one
writer (`td-firstboot grant-table 1000 clear sandbox NAME`); then, when
the ledger holds the name, it prepares the account and has the account
empty its own home (`td-login exec-service-as tdwUID -- td-authd
workspace-empty UID`), so root walks no tree a workload wrote. The
emptying opens each directory without following a link, restores its
owner's mode where the sandbox narrowed it, removes links without
following them, refuses past 128 levels, and walks again, at most four
times, until the home lists empty. Root then marks the emptied home
deleted with a file of its own, `.td-deleted`, made new and never through
a link; a marked sandbox takes no launch and no grant change, refused
`deleted`, until it is created again, which takes the mark. The ledger
row stays: rows are permanent, and a sandbox created again under the
name gets the same account, empty. Views a deleted sandbox's earlier
launch published are released at its account's next launch or at
shutdown.
## Import

Before sandbox accounts each profile lived at
`~/.local/share/td-sandbox/NAME`, its `home` mounted as the sandbox's
home, run as the human, with a `policy` of network, runtime and absolute
directory grants. `init-default` imports those profiles once: for each
whose name the ledger can hold, it writes the record, with the runtime
and `old-home`, then asks root to create the sandbox, to grant each share
below the human's home at the same access, and to give host network
where the policy did. The old home is left as it was, never moved or
mounted: a sandbox starts with an empty home of its account's, and
`inspect` names where the old one lies. A sandbox sees its shares at
their own paths below the human's home, not inside its home; the
`~/sandbox` link the old default's home held is gone, and the report
says so.

An old profile whose name a sandbox already has is never imported into:
one root holds is adopted as it is, its record naming no old home, and
one with a record here that names another old home, or none, is left. A
profile whose name begins with a digit or a hyphen, whose policy cannot
be read, or whose account root refuses finally, is dropped; so is a share
outside the human's home, or one root refuses finally. Root marks a
refusal final when asking again cannot change it (td-authd/DESIGN.md,
"Creation, deletion and grants"). Any other refusal, a busy table's, a
claim's, a failed writer's or a failed allocation's, stops the import
before its marker, as no answer does, the profile's record kept, so the
next `init-default` asks again; root cannot tell a full ledger from a
failed allocation, so a creation is never dropped this way, and its
refusal is printed at every run until it passes or the old profile is
moved out of `~/.local/share/td-sandbox`. Each drop is printed and written
to `~/.local/share/td-sandbox/import-report.txt` before the import goes
on, each line once, the report at most 1 MiB and ending, when full, in a
line that says so. The import looks at 4096 entries and imports at most
1024 profiles in name order; it reports each profile past that, and that
entries were left unread, so none is dropped silently. The marker
`.imported` beside the report ends the import; until it exists, every
`init-default` imports again: a profile whose record names its own old
home was the import's, and its creation and grants are asked again,
which root's create of a sandbox the import already made, refused
`exists`, and its idempotent grants make converge. The import reads at
most 64 KiB of each policy.
## Runtime

On td a sandbox may name a declared Flathub runtime
(`flathub_app::FLATHUB_RUNTIMES`: `freedesktop-platform-25-08`,
`freedesktop-sdk-25-08`) with `td-sandbox runtime NAME RUNTIME`, and drop
it with `td-sandbox runtime NAME native`; `inspect` shows it. The launch
request names it and root's spec carries it after `profile`
(APPLICATIONS.md §C). Setting one requires a declared name. Reading a
record checks the name's syntax alone, so a sandbox whose runtime a later
system no longer declares still inspects, clears to `native` and deletes.
td-jail is the authority at launch: it refuses an undeclared name, and
root refuses a runtime that is not installed before it answers.

The jail binds the runtime store's `current` tree read-only at `/usr`,
with `/lib` and `/lib64` links into it, and takes the runtime's
`ld.so.cache`, `ld.so.conf` and `ld.so.conf.d` into `/etc` in place of the
host's. td's `/bin` stays, first on `PATH` before `/usr/bin`, and no
`LD_LIBRARY_PATH` is set, so the runtime's loader cache serves the
runtime's programs. td's own programs are static or linked against td's
imported Debian glibc, and nothing requires one to carry a `RUNPATH`
covering every library it needs (`builder/src/build_root.rs`'s
`check_output` only refuses an interpreter or `RUNPATH` outside the
store, and only for build-root outputs). That glibc keeps Debian's
compiled-in `/usr` paths, so here a td process's library search after its
`RUNPATH` (the runtime's cache, then default directories under `/lib`
and `/usr`, which serves what it loads or `dlopen`s by name that its
`RUNPATH` does not hold), its gconv modules and its locale data resolve
into the runtime: a td program in such a sandbox can load Flathub code
built against another glibc; glibc's own `dlopen` of `libgcc_s.so.1`
is among those, unless the process already has it loaded. The
runtime's code runs under the workspace filter (the development profile
when network is on), as the sandbox's account, with the sandbox's grants
and network and the host-local sockets that network reaches, and outside
the application kind's D-Bus, portal and Wayland boundaries
(APPLICATIONS.md §C). The launch holds that generation until it ends
(WORKLOADS.md, "Runtimes").

A sandbox installs no runtime. Its runtime is installed and kept only
by an application that names it, today Firefox for
`freedesktop-platform-25-08`. With no `current` in the runtime's store the
launch is refused, naming the applications whose installation in
td-software installs it, or saying that no application installs it yet,
as for `freedesktop-sdk-25-08` until a runtime install path exists.
Removing the last installed application that names a runtime removes the
runtime: a running session keeps the generation it holds, and the next
launch is refused.

## Authority

A sandbox's processes run as its account inside and outside the jail.
They reach the human's files only through the views root publishes from
the grant table, each bound at the human's own path, a view's files
keeping the human as their owner on disk; executable views are the
account kind's choice, never a row's or a request's (td-authd/DESIGN.md,
"Grant views"). Its home is the account's alone. Writable grants entrust
their entire contents to the sandbox, including executable files and
repository hooks. System trees are read-only. User-local installations
and replacement binaries persist in the account's home across sessions;
`~/.local/bin` comes first on `PATH`. The frontend does not download,
validate, install or update third-party payloads. Those runtime files
never become target recipe inputs or distribution seeds. The one managed
foreign runtime is a declared Flathub runtime, admitted under
APPLICATIONS.md §W.9 and bound as above.

Network defaults to off: root's spec names `development-offline`, the
development syscall policy and executable home in a private network
namespace. `host` names `development`, sharing the host namespace,
including localhost, LAN and abstract Unix sockets. Off is not a
hostile-kernel attack-surface minimization profile. Resource caps are
inherited; td-jail prints its existing aggregate-memory/task/CPU-cap
limitation.

What the account does not bound is who may drive it: any program running
as the human can reach the intake and so start a shell in any sandbox the
ledger holds, with that sandbox's grants and network, and change those
grants within what the writer admits (td-login/THREAT-MODEL.md §1). The
account keeps a sandbox from the human's ungranted files, the human's
other sandboxes and applications, and the human's processes; it does not
keep the human's other programs from the sandbox.

## Image default profile

On installed and live desktop boots, the `sandbox-default` oneshot runs
`td-sandbox init-default` through `td-login exec-primary`, after firstboot
and root's workspace intake, waiting at most ten seconds for the intake's
socket. It runs the import above, then, without a readable `default`
record (an unreadable one is removed), creates `default` through root
with network off and one writable grant, the primary account's
`~/sandbox`, created 0700 and required to be a private directory of the
home reached as itself, never through a link; the record is written last,
so a boot cut short between the creation and the grant is completed by
the next, whose creation of the still free sandbox returns it. A
`default` root already holds, its record lost, is adopted as it is, its
grants its own: nothing is granted again. This is the only automatic
share; the account name is resolved at boot, never fixed to the image
seed. An existing record is left unchanged, with its networking and
removed grants. On the first boot after sandbox accounts the import can
take more than the unit's bound; a run it cuts short is completed on the
next boot.

The image compositor offers `td-term (sandbox)`. Its fixed authority request
starts a human-owned td-term with literal command `/bin/td-sandbox shell
default`, using the normal terminal readiness and process tracking. The
terminal remains outside the jail; its shell is inside. The launcher never
initializes or rewrites a record. Boot orders the compositor after default
provisioning but does not require provisioning success, so a damaged record
cannot prevent the desktop opening. A deleted default is seeded on the next
boot. Live media state lasts only as long as that live home does.

## Process and terminal ownership

`shell` and `run` send one launch request, `start NAME RUNTIME TERM` or
`run NAME RUNTIME pty|pipes TERM WORD...`, each word `x` and its
argument's bytes in lowercase hex. `shell` runs td's `/bin/sh -i`, which
root names; no shell the caller names is offered. Root answers `ok`, or
`refused` and why. After `ok` root starts td-jail's workspace kind as the
sandbox's account with the accepted connection on its stdin and stdout,
and the same connection is the relay below; td-sandbox keeps no other
descriptor of the jail's. td-jail reaps the whole PID namespace when the
immutable entry ends. The entry sends an empty first frame once its
command runs. A connection that ends before that frame is a launch that
never started, and one that ends before its exit frame a session that
ended unreported: td-sandbox says so and names
`/var/log/svc/workspace-intake.log`, where root reports both. td-sandbox
shares its intake socket transport with td-agent
(`engine/src/workspace_intake.rs`), with bounded dialing and writes, and
waits at most two minutes for a launch's answer and ten for any other,
a deletion's emptying among them. Root refuses a launch of a deleted
sandbox `deleted`, which td-sandbox tells as such.

The entry opens a fresh PTY in the private devpts via td-ui, then spawns
the command as a new session leader on its slave. The workspace's generic
/dev/tty (5,0) refers to that session's terminal, not the host's. PID 1
and the relay entry have no controlling terminal.

The duplex relay has a one-byte kind and little-endian u32 length, capped at
8192 bytes, with bounded 128-frame queues per reader/writer. Inbound kinds are input, resize
(exactly four bytes, positive u16 rows/columns) and EOF (empty). Outbound kinds
are stdout, stderr and exit (exactly one byte). Framing and payload shape are
validated; no message chooses a host operation beyond terminal output. PTY
output merges stderr; pipe mode preserves it and forwards stdin EOF. Exit
codes are retained, signals represented as 128+signal. Blocking input and host-output writes run on dedicated workers, outside
lifecycle polling. After command exit, output readers receive a 200ms drain
window each; descendants retaining stdin or idle output descriptors cannot
delay teardown. Delivering queued output and the exit frame can still wait
for the host output consumer, but stop remains responsive even then.

The outer foreground terminal guard uses td-ui's audited termios and signal
operations. It disables local canonical/echo/signal processing, reads with a
100ms timeout and polls sizes; bytes such as Control-C go to the inner PTY's
line discipline and foreground process group. HUP/INT/QUIT/TERM/TSTP are
blocked once the relay begins and consumed synchronously. They end the
session after restoring the original terminal, rather than suspending a raw
relay. SIGKILL cannot restore a terminal. Original attributes and signal mask
are restored on ordinary return and errors. Reader threads end with the
client process and are never joined on a blocking read.

`stop` connects to the session's private `control` socket beside its
record. Its filesystem address is opened through a retained directory
descriptor's procfs alias to avoid sockaddr_un's pathname limit. The
listener carries no general command protocol: an accepted connection ends
this session's relay, which closes the connection; the entry inside sees
its channel end and returns, and td-jail tears the namespace down. A
client that dies closes the connection the same way. A stale `control` is
replaced only while the record's lock is held.

## Validation

Unit tests cover bounded frames, the record and its lock, an unreadable
record's removal, each request line and answer as root's intake parses
them, home-relative grant paths, a held sandbox never created over and
its adoption, the import's drops, adoptions, bounds, report and
convergence after an interruption, `init-default`'s order, its refusal
of a linked `~/sandbox` and its completion of a seed cut short, and a
source pin that td-sandbox starts no program outside the jail.
td-authd's tests cover the intake's verbs, `status`, a creation that
never goes over a held sandbox, the deletion's order, mark and home
emptying, root's reading of a home by its listing and mark, the refusal
of a deleted sandbox's launches and grant changes, and the per-name
ordering; td-jail's that td's account may not name the sandbox profile.
td-ui tests verify exact terminal restoration and pin its raw surface.

The `sandbox-evidence` boot oracle (`TD-SANDBOX-ACCOUNT-OK`) runs the
whole path on an installed autotest image. As the human it creates a
sandbox, shares a directory and sets network off, then runs a command
that must see a sandbox uid, write and execute in the share, list the
human's home as the share alone (the home is 0700, so a listing proves
the jail's view of it, not the discretionary bits, hides the rest), fail
to read a world-readable file of the home it was not granted, find no
network beyond loopback, and fill its own home with a directory of mode 000.
Root checks the shared file's owner on disk. With the sandbox's record
removed, a create over the held sandbox must be refused, naming `adopt`,
its home kept, and `adopt` must record it; with network host a run must
see an interface beyond loopback, and the grant table must hold both
rows. After the delete no row of the name may remain, the ledger row
must stay, and the home must hold root's mark alone; then the seeded
`default` must write and run a script in `~/sandbox`. No pty session is
driven, since no program in the image's `/bin` opens one for a test;
`start` shares the launch path with `run` but for the relay's PTY, which
td-ui's tests cover.
