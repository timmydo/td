use super::smtp_transport::{upgrade_protocol, TlsDriver};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::Arc,
};

pub(super) fn open(
    session: &td_json::Json,
    address: SocketAddr,
    ca: &[u8],
    credentials: &str,
    close_after: &str,
    last_id: Option<&str>,
) -> BufReader<TlsDriver> {
    let (tls, _) = upgrade_protocol(
        TcpStream::connect(address).unwrap(),
        Arc::new(td_mta::clock::RuntimeClock::new()),
        ca,
        td_crypto::TlsProtocol::Http1,
    );
    let host = format!("localhost:{}", address.port());
    request(
        tls,
        session,
        &host,
        &format!("https://{host}"),
        credentials,
        close_after,
        last_id,
    )
}

pub(super) fn request<T: Read + Write>(
    mut io: T,
    session: &td_json::Json,
    host: &str,
    origin: &str,
    credentials: &str,
    close_after: &str,
    last_id: Option<&str>,
) -> BufReader<T> {
    let path = session["eventSourceUrl"]
        .as_str()
        .unwrap()
        .strip_prefix(origin)
        .unwrap()
        .replace("{types}", "Email")
        .replace("{closeafter}", close_after)
        .replace("{ping}", "0");
    write!(io, "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {credentials}\r\nAccept: text/event-stream\r\n").unwrap();
    if let Some(last_id) = last_id {
        write!(io, "Last-Event-ID: {last_id}\r\n").unwrap();
    }
    io.write_all(b"\r\n").unwrap();
    io.flush().unwrap();
    let mut reader = BufReader::new(io);
    let mut head = String::new();
    while !head.ends_with("\r\n\r\n") {
        assert_ne!(reader.read_line(&mut head).unwrap(), 0, "missing SSE head");
        assert!(head.len() < 4096);
    }
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert!(head
        .to_ascii_lowercase()
        .contains("content-type: text/event-stream"));
    reader
}

pub(super) fn state(reader: &mut impl BufRead, account: &str) -> (String, String) {
    let mut frame = String::new();
    while !frame.ends_with("\n\n") {
        assert_ne!(
            reader.read_line(&mut frame).unwrap(),
            0,
            "missing SSE state"
        );
        assert!(frame.len() <= 2048);
    }
    assert!(frame.lines().any(|line| line == "event: state"), "{frame}");
    let field = |name| {
        frame
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
    };
    let id = field("id: ");
    assert!(!id.is_empty());
    let message = td_json::parse(field("data: ")).unwrap();
    assert_eq!(message["@type"].as_str(), Some("StateChange"));
    let changed = &message["changed"];
    assert_eq!(changed.as_obj().unwrap().len(), 1);
    assert_eq!(changed[account].as_obj().unwrap().len(), 1);
    (
        id.to_owned(),
        changed[account]["Email"].as_str().unwrap().to_owned(),
    )
}

pub(super) fn observe_change<T>(
    session: &td_json::Json,
    address: SocketAddr,
    ca: &[u8],
    credentials: &str,
    change: impl FnOnce() -> T,
) -> T {
    let account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        .as_str()
        .unwrap();
    let connect = |last| open(session, address, ca, credentials, "state", last);
    let mut initial = connect(None);
    let (last_id, before) = state(&mut initial, account);
    assert_eq!(initial.read(&mut [0]).unwrap(), 0);
    let mut live = connect(Some(&last_id));
    let outcome = change();
    let (next_id, after) = state(&mut live, account);
    assert_ne!(before, after);
    assert_ne!(last_id, next_id);
    assert_eq!(live.read(&mut [0]).unwrap(), 0);
    let mut reconnect = connect(Some(&last_id));
    assert_ne!(state(&mut reconnect, account).1, before);
    assert_eq!(reconnect.read(&mut [0]).unwrap(), 0);
    outcome
}
