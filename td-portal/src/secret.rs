//! td.Secret1 returns credential bytes, not the upstream Secret master key.

use super::*;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const LIMIT: usize = 16;
const PER_OWNER: usize = 4;
pub(super) struct Receipt {
    pub owner: String,
    app: String,
    name: String,
    deadline: Instant,
}

const DENIED: &str = "org.freedesktop.portal.Error.NotAllowed";

pub(super) struct Pending {
    pub owner: String,
    name: String,
    serial: u32,
    endian: Endian,
    deadline: Instant,
}

pub(super) fn is_call(call: &Message<'_>) -> bool {
    call.kind == MessageType::MethodCall
        && call.fields.path == Some(PORTAL_PATH)
        && call.fields.interface == Some(SECRET_INTERFACE)
        && call.fields.member == Some("Retrieve")
}

fn error(connection: &mut Connection, pending: &Pending, name: &str, text: &str) -> io::Result<()> {
    let serial = connection.next_serial()?;
    connection.write_frame(&method_error(
        pending.endian,
        serial,
        pending.serial,
        &pending.owner,
        name,
        text,
    )?)
}

pub(super) fn begin(
    connection: &mut Connection,
    state: &mut ServiceState,
    call: &Message<'_>,
) -> io::Result<()> {
    if call.flags & message::FLAG_NO_REPLY_EXPECTED != 0 {
        return Ok(());
    }
    let owner = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("Retrieve has no broker sender"))?;
    let name = match call.args() {
        [Value::Str(name)] if exact_signature(call, "s") && secret_store::valid_name(name) => {
            Some(*name)
        }
        _ => None,
    };
    let pending = Pending {
        owner: owner.into(),
        name: name.unwrap_or_default().into(),
        serial: call.serial,
        endian: call.endian,
        deadline: Instant::now() + EXCHANGE_TIMEOUT,
    };
    if name.is_none() {
        return error(
            connection,
            &pending,
            "org.freedesktop.DBus.Error.InvalidArgs",
            "Retrieve takes one credential name",
        );
    }
    if state.secrets.len() + state.secret_receipts.len() >= LIMIT
        || state.secrets.values().filter(|p| p.owner == owner).count()
            + state
                .secret_receipts
                .values()
                .filter(|p| p.owner == owner)
                .count()
            >= PER_OWNER
    {
        return error(
            connection,
            &pending,
            "org.freedesktop.DBus.Error.LimitsExceeded",
            "too many credential requests",
        );
    }
    let serial = connection.next_serial()?;
    let query = credentials_query(serial, owner)?;
    state.secrets.insert(serial, pending);
    connection.write_frame(&query)
}

pub(super) fn identity_reply(
    connection: &mut Connection,
    state: &mut ServiceState,
    reply: &Message<'_>,
) -> io::Result<bool> {
    let Some(serial) = reply.fields.reply_serial else {
        return Ok(false);
    };
    if !state.secrets.contains_key(&serial) {
        return Ok(false);
    }
    if !matches!(reply.kind, MessageType::MethodReturn | MessageType::Error)
        || reply.fields.sender != Some(BUS_NAME)
        || reply.fields.destination != connection.unique.as_deref()
    {
        return Ok(true);
    }
    let pending = state
        .secrets
        .remove(&serial)
        .ok_or_else(|| io::Error::other("credential request disappeared"))?;
    let app = credentials_app_id(reply)
        .ok()
        .flatten()
        .filter(|app| secret_store::valid_name(app));
    let Some(app) = app.filter(|_| Instant::now() < pending.deadline) else {
        error(
            connection,
            &pending,
            DENIED,
            "credential caller is not an authenticated application",
        )?;
        return Ok(true);
    };
    let result = secret_store::Store::open(&secret_store::user_path(UI_UID), UI_UID, false)
        .and_then(|store| store.get(&app, &pending.name))
        .and_then(|secret| secret.ok_or_else(|| "credential is not provisioned".into()));
    let mut secret = match result {
        Ok(secret) => secret,
        Err(_) => {
            error(
                connection,
                &pending,
                "org.freedesktop.portal.Error.Failed",
                "credential is unavailable",
            )?;
            return Ok(true);
        }
    };
    let file = credential_file(&secret);
    secret.fill(0);
    let (file, token) = match file {
        Ok(file) => file,
        Err(_) => {
            error(
                connection,
                &pending,
                "org.freedesktop.portal.Error.Failed",
                "credential descriptor is unavailable",
            )?;
            return Ok(true);
        }
    };
    state.secret_receipts.insert(
        token.clone(),
        Receipt {
            owner: pending.owner.clone(),
            app,
            name: pending.name.clone(),
            deadline: Instant::now() + EXCHANGE_TIMEOUT,
        },
    );
    let serial = connection.next_serial()?;
    let frame = message::Builder::method_return(pending.endian, pending.serial)
        .destination(&pending.owner)
        .unix_fds(1)
        .body("hs", |writer| {
            writer.uint32(0);
            writer.string(&token)
        })
        .map_err(wire_error)?
        .serial(serial)
        .encode()
        .map_err(message_error)?;
    connection
        .stream
        .set_write_timeout(Some(EXCHANGE_TIMEOUT))?;
    let sent = sys::send_with_fd(&connection.stream, &frame, file.as_raw_fd());
    connection.stream.set_write_timeout(None)?;
    sent?;
    Ok(true)
}

fn credential_file(secret: &[u8]) -> io::Result<(File, String)> {
    let mut nonce = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let name = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let path = PathBuf::from(format!("/run/user/{UI_UID}")).join(format!(".td-secret-{name}"));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    fs::remove_file(&path)?;
    // No pathname contains credential bytes, and the recipient gets read-only access.
    file.write_all(secret)?;
    File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).map(|file| (file, name))
}

pub(super) fn expire(connection: &mut Connection, state: &mut ServiceState) -> io::Result<()> {
    let now = Instant::now();
    state
        .secret_receipts
        .retain(|_, receipt| now < receipt.deadline);
    let expired = state
        .secrets
        .iter()
        .filter_map(|(serial, p)| (now >= p.deadline).then_some(*serial))
        .collect::<Vec<_>>();
    for serial in expired {
        if let Some(pending) = state.secrets.remove(&serial) {
            error(
                connection,
                &pending,
                DENIED,
                "credential identity lookup expired",
            )?;
        }
    }
    Ok(())
}

pub(super) fn acknowledge(
    connection: &mut Connection,
    state: &mut ServiceState,
    call: &Message<'_>,
) -> io::Result<bool> {
    if call.kind != MessageType::MethodCall
        || call.fields.path != Some(PORTAL_PATH)
        || call.fields.interface != Some(SECRET_INTERFACE)
        || call.fields.member != Some("Received")
    {
        return Ok(false);
    }
    let owner = call
        .fields
        .sender
        .ok_or_else(|| io::Error::other("credential receipt has no broker sender"))?;
    let token = match call.args() {
        [Value::Str(token)] if exact_signature(call, "s") && token.len() == 32 => Some(*token),
        _ => None,
    };
    let valid = token
        .and_then(|token| state.secret_receipts.get(token))
        .is_some_and(|receipt| receipt.owner == owner && Instant::now() < receipt.deadline);
    let serial = connection.next_serial()?;
    if !valid {
        if call.flags & message::FLAG_NO_REPLY_EXPECTED == 0 {
            connection.write_frame(&method_error(
                call.endian,
                serial,
                call.serial,
                owner,
                DENIED,
                "invalid credential receipt",
            )?)?;
        }
        return Ok(true);
    }
    let receipt = token
        .and_then(|token| state.secret_receipts.remove(token))
        .ok_or_else(|| io::Error::other("credential receipt disappeared"))?;
    if call.flags & message::FLAG_NO_REPLY_EXPECTED == 0 {
        connection.write_frame(&method_return(
            call.endian,
            serial,
            call.serial,
            owner,
            "",
            |_| Ok(()),
        )?)?;
    }
    if receipt.app == "mail" && receipt.name == "main" {
        eprintln!("TD-SECRET-READY app=mail name=main");
    }
    Ok(true)
}

pub(super) struct DescriptorReader {
    stream: UnixStream,
    count: u32,
}
impl DescriptorReader {
    pub fn new(stream: UnixStream) -> Self {
        Self { stream, count: 0 }
    }
    pub fn finish(&mut self, frame: IncomingFrame) -> io::Result<IncomingFrame> {
        let count = std::mem::take(&mut self.count);
        match frame {
            IncomingFrame::Message(bytes) if count != 0 => {
                Ok(IncomingFrame::Descriptors { bytes, count })
            }
            frame => Ok(frame),
        }
    }
}
impl Read for DescriptorReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let received =
            sys::recv_with_fds(&self.stream, bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let count = received.fds.len() as u32;
        sys::discard_received(&received.fds);
        self.count = self.count.saturating_add(count);
        if self.count > message::MAX_FDS_PER_MESSAGE {
            return Err(io::Error::other("too many incoming portal descriptors"));
        }
        Ok(received.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> (Connection, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (
            Connection {
                stream,
                serial: 10,
                unique: Some(":1.10".into()),
                until: None,
            },
            peer,
        )
    }

    fn retrieve() -> Vec<u8> {
        message::Builder::method_call(
            Endian::Little,
            PORTAL_PATH,
            Some(SECRET_INTERFACE),
            "Retrieve",
        )
        .serial(1)
        .sender(":1.9")
        .destination(PORTAL_NAME)
        .body("s", |w| w.string("main"))
        .unwrap()
        .encode()
        .unwrap()
    }

    #[test]
    fn identities_are_broker_only_and_unconfined_is_refused() {
        let (mut connection, mut peer) = connection();
        let mut state = ServiceState::default();
        let bytes = retrieve();
        let (call, _) = message::decode(&bytes, 0).unwrap();
        begin(&mut connection, &mut state, &call).unwrap();
        let IncomingFrame::Message(query) = read_frame(&mut peer).unwrap() else {
            panic!("query");
        };
        let (query, _) = message::decode(&query, 0).unwrap();
        assert_eq!(query.fields.destination, Some(BUS_NAME));
        assert_eq!(query.fields.member, Some("GetConnectionCredentials"));
        assert_eq!(query.args(), &[Value::Str(":1.9")]);
        for sender in [":1.99", BUS_NAME] {
            let bytes = message::Builder::method_return(Endian::Little, query.serial)
                .serial(50)
                .sender(sender)
                .destination(":1.10")
                .body("a{sv}", |w| {
                    w.array("{sv}", |w| {
                        w.dict_entry(|w| {
                            w.string("UnixUserID")?;
                            w.variant("u", |w| {
                                w.uint32(UI_UID);
                                Ok(())
                            })
                        })
                    })
                })
                .unwrap()
                .encode()
                .unwrap();
            let (reply, _) = message::decode(&bytes, 0).unwrap();
            assert!(identity_reply(&mut connection, &mut state, &reply).unwrap());
            assert_eq!(state.secrets.len(), usize::from(sender != BUS_NAME));
        }
        let IncomingFrame::Message(refusal) = read_frame(&mut peer).unwrap() else {
            panic!("refusal");
        };
        let (refusal, _) = message::decode(&refusal, 0).unwrap();
        assert_eq!(refusal.fields.error_name, Some(DENIED));
        assert!(state.secret_receipts.is_empty());
    }

    #[test]
    fn incoming_descriptors_are_closed_and_refused_without_losing_the_next_call() {
        let (service, mut peer) = UnixStream::pair().unwrap();
        let file = File::open("/dev/null").unwrap();
        let bytes = message::Builder::method_call(
            Endian::Little,
            PORTAL_PATH,
            Some(SECRET_INTERFACE),
            "Retrieve",
        )
        .serial(1)
        .sender(":1.9")
        .destination(PORTAL_NAME)
        .unix_fds(1)
        .body("h", |w| {
            w.uint32(0);
            Ok(())
        })
        .unwrap()
        .encode()
        .unwrap();
        sys::send_with_fd(&peer, &bytes, file.as_raw_fd()).unwrap();
        peer.write_all(&retrieve()).unwrap();
        let mut reader = DescriptorReader::new(service);
        let first = read_frame(&mut reader).unwrap();
        assert!(matches!(
            reader.finish(first).unwrap(),
            IncomingFrame::Descriptors { count: 1, .. }
        ));
        let next = read_frame(&mut reader).unwrap();
        assert!(matches!(
            reader.finish(next).unwrap(),
            IncomingFrame::Message(_)
        ));
    }

    #[test]
    fn receipts_are_owner_bound_expiring_and_one_use() {
        let (mut connection, mut peer) = connection();
        let token = "01234567890123456789012345678901";
        let mut state = ServiceState::default();
        state.secret_receipts.insert(
            token.into(),
            Receipt {
                owner: ":1.9".into(),
                app: "other".into(),
                name: "main".into(),
                deadline: Instant::now() + EXCHANGE_TIMEOUT,
            },
        );
        for (owner, accepted) in [(":1.99", false), (":1.9", true), (":1.9", false)] {
            let bytes = message::Builder::method_call(
                Endian::Little,
                PORTAL_PATH,
                Some(SECRET_INTERFACE),
                "Received",
            )
            .serial(1)
            .sender(owner)
            .destination(PORTAL_NAME)
            .body("s", |w| w.string(token))
            .unwrap()
            .encode()
            .unwrap();
            let (call, _) = message::decode(&bytes, 0).unwrap();
            assert!(acknowledge(&mut connection, &mut state, &call).unwrap());
            let IncomingFrame::Message(reply) = read_frame(&mut peer).unwrap() else {
                panic!("reply");
            };
            let (reply, _) = message::decode(&reply, 0).unwrap();
            assert_eq!(reply.kind == MessageType::MethodReturn, accepted);
        }
    }

    #[test]
    fn extra_descriptor_call_sites_are_confined() {
        let source = include_str!("secret.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for site in [
            "sys::send_with_fd(",
            "sys::recv_with_fds(",
            "sys::discard_received(",
        ] {
            assert_eq!(source.matches(site).count(), 1);
        }
    }
    #[test]
    fn names_cannot_select_another_application() {
        for name in ["", "mail/main", "../main", "main.other", "main\0", "main\n"] {
            assert!(!secret_store::valid_name(name));
        }
        assert!(secret_store::valid_name("main"));
    }
}
