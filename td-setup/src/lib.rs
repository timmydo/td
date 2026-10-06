#![forbid(unsafe_code)]

//! td-setup: the offline graphical installer's front end, the second
//! `td_ui` consumer after td-editor. It will present the installation
//! wizard as a dependency-free Rust Wayland client over the shared
//! toolkit's raster and chrome bands, following td-install/INSTALLER.md.
//!
//! The `window` turn loop presents welcome, then asks the installation
//! service (`service`, through td-authd's setup intake) for eligible disks
//! and lists them, or shows why it cannot; a selected disk leads to the
//! settings form, whose time zones the service supplies, and the completed
//! form to the service's review of it; from the review it asks for trusted
//! consent and follows the installation to its outcome, through a
//! device-bound installation's recovery key (`recovery`), shown once and
//! typed back. The privileged disk writer stays in td-install; this front
//! end holds no disk-writing authority (INSTALLER.md).

pub mod destination;
pub mod evidence;
pub mod outcome;
pub mod recovery;
pub mod review;
pub mod service;
pub mod settings;
pub mod welcome;
pub mod window;

use td_install::installation_plan::{
    Basis, Destination, DestinationObservation, Plan, Settings, Storage,
};
use td_ui::raster::{Composition, Primitive, Raster, Scale, Surface};
use welcome::Welcome;

/// Smallest common installer view within the compositor's 800x600 output.
pub(crate) const MIN_PAGE_WIDTH: usize = 752;
/// Minimum height shared by installer pages.
pub(crate) const MIN_PAGE_HEIGHT: usize = 480;

pub(crate) fn supported_page(surface: Surface) -> Option<()> {
    surface.check().ok()?;
    let scale = surface.scale.value();
    let width = MIN_PAGE_WIDTH.checked_mul(scale)?;
    let height = MIN_PAGE_HEIGHT.checked_mul(scale)?;
    (surface.width >= width && surface.height >= height).then_some(())
}

/// Paint every installer view at reference and live-tile sizes using bounded,
/// synthetic data. This checks the realized target renderer; it supplies no
/// service state, consent, or installation authority.
pub fn render_check() -> Result<(), String> {
    let font = td_ui::font::pinned()?;
    for (width, height) in [(800, 600), (752, 508)] {
        let surface = Surface::new(width, height, Scale::new(1).map_err(|e| format!("{e:?}"))?)
            .map_err(|e| format!("{e:?}"))?;
        render_check_surface(&font, surface)
            .map_err(|error| format!("render check at {width}x{height}: {error}"))?;
    }
    Ok(())
}

fn render_check_surface(font: &td_ui::font::Font, surface: Surface) -> Result<(), String> {
    let mut pixels = vec![0u8; surface.width * surface.height * 4];
    let mut paint = |page: &dyn Composition| -> Result<(), String> {
        let mut missing = None;
        let mut glyphs = 0usize;
        page.emit(surface.bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                glyphs += 1;
                if missing.is_none() && !font.covers(scalar) {
                    missing = Some(scalar);
                }
            }
        });
        if let Some(scalar) = missing {
            return Err(format!(
                "page uses missing font glyph U+{:04X}",
                scalar as u32
            ));
        }
        if glyphs == 0 {
            return Err("page emitted no text".into());
        }
        pixels.fill(0);
        Raster::new(&mut pixels, font, surface, surface.width * 4)
            .map_err(|e| format!("{e:?}"))?
            .paint(page, surface.bounds())
            .map_err(|e| format!("{e:?}"))
    };

    paint(&Welcome::new(surface).ok_or("welcome page did not fit")?)?;
    let disk = Destination::new(DestinationObservation {
        name: "vda",
        major: 254,
        minor: 0,
        sequence: 7,
        capacity: 16_000_000_000,
        sector: 512,
        removable: false,
        model: Some("Synthetic disk; no device is opened"),
        serial: Some(&"S".repeat(256)),
        wwid: Some(&"W".repeat(256)),
    })?;
    let disks = [disk.clone()];
    paint(
        &destination::DestinationPage::unavailable(surface)
            .ok_or("unavailable destination page did not fit")?,
    )?;
    paint(
        &destination::DestinationPage::waiting(surface)
            .ok_or("waiting destination page did not fit")?,
    )?;
    paint(
        &destination::DestinationPage::refused(surface, "the disks could not be examined")
            .ok_or("refused destination page did not fit")?,
    )?;
    paint(
        &destination::DestinationPage::new(surface, &[], None, 0)
            .ok_or("empty destination page did not fit")?,
    )?;
    paint(
        &destination::DestinationPage::new(surface, &disks, None, 0)
            .ok_or("unselected destination page did not fit")?,
    )?;
    let destination = destination::DestinationPage::new(surface, &disks, Some(0), 0)
        .ok_or("destination page did not fit")?;
    paint(&destination)?;
    let (_, detail_pages) = destination.detail_position();
    for index in 1..detail_pages {
        paint(
            &destination::DestinationPage::new(surface, &disks, Some(0), index)
                .ok_or("destination detail page did not fit")?,
        )?;
    }
    paint(
        &settings::SettingsPage::new(
            surface,
            ["alice", "tdhost", "us", "Etc/UTC"],
            Some(0),
            [5, 6],
            true,
        )
        .ok_or("settings page did not fit")?,
    )?;
    paint(
        &settings::SettingsPage::new(surface, ["", "", "", ""], Some(2), [0, 0], true)
            .ok_or("draft settings page did not fit")?,
    )?;
    paint(
        &settings::Draft::default()
            .page(
                surface,
                Some("the time zones could not be read"),
                Some("the selected disk changed"),
            )
            .ok_or("unlisted settings page did not fit")?,
    )?;
    let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC")?;
    let uuid = [0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 0];
    let plan = Plan::new(
        [1; 32],
        disk,
        [2; 32],
        uuid,
        Storage::Unencrypted,
        Basis::default(),
        settings,
    )?;
    let bound = Plan::new(
        [1; 32],
        plan.destination().clone(),
        [2; 32],
        uuid,
        Storage::DeviceBound,
        Basis::new(true, true),
        plan.settings().clone(),
    )?;
    for plan in [&plan, &bound] {
        let review = review::ReviewPage::new(surface, plan, 0).ok_or("review page did not fit")?;
        paint(&review)?;
        let (_, review_pages) = review.position();
        for index in 1..review_pages {
            paint(
                &review::ReviewPage::new(surface, plan, index)
                    .ok_or("review detail page did not fit")?,
            )?;
        }
    }
    for phase in [
        outcome::Phase::PreparingDisk,
        outcome::Phase::WritingFilesystems,
        outcome::Phase::PublishingDeployment,
        outcome::Phase::ApplyingSettings,
        outcome::Phase::VerifyingBoot,
    ] {
        paint(
            &outcome::ProgressPage::new(surface, outcome::Progress::Running(phase))
                .ok_or("progress page did not fit")?,
        )?;
    }
    for failure in [
        outcome::Failure::DestinationChanged,
        outcome::Failure::InsufficientSpace,
        outcome::Failure::WriteFailed,
        outcome::Failure::VerificationFailed,
        outcome::Failure::SettingsFailed,
        outcome::Failure::RecoveryUnconfirmed,
    ] {
        paint(
            &outcome::ProgressPage::new(surface, outcome::Progress::Failed(failure))
                .ok_or("failure page did not fit")?,
        )?;
    }
    paint(
        &outcome::ProgressPage::new(surface, outcome::Progress::Consent)
            .ok_or("consent page did not fit")?,
    )?;
    paint(
        &outcome::ProgressPage::new(surface, outcome::Progress::Finishing)
            .ok_or("finishing page did not fit")?,
    )?;
    paint(
        &outcome::ProgressPage::new(surface, outcome::Progress::Unconfirmed)
            .ok_or("unconfirmed page did not fit")?,
    )?;
    // The pinned counting key, td-protector's own example; no service
    // supplied it and it opens nothing.
    let key = recovery::Key::from_digits(b"000013005150010290015439020571025716030859035998")?;
    let shown = key.display();
    let mut entry = recovery::Entry::default();
    for chord in ["0", "0", "0", "0", "1", "3", "-", "0", "0", "5"] {
        entry.key(chord);
    }
    let feedback = entry.feedback();
    for step in [
        recovery::Step::Asking,
        recovery::Step::Shown(shown.as_str()),
        recovery::Step::TypeBack {
            entry: &entry,
            feedback: &feedback,
            notice: Some("That is not the recovery key shown. Check it and type it again."),
        },
    ] {
        paint(&recovery::RecoveryPage::new(surface, step).ok_or("recovery-key page did not fit")?)?;
    }
    paint(
        &outcome::ProgressPage::new(surface, outcome::Progress::Unknown)
            .ok_or("unknown outcome page did not fit")?,
    )?;
    paint(&outcome::CompletionPage::new(surface).ok_or("completion page did not fit")?)
}

#[cfg(test)]
#[test]
fn all_installer_pages_paint_with_the_pinned_font() -> Result<(), String> {
    render_check()
}

/// Renders the welcome page to a tight XRGB buffer, `width` by `height` at
/// `scale`, for a still-image preview. The surface must be large enough to
/// hold the page (see `Welcome::new`).
pub fn preview(width: usize, height: usize, scale: u8) -> Result<Vec<u8>, String> {
    let font = td_ui::font::pinned()?;
    let scale = Scale::new(scale).map_err(|error| format!("{error:?}"))?;
    let surface = Surface::new(width, height, scale).map_err(|error| format!("{error:?}"))?;
    let page = Welcome::new(surface).ok_or("surface too small for the welcome page")?;
    let mut pixels = vec![0u8; width * height * 4];
    Raster::new(&mut pixels, &font, surface, width * 4)
        .map_err(|error| format!("{error:?}"))?
        .paint(&page, surface.bounds())
        .map_err(|error| format!("{error:?}"))?;
    Ok(pixels)
}

/// Renders the welcome page to a binary PPM (P6) image, `width` by `height`
/// at `scale`, for a still-frame preview a caller can write out or inspect.
pub fn preview_ppm(width: usize, height: usize, scale: u8) -> Result<Vec<u8>, String> {
    let pixels = preview(width, height, scale)?;
    let scale = Scale::new(scale).map_err(|error| format!("{error:?}"))?;
    let surface = Surface::new(width, height, scale).map_err(|error| format!("{error:?}"))?;
    let rgb =
        td_ui::raster::rgb(&pixels, surface, width * 4).map_err(|error| format!("{error:?}"))?;
    Ok(td_ui::raster::ppm(surface, &rgb))
}
