//! td-recipe-eval — emit / list / verify recipes from the Rust catalog.
//!
//! Subcommands (recipes):
//!   list                  print every recipe's `.ts` file stem, one per line
//!   emit STEM             print STEM's recipe as canonical JSON (the wire format
//!                         the build path consumes)
//!   verify STEM BOA.json  parse the boa-evaluated JSON for the same recipe and
//!                         assert it canon-equals the Rust recipe (the removable
//!                         migration oracle); exits non-zero on mismatch
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

use td_recipe::{catalog, json};

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

// Canon-equal check against a boa-emitted JSON file (the removable oracle).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn verify_against_boa(kind: &str, stem: &str, rust_canon: &str, path: &str) {
    let boa_text =
        std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("cannot read {path}: {e}")));
    let boa = json::parse(boa_text.trim())
        .unwrap_or_else(|e| die(&format!("{path}: invalid JSON: {e}")));
    let boa_canon = boa.to_canonical();
    if rust_canon == boa_canon {
        eprintln!("ok: {stem} — Rust {kind} canon-equals boa");
    } else {
        eprintln!("MISMATCH {stem}:");
        eprintln!("  rust: {rust_canon}");
        eprintln!("  boa : {boa_canon}");
        exit(1);
    }
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
        Some("meta") => {
            // A canonical JSON manifest of every recipe's census-relevant metadata
            // (stem, buildSystem, inputs, perturbed). The guix-dependence census
            // reads the committed tests/recipes-meta.json (rust-free, cheap)
            // instead of scanning tests/ts/recipe-*.ts; recipe-rs asserts this
            // command stays in sync with that file.
            use td_recipe::json::Json;
            let mut arr = Vec::new();
            for (stem, r) in catalog::all() {
                let inputs = r
                    .inputs
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(Json::Str)
                    .collect();
                arr.push(Json::Obj(vec![
                    ("stem".into(), Json::Str(stem.to_string())),
                    ("buildSystem".into(), Json::Str(r.build_system_name().into())),
                    ("inputs".into(), Json::Arr(inputs)),
                    ("perturbed".into(), Json::Bool(stem.contains("perturbed"))),
                ]));
            }
            println!("{}", Json::Arr(arr).to_canonical());
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
        Some("verify") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: verify STEM BOA.json"));
            let path = args.get(3).unwrap_or_else(|| die("usage: verify STEM BOA.json"));
            let rust_canon = lookup_or_die(stem).to_json().to_canonical();
            verify_against_boa("recipe", stem, &rust_canon, path);
        }
        _ => die("usage: td-recipe-eval list|meta|emit|verify|check-list|check-count|check-script ..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_filter_counts_recipe_check_bodies() {
        let sed = catalog::lookup("sed").unwrap();
        assert_eq!(recipe_checks(&sed, Some(td_recipe::types::CheckTier::Pr)).len(), 0);
        assert_eq!(recipe_checks(&sed, Some(td_recipe::types::CheckTier::Daily)).len(), 2);
        assert_eq!(recipe_checks(&sed, None).len(), 2);

        let hello = catalog::lookup("hello").unwrap();
        assert_eq!(recipe_checks(&hello, Some(td_recipe::types::CheckTier::Pr)).len(), 1);
        assert_eq!(recipe_checks(&hello, Some(td_recipe::types::CheckTier::Daily)).len(), 1);
        assert_eq!(recipe_checks(&hello, None).len(), 2);
    }

    #[test]
    fn unchecked_recipes_have_zero_check_bodies() {
        let td_builder = catalog::lookup("td-builder").unwrap();
        assert_eq!(recipe_checks(&td_builder, None).len(), 0);
    }
}
