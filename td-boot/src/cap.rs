//! The live selector's PCR 12 release cap through td-protector
//! (td-install/MEDIA.md "Live boot" step 0, ENCRYPTION.md's release order,
//! step 4). The installed selector's cap is its release's, not this one.
use std::io;
use td_protector::CapError;
use td_tpm::{Client, Device, Transport};

/// A cap outcome under which the live boot proceeds.
#[derive(Debug, PartialEq, Eq)]
pub enum Proceed {
    /// `/dev/tpmrm0` is absent: nothing can release, so nothing is capped.
    NoDevice,
    /// This boot extended PCR 12 and read back the exact value.
    Capped,
    /// PCR 12 was already non-zero, so release was already closed.
    AlreadyClosed,
    /// The TPM has no SHA-256 PCR bank, so no protector's policy can be met.
    NoSha256Bank,
}

impl Proceed {
    /// The console line for this outcome.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::NoDevice => "no TPM device (/dev/tpmrm0): PCR 12 release cap skipped",
            Self::Capped => "PCR 12 release cap closed",
            Self::AlreadyClosed => "PCR 12 already extended: release already closed",
            Self::NoSha256Bank => {
                "TPM has no SHA-256 PCR bank: nothing can release; PCR 12 release cap skipped"
            }
        }
    }
}

/// The cap over `device`, the result of opening the TPM. Only an absent
/// device or an absent SHA-256 bank skips it, since neither can release.
/// A device that is present and cannot be opened, or a cap
/// that is uncertain or mismatched, cannot show release closed: the error
/// is a refusal, on which the caller halts.
pub fn live_with<T: Transport>(device: io::Result<T>) -> Result<Proceed, String> {
    let transport = match device {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Proceed::NoDevice),
        Err(error) => {
            return Err(format!(
                "PCR 12 release cap uncertain; platform reset required: {error}"
            ))
        }
        Ok(transport) => transport,
    };
    match td_protector::cap(&mut Client::new(transport)) {
        Ok(()) => Ok(Proceed::Capped),
        Err(CapError::AlreadyClosed) => Ok(Proceed::AlreadyClosed),
        Err(CapError::NoSha256Bank) => Ok(Proceed::NoSha256Bank),
        Err(error @ (CapError::Uncertain(_) | CapError::Mismatch)) => Err(error.to_string()),
    }
}

/// The cap through the kernel's TPM resource manager.
pub fn live() -> Result<Proceed, String> {
    live_with(Device::open_io())
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
    use crate::sha256;
    use std::cell::Cell;

    /// TPM2_PCR_Read of SHA-256 PCR 12 alone.
    const PCR_READ: &[u8] = &[
        0x80, 1, 0, 0, 0, 20, 0, 0, 1, 0x7e, // header
        0, 0, 0, 1, 0, 0x0b, 3, 0, 0x10, 0, // SHA-256 PCR 12
    ];
    /// The empty password-session PCR_Extend reply.
    const EXTEND_REPLY: &[u8] = &[
        0x80, 2, 0, 0, 0, 19, 0, 0, 0, 0, // session response
        0, 0, 0, 0, 0, 0, 1, 0, 0, // no parameters; empty password response
    ];

    /// A PCR_Read reply carrying PCR 12 at `pcr`.
    fn read_reply(pcr: [u8; 32]) -> Vec<u8> {
        let mut bytes = vec![
            0x80, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0x0b, 3, 0, 0x10, 0, 0, 0,
            0, 1, 0, 32,
        ];
        bytes.extend_from_slice(&pcr);
        let size = u32::try_from(bytes.len()).unwrap().to_be_bytes();
        bytes[2..6].copy_from_slice(&size);
        bytes
    }

    fn capped() -> [u8; 32] {
        let mut hash = sha256::Sha256::new();
        hash.update(&[0; 32]);
        hash.update(&td_protector::cap_event());
        hash.finalize()
    }

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
        reply: impl FnMut(usize, &[u8]) -> Result<Vec<u8>, String>,
    ) -> (Result<Proceed, String>, usize) {
        let calls = Cell::new(0);
        let result = live_with(Ok(Script {
            calls: &calls,
            reply,
        }));
        (result, calls.get())
    }

    type Never = Script<'static, fn(usize, &[u8]) -> Result<Vec<u8>, String>>;

    #[test]
    fn an_absent_device_skips_the_cap() {
        let absent = io::Error::new(io::ErrorKind::NotFound, "open TPM resource manager");
        assert_eq!(live_with::<Never>(Err(absent)), Ok(Proceed::NoDevice));
    }

    #[test]
    fn a_device_that_will_not_open_refuses() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::Other,
        ] {
            let error = live_with::<Never>(Err(io::Error::new(kind, "open TPM"))).unwrap_err();
            assert!(error.contains("platform reset required"), "{error}");
        }
    }

    #[test]
    fn a_closed_cap_proceeds() {
        let (result, calls) = run(|call, command| match call {
            1 => {
                assert_eq!(command, PCR_READ);
                Ok(read_reply([0; 32]))
            }
            2 => {
                assert_eq!(&command[6..10], &[0, 0, 1, 0x82], "PCR_Extend");
                assert_eq!(&command[10..14], &[0, 0, 0, 12], "PCR 12");
                assert!(command.ends_with(&td_protector::cap_event()));
                Ok(EXTEND_REPLY.to_vec())
            }
            3 => {
                assert_eq!(command, PCR_READ);
                Ok(read_reply(capped()))
            }
            _ => panic!("retried a TPM operation"),
        });
        assert_eq!(result, Ok(Proceed::Capped));
        assert_eq!(calls, 3);
    }

    #[test]
    fn an_already_closed_cap_proceeds_without_extending() {
        let (result, calls) = run(|call, command| {
            assert_eq!((call, command), (1, PCR_READ));
            Ok(read_reply([1; 32]))
        });
        assert_eq!(result, Ok(Proceed::AlreadyClosed));
        assert_eq!(calls, 1);
    }

    /// A TPM with only a SHA-1 bank answers the SHA-256 selection emptied:
    /// nothing can release, so the live boot proceeds after one read.
    #[test]
    fn a_tpm_without_a_sha256_bank_proceeds_without_extending() {
        let (result, calls) = run(|call, command| {
            assert_eq!((call, command), (1, PCR_READ));
            // Through the selection's width, then no PCR selected and no
            // values.
            let mut reply = read_reply([0; 32]);
            reply.truncate(21);
            reply.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
            reply[5] = 28;
            Ok(reply)
        });
        assert_eq!(result, Ok(Proceed::NoSha256Bank));
        assert_eq!(calls, 1);
    }

    #[test]
    fn an_uncertain_or_mismatched_cap_refuses() {
        let lost = || Err::<Vec<u8>, String>("lost".into());
        let zero = || Ok(read_reply([0; 32]));
        let extended = || Ok(EXTEND_REPLY.to_vec());
        let refused = || Ok(vec![0x80, 1, 0, 0, 0, 10, 0, 0, 0x01, 0x01]);
        // Both tries of the first read lost; a lost extension; a refused
        // extension; a lost readback; a wrong readback. Only the first read
        // is tried twice.
        let scenarios: Vec<Vec<Result<Vec<u8>, String>>> = vec![
            vec![lost(), lost()],
            vec![zero(), lost()],
            vec![zero(), refused()],
            vec![zero(), extended(), lost()],
            vec![zero(), extended(), Ok(read_reply([2; 32]))],
        ];
        for (scenario, replies) in scenarios.into_iter().enumerate() {
            let count = replies.len();
            let mut replies = replies.into_iter();
            let (result, calls) = run(|_, _| replies.next().expect("retried a TPM operation"));
            let error = result.unwrap_err();
            assert!(error.contains("platform reset required"), "{error}");
            let expected = if scenario == 4 {
                td_protector::CapError::Mismatch.to_string()
            } else {
                "PCR 12 release cap uncertain".into()
            };
            assert!(error.starts_with(&expected), "{scenario}: {error}");
            assert_eq!(calls, count, "{scenario}");
        }
        // One lost first read is tried again, and the cap closes.
        let mut replies = vec![lost(), zero(), extended(), Ok(read_reply(capped()))].into_iter();
        let (result, calls) = run(|_, _| replies.next().expect("retried a TPM operation"));
        assert_eq!(result, Ok(Proceed::Capped));
        assert_eq!(calls, 4);
    }
}
