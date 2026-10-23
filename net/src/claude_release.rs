//! Anthropic's signed native Claude Code release manifest.
//! The latest endpoint is a hint. Only the compiled RSA key authenticates
//! the exact manifest bytes; its version, size and digest bind the payload.

use crate::openpgp::{self, RsaKey};
use td_engine::json::{self, Json};

pub(crate) const REPOSITORY: &str = "https://downloads.claude.ai/claude-code-releases";
pub(crate) const MAX_MANIFEST: u64 = 64 * 1024;
pub(crate) const MAX_SIGNATURE: u64 = 16 * 1024;
pub(crate) const MAX_BINARY: u64 = td_engine::dynamic_application::MAX_DYNAMIC_ELF_BYTES;
const FINGERPRINT: &str = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE";
// Public-key packet body from https://downloads.claude.ai/keys/claude-code.asc.
const KEY_HEX: &str = concat!(
    "0469caef70011000a76f2b495e48f0798af022bd1a2c4151947f82d27178e0ac",
    "4573b810989e8f8d23cc19900377165c6a2c54ef3665f16c44a553b4c0b9881f",
    "c71fdb318e772b9983561aba0935bc76f66fc58184018240974f801f165eab9e",
    "75ea4462d939eae3ff802a138c90d4665b841811e7967bae8d2278a2bc447614",
    "b2904a0051f25918420b3e931d6e9845b7f4a7f846ff4f13398e928f7709eff0",
    "04a1345ff59995357d64d0abdb5128951a9198f4e2075a0c51254176a40ad4e2",
    "8fe193eb2a180804521f018ef8f0bf3ebeaae2d73e0fb2d18f1481bd4583a054",
    "a696a6ff3c9d618fd0ffec8d8e7b85d79e200f58c479e8abf11e5cceceb35074",
    "9ba36c9752902c21d598a3912777cbc6b25b959851319d45a6fdf4d5306a0ce5",
    "69b7bf55517a93327449c9f9ab73e2841d98f41c28d022a791893b6bb293efb1",
    "16bf42a4abe54798eb6a5f76b68615fe6fa6babdef6bcc9d6a6581a3fa86effc",
    "2dd2c752f352301fa95cbb4490dfdae0283f9b7683cbbf4e44a953184c5448d9",
    "0644b161984ccb340ceadf0882451f9d2ac83d31e2c5b0d60116786de30e063c",
    "723d92b59cfece3ab351dd390a26dc0b31001a6d44420e72f43e34b48b005353",
    "88edba6043d574942f25aea13bcc963ae942bd67575a03c8eb7438ba9a280d8c",
    "716b5df4f757842f32d84220ed39ab4e524b7c761b7669220988f8278f50f075",
    "a437fe9613e453530011010001",
);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Release {
    pub version: String,
    /// Lexically ordered bounded version components, used as an admission floor.
    pub order: u64,
    pub digest: String,
    pub size: u64,
}

pub(crate) fn version_order(version: &str) -> Result<u64, String> {
    let mut parts = version.split('.');
    let mut order = 0u64;
    for _ in 0..3 {
        let value = parts
            .next()
            .ok_or("release version needs three components")?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|c| c.is_ascii_digit())
        {
            return Err("release version is not canonical major.minor.patch".into());
        }
        let value = value
            .parse::<u64>()
            .map_err(|_| "release component overflows")?;
        if value >= 1 << 21 {
            return Err("release component exceeds 21 bits".into());
        }
        order = (order << 21) | value;
    }
    if parts.next().is_some() {
        return Err("release version has extra components".into());
    }
    Ok(order)
}

fn unhex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).map_err(|_| "invalid hex")?;
            u8::from_str_radix(text, 16).map_err(|_| "invalid hex".into())
        })
        .collect()
}

/// Decode the bounded, headerless armor Anthropic publishes. The detached
/// signature verifies the decoded packet; the armor CRC is transport metadata.
fn signature_packet(armor: &[u8]) -> Result<Vec<u8>, String> {
    if armor.len() as u64 > MAX_SIGNATURE {
        return Err("signature armor is too large".into());
    }
    let text = std::str::from_utf8(armor).map_err(|_| "signature armor is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some("-----BEGIN PGP SIGNATURE-----") || lines.next() != Some("") {
        return Err("signature armor header is invalid".into());
    }
    let mut encoded = Vec::new();
    let mut ended = false;
    let mut crc = false;
    for line in lines.by_ref() {
        if line == "-----END PGP SIGNATURE-----" {
            ended = true;
            break;
        }
        if line.starts_with('=') {
            if crc || line.len() != 5 {
                return Err("invalid signature armor CRC".into());
            }
            crc = true;
            continue;
        }
        if crc || line.is_empty() {
            return Err("invalid signature armor body".into());
        }
        encoded.extend_from_slice(line.as_bytes());
    }
    if !ended || lines.any(|line| !line.is_empty()) || !encoded.len().is_multiple_of(4) {
        return Err("signature armor is incomplete or has trailing data".into());
    }
    let sextet = |b: u8| -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok(u32::from(b - b'A')),
            b'a'..=b'z' => Ok(u32::from(b - b'a' + 26)),
            b'0'..=b'9' => Ok(u32::from(b - b'0' + 52)),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("invalid signature base64".into()),
        }
    };
    let mut out = Vec::new();
    let count = encoded.len() / 4;
    for (index, chunk) in encoded.as_chunks::<4>().0.iter().enumerate() {
        let [a, b, c, d] = chunk;
        let padding = usize::from(*c == b'=') + usize::from(*d == b'=');
        if padding > 0 && (index + 1 != count || *d != b'=') {
            return Err("invalid base64 padding".into());
        }
        let word = (sextet(*a)? << 18)
            | (sextet(*b)? << 12)
            | (if *c == b'=' { 0 } else { sextet(*c)? } << 6)
            | if *d == b'=' { 0 } else { sextet(*d)? };
        if (padding == 2 && word & 0xffff != 0) || (padding == 1 && word & 0xff != 0) {
            return Err("noncanonical base64 padding".into());
        }
        out.push((word >> 16) as u8);
        if padding < 2 {
            out.push((word >> 8) as u8);
        }
        if padding == 0 {
            out.push(word as u8);
        }
    }
    Ok(out)
}

pub(crate) fn verify(
    manifest: &[u8],
    signature: &[u8],
    version: &str,
    now: u64,
) -> Result<Release, String> {
    let order = version_order(version)?;
    if manifest.len() as u64 > MAX_MANIFEST {
        return Err("release manifest is too large".into());
    }
    let key = RsaKey::from_packet_body(&unhex(KEY_HEX)?)?;
    if key.fingerprint_hex() != FINGERPRINT {
        return Err("compiled Claude signing key mismatch".into());
    }
    openpgp::verify(&key, &signature_packet(signature)?, manifest, now)?;
    let text = std::str::from_utf8(manifest).map_err(|_| "manifest is not UTF-8")?;
    let json = json::parse(text).map_err(|e| format!("release manifest: {e}"))?;
    if json.get("version").and_then(Json::as_str) != Some(version) {
        return Err("signed manifest version does not match requested release".into());
    }
    let platform = json
        .get("platforms")
        .and_then(|p| p.get("linux-x64"))
        .ok_or("manifest has no linux-x64 platform")?;
    if platform.get("binary").and_then(Json::as_str) != Some("claude") {
        return Err("manifest names an unexpected binary".into());
    }
    let digest = platform
        .get("checksum")
        .and_then(Json::as_str)
        .ok_or("manifest has no checksum")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("manifest checksum is not canonical SHA-256".into());
    }
    let size = platform
        .get("size")
        .and_then(|value| match value {
            Json::Num(n) => n.parse::<u64>().ok(),
            _ => None,
        })
        .ok_or("manifest has no integer size")?;
    if size == 0 || size > MAX_BINARY {
        return Err("release binary exceeds size policy".into());
    }
    Ok(Release {
        version: version.into(),
        order,
        digest: digest.into(),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const MANIFEST: &[u8] = include_bytes!("fixtures/claude-2.1.309-manifest.json");
    const SIGNATURE: &[u8] = include_bytes!("fixtures/claude-2.1.309-manifest.asc");
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn published_manifest_verifies_and_binds_version_and_payload() {
        let release = verify(MANIFEST, SIGNATURE, "2.1.309", NOW).unwrap();
        assert_eq!(release.version, "2.1.309");
        assert!(release.size > 200_000_000 && release.size < MAX_BINARY);
        assert_eq!(release.digest.len(), 64);
        assert!(verify(MANIFEST, SIGNATURE, "2.1.310", NOW).is_err());
        let mut changed = MANIFEST.to_vec();
        changed.push(b' ');
        assert!(verify(&changed, SIGNATURE, "2.1.309", NOW).is_err());
        let mut signature = SIGNATURE.to_vec();
        signature[80] ^= 1;
        assert!(verify(MANIFEST, &signature, "2.1.309", NOW).is_err());
        assert!(verify(MANIFEST, SIGNATURE, "2.1.309", 0).is_err());
    }

    #[test]
    fn numeric_versions_order_without_lexical_or_overflow_ambiguity() {
        assert!(version_order("2.1.99").unwrap() < version_order("2.1.100").unwrap());
        assert!(version_order("2.99.0").unwrap() < version_order("3.0.0").unwrap());
        for bad in [
            "",
            "2.1",
            "2.1.1.0",
            "2.01.1",
            "2.1.1-beta",
            "../../3",
            "2097152.0.0",
        ] {
            assert!(version_order(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn armor_refuses_truncation_extra_packets_and_bad_base64() {
        for data in [
            b"".as_slice(),
            &SIGNATURE[..SIGNATURE.len() / 2],
            b"-----BEGIN PGP SIGNATURE-----\n\n!!!!\n-----END PGP SIGNATURE-----\n",
        ] {
            assert!(signature_packet(data).is_err());
        }
        let mut doubled = SIGNATURE.to_vec();
        doubled.extend_from_slice(SIGNATURE);
        assert!(signature_packet(&doubled).is_err());
    }
}
