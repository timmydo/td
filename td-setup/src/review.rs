//! Pure review of one immutable installation proposal. The service must
//! authenticate the source, hold the disk claim and obtain trusted consent.

use td_install::installation_plan::{Plan, Storage};
use td_ui::chrome::{Block, Status, BLOCK_SCALARS, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::destination::{push_identity_lines, push_lines_with, safe_label};

const INSET: usize = CELL_WIDTH;
const BODY_TOP: usize = 4 * ROW;
const BODY_ROWS: usize = 15;
const WARNING_TOP: usize = BODY_TOP + BODY_ROWS * CELL_HEIGHT + ROW;
/// The warnings for each storage the service's own operand chooses; no
/// request chooses it, so the page says what the review carries.
const UNENCRYPTED: &[&str] = &[
    "All data on the selected disk will be lost after trusted consent.",
    "Storage is not encrypted. The account signs in automatically without a password or PIN.",
];
/// Device-bound storage protects a disk read away from this computer, not
/// a lost one (td-install/ENCRYPTION.md "Device-bound default").
const DEVICE_BOUND: &[&str] = &[
    "All data on the selected disk will be lost after trusted consent.",
    "Storage is encrypted to this computer's TPM; this does not protect a lost computer.",
    "The account signs in automatically. Write down the recovery key shown at the end.",
];
/// The most warning rows any storage shows.
const WARNING_ROWS: usize = 3;

/// The row under `rows` warnings that says what became of an install
/// request.
const fn notice_top(rows: usize) -> usize {
    WARNING_TOP + rows * ROW
}

fn warnings(storage: Storage) -> &'static [&'static str] {
    match storage {
        Storage::Unencrypted => UNENCRYPTED,
        Storage::DeviceBound => DEVICE_BOUND,
    }
}

/// One bounded page of the proposed disk and settings. The page does not
/// authorize installation; every page must be available before consent.
pub struct ReviewPage {
    surface: Surface,
    body: Block,
    detail: String,
    page: usize,
    pages: usize,
    status: String,
    footer: Status,
    notice: Option<&'static str>,
    warnings: &'static [&'static str],
}

impl ReviewPage {
    pub fn new(surface: Surface, plan: &Plan, page: usize) -> Option<Self> {
        crate::supported_page(surface)?;
        let scale = surface.scale.value();
        let body = Block::new(surface, (BODY_TOP * scale) as i64, BODY_ROWS)?;
        let footer = Status::new(surface);
        // The notice row is the lowest, under the warnings.
        if ((notice_top(WARNING_ROWS) + CELL_HEIGHT) * scale) as i64 > footer.rect().y {
            return None;
        }
        let columns = body
            .columns()
            .min((BLOCK_SCALARS - (BODY_ROWS - 1)) / BODY_ROWS);
        let mut lines = Vec::new();
        push_identity_lines(plan.destination(), columns, &mut lines);
        let settings = plan.settings();
        for (line, continuation) in [
            (
                format!("Username: {}", safe_label(settings.username())),
                "| Username: ",
            ),
            (
                format!("Hostname: {}", safe_label(settings.hostname())),
                "| Hostname: ",
            ),
            (
                format!("Keyboard layout: {}", safe_label(settings.keyboard())),
                "| Keyboard layout: ",
            ),
            (
                format!("Time zone: {}", safe_label(settings.timezone())),
                "| Time zone: ",
            ),
        ] {
            push_lines_with(&mut lines, &line, columns, continuation);
        }
        let pages = lines.len().div_ceil(BODY_ROWS);
        if page >= pages {
            return None;
        }
        let start = page.checked_mul(BODY_ROWS)?;
        let end = start.saturating_add(BODY_ROWS).min(lines.len());
        let detail = lines.get(start..end)?.join("\n");
        let status = format!(
            "Review \u{b7} step 4 of 6 \u{b7} detail {}/{} \u{b7} Return to install",
            page + 1,
            pages
        );
        Some(Self {
            surface,
            body,
            detail,
            page,
            pages,
            status,
            footer,
            notice: None,
            warnings: warnings(plan.storage()),
        })
    }

    /// Shows `notice`, the installer's own words, cut to the width under
    /// the warnings.
    pub fn with_notice(mut self, notice: Option<&'static str>) -> Self {
        self.notice = notice;
        self
    }

    /// The current zero-based page and total page count.
    pub fn position(&self) -> (usize, usize) {
        (self.page, self.pages)
    }
}

impl Composition for ReviewPage {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let bounds = self.surface.bounds();
        if let Some(clip) = bounds.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: bounds,
                    color: CHROME,
                },
            });
        }
        let scale = self.surface.scale;
        let inset = (INSET * scale.value()) as i64;
        let heading = Rect {
            x: inset,
            y: (ROW * scale.value()) as i64,
            width: self.surface.width.saturating_sub(2 * INSET * scale.value()) as u32,
            height: (CELL_HEIGHT * scale.value()) as u32,
        };
        for (row, text) in [
            (1, "Review installation"),
            (
                2,
                "Inspect the disk identity and settings before continuing.",
            ),
        ] {
            let rect = Rect {
                y: (row * ROW * scale.value()) as i64,
                ..heading
            };
            text_run(
                scale,
                text.chars(),
                (inset, rect.y),
                rect,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        }
        self.body.emit(&self.detail, damage, sink);
        for (index, warning) in self.warnings.iter().enumerate() {
            let rect = Rect {
                y: ((WARNING_TOP + index * ROW) * scale.value()) as i64,
                ..heading
            };
            text_run(
                scale,
                warning.chars(),
                (inset, rect.y),
                rect,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        }
        if let Some(notice) = self.notice {
            let rect = Rect {
                y: (notice_top(self.warnings.len()) * scale.value()) as i64,
                ..heading
            };
            let columns = (heading.width as usize) / (CELL_WIDTH * scale.value());
            text_run(
                scale,
                notice.chars().take(columns),
                (inset, rect.y),
                rect,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        }
        self.footer.emit(self.status.chars(), damage, sink);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_install::installation_plan::{Destination, DestinationObservation, Settings};
    use td_ui::raster::Scale;

    #[test]
    fn a_notice_is_drawn_under_the_warnings_within_the_width() {
        let surface = td_ui::raster::Surface::new(
            crate::MIN_PAGE_WIDTH,
            crate::MIN_PAGE_HEIGHT,
            td_ui::raster::Scale::new(1).unwrap(),
        )
        .unwrap();
        let long: &'static str =
            Box::leak("trusted consent is unavailable ".repeat(6).into_boxed_str());
        let page = ReviewPage::new(surface, &plan("SERIAL"), 0)
            .unwrap()
            .with_notice(Some(long));
        let mut row = String::new();
        page.emit(surface.bounds(), &mut |draw| {
            if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                if y == notice_top(UNENCRYPTED.len()) as i64 {
                    row.push(scalar);
                }
            }
        });
        assert!(row.starts_with("trusted consent is unavailable"));
        assert!(row.len() < long.len() && long.starts_with(&row));
    }

    fn plan(serial: &str) -> Plan {
        stored(serial, Storage::Unencrypted)
    }

    fn stored(serial: &str, storage: Storage) -> Plan {
        let disk = Destination::new(DestinationObservation {
            name: "nvme0n1",
            major: 259,
            minor: 0,
            sequence: 7,
            capacity: 512_000_000_000,
            sector: 512,
            removable: false,
            model: Some("Fast Disk"),
            serial: Some(serial),
            wwid: Some(serial),
        })
        .unwrap();
        let settings = Settings::new("alice", "tdhost", "us", "America/Los_Angeles").unwrap();
        let mut uuid = [0u8; 16];
        *uuid.get_mut(6).unwrap() = 0x40;
        *uuid.get_mut(8).unwrap() = 0x80;
        Plan::new([1; 32], disk, [2; 32], uuid, storage, settings).unwrap()
    }

    fn surface(width: usize, height: usize) -> Surface {
        Surface::new(width, height, Scale::new(1).unwrap()).unwrap()
    }

    #[test]
    fn review_shows_exact_disk_settings_and_persistent_warning() {
        let serial = "S".repeat(256);
        let plan = plan(&serial);
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let first = ReviewPage::new(screen, &plan, 0).unwrap();
        let (_, pages) = first.position();
        assert!(pages > 1);
        let mut details = String::new();
        for index in 0..pages {
            let view = ReviewPage::new(screen, &plan, index).unwrap();
            assert!(view.detail.chars().count() <= BLOCK_SCALARS);
            details.push_str(&view.detail);
            details.push('\n');
            let mut glyphs = String::new();
            view.emit(screen.bounds(), &mut |draw| {
                if let Primitive::Glyph { scalar, .. } = draw.primitive {
                    glyphs.push(scalar);
                }
            });
            for warning in UNENCRYPTED {
                assert!(glyphs.contains(warning));
            }
            for line in view.detail.lines() {
                assert!(glyphs.contains(line), "unpainted detail: {line}");
            }
        }
        let observed_serial = details
            .lines()
            .filter_map(|line| {
                line.strip_prefix("Serial: value: ")
                    .or_else(|| line.strip_prefix("| Serial: "))
            })
            .collect::<String>();
        assert_eq!(observed_serial, serial);
        let observed_wwid = details
            .lines()
            .filter_map(|line| {
                line.strip_prefix("WWID: value: ")
                    .or_else(|| line.strip_prefix("| WWID: "))
            })
            .collect::<String>();
        assert_eq!(observed_wwid, serial);
        for expected in [
            "nvme0n1",
            "259:0",
            "512000000000",
            "sector: 512 bytes",
            "Media: fixed",
            "Fast\\u{20}Disk",
            "WWID: value: ",
            "alice",
            "tdhost",
            "Keyboard layout: us",
            "America/Los_Angeles",
        ] {
            assert!(details.contains(expected), "missing {expected}");
        }
        assert!(ReviewPage::new(screen, &plan, pages).is_none());
        assert!(ReviewPage::new(surface(752, 480), &plan, 0).is_some());
        assert!(ReviewPage::new(surface(751, 480), &plan, 0).is_none());
        assert!(ReviewPage::new(surface(752, 479), &plan, 0).is_none());
        let scaled = Surface::new(1504, 960, Scale::new(2).unwrap()).unwrap();
        assert!(ReviewPage::new(scaled, &plan, 0).is_some());
        let too_narrow = Surface::new(1503, 960, Scale::new(2).unwrap()).unwrap();
        assert!(ReviewPage::new(too_narrow, &plan, 0).is_none());
    }

    /// A device-bound review never says storage is unencrypted, never
    /// claims to protect a lost computer, and fits as the other does.
    #[test]
    fn a_device_bound_review_says_what_its_storage_is() {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let columns = (crate::MIN_PAGE_WIDTH - 2 * INSET) / CELL_WIDTH;
        for warning in UNENCRYPTED.iter().chain(DEVICE_BOUND) {
            assert!(warning.chars().count() <= columns, "{warning}");
        }
        assert!(UNENCRYPTED.len().max(DEVICE_BOUND.len()) <= WARNING_ROWS);
        let plan = stored("SERIAL", Storage::DeviceBound);
        let view = ReviewPage::new(screen, &plan, 0)
            .unwrap()
            .with_notice(Some("trusted consent is unavailable"));
        let mut glyphs = String::new();
        let mut notice = String::new();
        view.emit(screen.bounds(), &mut |draw| {
            if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                glyphs.push(scalar);
                if y == notice_top(DEVICE_BOUND.len()) as i64 {
                    notice.push(scalar);
                }
            }
        });
        for warning in DEVICE_BOUND {
            assert!(glyphs.contains(warning), "{warning}");
        }
        assert!(!glyphs.contains("not encrypted"));
        assert!(glyphs.contains("does not protect a lost computer"));
        assert_eq!(notice, "trusted consent is unavailable");
        assert!(ReviewPage::new(surface(752, 479), &plan, 0).is_none());
    }
}
