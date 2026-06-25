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
//                                 secret, never committed), PUB = hex public key (pinned).
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
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn die(msg: String) -> ! {
    eprintln!("td-subst: {msg}");
    std::process::exit(1);
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("bad hex: {e}")))
        .collect()
}

// ---- ed25519 (ring) ----

fn sign_msg(pkcs8: &[u8], msg: &[u8]) -> Result<Vec<u8>, String> {
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| format!("bad private key: {e}"))?;
    Ok(kp.sign(msg).as_ref().to_vec())
}

fn verify_msg(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, pubkey).verify(msg, sig).is_ok()
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
    let resp = ureq::get(url).call().map_err(|e| format!("GET {url}: {e}"))?;
    let mut body = Vec::new();
    resp.into_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("read {url}: {e}"))?;
    Ok(body)
}

fn respond(conn: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()
}

/// Serve one request: GET /<path> streams ROOT/<path> (traversal-safe). The substitute
/// protocol puts verification on the CONSUMER (signature + NarHash), so serve is a plain,
/// safe static file server.
fn handle_conn(mut conn: TcpStream, root: &Path) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut req_line = String::new();
    reader.read_line(&mut req_line)?;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
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
    match std::fs::read(&full) {
        Ok(b) => respond(&mut conn, 200, "OK", &b),
        Err(_) => respond(&mut conn, 404, "Not Found", b"not found\n"),
    }
}

fn serve_loop(listener: TcpListener, root: Arc<PathBuf>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let root = Arc::clone(&root);
        std::thread::spawn(move || {
            let _ = handle_conn(conn, &root);
        });
    }
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
    let nar = try_get(&format!("{base}/{narfile}"))?;
    let got = hex_sha256(&nar);
    if got != want {
        return Err(format!("nar sha256 mismatch for {name}\n  want {want}\n  got  {got}"));
    }
    // Safe: narfile comes from the SIGNED body, but re-check it can't escape OUTDIR.
    let nar_dst = safe_path(outdir, narfile).ok_or_else(|| format!("unsafe NarFile {narfile:?}"))?;
    write_atomic(&nar_dst, &nar)?;
    write_atomic(&outdir.join(format!("{name}.narinfo")), ni.as_bytes())?;
    Ok(store_path)
}

fn keygen() -> (Vec<u8>, Vec<u8>) {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("generate ed25519 key");
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse generated key");
    (pkcs8.as_ref().to_vec(), kp.public_key().as_ref().to_vec())
}

fn selftest() {
    // 1. A keypair (publisher secret + pinned-style public key).
    let (pkcs8, pubkey) = keygen();

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
        if fetch(&url, base, &out, &pubkey).is_ok() {
            die("fetch ACCEPTED a corrupted nar — the NarHash check is not load-bearing".into());
        }
        std::fs::write(dir.join(&narfile), &blob).unwrap(); // restore
    }

    // 8. SELF-DISCRIMINATION (wrong key): a DIFFERENT public key must reject the signature.
    {
        let (_other_priv, other_pub) = keygen();
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

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("keygen") if a.len() == 4 => {
            let (priv_path, pub_path) = (&a[2], &a[3]);
            let (pkcs8, pubkey) = keygen();
            std::fs::write(priv_path, &pkcs8).unwrap_or_else(|e| die(format!("write {priv_path}: {e}")));
            std::fs::write(pub_path, format!("{}\n", to_hex(&pubkey)))
                .unwrap_or_else(|e| die(format!("write {pub_path}: {e}")));
            println!("td-subst: keygen OK — private (pkcs8) -> {priv_path}, public (hex) -> {pub_path}");
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
