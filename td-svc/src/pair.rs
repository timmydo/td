//! One service lifecycle, two daemons, and a private descriptor on each stdin.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::Duration;

pub const VERB: &str = "pair-run";
const MAX_ARGS: usize = 256;
const MAX_BYTES: usize = 32 * 1024;
const START: u8 = 1;

pub fn validate_command(argv: &[String]) -> Result<(), String> {
    if !argv.first().is_some_and(|program| program.starts_with('/')) {
        return Err("paired command needs an absolute executable path".into());
    }
    if argv.len() > MAX_ARGS
        || argv.iter().any(|arg| arg.contains('\0'))
        || argv
            .iter()
            .try_fold(0usize, |size, arg| size.checked_add(arg.len()))
            .is_none_or(|size| size > MAX_BYTES)
    {
        return Err("paired command exceeds argument bounds or contains NUL".into());
    }
    Ok(())
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

/// EOF without the exact grant prevents both commands from starting.
pub fn await_start(input: &mut impl Read) -> Result<(), String> {
    let mut grant = Vec::new();
    input
        .take(2)
        .read_to_end(&mut grant)
        .map_err(|e| format!("pair start gate: {e}"))?;
    if grant != [START] {
        return Err("pair start gate refused or supervisor disconnected".into());
    }
    Ok(())
}

/// Called only after placement, recording, and installing the supervisor waiter.
pub fn release_start(pipe: Option<ChildStdin>, placed: bool, recorded: bool) -> Result<(), String> {
    if !placed {
        return Err("paired daemons refused: coordinator was not placed in its cgroup".into());
    }
    if !recorded {
        return Err("paired daemons refused: coordinator pid/starttime was not recorded".into());
    }
    let mut pipe = pipe.ok_or("pair start gate is missing")?;
    pipe.write_all(&[START])
        .map_err(|e| format!("pair start gate: {e}"))
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

/// Pins the paired unit's kernel containment across coordinator death and PID reuse.
pub struct Cohort {
    kill: File,
    events: File,
    killed: bool,
}

impl Cohort {
    /// The caller supplies the root-owned service leaf established by cgroup::create_service.
    pub fn open(leaf: &Path) -> Result<Self, String> {
        let mut cohort = Self {
            kill: OpenOptions::new()
                .write(true)
                .open(leaf.join("cgroup.kill"))
                .map_err(|e| format!("paired cgroup kill control: {e}"))?,
            events: File::open(leaf.join("cgroup.events"))
                .map_err(|e| format!("paired cgroup events: {e}"))?,
            killed: false,
        };
        if !cohort.empty()? {
            return Err("paired cgroup already contains live processes".into());
        }
        Ok(cohort)
    }

    pub fn kill(&mut self) -> Result<(), String> {
        if !self.killed {
            self.kill
                .write_all(b"1")
                .map_err(|e| format!("paired cgroup kill: {e}"))?;
            self.killed = true;
        }
        Ok(())
    }

    pub fn empty(&mut self) -> Result<bool, String> {
        self.events
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("paired cgroup events: {e}"))?;
        let mut text = String::new();
        (&mut self.events)
            .take(4097)
            .read_to_string(&mut text)
            .map_err(|e| format!("paired cgroup events: {e}"))?;
        if text.len() > 4096 {
            return Err("paired cgroup events exceeded 4096 bytes".into());
        }
        let mut populated = None;
        for line in text.lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("populated") {
                continue;
            }
            let value = match words.next() {
                Some("0") => false,
                Some("1") => true,
                _ => return Err("paired cgroup has malformed populated value".into()),
            };
            if words.next().is_some() || populated.replace(value).is_some() {
                return Err("paired cgroup has ambiguous populated value".into());
            }
        }
        populated
            .map(|value| !value)
            .ok_or("paired cgroup has no populated value".into())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

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

    struct Controls(std::path::PathBuf);
    impl Controls {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("td-pair-controls-{}-{stamp}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("cgroup.kill"), "").unwrap();
            std::fs::write(path.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
            Self(path)
        }
        fn events(&self, text: &str) {
            std::fs::write(self.0.join("cgroup.events"), text).unwrap();
        }
    }
    impl Drop for Controls {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cohort_does_not_claim_a_kill_is_emptiness() {
        let controls = Controls::new();
        let mut cohort = Cohort::open(&controls.0).unwrap();
        controls.events("populated 1\nfrozen 0\n");
        cohort.kill().unwrap();
        cohort.kill().unwrap();
        assert_eq!(
            std::fs::read_to_string(controls.0.join("cgroup.kill")).unwrap(),
            "1"
        );
        assert!(!cohort.empty().unwrap());
        controls.events("populated 0\nfrozen 0\n");
        assert!(cohort.empty().unwrap());
    }

    #[test]
    fn cohort_refuses_unknown_state_and_occupied_launches() {
        let controls = Controls::new();
        for text in [
            "",
            "frozen 0\n",
            "populated 2\n",
            "populated 0\npopulated 0\n",
            "populated 0 extra\n",
            "populated 1\n",
        ] {
            controls.events(text);
            assert!(Cohort::open(&controls.0).is_err(), "{text:?}");
        }
        controls.events("populated 0\n");
        let mut cohort = Cohort::open(&controls.0).unwrap();
        controls.events("frozen 0\n");
        assert!(cohort.empty().is_err());
        controls.events(&"x".repeat(4097));
        assert!(cohort.empty().is_err());
    }

    #[test]
    fn cohort_keeps_the_original_kernel_objects_after_path_replacement() {
        let controls = Controls::new();
        let mut cohort = Cohort::open(&controls.0).unwrap();
        controls.events("populated 1\n");
        std::fs::rename(
            controls.0.join("cgroup.events"),
            controls.0.join("old-events"),
        )
        .unwrap();
        controls.events("populated 0\n");
        assert!(!cohort.empty().unwrap());
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
