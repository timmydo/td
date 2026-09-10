//! Selector-owned SHA-256 PCR 11 measurement; no key release or PCR reset.
use crate::{invalid, read_bounded_real_file, sha256, valid_digest, MAX_CMDLINE_BYTES};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

pub const POLICY_PATH: &str = "etc/td/boot-measurement";
const POLICY: &[u8] = b"td-selector-pcr11-v1\n";
const DEVICE: &str = "/dev/tpmrm0";
const PCR_READ: &[u8] = &[
    0x80, 1, 0, 0, 0, 20, 0, 0, 1, 0x7e, // header
    0, 0, 0, 1, 0, 0x0b, 3, 0, 8, 0, // SHA-256 PCR 11
];
const EXTEND_REPLY: &[u8] = &[
    0x80, 2, 0, 0, 0, 19, 0, 0, 0, 0, // session response
    0, 0, 0, 0, 0, 0, 1, 0, 0, // no parameters; empty password response
];

pub fn enabled(rootfs: &Path) -> io::Result<bool> {
    let path = rootfs.join(POLICY_PATH);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
        Ok(_) => {}
    }
    let bytes = read_bounded_real_file(&path, "boot measurement policy", POLICY.len() as u64)?;
    if bytes != POLICY {
        return Err(invalid("unsupported boot measurement policy"));
    }
    Ok(true)
}

pub fn event_digest(deployment: &str, cmdline: &[u8]) -> io::Result<[u8; 32]> {
    if !valid_digest(deployment.as_bytes())
        || cmdline.len() >= MAX_CMDLINE_BYTES
        || !cmdline.iter().all(|b| matches!(b, b' '..=b'~'))
    {
        return Err(invalid("invalid measured deployment or boot arguments"));
    }
    let length = u32::try_from(cmdline.len()).map_err(|_| invalid("boot arguments too long"))?;
    let mut hash = sha256::Sha256::new();
    hash.update(b"td/selector-deployment/v1\0");
    hash.update(deployment.as_bytes());
    hash.update(&length.to_be_bytes());
    hash.update(cmdline);
    Ok(hash.finalize())
}

fn read_pcr(reply: &[u8]) -> io::Result<[u8; 32]> {
    // The update counter may vary. Every other field must name exactly one
    // SHA-256 digest for PCR 11, with no omitted or extra bank/selection.
    let header = [0x80, 1, 0, 0, 0, 62, 0, 0, 0, 0];
    let selection = [0, 0, 0, 1, 0, 0x0b, 3, 0, 8, 0, 0, 0, 0, 1, 0, 32];
    if reply.len() != 62
        || reply.get(..10) != Some(&header)
        || reply.get(14..30) != Some(&selection)
    {
        return Err(invalid("TPM refused or returned malformed SHA-256 PCR 11"));
    }
    reply
        .get(30..)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("missing PCR 11 digest"))
}

fn measure_with(
    digest: &[u8; 32],
    mut exchange: impl FnMut(&[u8]) -> io::Result<Vec<u8>>,
) -> io::Result<[u8; 32]> {
    if read_pcr(&exchange(PCR_READ)?)? != [0; 32] {
        return Err(invalid("PCR 11 already used; cold boot required"));
    }
    let mut command = vec![
        0x80, 2, 0, 0, 0, 65, 0, 0, 1, 0x82, // PCR_Extend
        0, 0, 0, 11, 0, 0, 0, 9, // handle, authorization size
        0x40, 0, 0, 9, 0, 0, 0, 0, 0, // empty password session
        0, 0, 0, 1, 0, 0x0b, // one SHA-256 digest
    ];
    command.extend_from_slice(digest);
    if exchange(&command)? != EXTEND_REPLY {
        return Err(invalid("TPM PCR 11 extension failed; cold boot required"));
    }
    let mut hash = sha256::Sha256::new();
    hash.update(&[0; 32]);
    hash.update(digest);
    let expected = hash.finalize();
    if read_pcr(&exchange(PCR_READ)?)? != expected {
        return Err(invalid("TPM PCR 11 readback mismatch; cold boot required"));
    }
    Ok(expected)
}

pub fn measure(deployment: &str, cmdline: &[u8]) -> io::Result<[u8; 32]> {
    let digest = event_digest(deployment, cmdline)?;
    let mut device = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0o400000)
        .open(DEVICE)
        .map_err(|e| io::Error::new(e.kind(), format!("open {DEVICE}: {e}")))?;
    if !device.metadata()?.file_type().is_char_device() {
        return Err(invalid("TPM resource manager must be a character device"));
    }
    measure_with(&digest, |command| {
        // One device write is one command. Never retry an uncertain extension.
        if device.write(command)? != command.len() {
            return Err(io::Error::other("short TPM command write"));
        }
        let mut reply = vec![0; 128];
        let size = device.read(&mut reply)?;
        reply.truncate(size);
        Ok(reply)
    })
    .map_err(|e| io::Error::new(e.kind(), format!("measure through {DEVICE}: {e}")))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn response(pcr: [u8; 32]) -> Vec<u8> {
        let mut bytes = vec![
            0x80, 1, 0, 0, 0, 62, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0x0b, 3, 0, 8, 0, 0, 0, 0,
            1, 0, 32,
        ];
        bytes.extend_from_slice(&pcr);
        bytes
    }

    #[test]
    fn event_binds_the_manifest_identity_and_exact_arguments() {
        let id = "a".repeat(64);
        let first = event_digest(&id, b"console=ttyS0").unwrap();
        assert_eq!(
            sha256::to_base16(&first),
            "4cc5b996bf66ef562d6493a106efff61bd2eef400d18fcab11a42e07328c08af"
        );
        assert_ne!(
            first,
            event_digest(&"b".repeat(64), b"console=ttyS0").unwrap()
        );
        assert_ne!(first, event_digest(&id, b"console=ttyS0 ").unwrap());
        assert!(event_digest(&id, b"a\0b").is_err());
        assert!(event_digest(&id, &vec![b'a'; MAX_CMDLINE_BYTES]).is_err());
        assert!(event_digest(&"A".repeat(64), b"a").is_err());
    }

    #[test]
    fn only_exact_pcr_selection_and_complete_replies_are_accepted() {
        let good = response([9; 32]);
        assert_eq!(read_pcr(&good).unwrap(), [9; 32]);
        for index in (0..10).chain(14..30) {
            let mut bad = good.clone();
            bad[index] ^= 1;
            assert!(read_pcr(&bad).is_err(), "field {index}");
        }
        for length in 0..good.len() {
            assert!(read_pcr(&good[..length]).is_err());
        }
        let mut extra = good;
        extra.push(0);
        assert!(read_pcr(&extra).is_err());
    }

    #[test]
    fn extension_requires_unused_pcr_and_verified_readback_without_retry() {
        let event = event_digest(&"a".repeat(64), b"console=ttyS0").unwrap();
        let mut hash = sha256::Sha256::new();
        hash.update(&[0; 32]);
        hash.update(&event);
        let expected = hash.finalize();
        for scenario in 0..5 {
            let mut calls = 0;
            let result = measure_with(&event, |command| {
                calls += 1;
                match calls {
                    1 => {
                        assert_eq!(command, PCR_READ);
                        Ok(response(if scenario == 1 { [1; 32] } else { [0; 32] }))
                    }
                    2 => {
                        assert_eq!(command.len(), 65);
                        assert_eq!(&command[10..14], &11u32.to_be_bytes());
                        assert_eq!(&command[33..], &event);
                        if scenario == 2 {
                            return Err(io::Error::other("lost reply"));
                        }
                        Ok(if scenario == 3 {
                            vec![0x80, 1]
                        } else {
                            EXTEND_REPLY.to_vec()
                        })
                    }
                    3 => {
                        assert_eq!(command, PCR_READ);
                        Ok(response(if scenario == 4 { [0; 32] } else { expected }))
                    }
                    _ => panic!("retried a TPM operation"),
                }
            });
            assert_eq!(result.is_ok(), scenario == 0);
            assert_eq!(
                calls,
                if scenario == 1 {
                    1
                } else if scenario == 2 || scenario == 3 {
                    2
                } else {
                    3
                }
            );
        }
    }
}
