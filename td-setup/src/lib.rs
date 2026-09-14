#![forbid(unsafe_code)]

//! td-setup: the offline graphical installer's front end, the second
//! `td_ui` consumer after td-editor. It will present the installation
//! wizard as a dependency-free Rust Wayland client over the shared
//! toolkit's raster and chrome bands, following td-install/INSTALLER.md.
//!
//! So far this crate carries the wizard's pages and their rendering, and
//! the `window` turn loop that presents them as a live client. The
//! privileged disk writer stays in td-install; the front end holds no
//! disk-writing authority (INSTALLER.md). The first page is `welcome`; the
//! wizard's remaining pages, its model and the input that advances them
//! follow in later increments.

pub mod welcome;
pub mod window;

use td_ui::raster::{Raster, Scale, Surface};
use welcome::Welcome;

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
