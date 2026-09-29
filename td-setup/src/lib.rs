#![forbid(unsafe_code)]

//! td-setup: the offline graphical installer's front end, the second
//! `td_ui` consumer after td-editor. It will present the installation
//! wizard as a dependency-free Rust Wayland client over the shared
//! toolkit's raster and chrome bands, following td-install/INSTALLER.md.
//!
//! The `window` turn loop presents welcome and a service-unavailable
//! destination state. The pure settings, review and outcome views render
//! bounded inputs but are not yet connected to that loop. The privileged
//! disk writer stays in td-install; this front end holds no disk-writing
//! authority (INSTALLER.md). Service wiring and later navigation follow.

pub mod destination;
pub mod outcome;
pub mod review;
pub mod settings;
pub mod welcome;
pub mod window;

use td_install::installation_plan::{Destination, DestinationObservation, Plan, Settings};
use td_ui::raster::{Composition, Primitive, Raster, Scale, Surface};
use welcome::Welcome;

/// Paint every installer view using bounded, synthetic data. This checks that
/// the realized target binary can execute the rendering paths; it supplies
/// no service state, consent, or installation authority.
pub fn render_check() -> Result<(), String> {
    let font = td_ui::font::pinned()?;
    let surface = Surface::new(800, 600, Scale::new(1).map_err(|e| format!("{e:?}"))?)
        .map_err(|e| format!("{e:?}"))?;
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
        Raster::new(&mut pixels, &font, surface, surface.width * 4)
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
    let settings = Settings::new("alice", "tdhost", "us", "Etc/UTC")?;
    let uuid = [0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 0];
    let plan = Plan::new([1; 32], disk, [2; 32], uuid, settings)?;
    let review = review::ReviewPage::new(surface, &plan, 0).ok_or("review page did not fit")?;
    paint(&review)?;
    let (_, review_pages) = review.position();
    for index in 1..review_pages {
        paint(
            &review::ReviewPage::new(surface, &plan, index)
                .ok_or("review detail page did not fit")?,
        )?;
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
    ] {
        paint(
            &outcome::ProgressPage::new(surface, outcome::Progress::Failed(failure))
                .ok_or("failure page did not fit")?,
        )?;
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
