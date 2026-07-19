use crate::types::{Recipe, SourcePin};

// uutils ships its coreutils implementation as one multicall binary. Enable the
// Unix utility set while leaving optional ACL/SELinux/systemd integrations out:
// those would add native libraries that are not part of td's final userland yet.
pub fn recipe() -> Recipe {
    Recipe::rust("uutils", "0.9.0")
        .source_pin(SourcePin::new(
            "uutils-source",
            "https://static.crates.io/crates/coreutils/coreutils-0.9.0.crate",
            "b92df9b821533650f3797aadae46e547f72db281c1f8a27f381f36d54284d34b",
            "coreutils-0.9.0.crate",
        ))
        .bins(&["coreutils"])
        .features(&["unix"])
}
