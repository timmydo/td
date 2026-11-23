//! td-postgres against a real PostgreSQL 17 server, started the way
//! td-mta's native fixtures start theirs: the first `initdb` on PATH and
//! the `postgres` beside it, on a disposable cluster in an owned
//! temporary directory. Ignored in ordinary runs; the host preflight
//! runs it (`test-args = "--include-ignored"`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use td_postgres::{
    connect_tls, connect_unix, connect_unix_through, Array, Client, CommitError, Config, Error,
    Limits, Severity, Socket, SqlState, Stream, StreamError, TlsConnect, TransactionStatus,
};

const PASSWORD: &str = "td-postgres-fixture-0123456789abcdef";

/// A disposable cluster, stopped and removed on drop.
struct Server {
    bin: PathBuf,
    data: PathBuf,
    sockets: PathBuf,
    port: u16,
    child: Child,
    _root: td_fs::TempDir,
}

impl Server {
    fn start() -> Self {
        let initdb = td_fs::executable_on_path("initdb").expect(
            "PostgreSQL 17's initdb must be first on PATH (DEVELOPMENT.md, Native mail fixtures)",
        );
        let bin = initdb.parent().unwrap().to_path_buf();
        for companion in ["postgres", "pg_ctl"] {
            assert!(
                bin.join(companion).is_file(),
                "{companion} must sit beside {}",
                initdb.display()
            );
        }
        let version = output(tool(&bin, "postgres").arg("--version"));
        assert!(
            version.starts_with("postgres (PostgreSQL) 17."),
            "requires PostgreSQL 17, found {version}"
        );
        let root = td_fs::TempDir::new("td-postgres").unwrap();
        let data = root.path().join("data");
        let sockets = root.path().join("sockets");
        std::fs::create_dir(&sockets).unwrap();
        let password_file = root.path().join("password");
        std::fs::write(&password_file, PASSWORD).unwrap();
        output(
            tool(&bin, "initdb")
                .arg("-D")
                .arg(&data)
                .args([
                    "-U",
                    "td",
                    "--auth=scram-sha-256",
                    "-E",
                    "UTF8",
                    "--no-locale",
                    "--no-sync",
                ])
                .arg(format!("--pwfile={}", password_file.display())),
        );
        std::fs::write(
            data.join("pg_hba.conf"),
            "local all td scram-sha-256\n\
             local all cleartext password\n\
             local all trusted trust\n\
             host all td 127.0.0.1/32 scram-sha-256\n",
        )
        .unwrap();
        // A port free now; the race with another process is accepted.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // sun_path holds 107 bytes and a NUL; the postmaster refuses more.
        let socket = sockets.join(format!(".s.PGSQL.{port}"));
        assert!(
            socket.as_os_str().len() <= 107,
            "socket path too long for a Unix socket: {}",
            socket.display()
        );
        let log = std::fs::File::create(root.path().join("log")).unwrap();
        let child = tool(&bin, "postgres")
            .arg("-D")
            .arg(&data)
            .arg("-k")
            .arg(&sockets)
            .args(["-p", &port.to_string()])
            .args(["-c", "listen_addresses=127.0.0.1", "-c", "ssl=off"])
            .args(["-c", "fsync=off", "-c", "unix_socket_permissions=0700"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut server = Self {
            bin,
            data,
            sockets,
            port,
            child,
            _root: root,
        };
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if server.socket().exists() && TcpStream::connect(server.address()).is_ok() {
                if let Ok(client) = server.connect(&config("td")) {
                    drop(client);
                    break;
                }
            }
            let exited = server.child.try_wait().ok().flatten();
            assert!(
                exited.is_none() && Instant::now() < until,
                "PostgreSQL did not start ({exited:?}):\n{}",
                server.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
        server
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self._root.path().join("log")).unwrap_or_default()
    }

    fn socket(&self) -> PathBuf {
        self.sockets.join(format!(".s.PGSQL.{}", self.port))
    }

    fn address(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    fn connect(&self, config: &Config<'_>) -> Result<Client<Socket>, Error> {
        connect_unix(&self.socket(), config, limits(), soon())
    }

    fn client(&self) -> Client<Socket> {
        self.connect(&config("td")).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = tool(&self.bin, "pg_ctl")
            .arg("-D")
            .arg(&self.data)
            .args(["stop", "-m", "fast", "-w", "-t", "20"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// A PostgreSQL tool with none of the caller's environment but PATH, in
/// the C locale.
fn tool(bin: &Path, name: &str) -> Command {
    let mut command = Command::new(bin.join(name));
    command.env_clear().env("LC_ALL", "C");
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
}

fn output(command: &mut Command) -> String {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_string()
}

fn limits() -> Limits {
    Limits {
        message_bytes: 1 << 20,
        request_bytes: 1 << 20,
        messages: 10_000,
        rows: 1000,
        columns: 64,
        result_bytes: 8 << 20,
        notifications: 16,
        scram_iterations: 10_000,
    }
}

fn config(user: &str) -> Config<'_> {
    Config {
        user,
        database: "postgres",
        password: PASSWORD.as_bytes(),
        application_name: "td-postgres-test",
        parameters: &[],
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn code(error: Error) -> String {
    match error {
        Error::Db(error) => error.code().as_str().to_string(),
        other => panic!("not a server error: {other:?}"),
    }
}

#[test]
#[ignore = "needs PostgreSQL 17 on PATH; the host preflight runs it"]
fn real_postgres() {
    let server = Server::start();
    authentication(&server);
    tls_request(&server);
    wrapped(&server);
    types(&server);
    statements(&server);
    transactions(&server);
    errors(&server);
    deadlines(&server);
    unknown_commits(&server);
    notifications(&server);
    limits_hold(&server);
}

fn authentication(server: &Server) {
    let mut client = server.client();
    let row = client
        .query_one(
            "SELECT current_user, pg_catalog.current_setting('client_encoding')",
            &[],
            soon(),
        )
        .unwrap();
    assert_eq!(row.get::<&str>(0), Ok("td"));
    assert_eq!(row.get::<&str>(1), Ok("UTF8"));
    let setting = client
        .query_one(
            "SELECT pg_catalog.current_setting('application_name')",
            &[],
            soon(),
        )
        .unwrap();
    assert_eq!(setting.get::<&str>(0), Ok("td-postgres-test"));
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
    client.close(soon()).unwrap();

    // Startup parameters reach the session.
    let mut timed = config("td");
    timed.parameters = &[
        ("statement_timeout", "1234"),
        ("options", "-c search_path=pg_catalog"),
    ];
    let mut client = server.connect(&timed).unwrap();
    let row = client
        .query_one(
            "SELECT current_setting('statement_timeout'), current_setting('search_path')",
            &[],
            soon(),
        )
        .unwrap();
    assert_eq!(
        (row.get::<&str>(0), row.get::<&str>(1)),
        (Ok("1234ms"), Ok("pg_catalog"))
    );

    // A wrong password is the server's FATAL 28P01.
    let mut wrong = config("td");
    wrong.password = b"not-the-password";
    match server.connect(&wrong) {
        Err(Error::Db(error)) => {
            assert_eq!(error.code().as_str(), "28P01");
            assert_eq!(error.severity(), Severity::Fatal);
        }
        other => panic!("wrong password: {:?}", other.err()),
    }

    // Cleartext and trust are refused by the client itself.
    let mut admin = server.client();
    admin
        .batch(
            &format!(
                "CREATE ROLE cleartext LOGIN PASSWORD '{PASSWORD}'; CREATE ROLE trusted LOGIN"
            ),
            soon(),
        )
        .unwrap();
    assert_eq!(
        server.connect(&config("cleartext")).err(),
        Some(Error::Authentication)
    );
    assert_eq!(
        server.connect(&config("trusted")).err(),
        Some(Error::Authentication)
    );
    // A role with no matching pg_hba line is the server's refusal.
    assert_eq!(
        code(server.connect(&config("nobody")).err().unwrap()),
        "28000"
    );
}

/// A connector that only records that it was asked.
struct Never(Arc<AtomicBool>);

impl TlsConnect for Never {
    type Stream = Socket;

    fn connect(self, _: TcpStream, _: Instant) -> Result<Socket, StreamError> {
        self.0.store(true, Ordering::SeqCst);
        Err(StreamError::Failed)
    }
}

fn tls_request(server: &Server) {
    // The fixture runs with ssl=off: the server answers N, and there is no
    // plaintext fallback.
    let asked = Arc::new(AtomicBool::new(false));
    let result = connect_tls(
        server.address(),
        Never(Arc::clone(&asked)),
        &config("td"),
        limits(),
        soon(),
    );
    assert_eq!(result.err(), Some(Error::TlsRefused));
    assert!(!asked.load(Ordering::SeqCst));
}

/// A caller's stream around the socket, counting both directions.
struct Counting {
    inner: Socket,
    counts: Arc<(AtomicUsize, AtomicUsize)>,
}

impl Stream for Counting {
    fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<usize, StreamError> {
        let count = self.inner.read(buffer, deadline)?;
        self.counts.0.fetch_add(count, Ordering::SeqCst);
        Ok(count)
    }

    fn write_all(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), StreamError> {
        self.counts.1.fetch_add(bytes.len(), Ordering::SeqCst);
        self.inner.write_all(bytes, deadline)
    }
}

fn wrapped(server: &Server) {
    let counts = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
    let mut client = connect_unix_through(
        &server.socket(),
        |inner| Counting {
            inner,
            counts: Arc::clone(&counts),
        },
        &config("td"),
        limits(),
        soon(),
    )
    .unwrap();
    let login = (
        counts.0.load(Ordering::SeqCst),
        counts.1.load(Ordering::SeqCst),
    );
    // SCRAM's messages out; its challenge and the parameters in.
    assert!(login.0 > 200 && login.1 > 100, "{login:?}");
    let row = client.query_one("SELECT 7::int8", &[], soon()).unwrap();
    assert_eq!(row.get::<i64>(0), Ok(7));
    assert!(counts.0.load(Ordering::SeqCst) > login.0);
    assert!(counts.1.load(Ordering::SeqCst) > login.1);
}

fn types(server: &Server) {
    let mut client = server.client();
    let every_byte: Vec<u8> = (0..=255).collect();
    let bytes: Vec<Option<&[u8]>> = vec![Some(b"a"), None, Some(b""), Some(&every_byte)];
    let counts: Vec<Option<i64>> = vec![Some(i64::MIN), None, Some(i64::MAX)];
    let words: Vec<String> = vec!["héllo".to_string(), "✓ world".to_string(), String::new()];
    let flags = vec![true, false];
    let owned: Vec<Vec<u8>> = vec![b"x".to_vec(), Vec::new()];
    let row = client
        .query_one(
            "SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14",
            &[
                &i16::MIN,
                &i32::MAX,
                &i64::MIN,
                &true,
                &every_byte,
                &"héllo, ✓",
                &bytes,
                &counts,
                &words,
                &flags,
                &owned,
                &None::<i64>,
                &Vec::<i64>::new(),
                &String::from("owned"),
            ],
            soon(),
        )
        .unwrap();
    assert_eq!(row.get::<i16>(0), Ok(i16::MIN));
    assert_eq!(row.get::<i32>(1), Ok(i32::MAX));
    assert_eq!(row.get::<i64>(2), Ok(i64::MIN));
    assert_eq!(row.get::<bool>(3), Ok(true));
    assert_eq!(row.get::<&[u8]>(4), Ok(every_byte.as_slice()));
    assert_eq!(row.get::<&str>(5), Ok("héllo, ✓"));
    let read: Vec<Option<&[u8]>> = row
        .get::<Array<'_>>(6)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, bytes);
    let read: Vec<Option<i64>> = row
        .get::<Array<'_>>(7)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, counts);
    let read: Vec<&str> = row
        .get::<Array<'_>>(8)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, words);
    let read: Vec<bool> = row
        .get::<Array<'_>>(9)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, flags);
    let read: Vec<&[u8]> = row
        .get::<Array<'_>>(10)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, [&b"x"[..], b""]);
    assert_eq!(row.get::<Option<i64>>(11), Ok(None));
    assert_eq!(row.get::<i64>(11), Err(td_postgres::DecodeError::Null));
    assert!(row.get::<Array<'_>>(12).unwrap().is_empty());
    assert_eq!(row.get::<&str>(13), Ok("owned"));
    // The other extreme of each integer.
    let row = client
        .query_one(
            "SELECT $1, $2, $3",
            &[&i16::MAX, &i32::MIN, &i64::MAX],
            soon(),
        )
        .unwrap();
    assert_eq!(row.get::<i16>(0), Ok(i16::MAX));
    assert_eq!(row.get::<i32>(1), Ok(i32::MIN));
    assert_eq!(row.get::<i64>(2), Ok(i64::MAX));
    // A type the column is not is refused on read, as the server refuses
    // a parameter its statement cannot take.
    assert_eq!(row.get::<i32>(2), Err(td_postgres::DecodeError::Type(20)));
    let error = client
        .query("SELECT $1 + 1::int8", &[&"seven"], soon())
        .err()
        .unwrap();
    assert_eq!(code(error), "42883");
    assert!(!client.is_broken());

    // Values the server computes, arrays it aggregates with NULLs, a name.
    let row = client
        .query_one(
            "SELECT 7::int2, array_agg(x ORDER BY x NULLS LAST), array['a', 'b']::text[], \
             current_user::name, 't'::bool \
             FROM (VALUES ('\\x01'::bytea), (NULL)) v(x)",
            &[],
            soon(),
        )
        .unwrap();
    assert_eq!(row.get::<i16>(0), Ok(7));
    let read: Vec<Option<&[u8]>> = row
        .get::<Array<'_>>(1)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, [Some(&[1u8][..]), None]);
    let read: Vec<&str> = row
        .get::<Array<'_>>(2)
        .unwrap()
        .iter()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, ["a", "b"]);
    assert_eq!(row.get::<&str>(3), Ok("td"));
    assert_eq!(row.get::<bool>(4), Ok(true));
    // Text with a NUL is refused before sending.
    assert_eq!(
        client.query("SELECT $1", &[&"a\0b"], soon()).err(),
        Some(Error::Invalid)
    );
    assert!(!client.is_broken());
}

fn statements(server: &Server) {
    let mut client = server.client();
    client
        .batch(
            "CREATE TABLE items (id int8 PRIMARY KEY, body bytea, note text)",
            soon(),
        )
        .unwrap();
    for id in 1..=3i64 {
        let written = client
            .execute(
                "INSERT INTO items (id, body, note) VALUES ($1, $2, $3)",
                &[&id, &id.to_be_bytes().as_slice(), &format!("note {id}")],
                soon(),
            )
            .unwrap();
        assert_eq!(written, 1);
    }
    assert_eq!(
        client.execute(
            "UPDATE items SET note = $1 WHERE id >= $2",
            &[&"later", &2i64],
            soon()
        ),
        Ok(2)
    );
    let rows = client
        .query(
            "SELECT id, body, note FROM items WHERE id = ANY($1) ORDER BY id",
            &[&vec![1i64, 3]],
            soon(),
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.width(), 3);
    let ids: Vec<i64> = rows
        .iter()
        .map(|row| row.get_named("id").unwrap())
        .collect();
    assert_eq!(ids, [1, 3]);
    assert_eq!(rows.row(1).unwrap().get_named::<&str>("note"), Ok("later"));
    assert_eq!(
        rows.row(0).unwrap().get::<&[u8]>(1),
        Ok(&1i64.to_be_bytes()[..])
    );
    assert!(client
        .query_opt("SELECT id FROM items WHERE id = $1", &[&9i64], soon())
        .unwrap()
        .is_none());
    assert_eq!(
        client.query_one("SELECT id FROM items", &[], soon()).err(),
        Some(Error::RowCount)
    );
    assert_eq!(
        client.query_opt("SELECT id FROM items", &[], soon()).err(),
        Some(Error::RowCount)
    );
    assert_eq!(
        client.execute("DELETE FROM items WHERE id = 3", &[], soon()),
        Ok(1)
    );
    assert_eq!(client.execute("SELECT * FROM items", &[], soon()), Ok(2));
    // An empty statement and a batch of several.
    assert_eq!(client.execute("", &[], soon()), Ok(0));
    client
        .batch(
            "SET search_path = public; SELECT 1; DO $$ BEGIN RAISE NOTICE 'hi'; END $$",
            soon(),
        )
        .unwrap();
    assert!(!client.is_broken());
}

fn transactions(server: &Server) {
    let mut client = server.client();
    client
        .batch("CREATE TABLE ledger (id int8 PRIMARY KEY)", soon())
        .unwrap();
    let count = |client: &mut Client<Socket>| -> i64 {
        client
            .query_one("SELECT count(*) FROM ledger", &[], soon())
            .unwrap()
            .get(0)
            .unwrap()
    };

    client
        .batch(
            "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on",
            soon(),
        )
        .unwrap();
    assert_eq!(
        client.transaction_status(),
        TransactionStatus::InTransaction
    );
    client
        .execute("INSERT INTO ledger VALUES ($1)", &[&1i64], soon())
        .unwrap();
    assert_eq!(client.commit(soon()), Ok(()));
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
    assert_eq!(count(&mut client), 1);

    client.batch("BEGIN", soon()).unwrap();
    client
        .execute("INSERT INTO ledger VALUES ($1)", &[&2i64], soon())
        .unwrap();
    client.rollback(soon()).unwrap();
    assert_eq!(count(&mut client), 1);

    // A failed transaction's COMMIT is the server's ROLLBACK.
    client.batch("BEGIN", soon()).unwrap();
    let duplicate = client
        .execute("INSERT INTO ledger VALUES ($1)", &[&1i64], soon())
        .err()
        .unwrap();
    assert_eq!(code(duplicate), "23505");
    assert_eq!(client.transaction_status(), TransactionStatus::Failed);
    assert_eq!(client.commit(soon()), Err(CommitError::RolledBack(None)));
    assert!(!client.is_broken());

    // A deferred constraint fails at COMMIT itself: a definite rollback.
    client
        .batch(
            "CREATE TABLE deferred (id int8, CONSTRAINT once UNIQUE (id) DEFERRABLE INITIALLY DEFERRED)",
            soon(),
        )
        .unwrap();
    client.batch("BEGIN", soon()).unwrap();
    client
        .execute("INSERT INTO deferred VALUES (1), (1)", &[], soon())
        .unwrap();
    match client.commit(soon()) {
        Err(CommitError::RolledBack(Some(error))) => assert_eq!(error.code().as_str(), "23505"),
        other => panic!("deferred violation: {other:?}"),
    }
    assert!(!client.is_broken());
    let deferred: i64 = client
        .query_one("SELECT count(*) FROM deferred", &[], soon())
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!(deferred, 0);
    // Nothing to commit outside a transaction, and nothing is sent.
    assert_eq!(
        client.commit(soon()),
        Err(CommitError::NotSent(Error::Invalid))
    );
    assert!(!client.is_broken());
}

fn errors(server: &Server) {
    let mut client = server.client();
    assert_eq!(
        code(client.batch("SELEC 1", soon()).err().unwrap()),
        "42601"
    );
    assert_eq!(
        code(
            client
                .query("SELECT * FROM missing", &[], soon())
                .err()
                .unwrap()
        ),
        "42P01"
    );
    assert_eq!(
        code(client.query("SELECT 1/0", &[], soon()).err().unwrap()),
        "22012"
    );
    assert!(!client.is_broken());
    assert_eq!(
        client.query("SELECT 1", &[], soon()).map(|rows| rows.len()),
        Ok(1)
    );

    // lock_not_available, the one SQLSTATE td-mta reads.
    client
        .batch(
            "CREATE TABLE locked (id int8 PRIMARY KEY); INSERT INTO locked VALUES (1)",
            soon(),
        )
        .unwrap();
    let mut holder = server.client();
    holder
        .batch("BEGIN; SELECT id FROM locked FOR UPDATE", soon())
        .unwrap();
    let waiting = client
        .query(
            "SELECT id FROM locked WHERE id = $1 FOR UPDATE NOWAIT",
            &[&1i64],
            soon(),
        )
        .err()
        .unwrap();
    match waiting {
        Error::Db(error) => assert_eq!(error.code(), SqlState::LOCK_NOT_AVAILABLE),
        other => panic!("NOWAIT: {other:?}"),
    }
    client.batch("SET lock_timeout = 100", soon()).unwrap();
    let timed_out = client
        .execute("UPDATE locked SET id = 1", &[], soon())
        .err()
        .unwrap();
    assert_eq!(code(timed_out), "55P03");
    holder.rollback(soon()).unwrap();

    // A statement_timeout from the startup parameters.
    let mut short = config("td");
    short.parameters = &[("statement_timeout", "100")];
    let mut client = server.connect(&short).unwrap();
    assert_eq!(
        code(client.batch("SELECT pg_sleep(2)", soon()).err().unwrap()),
        "57014"
    );
    assert!(!client.is_broken());
}

fn deadlines(server: &Server) {
    let mut client = server.client();
    let started = Instant::now();
    let result = client.query(
        "SELECT pg_sleep(5)",
        &[],
        Instant::now() + Duration::from_millis(300),
    );
    assert_eq!(result.err(), Some(Error::Deadline));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(300) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
    assert!(client.is_broken());
    assert_eq!(
        client.query("SELECT 1", &[], soon()).err(),
        Some(Error::Broken)
    );
    // A connection deadline already past connects to nothing.
    assert_eq!(
        connect_unix(&server.socket(), &config("td"), limits(), Instant::now()).err(),
        Some(Error::Deadline)
    );
}

fn unknown_commits(server: &Server) {
    let mut admin = server.client();
    admin
        .batch(
            "CREATE TABLE slow (id int8 PRIMARY KEY); \
             CREATE FUNCTION linger() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM pg_sleep(2); RETURN NULL; END $$; \
             CREATE CONSTRAINT TRIGGER linger AFTER INSERT ON slow \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION linger()",
            soon(),
        )
        .unwrap();

    // The deadline passes while COMMIT runs its deferred trigger: the
    // outcome is unknown, and here the server goes on to commit.
    let mut client = server.client();
    client.batch("BEGIN", soon()).unwrap();
    client
        .execute("INSERT INTO slow VALUES (1)", &[], soon())
        .unwrap();
    let outcome = client.commit(Instant::now() + Duration::from_millis(300));
    assert_eq!(outcome, Err(CommitError::OutcomeUnknown(Error::Deadline)));
    assert!(client.is_broken());
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let committed: i64 = admin
            .query_one("SELECT count(*) FROM slow WHERE id = 1", &[], soon())
            .unwrap()
            .get(0)
            .unwrap();
        if committed == 1 {
            break;
        }
        assert!(Instant::now() < until, "the abandoned COMMIT never landed");
        thread::sleep(Duration::from_millis(100));
    }

    // The backend is terminated during COMMIT: a FATAL, still unknown.
    let mut client = server.client();
    let pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[], soon())
        .unwrap()
        .get(0)
        .unwrap();
    client.batch("BEGIN", soon()).unwrap();
    client
        .execute("INSERT INTO slow VALUES (2)", &[], soon())
        .unwrap();
    let killer = thread::spawn(move || {
        // Only once the backend is running COMMIT, its trigger sleeping.
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let running = admin
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity \
                     WHERE pid = $1 AND state = 'active' AND query = 'COMMIT'",
                    &[&pid],
                    soon(),
                )
                .unwrap()
                .get::<i64>(0)
                .unwrap();
            if running == 1 {
                break;
            }
            assert!(Instant::now() < until, "COMMIT never started");
            thread::sleep(Duration::from_millis(20));
        }
        admin
            .query_one("SELECT pg_terminate_backend($1)", &[&pid], soon())
            .unwrap()
            .get::<bool>(0)
            .unwrap()
    });
    match client.commit(soon()) {
        Err(CommitError::OutcomeUnknown(Error::Db(error))) => {
            assert_eq!(error.code().as_str(), "57P01");
            assert_eq!(error.severity(), Severity::Fatal);
        }
        other => panic!("terminated COMMIT: {other:?}"),
    }
    assert!(killer.join().unwrap());
    assert!(client.is_broken());
}

fn notifications(server: &Server) {
    let mut listener = server.client();
    let mut notifier = server.client();
    listener.batch("LISTEN td_mta_relay", soon()).unwrap();
    // Nothing yet: the wait times out quietly and the session stays.
    let started = Instant::now();
    listener
        .wait(Instant::now() + Duration::from_millis(200))
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
    assert!(listener.take_notification().is_none());
    listener.wait(Instant::now()).unwrap();

    notifier.batch("NOTIFY td_mta_relay", soon()).unwrap();
    let started = Instant::now();
    listener.wait(soon()).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait was not woken"
    );
    let note = listener.take_notification().unwrap();
    assert_eq!((note.channel(), note.payload()), ("td_mta_relay", ""));
    notifier
        .query(
            "SELECT pg_notify('td_mta_relay', $1)",
            &[&"payload ✓"],
            soon(),
        )
        .unwrap();
    let started = Instant::now();
    listener.wait(soon()).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait was not woken"
    );
    assert_eq!(listener.take_notification().unwrap().payload(), "payload ✓");
    assert_eq!(
        listener
            .query("SELECT 1", &[], soon())
            .map(|rows| rows.len()),
        Ok(1)
    );

    // A server that ends an idle session is found by a poll.
    let pid: i32 = listener
        .query_one("SELECT pg_backend_pid()", &[], soon())
        .unwrap()
        .get(0)
        .unwrap();
    notifier
        .query("SELECT pg_terminate_backend($1)", &[&pid], soon())
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    let ended = loop {
        match listener.wait(Instant::now() + Duration::from_millis(100)) {
            Ok(()) => assert!(Instant::now() < until, "the ended session went unnoticed"),
            Err(error) => break error,
        }
    };
    assert!(
        matches!(ended, Error::Db(error) if error.code().as_str() == "57P01"),
        "{ended:?}"
    );
    assert!(listener.is_broken());

    // A request written to a session the server has already ended reports
    // the FATAL waiting there, and a COMMIT so written stays unknown.
    for in_transaction in [false, true] {
        let mut client = server.client();
        let pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[], soon())
            .unwrap()
            .get(0)
            .unwrap();
        if in_transaction {
            client.batch("BEGIN", soon()).unwrap();
        }
        notifier
            .query("SELECT pg_terminate_backend($1)", &[&pid], soon())
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let left: i64 = notifier
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid = $1",
                    &[&pid],
                    soon(),
                )
                .unwrap()
                .get(0)
                .unwrap();
            // Gone from pg_stat_activity is not yet gone: the process,
            // and with it the socket, closes after.
            let process = Path::new("/proc").join(pid.to_string());
            if left == 0 && !process.exists() {
                break;
            }
            assert!(Instant::now() < until, "the backend never exited");
            thread::sleep(Duration::from_millis(20));
        }
        let terminated =
            |error: &Error| matches!(error, Error::Db(e) if e.code().as_str() == "57P01");
        if in_transaction {
            match client.commit(soon()) {
                Err(CommitError::OutcomeUnknown(error)) => {
                    assert!(terminated(&error), "{error:?}")
                }
                other => panic!("COMMIT to an ended session: {other:?}"),
            }
        } else {
            let error = client.query("SELECT 1", &[], soon()).err().unwrap();
            assert!(terminated(&error), "{error:?}");
        }
        assert!(client.is_broken());
    }
}

fn limits_hold(server: &Server) {
    let mut client = server.client();
    let mut few = limits();
    few.rows = 5;
    client.set_limits(few).unwrap();
    assert_eq!(
        client
            .query("SELECT generate_series(1, 5)", &[], soon())
            .map(|rows| rows.len()),
        Ok(5)
    );
    assert_eq!(
        client
            .query("SELECT generate_series(1, 6)", &[], soon())
            .err(),
        Some(Error::Limit)
    );
    assert!(client.is_broken());

    let mut client = server.client();
    let mut small = limits();
    small.message_bytes = 4096;
    client.set_limits(small).unwrap();
    assert_eq!(
        client
            .query("SELECT repeat('x', 5000)::bytea", &[], soon())
            .err(),
        Some(Error::Limit)
    );
    assert!(client.is_broken());

    let mut client = server.client();
    let mut tight = limits();
    tight.result_bytes = 1000;
    client.set_limits(tight).unwrap();
    assert_eq!(
        client
            .query(
                "SELECT repeat('x', 300) FROM generate_series(1, 4)",
                &[],
                soon()
            )
            .err(),
        Some(Error::Limit)
    );
    assert!(client.is_broken());

    // A request past its bound is refused before anything is sent.
    let mut client = server.client();
    let mut narrow = limits();
    narrow.request_bytes = 1024;
    client.set_limits(narrow).unwrap();
    let big = vec![0u8; 2048];
    assert_eq!(
        client.execute("SELECT $1", &[&big], soon()).err(),
        Some(Error::Limit)
    );
    assert!(!client.is_broken());
    assert_eq!(client.execute("SELECT 1", &[], soon()), Ok(1));
}
