//! The widget window: the `App` that presents what a program paints into
//! a raster over the surface and drives the program's [`Handler`] with
//! what the window receives — keypresses as the keymap's chords, the left
//! button's press, drag and release in surface pixels with Shift, wheel
//! travel in rows and columns, the surface laid out again on configure,
//! focus and the close request — and polls it each turn under the wait it
//! asks for, so a program whose work arrives on a channel from another
//! thread is served without a descriptor of its own in the loop. The
//! window owns the client, the pinned face and the pointer; the handler
//! owns everything else, its widgets and their compositions included.

use crate::client::{App, Client, Handled, KeyboardEvent, Tag};
pub use crate::driven::PointerPhase;
use crate::font::Font;
use crate::pointer::{self, Wheel};
use crate::raster::{Raster, Scale, Surface};
use crate::wire::Message;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;

/// The extent a toplevel is laid out for until the compositor's first
/// configure names one.
pub const DEFAULT_WIDTH: usize = 800;
pub const DEFAULT_HEIGHT: usize = 600;

/// The left button, as `wl_pointer.button` names it.
const LEFT: u32 = 0x110;

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
    Close,
}

/// The program behind a widget window. It names the toplevel, reads the
/// window's inputs, is polled each turn, and paints the surface when it
/// says it must.
pub trait Handler {
    fn app_id(&self) -> &str;
    /// The toplevel's title, read at binding and again before every
    /// present, so a handler may retitle the window from its state.
    fn title(&self) -> &str;
    fn input(&mut self, input: Input<'_>) -> Flow;
    /// Each turn, with monotonic milliseconds since the loop began: read
    /// what arrived elsewhere, run timers.
    fn poll(&mut self, now: u64) -> Flow;
    /// How long the loop may wait before the next poll. The client's own
    /// wait, at most `wayland::IDLE_WAIT` and shorter under an armed
    /// repeat, bounds it from above; the floor is one millisecond.
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
    /// the raster refused.
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
    /// that followed the input it quit on.
    fn deliver(&mut self, input: Input<'_>) {
        if self.client.closed() {
            return;
        }
        if self.handler.input(input) == Flow::Quit {
            self.client.close();
        }
    }

    /// Keyboard focus as the handler is told it: a change only, so a
    /// keyboard that was never there is not a loss.
    fn focus(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.deliver(Input::Focus(focused));
    }

    /// Ends a held button without a release.
    fn cancel_pointer(&mut self) {
        if self.held {
            self.held = false;
            self.deliver(Input::CancelPointer);
        }
    }

    /// Lays the surface out for the configured extent, a zero axis keeping
    /// the current one; an extent the raster refuses keeps the last
    /// surface, is reported to the handler and delivers nothing. A
    /// configure that names the extent the surface already has, which
    /// compositors send for activation and tiling changes, tells the
    /// handler nothing.
    fn resize(&mut self, width: i32, height: i32) {
        if width > 0 {
            self.size.0 = width as usize;
        }
        if height > 0 {
            self.size.1 = height as usize;
        }
        if self.size == (self.surface.width, self.surface.height) {
            return;
        }
        match Surface::new(self.size.0, self.size.1, Scale::default()) {
            Ok(surface) => self.surface = surface,
            Err(why) => {
                // The surface is as it was, so the handler hears of the
                // refusal and of no new extent.
                self.size = (self.surface.width, self.surface.height);
                self.handler
                    .notice(&format!("window extent refused: {why}"));
                return;
            }
        }
        self.dirty = true;
        self.cancel_pointer();
        let surface = self.surface;
        self.deliver(Input::Resize(surface));
    }

    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Key { key, stroke, .. } => {
                self.deliver(Input::Key {
                    chord: &stroke.chord,
                    repeat: false,
                });
                if stroke.repeat && !self.client.closed() {
                    self.client.arm(key, self.clock);
                }
            }
            KeyboardEvent::Focus(focused) => self.focus(focused),
            KeyboardEvent::Keymap(Err(why)) | KeyboardEvent::Refused(why) => {
                self.handler.notice(&format!("keyboard: {why}"));
            }
            KeyboardEvent::Keymap(Ok(())) | KeyboardEvent::Ready => {}
        }
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

    fn button(&mut self, phase: PointerPhase) {
        let (x, y) = self.pointer;
        let extend = phase == PointerPhase::Press && self.extend();
        self.deliver(Input::Pointer {
            phase,
            x,
            y,
            extend,
        });
    }

    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        use pointer::Event as P;
        match event {
            // Coordinates arrive in 24.8 fixed point.
            P::Enter { x, y, .. } | P::Motion(x, y) => {
                self.pointer = (i64::from(x).div_euclid(256), i64::from(y).div_euclid(256));
                if self.held {
                    self.button(PointerPhase::Move);
                }
            }
            P::Leave(_) => {
                self.wheel = Wheel::default();
                self.cancel_pointer();
            }
            P::Button {
                button: LEFT,
                pressed,
                ..
            } if self.client.entered().is_some() => {
                if pressed && !self.held {
                    self.held = true;
                    self.button(PointerPhase::Press);
                } else if !pressed && self.held {
                    self.held = false;
                    self.button(PointerPhase::Release);
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
                    self.deliver(Input::Wheel { rows, columns });
                }
            }
            _ => {}
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
                self.deliver(Input::Resize(surface));
            }
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    self.resize(width, height);
                }
                self.client.acknowledge(serial)?;
            }
            Handled::CloseRequested => {
                self.deliver(Input::Close);
                self.client.close();
            }
            Handled::Keyboard(event) => self.keyboard(event),
            Handled::Pointer(event) => self.pointer(event)?,
            Handled::Capabilities { keyboard, pointer } => {
                if !pointer {
                    self.wheel = Wheel::default();
                    self.cancel_pointer();
                }
                if !keyboard {
                    self.focus(false);
                }
            }
            Handled::SeatRemoved => {
                self.wheel = Wheel::default();
                self.cancel_pointer();
                self.focus(false);
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
            Handled::Done
            | Handled::FrameDone
            | Handled::GlobalRemoved { .. }
            | Handled::Clipboard(_) => {}
        }
        Ok(())
    }

    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        self.clock = now;
        if idle && !self.client.closed() {
            if let Some(stroke) = self.client.repeat(now)? {
                self.deliver(Input::Key {
                    chord: &stroke.chord,
                    repeat: true,
                });
            }
        }
        if !self.client.closed() && self.handler.poll(now) == Flow::Quit {
            self.client.close();
        }
        let wait = self
            .client
            .wait_ms(now)
            .min(self.handler.wait_ms(now))
            .max(1);
        self.client
            .connection()
            .set_wait(Duration::from_millis(wait));
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
