//! A mock cloud metadata service for `qemu-boot-server` (td-svc/DESIGN.md
//! §10, "Cloud metadata"). QEMU's user network runs `td-recipe-eval
//! cloud-mock ec2 DIR` once for each guest connection to 169.254.169.254:80
//! (`guestfwd=...-cmd:`), with the connection as its stdin, stdout and
//! stderr. It answers that one request as EC2's IMDSv2 does, refusing a GET
//! without the session token, and logs it, or why it could not answer, to
//! `DIR/requests.log` for the oracle.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The session token the mock issues and then requires.
const TOKEN: &str = "td-oracle-imds-token";
/// A full name, so the boot shows only its first label is taken.
pub(super) const HOSTNAME: &str = "td-ec2-host.ec2.internal";
pub(super) const HOSTNAME_LABEL: &str = "td-ec2-host";
/// A user network that holds 169.254.169.254, as libslirp requires of a
/// guestfwd address, and Azure's 168.63.129.16.
const NETWORK: &str = "net=168.0.0.0/7,host=168.0.0.2,dns=168.0.0.3,dhcpstart=168.0.0.15";
const KEY_FILE: &str = "openssh-key";
const LOG: &str = "requests.log";
const MAX_HEAD: usize = 16 * 1024;

/// What the guest must ask, in order: a token, then each path with it.
pub(super) const EXPECTED: &[&str] = &[
    "PUT /latest/api/token ttl",
    "GET /latest/meta-data/local-hostname token",
    "GET /latest/meta-data/public-keys/ token",
    "GET /latest/meta-data/public-keys/0/openssh-key token",
];

pub(crate) struct CloudMock {
    dir: PathBuf,
    exe: PathBuf,
}

impl CloudMock {
    /// An EC2 service offering `key` as the instance's one key pair, kept
    /// under `dir`, which must not exist.
    pub(crate) fn ec2(dir: &Path, key: &str) -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("locate td-recipe-eval for the mock IMDS: {error}"))?;
        for path in [dir, exe.as_path()] {
            if path
                .to_str()
                .is_none_or(|text| text.contains([',', ' ', '\t', '\n', '\'', '"', '\\']))
            {
                return Err(format!(
                    "{} must be UTF-8 without commas, whitespace, quotes or \
                     backslashes: QEMU's netdev options split on commas, and \
                     libslirp parses its command line as a shell would",
                    path.display()
                ));
            }
        }
        fs::create_dir(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
        fs::write(dir.join(KEY_FILE), format!("{key}\n"))
            .map_err(|error| format!("write the mock's key: {error}"))?;
        Ok(Self {
            dir: dir.to_owned(),
            exe,
        })
    }

    /// The options a boot's `-netdev user` gains: the network, and the
    /// metadata address run as this mock.
    pub(crate) fn netdev_options(&self) -> String {
        format!(
            ",{NETWORK},guestfwd=tcp:169.254.169.254:80-cmd:{} cloud-mock ec2 {}",
            self.exe.display(),
            self.dir.display()
        )
    }

    /// The firmware identity EC2's Nitro instances carry.
    pub(crate) fn smbios(&self) -> [&'static str; 2] {
        ["-smbios", "type=1,manufacturer=Amazon EC2"]
    }

    /// Every request the guest made, in order, a retried one once: the
    /// guest retries a request whose answer is slow, and this mock is a
    /// process started for each.
    pub(super) fn requests(&self) -> Result<Vec<String>, String> {
        let path = self.dir.join(LOG);
        let mut requests: Vec<String> = match fs::read_to_string(&path) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        requests.dedup();
        Ok(requests)
    }
}

/// `cloud-mock ec2 DIR`: one request on stdin, its answer on stdout. What
/// stops it is logged and answered as a 500, since its stderr is the
/// guest's connection too.
pub(crate) fn serve_cli(args: &[String]) -> Result<(), String> {
    let [kind, dir] = args else {
        return Err("usage: cloud-mock ec2 DIR".into());
    };
    if kind != "ec2" {
        return Err(format!("cloud-mock serves ec2, not {kind:?}"));
    }
    let dir = Path::new(dir);
    serve(dir).or_else(|error| {
        log(dir, &format!("ERROR {error}"))?;
        answer("500 Internal Server Error", "")
    })
}

fn log(dir: &Path, line: &str) -> Result<(), String> {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG))
        .map_err(|error| format!("open the mock's log: {error}"))?;
    writeln!(log, "{line}").map_err(|error| format!("log the request: {error}"))
}

fn answer(status: &str, body: &str) -> Result<(), String> {
    let mut output = std::io::stdout().lock();
    write!(
        output,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .and_then(|()| output.flush())
    .map_err(|error| format!("answer the request: {error}"))
}

fn serve(dir: &Path) -> Result<(), String> {
    let key = fs::read_to_string(dir.join(KEY_FILE))
        .map_err(|error| format!("read the mock's key: {error}"))?;
    let mut input = std::io::stdin().lock();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err("a request head past its bound".into());
        }
        match input.read(&mut byte) {
            Ok(0) => return Err("the connection closed inside a request head".into()),
            Ok(_) => head.extend_from_slice(&byte),
            Err(error) => return Err(format!("read the request: {error}")),
        }
    }
    let head = String::from_utf8(head).map_err(|_| "a request head that is not UTF-8")?;
    let (status, body, logged) = route_ec2(&head, key.trim_end());
    log(dir, &logged)?;
    answer(status, &body)
}

/// The status, body and log line for one IMDSv2 request.
fn route_ec2(head: &str, key: &str) -> (&'static str, String, String) {
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut parts = request.split(' ');
    let (method, path) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let auth = if method == "PUT" && header("X-aws-ec2-metadata-token-ttl-seconds").is_some() {
        "ttl"
    } else if header("X-aws-ec2-metadata-token").as_deref() == Some(TOKEN) {
        "token"
    } else {
        "none"
    };
    let logged = format!("{method} {path} {auth}");
    let found = |body: String| ("200 OK", body, logged.clone());
    match (method, path, auth) {
        ("PUT", "/latest/api/token", "ttl") => found(TOKEN.to_owned()),
        (_, _, "none" | "ttl") => ("401 Unauthorized", String::new(), logged.clone()),
        ("GET", "/latest/meta-data/local-hostname", _) => found(HOSTNAME.to_owned()),
        ("GET", "/latest/meta-data/public-keys/", _) => found("0=td-oracle".to_owned()),
        ("GET", "/latest/meta-data/public-keys/0/openssh-key", _) => found(format!("{key}\n")),
        _ => ("404 Not Found", String::new(), logged.clone()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn ask(head: &str) -> (&'static str, String, String) {
        route_ec2(head, "ssh-ed25519 AAAA k")
    }

    #[test]
    fn the_mock_answers_imdsv2_and_refuses_imdsv1() {
        let (status, body, logged) = ask(
            "PUT /latest/api/token HTTP/1.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 300\r\n\r\n",
        );
        assert_eq!((status, body.as_str()), ("200 OK", TOKEN));
        assert_eq!(logged, EXPECTED[0]);
        let with = |path: &str| {
            ask(&format!(
                "GET {path} HTTP/1.1\r\nx-aws-ec2-metadata-token: {TOKEN}\r\n\r\n"
            ))
        };
        assert_eq!(with("/latest/meta-data/local-hostname").1, HOSTNAME);
        assert_eq!(with("/latest/meta-data/public-keys/").1, "0=td-oracle");
        let (_, key, logged) = with("/latest/meta-data/public-keys/0/openssh-key");
        assert_eq!(key, "ssh-ed25519 AAAA k\n");
        assert_eq!(logged, EXPECTED[3]);
        assert_eq!(with("/latest/meta-data/other").0, "404 Not Found");
        let (status, _, logged) = ask("GET /latest/meta-data/local-hostname HTTP/1.1\r\n\r\n");
        assert_eq!(status, "401 Unauthorized");
        assert_eq!(logged, "GET /latest/meta-data/local-hostname none");
        assert_eq!(HOSTNAME.split('.').next(), Some(HOSTNAME_LABEL));
    }

    #[test]
    fn the_network_holds_both_metadata_addresses() {
        let mock = CloudMock {
            dir: PathBuf::from("/s/mock"),
            exe: PathBuf::from("/b/td-recipe-eval"),
        };
        assert_eq!(
            mock.netdev_options(),
            ",net=168.0.0.0/7,host=168.0.0.2,dns=168.0.0.3,dhcpstart=168.0.0.15,\
             guestfwd=tcp:169.254.169.254:80-cmd:/b/td-recipe-eval cloud-mock ec2 /s/mock"
        );
        // 168.0.0.0/7 spans 168.0.0.0 to 169.255.255.255.
        for address in [[169u8, 254, 169, 254], [168, 63, 129, 16]] {
            assert_eq!(address[0] & 0xfe, 168);
        }
    }
}
