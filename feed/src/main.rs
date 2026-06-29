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
use std::process::{Command, Stdio};
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
/// index cksum — the td-owned, verifying egress. The cache is NOT trusted blindly: a cache
/// hit is re-verified against the index cksum on every serve, and a corrupted/stale entry
/// is discarded and refetched. So the sha256==index-cksum guarantee holds for cached hits,
/// not just fresh downloads (the integrity tools/warm-cargo-proxy.sh relies on).
fn serve_crate(store: &Path, cr: &str, ver: &str) -> Result<Vec<u8>, String> {
    let cache = store_path(&store.join("crates"), &format!("{cr}-{ver}.crate"))
        .ok_or("unsafe crate name")?;
    let idx = serve_index(store, &index_path(cr))?;
    let cksum = cksum_for(&String::from_utf8_lossy(&idx), ver)
        .ok_or_else(|| format!("no cksum for {cr} {ver} in the index"))?;
    if let Ok(b) = std::fs::read(&cache) {
        if hex_sha256(&b) == cksum {
            return Ok(b);
        }
        // Corrupted/stale cache entry — drop it and refetch rather than serve bad bytes.
        let _ = std::fs::remove_file(&cache);
    }
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
    // Cache-integrity: a CACHE HIT is re-verified against the index cksum, not trusted
    // blindly. Corrupt the cached crate, then fetch again — the proxy must reject the bad
    // cached bytes, refetch from upstream, serve the correct bytes, and heal the cache.
    std::fs::write(store.join("crates/unarray-0.1.0.crate"), b"corrupted-cache-bytes")
        .unwrap_or_else(|e| die(format!("corrupt cache: {e}")));
    let healed = try_get(&format!("{proxy}/dl/unarray/0.1.0/download"))
        .unwrap_or_else(|e| die(format!("dl unarray after cache corruption: {e}")));
    if healed != cbytes {
        die("proxy SERVED a corrupted cache entry — a cache hit is trusted without re-verifying its index cksum".into());
    }
    if std::fs::read(store.join("crates/unarray-0.1.0.crate")).unwrap_or_default() != cbytes {
        die("proxy did not heal the corrupted cache entry after refetch".into());
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
         cksum is refused; a corrupted cache hit is re-verified, refetched, and healed"
    );
}

// =======================================================================================
// warm <action> — the STRUCTURED host-PREP orchestration that consolidates the former
// tools/warm-{cargo-proxy,cargo-proxy-local,bootstrap-sources,kernel-headers{,-x86_64}}.sh
// shell scripts into one typed, in-process subcommand (move-off-shell). These run on the
// HOST during check.sh's network-permitted prelude (the offline loop has no egress) and are
// BEST-EFFORT by design: a runner without cargo/make/network warns to stderr and skips
// (exit 0) — the heavy `rust-*` / `bootstrap-*` gates that CONSUME the warmed outputs fail
// loudly if they actually run cold. The crown-jewel win over the shell: the cargo-proxy is
// bound IN-PROCESS, so we know its loopback address immediately — no background process, no
// log-file scrape, no `sed` parse, no sleep-poll.
//
// Paths resolve relative to the repo root (the prelude's CWD); TD_ROOT overrides it.

/// The repo root: $TD_ROOT, else the current directory.
fn repo_root() -> PathBuf {
    std::env::var_os("TD_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Is `cmd` an executable on PATH? (best-effort `command -v` equivalent).
fn have_cmd(cmd: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(path) = std::env::var("PATH") else { return false };
    path.split(':').filter(|d| !d.is_empty()).any(|dir| {
        let p = Path::new(dir).join(cmd);
        std::fs::metadata(&p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Count `*.crate` files directly under `dir`.
fn count_crates(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "crate"))
                .count()
        })
        .unwrap_or(0)
}

/// Copy every `*.crate` from `from` into `to`; returns the count copied.
fn copy_crates(from: &Path, to: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(from) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "crate") {
                if let Some(name) = p.file_name() {
                    if std::fs::copy(&p, to.join(name)).is_ok() {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

/// Run `cmd` with stdio discarded; true on a zero exit (best-effort, never panics).
fn run_quiet(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start the cargo-proxy on an OS-picked loopback port IN THIS PROCESS; returns its
/// `HOST:PORT`. A background thread runs the serve loop over `store`. Because we hold the
/// bound listener, the address is known immediately — no subprocess + log scrape + poll
/// (the fragile shell this replaces). The connect from cargo/try_get succeeds against the
/// already-bound listener's backlog, so no readiness wait is needed.
fn start_cargo_proxy(store: &Path) -> Result<String, String> {
    std::fs::create_dir_all(store).map_err(|e| format!("mkdir {}: {e}", store.display()))?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind cargo-proxy: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("cargo-proxy local_addr: {e}"))?
        .to_string();
    let s = Arc::new(store.to_path_buf());
    let base = addr.clone();
    std::thread::spawn(move || cargo_proxy_loop(listener, s, base));
    Ok(addr)
}

/// The cargo `config.toml` body that routes crates.io through the proxy at `addr` (sparse
/// source replacement). GOTCHA (#163): `cargo vendor` IGNORES source replacement; `cargo
/// fetch`/`build` HONOR it — so the warm uses `cargo fetch`.
fn cargo_config(addr: &str) -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"td-proxy\"\n[source.td-proxy]\nregistry = \"sparse+http://{addr}/\"\n"
    )
}

/// Write a FRESH CARGO_HOME at `dir` routed at the proxy. Fresh ⇒ every crate is a proxy
/// miss (verified td egress), none served from a prior cargo cache, so the vendored closure
/// stays complete + pinned.
fn write_cargo_home(dir: &Path, addr: &str) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    std::fs::write(dir.join("config.toml"), cargo_config(addr))
        .map_err(|e| format!("write cargo config: {e}"))
}

/// `cargo fetch --locked` in `srcdir` with CARGO_HOME=`ch` (so it routes through the proxy).
fn cargo_fetch_locked(srcdir: &Path, ch: &Path) -> bool {
    run_quiet(
        Command::new("cargo")
            .arg("fetch")
            .arg("--locked")
            .current_dir(srcdir)
            .env("CARGO_HOME", ch),
    )
}

/// warm crate CRATE VERSION [DEST] — provision a crates.io package's SOURCE tree + its FULL
/// locked dep closure THROUGH the in-process cargo-proxy (the proxy verifies each `.crate`
/// sha256 == the crates.io sparse-index cksum, the UPSTREAM pin — no guix). Leaves, for the
/// offline gate to intern + build via TD_VENDOR_DIR:
///   .td-build-cache/crate-vendor/<dest>/src/<crate>-<ver>/  the extracted source tree
///   .td-build-cache/crate-vendor/<dest>/vendor/*.crate      the locked dep closure
fn warm_crate(root: &Path, krate: &str, ver: &str, dest: &str) {
    let cv = root.join(".td-build-cache/crate-vendor").join(dest);
    let srcparent = cv.join("src");
    let srcdir = srcparent.join(format!("{krate}-{ver}"));
    let vendor = cv.join("vendor");

    if srcdir.join("Cargo.toml").is_file() && count_crates(&vendor) >= 1 {
        eprintln!(
            "td-feed warm crate: {krate}-{ver} already warm ({} crates) in {}",
            count_crates(&vendor),
            cv.display()
        );
        return;
    }
    if !have_cmd("cargo") {
        eprintln!("td-feed warm crate: no cargo — skipping {krate}-{ver} (PREP best-effort)");
        return;
    }
    if !have_cmd("tar") {
        eprintln!("td-feed warm crate: no tar — skipping {krate}-{ver}");
        return;
    }

    let work = cv.join("work");
    let _ = std::fs::remove_dir_all(&work);
    let proxy_store = work.join("proxy-store");
    let addr = match start_cargo_proxy(&proxy_store) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("td-feed warm crate: {e} — skipping {krate}-{ver}");
            return;
        }
    };

    // 1) Grab the SOURCE crate through the proxy's VERIFYING /dl endpoint (a plain GET — the
    //    proxy fetches static.crates.io, verifies sha256 == the index cksum, caches, serves).
    //    NOT a throwaway `cargo fetch`: a fresh resolve can FAIL where the shipped Cargo.lock's
    //    exact pins resolve (e.g. coreutils 0.9.0: a fresh ordered-multimap picks a dlv-list
    //    that isn't there). The /dl GET sidesteps resolution; deps come later from the source's
    //    OWN lock (step 3).
    let dlurl = format!("http://{addr}/dl/{krate}/{ver}/download");
    let body = match try_get(&dlurl) {
        Ok(b) if !b.is_empty() => b,
        _ => {
            eprintln!("td-feed warm crate: source fetch failed for {krate}-{ver} (GET {dlurl})");
            return;
        }
    };
    let srccrate = work.join(format!("{krate}-{ver}.crate"));
    if std::fs::create_dir_all(&work).is_err() || std::fs::write(&srccrate, &body).is_err() {
        eprintln!("td-feed warm crate: could not stage the source crate for {krate}-{ver}");
        return;
    }

    // 2) Extract the source crate -> the source tree.
    let _ = std::fs::remove_dir_all(&srcparent);
    if std::fs::create_dir_all(&srcparent).is_err()
        || !run_quiet(Command::new("tar").arg("-xzf").arg(&srccrate).arg("-C").arg(&srcparent))
    {
        eprintln!("td-feed warm crate: could not extract the source crate for {krate}-{ver}");
        return;
    }
    if !srcdir.join("Cargo.toml").is_file() {
        eprintln!("td-feed warm crate: extracted source has no Cargo.toml at {}", srcdir.display());
        return;
    }
    if !srcdir.join("Cargo.lock").is_file() {
        eprintln!("td-feed warm crate: source {krate}-{ver} ships no Cargo.lock — cannot pin the closure");
        return;
    }

    // 3) Fetch the FULL locked closure through the proxy from the source's OWN Cargo.lock, with
    //    a CLEAN proxy cache + a FRESH cargo home (every crate a verified proxy miss).
    let _ = std::fs::remove_dir_all(proxy_store.join("crates"));
    let _ = std::fs::remove_dir_all(proxy_store.join("index"));
    let ch = work.join("ch-deps");
    if write_cargo_home(&ch, &addr).is_err() || !cargo_fetch_locked(&srcdir, &ch) {
        eprintln!("td-feed warm crate: locked dep fetch failed for {krate}-{ver}");
        return;
    }

    // 4) Publish the vendor set (the proxy's verified crate cache) + drop cargo build state.
    let _ = std::fs::remove_dir_all(&vendor);
    if std::fs::create_dir_all(&vendor).is_err() {
        eprintln!("td-feed warm crate: could not create vendor dir for {krate}-{ver}");
        return;
    }
    let n = copy_crates(&proxy_store.join("crates"), &vendor);
    let _ = std::fs::remove_dir_all(srcdir.join("target"));
    if n < 1 {
        eprintln!("td-feed warm crate: no crates vendored for {krate}-{ver}");
        return;
    }
    eprintln!(
        "td-feed warm crate: {krate}-{ver} — source + {n} crates provisioned guix-free \
         (cargo-proxy, Cargo.lock-pinned, sha==index cksum) in {}",
        cv.display()
    );
}

/// warm crate-local SRCDIR DEST — provision a LOCAL (in-tree) crate's dep closure THROUGH
/// the in-process cargo-proxy. No source crate to fetch (the source IS the in-tree dir, which
/// the gate interns itself); only the locked dep closure -> .td-build-cache/crate-vendor/<dest>/*.crate.
fn warm_crate_local(root: &Path, srcrel: &str, dest: &str) {
    let srcdir = match std::fs::canonicalize(root.join(srcrel)) {
        Ok(p) if p.join("Cargo.lock").is_file() => p,
        _ => {
            eprintln!("td-feed warm crate-local: {dest} has no Cargo.lock at the source dir — cannot pin the closure");
            return;
        }
    };
    let vendor = root.join(".td-build-cache/crate-vendor").join(dest);
    if count_crates(&vendor) >= 1 {
        eprintln!(
            "td-feed warm crate-local: {dest} already warm ({} crates) in {}",
            count_crates(&vendor),
            vendor.display()
        );
        return;
    }
    if !have_cmd("cargo") {
        eprintln!("td-feed warm crate-local: no cargo — skipping {dest} (PREP best-effort)");
        return;
    }

    let work = root.join(".td-build-cache/crate-vendor").join(format!("{dest}.work"));
    let _ = std::fs::remove_dir_all(&work);
    let proxy_store = work.join("proxy-store");
    let addr = match start_cargo_proxy(&proxy_store) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("td-feed warm crate-local: {e} — skipping {dest}");
            return;
        }
    };
    let ch = work.join("cargo-home");
    if write_cargo_home(&ch, &addr).is_err() || !cargo_fetch_locked(&srcdir, &ch) {
        eprintln!("td-feed warm crate-local: locked dep fetch failed for {dest} (in {})", srcdir.display());
        return;
    }
    let _ = std::fs::remove_dir_all(&vendor);
    if std::fs::create_dir_all(&vendor).is_err() {
        eprintln!("td-feed warm crate-local: could not create vendor dir for {dest}");
        return;
    }
    let n = copy_crates(&proxy_store.join("crates"), &vendor);
    let _ = std::fs::remove_dir_all(&work);
    if n < 1 {
        eprintln!("td-feed warm crate-local: no crates vendored for {dest}");
        return;
    }
    eprintln!(
        "td-feed warm crate-local: {dest} — {n} crates provisioned guix-free \
         (cargo-proxy, {}/Cargo.lock-pinned, sha==index cksum) in {}",
        srcdir.display(),
        vendor.display()
    );
}

/// Parse a `seed/sources/*.lock`: the first `url`/`sha256`/`file` line of each kind.
fn parse_source_lock(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut url, mut sha, mut file) = (None, None, None);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("url ") {
            url.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("sha256 ") {
            sha.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("file ") {
            file.get_or_insert_with(|| v.trim().to_string());
        }
    }
    (url, sha, file)
}

/// `https://h/p` / `http://h/p` -> `h/p` (the feed serves a URL-path mirror at `GET /<h>/<p>`).
fn strip_scheme(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .to_string()
}

/// The shared feed `(addr, store)` if TD_FEED_BASE is set (the bootstrap-sources shim runs
/// feed-ensure.sh — daemon lifecycle stays in shell — and exports it). Else None (direct).
fn shared_feed() -> Option<(String, PathBuf)> {
    let base = std::env::var("TD_FEED_BASE").ok().filter(|s| !s.is_empty())?;
    let addr = strip_scheme(&base);
    let feed_dir = std::env::var("TD_FEED_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".td/feed")
        });
    Some((addr, feed_dir.join("store")))
}

/// warm sources — fetch the pinned source-bootstrap tarballs (seed/sources/*.lock) into
/// .td-build-cache/sources/ for the offline heavy `bootstrap-*` gates, then produce the i386
/// + x86_64 Linux UAPI headers. Prefers the shared feed (TD_FEED_BASE), else a direct GET.
/// td OWNS the fetch (no guix-as-fetcher); each tarball is verified against its lock sha256.
fn warm_sources(root: &Path) {
    let srcdir = root.join("seed/sources");
    let dest = root.join(".td-build-cache/sources");
    let mut locks: Vec<PathBuf> = match std::fs::read_dir(&srcdir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "lock"))
            .collect(),
        Err(_) => return, // no sources dir -> nothing to warm
    };
    if locks.is_empty() {
        return;
    }
    locks.sort();
    if std::fs::create_dir_all(&dest).is_err() {
        eprintln!(">> td-feed warm sources: could not create {}", dest.display());
        return;
    }

    let feed = shared_feed();
    if let Some((addr, store)) = &feed {
        eprintln!(">> td-feed warm sources: using the shared feed at http://{addr} (store {})", store.display());
    }

    for lock in &locks {
        let text = std::fs::read_to_string(lock).unwrap_or_default();
        let (url, sha, file) = match parse_source_lock(&text) {
            (Some(u), Some(s), Some(f)) => (u, s, f),
            _ => {
                eprintln!(">> td-feed warm sources: {} malformed (need url/sha256/file) — skipping", lock.display());
                continue;
            }
        };
        let out = dest.join(&file);
        if let Ok(b) = std::fs::read(&out) {
            if hex_sha256(&b) == sha {
                continue; // already warm + verified
            }
        }

        let mut via: Option<String> = None;
        // Preferred: through the SHARED feed. Populate it (warm_one egresses only if the
        // shared store is cold — another worktree may already hold it), then GET it back from
        // the feed (offline once warm). So the egress happens ONCE across all worktrees.
        if let Some((addr, store)) = &feed {
            let path = strip_scheme(&url);
            let e = Entry { path: path.clone(), url: url.clone(), sha256: sha.clone() };
            let _ = warm_one(&e, store);
            if let Ok(b) = try_get(&format!("http://{addr}/{path}")) {
                if hex_sha256(&b) == sha && write_atomic(&out, &b).is_ok() {
                    via = Some(format!("the shared feed (http://{addr})"));
                }
            }
        }
        // Fallback: a direct GET (feed unavailable, or a cold-feed miss).
        if via.is_none() {
            if let Ok(b) = try_get(&url) {
                if hex_sha256(&b) == sha && write_atomic(&out, &b).is_ok() {
                    via = Some("a direct fetch".to_string());
                }
            }
        }
        match via {
            Some(v) => eprintln!(">> td-feed warm sources: warmed {} via {v} (sha256 verified)", out.display()),
            None => eprintln!(
                ">> td-feed warm sources: could not warm {file} (feed + direct both failed) — skipping (the bootstrap gate will report if it runs)"
            ),
        }
    }

    // Derived inputs: the sanitized Linux UAPI headers for the glibc rungs, produced FROM the
    // pinned linux source (the sandbox can't run the kernel build). Both lanes, best-effort.
    warm_kernel_headers(root, "i386");
    warm_kernel_headers(root, "x86_64");
}

/// `LINUX_VERSION_CODE` for a `maj.min.sub` version (e.g. 4.14.67 -> 265795).
fn linux_version_code(ver: &str) -> u64 {
    let mut it = ver.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let maj = it.next().unwrap_or(0);
    let min = it.next().unwrap_or(0);
    let sub = it.next().unwrap_or(0);
    maj * 65536 + min * 256 + sub
}

/// The hand-written `linux/version.h` body (`headers_install` does NOT emit it, but glibc's
/// configure checks LINUX_VERSION_CODE >= 2.0.10, else "kernel header files TOO OLD!").
fn version_h(code: u64) -> String {
    format!("#define LINUX_VERSION_CODE {code}\n#define KERNEL_VERSION(a,b,c) (((a) << 16) + ((b) << 8) + (c))\n")
}

/// `linux-<ver>.tar.<ext>` -> `<ver>`.
fn linux_ver_from_file(file: &str) -> Option<String> {
    let s = file.strip_prefix("linux-")?;
    let i = s.find(".tar.")?;
    Some(s[..i].to_string())
}

/// `xz -dc src | tar -xf - -C dest --strip-components=1` (don't rely on tar's xz support).
fn extract_xz_tar(src: &Path, dest: &Path) -> bool {
    let mut xz = match Command::new("xz")
        .arg("-dc")
        .arg(src)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(xzout) = xz.stdout.take() else { return false };
    let tar_ok = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(dest)
        .arg("--strip-components=1")
        .stdin(Stdio::from(xzout))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let xz_ok = xz.wait().map(|s| s.success()).unwrap_or(false);
    tar_ok && xz_ok
}

/// warm kernel-headers ARCH — produce the sanitized Linux UAPI headers for `ARCH` (i386 /
/// x86_64) FROM the pinned linux source via `make headers_install`, into
/// .td-build-cache/sources/linux-headers-<ver>-<ARCH>.tar.gz (+ a hand-written version.h).
/// guix ships a prebuilt header BLOB; td produces the same headers FROM canonical source.
fn warm_kernel_headers(root: &Path, arch: &str) {
    let srcdir = root.join("seed/sources");
    let mut linux_locks: Vec<PathBuf> = match std::fs::read_dir(&srcdir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("linux-") && n.ends_with(".lock"))
            })
            .collect(),
        Err(_) => return,
    };
    if linux_locks.is_empty() {
        return; // no linux lock -> nothing to do
    }
    linux_locks.sort();
    let text = std::fs::read_to_string(&linux_locks[0]).unwrap_or_default();
    let Some(file) = parse_source_lock(&text).2 else { return };
    let Some(ver) = linux_ver_from_file(&file) else {
        eprintln!(">> td-feed warm kernel-headers ({arch}): cannot parse version from {file} — skipping");
        return;
    };
    let cache = root.join(".td-build-cache/sources");
    let src = cache.join(&file);
    let out = cache.join(format!("linux-headers-{ver}-{arch}.tar.gz"));
    if out.exists() {
        return; // already produced
    }
    if !src.is_file() {
        eprintln!(">> td-feed warm kernel-headers ({arch}): linux source not warm ({}) — skipping (PREP best-effort)", src.display());
        return;
    }
    if !(have_cmd("make") && have_cmd("gcc") && have_cmd("xz")) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): need host make+gcc+xz to produce headers — skipping (best-effort)");
        return;
    }

    let work = std::env::temp_dir().join(format!("td-feed-kh-{arch}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    if std::fs::create_dir_all(&work).is_err() {
        return;
    }
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&work);
    };
    if !extract_xz_tar(&src, &work) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): could not extract {file} — skipping");
        cleanup();
        return;
    }
    let hdr = work.join("hdr");
    if !run_quiet(
        Command::new("make")
            .current_dir(&work)
            .arg(format!("ARCH={arch}"))
            .arg(format!("INSTALL_HDR_PATH={}", hdr.display()))
            .arg("headers_install"),
    ) {
        eprintln!(">> td-feed warm kernel-headers ({arch}): headers_install failed — skipping");
        cleanup();
        return;
    }
    let code = linux_version_code(&ver);
    let vdir = hdr.join("include/linux");
    let _ = std::fs::create_dir_all(&vdir);
    if std::fs::write(vdir.join("version.h"), version_h(code)).is_err() {
        cleanup();
        return;
    }
    let tmp = cache.join(format!("linux-headers-{ver}-{arch}.tar.gz.tmp"));
    let ok = run_quiet(
        Command::new("tar")
            .arg("-czf")
            .arg(&tmp)
            .arg("-C")
            .arg(hdr.join("include"))
            .arg("."),
    );
    if ok && std::fs::rename(&tmp, &out).is_ok() {
        eprintln!(">> td-feed warm kernel-headers ({arch}): produced {} (LINUX_VERSION_CODE={code}) from the pinned {file}", out.display());
    } else {
        let _ = std::fs::remove_file(&tmp);
        eprintln!(">> td-feed warm kernel-headers ({arch}): could not pack the headers tarball — skipping");
    }
    cleanup();
}

/// Hermetic OFFLINE selftest of the warm orchestration's pure + in-process legs (the parts
/// that need NO cargo/make/network). The cargo/make/network legs stay best-effort host PREP,
/// proven by the consuming heavy gates (as the shell scripts were). std::net loopback only.
fn warm_selftest() {
    // 1) parse_source_lock: a well-formed lock parses; a malformed one yields no fields.
    let lk = "url https://ftp.gnu.org/x.tar.xz\nsha256 deadbeef\nfile x.tar.xz\n";
    let (u, s, f) = parse_source_lock(lk);
    if u.as_deref() != Some("https://ftp.gnu.org/x.tar.xz")
        || s.as_deref() != Some("deadbeef")
        || f.as_deref() != Some("x.tar.xz")
    {
        die("warm-selftest: parse_source_lock did not parse a well-formed lock".into());
    }
    if parse_source_lock("garbage\nno fields here\n").0.is_some() {
        die("warm-selftest: parse_source_lock accepted a malformed lock".into());
    }
    if strip_scheme("https://h/p") != "h/p" || strip_scheme("http://h/p") != "h/p" {
        die("warm-selftest: strip_scheme wrong".into());
    }

    // 2) linux_version_code + version.h (the glibc "TOO OLD!" guard).
    if linux_version_code("4.14.67") != 265795 {
        die(format!("warm-selftest: linux_version_code(4.14.67)={} != 265795", linux_version_code("4.14.67")));
    }
    if linux_ver_from_file("linux-4.14.67.tar.xz").as_deref() != Some("4.14.67") {
        die("warm-selftest: linux_ver_from_file wrong".into());
    }
    if !version_h(265795).contains("#define LINUX_VERSION_CODE 265795") {
        die("warm-selftest: version_h missing LINUX_VERSION_CODE".into());
    }

    // 3) cargo_config: routes crates.io at the proxy via sparse source replacement.
    let cfg = cargo_config("127.0.0.1:4321");
    if !cfg.contains("replace-with = \"td-proxy\"") || !cfg.contains("registry = \"sparse+http://127.0.0.1:4321/\"") {
        die("warm-selftest: cargo_config does not route crates.io through the proxy".into());
    }

    // 4) The IN-PROCESS cargo-proxy (the crown-jewel replacement for the shell's background
    //    process + log scrape): bind it, then drive a verifying source-crate GET through it
    //    against a mock upstream. A crate whose bytes match its index cksum round-trips; one
    //    whose bytes mismatch is REFUSED (the verifying egress is load-bearing).
    let good: Vec<u8> = (0u16..1500).map(|x| (x % 251) as u8).collect();
    let cksum = hex_sha256(&good);
    let up = TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
    let uport = up.local_addr().unwrap().port();
    let (gb, ck) = (good.clone(), cksum.clone());
    std::thread::spawn(move || loop {
        match up.accept() {
            Ok((mut c, _)) => {
                let mut buf = [0u8; 1024];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("");
                // index_path("warmcrate") = "wa/rm/warmcrate"; index_path("badcrate") = "ba/dc/badcrate".
                let body: Vec<u8> = match path {
                    "/wa/rm/warmcrate" => format!(
                        "{{\"name\":\"warmcrate\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{ck}\",\"features\":{{}},\"yanked\":false}}\n"
                    ).into_bytes(),
                    "/ba/dc/badcrate" => format!(
                        "{{\"name\":\"badcrate\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{}\",\"features\":{{}},\"yanked\":false}}\n",
                        "0".repeat(64)
                    ).into_bytes(),
                    "/crates/warmcrate/warmcrate-0.1.0.crate" => gb.clone(),
                    "/crates/badcrate/badcrate-0.1.0.crate" => b"bytes-that-do-not-match-the-cksum".to_vec(),
                    _ => Vec::new(),
                };
                let _ = respond(&mut c, 200, "OK", &body);
            }
            Err(_) => break,
        }
    });
    let ubase = format!("http://127.0.0.1:{uport}");
    std::env::set_var("TD_INDEX_BASE", &ubase);
    std::env::set_var("TD_CRATES_BASE", &ubase);

    let store = std::env::temp_dir().join(format!("td-warm-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    let addr = start_cargo_proxy(&store).unwrap_or_else(|e| die(format!("warm-selftest: {e}")));
    let got = try_get(&format!("http://{addr}/dl/warmcrate/0.1.0/download"))
        .unwrap_or_else(|e| die(format!("warm-selftest: source-crate GET through the in-process proxy failed: {e}")));
    if got != good || hex_sha256(&got) != cksum {
        die("warm-selftest: the in-process proxy served a source crate that differs from upstream / its index cksum".into());
    }
    if try_get(&format!("http://{addr}/dl/badcrate/0.1.0/download")).is_ok() {
        die("warm-selftest: the in-process proxy SERVED a crate whose bytes mismatch its index cksum — verify-on-fetch is not load-bearing".into());
    }
    let _ = std::fs::remove_dir_all(&store);

    println!(
        "td-feed: warm selftest OK — parse_source_lock (+malformed reject), linux_version_code/version.h \
         (the glibc TOO-OLD guard), cargo_config (sparse source replacement), and the IN-PROCESS cargo-proxy \
         round-trip a verifying source-crate GET over loopback (mock upstream 127.0.0.1:{uport}); a crate whose \
         bytes mismatch its index cksum is refused (the verifying egress is load-bearing)"
    );
}

fn warm_usage() -> ! {
    eprintln!(
        "usage:\n  td-feed warm index INDEX STORE        (also: td-feed warm INDEX STORE)\n  \
         td-feed warm crate CRATE VERSION [DEST]\n  td-feed warm crate-local SRCDIR DEST\n  \
         td-feed warm sources\n  td-feed warm kernel-headers ARCH"
    );
    std::process::exit(2);
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        // warm <action> — the structured host-PREP orchestration (consolidated warm-*.sh).
        // The low-level `warm INDEX STORE` primitive (feed-shared gate, feed-ensure serve)
        // stays: dispatch on a known action keyword, else treat it as the legacy 2-arg form.
        Some("warm") => {
            // The legacy primitive: warm an `<path> <url> <sha256>` index into a store.
            let warm_index = |index: &str, store: &str| {
                let entries = read_index(index);
                let store = PathBuf::from(store);
                match warm(&entries, &store) {
                    Ok((fetched, w)) => println!(
                        "td-feed: warm OK — {fetched} fetched, {w} already warm, {} total -> {}",
                        entries.len(),
                        store.display()
                    ),
                    Err(e) => die(e),
                }
            };
            let root = repo_root();
            match a.get(2).map(String::as_str) {
                Some("index") if a.len() == 5 => warm_index(&a[3], &a[4]),
                Some("crate") if a.len() == 5 => warm_crate(&root, &a[3], &a[4], &a[3]),
                Some("crate") if a.len() == 6 => warm_crate(&root, &a[3], &a[4], &a[5]),
                Some("crate-local") if a.len() == 5 => warm_crate_local(&root, &a[3], &a[4]),
                Some("sources") if a.len() == 3 => warm_sources(&root),
                Some("kernel-headers") if a.len() == 4 => warm_kernel_headers(&root, &a[3]),
                // Legacy: `warm INDEX STORE` (a[2] is an index path, not an action keyword).
                // Exclude every action keyword so a mis-argc'd action (e.g. `warm crate X`)
                // reports usage instead of being misread as an index path.
                Some(kw)
                    if a.len() == 4
                        && !matches!(kw, "index" | "crate" | "crate-local" | "sources" | "kernel-headers") =>
                {
                    warm_index(&a[2], &a[3])
                }
                _ => warm_usage(),
            }
        }
        Some("warm-selftest") if a.len() == 2 => warm_selftest(),
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
                "usage:\n  td-feed warm INDEX STORE   (low-level; also: warm index INDEX STORE)\n  \
                 td-feed warm crate CRATE VERSION [DEST]\n  td-feed warm crate-local SRCDIR DEST\n  \
                 td-feed warm sources\n  td-feed warm kernel-headers ARCH\n  td-feed serve STORE ADDR\n  \
                 td-feed cargo-proxy STORE ADDR\n  td-feed selftest\n  td-feed cargo-proxy-selftest\n  \
                 td-feed warm-selftest"
            );
            std::process::exit(2);
        }
    }
}
