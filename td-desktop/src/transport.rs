//! SSH carries two request/reply streams so a frame cannot monopolize the
//! writer. The installed helper relays validated requests to one local seat.
use crate::diagnostic;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use td_alloc::{Budget, BudgetShared};
use td_desktop_protocol::{Reply, Request, MAX_REPLY, MAX_REQUEST};
use td_remote::{Hello, Inbox, Link, Options, Sender};

pub const DISPLAY: u32 = 1;
pub const CONTROL: u32 = 2;
const SOCKET: &str = "/run/td-compositor/1000/desktop";

pub fn options(incoming: usize) -> Options {
    Options {
        hello: Hello {
            program: td_desktop_protocol::PROGRAM,
            version: td_desktop_protocol::VERSION,
        },
        streams: &[DISPLAY, CONTROL],
        max_message: incoming,
        unread: 1,
    }
}

// td-remote uses one bound for sending and receiving. Enforce each
// direction's smaller bound before dispatch or copying into the local socket.
pub fn serve() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let socket = td_fs::unix_connect_until(Path::new(SOCKET), deadline).map_err(diagnostic)?;
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(diagnostic)?;
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(diagnostic)?;
    let mut link = Link::over(io::stdin(), io::stdout(), options(MAX_REPLY)).map_err(diagnostic)?;
    let budget = Budget::new(4096).map_err(diagnostic)?;
    let socket = BudgetShared::new(&budget, Mutex::new(socket)).map_err(diagnostic)?;
    let frames = link
        .take_inbox(DISPLAY)
        .ok_or_else(|| diagnostic("missing frame stream"))?;
    let control = link
        .take_inbox(CONTROL)
        .ok_or_else(|| diagnostic("missing input stream"))?;
    let sender = link.sender();
    let output = socket.try_clone().map_err(diagnostic)?;
    let (done, ended) = std::sync::mpsc::sync_channel(2);
    let done = BudgetShared::new(&budget, done).map_err(diagnostic)?;
    let display_done = done.try_clone().map_err(diagnostic)?;
    std::thread::Builder::new()
        .spawn(move || {
            let _ = display_done.send(relay(frames, sender, DISPLAY, &output));
        })
        .map_err(diagnostic)?;
    let sender = link.sender();
    std::thread::Builder::new()
        .spawn(move || {
            let _ = done.send(relay(control, sender, CONTROL, &socket));
        })
        .map_err(diagnostic)?;
    ended.recv().map_err(diagnostic)?
}

fn relay(
    inbox: Inbox,
    sender: Sender,
    channel: u32,
    socket: &Mutex<UnixStream>,
) -> Result<(), String> {
    loop {
        let bytes = inbox.recv().map_err(diagnostic)?;
        if bytes.len() > MAX_REQUEST {
            return Err(diagnostic("desktop request too large"));
        }
        let request = Request::decode(&bytes).map_err(diagnostic)?;
        if matches!(request, Request::Frame) != (channel == DISPLAY) {
            return Err(diagnostic("desktop request on the wrong stream"));
        }
        let reply = {
            let mut stream = socket
                .lock()
                .map_err(|_| diagnostic("desktop socket failed"))?;
            let length = u32::try_from(bytes.len()).map_err(diagnostic)?;
            stream
                .write_all(&length.to_be_bytes())
                .map_err(diagnostic)?;
            stream.write_all(&bytes).map_err(diagnostic)?;
            let mut header = [0; 4];
            stream.read_exact(&mut header).map_err(diagnostic)?;
            let size = usize::try_from(u32::from_be_bytes(header)).map_err(diagnostic)?;
            let limit = if channel == DISPLAY {
                MAX_REPLY
            } else {
                1 + td_desktop_protocol::MAX_TEXT
            };
            if size == 0 || size > limit {
                return Err(diagnostic("invalid local reply size"));
            }
            let mut bytes = td_alloc::filled(size, 0u8).map_err(diagnostic)?;
            stream.read_exact(&mut bytes).map_err(diagnostic)?;
            bytes
        };
        Reply::decode(&reply).map_err(diagnostic)?;
        sender.send(channel, &reply).map_err(diagnostic)?;
    }
}

pub fn encode(request: Request<'_>) -> Result<Vec<u8>, String> {
    let capacity = match request {
        Request::WriteText { text, .. } => text.len().saturating_add(9),
        _ => 17,
    };
    if capacity > MAX_REQUEST {
        return Err(diagnostic("clipboard exceeds 64 KiB"));
    }
    let mut payload = td_alloc::with_capacity(capacity).map_err(diagnostic)?;
    request.write(&mut payload).map_err(diagnostic)?;
    Ok(payload)
}
