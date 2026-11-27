use crate::{diagnostic, transport};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::Duration;
use td_desktop_protocol::{Output, Position, Reply, Request, MAX_REPLY};
use td_remote::{Host, Link, RemoteProgram};
use td_ui::client::{self, App, Client, ClipboardEvent, Handled, KeyboardEvent};
use td_ui::clipboard::{Incoming, Outgoing, OwnedText};
use td_ui::pointer::Event as Pointer;
use td_ui::window::Object;
use td_ui::wire::{Cursor, Message};

type Result<T> = std::result::Result<T, String>;

struct Command {
    bytes: Vec<u8>,
    copy: Option<u32>,
}
enum Update {
    Connected(Link),
    Reply(u32, Option<u32>, Vec<u8>),
    End(String),
}

fn connect(
    host: Host,
    updates: &td_alloc::BudgetShared<SyncSender<Update>>,
    display: Receiver<Command>,
    control: Receiver<Command>,
) -> Result<()> {
    let mut link = Link::ssh(
        &host,
        &RemoteProgram::parse("/bin/td-desktop")?,
        &["--serve"],
        transport::options(MAX_REPLY),
    )
    .map_err(diagnostic)?;
    for (channel, commands) in [(transport::DISPLAY, display), (transport::CONTROL, control)] {
        let sender = link.sender();
        let inbox = link
            .take_inbox(channel)
            .ok_or_else(|| diagnostic("missing desktop stream"))?;
        let updates = updates.try_clone().map_err(diagnostic)?;
        std::thread::Builder::new()
            .spawn(move || {
                for command in commands {
                    match sender
                        .send(channel, &command.bytes)
                        .and_then(|()| inbox.recv())
                    {
                        Ok(reply) => {
                            if updates
                                .send(Update::Reply(channel, command.copy, reply))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(why) => {
                            let _ = updates.send(Update::End(diagnostic(why)));
                            break;
                        }
                    }
                }
            })
            .map_err(diagnostic)?;
    }
    updates.send(Update::Connected(link)).map_err(diagnostic)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct View {
    output: Output,
    width: usize,
    height: usize,
    left: usize,
    top: usize,
    drawn_width: usize,
    drawn_height: usize,
}

impl View {
    fn fit(output: Output, width: u16, height: u16, window: (usize, usize)) -> Option<Self> {
        let (width, height) = (usize::from(width), usize::from(height));
        let (w, h) = window;
        if width == 0 || height == 0 || w == 0 || h == 0 {
            return None;
        }
        let drawn_width = w.min(h.checked_mul(width)? / height).max(1);
        let drawn_height = h.min(w.checked_mul(height)? / width).max(1);
        Some(Self {
            output,
            width,
            height,
            left: (w - drawn_width) / 2,
            top: (h - drawn_height) / 2,
            drawn_width,
            drawn_height,
        })
    }

    fn at(self, x: usize, y: usize) -> Option<Position> {
        let x = x.checked_sub(self.left)?;
        let y = y.checked_sub(self.top)?;
        if x >= self.drawn_width || y >= self.drawn_height {
            return None;
        }
        Some(Position {
            output: self.output,
            x: u16::try_from(x.checked_mul(self.width)? / self.drawn_width).ok()?,
            y: u16::try_from(y.checked_mul(self.height)? / self.drawn_height).ok()?,
        })
    }

    fn paint(self, dst: &mut [u8], stride: usize, src: &[u8]) {
        for (y, row) in dst
            .chunks_exact_mut(stride)
            .enumerate()
            .skip(self.top)
            .take(self.drawn_height)
        {
            let source_y = (y - self.top) * self.height / self.drawn_height;
            let start = source_y * self.width * 4;
            let Some(source) = src.get(start..start + self.width * 4) else {
                return;
            };
            for (x, pixel) in row
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .skip(self.left)
                .take(self.drawn_width)
                .enumerate()
            {
                let from = (x * self.width / self.drawn_width) * 4;
                if let Some(value) = source.get(from..from + 4) {
                    pixel.copy_from_slice(value);
                }
            }
        }
    }
}

struct Viewer {
    title: String,
    paused: bool,
    client: Client<Object>,
    updates: Receiver<Update>,
    display: SyncSender<Command>,
    control: SyncSender<Command>,
    link: Option<Link>,
    window: (usize, usize),
    frame: Option<Vec<u8>>,
    shown: Option<View>,
    dirty: bool,
    pending_frame: bool,
    next_frame: u64,
    clock: u64,
    point: (i32, i32),
    motion_pending: bool,
    controls_pending: usize,
    buttons: u8,
    copy: Option<u32>,
    owned: Option<OwnedText>,
    paste: Option<(u32, u32)>,
    incoming: Option<(u64, Incoming)>,
    outgoing: Option<Outgoing>,
    budget: td_alloc::Budget,
    wheel: td_ui::pointer::Wheel,
}

pub fn run(host: Host) -> Result<()> {
    let title =
        td_alloc::format(format_args!("td-desktop — {}", host.as_str())).map_err(diagnostic)?;
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let client = Client::new(td_ui::wayland::connect(endpoint)?, std::env::temp_dir())?;
    let (display, frames) = mpsc::sync_channel(1);
    let (control, commands) = mpsc::sync_channel(64);
    let (tx, updates) = mpsc::sync_channel(4);
    let budget = td_alloc::Budget::new(256 * 1024).map_err(diagnostic)?;
    let tx = td_alloc::BudgetShared::new(&budget, tx).map_err(diagnostic)?;
    std::thread::Builder::new()
        .spawn(move || {
            if let Err(error) = connect(host, &tx, frames, commands) {
                let _ = tx.send(Update::End(error));
            }
        })
        .map_err(diagnostic)?;
    let mut viewer = Viewer {
        title,
        paused: false,
        client,
        updates,
        display,
        control,
        link: None,
        window: (800, 600),
        frame: None,
        shown: None,
        dirty: true,
        pending_frame: false,
        next_frame: 0,
        clock: 0,
        point: (-1, -1),
        motion_pending: false,
        controls_pending: 0,
        buttons: 0,
        copy: None,
        owned: None,
        incoming: None,
        paste: None,
        outgoing: None,
        budget,
        wheel: Default::default(),
    };
    client::run(&mut viewer)
}

impl Viewer {
    fn submit(&mut self, request: Request<'_>, copy: Option<u32>) -> Result<()> {
        if self.link.is_none() {
            return Ok(());
        }
        let display = matches!(request, Request::Frame);
        let sender = if display {
            &self.display
        } else {
            &self.control
        };
        sender
            .try_send(Command {
                bytes: transport::encode(request)?,
                copy,
            })
            .map_err(|_| diagnostic("desktop connection cannot keep up with input"))?;
        if !display {
            self.controls_pending += 1;
        }
        Ok(())
    }
    fn release(&mut self) -> Result<()> {
        self.motion_pending = false;
        self.buttons = 0;
        self.copy = None;
        self.incoming = None;
        self.paste = None;
        self.wheel = Default::default();
        self.submit(Request::ReleaseAll, None)
    }
    fn position(&self) -> Option<Position> {
        self.shown?.at(
            usize::try_from(self.point.0.div_euclid(256)).ok()?,
            usize::try_from(self.point.1.div_euclid(256)).ok()?,
        )
    }
    fn pointer(&mut self, event: Pointer) -> Result<()> {
        match event {
            Pointer::Enter { x, y, .. } | Pointer::Motion(x, y) => {
                self.point = (x, y);
                self.motion_pending = true;
            }
            Pointer::Leave(_) => {
                self.point = (-1, -1);
                return self.release();
            }
            Pointer::Button {
                button, pressed, ..
            } => {
                self.motion_pending = false;
                let bit = match button {
                    272 => 1,
                    273 => 2,
                    274 => 4,
                    _ => return Ok(()),
                };
                if pressed {
                    if self.position().is_none() {
                        return Ok(());
                    }
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                if let Some(at) = self.position() {
                    self.submit(
                        Request::Pointer {
                            at,
                            buttons: self.buttons,
                        },
                        None,
                    )?;
                } else if !pressed {
                    self.release()?;
                }
            }
            Pointer::Frame => {
                let (rows, cols) = self.wheel.frame();
                if let Some(at) = self.position() {
                    let steps = |v: isize| {
                        i16::try_from(
                            v.signum()
                                * ((v.unsigned_abs().saturating_add(2) / 3).min(32767) as isize),
                        )
                        .unwrap_or(0)
                    };
                    if rows != 0 || cols != 0 {
                        self.submit(
                            Request::Scroll {
                                at,
                                x: steps(cols),
                                y: steps(rows),
                            },
                            None,
                        )?;
                    }
                }
            }
            other => self.wheel.update(other)?,
        }
        Ok(())
    }

    fn key(&mut self, message: &Message) -> Result<()> {
        if self.shown.is_none() || !self.client.input().focused {
            return Ok(());
        }
        let mut cursor = Cursor::new(&message.payload);
        let serial = cursor.u32()?;
        let _time = cursor.u32()?;
        let code = u16::try_from(cursor.u32()?).map_err(diagnostic)?;
        let down = cursor.u32()? == 1;
        match code {
            67 if down => {
                self.copy = Some(serial);
                self.submit(Request::ReadText, Some(serial))?;
            }
            68 if down && self.client.selection_mime().is_some() => {
                self.incoming = None;
                self.paste = self.client.selection().map(|id| (serial, id));
                if self.paste.is_some() {
                    self.submit(Request::PrepareText, Some(serial))?;
                }
            }
            67 | 68 => {}
            _ => self.submit(Request::Key { code, down }, None)?,
        }
        Ok(())
    }

    fn update(&mut self, update: Update) -> Result<()> {
        match update {
            Update::Connected(link) => {
                self.link = Some(link);
                if self.client.bound() {
                    self.client.set_title(&self.title)?;
                }
            }
            Update::End(why) => return Err(why),
            Update::Reply(channel, serial, bytes) => {
                let reply = Reply::decode(&bytes).map_err(diagnostic)?;
                if channel == transport::DISPLAY {
                    if reply == Reply::Applied && self.frame.is_some() {
                        self.pending_frame = false;
                        self.next_frame = self.clock.saturating_add(90);
                        return Ok(());
                    }
                    let paused = reply == Reply::Unavailable;
                    if paused != self.paused {
                        self.paused = paused;
                        if self.client.bound() {
                            if paused {
                                let title = td_alloc::format(format_args!(
                                    "{} — desktop unavailable",
                                    self.title
                                ))
                                .map_err(diagnostic)?;
                                self.client.set_title(&title)?;
                            } else {
                                self.client.set_title(&self.title)?;
                            }
                        }
                    }
                    self.frame = match reply {
                        Reply::Frame { .. } => Some(bytes),
                        Reply::Unavailable => None,
                        _ => return Err(diagnostic("unexpected frame reply")),
                    };
                    if self.frame.is_none() {
                        self.shown = None;
                        self.motion_pending = false;
                        self.buttons = 0;
                        self.copy = None;
                        self.incoming = None;
                        self.paste = None;
                    }
                    self.pending_frame = false;
                    self.next_frame = self.clock.saturating_add(90);
                    self.dirty = true;
                } else {
                    self.controls_pending = self.controls_pending.saturating_sub(1);
                    match reply {
                        Reply::Applied => {}
                        Reply::Revision(revision) => {
                            if self.paste.is_some_and(|(gesture, source)| {
                                Some(gesture) == serial && Some(source) == self.client.selection()
                            }) && self.client.input().focused
                            {
                                self.paste = None;
                                match Incoming::begin_bounded(
                                    self.clock,
                                    td_desktop_protocol::MAX_TEXT,
                                ) {
                                    Ok((incoming, endpoint)) => {
                                        self.client.receive(&endpoint)?;
                                        self.incoming = Some((revision, incoming));
                                    }
                                    Err(error) => eprintln!("td-desktop: clipboard: {error}"),
                                }
                            }
                        }
                        Reply::Text(s)
                            if serial.is_some()
                                && serial == self.copy
                                && self.client.input().focused =>
                        {
                            let owned = OwnedText::copy(&self.budget, s)
                                .map_err(|_| diagnostic("clipboard budget exhausted"))?;
                            if let Some(serial) = self.copy.take() {
                                self.client.offer_selection(serial)?;
                                self.owned = Some(owned);
                            }
                        }
                        Reply::Text(_) | Reply::Unavailable => {}
                        Reply::StaleOutput => {
                            self.shown = None;
                            self.dirty = true;
                            self.next_frame = 0;
                        }
                        _ => return Err(diagnostic("unexpected desktop input reply")),
                    }
                }
            }
        }
        Ok(())
    }
}

impl App for Viewer {
    type Tag = Object;
    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }
    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }
    fn descriptor_wait(&mut self) {}
    fn tick(&mut self, now: u64) -> Result<()> {
        self.clock = now;
        Ok(())
    }
    fn event(&mut self, message: Message) -> Result<()> {
        let key = self.client.keyboard() == Some(message.object) && message.opcode == 3;
        match self.client.handle(&message, self.clock)? {
            Handled::Bound => {
                self.client.set_title(if self.link.is_some() {
                    &self.title
                } else {
                    "td-desktop — connecting"
                })?;
                self.client.set_app_id("td-desktop")?;
                self.client.commit()?;
            }
            Handled::Configure { size, serial } => {
                if let Some((w, h)) = size {
                    if w > 0 {
                        self.window.0 = usize::try_from(w).map_err(diagnostic)?;
                    }
                    if h > 0 {
                        self.window.1 = usize::try_from(h).map_err(diagnostic)?;
                    }
                    self.shown = None;
                    self.dirty = true;
                }
                self.client.acknowledge(serial)?;
            }
            Handled::CloseRequested => self.client.close(),
            Handled::Keyboard(KeyboardEvent::Focus(false)) | Handled::SeatRemoved => {
                self.release()?
            }
            Handled::Capabilities { keyboard, pointer } if !keyboard || !pointer => {
                self.release()?
            }
            Handled::Pointer(event) => self.pointer(event)?,
            Handled::Clipboard(ClipboardEvent::Send(fd)) if self.outgoing.is_none() => {
                if let Some(text) = &self.owned {
                    self.outgoing = Some(
                        Outgoing::begin_owned(
                            fd,
                            text.try_clone().map_err(diagnostic)?,
                            self.clock,
                        )
                        .map_err(diagnostic)?,
                    );
                }
            }
            Handled::Clipboard(ClipboardEvent::Cancelled | ClipboardEvent::Released) => {
                self.owned = None;
                self.outgoing = None;
            }
            Handled::Clipboard(ClipboardEvent::Selection) => {
                self.incoming = None;
                self.paste = None;
            }
            Handled::GlobalRemoved { required: true, .. } => {
                return Err(diagnostic("required Wayland global removed"))
            }
            _ => {}
        }
        if key {
            self.key(&message)?;
        }
        Ok(())
    }
    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        self.clock = now;
        loop {
            match self.updates.try_recv() {
                Ok(update) => self.update(update)?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(diagnostic("desktop transport ended"))
                }
            }
        }
        if self.motion_pending && self.controls_pending == 0 {
            self.motion_pending = false;
            if let Some(at) = self.position() {
                self.submit(
                    Request::Pointer {
                        at,
                        buttons: self.buttons,
                    },
                    None,
                )?;
            }
        }
        if self.link.is_some() && !self.pending_frame && now >= self.next_frame {
            self.submit(Request::Frame, None)?;
            self.pending_frame = true;
        }
        if let Some(out) = &mut self.outgoing {
            match out.step(now) {
                Ok(false) => {}
                Ok(true) => self.outgoing = None,
                Err(error) => {
                    self.outgoing = None;
                    eprintln!("td-desktop: clipboard: {error}");
                }
            }
        }
        if idle && self.client.input().focused {
            if let Some((revision, mut incoming)) = self.incoming.take() {
                match incoming.step(now) {
                    Ok(false) => self.incoming = Some((revision, incoming)),
                    Ok(true) => match incoming.finish() {
                        Ok(text) => self.submit(
                            Request::WriteText {
                                revision,
                                text: &text,
                            },
                            None,
                        )?,
                        Err(error) => eprintln!("td-desktop: clipboard: {error}"),
                    },
                    Err(error) => eprintln!("td-desktop: clipboard: {error}"),
                }
            }
        }
        self.client.connection().set_wait(Duration::from_millis(16));
        Ok(())
    }
    fn draw(&mut self) -> Result<()> {
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        let window = self.window;
        let mut next = None;
        let frame = self
            .frame
            .as_deref()
            .map(Reply::decode)
            .transpose()
            .map_err(diagnostic)?;
        let shown = self.client.present(window.0, window.1, &mut |bytes| {
            bytes.fill(0);
            if let Some(Reply::Frame {
                output,
                width,
                height,
                pixels,
            }) = frame
            {
                next = View::fit(output, width, height, window);
                if let Some(view) = next {
                    view.paint(bytes, window.0 * 4, pixels);
                }
            }
            Ok(())
        })?;
        if shown {
            self.shown = next;
            self.dirty = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn pointer_maps_to_the_pixel_shown_and_letterboxing_has_no_target() {
        let output = Output(std::num::NonZeroU64::MIN);
        for (width, height, window) in [
            (4, 3, (5, 9)),
            (3, 5, (10, 7)),
            (4, 4, (1, 1)),
            (2, 1, (4, 2)),
        ] {
            let view = View::fit(output, width, height, window).unwrap();
            let source = (1..=u32::from(width) * u32::from(height))
                .flat_map(u32::to_be_bytes)
                .collect::<Vec<_>>();
            let mut painted = vec![0; window.0 * window.1 * 4];
            view.paint(&mut painted, window.0 * 4, &source);
            for (i, pixel) in painted.as_chunks::<4>().0.iter().enumerate() {
                let at = view.at(i % window.0, i / window.0);
                let expected = at.map_or(0, |p| {
                    1 + u32::from(p.x) + u32::from(p.y) * u32::from(width)
                });
                assert_eq!(u32::from_be_bytes(*pixel), expected);
            }
        }
    }
}
