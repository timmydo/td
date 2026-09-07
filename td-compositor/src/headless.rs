//! Disposable, process-owned software sessions for native clients and tests.

use crate::{control, framebuffer::Framebuffer, runtime::Runtime, server};
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

const USAGE: &str = "headless requires --session-dir NEW_ABSOLUTE_PATH \
    --width N --height N [--input-control enabled] [--capture-control enabled]; \
    keep stdin open for the session lifetime";

#[derive(Debug)]
struct Options {
    directory: PathBuf,
    width: usize,
    height: usize,
    input_control: bool,
    capture_control: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        if !matches!(args.len(), 6 | 8 | 10) {
            return Err(USAGE.into());
        }
        let (mut directory, mut width, mut height) = (None, None, None);
        let mut input_control = None;
        let mut capture_control = None;
        for [flag, value] in args.as_chunks::<2>().0 {
            let slot = match flag.as_str() {
                "--session-dir" => &mut directory,
                "--width" => &mut width,
                "--height" => &mut height,
                "--input-control" => &mut input_control,
                "--capture-control" => &mut capture_control,
                _ => return Err(format!("unknown headless option {flag}; {USAGE}")),
            };
            if slot.replace(value.as_str()).is_some() {
                return Err(format!("duplicate headless option {flag}"));
            }
        }
        let directory = PathBuf::from(directory.ok_or(USAGE)?);
        if !directory.is_absolute() || directory.file_name().is_none() {
            return Err(USAGE.into());
        }
        let dimension = |value: Option<&str>| -> Result<usize, String> {
            let value = value.ok_or(USAGE)?;
            let number = value.parse::<usize>().map_err(|_| USAGE)?;
            if number == 0 || number > crate::MAX_UI_DIMENSION {
                return Err("headless dimension outside 1..=16384".into());
            }
            Ok(number)
        };
        let width = dimension(width)?;
        let height = dimension(height)?;
        let input_control = match input_control {
            None => false,
            Some("enabled") => true,
            Some(_) => return Err("--input-control accepts only 'enabled'".into()),
        };
        let capture_control = match capture_control {
            None => false,
            Some("enabled") => true,
            Some(_) => return Err("--capture-control accepts only 'enabled'".into()),
        };
        if width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .is_none_or(|n| n > crate::MAX_UI_FRAME_BYTES)
        {
            return Err("headless output exceeds 32 MiB".into());
        }
        Ok(Self {
            directory,
            width,
            height,
            input_control,
            capture_control,
        })
    }
}

/// Only the new directory and the exact inodes created inside it are ours.
/// Never recursively remove client files or a replacement at the same name.
struct SessionDirectory {
    path: PathBuf,
    identity: Metadata,
    entries: Vec<(PathBuf, Metadata)>,
    cleanup_attempted: bool,
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.file_type() == right.file_type()
}

impl SessionDirectory {
    fn create(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or(USAGE)?
            .canonicalize()
            .map_err(|error| format!("resolve headless parent: {error}"))?;
        let path = parent.join(path.file_name().ok_or(USAGE)?);
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("create new headless session {}: {error}", path.display()))?;
        let identity = fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect new headless session: {error}"))?;
        let owned = Self {
            path,
            identity,
            entries: Vec::new(),
            cleanup_attempted: false,
        };
        fs::set_permissions(&owned.path, Permissions::from_mode(0o700))
            .map_err(|error| format!("chmod headless session: {error}"))?;
        Ok(owned)
    }

    fn record(&mut self, path: &Path) -> Result<(), String> {
        let identity = fs::symlink_metadata(path)
            .map_err(|error| format!("inspect headless endpoint: {error}"))?;
        self.entries.push((path.to_path_buf(), identity));
        Ok(())
    }

    fn bind(&mut self, name: &str) -> Result<UnixListener, String> {
        let path = self.path.join(name);
        let listener =
            UnixListener::bind(&path).map_err(|error| format!("bind headless {name}: {error}"))?;
        self.record(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod headless {name}: {error}"))?;
        Ok(listener)
    }

    fn output_file(&mut self) -> Result<File, String> {
        let path = self.path.join("output.xrgb");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("create headless output: {error}"))?;
        self.record(&path)?;
        fs::remove_file(&path).map_err(|error| format!("unlink headless output: {error}"))?;
        // Keep only the descriptor: capture will be an explicit control grant,
        // not a permanently readable raw output (which could include private UI).
        self.entries.pop();
        Ok(file)
    }

    fn clean(&mut self) -> Result<(), String> {
        if std::mem::replace(&mut self.cleanup_attempted, true) {
            return Ok(());
        }
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(identity) if same_inode(&identity, &self.identity) => {}
            Ok(_) => return Err("headless directory changed; refusing cleanup".into()),
            Err(error) => return Err(format!("inspect headless directory: {error}")),
        }
        let mut failures = Vec::new();
        for (path, identity) in &self.entries {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(current) if same_inode(identity, &current) => {
                    if let Err(error) = fs::remove_file(path) {
                        failures.push(format!(
                            "remove headless endpoint {}: {error}",
                            path.display()
                        ));
                    }
                }
                Ok(_) => failures.push(format!(
                    "headless endpoint changed; preserving {}",
                    path.display()
                )),
                Err(error) => failures.push(format!(
                    "inspect headless endpoint {}: {error}",
                    path.display()
                )),
            }
        }
        if let Err(error) = fs::remove_dir(&self.path) {
            failures.push(format!("remove headless session directory: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

impl Drop for SessionDirectory {
    fn drop(&mut self) {
        if let Err(error) = self.clean() {
            eprintln!("td-compositor: {error}");
        }
    }
}

/// Report an unexpected unwind as well as an ordinary worker return. A
/// sender merely dropping cannot wake recv while another worker is alive.
pub(crate) struct Completion {
    sender: Option<mpsc::Sender<Result<(), String>>>,
    label: &'static str,
}

impl Completion {
    pub(crate) fn new(sender: mpsc::Sender<Result<(), String>>, label: &'static str) -> Self {
        Self {
            sender: Some(sender),
            label,
        }
    }

    pub(crate) fn report(mut self, result: Result<(), String>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(format!(
                "headless {} worker departed without an outcome",
                self.label
            )));
        }
    }
}

fn owner_closed(input: &mut impl Read) -> Result<(), String> {
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err("headless lifetime stdin accepts EOF only, not commands".into()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("headless lifetime stdin: {error}")),
        }
    }
}

/// This is a process entry point, not an embeddable server. Returning retires
/// all blocking client/listener workers through the immediate process exit.
pub(crate) fn run(args: &[String], mut input: impl Read + Send + 'static) -> Result<(), String> {
    let options = Options::parse(args)?;
    let mut directory = SessionDirectory::create(&options.directory)?;
    let result = (|| {
        let framebuffer =
            Framebuffer::headless(directory.output_file()?, options.width, options.height)?;
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint()?;
        let runtime = Arc::new(Mutex::new(runtime));
        let wayland = directory.bind("wayland-0")?;
        let control = directory.bind("td-control")?;
        let (ended, outcome) = mpsc::channel();
        server::serve_headless(
            wayland,
            &directory.path,
            Arc::clone(&runtime),
            ended.clone(),
        )?;
        control::serve_headless(
            control, runtime, options.input_control, options.capture_control, ended.clone(),
        )?;
        let completion = Completion::new(ended, "owner");
        std::thread::Builder::new()
            .name("headless-owner".into())
            .spawn(move || {
                completion.report(owner_closed(&mut input));
            })
            .map_err(|error| format!("start headless owner observer: {error}"))?;
        let mut out = std::io::stdout().lock();
        writeln!(
            out,
            "TD-COMPOSITOR-HEADLESS-READY version=1 width={} height={} scale=1",
            options.width, options.height
        )
        .and_then(|()| out.flush())
        .map_err(|error| format!("announce headless readiness: {error}"))?;
        drop(out);
        outcome
            .recv()
            .map_err(|_| "headless lifecycle observers departed".to_string())?
    })();
    let cleanup = directory.clean();
    match (result, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(width: &str, height: &str) -> Vec<String> {
        [
            "--session-dir",
            "/tmp/td-headless-new",
            "--width",
            width,
            "--height",
            height,
        ]
        .map(String::from)
        .into()
    }

    #[test]
    fn dimensions_are_explicit_and_bounded_before_startup() {
        let options = Options::parse(&args("800", "600")).unwrap();
        assert_eq!((options.width, options.height), (800, 600));
        assert!(Options::parse(&args("3840", "2160")).is_ok());
        for (width, height) in [
            ("0", "1"),
            ("1", "0"),
            ("16385", "1"),
            ("4096", "2160"),
            ("x", "1"),
            ("18446744073709551616", "1"),
        ] {
            assert!(Options::parse(&args(width, height)).is_err());
        }
        assert!(Options::parse(&[]).is_err());
        for word in [
            "--input",
            "--terminal-authority",
            "--control-socket",
            "--session-dir",
        ] {
            let mut values = args("800", "600");
            if let Some(flag) = values.get_mut(2) {
                *flag = word.into();
            }
            assert!(Options::parse(&values).is_err());
        }
    }

    #[test]
    fn keyboard_control_requires_one_explicit_enable_pair() {
        assert!(!Options::parse(&args("800", "600")).unwrap().input_control);
        let mut values = args("800", "600");
        values.extend(["--input-control".into(), "enabled".into()]);
        assert!(Options::parse(&values).unwrap().input_control);
        *values.last_mut().unwrap() = "true".into();
        assert!(Options::parse(&values).is_err());
        *values.last_mut().unwrap() = "enabled".into();
        values.extend(["--input-control".into(), "enabled".into()]);
        assert!(Options::parse(&values).is_err());
    }

    #[test]
    fn capture_is_a_separate_explicit_grant() {
        let mut values = args("800", "600");
        assert!(!Options::parse(&values).unwrap().capture_control);
        values.extend(["--capture-control".into(), "enabled".into()]);
        let options = Options::parse(&values).unwrap();
        assert!(options.capture_control);
        assert!(!options.input_control);
        values.extend(["--input-control".into(), "enabled".into()]);
        let options = Options::parse(&values).unwrap();
        assert!(options.capture_control && options.input_control);
        values.pop();
        assert!(Options::parse(&values).is_err());
        let mut values = args("800", "600");
        values.extend(["--capture-control".into(), "true".into()]);
        assert!(Options::parse(&values).is_err());
    }

    #[test]
    fn lifetime_is_eof_not_an_unbounded_command_stream() {
        assert!(owner_closed(&mut &b""[..]).is_ok());
        assert!(owner_closed(&mut &b"quit\n"[..]).is_err());
        struct Fault(std::io::ErrorKind);
        impl Read for Fault {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                let kind = std::mem::replace(&mut self.0, std::io::ErrorKind::BrokenPipe);
                Err(kind.into())
            }
        }
        let error = owner_closed(&mut Fault(std::io::ErrorKind::Interrupted)).unwrap_err();
        assert!(error.contains("headless lifetime stdin"));
        assert!(error.contains("broken pipe"));
    }

    #[test]
    fn a_departing_worker_reports_even_when_another_sender_survives() {
        let (sender, receiver) = mpsc::channel();
        drop(Completion::new(sender.clone(), "fixture"));
        assert_eq!(
            receiver.try_recv().unwrap(),
            Err("headless fixture worker departed without an outcome".into())
        );
        Completion::new(sender.clone(), "normal").report(Ok(()));
        assert_eq!(receiver.try_recv().unwrap(), Ok(()));
        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    }

    #[test]
    fn headless_entry_has_no_hardware_or_trusted_input_startup() {
        let source = include_str!("headless.rs")
            .split_once("\n#[cfg(test)]\nmod tests {")
            .unwrap()
            .0;
        for forbidden in [
            "input::",
            "authority::",
            "vm_bridge::",
            "bar::",
            "enable_attention",
            "Command::",
            "Framebuffer::open",
        ] {
            assert!(!source.contains(forbidden), "{forbidden}");
        }
        assert!(source.contains("server::serve_headless("));
        assert!(source.contains("control::serve_headless("));
        let paint = source.find("runtime.repaint()?").unwrap();
        let bind = source.find("directory.bind(\"wayland-0\")?").unwrap();
        let serve = source.find("server::serve_headless(").unwrap();
        let ready = source
            .find("TD-COMPOSITOR-HEADLESS-READY version=1")
            .unwrap();
        assert!(paint < bind && bind < serve && serve < ready);
    }
}
