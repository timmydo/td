#![deny(unsafe_code)]

fn main() -> std::process::ExitCode {
    match td_secret::run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-secret: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
