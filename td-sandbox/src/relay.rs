use crate::wire;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use td_ui::pty::{ChildCommand, Pty, RelayTerminal};

pub struct Running(pub Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn err(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}
fn exit_code(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or_else(|| u8::try_from(128 + status.signal().unwrap_or(1)).unwrap_or(1))
}

pub fn outside(
    mut channel: UnixStream,
    child: &mut Running,
    stop: &std::os::unix::net::UnixListener,
    terminal: bool,
) -> io::Result<u8> {
    let mut terminal = if terminal {
        Some(RelayTerminal::enter(File::from(
            io::stdin().as_fd().try_clone_to_owned()?,
        ))?)
    } else {
        None
    };
    let mut reader = channel.try_clone()?;
    let (tx, rx) = mpsc::sync_channel(128);
    std::thread::spawn(move || loop {
        let frame = wire::read(&mut reader);
        let done = frame.is_err();
        if tx.send(frame).is_err() || done {
            break;
        }
    });
    let (input_tx, input_rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(128);
    std::thread::spawn(move || {
        for (kind, bytes) in input_rx {
            if wire::send(&mut channel, kind, &bytes).is_err() {
                break;
            }
        }
    });
    if terminal.is_none() {
        let tx = input_tx.clone();
        std::thread::spawn(move || {
            let mut bytes = [0; wire::LIMIT];
            let mut input = io::stdin();
            loop {
                match input.read(&mut bytes) {
                    Ok(0) => {
                        let _ = tx.send((wire::EOF, Vec::new()));
                        break;
                    }
                    Ok(n) => {
                        if tx
                            .send((wire::INPUT, bytes.get(..n).unwrap_or_default().to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    let (output_tx, output_rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(128);
    let (written_tx, written_rx) = mpsc::sync_channel(128);
    std::thread::spawn(move || {
        for (kind, bytes) in output_rx {
            let result = if kind == wire::OUTPUT {
                io::stdout()
                    .write_all(&bytes)
                    .and_then(|()| io::stdout().flush())
            } else {
                io::stderr()
                    .write_all(&bytes)
                    .and_then(|()| io::stderr().flush())
            };
            let failed = result.is_err();
            if written_tx.send(result).is_err() || failed {
                break;
            }
        }
    });
    let mut size = None;
    let mut bytes = [0; wire::LIMIT];
    let mut pending_input = None;
    let mut pending_output = None;
    let mut outstanding = 0usize;
    let mut exit = None;
    loop {
        if stop.accept().is_ok() {
            return Ok(143);
        }
        for result in written_rx.try_iter() {
            result?;
            outstanding = outstanding.saturating_sub(1);
        }
        if let Some(code) = exit {
            if outstanding == 0 && pending_output.is_none() {
                return Ok(code);
            }
        }
        if let Some(terminal) = &mut terminal {
            if let Some(signal) = terminal.signal()? {
                return Ok(128 + signal);
            }
            if pending_input.is_none() && exit.is_none() {
                let now = terminal.window().map_err(err)?;
                if size != Some(now) {
                    let [a, b] = now.rows.to_le_bytes();
                    let [c, d] = now.columns.to_le_bytes();
                    pending_input = Some((wire::SIZE, vec![a, b, c, d]));
                    size = Some(now);
                } else {
                    match terminal.read(&mut bytes) {
                        Ok(0) => {}
                        Ok(n) => {
                            pending_input =
                                Some((wire::INPUT, bytes.get(..n).unwrap_or_default().to_vec()))
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        if let Some(frame) = pending_input.take() {
            match input_tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(frame)) => pending_input = Some(frame),
                Err(mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
        for _ in 0..128 {
            if let Some(frame) = pending_output.take() {
                match output_tx.try_send(frame) {
                    Ok(()) => outstanding += 1,
                    Err(mpsc::TrySendError::Full(frame)) => {
                        pending_output = Some(frame);
                        break;
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        return Err(err("host output relay failed"))
                    }
                }
            }
            if exit.is_some() {
                break;
            }
            match rx.try_recv() {
                Ok(Ok((kind @ (wire::OUTPUT | wire::ERROR), bytes))) => {
                    pending_output = Some((kind, bytes))
                }
                Ok(Ok((wire::EXIT, bytes))) if bytes.len() == 1 => exit = bytes.first().copied(),
                Ok(Ok(_)) => return Err(err("unexpected jail relay frame")),
                Ok(Err(e)) => {
                    return match child.0.try_wait()? {
                        Some(status) if !status.success() => Ok(exit_code(status)),
                        _ => Err(e),
                    };
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(err("jail ended without an exit frame"))
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn is_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

type Output = Arc<Mutex<io::Stdout>>;
fn output(to: &Output, kind: u8, bytes: &[u8]) -> io::Result<()> {
    wire::send(&mut *to.lock().map_err(err)?, kind, bytes)
}
fn pump(
    mut from: impl Read + Send + 'static,
    to: Output,
    kind: u8,
) -> std::thread::JoinHandle<io::Result<()>> {
    std::thread::spawn(move || {
        let mut bytes = [0; wire::LIMIT];
        loop {
            match from.read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(n) => output(&to, kind, bytes.get(..n).unwrap_or_default())?,
                Err(e) if e.raw_os_error() == Some(5) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    })
}

pub fn inside(arguments: Vec<OsString>) -> io::Result<u8> {
    if std::env::current_exe()? != Path::new("/opt/workspace/bin/td-sandbox") {
        return Err(err("internal entry requires the workspace jail"));
    }
    let mut args = arguments.into_iter();
    let mode = args.next().ok_or_else(|| err("missing relay mode"))?;
    let terminal = match mode.to_str() {
        Some("pty") => true,
        Some("pipes") => false,
        _ => return Err(err("invalid relay mode")),
    };
    let term = args.next().ok_or_else(|| err("missing terminal type"))?;
    if !term.to_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
    }) {
        return Err(err("invalid terminal type"));
    }
    let program = PathBuf::from(args.next().ok_or_else(|| err("missing command"))?);
    let args: Vec<_> = args.collect();
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| err("missing private home"))?);
    let mut path = home.join(".local/bin").into_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let mut environment: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    environment.retain(|(k, _)| k != "PATH" && k != "TERM");
    environment.extend([("PATH".into(), path), ("TERM".into(), term)]);
    for ca in ["/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem"] {
        if Path::new(ca).is_file() {
            environment.extend([
                ("SSL_CERT_FILE".into(), ca.into()),
                ("CURL_CA_BUNDLE".into(), ca.into()),
            ]);
            break;
        }
    }
    // The chosen shell is supplied by the trusted outer launcher for shell mode;
    // direct commands discover shells on the admitted PATH as usual.
    if program
        .file_name()
        .is_some_and(|n| n == "bash" || n == "zsh" || n == "sh" || n == "td-sh")
    {
        environment.push(("SHELL".into(), program.clone().into_os_string()));
    }
    let output_channel = Arc::new(Mutex::new(io::stdout()));
    let pty = if terminal {
        let p = Pty::open().map_err(err)?;
        p.resize(24, 80).map_err(err)?;
        Some(p)
    } else {
        None
    };
    let mut child;
    let input: Option<Box<dyn Write + Send>>;
    let mut pumps = Vec::new();
    if let Some(pty) = &pty {
        child = Running(
            td_ui::pty::spawn(
                &ChildCommand {
                    program,
                    arguments: args,
                    leads_session: true,
                },
                &environment,
                &std::env::current_dir()?,
                pty.peer().map_err(err)?,
            )
            .map_err(err)?,
        );
        input = Some(Box::new(pty.master().try_clone()?));
        pumps.push(pump(
            pty.master().try_clone()?,
            output_channel.clone(),
            wire::OUTPUT,
        ));
    } else {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child = Running(command.spawn()?);
        input = child
            .0
            .stdin
            .take()
            .map(|v| Box::new(v) as Box<dyn Write + Send>);
        pumps.push(pump(
            child.0.stdout.take().ok_or_else(|| err("missing stdout"))?,
            output_channel.clone(),
            wire::OUTPUT,
        ));
        pumps.push(pump(
            child.0.stderr.take().ok_or_else(|| err("missing stderr"))?,
            output_channel.clone(),
            wire::ERROR,
        ));
    }
    let (tx, rx) = mpsc::sync_channel(128);
    std::thread::spawn(move || {
        let mut input = io::stdin();
        loop {
            let frame = wire::read(&mut input);
            let done = frame.is_err();
            if tx.send(frame).is_err() || done {
                break;
            }
        }
    });
    let (input_tx, input_rx) = mpsc::sync_channel::<Option<Vec<u8>>>(128);
    std::thread::spawn(move || {
        let mut input = input;
        for bytes in input_rx {
            match bytes {
                Some(bytes) => {
                    if let Some(to) = &mut input {
                        if to.write_all(&bytes).is_err() {
                            input.take();
                        }
                    }
                }
                None => {
                    input.take();
                }
            }
        }
    });
    let mut pending_input = None;
    let code = loop {
        if let Some(status) = child.0.try_wait()? {
            break exit_code(status);
        }
        if let Some(bytes) = pending_input.take() {
            match input_tx.try_send(bytes) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(bytes)) => {
                    pending_input = Some(bytes);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(err("input relay failed")),
            }
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok((wire::INPUT, bytes))) => pending_input = Some(Some(bytes)),
            Ok(Ok((wire::SIZE, bytes))) => {
                let words: [u8; 4] = bytes.try_into().map_err(|_| err("invalid resize"))?;
                let [a, b, c, d] = words;
                if u16::from_le_bytes([a, b]) == 0 || u16::from_le_bytes([c, d]) == 0 {
                    return Err(err("empty terminal size"));
                }
                if let Some(pty) = &pty {
                    pty.resize(
                        usize::from(u16::from_le_bytes([a, b])),
                        usize::from(u16::from_le_bytes([c, d])),
                    )
                    .map_err(err)?;
                }
            }
            Ok(Ok((wire::EOF, bytes))) if bytes.is_empty() => pending_input = Some(None),
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(err("relay channel closed or invalid"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    // Descendants may retain output descriptors. Give output a bounded drain;
    // PID 1 tears down every descendant once this entry returns.
    for handle in pumps {
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            handle.join().map_err(|_| err("output relay failed"))??;
        }
    }
    output(&output_channel, wire::EXIT, &[code])?;
    Ok(0)
}
