//! The database socket keeps its admission check on every connect.
use super::*;

#[test]
#[ignore = "requires PostgreSQL 17 and private trusted TMPDIR"]
fn local_database_admits_only_its_private_socket() {
    exercise();
}

pub(super) fn exercise() {
    private_listener_lifetime_and_permissions();
    let fixture = Fixture::new();
    let socket = fixture.socket().as_path().to_path_buf();
    let request = || fixture.request();
    let status = probe(&request(), deadline()).unwrap();
    assert!(status.server_version >= 170000);
    let receipt = td_mta::postgres_schema::initialize(&request(), deadline()).unwrap();
    assert_eq!(
        td_mta::postgres_schema::inspect(&request(), deadline()).unwrap(),
        receipt
    );

    let uri = format!(
        "postgresql://td_probe:{}@:{}/postgres?host={}&sslmode=disable\n",
        std::str::from_utf8(PASSWORD).unwrap(),
        fixture.port,
        fixture.root.display()
    );
    let connection = fixture.root.join("connection");
    fs::write(&connection, uri).unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(command_probe(&fixture, true).contains("\"status\":\"ok\""));
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660)).unwrap();
    assert_eq!(probe(&request(), deadline()), Err(Error::Transport));
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(probe(&request(), deadline()).unwrap(), status);
}

fn private_listener_lifetime_and_permissions() {
    use std::os::unix::net::UnixListener;
    use td_mta::ports::Error;
    use td_mta::unix_listener::PrivateListener;
    let root = td_fs::TempDir::new("jmap-listener").unwrap();
    let socket = root.path().join("socket");
    let path = td_fs::SocketPath::new(&socket).unwrap();
    let listener = PrivateListener::bind(path).unwrap();
    assert_eq!(listener.path(), socket);
    td_fs::check_private_socket(&socket).unwrap();
    assert!(matches!(PrivateListener::bind(path), Err(Error::Busy)));
    let client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let accepted = listener.accept().unwrap();
    drop((client, accepted, listener));
    assert!(!socket.exists());

    let stale = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let listener = PrivateListener::bind(path).unwrap();
    fs::remove_file(&socket).unwrap();
    fs::write(&socket, b"replacement must survive cleanup").unwrap();
    drop(listener);
    assert_eq!(
        fs::read(&socket).unwrap(),
        b"replacement must survive cleanup"
    );
    assert!(matches!(PrivateListener::bind(path), Err(Error::Forbidden)));
    fs::remove_file(&socket).unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
    assert!(PrivateListener::bind(path).is_err());
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
}
