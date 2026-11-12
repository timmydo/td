#![deny(unsafe_code)]
//! Fallible allocation and explicitly owned, hierarchical storage budgets.

#[path = "lib.rs"]
mod helpers;
pub use helpers::*;

mod budget;
mod raw;
mod storage;
pub use budget::{Budget, BudgetError, Charge};
pub use storage::{Arena, BudgetString, BudgetVec, Buffer};
