//! Native compilation caches must distinguish installed runtime closures.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// td's installed tools resolve into immutable store outputs. In particular,
/// a new /bin/cc can link a new loader without Cargo observing an input change.
/// This key separates caches; it does not authenticate cached build outputs.
pub fn key(tools: &[&Path]) -> Result<String, String> {
    let mut identity = b"td-native-runtime-v1\0".to_vec();
    for tool in tools {
        let resolved = fs::canonicalize(tool)
            .map_err(|error| format!("resolve native tool {}: {error}", tool.display()))?;
        identity.extend_from_slice(resolved.as_os_str().as_bytes());
        identity.push(0);
    }
    Ok(crate::sha256::hex_digest(&identity))
}
