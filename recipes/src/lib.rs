//! td-recipe — td's package-recipe surface, declared in Rust.
//!
//! Replaces the boa/TypeScript package surface (`ts-eval/` + `tests/ts/recipe-*.ts`
//! ): the recipe vocabulary is typed Rust (`types`), the catalog is
//! plain Rust data (`catalog`), and JSON I/O is a tiny hand-rolled module (`json`)
//! so the crate builds OFFLINE with no external dependencies. The emitted JSON is
//! the same shape the Guile lowering bridge already consumes from boa, so no bridge
//! change is needed (the consumer cutover is a follow-up).

pub mod catalog;
pub mod json;
pub mod types;
