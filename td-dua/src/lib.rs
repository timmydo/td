//! td-dua: a disk usage analyzer (`DESIGN.md`). The scan, the tree, the
//! list's rows, the treemap and the window's state are library modules so
//! the tests drive them; the binary adds the window and the worker.
#![forbid(unsafe_code)]

pub mod app;
pub mod delete;
pub mod scan;
pub mod tree;
pub mod treemap;
pub mod view;
pub mod worker;
