//! The interpreter: shell state, the command-tree walker, redirections and
//! command substitution. Builtins live in `builtin.rs`; process spawning and
//! pipelines in `process.rs`.
//!
//! Control flow that unwinds the tree — `break`, `continue`, `return`, `exit`
//! and a fatal expansion error — travels as the `Sig` error variant of `R`, so
//! the ordinary `?` operator carries it out to the right handler. A normal
//! command just leaves its status in `Shell::status`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::arith;
use crate::ast::{AndOr, ArithCmp, Cmd, CondExpr, CondOp, Conn, List, Pipeline, Redir, Sep, Stage, Word};
use crate::builtin;
use crate::expand;
use crate::parser::{self, Aliases};
use crate::pattern;
use crate::process::{self, Fds};

/// A non-local transfer of control. `Sig::Exit` and `Sig::Abort` leave the
/// interpreter; the loop and function forms are caught by their construct.
#[derive(Clone, Copy, Debug)]
pub enum Sig {
    Break(u32),
    Continue(u32),
    Return(i32),
    Exit(i32),
    /// dash's `sh_error`/`EXERROR`: abandon the command being run. Identical to
    /// `Exit` everywhere except an interactive top level, which catches it and
    /// prompts again -- so a typo costs the command, not the session. The two
    /// wrong answers it replaces were "always exit" (which ends an interactive
    /// shell) and "return Ok if interactive" (which resumes the very loop the
    /// failing command was meant to leave, and spins).
    Abort(i32),
    /// The terminal interrupted the shell -- inferred from a foreground child
    /// dying of SIGINT, since td-sh installs no handler to be told directly.
    ///
    /// Distinct from `Abort` because of where it must NOT stop. A subshell,
    /// a pipeline stage and a command substitution are all in-process CLONES
    /// here, and each confines an `exit` or a fatal error to itself -- correctly,
    /// since a forked one would only have ended that process. An interrupt is the
    /// opposite: the terminal signals the whole foreground process group, so a
    /// forked shell at every level would have died of it too. Confining it would
    /// leave `x=$(sleep 100); echo after` printing `after`, which is the loop
    /// nothing can stop wearing a different hat.
    Interrupt(i32),
}

pub type R<T> = Result<T, Sig>;

#[derive(Clone, Debug)]
pub struct Var {
    /// `None` is DECLARED BUT UNSET: the name reads as absent and stays out of a
    /// child's environment, but keeps its attributes, so `export x` before any
    /// value -- and the name a bare `local` just unset -- still export once
    /// assigned. ash's `struct var` with `VUNSET` is the same state.
    pub value: Option<String>,
    pub exported: bool,
    pub readonly: bool,
    /// Declared by `local` in SOME live frame, not necessarily the current one:
    /// ash sets `VSTRFIXED` on the variable rather than tracking it per frame
    /// (ash.c:10020), so an inner function's `unset` sees an outer function's
    /// declaration. The frame's own restore puts the flag back with the binding.
    pub localised: bool,
    /// ash's `VDYNAMIC`: a read runs code instead of returning the stored text.
    /// It lives on the VARIABLE and not on the shell because `unsetvar` clears
    /// it while a frame's restore puts it back (ash.c's `mklocal`/
    /// `poplocalvars`), so `local RANDOM` suspends the generator for the call
    /// and returns it afterwards.
    pub dynamic: Option<Dyn>,
}

/// The two names whose value is COMPUTED at each read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dyn {
    /// `$RANDOM`: every read draws, and an assignment SEEDS the generator
    /// rather than being stored -- so it survives one.
    Random,
    /// `$LINENO`: every read reports the line of the command being run, and
    /// anything STORED in it ends that. dash tests `v->text == linenovar`
    /// (var.c:316), which stops matching the moment an assignment -- or an
    /// inherited `LINENO=50` -- puts another string there, so the two cases
    /// need no separate rule.
    Lineno,
}

/// A defined function: its body, and the line the DEFINITION opened on. The
/// line is not the body's own -- it is what dash subtracts from every line
/// inside the call (`funcline`, eval.c:996).
#[derive(Clone, Debug)]
pub struct Func {
    pub line: u32,
    /// `Arc` so a call site can hold the body while the definition is redefined
    /// out from under it. It carries its own line: a compound's header --
    /// a `for` word list, a `case` subject, a redirection target -- expands
    /// under the BODY's line, not under the caller's.
    pub body: Arc<Stage>,
}

/// A binding displaced by `local`, kept so the function's return can put it back.
/// `None` means the name did not exist, so restoring it is an unset.
#[derive(Clone, Debug)]
pub enum Local {
    Var(String, Option<Var>),
    /// `local -`, which saves the option set rather than a variable.
    Opts(Opts),
    /// Only ever in `pending_unwind`: the frame depth a terminating unwind did NOT
    /// put back, so a recovery can. Carrying it here rather than at each recovery
    /// site is what makes "undo everything that unwind deferred" one operation, and
    /// what gets the OUTERMOST frame's value when several nest.
    Depth(u32),
}

/// The `set -o` flags the interpreter honours.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    pub errexit: bool,   // -e
    pub nounset: bool,   // -u
    pub xtrace: bool,    // -x
    pub noglob: bool,    // -f
    pub verbose: bool,   // -v
    pub noclobber: bool, // -C
    pub noexec: bool,    // -n
    pub allexport: bool, // -a
    /// `-s`: read the program from stdin. Only the invocation acts on it, as in
    /// dash, but it is an optlist entry there so it shows up in `$-`.
    pub stdin: bool,
}

impl Opts {
    /// The `$-` letters. The order is dash's `optlist`, which is the order it
    /// prints them in -- not alphabetical and not the order they were set.
    pub fn letters(&self, interactive: bool) -> String {
        let mut s = String::new();
        for (on, c) in [
            (self.errexit, 'e'),
            (self.noglob, 'f'),
            (interactive, 'i'),
            (self.noexec, 'n'),
            (self.stdin, 's'),
            (self.xtrace, 'x'),
            (self.verbose, 'v'),
            (self.noclobber, 'C'),
            (self.allexport, 'a'),
            (self.nounset, 'u'),
        ] {
            if on {
                s.push(c);
            }
        }
        s
    }
}

pub struct Shell {
    pub vars: HashMap<String, Var>,
    pub funcs: HashMap<String, Func>,
    pub params: Vec<String>, // positional parameters $1..
    pub arg0: String,        // $0
    /// What `$LINENO` reads: the input line of the command being run, already
    /// relative to `funcline`. dash keeps the same pair (`lineno` in var.c,
    /// `funcline` in eval.c) and computes the value at each command rather than
    /// at each read. SIGNED because dash's is a plain `int` subtraction that
    /// can go below zero -- see `set_lineno`.
    pub lineno: i64,
    /// ash's `commandname` (ash.c:635): the RUNNING builtin's `argv[0]`, or a
    /// sourced file's path, or `$0` for a script invoked by name -- and `None`
    /// under `-c` and on stdin, where ash never sets it. It is what `diag`
    /// gates BOTH the command component and the line number on, so it decides
    /// where a diagnostic says it happened rather than merely what it is called.
    pub commandname: Option<String>,
    /// ash's `pf_fd > 0`, which with `interactive` is its `!iflag || pf_fd > 0`:
    /// a shell at a terminal prompt reports no line, since the operator just
    /// typed it. The test is the DESCRIPTOR rather than the file -- `sh <
    /// script` reads a real file and ash still prints none, because that file
    /// arrived as fd 0 -- so this is set where a path was opened (a script
    /// operand, a sourced file, a profile) and nowhere else.
    pub input_is_file: bool,
    /// The line the function whose body is running was DEFINED on, or 0 outside
    /// one. dash reports `$LINENO` inside a function relative to its definition
    /// (eval.c:752), where busybox ash and bash report the absolute line; the
    /// corpus grades this shell on dash's answer.
    pub funcline: u32,
    pub status: i32,         // $?
    /// ash's `random_gen`, lazily seeded: `None` is "never seeded", which takes
    /// the pid and the clock on first read as ash's `UNINITED_RANDOM_T` does.
    pub random: Option<crate::random::Rand>,
    /// `$!`, which is UNSET until a background job runs -- ash errors on it
    /// under `set -u` and expands it to nothing otherwise, where a `0` default
    /// would silently name process zero.
    pub last_bg: Option<u32>,
    pub opts: Opts,
    /// The PHYSICAL working directory: what a child process is started in and
    /// what a relative path resolves against, so it is kept canonical.
    pub cwd: PathBuf,
    /// dash's `curdir`: the LOGICAL path, the one the shell was walked to by
    /// name. It is what `$PWD` and `pwd` report, and what `..` is applied to,
    /// so a directory reached through a symlink keeps the name it was reached
    /// by. Only `cd` moves it.
    pub logical_cwd: PathBuf,
    pub fds: Fds,
    /// The background jobs this shell started. Not carried into a clone: a
    /// subshell cannot wait for its parent's, and a `JoinHandle` belongs to one
    /// shell.
    ///
    /// Declared AFTER `fds` deliberately. Fields drop in declaration order, and
    /// dropping this one JOINS every job -- so with the table first the shell
    /// still held its whole descriptor table while it waited, and a job blocked
    /// for EOF on a descriptor only the parent had open never finished:
    /// `cat fifo & exec 3>fifo` hung for good where bash exits at once. Letting
    /// `fds` go first closes the parent's copies; a job that still needs one
    /// holds its own `Arc` of the same `File`, so nothing a job is using shuts.
    pub jobs: crate::jobs::Jobs,
    /// ash's `localvar_stack` depth. `local` is an error at zero and nowhere else,
    /// so this is the whole of "am I somewhere a `local` may be declared".
    pub localvar_depth: u32,
    /// Bindings this function invocation's `local`s displaced, in declaration
    /// order. `call_function` swaps in a fresh list per call, so each invocation
    /// unwinds only its own — including a recursive one's.
    pub locals: Vec<Local>,
    /// Bindings a terminating unwind left standing, newest first, so an EXIT trap
    /// still sees the frame the shell died in. Drained by `unwind_pending` at the
    /// points where the shell recovers instead.
    pub pending_unwind: Vec<Local>,
    /// How much of `pending_unwind` belongs to a frame OUTSIDE the running call.
    /// An EXIT trap runs inside the frame the shell died in, so a `local` at the
    /// trap's top level repeats that frame's -- but a function the trap calls gets
    /// a frame of its own, and must not read the dying one as already declared.
    pub pending_floor: usize,
    /// Environment entries whose NAME does not decode as UTF-8. They are not
    /// variables -- no expansion can spell one -- but a child still inherits them,
    /// so they are carried verbatim rather than dropped or mangled.
    pub opaque_env: Vec<(OsString, OsString)>,
    pub loop_depth: u32,
    /// Runtime recursion depth (function calls + command substitution), bounded so
    /// `f() { f; }; f` and `$( $( … ) )` error instead of overflowing the stack.
    pub run_depth: u32,
    /// Count of command substitutions performed, used to decide the exit status of
    /// an assignment-only command (`x=$(cmd)` takes the last substitution's status).
    pub cmdsubst_count: u64,
    /// Nesting depth of `errexit`-suppressed contexts (an `if`/`while`/`until`
    /// condition). Non-zero means a failing command must NOT trigger `set -e`, and
    /// it propagates into compounds nested inside the condition.
    pub errexit_suppressed: u32,
    pub interactive: bool,
    /// This shell runs on a thread of its own beside its siblings — a pipeline
    /// stage or a background job, or something nested in one. It is what decides
    /// that the two PROCESS-global mechanisms are not this shell's to touch: the
    /// interrupt guard is not taken, and `trap ''` is recorded rather than
    /// installed. Both are one cell shared with whatever else is running, so a
    /// concurrent shell that set either would be setting a sibling's. Carried
    /// into clones: a subshell inside one is still concurrent with the rest.
    pub concurrent: bool,
    /// dash's `inps4`: set while $PS4 is being expanded, so a command
    /// substitution inside it cannot trace itself into infinite regress.
    pub in_ps4: bool,
    /// `getopts` scan cursor, hidden like dash's: the 1-based index of the next
    /// WORD and the byte offset inside the word being consumed (-1 == start a
    /// fresh one). Hidden rather than read back out of $OPTIND because dash keeps
    /// it per argument frame -- a function gets its own, and `set`/`shift` reset
    /// it -- while the OPTIND *variable* is global and only written by `getopts`.
    pub getopts_optind: i64,
    pub getopts_off: i64,
    /// Aliases in force. They are consumed at PARSE time, so only the unit loop
    /// and the other parse entry points read this.
    pub aliases: Aliases,
    /// This `Shell` is an in-process CLONE (subshell, async list, command
    /// substitution), not a forked process. `exec` must not replace the real
    /// process from one, or the rest of the script would be lost with it.
    pub cloned: bool,
    /// Set while a trap action runs, to the status the shell was exiting with.
    /// POSIX makes that the value a bare `exit` in the action reports.
    pub trap_status: Option<i32>,
    /// `trap` actions by signal number, 0 being EXIT. An empty action is POSIX's
    /// "ignore", and that half of `trap` is REAL: it installs `SIG_IGN`, which
    /// the kernel then hands to every child. Only EXIT is ever RUN, though —
    /// CATCHING a signal needs a handler this shell cannot install (see the
    /// crate-root note), so the rest are kept so that `trap` reports them
    /// faithfully.
    pub traps: BTreeMap<u8, String>,
    /// Whether td-sh may set each signal's disposition, answered once by asking
    /// the kernel the first time `trap` names it and cached because after the
    /// first change it can no longer be asked. `false` means the process
    /// STARTED with something other than `SIG_DFL` installed, which is not
    /// td-sh's to overwrite: POSIX says a signal ignored on entry cannot be
    /// trapped or reset, and Rust's runtime ignores SIGPIPE and handles
    /// SEGV/BUS before `main`. Cloned into a subshell exactly as `fork(2)`
    /// copies dash's `sigmode`.
    pub sig_may_set: BTreeMap<u8, bool>,
    /// Dispositions this shell CHANGED, and whether each was IGNORED before it
    /// did — the undo a real fork would not need. One bit says which to put
    /// back because `SIG_IGN` and `SIG_DFL` are the only two td-sh installs.
    /// Recorded only in a clone, and only on the FIRST change to each signal, so
    /// every entry already holds the parent's value and the list is bounded by
    /// the signal count.
    pub sig_undo: Vec<(u8, bool)>,
    /// Signals this shell has INSTALLED an ignore for, which is what the process
    /// is holding. Not the same as the trap table's ignore set: a pipeline stage
    /// RECORDS a trap without installing one, so a stage that clears its parent's
    /// `trap '' TERM` leaves the process still ignoring it, and the spawn has to
    /// know to put `SIG_DFL` back for the child. Cloned like `sig_may_set`.
    pub sig_installed: Vec<u8>,
    /// Set when a DIAGNOSTIC write to fd 2 met a broken pipe, and read back at
    /// the `run_command` choke point so the shell ends there.
    ///
    /// A pending flag rather than a return value, because the alternative is
    /// threading one through `err_line`'s thirty-odd callers and every `trace`;
    /// and because this is what a real SIGPIPE would be — asynchronous, noticed
    /// at the next command rather than at the write. `AtomicBool` so the writers
    /// can keep taking `&Shell`, which most of them only ever had.
    pub stderr_epipe: AtomicBool,
    /// Whether THIS shell has changed the process umask. A clone restores the
    /// mask it captured only when this is set, because a clone that never
    /// touched it has nothing to put back — and with pipeline stages running at
    /// once, restoring anyway is a stage undoing a SIBLING's `umask` at the
    /// moment it happens to exit. Not inherited: a clone starts having changed
    /// nothing, whatever its parent did.
    pub umask_changed: bool,
}

/// Bound on nested command execution — enforced once, at the `run_command` choke
/// point every compound/nested command descends through (subshells, groups,
/// if/for/while bodies, function calls, `eval`, `.`/source, and command
/// substitution). Deep shell recursion is almost always a bug; this fires well
/// past any legitimate script but well before the native stack overflows (which
/// would SIGABRT — there is no unsafe/stack-probe escape hatch here).
const MAX_RUN_DEPTH: u32 = 256;

/// A value the kernel publishes as an ordinary file: the whole contents less the
/// terminating newline when `key` is `None`, else the remainder of the first line
/// starting with it. This is how `$PPID` and `$HOSTNAME` are answered without
/// `getppid(2)` or `gethostname(2)` -- a new syscall is an UNSAFE.md amendment,
/// and `/proc` makes one unnecessary.
///
/// Two rules the obvious `trim()` gets wrong, both because ash reads these from
/// a syscall that returns the bytes verbatim. The newline is procfs's framing
/// and nothing else is: a hostname of `" x "` keeps its spaces. And `None` means
/// the field could not be READ, where an EMPTY value is a value -- `uname(2)`
/// cannot fail, so an empty nodename is seeded set-and-empty, and collapsing the
/// two would leave `HOSTNAME` unset and hand `set -u` back the abort this
/// seeding exists to remove. The keyed form still trims, because there the
/// separator is whitespace rather than data.
fn proc_field(path: &str, key: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    match key {
        None => Some(text.strip_suffix('\n').unwrap_or(&text).to_string()),
        Some(k) => text
            .lines()
            .filter_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_string())
            .next(),
    }
}

/// The `SHLVL` an inherited value yields, as `utoa((int)strtol(p, NULL, 10) + 1)`
/// (ash.c:14543). That is C `atoi`, so the three parts a whole-string parse gets
/// wrong: a leading numeric PREFIX counts (`4x` is 4), the scan SATURATES rather
/// than failing (a value past `LONG_MAX` truncates to -1, so the next shell is 0
/// and not 1), and the sum is printed UNSIGNED (`-3` yields 4294967294).
fn shlvl_next(inherited: &str) -> u32 {
    let mut rest = inherited.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let neg = match rest.strip_prefix('-') {
        Some(r) => {
            rest = r;
            true
        }
        None => {
            rest = rest.strip_prefix('+').unwrap_or(rest);
            false
        }
    };
    let mut acc: i64 = 0;
    for c in rest.chars() {
        let Some(d) = c.to_digit(10).map(i64::from) else {
            break;
        };
        let step = acc
            .checked_mul(10)
            .and_then(|a| if neg { a.checked_sub(d) } else { a.checked_add(d) });
        acc = match step {
            Some(v) => v,
            // strtol clamps and keeps scanning; the remaining digits cannot
            // move it back off the limit.
            None if neg => i64::MIN,
            None => i64::MAX,
        };
    }
    // Truncation to `int` then the unsigned print, in one step: the low 32 bits.
    u32::try_from(acc.rem_euclid(0x1_0000_0000))
        .unwrap_or_default()
        .wrapping_add(1)
}

/// Whether two paths name the same directory, by device and inode as dash's
/// startup check does -- a string compare would accept a stale inherited `PWD`
/// that merely looks plausible.
fn same_dir(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

impl Shell {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut vars = HashMap::new();
        let mut opaque_env = Vec::new();
        // `vars_os`, because `vars` ABORTS on an entry that does not decode. An
        // undecodable VALUE then becomes U+FFFD, the rule `read` and `$( )` already
        // follow; an undecodable NAME is kept verbatim instead, since no expansion
        // can spell one and rewriting it could only corrupt what a child inherits.
        for (k, v) in std::env::vars_os() {
            let k = match k.into_string() {
                Ok(name) => name,
                Err(raw) => {
                    opaque_env.push((raw, v));
                    continue;
                }
            };
            vars.insert(
                k,
                Var {
                    value: Some(v.to_string_lossy().into_owned()),
                    exported: true,
                    readonly: false,
                    localised: false,
                    dynamic: None,
                },
            );
        }
        let mut sh = Shell {
            vars,
            funcs: HashMap::new(),
            params: Vec::new(),
            arg0: "td-sh".to_string(),
            lineno: 1,
            commandname: None,
            input_is_file: false,
            funcline: 0,
            status: 0,
            last_bg: None,
            random: None,
            opts: Opts::default(),
            logical_cwd: cwd.clone(),
            cwd,
            fds: Fds::new(),
            localvar_depth: 0,
            locals: Vec::new(),
            pending_unwind: Vec::new(),
            pending_floor: 0,
            opaque_env,
            loop_depth: 0,
            run_depth: 0,
            cmdsubst_count: 0,
            errexit_suppressed: 0,
            interactive: false,
            concurrent: false,
            jobs: crate::jobs::Jobs::new(),
            in_ps4: false,
            getopts_optind: 1,
            getopts_off: -1,
            aliases: Aliases::new(),
            cloned: false,
            traps: BTreeMap::new(),
            sig_may_set: BTreeMap::new(),
            sig_installed: Vec::new(),
            stderr_epipe: AtomicBool::new(false),
            umask_changed: false,
            sig_undo: Vec::new(),
            trap_status: None,
        };
        // dash's varinit carries `PS4=+ `, so it is a real variable a script can
        // read, not just a default the tracer falls back to.
        if sh.get_var("PS4").is_none() {
            let _ = sh.set_var("PS4", "+ ");
        }
        // POSIX seeds these when absent; scripts assume they exist.
        if sh.get_var("IFS").is_none() {
            let _ = sh.set_var("IFS", " \t\n");
        }
        // dash's setpwd at startup: an inherited PWD is kept only if it is
        // absolute AND names this very directory (same dev/ino, so a stale or
        // lying value from the parent cannot survive); otherwise the physical
        // path replaces it. Either way PWD ends up set and EXPORTED.
        let logical = match sh.get_var("PWD") {
            Some(p) if p.starts_with('/') && same_dir(Path::new(&p), &sh.cwd) => {
                PathBuf::from(p)
            }
            _ => sh.cwd.clone(),
        };
        let _ = sh.set_var("PWD", &logical.to_string_lossy());
        sh.logical_cwd = logical;
        sh.export_var("PWD");
        // POSIX: OPTIND is 1 at shell start, overriding any imported value.
        let _ = sh.set_var("OPTIND", "1");
        // ash's varinit carries RANDOM as a VDYNAMIC name that is UNSET until
        // first read (ash.c:2169). The environment import runs through
        // `setvareq`, which fires that func -- so an INHERITED RANDOM SEEDS the
        // generator instead of sitting in the map as an ordinary string.
        let inherited_random = sh.get_var("RANDOM");
        match sh.vars.get_mut("RANDOM") {
            Some(v) => v.dynamic = Some(Dyn::Random),
            None => {
                sh.vars.insert(
                    "RANDOM".to_string(),
                    Var {
                        value: None,
                        exported: false,
                        readonly: false,
                        localised: false,
                        dynamic: Some(Dyn::Random),
                    },
                );
            }
        }
        if let Some(text) = inherited_random {
            sh.random = Some(crate::random::Rand::seeded(crate::random::seed_of(&text)));
        }
        sh.start_lineno();
        // The names ash seeds itself when the environment carries none. Each is
        // an ORDINARY variable a script may reassign -- only PWD and SHLVL are
        // exported -- but a shell that leaves them unset makes `set -u` fatal on
        // idioms like `${HOSTNAME%%.*}` that work everywhere else.
        if sh.get_var("PATH").is_none() {
            let _ = sh.set_var("PATH", "/sbin:/usr/sbin:/bin:/usr/bin");
        }
        if sh.get_var("PS1").is_none() {
            let _ = sh.set_var("PS1", "\\w \\$ ");
        }
        if sh.get_var("PS2").is_none() {
            let _ = sh.set_var("PS2", "> ");
        }
        if sh.get_var("HOSTNAME").is_none() {
            if let Some(h) = proc_field("/proc/sys/kernel/hostname", None) {
                let _ = sh.set_var("HOSTNAME", &h);
            }
        }
        // PPID is the one name the environment does NOT get to supply: ash sets
        // it unguarded (ash.c:14540), so a stale exported value from the parent
        // is replaced rather than believed.
        if let Some(v) = proc_field("/proc/self/status", Some("PPid:")) {
            let _ = sh.set_var("PPID", &v);
        }
        // SHLVL counts nested shells. Exported, so the count reaches the child
        // that increments it next; an absent value reads as ash's `atoi(NULL
        // ? ...: 0)`, which is the same 0 an unparsable one gives.
        let depth = shlvl_next(sh.get_var("SHLVL").as_deref().unwrap_or(""));
        let _ = sh.set_var("SHLVL", &depth.to_string());
        sh.export_var("SHLVL");
        sh
    }

    /// A shell with no inherited environment — deterministic for unit tests.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let mut sh = Shell {
            vars: HashMap::new(),
            funcs: HashMap::new(),
            params: Vec::new(),
            // Seeded rather than left lazy, so a unit test that reads `$RANDOM`
            // is deterministic instead of taking the pid and the clock.
            random: Some(crate::random::Rand::seeded(1)),
            arg0: "td-sh".to_string(),
            lineno: 1,
            commandname: None,
            input_is_file: false,
            funcline: 0,
            status: 0,
            last_bg: None,
            opts: Opts::default(),
            logical_cwd: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/")),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            fds: Fds::new(),
            localvar_depth: 0,
            locals: Vec::new(),
            pending_unwind: Vec::new(),
            pending_floor: 0,
            opaque_env: Vec::new(),
            loop_depth: 0,
            run_depth: 0,
            cmdsubst_count: 0,
            errexit_suppressed: 0,
            interactive: false,
            concurrent: false,
            jobs: crate::jobs::Jobs::new(),
            in_ps4: false,
            getopts_optind: 1,
            getopts_off: -1,
            aliases: Aliases::new(),
            cloned: false,
            traps: BTreeMap::new(),
            sig_may_set: BTreeMap::new(),
            sig_installed: Vec::new(),
            stderr_epipe: AtomicBool::new(false),
            umask_changed: false,
            sig_undo: Vec::new(),
            trap_status: None,
        };
        sh.vars.insert(
            "RANDOM".to_string(),
            Var {
                value: None,
                exported: false,
                readonly: false,
                localised: false,
                dynamic: Some(Dyn::Random),
            },
        );
        sh.start_lineno();
        let _ = sh.set_var("IFS", " \t\n");
        let _ = sh.set_var("OPTIND", "1");
        let _ = sh.set_var("PS4", "+ ");
        sh
    }

    /// Make `LINENO` answer with the line -- unless the environment already
    /// carried one, which is a string STORED in it and so freezes it exactly as
    /// an assignment does. Measured: `LINENO=50 dash -c 'echo $LINENO'` is 50,
    /// where bash's is 1.
    fn start_lineno(&mut self) {
        if self.vars.contains_key("LINENO") {
            return;
        }
        self.vars.insert(
            "LINENO".to_string(),
            Var {
                // Set-and-EMPTY, not unset: dash's `linenovar` is a buffer that
                // exists from the start and is only filled by a read, so before
                // the first one `set` lists `LINENO=''` and an exported one
                // reaches the child set and empty. The value is never what a
                // `$LINENO` answers with -- the dynamic arm intercepts -- so
                // this is only what the LISTINGS and the environment see.
                value: Some(String::new()),
                exported: false,
                readonly: false,
                localised: false,
                dynamic: Some(Dyn::Lineno),
            },
        );
    }

    /// dash's `errlinno = lineno = n->…linno; if (funcline) lineno -= funcline
    /// - 1`, run once per command node rather than at each `$LINENO` read.
    ///
    /// The subtraction is plain and signed, as dash's is, and really can go
    /// NEGATIVE: a string reparsed inside a function starts again at line 1
    /// while `funcline` is wherever the function was defined, so with `f`
    /// defined on line 4, `eval 'echo $LINENO'` in its body is `-2` here and
    /// in dash. Saturating at zero would be tidier and would be this shell
    /// inventing an answer.
    pub fn set_lineno(&mut self, line: u32) {
        let line = i64::from(line);
        self.lineno = match self.funcline {
            0 => line,
            f => line - i64::from(f) + 1,
        };
    }

    pub fn get_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).and_then(|v| v.value.clone())
    }

    /// Assign a shell variable, honouring the readonly attribute. A write to a
    /// readonly name is dash's sh_error, so it goes out as `Sig::Abort` and `?`
    /// carries it to the nearest handler rather than leaving a status to test.
    /// ash's OPTIND hook (`getoptsreset`): any assignment moves the cursor and
    /// abandons a half-consumed word. An all-digit string is taken (0 coerced up
    /// to 1) and anything else -- " 2", "+1", "-1", the empty string -- IS 1.
    fn var_hook(&mut self, name: &str, value: &str) {
        match self.vars.get(name).and_then(|v| v.dynamic) {
            Some(Dyn::Random) => {
                self.random = Some(crate::random::Rand::seeded(crate::random::seed_of(value)));
                return;
            }
            // The freeze belongs to `set_var`, which is the only path that
            // STORES: this hook also runs on a local frame's restore, where
            // clearing the flag would end the tracking a `local` only suspends.
            Some(Dyn::Lineno) | None => {}
        }
        if name != "OPTIND" {
            return;
        }
        // ash's `getoptsreset` (ash.c:2272) assigns 1 first and takes the value
        // only if `is_number` passes, which is what stops `number()` reaching its
        // `Illegal number` raise: anything else restarts the scan silently.
        let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        self.getopts_optind =
            if digits { value.parse::<i64>().unwrap_or(i64::MAX).max(1) } else { 1 };
        self.getopts_off = -1;
    }

    /// The same hook for the value going AWAY. ash fires it on an unset too
    /// (`unsetvar` is `setvar(s, NULL, 0)`), which reaches `getoptsreset` with no
    /// value and so restarts at word 1. dash rejects `unset OPTIND` outright, so
    /// ash decides this one.
    fn unset_hook(&mut self, name: &str) {
        // ash: "as soon as they're unset, they're no longer dynamic" (the
        // comment on `lookupvar`). `local VAR` reaches this through the same
        // `unsetvar`, which is why only the frame's RESTORE brings it back.
        if let Some(v) = self.vars.get_mut(name) {
            v.dynamic = None;
        }
        if name == "OPTIND" {
            self.getopts_optind = 1;
            self.getopts_off = -1;
        }
    }

    pub fn set_var(&mut self, name: &str, value: &str) -> R<()> {
        // Assigning OPTIND at all -- even the value it already holds -- restarts
        // `getopts` at a word boundary. `getopts` itself re-establishes the
        // offset after publishing OPTIND.
        // A DYNAMIC variable is exempt from the readonly refusal: ash tests
        // `(flags & (VREADONLY|VDYNAMIC)) == VREADONLY`, so `readonly RANDOM`
        // still lists the name readonly while an assignment to it reseeds
        // rather than failing.
        let dynamic = !self.readonly_refuses(name);
        self.var_hook(name, value);
        match self.vars.get_mut(name) {
            // dash reports this through sh_error, which ends a non-interactive
            // shell with status 2 -- not a status a script can test.
            Some(v) if v.readonly && !dynamic => {
                return Err(self.readonly_fatal(name));
            }
            Some(v) => {
                // dash's LINENO stops answering with the line the moment
                // anything is stored in it, which is what `LINENO=99; echo
                // $LINENO` printing 99 is.
                if v.dynamic == Some(Dyn::Lineno) {
                    v.dynamic = None;
                }
                v.value = Some(value.to_string());
                v.exported |= self.opts.allexport;
            }
            None => {
                self.vars.insert(
                    name.to_string(),
                    Var {
                        value: Some(value.to_string()),
                        // `set -a` is exactly this: every name an assignment
                        // touches is marked for export as it is written.
                        exported: self.opts.allexport,
                        readonly: false,
                        localised: false,
                        dynamic: None,
                    },
                );
            }
        }
        Ok(())
    }

    pub fn export(&mut self, name: &str) {
        if let Some(v) = self.vars.get_mut(name) {
            v.exported = true;
        } else {
            self.vars.insert(
                name.to_string(),
                Var {
                    value: None,
                    exported: true,
                    readonly: false,
                    localised: false,
                    dynamic: None,
                },
            );
        }
    }

    /// `export -n`. `set -a` exports an absent name's fresh entry (ash.c:2417
    /// via 14164); a name ash SEEDS is found and flag-edited instead (14158).
    pub fn unexport(&mut self, name: &str) {
        if let Some(v) = self.vars.get_mut(name) {
            v.exported = false;
        } else if self.opts.allexport && !ash_seeds_entry(name) {
            self.export(name);
        }
    }

    pub fn set_readonly(&mut self, name: &str) {
        if let Some(v) = self.vars.get_mut(name) {
            v.readonly = true;
        } else {
            self.vars.insert(
                name.to_string(),
                Var {
                    value: None,
                    // `set -a` exports a fresh entry (ash.c:2417 via 14164),
                    // never a seeded one, which `exportcmd` flag-edits (14158).
                    exported: self.opts.allexport && !ash_seeds_entry(name),
                    readonly: true,
                    localised: false,
                    dynamic: None,
                },
            );
        }
    }

    /// Mark an existing name for export. dash's setvar takes a flags word, so
    /// `PWD` and `OLDPWD` are written and exported in one step; this is that
    /// second half for the callers that need it.
    pub fn export_var(&mut self, name: &str) {
        if let Some(v) = self.vars.get_mut(name) {
            v.exported = true;
        }
    }

    /// ash's one readonly test, `(flags & (VREADONLY|VDYNAMIC)) == VREADONLY`
    /// (ash.c:2421), scoped to the one dynamic name ash HAS: `readonly RANDOM`
    /// still lists the name readonly while an assignment reseeds, where dash
    /// refuses `readonly LINENO; LINENO=5` with `is read only`. `setvareq` is
    /// reached by assignment AND by unset, so all three callers owe the same
    /// predicate.
    fn readonly_refuses(&self, name: &str) -> bool {
        // Scoped to RANDOM rather than to DYNAMIC: ash exempts its one dynamic
        // name, and dash -- whose LINENO this is -- refuses `readonly LINENO;
        // LINENO=5` with `is read only`, measured.
        self.vars.get(name).is_some_and(|v| v.readonly && v.dynamic != Some(Dyn::Random))
    }

    pub fn unset_var(&mut self, name: &str) -> bool {
        if self.readonly_refuses(name) {
            return false;
        }
        // A readonly DYNAMIC name is the only one that gets past that gate with
        // an attribute worth keeping: ash unsets through `setvareq`, so the
        // struct survives and VREADONLY with it -- the NEXT assignment is still
        // refused, which dropping the entry would lose.
        if self.vars.get(name).is_some_and(|v| v.readonly) {
            return self.unset_value(name);
        }
        // Under `set -a`, `setvareq` ORs `VEXPORT` into the flags an unset writes
        // (ash.c:2417), so the free test below it can never hold: the entry
        // survives -- created, if the name was new -- and only the value goes.
        if self.opts.allexport {
            return self.unset_value(name);
        }
        // A seeded entry survives the unset (ash.c:2440) and keeps its
        // `VEXPORT` (2449), which a later bare `readonly` then flag-edits.
        if ash_seeds_entry(name) {
            return self.unset_value(name);
        }
        // Once per path: the three branches above hook inside `unset_value`.
        self.unset_hook(name);
        self.vars.remove(name);
        true
    }

    /// Whether some live frame declared this name with `local`. Deliberately not
    /// read off `locals`, which holds only the CURRENT frame: ash keeps the answer
    /// on the variable, so a function called from one that localised the name sees
    /// the declaration too.
    pub fn is_local(&self, name: &str) -> bool {
        self.vars.get(name).is_some_and(|v| v.localised)
    }

    /// `local NAME`'s effect on the variable itself, whatever form it took: the
    /// entry exists and is flagged from here on, even if the name was new and has
    /// no value yet (ash's `setvar(name, NULL, VSTRFIXED)`).
    pub fn mark_local(&mut self, name: &str) {
        match self.vars.get_mut(name) {
            Some(v) => v.localised = true,
            None => {
                self.vars.insert(
                    name.to_string(),
                    Var {
                        value: None,
                        exported: false,
                        readonly: false,
                        localised: true,
                        dynamic: None,
                    },
                );
            }
        }
    }

    /// What a bare `local x` does: clear the VALUE but keep the entry, so the
    /// name's export attribute survives being localised and a later assignment
    /// still reaches a child. The `unset` builtin drops the attributes with the
    /// name; ash's `mklocal` keeps them, and this is that difference.
    pub fn unset_value(&mut self, name: &str) -> bool {
        if self.readonly_refuses(name) {
            return false;
        }
        // The `VEXPORT` that `set -a` ORs in is written to the entry too, so an
        // unset under it leaves the name marked for export even if it never was.
        let allexport = self.opts.allexport;
        if let Some(v) = self.vars.get_mut(name) {
            v.value = None;
            v.exported |= allexport;
        } else if allexport {
            self.vars.insert(
                name.to_string(),
                Var {
                    value: None,
                    exported: true,
                    readonly: false,
                    localised: false,
                    dynamic: None,
                },
            );
        }
        self.unset_hook(name);
        true
    }

    /// The environment a child process inherits: every exported variable.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(_, v)| v.exported)
            .filter_map(|(k, v)| Some((k.clone(), v.value.clone()?)))
            .collect()
    }

    /// Resolve a path against the shell's logical cwd.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// busybox ash declares `$?` as `uint8_t exitstatus` (ash.c), so a status wider
    /// than a byte is narrowed the moment it is stored -- `return 300` leaves 44.
    /// dash's is an `int` and keeps 300; the chain puts ash first.
    pub fn set_status(&mut self, code: i32) {
        self.status = code & 0xff;
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse and run a whole program, returning the final `$?`. A parse error prints
/// to stderr and yields 2 (the POSIX syntax-error status).
pub fn run_program(sh: &mut Shell, src: &str) -> i32 {
    let status = match run_source(sh, src) {
        Ok(()) => sh.status,
        Err(Sig::Exit(code) | Sig::Abort(code) | Sig::Interrupt(code)) => code,
        // A stray break/continue/return at the top level is not an error worth
        // aborting the process over; POSIX leaves it unspecified.
        Err(_) => sh.status,
    };
    // Narrowed here so the return really is `$?`: an `exit 300` reaches this arm
    // whole, and this is where the shell environment ends.
    run_exit_trap(sh, status) & 0xff
}

/// Run one unit read at an interactive prompt. `Some(code)` means the shell is
/// over; `None` means prompt again -- which is the whole point of `Sig::Abort`,
/// and the one place it differs from `Sig::Exit`.
pub fn run_interactive_unit(sh: &mut Shell, list: &List) -> Option<i32> {
    match run_list(sh, list) {
        Ok(()) => None,
        Err(Sig::Exit(code)) => Some(code),
        Err(Sig::Abort(code) | Sig::Interrupt(code)) => {
            // The shell is recovering, not exiting, so whatever the unwind left
            // standing goes now rather than into the next prompt. Nothing outer
            // survives a top-level recovery, so the mark is 0. An interrupt lands
            // here too: Ctrl-C returns an interactive shell to its prompt.
            unwind_pending_to(sh, 0);
            sh.set_status(code);
            None
        }
        // A stray break/continue/return typed at a prompt is not an error.
        Err(_) => None,
    }
}

/// Run the EXIT trap on the way out of a shell environment. POSIX: the action sees
/// the exiting status in `$?`, its own status is discarded, and only an `exit` WITH
/// an operand replaces the status (`trap_status` is what makes a bare one report the
/// status being exited with). Taken out of the table first, so it cannot re-enter.
pub fn run_exit_trap(sh: &mut Shell, status: i32) -> i32 {
    let Some(action) = sh.traps.remove(&0) else {
        return status;
    };
    sh.set_status(status);
    // The trap RUNS, which is the whole difference between this and a signal
    // death and is what the stdout rule beside it already gives. Its own first
    // command would otherwise meet the pending flag and return 141 having done
    // nothing, silently losing every cleanup trap on this path. If the action
    // writes to the same broken pipe it sets the flag again and stops there,
    // which is bounded and is what a second SIGPIPE would do.
    sh.stderr_epipe.store(false, Ordering::Relaxed);
    let saved = sh.trap_status.replace(status);
    let code = match run_source(sh, &action) {
        Ok(()) => status,
        Err(Sig::Exit(code) | Sig::Abort(code) | Sig::Interrupt(code)) => code,
        Err(_) => status,
    };
    sh.trap_status = saved;
    code
}

/// Run `src` one top-level unit at a time, as dash reads a script: a command is
/// parsed only once everything before it has run. That is what makes an `alias`
/// visible to the next line but not to the rest of its own line. A syntax error
/// stops the run with status 2, reported as `$0: {what}{error}`.
pub fn run_source(sh: &mut Shell, src: &str) -> R<()> {
    let mut units = parser::Units::new(src);
    loop {
        match units.next_unit(&sh.aliases) {
            None => return Ok(()),
            Some(Err(e)) => {
                // The parser's position, not `$LINENO`: nothing RAN at the
                // point a parse failed, so the line the shell is otherwise
                // holding belongs to some earlier command.
                sh.set_lineno(units.error_line());
                let _ = diag(sh, &e);
                sh.set_status(2);
                // Abandons the enclosing list, as `eval 'if'; echo` shows in both
                // references: reporting and returning Ok ran the rest of it.
                return Err(Sig::Abort(2));
            }
            // Still PARSED under `-n`, which is the point of the mode: the syntax
            // errors are reported. `run_command` is what declines to run it.
            Some(Ok(list)) => run_list(sh, &list)?,
        }
    }
}


pub fn run_list(sh: &mut Shell, list: &List) -> R<()> {
    for (and_or, sep) in &list.items {
        if *sep == Sep::Bg {
            // An ISOLATED subshell, so the job's variable/cwd/option changes
            // cannot leak back, running on a THREAD of its own -- `jobs.rs` has
            // why it is a thread and what that costs.
            match crate::jobs::spawn(process::fork_shell(sh), and_or) {
                // A job that could not START is the shell failing, not the job
                // reporting -- said out loud rather than left as a silent 0,
                // since `&` otherwise reports success for a list that never ran.
                // `$!` is left alone: it still names whatever job it named, and
                // moving it to an id nothing is running under would make the
                // `wait` that follows answer for the wrong job.
                Err(e) => {
                    let _ = diag(sh, &format!("cannot start job: {e}"));
                    sh.set_status(1);
                }
                // The job is already RUNNING, so an exhausted id table is not a
                // reason to pretend it is not: it is joined at the end like any
                // other and only `$!`/`wait` cannot name it. Left where it was
                // rather than unset, for the reason above.
                Ok(handle) => match sh.jobs.record(handle) {
                    Some(id) => {
                        sh.last_bg = Some(id);
                        sh.set_status(0);
                    }
                    None => {
                        let _ = diag(sh, "too many jobs to name this one");
                        sh.set_status(0);
                    }
                },
            }
        } else {
            run_and_or(sh, and_or)?;
        }
    }
    Ok(())
}

/// Run a list as an `if`/`elif`/`while`/`until` CONDITION: `errexit` is suppressed
/// for the whole subtree (POSIX), so a failing test does not exit the shell. The
/// suppression is a counter, so it also covers compounds nested in the condition.
fn run_condition(sh: &mut Shell, list: &List) -> R<()> {
    sh.errexit_suppressed += 1;
    let result = run_list(sh, list);
    sh.errexit_suppressed = sh.errexit_suppressed.saturating_sub(1);
    result
}

pub(crate) fn run_and_or(sh: &mut Shell, and_or: &AndOr) -> R<()> {
    let n_rest = and_or.rest.len();
    // Operand 0 is the structurally-last only when there is no `&&`/`||` tail.
    run_operand(sh, &and_or.first, n_rest == 0)?;
    for (idx, (conn, pipe)) in and_or.rest.iter().enumerate() {
        let go = match conn {
            Conn::And => sh.status == 0,
            Conn::Or => sh.status != 0,
        };
        if !go {
            continue;
        }
        run_operand(sh, pipe, idx + 1 == n_rest)?;
    }
    Ok(())
}

/// Run one `&&`/`||` operand. POSIX ignores `errexit` while executing any operand
/// that is not the structurally-last, and any `!`-negated pipeline. The exemption
/// must cover the WHOLE (possibly compound/function) operand — a failing command
/// nested inside an exempt operand must not exit the shell — so it is a
/// suppression scope, not a post-hoc check. The final, non-negated operand is
/// then subject to `errexit` only where its node is one ash tests: see
/// `checks_errexit`.
fn run_operand(sh: &mut Shell, pipe: &Pipeline, is_last: bool) -> R<()> {
    let exempt = !is_last || pipe.bang;
    if exempt {
        sh.errexit_suppressed += 1;
    }
    let result = run_pipeline(sh, pipe);
    if exempt {
        sh.errexit_suppressed = sh.errexit_suppressed.saturating_sub(1);
    }
    result?;
    if !exempt && checks_errexit(pipe) {
        maybe_errexit(sh)?;
    }
    Ok(())
}

/// Whether this node's own status is tested against `errexit` -- ash's
/// `checkexit`, which `evaltree` reaches from `NCMD`, `NPIPE` and
/// `NSUBSHELL`/`NBACKGND` and from nowhere else. Every compound leaves it 0 and
/// so never exits the shell on its own status: `NIF`, `NFOR`, `NWHILE`, `NCASE`,
/// the `NREDIR` that wraps a compound carrying redirections, `NNOT` and `NDEFUN`.
///
/// That looks like it would defeat `set -e` inside every compound and does not,
/// because a body's own commands are checked as they run: `if :; then false; fi`
/// exits at the `false`, and the `if` never gets to report anything. It decides
/// only the case where a body ends in a command that was NOT checked -- a
/// compound's failed redirection, an `!`, or a non-final `&&`/`||` operand --
/// and there the enclosing compound must not re-test a status the rule has
/// already passed over.
///
/// A subshell is the exception among compounds and it is the PARSER's doing:
/// `parse_command` wraps a redirected compound in an `NREDIR` unless it is
/// already an `NSUBSHELL`, whose redirections stay on the node itself -- so
/// `{ :; } <missing` is exempt and `( : ) <missing` exits. `NBACKGND` is checked
/// too but never arrives here: `run_list` dispatches `&` to `jobs::spawn`.
///
/// `NNOT` is `run_operand`'s to answer, along with every non-final `&&`/`||`
/// operand, before it asks. The match below is deliberately exhaustive: a new
/// `Cmd` variant is a compile error until someone says which side it falls on.
fn checks_errexit(pipe: &Pipeline) -> bool {
    // Two or more stages is `NPIPE` whatever they are. Zero is unparseable --
    // `parse_pipeline` seeds `cmds` with one stage -- and would report 0, which
    // `maybe_errexit` passes over anyway.
    let [stage] = pipe.cmds.as_slice() else {
        return true;
    };
    match &stage.cmd {
        Cmd::Simple { .. } | Cmd::Subshell { .. } => true,
        // busybox ash parses `[[` into an ordinary command word list and serves
        // it from the `test` builtin, so it is an `NCMD` like any other.
        Cmd::Cond { .. } => true,
        Cmd::Group { .. }
        | Cmd::If { .. }
        | Cmd::For { .. }
        | Cmd::Loop { .. }
        | Cmd::Case { .. } => false,
        // `NDEFUN`, which reports 0 always -- so this answer is unobservable and
        // is ash's only for being ash's.
        Cmd::FuncDef { .. } => false,
    }
}

fn maybe_errexit(sh: &mut Shell) -> R<()> {
    if sh.opts.errexit && sh.errexit_suppressed == 0 && sh.status != 0 {
        return Err(Sig::Exit(sh.status));
    }
    Ok(())
}

fn run_pipeline(sh: &mut Shell, pipe: &Pipeline) -> R<()> {
    if pipe.cmds.len() == 1 {
        if let Some(stage) = pipe.cmds.first() {
            sh.set_lineno(stage.line);
            run_command(sh, &stage.cmd)?;
        }
    } else {
        process::run_pipeline(sh, &pipe.cmds)?;
    }
    if pipe.bang {
        sh.set_status(i32::from(sh.status == 0));
    }
    Ok(())
}

/// Run one command in the current shell (not a pipeline stage). This is the single
/// choke point where execution nesting is bounded: every compound body, function
/// call, `eval`/`.`, and command-substitution body re-enters here, so one guard
/// covers them all (and their compositions) against a native stack overflow.
pub fn run_command(sh: &mut Shell, cmd: &Cmd) -> R<()> {
    // dash's and busybox ash's `nflag`, which both test at the top of evaltree --
    // this walker's counterpart -- so `-n` stops at the next COMMAND, not merely
    // at the next parsed unit. That is why `set +n` can never turn it back off,
    // and why `eval` and a trap action are suppressed too: they reach evaltree
    // the same way. Neither shell exempts an interactive shell (POSIX 2.5.1 does),
    // so neither does td-sh. `$?` is left alone, as at evaltree's `out:`.
    if sh.opts.noexec {
        return Ok(());
    }
    if sh.run_depth >= MAX_RUN_DEPTH {
        return Err(sh.fatal("maximum recursion depth exceeded", 2));
    }
    sh.run_depth += 1;
    let result = run_command_inner(sh, cmd);
    sh.run_depth -= 1;
    // AFTER the command rather than before the next one, which is not the same
    // thing at the two ends of a script: the LAST command's own diagnostic
    // would otherwise never be looked at, and `set -x; sleep .3; :` with a
    // broken stderr would report 0 where bash reports 141. It is also what
    // makes `set -n` safe to leave above — the flag is seen by the post-check
    // of the very command that set it, so it can never be left pending for a
    // `noexec` early return to step over forever. The command's own error wins,
    // being the more specific answer.
    result?;
    epipe_pending(sh)
}

/// End the shell if a diagnostic write has met a broken pipe since it was last
/// asked. Sampled at the `run_command` choke point every compound body,
/// function call and loop iteration descends through, which is the only place
/// that sees it without every diagnostic returning one.
///
/// dash and bash end these shapes by DYING of SIGPIPE; this shell cannot, since
/// the Rust runtime ignores SIGPIPE before `main`, so it ends the way a caller
/// observes: 128 + SIGPIPE, as `Sig::Exit` so a pipeline stage is confined
/// exactly as a forked producer's death would be.
pub(crate) fn epipe_pending(sh: &Shell) -> R<()> {
    if sh.stderr_epipe.load(Ordering::Relaxed) {
        return Err(Sig::Exit(141));
    }
    Ok(())
}

fn run_command_inner(sh: &mut Shell, cmd: &Cmd) -> R<()> {
    match cmd {
        Cmd::Simple {
            assigns,
            words,
            redirs,
        } => run_simple(sh, assigns, words, redirs),
        Cmd::Subshell { body, redirs } => {
            let list = body.clone();
            let redirs = redirs.clone();
            process::run_subshell(sh, &list, &redirs)
        }
        Cmd::Group { body, redirs } => with_redirs(sh, redirs, |sh| run_list(sh, body)),
        Cmd::Cond { expr, redirs } => with_redirs(sh, redirs, |sh| run_cond(sh, expr)),
        Cmd::If {
            arms,
            otherwise,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_if(sh, arms, otherwise)),
        Cmd::For {
            var,
            words,
            body,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_for(sh, var, words.as_deref(), body)),
        Cmd::Loop {
            until,
            cond,
            body,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_loop(sh, *until, cond, body)),
        Cmd::Case {
            word,
            items,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_case(sh, word, items)),
        Cmd::FuncDef { name, body, line } => {
            if let Some(name) = name {
                sh.funcs.insert(
                    name.clone(),
                    Func {
                        line: *line,
                        body: body.clone(),
                    },
                );
            }
            sh.set_status(0);
            Ok(())
        }
    }
}

fn run_if(
    sh: &mut Shell,
    arms: &[crate::ast::IfArm],
    otherwise: &Option<List>,
) -> R<()> {
    for arm in arms {
        run_condition(sh, &arm.cond)?;
        if sh.status == 0 {
            return run_list(sh, &arm.body);
        }
    }
    if let Some(body) = otherwise {
        return run_list(sh, body);
    }
    sh.set_status(0);
    Ok(())
}

fn run_for(sh: &mut Shell, var: &str, words: Option<&[Word]>, body: &List) -> R<()> {
    let items: Vec<String> = match words {
        Some(ws) => expand::expand_word_list(sh, ws)?,
        None => sh.params.clone(),
    };
    sh.set_status(0);
    sh.loop_depth += 1;
    let result = (|| {
        for item in items {
            sh.set_var(var, &item)?;
            match run_list(sh, body) {
                Ok(()) => {}
                Err(Sig::Break(n)) => return break_out(sh, n),
                Err(Sig::Continue(n)) => {
                    if n > 1 && sh.loop_depth > 1 {
                        return Err(Sig::Continue(n - 1));
                    }
                    // `continue N` past the outermost loop continues this one.
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    })();
    sh.loop_depth -= 1;
    result
}

fn run_loop(sh: &mut Shell, until: bool, cond: &List, body: &List) -> R<()> {
    sh.set_status(0);
    sh.loop_depth += 1;
    // The loop's status is the last body command's (or 0 if the body never
    // ran) — NOT the condition's, which is non-zero on the exit iteration.
    let mut body_status = 0;
    let result = (|| {
        loop {
            run_condition(sh, cond)?;
            let go = if until { sh.status != 0 } else { sh.status == 0 };
            if !go {
                break;
            }
            match run_list(sh, body) {
                Ok(()) => body_status = sh.status,
                Err(Sig::Break(n)) => {
                    body_status = sh.status;
                    return break_out(sh, n);
                }
                Err(Sig::Continue(n)) => {
                    body_status = sh.status;
                    if n > 1 && sh.loop_depth > 1 {
                        return Err(Sig::Continue(n - 1));
                    }
                    // `continue N` past the outermost loop continues this one.
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    })();
    sh.loop_depth -= 1;
    sh.set_status(body_status);
    result
}

/// A `break N` that names an enclosing loop turns into a break of the remaining
/// levels once this loop has stopped. When `N` exceeds the number of enclosing
/// loops, POSIX exits all of them and then continues normally — so the break is
/// only propagated while an enclosing loop actually exists (`loop_depth` still
/// counts this one at the catch point, hence `> 1`).
fn break_out(sh: &mut Shell, n: u32) -> R<()> {
    if n > 1 && sh.loop_depth > 1 {
        Err(Sig::Break(n - 1))
    } else {
        Ok(())
    }
}

/// `[[ expr ]]`. Status 0 for true, 1 for false, 2 for a bad expression -- the
/// statuses `test` uses, so a script cannot tell the two constructs apart by
/// them.
fn run_cond(sh: &mut Shell, expr: &CondExpr) -> R<()> {
    match eval_cond(sh, expr)? {
        Ok(true) => sh.set_status(0),
        Ok(false) => sh.set_status(1),
        Err(CondError { msg, status }) => {
            // `write_stderr` supplies the newline.
            if let Some(msg) = msg {
                let _ = diag(sh, &format!("[[: {msg}"));
            }
            sh.set_status(status);
        }
    }
    Ok(())
}

/// A reported expression failure: what to say, and what status to leave. The
/// STATUS varies because bash's does -- a malformed ARITHMETIC operand is
/// diagnosed and then treated as FALSE (1), where a malformed expression is 2,
/// the status `test` uses for one.
///
/// The MESSAGE is `None` where the failure was already reported by whoever
/// raised it -- an arithmetic operand refused as read only writes its own
/// diagnostic and unwinds, and saying it again under a `[[: ` prefix is the
/// same words twice.
struct CondError {
    msg: Option<String>,
    status: i32,
}

impl CondError {
    /// A malformed EXPRESSION: status 2, the one `test` reports for one.
    fn bad(msg: String) -> CondError {
        CondError { msg: Some(msg), status: 2 }
    }
}

/// A failed arithmetic OPERAND: diagnosed, then FALSE, which is bash's answer
/// and the reason `CondError` carries a status rather than assuming 2.
///
/// The abort is the readonly refusal, whose words `set_var` already wrote. Any
/// other `Sig` leaves the construct instead of becoming a false result -- the
/// outer channel's whole purpose -- and none is reachable from `set_var`
/// today, which is why this is written as a rule rather than a match on one.
fn operand_error(stop: arith::Stop) -> R<CondError> {
    let msg = match stop {
        arith::Stop::Msg(msg) => Some(msg),
        arith::Stop::Reported(Sig::Abort(_)) => None,
        arith::Stop::Reported(sig) => return Err(sig),
    };
    Ok(CondError { msg, status: 1 })
}

/// Two error channels, and they are not interchangeable. The OUTER `R` carries
/// a `Sig` -- expansion runs inside `[[ ]]`, so `set -u` on an unset operand or
/// an `exit` from a command substitution has to leave the whole construct, not
/// become a false result. The INNER `Result` is the expression being malformed,
/// which is status 2 and a diagnostic.
///
/// `&&` and `||` SHORT-CIRCUIT, which is not merely an optimisation: the right
/// side of `[[ -n $f && -r $f ]]` is written on the assumption the left one
/// held, and evaluating it anyway can error where bash reports false.
fn eval_cond(sh: &mut Shell, expr: &CondExpr) -> R<Result<bool, CondError>> {
    Ok(match expr {
        CondExpr::Word(w) => Ok(!expand::expand_single(sh, w)?.is_empty()),
        CondExpr::Not(inner) => match eval_cond(sh, inner)? {
            Ok(v) => Ok(!v),
            e => e,
        },
        CondExpr::And(l, r) => match eval_cond(sh, l)? {
            Ok(true) => eval_cond(sh, r)?,
            other => other,
        },
        CondExpr::Or(l, r) => match eval_cond(sh, l)? {
            Ok(false) => eval_cond(sh, r)?,
            other => other,
        },
        CondExpr::Unary { op, arg } => {
            let arg = expand::expand_single(sh, arg)?;
            // Three operators `test` does not serve, for three different
            // reasons. `-v` (is this NAME set) has no `test` spelling at all.
            // `-o` (is this shell option on) is the reader for what `set -o`
            // writes. And `-a` is file-exists HERE while in `test` it is the
            // binary AND operator -- inside `[[ ]]` the connective is `&&`, so
            // the letter is free and bash gives it to `-e`'s meaning.
            match op.as_str() {
                "-v" => Ok(cond_is_set(sh, &arg)),
                "-o" => Ok(builtin::named_option_is_set(sh, &arg)),
                "-a" => builtin::unary_op(sh, "e", &arg).map_err(CondError::bad),
                _ => builtin::unary_op(sh, op.strip_prefix('-').unwrap_or(op), &arg)
                    .map_err(CondError::bad),
            }
        }
        CondExpr::Binary { op, lhs, rhs } => {
            let left = expand::expand_single(sh, lhs)?;
            match op {
                // The right side is a PATTERN, matched exactly as `case` does:
                // an unquoted `*` matches anything, a quoted one matches itself.
                // That per-character distinction is why the RHS is still a Word
                // here rather than a string -- flattening it would lose it.
                CondOp::Match | CondOp::NoMatch => {
                    let chars = expand::expand_pattern(sh, rhs)?;
                    let units = pattern::compile(&chars);
                    let hit = pattern::matches(&units, &left);
                    Ok(if matches!(op, CondOp::Match) { hit } else { !hit })
                }
                // A SEARCH, not a whole-string match, so `[[ abc =~ b ]]` holds.
                // A regex that does not compile is FATAL rather than false:
                // that is the corpus's graded answer (bash's carry-on is
                // recorded there as the BUG), and it is the right one -- a
                // malformed regex is a mistake in the script, like a syntax
                // error, not a condition to branch on. The distinction matters
                // because the alternative silently answers "no match" for
                // every subject, which is indistinguishable from a real miss.
                CondOp::Regex => {
                    let chars = expand::expand_pattern(sh, rhs)?;
                    match crate::regex::compile(&chars) {
                        Ok(re) => Ok(re.is_match(&left)),
                        Err(msg) => return Err(sh.fatal(&format!("[[: {msg}"), 2)),
                    }
                }
                CondOp::Before => Ok(left < expand::expand_single(sh, rhs)?),
                CondOp::After => Ok(left > expand::expand_single(sh, rhs)?),
                // ARITHMETIC on both sides, not integer parsing: `[[ 1+1 -eq 2 ]]`
                // holds, and a bare name is its value (`x=5; [[ x -eq 5 ]]`),
                // where `test x -eq 5` is an error. Measured against bash rather
                // than assumed -- reusing `test`'s comparison here was wrong for
                // every one of those.
                CondOp::Arith(cmp) => {
                    let right = expand::expand_single(sh, rhs)?;
                    // The operands are evaluated in ORDER and the second is not
                    // reached once the first has failed, which is bash's answer
                    // and not merely tidier: an operand ASSIGNS, so evaluating
                    // it anyway leaves a name set that bash never sets.
                    let a = match cond_arith(sh, &left) {
                        Ok(a) => a,
                        Err(stop) => return Ok(Err(operand_error(stop)?)),
                    };
                    let b = match cond_arith(sh, &right) {
                        Ok(b) => b,
                        Err(stop) => return Ok(Err(operand_error(stop)?)),
                    };
                    Ok(match cmp {
                        ArithCmp::Eq => a == b,
                        ArithCmp::Ne => a != b,
                        ArithCmp::Lt => a < b,
                        ArithCmp::Le => a <= b,
                        ArithCmp::Gt => a > b,
                        ArithCmp::Ge => a >= b,
                    })
                }
                CondOp::File(spelling) => {
                    let right = expand::expand_single(sh, rhs)?;
                    builtin::binary_op(sh, &left, spelling, &right).map_err(CondError::bad)
                }
            }
        }
    })
}

/// `-v NAME`. A positional is set when the list is long enough, which
/// `get_var` cannot answer: positionals live in `params`, not in the variable
/// table, so `set -- a; [[ -v 1 ]]` reported UNSET while `$1` expanded to `a`.
/// A digit is the only special parameter bash's `-v` accepts.
fn cond_is_set(sh: &Shell, name: &str) -> bool {
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
        return match name.parse::<usize>() {
            // `$0` is the shell's own name and is always set.
            Ok(0) => true,
            Ok(n) => sh.params.len() >= n,
            Err(_) => false,
        };
    }
    sh.get_var(name).is_some()
}

/// One side of an `-eq`-family comparison. A BLANK operand is zero, which is
/// bash's answer for `[[ "" -eq 0 ]]` and the reason this is not a bare
/// `arith::eval`: an unset variable is the commonest way to reach here, and
/// erroring would turn a false comparison into a diagnostic. `arith` answers a
/// null expression with 0 itself now, so what is left here is the operand
/// `trim` calls blank and arithmetic does not -- a Unicode space.
/// Reported rather than FATAL, which is the difference between this and
/// `$(( ))`: bash answers `[[ 1+ -eq 2 ]]` with a diagnostic and a false result
/// and carries on, where `arith::eval` would end a non-interactive shell at the
/// first bad expression -- and an operand here is usually a variable somebody
/// else set.
fn cond_arith(sh: &mut Shell, text: &str) -> Result<i64, arith::Stop> {
    if text.trim().is_empty() {
        return Ok(0);
    }
    arith::try_eval(sh, text)
}

fn run_case(sh: &mut Shell, word: &Word, items: &[crate::ast::CaseItem]) -> R<()> {
    let subject = expand::expand_single(sh, word)?;
    for item in items {
        for pat in &item.patterns {
            let chars = expand::expand_pattern(sh, pat)?;
            let units = pattern::compile(&chars);
            if pattern::matches(&units, &subject) {
                return run_list(sh, &item.body);
            }
        }
    }
    sh.set_status(0);
    Ok(())
}

/// `set -x`: report the command on stderr, prefixed by the EXPANDED `$PS4`.
/// `errout` overrides the destination with what fd 2 held before this command's
/// redirections ran -- `Some(None)` for "it held nothing", which fails the write
/// rather than falling back to the redirected fd. `None` writes to fd 2 as it stands.
fn trace(sh: &mut Shell, parts: &[String], errout: Option<Option<&process::Fd>>) {
    // dash's `inps4` guard: a `$(...)` inside PS4 must not trace its own
    // commands, which would re-enter here forever.
    if !sh.opts.xtrace || sh.in_ps4 {
        return;
    }
    // An UNSET PS4 prefixes nothing. Both references reach the value as
    // `vps4.text + 4` -- past the "PS4=" of a variable they never let go of --
    // so unsetting it leaves an empty string rather than restoring `+ `.
    let ps4 = sh.get_var("PS4").unwrap_or_default();
    // Tracing only observes. A `$(...)` in $PS4 runs a real command, but it must not
    // become the traced command's own `$?`, nor be counted among ITS substitutions --
    // that count is what decides an assignment-only command's status, so leaking one
    // makes `PS4='$(false) '; x=1` report 1 where POSIX says 0.
    let saved_status = sh.status;
    let saved_cmdsubst = sh.cmdsubst_count;
    sh.in_ps4 = true;
    let expanded = (|| {
        // `fatal` here is only a way to get the diagnostic onto stderr: the `Sig` it
        // builds is dropped with the rest of the error below, so the status is inert.
        let word = crate::lexer::word_from_str(&ps4).map_err(|e| sh.fatal(&e, 2))?;
        expand::expand_single(sh, &word)
    })();
    sh.in_ps4 = false;
    sh.status = saved_status;
    sh.cmdsubst_count = saved_cmdsubst;
    // A PS4 that will not expand -- an unterminated `${`, `$(( 1 / 0 ))` -- does
    // NOT stop the shell. Both references set a handler around this one
    // expansion and fall back to the unexpanded string ("readtoken1() might die
    // horribly", as busybox puts it), so the diagnostic is already on stderr and
    // the trace goes out with the raw value.
    let prefix = expanded.unwrap_or(ps4);
    let line = format!("{prefix}{}\n", parts.join(" "));
    let _ = note_epipe(
        sh,
        match errout {
            Some(target) => process::write_target(target, line.as_bytes()),
            None => process::write_fd(sh, 2, line.as_bytes()),
        },
    );
}

fn run_simple(
    sh: &mut Shell,
    assigns: &[crate::ast::Assign],
    words: &[Word],
    redirs: &[Redir],
) -> R<()> {
    let cmdsubst_before = sh.cmdsubst_count;
    let argv = expand::expand_command_words(sh, words)?;
    // No command name — either none was given (`a=1 b=2`) or every word field-split
    // away (`x=new $empty`). POSIX: the assignments affect the CURRENT shell,
    // redirections are performed then dropped (`>file` truncates), and the exit
    // status is the last command substitution's, or 0 if this command performed
    // none. `cmdsubst_before` captures whether any substitution ran (in the words
    // above or the assignments below) so an unrelated prior `$?` is not carried in.
    if argv.is_empty() {
        let saved = match process::apply_redirs(sh, redirs)? {
            process::RedirOutcome::Applied(s) => s,
            // A failed redirection skips the command; the assignments do not run.
            process::RedirOutcome::Failed => return Ok(()),
        };
        let result = (|| {
            // dash traces the assignments as its `varlist`, with the values
            // already expanded, and does it AFTER applying them.
            let mut traced = Vec::with_capacity(assigns.len());
            for a in assigns {
                let value = expand::expand_assign(sh, &a.value)?;
                sh.set_var(&a.name, &value)?;
                traced.push(format!("{}={}", a.name, value));
            }
            // Through the stderr this command redirected AWAY from: `x=1 2>/dev/null`
            // still traces, because dash traces to `preverrout` and not to fd 2.
            trace(sh, &traced, saved.prev_stderr());
            if sh.cmdsubst_count == cmdsubst_before {
                sh.set_status(0);
            }
            Ok(())
        })();
        process::restore_redirs(sh, saved);
        return result;
    }

    // ash pushes a local-var frame in `evalcommand` for every command that HAS a
    // command word, unless that word is a special builtin -- and it is always the
    // FIRST word, since `spclbltin` is locked to it. That single rule is why
    // `command eval 'local x=1'` works while `eval 'local x=1'` does not, and why
    // the frame is already standing when a prefix assignment or a redirection word
    // dies. The words above expanded first, so `true ${u:?e}` dies without one.
    // Above `trace` because ash pushes before its xtrace block, and td-sh expands a
    // `$( )` in PS4 for real.
    let saved_depth = sh.localvar_depth;
    let framed = argv.first().is_some_and(|w| {
        // Resolved the way `dispatch_simple` resolves it below: a function shadowing
        // a special builtin's NAME is still a function, and ash gives it a frame.
        sh.funcs.contains_key(w) || !builtin::is_ash_special_word(w)
    });
    if framed {
        sh.localvar_depth = sh.localvar_depth.saturating_add(1);
    }
    trace(sh, &argv, None);
    let result = dispatch_simple(sh, &argv, assigns, redirs);
    // Same rule as the frame's bindings: a terminating unwind leaves it standing so
    // an EXIT trap can still declare a `local`, and the recovery that drains those
    // bindings puts the depth back along with them. Pushed after the callee's own
    // deferrals, so a forward drain applies the OUTERMOST depth last.
    if framed {
        if result.as_ref().err().is_some_and(terminating) {
            sh.pending_unwind.push(Local::Depth(saved_depth));
        } else {
            sh.localvar_depth = saved_depth;
        }
    }
    result
}

/// The three kinds of command word, once the frame around them is decided.
fn dispatch_simple(
    sh: &mut Shell,
    argv: &[String],
    assigns: &[crate::ast::Assign],
    redirs: &[Redir],
) -> R<()> {
    // A function call runs in the current shell with the assignments applied for
    // its duration and the words as its positional parameters.
    if let Some(func) = argv.first().and_then(|name| sh.funcs.get(name)).cloned() {
        return call_function(sh, &func, argv, assigns, redirs);
    }

    if let Some(bi) = builtin::lookup(argv.first().map(String::as_str).unwrap_or("")) {
        return run_builtin(sh, bi, argv, assigns, redirs);
    }

    // External command: the assignments are transient, but they are SET on the shell
    // rather than merely handed to the child. ash puts them in a localvar frame with
    // `VEXPORT` (ash.c:10497) and locates the command only afterwards, so
    // `PATH=dir prog` finds `prog` in `dir` -- and the child inherits them because
    // they are exported, not through a separate list.
    let pending_mark = sh.pending_unwind.len();
    let mut saved_vars: Vec<(String, Option<Var>)> = Vec::with_capacity(assigns.len());
    // Redirections FIRST, then the assignments -- ash's order (`redirectsafe` at
    // ash.c:10477, the assignment loop at 10490). It is observable both ways: a
    // target naming one of these names expands to the value it had BEFORE the
    // command, and a target that fails to expand leaves the old value for the EXIT
    // trap to see. `?` here is that second case, and it is correct that it skips
    // the rollback below: nothing was assigned yet.
    let result = match process::apply_redirs(sh, redirs)? {
        // A failed redirection skips the command without exiting the shell.
        process::RedirOutcome::Failed => Ok(()),
        process::RedirOutcome::Applied(saved) => {
            // Closed over so an unwind part-way through -- a readonly target --
            // still reaches the rollback below.
            let r = (|| {
                for a in assigns {
                    let value = expand::expand_assign(sh, &a.value)?;
                    saved_vars.push((a.name.clone(), sh.vars.get(&a.name).cloned()));
                    sh.set_var(&a.name, &value)?;
                    sh.export(&a.name);
                }
                process::exec_external(sh, argv, None, None)
            })();
            process::restore_redirs(sh, saved);
            r
        }
    };
    // Same rule as the regular-builtin frame above: a terminating unwind leaves it
    // standing for an EXIT trap, anything else takes it off here. The mark is
    // defensive rather than load-bearing -- nothing an external command reaches can
    // defer onto this shell's list, since every nested evaluation here forks.
    if result.as_ref().err().is_some_and(terminating) {
        defer_vars(sh, saved_vars);
    } else {
        unwind_pending_to(sh, pending_mark);
        restore_vars(sh, saved_vars);
    }
    result
}

fn run_builtin(
    sh: &mut Shell,
    bi: builtin::Builtin,
    argv: &[String],
    assigns: &[crate::ast::Assign],
    redirs: &[Redir],
) -> R<()> {
    if builtin::is_special(bi) {
        // POSIX special builtins (`:`, `.`, `eval`, `export`, `set`, `shift`, …):
        // prefix assignments persist in the current shell. Redirections precede the
        // assignments (POSIX 2.9.1 order), so a failed redirection skips both.
        let saved = match process::apply_redirs(sh, redirs)? {
            process::RedirOutcome::Applied(s) => s,
            // A redirection error on a special builtin is POSIX-fatal, so it takes
            // the same route every other `sh_error` here does. `$?` is already 1.
            process::RedirOutcome::Failed => return Err(Sig::Abort(sh.status)),
        };
        let result = (|| {
            for a in assigns {
                let value = expand::expand_assign(sh, &a.value)?;
                sh.set_var(&a.name, &value)?;
                // `exec`'s prefix bindings go to the replacement process, so they
                // are exported as well as set (dash's listsetvar VEXPORT).
                if matches!(bi, builtin::Builtin::Exec) {
                    sh.export(&a.name);
                }
            }
            builtin::run(sh, bi, argv)
        })();
        // Bare `exec` is the one builtin whose redirections are the POINT: they
        // stay in force for the rest of the shell instead of being unwound here.
        // With a command word they belong to the replacement process, so a FAILED
        // `exec` (which only returns in an interactive shell) must still unwind.
        if matches!(bi, builtin::Builtin::Exec) && builtin::exec_keeps_redirections(argv) {
            return result;
        }
        process::restore_redirs(sh, saved);
        return result;
    }
    // Regular builtins (`echo`, `read`, `test`, `cd`, …): a prefix assignment is
    // transient — visible only for the builtin's own run, like an external
    // command's environment — so save and restore each affected variable. It is also
    // exported for that run so a builtin that itself execs an external utility
    // (`FOO=bar command extcmd`) passes it through; the saved prior `Var` carries the
    // original export flag, which the restore below puts back.
    // `local` is the exception: dash groups it with the declaration utilities, whose
    // prefix assignment persists (`export`/`readonly` are special builtins and took
    // the branch above). Rolling it back here would also undo what `local` itself
    // just assigned to the same name, leaving `x=t local x=l` with neither value.
    let transient = !matches!(bi, builtin::Builtin::Local);
    // Anything deferred from here on belongs to THIS command, so a swallowed
    // abort undoes exactly that and leaves an outer unwind's frames alone.
    let pending_mark = sh.pending_unwind.len();
    let mut saved_vars: Vec<(String, Option<Var>)> = Vec::with_capacity(assigns.len());
    // Redirections FIRST, then the assignments -- ash's order (`redirectsafe` at
    // ash.c:10477, the assignment loop at 10490), and already what the special
    // branch above does. It is observable both ways: a target that NAMES one of
    // these expands to the value it had before the command, and a target that
    // fails to expand leaves the old value for an EXIT trap to see. The `?` on
    // `apply_redirs` is that second case, and it correctly skips the rollback:
    // nothing has been assigned yet.
    let result = match process::apply_redirs(sh, redirs)? {
        // `local` is in ash's `spclbltin` set even though POSIX's special list
        // omits it, and that set -- not the POSIX one -- is what makes a
        // redirection error fatal (ash.c:10484). It reaches this branch rather
        // than the one above because td-sh keys the PERSISTENT-assignment split
        // on the POSIX list, where `local` genuinely differs.
        process::RedirOutcome::Failed if builtin::is_ash_special(bi) => {
            return Err(Sig::Abort(sh.status));
        }
        // A failed redirection skips a regular builtin; `$?` is already 1.
        process::RedirOutcome::Failed => Ok(()),
        process::RedirOutcome::Applied(saved) => {
            // Closed over so an unwind mid-way -- a readonly target -- still reaches
            // the rollback below; a `?` here used to skip it and leak the binding
            // into the next command, which only became visible once the shell
            // survived.
            let r = (|| {
                for a in assigns {
                    let value = expand::expand_assign(sh, &a.value)?;
                    if transient {
                        saved_vars.push((a.name.clone(), sh.vars.get(&a.name).cloned()));
                    }
                    sh.set_var(&a.name, &value)?;
                    if transient {
                        sh.export(&a.name);
                    }
                }
                Ok(())
            })()
            .and_then(|()| {
                // dash re-raises EXERROR from `evalbltin` only for a SPECIAL
                // builtin, and only for an error raised in the builtin's own BODY
                // -- an error from the assignments or redirect words is
                // `evalcommand`'s and stays fatal. `Exit` propagates regardless, so
                // `command exit 7` still exits.
                match builtin::run(sh, bi, argv) {
                    Err(Sig::Abort(code)) if swallows_abort(bi) => {
                        unwind_pending_to(sh, pending_mark);
                        sh.set_status(code);
                        Ok(())
                    }
                    other => other,
                }
            });
            process::restore_redirs(sh, saved);
            r
        }
    };
    // Same rule as a function's frame: a terminating unwind leaves it standing for
    // an EXIT trap. A failed redirection is NOT one -- it returns `Ok`, and both
    // references show the frame already gone by the time the trap runs.
    if result.as_ref().err().is_some_and(terminating) {
        defer_vars(sh, saved_vars);
    } else {
        // As in `command`'s scratch frame, and load-bearing for the same reason: a
        // builtin run from an EXIT trap has the dying frame deferred beneath it, so
        // the mark is what stops this from taking that frame off too. Anything
        // deferred inside is newer than this frame and must come off before it, or
        // this frame's saved values are stale.
        unwind_pending_to(sh, pending_mark);
        restore_vars(sh, saved_vars);
    }
    result
}

/// Whether an `Abort` raised inside this builtin's body stops at the command
/// boundary. ash wraps EVERY builtin in its own handler and re-raises only for a
/// SPECIAL one (ash.c:10619), so a regular builtin's error ends the COMMAND and
/// leaves its status standing. `command` was named here alone, but it is an
/// instance of that rule rather than an exception to it: `spclbltin` is locked to
/// the FIRST word (ash.c:10418), which is why it strips fatality from what
/// follows.
fn swallows_abort(bi: builtin::Builtin) -> bool {
    !builtin::is_ash_special(bi)
}

/// Whether this signal is on its way out of the shell rather than out of a
/// construct. Only these three skip a frame's cleanup; `return` and `break`
/// unwind normally and undo it. An interrupt belongs with the other two rather
/// than with those: the EXIT trap is owed the dying function's `local`s, and a
/// trap that says `local` at all needs the frame still standing to say it in.
pub fn terminating(sig: &Sig) -> bool {
    matches!(sig, Sig::Exit(_) | Sig::Abort(_) | Sig::Interrupt(_))
}


/// Put back what one binding displaced. `None` means the name did not exist, so
/// restoring it is an unset -- and it overwrites whatever the body assigned
/// through the binding, which is what makes a temp frame vanish entirely. The
/// write goes into the map rather than through `set_var`: a name made `readonly`
/// inside the frame must not block the restore of what preceded it.
fn undo_binding(sh: &mut Shell, entry: Local) {
    match entry {
        Local::Var(name, Some(var)) => {
            // Bypassing `set_var` still owes the hook, which dash runs on the
            // RESTORE too (`poplocalvars` calls the var's `func`): a frame that
            // displaced OPTIND has to put the hidden cursor back with it, or
            // `getopts` resumes at the word the frame's value pointed at.
            // The entry goes back BEFORE the hook runs, since the hook reads the
            // restored flags. A DYNAMIC name is re-run with the saved TEXT rather
            // than treated as an unset -- for a valueless entry that text is
            // empty, which reseeds with `strtoul("")`, not the generator's death.
            let restored = var.value.clone();
            let dynamic = var.dynamic.is_some();
            sh.vars.insert(name.clone(), var);
            match restored.as_deref() {
                Some(v) => sh.var_hook(&name, v),
                None if dynamic => sh.var_hook(&name, ""),
                None => sh.unset_hook(&name),
            }
        }
        Local::Var(name, None) => {
            sh.unset_hook(&name);
            // ash's restore is `unsetvar` = `setvar(s, NULL, 0)` (ash.c:2525),
            // whose `setvareq` ORs `VEXPORT` in under `set -a` (ash.c:2417).
            // A name ash SEEDS is restored instead, so it never reaches that.
            if sh.opts.allexport && !ash_seeds_entry(&name) {
                sh.vars.insert(
                    name,
                    Var {
                        value: None,
                        exported: true,
                        readonly: false,
                        localised: false,
                        dynamic: None,
                    },
                );
            } else {
                sh.vars.remove(&name);
            }
        }
        Local::Opts(opts) => sh.opts = opts,
        Local::Depth(was) => sh.localvar_depth = was,
    }
}

/// Names ash's `varinit_data` (ash.c:2154-2181) keeps a permanent vartab entry
/// for, as THIS build configures it. `findvar` finds one, so `mklocal` saves it
/// rather than recording an unset (10011) and `exportcmd` flag-edits it (14158).
fn ash_seeds_entry(name: &str) -> bool {
    matches!(
        name,
        "IFS"
            | "MAIL"
            | "MAILPATH"
            | "PATH"
            | "PS1"
            | "PS2"
            | "PS4"
            | "OPTIND"
            | "LINENO"
            | "FUNCNAME"
            | "RANDOM"
            | "EPOCHSECONDS"
            | "EPOCHREALTIME"
            | "HISTFILE"
    )
}

/// Undo a frame's `local`s, newest first -- a function's on return, or the
/// scratch one `command` runs a builtin in.
pub fn pop_locals(sh: &mut Shell) {
    while let Some(entry) = sh.locals.pop() {
        undo_binding(sh, entry);
    }
}

/// Undo a temp frame, newest first.
fn restore_vars(sh: &mut Shell, saved: Vec<(String, Option<Var>)>) {
    for (name, prev) in saved.into_iter().rev() {
        undo_binding(sh, Local::Var(name, prev));
    }
}

/// Hand a temp frame to `sh.pending_unwind` instead of undoing it, newest first.
fn defer_vars(sh: &mut Shell, saved: Vec<(String, Option<Var>)>) {
    sh.pending_unwind
        .extend(saved.into_iter().rev().map(|(n, v)| Local::Var(n, v)));
}

/// The same for this scope's `local` frame.
pub fn defer_locals(sh: &mut Shell) {
    let mine = std::mem::take(&mut sh.locals);
    sh.pending_unwind.extend(mine.into_iter().rev());
}

/// dash's `exitreset` -> `unwindlocalvars(NULL)`, bounded to what was deferred
/// after `mark` was taken: bindings a terminating unwind left standing go away
/// once the shell turns out to be recovering rather than exiting, but only the
/// ones belonging to the command that recovered. Frames deferred BEFORE the mark
/// belong to an outer unwind that is still on its way out, and undoing those
/// would take a dying function's bindings away from the EXIT trap.
///
/// It must also run before the marking scope undoes its OWN frame, since what it
/// puts back is what that frame would otherwise have to save again.
pub fn unwind_pending_to(sh: &mut Shell, mark: usize) {
    if mark >= sh.pending_unwind.len() {
        return;
    }
    // Deferred newest first within one unwind, so this drains in that order.
    for entry in sh.pending_unwind.split_off(mark) {
        undo_binding(sh, entry);
    }
}

fn call_function(
    sh: &mut Shell,
    func: &Func,
    argv: &[String],
    assigns: &[crate::ast::Assign],
    redirs: &[Redir],
) -> R<()> {
    // Recursion is bounded centrally in `run_command` (the function body re-enters
    // there), so `f() { f; }; f` errors instead of overflowing the stack.
    //
    // A prefix assignment is a TEMP FRAME: visible inside the call, exported for
    // it as an external command's environment would be, and gone afterwards --
    // even if the body assigned through it. It goes into the frame's OWN list, as
    // ash's `evalcommand` does it (`mklocal` at ash.c:10497 into the frame pushed
    // at 10446), so a `local` of the same name in the body sees it already
    // declared and leaves it alone. Being the oldest entries, they unwind last.
    let saved_locals = std::mem::take(&mut sh.locals);
    // The callee's frame starts empty in BOTH halves: what a dying outer frame
    // deferred is not something this call has declared.
    let saved_floor = std::mem::replace(&mut sh.pending_floor, sh.pending_unwind.len());
    let applied = (|| -> R<()> {
        for a in assigns {
            let value = expand::expand_assign(sh, &a.value)?;
            sh.locals
                .push(Local::Var(a.name.clone(), sh.vars.get(&a.name).cloned()));
            sh.set_var(&a.name, &value)?;
            sh.export(&a.name);
        }
        Ok(())
    })();
    if let Err(sig) = applied {
        if terminating(&sig) {
            defer_locals(sh);
        } else {
            pop_locals(sh);
        }
        sh.locals = saved_locals;
        sh.pending_floor = saved_floor;
        return Err(sig);
    }
    let new_params = argv.get(1..).unwrap_or(&[]).to_vec();
    let saved_params = std::mem::replace(&mut sh.params, new_params);
    // The cursor belongs to the argument frame, so the function scans its own
    // arguments from the start and the caller resumes where it left off. The
    // OPTIND variable is global and deliberately NOT restored.
    let saved_getopts = (sh.getopts_optind, sh.getopts_off);
    sh.getopts_optind = 1;
    sh.getopts_off = -1;
    let saved_loop_depth = sh.loop_depth;
    sh.loop_depth = 0;
    // dash saves and restores `funcline` around the call (eval.c:986/1007) and
    // NOT `lineno`, so a nested call reports relative to its OWN definition and
    // the caller's next command sets the line again on its way past. The body's
    // own line is published AFTER, so the subtraction is already in effect.
    let saved_funcline = std::mem::replace(&mut sh.funcline, func.line);
    sh.set_lineno(func.body.line);
    // Not `?`: a fatal error in a redirection WORD (`f 2>${u:?}`) must still unwind
    // the argument frame below, or the caller -- or an EXIT trap -- sees the
    // function's `$1`/`$#`.
    let result = match process::apply_redirs(sh, redirs) {
        Ok(process::RedirOutcome::Applied(saved)) => {
            let r = run_command(sh, &func.body.cmd);
            process::restore_redirs(sh, saved);
            r
        }
        // A failed redirection skips the function body; `$?` is already 1.
        Ok(process::RedirOutcome::Failed) => Ok(()),
        Err(sig) => Err(sig),
    };
    // The argument frame is restored either way -- both references show `$#` back
    // to the caller's inside an EXIT trap. The BINDINGS are not: a terminating
    // unwind longjmps past dash's `poplocalvars`/`unwindlocalvars`, so the trap
    // still sees the `local` and the temp binding of the function it died in.
    (sh.getopts_optind, sh.getopts_off) = saved_getopts;
    sh.funcline = saved_funcline;
    if result.as_ref().err().is_some_and(terminating) {
        defer_locals(sh);
    } else {
        pop_locals(sh);
    }
    sh.locals = saved_locals;
    sh.pending_floor = saved_floor;
    sh.params = saved_params;
    sh.loop_depth = saved_loop_depth;
    match result {
        Err(Sig::Return(code)) => {
            sh.set_status(code);
            Ok(())
        }
        other => other,
    }
}

/// Run `body` with `redirs` applied, restoring the descriptors afterward even if
/// the body unwinds.
pub fn with_redirs<F>(sh: &mut Shell, redirs: &[Redir], body: F) -> R<()>
where
    F: FnOnce(&mut Shell) -> R<()>,
{
    if redirs.is_empty() {
        return body(sh);
    }
    let saved = match process::apply_redirs(sh, redirs)? {
        process::RedirOutcome::Applied(s) => s,
        // A failed redirection skips the compound command; `$?` is already 1.
        // Whether that trips `set -e` is not decided here but by the NODE the
        // status is reported from -- see `checks_errexit`.
        process::RedirOutcome::Failed => return Ok(()),
    };
    let result = body(sh);
    process::restore_redirs(sh, saved);
    result
}

/// `$(...)` / `` `...` ``: run the code with stdout captured, strip trailing
/// newlines, and return the text. Runs in a subshell so its state changes do not
/// leak, matching POSIX.
pub fn command_subst(sh: &mut Shell, code: &str, line: u32) -> R<String> {
    // Nesting is bounded centrally in `run_command` (the substituted body re-enters
    // there), so `$( $( … ) )` errors instead of overflowing the stack.
    // Counted so an assignment-only command can adopt the last substitution's $?.
    sh.cmdsubst_count = sh.cmdsubst_count.wrapping_add(1);
    let mut out = process::capture_stdout(sh, code, line)?;
    while out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

/// The word after a here-document body has been assembled into a `Word`; expand
/// it to the text fed to the command.
pub fn here_body(sh: &mut Shell, body: &Word) -> R<String> {
    expand::expand_single(sh, body)
}

/// Text of a redirection target word (single-word expansion, no split/glob).
pub fn redir_target(sh: &mut Shell, r: &Redir) -> R<String> {
    expand::expand_single(sh, &r.word)
}

/// The errnos td-sh names itself, the failure being one it DECIDED rather than
/// one a syscall reported. Linux numbering, as every other constant here is.
pub const ENOENT: i32 = 2;
pub const EACCES: i32 = 13;
pub const EEXIST: i32 = 17;
pub const ENOTDIR: i32 = 20;
pub const ENAMETOOLONG: i32 = 36;
pub const ELOOP: i32 = 40;

/// What a shell prints for an errno: the system's own text, without the
/// ` (os error N)` `io::Error`'s Display appends and no shell prints.
///
/// Removed as the exact suffix rather than searched for, so a message std did
/// not compose is never truncated at a marker it happens to contain; one with
/// no errno has no suffix and passes through whole.
pub fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    let Some(n) = e.raw_os_error() else { return text };
    let suffix = format!(" (os error {n})");
    text.strip_suffix(&suffix).unwrap_or(&text).to_owned()
}

/// Write `msg` plus a newline to the shell's current stderr.
pub fn write_stderr(sh: &Shell, msg: &str) -> std::io::Result<()> {
    note_epipe(sh, process::write_fd(sh, 2, format!("{msg}\n").as_bytes()))
}

/// Report a diagnostic: the shell's own name, the command component and the
/// line if there is one, then the message.
///
/// ash writes that name before every diagnostic it issues (`ash_vmsg`,
/// ash.c:1419) and it is `$0` rather than a constant (ash.c:423), so a
/// script's failures are attributed to the script and `-c CMD NAME` to NAME.
/// Naming it in ONE place is the point: a prefix repeated at each call site
/// is one a site can forget, which is how most of them came to lack it.
///
/// The other two parts come from shell STATE for the same reason, and are the
/// whole of `ash_vmsg`'s remaining body: a component that repeats `$0` is
/// dropped, and both it and the line are gated on `commandname`, so a `-c`
/// command that never enters a builtin reports neither.
pub fn diag(sh: &Shell, msg: &str) -> std::io::Result<()> {
    let mut out = format!("{}: ", sh.arg0);
    if let Some(cmd) = sh.commandname.as_deref() {
        if cmd != sh.arg0 {
            out.push_str(cmd);
            out.push_str(": ");
        }
        if !sh.interactive || sh.input_is_file {
            out.push_str("line ");
            out.push_str(&sh.lineno.to_string());
            out.push_str(": ");
        }
    }
    out.push_str(msg);
    write_stderr(sh, &out)
}

/// The libbb class: `$0` and the message, with neither component nor line.
///
/// `test`/`[`, `printf` and `read`'s bad variable name do not reach `ash_vmsg`
/// at all -- they are busybox APPLET diagnostics through `bb_error_msg`, which
/// knows nothing of `commandname` or the parser's line. Measured in a script,
/// where `ash_vmsg` certainly would print one, ash answers `[ x` with a bare
/// `missing ]` and `read 1bad` with `read: '1bad': bad variable name`: the
/// name each carries is in the MESSAGE, not composed for it.
///
/// So this is the shape `diag` had before the line arrived, kept for exactly
/// those messages. What still differs is the name in front, which is the
/// applet's for ash and `$0` here -- the class a47199ef measured and deferred,
/// and unchanged by this.
pub fn diag_applet(sh: &Shell, msg: &str) -> std::io::Result<()> {
    write_stderr(sh, &format!("{}: {msg}", sh.arg0))
}

/// Record a broken pipe on the shell's diagnostic descriptor, passing the result
/// through untouched.
///
/// `BrokenPipe` and nothing else — not "any failed write". A closed descriptor
/// (`cd /nope 2>&-`) fails too, and the operator closed it on purpose; bash
/// carries on and so does this. That one is not the kernel's EBADF either:
/// `write_target` answers `Fd::Closed` itself, so nothing reaches a syscall,
/// and the same `Other` covers a read-only target and a poisoned mutex. Which
/// is the argument for naming the kind that ENDS the shell rather than
/// enumerating the kinds that do not.
pub fn note_epipe(sh: &Shell, r: std::io::Result<()>) -> std::io::Result<()> {
    if r.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
        sh.stderr_epipe.store(true, Ordering::Relaxed);
    }
    r
}

#[cfg(test)]
mod tests {
    fn run(src: &str) -> (i32, String, String) {
        crate::process::run_capturing(src)
    }

    /// The two rules a `trim()` would break. An EMPTY field is a value, not a
    /// failure to read one -- ash's `uname(2)` cannot fail, so a host with no
    /// hostname gets `HOSTNAME` set-and-empty, and collapsing empty into `None`
    /// would leave it UNSET and hand `set -u` back the abort this seeding
    /// removes. And only the terminating newline is procfs framing, so a
    /// hostname's own spaces survive. Driven through files rather than a UTS
    /// namespace so both are pinned without depending on the host's hostname.
    #[test]
    fn a_proc_field_distinguishes_empty_from_unreadable() {
        let dir = std::env::temp_dir().join(format!("td-sh-procfield-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p.to_string_lossy().into_owned()
        };
        let field = |p: &str, k: Option<&str>| super::proc_field(p, k);

        assert_eq!(field(&write("full", "t5700g\n"), None).as_deref(), Some("t5700g"));
        assert_eq!(field(&write("empty", ""), None).as_deref(), Some(""));
        assert_eq!(field(&write("blank", "\n"), None).as_deref(), Some(""));
        // Only the terminating newline is framing. A hostname may hold spaces,
        // and ash's `uname(2)` hands them over verbatim, so `trim()` would
        // silently rename the host.
        assert_eq!(field(&write("spaced", " x \n"), None).as_deref(), Some(" x "));
        assert_eq!(field(&write("inner", "a b\n"), None).as_deref(), Some("a b"));
        // Exactly one newline: a value that really ends in one keeps the rest.
        assert_eq!(field(&write("two", "x\n\n"), None).as_deref(), Some("x\n"));
        assert_eq!(field(&write("nonl", "x"), None).as_deref(), Some("x"));
        assert_eq!(field(&dir.join("absent").to_string_lossy(), None), None);
        // A directory is readable-but-not-a-file; it must not look like a value.
        assert_eq!(field(&dir.to_string_lossy(), None), None);

        let status = write("status", "Name:\tsh\nPid:\t9\nPPid:\t7\nTracerPid:\t0\n");
        assert_eq!(field(&status, Some("PPid:")).as_deref(), Some("7"));
        // `Pid:` and `TracerPid:` must not be mistaken for it in either
        // direction -- the first is a prefix-of-a-prefix, the second contains it.
        assert_eq!(field(&status, Some("Pid:")).as_deref(), Some("9"));
        assert_eq!(field(&status, Some("Nope:")), None);
        // Key present, remainder empty: still READ, so the caller seeds what the
        // kernel published rather than second-guessing it.
        assert_eq!(
            field(&write("nopid", "PPid:\t\n"), Some("PPid:")).as_deref(),
            Some("")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ash's counter is `atoi` into an `int` printed UNSIGNED, and each of those
    /// three is a place a whole-string `parse()` silently disagrees. The
    /// spawned test covers the same rules end to end; these pin the arithmetic
    /// where a wrong answer is a number rather than a failure.
    #[test]
    fn shlvl_follows_atoi_into_an_unsigned_32_bit_counter() {
        for (inherited, want) in [
            ("", 1),
            ("0", 1),
            ("4", 5),
            ("007", 8),
            ("+7", 8),
            ("zz", 1),
            ("0x10", 1),
            // A numeric prefix counts, and the scan stops at the first
            // non-digit instead of rejecting the value.
            ("4x", 5),
            ("1e2", 2),
            ("123abc456", 124),
            // strtol's leading-blank set is the C locale's, which is NARROWER
            // than Unicode's -- `trim_start()` would skip the NBSP and count
            // the 4 that ash never reaches.
            (" \t\n\x0b\x0c\r4", 5),
            ("\u{a0}4", 1),
            ("\u{2003}4", 1),
            // Negative values wrap, because the sum is printed unsigned.
            ("-1", 0),
            ("-3", 4_294_967_294),
            ("4294967295", 0),
            ("4294967296", 1),
            // Past LONG_MAX the scan saturates rather than failing, and the low
            // 32 bits of LONG_MAX are -1 -- so this is 0, where a failed parse
            // would give 1. LONG_MIN's low 32 bits are 0, so that one IS 1.
            ("99999999999999999999", 0),
            ("9223372036854775807", 0),
            ("-99999999999999999999", 1),
            ("-9223372036854775808", 1),
        ] {
            assert_eq!(super::shlvl_next(inherited), want, "SHLVL={inherited:?}");
        }
    }

    #[test]
    fn echo_and_exit_status() {
        let (status, out, _) = run("echo hello world");
        assert_eq!(status, 0);
        assert_eq!(out, "hello world\n");
        let (status, _, _) = run("exit 7");
        assert_eq!(status, 7);
    }

    #[test]
    fn allexport_marks_every_assignment_for_export() {
        // dash's setvareq ORs in VEXPORT under aflag, so `set -a` covers every
        // writer, not just a plain assignment. Asserted on the variable table
        // because td-sh has no `export -p` listing to read it back with.
        let mut sh = super::Shell::new_for_test();
        sh.set_var("before", "0").unwrap();
        sh.opts.allexport = true;
        sh.set_var("during", "1").unwrap();
        // An assignment to an EXISTING name exports it too -- the flag is applied
        // on write, not only on creation.
        sh.set_var("before", "2").unwrap();
        sh.opts.allexport = false;
        sh.set_var("after", "3").unwrap();
        let mut env: Vec<String> =
            sh.exported_env().into_iter().map(|(k, _)| k).collect();
        env.sort();
        assert_eq!(env, ["before", "during"]);
    }

    #[test]
    fn xtrace_uses_ps4_and_traces_assignments() {
        // The prefix is the EXPANDED $PS4, re-expanded per command, so it can
        // report state that changes as the script runs.
        let (_, _, err) = run("PS4='[last=$?] '; set -x; false; echo ok");
        assert_eq!(err, "[last=0] false\n[last=1] echo ok\n");
        // An assignment-only command is traced too, with its values expanded and
        // AFTER they are applied -- dash prints its varlist here.
        let (_, out, err) = run("set -x; x=1 x=2; echo $x");
        assert_eq!((out.as_str(), err.as_str()), ("2\n", "+ x=1 x=2\n+ echo 2\n"));
        // dash seeds PS4 as a real variable; unsetting it leaves an EMPTY prefix
        // rather than restoring the default, and the command is still traced.
        let (_, out, _) = run("echo [$PS4]");
        assert_eq!(out, "[+ ]\n");
        let (_, _, err) = run("unset PS4; set -x; echo hi");
        assert_eq!(err, "echo hi\n");
        // $PS4 is read under double-quote rules and expanded quoted, so a quote
        // character in it is literal rather than an unterminated quote, and
        // neither tilde expansion nor globbing applies.
        let (_, _, err) = run("PS4=\"it's ~/ * \"; set -x; echo hi");
        assert_eq!(err, "it's ~/ * echo hi\n");
        // A backslash escapes what it would inside `"..."`, with the one exception
        // dash gets from scanning against a fake end marker: `\"` keeps its
        // backslash where `\$` loses one.
        let (_, _, err) = run(r#"PS4='\q \" \$ '; set -x; echo hi"#);
        assert_eq!(err, "\\q \\\" $ echo hi\n");
        // Traced to the stderr in force BEFORE this command's redirections --
        // dash's `preverrout` -- so a command that redirects its own stderr away
        // is still reported.
        let (_, out, err) = run("set -x; x=1 2>/dev/null; echo $x");
        assert_eq!((out.as_str(), err.as_str()), ("1\n", "+ x=1\n+ echo 1\n"));
    }

    #[test]
    fn expanding_ps4_does_not_disturb_the_traced_command() {
        // A `$(...)` in $PS4 runs a real command, but tracing only observes: it is
        // not the traced command's own substitution, so it neither supplies its `$?`
        // nor counts toward the rule that gives an assignment-only command the last
        // substitution's status.
        let (_, out, _) = run("set -x; PS4='$(false) '; x=1; echo $?");
        assert_eq!(out, "0\n");
        let (_, out, _) = run("PS4='$(true) '; set -x; false; echo status=$?");
        assert_eq!(out, "status=1\n");
        // A substitution in the assignment ITSELF still sets the status, as dash does.
        let (_, out, _) = run("x=$(false); echo $?");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn a_ps4_that_cannot_expand_does_not_stop_the_shell() {
        // Both references wrap this one expansion in a handler and fall back to
        // the raw string, so the script runs on. Verified against a dash 0.5.12
        // built from source: it prints its diagnostic and still reports 0.
        for ps4 in ["+${x", "+$(x", "+oops $(( 1 / 0 )) \\$"] {
            let (status, out, _) =
                run(&format!("PS4='{ps4}'; set -x; echo one; echo status=$?"));
            assert_eq!((status, out.as_str()), (0, "one\nstatus=0\n"), "{ps4}");
        }
        // A LIVE command substitution in PS4 -- single-quoted, so it runs at
        // trace time -- must expand once and not trace its own commands. The
        // subshell it runs in inherits the guard, or this recurses until the
        // depth cap.
        let (status, out, err) = run("PS4='$(echo X)'; set -x; echo hi");
        assert_eq!((status, out.as_str(), err.as_str()), (0, "hi\n", "Xecho hi\n"));
    }

    #[test]
    fn and_or_short_circuits() {
        let (_, out, _) = run("false && echo no; true || echo no; echo done");
        assert_eq!(out, "done\n");
    }

    #[test]
    fn if_takes_the_right_branch() {
        let (_, out, _) = run("if true; then echo yes; else echo no; fi");
        assert_eq!(out, "yes\n");
        let (_, out, _) = run("if false; then echo yes; else echo no; fi");
        assert_eq!(out, "no\n");
    }

    #[test]
    fn for_loop_iterates() {
        let (_, out, _) = run("for x in a b c; do echo $x; done");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn while_loop_counts() {
        let (_, out, _) = run("i=0; while [ $i -lt 3 ]; do echo $i; i=$((i + 1)); done");
        assert_eq!(out, "0\n1\n2\n");
    }

    /// `[[ ]]`'s operands are neither field-split nor pathname-expanded, which
    /// is the whole reason it is syntax rather than a builtin: `[ -n $f ]` on a
    /// name with a space in it is a syntax error, and this is not.
    #[test]
    fn a_conditional_does_not_split_or_glob_its_operands() {
        let (st, _, _) = run("f='a b'; [[ -n $f ]]");
        assert_eq!(st, 0);
        // Two words to `test`, one to `[[ ]]`.
        let (st, _, _) = run("f='a b'; [[ $f == 'a b' ]]");
        assert_eq!(st, 0);
    }

    /// The right side of `==` is a PATTERN, and its QUOTING decides per
    /// character -- exactly `case`'s rule. Getting this backwards would make
    /// every `[[ $x == *.c ]]` a literal comparison.
    #[test]
    fn a_conditional_match_takes_its_right_side_as_a_pattern() {
        assert_eq!(run("[[ abc == a* ]]").0, 0);
        assert_eq!(run("[[ abc == \"a*\" ]]").0, 1);
        assert_eq!(run("[[ 'a*' == \"a*\" ]]").0, 0);
        assert_eq!(run("[[ abc == ?b? ]]").0, 0);
        assert_eq!(run("[[ abc != a* ]]").0, 1);
    }

    /// An `-eq` operand is a full arithmetic expression, so a name resolves
    /// through its value and an assignment inside one TAKES. ash reads the
    /// operand as a number instead and calls this one bad, which is a
    /// divergence this construct inherits from `$(( ))` rather than chooses.
    #[test]
    fn a_conditional_arithmetic_operand_resolves_like_an_expression() {
        assert_eq!(run("e=1+2; [[ e -eq 3 ]]").0, 0);
        assert_eq!(run("e=1+2; [[ e -eq 4 ]]").0, 1);
        assert_eq!(run("n=m; m=5; [[ n -eq 5 ]]").0, 0);
        assert_eq!(run("x='(y=9)'; [[ x -eq 9 ]] && echo y=$y").1, "y=9\n");
        // A bad expression is still REPORTED rather than fatal, which is the
        // whole reason this path is `try_eval` and not `eval`.
        assert_eq!(run("e=1+; [[ e -eq 3 ]]; echo after").1, "after\n");
        // An operand is a WHOLE expression, so a logical operator inside one
        // runs both its sides here as it does in `$(( ))`. bash short-circuits
        // and ash calls the operand a bad number; sharing one evaluator is what
        // this costs, and it is confined to operands whose text has `&&`/`||`.
        assert_eq!(run("x='0 && (m=7)'; [[ x -eq 0 ]] && echo m=$m").1, "m=7\n");
        assert_eq!(run("m=0; [[ 'm++ && m++' -eq 0 ]] && echo m=$m").1, "m=2\n");
    }

    /// `<`/`>` are STRING order inside `[[ ]]`, where outside they are
    /// redirections -- `[[ 10 < 9 ]]` is true because "1" sorts before "9",
    /// and the numeric spellings are the `-lt` family. Both directions are
    /// pinned because a `<` read as a redirection would silently create a file
    /// and report success.
    #[test]
    fn a_conditional_compares_strings_with_angle_brackets() {
        assert_eq!(run("[[ a < b ]]").0, 0);
        assert_eq!(run("[[ b < a ]]").0, 1);
        assert_eq!(run("[[ 10 < 9 ]]").0, 0);
        assert_eq!(run("[[ 10 -lt 9 ]]").0, 1);
    }

    /// The `-eq` family evaluates both sides as ARITHMETIC, not as integers:
    /// `[[ 1+1 -eq 2 ]]` holds and a bare name is its value, where
    /// `test x -eq 5` is an error. Measured against bash rather than assumed --
    /// deferring these to `test`'s comparison was wrong for every line here.
    #[test]
    fn a_conditional_compares_numbers_arithmetically() {
        assert_eq!(run("x=5; [[ x -eq 5 ]]").0, 0);
        assert_eq!(run("[[ 1+1 -eq 2 ]]").0, 0);
        // An unset name is zero in arithmetic, so this is false, not an error.
        assert_eq!(run("[[ nosuch -eq 1 ]]").0, 1);
        assert_eq!(run("[[ nosuch -eq 0 ]]").0, 0);
        // ...and so is an operand that expanded to nothing at all.
        assert_eq!(run("[[ \"\" -eq 0 ]]").0, 0);
        assert_eq!(run("[[ 3 -ne 4 && 2 -le 2 ]]").0, 0);
    }

    /// `!`, `&&`, `||` and parentheses bind as the parser says rather than as a
    /// flat argv has to guess, and `&&`/`||` SHORT-CIRCUIT.
    #[test]
    fn a_conditional_binds_and_short_circuits() {
        assert_eq!(run("[[ ! a == b ]]").0, 0);
        assert_eq!(run("[[ ! ! a == a ]]").0, 0);
        assert_eq!(run("[[ a == a || b == c ]]").0, 0);
        assert_eq!(run("[[ ( a == b ) || ( c == c ) ]]").0, 0);
        assert_eq!(run("[[ a == a && b == b && c == c ]]").0, 0);
        // The right side of a failed `&&` is never evaluated: a command
        // substitution there would otherwise run.
        let (st, out, _) = run("[[ a == b && $(echo ran) == ran ]]; echo $?");
        assert_eq!((st, out.as_str()), (0, "1\n"));
    }

    /// `( … )` and `!` inside `[[ ]]` recurse WITHOUT passing through
    /// `parse_command`, so they need its depth cap explicitly. A stack overflow
    /// is the shell DYING rather than a diagnostic, whatever the panic strategy
    /// -- and it did, with SIGABRT, until `cond_term` was
    /// guarded. Both shapes are pinned: the parenthesis nesting that found it,
    /// and the `!` chain that reaches the same recursion without any bracket.
    #[test]
    fn a_deeply_nested_conditional_errors_instead_of_overflowing() {
        // The shell's own status, not `echo $?`: this is a PARSE error, so it
        // ends the whole input and nothing after it runs.
        let src = format!("[[ {} a {} ]]", "(".repeat(5000), ")".repeat(5000));
        assert_eq!(run(&src).0, 2);
        let src = format!("[[ {} a ]]", "! ".repeat(5000));
        assert_eq!(run(&src).0, 2);
        // A FLAT chain is the same hazard by a different route: parsing it
        // iterates, but the tree is left-deep and both `eval_cond` and its own
        // `Drop` walk that spine, so a long chain aborted with nothing deep in
        // the parser's stack to show for it.
        let src = format!("[[ t{} ]]", " && t".repeat(100_000));
        assert_eq!(run(&src).0, 2);
        let src = format!("[[ t{} ]]", " || t".repeat(100_000));
        assert_eq!(run(&src).0, 2);
        // ...and a chain of ordinary length still runs.
        let src = format!("[[ a == a{} ]]; echo $?", " && a == a".repeat(200));
        assert_eq!(run(&src).1, "0\n");
        // ...and an ordinary depth still parses and runs.
        assert_eq!(run("[[ ((((a)))) ]]; echo $?").1, "0\n");
    }

    /// A pipeline STAGE and a background JOB run this same recursive evaluator
    /// on a THREAD, and every depth guard here bounds native recursion against
    /// the main thread's stack. Rust's default 2 MiB left `MAX_RUN_DEPTH` above
    /// where a stage's stack actually ended, so the guard could not fire: a
    /// debug build died on `${x:-$( )}` nesting from 200 and `$( )` from 240,
    /// and dying is the whole PROCESS rather than the stage: Rust aborts on a
    /// stack overflow whatever the panic strategy, so it is a signal and not a
    /// diagnostic.
    ///
    /// Both thread kinds are pinned, at a depth PAST the guard, because what
    /// broke was reaching the guard at all rather than what it then said.
    ///
    /// The JOB goes first, and that order is the test rather than a detail:
    /// glibc caches an exiting thread's stack and gives it to the next request
    /// whatever size that asks for, so a stage running first hands its 8 MiB on
    /// and the job passes without ever having asked for one.
    #[test]
    fn a_stage_and_a_job_reach_the_recursion_guard_rather_than_the_stack_end() {
        // Past the guard by construction rather than by a literal, so raising
        // MAX_RUN_DEPTH cannot leave this passing a script that never reaches
        // it -- and if it is ever raised past what the stack holds, this is
        // what stops instead of a user.
        let n = super::MAX_RUN_DEPTH as usize + 50;
        let subs = format!("{}echo 1{}", "$(".repeat(n), ")".repeat(n));
        let param = format!("{}echo 1{}", "${x:-$(".repeat(n), ")}".repeat(n));
        for nest in [&subs, &param] {
            for src in [
                format!("{{ echo {nest}; }} &\nwait\necho AFTER"),
                // Consumed by a BUILTIN: `cat` is not on the test shell's PATH,
                // so the pipeline would report that as well and the assertion
                // would be reading a stage that failed for a second reason.
                format!("echo {nest} | while read x; do :; done; echo AFTER"),
            ] {
                let (_, out, err) = run(&src);
                assert!(
                    err.contains("maximum recursion depth exceeded"),
                    "guard did not fire: {err:?}",
                );
                // Ends WITH it rather than equals: the abandoned substitution
                // still leaves `echo` a blank line to print.
                assert!(out.ends_with("AFTER\n"), "the shell did not survive it: {out:?}");
            }
        }
        // ...and an ordinary nesting in a stage still runs, so the bound is not
        // simply refusing everything a thread evaluates. Consumed by a BUILTIN
        // rather than `cat`, whose output a captured fd never sees.
        let src = "echo $(echo $(echo hi)) | while read x; do echo got=$x; done";
        assert_eq!(run(src).1, "got=hi\n");
    }

    /// The same depth on the PLAIN path -- no stage, no job -- which the two
    /// `stack_size` calls beside it never covered. `main` runs on the shell's
    /// stack now and so does this harness, and the harness is the half that
    /// could not be taken on trust: libtest gives a test thread 2 MiB, the very
    /// size that made a stage crash, so before this the assertion below aborted
    /// the test binary instead of failing.
    #[test]
    fn the_plain_path_reaches_the_recursion_guard_rather_than_the_stack_end() {
        let n = super::MAX_RUN_DEPTH as usize + 50;
        let param = format!("{}echo 1{}", "${x:-$(".repeat(n), ")}".repeat(n));
        let (_, out, err) = run(&format!("echo {param}; echo AFTER"));
        assert!(err.contains("maximum recursion depth exceeded"), "guard: {err:?}");
        assert!(out.ends_with("AFTER\n"), "the shell did not survive it: {out:?}");
    }

    /// The three unary operators `test` does not serve. `-a` is the interesting
    /// one: in `test` that spelling is the binary AND operator, so `test`'s
    /// roster cannot have it, while inside `[[ ]]` the connective is `&&` and
    /// bash gives the letter `-e`'s meaning.
    #[test]
    fn a_conditional_serves_three_operators_test_cannot() {
        assert_eq!(run("v=1; [[ -v v ]]").0, 0);
        assert_eq!(run("v=''; [[ -v v ]]").0, 0);
        assert_eq!(run("unset u; [[ -v u ]]").0, 1);
        // A POSITIONAL is set when the list reaches it. These live in `params`
        // rather than in the variable table, so asking `get_var` reported every
        // one of them unset while `$1` expanded to its value.
        assert_eq!(run("set -- a; [[ -v 1 ]]").0, 0);
        assert_eq!(run("set -- a; [[ -v 2 ]]").0, 1);
        assert_eq!(run("set -- a b; [[ -v 2 ]]").0, 0);
        assert_eq!(run("[[ -v 0 ]]").0, 0);
        assert_eq!(run("[[ -a / ]]").0, 0);
        assert_eq!(run("[[ -a /nonexistent-4b8a ]]").0, 1);
        assert_eq!(run("set -e; [[ -o errexit ]]").0, 0);
        assert_eq!(run("[[ -o errexit ]]").0, 1);
        // An unknown option name is FALSE, not an error, as bash's is.
        assert_eq!(run("[[ -o nosuchoption ]]").0, 1);
    }

    /// A bad ARITHMETIC operand is diagnosed and then FALSE, and above all does
    /// not end the shell. Routing it through `arith::eval` did: that reports a
    /// malformed expression the way `$(( ))` needs, which is fatally, so
    /// `[[ 1+ -eq 2 ]]` killed a non-interactive shell before the next command.
    /// bash prints the diagnostic, answers 1, and carries on.
    #[test]
    fn a_bad_arithmetic_operand_is_false_and_does_not_end_the_shell() {
        let (st, out, err) = run("[[ 1+ -eq 2 ]]; echo after");
        assert_eq!(out, "after\n", "the shell did not survive the expression");
        assert_eq!(st, 0);
        assert_eq!(err, "td-sh: [[: arithmetic syntax error\n");
        // The conditional's own status is bash's 1, not the 2 a malformed
        // EXPRESSION gets.
        assert_eq!(run("[[ 1+ -eq 2 ]]").0, 1);
    }

    /// A refusal the SHELL raised is reported ONCE.
    ///
    /// All three constructs that reach arith are pinned, because they report
    /// DIFFERENTLY and share only the mechanism: `$(( ))` is fatal at status 2
    /// as ash's is, `[[ ]]` diagnoses and answers 1 as bash's does, and a
    /// SLICE bound carries the refusal out through the expansion around it.
    /// Stderr is asserted WHOLE rather than by substring, since the defect was
    /// a duplicate that every `contains` check passed.
    #[test]
    fn a_refusal_the_shell_reported_is_not_reported_again() {
        for expr in ["n=2", "n+=2", "n++", "++n", "1?n=2:3"] {
            let (st, out, err) = run(&format!("readonly n=1; echo $(({expr}))"));
            assert_eq!(err, "td-sh: n: is read only\n", "$(({expr}))");
            assert_eq!(st, 2, "$(({expr}))");
            assert_eq!(out, "", "$(({expr}))");
        }
        // Fatal, so nothing after it runs -- ash and bash both stop here too.
        assert_eq!(run("readonly n=1; echo $((n=2)); echo after").1, "");
        // An arm the conditional disabled never assigns, so there is no
        // refusal to report once or twice.
        assert_eq!(run("readonly n=1; echo $((0?n=2:3))"), (0, "3\n".into(), String::new()));
        // `[[ ]]` catches it instead of dying of it: one diagnostic, and NOT
        // under the `[[: ` prefix, which belongs to messages it raised itself.
        let (st, out, err) = run("readonly n=1; [[ 'n=2' -eq 2 ]]; echo after=$?");
        assert_eq!(err, "td-sh: n: is read only\n");
        assert_eq!(out, "after=1\n");
        assert_eq!(st, 0);
        // The other arm still earns that prefix, which is what stops the fix
        // above from silencing a message arith really did raise.
        assert_eq!(run("[[ 1+ -eq 2 ]]").2, "td-sh: [[: arithmetic syntax error\n");
        // A SLICE bound is the third way into arith, and reaches it through
        // `expand` rather than either entry point above -- so it is the one
        // that shows the fix is in the shared type and not in a caller.
        for src in ["${v:n=2:2}", "${v:1:n=2}"] {
            let (st, _, err) = run(&format!("readonly n=1; v=abcdef; echo \"{src}\""));
            assert_eq!(err, "td-sh: n: is read only\n", "{src}");
            assert_eq!(st, 2, "{src}");
        }
    }

    /// A failed arithmetic OPERAND stops the comparison: the second is never
    /// evaluated, which is bash's answer and matters twice over. It is how two
    /// refusals stay one diagnostic, and -- the half no message count would
    /// catch -- an operand ASSIGNS, so evaluating the second anyway left a name
    /// set that bash never sets. The side effect is the assertion for that
    /// reason; `m` is the witness that the operand ran.
    #[test]
    fn a_failed_arithmetic_operand_stops_the_comparison() {
        // Left refused as read only: `m` is untouched and the refusal is said
        // once, where before it was said twice and `m` took 3.
        let (st, out, err) = run("readonly n=1; [[ 'n=2' -eq 'm=3' ]]; echo m=[$m]");
        assert_eq!(err, "td-sh: n: is read only\n");
        assert_eq!(out, "m=[]\n");
        assert_eq!(st, 0);
        // Two readonly names is the shape that printed two different lines.
        let err = run("readonly n=1 m=1; [[ 'n=2' -eq 'm=3' ]]").2;
        assert_eq!(err, "td-sh: n: is read only\n");
        // A left operand that is merely MALFORMED stops it just the same.
        assert_eq!(run("[[ '1+' -eq 'm=3' ]]; echo m=[$m]").1, "m=[]\n");
        // A failure on the RIGHT still leaves the left one's assignment, which
        // is what makes the rule an ORDER rather than a refusal to assign.
        let (_, out, err) = run("readonly n=1; [[ 'a=1' -eq 'n=2' ]]; echo a=[$a]");
        assert_eq!(err, "td-sh: n: is read only\n");
        assert_eq!(out, "a=[1]\n");
        // Both operands run when the first SUCCEEDS -- the short-circuit must
        // not cost the ordinary case its second side effect.
        assert_eq!(run("[[ 'a=1' -eq 'm=3' ]]; echo a=[$a] m=[$m]").1, "a=[1] m=[3]\n");
        assert_eq!(run("[[ 'a=3' -eq 'm=3' ]]").0, 0);
    }

    /// `=~` searches rather than matching whole, its right-hand side is a
    /// regex where `==`'s is a glob, and quoting still decides per character.
    #[test]
    fn a_conditional_searches_with_a_regular_expression() {
        assert_eq!(run("[[ abc =~ b ]]").0, 0, "a search, not a whole-string match");
        assert_eq!(run("[[ abc =~ ^b ]]").0, 1);
        assert_eq!(run("[[ abc =~ ^a ]]").0, 0);
        assert_eq!(run("[[ abc =~ c$ ]]").0, 0);
        // A quoted metacharacter is a literal, as it is for `==`.
        assert_eq!(run("[[ abc =~ \"a.c\" ]]").0, 1);
        assert_eq!(run("[[ a.c =~ \"a.c\" ]]").0, 0);
        // ...and an UNQUOTED expansion is a regex, which is the documented idiom.
        assert_eq!(run("r='a.c'; [[ abc =~ $r ]]").0, 0);
        assert_eq!(run("r='a.c'; [[ abc =~ \"$r\" ]]").0, 1);
        // The `==` operator is unaffected: its RHS is still a GLOB, and the
        // two readings of `a*` have to be told apart by a case where they
        // DISAGREE -- a glob `a*` anchors at the start and must match the whole
        // subject, where the regex one matches an empty run anywhere.
        assert_eq!(run("[[ xbc == a* ]]").0, 1, "glob: no match");
        assert_eq!(run("[[ xbc =~ a* ]]").0, 0, "regex: empty run matches");
        assert_eq!(run("[[ ab == a* ]]").0, 0);
        assert_eq!(run("[[ zab =~ ^a* ]]").0, 0, "`a*` matches the empty prefix");
    }

    /// A regex that does not COMPILE ends the shell, where a false comparison
    /// does not. That is the corpus's graded answer and bash's recorded BUG:
    /// carrying on would answer "no match" for every subject, which is
    /// indistinguishable from a real miss.
    #[test]
    fn an_uncompilable_regex_is_fatal() {
        let (st, out, err) = run("[[ abc =~ a[ ]]; echo after");
        assert_eq!(out, "", "the shell kept going after a bad regex");
        assert_eq!(st, 2);
        assert!(err.contains("[["), "no diagnostic: {err:?}");
        assert_eq!(run("[[ { =~ { ]]; echo after").1, "");
    }

    /// The one place the shell's punctuation and a regex's overlap. Inside a
    /// `=~` right-hand side `|` is alternation and a balanced `( )` group is
    /// part of the expression, blanks and all -- and NOWHERE else does that
    /// hold, which is the half worth testing.
    #[test]
    fn the_regex_operand_lexes_as_one_word() {
        assert_eq!(run("[[ abc =~ a|z ]]").0, 0);
        assert_eq!(run("[[ abc =~ (a|b) ]]").0, 0);
        assert_eq!(run("[[ ab =~ (a)(b) ]]").0, 0);
        assert_eq!(run("[[ 'a b' =~ (a b) ]]").0, 0, "a group absorbs a blank");
        assert_eq!(run("[[ abc =~ (a|(b|c)) ]]").0, 0);
        // An unbalanced group is an error, not a swallowed rest-of-script.
        assert_eq!(run("[[ abc =~ a(b ]]").0, 2);
        // `&&` is still the connective, so the left side really is tested.
        assert_eq!(run("[[ zzz =~ a&&b ]]").0, 1);
        assert_eq!(run("[[ abc =~ a&&b ]]").0, 0);
        // Outside the brackets nothing changed: `=~` is an ordinary word and
        // `|` is still a pipe.
        assert_eq!(run("echo =~").1, "=~\n");
        assert_eq!(run("echo ab | { read x; echo [$x]; }").1, "[ab]\n");
    }

    /// The regex lexer mode must not survive the command it was armed in. The
    /// lexer cannot tell `[[` in COMMAND POSITION from `[[` as an argument --
    /// that is the parser's job -- so `echo [[` raises a bracket count nothing
    /// will ever close, and before this was bounded a later `=~` lexed its
    /// operand as a regex and SWALLOWED A PIPE: `echo [[; echo a =~ b|tr a-z
    /// A-Z` printed the pipeline as text.
    #[test]
    fn the_regex_lexer_mode_does_not_outlive_its_command() {
        // `;`, `|` and a newline each end it. The sink is a builtin group so
        // the pipeline needs no external command: what it prints is what
        // reached the pipe, which is the whole question.
        let sink = "{ read l; echo \"got:$l\"; }";
        assert_eq!(run(&format!("echo [[; echo a =~ b|{sink}")).1, "[[\ngot:a =~ b\n");
        assert_eq!(run(&format!("echo [[ |{sink}; echo a =~ b|{sink}")).1, "got:[[\ngot:a =~ b\n");
        assert_eq!(run(&format!("echo [[\necho a =~ b|{sink}")).1, "[[\ngot:a =~ b\n");
        // ...and the connectives do NOT, since a conditional continues past
        // them: a regex after one still lexes as a regex.
        assert_eq!(run("[[ abc =~ a|z &&\nabc =~ b|y ]]").0, 0);
        // A `[[` that is an ARGUMENT arms nothing, even in the same command --
        // there is no `;` here to end anything, so only its position saves it.
        let sink = "{ read l; echo \"got:$l\"; }";
        assert_eq!(run(&format!("echo [[ =~ a|{sink}")).1, "got:[[ =~ a\n");
        // ...and the word `=~` as a literal OPERAND does not arm it either,
        // which needs an operand to actually be there to scan.
        assert_eq!(run("[[ =~ && x ]]").0, 0);
        // Every position a conditional can legitimately start in still works.
        assert_eq!(run("if [[ abc =~ a|z ]]; then echo y; fi").1, "y\n");
        assert_eq!(run("while [[ abc =~ a|z ]]; do echo y; break; done").1, "y\n");
        assert_eq!(run("until [[ abc =~ q|z ]]; do echo y; break; done").1, "y\n");
        assert_eq!(run("! [[ abc =~ q|z ]]").0, 0);
        assert_eq!(run("{ [[ abc =~ a|z ]]; }").0, 0);
    }

    /// A newline continues the expression exactly where a TERM is expected and
    /// ends it everywhere else. All ten positions were measured against bash
    /// 5.2 rather than assumed: it accepts the first five here and refuses the
    /// last five, which is the split this pins.
    #[test]
    fn a_newline_continues_a_conditional_only_where_a_term_is_expected() {
        for src in [
            "[[\na ]]",
            "[[ a &&\nb ]]",
            "[[ a ||\nb ]]",
            "[[ (\na ) ]]",
            "[[ a &&\n\n\nb ]]",
        ] {
            assert_eq!(run(src).0, 0, "should have continued: {src:?}");
        }
        // `[[ !\na ]]` continues too, and negating a non-empty word is 1.
        assert_eq!(run("[[ !\na ]]").0, 1);
        for src in [
            "[[ a\n&& b ]]",
            "[[ ( a\n) ]]",
            "[[ a\n]]",
            "[[ a ==\na ]]",
            "[[ -z\na ]]",
        ] {
            assert_eq!(run(src).0, 2, "should have been a syntax error: {src:?}");
        }
    }

    /// Digits hard against a redirection operator lex as an `IoNumber`, which
    /// inside `[[ ]]` can only come from `2>1`. bash calls that a conditional
    /// syntax error; reading it back as a word would silently compare "2" with
    /// "1" and answer a question nobody asked.
    #[test]
    fn a_redirection_inside_a_conditional_is_a_syntax_error() {
        assert_eq!(run("[[ 2>1 ]]").0, 2);
        // The spaced form is an ordinary comparison and still works.
        assert_eq!(run("[[ 2 -gt 1 ]]").0, 0);
    }

    /// A malformed expression is status 2 with a diagnostic, as `test`'s is --
    /// so a script cannot tell the two constructs apart by their statuses --
    /// and `[[` keeps its ordinary meaning everywhere the grammar does not
    /// claim it.
    #[test]
    fn a_malformed_conditional_is_status_two_and_the_word_still_works() {
        let (st, _, err) = run("[[ || true ]]");
        assert_eq!(st, 2);
        assert!(!err.is_empty(), "a syntax error said nothing");
        let (st, _, _) = run("[[ a == ]]");
        assert_eq!(st, 2);
        // Not in command position, `[[` is just a word.
        let (_, out, _) = run("echo [[ ]]");
        assert_eq!(out, "[[ ]]\n");
    }

    #[test]
    fn case_matches_a_pattern() {
        let (_, out, _) = run("x=banana; case $x in apple) echo a ;; b*) echo b ;; *) echo o ;; esac");
        assert_eq!(out, "b\n");
    }

    #[test]
    fn function_sees_positional_params() {
        let (_, out, _) = run("greet() { echo \"hi $1\"; }; greet world");
        assert_eq!(out, "hi world\n");
    }

    #[test]
    fn subshell_status_propagates() {
        let (_, out, _) = run("( exit 4 ); echo $?");
        assert_eq!(out, "4\n");
    }

    #[test]
    fn break_and_continue() {
        let (_, out, _) = run("for x in 1 2 3 4; do if [ $x = 3 ]; then break; fi; echo $x; done");
        assert_eq!(out, "1\n2\n");
        let (_, out, _) =
            run("for x in 1 2 3; do if [ $x = 2 ]; then continue; fi; echo $x; done");
        assert_eq!(out, "1\n3\n");
    }

    #[test]
    fn errexit_stops_on_failure() {
        let (status, out, _) = run("set -e; false; echo unreached");
        assert_eq!(out, "");
        assert_eq!(status, 1);
    }

    #[test]
    fn a_compound_redirection_failure_is_exempt_from_errexit() {
        // ash wraps a compound carrying redirections in an `NREDIR`, which leaves
        // `checkexit` at 0, so the 1 it reports is never tested against `set -e`.
        for src in [
            "{ :; } </no/such/td-e",
            "while :; do break; done </no/such/td-e",
            "for x in a; do :; done </no/such/td-e",
            "if :; then :; fi </no/such/td-e",
            "case x in x) :;; esac </no/such/td-e",
        ] {
            let (status, out, _) = run(&format!("set -e; {src}; echo st=$?; echo alive"));
            assert_eq!((status, out.as_str()), (0, "st=1\nalive\n"), "src: {src}");
        }
        // The contrast that makes it a rule about the NODE rather than about
        // redirections: each reports the same 1 from a node ash checks, and so
        // exits. The function call and `[[ ]]` are `NCMD`s, the latter because
        // busybox ash serves `[[` from `test`; the last two are the exemption
        // losing to the `NPIPE` around it, which is checked whatever its stages
        // are. `true` rather than `:` deliberately: POSIX makes a failed
        // redirection on a SPECIAL builtin fatal outright, so `:` would leave
        // here whether or not `errexit` looked at it.
        for src in [
            "true </no/such/td-e",
            "[[ x = x ]] </no/such/td-e",
            "f() { { :; } </no/such/td-e; }; f",
            "true | false",
            "true | { :; } </no/such/td-e",
        ] {
            let (status, out, _) = run(&format!("set -e; {src}; echo unreached"));
            assert_eq!((status, out.as_str()), (1, ""), "src: {src}");
        }
        // The subshell is the one a reader expects to see above: ash's parser
        // leaves its redirections on the `NSUBSHELL` rather than wrapping it, so
        // it is checked. 2 and not 1 because the status is the fatal one its
        // child dies of -- `SUBSHELL_REDIR_ERROR`, pinned in `process.rs`.
        let (status, out, _) = run("set -e; ( : ) </no/such/td-e; echo unreached");
        assert_eq!((status, out.as_str()), (2, ""));
    }

    #[test]
    fn the_exemption_survives_an_enclosing_compound() {
        // The exemption belongs to every node on the way out, not to the innermost
        // one: an enclosing compound reports the very same status, and re-testing
        // it there would exit the shell for a failure the rule just passed over.
        for src in [
            "{ { :; } </no/such/td-e; }",
            "if :; then { :; } </no/such/td-e; fi",
            "for x in a; do { :; } </no/such/td-e; done",
            "case x in x) { :; } </no/such/td-e ;; esac",
            "{ if :; then { :; } </no/such/td-e; fi; }",
        ] {
            let (status, out, _) = run(&format!("set -e; {src}; echo st=$?; echo alive"));
            assert_eq!((status, out.as_str()), (0, "st=1\nalive\n"), "src: {src}");
        }
    }

    #[test]
    fn a_compound_does_not_retest_a_status_errexit_passed_over() {
        // The same rule reached through the OTHER two nodes that report a nonzero
        // status nothing checked -- `!` (`NNOT`) and a non-final `&&`/`||` operand
        // -- which is what makes it the node's property rather than a redirection's.
        for src in [
            "if :; then ! true; fi",
            "for x in a; do ! true; done",
            "{ false && true; }",
            "if :; then false && true; fi",
        ] {
            let (status, out, _) = run(&format!("set -e; {src}; echo st=$?; echo alive"));
            assert_eq!((status, out.as_str()), (0, "st=1\nalive\n"), "src: {src}");
        }
    }

    #[test]
    fn the_redirection_exemption_does_not_reach_the_next_command() {
        // The exemption is the node's, so the next command is judged on its own:
        // were it a property of the shell instead, `set -e` would silently stop
        // working for the rest of the script.
        let (status, out, _) = run("set -e; { :; } </no/such/td-e; false; echo unreached");
        assert_eq!((status, out.as_str()), (1, ""));
        // `|| false` and not `|| true; false`: the exempt compound's own operand
        // is the one that must not carry anything forward, and only this shape
        // asks it. With a command in between, that operand takes the exemption
        // and the assertion holds whether or not the next one would have.
        let (status, out, _) = run("set -e; { :; } </no/such/td-e || false; echo unreached");
        assert_eq!((status, out.as_str()), (1, ""));
    }

    #[test]
    fn errexit_exempts_conditions_and_nonfinal_operands() {
        // A failing `if`/`while` condition, a `!`-negated pipeline, and a non-final
        // `&&`/`||` operand must NOT trip errexit.
        let (_, out, _) = run("set -e; if false; then echo x; fi; echo a");
        assert_eq!(out, "a\n");
        let (_, out, _) = run("set -e; ! false; echo b");
        assert_eq!(out, "b\n");
        let (_, out, _) = run("set -e; false && echo no; echo c");
        assert_eq!(out, "c\n");
        let (_, out, _) = run("set -e; false || true; echo d");
        assert_eq!(out, "d\n");
    }

    #[test]
    fn prefix_assignment_is_transient_for_regular_builtins() {
        // A prefix on a regular builtin (echo) is visible only for that command.
        let (_, out, _) = run("FOO=bar echo hi; echo \"[${FOO}]\"");
        assert_eq!(out, "hi\n[]\n");
    }

    #[test]
    fn a_redirection_is_in_force_before_the_prefix_is_assigned() {
        // ash's order: `redirectsafe` (ash.c:10477) precedes the assignment loop
        // (10490). Three things turn on it, and all three were wrong here.
        //
        // A target that NAMES one of the assignments expands to the value it had
        // BEFORE the command, so this target is the empty one and the redirection
        // fails -- taking the builtin with it.
        let (_, out, _) = run("X=; X=/dev/null echo hi >\"$X\"; echo st=$?");
        assert_eq!(out, "st=1\n");
        // The assignment's own expansion runs with the redirection already applied.
        let (_, out, err) = run("y=$(echo E >&2) true 2>/dev/null; echo done");
        assert_eq!((out.as_str(), err.as_str()), ("done\n", ""));
        // A target that fails to EXPAND leaves the old value standing, which is
        // what an EXIT trap then finds.
        let (_, out, _) = run("trap 'echo t=[$X]' EXIT; X=old; X=new true >${u:?boom}");
        assert_eq!(out, "t=[old]\n");
        // The same for a SPECIAL builtin, whose branch already had this order --
        // and where the failed redirection is POSIX-fatal, so nothing after it
        // runs at all.
        let (_, out, err) = run("X=; X=/dev/null : >\"$X\"; echo st=$?");
        assert_eq!(out, "");
        assert!(!err.is_empty(), "expected a redirection diagnostic");
    }

    #[test]
    fn prefix_assignment_persists_for_special_builtins() {
        // A prefix on a special builtin (`:`) stays set in the current shell.
        let (_, out, _) = run("FOO=bar :; echo \"[${FOO}]\"");
        assert_eq!(out, "[bar]\n");
    }

    #[test]
    fn a_failed_redirection_leaves_the_prefix_unexpanded() {
        // ash `goto out`s at the failure (ash.c:10487) without ever reaching the
        // assignment loop. Redirecting FIRST is not enough on its own -- the
        // assignments have to be skipped too, and only a value with a side effect
        // can tell the two apart, since a rolled-back binding looks the same
        // either way.
        let (_status, out, err) =
            run("A=$(echo SIDE >&2) true >/no/such/td-dir/f; echo after st=$?");
        assert_eq!(out, "after st=1\n");
        assert!(!err.contains("SIDE"), "err: {err:?}");
        // The other tell: an assignment that would ITSELF raise stays silent.
        let (_status, out, err) =
            run("readonly B=b; A=1 B=2 true >/no/such/td-dir/f; echo alive st=$?");
        assert_eq!(out, "alive st=1\n");
        assert!(!err.contains("read only"), "err: {err:?}");
    }

    #[test]
    fn the_redirections_are_restored_however_the_builtin_left() {
        // `restore_redirs` is unconditional in the applied arm, which only the two
        // shapes that reach it with an `Err` can show. A non-terminating signal:
        // if fd 1 stayed on /dev/null, `alive` would vanish rather than fail.
        let (_status, out, _) = run("f() { command return 3 >/dev/null; }; f; echo alive");
        assert_eq!(out, "alive\n");
        // And a terminating one, whose EXIT trap has to reach the real stdout.
        let (status, out, _) =
            run("trap 'echo TRAPOUT' EXIT; readonly B=b; B=B2 true >/dev/null; echo unreached");
        assert_eq!((status, out.as_str()), (2, "TRAPOUT\n"));
    }

    #[test]
    fn a_failed_redirection_on_local_is_fatal() {
        // `local` is special in ash's sense (`spclbltin`) though not in POSIX's,
        // and that is the set the fatality turns on -- so this aborts where a
        // regular builtin's failed redirection would only report.
        let (status, out, err) =
            run("f() { local X=1 >/no/such/td-dir/f; echo survived; }; f; echo outer");
        assert_eq!((status, out.as_str()), (1, ""));
        assert!(!err.is_empty(), "expected a redirection diagnostic");
        // Not under `command`: ash locks that decision to the command WORD, which
        // is then `command`, a regular builtin.
        let (_status, out, _) =
            run("f() { command local X=1 >/no/such/td-dir/f; echo survived; }; f; echo outer");
        assert_eq!(out, "survived\nouter\n");
    }

    #[test]
    fn command_swallows_its_bodys_abort_but_not_the_prefixs() {
        // Both surface as an `Abort` and only one is caught: ash re-raises what
        // `evalcommand` itself raised, so a bad prefix assignment stays fatal.
        let (status, out, _) = run("A=1 B=${u:?boom} command true; echo after");
        assert_eq!((status, out.as_str()), (2, ""));
        // The body's is swallowed -- here with a redirection already applied, so
        // the restore has to survive the swallow too.
        let (status, out, _) = run("command shift bad 2>/dev/null; echo after=$?");
        assert_eq!((status, out.as_str()), (0, "after=2\n"));
        // An assignment abort under an applied redirection leaves the old value
        // standing for the EXIT trap, and does not take the trap's stdout with it.
        let (status, out, _) =
            run("X=old; trap 'echo t=[$X]' EXIT; A=${u:?boom} true >/dev/null; echo after");
        assert_eq!((status, out.as_str()), (2, "t=[old]\n"));
    }

    #[test]
    fn break_and_continue_with_a_level_count() {
        let (_, out, _) =
            run("for i in 1 2; do for j in a b; do break 2; done; echo $i; done; echo done");
        assert_eq!(out, "done\n");
        let (_, out, _) = run(
            "for i in 1 2; do for j in a b; do continue 2; done; echo inner$i; done; echo done",
        );
        assert_eq!(out, "done\n");
    }

    #[test]
    fn unbounded_recursion_is_caught_not_crashed() {
        // The runtime depth guard turns runaway recursion into a graceful fatal
        // error (controlled unwind + message) rather than a stack-overflow SIGSEGV.
        let (status, _out, err) = run("f() { f; }; f; echo unreached");
        assert_eq!(status, 2);
        assert!(err.contains("recursion depth exceeded"), "err: {err:?}");
    }

    #[test]
    fn pipeline_stages_do_not_leak_assignments() {
        // POSIX runs every pipeline stage in a subshell, so an assignment in a stage
        // is invisible to the parent.
        let (_, out, _) = run("y=1; { y=2; } | true; echo $y");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn redirection_failure_is_not_fatal_for_a_regular_command() {
        // Without `-e`, a failed redirection reports the error and sets a non-zero
        // status, but the shell continues (dash) rather than exiting. The command
        // itself does not run, so no `x` is printed.
        let (_status, out, err) = run("echo x </nonexistent/td-sh-nope; echo survived");
        assert_eq!(out, "survived\n");
        assert!(err.contains("nonexistent"), "err: {err:?}");
    }

    #[test]
    fn a_failed_redirection_reports_ashs_one_and_not_dashs_two() {
        // The two shells td-sh grades against disagree here -- dash's
        // `redirectsafe` doubles its `setjmp` return and ash's does not -- so this
        // pins the ash-first answer against the plausible other one rather than
        // against nothing. Asserted on all three ways a redirection fails: a target
        // that will not open, a descriptor that is not open, and `set -C` refusing
        // to truncate.
        let kept = std::env::temp_dir().join(format!("td-sh-noclobber-{}", std::process::id()));
        // Unwrapped: `set -C` only refuses a file that EXISTS, so a setup that
        // quietly failed would leave the case asserting nothing about noclobber.
        std::fs::write(&kept, b"").expect("noclobber fixture");
        let noclobber = format!("set -C; echo x >'{}'; echo st=$?", kept.display());
        for src in [
            "echo x >/nonexistent/td-sh-nope/f; echo st=$?",
            "echo x >&7; echo st=$?",
            noclobber.as_str(),
        ] {
            let (_status, out, _) = run(src);
            assert_eq!(out, "st=1\n", "src: {src}");
        }
        let _ = std::fs::remove_file(&kept);
        // And it is the status the shell EXITS with under `-e`, which is the only
        // way the value reaches a caller that never runs `echo $?`.
        let (status, out, _) = run("set -e; echo x >/nonexistent/td-sh-nope/f; echo unreached");
        assert_eq!((status, out.as_str()), (1, ""));
    }

    #[test]
    fn a_subshells_failed_redirection_reports_the_fatal_two() {
        // The exception to the 1 above, and the ONLY one: ash applies a subshell's
        // redirections in the forked child with the plain `redirect`, so the child
        // dies of a fatal shell error rather than reporting a redirection's. Every
        // spelling of a subshell answers 2 -- including as a pipeline STAGE, where
        // the surrounding compound would answer 1 -- and the function form, which
        // is a subshell body rather than the brace one beside it.
        for src in [
            "( : ) </no/such/td-e",
            "( ( : ) </no/such/td-e )",
            "{ ( : ) </no/such/td-e; }",
            "x=$( ( : ) </no/such/td-e )",
            "true | ( : ) </no/such/td-e",
            "f() ( : ) </no/such/td-e; f",
            "( : ) </no/such/td-e & wait $!",
        ] {
            let (_status, out, err) = run(&format!("{src}; echo st=$?"));
            assert_eq!(out, "st=2\n", "src: {src}");
            // The status is the child dying, so the diagnostic must still be the
            // redirection's -- a fatal path that reported 2 and said nothing
            // would be indistinguishable from one that failed for another reason.
            assert_eq!(err, "td-sh: can't open /no/such/td-e: no such file\n", "src: {src}");
        }
        // The contrast, at the same three shapes: everything that is NOT a
        // subshell goes through the equivalent of `redirectsafe` and answers 1.
        for src in [
            "{ :; } </no/such/td-e",
            "true | { :; } </no/such/td-e",
            "f() { :; } </no/such/td-e; f",
        ] {
            let (_status, out, _) = run(&format!("{src}; echo st=$?"));
            assert_eq!(out, "st=1\n", "src: {src}");
        }
        // A body that fails on its own still reports ITS status, so the 2 is the
        // redirection's answer and not every subshell failure's.
        let (_status, out, _) = run("( exit 7 ); echo st=$?");
        assert_eq!(out, "st=7\n");
        // The body must be SKIPPED, which every case above is blind to: each has
        // a silent `:` for a body, so a subshell that ran it and then reported 2
        // anyway satisfies all of them. This one would print.
        let (_status, out, _) = run("( echo RAN ) </no/such/td-e; echo st=$?");
        assert_eq!(out, "st=2\n");
    }

    #[test]
    fn a_backgrounded_compounds_failed_redirection_is_fatal_too() {
        // The OTHER node that reaches ash's forked-child redirect path: `&` wraps
        // its operand in an `NREDIR` only when it is not one already, so a bare
        // compound carrying redirections keeps them and is retyped to `NBACKGND`,
        // which `evalsubshell` runs. Every compound spelling, plus the brace group
        // that is transparent because it carries no redirections of its own.
        for src in [
            "{ :; } </no/such/td-e",
            "if true; then :; fi </no/such/td-e",
            "while false; do :; done </no/such/td-e",
            "until true; do :; done </no/such/td-e",
            "for i in a; do :; done </no/such/td-e",
            "case x in x) :;; esac </no/such/td-e",
            "{ { :; } </no/such/td-e; }",
            "{ { { :; } </no/such/td-e; }; }",
        ] {
            let (_status, out, _) = run(&format!("{src} & wait $!; echo st=$?"));
            assert_eq!(out, "st=2\n", "src: {src}");
        }
        // The controls, which are what make it the NODE's rule. The first six have
        // a node of their own, so `&` wraps them and the redirection stays
        // `redirectsafe` -- `[[ ]]` among them, since busybox ash serves it from
        // `test` in an ordinary `NCMD`. The last three are the transparency
        // boundary: two items inside is an `NSEMI`, an outer group carrying its
        // OWN redirection is itself the node that gets retyped so the inner
        // failure is ordinary, and an inner item that is already `&` is not the
        // single sequential one this sees through.
        for (src, want) in [
            ("true </no/such/td-e", "st=1\n"),
            ("f() { :; }; f </no/such/td-e", "st=1\n"),
            ("( { :; } </no/such/td-e )", "st=1\n"),
            ("{ :; } </no/such/td-e && true", "st=1\n"),
            ("{ true </no/such/td-e; }", "st=1\n"),
            ("true | { :; } </no/such/td-e", "st=1\n"),
            ("[[ -n x ]] </no/such/td-e", "st=1\n"),
            ("! { :; } </no/such/td-e", "st=0\n"),
            ("{ { :; } </no/such/td-e; echo X; }", "X\nst=0\n"),
            ("{ { :; } </no/such/td-e; } >/dev/null", "st=1\n"),
            ("{ { :; } </no/such/td-e & }", "st=0\n"),
        ] {
            let (_status, out, _) = run(&format!("{src} & wait $!; echo st=$?"));
            assert_eq!(out, want, "src: {src}");
        }
        // The same group written over lines. `Sep` has only `Seq` and `Bg`, so a
        // newline is already `Seq` and this holds -- pinned because the
        // transparency test matches that variant by name, and a third one would
        // make every multi-line script take the ordinary path in silence.
        let (_status, out, _) = run("{\n{ :; } </no/such/td-e\n} & wait $!; echo st=$?");
        assert_eq!(out, "st=2\n");
        // And the compound still RUNS when its redirections are fine, which none
        // of the above would notice: the shape is detected by taking the
        // redirections OFF the command, so a bug there loses the body or its
        // output rather than a status.
        let path = std::env::temp_dir().join(format!("td-sh-bgredir-{}", std::process::id()));
        let (_status, out, _) = run(&format!(
            "{{ echo INSIDE; }} >'{p}' & wait $!; echo st=$?; read x <'{p}'; echo got=$x",
            p = path.display()
        ));
        assert_eq!(out, "st=0\ngot=INSIDE\n");
        let _ = std::fs::remove_file(&path);
        let (_status, out, _) = run("{ exit 7; } >/dev/null & wait $!; echo st=$?");
        assert_eq!(out, "st=7\n");
    }

    #[test]
    fn a_backgrounded_compounds_redirections_wait_for_the_guards_around_them() {
        // This arm reaches the redirections before `run_pipeline` and
        // `run_command` do, so it owes what they do around one. `-n` first:
        // applying a redirection IS running, and an output target would be
        // created or truncated by a shell asked only to parse.
        let unmade = std::env::temp_dir().join(format!("td-sh-noexec-{}", std::process::id()));
        let _ = std::fs::remove_file(&unmade);
        let (_status, _out, err) =
            run(&format!("set -n; {{ :; }} >'{}' &", unmade.display()));
        assert!(!unmade.exists(), "-n created the target");
        assert_eq!(err, "");
        // And the line, which the redirections expand under: the target names
        // the compound's OWN line, as it does in every other position. Written
        // on line 3 below, and the bug this pins reported the script's first.
        let dir = std::env::temp_dir().join(format!("td-sh-bgline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let (_status, out, _) = run(&format!(
            ":\n:\n{{ :; }} >\"{}/f$LINENO\" & wait $!\nfor f in '{}'/*; do echo made=${{f##*/}}; done",
            dir.display(),
            dir.display()
        ));
        assert_eq!(out, "made=f3\n");
        let _ = std::fs::remove_dir_all(&dir);
        // And through a transparent brace it is the INNER node's line -- the one
        // ash retypes -- not the group's. Measured: this writes `f3` under ash,
        // where the outer `{` is on line 2.
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let (_status, out, _) = run(&format!(
            ":\n{{\n{{ :; }} >\"{}/f$LINENO\"\n}} & wait $!\nfor f in '{}'/*; do echo made=${{f##*/}}; done",
            dir.display(),
            dir.display()
        ));
        assert_eq!(out, "made=f3\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `io::Error`'s Display is `{strerror} (os error {N})`. The tail is Rust's
    /// and no shell prints it; the head is the system's and every shell does.
    #[test]
    fn an_errno_renders_as_the_system_words_it_and_no_further() {
        for n in [super::ENOENT, super::EEXIST, super::ENOTDIR, 13, 21] {
            let e = std::io::Error::from_raw_os_error(n);
            let text = super::strerror(&e);
            assert!(!text.contains("os error"), "{n}: {text}");
            // And the strip takes the suffix ONLY -- putting it back has to
            // reconstruct Display exactly, or the message itself was truncated.
            assert_eq!(format!("{text} (os error {n})"), e.to_string(), "{n}");
        }
        // No errno, so nothing to strip: the message passes through whole.
        let custom = std::io::Error::other("poisoned stdin");
        assert_eq!(super::strerror(&custom), "poisoned stdin");
        // Even one that CONTAINS the marker, which pins the order rather than
        // the strip: the errno is asked first, so a message Rust did not
        // compose is never searched. Exact-suffix against marker-search is not
        // distinguishable below that -- an errno's Display always ends with its
        // own suffix -- so this is as close as a test gets to it.
        let odd = std::io::Error::other("can't open (os error 2) x");
        assert_eq!(super::strerror(&odd), "can't open (os error 2) x");
    }

    /// Asserted over the diagnostics rather than over the helper. It is a
    /// table, not a sweep: a site absent from it is a site nothing checks.
    #[test]
    fn no_diagnostic_carries_rusts_os_error_number() {
        let dir = std::env::temp_dir().join(format!("td-sh-oserr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("f"), b"x");
        let d = dir.display();
        for src in [
            format!(": < {d}/nope"),
            format!(": > {d}/no/such/dir/x"),
            format!(": >> {d}/no/such/dir/x"),
            format!(": <> {d}/no/such/dir/x"),
            format!(": >| {d}/no/such/dir/x"),
            format!(": > {d}"),
            format!(": < {d}/f/under"),
            format!("set -C; : > {d}/f"),
            "e=; : < $e".to_string(),
            "e=; : > $e".to_string(),
            format!("cd {d}/nope"),
            format!("cd {d}/f"),
            format!(". {d}/nope.sh"),
            format!("source {d}/nope.sh"),
            format!("echo x >&{d}/no/such/dir/x"),
        ] {
            let (_status, _out, err) = run(&src);
            assert!(!err.is_empty(), "src: {src}");
            assert!(!err.contains("os error"), "src: {src}, err: {err:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `.` names the file it OPENED, not the operand -- they differ only when
    /// PATH resolved it, so an operand with a slash in it cannot show this.
    /// The entries are RELATIVE for a second reason: ash reports the
    /// concatenation as WRITTEN, so an absolute one could not tell the name it
    /// reports from the path it opened.
    #[test]
    fn dot_names_the_file_path_resolved_not_the_operand() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("td-sh-dotpath-{}", std::process::id()));
        let sub = dir.join("pdir");
        let _ = std::fs::create_dir_all(&sub);
        let target = sub.join("target.sh");
        let _ = std::fs::write(&target, b"echo sourced\n");
        // Unreadable, so the OPEN fails and the name is what gets reported.
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000));
        for (dir_word, want) in [
            ("PATH=pdir", "pdir/target.sh"),
            ("PATH=./pdir", "./pdir/target.sh"),
        ] {
            let (_s, _o, err) = run(&format!("cd {}; {dir_word}; . target.sh", dir.display()));
            assert_eq!(err, format!("td-sh: .: line 1: can't open '{want}': Permission denied\n"));
        }
        // An empty entry is the cwd and contributes NO prefix.
        let (_s, _o, err) = run(&format!("cd {}; PATH=; . target.sh", sub.display()));
        assert_eq!(err, "td-sh: .: line 1: can't open 'target.sh': Permission denied\n");
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pairs are ash's, measured. A command that cannot be run reports
    /// the system's reason, and a status that turns on WHICH of ash's two
    /// functions failed rather than on the errno.
    #[test]
    fn a_command_that_cannot_be_run_says_why_and_answers_ashs_status() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("td-sh-execwhy-{}", std::process::id()));
        let a = dir.join("a");
        let _ = std::fs::create_dir_all(a.join("dirtool"));
        let _ = std::fs::write(a.join("only"), b"data");
        let _ = std::fs::set_permissions(a.join("only"), std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::write(a.join("tool"), b"x");
        let _ = std::fs::set_permissions(a.join("tool"), std::fs::Permissions::from_mode(0o644));
        let _ = std::os::unix::fs::symlink("loopy", dir.join("loopy"));
        let d = dir.display();
        let ad = a.display();
        for (src, want, code) in [
            // A slash name is handed to the kernel, so a DIRECTORY is EACCES.
            (format!("{ad}"), "Permission denied", 126),
            (format!("{d}/loopy"), "Too many levels of symbolic links", 127),
            (format!("{d}/nope"), "not found", 127),
            // A PATH walk: a regular file it cannot run is 126, a directory 127
            // -- same message, and the status is the only tell.
            (format!("PATH={ad}; only"), "Permission denied", 126),
            (format!("PATH={ad}; dirtool"), "Permission denied", 127),
            (format!("PATH={ad}; zzznope"), "not found", 127),
            // A later entry that does not EXIST must not erase what an earlier
            // one established -- ash only ever overwrites `e` with a more
            // specific errno, never back to "nothing there".
            (format!("PATH={ad}:{d}/no/such; only"), "Permission denied", 126),
            (format!("PATH={d}/no/such:{ad}; only"), "Permission denied", 126),
            (format!("PATH={ad}:{d}/no/such; dirtool"), "Permission denied", 127),
        ] {
            let (status, _o, err) = run(&src);
            let name = src.rsplit(' ').next().unwrap_or(&src);
            assert_eq!(err, format!("td-sh: {name}: {want}\n"), "src: {src}");
            assert_eq!(status, code, "src: {src}");
        }
        // A walk that stat'd nothing runnable still carries WHY.
        let lp = dir.join("lp");
        let _ = std::fs::create_dir_all(&lp);
        let _ = std::os::unix::fs::symlink("tool", lp.join("tool"));
        let noread = dir.join("noread");
        let _ = std::fs::create_dir_all(&noread);
        let _ = std::fs::write(noread.join("tool"), b"x");
        let _ = std::fs::set_permissions(&noread, std::fs::Permissions::from_mode(0o000));
        let ddir = dir.join("dd");
        let _ = std::fs::create_dir_all(ddir.join("tool"));
        let dd = ddir.display();
        let lpd = lp.display();
        let nrd = noread.display();
        for (src, want, code) in [
            (format!("PATH={lpd}; tool"), "Too many levels of symbolic links", 127),
            (format!("PATH={nrd}; tool"), "Permission denied", 127),
            // A later errno REPLACES an earlier one, so the loop decides both
            // the word and the status here -- 127, not the file's 126.
            (format!("PATH={ad}:{lpd}; tool"), "Too many levels of symbolic links", 127),
            // A regular file makes ash's lookup SUCCEED, and a later directory
            // cannot un-succeed it: 126 in both orders.
            (format!("PATH={ad}:{dd}; tool"), "Permission denied", 126),
            (format!("PATH={dd}:{ad}; tool"), "Permission denied", 126),
        ] {
            let (status, _o, err) = run(&src);
            assert_eq!(err, format!("td-sh: tool: {want}\n"), "src: {src}");
            assert_eq!(status, code, "src: {src}");
        }
        // `exec` reads the SAME walk differently: it runs no lookup, so a
        // directory is the errno's own 126 where a plain command is 127.
        let (status, _o, err) = run(&format!("PATH={ad}; exec dirtool"));
        assert_eq!(err, "td-sh: exec: line 1: dirtool: Permission denied\n");
        assert_eq!(status, 126);
        let (status, _o, _e) = run(&format!("PATH={ad}; exec zzznope"));
        assert_eq!(status, 127);
        // A shell that cannot BECOME the command runs it instead, and must
        // still speak as `exec` -- a subshell, a stage, a substitution. The
        // slash name is the one that reaches that path, since a PATH name
        // fails the lookup before it.
        let gone = dir.join("gone");
        let g = gone.display();
        for src in [
            format!("(exec {g})"),
            format!("exec {g} | true"),
            format!("echo $(exec {g})"),
        ] {
            let (_s, _o, err) = run(&src);
            assert_eq!(err, format!("td-sh: exec: line 1: {g}: not found\n"), "src: {src}");
        }
        let _ = std::fs::set_permissions(&noread, std::fs::Permissions::from_mode(0o755));
        // And a non-executable match must still not shadow a real one later.
        // The rows above pin that the walk keeps RECORDING past one; this pins
        // that it would still SELECT a later program, which needs one to run.
        if std::path::Path::new("/bin/sh").exists() {
            let b = dir.join("b");
            let _ = std::fs::create_dir_all(&b);
            let _ = std::fs::write(b.join("only"), b"#!/bin/sh\necho REAL\n");
            let _ =
                std::fs::set_permissions(b.join("only"), std::fs::Permissions::from_mode(0o755));
            let (status, out, err) = run(&format!("PATH={ad}:{}; only", b.display()));
            assert_eq!((status, out.as_str()), (0, "REAL\n"), "err: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The errno a lookup never sees: it comes back from `Command::spawn`
    /// after `resolve_program` has already answered.
    #[test]
    fn a_command_that_cannot_be_spawned_gives_the_systems_reason() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("td-sh-spawnfail-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("noexec");
        let _ = std::fs::write(&f, b"x");
        // A slash in the name, so `resolve_program` hands it over without
        // asking about the execute bit and the KERNEL is what refuses.
        let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644));
        // This harness buffers stdout, so the spawn takes the piped path; the
        // one with real stdio, and `exec`'s own, are asked of the built binary
        // in `tests/conformance.rs`.
        let p = f.display();
        let (status, _o, err) = run(&format!("{p}"));
        assert_eq!(err, format!("td-sh: {p}: Permission denied\n"));
        assert_eq!(status, 126);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the folds `lexer.rs` pins the token shape of actually DO. Every
    /// line below runs under busybox ash 1.37.0.
    #[test]
    fn an_operator_split_by_a_line_continuation_still_runs() {
        // Status and BOTH streams per case, or a diagnostic written beside the
        // right output would pass.
        for (before, after, out, err) in [
            ("true &", "& echo yes", "yes\n", ""),
            ("false |", "| echo yes", "yes\n", ""),
            ("case a in a) echo hit ;", "; esac", "hit\n", ""),
            ("echo hi >", "&2", "", "hi\n"),
            // The here-document pair, and the `-` that strips its body's tabs.
            ("read v <", "<E\nbody\nE\necho $v", "body\n", ""),
            ("read v <<", "-E\n\tbody\nE\necho $v", "body\n", ""),
        ] {
            let src = format!("{before}\\\n{after}");
            assert_eq!(run(&src), (0, out.to_string(), err.to_string()), "{src:?}");
        }
    }

    /// ash reads every character of a `$` construct through `pgetc_top`
    /// (ash.c:11130) -- `pgetc_eatbnl` for every syntax but the single-quoted
    /// one -- so a fold never reaches that construct's syntax. Every line below
    /// produces the same output under busybox ash 1.37.0, and each was a `bad
    /// substitution` or a syntax error here before.
    #[test]
    fn a_line_continuation_is_invisible_inside_a_dollar_construct() {
        let out = |src: &str| run(src).1;
        // Between the `$` and whatever it opens.
        assert_eq!(out("a=1; echo $\\\n{a}"), "1\n");
        assert_eq!(out("a=1; echo $\\\na"), "1\n");
        assert_eq!(out("echo $\\\n(echo sub)"), "sub\n");
        assert_eq!(out("echo $\\\n((1+2))"), "3\n");
        // The corpus case this promotes: `$` and a special parameter, split.
        assert_eq!(out("true; echo $\\\n?"), "0\n");
        // Inside the braces, where the text is collected for a later re-lex.
        assert_eq!(out("a=1; echo ${\\\na}"), "1\n");
        assert_eq!(out("a=1; echo ${a\\\n}"), "1\n");
        assert_eq!(out("echo ${a\\\n-x}"), "x\n");
        assert_eq!(out("a=abc; echo ${#\\\na}"), "3\n");
        assert_eq!(out("a=abc; echo ${a\\\n#a}"), "bc\n");
        assert_eq!(out("set -- p; echo ${\\\n1}"), "p\n");
        assert_eq!(out("a=1; echo ${b-${\\\na}}"), "1\n");
        // Both parens of `$((` and of the `))` that closes it.
        assert_eq!(out("echo $(\\\n(1+2))"), "3\n");
        assert_eq!(out("echo $((1+2)\\\n)"), "3\n");
        // A single-quoted run keeps its backslash and its newline, which is the
        // one syntax ash reads with a plain `pgetc`.
        assert_eq!(out("echo '${a\\\n}'"), "${a\\\n}\n");
        // And a fold with nothing after the `$` spends itself, leaving the `$`
        // as an ordinary character rather than an unfinished construct.
        assert_eq!(out("echo $\\\n"), "$\n");
        // An UNBRACED name is read through folds too, so this names `ab` --
        // splitting it would silently expand a different variable.
        assert_eq!(out("a=X; ab=Y; echo $a\\\nb"), "Y\n");
        assert_eq!(out("a=X; ab=Y; echo \"$a\\\nb\""), "Y\n");
        // The two READ-ONLY lookaheads inside `${...}` have to read the same
        // text the scan will. The first decides whether the body is scanned
        // with quotes demoted, and a fold before the operator flipping that
        // answer is not an error but a different program: here the `'` would
        // wrongly protect the first `}`. The second finds a nested opener, and
        // without it the nested `$( )` is not copied whole and its `}` ends
        // the outer expansion.
        assert_eq!(out("echo \"${u\\\n-'x}y'}\""), "'xy'}\n");
        assert_eq!(out("echo \"${u-$\\\n(printf %s '}')}\""), "}\n");
    }

    /// Input that ENDS in a fold. ash reads it through `pgetc_eatbnl` and then
    /// gets PEOF, so nothing is unfinished: the line simply runs. What must NOT
    /// move with it is the interactive reader's probe, which still asks for
    /// another line -- `parser.rs` pins that half, and this one is the sealed
    /// parse every non-interactive reader ends at -- the STREAMING one reaches
    /// the same decision by another route, and `conformance.rs` drives that.
    /// Statuses measured on ash.
    #[test]
    fn a_fold_at_the_end_of_the_input_is_spent_rather_than_unfinished() {
        for (src, out) in [
            ("echo x \\\n", "x\n"),
            ("echo ab\\\n", "ab\n"),
            ("\\\n", ""),
            ("echo a; echo b \\\n", "a\nb\n"),
        ] {
            assert_eq!(run(src), (0, out.to_string(), String::new()), "{src:?}");
        }
        // An operator the fold left open is still a syntax error -- what ended
        // is the input, not the command. ash exits 2 for both, as this does;
        // only the wording differs, which is its own open item.
        for src in ["echo x >\\\n", "echo x |\\\n"] {
            let (status, out, err) = run(src);
            assert_eq!(status, 2, "{src:?}");
            assert_eq!(out, "", "{src:?}");
            assert!(!err.is_empty(), "{src:?}: the refusal is reported");
        }
    }

    /// Input that ENDS ends a here-document body with it, and the command runs
    /// with what was collected -- ash leaves its body read on PEOF (`case
    /// CENDFILE`) and reports nothing at all. The body is the characters up to
    /// the delimiter LINE, so a last line the input ran out on contributes no
    /// newline. `read` returns non-zero on a line no newline ended, so the
    /// plain loop below DROPS that last line where the guarded idiom keeps it:
    /// that pair is the difference a script can see, and it is what pins the
    /// byte. Every row measured against busybox ash 1.37.0.
    #[test]
    fn input_that_ends_ends_a_here_document_body_with_it() {
        let plain = r#"while read x; do echo "[$x]"; done"#;
        let kept = r#"while read x || [ -n "$x" ]; do echo "[$x]"; done"#;
        for (loop_src, redir, out) in [
            (plain, "<<E", ""),
            (kept, "<<E", ""),
            (plain, "<<E\none", ""),
            (kept, "<<E\none", "[one]\n"),
            (plain, "<<E\none\ntwo", "[one]\n"),
            (kept, "<<E\none\ntwo", "[one]\n[two]\n"),
            // A newline the body really HAS is still real; the delimited body,
            // which both loops answer alike, is the pair's control.
            (plain, "<<E\none\n", "[one]\n"),
            (plain, "<<E\none\nE", "[one]\n"),
            (kept, "<<E\none\nE", "[one]\n"),
            // A quoted delimiter and `<<-`'s stripping reach the last line like
            // any other, and an unquoted body expands to the end.
            (kept, "<<'E'\n$y", "[$y]\n"),
            (kept, "<<-E\n\tone", "[one]\n"),
            (kept, "<<E\nv=$y", "[v=Y]\n"),
            // Two bodies end at once, and the LAST redirect is the one read.
            (kept, "<<A <<B\none", ""),
            (kept, "<<A <<B\none\nA\ntwo", "[two]\n"),
        ] {
            let src = format!("y=Y; {loop_src} {redir}");
            assert_eq!(run(&src), (0, out.to_string(), String::new()), "{src:?}");
        }
        // Only the LAST body runs off the end: one that found its delimiter is
        // still bounded by it.
        let two = format!("{plain} <<A\none\nA\n{kept} <<B\ntwo");
        assert_eq!(run(&two), (0, "[one]\n[two]\n".to_string(), String::new()));
        // The command's own status, not the reader's.
        assert_eq!(run("false <<E\nbody").0, 1);
        // A body that ended at EOF is still a body, so an unfinished construct
        // inside it is a syntax error exactly as in a delimited one; so is a
        // compound the body did not close, and a `<<` whose delimiter word never
        // arrived was never a body at all. ash exits 2 for all eight.
        //
        // The parent exited 2 for all eight as well, so the STATUS alone pins
        // nothing here -- what changed is which refusal it is. Each of these
        // now reports the thing that is actually unfinished, where before the
        // here-document swallowed the question and answered for it.
        for src in [
            "cat <<E\n${a",
            "cat <<E\n$((1+2",
            "cat <<E\n$(echo hi",
            "cat <<E\n`echo hi",
            "{ cat <<E\none",
            "if true; then cat <<E\none",
            "cat <<",
            "cat <<-",
        ] {
            let (status, out, err) = run(src);
            assert_eq!(status, 2, "{src:?}");
            assert_eq!(out, "", "{src:?}");
            assert!(!err.is_empty(), "{src:?}: the refusal is reported");
            assert!(!err.contains("delimited by"), "{src:?}: {err}");
        }
    }

    /// Where a body ends turns on whether the text OWNS its own end. `eval` and
    /// a backquote body are each a complete text, so a body left open in one
    /// ends with it -- ash agrees, reading a backquote body as a STRING before
    /// re-lexing it (`PARSEBACKQOLD`) where it parses `$( )` inline with the
    /// enclosing input.
    ///
    /// An alias replacement does NOT own its end: ash pushes it onto the input
    /// and reads the body from the lines that follow. Splicing TOKENS cannot do
    /// that, so an open body there is REFUSED rather than given an empty one --
    /// otherwise the script's here-document DATA comes back as commands.
    ///
    /// `$( )` refuses, and for ash's OWN reason rather than by policy: the
    /// here-document body swallows the `)`, so the substitution never closes.
    /// That the two agree is not luck -- a body is collected before a closer is
    /// looked for either way -- and it holds on both sides of the boundary
    /// where the models could part, the delimiter falling inside the parens or
    /// after them.
    #[test]
    fn only_a_text_that_owns_its_end_ends_a_here_document_body() {
        let l = r#"while read x || [ -n "$x" ]; do echo [$x]; done"#;
        for (src, want) in [
            // Complete texts: the body ends with them, as ash has it.
            (format!("eval '{l} <<E\none'"), "[one]\n"),
            (format!("v=`{l} <<E\none`; echo \"v=[$v]\""), "v=[[one]]\n"),
            (format!("v=`{l} <<E`; echo \"v=[$v]\""), "v=[]\n"),
            // A substitution with no body LINE at all agrees with ash too: its
            // body would start on a line the parens do not reach.
            (format!("v=$({l} <<E); echo \"v=[$v]\""), "v=[]\n"),
            // A backquote body inside an alias replacement is still its own
            // text, so it ends there as well.
            (format!("alias e='v=`{l} <<E\none`; echo \"v=[$v]\"'\ne"), "v=[[one]]\n"),
        ] {
            assert_eq!(run(&src), (0, want.to_string(), String::new()), "{src:?}");
        }
        // A `$( )` whose body runs PAST the `)` does not close, written plainly
        // or through an alias. ash exits 2 for both.
        for src in [
            format!("v=$({l} <<E\none); echo \"v=[$v]\""),
            format!("alias e='v=$({l} <<E\none); echo \"v=[$v]\"'\ne"),
        ] {
            let (status, out, err) = run(&src);
            assert_eq!((status, out.as_str()), (2, ""), "{src:?}");
            // The REASON, not merely that something failed: an unrelated error
            // reaching stderr would satisfy a non-empty check.
            assert!(err.contains("unmatched `$(`"), "{src:?}: {err}");
        }
        // A `.` script is the third text that owns its end, and takes a file to
        // be one. What it pins beyond the body is that the SOURCING script
        // carries on afterwards, which is ash's answer too.
        let dir = std::env::temp_dir().join(format!("td-sh-heredoc-eof-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("sourced.sh");
        let _ = std::fs::write(&script, format!("{l} <<E\none"));
        assert_eq!(
            run(&format!(". {}\necho after", script.display())),
            (0, "[one]\nafter\n".to_string(), String::new())
        );
        let _ = std::fs::remove_dir_all(&dir);
        // A splice keeps the refusal, whether or not the delimiter turns up on
        // a later line: neither body is text that scan holds.
        for src in [
            format!("alias e='{l} <<E'\ne\none"),
            format!("alias e='{l} <<E'\ne\none\nE\necho done"),
        ] {
            let (status, out, err) = run(&src);
            assert_eq!((status, out.as_str()), (2, ""), "{src:?}");
            assert!(err.contains("alias `e'"), "{src:?}: {err}");
        }
    }

    /// A `)` the body's own syntax has ALREADY accounted for does not close
    /// the substitution. The closer is the one the body's own TOKENS reach, so
    /// a `)` inside a comment, a here-document body, an arithmetic shift or a
    /// `${...}` operand is not one. Measured against busybox ash 1.37.0.
    #[test]
    fn a_paren_the_body_has_accounted_for_does_not_close_a_substitution() {
        let l = r#"while read x || [ -n "$x" ]; do echo [$x]; done"#;
        for (src, want) in [
            ("echo $(echo hi # )\n)".to_string(), "hi\n"),
            ("echo $(echo hi # ))\n)".to_string(), "hi\n"),
            ("echo $(echo hi # )\necho bye)".to_string(), "hi bye\n"),
            // A `#` INSIDE a word is not a comment, and one inside quotes is
            // not either -- the same rule the main lexer uses, which is why the
            // body means the same thing when it is lexed again.
            ("echo $(echo a#b)".to_string(), "a#b\n"),
            ("echo $(echo \"a # )b\")".to_string(), "a # )b\n"),
            // A here-document body, plain, with a quoted delimiter, and `<<-`.
            (format!("echo $({l} <<E\n)\nE\n)"), "[)]\n"),
            (format!("echo $({l} <<'E'\n)\nE\n)"), "[)]\n"),
            (format!("echo $({l} <<-E\n\t)\n\tE\n)"), "[)]\n"),
            // A `(` in a body is not an opener either.
            (format!("echo $({l} <<E\n(\nE\n)"), "[(]\n"),
            // TWO bodies deferred at once, and the second is the one read.
            (format!("echo $({l} <<A <<B\nx\nA\n)\nB\n)"), "[)]\n"),
            // The operator names a body that does not begin until the NEXT
            // line, so a `)` on the operator's own line still closes.
            (format!("v=$({l} <<E); echo \"v=[$v]\""), "v=[]\n"),
            // A `<<` that is not an operator at all: an arithmetic SHIFT, and
            // one inside a `${...}` operand. Both read as raw text by anything
            // that does not lex, and both then eat the rest of the script.
            ("echo $(echo $((1<<3))\necho done)".to_string(), "8 done\n"),
            ("echo $(echo $(( 1 << 3 ))\necho done)".to_string(), "8 done\n"),
            ("echo $(echo ${x:-a<<b}\necho two)".to_string(), "a<<b two\n"),
            // ... and a `#` that is not a comment: inside a `${...}` operand,
            // and after a QUOTED blank, which leaves a word still open.
            ("x=abc; echo $(echo ${x:- #y})".to_string(), "abc\n"),
            ("echo $(echo a\\ #c)".to_string(), "a #c\n"),
            // A here-document delimiter folded across a line: `E\<newline>OF`
            // is the delimiter `EOF`, which no body line can equal if the fold
            // is left in it.
            (format!("v=$({l} <<E\\\nOF\nbody\nEOF\n); echo \"v=[$v]\""), "v=[[body]]\n"),
        ] {
            assert_eq!(run(&src), (0, want.to_string(), String::new()), "{src:?}");
        }
        // What must still be refused: a substitution that never closes, however
        // the scan got there. ash exits 2 for each of these too.
        for src in [
            "echo $(echo hi".to_string(),
            "echo $(echo hi #".to_string(),
            format!("echo $({l} <<E"),
            format!("echo $({l} <<E\nx"),
        ] {
            let (status, out, err) = run(&src);
            assert_eq!((status, out.as_str()), (2, ""), "{src:?}");
            assert!(err.contains("unmatched `$(`"), "{src:?}: {err}");
        }
    }

    /// A reserved word that only ever CLOSES a construct is a syntax error
    /// where a command was expected, not a command named `fi`. The status is
    /// the point as much as the text: ash refuses the whole input with 2 and
    /// runs none of it, where a command name would be 127 after everything
    /// before it had already run -- and inside a loop, forever. Measured
    /// against busybox ash 1.37.0, whose wording this matches byte for byte.
    #[test]
    fn a_word_that_only_closes_a_construct_cannot_start_a_command() {
        for w in ["then", "else", "elif", "fi", "do", "done", "esac", "in", "}"] {
            let (status, out, err) = run(w);
            assert_eq!((status, out.as_str()), (2, ""), "{w}");
            assert_eq!(err, format!("td-sh: syntax error: unexpected \"{w}\"\n"), "{w}");
        }
        // Every position that is a command position, including the one that
        // would otherwise LOOP: `fi` as a command is not found over and over.
        for (src, want) in [
            ("( fi )", "fi"),
            ("{ fi; }", "fi"),
            ("true && fi", "fi"),
            ("true | fi", "fi"),
            ("while :; do fi; done", "fi"),
            ("if :; then done; fi", "done"),
            // A name is read as one only after the word is allowed to be a
            // command at all, so the DEFINITION is refused -- with nothing
            // after it, or the refusal could be of the call instead.
            ("fi() { echo x; }", "fi"),
            ("done() { echo x; }", "done"),
        ] {
            let (status, out, err) = run(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src:?}");
            // The TOKEN, not merely that something was unexpected.
            assert!(err.contains(&format!("unexpected \"{want}\"")), "{src:?}: {err}");
        }
        // The whole input is refused, so what precedes the word does not run.
        assert_eq!(run("echo a; done").1, "", "nothing before the error runs");
        // And where it is NOT a command position it is an ordinary word: an
        // argument, an assignment's value, a name, or quoted. ash agrees with
        // every row -- `x=1 fi` really does look for a command called `fi`.
        for (src, want) in [
            ("echo fi", "fi\n"),
            ("fi=1; echo $fi", "1\n"),
            ("for fi in 1; do echo $fi; done", "1\n"),
            ("echo done esac in", "done esac in\n"),
        ] {
            assert_eq!(run(src), (0, want.to_string(), String::new()), "{src:?}");
        }
        for src in ["x=1 fi", "\"fi\"", ">/dev/null fi", "2>&1 fi"] {
            assert_eq!(run(src).0, 127, "{src:?} is a command name, not a keyword");
        }
        // An arm's terminator position: refused, though ash adds `(expecting
        // ";;")` to the text and this does not.
        let (status, _, err) = run("case x in x) fi;; esac");
        assert_eq!(status, 2);
        assert!(err.contains("unexpected \"fi\""), "{err}");
        // A substitution BODY is not refused: ash reads an old-style `` `…` ``
        // with `list(2)`, which ends its list at one of these and never checks,
        // so the script runs. Refusing would stop one ash runs.
        for src in ["echo `fi`", "echo `echo a; done`", "echo `}`"] {
            assert_eq!(run(src).0, 0, "{src:?} must not stop the script");
        }
        // `}` is a closing word again because `function name { … }` is a
        // construct now; before it was, refusing that `}` stopped a script
        // that only CONTAINED one in a branch never taken.
        assert_eq!(
            run("if false; then function f { :; }; fi; echo alive"),
            (0, "alive\n".to_string(), String::new())
        );
    }

    /// `function NAME [()] <compound>` is bash's spelling and ash takes it, the
    /// reference build carrying BASH_FUNCTION. It is what lets `}` be a closing
    /// word: without the construct, the brace ending one of these reached a
    /// command position and refusing it stopped the whole script.
    /// Measured against busybox ash 1.37.0.
    #[test]
    fn the_function_keyword_defines_one_as_the_bare_name_does() {
        for (src, want) in [
            ("function f { echo x; }; f", "x\n"),
            ("function f() { echo x; }; f", "x\n"),
            // Any compound command is a body, not only a group.
            ("function f case x in x) echo c;; esac; f", "c\n"),
            ("function f if :; then echo i; fi; f", "i\n"),
            ("function f for i in 1; do echo $i; done; f", "1\n"),
            ("function f while false; do :; done; f", ""),
            ("function f until :; do :; done; f", ""),
            ("function f [[ -n x ]]; f", ""),
            // ash reads what follows the name with CHKNL, so the `()` and the
            // body may each start on the next line.
            ("function f\n{ echo x; }\nf", "x\n"),
            ("function f\n() { echo x; }\nf", "x\n"),
            ("function f ()\n{ echo x; }\nf", "x\n"),
            // The body takes redirections, as the bare spelling's does.
            ("function f { echo x; } >/dev/null\nf; echo done", "done\n"),
            // `=` makes an assignment only where a NAME precedes it, so this
            // name is not one and does define.
            ("function 1f=b { echo x; }; 1f=b", "x\n"),
            // ash reads the NAME with CHKALIAS and not CHKKWD, so an alias
            // fires there -- even one named for a keyword -- and may supply the
            // `()` and the body with it.
            ("alias N=f\nfunction N { echo BODY; }\nunalias N\nf", "BODY\n"),
            ("alias fi=g\nfunction fi { echo BODY; }\nunalias fi\ng", "BODY\n"),
            ("alias N=\"f() { echo BODY; }\"\nfunction N\nf", "BODY\n"),
            // The BODY position takes one only through the `()` spelling, which
            // rejoins the ordinary path; without it ash reads that token with
            // no CHKALIAS at all.
            ("alias B='{ echo BODY; }'\nfunction f() B\nf", "BODY\n"),
            // The NAME is any word here, where the bare spelling needs a
            // `is_name` one -- both of these define under ash and neither is
            // a name.
            ("function 1f { echo x; }; 1f", "x\n"),
            ("function f-g { echo x; }; f-g", "x\n"),
            // ... and `function` is a keyword only where a command starts.
            // `type`/`command -v` report it as one, and an alias may not take
            // its place, which is what `is_reserved` is for.
            ("type function", "function is a shell keyword\n"),
            ("command -v function", "function\n"),
            ("echo function", "function\n"),
            ("function=1; echo $function", "1\n"),
            ("echo function f", "function f\n"),
        ] {
            assert_eq!(run(src), (0, want.to_string(), String::new()), "{src:?}");
        }
        // A RESERVED word is a name here, where the bare spelling refuses it:
        // `function fi { …; }` defines one under ash and `fi() { …; }` does
        // not, the second's name sitting at a command position. Calling it
        // still needs the keyword not to win, which it always does.
        assert_eq!(
            run("function fi { echo x; }; echo defined"),
            (0, "defined\n".to_string(), String::new())
        );
        assert_eq!(run("fi() { echo x; }").0, 2, "the bare spelling refuses it");
        // A QUOTED or EXPANDED name is taken as readily -- refusing would stop
        // a whole script ash runs -- and so is one carrying a `/`. None of the
        // three can be called afterwards.
        for src in [
            "function \"myfunc\" { echo x; }; echo after",
            "function $undef { echo x; }; echo after",
            "function f/g { echo x; }; echo after",
        ] {
            assert_eq!(run(src), (0, "after\n".to_string(), String::new()), "{src:?}");
        }
        for src in ["function \"myfunc\" { echo x; }; myfunc", "function f/g { echo x; }; f/g"] {
            assert_eq!(run(src).0, 127, "{src:?}");
        }
        assert_eq!(run("function f/g { echo x; }; type f/g").0, 127);
        // What is measured against ash here is that the three spellings AGREE;
        // the number is dash's, whose relative rule this shell follows for a
        // definition's line, and ash says 4.
        for def in ["function f", "function f ()", "f()"] {
            assert_eq!(run(&format!(":\n:\n{def}\n{{\n  echo $LINENO\n}}\nf")).1, "3\n", "{def}");
        }
        // Without the parentheses the body must OPEN with one of ash's seven
        // words, which a nested definition, a `!` and a redirection are not;
        // `(` after the name opens the OPTIONAL `()`, so a subshell cannot be
        // written as a body there -- ash refuses the word inside it. An
        // assignment-shaped name is refused too, ash filing it as a variable
        // rather than as the definition's one argument.
        for src in [
            "function f echo x",
            "function f (echo s)",
            "function f\n( echo s )",
            "function f function g { echo x; }",
            "function f f2() { echo x; }",
            "function f ! true",
            "function f >out { echo x; }",
            "function a=b { echo x; }",
            "function f= { echo x; }",
            // An assignment whose VALUE is quoted or expanded is still one:
            // ash asks of the word, and so must this.
            "function a=\"$x\" { echo x; }",
            "function a=$x { echo x; }",
            "alias B='{ echo BODY; }'\nfunction f B",
            "alias function=echo\nfunction hi\necho AFTER",
            "function ;",
            "function | cat",
            "function\n{ echo x; }",
        ] {
            let (status, out, err) = run(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src:?}");
            // A HARD error, not the marker the interactive reader reads as a
            // request for another line: no continuation rescues `function ;`,
            // and PS2 would ask for one forever.
            assert!(err.contains("syntax error"), "{src:?}: {err}");
            assert!(!err.contains(crate::ast::INCOMPLETE), "{src:?}: {err}");
        }
        // Only an input that ENDED is incomplete. A newline may follow
        // `function f` and complete it; after the bare word one may NOT, and
        // the marker means only that the name has not arrived -- the
        // interactive reader has appended the newline before it probes, so a
        // prompt takes the hard error above.
        for src in ["function", "function f"] {
            let (status, _, err) = run(src);
            assert_eq!(status, 2, "{src:?}");
            assert!(err.contains(crate::ast::INCOMPLETE), "{src:?}: {err}");
        }
        // WITH the parentheses ash rejoins the ordinary path, so the two
        // spellings answer a body together -- including where this shell still
        // refuses a simple one that ash takes.
        assert_eq!(run("function f() echo x").0, run("f() echo x").0);
    }

    /// A `)` that ends a case PATTERN closes no substitution. It is the one
    /// thing a `)` token cannot say for itself, so the walk that finds the
    /// closer carries the little of the grammar that answers it -- and the
    /// answer has to hold in both directions, since reading `case` where it is
    /// an ordinary word would run the scan past the closer instead.
    /// Measured against busybox ash 1.37.0.
    #[test]
    fn a_case_patterns_paren_does_not_close_a_substitution() {
        let l = r#"while read x || [ -n "$x" ]; do echo [$x]; done"#;
        // A here-document body is the other region only the lexer sees; its `(`
        // used to cancel the pattern's `)` in a count, and cancels nothing now.
        assert_eq!(
            run(&format!("echo $({l} <<E\n(\nE\ncase x in x) echo p;; esac)")),
            (0, "[(] p\n".to_string(), String::new())
        );
        for (src, want) in [
            ("echo $(case x in x) echo hit;; esac)", "hit\n"),
            ("echo $(case x in\nx) echo hit;;\nesac)", "hit\n"),
            // The optional opener, alternation, several arms, the default arm.
            ("echo $(case x in (x) echo hit;; esac)", "hit\n"),
            ("echo $(case x in a|x) echo hit;; esac)", "hit\n"),
            ("echo $(case y in x) echo a;; y) echo b;; esac)", "b\n"),
            ("echo $(case z in x) echo a;; *) echo d;; esac)", "d\n"),
            // An arm with no `;;` before `esac`, and a case with no arm at all.
            ("echo $(case x in x) echo hit; esac)", "hit\n"),
            ("echo $(case x in esac; echo after)", "after\n"),
            // A `)` inside an arm that IS a closer -- a subshell's -- and a
            // whole nested case, whose patterns are pinned to their own depth.
            ("echo $(case x in x) (echo sub);; esac)", "sub\n"),
            (
                "echo $(case x in x) case y in y) echo deep;; esac;; esac)",
                "deep\n",
            ),
            // A case that a command can start before: after `then`, after `do`,
            // and after a pattern's own `)`, which has no separator after it.
            ("echo $(if :; then case x in x) echo t;; esac; fi)", "t\n"),
            ("echo $(for i in 1; do case x in x) echo d;; esac; done)", "d\n"),
            // The substitution keeps closing where it did: after `esac`, with a
            // command still to come, and around a nested substitution.
            ("echo $(case x in x) echo hit;; esac; echo tail)", "hit tail\n"),
            ("echo $(case x in x) echo $(echo in);; esac)", "in\n"),
            // `(` that only the LEXER hides: a `${...}` operand and a
            // here-document body. A count sees them and they used to cancel the
            // pattern's `)`; nothing cancels it now, so the rule above is what
            // these turn on.
            ("echo $(echo ${x:-(}; case x in x) echo p;; esac)", "( p\n"),
            // ... and the other direction: `case` is a reserved word only where
            // a command could start, so none of these opens one.
            ("echo $(echo case)", "case\n"),
            ("echo $(echo esac)", "esac\n"),
            ("echo $(echo case x in)", "case x in\n"),
            // ... and a word that opens a command only where it is one ITSELF.
            // `echo if` leaves ash at an argument, so the `case` after it is an
            // argument too, and the `)` is the closer it always was.
            ("echo $(echo if case x in a)", "if case x in a\n"),
            ("echo $(echo then case x in)", "then case x in\n"),
            ("echo $(echo { case x in a)", "{ case x in a\n"),
            // `esac` likewise: it ends an arm only from a command position, so
            // one echoed inside an arm leaves the case running.
            ("echo $(case y in x) echo esac;; y) echo hit;; esac)", "hit\n"),
            // ... and it is only RESERVED where a pattern may start. Past a `(`
            // or a `|` it is a pattern word, which ash checks no keyword at.
            ("echo $(case esac in (esac) echo hit;; esac)", "hit\n"),
            ("echo $(case esac in a|esac) echo hit;; esac)", "hit\n"),
            ("echo $(case esac in \"esac\") echo hit;; esac)", "hit\n"),
            // A function header's `()` is the other place a command starts with
            // no separator before it -- a body needs no braces to be a case.
            ("echo $(f() case x in x) echo yes;; esac; f)", "yes\n"),
            ("echo $(f() { case x in x) echo y;; esac; }; f)", "y\n"),
            ("echo $(echo \"case x in\")", "case x in\n"),
            ("case=1; echo $(echo $case)", "1\n"),
        ] {
            assert_eq!(run(src), (0, want.to_string(), String::new()), "{src:?}");
        }
        // A case the body never closes is still a refusal, as it is for ash.
        for src in ["echo $(case x in x) echo hit)", "echo $(case x in x"] {
            let (status, out, err) = run(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src:?}");
            assert!(!err.is_empty(), "{src:?}");
        }
    }

    /// Finding the closer is a LEXING pass of its own, so it can recurse and it
    /// can fail. Both exits route to the paren count, which does neither.
    #[test]
    fn a_scan_that_cannot_finish_leaves_the_report_where_it_was() {
        // Twice the depth at which an unbounded scan ran out of stack in the
        // profile the gate builds. What must survive is the DIAGNOSTIC: the
        // depth guard reporting, not an abort.
        let deep = format!("echo {}:{}", "$(".repeat(3_000), ")".repeat(3_000));
        let (status, _, err) = run(&deep);
        assert_eq!(status, 0, "{err}");
        assert!(err.contains("maximum recursion depth"), "{err}");
        // `$'...'` escapes its own closing quote, so the count has to know it or
        // the body ends at a quote that is the end of nothing. The bad delimiter
        // is what forces the fallback this reaches it through.
        let (status, out, err) = run("printf 'before\\n'; echo $(cat <<$x; printf %s $'a\\'b')");
        assert_eq!((status, out.as_str()), (2, "before\n"), "{err}");
        assert!(err.contains("delimiter may not be an expansion"), "{err}");
        // A FATAL body error refuses the whole text rather than the body alone,
        // so what is ahead of the substitution does not run. ash agrees.
        let (status, out, _) = run("printf 'before\\n'; echo $(cat <<E\n${\nE\n)");
        assert_eq!((status, out.as_str()), (2, ""));
    }

    /// Only `<` reaches ash's `eopen`; `>`, `>>`, `<>`, `>|` and the `>&word`
    /// that writes both streams all reach `ecreate`. That is one choice made
    /// six times, so it is pinned as a table -- the same ENOENT, told apart
    /// only by which spelling asked.
    #[test]
    fn every_redirect_spelling_takes_the_word_ash_gives_it() {
        let dir = std::env::temp_dir().join(format!("td-sh-spelling-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = format!("{}/no/such/dir/x", dir.display());
        for op in ["<", ">", ">>", "<>", ">|"] {
            let (_s, _o, err) = run(&format!(": {op} {p}"));
            let want = if op == "<" {
                format!("td-sh: can't open {p}: no such file\n")
            } else {
                format!("td-sh: can't create {p}: nonexistent directory\n")
            };
            assert_eq!(err, want, "op: {op}");
        }
        // `>&word` on fd 1 is a create too, and the one whose spelling gives no
        // hint of it -- it looks like a dup. `&>word` and a NONNUMERIC `1<&word`
        // reach the same place, so all three are asked.
        for src in [format!("echo x >&{p}"), format!("echo x &>{p}"), format!("echo x 1<&{p}")] {
            let (_s, _o, err) = run(&src);
            assert_eq!(err, format!("td-sh: can't create {p}: nonexistent directory\n"), "src: {src}");
        }
        // `set -C` opens through `noclobber_open` instead, whose OWN two open
        // arms carry the word -- and neither is reached by the loop above,
        // since it runs with the option off. The target that does not exist
        // takes the `create_new` arm; a directory takes the re-check arm.
        let adir = dir.join("d");
        let _ = std::fs::create_dir_all(&adir);
        let ad = adir.display();
        for (src, want) in [
            (format!("set -C; : > {p}"), format!("td-sh: can't create {p}: nonexistent directory\n")),
            (format!("set -C; echo x >&{p}"), format!("td-sh: can't create {p}: nonexistent directory\n")),
            (format!("set -C; : > {ad}"), format!("td-sh: can't create {ad}: Is a directory\n")),
            (format!("set -C; echo x >&{ad}"), format!("td-sh: can't create {ad}: Is a directory\n")),
        ] {
            let (_s, _o, err) = run(&src);
            assert_eq!(err, want, "src: {src}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ash_vmsg` gates the component AND the line on `commandname`, so what a
    /// diagnostic says about WHERE depends on whether a builtin is running --
    /// not on the message. A builtin sets it (ash.c:10670) and nothing else in
    /// this harness does, which is the `-c` shape: a failure that enters no
    /// builtin reports neither half.
    #[test]
    fn only_a_running_builtin_gives_a_diagnostic_a_place() {
        // The builtin names itself and the line.
        assert_eq!(run("cd /nope/x").2, "td-sh: cd: line 1: can't cd to /nope/x: No such file or directory\n");
        // The line is the COMMAND's, not a constant.
        assert_eq!(run(":\n:\ncd /nope/x").2, "td-sh: cd: line 3: can't cd to /nope/x: No such file or directory\n");
        // Inside a function it is whatever `$LINENO` answers, which this shell
        // measures from the DEFINITION as dash does and ash measures
        // absolutely. Defined below line 1 on purpose: with `f() {` on line 1
        // the two rules agree and the divergence is invisible.
        assert_eq!(
            run(":\n:\nf() {\n  cd /nope/x\n}\nf").2,
            "td-sh: cd: line 2: can't cd to /nope/x: No such file or directory\n"
        );
        // Neither half for a failure that enters no builtin: a command that is
        // not found, and a redirection that cannot be opened.
        assert_eq!(run("nosuchcmd_xyz").2, "td-sh: nosuchcmd_xyz: not found\n");
        assert_eq!(run(": < /nope/x").2, "td-sh: can't open /nope/x: no such file\n");
        // A readonly refusal is raised by the assignment rather than by a
        // builtin, so it is in that second group even though `readonly` made it.
        assert_eq!(run("readonly r=1\nr=2").2, "td-sh: r: is read only\n");
    }

    /// A sourced file is `commandname` for its duration (ash.c:13739), so a
    /// failure inside one names the FILE rather than the `.` that read it --
    /// and the line is the file's own, not the caller's.
    #[test]
    fn a_sourced_file_is_the_name_a_failure_inside_it_carries() {
        let dir = std::env::temp_dir().join(format!("td-sh-srcname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else { return };
        let inner = dir.join("inner.sh");
        let _ = std::fs::write(&inner, ":\n:\nnosuchcmd_xyz\n");
        let path = inner.to_string_lossy().into_owned();
        // The component is the file even though `nosuchcmd_xyz` entered no
        // builtin: what set `commandname` was the `.`, and it stays set for
        // everything the file runs.
        assert_eq!(
            run(&format!(":\n. {path}")).2,
            format!("td-sh: {path}: line 3: nosuchcmd_xyz: not found\n")
        );
        // And it is given BACK afterwards, so the same failure outside the file
        // reports neither half again.
        assert_eq!(
            run(&format!(". {path}\nnosuchcmd_xyz")).2,
            format!("td-sh: {path}: line 3: nosuchcmd_xyz: not found\ntd-sh: nosuchcmd_xyz: not found\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `cd` and `.` report through ash's `perror` rather than its `errmsg`, so
    /// they take the system's word for ENOENT where a redirection substitutes.
    #[test]
    fn cd_and_dot_give_the_reason_the_system_gives() {
        let dir = std::env::temp_dir().join(format!("td-sh-whyfail-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("f"), b"x");
        let d = dir.display();
        // Missing, and NOT `no such file`: that word is the redirection's.
        let (_s, _o, err) = run(&format!("cd {d}/nope"));
        assert_eq!(err, format!("td-sh: cd: line 1: can't cd to {d}/nope: No such file or directory\n"));
        // Resolves and is not a directory -- the arm no syscall answers, since
        // this shell's cwd is a variable and there is no `chdir` to fail.
        let (_s, _o, err) = run(&format!("cd {d}/f"));
        assert_eq!(err, format!("td-sh: cd: line 1: can't cd to {d}/f: Not a directory\n"));
        // `.` quotes the name, which nothing else in the shell does, and both
        // spellings of the word name themselves.
        for word in ["source", "."] {
            let (_s, _o, err) = run(&format!("{word} {d}/nope.sh"));
            let want = format!("td-sh: {word}: line 1: can't open '{d}/nope.sh': No such file or directory\n");
            assert_eq!(err, want);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A diagnostic names the shell whichever sink it came out of: ash writes
    /// `arg0` before all but the four below (`ash_vmsg`, ash.c:1419). The cases
    /// here reach four distinct `diag` call sites -- a builtin's `err_line`, a
    /// redirection, `Shell::fatal` (which arithmetic shares with expansion),
    /// and the not-found path -- each of which used to decide the prefix for
    /// itself, which is how most came to lack it.
    #[test]
    fn every_diagnostic_names_the_shell() {
        for src in [
            "cd /nonexistent/dir",
            "unset -z x",
            "cat < /nonexistent/x",
            ": ${undefined_here?msg}",
            "echo $((1/0))",
            "no_such_command_at_all_xyz",
            "readonly r=1; r=2",
        ] {
            let (_s, _o, err) = run(src);
            assert!(err.starts_with("td-sh: "), "{src}: {err:?}");
            // Once, not twice: a sink that kept its own literal would double it,
            // which is the shape this change had to remove from five call sites.
            assert!(!err.starts_with("td-sh: td-sh: "), "{src}: {err:?}");
        }
        // xtrace shares fd 2 and is NOT a diagnostic, so it takes no name. It is
        // the boundary a prefix moved into the wrong sink would corrupt, and the
        // corpus asserts these bytes in fifteen places.
        let (_s, _o, err) = run("set -x; :");
        assert_eq!(err, "+ :\n");
        // Four messages are the other exception, and the one this change first
        // got wrong: busybox writes these with a bare `fprintf` rather than
        // through `ash_vmsg`, so they take no name. The wording is identical
        // either way, so only a comparison against ash -- or this -- catches a
        // prefix arriving on them.
        for (src, want) in [
            ("set -- -Z; getopts a: o", "Illegal option -Z\n"),
            ("set -- -a; getopts a: o", "No arg for -a option\n"),
            ("alias nosuchalias", "alias: nosuchalias not found\n"),
            ("unalias nosuchalias", "unalias: nosuchalias not found\n"),
        ] {
            let (_s, _o, err) = run(src);
            assert_eq!(err, want, "{src}");
        }
    }

    #[test]
    fn an_empty_redirection_target_does_not_open_the_current_directory() {
        // The tell is the command RUNNING, not the diagnostic: joining "" onto the
        // cwd yields the cwd, which opens for reading, so this printed `RAN` with a
        // directory on fd 0. The write direction failed either way and only ever
        // said the wrong thing about why.
        let (_status, out, err) = run("e=; echo RAN <\"$e\"; echo after");
        assert_eq!(out, "after\n");
        // Whole messages, because this target is exactly where the read and
        // create wordings differ: one ENOENT, two answers.
        assert_eq!(err, "td-sh: can't open : no such file\n");
        let (_status, _out, err) = run("e=; echo x >\"$e\"");
        assert_eq!(err, "td-sh: can't create : nonexistent directory\n");
        // A target that really IS a directory keeps the SYSTEM's answer, which
        // is the half of `errmsg` that does not substitute.
        let (_status, _out, err) = run("echo x >/");
        assert_eq!(err, "td-sh: can't create /: Is a directory\n");
    }

    #[test]
    fn the_whole_redirect_list_is_settled_before_anything_opens() {
        // ash's `expredir` walks the WHOLE list and only then lets `redirect`
        // open anything (ash.c:9621 vs 5831), so a target that cannot be
        // classified stops the command before an EARLIER redirection has
        // truncated its file or moved the descriptor the diagnostic goes to.
        // All three assertions below are one fact seen from three sides.
        let dir = std::env::temp_dir().join(format!("td-sh-twophase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let victim = dir.join("victim");
        let keep = || {
            let Ok(()) = std::fs::write(&victim, b"KEEP") else {
                panic!("fixture");
            };
        };
        let v = victim.display();
        // The file of an earlier redirection is not truncated.
        keep();
        let (status, _o, _e) = run(&format!(": >'{v}' 2>&/nope/x"));
        assert_eq!(status, 2);
        let left = std::fs::read_to_string(&victim).unwrap_or_default();
        assert_eq!(left, "KEEP");
        // The OTHER fatal spelling takes the same phase. `bad fd number` is
        // pinned elsewhere for its status, but every case there is its
        // command's only redirection, so none of them can tell which phase
        // raised it -- and deferring just this one to phase 2 leaves the whole
        // suite green while `victim` starts being truncated.
        keep();
        let (status, _o, _e) = run(&format!(": >'{v}' 4>&99999999999"));
        assert_eq!(status, 2);
        let left = std::fs::read_to_string(&victim).unwrap_or_default();
        assert_eq!(left, "KEEP");
        // The diagnostic does not go through a redirection the same command
        // applied: with stderr duped onto stdout it must NOT reach stdout, and
        // with stderr sent to /dev/null it must still be reported.
        let (status, out, _e) = run("echo one 2>&1 3>&/nope/x");
        assert_eq!((status, out.as_str()), (2, ""));
        let (status, _o, err) = run("echo one 2>/dev/null 3>&/nope/x");
        assert_eq!(status, 2);
        assert!(err.contains("ambiguous redirect"), "err: {err:?}");
        // What must NOT move: a failed DUP and a failed OPEN are `redirect`-time
        // in ash, so they happen after the earlier redirection has taken and the
        // file really is truncated. Measured against busybox 1.37.0 ash, which
        // empties `victim` in both.
        for tail in ["2>&7", "2</nope/x"] {
            keep();
            let (status, _o, _e) = run(&format!(": >'{v}' {tail}"));
            assert_eq!(status, 1, "tail: {tail}");
            let left = std::fs::read_to_string(&victim).unwrap_or_default();
            assert_eq!(left, "", "tail: {tail}");
        }
        // A here-document body is the one thing `expredir` does NOT hoist -- it
        // has no `NHERE` arm, so the body expands at `openhere` time, among the
        // opens. `>victim` therefore truncates BEFORE the body reads it, and the
        // substitution inside it sees an empty file rather than `KEEP`. Builtins
        // only, since the unit harness has no external commands.
        keep();
        let (_s, out, _e) = run(&format!(
            "read got >'{v}' <<EOF\n$(read a <'{v}'; echo \"$a\")\nEOF\necho \"got=[$got]\""
        ));
        assert_eq!(out, "got=[]\n");
        // And it expands ONCE. Phase 1 leaving the body alone is what makes that
        // true: expanding it there as well would run a substitution inside it
        // twice, which nothing above would show, since phase 2's second answer is
        // the one installed.
        let tick = dir.join("tick");
        let (_s, out, _e) = run(&format!(
            "read got <<EOF\n$(echo mark >>'{}'; echo body)\nEOF\necho \"got=[$got]\"",
            tick.display()
        ));
        assert_eq!(out, "got=[body]\n");
        let marks = std::fs::read_to_string(&tick).unwrap_or_default();
        assert_eq!(marks, "mark\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_descriptor_duped_onto_itself_is_left_alone() {
        // `n>&n` is skipped rather than performed, so it neither fails on a closed
        // descriptor nor disturbs an open one. Serving it as a dup breaks the
        // first; serving it as a CLOSE breaks only the THIRD, because a command's
        // redirections are unwound afterwards and `exec`'s are not -- which is
        // measured, not assumed: mutating `Unchanged` to a close fails at the
        // `exec` line with the other two green. The middle one discriminates
        // nothing on its own and is here to say what the skip must not disturb.
        let (status, out, _) = run(": 3>&3; echo hello");
        assert_eq!((status, out.as_str()), (0, "hello\n"));
        let (_status, out, _) = run("exec 3>&1; : 3>&3; echo via3 >&3");
        assert_eq!(out, "via3\n");
        let (_status, out, _) = run("exec 3>&1; exec 3>&3; echo still3 >&3");
        assert_eq!(out, "still3\n");
    }

    #[test]
    fn a_descriptor_target_is_digits_only() {
        // `u32::from_str` also takes a leading `+`, which bash, dash and zsh all
        // reject -- and the self-dup above turns that from a wrong descriptor into
        // a silent one, since `3>&+3` then reads as `3>&3` and succeeds on a fd 3
        // that is closed.
        let (status, out, err) = run("echo BAD 3>&+3; echo after");
        assert_eq!((status, out.as_str()), (2, ""));
        assert!(err.contains("ambiguous redirect"), "err: {err:?}");
        // A leading ZERO is still a number, as it is in all three.
        let (_status, out, _) = run("echo Z >&01");
        assert_eq!(out, "Z\n");
    }

    #[test]
    fn a_redirect_target_is_expanded_once() {
        // `set -C` checked the file with an expansion of its own and left
        // `open_file` to expand a second time, so a command substitution in the
        // target RAN TWICE -- side effects and all, on the one option whose whole
        // job is to not touch the file.
        let (_status, _out, err) = run("set -C; echo hi >\"$(echo /dev/null; echo TWICE >&2)\"");
        assert_eq!(err.matches("TWICE").count(), 1, "err: {err:?}");
        // The unguarded path was always single, and stays that way.
        let (_status, _out, err) = run("echo hi >\"$(echo /dev/null; echo ONCE >&2)\"");
        assert_eq!(err.matches("ONCE").count(), 1, "err: {err:?}");
    }

    #[test]
    fn a_dup_target_that_is_not_a_descriptor_names_a_file() {
        // busybox ash's `BASH_REDIR_OUTPUT`, which td's defconfig enables for the
        // same reason `[[` is available: `>&word` whose word is not a descriptor
        // is bash's `&>` -- the word names a FILE and BOTH streams go to it.
        let dir = std::env::temp_dir().join(format!("td-sh-redirout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let d = dir.display();
        let both = "sh_o() { echo O; echo E >&2; }; ";
        let back = |f: &str| format!("while read l; do echo got=$l; done <'{d}/{f}'");
        // The gate is the DESTINATION descriptor, not the direction: `1<&f` does
        // exactly what `1>&f` does, which is why this lives in `classify_dup` and
        // not in the `>&` arm. Measured on busybox 1.37.0 ash.
        for (n, op) in [("a", ">&"), ("b", "1>&"), ("c", "1<&")] {
            let (_s, out, _e) = run(&format!("{both}sh_o {op}'{d}/{n}'; {}", back(n)));
            assert_eq!(out, "got=O\ngot=E\n", "op: {op}");
        }
        // Stderr is RESTORED afterwards, which the captures above cannot see:
        // it is the second descriptor this installs, so it is the one a missing
        // save entry would leave pointing at the file for the rest of the script.
        let (_s, _o, err) = run(&format!(
            "{both}sh_o >&'{d}/r'; echo AFTER >&2; {}",
            back("r")
        ));
        assert_eq!(err, "AFTER\n");
        // Every other descriptor keeps the ambiguous-target error, and the two
        // spellings that ARE descriptors keep dupping and closing. The STATUS is
        // asserted beside the message because the message alone does not pin the
        // rule: ash raises this from `expredir`, so it is fatal on every
        // destination but 1, and a version that printed the same line and let the
        // script carry on would satisfy a message-only check.
        for op in ["0>&", "2>&", "9>&", "<&", "0<&", "3<&"] {
            let (s, _o, err) = run(&format!("{both}sh_o {op}'{d}/z'"));
            assert!(err.contains("ambiguous redirect"), "op: {op}, err: {err:?}");
            assert_eq!(s, 2, "op: {op}");
        }
        let (_s, _o, err) = run("echo hi >&2");
        assert_eq!(err, "hi\n");
        let (_s, out, _e) = run("echo hi >&-; echo st=$?");
        assert_eq!(out, "st=1\n");
        // It TRUNCATES, which is what the noclobber argument below rests on.
        // Every other path here is fresh, so an `append(true)` would satisfy all
        // of them and leave the shell appending where ash truncates.
        let over = dir.join("over");
        let Ok(()) = std::fs::write(&over, b"STALESTALE") else {
            panic!("fixture");
        };
        let (_s, _o, _e) = run(&format!("{both}sh_o >&'{}'", over.display()));
        assert_eq!(std::fs::read(&over).ok(), Some(b"O\nE\n".to_vec()));
        // Opened WRITE-only, as ash opens it: asking for read as well fails
        // `EACCES` on a target the operator can only write, and every other
        // path here uses a readable file that would not notice.
        use std::os::unix::fs::PermissionsExt;
        let wo = dir.join("wo");
        let Ok(()) = std::fs::write(&wo, b"") else {
            panic!("fixture");
        };
        let mode = std::fs::Permissions::from_mode(0o222);
        let Ok(()) = std::fs::set_permissions(&wo, mode) else {
            panic!("fixture");
        };
        let (_s, out, err) = run(&format!("{both}sh_o >&'{}'; echo st=$?", wo.display()));
        assert_eq!(out, "st=0\n", "err: {err:?}");
        // It truncates, so `set -C` guards it as it guards `>` -- without which
        // the one redirection that writes TWO streams was the one that ignored
        // noclobber, and this assertion failed against a destroyed file.
        let kept = dir.join("kept");
        let Ok(()) = std::fs::write(&kept, b"KEEP") else {
            panic!("fixture");
        };
        let (_s, _o, err) = run(&format!("{both}set -C; sh_o >&'{}'", kept.display()));
        assert_eq!(err, format!("td-sh: can't create {}: File exists\n", kept.display()));
        assert_eq!(std::fs::read(&kept).ok(), Some(b"KEEP".to_vec()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two decisions in the `set -C` check that `>` and `>&word` share.
    #[test]
    fn noclobber_refuses_an_existing_regular_file_and_only_that() {
        let dir = std::env::temp_dir().join(format!("td-sh-noclob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let d = dir.display();
        // REGULAR, not merely existing: ash's `openredirect` falls to a plain
        // `O_WRONLY` for a target that exists and is not a regular file
        // (ash.c:5560), so `set -C` guards no device. Measured on busybox
        // 1.37.0 ash, which writes all three of these happily.
        //
        // `/dev/zero` earns its place over `/dev/null`, which the two before it
        // are: `open_file` answers that ONE name without opening anything, so a
        // `/dev/null` target never reaches the non-regular arm's open or the
        // re-check after it, and inverting that check's sense passed the whole
        // suite. This is the only case that evaluates it.
        for op in [">", ">&"] {
            for dev in ["/dev/null", "/dev/zero"] {
                let (_s, out, err) = run(&format!("set -C; echo x {op}{dev}; echo st=$?"));
                assert_eq!(out, "st=0\n", "op: {op}{dev}, err: {err:?}");
            }
        }
        // A directory is refused by the OPEN that follows, not by the check --
        // which is why the message is the open's rather than "cannot overwrite".
        let adir = dir.join("adir");
        let Ok(()) = std::fs::create_dir_all(&adir) else {
            panic!("fixture dir");
        };
        let ad = adir.display();
        for op in [">", ">&"] {
            let (_s, out, err) = run(&format!("set -C; echo x {op}'{ad}'; echo st=$?"));
            assert_eq!(out, "st=1\n", "op: {op}, err: {err:?}");
            assert!(!err.contains("File exists"), "op: {op}, err: {err:?}");
        }
        // `>|` is the override, and the ONLY one: `set -C` must not reach it.
        let over = dir.join("bar");
        let Ok(()) = std::fs::write(&over, b"KEEP") else {
            panic!("fixture");
        };
        let ov = over.display();
        let (_s, out, err) = run(&format!("set -C; echo x >|'{ov}'; echo st=$?"));
        assert_eq!(out, "st=0\n", "err: {err:?}");
        assert_eq!(std::fs::read(&over).ok(), Some(b"x\n".to_vec()));
        // The check consults the SHELL's cwd and not the process's. With the two
        // disagreeing, the check would look at one path while the open truncates
        // another -- so the file `set -C` exists to protect is destroyed, which
        // is the assertion below rather than a status.
        for (n, op) in [("kr", ">"), ("kd", ">&")] {
            let kept = dir.join(n);
            let Ok(()) = std::fs::write(&kept, b"KEEP") else {
                panic!("fixture");
            };
            let (_s, out, err) = run(&format!("cd '{d}'; set -C; echo x {op}{n}; echo st=$?"));
            assert_eq!(out, "st=1\n", "op: {op}, err: {err:?}");
            // The NAME is asserted, not just the refusal: this test exists for
            // which path the check consulted, and a message naming another one
            // would otherwise pass.
            let want = format!("td-sh: can't create {n}: File exists\n");
            assert_eq!(err, want, "op: {op}");
            let after = std::fs::read(&kept).ok();
            assert_eq!(after, Some(b"KEEP".to_vec()), "op: {op}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Why the `set -C` check and the create are ONE operation.
    #[test]
    fn noclobber_does_not_create_through_a_dangling_symlink() {
        // Looking first and creating after followed the link: the stat that
        // guards `>` sees nothing there, so `ln -s missing lnk; set -C;
        // echo x >lnk` created `missing` -- data written somewhere the operator
        // did not name, by the option whose whole job is not to. ash creates
        // with `O_CREAT|O_EXCL`, which the kernel refuses to follow a symlink
        // for, so there is no window between the two.
        let dir = std::env::temp_dir().join(format!("td-sh-dangle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        for (n, op) in [("la", ">"), ("lb", ">&")] {
            let link = dir.join(n);
            let missing = dir.join(format!("{n}-missing"));
            let Ok(()) = std::os::unix::fs::symlink(&missing, &link) else {
                panic!("fixture");
            };
            let (_s, out, err) = run(&format!(
                "set -C; echo x {op}'{}'; echo st=$?",
                link.display()
            ));
            assert_eq!(out, "st=1\n", "op: {op}, err: {err:?}");
            assert!(!missing.exists(), "op {op} created through the link");
        }
        // Without the option the link IS followed, which is ordinary symlink
        // behaviour and not something `set -C` should be read as changing.
        let link = dir.join("open");
        let missing = dir.join("open-missing");
        let Ok(()) = std::os::unix::fs::symlink(&missing, &link) else {
            panic!("fixture");
        };
        let (_s, out, _e) = run(&format!("echo x >'{}'; echo st=$?", link.display()));
        assert_eq!(out, "st=0\n");
        assert_eq!(std::fs::read(&missing).ok(), Some(b"x\n".to_vec()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `>&-` closes only spelled BARE -- the one place this shell has to care
    /// how a redirection target was written rather than what it expands to.
    #[test]
    fn only_a_bare_dash_closes_a_descriptor() {
        // ash recognises the lone `-` in the PARSER, on the unexpanded word
        // (`LONE_DASH`, ash.c:12012), so a word that needs expanding never
        // reaches that test: `>&'-'` and `>&$v` are ordinary non-digit targets,
        // which on fd 1 name a FILE called `-`. Measured on busybox 1.37.0 ash.
        let dir = std::env::temp_dir().join(format!("td-sh-baredash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let both = "sh_o() { echo O; echo E >&2; }; ";
        for (n, spell) in [("q", "$v"), ("r", "'-'"), ("s", "\"-\""), ("t", "\\-")] {
            let sub = dir.join(n);
            let Ok(()) = std::fs::create_dir_all(&sub) else {
                panic!("fixture dir");
            };
            let (_s, _o, err) = run(&format!(
                "{both}cd '{}'; v=-; sh_o >&{spell}",
                sub.display()
            ));
            let wrote = std::fs::read(sub.join("-")).ok();
            assert_eq!(wrote, Some(b"O\nE\n".to_vec()), "{spell}, err: {err:?}");
        }
        // Bare, and it closes: nothing is named, and the write to the closed
        // descriptor is what fails rather than the redirection.
        let sub = dir.join("bare");
        let Ok(()) = std::fs::create_dir_all(&sub) else {
            panic!("fixture dir");
        };
        let (_s, out, _e) = run(&format!("cd '{}'; echo O >&-; echo st=$?", sub.display()));
        assert_eq!(out, "st=1\n");
        assert!(!sub.join("-").exists(), "a bare dash named a file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boundary between the two arms above, which is not where the shorter
    /// rule "digits are a descriptor, anything else is a file" puts it.
    #[test]
    fn a_digit_string_is_a_descriptor_however_long_it_is() {
        // ash classifies on ALL-DIGITS first (`isdigit_str`, ash.c:560) and only
        // then asks what the digits mean, so a digit string is never a filename:
        // one too large to BE a descriptor is `raise_error_syntax("bad fd
        // number")` (ash.c:12026) and fatal. Without the split the file arm above
        // swallows the overflow and CREATES `99999999999` -- the wrong outcome in
        // the worst direction, since it succeeds.
        let dir = std::env::temp_dir().join(format!("td-sh-badfd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let d = dir.display();
        // Every descriptor and both directions: the digit test precedes the fd-1
        // file arm, so none of these reaches it.
        for op in [">&", "1>&", "2>&", "<&", "0<&"] {
            let (s, out, err) = run(&format!("cd '{d}'; echo A {op}99999999999; echo AFTER"));
            assert_eq!(s, 2, "op: {op}, err: {err:?}");
            assert_eq!(out, "", "op {op} ran what followed a fatal error");
        }
        // The empty string is all-digits by that predicate too, so it takes the
        // same arm rather than being an `open("")`.
        let (s, out, _e) = run(&format!("cd '{d}'; v=; echo A >&$v; echo AFTER"));
        assert_eq!((s, out.as_str()), (2, ""));
        // None of the seven created anything -- the assertion the file arm's
        // absence of a length check would fail.
        let made = std::fs::read_dir(&dir).ok().map(|it| it.count());
        assert_eq!(made, Some(0), "an overflowing target created a file");
        // The boundary itself is `INT_MAX`, ash's `bb_strtou` into an `int`
        // refusing a negative result (ash.c:12017) -- not the width of the type
        // td-sh happens to parse into. Below it the dup merely FAILS, which is
        // recoverable and lets the script carry on.
        // Both run inside the fixture dir like the rest: a regression that made
        // a digit string name a file would otherwise create it in the SOURCE
        // tree, where the next `git add -A` sweeps it up.
        let (s, out, _e) = run(&format!("cd '{d}'; echo A >&2147483647; echo AFTER"));
        assert_eq!((s, out.as_str()), (0, "AFTER\n"));
        let (s, out, _e) = run(&format!("cd '{d}'; echo A >&2147483648; echo AFTER"));
        assert_eq!((s, out.as_str()), (2, ""));
        // A leading zero is still digits, so it is still a descriptor: ten of
        // them naming fd 1 is a self-dup, not a file called `0000000001`.
        let (s, out, _e) = run(&format!("cd '{d}'; echo A >&0000000001"));
        assert_eq!((s, out.as_str()), (0, "A\n"));
        // Fatal is `Sig::Abort` and not `Sig::Exit`: on a pty ash returns to the
        // prompt and runs the next line, where `Exit` would end the session.
        // Every assertion above is a SCRIPT, where the two are indistinguishable.
        let units = ["echo A >&99999999999", "echo NEXT"];
        let (_s, out, _e) = crate::process::run_capturing_interactive_units(&units);
        assert_eq!(out, "NEXT\n");
        let made = std::fs::read_dir(&dir).ok().map(|it| it.count());
        assert_eq!(made, Some(0), "a leading-zero descriptor named a file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_target_that_is_not_a_descriptor_number_is_fatal() {
        // ash raises this one a step earlier than the failure below -- from
        // `expredir`, OUTSIDE the `redirectsafe` that makes the other
        // recoverable -- and so ends the shell on it with status 2. The two
        // sit either side of that boundary, which is the whole point of
        // testing them together.
        //
        // On fd 2, where a non-descriptor target stays ambiguous rather than
        // naming a file -- fd 1 is `classify_dup`'s `BASH_REDIR_OUTPUT` case.
        let (status, out, err) = run("echo one 2>&/nope/x; echo survived");
        assert_eq!((status, out.as_str()), (2, ""));
        assert!(err.contains("ambiguous redirect"), "err: {err:?}");
        // And on fd 1 the target IS a file, so the failure is the OPEN's, which
        // ash applies through `redirectsafe`: recoverable, and the command that
        // follows still runs.
        let (status, out, err) = run("echo one 1>&/nope/x; echo survived");
        assert_eq!((status, out.as_str()), (0, "survived\n"));
        assert!(err.contains("nonexistent directory"), "err: {err:?}");
        // Fatal is `Sig::Abort`, so an interactive shell returns to its prompt
        // rather than ending -- the same distinction the `bad fd number` arm
        // makes, and one no script can observe.
        let units = ["echo one 2>&/nope/x", "echo NEXT"];
        let (_s, out, _e) = crate::process::run_capturing_interactive_units(&units);
        assert_eq!(out, "NEXT\n");
    }

    #[test]
    fn a_failed_redirection_rolls_back_the_ones_before_it() {
        // Both arms of `apply_redirs`: a redirection that fails leaves the fd
        // table exactly as it found it, or the command's EARLIER redirections
        // outlive the command. Each case checks the log was really written to
        // (the first diagnostic lands in it, since `2>log` had already taken)
        // before checking the later marker is absent -- otherwise a `log` that
        // was never created passes both halves saying nothing.
        let dir = std::env::temp_dir().join(format!("td-sh-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let log = dir.join("log");
        let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
        // RECOVERABLE: a dup of a descriptor that is not open. The shell carries
        // on, so a leaked fd 2 shows up on the very next command.
        let (_s, _o, err) = run(&format!(
            "echo one 2>'{}' 3>&7; echo AFTER >&2",
            log.display()
        ));
        assert!(err.contains("AFTER"), "err: {err:?}");
        let logged = read_log();
        assert!(logged.contains("bad file descriptor"), "logged: {logged:?}");
        assert!(!logged.contains("AFTER"), "logged: {logged:?}");
        // FATAL: a here-document body, which is the only fatal error phase 2
        // still raises -- a bad target word is settled before the `2>log` beside
        // it opens, so using one would make this case vacuous. Interactive,
        // since only there does the shell survive an abort to show the leak.
        let _ = std::fs::remove_file(&log);
        let first = format!("cat 2>'{}' <<EOF\n${{nope:?}}\nEOF\n", log.display());
        let units = [first.as_str(), "echo LEAKTEST >&2"];
        let (_s, _o, err) = crate::process::run_capturing_interactive_units(&units);
        assert!(err.contains("LEAKTEST"), "err: {err:?}");
        let logged = read_log();
        assert!(logged.contains("nope"), "logged: {logged:?}");
        assert!(!logged.contains("LEAKTEST"), "logged: {logged:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_redirection_failure_is_fatal_on_every_ash_special_word() {
        // ash's `spclbltin` bit (`name[0] & 1`, ash.c:8205) makes a redirection
        // failure end the shell instead of skipping the command. The whole set is
        // swept rather than the two words that were wrong, because the point is
        // the PREDICATE: `source` and `times` were recoverable only because they
        // were the two ash marks special that td-sh did not implement, so they
        // resolved to no `Builtin` and took the external path. Both resolve now,
        // which is what retired the word-level arm that used to serve them --
        // reaching that path means `lookup` said None, so no word ash marks
        // special can get there again.
        for w in [
            ".", ":", "break", "continue", "eval", "exec", "exit", "export", "local", "readonly",
            "return", "set", "shift", "source", "times", "trap", "unset",
        ] {
            let (status, out, _e) = run(&format!("{w} 4>&9; echo AFTER"));
            assert_eq!((status, out.as_str()), (1, ""), "special: {w}");
            // With an ARGUMENT too, since the word-level test is on the command
            // NAME: reading the last word instead of the first passes every
            // one-word case above and then decides `ls times` by `times`.
            let (status, out, _e) = run(&format!("{w} x 4>&9; echo AFTER"));
            assert_eq!((status, out.as_str()), (1, ""), "special with arg: {w}");
        }
        // And a REGULAR builtin still skips the command and carries on.
        for w in ["true", "echo", "read", "pwd", "cd"] {
            let (status, out, _e) = run(&format!("{w} 4>&9; echo AFTER"));
            assert_eq!((status, out.as_str()), (0, "AFTER\n"), "regular: {w}");
        }
        // A special word as an ARGUMENT decides nothing -- the mirror of the
        // case above, and the half that a last-word test gets backwards.
        let (status, out, _e) = run("echo times 4>&9; echo AFTER");
        assert_eq!((status, out.as_str()), (0, "AFTER\n"));
        // Those five resolve to a `Builtin`, so they are decided by
        // `is_ash_special` and never reach the word-level guard. `type` is what
        // reads the predicate for them, so it is what pins it: adding one of
        // them to the hand-named arm passes every case above.
        let (_s, out, _e) = run("type read");
        assert_eq!(out, "read is a shell builtin\n");
        let (_s, out, _e) = run("type unset");
        assert_eq!(out, "unset is a special shell builtin\n");
        // Both words resolve now, so `is_ash_special_word` is a plain lookup
        // with nothing named by hand -- which is the point of it. A hand-named
        // arm is a second source of truth able to hide a missing `is_special`
        // entry, so this holds the predicate to the one it has.
        assert!(crate::builtin::lookup("source").is_some());
        assert!(crate::builtin::lookup("times").is_some());
        assert!(!crate::builtin::is_ash_special_word("notabuiltin"));
        // The guard must not over-fire. A FUNCTION of that name is dispatched
        // before it, and `command` resolves to a builtin that never carries the
        // redirections here -- both recoverable in ash, measured.
        for src in [
            "times() { echo FN; }; times 4>&9; echo AFTER",
            "command times 4>&9; echo AFTER",
        ] {
            let (status, out, _e) = run(src);
            assert_eq!((status, out.as_str()), (0, "AFTER\n"), "src: {src}");
        }
        // The abort is confined by a clone, which is what `Sig::Abort` is and
        // `Sig::Interrupt` is not: each of these reports the failure and the
        // shell carries on, in ash and here.
        for src in [
            "(times 4>&9); echo AFTER",
            "x=$(times 4>&9); echo AFTER",
            "times 4>&9 | cat; echo AFTER",
        ] {
            let (status, out, _e) = run(src);
            assert_eq!((status, out.as_str()), (0, "AFTER\n"), "src: {src}");
        }
        // And it returns to a PROMPT rather than ending the shell, which is the
        // one thing separating `Sig::Abort` from `Sig::Exit` and which no script
        // can observe.
        let units = ["times 4>&9", "echo NEXT"];
        let (_s, out, _e) = crate::process::run_capturing_interactive_units(&units);
        assert_eq!(out, "NEXT\n");
    }

    #[test]
    fn a_closed_descriptor_is_not_a_descriptor() {
        // ash makes a descriptor CLOSED by `>&-` and one never opened the same
        // thing: every dup FROM either is `dup2(n,m): Bad file descriptor` and
        // skips the command. Here the closed one stays in the table as
        // `Fd::Closed` -- a child has to tell closed from absent -- so without
        // refusing it beside `None` the marker gets duped and the command runs.
        // Measured against busybox 1.37.0 ash, every shape below.
        for src in [
            "echo CMD 3>&- 4>&3; echo AFTER",
            "exec 3>&1; exec 3>&-; echo CMD 4>&3; echo AFTER",
            // The read direction is the same rule; `<&` and `>&` differ only in
            // which descriptor is the default, never in what a target may be.
            "exec 3>&1; exec 3>&-; echo CMD 0<&3; echo AFTER",
            // Never opened, which was already refused -- kept so the fix cannot
            // be read as being about `Closed` alone.
            "echo CMD 4>&3; echo AFTER",
        ] {
            let (_s, out, err) = run(src);
            assert_eq!(out, "AFTER\n", "src: {src}");
            // The NUMBER is part of the diagnostic: every case here names a
            // different descriptor as its destination, so a message that dropped
            // it would read the same for all of them.
            assert!(err.contains("3: bad file descriptor"), "src: {src}, {err:?}");
        }
        // And the neighbours that must NOT become errors. Closing is idempotent
        // and may name a descriptor that was never open; a self-dup is skipped
        // without a lookup, so it does not care either way; and a descriptor
        // reopened after closing is ordinary again.
        for src in [
            "echo CMD 4>&-; echo AFTER",
            "exec 3>&1; exec 3>&-; echo CMD 3>&3; echo AFTER",
            "echo CMD 3>&3; echo AFTER",
            "exec 3>&1; exec 3>&-; exec 3>&1; echo CMD >&3; echo AFTER",
            // Duping FROM an INHERITED descriptor, which is the over-refusal
            // this arm invites most: `Some(Fd::Inherit(0)) | Some(Fd::Closed) |
            // None` is already the right pattern elsewhere in the file, where
            // fd 0 really is a bad target -- for a WRITE. As a dup SOURCE it is
            // an ordinary open descriptor, and the other cases here never reach
            // one, since the test harness gives fd 1 and 2 buffers instead.
            "echo CMD 3<&0; echo AFTER",
            "echo CMD 2>&0; echo AFTER",
        ] {
            let (_s, out, err) = run(src);
            assert_eq!(out, "CMD\nAFTER\n", "src: {src}");
            assert_eq!(err, "", "src: {src}");
        }
        // Closing a descriptor that is ALREADY closed is not an error either.
        // There is no command to see run here -- an `exec` carrying only
        // redirections runs none -- so what shows it is `AFTER` arriving with
        // stderr empty: the second close neither reported nor ended the shell.
        let (_s, out, err) = run("exec 3>&1; exec 3>&-; exec 3>&-; echo AFTER");
        assert_eq!((out.as_str(), err.as_str()), ("AFTER\n", ""));
        // `/dev/null` is a TARGET, not an absence: it lives in the table as
        // `Fd::Null` right beside `Fd::Closed`, and duping from it must still
        // work. Refusing the two together passes every case above.
        let (_s, out, err) = run("exec 3>/dev/null; echo CMD 4>&3; echo AFTER");
        assert_eq!((out.as_str(), err.as_str()), ("CMD\nAFTER\n", ""));
        // And a file-backed descriptor duped onto stdout carries the write to
        // the file, which is the ordinary case the refusal must not reach.
        let dir = std::env::temp_dir().join(format!("td-sh-dupfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let out_path = dir.join("out");
        let (_s, out, _e) = run(&format!(
            "exec 3>'{}'; echo CMD 1>&3; echo AFTER",
            out_path.display()
        ));
        assert_eq!(out, "AFTER\n");
        let wrote = std::fs::read_to_string(&out_path).unwrap_or_default();
        assert_eq!(wrote, "CMD\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `&>file` sends BOTH streams to one file, and is one token only when the
    /// `>` is glued to the `&`. The two halves are what make it worth a test:
    /// spaced, the same characters are a background job plus a redirect, and
    /// getting that wrong turns `f &>/dev/null` -- the idiom this exists for --
    /// into a job whose output still reaches the terminal.
    #[test]
    fn ampersand_greater_sends_both_streams_to_one_file() {
        let dir = std::env::temp_dir().join(format!("td-sh-ampgt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let f = dir.join("f");
        let g = dir.join("g");
        let (d, e) = (f.display(), g.display());
        let both = "{ echo O; echo E >&2; }";
        let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();

        // Both streams, and nothing left on either of the shell's own.
        let (_s, out, err) = run(&format!("{both} &>'{d}'"));
        assert_eq!((out.as_str(), err.as_str()), ("", ""));
        assert_eq!(read(&f), "O\nE\n");
        // ONE open file on two descriptors, so they share an offset and
        // interleave -- two opens would have the second overwrite the first.
        let (_s, _o, _e) = run(&format!("{{ printf AAAA; printf BBBB >&2; }} &>'{d}'"));
        assert_eq!(read(&f), "AAAABBBB");

        // The fd prefix decides whether stderr follows, exactly as it does in
        // ash: only on 1. `2&>f` is a plain `2>f` -- NOT the error `2>&f` is,
        // because the two spellings reach ash's NTO2 by different routes and
        // only `>&`'s carries the fd check.
        let (_s, out, err) = run(&format!("{both} 2&>'{d}'"));
        assert_eq!((out.as_str(), err.as_str()), ("O\n", ""));
        assert_eq!(read(&f), "E\n");
        let (_s, out, err) = run(&format!("{both} 3&>'{d}'"));
        assert_eq!((out.as_str(), err.as_str()), ("O\n", "E\n"));
        assert_eq!(read(&f), "");
        // fd 0 as well as 3, because 1 is a BOUNDARY: `dest <= STDOUT` reads
        // exactly like `dest == STDOUT` on every descriptor above it.
        let (_s, out, err) = run(&format!("{both} 0&>'{d}'"));
        assert_eq!((out.as_str(), err.as_str()), ("O\n", "E\n"));
        assert_eq!(read(&f), "");
        // It TRUNCATES on every descriptor, so `set -C` refuses it on every
        // descriptor -- status 1, not fatal, and the file left as it was. The
        // non-1 branch needs its own case: it is a different `Plan`, and the
        // only corpus case that covered `&>` under `set -C` is xfailed here.
        // The refused open SKIPS the command whichever descriptor it was for,
        // so nothing is written on any of them and the status is 1 rather
        // than fatal.
        for w in ["&>", "1&>", "2&>", "3&>"] {
            std::fs::write(&f, "PRE\n").unwrap();
            let (_s, out, err) =
                run(&format!("set -C; {both} {w}'{d}'; echo st=$?"));
            assert_eq!(out, "st=1\n", "{w}");
            assert!(!err.is_empty(), "{w}");
            assert_eq!(read(&f), "PRE\n", "{w}");
        }
        let _ = std::fs::remove_file(&f);
        // The `>&` spelling on fd 2 stays fatal, which is the divergence.
        let (status, _o, err) = run(&format!("{both} 2>&'{d}'; echo AFTER"));
        assert_eq!(status, 2);
        assert!(!err.is_empty());

        // Two INDEPENDENT descriptors, not a linked pair: a later `>` moves
        // stdout alone and leaves stderr on the file.
        let (_s, _o, _e) = run(&format!("{both} &>'{d}' >'{e}'"));
        assert_eq!((read(&f).as_str(), read(&g).as_str()), ("E\n", "O\n"));

        // Glued only. Spaced, these are a background job and a redirect on an
        // empty command, which is what td-sh did with every `&>` before this.
        for src in ["echo hi & >'{}'", "echo hi &  >'{}'"] {
            let (_s, out, _e) = run(&src.replace("{}", &d.to_string()));
            assert_eq!(out, "hi\n", "{src}");
            assert_eq!(read(&f), "");
        }
        // And a digit is the fd only when the WHOLE pair is glued to it. Both
        // halves matter: spaced off, `2` is an argument and `&>` is still the
        // operator; glued to a lone `&`, it is an argument again and the `&` is
        // a background job -- so peeking one character past the `&` is what
        // tells the fd prefix from the job.
        let (_s, out, _e) = run(&format!("echo 2 &>'{d}'"));
        assert_eq!(out, "");
        assert_eq!(read(&f), "2\n");
        let _ = std::fs::remove_file(&f);
        // A space is not the only thing that can follow that bare `&`, and
        // pinning only the spaced case leaves `peek_at(1) != Some(' ')` alive.
        // Each of these is a digit glued to a `&` that is NOT the operator.
        let (_s, out, _e) = run(&format!("echo 2& >'{d}'"));
        assert_eq!(out, "2\n");
        assert_eq!(read(&f), "");
        assert_eq!(run("echo 2&&echo x").1, "2\nx\n");
        let (_s, out, _e) = run("echo 2&\necho x");
        let mut lines: Vec<&str> = out.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["2", "x"], "{out:?}");
        assert_eq!(run("echo 2&; echo x").0, 2);
        // And `&<` is not this operator either: only `>` glues to the `&`. That
        // `&` backgrounds `echo hi`, so `wait` is what orders the two lines --
        // without it they race and the assertion is a coin toss.
        std::fs::write(&f, "PRE\n").unwrap();
        let (_s, out, err) = run(&format!("echo hi &<'{d}'; wait; echo done"));
        assert_eq!(out, "hi\ndone\n");
        assert_eq!(read(&f), "PRE\n", "{err}");

        // ash refuses these three where bash takes the first; the grammar gives
        // it for free, since nothing but a word may follow the operator.
        for src in ["echo hi &>>x", "echo hi &>&1", "echo hi &>|x"] {
            let (status, out, err) = run(&format!("{src}; echo AFTER"));
            assert_eq!((status, out.as_str()), (2, ""), "{src}");
            assert!(err.contains("syntax error"), "{src}: {err:?}");
        }
        // Inert inside quotes and expansions -- ash gates its lexer hunk on
        // `varnest == 0`, and `&` being a metacharacter is td-sh's equivalent.
        assert_eq!(run("echo \"${x:-a&>b}\"").1, "a&>b\n");
        assert_eq!(run("echo 'a&>b'").1, "a&>b\n");
        // The operator SPELLS itself in a diagnostic. Nothing else reads that
        // string, so a transposed `>&` there is invisible to every case above.
        let (_s, _o, err) = run("for i in &>; do :; done");
        assert!(err.contains("`&>`"), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_file_operator_opens_the_way_it_says() {
        // `>>` must not truncate and `<>` must not either; both were moved
        // verbatim into `Plan` arms, where a stray `truncate(true)` reads like
        // its neighbours and destroys the file instead of extending it.
        let dir = std::env::temp_dir().join(format!("td-sh-openmode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(()) = std::fs::create_dir_all(&dir) else {
            panic!("fixture dir");
        };
        let f = dir.join("f");
        let d = f.display();
        let (_s, out, _e) = run(&format!(
            "echo A >'{d}'; echo B >>'{d}'; while read l; do echo got=$l; done <'{d}'"
        ));
        assert_eq!(out, "got=A\ngot=B\n");
        // `<>` opens for read AND write without truncating, so what was there
        // survives a command that writes nothing.
        let (_s, out, _e) = run(&format!(
            "echo A >'{d}'; : <>'{d}'; while read l; do echo got=$l; done <'{d}'"
        ));
        assert_eq!(out, "got=A\n");
        // And `>` still truncates, which is what the other two are NOT.
        let (_s, out, _e) = run(&format!(
            "echo A >'{d}'; : >'{d}'; while read l; do echo got=$l; done <'{d}'"
        ));
        assert_eq!(out, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errexit_exemption_covers_a_compound_operand() {
        // A function on the non-final side of `||` is exempt from errexit for its
        // WHOLE body: an inner `false` must not exit before `echo survived` runs.
        let (status, out, _) =
            run("set -e; f() { false; echo survived; }; f || echo fallback; echo end");
        assert_eq!(out, "survived\nend\n");
        assert_eq!(status, 0);
    }

    #[test]
    fn subshell_in_a_condition_inherits_errexit_suppression() {
        // A subshell evaluated as an `if` condition is part of the suppressed
        // context: an inner `false` must not exit before `echo survived` runs.
        let (_status, out, _) =
            run("set -e; if (false; echo survived); then echo yes; fi; echo end");
        assert_eq!(out, "survived\nyes\nend\n");
    }

    #[test]
    fn prefix_assignment_exports_to_an_external_via_command() {
        // `FOO=bar command extcmd` must pass FOO into the external's environment even
        // though a prefix on a regular builtin (`command`) is otherwise transient.
        if !std::path::Path::new("/bin/sh").exists() {
            return; // hermetic guard: no host shell to exec the probe script
        }
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let uniq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("td-sh-prefix-{}-{}", std::process::id(), uniq));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"#!/bin/sh\nprintf %s \"$FOO\"\n").unwrap();
            let mut perm = f.metadata().unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&path, perm).unwrap();
        }
        let (_status, out, _err) = run(&format!("FOO=bar command '{}'", path.display()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "bar");
    }

    #[test]
    fn an_interactive_shell_loses_the_command_and_not_the_session() {
        use crate::process::run_capturing_interactive_units;
        // Every `sh_error` here: a bad operand on each of the four builtins that
        // parse one, a redirection error on a special builtin, and the two fatal
        // expansions. In each the REST OF THE LINE is dropped and the NEXT unit
        // still runs -- what ash and dash do on a pty.
        for bad in [
            "shift oops; echo SAME",
            "while :; do break oops; done; echo SAME",
            "exit oops; echo SAME",
            "f() { return oops; }; f; echo SAME",
            "export FOO=bar >/nonexistent/dir/x; echo SAME",
            "echo ${undefined_var:?bad}; echo SAME",
            "set -u; echo $undefined_var; echo SAME",
            "readonly RO=1; RO=2; echo SAME",
        ] {
            let (status, out, _) =
                run_capturing_interactive_units(&[bad, "set +u", "echo NEXT"]);
            assert_eq!(out, "NEXT\n", "{bad}");
            assert_eq!(status, 0, "{bad}");
        }
        // The contrast, and the whole of what `swallows_abort` decides: a REGULAR
        // builtin's error ends the COMMAND alone, so the rest of the line runs.
        // ash re-raises out of `evalbltin` for a special builtin only
        // (ash.c:10619), which is why every row above is one.
        for bad in [
            "readonly OPTARG=x; set -- -Z; getopts a O; echo SAME",
            "readonly N; read N </dev/null; echo SAME",
            "readonly O; getopts ab O -a; echo SAME",
        ] {
            let (status, out, _) =
                run_capturing_interactive_units(&[bad, "set +u", "echo NEXT"]);
            assert_eq!(out, "SAME\nNEXT\n", "{bad}");
            assert_eq!(status, 0, "{bad}");
        }
        // The redirection case also has to leave the prefix assignment undone,
        // because redirections are applied before it.
        let (_, out, _) = run_capturing_interactive_units(&[
            "export FOO=bar >/nonexistent/dir/x",
            "echo \"[${FOO}]\"",
        ]);
        assert_eq!(out, "[]\n");
        // `$?` is the aborted command's, not the shell's previous one. `${x:?}` is
        // the case that tests the HANDLER: `Shell::fatal` is the one raiser that
        // sets no status itself, so only `run_interactive_unit` can supply it.
        for src in ["shift oops", "echo ${undefined_var:?bad}"] {
            let (_, out, _) = run_capturing_interactive_units(&[src, "echo st=$?"]);
            assert_eq!(out, "st=2\n", "{src}");
        }
    }

    #[test]
    fn a_functions_prefix_assignment_is_a_temp_frame() {
        // Visible inside the call, gone after it -- including a mutation made
        // through the binding, which is what makes it a FRAME and not just a
        // save/restore of the value the caller supplied.
        for (src, want) in [
            ("f() { :; }; D=dd f; echo [${D-unset}]", "[unset]\n"),
            ("f() { echo in=$D; }; D=dd f; echo out=[$D]", "in=dd\nout=[]\n"),
            ("D=orig; f() { echo in=$D; }; D=dd f; echo out=$D", "in=dd\nout=orig\n"),
            ("D=orig; f() { D=mut; }; D=dd f; echo out=$D", "out=orig\n"),
            ("D=orig; f() { unset D; }; D=dd f; echo out=${D-UNSET}", "out=orig\n"),
            // Exported for the call, as an external command's environment is.
            ("f() { echo $D; }; D=dd f", "dd\n"),
        ] {
            let (_, out, _) = run(src);
            assert_eq!(out, want, "{src}");
        }
        // The corpus's own temp-frame case, whose last line is the whole point:
        // `local` unwinds to the binding, then the frame takes the binding away.
        let (_, out, _) = run(
            "x=global; f() { echo x=$x; x=mutated-temp; echo x=$x; local x=local; \
             echo x=$x; unset x; echo x=$x; }; x=temp-binding f; echo x=$x",
        );
        assert_eq!(out, "x=temp-binding\nx=mutated-temp\nx=local\nx=\nx=global\n");
        // A non-function command word is unaffected: still transient for a regular
        // builtin, still persistent for a special one.
        let (_, out, _) = run("D=dd echo hi; echo out=[$D]");
        assert_eq!(out, "hi\nout=[]\n");
        let (_, out, _) = run("D=dd export E=1; echo out=[$D]");
        assert_eq!(out, "out=[dd]\n");
    }

    #[test]
    fn an_exit_trap_runs_inside_the_frame_the_shell_died_in() {
        // The bindings of that frame already survived; being IN a function has to
        // survive with them, or `local` in the trap errors and the trap never
        // runs at all. Every value read off the td-built busybox ash.
        for (src, want) in [
            ("f() { exit 7; }; trap 'local Q=1; echo t=ok' EXIT; f", "t=ok\n"),
            ("f() { echo ${u:?bad}; }; trap 'local Q=1; echo t=ok' EXIT; f", "t=ok\n"),
            ("g() { exit 7; }; f() { local D=f; g; }; trap 'local Q=1; echo t=ok' EXIT; f",
             "t=ok\n"),
            // The trap sees the dead frame's `local`, and can shadow it with one
            // of its own.
            ("D=g; f() { local D=f; exit 7; }; trap 'echo t=$D; local D=x; echo u=$D' EXIT; f",
             "t=f\nu=x\n"),
            // Not inside one: at the top level, after a plain `exit`, and after a
            // function that RETURNED, `local` is still an error in all three.
            ("trap 'local Q=1; echo BAD' EXIT; true", ""),
            ("trap 'local Q=1; echo BAD' EXIT; exit 3", ""),
            ("f() { return 3; }; trap 'local Q=1; echo BAD' EXIT; f; exit 4", ""),
        ] {
            let (_, out, _) = run(src);
            assert_eq!(out, want, "{src}");
        }
        // The status is the dying one, not the trap's: a trap that now RUNS must
        // not change what the shell reports.
        assert_eq!(run("f() { exit 7; }; trap 'local Q=1' EXIT; f").0, 7);
        // ...and the error case still reports the failed `local`, as before.
        assert_eq!(run("trap 'local Q=1' EXIT; exit 3").0, 2);
    }

    #[test]
    fn a_recovery_puts_back_the_function_it_unwound_out_of() {
        // Deferring `in_function` is only safe because every recovery undoes it:
        // a shell that keeps running must not still think it is in the function
        // it just unwound out of, or `local` works where both references reject
        // it. `command` is the non-interactive recovery...
        let (_, out, err) =
            run("f() { echo ${u:?bad}; }; command eval 'f'; local R=1; echo after=$?");
        assert_eq!(out, "");
        assert!(err.contains("not in a function"), "{err}");
        // ...and the prompt loop is the other. Same program, one unit per line.
        let (_, out, err) = crate::process::run_capturing_interactive_units(&[
            "f() { echo ${u:?bad}; }",
            "f",
            "local R=1",
            "echo after=$?",
        ]);
        assert_eq!(out, "after=2\n");
        assert!(err.contains("not in a function"), "{err}");
        let (_, out, err) = run(
            "h() { echo ${u:?bad}; }; g() { h; }; f() { g; }; \
             command eval 'f'; local R=1; echo BAD",
        );
        assert_eq!(out, "");
        assert!(err.contains("not in a function"), "{err}");
        // Several frames dying at once each leave a marker, so the drain ORDER is
        // load-bearing: applied newest first, the OUTERMOST depth lands last. The
        // prompt is where that shows, because unlike the `command` case above there
        // is no surrounding command whose own restore would mask a wrong order.
        let (_, out, err) = crate::process::run_capturing_interactive_units(&[
            "h() { echo ${u:?bad}; }",
            "g() { h; }",
            "f() { g; }",
            "f",
            "local R=1",
            "echo after=$?",
        ]);
        assert_eq!(out, "after=2\n");
        assert!(err.contains("not in a function"), "{err}");
        // A swallow INSIDE a function that is still running must not take its
        // frame away: `local` there still works, and only the caller's does not.
        let (_, out, _) = run(
            "f() { command eval 'echo ${u:?bad}'; local Q=1; echo in=ok; }; f; echo done",
        );
        assert_eq!(out, "in=ok\ndone\n");
        // Same, but with an inner function dying, so a marker really is pushed and
        // carries `true`. This is what makes the value load-bearing rather than the
        // marker's presence: applying a hard-coded `false` reds only here.
        let (_, out, _) = run(
            "g() { echo ${u:?bad}; }; \
             f() { command eval 'g'; local Q=1; echo in=ok; }; f; echo done",
        );
        assert_eq!(out, "in=ok\ndone\n");
    }

    #[test]
    fn a_local_var_frame_comes_from_the_command_word() {
        // `local` tests only whether `evalcommand` pushed a frame (ash's
        // `localvar_stack`, ash.c:10047), and it pushes one for EVERY command with a
        // command word that is not a special builtin -- a regular builtin and an
        // external get one too, not just a function call. Every expectation below
        // was read off the td-built busybox ash and matches dash.
        for (body, framed) in [
            // Pushed: regular builtin, function, external. `command`'s own word is
            // regular, and `spclbltin` latches on the FIRST word, so what it goes on
            // to resolve cannot take the frame away again.
            ("A=${u:?e} true", true),
            ("A=${u:?e} f", true),
            ("A=${u:?e} /nonexistent/x", true),
            ("A=${u:?e} command true", true),
            ("A=${u:?e} command eval :", true),
            // Not pushed: a special builtin as that first word -- `local` included,
            // which is what keeps a top-level `local` an error.
            ("A=${u:?e} eval :", false),
            ("A=${u:?e} export X=1", false),
            ("A=${u:?e} local X=1", false),
            // Not pushed: no command word at all.
            ("${u:?e}", false),
            ("A=1 B=${u:?e}", false),
            // The words expand BEFORE the frame goes up, so an argument that dies
            // never sees one -- unlike a prefix assignment or a redirection word,
            // which are both processed after it.
            ("true ${u:?e}", false),
            ("f ${u:?e}", false),
            ("true >${u:?e}", true),
            ("f >${u:?e}", true),
            ("command >${u:?e} true", true),
            ("eval >${u:?e} :", false),
            (">${u:?e}", false),
        ] {
            let src = format!("f() {{ :; }}; trap 'local Q=1; echo FRAME' EXIT; {body}");
            let (_, out, err) = run(&src);
            assert_eq!(out.contains("FRAME"), framed, "{body}: out={out} err={err}");
            assert_eq!(err.contains("not in a function"), !framed, "{body}: {err}");
        }
        // The frame follows how the word RESOLVES, not how it is spelled: a function
        // shadowing a special builtin's name is a function and gets one. Measured on
        // ash alone -- dash rejects these function names with a syntax error.
        let (code, out, err) = run("eval() { exit 7; }; trap 'local Q=1; echo FRAME' EXIT; eval");
        assert_eq!((code, out.as_str()), (7, "FRAME\n"), "{err}");
        assert_eq!(run("export() { local Q=1; echo ok; }; export").1, "ok\n");
        // `source` and `times` were the two words special by NAME alone, back
        // when neither resolved to a `Builtin`; both resolve now and are framed
        // -- or not -- by the same rule as the other fifteen. A function of the
        // same name still is, by the rule just above.
        for w in ["times", "source"] {
            let (_, out, err) = run(&format!(
                "trap 'local Q=1; echo FRAME' EXIT; A=${{u:?e}} {w}"
            ));
            assert_eq!(out, "", "{w}");
            assert!(err.contains("not in a function"), "{w}: {err}");
            assert_eq!(run(&format!("{w}() {{ local Q=1; echo ok; }}; {w}")).1, "ok\n", "{w}");
        }
    }

    #[test]
    fn a_child_shell_inherits_the_frame_it_was_forked_inside() {
        // A subshell keeps the depth (its `local` is the enclosing function's to
        // declare) but not the bindings, and one forked at the top level has no
        // frame to inherit. Both measured on ash and dash.
        assert_eq!(run("f() { (local Q=1; echo ok); }; f").1, "ok\n");
        assert!(run("(local Q=1)").2.contains("not in a function"));
        // The child's own `local` must not follow the fork back out.
        assert_eq!(run("D=out; f() { (local D=in); echo [$D]; }; f").1, "[out]\n");
    }

    #[test]
    fn the_frame_is_standing_before_ps4_expands() {
        // ash's `pushlocalvars` precedes its xtrace block, and td-sh runs a `$( )` in
        // PS4 for real, so the order is observable here even though it is not in
        // either reference (both fail to re-parse such a PS4 at all). The
        // regular/special split still applies inside it.
        assert_eq!(run("PS4='$(local Q=1; echo F)'; set -x; true").2, "Ftrue\n");
        assert!(run("PS4='$(local Q=1; echo F)'; set -x; eval :")
            .2
            .contains("not in a function"));
    }

    #[test]
    fn command_carries_a_frame_that_a_special_builtin_never_gets() {
        // The live half of the same rule: `command` is regular, so everything it
        // resolves runs inside the frame it pushed, while `eval` reaching the very
        // same `local` directly has none. This is the pair that makes the frame a
        // property of the command word rather than of being inside a function.
        for (src, want, errors) in [
            ("command eval 'local Q=1; echo ok'", "ok\n", false),
            ("command local Q=1; echo ok", "ok\n", false),
            ("command command eval 'local Q=1; echo ok'", "ok\n", false),
            ("A=1 command eval 'local Q=1; echo ok'", "ok\n", false),
            ("eval 'local Q=1; echo BAD'", "", true),
            ("A=1 eval 'local Q=1; echo BAD'", "", true),
            ("local Q=1; echo BAD", "", true),
        ] {
            let (_, out, err) = run(src);
            assert_eq!(out, want, "{src}: {err}");
            assert_eq!(err.contains("not in a function"), errors, "{src}: {err}");
        }
        // It is still a SCRATCH frame: what it declares must not outlive it.
        assert_eq!(run("D=out; command eval 'local D=in'; echo [$D]").1, "[out]\n");
        // ...and its drain stops at its OWN mark. Run from an EXIT trap it sits on
        // top of the dying function's deferred frame, which it must leave standing:
        // drain to zero instead and the trap reads the global `X` rather than the
        // `local` the function died holding.
        assert_eq!(
            run("X=g; f() { local X=fn; ${u:?e}; }; trap 'command true; echo t=[$X]' EXIT; f").1,
            "t=[fn]\n",
        );
    }

    #[test]
    fn a_terminating_unwind_leaves_the_frame_for_the_exit_trap() {
        // dash longjmps past `poplocalvars`/`unwindlocalvars`, so the trap runs in
        // the frame the shell died in and sees its bindings. Every value here was
        // read off the td-built busybox ash.
        for (src, want) in [
            ("D=g; trap 'echo t=$D' EXIT; f() { D=body; exit 7; }; D=dd f", "t=body\n"),
            ("D=g; trap 'echo t=$D' EXIT; f() { exit 7; }; D=dd f", "t=dd\n"),
            ("D=g; trap 'echo t=$D' EXIT; f() { echo ${u:?bad}; }; D=dd f", "t=dd\n"),
            ("D=g; trap 'echo t=$D' EXIT; set -e; f() { false; }; D=dd f", "t=dd\n"),
            // `local` is the same rule, and it applied to `local` first.
            ("D=g; trap 'echo t=$D' EXIT; f() { local D=loc; exit 4; }; f", "t=loc\n"),
            ("D=g; trap 'echo t=$D' EXIT; g() { D=in; exit 5; }; f() { D=mid g; }; D=out f",
             "t=in\n"),
            // A frame only half applied when a readonly name rejected the next
            // assignment stays too -- the error is a terminating unwind as well.
            ("D=g; readonly R=r; trap 'echo t=$D' EXIT; f() { :; }; D=dd R=no f", "t=dd\n"),
            // A regular builtin's frame follows the same rule -- it is `evalcommand`
            // that skips the cleanup, not anything about functions.
            ("D=g; trap 'echo t=$D' EXIT; D=dd command exit 7", "t=dd\n"),
            ("D=g; readonly R=r; trap 'echo t=$D' EXIT; D=dd R=no true", "t=dd\n"),
            ("D=g; trap 'echo t=$D' EXIT; D=dd Y=${u:?bad} true", "t=dd\n"),
            // Not terminating: `return` unwinds normally and undoes the frame.
            ("D=g; trap 'echo t=$D' EXIT; f() { D=body; return 3; }; D=dd f", "t=g\n"),
            ("D=g; trap 'echo t=$D' EXIT; f() { local D=loc; }; f", "t=g\n"),
            // Nor is a failed redirection, which reports rather than raises; nor
            // `set -e` on a builtin, which fires after the frame is already gone.
            ("D=g; trap 'echo t=$D' EXIT; D=dd true >/nonexistent/d/f", "t=g\n"),
            ("D=g; trap 'echo t=$D' EXIT; set -e; D=dd false", "t=g\n"),
            // `command` wraps a builtin in a scratch `local` frame, which is a
            // frame like any other and stays standing on the way out.
            (
                "D=global; trap \"echo t=\\$D\" EXIT; \
                 f() { local D=f; command eval 'local D=scratch; exit 7'; }; f",
                "t=scratch\n",
            ),
        ] {
            let (_, out, _) = run(src);
            assert_eq!(out, want, "{src}");
        }
        // The argument frame is NOT deferred with the bindings: both references
        // show the caller's `$#` inside the trap.
        let (_, out, _) = run("trap 'echo [$1] $#' EXIT; f() { exit 3; }; f AA BB");
        assert_eq!(out, "[] 0\n");
        // The hidden `getopts` cursor travels with the argument frame, so the trap
        // resumes the CALLER's scan: `f` consumed two of its own three options, the
        // caller one of its two, and the trap picks up the caller's second.
        let (_, out, _) = run(
            "set -- -a -b; getopts ab o; f() { getopts ab i; getopts ab i; exit 7; }; \
             trap 'getopts ab o2; echo t=[$o2],$OPTIND' EXIT; f -a -b -a",
        );
        assert_eq!(out, "t=[b],3\n");
        // ...and an interactive shell that RECOVERS drops what was left standing,
        // rather than carrying it into the next command.
        for (bad, want) in [
            ("D=g; f() { echo ${u:?bad}; }; D=dd f", "after=g\n"),
            // The half-applied frame of the error path, which only a recovering
            // shell can observe -- a script dies with the binding still standing.
            ("D=g; readonly R=r; f() { :; }; D=dd R=no f", "after=g\n"),
            ("D=g; f() { local D=loc; echo ${u:?bad}; }; f", "after=g\n"),
            ("D=g; readonly R=r; D=dd R=no true", "after=g\n"),
            ("D=g; D=dd Y=${u:?bad} true", "after=g\n"),
        ] {
            let (_, out, _) =
                crate::process::run_capturing_interactive_units(&[bad, "echo after=$D"]);
            assert_eq!(out, want, "{bad}");
        }
    }

    #[test]
    fn a_recovery_undoes_only_what_it_deferred() {
        // A swallowed abort undoes the frames of the command that recovered, and
        // stops there: an OUTER unwind is still on its way out and its bindings
        // belong to the EXIT trap. Both values read off the td-built busybox ash.
        let (_, out, _) = run(
            "D=global; g() { local D=g; echo ${u:?bad}; }; \
             f() { local D=f; command eval 'local D=scratch; g'; echo after=$D; }; f",
        );
        assert_eq!(out, "after=f\n");
        // The mark: `f` died with `local D=f` standing, then the EXIT trap
        // recovered from an abort inside `g`. Only `g`'s frame may go.
        let (_, out, _) = run(
            "D=g; X=x; trap 'command eval \"g\"; echo after=$D,$X' EXIT; \
             g() { local X=gtrap; echo ${u:?bad}; }; f() { local D=f; exit 7; }; f",
        );
        assert_eq!(out, "after=f,x\n");
    }

    #[test]
    fn restoring_optind_moves_the_hidden_cursor_with_it() {
        // dash calls a variable's hook on the RESTORE too, so a frame that
        // displaced OPTIND puts the scan cursor back as well. Without it the
        // variable reads right and `getopts` still rescans the word the frame
        // pointed at -- which in a loop never advances.
        for src in [
            "set -- -a -b; getopts ab o; f() { :; }; OPTIND=1 f; getopts ab o",
            "set -- -a -b; getopts ab o; OPTIND=1 true; getopts ab o",
            "set -- -a -b; getopts ab o; f() { local OPTIND=1; }; f; getopts ab o",
        ] {
            let (_, out, _) = run(&format!("{src}; echo $o,$OPTIND"));
            assert_eq!(out, "b,3\n", "{src}");
        }
        // A half-consumed word is abandoned by the restore, as by any assignment.
        let (_, out, _) =
            run("set -- -ab; getopts ab o; f() { :; }; OPTIND=1 f; getopts ab o; echo $o,$OPTIND");
        assert_eq!(out, "?,2\n");
        // Restoring a name that did NOT exist is an unset, and ash fires the hook
        // for that too -- so the cursor goes back to word 1 rather than keeping
        // where the temporary value left it. Only a temp value above 1 can tell
        // those apart, which is why these use 2.
        for src in [
            "unset OPTIND; set -- -a -b; f() { :; }; OPTIND=2 f; getopts ab o",
            "unset OPTIND; set -- -a -b; OPTIND=2 true; getopts ab o",
        ] {
            let (_, out, _) = run(&format!("{src}; echo $o"));
            assert_eq!(out, "a\n", "{src}");
        }
    }

    #[test]
    fn an_abort_is_confined_by_every_stand_in_for_a_child_process() {
        // One case per `Sig::Exit(code) | Sig::Abort(code)` arm. Each value was
        // read off the td-built busybox ash and dash, which run these in a real
        // child -- so the status comes back and the enclosing list carries on.
        // Each raises through `Shell::fatal`, which sets no status of its own, so
        // the value can ONLY have come back through the arm under test -- with
        // `shift bad` the arm is unobservable, because `badnum` sets `$?` anyway.
        for (src, want) in [
            ("( echo ${x:?bad} ); echo $?", "2\n"),                 // subshell body
            ("echo a | { echo ${x:?bad}; }; echo $?", "2\n"),       // pipeline stage
            ("v=$(echo ${x:?bad}); echo $?", "2\n"),                // command sub
            ("( shift bad ); echo $?", "2\n"),
            // A background list reports 0 either way, so its own arm is only
            // visible from inside: the child's EXIT trap sees the aborted status.
            ("{ trap 'echo $?' EXIT; echo ${x:?bad}; } & wait; echo $?", "2\n0\n"),
        ] {
            let (status, out, _) = run(src);
            assert_eq!(out, want, "{src}");
            assert_eq!(status, 0, "{src}");
        }
        // The EXIT trap is the other arm: its abort becomes the shell's status.
        let (status, _, _) = run("trap 'shift bad' EXIT; true");
        assert_eq!(status, 2);
    }

    #[test]
    fn an_abort_stops_at_command_but_nowhere_else() {
        // dash re-raises EXERROR out of `evalbltin` only for a special COMMAND
        // WORD, so `command` -- which is not special -- turns one back into a
        // status and the list carries on.
        for src in [
            "command exit bad; echo SAME",
            "command shift bad; echo SAME",
            "command return bad; echo SAME",
            "f() { command exit bad; }; f; echo SAME",
        ] {
            let (status, out, _) = run(&format!("{src}; echo st=$?"));
            assert_eq!(out, "SAME\nst=0\n", "{src}");
            assert_eq!(status, 0, "{src}");
        }
        // Only `Abort`, though: a real `exit` through `command` still exits.
        let (status, out, _) = run("command exit 7; echo SAME");
        assert_eq!((status, out.as_str()), (7, ""));
        // And without `command` the same operand is fatal, as before.
        let (status, out, _) = run("shift bad; echo SAME");
        assert_eq!((status, out.as_str()), (2, ""));
    }

    #[test]
    fn a_syntax_error_abandons_the_list_it_was_parsed_in() {
        // Reported-and-continue let `eval 'if'; echo BAD` print BAD and exit 0;
        // both references abandon the list with status 2.
        for src in ["eval 'if'; echo BAD", ". /dev/null; eval 'for'; echo BAD"] {
            let (status, out, _) = run(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src}");
        }
    }

    #[test]
    fn a_prefix_assignment_is_rolled_back_even_when_the_command_aborts() {
        // The rollback used to sit after a `?`, so an abort mid-assignment left the
        // transient binding behind -- invisible while the shell then died, and not
        // once it survives.
        use crate::process::run_capturing_interactive_units;
        let (_, out, _) = run_capturing_interactive_units(&[
            "A=old; readonly R=old",
            "A=new R=new echo x",
            "echo A=$A",
        ]);
        assert_eq!(out, "A=old\n");
        let (_, out, _) = run_capturing_interactive_units(&[
            "A=old",
            "A=new echo hi >${UNSET_TARGET:?bad}",
            "echo A=$A",
        ]);
        assert_eq!(out, "A=old\n");
    }

    #[test]
    fn an_interactive_abort_leaves_the_loop_it_was_raised_in() {
        // The other half, and the one that used to hang: returning Ok to keep an
        // interactive shell alive resumed the loop the failing command was meant to
        // end. Bounded so a regression fails instead of wedging the gate.
        use crate::process::run_capturing_interactive_units;
        for spinner in [
            "for i in 1 2 3; do shift oops; done; echo SAME",
            "for i in 1 2 3; do echo tick; break oops; done; echo SAME",
        ] {
            let (_, out, _) = run_capturing_interactive_units(&[spinner, "echo NEXT"]);
            assert!(!out.contains("SAME"), "{spinner}: {out:?}");
            assert!(out.ends_with("NEXT\n"), "{spinner}: {out:?}");
        }
    }

    #[test]
    fn builtin_write_error_sets_nonzero_status() {
        // A builtin write to a closed descriptor (`>&-`) fails visibly ($?=1) rather
        // than being masked back to 0.
        let (_s, out, _e) = run("echo hi >&-; echo $?");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn errexit_triggers_on_a_builtin_write_error() {
        // Under `set -e` the failed write must abort the shell before `survived`.
        let (status, out, _e) = run("set -e; echo hi >&-; echo survived");
        assert_eq!(out, "");
        assert_eq!(status, 1);
    }

    #[test]
    fn subshell_redirection_target_side_effect_does_not_leak() {
        // A `${x:=…}` assignment in a SUBSHELL's redirection target stays in the
        // subshell (POSIX): the parent's `x` is untouched.
        let (_s, out, _e) = run("unset x; (:) >\"${x:=/dev/null}\"; echo \"${x-unset}\"");
        assert_eq!(out, "unset\n");
    }

    /// `$LINENO` is the line the COMMAND being run starts on -- dash's
    /// `lineno`, set once per command node (eval.c:751) rather than read off
    /// the scanner when the word is expanded. Every value asserted in these
    /// five tests was measured against dash 0.5.12 first.
    #[test]
    fn lineno_is_the_line_of_the_command() {
        assert_eq!(run("echo $LINENO\necho $LINENO\n\necho $LINENO").1, "1\n2\n4\n");
        // Blank and comment lines are counted but do not carry a command, so
        // the next one still reports its own.
        assert_eq!(run("# c\n\n# c\necho $LINENO").1, "4\n");
        // The word list of a `for` and the subject of a `case` belong to the
        // compound's own node, which is why both report the keyword's line and
        // the loop body reports its own on every iteration.
        assert_eq!(run("set -- a b\nfor x; do\n  echo $LINENO\ndone").1, "3\n3\n");
        assert_eq!(run("case $LINENO in\n  1) echo one ;;\n  *) echo no ;;\nesac").1, "one\n");
        // Both assignments of one command see one line, and a word carried
        // across a fold reports where its command OPENED, not where it sits.
        assert_eq!(run("a=$LINENO b=$LINENO\necho $a $b").1, "1 1\n");
        assert_eq!(run("echo one \\\n  $LINENO two").1, "one 1 two\n");
        // A here-document body is input the next command is past.
        assert_eq!(run("read x <<EOF\nbody\nEOF\necho $LINENO").1, "4\n");
        // dash takes the line with the command's first token already READ
        // (`savelinno` at the top of `simplecmd`, parser.c:524, after the
        // pushback), so a first word that itself spans lines reports where it
        // ENDS rather than where it opens. bash agrees; both measured at 2,
        // and the fold above is what keeps the two rules distinguishable.
        assert_eq!(run("x=\"a\nb\" y=$LINENO\necho $y").1, "2\n");
    }

    /// Per COMMAND and not per PIPELINE. dash sets the line at each NCMD and
    /// has no NPIPE case at all, so a pipeline spanning lines gives each stage
    /// its own -- measured at 2 under dash, where attaching the line to the
    /// pipeline would give 1.
    #[test]
    fn a_pipeline_stage_reports_its_own_line() {
        assert_eq!(run("true |\n  { read _; echo $LINENO; }").1, "2\n");
        // The stages run as concurrent threads, so this also pins that the
        // line is not one cell they share: each stage forks its own `Shell`.
        assert_eq!(run("echo $LINENO |\n  { read a; echo $a $LINENO; }").1, "1 2\n");
        // A stage that is a SIMPLE command is the only one that pins the
        // publish: a `{ …; }` stage has an inner command whose own line
        // overwrites it, so it passes even when nothing published the stage's.
        // The leading `:` is what makes the inherited line differ from 2.
        assert_eq!(run(":\necho $LINENO |\n  { read a; echo got=$a; }").1, "got=2\n");
    }

    /// dash reports a line inside a function RELATIVE to where the function
    /// was DEFINED (`funcline`, eval.c:752). busybox ash keeps the same
    /// variable but never subtracts it, so it and bash report the absolute
    /// line; the corpus grades this shell on dash's answer, its
    /// `$LINENO is the current line` case carrying a `BUG dash` block and no
    /// ash one.
    #[test]
    fn lineno_in_a_function_is_relative_to_its_definition() {
        assert_eq!(run("f() {\n  echo $LINENO\n}\nf").1, "2\n");
        assert_eq!(run("g() { echo $LINENO; }\ng").1, "1\n");
        // The subtraction is SAVED and restored rather than left standing: a
        // nested call is relative to its own definition, the caller resumes
        // relative to its, and the top level is absolute again afterwards.
        let src = "g() {\n  echo $LINENO\n}\nf() {\n  echo $LINENO\n  g\n  echo $LINENO\n}\nf\necho $LINENO";
        assert_eq!(run(src).1, "2\n2\n4\n10\n");
        // A definition made INSIDE a call records its absolute parse line, not
        // one already relative to the enclosing function.
        assert_eq!(run("f() {\n  g() {\n    echo $LINENO\n  }\n  g\n}\nf").1, "2\n");
        // The definition's line is the `)` TOKEN's, which is neither the name's
        // nor the body's: a definition folded across a `\` counts from the
        // parentheses, and a `{` on the next line does not move it. dash gives
        // 1 and 3.
        assert_eq!(run(":\n:\nfw \\\n() { echo $LINENO; }\nfw").1, "1\n");
        assert_eq!(run("f()\n{\n  echo $LINENO\n}\nf").1, "3\n");
        // The BODY is a command node too, so its own header expands under its
        // own line and not the CALLER's: a body with no inner command to
        // overwrite the line is the only place that shows.
        assert_eq!(run("f() for x in \"$LINENO\"; do echo $x; done\necho top\nf").1, "top\n1\n");
        assert_eq!(
            run("g() case $LINENO in 1) echo ONE;; *) echo \"OTHER=$LINENO\";; esac\necho top\ng").1,
            "top\nONE\n"
        );
        // And it really can go NEGATIVE, which is dash's plain signed
        // subtraction rather than an accident: a reparsed string starts at 1
        // while `funcline` is 4, measured at -2 in dash.
        assert_eq!(run(":\n:\n:\nf() {\n  eval 'echo $LINENO'\n}\nf").1, "-2\n");
    }

    /// An alias replacement stands where the NAME stood, so it is LEXED from
    /// there: dash reads one off the same input stream. Where dash goes
    /// further and this shell does not is the REST of the script -- dash
    /// shifts it by the newlines in the body, so a two-line body makes the
    /// file's line 4 report 5, which is a line number no line has.
    #[test]
    fn an_alias_body_is_lexed_where_the_name_stood() {
        assert_eq!(run("alias x='echo $LINENO'\necho top\nx\necho $LINENO").1, "top\n3\n4\n");
        // Including a `$( )` inside the body, which a separately-lexed
        // replacement would report as 1.
        assert_eq!(run("alias x='echo $(echo $LINENO)'\necho top\nx").1, "top\n3\n");
        // A body with a newline in it reports the line after for its second
        // command, as dash does -- and the line AFTER the invocation is still
        // its own, where dash shifts it to 5.
        let two = "alias x='echo $LINENO\necho $LINENO'\nx\necho $LINENO";
        assert_eq!(run(two).1, "3\n4\n4\n", "dash gives 3 4 5, shifting the rest of the file");
    }

    /// A substitution body is numbered the way it is PARSED. dash reads a
    /// `$( )` body from the outer input, so it counts on from the outer line;
    /// a backtick body is de-escaped and re-scanned as a string of its own, so
    /// it starts again at 1. bash numbers both absolutely -- this shell
    /// follows dash, and the two spellings are asserted against each other so
    /// neither can quietly take the other's rule.
    #[test]
    fn a_substitution_body_is_numbered_the_way_it_is_parsed() {
        assert_eq!(run("echo $(echo $LINENO)").1, "1\n");
        assert_eq!(run("echo x\necho $(echo $LINENO)").1, "x\n2\n");
        assert_eq!(run("echo \"$(\necho $LINENO\n)\"").1, "2\n");
        // Same body, same lines, one line number apart: the backtick is 2
        // because its body is re-scanned, the `$( )` 3 because it is not.
        assert_eq!(run("echo a\necho \"`\necho $LINENO\n`\"").1, "a\n2\n");
        assert_eq!(run("echo a\necho \"$(\necho $LINENO\n)\"").1, "a\n3\n");
        // `eval` re-parses a string, so it starts at 1 as dash's does.
        assert_eq!(run("echo x\neval 'echo $LINENO'").1, "x\n1\n");
        // An operand is a SUBSTRING of the outer input, so a `$( )` inside a
        // `${...}` word, an arithmetic body or a here-document body counts on
        // from the script too -- all three re-lex the text, and starting that
        // sub-lexer at 1 would report 1 where dash reports 3, 3 and 4.
        assert_eq!(run("echo a\necho b\necho \"${u:-$(echo $LINENO)}\"").1, "a\nb\n3\n");
        assert_eq!(run("echo a\necho b\necho $(( $(echo $LINENO) + 0 ))").1, "a\nb\n3\n");
        assert_eq!(run("echo a\necho b\nread x <<EOF\n$(echo $LINENO)\nEOF\necho $x").1, "a\nb\n4\n");
        // An operand's own offset inside the braces counts as well.
        assert_eq!(run("echo a\necho $((1 +\n$(echo $LINENO) ))").1, "a\n4\n");
        // A patsub REPLACEMENT is the one operand a newline can precede,
        // because the pattern before it may span lines. dash has no patsub to
        // measure this against; what it pins is self-consistency with the
        // `${` two lines above it.
        assert_eq!(run("v='a\nb'\necho \"${v/a\nb/$(echo $LINENO)}\"").1, "4\n");
    }

    /// dash formats the line into ONE static buffer on each read (var.c:317)
    /// and exports that buffer verbatim, so what a child inherits is the value
    /// the last read produced -- empty before the first, and NOT the line the
    /// `exec` is on. It is the same write-back `$RANDOM` has, so LINENO uses
    /// the same mechanism rather than a case in the environment builder.
    #[test]
    fn an_exported_lineno_carries_the_last_value_read() {
        // On the shell's stack like every other way into the evaluator: this one
        // keeps the shell afterwards, so it cannot use `run_capturing`.
        let seen = |src: &str| {
            crate::process::on_shell_stack(|| {
                let mut sh = super::Shell::new_for_test();
                super::run_program(&mut sh, src);
                // A plain loop rather than the searching iterator adaptor: this
                // file is written into the image by a `WriteFile`, which the
                // ladder's host-findutils guard scans as a command surface, so
                // that tool's name may not appear here at all -- not even in a
                // comment. `recipes/src/recipes/td-sh.rs` states it.
                let mut got = None;
                for (k, v) in sh.exported_env() {
                    if k == "LINENO" {
                        got = Some(v);
                    }
                }
                got
            })
            .expect("could not start the shell thread")
        };
        // Set-and-EMPTY before any read, which is what dash's child sees --
        // not absent, and not the line the export is on.
        assert_eq!(seen("export LINENO").as_deref(), Some(""));
        assert_eq!(seen("export LINENO\n:\n:\n:").as_deref(), Some(""));
        // ...then the value the LAST read produced, not the current line.
        assert_eq!(seen("export LINENO\necho $LINENO >&-\n:\n:").as_deref(), Some("2"));
        // A frozen LINENO exports what was stored, like any other variable.
        assert_eq!(seen("export LINENO\nLINENO=42").as_deref(), Some("42"));
    }

    /// dash's LINENO auto-updates only while nothing has been STORED in it
    /// (`v->text == linenovar`, var.c:316), which covers an assignment and an
    /// inherited `LINENO=50` with one rule. bash instead ignores the
    /// assignment and keeps reporting the line.
    #[test]
    fn storing_in_lineno_ends_it() {
        assert_eq!(run("LINENO=99\necho $LINENO").1, "99\n");
        assert_eq!(run("unset LINENO\necho [$LINENO]").1, "[]\n");
        // A valueless `local` UNSETS, which is ash's rule (`mklocal` at
        // ash.c:10028 says so in as many words) and the rule this shell already
        // followed -- so it takes the tracking with it for the call, and the
        // frame's restore brings it back. dash is the odd one out here, as it
        // is for `local` generally: its `mklocal` keeps the text, so `local
        // LINENO` there still reports the line (measured: `[1]`, and `got=1`
        // rather than an abort under `set -u`).
        assert_eq!(run("f() { local LINENO; echo \"[$LINENO]\"; }\nf\necho $LINENO").1, "[]\n3\n");
        assert_eq!(run("f() { local LINENO; LINENO=7; echo $LINENO; }\nf\necho $LINENO").1, "7\n3\n");
        // The readonly exemption is RANDOM's alone -- ash grants it to its one
        // dynamic name, and dash refuses this with `is read only`, measured.
        let (status, _out, err) = run("readonly LINENO\nLINENO=5\necho after");
        assert_eq!(status, 2);
        assert!(err.contains("read only"), "no diagnostic: {err:?}");
    }
}
