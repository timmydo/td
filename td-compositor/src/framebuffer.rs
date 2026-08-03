use crate::scene::Scene;
use crate::{MAX_UI_DIMENSION, MAX_UI_FRAME_BYTES};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_FRAMEBUFFER_BYTES: usize = 64 * 1024 * 1024;

pub struct Framebuffer {
    file: File,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    frame: Vec<u8>,
    #[cfg(test)]
    fail_next_paint: bool,
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
            #[cfg(test)]
            fail_next_paint: false,
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
            fail_next_paint: false,
        })
    }

    #[cfg(test)]
    pub fn fail_next_paint(&mut self) {
        self.fail_next_paint = true;
    }

    pub fn paint(&mut self, scene: &Scene) -> Result<(), String> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_paint) {
            return Err("injected framebuffer paint failure".to_string());
        }
        scene.render(&mut self.frame, self.width, self.height, self.stride);
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("seek framebuffer: {e}"))?;
        self.file
            .write_all(&self.frame)
            .map_err(|e| format!("write framebuffer: {e}"))?;
        self.file
            .flush()
            .map_err(|e| format!("flush framebuffer: {e}"))
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
        framebuffer.paint(&Scene::new()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 160);
        fs::remove_file(path).unwrap();
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
