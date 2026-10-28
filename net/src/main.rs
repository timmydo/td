#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: the merged applets pre-date the rust-lint rules (AGENTS.md). These
   // network tools are not linted in the loop (the cargo-test gate's clippy leg covers only
   // the dependency-free engine crates); the [lints] table still guards new unsafe (forbid)
   // and enforces under a local `cargo clippy`. The allow folds the former per-file headers
   // of fetch/feed/subst into this one. test_https is new and re-denies these lints at its
   // top; with no clippy leg for this crate, a local `cargo clippy` is what enforces that.

// td-net — td's OWN network tools as ONE multicall (busybox-style) control-plane binary,
// merging the former fetch/ (td-fetch), feed/ (td-feed), and subst/ (td-subst) crates.
//
// Dispatch is by argv[0]'s basename: the td-fetch/td-feed/td-subst applet links each point
// at this binary, so every existing call site (`td-fetch fetch …`, `td-feed warm …`,
// `td-subst sign …`) runs UNCHANGED — the applet's own arg parser sees the same argv it
// always did. Invoked by the umbrella name, the first argument selects the applet
// (`td-net fetch …`), rebuilt into the argv the applet link would have presented.
//
// The three applets keep their own helpers verbatim (this landing is a pure relocation +
// multicall shell); deduplicating the shared HTTP/sha256/serve helpers is a deliberate
// follow-up, kept out so the merge stays byte-for-byte auditable against the old crates.
//
// Cargo builds ONLY `td-net`; the applet NAMES are symlinks to it. The check-loop warm
// (builder/src/check_loop.rs host_net_applet) creates td-fetch/td-feed beside the built
// binary on demand; an operator who uses td-subst links that name or runs
// `td-net subst`. Invoking a bare
// `td-net` with no applet selector is a usage error (exit 2) by design.
mod app;
mod egress;
mod feed;
mod fetch;
mod fetchd;
mod flathub;
mod http;
mod launch;
mod openpgp;
mod ostree;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../engine/src/elf_synth.rs"]
mod rust_elf_synth;
#[cfg(test)]
use td_engine::elf;
mod rust_toolchain;
mod sig;
mod subst;
mod test_https;
#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

// The rule for reading a file that must be a real, bounded regular file:
// td-fs's, shared rather than mirrored.
#[allow(dead_code)]
#[path = "../../td-fs/src/real_file.rs"]
mod td_fs;
// td-fs's durable atomic replace, which the feed and the substituter write
// their files through.
#[path = "../../td-fs/src/replace.rs"]
mod td_fs_replace;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg0 = args.first().map(String::as_str).unwrap_or("td-net");
    let base = std::path::Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if base == "td-rust" || rust_toolchain::is_tool(base) {
        rust_toolchain::run(&args);
        return;
    }
    match base {
        "td-app" => app::run(&args),
        "td-fetch" => fetch::run(&args),
        "td-feed" => feed::run(&args),
        "td-subst" => subst::run(&args),
        "td-fetchd" => fetchd::run(&args),
        "td-egressd" => egress::run(&args),
        "td-launch" => launch::run(&args),
        "td-test-https" => test_https::run(&args),
        // An application's name, linked to this binary: that application,
        // beside this binary, with a fetch service of its own (launch.rs).
        _ if launch::launches(std::ffi::OsStr::new(base)).is_some() => {
            launch::run_named(base, args.get(1..).unwrap_or(&[]))
        }
        // Umbrella name (or an unknown link): the first real arg selects the applet.
        // Rebuild argv as the applet's own link would present it — argv[0] = the applet
        // name so each applet's basename/output is unchanged — then dispatch.
        _ => {
            let applet = match args.get(1).map(String::as_str) {
                Some("app") => "td-app",
                Some("rust") => "td-rust",
                Some("fetch") => "td-fetch",
                Some("feed") => "td-feed",
                Some("subst") => "td-subst",
                Some("fetchd") => "td-fetchd",
                Some("egressd") => "td-egressd",
                Some("launch") => "td-launch",
                Some("test-https") => "td-test-https",
                _ => {
                    eprintln!(
                        "usage: td-net <app|rust|fetch|feed|subst|fetchd|egressd|launch|test-https> ...\n  \
                         (or invoke via the td-fetch / td-feed / td-subst / \
                         td-fetchd / td-test-https applet links)"
                    );
                    std::process::exit(2);
                }
            };
            let mut argv = Vec::with_capacity(args.len().saturating_sub(1));
            argv.push(applet.to_string());
            argv.extend_from_slice(args.get(2..).unwrap_or(&[]));
            match applet {
                "td-app" => app::run(&argv),
                "td-rust" => rust_toolchain::run(&argv),
                "td-fetch" => fetch::run(&argv),
                "td-feed" => feed::run(&argv),
                "td-fetchd" => fetchd::run(&argv),
                "td-egressd" => egress::run(&argv),
                "td-launch" => launch::run(&argv),
                "td-test-https" => test_https::run(&argv),
                _ => subst::run(&argv),
            }
        }
    }
}
