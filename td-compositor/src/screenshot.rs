//! Print's screenshot (DESIGN.md, "Screenshots"). The press captures the
//! public scene without the cursor, and the chooser card keeps it until a
//! key copies it to the clipboard as `image/png`, saves it in the home
//! folder, or drops it. Encoding and the file write run on a worker of
//! their own, so neither holds the runtime lock nor the input reader.

use crate::clock::Clock;
use crate::help::Row;
use crate::runtime::Runtime;
use crate::text::Text;
use crate::ui;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const CARD_WIDTH: usize = 460;
const CARD_PADDING: usize = 24;
const TITLE_TOP: usize = 18;
const FIRST_ROW_TOP: usize = 48;
const ROW_STEP: usize = 24;
const BOTTOM_PADDING: usize = 10;
const KEYS_LEFT: usize = 20;
const ACTION_LEFT: usize = 180;
const CARD: [u8; 4] = [0x18, 0x20, 0x28, 0];
/// Saves name a second; a second save within it takes the next suffix.
const MAX_SUFFIX: u32 = 9;

/// What the input layer asks of the chooser: `Take` is Print, the rest are
/// the card's keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenshotAction {
    Take,
    Copy,
    Save,
    Cancel,
}

const COPY: Row = Row {
    keys: "C / ENTER",
    action: "COPY TO CLIPBOARD",
};
const SAVE: Row = Row {
    keys: "S",
    action: "SAVE IN HOME FOLDER",
};
const CANCEL: Row = Row {
    keys: "ESCAPE",
    action: "CANCEL",
};

/// The card's rows: saving only where this compositor can write the home
/// folder.
pub fn rows(saves: bool) -> impl Iterator<Item = &'static Row> {
    std::iter::once(&COPY)
        .chain(saves.then_some(&SAVE))
        .chain(std::iter::once(&CANCEL))
}

/// The output's public pixels at the press: BGRX rows of `width * 4`
/// bytes, the frame layout the scene renders.
pub(crate) struct Capture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Capture {
    /// Packed RGB, dropping each pixel's unused byte.
    fn rgb(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(self.width.saturating_mul(self.height).saturating_mul(3));
        for &[b, g, r, _] in self.pixels.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[r, g, b]);
        }
        rgb
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Destination {
    Clipboard,
    Home,
}

pub(crate) struct Job {
    pub capture: Capture,
    pub destination: Destination,
    /// The selection when the key was pressed, which a copy replaces only
    /// if it is still the selection when the picture is ready.
    pub replaces: Option<crate::runtime::DataSourceIdentity>,
}

/// The card, and the capture it holds while it is up: closing it for any
/// reason, a lock or secure attention among them, drops the pixels.
#[derive(Default)]
pub(crate) struct Chooser {
    capture: Option<Capture>,
    saves: bool,
}

impl Chooser {
    pub fn open(&mut self, capture: Capture, saves: bool) {
        self.capture = Some(capture);
        self.saves = saves;
    }

    /// The capture, closing the card.
    pub fn take(&mut self) -> Option<Capture> {
        self.capture.take()
    }

    pub fn close(&mut self) {
        self.capture = None;
    }

    pub fn visible(&self) -> bool {
        self.capture.is_some()
    }

    pub fn saves(&self) -> bool {
        self.saves
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize, text: &Text) {
        if !self.visible() {
            return;
        }
        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height =
            card_height(self.saves).min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let left = width.saturating_sub(card_width) / 2;
        let top = height.saturating_sub(card_height) / 2;
        let card = (left, top, card_width, card_height);
        ui::fill(frame, width, height, stride, card, CARD);
        ui::border(frame, width, height, stride, card, [0x70, 0xc0, 0xf0, 0]);
        text.draw(
            frame,
            width,
            height,
            stride,
            (
                left.saturating_add(KEYS_LEFT),
                top.saturating_add(TITLE_TOP),
            ),
            "SCREENSHOT",
            ([0xff, 0xff, 0xff, 0], CARD),
            card,
        );
        for (index, row) in rows(self.saves).enumerate() {
            let row_top = top
                .saturating_add(FIRST_ROW_TOP)
                .saturating_add(index.saturating_mul(ROW_STEP));
            text.draw(
                frame,
                width,
                height,
                stride,
                (left.saturating_add(KEYS_LEFT), row_top),
                row.keys,
                ([0xb0, 0xd8, 0xf0, 0], CARD),
                card,
            );
            text.draw(
                frame,
                width,
                height,
                stride,
                (left.saturating_add(ACTION_LEFT), row_top),
                row.action,
                ([0xff, 0xff, 0xff, 0], CARD),
                card,
            );
        }
    }
}

fn card_height(saves: bool) -> usize {
    FIRST_ROW_TOP
        .saturating_add(rows(saves).count().saturating_mul(ROW_STEP))
        .saturating_add(BOTTOM_PADDING)
}

/// `HOME` as the folder a save writes, when it names an existing absolute
/// directory.
pub(crate) fn home_folder(home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(home?);
    (path.is_absolute() && path.is_dir()).then_some(path)
}

/// Start the worker. `home` is the folder `Save` writes, given only where
/// the compositor runs as the person whose folder it is.
pub(crate) fn start(
    runtime: Arc<Mutex<Runtime>>,
    home: Option<PathBuf>,
) -> Result<SyncSender<Job>, String> {
    // One job queued behind the one encoding: a third press is told the
    // worker is busy rather than queued without bound.
    let (sender, receiver) = mpsc::sync_channel::<Job>(1);
    thread::Builder::new()
        .name("td-screenshot".into())
        .spawn(move || serve(&runtime, home.as_deref(), &receiver))
        .map_err(|e| format!("start screenshot worker: {e}"))?;
    Ok(sender)
}

fn serve(runtime: &Mutex<Runtime>, home: Option<&Path>, jobs: &Receiver<Job>) {
    let clock =
        Clock::load(Path::new("/etc/timezone"), Path::new("/etc/zoneinfo")).unwrap_or(Clock::Utc);
    while let Ok(job) = jobs.recv() {
        let notice = match run(runtime, home, &clock, job) {
            Ok(notice) => notice,
            Err(error) => {
                eprintln!("td-compositor: screenshot: {error}");
                format!("SCREENSHOT FAILED: {error}")
            }
        };
        let Ok(mut runtime) = runtime.lock() else {
            return;
        };
        if let Err(error) = runtime.screenshot_notice(notice) {
            eprintln!("td-compositor: screenshot notice: {error}");
        }
    }
}

fn run(
    runtime: &Mutex<Runtime>,
    home: Option<&Path>,
    clock: &Clock,
    job: Job,
) -> Result<String, String> {
    let Job {
        capture,
        destination,
        replaces,
    } = job;
    let png = crate::png_encode::encode_rgb8(capture.width, capture.height, &capture.rgb())
        .map_err(|e| e.to_string())?;
    drop(capture);
    match destination {
        Destination::Clipboard => {
            runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .own_png(png, replaces)?;
            Ok("SCREENSHOT COPIED".into())
        }
        Destination::Home => {
            let home = home.ok_or("this session cannot save in the home folder")?;
            let epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "clock before 1970")?
                .as_secs();
            let stamp = clock
                .file_stamp(epoch)
                .or_else(|| Clock::Utc.file_stamp(epoch))
                .ok_or("no time to name the file")?;
            let name = save(home, &stamp, &png)?;
            Ok(format!("SCREENSHOT SAVED: ~/{name}"))
        }
    }
}

/// Write `png` as `screenshot_STAMP.png` in `home`, never replacing a file:
/// a name already taken in the same second gains `-1` to `-9`. Readable by
/// its owner alone, since a screenshot shows whatever was on the screen.
fn save(home: &Path, stamp: &str, png: &[u8]) -> Result<String, String> {
    for suffix in 0..=MAX_SUFFIX {
        let name = if suffix == 0 {
            format!("screenshot_{stamp}.png")
        } else {
            format!("screenshot_{stamp}-{suffix}.png")
        };
        let path = home.join(&name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create {}: {e}", path.display())),
        };
        if let Err(e) = file.write_all(png).and_then(|()| file.sync_all()) {
            // A partial picture under the final name would read as a save.
            let _ = std::fs::remove_file(&path);
            return Err(format!("write {}: {e}", path.display()));
        }
        return Ok(name);
    }
    Err(format!("screenshot_{stamp}.png and its suffixes are taken"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn rgb_drops_the_padding_byte_and_reorders_bgrx() {
        let capture = Capture {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 0xff, 4, 5, 6, 0xee],
        };
        assert_eq!(capture.rgb(), vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn the_chooser_holds_its_capture_only_while_up() {
        let mut chooser = Chooser::default();
        assert!(!chooser.visible());
        let capture = || Capture {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        };
        chooser.open(capture(), true);
        assert!(chooser.visible() && chooser.saves());
        assert!(chooser.take().is_some());
        assert!(!chooser.visible());
        assert!(chooser.take().is_none());
        chooser.open(capture(), false);
        chooser.close();
        assert!(!chooser.visible());
    }

    #[test]
    fn only_a_saving_session_offers_save() {
        let without: Vec<&Row> = rows(false).collect();
        let with: Vec<&Row> = rows(true).collect();
        assert_eq!(without, [&COPY, &CANCEL]);
        assert_eq!(with, [&COPY, &SAVE, &CANCEL]);
        assert_eq!(card_height(true), card_height(false) + ROW_STEP);
    }

    #[test]
    fn every_row_fits_and_is_in_the_font() {
        for row in rows(true) {
            assert!(
                KEYS_LEFT + Text::width(row.keys) <= ACTION_LEFT,
                "{}",
                row.keys
            );
            assert!(
                ACTION_LEFT + Text::width(row.action) <= CARD_WIDTH - KEYS_LEFT,
                "{}",
                row.action
            );
            for character in row.keys.chars().chain(row.action.chars()) {
                assert!(crate::text::covered(character), "{character:?}");
            }
        }
        const _: () = assert!(TITLE_TOP + ROW_STEP < FIRST_ROW_TOP);
        // 800x600 holds the card whole.
        assert!(card_height(true) <= 600 - CARD_PADDING * 2);
    }

    #[test]
    fn a_visible_card_paints_inside_itself_and_a_hidden_one_nothing() {
        let (width, height) = (800usize, 600usize);
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        let mut chooser = Chooser::default();
        chooser.paint(&mut frame, width, height, stride, &Text::default());
        assert!(frame.iter().all(|byte| *byte == 0));
        chooser.open(
            Capture {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            },
            true,
        );
        chooser.paint(&mut frame, width, height, stride, &Text::default());
        let left = (width - CARD_WIDTH) / 2;
        let top = (height - card_height(true)) / 2;
        let mut painted = 0;
        for y in 0..height {
            for x in 0..width {
                let at = y * stride + x * 4;
                if frame[at..at + 4].iter().any(|byte| *byte != 0) {
                    painted += 1;
                    assert!(
                        (left..left + CARD_WIDTH).contains(&x)
                            && (top..top + card_height(true)).contains(&y),
                        "{x},{y} escaped the card"
                    );
                }
            }
        }
        assert!(painted > 0);
    }

    /// The worker itself: a saved file is a PNG of the capture's size
    /// under the stamped name, a copy owns the selection, and each says so
    /// on the bar.
    #[test]
    fn the_worker_encodes_saves_and_copies_and_says_so() {
        let dir = crate::temp_dir::TempDir::new("td-compositor-screenshot-worker").unwrap();
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&dir.path().join("fb"), 40, 30, 160)
                .unwrap();
        let mut runtime = Runtime::headless(framebuffer, 7);
        let (writer, _writes) = mpsc::sync_channel(2);
        runtime.clipboard_writer(Some(writer));
        let runtime = Arc::new(Mutex::new(runtime));
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let jobs = start(Arc::clone(&runtime), Some(home.clone())).unwrap();
        let capture = || Capture {
            width: 3,
            height: 2,
            pixels: (0..24).collect(),
        };
        let wait = |done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "worker stalled");
                thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        jobs.send(Job {
            capture: capture(),
            destination: Destination::Home,
            replaces: None,
        })
        .unwrap();
        wait(&|| runtime.lock().unwrap().has_notice());
        let names: Vec<String> = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        let name = &names[0];
        assert!(
            name.starts_with("screenshot_") && name.ends_with(".png"),
            "{name}"
        );
        let png = std::fs::read(home.join(name)).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        // IHDR's width and height.
        assert_eq!(&png[16..24], &[0, 0, 0, 3, 0, 0, 0, 2]);
        assert_eq!(
            runtime.lock().unwrap().take_notice().unwrap(),
            format!("SCREENSHOT SAVED: ~/{name}")
        );

        jobs.send(Job {
            capture: capture(),
            destination: Destination::Clipboard,
            replaces: None,
        })
        .unwrap();
        wait(&|| runtime.lock().unwrap().has_notice());
        let mut runtime = runtime.lock().unwrap();
        assert_eq!(runtime.take_notice().unwrap(), "SCREENSHOT COPIED");
        assert!(runtime.owns_picture());
    }

    #[test]
    fn a_save_never_replaces_a_file_and_is_its_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::temp_dir::TempDir::new("td-compositor-screenshot").unwrap();
        let png = b"\x89PNG not really";
        assert_eq!(
            save(dir.path(), "2026-10-17-142530", png).unwrap(),
            "screenshot_2026-10-17-142530.png"
        );
        assert_eq!(
            save(dir.path(), "2026-10-17-142530", png).unwrap(),
            "screenshot_2026-10-17-142530-1.png"
        );
        let first = dir.path().join("screenshot_2026-10-17-142530.png");
        assert_eq!(std::fs::read(&first).unwrap(), png);
        let mode = std::fs::metadata(&first).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        for suffix in 2..=MAX_SUFFIX {
            save(dir.path(), "2026-10-17-142530", png).unwrap();
            assert!(dir
                .path()
                .join(format!("screenshot_2026-10-17-142530-{suffix}.png"))
                .exists());
        }
        assert!(save(dir.path(), "2026-10-17-142530", png)
            .unwrap_err()
            .contains("taken"));
        // A link where a name would go is never followed: the save takes the
        // next name and the link's target is untouched.
        let target = dir.path().join("target");
        std::fs::write(&target, b"kept").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("screenshot_link.png")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("absent-target"),
            dir.path().join("screenshot_link-1.png"),
        )
        .unwrap();
        assert_eq!(
            save(dir.path(), "link", png).unwrap(),
            "screenshot_link-2.png"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        assert!(!dir.path().join("absent-target").exists());
        // A folder it cannot write is an error naming the file.
        assert!(save(&dir.path().join("absent"), "x", png)
            .unwrap_err()
            .contains("absent"));
    }
}
