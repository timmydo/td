//! td-recipe-eval — emit / list / verify recipes from the Rust catalog.
//!
//! Subcommands (recipes):
//!   list                  print every recipe's `.ts` file stem, one per line
//!   emit STEM             print STEM's recipe as canonical JSON (the wire format
//!                         the build path consumes)
//!   verify STEM BOA.json  parse the boa-evaluated JSON for the same recipe and
//!                         assert it canon-equals the Rust recipe (the removable
//!                         migration oracle); exits non-zero on mismatch
//! Subcommands (system specs):
//!   list-specs            print every system spec's `.ts` file stem
//!   emit-spec STEM        print STEM's system spec as canonical JSON
//!   verify-spec STEM BOA.json   canon-equal check against boa, like `verify`
//!
//! This is the loop tool the `recipe-rs` gate drives AND the corpus/spec consumer
//! entry (replacing `ts-emit` on the boa path).

use std::process::exit;

use td_recipe::{catalog, json, specs};

fn die(msg: &str) -> ! {
    eprintln!("td-recipe-eval: {msg}");
    exit(2);
}

fn lookup_or_die(stem: &str) -> td_recipe::types::Recipe {
    match catalog::lookup(stem) {
        Some(r) => r,
        None => die(&format!("unknown recipe stem '{stem}' (try `list`)")),
    }
}

fn lookup_spec_or_die(stem: &str) -> td_recipe::types::SystemSpec {
    match specs::lookup(stem) {
        Some(s) => s,
        None => die(&format!("unknown spec stem '{stem}' (try `list-specs`)")),
    }
}

// Canon-equal check against a boa-emitted JSON file (the removable oracle).
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
        Some("verify") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: verify STEM BOA.json"));
            let path = args.get(3).unwrap_or_else(|| die("usage: verify STEM BOA.json"));
            let rust_canon = lookup_or_die(stem).to_json().to_canonical();
            verify_against_boa("recipe", stem, &rust_canon, path);
        }
        Some("list-specs") => {
            for (stem, _) in specs::all() {
                println!("{stem}");
            }
        }
        Some("emit-spec") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: emit-spec STEM"));
            println!("{}", lookup_spec_or_die(stem).to_json().to_canonical());
        }
        Some("verify-spec") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: verify-spec STEM BOA.json"));
            let path = args.get(3).unwrap_or_else(|| die("usage: verify-spec STEM BOA.json"));
            let rust_canon = lookup_spec_or_die(stem).to_json().to_canonical();
            verify_against_boa("spec", stem, &rust_canon, path);
        }
        _ => die("usage: td-recipe-eval list|emit|verify | list-specs|emit-spec|verify-spec STEM [BOA.json]"),
    }
}
