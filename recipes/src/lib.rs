//! td-recipe — td's package-recipe surface, declared in Rust.
//!
//! Replaces the boa/TypeScript package surface (`ts-eval/` + `tests/ts/recipe-*.ts`
//! ): the recipe vocabulary is typed Rust (`types`), the catalog is
//! plain Rust data (`catalog`), and JSON I/O is a tiny hand-rolled module (`json`)
//! so the crate builds OFFLINE with no external dependencies. The emitted JSON is
//! the same shape the Guile lowering bridge already consumes from boa, so no bridge
//! change is needed (the consumer cutover is a follow-up).

pub mod application;
#[cfg(test)]
#[path = "../../td-busd/src/app_policy.rs"]
#[allow(dead_code)]
mod app_policy;
#[cfg(test)]
#[path = "../../td-authd/src/primary_account.rs"]
mod primary_account;
pub mod catalog;
// The build script's source scans, here for their tests only.
#[cfg(test)]
mod embed_scan;
// JSON value/parser/canonical writer lives in the shared, std-only td-engine
// (one copy for td-recipe-eval + td-builder). Re-exported so `crate::json::` /
// `td_recipe::json::` paths are unchanged.
pub use td_engine::json;
pub mod ladder;
pub mod ostree_pins;
#[cfg(test)]
pub mod permissions {
    pub use td_engine::permissions::*;
}
// The deployment contract, loaded ONCE for the whole lib. Two recipes name it —
// the installer check and the system image — and a `#[path]` include in each
// would load the same file twice, giving them distinct types for one contract.
#[path = "../../td-boot/src/protocol.rs"]
#[allow(dead_code)]
pub mod td_boot_protocol;
// Keep the ISO composer's shared file admission inside the catalog scan.
#[path = "../../td-boot/src/realfile.rs"]
pub mod td_boot_realfile;
// Shared check assertions must participate in the catalog dependency scan.
use td_boot_realfile as realfile;
#[path = "../../td-install/src/timezones.rs"]
pub mod td_install_timezones;
#[path = "../../td-compositor/src/timezone.rs"]
pub mod td_compositor_timezone;

// Keep the native guest oracle contract inside the catalog dependency scan.
#[path = "../../td-install-qemu-test/src/protocol.rs"]
pub mod td_install_qemu_protocol;
// The boot oracle and target evidence command consume one literal; loading the
// crate-owned file here keeps the image and host check from drifting.
pub mod source_pins;
#[path = "../../td-update/src/upstream.rs"]
pub mod release_upstream;
#[path = "../../td-profiler/src/contract.rs"]
pub mod td_profiler_contract;
pub mod types;

#[cfg(test)]
mod timezone_catalog_tests {
    #[test]
    fn tzdata_check_tracks_the_installer_catalog_source() {
        assert!(crate::catalog::named_dirs("tzdata").contains(&"td-install"));
        assert!(crate::catalog::named_dirs("tzdata").contains(&"td-compositor"));
    }
}
