#![forbid(unsafe_code)]

use std::io::Write;
use td_disks::runtime::{self, Places};
use td_disks::setting::Change;

const USAGE: &str = "usage: td-disks [status | add NAME UUID btrfs|ext4 [--exec] | remove NAME]";

fn request(change: &Change) -> Result<(), String> {
    let mut args = vec!["volume".to_string()];
    args.extend(change.words());
    let status = std::process::Command::new("/bin/td-authd")
        .args(&args)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("volume change did not complete".into())
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let places = Places::system();
    match words.as_slice() {
        ["--help"] | ["-h"] => writeln!(std::io::stdout(), "{USAGE}").map_err(|e| e.to_string()),
        [] | ["status"] => {
            let text = runtime::status(&places)?;
            std::io::stdout()
                .write_all(text.as_bytes())
                .map_err(|e| e.to_string())
        }
        ["mount-all"] => runtime::mount_all(&places),
        ["apply", rest @ ..] => runtime::apply(&places, &Change::parse(rest)?),
        ["add", name, uuid, kind] => request(&Change::parse(&["add", name, uuid, kind, "data"])?),
        ["add", name, uuid, kind, "--exec"] => {
            request(&Change::parse(&["add", name, uuid, kind, "exec"])?)
        }
        ["remove", name] => request(&Change::parse(&["remove", name])?),
        _ => Err(USAGE.into()),
    }
}

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(std::io::stderr(), "td-disks: {error}");
        std::process::exit(1);
    }
}
