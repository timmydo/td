//! Command-line entry for the shared sandbox frontend.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    td_sandbox::main()
}
