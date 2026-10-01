//! The disk-selection page is a view over destinations supplied by the
//! installer service. A `Destination` admits wire fields only: rendering it
//! here neither establishes eligibility nor authorizes a write.

use td_install::installation_plan::Destination;
use td_ui::chrome::{Block, Item, List, Status, BLOCK_SCALARS, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

const INSET: usize = CELL_WIDTH;
const LIST_TOP: usize = 4 * ROW;
const LIST_ROWS: usize = 8;
const DETAIL_TOP: usize = 13 * ROW;
const DETAIL_ROWS: usize = 8;
const ROW_LABEL_BYTES: usize = 48;
const MAX_DISKS: usize = 64;

struct Row {
    label: String,
    capacity: String,
}

/// A non-authoritative view of the service's eligible-disk list. `selected`
/// is an index in that list, not a disk claim or consent token.
pub struct DestinationPage {
    surface: Surface,
    list: List,
    rows: Vec<Row>,
    first: usize,
    selected: Option<usize>,
    detail: String,
    detail_page: usize,
    detail_pages: usize,
    status: String,
    footer: Status,
    block: Block,
}

impl DestinationPage {
    /// Build one detail page for a service-supplied list. A missing selection
    /// is valid; an out-of-range selection refuses, while a detail page
    /// past the end is clamped to the last available page.
    pub fn new(
        surface: Surface,
        disks: &[Destination],
        selected: Option<usize>,
        detail_page: usize,
    ) -> Option<Self> {
        Self::new_with_first(surface, disks, selected, 0, detail_page)
    }

    /// Render from a caller-held list position without snapping back on
    /// each turn. `selected` identifies the disk shown in detail.
    pub fn new_with_first(
        surface: Surface,
        disks: &[Destination],
        selected: Option<usize>,
        first: usize,
        detail_page: usize,
    ) -> Option<Self> {
        Self::build(surface, disks, selected, first, detail_page, None)
    }

    /// An absent service is distinct from a service reporting no eligible
    /// disks. This state offers no selectable destination.
    pub fn unavailable(surface: Surface) -> Option<Self> {
        Self::notice(
            surface,
            "The installer service is unavailable. No disk can be selected. Press Escape to return.",
        )
    }

    /// The service has been asked and has not yet answered.
    pub fn waiting(surface: Surface) -> Option<Self> {
        Self::notice(
            surface,
            "Asking the installer service for eligible disks. Press Escape to return.",
        )
    }

    /// The service refused to list disks; `reason` is the installer's own
    /// text for its refusal.
    pub fn refused(surface: Surface, reason: &str) -> Option<Self> {
        Self::notice(
            surface,
            &format!(
                "The installer service refused to list disks: {reason}. No disk can be selected. Press Escape to return."
            ),
        )
    }

    fn notice(surface: Surface, text: &str) -> Option<Self> {
        Self::build(surface, &[], None, 0, 0, Some(text))
    }

    fn build(
        surface: Surface,
        disks: &[Destination],
        selected: Option<usize>,
        first: usize,
        detail_page: usize,
        notice: Option<&str>,
    ) -> Option<Self> {
        crate::supported_page(surface)?;
        let scale = surface.scale.value();
        let overflow = disks.len() > MAX_DISKS;
        if selected.is_some_and(|index| index >= disks.len()) {
            return None;
        }
        let selected = if overflow { None } else { selected };
        let width = surface.width.checked_sub(2 * INSET * scale)?;
        let list = List::new(
            surface,
            Rect {
                x: (INSET * scale) as i64,
                y: (LIST_TOP * scale) as i64,
                width: width as u32,
                height: (LIST_ROWS * ROW * scale) as u32,
            },
        )?;
        let block = Block::new(surface, (DETAIL_TOP * scale) as i64, DETAIL_ROWS)?;
        let footer = Status::new(surface);
        if block.rect().y + i64::from(block.rect().height) > footer.rect().y {
            return None;
        }
        let cells = list.body().width as usize / (CELL_WIDTH * scale);
        let rows = disks
            .iter()
            .take(if overflow { 0 } else { MAX_DISKS })
            .map(|disk| {
                let capacity = capacity(disk.capacity());
                let label = format!("{}  {}", disk.name(), row_model(disk));
                let max_label = cells.saturating_sub(5 + capacity.chars().count());
                Row {
                    label: shorten(&label, max_label),
                    capacity,
                }
            })
            .collect::<Vec<_>>();
        let first = selected.map_or(first.min(rows.len().saturating_sub(list.rows())), |index| {
            list.reveal(rows.len(), index, first)
        });
        // Block stops after BLOCK_SCALARS, including newlines. Keep an
        // eight-row page within that limit even on a very wide surface.
        let columns = block
            .columns()
            .min((BLOCK_SCALARS - (DETAIL_ROWS - 1)) / DETAIL_ROWS);
        let mut lines = Vec::new();
        if let Some(text) = notice {
            push_lines(&mut lines, text, columns);
        } else if overflow {
            push_lines(
                &mut lines,
                "More than 64 eligible disks were reported. Installation cannot continue.",
                columns,
            );
        } else if let Some(disk) = selected.and_then(|index| disks.get(index)) {
            push_identity_lines(disk, columns, &mut lines);
            push_lines(
                &mut lines,
                "Selection alone does not erase this disk.",
                columns,
            );
        } else if disks.is_empty() {
            push_lines(
                &mut lines,
                "No eligible disks were reported. Installation cannot continue.",
                columns,
            );
        } else {
            push_lines(
                &mut lines,
                "Select a disk to inspect its identity before continuing.",
                columns,
            );
        }
        let detail_pages = lines.len().div_ceil(DETAIL_ROWS);
        let detail_page = detail_page.min(detail_pages.saturating_sub(1));
        let start = detail_page.checked_mul(DETAIL_ROWS)?;
        let end = start.saturating_add(DETAIL_ROWS).min(lines.len());
        let detail = lines.get(start..end)?.join("\n");
        let status = if detail_pages == 1 {
            "Destination disk · step 2 of 6".into()
        } else {
            format!(
                "Destination disk · step 2 of 6 · detail {}/{}",
                detail_page + 1,
                detail_pages
            )
        };
        Some(Self {
            surface,
            list,
            rows,
            first,
            selected,
            detail,
            detail_page,
            detail_pages,
            status,
            footer,
            block,
        })
    }

    /// The selected list index, for navigation only. The service must recheck
    /// the destination under its own held claim before any execution.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The first visible disk row, for caller-owned scrolling state.
    pub fn first_visible(&self) -> usize {
        self.first
    }

    /// Current zero-based page and page count for inspecting the complete
    /// escaped identity. Navigation must expose every page before review.
    pub fn detail_position(&self) -> (usize, usize) {
        (self.detail_page, self.detail_pages)
    }
}

fn capacity(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000_000_000_000;
    if bytes < MB - KB / 2 {
        return format!("{} KB", (bytes + KB / 2) / KB);
    }
    if bytes < GB - MB / 2 {
        return format!("{} MB", (bytes + MB / 2) / MB);
    }
    if bytes < TB - GB / 20 {
        return decimal_capacity(bytes, GB, "GB");
    }
    decimal_capacity(bytes, TB, "TB")
}

fn decimal_capacity(bytes: u64, unit: u64, suffix: &str) -> String {
    let tenths = ((bytes % unit) + unit / 20) / (unit / 10);
    format!("{}.{} {suffix}", bytes / unit + tenths / 10, tenths % 10)
}

fn model(disk: &Destination) -> String {
    disk.model()
        .map(label_or_empty)
        .unwrap_or_else(|| "not provided".into())
}

fn row_model(disk: &Destination) -> String {
    disk.model()
        .map(|text| {
            if text.is_empty() {
                "present, empty".into()
            } else {
                format!("value: {}", short_label(text))
            }
        })
        .unwrap_or_else(|| "not provided".into())
}

/// Append the complete escaped disk identity for both destination and review.
pub(crate) fn push_identity_lines(disk: &Destination, columns: usize, lines: &mut Vec<String>) {
    let (major, minor) = disk.number();
    let serial = disk
        .serial()
        .map(label_or_empty)
        .unwrap_or_else(|| "not provided".into());
    let wwid = disk
        .wwid()
        .map(label_or_empty)
        .unwrap_or_else(|| "not provided".into());
    for (line, continuation) in [
        (
            format!(
                "Device: /dev/{} ({}:{}, sequence {})",
                disk.name(),
                major,
                minor,
                disk.sequence()
            ),
            "| Device: ",
        ),
        (
            format!(
                "Capacity: {} bytes; sector: {} bytes",
                disk.capacity(),
                disk.sector()
            ),
            "| Capacity: ",
        ),
        (
            format!(
                "Media: {}",
                if disk.removable() {
                    "removable"
                } else {
                    "fixed"
                }
            ),
            "| Media: ",
        ),
        (format!("Model: {}", model(disk)), "| Model: "),
        (format!("Serial: {serial}"), "| Serial: "),
        (format!("WWID: {wwid}"), "| WWID: "),
    ] {
        push_lines_with(lines, &line, columns, continuation);
    }
}

/// Show untrusted device labels without terminal controls, bidirectional
/// overrides or ambiguous backslash escapes. The input is bounded to 256
/// bytes by `Destination`; this complete rendering can span detail pages.
pub(crate) fn safe_label(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        out.push_str(&escaped(ch));
    }
    out
}

fn label_or_empty(value: &str) -> String {
    if value.is_empty() {
        "present, empty".into()
    } else {
        format!("value: {}", safe_label(value))
    }
}

fn short_label(value: &str) -> String {
    let full = safe_label(value);
    if full.len() <= ROW_LABEL_BYTES {
        return full;
    }
    let mut out = String::new();
    for ch in value.chars() {
        let piece = escaped(ch);
        if out.len() + piece.len() > ROW_LABEL_BYTES - '…'.len_utf8() {
            out.push('…');
            break;
        }
        out.push_str(&piece);
    }
    out
}

fn shorten(value: &str, cells: usize) -> String {
    if value.chars().count() <= cells {
        return value.into();
    }
    if cells == 0 {
        return String::new();
    }
    let mut out = value.chars().take(cells - 1).collect::<String>();
    out.push('…');
    out
}

fn escaped(ch: char) -> String {
    if ch.is_ascii_graphic() && ch != '\\' {
        ch.to_string()
    } else {
        format!("\\u{{{:x}}}", ch as u32)
    }
}

fn push_lines(lines: &mut Vec<String>, text: &str, columns: usize) {
    push_lines_with(lines, text, columns, "| ");
}

/// Wrap a field while repeating its heading on each continuation line.
pub(crate) fn push_lines_with(
    lines: &mut Vec<String>,
    text: &str,
    columns: usize,
    continuation: &str,
) {
    let mut line = String::new();
    let mut used = 0;
    for ch in text.chars() {
        if used >= columns {
            lines.push(std::mem::take(&mut line));
            line.push_str(continuation);
            used = continuation.chars().count();
        }
        line.push(ch);
        used += 1;
    }
    lines.push(line);
}

impl Composition for DestinationPage {
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
        let heading = Rect {
            x: (INSET * scale.value()) as i64,
            y: (ROW * scale.value()) as i64,
            width: self.surface.width.saturating_sub(2 * INSET * scale.value()) as u32,
            height: (CELL_HEIGHT * scale.value()) as u32,
        };
        text_run(
            scale,
            "Choose a destination disk".chars(),
            (heading.x, heading.y),
            heading,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
        let warning = Rect {
            y: (2 * ROW * scale.value()) as i64,
            ..heading
        };
        text_run(
            scale,
            "The selected whole disk will be erased after review and consent.".chars(),
            (warning.x, warning.y),
            warning,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
        self.list.emit(
            self.rows.iter().skip(self.first).map(|row| Item {
                label: &row.label,
                meta: &row.capacity,
                enabled: true,
                marked: false,
            }),
            self.first,
            self.selected.unwrap_or(usize::MAX),
            self.rows.len(),
            damage,
            sink,
        );
        self.block.emit(&self.detail, damage, sink);
        self.footer.emit(self.status.chars(), damage, sink);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_install::installation_plan::DestinationObservation;
    use td_ui::raster::Scale;

    fn surface() -> Surface {
        Surface::new(800, 600, Scale::new(1).unwrap()).unwrap()
    }

    #[test]
    fn untrusted_labels_cannot_emit_controls_or_direction_overrides() {
        let disk = Destination::new(DestinationObservation {
            name: "sda",
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 512_000_000_000,
            sector: 512,
            removable: false,
            model: Some("Fast\u{202e}disk\nA "),
            serial: Some("S\\1"),
            wwid: None,
        })
        .unwrap();
        let page = DestinationPage::new(surface(), &[disk], Some(0), 0).unwrap();
        let mut glyphs = String::new();
        page.emit(surface().bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                glyphs.push(scalar);
            }
        });
        assert!(!glyphs.contains('\u{202e}'));
        assert!(glyphs.contains("\\u{202e}"));
        assert!(glyphs.contains("\\u{a}"));
        assert!(glyphs.contains("\\u{20}"));
        assert!(glyphs.contains("S\\u{5c}1"));
        assert!(glyphs.contains("8:0"));
        assert!(glyphs.contains("sequence 23"));
        assert!(glyphs.contains("512.0 GB"));
    }

    #[test]
    fn empty_and_invalid_selection_cannot_imply_a_disk_choice() {
        let empty = DestinationPage::new(surface(), &[], None, 0).unwrap();
        assert_eq!(empty.selected(), None);
        assert!(empty.detail.contains("cannot continue"));
        let unavailable = DestinationPage::unavailable(surface()).unwrap();
        assert_eq!(unavailable.selected(), None);
        assert!(unavailable.rows.is_empty());
        assert!(unavailable.detail.contains("service is unavailable"));
        assert!(!unavailable
            .detail
            .contains("No eligible disks were reported"));
        // Waiting and a refusal are notices too, never an empty list.
        let waiting = DestinationPage::waiting(surface()).unwrap();
        assert!(waiting.rows.is_empty());
        assert!(waiting.detail.contains("Asking the installer service"));
        let refused =
            DestinationPage::refused(surface(), "the disks could not be examined").unwrap();
        assert_eq!(refused.selected(), None);
        assert!(refused.rows.is_empty());
        assert!(refused.detail.contains("could not be"));
        assert!(!refused.detail.contains("No eligible disks were reported"));
        assert!(DestinationPage::new(surface(), &[], Some(0), 0).is_none());
        let small = Surface::new(640, 480, Scale::new(1).unwrap()).unwrap();
        assert!(DestinationPage::new(small, &[], None, 0).is_none());
        let minimum = Surface::new(752, 480, Scale::new(1).unwrap()).unwrap();
        assert!(DestinationPage::new(minimum, &[], None, 0).is_some());
        let narrower = Surface::new(751, 480, Scale::new(1).unwrap()).unwrap();
        assert!(DestinationPage::new(narrower, &[], None, 0).is_none());
    }

    #[test]
    fn complete_long_identifiers_are_visible_across_bounded_detail_pages() {
        let serial = format!("{}A", "S".repeat(255));
        let wwid = format!("{}B", "W".repeat(255));
        let disk = Destination::new(DestinationObservation {
            name: "sda",
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 512_000_000_000,
            sector: 512,
            removable: false,
            model: Some("M"),
            serial: Some(&serial),
            wwid: Some(&wwid),
        })
        .unwrap();
        let disks = vec![disk];
        let first = DestinationPage::new(surface(), &disks, Some(0), 0).unwrap();
        let (_, pages) = first.detail_position();
        assert!(pages > 1);
        let mut shown = String::new();
        for page in 0..pages {
            let view = DestinationPage::new(surface(), &disks, Some(0), page).unwrap();
            shown.push_str(&view.detail.replace('\n', ""));
            assert!(view.detail.lines().count() <= DETAIL_ROWS);
        }
        let unwrapped = shown
            .replace("| Model: ", "")
            .replace("| Serial: ", "")
            .replace("| WWID: ", "")
            .replace("| ", "");
        assert!(unwrapped.contains(&serial));
        assert!(unwrapped.contains(&wwid));
        let clamped = DestinationPage::new(surface(), &disks, Some(0), pages).unwrap();
        assert_eq!(clamped.detail_position(), (pages - 1, pages));
        let wide = Surface::new(2400, 480, Scale::new(1).unwrap()).unwrap();
        let resized = DestinationPage::new(wide, &disks, Some(0), pages).unwrap();
        assert_eq!(resized.detail_position().0, resized.detail_position().1 - 1);

        // The compositor's 800-pixel output gives the installer a 752-pixel
        // tile. Paint every detail page there, including the long labels.
        let tile = Surface::new(752, 508, Scale::new(1).unwrap()).unwrap();
        let count = DestinationPage::new(tile, &disks, Some(0), 0)
            .unwrap()
            .detail_position()
            .1;
        let font = td_ui::font::pinned().unwrap();
        let mut pixels = vec![0; tile.width * tile.height * 4];
        for index in 0..count {
            let page = DestinationPage::new(tile, &disks, Some(0), index).unwrap();
            td_ui::raster::Raster::new(&mut pixels, &font, tile, tile.width * 4)
                .unwrap()
                .paint(&page, tile.bounds())
                .unwrap();
        }
    }

    #[test]
    fn overflow_refuses_selection_visibly_and_list_scrolls_to_a_later_disk() {
        let disk = Destination::new(DestinationObservation {
            name: "sda",
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 512_128_000_000,
            sector: 512,
            removable: false,
            model: Some("M"),
            serial: Some(""),
            wwid: None,
        })
        .unwrap();
        assert_eq!(capacity(disk.capacity()), "512.1 GB");
        assert_eq!(capacity(50_000_000), "50 MB");
        assert_eq!(capacity(50_900_000), "51 MB");
        assert_eq!(capacity(512), "1 KB");
        assert_eq!(capacity(16_000_000_000_000), "16.0 TB");
        assert_eq!(capacity(999_500), "1 MB");
        assert_eq!(capacity(999_500_000), "1.0 GB");
        assert_eq!(capacity(999_950_000_000), "1.0 TB");
        let disks = vec![disk; 9];
        let later = DestinationPage::new(surface(), &disks, Some(8), 0).unwrap();
        assert_eq!(later.first, 1);
        assert_eq!(
            DestinationPage::new_with_first(surface(), &disks, None, 8, 0)
                .unwrap()
                .first_visible(),
            1
        );
        assert_eq!(
            DestinationPage::new_with_first(surface(), &disks, Some(7), 1, 0)
                .unwrap()
                .first_visible(),
            1
        );
        assert!(later.detail.contains("present, empty"));
        assert!(later.detail.contains("not provided"));
        assert!(DestinationPage::new(surface(), &disks, Some(9), 0).is_none());
        let too_many = vec![disks.first().unwrap().clone(); MAX_DISKS + 1];
        let refused = DestinationPage::new(surface(), &too_many, Some(0), 0).unwrap();
        assert_eq!(refused.selected(), None);
        assert!(refused.detail.contains("More than 64"));
        assert!(DestinationPage::new(surface(), &too_many, Some(MAX_DISKS + 1), 0).is_none());
    }

    #[test]
    fn wide_pages_paint_every_escaped_identifier_character() {
        let label = "\u{1}".repeat(256);
        let disk = Destination::new(DestinationObservation {
            name: "sda",
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 51_200_000,
            sector: 512,
            removable: true,
            model: Some(&label),
            serial: Some(&label),
            wwid: Some(&label),
        })
        .unwrap();
        let surface = Surface::new(1600, 600, Scale::new(1).unwrap()).unwrap();
        let disks = vec![disk];
        let first = DestinationPage::new(surface, &disks, Some(0), 0).unwrap();
        for index in 0..first.detail_pages {
            let page = DestinationPage::new(surface, &disks, Some(0), index).unwrap();
            assert!(page.detail.chars().count() <= BLOCK_SCALARS);
            let mut painted = String::new();
            page.emit(surface.bounds(), &mut |draw| {
                if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                    if y >= page.block.rect().y
                        && y < page.block.rect().y + i64::from(page.block.rect().height)
                    {
                        painted.push(scalar);
                    }
                }
            });
            assert_eq!(painted, page.detail.replace('\n', ""));
        }
    }

    #[test]
    fn missing_and_empty_model_remain_distinct() {
        let observation = |model| DestinationObservation {
            name: "sda",
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 51_200_000,
            sector: 512,
            removable: false,
            model,
            serial: None,
            wwid: None,
        };
        let missing = Destination::new(observation(None)).unwrap();
        let empty = Destination::new(observation(Some(""))).unwrap();
        assert_eq!(model(&missing), "not provided");
        assert_eq!(model(&empty), "present, empty");
        assert_eq!(row_model(&empty), "present, empty");
        for marker in ["not provided", "present, empty"] {
            let supplied = Destination::new(observation(Some(marker))).unwrap();
            let displayed = format!("value: {}", safe_label(marker));
            assert_eq!(model(&supplied), displayed);
            assert_eq!(row_model(&supplied), displayed);
            assert_eq!(label_or_empty(marker), displayed);
        }
    }

    #[test]
    fn wrapped_untrusted_values_cannot_impersonate_field_headings() {
        let model = format!("{}WWID: fake", "A".repeat(84));
        let disk = Destination::new(DestinationObservation {
            name: "sda",
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 51_200_000,
            sector: 512,
            removable: false,
            model: Some(&model),
            serial: None,
            wwid: None,
        })
        .unwrap();
        let page = DestinationPage::new(surface(), &[disk], Some(0), 0).unwrap();
        assert!(page.detail.contains("| Model: WWID:\\u{20}fake"));
        assert!(!page.detail.lines().any(|line| line == "WWID:\\u{20}fake"));
    }

    #[test]
    fn long_kernel_name_keeps_a_visible_row_truncation_marker() {
        let name = "a".repeat(64);
        let model = "M".repeat(256);
        let disk = Destination::new(DestinationObservation {
            name: &name,
            major: 8,
            minor: 0,
            sequence: 23,
            capacity: 51_200_000,
            sector: 512,
            removable: false,
            model: Some(&model),
            serial: None,
            wwid: None,
        })
        .unwrap();
        let page = DestinationPage::new(surface(), &[disk], Some(0), 0).unwrap();
        assert!(page.rows.first().unwrap().label.ends_with('…'));
        let mut painted = String::new();
        page.list.emit(
            page.rows.iter().map(|row| Item {
                label: &row.label,
                meta: &row.capacity,
                enabled: true,
                marked: false,
            }),
            0,
            0,
            1,
            surface().bounds(),
            &mut |draw| {
                if let Primitive::Glyph { scalar, .. } = draw.primitive {
                    painted.push(scalar);
                }
            },
        );
        assert!(painted.contains('…'));
    }
}
