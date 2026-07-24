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
   // and enforces under a local `cargo clippy`. The crate-level allow covers every applet
   // module — the former per-file headers of fetch/feed/subst folded into this one.

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
// binary on demand; the td-subst name is provided by the daily substitute stash
// (builder/src/daily.rs) and by operators per tools/resolve-toolchain.sh. Invoking a bare
// `td-net` with no applet selector is a usage error (exit 2) by design.
mod feed;
mod fetch;
mod subst;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg0 = args.first().map(String::as_str).unwrap_or("td-net");
    let base = std::path::Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match base {
        "td-fetch" => fetch::run(&args),
        "td-feed" => feed::run(&args),
        "td-subst" => subst::run(&args),
        // Umbrella name (or an unknown link): the first real arg selects the applet.
        // Rebuild argv as the applet's own link would present it — argv[0] = the applet
        // name so each applet's basename/output is unchanged — then dispatch.
        _ => {
            let applet = match args.get(1).map(String::as_str) {
                Some("fetch") => "td-fetch",
                Some("feed") => "td-feed",
                Some("subst") => "td-subst",
                _ => {
                    eprintln!(
                        "usage: td-net <fetch|feed|subst> ...\n  \
                         (or invoke via the td-fetch / td-feed / td-subst applet links)"
                    );
                    std::process::exit(2);
                }
            };
            let mut argv = Vec::with_capacity(args.len().saturating_sub(1));
            argv.push(applet.to_string());
            argv.extend_from_slice(args.get(2..).unwrap_or(&[]));
            match applet {
                "td-fetch" => fetch::run(&argv),
                "td-feed" => feed::run(&argv),
                _ => subst::run(&argv),
            }
        }
    }
}
