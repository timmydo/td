//! One service lifecycle, two daemons, and a private descriptor on each stdin;
//! and the start gate they share with a `stop=leaf` leader's launch.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::Duration;

pub const VERB: &str = "pair-run";
/// The trampoline a `stop=leaf` leader runs through (DESIGN.md I7).
pub const LEAF_VERB: &str = "leaf-exec";
const MAX_ARGS: usize = 256;
const MAX_BYTES: usize = 32 * 1024;
const START: u8 = 1;

pub fn validate_command(argv: &[String]) -> Result<(), String> {
    if !argv.first().is_some_and(|program| program.starts_with('/')) {
        return Err("paired command needs an absolute executable path".into());
    }
    if !within_bounds(argv) {
        return Err("paired command exceeds argument bounds or contains NUL".into());
    }
    Ok(())
}

/// The bounds every command td-svc hands to itself through `/proc/self/exe`
/// keeps, a pair's two and a `stop=leaf` leader's.
fn within_bounds(argv: &[String]) -> bool {
    argv.len() <= MAX_ARGS
        && !argv.iter().any(|arg| arg.contains('\0'))
        && argv
            .iter()
            .try_fold(0usize, |size, arg| size.checked_add(arg.len()))
            .is_some_and(|size| size <= MAX_BYTES)
}

pub fn parse(args: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let count_text = args.first().ok_or("pair-run needs an argument count")?;
    let count = count_text
        .parse::<usize>()
        .map_err(|_| "invalid pair argument count")?;
    if !(1..=MAX_ARGS).contains(&count) || count.to_string() != *count_text {
        return Err("invalid pair argument count".into());
    }
    let left = args
        .get(1..=count)
        .ok_or("truncated first paired command")?;
    let right = args
        .get(count + 1..)
        .filter(|args| !args.is_empty())
        .ok_or("missing second paired command")?;
    validate_command(left)?;
    validate_command(right)?;
    Ok((left.to_vec(), right.to_vec()))
}

pub fn command(left: &[String], right: &[String]) -> Result<Command, String> {
    validate_command(left)?;
    validate_command(right)?;
    // The running image stays executable even if its former store path is gone.
    let mut command = Command::new("/proc/self/exe");
    command
        .arg(VERB)
        .arg(left.len().to_string())
        .args(left)
        .args(right);
    command.stdin(Stdio::piped());
    Ok(command)
}

/// A `stop=leaf` leader: the unit's literal argv behind the start gate, so
/// nothing of the instance runs, or forks, before td-svc has placed it.
pub fn leaf_command(argv: &[String]) -> Result<Command, String> {
    if argv.is_empty() {
        return Err("empty exec".into());
    }
    if !within_bounds(argv) {
        return Err("exec exceeds argument bounds or contains NUL".into());
    }
    let mut command = Command::new("/proc/self/exe");
    command.arg(LEAF_VERB).arg("--").args(argv);
    command.stdin(Stdio::piped());
    Ok(command)
}

/// `leaf-exec -- PROGRAM [ARG...]`, exactly, within a pair's bounds.
pub fn parse_leaf(args: &[String]) -> Result<Vec<String>, String> {
    match args.split_first() {
        Some((separator, argv)) if separator == "--" && !argv.is_empty() => {
            if !within_bounds(argv) {
                return Err(format!(
                    "{LEAF_VERB} command exceeds argument bounds or contains NUL"
                ));
            }
            Ok(argv.to_vec())
        }
        _ => Err(format!("{LEAF_VERB} needs -- PROGRAM [ARG...]")),
    }
}

/// The trampoline: wait for the grant, then become the unit's program, with
/// the null stdin an ungated unit gets and the stdout and stderr td-svc gave
/// this process. Returns only on failure, having exec'd nothing.
pub fn leaf_exec(argv: &[String], grant: &mut impl Read) -> String {
    use std::os::unix::process::CommandExt;
    if let Err(why) = await_start(grant) {
        return why;
    }
    let Some(program) = argv.first() else {
        return "empty exec".into();
    };
    let error = Command::new(program)
        .args(argv.get(1..).unwrap_or(&[]))
        .stdin(Stdio::null())
        .exec();
    format!("{program}: {error}")
}

/// EOF without the exact grant prevents the gated commands from starting.
pub fn await_start(input: &mut impl Read) -> Result<(), String> {
    let mut grant = Vec::new();
    input
        .take(2)
        .read_to_end(&mut grant)
        .map_err(|e| format!("start gate: {e}"))?;
    if grant != [START] {
        return Err("start gate refused or supervisor disconnected".into());
    }
    Ok(())
}

/// Called only after placement, recording, and installing the supervisor waiter.
pub fn release_start(pipe: Option<ChildStdin>, placed: bool, recorded: bool) -> Result<(), String> {
    if !placed {
        return Err("start refused: its leader was not placed in its cgroup".into());
    }
    if !recorded {
        return Err("start refused: its leader's pid/starttime was not recorded".into());
    }
    let mut pipe = pipe.ok_or("start gate is missing")?;
    pipe.write_all(&[START])
        .map_err(|e| format!("start gate: {e}"))
}

fn spawn(argv: &[String], endpoint: UnixStream) -> Result<Child, String> {
    validate_command(argv)?;
    let program = argv.first().ok_or("missing paired executable")?;
    let mut command = Command::new(program);
    command.args(argv.get(1..).unwrap_or(&[]));
    command.stdin(Stdio::from(OwnedFd::from(endpoint)));
    // No new process group: both peers stay in the unit's containment.
    command
        .spawn()
        .map_err(|e| format!("paired executable {program}: {e}"))
}

struct Peers {
    left: Child,
    right: Option<Child>,
}

impl Drop for Peers {
    fn drop(&mut self) {
        // Child remembers a reaped status; kill never targets a reused PID.
        let _ = self.left.kill();
        if let Some(right) = &mut self.right {
            let _ = right.kill();
        }
        let _ = self.left.wait();
        if let Some(right) = &mut self.right {
            let _ = right.wait();
        }
    }
}

#[derive(Debug)]
pub struct Ended {
    pub peer: &'static str,
    pub status: ExitStatus,
}

pub fn run(left: &[String], right: &[String]) -> Result<Ended, String> {
    validate_command(left)?;
    validate_command(right)?;
    let (a, b) = UnixStream::pair().map_err(|e| format!("private pair socket: {e}"))?;
    let mut peers = Peers {
        left: spawn(left, a)?,
        right: None,
    };
    peers.right = Some(spawn(right, b)?);
    let Peers { left, right } = &mut peers;
    let right = right.as_mut().ok_or("missing second peer")?;
    loop {
        if let Some(status) = left
            .try_wait()
            .map_err(|e| format!("first peer wait: {e}"))?
        {
            return Ok(Ended {
                peer: "first",
                status,
            });
        }
        if let Some(status) = right
            .try_wait()
            .map_err(|e| format!("second peer wait: {e}"))?
        {
            return Ok(Ended {
                peer: "second",
                status,
            });
        }
        // Keeping Child ownership makes termination immune to PID reuse.
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// A leaf leader's argv keeps the bounds a pair's does, at both ends.
    #[test]
    fn a_leaf_command_keeps_a_pairs_bounds() {
        let leaf = |argv: Vec<String>| {
            let mut args = vec!["--".to_string()];
            args.extend(argv);
            parse_leaf(&args)
        };
        let most = vec!["a".to_string(); MAX_ARGS];
        assert_eq!(leaf(most.clone()), Ok(most.clone()));
        assert!(leaf_command(&most).is_ok());
        let too_many = vec!["a".to_string(); MAX_ARGS + 1];
        assert!(leaf(too_many.clone()).is_err());
        assert!(leaf_command(&too_many).is_err());
        let largest = vec!["a".repeat(MAX_BYTES)];
        assert!(leaf(largest.clone()).is_ok());
        let too_large = vec!["a".repeat(MAX_BYTES + 1)];
        assert!(leaf(too_large.clone()).is_err());
        assert!(leaf_command(&too_large).is_err());
        let nul = vec!["a\0b".to_string()];
        assert!(leaf(nul.clone()).is_err());
        assert!(leaf_command(&nul).is_err());
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn actual_producer_routes_and_preserves_both_commands() {
        let left = args(&["/first", "", "two words", "--"]);
        let right = args(&["/second", "1", ""]);
        let command = command(&left, &right).unwrap();
        let wire: Vec<String> = command
            .get_args()
            .map(|s| s.to_str().unwrap().to_string())
            .collect();
        assert!(
            matches!(crate::route(&wire), crate::Route::PairRun { left: a, right: b } if a == left && b == right)
        );
    }

    #[test]
    fn start_gate_child() {
        if std::env::var_os("TD_PAIR_GATE_FIXTURE").is_none() {
            return;
        }
        if await_start(&mut std::io::stdin().lock()).is_ok() {
            println!("GATE-GRANTED");
        } else {
            println!("GATE-REFUSED");
        }
    }

    #[test]
    fn supervisor_release_requires_placement_and_recording() {
        for (placed, recorded) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "pair::tests::start_gate_child", "--nocapture"])
                .env("TD_PAIR_GATE_FIXTURE", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let grant = release_start(child.stdin.take(), placed, recorded);
            assert_eq!(grant.is_ok(), placed && recorded);
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            let output = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                output.contains("GATE-GRANTED"),
                placed && recorded,
                "{output}"
            );
            assert_eq!(
                output.contains("GATE-REFUSED"),
                !(placed && recorded),
                "{output}"
            );
        }
    }

    #[test]
    fn wire_arguments_preserve_empty_values_and_separator_lookalikes() {
        let input = args(&["4", "/first", "", "--", "1", "/second", "two words"]);
        let (left, right) = parse(&input).unwrap();
        assert_eq!(left, args(&["/first", "", "--", "1"]));
        assert_eq!(right, args(&["/second", "two words"]));
    }

    #[test]
    fn malformed_pair_arguments_fail_before_spawn() {
        for input in [
            vec![],
            vec!["0", "/a", "/b"],
            vec!["01", "/a", "/b"],
            vec!["257", "/a", "/b"],
            vec!["1", "/a"],
            vec!["2", "/a"],
            vec!["1", "relative", "/b"],
            vec!["1", "/a", "relative"],
            vec!["1", "/a", "/b\0c"],
        ] {
            assert!(parse(&args(&input)).is_err(), "{input:?}");
        }
        assert!(validate_command(&vec!["/a".into(); MAX_ARGS + 1]).is_err());
        assert!(validate_command(&[format!("/{}", "a".repeat(MAX_BYTES))]).is_err());
    }

    #[test]
    fn startup_grant_requires_exact_bytes_and_eof() {
        assert!(await_start(&mut &[START][..]).is_ok());
        for bytes in [vec![], vec![0], vec![START, START], vec![START, 0, START]] {
            assert!(await_start(&mut bytes.as_slice()).is_err());
        }
    }
}
