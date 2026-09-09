//! Git identity and task-branch grammar shared by the manager and dispatcher.
#![forbid(unsafe_code)]

pub fn instance_valid(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn branch_valid(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 200
        && branch != "main"
        && branch != "HEAD"
        && !branch.starts_with("refs/")
        && !branch.contains("..")
        && branch.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}
