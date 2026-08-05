//! td-term's Wayland client: the handshake, and the grid the surface holds.
//!
//! The half that reaches a configured surface. Readiness is deliberately NOT
//! published here, and that is not a scoping choice: §12 has the terminal
//! announce itself only once the tile-sized buffer has come back with both
//! `wl_buffer.release` and its frame callback, and an unmapped surface never
//! receives a second configure — td's compositor tracks a view only once a
//! buffer is attached, so the only configure a bufferless client ever sees is
//! the initial zero one. Publishing here would therefore publish the FALLBACK
//! grid every time, which is exactly the number a readiness socket must not
//! be carrying. The frame landing is what earns it.

use crate::conn::{
    self, Connection, Globals, COMPOSITOR, REGISTRY, SHM, XDG_SURFACE, XDG_TOPLEVEL, XDG_WM_BASE,
};
use crate::font::Font;
use crate::scene::SHM_XRGB8888;
use crate::{font, pty, wire};

/// One past the last fixed id the TERMINAL creates. It binds no seat and
/// creates no keyboard or pointer, so its dynamic range starts three lower
/// than the demo's — and starting where the demo does would SKIP those three,
/// which Wayland forbids. See `conn`'s note.
#[allow(dead_code)]
const FIRST_DYNAMIC_ID: u32 = XDG_TOPLEVEL + 1;

/// What an operator sees in a title bar. td's own compositor parses and
/// discards it; it is set because a client that names itself is easier to
/// identify in a trace than one that does not.
const TITLE: &str = "td terminal";

/// The grid a terminal falls back to when the compositor proposes no size.
/// 80 by 24 is what a terminfo entry, a shell prompt and anything that draws
/// a box assume when they cannot ask — so the fallback is expressed in CELLS
/// and turned into pixels by the font, rather than being a pixel constant
/// that happens to divide into some grid or other.
const DEFAULT_COLUMNS: usize = 80;
const DEFAULT_ROWS: usize = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Size {
    pub width: usize,
    pub height: usize,
}

/// The pixel size of a grid this many cells across, which is what the client
/// asks for when the compositor proposes nothing.
pub fn default_size(font: &Font) -> Result<Size, String> {
    let width = DEFAULT_COLUMNS
        .checked_mul(font.width())
        .ok_or_else(|| "the default column count overflows a pixel width".to_string())?;
    let height = DEFAULT_ROWS
        .checked_mul(font.height())
        .ok_or_else(|| "the default row count overflows a pixel height".to_string())?;
    Ok(Size { width, height })
}

/// The cell grid a surface of this size holds, as a pair the readiness line
/// can carry. `grid_for_tile` is the same division the renderer clips to, and
/// `grid_size` is the same validity the winsize ioctl is held to — so a grid
/// this refuses is one nothing downstream would have accepted either.
pub fn grid(size: Size, font: &Font) -> Result<(u16, u16), String> {
    let (rows, columns) = pty::grid_for_tile(size.width, size.height, font.width(), font.height())?;
    let window = pty::grid_size(rows, columns)?;
    Ok((window.rows, window.columns))
}

/// The registry names of the globals this client bound, so that a later
/// `global_remove` naming one of them can be told from a device it never
/// asked for. A bound global that goes away leaves requests to it silently
/// ignored by the compositor, which is a terminal that stalls at its first
/// buffer rather than one that says what happened.
#[derive(Clone, Copy)]
struct Bound {
    compositor: u32,
    shm: u32,
    xdg_wm_base: u32,
}

impl Bound {
    fn interface(self, name: u32) -> Option<&'static str> {
        match name {
            _ if name == self.compositor => Some("wl_compositor"),
            _ if name == self.shm => Some("wl_shm"),
            _ if name == self.xdg_wm_base => Some("xdg_wm_base"),
            _ => None,
        }
    }
}

/// Bind the three globals a terminal needs. No `wl_seat`: there is no
/// keyboard to route yet, and binding one would mean fielding capability
/// events for a device nothing reads.
#[allow(dead_code)]
fn bind_globals(connection: &mut Connection) -> Result<Bound, String> {
    let globals = conn::discover_globals(connection)?;
    let (compositor_name, compositor_version) =
        Globals::require(globals.compositor(), "wl_compositor", 4, 4)?;
    let (shm_name, shm_version) = Globals::require(globals.shm(), "wl_shm", 1, 1)?;
    let (xdg_name, xdg_version) = Globals::require(globals.xdg_wm_base(), "xdg_wm_base", 1, 1)?;
    conn::bind(
        connection,
        compositor_name,
        "wl_compositor",
        compositor_version,
        COMPOSITOR,
    )?;
    conn::bind(connection, shm_name, "wl_shm", shm_version, SHM)?;
    conn::bind(
        connection,
        xdg_name,
        "xdg_wm_base",
        xdg_version,
        XDG_WM_BASE,
    )?;
    Ok(Bound {
        compositor: compositor_name,
        shm: shm_name,
        xdg_wm_base: xdg_name,
    })
}

/// What the client knows about its surface between configures.
///
/// The toplevel configure carries the size and the SURFACE configure is where
/// it takes effect, so a proposal is only ever adopted at the second — which
/// is also the only one that is acknowledged. Two events, one transition.
#[allow(dead_code)]
struct Surface {
    bound: Bound,
    proposed: Option<Size>,
    current: Option<Size>,
    xrgb: bool,
}

// The state machine is a unit the frame landing consumes whole, so the allow
// is on the block rather than on each of the four: there is no half of this
// that could be left over for a per-item allow to keep visible.
#[allow(dead_code)]
impl Surface {
    fn new(bound: Bound) -> Surface {
        Surface {
            bound,
            proposed: None,
            current: None,
            xrgb: false,
        }
    }

    /// Handle one event. `Ok(true)` means a configure completed, so the
    /// surface now has a size the caller can act on.
    fn dispatch(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
        fallback: Size,
    ) -> Result<bool, String> {
        if message.object == REGISTRY && (message.opcode == 0 || message.opcode == 1) {
            // `wl_registry` has no destroy request, so it stays live for the
            // whole session and a monitor or input device arriving after
            // discovery is delivered HERE. The terminal has everything it
            // asked for, so a later global is not its business — but dying on
            // one would make an ordinary hotplug take the terminal down. It is
            // still PARSED, because every other arm validates what it consumes
            // and a malformed event is a broken compositor either way.
            let mut args = wire::Cursor::new(&message.payload);
            let name = args.u32()?;
            if message.opcode == 0 {
                args.string()?;
                args.u32()?;
            }
            args.finish()?;
            // A global this client BOUND going away is different: the
            // compositor ignores every later request to that object, so the
            // terminal would stall at its first buffer with nothing to report.
            if message.opcode == 1 {
                if let Some(interface) = self.bound.interface(name) {
                    return Err(format!("compositor withdrew {interface} while it was in use"));
                }
            }
            return Ok(false);
        }
        if message.object == SHM && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            if args.u32()? == SHM_XRGB8888 {
                self.xrgb = true;
            }
            args.finish()?;
            return Ok(false);
        }
        if message.object == XDG_TOPLEVEL && message.opcode == 0 {
            self.proposed = Some(self.toplevel_size(message, fallback)?);
            return Ok(false);
        }
        if message.object == XDG_TOPLEVEL && message.opcode == 1 {
            return Err("compositor requested that the terminal close".into());
        }
        if message.object == XDG_SURFACE && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let serial = args.u32()?;
            args.finish()?;
            let mut ack = wire::Builder::new();
            ack.u32(serial);
            connection.send(XDG_SURFACE, 4, ack)?;
            // A bare surface configure REUSES the last applied size rather
            // than falling back: the compositor is confirming what it already
            // proposed, not withdrawing it.
            self.current = Some(self.proposed.take().or(self.current).unwrap_or(fallback));
            return Ok(true);
        }
        Err(format!(
            "unexpected Wayland event object={} opcode={}",
            message.object, message.opcode
        ))
    }

    /// One event, with the display's own three answered where they belong.
    fn dispatch_next(
        &mut self,
        connection: &mut Connection,
        fallback: Size,
    ) -> Result<bool, String> {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            return Ok(false);
        }
        self.dispatch(connection, &message, fallback)
    }

    /// A configure of zero in either axis is the compositor declining to
    /// choose, not a zero-sized window; each axis declines independently, so
    /// the fallback fills in per axis rather than wholesale.
    fn toplevel_size(&self, message: &wire::Message, fallback: Size) -> Result<Size, String> {
        let mut args = wire::Cursor::new(&message.payload);
        let width = args.i32()?;
        let height = args.i32()?;
        if width < 0 || height < 0 {
            return Err(format!(
                "compositor configured a negative terminal size {width}x{height}"
            ));
        }
        let states = usize::try_from(args.u32()?)
            .map_err(|_| "XDG state array length overflow".to_string())?;
        if !states.is_multiple_of(4) {
            return Err(format!("XDG state array has invalid length {states}"));
        }
        for _ in 0..states / 4 {
            args.u32()?;
        }
        args.finish()?;
        let width = usize::try_from(width)
            .map_err(|_| "configured terminal width escaped usize".to_string())?;
        let height = usize::try_from(height)
            .map_err(|_| "configured terminal height escaped usize".to_string())?;
        let current = self.current.unwrap_or(fallback);
        Ok(Size {
            width: if width == 0 { current.width } else { width },
            height: if height == 0 { current.height } else { height },
        })
    }
}

/// The handshake itself, from a dialled connection to a configured surface.
/// This is the PRODUCTION sequence — `run` will dial and then call it — and
/// the test wrapper below is deliberately thin so what runs against the real
/// server is what the binary will run, as the demo's `present_for_test` is.
#[allow(dead_code)]
fn handshake(connection: &mut Connection, fallback: Size) -> Result<(Size, bool), String> {
    let bound = bind_globals(connection)?;
    conn::create_surface(connection, TITLE)?;
    let mut surface = Surface::new(bound);
    while !surface.dispatch_next(connection, fallback)? {}
    let size = surface
        .current
        .ok_or_else(|| "the terminal was configured without a size".to_string())?;
    let unclaimed = connection.pending_fd_count();
    if unclaimed != 0 {
        return Err(format!(
            "the terminal handshake retained {unclaimed} unexpected descriptors"
        ));
    }
    Ok((size, surface.xrgb))
}

/// The handshake over an already-open stream, which is the only way to drive
/// it against the real server without a socket on disk.
#[cfg(test)]
pub fn handshake_for_test(
    stream: std::os::unix::net::UnixStream,
) -> Result<(Connection, Size, bool), String> {
    let font = font::pinned()?;
    let fallback = default_size(&font)?;
    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(20))
        .ok_or_else(|| "could not bound the terminal's Wayland handshake".to_string())?;
    let mut connection = Connection::over(stream, Some(deadline), FIRST_DYNAMIC_ID);
    connection.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
    let (size, xrgb) = handshake(&mut connection, fallback)?;
    Ok((connection, size, xrgb))
}

pub fn selftest() -> Result<(), String> {
    let font = font::pinned()?;
    let fallback = default_size(&font)?;
    let (rows, columns) = grid(fallback, &font)?;
    let expected = (
        u16::try_from(DEFAULT_ROWS).map_err(|_| "default rows escape a grid".to_string())?,
        u16::try_from(DEFAULT_COLUMNS).map_err(|_| "default columns escape a grid".to_string())?,
    );
    if (rows, columns) != expected {
        return Err(format!(
            "the default surface holds {rows}x{columns} cells, not {DEFAULT_ROWS}x{DEFAULT_COLUMNS}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{DISPLAY, KEYBOARD, POINTER, SEAT, SURFACE, SYNC_CALLBACK};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn font() -> Font {
        font::pinned().unwrap()
    }

    /// The fallback is expressed in cells, so the grid it yields is exactly
    /// the grid it names — whatever the pinned font's cell size happens to be.
    #[test]
    fn the_default_surface_holds_exactly_the_default_grid() {
        let font = font();
        let size = default_size(&font).unwrap();
        assert_eq!(size.width, DEFAULT_COLUMNS * font.width());
        assert_eq!(size.height, DEFAULT_ROWS * font.height());
        assert_eq!(grid(size, &font).unwrap(), (24, 80));
    }

    /// A tile smaller than one cell is still a terminal, because the renderer
    /// clips; a zero-row grid is not, because `grid_size` refuses it.
    #[test]
    fn a_surface_too_small_for_a_cell_still_names_a_grid() {
        let font = font();
        let tiny = Size {
            width: 1,
            height: 1,
        };
        assert_eq!(grid(tiny, &font).unwrap(), (1, 1));
    }

    fn message(object: u32, opcode: u16, payload: Vec<u8>) -> wire::Message {
        wire::Message {
            object,
            opcode,
            payload,
        }
    }

    fn toplevel_configure(width: i32, height: i32) -> wire::Message {
        let mut payload = Vec::new();
        payload.extend_from_slice(&width.to_ne_bytes());
        payload.extend_from_slice(&height.to_ne_bytes());
        payload.extend_from_slice(&0u32.to_ne_bytes());
        message(XDG_TOPLEVEL, 0, payload)
    }

    fn surface_configure(serial: u32) -> wire::Message {
        message(XDG_SURFACE, 0, serial.to_ne_bytes().to_vec())
    }

    fn pair() -> (Connection, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (Connection::over(ours, None, FIRST_DYNAMIC_ID), theirs)
    }

    /// Wayland ids must be allocated DENSELY, and td's own server only checks
    /// uniqueness — so nothing at runtime would report a gap. What the
    /// terminal uses has to be exactly 1..FIRST_DYNAMIC_ID with nothing
    /// skipped, and the demo's higher start would skip the three it does not
    /// create.
    #[test]
    fn the_terminal_leaves_no_gap_before_its_first_dynamic_id() {
        let mut used = vec![
            DISPLAY,
            REGISTRY,
            SYNC_CALLBACK,
            COMPOSITOR,
            SHM,
            XDG_WM_BASE,
            SURFACE,
            XDG_SURFACE,
            XDG_TOPLEVEL,
        ];
        used.sort_unstable();
        used.dedup();
        assert_eq!(
            used,
            (1..FIRST_DYNAMIC_ID).collect::<Vec<u32>>(),
            "the terminal's fixed ids are not dense up to its dynamic range"
        );
        // The three it deliberately does not create are the gap the demo's
        // start would leave.
        for absent in [SEAT, KEYBOARD, POINTER] {
            assert!(absent >= FIRST_DYNAMIC_ID);
        }
    }

    /// The size arrives on one event and takes effect on another. A toplevel
    /// configure alone must not move the surface, or a client would act on a
    /// size the compositor has not committed to.
    #[test]
    fn a_size_takes_effect_at_the_surface_configure_and_not_before() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            // The registry NAMES td's server assigns, not object ids: 3 is
            // wl_output there, which is why it is not one of these.
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 640,
            height: 480,
        };

        assert!(!surface
            .dispatch(&mut connection, &toplevel_configure(320, 200), fallback)
            .unwrap());
        assert_eq!(surface.current, None, "a proposal moved the surface");

        assert!(surface
            .dispatch(&mut connection, &surface_configure(7), fallback)
            .unwrap());
        assert_eq!(
            surface.current,
            Some(Size {
                width: 320,
                height: 200
            })
        );
    }

    /// A surface configure with no proposal before it is the compositor
    /// confirming what it already applied. Falling back here would resize a
    /// settled terminal to the default grid — silently, since a configure is
    /// not an error.
    #[test]
    fn a_bare_surface_configure_reuses_the_size_already_applied() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 640,
            height: 480,
        };
        let settled = Size {
            width: 1024,
            height: 768,
        };
        surface
            .dispatch(
                &mut connection,
                &toplevel_configure(1024, 768),
                fallback,
            )
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        assert_eq!(surface.current, Some(settled));

        // No toplevel configure this time: nothing was proposed at all.
        surface
            .dispatch(&mut connection, &surface_configure(2), fallback)
            .unwrap();
        assert_eq!(
            surface.current,
            Some(settled),
            "a bare configure resized a settled terminal"
        );
    }

    /// `wl_registry` has no destroy request, so a hotplug after discovery is
    /// delivered to a client that has stopped listening. Dying on it would
    /// take the terminal down when a monitor is plugged in.
    #[test]
    fn a_global_arriving_after_discovery_is_not_fatal() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        // A `global`: name, interface, version. Wayland strings carry their
        // length, a NUL, and padding to a four-byte boundary.
        let mut arrived = Vec::new();
        arrived.extend_from_slice(&9u32.to_ne_bytes());
        arrived.extend_from_slice(&10u32.to_ne_bytes());
        arrived.extend_from_slice(b"wl_output\0\0\0");
        arrived.extend_from_slice(&4u32.to_ne_bytes());
        // A `global_remove`: the name alone.
        let mut departed = Vec::new();
        departed.extend_from_slice(&9u32.to_ne_bytes());

        for (opcode, payload) in [(0u16, arrived), (1u16, departed)] {
            assert!(
                !surface
                    .dispatch(&mut connection, &message(REGISTRY, opcode, payload), fallback)
                    .unwrap(),
                "a registry event completed a configure"
            );
        }
        assert_eq!(surface.current, None, "a registry event moved the surface");

        // Tolerated is not unparsed: every other arm validates what it
        // consumes, and a truncated global is a broken compositor either way.
        assert!(surface
            .dispatch(&mut connection, &message(REGISTRY, 0, Vec::new()), fallback)
            .is_err());

        // A global this client BOUND going away is the case that is not
        // tolerable: every later request to that object is ignored by the
        // compositor, so the terminal would stall at its first buffer.
        for (name, interface) in [(1u32, "wl_compositor"), (2, "wl_shm"), (4, "xdg_wm_base")] {
            let refused = surface
                .dispatch(
                    &mut connection,
                    &message(REGISTRY, 1, name.to_ne_bytes().to_vec()),
                    fallback,
                )
                .unwrap_err();
            assert_eq!(
                refused,
                format!("compositor withdrew {interface} while it was in use")
            );
        }
    }

    /// Zero is the compositor declining to choose, per axis. A wholesale
    /// fallback would turn "keep your width, take this height" into a resize
    /// of both.
    #[test]
    fn each_axis_declines_independently() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 640,
            height: 480,
        };
        surface
            .dispatch(&mut connection, &toplevel_configure(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        assert_eq!(
            surface.current,
            Some(fallback),
            "a configure declining both axes resized"
        );

        surface
            .dispatch(&mut connection, &toplevel_configure(0, 200), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(2), fallback)
            .unwrap();
        assert_eq!(
            surface.current,
            Some(Size {
                width: 640,
                height: 200
            }),
            "a declined axis did not keep the size it had"
        );
    }

    /// The acknowledgement is not optional: a compositor that never receives
    /// one stops configuring, and the terminal waits for a size it will not
    /// be sent again.
    #[test]
    fn a_surface_configure_is_acknowledged_with_its_own_serial() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface
            .dispatch(&mut connection, &surface_configure(0x2a), fallback)
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut said = [0u8; 12];
        std::io::Read::read_exact(&mut peer, &mut said).unwrap();
        let object = u32::from_ne_bytes([said[0], said[1], said[2], said[3]]);
        let opcode = u16::from_ne_bytes([said[4], said[5]]);
        let serial = u32::from_ne_bytes([said[8], said[9], said[10], said[11]]);
        assert_eq!((object, opcode, serial), (XDG_SURFACE, 4, 0x2a));
    }

    #[test]
    fn a_negative_or_malformed_configure_is_refused() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        // Both axes are checked, and the MESSAGE is what pins the check: a
        // negative size fails `usize::try_from` a few lines later whether the
        // guard is there or not, so only the diagnostic distinguishes a
        // compositor that proposed nonsense from an arithmetic surprise.
        for (width, height) in [(-1, 8), (8, -1)] {
            let refused = surface
                .dispatch(
                    &mut connection,
                    &toplevel_configure(width, height),
                    fallback,
                )
                .unwrap_err();
            assert_eq!(
                refused,
                format!("compositor configured a negative terminal size {width}x{height}")
            );
        }
        // A state array whose length is not a whole number of words.
        let mut payload = Vec::new();
        payload.extend_from_slice(&8i32.to_ne_bytes());
        payload.extend_from_slice(&8i32.to_ne_bytes());
        payload.extend_from_slice(&3u32.to_ne_bytes());
        assert!(surface
            .dispatch(&mut connection, &message(XDG_TOPLEVEL, 0, payload), fallback)
            .is_err());
    }

    #[test]
    fn a_close_request_and_an_unknown_event_are_both_refused() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        assert!(surface
            .dispatch(&mut connection, &message(XDG_TOPLEVEL, 1, Vec::new()), fallback)
            .is_err());
        assert!(surface
            .dispatch(&mut connection, &message(4242, 0, Vec::new()), fallback)
            .is_err());
    }

    /// The format is only NOTICED here, not acted on: the refusal belongs
    /// with the frame that would use it, as the demo's does in `commit_frame`.
    /// What this pins is that the advertisement is seen at all, since it
    /// arrives before the configure and would otherwise be easy to drop.
    #[test]
    fn the_pixel_format_is_noticed_when_it_is_advertised() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        assert!(!surface.xrgb);
        surface
            .dispatch(
                &mut connection,
                &message(SHM, 0, 9u32.to_ne_bytes().to_vec()),
                fallback,
            )
            .unwrap();
        assert!(!surface.xrgb, "an unrelated format was accepted");
        surface
            .dispatch(
                &mut connection,
                &message(SHM, 0, SHM_XRGB8888.to_ne_bytes().to_vec()),
                fallback,
            )
            .unwrap();
        assert!(surface.xrgb);
    }

    #[test]
    fn the_selftest_covers_the_default_grid() {
        selftest().unwrap();
    }
}
