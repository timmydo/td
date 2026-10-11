//! The selector's FIDO2 client (td-install/ENCRYPTION.md, "FIDO2
//! protectors"): td-fido's disk flows over root hidraw admission, each
//! session's HID worker this program's `fido-worker` verb, started from
//! `/proc/self/exe` as td-secret starts its `hid-worker`. One operation
//! runs under one two-minute absolute deadline fixed when it begins, on
//! exactly one connected key. Linked in and run by nothing until 8c.

use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};
use td_fido::fido_device::{Device, Program, Session, MAX_LIFETIME};
use td_fido::fido_disk::{self, Created, DiskError, Operation, Person, Phase, Released, Token};
use td_fido::fido_transaction::Devices;

/// The verb that serves a session's HID worker.
pub(crate) const VERB: &str = "fido-worker";

/// td-boot as its own HID worker. The selector discovers root-admitted
/// nodes only, so the desktop verb is never built into a worker's argv,
/// and td-boot does not dispatch it.
#[allow(dead_code, reason = "a type-level Program, never a value")]
pub(crate) struct Worker;
impl Program for Worker {
    const PATH: &'static str = "/proc/self/exe";
    const ROOT: &'static str = VERB;
    const DESKTOP: &'static str = "fido-worker-desktop";
}

/// Serves one session's HID worker: td-fido's root worker, which
/// re-admits the node by index, inode and device number under its own
/// two-minute watchdog.
pub(crate) fn worker(index: &str, inode: &str, rdev: &str) -> Result<(), String> {
    td_fido::fido_device::worker(index, inode, rdev)
}

/// The root USB transport: root-owned mode-0600 `/dev/hidrawN` nodes.
#[allow(dead_code, reason = "the selector's FIDO2 release calls it from 8c")]
pub(crate) struct Root;
impl Devices for Root {
    type Node = Device;
    type Channel = Session<Worker>;
    fn discover(&mut self) -> Result<Vec<Device>, String> {
        Device::discover()
    }
    fn open(&mut self, node: Device, deadline: Instant) -> Result<Session<Worker>, String> {
        Session::open(node, deadline)
    }
    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}

/// One selector operation's token access and its absolute deadline.
#[allow(dead_code, reason = "the selector's FIDO2 release calls it from 8c")]
pub(crate) struct Client<D: Devices> {
    devices: D,
    deadline: Instant,
}

#[allow(dead_code, reason = "the selector's FIDO2 release calls it from 8c")]
impl Client<Root> {
    pub(crate) fn root() -> Self {
        Self::new(Root, Instant::now())
    }
}

#[allow(dead_code, reason = "the selector's FIDO2 release calls it from 8c")]
impl<D: Devices> Client<D> {
    fn new(devices: D, start: Instant) -> Self {
        Self {
            devices,
            deadline: start + MAX_LIFETIME,
        }
    }

    /// An unlock, or a protector change's authorization.
    pub(crate) fn release(
        &mut self,
        tokens: &[Token<'_>],
        operation: Operation<'_>,
        phase: Phase,
        person: &mut impl Person,
    ) -> Result<Released, DiskError> {
        fido_disk::release(
            &mut self.devices,
            self.deadline,
            tokens,
            operation,
            phase,
            person,
            &mut entropy,
        )
    }

    /// A new fido2 token's credential, excluding the header's.
    pub(crate) fn create(
        &mut self,
        excluded: &[&[u8]],
        operation: Operation<'_>,
        person: &mut impl Person,
    ) -> Result<Created, DiskError> {
        fido_disk::create(
            &mut self.devices,
            self.deadline,
            excluded,
            operation,
            person,
            &mut entropy,
        )
    }
}

/// Fresh kernel randomness for each client-data hash, salt and the PIN
/// protocol's ephemeral P-256 scalar. `/dev/random`, as td-tpm's
/// ephemeral scalar reads it: in early boot it blocks until the kernel's
/// generator is initialized, where the non-blocking device would not.
#[allow(dead_code, reason = "the selector's FIDO2 release calls it from 8c")]
fn entropy(out: &mut [u8]) -> Result<(), String> {
    File::open("/dev/random")
        .and_then(|mut random| random.read_exact(out))
        .map_err(|_| "read kernel entropy".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::rc::Rc;
    use td_fido::fido_device::Interruption;
    use td_fido::fido_hid::{self as hid, Event, Message};
    use td_fido::fido_pin::{LoginRefusal, Pin};
    use td_fido::fido_transaction::{Channel, Error};

    /// One scripted key: the CTAP replies it gives, in order, and every
    /// request it saw.
    #[derive(Default)]
    struct Script {
        replies: VecDeque<Vec<u8>>,
        requests: Vec<Vec<u8>>,
    }

    struct Scripted {
        script: Rc<RefCell<Script>>,
        deadline: Instant,
    }

    impl Channel for Scripted {
        fn check(&self) -> Result<(), Interruption> {
            if Instant::now() >= self.deadline {
                return Err(Interruption::Expired);
            }
            Ok(())
        }
        fn exchange(&mut self, request: &[u8]) -> Result<Message, String> {
            let mut script = self.script.borrow_mut();
            script.requests.push(request.to_vec());
            let reply = script.replies.pop_front().ok_or("script ended")?;
            message(&reply)
        }
    }

    /// The device nodes connected, and every deadline a session opened under.
    struct Transport {
        connected: Vec<u8>,
        script: Rc<RefCell<Script>>,
        deadlines: Vec<Instant>,
    }

    impl Devices for Transport {
        type Node = u8;
        type Channel = Scripted;
        fn discover(&mut self) -> Result<Vec<u8>, String> {
            Ok(self.connected.clone())
        }
        fn open(&mut self, _: u8, deadline: Instant) -> Result<Scripted, String> {
            self.deadlines.push(deadline);
            Ok(Scripted {
                script: Rc::clone(&self.script),
                deadline,
            })
        }
        fn pause(&mut self, _: Duration) {}
    }

    struct Console(usize);
    impl Person for Console {
        fn pin(&mut self, _: Phase, _: u8) -> Result<Pin, String> {
            self.0 += 1;
            Pin::new(b"4321".as_slice().into())
        }
        fn touch(&mut self, _: Phase) {}
    }

    fn message(bytes: &[u8]) -> Result<Message, String> {
        let mut decoder = hid::Decoder::cbor(1)?;
        for report in hid::cbor(1, bytes)?.as_ref() {
            if let Event::Complete(message) = decoder.push(report)? {
                return Ok(message);
            }
        }
        Err("incomplete scripted message".into())
    }

    /// getInfo: status 0, then {1: [version], 2: ["hmac-secret"], 3: AAGUID,
    /// 4: {"clientPin": true, "rk": false}, 6: [2]}.
    fn info(version: &str) -> Vec<u8> {
        let mut reply = vec![0, 0xa5, 0x01, 0x81, 0x60 + version.len() as u8];
        reply.extend_from_slice(version.as_bytes());
        reply.extend_from_slice(b"\x02\x81\x6bhmac-secret\x03\x50");
        reply.extend_from_slice(&[0x5a; 16]);
        reply.extend_from_slice(b"\x04\xa2\x62rk\xf4\x69clientPin\xf5");
        reply.extend_from_slice(b"\x06\x81\x02");
        reply
    }

    fn transport(connected: &[u8], replies: &[Vec<u8>]) -> Transport {
        Transport {
            connected: connected.to_vec(),
            script: Rc::new(RefCell::new(Script {
                replies: replies.iter().cloned().collect(),
                requests: Vec::new(),
            })),
            deadlines: Vec::new(),
        }
    }

    const OPERATION: Operation<'static> = Operation {
        volume: [7; 16],
        description: b"unlock td volume",
    };

    fn token(credential: &[u8]) -> Token<'_> {
        Token {
            credential,
            key: td_fido::fido_p256::PublicKey::from_coordinates(
                &[
                    0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63,
                    0xa4, 0x40, 0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1,
                    0x39, 0x45, 0xd8, 0x98, 0xc2, 0x96,
                ],
                &[
                    0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c,
                    0x0f, 0x9e, 0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6,
                    0x40, 0x68, 0x37, 0xbf, 0x51, 0xf5,
                ],
            )
            .unwrap(),
            salt: [9; 32],
        }
    }

    #[test]
    fn the_worker_verb_is_this_program_under_root_admission() {
        assert_eq!(Worker::PATH, "/proc/self/exe");
        assert_eq!(Worker::ROOT, "fido-worker");
        // main.rs routes exactly this verb to td-fido's root worker, and
        // nothing routes the desktop one.
        let main = include_str!("main.rs");
        assert!(main.contains("Mode::FidoWorker { index, inode, rdev } => {\n            fido::worker(&index, &inode, &rdev)"));
        assert!(!main.contains(Worker::DESKTOP));
        let parse = |argv: &[&str]| crate::parse_args(argv.iter().map(OsString::from));
        assert!(matches!(
            parse(&["fido-worker", "3", "812", "62914563"]),
            Ok(crate::Mode::FidoWorker { index, inode, rdev })
                if index == "3" && inode == "812" && rdev == "62914563"
        ));
        for argv in [
            &["fido-worker", "3", "812"][..],
            &["fido-worker", "3", "812", "62914563", "extra"],
            &[Worker::DESKTOP, "3", "812", "62914563", "/run/user/0"],
        ] {
            assert!(parse(argv).is_err(), "{argv:?}");
        }
        let source = include_str!("fido.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(source.contains("    td_fido::fido_device::worker(index, inode, rdev)\n"));
        assert!(!source.contains("desktop_worker") && !source.contains("discover_desktop"));
        // Entropy, the PIN protocol's ephemeral scalar's among it, is the
        // blocking generator, which early boot cannot read uninitialized.
        assert!(source.contains("    File::open(\"/dev/random\")\n"));
        assert!(!source.contains("urandom"));
    }

    #[test]
    fn none_or_two_keys_are_refused_before_any_session() {
        for connected in [&[][..], &[1, 2][..]] {
            let mut client = Client::new(transport(connected, &[]), Instant::now());
            let mut console = Console(0);
            assert_eq!(
                client
                    .release(
                        &[token(b"credential")],
                        OPERATION,
                        Phase::Unlock,
                        &mut console
                    )
                    .err(),
                Some(DiskError::OneKey)
            );
            assert_eq!(
                client.create(&[], OPERATION, &mut console).err(),
                Some(DiskError::OneKey)
            );
            assert!(client.devices.deadlines.is_empty());
            assert_eq!(console.0, 0);
        }
    }

    #[test]
    fn one_key_meets_the_disk_flow_over_the_transport() {
        // A key reporting only CTAP 2.0: one getInfo over the one session,
        // refused before any PIN.
        let mut client = Client::new(transport(&[1], &[info("FIDO_2_0")]), Instant::now());
        let mut console = Console(0);
        assert_eq!(
            client
                .release(
                    &[token(b"credential")],
                    OPERATION,
                    Phase::Unlock,
                    &mut console
                )
                .err(),
            Some(DiskError::Refused(LoginRefusal::NoFido21))
        );
        assert_eq!(client.devices.script.borrow().requests, [vec![4]]);
        assert_eq!(console.0, 0);
        // A CTAP 2.1 key holding none of the tokens: getInfo, then the
        // silent identify, answered NO_CREDENTIALS.
        let mut client = Client::new(
            transport(&[1], &[info("FIDO_2_1"), vec![0x2e]]),
            Instant::now(),
        );
        assert_eq!(
            client
                .release(
                    &[token(b"credential")],
                    OPERATION,
                    Phase::Unlock,
                    &mut console
                )
                .err(),
            Some(DiskError::NotEnrolled)
        );
        let requests = client.devices.script.borrow().requests.clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1][0], 2);
        assert!(requests[1]
            .windows(10)
            .any(|window| window == b"credential"));
        assert_eq!(console.0, 0);
    }

    #[test]
    fn one_absolute_deadline_bounds_the_whole_operation() {
        let start = Instant::now();
        let mut client = Client::new(transport(&[1], &[info("FIDO_2_1"), vec![0x2e]]), start);
        let _ = client.release(
            &[token(b"credential")],
            OPERATION,
            Phase::Unlock,
            &mut Console(0),
        );
        assert_eq!(client.devices.deadlines, [start + MAX_LIFETIME]);
        assert_eq!(MAX_LIFETIME, Duration::from_secs(120));
        // An operation begun two minutes ago opens nothing.
        let past = Instant::now().checked_sub(MAX_LIFETIME).unwrap();
        let mut client = Client::new(transport(&[1], &[info("FIDO_2_1")]), past);
        assert_eq!(
            client
                .release(
                    &[token(b"credential")],
                    OPERATION,
                    Phase::Unlock,
                    &mut Console(0)
                )
                .err(),
            Some(DiskError::Failed(Error::Interrupted(Interruption::Expired)))
        );
        assert!(client.devices.deadlines.is_empty());
    }
}
