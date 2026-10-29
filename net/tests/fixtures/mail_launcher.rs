use std::os::unix::process::CommandExt;
fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| arg == "--fetch-unix-endpoints")
    {
        println!("td-mail-unix-v1");
        if let Some(path) = std::env::var_os("TD_LAUNCH_TEST_UNIX_GRANT") {
            println!("{}", path.to_string_lossy());
        }
        return;
    }
    let Some(program) = std::env::var_os("TD_LAUNCH_TEST_PROGRAM") else {
        std::process::exit(1);
    };
    let error = std::process::Command::new(program).args(args).exec();
    eprintln!("test program: {error}");
    std::process::exit(1);
}
