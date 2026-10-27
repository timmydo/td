//! Cloud metadata a server reads once its network is up (td-svc/DESIGN.md
//! §10, "Cloud metadata"): EC2's IMDSv2, recognised by DMI, for the same
//! hostname and Ed25519 keys a NoCloud seed may offer.

use std::net::SocketAddrV4;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::http;
use crate::metadata::{self, Request};

pub(crate) const DMI: &str = "/sys/class/dmi/id";
/// The link-local metadata address every cloud td recognises serves.
pub(crate) const IMDS: SocketAddrV4 =
    SocketAddrV4::new(std::net::Ipv4Addr::new(169, 254, 169, 254), 80);
/// One attempt at one request.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Every request of a fetch, retried until then: long enough for the
/// network daemon to take a lease netup's DHCP gave up on.
const BUDGET: Duration = Duration::from_secs(150);
const RETRY: Duration = Duration::from_secs(1);
/// The least time the token's GETs have, however late it was issued.
const AFTER_TOKEN: Duration = Duration::from_secs(30);
const MAX_TOKEN: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cloud {
    Ec2,
}

impl Cloud {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Cloud::Ec2 => "ec2",
        }
    }
}

/// The cloud the firmware's DMI names, read from `dmi`: EC2's Nitro
/// instances call their system vendor `Amazon EC2`.
pub(crate) fn detect(dmi: &Path) -> Option<Cloud> {
    let field = |name: &str| {
        std::fs::read_to_string(dmi.join(name))
            .ok()
            .map(|text| text.trim().to_owned())
    };
    (field("sys_vendor").as_deref() == Some("Amazon EC2")).then_some(Cloud::Ec2)
}

/// What `cloud`'s metadata service at `address` asks for.
pub(crate) fn fetch(cloud: Cloud, address: SocketAddrV4) -> Result<Request, String> {
    match cloud {
        Cloud::Ec2 => ec2(address, Instant::now() + BUDGET),
    }
}

/// One request, retried until `deadline` while the service cannot be
/// reached, fails (5xx) or throttles (429).
fn ask(
    address: SocketAddrV4,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    deadline: Instant,
) -> Result<http::Response, String> {
    let mut last = format!("{method} {path}: no time left to ask");
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(last);
        }
        match http::request(address, method, path, headers, b"", left.min(TIMEOUT)) {
            Ok(response) if response.status == 429 || response.status >= 500 => {
                last = format!("{method} {path}: HTTP {}", response.status);
            }
            Ok(response) => return Ok(response),
            Err(error) => last = error,
        }
        std::thread::sleep(RETRY.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// IMDSv2: a session token from a PUT, then token-bearing GETs; IMDSv1's
/// tokenless GETs are never made.
fn ec2(address: SocketAddrV4, deadline: Instant) -> Result<Request, String> {
    let token = ask(
        address,
        "PUT",
        "/latest/api/token",
        &[("X-aws-ec2-metadata-token-ttl-seconds", "300")],
        deadline,
    )?;
    let token = match token.text_or_absent("the IMDSv2 token")? {
        Some(token)
            if !token.is_empty()
                && token.len() <= MAX_TOKEN
                && token.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            token
        }
        _ => return Err("IMDSv2 issued no usable token".into()),
    };
    let deadline = deadline.max(Instant::now() + AFTER_TOKEN);
    let get = |path: &str| {
        ask(
            address,
            "GET",
            path,
            &[("X-aws-ec2-metadata-token", &token)],
            deadline,
        )?
        .text_or_absent(path)
    };
    let hostname = get("/latest/meta-data/local-hostname")?
        .map(|name| name.trim().to_owned())
        .unwrap_or_default();
    let mut offered = Vec::new();
    let mut skipped = Vec::new();
    let listing = get("/latest/meta-data/public-keys/")?.unwrap_or_default();
    let indices = key_indices(&listing);
    if indices.len() > metadata::MAX_KEYS {
        skipped.push(format!(
            "{} key pair(s) past the first {}",
            indices.len() - metadata::MAX_KEYS,
            metadata::MAX_KEYS
        ));
    }
    for index in indices.iter().take(metadata::MAX_KEYS) {
        if let Some(key) = get(&format!(
            "/latest/meta-data/public-keys/{index}/openssh-key"
        ))? {
            offered.extend(
                key.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_owned),
            );
        }
    }
    Ok(metadata::admit(
        &[("EC2 local-hostname", hostname)],
        offered,
        skipped,
    ))
}

/// The indices of `public-keys/`'s `N=name` lines, in order.
fn key_indices(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split_once('=').map(|(index, _)| index.trim()))
        .filter(|index| {
            !index.is_empty() && index.len() <= 4 && index.bytes().all(|b| b.is_ascii_digit())
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgUi";

    #[test]
    fn ec2_is_its_dmi_system_vendor() {
        let dir = td_fs::TempDir::new("td-firstboot-dmi").unwrap();
        assert_eq!(detect(dir.path()), None);
        std::fs::write(dir.path().join("sys_vendor"), "Amazon EC2\n").unwrap();
        assert_eq!(detect(dir.path()), Some(Cloud::Ec2));
        std::fs::write(dir.path().join("sys_vendor"), "QEMU\n").unwrap();
        assert_eq!(detect(dir.path()), None);
    }

    #[test]
    fn key_pairs_are_listed_by_index() {
        assert_eq!(
            key_indices("0=my-key\n1=other\nbad\n=x\n12345=long\n2=last"),
            ["0", "1", "2"]
        );
        assert!(key_indices("").is_empty());
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(20)
    }

    /// A loopback IMDSv2 that refuses any GET without its token, and is
    /// unavailable for its first request.
    fn serve(responses: usize) -> (SocketAddrV4, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            panic!("an IPv4 listener has an IPv4 address");
        };
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap();
                let line = request.lines().next().unwrap().to_owned();
                let authorized = request.contains("\r\nX-aws-ec2-metadata-token: t0k\r\n");
                let (status, body) = match line.as_str() {
                    _ if seen.is_empty() => ("503 Service Unavailable", String::new()),
                    "PUT /latest/api/token HTTP/1.1"
                        if request.contains("X-aws-ec2-metadata-token-ttl-seconds: ") =>
                    {
                        ("200 OK", "t0k".to_owned())
                    }
                    _ if !authorized => ("401 Unauthorized", String::new()),
                    "GET /latest/meta-data/local-hostname HTTP/1.1" => {
                        ("200 OK", "ip-10-0-0-5.ec2.internal".to_owned())
                    }
                    "GET /latest/meta-data/public-keys/ HTTP/1.1" => {
                        ("200 OK", "0=mine\n1=rsa".to_owned())
                    }
                    "GET /latest/meta-data/public-keys/0/openssh-key HTTP/1.1" => {
                        ("200 OK", format!("{KEY} mine\n"))
                    }
                    "GET /latest/meta-data/public-keys/1/openssh-key HTTP/1.1" => {
                        ("200 OK", "ssh-rsa AAAAB3NzaC1yc2E= rsa\n".to_owned())
                    }
                    _ => ("404 Not Found", String::new()),
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                seen.push(line);
            }
            seen
        });
        (address, server)
    }

    #[test]
    fn imdsv2_gives_the_hostname_and_every_key_pair() {
        let (address, server) = serve(6);
        let asked = ec2(address, soon()).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "ip-10-0-0-5");
        assert_eq!(asked.keys, [format!("{KEY} mine")]);
        assert_eq!(asked.skipped, ["1 key(s) other than plain ssh-ed25519"]);
        let seen = server.join().unwrap();
        // The unavailable first answer was retried.
        assert_eq!(seen[..2], ["PUT /latest/api/token HTTP/1.1"; 2]);
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn a_service_that_stays_unavailable_fails_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            panic!("an IPv4 listener has an IPv4 address");
        };
        let asked = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&asked);
        // Left blocked in accept when the test ends.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                }
                stream
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
        let started = Instant::now();
        let error = ec2(address, started + Duration::from_millis(2500)).unwrap_err();
        assert!(error.contains("HTTP 503"), "{error}");
        assert!(asked.load(Ordering::SeqCst) >= 2);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
