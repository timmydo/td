//! profile — the user-package-manager PROFILE layer. `td-builder profile PROFILE-DIR PKG-OUT…`
//! unions the bin/sbin of installed package outputs into a symlink-tree profile (like a guix
//! profile / nix env): the step that turns the seed/`td shell` build engine into a durable,
//! inspectable install (build into a persistent store, link profile/bin/xyz -> store, put it on
//! PATH / link ~/bin/xyz). tests/profile.sh: td BUILDS hello + sed (build-recipe, no guix
//! process, stage0 builder), PLACES each into a persistent store ($store/<hash>-<name>), builds
//! a profile unioning them, and runs profile/bin/{hello,sed} + a ~/bin symlink into it. Asserts
//! behavioral (the binaries run through the profile + ~/bin chain), structural (profile entries
//! are symlinks INTO the store — the union), and a detected name COLLISION. Heavy (stage0 + two
//! source builds) → BUILD_GATES + HEAVY_GATES.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "profile",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> profile: td-builder profile unions installed packages into a symlink-tree profile; the binaries run through profile/bin (+ a ~/bin symlink) — the user-package-manager profile layer"
sh tests/profile.sh
"##,
    }
}
