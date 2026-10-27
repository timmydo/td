// td-test-https — a test HTTPS origin and its verifying client, on the
// rustls and ring this crate already links.
// The system image's autotest-only Firefox proof uses it in place of
// LibreSSL's `openssl req`/`x509`/`s_server`/`s_client`
// (APPLICATIONS.md §B.8 item 6):
//
//   td-test-https mint DIR
//       Mint a fresh ECDSA P-256 authority and a localhost server identity
//       signed by it, valid for one day: DIR/ca.pem (the authority's
//       certificate), DIR/server.pem and DIR/server.key (PKCS#8). Every file
//       is created new, mode 0600. The authority's private key never leaves
//       this process, so nothing can sign with it after the mint.
//
//   td-test-https serve --listen ADDR --cert FILE --key FILE --root DIR
//       Serve DIR's regular files over HTTPS, one HTTP/1.0 GET per
//       connection, `.html` as text/html and anything else as text/plain.
//       Silent: the fixture shares the boot console with exact evidence
//       lines. Returns only on a setup error.
//
//   td-test-https probe --connect ADDR --server-name NAME --ca FILE
//                       --path PATH --expect TEXT
//       Verify the origin's chain against FILE alone and its identity
//       against NAME, GET PATH, and succeed only on a 200 whose body
//       contains TEXT.
//
// It is a fixture, not a web server: no keep-alive, no ranges, no
// directories, and nothing outside DIR's own flat names. Every connection,
// served or probed, has one deadline for its whole life, handshake included.
// The crate-wide grandfathered allow in main.rs does not cover this module.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use sha2::Digest;

const VALID_SECS: u64 = 24 * 60 * 60;
/// Validity starts this far back, so a guest clock that steps
/// slightly backwards after the mint does not make it not-yet-valid.
const BACKDATE_SECS: u64 = 60 * 60;
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: u64 = 1024 * 1024;
const MAX_PEM: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 32;
/// The whole life of one connection, accept or connect to close.
const CONNECTION_BUDGET: Duration = Duration::from_secs(20);

const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
const OID_SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
const OID_SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1d, 0x0e];
const OID_AUTHORITY_KEY_ID: &[u8] = &[0x55, 0x1d, 0x23];

const CA_NAME: &str = "td test HTTPS authority";
const SERVER_NAME: &str = "localhost";
const SERVER_IP: [u8; 4] = [127, 0, 0, 1];

pub fn run(args: &[String]) {
    let rest = args.get(1..).unwrap_or(&[]);
    let result = match rest.split_first() {
        Some((verb, [dir])) if verb == "mint" => mint(Path::new(dir)),
        Some((verb, flags)) if verb == "serve" => {
            parse_serve(flags).and_then(|serve| serve.run().map(|never| match never {}))
        }
        Some((verb, flags)) if verb == "probe" => parse_probe(flags).and_then(|p| p.run()),
        _ => {
            eprintln!(
                "usage: td-test-https mint DIR\n       \
                 td-test-https serve --listen ADDR --cert FILE --key FILE --root DIR\n       \
                 td-test-https probe --connect ADDR --server-name NAME --ca FILE \
                 --path PATH --expect TEXT"
            );
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("td-test-https: {error}");
        std::process::exit(1);
    }
}

/// Read `--name value` pairs, each named flag exactly once and no other.
fn flags<'a>(args: &'a [String], names: &[&str]) -> Result<Vec<&'a str>, String> {
    let mut values: Vec<Option<&str>> = vec![None; names.len()];
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let index = names
            .iter()
            .position(|name| flag.strip_prefix("--") == Some(*name))
            .ok_or_else(|| format!("unknown argument {flag:?}"))?;
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let slot = values
            .get_mut(index)
            .ok_or_else(|| format!("unknown argument {flag:?}"))?;
        if slot.replace(value.as_str()).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    names
        .iter()
        .zip(values)
        .map(|(name, value)| value.ok_or_else(|| format!("--{name} is required")))
        .collect()
}

// ---- mint -------------------------------------------------------------------

/// One generated key pair and its PKCS#8 encoding.
struct Key {
    pkcs8: Vec<u8>,
    pair: EcdsaKeyPair,
}

impl Key {
    fn generate(rng: &SystemRandom) -> Result<Self, String> {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, rng)
            .map_err(|_| "generate a P-256 key".to_string())?
            .as_ref()
            .to_vec();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, rng)
            .map_err(|e| format!("load the generated key: {e}"))?;
        Ok(Self { pkcs8, pair })
    }

    /// RFC 7093 §2 method 1: SHA-256 of the public key, cut to 160 bits.
    fn id(&self) -> Vec<u8> {
        let digest = sha2::Sha256::digest(self.pair.public_key().as_ref());
        digest.get(..20).unwrap_or(&[]).to_vec()
    }

    fn spki(&self) -> Vec<u8> {
        let algorithm = seq(&[&der(0x06, OID_EC_PUBLIC_KEY), &der(0x06, OID_P256)]);
        seq(&[&algorithm, &bit_string(self.pair.public_key().as_ref())])
    }
}

/// The authority's certificate and the server's certificate and key, DER.
struct Minted {
    ca: Vec<u8>,
    server: Vec<u8>,
    server_key: Vec<u8>,
}

fn mint(dir: &Path) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970".to_string())?
        .as_secs();
    let minted = mint_at(now)?;
    let files = [
        ("ca.pem", pem("CERTIFICATE", &minted.ca)),
        ("server.pem", pem("CERTIFICATE", &minted.server)),
        ("server.key", pem("PRIVATE KEY", &minted.server_key)),
    ];
    let mut created = Vec::with_capacity(files.len());
    for (name, bytes) in &files {
        let path = dir.join(name);
        let result = write_new(&path, bytes, &mut created);
        if let Err(error) = result {
            // Leave nothing a rerun would trip over in place of this cause.
            for path in &created {
                let _ = std::fs::remove_file(path);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn mint_at(now: u64) -> Result<Minted, String> {
    let rng = SystemRandom::new();
    let not_before = now.saturating_sub(BACKDATE_SECS);
    let not_after = now
        .checked_add(VALID_SECS)
        .ok_or("the clock is out of range")?;
    let validity = seq(&[&time(not_before)?, &time(not_after)?]);
    let authority = Key::generate(&rng)?;
    let server = Key::generate(&rng)?;
    let ca_name = name(CA_NAME);
    let ca = certificate(
        &rng,
        &authority,
        &serial(&rng)?,
        &ca_name,
        &validity,
        &ca_name,
        &authority.spki(),
        &[
            extension(OID_BASIC_CONSTRAINTS, true, &seq(&[&der(0x01, &[0xff])])),
            // keyCertSign and cRLSign.
            extension(OID_KEY_USAGE, true, &der(0x03, &[0x01, 0x06])),
            extension(OID_SUBJECT_KEY_ID, false, &der(0x04, &authority.id())),
        ],
    )?;
    let mut alt_names = der(0x82, SERVER_NAME.as_bytes());
    alt_names.extend(der(0x87, &SERVER_IP));
    let leaf = certificate(
        &rng,
        &authority,
        &serial(&rng)?,
        &ca_name,
        &validity,
        &name(SERVER_NAME),
        &server.spki(),
        &[
            extension(OID_BASIC_CONSTRAINTS, true, &seq(&[])),
            // digitalSignature, all an ECDHE_ECDSA server needs.
            extension(OID_KEY_USAGE, true, &der(0x03, &[0x07, 0x80])),
            extension(
                OID_EXT_KEY_USAGE,
                false,
                &seq(&[&der(0x06, OID_SERVER_AUTH)]),
            ),
            extension(OID_SUBJECT_ALT_NAME, false, &der(0x30, &alt_names)),
            extension(OID_SUBJECT_KEY_ID, false, &der(0x04, &server.id())),
            extension(
                OID_AUTHORITY_KEY_ID,
                false,
                &seq(&[&der(0x80, &authority.id())]),
            ),
        ],
    )?;
    Ok(Minted {
        ca,
        server: leaf,
        server_key: server.pkcs8,
    })
}

#[allow(clippy::too_many_arguments)]
fn certificate(
    rng: &SystemRandom,
    signer: &Key,
    serial: &[u8],
    issuer: &[u8],
    validity: &[u8],
    subject: &[u8],
    spki: &[u8],
    extensions: &[Vec<u8>],
) -> Result<Vec<u8>, String> {
    let algorithm = seq(&[&der(0x06, OID_ECDSA_SHA256)]);
    let extensions: Vec<&[u8]> = extensions.iter().map(Vec::as_slice).collect();
    let tbs = seq(&[
        // [0] EXPLICIT version v3.
        &der(0xa0, &der(0x02, &[0x02])),
        serial,
        &algorithm,
        issuer,
        validity,
        subject,
        spki,
        &der(0xa3, &seq(&extensions)),
    ]);
    let signature = signer
        .pair
        .sign(rng, &tbs)
        .map_err(|_| "sign a certificate".to_string())?;
    Ok(seq(&[&tbs, &algorithm, &bit_string(signature.as_ref())]))
}

/// A positive 16-byte serial number, as an INTEGER.
fn serial(rng: &SystemRandom) -> Result<Vec<u8>, String> {
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes)
        .map_err(|_| "read random bytes".to_string())?;
    if let Some(first) = bytes.first_mut() {
        // Positive, and minimally encoded without a leading zero byte.
        *first = (*first & 0x7f) | 0x40;
    }
    Ok(der(0x02, &bytes))
}

fn name(common_name: &str) -> Vec<u8> {
    let attribute = seq(&[
        &der(0x06, OID_COMMON_NAME),
        &der(0x0c, common_name.as_bytes()),
    ]);
    seq(&[&der(0x31, &attribute)])
}

fn extension(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let oid = der(0x06, oid);
    let value = der(0x04, value);
    if critical {
        seq(&[&oid, &der(0x01, &[0xff]), &value])
    } else {
        seq(&[&oid, &value])
    }
}

/// RFC 5280 §4.1.2.5: UTCTime through 2049, GeneralizedTime after.
fn time(secs: u64) -> Result<Vec<u8>, String> {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil(days);
    let (hour, minute, second) = (rem / 3600, rem / 60 % 60, rem % 60);
    if !(1950..=9999).contains(&year) {
        return Err(format!("year {year} is outside a certificate's range"));
    }
    if year < 2050 {
        let text = format!(
            "{:02}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z",
            year % 100
        );
        Ok(der(0x17, text.as_bytes()))
    } else {
        let text = format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z");
        Ok(der(0x18, text.as_bytes()))
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil(days: u64) -> (u64, u64, u64) {
    // Shifted to 0000-03-01 so a leap day ends its 400-year era's year.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 6);
    out.push(tag);
    let len = body.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        let significant = bytes.get(skip..).unwrap_or(&[]);
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
    out.extend_from_slice(body);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(0);
    body.extend_from_slice(bytes);
    der(0x03, &body)
}

fn pem(label: &str, der: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = Vec::new();
    for chunk in der.chunks(3) {
        let byte = |i: usize| chunk.get(i).copied().unwrap_or(0);
        let (b0, b1, b2) = (byte(0), byte(1), byte(2));
        let sextets = [
            b0 >> 2,
            (b0 & 0x03) << 4 | b1 >> 4,
            (b1 & 0x0f) << 2 | b2 >> 6,
            b2 & 0x3f,
        ];
        for (i, sextet) in sextets.iter().enumerate() {
            text.push(if i <= chunk.len() {
                ALPHABET.get(usize::from(*sextet)).copied().unwrap_or(b'=')
            } else {
                b'='
            });
        }
    }
    let mut out = format!("-----BEGIN {label}-----\n").into_bytes();
    for line in text.chunks(64) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    out
}

/// Create `path` new, recording it in `created` once it exists.
fn write_new(path: &Path, bytes: &[u8], created: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    created.push(path.to_path_buf());
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// A PEM file the mint wrote: a real regular file, and small.
fn read_pem(path: &str) -> Result<Vec<u8>, String> {
    crate::td_fs::read_bounded_real_file(Path::new(path), "PEM file", MAX_PEM)
        .map_err(|e| e.to_string())
}

/// A socket whose reads and writes share one deadline: each sets its
/// timeout to what remains, so a peer trickling bytes cannot hold the
/// connection past the budget.
struct Deadline {
    socket: TcpStream,
    end: Instant,
}

impl Deadline {
    fn new(socket: TcpStream, start: Instant, budget: Duration) -> Self {
        Self {
            socket,
            // An unrepresentable end is no deadline at all: none left.
            end: start.checked_add(budget).unwrap_or(start),
        }
    }

    fn arm(
        &self,
        set: impl Fn(&TcpStream, Option<Duration>) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection deadline passed",
            ));
        }
        set(&self.socket, Some(left))
    }
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm(TcpStream::set_read_timeout)?;
        self.socket.read(buf)
    }
}

impl Write for Deadline {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm(TcpStream::set_write_timeout)?;
        self.socket.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

// ---- serve ------------------------------------------------------------------

/// An empty type: `serve` returns only its error.
enum Never {}

struct Serve {
    listen: SocketAddr,
    config: Arc<ServerConfig>,
    root: PathBuf,
    budget: Duration,
}

/// One held connection slot, given back however its thread ends.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>) -> Option<Self> {
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(active.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn parse_serve(args: &[String]) -> Result<Serve, String> {
    let values = flags(args, &["listen", "cert", "key", "root"])?;
    let [listen, cert, key, root] = values.as_slice() else {
        return Err("serve takes --listen, --cert, --key and --root".into());
    };
    let listen = listen
        .parse()
        .map_err(|e| format!("--listen {listen:?}: {e}"))?;
    let chain = CertificateDer::pem_slice_iter(&read_pem(cert)?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{cert}: {e}"))?;
    if chain.is_empty() {
        return Err(format!("{cert} holds no certificate"));
    }
    let key = PrivateKeyDer::from_pem_slice(&read_pem(key)?).map_err(|e| format!("{key}: {e}"))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("server identity: {e}"))?;
    Ok(Serve {
        listen,
        config: Arc::new(config),
        root: PathBuf::from(root),
        budget: CONNECTION_BUDGET,
    })
}

impl Serve {
    fn run(self) -> Result<Never, String> {
        let listener =
            TcpListener::bind(self.listen).map_err(|e| format!("listen {}: {e}", self.listen))?;
        self.accept(listener)
    }

    fn accept(self, listener: TcpListener) -> Result<Never, String> {
        let root = Arc::new(self.root);
        let active = Arc::new(AtomicUsize::new(0));
        loop {
            let socket = match listener.accept() {
                Ok((socket, _)) => (socket, Instant::now()),
                // A transient refusal (descriptors, an aborted handshake)
                // must not end the origin.
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let Some(slot) = Slot::take(&active) else {
                continue;
            };
            let (config, root, budget) = (self.config.clone(), root.clone(), self.budget);
            // A failed spawn drops the closure, and the slot with it.
            let _ = std::thread::Builder::new().spawn(move || {
                let _slot = slot;
                let (socket, start) = socket;
                // Silent per connection; the client sees the failure.
                let _ = respond(config, &root, Deadline::new(socket, start, budget));
            });
        }
    }
}

fn respond(config: Arc<ServerConfig>, root: &Path, socket: Deadline) -> Result<(), String> {
    let connection = ServerConnection::new(config).map_err(|e| e.to_string())?;
    let mut stream = StreamOwned::new(connection, socket);
    let head = read_head(&mut stream)?;
    let response = match request_path(&head).and_then(|name| file(root, name)) {
        Some((body, kind)) => {
            let mut out = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            out.extend(body);
            out
        }
        None => {
            b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    stream.write_all(&response).map_err(|e| e.to_string())?;
    stream.conn.send_close_notify();
    stream.flush().map_err(|e| e.to_string())
}

fn read_head(stream: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > MAX_HEAD {
            return Err("request head too long".into());
        }
        let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before the request ended".into());
        }
        head.extend_from_slice(buf.get(..n).unwrap_or(&[]));
    }
    Ok(head)
}

/// The flat file name a `GET /NAME HTTP/1.x` request line asks for.
fn request_path(head: &[u8]) -> Option<&str> {
    let line = head.split(|&b| b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut words = line.split(' ');
    let (method, target, version) = (words.next()?, words.next()?, words.next()?);
    if method != "GET" || !version.starts_with("HTTP/1.") || words.next().is_some() {
        return None;
    }
    let name = target.strip_prefix('/')?;
    let flat = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    flat.then_some(name)
}

/// A real regular file directly in `root`, and its content type; any
/// refusal (a symlink, a FIFO, a file over the bound) is a 404.
fn file(root: &Path, name: &str) -> Option<(Vec<u8>, &'static str)> {
    let body = crate::td_fs::read_bounded_real_file(&root.join(name), "document", MAX_BODY).ok()?;
    let kind = if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    Some((body, kind))
}

// ---- probe ------------------------------------------------------------------

struct Probe {
    connect: SocketAddr,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    path: String,
    expect: String,
}

fn parse_probe(args: &[String]) -> Result<Probe, String> {
    let values = flags(args, &["connect", "server-name", "ca", "path", "expect"])?;
    let [connect, server_name, ca, path, expect] = values.as_slice() else {
        return Err("probe takes --connect, --server-name, --ca, --path and --expect".into());
    };
    let connect = connect
        .parse()
        .map_err(|e| format!("--connect {connect:?}: {e}"))?;
    let server_name = ServerName::try_from(server_name.to_string())
        .map_err(|e| format!("--server-name {server_name:?}: {e}"))?;
    if !path.starts_with('/') || path.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(format!("--path {path:?} is not an absolute request path"));
    }
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&read_pem(ca)?) {
        let cert = cert.map_err(|e| format!("{ca}: {e}"))?;
        roots.add(cert).map_err(|e| format!("{ca}: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!("{ca} holds no certificate"));
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Probe {
        connect,
        server_name,
        config: Arc::new(config),
        path: path.to_string(),
        expect: expect.to_string(),
    })
}

impl Probe {
    fn run(self) -> Result<(), String> {
        let body = self.fetch()?;
        let text = String::from_utf8_lossy(&body);
        let (status, content) = text
            .split_once("\r\n\r\n")
            .ok_or("the response has no end of head")?;
        let status_line = status.lines().next().unwrap_or("");
        let mut words = status_line.split(' ');
        let ok =
            words.next().is_some_and(|v| v.starts_with("HTTP/1.")) && words.next() == Some("200");
        if !ok {
            return Err(format!("{}: {status_line:?}", self.path));
        }
        if !content.contains(&self.expect) {
            return Err(format!("{}: the body lacks {:?}", self.path, self.expect));
        }
        Ok(())
    }

    fn fetch(&self) -> Result<Vec<u8>, String> {
        let start = Instant::now();
        let socket = TcpStream::connect_timeout(&self.connect, CONNECTION_BUDGET)
            .map_err(|e| format!("connect {}: {e}", self.connect))?;
        let socket = Deadline::new(socket, start, CONNECTION_BUDGET);
        let connection = ClientConnection::new(self.config.clone(), self.server_name.clone())
            .map_err(|e| e.to_string())?;
        let mut stream = StreamOwned::new(connection, socket);
        let host = self.server_name.to_str();
        let request = format!("GET {} HTTP/1.0\r\nHost: {host}\r\n\r\n", self.path);
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| format!("TLS to {}: {e}", self.connect))?;
        let mut body = Vec::new();
        // The origin ends with close_notify; a bare close is a truncation.
        (&mut stream)
            .take(MAX_BODY + MAX_HEAD as u64)
            .read_to_end(&mut body)
            .map_err(|e| format!("TLS from {}: {e}", self.connect))?;
        Ok(body)
    }
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

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A served origin on an ephemeral loopback port, with its mint.
    struct Origin {
        dir: crate::test_temp::TempDir,
        addr: SocketAddr,
    }

    fn origin() -> Origin {
        origin_with(CONNECTION_BUDGET)
    }

    fn origin_with(budget: Duration) -> Origin {
        let dir = crate::test_temp::TempDir::new("test-https").unwrap();
        mint(dir.path()).unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("content.html"), "<title>SENTINEL</title>").unwrap();
        std::fs::write(docs.join("note.txt"), "plain").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.txt"), docs.join("link.txt")).unwrap();
        std::fs::write(docs.join("big.txt"), vec![b'x'; MAX_BODY as usize + 1]).unwrap();
        let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        let mut serve = parse_serve(&args(&[
            "--listen",
            "127.0.0.1:0",
            "--cert",
            &path("server.pem"),
            "--key",
            &path("server.key"),
            "--root",
            docs.to_str().unwrap(),
        ]))
        .unwrap();
        serve.budget = budget;
        let listener = TcpListener::bind(serve.listen).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || serve.accept(listener));
        Origin { dir, addr }
    }

    fn probe(
        origin: &Origin,
        ca: &Path,
        name: &str,
        path: &str,
        expect: &str,
    ) -> Result<(), String> {
        parse_probe(&args(&[
            "--connect",
            &origin.addr.to_string(),
            "--server-name",
            name,
            "--ca",
            ca.to_str().unwrap(),
            "--path",
            path,
            "--expect",
            expect,
        ]))?
        .run()
    }

    #[test]
    fn the_probe_verifies_the_minted_chain_and_reads_the_document() {
        let origin = origin();
        let ca = origin.dir.path().join("ca.pem");
        probe(&origin, &ca, "localhost", "/content.html", "SENTINEL").unwrap();
        probe(&origin, &ca, "127.0.0.1", "/note.txt", "plain").unwrap();
        let wrong = probe(&origin, &ca, "localhost", "/content.html", "ABSENT").unwrap_err();
        assert!(wrong.contains("lacks"), "{wrong}");
    }

    #[test]
    fn the_probe_refuses_another_authority_and_another_name() {
        let origin = origin();
        let other = crate::test_temp::TempDir::new("test-https-other").unwrap();
        mint(other.path()).unwrap();
        let foreign = probe(
            &origin,
            &other.path().join("ca.pem"),
            "localhost",
            "/content.html",
            "SENTINEL",
        )
        .unwrap_err();
        // Same authority name, other key: the signature cannot verify.
        assert!(foreign.contains("BadSignature"), "{foreign}");
        let ca = origin.dir.path().join("ca.pem");
        let name = probe(&origin, &ca, "example.com", "/content.html", "SENTINEL").unwrap_err();
        assert!(name.contains("not valid for name"), "{name}");
        // The server's own certificate is no authority.
        let leaf = probe(
            &origin,
            &origin.dir.path().join("server.pem"),
            "localhost",
            "/content.html",
            "SENTINEL",
        );
        assert!(leaf.is_err());
    }

    #[test]
    fn the_origin_serves_only_flat_regular_files() {
        let origin = origin();
        let ca = origin.dir.path().join("ca.pem");
        for path in [
            "/link.txt",
            "/big.txt",
            "/../outside.txt",
            "/missing.html",
            "/",
            "/.hidden",
        ] {
            let refused = probe(&origin, &ca, "localhost", path, "outside").unwrap_err();
            assert!(refused.contains("404"), "{path}: {refused}");
        }
    }

    #[test]
    fn identity_files_must_be_real_regular_files() {
        let dir = crate::test_temp::TempDir::new("test-https-files").unwrap();
        mint(dir.path()).unwrap();
        let link = dir.path().join("linked.pem");
        std::os::unix::fs::symlink(dir.path().join("server.pem"), &link).unwrap();
        let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        let serve = |cert: &str| {
            parse_serve(&args(&[
                "--listen",
                "127.0.0.1:0",
                "--cert",
                cert,
                "--key",
                &path("server.key"),
                "--root",
                dir.path().to_str().unwrap(),
            ]))
        };
        assert!(serve(&path("server.pem")).is_ok());
        let refused = serve(link.to_str().unwrap()).err().unwrap();
        assert!(refused.contains("linked.pem"), "{refused}");
        let big = dir.path().join("big.pem");
        std::fs::write(&big, vec![b'x'; MAX_PEM as usize + 1]).unwrap();
        let refused = serve(big.to_str().unwrap()).err().unwrap();
        assert!(refused.contains("exceeds"), "{refused}");
    }

    /// A peer that trickles a byte at a time, each well inside any
    /// per-read timeout, is still cut off when the connection's one
    /// budget runs out.
    #[test]
    fn a_trickling_client_is_cut_off_at_the_connection_deadline() {
        let origin = origin_with(Duration::from_secs(1));
        let mut client = TcpStream::connect(origin.addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let start = Instant::now();
        // A handshake record header promising more than ever arrives.
        let _ = client.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00]);
        let mut closed = None;
        while start.elapsed() < Duration::from_secs(6) {
            let _ = client.write_all(&[0]);
            match client.read(&mut [0u8; 64]) {
                Ok(0) => {}
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(_) => {}
            }
            closed = Some(start.elapsed());
            break;
        }
        let closed = closed.expect("the origin kept a trickling connection for 6 s");
        assert!(closed < Duration::from_secs(3), "{closed:?}");
    }

    #[test]
    fn a_failed_mint_leaves_nothing_it_created() {
        let dir = crate::test_temp::TempDir::new("test-https-partial").unwrap();
        std::fs::write(dir.path().join("server.key"), "earlier").unwrap();
        let error = mint(dir.path()).unwrap_err();
        assert!(error.contains("server.key"), "{error}");
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["server.key"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("server.key")).unwrap(),
            "earlier"
        );
    }

    #[test]
    fn mint_creates_only_new_private_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_temp::TempDir::new("test-https-mint").unwrap();
        mint(dir.path()).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["ca.pem", "server.key", "server.pem"]);
        for name in &names {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        // td-jail admits the authority into Firefox's /etc only as exactly
        // one PEM certificate.
        let ca = std::fs::read_to_string(dir.path().join("ca.pem")).unwrap();
        assert!(ca.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(ca.ends_with("-----END CERTIFICATE-----\n"));
        assert_eq!(ca.matches("-----BEGIN CERTIFICATE-----").count(), 1);
        assert!(mint(dir.path()).unwrap_err().contains("ca.pem"));
    }

    #[test]
    fn the_validity_is_one_day_from_an_hour_before_the_mint() {
        // 2026-10-20T12:00:00Z.
        let minted = mint_at(1_792_497_600).unwrap();
        for cert in [&minted.ca, &minted.server] {
            let window = b"\x17\x0d261020110000Z\x17\x0d261021120000Z";
            assert!(cert.windows(window.len()).any(|w| w == window));
        }
    }

    #[test]
    fn civil_dates_and_time_encodings() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_746), (2026, 10, 20));
        assert_eq!(time(2_524_607_999).unwrap(), der(0x17, b"491231235959Z"));
        assert_eq!(time(2_524_608_000).unwrap(), der(0x18, b"20500101000000Z"));
    }

    #[test]
    fn der_lengths_and_pem_lines() {
        assert_eq!(der(0x04, &[0; 2]), [0x04, 0x02, 0, 0]);
        assert_eq!(
            der(0x04, &[0; 0x80]).get(..3),
            Some(&[0x04, 0x81, 0x80][..])
        );
        assert_eq!(
            der(0x04, &[0; 0x100]).get(..4),
            Some(&[0x04, 0x82, 0x01, 0x00][..])
        );
        assert_eq!(
            pem("X", b"foob"),
            b"-----BEGIN X-----\nZm9vYg==\n-----END X-----\n"
        );
        assert_eq!(
            pem("X", b"fooba"),
            b"-----BEGIN X-----\nZm9vYmE=\n-----END X-----\n"
        );
        let long = pem("X", &[0; 60]);
        let text = String::from_utf8(long).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].len(), 64);
        assert_eq!(lines[2].len(), 16);
    }

    #[test]
    fn flags_are_exact() {
        assert!(flags(&args(&["--a", "1"]), &["a", "b"]).is_err());
        assert!(flags(&args(&["--a", "1", "--a", "2"]), &["a"]).is_err());
        assert!(flags(&args(&["--c", "1"]), &["a"]).is_err());
        assert!(flags(&args(&["--a"]), &["a"]).is_err());
        assert_eq!(
            flags(&args(&["--b", "2", "--a", "1"]), &["a", "b"]).unwrap(),
            ["1", "2"]
        );
    }
}
