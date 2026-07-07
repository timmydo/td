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
//! This is the loop tool the `recipe-rs` gate drives AND the corpus consumer
//! entry (replacing `ts-emit` on the boa path). (The system-spec subcommands —
//! list-specs/emit-spec/verify-spec — were retired with the guix-system museum
//! tier: their only real consumer was the deleted spec-diff differential.)

use std::process::exit;

use td_recipe::catalog;

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn die(msg: &str) -> ! {
    eprintln!("td-recipe-eval: {msg}");
    exit(2);
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
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
        other => die(&format!("unknown check tier '{other}' (expected pr|daily|all)")),
    }
}

fn recipe_has_check(r: &td_recipe::types::Recipe, tier: Option<td_recipe::types::CheckTier>) -> bool {
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

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
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
        _ => die("usage: td-recipe-eval list|emit|check-list|check-count|check-script ..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_filter_counts_recipe_check_bodies() {
        // sed + hello build ONLY on the /td/store store-native ladder now (their
        // guix-seeded pr/gnu-version checks retired with the corpus): one daily check each.
        let sed = catalog::lookup("sed").unwrap();
        assert_eq!(recipe_checks(&sed, Some(td_recipe::types::CheckTier::Pr)).len(), 0);
        assert_eq!(recipe_checks(&sed, Some(td_recipe::types::CheckTier::Daily)).len(), 1);
        assert_eq!(recipe_checks(&sed, None).len(), 1);

        let hello = catalog::lookup("hello").unwrap();
        assert_eq!(recipe_checks(&hello, Some(td_recipe::types::CheckTier::Pr)).len(), 0);
        assert_eq!(recipe_checks(&hello, Some(td_recipe::types::CheckTier::Daily)).len(), 1);
        assert_eq!(recipe_checks(&hello, None).len(), 1);
    }

    #[test]
    fn unchecked_recipes_have_zero_check_bodies() {
        let mes = catalog::lookup("mes").unwrap();
        assert_eq!(recipe_checks(&mes, None).len(), 0);
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
        let hello = catalog::lookup("hello").expect("hello recipe must exist (negative-control fixture)");
        let sed = catalog::lookup("sed").expect("sed recipe must exist (negative-control fixture)");
        assert_ne!(
            hello.to_json().to_canonical(),
            sed.to_json().to_canonical(),
            "hello and sed canon-equal — a JSON comparison would not discriminate a mismatch"
        );
    }
}
