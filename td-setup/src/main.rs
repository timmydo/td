#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = concat!(
    "td-setup [--preview | --render-check | --font-license | --help]\n",
    "The offline graphical installer front end (INSTALLER.md).\n",
    "With no argument it opens the installer window on the Wayland display\n",
    "named by WAYLAND_SOCKET, or WAYLAND_DISPLAY under XDG_RUNTIME_DIR.\n",
    "Return asks the installer service, through td-authd's setup intake,\n",
    "for eligible disks; Up and Down move through them, PageUp and\n",
    "PageDown through a disk's identity, Return continues with the selected\n",
    "disk to account and regional settings, and Escape goes back a step.\n",
    "In settings Tab and S-Tab move between fields, as Up, Down and\n",
    "Return do outside the time zone row; Left, Right, Home, End, Backspace\n",
    "and Delete edit the username and hostname; the time zone row takes Up,\n",
    "Down, PageUp, PageDown, Home, End and typed characters, and Return\n",
    "there asks the service to review the form. On its review PageUp and\n",
    "PageDown move between details, Return asks the service to seek consent\n",
    "at the secure prompt, and Escape withdraws it; Escape at the consent\n",
    "view withdraws it too. Progress and its outcome follow the service.\n",
    "On any page F1 shows these keys over it until F1, q, Escape or a\n",
    "click (the window ignores the pointer otherwise), and\n",
    "F12 moves the window to the next colour theme and keeps it.\n",
    "When the boot's command line holds td.setup-input=1 it also says, on\n",
    "standard error, each page state it showed while holding the keyboard.\n",
    "--preview writes the welcome page as a PPM image to stdout.\n",
    "--render-check renders all pages and exits without image output.\n",
    "This front end holds no disk-writing authority; td-install writes disks.\n",
);

const PREVIEW_WIDTH: usize = 800;
const PREVIEW_HEIGHT: usize = 600;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => td_setup::window::run_window(),
        [arg] if arg == "--preview" => preview(&mut io::stdout().lock()),
        [arg] if arg == "--render-check" => td_setup::render_check().map_err(io::Error::other),
        [arg] if arg == "--font-license" => {
            let mut output = io::stdout().lock();
            [
                td_ui::notices::FONT_PROVENANCE,
                td_ui::notices::FONT_COPYING,
                td_ui::notices::FONT_LICENSE,
                td_ui::notices::OUTLINE_FACE,
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
