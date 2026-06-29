//! td-recipe-eval — emit / list / verify recipes from the Rust catalog.
//!
//! Subcommands:
//!   list                  print every recipe's `.ts` file stem, one per line
//!   emit STEM             print STEM's recipe as canonical JSON (the Guile
//!                         lowering bridge's wire format)
//!   verify STEM BOA.json  parse the boa-evaluated JSON for the same recipe and
//!                         assert it canon-equals the Rust recipe (the removable
//!                         migration oracle); exits non-zero on mismatch
//!
//! This is the loop test tool the `recipe-rs` gate drives. `emit` is also the
//! future corpus consumer entry (replacing `ts-emit` on the boa path).

use std::process::exit;

use td_recipe::{catalog, json};

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
        Some("verify") => {
            let stem = args.get(2).unwrap_or_else(|| die("usage: verify STEM BOA.json"));
            let path = args.get(3).unwrap_or_else(|| die("usage: verify STEM BOA.json"));
            let rust_canon = lookup_or_die(stem).to_json().to_canonical();
            let boa_text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| die(&format!("cannot read {path}: {e}")));
            let boa = json::parse(boa_text.trim())
                .unwrap_or_else(|e| die(&format!("{path}: invalid JSON: {e}")));
            let boa_canon = boa.to_canonical();
            if rust_canon == boa_canon {
                eprintln!("ok: {stem} — Rust recipe canon-equals boa");
            } else {
                eprintln!("MISMATCH {stem}:");
                eprintln!("  rust: {rust_canon}");
                eprintln!("  boa : {boa_canon}");
                exit(1);
            }
        }
        _ => die("usage: td-recipe-eval list | emit STEM | verify STEM BOA.json"),
    }
}
