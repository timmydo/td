//! The installer window: a `td_ui` client that presents the wizard's pages
//! over a Wayland toplevel. It moves between welcome and the unavailable
//! destination state while the installer service is absent. It owns no
//! Wayland objects of its own, so its `Tag` is
//! the empty `Object`; the seat and its devices are the client's.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use td_ui::client::{run, App, Client, Handled, KeyboardEvent, Tag};
use td_ui::font::Font;
use td_ui::raster::{Draw, Primitive, Raster, Scale, Surface, CHROME};
use td_ui::wayland::{connect, endpoint};
use td_ui::wire::Message;

use crate::destination::DestinationPage;
use crate::welcome::Welcome;

type Result<T> = std::result::Result<T, String>;

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

/// The default toplevel extent, used until the compositor configures one.
const DEFAULT_SIZE: (usize, usize) = (800, 600);

/// The installer owns no Wayland objects of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {}

impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}

struct Window {
    client: Client<Object>,
    font: Font,
    /// The outline face the live window draws its text in.
    typeface: Option<td_ui::typeface::Typeface>,
    size: (usize, usize),
    dirty: bool,
    page: Page,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Welcome,
    DestinationUnavailable,
}

impl Page {
    fn key(self, chord: &str) -> Self {
        match (self, chord) {
            (Self::Welcome, "Return" | "KP_Enter") => Self::DestinationUnavailable,
            (Self::DestinationUnavailable, "Escape") => Self::Welcome,
            _ => self,
        }
    }
}

fn keyboard_page(page: Page, event: KeyboardEvent) -> Result<Page> {
    match event {
        KeyboardEvent::Key { stroke, .. } => Ok(page.key(&stroke.chord)),
        KeyboardEvent::Keymap(Err(detail)) => Err(format!("keyboard keymap: {detail}")),
        _ => Ok(page),
    }
}

impl Window {
    fn new(stream: UnixStream, temporary: PathBuf) -> Result<Self> {
        Ok(Self {
            client: Client::new(stream, temporary)?,
            font: td_ui::font::pinned()?,
            typeface: None,
            size: DEFAULT_SIZE,
            dirty: true,
            page: Page::Welcome,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        self.client.set_title("Install td")?;
        self.client.set_app_id("td-setup")?;
        self.client.commit()
    }

    fn event(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, 0)? {
            Handled::Done | Handled::FrameDone => Ok(()),
            Handled::Bound => self.initialize(),
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    // A zero axis keeps the current extent; a negative one is
                    // protocol-illegal, so treat it the same rather than cast
                    // it to a huge width.
                    let (current_w, current_h) = self.size;
                    self.size = (
                        if width <= 0 {
                            current_w
                        } else {
                            width as usize
                        },
                        if height <= 0 {
                            current_h
                        } else {
                            height as usize
                        },
                    );
                    self.dirty = true;
                }
                self.client.acknowledge(serial)
            }
            Handled::CloseRequested => {
                self.client.close();
                Ok(())
            }
            Handled::GlobalRemoved { required: true, .. } => {
                Err("required Wayland global was removed".into())
            }
            Handled::Capabilities {
                keyboard: false, ..
            }
            | Handled::SeatRemoved => Err("installer keyboard is unavailable".into()),
            // Only translated keyboard presses navigate the live pages.
            Handled::Keyboard(event) => {
                let next = keyboard_page(self.page, event)?;
                if next != self.page {
                    self.page = next;
                    self.dirty = true;
                }
                Ok(())
            }
            Handled::GlobalRemoved { .. }
            | Handled::Capabilities { .. }
            | Handled::Pointer(_)
            | Handled::Clipboard(_)
            | Handled::Primary(_) => Ok(()),
            Handled::Unhandled => Err(format!(
                "unexpected Wayland event {}:{}",
                message.object, message.opcode
            )),
        }
    }

    fn draw(&mut self) -> Result<()> {
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        let (width, height) = self.size;
        let surface = Surface::new(width, height, Scale::new(1).map_err(error)?).map_err(error)?;
        let Window {
            client,
            font,
            typeface,
            page,
            ..
        } = self;
        let presented = client.present(width, height, &mut |pixels| {
            let mut raster = Raster::new(pixels, font, surface, width * 4)
                .map_err(error)?
                .with_typeface(typeface.as_mut());
            let painted = match page {
                Page::Welcome => Welcome::new(surface)
                    .map(|view| raster.paint(&view, surface.bounds()).map_err(error)),
                Page::DestinationUnavailable => DestinationPage::unavailable(surface)
                    .map(|view| raster.paint(&view, surface.bounds()).map_err(error)),
            };
            match painted {
                Some(result) => result,
                // Too small for the page: a plain chrome ground, never garbage.
                None => {
                    raster.draw(Draw {
                        clip: surface.bounds(),
                        primitive: Primitive::Fill {
                            rect: surface.bounds(),
                            color: CHROME,
                        },
                    });
                    Ok(())
                }
            }
        })?;
        if presented {
            self.dirty = false;
        }
        Ok(())
    }
}

impl App for Window {
    type Tag = Object;

    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }

    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }

    fn descriptor_wait(&mut self) {}

    fn tick(&mut self, _now: u64) -> Result<()> {
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        Window::event(self, message)
    }

    fn end_turn(&mut self, _now: u64, _idle: bool) -> Result<()> {
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        Window::draw(self)
    }
}

/// Opens the installer window: resolves the display endpoint from the
/// environment, connects, and runs the turn loop presenting the welcome
/// page and the unavailable destination state.
pub fn run_window() -> std::io::Result<()> {
    let work = || -> Result<()> {
        let endpoint = endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        let stream = connect(endpoint)?;
        let mut window = Window::new(stream, std::env::temp_dir())?;
        window.typeface = td_ui::pinned_face::load_or_note(
            "td-setup",
            std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
        );
        run(&mut window)
    };
    work().map_err(std::io::Error::other)
}

#[cfg(test)]
#[test]
fn navigation_stops_at_the_service_boundary_and_returns() {
    assert_eq!(Page::Welcome.key("Return"), Page::DestinationUnavailable);
    assert_eq!(Page::Welcome.key("KP_Enter"), Page::DestinationUnavailable);
    assert_eq!(
        Page::DestinationUnavailable.key("Return"),
        Page::DestinationUnavailable
    );
    assert_eq!(Page::DestinationUnavailable.key("Escape"), Page::Welcome);
    assert_eq!(Page::Welcome.key("Escape"), Page::Welcome);
    assert_eq!(
        keyboard_page(Page::Welcome, KeyboardEvent::Keymap(Err("bad map".into()))),
        Err("keyboard keymap: bad map".into())
    );
}
