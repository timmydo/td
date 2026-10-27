//! Pure review of one immutable installation proposal. The service must
//! authenticate the source, hold the disk claim and obtain trusted consent.

use td_install::installation_plan::{Basis, Plan, Storage};
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
/// The fixed warnings under the details. Storage follows the service's
/// own probes, which the plan's basis records, unless the person chose
/// unencrypted storage; the page says what the review carries
/// (td-install/INSTALLER.md "Storage choice").
const LOSS: &str = "All data on the selected disk will be lost when you confirm this review.";
const PRESERVE: &[&str] = &[
    "Personal files, account and saved settings will be kept.",
    "The system and boot files will be replaced. Storage stays unencrypted.",
    "If interrupted, boot this USB and run reinstall again.",
];
const AUTOMATIC: &str = "The account signs in automatically without a password or PIN.";
/// Device-bound storage protects a disk read away from this computer, not
/// a lost one (td-install/ENCRYPTION.md "Device-bound default").
const DEVICE_BOUND: &[&str] = &[
    LOSS,
    "Storage is encrypted to this computer's TPM; this does not protect a lost computer.",
    "The account signs in automatically. Keep the recovery key shown at the end.",
];
const UNENCRYPTED_NO_TPM: &[&str] = &[
    LOSS,
    "Storage is not encrypted: this computer has no usable TPM 2.0.",
    AUTOMATIC,
];
const UNENCRYPTED_NO_CONSOLE: &[&str] = &[
    LOSS,
    "Storage is not encrypted: this computer has no keyboard console.",
    AUTOMATIC,
];
const UNENCRYPTED_NEITHER: &[&str] = &[
    LOSS,
    "Storage is not encrypted: no usable TPM 2.0 and no keyboard console.",
    AUTOMATIC,
];
const UNENCRYPTED_CHOSEN: &[&str] = &[
    LOSS,
    "Storage is not encrypted, as you chose: whoever has the disk can read it.",
    AUTOMATIC,
];
/// What the notice row offers when nothing else is said there: the other
/// storage, where the probes allow it.
const OFFER_UNENCRYPTED: &str = "Press U to install without encryption instead.";
const OFFER_ENCRYPTED: &str = "Press E to encrypt the disk to this computer instead.";
/// The most warning rows any storage shows.
const WARNING_ROWS: usize = 3;

/// The device-bound tier's disclosures (td-install/INSTALLER.md "Storage
/// choice"), on the detail pages after the settings; each is a paragraph
/// the detail block wraps.
const DEVICE_BOUND_DETAILS: &[&str] = &[
    "Storage is encrypted to this computer and released at power-on with no \
     interaction. That binds the disk to this computer; it does not \
     authenticate a person: anyone who powers on this computer with its TPM \
     unlocks the disk and reaches the desktop, which the account enters \
     automatically. It protects the disk read away from this computer (a \
     removed drive or a copied image), not a lost or stolen computer.",
    "What the disk held before installation is not erased: blocks the new \
     system has not overwritten keep it, readable to whoever has the disk.",
    "A recovery key follows, as digits and a code a phone can scan. It is \
     shown once and no copy is kept; the installation completes once you say \
     it is kept, and until then the disk does not start. Nothing asks you to \
     type it now; it is typed only to recover the disk. It is the only way \
     to open the disk if this computer's TPM, firmware measurements or \
     startup files change.",
    "Until the installed system first starts, release is not bound to this \
     computer's startup chain: any other system that leaves PCR 12 at zero, \
     td's live medium excepted, can release it, and so can the administrator \
     of an unencrypted td system running on this computer. A one-time \
     firmware boot-menu choice used for that first start can send later \
     ordinary starts to recovery.",
    "When release fails, the recovery prompt appears on the screen and on \
     the serial console. The key's digits are never shown, but a console \
     server or BMC recorder on the serial line may record what is typed.",
    "If this computer's TPM is missing at a start, or reaches the system \
     only after td's startup wait for it, that start goes to recovery, and \
     on a TPM that appears late the running system's administrator can \
     release the disk until the next restart.",
    "The screen and keyboard found now are what recovery will expect. One \
     removed or changed later (a USB keyboard unplugged, a display moved to \
     a graphics card without UEFI GOP or a td driver) can leave recovery on \
     the serial console alone, where the computer has one; the live medium \
     and the recovery key still open the disk. A device that only \
     advertises a keyboard's keys (a security token, a receiver with no \
     keyboard paired, a barcode scanner) counts as a keyboard here.",
    "The account signs in automatically.",
];

/// Every fresh installation carries the live session's saved Wi-Fi
/// networks (td-install/DESIGN.md "Carried Wi-Fi networks"); the page says
/// so whether or not any was joined, since the wizard never sees them.
const WIFI_DEVICE_BOUND: &str = "Wi-Fi networks joined in this session are copied to the \
     installed system with their keys, inside the encrypted storage. To leave one behind, run \
     td-netd wifi forget NAME in a terminal before installing.";
const WIFI_UNENCRYPTED: &str = "Wi-Fi networks joined in this session are copied to the \
     installed system with their keys, which the unencrypted disk holds readably for whoever has \
     it. To leave one behind, run td-netd wifi forget NAME in a terminal before installing.";

/// What an unencrypted review's details say a keyboard console is.
const KEYBOARD_CONSOLE: &str = "A keyboard console is a framebuffer console and a keyboard with \
     Enter and the top-row digits, which the startup recovery prompt needs.";

/// The row under `rows` warnings that says what became of an install
/// request.
const fn notice_top(rows: usize) -> usize {
    WARNING_TOP + rows * ROW
}

/// Pushes `paragraph` after a blank line, word-wrapped to `columns`.
fn push_paragraph(lines: &mut Vec<String>, paragraph: &str, columns: usize) {
    lines.push(String::new());
    lines.extend(td_ui::text::wrap(paragraph, columns));
}

/// Which fixed disclosure set a review shows: the plan's storage, and
/// for unencrypted storage why, the person's choice included. Its tag
/// names it in the evidence line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disclosure {
    DeviceBound,
    NoTpm,
    NoConsole,
    Neither,
    /// Unencrypted although the probes allowed device-bound storage.
    Chosen,
}

impl Disclosure {
    pub fn of(storage: Storage, basis: Basis) -> Self {
        match (storage, basis.tpm(), basis.keyboard_console()) {
            (Storage::DeviceBound, _, _) => Self::DeviceBound,
            (Storage::Unencrypted, true, true) => Self::Chosen,
            (Storage::Unencrypted, false, true) => Self::NoTpm,
            (Storage::Unencrypted, true, false) => Self::NoConsole,
            (Storage::Unencrypted, false, false) => Self::Neither,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::DeviceBound => "device-bound",
            Self::NoTpm => "no-tpm",
            Self::NoConsole => "no-console",
            Self::Neither => "no-tpm-no-console",
            Self::Chosen => "chosen",
        }
    }

    /// The other storage a fresh review offers: unencrypted for a
    /// device-bound one, device-bound again for one unencrypted by choice.
    /// Where the probes failed there is no other.
    fn offer(self) -> Option<&'static str> {
        match self {
            Self::DeviceBound => Some(OFFER_UNENCRYPTED),
            Self::Chosen => Some(OFFER_ENCRYPTED),
            Self::NoTpm | Self::NoConsole | Self::Neither => None,
        }
    }
}

/// The fixed warnings for the plan's storage, and why.
fn warnings(disclosure: Disclosure) -> &'static [&'static str] {
    match disclosure {
        Disclosure::DeviceBound => DEVICE_BOUND,
        Disclosure::NoTpm => UNENCRYPTED_NO_TPM,
        Disclosure::NoConsole => UNENCRYPTED_NO_CONSOLE,
        Disclosure::Neither => UNENCRYPTED_NEITHER,
        Disclosure::Chosen => UNENCRYPTED_CHOSEN,
    }
}

/// The detail row naming the plan's storage.
fn storage_line(disclosure: Disclosure) -> String {
    match disclosure {
        Disclosure::DeviceBound => "Storage: encrypted to this computer".into(),
        Disclosure::Chosen => "Storage: not encrypted, as chosen".into(),
        Disclosure::NoTpm | Disclosure::NoConsole | Disclosure::Neither => {
            "Storage: not encrypted".into()
        }
    }
}

/// The detail row naming what the service's probes found: why the storage
/// is what the row above names.
fn basis_line(basis: Basis) -> String {
    let found = |passed: bool| if passed { "found" } else { "not found" };
    format!(
        "Probes: usable TPM 2.0 {}, keyboard console {}",
        found(basis.tpm()),
        found(basis.keyboard_console()),
    )
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
    /// Said on the notice row while no notice is.
    offer: Option<&'static str>,
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
        let preserve = plan.mode() == td_install::installation_plan::Mode::Preserve;
        let disclosure = Disclosure::of(plan.storage(), plan.basis());
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
            (storage_line(disclosure), "| Storage: "),
            (
                if plan.mode() == td_install::installation_plan::Mode::Preserve {
                    "Mode: reinstall system, keep personal files".into()
                } else {
                    basis_line(plan.basis())
                },
                "| Probes: ",
            ),
        ] {
            push_lines_with(&mut lines, &line, columns, continuation);
        }
        let paragraphs = if plan.mode() == td_install::installation_plan::Mode::Preserve {
            &[][..]
        } else {
            match plan.storage() {
                Storage::DeviceBound => DEVICE_BOUND_DETAILS,
                Storage::Unencrypted if !plan.basis().keyboard_console() => &[KEYBOARD_CONSOLE][..],
                Storage::Unencrypted => &[],
            }
        };
        let wifi = match (plan.mode(), plan.storage()) {
            (td_install::installation_plan::Mode::Preserve, _) => None,
            (_, Storage::DeviceBound) => Some(WIFI_DEVICE_BOUND),
            (_, Storage::Unencrypted) => Some(WIFI_UNENCRYPTED),
        };
        for paragraph in paragraphs.iter().chain(&wifi) {
            push_paragraph(&mut lines, paragraph, columns);
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
            offer: (!preserve).then(|| disclosure.offer()).flatten(),
            warnings: if preserve {
                PRESERVE
            } else {
                warnings(disclosure)
            },
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
        if let Some(notice) = self.notice.or(self.offer) {
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
                if y == notice_top(UNENCRYPTED_NEITHER.len()) as i64 {
                    row.push(scalar);
                }
            }
        });
        assert!(row.starts_with("trusted consent is unavailable"));
        assert!(row.len() < long.len() && long.starts_with(&row));
    }

    fn plan(serial: &str) -> Plan {
        stored(serial, Basis::default())
    }

    fn stored(serial: &str, basis: Basis) -> Plan {
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
        Plan::new([1; 32], disk, [2; 32], uuid, basis, settings).unwrap()
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
            for warning in UNENCRYPTED_NEITHER {
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
            "Storage: not encrypted",
            "Probes: usable TPM 2.0 not found, keyboard console not found",
            "A keyboard console is a framebuffer console",
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

    /// Every page's text, and the glyphs painted for it.
    fn painted(plan: &Plan) -> (String, String) {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let (_, pages) = ReviewPage::new(screen, plan, 0).unwrap().position();
        let (mut details, mut glyphs) = (String::new(), String::new());
        for index in 0..pages {
            let view = ReviewPage::new(screen, plan, index).unwrap();
            assert!(view.detail.chars().count() <= BLOCK_SCALARS);
            assert_eq!(
                view.warnings,
                warnings(Disclosure::of(plan.storage(), plan.basis()))
            );
            let mut page = String::new();
            view.emit(screen.bounds(), &mut |draw| {
                if let Primitive::Glyph { scalar, .. } = draw.primitive {
                    page.push(scalar);
                }
            });
            for line in view.detail.lines() {
                assert!(page.contains(line), "unpainted detail: {line}");
            }
            details.push_str(&view.detail);
            details.push('\n');
            glyphs.push_str(&page);
        }
        (details, glyphs)
    }

    /// A device-bound review never says storage is unencrypted, never
    /// claims to protect a lost computer, carries every one of the tier's
    /// disclosures on its detail pages, and fits as the other does.
    #[test]
    fn a_device_bound_review_says_what_its_storage_is() {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let columns = (crate::MIN_PAGE_WIDTH - 2 * INSET) / CELL_WIDTH;
        for set in [
            DEVICE_BOUND,
            UNENCRYPTED_NO_TPM,
            UNENCRYPTED_NO_CONSOLE,
            UNENCRYPTED_NEITHER,
        ] {
            assert!(set.len() <= WARNING_ROWS);
            for warning in set {
                assert!(warning.chars().count() <= columns, "{warning}");
            }
        }
        let plan = stored("SERIAL", Basis::new(true, true));
        assert_eq!(plan.storage(), Storage::DeviceBound);
        let view = ReviewPage::new(screen, &plan, 0)
            .unwrap()
            .with_notice(Some("trusted consent is unavailable"));
        let mut notice = String::new();
        view.emit(screen.bounds(), &mut |draw| {
            if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                if y == notice_top(DEVICE_BOUND.len()) as i64 {
                    notice.push(scalar);
                }
            }
        });
        assert_eq!(notice, "trusted consent is unavailable");
        let (details, glyphs) = painted(&plan);
        for warning in DEVICE_BOUND {
            assert!(glyphs.contains(warning), "{warning}");
        }
        assert!(!glyphs.contains("not encrypted"));
        assert!(glyphs.contains("does not protect a lost computer"));
        assert!(details.contains("Storage: encrypted to this computer"));
        assert!(details.contains("Probes: usable TPM 2.0 found, keyboard console found"));
        // Each disclosure whole, its wrapped words in order.
        let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let flowed = words(&details);
        for paragraph in DEVICE_BOUND_DETAILS {
            assert!(flowed.contains(&words(paragraph)), "{paragraph}");
        }
        for phrase in [
            "not authenticate a person",
            "not a lost or stolen computer",
            "is not erased",
            "shown once and no copy is kept",
            "Nothing asks you to type it now; it is typed only to recover the disk.",
            "PCR 12 at zero",
            "boot-menu choice",
            "never shown, but a console server or BMC recorder",
            "TPM is missing at a start",
            "counts as a keyboard here",
            "The account signs in automatically.",
        ] {
            assert!(flowed.contains(phrase), "{phrase}");
        }
        assert!(!flowed.contains(&words(KEYBOARD_CONSOLE)));
        assert!(ReviewPage::new(surface(752, 479), &plan, 0).is_none());
    }

    /// Storage follows the basis: each basis names its storage and why on
    /// detail rows, and its warnings say the same.
    #[test]
    fn the_basis_names_the_storage_and_why() {
        for (tpm, console, storage, probes, warning, tag) in [
            (
                false,
                false,
                "Storage: not encrypted",
                "Probes: usable TPM 2.0 not found, keyboard console not found",
                "Storage is not encrypted: no usable TPM 2.0 and no keyboard console.",
                "no-tpm-no-console",
            ),
            (
                true,
                false,
                "Storage: not encrypted",
                "Probes: usable TPM 2.0 found, keyboard console not found",
                "Storage is not encrypted: this computer has no keyboard console.",
                "no-console",
            ),
            (
                false,
                true,
                "Storage: not encrypted",
                "Probes: usable TPM 2.0 not found, keyboard console found",
                "Storage is not encrypted: this computer has no usable TPM 2.0.",
                "no-tpm",
            ),
            (
                true,
                true,
                "Storage: encrypted to this computer",
                "Probes: usable TPM 2.0 found, keyboard console found",
                "Storage is encrypted to this computer's TPM; this does not protect a lost computer.",
                "device-bound",
            ),
        ] {
            let basis = Basis::new(tpm, console);
            assert_eq!(Disclosure::of(basis.storage(), basis).tag(), tag);
            let plan = stored("SERIAL", basis);
            let (details, glyphs) = painted(&plan);
            assert!(details.lines().any(|line| line == storage), "{storage}");
            assert!(details.lines().any(|line| line == probes), "{probes}");
            assert!(glyphs.contains(warning), "{warning}");
            assert!(glyphs.contains("signs in automatically"));
            // Only a review without a keyboard console says what one is.
            let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert_eq!(
                words(&details).contains(&words(KEYBOARD_CONSOLE)),
                !console
            );
            // Only a review the probes allow to encrypt offers unencrypted.
            assert_eq!(glyphs.contains(OFFER_UNENCRYPTED), tpm && console);
            assert!(!glyphs.contains(OFFER_ENCRYPTED));
        }
    }

    /// Unencrypted by choice, with both probes: said as chosen, readable
    /// to whoever has the disk, and offering encryption again; a notice
    /// takes the offer's row while it is said. A reinstall offers
    /// nothing.
    #[test]
    fn a_review_unencrypted_by_choice_says_so_and_offers_encryption() {
        let basis = Basis::new(true, true);
        let plan = stored("SERIAL", basis).unencrypted();
        assert_eq!(
            Disclosure::of(plan.storage(), plan.basis()),
            Disclosure::Chosen
        );
        assert_eq!(Disclosure::Chosen.tag(), "chosen");
        let (details, glyphs) = painted(&plan);
        assert!(details
            .lines()
            .any(|line| line == "Storage: not encrypted, as chosen"));
        assert!(glyphs.contains("Storage is not encrypted, as you chose"));
        assert!(glyphs.contains(OFFER_ENCRYPTED) && !glyphs.contains(OFFER_UNENCRYPTED));
        let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(words(&details).contains(&words(WIFI_UNENCRYPTED)));
        assert!(!words(&details).contains(&words(DEVICE_BOUND_DETAILS.first().unwrap())));
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let noticed = ReviewPage::new(screen, &plan, 0)
            .unwrap()
            .with_notice(Some("Starting the confirmed installation"));
        let mut said = String::new();
        noticed.emit(screen.bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                said.push(scalar);
            }
        });
        assert!(said.contains("Starting the confirmed installation"));
        assert!(!said.contains(OFFER_ENCRYPTED));
        for warning in UNENCRYPTED_CHOSEN
            .iter()
            .chain([&OFFER_ENCRYPTED, &OFFER_UNENCRYPTED])
        {
            assert!(
                warning.chars().count() <= (crate::MIN_PAGE_WIDTH - 2 * INSET) / CELL_WIDTH,
                "{warning}"
            );
        }
        let preserved = stored("SERIAL", Basis::default()).preserving();
        let mut glyphs = String::new();
        ReviewPage::new(screen, &preserved, 0)
            .unwrap()
            .emit(screen.bounds(), &mut |draw| {
                if let Primitive::Glyph { scalar, .. } = draw.primitive {
                    glyphs.push(scalar);
                }
            });
        assert!(glyphs.contains("Personal files"));
        assert!(!glyphs.contains(OFFER_ENCRYPTED) && !glyphs.contains(OFFER_UNENCRYPTED));
    }
}
