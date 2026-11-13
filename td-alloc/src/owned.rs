#![deny(unsafe_code)]
//! Fallible allocation and explicitly owned, hierarchical storage budgets.

#[path = "lib.rs"]
mod helpers;
pub use helpers::*;

mod budget;
mod raw;
pub use raw::SmallText;
mod storage;
pub use budget::{Budget, BudgetError, Charge};
pub use storage::{
    Arena, BudgetIntoIter, BudgetMap, BudgetShared, BudgetString, BudgetVec, Buffer,
};
