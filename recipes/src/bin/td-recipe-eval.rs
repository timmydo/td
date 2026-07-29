//! td-recipe-eval — emit / list recipes from the Rust catalog.
//!
//! Subcommands (recipes):
//!   list                  print every recipe's `.ts` file stem, one per line
//!   emit STEM             print STEM's recipe as canonical JSON (the wire format
//!                         the build path consumes)
//!   check-list            print recipe stems that own checks
//!   check-count STEM      print how many check bodies STEM owns
//!   check-script STEM [INDEX]
//!                         print STEM's owned check bodies; INDEX is 1-based and
//!                         emits a single body
//!   check-run STEM [INDEX]
//!                         run one recipe-owned package check through the Rust
//!                         runner instead of sourcing tests/ ladder helpers
//!   build-run TARGET [OUTPUT_STEM ...]
//!                         build a catalog target through the same Rust recipe
//!                         runner and print machine-readable local output paths
//!   clear-store           reset the ladder work dir (seed store/db + shared
//!                         build-cache); the next build re-derives seeds and
//!                         cold-climbs. The only path that clears persisted state
//!   warm [TARGET]         fetch every declared input TARGET's closure needs and
//!                         the caches lack (default system-x86-64), building
//!                         nothing. The host-side operator commands (`run`,
//!                         `qemu-boot*`) do this for themselves from a terminal
//!   source-pins           print recipe-owned fixed-output source pins as:
//!                         <key>\t<url>\t<sha256>\t<file>
//!   source-pin STEM       print the fixed-output source pin(s) owned by STEM
//!                         in the same tab-separated form
//! This is the loop tool the `recipe-rs` gate drives AND the corpus consumer
//! entry (replacing `ts-emit` on the boa path). (The system-spec subcommands —
//! list-specs/emit-spec/verify-spec — were retired with the guix-system museum
//! tier: their only real consumer was the deleted spec-diff differential.)

use std::process::exit;

use td_recipe::catalog;

#[path = "td_recipe_eval/check_runner.rs"]
mod check_runner;
#[path = "td_recipe_eval/checks/mod.rs"]
mod checks;
#[path = "td_recipe_eval/seed_digests.rs"]
mod seed_digests;
#[path = "td_recipe_eval/warm.rs"]
mod warm;
// SHA-256 lives in the shared, std-only td-engine (one copy for td-recipe-eval +
// td-builder). Re-exported at crate root so existing `crate::sha256::` paths are
// unchanged.
use td_engine::sha256;

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn die(msg: &str) -> ! {
    eprintln!("td-recipe-eval: {msg}");
    exit(2);
}

/// check-run/build-run errors, as a code a caller can branch on without
/// parsing stderr prose. The two unhappy endings that look alike from outside
/// get DIFFERENT codes, because a caller must tolerate one and never the other:
///
/// - a HOST gap (`HOST_GAP`) — nothing here can do the work, e.g. no toolchain
///   reachable in the loop sandbox. Exits `EXIT_UNPROVISIONED` and prints the
///   sentinel, which is what gate-run reads as a tolerated skip.
/// - a planning-time provenance rejection (`PROVENANCE_REJECTED`) — the
///   bootstrap graph cannot be realized with admissible inputs on ANY host (re
///   #469). Exits `EXIT_PROVENANCE_REJECTED`. It shared 69 with the host gap
///   until this split, so a caller reading the number alone could not tell a
///   machine that cannot run the work from a chain that nothing can build.
///
/// Every other error keeps the usage exit (2).
fn die_runner(msg: &str) -> ! {
    eprintln!("td-recipe-eval: {msg}");
    if msg.starts_with(check_runner::HOST_GAP) {
        eprintln!("{}", td_engine::exit::UNPROVISIONED_SENTINEL);
        exit(td_engine::exit::EXIT_UNPROVISIONED);
    }
    if msg.starts_with(check_runner::PROVENANCE_REJECTED) {
        exit(td_engine::exit::EXIT_PROVENANCE_REJECTED);
    }
    exit(2);
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn lookup_or_die(stem: &str) -> td_recipe::types::Recipe {
    match catalog::lookup(stem) {
        Some(r) => r,
        None => die(&format!("unknown recipe stem '{stem}' (try `list`)")),
    }
}

fn recipe_has_check(r: &td_recipe::types::Recipe) -> bool {
    !recipe_checks(r).is_empty()
}

fn recipe_checks(r: &td_recipe::types::Recipe) -> Vec<&td_recipe::types::RecipeCheck> {
    r.checks
        .as_ref()
        .map(|xs| xs.iter().collect())
        .unwrap_or_default()
}

fn print_source_pins() {
    for pin in td_recipe::source_pins::all() {
        println!("{}\t{}\t{}\t{}", pin.key, pin.url, pin.sha256, pin.file);
    }
}

fn print_recipe_source_pins(stem: &str) {
    let recipe = lookup_or_die(stem);
    let Some(pins) = recipe.source_pins else {
        die(&format!("recipe `{stem}' declares no fixed-output source pin"));
    };
    if pins.is_empty() {
        die(&format!("recipe `{stem}' declares no fixed-output source pin"));
    }
    for pin in pins {
        println!("{}\t{}\t{}\t{}", pin.key, pin.url, pin.sha256, pin.file);
    }
}

fn check_index(arg: Option<&String>) -> Option<usize> {
    let s = arg?;
    let n = s
        .parse::<usize>()
        .unwrap_or_else(|_| die(&format!("check index '{s}' is not a positive integer")));
    if n == 0 {
        die("check index must be 1-based");
    }
    Some(n)
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("list") => {
            for (stem, _) in catalog::all() {
                println!("{stem}");
            }
        }
        Some("emit") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: emit STEM"));
            println!("{}", lookup_or_die(stem).to_json().to_canonical());
        }
        Some("check-list") => {
            for (stem, r) in catalog::all() {
                if recipe_has_check(&r) {
                    println!("{stem}");
                }
            }
        }
        Some("check-count") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: check-count STEM"));
            let r = lookup_or_die(stem);
            println!("{}", recipe_checks(&r).len());
        }
        Some("check-script") => {
            let stem = args
                .get(2)
                .unwrap_or_else(|| die("usage: check-script STEM [INDEX]"));
            let index = check_index(args.get(3));
            let r = lookup_or_die(stem);
            let checks = recipe_checks(&r);
            if checks.is_empty() {
                die(&format!("{stem} owns no checks"));
            }
            if let Some(i) = index {
                match checks.get(i - 1) {
                    Some(c) => println!("{}", c.script),
                    None => die(&format!(
                        "{stem} owns only {} check(s); index {i} is out of range",
                        checks.len()
                    )),
                }
            } else {
                for c in checks {
                    println!("{}", c.script);
                }
            }
        }
        Some("check-run") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::cli(rest) {
                die_runner(&e);
            }
        }
        Some("build-run") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::build_cli(rest) {
                die_runner(&e);
            }
        }
        Some("clear-store") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::clear_store_cli(rest) {
                die_runner(&e);
            }
        }
        Some("qemu-boot") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::qemu_boot_cli(rest) {
                die_runner(&e);
            }
        }
        Some("qemu-boot-erofs") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::qemu_boot_erofs_cli(rest) {
                die_runner(&e);
            }
        }
        Some("qemu-boot-system") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::qemu_boot_system_cli(rest) {
                die_runner(&e);
            }
        }
        Some("qemu-boot-net") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::qemu_boot_net_cli(rest) {
                die_runner(&e);
            }
        }
        Some("qemu-boot-kexec") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::qemu_boot_kexec_cli(rest) {
                die_runner(&e);
            }
        }
        Some("run") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::run_cli(rest) {
                die_runner(&e);
            }
        }
        Some("warm") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::warm_cli(rest) {
                die_runner(&e);
            }
        }
        Some("verify-store") => {
            let rest = args.get(2..).unwrap_or(&[]);
            if let Err(e) = check_runner::verify_store_cli(rest) {
                die_runner(&e);
            }
        }
        Some("source-pins") => {
            if args.get(2).is_some() {
                die("usage: source-pins");
            }
            print_source_pins();
        }
        Some("source-pin") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: source-pin STEM"));
            if args.get(3).is_some() {
                die("usage: source-pin STEM");
            }
            print_recipe_source_pins(stem);
        }
        Some("seed-digests") => {
            if args.get(2).is_some() {
                die("usage: seed-digests");
            }
            if let Err(e) = check_runner::seed_digests_cli() {
                die_runner(&e);
            }
        }
        Some("local-source-digests") => {
            if args.get(2).is_some() {
                die("usage: local-source-digests");
            }
            // `die`, not `die_runner`: this reds on a stale COMMITTED table, which is a
            // hard failure to fix in the tree. die_runner would map its `provenance
            // rejected' prose to 69, the code a gate runner reads as UNPROVISIONED and
            // tolerates as a SKIP — the one outcome that would make this gate vacuous.
            if let Err(e) = check_runner::local_source_digests_cli() {
                die(&e);
            }
        }
        _ => die("usage: td-recipe-eval list|emit|check-list|check-count|check-script|check-run|build-run|clear-store|qemu-boot|qemu-boot-erofs|qemu-boot-system|qemu-boot-net|qemu-boot-kexec|run|warm|verify-store|source-pins|source-pin|seed-digests|local-source-digests ..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every recipe that owns checks owns exactly the bodies the catalog
    /// declares — the count `check-count` reports and `recipe-checks` loops over.
    #[test]
    fn checked_recipes_own_their_check_bodies() {
        for stem in [
            "make-test",
            "busybox-test",
            "gcc-10-bridge-test",
            "gcc-x86-64-stage2-test",
            "gcc-x86-64-native-test",
            "gcc-x86-64-self-test",
            "linux-x86-64-test",
            "flex-x86-64-test",
            "elfutils-x86-64-test",
            "btrfs-progs-x86-64-test",
            "td-boot-test",
            "hello-test",
            "rust-userland-auto-test",
        ] {
            let r = catalog::lookup(stem).unwrap();
            assert_eq!(recipe_checks(&r).len(), 1, "{stem}");
            assert!(recipe_has_check(&r), "{stem}");
        }
    }

    #[test]
    fn unchecked_recipes_have_zero_check_bodies() {
        let mes = catalog::lookup("mes").unwrap();
        assert_eq!(recipe_checks(&mes).len(), 0);
    }

    #[test]
    fn recipe_check_bodies_delegate_to_the_rust_runner() {
        for (stem, count) in [
            ("make-test", 1),
            ("busybox-test", 1),
            ("rust-toolchain", 1),
            ("gcc-10-bridge-test", 1),
            ("gcc-x86-64-stage2-test", 1),
            ("gcc-x86-64-native-test", 1),
            ("gcc-x86-64-self-test", 1),
            ("linux-x86-64-test", 1),
            // linux-x86-64 itself registers NO check: its qemu boot is a
            // host-side tool (`td-recipe-eval qemu-boot`), not a sandboxed gate
            // check, because a qemu boot needs host qemu the gate sandbox hides
            // (re #529). Its in-sandbox coverage is linux-x86-64-test above.
            ("flex-x86-64-test", 1),
            ("elfutils-x86-64-test", 1),
            ("btrfs-progs-x86-64-test", 1),
            ("td-boot-test", 1),
            ("hello-test", 1),
            ("rust-userland-auto-test", 1),
        ] {
            let recipe = catalog::lookup(stem).unwrap();
            let checks = recipe_checks(&recipe);
            assert_eq!(checks.len(), count);
            for (index, check) in checks.iter().enumerate() {
                let check_index = index + 1;
                let script = &check.script;
                assert!(check.runner.is_some());
                assert!(script.contains(&format!("check-run {stem} {check_index}")));
                assert!(!script.contains(". tests/cache-lib.sh"));
                assert!(!script.contains(". tests/ladder-lib.sh"));
                assert!(!script.contains(". tests/x86_64-cross-fns.sh"));
            }
        }
    }

    #[test]
    fn source_pins_cli_surface_has_the_legacy_lock_count() {
        let pins = td_recipe::source_pins::all();
        // 32 migrated legacy locks + oyacc-6.6 (the bash shell's `yacc`) +
        // bash-2.05b (the from-source bootstrap shell, re #469) + sed-4.2.2
        // (the gcc-mesboot1-era `sed` provider, re #469) + sed-4.0.9 (the
        // tcc-era `sed` cycle-breaker, re #469) + coreutils-5.0 (the tcc-era
        // coreutils cycle-breaker, re #469) + grep-2.4 (the tcc-era `grep`
        // cycle-breaker, re #469) + gawk-3.0.4 (the tcc-era `gawk`
        // cycle-breaker, re #469) + diffutils-2.7 (the tcc-era `diffutils`
        // cycle-breaker, re #469) + m4-1.4.19 (the glibc-rung `bison`
        // provider's macro processor, re #469) + bison-3.8.2 (the glibc-rung
        // parser generator, re #469) + Python-3.11.1 (the glibc-rung python3,
        // re #469) + GCC 10.5.0 (the compatibility bridge between
        // gcc-mesboot 4.9.4 and GCC 14.3.0) + the linux-x86-64 kernel source +
        // flex-2.6.4 + elfutils-0.192 (the modern-kernel host tools flex +
        // libelf, re #529) + CMake 3.31.12 + Rust 1.96.0 source and its exact
        // three-component Rust 1.95.0 stage0 snapshot + coreutils-0.9.0 (the
        // uutils, ripgrep, and fd userland `.crate` sources) + btrfs-progs 7.0
        // and util-linux 2.42.2 (the persistent-volume writer and its minimal
        // libraries).
        assert_eq!(pins.len(), 56);
        assert!(pins.iter().any(|pin| pin.key == "stage0-source"));
        assert!(pins.iter().any(|pin| pin.key == "cmake-x86-64-source"));
        assert!(pins.iter().any(|pin| pin.key == "rust-source"));
        assert!(pins.iter().any(|pin| pin.key == "rust-stage0-rustc-source"));
        assert!(pins.iter().any(|pin| pin.key == "rust-stage0-std-source"));
        assert!(pins.iter().any(|pin| pin.key == "rust-stage0-cargo-source"));
        assert!(pins.iter().any(|pin| pin.key == "oyacc-source"));
        assert!(pins.iter().any(|pin| pin.key == "bash-mesboot-source"));
        assert!(pins.iter().any(|pin| pin.key == "uutils-source"));
        assert!(pins
            .iter()
            .any(|pin| pin.key == "btrfs-progs-x86-64-source"));
        assert!(pins
            .iter()
            .any(|pin| pin.key == "util-linux-libs-x86-64-source"));
    }

    #[test]
    fn rust_userland_recipes_own_their_fixed_output_source_pins() {
        let platform: Vec<String> = [
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let ripgrep = catalog::lookup("ripgrep").unwrap();
        assert_eq!(ripgrep.source_input.as_deref(), Some("ripgrep-source"));
        assert_eq!(ripgrep.native_inputs.as_deref(), Some(platform.as_slice()));
        assert_eq!(
            ripgrep.cargo_lock.as_deref(),
            Some("recipes/locks/ripgrep/Cargo.lock")
        );
        let pins = ripgrep.source_pins.unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].key, "ripgrep-source");
        assert_eq!(
            pins[0].sha256,
            "f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9"
        );

        let fd = catalog::lookup("fd").unwrap();
        assert_eq!(fd.source_input.as_deref(), Some("fd-source"));
        assert_eq!(fd.native_inputs.as_deref(), Some(platform.as_slice()));
        assert_eq!(
            fd.cargo_lock.as_deref(),
            Some("recipes/locks/fd/Cargo.lock")
        );
        let pins = fd.source_pins.unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].key, "fd-source");
        assert_eq!(
            pins[0].sha256,
            "de08defa195af894cc295a43bfc65ba28903e492fd5f32f7a24bf75eafd9bf34"
        );

        let uutils = catalog::lookup("uutils").unwrap();
        assert_eq!(uutils.no_default_features, Some(true));
        let features = uutils.features.as_deref().unwrap();
        // Applets are selected individually, never via an aggregate: the aggregates
        // (`unix`/`feat_Tier1`/the `feat_require_unix_*` groups) pull the checksum,
        // factor, pager, and stdbuf crate subtrees we never ship. (The exact
        // feature<->/bin farm equality is asserted in system-x86-64.rs.)
        for expected in ["ls", "cat", "cp", "chmod", "id", "date", "mknod"] {
            assert!(
                features.iter().any(|feature| feature == expected),
                "missing shipped applet feature '{expected}'"
            );
        }
        for banned in [
            "unix",
            "stdbuf",
            "feat_Tier1",
            "feat_common_core",
            "feat_require_unix_core",
            "feat_require_unix_hostid",
            "feat_require_unix_utmpx",
        ] {
            assert!(
                !features.iter().any(|feature| feature == banned),
                "aggregate/unshipped feature '{banned}' must not be selected"
            );
        }
        let pins = uutils.source_pins.unwrap();
        assert_eq!(pins.len(), 1);
        let pin = pins.first().unwrap();
        assert_eq!(pin.key, "uutils-source");
        assert_eq!(
            pin.sha256,
            "b92df9b821533650f3797aadae46e547f72db281c1f8a27f381f36d54284d34b"
        );
    }

    #[test]
    fn build_run_rejects_unknown_targets_before_setup() {
        let err = check_runner::build_cli(&["not-a-recipe".to_string()]).unwrap_err();
        assert!(err.contains("unknown recipe stem 'not-a-recipe'"));
    }

    // The `recipe-rs` gate's (A) coverage leg (formerly tests/recipe-rs.sh, driven
    // over the `emit`/`verify` CLI subprocess) is ALREADY a plain unit test:
    // catalog::tests::every_recipe_emits_canonical_json_and_round_trips covers
    // "every recipe emits valid, round-tripping JSON" — no need to duplicate it
    // here, `cargo test --manifest-path recipes/Cargo.toml` already runs both.
    // (`verify` itself is gone — it was a boa-migration oracle with no live
    // caller left once this discrimination check moved off the CLI.)
    //
    // (C) discrimination leg (negative control): two different recipes' canonical
    // JSON must differ — the always-on proof that a JSON comparison actually
    // discriminates a mismatch, not a vacuous always-equal check.
    #[test]
    fn a_mismatched_recipe_is_discriminated() {
        let make = catalog::lookup("make-test")
            .expect("make-test recipe must exist (negative-control fixture)");
        let busybox = catalog::lookup("busybox-test")
            .expect("busybox-test recipe must exist (negative-control fixture)");
        assert_ne!(
            make.to_json().to_canonical(),
            busybox.to_json().to_canonical(),
            "make-test and busybox-test canon-equal — a JSON comparison would not discriminate a mismatch"
        );
    }
}
