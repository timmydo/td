//! Git identity and task-branch grammar shared by the manager and dispatcher.
#![forbid(unsafe_code)]

pub fn instance_valid(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub use crate::vm_wire::workspace::branch_valid;
