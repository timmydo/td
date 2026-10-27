//! A mock cloud metadata service for `qemu-boot-server` (td-svc/DESIGN.md
//! §10, "Cloud metadata"). QEMU's user network runs `td-recipe-eval
//! cloud-mock KIND DIR` once for each guest connection to 169.254.169.254:80,
//! and for Azure to its wireserver's 168.63.129.16:80 too
//! (`guestfwd=...-cmd:`), with the connection as its stdin, stdout and
//! stderr. It answers that one request as EC2's IMDSv2 does, refusing a GET
//! without the session token, or as Azure's IMDS and wireserver do, refusing
//! a request without their headers, and logs it, or why it could not answer,
//! to `DIR/requests.log` for the oracle.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The session token the mock issues and then requires.
const TOKEN: &str = "td-oracle-imds-token";
/// A full name, so the boot shows only its first label is taken.
pub(super) const HOSTNAME: &str = "td-ec2-host.ec2.internal";
pub(super) const HOSTNAME_LABEL: &str = "td-ec2-host";
pub(super) const AZURE_HOSTNAME: &str = "td-azure-host";
/// The goal state's incarnation, which the ready report must echo.
pub(super) const AZURE_INCARNATION: &str = "3";
const AZURE_CONTAINER: &str = "c0ffee00-td01-4e2a-9f00-000000000001";
const AZURE_INSTANCE: &str = "b4d00000-td02-4e2a-9f00-000000000002._td-azure-host";
/// Azure's chassis asset tag, as QEMU's SMBIOS type 3 option.
const AZURE_CHASSIS: &str = "type=3,asset=7783-7084-3265-9085-8269-3286-77";
const AZURE_API: &str = "?api-version=2021-02-01&format=text";
const MAX_BODY: usize = 64 * 1024;
/// A user network that holds 169.254.169.254, as libslirp requires of a
/// guestfwd address, and Azure's 168.63.129.16.
const NETWORK: &str = "net=168.0.0.0/7,host=168.0.0.2,dns=168.0.0.3,dhcpstart=168.0.0.15";
const KEY_FILE: &str = "openssh-key";
const LOG: &str = "requests.log";
const MAX_HEAD: usize = 16 * 1024;

/// What the guest must ask EC2, in order: a token, then each path with it.
pub(super) const EXPECTED: &[&str] = &[
    "PUT /latest/api/token ttl",
    "GET /latest/meta-data/local-hostname token",
    "GET /latest/meta-data/public-keys/ token",
    "GET /latest/meta-data/public-keys/0/openssh-key token",
];

/// What the guest must ask Azure, in order: its IMDS for the name and each
/// key until one is absent, then the wireserver for its goal state, and a
/// ready report that echoes it.
pub(super) const EXPECTED_AZURE: &[&str] = &[
    "GET 169.254.169.254 /metadata/instance/compute/osProfile/computerName\
     ?api-version=2021-02-01&format=text metadata",
    "GET 169.254.169.254 /metadata/instance/compute/publicKeys/0/keyData\
     ?api-version=2021-02-01&format=text metadata",
    "GET 169.254.169.254 /metadata/instance/compute/publicKeys/1/keyData\
     ?api-version=2021-02-01&format=text metadata",
    "GET 168.63.129.16 /machine/?comp=goalstate agent",
    "POST 168.63.129.16 /machine?comp=health agent ready",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ec2,
    Azure,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Ec2 => "ec2",
            Kind::Azure => "azure",
        }
    }
}

pub(crate) struct CloudMock {
    kind: Kind,
    dir: PathBuf,
    exe: PathBuf,
}

impl CloudMock {
    /// An EC2 service offering `key` as the instance's one key pair, kept
    /// under `dir`, which must not exist.
    pub(crate) fn ec2(dir: &Path, key: &str) -> Result<Self, String> {
        Self::new(Kind::Ec2, dir, key)
    }

    /// Azure's IMDS offering `key` as the VM's one public key, and its
    /// wireserver, kept under `dir`, which must not exist.
    pub(crate) fn azure(dir: &Path, key: &str) -> Result<Self, String> {
        Self::new(Kind::Azure, dir, key)
    }

    fn new(kind: Kind, dir: &Path, key: &str) -> Result<Self, String> {
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
            kind,
            dir: dir.to_owned(),
            exe,
        })
    }

    /// The options a boot's `-netdev user` gains: the network, and each
    /// address the cloud serves run as this mock.
    pub(crate) fn netdev_options(&self) -> String {
        let addresses: &[&str] = match self.kind {
            Kind::Ec2 => &["169.254.169.254"],
            Kind::Azure => &["169.254.169.254", "168.63.129.16"],
        };
        let mut options = format!(",{NETWORK}");
        for address in addresses {
            options.push_str(&format!(
                ",guestfwd=tcp:{address}:80-cmd:{} cloud-mock {} {}",
                self.exe.display(),
                self.kind.name(),
                self.dir.display()
            ));
        }
        options
    }

    /// The firmware identity the cloud's machines carry: EC2 Nitro's system
    /// vendor, or Azure's with its chassis asset tag.
    pub(crate) fn smbios(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::Ec2 => &["-smbios", "type=1,manufacturer=Amazon EC2"],
            Kind::Azure => &[
                "-smbios",
                "type=1,manufacturer=Microsoft Corporation",
                "-smbios",
                AZURE_CHASSIS,
            ],
        }
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

/// `cloud-mock ec2|azure DIR`: one request on stdin, its answer on stdout.
/// What stops it is logged and answered as a 500, since its stderr is the
/// guest's connection too.
pub(crate) fn serve_cli(args: &[String]) -> Result<(), String> {
    let [kind, dir] = args else {
        return Err("usage: cloud-mock ec2|azure DIR".into());
    };
    let kind = match kind.as_str() {
        "ec2" => Kind::Ec2,
        "azure" => Kind::Azure,
        other => return Err(format!("cloud-mock serves ec2 or azure, not {other:?}")),
    };
    let dir = Path::new(dir);
    serve(kind, dir).or_else(|error| {
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

fn serve(kind: Kind, dir: &Path) -> Result<(), String> {
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
    let length = match header(&head, "Content-Length") {
        Some(length) => length
            .parse::<usize>()
            .ok()
            .filter(|length| *length <= MAX_BODY)
            .ok_or("a malformed or oversized Content-Length")?,
        None => 0,
    };
    let mut body = vec![0u8; length];
    input
        .read_exact(&mut body)
        .map_err(|error| format!("read the request body: {error}"))?;
    let body = String::from_utf8(body).map_err(|_| "a request body that is not UTF-8")?;
    let (status, reply, logged) = match kind {
        Kind::Ec2 => route_ec2(&head, key.trim_end()),
        Kind::Azure => route_azure(&head, &body, key.trim_end()),
    };
    log(dir, &logged)?;
    answer(status, &reply)
}

/// A request head's method and path.
fn request_line(head: &str) -> (&str, &str) {
    let mut parts = head.split("\r\n").next().unwrap_or_default().split(' ');
    (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    )
}

/// The value of a request head's field `name`.
fn header(head: &str, name: &str) -> Option<String> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// The status, body and log line for one request to Azure's IMDS, which
/// requires `Metadata: true`, or its wireserver, which requires the
/// protocol version.
fn route_azure(head: &str, body: &str, key: &str) -> (&'static str, String, String) {
    let (method, path) = request_line(head);
    let host = header(head, "Host").unwrap_or_default();
    let auth = if header(head, "Metadata").as_deref() == Some("true") {
        "metadata"
    } else if header(head, "x-ms-version").as_deref() == Some("2012-11-30") {
        "agent"
    } else {
        "none"
    };
    let logged = format!("{method} {host} {path} {auth}");
    let refused = || ("400 Bad Request", String::new(), logged.clone());
    match (host.as_str(), auth) {
        ("169.254.169.254", "metadata") if method == "GET" => {
            let leaf = path.strip_suffix(AZURE_API).unwrap_or_default();
            match leaf {
                "/metadata/instance/compute/osProfile/computerName" => {
                    ("200 OK", AZURE_HOSTNAME.to_owned(), logged.clone())
                }
                "/metadata/instance/compute/publicKeys/0/keyData" => {
                    ("200 OK", key.to_owned(), logged.clone())
                }
                _ => ("404 Not Found", String::new(), logged.clone()),
            }
        }
        ("168.63.129.16", "agent") => match (method, path) {
            ("GET", "/machine/?comp=goalstate") => (
                "200 OK",
                format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<GoalState>\
                     <Version>2012-11-30</Version><Incarnation>{AZURE_INCARNATION}\
                     </Incarnation><Machine><ExpectedState>Started</ExpectedState>\
                     </Machine><Container><ContainerId>{AZURE_CONTAINER}</ContainerId>\
                     <RoleInstanceList><RoleInstance><InstanceId>{AZURE_INSTANCE}\
                     </InstanceId></RoleInstance></RoleInstanceList></Container>\
                     </GoalState>\n"
                ),
                logged.clone(),
            ),
            ("POST", "/machine?comp=health") => {
                let ready = header(head, "Content-Type").as_deref()
                    == Some("text/xml;charset=utf-8")
                    && [
                        format!("<GoalStateIncarnation>{AZURE_INCARNATION}</GoalStateIncarnation>"),
                        format!("<ContainerId>{AZURE_CONTAINER}</ContainerId>"),
                        format!("<InstanceId>{AZURE_INSTANCE}</InstanceId>"),
                        "<State>Ready</State>".to_owned(),
                    ]
                    .iter()
                    .all(|element| body.contains(element.as_str()));
                if ready {
                    ("200 OK", String::new(), format!("{logged} ready"))
                } else {
                    (
                        "400 Bad Request",
                        String::new(),
                        format!("{logged} malformed"),
                    )
                }
            }
            _ => ("404 Not Found", String::new(), logged.clone()),
        },
        _ => refused(),
    }
}

/// The status, body and log line for one IMDSv2 request.
fn route_ec2(head: &str, key: &str) -> (&'static str, String, String) {
    let (method, path) = request_line(head);
    let header = |name: &str| header(head, name);
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
    fn the_azure_mock_answers_only_with_its_headers_and_checks_the_report() {
        let ask = |head: &str, body: &str| route_azure(head, body, "ssh-ed25519 AAAA k");
        let imds = |path: &str| {
            ask(
                &format!("GET {path} HTTP/1.1\r\nHost: 169.254.169.254\r\nMetadata: true\r\n\r\n"),
                "",
            )
        };
        let name = imds(&format!(
            "/metadata/instance/compute/osProfile/computerName{AZURE_API}"
        ));
        assert_eq!((name.0, name.1.as_str()), ("200 OK", AZURE_HOSTNAME));
        assert_eq!(name.2, EXPECTED_AZURE[0]);
        let key = imds(&format!(
            "/metadata/instance/compute/publicKeys/0/keyData{AZURE_API}"
        ));
        assert_eq!(key.1, "ssh-ed25519 AAAA k");
        let absent = imds(&format!(
            "/metadata/instance/compute/publicKeys/1/keyData{AZURE_API}"
        ));
        assert_eq!(
            (absent.0, absent.2.as_str()),
            ("404 Not Found", EXPECTED_AZURE[2])
        );
        let bare = ask(
            "GET /metadata/instance HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
            "",
        );
        assert_eq!(bare.0, "400 Bad Request");
        let agent = "Host: 168.63.129.16\r\nx-ms-version: 2012-11-30\r\n";
        let goal = ask(
            &format!("GET /machine/?comp=goalstate HTTP/1.1\r\n{agent}\r\n"),
            "",
        );
        assert_eq!(goal.2, EXPECTED_AZURE[3]);
        assert!(goal
            .1
            .contains(&format!("<InstanceId>{AZURE_INSTANCE}</InstanceId>")));
        let report = |body: &str| {
            ask(
                &format!(
                    "POST /machine?comp=health HTTP/1.1\r\n{agent}\
                     Content-Type: text/xml;charset=utf-8\r\n\r\n"
                ),
                body,
            )
        };
        let whole = format!(
            "<GoalStateIncarnation>{AZURE_INCARNATION}</GoalStateIncarnation>\
             <ContainerId>{AZURE_CONTAINER}</ContainerId>\
             <InstanceId>{AZURE_INSTANCE}</InstanceId><State>Ready</State>"
        );
        assert_eq!(report(&whole).2, EXPECTED_AZURE[4]);
        assert_eq!(
            report(&whole.replace("Ready", "NotReady")).0,
            "400 Bad Request"
        );
    }

    #[test]
    fn the_network_holds_both_metadata_addresses() {
        let mock = CloudMock {
            kind: Kind::Ec2,
            dir: PathBuf::from("/s/mock"),
            exe: PathBuf::from("/b/td-recipe-eval"),
        };
        assert_eq!(
            mock.netdev_options(),
            ",net=168.0.0.0/7,host=168.0.0.2,dns=168.0.0.3,dhcpstart=168.0.0.15,\
             guestfwd=tcp:169.254.169.254:80-cmd:/b/td-recipe-eval cloud-mock ec2 /s/mock"
        );
        let azure = CloudMock {
            kind: Kind::Azure,
            ..mock
        };
        assert_eq!(
            azure.netdev_options(),
            ",net=168.0.0.0/7,host=168.0.0.2,dns=168.0.0.3,dhcpstart=168.0.0.15,\
             guestfwd=tcp:169.254.169.254:80-cmd:/b/td-recipe-eval cloud-mock azure /s/mock,\
             guestfwd=tcp:168.63.129.16:80-cmd:/b/td-recipe-eval cloud-mock azure /s/mock"
        );
        // 168.0.0.0/7 spans 168.0.0.0 to 169.255.255.255.
        for address in [[169u8, 254, 169, 254], [168, 63, 129, 16]] {
            assert_eq!(address[0] & 0xfe, 168);
        }
    }
}
