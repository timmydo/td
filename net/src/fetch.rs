// td-fetch — td's OWN seed fetcher (move-off-Guile §5). Two modes:
//
//   td-fetch fetch URL SHA256-HEX OUT   GET a pinned blob (http/https), verify its
//                                       sha256, write it to OUT. This replaces guix
//                                       url-fetch as the FETCHER of the pinned
//                                       fixed-output seeds (the tsgo tarball, crates,
//                                       source tarballs); td-builder then PLACES the
//                                       verified blob (store-add). The real external
//                                       (TLS) fetch runs in the network-permitted PREP
//                                       on the host — the offline loop never egresses.
//
//   td-fetch selftest FILE SHA256-HEX   Self-contained LOOPBACK round-trip (offline,
//                                       like the russh gate's loopback SSH): serve
//                                       FILE's bytes over HTTP on 127.0.0.1:<ephemeral>
//                                       from a worker thread, then fetch+verify them
//                                       back through the SAME client path. Exits 0 iff
//                                       the fetched bytes' sha256 equals SHA256-HEX —
//                                       so a wrong hash (or a perturbed FILE) reds it.
//
// Pure-Rust TLS (ureq + rustls/ring), no node/curl/openssl. The loopback server uses
// only std::net, so it adds no crate to the vendored closure.
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_FETCH_BYTES: u64 = 16 * 1024 * 1024 * 1024;

fn file_sha256(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let size = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    if size > MAX_FETCH_BYTES {
        return Err(format!(
            "fetch hash input {} exceeds its {MAX_FETCH_BYTES}-byte limit",
            path.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > MAX_FETCH_BYTES {
            return Err(format!(
                "fetch hash input {} exceeds its {MAX_FETCH_BYTES}-byte limit",
                path.display()
            ));
        }
        let chunk = buf
            .get(..n)
            .ok_or_else(|| "fetch hash read exceeded its buffer".to_string())?;
        hasher.update(chunk);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

/// Reroute through the td-feed mirror when `TD_FEED_BASE` is set: rewrite an upstream
/// `https://HOST/PATH` (or `http://…`) to `$TD_FEED_BASE/HOST/PATH` — the feed's URL-path
/// mirror layout (feed/, td-feed). Verification is unchanged: the sha256 still pins the
/// content, and the feed re-verifies on serve, so routing through it is safe. Unset (or a
/// non-http URL) ⇒ the URL is returned as-is. This is how td-native fetchers' web requests
/// are served through the feed (the offline loop points TD_FEED_BASE at the warm feed).
fn feed_url(url: &str) -> String {
    match std::env::var("TD_FEED_BASE") {
        Ok(base) if !base.is_empty() => {
            match url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
                Some(rest) => format!("{}/{}", base.trim_end_matches('/'), rest),
                None => url.to_string(),
            }
        }
        _ => url.to_string(),
    }
}

struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

fn sweep_download_temps(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let abandoned = entries.flatten().filter(|entry| {
        let name = entry.file_name();
        let bytes = name.as_bytes();
        bytes.starts_with(b".td-fetch-download-") && bytes.ends_with(b".tmp")
    });
    for entry in abandoned.take(4096) {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Fetch to a crash-swept sibling, verify without retaining the body in
/// memory, then atomically publish. The directory lock both deduplicates
/// concurrent callers and makes abandoned-partial cleanup race-free.
fn fetch_verified_to(url: &str, want: &str, out: &Path) -> Result<u64, String> {
    static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
    let parent = out
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let lock_path = parent.join(".td-fetch-download.lock");
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
    if let Ok((got, len)) = file_sha256(out) {
        if got == want {
            return Ok(len);
        }
    }
    let (tmp, reservation) = loop {
        let nonce = NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".td-fetch-download-{}-{nonce}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("reserve {}: {e}", candidate.display())),
        }
    };
    let cleanup = RemoveOnDrop(&tmp);
    crate::http::get_to_file(url, &tmp, MAX_FETCH_BYTES)?;
    let (got, len) = file_sha256(&tmp)?;
    if got != want {
        return Err(format!(
            "sha256 mismatch for {url}\n  want {want}\n  got  {got}"
        ));
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("publish {}: {e}", out.display()))?;
    drop(cleanup);
    drop(reservation);
    drop(directory_lock);
    Ok(len)
}

/// A one-connection HTTP/1.1 responder: read+discard the request, send `body`.
fn serve_once(conn: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    // Read the request head (up to the blank line) so the client can write fully.
    let mut buf = [0u8; 1024];
    let _ = conn.read(&mut buf)?;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()
}

pub fn run(a: &[String]) {
    match a.get(1).map(String::as_str) {
        Some("fetch") if a.len() == 5 => {
            let (url, want, out) = (&a[2], a[3].to_lowercase(), &a[4]);
            // Reroute through the td-feed mirror if TD_FEED_BASE is set; verify either way.
            let effective = feed_url(url);
            let len = fetch_verified_to(&effective, &want, Path::new(out)).unwrap_or_else(|e| {
                eprintln!("td-fetch: {e}");
                std::process::exit(1);
            });
            let via = if effective != *url {
                format!(" (via feed {effective})")
            } else {
                String::new()
            };
            println!("td-fetch: {len} bytes, sha256 {want} -> {out}{via}");
        }
        Some("selftest") if a.len() == 4 => {
            let (file, want) = (a[2].clone(), a[3].to_lowercase());
            let body = std::fs::read(&file).expect("read FILE");
            // Bind an ephemeral loopback port; serve `body` once from a worker.
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().expect("local_addr").port();
            let server_body = body.clone();
            let server = std::thread::spawn(move || {
                if let Ok((mut conn, _)) = listener.accept() {
                    let _ = serve_once(&mut conn, &server_body);
                }
            });
            let url = format!("http://127.0.0.1:{port}/{}", "blob");
            let fetched = std::env::temp_dir().join(format!(
                "td-fetch-selftest-{}-{}.tmp",
                std::process::id(),
                body.len()
            ));
            let got_len = fetch_verified_to(&url, &want, &fetched).unwrap_or_else(|e| {
                eprintln!("td-fetch: {e}");
                std::process::exit(1);
            });
            let _ = server.join();
            let got = std::fs::read(&fetched).unwrap_or_else(|e| {
                eprintln!("td-fetch: read selftest output {}: {e}", fetched.display());
                std::process::exit(1);
            });
            let _ = std::fs::remove_file(&fetched);
            if got != body {
                eprintln!("td-fetch: loopback body differs from source FILE");
                std::process::exit(1);
            }
            println!(
                "td-fetch: loopback round-trip OK ({} bytes, sha256 {}) via 127.0.0.1:{}",
                got_len,
                want,
                port
            );
        }
        _ => {
            eprintln!(
                "usage:\n  td-fetch fetch URL SHA256-HEX OUT\n  td-fetch selftest FILE SHA256-HEX"
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "td-fetch-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn an_admitted_fetch_reuses_a_concurrent_publish_and_sweeps_crash_partials() {
        let dir = temp_dir("dedup");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("crate");
        let stale = dir.join(".td-fetch-download-9-9.tmp");
        std::fs::write(&out, b"already published").unwrap();
        std::fs::write(&stale, b"abandoned").unwrap();
        let want = format!("{:x}", Sha256::digest(b"already published"));

        let len = fetch_verified_to("http://127.0.0.1:0/must-not-connect", &want, &out).unwrap();

        assert_eq!(len, 17);
        assert!(!stale.exists());
        assert_eq!(std::fs::read(&out).unwrap(), b"already published");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hash_rejects_an_oversized_existing_fetch_before_reading() {
        let dir = temp_dir("oversized");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FETCH_BYTES + 1).unwrap();

        let error = file_sha256(&path).unwrap_err();

        assert!(error.contains("exceeds"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
