//! td-web, td's own document browser (td-web/DESIGN.md): HTML, text,
//! PNG and JPEG over `http` and `https`, fetched through td's fetch
//! service and shown in a td-ui window. No script runs.
#![forbid(unsafe_code)]

mod app;
mod document;
mod fetch;
mod view;

use std::process::ExitCode;

const USAGE: &str = "usage: td-web [URL]

td's document browser: HTML, text, PNG and JPEG over http and https,
fetched through td's fetch service. No script runs. An address without
a scheme is taken as https.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let start = match args.as_slice() {
        [] => None,
        [flag] if flag == "--help" || flag == "-h" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        [flag] if flag == "--version" => {
            println!("td-web {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        [address] if !address.starts_with('-') => Some(address.clone()),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match app::run(start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("td-web: {why}");
            ExitCode::FAILURE
        }
    }
}
