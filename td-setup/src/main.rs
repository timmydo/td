#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = concat!(
    "td-setup [--preview | --font-license | --help]\n",
    "The offline graphical installer front end (INSTALLER.md).\n",
    "With no argument it opens the installer window on the Wayland display\n",
    "named by WAYLAND_SOCKET, or WAYLAND_DISPLAY under XDG_RUNTIME_DIR.\n",
    "It presents the welcome page; the wizard's later pages are not built yet.\n",
    "--preview writes the welcome page as a PPM image to stdout.\n",
    "This front end holds no disk-writing authority; td-install writes disks.\n",
);

const PREVIEW_WIDTH: usize = 800;
const PREVIEW_HEIGHT: usize = 600;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => td_setup::window::run_window(),
        [arg] if arg == "--preview" => preview(&mut io::stdout().lock()),
        [arg] if arg == "--font-license" => {
            let mut output = io::stdout().lock();
            [
                td_ui::notices::FONT_PROVENANCE,
                td_ui::notices::FONT_COPYING,
                td_ui::notices::FONT_LICENSE,
            ]
            .iter()
            .try_for_each(|notice| output.write_all(notice.as_bytes()))
        }
        [arg] if arg == "--help" => io::stdout().lock().write_all(HELP.as_bytes()),
        _ => {
            let _ = io::stderr().lock().write_all(HELP.as_bytes());
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "td-setup: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes the welcome page as a binary PPM (P6) image, for manual
/// inspection and a still-frame render check.
fn preview(output: &mut impl Write) -> io::Result<()> {
    let ppm = td_setup::preview_ppm(PREVIEW_WIDTH, PREVIEW_HEIGHT, 1).map_err(io::Error::other)?;
    output.write_all(&ppm)
}
