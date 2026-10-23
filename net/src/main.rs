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
mod claude_release;
mod deploy;
mod egress;
mod feed;
mod fetch;
mod fetchd;
mod flathub;
mod http;
mod launch;
mod openpgp;
mod ostree;
mod sig;
mod subst;
mod test_https;
#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

// The deployment protocol td-boot READS, included from the crate that defines
// it rather than restated here: `td-deploy` signs a manifest td-boot must
// accept, so the header it requires and the size bound it enforces are one
// copy. Unlike the engine sources below this is in the SHIPPED build — the
// signer consults it on every `sign`. Only the manifest constants are used;
// the rest of the file is the layout the one-bundle-writer landing shares.
#[allow(dead_code)]
#[path = "../../td-boot/src/protocol.rs"]
mod protocol;
// The rule for reading a file that must be a real, bounded regular file —
// td-fs's, and shared with td-boot rather than mirrored. The signer has to
// refuse exactly what the verifier refuses, or it signs a bundle that fails
// at boot instead of at signing; `deploy.rs` used to hold a copy written to
// match, which is the arrangement that guarantees they eventually will not.
#[allow(dead_code)]
#[path = "../../td-fs/src/real_file.rs"]
mod td_fs;
// td-fs's durable atomic replace, which the feed, the substituter and the
// signer write their files through.
#[path = "../../td-fs/src/replace.rs"]
mod td_fs_replace;

// The TARGET-side verifier, compiled into the test build only. td signs here
// with `ring` and verifies on the target with the engine's dependency-free
// implementation, so the two must agree byte for byte or a deployment that was
// signed correctly fails to boot. Declared at the crate root because
// `ed25519.rs` reaches its hash as `crate::sha512`, which is how it resolves
// inside the engine lib and inside td-boot alike.
//
// Including the SOURCE brings the engine's own `#[cfg(test)] mod tests` with
// it, so `cargo test` here runs ed25519's and sha512's unit tests a second
// time, reported under td-net. That is duplication of RUNS, not of code — a
// red named `ed25519::tests::*` in this crate belongs to engine/src.
#[cfg(test)]
#[path = "../../engine/src/ed25519.rs"]
mod ed25519;
#[cfg(test)]
#[path = "../../engine/src/sha512.rs"]
mod sha512;
// The engine's SIGNER, test-only here for one purpose: `ed25519_cross.rs`
// checks it against `ring` in both directions. It is not part of td-net's
// shipped build — `ring` is the signer this crate uses — and td-boot includes
// neither this file nor anything that reaches it.
#[cfg(test)]
mod ed25519_cross;
#[cfg(test)]
#[path = "../../engine/src/ed25519_sign.rs"]
mod ed25519_sign;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg0 = args.first().map(String::as_str).unwrap_or("td-net");
    let base = std::path::Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match base {
        "td-app" => app::run(&args),
        "td-fetch" => fetch::run(&args),
        "td-feed" => feed::run(&args),
        "td-subst" => subst::run(&args),
        "td-deploy" => deploy::run(&args),
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
                Some("fetch") => "td-fetch",
                Some("feed") => "td-feed",
                Some("subst") => "td-subst",
                Some("deploy") => "td-deploy",
                Some("fetchd") => "td-fetchd",
                Some("egressd") => "td-egressd",
                Some("launch") => "td-launch",
                Some("test-https") => "td-test-https",
                _ => {
                    eprintln!(
                        "usage: td-net <app|fetch|feed|subst|deploy|fetchd|egressd|launch|test-https> ...\n  \
                         (or invoke via the td-fetch / td-feed / td-subst / td-deploy / \
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
                "td-fetch" => fetch::run(&argv),
                "td-feed" => feed::run(&argv),
                "td-deploy" => deploy::run(&argv),
                "td-fetchd" => fetchd::run(&argv),
                "td-egressd" => egress::run(&argv),
                "td-launch" => launch::run(&argv),
                "td-test-https" => test_https::run(&argv),
                _ => subst::run(&argv),
            }
        }
    }
}
