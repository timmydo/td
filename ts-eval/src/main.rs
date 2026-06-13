use boa_engine::{Context, Source};
use std::io::Read;

// Hermetic curation prelude (DESIGN §7.1 ts-frontend "Hermetic eval"). boa ships
// no fetch/fs/process/web APIs, so only language-level nondeterminism needs
// neutering before user code runs: remove the clock (Date) and deny randomness.
// It also installs the lowering entrypoint `system()` (sub-task 4/5): a td spec
// ends with `system(spec)`, and we stash the argument so the evaluator can emit
// it as JSON for the Guile lowering bridge (tests/ts-diff.scm).
const PRELUDE: &str = r#"
(function () {
  "use strict";
  delete globalThis.Date;
  Math.random = function () { throw new Error("hermetic-eval: Math.random is denied"); };
  globalThis.system = function (spec) { globalThis.__td_system = spec; };
})();
"#;

// After user code runs: if the spec declared a system, emit it as JSON (the
// lowering bridge's input); otherwise fall back to the bare eval result (so
// `1 + 2 * 3` still prints 7 — the ts-eval rung's boa-runs assertion).
const CAPTURE: &str =
    "(typeof globalThis.__td_system !== 'undefined') ? JSON.stringify(globalThis.__td_system) : null";

fn main() {
    let mut user = String::new();
    std::io::stdin().read_to_string(&mut user).expect("read stdin");
    let mut ctx = Context::default();
    ctx.eval(Source::from_bytes(PRELUDE.as_bytes()))
        .expect("curate global");
    let result = match ctx.eval(Source::from_bytes(user.as_bytes())) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("eval error: {e}");
            std::process::exit(1);
        }
    };
    let captured = ctx
        .eval(Source::from_bytes(CAPTURE.as_bytes()))
        .expect("capture system()");
    let out = if captured.is_null() {
        result
            .to_string(&mut ctx)
            .expect("to_string")
            .to_std_string_escaped()
    } else {
        captured
            .to_string(&mut ctx)
            .expect("to_string")
            .to_std_string_escaped()
    };
    println!("{out}");
}
