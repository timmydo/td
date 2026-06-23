// td-feed — td's OWN local HTTP mirror of every artifact this repo downloads over the
// network (the td-fetch seed blobs, the url-fetch source tarballs, and all
// static.crates.io `.crate` deps). It is a sibling of fetch/ (td-fetch) and reuses the
// same pure-Rust HTTP(S)+sha256 stack (ureq + rustls/ring + sha2); the mirror server
// itself uses only std::net, so it adds no crate to the vendored closure.
//
// The INDEX is a pinned table, one artifact per line:
//
//     <path>  <url>  <sha256-hex>
//
// where <path> is the mirror path the feed serves (the upstream `<host>/<path>`, so
// different hosts never collide) and <url> is where `warm` fetches it from. Three modes:
//
//   td-feed warm INDEX STORE           HOST PREP (network egress ALLOWED): for every
//                                      index entry, GET <url>, verify its sha256 ==
//                                      <sha256>, and write it to STORE/<path>. Entries
//                                      already warm + matching are skipped. This is the
//                                      ONE egress point; it runs on the host, never in
//                                      the offline loop.
//
//   td-feed serve STORE INDEX ADDR     OFFLINE loopback mirror: serve STORE over HTTP on
//                                      ADDR (e.g. 127.0.0.1:8787). `GET /<path>` looks
//                                      <path> up in INDEX, reads STORE/<path>, RE-VERIFIES
//                                      its sha256 against the index (verify-on-serve —
//                                      store corruption 500s), and streams it. Unknown
//                                      paths 404. std::net only, no egress.
//
//   td-feed selftest                   Self-contained LOOPBACK round-trip (offline, like
//                                      td-fetch's selftest): stand up an ORIGIN server on
//                                      127.0.0.1, `warm` a one-entry index from it into a
//                                      temp store, `serve` the store on a 2nd loopback
//                                      port, then fetch the artifact back THROUGH the feed
//                                      and verify it. Also asserts the verification is
//                                      load-bearing: a wrong index hash reds `warm`, and a
//                                      corrupted store byte reds `serve` (verify-on-serve).
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One mirror artifact: served at `path`, fetched from `url`, content sha256 `sha256`.
struct Entry {
    path: String,
    url: String,
    sha256: String,
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Parse an index: `<path> <url> <sha256>` per line; `#` comments and blanks ignored.
fn parse_index(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        match (it.next(), it.next(), it.next(), it.next()) {
            (Some(p), Some(u), Some(h), None) => out.push(Entry {
                path: p.to_string(),
                url: u.to_string(),
                sha256: h.to_lowercase(),
            }),
            _ => return Err(format!("malformed index line {}: {line:?}", n + 1)),
        }
    }
    Ok(out)
}

/// Map a mirror path to a store file, rejecting traversal / absolute components so a
/// crafted index path can never escape STORE.
fn store_path(store: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    if rel
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    Some(store.join(rel))
}

/// GET `url` (http/https), returning the body or an error string.
fn try_get(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| format!("GET {url}: {e}"))?;
    let mut body = Vec::new();
    resp.into_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("read {url}: {e}"))?;
    Ok(body)
}

/// Warm one entry into `store`; returns Ok(true) if it was fetched, Ok(false) if it was
/// already warm and verified. Never egresses for an entry already present + matching.
fn warm_one(e: &Entry, store: &Path) -> Result<bool, String> {
    let dst = store_path(store, &e.path).ok_or_else(|| format!("unsafe index path {:?}", e.path))?;
    if let Ok(have) = std::fs::read(&dst) {
        if hex_sha256(&have) == e.sha256 {
            return Ok(false);
        }
    }
    let body = try_get(&e.url)?;
    let got = hex_sha256(&body);
    if got != e.sha256 {
        return Err(format!(
            "sha256 mismatch for {}\n  want {}\n  got  {}",
            e.url, e.sha256, got
        ));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("mkdir {}: {err}", parent.display()))?;
    }
    // Write via a temp + rename so a serve reading concurrently never sees a partial file.
    let tmp = dst.with_extension("td-feed-tmp");
    std::fs::write(&tmp, &body).map_err(|err| format!("write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).map_err(|err| format!("rename {}: {err}", dst.display()))?;
    Ok(true)
}

/// Warm every entry; returns (fetched, already-warm).
fn warm(index: &[Entry], store: &Path) -> Result<(usize, usize), String> {
    let mut fetched = 0;
    let mut warm = 0;
    for e in index {
        if warm_one(e, store)? {
            fetched += 1;
        } else {
            warm += 1;
        }
    }
    Ok((fetched, warm))
}

/// Write an HTTP/1.1 response with `Connection: close`.
fn respond(conn: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()
}

/// Handle one request: route a `GET /<path>` against the index, verify-on-serve, stream.
fn handle_conn(
    mut conn: TcpStream,
    store: &Path,
    map: &HashMap<String, String>,
) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut req_line = String::new();
    reader.read_line(&mut req_line)?;
    // Drain the rest of the request head so the client can write fully.
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
    let path = target.trim_start_matches('/');
    let want = match map.get(path) {
        Some(h) => h,
        None => return respond(&mut conn, 404, "Not Found", b"not in index\n"),
    };
    let full = match store_path(store, path) {
        Some(p) => p,
        None => return respond(&mut conn, 400, "Bad Request", b"bad path\n"),
    };
    let bytes = match std::fs::read(&full) {
        Ok(b) => b,
        Err(_) => return respond(&mut conn, 404, "Not Found", b"not warmed\n"),
    };
    if &hex_sha256(&bytes) != want {
        // verify-on-serve: the store has drifted from the pinned hash — refuse to serve.
        return respond(&mut conn, 500, "Integrity Failure", b"store sha256 mismatch\n");
    }
    respond(&mut conn, 200, "OK", &bytes)
}

/// Run the mirror server forever on `listener`.
fn serve_loop(listener: TcpListener, store: Arc<PathBuf>, map: Arc<HashMap<String, String>>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (store, map) = (Arc::clone(&store), Arc::clone(&map));
        std::thread::spawn(move || {
            let _ = handle_conn(conn, &store, &map);
        });
    }
}

fn index_map(index: &[Entry]) -> HashMap<String, String> {
    index.iter().map(|e| (e.path.clone(), e.sha256.clone())).collect()
}

fn die(msg: String) -> ! {
    eprintln!("td-feed: {msg}");
    std::process::exit(1);
}

fn read_index(path: &str) -> Vec<Entry> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("read index {path}: {e}")));
    parse_index(&text).unwrap_or_else(|e| die(e))
}

/// A one-shot origin responder used only by the selftest.
fn serve_once(conn: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    let _ = conn.read(&mut buf)?;
    respond(conn, 200, "OK", body)
}

fn selftest() {
    // A known artifact (non-trivial bytes so a flipped byte is detectable).
    let blob: Vec<u8> = (0u16..4096).map(|x| (x % 251) as u8).collect();
    let want = hex_sha256(&blob);

    // 1. An ORIGIN server on loopback, serving `blob` at /blob.
    let origin = TcpListener::bind("127.0.0.1:0").expect("bind origin");
    let origin_port = origin.local_addr().expect("addr").port();
    let ob = blob.clone();
    std::thread::spawn(move || loop {
        match origin.accept() {
            Ok((mut c, _)) => {
                let _ = serve_once(&mut c, &ob);
            }
            Err(_) => break,
        }
    });

    // 2. A one-entry index: serve it at origin.invalid/blob, fetch it from the origin.
    let path = "origin.invalid/blob".to_string();
    let index = vec![Entry {
        path: path.clone(),
        url: format!("http://127.0.0.1:{origin_port}/blob"),
        sha256: want.clone(),
    }];

    // 3. Warm into a fresh temp store.
    let store = std::env::temp_dir().join(format!("td-feed-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    let (fetched, _) = warm(&index, &store).unwrap_or_else(|e| die(format!("warm: {e}")));
    if fetched != 1 {
        die(format!("expected to fetch 1 artifact, fetched {fetched}"));
    }
    let stored = store_path(&store, &path).unwrap();
    if hex_sha256(&std::fs::read(&stored).expect("read stored")) != want {
        die("warmed store artifact does not match its pinned sha256".into());
    }

    // 4. Serve the store on a 2nd loopback port.
    let feed = TcpListener::bind("127.0.0.1:0").expect("bind feed");
    let feed_port = feed.local_addr().expect("addr").port();
    let map = Arc::new(index_map(&index));
    let store_arc = Arc::new(store.clone());
    {
        let (s, m) = (Arc::clone(&store_arc), Arc::clone(&map));
        std::thread::spawn(move || serve_loop(feed, s, m));
    }

    // 5. Fetch the artifact back THROUGH the feed; bytes + sha256 must match the origin.
    let feed_url = format!("http://127.0.0.1:{feed_port}/{path}");
    let got = try_get(&feed_url).unwrap_or_else(|e| die(format!("fetch through feed: {e}")));
    if got != blob {
        die("feed-served bytes differ from the origin artifact".into());
    }
    if hex_sha256(&got) != want {
        die("feed-served sha256 differs from the pin".into());
    }

    // 6. SELF-DISCRIMINATION (warm): a wrong index hash must red `warm`.
    let bad_index = vec![Entry {
        path: "origin.invalid/blob-bad".to_string(),
        url: format!("http://127.0.0.1:{origin_port}/blob"),
        sha256: "0".repeat(64),
    }];
    let bad_store = std::env::temp_dir().join(format!("td-feed-selftest-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bad_store);
    if warm(&bad_index, &bad_store).is_ok() {
        die("warm ACCEPTED a wrong sha256 — verification is not load-bearing".into());
    }
    let _ = std::fs::remove_dir_all(&bad_store);

    // 7. SELF-DISCRIMINATION (serve): corrupt the store byte; verify-on-serve must refuse.
    let mut corrupt = std::fs::read(&stored).expect("read stored");
    corrupt[0] ^= 0xff;
    std::fs::write(&stored, &corrupt).expect("corrupt stored");
    match try_get(&feed_url) {
        Ok(_) => die("feed SERVED a corrupted store artifact — verify-on-serve is not load-bearing".into()),
        Err(_) => {}
    }

    let _ = std::fs::remove_dir_all(&store);
    println!(
        "td-feed: selftest OK — warmed + served + fetched {} bytes (sha256 {}) over loopback \
         (origin 127.0.0.1:{}, feed 127.0.0.1:{}); a wrong index hash reds warm and a corrupted \
         store byte reds serve",
        blob.len(),
        want,
        origin_port,
        feed_port
    );
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("warm") if a.len() == 4 => {
            let (index, store) = (&a[2], PathBuf::from(&a[3]));
            let entries = read_index(index);
            match warm(&entries, &store) {
                Ok((fetched, warm)) => println!(
                    "td-feed: warm OK — {fetched} fetched, {warm} already warm, {} total -> {}",
                    entries.len(),
                    store.display()
                ),
                Err(e) => die(e),
            }
        }
        Some("serve") if a.len() == 5 => {
            let (store, index, addr) = (PathBuf::from(&a[2]), &a[3], &a[4]);
            let entries = read_index(index);
            let listener = TcpListener::bind(addr.as_str())
                .unwrap_or_else(|e| die(format!("bind {addr}: {e}")));
            let bound = listener.local_addr().unwrap_or_else(|e| die(format!("local_addr: {e}")));
            println!(
                "td-feed: serving {} artifacts from {} on http://{}/",
                entries.len(),
                store.display(),
                bound
            );
            let _ = io::stdout().flush();
            serve_loop(listener, Arc::new(store), Arc::new(index_map(&entries)));
        }
        Some("selftest") if a.len() == 2 => selftest(),
        _ => {
            eprintln!(
                "usage:\n  td-feed warm INDEX STORE\n  td-feed serve STORE INDEX ADDR\n  td-feed selftest"
            );
            std::process::exit(2);
        }
    }
}
