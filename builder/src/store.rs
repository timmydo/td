//! Store-path hashing — the daemon's content-addressed naming
//! (nix/libstore/store-api.cc makeStorePath / makeTextPath, nix/libutil/hash.cc
//! printHash32 + compressHash, read off the pin). Lets td compute a `.drv`'s own
//! store path (and, later, derivation output paths) WITHOUT guile — the
//! evaluator-as-library track.

use crate::sha256::{self, Sha256};

/// Guix's store directory (the daemon's `storeDir`).
pub const STORE_DIR: &str = "/gnu/store";

/// The nix store-path base-32 alphabet (omits e, o, u, t).
const BASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// nix base-32 of a digest, low-bit-first, MSB char first — the exact order
/// printHash32 emits (n from nrChars-1 down to 0).
pub fn base32(hash: &[u8]) -> String {
    let nchars = (hash.len() * 8 + 4) / 5; // ceil(bits / 5)
    let mut s = Vec::with_capacity(nchars);
    for n in (0..nchars).rev() {
        let b = n * 5;
        let (i, j) = (b / 8, b % 8);
        let c = (hash[i] as u16 >> j)
            | if i + 1 < hash.len() {
                (hash[i + 1] as u16) << (8 - j)
            } else {
                0
            };
        s.push(BASE32[(c & 0x1f) as usize]);
    }
    String::from_utf8(s).expect("base32 alphabet is ASCII")
}

/// XOR-fold a hash down to `size` bytes (nix compressHash).
fn compress_hash(hash: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    for (i, &b) in hash.iter().enumerate() {
        out[i % size] ^= b;
    }
    out
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// makeStorePath(type, inner-hash-hex, name): fingerprint
/// `type:sha256:<hex>:<storeDir>:<name>`, hashed and compressed to 20 bytes, then
/// base-32 — the store path's digest part.
pub fn make_store_path(ty: &str, inner_hash_hex: &str, name: &str) -> String {
    let fingerprint = format!("{ty}:sha256:{inner_hash_hex}:{STORE_DIR}:{name}");
    let compressed = compress_hash(&sha256_bytes(fingerprint.as_bytes()), 20);
    format!("{STORE_DIR}/{}-{}", base32(&compressed), name)
}

/// makeTextPath: a `.drv` (or any addTextToStore item) is content-addressed with
/// its references carried in the type (`text:` + the sorted refs), and the inner
/// hash is sha256 of the file content.
pub fn make_text_path(name: &str, content: &[u8], refs: &[String]) -> String {
    let mut refs = refs.to_vec();
    refs.sort();
    // The daemon builds `type = "text"` and appends `":" + ref` per reference
    // (computeStorePathForText), so the EMPTY-reference set is bare `"text"`, not
    // `"text:"`. (Not reachable from a real .drv — every one has >=1 input — but
    // correct for the general case the follow-on wiring will exercise.)
    let ty = if refs.is_empty() {
        "text".to_string()
    } else {
        format!("text:{}", refs.join(":"))
    };
    let content_hex = sha256::to_base16(&sha256_bytes(content));
    make_store_path(&ty, &content_hex, name)
}

/// The `<name>` part of a store path `/gnu/store/<32-char digest>-<name>` — the
/// 32-char base-32 digest plus its `-` separator are fixed-width.
pub fn name_from_store_path(path: &str) -> Option<String> {
    let base = path.rsplit('/').next()?;
    // 32 digest chars + '-'; the name itself may contain '-'.
    if base.len() > 33 && base.as_bytes()[32] == b'-' {
        Some(base[33..].to_string())
    } else {
        None
    }
}

/// The store path a `.drv` with `content` and `refs` (its inputDrvs ∪ inputSrcs)
/// would be written to.
pub fn drv_store_path(name: &str, content: &[u8], refs: &[String]) -> String {
    make_text_path(name, content, refs)
}

use crate::drv::{self, Derivation, Output};
use std::collections::HashMap;

/// A fixed-output derivation: a single `out` output carrying a hash (the daemon's
/// `isFixedOutput`). Its modulo-hash is a flat function of the hash, not the ATerm.
fn is_fixed_output(d: &Derivation) -> bool {
    d.outputs.len() == 1
        && d.outputs[0].name == "out"
        && !d.outputs[0].hash_algo.is_empty()
        && !d.outputs[0].hash.is_empty()
}

/// `hashDerivationModulo` (nix/libstore/derivations.cc), recursive: a fixed-output
/// drv hashes `fixed:out:<algo>:<hash>:<path>`; a normal drv hashes its ATerm with
/// every inputDrv path replaced by base-16 of ITS modulo-hash (recursed with
/// outputs UNmasked) and, when `mask_outputs`, its own output paths + output-named
/// env vars blanked. Memoized by drv path. `read` reads a `.drv` by store path.
fn hash_derivation_modulo(
    d: &Derivation,
    mask_outputs: bool,
    cache: &mut HashMap<String, [u8; 32]>,
    read: &impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<[u8; 32], String> {
    if is_fixed_output(d) {
        let o = &d.outputs[0];
        let s = format!("fixed:out:{}:{}:{}", o.hash_algo, o.hash, o.path);
        return Ok(sha256_bytes(s.as_bytes()));
    }
    // Replace each inputDrv path with base-16 of its modulo-hash; sort by the hex
    // (the daemon writes inputs2 as a map keyed on the hash).
    let mut inputs2: Vec<(String, Vec<String>)> = Vec::with_capacity(d.input_drvs.len());
    for (path, outs) in &d.input_drvs {
        let h = match cache.get(path) {
            Some(c) => *c,
            None => {
                let bytes = read(path)?;
                let dd = drv::parse(&bytes).map_err(|e| format!("{path}: {e}"))?;
                let hh = hash_derivation_modulo(&dd, false, cache, read)?;
                cache.insert(path.clone(), hh);
                hh
            }
        };
        inputs2.push((sha256::to_base16(&h), outs.clone()));
    }
    inputs2.sort();

    let output_names: Vec<&str> = d.outputs.iter().map(|o| o.name.as_str()).collect();
    let masked = Derivation {
        outputs: d
            .outputs
            .iter()
            .map(|o| Output {
                name: o.name.clone(),
                path: if mask_outputs { String::new() } else { o.path.clone() },
                hash_algo: o.hash_algo.clone(),
                hash: o.hash.clone(),
            })
            .collect(),
        input_drvs: inputs2,
        input_srcs: d.input_srcs.clone(),
        platform: d.platform.clone(),
        builder: d.builder.clone(),
        args: d.args.clone(),
        env: d
            .env
            .iter()
            .map(|(k, v)| {
                let blank = mask_outputs && output_names.contains(&k.as_str());
                (k.clone(), if blank { String::new() } else { v.clone() })
            })
            .collect(),
    };
    Ok(sha256_bytes(drv::serialize(&masked).as_bytes()))
}

/// Compute the output store path for output `out_name` of a NORMAL derivation
/// `d` whose name is `drv_name` (the `.drv` basename without the `.drv` suffix):
/// `make-store-path("output:<name>", hashDerivationModulo(d, mask), drv_name[-name])`.
pub fn output_path(
    d: &Derivation,
    drv_name: &str,
    out_name: &str,
    read: &impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<String, String> {
    let mut cache = HashMap::new();
    let h = hash_derivation_modulo(d, true, &mut cache, read)?;
    Ok(output_path_from_modulo(&sha256::to_base16(&h), drv_name, out_name))
}

fn output_path_from_modulo(modulo_hex: &str, drv_name: &str, out_name: &str) -> String {
    let name = if out_name == "out" {
        drv_name.to_string()
    } else {
        format!("{drv_name}-{out_name}")
    };
    make_store_path(&format!("output:{out_name}"), modulo_hex, &name)
}

/// CONSTRUCT a `.drv` (the evaluator-as-library payload): from the skeleton in `d`
/// (builder/args/inputs/env, with output paths NOT yet known), compute every
/// output path via `hashDerivationModulo`, fill them into the outputs AND the
/// output-named env vars, serialize the ATerm, and compute the `.drv`'s own store
/// path. Returns `(drv_store_path, content)`. This is what guix's `derivation`
/// does — now in Rust. (The skeleton's env order / input order are taken as given:
/// the daemon sorts both; a producer must hand them already sorted.)
pub fn construct_drv(
    d: &Derivation,
    drv_name: &str,
    read: &impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<(String, String), String> {
    // A fixed-output drv's output path uses makeFixedOutputPath, not the
    // makeOutputPath formula below — refuse it loudly rather than emit a wrong
    // path. (Not produced by td-build; guards the follow-on.)
    if is_fixed_output(d) {
        return Err("construct_drv: fixed-output derivations are unsupported".into());
    }
    let mut cache = HashMap::new();
    let modulo_hex = sha256::to_base16(&hash_derivation_modulo(d, true, &mut cache, read)?);

    // The output path for each output (normal drv: empty hash_algo/hash kept).
    let out_paths: HashMap<String, String> = d
        .outputs
        .iter()
        .map(|o| (o.name.clone(), output_path_from_modulo(&modulo_hex, drv_name, &o.name)))
        .collect();

    let rebuilt = Derivation {
        outputs: d
            .outputs
            .iter()
            .map(|o| Output {
                name: o.name.clone(),
                path: out_paths[&o.name].clone(),
                hash_algo: o.hash_algo.clone(),
                hash: o.hash.clone(),
            })
            .collect(),
        input_drvs: d.input_drvs.clone(),
        input_srcs: d.input_srcs.clone(),
        platform: d.platform.clone(),
        builder: d.builder.clone(),
        args: d.args.clone(),
        // Fill each output-named env var with its computed path.
        env: d
            .env
            .iter()
            .map(|(k, v)| match out_paths.get(k) {
                Some(p) => (k.clone(), p.clone()),
                None => (k.clone(), v.clone()),
            })
            .collect(),
    };

    let content = drv::serialize(&rebuilt);
    let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
    refs.extend(d.input_srcs.iter().cloned());
    let path = make_text_path(&format!("{drv_name}.drv"), content.as_bytes(), &refs);
    Ok((path, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_extraction() {
        assert_eq!(
            name_from_store_path("/gnu/store/2nfg943asrl9dv64zrr1a4kpb25mfafd-hello-2.12.2.drv")
                .unwrap(),
            "hello-2.12.2.drv"
        );
        assert_eq!(name_from_store_path("/gnu/store/short").is_none(), true);
    }

    #[test]
    fn base32_length_for_20_bytes() {
        // A 20-byte (compressed) hash encodes to 32 base-32 chars — store-path width.
        assert_eq!(base32(&[0u8; 20]).len(), 32);
        assert_eq!(base32(&[0u8; 20]), "0".repeat(32));
    }
}
