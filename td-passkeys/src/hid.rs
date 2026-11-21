//! Device side of the 64-byte CTAPHID transport. One ceremony can be
//! pending. A reply belongs to the same transaction even after resync.
use crate::{Error, Result};
use std::time::{Duration, Instant};
use td_fido::{
    fido_cbor::Encoder,
    fido_hid::{self as hid, Decoder, Event, Message, BROADCAST},
    web_request::{self, Request},
};

pub type Report = [u8; hid::REPORT_SIZE];
const PING: u8 = 0x81;
const INIT: u8 = 0x86;
const CBOR: u8 = 0x90;
const CANCEL: u8 = 0x91;
const ERROR: u8 = 0xbf;
const KEEPALIVE: u8 = 0xbb;
const FRAGMENT_TIMEOUT: Duration = Duration::from_secs(3);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_millis(90);

pub struct Command {
    ticket: u64,
    message: Message,
}
impl Command {
    pub fn ticket(&self) -> u64 {
        self.ticket
    }
    pub fn request(&self) -> Result<Request<'_>> {
        web_request::parse(self.message.as_ref()).map_err(|_| Error::Invalid)
    }
}

struct Fragments {
    channel: u32,
    ping: bool,
    decoder: Decoder,
    end: Instant,
}
struct Waiting {
    channel: u32,
    ticket: u64,
    end: Instant,
    heartbeat: Instant,
}
#[derive(Default)]
pub struct Authenticator {
    last_channel: u32,
    serial: u64,
    fragments: Option<Fragments>,
    waiting: Option<Waiting>,
}

fn control(channel: u32, command: u8, data: &[u8]) -> Result<Report> {
    let mut report = [0; 64];
    let (cid, rest) = report.split_at_mut(4);
    cid.copy_from_slice(&channel.to_be_bytes());
    let (head, body) = rest.split_at_mut(3);
    head.copy_from_slice(&[
        command,
        0,
        u8::try_from(data.len()).map_err(|_| Error::Invalid)?,
    ]);
    body.get_mut(..data.len())
        .ok_or(Error::Invalid)?
        .copy_from_slice(data);
    Ok(report)
}

impl Authenticator {
    pub fn active(&self, ticket: u64) -> bool {
        self.waiting
            .as_ref()
            .is_some_and(|w| w.ticket == ticket && Instant::now() < w.end)
    }
    pub fn poll(
        &mut self,
        now: Instant,
        mut send: impl FnMut(&Report) -> Result<()>,
    ) -> Result<()> {
        if let Some(f) = self.fragments.as_ref().filter(|f| now >= f.end) {
            send(&control(f.channel, ERROR, &[5])?)?;
            self.fragments = None;
        }
        if let Some(w) = &mut self.waiting {
            if now >= w.end {
                send(&control(w.channel, CBOR, &[0x2f])?)?;
                self.waiting = None;
            } else if now >= w.heartbeat {
                send(&control(w.channel, KEEPALIVE, &[2])?)?;
                w.heartbeat = now + HEARTBEAT;
            }
        }
        Ok(())
    }
    pub fn finish(
        &mut self,
        ticket: u64,
        status: u8,
        body: &[u8],
        mut send: impl FnMut(&Report) -> Result<()>,
    ) -> Result<()> {
        let waiting = self
            .waiting
            .as_ref()
            .filter(|w| w.ticket == ticket)
            .ok_or(Error::Stale)?;
        if Instant::now() >= waiting.end {
            let channel = waiting.channel;
            self.waiting = None;
            send(&control(channel, CBOR, &[0x2f])?)?;
            return Err(Error::Stale);
        }
        let channel = waiting.channel;
        if body.len() >= hid::MAX_MESSAGE || (status != 0 && !body.is_empty()) {
            return Err(Error::Invalid);
        }
        let mut bytes = td_alloc::with_capacity(body.len() + 1)?;
        td_alloc::push(&mut bytes, status)?;
        td_alloc::extend_from_slice(&mut bytes, body)?;
        self.waiting = None;
        for r in hid::cbor(channel, &bytes)
            .map_err(|_| Error::Invalid)?
            .as_ref()
        {
            send(r)?;
        }
        Ok(())
    }
    pub fn receive(
        &mut self,
        report: &Report,
        now: Instant,
        mut send: impl FnMut(&Report) -> Result<()>,
    ) -> Result<Option<Command>> {
        self.poll(now, &mut send)?;
        let (cid, rest) = report.split_at(4);
        let channel = u32::from_be_bytes(cid.try_into().map_err(|_| Error::Invalid)?);
        let [command, hi, lo, data @ ..] = rest else {
            return Err(Error::Invalid);
        };
        let size = u16::from_be_bytes([*hi, *lo]);
        let allocated = channel != 0 && channel != BROADCAST && channel <= self.last_channel;
        let fail = |send: &mut dyn FnMut(&Report) -> Result<()>, status: u8| {
            send(&control(channel, ERROR, &[status])?)?;
            Ok(None)
        };
        let owner = self
            .waiting
            .as_ref()
            .map(|w| w.channel)
            .or_else(|| self.fragments.as_ref().map(|f| f.channel));
        if owner.is_some_and(|active| active != channel) {
            if *command == CANCEL {
                return Ok(None);
            }
            return fail(&mut send, 6);
        }
        if *command == INIT {
            if size != 8 {
                return fail(&mut send, 3);
            }
            let id = if channel == BROADCAST {
                let id = self
                    .last_channel
                    .checked_add(1)
                    .filter(|id| *id != BROADCAST)
                    .ok_or(Error::Full)?;
                self.last_channel = id;
                id
            } else if allocated {
                channel
            } else {
                return fail(&mut send, 0x0b);
            };
            if self.waiting.as_ref().is_some_and(|w| w.channel == channel) {
                self.waiting = None;
            }
            if self
                .fragments
                .as_ref()
                .is_some_and(|f| f.channel == channel)
            {
                self.fragments = None;
            }
            let mut reply = [0; 17];
            let (nonce, remainder) = reply.split_at_mut(8);
            nonce.copy_from_slice(data.get(..8).ok_or(Error::Invalid)?);
            let (cid, version) = remainder.split_at_mut(4);
            cid.copy_from_slice(&id.to_be_bytes());
            version.copy_from_slice(&[2, 1, 0, 0, 0x0c]);
            send(&control(channel, INIT, &reply)?)?;
            return Ok(None);
        }
        if !allocated {
            return fail(&mut send, 0x0b);
        }
        if *command == CANCEL {
            if size != 0 {
                return fail(&mut send, 3);
            }
            if self.waiting.as_ref().is_some_and(|w| w.channel == channel) {
                self.waiting = None;
                send(&control(channel, CBOR, &[0x2d])?)?;
            }
            return Ok(None);
        }
        if self.waiting.is_some() {
            return fail(&mut send, 6);
        }
        if matches!(*command, CBOR | PING) {
            if (*command == CBOR && size == 0) || usize::from(size) > hid::MAX_MESSAGE {
                return fail(&mut send, 3);
            }
            if self.fragments.is_some() {
                return fail(&mut send, 6);
            }
            self.fragments = Some(Fragments {
                channel,
                ping: *command == PING,
                decoder: (if *command == PING {
                    Decoder::ping(channel)
                } else {
                    Decoder::cbor(channel)
                })
                .map_err(|_| Error::Invalid)?,
                end: now + FRAGMENT_TIMEOUT,
            });
        } else if command & 0x80 != 0 {
            return fail(&mut send, 1);
        }
        let Some(f) = &mut self.fragments else {
            return Ok(None);
        };
        let ping = f.ping;
        let event = Decoder::push(&mut f.decoder, report);
        if matches!(event, Ok(Event::Pending)) {
            return Ok(None);
        }
        self.fragments = None;
        let Ok(Event::Complete(message)) = event else {
            return fail(&mut send, 4);
        };
        if ping {
            for report in hid::ping(channel, message.as_ref())
                .map_err(|_| Error::Invalid)?
                .as_ref()
            {
                send(report)?;
            }
            return Ok(None);
        }
        match web_request::parse(message.as_ref()) {
            Err(e) => send(&control(channel, CBOR, &[e as u8])?)?,
            Ok(Request::Info) => {
                let info = info()?;
                for r in hid::cbor(channel, &info)
                    .map_err(|_| Error::Invalid)?
                    .as_ref()
                {
                    send(r)?;
                }
            }
            Ok(_) => {
                self.serial = self.serial.checked_add(1).ok_or(Error::Full)?;
                self.waiting = Some(Waiting {
                    channel,
                    ticket: self.serial,
                    end: now + APPROVAL_TIMEOUT,
                    heartbeat: now + HEARTBEAT,
                });
                return Ok(Some(Command {
                    ticket: self.serial,
                    message,
                }));
            }
        }
        Ok(None)
    }
}

fn info() -> Result<Vec<u8>> {
    // FIDO_2_0 uses a presence-only empty-pinAuth makeCredential to select.
    let mut e = Encoder::new();
    let body = (|| {
        e.head(5, 6)?;
        e.head(0, 1)?;
        e.head(4, 1)?;
        e.text("FIDO_2_0")?;
        e.head(0, 3)?;
        e.bytes(&[0; 16])?;
        e.head(0, 4)?;
        e.head(5, 4)?;
        for (k, v) in [("rk", true), ("up", true), ("uv", true), ("plat", false)] {
            e.text(k)?;
            e.boolean(v)?;
        }
        e.head(0, 5)?;
        e.head(0, hid::MAX_MESSAGE as u64)?;
        e.head(0, 7)?;
        e.head(0, web_request::MAX_CREDENTIALS as u64)?;
        e.head(0, 8)?;
        e.head(0, 1023)?;
        e.finish()
    })()
    .map_err(|_| Error::Invalid)?;
    let mut reply = td_alloc::with_capacity(body.len() + 1)?;
    td_alloc::push(&mut reply, 0)?;
    td_alloc::extend_from_slice(&mut reply, &body)?;
    Ok(reply)
}
