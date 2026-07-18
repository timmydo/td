//! td-recipe-eval — emit / list recipes from the Rust catalog.
//!
//! Subcommands (recipes):
//!   list                  print every recipe's `.ts` file stem, one per line
//!   emit STEM             print STEM's recipe as canonical JSON (the wire format
//!                         the build path consumes)
//!   check-list [pr|daily|all]
//!                         print recipe stems that own checks in the requested tier
//!   check-count STEM [pr|daily|all]
//!                         print how many check bodies STEM owns in the requested tier
//!   check-script STEM [pr|daily|all] [INDEX]
//!                         print STEM's owned check bodies for the requested tier;
//!                         INDEX is 1-based and emits a single body
//!   check-run STEM [pr|daily|all] [INDEX]
//!                         run one recipe-owned package check through the Rust
//!                         runner instead of sourcing tests/ ladder helpers
//!   build-run TARGET [OUTPUT_STEM ...]
//!                         build a catalog target through the same Rust recipe
//!                         runner and print machine-readable local output paths
//!   source-pins           print recipe-owned fixed-output source pins as:
//!                         <key>\t<url>\t<sha256>\t<file>
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
#[path = "td_recipe_eval/sha256.rs"]
mod sha256;

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

/// check-run/build-run errors: a planning-time provenance rejection exits 69
/// (EX_UNAVAILABLE) so callers — td-builder's loop prelude provisioning the
/// userland — can branch on "the bootstrap graph cannot be realized with
/// admissible inputs anywhere" (re #469) without parsing stderr prose. Every
/// other error keeps the usage exit (2).
fn die_runner(msg: &str) -> ! {
    eprintln!("td-recipe-eval: {msg}");
    if msg.starts_with(check_runner::PROVENANCE_REJECTED) {
        exit(69);
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

fn tier_filter(arg: Option<&String>) -> Option<td_recipe::types::CheckTier> {
    match arg.map(String::as_str).unwrap_or("all") {
        "all" => None,
        "pr" => Some(td_recipe::types::CheckTier::Pr),
        "daily" => Some(td_recipe::types::CheckTier::Daily),
        other => die(&format!(
            "unknown check tier '{other}' (expected pr|daily|all)"
        )),
    }
}

fn recipe_has_check(
    r: &td_recipe::types::Recipe,
    tier: Option<td_recipe::types::CheckTier>,
) -> bool {
    !recipe_checks(r, tier).is_empty()
}

fn recipe_checks(
    r: &td_recipe::types::Recipe,
    tier: Option<td_recipe::types::CheckTier>,
) -> Vec<&td_recipe::types::RecipeCheck> {
    r.checks
        .as_ref()
        .map(|xs| {
            xs.iter()
                .filter(|c| tier.map(|t| c.tier == t).unwrap_or(true))
                .collect()
        })
        .unwrap_or_default()
}

fn print_source_pins() {
    for pin in td_recipe::source_pins::all() {
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
            let tier = tier_filter(args.get(2));
            for (stem, r) in catalog::all() {
                if recipe_has_check(&r, tier) {
                    println!("{stem}");
                }
            }
        }
        Some("check-count") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: check-count STEM [pr|daily|all]"));
            let tier = tier_filter(args.get(3));
            let r = lookup_or_die(stem);
            println!("{}", recipe_checks(&r, tier).len());
        }
        Some("check-script") => {
            let stem =
                args.get(2).unwrap_or_else(|| die("usage: check-script STEM [pr|daily|all] [INDEX]"));
            let tier = tier_filter(args.get(3));
            let index = check_index(args.get(4));
            let r = lookup_or_die(stem);
            let checks = recipe_checks(&r, tier);
            if checks.is_empty() {
                die(&format!("{stem} has no checks in the requested tier"));
            }
            if let Some(i) = index {
                match checks.get(i - 1) {
                    Some(c) => println!("{}", c.script),
                    None => die(&format!(
                        "{stem} has only {} check(s) in the requested tier; index {i} is out of range",
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
        Some("source-pins") => {
            if args.get(2).is_some() {
                die("usage: source-pins");
            }
            print_source_pins();
        }
        Some("seed-digests") => {
            if args.get(2).is_some() {
                die("usage: seed-digests");
            }
            if let Err(e) = check_runner::seed_digests_cli() {
                die_runner(&e);
            }
        }
        _ => die("usage: td-recipe-eval list|emit|check-list|check-count|check-script|check-run|build-run|source-pins|seed-digests ..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_filter_counts_recipe_check_bodies() {
        let make = catalog::lookup("make-test").unwrap();
        assert_eq!(
            recipe_checks(&make, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&make, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&make, None).len(), 1);

        let busybox = catalog::lookup("busybox-test").unwrap();
        assert_eq!(
            recipe_checks(&busybox, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&busybox, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&busybox, None).len(), 1);

        let gcc_bridge = catalog::lookup("gcc-10-bridge-test").unwrap();
        assert_eq!(
            recipe_checks(&gcc_bridge, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&gcc_bridge, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&gcc_bridge, None).len(), 1);

        let x86_cross = catalog::lookup("gcc-x86-64-stage2-test").unwrap();
        assert_eq!(
            recipe_checks(&x86_cross, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&x86_cross, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&x86_cross, None).len(), 1);

        let x86_native = catalog::lookup("gcc-x86-64-native-test").unwrap();
        assert_eq!(
            recipe_checks(&x86_native, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&x86_native, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&x86_native, None).len(), 1);

        let x86_self = catalog::lookup("gcc-x86-64-self-test").unwrap();
        assert_eq!(
            recipe_checks(&x86_self, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&x86_self, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&x86_self, None).len(), 1);

        let linux = catalog::lookup("linux-x86-64-test").unwrap();
        assert_eq!(
            recipe_checks(&linux, Some(td_recipe::types::CheckTier::Pr)).len(),
            0
        );
        assert_eq!(
            recipe_checks(&linux, Some(td_recipe::types::CheckTier::Daily)).len(),
            1
        );
        assert_eq!(recipe_checks(&linux, None).len(), 1);
    }

    #[test]
    fn unchecked_recipes_have_zero_check_bodies() {
        let mes = catalog::lookup("mes").unwrap();
        assert_eq!(recipe_checks(&mes, None).len(), 0);
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
        ] {
            let recipe = catalog::lookup(stem).unwrap();
            let checks = recipe_checks(&recipe, Some(td_recipe::types::CheckTier::Daily));
            assert_eq!(checks.len(), count);
            for (index, check) in checks.iter().enumerate() {
                let check_index = index + 1;
                let script = &check.script;
                assert!(check.runner.is_some());
                assert!(script.contains(&format!("check-run {stem} daily {check_index}")));
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
        // gcc-mesboot 4.9.4 and GCC 14.3.0).
        assert_eq!(pins.len(), 44);
        assert!(pins.iter().any(|pin| pin.key == "stage0-source"));
        assert!(pins.iter().any(|pin| pin.key == "rust-toolchain-source"));
        assert!(pins.iter().any(|pin| pin.key == "oyacc-source"));
        assert!(pins.iter().any(|pin| pin.key == "bash-mesboot-source"));
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
