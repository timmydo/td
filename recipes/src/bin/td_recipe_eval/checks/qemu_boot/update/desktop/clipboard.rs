//! Exercise selection transfer through two real editors and a viewer reconnect.
use super::{
    changed_columns, child, error, lock, text_file, wait, Guest, Login, Process, Result, Viewer,
};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const FROM_HOST: &str = "from the viewer\nsecond line: café";
const FROM_GUEST: &str = "\nfromguest\nlastline";

fn guest_file(login: &Login, expected: &str) -> Result<bool> {
    Ok(login.read("cat \"$HOME/viewer\"")?.as_deref() == Some(expected))
}

// td-ui's SELECTED background, sampled in the document area. The
// midpoint also identifies a multi-line selection after a reconnect.
const SELECTED: [u8; 3] = [0x53, 0x6b, 0x73];
const INACTIVE: [u8; 3] = [0xc8, 0xc4, 0xbb];

fn highlighted_row(pixels: &[u8], color: [u8; 3]) -> Option<usize> {
    let mut rows = (110..510).filter(|y| {
        let start = 3 * (800 * y + 10);
        pixels
            .get(start..start + 180 * 3)
            .is_some_and(|line| line.chunks_exact(3).filter(|rgb| *rgb == color).count() >= 12)
    });
    let first = rows.next()?;
    Some((first + rows.last().unwrap_or(first)) / 2)
}

pub(super) fn exercise(
    view: &mut Viewer,
    login: &Login,
    viewer: &Path,
    editor: &Path,
    guest: &mut Guest,
) -> Result<()> {
    let file = child(login.work, "host-text")?;
    fs::write(&file, FROM_HOST).map_err(error)?;
    view.request("workspace 2")?;
    let _peer = Process::start(
        Command::new(editor)
            .current_dir(login.work)
            .arg("--keys=windows")
            .arg(&file)
            .env_remove("WAYLAND_SOCKET")
            .env("WAYLAND_DISPLAY", "/proc/self/cwd/v/wayland-0")
            .env("XDG_RUNTIME_DIR", login.work)
            .stdin(Stdio::null()),
    )?;
    view.focused("td-editor")?;
    view.ctrl(30)?;
    view.ctrl(46)?;
    // Keep the source focused until the editor has processed Copy and
    // the following edit. The copied snapshot must retain the original bytes.
    view.tap(106)?; // Right collapses the selection to its end.
    view.text("x")?;
    view.ctrl(31)?;
    wait("host editor processed Copy before losing focus", || {
        let saved = text_file(&file, 1024)?;
        Ok((saved.strip_suffix('x') == Some(FROM_HOST)).then_some(()))
    })?;
    view.request("workspace 1")?;
    view.focused("td-desktop")?;
    // F10 sends asynchronously. A paste before delivery can find no offer.
    // Replace the document on each attempt so a late paste cannot duplicate it.
    let mut grace = Duration::from_millis(250);
    wait("host clipboard pasted and saved in guest editor", || {
        view.tap(68)?;
        // Keyboard events invalidate the guest clipboard revision. Leave
        // the seat idle while the transfer runs, as an operator would.
        std::thread::sleep(grace);
        grace = (grace * 2).min(Duration::from_secs(2));
        view.ctrl(30)?;
        view.ctrl(47)?;
        view.ctrl(31)?;
        Ok(guest_file(login, FROM_HOST)?.then_some(()))
    })?;
    lock(guest, login.work)?;

    view.ctrl(30)?;
    view.text("prefix")?;
    view.tap(28)?;
    view.text("fromguest")?;
    view.tap(28)?;
    view.text("lastline")?;
    view.ctrl(31)?;
    wait("guest multiline document", || {
        Ok(guest_file(login, "prefix\nfromguest\nlastline")?.then_some(()))
    })?;
    // Locate the first line in the scaled viewer, then start the click
    // test with the caret on the last line so a missed click is observable.
    view.ctrl(102)?; // Ctrl+Home
    view.key(42, true)?;
    view.tap(107)?; // Shift+End
    view.key(42, false)?;
    let row = wait("first line selected", || {
        Ok(highlighted_row(&view.capture()?, SELECTED))
    })?;
    view.ctrl(107)?; // Ctrl+End
    wait("first line deselected", || {
        Ok(highlighted_row(&view.capture()?, SELECTED)
            .is_none()
            .then_some(()))
    })?;
    let x = 350; // To the right of the short lines.
    let bottom = row + 90;
    view.pointer(x, row, false)?;
    view.pointer(x, row, true)?;
    view.pointer(x, row, false)?;
    view.text("x")?;
    view.pointer(x, row, true)?;
    view.pointer(x, bottom, true)?;
    let (middle, selected) = wait("forward pointer selection painted", || {
        let pixels = view.capture()?;
        Ok(highlighted_row(&pixels, SELECTED)
            .filter(|middle| *middle > row)
            .map(|middle| (middle, pixels)))
    })?;
    view.pointer(x, bottom, false)?;
    view.ctrl(46)?;
    view.ctrl(31)?;
    wait(
        "guest editor processed Copy and saved the click edit",
        || Ok(guest_file(login, "prefixx\nfromguest\nlastline")?.then_some(())),
    )?;
    view.tap(67)?; // F9 publishes the guest's pointer selection.
                   // ReadText and the following input use the same ordered control stream.
                   // Keep the viewer focused through the reply and a subsequent repaint.
    view.tap(66)?;
    wait("viewer launcher after clipboard copy", || {
        Ok((changed_columns(&selected, &view.capture()?) > 150).then_some(()))
    })?;
    view.tap(1)?;
    wait("return from viewer launcher", || {
        Ok((highlighted_row(&view.capture()?, SELECTED) == Some(middle)).then_some(()))
    })?;
    view.request("workspace 2")?;
    view.focused("td-editor")?;
    view.ctrl(30)?;
    view.ctrl(47)?;
    wait("guest clipboard pasted and saved in host editor", || {
        view.ctrl(31)?;
        Ok((text_file(&file, 1024)? == FROM_GUEST).then_some(()))
    })?;
    view.request("workspace 1")?;
    view.focused("td-desktop")?;
    view.pointer(x, bottom, false)?;
    view.pointer(x, bottom, true)?;
    wait("press cleared the earlier selection", || {
        Ok(highlighted_row(&view.capture()?, SELECTED)
            .is_none()
            .then_some(()))
    })?;
    view.pointer(x, row, true)?;
    wait("reverse selection painted with button still down", || {
        Ok((highlighted_row(&view.capture()?, SELECTED) == Some(middle)).then_some(()))
    })?;
    view.process.0.kill().map_err(error)?;
    view.process.0.wait().map_err(error)?;
    wait("old viewer window removed", || {
        let layout = String::from_utf8(view.request("layout")?).map_err(error)?;
        Ok((!layout.contains("app_id=td-desktop ")).then_some(()))
    })?;
    // Release on the host border while no viewer exists to receive it.
    view.pointer(0, 599, false)?;
    lock(guest, login.work)?;
    view.process = Viewer::connect(login, viewer)?;
    wait("reconnected viewer displays the existing selection", || {
        // A terminating SSH helper can briefly retain the single viewer slot.
        if view.process.0.try_wait().map_err(error)?.is_some() {
            view.process = Viewer::connect(login, viewer)?;
            return Ok(None);
        }
        let pixels = view.capture()?;
        Ok([SELECTED, INACTIVE]
            .into_iter()
            .any(|color| highlighted_row(&pixels, color) == Some(middle))
            .then_some(()))
    })?;
    // The first keyboard edit must replace the drag's original selection.
    view.text("z")?;
    view.ctrl(31)?;
    wait("edit after reconnect replaces the selected text", || {
        Ok(guest_file(login, "prefixxz")?.then_some(()))
    })?;
    lock(guest, login.work)
}
