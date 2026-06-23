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
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// GET `url` and return its body, or exit(1) with a message.
fn get_body(url: &str) -> Vec<u8> {
    let resp = ureq::get(url).call().unwrap_or_else(|e| {
        eprintln!("td-fetch: GET {url}: {e}");
        std::process::exit(1);
    });
    let mut body = Vec::new();
    resp.into_reader()
        .read_to_end(&mut body)
        .expect("read body");
    body
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

/// Fetch `url`, verify its sha256 == `want`; exit(1) on mismatch. Returns the bytes.
fn fetch_verified(url: &str, want: &str) -> Vec<u8> {
    let body = get_body(url);
    let got = hex_sha256(&body);
    if got != want {
        eprintln!("td-fetch: sha256 mismatch for {url}\n  want {want}\n  got  {got}");
        std::process::exit(1);
    }
    body
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

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("fetch") if a.len() == 5 => {
            let (url, want, out) = (&a[2], a[3].to_lowercase(), &a[4]);
            // Reroute through the td-feed mirror if TD_FEED_BASE is set; verify either way.
            let effective = feed_url(url);
            let body = fetch_verified(&effective, &want);
            std::fs::write(out, &body).expect("write out");
            let via = if effective != *url {
                format!(" (via feed {effective})")
            } else {
                String::new()
            };
            println!("td-fetch: {} bytes, sha256 {} -> {}{}", body.len(), want, out, via);
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
            let got = fetch_verified(&url, &want);
            let _ = server.join();
            if got != body {
                eprintln!("td-fetch: loopback body differs from source FILE");
                std::process::exit(1);
            }
            println!(
                "td-fetch: loopback round-trip OK ({} bytes, sha256 {}) via 127.0.0.1:{}",
                got.len(),
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
