//! Human-side submission of one immutable credential descriptor.

use crate::consent::Role;
use crate::secret_request::{self, Credential, Target, ADMITTED, GREETING, MAX_SECRET, SOCKET};
use crate::secret_sys;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(crate) fn set(target: &str, role: Role) -> Result<(), String> {
    let target = Target::parse(target, role)?;
    secret_request::require_protected_memory()?;
    if io::stdin().is_terminal() {
        return Err("read credential bytes from a pipe or redirected file".into());
    }
    // The root listener authenticates the actual sender on each fragment.
    // Checking the server precedes reading input or sending credential bytes.
    let mut stream =
        UnixStream::connect(SOCKET).map_err(|e| format!("connect credential authority: {e}"))?;
    if secret_sys::peer_uid(&stream).map_err(|e| e.to_string())? != 0 {
        return Err("credential authority is not root".into());
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut greeting = [0; 8];
    stream.read_exact(&mut greeting).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            "credential authority did not admit the request; it may be busy or unavailable".into()
        } else {
            format!("credential authority greeting: {e}")
        }
    })?;
    if greeting != *GREETING {
        return Err("invalid credential authority greeting".into());
    }
    let mut credential = Credential(Vec::with_capacity(MAX_SECRET + 1));
    io::stdin()
        .take((MAX_SECRET + 1) as u64)
        .read_to_end(&mut credential.0)
        .map_err(|e| format!("read credential input: {e}"))?;
    if credential.0.is_empty() || credential.0.len() > MAX_SECRET {
        return Err("credential must contain 1 through 4096 bytes".into());
    }
    let mut file = secret_sys::create_credential().map_err(|e| e.to_string())?;
    file.write_all(&credential.0).map_err(|e| e.to_string())?;
    secret_sys::seal_credential(&file).map_err(|e| e.to_string())?;
    drop(credential);
    let description = target.encode();
    let mut frame = (description.len() as u16).to_be_bytes().to_vec();
    frame.extend_from_slice(&description);
    secret_sys::send_descriptor(&stream, &frame, &file).map_err(|e| e.to_string())?;
    drop(file);
    await_admission(
        &mut stream,
        Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or("credential admission deadline overflow")?,
    )?;
    eprintln!("td-secret: press Ctrl+Alt+Esc, then W, and check the credential target before touching the token");
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(185))
        .ok_or("credential result deadline overflow")?;
    let mut result = [0];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or("credential result expired; outcome unknown; do not automatically retry")?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        match stream.read(&mut result) {
            Ok(1) if result == [1] => {
                eprintln!("td-secret: credential stored");
                return Ok(());
            }
            Ok(1) if result == [0] => return Err(
                "credential operation did not complete successfully; do not automatically retry"
                    .into(),
            ),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            _ => {
                return Err(
                    "credential result unavailable; outcome unknown; do not automatically retry"
                        .into(),
                )
            }
        }
    }
}

fn await_admission(stream: &mut UnixStream, deadline: Instant) -> Result<(), String> {
    let mut reply = [0];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or("credential admission expired; request was not confirmed ready")?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        match stream.read(&mut reply) {
            Ok(1) if reply == [ADMITTED] => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            _ => return Err("credential request was not confirmed ready; authority refused or became unavailable".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_admission_reply_allows_the_attention_instruction() {
        for bytes in [&[ADMITTED][..], &[0], &[1], &[3], &[]] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            server.write_all(bytes).unwrap();
            drop(server);
            assert_eq!(
                await_admission(&mut client, Instant::now() + Duration::from_secs(1)).is_ok(),
                bytes == [ADMITTED]
            );
        }
        let (mut client, _server) = UnixStream::pair().unwrap();
        assert!(await_admission(&mut client, Instant::now() + Duration::from_millis(20)).is_err());
    }
}
