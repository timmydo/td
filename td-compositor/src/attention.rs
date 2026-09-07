use crate::authority::consent::Request;
use crate::ui;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Notice {
    #[default]
    Menu,
    Pending,
    Unlocked,
    Stored,
    NoWrite,
    Enrolled,
    Unenrolled,
    Unavailable,
    Failed,
    Busy,
}

/// Rasterize once, retaining the exact immutable description beside its pixels.
#[derive(Debug)]
pub(crate) struct Prepared {
    request: Request,
    pixels: Vec<u8>,
    geometry: (usize, usize, usize),
}

impl Prepared {
    #[cfg(test)]
    pub fn new(request: Request, width: usize, height: usize, stride: usize) -> Result<Self, String> {
        Self::with_time(request, width, height, stride, None)
    }

    pub fn with_time(
        request: Request,
        width: usize,
        height: usize,
        stride: usize,
        remaining: Option<u64>,
    ) -> Result<Self, String> {
        let length = stride
            .checked_mul(height)
            .ok_or("prompt output size overflow")?;
        if width < 320
            || height < 200
            || stride < width.saturating_mul(4)
            || length > 64 * 1024 * 1024
        {
            return Err("output cannot hold a complete trusted prompt".into());
        }
        let font = crate::font::pinned()?;
        let scale = if width >= 800 && height >= 600 { 2 } else { 1 };
        let cell_width = font
            .width()
            .checked_mul(scale)
            .ok_or("prompt cell overflow")?;
        let cell_height = font
            .height()
            .checked_mul(scale)
            .ok_or("prompt cell overflow")?;
        let columns = width
            .saturating_sub(48)
            .checked_div(cell_width)
            .filter(|value| *value >= 24)
            .ok_or("output cannot hold a complete trusted prompt")?;
        let mut lines = request.lines();
        if let Some(seconds) = remaining {
            if !(1..=120).contains(&seconds) { return Err("invalid trusted operation time budget".into()); }
            let unit = if seconds == 1 { "SECOND" } else { "SECONDS" };
            lines.push(format!("TIME LEFT WHEN SHOWN: {seconds} {unit}"));
        }
        let mut rows = Vec::new();
        for line in &lines {
            if !line.is_ascii() || !line.chars().all(|character| font.covers(character)) {
                return Err("trusted prompt contains an unsupported glyph".into());
            }
            let mut remaining = line.as_str();
            let mut indent = 0;
            while !remaining.is_empty() {
                let count = (columns - indent).min(remaining.len());
                let chunk = remaining
                    .get(..count)
                    .ok_or("invalid trusted prompt text")?;
                rows.push((indent, chunk));
                remaining = remaining
                    .get(count..)
                    .ok_or("invalid trusted prompt text")?;
                indent = 2;
            }
        }
        let row_height = cell_height
            .checked_add(4 * scale)
            .ok_or("prompt row overflow")?;
        let text_height = rows
            .len()
            .checked_mul(row_height)
            .and_then(|height| height.checked_sub(4 * scale))
            .ok_or("prompt height overflow")?;
        if text_height > height.saturating_sub(48) {
            return Err("output cannot hold every trusted prompt argument".into());
        }
        let mut pixels = vec![0; length];
        ui::fill(
            &mut pixels,
            width,
            height,
            stride,
            (0, 0, width, height),
            [0x28, 0x20, 0x18, 0],
        );
        let top = (height - text_height) / 2;
        for (row, (indent, text)) in rows.iter().enumerate() {
            for (column, character) in text.chars().enumerate() {
                let glyph = font.index(character);
                for y in 0..font.height() {
                    for x in 0..font.width() {
                        if font.pixel(glyph, x, y) {
                            ui::fill(
                                &mut pixels,
                                width,
                                height,
                                stride,
                                (
                                    24 + (column + indent) * cell_width + x * scale,
                                    top + row * row_height + y * scale,
                                    scale,
                                    scale,
                                ),
                                [0xff, 0xff, 0xff, 0],
                            );
                        }
                    }
                }
            }
        }
        Ok(Self {
            request,
            pixels,
            geometry: (width, height, stride),
        })
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize) -> bool {
        if self.geometry != (width, height, stride) || frame.len() != self.pixels.len() {
            return false;
        }
        frame.copy_from_slice(&self.pixels);
        true
    }
}

/// Display-only pixels: ordinary scene rendering never calls this painter.
pub(crate) fn paint(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    draining: bool,
    notice: Notice,
) {
    let bounds = (0, 0, width, height);
    ui::fill(frame, width, height, stride, bounds, [0x28, 0x20, 0x18, 0]);
    let top = height.saturating_sub(212) / 2;
    for (index, text) in [
        "TD SECURE ATTENTION",
        if draining {
            "CANCELLING REQUEST"
        } else {
            match notice {
                Notice::Menu => "U: UNLOCK  R: RECOVERY TOKEN",
                Notice::Pending => "PREPARING SECRET REQUEST",
                Notice::Stored => "CREDENTIAL STORED",
                Notice::NoWrite => "NO READY CREDENTIAL WRITE - RUN TD-SECRET SET FIRST",
                Notice::Unlocked => "SECRETS UNLOCKED",
                Notice::Enrolled => "STORE ENROLLED - REOPEN AND PRESS U TO UNLOCK",
                Notice::Unenrolled => "STORE NOT ENROLLED - REOPEN TO TRY AGAIN",
                Notice::Unavailable => "STORE STATE UNAVAILABLE",
                Notice::Failed => "SECRET REQUEST FAILED",
                Notice::Busy => "PREVIOUS REQUEST IS STILL FINISHING",
            }
        },
        if notice == Notice::Menu && !draining { "E: ENROLL TWO TOKENS (HAVE BOTH READY)" } else { "" },
        if notice == Notice::Menu && !draining { "X: ENROLL WITHOUT RECOVERY - LOSS IS FINAL" } else { "" },
        if notice == Notice::Menu && !draining { "W: REVIEW PENDING CREDENTIAL WRITE" } else { "" },
        if draining {
            "RELEASE KEYS AND BUTTONS"
        } else {
            "ESC TO RETURN"
        },
    ]
    .into_iter()
    .enumerate()
    {
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            24,
            top.saturating_add(index.saturating_mul(36)),
            2,
            text,
            [0xff, 0xff, 0xff, 0],
            bounds,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::authority::consent::Operation;

    fn request(name: &str) -> Request {
        Request::new(
            [1; 32],
            1000,
            Operation::Set {
                role: crate::authority::consent::Role::Primary,
                application: "mail".into(),
                name: name.into(),
                application_uid: 65537,
                requester: 1000,
            },
        )
        .unwrap()
    }

    #[test]
    fn complete_prompt_preserves_identifier_case_and_refuses_clipping() {
        let upper = Prepared::new(request("Main"), 800, 600, 3200).unwrap();
        let lower = Prepared::new(request("main"), 800, 600, 3200).unwrap();
        assert_ne!(upper.pixels, lower.pixels);
        assert_eq!(upper.request(), &request("Main"));
        assert!(Prepared::new(request(&"a".repeat(64)), 320, 200, 1280).is_err());
        assert!(Prepared::new(request("main"), 200, 200, 800).is_err());
        assert!(Prepared::new(request("main"), 800, 600, 1).is_err());
        assert!(Prepared::new(request("main"), 800, usize::MAX, 3200).is_err());
        let mut display = vec![0; 800 * 600 * 4];
        assert!(upper.paint(&mut display, 800, 600, 3200));
        assert_eq!(display, upper.pixels);
        let before = display.clone();
        assert!(!upper.paint(&mut display, 799, 600, 3200));
        assert_eq!(display, before);
    }
}
