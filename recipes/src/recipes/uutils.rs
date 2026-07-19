use crate::types::{Recipe, SourcePin};

// uutils ships its coreutils implementation as one multicall binary. The
// published 0.9.0 `unix` aggregate cannot be used: it enables `stdbuf`, whose
// crates.io archive lacks src/libstdbuf and therefore embeds an empty preload
// library. Select the aggregate's Unix groups directly, excluding only stdbuf.
// Optional ACL/SELinux/systemd integrations stay out too; their native libraries
// are not part of td's final userland yet.
pub fn recipe() -> Recipe {
    Recipe::rust("uutils", "0.9.0")
        .source_pin(SourcePin::new(
            "uutils-source",
            "https://static.crates.io/crates/coreutils/coreutils-0.9.0.crate",
            "b92df9b821533650f3797aadae46e547f72db281c1f8a27f381f36d54284d34b",
            "coreutils-0.9.0.crate",
        ))
        .bins(&["coreutils"])
        .no_default_features()
        .features(&[
            "feat_Tier1",
            "feat_require_unix_core",
            "feat_require_unix_hostid",
            "feat_require_unix_utmpx",
        ])
}
