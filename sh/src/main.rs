//! td-sh — td's seed shell: the brush bash-compatible shell (pure Rust, MIT)
//! wrapped as a td binary, so the bootstrap ladder's rungs can declare their
//! shell as a td seed tool instead of taking a /gnu/store bash from the host
//! (re #469: seed/recipe-only execution provenance).
//!
//! The wrapper is deliberately one call: brush-shell's `entry::run()` IS the
//! bash-compatible CLI (`-c`, script file + args, `-e`/`-x`/`-u`, `-s`,
//! `--sh`, `+O`, …), and every flag-parsing corner case it covers is
//! compatibility td would otherwise re-implement and maintain. Policy — which
//! rungs use td-sh, what goes on PATH — lives in the recipes, not here.

fn main() {
    brush_shell::entry::run();
}
