use crate::output::{
    Damage, Fourcc, FrameTarget, Output, OutputBackend, OutputDimensions, OutputEvent, OutputId,
    OutputScale, OutputTransform, Submission, DRM_FORMAT_XRGB8888,
};
use crate::scene::{Scene, SurfaceKey};
use crate::{MAX_UI_DIMENSION, MAX_UI_FRAME_BYTES};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_FRAMEBUFFER_BYTES: usize = 64 * 1024 * 1024;

/// `open` refuses anything but 32 bits per pixel and writes XRGB rows, so
/// this backend can scan out exactly one format.
const FBDEV_FORMATS: [Fourcc; 1] = [DRM_FORMAT_XRGB8888];

/// One paint in every this many is a full write, so pixels the compositor did
/// not put there -- fbcon still owns the VT -- cannot outlive the interval.
const RESEND_INTERVAL: usize = 240;

pub struct Framebuffer {
    file: File,
    // Private: the output's size is `output().dimensions`, which is the
    // trait's question and not this backend's field — `dimensions()` is a
    // view of that, not a second home for it. A caller that reached the
    // field would be reading fbdev rather than an output, which is the
    // coupling the split exists to remove.
    width: usize,
    height: usize,
    stride: usize,
    frame: Vec<u8>,
    comparison: Vec<u8>,
    // What the device is believed to hold. `resend_all` says that belief is
    // unfounded, so the next paint writes the whole image rather than a band.
    written: Vec<u8>,
    resend_all: bool,
    since_resend: usize,
    #[cfg(test)]
    writes: Vec<(u64, usize)>,
    #[cfg(test)]
    fail_next_paint: bool,
    #[cfg(test)]
    fail_next_write: bool,
}

fn parse_number(path: &Path) -> Result<usize, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    text.trim().parse::<usize>().map_err(|_| {
        format!(
            "{} contains invalid number '{}'",
            path.display(),
            text.trim()
        )
    })
}

fn parse_virtual_size(path: &Path) -> Result<(usize, usize), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let (width, height) = text
        .trim()
        .split_once(',')
        .ok_or_else(|| format!("{} is not WIDTH,HEIGHT", path.display()))?;
    let width = width
        .parse::<usize>()
        .map_err(|_| format!("{} has invalid width", path.display()))?;
    let height = height
        .parse::<usize>()
        .map_err(|_| format!("{} has invalid height", path.display()))?;
    Ok((width, height))
}

fn validate_geometry(width: usize, height: usize, stride: usize) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err("framebuffer dimensions must be non-zero".into());
    }
    if width > MAX_UI_DIMENSION || height > MAX_UI_DIMENSION {
        return Err(format!(
            "framebuffer {width}x{height} exceeds the {MAX_UI_DIMENSION}-pixel dimension limit"
        ));
    }
    let row = width
        .checked_mul(4)
        .ok_or_else(|| "framebuffer row size overflow".to_string())?;
    if stride < row {
        return Err(format!(
            "framebuffer stride {stride} is smaller than {width} XRGB pixels"
        ));
    }
    let pixels = row
        .checked_mul(height)
        .ok_or_else(|| "framebuffer pixel size overflow".to_string())?;
    if pixels > MAX_UI_FRAME_BYTES {
        return Err(format!(
            "framebuffer pixels need {pixels} bytes, above the {MAX_UI_FRAME_BYTES}-byte client limit"
        ));
    }
    let size = stride
        .checked_mul(height)
        .ok_or_else(|| "framebuffer size overflow".to_string())?;
    if size > MAX_FRAMEBUFFER_BYTES {
        return Err(format!(
            "framebuffer needs {size} bytes, above the {MAX_FRAMEBUFFER_BYTES}-byte shadow limit"
        ));
    }
    Ok(size)
}

/// First and last rows whose bytes differ, or `None` when the image is
/// unchanged. Rows, not rectangles: a band is one contiguous write, where
/// per-row column spans would be one `seek`+`write` pair each and the syscalls
/// would cost more than the bytes they saved.
fn damaged_rows(written: &[u8], frame: &[u8], stride: usize) -> Option<(usize, usize)> {
    if stride == 0 {
        return None;
    }
    let rows = frame.len() / stride;
    if written.len() != frame.len() {
        return rows.checked_sub(1).map(|last| (0, last));
    }
    let pairs = || frame.chunks_exact(stride).zip(written.chunks_exact(stride));
    let first = pairs().position(|(current, previous)| current != previous)?;
    let last = pairs()
        .rposition(|(current, previous)| current != previous)
        .unwrap_or(first);
    Some((first, last))
}

fn attributed_rgb_counts(
    rendered: &[u8],
    omitted: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    rgbs: [[u8; 3]; 2],
) -> [usize; 2] {
    let mut counts = [0usize; 2];
    let visible = width.saturating_mul(4);
    for (rendered_row, omitted_row) in rendered
        .chunks(stride)
        .zip(omitted.chunks(stride))
        .take(height)
    {
        let (Some(rendered_pixels), Some(omitted_pixels)) = (
            rendered_row.get(..visible),
            omitted_row.get(..visible),
        ) else {
            return [0; 2];
        };
        for (pixel, without) in rendered_pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(omitted_pixels.as_chunks::<4>().0)
        {
            if pixel == without {
                continue;
            }
            let [blue, green, red, _] = pixel;
            let rgb = [*red, *green, *blue];
            for (index, expected) in rgbs.iter().enumerate() {
                if rgb == *expected {
                    if let Some(count) = counts.get_mut(index) {
                        *count = count.saturating_add(1);
                    }
                }
            }
        }
    }
    counts
}

impl Framebuffer {
    pub fn open(path: &Path) -> Result<Framebuffer, String> {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("framebuffer path {} has no UTF-8 basename", path.display()))?;
        let sys = PathBuf::from("/sys/class/graphics").join(name);
        let (width, height) = parse_virtual_size(&sys.join("virtual_size"))?;
        let stride = parse_number(&sys.join("stride"))?;
        let bits = parse_number(&sys.join("bits_per_pixel"))?;
        if bits != 32 {
            return Err(format!(
                "{} is {bits} bits per pixel; td supports only XRGB8888",
                path.display()
            ));
        }
        let size = validate_geometry(width, height, stride)?;
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| format!("open framebuffer {}: {e}", path.display()))?;
        Ok(Framebuffer {
            file,
            width,
            height,
            stride,
            frame: vec![0; size],
            comparison: Vec::new(),
            written: vec![0; size],
            resend_all: true,
            since_resend: 0,
            #[cfg(test)]
            writes: Vec::new(),
            #[cfg(test)]
            fail_next_paint: false,
            #[cfg(test)]
            fail_next_write: false,
        })
    }

    #[cfg(test)]
    pub fn test_file(
        path: &Path,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<Framebuffer, String> {
        let size = validate_geometry(width, height, stride)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("create test framebuffer {}: {e}", path.display()))?;
        file.set_len(u64::try_from(size).map_err(|_| "test framebuffer is too large".to_string())?)
            .map_err(|e| format!("size test framebuffer: {e}"))?;
        Ok(Framebuffer {
            file,
            width,
            height,
            stride,
            frame: vec![0; size],
            comparison: Vec::new(),
            written: vec![0; size],
            resend_all: true,
            since_resend: 0,
            writes: Vec::new(),
            fail_next_paint: false,
            fail_next_write: false,
        })
    }

    #[cfg(test)]
    pub fn fail_next_paint(&mut self) {
        self.fail_next_paint = true;
    }

    /// Disarm one that was never consumed, so a test can prove a path took
    /// NO paint rather than only that it survived one.
    #[cfg(test)]
    pub fn clear_paint_failure(&mut self) {
        self.fail_next_paint = false;
    }

    /// Fail the next write after the shadow copy has been marked untrustworthy.
    #[cfg(test)]
    pub fn fail_next_write(&mut self) {
        self.fail_next_write = true;
    }

    /// Offset and length of every write this framebuffer has issued.
    #[cfg(test)]
    pub fn take_writes(&mut self) -> Vec<(u64, usize)> {
        std::mem::take(&mut self.writes)
    }

    /// The device's row pitch. fbdev's own, reported by the startup
    /// diagnostic: a dumb buffer's pitch is the kernel's to choose, so this
    /// is a property of THIS backend's memory rather than of the output, and
    /// it is not on the trait for that reason.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Exact final-output pixels attributable to `surface`. Re-rendering with
    /// only that surface omitted makes hidden, clipped and occluded pixels
    /// disappear from the count while leaving its descendants in place.
    pub(crate) fn surface_rgb_pixel_counts(
        &mut self,
        scene: &Scene,
        surface: SurfaceKey,
        rgbs: [[u8; 3]; 2],
    ) -> Result<[usize; 2], String> {
        if self.comparison.len() != self.frame.len() {
            let additional = self.frame.len().saturating_sub(self.comparison.len());
            self.comparison
                .try_reserve_exact(additional)
                .map_err(|_| "reserve application comparison frame".to_string())?;
            self.comparison.resize(self.frame.len(), 0);
        }
        scene.render_omitting(
            &mut self.comparison,
            self.width,
            self.height,
            self.stride,
            Some(surface),
        );
        Ok(attributed_rgb_counts(
            &self.frame,
            &self.comparison,
            self.width,
            self.height,
            self.stride,
            rgbs,
        ))
    }

    #[cfg(test)]
    pub(crate) fn rgb_pixel_counts_for_test(&self, rgbs: [[u8; 3]; 2]) -> [usize; 2] {
        let omitted = vec![0; self.frame.len()];
        attributed_rgb_counts(
            &self.frame,
            &omitted,
            self.width,
            self.height,
            self.stride,
            rgbs,
        )
    }
}

impl OutputBackend for Framebuffer {
    /// fbdev has one device, no mode list and no connector to ask, so its
    /// answer is fixed — and fixed is the point: it is reported from one
    /// place instead of being spelled as literals wherever the wire needs it.
    /// `dimensions` is the trait's view of this, not a second answer.
    fn output(&self) -> Output {
        Output {
            id: OutputId::FIRST,
            dimensions: OutputDimensions {
                width: self.width,
                height: self.height,
            },
            scale: OutputScale::ONE,
            transform: OutputTransform::Normal,
        }
    }

    fn supported_formats(&self) -> &[Fourcc] {
        &FBDEV_FORMATS
    }

    fn begin_frame(&mut self, damage: Damage) -> Result<FrameTarget<'_>, String> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_paint) {
            return Err("injected framebuffer paint failure".to_string());
        }
        // The caller's `Whole` is this backend's shadow-copy distrust: it says
        // the device may hold pixels the compositor did not write, which is
        // the same thing a failed write leaves behind.
        if matches!(damage, Damage::Whole) {
            self.resend_all = true;
        }
        Ok(FrameTarget {
            pixels: &mut self.frame,
            width: self.width,
            height: self.height,
            stride: self.stride,
        })
    }

    /// fbdev's submit completes while it runs: `write` returns when the bytes
    /// are the device's, and there is no later event to wait for. So this
    /// answers `Presented` — the only backend that will.
    fn present(&mut self) -> Result<Submission, String> {
        let rows = self.frame.len().checked_div(self.stride).unwrap_or(0);
        let full = self.resend_all || self.since_resend.saturating_add(1) >= RESEND_INTERVAL;
        let band = if full {
            rows.checked_sub(1).map(|last| (0, last))
        } else {
            damaged_rows(&self.written, &self.frame, self.stride)
        };
        let Some((first, last)) = band else {
            return Ok(Submission::Presented);
        };
        let start = first
            .checked_mul(self.stride)
            .ok_or_else(|| "framebuffer damage offset overflow".to_string())?;
        let end = last
            .checked_add(1)
            .and_then(|rows| rows.checked_mul(self.stride))
            .ok_or_else(|| "framebuffer damage extent overflow".to_string())?;
        let band = self
            .frame
            .get(start..end)
            .ok_or_else(|| format!("framebuffer damage {start}..{end} is outside the frame"))?;
        let at =
            u64::try_from(start).map_err(|_| "framebuffer damage offset overflow".to_string())?;
        // Pessimistic across the write: a partial or failed one leaves the
        // device holding something no shadow copy describes.
        self.resend_all = true;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_write) {
            return Err("injected framebuffer write failure".to_string());
        }
        self.file
            .seek(SeekFrom::Start(at))
            .map_err(|e| format!("seek framebuffer: {e}"))?;
        self.file
            .write_all(band)
            .map_err(|e| format!("write framebuffer: {e}"))?;
        self.file
            .flush()
            .map_err(|e| format!("flush framebuffer: {e}"))?;
        self.written
            .get_mut(start..end)
            .ok_or_else(|| format!("framebuffer shadow {start}..{end} is outside the image"))?
            .copy_from_slice(band);
        self.resend_all = false;
        self.since_resend = if full {
            0
        } else {
            self.since_resend.saturating_add(1)
        };
        #[cfg(test)]
        self.writes.push((at, band.len()));
        Ok(Submission::Presented)
    }

    /// fbdev originates nothing: there is no page flip to complete and no
    /// hotplug to report, so this appends nothing rather than the method
    /// being absent.
    fn poll_events(&mut self, _events: &mut Vec<OutputEvent>) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn test_framebuffer_is_exactly_stride_times_height() {
        let path = std::env::temp_dir().join(format!(
            "td-framebuffer-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut framebuffer = Framebuffer::test_file(&path, 8, 4, 40).unwrap();
        framebuffer.paint(&Scene::new(), Damage::Unknown).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 160);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn application_color_counts_exclude_stride_padding() {
        let path = std::env::temp_dir().join(format!(
            "td-framebuffer-color-count-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let rendered = [0xff, 0x00, 0xff, 0, 0x00, 0xff, 0x00, 0, 0x00, 0xff, 0x00, 0];
        let mut omitted = [0u8; 12];
        let colors = [[0xff, 0x00, 0xff], [0x00, 0xff, 0x00]];
        assert_eq!(
            attributed_rgb_counts(&rendered, &omitted, 2, 1, 12, colors),
            [1, 1]
        );
        omitted[4..8].copy_from_slice(&rendered[4..8]);
        assert_eq!(
            attributed_rgb_counts(&rendered, &omitted, 2, 1, 12, colors),
            [1, 0]
        );
        let framebuffer = Framebuffer::test_file(&path, 2, 1, 12).unwrap();
        drop(framebuffer);
        fs::remove_file(path).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "td-framebuffer-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn damaged_rows_reports_the_band_that_changed() {
        let unchanged = vec![7u8; 24];
        assert_eq!(damaged_rows(&unchanged, &unchanged, 4), None);
        let mut moved = unchanged.clone();
        moved[9] = 0;
        assert_eq!(damaged_rows(&unchanged, &moved, 4), Some((2, 2)));
        moved[17] = 0;
        assert_eq!(damaged_rows(&unchanged, &moved, 4), Some((2, 4)));
        moved[0] = 0;
        assert_eq!(damaged_rows(&unchanged, &moved, 4), Some((0, 4)));
        // A shadow of the wrong size describes nothing, so everything is owed.
        assert_eq!(damaged_rows(&[], &unchanged, 4), Some((0, 5)));
        assert_eq!(damaged_rows(&[], &[], 4), None);
        assert_eq!(damaged_rows(&unchanged, &unchanged, 0), None);
    }

    #[test]
    fn an_unchanged_scene_writes_nothing_and_a_moved_pointer_writes_its_band() {
        let cleanup = Cleanup(scratch("damage"));
        let mut framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 480).unwrap();
        let mut scene = Scene::new();
        scene.move_pointer(40, 40, 120, 80);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![(0, 480 * 80)]);

        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![]);

        // The cursor is a 13-row cross, so one pixel sideways is 13 rows.
        scene.move_pointer(1, 0, 120, 80);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![(34 * 480, 13 * 480)]);

        // One pixel down extends the band by the row it left and the row it
        // reached, and still costs a fraction of the image.
        scene.move_pointer(0, 1, 120, 80);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![(34 * 480, 14 * 480)]);
    }

    #[test]
    fn the_damaged_band_leaves_the_device_holding_the_whole_image() {
        let cleanup = Cleanup(scratch("band-equals-full"));
        let mut banded = Framebuffer::test_file(&cleanup.0, 40, 24, 160).unwrap();
        let whole = Cleanup(scratch("band-equals-full-reference"));
        let mut reference = Framebuffer::test_file(&whole.0, 40, 24, 160).unwrap();
        let mut scene = Scene::new();
        scene.move_pointer(20, 12, 40, 24);
        banded.paint(&scene, Damage::Unknown).unwrap();
        for step in 0..7 {
            scene.move_pointer(1, i32::from(step % 2 == 0), 40, 24);
            banded.paint(&scene, Damage::Unknown).unwrap();
        }
        // The reference has only ever seen the final scene, as one full write.
        reference.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(reference.take_writes(), vec![(0, 160 * 24)]);
        assert!(banded.take_writes().len() > 1);
        assert_eq!(fs::read(&cleanup.0).unwrap(), fs::read(&whole.0).unwrap());
    }

    /// fbcon can draw on the same device, and the shadow copy cannot see it.
    /// Before damage tracking every paint healed that; this bounds it instead.
    #[test]
    fn a_full_image_is_resent_at_least_every_interval() {
        let cleanup = Cleanup(scratch("interval"));
        let full = 480 * 80;
        let mut framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 480).unwrap();
        let mut scene = Scene::new();
        scene.move_pointer(10, 40, 120, 80);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![(0, full)]);

        let mut banded = 0;
        let mut resends = 0;
        for step in 0..RESEND_INTERVAL {
            // Bounce, so every paint has something to write. A screen nothing
            // changes never writes at all, and so never spends the interval.
            scene.move_pointer(if step % 2 == 0 { 1 } else { -1 }, 0, 120, 80);
            framebuffer.paint(&scene, Damage::Unknown).unwrap();
            for (at, length) in framebuffer.take_writes() {
                if (at, length) == (0, full) {
                    resends += 1;
                } else {
                    banded += 1;
                }
            }
        }
        assert_eq!(resends, 1);
        assert_eq!(banded, RESEND_INTERVAL - 1);
    }

    /// The complaint this exists for: at the first supported output size, a
    /// pointer that moved one pixel used to cost a whole 8 MiB image.
    #[test]
    fn a_pointer_step_costs_a_fraction_of_a_full_size_output() {
        let cleanup = Cleanup(scratch("full-size"));
        let stride = 1920 * 4;
        let full = stride * 1080;
        let mut framebuffer = Framebuffer::test_file(&cleanup.0, 1920, 1080, stride).unwrap();
        let mut scene = Scene::new();
        scene.move_pointer(960, 540, 1920, 1080);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![(0, full)]);

        scene.move_pointer(1, 1, 1920, 1080);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        let written: usize = framebuffer
            .take_writes()
            .iter()
            .map(|(_, length)| *length)
            .sum();
        assert_eq!(written, 14 * stride);
        assert!(
            written * 50 < full,
            "{written} bytes is not under 2% of {full}"
        );
    }

    #[test]
    fn a_failed_write_resends_the_whole_image_even_though_the_scene_is_the_same() {
        let cleanup = Cleanup(scratch("resend"));
        let mut framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 480).unwrap();
        let mut scene = Scene::new();
        scene.move_pointer(40, 40, 120, 80);
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        framebuffer.take_writes();

        scene.move_pointer(1, 0, 120, 80);
        framebuffer.fail_next_write();
        assert!(framebuffer.paint(&scene, Damage::Unknown).is_err());
        assert_eq!(framebuffer.take_writes(), vec![]);

        // Same scene: a shadow trusted after a failed write would write nothing
        // here and leave the stale image on the device forever.
        framebuffer.paint(&scene, Damage::Unknown).unwrap();
        assert_eq!(framebuffer.take_writes(), vec![(0, 480 * 80)]);
    }

    #[test]
    fn invalid_geometry_is_rejected() {
        assert!(validate_geometry(0, 1, 4).is_err());
        assert!(validate_geometry(10, 1, 39).is_err());
        assert!(validate_geometry(usize::MAX, 2, usize::MAX).is_err());
        assert!(validate_geometry(MAX_UI_DIMENSION + 1, 1, (MAX_UI_DIMENSION + 1) * 4).is_err());
        assert_eq!(
            validate_geometry(4096, 2048, 4096 * 4),
            Ok(MAX_UI_FRAME_BYTES)
        );
        assert_eq!(
            validate_geometry(4096, 2048, 4096 * 4 + 256),
            Ok(MAX_UI_FRAME_BYTES + 2048 * 256)
        );
        assert!(validate_geometry(4096, 2049, 4096 * 4).is_err());
        assert!(validate_geometry(1, 1, MAX_FRAMEBUFFER_BYTES + 1).is_err());
    }
}
