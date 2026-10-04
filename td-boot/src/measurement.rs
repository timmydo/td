//! Selector-owned SHA-256 PCR 11 measurement through the shared td-tpm
//! client; no key release or PCR reset.
use crate::{invalid, read_bounded_real_file, sha256, valid_digest, MAX_CMDLINE_BYTES};
use std::fs;
use std::io;
use std::path::Path;
use td_tpm::{Client, Device, Transport};

pub const POLICY_PATH: &str = "etc/td/boot-measurement";
const POLICY: &[u8] = b"td-selector-pcr11-v1\n";
const PCR: u8 = 11;

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

fn measure_with<T: Transport>(digest: &[u8; 32], client: &mut Client<T>) -> io::Result<[u8; 32]> {
    let unused = client
        .read_pcr(PCR)
        .map_err(|e| invalid(format!("read SHA-256 PCR 11: {e}")))?;
    if unused != [0; 32] {
        return Err(invalid("PCR 11 already used; cold boot required"));
    }
    // One extension, never retried: a refused or malformed reply leaves the
    // PCR state uncertain.
    client.extend_pcr(PCR, digest).map_err(|e| {
        invalid(format!(
            "TPM PCR 11 extension failed; cold boot required: {e}"
        ))
    })?;
    let mut hash = sha256::Sha256::new();
    hash.update(&[0; 32]);
    hash.update(digest);
    let expected = hash.finalize();
    // The extension was sent: a failed readback leaves PCR 11 uncertain.
    let measured = client.read_pcr(PCR).map_err(|e| {
        invalid(format!(
            "read back SHA-256 PCR 11 failed; cold boot required: {e}"
        ))
    })?;
    if measured != expected {
        return Err(invalid("TPM PCR 11 readback mismatch; cold boot required"));
    }
    Ok(expected)
}

pub fn measure(deployment: &str, cmdline: &[u8]) -> io::Result<[u8; 32]> {
    let digest = event_digest(deployment, cmdline)?;
    let device = Device::open_io()?;
    measure_with(&digest, &mut Client::new(device))
        .map_err(|e| io::Error::new(e.kind(), format!("measure through /dev/tpmrm0: {e}")))
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
    use std::cell::Cell;

    /// A PCR_Read reply whose one SHA-256 selection carries `select`.
    fn response_with(select: &[u8], pcr: [u8; 32]) -> Vec<u8> {
        let mut bytes = vec![
            0x80, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0x0b,
        ];
        bytes.push(u8::try_from(select.len()).unwrap());
        bytes.extend_from_slice(select);
        bytes.extend_from_slice(&[0, 0, 0, 1, 0, 32]);
        bytes.extend_from_slice(&pcr);
        let size = u32::try_from(bytes.len()).unwrap().to_be_bytes();
        bytes[2..6].copy_from_slice(&size);
        bytes
    }

    fn response(pcr: [u8; 32]) -> Vec<u8> {
        response_with(&[0, 8, 0], pcr)
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

    /// TPM2_PCR_Read of SHA-256 PCR 11 and the exact empty password-session
    /// PCR_Extend reply; td-tpm's own tests pin the same bytes.
    const PCR_READ: &[u8] = &[
        0x80, 1, 0, 0, 0, 20, 0, 0, 1, 0x7e, // header
        0, 0, 0, 1, 0, 0x0b, 3, 0, 8, 0, // SHA-256 PCR 11
    ];
    const EXTEND_REPLY: &[u8] = &[
        0x80, 2, 0, 0, 0, 19, 0, 0, 0, 0, // session response
        0, 0, 0, 0, 0, 0, 1, 0, 0, // no parameters; empty password response
    ];

    /// Answers each command from `reply`, numbered from one.
    struct Script<'a, F: FnMut(usize, &[u8]) -> Result<Vec<u8>, String>> {
        calls: &'a Cell<usize>,
        reply: F,
    }
    impl<F: FnMut(usize, &[u8]) -> Result<Vec<u8>, String>> Transport for Script<'_, F> {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            self.calls.set(self.calls.get() + 1);
            (self.reply)(self.calls.get(), command)
        }
    }

    fn run(
        event: &[u8; 32],
        reply: impl FnMut(usize, &[u8]) -> Result<Vec<u8>, String>,
    ) -> (io::Result<[u8; 32]>, usize) {
        let calls = Cell::new(0);
        let result = measure_with(
            event,
            &mut Client::new(Script {
                calls: &calls,
                reply,
            }),
        );
        (result, calls.get())
    }

    #[test]
    fn only_exact_pcr_selection_and_complete_replies_are_accepted() {
        let event = [7; 32];
        let good = response([0; 32]);
        // Bytes 10..14 are the update counter, which may vary.
        let mut bad: Vec<Vec<u8>> = (0..10)
            .chain(14..30)
            .map(|index| {
                let mut bad = good.clone();
                bad[index] ^= 1;
                bad
            })
            .collect();
        bad.extend((0..good.len()).map(|length| good[..length].to_vec()));
        let mut extra = good.clone();
        extra.push(0);
        bad.push(extra);
        for reply in bad {
            let (result, calls) = run(&event, |_, command| {
                assert_eq!(command, PCR_READ);
                Ok(reply.clone())
            });
            assert!(result
                .unwrap_err()
                .to_string()
                .starts_with("read SHA-256 PCR 11"));
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn a_four_byte_selection_needs_pcr_11_alone_and_a_zero_extra_byte() {
        let event = [7; 32];
        let mut hash = sha256::Sha256::new();
        hash.update(&[0; 32]);
        hash.update(&event);
        let expected = hash.finalize();
        let (result, calls) = run(&event, |call, _| {
            Ok(match call {
                1 => response_with(&[0, 8, 0, 0], [0; 32]),
                2 => EXTEND_REPLY.to_vec(),
                _ => response_with(&[0, 8, 0, 0], expected),
            })
        });
        assert_eq!(result.unwrap(), expected);
        assert_eq!(calls, 3);
        for select in [
            [0, 8, 0, 1],
            [0, 8, 1, 0],
            [0, 0x10, 0, 0],
            [0, 0x18, 0, 0],
            [8, 0, 0, 0],
        ] {
            let (result, calls) = run(&event, |_, _| Ok(response_with(&select, [0; 32])));
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .starts_with("read SHA-256 PCR 11"),
                "{select:?}"
            );
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn extension_requires_unused_pcr_and_verified_readback_without_retry() {
        let event = event_digest(&"a".repeat(64), b"console=ttyS0").unwrap();
        let mut hash = sha256::Sha256::new();
        hash.update(&[0; 32]);
        hash.update(&event);
        let expected = hash.finalize();
        let mut extend = vec![
            0x80, 2, 0, 0, 0, 65, 0, 0, 1, 0x82, // PCR_Extend
            0, 0, 0, 11, 0, 0, 0, 9, // handle, authorization size
            0x40, 0, 0, 9, 0, 0, 0, 0, 0, // empty password session
            0, 0, 0, 1, 0, 0x0b, // one SHA-256 digest
        ];
        extend.extend_from_slice(&event);
        for scenario in 0..6 {
            let (result, calls) = run(&event, |call, command| match call {
                1 => {
                    assert_eq!(command, PCR_READ);
                    Ok(response(if scenario == 1 { [1; 32] } else { [0; 32] }))
                }
                2 => {
                    assert_eq!(command, extend);
                    if scenario == 2 {
                        return Err("lost reply".into());
                    }
                    Ok(if scenario == 3 {
                        vec![0x80, 1]
                    } else {
                        EXTEND_REPLY.to_vec()
                    })
                }
                3 => {
                    assert_eq!(command, PCR_READ);
                    if scenario == 5 {
                        return Err("lost readback".into());
                    }
                    Ok(response(if scenario == 4 { [0; 32] } else { expected }))
                }
                _ => panic!("retried a TPM operation"),
            });
            assert_eq!(result.is_ok(), scenario == 0, "scenario {scenario}");
            match result {
                Ok(pcr) => assert_eq!(pcr, expected),
                Err(e) if scenario == 1 => {
                    assert_eq!(e.to_string(), "PCR 11 already used; cold boot required")
                }
                Err(e) if scenario == 5 => assert_eq!(
                    e.to_string(),
                    "read back SHA-256 PCR 11 failed; cold boot required: lost readback"
                ),
                Err(e) => assert!(
                    e.to_string().contains("; cold boot required"),
                    "scenario {scenario}: {e}"
                ),
            }
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
