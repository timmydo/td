#![deny(unsafe_code)]
mod clock_sys;
mod runtime;
mod state;

use std::io::Write;
use td_time::request::Change;

const USAGE: &str =
    "usage: td-time [status | set YYYY-MM-DDTHH:MM:SSZ | timezone Area/City | automatic on|off]";

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--help"] | ["-h"] => writeln!(std::io::stdout(), "{USAGE}").map_err(|e| e.to_string()),
        [] | ["status"] => runtime::status(),
        ["query-worker"] => runtime::worker(),
        ["serve"] => runtime::serve(),
        ["apply", action, value] => runtime::apply(Change::parse(action, value)?),
        [action, value] => {
            let change = Change::parse(action, value)?;
            let status = std::process::Command::new("/bin/td-authd")
                .args(["time", change.action(), &change.value()?])
                .status()
                .map_err(|e| e.to_string())?;
            if status.success() {
                Ok(())
            } else {
                Err("time change did not complete".into())
            }
        }
        _ => Err(USAGE.into()),
    }
}

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(std::io::stderr(), "td-time: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;
