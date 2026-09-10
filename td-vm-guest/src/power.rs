//! Fixed compositor-to-supervisor poweroff handoff; no user commands.
use crate::{create_directory, directory, io, vm_wire as wire, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn request(path: &Path, owner: u32) -> Result<(u64, u64)> {
    directory(
        path.parent().ok_or("power request has no parent")?,
        owner,
        false,
    )?;
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(0x20000 | 0x800)
            .open(path),
        "open power request",
    )?;
    let meta = io(file.metadata(), "inspect power request")?;
    if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o022 != 0 || meta.nlink() != 1 {
        return Err("untrusted power request".into());
    }
    let mut bytes = Vec::new();
    io(
        file.take(wire::POWER_RECORD.len() as u64 + 1)
            .read_to_end(&mut bytes),
        "read power request",
    )?;
    if bytes != wire::POWER_RECORD {
        return Err("invalid power request".into());
    }
    Ok((meta.dev(), meta.ino()))
}

fn poweroff(tool: &Path, lease: &File, timeout: Duration) -> Result<()> {
    let (mut stream, output) = io(UnixStream::pair(), "create power reply channel")?;
    io(stream.set_nonblocking(true), "bound power reply")?;
    // Connect runs in the fixed client, so even a full supervisor backlog is
    // covered by the parent deadline without adding a socket syscall surface.
    let mut child = io(
        Command::new(tool)
            .arg("poweroff")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(io(
                lease.try_clone(),
                "retain power worker lease",
            )?))
            .stdout(Stdio::from(OwnedFd::from(output)))
            .stderr(Stdio::null())
            .spawn(),
        "start poweroff client",
    )?;
    let result = (|| {
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut buffer = [0; 128];
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err("guest supervisor poweroff timed out".into());
            }
            match stream.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    bytes.extend_from_slice(buffer.get(..n).ok_or("invalid power reply length")?);
                    if bytes.len() > 128 {
                        return Err("guest supervisor poweroff reply exceeds limit".into());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(format!("read guest poweroff reply: {e}")),
            }
            if let Some(status) = io(child.try_wait(), "observe poweroff client")? {
                if !status.success() {
                    return Err("guest supervisor poweroff client failed".into());
                }
                if eof {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        match bytes.as_slice() {
            b"poweroff requested\n" | b"shutdown already in progress (poweroff)\n" => Ok(()),
            _ => Err("guest supervisor did not confirm poweroff".into()),
        }
    })();
    if result.is_err() {
        // This is our recorded Child, never a process selected by command text.
        let _ = child.kill();
        io(child.wait(), "reap poweroff client")?;
    }
    result
}

fn carrier_present(root: &Path) -> Result<bool> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("inspect VM carriers: {e}")),
    };
    for (index, entry) in entries.enumerate() {
        if index >= 64 {
            return Err("too many VM carriers".into());
        }
        let path = io(entry, "inspect VM carrier")?.path().join("name");
        let mut value = String::new();
        match File::open(path) {
            Ok(file) => file,
            // Unnamed ports have no name attribute; discovery can also race
            // a kernel port removal. Neither grants power authority.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("open kernel VM carrier name: {e}")),
        }
        .take(129)
        .read_to_string(&mut value)
        .map_err(|e| format!("read VM carrier name: {e}"))?;
        if value == format!("{}\n", wire::PORT) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn serve() -> Result<()> {
    if io(fs::metadata("/proc/self"), "inspect power helper identity")?.uid() != 0 {
        return Err("power-serve requires the root service identity".into());
    }
    directory(Path::new("/run"), 0, false)?;
    let state = Path::new("/run/td-vm-power");
    create_directory(state, 0)?;
    let lease: File = io(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(0x20000 | 0x800)
            .open(state.join("lock")),
        "open power worker lease",
    )?;
    lease
        .try_lock()
        .map_err(|e| format!("acquire power worker lease: {e}"))?;
    let path = Path::new(wire::POWER_REQUEST);
    let mut attempted = None;
    let mut last_error = String::new();
    loop {
        let result = (|| {
            if !carrier_present(Path::new("/sys/class/virtio-ports"))? {
                return Ok(());
            }
            directory(Path::new("/run/td-compositor"), 0, false)?;
            if matches!(fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(());
            }
            let stamp = request(path, 993)?;
            if attempted == Some(stamp) {
                return Ok(());
            }
            // An explicit host retry replaces the request inode. Never turn a
            // transport failure into continuous shutdown requests.
            attempted = Some(stamp);
            directory(Path::new("/run/td-svc"), 0, true)?;
            poweroff(Path::new("/bin/td-svc"), &lease, Duration::from_secs(3))
        })();
        if let Err(error) = result {
            if error != last_error {
                eprintln!("td-vm-power: {error}");
                last_error = error;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    };

    #[test]
    fn power_authority_requires_the_kernel_named_vm_carrier() {
        let root = std::env::temp_dir().join(format!("td-power-carrier-{}", std::process::id()));
        assert!(!carrier_present(&root).unwrap());
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("vport0p1")).unwrap();
        assert!(!carrier_present(&root).unwrap());
        fs::write(root.join("vport0p1/name"), "unrelated\n").unwrap();
        assert!(!carrier_present(&root).unwrap());
        fs::write(root.join("vport0p1/name"), format!("{}\n", wire::PORT)).unwrap();
        assert!(carrier_present(&root).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn power_request_refuses_aliases_permissions_and_extra_data() {
        let temp = std::env::temp_dir().join(format!("td-power-request-{}", std::process::id()));
        fs::create_dir(&temp).unwrap();
        let uid = fs::metadata(&temp).unwrap().uid();
        let path = temp.join("request");
        fs::write(&path, wire::POWER_RECORD).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let first = request(&path, uid).unwrap();
        assert!(request(&path, uid + 1).is_err());
        let alias = temp.join("alias");
        symlink(&path, &alias).unwrap();
        assert!(request(&alias, uid).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        assert!(request(&path, uid).is_err());
        fs::remove_file(&alias).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(request(&path, uid).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&alias, wire::POWER_RECORD).unwrap();
        fs::rename(&alias, &path).unwrap();
        assert_ne!(request(&path, uid).unwrap(), first);
        fs::write(&path, b"TDVM-POWEROFF-1\nreboot\n").unwrap();
        assert!(request(&path, uid).is_err());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    #[ignore = "requires host rustc and its linker"]
    fn handoff_pins_argv_and_bounds_reply_exit_and_blocked_connect() {
        let root = std::env::temp_dir().join(format!("td-power-child-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let source = root.join("fixture.rs");
        fs::write(&source, format!(r#"
use std::{{fs,env,io::Write,os::unix::net::UnixStream,path::Path}};
fn main() {{
 let args:Vec<_>=env::args().collect();assert_eq!(args.len(),2);assert_eq!(args[1],"poweroff");
 assert!(env::var_os("TD_POWER_AMBIENT_TEST").is_none());
 fs::write({pid:?},std::process::id().to_string()).unwrap();
 let name=Path::new(&args[0]).file_name().unwrap().to_str().unwrap();
 match name {{
  "ok" => print!("poweroff requested\n"),
  "already" => print!("shutdown already in progress (poweroff)\n"),
  "wrong" => print!("shutdown already in progress (reboot)\n"),
  "large" => print!("{{}}","x".repeat(256)),
  "exit" => std::process::exit(1),
  "held" => {{ print!("poweroff requested\n");std::io::stdout().flush().unwrap();std::thread::sleep(std::time::Duration::from_secs(60)); }},
  "backlog" => {{ let mut sockets=Vec::new();for _ in 0..10000 {{ sockets.push(UnixStream::connect({socket:?}).unwrap()); }} }},
  _ => panic!("bad fixture"),
 }}
}}
"#, pid=root.join("pid"), socket=root.join("control"))).unwrap();
        assert!(Command::new("rustc")
            .args(["-C", "linker=gcc"])
            .arg(&source)
            .arg("-o")
            .arg(root.join("ok"))
            .status()
            .unwrap()
            .success());
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.join("lock"))
            .unwrap();
        lease.lock().unwrap();
        let listener = UnixListener::bind(root.join("control")).unwrap();
        for (name, pass) in [
            ("ok", true),
            ("already", true),
            ("wrong", false),
            ("large", false),
            ("exit", false),
            ("held", false),
            ("backlog", false),
        ] {
            if name != "ok" {
                fs::hard_link(root.join("ok"), root.join(name)).unwrap();
            }
            let start = Instant::now();
            let result = poweroff(&root.join(name), &lease, Duration::from_millis(300));
            assert_eq!(result.is_ok(), pass, "{name}: {result:?}");
            assert!(start.elapsed() < Duration::from_secs(3));
            let pid = fs::read_to_string(root.join("pid")).unwrap();
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "client must be reaped"
            );
        }
        drop(listener);
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
}
