//! Host commands run with a shared cancellation/deadline and bounded captured output.
use crate::R;
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::OwnedFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use td_crypto::{Crypto, Digest, Provider};
use td_json::Json;

pub fn require(condition: bool, message: &str) -> R<()> {
    if condition {
        Ok(())
    } else {
        Err(owned(message)?.into())
    }
}
pub fn owned(s: &str) -> R<String> {
    Ok(td_alloc::string(s)?)
}
pub fn fmt(args: std::fmt::Arguments<'_>) -> R<String> {
    Ok(td_alloc::format(args)?)
}
pub fn child(parent: &Path, name: &str) -> R<PathBuf> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    if Path::new(name).is_absolute() {
        return Ok(PathBuf::from(owned(name)?));
    }
    let mut bytes = td_alloc::copy(parent.as_os_str().as_bytes())?;
    if !bytes.is_empty() && !bytes.ends_with(b"/") {
        td_alloc::push(&mut bytes, b'/')?;
    }
    td_alloc::extend_from_slice(&mut bytes, name.as_bytes())?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}
pub fn set(obj: &mut Json, key: &str, value: Json) -> R<()> {
    let Json::Obj(fields) = obj else {
        return Err("expected JSON object".into());
    };
    if let Some((_, slot)) = fields.iter_mut().find(|(k, _)| k == key) {
        *slot = value;
    } else {
        td_alloc::push(fields, (owned(key)?, value))?;
    }
    Ok(())
}
pub fn random_hex(n: usize) -> R<String> {
    let mut bytes = td_alloc::filled(n, 0u8)?;
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    hex(&bytes)
}
fn hex(bytes: &[u8]) -> R<String> {
    use std::fmt::Write as _;
    let mut output = String::new();
    td_alloc::reserve_string(
        &mut output,
        bytes.len().checked_mul(2).ok_or("hex capacity")?,
    )?;
    for b in bytes {
        write!(&mut output, "{b:02x}")?;
    }
    Ok(output)
}
pub fn digest(path: &Path) -> R<String> {
    let mut hash = Provider.sha256()?;
    let mut input = File::open(path)?;
    let mut buffer = [0; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(buffer.get(..n).ok_or("digest length")?)?;
    }
    hex(&hash.finish()?)
}
pub fn private_dir(parent: &Path, prefix: &str) -> R<PathBuf> {
    let path = child(parent, &fmt(format_args!("{prefix}{}", random_hex(12)?))?)?;
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(path)
}
pub fn durable(path: &Path, value: &Json) -> R<()> {
    let parent = path.parent().ok_or("record parent")?;
    let tmp = child(parent, &fmt(format_args!(".record-{}", random_hex(12)?))?)?;
    let result = (|| -> R<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        writeln!(f, "{value}")?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
pub fn free_bytes(path: &Path) -> R<u64> {
    let host = Host::default();
    let output = host.out(
        host.cmd("df")
            .args(["-B1", "--output=avail", "--"])
            .arg(path),
    )?;
    Ok(output
        .lines()
        .nth(1)
        .ok_or("filesystem space")?
        .trim()
        .parse()?)
}
#[derive(Default)]
pub struct Host {
    until: Mutex<Option<Instant>>,
    recovering: AtomicBool,
}
static CANCELLED: AtomicBool = AtomicBool::new(false);
static SUPERVISING: AtomicBool = AtomicBool::new(false);
pub fn block_signals() -> R<()> {
    crate::signal_sys::block()?;
    SUPERVISING.store(true, Ordering::SeqCst);
    Ok(())
}

fn command_label(cmd: &Command) -> &OsStr {
    let mut args = cmd.get_args();
    if cmd.get_program() == "env"
        && args.next() == Some(OsStr::new("--default-signal=INT,TERM"))
        && args.next() == Some(OsStr::new("--"))
    {
        return args.next().unwrap_or(cmd.get_program());
    }
    cmd.get_program()
}

fn check_status(cmd: &Command, status: std::process::ExitStatus) -> R<()> {
    if status.success() {
        return Ok(());
    }
    Err(fmt(format_args!(
        "{} exited {status}",
        command_label(cmd).to_string_lossy()
    ))?
    .into())
}

impl Host {
    pub fn cmd(&self, program: impl AsRef<OsStr>) -> Command {
        // Children must receive ordinary TERM/INT, including the WAL receiver.
        let mut cmd = Command::new("env");
        cmd.args(["--default-signal=INT,TERM","--"]).arg(program)
           .env_clear().env("HOME","/root").env("LANG","C").env("LC_ALL","C")
           .env("PATH","/opt/td-mta/postgresql/usr/lib/postgresql/17/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
           .env("LD_LIBRARY_PATH","/opt/td-mta/postgresql/usr/lib/x86_64-linux-gnu")
           .env("PGSERVICEFILE","/var/lib/td-mta/backup-control/service.conf").env("PGCONNECT_TIMEOUT","3");
        cmd
    }
    pub fn deadline(&self, until: Option<Instant>) {
        *self.until.lock().unwrap_or_else(|e| e.into_inner()) = until;
    }
    pub fn recover(&self) {
        self.recovering.store(true, Ordering::SeqCst);
        self.deadline(None);
    }
    pub fn remaining(&self) -> R<Duration> {
        if SUPERVISING.load(Ordering::SeqCst) && !self.recovering.load(Ordering::SeqCst) {
            if crate::signal_sys::requested()? {
                CANCELLED.store(true, Ordering::SeqCst);
            }
            if CANCELLED.load(Ordering::SeqCst) {
                return Err("deployment interrupted".into());
            }
        }
        match *self.until.lock().map_err(|_| "cutover clock")? {
            Some(end) => end
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "listener interruption budget exhausted".into()),
            None => Ok(Duration::MAX),
        }
    }
    pub fn run(&self, cmd: &mut Command) -> R<()> {
        self.exec(cmd, &[], Duration::from_secs(40), true)?;
        Ok(())
    }
    pub fn out(&self, cmd: &mut Command) -> R<String> {
        let p = self.exec(cmd, &[], Duration::from_secs(40), true)?;
        owned(std::str::from_utf8(&p.stdout)?.trim())
    }
    pub fn exec(
        &self,
        cmd: &mut Command,
        input: &[u8],
        timeout: Duration,
        check: bool,
    ) -> R<Output> {
        let (mut input_socket, child_in) = UnixStream::pair()?;
        let (mut output_socket, child_out) = UnixStream::pair()?;
        input_socket.set_nonblocking(true)?;
        output_socket.set_nonblocking(true)?;
        cmd.stdin(Stdio::from(OwnedFd::from(child_in)))
            .stdout(Stdio::from(OwnedFd::from(child_out)))
            .stderr(Stdio::null())
            .process_group(0);
        let end = Instant::now()
            .checked_add(timeout.min(self.remaining()?))
            .ok_or("command timeout")?;
        let mut child = cmd.spawn()?;
        // Release the parent's copies of the child ends, including on reusable Commands.
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        let mut out = Vec::new();
        let mut pending = input;
        let mut writing = true;
        let mut eof = false;
        let mut buf = [0u8; 16384];
        let result = (|| -> R<_> {
            loop {
                self.remaining()?;
                require(Instant::now() < end, "command timed out")?;
                if writing {
                    if pending.is_empty() {
                        input_socket.shutdown(Shutdown::Write)?;
                        writing = false;
                    } else {
                        match input_socket.write(pending) {
                            Ok(n) if n > 0 => pending = pending.get(n..).ok_or("write count")?,
                            Ok(_) => return Err("short command input".into()),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => writing = false,
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
                if !eof {
                    match output_socket.read(&mut buf) {
                        Ok(0) => eof = true,
                        Ok(n) => {
                            require(
                                out.len().saturating_add(n) <= 16 * 1024 * 1024,
                                "command output too large",
                            )?;
                            td_alloc::extend_from_slice(
                                &mut out,
                                buf.get(..n).ok_or("read count")?,
                            )?;
                            continue;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                        Err(e) => return Err(e.into()),
                    }
                }
                // Keep the leader unreaped while descendants still hold stdout.
                // Its PID then remains reserved if we need to stop the group.
                if eof {
                    if let Some(done) = child.try_wait()? {
                        return Ok(done);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        let status = match result {
            Ok(s) => s,
            Err(e) => {
                stop_group(&mut child);
                return Err(e);
            }
        };
        if check {
            check_status(cmd, status)?;
        }
        require(
            !check || pending.is_empty(),
            "command closed its input early",
        )?;
        Ok(Output {
            status,
            stdout: out,
            stderr: Vec::new(),
        })
    }
    pub fn run_to(
        &self,
        cmd: &mut Command,
        timeout: Duration,
        stdout_file: &File,
        stderr_file: Option<&File>,
    ) -> R<()> {
        let end = Instant::now()
            .checked_add(timeout.min(self.remaining()?))
            .ok_or("command timeout")?;
        cmd.stdin(Stdio::null())
            .stdout(stdout_file.try_clone()?)
            .stderr(match stderr_file {
                Some(f) => Stdio::from(f.try_clone()?),
                None => Stdio::null(),
            })
            .process_group(0);
        let mut process = cmd.spawn()?;
        loop {
            if let Some(s) = process.try_wait()? {
                return check_status(cmd, s);
            }
            if self.remaining().is_err() || Instant::now() >= end {
                stop_group(&mut process);
                return Err("command interrupted or timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
pub(crate) fn stop_group(process: &mut Child) {
    if let Ok(group) = fmt(format_args!("-{}", process.id())) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = process.kill();
    let _ = process.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_command_names_only_the_program() {
        let host = Host::default();
        let mut command = host.cmd("psql");
        command.args(["-c", "SELECT private_fixture"]);
        assert_eq!(command_label(&command), "psql");
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8);
        assert!(
            matches!(check_status(&command, status), Err(e) if e.to_string() == "psql exited exit status: 1")
        );
    }

    #[test]
    fn commands_exchange_large_input_without_deadlock() -> R<()> {
        let input = vec![0x6a; 1024 * 1024];
        let host = Host::default();
        let result = host.exec(
            &mut Command::new("cat"),
            &input,
            Duration::from_secs(5),
            true,
        )?;
        assert_eq!(result.stdout, input);
        assert!(host
            .exec(
                &mut Command::new("true"),
                &input,
                Duration::from_secs(5),
                true
            )
            .is_err());
        Ok(())
    }

    #[test]
    fn deadline_reaps_group_when_a_descendant_holds_stdout() {
        let host = Host::default();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 60 & exit 0"]);
        let start = Instant::now();
        let result = host.exec(&mut cmd, &[], Duration::from_millis(150), true);
        assert!(matches!(result,Err(e) if e.to_string().contains("timed out")));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
