// td-subst — the network + crypto half of td's OWN substitute (binary-cache) server. A
// sibling of feed/ (td-feed) and fetch/ (td-fetch) reusing the same pure-Rust HTTP(S)
// stack (ureq + rustls/ring + sha2); ed25519 signing/verification is ring's. Where
// td-feed mirrors SOURCE downloads (content-addressed, self-verifying), td-subst serves
// BUILT /td/store outputs: the dependency-free engine (`td-builder subst-export`) writes a
// serve-able directory — a td-native `<basename>.narinfo` (StorePath/NarHash/NarSize/
// NarFile/References) + `nar/<narhash>.nar` per closure member — and this tool SIGNS,
// SERVES, and FETCHES+VERIFIES it. Input-addressed build outputs are NOT self-verifying
// (the path hash comes from the .drv, not the bytes), so trust is a signature: the
// publisher signs each narinfo with an ed25519 key and the consumer verifies against a
// PINNED public key, then re-checks the fetched nar's sha256 against the signed NarHash.
//
//   td-subst keygen PRIV PUB      Generate an ed25519 keypair: PRIV = pkcs8 (publisher
//                                 secret, never committed, created 0600), PUB = hex public
//                                 key (pinned). Both are created EXCLUSIVELY: an existing
//                                 path is an error, not a replacement, because silently
//                                 truncating a signing key leaves nothing matching what
//                                 consumers have pinned.
//   td-subst sign DIR PRIV        Append `Sig: <hex>` to every <…>.narinfo in DIR, signed
//                                 over the narinfo body (everything before the Sig line).
//   td-subst serve DIR ADDR       Static, traversal-safe HTTP server for the export dir
//                                 (narinfos + nar/*). The CONSUMER verifies; no egress.
//   td-subst fetch URL NAME OUT PUB   GET URL/NAME.narinfo, verify its Sig against PUB,
//                                 GET the referenced nar, verify sha256 == NarHash, and
//                                 write NAME.narinfo + the nar into OUT. (td-builder then
//                                 restores the nar with `nar-restore` + registers it.)
//   td-subst selftest             Self-contained LOOPBACK round-trip (offline): keygen,
//                                 build+sign a one-entry export dir, serve it, fetch it
//                                 back + verify. Also asserts the guards are load-bearing:
//                                 a tampered narinfo reds (signature), a corrupted nar reds
//                                 (NarHash), and a wrong public key reds (signature).
use crate::sig::{from_hex, keygen, sign_msg, to_hex, verify_msg, write_keypair};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SERVE_WORKERS: usize = 8;
const SERVE_WORKER_STACK_BYTES: usize = 512 * 1024;
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
const REQUEST_IO_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_DEADLINE: Duration = Duration::from_secs(30 * 60);
const MAX_NAR_BYTES: u64 = 16 * 1024 * 1024 * 1024;

fn die(msg: String) -> ! {
    eprintln!("td-subst: {msg}");
    std::process::exit(1);
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    if file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len()
        > MAX_NAR_BYTES
    {
        return Err(format!(
            "substitute hash input {} exceeds its {MAX_NAR_BYTES}-byte limit",
            path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > MAX_NAR_BYTES {
            return Err(format!(
                "substitute hash input {} exceeds its {MAX_NAR_BYTES}-byte limit",
                path.display()
            ));
        }
        let bytes = buf
            .get(..n)
            .ok_or_else(|| "substitute hash read exceeded its buffer".to_string())?;
        hasher.update(bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---- narinfo ----

/// Split a narinfo into its signed BODY (everything up to the `Sig:` line) and the Sig hex,
/// if present. `sign` appends `Sig: <hex>\n` last, so the body is byte-stable.
fn split_sig(text: &str) -> (&str, Option<&str>) {
    if let Some(pos) = text.find("\nSig: ") {
        let body = &text[..pos + 1]; // include the trailing '\n' of the last body line
        let sig = text[pos + 1..].trim().strip_prefix("Sig: ").map(str::trim);
        (body, sig)
    } else {
        (text, None)
    }
}

/// Read a single `Key: value` field out of a narinfo body.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(": ")))
}

// ---- path safety (mirror of td-feed) ----

/// Map a request path to a file under ROOT, rejecting traversal / absolute components.
fn safe_path(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    if rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(root.join(rel))
}

fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut t = dst.as_os_str().to_os_string();
    t.push(format!(".{}.td-subst-tmp", std::process::id()));
    let tmp = PathBuf::from(t);
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("rename {}: {e}", dst.display()))
}

// ---- HTTP (mirror of td-feed) ----

fn try_get(url: &str) -> Result<Vec<u8>, String> {
    crate::http::get_body(url)
}

fn write_before(conn: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "substitute response deadline expired",
            )
        })?;
        conn.set_write_timeout(Some(remaining.min(REQUEST_IO_TIMEOUT)))?;
        let count = conn.write(bytes)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "substitute response made no progress",
            ));
        }
        bytes = bytes.get(count..).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "substitute response write exceeded its buffer",
            )
        })?;
    }
    Ok(())
}

fn flush_before(conn: &mut TcpStream, deadline: Instant) -> io::Result<()> {
    let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "substitute response deadline expired",
        )
    })?;
    conn.set_write_timeout(Some(remaining.min(REQUEST_IO_TIMEOUT)))?;
    conn.flush()
}

fn respond(conn: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + RESPONSE_DEADLINE;
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write_before(conn, head.as_bytes(), deadline)?;
    write_before(conn, body, deadline)?;
    flush_before(conn, deadline)
}

fn respond_file(conn: &mut TcpStream, file: &mut File, len: u64) -> io::Result<()> {
    let deadline = Instant::now() + RESPONSE_DEADLINE;
    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
    write_before(conn, head.as_bytes(), deadline)?;
    let mut remaining = len;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let width = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let chunk = buf.get_mut(..width).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "substitute response read exceeded its buffer",
            )
        })?;
        let count = file.read(chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "substitute ended while it was served",
            ));
        }
        let bytes = chunk.get(..count).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "substitute response read exceeded its buffer",
            )
        })?;
        write_before(conn, bytes, deadline)?;
        remaining = remaining.saturating_sub(u64::try_from(count).unwrap_or(u64::MAX));
    }
    flush_before(conn, deadline)
}

fn read_request_head(conn: &mut TcpStream) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + REQUEST_IO_TIMEOUT;
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request deadline expired"))?;
        conn.set_read_timeout(Some(remaining))?;
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its header",
            ));
        }
        let bytes = chunk.get(..n).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "request read exceeded its buffer")
        })?;
        if head.len().saturating_add(bytes.len()) > MAX_REQUEST_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeds its byte limit",
            ));
        }
        head.extend_from_slice(bytes);
        if head.windows(4).any(|window| window == b"\r\n\r\n")
            || head.windows(2).any(|window| window == b"\n\n")
        {
            return Ok(head);
        }
    }
}

/// Serve one request: GET /<path> streams ROOT/<path> (traversal-safe). The substitute
/// protocol puts verification on the CONSUMER (signature + NarHash), so serve is a plain,
/// safe static file server.
fn handle_conn(mut conn: TcpStream, root: &Path) -> io::Result<()> {
    conn.set_write_timeout(Some(REQUEST_IO_TIMEOUT))?;
    let head = read_request_head(&mut conn)?;
    let line_end = head
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let req_line = std::str::from_utf8(head.get(..line_end).unwrap_or_default())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 request line"))?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        return respond(&mut conn, 405, "Method Not Allowed", b"method not allowed\n");
    }
    let full = match safe_path(root, target.trim_start_matches('/')) {
        Some(p) => p,
        None => return respond(&mut conn, 400, "Bad Request", b"bad path\n"),
    };
    match File::open(&full) {
        Ok(mut file) => {
            let len = file.metadata()?.len();
            if len > MAX_NAR_BYTES {
                return respond(&mut conn, 413, "Too Large", b"substitute too large\n");
            }
            respond_file(&mut conn, &mut file, len)
        }
        Err(_) => respond(&mut conn, 404, "Not Found", b"not found\n"),
    }
}

fn serve_worker(listener: TcpListener, root: Arc<PathBuf>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let _ = handle_conn(conn, &root);
    }
}

fn serve_loop(listener: TcpListener, root: Arc<PathBuf>) {
    for worker in 1..SERVE_WORKERS {
        let Ok(worker_listener) = listener.try_clone() else {
            break;
        };
        let worker_root = Arc::clone(&root);
        let _ = std::thread::Builder::new()
            .name(format!("td-subst-{worker}"))
            .stack_size(SERVE_WORKER_STACK_BYTES)
            .spawn(move || serve_worker(worker_listener, worker_root));
    }
    serve_worker(listener, root);
}

struct RemoveOnDrop {
    path: PathBuf,
    _directory_lock: File,
    _reservation: File,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sweep_download_temps(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let abandoned = entries.flatten().filter(|entry| {
        let name = entry.file_name();
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(name.as_os_str());
        bytes.starts_with(b".td-subst-download-") && bytes.ends_with(b".tmp")
    });
    for entry in abandoned.take(4096) {
        let _ = std::fs::remove_file(entry.path());
    }
}

fn download_nar(url: &str, dst: &Path, want: &str) -> Result<(), String> {
    static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
    let parent = dst
        .parent()
        .ok_or_else(|| format!("substitute path {} has no parent", dst.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let lock_path = parent.join(".td-subst-download.lock");
    let directory_lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    directory_lock
        .lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    sweep_download_temps(parent);
    if file_sha256(dst).ok().as_deref() == Some(want) {
        return Ok(());
    }
    let (path, reservation) = loop {
        let nonce = NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".td-subst-download-{}-{nonce}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => break (path, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("reserve {}: {e}", path.display())),
        }
    };
    let tmp = RemoveOnDrop {
        path,
        _directory_lock: directory_lock,
        _reservation: reservation,
    };
    crate::http::get_to_file(url, &tmp.path, MAX_NAR_BYTES)?;
    let got = file_sha256(&tmp.path)?;
    if got != want {
        return Err(format!(
            "nar sha256 mismatch\n  want {want}\n  got  {got}"
        ));
    }
    std::fs::rename(&tmp.path, dst).map_err(|e| format!("publish {}: {e}", dst.display()))
}

// ---- commands ----

/// Append `Sig: <hex>` to every `*.narinfo` in DIR, signed over the body. Returns the count.
fn sign_dir(dir: &Path, pkcs8: &[u8]) -> Result<usize, String> {
    let mut signed = 0;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for e in entries {
        let path = e.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("narinfo") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let (body, existing) = split_sig(&text);
        if existing.is_some() {
            continue; // already signed; don't double-sign
        }
        let sig = sign_msg(pkcs8, body.as_bytes())?;
        let out = format!("{body}Sig: {}\n", to_hex(&sig));
        write_atomic(&path, out.as_bytes())?;
        signed += 1;
    }
    Ok(signed)
}

/// Fetch NAME from BASEURL: verify the narinfo signature against PUBKEY, fetch the
/// referenced nar, verify its sha256 == NarHash, and write both into OUTDIR. Returns the
/// store path the nar restores to (the narinfo's StorePath).
fn fetch(baseurl: &str, name: &str, outdir: &Path, pubkey: &[u8]) -> Result<String, String> {
    let base = baseurl.trim_end_matches('/');
    let ni = String::from_utf8(try_get(&format!("{base}/{name}.narinfo"))?)
        .map_err(|_| "narinfo is not UTF-8".to_string())?;
    let (body, sig_hex) = split_sig(&ni);
    let sig = from_hex(sig_hex.ok_or("narinfo has no Sig line")?)?;
    if !verify_msg(pubkey, body.as_bytes(), &sig) {
        return Err(format!("narinfo signature does not verify for {name}"));
    }
    let store_path = field(body, "StorePath").ok_or("narinfo has no StorePath")?.to_string();
    let narhash = field(body, "NarHash").ok_or("narinfo has no NarHash")?;
    let narfile = field(body, "NarFile").ok_or("narinfo has no NarFile")?;
    let want = narhash.strip_prefix("sha256:").unwrap_or(narhash);
    // Safe: narfile comes from the SIGNED body, but re-check it can't escape OUTDIR.
    let nar_dst = safe_path(outdir, narfile).ok_or_else(|| format!("unsafe NarFile {narfile:?}"))?;
    download_nar(&format!("{base}/{narfile}"), &nar_dst, want)
        .map_err(|e| format!("{e} for {name}"))?;
    write_atomic(&outdir.join(format!("{name}.narinfo")), ni.as_bytes())?;
    Ok(store_path)
}

fn selftest() {
    // 1. A keypair (publisher secret + pinned-style public key).
    let (pkcs8, pubkey) = keygen().unwrap_or_else(|e| die(e));

    // 2. Build a one-entry export dir (the "nar" is opaque bytes here — the subst layer
    //    moves + verifies bytes; that the nar RESTORES to a store path is td-builder's
    //    nar-restore, exercised by the from-source gate, not this self-contained selftest).
    let blob: Vec<u8> = (0u16..4096).map(|x| (x % 251) as u8).collect();
    let narhash = hex_sha256(&blob);
    let store_path = "/gnu/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-thing-1.0";
    let base = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-thing-1.0";
    let dir = std::env::temp_dir().join(format!("td-subst-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let narfile = format!("nar/{narhash}.nar");
    write_atomic(&dir.join(&narfile), &blob).unwrap_or_else(|e| die(e));
    let body = format!(
        "StorePath: {store_path}\nNarHash: sha256:{narhash}\nNarSize: {}\nNarFile: {narfile}\nReferences: \n",
        blob.len()
    );
    let ni = dir.join(format!("{base}.narinfo"));
    write_atomic(&ni, body.as_bytes()).unwrap_or_else(|e| die(e));

    // 3. Sign the export dir.
    let n = sign_dir(&dir, &pkcs8).unwrap_or_else(|e| die(format!("sign: {e}")));
    if n != 1 {
        die(format!("expected to sign 1 narinfo, signed {n}"));
    }

    // 4. Serve it on a loopback port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind feed");
    let port = listener.local_addr().expect("addr").port();
    {
        let root = Arc::new(dir.clone());
        std::thread::spawn(move || serve_loop(listener, root));
    }
    let url = format!("http://127.0.0.1:{port}");

    // 5. Fetch it back THROUGH the server + verify (signature + NarHash).
    let out = std::env::temp_dir().join(format!("td-subst-selftest-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let sp = fetch(&url, base, &out, &pubkey).unwrap_or_else(|e| die(format!("fetch: {e}")));
    if sp != store_path {
        die(format!("fetched StorePath {sp} != {store_path}"));
    }
    if std::fs::read(out.join(&narfile)).unwrap_or_else(|e| die(e.to_string())) != blob {
        die("fetched nar bytes differ from the served artifact".into());
    }

    // 6. SELF-DISCRIMINATION (signature): tamper a body byte of the served narinfo,
    //    keeping its Sig — fetch must reject (the signature no longer covers the body).
    {
        let text = std::fs::read_to_string(&ni).unwrap();
        let tampered = text.replacen("thing-1.0", "thing-9.9", 1);
        assert_ne!(text, tampered);
        std::fs::write(&ni, &tampered).unwrap();
        if fetch(&url, base, &out, &pubkey).is_ok() {
            die("fetch ACCEPTED a tampered narinfo — the signature is not load-bearing".into());
        }
        std::fs::write(&ni, &text).unwrap(); // restore
    }

    // 7. SELF-DISCRIMINATION (NarHash): corrupt the served nar — fetch must reject.
    {
        let mut bad = blob.clone();
        bad[0] ^= 0xff;
        std::fs::write(dir.join(&narfile), &bad).unwrap();
        // Remove the previously verified local publication so this assertion
        // exercises the corrupted server body. Reusing that valid file is the
        // intended no-egress path for concurrent and repeated fetches.
        let _ = std::fs::remove_file(out.join(&narfile));
        if fetch(&url, base, &out, &pubkey).is_ok() {
            die("fetch ACCEPTED a corrupted nar — the NarHash check is not load-bearing".into());
        }
        std::fs::write(dir.join(&narfile), &blob).unwrap(); // restore
    }

    // 8. SELF-DISCRIMINATION (wrong key): a DIFFERENT public key must reject the signature.
    {
        let (_other_priv, other_pub) = keygen().unwrap_or_else(|e| die(e));
        if fetch(&url, base, &out, &other_pub).is_ok() {
            die("fetch ACCEPTED a narinfo under the WRONG public key — verification is not load-bearing".into());
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
    println!(
        "td-subst: selftest OK — keygen, signed + served a {}-byte nar (sha256 {}) on \
         127.0.0.1:{}, fetched it back + verified (ed25519 signature + NarHash); a tampered \
         narinfo, a corrupted nar, and a wrong public key each red the fetch",
        blob.len(),
        narhash,
        port
    );
}

pub fn run(a: &[String]) {
    match a.get(1).map(String::as_str) {
        Some("keygen") if a.len() == 4 => {
            let (priv_path, pub_path) = (&a[2], &a[3]);
            let (pkcs8, pubkey) = keygen().unwrap_or_else(|e| die(e));
            write_keypair(priv_path, &pkcs8, pub_path, &pubkey).unwrap_or_else(|e| die(e));
            println!(
                "td-subst: keygen OK — private (pkcs8, 0600) -> {priv_path}, \
                 public (hex) -> {pub_path}"
            );
        }
        Some("sign") if a.len() == 4 => {
            let (dir, priv_path) = (PathBuf::from(&a[2]), &a[3]);
            let pkcs8 = std::fs::read(priv_path).unwrap_or_else(|e| die(format!("read {priv_path}: {e}")));
            match sign_dir(&dir, &pkcs8) {
                Ok(n) => println!("td-subst: sign OK — signed {n} narinfo(s) in {}", dir.display()),
                Err(e) => die(e),
            }
        }
        Some("serve") if a.len() == 4 => {
            let (dir, addr) = (PathBuf::from(&a[2]), &a[3]);
            let listener =
                TcpListener::bind(addr.as_str()).unwrap_or_else(|e| die(format!("bind {addr}: {e}")));
            let bound = listener.local_addr().unwrap_or_else(|e| die(format!("local_addr: {e}")));
            println!("td-subst: serving {} on http://{}/", dir.display(), bound);
            let _ = io::stdout().flush();
            serve_loop(listener, Arc::new(dir));
        }
        Some("fetch") if a.len() == 6 => {
            let (url, name, outdir, pub_path) = (&a[2], &a[3], PathBuf::from(&a[4]), &a[5]);
            let pub_hex = std::fs::read_to_string(pub_path)
                .unwrap_or_else(|e| die(format!("read {pub_path}: {e}")));
            let pubkey = from_hex(&pub_hex).unwrap_or_else(|e| die(format!("public key: {e}")));
            match fetch(url, name, &outdir, &pubkey) {
                Ok(sp) => println!("td-subst: fetch OK — {name} verified -> {} (StorePath {sp})", outdir.display()),
                Err(e) => die(e),
            }
        }
        Some("selftest") if a.len() == 2 => selftest(),
        _ => {
            eprintln!(
                "usage:\n  td-subst keygen PRIV PUB\n  td-subst sign DIR PRIV\n  \
                 td-subst serve DIR ADDR\n  td-subst fetch URL NAME OUTDIR PUB\n  td-subst selftest"
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{file_sha256, MAX_NAR_BYTES};

    #[test]
    fn hash_rejects_an_oversized_existing_nar_before_reading() {
        let dir = std::env::temp_dir().join(format!(
            "td-subst-oversized-hash-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact.nar");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_NAR_BYTES + 1).unwrap();

        let error = file_sha256(&path).unwrap_err();

        assert!(error.contains("exceeds"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
