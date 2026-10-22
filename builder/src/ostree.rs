//! `ostree-materialize`'s publication: td-engine's deploy materializer with
//! the builder's `renameat2(RENAME_NOREPLACE)` wrapper, the syscall surface's
//! one caller (UNSAFE.md).

use std::path::Path;

use td_engine::ostree_deploy::MaterializedStats;

use crate::sys;

pub(crate) fn materialize(
    cache: &Path,
    exact_ref: &str,
    commit: &str,
    content: &str,
    destination: &Path,
) -> Result<MaterializedStats, String> {
    td_engine::ostree_deploy::materialize(
        cache,
        exact_ref,
        commit,
        content,
        destination,
        sys::rename_noreplace,
    )
}
