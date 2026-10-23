//! td-recipe — td's package-recipe surface, declared in Rust.
//!
//! Replaces the boa/TypeScript package surface (`ts-eval/` + `tests/ts/recipe-*.ts`
//! ): the recipe vocabulary is typed Rust (`types`), the catalog is
//! plain Rust data (`catalog`), and JSON I/O is a tiny hand-rolled module (`json`)
//! so the crate builds OFFLINE with no external dependencies. The emitted JSON is
//! the same shape the Guile lowering bridge already consumes from boa, so no bridge
//! change is needed (the consumer cutover is a follow-up).

#[cfg(test)]
#[path = "../../td-busd/src/app_policy.rs"]
#[allow(dead_code)]
mod app_policy;
pub mod application;
pub mod catalog;
#[cfg(test)]
#[path = "../../td-authd/src/primary_account.rs"]
mod primary_account;
// The trees a local-source recipe's files name, for recipe tests.
#[cfg(test)]
mod source_closure;
// JSON value/parser/canonical writer lives in the shared, std-only td-engine
// (one copy for td-recipe-eval + td-builder). Re-exported so `crate::json::` /
// `td_recipe::json::` paths are unchanged.
pub use td_engine::json;
pub mod shared;
#[cfg(test)]
pub mod permissions {
    pub use td_engine::permissions::*;
}
// td-jail's authority, compiled by the td-jail recipe's tests, resolves
// cache-served specs through these.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../td-jail/src/flathub.rs"]
mod flathub;
#[cfg(test)]
pub mod flathub_app {
    pub use td_engine::flathub_app::*;
}
// The deployment contract, loaded ONCE for the whole lib. Two recipes name it —
// the installer check and the system image — and a `#[path]` include in each
// would load the same file twice, giving them distinct types for one contract.
#[path = "../../td-boot/src/protocol.rs"]
#[allow(dead_code)]
pub mod td_boot_protocol;
#[path = "../../td-civil/src/tzif.rs"]
pub mod td_civil_tzif;
// Keep the ISO composer's shared file admission inside the catalog scan; the
// installer's zone table, compiled below, reaches it as `super::td_fs`.
#[path = "../../td-fs/src/real_file.rs"]
pub mod td_fs;
#[path = "../../td-install/src/timezones.rs"]
pub mod td_install_timezones;
#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

// Keep the native guest oracle contract inside the catalog dependency scan.
#[path = "../../td-install-qemu-test/src/protocol.rs"]
pub mod td_install_qemu_protocol;
// The boot oracle and target evidence command consume one literal; loading the
// crate-owned file here keeps the image and host check from drifting.
#[path = "../../td-update/src/upstream.rs"]
pub mod release_upstream;
pub mod source_pins;
#[path = "../../td-profiler/src/contract.rs"]
pub mod td_profiler_contract;
pub mod types;

#[cfg(test)]
mod timezone_catalog_tests {
    #[test]
    fn tzdata_check_tracks_the_installer_catalog_source() {
        // Assembled: this file is under the build script's own scan.
        let shared = crate::catalog::shared_embeds();
        for (dir, file) in [
            ("td-install", "/src/timezones.rs"),
            ("td-civil", "/src/tzif.rs"),
        ] {
            let path = [dir, file].concat();
            assert!(shared.contains(&path.as_str()), "{shared:?}");
        }
    }
}
