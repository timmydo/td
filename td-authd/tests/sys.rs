#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use std::os::fd::{AsFd, IntoRawFd};

fn control(kind: i32, payload: &[u8]) -> Vec<u8> {
    let length = HEADER + payload.len();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&length.to_ne_bytes());
    bytes.extend_from_slice(&(SOL_SOCKET as i32).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize((length + 7) & !7, 0);
    bytes
}

#[test]
fn unexpected_rights_are_closed_after_an_unknown_record() {
    use std::io::Read;
    let (mut reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let fd = writer.into_raw_fd();
    let mut bytes = control(99, &[]);
    bytes.extend(control(SCM_RIGHTS, &fd.to_ne_bytes()));
    let (_, fds, valid) = harvest(&bytes);
    assert!(!valid && fds.is_empty());
    assert_eq!(reader.read(&mut [0u8]).unwrap(), 0);
    assert!(!harvest(&control(SCM_RIGHTS, &[])).2);
}

#[test]
fn credentials_and_pidfd_shapes_are_strict() {
    let credentials: Vec<u8> = [123i32, 1000, 1000]
        .into_iter()
        .flat_map(i32::to_ne_bytes)
        .collect();
    let (parsed, fds, valid) = harvest(&control(SCM_CREDENTIALS, &credentials));
    assert!(valid && fds.is_empty());
    assert_eq!(parsed.unwrap().pid, 123);
    let mut duplicate = control(SCM_CREDENTIALS, &credentials);
    duplicate.extend_from_slice(&duplicate.clone());
    assert!(!harvest(&duplicate).2);
    assert!(!harvest(&control(SCM_PIDFD, &[])).2);
    assert!(!harvest(&control(SCM_PIDFD, &(-3i32).to_ne_bytes())).2);
    assert!(!harvest(&control(SCM_CREDENTIALS, &[0; 11])).2);
    assert!(!harvest(&[0; HEADER]).2);
}

#[test]
fn receive_reports_real_sender_and_cloexec_pidfd() {
    use std::io::Write;
    let (left, mut right) = UnixStream::pair().unwrap();
    let creator = prepare(&left).unwrap();
    prepare(&right).unwrap();
    right.write_all(b"X").unwrap();
    let (count, sender) = receive(&left, &mut [0u8]).unwrap();
    assert_eq!(count, 1);
    assert_eq!(sender.credentials, creator);
    alive(sender.pidfd.as_fd()).unwrap();
    let info =
        std::fs::read_to_string(format!("/proc/self/fdinfo/{}", sender.pidfd.as_raw_fd())).unwrap();
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .unwrap();
    assert_ne!(u32::from_str_radix(flags, 8).unwrap() & 0o2000000, 0);
}

#[test]
fn receive_policy_closes_duplicate_truncated_and_overlong_descriptor_sets() {
    use std::io::Read;
    for (count, flags, control_len) in [(2, 0, 80), (1, MSG_CTRUNC, 56), (1, 0, CONTROL + 1)] {
        let mut readers = Vec::new();
        let mut bytes = control(
            SCM_CREDENTIALS,
            &[1i32, 0, 0]
                .into_iter()
                .flat_map(i32::to_ne_bytes)
                .collect::<Vec<_>>(),
        );
        for _ in 0..count {
            let (reader, writer) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
            readers.push(reader);
            let fd = writer.into_raw_fd();
            bytes.extend(control(SCM_PIDFD, &fd.to_ne_bytes()));
        }
        let records = harvest(&bytes);
        assert!(records.2);
        assert_eq!(records.1.len(), count);
        assert!(admit(1, control_len, flags, records).is_err());
        for mut reader in readers {
            assert_eq!(reader.read(&mut [0u8]).unwrap(), 0);
        }
    }
}
