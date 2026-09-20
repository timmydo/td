//! The screen window: the `App` that presents a [`Screen`] and drives a
//! program's [`Handler`] with what the window receives — keypresses
//! translated to the screen's vocabulary, left clicks and wheel travel
//! mapped to cells, the grid laid out again on configure, focus and the
//! close request — and polls it each turn under the wait it asks for, so
//! a program whose work arrives on a channel from another thread is
//! served without a descriptor of its own in the loop. The window owns
//! the client and the pinned face; the handler owns everything else.

use crate::client::{App, Client, Handled, KeyboardEvent, Tag};
use crate::font::Font;
use crate::pointer::{self, Wheel};
use crate::raster::{Raster, Scale, Surface};
use crate::screen::{press, Input, Screen, Style};
use crate::wire::Message;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;

/// The extent a toplevel is laid out for until the compositor's first
/// configure names one: 100 by 30 cells.
pub const DEFAULT_WIDTH: usize = 800;
pub const DEFAULT_HEIGHT: usize = 480;

/// The left button, as `wl_pointer.button` names it.
const LEFT: u32 = 0x110;

/// What a handler answers: keep going, or close the window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    Continue,
    Quit,
}

/// The program behind a screen window. It names the toplevel, reads the
/// window's inputs, is polled each turn, and paints the screen when it
/// says it must.
pub trait Handler {
    fn app_id(&self) -> &str;
    /// The style a freshly laid-out grid is cleared to.
    fn ground(&self) -> Style;
    fn input(&mut self, input: Input) -> Flow;
    /// Each turn, with monotonic milliseconds since the loop began: read
    /// what arrived elsewhere, run timers.
    fn poll(&mut self, now: u64) -> Flow;
    /// How long the loop may wait before the next poll. The client's own
    /// wait, at most `wayland::IDLE_WAIT` and shorter under an armed
    /// repeat, bounds it from above, so a channel the handler reads is
    /// never left longer than that; the floor is one millisecond, and a
    /// handler that asks for zero is polled a thousand times a second.
    fn wait_ms(&self, now: u64) -> u64;
    /// Whether the screen must be painted again before the next present.
    fn needs_redraw(&self) -> bool;
    /// Paints the screen whole; the window presents it. A paint, not a
    /// frame: the window calls it again when the client had no buffer to
    /// present the last one in, so it must be repeatable.
    fn render(&mut self, screen: &mut Screen);
    /// The toplevel's title, read at binding and again after every
    /// render, so a handler may retitle the window from its state.
    fn title(&self) -> &str;
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
    screen: Screen,
    font: Font,
    clock: u64,
    size: (usize, usize),
    /// The pointer's last position in surface pixels.
    pointer: (i64, i64),
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
        let screen = Screen::new(surface, handler.ground()).map_err(|why| why.to_string())?;
        Ok(Self {
            client: Client::new(stream, temporary)?,
            handler,
            screen,
            font: crate::font::pinned()?,
            clock: 0,
            size: (DEFAULT_WIDTH, DEFAULT_HEIGHT),
            pointer: (0, 0),
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
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    fn deliver(&mut self, input: Input) {
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

    fn grid(&self) -> Input {
        Input::Resize {
            rows: self.screen.rows(),
            columns: self.screen.columns(),
        }
    }

    /// Lays the grid out for the configured extent, a zero axis keeping
    /// the current one; an extent the raster refuses keeps the last grid,
    /// is reported to the handler and delivers nothing. A configure that
    /// names the extent the grid already has, which compositors send for
    /// activation and tiling changes, keeps the cells and tells the
    /// handler nothing.
    fn resize(&mut self, width: i32, height: i32) {
        if width > 0 {
            self.size.0 = width as usize;
        }
        if height > 0 {
            self.size.1 = height as usize;
        }
        let surface = self.screen.surface();
        if self.size == (surface.width, surface.height) {
            return;
        }
        let ground = self.handler.ground();
        match Surface::new(self.size.0, self.size.1, Scale::default())
            .and_then(|surface| self.screen.resize(surface, ground))
        {
            Ok(()) => {}
            Err(why) => {
                // The grid is as it was, so the handler hears of the
                // refusal and of no new extent.
                let surface = self.screen.surface();
                self.size = (surface.width, surface.height);
                self.handler
                    .notice(&format!("window extent refused: {why}"));
                return;
            }
        }
        self.dirty = true;
        let grid = self.grid();
        self.deliver(grid);
    }

    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Key { key, stroke, .. } => match press(&stroke.chord) {
                Some(press) => {
                    self.deliver(Input::Key(press));
                    if stroke.repeat && !self.client.closed() {
                        self.client.arm(key, self.clock);
                    }
                }
                // Defensive: every chord the keymap can emit is a key, so
                // this reports a vocabulary drift, not a live keyboard.
                None => self
                    .handler
                    .notice(&format!("keyboard: chord {:?} is not a key", stroke.chord)),
            },
            KeyboardEvent::Focus(focused) => self.focus(focused),
            KeyboardEvent::Keymap(Err(why)) | KeyboardEvent::Refused(why) => {
                self.handler.notice(&format!("keyboard: {why}"));
            }
            KeyboardEvent::Keymap(Ok(())) | KeyboardEvent::Ready => {}
        }
    }

    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        use pointer::Event as P;
        match event {
            // Coordinates arrive in 24.8 fixed point.
            P::Enter { x, y, .. } | P::Motion(x, y) => {
                self.pointer = (i64::from(x).div_euclid(256), i64::from(y).div_euclid(256));
            }
            P::Leave(_) => self.wheel = Wheel::default(),
            P::Button {
                button: LEFT,
                pressed: true,
                ..
            } if self.client.entered().is_some() => {
                let (x, y) = self.pointer;
                if let Some((row, column)) = self.screen.hit(x, y) {
                    self.deliver(Input::Click { row, column });
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
                // The grid the window was laid out for, so the handler has
                // seen an extent before it is first asked to render, even
                // when the compositor's configure names none.
                let grid = self.grid();
                self.deliver(grid);
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
                }
                if !keyboard {
                    self.focus(false);
                }
            }
            Handled::SeatRemoved => {
                self.wheel = Wheel::default();
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
                match press(&stroke.chord) {
                    Some(press) => self.deliver(Input::Key(press)),
                    None => self.client.cancel_repeat(),
                }
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
        self.handler.render(&mut self.screen);
        if self.handler.title() != self.title {
            self.title = self.handler.title().to_string();
            self.client.set_title(&self.title)?;
        }
        let surface = self.screen.surface();
        let Window {
            client,
            screen,
            font,
            ..
        } = self;
        let presented = client.present(surface.width, surface.height, &mut |pixels| {
            let mut raster = Raster::new(pixels, font, surface, surface.width * 4)
                .map_err(|why| why.to_string())?;
            raster
                .paint(screen, surface.bounds())
                .map_err(|why| why.to_string())
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
