//! Live development-profile boundary checks. TMPDIR must be outside /tmp,
//! which workspace admission reserves, for example this worktree's scratch.
#![forbid(unsafe_code)]

use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires user namespaces and TMPDIR inside the worktree scratch"]
fn development_shares_host_network_and_hides_ungranted_files() -> Result<(), Box<dyn Error>> {
    let owned = td_fs::TempDir::new("development")?;
    let base = fs::canonicalize(owned.path())?;
    for name in ["home", "tree"] {
        fs::create_dir(base.join(name))?;
    }
    fs::set_permissions(base.join("home"), fs::Permissions::from_mode(0o700))?;
    let secret = base.join("ungranted");
    fs::write(&secret, "host-only")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    fs::write(
        base.join("tree/inputs"),
        format!(
            "{}\n{}\n{}\n",
            listener.local_addr()?,
            fs::read_link("/proc/self/ns/net")?.display(),
            secret.display()
        ),
    )?;
    let spec = base.join("spec");
    fs::write(
        &spec,
        format!(
            "format=1\nprofile=development\nentry={}\nhome={}\nworktree={}\n",
            fs::canonicalize(std::env::current_exe()?)?.display(),
            base.join("home").display(),
            base.join("tree").display()
        ),
    )?;
    fs::set_permissions(&spec, fs::Permissions::from_mode(0o600))?;
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => return stream.write_all(b"host-reached"),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        }
    });
    let (mut channel, entry) = UnixStream::pair()?;
    channel.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .args(["--workspace", &std::process::id().to_string()])
        .arg(&spec)
        .args(["--exact", "development_probe", "--ignored", "--nocapture"])
        .stdin(Stdio::from(OwnedFd::from(entry.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(entry)))
        .stderr(Stdio::piped())
        .spawn()?;
    let mut reply = String::new();
    let read = channel.read_to_string(&mut reply);
    if read.is_err() {
        let _ = child.kill();
    }
    let output = child.wait_with_output()?;
    let served = server.join().map_err(|_| "host listener panicked")?;
    assert!(
        output.status.success(),
        "{reply}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    read?;
    served?;
    assert!(reply.contains("development-boundary=passed"), "{reply}");
    Ok(())
}

#[test]
#[ignore = "invoked only by the development jail fixture"]
fn development_probe() -> Result<(), Box<dyn Error>> {
    if !std::env::current_exe()?.starts_with("/opt/workspace/bin") {
        return Ok(());
    }
    let input = fs::read_to_string("inputs")?;
    let mut lines = input.lines();
    let address = lines.next().ok_or("missing host address")?;
    let network = lines.next().ok_or("missing host network")?;
    let secret = lines.next().ok_or("missing host secret path")?;
    assert_eq!(
        fs::read_link("/proc/self/ns/net")?.to_string_lossy(),
        network
    );
    assert!(!Path::new(secret).exists());
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    assert_eq!(response, "host-reached");
    let unix = UnixListener::bind("local.sock")?;
    let client = UnixStream::connect("local.sock")?;
    let (_accepted, _) = unix.accept()?;
    drop(client);
    let (_a, _b) = UnixDatagram::pair()?;
    assert!(fs::metadata("/etc/resolv.conf")?.is_file());
    assert!(fs::write("/etc/resolv.conf", "changed").is_err());
    println!("\ndevelopment-boundary=passed");
    Ok(())
}
