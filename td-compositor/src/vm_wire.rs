//! td-owned VM messages. The carrier supplies isolation; request IDs only
//! correlate replies. No guest message can request a host operation.
use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)] // Used by the host runner that includes this codec.
pub const PORT: &str = "org.td.vm.1";
#[allow(dead_code)] // Used by the guest worker that includes this codec.
pub const FEED_FILE: &str = "/run/td-compositor/1000/vm-feed";
pub const MAX_TEXT: usize = 65536;
pub const MAX_LINE: usize = MAX_TEXT * 2 + 128;
pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const SNAPSHOT: &str = "snapshot";
pub const PUT: &str = "put";
pub const GET: &str = "get";
pub const FEED: &str = "feed";
pub const KEY: &str = "git-key";
pub const WORKSPACE: &str = "workspace";
pub const OK: &str = "ok";
pub const ERROR: &str = "error";

#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    pub verb: String,
    pub revision: u64,
    pub data: Vec<u8>,
}

impl Message {
    pub fn new(id: u64, verb: &str, revision: u64, data: Vec<u8>) -> Self {
        Self {
            id,
            verb: verb.into(),
            revision,
            data,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.data.len() > MAX_TEXT || !valid_verb(&self.verb) || self.id == 0 {
            return Err("invalid VM message".into());
        }
        let mut line = format!(
            "TDVM1 {} {} {} {} ",
            self.id,
            self.verb,
            self.revision,
            self.data.len()
        );
        const HEX: &[u8] = b"0123456789abcdef";
        for byte in &self.data {
            for nibble in [byte >> 4, byte & 15] {
                line.push(char::from(
                    *HEX.get(usize::from(nibble)).ok_or("hex digit")?,
                ));
            }
        }
        line.push('\n');
        Ok(line.into_bytes())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_LINE {
            return Err("VM message exceeds limit".into());
        }
        let line = std::str::from_utf8(bytes).map_err(|_| "non-ASCII VM header")?;
        let mut fields = line
            .strip_suffix('\n')
            .ok_or("truncated VM message")?
            .split(' ');
        if fields.next() != Some("TDVM1") {
            return Err("unsupported VM protocol".into());
        }
        let id = decimal(fields.next())?;
        let verb = fields.next().ok_or("missing VM verb")?;
        let revision = decimal(fields.next())?;
        let length = decimal(fields.next())?;
        let hex = fields.next().ok_or("missing VM payload")?;
        if id == 0
            || !valid_verb(verb)
            || fields.next().is_some()
            || hex.len() > MAX_TEXT * 2
            || length > MAX_TEXT as u64
            || hex.len() as u64 != length * 2
        {
            return Err("invalid VM message fields".into());
        }
        let mut data = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().as_chunks::<2>().0 {
            let high = unhex(*pair.first().ok_or("hex pair")?)?;
            let low = unhex(*pair.get(1).ok_or("hex pair")?)?;
            data.push(high * 16 + low);
        }
        Ok(Self::new(id, verb, revision, data))
    }
}

fn valid_verb(verb: &str) -> bool {
    matches!(verb, SNAPSHOT | PUT | GET | FEED | KEY | WORKSPACE | OK | ERROR)
}

fn decimal(value: Option<&str>) -> Result<u64, String> {
    let value = value.ok_or("missing VM number")?;
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err("invalid VM number".into());
    }
    value.parse().map_err(|_| "VM number overflow".into())
}

fn unhex(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("invalid VM hex payload".into()),
    }
}

pub fn text(bytes: &[u8]) -> Result<&str, String> {
    if bytes.len() > MAX_TEXT {
        return Err("clipboard exceeds 64 KiB".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "clipboard is not UTF-8")?;
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("clipboard contains terminal controls".into());
    }
    Ok(text)
}

/// The initial feed route is deliberately the host alias on QEMU user-net.
/// No guest-supplied URL or host fetch command crosses this protocol.
pub fn feed(bytes: &[u8]) -> Result<&str, String> {
    let value = std::str::from_utf8(bytes).map_err(|_| "invalid feed endpoint")?;
    if value.is_empty() {
        return Ok(value);
    }
    let port = value
        .strip_prefix("http://10.0.2.2:")
        .ok_or("feed must be http://10.0.2.2:PORT")?;
    let port = decimal(Some(port))?;
    if !(1..=65535).contains(&port) {
        return Err("invalid feed port".into());
    }
    Ok(value)
}

/// New carriers begin with a newline to separate abandoned bytes from new frames.
/// An oversized or expired frame is discarded through its next delimiter.
#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
    started: Option<Instant>,
    discard: bool,
}

impl Decoder {
    pub fn push(&mut self, byte: u8, now: Instant) -> Option<Result<Message, String>> {
        if self
            .started
            .is_some_and(|start| now.duration_since(start) >= TIMEOUT)
        {
            self.bytes.clear();
            self.discard = true;
        }
        if byte == b'\n' {
            self.started = None;
            if self.discard {
                self.discard = false;
                self.bytes.clear();
                return Some(Err("VM frame exceeded its size or deadline".into()));
            }
            if self.bytes.is_empty() {
                return None;
            }
            self.bytes.push(byte);
            let result = Message::decode(&self.bytes);
            self.bytes.clear();
            return Some(result);
        }
        self.started.get_or_insert(now);
        if !self.discard {
            if self.bytes.len() >= MAX_LINE - 1 {
                self.bytes.clear();
                self.discard = true;
            } else {
                self.bytes.push(byte);
            }
        }
        None
    }
}

/// Callers provide nonblocking descriptors and one absolute deadline across
/// all reads/writes in a conversation.
pub fn write_all(io: &mut impl Write, mut bytes: &[u8], deadline: Instant) -> Result<(), String> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err("VM bridge write timed out".into());
        }
        match io.write(bytes) {
            Ok(0) => return Err("VM bridge closed during write".into()),
            Ok(n) => bytes = bytes.get(n..).ok_or("VM write length")?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(format!("VM bridge write: {e}")),
        }
    }
    Ok(())
}

// One outstanding conversation only: bytes after its reply are not queued.
// Unsolicited/asynchronous guest messages require a persistent decoder instead.
#[allow(dead_code)] // The guest streams requests through Decoder directly.
pub fn receive(io: &mut impl Read, id: Option<u64>, deadline: Instant) -> Result<Message, String> {
    let mut decoder = Decoder::default();
    let mut buffer = [0; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err("VM bridge unavailable or timed out".into());
        }
        match io.read(&mut buffer) {
            Ok(0) => return Err("VM bridge disconnected".into()),
            Ok(n) => {
                for byte in buffer.get(..n).ok_or("VM read length")? {
                    if let Some(message) = decoder.push(*byte, Instant::now()) {
                        let message = message?;
                        if id.is_none_or(|id| message.id == id) {
                            return Ok(message);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(format!("VM bridge read: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;

    #[test]
    fn framing_is_bounded_and_recovers_after_abandonment() {
        let message = Message::new(7, PUT, 3, "hello\n世界\t".as_bytes().to_vec());
        let encoded = message.encode().unwrap();
        assert_eq!(Message::decode(&encoded).unwrap(), message);
        let mut decoder = Decoder::default();
        let now = Instant::now();
        for _ in 0..MAX_LINE + 12 {
            assert!(decoder.push(b'x', now).is_none());
        }
        assert!(decoder.bytes.len() <= MAX_LINE);
        assert!(decoder.push(b'\n', now).unwrap().is_err());
        let mut result = None;
        for byte in encoded {
            result = decoder.push(byte, now);
        }
        assert_eq!(result.unwrap().unwrap(), message);
        decoder.push(b'T', now);
        assert!(decoder.push(b'\n', now + TIMEOUT).unwrap().is_err());
    }

    #[test]
    fn a_new_carrier_delimiter_cannot_commit_a_truncated_payload() {
        let mut frame = Message::new(9, PUT, 7, b"abcdef".to_vec())
            .encode()
            .unwrap();
        frame.truncate(frame.len() - 7);
        frame.push(b'\n');
        assert!(Message::decode(&frame).is_err());
    }

    #[test]
    fn rejects_versions_overflow_controls_and_endpoint_authority() {
        for line in [
            "TDVM2 1 get 0 0 \n",
            "TDVM1 0 get 0 0 \n",
            "TDVM1 01 get 0 0 \n",
            "TDVM1 1 exec 0 0 \n",
            "TDVM1 1 get 0 1 a\n",
            "TDVM1 1 get 0 1 gg\n",
            "TDVM1 1 get 18446744073709551616 0 \n",
            "TDVM1 1 get 0 ",
        ] {
            assert!(Message::decode(line.as_bytes()).is_err(), "{line}");
        }
        assert!(text(b"\x1b[201~").is_err());
        assert!(text(&[255]).is_err());
        assert!(text(&vec![b'a'; MAX_TEXT + 1]).is_err());
        assert!(feed(b"http://10.0.2.2:1234").is_ok());
        for endpoint in [
            "http://127.0.0.1:1234",
            "http://10.0.2.2:0",
            "http://10.0.2.2:123/a",
            "http://10.0.2.2:123?x",
            "http://10.0.2.2:123\n",
            "http://10.0.2.2:65536",
        ] {
            assert!(feed(endpoint.as_bytes()).is_err());
        }
    }
}

#[allow(dead_code)] // Public key codec is shared with the guest helper and host manager.
pub mod git_key {
    //! Public key exchange only; the guest never sends private key bytes.

    pub const REQUEST: &str = "/run/td-compositor/1000/vm-git-identity";
    pub const RESPONSE: &str = "/run/td-guest/1000/git-key";
    pub const LIMIT: usize = 256;

    pub fn identity(bytes: &[u8]) -> Result<&str, String> {
        let value = std::str::from_utf8(bytes).map_err(|_| "invalid VM identity")?;
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("VM identity must be 32 lowercase hexadecimal digits".into());
        }
        Ok(value)
    }

    pub fn key(value: &str) -> Result<&str, String> {
        let encoded = value
            .strip_prefix("ssh-ed25519 ")
            .ok_or("expected Ed25519 public key")?;
        if encoded.len() != 68
            || !encoded.starts_with("AAAAC3NzaC1lZDI1NTE5AAAAI")
            || !matches!(encoded.as_bytes().get(25), Some(b'A'..=b'P'))
            || !encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
        {
            return Err("invalid Ed25519 public key".into());
        }
        Ok(encoded)
    }

    pub fn encode(id: &str, public_key: &str) -> Result<Vec<u8>, String> {
        identity(id.as_bytes())?;
        key(public_key)?;
        Ok(format!("TDVM-GIT-KEY-1\n{id}\n{public_key}\n").into_bytes())
    }

    pub fn parse(bytes: &[u8], expected: &str) -> Result<String, String> {
        identity(expected.as_bytes())?;
        if bytes.len() > LIMIT {
            return Err("VM public key reply exceeds limit".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "invalid public key reply")?;
        let mut lines = text
            .strip_suffix('\n')
            .ok_or("incomplete public key reply")?
            .split('\n');
        if lines.next() != Some("TDVM-GIT-KEY-1") {
            return Err("unsupported guest public key reply header".into());
        }
        let id = lines
            .next()
            .filter(|id| !id.is_empty())
            .ok_or("missing guest key identity")?;
        if id != expected {
            return Err("guest key identity does not match this instance".into());
        }
        let public_key = lines.next().ok_or("missing guest public key")?;
        key(public_key)?;
        if lines.next().is_some() {
            return Err("extra public key reply data".into());
        }
        Ok(public_key.into())
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;
        const ID: &str = "0123456789abcdef0123456789abcdef";
        const KEY: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
        #[test]
        fn reply_is_bounded_and_bound_to_one_identity() {
            assert!(parse(b"TDVM-GIT-KEY-1\n", ID)
                .unwrap_err()
                .contains("missing guest key identity"));
            let reply = encode(ID, KEY).unwrap();
            assert_eq!(parse(&reply, ID).unwrap(), KEY);
            assert!(parse(&reply, "1123456789abcdef0123456789abcdef").is_err());
            assert!(parse(&[reply, b"extra\n".to_vec()].concat(), ID).is_err());
            assert!(identity(b"../path").is_err());
            assert!(encode(ID, &format!("{KEY}\nprivate-data")).is_err());
        }
    }
}


#[allow(dead_code)] // Shared public provisioning contract at all three boundaries.
pub mod workspace {
    pub const REQUEST: &str = "/run/td-compositor/1000/vm-workspace";
    pub const RESPONSE: &str = "/run/td-guest/1000/workspace";
    pub const LIMIT: usize = 2048;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Plan {
        pub id: String,
        pub branch: String,
        pub commit: String,
        pub repository: String,
        pub address: String,
        pub port: u16,
        pub user: String,
        pub host_key: String,
        pub guest_key: String,
        pub author_name: String,
        pub author_email: String,
    }
    impl Plan {
        pub fn parse(bytes: &[u8]) -> Result<Self, String> {
            if bytes.len() > LIMIT {
                return Err("workspace plan exceeds limit".into());
            }
            let text = std::str::from_utf8(bytes).map_err(|_| "workspace plan is not UTF-8")?;
            let mut fields = text
                .strip_suffix('\n')
                .ok_or("incomplete workspace plan")?
                .split('\n');
            if fields.next() != Some("TDVM-CLONE-1") {
                return Err("unsupported workspace plan".into());
            }
            let mut field = || {
                fields
                    .next()
                    .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                    .map(String::from)
                    .ok_or_else(|| "missing or invalid workspace field".to_string())
            };
            let id = field()?;
            let branch = field()?;
            let commit = field()?;
            let repository = field()?;
            let address = field()?;
            let port_text = field()?;
            let port: u16 = port_text
                .parse()
                .map_err(|_| "invalid workspace SSH port")?;
            let result = Self {
                id,
                branch,
                commit,
                repository,
                address,
                port,
                user: field()?,
                host_key: field()?,
                guest_key: field()?,
                author_name: field()?,
                author_email: field()?,
            };
            if fields.next().is_some() || port == 0 || port.to_string() != port_text {
                return Err("extra workspace fields or invalid port".into());
            }
            super::git_key::identity(result.id.as_bytes())?;
            super::git_key::key(&result.host_key)?;
            super::git_key::key(&result.guest_key)?;
            if !branch_valid(&result.branch)
                || !matches!(result.commit.len(), 40 | 64)
                || !result
                    .commit
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || result.commit.bytes().all(|b| b == b'0')
            {
                return Err("invalid workspace branch or commit".into());
            }
            let path = &result.repository;
            if !path.starts_with('/')
                || path.len() > 200
                || path
                    .split('/')
                    .skip(1)
                    .any(|s| s.is_empty() || s == "." || s == "..")
                || !path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
            {
                return Err("invalid workspace origin path".into());
            }
            if result.address.len() > 253
                || result.address.split('.').any(|part| {
                    part.is_empty()
                        || part.len() > 63
                        || part.starts_with('-')
                        || part.ends_with('-')
                        || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err("invalid workspace host address".into());
            }
            if result.address.split('.').count() == 4
                && result
                    .address
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.')
                && result.address.parse::<std::net::Ipv4Addr>().is_err()
            {
                return Err("invalid workspace IPv4 address".into());
            }
            if result.user.len() > 32
                || result.user.starts_with('-')
                || !result
                    .user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err("invalid workspace SSH user".into());
            }
            for author in [&result.author_name, &result.author_email] {
                if author.len() > 200 || author.trim() != author || author.contains(['<', '>']) {
                    return Err("invalid workspace author".into());
                }
            }
            Ok(result)
        }
        pub fn encode(&self) -> Vec<u8> {
            format!(
                "TDVM-CLONE-1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
                self.id,
                self.branch,
                self.commit,
                self.repository,
                self.address,
                self.port,
                self.user,
                self.host_key,
                self.guest_key,
                self.author_name,
                self.author_email
            )
            .into_bytes()
        }
        pub fn retention_ref(&self) -> String {
            format!("refs/td-vm/start/{}", self.id)
        }
        pub fn origin(&self) -> String {
            format!("ssh://{}@td-host{}", self.user, self.repository)
        }
    }
    pub fn branch_valid(branch: &str) -> bool {
        !branch.is_empty()
            && branch.len() <= 200
            && branch != "main"
            && branch != "HEAD"
            && !branch.starts_with("refs/")
            && !branch.contains("..")
            && branch.split('/').all(|part| {
                !part.is_empty()
                    && !part.starts_with(['.', '-'])
                    && !part.ends_with('.')
                    && !part.ends_with(".lock")
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
    }
    pub fn ready(plan: &Plan) -> Vec<u8> {
        [b"TDVM-CLONE-READY-1\n".as_slice(), plan.encode().as_slice()].concat()
    }
    pub fn parse_ready(bytes: &[u8], expected: &Plan) -> Result<(), String> {
        let plan = Plan::parse(
            bytes
                .strip_prefix(b"TDVM-CLONE-READY-1\n")
                .ok_or("workspace is not ready")?,
        )?;
        if plan != *expected {
            return Err("workspace reply differs from this request".into());
        }
        Ok(())
    }
    pub fn failure(plan: &Plan, error: &str) -> Vec<u8> {
        let message: String = error
            .chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect();
        [
            format!("TDVM-CLONE-FAILED-1\n{message}\n").as_bytes(),
            plan.encode().as_slice(),
        ]
        .concat()
    }
    pub fn status(bytes: &[u8], expected: &Plan) -> Result<(), String> {
        if let Some(rest) = bytes.strip_prefix(b"TDVM-CLONE-FAILED-1\n") {
            let text = std::str::from_utf8(rest).map_err(|_| "invalid workspace failure")?;
            let (message, encoded) = text.split_once('\n').ok_or("invalid workspace failure")?;
            if message.len() > 1024
                || message.chars().any(char::is_control)
                || Plan::parse(encoded.as_bytes())? != *expected
            {
                return Err("workspace failure differs from this request".into());
            }
            return Err(format!(
                "Previous guest clone attempt failed: {message}. A retry was requested; run clone again to inspect completion"
            ));
        }
        parse_ready(bytes, expected)
    }
    #[cfg(test)]
    pub fn example() -> Plan {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
        Plan { id: "0123456789abcdef0123456789abcdef".into(), branch: "task".into(), commit: "a".repeat(40),
            repository: "/srv/git/td.git".into(), address: "10.0.2.2".into(), port: 22, user: "test".into(),
            host_key: key.into(), guest_key: key.into(), author_name: "Fixture".into(), author_email: "fixture@example.invalid".into() }
    }
    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;
        #[test]
        fn plans_and_status_bind_every_field_and_refuse_injection() {
            let plan = example();
            let text = String::from_utf8(plan.encode()).unwrap();
            assert_eq!(Plan::parse(text.as_bytes()).unwrap(), plan);
            for bad in [
                text.replace("\ntask\n", "\nmain\n"), text.replace("\ntask\n", "\n--option\n"),
                text.replace("10.0.2.2", "host -oProxyCommand=bad"), text.replace("10.0.2.2", "999.0.0.1"),
                text.replace("/srv/git/td.git", "/srv/../repo"), text.replace("/srv/git/td.git", "/repo;command"),
                text.replace("\n22\n", "\n022\n"), text.replace("\n22\n", "\n0\n"),
                text.replace(&"a".repeat(40), &"0".repeat(40)), text.replace("Fixture", "Bad\rName"),
                format!("{text}extra\n"), text.trim_end().into(),
            ] { assert!(Plan::parse(bad.as_bytes()).is_err(), "{bad}"); }
            assert!(parse_ready(&ready(&plan), &plan).is_ok());
            let mut other = plan.clone(); other.branch = "other".into();
            assert!(parse_ready(&ready(&other), &plan).is_err());
            assert!(status(&failure(&plan, "fixed failure"), &plan).unwrap_err().contains("fixed failure"));
            assert!(status(&failure(&other, "fixed failure"), &plan).unwrap_err().contains("differs"));
        }
    }

}
