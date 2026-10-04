//! Pure progress and completion views. The caller must map authenticated
//! service state to these views; a queued job never establishes success.

use td_ui::chrome::{Status, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

const INSET: usize = CELL_WIDTH;
const CONSENT_FOOTER: &str = "Consent \u{b7} step 5 of 6 \u{b7} Escape withdraws the review";
const PROGRESS_FOOTER: &str = "Installation progress \u{b7} step 5 of 6";
const FAILED_FOOTER: &str = "Installation stopped \u{b7} step 5 of 6";
const UNKNOWN_FOOTER: &str = "Outcome unknown \u{b7} step 5 of 6";
const COMPLETE_FOOTER: &str = "Complete \u{b7} step 6 of 6";

/// A bounded operation label supplied by the installation service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    PreparingDisk,
    WritingFilesystems,
    PublishingDeployment,
    ApplyingSettings,
    VerifyingBoot,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::PreparingDisk => "Preparing the destination disk",
            Self::WritingFilesystems => "Writing the filesystems",
            Self::PublishingDeployment => "Publishing the system deployment",
            Self::ApplyingSettings => "Applying account and regional settings",
            Self::VerifyingBoot => "Verifying installed boot artifacts",
        }
    }
}

/// A bounded explanation of why the service stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    DestinationChanged,
    InsufficientSpace,
    WriteFailed,
    VerificationFailed,
    SettingsFailed,
}

impl Failure {
    fn operation(self) -> &'static str {
        match self {
            Self::DestinationChanged => "Checking the destination disk",
            Self::InsufficientSpace => "Checking installation space",
            Self::WriteFailed => "Writing the destination disk",
            Self::VerificationFailed => "Verifying installed boot artifacts",
            Self::SettingsFailed => "Publishing account and regional settings",
        }
    }

    fn explanation(self) -> &'static str {
        match self {
            Self::DestinationChanged => "The selected disk changed or became unavailable.",
            Self::InsufficientSpace => "The disk or installation scratch space was too small.",
            Self::WriteFailed => "The installer could not finish a disk write.",
            Self::VerificationFailed => "The installed boot artifacts failed verification.",
            Self::SettingsFailed => "The account or regional settings could not be published.",
        }
    }
}

/// Progress from the service, or local uncertainty when the installer
/// cannot confirm what the service did.
/// A failed operation does not claim old disk contents can be recovered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    /// The service seeks compositor-owned consent; nothing is written.
    Consent,
    Running(Phase),
    Failed(Failure),
    Unknown,
}

/// One view of consent, an installation in progress or failed, or an
/// outcome the installer cannot confirm.
pub struct ProgressPage {
    surface: Surface,
    progress: Progress,
    footer: Status,
}

impl ProgressPage {
    pub fn new(surface: Surface, progress: Progress) -> Option<Self> {
        supported(surface)?;
        Some(Self {
            surface,
            progress,
            footer: Status::new(surface),
        })
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }
}

impl Composition for ProgressPage {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        fill(self.surface, damage, sink);
        let footer = match self.progress {
            Progress::Consent => {
                row(self.surface, 1, "Waiting for consent", damage, sink);
                row(
                    self.surface,
                    4,
                    "Confirm or decline the installation at the secure prompt.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    6,
                    "Nothing is written to the disk before consent is given there.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    8,
                    "This window cannot give consent.",
                    damage,
                    sink,
                );
                CONSENT_FOOTER
            }
            Progress::Running(phase) => {
                row(self.surface, 1, "Installing td", damage, sink);
                row(self.surface, 4, phase.label(), damage, sink);
                row(
                    self.surface,
                    6,
                    "The installation service is working on the selected disk.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    8,
                    "Keep the destination disk and installation media connected.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    9,
                    "Closing this window cannot restore previous disk contents.",
                    damage,
                    sink,
                );
                PROGRESS_FOOTER
            }
            Progress::Failed(reason) => {
                row(self.surface, 1, "Installation stopped", damage, sink);
                row(self.surface, 3, "Stopped while:", damage, sink);
                row(self.surface, 4, reason.operation(), damage, sink);
                row(self.surface, 6, reason.explanation(), damage, sink);
                row(
                    self.surface,
                    8,
                    "The disk may be incomplete. Do not boot it as an installed system.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    9,
                    "Start a new review before another installation attempt.",
                    damage,
                    sink,
                );
                FAILED_FOOTER
            }
            Progress::Unknown => {
                row(
                    self.surface,
                    1,
                    "Installation outcome unknown",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    4,
                    "The installer cannot confirm what the service did.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    6,
                    "The destination disk state cannot be confirmed.",
                    damage,
                    sink,
                );
                row(
                    self.surface,
                    8,
                    "Recover service status before retrying or rebooting.",
                    damage,
                    sink,
                );
                UNKNOWN_FOOTER
            }
        };
        self.footer.emit(footer.chars(), damage, sink);
    }
}

/// The final screen. The caller may construct it only after the service
/// reports durable filesystem and deployment publication, verified boot
/// artifacts, and settings publication.
pub struct CompletionPage {
    surface: Surface,
    footer: Status,
    notice: Option<&'static str>,
}

impl CompletionPage {
    pub fn new(surface: Surface) -> Option<Self> {
        supported(surface)?;
        Some(Self {
            surface,
            footer: Status::new(surface),
            notice: None,
        })
    }

    /// What became of the restart, under the instruction.
    pub fn with_notice(mut self, notice: Option<&'static str>) -> Self {
        self.notice = notice;
        self
    }
}

impl Composition for CompletionPage {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        fill(self.surface, damage, sink);
        row(self.surface, 1, "Installation complete", damage, sink);
        row(
            self.surface,
            4,
            "The installed system is ready to boot.",
            damage,
            sink,
        );
        row(
            self.surface,
            6,
            "Press Return to restart the computer. Keep the installation media",
            damage,
            sink,
        );
        row(
            self.surface,
            7,
            "in until it has restarted and shows its startup screen, then",
            damage,
            sink,
        );
        row(
            self.surface,
            8,
            "remove it, so that it starts the installed system.",
            damage,
            sink,
        );
        if let Some(notice) = self.notice {
            row(self.surface, 10, notice, damage, sink);
        }
        self.footer.emit(COMPLETE_FOOTER.chars(), damage, sink);
    }
}

fn supported(surface: Surface) -> Option<()> {
    crate::supported_page(surface)
}

fn fill(surface: Surface, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let bounds = surface.bounds();
    if let Some(clip) = bounds.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: bounds,
                color: CHROME,
            },
        });
    }
}

fn row(surface: Surface, index: usize, text: &str, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let scale = surface.scale;
    let inset = (INSET * scale.value()) as i64;
    let y = (index * ROW * scale.value()) as i64;
    let rect = Rect {
        x: inset,
        y,
        width: surface.width.saturating_sub(2 * INSET * scale.value()) as u32,
        height: (CELL_HEIGHT * scale.value()) as u32,
    };
    text_run(
        scale,
        text.chars(),
        (inset, y),
        rect,
        GlyphStyle::medium(INK, CHROME),
        damage,
        sink,
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_ui::raster::Scale;

    fn surface(width: usize, height: usize) -> Surface {
        Surface::new(width, height, Scale::new(1).unwrap()).unwrap()
    }

    fn glyphs(view: &dyn Composition, screen: Surface) -> String {
        let mut text = String::new();
        view.emit(screen.bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                text.push(scalar);
            }
        });
        text
    }

    #[test]
    fn all_progress_states_explain_the_operation_without_claiming_success() {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        for phase in [
            Phase::PreparingDisk,
            Phase::WritingFilesystems,
            Phase::PublishingDeployment,
            Phase::ApplyingSettings,
            Phase::VerifyingBoot,
        ] {
            let running = ProgressPage::new(screen, Progress::Running(phase)).unwrap();
            let painted = glyphs(&running, screen);
            assert!(painted.contains(phase.label()));
            assert!(painted.contains("Closing this window cannot restore"));
            assert!(painted.contains("destination disk and installation media"));
            assert!(painted.contains(PROGRESS_FOOTER));
            assert!(!painted.contains("Installation stopped"));
        }
        for reason in [
            Failure::DestinationChanged,
            Failure::InsufficientSpace,
            Failure::WriteFailed,
            Failure::VerificationFailed,
            Failure::SettingsFailed,
        ] {
            let failed = ProgressPage::new(screen, Progress::Failed(reason)).unwrap();
            let painted = glyphs(&failed, screen);
            assert!(painted.contains(reason.operation()));
            assert!(painted.contains(reason.explanation()));
            assert!(painted.contains("Stopped while:"));
            assert!(painted.contains("disk may be incomplete"));
            assert!(painted.contains("Start a new review"));
            assert!(painted.contains(FAILED_FOOTER));
            assert!(!painted.contains("service is working"));
        }
        let consent = ProgressPage::new(screen, Progress::Consent).unwrap();
        let painted = glyphs(&consent, screen);
        assert!(painted.contains("secure prompt") && painted.contains(CONSENT_FOOTER));
        let unknown = ProgressPage::new(screen, Progress::Unknown).unwrap();
        let painted = glyphs(&unknown, screen);
        assert!(painted.contains("outcome unknown"));
        assert!(painted.contains("disk state cannot be confirmed"));
        assert!(painted.contains(UNKNOWN_FOOTER));
        assert!(!painted.contains("Installing td"));
    }

    #[test]
    fn completion_requires_a_supported_surface_and_instructs_media_removal() {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let complete = CompletionPage::new(screen).unwrap();
        let painted = glyphs(&complete, screen);
        assert!(painted.contains("Installation complete"));
        assert!(painted.contains("Press Return to restart the computer"));
        assert!(painted.contains("Keep the installation media"));
        let notice = glyphs(&complete.with_notice(Some("Restarting")), screen);
        assert!(notice.contains("Restarting"));
        assert!(painted.contains(COMPLETE_FOOTER));
        assert!(
            ProgressPage::new(surface(752, 480), Progress::Running(Phase::PreparingDisk)).is_some()
        );
        assert!(
            ProgressPage::new(surface(751, 480), Progress::Running(Phase::PreparingDisk)).is_none()
        );
        assert!(CompletionPage::new(surface(752, 480)).is_some());
        assert!(CompletionPage::new(surface(752, 479)).is_none());
        let scaled = Surface::new(1504, 960, Scale::new(2).unwrap()).unwrap();
        assert!(ProgressPage::new(scaled, Progress::Running(Phase::PreparingDisk)).is_some());
        assert!(CompletionPage::new(scaled).is_some());
        let too_narrow = Surface::new(1503, 960, Scale::new(2).unwrap()).unwrap();
        assert!(CompletionPage::new(too_narrow).is_none());
    }
}
