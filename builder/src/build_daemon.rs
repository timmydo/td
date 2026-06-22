//! td's own persistent BUILD daemon (own-builder-daemon track): a long-running
//! td-builder that realizes derivations served over a Unix socket — the loop's
//! builder instead of guix-daemon. `serve` is the accept loop; `request` is the
//! in-process client (so a caller needs no nc/socat). Realize itself is injected
//! as a closure that reuses the exact `realize_drv` path (same userns sandbox,
//! NEWPID, no guix-daemon) — the daemon only adds persistence + a socket front
//! end. Line protocol (one request per connection):
//!   request  = "<drv-path>\n"           (or "SHUTDOWN\n" for a clean stop)
//!   response = "OK <canonical-store-path> <host-output-path>\n" | "ERR <msg>\n"

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Accept-loop over a Unix socket at `socket`. For each connection, read one
/// request line and realize it via `realize`, passing the daemon's persistent
/// `scratch_base`, then write the response. The `realize` callback owns where
/// under `scratch_base` it builds (the daemon arm keys a CONTENT-ADDRESSED
/// per-drv subdir, so a repeat request for the same drv reuses + reads back a
/// valid prior realization instead of rebuilding — guix-daemon parity). Serves
/// requests until a "SHUTDOWN" line (or the socket errors). `realize(drv,
/// scratch_base)` returns (canonical store path, host-side output path).
pub fn serve(
    socket: &str,
    realize: impl Fn(&str, &Path) -> Result<(String, String), String>,
    scratch_base: &Path,
) -> Result<(), String> {
    // A stale socket from a prior run would make bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(socket);
    std::fs::create_dir_all(scratch_base).map_err(|e| e.to_string())?;
    let listener = UnixListener::bind(socket).map_err(|e| format!("bind {socket}: {e}"))?;
    for conn in listener.incoming() {
        let conn = conn.map_err(|e| format!("accept: {e}"))?;
        // Read the request line; scope the reader's borrow before writing back.
        let req = {
            let mut line = String::new();
            BufReader::new(&conn)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            line.trim().to_string()
        };
        if req.is_empty() || req == "SHUTDOWN" {
            let _ = (&conn).write_all(b"OK shutdown\n");
            break;
        }
        let resp = match realize(&req, scratch_base) {
            Ok((canon, host)) => format!("OK {canon} {host}\n"),
            // Keep the response a single line — a realize error can be multi-line.
            Err(e) => format!("ERR {}\n", e.replace('\n', " ")),
        };
        let _ = (&conn).write_all(resp.as_bytes());
    }
    Ok(())
}

/// Connect to the daemon at `socket`, send `drv` (a derivation file path), and
/// return its single-line response ("OK …" or "ERR …").
pub fn request(socket: &str, drv: &str) -> Result<String, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("connect {socket}: {e}"))?;
    writeln!(&stream, "{drv}").map_err(|e| e.to_string())?;
    let mut resp = String::new();
    BufReader::new(&stream)
        .read_line(&mut resp)
        .map_err(|e| e.to_string())?;
    Ok(resp.trim_end().to_string())
}
