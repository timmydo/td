//! The real headless `td-compositor` as a render oracle for the file chooser.
//! td's shared native harness (`td-test-compositor`) launches the compositor
//! from `TD_TEST_COMPOSITOR`,
//! and a minimal in-process `ChooserClient` maps one toplevel that presents
//! `Chooser::render_sized`. The portal binary is a D-Bus service, not a
//! spawnable Wayland client, and its manager global lives only on the private
//! socket, so this case drives a bare client over the public compositor: it
//! proves the chooser's real pixels survive the live compositor round-trip,
//! while `tests/dialog.rs` covers the manager protocol the wire cannot.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use td_portal::file_chooser::{Chooser, Mode};
use td_test_compositor::{ppm, Compositor, Controls, FRAME_BYTES, OUTPUT_HEIGHT, OUTPUT_WIDTH};
use td_ui::client::{run, App, Client, Handled, Tag};
use td_ui::wire::Message;

const APP_ID: &str = "td-portal-file-chooser";

/// The chooser client owns no Wayland objects of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {}

impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}

/// A minimal live client: it maps one toplevel and presents the chooser at the
/// compositor's configured extent, then stops when asked. It reads nothing
/// from the seat, so seat, keyboard, pointer and clipboard events are no-ops.
struct ChooserClient {
    client: Client<Object>,
    chooser: Chooser,
    size: (usize, usize),
    dirty: bool,
    stop: Arc<AtomicBool>,
}

impl ChooserClient {
    fn new(
        stream: UnixStream,
        temporary: PathBuf,
        chooser: Chooser,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            client: Client::new(stream, temporary).unwrap(),
            chooser,
            size: (
                td_portal::file_chooser::WIDTH,
                td_portal::file_chooser::HEIGHT,
            ),
            dirty: true,
            stop,
        }
    }
}

impl App for ChooserClient {
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
        match self.client.handle(&message, 0)? {
            Handled::Done | Handled::FrameDone => Ok(()),
            Handled::Bound => {
                self.client.set_title("Open file")?;
                self.client.set_app_id(APP_ID)?;
                self.client.commit()
            }
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
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
            Handled::GlobalRemoved { .. }
            | Handled::Capabilities { .. }
            | Handled::Keyboard(_)
            | Handled::Pointer(_)
            | Handled::Clipboard(_)
            | Handled::Primary(_)
            | Handled::SeatRemoved => Ok(()),
            Handled::Unhandled => Err(format!(
                "unexpected Wayland event {}:{}",
                message.object, message.opcode
            )),
        }
    }

    fn end_turn(&mut self, _now: u64, _idle: bool) -> Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            self.client.close();
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        let (width, height) = self.size;
        let Self {
            client, chooser, ..
        } = self;
        let presented = client.present(width, height, &mut |pixels| {
            let frame = chooser.render_sized(width, height)?;
            if frame.len() != pixels.len() {
                return Err("chooser frame size".into());
            }
            pixels.copy_from_slice(&frame);
            Ok(())
        })?;
        if presented {
            self.dirty = false;
        }
        Ok(())
    }
}

/// A directory with one file, so the chooser has a deterministic entry to
/// render both in the client and in the oracle.
fn chooser_root(directory: &Directory) -> PathBuf {
    let root = directory.0.join("Downloads");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("alpha.txt"), b"native fixture").unwrap();
    root
}

fn open_chooser(root: &Path) -> Chooser {
    Chooser::open_with_options(
        "Open file",
        root,
        Path::new("/home/td/Downloads"),
        Mode::OpenFile { multiple: false },
        None,
        None,
    )
    .unwrap()
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn chooser_presents_over_the_native_compositor() {
    let compositor_directory = Directory::new("td-portal-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );

    let client_directory = Directory::new("td-portal-process");
    let root = chooser_root(&client_directory);
    let socket = compositor.display();
    let stop = Arc::new(AtomicBool::new(false));
    let client_stop = stop.clone();
    let client_runtime = client_directory.0.clone();
    let client_root = root.clone();
    let client = std::thread::spawn(move || -> Result<()> {
        let stream =
            UnixStream::connect(&socket).map_err(|e| format!("connect compositor: {e}"))?;
        let chooser = open_chooser(&client_root);
        let mut app = ChooserClient::new(stream, client_runtime, chooser, client_stop);
        run(&mut app)
    });

    // Wait for the client to bind, set its app id, and map its one toplevel;
    // the compositor then reports the tile it composited it into.
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement(APP_ID) {
            break place;
        }
        assert!(
            Instant::now() < deadline,
            "chooser window never mapped; client finished: {}",
            client.is_finished()
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        place.x + place.width <= OUTPUT_WIDTH && place.y + place.height <= OUTPUT_HEIGHT,
        "reported tile {place:?} exceeds the {OUTPUT_WIDTH}x{OUTPUT_HEIGHT} output"
    );

    // The client obeys the compositor's configure and presents the chooser at
    // the tile's extent, so the captured pixels equal the crate's own render of
    // that same surface. A capture may still be the earlier default-extent
    // frame clipped to the tile, so the loop waits for the settled frame under
    // a two-observation sandwich around the capture, within its own deadline.
    let expected = open_chooser(&root)
        .render_sized(place.width, place.height)
        .unwrap();
    let render_deadline = Instant::now() + TIMEOUT;
    let mut rendered = false;
    while Instant::now() < render_deadline {
        std::thread::sleep(Duration::from_millis(2));
        let first = compositor.observe(&place.window);
        if !first.current {
            continue;
        }
        let capture = compositor.request("capture", FRAME_BYTES + 128);
        let (output, pixels) = ppm(&capture, compositor.session()).unwrap();
        let second = compositor.observe(&place.window);
        if !second.current || second.commit != first.commit {
            continue;
        }
        assert_eq!(first.client, second.client);
        assert!(output >= first.output && output <= second.output);
        let matches = (0..place.height).all(|y| {
            (0..place.width).all(|x| {
                let source = ((place.y + y) * OUTPUT_WIDTH + place.x + x) * 3;
                let target = (y * place.width + x) * 4;
                pixels[source..source + 3]
                    == [expected[target + 2], expected[target + 1], expected[target]]
            })
        });
        if matches {
            rendered = true;
            break;
        }
    }
    assert!(
        rendered,
        "chooser did not present over the native compositor within {TIMEOUT:?}"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = client.join().unwrap();
    compositor.stop();
}
