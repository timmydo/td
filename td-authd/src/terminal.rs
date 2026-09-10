//! An unprivileged terminal bridge; only its fresh slave enters the jail.
use crate::terminal_sys as sys;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;

const BUFFER: usize = 8192;
const INPUT_LIMIT: usize = 64 * 1024;
const O_NOCTTY: i32 = 0x100;
const O_NONBLOCK: i32 = 0x800;

struct Terminal<'a> {
    fd: BorrowedFd<'a>,
    saved: [u8; 36],
}
impl Drop for Terminal<'_> {
    fn drop(&mut self) {
        let _ = sys::relay_termios_set(self.fd, &self.saved);
    }
}

fn raw(fd: BorrowedFd<'_>) -> io::Result<Terminal<'_>> {
    let saved = sys::relay_termios_get(fd)?;
    let terminal = Terminal { fd, saved };
    let mut desired = saved;
    // cfmakeraw's Linux flag masks, preserving the kernel's speed/control bytes.
    for (at, clear, set) in [
        (0, 0x5eb_u32, 0),
        (4, 1, 0),
        (8, 0x130, 0x30),
        (12, 0x804b, 0),
    ] {
        let slot = desired
            .get_mut(at..at + 4)
            .ok_or_else(|| io::Error::other("termios layout"))?;
        let word = u32::from_ne_bytes(
            slot.try_into()
                .map_err(|_| io::Error::other("termios word"))?,
        );
        slot.copy_from_slice(&((word & !clear) | set).to_ne_bytes());
    }
    if let Some(value) = desired.get_mut(22) {
        *value = 0;
    } // VTIME
    if let Some(value) = desired.get_mut(23) {
        *value = 1;
    } // VMIN
    sys::relay_termios_set(fd, &desired)?;
    if sys::relay_termios_get(fd)? != desired {
        return Err(io::Error::other("terminal did not accept relay mode"));
    }
    Ok(terminal)
}

pub(crate) fn terminal_pair() -> io::Result<(File, File)> {
    let master = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open("/dev/ptmx")?;
    let slave = sys::relay_pty_peer(master.as_fd())?;
    Ok((master, slave))
}

pub(crate) fn initial_size() -> io::Result<[u16; 4]> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        sys::relay_window_get(io::stdin().as_fd())
    } else {
        Ok([24, 80, 0, 0])
    }
}

pub(crate) fn relay(mut master: File) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let terminal = stdin.is_terminal() && stdout.is_terminal();
    let mut size = initial_size()?;
    sys::relay_window_set(master.as_fd(), &size)?;
    let _terminal = if terminal {
        Some(raw(stdin.as_fd())?)
    } else {
        None
    };
    let mut input = File::from(stdin.as_fd().try_clone_to_owned()?);
    let mut output = stdout.lock();
    bridge(
        &mut master,
        &mut input,
        &mut output,
        stdin.as_raw_fd(),
        &mut size,
        terminal,
    )
}

fn bridge(
    master: &mut File,
    input: &mut impl Read,
    output: &mut impl Write,
    input_fd: i32,
    size: &mut [u16; 4],
    terminal: bool,
) -> io::Result<()> {
    let mut queue = VecDeque::with_capacity(INPUT_LIMIT);
    let mut buffer = [0u8; BUFFER];
    let mut eof = false;
    loop {
        if terminal {
            let current = sys::relay_window_get(io::stdin().as_fd())?;
            if current != *size {
                sys::relay_window_set(master.as_fd(), &current)?;
                *size = current;
            }
        }
        let (input_ready, master_ready) = sys::relay_poll(
            if !eof && queue.len() <= INPUT_LIMIT - BUFFER {
                input_fd
            } else {
                -1
            },
            master.as_fd(),
            !queue.is_empty(),
        )?;
        if master_ready & 1 != 0 || master_ready & 0x18 != 0 {
            match master.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    output.write_all(
                        buffer
                            .get(..count)
                            .ok_or_else(|| io::Error::other("PTY read count"))?,
                    )?;
                    output.flush()?;
                }
                Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if input_ready {
            match input.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    queue.push_back(4);
                }
                Ok(count) => queue.extend(
                    buffer
                        .get(..count)
                        .ok_or_else(|| io::Error::other("input read count"))?,
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if master_ready & 4 != 0 && !queue.is_empty() {
            match master.write(queue.as_slices().0) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "PTY stopped accepting input",
                    ))
                }
                Ok(count) => {
                    queue.drain(..count);
                }
                Err(error) if error.raw_os_error() == Some(5) => {
                    // The slave closed while input was queued. Drain its output
                    // before returning the child's status through the read path.
                    queue.clear();
                    eof = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::process::{Command, Stdio};

    #[test]
    fn raw_mode_restores_the_exact_original_terminal() {
        let (_master, slave) = terminal_pair().unwrap();
        let before = sys::relay_termios_get(slave.as_fd()).unwrap();
        {
            let _raw = raw(slave.as_fd()).unwrap();
            let after = sys::relay_termios_get(slave.as_fd()).unwrap();
            assert_ne!(before, after);
        }
        assert_eq!(sys::relay_termios_get(slave.as_fd()).unwrap(), before);
    }

    #[test]
    fn child() {
        let Some(expected_uid) = std::env::var_os("TD_RELAY_TEST_UID") else {
            return;
        };
        assert_eq!(
            std::fs::metadata("/proc/self").unwrap().uid().to_string(),
            expected_uid.to_str().unwrap()
        );
        assert_eq!(
            std::env::current_dir().unwrap(),
            std::path::Path::new("/tmp")
        );
        assert_eq!(
            sys::relay_window_get(io::stdin().as_fd()).unwrap(),
            [31, 97, 0, 0]
        );
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line, "hello relay\n");
        writeln!(io::stdout(), "TD-RELAY-CHILD-OK").unwrap();
    }

    #[test]
    fn fresh_pty_relays_input_output_and_preserves_uid_and_cwd() {
        let (mut master, slave) = terminal_pair().unwrap();
        let mut size = [31, 97, 0, 0];
        sys::relay_window_set(master.as_fd(), &size).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "terminal::tests::child", "--nocapture"])
            .env(
                "TD_RELAY_TEST_UID",
                std::fs::metadata("/proc/self").unwrap().uid().to_string(),
            )
            .current_dir("/tmp")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let mut child = command.spawn().unwrap();
        drop(command);
        let (mut input, mut writer) = io::pipe().unwrap();
        writer.write_all(b"hello relay\n").unwrap();
        drop(writer);
        let input_fd = input.as_raw_fd();
        let mut output = Vec::new();
        bridge(
            &mut master,
            &mut input,
            &mut output,
            input_fd,
            &mut size,
            false,
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("TD-RELAY-CHILD-OK"));
    }
}
