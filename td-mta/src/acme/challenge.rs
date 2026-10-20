//! Dedicated HTTP-01 responder: exact host/token, one bounded connection at a time.
use super::Error;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        RwLock,
    },
    time::{Duration, Instant},
};
pub struct Current {
    pub names: Vec<String>,
    pub token: String,
    pub authorization: String,
    pub expires: Instant,
}
pub fn run(
    listener: TcpListener,
    current: &RwLock<Current>,
    stop: &AtomicBool,
) -> Result<(), Error> {
    listener
        .set_nonblocking(true)
        .map_err(|_| Error::Transport)?;
    let mut window = Instant::now();
    let mut used = 0;
    while !stop.load(Ordering::Acquire) {
        if window.elapsed() >= Duration::from_secs(60) {
            window = Instant::now();
            used = 0;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if used >= 120 {
                    drop(stream);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                used += 1;
                let _ = respond(stream, current, stop);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => return Err(Error::Transport),
        }
    }
    Ok(())
}
fn respond(
    mut stream: TcpStream,
    current: &RwLock<Current>,
    stop: &AtomicBool,
) -> Result<(), Error> {
    stream.set_nonblocking(true).map_err(|_| Error::Transport)?;
    let until = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::with_capacity(4096);
    let mut buffer = [0; 512];
    while !bytes.windows(4).any(|b| b == b"\r\n\r\n") {
        if Instant::now() >= until || stop.load(Ordering::Acquire) {
            return Err(Error::Deadline);
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err(Error::Transport),
            Ok(n) => {
                if bytes.len() + n > 4096 {
                    return Err(Error::Capacity);
                }
                bytes.extend_from_slice(buffer.get(..n).ok_or(Error::Invalid)?);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(_) => return Err(Error::Transport),
        }
    }
    let body = {
        let current = current.read().map_err(|_| Error::Conflict)?;
        answer(&bytes, &current, Instant::now()).unwrap_or_default()
    };
    let status = if body.is_empty() {
        "404 Not Found"
    } else {
        "200 OK"
    };
    let response=format!("HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",body.len());
    let mut bytes = response.as_bytes();
    while !bytes.is_empty() {
        if Instant::now() >= until || stop.load(Ordering::Acquire) {
            return Err(Error::Deadline);
        }
        match stream.write(bytes) {
            Ok(0) => return Err(Error::Transport),
            Ok(n) => bytes = bytes.get(n..).ok_or(Error::Invalid)?,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(_) => return Err(Error::Transport),
        }
    }
    Ok(())
}
fn answer(request: &[u8], current: &Current, now: Instant) -> Option<String> {
    if now >= current.expires || current.token.is_empty() || current.authorization.is_empty() {
        return None;
    }
    let request = std::str::from_utf8(request).ok()?;
    let mut lines = request.split("\r\n");
    let line = lines.next()?;
    let target = line.strip_prefix("GET ")?.strip_suffix(" HTTP/1.1")?;
    if target.strip_prefix("/.well-known/acme-challenge/")? != current.token {
        return None;
    }
    let mut host = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return None;
            }
            host = Some(value.trim_matches([' ', '\t']));
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
        {
            return None;
        }
    }
    let host = host?;
    let host = host.strip_suffix(":80").unwrap_or(host);
    if !current
        .names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(host))
    {
        return None;
    }
    Some(current.authorization.clone())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn challenge_is_exact_and_expires() {
        let now = Instant::now();
        let current = Current {
            names: vec!["mail.example.test".into()],
            token: "abc".into(),
            authorization: "abc.key".into(),
            expires: now + Duration::from_secs(1),
        };
        let good =
            b"GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\n\r\n";
        assert_eq!(answer(good, &current, now), Some("abc.key".into()));
        assert!(answer(good, &current, current.expires).is_none());
        for bad in ["GET /.well-known/acme-challenge/abc?x HTTP/1.1\r\nHost: mail.example.test\r\n\r\n","GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: evil.test\r\n\r\n","GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\nHost: mail.example.test\r\n\r\n"] {assert!(answer(bad.as_bytes(),&current,now).is_none());}
    }
}
