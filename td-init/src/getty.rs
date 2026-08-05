//! `getty [-L] -n -l PROG BAUD TTY [TERM]` — open a terminal, make it a session
//! of its own, set the line up, and exec the login program on it.
//!
//! This is the last busybox applet the image ran, and taking it is what lets the
//! multicall leave the root entirely. It is deliberately NOT a general getty:
//! the prompting half — `/etc/issue`, reading a username and passing it to
//! `login`, `-t` timeouts, cycling speeds on BREAK — is not implemented, so `-n`
//! and `-l` are REQUIRED rather than optional. An image that auto-logs in never
//! prompts, and an applet that accepted the flags for prompting while doing
//! something else would be worse than one that refuses them.
//!
//! What it does implement is the part with consequences. `setsid(2)` plus
//! `TIOCSCTTY` are why this lives in td-init rather than the safe td-util
//! multicall, and the line settings are the third and fourth requests on this
//! crate's `ioctl` roster; `term.rs` holds the layout and the readback.
//!
//! Deliberately NOT here: `vhangup(2)`, which util-linux's agetty offers to
//! evict whatever still holds the line before a login starts on it. It would be
//! an ELEVENTH syscall on this surface to duplicate work td-svc already does —
//! a `tty=` unit's containment is the leader together with every process
//! holding that device, so the supervisor has torn the previous session down
//! before it respawns the greeter (td-svc/DESIGN.md, "Stopping").
//!
//! Unlike `cttyhack`, a terminal that cannot be claimed is FATAL here. cttyhack
//! degrades because a rescue shell without job control still beats no shell;
//! getty has the opposite duty — the caller asked for a login session on this
//! terminal, and a session with no controlling terminal is one where Ctrl-C
//! reaches nothing and `login`'s child cannot be signalled. Failing is also what
//! the shipped `/etc/tty-session` is written around: its `getty … && td-svc
//! reboot` short-circuits, so the supervisor restarts the greeter instead of
//! powering the machine off as though the operator had logged out.

use crate::sys;
use crate::term;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// `O_NOCTTY` — the terminal is claimed explicitly below, never by side effect.
/// Without it a session leader opening a free tty acquires it implicitly and the
/// `TIOCSCTTY` result would say nothing about whether we hold it.
const O_NOCTTY: i32 = 0o400;

/// `O_NONBLOCK` — for the FIRST open only; see `run`.
const O_NONBLOCK: i32 = 0o4000;

/// EPERM — `setsid(2)` returns it when we already lead a process group. As in
/// `cttyhack`, that is not the decision: `TIOCSCTTY` decides, and it reports.
const EPERM: i32 = 1;

fn usage() -> String {
    "usage: getty [-L] -n -l PROG BAUD TTY [TERM]".to_string()
}

/// What an argv asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub login: String,
    pub speed: u32,
    pub device: String,
    pub term: Option<String>,
    pub local: bool,
}

/// Parse the command line. `-n` and `-l PROG` are required — see the module
/// note. The two positional orders are both accepted because both are in use:
/// busybox spells it `BAUD TTY` and util-linux's agetty `TTY BAUD`, and a getty
/// that silently took the wrong one of them would open a device named `115200`.
pub fn parse(args: &[String]) -> Result<Request, String> {
    let (mut no_prompt, mut local, mut login) = (false, false, None);
    let mut positional: Vec<&String> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "-n" {
            no_prompt = true;
        } else if arg == "-L" {
            local = true;
        } else if arg == "-l" {
            let prog = rest.next().ok_or_else(|| "-l needs a program".to_string())?;
            login = Some(prog.clone());
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(format!("unrecognised argument '{arg}'\n{}", usage()));
        } else {
            positional.push(arg);
        }
    }
    let login = login.ok_or_else(|| {
        format!("-l PROG is required; this getty does not prompt\n{}", usage())
    })?;
    if !no_prompt {
        return Err(format!(
            "-n is required; this getty does not prompt for a login name\n{}",
            usage()
        ));
    }
    // Refused rather than ignored, for the reason the ambiguous-operand case is:
    // an argument this getty does not understand is one the operator believes is
    // doing something. THREE is the whole form — speed, terminal, TERM.
    if positional.len() > 3 {
        return Err(format!(
            "{} operands; this getty takes BAUD, TTY and an optional TERM\n{}",
            positional.len(),
            usage()
        ));
    }
    let (speed, device) = operands(&positional)?;
    if device == "-" {
        return Err(format!(
            "'-' means \"the terminal is already open on stdin\" to other gettys; \
             this one opens the device it is given\n{}",
            usage()
        ));
    }
    Ok(Request {
        login,
        speed,
        device,
        term: positional.get(2).map(|t| (*t).clone()),
        local,
    })
}

/// Split the positional operands into a speed and a device, whichever order
/// they came in. Exactly one of the two may name a speed; if both or neither
/// do, the command line is ambiguous and is refused rather than guessed at.
fn operands(positional: &[&String]) -> Result<(u32, String), String> {
    let (Some(first), Some(second)) = (positional.first(), positional.get(1)) else {
        return Err(format!("BAUD and TTY are required\n{}", usage()));
    };
    match (term::speed_named(first), term::speed_named(second)) {
        (Some(speed), None) => Ok((speed, (*second).clone())),
        (None, Some(speed)) => Ok((speed, (*first).clone())),
        (Some(_), Some(_)) => Err(format!(
            "'{first}' and '{second}' both name a line speed; one must be the terminal"
        )),
        (None, None) => Err(format!("'{first}' is not a line speed this getty knows")),
    }
}

/// `/dev/ttyS0` for `ttyS0`, and an absolute path used as given.
///
/// A bare `-` is refused by `parse` before reaching here: busybox getty reads it
/// as "the terminal is already open on stdin", which this getty does not
/// implement, and resolving it would open `/dev/-` and fail with a message about
/// a file rather than about the mode that was asked for.
fn device_path(name: &str) -> String {
    if name.starts_with('/') {
        name.to_string()
    } else {
        format!("/dev/{name}")
    }
}

/// Become a session leader, then claim the terminal. EPERM from `setsid(2)`
/// means we already lead a process group — either this session's or one inside
/// somebody else's — and says nothing on its own, so the claim below is what
/// reports. Resolving and opening happen BEFORE the `setsid`, which drops
/// whatever controlling terminal we had: fallible work after it would leave the
/// process worse off than it started.
fn claim(tty: &File, path: &str) -> Result<(), String> {
    if let Err(e) = sys::setsid() {
        if e.raw_os_error() != Some(EPERM) {
            return Err(format!("setsid: {e}"));
        }
    }
    sys::set_controlling_tty(tty.as_raw_fd()).map_err(|e| {
        format!(
            "cannot claim {path} as the controlling terminal: {e}; \
             refusing to start a session that could not be signalled"
        )
    })
}

/// Discard whatever is already queued on the line, best-effort, while the
/// descriptor is still non-blocking.
///
/// Bytes typed at a console during boot — or left by a session that died
/// mid-line — are delivered to whatever reads next, and what reads next here is
/// a shell that was auto-logged-in. They would arrive as COMMANDS. A complete
/// flush is `TCFLSH`, a FIFTH ioctl request and so its own reviewed amendment;
/// this is what the four already on the roster can do, and it is bounded rather
/// than complete: in canonical mode the line discipline hands over whole lines,
/// so a PARTIAL line still sitting in its buffer survives and completes when the
/// operator next presses Enter. Reading is capped so a device with something
/// permanently ready cannot spin here instead of starting the session.
fn drain(tty: &File) {
    let mut sink = [0u8; 256];
    for _ in 0..64 {
        match (&mut &*tty).read(&mut sink) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let request = parse(args)?;
    let path = device_path(&request.device);
    // O_NONBLOCK, and it is load-bearing rather than tidy: on a serial line with
    // CLOCAL clear and no carrier detected, a plain `open` BLOCKS — forever, in
    // the respawn loop, with nothing printed. The CLOCAL that `-L` asks for is
    // set by the termios below, which that open would never reach. So the line
    // is opened in the one mode that cannot block, configured, and then reopened
    // for the session.
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(&path)
        .map_err(|e| format!("{path}: {e}"))?;
    claim(&tty, &path)?;
    match term::configure(tty.as_raw_fd(), request.speed, request.local)? {
        term::Speed::Took => {}
        // Reported rather than refused: see `term::Speed`. One line, because a
        // respawning greeter would otherwise print it on every restart.
        term::Speed::Ignored => {
            crate::emit_err(&format!("getty: {path} has no line speed to set\n"));
        }
    }
    drain(&tty);

    // Reopened BLOCKING for the session itself: `login` and the shell must never
    // see EAGAIN from a terminal read, which is what a non-blocking descriptor
    // handed to them would give. This open cannot hang the way the first one
    // could — `-L` has set CLOCAL by now, and where it was NOT given, waiting
    // for carrier is precisely what the caller asked for. It is the same
    // terminal, so the claim above still stands: a controlling terminal belongs
    // to the session, not to a descriptor.
    let session = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(&path)
        .map_err(|e| format!("{path}: reopening for the session: {e}"))?;
    // Dropped only once the session's own descriptor exists: a serial line whose
    // last descriptor closes hangs up, and dropping DTR between the two opens
    // would be a modem disconnect in the middle of starting a login.
    drop(tty);
    let (Ok(out), Ok(err)) = (session.try_clone(), session.try_clone()) else {
        return Err(format!("{path}: could not duplicate the terminal handle"));
    };
    let mut cmd = Command::new(&request.login);
    cmd.stdin(Stdio::from(session))
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    if let Some(term) = &request.term {
        cmd.env("TERM", term);
    }
    // `exec` replaces this process image and does not return on success, so the
    // session's exit status becomes this applet's — which is what makes
    // `/etc/tty-session`'s `&&` mean "the operator logged out".
    Err(format!("exec {}: {}", request.login, cmd.exec()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    /// The shipped `/etc/tty-session` line, verbatim. If this stops parsing, the
    /// image's greeter stops starting.
    #[test]
    fn the_shipped_invocation_parses() {
        let r = parse(&args(&["-L", "-n", "-l", "/etc/autologin", "115200", "ttyS0", "vt100"]))
            .unwrap();
        assert_eq!(r.login, "/etc/autologin");
        assert_eq!(r.speed, 0x1002);
        assert_eq!(r.device, "ttyS0");
        assert_eq!(r.term.as_deref(), Some("vt100"));
        assert!(r.local);
    }

    /// Both operand orders, because both conventions are in the wild. The
    /// speed is what identifies itself; the other operand is the terminal.
    #[test]
    fn either_operand_order_is_understood() {
        let bb = parse(&args(&["-n", "-l", "/bin/login", "9600", "ttyS0"])).unwrap();
        let agetty = parse(&args(&["-n", "-l", "/bin/login", "ttyS0", "9600"])).unwrap();
        assert_eq!(bb.device, "ttyS0");
        assert_eq!(agetty.device, "ttyS0");
        assert_eq!(bb.speed, agetty.speed);
    }

    /// An ambiguous or speedless pair is refused rather than guessed at: taking
    /// the first operand would open a device named `115200`.
    #[test]
    fn ambiguous_operands_are_refused() {
        let both = parse(&args(&["-n", "-l", "/bin/login", "9600", "115200"])).unwrap_err();
        assert!(both.contains("both name a line speed"), "{both}");
        let neither = parse(&args(&["-n", "-l", "/bin/login", "ttyS0", "vt100"])).unwrap_err();
        assert!(neither.contains("is not a line speed"), "{neither}");
        let short = parse(&args(&["-n", "-l", "/bin/login", "9600"])).unwrap_err();
        assert!(short.contains("BAUD and TTY are required"), "{short}");
    }

    /// The prompting half is not implemented, so the flags that turn it off are
    /// required. Accepting the command line without them would start a session
    /// as some user nobody named.
    #[test]
    fn prompting_is_refused_rather_than_faked() {
        let no_l = parse(&args(&["-n", "115200", "ttyS0"])).unwrap_err();
        assert!(no_l.contains("-l PROG is required"), "{no_l}");
        let no_n = parse(&args(&["-l", "/bin/login", "115200", "ttyS0"])).unwrap_err();
        assert!(no_n.contains("-n is required"), "{no_n}");
        let dangling = parse(&args(&["-n", "-l"])).unwrap_err();
        assert!(dangling.contains("-l needs a program"), "{dangling}");
    }

    /// An option this getty does not implement is refused, not ignored. busybox
    /// getty takes a dozen more; silently accepting one would make the line
    /// settings or the login program differ from what the operator asked for.
    #[test]
    fn an_unimplemented_option_is_refused() {
        let e = parse(&args(&["-n", "-l", "/bin/login", "-t", "60", "115200", "ttyS0"]))
            .unwrap_err();
        assert!(e.contains("unrecognised argument '-t'"), "{e}");
        assert!(e.contains("usage: getty"), "{e}");
    }

    /// A bare `-` reaches the operand parser rather than the option guard — the
    /// guard is on length — and is then refused as a MODE this getty does not
    /// implement. Opening `/dev/-` would fail too, but with a message about a
    /// missing file rather than about what was asked for.
    #[test]
    fn a_bare_dash_is_refused_as_a_mode_this_getty_lacks() {
        let e = parse(&args(&["-n", "-l", "/bin/login", "9600", "-"])).unwrap_err();
        assert!(e.contains("already open on stdin"), "{e}");
        assert!(!e.contains("unrecognised argument"), "{e}");
    }

    /// A fourth operand is refused. Nothing on this image passes one, which is
    /// exactly why silently dropping it would go unnoticed.
    #[test]
    fn a_fourth_operand_is_refused() {
        let e = parse(&args(&["-n", "-l", "/bin/login", "115200", "ttyS0", "vt100", "extra"]))
            .unwrap_err();
        assert!(e.contains("4 operands"), "{e}");
        assert!(e.contains("usage: getty"), "{e}");
    }

    #[test]
    fn a_bare_name_is_resolved_under_dev() {
        assert_eq!(device_path("ttyS0"), "/dev/ttyS0");
        assert_eq!(device_path("/dev/tty1"), "/dev/tty1");
    }

    /// No operands at all is a usage error, and it names the form rather than
    /// the first missing piece.
    #[test]
    fn no_arguments_is_a_usage_error() {
        let e = parse(&[]).unwrap_err();
        assert!(e.contains("usage: getty"), "{e}");
    }
}
