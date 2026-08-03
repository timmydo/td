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
use std::sync::Arc;

use crate::ast::{AndOr, Cmd, Conn, List, Pipeline, Redir, Sep, Word};
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
    pub funcs: HashMap<String, Arc<Cmd>>,
    pub params: Vec<String>, // positional parameters $1..
    pub arg0: String,        // $0
    pub status: i32,         // $?
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
    /// "ignore". Only EXIT is ever RUN: delivering a real signal needs a handler
    /// this shell cannot install (see the crate-root note), so the rest are kept
    /// so that `trap` reports them faithfully.
    pub traps: BTreeMap<u8, String>,
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
/// `getppid(2)` or `gethostname(2)` -- a new syscall is an AGENTS.md amendment,
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
                },
            );
        }
        let mut sh = Shell {
            vars,
            funcs: HashMap::new(),
            params: Vec::new(),
            arg0: "td-sh".to_string(),
            status: 0,
            last_bg: None,
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
            in_ps4: false,
            getopts_optind: 1,
            getopts_off: -1,
            aliases: Aliases::new(),
            cloned: false,
            traps: BTreeMap::new(),
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
            arg0: "td-sh".to_string(),
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
            in_ps4: false,
            getopts_optind: 1,
            getopts_off: -1,
            aliases: Aliases::new(),
            cloned: false,
            traps: BTreeMap::new(),
            trap_status: None,
        };
        let _ = sh.set_var("IFS", " \t\n");
        let _ = sh.set_var("OPTIND", "1");
        let _ = sh.set_var("PS4", "+ ");
        sh
    }

    pub fn get_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).and_then(|v| v.value.clone())
    }

    /// Assign a shell variable, honouring the readonly attribute. A write to a
    /// readonly name is dash's sh_error, so it goes out as `Sig::Abort` and `?`
    /// carries it to the nearest handler rather than leaving a status to test.
    /// dash's OPTIND hook (`getoptsreset`): any assignment moves the cursor and
    /// abandons a half-consumed word. dash's number() takes an all-digit string
    /// only (so " 2", "+1" and "-1" are all rejected) and coerces 0 up to 1; a
    /// rejected value parks -1, which `getopts` reports when it next runs.
    fn var_hook(&mut self, name: &str, value: &str) {
        if name != "OPTIND" {
            return;
        }
        let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        self.getopts_optind =
            if digits { value.parse::<i64>().unwrap_or(i64::MAX).max(1) } else { -1 };
        self.getopts_off = -1;
    }

    /// The same hook for the value going AWAY. ash fires it on an unset too
    /// (`unsetvar` is `setvar(s, NULL, 0)`), where `getoptsreset` restarts the
    /// scan at word 1 for anything that is not a number (ash.c:2272) rather than
    /// parking the error an assignment parks. dash rejects `unset OPTIND`
    /// outright, so ash decides this one.
    fn unset_hook(&mut self, name: &str) {
        if name == "OPTIND" {
            self.getopts_optind = 1;
            self.getopts_off = -1;
        }
    }

    pub fn set_var(&mut self, name: &str, value: &str) -> R<()> {
        // Assigning OPTIND at all -- even the value it already holds -- restarts
        // `getopts` at a word boundary. `getopts` itself re-establishes the
        // offset after publishing OPTIND.
        self.var_hook(name, value);
        match self.vars.get_mut(name) {
            // dash reports this through sh_error, which ends a non-interactive
            // shell with status 2 -- not a status a script can test.
            Some(v) if v.readonly => {
                return Err(self.fatal(&format!("{name}: is read only"), 2));
            }
            Some(v) => {
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
                },
            );
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
                    exported: false,
                    readonly: true,
                    localised: false,
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

    pub fn unset_var(&mut self, name: &str) -> bool {
        if self.vars.get(name).is_some_and(|v| v.readonly) {
            return false;
        }
        // Under `set -a`, `setvareq` ORs `VEXPORT` into the flags an unset writes
        // (ash.c:2417), so the free test below it can never hold: the entry
        // survives -- created, if the name was new -- and only the value goes.
        if self.opts.allexport {
            return self.unset_value(name);
        }
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
        if self.vars.get(name).is_some_and(|v| v.readonly) {
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
    let status = match run_source(sh, src, "") {
        Ok(()) => sh.status,
        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
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
        Err(Sig::Abort(code)) => {
            // The shell is recovering, not exiting, so whatever the unwind left
            // standing goes now rather than into the next prompt. Nothing outer
            // survives a top-level recovery, so the mark is 0.
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
    let saved = sh.trap_status.replace(status);
    let code = match run_source(sh, &action, "") {
        Ok(()) => status,
        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
        Err(_) => status,
    };
    sh.trap_status = saved;
    code
}

/// Run `src` one top-level unit at a time, as dash reads a script: a command is
/// parsed only once everything before it has run. That is what makes an `alias`
/// visible to the next line but not to the rest of its own line. A syntax error
/// stops the run with status 2, reported as `td-sh: {what}{error}`.
pub fn run_source(sh: &mut Shell, src: &str, what: &str) -> R<()> {
    let mut units = parser::Units::new(src);
    loop {
        match units.next_unit(&sh.aliases) {
            None => return Ok(()),
            Some(Err(e)) => {
                let _ = write_stderr(sh, &format!("td-sh: {what}{e}"));
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
            // No job control yet: run the async list in an ISOLATED subshell so its
            // variable/cwd/option changes cannot leak, then continue immediately
            // with $?=0. True background execution, a real $! and a functional
            // `wait` are deferred (see the crate-root note); $! is a placeholder.
            let mut child = process::fork_shell(sh);
            let status = match run_and_or(&mut child, and_or) {
                Ok(()) => child.status,
                Err(Sig::Exit(code) | Sig::Abort(code)) => code,
                Err(_) => child.status,
            };
            let _ = run_exit_trap(&mut child, status);
            sh.last_bg = Some(std::process::id());
            sh.set_status(0);
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

fn run_and_or(sh: &mut Shell, and_or: &AndOr) -> R<()> {
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
/// suppression scope, not a post-hoc check. Only the final, non-negated operand,
/// once run, is subject to `errexit`.
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
    if !exempt {
        maybe_errexit(sh)?;
    }
    Ok(())
}

fn maybe_errexit(sh: &mut Shell) -> R<()> {
    if sh.opts.errexit && sh.errexit_suppressed == 0 && sh.status != 0 {
        return Err(Sig::Exit(sh.status));
    }
    Ok(())
}

fn run_pipeline(sh: &mut Shell, pipe: &Pipeline) -> R<()> {
    if pipe.cmds.len() == 1 {
        if let Some(cmd) = pipe.cmds.first() {
            run_command(sh, cmd)?;
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
    result
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
        Cmd::FuncDef { name, body } => {
            sh.funcs.insert(name.clone(), body.clone());
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
    let _ = match errout {
        Some(target) => process::write_target(target, line.as_bytes()),
        None => process::write_fd(sh, 2, line.as_bytes()),
    };
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
        sh.funcs.contains_key(w) || !builtin::blocks_localvar_frame(w)
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
    if let Some(cmd) = argv.first().and_then(|name| sh.funcs.get(name)).cloned() {
        return call_function(sh, &cmd, argv, assigns, redirs);
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
                process::exec_external(sh, argv, None)
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
        if matches!(bi, builtin::Builtin::Exec) && argv.len() == 1 {
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
/// boundary. dash keys it on the COMMAND WORD's own flags (`spclbltin` is locked
/// to the first word), so only `command`, which is not special and which resolves
/// what follows, hands the error back as a status.
fn swallows_abort(bi: builtin::Builtin) -> bool {
    matches!(bi, builtin::Builtin::Command)
}

/// Whether this signal is on its way out of the shell rather than out of a
/// construct. Only these two skip a frame's cleanup; `return` and `break` unwind
/// normally and undo it.
pub fn terminating(sig: &Sig) -> bool {
    matches!(sig, Sig::Exit(_) | Sig::Abort(_))
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
            // `getopts` resumes at the word the frame's value pointed at. An
            // attributes-only entry restores as the unset it is.
            match var.value.as_deref() {
                Some(v) => sh.var_hook(&name, v),
                None => sh.unset_hook(&name),
            }
            sh.vars.insert(name, var);
        }
        Local::Var(name, None) => {
            sh.unset_hook(&name);
            sh.vars.remove(&name);
        }
        Local::Opts(opts) => sh.opts = opts,
        Local::Depth(was) => sh.localvar_depth = was,
    }
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
    body: &Arc<Cmd>,
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
    // Not `?`: a fatal error in a redirection WORD (`f 2>${u:?}`) must still unwind
    // the argument frame below, or the caller -- or an EXIT trap -- sees the
    // function's `$1`/`$#`.
    let result = match process::apply_redirs(sh, redirs) {
        Ok(process::RedirOutcome::Applied(saved)) => {
            let r = run_command(sh, body);
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
        process::RedirOutcome::Failed => return Ok(()),
    };
    let result = body(sh);
    process::restore_redirs(sh, saved);
    result
}

/// `$(...)` / `` `...` ``: run the code with stdout captured, strip trailing
/// newlines, and return the text. Runs in a subshell so its state changes do not
/// leak, matching POSIX.
pub fn command_subst(sh: &mut Shell, code: &str) -> R<String> {
    // Nesting is bounded centrally in `run_command` (the substituted body re-enters
    // there), so `$( $( … ) )` errors instead of overflowing the stack.
    // Counted so an assignment-only command can adopt the last substitution's $?.
    sh.cmdsubst_count = sh.cmdsubst_count.wrapping_add(1);
    let mut out = process::capture_stdout(sh, code)?;
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

/// Write `msg` plus a newline to the shell's current stderr.
pub fn write_stderr(sh: &Shell, msg: &str) -> std::io::Result<()> {
    process::write_fd(sh, 2, format!("{msg}\n").as_bytes())
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
            "set -- -a; OPTIND=abc; getopts a o; echo SAME",
        ] {
            let (status, out, _) =
                run_capturing_interactive_units(&[bad, "set +u", "echo NEXT"]);
            assert_eq!(out, "NEXT\n", "{bad}");
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
        // `source` and `times` are special to ash but unimplemented here, so without
        // naming them they would reach the external path and be framed. A function
        // of the same name still is, by the rule just above.
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
        // for that too -- restarting at word 1 rather than parking the error a
        // non-numeric ASSIGNMENT parks. Only a temp value above 1 can tell the two
        // apart, which is why these use 2.
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
}
