#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = concat!(
    "td-setup [--preview | --render-check | --font-license | --help]\n",
    "The offline graphical installer front end (INSTALLER.md).\n",
    "With no argument it opens the installer window on the Wayland display\n",
    "named by WAYLAND_SOCKET, or WAYLAND_DISPLAY under XDG_RUNTIME_DIR.\n",
    "Return asks the installer service, through td-authd's setup intake,\n",
    "for eligible disks; Up and Down, or a click on a disk's row, select\n",
    "one, PageUp and PageDown page through a disk's identity, Return\n",
    "continues with the selected disk to account and regional settings,\n",
    "and Escape goes back a step.\n",
    "In settings Tab and S-Tab move between fields, as Up, Down and\n",
    "Return do outside the time zone row; Left, Right, Home, End, Backspace\n",
    "and Delete edit the username and hostname; the time zone row takes Up,\n",
    "Down, PageUp, PageDown, Home, End and typed characters, and Return\n",
    "there asks the service to review the form. Space, M-Down or a click\n",
    "opens the keyboard layout or time zone list; there Return or a click\n",
    "on a row chooses and closes it, Escape closes it unchanged, and the\n",
    "wheel scrolls it; a click on a text field focuses it. On its review\n",
    "PageUp and PageDown move between details, Return asks the service to\n",
    "seek consent at the secure prompt (Ctrl+Alt+Escape, then I, opens it;\n",
    "Enter there confirms), and Escape withdraws it; Escape at the consent\n",
    "view withdraws it too. Progress and its outcome follow the service.\n",
    "A device-bound installation then shows its recovery key once: Return\n",
    "asks for it to be typed back, in digits with spaces or hyphens between\n",
    "groups if wanted, Return there sends it and Escape shows the key again.\n",
    "On completion Left or Up chooses restart, Right or Down power\n",
    "off, Tab and S-Tab the other button, and Return asks the service\n",
    "for it.\n",
    "Each step page also shows step buttons on its footer row: Back acts as\n",
    "Escape, and Next (Review on settings, Install on the review, Confirm\n",
    "on the type-back) as Return there, except that Review asks for the\n",
    "review from any field; the consent view has Withdraw, as Escape. On\n",
    "completion Restart and Power off can also be clicked.\n",
    "On any page F1 shows these keys over it until F1, q, Escape or a\n",
    "click, and\n",
    "F12 moves the window to the next colour theme and keeps it.\n",
    "Held, the arrows, PageUp, PageDown, Backspace, Delete and typed\n",
    "characters repeat; Return, Escape and the rest act once a press.\n",
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
