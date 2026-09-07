#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

struct Toolchain<'a> {
    gcc: &'a str,
    binutils: &'a str,
    glibc: &'a str,
    unwind: &'a str,
}

impl Toolchain<'_> {
    fn compiled() -> Result<Self, String> {
        Ok(Self {
            gcc: option_env!("TD_CC_GCC")
                .ok_or("td-cc needs its target recipe's GCC configuration")?,
            binutils: option_env!("TD_CC_BINUTILS")
                .ok_or("td-cc needs its target recipe's binutils configuration")?,
            glibc: option_env!("TD_CC_GLIBC")
                .ok_or("td-cc needs its target recipe's glibc configuration")?,
            unwind: option_env!("TD_CC_UNWIND")
                .ok_or("td-cc needs its target recipe's unwinder configuration")?,
        })
    }

    fn command(&self, cxx: bool, directory: &Path, arguments: Vec<OsString>) -> Command {
        let compiler = Path::new(self.gcc)
            .join("bin")
            .join(if cxx { "g++" } else { "gcc" });
        let mut command = Command::new(compiler);
        command.args([
            format!("-B{}/bin/", self.binutils),
            format!("-B{}/lib/", self.glibc),
            format!("-L{}/lib", self.glibc),
            format!("-L{}", self.unwind),
        ]);
        // GCC keeps explicit include paths even under -nostdinc.
        if !arguments.iter().any(|argument| argument == "-nostdinc") {
            command
                .arg("-idirafter")
                .arg(format!("{}/include", self.glibc));
        }
        command.args([
            "-static-libgcc",
            "-fno-omit-frame-pointer",
            "-g1",
            "-Wl,--build-id=sha1",
        ]);
        if cxx {
            command.arg("-static-libstdc++");
        }
        command.arg(format!(
            "-Wl,--dynamic-linker,{}/lib/ld-linux-x86-64.so.2",
            self.glibc
        ));
        command.arg(format!("-Wl,--enable-new-dtags,-rpath,{}/lib", self.glibc));
        let mut remap = OsString::from("-ffile-prefix-map=");
        remap.push(directory);
        remap.push("=/td-build");
        command.arg(remap);
        command.args(arguments);
        command
    }
}

fn is_cxx(invocation: &OsStr) -> bool {
    matches!(
        Path::new(invocation).file_name().and_then(OsStr::to_str),
        Some("c++" | "g++")
    )
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os();
    let invocation = arguments.next().ok_or("td-cc has no invocation name")?;
    let directory: PathBuf =
        std::env::current_dir().map_err(|e| format!("td-cc: working directory: {e}"))?;
    let mut command =
        Toolchain::compiled()?.command(is_cxx(&invocation), &directory, arguments.collect());
    let error = command.exec();
    Err(format!(
        "td-cc: execute {}: {error}",
        command.get_program().to_string_lossy()
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn fixture() -> Toolchain<'static> {
        Toolchain {
            gcc: "/td/store/compiler/stage",
            binutils: "/td/store/linker",
            glibc: "/td/store/libc/stage",
            unwind: "/td/store/td-cc/lib",
        }
    }

    #[test]
    fn compiler_selection_uses_only_the_invocation_basename() {
        for name in ["c++", "/bin/c++", "/td/store/compiler/bin/g++"] {
            assert!(is_cxx(OsStr::new(name)));
        }
        for name in ["cc", "gcc", "td-cc", "/g++/cc", "g++-other"] {
            assert!(!is_cxx(OsStr::new(name)));
        }
        assert_eq!(
            fixture()
                .command(true, Path::new("/work"), vec![])
                .get_program(),
            "/td/store/compiler/stage/bin/g++"
        );
        assert_eq!(
            fixture()
                .command(false, Path::new("/work"), vec![])
                .get_program(),
            "/td/store/compiler/stage/bin/gcc"
        );
    }

    #[test]
    fn defaults_pin_native_tools_and_runtime_without_shell_interpretation() {
        let supplied = vec![
            OsString::from("-o"),
            OsString::from("a b;$x"),
            OsString::from("input.c"),
        ];
        let command = fixture().command(false, Path::new("/work tree"), supplied.clone());
        let args: Vec<_> = command.get_args().map(OsStr::to_os_string).collect();
        for expected in [
            "-B/td/store/linker/bin/",
            "-B/td/store/libc/stage/lib/",
            "-L/td/store/td-cc/lib",
            "-Wl,--dynamic-linker,/td/store/libc/stage/lib/ld-linux-x86-64.so.2",
            "-Wl,--enable-new-dtags,-rpath,/td/store/libc/stage/lib",
            "-ffile-prefix-map=/work tree=/td-build",
            "-fno-omit-frame-pointer",
        ] {
            assert!(
                args.contains(&OsString::from(expected)),
                "missing {expected}"
            );
        }
        assert!(args.ends_with(&supplied));
        assert!(!args.contains(&OsString::from("-static-libstdc++")));
        assert!(fixture()
            .command(true, Path::new("/work"), vec![])
            .get_args()
            .any(|a| a == "-static-libstdc++"));
    }

    #[test]
    fn non_utf8_arguments_and_working_directory_are_preserved() {
        let input = OsString::from_vec(b"source-\xff.c".to_vec());
        let directory = Path::new(OsStr::from_bytes(b"/work-\xfe"));
        let command = fixture().command(false, directory, vec![input.clone()]);
        assert_eq!(command.get_args().last(), Some(input.as_os_str()));
        assert!(command
            .get_args()
            .any(|arg| arg.as_bytes() == b"-ffile-prefix-map=/work-\xfe=/td-build"));
    }
}
