use crate::sys;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const PROBE_ARG: &str = "--probe-transition";
const STAGE2_ARG: &str = "--internal-stage-2";
pub const TRANSITION_MARKER: &str = "TD-JAIL-TRANSITION-OK";
const STAGE2_MARKER: &str = "TD-JAIL-STAGE2-OK";
const TOKEN_LEN: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    uid: u32,
    gid: u32,
}

#[derive(Debug, Eq, PartialEq)]
struct NamespaceSnapshot {
    user: PathBuf,
    mount: PathBuf,
    pid: PathBuf,
    uts: PathBuf,
    network: PathBuf,
}

impl NamespaceSnapshot {
    fn read() -> io::Result<Self> {
        Ok(Self {
            user: fs::read_link("/proc/self/ns/user")?,
            mount: fs::read_link("/proc/self/ns/mnt")?,
            pid: fs::read_link("/proc/self/ns/pid")?,
            uts: fs::read_link("/proc/self/ns/uts")?,
            network: fs::read_link("/proc/self/ns/net")?,
        })
    }

    fn require_all_changed(&self, before: &Self) -> io::Result<()> {
        for (name, old, new) in [
            ("user", &before.user, &self.user),
            ("mount", &before.mount, &self.mount),
            ("uts", &before.uts, &self.uts),
            ("network", &before.network, &self.network),
        ] {
            if old == new {
                return Err(io::Error::other(format!(
                    "unshare reported success but the {name} namespace did not change"
                )));
            }
        }
        Ok(())
    }
}

fn require_child_pid_namespace_changed(before: &NamespaceSnapshot, child: u32) -> io::Result<()> {
    let path = format!("/proc/{child}/ns/pid");
    let current = fs::read_link(&path)
        .map_err(|e| io::Error::other(format!("read stage-2 PID namespace at {path}: {e}")))?;
    if current == before.pid {
        return Err(io::Error::other(
            "stage 2 remained in stage 1's PID namespace",
        ));
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub enum Mode {
    Probe,
    Stage2 {
        token: [u8; TOKEN_LEN],
        identity: Identity,
    },
}

pub fn parse_mode<I>(mut args: I) -> io::Result<Mode>
where
    I: Iterator<Item = OsString>,
{
    let mode = args.next().ok_or_else(usage_error)?;
    if mode == PROBE_ARG {
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::Probe);
    }
    if mode == STAGE2_ARG {
        let encoded = args.next().ok_or_else(usage_error)?;
        let uid = parse_id(args.next(), "uid")?;
        let gid = parse_id(args.next(), "gid")?;
        if args.next().is_some() {
            return Err(usage_error());
        }
        return Ok(Mode::Stage2 {
            token: decode_token(&encoded)?,
            identity: Identity { uid, gid },
        });
    }
    Err(usage_error())
}

fn parse_id(value: Option<OsString>, name: &str) -> io::Result<u32> {
    value
        .ok_or_else(usage_error)?
        .to_str()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("stage-2 {name} is not UTF-8"),
            )
        })?
        .parse()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stage-2 {name}: {e}"),
            )
        })
}

fn usage_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "application launch is disabled until td-jail confinement is complete; only --probe-transition is available",
    )
}

fn current_identity() -> io::Result<Identity> {
    let status = fs::read_to_string("/proc/self/status")?;
    Ok(Identity {
        uid: effective_id(&status, "Uid:")?,
        gid: effective_id(&status, "Gid:")?,
    })
}

fn effective_capabilities(status: &str) -> io::Result<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .ok_or_else(|| io::Error::other("/proc/self/status has no CapEff row"))?
        .trim();
    u64::from_str_radix(value, 16)
        .map_err(|e| io::Error::other(format!("invalid /proc/self/status CapEff: {e}")))
}

fn effective_id(status: &str, key: &str) -> io::Result<u32> {
    let fields = status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .ok_or_else(|| io::Error::other(format!("/proc/self/status has no {key} row")))?;
    fields
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::other(format!("/proc/self/status {key} has no effective id")))?
        .parse()
        .map_err(|e| io::Error::other(format!("invalid /proc/self/status {key}: {e}")))
}

fn install_identity_maps(identity: Identity) -> io::Result<()> {
    fs::write("/proc/self/setgroups", "deny\n")?;
    fs::write(
        "/proc/self/uid_map",
        format!("{} {} 1\n", identity.uid, identity.uid),
    )?;
    fs::write(
        "/proc/self/gid_map",
        format!("{} {} 1\n", identity.gid, identity.gid),
    )?;
    require_single_map("/proc/self/uid_map", identity.uid)?;
    require_single_map("/proc/self/gid_map", identity.gid)
}

fn require_single_map(path: &str, id: u32) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut rows = text.lines().filter(|line| !line.trim().is_empty());
    let row = rows
        .next()
        .ok_or_else(|| io::Error::other(format!("{path} is empty after write")))?;
    if rows.next().is_some() {
        return Err(io::Error::other(format!(
            "{path} contains more than the one identity mapping td-jail wrote"
        )));
    }
    let values = row
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::other(format!("invalid {path} readback: {e}")))?;
    if values.as_slice() != [id, id, 1] {
        return Err(io::Error::other(format!(
            "{path} readback does not match the identity map td-jail wrote"
        )));
    }
    Ok(())
}

fn random_token() -> io::Result<[u8; TOKEN_LEN]> {
    let mut token = [0_u8; TOKEN_LEN];
    fs::File::open("/dev/urandom")?.read_exact(&mut token)?;
    Ok(token)
}

fn encode_token(token: &[u8; TOKEN_LEN]) -> String {
    let mut encoded = String::with_capacity(TOKEN_LEN * 2);
    for byte in token {
        encoded.push(encode_nibble(byte >> 4));
        encoded.push(encode_nibble(byte & 0x0f));
    }
    encoded
}

fn encode_nibble(nibble: u8) -> char {
    match nibble {
        0..=9 => char::from(b'0' + nibble),
        _ => char::from(b'a' + nibble - 10),
    }
}

fn decode_token(encoded: &OsString) -> io::Result<[u8; TOKEN_LEN]> {
    let bytes = encoded
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stage-2 token is not UTF-8"))?
        .as_bytes();
    if bytes.len() != TOKEN_LEN * 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stage-2 token has the wrong length",
        ));
    }
    let mut token = [0_u8; TOKEN_LEN];
    for (index, slot) in token.iter_mut().enumerate() {
        let offset = index * 2;
        let high = decode_nibble(bytes.get(offset).copied())?;
        let low = decode_nibble(bytes.get(offset + 1).copied())?;
        *slot = (high << 4) | low;
    }
    Ok(token)
}

fn decode_nibble(byte: Option<u8>) -> io::Result<u8> {
    match byte {
        Some(value @ b'0'..=b'9') => Ok(value - b'0'),
        Some(value @ b'a'..=b'f') => Ok(value - b'a' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stage-2 token is not lowercase hexadecimal",
        )),
    }
}

fn tokens_equal(left: &[u8; TOKEN_LEN], right: &[u8; TOKEN_LEN]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (a, b)| different | (a ^ b))
        == 0
}

pub fn probe_transition() -> io::Result<()> {
    let identity = current_identity()?;
    let before = NamespaceSnapshot::read()?;
    let token = random_token()?;

    sys::unshare_namespaces(true)?;
    install_identity_maps(identity)?;
    NamespaceSnapshot::read()?.require_all_changed(&before)?;

    let (proof_reader, mut proof_writer) = io::pipe()?;
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg(STAGE2_ARG)
        .arg(encode_token(&token))
        .arg(identity.uid.to_string())
        .arg(identity.gid.to_string())
        .stdin(Stdio::from(proof_reader))
        .stdout(Stdio::piped())
        .spawn()?;

    if let Err(error) = require_child_pid_namespace_changed(&before, child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    if let Err(error) = proof_writer.write_all(&token) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let expected = format!("{STAGE2_MARKER} pid=1\n");
    let mut output = String::new();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("stage-2 stdout pipe was not created"))?;
    if let Err(error) = stdout
        .take(u64::try_from(expected.len()).unwrap_or(u64::MAX) + 1)
        .read_to_string(&mut output)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = child.wait()?;
    drop(proof_writer);
    if !status.success() {
        return Err(io::Error::other(format!(
            "stage 2 refused the namespace transition: {status}"
        )));
    }
    if output != expected {
        return Err(io::Error::other(format!(
            "stage 2 returned an unexpected transition response: {output:?}"
        )));
    }
    writeln!(io::stdout(), "{TRANSITION_MARKER} pid=1")
}

pub fn run_stage2(expected: [u8; TOKEN_LEN], expected_identity: Identity) -> io::Result<()> {
    if std::process::id() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "internal stage 2 is not PID 1 of a new namespace",
        ));
    }

    let mut actual = [0_u8; TOKEN_LEN];
    let mut stdin = io::stdin().lock();
    stdin.read_exact(&mut actual)?;
    if !tokens_equal(&actual, &expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "internal stage-2 proof does not match",
        ));
    }

    let status = fs::read_to_string("/proc/self/status")?;
    let identity = Identity {
        uid: effective_id(&status, "Uid:")?,
        gid: effective_id(&status, "Gid:")?,
    };
    if identity != expected_identity {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage-2 credentials do not match stage 1",
        ));
    }
    require_single_map("/proc/self/uid_map", identity.uid)?;
    require_single_map("/proc/self/gid_map", identity.gid)?;
    if identity.uid != 0 && effective_capabilities(&status)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage 2 retained effective capabilities after exec",
        ));
    }
    writeln!(io::stdout(), "{STAGE2_MARKER} pid=1")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]

    use super::*;

    fn args(values: &[&str]) -> std::vec::IntoIter<OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn public_mode_is_only_the_transition_probe() {
        assert_eq!(parse_mode(args(&[PROBE_ARG])).unwrap(), Mode::Probe);
        assert!(parse_mode(args(&[])).is_err());
        assert!(parse_mode(args(&["firefox"])).is_err());
        assert!(parse_mode(args(&[PROBE_ARG, "extra"])).is_err());
    }

    #[test]
    fn stage2_token_round_trips_and_is_strict() {
        let mut token = [0_u8; TOKEN_LEN];
        for (index, byte) in token.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        let encoded = encode_token(&token);
        assert_eq!(decode_token(&OsString::from(&encoded)).unwrap(), token);
        assert!(decode_token(&OsString::from("00")).is_err());
        assert!(decode_token(&OsString::from("G0".repeat(TOKEN_LEN))).is_err());
        assert!(parse_mode(args(&[STAGE2_ARG, &encoded, "1000", "1000"])).is_ok());
        assert!(parse_mode(args(&[STAGE2_ARG, &encoded])).is_err());
        assert!(parse_mode(args(&[STAGE2_ARG, &encoded, "1000", "1000", "extra"])).is_err());
    }

    #[test]
    fn status_parser_uses_the_effective_column() {
        let status = "Name:\ttd-jail\nUid:\t1000\t1001\t1002\t1003\nGid:\t10\t11\t12\t13\nCapEff:\t0000000000200000\n";
        assert_eq!(effective_id(status, "Uid:").unwrap(), 1001);
        assert_eq!(effective_id(status, "Gid:").unwrap(), 11);
        assert_eq!(effective_capabilities(status).unwrap(), 1 << 21);
        assert!(effective_id(status, "Groups:").is_err());
        assert!(effective_capabilities("Name:\ttd-jail\n").is_err());
    }

    #[test]
    fn proof_comparison_checks_every_byte() {
        let token = [7_u8; TOKEN_LEN];
        assert!(tokens_equal(&token, &token));
        let mut changed = token;
        changed[TOKEN_LEN - 1] = 8;
        assert!(!tokens_equal(&token, &changed));
    }
}
