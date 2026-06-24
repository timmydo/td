// td-feed — td's OWN local HTTP mirror of the artifacts this repo downloads over the
// network. A sibling of fetch/ (td-fetch) reusing the same pure-Rust HTTP(S)+sha256 stack
// (ureq + rustls/ring + sha2); the mirror server uses only std::net (no extra crate).
//
// It is run as a SHARED, persistent host daemon (tools/feed-ensure.sh) serving a shared
// store across worktrees, so a td-native download happens ONCE. Two verification layers:
//
//   - warm = the SUPPLY-CHAIN gate. `td-feed warm INDEX STORE` (host PREP, egress) GETs
//     each pinned `<path> <url> <sha256>` entry, verifies the bytes against the PINNED
//     index sha256, and writes STORE/<path> PLUS a STORE/<path>.sha256 sidecar (the
//     verified hash). Idempotent: an entry already present + matching is skipped.
//
//   - serve = the INTEGRITY gate, and INDEX-FREE. `td-feed serve STORE ADDR` answers
//     `GET /<path>` by reading STORE/<path> + its .sha256 sidecar, RE-VERIFYING the file
//     against the sidecar (store corruption 500s), and streaming it. Because each artifact
//     is self-describing, a persistent daemon serves whatever any branch has warmed into
//     the shared store with no index coupling. Missing path/sidecar 404/500. No egress.
//
//   td-feed selftest   Self-contained LOOPBACK round-trip (offline): an ORIGIN server on
//                      127.0.0.1, `warm` a one-entry index from it, `serve` the store on a
//                      2nd port, fetch the artifact back THROUGH the feed and verify it.
//                      Also asserts both gates are load-bearing: a wrong pinned hash reds
//                      warm, a corrupted store byte reds serve (sidecar mismatch).
use sha2::{Digest, Sha256};
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
/// crafted index path or request can never escape STORE.
fn store_path(store: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    if rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(store.join(rel))
}

/// The integrity sidecar path for a store file: `<file>.sha256` (append, not replace, so
/// `x.crate` -> `x.crate.sha256`).
fn sidecar_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_os_string();
    s.push(".sha256");
    PathBuf::from(s)
}

/// Write `bytes` to `dst` atomically (pid-unique temp + rename), so a concurrent serve /
/// another warming agent never sees a partial file.
fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut t = dst.as_os_str().to_os_string();
    t.push(format!(".{}.td-feed-tmp", std::process::id()));
    let tmp = PathBuf::from(t);
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("rename {}: {e}", dst.display()))
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

/// Warm one entry into `store` (+ its sidecar); Ok(true) if fetched, Ok(false) if already
/// warm + verified. Never egresses for an entry already present + matching.
fn warm_one(e: &Entry, store: &Path) -> Result<bool, String> {
    let dst = store_path(store, &e.path).ok_or_else(|| format!("unsafe index path {:?}", e.path))?;
    let side = sidecar_path(&dst);
    if let Ok(have) = std::fs::read(&dst) {
        if hex_sha256(&have) == e.sha256 {
            // File is warm; make sure the integrity sidecar is present + correct.
            let ok = std::fs::read_to_string(&side)
                .map(|s| s.trim() == e.sha256)
                .unwrap_or(false);
            if !ok {
                write_atomic(&side, format!("{}\n", e.sha256).as_bytes())?;
            }
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
    write_atomic(&dst, &body)?;
    write_atomic(&side, format!("{got}\n").as_bytes())?;
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

/// Handle one request: route `GET /<path>`, verify the file against its sidecar, stream.
fn handle_conn(mut conn: TcpStream, store: &Path) -> io::Result<()> {
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
    // The integrity sidecars are internal — never serve them.
    if path.ends_with(".sha256") {
        return respond(&mut conn, 404, "Not Found", b"not served\n");
    }
    let full = match store_path(store, path) {
        Some(p) => p,
        None => return respond(&mut conn, 400, "Bad Request", b"bad path\n"),
    };
    let bytes = match std::fs::read(&full) {
        Ok(b) => b,
        Err(_) => return respond(&mut conn, 404, "Not Found", b"not warmed\n"),
    };
    let want = match std::fs::read_to_string(sidecar_path(&full)) {
        Ok(s) => s.trim().to_string(),
        // No sidecar ⇒ the artifact was not placed by `warm`; refuse to serve unverified.
        Err(_) => return respond(&mut conn, 500, "No Integrity Sidecar", b"no sidecar\n"),
    };
    if hex_sha256(&bytes) != want {
        // verify-on-serve: the store drifted from the warmed hash — refuse to serve.
        return respond(&mut conn, 500, "Integrity Failure", b"store sha256 mismatch\n");
    }
    respond(&mut conn, 200, "OK", &bytes)
}

/// Run the mirror server forever on `listener` (one thread per connection).
fn serve_loop(listener: TcpListener, store: Arc<PathBuf>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            let _ = handle_conn(conn, &store);
        });
    }
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

    // 3. Warm into a fresh temp store (writes the file + its sidecar).
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
    if !sidecar_path(&stored).exists() {
        die("warm did not write the integrity sidecar".into());
    }

    // 4. Serve the store on a 2nd loopback port (index-free — sidecars carry the hashes).
    let feed = TcpListener::bind("127.0.0.1:0").expect("bind feed");
    let feed_port = feed.local_addr().expect("addr").port();
    {
        let s = Arc::new(store.clone());
        std::thread::spawn(move || serve_loop(feed, s));
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

    // 6. SELF-DISCRIMINATION (warm): a wrong pinned hash must red `warm`.
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
    if try_get(&feed_url).is_ok() {
        die("feed SERVED a corrupted store artifact — verify-on-serve is not load-bearing".into());
    }

    let _ = std::fs::remove_dir_all(&store);
    println!(
        "td-feed: selftest OK — warmed + served + fetched {} bytes (sha256 {}) over loopback \
         (origin 127.0.0.1:{}, feed 127.0.0.1:{}); a wrong pinned hash reds warm and a corrupted \
         store byte reds serve (sidecar integrity)",
        blob.len(),
        want,
        origin_port,
        feed_port
    );
}

// ---------------------------------------------------------------------------------------
// cargo-proxy: a cargo SPARSE registry mirror. `cargo fetch`/`cargo build` fetch their WHOLE
// crate closure THROUGH td (fetch-then-save + verify), so cargo does the dependency
// resolution + fetching and td owns the verifying, caching, shareable egress — the generic,
// guix-free crate provisioning. Point cargo at `sparse+http://<addr>/` (source replacement).
// Three request kinds (cargo's sparse protocol):
//   GET /config.json                    -> {"dl":"http://<addr>/dl","api":"http://<addr>"}
//   GET /<idx-path>                     -> proxy+cache index.crates.io/<idx-path> (newline
//                                          JSON version metadata incl. each cksum)
//   GET /dl/<crate>/<version>/download  -> fetch static.crates.io, VERIFY sha256 == the index
//                                          cksum, cache, serve (the .crate tarball)
// Cache under STORE: index/<idx-path>, crates/<crate>-<version>.crate (the vendor set,
// shareable). (cargo `vendor` bypasses source replacement; cargo `fetch` honors it.)

/// Upstream bases (env-overridable for the hermetic selftest): the sparse index + `.crate` CDN.
fn index_base() -> String {
    std::env::var("TD_INDEX_BASE").unwrap_or_else(|_| "https://index.crates.io".into())
}
fn crates_base() -> String {
    std::env::var("TD_CRATES_BASE").unwrap_or_else(|_| "https://static.crates.io".into())
}

/// The sparse-registry index path for a crate name (lowercased): `1/{n}`, `2/{n}`,
/// `3/{c}/{n}`, else `{n[0:2]}/{n[2:4]}/{n}`.
fn index_path(name: &str) -> String {
    let n = name.to_lowercase();
    match n.len() {
        0 => n,
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[0..1]),
        _ => format!("{}/{}/{n}", &n[0..2], &n[2..4]),
    }
}

/// Extract the sha256 `cksum` for `version` from a sparse-index document (newline JSON).
fn cksum_for(index_text: &str, version: &str) -> Option<String> {
    let needle = format!("\"vers\":\"{version}\"");
    for line in index_text.lines() {
        if line.contains(&needle) {
            let k = "\"cksum\":\"";
            let i = line.find(k)? + k.len();
            let j = line[i..].find('"')?;
            return Some(line[i..i + j].to_string());
        }
    }
    None
}

/// Serve (cache-or-proxy) a sparse-index document from index.crates.io.
fn serve_index(store: &Path, idxpath: &str) -> Result<Vec<u8>, String> {
    let cache = store_path(&store.join("index"), idxpath).ok_or("unsafe index path")?;
    if let Ok(b) = std::fs::read(&cache) {
        return Ok(b);
    }
    let body = try_get(&format!("{}/{idxpath}", index_base()))?;
    if let Some(p) = cache.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    }
    write_atomic(&cache, &body)?;
    Ok(body)
}

/// Serve (cache-or-fetch+verify) a `.crate` from static.crates.io, verified against the
/// index cksum — the td-owned, verifying egress.
fn serve_crate(store: &Path, cr: &str, ver: &str) -> Result<Vec<u8>, String> {
    let cache = store_path(&store.join("crates"), &format!("{cr}-{ver}.crate"))
        .ok_or("unsafe crate name")?;
    if let Ok(b) = std::fs::read(&cache) {
        return Ok(b);
    }
    let idx = serve_index(store, &index_path(cr))?;
    let cksum = cksum_for(&String::from_utf8_lossy(&idx), ver)
        .ok_or_else(|| format!("no cksum for {cr} {ver} in the index"))?;
    let url = format!("{}/crates/{cr}/{cr}-{ver}.crate", crates_base());
    let body = try_get(&url)?;
    if hex_sha256(&body) != cksum {
        return Err(format!("sha256 mismatch for {cr} {ver}: index cksum {cksum}"));
    }
    if let Some(p) = cache.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
    }
    write_atomic(&cache, &body)?;
    Ok(body)
}

/// Route one cargo sparse-registry request. `base` is HOST:PORT for the config URLs.
fn cargo_route(store: &Path, base: &str, path: &str) -> Result<Vec<u8>, String> {
    if path == "/config.json" {
        return Ok(format!("{{\"dl\":\"http://{base}/dl\",\"api\":\"http://{base}\"}}").into_bytes());
    }
    // The download endpoint is `/dl/<crate>/<version>/download`. A crate whose name starts with
    // "dl" has a sparse-index path that ALSO starts with `dl/` (e.g. `dlv-list` -> `dl/v-/dlv-list`),
    // so `/dl/` alone is ambiguous. It is only a download when the shape matches exactly (3 parts,
    // last == "download"); no index path can be `/dl/XX/download` (that needs a crate named
    // "download", whose index path is `do/wn/download`, not `dl/...`). Anything else under `/dl/`
    // is such an index path — fall through to serve_index, don't 404 it.
    if let Some(rest) = path.strip_prefix("/dl/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 && parts[2] == "download" && !parts[0].is_empty() && !parts[1].is_empty() {
            return serve_crate(store, parts[0], parts[1]);
        }
    }
    serve_index(store, path.trim_start_matches('/'))
}

/// Handle one cargo request: parse `GET /<path>`, route, stream.
fn handle_cargo_conn(mut conn: TcpStream, store: &Path, base: &str) -> io::Result<()> {
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
    match cargo_route(store, base, target) {
        Ok(bytes) => respond(&mut conn, 200, "OK", &bytes),
        Err(e) => {
            eprintln!("td-feed cargo-proxy: {target}: {e}");
            let code = if e.starts_with("no cksum") || e.starts_with("bad download") {
                404
            } else {
                502
            };
            respond(&mut conn, code, "Error", e.as_bytes())
        }
    }
}

/// Run the cargo-proxy forever on `listener` (one thread per connection).
fn cargo_proxy_loop(listener: TcpListener, store: Arc<PathBuf>, base: String) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (store, base) = (Arc::clone(&store), base.clone());
        std::thread::spawn(move || {
            let _ = handle_cargo_conn(conn, &store, &base);
        });
    }
}

/// Hermetic loopback selftest of the cargo-proxy: a mock index/static.crates.io on 127.0.0.1
/// (TD_INDEX_BASE/TD_CRATES_BASE), the proxy fetches a `.crate` THROUGH it and verifies it
/// against the index cksum; a crate whose bytes mismatch its index cksum is refused (the
/// verifying egress is load-bearing). Offline (std::net only).
fn cargo_proxy_selftest() {
    let cbytes: Vec<u8> = (0u16..2048).map(|x| (x % 251) as u8).collect();
    let cksum = hex_sha256(&cbytes);
    let badbytes = b"corrupt-upstream-bytes".to_vec();
    let badcksum = "0".repeat(64); // the index claims this; the served bytes won't match

    let up = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let uport = up.local_addr().unwrap().port();
    let (cb, bb, ck) = (cbytes.clone(), badbytes.clone(), cksum.clone());
    std::thread::spawn(move || loop {
        match up.accept() {
            Ok((mut c, _)) => {
                let mut buf = [0u8; 1024];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("");
                let body: Vec<u8> = match path {
                    "/un/ar/unarray" => format!(
                        "{{\"name\":\"unarray\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{ck}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    "/3/b/bad" => format!(
                        "{{\"name\":\"bad\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{badcksum}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    // a crate whose name starts with "dl": its sparse-index path is `dl/te/dltest`,
                    // which collides with the `/dl/` download prefix (the dlv-list bug regression).
                    "/dl/te/dltest" => format!(
                        "{{\"name\":\"dltest\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{ck}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    "/crates/unarray/unarray-0.1.0.crate" => cb.clone(),
                    "/crates/bad/bad-1.0.0.crate" => bb.clone(),
                    _ => Vec::new(),
                };
                // Respond directly: the request was already read above; serve_once would
                // re-read and block, since the client is now awaiting the response.
                let _ = respond(&mut c, 200, "OK", &body);
            }
            Err(_) => break,
        }
    });
    let ubase = format!("http://127.0.0.1:{uport}");
    std::env::set_var("TD_INDEX_BASE", &ubase);
    std::env::set_var("TD_CRATES_BASE", &ubase);

    let store = std::env::temp_dir().join(format!("td-cargo-proxy-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    std::fs::create_dir_all(&store).expect("mkdir store");
    let plist = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let pport = plist.local_addr().unwrap().port();
    {
        let (s, b) = (Arc::new(store.clone()), format!("127.0.0.1:{pport}"));
        std::thread::spawn(move || cargo_proxy_loop(plist, s, b));
    }
    let proxy = format!("http://127.0.0.1:{pport}");

    let cfg = try_get(&format!("{proxy}/config.json")).unwrap_or_else(|e| die(format!("config.json: {e}")));
    if !String::from_utf8_lossy(&cfg).contains("\"dl\"") {
        die("config.json missing dl".into());
    }
    let got = try_get(&format!("{proxy}/dl/unarray/0.1.0/download"))
        .unwrap_or_else(|e| die(format!("dl unarray: {e}")));
    if got != cbytes || hex_sha256(&got) != cksum {
        die("proxy-served crate differs from upstream / its index cksum".into());
    }
    if !store.join("crates/unarray-0.1.0.crate").exists() {
        die("proxy did not cache the fetched crate".into());
    }
    if try_get(&format!("{proxy}/dl/bad/1.0.0/download")).is_ok() {
        die("proxy SERVED a crate whose bytes mismatch the index cksum — verify-on-fetch is not load-bearing".into());
    }
    // Regression (the dlv-list bug): a crate whose name starts with "dl" has a sparse-index path
    // starting with `dl/` (dltest -> /dl/te/dltest). The proxy must serve it as an INDEX, not
    // mis-route it to the `/dl/<crate>/<version>/download` handler and 404 it.
    let idx = try_get(&format!("{proxy}/dl/te/dltest"))
        .unwrap_or_else(|e| die(format!("dl-prefixed sparse-index path failed (the dlv-list collision): {e}")));
    if !String::from_utf8_lossy(&idx).contains("\"name\":\"dltest\"") {
        die("proxy did not serve the dl-prefixed path as a sparse index (download/index route collision)".into());
    }
    let _ = std::fs::remove_dir_all(&store);
    println!(
        "td-feed: cargo-proxy selftest OK — fetched + verified a crate through the proxy (upstream \
         127.0.0.1:{uport}, proxy 127.0.0.1:{pport}, cached); a crate whose bytes mismatch its index \
         cksum is refused"
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
        Some("serve") if a.len() == 4 => {
            let (store, addr) = (PathBuf::from(&a[2]), &a[3]);
            let listener =
                TcpListener::bind(addr.as_str()).unwrap_or_else(|e| die(format!("bind {addr}: {e}")));
            let bound = listener.local_addr().unwrap_or_else(|e| die(format!("local_addr: {e}")));
            println!("td-feed: serving {} on http://{}/", store.display(), bound);
            let _ = io::stdout().flush();
            serve_loop(listener, Arc::new(store));
        }
        Some("cargo-proxy") if a.len() == 4 => {
            let (store, addr) = (PathBuf::from(&a[2]), &a[3]);
            let listener =
                TcpListener::bind(addr.as_str()).unwrap_or_else(|e| die(format!("bind {addr}: {e}")));
            let bound = listener.local_addr().unwrap_or_else(|e| die(format!("local_addr: {e}")));
            println!("td-feed: cargo-proxy on http://{bound}/ (store {})", store.display());
            let _ = io::stdout().flush();
            cargo_proxy_loop(listener, Arc::new(store), bound.to_string());
        }
        Some("selftest") if a.len() == 2 => selftest(),
        Some("cargo-proxy-selftest") if a.len() == 2 => cargo_proxy_selftest(),
        _ => {
            eprintln!(
                "usage:\n  td-feed warm INDEX STORE\n  td-feed serve STORE ADDR\n  td-feed cargo-proxy STORE ADDR\n  td-feed selftest\n  td-feed cargo-proxy-selftest"
            );
            std::process::exit(2);
        }
    }
}
