//! Times the terminal renderer, the per-frame copy td-ui's client makes and
//! the model's parse rate, without a compositor:
//!
//! ```text
//! TD_UI_FACE=DIR cargo run --release --manifest-path td-ui/Cargo.toml \
//!     --example vt_render_bench [WIDTH HEIGHT [FRAMES]]
//! ```
//!
//! `DIR` holds the pinned outline face; without it only the bitmap face is
//! timed. Each screen is fed to a model the size of the surface's grid and
//! drawn `FRAMES` times after one untimed frame; a time is the median and
//! the fastest frame.

use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use td_ui::face::{Face, Sizing};
use td_ui::font::{self, Font};
use td_ui::pinned_face;
use td_ui::pty::READ_CHUNK;
use td_ui::vt::Terminal;
use td_ui::vt_render::{self, Palette, Snapshot, BYTES_PER_PIXEL};
use td_ui::wayland::backing_file;

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "usage: vt_render_bench [WIDTH HEIGHT [FRAMES]], each at least 1";

const WORDS: [&str; 12] = [
    "fn", "let", "match", "self", "render", "pixels", "width", "Some", "None", "return", "=>",
    "0x7f",
];

/// How much plain text a parse-rate run feeds, and how many runs there are.
const FEED_BYTES: usize = 16 << 20;
const FEED_RUNS: usize = 3;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |at: usize, default: usize| -> Result<usize> {
        let value = args.get(at).map_or(Ok(default), |value| {
            value.parse().map_err(|_| format!("{value:?}: {USAGE}"))
        })?;
        if value == 0 {
            return Err(USAGE.into());
        }
        Ok(value)
    };
    let (width, height, frames) = (number(0, 1920)?, number(1, 1080)?, number(2, 60)?);
    let bytes = width
        .checked_mul(height)
        .and_then(|area| area.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| format!("surface {width}x{height} overflows a byte count"))?;
    let font = font::pinned()?;
    let palette = Palette::pinned();

    println!("surface {width}x{height}, {frames} frames each; times are median / fastest");
    copy_cost(bytes, frames)?;
    for (name, mut face) in faces(&font)? {
        let (cell_width, cell_height) = vt_render::cell_size(&font, face.as_ref());
        let (columns, rows) = (width / cell_width, height / cell_height);
        println!("{name}: cell {cell_width}x{cell_height}, grid {columns}x{rows}");
        if columns == 0 || rows == 0 {
            return Err(format!("surface {width}x{height} holds no whole cell"));
        }
        for (screen, input) in screens(columns, rows) {
            let mut terminal = Terminal::new(rows, columns)?;
            terminal.feed(&input);
            let mut pixels = vec![0u8; bytes];
            let times = timed(frames, || {
                let snapshot = Snapshot::new(&terminal, true, false);
                vt_render::render_with(
                    &snapshot,
                    &palette,
                    &font,
                    face.as_mut(),
                    std::hint::black_box(&mut pixels),
                    width,
                    height,
                )
            })?;
            println!("  render {screen:<8} {}", summary(&times));
        }
    }
    let (columns, rows) = (width / font.width(), height / font.height());
    feed_cost(columns.max(1), rows.max(1))?;
    Ok(())
}

/// The bitmap face alone, then the outline face fitted to the bitmap cell,
/// as td-term starts without `--font-size`, and at an 11 point one.
fn faces(font: &Font) -> Result<Vec<(String, Option<Face>)>> {
    let mut faces = vec![("bitmap".to_string(), None)];
    let Some(dir) = std::env::var_os(pinned_face::SETTING).map(PathBuf::from) else {
        println!(
            "{} unset: the outline face is not timed",
            pinned_face::SETTING
        );
        return Ok(faces);
    };
    let fitted = Sizing::Cell {
        width: font.width(),
        height: font.height(),
    };
    for (name, sizing) in [
        ("outline fitted", fitted),
        ("outline 11pt", Sizing::PixelsPerEm(11.0 * 96.0 / 72.0)),
    ] {
        faces.push((
            name.to_string(),
            Some(pinned_face::styles_from(&dir, sizing)?),
        ));
    }
    Ok(faces)
}

/// What a frame draws: every row filled to its edge with plain text, the
/// same words in dense colour and renditions, and a shell prompt on an
/// otherwise blank screen.
fn screens(columns: usize, rows: usize) -> Vec<(&'static str, Vec<u8>)> {
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    let mut text = Vec::new();
    let mut color = Vec::new();
    for row in 0..rows {
        let mut used = 0;
        while used < columns {
            let word = random.pick(&WORDS);
            let word = word.get(..columns - used).unwrap_or(word);
            let sgr = match random.next() % 4 {
                0 => format!("\x1b[1;38;5;{}m", random.next() % 256),
                1 => format!("\x1b[3;48;5;{}m", random.next() % 256),
                2 => format!("\x1b[4;3{}m", random.next() % 8),
                _ => "\x1b[0m".to_string(),
            };
            text.extend_from_slice(word.as_bytes());
            color.extend_from_slice(sgr.as_bytes());
            color.extend_from_slice(word.as_bytes());
            used += word.len();
            if used < columns {
                text.push(b' ');
                color.push(b' ');
                used += 1;
            }
        }
        if row + 1 < rows {
            text.extend_from_slice(b"\r\n");
            color.extend_from_slice(b"\x1b[0m\r\n");
        }
    }
    let prompt = b"\x1b[1;32mtest@td\x1b[0m:\x1b[1;34m~/src/td\x1b[0m$ ".to_vec();
    vec![("text", text), ("color", color), ("prompt", prompt)]
}

/// The client paints into its own frame and writes the whole frame into the
/// buffer's backing file each present; this is that write, into a file made
/// as the client makes it, in the directory td-term gives it.
fn copy_cost(bytes: usize, frames: usize) -> Result<()> {
    let dir = std::env::temp_dir();
    let file = backing_file(&dir, bytes)?;
    let pixels = vec![0x22u8; bytes];
    let times = timed(frames, || {
        file.write_all_at(&pixels, 0).map_err(|why| why.to_string())
    })?;
    println!("present copy into {} {}", dir.display(), summary(&times));
    Ok(())
}

/// The model's parse rate over plain text in the PTY reader's chunks, the
/// throughput half of a `cat`: lines end in CR LF, as the line discipline's
/// default ONLCR delivers them. The best of a few runs over a fresh model.
fn feed_cost(columns: usize, rows: usize) -> Result<()> {
    let mut random = Random(7);
    let mut bytes = Vec::with_capacity(FEED_BYTES);
    while bytes.len() < FEED_BYTES {
        bytes.extend_from_slice(random.pick(&WORDS).as_bytes());
        if random.next().is_multiple_of(12) {
            bytes.extend_from_slice(b"\r\n");
        } else {
            bytes.push(b' ');
        }
    }
    let mut best = Duration::MAX;
    for _ in 0..FEED_RUNS {
        let mut terminal = Terminal::new(rows, columns)?;
        let start = Instant::now();
        for chunk in bytes.chunks(READ_CHUNK) {
            terminal.feed(chunk);
        }
        best = best.min(start.elapsed());
        std::hint::black_box(&terminal);
    }
    let rate = bytes.len() as f64 / best.as_secs_f64() / f64::from(1 << 20);
    println!("feed plain text into {columns}x{rows}: {rate:.1} MiB/s, best of {FEED_RUNS}");
    Ok(())
}

fn timed(frames: usize, mut frame: impl FnMut() -> Result<()>) -> Result<Vec<Duration>> {
    frame()?;
    let mut times = Vec::with_capacity(frames);
    for _ in 0..frames {
        let start = Instant::now();
        frame()?;
        times.push(start.elapsed());
    }
    times.sort();
    Ok(times)
}

fn summary(times: &[Duration]) -> String {
    let ms = |at: Option<&Duration>| at.map_or(0.0, |time| time.as_secs_f64() * 1e3);
    format!(
        "{:7.3} ms / {:7.3} ms",
        ms(times.get(times.len() / 2)),
        ms(times.first())
    )
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        let count = u64::try_from(from.len()).unwrap_or(1).max(1);
        let at = usize::try_from(self.next() % count).unwrap_or(0);
        from.get(at).copied().unwrap_or("")
    }
}
