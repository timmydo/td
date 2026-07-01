//! Store-path hashing — the daemon's content-addressed naming
//! (nix/libstore/store-api.cc makeStorePath / makeTextPath, nix/libutil/hash.cc
//! printHash32 + compressHash, read off the pin). Lets td compute a `.drv`'s own
//! store path (and, later, derivation output paths) WITHOUT guile — the
//! evaluator-as-library track.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::sha256::{self, Sha256};

/// The DEFAULT store directory — guix's (the daemon's `storeDir`). td's own store
/// prefix is `/td/store` when `TD_STORE_DIR` is set (user-pm Phase 1, the break from
/// guix): the prefix is part of every content hash, so `/td/store` paths are a
/// distinct store from guix's `/gnu/store`.
pub const STORE_DIR: &str = "/gnu/store";

/// The active store prefix: `$TD_STORE_DIR` (e.g. `/td/store`) or the default
/// `/gnu/store`. Read where a store path is computed or recognized so a single env
/// var re-prefixes the whole store (like nix's `NIX_STORE_DIR`).
pub fn store_dir() -> String {
    match std::env::var("TD_STORE_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => STORE_DIR.to_string(),
    }
}

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
    make_store_path_in(&store_dir(), ty, inner_hash_hex, name)
}

/// makeStorePath with an EXPLICIT store prefix — the prefix is in the fingerprint, so
/// re-prefixing (`/gnu/store` → `/td/store`) re-hashes the path (a distinct store).
/// `make_store_path` is this with the active `store_dir()`.
pub fn make_store_path_in(store_dir: &str, ty: &str, inner_hash_hex: &str, name: &str) -> String {
    let fingerprint = format!("{ty}:sha256:{inner_hash_hex}:{store_dir}:{name}");
    let compressed = compress_hash(&sha256_bytes(fingerprint.as_bytes()), 20);
    format!("{store_dir}/{}-{}", base32(&compressed), name)
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

/// An INPUT-ADDRESSED store path: the digest is `key_hex` (a hash of the artifact's
/// DECLARED INPUTS, not of its built content), named like a normal derivation `out`
/// output (`make-store-path("output:out", key, name)`). This is the stable key the
/// toolchain needs — `store-add-recursive` content-addresses by the recursive NAR
/// hash, so a non-byte-reproducible tree (the modern toolchain's cc1 stamp / ar
/// mtimes) lands at a path that VARIES build-to-build; an input-addressed path is a
/// pure function of the inputs, so it is identical across rebuilds and a td-subst
/// consumer can compute it from the lock BEFORE fetching. The placed bytes are still
/// registered with their REAL NAR hash (the daemon's `output:` semantics — naming and
/// content-integrity are orthogonal), so the closure/verify machinery is unchanged.
pub fn input_addressed_path(key_hex: &str, name: &str) -> String {
    output_path_from_modulo(key_hex, name, "out")
}

/// The parsed `td-toolchain.lock` — the toolchain's declared INPUT set, the source of
/// truth for its stable input-addressed key. The lock is line-based (`field value`,
/// like the seed/source locks): one `name`, one `recipe-rev`, one or more `component`
/// (the toolchain's parts, e.g. gcc-14.3.0), and the pinned `input`/`patch`
/// `<sha256> <file>` lines (mirrors of seed/sources/*.lock + the vendored boot
/// patches — the gate asserts they stay in sync). The KEY hashes ALL of these, so the
/// input-addressed path changes iff a declared input changes (load-bearing) and is
/// content-independent (stable across non-reproducible rebuilds).
pub struct ToolchainLock {
    pub name: String,
    pub recipe_rev: String,
    pub components: Vec<String>,
    /// Canonical `"<sha256> <file>"` lines for sources.
    pub inputs: Vec<String>,
    /// Canonical `"<sha256> <file>"` lines for vendored patches.
    pub patches: Vec<String>,
}

impl ToolchainLock {
    pub fn parse(content: &str) -> Result<ToolchainLock, String> {
        let (mut name, mut recipe_rev) = (String::new(), String::new());
        let (mut components, mut inputs, mut patches) = (Vec::new(), Vec::new(), Vec::new());
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, val) = line.split_once(' ').map(|(k, v)| (k, v.trim())).unwrap_or((line, ""));
            match key {
                "name" => {
                    if !name.is_empty() {
                        return Err("td-toolchain.lock: duplicate `name`".into());
                    }
                    name = val.to_string();
                }
                "recipe-rev" => {
                    if !recipe_rev.is_empty() {
                        return Err("td-toolchain.lock: duplicate `recipe-rev`".into());
                    }
                    recipe_rev = val.to_string();
                }
                "component" => components.push(val.to_string()),
                "input" | "patch" => {
                    let (sha, file) = val.split_once(' ').ok_or_else(|| {
                        format!("td-toolchain.lock: malformed `{key}` line (want `<sha256> <file>`): {val}")
                    })?;
                    let canon = format!("{} {}", sha.trim(), file.trim());
                    if key == "input" {
                        inputs.push(canon);
                    } else {
                        patches.push(canon);
                    }
                }
                _ => return Err(format!("td-toolchain.lock: unknown field `{key}`")),
            }
        }
        if name.is_empty() {
            return Err("td-toolchain.lock: missing `name`".into());
        }
        if recipe_rev.is_empty() {
            return Err("td-toolchain.lock: missing `recipe-rev`".into());
        }
        if components.is_empty() {
            return Err("td-toolchain.lock: needs at least one `component`".into());
        }
        if inputs.is_empty() {
            return Err("td-toolchain.lock: needs at least one `input`".into());
        }
        Ok(ToolchainLock { name, recipe_rev, components, inputs, patches })
    }

    /// The toolchain's stable INPUT key: sha256 (base-16) over a canonical, ORDER-
    /// INDEPENDENT serialization of every declared input. Sorting the multi-valued
    /// fields means the key depends on the SET of inputs, not the lock's line order.
    pub fn key(&self) -> String {
        let mut components = self.components.clone();
        components.sort();
        let mut inputs = self.inputs.clone();
        inputs.sort();
        let mut patches = self.patches.clone();
        patches.sort();
        let mut canon = String::new();
        canon.push_str(&format!("name={}\n", self.name));
        canon.push_str(&format!("recipe-rev={}\n", self.recipe_rev));
        for c in &components {
            canon.push_str(&format!("component={c}\n"));
        }
        for i in &inputs {
            canon.push_str(&format!("input={i}\n"));
        }
        for p in &patches {
            canon.push_str(&format!("patch={p}\n"));
        }
        sha256::to_base16(&sha256_bytes(canon.as_bytes()))
    }

    /// The input-addressed store path for `name` (a component, or the toolchain's own
    /// `name` when `None`) under the active `store_dir()`.
    pub fn path_for(&self, name: Option<&str>) -> String {
        input_addressed_path(&self.key(), name.unwrap_or(&self.name))
    }
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

/// td-drv-assemble: ASSEMBLE a `.drv` from a raw line-based spec — the last guile
/// `(derivation …)` removed. Guile resolves the inputs and emits the spec (name /
/// system / builder / arg / input-drv `<path> <out,..>` / input-src / `env k=v`,
/// WITHOUT the output paths or the `out` env var); td does the ASSEMBLY that
/// `(derivation …)` does — add the `out` output + its env var, SORT env by key and
/// inputs/sources by path (the daemon's canonical order) — then `construct_drv`
/// computes the output path + serializes. Byte-identical to guix's `(derivation …)`.
pub fn assemble_drv(
    spec: &str,
    read: &impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<(String, String), String> {
    let (mut name, mut system, mut builder) = (String::new(), String::new(), String::new());
    let mut args: Vec<String> = Vec::new();
    let mut input_drvs: Vec<(String, Vec<String>)> = Vec::new();
    let mut input_srcs: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    for line in spec.lines() {
        if let Some(r) = line.strip_prefix("name ") {
            name = r.to_string();
        } else if let Some(r) = line.strip_prefix("system ") {
            system = r.to_string();
        } else if let Some(r) = line.strip_prefix("builder ") {
            builder = r.to_string();
        } else if let Some(r) = line.strip_prefix("arg ") {
            args.push(r.to_string());
        } else if let Some(r) = line.strip_prefix("input-drv ") {
            let (path, outs) = r.split_once(' ').ok_or("malformed input-drv line")?;
            let mut o: Vec<String> = outs.split(',').map(String::from).collect();
            o.sort();
            input_drvs.push((path.to_string(), o));
        } else if let Some(r) = line.strip_prefix("input-src ") {
            input_srcs.push(r.to_string());
        } else if let Some(r) = line.strip_prefix("env ") {
            let (k, v) = r.split_once('=').ok_or("malformed env line")?;
            env.push((k.to_string(), v.to_string()));
        } else if line.is_empty() {
        } else {
            return Err(format!("unknown spec line: {line}"));
        }
    }
    if name.is_empty() || builder.is_empty() || system.is_empty() {
        return Err("spec missing name/builder/system".to_string());
    }
    // A single `out` output (path computed by construct_drv); its env var is added
    // blank for construct_drv to fill. Then SORT env by key and inputs by path —
    // exactly the canonical order `(derivation …)`/unparseDerivation impose.
    let outputs = vec![Output {
        name: "out".to_string(),
        path: String::new(),
        hash_algo: String::new(),
        hash: String::new(),
    }];
    env.push(("out".to_string(), String::new()));
    env.sort();
    input_drvs.sort();
    input_srcs.sort();
    let d = Derivation {
        outputs,
        input_drvs,
        input_srcs,
        platform: system,
        builder,
        args,
        env,
    };
    construct_drv(&d, &name, read)
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

    #[test]
    fn re_prefix_changes_the_path_and_the_hash() {
        // user-pm Phase 1: the store prefix is configurable. /td/store paths share the
        // 32-char digest WIDTH but are a DISTINCT store — the prefix is in the fingerprint,
        // so the digest differs from /gnu/store's for the same content. (Break from guix.)
        let (ty, h, name) = ("source", "abc123", "hello-2.12.2");
        let guix = make_store_path_in("/gnu/store", ty, h, name);
        let td = make_store_path_in("/td/store", ty, h, name);
        assert!(guix.starts_with("/gnu/store/") && guix.ends_with("-hello-2.12.2"));
        assert!(td.starts_with("/td/store/") && td.ends_with("-hello-2.12.2"));
        // Same name, same 32-char digest width — but a DIFFERENT digest (the prefix is hashed).
        assert_eq!(name_from_store_path(&guix).unwrap(), name_from_store_path(&td).unwrap());
        assert_ne!(
            guix.rsplit('/').next().unwrap(),
            td.rsplit('/').next().unwrap(),
            "re-prefixing must re-hash — /td/store is a distinct store, not a rename"
        );
        // `make_store_path` follows TD_STORE_DIR (default /gnu/store when unset).
        assert!(make_store_path(ty, h, name).starts_with(&format!("{}/", store_dir())));
    }

    #[test]
    fn input_addressed_path_is_a_function_of_key_and_name_only() {
        // The whole point: the path is content-INDEPENDENT. Same (key, name) ⇒ same
        // path; a different key ⇒ a different path (the inputs are load-bearing).
        let a = input_addressed_path("deadbeef", "td-toolchain");
        let b = input_addressed_path("deadbeef", "td-toolchain");
        assert_eq!(a, b, "same key+name must yield the same input-addressed path");
        assert!(a.ends_with("-td-toolchain"));
        assert_ne!(
            a,
            input_addressed_path("cafef00d", "td-toolchain"),
            "a different key must move the path (inputs are load-bearing)"
        );
        assert_ne!(
            a,
            input_addressed_path("deadbeef", "glibc-2.41"),
            "a different name must move the path"
        );
    }

    #[test]
    fn toolchain_lock_key_is_order_independent_and_load_bearing() {
        let l1 = ToolchainLock::parse(
            "name td-toolchain\nrecipe-rev 1\ncomponent gcc-14.3.0\ncomponent glibc-2.41\n\
             input aaaa gcc-14.3.0.tar.xz\ninput bbbb glibc-2.41.tar.xz\npatch cccc boot.patch\n",
        )
        .unwrap();
        // Same SET of inputs, different LINE ORDER ⇒ identical key (it hashes the set).
        let l2 = ToolchainLock::parse(
            "name td-toolchain\nrecipe-rev 1\ncomponent glibc-2.41\ncomponent gcc-14.3.0\n\
             input bbbb glibc-2.41.tar.xz\ninput aaaa gcc-14.3.0.tar.xz\npatch cccc boot.patch\n",
        )
        .unwrap();
        assert_eq!(l1.key(), l2.key(), "key must be order-independent");
        // Perturb one pin ⇒ the key (and so the path) changes — self-discrimination.
        let l3 = ToolchainLock::parse(
            "name td-toolchain\nrecipe-rev 1\ncomponent gcc-14.3.0\ncomponent glibc-2.41\n\
             input aaaa gcc-14.3.0.tar.xz\ninput bbbX glibc-2.41.tar.xz\npatch cccc boot.patch\n",
        )
        .unwrap();
        assert_ne!(l1.key(), l3.key(), "perturbing a pin must change the key");
        // recipe-rev is part of the key too (a recipe change with unchanged inputs re-keys).
        let l4 = ToolchainLock::parse(
            "name td-toolchain\nrecipe-rev 2\ncomponent gcc-14.3.0\ncomponent glibc-2.41\n\
             input aaaa gcc-14.3.0.tar.xz\ninput bbbb glibc-2.41.tar.xz\npatch cccc boot.patch\n",
        )
        .unwrap();
        assert_ne!(l1.key(), l4.key(), "bumping recipe-rev must change the key");
    }

    #[test]
    fn toolchain_lock_rejects_malformed() {
        assert!(ToolchainLock::parse("recipe-rev 1\ncomponent x\ninput a f\n").is_err()); // no name
        assert!(ToolchainLock::parse("name x\ncomponent y\ninput a f\n").is_err()); // no recipe-rev
        assert!(ToolchainLock::parse("name x\nrecipe-rev 1\ninput a f\n").is_err()); // no component
        assert!(ToolchainLock::parse("name x\nrecipe-rev 1\ncomponent y\n").is_err()); // no input
        assert!(ToolchainLock::parse("name x\nrecipe-rev 1\ncomponent y\ninput a f\nbogus z\n").is_err());
        assert!(ToolchainLock::parse("name x\nrecipe-rev 1\ncomponent y\ninput nosha\n").is_err());
    }
}
