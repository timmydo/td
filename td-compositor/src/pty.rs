//! td-term's PTY and child-process adapter.
//!
//! The policy here — grid derivation, account selection, environment, and argv
//! — is pure and tested without a device. Only `Pty` itself touches the kernel,
//! and it does so through the four reviewed `ioctl(2)` requests in `sys.rs`.
//!
//! The device half has no production caller yet: the Wayland client that
//! composes it needs the renderer, which waits on the pinned font asset. Host
//! tests drive every item against a real PTY, and `selftest` covers the policy
//! layer inside the packaged binary, where devpts may not be mounted. Each such
//! item carries its own `dead_code` allow rather than the module carrying one,
//! so anything left over after the client lands is still visible.

use crate::sys;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::thread::JoinHandle;

/// `O_NOCTTY` — the child claims the terminal, through cttyhack. td-term must
/// not acquire it by opening the master or the peer.
const O_NOCTTY: i32 = 0o400;

/// The kernel's hangup once the last slave descriptor is gone.
const EIO: i32 = 5;

/// Awaiting the Wayland client that composes this adapter.
#[allow(dead_code)]
pub const DEV_PTMX: &str = "/dev/ptmx";

/// The declared td-init input that gives the child a session and a controlling
/// terminal; safe `Command` reaches neither.
pub const CTTYHACK: &str = "/bin/cttyhack";
pub const CTTYHACK_STDIN: &str = "--stdin";
pub const DEFAULT_SHELL: &str = "/bin/sh";

/// §10's PTY-output ceiling, as whole read chunks. A full channel blocks the
/// reader thread, which is how the kernel's PTY buffer backpressures the child.
#[allow(dead_code)]
pub const READ_CHUNK: usize = 8 * 1024;
#[allow(dead_code)]
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
#[allow(dead_code)]
pub const MAX_OUTPUT_CHUNKS: usize = MAX_OUTPUT_BYTES / READ_CHUNK;

/// Bounded reads of the two small files the child environment is derived from.
const MAX_STATUS_BYTES: usize = 64 * 1024;
const MAX_PASSWD_BYTES: usize = 1024 * 1024;

/// The graphical account, as `/proc/self/status` and `/etc/passwd` agree it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub uid: u32,
    pub name: String,
    pub home: String,
}

/// An open PTY master whose slave has been unlocked but not yet handed out.
#[allow(dead_code)]
pub struct Pty {
    master: File,
}

#[allow(dead_code)]
impl Pty {
    pub fn open(ptmx: &Path) -> Result<Pty, String> {
        let master = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(ptmx)
            .map_err(|e| format!("open {}: {e}", ptmx.display()))?;
        sys::unlock_pty(&master)?;
        Ok(Pty { master })
    }

    pub fn master(&self) -> &File {
        &self.master
    }

    pub fn into_master(self) -> File {
        self.master
    }

    /// The slave, obtained from the master rather than by name.
    pub fn peer(&self) -> Result<File, String> {
        sys::pty_peer(&self.master)
    }

    /// Publish a grid size and verify it before anything may observe it. An
    /// unverified `TIOCSWINSZ` is indistinguishable at the call site from one
    /// the kernel clamped or ignored, and the child would then lay out its
    /// screen for a size the terminal does not have.
    pub fn resize(&self, rows: usize, columns: usize) -> Result<sys::WindowSize, String> {
        let requested = grid_size(rows, columns)?;
        sys::set_window_size(&self.master, requested)?;
        let observed = sys::window_size(&self.master)?;
        if observed.rows != requested.rows || observed.columns != requested.columns {
            return Err(format!(
                "published {}x{} but the terminal reports {}x{}",
                requested.rows, requested.columns, observed.rows, observed.columns
            ));
        }
        Ok(observed)
    }
}

/// A grid the kernel can represent. Zero is not a size a terminal can be laid
/// out for, and the winsize fields are sixteen bits wide.
pub fn grid_size(rows: usize, columns: usize) -> Result<sys::WindowSize, String> {
    let rows = u16::try_from(rows)
        .ok()
        .filter(|rows| *rows > 0)
        .ok_or_else(|| format!("terminal row count {rows} is not a representable grid"))?;
    let columns = u16::try_from(columns)
        .ok()
        .filter(|columns| *columns > 0)
        .ok_or_else(|| format!("terminal column count {columns} is not a representable grid"))?;
    Ok(sys::WindowSize {
        rows,
        columns,
        x_pixels: 0,
        y_pixels: 0,
    })
}

/// The cell grid a tile of this many pixels holds. A tile too small for one
/// cell still gets a logical 1-by-1 grid, whose pixels the renderer clips to
/// the actual surface; a zero-row terminal has no representable state.
pub fn grid_for_tile(
    width: usize,
    height: usize,
    cell_width: usize,
    cell_height: usize,
) -> Result<(usize, usize), String> {
    if cell_width == 0 || cell_height == 0 {
        return Err(format!(
            "font cell {cell_width}x{cell_height} has no area"
        ));
    }
    let columns = width.checked_div(cell_width).unwrap_or(0).max(1);
    let rows = height.checked_div(cell_height).unwrap_or(0).max(1);
    Ok((rows, columns))
}

fn read_bounded(path: &Path, limit: usize) -> Result<String, String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if metadata.len() > limit as u64 {
        return Err(format!(
            "{} is larger than the {limit}-byte bound",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.min(4096));
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "{} is larger than the {limit}-byte bound",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))
}

/// The effective uid from `/proc/self/status`. The effective one is what the
/// kernel checks and what owns `/run/user/UID`, so it is what the child's
/// environment must describe.
pub fn effective_uid(status: &str) -> Result<u32, String> {
    let mut uid_line = None;
    for line in status.lines() {
        if let Some(fields) = line.strip_prefix("Uid:") {
            uid_line = Some(fields);
            break;
        }
    }
    let line = uid_line.ok_or_else(|| "process status has no Uid line".to_string())?;
    let mut fields = line.split_whitespace();
    let _real = fields
        .next()
        .ok_or_else(|| "process status Uid line has no real uid".to_string())?;
    let effective = fields
        .next()
        .ok_or_else(|| "process status Uid line has no effective uid".to_string())?;
    effective
        .parse()
        .map_err(|_| format!("process status effective uid '{effective}' is not a number"))
}

/// The unique `/etc/passwd` entry for a uid. Fail-closed on every ambiguity:
/// a duplicate uid, an absent one, or any malformed line closes the terminal
/// rather than starting a shell whose HOME belongs to somebody else.
pub fn account(passwd: &str, uid: u32) -> Result<Account, String> {
    let mut found: Option<Account> = None;
    for (number, line) in passwd.lines().enumerate() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() != 7 {
            return Err(format!(
                "passwd line {} has {} fields, expected 7",
                number.saturating_add(1),
                fields.len()
            ));
        }
        let name = fields.first().copied().unwrap_or_default();
        let entry_uid = fields.get(2).copied().unwrap_or_default();
        let home = fields.get(5).copied().unwrap_or_default();
        let entry_uid: u32 = entry_uid.parse().map_err(|_| {
            format!(
                "passwd line {} has non-numeric uid '{entry_uid}'",
                number.saturating_add(1)
            )
        })?;
        if entry_uid != uid {
            continue;
        }
        if found.is_some() {
            return Err(format!("passwd has more than one entry for uid {uid}"));
        }
        if name.is_empty() {
            return Err(format!("passwd entry for uid {uid} has no user name"));
        }
        if !home.starts_with('/') {
            return Err(format!(
                "passwd entry for uid {uid} has a relative home '{home}'"
            ));
        }
        found = Some(Account {
            uid,
            name: name.to_string(),
            home: home.to_string(),
        });
    }
    found.ok_or_else(|| format!("passwd has no entry for uid {uid}"))
}

/// The account td-term runs as, read from the live process and account files.
#[allow(dead_code)]
pub fn current_account(status: &Path, passwd: &Path) -> Result<Account, String> {
    let uid = effective_uid(&read_bounded(status, MAX_STATUS_BYTES)?)?;
    account(&read_bounded(passwd, MAX_PASSWD_BYTES)?, uid)
}

/// The child's complete environment. It is constructed, never inherited: an
/// outer `TERM` describes the parent terminal and would be a false capability
/// claim for this one.
pub fn environment(account: &Account) -> Vec<(String, String)> {
    vec![
        ("COLORTERM".into(), "truecolor".into()),
        ("HOME".into(), account.home.clone()),
        ("LOGNAME".into(), account.name.clone()),
        ("PATH".into(), "/bin".into()),
        ("SHELL".into(), DEFAULT_SHELL.into()),
        ("TERM".into(), "td-term".into()),
        ("TERMINFO".into(), "/etc/terminfo".into()),
        ("USER".into(), account.name.clone()),
        ("WAYLAND_DISPLAY".into(), "wayland-0".into()),
        (
            "XDG_RUNTIME_DIR".into(),
            format!("/run/user/{}", account.uid),
        ),
    ]
}

/// What td-term execs: literal argv values, no shell, no PATH search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildCommand {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
}

/// `/bin/cttyhack --stdin /bin/sh`, or the command supplied on td-term's own
/// command line. Both paths must be absolute: a relative program would be
/// resolved against an ambient PATH this adapter deliberately does not have.
pub fn child_command(wrapper: &Path, command: &[String]) -> Result<ChildCommand, String> {
    if !wrapper.is_absolute() {
        return Err(format!(
            "terminal session wrapper '{}' is not absolute",
            wrapper.display()
        ));
    }
    let program = command.first().map_or(DEFAULT_SHELL, String::as_str);
    if !Path::new(program).is_absolute() {
        return Err(format!("terminal command '{program}' is not absolute"));
    }
    let mut arguments = vec![OsString::from(CTTYHACK_STDIN), OsString::from(program)];
    for argument in command.iter().skip(1) {
        arguments.push(OsString::from(argument));
    }
    Ok(ChildCommand {
        program: wrapper.to_path_buf(),
        arguments,
    })
}

/// Start the child on the slave. The slave and all three parent-side clones are
/// consumed here and dropped before this returns, so only the master remains
/// and closing it produces the kernel's normal hangup.
///
/// `directory` is the account's verified home. Setting `HOME` does not move the
/// child, so without this the shell would start in whatever directory td-svc
/// left the graphical service in and disagree with its own environment. A home
/// the child cannot enter fails the spawn rather than silently landing in `/`.
#[allow(dead_code)]
pub fn spawn(
    command: &ChildCommand,
    environment: &[(String, String)],
    directory: &Path,
    slave: File,
) -> Result<Child, String> {
    let output = slave
        .try_clone()
        .map_err(|e| format!("duplicate terminal for child stdout: {e}"))?;
    let errors = slave
        .try_clone()
        .map_err(|e| format!("duplicate terminal for child stderr: {e}"))?;
    let mut process = Command::new(&command.program);
    process
        .args(&command.arguments)
        .env_clear()
        .current_dir(directory)
        .stdin(Stdio::from(slave))
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors));
    for (name, value) in environment {
        process.env(name, value);
    }
    process.spawn().map_err(|e| {
        format!(
            "spawn {} in {}: {e}",
            command.program.display(),
            directory.display()
        )
    })
}

/// The bounded PTY-output channel between the reader thread and the main loop.
#[allow(dead_code)]
pub fn output_channel() -> (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) {
    sync_channel(MAX_OUTPUT_CHUNKS)
}

/// Pump the master into the bounded channel until hangup. A full channel blocks
/// this thread rather than dropping bytes; `EIO` is the kernel's hangup once the
/// last slave descriptor is gone, not a fault to report.
///
/// This thread owns a master descriptor and is parked in `read` whenever the
/// child is idle, and safe `std` offers no way to interrupt that: no poll, no
/// timeout, and closing a descriptor another thread is reading is not something
/// this crate may express. So the retirement path is the child's — its exit
/// closes the last slave and the read returns — and there is no path that
/// retires the reader while the child lives.
///
/// That is sound only because td-term is one process per terminal (§9): closing
/// "the terminal" IS exiting, process exit closes this descriptor, and the
/// kernel then sends the child `SIGHUP` for its controlling terminal. The
/// caller must therefore NOT join this handle on a teardown path — a detached
/// thread cannot delay process exit, but a join would wait for a read that
/// never returns. Interrupting the reader for any other reason needs a
/// separately reviewed wakeup surface.
#[allow(dead_code)]
pub fn spawn_reader(
    mut master: File,
    sender: SyncSender<Vec<u8>>,
) -> Result<JoinHandle<Result<(), String>>, String> {
    thread::Builder::new()
        .name("td-term-pty".into())
        .spawn(move || {
            let mut buffer = vec![0u8; READ_CHUNK];
            loop {
                match master.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(count) => {
                        let Some(bytes) = buffer.get(..count) else {
                            return Err(format!("PTY read reported {count} bytes of a short buffer"));
                        };
                        if sender.send(bytes.to_vec()).is_err() {
                            return Ok(());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    // EIO is the hangup this thread retires on. Every other
                    // errno is a real fault, and reporting it as a clean child
                    // exit would make a broken terminal look like a closed one.
                    Err(error) if error.raw_os_error() == Some(EIO) => return Ok(()),
                    Err(error) => return Err(format!("read terminal: {error}")),
                }
            }
        })
        .map_err(|e| format!("spawn PTY reader: {e}"))
}

/// Drain the keyboard queue into the child, consuming only what the kernel
/// actually took. `write_all` would lose the remainder of a partial write, and
/// those bytes are keystrokes with nowhere to come back from.
///
/// The master is blocking, so a child that stops reading blocks this call once
/// the line discipline fills. §12 puts the PTY writer on its own thread for
/// exactly that reason; the main loop enqueues and never writes.
#[allow(dead_code)]
pub fn write_input(master: &File, queue: &mut crate::keys::InputQueue) -> Result<(), String> {
    loop {
        if queue.is_empty() {
            return Ok(());
        }
        let mut borrowed = master;
        match borrowed.write(queue.front(READ_CHUNK)) {
            Ok(0) => return Err("terminal accepted no input bytes".into()),
            Ok(count) => queue.consume(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("write terminal input: {error}")),
        }
    }
}

/// The packaged binary's own check of the PTY policy layer. It opens no device:
/// the live ioctl round trip is a host test, because the target selftest runs
/// wherever the artifact does, including where devpts is not mounted.
pub fn selftest() -> Result<(), String> {
    let account = account("root:x:0:0:root:/root:/bin/sh\ntd:x:1000:1000::/var/home/td:/bin/sh\n", 1000)?;
    if account.name != "td" || account.home != "/var/home/td" {
        return Err("PTY selftest selected the wrong account".into());
    }
    if effective_uid("Name:\tsh\nUid:\t1000\t1000\t1000\t1000\n")? != 1000 {
        return Err("PTY selftest misread its own uid".into());
    }
    let environment = environment(&account);
    let named = |name: &str| {
        let mut value = None;
        for (key, candidate) in &environment {
            if key == name {
                value = Some(candidate.as_str());
            }
        }
        value
    };
    if named("TERM") != Some("td-term")
        || named("XDG_RUNTIME_DIR") != Some("/run/user/1000")
        || named("HOME") != Some("/var/home/td")
        || environment.len() != 10
    {
        return Err("PTY selftest built the wrong child environment".into());
    }
    let command = child_command(Path::new(CTTYHACK), &[])?;
    if command.program != Path::new(CTTYHACK)
        || command.arguments != vec![OsString::from(CTTYHACK_STDIN), OsString::from(DEFAULT_SHELL)]
    {
        return Err("PTY selftest composed the wrong child command".into());
    }
    let size = grid_size(24, 80)?;
    if (size.rows, size.columns) != (24, 80)
        || grid_size(0, 80).is_ok()
        || grid_size(24, 0).is_ok()
        || grid_size(1, 65_536).is_ok()
        || grid_for_tile(512, 320, 8, 16)? != (20, 64)
        || grid_for_tile(3, 3, 8, 16)? != (1, 1)
    {
        return Err("PTY selftest derived the wrong grid".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          td:x:1000:1000:td user:/var/home/td:/bin/sh\n";

    fn open_pty() -> Pty {
        Pty::open(Path::new(DEV_PTMX)).unwrap_or_else(|error| {
            panic!("this host cannot provide a PTY, which td-term requires: {error}")
        })
    }

    #[test]
    fn a_grid_must_be_representable_by_the_kernel() {
        assert_eq!(grid_size(24, 80).unwrap().rows, 24);
        assert_eq!(grid_size(24, 80).unwrap().columns, 80);
        assert_eq!(grid_size(24, 80).unwrap().x_pixels, 0);
        assert!(grid_size(0, 80).is_err());
        assert!(grid_size(24, 0).is_err());
        assert!(grid_size(65_536, 80).is_err());
        assert!(grid_size(24, 65_536).is_err());
        assert_eq!(grid_size(65_535, 65_535).unwrap().rows, 65_535);
    }

    #[test]
    fn a_tile_smaller_than_one_cell_still_has_a_logical_grid() {
        assert_eq!(grid_for_tile(512, 320, 8, 16).unwrap(), (20, 64));
        // Partial cells are not shown, so they are not counted.
        assert_eq!(grid_for_tile(519, 335, 8, 16).unwrap(), (20, 64));
        assert_eq!(grid_for_tile(0, 0, 8, 16).unwrap(), (1, 1));
        assert_eq!(grid_for_tile(7, 15, 8, 16).unwrap(), (1, 1));
        assert!(grid_for_tile(512, 320, 0, 16).is_err());
        assert!(grid_for_tile(512, 320, 8, 0).is_err());
    }

    #[test]
    fn the_effective_uid_is_the_one_taken_from_process_status() {
        let status = "Name:\ttd-term\nUid:\t0\t1000\t1000\t1000\nGid:\t0\t1000\t1000\t1000\n";
        assert_eq!(effective_uid(status).unwrap(), 1000);
        assert!(effective_uid("Name:\ttd-term\n").is_err());
        assert!(effective_uid("Uid:\t1000\n").is_err());
        assert!(effective_uid("Uid:\t1000\tnope\n").is_err());
    }

    #[test]
    fn the_account_must_be_unique_well_formed_and_present() {
        let account = account(PASSWD, 1000).unwrap();
        assert_eq!(
            account,
            Account {
                uid: 1000,
                name: "td".into(),
                home: "/var/home/td".into(),
            }
        );
        assert!(account_error(PASSWD, 1001).contains("no entry for uid 1001"));
        let duplicate = format!("{PASSWD}other:x:1000:1000::/var/home/other:/bin/sh\n");
        assert!(account_error(&duplicate, 1000).contains("more than one entry"));
        assert!(account_error("td:x:1000:1000::/var/home/td\n", 1000).contains("6 fields"));
        assert!(account_error(":x:1000:1000::/var/home/td:/bin/sh\n", 1000).contains("no user name"));
        assert!(
            account_error("td:x:1000:1000::var/home/td:/bin/sh\n", 1000).contains("relative home")
        );
        assert!(account_error("td:x:x:1000::/var/home/td:/bin/sh\n", 1000).contains("non-numeric"));
        // Whole-file strictness reaches a blank line too: it is a line td
        // cannot account for, and the entry being looked up may sit after it.
        // `lines()` drops the trailing newline, so a well-formed file has none.
        let blank = format!("\n{PASSWD}");
        assert!(account_error(&blank, 1000).contains("line 1 has 1 fields"));
        let internal = PASSWD.replace("td:x:1000", "\ntd:x:1000");
        assert!(account_error(&internal, 1000).contains("1 fields"));
    }

    fn account_error(passwd: &str, uid: u32) -> String {
        account(passwd, uid).unwrap_err()
    }

    #[test]
    fn the_child_environment_is_constructed_rather_than_inherited() {
        let account = account(PASSWD, 1000).unwrap();
        let environment = environment(&account);
        let names: Vec<&str> = environment.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "COLORTERM",
                "HOME",
                "LOGNAME",
                "PATH",
                "SHELL",
                "TERM",
                "TERMINFO",
                "USER",
                "WAYLAND_DISPLAY",
                "XDG_RUNTIME_DIR",
            ]
        );
        let value = |name: &str| {
            let mut found = None;
            for (key, candidate) in &environment {
                if key == name {
                    found = Some(candidate.clone());
                }
            }
            found.unwrap()
        };
        assert_eq!(value("TERM"), "td-term");
        assert_eq!(value("HOME"), "/var/home/td");
        assert_eq!(value("USER"), "td");
        assert_eq!(value("LOGNAME"), "td");
        assert_eq!(value("XDG_RUNTIME_DIR"), "/run/user/1000");
        assert_eq!(value("WAYLAND_DISPLAY"), "wayland-0");
        assert_eq!(value("TERMINFO"), "/etc/terminfo");
    }

    #[test]
    fn the_child_command_is_literal_argv_through_cttyhack() {
        let default = child_command(Path::new(CTTYHACK), &[]).unwrap();
        assert_eq!(default.program, PathBuf::from("/bin/cttyhack"));
        assert_eq!(default.arguments, vec!["--stdin", "/bin/sh"]);
        let explicit = child_command(
            Path::new(CTTYHACK),
            &["/bin/sh".to_string(), "-c".to_string(), "echo hi".to_string()],
        )
        .unwrap();
        assert_eq!(explicit.arguments, vec!["--stdin", "/bin/sh", "-c", "echo hi"]);
        assert!(child_command(Path::new("cttyhack"), &[]).is_err());
        assert!(child_command(Path::new(CTTYHACK), &["sh".to_string()]).is_err());
    }

    #[test]
    fn current_account_reads_the_live_process_and_account_files() {
        let directory = std::env::temp_dir().join(format!("td-term-account-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let passwd = directory.join("passwd");
        std::fs::write(&passwd, PASSWD).unwrap();
        let status = directory.join("status");
        std::fs::write(&status, "Uid:\t1000\t1000\t1000\t1000\n").unwrap();
        assert_eq!(current_account(&status, &passwd).unwrap().name, "td");
        std::fs::write(&status, "Uid:\t1000\t4242\t4242\t4242\n").unwrap();
        assert!(current_account(&status, &passwd).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// The peer comes from the master, and the size the master publishes is the
    /// size the slave — the child's own descriptor — reports back.
    #[test]
    fn a_published_grid_is_the_one_the_slave_reports() {
        let pty = open_pty();
        let observed = pty.resize(24, 80).unwrap();
        assert_eq!((observed.rows, observed.columns), (24, 80));
        let slave = pty.peer().unwrap();
        let from_slave = sys::window_size(&slave).unwrap();
        assert_eq!((from_slave.rows, from_slave.columns), (24, 80));
        // A later resize reaches the same already-open slave.
        pty.resize(40, 100).unwrap();
        let from_slave = sys::window_size(&slave).unwrap();
        assert_eq!((from_slave.rows, from_slave.columns), (40, 100));
        assert!(pty.resize(0, 80).is_err());
    }

    /// Bytes written to the master reach the slave, and the reader thread
    /// delivers what the slave writes back.
    #[test]
    fn the_reader_thread_delivers_slave_output_until_hangup() {
        let pty = open_pty();
        pty.resize(24, 80).unwrap();
        let mut slave = pty.peer().unwrap();
        let master = pty.master().try_clone().unwrap();
        let (sender, receiver) = output_channel();
        let reader = spawn_reader(master, sender).unwrap();

        // Through the bounded queue, as the writer thread will: the queue is
        // what makes a partial write recoverable.
        let mut queue = crate::keys::InputQueue::new();
        assert!(queue.push(b"input\n"));
        write_input(pty.master(), &mut queue).unwrap();
        assert!(queue.is_empty(), "the writer consumed only what it wrote");
        let mut seen = Vec::new();
        while !seen.windows(6).any(|window| window == b"input\n") {
            let mut chunk = [0u8; 64];
            let count = slave.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "the slave saw hangup before its input");
            seen.extend_from_slice(&chunk[..count]);
        }

        slave.write_all(b"output\n").unwrap();
        let mut delivered = Vec::new();
        while !delivered.windows(6).any(|window| window == b"output") {
            let chunk = receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("the reader thread stopped before delivering output");
            delivered.extend_from_slice(&chunk);
        }

        // Dropping every slave descriptor is the kernel's hangup, and the
        // reader retires on it rather than reporting a fault. The channel is
        // observed FIRST: a leaked parent-side slave would leave the reader
        // parked in `read` forever, and joining first would hang the gate
        // instead of failing it — `cargo test` has no per-test timeout.
        drop(slave);
        assert!(
            matches!(
                receiver.recv_timeout(Duration::from_secs(30)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            "the reader did not retire on hangup"
        );
        reader.join().unwrap().unwrap();
    }

    /// The child gets the slave on all three descriptors, sees the grid the
    /// master published, and receives exactly the constructed environment.
    #[test]
    fn a_spawned_child_inherits_the_slave_and_the_published_grid() {
        let pty = open_pty();
        pty.resize(31, 97).unwrap();
        let slave = pty.peer().unwrap();
        let command = ChildCommand {
            program: std::env::current_exe().unwrap(),
            arguments: vec![
                "--exact".into(),
                "pty::tests::pty_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        };
        let account = account(PASSWD, 1000).unwrap();
        let mut environment = environment(&account);
        environment.push((FIXTURE.into(), "1".into()));
        let home = std::env::temp_dir();
        let mut child = spawn(&command, &environment, &home, slave).unwrap();

        let master = pty.into_master();
        let (sender, receiver) = output_channel();
        let reader = spawn_reader(master, sender).unwrap();
        let mut seen = String::new();
        let marker = loop {
            match receiver.recv_timeout(Duration::from_secs(30)) {
                Ok(chunk) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    // Complete lines only. `str::lines` yields an
                    // unterminated tail as a line, so a chunk boundary inside
                    // the marker would otherwise be accepted as a short one.
                    let mut marker = None;
                    let complete = seen.rsplit_once('\n').map_or("", |(head, _)| head);
                    for line in complete.lines() {
                        if let Some(tail) = line.trim_end().strip_prefix("TD-TERM-FIXTURE ") {
                            marker = Some(tail.to_string());
                        }
                    }
                    if let Some(marker) = marker {
                        break marker;
                    }
                }
                Err(error) => panic!("no fixture marker in {seen:?}: {error}"),
            }
        };
        // rows, columns, TERM, environment size, and working directory, all as
        // the child itself observed them. The directory is the one passed to
        // `spawn`: setting HOME does not move a child, so this is what proves
        // the shell starts where its own environment says it does.
        let home = home.canonicalize().unwrap();
        assert_eq!(marker, format!("31 97 td-term 11 {}", home.display()));
        let status = child.wait().unwrap();
        assert!(status.success(), "the fixture failed its own checks: {status}");
        // Same ordering as above: the child's exit closed the last slave, so
        // the channel must disconnect before the join can be safe.
        while receiver.recv_timeout(Duration::from_secs(30)).is_ok() {}
        reader.join().unwrap().unwrap();
    }

    const FIXTURE: &str = "TD_TERM_PTY_FIXTURE";

    /// The child half of the test above: it runs only when the parent asks for
    /// it, and reports what it sees on its own stdin.
    #[test]
    #[ignore]
    fn pty_child_fixture() {
        if std::env::var_os(FIXTURE).is_none() {
            return;
        }
        let size = sys::window_size(&std::io::stdin()).unwrap();
        let term = std::env::var("TERM").unwrap_or_default();
        let count = std::env::vars_os().count();
        let directory = std::env::current_dir().unwrap();
        println!(
            "TD-TERM-FIXTURE {} {} {term} {count} {}",
            size.rows,
            size.columns,
            directory.display()
        );
    }

    #[test]
    fn the_selftest_covers_the_policy_layer() {
        selftest().unwrap();
    }
}
