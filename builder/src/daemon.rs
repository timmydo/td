//! A minimal guix-daemon worker-protocol CLIENT — just enough to
//! `addTextToStore`, so td can REGISTER its own constructed `.drv` in the store
//! without guile's `(derivation …)` (the evaluator-as-library wiring;
//! DESIGN §7.1 td-drv-add). The protocol is transcribed from `(guix store)` /
//! `(guix serialization)` at the pin: 8-byte little-endian ints, strings framed
//! as length + bytes + zero-pad to an 8-byte boundary (the same framing as the
//! NAR strings), the magic/version handshake, and the `process-stderr` stream.
//!
//! The daemon (C++) stays the store/build backend; what this removes from the
//! `.drv` path is the GUILE client, not the daemon.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

const WORKER_MAGIC_1: u64 = 0x6e69_7863; // "nixc"
const WORKER_MAGIC_2: u64 = 0x6478_696f; // "dxio"
const PROTOCOL_VERSION: u64 = 0x163;
const WOP_ADD_TEXT_TO_STORE: u64 = 8;

const STDERR_NEXT: u64 = 0x6f6c_6d67; // a log string
const STDERR_READ: u64 = 0x6461_7461; // daemon wants input
const STDERR_WRITE: u64 = 0x6461_7416; // daemon sends data
const STDERR_LAST: u64 = 0x616c_7473; // done
const STDERR_ERROR: u64 = 0x6378_7470; // error + status

pub const DEFAULT_SOCKET: &str = "/var/guix/daemon-socket/socket";

fn ioerr(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.into())
}

fn write_int(w: &mut impl Write, n: u64) -> io::Result<()> {
    w.write_all(&n.to_le_bytes())
}

fn read_int(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// length (int) + bytes + zero-pad to the next 8-byte boundary.
fn write_bytes(w: &mut impl Write, s: &[u8]) -> io::Result<()> {
    write_int(w, s.len() as u64)?;
    w.write_all(s)?;
    let pad = (8 - s.len() % 8) % 8;
    w.write_all(&[0u8; 8][..pad])
}

fn read_bytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_int(r)? as usize;
    let pad = (8 - len % 8) % 8;
    let mut buf = vec![0u8; len + pad];
    r.read_exact(&mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

pub struct Daemon {
    sock: UnixStream,
}

impl Daemon {
    /// Connect and perform the worker-protocol handshake.
    pub fn connect(path: &str) -> io::Result<Daemon> {
        let mut sock = UnixStream::connect(path)?;
        write_int(&mut sock, WORKER_MAGIC_1)?;
        if read_int(&mut sock)? != WORKER_MAGIC_2 {
            return Err(ioerr("daemon handshake: wrong magic"));
        }
        let v = read_int(&mut sock)?;
        if (v >> 8) & 0xff != (PROTOCOL_VERSION >> 8) & 0xff {
            return Err(ioerr(format!("daemon protocol major mismatch (daemon {v:#x})")));
        }
        write_int(&mut sock, PROTOCOL_VERSION)?;
        // The daemon reads these optional fields gated on OUR advertised client
        // minor (PROTOCOL_VERSION & 0xff = 0x63 = 99, always >= 14 and >= 11), not
        // on its own version — so write both unconditionally.
        write_int(&mut sock, 0)?; // cpu-affinity: none (client minor >= 14)
        write_int(&mut sock, 0)?; // reserve-space: no  (client minor >= 11)
        sock.flush()?;
        let mut d = Daemon { sock };
        d.drain_stderr()?; // the handshake's process-stderr loop
        Ok(d)
    }

    /// Drain the daemon's stderr stream until STDERR_LAST; raise on STDERR_ERROR.
    fn drain_stderr(&mut self) -> io::Result<()> {
        loop {
            match read_int(&mut self.sock)? {
                STDERR_LAST => return Ok(()),
                STDERR_NEXT => {
                    let _ = read_bytes(&mut self.sock)?; // log line, ignored
                }
                STDERR_WRITE => {
                    let _ = read_bytes(&mut self.sock)?; // not expected here
                }
                STDERR_ERROR => {
                    let msg = read_bytes(&mut self.sock)?;
                    let _status = read_int(&mut self.sock).unwrap_or(1);
                    return Err(ioerr(format!(
                        "daemon error: {}",
                        String::from_utf8_lossy(&msg)
                    )));
                }
                STDERR_READ => {
                    return Err(ioerr("daemon asked for input (unexpected)"));
                }
                other => return Err(ioerr(format!("unexpected stderr tag {other:#x}"))),
            }
        }
    }

    /// `addTextToStore(name, text, references)` — write `text` under `name` into
    /// the store with the given `references`, and return the resulting store
    /// path. The path the daemon computes is `makeTextPath`, the same one
    /// `store::make_text_path` does — so a td-constructed `.drv` lands at td's
    /// computed path.
    pub fn add_text_to_store(
        &mut self,
        name: &str,
        text: &[u8],
        references: &[String],
    ) -> io::Result<String> {
        write_int(&mut self.sock, WOP_ADD_TEXT_TO_STORE)?;
        write_bytes(&mut self.sock, name.as_bytes())?;
        write_bytes(&mut self.sock, text)?;
        write_int(&mut self.sock, references.len() as u64)?;
        for r in references {
            write_bytes(&mut self.sock, r.as_bytes())?;
        }
        self.sock.flush()?;
        self.drain_stderr()?;
        let path = read_bytes(&mut self.sock)?;
        String::from_utf8(path).map_err(|_| ioerr("daemon returned a non-UTF-8 path"))
    }
}
