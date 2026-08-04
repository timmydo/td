//! I/O plumbing: a per-shell descriptor table, redirection application,
//! pipelines, subshells and external-command spawning.
//!
//! The shell keeps its OWN file-descriptor table (`Fds`) rather than dup2'ing
//! real kernel descriptors, because `std` exposes no `dup2`/`fork` and the
//! crate's one `unsafe` surface is `sys.rs`'s `umask(2)`, nothing else.
//! Builtins and shell functions read and write through
//! this table; only when an *external* program runs is the table translated into
//! `std::process::Command` stdio. Pipelines between builtins are run stage by
//! stage with the previous stage's output buffered as the next stage's input —
//! correct for every finite producer, which is the whole seed corpus and the
//! overwhelming majority of scripts. True concurrent pipes are a later refinement.
//!
//! The virtual table is why `exec cmd` hands the child only descriptors 0/1/2:
//! passing a higher one across an `execve` needs a `pre_exec` `dup2` (unsafe) or a
//! real `fork`, so `exec 3>f; cmd >&3` works (3 is remapped onto a standard
//! descriptor for the child) while `exec 3>f; cmd` cannot let `cmd` see fd 3.
//! A `Fd::Closed` likewise reaches the child as `/dev/null` rather than closed.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::ast::{Cmd, List, Redir, RedirKind};
use crate::exec::{self, Shell, Sig, R};

/// One entry in the shell's descriptor table. Everything shareable is behind an
/// `Arc<Mutex<…>>` so a subshell or pipeline stage inherits the same open file
/// (and its offset), the way a real dup'd descriptor would.
#[derive(Clone)]
pub enum Fd {
    /// The process's real stdin/stdout/stderr (0/1/2).
    Inherit(u8),
    File(Arc<Mutex<File>>),
    ReadBuf(Arc<Mutex<Cursor<Vec<u8>>>>),
    WriteBuf(Arc<Mutex<Vec<u8>>>),
    Null,
    Closed,
}

pub struct Fds {
    map: HashMap<u32, Fd>,
}

impl Fds {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(0, Fd::Inherit(0));
        map.insert(1, Fd::Inherit(1));
        map.insert(2, Fd::Inherit(2));
        Fds { map }
    }

    fn get(&self, fd: u32) -> Option<&Fd> {
        self.map.get(&fd)
    }

    /// Whether descriptor `fd` is a terminal. Only an INHERITED descriptor can
    /// be: the shell's own table means a redirection to a file or to an internal
    /// pipeline buffer must answer no, which `IsTerminal` on the process's real
    /// stream would get wrong.
    pub fn is_terminal(&self, fd: u32) -> bool {
        use std::io::IsTerminal;
        match self.get(fd) {
            Some(Fd::Inherit(0)) => std::io::stdin().is_terminal(),
            Some(Fd::Inherit(1)) => std::io::stdout().is_terminal(),
            Some(Fd::Inherit(2)) => std::io::stderr().is_terminal(),
            // An opened `/dev/tty` is one too, which `read -p` turns on.
            Some(Fd::File(f)) => f.lock().is_ok_and(|g| g.is_terminal()),
            _ => false,
        }
    }

    fn set(&mut self, fd: u32, target: Fd) {
        self.map.insert(fd, target);
    }
}

impl Default for Fds {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Fds {
    fn clone(&self) -> Self {
        Fds {
            map: self.map.clone(),
        }
    }
}

/// Write bytes to a shell descriptor. An unwritable or closed descriptor is an
/// error, surfaced to the caller as an `io::Error` (EBADF-like).
pub fn write_fd(sh: &Shell, fd: u32, bytes: &[u8]) -> std::io::Result<()> {
    write_target(sh.fds.get(fd), bytes)
}

/// Write to a descriptor's target directly, for a caller holding one that is no
/// longer in the table -- dash's `preverrout`.
pub fn write_target(target: Option<&Fd>, bytes: &[u8]) -> std::io::Result<()> {
    match target {
        Some(Fd::Inherit(0)) | Some(Fd::Closed) | None => Err(std::io::Error::other("bad file descriptor")),
        Some(Fd::Inherit(1)) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(bytes)?;
            lock.flush()
        }
        Some(Fd::Inherit(_)) => {
            let stderr = std::io::stderr();
            let mut lock = stderr.lock();
            lock.write_all(bytes)?;
            lock.flush()
        }
        Some(Fd::File(f)) => lock_write(f, bytes),
        Some(Fd::WriteBuf(b)) => {
            if let Ok(mut v) = b.lock() {
                v.extend_from_slice(bytes);
                Ok(())
            } else {
                Err(std::io::Error::other("poisoned buffer"))
            }
        }
        Some(Fd::ReadBuf(_)) => Err(std::io::Error::other(
            "descriptor not open for writing",
        )),
        Some(Fd::Null) => Ok(()),
    }
}

fn lock_write(f: &Arc<Mutex<File>>, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = f
        .lock()
        .map_err(|_| std::io::Error::other("poisoned file"))?;
    file.write_all(bytes)?;
    file.flush()
}

/// Read the next byte from a shell descriptor, or `None` at end of input.
pub fn read_byte(sh: &Shell, fd: u32) -> std::io::Result<Option<u8>> {
    let mut one = [0u8; 1];
    let n = match sh.fds.get(fd) {
        Some(Fd::Inherit(0)) => {
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            lock.read(&mut one)?
        }
        Some(Fd::File(f)) => {
            let mut file = f
                .lock()
                .map_err(|_| std::io::Error::other("poisoned file"))?;
            file.read(&mut one)?
        }
        Some(Fd::ReadBuf(b)) => {
            let mut cur = b
                .lock()
                .map_err(|_| std::io::Error::other("poisoned buffer"))?;
            cur.read(&mut one)?
        }
        Some(Fd::Null) => 0,
        _ => {
            return Err(std::io::Error::other(
                "descriptor not open for reading",
            ))
        }
    };
    Ok(if n == 0 { None } else { Some(one[0]) })
}

/// Whether a read on `fd` is READY -- meaning it would not block, which is what
/// `poll(2)` answers and hence what `read -t 0` and `-t N` turn on. NOT "bytes
/// remain": a file at EOF, an exhausted buffer and a closed descriptor are all
/// ready, because a read on them returns (0, or an error) immediately. `None`
/// means td-sh cannot tell, which is exactly the inherited descriptor -- it may
/// be a pipe or a terminal with nothing in it, and only `poll(2)` distinguishes
/// that from one holding data. td-sh's syscall surface is `umask(2)` and adding
/// to it is an UNSAFE.md amendment, so the caller reports the limit rather than
/// guessing.
pub fn read_ready(sh: &Shell, fd: u32) -> Option<bool> {
    match sh.fds.get(fd) {
        // Everything td-sh's own table holds is ready; only what it inherited
        // from the host can block.
        Some(Fd::Null) | Some(Fd::File(_)) | Some(Fd::ReadBuf(_)) | Some(Fd::Closed)
        | Some(Fd::WriteBuf(_)) | None => Some(true),
        Some(Fd::Inherit(_)) => None,
    }
}

/// A descriptor saved by `apply_redirs` so `restore_redirs` can put it back.
pub struct Saved {
    entries: Vec<(u32, Option<Fd>)>,
}

impl Saved {
    /// The fd 2 that was in effect BEFORE this command's redirections, if they
    /// touched it. dash saves it as `preverrout` (REDIR_SAVEFD2) so `set -x`
    /// still reports a command that sends its own stderr elsewhere.
    ///
    /// The outer `Option` says whether fd 2 was redirected at all; the inner one
    /// distinguishes "had no entry before" from "had this target", so a caller
    /// cannot mistake an absent prior stderr for an untouched one and write the
    /// trace into the very file the command redirected to.
    pub fn prev_stderr(&self) -> Option<Option<&Fd>> {
        // The FIRST entry for fd 2 is the pre-command one; a later `2>` in the
        // same command saved only the value an earlier one had just installed.
        for (fd, prev) in &self.entries {
            if *fd == 2 {
                return Some(prev.as_ref());
            }
        }
        None
    }
}

/// The result of applying one command's redirections.
pub enum RedirOutcome {
    /// All redirections applied; here is what to restore afterward.
    Applied(Saved),
    /// A redirection failed to open/dup a target (message already printed, `$?`
    /// set to 1). The command must NOT run. POSIX makes this fatal only for a
    /// special built-in; for every other command the caller skips it and keeps the
    /// shell alive. (Contrast an *expansion* error in a target — `>${x:?}` — which
    /// is always fatal and propagates as `Err(Sig)`.)
    Failed,
}

/// Apply a command's redirections to the descriptor table, returning what to
/// restore. Order matters: `2>&1 1>file` differs from `1>file 2>&1`.
pub fn apply_redirs(sh: &mut Shell, redirs: &[Redir]) -> R<RedirOutcome> {
    let mut saved = Saved {
        entries: Vec::with_capacity(redirs.len()),
    };
    for r in redirs {
        let fd = default_fd(r);
        let prev = sh.fds.map.get(&fd).cloned();
        match open_redir(sh, r) {
            Ok(Ok(target)) => {
                sh.fds.set(fd, target);
                saved.entries.push((fd, prev));
            }
            Ok(Err(())) => {
                // A recoverable open/dup failure: roll back this command's earlier
                // redirections, set the failure status, and report "skip".
                restore_redirs(sh, saved);
                sh.set_status(1);
                return Ok(RedirOutcome::Failed);
            }
            Err(sig) => {
                // A fatal expansion error in a later target word (`>${x:?}`) still
                // rolls back the redirections already applied so the fd table is not
                // left corrupted before the error unwinds.
                restore_redirs(sh, saved);
                return Err(sig);
            }
        }
    }
    Ok(RedirOutcome::Applied(saved))
}

pub fn restore_redirs(sh: &mut Shell, saved: Saved) {
    // Restore in reverse so a doubly-redirected fd lands on its original value.
    for (fd, prev) in saved.entries.into_iter().rev() {
        match prev {
            Some(target) => sh.fds.set(fd, target),
            None => {
                sh.fds.map.remove(&fd);
            }
        }
    }
}

fn default_fd(r: &Redir) -> u32 {
    if let Some(fd) = r.fd {
        return fd;
    }
    match &r.kind {
        RedirKind::In | RedirKind::DupIn | RedirKind::Here(_) | RedirKind::ReadWrite => 0,
        _ => 1,
    }
}

/// Open one redirection's target descriptor. An error in *expanding* the target
/// word (`>${x:?}`, `set -u`) is fatal and propagates as `Err(Sig)`. A failure to
/// open/dup the resulting target is recoverable: the message is printed and
/// `Ok(Err(()))` is returned so `apply_redirs` can turn it into a skipped command.
fn open_redir(sh: &mut Shell, r: &Redir) -> R<Result<Fd, ()>> {
    match &r.kind {
        RedirKind::Here(body) => {
            let text = exec::here_body(sh, body)?;
            Ok(Ok(Fd::ReadBuf(Arc::new(Mutex::new(Cursor::new(
                text.into_bytes(),
            ))))))
        }
        RedirKind::DupIn | RedirKind::DupOut => {
            let target = exec::redir_target(sh, r)?;
            Ok(dup_target(sh, &target))
        }
        RedirKind::In => open_file(sh, r, OpenOptions::new().read(true)),
        RedirKind::Out | RedirKind::Clobber => {
            if matches!(r.kind, RedirKind::Out) && sh.opts.noclobber {
                // `set -C`: refuse to truncate an existing regular file.
                let path = exec::redir_target(sh, r)?;
                if sh.resolve(&path).is_file() {
                    let _ = exec::write_stderr(sh, &format!("{path}: cannot overwrite existing file"));
                    return Ok(Err(()));
                }
            }
            open_file(
                sh,
                r,
                OpenOptions::new().write(true).create(true).truncate(true),
            )
        }
        RedirKind::Append => open_file(
            sh,
            r,
            OpenOptions::new().write(true).create(true).append(true),
        ),
        RedirKind::ReadWrite => {
            open_file(sh, r, OpenOptions::new().read(true).write(true).create(true))
        }
    }
}

/// `>&2`, `<&0`, `>&-` (close). A numeric target dups that descriptor; `-` closes.
/// Returns `Err(())` (message already printed) for a recoverable bad/ambiguous target.
fn dup_target(sh: &mut Shell, target: &str) -> Result<Fd, ()> {
    if target == "-" {
        return Ok(Fd::Closed);
    }
    match target.parse::<u32>() {
        Ok(n) => match sh.fds.get(n) {
            Some(fd) => Ok(fd.clone()),
            None => {
                let _ = exec::write_stderr(sh, &format!("{n}: bad file descriptor"));
                Err(())
            }
        },
        Err(_) => {
            let _ = exec::write_stderr(sh, &format!("{target}: ambiguous redirect"));
            Err(())
        }
    }
}

fn open_file(sh: &mut Shell, r: &Redir, opts: &OpenOptions) -> R<Result<Fd, ()>> {
    let name = exec::redir_target(sh, r)?;
    if name == "/dev/null" {
        return Ok(Ok(Fd::Null));
    }
    let path = sh.resolve(&name);
    match opts.open(&path) {
        Ok(f) => Ok(Ok(Fd::File(Arc::new(Mutex::new(f))))),
        Err(e) => {
            let _ = exec::write_stderr(sh, &format!("{name}: {e}"));
            Ok(Err(()))
        }
    }
}

/// Run a pipeline of two or more stages. Each stage's stdout is captured and
/// handed to the next stage as its stdin; the last stage keeps the shell's real
/// stdout. The pipeline's status is the last stage's (POSIX).
///
/// Every stage runs in its OWN subshell environment (a `fork_shell` clone), so a
/// stage's assignments, `cd`, `exit`, `break`/`continue`/`return`, and option
/// changes affect neither the parent shell nor a sibling stage — matching POSIX,
/// which specifies each pipeline command in a separate environment. Stages are
/// still run sequentially with the producer's output fully buffered before the
/// consumer starts (correct for every finite producer; true concurrent streaming
/// is a later refinement — see the module header).
pub fn run_pipeline(sh: &mut Shell, cmds: &[Cmd]) -> R<()> {
    let mut input: Option<Vec<u8>> = None;
    let last = cmds.len().saturating_sub(1);
    let mut last_status = 0;
    for (i, cmd) in cmds.iter().enumerate() {
        let is_last = i == last;
        let mut stage = fork_shell(sh);

        // Feed the previous stage's output in as this stage's stdin.
        if let Some(bytes) = input.take() {
            stage
                .fds
                .set(0, Fd::ReadBuf(Arc::new(Mutex::new(Cursor::new(bytes)))));
        }
        // Capture stdout unless this is the final stage, which keeps the parent's
        // real destination (inherited by the clone).
        let capture = if is_last {
            None
        } else {
            let buf = Arc::new(Mutex::new(Vec::new()));
            stage.fds.set(1, Fd::WriteBuf(buf.clone()));
            Some(buf)
        };

        // A non-local transfer (`exit`, break/continue/return) is confined to the
        // stage's subshell; only its exit status survives.
        last_status = match exec::run_command(&mut stage, cmd) {
            Ok(()) => stage.status,
            Err(Sig::Exit(code) | Sig::Abort(code)) => code,
            Err(_) => stage.status,
        };
        last_status = exec::run_exit_trap(&mut stage, last_status);

        input = capture.and_then(|buf| buf.lock().ok().map(|v| v.clone()));
    }
    sh.set_status(last_status);
    Ok(())
}

/// `( list )`: run in a cloned shell so nothing the subshell does — variables,
/// cwd, options, traps — is visible afterward. Only `$?` comes back.
pub fn run_subshell(sh: &mut Shell, body: &List, redirs: &[Redir]) -> R<()> {
    // The redirections belong to the SUBSHELL's environment, so apply them to the
    // clone — never the parent. Otherwise a target-word side effect leaks out
    // (`unset x; (:) >${x:=/dev/null}; echo ${x-unset}` must print `unset`). The
    // clone is discarded afterward, so its fd table needs no restore.
    let mut child = fork_shell(sh);
    let status = match apply_redirs(&mut child, redirs) {
        Ok(RedirOutcome::Applied(_saved)) => match exec::run_list(&mut child, body) {
            Ok(()) => child.status,
            Err(Sig::Exit(code) | Sig::Abort(code)) => code,
            // break/continue/return that escape a subshell are confined to it.
            Err(_) => child.status,
        },
        // A failed redirection skips the subshell body; `$?` is already 1.
        Ok(RedirOutcome::Failed) => child.status,
        // A fatal expansion error in a target word (`>${x:?}`) exits the SUBSHELL,
        // not the parent — confine it here rather than propagating.
        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
        Err(_) => child.status,
    };
    let status = exec::run_exit_trap(&mut child, status);
    sh.set_status(status);
    Ok(())
}

/// The file-creation mask, saved and put back on `Drop`.
///
/// It is the one piece of subshell state that is NOT in `Shell`: the mask lives
/// in the kernel, one per process. ash forks, so a subshell's `umask` cannot
/// reach the parent; td-sh's subshells are in-process clones, so the
/// save/restore a fork gives for free has to be explicit. Restoring on `Drop`
/// is what carries it across the `?`-shaped exits those bodies take.
struct UmaskScope(u32);

impl UmaskScope {
    fn capture() -> Self {
        Self(crate::sys::get())
    }
}

impl Drop for UmaskScope {
    fn drop(&mut self) {
        // Nothing to report to: the mask was this process's a moment ago, so a
        // refusal here would mean the kernel changed its mind about a value it
        // already accepted.
        let _ = crate::sys::set(self.0);
    }
}

/// A cloned shell environment and the mask guard it cannot be separated from.
///
/// The guard is bundled here rather than left to each caller because there is
/// no way to tell from a subshell's OUTPUT that the mask leaked -- it shows up
/// later, in the permissions of a file some unrelated command creates. So
/// `fork_shell` hands out no bare `Shell`: a subshell construct added later
/// gets the save/restore whether or not its author knew to ask. Derefs to
/// `Shell`, so callers use it as one.
pub struct Subshell {
    shell: Shell,
    _mask: UmaskScope,
}

impl std::ops::Deref for Subshell {
    type Target = Shell;
    fn deref(&self) -> &Shell {
        &self.shell
    }
}

impl std::ops::DerefMut for Subshell {
    fn deref_mut(&mut self) -> &mut Shell {
        &mut self.shell
    }
}

/// A child shell that shares the parent's open descriptors (so redirections and
/// captured output flow through) but owns an independent copy of the mutable
/// state a subshell must not leak back. Recursion/substitution counters and the
/// `errexit`-suppression depth are inherited: a subshell or command substitution
/// spawned while evaluating an `if`/`while` condition (or a non-final `&&`/`||`
/// operand) is still part of that suppressed context, so it must not exit on an
/// inner failure either.
pub fn fork_shell(sh: &Shell) -> Subshell {
    // Taken before the clone, so the guard's life spans the child's.
    let _mask = UmaskScope::capture();
    let shell = Shell {
        vars: sh.vars.clone(),
        funcs: sh.funcs.clone(),
        params: sh.params.clone(),
        arg0: sh.arg0.clone(),
        status: sh.status,
        last_bg: sh.last_bg,
        // CLEARED, not carried: ash does this in `forkchild` with the comment
        // "or else $RANDOM repeats in child" (ash.c:5344), so a subshell reseeds
        // from its own pid rather than replaying the parent's sequence. The
        // DYNAMIC flag rides along in `vars`, which is cloned above.
        random: None,
        opts: sh.opts,
        cwd: sh.cwd.clone(),
        logical_cwd: sh.logical_cwd.clone(),
        fds: sh.fds.clone(),
        localvar_depth: sh.localvar_depth,
        // Carried, not dropped: a fork copies the frame, so a subshell is still
        // inside it and a `local` there for a name the function already declared
        // is a REPEAT. Restoring these only ever touches the clone's own map.
        locals: sh.locals.clone(),
        pending_unwind: Vec::new(),
        pending_floor: 0,
        // A subshell's children inherit them too.
        opaque_env: sh.opaque_env.clone(),
        loop_depth: 0,
        run_depth: sh.run_depth,
        cmdsubst_count: sh.cmdsubst_count,
        errexit_suppressed: sh.errexit_suppressed,
        interactive: false,
        // Inherited: a `$(...)` inside $PS4 runs in one of these, and it must
        // still know it is inside PS4 or the guard buys nothing.
        in_ps4: sh.in_ps4,
        getopts_optind: sh.getopts_optind,
        getopts_off: sh.getopts_off,
        // A subshell inherits the aliases but cannot publish one back (POSIX).
        aliases: sh.aliases.clone(),
        cloned: true,
        trap_status: None,
        // POSIX 2.12: a subshell resets the traps it inherited to their defaults,
        // so only one it sets ITSELF runs when its environment ends -- but one set
        // to IGNORE (dash's empty action) stays ignored, and keeps being reported.
        traps: sh
            .traps
            .iter()
            .filter(|(_, action)| action.is_empty())
            .map(|(signo, action)| (*signo, action.clone()))
            .collect(),
    };
    Subshell { shell, _mask }
}

/// `$(code)`: run `code` in a subshell with stdout captured to a buffer, and
/// return the captured bytes as text.
pub fn capture_stdout(sh: &mut Shell, code: &str) -> R<String> {
    let list = match crate::parser::parse_aliased(code, &sh.aliases) {
        Ok(l) => l,
        Err(e) => return Err(sh.fatal(&e, 2)),
    };
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut child = fork_shell(sh);
    child.fds.set(1, Fd::WriteBuf(buf.clone()));
    let outcome = exec::run_list(&mut child, &list);
    let status = match outcome {
        Ok(()) => child.status,
        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
        Err(_) => child.status,
    };
    let status = exec::run_exit_trap(&mut child, status);
    // Command substitution updates $? of the enclosing shell.
    sh.set_status(status);
    let bytes = buf
        .lock()
        .map(|v| v.clone())
        .map_err(|_| sh.fatal("command substitution: poisoned capture buffer", 1))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `exec command …`: replace this shell process with `command`.
///
/// Returns only if the command cannot be run at all — a real `execve` never comes
/// back. Falls back to run-then-exit when a descriptor is one of the shell's
/// in-process buffers (a pipeline stage or command substitution): those bytes have
/// no kernel descriptor to hand over, so the command is run normally and the shell
/// exits with its status, which is what the caller would have observed anyway.
pub fn exec_replace(sh: &mut Shell, argv: &[String]) -> R<()> {
    use std::os::unix::process::CommandExt;

    let Some(program) = argv.first() else {
        return Ok(());
    };
    let Some(resolved) = resolve_program(sh, program, None) else {
        let _ = exec::write_stderr(sh, &format!("td-sh: exec: {program}: not found"));
        return failed_exec(sh, 127);
    };
    // Replacing the process is only safe from the real shell with real stdio. An
    // in-process clone (subshell, `&`, command substitution) would take the whole
    // script with it, and an in-process buffer has no kernel fd to hand over; both
    // run the command and exit instead, which is what the caller would have seen.
    let buffered = (0..=2).any(|fd| {
        matches!(sh.fds.get(fd), Some(Fd::ReadBuf(_)) | Some(Fd::WriteBuf(_)))
    });
    if sh.cloned || buffered {
        // A real `execve` replaces the image, taking the trap table with it, so the
        // emulation has to drop it too -- otherwise this shell runs an EXIT trap
        // the exec'd program could never have run.
        sh.traps.clear();
        exec_external(sh, argv, None)?;
        return Err(Sig::Exit(sh.status));
    }

    let mut cmd = Command::new(&resolved);
    cmd.args(argv.iter().skip(1));
    cmd.env_clear();
    for (k, v) in sh.exported_env() {
        cmd.env(k, v);
    }
    // Names the shell cannot spell still belong to the environment it was handed.
    for (k, v) in &sh.opaque_env {
        cmd.env(k, v);
    }
    cmd.current_dir(&sh.cwd);
    cmd.stdin(stdio_for(sh, 0)?);
    cmd.stdout(stdio_for(sh, 1)?);
    cmd.stderr(stdio_for(sh, 2)?);

    // Safe: `CommandExt::exec` returns the error rather than trapping it.
    let e = cmd.exec();
    let _ = exec::write_stderr(sh, &format!("td-sh: exec: {program}: {e}"));
    failed_exec(sh, 126)
}

/// A failed `exec` ends the shell, interactive or not: dash and busybox-ash both
/// clear `iflag` before handing over, and by the time `CommandExt::exec` reports
/// failure it has already applied the redirections to the REAL descriptors, so
/// carrying on would leave the shell rewired.
fn failed_exec(sh: &mut Shell, code: i32) -> R<()> {
    sh.set_status(code);
    Err(Sig::Exit(code))
}

/// Spawn an external program, wiring its stdio to the current descriptor table.
/// The seed corpus never reaches here (it is builtin-only), but real scripts do.
///
/// A buffered shell descriptor (a `ReadBuf` feeding stdin, or a `WriteBuf`
/// capturing stdout/stderr — as set up by command substitution and pipelines)
/// cannot be handed to a foreign process directly, so it is bridged through a real
/// OS pipe: the in-process bytes are pumped to/from the child on helper threads.
/// Without this an external consumer would read the shell's real inherited stdin
/// (blocking the shell forever on a live terminal) and `x=$(external)` would lose
/// the command's output.
pub fn exec_external(sh: &mut Shell, argv: &[String], path: Option<&str>) -> R<()> {
    let Some(program) = argv.first() else {
        sh.set_status(0);
        return Ok(());
    };
    let resolved = match resolve_program(sh, program, path) {
        Some(p) => p,
        None => {
            let _ = exec::write_stderr(sh, &format!("td-sh: {program}: not found"));
            sh.set_status(127);
            return Ok(());
        }
    };

    let mut cmd = Command::new(&resolved);
    cmd.args(argv.iter().skip(1));
    cmd.env_clear();
    for (k, v) in sh.exported_env() {
        cmd.env(k, v);
    }
    // Names the shell cannot spell still belong to the environment it was handed.
    for (k, v) in &sh.opaque_env {
        cmd.env(k, v);
    }
    cmd.current_dir(&sh.cwd);

    // A `ReadBuf` on stdin becomes piped input; a `WriteBuf` on stdout/stderr
    // becomes a captured pipe. Everything else maps to a `Stdio` directly.
    let stdin_bytes: Option<Vec<u8>> = match sh.fds.get(0) {
        Some(Fd::ReadBuf(b)) => b.lock().ok().map(|mut cur| {
            let mut v = Vec::new();
            let _ = cur.read_to_end(&mut v);
            v
        }),
        _ => None,
    };
    let stdout_buf = match sh.fds.get(1) {
        Some(Fd::WriteBuf(b)) => Some(b.clone()),
        _ => None,
    };
    let stderr_buf = match sh.fds.get(2) {
        Some(Fd::WriteBuf(b)) => Some(b.clone()),
        _ => None,
    };

    cmd.stdin(if stdin_bytes.is_some() {
        Stdio::piped()
    } else {
        stdio_for(sh, 0)?
    });
    cmd.stdout(if stdout_buf.is_some() {
        Stdio::piped()
    } else {
        stdio_for(sh, 1)?
    });
    cmd.stderr(if stderr_buf.is_some() {
        Stdio::piped()
    } else {
        stdio_for(sh, 2)?
    });

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = exec::write_stderr(sh, &format!("td-sh: {program}: {e}"));
            sh.set_status(126);
            return Ok(());
        }
    };

    // Feed stdin and drain stdout concurrently so a child that writes while reading
    // cannot deadlock a single-threaded pump. `Builder::spawn` is used (not
    // `thread::spawn`) so an OS thread-creation failure drops the pump closure —
    // closing that pipe end (child sees EOF on stdin / EPIPE on stdout) rather than
    // panicking; there is no inline fallback, only this safe degradation. The stdin
    // writer is DETACHED (never joined): if the consumer exits early it gets EPIPE
    // and ends, and if the child hands its stdin
    // to a lingering grandchild the writer blocks harmlessly in the background
    // instead of hanging the shell. stdout is captured on a joined thread; stderr is
    // drained on this thread.
    if let (Some(bytes), Some(mut si)) = (stdin_bytes, child.stdin.take()) {
        // Detached writer. On OS thread-exhaustion the closure is dropped, closing
        // `si` so the child sees empty stdin — degraded, but never a panic or hang.
        let _ = std::thread::Builder::new().spawn(move || {
            let _ = si.write_all(&bytes);
        });
    }
    let stdout_join = match (stdout_buf, child.stdout.take()) {
        (Some(buf), Some(mut so)) => {
            let drain = move || {
                let mut v = Vec::new();
                let _ = so.read_to_end(&mut v);
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(&v);
                }
            };
            // On OS thread-exhaustion the closure (owning `so`/`buf`) is dropped,
            // closing the pipe's read end; the child then sees EPIPE on stdout rather
            // than the shell hanging — degraded capture, never a panic or deadlock.
            std::thread::Builder::new().spawn(drain).ok()
        }
        _ => None,
    };
    // Drain stderr on this thread (the third concurrent stream).
    if let (Some(buf), Some(mut se)) = (stderr_buf, child.stderr.take()) {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v);
        if let Ok(mut b) = buf.lock() {
            b.extend_from_slice(&v);
        }
    }

    let status = child.wait();
    if let Some(j) = stdout_join {
        let _ = j.join();
    }

    match status {
        Ok(status) => {
            // A signal-terminated child reports 128 + signal number (POSIX).
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            sh.set_status(code);
            Ok(())
        }
        Err(e) => {
            let _ = exec::write_stderr(sh, &format!("td-sh: {program}: {e}"));
            sh.set_status(126);
            Ok(())
        }
    }
}

/// Translate a NON-buffered descriptor into a `Stdio` for a child. Buffered
/// descriptors (`ReadBuf`/`WriteBuf`) are handled by the pipe bridge in
/// `exec_external` before this is reached; the arms here remain a safe fallback
/// (inherit) for any fd the bridge does not special-case.
fn stdio_for(sh: &Shell, fd: u32) -> R<Stdio> {
    match sh.fds.get(fd) {
        // Map to the REAL stream the entry names, not to this position: after
        // `1>&2` fd 1 holds `Inherit(2)`, so the child's stdout must go to the
        // shell's stderr. `try_clone_to_owned` is the safe dup; if it fails the
        // positional inherit is the harmless fallback.
        Some(Fd::Inherit(n)) => Ok(inherit_stream(*n)),
        None => Ok(Stdio::inherit()),
        Some(Fd::Null) => Ok(Stdio::null()),
        Some(Fd::File(f)) => match f.lock() {
            Ok(file) => match file.try_clone() {
                Ok(c) => Ok(Stdio::from(c)),
                Err(_) => Ok(Stdio::inherit()),
            },
            Err(_) => Ok(Stdio::inherit()),
        },
        Some(Fd::Closed) => Ok(Stdio::null()),
        Some(Fd::ReadBuf(_)) | Some(Fd::WriteBuf(_)) => Ok(Stdio::inherit()),
    }
}

/// A `Stdio` for the process's own stream `n` (0/1/2).
fn inherit_stream(n: u8) -> Stdio {
    use std::os::fd::AsFd;
    let cloned = match n {
        0 => std::io::stdin().as_fd().try_clone_to_owned(),
        1 => std::io::stdout().as_fd().try_clone_to_owned(),
        _ => std::io::stderr().as_fd().try_clone_to_owned(),
    };
    match cloned {
        Ok(owned) => Stdio::from(owned),
        Err(_) => Stdio::inherit(),
    }
}

/// `command -p`'s default utility path: ash's `bb_default_path`, which is
/// `BB_PATH_ROOT_PATH` (libbb.h) less its `/sbin` pair. The supplied busybox
/// leaves `BB_ADDITIONAL_PATH` -- the CFLAGS hook that can extend it -- empty, and
/// its strings confirm the result. A td image has `/bin` and no `/usr/bin`.
pub const DEFAULT_UTILITY_PATH: &str = "/bin:/usr/bin";

/// Locate an external program: a path containing `/` is used directly, otherwise
/// each element of `path` -- or of `PATH` when it is `None` -- is tried. Relative
/// elements resolve against the shell cwd (not the process cwd) so the lookup
/// agrees with the child, which runs with `current_dir(sh.cwd)`.
///
/// `path` is `command -p`'s override: only the LOOKUP moves, never the variable a
/// child inherits, as ash's `path` local does.
pub fn resolve_program(
    sh: &Shell,
    program: &str,
    path: Option<&str>,
) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        let p = sh.resolve(program);
        return if p.is_file() { Some(p) } else { None };
    }
    let owned;
    let path = match path {
        Some(p) => p,
        None => {
            owned = sh.get_var("PATH").unwrap_or_default();
            &owned
        }
    };
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = sh.resolve(dir).join(program);
        // Skip a non-executable match and keep searching, so a data file earlier in
        // PATH does not shadow a real executable later in it.
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Parse and run a program with stdout and stderr captured to buffers — the
/// harness used by unit and conformance tests.
#[cfg(test)]
pub fn run_capturing(src: &str) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let mut sh = Shell::new_for_test();
    sh.fds.set(1, Fd::WriteBuf(out.clone()));
    sh.fds.set(2, Fd::WriteBuf(err.clone()));
    let status = exec::run_program(&mut sh, src);
    let out_s = out
        .lock()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    let err_s = err
        .lock()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    (status, out_s, err_s)
}

/// `run_capturing`'s stdout as raw BYTES. `echo`/`printf` escapes can name a
/// byte that is not UTF-8, which the lossy `String` above folds to U+FFFD --
/// so an assertion about, say, `\377` cannot tell 0xff from any other bad byte.
#[cfg(test)]
pub fn run_capturing_bytes(src: &str) -> (i32, Vec<u8>) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut sh = Shell::new_for_test();
    sh.fds.set(1, Fd::WriteBuf(out.clone()));
    let status = exec::run_program(&mut sh, src);
    let bytes = out.lock().map(|v| v.clone()).unwrap_or_default();
    (status, bytes)
}

/// Drive several units through the INTERACTIVE handler, as the prompt loop does,
/// returning `$?` plus captured stdout/stderr. Distinct from `run_capturing`:
/// only this path can show that a shell survives an aborted command.
#[cfg(test)]
pub fn run_capturing_interactive_units(units: &[&str]) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let mut sh = Shell::new_for_test();
    sh.interactive = true;
    sh.fds.set(1, Fd::WriteBuf(out.clone()));
    sh.fds.set(2, Fd::WriteBuf(err.clone()));
    for unit in units {
        match crate::parser::parse_aliased(unit, &sh.aliases) {
            Ok(list) => {
                if let Some(code) = exec::run_interactive_unit(&mut sh, &list) {
                    sh.set_status(code);
                    break;
                }
            }
            Err(_) => sh.set_status(2),
        }
    }
    let text = |b: &Arc<Mutex<Vec<u8>>>| {
        b.lock().map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_default()
    };
    (sh.status, text(&out), text(&err))
}

