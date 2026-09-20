//! The widget window: the `App` that presents what a program paints into
//! a raster over the surface and drives the program's [`Handler`] with
//! what the window receives — keypresses as the keymap's chords, the left
//! button's press, drag and release in surface pixels with Shift, wheel
//! travel in rows and columns, the surface laid out again on configure,
//! focus, the close request and the text of a paste it asked for — and
//! polls it each turn under the wait it asks for, so a program whose work
//! arrives on a channel from another thread is served without a
//! descriptor of its own in the loop. The window owns the client, the
//! pinned face, the pointer and the clipboard's transfers; the handler
//! owns everything else, its widgets and their compositions included,
//! and copies and pastes through the [`Clipboard`] handed it with each
//! input.

use crate::client::{App, Client, ClipboardEvent, Handled, KeyboardEvent, Tag};
use crate::clipboard::{Incoming, Outgoing, MAX_BYTES};
pub use crate::driven::PointerPhase;
use crate::font::Font;
use crate::pointer::{self, Wheel};
use crate::raster::{Raster, Scale, Surface};
use crate::wire::Message;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;

/// The extent a toplevel is laid out for until the compositor's first
/// configure names one.
pub const DEFAULT_WIDTH: usize = 800;
pub const DEFAULT_HEIGHT: usize = 600;

/// The left button, as `wl_pointer.button` names it.
const LEFT: u32 = 0x110;

/// The longest a turn waits while a clipboard transfer is pending: its
/// endpoint is not in the loop's wait, so the transfer is stepped at
/// this pace.
const TRANSFER_WAIT: u64 = 10;

/// What a handler answers: keep going, or close the window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    Continue,
    Quit,
}

/// What the window hands the handler. Pointer positions are the
/// surface's physical pixels, signed so travel past an edge is
/// representable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input<'a> {
    /// A press as the keymap spells it (`a`, `C-x`, `S-Right`), `repeat`
    /// for a delivery the repeat clock made.
    Key {
        chord: &'a str,
        repeat: bool,
    },
    /// The left button; `extend` is Shift held at the press, read from the
    /// keyboard's synchronized modifier state while the window has focus,
    /// and false on a move or a release.
    Pointer {
        phase: PointerPhase,
        x: i64,
        y: i64,
        extend: bool,
    },
    /// The pointer left, or the device went, while the button was held:
    /// the handler ends its drag without a release.
    CancelPointer,
    /// Wheel travel per frame in cell rows and columns.
    Wheel {
        rows: isize,
        columns: isize,
    },
    /// The surface the handler paints and lays out for.
    Resize(Surface),
    Focus(bool),
    /// The compositor asks the window to close: `Flow::Quit` closes it,
    /// and `Flow::Continue` keeps it, for a handler with something to
    /// ask about first; the request comes again when it is repeated.
    Close,
    /// The selection's text, whole, that `Clipboard::paste` asked for.
    Paste(&'a str),
}

/// Why the clipboard refused a copy or a paste; `Display` says it for a
/// status line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The seat has no data device: the compositor offers no clipboard.
    NoDevice,
    /// The window has no keyboard focus, which a selection follows.
    NoFocus,
    /// The request is not answering a key or button press of the user's,
    /// whose serial a selection is set at.
    NoSerial,
    /// The text offered last is still being sent.
    Sending,
    /// A paste is still arriving.
    Pasting,
    /// The selection offers no text.
    NoSelection,
    /// The text is past `clipboard::MAX_BYTES`.
    TooLong,
    /// No private endpoint for the paste could be made: the process is
    /// at its descriptor limit.
    NoEndpoint,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoDevice => "the compositor offers no clipboard",
            Self::NoFocus => "the window has no keyboard focus",
            Self::NoSerial => "the clipboard answers a key or button press only",
            Self::Sending => "the last copy is still being sent",
            Self::Pasting => "a paste is still arriving",
            Self::NoSelection => "the clipboard holds no text",
            Self::TooLong => "the text is too long for the clipboard",
            Self::NoEndpoint => "no endpoint for the paste could be made",
        })
    }
}

/// The clipboard as a handler reaches it with each input: a copy offers
/// text as the seat's selection, a paste asks the selection for its
/// text, which arrives as `Input::Paste` once it is whole. The window's
/// is `WindowClipboard`; a test or a headless run hands `NoClipboard`,
/// which refuses everything.
pub trait Clipboard {
    /// Whether the seat has a data device.
    fn available(&self) -> bool;
    /// Whether the selection offers text a paste could ask for.
    fn has_text(&self) -> bool;
    /// Whether a paste is arriving.
    fn pasting(&self) -> bool;
    /// Offers `text` as the seat's selection at the serial of the press
    /// being delivered, the window focused; a paste in flight is dropped.
    /// The text is kept for the sends that follow until the compositor
    /// cancels the source or another copy replaces it.
    fn copy(&mut self, text: Arc<str>) -> std::result::Result<(), Refusal>;
    /// Asks the selection for its text over a fresh private endpoint, the
    /// window focused; it arrives as `Input::Paste` when whole, or a
    /// notice says why not.
    fn paste(&mut self) -> std::result::Result<(), Refusal>;
}

/// No clipboard: every request is refused as `Refusal::NoDevice`.
pub struct NoClipboard;

impl Clipboard for NoClipboard {
    fn available(&self) -> bool {
        false
    }
    fn has_text(&self) -> bool {
        false
    }
    fn pasting(&self) -> bool {
        false
    }
    fn copy(&mut self, _: Arc<str>) -> std::result::Result<(), Refusal> {
        Err(Refusal::NoDevice)
    }
    fn paste(&mut self) -> std::result::Result<(), Refusal> {
        Err(Refusal::NoDevice)
    }
}

/// The window's half of the clipboard, over its client and its transfers.
pub struct WindowClipboard<'w> {
    client: &'w mut Client<Object>,
    board: &'w mut Board,
    clock: u64,
}

impl Clipboard for WindowClipboard<'_> {
    fn available(&self) -> bool {
        self.client.clipboard()
    }

    fn has_text(&self) -> bool {
        self.client.selection_mime().is_some()
    }

    fn pasting(&self) -> bool {
        self.board.incoming.is_some()
    }

    fn copy(&mut self, text: Arc<str>) -> std::result::Result<(), Refusal> {
        if self.board.error.is_some() {
            return Err(Refusal::NoDevice);
        }
        if !self.client.clipboard() {
            return Err(Refusal::NoDevice);
        }
        if !self.client.input().focused {
            return Err(Refusal::NoFocus);
        }
        let serial = self.board.serial.ok_or(Refusal::NoSerial)?;
        if self.board.outgoing.is_some() {
            return Err(Refusal::Sending);
        }
        if text.len() > MAX_BYTES {
            return Err(Refusal::TooLong);
        }
        // A request the connection refused ends the loop once the input
        // is handled, as the editor's window ends on it; nothing is kept
        // behind a source that may not be live.
        if let Err(e) = self.client.offer_selection(serial) {
            self.board.text = None;
            self.board.error = Some(e);
            return Err(Refusal::NoDevice);
        }
        self.board.text = Some(text);
        if self.board.incoming.take().is_some() {
            self.board.notice = Some("paste cancelled: a copy replaced the selection".into());
        }
        Ok(())
    }

    fn paste(&mut self) -> std::result::Result<(), Refusal> {
        if self.board.error.is_some() {
            return Err(Refusal::NoDevice);
        }
        if !self.client.clipboard() {
            return Err(Refusal::NoDevice);
        }
        if !self.client.input().focused {
            return Err(Refusal::NoFocus);
        }
        if self.board.incoming.is_some() {
            return Err(Refusal::Pasting);
        }
        if self.client.selection_mime().is_none() {
            return Err(Refusal::NoSelection);
        }
        // An endpoint the process cannot make is the paste's refusal,
        // not the connection's failure: the window goes on, as the
        // editor's does.
        let (incoming, peer) = match Incoming::begin(self.clock) {
            Ok(pair) => pair,
            Err(e) => {
                self.board.notice = Some(format!("paste refused: clipboard endpoint: {e}"));
                return Err(Refusal::NoEndpoint);
            }
        };
        if let Err(e) = self.client.receive(&peer) {
            self.board.error = Some(e);
            return Err(Refusal::NoDevice);
        }
        drop(peer);
        self.board.incoming = Some(incoming);
        Ok(())
    }
}

/// The transfers and the text behind the live source, the window's.
#[derive(Default)]
struct Board {
    text: Option<Arc<str>>,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    /// The serial of the press being delivered, for a copy it asks.
    serial: Option<u32>,
    /// A notice a request raised, given to the handler once the input
    /// it came from is handled.
    notice: Option<String>,
    /// A connection error a request met, which ends the loop once the
    /// input it came from is handled.
    error: Option<String>,
}

/// The program behind a widget window. It names the toplevel, reads the
/// window's inputs, is polled each turn, and paints the surface when it
/// says it must.
pub trait Handler {
    fn app_id(&self) -> &str;
    /// The toplevel's title, read at binding and again before every
    /// present, so a handler may retitle the window from its state.
    fn title(&self) -> &str;
    /// An input, with the clipboard to copy to or paste from while it
    /// is handled.
    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow;
    /// Each turn, with monotonic milliseconds since the loop began: read
    /// what arrived elsewhere, run timers.
    fn poll(&mut self, now: u64) -> Flow;
    /// How long the loop may wait before the next poll. The client's own
    /// wait, at most `wayland::IDLE_WAIT` and shorter under an armed
    /// repeat or a pending clipboard transfer, bounds it from above; the
    /// floor is one millisecond.
    fn wait_ms(&self, now: u64) -> u64;
    /// Whether the surface must be painted again before the next present.
    fn needs_redraw(&self) -> bool;
    /// Paints the surface whole into a raster over it; the window presents
    /// it. A paint, not a frame: the window calls it again when the client
    /// had no buffer to present the last one in, so it must be repeatable.
    /// A paint that fails ends the loop with its error: a program that
    /// cannot paint its surface has nothing to show.
    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<()>;
    /// A diagnostic from the window: a refused keymap or press, an extent
    /// the raster refused, a clipboard transfer that failed or was
    /// cancelled.
    fn notice(&mut self, _message: &str) {}
}

/// The window owns no Wayland objects of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Object {}

impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}

pub struct Window<'h, H: Handler> {
    client: Client<Object>,
    handler: &'h mut H,
    font: Font,
    clock: u64,
    surface: Surface,
    /// The configured extent; a refused one is put back to the surface's.
    size: (usize, usize),
    /// The pointer's last position in surface pixels.
    pointer: (i64, i64),
    /// Whether the left button is held: motion is a drag until release.
    held: bool,
    wheel: Wheel,
    dirty: bool,
    /// The title the toplevel was last given, re-read from the handler
    /// before each present.
    title: String,
    /// Whether the handler was last told it has keyboard focus.
    focused: bool,
    board: Board,
}

impl<'h, H: Handler> Window<'h, H> {
    pub fn new(handler: &'h mut H, stream: UnixStream, temporary: PathBuf) -> Result<Self> {
        let surface = Surface::new(DEFAULT_WIDTH, DEFAULT_HEIGHT, Scale::default())
            .map_err(|why| why.to_string())?;
        Ok(Self {
            client: Client::new(stream, temporary)?,
            handler,
            font: crate::font::pinned()?,
            clock: 0,
            surface,
            size: (DEFAULT_WIDTH, DEFAULT_HEIGHT),
            pointer: (0, 0),
            held: false,
            wheel: Wheel::default(),
            dirty: true,
            title: String::new(),
            focused: false,
            board: Board::default(),
        })
    }

    pub fn handler(&self) -> &H {
        self.handler
    }
    pub fn handler_mut(&mut self) -> &mut H {
        self.handler
    }
    pub fn surface(&self) -> Surface {
        self.surface
    }

    /// Hands the handler an input, unless the window is closed: a handler
    /// that quit hears nothing more, not even the resize or focus loss
    /// that followed the input it quit on. A clipboard request the
    /// connection refused is the loop's error once the input is handled.
    fn deliver(&mut self, input: Input<'_>) -> Result<()> {
        if self.client.closed() {
            return Ok(());
        }
        let Window {
            client,
            handler,
            board,
            clock,
            ..
        } = self;
        let mut clipboard = WindowClipboard {
            client,
            board,
            clock: *clock,
        };
        let flow = handler.input(input, &mut clipboard);
        if let Some(notice) = self.board.notice.take() {
            self.handler.notice(&notice);
        }
        if let Some(error) = self.board.error.take() {
            return Err(error);
        }
        if flow == Flow::Quit {
            self.client.close();
        }
        Ok(())
    }

    /// Hands the handler an input answering a press of the user's, whose
    /// serial a copy it asks for is made at.
    fn deliver_at(&mut self, serial: u32, input: Input<'_>) -> Result<()> {
        self.board.serial = Some(serial);
        let delivered = self.deliver(input);
        self.board.serial = None;
        delivered
    }

    /// Keyboard focus as the handler is told it: a change only, so a
    /// keyboard that was never there is not a loss. A paste in flight
    /// follows the focus the selection did.
    fn focus(&mut self, focused: bool) -> Result<()> {
        if self.focused == focused {
            return Ok(());
        }
        self.focused = focused;
        if !focused && self.board.incoming.take().is_some() {
            self.handler.notice("paste cancelled: focus lost");
        }
        self.deliver(Input::Focus(focused))
    }

    /// Ends a held button without a release.
    fn cancel_pointer(&mut self) -> Result<()> {
        if self.held {
            self.held = false;
            self.deliver(Input::CancelPointer)?;
        }
        Ok(())
    }

    /// Lays the surface out for the configured extent, a zero axis keeping
    /// the current one; an extent the raster refuses keeps the last
    /// surface, is reported to the handler and delivers nothing. A
    /// configure that names the extent the surface already has, which
    /// compositors send for activation and tiling changes, tells the
    /// handler nothing.
    fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        if width > 0 {
            self.size.0 = width as usize;
        }
        if height > 0 {
            self.size.1 = height as usize;
        }
        if self.size == (self.surface.width, self.surface.height) {
            return Ok(());
        }
        match Surface::new(self.size.0, self.size.1, Scale::default()) {
            Ok(surface) => self.surface = surface,
            Err(why) => {
                // The surface is as it was, so the handler hears of the
                // refusal and of no new extent.
                self.size = (self.surface.width, self.surface.height);
                self.handler
                    .notice(&format!("window extent refused: {why}"));
                return Ok(());
            }
        }
        self.dirty = true;
        self.cancel_pointer()?;
        let surface = self.surface;
        self.deliver(Input::Resize(surface))
    }

    fn keyboard(&mut self, event: KeyboardEvent) -> Result<()> {
        match event {
            KeyboardEvent::Key {
                serial,
                key,
                stroke,
            } => {
                self.deliver_at(
                    serial,
                    Input::Key {
                        chord: &stroke.chord,
                        repeat: false,
                    },
                )?;
                if stroke.repeat && !self.client.closed() {
                    self.client.arm(key, self.clock);
                }
            }
            KeyboardEvent::Focus(focused) => self.focus(focused)?,
            KeyboardEvent::Keymap(Err(why)) | KeyboardEvent::Refused(why) => {
                self.handler.notice(&format!("keyboard: {why}"));
            }
            // The widget window has no hint layer yet: held roles reach a
            // handler only under a key.
            KeyboardEvent::Keymap(Ok(())) | KeyboardEvent::Ready | KeyboardEvent::Held(_) => {}
        }
        Ok(())
    }

    /// Shift as the keyboard's synchronized state reports it while the
    /// window has focus; a keyboard the window lacks extends nothing.
    fn extend(&self) -> bool {
        let input = self.client.input();
        input.focused
            && input.synchronized
            && input
                .map
                .as_ref()
                .is_some_and(|map| map.pointer_extend(input.modifiers))
    }

    /// The left button's press, at its serial, or its release, at none: a
    /// selection is set at a press.
    fn button(&mut self, serial: u32, phase: PointerPhase) -> Result<()> {
        let (x, y) = self.pointer;
        let extend = phase == PointerPhase::Press && self.extend();
        let input = Input::Pointer {
            phase,
            x,
            y,
            extend,
        };
        if phase == PointerPhase::Press {
            self.deliver_at(serial, input)
        } else {
            self.deliver(input)
        }
    }

    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        use pointer::Event as P;
        match event {
            // Coordinates arrive in 24.8 fixed point.
            P::Enter { x, y, .. } | P::Motion(x, y) => {
                self.pointer = (i64::from(x).div_euclid(256), i64::from(y).div_euclid(256));
                if self.held {
                    let (x, y) = self.pointer;
                    self.deliver(Input::Pointer {
                        phase: PointerPhase::Move,
                        x,
                        y,
                        extend: false,
                    })?;
                }
            }
            P::Leave(_) => {
                self.wheel = Wheel::default();
                self.cancel_pointer()?;
            }
            P::Button {
                serial,
                button: LEFT,
                pressed,
            } if self.client.entered().is_some() => {
                if pressed && !self.held {
                    self.held = true;
                    self.button(serial, PointerPhase::Press)?;
                } else if !pressed && self.held {
                    self.held = false;
                    self.button(serial, PointerPhase::Release)?;
                }
            }
            P::Axis(..) | P::Source(_) | P::Stop(_) | P::Discrete(..)
                if self.client.entered().is_some() =>
            {
                self.wheel.update(event)?;
            }
            P::Frame => {
                let (rows, columns) = self.wheel.frame();
                if rows != 0 || columns != 0 {
                    self.deliver(Input::Wheel { rows, columns })?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The client's clipboard outcomes: the window's half of each.
    fn clipboard(&mut self, event: ClipboardEvent) {
        match event {
            ClipboardEvent::Selection => {
                if self.board.incoming.take().is_some() {
                    self.handler
                        .notice("paste cancelled: the selection changed");
                }
            }
            ClipboardEvent::Send(right) => {
                if self.board.outgoing.is_some() {
                    // A busy send drops exactly its right.
                    drop(right);
                    return;
                }
                // The live source and the text are set together by `copy`
                // and cleared together on cancel and release.
                let Some(text) = self.board.text.clone() else {
                    self.handler
                        .notice("clipboard send: no text behind the source");
                    return;
                };
                match Outgoing::begin(right, text, self.clock) {
                    Ok(transfer) => self.board.outgoing = Some(transfer),
                    Err(e) => self.handler.notice(&format!("clipboard send refused: {e}")),
                }
            }
            // A send already begun keeps its own copy of the text.
            ClipboardEvent::Cancelled => self.board.text = None,
            ClipboardEvent::Released => self.release_clipboard(),
        }
    }

    /// The data device went with its seat or its manager: the transfers
    /// end and the text behind the source is dropped.
    fn release_clipboard(&mut self) {
        if self.board.incoming.take().is_some() {
            self.handler
                .notice("paste cancelled: the clipboard went away");
        }
        if let Some(transfer) = self.board.outgoing.take() {
            if let Err(e) = transfer.cancel() {
                self.handler
                    .notice(&format!("clipboard send cancelled: {e}"));
            }
        }
        self.board.text = None;
    }

    /// Steps the pending transfers at the turn's clock: a paste that is
    /// whole reaches the handler, a send that is done is over, and a
    /// failure of either is a notice. Only an idle turn, its queue
    /// drained, admits a paste, so a focus loss, a selection change or
    /// a close still queued is seen first; an expired transfer ends
    /// whatever the turn.
    fn transfers(&mut self, now: u64, idle: bool) -> Result<()> {
        let due = |expired: bool| idle || expired;
        if self
            .board
            .incoming
            .as_ref()
            .is_some_and(|incoming| due(incoming.expired(now)))
        {
            if let Some(mut incoming) = self.board.incoming.take() {
                match incoming.step(now) {
                    Ok(false) => self.board.incoming = Some(incoming),
                    Ok(true) => match incoming.finish() {
                        Ok(text) if self.focused => self.deliver(Input::Paste(&text))?,
                        Ok(_) => self.handler.notice("paste cancelled: focus lost"),
                        Err(e) => self.handler.notice(&format!("paste failed: {e}")),
                    },
                    Err(e) => self.handler.notice(&format!("paste failed: {e}")),
                }
            }
        }
        if self
            .board
            .outgoing
            .as_ref()
            .is_some_and(|outgoing| due(outgoing.expired(now)))
        {
            if let Some(mut outgoing) = self.board.outgoing.take() {
                match outgoing.step(now) {
                    Ok(false) => self.board.outgoing = Some(outgoing),
                    Ok(true) => {}
                    Err(e) => self.handler.notice(&format!("clipboard send failed: {e}")),
                }
            }
        }
        Ok(())
    }
}

impl<H: Handler> App for Window<'_, H> {
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
        match self.client.handle(&message, self.clock)? {
            Handled::Bound => {
                self.title = self.handler.title().to_string();
                self.client.set_title(&self.title)?;
                self.client.set_app_id(self.handler.app_id())?;
                self.client.commit()?;
                // The surface the window was laid out for, so the handler
                // has seen an extent before it is first asked to paint,
                // even when the compositor's configure names none.
                let surface = self.surface;
                self.deliver(Input::Resize(surface))?;
            }
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    self.resize(width, height)?;
                }
                self.client.acknowledge(serial)?;
            }
            // A paste in flight is dropped with the request, as the
            // editor drops its own: a handler that keeps the window
            // asks again.
            Handled::CloseRequested => {
                if self.board.incoming.take().is_some() {
                    self.handler.notice("paste cancelled: close requested");
                }
                self.deliver(Input::Close)?;
            }
            Handled::Keyboard(event) => self.keyboard(event)?,
            Handled::Pointer(event) => self.pointer(event)?,
            Handled::Clipboard(event) => self.clipboard(event),
            Handled::Capabilities { keyboard, pointer } => {
                if !pointer {
                    self.wheel = Wheel::default();
                    self.cancel_pointer()?;
                }
                if !keyboard {
                    self.focus(false)?;
                }
            }
            // The client released the data device with the seat.
            Handled::SeatRemoved => {
                self.wheel = Wheel::default();
                self.cancel_pointer()?;
                self.release_clipboard();
                self.focus(false)?;
            }
            Handled::GlobalRemoved { required: true, .. } => {
                return Err("required Wayland global was removed".into())
            }
            Handled::Unhandled => {
                return Err(format!(
                    "unexpected Wayland event {}:{}",
                    message.object, message.opcode
                ))
            }
            Handled::Done | Handled::FrameDone | Handled::GlobalRemoved { .. } => {}
        }
        Ok(())
    }

    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        self.clock = now;
        self.transfers(now, idle)?;
        if idle && !self.client.closed() {
            if let Some(stroke) = self.client.repeat(now)? {
                self.deliver(Input::Key {
                    chord: &stroke.chord,
                    repeat: true,
                })?;
            }
        }
        if !self.client.closed() && self.handler.poll(now) == Flow::Quit {
            self.client.close();
        }
        let mut wait = self.client.wait_ms(now).min(self.handler.wait_ms(now));
        if self.board.incoming.is_some() || self.board.outgoing.is_some() {
            wait = wait.min(TRANSFER_WAIT);
        }
        self.client
            .connection()
            .set_wait(Duration::from_millis(wait.max(1)));
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if self.handler.needs_redraw() {
            self.dirty = true;
        }
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        // The title goes out before the frame's commit; a retitle made
        // while painting rides the next frame.
        if self.handler.title() != self.title {
            self.title = self.handler.title().to_string();
            self.client.set_title(&self.title)?;
        }
        // The handler paints only into a buffer: a frame refused for
        // want of one asks for no paint and leaves the window dirty.
        let surface = self.surface;
        let Window {
            client,
            handler,
            font,
            ..
        } = self;
        let presented = client.present(surface.width, surface.height, &mut |pixels| {
            let mut raster = Raster::new(pixels, font, surface, surface.width * 4)
                .map_err(|why| why.to_string())?;
            handler.paint(&mut raster, surface)
        })?;
        if presented {
            self.dirty = false;
        }
        Ok(())
    }
}

/// Runs `handler` in a window over `stream`, its pool files under
/// `temporary`, until the window closes; the handler stays the caller's
/// whichever way the loop ends.
pub fn run<H: Handler>(handler: &mut H, stream: UnixStream, temporary: PathBuf) -> Result<()> {
    let mut window = Window::new(handler, stream, temporary)?;
    crate::client::run(&mut window)
}
