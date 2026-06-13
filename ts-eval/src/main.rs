use boa_engine::{Context, Source};
use std::io::Read;

// Hermetic curation prelude (DESIGN §7.1 ts-frontend "Hermetic eval"). boa ships
// no fetch/fs/process/web APIs, so only language-level nondeterminism needs
// neutering before user code runs: remove the clock (Date) and deny randomness.
const PRELUDE: &str = r#"
(function () {
  "use strict";
  delete globalThis.Date;
  Math.random = function () { throw new Error("hermetic-eval: Math.random is denied"); };
})();
"#;

fn main() {
    let mut user = String::new();
    std::io::stdin().read_to_string(&mut user).expect("read stdin");
    let mut ctx = Context::default();
    ctx.eval(Source::from_bytes(PRELUDE.as_bytes())).expect("curate global");
    match ctx.eval(Source::from_bytes(user.as_bytes())) {
        Ok(v) => {
            let s = v.to_string(&mut ctx).expect("to_string").to_std_string_escaped();
            println!("{s}");
        }
        Err(e) => {
            eprintln!("eval error: {e}");
            std::process::exit(1);
        }
    }
}
