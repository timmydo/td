//! `td-install serve` refuses a misplaced start before sending a byte.
//!
//! A SUBPROCESS test, because admission reads the real process's effective
//! uid and descriptors 0 and 1; the unit tests inject all three.

use std::error::Error;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};

type Res<T> = Result<T, Box<dyn Error>>;

const BIN: &str = env!("CARGO_BIN_EXE_td-install");

/// Run `serve` with `stdin` and `stdout`; return its result and every byte
/// it sent back on `peer`, once its side of the channel is closed.
fn serve(
    operands: &[String],
    stdin: Stdio,
    stdout: Stdio,
    peer: Option<&mut UnixStream>,
) -> Res<(Output, Vec<u8>)> {
    let output = {
        let child = Command::new(BIN)
            .arg("serve")
            .args(operands)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(peer) = &peer {
            // A service that greets then waits for ours sees the channel close.
            peer.shutdown(std::net::Shutdown::Write)?;
        }
        child.wait_with_output()?
    };
    let mut sent = Vec::new();
    if let Some(peer) = peer {
        peer.read_to_end(&mut sent)?;
    }
    Ok((output, sent))
}

#[test]
fn a_misplaced_serve_exits_without_a_byte() -> Res<()> {
    // Absolute and of the right kind, so only the check under test refuses.
    let exe = std::env::current_exe()?;
    let file = exe.display().to_string();
    let directory = exe.parent().ok_or("test binary has no parent")?;
    let directory = directory.display().to_string();
    let operands = vec![
        file.clone(),
        directory.clone(),
        file.clone(),
        directory.clone(),
        file.clone(),
    ];

    let mut misplaced = operands.clone();
    misplaced[4] = directory.clone();
    let (output, _) = serve(&misplaced, Stdio::null(), Stdio::null(), None)?;
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&format!("{directory} is not an executable file")),
        "{output:?}"
    );

    let mut missing = operands.clone();
    missing[0] = "/nonexistent/td-boot".into();
    let (mut ours, theirs) = UnixStream::pair()?;
    let (output, sent) = serve(
        &missing,
        Stdio::from(OwnedFd::from(theirs)),
        Stdio::null(),
        Some(&mut ours),
    )?;
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("/nonexistent/td-boot is not present"));
    assert!(sent.is_empty());

    let root = std::fs::metadata("/proc/self")?.uid() == 0;
    let (output, _) = serve(&operands, Stdio::null(), Stdio::null(), None)?;
    assert!(!output.status.success(), "{output:?}");
    let expected = if root {
        "serve requires its installer channel on stdin"
    } else {
        "serve requires the installation authority"
    };
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "{output:?}"
    );

    // A root start with no consent channel is refused before a byte.
    let (mut ours, theirs) = UnixStream::pair()?;
    let (output, sent) = serve(
        &operands,
        Stdio::from(OwnedFd::from(theirs)),
        Stdio::null(),
        Some(&mut ours),
    )?;
    assert!(!output.status.success(), "{output:?}");
    assert!(sent.is_empty(), "{sent:?}");
    let expected = if root {
        "serve requires its consent channel on stdout"
    } else {
        "serve requires the installation authority"
    };
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "{output:?}"
    );

    let (mut ours, theirs) = UnixStream::pair()?;
    let (_authority, consent) = UnixStream::pair()?;
    let (output, sent) = serve(
        &operands,
        Stdio::from(OwnedFd::from(theirs)),
        Stdio::from(OwnedFd::from(consent)),
        Some(&mut ours),
    )?;
    assert!(!output.status.success(), "{output:?}");
    if root {
        // Admitted: it greets, then ends when the installer closes unanswered.
        assert_eq!(sent, b"TDINS02\n");
    } else {
        assert!(sent.is_empty(), "{sent:?}");
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("serve requires the installation authority"));
    }
    Ok(())
}
