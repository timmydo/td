//! Cloud metadata a server reads once its network is up (td-svc/DESIGN.md
//! §10, "Cloud metadata"): EC2's IMDSv2 or Azure's IMDS, recognised by DMI,
//! for the same hostname and Ed25519 keys a NoCloud seed may offer, and
//! Azure's ready report to its wireserver.

use std::net::SocketAddrV4;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::http;
use crate::metadata::{self, Request};

pub(crate) const DMI: &str = "/sys/class/dmi/id";
/// The link-local metadata address every cloud td recognises serves.
pub(crate) const IMDS: SocketAddrV4 =
    SocketAddrV4::new(std::net::Ipv4Addr::new(169, 254, 169, 254), 80);
/// Azure's host agent endpoint, which takes the guest's ready report.
pub(crate) const WIRESERVER: SocketAddrV4 =
    SocketAddrV4::new(std::net::Ipv4Addr::new(168, 63, 129, 16), 80);
/// The chassis asset tag every Azure VM's firmware carries.
const AZURE_ASSET_TAG: &str = "7783-7084-3265-9085-8269-3286-77";
const AZURE_API: &str = "api-version=2021-02-01&format=text";
/// How long one attempt has, and whether an absent answer (404) is one
/// the service gives while it settles.
#[derive(Clone, Copy)]
struct Patience {
    attempt: Duration,
    absent: bool,
}

const QUICK: Patience = Patience {
    attempt: TIMEOUT,
    absent: false,
};
/// A resource every VM has, which a settling IMDS may not serve yet.
const SETTLING: Patience = Patience {
    attempt: TIMEOUT,
    absent: true,
};
/// Azure's wireserver, which may be slow to answer and to have a goal state.
const AGENT: Patience = Patience {
    attempt: Duration::from_secs(15),
    absent: true,
};

/// The wireserver protocol version the ready report speaks.
const AZURE_AGENT: &[(&str, &str)] = &[
    ("x-ms-version", "2012-11-30"),
    ("x-ms-agent-name", "td-firstboot"),
];
const MAX_GOAL_FIELD: usize = 256;
/// One attempt at one request.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Every request of a fetch, retried until then: long enough for the
/// network daemon to take a lease netup's DHCP gave up on.
pub(crate) const BUDGET: Duration = Duration::from_secs(150);
/// The least time Azure's ready report has, however late the fetch ended.
const REPORT_FLOOR: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(1);
/// The least time the token's GETs have, however late it was issued.
const AFTER_TOKEN: Duration = Duration::from_secs(30);
const MAX_TOKEN: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cloud {
    Ec2,
    Azure,
}

impl Cloud {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Cloud::Ec2 => "ec2",
            Cloud::Azure => "azure",
        }
    }
}

/// The cloud the firmware's DMI names, read from `dmi`: EC2's Nitro
/// instances call their system vendor `Amazon EC2`, and Azure's VMs carry
/// its chassis asset tag.
pub(crate) fn detect(dmi: &Path) -> Option<Cloud> {
    let field = |name: &str| {
        std::fs::read_to_string(dmi.join(name))
            .ok()
            .map(|text| text.trim().to_owned())
    };
    if field("sys_vendor").as_deref() == Some("Amazon EC2") {
        Some(Cloud::Ec2)
    } else if field("chassis_asset_tag").as_deref() == Some(AZURE_ASSET_TAG) {
        Some(Cloud::Azure)
    } else {
        None
    }
}

/// What `cloud`'s metadata service at `address` asks for, by `deadline`.
pub(crate) fn fetch(
    cloud: Cloud,
    address: SocketAddrV4,
    deadline: Instant,
) -> Result<Request, String> {
    match cloud {
        Cloud::Ec2 => ec2(address, deadline),
        Cloud::Azure => azure(address, deadline),
    }
}

/// One request, retried until `deadline` while the service cannot be
/// reached, fails (5xx), throttles (429) or is not yet available (410).
fn ask(
    address: SocketAddrV4,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    deadline: Instant,
) -> Result<http::Response, String> {
    send(address, method, path, headers, b"", QUICK, deadline)
}

/// `ask`, with a request body and `patience`.
fn send(
    address: SocketAddrV4,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    patience: Patience,
    deadline: Instant,
) -> Result<http::Response, String> {
    let mut last = format!("{method} {path}: no time left to ask");
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(last);
        }
        match http::request(
            address,
            method,
            path,
            headers,
            body,
            left.min(patience.attempt),
        ) {
            Ok(response)
                if matches!(response.status, 410 | 429)
                    || response.status >= 500
                    || (patience.absent && response.status == 404) =>
            {
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

/// Azure's IMDS, which answers only a request that says `Metadata: true`:
/// the VM's computer name, which every VM has and is retried until served,
/// and then each public key by index until one is absent.
fn azure(address: SocketAddrV4, deadline: Instant) -> Result<Request, String> {
    let metadata = [("Metadata", "true")];
    let get = |path: &str, patience| {
        send(address, "GET", path, &metadata, b"", patience, deadline)?.text_or_absent(path)
    };
    let hostname = get(
        &format!("/metadata/instance/compute/osProfile/computerName?{AZURE_API}"),
        SETTLING,
    )?
    .map(|name| name.trim().to_owned())
    .unwrap_or_default();
    let mut offered = Vec::new();
    let mut skipped = Vec::new();
    for index in 0..=metadata::MAX_KEYS {
        let Some(key) = get(
            &format!("/metadata/instance/compute/publicKeys/{index}/keyData?{AZURE_API}"),
            QUICK,
        )?
        else {
            break;
        };
        if index == metadata::MAX_KEYS {
            skipped.push(format!(
                "public key(s) past the first {}",
                metadata::MAX_KEYS
            ));
            break;
        }
        offered.extend(
            key.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned),
        );
    }
    Ok(metadata::admit(
        &[("Azure computerName", hostname)],
        offered,
        skipped,
    ))
}

/// Tells Azure's wireserver at `address` the VM is ready, as its goal
/// state names it, so the platform ends provisioning, by `deadline` or at
/// least a minute from now; returns the goal state's incarnation. The
/// paths are Azure's own agent's.
pub(crate) fn report_ready(address: SocketAddrV4, deadline: Instant) -> Result<String, String> {
    let deadline = deadline.max(Instant::now() + REPORT_FLOOR);
    let goal = send(
        address,
        "GET",
        "/machine/?comp=goalstate",
        AZURE_AGENT,
        b"",
        AGENT,
        deadline,
    )?
    .text_or_absent("the goal state")?
    .ok_or("the wireserver has no goal state")?;
    let incarnation = goal_field(&goal, "Incarnation")?;
    let container = goal_field(&goal, "ContainerId")?;
    let instance = goal_field(&goal, "InstanceId")?;
    let mut headers = AZURE_AGENT.to_vec();
    headers.push(("Content-Type", "text/xml;charset=utf-8"));
    let response = send(
        address,
        "POST",
        "/machine?comp=health",
        &headers,
        health(incarnation, container, instance).as_bytes(),
        AGENT,
        deadline,
    )?;
    if !(200..300).contains(&response.status) {
        return Err(format!("the ready report: HTTP {}", response.status));
    }
    Ok(incarnation.to_owned())
}

/// The text of the first `<name>` element in `goal`, which the report
/// echoes as it stands: it must be text without markup, control characters
/// or an `&` that does not begin an entity.
fn goal_field<'a>(goal: &'a str, name: &str) -> Result<&'a str, String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let (_, rest) = goal
        .split_once(&open)
        .ok_or_else(|| format!("the goal state has no {name}"))?;
    let (value, _) = rest
        .split_once(&close)
        .ok_or_else(|| format!("the goal state's {name} is not closed"))?;
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_GOAL_FIELD
        || value
            .chars()
            .any(|c| c.is_control() || c == '<' || c == '>')
        || !entities_complete(value)
    {
        return Err(format!("the goal state's {name} is not plain text"));
    }
    Ok(value)
}

/// Whether each `&` in `text` begins one of XML's five named entities or a
/// character reference.
fn entities_complete(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        let tail = rest.get(at + 1..).unwrap_or_default();
        let Some(end) = tail.find(';') else {
            return false;
        };
        let entity = tail.get(..end).unwrap_or_default();
        let reference = |digits: Option<&str>, hex: bool| {
            digits.is_some_and(|digits| {
                !digits.is_empty()
                    && digits.chars().all(|c| {
                        if hex {
                            c.is_ascii_hexdigit()
                        } else {
                            c.is_ascii_digit()
                        }
                    })
            })
        };
        if !(matches!(entity, "amp" | "lt" | "gt" | "quot" | "apos")
            || reference(entity.strip_prefix("#x"), true)
            || reference(entity.strip_prefix('#'), false))
        {
            return false;
        }
        rest = tail.get(end + 1..).unwrap_or_default();
    }
    true
}

/// The health report that says the one role instance is ready.
fn health(incarnation: &str, container: &str, instance: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <Health xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
         xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n\
         <GoalStateIncarnation>{incarnation}</GoalStateIncarnation>\n\
         <Container>\n\
         <ContainerId>{container}</ContainerId>\n\
         <RoleInstanceList>\n\
         <Role>\n\
         <InstanceId>{instance}</InstanceId>\n\
         <Health>\n\
         <State>Ready</State>\n\
         </Health>\n\
         </Role>\n\
         </RoleInstanceList>\n\
         </Container>\n\
         </Health>\n"
    )
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
        std::fs::write(
            dir.path().join("chassis_asset_tag"),
            format!("{AZURE_ASSET_TAG}\n"),
        )
        .unwrap();
        assert_eq!(detect(dir.path()), Some(Cloud::Azure));
        std::fs::write(dir.path().join("chassis_asset_tag"), "7783\n").unwrap();
        assert_eq!(detect(dir.path()), None);
    }

    /// A loopback service answering each request by `answer`, which sees
    /// the request head and body; it hands back what it was asked.
    fn answering(
        responses: usize,
        answer: fn(&str, &str) -> (&'static str, String),
    ) -> (SocketAddrV4, std::thread::JoinHandle<Vec<(String, String)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            panic!("an IPv4 listener has an IPv4 address");
        };
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let end = loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                };
                let head = String::from_utf8(request[..end].to_vec()).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .map_or(0, |length| length.trim().parse().unwrap());
                while request.len() < end + length {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                }
                let body = String::from_utf8(request[end..end + length].to_vec()).unwrap();
                let (status, reply) = answer(&head, &body);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                seen.push((head.lines().next().unwrap().to_owned(), body));
            }
            seen
        });
        (address, server)
    }

    fn azure_imds(head: &str, _: &str) -> (&'static str, String) {
        if !head.contains("\r\nMetadata: true\r\n") {
            return ("400 Bad Request", String::new());
        }
        let path = head.split(' ').nth(1).unwrap();
        let api = "?api-version=2021-02-01&format=text";
        match path.strip_suffix(api).unwrap_or_default() {
            "/metadata/instance/compute/osProfile/computerName" => ("200 OK", "td-vm\n".to_owned()),
            "/metadata/instance/compute/publicKeys/0/keyData" => {
                ("200 OK", format!("{KEY} azure\r\n"))
            }
            "/metadata/instance/compute/publicKeys/1/keyData" => {
                ("200 OK", "ssh-rsa AAAAB3NzaC1yc2E= rsa".to_owned())
            }
            _ => ("404 Not Found", String::new()),
        }
    }

    /// `azure_imds`, but unavailable and then without the computer name
    /// while it settles.
    fn settling_imds(head: &str, body: &str) -> (&'static str, String) {
        static ASKED: AtomicUsize = AtomicUsize::new(0);
        match ASKED.fetch_add(1, Ordering::SeqCst) {
            0 => ("410 Gone", String::new()),
            1 => ("404 Not Found", String::new()),
            _ => azure_imds(head, body),
        }
    }

    #[test]
    fn a_settling_azure_imds_is_asked_again_for_the_computer_name() {
        let (address, server) = answering(6, settling_imds);
        let asked = azure(address, soon()).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "td-vm");
        assert_eq!(asked.keys, [format!("{KEY} azure")]);
        assert_eq!(server.join().unwrap().len(), 6);
    }

    #[test]
    fn azure_imds_gives_the_computer_name_and_each_key_until_one_is_absent() {
        let (address, server) = answering(4, azure_imds);
        let asked = azure(address, soon()).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "td-vm");
        assert_eq!(asked.keys, [format!("{KEY} azure")]);
        assert_eq!(asked.skipped, ["1 key(s) other than plain ssh-ed25519"]);
        let seen = server.join().unwrap();
        assert_eq!(
            seen.last().unwrap().0,
            "GET /metadata/instance/compute/publicKeys/2/keyData\
             ?api-version=2021-02-01&format=text HTTP/1.1"
        );
    }

    const GOAL: &str = "<?xml version=\"1.0\"?>\n<GoalState><Version>2012-11-30</Version>\
        <Incarnation>3</Incarnation><Machine><ExpectedState>Started</ExpectedState>\
        </Machine><Container><ContainerId>c0ffee-01</ContainerId><RoleInstanceList>\
        <RoleInstance><InstanceId>b4d-02._td-vm</InstanceId></RoleInstance>\
        </RoleInstanceList></Container></GoalState>";

    fn wireserver(head: &str, body: &str) -> (&'static str, String) {
        if !head.contains("\r\nx-ms-version: 2012-11-30\r\n") {
            return ("400 Bad Request", String::new());
        }
        match head.lines().next().unwrap() {
            "GET /machine/?comp=goalstate HTTP/1.1" => ("200 OK", GOAL.to_owned()),
            "POST /machine?comp=health HTTP/1.1"
                if head.contains("\r\nContent-Type: text/xml;charset=utf-8\r\n")
                    && body.contains("<GoalStateIncarnation>3</GoalStateIncarnation>")
                    && body.contains("<ContainerId>c0ffee-01</ContainerId>")
                    && body.contains("<InstanceId>b4d-02._td-vm</InstanceId>")
                    && body.contains("<State>Ready</State>") =>
            {
                ("200 OK", String::new())
            }
            _ => ("400 Bad Request", String::new()),
        }
    }

    #[test]
    fn the_ready_report_echoes_the_goal_state() {
        let (address, server) = answering(2, wireserver);
        assert_eq!(report_ready(address, Instant::now()).unwrap(), "3");
        let seen = server.join().unwrap();
        assert_eq!(seen[1].0, "POST /machine?comp=health HTTP/1.1");
    }

    #[test]
    fn a_goal_state_field_is_echoed_only_as_plain_text() {
        assert_eq!(goal_field(GOAL, "InstanceId").unwrap(), "b4d-02._td-vm");
        for (field, value) in [
            ("<I>a&amp;b</I>", "a&amp;b"),
            ("<I>vm&#96;1&#x7e;</I>", "vm&#96;1&#x7e;"),
            ("<I> 3 </I>", "3"),
        ] {
            assert_eq!(goal_field(field, "I").unwrap(), value);
        }
        for goal in [
            "<GoalState></GoalState>",
            "<I>3",
            "<I></I>",
            "<I>3<x/></I>",
            "<I>a&b</I>",
            "<I>a&bogus;</I>",
            "<I>a&#;</I>",
            "<I>a&#x;</I>",
            "<I>a&#12a;</I>",
            "<I>a\u{7}b</I>",
        ] {
            assert!(goal_field(goal, "I").is_err(), "{goal}");
        }
        assert!(goal_field(&format!("<I>{}</I>", "a".repeat(257)), "I").is_err());
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
