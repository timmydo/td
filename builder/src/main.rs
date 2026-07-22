//! td-builder — td's own builder (DESIGN §7.1 side-track).
//!
//! Goal of the track: a td-owned Rust binary that executes a `.drv` in a
//! user-namespace sandbox and registers the output, proven behaviorally
//! equivalent to the pinned `guix-daemon` (prime directive 4 — the daemon is
//! the oracle; never replace without a differential).
//!
//! Grown rung by rung, each with its own daemon differential:
//!   • S1 — toolchain probe: the bare invocation prints a stable sentinel the
//!     `td-builder` rung greps (proves the COMPILED BINARY ran — stronger than
//!     "cargo build exited 0");
//!   • S2 — `nar-hash PATH`: NAR serializer + SHA-256, bit-for-bit equal to
//!     the daemon's recorded hash (the rung's S2 leg diffs them);
//!   • S3 — an ATerm `.drv` parser + a userns build sandbox + store
//!     registration;
//!   • S4 — the daemon-vs-td-builder store differential, as a check.sh rung.

mod affected;
mod bootstrap;
mod build;
mod build_daemon;
mod bzip2;
mod check_loop;
mod daily;
mod drv;
mod elf;
mod erofs;
// The comment-splice static guard (#300) is exercised only by its own `#[test]`
// (the cargo-test tier) — gate it to test builds so it adds no dead-code surface
// to the release binary or the clippy pass.
#[cfg(test)]
mod gate_lint;
mod gate_bodies;
mod gate_inputs;
mod gate_timing;
mod gates;
mod gzip;
mod json;
mod lock;
mod mes_boot;
mod nar;
mod oci;
mod sandbox;
mod scan;
mod sha256;
mod stage0;
mod store;
mod store_db;
mod store_db_read;
mod sys;
mod tar;
mod toolchain_x86_64;
mod xz;

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Stream-hash a tree/file in NAR form — one implementation, shared with the
/// staging boundary's verifier (`sandbox::verify_staged_item`) and the
/// loop-userland cache so every `sha256:<hex>` record is produced identically.
fn nar_hash_path(path: &Path) -> Result<String, std::io::Error> {
    sandbox::nar_hash_of(path)
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nar_hash(path: &str) -> Result<String, std::io::Error> {
    nar_hash_path(Path::new(path))
}

/// Adapter: hash AND count the NAR bytes in one serialization pass (the seed
/// manifest needs both the NAR hash and the NAR size — the daemon's `narSize`).
struct HashSizeWriter {
    hasher: sha256::Sha256,
    size: u64,
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
impl std::io::Write for HashSizeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.size += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The (NAR hash, NAR size) of a path — one serialization pass.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nar_hash_size_path(path: &Path) -> Result<(String, u64), std::io::Error> {
    let mut w = HashSizeWriter { hasher: sha256::Sha256::new(), size: 0 };
    nar::write_nar(&mut w, path)?;
    Ok((format!("sha256:{}", sha256::to_base16(&w.hasher.finalize())), w.size))
}

fn read_arg_bytes(path: &str) -> Result<Vec<u8>, String> {
    if path == "-" {
        let mut buf = Vec::new();
        let mut stdin = std::io::stdin();
        std::io::Read::read_to_end(&mut stdin, &mut buf)
            .map_err(|e| format!("read stdin: {e}"))?;
        return Ok(buf);
    }
    std::fs::read(path).map_err(|e| format!("read {path}: {e}"))
}

fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn first_line_with_prefix(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::to_string))
}

fn last_line_with_prefix(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .rev()
        .filter_map(|line| line.strip_prefix(prefix).map(str::to_string))
        .next()
}

fn first_line_containing(text: &str, needle: &str) -> Option<String> {
    text.lines()
        .find(|line| line.contains(needle))
        .map(str::to_string)
}

fn count_line_exact(text: &str, needle: &str) -> usize {
    text.lines().filter(|line| *line == needle).count()
}

fn count_nonempty_lines(text: &str) -> usize {
    text.lines().filter(|line| !line.is_empty()).count()
}

fn cargo_test_reported_nonzero_tests(text: &str) -> bool {
    let marker = "test result: ok. ";
    text.lines().any(|line| {
        let Some(rest) = line.split_once(marker).map(|(_, r)| r) else {
            return false;
        };
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }
        let Some(num_s) = rest.get(..digits) else {
            return false;
        };
        let Some(tail) = rest.get(digits..) else {
            return false;
        };
        tail.starts_with(" passed") && num_s.parse::<u64>().is_ok_and(|n| n > 0)
    })
}

fn contains_gcc_lib_ref(text: &str) -> bool {
    text.lines()
        .any(|line| line.contains("-gcc-") && line.contains("-lib"))
}

fn text_cli(args: &[String]) -> ExitCode {
    let fail = |msg: &str| {
        eprintln!("td-builder: text: {msg}");
        ExitCode::FAILURE
    };
    match args {
        [op, needle, file] if op == "contains" => match read_arg_bytes(file) {
            Ok(bytes) if bytes_contains(&bytes, needle.as_bytes()) => ExitCode::SUCCESS,
            Ok(_) => ExitCode::FAILURE,
            Err(e) => fail(&e),
        },
        [op, needle, file] if op == "not-contains" => match read_arg_bytes(file) {
            Ok(bytes) if !bytes_contains(&bytes, needle.as_bytes()) => ExitCode::SUCCESS,
            Ok(_) => ExitCode::FAILURE,
            Err(e) => fail(&e),
        },
        [op, needle, file] if op == "line-exact" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                if text.lines().any(|line| line == needle) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => fail(&e),
        },
        [op, needle, file] if op == "count-line-exact" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                println!("{}", count_line_exact(&text, needle));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        [op, file] if op == "count-nonempty" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                println!("{}", count_nonempty_lines(&text));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        [op, prefix, file] if op == "extract-prefix" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                match first_line_with_prefix(&text, prefix) {
                    Some(v) => {
                        println!("{v}");
                        ExitCode::SUCCESS
                    }
                    None => ExitCode::FAILURE,
                }
            }
            Err(e) => fail(&e),
        },
        [op, prefix, file] if op == "extract-prefix-last" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                match last_line_with_prefix(&text, prefix) {
                    Some(v) => {
                        println!("{v}");
                        ExitCode::SUCCESS
                    }
                    None => ExitCode::FAILURE,
                }
            }
            Err(e) => fail(&e),
        },
        [op, needle, file] if op == "extract-containing" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                match first_line_containing(&text, needle) {
                    Some(v) => {
                        println!("{v}");
                        ExitCode::SUCCESS
                    }
                    None => ExitCode::FAILURE,
                }
            }
            Err(e) => fail(&e),
        },
        [op, file] if op == "sha256" => match sha256::sha256_file(Path::new(file)) {
            Ok(h) => {
                println!("{h}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("sha256 {file}: {e}")),
        },
        [op, file] if op == "cargo-test-ok" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                if cargo_test_reported_nonzero_tests(&text) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => fail(&e),
        },
        [op, file] if op == "contains-gcc-lib" => match read_arg_bytes(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                if contains_gcc_lib_ref(&text) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => fail(&e),
        },
        _ => {
            eprintln!("usage: td-builder text contains NEEDLE FILE|-");
            eprintln!("       td-builder text not-contains NEEDLE FILE|-");
            eprintln!("       td-builder text line-exact LINE FILE|-");
            eprintln!("       td-builder text count-line-exact LINE FILE|-");
            eprintln!("       td-builder text count-nonempty FILE|-");
            eprintln!("       td-builder text extract-prefix PREFIX FILE|-");
            eprintln!("       td-builder text extract-prefix-last PREFIX FILE|-");
            eprintln!("       td-builder text extract-containing NEEDLE FILE|-");
            eprintln!("       td-builder text sha256 FILE");
            eprintln!("       td-builder text cargo-test-ok FILE");
            eprintln!("       td-builder text contains-gcc-lib FILE");
            ExitCode::from(2)
        }
    }
}

fn collect_regular_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if meta.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(path).map_err(|e| format!("read dir {}: {e}", path.display()))? {
            let entry = entry.map_err(|e| format!("read dir {}: {e}", path.display()))?;
            entries.push(entry.path());
        }
        entries.sort();
        for child in entries {
            collect_regular_files(&child, out)?;
        }
    }
    Ok(())
}

fn regular_files_under(args: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for arg in args {
        collect_regular_files(Path::new(arg), &mut files)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_named_file_entries(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(path).map_err(|e| format!("read dir {}: {e}", path.display()))? {
            let entry = entry.map_err(|e| format!("read dir {}: {e}", path.display()))?;
            entries.push(entry.path());
        }
        entries.sort();
        for child in entries {
            collect_named_file_entries(&child, out)?;
        }
        return Ok(());
    }
    let file_type = meta.file_type();
    if meta.is_file() || file_type.is_symlink() {
        out.push(path.to_path_buf());
    }
    Ok(())
}

fn named_file_entries_under(args: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for arg in args {
        collect_named_file_entries(Path::new(arg), &mut files)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let starts_with_star = pattern.starts_with('*');
    let ends_with_star = pattern.ends_with('*');
    let mut rest = text;
    let mut first = true;
    for part in pattern.split('*').filter(|p| !p.is_empty()) {
        if first && !starts_with_star {
            let Some(next) = rest.strip_prefix(part) else {
                return false;
            };
            rest = next;
            first = false;
            continue;
        }
        let Some(pos) = rest.find(part) else {
            return false;
        };
        let Some(next) = rest.get(pos + part.len()..) else {
            return false;
        };
        rest = next;
        first = false;
    }
    ends_with_star || rest.is_empty()
}

fn first_file_named(pattern: &str, roots: &[String]) -> Result<Option<PathBuf>, String> {
    for file in named_file_entries_under(roots)? {
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if wildcard_match(pattern, name) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn tree_fingerprint(args: &[String]) -> Result<String, String> {
    let files = regular_files_under(args)?;
    let mut h = sha256::Sha256::new();
    for file in files {
        let path = file.to_string_lossy();
        let digest = sha256::sha256_file(&file).map_err(|e| format!("sha256 {}: {e}", file.display()))?;
        h.update(path.as_bytes());
        h.update(b"\0");
        h.update(digest.as_bytes());
        h.update(b"\n");
    }
    Ok(sha256::to_base16(&h.finalize()))
}

fn tree_first_containing(needle: &str, roots: &[String]) -> Result<Option<PathBuf>, String> {
    for file in regular_files_under(roots)? {
        let bytes = std::fs::read(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
        if bytes_contains(&bytes, needle.as_bytes()) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn path_older_than(path: &str, days: &str) -> Result<bool, String> {
    let days = days
        .parse::<u64>()
        .map_err(|e| format!("parse days `{days}`: {e}"))?;
    let secs = days
        .checked_mul(86_400)
        .ok_or_else(|| format!("days `{days}` is too large"))?;
    let modified = std::fs::metadata(path)
        .map_err(|e| format!("stat {path}: {e}"))?
        .modified()
        .map_err(|e| format!("mtime {path}: {e}"))?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Ok(age.as_secs() > secs)
}

fn unique_lock_path(lock_file: &str, stem: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(lock_file).map_err(|e| format!("read {lock_file}: {e}"))?;
    let entries = lock::parse(&text, "")?;
    let mut hits: Vec<&str> = entries
        .iter()
        .map(|e| e.path.as_str())
        .filter(|p| gate_inputs::path_names_stem(p, stem))
        .collect();
    hits.sort_unstable();
    hits.dedup();
    match hits.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(format!("{lock_file}: no path names `{stem}`")),
        many => Err(format!(
            "{lock_file}: `{stem}` is ambiguous ({} matches: {})",
            many.len(),
            many.join(", ")
        )),
    }
}

fn lock_paths(lock_file: &str, prefix: Option<&str>) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(lock_file).map_err(|e| format!("read {lock_file}: {e}"))?;
    let entries = lock::parse(&text, "")?;
    let mut out: Vec<String> = entries
        .into_iter()
        .map(|e| e.path)
        .filter(|p| prefix.is_none_or(|pre| p.starts_with(pre)))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

fn rewrite_gcc_toolchain_lock_body(text: &str, toolchain: &str, glibc: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut replaced = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            out.push_str(raw);
            out.push('\n');
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        let is_gcc_toolchain = toks
            .get(1)
            .is_some_and(|p| gate_inputs::path_names_stem(p, "gcc-toolchain"))
            || toks.first().is_some_and(|name| *name == "gcc-toolchain");
        if is_gcc_toolchain {
            if !replaced {
                out.push_str(&format!("gcc-toolchain {toolchain} seed\n"));
                out.push_str(&format!("glibc-2.41 {glibc} seed\n"));
                replaced = true;
            }
            continue;
        }
        out.push_str(raw);
        out.push('\n');
    }
    if replaced {
        Ok(out)
    } else {
        Err("no gcc-toolchain line to rewrite".to_string())
    }
}

fn lock_cli(args: &[String]) -> ExitCode {
    let fail = |msg: String| {
        eprintln!("td-builder: lock: {msg}");
        ExitCode::FAILURE
    };
    match args {
        [op, lock_file, stem] if op == "path" => match unique_lock_path(lock_file, stem) {
            Ok(p) => {
                println!("{p}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        [op, lock_file] if op == "paths" => match lock_paths(lock_file, None) {
            Ok(paths) => {
                for p in paths {
                    println!("{p}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        [op, lock_file, prefix] if op == "paths" => match lock_paths(lock_file, Some(prefix)) {
            Ok(paths) => {
                for p in paths {
                    println!("{p}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        [op, input, output, toolchain, glibc] if op == "rewrite-gcc-toolchain" => {
            let run = || -> Result<(), String> {
                let text = std::fs::read_to_string(input)
                    .map_err(|e| format!("read {input}: {e}"))?;
                let body = rewrite_gcc_toolchain_lock_body(&text, toolchain, glibc)?;
                std::fs::write(output, body).map_err(|e| format!("write {output}: {e}"))
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(e),
            }
        }
        _ => {
            eprintln!("usage: td-builder lock path LOCK STEM");
            eprintln!("       td-builder lock paths LOCK [PREFIX]");
            eprintln!("       td-builder lock rewrite-gcc-toolchain IN OUT TOOLCHAIN GLIBC");
            ExitCode::from(2)
        }
    }
}

fn parse_u64_prefix(s: &str) -> Option<(u64, &str)> {
    let n = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let num = s.get(..n)?.parse::<u64>().ok()?;
    let rest = s.get(n..)?;
    Some((num, rest))
}

fn daemon_budget_stats(text: &str, budget: u64) -> Result<(u64, u64), String> {
    let budget_msg = format!("budget {budget} concurrent builds");
    if !text.contains(&budget_msg) {
        return Err(format!("daemon log does not contain `{budget_msg}`"));
    }
    let mut peak = 0u64;
    let mut starts = 0u64;
    for line in text.lines() {
        if line.contains("daemon build START") {
            starts = starts.saturating_add(1);
        }
        let Some(rest) = line.split_once("START (").map(|(_, r)| r) else {
            continue;
        };
        let Some((active, rest)) = parse_u64_prefix(rest) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix('/') else {
            continue;
        };
        let Some((seen_budget, _)) = parse_u64_prefix(rest) else {
            continue;
        };
        if seen_budget == budget {
            peak = peak.max(active);
        }
    }
    Ok((peak, starts))
}

fn daemon_budget_check(log_file: &str, budget: &str) -> Result<(u64, u64), String> {
    let budget = budget
        .parse::<u64>()
        .map_err(|e| format!("parse budget `{budget}`: {e}"))?;
    let text = std::fs::read_to_string(log_file).map_err(|e| format!("read {log_file}: {e}"))?;
    let (peak, starts) = daemon_budget_stats(&text, budget)?;
    if peak != budget {
        return Err(format!("peak active builds = {peak}, expected {budget}"));
    }
    let min_starts = budget.saturating_add(1);
    if starts < min_starts {
        return Err(format!(
            "only {starts} build starts observed, expected at least {min_starts}"
        ));
    }
    Ok((peak, starts))
}

// --- substitute server: export half (store-coupled, dependency-free) ---
// Write a serve-able directory for a store closure: a td-native `<basename>.narinfo` per
// member + `nar/<narhash-hex>.nar`. This is the dual of `seed-manifest`/`seed-unpack` —
// the seed pair captures a closure into ONE tarball + manifest; the substitute export
// serves each path on its OWN, addressable by basename, so a consumer can fetch just the
// paths it lacks. The networked `subst/` binary signs + serves this dir and the consumer
// verifies + restores it (with `nar-restore`); this half stays in the dependency-free
// engine because it needs the store DB + NAR serializer, not crypto/HTTP. Same reader +
// `write_nar` as seed-manifest, so the served bytes match the daemon's.

/// One member of a substitute export.
struct SubstMember {
    store_path: String,           // logical path, e.g. /gnu/store/<hash>-name
    physical: std::path::PathBuf, // where to read it on disk (== store_path on live store)
    refs: Vec<String>,            // direct references (logical store paths)
}

/// The basename (`<hash>-name`) of a store path.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn store_basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Render a td-native narinfo (minimal, line-oriented). References are recorded as
/// basenames so the record is store-location independent; the consumer rebases them onto
/// its own store dir. The signature line (`Sig:`) is appended later by the signer.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn narinfo_text(
    store_path: &str,
    narhash: &str,
    narsize: u64,
    narfile: &str,
    ref_basenames: &[String],
) -> String {
    format!(
        "StorePath: {store_path}\nNarHash: {narhash}\nNarSize: {narsize}\nNarFile: {narfile}\nReferences: {}\n",
        ref_basenames.join(" ")
    )
}

/// Write a serve-able substitute directory for MEMBERS into OUTDIR. Returns the basenames
/// written. Each member yields `OUTDIR/<basename>.narinfo` + `OUTDIR/nar/<narhash>.nar`.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn subst_export(outdir: &Path, members: &[SubstMember]) -> std::io::Result<Vec<String>> {
    let nardir = outdir.join("nar");
    std::fs::create_dir_all(&nardir)?;
    let mut written = Vec::new();
    for m in members {
        let (narhash, narsize) = nar_hash_size_path(&m.physical)?;
        let hex = narhash.strip_prefix("sha256:").unwrap_or(&narhash);
        let narfile = format!("nar/{hex}.nar");
        let mut f = std::fs::File::create(nardir.join(format!("{hex}.nar")))?;
        nar::write_nar(&mut f, &m.physical)?;
        drop(f);
        let base = store_basename(&m.store_path);
        let refbases: Vec<String> =
            m.refs.iter().map(|r| store_basename(r).to_string()).collect();
        let text = narinfo_text(&m.store_path, &narhash, narsize, &narfile, &refbases);
        std::fs::write(outdir.join(format!("{base}.narinfo")), text)?;
        written.push(base.to_string());
    }
    Ok(written)
}

/// Build the `SubstMember` list to export for ROOTS — paths + their direct refs read from DB,
/// each member's bytes taken from `STORE_DIR/<basename>`. With `walk_closure`, ROOTS expands to
/// its full closure over DB's Refs graph (a whole-closure mirror). Without, EXACTLY the roots
/// are exported — per-output granularity, so a publisher of a single build output need not
/// stage that output's whole closure into STORE_DIR (its external refs live elsewhere). The
/// narinfo still lists each path's refs as basenames either way, so a consumer can scan-verify
/// the restored bytes. (td-builder's OWN substitute-consumer hook was deleted, re #469; the
/// format's consumer half is proven by the `restore_substitute` round-trip test.)
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn subst_export_members(
    db: &store_db_read::Db,
    store_dir: &str,
    roots: &[String],
    walk_closure: bool,
) -> Result<Vec<SubstMember>, String> {
    let mut paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in roots {
        // db.closure errors if the root is not in the DB; in paths-only mode we keep that
        // existence check but take only the root itself (not its refs).
        let c = db.closure(r)?;
        if walk_closure {
            for p in c {
                paths.insert(p);
            }
        } else {
            paths.insert(r.clone());
        }
    }
    let refs = db.refs_by_path()?;
    Ok(paths
        .iter()
        .map(|p| {
            let mut rs = refs.get(p).cloned().unwrap_or_default();
            rs.sort();
            rs.dedup();
            let base = p.rsplit('/').next().unwrap_or(p);
            SubstMember {
                store_path: p.clone(),
                physical: Path::new(store_dir).join(base),
                refs: rs,
            }
        })
        .collect())
}

/// The fixed logical store path under which the guix-less-runner harness ships as a single
/// whole-tree substitute (issue #314). Unlike the lock-keyed toolchain CLOSURES (whose name a
/// consumer recomputes from the lock), the /td/store harness — the busybox+make set, the staged
/// C toolchain, the /td/store/ld loader, and the `rel`/`toolchain` metadata — is a
/// content-addressed BUILD OUTPUT with no derivable lock name, so it ships as ONE nar of the
/// whole `.td-build-cache/harness` tree under this fixed name. Integrity is the signed NarHash;
/// trust is the pinned ed25519 key (tests/td-subst.pub). A forger without the private key cannot
/// mint a valid `td-harness.narinfo`, so the worst a store-writer can do is a signed DOWNGRADE
/// to an older td-published harness — acceptable for an optimization the consumer fails CLOSED on.
const HARNESS_SUBST_STORE_PATH: &str = "/td/store/td-harness";

/// Export the harness tree at `harness_dir` (the `.td-build-cache/harness` layout: `store/` +
/// `rel` + `toolchain`) as a single substitute: one nar of the WHOLE tree + a `td-harness.narinfo`
/// (StorePath == `HARNESS_SUBST_STORE_PATH`, no References), written under `outdir`. No store DB is
/// needed — the harness is content-addressed by its NarHash, not by a lock. The old daily
/// publisher is retired; this helper is dormant until the recipe-graph harness path has a current
/// producer again. Returns the written basenames (exactly one: `td-harness`).
fn harness_subst_export(outdir: &Path, harness_dir: &Path) -> Result<Vec<String>, String> {
    if !harness_dir.join("store").is_dir() || !harness_dir.join("rel").is_file() {
        return Err(format!(
            "{} is not a harness tree (expected store/ + rel)",
            harness_dir.display()
        ));
    }
    let member = SubstMember {
        store_path: HARNESS_SUBST_STORE_PATH.to_string(),
        physical: harness_dir.to_path_buf(),
        refs: Vec::new(),
    };
    subst_export(outdir, std::slice::from_ref(&member)).map_err(|e| e.to_string())
}

/// The `path` column (index 1) of a read `ValidPaths` row, or "" if absent.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn path_at(cols: &[store_db_read::Value]) -> &str {
    match cols.get(1) {
        Some(store_db_read::Value::Text(p)) => p,
        _ => "",
    }
}

/// Recreate the tree at `src` under `dst` as a canonical store entry — the
/// daemon's addToStore canonicalization, for the properties NAR (hence the
/// content-addressed store path) actually captures: the tree STRUCTURE, file
/// CONTENTS, the file EXECUTABLE bit, and SYMLINK targets. NAR omits directory
/// permissions, the read/write permission bits, and mtimes, so those are not
/// reproduced (dirs are left writable so the scratch copy can be cleaned up);
/// regular files get the canonical `0555`/`0444` by their source exec bit, which
/// is the one perm NAR encodes. Mirrors `(guix serialization) write-file`.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn copy_canonical(src: &Path, dst: &Path) -> Result<(), String> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let md = std::fs::symlink_metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src).map_err(|e| format!("{}: {e}", src.display()))?;
        symlink(&target, dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        for entry in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_canonical(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        // Regular file: copy contents and set the canonical mode by the source's
        // executable bit (the only permission NAR distinguishes). Key off OWNER-exec
        // (`0o100`) — exactly what the daemon's canonicaliser (S_IXUSR) and td's own
        // NAR serializer (`nar.rs`) use, so the restored tree's NAR matches the source's.
        let content = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
        std::fs::write(dst, &content).map_err(|e| format!("{}: {e}", dst.display()))?;
        let exec = md.permissions().mode() & 0o100 != 0;
        let mode = if exec { 0o555 } else { 0o444 };
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    Ok(())
}

/// Remove PATH (file, dir, or symlink) if present; a missing path is not an error. Never
/// follows a symlink — removes the link itself.
fn remove_store_path(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => {
            std::fs::remove_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Acquire the exclusive per-store commit lock (`<db>.commit.lock`), so the sweep, the
/// registered-path snapshot, the tree commits, the db merge, and the receipt write form one
/// critical section per store/db. The client-side ladder lock serializes recipe-check builds
/// but does NOT cover a committer that reaches the same cache another way — a parent-death-
/// orphaned builder child, or a direct `store-commit`/`build-recipe` with TD_PERSIST_* at the
/// cache; since recovery DELETES a torn orphan, an unlocked second writer racing the snapshot
/// could clobber a just-registered path. Different stores use different lock files, so
/// unrelated builds never contend. Blocks until free; the guard releases on drop / process exit.
fn lock_store_commit(db: &Path) -> Result<std::fs::File, String> {
    // Site the lock as a SIBLING of the store directory (db's parent), never inside it: the
    // shared build-cache's whole dir is renamed aside and recreated by over-cap eviction, so a
    // lock kept inside would get a fresh inode across an evict/recreate and stop excluding a
    // committer holding the old one. A sibling keeps one stable lock inode.
    let anchor = db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(db);
    let mut os = anchor.as_os_str().to_owned();
    os.push(".commit.lock");
    let lock_path = std::path::PathBuf::from(os);
    if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("open commit lock {}: {e}", lock_path.display()))?;
    file.lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    Ok(file)
}

/// Sibling staging path for an atomic commit of DEST: same parent dir (hence the same
/// filesystem, so `rename` into place is atomic), a `.commit-tmp.` prefix a sweep can find
/// (pid FIRST so the sweep can parse it), and a `.staging` suffix so a staged receipt temp
/// never ends in `.receipt` and gets mistaken for a real sidecar by the `*.receipt` intake.
fn commit_temp_path(dest: &Path) -> Result<std::path::PathBuf, String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("{} has no parent dir", dest.display()))?;
    let base = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} has no basename", dest.display()))?;
    Ok(parent.join(format!(".commit-tmp.{}.{base}.staging", std::process::id())))
}

/// Whether PID is a live process (a `/proc/<pid>` entry exists) — so a sweep reaps only
/// crash-orphaned staging temps, never a concurrent committer's live one. Only a NotFound
/// `/proc/<pid>` is treated as dead; an ambiguous error is treated as ALIVE, so uncertainty
/// never reaps a temp we cannot prove is an orphan (a leak is safe; a wrong reap is not).
fn pid_is_alive(pid: u32) -> bool {
    match std::fs::symlink_metadata(format!("/proc/{pid}")) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Sweep crash-orphaned commit temporaries (`.commit-tmp.<pid>.<base>`) under DIR whose
/// owning pid is dead — a hard kill between the canonical copy and the atomic rename leaves
/// one behind. A live pid's staging tree is left untouched. Best-effort: an error defers the
/// reclaim to a later pass.
fn sweep_commit_temps(dir: &Path) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let pid = name
            .strip_prefix(".commit-tmp.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|p| p.parse::<u32>().ok());
        if let Some(pid) = pid {
            if !pid_is_alive(pid) {
                let _ = remove_store_path(&entry.path());
            }
        }
    }
}

/// Copy SRC canonically to DEST atomically: stage into a sibling temp, then rename into
/// place. A kill before the rename leaves only the (swept) temp — DEST is never a
/// partially-copied tree. DEST must be absent; the caller handles an existing DEST.
fn commit_canonical_atomic(src: &Path, dest: &Path) -> Result<(), String> {
    let tmp = commit_temp_path(dest)?;
    remove_store_path(&tmp)?; // clear this pid's own stale temp from an earlier crash
    if let Err(e) = copy_canonical(src, &tmp) {
        let _ = remove_store_path(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = remove_store_path(&tmp);
        format!("commit rename {} -> {}: {e}", tmp.display(), dest.display())
    })
}

/// Write BYTES to PATH atomically: to a sibling temp, then rename over PATH. A kill
/// mid-write leaves only the (swept) temp; PATH is never a truncated file — the torn-db /
/// torn-receipt failure mode.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = commit_temp_path(path)?;
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = remove_store_path(&tmp);
        format!("commit rename {} -> {}: {e}", tmp.display(), path.display())
    })
}

/// The store paths already committed (registered with a hash) in the persistent cache DB —
/// used to tell a torn orphan (unregistered) from a real ABI-drift conflict (registered). A
/// missing DB is the first commit (empty set); an unreadable DB is a torn write from an
/// interrupted commit, surfaced with a recovery hint rather than read as empty (which would
/// misjudge every registered path a torn orphan and clobber the whole cache).
fn read_registered_paths(db: &Path) -> Result<std::collections::HashSet<String>, String> {
    match std::fs::read(db) {
        Ok(bytes) => {
            let hashes = store_db_read::Db::open(bytes)
                .and_then(|d| d.hashes_by_path())
                .map_err(|e| {
                    format!(
                        "read cache db {}: {e} — the persistent build-cache db is unreadable \
                         (a torn write from an interrupted commit); remove {} to rebuild it",
                        db.display(),
                        db.display()
                    )
                })?;
            Ok(hashes.into_keys().collect())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `read` follows symlinks, so NotFound means the db is genuinely absent (an empty
            // first-commit set) ONLY if nothing is at the path at all. A dangling/unreadable db
            // symlink also reads NotFound; treat that as a torn db and fail closed rather than
            // misjudge every registered path a torn orphan.
            match std::fs::symlink_metadata(db) {
                Err(m) if m.kind() == std::io::ErrorKind::NotFound => {
                    Ok(std::collections::HashSet::new())
                }
                Ok(_) => Err(format!(
                    "read cache db {}: the path exists but its contents are unreadable \
                     (a dangling or torn db from an interrupted commit); remove {} to rebuild it",
                    db.display(),
                    db.display()
                )),
                Err(m) => Err(format!("stat cache db {}: {m}", db.display())),
            }
        }
        Err(e) => Err(format!("read cache db {}: {e}", db.display())),
    }
}

/// Commit a freshly-built output tree to a store DESTINATION that may already exist, safely
/// under ABI-token addressing. Pre-ABI an already-present store path was guaranteed to hold
/// the same bytes (the path WAS the content hash), so a commit was a pure idempotent skip.
/// Output paths are now keyed on the ABI token (store::builder_identity_path), NOT their
/// content, so an already-present path is NOT guaranteed identical: a builder change that
/// alters OUTPUTS without a matching BUILDER_ABI bump would land different bytes at the SAME
/// path. So when `dest` exists, re-hash it and FAIL CLOSED on a mismatch — rather than skip
/// the copy and let the caller's `merge_output_db`/receipt-write describe bytes that are not
/// there (a stale tree with a fresh DB record). `expected` is the NAR hash the current build
/// recorded for this output (`sha256:<hex>`). Absent dest → a plain copy; matching dest → an
/// idempotent skip; mismatching dest → recovered if a torn orphan, else the ABI-bump demand.
///
/// The commit is ATOMIC (stage into a sibling temp, rename into place), so a kill can never
/// leave a partial tree at DEST. An already-present DEST that hashes `expected` is the
/// idempotent skip. A mismatching DEST is disambiguated by `registered` — whether the
/// persistent DB already vouches this path: a REGISTERED mismatch is real ABI-drift (two
/// builds produced different bytes for one ABI-keyed path) and fails closed; an
/// UNREGISTERED mismatch is a torn tree an interrupted commit left behind (its DB
/// registration never ran) and is recovered — removed and re-committed — rather than
/// wedging the shared cache forever.
fn commit_tree_checked(
    src: &Path,
    dest: &Path,
    expected: &str,
    registered: bool,
) -> Result<(), String> {
    // Only a NotFound dest is genuinely absent; any other metadata error must surface, never be
    // read as "absent" — that would rename over a registered path we simply could not stat.
    match dest.symlink_metadata() {
        Ok(_) => match nar_hash_path(dest) {
            Ok(have) if have == expected => return Ok(()),
            outcome if registered => {
                let detail = match outcome {
                    Ok(have) => format!("holds a tree hashing {have}"),
                    Err(e) => format!("is unreadable ({e})"),
                };
                return Err(format!(
                    "store commit: {} {detail}, but this build produced {expected} — an output \
                     path is keyed on the ABI token, so identical paths MUST be identical bytes. \
                     A registered output changed without a BUILDER_ABI bump: bump store::BUILDER_ABI \
                     (or set TD_BUILDER_ABI) so the changed output takes a fresh path. Refusing to \
                     overwrite the store record over stale bytes.",
                    dest.display()
                ));
            }
            _ => remove_store_path(dest)?,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat {}: {e}", dest.display())),
    }
    commit_canonical_atomic(src, dest)
}

/// One built output's post-build registration facts — the daemon's per-path
/// record (the `build`/`realize` NAR scan computes these).
#[derive(Clone)]
struct OutputReg {
    store_path: String,
    nar_hash: String,
    nar_size: u64,
    refs: Vec<String>,
    deriver: String,
}

/// Write a td store-db (the daemon's `ValidPaths`/`Refs` authority, via the
/// zero-dep `store_db` writer) registering the just-built OUTPUTS — td OWNS the
/// store record of its own build, not just a text file. Each output is fully
/// registered (path/hash/registrationTime/deriver/narSize) with ids 1..N; its
/// references resolve to another output's id or to a scaffolding `ValidPaths` row
/// (path only) — the same shape `store-add-referenced` writes. registrationTime is
/// a fixed sentinel (excluded from the daemon differential, as in `store-register`).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn write_output_db(regs: &[OutputReg], out_db: &Path) -> Result<(), String> {
    use std::collections::BTreeMap;
    use store_db::{Table, Value};
    let out_id: BTreeMap<&str, i64> = regs
        .iter()
        .enumerate()
        .map(|(i, r)| (r.store_path.as_str(), i as i64 + 1))
        .collect();
    // External references (not themselves outputs) get ids after the outputs, in
    // first-seen order — stable, so the db is deterministic.
    let mut ext_order: Vec<String> = Vec::new();
    let mut ext_id: BTreeMap<String, i64> = BTreeMap::new();
    let mut next = regs.len() as i64 + 1;
    for r in regs {
        for rf in &r.refs {
            if !out_id.contains_key(rf.as_str()) && !ext_id.contains_key(rf) {
                ext_id.insert(rf.clone(), next);
                ext_order.push(rf.clone());
                next += 1;
            }
        }
    }
    let id_of = |p: &str| -> i64 {
        *out_id
            .get(p)
            .or_else(|| ext_id.get(p))
            .expect("reference id assigned above")
    };
    let mut valid: Vec<(i64, Vec<Value>)> = Vec::new();
    for (i, r) in regs.iter().enumerate() {
        valid.push((
            i as i64 + 1,
            vec![
                Value::Null, // id (integer primary key) — rowid is the id
                Value::Text(r.store_path.clone()),
                Value::Text(r.nar_hash.clone()),
                Value::Int(1), // registrationTime (sentinel; excluded from diffs)
                Value::Text(r.deriver.clone()),
                Value::Int(r.nar_size as i64),
            ],
        ));
    }
    for p in &ext_order {
        valid.push((
            ext_id[p],
            vec![
                Value::Null,
                Value::Text(p.clone()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ));
    }
    let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut rid = 1i64;
    for (i, r) in regs.iter().enumerate() {
        for rf in &r.refs {
            ref_rows.push((rid, vec![Value::Int(i as i64 + 1), Value::Int(id_of(rf))]));
            rid += 1;
        }
    }
    let tables = [
        Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: valid,
        },
        Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: ref_rows,
        },
    ];
    std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
    Ok(())
}

/// MERGE built outputs into a PERSISTENT store DB — the accumulating dual of
/// `write_output_db`'s clobber. Given the EXISTING db bytes (None for the first
/// commit) and the NEW outputs, union their `ValidPaths` + `Refs` into one db and
/// return the serialized bytes. This is what makes a td store *persistent*: a build
/// adds its result to a store that already holds prior builds' results, instead of
/// every build writing a fresh single-output db. The daemon's accumulating
/// `ValidPaths`/`Refs` authority across SEPARATE builds, in pure Rust.
///
/// The store PATH is the identity:
///   - re-committing the same output is IDEMPOTENT (one row; the bytes are
///     byte-deterministic, so a re-merge of the same set reproduces them exactly);
///   - a path first seen only as another output's *reference* is a SCAFFOLD row
///     (path, no hash) and is UPGRADED in place to a full row when a later commit
///     registers it for real;
///   - rowids are assigned in sorted-path order, so the db is deterministic
///     regardless of commit order.
/// Mirrors `store-gc-sweep`'s renumber-and-remap-Refs rewrite (its additive dual):
/// reads with the td reader, writes with the td writer, no daemon, no sqlite engine.
/// Scope is the GC/closure authority — `ValidPaths` + `Refs`; a persistent commit DB
/// does not carry `DerivationOutputs` (as `store-gc-sweep`'s swept DB does not: the
/// drv→output mapping is rebuilt by registration, not by accumulation).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn merge_regs(existing: Option<&[u8]>, new_regs: &[OutputReg]) -> Result<Vec<u8>, String> {
    use std::collections::{BTreeMap, BTreeSet};
    use store_db::{Table, Value as WV};
    use store_db_read::{Db, Value as RV};
    // One accumulated record per path: full fields when known, else scaffold (None).
    struct Rec {
        hash: Option<String>,
        deriver: Option<String>,
        size: Option<u64>,
        refs: BTreeSet<String>,
    }
    let mut recs: BTreeMap<String, Rec> = BTreeMap::new();
    fn ensure<'a>(recs: &'a mut BTreeMap<String, Rec>, p: &str) -> &'a mut Rec {
        recs.entry(p.to_string()).or_insert_with(|| Rec {
            hash: None,
            deriver: None,
            size: None,
            refs: BTreeSet::new(),
        })
    }
    // 1) Fold in the existing db (if this is not the first commit).
    if let Some(bytes) = existing {
        let db = Db::open(bytes.to_vec())?;
        let mut path_of: BTreeMap<i64, String> = BTreeMap::new();
        for (rowid, cols) in db.table("ValidPaths")? {
            let path = match cols.get(1) {
                Some(RV::Text(p)) => p.clone(),
                _ => continue,
            };
            path_of.insert(rowid, path.clone());
            let r = ensure(&mut recs, &path);
            if let Some(RV::Text(h)) = cols.get(2) {
                if !h.is_empty() {
                    r.hash = Some(h.clone());
                }
            }
            if let Some(RV::Text(d)) = cols.get(4) {
                if !d.is_empty() {
                    r.deriver = Some(d.clone());
                }
            }
            if let Some(RV::Int(s)) = cols.get(5) {
                r.size = Some(*s as u64);
            }
        }
        for (_rid, cols) in db.table("Refs")? {
            if let (Some(RV::Int(a)), Some(RV::Int(b))) = (cols.first(), cols.get(1)) {
                if let (Some(ap), Some(bp)) = (path_of.get(a), path_of.get(b)) {
                    let (ap, bp) = (ap.clone(), bp.clone());
                    ensure(&mut recs, &bp); // a referenced path is at least a scaffold
                    ensure(&mut recs, &ap).refs.insert(bp);
                }
            }
        }
    }
    // 2) Union the new outputs — each a full row; its refs are at least scaffolds.
    for reg in new_regs {
        {
            let r = ensure(&mut recs, &reg.store_path);
            r.hash = Some(reg.nar_hash.clone());
            r.deriver = Some(reg.deriver.clone());
            r.size = Some(reg.nar_size);
        }
        for rf in &reg.refs {
            ensure(&mut recs, rf);
            ensure(&mut recs, &reg.store_path).refs.insert(rf.clone());
        }
    }
    // 3) Assign rowids in sorted-path order (BTreeMap iterates sorted → deterministic).
    // `id_of` resolves a reference's TARGET path to its rowid; a row's OWN id is just
    // its (sorted) position, so the loop uses the enumerate index for that directly.
    let id_of: BTreeMap<&str, i64> = recs
        .keys()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i as i64 + 1))
        .collect();
    let mut valid: Vec<(i64, Vec<WV>)> = Vec::with_capacity(recs.len());
    let mut ref_rows: Vec<(i64, Vec<WV>)> = Vec::new();
    let mut rid = 1i64;
    for (i, (p, r)) in recs.iter().enumerate() {
        let myid = i as i64 + 1;
        // registrationTime is the same fixed sentinel write_output_db uses (excluded
        // from the daemon differential); a scaffold (no hash) keeps it NULL too.
        let (regtime, deriver, size) = match &r.hash {
            Some(_) => (
                WV::Int(1),
                r.deriver.clone().map(WV::Text).unwrap_or(WV::Null),
                r.size.map(|s| WV::Int(s as i64)).unwrap_or(WV::Null),
            ),
            None => (WV::Null, WV::Null, WV::Null),
        };
        valid.push((
            myid,
            vec![
                WV::Null, // id (integer primary key) — rowid is the id
                WV::Text(p.clone()),
                r.hash.clone().map(WV::Text).unwrap_or(WV::Null),
                regtime,
                deriver,
                size,
            ],
        ));
        for rf in &r.refs {
            ref_rows.push((rid, vec![WV::Int(myid), WV::Int(id_of[rf.as_str()])]));
            rid += 1;
        }
    }
    let tables = [
        Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: valid,
        },
        Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: ref_rows,
        },
    ];
    Ok(store_db::write_db(&tables))
}

/// Read-modify-write `merge_regs` against an on-disk persistent DB: load DEST-DB if
/// it exists (a missing file = the first commit), union the NEW outputs in, write it
/// back. The store dir's bytes are interned by the caller (`store-commit`).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn merge_output_db(dest_db: &Path, new_regs: &[OutputReg]) -> Result<(), String> {
    let existing = match std::fs::read(dest_db) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {}: {e}", dest_db.display())),
    };
    let bytes = merge_regs(existing.as_deref(), new_regs)?;
    write_atomic(dest_db, &bytes)
}

/// The `store-add-recursive` body, factored so the merge semantics unit-test:
/// intern SRC as a content-addressed `source` item under STORE-DIR and MERGE
/// its registration into OUT-DB (a missing file is the first intern — see the
/// CLI arm's doc for why merging is load-bearing, re #469). Prints nothing;
/// returns the computed store path.
fn store_add_recursive(
    name: &str,
    src: &str,
    store_dir: &str,
    out_db: &str,
) -> Result<String, String> {
    // Content-addressed path from the source tree's recursive NAR sha256.
    let nar = nar_hash(src).map_err(|e| e.to_string())?;
    let hex = nar
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("nar-hash returned `{nar}', expected sha256:<hex>"))?;
    let path = store::make_store_path("source", hex, name);
    let base = path
        .rsplit('/')
        .next()
        .filter(|_| store::name_from_store_path(&path).is_some())
        .ok_or_else(|| format!("computed path {path} is malformed"))?
        .to_string();
    // Canonically restore the tree into the td-owned store. The path is
    // content-addressed, so an EXISTING tree there must already BE this
    // content: verify it instead of copying over it (idempotent re-intern —
    // the runner re-interns every seed on warm runs); a hash mismatch is a
    // corrupt store item, never a reuse.
    std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
    let disk = Path::new(store_dir).join(&base);
    if disk.symlink_metadata().is_ok() {
        let got = nar_hash_path(&disk).map_err(|e| e.to_string())?;
        if got != nar {
            return Err(format!(
                "store item {} exists but hashes {got}, expected {nar} — corrupt content-addressed item; refusing to re-register it (re #469)",
                disk.display()
            ));
        }
    } else {
        copy_canonical(Path::new(src), &disk)?;
    }
    // Register: NAR hash + size of the tree td restored (the `build`
    // machinery), references scanned among the single-path closure.
    let closure = vec![path.clone()];
    let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
    nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
    let (hash, size, refs) = s.finish();
    if !refs.is_empty() && refs != [path.clone()] {
        return Err(format!(
            "source {name} has references {refs:?}; referenced sources are a later increment"
        ));
    }
    let reg = OutputReg {
        store_path: path.clone(),
        nar_hash: hash,
        nar_size: size,
        refs,
        deriver: String::new(), // a source add has none
    };
    merge_output_db(Path::new(out_db), std::slice::from_ref(&reg))?;
    Ok(path)
}

/// Execute DRV in a userns sandbox against CLOSURE (the staged input store paths,
/// one per line) and write a registration record — `path` / `nar-hash` /
/// `nar-size` / `reference`* / `deriver` per output — to SCRATCH/registration,
/// printing `OUT=<name> <path>` per output. The reference candidates are the
/// closure plus the drv's own outputs (self-references), the daemon's candidate
/// shape. Returns the per-output registration facts (for `realize` to write a td
/// store-db). Shared by `build` (CLOSURE handed in as a file) and `realize`
/// (CLOSURE computed by td itself from the store DB's Refs graph).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_and_register(
    drv_path: &str,
    closure: &[String],
    scratch: &Path,
    manifest: &sandbox::StageManifest,
) -> Result<Vec<OutputReg>, String> {
    let bytes = std::fs::read(drv_path).map_err(|e| e.to_string())?;
    let parsed = drv::parse(&bytes).map_err(|e| e.to_string())?;
    // The deriver recorded is the .drv's OWN store path. For a store-path input
    // that is drv_path; for an emitted .drv handed in from outside the store,
    // compute its content-addressed store path so the registration matches the
    // daemon's recorded deriver.
    let deriver = if drv_path.starts_with(store::store_dir().as_str()) {
        drv_path.to_string()
    } else {
        let out0 = parsed
            .outputs
            .first()
            .ok_or_else(|| "derivation has no outputs".to_string())?;
        let drv_name = format!(
            "{}.drv",
            store::name_from_store_path(&out0.path)
                .ok_or_else(|| "output is not a store path".to_string())?
        );
        let mut refs: Vec<String> = parsed.input_drvs.iter().map(|(p, _)| p.clone()).collect();
        refs.extend(parsed.input_srcs.iter().cloned());
        store::drv_store_path(&drv_name, &bytes, &refs)
    };
    let outputs =
        sandbox::build(&parsed, drv_path, closure, scratch, manifest).map_err(|e| e.to_string())?;
    // Reference candidates: the staged closure plus the drv's own outputs
    // (self-references), the daemon's candidate shape. A closure entry may carry an
    // on-disk override (`CANONICAL\tON-DISK`); reference scanning matches the
    // CANONICAL store paths, so take the canonical half.
    let mut candidates: Vec<String> = closure
        .iter()
        .map(|e| sandbox::split_closure_entry(e).0.to_string())
        .collect();
    candidates.extend(parsed.outputs.iter().map(|o| o.path.clone()));
    let mut regs: Vec<OutputReg> = Vec::new();
    for (name, host) in &outputs {
        let store_path = &parsed
            .outputs
            .iter()
            .find(|o| &o.name == name)
            .expect("output came from this drv")
            .path;
        let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, host).map_err(|e| e.to_string())?;
        let (hash, size, refs) = scanner.finish();
        println!("OUT={name} {store_path}");
        regs.push(OutputReg {
            store_path: store_path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs,
            deriver: deriver.clone(),
        });
    }
    std::fs::write(scratch.join("registration"), registration_text(&regs))
        .map_err(|e| e.to_string())?;
    Ok(regs)
}

/// Serialize per-output registration records into a SCRATCH/registration blob — the
/// inverse of `parse_registration_blocks` (`parse(registration_text(regs)) == regs`).
/// One `path`/`nar-hash`/`nar-size`/`reference`*/`deriver` block per output, blank-line
/// separated. Written by `build_and_register` after a real build and by a
/// persistent-store read-back (so a fresh scratch that reused a prior build's output
/// still carries the same registration a real build would have).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn registration_text(regs: &[OutputReg]) -> String {
    let mut record = String::new();
    for r in regs {
        record.push_str(&format!("path {}\n", r.store_path));
        record.push_str(&format!("nar-hash {}\n", r.nar_hash));
        record.push_str(&format!("nar-size {}\n", r.nar_size));
        for rf in &r.refs {
            record.push_str(&format!("reference {rf}\n"));
        }
        record.push_str(&format!("deriver {}\n\n", r.deriver));
    }
    record
}

/// Intern a finished build SCRATCH (its `registration` + `newstore/<base>` trees) into a
/// PERSISTENT store and MERGE its registration into the accumulating DB — the build-into
/// half of an incremental store. Idempotent (a content path already present is a no-op).
/// Shared by the `store-commit` subcommand and build-recipe's persistent-store build-into.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn commit_scratch_to_store(scratch: &Path, store_dir: &str, db: &Path) -> Result<Vec<String>, String> {
    let reg = std::fs::read_to_string(scratch.join("registration")).map_err(|e| {
        format!("read {}/registration: {e} (build into this scratch first)", scratch.display())
    })?;
    let regs = parse_registration_blocks(&reg);
    if regs.is_empty() {
        return Err("no outputs in the registration to commit".to_string());
    }
    // Hold the per-store commit lock across create-dir -> sweep -> snapshot -> tree commits ->
    // db merge -> receipt, so no second writer — nor GC renaming the whole cache aside — can
    // race a path this transaction is publishing or recovering. Taken BEFORE the cache dir is
    // (re)created so it serializes with eviction, which takes the same stable lock.
    let _commit_lock = lock_store_commit(db)?;
    std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
    // Recover from any interrupted commit before writing: sweep crash-orphaned staging
    // temps from the store, the db dir, and the receipts dir.
    sweep_commit_temps(Path::new(store_dir));
    if let Some(dbdir) = db.parent() {
        sweep_commit_temps(dbdir);
    }
    let mut receipts_dir = db.as_os_str().to_owned();
    receipts_dir.push(".receipts");
    sweep_commit_temps(Path::new(&receipts_dir));
    // Which paths the persistent db already vouches — tells a torn orphan (unregistered)
    // from a real ABI-drift conflict (registered) in commit_tree_checked.
    let registered = read_registered_paths(db)?;
    let newstore = scratch.join("newstore");
    let mut committed = Vec::with_capacity(regs.len());
    for r in &regs {
        let base = r
            .store_path
            .rsplit('/')
            .next()
            .filter(|_| store::name_from_store_path(&r.store_path).is_some())
            .ok_or_else(|| format!("output {} is not a store path", r.store_path))?;
        let src = newstore.join(base);
        if !src.exists() {
            return Err(format!("output tree missing under {}", src.display()));
        }
        let dest = Path::new(store_dir).join(base);
        // Commit atomically and fail closed only on a REGISTERED mismatch (a torn orphan is
        // recovered) — see commit_tree_checked.
        commit_tree_checked(&src, &dest, &r.nar_hash, registered.contains(&r.store_path))?;
        committed.push(r.store_path.clone());
    }
    merge_output_db(db, &regs)?;
    // Persist the engine-issued receipt beside the db, keyed by the producing drv
    // (re #469 round-7): a later persistent_realization reuse of these outputs
    // requires it to match the then-current plan identity. A scratch without a
    // receipt (a plain store-commit of interned trees) commits bytes + rows only —
    // such entries are simply never receipt-reusable.
    if let Ok(receipt) = std::fs::read_to_string(scratch.join("receipt")) {
        if let Some(rp) = regs
            .first()
            .filter(|r| !r.deriver.is_empty())
            .and_then(|r| persist_receipt_path(db, &r.deriver))
        {
            if let Some(dir) = rp.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            write_atomic(&rp, receipt.as_bytes())?;
        }
    }
    Ok(committed)
}

/// The receipt SIDECAR of a persistent store db: `<db>.receipts/<drv-basename>.receipt`,
/// written by `commit_scratch_to_store` from the receipt the engine issued at build
/// time, keyed by the producing drv's store basename (content-addressed, so one
/// receipt per distinct derivation). The `.receipt` suffix is LOAD-BEARING:
/// `authenticate_recipe_output_db` reads only `*.receipt` files, so a bare
/// `<drv-basename>` name (the round-9 P1) made every engine-produced db fail
/// `--recipe-output-db` intake — written and read paths must stay this one fn.
/// A warm `/td/store` cache whose receipts predate this suffix simply MISSes
/// once (the bare-named sidecar is no longer found) and rebuilds — safe and
/// self-healing under content addressing, not a regression to chase.
fn persist_receipt_path(persist_db: &Path, deriver: &str) -> Option<std::path::PathBuf> {
    let base = deriver.rsplit('/').next().filter(|b| !b.is_empty())?;
    let mut dir = persist_db.as_os_str().to_owned();
    dir.push(".receipts");
    Some(Path::new(&dir).join(format!("{base}.receipt")))
}

/// Persistent-store build cache — like `cached_realization`, but keyed on a PERSISTENT
/// store (dir + accumulating DB) that survives ACROSS invocations (the incremental
/// /td/store), and RECEIPT-GATED the same way (re #469 round-7): the reuse requires
/// the engine-issued receipt sidecar for THIS derivation to match the CURRENT plan's
/// identity (`ReceiptExpect`), every ValidPaths row to record THIS drv as its deriver
/// (EXPECTED_DERIVER — a row minted for some other derivation, or with no deriver,
/// vouches nothing here), the row hash to equal the receipt's, and the tree under
/// PERSIST_STORE to re-serialize to it. Then each output tree is staged into
/// SCRATCH/newstore and the read-back regs returned (the caller writes
/// SCRATCH/registration + td.db from them) — so the build is SKIPPED. Any
/// missing/mismatched leg ⇒ None (rebuild), and any tree staged so far is unwound.
/// The daemon's valid-path skip, sourced across process boundaries from an on-disk store.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn persistent_realization(
    parsed: &drv::Derivation,
    persist_store: &str,
    persist_db: &Path,
    scratch: &Path,
    expect: &ReceiptExpect,
    expected_deriver: &str,
) -> Result<Option<Vec<OutputReg>>, String> {
    use store_db_read::{Db, Value as RV};
    let bytes = match std::fs::read(persist_db) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", persist_db.display())),
    };
    // The receipt gate: the sidecar for THIS derivation must exist and match the
    // CURRENT plan's identity, or the persistent entry is a miss (rebuild).
    let receipt_hashes = match persist_receipt_path(persist_db, expected_deriver)
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        Some(text) => match receipt_outputs(&text, expect) {
            Some(h) => h,
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    let db = Db::open(bytes)?;
    // Fully-registered paths (hash present) → (hash, narSize, deriver).
    let mut full: std::collections::HashMap<String, (String, u64, String)> =
        std::collections::HashMap::new();
    for (_rid, cols) in db.table("ValidPaths")? {
        if let (Some(RV::Text(p)), Some(RV::Text(h))) = (cols.get(1), cols.get(2)) {
            if h.is_empty() {
                continue;
            }
            let size = match cols.get(5) {
                Some(RV::Int(s)) => *s as u64,
                _ => 0,
            };
            let deriver = match cols.get(4) {
                Some(RV::Text(d)) => d.clone(),
                _ => String::new(),
            };
            full.insert(p.clone(), (h.clone(), size, deriver));
        }
    }
    let refs_map = db.refs_by_path()?;
    let newstore = scratch.join("newstore");
    std::fs::create_dir_all(&newstore).map_err(|e| e.to_string())?;
    let mut out: Vec<OutputReg> = Vec::with_capacity(parsed.outputs.len());
    let mut staged: Vec<std::path::PathBuf> = Vec::new();
    // A partial hit (some outputs found, then a miss) must not leave half the outputs
    // staged in newstore (the rebuild would build ON them) — unwind before returning None.
    fn unwind(staged: &[std::path::PathBuf]) {
        for d in staged {
            let _ = std::fs::remove_dir_all(d);
        }
    }
    for o in &parsed.outputs {
        let (hash, size, deriver) = match full.get(&o.path) {
            Some(x) => x.clone(),
            None => {
                unwind(&staged);
                return Ok(None);
            }
        };
        // Deriver binding (re #469 round-7): the row must have been registered FOR
        // this derivation — a row carrying another drv's deriver (or none) cannot
        // vouch this drv's output, however plausible its path looks. And the row's
        // hash must be the one the engine's receipt recorded at build time.
        if deriver != expected_deriver || receipt_hashes.get(&o.path) != Some(&hash) {
            unwind(&staged);
            return Ok(None);
        }
        let base = match o.path.rsplit('/').next() {
            Some(b) => b,
            None => {
                unwind(&staged);
                return Ok(None);
            }
        };
        let src = Path::new(persist_store).join(base);
        if !src.exists() {
            unwind(&staged);
            return Ok(None);
        }
        let refs: Vec<String> = refs_map.get(&o.path).cloned().unwrap_or_default();
        // Integrity: the persistent tree must re-serialize to the recorded hash — a
        // corrupt/partial persistent entry is a MISS (rebuild), never trusted.
        let mut scanner = scan::Scanner::new(&refs).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, &src).map_err(|e| e.to_string())?;
        let (got, _, _) = scanner.finish();
        if got != hash {
            unwind(&staged);
            return Ok(None);
        }
        let dest = newstore.join(base);
        if dest.exists() {
            let _ = std::fs::remove_dir_all(&dest);
        }
        copy_canonical(&src, &dest)?;
        staged.push(dest);
        out.push(OutputReg {
            store_path: o.path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs,
            deriver,
        });
    }
    Ok(Some(out))
}

/// Parse a SCRATCH/registration blob into per-output records. The blob is the
/// `path`/`nar-hash`/`nar-size`/`reference`*/`deriver` blocks `build_and_register`
/// writes — one block per output, a `path ` line opening each. Order is preserved.
/// Shared by `cached_realization` (the build cache) and `store-commit` (interning a
/// finished build into the persistent store), so both read the registration the same way.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn parse_registration_blocks(text: &str) -> Vec<OutputReg> {
    let mut recs: Vec<OutputReg> = Vec::new();
    let mut cur: Option<OutputReg> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("path ") {
            if let Some(r) = cur.take() {
                recs.push(r);
            }
            cur = Some(OutputReg {
                store_path: p.to_string(),
                nar_hash: String::new(),
                nar_size: 0,
                refs: Vec::new(),
                deriver: String::new(),
            });
        } else if let (Some(r), Some(h)) = (cur.as_mut(), line.strip_prefix("nar-hash ")) {
            r.nar_hash = h.to_string();
        } else if let (Some(r), Some(s)) = (cur.as_mut(), line.strip_prefix("nar-size ")) {
            r.nar_size = s.parse().unwrap_or(0);
        } else if let (Some(r), Some(rf)) = (cur.as_mut(), line.strip_prefix("reference ")) {
            r.refs.push(rf.to_string());
        } else if let (Some(r), Some(d)) = (cur.as_mut(), line.strip_prefix("deriver ")) {
            r.deriver = d.to_string();
        }
    }
    if let Some(r) = cur {
        recs.push(r);
    }
    recs
}

/// The reuse identity the CURRENT plan derives, independent of anything a cache
/// stores (re #469): the SHA-256 of the assembled `.drv` FILE BYTES (they cover
/// inputs, builder path, args, env), the CLOSURE-SCOPED manifest digest
/// (`reuse_key_manifest_digest` — the hash AND origin class of every input in
/// THIS drv's own transitive input closure, EXCEPT the builder's own driver row),
/// and the drv's builder path. A cache entry is reusable only if the receipt the ENGINE
/// wrote at build time matches ALL of these recomputed-now values — so metadata
/// stored beside an output is never its own authority: a forged record must
/// reproduce the exact identity the planner derives today, and a stale one
/// (changed drv, changed input set, changed builder ABI) can never hit. The digest
/// is scoped to the drv's OWN closure, NOT the plan-wide manifest union
/// enforcement uses, so the SAME drv keys the same under any build target (a
/// higher target's unrelated seeds do not drift the key); the builder is covered
/// by folding the drv's DECLARED ABI builder IDENTITY into that digest —
/// content-independent, so like the OUTPUT path the key moves only on a
/// BUILDER_ABI bump, never on an output-neutral builder recompile. The builder's
/// own `ControlPlaneBuilder` row is EXCLUDED from the reuse digest (its hash tracks
/// the builder ELF, which the ABI identity deliberately decouples from output
/// identity); ENFORCEMENT keeps that row and still binds the real builder bytes for
/// #469 provenance — only the reuse KEY trusts the ABI identity, matching how the
/// OUTPUT path already keys on it, not on the builder's content.
/// Honest limit, stated: receipts live in the same
/// user-writable cache as the outputs, so an attacker who can rewrite both and
/// can read the current plan can still forge a hit — closing THAT requires a
/// daemon-owned provenance database (or no reuse at all), which is follow-on
/// work, not this increment.
#[derive(Clone)]
struct ReceiptExpect {
    drv_sha256: String,
    manifest_sha256: String,
    builder: String,
}

/// Digest of the typed staging manifest: SHA-256 over its `path hash origin`
/// lines (BTreeMap iteration is sorted, so the digest is canonical). Binds a
/// receipt to the exact input-authority set the plan derives.
fn manifest_digest(m: &sandbox::StageManifest) -> String {
    let mut h = sha256::Sha256::new();
    for (p, si) in m {
        h.update(p.as_bytes());
        h.update(b" ");
        h.update(si.nar_hash.as_bytes());
        h.update(b" ");
        h.update(si.origin.as_str().as_bytes());
        h.update(b"\n");
    }
    sha256::to_base16(&h.finalize())
}

/// The DRV's declared input ROOTS — its input-srcs plus each input derivation's
/// requested output paths, in file order (input-srcs first). The seed of the
/// transitive-closure scan `stage_input_closure` computes; factored out so the
/// input-drv resolution (read the input `.drv`, look up the named output) is a
/// single, unit-testable routine. The builder-identity token, if present as an
/// input-src, rides through verbatim; `stage_input_closure` substitutes the real
/// builder for it.
fn drv_declared_inputs(parsed: &drv::Derivation) -> Result<Vec<String>, String> {
    let mut roots: Vec<String> = parsed.input_srcs.clone();
    for (idrv, outnames) in &parsed.input_drvs {
        // The input `.drv` path is a LOCAL store file: td's daemon is local per-worktree
        // and each input drv was written (assemble) before this drv could reference it, so
        // it is on disk at every reuse-key read site as well as at realize. (td-assembled
        // RECIPE drvs carry no input-drvs — input-srcs only — so this loop is a no-op there;
        // it exists for a general drv the daemon may realize.) A missing file reds here.
        let ib = std::fs::read(idrv).map_err(|e| format!("read input drv {idrv}: {e}"))?;
        let ip = drv::parse(&ib).map_err(|e| format!("parse input drv {idrv}: {e}"))?;
        for on in outnames {
            let o = ip
                .outputs
                .iter()
                .find(|o| &o.name == on)
                .ok_or_else(|| format!("input drv {idrv} has no output `{on}'"))?;
            roots.push(o.path.clone());
        }
    }
    Ok(roots)
}

/// The reuse-key digest (`ReceiptExpect.manifest_sha256`), SCOPED to the
/// derivation's OWN input CLOSURE instead of the plan-wide manifest union
/// (re #469). `manifest` is the full authority manifest a fresh realize
/// assembles — the union of EVERY typed td-owned db the plan carries (the whole
/// seed db, the bless db, and every prior step's td.db). Digesting that union
/// keyed the reuse identity to the WHOLE PLAN: the seed db is interned per
/// TARGET graph, so building a higher target folds unrelated seeds into a low
/// rung's key, and the SAME drv then gets a DIFFERENT key under a different
/// target — a receipt-identity miss that rebuilds an already-valid output and,
/// because the rebuild need not be bit-identical, collides with the cached tree
/// and fails the plan. A sandboxed build cannot be influenced by inputs outside
/// its own closure, so those inputs must not be in its reuse key.
///
/// So the key digests ONLY the manifest entries whose path is in `closure` — the
/// EXACT transitive input set `realize_drv` stages and enforces over, computed by
/// the shared `stage_input_closure` at both the write site and every read site
/// (identical routine ⇒ identical set ⇒ no asymmetry miss) — with ONE exclusion: the
/// builder's OWN `ControlPlaneBuilder` row (at `real_builder_cb`, the real content path
/// the PRE-rekey `closure` names). That row's hash tracks the builder ELF, so scoping
/// it in would move the key on every builder recompile — the precise bust this fix
/// removes, and the one the ladder ALWAYS hits because it stages the builder as a
/// content-addressed override (`TD_BUILDER_PATH`), which puts the builder in-closure and
/// moves its nar_hash each build even though the ABI-keyed OUTPUT path does not. In its
/// place the drv's DECLARED ABI builder IDENTITY (`parsed.builder`, content-independent)
/// is folded in, so the builder is bound by the SAME ABI identity the OUTPUT path keys
/// on, not by its ELF bytes. An output-neutral builder recompile (same BUILDER_ABI)
/// therefore does NOT move the key for an OVERRIDE-staged builder; a BUILDER_ABI bump moves
/// BOTH the identity and the output path. This covers the CHECK-LADDER, whose `build_recipe`
/// reuse cache is ONLY ever reached with a builder override (`TD_BUILDER_PATH`) — the reuse
/// cache that used to bust on every recompile. SCOPE NOTE: the in-process / SELF_TREE case (a
/// daemon started with no `TD_BUILDER_*` serving a bare drv, or a manual `build-recipe`) is NOT
/// stabilized by this — there the builder is `self_store_path()`, content-scanned and vouched as
/// `BlessedSeedClosure`/`AuditedSeed`, NOT `ControlPlaneBuilder`, so the origin gate does not
/// exclude it and an in-process recompile still moves the key, exactly as before this change.
/// The check-ladder never takes that path; stabilizing self-driven builds is a separate
/// follow-up. A change to — or removal of — any OTHER real input still moves the key; the
/// builder's runtime-linkage inputs (its libc etc.) are NOT the excluded row — they keep their
/// rows and bind normally. The digest is built CLOSURE-DRIVEN (it
/// iterates the closure and looks up each row), so a closure member with NO manifest row is
/// recorded as `absent` and still moves the key — an unvouched member (a builder runtime dep
/// a recompile pulled in) cannot yield a spurious hit at the read sites, which do not run
/// enforcement (Codex P1); `absent` is empty in every valid build. ENFORCEMENT is deliberately
/// left FULL-manifest (`enforce_realize_input_policy` runs over the un-scoped `manifest`, which
/// KEEPS the real builder row): it only LOOKS UP each closure item, so a superset is correct —
/// scoping it would instead drop the vouching rows the build stages. Scoping (and
/// builder-excluding) the KEY narrows what a cache hit must match; it does not weaken what a
/// build may stage or #469 enforces.
fn reuse_key_manifest_digest(
    closure: &[String],
    manifest: &sandbox::StageManifest,
    builder_identity: &str,
    real_builder_cb: Option<&str>,
) -> String {
    // Build the scoped digest CLOSURE-DRIVEN (iterate the closure, look up each row), NOT
    // manifest-driven — so a closure member with NO manifest row cannot silently contribute
    // nothing (Codex P1). Each non-excluded closure path is folded via its manifest row when
    // vouched, or recorded in `absent` when not, so an unvouched closure member (e.g. a new,
    // undeclared builder runtime dep a same-ABI recompile pulled in) MOVES the key and forces a
    // miss → the rebuild runs `enforce_realize_input_policy`, which rejects it. The read sites do
    // NOT run enforcement, so without this they could return a spurious hit. `absent` is EMPTY in
    // every valid build (enforce requires every closure member vouched), so this never causes a
    // spurious miss — it only fires on the anomalous unvouched case.
    let mut scoped = sandbox::StageManifest::new();
    let mut absent: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for e in closure {
        // The manifest is keyed by CANONICAL store path; each closure entry is `canonical` or
        // `canonical\ton-disk`, so scope by the canonical (left) half.
        let p = sandbox::split_closure_entry(e).0;
        let row = manifest.get(p);
        // EXCLUDE the builder's OWN control-plane DRIVER row: `real_builder_cb` (the resolved
        // builder path, BY CONSTRUCTION) AND a row vouched `ControlPlaneBuilder`. Its hash tracks
        // the builder ELF, and folding the ABI identity below binds the builder instead — without
        // this the ladder busts the key on every recompile (it stages the builder as a
        // content-addressed override, so this row is in-closure and its nar_hash moves each build
        // even though the ABI-keyed output path does not). The origin gate keeps the exclusion
        // PRECISE (Agy #1 / Codex P2): `manifest_add_db` keeps the FIRST writer's origin on a
        // same-hash collision and the extra/src dbs merge BEFORE the builder db, so a row at this
        // path with a DATA origin is NOT the driver and stays. The builder is architecturally never
        // a recipe data input (a recipe names the virtual ABI identity, which has no materialized
        // bytes, and realize rejects an output referencing it), so today this is always the driver
        // row; the gate is fail-safe either way — a foreign-origin builder row STAYS → the key
        // moves → a rebuild that re-enforces, never a silent stale-output hit. The builder's
        // runtime-linkage inputs keep their rows and bind normally.
        if real_builder_cb == Some(p)
            && row.is_some_and(|si| si.origin == sandbox::InputOrigin::ControlPlaneBuilder)
        {
            continue;
        }
        match row {
            Some(si) => {
                scoped.insert(p.to_string(), si.clone());
            }
            None => {
                absent.insert(p);
            }
        }
    }
    let mut h = sha256::Sha256::new();
    h.update(manifest_digest(&scoped).as_bytes());
    // Fold the sorted set of closure members with NO manifest row (Codex P1) so an unvouched
    // member still moves the key. Deterministic (BTreeSet) and symmetric across read/write (the
    // closure is derived by the shared `stage_input_closure`). Empty in every valid build.
    h.update(b"\nabsent");
    for p in &absent {
        h.update(b"\n");
        h.update(p.as_bytes());
    }
    // Bind the drv's DECLARED builder IDENTITY (`parsed.builder`) IN PLACE OF the excluded
    // builder row — for a recipe drv the ABI-keyed `store::builder_identity_path()/bin/td-builder`,
    // content-INDEPENDENT, the SAME string `ReceiptExpect.builder` records. The output path already
    // keys on this ABI identity (store::builder_identity_path), so the reuse key must too — else an
    // output-neutral builder recompile (same BUILDER_ABI) would move the key, force a rebuild of an
    // already-valid output, and, on a non-reproducible rung, collide with the cached tree
    // (`commit_tree_checked` "Refusing to overwrite"). This leg binds the builder identity for the
    // OVERRIDE-staged builder (row excluded above) — the check-ladder case. (In-process/SELF_TREE
    // builds keep their non-ControlPlaneBuilder self row and are NOT stabilized; see the fn doc.)
    // A BUILDER_ABI bump changes this identity and correctly moves the key (re #469).
    h.update(b"\nbuilder ");
    h.update(builder_identity.as_bytes());
    sha256::to_base16(&h.finalize())
}

/// SHA-256 (hex) of a byte blob — the drv-file leg of `ReceiptExpect`.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = sha256::Sha256::new();
    h.update(bytes);
    sha256::to_base16(&h.finalize())
}

/// Serialize the engine-issued build receipt `realize_drv` writes beside the
/// registration after a REAL build: the current-plan identity plus every
/// output's (name, store path, NAR hash, NAR size) and `producer local-build`.
fn receipt_text(expect: &ReceiptExpect, regs: &[OutputReg]) -> String {
    let mut t = String::from("td-receipt v1\n");
    t.push_str(&format!("drv-sha256 {}\n", expect.drv_sha256));
    t.push_str(&format!("manifest-sha256 {}\n", expect.manifest_sha256));
    t.push_str(&format!("builder {}\n", expect.builder));
    t.push_str("producer local-build\n");
    for r in regs {
        t.push_str(&format!("output {} {} {}\n", r.store_path, r.nar_hash, r.nar_size));
    }
    t
}

/// Parse + verify a receipt against the CURRENT plan's `ReceiptExpect`. Returns
/// the receipt's per-output `store path -> NAR hash` map iff the version,
/// producer, and every identity field match exactly; any anomaly is `None`
/// (the caller treats it as a cache MISS — rebuild, never trust).
fn receipt_outputs(
    text: &str,
    expect: &ReceiptExpect,
) -> Option<std::collections::HashMap<String, String>> {
    let mut lines = text.lines();
    if lines.next() != Some("td-receipt v1") {
        return None;
    }
    let mut drv_sha = None;
    let mut manifest_sha = None;
    let mut builder = None;
    let mut producer = None;
    let mut outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // A duplicated field is a CONTRADICTORY receipt, not a hit — last-wins
    // would let a second `output`/`producer` line quietly override the first.
    for l in lines {
        if let Some(v) = l.strip_prefix("drv-sha256 ") {
            if drv_sha.replace(v).is_some() {
                return None;
            }
        } else if let Some(v) = l.strip_prefix("manifest-sha256 ") {
            if manifest_sha.replace(v).is_some() {
                return None;
            }
        } else if let Some(v) = l.strip_prefix("builder ") {
            if builder.replace(v).is_some() {
                return None;
            }
        } else if let Some(v) = l.strip_prefix("producer ") {
            if producer.replace(v).is_some() {
                return None;
            }
        } else if let Some(v) = l.strip_prefix("output ") {
            let mut f = v.split_whitespace();
            let (p, h) = (f.next()?, f.next()?);
            if outputs.insert(p.to_string(), h.to_string()).is_some() {
                return None;
            }
        } else if !l.trim().is_empty() {
            return None; // an unknown line is a malformed receipt, not a hit
        }
    }
    let ok = drv_sha == Some(expect.drv_sha256.as_str())
        && manifest_sha == Some(expect.manifest_sha256.as_str())
        && builder == Some(expect.builder.as_str())
        && producer == Some("local-build");
    if ok { Some(outputs) } else { None }
}

/// AUTHENTICATE a `--recipe-output-db DB` before its rows can be typed
/// `RecipeOutput` (re #469 round-8): the db path is a public argv, so presence
/// of a registration row is not authority — every hashed row must be backed by
/// an ENGINE-ISSUED receipt in `<DB>.receipts/` (written by
/// `commit_scratch_to_store` after a real local build) whose `output` line
/// records exactly that (path, NAR hash) and whose producer is `local-build`.
/// A db `store-register`'d over arbitrary bytes has no receipts and refuses
/// intake; a receipt disagreeing with the row refuses intake. Same honest
/// limit as the receipt layer: a same-user writer can forge both files —
/// the daemon-owned provenance db is the follow-on (re #472); what this
/// removes is the cheaper forgery the review named, a raw db path minting a
/// typed origin.
fn authenticate_recipe_output_db(dbp: &str) -> Result<(), String> {
    let receipts_dir = format!("{dbp}.receipts");
    let mut backed: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    match std::fs::read_dir(&receipts_dir) {
        Err(e) => {
            return Err(format!(
                "recipe-output db {dbp}: provenance rejected: no engine-issued receipts at \
                 {receipts_dir} ({e}) — a registration db alone cannot be typed RecipeOutput; \
                 rebuild through the receipt-writing path (re #469 round-8)"
            ))
        }
        Ok(rd) => {
            for ent in rd {
                let ent = ent.map_err(|e| format!("read {receipts_dir}: {e}"))?;
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) != Some("receipt") {
                    continue;
                }
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| format!("read receipt {}: {e}", p.display()))?;
                let mut lines = text.lines();
                if lines.next() != Some("td-receipt v1") {
                    return Err(format!(
                        "recipe-output db {dbp}: malformed receipt {} (bad header)",
                        p.display()
                    ));
                }
                let mut producer_ok = false;
                let mut outs: Vec<(String, String)> = Vec::new();
                for l in lines {
                    if let Some(v) = l.strip_prefix("producer ") {
                        producer_ok = v == "local-build";
                    } else if let Some(v) = l.strip_prefix("output ") {
                        let mut f = v.split_whitespace();
                        if let (Some(pth), Some(h)) = (f.next(), f.next()) {
                            outs.push((pth.to_string(), h.to_string()));
                        }
                    }
                }
                if !producer_ok {
                    continue; // a receipt that is not a local build backs nothing
                }
                for (pth, h) in outs {
                    if let Some(prev) = backed.get(&pth) {
                        if prev != &h {
                            return Err(format!(
                                "recipe-output db {dbp}: receipts disagree on {pth} \
                                 ({prev} vs {h}) — refusing a contradictory record"
                            ));
                        }
                    }
                    backed.insert(pth, h);
                }
            }
        }
    }
    let data = std::fs::read(dbp).map_err(|e| format!("read recipe-output db {dbp}: {e}"))?;
    let db = store_db_read::Db::open(data)?;
    for (path, hash) in db.hashes_by_path()? {
        match backed.get(&path) {
            Some(h) if *h == hash => {}
            Some(h) => {
                return Err(format!(
                    "recipe-output db {dbp}: provenance rejected: `{path}' is registered with \
                     hash {hash} but its receipt records {h} (re #469 round-8)"
                ))
            }
            None => {
                return Err(format!(
                    "recipe-output db {dbp}: provenance rejected: no engine-issued receipt \
                     vouches for `{path}' — a registration row alone cannot be typed \
                     RecipeOutput (re #469 round-8)"
                ))
            }
        }
    }
    Ok(())
}

/// Content-addressed build cache, RECEIPT-GATED (re #469 round-7). The assembled
/// `.drv` path is deterministic (its hash covers the inputs + builder + env), so
/// if every output of PARSED is already present under SCRATCH/newstore, recorded
/// in SCRATCH/registration, AND the engine-issued SCRATCH/receipt matches the
/// CURRENT plan's identity (`ReceiptExpect`: drv bytes, typed-manifest digest,
/// builder) with the on-disk bytes re-verifying against the RECEIPT's NAR hash,
/// the build was already done — same drv ⇒ same result, the guix-daemon
/// valid-path skip. Returns the recorded outputs to reuse, or None to (re)build.
/// A registration without a matching receipt is a MISS: a record beside the
/// bytes is not its own authority. Re-hashing the cached output (cheap vs a
/// rebuild) guards a corrupted / partially-deleted entry. Consulted only by
/// `build-recipe` and the daemon build verb — AFTER manifest assembly, so the
/// reuse decision is bound to the same typed authority a fresh build would
/// stage from; the reproducibility `check` force-rebuilds, so reuse here never
/// weakens the repro proof.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn cached_realization(
    parsed: &drv::Derivation,
    scratch: &Path,
    expect: &ReceiptExpect,
) -> Result<Option<Vec<OutputReg>>, String> {
    let reg = match std::fs::read_to_string(scratch.join("registration")) {
        Ok(s) => s,
        Err(_) => return Ok(None), // never built here
    };
    // The receipt gate: no engine-issued receipt matching the CURRENT plan ⇒ miss.
    let receipt = match std::fs::read_to_string(scratch.join("receipt")) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let Some(receipt_hashes) = receipt_outputs(&receipt, expect) else {
        return Ok(None);
    };
    let recs: std::collections::HashMap<String, OutputReg> = parse_registration_blocks(&reg)
        .into_iter()
        .map(|r| (r.store_path.clone(), r))
        .collect();

    let newstore = scratch.join("newstore");
    let mut out: Vec<OutputReg> = Vec::with_capacity(parsed.outputs.len());
    for o in &parsed.outputs {
        let rec = match recs.get(&o.path) {
            Some(r) if !r.nar_hash.is_empty() => r.clone(),
            _ => return Ok(None),
        };
        // The registration must agree with the RECEIPT's hash for this output —
        // the record that vouches downstream is the one the receipt binds.
        if receipt_hashes.get(&o.path) != Some(&rec.nar_hash) {
            return Ok(None);
        }
        let base = o.path.rsplit('/').next().unwrap_or("");
        let physical = newstore.join(base);
        if !physical.exists() {
            return Ok(None);
        }
        let mut scanner = scan::Scanner::new(&rec.refs).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, &physical).map_err(|e| e.to_string())?;
        let (hash, _, _) = scanner.finish();
        if hash != rec.nar_hash {
            return Ok(None); // corrupt/partial cache entry — rebuild
        }
        out.push(rec);
    }
    Ok(Some(out))
}

/// Read a `Key: value` field from a td-native narinfo body. Test-only since the
/// engine's substitute-consumer hook was deleted (re #469): it survives as the
/// consumer half of the subst-export round-trip proof
/// (`restore_substitute_round_trips_and_rejects_corruption`).
#[cfg(test)]
fn narinfo_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(": ")))
}

/// Restore a SUBSTITUTE output from a fetched (already signature-verified) narinfo + NAR:
/// unpack the NAR into NEWSTORE/<base> (nar::read_nar), then re-serialize it and require
/// the NAR hash to equal the narinfo's NarHash. That equality is the DURABLE leg — a
/// substitute is only accepted if the bytes it restores to are the bytes the publisher
/// signed (and, since td builds are reproducible, those are the bytes a local build would
/// produce). Returns the output's registration record (refs detected by the same scanner
/// build_and_register uses, so the store-db registration is identical to a real build's).
///
/// Test-only since the engine's substitute-consumer hook (`try_substitute`) was
/// deleted (re #469 — no step class may admit remotely-vouched executable
/// inputs): it survives as the consumer half of the subst-export FORMAT proof,
/// exercised by `restore_substitute_round_trips_and_rejects_corruption`.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn restore_substitute(
    narinfo: &str,
    narfile: &Path,
    output_path: &str,
    newstore: &Path,
    deriver: &str,
) -> Result<OutputReg, String> {
    // The narinfo signature attests only that the publisher signed THIS StorePath — not
    // that it is the output we asked for. A validly-signed narinfo for some OTHER path,
    // served under this output's basename, must NOT be accepted as this output (it would
    // register another path's bytes as our derivation's result). Bind the signed StorePath
    // to the requested output_path before trusting any of its bytes.
    let signed_path = narinfo_field(narinfo, "StorePath").ok_or("narinfo: no StorePath")?;
    if signed_path != output_path {
        return Err(format!(
            "substitute StorePath does not match the requested output\n  want {output_path}\n  got  {signed_path}"
        ));
    }
    let want_hash = narinfo_field(narinfo, "NarHash").ok_or("narinfo: no NarHash")?;
    // References are recorded as basenames; rebase onto the active store dir for scanning.
    let store_dir = store::store_dir();
    let full_refs: Vec<String> = narinfo_field(narinfo, "References")
        .unwrap_or("")
        .split_whitespace()
        .map(|b| format!("{store_dir}/{b}"))
        .collect();
    let base = output_path.rsplit('/').next().unwrap_or(output_path);
    let dest = newstore.join(base);
    let _ = std::fs::remove_dir_all(&dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Restore + verify inside a closure so that ANY failure — a NAR parse/write error part
    // way through `read_nar`, or a NarHash that does not match the signed one — removes the
    // partially-written tree before returning. A half-restored output left under newstore
    // would otherwise contaminate the build fallback (which writes its fresh outputs there)
    // or a later restore attempt.
    let restored = (|| -> Result<(String, u64, Vec<String>), String> {
        let mut r = std::io::BufReader::new(
            std::fs::File::open(narfile).map_err(|e| format!("open {}: {e}", narfile.display()))?,
        );
        nar::read_nar(&mut r, &dest)
            .map_err(|e| format!("restore nar -> {}: {e}", dest.display()))?;
        // Re-serialize the restored tree exactly as build_and_register does (scanner over the
        // reference candidates), and require the hash to match what the publisher signed.
        let mut scanner = scan::Scanner::new(&full_refs).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, &dest).map_err(|e| e.to_string())?;
        let (hash, size, refs) = scanner.finish();
        if hash != want_hash {
            return Err(format!(
                "restored substitute NAR hash != signed NarHash for {output_path}\n  want {want_hash}\n  got  {hash}"
            ));
        }
        Ok((hash, size, refs))
    })();
    let (hash, size, refs) = match restored {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e);
        }
    };
    Ok(OutputReg {
        store_path: output_path.to_string(),
        nar_hash: hash,
        nar_size: size,
        refs,
        deriver: deriver.to_string(),
    })
}

/// A td-OWNED source store handed to `realize`/`build-recipe`: the `canonical`
/// source path is NOT in the daemon DB (td interned it itself, gate 285's
/// store-add-recursive), so its no-reference closure is read from the td `db`, and
/// it is staged by binding from `on_disk` (the td store dir) rather than its
/// canonical `/gnu/store/<base>` (which the daemon never created). Retires the
/// `guix repl … lower-object %builder-source` source PREP (move-off-Guile §5).
struct SrcOverride {
    canonical: String,
    on_disk: String,
    db: String,
}

/// A td-OWNED builder handed to `build-recipe` (bootstrap brick 2): the `canonical`
/// builder path is NOT in the daemon DB — td placed a stage0 td-builder there itself
/// (store-add-builder), a binary guix NEVER produced. Unlike `SrcOverride` the builder
/// HAS references (the glibc/gcc-lib it links), so its closure spans two DBs: its
/// DIRECT refs come from the builder `db` (store-add-builder registered them), and each
/// such ref's TRANSITIVE closure from the daemon/seed `store_db` (the pinned toolchain
/// lives there). The `canonical` entry is staged by binding from `on_disk` (the td
/// store dir). Lets the loop BUILD with stage0 as the builder-of-record (move-off-Guile
/// §5 "build the seed with td").
struct BuilderOverride {
    canonical: String,
    on_disk: String,
    db: String,
}

/// The machine-wide concurrent-build budget for the build daemon: `TD_BUILD_JOBS` if set,
/// else `min(nproc*3/4, MemAvailableGiB / 2)` clamped to ≥1. This is the ONE cap that all
/// agents' submissions to the single shared daemon share, so it must bound the whole box
/// (leaving ~1/4 of cores + memory headroom for interactive work and the not-yet-daemon-
/// managed heavy gates) — never a per-check slice, which N agents would multiply.
fn daemon_budget() -> usize {
    if let Ok(v) = std::env::var("TD_BUILD_JOBS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n >= 1 {
                return n;
            }
        }
    }
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cpu_cap = (nproc * 3 / 4).max(1);
    match mem_available_gib() {
        Some(g) => cpu_cap.min(((g / 2.0) as usize).max(1)),
        None => cpu_cap,
    }
}

/// MemAvailable from /proc/meminfo, in GiB (None if unreadable).
fn mem_available_gib() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb / 1024.0 / 1024.0);
        }
    }
    None
}

/// LINEAGE-verify a builder override before it can carry `ControlPlaneBuilder`
/// authority (re #469 round-10 P0 #2): `store-add-builder` + TD_BUILDER_* could
/// register ANY self-content-addressed tree as the control-plane builder —
/// content addressing (`authenticate_ca_db`) proves the tree's INTEGRITY, not
/// that its bytes came from the permitted stage0 build. The placed tree's NAR
/// hash (read from the placement db the caller supplied; the bind boundary
/// re-hashes the staged bytes against that same db row, so a record for other
/// bytes is authority for nothing) must have a lineage record that
/// `stage0_place` — the one code path that compiles the builder from this
/// repo's builder/ source — wrote at the DERIVED registry. Absent → fail
/// closed. Called at BOTH `BuilderOverride` intake sites (the TD_BUILDER_* env
/// read and build-recipe's argv triple), so no public channel mints the origin.
fn verify_builder_lineage(ov: &BuilderOverride) -> Result<(), String> {
    let data = std::fs::read(&ov.db).map_err(|e| format!("read builder db {}: {e}", ov.db))?;
    let rows = store_db_read::Db::open(data)?.hashes_by_path()?;
    let hash = rows.get(&ov.canonical).ok_or_else(|| {
        format!("builder db {} has no hashed row for {}", ov.db, ov.canonical)
    })?;
    let dir = stage0::builder_lineage_dir()?;
    if stage0::builder_lineage_recorded_in(&dir, hash)? {
        return Ok(());
    }
    Err(format!(
        "provenance rejected: builder {} ({hash}) has no stage0 lineage record under {} — \
         content addressing proves integrity, not origin: only a placement `td-builder \
         stage0-place` itself compiled from this repo's builder/ source may be typed \
         ControlPlaneBuilder (re #469)",
        ov.canonical,
        dir.display()
    ))
}

/// The optional td-owned builder override from TD_BUILDER_PATH/STORE/DB (all three set
/// together, or none) — the stage0 td-builder that a corpus drv names as its builder.
/// Shared by the daemon and its spawned per-build children (which re-read the same env).
/// The override is LINEAGE-verified here at intake (`verify_builder_lineage`): a
/// self-content-addressed tree that `stage0-place` never produced cannot become
/// the drv's builder, whatever env a daemon request carries.
fn builder_override_from_env() -> Result<Option<BuilderOverride>, String> {
    let bp = std::env::var("TD_BUILDER_PATH").ok();
    let bs = std::env::var("TD_BUILDER_STORE").ok();
    let bd = std::env::var("TD_BUILDER_DB").ok();
    match (&bp, &bs, &bd) {
        (Some(canonical), Some(store_dir), Some(db)) => {
            let base = canonical.rsplit('/').next().unwrap_or(canonical);
            let ov = BuilderOverride {
                canonical: canonical.clone(),
                on_disk: format!("{store_dir}/{base}"),
                db: db.clone(),
            };
            verify_builder_lineage(&ov)?;
            Ok(Some(ov))
        }
        (None, None, None) => Ok(None),
        _ => {
            Err("TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together".to_string())
        }
    }
}

/// The content-addressed first-output basename of `drv` — the STABLE per-drv scratch/dedup
/// key (the same drv always keys the same dir, so a valid prior realization is a cache hit).
fn drv_scratch_key(drv: &str) -> Result<String, String> {
    let content = std::fs::read(drv).map_err(|e| format!("read {drv}: {e}"))?;
    let parsed = drv::parse(&content).map_err(|e| format!("parse drv {drv}: {e}"))?;
    let first = parsed
        .outputs
        .first()
        .ok_or_else(|| format!("{drv}: derivation has no outputs"))?;
    first
        .path
        .rsplit('/')
        .next()
        .map(str::to_string)
        .ok_or_else(|| format!("{}: not a store path", first.path))
}

/// Host path of `canon`'s output tree under a keyed scratch dir (`<scr>/newstore/<base>`).
/// The prefix stripped is the ACTIVE store (`store::store_dir()`), not a hardcoded
/// `/gnu/store/` — under `TD_STORE_DIR=/td/store` the daemon's canonical output
/// paths are `/td/store/...` and a hardcoded strip would reject every one of them.
fn daemon_host_path(scr: &Path, canon: &str) -> Result<String, String> {
    let prefix = format!("{}/", store::store_dir());
    let base = canon
        .strip_prefix(&prefix)
        .ok_or_else(|| format!("{canon}: not a store path (active store: {prefix})"))?;
    Ok(scr.join("newstore").join(base).to_string_lossy().into_owned())
}

/// Is every one of `canons`' output trees present under `dir` (a keyed build scratch)?
/// The daemon's CHECK verb uses this to decide whether it can reuse a prior build as one
/// of the two independent reproducibility builds (all present ⇒ reuse, one rebuild) or
/// must do a second fresh build (any missing ⇒ the bare-CHECK fallback). Empty ⇒ false, so
/// a drv with no outputs never spuriously "reuses" a vacuous baseline.
fn output_trees_present(dir: &Path, canons: &[String]) -> bool {
    !canons.is_empty()
        && canons.iter().all(|canon| {
            daemon_host_path(dir, canon)
                .map(|p| Path::new(&p).exists())
                .unwrap_or(false)
        })
}

/// Realize ONE drv into a content-addressed keyed scratch under `scratch_base`, with
/// guix-daemon-parity cache reuse (a valid prior output is not rebuilt). Returns
/// DERIVE the blessed seed-closure db for this process's repo root (cwd) and
/// SEED-DIR — the round-8 replacement for the deleted `[BLESS-DB]` argv: the
/// db's location is a pure function of the repo's checked-in seed-lock
/// declarations, so a public caller cannot point the `BlessedSeedClosure`
/// origin at a db of their choosing. A derived path with no db on disk means
/// "nothing blessed": the build proceeds without that authority and strict
/// staging fails closed on unvouched closure items.
fn derived_bless_db(seed_dir: &str) -> Result<Option<String>, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
    match check_loop::blessed_seed_db_path(&cwd, seed_dir)? {
        Some(db) if db.is_file() => Ok(Some(db.display().to_string())),
        Some(db) => {
            eprintln!(
                "td-builder: no blessed seed-closure db at the derived {} — staging has no \
                 BlessedSeedClosure authority (run `td-builder check` to bless; re #469)",
                db.display()
            );
            Ok(None)
        }
        None => Ok(None),
    }
}

/// `derived_bless_db` with the SEED-DIR derived too (`daemon_seed_dir`: the
/// operator env override or the declared seed-lock parent) — the ladder
/// entrances (`build-plan`, `build-recipe`) take no seed-dir argv, and must
/// not grow one for this: the whole point of the derived channel is that no
/// caller input selects the db. The authority it adds is what vouches the
/// control-plane builder's host-seed runtime closure (glibc/gcc-lib) in
/// strict staging manifests.
fn derived_bless_db_auto() -> Result<Option<String>, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
    match check_loop::daemon_seed_dir(&cwd) {
        Some(seed_dir) => derived_bless_db(&seed_dir),
        None => Ok(None),
    }
}

/// (canonical store path, host output path). Run in a child process by `daemon-build`.
fn daemon_realize_one(
    drv: &str,
    seed_dir: &str,
    scratch_base: &Path,
    bless_db: Option<&str>,
) -> Result<(String, String, bool), String> {
    let ov = builder_override_from_env()?;
    let content = std::fs::read(drv).map_err(|e| format!("read {drv}: {e}"))?;
    let parsed = drv::parse(&content).map_err(|e| format!("parse drv {drv}: {e}"))?;
    let key = drv_scratch_key(drv)?;
    let scr = scratch_base.join(&key);
    let mk = |regs: &[OutputReg]| -> Result<(String, String), String> {
        let first = regs
            .first()
            .ok_or_else(|| "realize produced no outputs".to_string())?;
        let canon = first.store_path.clone();
        let host = daemon_host_path(&scr, &canon)?;
        Ok((canon, host))
    };
    // The daemon scans the live store dir: entries are canonical where they sit.
    // Strict manifests are unconditional (re #469): the vouching db is the blessed
    // seed-closure db at its DERIVED location (`derived_bless_db` in the arm) —
    // ensure_build_daemon blesses the REPO-DECLARED seed-lock closure once, and
    // the child re-derives where that record must live, so every staged item must
    // match the hash recorded at bless time and neither env nor argv can add
    // manifest authority (re #469 round-8 origin authentication).
    let extra_dbs: Vec<(String, sandbox::InputOrigin)> = bless_db
        .map(|d| vec![(d.to_string(), sandbox::InputOrigin::BlessedSeedClosure)])
        .unwrap_or_default();
    // The reuse identity comes BEFORE the cache read (re #469 round-7): the typed
    // manifest is assembled first and the prior realization must carry a receipt
    // matching it — the daemon's pre-manifest cache hit is gone. The reuse-key digest
    // is CLOSURE-SCOPED to this drv's own transitive input closure (reuse_key_manifest_digest),
    // computed by the SAME shared routine realize uses (stage_input_closure) with the SAME
    // args realize_drv is called with below, so the daemon's read key matches the write; the
    // full manifest remains what realize_drv enforces.
    let manifest_now = assemble_input_manifest(&extra_dbs, &[], ov.as_ref())?;
    let seed_dirs = [seed_dir.to_string()];
    let ic = stage_input_closure(&parsed, &seed_dirs, &store::store_dir(), &extra_dbs, &[], ov.as_ref(), None)?;
    let expect = ReceiptExpect {
        drv_sha256: sha256_hex(&content),
        // The reuse key folds the drv's DECLARED ABI builder identity (parsed.builder) and
        // EXCLUDES the builder's own content row (ic.real_builder_cb), NOT the resolved builder
        // ELF (ic.builder_exec, which drives enforcement) — an output-neutral builder recompile
        // must not move the key (see reuse_key_manifest_digest).
        manifest_sha256: reuse_key_manifest_digest(
            &ic.closure,
            &manifest_now,
            &parsed.builder,
            ic.real_builder_cb.as_deref(),
        ),
        builder: parsed.builder.clone(),
    };
    if let Some(regs) = cached_realization(&parsed, &scr, &expect)? {
        eprintln!(
            "td-builder: daemon CACHE HIT for {drv} — output already valid under {}, not rebuilding",
            scr.display()
        );
        let (c, h) = mk(&regs)?;
        return Ok((c, h, true));
    }
    eprintln!("td-builder: daemon CACHE MISS for {drv} — realizing");
    let regs = realize_drv(drv, &seed_dirs, &store::store_dir(), &extra_dbs, &scr, &[], ov.as_ref(), None)?;
    let (c, h) = mk(&regs)?;
    Ok((c, h, false))
}

/// Reproducibility check of ONE drv (the daemon's `CHECK` verb): compare two INDEPENDENT
/// realizations of the drv by per-output NAR hash. Returns the first output's (canonical,
/// host) on success, an Err naming the divergence otherwise. Run in a child process by
/// `daemon-check` so the repro rebuild ALSO counts against the budget.
///
/// The proof needs two independent builds; it does NOT need two *fresh* ones. The `daemon-build`
/// verb already realized this drv into `scratch_base/<key>` — the artifact the client consumes —
/// so this reuses THAT as the first build and rebuilds only ONCE here: two genuine builds total,
/// not three (this verb used to discard the built artifact and realize twice more, tripling the
/// single-threaded build cost that dominates `build-recipes`). In the loop substitutes are off,
/// so the build verb's output is a real local build; comparing a fresh rebuild against it is a
/// full two-independent-build reproducibility test (and additionally catches cross-run drift).
/// When no prior build output is present (a bare `CHECK` issued with no preceding build), it
/// falls back to a second fresh build, so the verb stays correct on its own.
fn daemon_check_one(
    drv: &str,
    seed_dir: &str,
    scratch_base: &Path,
    bless_db: Option<&str>,
) -> Result<(String, String), String> {
    let ov = builder_override_from_env()?;
    let seed_dirs = [seed_dir.to_string()];
    let key = drv_scratch_key(drv)?;
    let scr = scratch_base.join(format!("{key}-chk"));
    let _ = std::fs::remove_dir_all(&scr); // the rebuild here must be fresh, never a cache reuse
    let r1 = scr.join("r1");
    // Same derived-location channel as daemon_realize_one (re #469 round-8).
    let extra_dbs: Vec<(String, sandbox::InputOrigin)> = bless_db
        .map(|d| vec![(d.to_string(), sandbox::InputOrigin::BlessedSeedClosure)])
        .unwrap_or_default();
    let regs1 = realize_drv(drv, &seed_dirs, &store::store_dir(), &extra_dbs, &r1, &[], ov.as_ref(), None)?;
    // Baseline for the comparison: the build verb's already-realized output at
    // scratch_base/<key> when every output tree is present there (the loop's normal path,
    // ⇒ 2 builds total), else a SECOND fresh build (bare-CHECK fallback ⇒ the original 3).
    let built = scratch_base.join(&key);
    let canons: Vec<String> = regs1.iter().map(|r| r.store_path.clone()).collect();
    let base_dir = if output_trees_present(&built, &canons) {
        built
    } else {
        let r2 = scr.join("r2");
        let _ = realize_drv(drv, &seed_dirs, &store::store_dir(), &extra_dbs, &r2, &[], ov.as_ref(), None)?;
        r2
    };
    for reg in &regs1 {
        let canon = &reg.store_path;
        let h1 = nar_hash(&daemon_host_path(&r1, canon)?).map_err(|e| e.to_string())?;
        let h2 = nar_hash(&daemon_host_path(&base_dir, canon)?).map_err(|e| e.to_string())?;
        if h1 != h2 {
            return Err(format!("NON-REPRODUCIBLE {canon}: {h1} != {h2}"));
        }
    }
    let first = regs1
        .first()
        .ok_or_else(|| "realize produced no outputs".to_string())?;
    let canon = first.store_path.clone();
    let host = daemon_host_path(&r1, &canon)?;
    Ok((canon, host))
}

/// Build the content-scan candidate index over one or more on-disk store DIRECTORIES —
/// the guix/seed store bytes — with NO store DB and NO guix daemon. Returns the candidate
/// CANONICAL paths (`<CANONICAL_PREFIX>/<basename>`, what a reference literally present in
/// the bytes resolves to) plus a canonical→on-disk map (where those bytes actually live, so
/// a seed staged under a td-store dir is NAR-read from there). Dedup is by 32-char hash part
/// keeping the SHORTEST basename (the canonical entry, not a `.chroot`/`.check` sibling), and
/// `.lock` aux files are skipped — the daemon's own candidate criterion. An absent dir is
/// skipped (a caller may pass an optional td-store dir). This is the hoisted candidate set a
/// `scan::Scanner` matches against (store-closure-scan / #260): building it ONCE and
/// `reset()`-ing between paths keeps a whole-live-store walk O(bytes), not O(candidates).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn scan_candidate_index(
    store_dirs: &[String],
    canonical_prefix: &str,
) -> Result<(Vec<String>, std::collections::HashMap<String, String>), String> {
    use std::collections::HashMap;
    // hash part -> (basename, on-disk dir); shortest basename wins.
    let mut by_hash: HashMap<String, (String, String)> = HashMap::new();
    for dir in store_dirs {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => continue, // an absent optional store dir contributes nothing
        };
        for entry in rd {
            let entry = entry.map_err(|e| format!("{dir}: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".lock") {
                continue;
            }
            match name.split('-').next() {
                Some(p) if p.len() == 32 => {
                    let keep = match by_hash.get(p) {
                        Some((cur, _)) => name.len() < cur.len(),
                        None => true,
                    };
                    if keep {
                        by_hash.insert(p.to_string(), (name.clone(), dir.clone()));
                    }
                }
                _ => continue,
            }
        }
    }
    let mut candidates = Vec::with_capacity(by_hash.len());
    let mut on_disk = HashMap::with_capacity(by_hash.len());
    for (_h, (name, dir)) in by_hash {
        let canonical = format!("{canonical_prefix}/{name}");
        candidates.push(canonical.clone());
        on_disk.insert(canonical, format!("{dir}/{name}"));
    }
    Ok((candidates, on_disk))
}

/// Re-key candidate-index entries onto their TRUE canonical store paths (#292). A seed
/// staging dir mixes entries whose canonical homes DIFFER — guix-captured bytes live at
/// `/gnu/store`, td-built copies (a chained /td/store toolchain) at `/td/store` — but
/// `scan_candidate_index` can only stamp ONE prefix on all of them. OVERRIDES carries the
/// hash-keyed truth the caller does know: the drv's own roots (the lock is authoritative
/// for its entries' canonicals) and every td-OWNED store DB registration. Without this,
/// a root whose prefix differs from the stamped one misses the on-disk map, is never
/// content-scanned, and silently drops its whole transitive runtime closure (gate 377:
/// coreutils' gmp vanished and `expr` died on libgmp.so.10).
///
/// PRECONDITION: an entry whose true canonical differs from the stamped seed prefix must
/// be visible as a drv root or via a typed extra-db registration, or it keeps the stamp.
/// Callers satisfy this by construction — every td-built tree is created WITH its OUT-DB
/// (store-add-recursive/store-add-builder/write_output_db), and the paths that stage one
/// into a seed dir (gate 377's toolchain pair, the td-shell native store) pass that DB as
/// a typed extra db and/or name the tree as a lock root. Don't stage an unregistered
/// td-built tree into a seed dir.
fn recanonicalize_candidates(
    candidates: &mut [String],
    on_disk: &mut std::collections::HashMap<String, String>,
    overrides: &std::collections::HashMap<String, String>,
) {
    for c in candidates.iter_mut() {
        let Some(h) = store::hash_from_store_path(c) else { continue };
        let Some(true_canonical) = overrides.get(h) else { continue };
        if true_canonical == c {
            continue;
        }
        if let Some(od) = on_disk.remove(c) {
            on_disk.insert(true_canonical.clone(), od);
        }
        *c = true_canonical.clone();
    }
}

/// Compute the runtime closure of ROOTS with NO guix store DB: BFS to fixpoint, each path's
/// references found by NAR-scanning its bytes (`scan::Scanner` against the seed candidate
/// index) UNIONed with the direct references any td-OWNED store DB registered for it
/// (EXTRA_REFS — build-plan's td.dbs, whose td-built dep bytes live OUTSIDE the scanned seed
/// dirs). Content-scan is the daemon's scanForReferences — equal to `guix gc -R` for an
/// output root (gate 290, store-gc); a union with a byte-scan superset never DROPS a real
/// reference (the only unsafe direction is under-staging). SCANNER carries the candidate
/// index built ONCE; it is `reset()` between paths, so this is O(bytes scanned), not
/// O(candidates × paths). Returns the reachable canonical paths (ROOTS included).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn scan_closure_hybrid(
    scanner: &mut scan::Scanner,
    on_disk: &std::collections::HashMap<String, String>,
    extra_refs: &std::collections::HashMap<String, Vec<String>>,
    roots: &[String],
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(p) = stack.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        let mut refs: Vec<String> = Vec::new();
        // Seed bytes (this path lives in a scanned store dir): content-scan its NAR.
        if let Some(od) = on_disk.get(&p) {
            scanner.reset();
            nar::write_nar(scanner, Path::new(od))
                .map_err(|e| format!("nar {p} (at {od}): {e}"))?;
            refs.extend(scanner.refs());
        }
        // A td-OWNED store DB's DIRECT refs (a td-built dep staged outside the seed dirs).
        if let Some(rs) = extra_refs.get(&p) {
            refs.extend(rs.iter().cloned());
        }
        for r in refs {
            if !seen.contains(&r) {
                stack.push(r);
            }
        }
    }
    Ok(seen)
}

/// The transitive closure of the control-plane builder's runtime libraries by DYNAMIC
/// LINKAGE — the store paths the loader consults to run the builder — computed from the
/// loader's OWN search structure (`PT_INTERP` + `DT_RUNPATH`/`DT_RPATH`), NOT a content
/// scan. ROOTS are the builder's DIRECT runtime refs (the vouched builder-placement db's
/// closure minus the builder tree — glibc/gcc-lib). For each store path reached, EVERY ELF
/// object in it contributes its interp + run-path store dirs; the walk recurses to fixpoint.
///
/// This is narrower than `scan_closure_hybrid` (`guix gc -R`) on purpose: glibc's
/// `libc.so.6` bakes the absolute bash-static path into its `_PATH_BSHELL` STRING CONSTANT,
/// so a content scan pulls that runnable host shell into the builder's blessed closure and
/// stages it into the sandbox — an undeclared host executable an absolute `Step::Run` could
/// invoke (re #469). bash-static is nobody's DT_NEEDED and lives in nobody's run-path, so it
/// never appears here. SAFE direction: a Guix ELF's run-path lists a store dir for every
/// library it links (`validate-runpath`), and we take the store path of EVERY run-path entry
/// of EVERY object, so the result never UNDER-stages a real runtime lib — it can only
/// over-approximate (bind an extra store dir that ships an unused run-path entry), which the
/// blessed-seed manifest still vouches. `on_disk` maps a seed canonical store path to its
/// on-disk dir (the #292 re-canonicalization); a path with no row falls back to
/// `<store_dir>/<basename>`.
fn resolve_link_closure(
    roots: &[String],
    store_dirs: &[String],
    canonical_prefix: &str,
    on_disk: &std::collections::HashMap<String, String>,
) -> Result<std::collections::BTreeSet<String>, String> {
    // The canonical store PATH that a canonical file path / run-path dir lives in:
    // `<canonical_prefix>/<first component after the prefix>`. The `lib/x.so` or unnormalized
    // `../lib` tail is irrelevant to the store path, so no `..`/symlink normalization is
    // needed. A path outside the seed prefix ($ORIGIN-relative, or a host default like
    // /lib) yields None and is skipped — it names no seed store path.
    let store_path_of = |p: &str| -> Option<String> {
        let rest = p.strip_prefix(canonical_prefix)?.strip_prefix('/')?;
        let base = rest.split('/').next().filter(|b| !b.is_empty())?;
        Some(format!("{canonical_prefix}/{base}"))
    };
    let on_disk_dir = |canon: &str| -> Option<PathBuf> {
        if let Some(od) = on_disk.get(canon) {
            return Some(PathBuf::from(od));
        }
        let base = canon.strip_prefix(canonical_prefix)?.strip_prefix('/')?;
        for sd in store_dirs {
            let cand = Path::new(sd).join(base);
            if cand.exists() {
                return Some(cand);
            }
        }
        None
    };
    let mut closure: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(canon) = stack.pop() {
        if !closure.insert(canon.clone()) {
            continue;
        }
        let Some(dir) = on_disk_dir(&canon) else {
            continue; // no bytes on disk for this canonical — nothing to walk
        };
        let mut refs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut walk: Vec<PathBuf> = vec![dir];
        while let Some(d) = walk.pop() {
            let rd = match std::fs::read_dir(&d) {
                Ok(rd) => rd,
                Err(_) => continue, // an unreadable subdir contributes no link edges
            };
            for entry in rd {
                let entry = entry.map_err(|e| format!("walk {}: {e}", d.display()))?;
                let ft = entry
                    .file_type()
                    .map_err(|e| format!("{}: {e}", entry.path().display()))?;
                if ft.is_dir() {
                    walk.push(entry.path());
                    continue;
                }
                // A symlink's TEXT is not a link edge — the loader resolves it to a real ELF
                // in some store path whose OWN run-path we already parse — so only regular
                // files are inspected.
                if !ft.is_file() {
                    continue;
                }
                let (interp, rpaths) = elf::runtime_link_search(&entry.path())?;
                if let Some(i) = interp {
                    if let Some(sp) = store_path_of(&i) {
                        refs.insert(sp);
                    }
                }
                for rp in rpaths {
                    if let Some(sp) = store_path_of(&rp) {
                        refs.insert(sp);
                    }
                }
            }
        }
        for r in refs {
            if !closure.contains(&r) {
                stack.push(r);
            }
        }
    }
    Ok(closure)
}

/// Merge the DIRECT-reference graph of one or more typed td-OWNED store DBs
/// (build-plan's td.dbs / the blessed seed-closure db) into a single `path -> direct refs`
/// map, for `scan_closure_hybrid`. These DBs are td's OWN registration (never `/var/guix`);
/// they carry a td-built dep whose bytes live outside the content-scanned seed dirs, so
/// its refs are read from the DB it wrote.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn merge_extra_refs(
    extra_dbs: &[(String, sandbox::InputOrigin)],
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    let mut extra_refs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (dbp, _) in extra_dbs {
        let data = std::fs::read(dbp).map_err(|e| format!("read store db {dbp}: {e}"))?;
        let db = store_db_read::Db::open(data)?;
        for (from, tos) in db.refs_by_path()? {
            extra_refs.entry(from).or_default().extend(tos);
        }
    }
    Ok(extra_refs)
}

/// Fold one td-owned store DB's full registrations (path → NAR hash) into the
/// staged-input manifest, stamping every row with the ORIGIN class the caller
/// declared for this db at its typed intake site (re #469). A path registered
/// under two DIFFERENT hashes is a contradiction, never a merge — refuse to
/// build on it; the same path re-registered under the SAME hash keeps the
/// first record's origin.
fn manifest_add_db(
    m: &mut sandbox::StageManifest,
    db: &store_db_read::Db,
    label: &str,
    origin: sandbox::InputOrigin,
) -> Result<(), String> {
    for (p, h) in db.hashes_by_path()? {
        match m.get(&p) {
            Some(prev) if prev.nar_hash != h => {
                return Err(format!(
                    "provenance conflict: {p} is registered with hash {} and {h} ({label}) — refusing to build on a contradictory record",
                    prev.nar_hash
                ));
            }
            Some(_) => {}
            None => {
                m.insert(p, sandbox::StagedInput { nar_hash: h, origin });
            }
        }
    }
    Ok(())
}

/// Assemble a staged-input manifest from TYPED td-owned store DB files — each
/// db paired, in code at the planner's intake site, with the `InputOrigin`
/// class of the rows it contributes (re #469). This is the only constructor
/// of manifest authority: there is no path from an untyped db list, an
/// environment variable, or a cache file to a manifest entry.
fn manifest_from_typed_dbs(
    dbs: &[(String, sandbox::InputOrigin)],
) -> Result<sandbox::StageManifest, String> {
    let mut m = sandbox::StageManifest::new();
    for (dbp, origin) in dbs {
        let data = std::fs::read(dbp).map_err(|e| format!("read store db {dbp}: {e}"))?;
        manifest_add_db(&mut m, &store_db_read::Db::open(data)?, dbp, *origin)?;
    }
    Ok(m)
}

/// AUTHENTICATE a content-addressed placement db before its rows join the
/// typed staging manifest (re #469 round-8): a typed origin must be a VERIFIED
/// claim, not a label a caller minted by handing the engine a path. Every
/// hashed row must describe a store item whose on-disk bytes — read from
/// `items_dir/<basename>` — reproduce BOTH the recorded NAR hash and the
/// row's own canonical path (`make_store_path_in` over the item's name, the
/// same recomputation `auto_seed_provenance` performs on seeds). A db that
/// `store-register`/`store-add-recursive` wrote over foreign bytes fails the
/// recomputation: the db can locate bytes, but only bytes that content-address
/// to their claimed name can carry authority. Cost: one NAR hash per row per
/// process — placement dbs hold the placed item plus hashless scaffolding-ref
/// rows, so this is one source/builder tree per build, the recorded
/// re-hash-every-step decision. Verified (db, items_dir) pairs are memoized
/// per process (the same db is assembled for the reuse gate AND the realize).
///
/// Honest limit, same trust domain as the receipt layer: authentication reads
/// the db here, and `manifest_from_typed_dbs` re-reads it at assembly — a
/// same-user writer swapping the file between the two (or after the memo hit)
/// is not stopped, though `verify_staged_item` still re-hashes the ITEM bytes
/// at the bind boundary, so a swap can at most re-point rows at other
/// already-CA-valid items. Closing the window means one read feeding both
/// authentication and assembly from a daemon-owned db — the #472 follow-on.
fn authenticate_ca_db(dbp: &str, items_dir: &Path, label: &str) -> Result<(), String> {
    use std::sync::{Mutex, OnceLock};
    static VERIFIED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let memo_key = format!("{dbp}\x1f{}", items_dir.display());
    let memo = VERIFIED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    if let Ok(seen) = memo.lock() {
        if seen.contains(&memo_key) {
            return Ok(());
        }
    }
    let data = std::fs::read(dbp).map_err(|e| format!("read {label} db {dbp}: {e}"))?;
    let db = store_db_read::Db::open(data)?;
    for (path, hash) in db.hashes_by_path()? {
        let base = path.rsplit('/').next().unwrap_or(path.as_str());
        let prefix = path
            .strip_suffix(base)
            .and_then(|p| p.strip_suffix('/'))
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                format!("{label} db {dbp}: row `{path}' is not a canonical store path")
            })?;
        let on_disk = items_dir.join(base);
        let got = nar_hash_path(&on_disk).map_err(|e| {
            format!(
                "{label} db {dbp}: authenticate `{path}': hash {}: {e}",
                on_disk.display()
            )
        })?;
        if got != hash {
            return Err(format!(
                "{label} db {dbp}: provenance rejected: `{path}' is recorded with hash {hash} \
                 but the bytes at {} hash to {got} — a placement db vouches only for bytes it \
                 can reproduce (re #469 round-8)",
                on_disk.display()
            ));
        }
        let hex = got.strip_prefix("sha256:").ok_or_else(|| {
            format!("{label} db {dbp}: unexpected NAR hash format for `{path}': {got}")
        })?;
        let item_name = base.split_once('-').map_or(base, |(_, n)| n);
        let expect = store::make_store_path_in(prefix, "source", hex, item_name);
        if expect != path {
            return Err(format!(
                "{label} db {dbp}: provenance rejected: `{path}' content-addresses to \
                 `{expect}' — the item's bytes do not reproduce its own name (self-registered \
                 under a foreign address or tampered post-intern; re #469 round-8)"
            ));
        }
    }
    if let Ok(mut seen) = memo.lock() {
        seen.insert(memo_key);
    }
    Ok(())
}

/// Assemble the COMPLETE typed staging manifest a realize of this plan would
/// stage from: the caller's typed extra dbs, each td-interned source/vendor
/// placement db (`AuditedSeed` — a declared fixed-output fetch td restored
/// itself), and the builder placement db (`ControlPlaneBuilder`). This FULL
/// union is the ENFORCEMENT input — `enforce_realize_input_policy` looks up each
/// closure item in it, so it must be a superset. The reuse gates'
/// `ReceiptExpect.manifest_sha256` is a DIFFERENT thing: `reuse_key_manifest_digest`
/// scopes THIS union down to the drv's own transitive input closure (a subset,
/// computed by `stage_input_closure`) BEFORE any cache is consulted — so a reuse
/// decision is bound to the drv's real closure, NOT to unrelated plan-wide seeds
/// that would drift the key across build targets (re #469).
/// The source and builder placement dbs are AUTHENTICATED here (round-8):
/// their rows must content-address to themselves (`authenticate_ca_db`) — a
/// caller-supplied SRC-DB/TD_BUILDER_DB path locates bytes but cannot type
/// foreign bytes `AuditedSeed`/`ControlPlaneBuilder`.
fn assemble_input_manifest(
    extra_dbs: &[(String, sandbox::InputOrigin)],
    src_overrides: &[SrcOverride],
    builder_override: Option<&BuilderOverride>,
) -> Result<sandbox::StageManifest, String> {
    let mut manifest = manifest_from_typed_dbs(extra_dbs)?;
    for ov in src_overrides {
        let items_dir = Path::new(&ov.on_disk).parent().ok_or_else(|| {
            format!("source placement {} has no parent store dir", ov.on_disk)
        })?;
        authenticate_ca_db(&ov.db, items_dir, "source placement")?;
        let data =
            std::fs::read(&ov.db).map_err(|e| format!("read source db {}: {e}", ov.db))?;
        manifest_add_db(
            &mut manifest,
            &store_db_read::Db::open(data)?,
            &ov.db,
            sandbox::InputOrigin::AuditedSeed,
        )?;
    }
    if let Some(ov) = builder_override {
        let items_dir = Path::new(&ov.on_disk).parent().ok_or_else(|| {
            format!("builder placement {} has no parent store dir", ov.on_disk)
        })?;
        authenticate_ca_db(&ov.db, items_dir, "builder placement")?;
        let data =
            std::fs::read(&ov.db).map_err(|e| format!("read builder db {}: {e}", ov.db))?;
        manifest_add_db(
            &mut manifest,
            &store_db_read::Db::open(data)?,
            &ov.db,
            sandbox::InputOrigin::ControlPlaneBuilder,
        )?;
    }
    Ok(manifest)
}

/// BLESS the declared seed closure (re #469): compute the transitive closure
/// of ROOTS by content-scanning SEED-DIR, NAR-hash every member, and write a
/// td-owned store db recording (path, hash, size, refs). This is the explicit
/// trust moment for a host-provisioned seed — the pinned toolchain the daemon
/// flow builds with (§5, retired last): strict staging manifests verify every
/// later build's staged bytes against THIS record, so a seed-store item that
/// changes after the bless, or was never reachable from the declared roots,
/// refuses to stage. The canonical prefix is the ACTIVE store dir, mirroring
/// the daemon children's realize exactly. Bless ONCE per declared root set and
/// keep the db (the caller keys it by the roots): re-blessing would re-trust
/// whatever bytes are currently on disk, which is exactly the
/// existence-as-authority hole the manifest closes. The PUBLIC verb derives
/// ROOTS itself from the repo's checked-in seed-lock declarations
/// (`seed_lock_roots`) so a caller cannot mint authority for arbitrary paths;
/// this function takes them as a parameter for the engine and its tests.
fn bless_seed_closure(seed_dir: &str, roots: &[String], out_db: &Path) -> Result<usize, String> {
    if roots.is_empty() {
        return Err("seed-bless: no roots to bless".to_string());
    }
    let dirs = [seed_dir.to_string()];
    let (candidates, on_disk) = scan_candidate_index(&dirs, &store::store_dir())?;
    let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
    let closure = scan_closure_hybrid(
        &mut scanner,
        &on_disk,
        &std::collections::HashMap::new(),
        roots,
    )?;
    let closure: Vec<String> = closure.into_iter().collect();
    let mut regs: Vec<OutputReg> = Vec::with_capacity(closure.len());
    for canon in &closure {
        let od = on_disk.get(canon).ok_or_else(|| {
            format!("seed-bless: closure item {canon} is not in the seed dir {seed_dir}")
        })?;
        // Fresh scanner per item (store-register's shape): finish() yields the
        // (hash, size, refs) triple a full registration records.
        let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
        nar::write_nar(&mut s, Path::new(od))
            .map_err(|e| format!("seed-bless: nar {canon} (at {od}): {e}"))?;
        let (hash, size, refs) = s.finish();
        regs.push(OutputReg {
            store_path: canon.clone(),
            nar_hash: hash,
            nar_size: size,
            refs,
            deriver: String::new(), // a blessed seed item has no td deriver
        });
    }
    write_output_db(&regs, out_db)?;
    Ok(regs.len())
}

/// The staging-boundary input policy (re #469 round-10 — the host-tool
/// mandate): the builder accepts NO host tools. Enforced HERE at the one
/// choke point every realize path goes through (build-plan, build-recipe, the
/// daemon children) — not in any planner, because the daemon realizes drvs a
/// planner never saw (the round-10 P0 #1: a crafted drv could select host
/// bash/coreutils/tar/gzip from the blessed seed closure, or name host bash
/// as its builder outright).
///
/// Three rules over the typed manifest:
///   1. every closure item is vouched by a typed td-owned db (unchanged);
///   2. the drv's BUILDER must itself be admissible executable provenance —
///      the lineage-verified stage0 placement (`ControlPlaneBuilder`), a td
///      recipe output (`RecipeOutput` — a td-built tool may drive
///      builds), or this very engine binary (SELF_TREE, the engine
///      realizing with itself). A seed-store path — blessed or merely
///      scannable — is never a builder;
///   3. `BlessedSeedClosure` rows vouch ONLY the builder's own runtime
///      closure (BUILDER_REACH — the glibc/gcc-lib the control-plane builder
///      links until it self-hosts), never a drv input: host tools stopped
///      being reachable as inputs when their only authority was the bless db.
fn enforce_realize_input_policy(
    drv_builder: &str,
    roots: &[String],
    closure: &[String],
    builder_reach: &std::collections::BTreeSet<String>,
    manifest: &sandbox::StageManifest,
    self_tree: Option<&str>,
) -> Result<(), String> {
    // `r` owns the builder iff the builder path IS `r` or lives under `r/…`; the
    // `starts_with('/')` guard blocks prefix confusion (`/gnu/store/abc` does not
    // own `/gnu/store/abcd/…`), and the `!r.is_empty()` guard blocks the empty
    // root, which would else own every absolute builder (`strip_prefix("")` is the
    // whole path) — defensive: an empty root is not an admissible manifest entry.
    let owns = |r: &str| {
        !r.is_empty()
            && (drv_builder == r || drv_builder.strip_prefix(r).is_some_and(|t| t.starts_with('/')))
    };
    if !self_tree.filter(|t| !t.is_empty()).is_some_and(owns) {
        let admissible = roots.iter().any(|r| {
            owns(r)
                && manifest.get(r.as_str()).is_some_and(|si| {
                    matches!(
                        si.origin,
                        sandbox::InputOrigin::ControlPlaneBuilder
                            | sandbox::InputOrigin::RecipeOutput
                    )
                })
        });
        if !admissible {
            return Err(format!(
                "provenance rejected: drv builder {drv_builder} is not admissible executable \
                 provenance (re #469) — a build may be driven only by the lineage-verified \
                 control-plane builder, a td recipe output, or the engine itself; a host tool \
                 is never a builder"
            ));
        }
    }
    for e in closure {
        let (canonical, _) = sandbox::split_closure_entry(e);
        match manifest.get(canonical) {
            None => {
                return Err(format!(
                    "provenance rejected: closure item {canonical} has no td-owned store-db record (re #469) — every staged input must be vouched for by the plan's seed db, a prior step's td.db, a source/builder placement db, or the blessed seed-closure db"
                ));
            }
            Some(si)
                if matches!(si.origin, sandbox::InputOrigin::BlessedSeedClosure)
                    && !builder_reach.contains(canonical) =>
            {
                return Err(format!(
                    "provenance rejected: closure item {canonical} is blessed-seed bytes outside \
                     the builder's own runtime closure (re #469) — host tools are not admissible \
                     build inputs: a build may stage only td recipe outputs, pinned sources, and \
                     the control-plane builder's runtime libs"
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Re-key the builder's OWN closure entry from its real content-addressed path to the
/// stable ABI builder-identity path the drv names. An entry whose CANONICAL half is
/// `real_cb` (`real_cb`, or `real_cb\tON-DISK`) becomes `stable_id\tON-DISK`, so the
/// sandbox binds the real builder bytes at the stable path and execs it there; a bare
/// entry (daemon-resident, no on-disk override) uses `real_cb` itself as the bind source.
/// Every other closure entry — the builder's runtime refs and every build input — is
/// untouched. A no-op on a closure with no `real_cb` entry.
fn rekey_builder_entry(closure: Vec<String>, real_cb: &str, stable_id: &str) -> Vec<String> {
    closure
        .into_iter()
        .map(|e| match e.split_once('\t') {
            Some((canon, on_disk)) if canon == real_cb => format!("{stable_id}\t{on_disk}"),
            None if e == real_cb => format!("{stable_id}\t{real_cb}"),
            _ => e,
        })
        .collect()
}

/// Build the staging manifest the SANDBOX verifies against for an ABI drv: a clone of the
/// authority manifest with the real builder's record MIRRORED onto the stable ABI identity
/// path. `sandbox::verify_staged_item` re-hashes every staged item against the manifest
/// keyed by the closure entry's canonical (left) half, and `rekey_builder_entry` moved the
/// builder entry's canonical from the real content path to `stable_id` — so without this
/// mirror the sandbox finds no record for `stable_id` and refuses to stage the builder. The
/// mirror carries the SAME nar_hash + origin as the real builder's record (the same real
/// bytes bind, via the entry's untouched on-disk half), and the real record is left in place,
/// so the reuse digest — `manifest_digest` over the un-mirrored authority manifest — is
/// unaffected. A no-op clone when the real builder has no manifest record: enforcement ran
/// first (over the real path), so a reachable realize always has that record to mirror.
fn manifest_with_builder_alias(
    manifest: &sandbox::StageManifest,
    real_builder_cb: &Option<String>,
    stable_id: &str,
) -> sandbox::StageManifest {
    let mut m = manifest.clone();
    if let Some(real) = real_builder_cb {
        if let Some(si) = manifest.get(real) {
            m.insert(stable_id.to_string(), si.clone());
        }
    }
    m
}

/// A drv's staged input closure plus the resolved real builder — everything the
/// reuse-key digest and `realize_drv`'s staging need that is a pure function of the
/// drv and its declared input authority (NOT of the build's OUTPUT). Computed by
/// `stage_input_closure` so the WRITE site (`realize_drv`) and the reuse-key READ
/// sites (`build_recipe`, `daemon_realize_one`) derive the IDENTICAL closure from
/// the same routine — any asymmetry reintroduces the very cache miss the
/// closure-scoped reuse key exists to remove.
struct InputClosure {
    /// The resolved real builder's content path (`ov.canonical` / `self_store_path`),
    /// or None for a non-td drv. The closure's builder entry sits at THIS path
    /// (pre-rekey); realize re-keys it onto the ABI identity after enforcement.
    real_builder_cb: Option<String>,
    /// The real builder EXEC path (`{real}/bin/td-builder`, or `parsed.builder` for a
    /// non-td drv) — what actually runs and what #469 enforces. The reuse key does NOT
    /// fold this: it folds the drv's DECLARED ABI identity (`parsed.builder`) so an
    /// output-neutral builder recompile does not invalidate reuse (see
    /// `reuse_key_manifest_digest`).
    builder_exec: String,
    /// The drv's input roots (input-srcs + input-drv outputs), the ABI builder token
    /// substituted to the real builder.
    roots: Vec<String>,
    /// The transitive input closure, PRE builder-rekey: each entry `canonical` or
    /// `canonical\ton-disk`. The exact set realize stages, enforces over, and the
    /// reuse key scopes to.
    closure: Vec<String>,
    /// The closure slice reachable from the builder — the only slice a
    /// `BlessedSeedClosure` row may vouch (`enforce_realize_input_policy`).
    builder_reach: std::collections::BTreeSet<String>,
}

/// Compute a drv's staged input closure with NO guix store DB: resolve the real
/// builder behind the drv's ABI-token builder, then CONTENT-SCAN the seed store
/// dir(s) for the drv's roots (`guix gc -R`, gate 290) unioned with any td-owned
/// db refs, the td-placed builder's dynamic-linkage closure, and each td-interned
/// source/vendor tree's own-db closure. Shared by `realize_drv` (which then
/// enforces + stages + re-keys the builder entry) and the reuse-key read sites
/// (which scope the reuse digest to it BEFORE the cache read). Reads only INPUT
/// trees/dbs — all materialized before this step's build — so read and write see
/// the same bytes and derive the same closure.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn stage_input_closure(
    parsed: &drv::Derivation,
    seed_store_dirs: &[String],
    seed_canonical_prefix: &str,
    extra_dbs: &[(String, sandbox::InputOrigin)],
    src_overrides: &[SrcOverride],
    builder_override: Option<&BuilderOverride>,
    td_store: Option<&Path>,
) -> Result<InputClosure, String> {
    // A recipe drv (assemble_recipe_drv) names the STABLE ABI builder-identity path
    // (store::builder_identity_path) as its builder + builder input-src — keyed on the
    // ABI, not the builder ELF. Resolve it to the REAL builder up front so BOTH the #469
    // provenance checks below AND staging run against the executable that actually drives
    // the build (its real content-addressed path — exactly what the drv named directly
    // before the ABI token). The real builder is this realize's override, else its own
    // running binary; both are ABI-compatible with the token by construction.
    //
    // Guard against ABI SKEW: the drv's `builder` line is `<identity>/bin/td-builder`, and
    // that identity MUST be this realize's ABI token path — assemble and realize must agree
    // on BUILDER_ABI/TD_BUILDER_ABI. A stale on-disk drv, or the daemon's separate assemble
    // and realize processes disagreeing on the env override, would otherwise leave the
    // unresolved token path to be content-scanned, not found, and fail deep in staging with
    // a confusing "No such file". Catch it CLEARLY here. A drv whose builder is NOT a
    // td-builder identity (a non-td/guix drv) leaves real_builder_cb None — the roots
    // substitution and the closure re-key below are both no-ops, and provenance runs
    // against parsed.builder unchanged.
    let stable_builder_id = store::builder_identity_path();
    let real_builder_cb: Option<String> = match parsed.builder.strip_suffix("/bin/td-builder") {
        Some(drv_builder_id) => {
            if drv_builder_id != stable_builder_id {
                return Err(format!(
                    "realize: drv builder identity `{drv_builder_id}` != this builder's ABI \
                     identity `{stable_builder_id}` — the drv was assembled under a different \
                     BUILDER_ABI/TD_BUILDER_ABI than this realize"
                ));
            }
            Some(match builder_override {
                Some(ov) => ov.canonical.clone(),
                None => self_store_path()?,
            })
        }
        None => None,
    };
    // The REAL builder EXEC path — what actually runs, and whose provenance #469 enforces.
    // For an ABI drv it is the resolved real builder's `.../bin/td-builder`; for a non-td
    // drv, parsed.builder verbatim. Every #469 builder-provenance check below keys on THIS,
    // never the virtual identity the drv names — so enforcement sees exactly the real
    // builder path the drv used to name directly, and its behavior is unchanged by the ABI
    // token.
    let builder_exec = match &real_builder_cb {
        Some(real) => format!("{real}/bin/td-builder"),
        None => parsed.builder.clone(),
    };
    // BIND the builder override to THIS drv (re #469 round-8): the override's db may vouch
    // only for the builder that will actually run — post-ABI that is the RESOLVED builder
    // (real_builder_cb), which the skew guard just proved ABI-compatible with the drv. A
    // `ControlPlaneBuilder` record for some other binary is authority for nothing here.
    if let Some(ov) = builder_override {
        let under = format!("{}/", ov.canonical);
        if builder_exec != ov.canonical && !builder_exec.starts_with(&under) {
            return Err(format!(
                "builder override {} does not match the drv's resolved builder {} — a builder \
                 placement db carries authority only for the drv's own builder (re #469)",
                ov.canonical, builder_exec
            ));
        }
    }
    // Input ROOTS: the drv's source inputs, plus each input derivation's requested
    // output paths (resolved by reading that input .drv).
    let mut roots: Vec<String> = drv_declared_inputs(parsed)?;
    // Substitute the stable ABI builder-identity path in `roots` with the REAL builder, so
    // the closure logic (which keys its builder branch on the override's canonical) runs
    // UNCHANGED over the real path; the ONE builder entry is re-keyed back to the stable id
    // AFTER provenance is enforced (below), so the sandbox binds the real builder bytes AT
    // the path the drv execs. Non-ABI drv (real_builder_cb None): no-op.
    if let Some(real) = &real_builder_cb {
        for r in roots.iter_mut() {
            if *r == stable_builder_id {
                *r = real.clone();
            }
        }
    }
    // Compute the input closure with NO guix store DB: CONTENT-SCAN the seed store dir(s)
    // for the seed/toolchain roots (scanForReferences == `guix gc -R` for an output root,
    // gate 290), UNIONed with the direct refs any td-OWNED store DB registers (build-plan's
    // td.dbs — a td-built dep staged outside the seed dirs). The candidate index (canonical
    // paths + a canonical→on-disk map) is built ONCE; the Scanner is reset() between roots.
    if seed_store_dirs.is_empty() {
        return Err("realize: no seed store dir given".to_string());
    }
    let extra_refs = merge_extra_refs(extra_dbs)?;
    // TRUE-canonical overrides for the index, keyed by store hash (#292): td-owned DB
    // registrations first, then the drv's own roots (the drv/lock is the stronger
    // authority where both name the same hash). Every other seed entry keeps
    // SEED_CANONICAL_PREFIX.
    let mut canonical_overrides: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for p in extra_refs.keys() {
        if let Some(h) = store::hash_from_store_path(p) {
            canonical_overrides.insert(h.to_string(), p.clone());
        }
    }
    for r in &roots {
        if let Some(h) = store::hash_from_store_path(r) {
            canonical_overrides.insert(h.to_string(), r.clone());
        }
    }
    let (mut candidates, mut on_disk) =
        scan_candidate_index(seed_store_dirs, seed_canonical_prefix)?;
    recanonicalize_candidates(&mut candidates, &mut on_disk, &canonical_overrides);
    let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
    // Each td-OWNED interned tree (the recipe source AND the vendored-crate tree) has its
    // own DB — the seed store has no row for it. Open them paired with their override so a
    // root can be matched to its store + db. Both are no-reference content-addressed trees
    // (store-add-recursive), so they share the SrcOverride handling.
    let src_dbs: Vec<(&SrcOverride, store_db_read::Db)> = src_overrides
        .iter()
        .map(|ov| {
            let data =
                std::fs::read(&ov.db).map_err(|e| format!("read source db {}: {e}", ov.db))?;
            Ok::<_, String>((ov, store_db_read::Db::open(data)?))
        })
        .collect::<Result<_, _>>()?;
    let mut closure: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // The closure slice reachable FROM THE DRV'S BUILDER (the builder tree plus
    // its refs' transitive closures) — the ONLY slice `BlessedSeedClosure` rows
    // may vouch (`enforce_realize_input_policy`, re #469 round-10): the blessed
    // db exists to vouch the control-plane builder's own host-seed runtime
    // closure (glibc/gcc-lib), never a host tool a drv selects as an input.
    let mut builder_reach: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Same prefix-safe + non-empty ownership test as enforce_realize_input_policy's
    // `owns`: only a root that is (or contains) the drv's builder tree tracks into
    // builder_reach, so the bless db vouches exactly the builder's runtime closure.
    let builder_owns = |r: &str| {
        !r.is_empty()
            && (builder_exec == r
                || builder_exec.strip_prefix(r).is_some_and(|t| t.starts_with('/')))
    };
    for r in &roots {
        // A td-interned tree (the recipe source OR the vendored-crate tree): no-ref
        // closure from its OWN db, the entry bound FROM on_disk (canonical\ton-disk).
        if let Some(entry) = src_dbs.iter().find(|e| r == &e.0.canonical) {
            let (ov, sdb) = (entry.0, &entry.1);
            for p in sdb.closure(r)? {
                let line = if p == ov.canonical {
                    format!("{p}\t{}", ov.on_disk)
                } else {
                    p
                };
                closure.insert(line);
            }
            continue;
        }
        // The td-placed builder gets its STAGED closure from its own DYNAMIC LINKAGE;
        // every other root from the seed content-scan (∪ any td-owned extra dbs).
        match builder_override {
            // The td-placed builder: the tree binds from on_disk (canonical\ton-disk), and
            // its runtime deps are resolved by DYNAMIC LINKAGE (PT_INTERP + DT_RUNPATH) from
            // the builder BINARY itself — NOT from a content scan of it. A content scan drags
            // bash-static into the sandbox (glibc's libc.so.6 bakes the bash path into its
            // `_PATH_BSHELL` string constant — a runnable host shell the loader never links),
            // and a STATIC builder inlines that very .rodata, so its own content-scan direct
            // refs would name glibc + bash-static even though it links NOTHING (re #469).
            // Linkage stages exactly the loader's search set — empty for a static builder,
            // the pinned glibc/gcc-lib for a dynamic one — and nothing a glibc helper SCRIPT
            // (bin/ldd) or HEADER (paths.h) merely names.
            Some(ov) if r == &ov.canonical => {
                // Bind the builder tree itself from on_disk.
                builder_reach.insert(ov.canonical.clone());
                closure.insert(format!("{}\t{}", ov.canonical, ov.on_disk));
                // Stage its runtime deps by LINKAGE from the builder binary ITSELF, not
                // from the builder db's content-scanned DIRECT refs. A STATIC stage0
                // builder (builder/src/stage0.rs) has NO link deps — but it inlines
                // glibc's `.rodata`, i.e. glibc's own store path AND glibc's
                // `_PATH_BSHELL` bash-static path, which a CONTENT scan records as direct
                // refs the loader never links. Walking the builder tree's PT_INTERP +
                // DT_RUNPATH stages EXACTLY the loader search set: empty for a fully
                // static builder (no host bytes leak, no unvouched bash-static), the
                // pinned glibc/gcc-lib for a dynamic one (re #469). The builder's own
                // store db (ov.db, content-scanned direct refs — which DO name bash-static
                // for a static builder) is still read+authenticated by
                // assemble_input_manifest to record ControlPlaneBuilder provenance; it no
                // longer sources what gets STAGED, so those extra names never reach a sandbox.
                let mut od = on_disk.clone();
                od.insert(ov.canonical.clone(), ov.on_disk.clone());
                let mut linkage = resolve_link_closure(
                    std::slice::from_ref(&ov.canonical),
                    seed_store_dirs,
                    seed_canonical_prefix,
                    &od,
                )?;
                // resolve_link_closure SEEDS its root into the output; the builder tree
                // is already bound (canonical\ton-disk) above, so drop the bare entry.
                linkage.remove(&ov.canonical);
                for canon in linkage {
                    builder_reach.insert(canon.clone());
                    closure.insert(canon);
                }
            }
            _ => {
                let track = builder_owns(r);
                for q in scan_closure_hybrid(
                    &mut scanner,
                    &on_disk,
                    &extra_refs,
                    std::slice::from_ref(r),
                )? {
                    if track {
                        let (canon, _) = sandbox::split_closure_entry(&q);
                        builder_reach.insert(canon.to_string());
                    }
                    closure.insert(q);
                }
            }
        }
    }
    // A td-BUILT dep's files live under TD_STORE/<base>, not /gnu/store. Re-key those
    // closure entries to `canonical\ton-disk` so the sandbox binds them FROM td's store
    // (split_closure_entry) — the build-plan chaining edge, the same on-disk encoding
    // SRC_OVERRIDE uses. Bare guix-seed entries and already-overridden entries (the
    // SRC_OVERRIDE source) pass through unchanged. The on-disk half rides through
    // closure.txt, so a later `td-builder check` of this drv stages the dep with no
    // extra argument.
    let closure: Vec<String> = closure
        .into_iter()
        .map(|e| {
            if e.contains('\t') {
                return e;
            }
            if let Some(ts) = td_store {
                // Re-key by BASENAME (store hashes are unique), so a seed staging-store that holds BOTH
                // /gnu/store deps AND /td/store td-built deps (e.g. a chained /td/store toolchain, brick 8)
                // binds every input from the seed regardless of its canonical prefix — not only paths under
                // the active store_dir(). Bare entries whose basename isn't in the seed pass through.
                let base = e.rsplit('/').next().unwrap_or(e.as_str());
                if !base.is_empty() {
                    let on_disk = ts.join(base);
                    if on_disk.exists() {
                        return format!("{e}\t{}", on_disk.display());
                    }
                }
            }
            e
        })
        .collect();
    Ok(InputClosure { real_builder_cb, builder_exec, roots, closure, builder_reach })
}

/// Realize DRV with NO guix-daemon and NO guix store DB: compute the input closure ITSELF by
/// CONTENT-SCANNING the seed store dir(s) (the daemon's scanForReferences / `guix gc -R`,
/// gate 290) — no `/var/guix/db` read — build it in the userns sandbox (build_and_register),
/// and register the output(s) into a td store-db at SCRATCH/td.db. Returns the per-output
/// records. Shared by `realize`, `build-recipe` and the build daemon. SRC_OVERRIDE, when set,
/// supplies the recipe source from a td-owned store instead of the daemon store (no `guix
/// repl` interning). SEED_STORE_DIRS is the set of store DIRECTORIES the seed/toolchain
/// closure is content-scanned over (`/gnu/store`, or the unpacked seed store); EXTRA_DBS is
/// the set of td-OWNED store DBs whose td-built deps live outside those dirs, each TYPED
/// with the `InputOrigin` class its rows carry into the staging manifest (build-plan
/// passes the seed db as `AuditedSeed` + the prior steps' td.dbs as `RecipeOutput` so a
/// downstream build's closure spans both). The drv's `builder` is the STABLE ABI identity
/// path (store::builder_identity_path), so realize resolves the REAL builder to stage +
/// bind at it: BUILDER_OVERRIDE, when set, names a td-owned builder (a td-bootstrapped
/// stage0, not the guix-built td-builder) — its entry binds from the builder DB and its
/// direct refs' TRANSITIVE closures come from the seed content-scan — else the realize's
/// own running binary (self_store_path). Provenance is enforced against the RESOLVED real
/// builder; the builder's closure entry is then re-keyed onto the stable identity path
/// (rekey_builder_entry) so the sandbox binds the real bytes AT the path the drv execs.
/// TD_STORE, when set, names td's
/// own store dir holding td-BUILT deps: a closure path whose tree lives under TD_STORE/<base>
/// is emitted `canonical\ton-disk` so the sandbox binds it FROM THERE (the build-plan chaining
/// edge) — the same on-disk encoding SRC_OVERRIDE uses. SEED_CANONICAL_PREFIX is the canonical
/// home of the seed dirs' entries — `/gnu/store` for a guix-captured seed/warm-seed staging
/// dir, the live `store::store_dir()` when scanning the active store itself; per-entry truth
/// (a td-built copy inside a guix seed dir, or vice versa) is restored from the drv roots +
/// td-owned DBs by `recanonicalize_candidates` (#292).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn realize_drv(
    drv_path: &str,
    seed_store_dirs: &[String],
    seed_canonical_prefix: &str,
    extra_dbs: &[(String, sandbox::InputOrigin)],
    scratch: &Path,
    src_overrides: &[SrcOverride],
    builder_override: Option<&BuilderOverride>,
    td_store: Option<&Path>,
) -> Result<Vec<OutputReg>, String> {
    let bytes = std::fs::read(drv_path).map_err(|e| e.to_string())?;
    let parsed = drv::parse(&bytes).map_err(|e| e.to_string())?;
    // Resolve the real builder + compute this drv's transitive input CLOSURE with the
    // SHARED routine the reuse-key read sites also call (stage_input_closure), so read and
    // write scope the reuse key to an identical closure — any asymmetry reintroduces the
    // very cache miss the scoping removes. Destructured into the same locals the enforce /
    // stage / builder-re-key logic below has always used.
    let stable_builder_id = store::builder_identity_path();
    let InputClosure { real_builder_cb, builder_exec, roots, closure, builder_reach } =
        stage_input_closure(
            &parsed,
            seed_store_dirs,
            seed_canonical_prefix,
            extra_dbs,
            src_overrides,
            builder_override,
            td_store,
        )?;
    eprintln!(
        "td-builder: realize computed the input closure ITSELF — {} paths by CONTENT-SCANNING {} seed store dir(s) (+ {} td-owned db(s)); no /var/guix/db, no guix gc, no daemon",
        closure.len(),
        seed_store_dirs.len(),
        extra_dbs.len()
    );
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    // STRICT PROVENANCE — the ONLY mode (re #469): assemble the staged-input
    // manifest from TYPED td-OWNED store DBs ONLY — the interned-seed/extra dbs (typed at
    // the caller's intake site), each source placement's db (`AuditedSeed` — a td-interned
    // pinned fetch), the builder placement's db (`ControlPlaneBuilder`) — and require
    // EVERY closure item to be accounted for before anything is staged. The
    // content-scanned seed DIRECTORY thereby contributes bytes, never authority: an item
    // the dbs don't vouch for reds HERE, and sandbox::build re-hashes each item against
    // the manifest at the bind boundary. Scaffolding rows (hashless placement refs) vouch
    // for nothing by construction. The SAME assembly (assemble_input_manifest) feeds the
    // reuse gates' manifest digest, so a cache hit binds to this exact authority set.
    //
    // Enforced over the REAL builder (builder_exec) and the PRE-re-key closure — whose
    // builder entry is still at the real content path the manifest vouches — so provenance
    // enforcement is byte-for-byte the pre-ABI-token behavior (the ABI token only renames
    // the builder for staging, below, never for the authority check).
    let manifest = assemble_input_manifest(extra_dbs, src_overrides, builder_override)?;
    enforce_realize_input_policy(
        &builder_exec,
        &roots,
        &closure,
        &builder_reach,
        &manifest,
        self_store_path().ok().as_deref(),
    )?;
    // The reuse-key digest (re #469): scope the plan-wide `manifest` to THIS drv's own
    // transitive input closure, EXCLUDE the builder's own content row (real_builder_cb), and
    // fold in the drv's DECLARED ABI builder identity (parsed.builder — content-independent),
    // NOT the resolved builder ELF (builder_exec, which stays the ENFORCEMENT input above).
    // Computed here — over the PRE-re-key `closure`, whose builder entry is still at the real
    // builder's content path the manifest vouches, hence the exclusion — so a later reuse
    // re-derives the identical key, and an output-neutral builder recompile (same BUILDER_ABI)
    // does not move it whether the builder ran in-process or as an override. The full
    // `manifest` above stays the ENFORCEMENT input, untouched.
    let reuse_manifest_sha256 =
        reuse_key_manifest_digest(&closure, &manifest, &parsed.builder, real_builder_cb.as_deref());
    // ABI: provenance is now enforced over the real builder path, so re-key the builder's
    // OWN closure entry from its real content path to the stable ABI identity path the drv
    // names (its runtime refs keep their real canonical paths). The sandbox binds the real
    // builder bytes (the on-disk half) at that path and execs `{stable_id}/bin/td-builder`
    // there — so the recipe's identity dropped the builder ELF but the build still runs the
    // real builder. closure.txt carries the re-keyed entry, so a later `td-builder check`
    // of this drv stages identically. A non-td drv (real_builder_cb None) is unchanged.
    let closure = match &real_builder_cb {
        Some(real) => rekey_builder_entry(closure, real, &stable_builder_id),
        None => closure,
    };
    std::fs::write(scratch.join("closure.txt"), closure.join("\n")).map_err(|e| e.to_string())?;
    // The manifest the SANDBOX verifies against must cover the builder at the stable identity
    // path the re-keyed closure entry now names (verify_staged_item keys on the canonical
    // half); mirror the real builder's record onto it. The authority `manifest` is left
    // untouched, and the reuse digest was already taken (above) over the real, vouched builder.
    let staging_manifest = manifest_with_builder_alias(&manifest, &real_builder_cb, &stable_builder_id);
    // The durable audit beside closure.txt: what may stage, under which hash + provenance
    // class (the origin column). Written from the STAGING manifest so the two files DESCRIBE
    // each other — the re-keyed builder entry's stable-id path in closure.txt appears here
    // too, as the mirror of the real builder's record (same hash + origin), not only the real
    // path. The un-mirrored authority `manifest` already fed the reuse digest above.
    let mut lines = String::new();
    for (p, si) in &staging_manifest {
        lines.push_str(&format!("{p} {} {}\n", si.nar_hash, si.origin.as_str()));
    }
    std::fs::write(scratch.join("provenance.manifest"), lines).map_err(|e| e.to_string())?;
    let regs = build_and_register(drv_path, &closure, scratch, &staging_manifest)?;
    // The builder identity path is a VIRTUAL mount alias: the real builder bytes are
    // registered under the builder's OWN content-addressed path, never under this token
    // path (which has no materialized store member / NAR). An output must therefore not
    // retain it as a reference, or its registered closure would carry a dangling store
    // path that later gc/verify/export cannot resolve. A real recipe never references the
    // build DRIVER (the builder is not a runtime input), so reject it loudly here rather
    // than register an unresolvable ref. Scoped to an ABI drv (real_builder_cb Some).
    if real_builder_cb.is_some() {
        for r in &regs {
            if r.refs.iter().any(|rf| rf == &stable_builder_id) {
                return Err(format!(
                    "realize: output {} references the builder identity path {stable_builder_id}, \
                     a virtual mount alias with no materialized store path — a recipe must not \
                     embed the build driver",
                    r.store_path
                ));
            }
        }
    }
    // td OWNS the store record of its build: write a td store-db registering the
    // realized output(s) — the daemon's post-build registration, in pure Rust.
    write_output_db(&regs, &scratch.join("td.db"))?;
    // The engine-issued build RECEIPT (re #469 round-7): bind these outputs to the
    // identity of the plan that produced them — drv bytes, CLOSURE-SCOPED manifest
    // digest (computed above, over the drv's own input closure — NOT the plan-wide
    // `manifest` union enforcement uses — so a higher target's extra unrelated seeds do
    // not drift the key), builder — so a later reuse must re-derive the same identity to hit.
    let expect = ReceiptExpect {
        drv_sha256: sha256_hex(&bytes),
        manifest_sha256: reuse_manifest_sha256,
        builder: parsed.builder.clone(),
    };
    std::fs::write(scratch.join("receipt"), receipt_text(&expect, &regs))
        .map_err(|e| e.to_string())?;
    eprintln!(
        "td-builder: realize registered {} output(s) into td's store-db {}",
        regs.len(),
        scratch.join("td.db").display()
    );
    Ok(regs)
}

/// The td-builder store path of the RUNNING binary (…/td-builder-<v>), stripped of
/// the trailing `/bin/td-builder` — so a recipe built by td references the very
/// builder that built it, with no Guile resolution.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn self_store_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let s = exe.to_string_lossy();
    let bin = s
        .strip_suffix("/bin/td-builder")
        .ok_or_else(|| format!("td-builder is not at <store>/bin/td-builder: {s}"))?;
    Ok(bin.to_string())
}

/// build-recipe: build a TS-authored recipe with NO Guile and NO guix-daemon in the
/// path. Reads the recipe JSON (produced Guile-free by ts-eval), resolves EVERY input
/// from LOCK (`NAME <path>`, no specification->package) — the source is keyed
/// `<name>-source`, the td-builder builder is the running binary, every other lock
/// entry is a build input — assembles the `.drv` itself (store::assemble_drv, the
/// inputs as input-SOURCES), and realizes it (realize_drv over STORE-DB). The
/// toolchain + lock are the guix-built SEED (§5, retired last); nothing in the
/// build path is guix/Guile. The recipe's `buildSystem` selects the phase runner —
/// `"gnu"` → `autotools-build` (configureFlags/phases), `"rust"` → `rust-build`
/// (cargo; installs the recipe's `bins`). Usage: build-recipe RECIPE-JSON LOCK
/// SCRATCH STORE-DB [SRC-STORE-DIR SRC-DB]
///
/// SRC-STORE-DIR + SRC-DB (optional) make the `<name>-source` a td-OWNED source: td
/// interned the tree ITSELF (store-add-recursive) into SRC-STORE-DIR + SRC-DB, so the
/// source is staged from there and its closure read from SRC-DB — no `guix repl …
/// lower-object` daemon interning in the source PREP (move-off-Guile §5). Omitted →
/// the source is a daemon-resident store path, exactly as before.
///
/// BUILDER_STORE (optional, `(canonical, store_dir, db)`) makes the drv's `builder` a
/// td-OWNED stage0 td-builder (store-add-builder placed it at `canonical`, restored
/// under `store_dir`, refs in `db`) instead of the running guix-built binary — the
/// loop then BUILDS with a binary guix never produced (bootstrap brick 2). Omitted →
/// the builder is `self_store_path()` (the guix-built td-builder), exactly as before.
///
/// STORE_DBS (the closure's store-db set) and TD_STORE (td's own store dir for td-BUILT
/// deps) thread straight through to realize_drv — build-plan passes the multi-db set +
/// td-store so a downstream step consumes an upstream step's td-built output.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_recipe(
    recipe_json: &str,
    lock_file: &str,
    scratch: &Path,
    seed_store_dirs: &[String],
    seed_canonical_prefix: &str,
    extra_dbs: &[(String, sandbox::InputOrigin)],
    src_store: Option<(&str, &str)>,
    vendor_store: Option<(&str, &str, &str)>,
    builder_store: Option<(&str, &str, &str)>,
    td_store: Option<&Path>,
    persist: Option<(&str, &str)>,
) -> Result<Vec<OutputReg>, String> {
    // A td-OWNED builder (optional, bootstrap brick 2): the drv's `builder` is a stage0
    // td-builder td placed at `canonical` (store-add-builder), restored under store_dir,
    // refs in db — a binary guix never produced. The on-disk tree is the canonical
    // basename under store_dir. Omitted → the running guix-built binary.
    // LINEAGE-verified at this intake (`verify_builder_lineage`, re #469): the
    // argv triple locates bytes but cannot type a tree `stage0-place` never
    // produced as the control-plane builder. The drv's `builder` line is the stable
    // ABI-token identity (assemble_recipe_drv), so this override is resolved + bound at
    // realize — no builder path is baked into the drv here.
    let builder_override = builder_store
        .map(|(canonical, store_dir, db)| {
            let base = canonical.rsplit('/').next().unwrap_or(canonical);
            let ov = BuilderOverride {
                canonical: canonical.to_string(),
                on_disk: format!("{store_dir}/{base}"),
                db: db.to_string(),
            };
            verify_builder_lineage(&ov).map(|()| ov)
        })
        .transpose()?;
    // td assembles the .drv ITSELF (pure Rust, no guix (derivation …), no Guile, no
    // daemon) and writes it to SCRATCH — the SAME assembly `assemble-recipe` uses, so a
    // separate process (the build daemon) realizes a byte-identical td-assembled drv. The
    // drv's builder is the stable ABI-token identity path; the real builder (this
    // override, or the running binary) is resolved + bound at realize.
    let (drv_path, drv_file, parsed, source) = assemble_recipe_drv(
        recipe_json,
        lock_file,
        scratch,
        vendor_store.map(|(canonical, _, _)| canonical),
    )?;
    // A td-OWNED source store (optional): the `<name>-source` path was interned by td
    // itself into SRC-STORE-DIR + SRC-DB, so realize stages it from there + reads its
    // closure from SRC-DB — no daemon interning. The on-disk tree is the canonical
    // basename under SRC-STORE-DIR (store-add-recursive restored it there).
    let src_override = src_store.map(|(store_dir, db)| {
        let base = source.rsplit('/').next().unwrap_or(&source);
        SrcOverride {
            canonical: source.clone(),
            on_disk: format!("{store_dir}/{base}"),
            db: db.to_string(),
        }
    });
    // A td-OWNED vendored-crate tree (optional, the guix-free crate path): td interned the
    // crate SET itself (store-add-recursive) into VENDOR-STORE-DIR + VENDOR-DB — a no-ref
    // content-addressed tree, staged + its closure read from there exactly like the source,
    // with NO daemon and NO `/gnu/store` crate path. run_rust vendors from it (TD_VENDOR_DIR).
    let vendor_override = vendor_store.map(|(canonical, store_dir, db)| {
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        SrcOverride {
            canonical: canonical.to_string(),
            on_disk: format!("{store_dir}/{base}"),
            db: db.to_string(),
        }
    });
    // Both no-ref td-interned trees go to realize_drv as src-overrides.
    let src_overrides: Vec<SrcOverride> =
        src_override.into_iter().chain(vendor_override).collect();
    // The reuse identity, derived from the CURRENT plan BEFORE any cache is read
    // (re #469 round-7): the typed staging manifest is assembled FIRST — the same
    // assembly realize_drv enforces at the bind boundary — so a reuse decision is
    // bound to the drv bytes, the exact typed input-authority set, and the builder.
    // A cache record can only confirm this identity, never substitute for it.
    let drv_bytes = std::fs::read(&drv_file).map_err(|e| e.to_string())?;
    let manifest_now =
        assemble_input_manifest(extra_dbs, &src_overrides, builder_override.as_ref())?;
    // Compute THIS drv's transitive input closure with the SAME shared routine realize
    // uses (stage_input_closure) BEFORE the cache read, so the reuse key is scoped to the
    // drv's real closure IDENTICALLY at read + write (any asymmetry reintroduces the miss
    // the scoping removes). Every input tree/db the scan reads is materialized: the
    // preceding plan steps built + committed their outputs before this step's cache check,
    // so the closure is available now. The plan-wide `manifest_now` union stays the
    // ENFORCEMENT input; only the reuse KEY is closure-scoped.
    let ic = stage_input_closure(
        &parsed,
        seed_store_dirs,
        seed_canonical_prefix,
        extra_dbs,
        &src_overrides,
        builder_override.as_ref(),
        td_store,
    )?;
    let expect = ReceiptExpect {
        drv_sha256: sha256_hex(&drv_bytes),
        // The reuse key folds the drv's DECLARED ABI builder identity (parsed.builder) and
        // EXCLUDES the builder's own content row (ic.real_builder_cb), NOT the resolved builder
        // ELF (ic.builder_exec, which drives enforcement) — an output-neutral builder recompile
        // must not move the key (see reuse_key_manifest_digest).
        manifest_sha256: reuse_key_manifest_digest(
            &ic.closure,
            &manifest_now,
            &parsed.builder,
            ic.real_builder_cb.as_deref(),
        ),
        builder: parsed.builder.clone(),
    };
    // Content-addressed build cache: if SCRATCH already holds a valid realization of
    // this exact (deterministic) drv, RECEIPT-verified against the identity above,
    // reuse it — skip the build. The gate points
    // SCRATCH at a persistent cache, so an unchanged recipe is a cache HIT and only a
    // CHANGED recipe (⇒ different drv hash ⇒ different output path, a miss) rebuilds.
    if let Some(regs) = cached_realization(&parsed, scratch, &expect)? {
        eprintln!(
            "td-builder: build-recipe CACHE HIT for {drv_path} — {} output(s) already realized + NAR-verified under {}; skipping the build",
            regs.len(),
            scratch.display()
        );
        for (o, r) in parsed.outputs.iter().zip(&regs) {
            println!("OUT={} {}", o.name, r.store_path);
        }
        // Re-write the td store-db even on a hit (deterministic from regs): a
        // downstream build-plan step reads this step's td.db to resolve the closure
        // of a td-built dependency, so it must exist whether or not we rebuilt.
        write_output_db(&regs, &scratch.join("td.db"))?;
        println!("CACHE=hit");
        return Ok(regs);
    }
    // PERSISTENT-STORE skip (opt-in, TD_PERSIST_STORE/TD_PERSIST_DB): an incremental
    // store that survives ACROSS invocations (the /td/store the loop builds into). If
    // this exact (deterministic) drv's output is already a valid path there — a PRIOR
    // invocation built it, its receipt sidecar matches the identity above, its rows
    // name THIS drv as deriver — and its tree re-verifies, read it back instead of
    // rebuilding. The daemon's valid-path skip, backed by an on-disk store across
    // process boundaries.
    if let Some((ps, pd)) = persist {
        if let Some(regs) =
            persistent_realization(&parsed, ps, Path::new(pd), scratch, &expect, &drv_path)?
        {
            eprintln!(
                "td-builder: build-recipe PERSISTENT-STORE HIT for {drv_path} — {} output(s) already valid under {ps}; skipping the build",
                regs.len()
            );
            for (o, r) in parsed.outputs.iter().zip(&regs) {
                println!("OUT={} {}", o.name, r.store_path);
            }
            // A fresh scratch reusing a prior build's output still needs the registration
            // + td.db a real build writes (downstream staging / a later store-commit).
            std::fs::write(scratch.join("registration"), registration_text(&regs))
                .map_err(|e| e.to_string())?;
            write_output_db(&regs, &scratch.join("td.db"))?;
            println!("CACHE=persist");
            return Ok(regs);
        }
    }
    // SUBSTITUTE-OR-BUILD is DELETED (re #469): the engine refuses the channel
    // outright rather than ignoring it. Substituted bytes come vouched by a remote
    // server's signature, not by the audited seed/recipe chain the staging manifest
    // certifies — and with strict manifests unconditional there is no step class
    // that could admit them, so the env set is a configuration error, never a
    // silent download. (The subst SERVER side — subst-export and its narinfo/nar
    // round-trip — is unaffected; only the engine's consumer hook is gone.)
    if std::env::var_os("TD_SUBST_URL").is_some() {
        return Err(
            "provenance rejected: TD_SUBST_URL is set — a substitute server is not an \
             admissible executable-input provenance (only audited seeds and prior td \
             recipe outputs are, re #469); unset TD_SUBST_URL to build from source"
                .to_string(),
        );
    }
    eprintln!("td-builder: build-recipe assembled {drv_path} (no guix (derivation), no Guile)");
    // td realizes it (no guix-daemon). With a td-owned source store, the source is
    // staged from td's own store + closure read from the td DB (no daemon interning);
    // with a td-owned builder, the drv's builder is staged from td's store + its
    // closure spans the builder DB ∪ the seed DB (no guix-built builder, brick 2);
    // td_store carries any td-BUILT deps (build-plan) for the multi-db closure's staging.
    let regs = realize_drv(
        &drv_file.to_string_lossy(),
        seed_store_dirs,
        seed_canonical_prefix,
        extra_dbs,
        scratch,
        &src_overrides,
        builder_override.as_ref(),
        td_store,
    )?;
    // PERSISTENT-STORE build-into: commit the freshly-built output(s) into the
    // incremental store so a LATER invocation reads them back (the skip above) —
    // build-into / read-back across builds, no daemon.
    if let Some((ps, pd)) = persist {
        commit_scratch_to_store(scratch, ps, Path::new(pd))?;
        eprintln!(
            "td-builder: build-recipe committed {} output(s) into the persistent store {ps}",
            regs.len()
        );
    }
    println!("CACHE=miss");
    Ok(regs)
}

/// Assemble a recipe's `.drv` with NO Guile and NO realize. Parses RECIPE-JSON, resolves
/// every input from LOCK (no specification->package), builds the drv spec (inputs as
/// input-SOURCES; BUILDER_PATH's `/bin/td-builder` is the drv's builder), assembles it
/// with `store::assemble_drv` (pure Rust, no guix (derivation …)), and writes it to
/// SCRATCH/<name>-<version>.drv — WITHOUT building it. Returns (canonical drv store path,
/// the written `.drv` file, the parsed derivation, the `<name>-source` path).
///
/// Substitute the /td/store gcc-toolchain `tc` for the lock's guix gcc-toolchain input(s) in
/// `inputs` (corpus-toolchain-default). A gcc-toolchain input is identified by its store-path
/// PACKAGE NAME (the part after the `<hash>-` store prefix) being `gcc-toolchain-…` — so
/// `<hash>-gcc-toolchain-15.2.0` matches but a bare `<hash>-gcc-14.3.0`, or an unrelated package
/// that merely embeds the segment interior (e.g. `<hash>-libfoo-gcc-toolchain-helper`), does NOT.
/// Only the toolchain input is swapped; every other build input + the order are untouched. Returns
/// true iff at least one input was substituted (callers no-op silently when none — see the override
/// site). A multi-match dedup is the caller's (`inputs.dedup()` after sort).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn substitute_gcc_toolchain(inputs: &mut [String], tc: &str) -> bool {
    let mut swapped = false;
    for p in inputs.iter_mut() {
        let base = p.rsplit('/').next().unwrap_or(p);
        // store basename = `<nix-base32 hash>-<package name>`; match the gcc-toolchain PACKAGE,
        // anchored at the name (split at the first `-`), not an interior substring.
        let is_toolchain =
            base.split_once('-').is_some_and(|(_hash, name)| name.starts_with("gcc-toolchain-"));
        if is_toolchain {
            *p = tc.to_string();
            swapped = true;
        }
    }
    swapped
}

// The nested `stage/td/store/<pkg>` prefix each `-self`/glibc recipe output carries.
// These embed the same pinned versions the rust-toolchain check's GCC_STAGE/GLIBC_STAGE
// consts do; the two must move together on a compiler/libc bump (re #547).
const NATIVE_GCC_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64-self";
const NATIVE_GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

/// The six native `/td/store` link strings a `rust` recipe bakes into its drv env
/// (TD_RUST_STORE_*), derived from the declared native inputs.
struct NativeRustLinkEnv {
    interp: String,
    rpath: String,
    bdir: String,
    cc: String,
    cxx: String,
    include: String,
}

/// Derive the native link env from a rust recipe's declared toolchain inputs, the
/// build-plan `--auto` counterpart to the `td shell` path where the rust-toolchain
/// check pre-sets TD_RUST_STORE_* in the environment. The three inputs' resolved
/// store paths are the same `{TD_STORE_DIR}/{base}` values that check bakes into
/// TD_SHELL_NATIVE_*, so the formulas here mirror `checks/rust_toolchain.rs` exactly
/// — a `td shell` and an `--auto` build of the same recipe get identical link env.
/// `None` when any of the three inputs is absent (a rust recipe that declares no
/// native toolchain — not linkable this way).
fn derive_native_rust_link_env(entries: &[lock::Entry]) -> Option<NativeRustLinkEnv> {
    let find = |n: &str| {
        entries
            .iter()
            .find(|e| e.name == n)
            .map(|e| e.path.as_str())
    };
    let gcc_root = find("gcc-x86-64-self")?;
    let binutils_root = find("binutils-x86-64-self")?;
    let glibc_root = find("glibc-x86-64")?;
    let gcc_path = format!("{gcc_root}/{NATIVE_GCC_STAGE}");
    let binutils_path = format!("{binutils_root}/bin");
    let glibc_path = format!("{glibc_root}/{NATIVE_GLIBC_STAGE}");
    Some(NativeRustLinkEnv {
        interp: format!("{glibc_path}/lib/ld-linux-x86-64.so.2"),
        rpath: format!("{glibc_path}/lib"),
        bdir: format!("{binutils_path}:{glibc_path}/lib"),
        cc: format!("{gcc_path}/bin/gcc"),
        cxx: format!("{gcc_path}/bin/g++"),
        include: format!("{glibc_path}/include"),
    })
}

/// Shared by `build-recipe` (which then realizes it daemon-free) and `assemble-recipe`
/// (assemble-only, so a SEPARATE process — the build daemon — realizes the td-assembled
/// drv). Splitting assembly from realization is what lets td's own daemon, not a `guix
/// repl`-emitted drv, be the build's input (own-builder-daemon §5).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn assemble_recipe_drv(
    recipe_json: &str,
    lock_file: &str,
    scratch: &Path,
    vendor_dir: Option<&str>,
) -> Result<(String, std::path::PathBuf, drv::Derivation, String), String> {
    let alist = json::parse(recipe_json).map_err(|e| format!("recipe JSON: {e}"))?;
    let name = alist.get("name").and_then(json::Json::as_str).ok_or("recipe: no name")?;
    let version = alist.get("version").and_then(json::Json::as_str).ok_or("recipe: no version")?;
    let full = format!("{name}-{version}");
    // The build system selects the td-builder phase runner. "gnu" (default) is the
    // autotools path; "rust" is the cargo path (build::run_rust), used to SELF-HOST
    // td-builder itself off Guile-construction + the daemon.
    let build_system = alist.get("buildSystem").and_then(json::Json::as_str).unwrap_or("gnu");
    let phase_runner = match build_system {
        "gnu" => "autotools-build",
        "rust" => "rust-build",
        // cmake: td's own cmake phase runner (build::run_cmake), the cmake-build-system
        // replacement — out-of-source cmake configure -> make -> make install in Rust.
        "cmake" => "cmake-build",
        // stage0: the seed executor (build::run_stage0, #378) — place the pinned
        // stage0-posix tree writable and exec its kaem interpreter; no build inputs.
        "stage0" => "stage0-build",
        // mesboot: the bootstrap-RUNG executor (build::run_mesboot, #378 slices
        // 2+3) — the recipe's typed steps run in the sandbox over staged inputs.
        "mesboot" => "mesboot-build",
        // rust-stage0: assemble the exact upstream rustc/rust-std/Cargo bootstrap
        // components and retarget them to td's declared runtime closure.
        "rust-stage0" => "rust-stage0-build",
        other => return Err(format!("recipe: unknown buildSystem `{other}' (known: gnu, rust, cmake, stage0, mesboot, rust-stage0)")),
    };
    // configure flags + phases (both optional) -> JSON array string. A configure
    // flag may itself contain whitespace (e.g. `CFLAGS=-O2 -g -Wno-foo`), so the
    // list is carried as JSON — each element stays ONE ./configure argument — the
    // same drv-safe encoding TD_PHASES uses. (Space-joining shattered such flags.)
    let cflags = match alist.get("configureFlags") {
        Some(c) => c.to_json_string(),
        None => String::new(),
    };
    let phases = match alist.get("phases") {
        Some(p) => p.to_json_string(),
        None => String::new(),
    };
    // Resolve EVERY input from the lock (no Guile), via the typed lock parser
    // (`NAME PATH [CLASS]`, backward-compatible with 2-field locks). The `source`
    // entry is TD_SRC; a `crate` entry is a vendored Rust dependency
    // (TD_VENDOR_CRATES); a `seed` or `td-recipe-output` entry is a build input
    // (TD_INPUTS). Each input is also an input-src. A `td-recipe-output` entry's
    // PATH is td's own dep build when build-plan substituted it, or the guix
    // oracle when this lock is consumed standalone — either way it is just an
    // input here.
    let lock_text =
        std::fs::read_to_string(lock_file).map_err(|e| format!("read lock {lock_file}: {e}"))?;
    let src_key = format!("{name}-source");
    let entries = lock::parse(&lock_text, &src_key)?;
    let mut source = String::new();
    let mut inputs: Vec<String> = Vec::new();
    // Vendored Rust deps: `crate`-class entries are the dependency closure (from
    // Cargo.lock), handed to the rust phase runner as TD_VENDOR_CRATES rather than
    // as toolchain inputs. A gnu recipe has none, so its spec is unchanged.
    let mut vendor: Vec<String> = Vec::new();
    for e in &entries {
        match e.class {
            lock::Class::Source => source = e.path.clone(),
            lock::Class::Crate => vendor.push(e.path.clone()),
            lock::Class::Seed | lock::Class::TdRecipeOutput => inputs.push(e.path.clone()),
        }
    }
    // Every build system needs its own source EXCEPT a mesboot recipe that EXPLICITLY
    // declares it has none (make-test, #429: it only RUNS a sibling rung's output) — its
    // recipe carries no `sourceInput`, so --auto synthesizes no `<name>-source` line, and
    // requiring one here would force back the removed nominal-source alias hack. Scoped to
    // BOTH conditions (build system AND the recipe's own declaration), not build_system
    // alone: a mesboot rung that DOES declare a `sourceInput` (every rung but make-test)
    // still hard-errors here if its lock ends up missing the source line — catching that
    // mistake immediately at drv-assembly time instead of a confusing failure deep in step
    // execution when a `{in:<name>-source}` template has nothing to resolve.
    let declares_no_source = build_system == "mesboot" && alist.get("sourceInput").is_none();
    if source.is_empty() && !declares_no_source {
        return Err(format!("lock has no `{src_key}' entry (the recipe source)"));
    }
    // Default corpus toolchain (corpus-toolchain-default): when TD_GCC_TOOLCHAIN names a /td/store
    // gcc-toolchain-shaped tree, SUBSTITUTE it for the lock's guix `gcc-toolchain-15.2.0` input — so the
    // corpus package is compiled by td's OWN /td/store toolchain (no guix gcc-toolchain bytes) instead of
    // guix's. The override path is staged as an input-src + reaches TD_INPUTS like any other input below;
    // its closure must be in the caller's store-dbs (the corpus gate interns the toolchain + threads its
    // db, exactly as the inline lock-rewrite did). Equivalent to rewriting the lock's gcc-toolchain line,
    // but done in the engine so it can be the DEFAULT for the corpus build path, not per-gate shell.
    // A no-swap (a lock with no gcc-toolchain — e.g. a pure-source package) is a SILENT no-op, NOT an
    // error: TD_GCC_TOOLCHAIN must be safe to set corpus-wide as the default. A package that wrongly
    // still pulls guix's toolchain is caught downstream by the gate's [no-guix-toolchain] assertion.
    if let Ok(tc) = std::env::var("TD_GCC_TOOLCHAIN") {
        if !tc.is_empty() {
            substitute_gcc_toolchain(&mut inputs, &tc);
        }
    }
    inputs.sort();
    // Dedup: the override collapses any (today single, but defensively >1) gcc-toolchain inputs to the
    // same path; the input-src loop + TD_INPUTS below must not carry it twice.
    inputs.dedup();
    vendor.sort();
    // The drv's builder identity is the STABLE ABI-token path, NOT the builder binary's
    // content-addressed store path — so the recipe's drv AND output hash are keyed on the
    // ABI revision, not the builder ELF (store::builder_identity_path). realize resolves
    // this to the real builder and binds its bytes here, so the sandbox execs the real
    // builder at this path; a builder-binary change no longer re-hashes every recipe.
    let builder_id = store::builder_identity_path();
    let builder = format!("{builder_id}/bin/td-builder");
    // Assemble the .drv spec: inputs as input-SOURCES (already-realized seed paths,
    // no input-derivations — so this diverges from guix's nano, by design).
    let mut spec = String::new();
    spec.push_str(&format!("name {full}\n"));
    spec.push_str("system x86_64-linux\n");
    spec.push_str(&format!("builder {builder}\n"));
    spec.push_str(&format!("arg {phase_runner}\n"));
    if !source.is_empty() {
        spec.push_str(&format!("input-src {source}\n"));
    }
    // The builder input-src is the SAME stable identity path (not the real builder Cb):
    // it makes the builder a closure ROOT so realize stages it, and it enters the hash as
    // the ABI token. realize substitutes the real builder for it when computing the
    // closure, then re-keys that entry back to this path so the real bytes bind here.
    spec.push_str(&format!("input-src {builder_id}\n"));
    for p in &inputs {
        spec.push_str(&format!("input-src {p}\n"));
    }
    // Vendored crates are also staged into the build (input-srcs); a gnu recipe has
    // none, so this adds nothing to its spec.
    for p in &vendor {
        spec.push_str(&format!("input-src {p}\n"));
    }
    // The td-OWNED vendored-crate TREE (guix-free crate path): one interned dir of
    // `*.crate`, staged as an input-src; run_rust vendors from it (TD_VENDOR_DIR set below).
    if let Some(vd) = vendor_dir {
        spec.push_str(&format!("input-src {vd}\n"));
    }
    if !source.is_empty() {
        spec.push_str(&format!("env TD_SRC={source}\n"));
    }
    spec.push_str(&format!("env TD_INPUTS={}\n", inputs.join(":")));
    match build_system {
        // gnu: the autotools phase runner reads the configure flags + custom phases.
        "gnu" => {
            spec.push_str(&format!("env TD_CONFIGURE_FLAGS={cflags}\n"));
            spec.push_str(&format!("env TD_PHASES={phases}\n"));
        }
        // cmake: the cmake phase runner reads the extra `cmake` flags (TD_CONFIGURE_FLAGS);
        // the autotools `substitute*` phase interpreter (TD_PHASES) does not apply here.
        "cmake" => {
            spec.push_str(&format!("env TD_CONFIGURE_FLAGS={cflags}\n"));
        }
        // stage0: sealed — source + builder are the WHOLE closure. Any other build
        // material (inputs, crates/vendor tree) or unrunnable field (configureFlags/
        // phases — run_stage0 reads only TD_SRC/out) is a hard error, never ignored.
        "stage0" => {
            if !inputs.is_empty() {
                return Err(format!(
                    "recipe: buildSystem \"stage0\" takes no build inputs (the seed needs nothing), but the lock carries {}",
                    inputs.join(" ")
                ));
            }
            if !vendor.is_empty() || vendor_dir.is_some() {
                return Err(
                    "recipe: buildSystem \"stage0\" takes no vendored crates — a crate-class lock entry or vendor tree would stage a store path into the sealed seed sandbox".into(),
                );
            }
            if alist.get("configureFlags").is_some() || alist.get("phases").is_some() {
                return Err(
                    "recipe: buildSystem \"stage0\" supports no configureFlags/phases — the seed runner would silently ignore them, so declaring them is an error".into(),
                );
            }
        }
        // mesboot: the bootstrap-rung step executor (#378 slices 2+3). The typed
        // steps ride as JSON; {in:NAME} templates resolve through TD_INPUT_MAP
        // (lock entry name -> canonical store path, source entry included).
        // configureFlags/phases have no runner here — hard error, never ignored.
        "mesboot" => {
            let steps = alist
                .get("steps")
                .ok_or("recipe: buildSystem \"mesboot\" requires `steps'")?;
            if alist.get("configureFlags").is_some() || alist.get("phases").is_some() {
                return Err(
                    "recipe: buildSystem \"mesboot\" supports no configureFlags/phases — rungs declare typed `steps'".into(),
                );
            }
            spec.push_str(&format!("env TD_STEPS={}\n", steps.to_json_string()));
            let map = json::Json::Obj(
                entries
                    .iter()
                    .map(|e| (e.name.clone(), json::Json::Str(e.path.clone())))
                    .collect(),
            );
            spec.push_str(&format!("env TD_INPUT_MAP={}\n", map.to_json_string()));
        }
        // rust-stage0: the exact upstream bootstrap-component ELF-retarget transform.
        // TD_SRC is the rustc component; rust-std/Cargo sources and the td runtime
        // closure resolve by NAME through TD_INPUT_MAP. No compilation occurs here.
        "rust-stage0" => {
            if alist.get("configureFlags").is_some()
                || alist.get("phases").is_some()
                || alist.get("bins").is_some()
                || alist.get("steps").is_some()
            {
                return Err(
                    "recipe: buildSystem \"rust-stage0\" supports no configureFlags/phases/bins/steps — it transforms exact rustc/rust-std/Cargo component tarballs against declared runtime inputs".into(),
                );
            }
            if !vendor.is_empty() || vendor_dir.is_some() {
                return Err(
                    "recipe: buildSystem \"rust-stage0\" takes no vendored crates — the transform extracts prebuilt bootstrap components, it does not compile".into(),
                );
            }
            let map = json::Json::Obj(
                entries
                    .iter()
                    .map(|e| (e.name.clone(), json::Json::Str(e.path.clone())))
                    .collect(),
            );
            spec.push_str(&format!("env TD_INPUT_MAP={}\n", map.to_json_string()));
        }
        // rust: the cargo phase runner installs the named binaries (TD_RUST_BINS) and,
        // if any vendored deps were locked, resolves them offline (TD_VENDOR_CRATES).
        "rust" => {
            let bins: Vec<&str> = alist
                .get("bins")
                .and_then(json::Json::as_arr)
                .map(|a| a.iter().filter_map(json::Json::as_str).collect())
                .unwrap_or_default();
            if bins.is_empty() {
                return Err("recipe: buildSystem \"rust\" requires a non-empty `bins'".into());
            }
            spec.push_str(&format!("env TD_RUST_BINS={}\n", bins.join(" ")));
            if !vendor.is_empty() {
                spec.push_str(&format!("env TD_VENDOR_CRATES={}\n", vendor.join(":")));
            }
            // td's OWN guix-free crate set: one interned dir of `*.crate` (run_rust reads
            // every crate from it). No `/gnu/store` crate path, no guix-daemon FOD.
            if let Some(vd) = vendor_dir {
                spec.push_str(&format!("env TD_VENDOR_DIR={vd}\n"));
            }
            // Native /td/store toolchain link mode (#258): the sandbox clears the env, so run_rust
            // only sees the drv's `env` lines — bake the six TD_RUST_STORE_* into the drv (mirroring
            // the TD_GCC_TOOLCHAIN input override above). When they are set the build links against the
            // native /td/store gcc (a PLAIN gcc, no ld-wrapper): run_rust bakes the interp/RUNPATH/-B
            // explicitly so the built binary resolves its libc/libgcc_s from /td/store at run time. The
            // values are fixed /td/store paths, so the drv (and its double-build `check`) stay
            // deterministic. Two sources, env-first: `td shell` pre-sets them in the environment
            // (provision_rust_inputs); `build-plan --auto` clears the env, so DERIVE the identical
            // values from the recipe's declared native inputs (re #547). Neither ⇒ no env lines ⇒ the
            // legacy ld-wrapper path, unchanged.
            let derived = derive_native_rust_link_env(&entries);
            for (k, d) in [
                ("TD_RUST_STORE_INTERP", derived.as_ref().map(|d| &d.interp)),
                ("TD_RUST_STORE_RPATH", derived.as_ref().map(|d| &d.rpath)),
                ("TD_RUST_STORE_BDIR", derived.as_ref().map(|d| &d.bdir)),
                ("TD_RUST_STORE_CC", derived.as_ref().map(|d| &d.cc)),
                ("TD_RUST_STORE_CXX", derived.as_ref().map(|d| &d.cxx)),
                ("TD_RUST_STORE_INCLUDE", derived.as_ref().map(|d| &d.include)),
            ] {
                let v = match std::env::var(k) {
                    Ok(v) if !v.is_empty() => Some(v),
                    _ => d.cloned(),
                };
                if let Some(v) = v {
                    if !v.is_empty() {
                        spec.push_str(&format!("env {k}={v}\n"));
                    }
                }
            }
            // Optional cargo feature selection (both default-absent ⇒ a plain
            // `cargo build` with the crate's defaults, unchanged). `noDefaultFeatures`
            // drops the crate's default features — e.g. fd's `use-jemalloc`, whose
            // jemalloc-sys runs a C ./configure the scrubbed build-env can't satisfy;
            // `features` adds back the wanted ones (e.g. "completions").
            if alist.get("noDefaultFeatures").is_some_and(json::Json::is_true) {
                spec.push_str("env TD_CARGO_NO_DEFAULT=1\n");
            }
            if let Some(feats) = alist.get("features").and_then(json::Json::as_arr) {
                let fl: Vec<&str> = feats.iter().filter_map(json::Json::as_str).collect();
                if !fl.is_empty() {
                    spec.push_str(&format!("env TD_CARGO_FEATURES={}\n", fl.join(",")));
                }
            }
        }
        _ => unreachable!("buildSystem already validated"),
    }
    // td assembles the .drv (pure Rust, no guix (derivation …), no daemon).
    let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
    let (drv_path, content) = store::assemble_drv(&spec, &read)?;
    let parsed = drv::parse(content.as_bytes()).map_err(|e| format!("parse assembled drv: {e}"))?;
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let drv_file = scratch.join(format!("{full}.drv"));
    std::fs::write(&drv_file, &content).map_err(|e| e.to_string())?;
    Ok((drv_path, drv_file, parsed, source))
}

/// The optional td-OWNED stage0 builder override from TD_BUILDER_PATH/STORE/DB — all
/// three set together (a `store-add-builder` placement) → the drv's builder is that
/// td-placed stage0, staged from its own store + db; none set → the running binary
/// (self_store_path). Any partial set is a loud error. Returns owned strings; borrow
/// them into the `(&str, &str, &str)` build_recipe/build_plan expects at the call site.
fn builder_store_env() -> Result<Option<(String, String, String)>, String> {
    match (
        std::env::var("TD_BUILDER_PATH").ok(),
        std::env::var("TD_BUILDER_STORE").ok(),
        std::env::var("TD_BUILDER_DB").ok(),
    ) {
        (Some(p), Some(s), Some(d)) => Ok(Some((p, s, d))),
        (None, None, None) => Ok(None),
        _ => Err("TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together".into()),
    }
}

/// The OPTIONAL persistent store the build-plan chain reads-back-or-commits into
/// (re #469 build speed): TD_PERSIST_STORE + TD_PERSIST_DB, set together, name a
/// store+db that survive ACROSS invocations. With them set, each build_recipe step
/// reuses a prior run's output for an UNCHANGED rung (a persistent_realization HIT,
/// receipt+NAR-verified) and commits a freshly-built rung's output back — so a
/// CHANGED rung (different drv ⇒ different output path) still rebuilds. Unset →
/// None → build-plan owns its own in-run td-store and rebuilds the whole chain (the
/// clean-room default). Mirrors the `store-build` subcommand's persist convention.
fn persist_store_env() -> Result<Option<(String, String)>, String> {
    let ps = std::env::var("TD_PERSIST_STORE").ok().filter(|s| !s.is_empty());
    let pd = std::env::var("TD_PERSIST_DB").ok().filter(|s| !s.is_empty());
    match (ps, pd) {
        (Some(s), Some(d)) => Ok(Some((s, d))),
        (None, None) => Ok(None),
        _ => Err("TD_PERSIST_STORE/TD_PERSIST_DB must be set together".into()),
    }
}

/// A recipe's committed `cargoLock` path must be repo-relative — no absolute prefix,
/// no `..` component, and (once joined and canonicalized) landing INSIDE the repo root
/// so an in-repo symlink cannot redirect it either. The path names a reviewed in-repo
/// lock; confine it so a recipe cannot aim the gate at an arbitrary host file
/// (re #469/#547).
fn confine_repo_relative(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let relp = Path::new(rel);
    if relp.is_absolute() {
        return Err(format!("cargoLock `{rel}' must be repo-relative, not absolute"));
    }
    if relp
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("cargoLock `{rel}' must not escape the repo with `..'"));
    }
    let joined = root.join(relp);
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize repo root {}: {e}", root.display()))?;
    let canon = joined
        .canonicalize()
        .map_err(|e| format!("canonicalize cargoLock {}: {e}", joined.display()))?;
    if !canon.starts_with(&canon_root) {
        return Err(format!(
            "cargoLock `{rel}' resolves to {} outside the repo root {}",
            canon.display(),
            canon_root.display()
        ));
    }
    // Return the symlink-resolved path so the caller reads the validated node — not the
    // pre-canonicalize `joined`, which a swapped symlink could redirect after the check.
    Ok(canon)
}

/// `remove_dir_all` that tolerates an absent dir but propagates every other error: a
/// tree that MUST start empty for a security guarantee (the interned vendor staging)
/// cannot silently inherit leftovers from a clean that quietly failed.
fn clear_dir(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("clear {}: {e}", dir.display())),
    }
}

/// Stage the crates a recipe's COMMITTED Cargo.lock pins into a FRESH private tree,
/// verifying as we go: every lock entry must be present in `vendor_dir` as
/// `<name>-<version>.crate` with a matching sha256, the dir must carry NO extra
/// `.crate` beyond that pinned set, and only the verified bytes are copied into
/// `staged_dir`. Interning `staged_dir` (never the shared cache dir) means a stale or
/// tampered `.td-build-cache` can neither drop, swap, smuggle, NOR win a
/// verify-to-intern race for a crate the gate did not vouch for (re #469/#547).
/// Returns the verified crate count.
fn stage_verified_vendor(
    vendor_dir: &Path,
    lock_text: &str,
    staged_dir: &Path,
) -> Result<usize, String> {
    let want = check_loop::parse_lock_checksums(lock_text);
    if want.is_empty() {
        return Err("committed Cargo.lock pins no checksummed crates".to_string());
    }
    if !vendor_dir.is_dir() {
        return Err(format!(
            "vendored crate dir {} absent — `td-builder check' warms the crate closure before the graph build",
            vendor_dir.display()
        ));
    }
    let mut expected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (crate_name, ver, _sum) in &want {
        let fname = format!("{crate_name}-{ver}.crate");
        // The name/version come from the lock text; a `..`/separator in either would let
        // `<dir>.join(fname)` below escape the vendor/staged trees. Admit only a plain
        // filename (its own `file_name`), never a path with directory components.
        if Path::new(&fname).file_name() != Some(std::ffi::OsStr::new(fname.as_str())) {
            return Err(format!(
                "committed-lock crate `{crate_name}-{ver}' is not a plain filename — refusing a path-bearing crate identity"
            ));
        }
        expected.insert(fname);
    }
    // Reject extras up front: a `.crate` the committed lock never pinned is a stale or
    // tampered cache and must fail loudly rather than be silently left behind.
    for entry in std::fs::read_dir(vendor_dir)
        .map_err(|e| format!("read vendor dir {}: {e}", vendor_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("read vendor dir {}: {e}", vendor_dir.display()))?;
        let file_name = entry.file_name();
        let fname = file_name.to_string_lossy();
        if fname.ends_with(".crate") && !expected.contains(fname.as_ref()) {
            return Err(format!(
                "vendored crate `{fname}' is not pinned by the committed Cargo.lock — the vendor dir carries a crate the gate did not verify"
            ));
        }
    }
    // Fail-closed clean: the staged tree is interned wholesale, so it must start empty —
    // a silently-ignored clean failure could leave an unverified leftover to be interned.
    clear_dir(staged_dir)?;
    std::fs::create_dir_all(staged_dir)
        .map_err(|e| format!("create staged vendor dir {}: {e}", staged_dir.display()))?;
    for (crate_name, ver, sum) in &want {
        let fname = format!("{crate_name}-{ver}.crate");
        let src = vendor_dir.join(&fname);
        let dest = staged_dir.join(&fname);
        // Copy first, then hash the STAGED copy — the exact bytes that get interned — so
        // a concurrent rewrite of the cache file between hash and copy cannot slip
        // unverified bytes past the gate (verify what you use, not what you read).
        std::fs::copy(&src, &dest)
            .map_err(|e| format!("stage vendored crate {}: {e}", src.display()))?;
        let got = crate::sha256::sha256_file(&dest)
            .map_err(|e| format!("read staged crate {}: {e}", dest.display()))?;
        if &got != sum {
            return Err(format!(
                "vendored crate {crate_name}-{ver} sha256 {got} != committed-lock {sum}"
            ));
        }
    }
    Ok(want.len())
}

/// A committed `Cargo.lock` must be fully checksum-pinned before it is trusted as the
/// closure record: reject any package with a `git+` source (git deps are unsupported)
/// or a registry source lacking a valid sha256 checksum. Workspace/path members (no
/// `source`) legitimately carry no checksum. stage_verified_vendor gates only the
/// CHECKSUMMED set, so without this a git or unchecksummed dependency would be silently
/// omitted while the gate reported success (re #547).
fn reject_unpinned_dependencies(lock_text: &str) -> Result<(), String> {
    let (mut name, mut source, mut checksum) = (String::new(), None, None);
    let mut in_pkg = false;
    for line in lock_text.lines() {
        let t = line.trim();
        if t == "[[package]]" {
            if in_pkg {
                check_pkg_pinned(&name, &source, &checksum)?;
            }
            in_pkg = true;
            name.clear();
            source = None;
            checksum = None;
        } else if t.starts_with('[') {
            // any other table (e.g. a trailing [metadata]) ends the package section
            if in_pkg {
                check_pkg_pinned(&name, &source, &checksum)?;
                in_pkg = false;
            }
        } else if in_pkg {
            // Split on the first `=` and trim both sides so non-canonical spacing
            // (e.g. `source="git+…"`) cannot hide a field from the pin check.
            if let Some((k, v)) = t.split_once('=') {
                let val = v.trim().trim_matches('"').to_string();
                match k.trim() {
                    "name" => name = val,
                    "source" => source = Some(val),
                    "checksum" => checksum = Some(val),
                    _ => {}
                }
            }
        }
    }
    if in_pkg {
        check_pkg_pinned(&name, &source, &checksum)?;
    }
    Ok(())
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_pkg_pinned(
    name: &str,
    source: &Option<String>,
    checksum: &Option<String>,
) -> Result<(), String> {
    let Some(src) = source else {
        return Ok(()); // no source ⇒ workspace/path member ⇒ no checksum expected
    };
    if src.starts_with("git+") {
        return Err(format!(
            "committed lock package `{name}' is a git dependency (`{src}') — git deps are unsupported"
        ));
    }
    match checksum {
        Some(c) if is_sha256_hex(c) => Ok(()),
        _ => Err(format!(
            "committed lock package `{name}' has registry source `{src}' but no valid sha256 checksum"
        )),
    }
}

/// build-plan `--auto` crate vendoring: a `rust` recipe's dependency closure rides the
/// same typed content-addressed vendor channel `td shell` uses (interned tree ->
/// TD_VENDOR_DIR), NOT a lock-line ingress — so the #469 crate-class reject in
/// `build_plan` stays intact. The crates are the ones `td-feed warm crate` fetched into
/// `.td-build-cache/crate-vendor/<name>/vendor`; they are staged into a private tree and
/// verified against the recipe's committed, fully-checksum-pinned `Cargo.lock` here
/// (set-equality: every pinned crate present with a matching sha256 and no extra), then
/// that tree is interned. The build itself is anchored to the AUTHENTICATED source: cargo
/// `--frozen` unpacks the content-addressed sourceInput and enforces its embedded
/// Cargo.lock against each vendored crate's checksum. The warm vendor set is fetched by
/// the committed lock's checksums, and cargo `--frozen` then enforces equality with the
/// source's embedded lock — so the closure is authenticated downstream, without trusting
/// the mutable warm cache. (Byte-anchoring the committed lock to
/// the in-sandbox unpacked source lock — staged into the derivation and compared in
/// run_rust before cargo — is the stronger follow-up form; reading the mutable
/// `.td-build-cache` src lock here would only give false assurance.) Returns the
/// `(canonical, store_dir, db)` triple `build_recipe` wants, or `None` for a non-rust
/// recipe or a rust recipe with no `cargoLock`. A declared `cargoLock` makes the
/// `TD_AUTO_REPO_ROOT` anchor MANDATORY: a plan that cannot resolve and gate the
/// committed closure fails closed rather than building the node ungated.
fn provision_auto_vendor(
    alist: &json::Json,
    name: &str,
    step_scratch: &Path,
) -> Result<Option<(String, String, String)>, String> {
    if alist.get("buildSystem").and_then(json::Json::as_str) != Some("rust") {
        return Ok(None);
    }
    let Some(lock_rel) = alist.get("cargoLock").and_then(json::Json::as_str) else {
        return Ok(None);
    };
    // The recipe name is interpolated into the cache path and the store-add label below;
    // admit only a plain single component so it cannot traverse out of the cache tree.
    if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
        return Err(format!(
            "--auto rust `{name}': recipe name is not a plain path component"
        ));
    }
    let repo_root = match std::env::var("TD_AUTO_REPO_ROOT") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            return Err(format!(
                "--auto rust `{name}': declares cargoLock `{lock_rel}' but TD_AUTO_REPO_ROOT is unset — cannot resolve or gate the committed crate closure"
            ))
        }
    };
    let root = Path::new(&repo_root);
    let committed =
        confine_repo_relative(root, lock_rel).map_err(|e| format!("--auto rust `{name}': {e}"))?;
    let lock_text = std::fs::read_to_string(&committed).map_err(|e| {
        format!("--auto rust `{name}': read committed lock {}: {e}", committed.display())
    })?;
    // Trust the committed lock as the closure record only if it is fully checksum-pinned —
    // no git/unchecksummed deps that the set-equality gate below would silently skip.
    reject_unpinned_dependencies(&lock_text).map_err(|e| format!("--auto rust `{name}': {e}"))?;
    let vendor_dir = root.join(format!(".td-build-cache/crate-vendor/{name}/vendor"));
    let staged = step_scratch.join("vendor-verified");
    let n = stage_verified_vendor(&vendor_dir, &lock_text, &staged)
        .map_err(|e| format!("--auto rust `{name}': {e}"))?;
    let self_exe = std::env::current_exe()
        .map_err(|e| format!("--auto rust `{name}': locate td-builder: {e}"))?
        .to_string_lossy()
        .into_owned();
    let vendor_store = step_scratch.join("vendorstore");
    let vendor_db = step_scratch.join("vendor.db");
    clear_dir(&vendor_store)?;
    match std::fs::remove_file(&vendor_db) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("clear {}: {e}", vendor_db.display())),
    }
    let vendor_canonical = run_store_add(
        &self_exe,
        &format!("{name}-vendor"),
        &staged,
        &vendor_store,
        &vendor_db,
    )?;
    eprintln!(
        "td-builder: build-plan step `{name}': interned {n} committed-lock-verified crate(s) from {} -> {vendor_canonical}",
        vendor_dir.display()
    );
    Ok(Some((
        vendor_canonical,
        vendor_store.to_string_lossy().into_owned(),
        vendor_db.to_string_lossy().into_owned(),
    )))
}

/// build-plan: realize a TOPO-ordered chain of recipes where a downstream step
/// consumes an UPSTREAM step's td-BUILT output instead of a guix store path. This
/// is the edge the per-package locks could not express: `recipe-checks` builds
/// grep's own derivation Guile-free but still links GUIX's pcre2; here grep links
/// the pcre2 td just built.
///
/// Reached ONLY through `build-plan --auto` (the raw-plan CLI arm is deleted,
/// re #469 — a hand-written plan/lock was an untyped host-path ingress channel).
/// PLAN is line-based — `step RECIPE-JSON LOCK` per step, in dependency order. For
/// each step every lock entry is CLASS-GATED: a `td-recipe-output` is SUBSTITUTED
/// with the matching earlier step's output (matched by NAME == the producing
/// recipe's `name`), a `seed`/`source` must pass `auto_seed_provenance` against
/// the plan's seed store, and anything else is rejected. The recipe is built with
/// `build_recipe` under STRICT PROVENANCE: the closure spans SEED-DB (the interned
/// seeds' registrations) ∪ every prior step's `td.db`, and every staged item must
/// be vouched for by one of those dbs and NAR-hash-match it at the sandbox staging
/// boundary. The output of each step is copied into the shared TD-STORE and its
/// store path recorded for downstream steps.
fn build_plan(
    plan_file: &str,
    guix_store: &str,
    seed_db: &str,
    scratch: &Path,
    builder_store: Option<(&str, &str, &str)>,
    persist: Option<(&str, &str)>,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    let plan = std::fs::read_to_string(plan_file)
        .map_err(|e| format!("read plan {plan_file}: {e}"))?;
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    // The shared td-store: each step copies its output here, and a downstream step
    // stages a td-built dep FROM here — realize_drv re-keys a closure entry whose tree
    // lives under <tdstore>/<base> to `canonical\ton-disk`, so the sandbox binds it from
    // td's store (split_closure_entry) instead of the daemon's /gnu/store. The on-disk
    // half rides through closure.txt, so a later `td-builder check` needs no extra state.
    let tdstore = scratch.join("tdstore");
    std::fs::create_dir_all(&tdstore).map_err(|e| e.to_string())?;

    // recipe name -> its (single) output store path; and each step's td.db, fed
    // into the closure of later steps so a td-built dep resolves. The db set is
    // SEEDED with the plan's seed db (the registrations `store-add-recursive`
    // wrote when the seeds were interned), typed `AuditedSeed`; each step's
    // td.db joins typed `RecipeOutput` — under strict provenance every staged
    // item must be vouched for by one of these TYPED dbs, so the seed STORE
    // directory contributes bytes, never authority (re #469).
    // The seed db is AUTHENTICATED before it carries any (round-8): SEED-DB is
    // a caller path (and may pre-exist warm across runs), so every row must
    // content-address to itself AND land on a basename the compiled seed-digest
    // table pins — a store-register'd row over foreign bytes, or a CA-valid
    // item the pins never derived, cannot be typed `AuditedSeed`.
    authenticate_seed_db(seed_db, Path::new(guix_store))?;
    let mut built: BTreeMap<String, String> = BTreeMap::new();
    let mut td_dbs: Vec<(String, sandbox::InputOrigin)> =
        vec![(seed_db.to_string(), sandbox::InputOrigin::AuditedSeed)];
    // The DERIVED blessed seed-closure db (re #469 round-8): vouches the
    // control-plane builder's host-seed runtime closure (glibc/gcc-lib — the
    // §5 toolchain td-builder itself links against until it self-hosts).
    // Derived, never an argument; absent means that authority is simply not
    // there and staging reds on any closure item that needed it.
    if let Some(bless) = derived_bless_db_auto()? {
        td_dbs.push((bless, sandbox::InputOrigin::BlessedSeedClosure));
    }
    let store_prefix = store::store_dir();

    for raw in plan.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        let (recipe_json, lock_file) = match toks.as_slice() {
            ["step", r, l] => (*r, *l),
            _ => return Err(format!("malformed plan line (want `step RECIPE-JSON LOCK'): {line}")),
        };
        let recipe_text = std::fs::read_to_string(recipe_json)
            .map_err(|e| format!("read recipe {recipe_json}: {e}"))?;
        let alist = json::parse(&recipe_text).map_err(|e| format!("recipe JSON {recipe_json}: {e}"))?;
        let name = alist
            .get("name")
            .and_then(json::Json::as_str)
            .ok_or_else(|| format!("recipe {recipe_json}: no name"))?;
        let step_scratch = scratch.join(name);
        std::fs::create_dir_all(&step_scratch).map_err(|e| e.to_string())?;

        // Substitute td-recipe-output entries with the producing step's output.
        let src_key = format!("{name}-source");
        let lock_text = std::fs::read_to_string(lock_file)
            .map_err(|e| format!("read lock {lock_file}: {e}"))?;
        let entries = lock::parse(&lock_text, &src_key)?;
        let mut resolved = String::new();
        let mut substituted: Vec<String> = Vec::new();
        for e in &entries {
            // Class-typed provenance gate (re #469): a td-recipe-output MUST be an
            // earlier step's build; a seed/source MUST be a canonical store item
            // interned in the plan's seed store; nothing else is admissible. This
            // holds for ANY lock this function is handed, not only the ones
            // auto_synthesize_lock produced — the plan file is not an ingress
            // channel that can type arbitrary host paths as seeds.
            let path = match e.class {
                lock::Class::TdRecipeOutput => {
                    let p = built.get(&e.name).ok_or_else(|| {
                        format!("step `{name}': lock entry `{}' is td-recipe-output but no earlier step built it (plan out of topo order?)", e.name)
                    })?;
                    substituted.push(format!("{}={}", e.name, p));
                    p.clone()
                }
                lock::Class::Seed | lock::Class::Source => {
                    // Digest-gate by the SOURCE KEY, not the lock entry name (see
                    // seed_gate_key): the recipe's own source entry is named
                    // `{name}-source` but pinned under its `sourceInput` key, which may
                    // be a shared seed.
                    let gate_key = seed_gate_key(&e.name, &src_key, &alist);
                    auto_seed_provenance(
                        &store_prefix,
                        Path::new(guix_store),
                        name,
                        gate_key,
                        &e.path,
                    )?;
                    e.path.clone()
                }
                lock::Class::Crate => {
                    return Err(format!(
                        "step `{name}': provenance rejected: lock entry `{}' is a vendored crate — a bootstrap plan admits only interned seeds and prior recipe outputs (re #469)",
                        e.name
                    ));
                }
            };
            // Re-emit 2-field; build_recipe re-infers the class. A substituted td
            // path infers `seed` → an input-src, exactly the intent (it IS now a
            // realized input — just td's, not guix's).
            resolved.push_str(&format!("{} {}\n", e.name, path));
        }
        let resolved_lock = step_scratch.join("resolved.lock");
        std::fs::write(&resolved_lock, &resolved).map_err(|e| e.to_string())?;
        if substituted.is_empty() {
            eprintln!("td-builder: build-plan step `{name}': no td-built deps to substitute");
        } else {
            eprintln!("td-builder: build-plan step `{name}': substituted td outputs -> {}", substituted.join(" "));
        }

        // A `rust` step's crate closure is interned from its committed-lock-verified warm
        // vendor tree and handed to build_recipe as the typed vendor tree (TD_VENDOR_DIR) —
        // NOT a lock line, so the crate-class reject above stays intact (re #469/#547).
        // None for every other step ⇒ the vendor channel is unchanged for the gnu corpus.
        let vendor_owned = provision_auto_vendor(&alist, name, &step_scratch)?;
        let vendor_arg = vendor_owned
            .as_ref()
            .map(|(c, s, d)| (c.as_str(), s.as_str(), d.as_str()));

        // Closure content-scans guix's seed store (seeds) + reads every prior step's td.db
        // (td deps, whose bytes live in the shared td-store, outside the seed dir).
        let seed_dirs = [guix_store.to_string()];
        let regs = build_recipe(
            &recipe_text,
            &resolved_lock.to_string_lossy(),
            &step_scratch,
            &seed_dirs,
            store::STORE_DIR, // the guix seed store's canonical home
            &td_dbs,
            None,            // src_store: build-plan locks carry resolved paths
            vendor_arg,      // vendor_store: a rust step's committed-lock-verified crate tree, else None
            builder_store,   // builder_store: the td-placed stage0 (TD_BUILDER_*), or None → self
            Some(&tdstore),  // td_store: stage td-built deps from the shared td-store
            persist,         // persist: reuse-or-commit each rung across runs (re #469), or None → clean-room in-run store
        )?;
        // Single-output recipes (the gnu corpus): the dep is regs[0].
        let out = regs
            .first()
            .ok_or_else(|| format!("step `{name}': build produced no output"))?;
        let base = out
            .store_path
            .rsplit('/')
            .next()
            .ok_or_else(|| format!("step `{name}': output is not a store path"))?;
        // Atomically stage the step's output into the per-invocation td-store so a
        // downstream step can bind-mount it (a real dir, no symlink). This store has no
        // persistent db, so any already-present mismatch is a real in-invocation conflict:
        // fail closed (registered = true), never a torn-orphan recovery.
        let physical = step_scratch.join("newstore").join(base);
        let dest = tdstore.join(base);
        commit_tree_checked(&physical, &dest, &out.nar_hash, true)?;
        built.insert(name.to_string(), out.store_path.clone());
        td_dbs.push((
            step_scratch.join("td.db").to_string_lossy().into_owned(),
            sandbox::InputOrigin::RecipeOutput,
        ));
        println!("STEP {name} {}", out.store_path);
        eprintln!(
            "td-builder: build-plan step `{name}': out {} (staged into td-store {})",
            out.store_path,
            tdstore.display()
        );
    }
    eprintln!("td-builder: build-plan complete — {} step(s)", built.len());
    Ok(())
}

/// A recipe's declared inputs — the JSON `inputs` array UNION `nativeInputs`
/// (#378 staged builders: a rung's compiler is a prior rung's output; --auto
/// chains both edge kinds identically).
/// The compiled-seed-digest key a build-plan lock entry is gated by. A recipe's
/// OWN source entry is named `{name}-source` (`src_key`) so its steps can reference
/// `{in:{name}-source}` uniformly, but its digest is pinned under the recipe's
/// `sourceInput` KEY — which may be a shared seed the rung renames locally
/// (gcc-mesboot0-source <- patch-gcc-boot-2.95.3, mesboot-headers-source <-
/// linux-headers). Resolve that source entry to its pin key; gate every other seed
/// by its own name. For a conventional rung (sourceInput == `{name}-source`) the two
/// coincide, so this is a no-op. `alist` is the entry's own recipe JSON.
fn seed_gate_key<'a>(entry_name: &'a str, src_key: &str, alist: &'a json::Json) -> &'a str {
    if entry_name == src_key {
        alist
            .get("sourceInput")
            .and_then(json::Json::as_str)
            .unwrap_or(entry_name)
    } else {
        entry_name
    }
}

fn inputs_from_recipe_json(alist: &json::Json) -> Vec<String> {
    let mut xs: Vec<String> = Vec::new();
    for key in ["inputs", "nativeInputs"] {
        if let Some(a) = alist.get(key).and_then(json::Json::as_arr) {
            xs.extend(a.iter().filter_map(json::Json::as_str).map(str::to_string));
        }
    }
    xs
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_inputs(recipe_dir: &str, name: &str) -> Result<Vec<String>, String> {
    let p = format!("{recipe_dir}/{name}.json");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("read recipe {p}: {e}"))?;
    let alist = json::parse(&text).map_err(|e| format!("recipe JSON {p}: {e}"))?;
    Ok(inputs_from_recipe_json(&alist))
}

/// An input is OWNED (td reconstructs it) iff its recipe JSON exists in RECIPE-DIR;
/// otherwise it is an external seed/tool (the toolchain, retired last) resolved
/// through the --auto MAP instead (#429 — no per-rung hand-written base lock).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_is_owned(recipe_dir: &str, name: &str) -> bool {
    Path::new(&format!("{recipe_dir}/{name}.json")).exists()
}

/// Post-order DFS over the OWNED-input subgraph: appends each recipe AFTER its owned
/// deps → a topo order (deps first). Cycles error.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_topo(
    recipe_dir: &str,
    name: &str,
    order: &mut Vec<String>,
    seen: &mut std::collections::BTreeSet<String>,
    stack: &mut Vec<String>,
) -> Result<(), String> {
    if seen.contains(name) {
        return Ok(());
    }
    if stack.iter().any(|s| s == name) {
        return Err(format!("--auto: dependency cycle through `{name}'"));
    }
    stack.push(name.to_string());
    for inp in auto_inputs(recipe_dir, name)? {
        if auto_is_owned(recipe_dir, &inp) {
            auto_topo(recipe_dir, &inp, order, seen, stack)?;
        }
    }
    stack.pop();
    seen.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

/// Parse a --auto MAP file: `NAME PATH` per line (blank/`#`-comment lines skipped) —
/// the pinned-source resolution `ladder_setup` interns (the fresh per-run auto-map;
/// the host-tool `tools.map` half is deleted, re #469). The FIRST occurrence of a
/// name wins, so a duplicated name keeps its earliest entry.
fn auto_parse_map(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((n, p)) = line.split_once(' ') {
            m.entry(n.trim().to_string()).or_insert_with(|| p.trim().to_string());
        }
    }
    m
}

/// Resolve NAME to a real store path through the --auto MAP — every declared input
/// that is NOT itself an owned recipe (a host tool, a pinned seed/source tarball)
/// must be in MAP or synthesis fails loudly: a recipe declaring an input nothing
/// interned is a bug to surface, not an edge to silently drop.
fn auto_map_lookup(map: &std::collections::BTreeMap<String, String>, name: &str) -> Result<String, String> {
    map.get(name).cloned().ok_or_else(|| {
        format!("no map entry for `{name}' (not an owned recipe, not interned by ladder_setup)")
    })
}

/// Enforce the #469 provenance boundary on a MAP-resolved `seed`/`source` lock entry
/// AT SYNTHESIS (the plan's planning step, before anything is staged or executed):
/// the path must be a canonical top-level item of the active store prefix, the item
/// must already be interned in the plan's seed store (SEED-STORE/<basename> exists —
/// the store td-recipe-eval's classified planning pass populated), AND the item must
/// be SELF-AUTHENTICATING: seeds are interned content-addressed
/// (`make_store_path("source", sha256(NAR), name)`), so the on-disk bytes are
/// re-hashed here and the path is recomputed from them — a name whose digest its own
/// bytes cannot reproduce reds. AND the key→basename binding must match the
/// COMPILED seed-digest table (`seed_digests`, the same repo file
/// td-recipe-eval compiles in): the table pins which basename each seed key
/// may resolve to, so a forged MAP/SEED-STORE/SEED-DB triple — internally
/// consistent but never derived from the pins — reds here even when
/// td-builder is invoked directly with caller-supplied files. Origin
/// authority is compiled in; the content-address recomputation then proves
/// the on-disk bytes ARE the pinned bytes. (Cost: one NAR hash per seed
/// entry per synthesis — the same recorded re-hash-every-step decision as
/// `StageManifest`.) A bare host path (`/usr/bin/env`), a foreign store path
/// (`/gnu/store/…`), or a never-interned name all red here too — the MAP file is
/// NOT a channel that can type arbitrary host paths as seeds.
/// The COMPILED seed-digest table (re #469): `seed/seed-digests.txt`, the
/// same audited repo file td-recipe-eval compiles in — key → the store
/// basename its pinned bytes derive. Compiled into the binary so the
/// authority travels with td-builder itself; a caller-supplied map/store/db
/// cannot substitute for it. Malformed rows are a hard error (trust anchor,
/// never best-effort).
const SEED_DIGESTS: &str = include_str!("../../seed/seed-digests.txt");

/// The compiled expected basename for a seed key, if pinned.
fn seed_digests_expected(key: &str) -> Result<Option<&'static str>, String> {
    for (n, line) in SEED_DIGESTS.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        match (it.next(), it.next(), it.next()) {
            (Some(k), Some(base), None) if !base.contains('/') => {
                if k == key {
                    return Ok(Some(base));
                }
            }
            _ => {
                return Err(format!(
                    "seed/seed-digests.txt line {}: malformed row `{line}' (want `key basename')",
                    n + 1
                ))
            }
        }
    }
    Ok(None)
}

/// The COMPILED control-plane seed-capture pins (re #469 round-8):
/// `seed/control-plane-seed-pins.txt`, sha256 of each ADMISSIBLE frozen seed
/// tarball (the `seed-manifest`/`seed-unpack` capture of the pinned §5
/// toolchain). Compiled in so the authority travels with td-builder itself;
/// pinning a new capture is a reviewed commit, never a runtime side effect.
const CONTROL_PLANE_SEED_PINS: &str = include_str!("../../seed/control-plane-seed-pins.txt");

/// AUTHENTICATE a `TD_SEED_DB` seed-capture db (re #469 round-8): the env var
/// is public, so the db is not trusted by presence — `seed-unpack` binds the
/// db to the tarball it restored from via the `<db>.seed-tarball` sidecar
/// (sha256 of the tarball bytes), and that sha256 must be one the COMPILED
/// pins file admits. The unforgeable core is the compiled pin: it names ONE
/// exact audited capture. (The sidecar itself is same-user-writable — the
/// standing trust-domain limit recorded on the receipt layer; the daemon-owned
/// provenance db is the follow-on, re #472.) A db with no sidecar (any db not
/// written by the current `seed-unpack`) or an unpinned capture refuses intake.
fn authenticate_seed_capture_db(dbp: &str) -> Result<(), String> {
    authenticate_seed_capture_db_with(dbp, CONTROL_PLANE_SEED_PINS)
}

/// The pins-parameterized core of `authenticate_seed_capture_db` — production
/// always passes the COMPILED pins; tests pass synthetic pins to drive the
/// green path without a repo-pinned capture existing yet.
fn authenticate_seed_capture_db_with(dbp: &str, pins: &str) -> Result<(), String> {
    let sidecar = format!("{dbp}.seed-tarball");
    let text = std::fs::read_to_string(&sidecar).map_err(|e| {
        format!(
            "seed db {dbp}: provenance rejected: no seed-tarball binding at {sidecar} ({e}) — \
             re-run `td-builder seed-unpack` (it records the capture the db restores) and pin \
             the capture in seed/control-plane-seed-pins.txt (re #469 round-8)"
        )
    })?;
    let sha = text
        .lines()
        .find_map(|l| l.strip_prefix("sha256 "))
        .map(|rest| rest.split_whitespace().next().unwrap_or(""))
        .filter(|s| s.len() == 64)
        .ok_or_else(|| format!("seed db {dbp}: malformed sidecar {sidecar}"))?;
    // The sidecar also fixes the db BYTES seed-unpack wrote: a row added or
    // edited after the restore breaks this leg even with a pinned capture —
    // the pin vouches for what the tarball restored, not for later edits.
    let want_db_sha = text
        .lines()
        .find_map(|l| l.strip_prefix("db-sha256 "))
        .map(str::trim)
        .filter(|s| s.len() == 64)
        .ok_or_else(|| format!("seed db {dbp}: sidecar {sidecar} has no db-sha256 binding"))?;
    let got_db_sha = sha256::sha256_file(Path::new(dbp))
        .map_err(|e| format!("sha256 {dbp}: {e}"))?;
    if got_db_sha != want_db_sha {
        return Err(format!(
            "seed db {dbp}: provenance rejected: the db was modified after seed-unpack wrote \
             it (sidecar db-sha256 {want_db_sha}, current {got_db_sha}) — re-run seed-unpack \
             (re #469 round-8)"
        ));
    }
    for (n, line) in pins.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let pin = line.split_whitespace().next().unwrap_or("");
        if pin.len() != 64 || !pin.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "seed/control-plane-seed-pins.txt line {}: malformed pin `{line}'",
                n + 1
            ));
        }
        if pin == sha {
            return Ok(());
        }
    }
    Err(format!(
        "seed db {dbp}: provenance rejected: its capture sha256 {sha} is not pinned in \
         seed/control-plane-seed-pins.txt — an unaudited seed capture cannot be typed \
         AuditedSeed; pin the capture via a reviewed commit (re #469 round-8)"
    ))
}

/// AUTHENTICATE a plan's seed db (re #469 round-8): the db path is
/// caller-supplied (and reused warm across `--auto` runs), so its rows are not
/// trusted by presence — every row must (a) content-address to its own on-disk
/// bytes (`authenticate_ca_db`) and (b) land on a basename the COMPILED
/// seed-digest table pins for some seed key. (a) kills rows registered over
/// foreign bytes; (b) kills self-consistent CA items the audited pins never
/// derived — together the db can only vouch for the pinned seed universe.
/// An ABSENT db authenticates vacuously: authority rides rows, and a missing
/// file has none to grant — any step that actually needs seed items then reds
/// at per-entry provenance or manifest assembly, never silently succeeds.
fn authenticate_seed_db(dbp: &str, items_dir: &Path) -> Result<(), String> {
    if !Path::new(dbp).is_file() {
        return Ok(());
    }
    authenticate_ca_db(dbp, items_dir, "plan seed")?;
    let mut pinned: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    for (n, line) in SEED_DIGESTS.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        match (it.next(), it.next(), it.next()) {
            (Some(_k), Some(base), None) if !base.contains('/') => {
                pinned.insert(base);
            }
            _ => {
                return Err(format!(
                    "seed/seed-digests.txt line {}: malformed row `{line}' (want `key basename')",
                    n + 1
                ))
            }
        }
    }
    let data = std::fs::read(dbp).map_err(|e| format!("read plan seed db {dbp}: {e}"))?;
    // Prefix a torn/truncated-db parse red with `plan seed db {dbp}` too: a crash-torn seed db
    // fails here, not at the basename check below, and the runner keys its clear-store recovery
    // hint on the `plan seed db` marker — without the prefix a torn seed db would red unhinted.
    let db = store_db_read::Db::open(data).map_err(|e| format!("plan seed db {dbp}: {e}"))?;
    for (path, _hash) in db
        .hashes_by_path()
        .map_err(|e| format!("plan seed db {dbp}: {e}"))?
    {
        let base = path.rsplit('/').next().unwrap_or(path.as_str());
        if !pinned.contains(base) {
            return Err(format!(
                "plan seed db {dbp}: provenance rejected: `{path}' is not a basename the \
                 compiled seed-digest table pins — an unpinned item cannot be typed \
                 AuditedSeed, however self-consistent its bytes (re #469 round-8)"
            ));
        }
    }
    Ok(())
}

fn auto_seed_provenance(
    store_prefix: &str,
    seed_store: &Path,
    name: &str,
    key: &str,
    path: &str,
) -> Result<(), String> {
    let base = path
        .strip_prefix(store_prefix)
        .and_then(|r| r.strip_prefix('/'))
        .filter(|b| !b.is_empty() && !b.contains('/'));
    let Some(base) = base else {
        return Err(format!(
            "--auto: provenance rejected: recipe `{name}' input `{key}' resolves to `{path}' — \
             not a canonical {store_prefix} item (a host path is not an admissible bootstrap \
             input, re #469)"
        ));
    };
    // COMPILED origin binding first (cheap, no IO): the seed-digest table
    // pins which basename this key may resolve to. A key the table does not
    // pin, or a basename the pins never derived, is inadmissible regardless
    // of how self-consistent the caller's store and db are.
    match seed_digests_expected(key)? {
        None => {
            return Err(format!(
                "--auto: provenance rejected: recipe `{name}' input `{key}' has no compiled \
                 expected digest (seed/seed-digests.txt) — an unpinned seed key is not \
                 admissible, whatever the map resolves it to (re #469)"
            ))
        }
        Some(exp) if exp != base => {
            return Err(format!(
                "--auto: provenance rejected: recipe `{name}' input `{key}' resolves to \
                 `{base}' but the compiled table pins `{exp}' — the map names bytes the \
                 pinned seed never derived (re #469)"
            ))
        }
        Some(_) => {}
    }
    let on_disk = seed_store.join(base);
    if !on_disk.exists() {
        return Err(format!(
            "--auto: provenance rejected: recipe `{name}' input `{key}' resolves to `{path}' \
             but `{base}' is not interned in the seed store {} (re #469)",
            seed_store.display()
        ));
    }
    let nar = nar_hash_path(&on_disk)
        .map_err(|e| format!("--auto: hash seed item {}: {e}", on_disk.display()))?;
    let hex = nar
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("--auto: unexpected NAR hash format for {}: {nar}", on_disk.display()))?;
    let item_name = base.split_once('-').map_or(base, |(_, n)| n);
    let expect = store::make_store_path_in(store_prefix, "source", hex, item_name);
    if expect != path {
        return Err(format!(
            "--auto: provenance rejected: recipe `{name}' input `{key}' resolves to `{path}' \
             but the interned bytes content-address to `{expect}' — the item's bytes do not \
             reproduce its own name (renamed, self-registered under the wrong address, or \
             tampered post-intern; origin authority is the calling runner's compiled pins, \
             re #469)"
        ));
    }
    Ok(())
}

/// Synthesize NAME's whole lock text FROM ITS RECIPE JSON — the #429 replacement for
/// the hand-written `ladder_lock` shell calls (which re-declared, per rung, exactly
/// what the recipe's own `inputs`/`nativeInputs`/`sourceInput` already say). Every
/// declared `inputs`/`nativeInputs` entry that is itself OWNED (has a recipe JSON)
/// becomes a `td-recipe-output` PENDING placeholder — build_plan substitutes the real
/// path once that step has run; every other declared input is resolved through MAP
/// (the pinned seed/source paths `ladder_setup` interned) and written `seed`, gated
/// by `auto_seed_provenance` (canonical store item, present in SEED-STORE — re #469).
/// The recipe's own declared `sourceInput` (if any) becomes the required
/// `<name>-source` line, resolved and gated the same way; a recipe with no
/// `sourceInput` (e.g. make-test, which only RUNS a sibling rung's output, not
/// compiles one) gets no source line. Reads + parses NAME's recipe JSON exactly once
/// (shared between the sourceInput check and the declared-inputs loop below).
fn auto_synthesize_lock(
    recipe_dir: &str,
    map: &std::collections::BTreeMap<String, String>,
    name: &str,
    store_prefix: &str,
    seed_store: &Path,
) -> Result<String, String> {
    let p = format!("{recipe_dir}/{name}.json");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("read recipe {p}: {e}"))?;
    let alist = json::parse(&text).map_err(|e| format!("recipe JSON {p}: {e}"))?;
    let mut out = String::new();
    if let Some(key) = alist.get("sourceInput").and_then(json::Json::as_str) {
        let path = auto_map_lookup(map, key)
            .map_err(|e| format!("--auto: recipe `{name}' sourceInput `{key}': {e}"))?;
        auto_seed_provenance(store_prefix, seed_store, name, key, &path)?;
        // The source lock entry is named `{name}-source` — the recipe references its
        // own source as `{in:{name}-source}` regardless of which pinned seed provides
        // it (gcc-mesboot0-source <- patch-gcc-boot-2.95.3, mesboot-headers-source <-
        // linux-headers). The sourceInput KEY it is digest-gated by is recovered at the
        // build-plan re-gate from the recipe's own sourceInput (see the Seed|Source arm).
        out.push_str(&format!("{name}-source {path} source\n"));
    }
    for inp in inputs_from_recipe_json(&alist) {
        if auto_is_owned(recipe_dir, &inp) {
            out.push_str(&format!("{inp} /td/store/pending-{inp} td-recipe-output\n"));
        } else {
            let path = auto_map_lookup(map, &inp)
                .map_err(|e| format!("--auto: recipe `{name}' input `{inp}': {e}"))?;
            auto_seed_provenance(store_prefix, seed_store, name, &inp, &path)?;
            out.push_str(&format!("{inp} {path} seed\n"));
        }
    }
    Ok(out)
}

/// build-plan --auto: GENERATE the plan from the recipe GRAPH, then run it. Given a
/// TARGET recipe spec, recursively resolve every declared input that is itself an owned
/// recipe (RECIPE-DIR/<name>.json exists), topo-sort, SYNTHESIZE each owned recipe's
/// whole lock straight from its declared graph (`auto_synthesize_lock` — owned deps
/// `td-recipe-output`, everything else resolved through MAP-FILE, the recipe's declared
/// `sourceInput` if any), and feed the generated plan to build_plan. No hand-written
/// lock, plan, or manifest (#429) — a recipe's edges chain automatically as the owned
/// set grows. MAP-resolved seed paths are provenance-gated at synthesis
/// (`auto_seed_provenance`): each must be a canonical store item interned in
/// GUIX-STORE (the plan's seed store), so the map cannot smuggle a host path in as a
/// `seed` lock entry (re #469).
///
/// TRUST BOUNDARY: this arm is the runner's PRIVATE BACKEND, not a production
/// entrance. Origin authority lives in the calling td-recipe-eval runner,
/// which re-derives every seed from its COMPILED pin table each run and hands
/// this arm an already-reconciled MAP/SEED-STORE/SEED-DB. The gates here
/// (self-authenticating content addresses, typed lock classes, strict
/// staging) are defense in depth against a drifted or tampered store — an
/// operator invoking this arm directly with a forged map is outside the
/// boundary, the same trust class as pointing TD_RECIPE_EVAL at old code.
///
/// Usage: build-plan --auto TARGET RECIPE-DIR MAP-FILE SEED-STORE SEED-DB SCRATCH
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
#[allow(clippy::too_many_arguments)] // seed_db (#468) + persist (#474 cache) both thread through this arm
fn build_plan_auto(
    target: &str,
    recipe_dir: &str,
    map_file: &str,
    guix_store: &str,
    seed_db: &str,
    scratch: &Path,
    builder_store: Option<(&str, &str, &str)>,
    persist: Option<(&str, &str)>,
) -> Result<(), String> {
    if !auto_is_owned(recipe_dir, target) {
        return Err(format!("--auto target `{target}': need {recipe_dir}/{target}.json"));
    }
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let map_text =
        std::fs::read_to_string(map_file).map_err(|e| format!("read --auto map {map_file}: {e}"))?;
    let map = auto_parse_map(&map_text);
    let mut order: Vec<String> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    auto_topo(recipe_dir, target, &mut order, &mut seen, &mut stack)?;
    eprintln!(
        "td-builder: build-plan --auto {target}: derived a {}-step plan from the recipe graph: {}",
        order.len(),
        order.join(" -> ")
    );
    let mut plan = String::new();
    let store_prefix = store::store_dir();
    for name in &order {
        let synthesized =
            auto_synthesize_lock(recipe_dir, &map, name, &store_prefix, Path::new(guix_store))?;
        let lock_path = scratch.join(format!("{name}-auto.lock"));
        std::fs::write(&lock_path, &synthesized).map_err(|e| e.to_string())?;
        plan.push_str(&format!(
            "step {recipe_dir}/{name}.json {}\n",
            lock_path.to_string_lossy()
        ));
    }
    let plan_path = scratch.join("auto.plan");
    std::fs::write(&plan_path, &plan).map_err(|e| e.to_string())?;
    build_plan(&plan_path.to_string_lossy(), guix_store, seed_db, scratch, builder_store, persist)
}

/// Emit PKG's recipe JSON from td's Rust catalog via `td-recipe-eval emit` — the
/// dependency-free evaluator (recipes/), set in TD_RECIPE_EVAL by the caller (placed,
/// td-built). This REPLACES the old `tsgo`+`td-ts-eval` `.ts` emit (the TypeScript
/// recipe surface was deleted in rust-recipe-surface, #224). td-recipe-eval `die`s
/// with a non-zero exit on an unknown stem, which we surface as the loud "no td
/// recipe for PKG" error — td shell resolves PKG to a td recipe or fails; it never
/// falls back to guix.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn emit_recipe_json(pkg: &str) -> Result<String, String> {
    let eval = std::env::var("TD_RECIPE_EVAL").map_err(|_| {
        "TD_RECIPE_EVAL must point at td's td-recipe-eval binary (the Rust recipe catalog evaluator)"
            .to_string()
    })?;
    let out = Command::new(&eval)
        .args(["emit", pkg])
        .output()
        .map_err(|e| format!("spawn td-recipe-eval ({eval}): {e}"))?;
    if !out.status.success() {
        // Unknown stem (or any emit failure) ⇒ loud error, NOT a guix fallback. Keep the
        // "no td recipe for" phrasing the td-shell gate's load-bearing leg asserts on.
        return Err(format!(
            "no td recipe for `{pkg}' — td shell builds td packages (the recipes/ catalog via td-recipe-eval), it does not fall back to guix: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("td-recipe-eval output not UTF-8: {e}"))
}

/// The pre-provisioned NATIVE `/td/store` toolchain `td shell` builds the Rust userland with,
/// handed in via the `TD_SHELL_NATIVE_*` environment (its provisioning gate `td-shell-userland`
/// is staged by the rust-toolchain recipe's daily product proof. When present, a vendored rust build (ripgrep,
/// fd, …) links this toolchain — never a host or downloaded-stage0 compiler. Its lock is assembled
/// from the interned package source plus these recipe outputs, whose physical store and databases
/// ride the typed `--recipe-output-store` / `--recipe-output-db` argv channels. Exact compiler,
/// interpreter, RUNPATH, include, and `-B` paths put run_rust in native link mode. A package build
/// with no native toolchain provisioned is a hard error.
struct NativeToolchain {
    /// Physical store containing the source-built stage2 and native build platform.
    /// Its canonical location is `/td/store`; `--recipe-output-store` carries the
    /// physical/canonical split explicitly to the build engine.
    store: String,
    /// The native toolchain's own td store db(s) (its `/td/store` outputs + refs), colon-
    /// separated → passed to build-recipe as `--recipe-output-db` argv (typed, re #469).
    extra_dbs: String,
    /// Native link mode: the `/td/store` glibc loader, RUNPATH, and `-B` dir baked by run_rust.
    interp: String,
    rpath: String,
    bdir: String,
    cc: String,
    cxx: String,
    include: String,
    /// Recipe-output lock lines for the source-built stage2, native
    /// GCC/binutils/glibc, and build userland.
    lock_lines: String,
}

impl NativeToolchain {
    /// Read the `TD_SHELL_NATIVE_*` env. `Ok(None)` when unset (no native toolchain provisioned);
    /// `Err` when partially set (a provisioning bug we surface loudly rather than silently
    /// falling back to a host compiler). `TD_SHELL_NATIVE_LOCK` names a file with
    /// the recipe-output lock lines.
    fn from_env() -> Result<Option<NativeToolchain>, String> {
        let store = match std::env::var("TD_SHELL_NATIVE_STORE") {
            Ok(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };
        let get = |k: &str| -> Result<String, String> {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("TD_SHELL_NATIVE_STORE is set but {k} is not (native-toolchain provisioning is incomplete)"))
        };
        let extra_dbs = get("TD_SHELL_NATIVE_EXTRA_DBS")?;
        let interp = get("TD_SHELL_NATIVE_INTERP")?;
        let rpath = get("TD_SHELL_NATIVE_RPATH")?;
        let bdir = get("TD_SHELL_NATIVE_BDIR")?;
        let cc = get("TD_SHELL_NATIVE_CC")?;
        let cxx = get("TD_SHELL_NATIVE_CXX")?;
        let include = get("TD_SHELL_NATIVE_INCLUDE")?;
        let lock_file = get("TD_SHELL_NATIVE_LOCK")?;
        let lock_lines = std::fs::read_to_string(&lock_file)
            .map_err(|e| format!("read TD_SHELL_NATIVE_LOCK {lock_file}: {e}"))?;
        Ok(Some(NativeToolchain {
            store,
            extra_dbs,
            interp,
            rpath,
            bdir,
            cc,
            cxx,
            include,
            lock_lines,
        }))
    }
}

/// td-builder shell — run a command with td-BUILT packages on PATH. td's own
/// `guix shell`, but with NO guix anywhere: each PKG is resolved to a td RECIPE and
/// BUILT by td-builder itself (the recipe → `td-builder build-recipe`, whose
/// content-addressed cache makes this build-on-demand + cached), then td composes
/// the command's PATH from the td store OUTPUT and execs. There is no `guix`
/// process in the resolve/build/exec path; an unknown package errors ("no td recipe
/// for PKG"), it does NOT fall back to guix. The package that lands on PATH is td's
/// own build at td's own store path. A vendored rust build (ripgrep, fd, …) links the
/// NATIVE `/td/store` toolchain provisioned via `TD_SHELL_NATIVE_*` (see `NativeToolchain`),
/// never the downloaded stage0 Rust snapshot or a host compiler.
///
/// Config (env): TD_RECIPE_EVAL (td's Rust recipe-catalog evaluator, to emit the
/// recipe),
/// TD_SHELL_NATIVE_* (the pre-provisioned native `/td/store` toolchain for vendored rust builds —
/// `NativeToolchain::from_env`), TD_SHELL_CACHE (build cache root, default `$HOME/.cache/td-shell`),
/// TD_BUILDER_PATH/STORE/DB (optional stage0 builder override, so the build's builder
/// is td-placed too).
///
/// Usage: shell PKG... [-- CMD ARGS...]
///   PKG...      td package names (a recipe must exist; no guix fallback)
///   -- CMD...   the command to run in the composed env; omitted → interactive $SHELL
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn run_shell(rest: &[String]) -> Result<std::process::ExitStatus, String> {
    // Everything before the first `--` is a package name; after it, the command.
    let sep = rest.iter().position(|a| a == "--");
    let (pkgs, cmd): (&[String], &[String]) = match sep {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[]),
    };

    let cache = match std::env::var("TD_SHELL_CACHE") {
        Ok(c) => c,
        Err(_) => format!(
            "{}/.cache/td-shell",
            std::env::var("HOME").map_err(|_| "set TD_SHELL_CACHE or HOME".to_string())?
        ),
    };
    let self_exe = std::env::current_exe()
        .map_err(|e| format!("locate td-builder: {e}"))?
        .to_string_lossy()
        .into_owned();
    // The pre-provisioned NATIVE /td/store toolchain (TD_SHELL_NATIVE_*), if any. When present, a
    // vendored rust build links it instead of downloaded stage0 or a host compiler.
    let native = NativeToolchain::from_env()?;

    // Build each named package with td-builder itself — no guix — and collect the
    // td store output's bin/sbin dirs to put on PATH.
    let mut prefix_dirs: Vec<String> = Vec::new();
    for pkg in pkgs {
        // Resolve PKG to a td recipe via the Rust catalog (td-recipe-eval) and emit its
        // JSON FIRST — an unknown PKG ⇒ loud "no td recipe" error, NOT a guix fallback.
        // (Resolve before the lock check so an unknown package reports "no td recipe", the
        // load-bearing leg the td-shell gate asserts; a known pkg then needs its lock.)
        let recipe_json = emit_recipe_json(pkg)?;
        // Stage the recipe JSON in the per-package cache dir that build-recipe also keys
        // its build cache on.
        let sd = format!("{cache}/{pkg}");
        std::fs::create_dir_all(&sd).map_err(|e| e.to_string())?;
        let json_file = format!("{sd}/recipe.json");
        std::fs::write(&json_file, &recipe_json).map_err(|e| e.to_string())?;
        // Assemble the build-recipe argv. A rust userland recipe (ripgrep/fd/…) needs its
        // whole crate closure provisioned GUIX-FREE: td interns the warmed source + crate
        // set and feeds build-recipe's 11-arg form (TD_VENDOR_DIR), exactly as the
        // crate-free corpus gates do — but here from the real `td shell` product command,
        // not a bespoke harness.
        let mut bargs: Vec<String> = vec!["build-recipe".into(), json_file.clone()];
        // A vendored rust build links the native /td/store toolchain when it is provisioned; a
        // package without warmed inputs cannot use the retired legacy shell path. Keep the chosen
        // native env so the build-recipe subprocess gets TD_RUST_STORE_* (and the
        // recipe-output store/db argv) for the vendored-rust case.
        let native_env = match provision_rust_inputs(pkg, &sd, &self_exe)? {
            Some((seedlock, source_lock, extra)) => {
                let Some(nt) = &native else {
                    return Err(format!(
                        "build `{pkg}': a vendored rust build needs the native /td/store toolchain, \
                         but TD_SHELL_NATIVE_STORE is not set. The rust-toolchain recipe check \
                         must stage native gcc/binutils/glibc plus the recipe-graph source-built \
                         stage2 Rust toolchain. Downloaded stage0 and host compiler fallbacks are \
                         forbidden for `td shell'."
                    ));
                };
                // Add only the explicitly provisioned recipe outputs to the
                // interned package source lock. No host or stage0 compiler line
                // is inherited from a legacy package lock.
                let retargeted = recipe_toolchain_lock_body(&source_lock, &nt.lock_lines)?;
                std::fs::write(&seedlock, &retargeted)
                    .map_err(|e| format!("write seed lock {seedlock}: {e}"))?;
                bargs.push(seedlock);
                bargs.push(sd.clone());
                bargs.push(nt.store.clone());
                bargs.extend(extra);
                bargs.push("--recipe-output-store".into());
                bargs.push(nt.store.clone());
                // The native toolchain's own store db(s): prior td recipe outputs,
                // passed as the TYPED `--recipe-output-db` argv channel (the
                // TD_EXTRA_DBS env authority channel is deleted, re #469).
                for db in nt.extra_dbs.split(':').filter(|s| !s.is_empty()) {
                    bargs.push("--recipe-output-db".into());
                    bargs.push(db.to_string());
                }
                nt
            }
            None => {
                return Err(format!(
                    "build `{pkg}': no warmed vendored source/crate closure found under \
                     TD_SHELL_VENDOR_ROOT; the legacy shell build path is retired"
                ));
            }
        };
        // BUILD it via the build-recipe subcommand (its content-addressed cache makes
        // an unchanged recipe a HIT — build-on-demand + cached). A subprocess keeps the
        // build's chatter off the command's stdout, and rides the inherited
        // TD_BUILDER_* override so the builder is the td-placed stage0 too.
        let mut build = Command::new(&self_exe);
        build.args(&bargs);
        // Native link mode: recipe outputs are admitted through typed argv,
        // while the exact nested compiler/runtime paths are embedded in the drv.
        build
            .env("TD_RUST_STORE_INTERP", &native_env.interp)
            .env("TD_RUST_STORE_RPATH", &native_env.rpath)
            .env("TD_RUST_STORE_BDIR", &native_env.bdir)
            .env("TD_RUST_STORE_CC", &native_env.cc)
            .env("TD_RUST_STORE_CXX", &native_env.cxx)
            .env("TD_RUST_STORE_INCLUDE", &native_env.include);
        let out = build
            .output()
            .map_err(|e| format!("build `{pkg}': spawn td-builder build-recipe: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "build `{pkg}' failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // build-recipe prints `OUT=out <canonical-store-path>`; the realized tree is
        // staged at <sd>/newstore/<basename> (a td path distinct from guix's).
        let outline = String::from_utf8_lossy(&out.stdout);
        let canonical = outline
            .lines()
            .find_map(|l| l.strip_prefix("OUT=out "))
            .ok_or_else(|| format!("build `{pkg}': build-recipe reported no `out' output"))?
            .trim();
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        let outdir = format!("{sd}/newstore/{base}");
        let mut any = false;
        for sub in ["bin", "sbin"] {
            let dir = format!("{outdir}/{sub}");
            if Path::new(&dir).is_dir() {
                prefix_dirs.push(dir);
                any = true;
            }
        }
        if !any {
            return Err(format!("build `{pkg}': td output {outdir} has no bin/sbin"));
        }
    }

    // Compose the child PATH ourselves: the td package bins FIRST (so the
    // package's binary wins — the package is load-bearing), then the inherited
    // PATH (guix shell's non-pure default). td builds this string; no guix
    // process is between us and the command.
    let inherited = std::env::var("PATH").unwrap_or_default();
    let mut path = prefix_dirs.join(":");
    if !inherited.is_empty() {
        if !path.is_empty() {
            path.push(':');
        }
        path.push_str(&inherited);
    }

    // Explicit `-- CMD…`, else drop into an interactive $SHELL (fallback /bin/sh).
    let shell;
    let (prog, prog_args): (&str, &[String]) = if let Some((first, args)) = cmd.split_first() {
        (first.as_str(), args)
    } else {
        shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        (shell.as_str(), &[])
    };

    Command::new(prog)
        .args(prog_args)
        .env("PATH", &path)
        .status()
        .map_err(|e| format!("run `{prog}': {e}"))
}

/// Intern a source path into a td-OWNED store with td's OWN recursive add-to-store
/// (`store-add-recursive`) — no `guix repl`, no guix-daemon. Returns the
/// content-addressed `source` store path td computed from the path's recursive
/// NAR sha256 and restored under `store_dir` (+ `db`).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn run_store_add(
    self_exe: &str,
    name: &str,
    tree: &Path,
    store_dir: &Path,
    db: &Path,
) -> Result<String, String> {
    std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
    let out = Command::new(self_exe)
        .args([
            "store-add-recursive",
            name,
            &tree.to_string_lossy(),
            &store_dir.to_string_lossy(),
            &db.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("spawn store-add-recursive {name}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "store-add-recursive {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Err(format!("store-add-recursive {name} produced no path"));
    }
    Ok(path)
}

/// The package build lock begins with only its content-addressed source. Registry
/// crates ride the separately interned vendor tree and the build platform is
/// appended from authenticated recipe outputs below; no retired package lock or
/// host-tool line is carried forward.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn source_lock_body(sourcekey: &str, src_canonical: &str) -> String {
    format!("{sourcekey} {src_canonical} source\n")
}

struct ShellSourcePin {
    key: String,
    sha256: String,
    file: String,
}

/// Resolve a package's recipe-owned fixed-output source pin. Keeping this
/// lookup in td-recipe-eval means the builder does not duplicate package URLs
/// or hashes, and final-userland pins do not enter the bootstrap seed table.
fn shell_recipe_source_pin(pkg: &str) -> Result<ShellSourcePin, String> {
    let eval = std::env::var("TD_RECIPE_EVAL").map_err(|_| {
        "TD_RECIPE_EVAL must point at td's td-recipe-eval binary (source pin lookup)".to_string()
    })?;
    let out = Command::new(&eval)
        .args(["source-pin", pkg])
        .output()
        .map_err(|e| format!("spawn td-recipe-eval ({eval}) for `{pkg}' source pin: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "td-recipe-eval source-pin `{pkg}' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let pins = String::from_utf8(out.stdout)
        .map_err(|e| format!("td-recipe-eval source pins are not UTF-8: {e}"))?;
    parse_shell_source_pin(&pins)
}

fn parse_shell_source_pin(pins: &str) -> Result<ShellSourcePin, String> {
    let mut found: Option<ShellSourcePin> = None;
    for line in pins.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [pin_key, url, sha256, file] = fields.as_slice() else {
            return Err(format!("malformed td shell recipe source pin: {line}"));
        };
        if found.is_some() {
            return Err("td shell Rust recipe must declare exactly one fixed-output source pin".into());
        }
        let key = *pin_key;
        if key.is_empty() {
            return Err("td shell recipe source pin has an empty key".into());
        }
        if url.is_empty() {
            return Err(format!("recipe source pin `{key}' has an empty URL"));
        }
        if sha256.len() != 64
            || !sha256
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(format!("recipe source pin `{key}' has a non-canonical sha256"));
        }
        if file.is_empty()
            || Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file)
        {
            return Err(format!("recipe source pin `{key}' has a non-basename file `{file}'"));
        }
        found = Some(ShellSourcePin {
            key: key.to_string(),
            sha256: (*sha256).to_string(),
            file: (*file).to_string(),
        });
    }
    found.ok_or_else(|| "td shell Rust recipe declares no fixed-output source pin".to_string())
}

fn verify_shell_source_archive(path: &Path, pin: &ShellSourcePin) -> Result<(), String> {
    let got = sha256::sha256_file(path)
        .map_err(|e| format!("read pinned source archive {}: {e}", path.display()))?;
    if got != pin.sha256 {
        return Err(format!(
            "warmed source archive {} sha256 {got} != recipe source pin {} for `{}`",
            path.display(), pin.sha256, pin.key
        ));
    }
    Ok(())
}

/// Add the authenticated `/td/store` recipe-output build platform to an
/// interned-source lock. This replaces the old mixed Guix/native retargeting:
/// any `/gnu/store` line or downloaded `rust-stage0` input is now a hard error,
/// not something filtered heuristically.
fn recipe_toolchain_lock_body(seed_body: &str, native_lines: &str) -> Result<String, String> {
    let source_lines: Vec<&str> = seed_body.lines().filter(|line| !line.trim().is_empty()).collect();
    if source_lines.len() != 1 {
        return Err("td shell package source lock must contain exactly one source line".into());
    }
    let source_line = source_lines
        .first()
        .copied()
        .ok_or("td shell package source lock has no source line")?;
    let source_fields: Vec<&str> = source_line
        .split_whitespace()
        .collect();
    if source_fields.len() != 3
        || source_fields.get(1).is_none_or(|path| !path.starts_with("/td/store/"))
        || source_fields.get(2).copied() != Some("source")
    {
        return Err(format!(
            "td shell package source lock is not one canonical /td/store source: {}",
            source_line
        ));
    }
    let native = native_lines.trim_end_matches('\n');
    if native.is_empty() {
        return Err("td shell native recipe-output lock is empty".into());
    }
    for line in native.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 3
            || fields.get(1).is_none_or(|path| !path.starts_with("/td/store/"))
            || fields.get(2).copied() != Some("td-recipe-output")
        {
            return Err(format!(
                "td shell native lock line is not one canonical /td/store recipe output: {line}"
            ));
        }
        if fields.iter().any(|field| field.contains("rust-stage0")) {
            return Err(format!(
                "td shell native lock names the downloaded rust-stage0 trust root: {line}"
            ));
        }
    }
    let mut out = source_line.to_string();
    out.push('\n');
    out.push_str(native);
    out.push('\n');
    Ok(out)
}

/// Provision a rust recipe's crate closure for `td shell`, GUIX-FREE, so
/// `td shell ripgrep -- rg …` builds the real shipped userland through the
/// product command rather than a bespoke gate harness.
///
/// Source of the crates: a warmed tree at `$TD_SHELL_VENDOR_ROOT/<pkg>/{work,vendor}`
/// (host PREP via `td-feed warm crate`). The package source archive is verified again
/// here against the recipe catalog's committed fixed-output SHA-256 before interning;
/// dependency archives remain selected and checksum-verified by the pinned Cargo.lock.
/// This:
///   - verifies and interns the immutable source `.crate` with `store-add-recursive`,
///   - interns the crate SET the same way (a no-ref content-addressed tree),
///   - writes a lock containing only `<pkg>-source <interned-src>`; Cargo.lock
///     inside that source selects and verifies the separately interned crates,
///
/// and returns `(seed-lock-path, seed-lock-body, [src-store, src-db, vendor-canonical, vendor-store,
/// vendor-db])` — the extra positional args build-recipe's 11-arg form takes.
///
/// Returns `Ok(None)` when no warmed closure exists for PKG (`TD_SHELL_VENDOR_ROOT` unset,
/// or no `<pkg>/vendor` under it); the caller fails closed because the legacy
/// shell fallback was retired with the corpus.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn provision_rust_inputs(
    pkg: &str,
    sd: &str,
    self_exe: &str,
) -> Result<Option<(String, String, [String; 5])>, String> {
    let vendor_root = match std::env::var("TD_SHELL_VENDOR_ROOT") {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(None),
    };
    let pkg_root = Path::new(&vendor_root).join(pkg);
    let vendor = pkg_root.join("vendor");
    // No warmed crate closure for this package here ⇒ not a vendored rust build.
    if !vendor.is_dir() {
        return Ok(None);
    }
    let pin = shell_recipe_source_pin(pkg)?;
    let source_archive = pkg_root.join("work").join(&pin.file);
    verify_shell_source_archive(&source_archive, &pin)?;
    let ncrate = std::fs::read_dir(&vendor)
        .map_err(|e| format!("read {}: {e}", vendor.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "crate"))
        .count();
    if ncrate == 0 {
        return Err(format!(
            "no `.crate' files under {} — re-run `td-feed warm crate'",
            vendor.display()
        ));
    }

    let work = Path::new(sd);
    // --- intern the recipe-authenticated source archive ---
    let src_store = work.join("srcstore");
    let src_db = work.join("src.db");
    let _ = std::fs::remove_dir_all(&src_store);
    let _ = std::fs::remove_file(&src_db);
    let src_canonical = run_store_add(
        self_exe,
        &format!("{pkg}-src"),
        &source_archive,
        &src_store,
        &src_db,
    )?;

    // --- intern the crate set ---
    let vendor_store = work.join("vendorstore");
    let vendor_db = work.join("vendor.db");
    let _ = std::fs::remove_dir_all(&vendor_store);
    let _ = std::fs::remove_file(&vendor_db);
    let vendor_canonical =
        run_store_add(self_exe, &format!("{pkg}-vendor"), &vendor, &vendor_store, &vendor_db)?;

    // --- source lock: exact interned package source only --------------------
    let sourcekey = pin.key;
    let seed = source_lock_body(&sourcekey, &src_canonical);
    let seedlock = work.join("seed.lock");

    Ok(Some((
        seedlock.to_string_lossy().into_owned(),
        seed,
        [
            src_store.to_string_lossy().into_owned(),
            src_db.to_string_lossy().into_owned(),
            vendor_canonical,
            vendor_store.to_string_lossy().into_owned(),
            vendor_db.to_string_lossy().into_owned(),
        ],
    )))
}

/// td-builder profile — build a PROFILE: a symlink tree unioning the `bin`/`sbin` of a
/// set of installed package outputs, the user-package-manager profile layer (like a guix
/// profile / nix env). PROFILE-DIR is rebuilt fresh; for each PKG-OUT (a store output dir,
/// e.g. `~/.td/store/<hash>-hello`), every entry under `bin`/`sbin` is symlinked into
/// PROFILE-DIR/{bin,sbin}, pointing at the absolute store path. A user puts PROFILE-DIR/bin
/// on PATH (or symlinks `~/bin/<tool>` → PROFILE-DIR/bin/<tool>). A name provided by two
/// packages is a COLLISION (error — explicit, like guix). The symlinks resolve to the
/// store, so the profile is a thin, GC-friendly view that swaps atomically when rebuilt.
///
/// Usage: profile PROFILE-DIR PKG-OUT...
/// Union the bin/sbin of each package output into a symlink-tree profile. When
/// `store_native_prefix` is `Some(prefix)`, the symlink TARGETS are the LOGICAL store paths
/// (`<prefix>/<basename(pkg)>/<sub>/<entry>`) rather than the physical PKG-OUT path passed in
/// — so the profile resolves inside a store-ns own-root where `prefix` (e.g. `/td/store`) is
/// the bound store but the physical scratch dir is absent. `None` keeps the thin-view behavior
/// (link straight at PKG-OUT as given). Enumeration always reads the physical PKG-OUT dir.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_profile(
    profile_dir: &str,
    pkgs: &[String],
    store_native_prefix: Option<&str>,
) -> Result<usize, String> {
    use std::os::unix::fs::symlink;
    let pdir = Path::new(profile_dir);
    // Rebuild fresh (idempotent) — a profile is a derived view, not state.
    if pdir.exists() {
        std::fs::remove_dir_all(pdir).map_err(|e| format!("clear {profile_dir}: {e}"))?;
    }
    std::fs::create_dir_all(pdir).map_err(|e| e.to_string())?;
    let mut linked = 0usize;
    for pkg in pkgs {
        let pkgp = Path::new(pkg);
        if !pkgp.is_dir() {
            return Err(format!("package output `{pkg}' is not a directory"));
        }
        for sub in ["bin", "sbin"] {
            let src_dir = pkgp.join(sub);
            if !src_dir.is_dir() {
                continue;
            }
            let dst_dir = pdir.join(sub);
            std::fs::create_dir_all(&dst_dir).map_err(|e| e.to_string())?;
            let mut entries: Vec<_> = std::fs::read_dir(&src_dir)
                .map_err(|e| format!("read {}: {e}", src_dir.display()))?
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            entries.sort_by_key(|e| e.file_name());
            for ent in entries {
                let dst = dst_dir.join(ent.file_name());
                // symlink_metadata (lexists), not exists(): a store-native link is a LOGICAL
                // path that dangles on the host, so exists() would follow it and miss the clash.
                if dst.symlink_metadata().is_ok() {
                    return Err(format!(
                        "profile collision: `{sub}/{}' is provided by more than one package (last: {pkg})",
                        ent.file_name().to_string_lossy()
                    ));
                }
                // Absolute symlink INTO the store (so the profile is a thin view). In
                // store-native mode, retarget to the LOGICAL store path so it resolves in
                // the own-root; otherwise link straight at the physical PKG-OUT entry.
                let target = match store_native_prefix {
                    Some(prefix) => {
                        let base = pkgp
                            .file_name()
                            .ok_or_else(|| format!("package `{pkg}' has no basename"))?;
                        Path::new(prefix).join(base).join(sub).join(ent.file_name())
                    }
                    None => ent.path(),
                };
                symlink(&target, &dst)
                    .map_err(|e| format!("symlink {} -> {}: {e}", dst.display(), target.display()))?;
                linked += 1;
            }
        }
    }
    if linked == 0 {
        return Err("no bin/sbin entries in any package — refusing to write an empty profile".into());
    }
    Ok(linked)
}

/// The nice value build work runs at, from `TD_BUILD_NICE` (default 10). Parsed
/// from the raw env value so the policy is unit-testable without touching real
/// process state. Clamped to the kernel's -20..=19 range; a missing/garbage value
/// falls back to the default.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn parse_build_nice(raw: Option<String>) -> i32 {
    raw.and_then(|v| v.trim().parse::<i32>().ok()).unwrap_or(10).clamp(-20, 19)
}

/// Raise THIS process's niceness so the compilers/`make` it spawns (which inherit
/// the value at fork) yield CPU to anything interactive sharing the host — a
/// desktop/compositor stays responsive during a build storm. Best-effort and
/// increase-only: the kernel rejects an unprivileged DEcrease with EPERM, which
/// just means we were already at least this nice, so we ignore the result. Purely
/// a scheduling knob — build OUTPUT (and thus reproducibility) is unaffected.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nice_self_for_builds() {
    let _ = sys::set_self_priority(parse_build_nice(std::env::var("TD_BUILD_NICE").ok()));
}

/// Parse an `oci-image`/`oci-image-closure` CONFIG-JSON ({"repoTag","env","entrypoint",
/// "cmd"}, all optional; repoTag defaults to td:latest) into an `oci::ImageConfig`.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn image_config_from_json(cj: &json::Json) -> oci::ImageConfig {
    let strs = |key: &str| -> Vec<String> {
        cj.get(key)
            .and_then(json::Json::as_arr)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    oci::ImageConfig {
        repo_tag: cj.get("repoTag").and_then(json::Json::as_str).unwrap_or("td:latest").to_string(),
        env: strs("env"),
        entrypoint: strs("entrypoint"),
        cmd: strs("cmd"),
    }
}

/// Parsed `host-sandbox` invocation (the loop container). Pure data so the flag
/// grammar is unit-testable without touching namespaces.
#[derive(Debug)]
struct HostSandboxArgs {
    expose_cwd: bool,
    /// `--store-from DIR`: bind DIR (an unpacked store, e.g. a captured seed or the
    /// `/td/store` harness) instead of the host `/gnu/store`.
    store_from: Option<String>,
    /// `--store-at DEST`: the in-sandbox mount point for `--store-from`. Defaults to
    /// the ACTIVE store dir (`store::store_dir()` — a guix-captured seed's binaries
    /// hardcode that interpreter path under the `/gnu/store` default); pass
    /// `/td/store` for td's own store-native harness (interp relinked to
    /// `/td/store/ld`). Only meaningful with `--store-from`; when DEST != `/gnu/store`
    /// the host `/gnu/store` is NOT bound at all — the guix-byte-free VM substrate.
    store_at: Option<String>,
    /// `--store-item PATH` (repeatable): bind ONE store item (dir or file)
    /// read-only at its own path, and `--store-item-at SRC DEST` (repeatable):
    /// bind SRC read-only at DEST — for items whose durable host home is not
    /// their canonical store path (the td-built loop userland lives in a
    /// loop-owned host dir but is hashed for, and must appear at, its
    /// /td/store path). Together these are the loop's input-only store
    /// exposure (`td-builder check` passes its declared input set item by
    /// item; no store DIRECTORY is ever mounted, mirroring the drv build
    /// jail's staged-closure model). Each ITEM's read-only remount is
    /// load-bearing, and the dir holding the own-path (`--store-item`)
    /// mountpoints is itself locked READ-ONLY after binding (host_shell
    /// ro_dirs) so an accidental write can't plant a sibling entry next to
    /// the declared items (not a boundary against a hostile gate, which owns
    /// the sandbox namespaces); only the DEST-mapped items' /td/store parent
    /// stays writable — the loop's working store prefix.
    ///
    /// NOT a host-ingress channel (re #469): `--store-item` binds whatever
    /// path its CALLER names, but the callers are contained. Recipe builds
    /// never reach this verb — they stage through `realize_drv`'s
    /// hash-verified StageManifest. Gate bodies invoking it run INSIDE the
    /// loop sandbox, whose mount namespace holds only the already-admitted
    /// inputs (per-item binds + the worktree + /td/store), so any path a
    /// nested invocation can name is already inside the boundary — host
    /// paths do not exist there to be named. An operator running
    /// `td-builder host-sandbox` directly on the host is the operator
    /// debugging the sandbox itself: operator trust, outside the boundary,
    /// same class as pointing TD_RECIPE_EVAL at old code.
    store_items: Vec<(String, Option<String>)>,
    /// `--no-daemon`: accepted for compatibility. The loop sandbox no longer binds
    /// the host daemon state in either mode.
    no_daemon: bool,
    cmd: String,
    cmd_args: Vec<String>,
}

/// Parse the full `td-builder host-sandbox …` argv (args[0]=prog, args[1]=subcommand,
/// flags…, `--`, CMD, CMD-ARGS…). Returns the parsed form or a user-facing message.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn parse_host_sandbox_args(args: &[String]) -> Result<HostSandboxArgs, String> {
    let mut i = 2usize;
    let mut expose_cwd = false;
    let mut store_from: Option<String> = None;
    let mut store_at: Option<String> = None;
    let mut store_items: Vec<(String, Option<String>)> = Vec::new();
    let mut no_daemon = false;
    while i < args.len() && args[i] != "--" {
        match args[i].as_str() {
            "--expose-cwd" => expose_cwd = true,
            "--no-daemon" => no_daemon = true,
            "--store-from" => {
                i += 1;
                if i >= args.len() || args[i] == "--" {
                    return Err("--store-from needs a DIR".to_string());
                }
                store_from = Some(args[i].clone());
            }
            "--store-at" => {
                i += 1;
                if i >= args.len() || args[i] == "--" {
                    return Err("--store-at needs a DIR".to_string());
                }
                store_at = Some(args[i].clone());
            }
            "--store-item" => {
                i += 1;
                if i >= args.len() || args[i] == "--" {
                    return Err("--store-item needs a PATH".to_string());
                }
                store_items.push((args[i].clone(), None));
            }
            "--store-item-at" => {
                if i + 2 >= args.len() || args[i + 1] == "--" || args[i + 2] == "--" {
                    return Err("--store-item-at needs SRC and DEST".to_string());
                }
                store_items.push((args[i + 1].clone(), Some(args[i + 2].clone())));
                i += 2;
            }
            other => return Err(format!("unknown flag `{other}'")),
        }
        i += 1;
    }
    if store_at.is_some() && store_from.is_none() {
        return Err("--store-at requires --store-from".to_string());
    }
    // args[i] is now "--" (or we ran off the end); the command follows it.
    if i >= args.len() || i + 1 >= args.len() {
        return Err("usage: td-builder host-sandbox [--expose-cwd] [--store-from DIR [--store-at DEST]] [--store-item PATH]... [--store-item-at SRC DEST]... [--no-daemon] -- CMD ARGS...".to_string());
    }
    Ok(HostSandboxArgs {
        expose_cwd,
        store_from,
        store_at,
        store_items,
        no_daemon,
        cmd: args[i + 1].clone(),
        cmd_args: args[i + 2..].to_vec(),
    })
}

fn host_sandbox_base_binds(store_from: Option<&str>, store_at: Option<&str>) -> Vec<sandbox::Bind> {
    match store_from {
        Some(dir) => vec![sandbox::Bind {
            src: dir.to_string(),
            // --store-at omitted → mount at the ACTIVE store dir (TD_STORE_DIR
            // or the default), not a hardcoded /gnu/store.
            dest: Some(store_at.map_or_else(store::store_dir, str::to_string)),
            readonly: true,
            ro_optional: false,
        }],
        None => Vec::new(),
    }
}

fn run_mount_applet(args: &[String]) -> Result<i32, String> {
    if args.get(1).map(String::as_str) != Some("--bind") || args.len() != 4 {
        return Err("usage: mount --bind SRC DEST".to_string());
    }
    let src = args.get(2).ok_or_else(|| "missing bind source".to_string())?;
    let dest = args.get(3).ok_or_else(|| "missing bind target".to_string())?;
    if !Path::new(dest).exists() && std::env::var("TD_HOST_SANDBOX").as_deref() == Ok("1") {
        let src_md = std::fs::metadata(src).map_err(|e| format!("bind source `{src}`: {e}"))?;
        if src_md.is_dir() {
            std::fs::create_dir_all(dest)
                .map_err(|e| format!("create sandbox bind target dir `{dest}`: {e}"))?;
        } else {
            if let Some(parent) = Path::new(dest).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create sandbox bind target parent `{}`: {e}", parent.display()))?;
            }
            std::fs::File::create(dest)
                .map_err(|e| format!("create sandbox bind target file `{dest}`: {e}"))?;
        }
    }
    let src_c = CString::new(src.as_str()).map_err(|e| format!("bind source `{src}`: {e}"))?;
    let dest_c = CString::new(dest.as_str()).map_err(|e| format!("bind target `{dest}`: {e}"))?;
    sys::mount(Some(&src_c), &dest_c, None, sys::MS_BIND, None)
        .map_err(|e| format!("bind-mount `{src}` on `{dest}`: {e}"))?;
    Ok(0)
}

fn take_flock(fd: i32, nonblock: bool) -> Result<bool, String> {
    if nonblock {
        sys::flock_try_exclusive(fd).map_err(|e| format!("flock fd {fd}: {e}"))
    } else {
        sys::flock_exclusive(fd).map_err(|e| format!("flock fd {fd}: {e}"))?;
        Ok(true)
    }
}

fn run_flock_applet(args: &[String]) -> Result<i32, String> {
    let mut i = 1usize;
    let nonblock = if args.get(i).map(String::as_str) == Some("-n") {
        i += 1;
        true
    } else {
        false
    };
    let target = args
        .get(i)
        .ok_or_else(|| "usage: flock [-n] FD | flock [-n] PATH COMMAND [ARG...]".to_string())?;
    i += 1;

    if let Ok(fd) = target.parse::<i32>() {
        if i != args.len() {
            return Err("usage: flock [-n] FD".to_string());
        }
        return Ok(if take_flock(fd, nonblock)? { 0 } else { 1 });
    }

    let cmd = args
        .get(i)
        .ok_or_else(|| "usage: flock [-n] PATH COMMAND [ARG...]".to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(target)
        .map_err(|e| format!("open lock `{target}`: {e}"))?;
    if !take_flock(lock.as_raw_fd(), nonblock)? {
        return Ok(1);
    }
    let cmd_args = args
        .get(i + 1..)
        .ok_or_else(|| "usage: flock [-n] PATH COMMAND [ARG...]".to_string())?;
    let status = Command::new(cmd)
        .args(cmd_args)
        .status()
        .map_err(|e| format!("run `{cmd}` under flock `{target}`: {e}"))?;
    Ok(status.code().map_or(1, |code| code))
}

fn applet_exit(name: &str, result: Result<i32, String>) -> ExitCode {
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("td-builder {name} applet: {e}");
            ExitCode::FAILURE
        }
    }
}

struct RecipeOutputOptions {
    positional_len: usize,
    dbs: Vec<(String, sandbox::InputOrigin)>,
    store: Option<String>,
}

/// Select the physical byte stores and their canonical prefix for a
/// `build-recipe` invocation. Recipe-output DBs remain the authority that
/// admits prior outputs; the recipe-output store is also a candidate byte
/// directory so closure staging can materialize those authenticated rows.
fn build_recipe_store_layout<'a>(
    seed_store: Option<&'a str>,
    seed_db: Option<&str>,
    recipe_output_store: Option<&'a str>,
    legacy_store: &'a str,
) -> Result<(Vec<String>, String, Option<&'a Path>), String> {
    match (seed_store, seed_db) {
        (Some(seed), Some(_)) => Ok((
            vec![seed.to_string()],
            store::STORE_DIR.to_string(),
            recipe_output_store
                .map(Path::new)
                .or_else(|| Some(Path::new(seed))),
        )),
        (None, None) => match recipe_output_store {
            Some(recipe_store) => Ok((
                vec![recipe_store.to_string()],
                store::store_dir(),
                Some(Path::new(recipe_store)),
            )),
            // Legacy callers without the explicit option still use STORE-DIR
            // as both physical and canonical input space.
            None => Ok((
                vec![legacy_store.to_string()],
                legacy_store.to_string(),
                None,
            )),
        },
        _ => Err("TD_SEED_STORE/TD_SEED_DB must be set together".into()),
    }
}

/// Peel build-recipe's typed recipe-output options from the argv tail. They
/// may be interleaved, DBs are repeatable in caller order, and the physical
/// store may be supplied once.
fn parse_recipe_output_options(args: &[String]) -> Result<RecipeOutputOptions, String> {
    let mut positional_len = args.len();
    let mut dbs = Vec::new();
    let mut store = None;
    while positional_len >= 8 {
        match args.get(positional_len.saturating_sub(2)).map(String::as_str) {
            Some("--recipe-output-db") => {
                let db = args
                    .get(positional_len.saturating_sub(1))
                    .ok_or("--recipe-output-db has no value")?;
                dbs.push((db.clone(), sandbox::InputOrigin::RecipeOutput));
                positional_len = positional_len.saturating_sub(2);
            }
            Some("--recipe-output-store") => {
                if store.is_some() {
                    return Err("--recipe-output-store may be passed only once".into());
                }
                store = args.get(positional_len.saturating_sub(1)).cloned();
                positional_len = positional_len.saturating_sub(2);
            }
            _ => break,
        }
    }
    dbs.reverse();
    Ok(RecipeOutputOptions {
        positional_len,
        dbs,
        store,
    })
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let applet = Path::new(args.first().map_or("", String::as_str))
        .file_name()
        .and_then(|n| n.to_str());
    match applet {
        Some("mount") => return applet_exit("mount", run_mount_applet(&args)),
        Some("flock") => return applet_exit("flock", run_flock_applet(&args)),
        _ => {}
    }
    // Builds run nicer than the loop's other work so a shared desktop stays smooth.
    // Scope to the build-executing subcommands; their spawned compilers inherit it.
    if matches!(args.get(1).map(String::as_str), Some("build" | "realize" | "autotools-build")) {
        nice_self_for_builds();
    }
    match args.get(1).map(String::as_str) {
        // S1 sentinel — the rung's run leg greps for this exact line.
        None => {
            println!("td-builder {} ok", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        // affected-checks — port of tools/affected-checks.sh (rust-migration C1):
        // map the branch diff to a right-sized check set + the waive/escalate
        // decision. Run from the repo root. See builder/src/affected.rs.
        Some("affected-checks") => affected::main(&args[2..]),
        // gate-run — td's OWN gate runner: the loop scheduler that replaced `make`
        // on the spine. The gates are compiled in (src/gate_defs/*.rs registry);
        // runs the requested tier/gates with cheap-serial + heavy-parallel
        // ordering, a MACHINE-WIDE flock slot pool (TD_CHECK_SLOTS, shared across
        // every concurrent check on the box), and data-driven longest-first heavy
        // order. Run from the repo root, inside the loop sandbox (`td-builder
        // check` execs it there). See builder/src/gates.rs.
        Some("gate-run") => gates::cli(args.get(2..).unwrap_or(&[])),
        // gate-body <name> — run one NATIVE (typed-Rust) gate body (#318 axis 3).
        // The runner execs this in place of `bash -c <script>` for a gate whose
        // GateDef.script is empty; see builder/src/gate_bodies.rs.
        Some("gate-body") if args.len() == 3 => gate_bodies::cli(&args[2]),
        // check [GOAL...] — the loop's HOST PRELUDE (the old shell check.sh,
        // ported): guards, stage0 + toolchain provisioning, warms, the shared
        // daemon, then the sandboxed gate-run. check.sh is now a guix-free cargo
        // bootstrap shim that execs this. (The drv reproducibility double-build
        // that used to share this verb is `check-drv` now — no argument sniffing.)
        Some("check") => check_loop::cli(args.get(2..).unwrap_or(&[])),
        // daily [--no-system] [--verdict FILE] — the daily-backstop runner: run the
        // full suite on fresh origin/main + write a machine verdict (was
        // ci/daily-full-suite.sh). See builder/src/daily.rs.
        Some("daily") => daily::cli(args.get(2..).unwrap_or(&[])),
        // check-rung HARNESS [ARGS...] — dev-iteration helper: run a cached-chain
        // bootstrap harness inside the loop sandbox (was tools/check-rung.sh).
        Some("check-rung") => check_loop::check_rung_cli(args.get(2..).unwrap_or(&[])),
        // Narrow td-owned replacements for the loop's pre-userland sed/grep/find
        // assertions and manifest shuffling. These are intentionally typed, not a
        // general regex tool clone.
        Some("text") => text_cli(args.get(2..).unwrap_or(&[])),
        Some("lock") => lock_cli(args.get(2..).unwrap_or(&[])),
        Some("files") => match args.get(2..).filter(|rest| !rest.is_empty()) {
            Some(rest) => match regular_files_under(rest) {
                Ok(files) => {
                    for f in files {
                        println!("{}", f.display());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: files: {e}");
                    ExitCode::FAILURE
                }
            }
            None => {
                eprintln!("usage: td-builder files PATH...");
                ExitCode::from(2)
            },
        },
        Some("files-name-first") => match (args.get(2), args.get(3..)) {
            (Some(pattern), Some(roots)) if !roots.is_empty() => {
                match first_file_named(pattern, roots) {
                    Ok(Some(p)) => {
                        println!("{}", p.display());
                        ExitCode::SUCCESS
                    }
                    Ok(None) => ExitCode::FAILURE,
                    Err(e) => {
                        eprintln!("td-builder: files-name-first: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("usage: td-builder files-name-first PATTERN PATH...");
                ExitCode::from(2)
            },
        },
        Some("tree-fingerprint") => match args.get(2..).filter(|roots| !roots.is_empty()) {
            Some(roots) => match tree_fingerprint(roots) {
                Ok(fp) => {
                    println!("{fp}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: tree-fingerprint: {e}");
                    ExitCode::FAILURE
                }
            }
            None => {
                eprintln!("usage: td-builder tree-fingerprint PATH...");
                ExitCode::from(2)
            },
        },
        Some("tree-contains") => match (args.get(2), args.get(3..)) {
            (Some(needle), Some(roots)) if !roots.is_empty() => match tree_first_containing(needle, roots) {
                Ok(Some(_)) => ExitCode::SUCCESS,
                Ok(None) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("td-builder: tree-contains: {e}");
                    ExitCode::FAILURE
                }
            },
            _ => {
                eprintln!("usage: td-builder tree-contains NEEDLE PATH...");
                ExitCode::from(2)
            },
        },
        Some("tree-not-contains") => match (args.get(2), args.get(3..)) {
            (Some(needle), Some(roots)) if !roots.is_empty() => match tree_first_containing(needle, roots) {
                Ok(Some(p)) => {
                    eprintln!("td-builder: tree-not-contains: {} contains {}", p.display(), needle);
                    ExitCode::FAILURE
                }
                Ok(None) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: tree-not-contains: {e}");
                    ExitCode::FAILURE
                }
            },
            _ => {
                eprintln!("usage: td-builder tree-not-contains NEEDLE PATH...");
                ExitCode::from(2)
            },
        },
        Some("tree-first-containing") => match (args.get(2), args.get(3..)) {
            (Some(needle), Some(roots)) if !roots.is_empty() => {
                match tree_first_containing(needle, roots) {
                    Ok(Some(p)) => {
                        println!("{}", p.display());
                        ExitCode::SUCCESS
                    }
                    Ok(None) => ExitCode::FAILURE,
                    Err(e) => {
                        eprintln!("td-builder: tree-first-containing: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            _ => {
                eprintln!("usage: td-builder tree-first-containing NEEDLE PATH...");
                ExitCode::from(2)
            },
        },
        Some("path-older-than") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(path), Some(days), None) => match path_older_than(path, days) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("td-builder: path-older-than: {e}");
                    ExitCode::FAILURE
                }
            }
            _ => {
                eprintln!("usage: td-builder path-older-than PATH DAYS");
                ExitCode::from(2)
            },
        },
        Some("daemon-budget-check") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(log), Some(budget), None) => match daemon_budget_check(log, budget) {
                Ok((peak, starts)) => {
                    println!("peak={peak} starts={starts}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-budget-check: {e}");
                    ExitCode::FAILURE
                }
            },
            _ => {
                eprintln!("usage: td-builder daemon-budget-check LOG BUDGET");
                ExitCode::from(2)
            },
        },
        // bootstrap-recipe <name> | --list — run a structured source-bootstrap rung
        // (the tests/bootstrap-*.sh drivers as typed Rust data; see bootstrap.rs).
        Some("bootstrap-recipe") => bootstrap::cli(&args),
        // toolchain-recipe <name> — build a /td/store toolchain rung as a structured Rust
        // recipe (see toolchain_x86_64.rs).
        Some("toolchain-recipe") => toolchain_x86_64::cli(&args),
        Some("nar-hash") if args.len() == 3 => match nar_hash(&args[2]) {
            Ok(h) => {
                println!("{h}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("td-builder: nar-hash {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // The inverse of nar-hash's serializer: restore NARFILE onto DEST (which must
        // not already exist). The read side of the codec the substitute consumer uses to
        // unpack a fetched NAR; strict — a truncated/garbled archive errors, never a
        // partial tree.
        Some("nar-restore") if args.len() == 4 => {
            let (narfile, dest) = (&args[2], &args[3]);
            let run = || -> std::io::Result<()> {
                let mut r = std::io::BufReader::new(std::fs::File::open(narfile)?);
                nar::read_nar(&mut r, Path::new(dest))
            };
            match run() {
                Ok(()) => {
                    println!("td-builder: restored {narfile} -> {dest}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: nar-restore {narfile} {dest}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // oci-image: pack a PREPARED rootfs directory into a deterministic, uncompressed
        // docker-archive (OCI image) — td-native, no guix/Guile (system-image-native brick
        // 1). CONFIG-JSON is {"repoTag","env":[],"entrypoint":[],"cmd":[]} (all optional;
        // repoTag defaults to td:latest). Usage: oci-image ROOTFS-DIR CONFIG-JSON OUT.tar
        Some("oci-image") if args.len() == 5 => {
            let (rootfs, config_file, out_file) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                let cfg_text = std::fs::read_to_string(config_file)
                    .map_err(|e| format!("read {config_file}: {e}"))?;
                let cj = json::parse(&cfg_text).map_err(|e| format!("config JSON: {e}"))?;
                let cfg = image_config_from_json(&cj);
                let mut w =
                    std::fs::File::create(out_file).map_err(|e| format!("create {out_file}: {e}"))?;
                oci::write_docker_archive(&mut w, Path::new(rootfs), &cfg)
                    .map_err(|e| format!("write docker-archive: {e}"))?;
                Ok(())
            };
            match run() {
                Ok(()) => {
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: oci-image: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // mkfs-erofs: pack a staged directory tree into a deterministic, uncompressed,
        // read-only **erofs** image (see erofs.rs). Control-plane capability for the
        // read-only-root arc (#548): the read-only `/td/store` real root the two-stage
        // boot mounts and switch_roots into. Never a recipe tool.
        // Usage: mkfs-erofs ROOTFS-DIR OUT.img
        Some("mkfs-erofs") if args.len() == 4 => {
            let (rootfs, out_file) = (&args[2], &args[3]);
            let run = || -> Result<(), String> {
                let img = erofs::build_image(Path::new(rootfs))
                    .map_err(|e| format!("build erofs image from {rootfs}: {e}"))?;
                std::fs::write(out_file, &img).map_err(|e| format!("write {out_file}: {e}"))?;
                Ok(())
            };
            match run() {
                Ok(()) => {
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: mkfs-erofs: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // oci-image-closure: the td-native replacement for `guix system image -t docker`.
        // Compute the store CLOSURE of ROOT… by CONTENT-SCANNING STORE-DIR (no /var/guix/db,
        // no guix process — scanForReferences == `guix gc -R` for an output root, gate 290),
        // lay each member at its STORE-DIR location into a single layer, and pack the
        // docker-archive. TD_STORE (env; the same td-owned-store concept realize_drv threads
        // as its `td_store` PARAMETER — build-plan passes it programmatically; this subcommand
        // is the only env reader of the name), when set,
        // names td's OWN store dir holding td-BUILT trees (the shared daemon cache): its
        // entries join the candidate index CANONICALIZED at STORE-DIR, are content-scanned
        // where their bytes lie, and are packed at their canonical names — so a td-built
        // root packs next to the guix-seed deps physically in STORE-DIR.
        // Usage: oci-image-closure STORE-DIR CONFIG-JSON OUT.tar ROOT...
        Some("oci-image-closure") if args.len() >= 6 => {
            let (store_dir, config_file, out_file) = (&args[2], &args[3], &args[4]);
            let roots = &args[5..];
            let run = || -> Result<usize, String> {
                let mut store_dirs = vec![store_dir.clone()];
                if let Some(ts) = std::env::var("TD_STORE").ok().filter(|s| !s.is_empty()) {
                    store_dirs.push(ts);
                }
                let (candidates, mut on_disk) = scan_candidate_index(&store_dirs, store_dir)?;
                let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                let empty = std::collections::HashMap::new();
                let mut closure_set: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for r in roots {
                    closure_set.extend(scan_closure_hybrid(
                        &mut scanner,
                        &on_disk,
                        &empty,
                        std::slice::from_ref(r),
                    )?);
                }
                // Pack each canonical member from where its bytes really live. A member in
                // NO scanned dir is a hole in the image — fail loud, never ship a
                // silently-incomplete closure.
                let mut members: Vec<(String, String)> = Vec::with_capacity(closure_set.len());
                for c in closure_set {
                    let od = on_disk.remove(&c).ok_or_else(|| {
                        format!(
                            "closure member {c} is on disk in none of the scanned store dir(s) {}",
                            store_dirs.join(", ")
                        )
                    })?;
                    members.push((c, od));
                }
                let n = members.len();
                let cfg_text = std::fs::read_to_string(config_file)
                    .map_err(|e| format!("read {config_file}: {e}"))?;
                let cj = json::parse(&cfg_text).map_err(|e| format!("config JSON: {e}"))?;
                let cfg = image_config_from_json(&cj);
                let mut w =
                    std::fs::File::create(out_file).map_err(|e| format!("create {out_file}: {e}"))?;
                oci::write_docker_archive_from_closure(
                    &mut w,
                    Path::new(store_dir),
                    &members,
                    &cfg,
                )
                .map_err(|e| format!("write docker-archive: {e}"))?;
                Ok(n)
            };
            match run() {
                Ok(n) => {
                    eprintln!("td-builder: oci-image-closure: packed {n} store paths");
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: oci-image-closure: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // oci-image-paths: pack a PRE-RESOLVED store closure into a docker-archive — like
        // oci-image-closure, but the closure is read from a PATHS FILE (one store path per
        // line) instead of walking /var/guix/db. Lets a caller keep closure RESOLUTION
        // wherever it already is (e.g. a guix-resolved input-resolution step, retired last)
        // while the image CONSTRUCTION is td-native, and td reads no guix private state.
        // Usage: oci-image-paths PATHS-FILE STORE-DIR CONFIG-JSON OUT.tar
        Some("oci-image-paths") if args.len() == 6 => {
            let (paths_file, store_dir, config_file, out_file) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<usize, String> {
                let text = std::fs::read_to_string(paths_file)
                    .map_err(|e| format!("read paths {paths_file}: {e}"))?;
                let mut closure: Vec<String> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect();
                closure.sort();
                closure.dedup();
                if closure.is_empty() {
                    return Err(format!("no store paths in {paths_file}"));
                }
                let n = closure.len();
                let cfg_text = std::fs::read_to_string(config_file)
                    .map_err(|e| format!("read {config_file}: {e}"))?;
                let cj = json::parse(&cfg_text).map_err(|e| format!("config JSON: {e}"))?;
                let cfg = image_config_from_json(&cj);
                let mut w =
                    std::fs::File::create(out_file).map_err(|e| format!("create {out_file}: {e}"))?;
                oci::write_docker_archive_from_store_paths(
                    &mut w,
                    Path::new(store_dir),
                    &closure,
                    &cfg,
                )
                .map_err(|e| format!("write docker-archive: {e}"))?;
                Ok(n)
            };
            match run() {
                Ok(n) => {
                    eprintln!("td-builder: oci-image-paths: packed {n} store paths");
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: oci-image-paths: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // subst-export: write a serve-able substitute directory for the closure of ROOT…
        // over DB's Refs graph — a `<basename>.narinfo` + `nar/<narhash>.nar` per member —
        // the store-coupled, dependency-free half of the substitute server. STORE-DIR is the
        // directory holding each path FLAT as `<basename>` — `/gnu/store` for the live store,
        // or a build's `newstore` (the same flat layout build_and_register / store-add-text
        // write). The networked subst/ binary signs + serves OUTDIR. Usage:
        //   subst-export DB STORE-DIR OUTDIR ROOT...
        Some("subst-export") if args.len() >= 6 => {
            // Optional leading `--paths`: export EXACTLY the roots (no closure walk) — the
            // per-output granularity the substitute consumer fetches. Default = whole closure.
            let paths_only = args.get(2).map(|s| s.as_str()) == Some("--paths");
            let off = if paths_only { 3 } else { 2 };
            let run = || -> Result<Vec<String>, String> {
                if args.len() < off + 4 {
                    return Err("usage: subst-export [--paths] DB STORE-DIR OUTDIR ROOT...".into());
                }
                let (db_path, store_dir, outdir) = (&args[off], &args[off + 1], &args[off + 2]);
                let roots = &args[off + 3..];
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                let members = subst_export_members(&db, store_dir, roots, !paths_only)?;
                subst_export(Path::new(outdir), &members).map_err(|e| e.to_string())
            };
            match run() {
                Ok(written) => {
                    println!(
                        "td-builder: subst-export wrote {} narinfo(s) + nars -> {}",
                        written.len(),
                        args.get(off + 2).map(|s| s.as_str()).unwrap_or("")
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: subst-export: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // harness-subst-export OUTDIR HARNESS-DIR — ship the whole /td/store harness tree
        // (.td-build-cache/harness: store/ + rel + toolchain) to a guix-less runner as ONE nar +
        // a fixed-name `td-harness.narinfo` (issue #314). The old daily publisher is retired;
        // keep the export helper dormant until the recipe-graph harness path has a current
        // producer again.
        Some("harness-subst-export") if args.len() == 4 => {
            let (outdir, harness_dir) = (&args[2], &args[3]);
            match harness_subst_export(Path::new(outdir), Path::new(harness_dir)) {
                Ok(written) => {
                    println!(
                        "td-builder: harness-subst-export wrote {} narinfo(s) + nar -> {outdir}",
                        written.len()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: harness-subst-export: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // S3a — parse the ATerm drv and print the canonical dump.
        Some("drv-parse") if args.len() == 3 => match std::fs::read(&args[2]) {
            Ok(bytes) => match drv::parse(&bytes) {
                Ok(d) => {
                    print!("{}", drv::dump(&d));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-parse {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("td-builder: drv-parse {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // evaluator-as-library (sub-task 2): round-trip a `.drv` — parse then
        // re-serialize — and exit 0 only if byte-identical to the file. Proves the
        // ATerm serializer matches the daemon's writer on a real derivation.
        Some("drv-roundtrip") if args.len() == 3 => match std::fs::read(&args[2]) {
            Ok(bytes) => match drv::parse(&bytes) {
                Ok(d) => {
                    let re = drv::serialize(&d);
                    if re.as_bytes() == bytes.as_slice() {
                        println!("OK {}", args[2]);
                        ExitCode::SUCCESS
                    } else {
                        eprintln!("DIFFER: re-serialized {} is not byte-identical", args[2]);
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("td-builder: drv-roundtrip {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("td-builder: drv-roundtrip {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // evaluator-as-library (sub-task 3): compute a `.drv`'s OWN store path
        // from its content + references (inputDrvs ∪ inputSrcs), the daemon's
        // makeTextPath. Prints the computed path; the rung compares it to the real
        // one. Proves nix-base32 + make-store-path match guix.
        Some("drv-path") if args.len() == 3 => {
            let file = &args[2];
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let name = store::name_from_store_path(file)
                    .ok_or_else(|| format!("{file} is not a store path"))?;
                let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                refs.extend(d.input_srcs.iter().cloned());
                Ok(store::drv_store_path(&name, &bytes, &refs))
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-path {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // drv-refs: print a `.drv`'s DIRECT references — the store paths folded into its own
        // content-addressed path (inputDrvs ∪ inputSrcs), the exact set `drv-path`/the daemon's
        // makeTextPath uses. Parsed from the `.drv` bytes (drv::parse), so it is guix-free and
        // needs no store DB / no `guix gc --references`. One path per line, sorted+deduped —
        // the reference list `store-add-referenced` folds back in. Usage: drv-refs FILE
        Some("drv-refs") if args.len() == 3 => {
            let file = &args[2];
            let run = || -> Result<Vec<String>, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                refs.extend(d.input_srcs.iter().cloned());
                refs.sort();
                refs.dedup();
                Ok(refs)
            };
            match run() {
                Ok(refs) => {
                    for r in &refs {
                        println!("{r}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-refs {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // evaluator-as-library (sub-task 4): compute output `out`'s store path via
        // the recursive hashDerivationModulo. Prints the computed path; the rung
        // compares it to the real one. Proves the modulo recursion matches guix.
        Some("drv-outpath") if args.len() == 3 => {
            let file = &args[2];
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let drv_name = store::name_from_store_path(file)
                    .and_then(|n| n.strip_suffix(".drv").map(str::to_string))
                    .ok_or_else(|| format!("{file} is not a .drv store path"))?;
                store::output_path(&d, &drv_name, "out", &read)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-outpath {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // evaluator-as-library (sub-task 5): CONSTRUCT the `.drv` from its skeleton
        // — recompute every output path + the `.drv`'s own store path + serialize —
        // and verify byte-identical (path AND content) to guix's. This is the
        // §6-named differential: identical `.drv` both ways, with guix the oracle.
        Some("drv-emit") if args.len() == 3 => {
            let file = &args[2];
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<(), String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let drv_name = store::name_from_store_path(file)
                    .and_then(|n| n.strip_suffix(".drv").map(str::to_string))
                    .ok_or_else(|| format!("{file} is not a .drv store path"))?;
                let (path, content) = store::construct_drv(&d, &drv_name, &read)?;
                let path_ok = path == *file;
                let content_ok = content.as_bytes() == bytes.as_slice();
                if path_ok && content_ok {
                    Ok(())
                } else {
                    Err(format!(
                        "DIFFER: store path {} (computed {path}); content {}",
                        if path_ok { "matches" } else { "MISMATCH" },
                        if content_ok { "matches" } else { "MISMATCH" },
                    ))
                }
            };
            match run() {
                Ok(()) => {
                    println!("OK {file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-emit {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-drv-build (sub-task 2): like drv-emit, but WRITE the constructed `.drv`
        // to OUT (so the td-builder executor can build it). Prints the computed store
        // path. The end-to-end rung then builds OUT in the td-builder sandbox.
        Some("drv-emit-to") if args.len() == 4 => {
            let (oracle, out_file) = (&args[2], &args[3]);
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(oracle).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let drv_name = store::name_from_store_path(oracle)
                    .and_then(|n| n.strip_suffix(".drv").map(str::to_string))
                    .ok_or_else(|| format!("{oracle} is not a .drv store path"))?;
                let (path, content) = store::construct_drv(&d, &drv_name, &read)?;
                std::fs::write(out_file, content.as_bytes()).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-emit-to {oracle}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: WRITE the store SQLite DB ourselves — the daemon's
        // `ValidPaths`/`Refs`/`DerivationOutputs` authority. td computes the
        // registration (NAR hash + size + reference scan, the same machinery `build`
        // uses) AND writes the SQLite file format directly (store_db, zero-dep) — the
        // real replacement of the daemon's libsqlite, no `sqlite3` engine. Usage:
        //   store-register STORE-PATH DERIVER CANDIDATES-FILE OUT-DB
        // CANDIDATES-FILE is STORE-PATH's full closure (`guix gc -R`). td registers
        // EVERY path in it — each fully scanned (real hash/size/refs) — plus all the
        // inter-path Refs and the deriver→output mapping. Only the deriver (a `.drv`,
        // not a closure member) is a scaffolding row so DerivationOutputs.drv resolves.
        // STORE-PATH carries its deriver; per-path derivers for the rest are the
        // daemon's input-resolution (a later increment). registrationTime is the
        // daemon's "now" — a fixed sentinel here, excluded from the differential.
        Some("store-register") if args.len() == 6 => {
            let (store_path, deriver, candidates_file, out_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<(), String> {
                use store_db::{Table, Value};
                // CANDIDATES-FILE is the artifact's full closure (`guix gc -R PATH`):
                // td registers EVERY path in it, each fully scanned — no placeholders.
                let closure: Vec<String> = std::fs::read_to_string(candidates_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                // Stable ids (= b-tree rowids), assigned ascending: the artifact = 1,
                // the deriver = 2, then the other closure paths in file order = 3.. .
                // Every reference is a closure member. The deriver `.drv` is ALWAYS id 2
                // (so DerivationOutputs.drv resolves); `others` excludes it so it is never
                // registered twice — the duplicate `ValidPaths` row that occurs when the
                // deriver IS itself a closure member (e.g. the rootless `img_drv`, which
                // is in its own `gc -R` set because it is bound into the staged store).
                let deriver_in_closure = closure.iter().any(|p| p.as_str() == deriver.as_str());
                let others: Vec<String> = closure
                    .iter()
                    .filter(|p| p.as_str() != store_path.as_str() && p.as_str() != deriver.as_str())
                    .cloned()
                    .collect();
                let id_of = |p: &str| -> Result<i64, String> {
                    if p == store_path.as_str() {
                        Ok(1)
                    } else if p == deriver.as_str() {
                        Ok(2)
                    } else {
                        others
                            .iter()
                            .position(|o| o.as_str() == p)
                            .map(|i| 3 + i as i64)
                            .ok_or_else(|| format!("reference `{p}' is not in the closure"))
                    }
                };
                // Scan one path; return its (hash, size, references) — the `build`
                // machinery, references found among the closure.
                let scan_path = |p: &str| -> Result<(String, u64, Vec<String>), String> {
                    let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                    nar::write_nar(&mut s, Path::new(p)).map_err(|e| e.to_string())?;
                    Ok(s.finish())
                };

                // ValidPaths rows in ascending rowid order; Refs accumulated per path.
                let mut valid: Vec<(i64, Vec<Value>)> = Vec::with_capacity(closure.len() + 1);
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut ref_rowid = 1i64;

                // id 1: the artifact, fully registered, with its deriver.
                let (a_hash, a_size, a_refs) = scan_path(store_path)?;
                valid.push((
                    1,
                    vec![
                        Value::Null, // id (integer primary key) — rowid is the id
                        Value::Text(store_path.to_string()),
                        Value::Text(a_hash),
                        Value::Int(1), // registrationTime (sentinel; excluded)
                        Value::Text(deriver.to_string()),
                        Value::Int(a_size as i64),
                    ],
                ));
                for r in &a_refs {
                    ref_rows.push((ref_rowid, vec![Value::Int(1), Value::Int(id_of(r)?)]));
                    ref_rowid += 1;
                }
                // id 2: the deriver. When it IS a closure member (the rootless case —
                // `img_drv` is bound into the staged store and the nested daemon reads it
                // to rebuild it for --check), register it FULLY (real hash/size/refs) so
                // the daemon accepts it as a valid path. Otherwise a scaffolding row (path
                // only) that exists solely so DerivationOutputs.drv resolves; the daemon
                // need not see the `.drv` as a valid built path in that case. Either way
                // id 2, so DerivationOutputs is unchanged.
                if deriver_in_closure {
                    let (d_hash, d_size, d_refs) = scan_path(deriver)?;
                    valid.push((
                        2,
                        vec![
                            Value::Null,
                            Value::Text(deriver.to_string()),
                            Value::Text(d_hash),
                            Value::Int(1),
                            Value::Null, // a `.drv` has no deriver of its own
                            Value::Int(d_size as i64),
                        ],
                    ));
                    for r in &d_refs {
                        ref_rows.push((ref_rowid, vec![Value::Int(2), Value::Int(id_of(r)?)]));
                        ref_rowid += 1;
                    }
                } else {
                    valid.push((
                        2,
                        vec![
                            Value::Null,
                            Value::Text(deriver.to_string()),
                            Value::Null,
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ],
                    ));
                }
                // ids 3..: the other closure paths, each fully registered (deriver NULL
                // — per-path derivers are the daemon's input-resolution, a later
                // increment; the differential is td's computed hash/size/refs).
                for p in &others {
                    let (hash, size, refs) = scan_path(p)?;
                    valid.push((
                        id_of(p)?,
                        vec![
                            Value::Null,
                            Value::Text(p.to_string()),
                            Value::Text(hash),
                            Value::Int(1),
                            Value::Null,
                            Value::Int(size as i64),
                        ],
                    ));
                    for r in &refs {
                        ref_rows.push((ref_rowid, vec![Value::Int(id_of(p)?), Value::Int(id_of(r)?)]));
                        ref_rowid += 1;
                    }
                }
                // DerivationOutputs: the deriver (id 2) → "out" → the artifact.
                let drv_out = vec![(
                    1i64,
                    vec![
                        Value::Int(2),
                        Value::Text("out".to_string()),
                        Value::Text(store_path.to_string()),
                    ],
                )];

                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                    Table {
                        name: "DerivationOutputs",
                        sql: "CREATE TABLE DerivationOutputs (drv integer, id text, path text)",
                        rows: drv_out,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: store-register {store_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: READ td's own store DB ourselves — the daemon's store-query
        // role, in pure Rust. `store_db_read` parses the SQLite file format that
        // `store-register` writes (no `sqlite3` engine, no daemon, in td's own
        // store-query path). Usage:
        //   store-query DB info            -> "path|hash|narSize" per fully-registered path
        //   store-query DB references      -> "referrer|reference" for the full Refs relation
        //   store-query DB references-only -> "reference" paths only
        //   store-query DB outputs         -> "outpath|deriver|drvpath|id" per DerivationOutputs row
        // All sorted, so a set-comparison against the daemon oracle is order-free.
        Some("store-query") if args.len() == 4 => {
            let (db_path, mode) = (&args[2], &args[3]);
            let run = || -> Result<Vec<String>, String> {
                use store_db_read::{Db, Value};
                let text = |v: &Value| match v {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                };
                let int = |v: &Value| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                };
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = Db::open(bytes)?;
                let mut out = match mode.as_str() {
                    // ValidPaths(id, path, hash, registrationTime, deriver, narSize):
                    // the path|hash|narSize of every fully-registered path (hash NOT NULL;
                    // a scaffolding row leaves hash/size NULL and is skipped).
                    "info" => {
                        let mut lines = Vec::new();
                        for (_rowid, cols) in db.table("ValidPaths")? {
                            match (text(&cols[1]), text(&cols[2]), int(&cols[5])) {
                                (Some(path), Some(hash), Some(size)) => {
                                    lines.push(format!("{path}|{hash}|{size}"));
                                }
                                _ => {}
                            }
                        }
                        lines
                    }
                    // Resolve Refs(referrer, reference) ids -> paths via the ValidPaths
                    // rowid (= the integer-primary-key id).
                    "references" | "references-only" => {
                        let mut path_of = std::collections::HashMap::new();
                        for (rowid, cols) in db.table("ValidPaths")? {
                            if let Some(p) = text(&cols[1]) {
                                path_of.insert(rowid, p);
                            }
                        }
                        let resolve = |id: i64| -> Result<String, String> {
                            path_of
                                .get(&id)
                                .cloned()
                                .ok_or_else(|| format!("Refs id {id} has no ValidPaths row"))
                        };
                        let mut lines = Vec::new();
                        for (_rowid, cols) in db.table("Refs")? {
                            match (int(&cols[0]), int(&cols[1])) {
                                (Some(a), Some(b)) => {
                                    if mode == "references-only" {
                                        lines.push(resolve(b)?);
                                    } else {
                                        lines.push(format!("{}|{}", resolve(a)?, resolve(b)?));
                                    }
                                }
                                _ => return Err("Refs row has non-integer columns".to_string()),
                            }
                        }
                        lines
                    }
                    // DerivationOutputs(drv, id, path): resolve `drv` (the referencing
                    // ValidPaths rowid) to the drv's OWN path, and the output's OWN
                    // registered deriver (ValidPaths.deriver where path=output) — the
                    // daemon's post-build deriver + drv->output registration, dumped for
                    // every output row (as `references` dumps every Refs edge).
                    "outputs" => {
                        let mut path_of = std::collections::HashMap::new();
                        let mut deriver_of = std::collections::HashMap::new();
                        for (rowid, cols) in db.table("ValidPaths")? {
                            if let Some(p) = cols.get(1).and_then(text) {
                                deriver_of.insert(p.clone(), cols.get(4).and_then(text).unwrap_or_default());
                                path_of.insert(rowid, p);
                            }
                        }
                        let mut lines = Vec::new();
                        for (_rowid, cols) in db.table("DerivationOutputs")? {
                            match (cols.first().and_then(int), cols.get(1).and_then(text), cols.get(2).and_then(text)) {
                                (Some(drv_id), Some(id), Some(outpath)) => {
                                    let drvpath = path_of.get(&drv_id).cloned().ok_or_else(|| {
                                        format!("DerivationOutputs drv {drv_id} has no ValidPaths row")
                                    })?;
                                    let deriver = deriver_of.get(&outpath).cloned().ok_or_else(|| {
                                        format!("DerivationOutputs path {outpath} has no ValidPaths row")
                                    })?;
                                    lines.push(format!("{outpath}|{deriver}|{drvpath}|{id}"));
                                }
                                _ => return Err("DerivationOutputs row has non-int/text columns".to_string()),
                            }
                        }
                        lines
                    }
                    other => {
                        return Err(format!("unknown query mode `{other}' (info|references|references-only|outputs)"))
                    }
                };
                out.sort();
                out.dedup();
                Ok(out)
            };
            match run() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-query {db_path} {mode}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: compute the GC-reachable CLOSURE of ROOT(s) by CONTENT-SCANNING a
        // store — the daemon's scanForReferences (scan.rs) recursed to fixpoint — with NO
        // store DB and NO guix process. STORE-DIR's entries are the candidate set and each
        // ROOT's NAR (read from STORE-DIR) is scanned for the candidates it references,
        // transitively; output paths are STORE-DIR/<basename>. This re-derives a closure
        // from the BYTES — the same set `store-closure` walks from the store DB (and the
        // same set `guix gc -R`/`--requisites` returns), computed without any DB or daemon.
        // STORE-DIR is EITHER a self-contained td-owned store (e.g. an unpacked seed) OR the
        // live /gnu/store: the candidate index is built once and reused across the walk
        // (see Scanner::reset), so even a ~500k-entry live store is fast. A match is a
        // 32-char hash literally present in the bytes — exactly the daemon's own reference
        // criterion, so scanning the live store cannot report a reference guix would not
        // (the store-closure-live gate proves == `guix gc -R`). STORE-DIR may be a
        // COMMA-SEPARATED list DIR1,DIR2,…: the candidate index then spans every listed
        // dir (a path's bytes are read from whichever dir holds them — matching is by 32-char
        // hash, not by prefix, so a member found under a non-canonical dir still resolves),
        // while the FIRST dir is the canonical prefix the ROOT paths use. This closes a
        // subject whose output tree lives in one store (e.g. a build scratch's `newstore`)
        // and whose deps live in another (the seed /gnu/store) in a SINGLE scan. Usage:
        //   store-closure-scan STORE-DIR[,EXTRA-DIR...] ROOT [ROOT...]
        // Prints the reachable store paths (canonical under the first dir), sorted (ROOTs incl).
        Some("store-closure-scan") if args.len() >= 4 => {
            let store_dirs: Vec<String> = args[2]
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            // The first (canonical) dir names the ROOT/candidate prefix; the rest are extra
            // byte sources merged into the index. A degenerate all-separator arg (e.g. ",")
            // yields no dirs → an empty prefix and an empty candidate set (the scan then just
            // echoes the roots) rather than a panic — callers pass real dirs.
            let canonical_prefix = store_dirs.first().cloned().unwrap_or_default();
            let roots: Vec<String> = args[3..].to_vec();
            let run = || -> Result<Vec<String>, String> {
                // Candidates = the store-path entries under every listed dir, keyed by 32-char
                // hash; the canonical prefix is the FIRST dir (a single-dir list keeps the
                // original "the dir IS the canonical location" behavior). BFS over CONTENT-
                // scanned refs to fixpoint (no store DB, no extra dbs): the shared
                // `scan_candidate_index` + `scan_closure_hybrid` — the same content-scan
                // realize_drv uses. Index built ONCE, reset() between paths, so even a
                // ~500k-entry live store is fast.
                let (candidates, on_disk) =
                    scan_candidate_index(&store_dirs, &canonical_prefix)?;
                let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                let empty = std::collections::HashMap::new();
                let seen = scan_closure_hybrid(&mut scanner, &on_disk, &empty, &roots)?;
                Ok(seen.into_iter().collect())
            };
            match run() {
                Ok(paths) => {
                    for p in &paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-closure-scan {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: compute the GC-reachable CLOSURE of one or more paths from td's
        // OWN store DB — the daemon's GC "mark" set (`guix gc -R ROOT` / the union
        // `guix gc --requisites ROOT…`), in pure Rust. Reads the DB with td's own
        // reader (`store_db_read`) and walks the Refs graph from each ROOT; no daemon.
        // Multiple ROOTs parse the DB once and union their closures. Usage:
        //   store-closure DB ROOT [ROOT...]
        // Prints the reachable store paths, sorted and deduped (every ROOT included).
        Some("store-closure") if args.len() >= 4 => {
            let db_path = &args[2];
            let roots: Vec<String> = args[3..].to_vec();
            let run = || -> Result<Vec<String>, String> {
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                db.closure_roots(&roots)
            };
            match run() {
                Ok(paths) => {
                    for p in paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-closure {db_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // seed-manifest: emit the MANIFEST for a seed closure — the capture half of the
        // frozen seed tarball (North-Star step 2). For the GC closure of ROOT…, print one
        // line per member: `<path> <nar-hash> <nar-size> <ref,ref,…>` (direct refs sorted;
        // `-` if none), all from td's OWN reader + NAR serializer (no daemon). The capture
        // tool tars the same closure; `seed-unpack` restores + registers from this manifest.
        //
        // SOURCE (arg 1) is EITHER a store DB FILE — closure + direct refs read from its Refs
        // graph (td's `store_db_read`) — OR a store DIRECTORY, in which case the closure and
        // every member's direct refs are computed by CONTENT-SCANNING the store bytes
        // (`scan_candidate_index` + `scan_closure_hybrid`, the same content-scan realize_drv
        // uses, == `guix gc -R`; gate 290) with NO store DB read at all. The dir form lets a
        // seed be captured with ZERO reads of guix's PRIVATE /var/guix/db (directive 8) — the
        // caller points it at the store dir the bytes live in (e.g. /gnu/store). Auto-detected
        // by whether SOURCE is a directory (a DB is always a file). Usage:
        //   seed-manifest DB-FILE-OR-STORE-DIR ROOT...
        Some("seed-manifest") if args.len() >= 4 => {
            let src = &args[2];
            let roots = &args[3..];
            // A closure member's `<path> <nar-hash> <nar-size> <refs>` line — refs sorted +
            // deduped, `-` when none (both branches emit the identical format).
            let manifest_line = |p: &str, hash: &str, size: u64, refs: &[String]| -> String {
                let mut rs: Vec<String> = refs.to_vec();
                rs.sort();
                rs.dedup();
                let refstr = if rs.is_empty() { "-".to_string() } else { rs.join(",") };
                format!("{p} {hash} {size} {refstr}")
            };
            let run = || -> Result<Vec<String>, String> {
                // STORE-DIR form: compute the closure + each member's direct refs by
                // content-scanning the store bytes — no store DB, no /var/guix/db, no daemon.
                if Path::new(src).is_dir() {
                    let store_dirs = std::slice::from_ref(src);
                    let (candidates, on_disk) = scan_candidate_index(store_dirs, src)?;
                    let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                    let empty = std::collections::HashMap::new();
                    // BFS the runtime closure over content-scanned refs (== guix gc -R).
                    let closure_set =
                        scan_closure_hybrid(&mut scanner, &on_disk, &empty, roots)?;
                    // Refs restricted to the (ref-closed) closure — a member's real direct
                    // refs are all closure members, so scanning against the closure set finds
                    // exactly them and drops nothing (superset-safe; matches the DB form).
                    let closure: Vec<String> = closure_set.iter().cloned().collect();
                    let mut lines = Vec::with_capacity(closure.len());
                    for p in &closure {
                        let od = on_disk.get(p).map(String::as_str).unwrap_or(p.as_str());
                        let mut s =
                            scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                        nar::write_nar(&mut s, Path::new(od))
                            .map_err(|e| format!("nar of {p} (at {od}): {e}"))?;
                        // finish() gives (nar-hash, nar-size, sorted refs) in the ONE pass —
                        // same "sha256:<base16>" hash + narSize the DB form's nar_hash_size_path emits.
                        let (hash, size, refs) = s.finish();
                        lines.push(manifest_line(p, &hash, size, &refs));
                    }
                    return Ok(lines);
                }
                // STORE-DB form: closure + direct refs from the DB's Refs graph.
                let bytes = std::fs::read(src).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                let mut closure = std::collections::BTreeSet::new();
                for r in roots {
                    for p in db.closure(r)? {
                        closure.insert(p);
                    }
                }
                let refs = db.refs_by_path()?;
                let mut lines = Vec::new();
                for p in &closure {
                    let (hash, size) =
                        nar_hash_size_path(Path::new(p)).map_err(|e| format!("nar of {p}: {e}"))?;
                    let rs: Vec<String> = refs.get(p).cloned().unwrap_or_default();
                    lines.push(manifest_line(p, &hash, size, &rs));
                }
                Ok(lines)
            };
            match run() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: seed-manifest: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // seed-unpack: RESTORE a frozen seed tarball into a td-owned store + register it
        // from the manifest — North-Star step 2, so the loop has the toolchain seed with NO
        // guix install. Extracts TARBALL into DEST-STORE (the canonical `/gnu/store/<base>`
        // trees land at `DEST-STORE/gnu/store/<base>`), VERIFIES each restored tree's NAR
        // hash equals the manifest (the seed survived the tarball, byte-for-byte), and writes
        // DEST-DB (ValidPaths + Refs) FROM the manifest — no re-scan (the live /gnu/store is
        // read-only in the loop), no daemon. Usage:
        //   seed-unpack TARBALL MANIFEST DEST-STORE DEST-DB
        Some("seed-unpack") if args.len() == 6 => {
            let (tarball, manifest, dest_store, dest_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<usize, String> {
                use store_db::{Table, Value};
                // Parse the manifest: `<path> <nar-hash> <nar-size> <ref,ref,…>`.
                let text = std::fs::read_to_string(manifest)
                    .map_err(|e| format!("read manifest {manifest}: {e}"))?;
                struct Entry {
                    path: String,
                    hash: String,
                    size: u64,
                    refs: Vec<String>,
                }
                let mut entries: Vec<Entry> = Vec::new();
                for (i, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let f: Vec<&str> = line.split(' ').collect();
                    if f.len() != 4 {
                        return Err(format!("manifest:{}: want `PATH HASH SIZE REFS', got `{line}'", i + 1));
                    }
                    let refs = if f[3] == "-" {
                        Vec::new()
                    } else {
                        f[3].split(',').map(str::to_string).collect()
                    };
                    entries.push(Entry {
                        path: f[0].to_string(),
                        hash: f[1].to_string(),
                        size: f[2].parse().map_err(|_| format!("manifest:{}: bad size", i + 1))?,
                        refs,
                    });
                }
                if entries.is_empty() {
                    return Err("manifest is empty".into());
                }
                // Extract the tar into DEST-STORE (its members are `gnu/store/<base>`).
                std::fs::create_dir_all(dest_store).map_err(|e| e.to_string())?;
                tar::extract_tar(Path::new(tarball), Path::new(dest_store))?;
                // Verify every restored tree is NAR-identical to the manifest.
                for e in &entries {
                    let on_disk = format!("{dest_store}{}", e.path); // DEST-STORE + /gnu/store/<base>
                    let got = nar_hash_path(Path::new(&on_disk))
                        .map_err(|err| format!("nar-hash {on_disk}: {err}"))?;
                    if got != e.hash {
                        return Err(format!(
                            "NAR mismatch after restore for {} (restored={got} manifest={})",
                            e.path, e.hash
                        ));
                    }
                }
                // Register DEST-DB from the manifest: rowids in manifest order, Refs by id.
                let id_of: std::collections::HashMap<&str, i64> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (e.path.as_str(), i as i64 + 1))
                    .collect();
                let mut valid: Vec<(i64, Vec<Value>)> = Vec::with_capacity(entries.len());
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut ref_rowid = 1i64;
                for e in &entries {
                    let id = id_of[e.path.as_str()];
                    valid.push((
                        id,
                        vec![
                            Value::Null,
                            Value::Text(e.path.clone()),
                            Value::Text(e.hash.clone()),
                            Value::Int(1), // registrationTime sentinel
                            Value::Null,   // deriver: a seed has none
                            Value::Int(e.size as i64),
                        ],
                    ));
                    for r in &e.refs {
                        let rid = *id_of.get(r.as_str()).ok_or_else(|| {
                            format!("reference `{r}' of {} is not in the manifest", e.path)
                        })?;
                        ref_rows.push((ref_rowid, vec![Value::Int(id), Value::Int(rid)]));
                        ref_rowid += 1;
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(dest_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                // BIND the db to the capture it restores (re #469 round-8): the
                // `<db>.seed-tarball` sidecar records the tarball's sha256, and
                // `authenticate_seed_capture_db` admits the db as AuditedSeed only
                // if that sha256 is pinned in seed/control-plane-seed-pins.txt.
                let tb_sha = sha256::sha256_file(Path::new(tarball))
                    .map_err(|e| format!("sha256 {tarball}: {e}"))?;
                let db_sha = sha256::sha256_file(Path::new(dest_db))
                    .map_err(|e| format!("sha256 {dest_db}: {e}"))?;
                let tb_base = tarball.rsplit('/').next().unwrap_or(tarball);
                std::fs::write(
                    format!("{dest_db}.seed-tarball"),
                    format!("sha256 {tb_sha} {tb_base}\ndb-sha256 {db_sha}\n"),
                )
                .map_err(|e| format!("write {dest_db}.seed-tarball: {e}"))?;
                Ok(entries.len())
            };
            match run() {
                Ok(n) => {
                    eprintln!("td-builder: seed-unpack restored + registered {n} seed paths (NAR-verified, no daemon)");
                    println!("{n}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: seed-unpack: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // gzip-decompress: expand a gzip member stream with td's std-only reader.
        Some("gzip-decompress") if args.len() == 4 => {
            let (gzip_file, out_file) = (&args[2], &args[3]);
            match gzip::decompress_file(Path::new(gzip_file))
                .and_then(|bytes| std::fs::write(out_file, bytes).map_err(|e| e.to_string()))
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: gzip-decompress: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // tar-extract / tar-gz-extract / tar-bz2-extract: extract POSIX tar
        // archives with td's std-only extractor and gzip/bzip2 readers. This is
        // intentionally small: enough for source seed archives and frozen seed
        // closure tars, without adding an unpacker dependency or requiring host
        // tar/gzip/bzip2.
        Some("tar-extract") if args.len() == 4 => {
            let (tarball, dest) = (&args[2], &args[3]);
            match tar::extract_tar(Path::new(tarball), Path::new(dest)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: tar-extract: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("tar-gz-extract") if args.len() == 4 => {
            let (tarball, dest) = (&args[2], &args[3]);
            match tar::extract_tar_gz(Path::new(tarball), Path::new(dest)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: tar-gz-extract: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("tar-bz2-extract") if args.len() == 4 => {
            let (tarball, dest) = (&args[2], &args[3]);
            match tar::extract_tar_bz2(Path::new(tarball), Path::new(dest)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: tar-bz2-extract: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: ADD a path to a td-OWNED store ourselves — the daemon's
        // addToStore (the WRITE side), in pure Rust. td computes the addTextToStore
        // path (`make_text_path`), WRITES the content into STORE-DIR as a canonical
        // store file (a regular, read-only 0444 file), and REGISTERS it in a td store
        // DB (`store_db`) — no daemon in the write path. NAR (hence the store path's
        // identity) ignores mtime and the read/write permission bits, so the
        // registration is metadata-independent. Usage:
        //   store-add-text NAME CONTENT-FILE STORE-DIR OUT-DB
        // Prints the store path. Flat/text case, no references — the recursive
        // directory case (canonical tree restore) is a later increment.
        Some("store-add-text") if args.len() == 6 => {
            let (name, content_file, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<String, String> {
                use std::os::unix::fs::PermissionsExt;
                use store_db::{Table, Value};
                let content = std::fs::read(content_file).map_err(|e| e.to_string())?;
                // td computes the addTextToStore path itself (no references).
                let path = store::make_text_path(name, &content, &[]);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Write the content into the td-owned store as a canonical store file:
                // a regular, world-readable, read-only (0444) file.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                std::fs::write(&disk, &content).map_err(|e| e.to_string())?;
                let mut perm =
                    std::fs::metadata(&disk).map_err(|e| e.to_string())?.permissions();
                perm.set_mode(0o444);
                std::fs::set_permissions(&disk, perm).map_err(|e| e.to_string())?;
                // Register it: NAR-hash + size of the file td just wrote (the `build`
                // machinery), references scanned among the single-path closure.
                let closure = vec![path.clone()];
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                let valid = vec![(
                    1i64,
                    vec![
                        Value::Null, // id (integer primary key) — rowid is the id
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1), // registrationTime (sentinel; excluded)
                        Value::Null,   // deriver — a source add has none
                        Value::Int(size as i64),
                    ],
                )];
                // A flat text add references nothing but (possibly) itself.
                let mut ref_rows = Vec::new();
                let mut rid = 1i64;
                for r in &refs {
                    if r == &path {
                        ref_rows.push((rid, vec![Value::Int(1), Value::Int(1)]));
                        rid += 1;
                    } else {
                        return Err(format!("unexpected reference {r} in a flat text add"));
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-text {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: ADD a DIRECTORY TREE to a td-OWNED store ourselves — the
        // RECURSIVE addToStore (the general write side), in pure Rust. td computes the
        // content-addressed `source` path from the recursive NAR hash
        // (`make_store_path("source", sha256(NAR), name)` — the daemon's
        // makeFixedOutputPath for recursive-sha256, no references), CANONICALLY restores
        // the tree into a td-owned store dir (`copy_canonical`: structure + contents +
        // exec bit + symlinks, the NAR-relevant properties), and REGISTERS it in a td
        // store DB (`store_db`). The registration MERGES into OUT-DB (a missing file is
        // the first intern): the runner interns MANY seeds into ONE db and passes that
        // db as build-plan's strict-provenance SEED-DB, so every interned seed must stay
        // vouched — a clobbering single-row write would silently un-vouch all earlier
        // seeds and red the first multi-seed rung (re #469). No daemon in the write
        // path. Usage:
        //   store-add-recursive NAME SRC STORE-DIR OUT-DB
        // Prints the store path. No-reference sources (this increment); referenced
        // sources are a later increment.
        Some("store-add-recursive") if args.len() == 6 => {
            let (name, src, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            match store_add_recursive(name, src, store_dir, out_db) {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-recursive {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // INPUT-ADDRESSED add: like store-add-recursive, but the store path's digest is
        // KEY (a hash of the artifact's DECLARED INPUTS — `toolchain-key`), NOT the tree's
        // recursive NAR hash. So a NON-byte-reproducible tree (the modern toolchain: cc1
        // stamp, ar/install mtimes) lands at a STABLE path: identical across rebuilds, and
        // computable from the lock BEFORE the build (the prereq for td-subst chain-caching).
        // The tree is still REGISTERED with its real NAR hash + size (naming and content-
        // integrity are orthogonal — the daemon's `output:` semantics), so closure/verify
        // are unchanged. Usage:
        //   store-add-input-addressed NAME KEY SRC STORE-DIR OUT-DB    (prints the path)
        Some("store-add-input-addressed") if args.len() == 7 => {
            let (name, key, src, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                // Input-addressed path: digest = KEY (declared inputs), not the content.
                let path = store::input_addressed_path(key, name);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed (bad KEY/NAME?)"))?
                    .to_string();
                // Canonically restore the tree into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(src), &disk)?;
                // Register the REAL NAR hash + size of the placed tree (self-references
                // among the single-path closure — the store-add-recursive registration).
                let closure = vec![path.clone()];
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                let valid = vec![(
                    1i64,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null, // deriver — set by the producer's drv when there is one
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows = Vec::new();
                let mut rid = 1i64;
                for r in &refs {
                    if r == &path {
                        ref_rows.push((rid, vec![Value::Int(1), Value::Int(1)]));
                        rid += 1;
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-input-addressed {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // bootstrap brick 2: PLACE a tree WITH references into a td-owned store,
        // content-addressed — the builder analog of store-add-recursive (which REFUSES a
        // referenced tree). td restores the tree into STORE-DIR, computes its
        // content-addressed `source` path from the recursive NAR, SCANS its references
        // against the SEED STORE DIRECTORY's entries (readdir via scan_candidate_index —
        // the pinned toolchain store holding the glibc/gcc-lib the stage0 builder links;
        // NO guix db read, #313), and registers the path + those refs in OUT-DB (each ref
        // a scaffolding ValidPaths row so the Refs join resolves — store-add-referenced's
        // external-ref shape). An ABSENT seed dir contributes no candidates: a guix-less
        // host's rustup/system-cc stage0 embeds no store paths, so the cold start records
        // an empty reference set. This lets the loop use a td-BOOTSTRAPPED builder (stage0,
        // NEVER produced by guix) as a recipe's builder-of-record: build-recipe reads its
        // closure as OUT-DB.closure(path) (the builder + its DIRECT refs) ∪ the seed
        // content-scan (those refs' transitive closures). No daemon, no guix.
        // PLACEMENT ONLY — this verb mints NO authority (re #469 round-10 P0 #2):
        // typing the placed tree `ControlPlaneBuilder` additionally requires the
        // stage0 LINEAGE record only `stage0-place` writes (verify_builder_lineage
        // at both BuilderOverride intakes), so registering an arbitrary
        // self-content-addressed tree here gains a caller nothing. Usage:
        //   store-add-builder NAME TREE STORE-DIR OUT-DB SEED-STORE-DIR  (prints the store path)
        Some("store-add-builder") if args.len() == 7 => {
            let (name, tree, store_dir, out_db, seed_store) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                // Content-addressed path from the tree's recursive NAR sha256 (same as
                // store-add-recursive — a `source`-type path).
                let nar = nar_hash(tree).map_err(|e| e.to_string())?;
                let hex = nar
                    .strip_prefix("sha256:")
                    .ok_or_else(|| format!("nar-hash returned `{nar}', expected sha256:<hex>"))?;
                let path = store::make_store_path("source", hex, name);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Canonically restore the tree into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(tree), &disk)?;
                // Scan the restored tree for references AGAINST the seed store DIRECTORY's
                // entries (the pinned toolchain store) — the builder's actual store deps,
                // with NO read of guix's private db (#313: a guix-less host cold-starts).
                // An ABSENT seed dir is legitimate (a guix-less host has no /gnu/store, so
                // the stage0 embeds no store refs and the placement records an empty ref
                // set). But a PRESENT-but-unreadable seed dir (a typo'd path, a regular
                // file, an EACCES mount) must FAIL LOUDLY, not be silently treated as
                // empty — a refless placement would poison the builder's closure and
                // surface only as an opaque exec/link failure at build time. So
                // distinguish NotFound (benign, no candidates) from any other read_dir
                // error here, restoring the loud failure the old sqlite seed read gave;
                // scan_candidate_index itself swallows both as "contributes nothing".
                match std::fs::read_dir(seed_store) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("seed store {seed_store}: {e}")),
                }
                // The path itself is a candidate so a self-reference is detected. Extra
                // never-matching candidates cannot add references (scan.rs candidate note).
                let (mut candidates, _on_disk) =
                    scan_candidate_index(std::slice::from_ref(seed_store), seed_store)?;
                candidates.push(path.clone());
                let mut s = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, mut refs) = s.finish();
                refs.sort();
                refs.dedup();
                // Register: id 1 = the builder (full record), each external reference a
                // scaffolding ValidPaths row (path only) so the Refs ids resolve. So
                // OUT-DB.closure(path) returns the builder + its DIRECT refs; realize then
                // spans those refs' transitive closures from the seed content-scan.
                let mut valid: Vec<(i64, Vec<Value>)> = vec![(
                    1,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null,
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut edge = 1i64;
                let mut next_id = 2i64;
                for r in &refs {
                    let target = if r == &path {
                        1 // a self-reference resolves to id 1
                    } else {
                        valid.push((
                            next_id,
                            vec![
                                Value::Null,
                                Value::Text(r.clone()),
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ],
                        ));
                        let id = next_id;
                        next_id += 1;
                        id
                    };
                    ref_rows.push((edge, vec![Value::Int(1), Value::Int(target)]));
                    edge += 1;
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-builder {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: ADD a path WITH REFERENCES to a td-owned store — the daemon's
        // addToStore/addTextToStore WITH a references set, in pure Rust. td computes the
        // content-addressed path with the references folded into the type
        // (`make_text_path`: `text:<sorted refs>` — the daemon's makeTextPath/makeType),
        // WRITES the content into a td-owned store dir (canonical 0444 file), and
        // REGISTERS it with its `Refs` to the referenced paths (each a scaffolding
        // ValidPaths row so the join resolves). No daemon. The canonical referenced
        // content-addressed item is a `.drv` (referenced by its input drvs/srcs). Usage:
        //   store-add-referenced NAME CONTENT-FILE REFS-FILE STORE-DIR OUT-DB
        // REFS-FILE lists the references (one store path per line). Prints the store path.
        Some("store-add-referenced") if args.len() == 7 => {
            let (name, content_file, refs_file, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use std::os::unix::fs::PermissionsExt;
                use store_db::{Table, Value};
                let content = std::fs::read(content_file).map_err(|e| e.to_string())?;
                let mut refs: Vec<String> = std::fs::read_to_string(refs_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                refs.sort();
                refs.dedup();
                // td computes the path with the references in the type (makeTextPath).
                let path = store::make_text_path(name, &content, &refs);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Write the content as a canonical (0444) store file.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                std::fs::write(&disk, &content).map_err(|e| e.to_string())?;
                let mut perm =
                    std::fs::metadata(&disk).map_err(|e| e.to_string())?.permissions();
                perm.set_mode(0o444);
                std::fs::set_permissions(&disk, perm).map_err(|e| e.to_string())?;
                // NAR hash + size of what td wrote (for the registration record).
                let mut s = scan::Scanner::new(&[path.clone()]).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, _) = s.finish();
                // Register: id 1 = the path (full), with its declared references; each
                // reference is a scaffolding ValidPaths row (path only) so Refs resolves.
                let mut valid: Vec<(i64, Vec<Value>)> = vec![(
                    1,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null,
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut edge = 1i64;
                let mut next_id = 2i64;
                for r in &refs {
                    let target = if r == &path {
                        1 // a self-reference resolves to id 1
                    } else {
                        valid.push((
                            next_id,
                            vec![
                                Value::Null,
                                Value::Text(r.clone()),
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ],
                        ));
                        let id = next_id;
                        next_id += 1;
                        id
                    };
                    ref_rows.push((edge, vec![Value::Int(1), Value::Int(target)]));
                    edge += 1;
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-referenced {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: a td STORE BACKEND for a BUILD OUTPUT — place a built output's
        // TREE into a td-owned store at its output path and FULLY REGISTER it (the
        // daemon's post-build registration: hash + narSize + deriver + the output's
        // references + the drv->output mapping), in pure Rust, no daemon. The result is
        // a td-owned store that HOLDS the build result and is served by td's own tools
        // (store-query / store-verify / store-closure). Usage:
        //   store-add-output OUTPUT DERIVER CLOSURE-FILE STORE-DIR OUT-DB
        // CLOSURE-FILE is OUTPUT's runtime closure (`guix gc -R`), used to scan
        // references. The output's tree is placed; its references are scaffolding rows.
        Some("store-add-output") if args.len() == 7 => {
            let (output, deriver, closure_file, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                let closure: Vec<String> = std::fs::read_to_string(closure_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                let base = output
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(output).is_some())
                    .ok_or_else(|| format!("output {output} is not a store path"))?
                    .to_string();
                // Place the output TREE canonically into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(output), &disk)?;
                // Scan the PLACED tree for its registration (hash + size + references
                // among the closure) — the `build` machinery.
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                // Register: id 1 = the OUTPUT (full, with its deriver); id 2 = the
                // deriver scaffold (so DerivationOutputs.drv resolves); ids 3.. = the
                // references (scaffold, path only). Refs: output -> each reference.
                let mut valid: Vec<(i64, Vec<Value>)> = vec![
                    (
                        1,
                        vec![
                            Value::Null,
                            Value::Text(output.to_string()),
                            Value::Text(hash),
                            Value::Int(1),
                            Value::Text(deriver.to_string()),
                            Value::Int(size as i64),
                        ],
                    ),
                    (
                        2,
                        vec![
                            Value::Null,
                            Value::Text(deriver.to_string()),
                            Value::Null,
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ],
                    ),
                ];
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut edge = 1i64;
                let mut next_id = 3i64;
                for r in &refs {
                    let target = if r == output {
                        1 // self-reference -> id 1
                    } else {
                        valid.push((
                            next_id,
                            vec![
                                Value::Null,
                                Value::Text(r.clone()),
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ],
                        ));
                        let id = next_id;
                        next_id += 1;
                        id
                    };
                    ref_rows.push((edge, vec![Value::Int(1), Value::Int(target)]));
                    edge += 1;
                }
                let drv_out = vec![(
                    1i64,
                    vec![Value::Int(2), Value::Text("out".to_string()), Value::Text(output.to_string())],
                )];
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                    Table {
                        name: "DerivationOutputs",
                        sql: "CREATE TABLE DerivationOutputs (drv integer, id text, path text)",
                        rows: drv_out,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(output.to_string())
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-output {output}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: COMMIT a finished build into a PERSISTENT td store — the
        // build-into half of an accumulating store+DB that survives across separate
        // `td-builder` invocations (vs the per-build scratch a one-shot build leaves).
        // Given a build SCRATCH (its `registration` + `newstore/<base>` trees, as
        // `build-recipe`/`realize` write), INTERN each output tree into STORE-DIR at
        // its basename (idempotent — a content path already present is a no-op) and
        // MERGE its registration into DB (the accumulating `ValidPaths`/`Refs`, via
        // `merge_output_db`) instead of clobbering. A later, SEPARATE invocation then
        // reads those outputs back out of STORE-DIR + DB (store-query/store-verify/
        // store-closure) — build-into / read-back across builds, no daemon. Usage:
        //   store-commit STORE-DIR DB SCRATCH
        Some("store-commit") if args.len() == 5 => {
            let (store_dir, db_path, scratch) = (&args[2], &args[3], &args[4]);
            let run =
                || commit_scratch_to_store(Path::new(scratch), store_dir, Path::new(db_path));
            match run() {
                Ok(paths) => {
                    for p in paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-commit: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // toolchain stable key: print the INPUT key of a td-toolchain.lock — sha256 over
        // its declared inputs (sources + patches + name + recipe-rev), order-independent.
        // The input-addressed toolchain path (toolchain-path) is named by this key, so it
        // is stable across non-reproducible rebuilds. Usage: toolchain-key LOCK
        Some("toolchain-key") if args.len() == 3 => {
            match std::fs::read_to_string(&args[2])
                .map_err(|e| e.to_string())
                .and_then(|c| store::ToolchainLock::parse(&c))
            {
                Ok(lock) => {
                    println!("{}", lock.key());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: toolchain-key {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // toolchain stable path: print the INPUT-ADDRESSED store path for a component of
        // the toolchain (or the toolchain itself when NAME is omitted), under the active
        // store_dir() (set TD_STORE_DIR=/td/store for the /td/store path). This is the
        // path the producer interns the built tree at (store-add-input-addressed) and the
        // path a td-subst consumer computes from the lock BEFORE fetching — the 2a stable key.
        // Usage: toolchain-path LOCK [NAME]
        Some("toolchain-path") if args.len() == 3 || args.len() == 4 => {
            let name = args.get(3).map(String::as_str);
            match std::fs::read_to_string(&args[2])
                .map_err(|e| e.to_string())
                .and_then(|c| store::ToolchainLock::parse(&c))
            {
                Ok(lock) => {
                    if let Some(n) = name {
                        if !lock.components.iter().any(|c| c == n) && n != lock.name {
                            eprintln!(
                                "td-builder: toolchain-path: `{n}` is not a component of {} (have: {})",
                                lock.name,
                                lock.components.join(", ")
                            );
                            return ExitCode::FAILURE;
                        }
                    }
                    println!("{}", lock.path_for(name));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: toolchain-path {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: VERIFY a td store's integrity ourselves — the daemon's
        // `guix gc --verify --check-contents`, in pure Rust. Reads the recorded
        // registration from a td store DB (`store_db_read`, #36), re-NAR-hashes each
        // registered path at STORE-ROOT/<basename>, and reports any path whose content
        // no longer matches its recorded `hash` (corruption / disk-rot). No daemon.
        // Usage:
        //   store-verify DB STORE-ROOT
        // STORE-ROOT holds the path bytes (e.g. /gnu/store, or a td-owned store dir).
        // Exit 0 if every registered path verifies; exit 1 (listing the mismatches) if
        // any content differs from its recorded hash.
        Some("store-verify") if args.len() == 4 => {
            let (db_path, store_root) = (&args[2], &args[3]);
            let run = || -> Result<Vec<String>, String> {
                use store_db_read::{Db, Value};
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = Db::open(bytes)?;
                let mut mismatches = Vec::new();
                let mut checked = 0u64;
                for (_rowid, cols) in db.table("ValidPaths")? {
                    // Only paths with a recorded hash (skip scaffolding rows).
                    let (path, recorded) = match (cols.get(1), cols.get(2)) {
                        (Some(Value::Text(p)), Some(Value::Text(h))) => (p, h),
                        _ => continue,
                    };
                    let base = path
                        .rsplit('/')
                        .next()
                        .ok_or_else(|| format!("malformed path {path}"))?;
                    let location = Path::new(store_root).join(base);
                    let got = nar_hash_path(&location).map_err(|e| format!("{}: {e}", location.display()))?;
                    checked += 1;
                    if &got != recorded {
                        mismatches.push(format!("{path}: recorded {recorded} got {got}"));
                    }
                }
                if checked == 0 {
                    Err("no registered paths with a recorded hash to verify".to_string())
                } else if mismatches.is_empty() {
                    Ok(vec![format!("verified {checked} paths")])
                } else {
                    Err(format!(
                        "{} of {checked} paths FAILED verification:\n{}",
                        mismatches.len(),
                        mismatches.join("\n")
                    ))
                }
            };
            match run() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-verify: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: the destructive GC SWEEP — the other half of GC (after the
        // mark/liveness `store-closure`, #39), in pure Rust. Given a td-owned store DIR,
        // its DB, and a GC ROOT, td computes the live set (closure of ROOT over Refs),
        // DELETES every registered content path NOT reachable from ROOT from STORE-DIR,
        // and rewrites the DB to the live set (ValidPaths + Refs renumbered). No daemon.
        // Boundary: operates ONLY on the given (td-owned) STORE-DIR/DB — NEVER the host
        // store. Usage:
        //   store-gc-sweep STORE-DIR DB ROOT
        // Prints how many paths were swept / remain.
        Some("store-gc-sweep") if args.len() == 5 => {
            let (store_dir, db_path, root) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<String, String> {
                use std::collections::{HashMap, HashSet};
                use store_db::{Table, Value as WV};
                use store_db_read::Value as RV;
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                let live: HashSet<String> = db.closure(root)?.into_iter().collect();
                let valid = db.table("ValidPaths")?;
                let refs = db.table("Refs")?;
                // old rowid -> path (to remap Refs after renumbering).
                let mut path_of: HashMap<i64, String> = HashMap::new();
                for (rid, cols) in &valid {
                    if let Some(RV::Text(p)) = cols.get(1) {
                        path_of.insert(*rid, p.clone());
                    }
                }
                // A registered content path = a row WITH a recorded hash (skip the
                // deriver scaffold). Keep the live ones; DELETE the dead ones' files.
                let mut survivors: Vec<&Vec<RV>> = Vec::new();
                let mut deleted = 0u64;
                for (_rid, cols) in &valid {
                    let path = match (cols.get(1), cols.get(2)) {
                        (Some(RV::Text(p)), Some(RV::Text(_))) => p,
                        _ => continue, // no hash -> scaffolding, not a content path
                    };
                    if live.contains(path) {
                        survivors.push(cols);
                    } else if let Some(base) = path.rsplit('/').next() {
                        let entry = Path::new(store_dir).join(base);
                        if entry.exists() {
                            if entry.is_dir() {
                                std::fs::remove_dir_all(&entry)
                                    .map_err(|e| format!("{}: {e}", entry.display()))?;
                            } else {
                                std::fs::remove_file(&entry)
                                    .map_err(|e| format!("{}: {e}", entry.display()))?;
                            }
                            deleted += 1;
                        }
                    }
                }
                // Renumber survivors 1..k by path; remap Refs among them.
                survivors.sort_by(|a, b| path_at(a).cmp(path_at(b)));
                let mut newid: HashMap<String, i64> = HashMap::new();
                let mut vrows: Vec<(i64, Vec<WV>)> = Vec::new();
                for (i, cols) in survivors.iter().enumerate() {
                    let nid = i as i64 + 1;
                    let path = path_at(cols).to_string();
                    newid.insert(path.clone(), nid);
                    let conv = |v: Option<&RV>| -> WV {
                        match v {
                            Some(RV::Int(n)) => WV::Int(*n),
                            Some(RV::Text(s)) => WV::Text(s.clone()),
                            _ => WV::Null,
                        }
                    };
                    vrows.push((
                        nid,
                        vec![
                            WV::Null,
                            WV::Text(path),
                            conv(cols.get(2)), // hash
                            conv(cols.get(3)), // registrationTime
                            conv(cols.get(4)), // deriver
                            conv(cols.get(5)), // narSize
                        ],
                    ));
                }
                let mut rrows: Vec<(i64, Vec<WV>)> = Vec::new();
                let mut rid = 1i64;
                for (_r, cols) in &refs {
                    let (a, b) = match (cols.first(), cols.get(1)) {
                        (Some(RV::Int(a)), Some(RV::Int(b))) => (*a, *b),
                        _ => continue,
                    };
                    if let (Some(pa), Some(pb)) = (path_of.get(&a), path_of.get(&b)) {
                        if let (Some(&na), Some(&nb)) = (newid.get(pa), newid.get(pb)) {
                            rrows.push((rid, vec![WV::Int(na), WV::Int(nb)]));
                            rid += 1;
                        }
                    }
                }
                // The swept DB carries the live ValidPaths + Refs only; the deriver
                // scaffold and DerivationOutputs are intentionally not carried (a swept
                // store is content + references — the build-derivation mapping is rebuilt
                // by registration, not GC).
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: vrows,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: rrows,
                    },
                ];
                std::fs::write(db_path, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(format!("swept {deleted} dead paths, {} live remain", newid.len()))
            };
            match run() {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-gc-sweep: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // The generic `build DRV CLOSURE SCRATCH` and `realize DRV STORE-DIR SCRATCH`
        // arms are DELETED (re #469 typed origins): both took their entire staging
        // manifest from the caller-writable TD_EXTRA_DBS env — self-issued authority
        // the reviewer's finding named — and no gate, test, or tool invoked either.
        // Every production realize path is a TYPED planner site now: `build-plan
        // --auto` (compiled-digest-gated seed db + step td.dbs), `build-recipe`
        // (seed db + `--recipe-output-db` argv), and the daemon children (the
        // blessed seed-closure db at its derived location).
        // td-builder daemon — td's OWN persistent build daemon: a long-running
        // process that realizes derivations served over a Unix socket, instead of
        // guix-daemon (own-builder-daemon track). Each request realizes via the
        // exact `realize_drv` path (same sandbox/NEWPID, daemon-free) into a
        // CONTENT-ADDRESSED per-drv scratch dir under SCRATCH-BASE — keyed by the
        // drv's (content-addressed) output basename, so a repeat request for the
        // SAME drv reuses + NAR-verifies a valid prior realization (cached_realization)
        // instead of rebuilding (guix-daemon parity: a valid path is not rebuilt).
        // The response carries the realized output's canonical store path + its
        // host-side path under that scratch.
        //
        // Optional td-OWNED builder (same convention as build-recipe, bootstrap brick 2):
        // with TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB set together, a drv whose
        // builder is td's stage0 td-builder (a binary guix never produced) is realized by
        // staging that builder from TD_BUILDER_STORE with its direct refs from
        // TD_BUILDER_DB — so the daemon builds with td's OWN builder, not the guix-built
        // one, and needs no new `guix build -e' packager site. The override is matched by
        // PATH (only the drv root equal to its canonical is re-keyed), so it is a harmless
        // no-op for a drv that does not name the stage0 (e.g. the guile probes of gate 358).
        // The triple is LINEAGE-verified at intake (re #469 round-10): a per-request
        // TD_BUILDER_* naming a tree `stage0-place` never produced fails closed, and
        // the children's realize enforces the staging-boundary host-tool policy
        // (enforce_realize_input_policy) — an arbitrary drv over the socket can no
        // longer select blessed host tools as inputs or name one as its builder.
        // Usage:  daemon SOCKET STORE-DIR SCRATCH-BASE
        // The blessed seed-closure db is never an argument: build children
        // derive its location from the repo's seed-lock declarations
        // (re #469 round-8 origin authentication).
        // td-builder stage0-place — compile + place the guix-free stage0 td-builder
        // under BASEDIR and print its canonical store path (the one entry point every
        // stage0 consumer goes through; the Rust port of tests/stage0-builder.sh —
        // the setup path runs no ambient host sh, re #469). Memoized on the builder
        // source fingerprint; see stage0::stage0_place.
        // Usage: stage0-place BASEDIR   (cwd must be the repo root)
        Some("stage0-place") if args.len() == 3 => {
            let run = || -> Result<String, String> {
                let root = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
                stage0::stage0_place(&root, Path::new(&args[2]))
            };
            match run() {
                Ok(cb) => {
                    println!("{cb}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: stage0-place: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder provision-rust / provision-cc — resolve the SEED build's
        // guix-free toolchain and print a PATH fragment (the Rust port of
        // tools/provision-{rust,cc}.sh; resolution order in stage0.rs). Exit 3 when
        // nothing resolves — operator guidance, distinct from a hard failure.
        Some("provision-rust") if args.len() == 2 => {
            let run = || -> Result<String, String> {
                let root = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
                stage0::provision_rust(&stage0::ProvisionEnv::from_env(&root))
            };
            match run() {
                Ok(frag) => {
                    println!("{frag}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: provision-rust: {e}");
                    ExitCode::from(3)
                }
            }
        }
        Some("provision-cc") if args.len() == 2 => {
            let run = || -> Result<String, String> {
                let root = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
                stage0::provision_cc(&stage0::ProvisionEnv::from_env(&root))
            };
            match run() {
                Ok(frag) => {
                    println!("{frag}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: provision-cc: {e}");
                    ExitCode::from(3)
                }
            }
        }
        // td-builder provision-glibc-static — resolve the MATCHED static glibc's
        // `lib/` dir (holding `libc.a`) for a crt-static control-plane build. The
        // shell recipe-eval-tool.sh consumes it as the RUSTFLAGS `-L` so the
        // evaluator links statically (no host/guix-home runpath). Same
        // operator-guidance contract as provision-{rust,cc}: exit 3 when nothing
        // resolves (set TD_GLIBC_STATIC_HOME, a build-essential cc, or a lock pin).
        Some("provision-glibc-static") if args.len() == 2 => {
            let run = || -> Result<String, String> {
                let root = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
                stage0::provision_glibc_static(&stage0::ProvisionEnv::from_env(&root))
            };
            match run() {
                Ok(frag) => {
                    println!("{frag}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: provision-glibc-static: {e}");
                    ExitCode::from(3)
                }
            }
        }
        // td-builder assert-static PATH — verify a host-built control-plane binary
        // is FULLY static: no PT_INTERP, no DT_NEEDED, no run-path. The no-leakage
        // invariant enforced at every host build site (re #469) so a dynamically
        // linked control-plane tool can never drag its host glibc/libgcc closure —
        // or a mutable guix-home runpath — into a build. Used by
        // recipe-eval-tool.sh to fail closed if a toolchain silently linked the
        // evaluator dynamically.
        Some("assert-static") if args.len() == 3 => match elf::assert_static(Path::new(&args[2])) {
            Ok(()) => {
                println!("static ok: {}", args[2]);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("td-builder: assert-static: {e}");
                ExitCode::FAILURE
            }
        },
        // td-builder seed-bless — record the DECLARED seed closure's hashes into a
        // td-owned store db (re #469): derive the roots ITSELF from the repo's
        // checked-in seed-lock declarations (`seed_lock_roots` over the cwd — the
        // caller cannot select them), content-scan SEED-DIR for their transitive
        // closure, and fully register every member (path/hash/size/refs). The db is
        // the strict staging manifest's authority for the daemon flow
        // (`ensure_build_daemon` blesses once per root set into the DERIVED
        // `blessed_seed_db_path` location the build children re-derive). Bless
        // ONCE per root set — see
        // `bless_seed_closure` on why re-blessing would defeat the gate. A verb that
        // blessed arbitrary caller-supplied roots would mint manifest authority for
        // whatever bytes occupy those paths, exactly the self-issued-provenance hole
        // typed origins close.
        // Usage: seed-bless SEED-DIR OUT-DB   (cwd must be the repo root)
        Some("seed-bless") if args.len() == 4 => {
            let (seed_dir, out_db) = (&args[2], &args[3]);
            let run = || -> Result<usize, String> {
                let root = std::env::current_dir().map_err(|e| format!("getcwd: {e}"))?;
                let roots = check_loop::seed_lock_roots(&root);
                bless_seed_closure(seed_dir, &roots, Path::new(out_db))
            };
            match run() {
                Ok(n) => {
                    println!("blessed {n} paths");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: seed-bless: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("daemon") if args.len() == 5 => {
            let (socket, seed_dir, scratch) = (args[2].clone(), args[3].clone(), args[4].clone());
            // The blessed seed-closure db is NOT an argument (re #469 round-8):
            // each build child RE-DERIVES its location from the repo's own
            // seed-lock declarations (`check_loop::blessed_seed_db_path` over
            // the child's cwd + its request's seed dir), so no caller-selected
            // path can be typed `BlessedSeedClosure`.
            // Fail fast on a half-set builder override (the children re-read the same env).
            if let Err(e) = builder_override_from_env() {
                eprintln!("td-builder: daemon: {e}");
                return ExitCode::FAILURE;
            }
            let _ = std::fs::create_dir_all(&scratch);
            let budget = daemon_budget();
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("td-builder: daemon: current_exe: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Per-key dedup: concurrent requests for the SAME output serialize on one lock,
            // so the drv builds once (the 2nd cache-hits) and two builds never race the same
            // content-addressed scratch — the guix-daemon "a valid path is built once"
            // property, preserved across concurrency and across agents (one shared daemon).
            let keymap: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>> =
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            let handle = move |req: &str| -> Result<String, String> {
                // Request grammar: "<drv> [SEED-DIR BP BS BD]" (build) or "CHECK <drv> [SEED-DIR
                // BP BS BD]" (reproducibility). The optional trailing fields are the SEED store
                // DIR (content-scanned for the input closure — #267 retired the /var/guix/db read)
                // and the td-owned builder override (TD_BUILDER_PATH/STORE/DB). Both are carried
                // PER REQUEST because ONE shared daemon serves many worktrees: each declares the
                // seed store dir its inputs come from and the stage0 builder its drv names (bound
                // at identical absolute paths in every sandbox, so the daemon on the host opens
                // exactly what the submitter names). Absent → the child uses the daemon's own
                // start-time seed dir + inherited env (gates 358/359 pass a bare drv).
                let mut toks = req.split_whitespace();
                let first = toks.next().ok_or_else(|| "empty request".to_string())?;
                let (sub, drv) = if first == "CHECK" {
                    ("daemon-check", toks.next().ok_or_else(|| "CHECK: missing drv".to_string())?)
                } else {
                    ("daemon-build", first)
                };
                let rest: Vec<&str> = toks.collect();
                let (seed_dir_req, override_env): (String, Vec<(&str, &str)>) = match rest.as_slice() {
                    [sdb, bp, bs, bd] => (
                        (*sdb).to_string(),
                        vec![
                            ("TD_BUILDER_PATH", *bp),
                            ("TD_BUILDER_STORE", *bs),
                            ("TD_BUILDER_DB", *bd),
                        ],
                    ),
                    [] => (seed_dir.clone(), Vec::new()),
                    _ => {
                        return Err(format!(
                            "malformed request (expected DRV [SEED-DIR BUILDER_PATH BUILDER_STORE BUILDER_DB]): {req}"
                        ))
                    }
                };
                let key = drv_scratch_key(drv)?;
                let keylock = {
                    let mut m = keymap.lock().unwrap();
                    m.entry(key)
                        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
                        .clone()
                };
                let _kg = keylock.lock().unwrap();
                // Each build runs in its OWN child td-builder process (Command = the safe
                // fork+exec): an in-process fork on a daemon thread is unsound (sandbox::build
                // mutates the process CWD + forks with heavy pre-exec work). The child's stderr
                // is inherited so the daemon log keeps the CACHE HIT/MISS lines (gate
                // daemon-recipe greps them).
                let mut cmd = Command::new(&exe);
                cmd.arg(sub)
                    .arg(drv)
                    .arg(&seed_dir_req)
                    .arg(&scratch)
                    .stderr(std::process::Stdio::inherit());
                // No bless-db argument: the child derives the blessed
                // seed-closure db's location itself (re #469 round-8).
                for (k, v) in &override_env {
                    cmd.env(k, v);
                }
                let out = cmd
                    .output()
                    .map_err(|e| format!("spawn {sub} for {drv}: {e}"))?;
                if !out.status.success() {
                    return Err(format!("{sub} failed for {drv} (see daemon log)"));
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout
                    .lines()
                    .find_map(|l| l.strip_prefix("OK "))
                    .map(str::to_string)
                    .ok_or_else(|| format!("{sub}: no OK line for {drv}"))
            };
            // Reserve free memory before admitting a build — the global OOM guard on this
            // swapless host, shared by every daemon via /proc/meminfo (bounds machine-wide
            // memory even when per-binary daemons fragment the concurrency budget).
            let min_free_gib = std::env::var("TD_MIN_FREE_GIB")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(4.0);
            match build_daemon::serve(&socket, budget, min_free_gib, handle) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: daemon: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder daemon-build / daemon-check — the per-build CHILD processes the daemon
        // spawns (one build per process = safe fork + full isolation). daemon-build realizes
        // one drv into a content-addressed keyed scratch (guix-daemon-parity cache reuse) and
        // prints `OK <canonical> <host>`; daemon-check reproducibility-double-builds it and
        // prints `OK repro <canonical> <host>`. Both read the td-owned builder from
        // TD_BUILDER_* (inherited from the daemon). The blessed seed-closure db is
        // NOT an argument: the child DERIVES its location from the repo's own
        // seed-lock declarations (cwd + the request's seed dir) — a caller-selected
        // db path can no longer be typed `BlessedSeedClosure` (re #469 round-8).
        // Usage: daemon-build|daemon-check DRV SEED-DIR SCRATCH-BASE
        Some("daemon-build") if args.len() == 5 => {
            let bless = match derived_bless_db(&args[3]) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("td-builder: daemon-build {}: {e}", args[2]);
                    return ExitCode::FAILURE;
                }
            };
            match daemon_realize_one(
                &args[2],
                &args[3],
                Path::new(&args[4]),
                bless.as_deref(),
            ) {
                Ok((canon, host, hit)) => {
                    println!("OK {canon} {host} {}", if hit { "hit" } else { "built" });
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-build {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        Some("daemon-check") if args.len() == 5 => {
            let bless = match derived_bless_db(&args[3]) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("td-builder: daemon-check {}: {e}", args[2]);
                    return ExitCode::FAILURE;
                }
            };
            match daemon_check_one(
                &args[2],
                &args[3],
                Path::new(&args[4]),
                bless.as_deref(),
            ) {
                Ok((canon, host)) => {
                    println!("OK repro {canon} {host}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-check {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder daemon-request — the in-process client for `daemon` (so a
        // caller needs no nc/socat): connect to SOCKET, send REQUEST, print the daemon's
        // single-line response and exit 0 only on "OK …". REQUEST is a drv path (build),
        // "CHECK <drv>" (reproducibility double-build), or "SHUTDOWN". Usage:
        //   daemon-request SOCKET REQUEST
        Some("daemon-request") if args.len() == 4 => {
            let (socket, drv) = (&args[2], &args[3]);
            match build_daemon::request(socket, drv) {
                Ok(resp) => {
                    println!("{resp}");
                    if resp.starts_with("OK ") {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-request: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder assemble-recipe — ASSEMBLE a recipe's `.drv` with NO Guile and
        // NO realize (own-builder-daemon §5): read RECIPE-JSON + LOCK and assemble the
        // `.drv` (store::assemble_drv) to SCRATCH/<name>-<version>.drv, WITHOUT building
        // it — so a SEPARATE process (the persistent build daemon) realizes the
        // td-assembled drv. The drv's builder is the STABLE ABI-token identity path
        // (store::builder_identity_path), independent of which td-builder binary
        // assembles or realizes it; the realizing process binds the real builder (its
        // TD_BUILDER_* override, else its own binary) at that path. Prints `DRV=<file>`
        // then one `OUT=<name> <store-path>` per output. Usage:
        //   assemble-recipe RECIPE-JSON-FILE LOCK SCRATCH
        Some("assemble-recipe") if args.len() == 5 => {
            let (recipe_file, lock, scratch) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                let recipe_json =
                    std::fs::read_to_string(recipe_file).map_err(|e| e.to_string())?;
                let (drv_path, drv_file, parsed, _source) =
                    assemble_recipe_drv(&recipe_json, lock, Path::new(scratch), None)?;
                eprintln!(
                    "td-builder: assemble-recipe assembled {drv_path} (no guix (derivation), no Guile, no realize)"
                );
                println!("DRV={}", drv_file.display());
                for o in &parsed.outputs {
                    println!("OUT={} {}", o.name, o.path);
                }
                Ok(())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: assemble-recipe {recipe_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder build-recipe — build a TS recipe with NO Guile and NO
        // guix-daemon in the path: read the recipe JSON (ts-eval produced it,
        // Guile-free), resolve every input from the pinned LOCK (no
        // specification->package), assemble the .drv itself, and realize it. The
        // toolchain + lock are the guix-built SEED (§5, retired last). The optional
        // trailing SRC-STORE-DIR + SRC-DB make the `<name>-source` a td-OWNED source
        // (interned by td's store-add-recursive, no `guix repl`); omitted → a
        // daemon-resident source path, as before. STORE-DIR is the seed store DIRECTORY
        // whose bytes the input closure is CONTENT-SCANNED over (no /var/guix/db). Usage:
        //   build-recipe RECIPE-JSON-FILE LOCK SCRATCH STORE-DIR [SRC-STORE-DIR SRC-DB]
        //     [--recipe-output-store STORE] [--recipe-output-db DB]...
        Some("build-recipe") if args.len() >= 6 => {
            // Trailing repeatable `--recipe-output-db DB` pairs: ADDITIONAL td-OWNED
            // store DBs of PRIOR td recipe outputs (e.g. the /td/store native
            // toolchain's own db, brick 8) — an explicit, typed argv channel; their
            // rows join the staging manifest as `RecipeOutput`. This replaces the
            // deleted TD_EXTRA_DBS env channel: manifest authority arrives only as a
            // declared argument, never ambiently (re #469 typed origins).
            let options = match parse_recipe_output_options(&args) {
                Ok(options) => options,
                Err(e) => {
                    eprintln!("td-builder: build-recipe: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let pos_len = options.positional_len;
            let recipe_output_dbs = options.dbs;
            let recipe_output_store = options.store;
            if !(pos_len == 6 || pos_len == 8 || pos_len == 11) {
                eprintln!(
                    "td-builder: build-recipe: usage: build-recipe RECIPE-JSON-FILE LOCK \
                     SCRATCH STORE-DIR [SRC-STORE-DIR SRC-DB [VENDOR-CANONICAL VENDOR-STORE \
                     VENDOR-DB]] [--recipe-output-store STORE] [--recipe-output-db DB]..."
                );
                return ExitCode::FAILURE;
            }
            let (recipe_file, lock, scratch, store_dir) =
                (&args[2], &args[3], &args[4], &args[5]);
            let src_store = if pos_len >= 8 {
                Some((args[6].as_str(), args[7].as_str()))
            } else {
                None
            };
            // Optional td-OWNED vendored-crate tree (the guix-free crate path): td interned
            // the crate SET itself (store-add-recursive). VENDOR-CANONICAL is its store path,
            // VENDOR-STORE the td store dir, VENDOR-DB its db. run_rust vendors from it
            // (TD_VENDOR_DIR) — no `/gnu/store` crate, no guix-daemon FOD.
            //   build-recipe RECIPE LOCK SCRATCH STORE-DB [SRC-STORE SRC-DB [VENDOR-CANONICAL VENDOR-STORE VENDOR-DB]]
            let vendor_store = if pos_len == 11 {
                Some((args[8].as_str(), args[9].as_str(), args[10].as_str()))
            } else {
                None
            };
            // Optional td-OWNED builder (bootstrap brick 2): all three env vars set
            // together → the drv's builder is a td-placed stage0 (store-add-builder),
            // not the running guix-built binary. TD_BUILDER_PATH is its canonical store
            // path, TD_BUILDER_STORE the td store dir it was restored under, and
            // TD_BUILDER_DB the db registering it + its direct refs.
            let bp = std::env::var("TD_BUILDER_PATH").ok();
            let bs = std::env::var("TD_BUILDER_STORE").ok();
            let bd = std::env::var("TD_BUILDER_DB").ok();
            // North-Star step 2: build from the UNPACKED SEED, not a host guix. With
            // TD_SEED_STORE + TD_SEED_DB set (a `td-builder seed-unpack` output), the input
            // closure is CONTENT-SCANNED from the unpacked seed store and every seed input
            // binds from it (TD_SEED_STORE/<base>) — so STORE-DIR and the live /gnu/store are
            // out of the build path. Set together; the build is otherwise identical (same drv,
            // same output). TD_SEED_DB is the seed's VOUCHING db: the content-scan of
            // TD_SEED_STORE supplies the closure's edges, and the db's registrations are what
            // admit its items into the strict staging manifest (re #469).
            let seed_store = std::env::var("TD_SEED_STORE").ok();
            let seed_db = std::env::var("TD_SEED_DB").ok();
            // Optional PERSISTENT store (the incremental /td/store the loop builds into):
            // set TD_PERSIST_STORE + TD_PERSIST_DB together and the build reads an
            // already-built output back from there (skip) or, on a miss, commits its fresh
            // output into it (build-into) — build-into / read-back across invocations.
            let run = || -> Result<(), String> {
                // Parsed by the shared persist_store_env helper (same set-together
                // convention as the build-plan dispatch); `?` defers the partial-set
                // error into run() exactly as the prior inline match did.
                let pov = persist_store_env()?;
                let persist = pov.as_ref().map(|(s, d)| (s.as_str(), d.as_str()));
                let builder_store = match (&bp, &bs, &bd) {
                    (Some(p), Some(s), Some(d)) => Some((p.as_str(), s.as_str(), d.as_str())),
                    (None, None, None) => None,
                    _ => {
                        return Err(
                            "TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together"
                                .into(),
                        )
                    }
                };
                // The seed staging dir's entries are guix-captured bytes whose canonical
                // home is /gnu/store even when the BUILD targets TD_STORE_DIR=/td/store
                // (#292 — gate 377's collapse); td-built copies inside it are restored to
                // their /td/store canonicals from the roots + the typed recipe-output dbs.
                // Without a seed, the scanned dir IS the live store, canonical where it sits.
                let (seed_store_dirs, seed_prefix, td_store) = build_recipe_store_layout(
                    seed_store.as_deref(),
                    seed_db.as_deref(),
                    recipe_output_store.as_deref(),
                    store_dir,
                )?;
                // The typed db set: the argv `--recipe-output-db` entries (prior td
                // recipe outputs whose bytes live OUTSIDE the seed dir — their refs
                // come from the db they wrote; the FILES stage from td_store/<base>),
                // plus the seed's own vouching db — under unconditional strict staging
                // (re #469) the unpacked seed's items are admitted by ITS
                // registrations, typed `AuditedSeed`, not by being on disk.
                // Both channels are AUTHENTICATED at intake (round-8): recipe-output
                // rows must be engine-receipt-backed, and the seed db must bind to a
                // COMPILED-pinned seed capture — a raw path/env value cannot mint a
                // typed origin.
                for (dbp, _) in &recipe_output_dbs {
                    authenticate_recipe_output_db(dbp)?;
                }
                let mut extra_dbs: Vec<(String, sandbox::InputOrigin)> = recipe_output_dbs.clone();
                if let Some(d) = &seed_db {
                    authenticate_seed_capture_db(d)?;
                    extra_dbs.push((d.clone(), sandbox::InputOrigin::AuditedSeed));
                }
                // The DERIVED blessed seed-closure db (re #469 round-8): the
                // control-plane builder's own runtime closure (host glibc/
                // gcc-lib until td-builder self-hosts) needs vouching in the
                // strict manifest, and its authority is derived — never argv.
                if let Some(bless) = derived_bless_db_auto()? {
                    extra_dbs.push((bless, sandbox::InputOrigin::BlessedSeedClosure));
                }
                let recipe_json =
                    std::fs::read_to_string(recipe_file).map_err(|e| e.to_string())?;
                build_recipe(
                    &recipe_json,
                    lock,
                    Path::new(scratch),
                    &seed_store_dirs,
                    &seed_prefix,
                    &extra_dbs,
                    src_store,
                    vendor_store,
                    builder_store,
                    td_store,
                    persist,
                )
                .map(|_| ())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build-recipe {recipe_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // The raw `build-plan PLAN STORE SCRATCH` arm is DELETED (re #469): a
        // hand-written plan/lock pair was an untyped ingress channel that could
        // stage arbitrary host paths as `seed` entries. The only plan entrance is
        // `--auto` below — locks synthesized from the recipe graph, seed paths
        // provenance-gated at synthesis AND per-entry in build_plan, every staged
        // item db-vouched + NAR-verified at the sandbox boundary.
        // td-builder build-plan --auto — GENERATE the plan from the recipe GRAPH (no
        // hand-written lock, plan, or manifest, #429): topo-sort TARGET's owned-input
        // closure, SYNTHESIZE each owned recipe's lock straight from its declared
        // `inputs`/`nativeInputs`/`sourceInput` (owned deps `td-recipe-output`,
        // everything else resolved through MAP-FILE), and run it. An input is owned iff
        // RECIPE-DIR/<name>.json exists. MAP-FILE is `NAME PATH` per line — the pinned
        // seed/source paths `ladder_setup` interned (the fresh per-run auto-map; host tools are not
        // admissible inputs, and each PATH must be a canonical store item interned in
        // SEED-STORE or synthesis reds with `provenance rejected`, re #469). SEED-DB is
        // the td-owned db those interns registered into — under strict provenance every
        // staged closure item must be vouched for by it (or a prior step's td.db) and
        // NAR-hash-match at the sandbox staging boundary.
        // Usage: build-plan --auto TARGET RECIPE-DIR MAP-FILE SEED-STORE SEED-DB SCRATCH
        Some("build-plan") if args.len() == 9 && args[2] == "--auto" => {
            let (target, recipe_dir, map_file, guix_store, seed_db, scratch) =
                (&args[3], &args[4], &args[5], &args[6], &args[7], &args[8]);
            let bov = match builder_store_env() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("td-builder: build-plan --auto {target}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let builder_store = bov.as_ref().map(|(p, s, d)| (p.as_str(), s.as_str(), d.as_str()));
            let pov = match persist_store_env() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("td-builder: build-plan --auto {target}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let persist = pov.as_ref().map(|(s, d)| (s.as_str(), d.as_str()));
            match build_plan_auto(target, recipe_dir, map_file, guix_store, seed_db, Path::new(scratch), builder_store, persist) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build-plan --auto {target}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // The `check-drv DRV CLOSURE SCRATCH` reproducibility arm is DELETED
        // (re #469 typed origins): like `build`/`realize` it manifested from the
        // caller-writable TD_EXTRA_DBS env, and nothing invoked it — the live
        // reproducibility oracle is the daemon's CHECK verb (`daemon_check_one`),
        // whose two independent builds run the typed realize_drv path.
        // td-builder shell — td's own package shell (NOT a container): resolve
        // the named recipes, build them with td, compose the command's PATH from
        // their outputs, and run it. The durable assertion is behavioral — the
        // command actually runs with the package on PATH. Usage:
        //   shell PKG... [-- CMD ARGS...]
        Some("shell") => match run_shell(&args[2..]) {
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(e) => {
                eprintln!("td-builder: shell: {e}");
                ExitCode::FAILURE
            }
        },
        // td-builder profile — build a profile symlink tree (the user-package-manager
        // profile layer): union the bin/sbin of the given package outputs into PROFILE-DIR.
        // See build_profile. Usage: profile PROFILE-DIR PKG-OUT...
        // profile [--store-native] PROFILE-DIR PKG-OUT… — union the packages' bin/sbin into a
        // symlink-tree profile. With --store-native the links target the LOGICAL store
        // (store::store_dir(), e.g. /td/store) so the profile resolves in a store-ns own-root.
        Some("profile") if args.len() >= 4 => {
            let (store_native, rest): (bool, &[String]) = if args[2] == "--store-native" {
                (true, &args[3..])
            } else {
                (false, &args[2..])
            };
            if rest.len() < 2 {
                eprintln!("usage: td-builder profile [--store-native] PROFILE-DIR PKG-OUT...");
                ExitCode::FAILURE
            } else {
                let sd = store::store_dir();
                let prefix = if store_native { Some(sd.as_str()) } else { None };
                match build_profile(&rest[0], &rest[1..], prefix) {
                    Ok(n) => {
                        eprintln!(
                            "td-builder: profile {} — linked {n} entr{}{}",
                            rest[0],
                            if n == 1 { "y" } else { "ies" },
                            if store_native { " (store-native)" } else { "" }
                        );
                        println!("{}", rest[0]);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("td-builder: profile: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
        // elf-interp FILE — print FILE's program interpreter (PT_INTERP), or nothing for a
        // shared object. td's OWN ELF reader (no patchelf / no guix tool).
        Some("elf-interp") if args.len() == 3 => match elf::read_interp(Path::new(&args[2])) {
            Ok(Some(i)) => {
                println!("{i}");
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::SUCCESS, // no interpreter (e.g. a .so)
            Err(e) => {
                eprintln!("td-builder: elf-interp {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // elf-set-interp FILE NEW — rewrite FILE's PT_INTERP to NEW: in place when it fits
        // the existing slot, else GROWN (string appended at EOF, mapped by repurposing the
        // PT_NOTE segment into a covering PT_LOAD — see elf::set_interp), so a full hashed
        // /td/store/<hash>-glibc.../ld loader path fits. The one patchelf feature the
        // Rust toolchain relinking needs, owned by td in Rust so the build path adds NO guix tool.
        Some("elf-set-interp") if args.len() == 4 => {
            match elf::set_interp(Path::new(&args[2]), &args[3]) {
                Ok(()) => {
                    eprintln!("td-builder: elf-set-interp {} -> {}", args[2], args[3]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: elf-set-interp {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // elf-rpath FILE — print FILE's run-path (DT_RUNPATH, else legacy DT_RPATH), or
        // nothing for a static binary / one with no run-path. td's OWN ELF reader.
        Some("elf-rpath") if args.len() == 3 => match elf::read_rpath(Path::new(&args[2])) {
            Ok(Some(r)) => {
                println!("{r}");
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::SUCCESS, // no run-path
            Err(e) => {
                eprintln!("td-builder: elf-rpath {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // elf-set-rpath FILE NEW — rewrite FILE's DT_RPATH/DT_RUNPATH to NEW in place (must
        // fit the existing .dynstr slot). Makes a toolchain binary self-sufficient — bake an
        // absolute /td/store run-path so it finds its shared libc with no LD_LIBRARY_PATH
        // wrapper. The second patchelf feature td owns in Rust (no guix tool on the path).
        Some("elf-set-rpath") if args.len() == 4 => {
            match elf::set_rpath(Path::new(&args[2]), &args[3]) {
                Ok(()) => {
                    eprintln!("td-builder: elf-set-rpath {} -> {}", args[2], args[3]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: elf-set-rpath {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // loop-sandbox: the DEV-SHELL — run a command inside td's own hermetic
        // container (pivot into a fresh root exposing only the selected store,
        // synthetic /proc and /dev, and its own loopback-only netns), toward
        // replacing `guix shell -C`. With
        // `--expose-cwd` it adds the FULL loop env (worktree + cgroups,
        // caller PATH + TD_SUBST_*/TD_DAEMON_* preserved, chdir into the
        // cwd) so a real rung runs as under `guix shell -C`.
        //
        // GUIX-LESS provisioning (host-sandbox-stage0 inc2 — the daily-suite VM):
        //   --store-from DIR : bind DIR (an UNPACKED SEED store, e.g.
        //                      <seed>/store/gnu/store) at /gnu/store INSIDE the
        //                      sandbox instead of the host /gnu/store, so the loop
        //                      toolchain resolves from the seed and the host store
        //                      is absent — the substrate for a VM with no guix.
        //   --no-daemon      : accepted for compatibility. /var/guix is never
        //                      bound; the build path uses td-builder's own build
        //                      jail and shared build daemon.
        //   --store-at DEST  : where --store-from is mounted INSIDE (default: the
        //                      active store dir, store::store_dir()). Pass /td/store
        //                      for td's own store-native harness (busybox/make/
        //                      td-builder relinked to /td/store/ld); then the host
        //                      /gnu/store is NOT bound at all — the guix-byte-free
        //                      loop substrate.
        //   --store-item PATH: bind ONE store item read-only at its own path
        //                      (repeatable). The loop's input-only exposure:
        //                      `td-builder check` passes its declared input set
        //                      item by item, so NO store directory is ever
        //                      mounted — only declared inputs, like the drv
        //                      build jail.
        //   --store-item-at SRC DEST: like --store-item, but bound at DEST
        //                      (repeatable) — the td-built loop userland's
        //                      durable host copy appears at its canonical
        //                      /td/store path.
        // Without --store-from/--store-item the sandbox binds no host store.
        // Usage:
        //   host-sandbox [--expose-cwd] [--store-from DIR [--store-at DEST]] [--store-item PATH]... [--store-item-at SRC DEST]... [--no-daemon] -- CMD ARGS...
        Some("host-sandbox") if args.len() >= 4 => {
            let parsed = match parse_host_sandbox_args(&args) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("td-builder: host-sandbox: {msg}");
                    return ExitCode::from(2);
                }
            };
            let HostSandboxArgs { expose_cwd, store_from, store_at, store_items, no_daemon, cmd, cmd_args } =
                parsed;
            let _daemon_bind_compat = no_daemon;
            let run = || -> Result<std::process::ExitStatus, String> {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/home/td".to_string());
                // The base exposure set is the requested store (ro). /dev is NOT bound — host_shell builds a
                // minimal synthetic /dev (standard char devices + shm + pts + fd
                // links, like `guix shell -C`) instead of leaking the host device
                // tree (kmsg/kvm/disks/input/GPUs). /proc is NOT bound either —
                // host_shell mounts a FRESH procfs reflecting the sandbox's own
                // PID namespace, so the host /proc never leaks in and nested
                // containers see a private /proc.
                //
                // The store bind is explicit: with --store-from DIR, bind the
                // UNPACKED store DIR, mounted at DEST
                // (--store-at, default: the active store dir) so its binaries'
                // hardcoded interpreters resolve. With --store-at /td/store (td's own
                // store-native harness) the host /gnu/store is then absent — the
                // guix-byte-free loop substrate. /var/guix is never bound.
                let mut binds = host_sandbox_base_binds(store_from.as_deref(), store_at.as_deref());
                // Per-item store inputs (--store-item / --store-item-at): each
                // declared input bound READ-ONLY at its own path (or the given
                // DEST) — the input-only exposure the loop uses instead of
                // mounting any store directory.
                for (src, dest) in &store_items {
                    binds.push(sandbox::Bind {
                        src: src.clone(),
                        dest: dest.clone(),
                        readonly: true,
                        ro_optional: false,
                    });
                }
                // The parent dirs holding the own-path (--store-item) bind
                // mountpoints — e.g. the seed store dir — are locked
                // READ-ONLY after binding (host_shell ro_dirs): the items are
                // already ro, and this closes the remaining hole of an
                // ACCIDENTAL write creating a sibling entry next to them in
                // the writable tmpfs dir (a fake "store item"; a hostile gate
                // owning the sandbox namespaces could still over-mount — same
                // trust model as every mount here). The DEST-mapped items'
                // /td/store parent deliberately stays writable — it is the
                // loop's WORKING store prefix (store gates create entries
                // under it; their own nested jails guard those trees).
                let mut ro_dirs: Vec<String> = Vec::new();
                for (src, dest) in &store_items {
                    if dest.is_some() {
                        continue;
                    }
                    if let Some(parent) = Path::new(src).parent() {
                        let d = parent.to_string_lossy().into_owned();
                        if !d.is_empty() && d != "/" && !ro_dirs.contains(&d) {
                            ro_dirs.push(d);
                        }
                    }
                }
                let mut tmpfs = vec!["/tmp".to_string()];
                let mut path_env = String::new();
                let mut workdir = String::new();
                let mut extra_env: Vec<(String, String)> = Vec::new();
                if expose_cwd {
                    let cwd = std::env::current_dir()
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned();
                    // Worktree (rw, like guix shell -C's shared cwd) and the host
                    // cgroup hierarchy (ro — gate-run's per-gate memory-limit
                    // delegation, issue #328, needs to READ the hierarchy structure;
                    // only its own delegated subtree, bound separately below, is
                    // writable). HOME is a dir on the writable root tmpfs (created by
                    // these binds), so no HOME tmpfs.
                    binds.push(sandbox::Bind { src: cwd.clone(), dest: None, readonly: false, ro_optional: false });
                    if Path::new("/sys/fs/cgroup").is_dir() {
                        // ro is defense-in-depth (least-privilege default: expose the
                        // hierarchy read-only unless a specific subtree needs writes).
                        // A child userns can't remount-ro the host-owned cgroup2 on
                        // some kernels (EPERM, e.g. the azure CI runner); there the
                        // bind is DETACHED (fail-closed), never left writable — see
                        // Bind::ro_optional.
                        binds.push(sandbox::Bind {
                            src: "/sys/fs/cgroup".to_string(),
                            dest: None,
                            readonly: true,
                            ro_optional: true,
                        });
                        // The delegated per-run cgroup dir (issue #328): bound
                        // RW OVER the ro hierarchy so gate-run (inside) can
                        // create per-gate child cgroups + set memory.max. Only
                        // when `td-builder check` probed a delegation; the rest
                        // of the hierarchy stays ro.
                        if let Ok(cg) = std::env::var("TD_CHECK_CGROUP") {
                            if !cg.is_empty() && Path::new(&cg).is_dir() {
                                binds.push(sandbox::Bind {
                                    src: cg,
                                    dest: None,
                                    readonly: false,
                                    ro_optional: false,
                                });
                            }
                        }
                    }
                    // The persistent signed substitute store (~/.td/subst, populated by the daily) —
                    // READ-ONLY: the loop FETCHES the lock-keyed toolchain closure from it
                    // (x64-toolchain-subst) over its own loopback netns instead of rebuilding ~98 min
                    // from seed, and never writes it. Like the host /gnu/store + guix cache, it is a
                    // declared, exposed input — no network egress (resolve-toolchain serves loopback).
                    let subst = format!("{home}/.td/subst");
                    if Path::new(&subst).is_dir() {
                        binds.push(sandbox::Bind { src: subst, dest: None, readonly: true, ro_optional: false });
                    }
                    // The ONE shared build daemon's socket + output store (~/.td/build-daemon,
                    // started on the host by the `td-builder check` prelude). The corpus build
                    // (inside this sandbox) SUBMITS drvs to it over the socket and reads its
                    // output back, so it must be visible at the SAME absolute path in every
                    // check sandbox — RW (connect to the socket; read the store). Bound only
                    // when present; a cold machine without a running daemon simply lacks it.
                    let bdd = format!("{home}/.td/build-daemon");
                    if Path::new(&bdd).is_dir() {
                        binds.push(sandbox::Bind { src: bdd.clone(), dest: None, readonly: false, ro_optional: false });
                    }
                    // The #317 warm chain-brick cache: when the operator points
                    // TD_CHECK_CHAIN_CACHE at a CUSTOM host path (the default lives
                    // under ~/.td/build-daemon, bound above), bind it RW so warm
                    // bricks actually persist — unbound, the override would silently
                    // write to the sandbox's ephemeral root and vanish on teardown.
                    if let Ok(cc) = std::env::var("TD_CHECK_CHAIN_CACHE") {
                        if !cc.is_empty() && !Path::new(&cc).starts_with(&bdd) {
                            let _ = std::fs::create_dir_all(&cc);
                            if Path::new(&cc).is_dir() {
                                binds.push(sandbox::Bind {
                                    src: cc,
                                    dest: None,
                                    readonly: false,
                                    ro_optional: false,
                                });
                            }
                        }
                    }
                    path_env = std::env::var("PATH").unwrap_or_default();
                    workdir = cwd;
                    for (k, v) in std::env::vars() {
                        // TD_SUBST_* = the host-provisioned
                        // substitute resolver knobs (TD_SUBST_BIN/STORE/PUBKEY) the toolchain gates
                        // read to FETCH the lock-keyed closure instead of building from seed;
                        // TD_DAEMON_* = the shared build daemon's socket (TD_DAEMON_SOCKET) the
                        // corpus build submits to. (TD_CHECK_CHAIN_CACHE — the #317 warm
                        // chain-brick knob, including its set-and-empty force-cold form —
                        // rides the TD_CHECK_ prefix.)
                        if k.starts_with("TD_CHECK_")
                            || k.starts_with("TD_SUBST_")
                            || k.starts_with("TD_DAEMON_")
                        {
                            extra_env.push((k, v));
                        }
                    }
                } else {
                    tmpfs.push(home.clone());
                }
                let scratch = std::env::temp_dir()
                    .join(format!("td-host-sandbox-{}-{}", sys::getuid(), std::process::id()));
                let _ = std::fs::remove_dir_all(&scratch);
                std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
                let result = sandbox::host_shell(
                    &cmd, &cmd_args, &binds, &tmpfs, &path_env, &home, &workdir, &extra_env,
                    &ro_dirs, &scratch,
                )
                .map_err(|e| e.to_string());
                // Remove the scratch tree (the sandbox's mounts lived in the
                // child's now-gone mount namespace, so only an empty dir remains
                // here). Previously leaked one dir per run.
                let _ = std::fs::remove_dir_all(&scratch);
                result
            };
            match run() {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
                Err(e) => {
                    eprintln!("td-builder: host-sandbox: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder store-ns — OWN ROOT, td's OWN store at /td/store, NO guix (user-pm
        // Phase 0; human 2026-06-21: break from guix's /gnu/store, not mixed with the local
        // guix install). Enter a user namespace pivoted into a minimal td-owned root
        // (host_shell's fresh-tmpfs root + minimal /dev + private /proc), bind STORE-DIR at
        // `/td/store`, and bind NOTHING from /gnu/store or /var/guix — so inside, `/td/store`
        // IS the store (= STORE-DIR) and the host `/gnu/store` + guix install are ABSENT. A
        // static binary in STORE-DIR runs by absolute path; dynamic content needs the seed
        // relocated to /td/store (Phase 2). Rootless (no daemon, no root), unmixed from guix.
        //   store-ns STORE-DIR -- CMD ARGS...
        Some("store-ns") if args.len() >= 5 && args[3] == "--" => {
            let store_dir = args[2].clone();
            let cmd = args[4].clone();
            let cmd_args: Vec<String> = args[5..].to_vec();
            let run = || -> Result<std::process::ExitStatus, String> {
                if !Path::new(&store_dir).is_dir() {
                    return Err(format!("store dir `{store_dir}' does not exist"));
                }
                // The ONLY bind: the user store at td's prefix. No /gnu/store, no /var/guix.
                let binds = vec![sandbox::Bind {
                    src: store_dir,
                    dest: Some("/td/store".to_string()),
                    readonly: true,
                    ro_optional: false,
                }];
                let tmpfs = vec!["/tmp".to_string()];
                let home = "/tmp".to_string();
                let path_env = "/td/store/bin".to_string();
                let scratch = std::env::temp_dir()
                    .join(format!("td-store-ns-{}-{}", sys::getuid(), std::process::id()));
                let _ = std::fs::remove_dir_all(&scratch);
                std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
                let result = sandbox::host_shell(
                    &cmd, &cmd_args, &binds, &tmpfs, &path_env, &home, "", &[], &[], &scratch,
                )
                .map_err(|e| e.to_string());
                let _ = std::fs::remove_dir_all(&scratch);
                result
            };
            match run() {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
                Err(e) => {
                    eprintln!("td-builder: store-ns: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder userns-private -- CMD ARGS... — a private mount+user namespace
        // over the CURRENT root: no pivot_root, no fresh tmpfs, no bind allowlist.
        // The native replacement for the util-linux `unshare -rm` CLI a gate body
        // used to shell out to (gate 171 stage0-cold-start's cold leg: it needs the
        // WHOLE host filesystem visible minus one path hidden by a private bind
        // mount — the opposite shape from host-sandbox/store-ns, which both pivot
        // into a FRESH root with an explicit bind allowlist). Maps to ROOT (uid/gid
        // 0), matching `-r`/`--map-root-user`: the namespace creator holds full
        // capabilities inside it regardless of the mapped id, so the raw mount(2)
        // syscall would succeed either way, but the `mount(8)` CLI itself refuses
        // ("must be superuser") unless its apparent euid is 0 — verified empirically
        // (an identity map here made a plain `mount --bind` inside fail with that
        // message even though the syscall-level capability was present).
        //   userns-private -- CMD ARGS...
        Some("userns-private") if args.len() >= 4 && args[2] == "--" => {
            let cmd = args[3].clone();
            let cmd_args: Vec<String> = args[4..].to_vec();
            // Returns Infallible on the Ok side: this only ever ends by exec'ing the
            // command (which replaces the process and never returns here on success)
            // or by producing an error — there is no success value to carry.
            let run = || -> Result<std::convert::Infallible, String> {
                let host_uid = sys::getuid();
                let host_gid = sys::getgid();
                sys::unshare(sys::CLONE_NEWUSER | sys::CLONE_NEWNS)
                    .map_err(|e| format!("unshare(NEWUSER|NEWNS): {e}"))?;
                sandbox::map_userns_id(host_uid, host_gid, 0, 0)
                    .map_err(|e| format!("map_userns_id: {e}"))?;
                // Make the root mount tree private so a nested mount (e.g. the
                // caller's `mount --bind ... /var/guix`) never propagates to the
                // host — `unshare -m`'s actual behavior, not merely a fresh
                // mount-namespace id.
                let root_c = CString::new("/").map_err(|e| e.to_string())?;
                sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)
                    .map_err(|e| format!("mount(/ private): {e}"))?;
                let err = Command::new(&cmd).args(&cmd_args).exec();
                Err(format!("exec {cmd}: {err}"))
            };
            match run() {
                Err(e) => {
                    eprintln!("td-builder: userns-private: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // (the gate runner's per-process memory cap needs no subcommand here:
        // gates.rs applies setrlimit(RLIMIT_DATA) in the child via
        // sandbox::cap_child_data_rlimit — td's native prlimit(1) replacement.)
        // (the `resolve` subcommand — a Guile-oracle lock resolver — retired with
        // the guix Guile-lowering gates; native input resolution lives in gate_inputs.rs.)
        // corpus-independence: run AS a derivation's builder, executing the
        // autotools phases in Rust (replaces gnu-build-system). Reads the build
        // environment from env vars (out, TD_SRC, TD_INPUTS, TD_CONFIGURE_FLAGS)
        // that the td-native derivation contract sets on the derivation.
        Some("autotools-build") if args.len() == 2 => match build::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: autotools-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's OWN Rust/cargo build system (the cargo-build-system replacement):
        // builds the TD_SRC crate with `cargo build --offline` and installs
        // TD_RUST_BINS into $out/bin. Sibling of autotools-build; same
        // env-driven derivation-builder contract.
        Some("rust-build") if args.len() == 2 => match build::run_rust() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: rust-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's OWN cmake build system (the cmake-build-system replacement): runs an
        // out-of-source `cmake` configure -> make -> make install over the TD_SRC
        // tree, installing into $out. Sibling of autotools-build/rust-build; same
        // env-driven derivation-builder contract.
        Some("cmake-build") if args.len() == 2 => match build::run_cmake() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: cmake-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's stage0-posix SEED build system (#378): see build::run_stage0.
        // Sibling of autotools-build/rust-build/cmake-build; same env contract.
        Some("stage0-build") if args.len() == 2 => match build::run_stage0() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: stage0-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's bootstrap-RUNG step executor (#378 slices 2+3): see
        // build::run_mesboot. Same env-driven derivation-builder contract.
        Some("mesboot-build") if args.len() == 2 => match build::run_mesboot() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: mesboot-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's Rust bootstrap-snapshot transform: see build::run_rust_stage0.
        // Same env-driven derivation-builder contract as the other phase runners.
        Some("rust-stage0-build") if args.len() == 2 => match build::run_rust_stage0() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: rust-stage0-build: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: td-builder            # print the S1 sentinel");
            eprintln!("       td-builder check [GOAL...]             # the loop: host prelude + sandboxed gate ladder");
            eprintln!("       td-builder gate-run [-j N] [GOAL...]   # the in-sandbox gate scheduler (src/gate_defs/)");
            eprintln!("       td-builder check-rung HARNESS [ARG...] # dev: run a harness inside the loop sandbox");
            eprintln!("       td-builder text <op> ...               # typed text assertions/extraction for loop scripts");
            eprintln!("       td-builder lock <op> ...               # typed lock path extraction/rewrites");
            eprintln!("       td-builder files PATH...");
            eprintln!("       td-builder files-name-first PATTERN PATH...");
            eprintln!("       td-builder tree-fingerprint PATH...");
            eprintln!("       td-builder tree-contains NEEDLE PATH...");
            eprintln!("       td-builder tree-not-contains NEEDLE PATH...");
            eprintln!("       td-builder tree-first-containing NEEDLE PATH...");
            eprintln!("       td-builder path-older-than PATH DAYS");
            eprintln!("       td-builder daemon-budget-check LOG BUDGET");
            eprintln!("       td-builder gzip-decompress GZFILE OUTFILE");
            eprintln!("       td-builder nar-hash PATH");
            eprintln!("       td-builder nar-restore NARFILE DEST");
            eprintln!("       td-builder tar-extract TARFILE DEST");
            eprintln!("       td-builder tar-gz-extract TAR_GZ_FILE DEST");
            eprintln!("       td-builder tar-bz2-extract TAR_BZ2_FILE DEST");
            eprintln!("       td-builder subst-export DB STORE-DIR OUTDIR ROOT...");
            eprintln!("       td-builder drv-parse FILE.drv");
            eprintln!("       td-builder drv-refs FILE.drv");
            eprintln!("       td-builder store-register STORE-PATH DERIVER CANDIDATES-FILE OUT-DB");
            eprintln!("       td-builder store-query DB info|references|references-only|outputs");
            eprintln!("       td-builder store-closure DB ROOT");
            eprintln!("       td-builder store-closure-scan STORE-DIR[,EXTRA-DIR...] ROOT...");
            eprintln!("       td-builder store-add-text NAME CONTENT-FILE STORE-DIR OUT-DB");
            eprintln!("       td-builder store-add-recursive NAME SRC STORE-DIR OUT-DB");
            eprintln!("       td-builder store-add-referenced NAME CONTENT-FILE REFS-FILE STORE-DIR OUT-DB");
            eprintln!("       td-builder store-add-output OUTPUT DERIVER CLOSURE-FILE STORE-DIR OUT-DB");
            eprintln!("       td-builder store-verify DB STORE-ROOT");
            eprintln!("       td-builder store-gc-sweep STORE-DIR DB ROOT");
            eprintln!("       td-builder resolve LOCKFILE NAME...");
            eprintln!("       td-builder seed-bless SEED-DIR OUT-DB");
            eprintln!("       td-builder build-recipe RECIPE-JSON LOCK SCRATCH-DIR STORE-DIR [SRC-STORE-DIR SRC-DB] [--recipe-output-store STORE] [--recipe-output-db DB]...");
            eprintln!("       td-builder build-plan --auto TARGET RECIPE-DIR MAP-FILE SEED-STORE SEED-DB SCRATCH");
            eprintln!("       td-builder stage0-place BASEDIR        # compile+place the guix-free stage0 (memoized)");
            eprintln!("       td-builder provision-rust              # print the guix-free rust toolchain PATH fragment");
            eprintln!("       td-builder provision-cc                # print the guix-free C toolchain PATH fragment");
            eprintln!("       td-builder provision-glibc-static      # print the matched static glibc lib dir (crt-static -L)");
            eprintln!("       td-builder assert-static PATH          # fail unless PATH is a fully static binary");
            eprintln!("       td-builder autotools-build   # as a derivation builder");
            eprintln!("       td-builder rust-build        # as a derivation builder (cargo)");
            eprintln!("       td-builder cmake-build       # as a derivation builder (cmake)");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The daemon CHECK verb reuses the build already realized as ONE of the two
    /// independent reproducibility builds — but ONLY when every output tree is actually
    /// present under the build scratch; a missing output must force the fresh-rebuild
    /// fallback (never a comparison against an absent baseline). This pins that decision.
    #[test]
    fn output_trees_present_gates_the_repro_build_reuse() {
        let dir = std::env::temp_dir().join(format!("td-repro-baseline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = "/gnu/store/aaaaaaaaaaaaaaaa-hello-2.12".to_string();
        let b = "/gnu/store/bbbbbbbbbbbbbbbb-hello-2.12-lib".to_string();
        let touch = |canon: &str| {
            let p = daemon_host_path(&dir, canon).unwrap();
            let p = std::path::Path::new(&p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"out").unwrap();
        };
        // Empty ⇒ false: a drv with no outputs must never reuse a vacuous baseline.
        assert!(!output_trees_present(&dir, &[]), "empty canon set must not reuse");
        // Missing ⇒ false (the fallback trigger — VERIFIED-RED for the reuse guard).
        assert!(!output_trees_present(&dir, &[a.clone()]), "absent output must force a rebuild");
        // Present ⇒ true (the loop's normal 2-build path).
        touch(&a);
        assert!(output_trees_present(&dir, &[a.clone()]), "present output must be reusable");
        // Multi-output: reuse only when EVERY output is present.
        assert!(!output_trees_present(&dir, &[a.clone(), b.clone()]), "one missing output must force a rebuild");
        touch(&b);
        assert!(output_trees_present(&dir, &[a, b]), "all outputs present must be reusable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // `td shell` starts a Rust package lock from the one content-addressed
    // source it just interned. Cargo.lock and the separately interned vendor
    // tree carry the dependency closure; no legacy external package lock leaks
    // a build tool into this boundary.
    #[test]
    fn source_lock_body_contains_only_the_interned_source() {
        let lock = source_lock_body("ripgrep-source", "/td/store/zzz-ripgrep-src");
        assert_eq!(
            lock,
            "ripgrep-source /td/store/zzz-ripgrep-src source\n"
        );
    }

    // A warmed extraction is mutable cache state, so td shell consumes only the
    // archived bytes whose digest the recipe catalog commits. The second leg is
    // the verified-red control: changing one source byte must reject the cache.
    #[test]
    fn shell_source_archive_must_match_the_recipe_pin() {
        let pins = "ripgrep-source\thttps://example.invalid/ripgrep.crate\tba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\tripgrep.crate\n";
        let pin = parse_shell_source_pin(pins).unwrap();
        assert_eq!(pin.key, "ripgrep-source");
        assert_eq!(pin.file, "ripgrep.crate");
        let malformed_second = format!("{pins}not-a-tab-separated-pin\n");
        let err = parse_shell_source_pin(&malformed_second).err().unwrap();
        assert!(err.contains("malformed td shell recipe source pin"), "{err}");

        let dir = std::env::temp_dir().join(format!(
            "td-shell-source-pin-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("ripgrep.crate");
        std::fs::write(&archive, b"abc").unwrap();
        verify_shell_source_archive(&archive, &pin).unwrap();
        std::fs::write(&archive, b"abd").unwrap();
        let err = verify_shell_source_archive(&archive, &pin).unwrap_err();
        assert!(err.contains("!= recipe source pin"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The product shell may add only current `/td/store` recipe outputs to
    // that source lock. Guix paths and the downloaded Rust stage0 are rejected
    // rather than silently removed.
    #[test]
    fn recipe_toolchain_lock_accepts_only_td_recipe_outputs() {
        let source = "ripgrep-source /td/store/src-ripgrep source\n";
        let native = "\
rust-toolchain /td/store/qnkl-rust-toolchain td-recipe-output
gcc-x86-64-self /td/store/ng-gcc-self td-recipe-output
binutils-x86-64-self /td/store/nb-binutils-self td-recipe-output
glibc-x86-64 /td/store/gl-glibc td-recipe-output
";
        let out = recipe_toolchain_lock_body(source, native).unwrap();
        assert!(out.starts_with(source));
        for nl in native.lines() {
            assert_eq!(out.matches(nl).count(), 1);
        }
        assert!(!out.contains("/gnu/store"));
        let padded = format!("\n{source}\n");
        assert_eq!(recipe_toolchain_lock_body(&padded, native).unwrap(), out);
    }

    #[test]
    fn recipe_toolchain_lock_rejects_guix_and_stage0() {
        let source = "ripgrep-source /td/store/src-ripgrep source\n";
        assert!(recipe_toolchain_lock_body(
            "ripgrep-source /gnu/store/src-ripgrep source\n",
            "rust-toolchain /td/store/final td-recipe-output\n"
        )
        .is_err());
        assert!(recipe_toolchain_lock_body(
            source,
            "rust-toolchain /gnu/store/final td-recipe-output\n"
        )
        .is_err());
        assert!(recipe_toolchain_lock_body(
            source,
            "rust-stage0 /td/store/downloaded-rust-stage0 td-recipe-output\n"
        )
        .is_err());
        assert!(recipe_toolchain_lock_body(
            source,
            "rust-toolchain /td/store/final seed\n"
        )
        .is_err());
        assert!(recipe_toolchain_lock_body(
            "ripgrep-source /td/store/src-ripgrep source\nextra /td/store/extra source\n",
            "rust-toolchain /td/store/final td-recipe-output\n"
        )
        .is_err());
    }

    #[test]
    fn recipe_toolchain_lock_requires_a_build_platform() {
        assert!(recipe_toolchain_lock_body(
            "ripgrep-source /td/store/src-ripgrep source\n",
            ""
        )
        .is_err());
    }

    #[test]
    fn recipe_output_options_preserve_dbs_and_allow_one_store() {
        let args: Vec<String> = [
            "td-builder",
            "build-recipe",
            "recipe.json",
            "recipe.lock",
            "scratch",
            "store",
            "--recipe-output-db",
            "one.db",
            "--recipe-output-store",
            "physical-store",
            "--recipe-output-db",
            "two.db",
        ]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
        let parsed = parse_recipe_output_options(&args).unwrap();
        assert_eq!(parsed.positional_len, 6);
        assert_eq!(parsed.store.as_deref(), Some("physical-store"));
        let dbs: Vec<&str> = parsed.dbs.iter().map(|(db, _)| db.as_str()).collect();
        assert_eq!(dbs, ["one.db", "two.db"]);
    }

    #[test]
    fn recipe_output_options_reject_a_second_store() {
        let args: Vec<String> = [
            "td-builder",
            "build-recipe",
            "recipe.json",
            "recipe.lock",
            "scratch",
            "store",
            "--recipe-output-store",
            "one",
            "--recipe-output-store",
            "two",
        ]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
        assert!(parse_recipe_output_options(&args).is_err());
    }

    #[test]
    fn recipe_output_only_store_is_a_closure_staging_candidate() {
        let physical = "/tmp/td-recipe-output-store";
        let (dirs, prefix, td_store) =
            build_recipe_store_layout(None, None, Some(physical), "/legacy/store").unwrap();
        assert_eq!(dirs, [physical]);
        assert_eq!(prefix, store::store_dir());
        assert_eq!(td_store, Some(Path::new(physical)));
    }

    #[test]
    fn loop_text_helpers_extract_and_count() {
        let text = "alpha\nDRV=/tmp/a.drv\nSTEP gcc /td/store/gcc\nSTEP gcc /td/store/gcc2\n\n";
        assert_eq!(first_line_with_prefix(text, "DRV="), Some("/tmp/a.drv".to_string()));
        assert_eq!(last_line_with_prefix(text, "STEP gcc "), Some("/td/store/gcc2".to_string()));
        assert_eq!(first_line_containing(text, "store/gcc"), Some("STEP gcc /td/store/gcc".to_string()));
        assert_eq!(count_line_exact(text, "alpha"), 1);
        assert_eq!(count_nonempty_lines(text), 4);
        assert!(cargo_test_reported_nonzero_tests("test result: ok. 12 passed; 0 failed"));
        assert!(!cargo_test_reported_nonzero_tests("test result: ok. 0 passed; 0 failed"));
    }

    #[test]
    fn loop_tree_helpers_find_names_and_bytes() {
        let d = std::env::temp_dir().join(format!("td-loop-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/libstdc++.so.6.0"), b"hello /td/store").unwrap();
        std::os::unix::fs::symlink("libstdc++.so.6.0", d.join("a/b/libstdc++.so.6")).unwrap();
        std::fs::write(d.join("a/cc1"), b"compiler").unwrap();
        let roots = vec![d.display().to_string()];
        assert_eq!(
            first_file_named("libstdc++.so.6", &roots).unwrap().unwrap(),
            d.join("a/b/libstdc++.so.6")
        );
        assert_eq!(
            first_file_named("libstdc++.so.6*", &roots).unwrap().unwrap(),
            d.join("a/b/libstdc++.so.6")
        );
        assert_eq!(
            tree_first_containing("/td/store", &roots).unwrap().unwrap(),
            d.join("a/b/libstdc++.so.6.0")
        );
        assert_eq!(first_file_named("cc1", &roots).unwrap().unwrap(), d.join("a/cc1"));
        assert!(tree_first_containing("/gnu/store", &roots).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lock_rewrite_replaces_gcc_toolchain_once() {
        let body = "\
# keep
coreutils /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-coreutils-9.1
old-gcc /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-toolchain-15.2.0
kept-input /gnu/store/cccccccccccccccccccccccccccccccc-kept-input-1.0
";
        let out = rewrite_gcc_toolchain_lock_body(
            body,
            "/td/store/tttt-gcc-toolchain-tdstore",
            "/td/store/gggg-glibc-2.41",
        )
        .unwrap();
        assert!(out.contains("gcc-toolchain /td/store/tttt-gcc-toolchain-tdstore seed\n"));
        assert!(out.contains("glibc-2.41 /td/store/gggg-glibc-2.41 seed\n"));
        assert!(!out.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-toolchain"));
        assert_eq!(out.matches("gcc-toolchain ").count(), 1);
    }

    #[test]
    fn daemon_budget_parser_preserves_peak_and_min_start_assertions() {
        let log = "\
daemon: budget 2 concurrent builds
daemon build START (1/2 active)
daemon build START (2/2 active)
daemon build START (2/2 active)
";
        assert_eq!(daemon_budget_stats(log, 2).unwrap(), (2, 3));
        let d = std::env::temp_dir().join(format!("td-daemon-budget-{}.log", std::process::id()));
        std::fs::write(&d, log).unwrap();
        assert_eq!(daemon_budget_check(d.to_str().unwrap(), "2").unwrap(), (2, 3));
        std::fs::write(&d, "daemon: budget 2 concurrent builds\ndaemon build START (2/2 active)\n").unwrap();
        let err = daemon_budget_check(d.to_str().unwrap(), "2").unwrap_err();
        assert!(err.contains("expected at least 3"), "got: {err}");
        let _ = std::fs::remove_file(&d);
    }

    // ---- persistent accumulating store DB (merge_regs) ----------------------
    // These are the durable, daemon-free proof that a td store ACCUMULATES across
    // builds: merge_regs takes the existing db bytes + new outputs and returns a db
    // that holds BOTH, by store path.

    fn reg(path: &str, hash: &str, refs: &[&str]) -> OutputReg {
        OutputReg {
            store_path: path.to_string(),
            nar_hash: hash.to_string(),
            nar_size: 42,
            refs: refs.iter().map(|s| s.to_string()).collect(),
            deriver: format!("{path}.drv"),
        }
    }
    // A path's full ValidPaths row (rowid + hash) or None if it is only a scaffold.
    fn full_row(db: &store_db_read::Db, path: &str) -> Option<(i64, String)> {
        for (rowid, cols) in db.table("ValidPaths").unwrap() {
            if let (Some(store_db_read::Value::Text(p)), Some(store_db_read::Value::Text(h))) =
                (cols.get(1), cols.get(2))
            {
                if p == path && !h.is_empty() {
                    return Some((rowid, h.clone()));
                }
            }
        }
        None
    }
    fn sorted_closure(db: &store_db_read::Db, root: &str) -> Vec<String> {
        let mut c = db.closure(root).unwrap();
        c.sort();
        c
    }

    const A: &str = "/gnu/store/00000000000000000000000000000000-a";
    const B: &str = "/gnu/store/11111111111111111111111111111111-b";
    const X: &str = "/gnu/store/22222222222222222222222222222222-x";

    #[test]
    fn merge_into_empty_registers_output_and_scaffold_ref() {
        // First commit (no existing db): A (full) referencing X (scaffold).
        let bytes = merge_regs(None, &[reg(A, "hashA", &[X])]).unwrap();
        let db = store_db_read::Db::open(bytes).unwrap();
        assert_eq!(full_row(&db, A).map(|r| r.1), Some("hashA".to_string()));
        assert!(full_row(&db, X).is_none(), "a bare reference is a scaffold (no hash)");
        assert_eq!(sorted_closure(&db, A), vec![A.to_string(), X.to_string()]);
    }

    #[test]
    fn merge_accumulates_across_commits_without_clobbering() {
        // Commit A, then commit B (referencing A) into the SAME db: BOTH survive.
        // This is the accumulation property a fresh-write (clobber) lacks — the
        // verified-red is exactly "make merge ignore `existing` → A vanishes here".
        let db1 = merge_regs(None, &[reg(A, "hashA", &[])]).unwrap();
        let db2 = merge_regs(Some(&db1), &[reg(B, "hashB", &[A])]).unwrap();
        let db = store_db_read::Db::open(db2).unwrap();
        assert_eq!(full_row(&db, A).map(|r| r.1), Some("hashA".to_string()), "A NOT clobbered by B's commit");
        assert_eq!(full_row(&db, B).map(|r| r.1), Some("hashB".to_string()));
        // B's closure spans the earlier-committed A (read-back across commits).
        assert_eq!(sorted_closure(&db, B), vec![A.to_string(), B.to_string()]);
    }

    #[test]
    fn merge_is_idempotent_and_byte_deterministic() {
        // Re-committing the same set reproduces the bytes exactly (sorted rowids),
        // so a redundant commit is a safe no-op on the db.
        let once = merge_regs(None, &[reg(A, "hashA", &[X]), reg(B, "hashB", &[A])]).unwrap();
        let twice = merge_regs(Some(&once), &[reg(A, "hashA", &[X]), reg(B, "hashB", &[A])]).unwrap();
        assert_eq!(once, twice, "re-merging the same outputs must be byte-identical");
        // Commit ORDER must not matter either (rowids assigned by sorted path).
        let other = merge_regs(None, &[reg(B, "hashB", &[A]), reg(A, "hashA", &[X])]).unwrap();
        assert_eq!(once, other, "merge result is independent of commit order");
    }

    #[test]
    fn merge_upgrades_scaffold_to_full_row() {
        // A appears first only as B's reference (scaffold), then is committed for
        // real: its row gains the hash in place (no duplicate path row).
        let db1 = merge_regs(None, &[reg(B, "hashB", &[A])]).unwrap();
        assert!(full_row(&store_db_read::Db::open(db1.clone()).unwrap(), A).is_none());
        let db2 = merge_regs(Some(&db1), &[reg(A, "hashA", &[])]).unwrap();
        let db = store_db_read::Db::open(db2).unwrap();
        assert_eq!(full_row(&db, A).map(|r| r.1), Some("hashA".to_string()), "scaffold A upgraded to full");
        let a_rows = db
            .table("ValidPaths")
            .unwrap()
            .iter()
            .filter(|(_, c)| matches!(c.get(1), Some(store_db_read::Value::Text(p)) if p == A))
            .count();
        assert_eq!(a_rows, 1, "A is a single row, not duplicated");
        // B's edge to A is preserved through the upgrade.
        assert_eq!(sorted_closure(&db, B), vec![A.to_string(), B.to_string()]);
    }

    #[test]
    fn registration_text_round_trips_through_parse() {
        // registration_text is the inverse of parse_registration_blocks — a
        // persistent-store read-back writes it so a fresh scratch carries the same
        // registration a real build would (incl. an empty deriver, e.g. a source).
        let regs = vec![
            reg(A, "sha256:aa", &[X]),
            OutputReg {
                store_path: B.to_string(),
                nar_hash: "sha256:bb".to_string(),
                nar_size: 7,
                refs: vec![],
                deriver: String::new(),
            },
        ];
        let parsed = parse_registration_blocks(&registration_text(&regs));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].store_path, A);
        assert_eq!(parsed[0].nar_hash, "sha256:aa");
        assert_eq!(parsed[0].nar_size, 42);
        assert_eq!(parsed[0].refs, vec![X.to_string()]);
        assert_eq!(parsed[0].deriver, format!("{A}.drv"));
        assert_eq!(parsed[1].store_path, B);
        assert_eq!(parsed[1].deriver, "", "an empty deriver round-trips");
    }

    fn one_output_drv(out_path: &str) -> drv::Derivation {
        drv::Derivation {
            outputs: vec![drv::Output {
                name: "out".to_string(),
                path: out_path.to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            platform: String::new(),
            builder: String::new(),
            args: vec![],
            env: vec![],
        }
    }

    #[test]
    fn persistent_realization_hits_stages_and_rejects_miss_or_corrupt() {
        // The cross-invocation SKIP is RECEIPT-GATED (re #469 round-7): reuse needs
        // (a) the engine receipt sidecar for THIS drv matching the CURRENT plan
        // identity, (b) rows whose deriver IS this drv, and (c) a tree that
        // re-verifies. A valid row+tree without a receipt, a receipt for a
        // different plan identity, rows minted for another deriver, an unknown
        // output, or a tampered tree is a MISS (rebuild) — and a corrupt miss
        // stages nothing.
        let tmp = std::env::temp_dir().join(format!("td-persist-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = tmp.join("store");
        let base = "00000000000000000000000000000abc-persist-demo-1";
        let path = format!("/td/store/{base}");
        let deriver = format!("{path}.drv");
        let tree = store.join(base);
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        std::fs::write(tree.join("bin/run"), b"#!/bin/sh\necho hi\n").unwrap();
        // Register with the hash the checker computes (Scanner over the tree, no refs).
        let mut sc = scan::Scanner::new(&[]).unwrap();
        nar::write_nar(&mut sc, &tree).unwrap();
        let (hash, size, _) = sc.finish();
        let reg = OutputReg {
            store_path: path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs: vec![],
            deriver: deriver.clone(),
        };
        let db = tmp.join("db");
        std::fs::write(&db, merge_regs(None, std::slice::from_ref(&reg)).unwrap()).unwrap();
        let sd = store.to_str().unwrap();
        let expect = ReceiptExpect {
            drv_sha256: "11".repeat(32),
            manifest_sha256: "22".repeat(32),
            builder: "/td/store/33333333333333333333333333333333-b/bin/td-builder".to_string(),
        };

        // NO receipt sidecar → MISS even though DB row + tree are valid: a record
        // beside the bytes is not its own authority.
        let s0 = tmp.join("s-noreceipt");
        std::fs::create_dir_all(&s0).unwrap();
        let no_receipt =
            persistent_realization(&one_output_drv(&path), sd, &db, &s0, &expect, &deriver)
                .unwrap();
        assert!(no_receipt.is_none(), "a valid row+tree without an engine receipt must MISS");

        // Write the engine receipt sidecar for this deriver.
        let rp = persist_receipt_path(&db, &deriver).unwrap();
        std::fs::create_dir_all(rp.parent().unwrap()).unwrap();
        std::fs::write(&rp, receipt_text(&expect, std::slice::from_ref(&reg))).unwrap();

        // HIT: staged into scratch/newstore + the reg returned.
        let s1 = tmp.join("s-hit");
        std::fs::create_dir_all(&s1).unwrap();
        let regs = persistent_realization(&one_output_drv(&path), sd, &db, &s1, &expect, &deriver)
            .unwrap()
            .expect("expected a persistent-store HIT");
        assert_eq!(regs[0].store_path, path);
        assert!(s1.join("newstore").join(base).join("bin/run").exists(), "output tree staged into newstore");

        // A DIFFERENT current plan identity (the typed-manifest digest moved) → MISS:
        // the stored receipt cannot vouch a plan it was not issued for.
        let s_id = tmp.join("s-wrong-identity");
        std::fs::create_dir_all(&s_id).unwrap();
        let other = ReceiptExpect { manifest_sha256: "99".repeat(32), ..expect.clone() };
        let wrong_id =
            persistent_realization(&one_output_drv(&path), sd, &db, &s_id, &other, &deriver)
                .unwrap();
        assert!(wrong_id.is_none(), "a receipt issued for another plan identity must MISS");

        // Rows minted for a DIFFERENT deriver → MISS even with a matching sidecar:
        // the ValidPaths row must itself record THIS drv as its producer.
        let alien = "/td/store/00000000000000000000000000000abc-alien.drv";
        let ap = persist_receipt_path(&db, alien).unwrap();
        std::fs::write(&ap, receipt_text(&expect, std::slice::from_ref(&reg))).unwrap();
        let s_al = tmp.join("s-alien-deriver");
        std::fs::create_dir_all(&s_al).unwrap();
        let alien_hit =
            persistent_realization(&one_output_drv(&path), sd, &db, &s_al, &expect, alien)
                .unwrap();
        assert!(alien_hit.is_none(), "rows derived by another drv must not vouch this one");

        // MISS: an output path not registered in the persistent DB.
        let s2 = tmp.join("s-miss");
        std::fs::create_dir_all(&s2).unwrap();
        let miss = persistent_realization(
            &one_output_drv("/td/store/11111111111111111111111111111111-other-1"),
            sd,
            &db,
            &s2,
            &expect,
            &deriver,
        )
        .unwrap();
        assert!(miss.is_none(), "an unregistered output must be a MISS");

        // CORRUPT: the tree no longer matches the registered hash → MISS, nothing staged.
        std::fs::write(tree.join("bin/run"), b"tampered\n").unwrap();
        let s3 = tmp.join("s-corrupt");
        std::fs::create_dir_all(&s3).unwrap();
        let corrupt =
            persistent_realization(&one_output_drv(&path), sd, &db, &s3, &expect, &deriver)
                .unwrap();
        assert!(corrupt.is_none(), "a tree that no longer matches its hash must be a MISS");
        assert!(!s3.join("newstore").join(base).exists(), "a corrupt miss must not leave a staged tree");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_registration_blocks_reads_multi_output() {
        let blob = "path /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-o\n\
                    nar-hash sha256:deadbeef\nnar-size 7\n\
                    reference /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep\n\
                    deriver /gnu/store/cccccccccccccccccccccccccccccccc-o.drv\n";
        let regs = parse_registration_blocks(blob);
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].store_path, "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-o");
        assert_eq!(regs[0].nar_hash, "sha256:deadbeef");
        assert_eq!(regs[0].nar_size, 7);
        assert_eq!(regs[0].refs, vec!["/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep".to_string()]);
        assert_eq!(regs[0].deriver, "/gnu/store/cccccccccccccccccccccccccccccccc-o.drv");
    }

    // TD_BUILD_NICE policy: default 10 when unset/garbage, honor a valid value,
    // clamp to the kernel's -20..=19 nice range. Pure parse, no process state.
    #[test]
    fn build_nice_target_parses_and_clamps() {
        assert_eq!(parse_build_nice(None), 10, "unset -> default 10");
        assert_eq!(parse_build_nice(Some("garbage".into())), 10, "garbage -> default");
        assert_eq!(parse_build_nice(Some("".into())), 10, "empty -> default");
        assert_eq!(parse_build_nice(Some(" 15 ".into())), 15, "trimmed valid value");
        assert_eq!(parse_build_nice(Some("0".into())), 0, "0 is honored (opt out)");
        assert_eq!(parse_build_nice(Some("99".into())), 19, "clamp above max");
        assert_eq!(parse_build_nice(Some("-99".into())), -20, "clamp below min");
    }

    // host-sandbox flag grammar (the loop container). The `--store-at` flag (inc2c)
    // lets the harness be bound at /td/store instead of the hardcoded /gnu/store.
    fn hs(argv: &[&str]) -> Result<HostSandboxArgs, String> {
        let v: Vec<String> = std::iter::once("td-builder".to_string())
            .chain(std::iter::once("host-sandbox".to_string()))
            .chain(argv.iter().map(|s| s.to_string()))
            .collect();
        parse_host_sandbox_args(&v)
    }

    #[test]
    fn host_sandbox_store_at_for_td_store_harness() {
        // The inc2c path: bind td's own harness at /td/store.
        let p = hs(&["--store-from", "/h/store", "--store-at", "/td/store", "--no-daemon",
                     "--", "/td/store/bin/busybox", "sh", "-c", "true"])
            .expect("valid");
        assert_eq!(p.store_from.as_deref(), Some("/h/store"));
        assert_eq!(p.store_at.as_deref(), Some("/td/store"));
        assert!(p.no_daemon, "--no-daemon parsed");
        assert!(!p.expose_cwd);
        assert_eq!(p.cmd, "/td/store/bin/busybox");
        assert_eq!(p.cmd_args, vec!["sh", "-c", "true"]);
    }

    #[test]
    fn host_sandbox_back_compat_no_store_at() {
        // inc2a back-compat: --store-from alone still means "mount at the active
        // store dir" (store_at None — the handler defaults the dest via
        // store::store_dir()), and the daemon/cwd flags parse as before.
        // Asserting store_at==None keeps the default wired here.
        let p = hs(&["--expose-cwd", "--store-from", "/seed", "--", "make", "check"])
            .expect("valid");
        assert_eq!(p.store_from.as_deref(), Some("/seed"));
        assert_eq!(p.store_at, None, "no --store-at -> handler binds at the active store dir");
        assert!(p.expose_cwd);
        assert!(!p.no_daemon);
        assert_eq!(p.cmd, "make");
        assert_eq!(p.cmd_args, vec!["check"]);
    }

    #[test]
    fn host_sandbox_store_at_requires_store_from() {
        // --store-at is meaningless without something to mount: reject it loudly
        // rather than silently bind the host /gnu/store at the wrong place.
        let e = hs(&["--store-at", "/td/store", "--", "true"]).unwrap_err();
        assert!(e.contains("--store-at requires --store-from"), "got: {e}");
    }

    #[test]
    fn host_sandbox_base_binds_never_mount_var_guix() {
        let default = host_sandbox_base_binds(None, None);
        assert!(default.is_empty());
        assert!(default.iter().all(|b| b.src != "/var/guix"));

        let harness = host_sandbox_base_binds(Some("/h/store"), Some("/td/store"));
        assert_eq!(harness.len(), 1);
        assert_eq!(harness[0].src.as_str(), "/h/store");
        assert_eq!(harness[0].dest.as_deref(), Some("/td/store"));
        assert!(harness.iter().all(|b| b.src != "/var/guix"));
    }

    #[test]
    fn host_sandbox_flag_errors() {
        assert!(hs(&["--store-from", "--", "true"]).unwrap_err().contains("--store-from needs a DIR"));
        assert!(hs(&["--store-at", "--", "true"]).unwrap_err().contains("--store-at needs a DIR"));
        assert!(hs(&["--store-item", "--", "true"]).unwrap_err().contains("--store-item needs a PATH"));
        assert!(hs(&["--store-item-at", "/src", "--", "true"]).unwrap_err().contains("--store-item-at needs SRC and DEST"));
        assert!(hs(&["--bogus", "--", "true"]).unwrap_err().contains("unknown flag"));
        // a `--` with no command after it is a usage error (no vacuous empty cmd).
        assert!(hs(&["--expose-cwd", "--"]).unwrap_err().contains("usage:"));
    }

    // --store-item / --store-item-at (the loop's input-only store exposure):
    // repeatable, order preserved, and independent of --store-from (no store
    // DIRECTORY implied). --store-item-at carries a distinct in-sandbox DEST
    // (the td-built userland's durable host copy appears at its /td/store path).
    #[test]
    fn host_sandbox_store_items_repeat_and_stay_directory_free() {
        let p = hs(&["--expose-cwd", "--no-daemon",
                     "--store-item", "/seed/store/aaa-rust-1.93.0",
                     "--store-item-at", "/home/u/.td/loop/bbb-busybox-1.37.0",
                                        "/td/store/bbb-busybox-1.37.0",
                     "--", "gate-run"])
            .expect("valid");
        assert_eq!(p.store_items,
                   vec![("/seed/store/aaa-rust-1.93.0".to_string(), None),
                        ("/home/u/.td/loop/bbb-busybox-1.37.0".to_string(),
                         Some("/td/store/bbb-busybox-1.37.0".to_string()))]);
        assert_eq!(p.store_from, None, "per-item exposure implies no store-dir bind");
        assert_eq!(p.cmd, "gate-run");
    }

    // build_profile --store-native: enumerate the PHYSICAL package dir but point the
    // symlinks at the LOGICAL store path, so the profile resolves in a store-ns own-root.
    #[test]
    fn profile_store_native_links_logical_paths() {
        let dir = std::env::temp_dir().join(format!("prof-sn-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let pkg = dir.join("aaaa-hello");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(pkg.join("bin").join("hello"), b"#!/x\n").unwrap();
        let prof = dir.join("profile");

        // store-native: link target is <prefix>/<basename(pkg)>/bin/hello, NOT the physical pkg.
        let n = build_profile(
            prof.to_str().unwrap(),
            std::slice::from_ref(&pkg.to_string_lossy().into_owned()),
            Some("/td/store"),
        )
        .unwrap();
        assert_eq!(n, 1);
        let link = std::fs::read_link(prof.join("bin").join("hello")).unwrap();
        assert_eq!(link, Path::new("/td/store/aaaa-hello/bin/hello"));

        // default (None): link straight at the physical PKG-OUT entry.
        let n2 = build_profile(
            prof.to_str().unwrap(),
            std::slice::from_ref(&pkg.to_string_lossy().into_owned()),
            None,
        )
        .unwrap();
        assert_eq!(n2, 1);
        let link2 = std::fs::read_link(prof.join("bin").join("hello")).unwrap();
        assert_eq!(link2, pkg.join("bin").join("hello"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // build_profile still refuses a name collision across packages (store-native or not).
    #[test]
    fn profile_rejects_collision() {
        let dir = std::env::temp_dir().join(format!("prof-col-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for p in ["aaaa-a", "bbbb-b"] {
            std::fs::create_dir_all(dir.join(p).join("bin")).unwrap();
            std::fs::write(dir.join(p).join("bin").join("dup"), b"x").unwrap();
        }
        let pkgs = vec![
            dir.join("aaaa-a").to_string_lossy().into_owned(),
            dir.join("bbbb-b").to_string_lossy().into_owned(),
        ];
        let err = build_profile(dir.join("profile").to_str().unwrap(), &pkgs, Some("/td/store"))
            .unwrap_err();
        assert!(err.contains("collision"), "unexpected: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --auto: topo-sort follows the recipe JSONs' `inputs`, ordering deps before
    // dependents, recursing only through OWNED inputs (those with a recipe JSON — #429
    // dropped the base-lock half of ownership); a non-owned input (toolchain seed) is
    // not a node.
    #[test]
    fn auto_topo_orders_deps_before_dependents() {
        let d = std::env::temp_dir().join(format!("td-auto-topo-{}", std::process::id()));
        let rj = d.join("rj");
        std::fs::create_dir_all(&rj).unwrap();
        let put = |name: &str, json: &str| {
            std::fs::write(rj.join(format!("{name}.json")), json).unwrap();
        };
        put("bash", r#"{"name":"bash","inputs":["readline","ncurses","gcc-toolchain"]}"#);
        put("readline", r#"{"name":"readline","inputs":["ncurses"]}"#);
        put("ncurses", r#"{"name":"ncurses"}"#);
        // gcc-toolchain has no recipe JSON → not owned → not a node.
        let rjs = rj.to_string_lossy().to_string();
        let mut order = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = Vec::new();
        auto_topo(&rjs, "bash", &mut order, &mut seen, &mut stack).unwrap();
        assert_eq!(order, vec!["ncurses", "readline", "bash"]);
        std::fs::remove_dir_all(&d).ok();
    }

    // --auto MAP file: `NAME PATH` per line, blank/`#`-comment lines skipped, and the
    // FIRST occurrence of a repeated name wins.
    #[test]
    fn auto_parse_map_skips_blanks_and_comments_first_wins() {
        let text = "# a comment\n\nbash /td/store/aaa-bash\nbash /td/store/zzz-bash-dup\nmake /td/store/bbb-make\n";
        let m = auto_parse_map(text);
        assert_eq!(m.get("bash").map(String::as_str), Some("/td/store/aaa-bash"));
        assert_eq!(m.get("make").map(String::as_str), Some("/td/store/bbb-make"));
        assert_eq!(m.len(), 2);
    }

    // Intern BYTES into a test seed store the way the ladder does: content-address
    // them (`make_store_path_in("source", sha256(NAR), name)`) and place them at
    // their own basename — the only shape `auto_seed_provenance` accepts, now that
    // seed items must self-authenticate.
    fn intern_test_seed(seeds: &Path, name: &str, bytes: &[u8]) -> String {
        let tmp = seeds.join(format!(".tmp-{name}"));
        std::fs::write(&tmp, bytes).unwrap();
        let nar = nar_hash_path(&tmp).unwrap();
        let hex = nar.strip_prefix("sha256:").unwrap();
        let path = store::make_store_path_in("/td/store", "source", hex, name);
        let base = path.rsplit('/').next().unwrap();
        std::fs::rename(&tmp, seeds.join(base)).unwrap();
        path
    }

    fn seed_repo_root() -> PathBuf {
        // builder/ → repo root.
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }

    // Intern the REAL repo bytes of a compiled-table PATCH seed under its key —
    // the one seed class whose bytes live in the repo (cold-CI safe), so
    // green-path tests exercise the same compiled seed-digest gate production
    // does: the derived basename must agree with seed/seed-digests.txt.
    fn intern_real_patch_seed(seeds: &Path, key: &str) -> String {
        let stem = key.strip_prefix("patch-").unwrap();
        let bytes = std::fs::read(
            seed_repo_root().join("seed/patches").join(format!("{stem}.patch")),
        )
        .unwrap();
        let path = intern_test_seed(seeds, key, &bytes);
        let base = path.rsplit('/').next().unwrap();
        assert_eq!(
            seed_digests_expected(key).unwrap(),
            Some(base),
            "repo patch bytes must derive the pinned basename for {key} — regenerate \
             seed/seed-digests.txt if the patch changed"
        );
        path
    }

    // seed-bless (re #469): blessing a declared root set content-scans its
    // closure, records every member's NAR hash into a td-owned db, and the
    // strict staging manifest built from that db (a) vouches exactly the
    // closure, (b) verifies untouched bytes, and (c) reds bytes tampered
    // AFTER the bless — the existence-as-authority hole, closed. Also red:
    // a root that is not in the seed dir cannot be blessed at all.
    #[test]
    fn seed_bless_vouches_the_closure_and_tampering_after_the_bless_reds() {
        let d = std::env::temp_dir().join(format!("td-seed-bless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Two "store items": tool references lib by embedding its store path.
        let lib_base = format!("{}-lib", "b".repeat(32));
        let tool_base = format!("{}-tool", "a".repeat(32));
        let lib_canon = format!("{}/{lib_base}", store::store_dir());
        let tool_canon = format!("{}/{tool_base}", store::store_dir());
        std::fs::write(d.join(&lib_base), b"lib bytes").unwrap();
        std::fs::write(d.join(&tool_base), format!("exec {lib_canon}").as_bytes()).unwrap();
        let db = d.join("bless.db");
        let n = bless_seed_closure(
            &d.to_string_lossy(),
            std::slice::from_ref(&tool_canon),
            &db,
        )
        .unwrap();
        assert_eq!(n, 2, "the scan must pull lib into the blessed closure via tool's bytes");
        let manifest = manifest_from_typed_dbs(&[(
            db.to_string_lossy().into_owned(),
            sandbox::InputOrigin::BlessedSeedClosure,
        )])
        .unwrap();
        for (canon, base) in [(&tool_canon, &tool_base), (&lib_canon, &lib_base)] {
            assert!(manifest.contains_key(canon.as_str()), "{canon} not vouched");
            assert_eq!(
                manifest.get(canon.as_str()).map(|si| si.origin),
                Some(sandbox::InputOrigin::BlessedSeedClosure),
                "{canon} must carry the intake site's declared origin class"
            );
            sandbox::verify_staged_item(&manifest, canon, &d.join(base).to_string_lossy())
                .unwrap();
        }
        // Tamper AFTER the bless: same path, different bytes — refuses to stage.
        std::fs::write(d.join(&lib_base), b"tampered lib bytes").unwrap();
        let err = sandbox::verify_staged_item(
            &manifest,
            &lib_canon,
            &d.join(&lib_base).to_string_lossy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to stage tampered bytes"), "{err}");
        // An item the bless never recorded is not vouched at all.
        let err = sandbox::verify_staged_item(
            &manifest,
            &format!("{}/{}-ghost", store::store_dir(), "c".repeat(32)),
            &d.join(&lib_base).to_string_lossy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no td-owned store-db record"), "{err}");
        // A root that does not live in the seed dir cannot be blessed.
        let err = bless_seed_closure(
            &d.to_string_lossy(),
            &[format!("{}/{}-absent", store::store_dir(), "d".repeat(32))],
            &d.join("bless2.db"),
        )
        .unwrap_err();
        assert!(err.contains("is not in the seed dir"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // authenticate_ca_db (re #469 round-8): a placement db carries authority
    // only for rows whose on-disk bytes reproduce BOTH the recorded NAR hash
    // and the row's own content-addressed name. Green: a properly interned
    // item. Red: bytes tampered after registration (hash leg); a self-
    // consistent item registered under a name its bytes do not derive
    // (name leg — the store-register-over-chosen-bytes forgery the round-8
    // review named).
    #[test]
    fn authenticate_ca_db_admits_only_self_addressing_rows() {
        let d = std::env::temp_dir().join(format!("td-auth-ca-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        // Green: interned bytes, canonical name, matching db row.
        let items = d.join("items");
        std::fs::create_dir_all(&items).unwrap();
        let path = intern_test_seed(&items, "thing-1.0", b"payload");
        let base = path.rsplit('/').next().unwrap();
        let hash = nar_hash_path(&items.join(base)).unwrap();
        let reg = |p: &str, h: &str| OutputReg {
            store_path: p.to_string(),
            nar_hash: h.to_string(),
            nar_size: 7,
            refs: vec![],
            deriver: "/td/store/x.drv".to_string(),
        };
        let good_db = d.join("ca-good.db");
        write_output_db(std::slice::from_ref(&reg(&path, &hash)), &good_db).unwrap();
        authenticate_ca_db(&good_db.to_string_lossy(), &items, "test").unwrap();
        // Red (hash leg): same registration, bytes rewritten after intern.
        // Fresh db path + items dir — authenticate_ca_db memoizes verified
        // (db, items_dir) pairs per process.
        let items2 = d.join("items2");
        std::fs::create_dir_all(&items2).unwrap();
        let path2 = intern_test_seed(&items2, "thing-1.0", b"payload");
        let base2 = path2.rsplit('/').next().unwrap();
        let tamper_db = d.join("ca-tamper.db");
        write_output_db(std::slice::from_ref(&reg(&path2, &hash)), &tamper_db).unwrap();
        std::fs::write(items2.join(base2), b"tampered").unwrap();
        let err = authenticate_ca_db(&tamper_db.to_string_lossy(), &items2, "test").unwrap_err();
        assert!(err.contains("vouches only for bytes it can reproduce"), "{err}");
        // Red (name leg): bytes hash correctly but the claimed store name is
        // not the one those bytes derive — a valid-looking db over chosen
        // bytes at a chosen address.
        let items3 = d.join("items3");
        std::fs::create_dir_all(&items3).unwrap();
        let alien_path = format!("/td/store/{}-alien-1.0", "e".repeat(32));
        let alien_base = alien_path.rsplit('/').next().unwrap();
        std::fs::write(items3.join(alien_base), b"alien bytes").unwrap();
        let alien_hash = nar_hash_path(&items3.join(alien_base)).unwrap();
        let alien_db = d.join("ca-alien.db");
        write_output_db(std::slice::from_ref(&reg(&alien_path, &alien_hash)), &alien_db).unwrap();
        let err = authenticate_ca_db(&alien_db.to_string_lossy(), &items3, "test").unwrap_err();
        assert!(err.contains("do not reproduce its own name"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // A duplicated receipt field is a CONTRADICTION, not a last-wins update:
    // a second `output` line for the same path (or a second producer) must
    // make the whole receipt a non-hit.
    #[test]
    fn receipt_outputs_rejects_duplicated_fields() {
        let expect = ReceiptExpect {
            drv_sha256: "d".repeat(64),
            manifest_sha256: "m".repeat(64),
            builder: "sha256:bb".to_string(),
        };
        let base = format!(
            "td-receipt v1\ndrv-sha256 {}\nmanifest-sha256 {}\nbuilder {}\nproducer local-build\n",
            expect.drv_sha256, expect.manifest_sha256, expect.builder
        );
        let good = format!("{base}output /td/store/x-1.0 sha256:{}\n", "a".repeat(64));
        assert!(receipt_outputs(&good, &expect).is_some());
        let dup_output = format!(
            "{base}output /td/store/x-1.0 sha256:{}\noutput /td/store/x-1.0 sha256:{}\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(receipt_outputs(&dup_output, &expect).is_none());
        let dup_producer = format!("{base}producer local-build\n");
        assert!(receipt_outputs(&dup_producer, &expect).is_none());
    }

    // re #469: the reuse-key digest (`ReceiptExpect.manifest_sha256`) is
    // CLOSURE-SCOPED to the drv's OWN transitive input closure, not the plan-wide
    // manifest union. The seed db is interned per TARGET graph, so a higher target
    // folds unrelated seeds into the shared manifest; under the old plan-wide digest
    // the SAME drv got a DIFFERENT reuse key across targets — a receipt-identity miss
    // that rebuilt an already-valid rung and then collided with its cached tree. The
    // scoped key must be IDENTICAL across the two plans, yet still MOVE when a real
    // input of THIS drv — or the builder's ABI identity — changes.
    #[test]
    fn reuse_key_digest_is_scoped_to_the_drvs_own_closure() {
        let src = format!("/td/store/{}-mes-source", "a".repeat(32));
        let seed_a = format!("/td/store/{}-tinycc-seed", "b".repeat(32));
        // `builder` is the ABI-keyed builder-identity store path (input-addressed on the
        // BUILDER_ABI token, content-INDEPENDENT); `builder_identity` is the drv's declared
        // `parsed.builder` = `{identity}/bin/td-builder`, exactly what the reuse key folds.
        let builder = format!("/td/store/{}-td-builder", "9".repeat(32));
        let builder_identity = format!("{builder}/bin/td-builder");
        // seed_x/seed_y are OTHER rungs' seeds a higher target interns into the shared
        // seed db — outside THIS drv's closure.
        let seed_x = format!("/gnu/store/{}-gcc-seed", "c".repeat(32));
        let seed_y = format!("/gnu/store/{}-glibc-seed", "d".repeat(32));

        // THIS drv's transitive input closure: src, a seed (with an on-disk half, to
        // exercise split_closure_entry's canonical extraction), and the real builder.
        let closure = vec![
            src.clone(),
            format!("{seed_a}\t/seed/on-disk/tinycc"),
            format!("{builder}\t/td/store-disk/td-builder"),
        ];

        let si = |h: &str, o: sandbox::InputOrigin| sandbox::StagedInput {
            nar_hash: format!("sha256:{h}"),
            origin: o,
        };
        // The LOW target's manifest: exactly the drv's own closure.
        let mut low: sandbox::StageManifest = sandbox::StageManifest::new();
        low.insert(src.clone(), si("11", sandbox::InputOrigin::AuditedSeed));
        low.insert(seed_a.clone(), si("22", sandbox::InputOrigin::AuditedSeed));
        low.insert(builder.clone(), si("bb", sandbox::InputOrigin::ControlPlaneBuilder));
        // The HIGH target's manifest: the same closure PLUS unrelated seeds folded into
        // the shared seed db.
        let mut high = low.clone();
        high.insert(seed_x, si("33", sandbox::InputOrigin::AuditedSeed));
        high.insert(seed_y.clone(), si("44", sandbox::InputOrigin::AuditedSeed));

        // The OLD bug: the plan-wide digest DIFFERS across targets even though the drv
        // and its own closure are identical — the receipt-identity miss.
        assert_ne!(
            manifest_digest(&low),
            manifest_digest(&high),
            "the plan-wide digest drifts with unrelated seeds — the bug this scopes away"
        );
        // The FIX: the closure-scoped reuse key is IDENTICAL across the two plans. The
        // builder is staged as a content-addressed override (its row is in-closure), so
        // real_builder_cb = the builder's content path — the row the digest EXCLUDES.
        let rb = Some(builder.as_str());
        let low_key = reuse_key_manifest_digest(&closure, &low, &builder_identity, rb);
        let high_key = reuse_key_manifest_digest(&closure, &high, &builder_identity, rb);
        assert_eq!(
            low_key, high_key,
            "the reuse key must not drift with inputs outside the drv's own closure"
        );
        // An unrelated seed ADDED to the plan (outside the closure) never moves the key.
        let mut plus_outside = low.clone();
        plus_outside.insert(
            format!("/gnu/store/{}-unrelated", "e".repeat(32)),
            si("ee", sandbox::InputOrigin::AuditedSeed),
        );
        assert_eq!(
            low_key,
            reuse_key_manifest_digest(&closure, &plus_outside, &builder_identity, rb),
            "an input outside the closure must not move the key"
        );

        // Binding preserved: CHANGING an in-closure NON-builder input's hash moves the key
        // (the exclusion is PRECISE — only the builder's own row is dropped).
        let mut changed = low.clone();
        changed.insert(seed_a.clone(), si("ff", sandbox::InputOrigin::AuditedSeed));
        assert_ne!(
            low_key,
            reuse_key_manifest_digest(&closure, &changed, &builder_identity, rb),
            "a changed hash on an in-closure input must move the key"
        );
        // REMOVING an in-closure input moves the key too — the binding to real inputs stands.
        let mut removed = low.clone();
        removed.remove(seed_a.as_str());
        assert_ne!(
            low_key,
            reuse_key_manifest_digest(&closure, &removed, &builder_identity, rb),
            "dropping an in-closure input must move the key"
        );

        // P1-B: the key folds the drv's DECLARED ABI builder IDENTITY (parsed.builder),
        // content-INDEPENDENT — the SAME string ReceiptExpect.builder records. A builder-ELF
        // RECOMPILE that leaves BUILDER_ABI alone leaves parsed.builder unchanged, so the
        // call sites fold the SAME identity → the SAME key, and an output-neutral rebuild
        // does NOT invalidate reuse (the fix: outputs already key on this ABI identity via
        // store::builder_identity_path, so the reuse key must too). A BUILDER_ABI bump
        // changes the identity → the key MOVES. (The call sites pass parsed.builder, NOT the
        // resolved builder_exec — that contract IS the fix; this function only sees, and
        // binds, whatever identity it is handed.) The `bare` case below ISOLATES the fold leg:
        // handed a closure with no builder row at all, the identity fold alone still moves the
        // key — proving the fold binds the identity independent of any manifest row.
        let bumped_identity = format!("/td/store/{}-td-builder/bin/td-builder", "8".repeat(32));
        assert_ne!(
            low_key,
            reuse_key_manifest_digest(&closure, &low, &bumped_identity, rb),
            "a BUILDER_ABI-identity change must move the key"
        );
        let bare = vec![src.clone(), seed_a.clone()]; // no builder in the closure/manifest
        assert_ne!(
            reuse_key_manifest_digest(&bare, &low, &builder_identity, None),
            reuse_key_manifest_digest(&bare, &low, &bumped_identity, None),
            "the identity fold binds the key independent of any builder manifest row"
        );
    }

    // re #469 (P1#1, cross-model review): the LADDER stages the builder as a
    // content-addressed OVERRIDE (`TD_BUILDER_PATH = place_stage0_builder(...)`), so the
    // builder's own row IS in every recipe drv's closure and its nar_hash + store path move
    // on EVERY builder recompile — even output-neutral ones. Folding parsed.builder alone
    // did NOT fix that: the excluded-row leg is what makes an override recompile
    // reuse-stable. This asserts the property the ladder actually needs — recompiling the
    // builder (new content path AND new bytes) with the SAME ABI identity does NOT move the
    // reuse key — and proves the exclusion is load-bearing (without it, the key WOULD move).
    #[test]
    fn reuse_key_is_stable_across_a_same_abi_builder_recompile() {
        let src = format!("/td/store/{}-mes-source", "a".repeat(32));
        let seed = format!("/td/store/{}-tinycc-seed", "b".repeat(32));
        // The ABI-keyed identity path the drv NAMES — unchanged by a recompile (it is a pure
        // function of BUILDER_ABI). Both builds fold this SAME string.
        let identity = format!("/td/store/{}-td-builder/bin/td-builder", "9".repeat(32));

        // Build v1 and v2 are the SAME builder recompiled: DIFFERENT content path (v1 vs v2)
        // AND different bytes (nar_hash b1 vs b2), same ABI identity. This is exactly what
        // `place_stage0_builder` produces across a rebuild of td-builder.
        let builder_v1 = format!("/td/store/{}-td-builder", "1".repeat(32));
        let builder_v2 = format!("/td/store/{}-td-builder", "2".repeat(32));

        let si = |h: &str, o: sandbox::InputOrigin| sandbox::StagedInput {
            nar_hash: format!("sha256:{h}"),
            origin: o,
        };
        let mk = |bpath: &str, bhash: &str| {
            // The drv's closure carries the builder at its real content path (pre-rekey),
            // plus the drv's genuine inputs.
            let closure = vec![src.clone(), seed.clone(), bpath.to_string()];
            let mut m: sandbox::StageManifest = sandbox::StageManifest::new();
            m.insert(src.clone(), si("11", sandbox::InputOrigin::AuditedSeed));
            m.insert(seed.clone(), si("22", sandbox::InputOrigin::AuditedSeed));
            m.insert(bpath.to_string(), si(bhash, sandbox::InputOrigin::ControlPlaneBuilder));
            (closure, m)
        };
        let (cl1, m1) = mk(&builder_v1, "b1");
        let (cl2, m2) = mk(&builder_v2, "b2");

        // THE PROPERTY: recompiling the builder does NOT move the reuse key — the builder's
        // own content row is excluded and the stable ABI identity binds it instead.
        assert_eq!(
            reuse_key_manifest_digest(&cl1, &m1, &identity, Some(&builder_v1)),
            reuse_key_manifest_digest(&cl2, &m2, &identity, Some(&builder_v2)),
            "an output-neutral builder recompile (override) must not move the reuse key"
        );
        // The exclusion is LOAD-BEARING: without it (passing None so the builder row is
        // scoped in) the two keys WOULD differ — this is precisely the P1#1 bust.
        assert_ne!(
            reuse_key_manifest_digest(&cl1, &m1, &identity, None),
            reuse_key_manifest_digest(&cl2, &m2, &identity, None),
            "without the exclusion the builder's content row busts the key — the bug"
        );
        // The exclusion is PRECISE: a NON-builder input change still moves the key.
        let mut m1b = m1.clone();
        m1b.insert(seed.clone(), si("ff", sandbox::InputOrigin::AuditedSeed));
        assert_ne!(
            reuse_key_manifest_digest(&cl1, &m1, &identity, Some(&builder_v1)),
            reuse_key_manifest_digest(&cl1, &m1b, &identity, Some(&builder_v1)),
            "a real (non-builder) input change must still move the key"
        );

        // The exclusion's ORIGIN GATE is precise (Agy finding #1): the row is dropped ONLY
        // when its origin is ControlPlaneBuilder (the DRIVER). A row AT the builder path but
        // with a DATA origin (AuditedSeed) is NOT excluded — so were the builder ever a genuine
        // recipe data input, its bytes would still bind. Same path, data origin, different hash
        // → key MOVES (unlike the driver-origin recompile above, which does not).
        let data_at_builder = |h: &str| {
            let mut m: sandbox::StageManifest = sandbox::StageManifest::new();
            m.insert(src.clone(), si("11", sandbox::InputOrigin::AuditedSeed));
            m.insert(seed.clone(), si("22", sandbox::InputOrigin::AuditedSeed));
            m.insert(builder_v1.clone(), si(h, sandbox::InputOrigin::AuditedSeed));
            m
        };
        assert_ne!(
            reuse_key_manifest_digest(&cl1, &data_at_builder("d1"), &identity, Some(&builder_v1)),
            reuse_key_manifest_digest(&cl1, &data_at_builder("d2"), &identity, Some(&builder_v1)),
            "a DATA-origin row at the builder path must NOT be excluded — it must still bind"
        );

        // Codex P1: an UNVOUCHED closure member (in the closure, NO manifest row — e.g. a new
        // builder runtime dep a same-ABI recompile pulled in) MOVES the key, so the read sites
        // (which skip enforcement) cannot return a spurious hit; the forced miss rebuilds and
        // re-runs enforce_realize_input_policy, which rejects the unvouched member. The digest is
        // closure-driven, so the extra path lands in `absent` and perturbs the key even with no
        // row of its own.
        let dep = format!("/td/store/{}-newlib", "7".repeat(32));
        let mut cl_with_dep = cl1.clone();
        cl_with_dep.push(dep.clone()); // in the closure, but deliberately absent from m1
        assert!(!m1.contains_key(&dep), "dep must be unvouched for this test");
        assert_ne!(
            reuse_key_manifest_digest(&cl1, &m1, &identity, Some(&builder_v1)),
            reuse_key_manifest_digest(&cl_with_dep, &m1, &identity, Some(&builder_v1)),
            "an unvouched closure member must move the key (forces a miss → enforcement)"
        );
    }

    // drv_declared_inputs (the reuse-key/closure ROOTS): input-srcs pass through in
    // order, and each input-drv is resolved by READING the input .drv and looking up
    // the NAMED output; an unknown output name is an error (not a silent skip).
    #[test]
    fn drv_declared_inputs_resolves_srcs_and_input_drvs() {
        let tmp = std::env::temp_dir().join(format!("td-declared-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // A real input .drv with two named outputs, serialized the way drv::parse reads.
        let dep_out = format!("/td/store/{}-dep-out", "1".repeat(32));
        let dep_lib = format!("/td/store/{}-dep-lib", "2".repeat(32));
        let dep = drv::Derivation {
            outputs: vec![
                drv::Output { name: "out".to_string(), path: dep_out.clone(), hash_algo: String::new(), hash: String::new() },
                drv::Output { name: "lib".to_string(), path: dep_lib.clone(), hash_algo: String::new(), hash: String::new() },
            ],
            input_drvs: vec![],
            input_srcs: vec![],
            platform: "x86_64-linux".to_string(),
            builder: String::new(),
            args: vec![],
            env: vec![],
        };
        let dep_path = tmp.join("dep.drv");
        std::fs::write(&dep_path, drv::serialize(&dep)).unwrap();

        let src = format!("/td/store/{}-src", "a".repeat(32));
        let mut parent = one_output_drv(&format!("/td/store/{}-parent", "e".repeat(32)));
        parent.input_srcs = vec![src.clone()];
        // Request BOTH the `lib` and `out` outputs of the dep (order as written).
        parent.input_drvs =
            vec![(dep_path.to_string_lossy().into_owned(), vec!["lib".to_string(), "out".to_string()])];

        let roots = drv_declared_inputs(&parent).unwrap();
        // input-srcs first (verbatim), then the resolved input-drv outputs in request order.
        assert_eq!(roots, vec![src.clone(), dep_lib.clone(), dep_out.clone()]);

        // An unknown output NAME is a hard error, never a silent drop.
        parent.input_drvs =
            vec![(dep_path.to_string_lossy().into_owned(), vec!["nope".to_string()])];
        let err = drv_declared_inputs(&parent).unwrap_err();
        assert!(err.contains("has no output `nope'"), "{err}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // authenticate_recipe_output_db (re #469 round-8): `--recipe-output-db` is
    // public argv, so a registration db types RecipeOutput only when every row
    // is backed by an engine-issued `producer local-build` receipt recording
    // exactly that (path, hash). Red: no receipts dir (a store-register'd db);
    // an unbacked row; a receipt disagreeing with the row; a non-local
    // producer. Green: the receipt commit_scratch_to_store writes.
    #[test]
    fn authenticate_recipe_output_db_requires_receipt_backing() {
        let d = std::env::temp_dir().join(format!("td-auth-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let out_path = format!("/td/store/{}-out-1.0", "a".repeat(32));
        let db = d.join("ro.db");
        write_output_db(
            std::slice::from_ref(&OutputReg {
                store_path: out_path.clone(),
                nar_hash: "sha256:deadbeef".to_string(),
                nar_size: 7,
                refs: vec![],
                deriver: "/td/store/x.drv".to_string(),
            }),
            &db,
        )
        .unwrap();
        let dbp = db.to_string_lossy().into_owned();
        // Red: no receipts at all — the exact store-register forgery shape.
        let err = authenticate_recipe_output_db(&dbp).unwrap_err();
        assert!(err.contains("no engine-issued receipts"), "{err}");
        // Red: receipts dir exists but nothing vouches for the row.
        let rdir = d.join("ro.db.receipts");
        std::fs::create_dir_all(&rdir).unwrap();
        let err = authenticate_recipe_output_db(&dbp).unwrap_err();
        assert!(err.contains("no engine-issued receipt vouches"), "{err}");
        // Red: a receipt exists but records a different hash for the path.
        let receipt = rdir.join("x.receipt");
        let body = |hash: &str, producer: &str| {
            format!(
                "td-receipt v1\ndrv-sha256 d\nmanifest-sha256 m\nbuilder b\n\
                 producer {producer}\noutput {out_path} {hash} 7\n"
            )
        };
        std::fs::write(&receipt, body("sha256:beefdead", "local-build")).unwrap();
        let err = authenticate_recipe_output_db(&dbp).unwrap_err();
        assert!(err.contains("but its receipt records"), "{err}");
        // Red: right hash, wrong producer — a non-local record backs nothing.
        std::fs::write(&receipt, body("sha256:deadbeef", "substitute")).unwrap();
        let err = authenticate_recipe_output_db(&dbp).unwrap_err();
        assert!(err.contains("no engine-issued receipt vouches"), "{err}");
        // Green: the engine-issued local-build receipt backing the row.
        std::fs::write(&receipt, body("sha256:deadbeef", "local-build")).unwrap();
        authenticate_recipe_output_db(&dbp).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    // Round-9 P1 regression: the sidecar the ENGINE writes (persist_receipt_path)
    // must be the very file authenticate_recipe_output_db reads — the reader
    // accepts only `*.receipt`, and the pre-fix writer used the bare drv
    // basename, so every engine-produced db failed `--recipe-output-db` intake.
    #[test]
    fn engine_written_receipts_pass_recipe_output_db_intake() {
        let d = std::env::temp_dir().join(format!("td-receipt-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let out_path = format!("/td/store/{}-out-1.0", "b".repeat(32));
        let deriver = format!("/td/store/{}-out-1.0.drv", "c".repeat(32));
        let reg = OutputReg {
            store_path: out_path.clone(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 7,
            refs: vec![],
            deriver: deriver.clone(),
        };
        let db = d.join("ro.db");
        write_output_db(std::slice::from_ref(&reg), &db).unwrap();
        let expect = ReceiptExpect {
            drv_sha256: "11".repeat(32),
            manifest_sha256: "22".repeat(32),
            builder: "b".to_string(),
        };
        let rp = persist_receipt_path(&db, &deriver).unwrap();
        assert_eq!(
            rp.extension().and_then(|e| e.to_str()),
            Some("receipt"),
            "the engine sidecar must carry the .receipt suffix the intake reader requires"
        );
        std::fs::create_dir_all(rp.parent().unwrap()).unwrap();
        std::fs::write(&rp, receipt_text(&expect, std::slice::from_ref(&reg))).unwrap();
        authenticate_recipe_output_db(&db.to_string_lossy()).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    // re #469 round-10 P0 #1: the staging-boundary host-tool policy. The daemon
    // realizes drvs no planner saw, so a crafted drv could (a) SELECT blessed
    // host tools (bash/coreutils/tar/gzip live in the blessed seed closure) as
    // inputs, or (b) name one as its BUILDER. Both must red at the bind
    // boundary; the builder's own runtime closure (glibc/gcc-lib) must stay
    // green — that is the only slice the bless db exists to vouch.
    #[test]
    fn blessed_seed_items_vouch_only_the_builder_runtime_closure() {
        let builder_tree = format!("/gnu/store/{}-td-builder-0.1.0", "a".repeat(32));
        let builder = format!("{builder_tree}/bin/td-builder");
        let glibc = format!("/gnu/store/{}-glibc-2.41", "b".repeat(32));
        let bash = format!("/gnu/store/{}-bash-5.2.37", "c".repeat(32));
        let si = |origin| sandbox::StagedInput { nar_hash: "sha256:00".to_string(), origin };
        let mut manifest = sandbox::StageManifest::new();
        manifest.insert(builder_tree.clone(), si(sandbox::InputOrigin::ControlPlaneBuilder));
        manifest.insert(glibc.clone(), si(sandbox::InputOrigin::BlessedSeedClosure));
        manifest.insert(bash.clone(), si(sandbox::InputOrigin::BlessedSeedClosure));
        let roots = vec![builder_tree.clone(), bash.clone()];
        let reach: std::collections::BTreeSet<String> =
            [builder_tree.clone(), glibc.clone()].into_iter().collect();
        // Green: the builder plus its glibc runtime — the legitimate bless slice.
        let ok_closure = vec![builder_tree.clone(), glibc.clone()];
        enforce_realize_input_policy(&builder, &roots, &ok_closure, &reach, &manifest, None)
            .unwrap();
        // Red: the drv additionally selected blessed host bash as an INPUT.
        let bad_closure = vec![builder_tree, glibc, bash];
        let err =
            enforce_realize_input_policy(&builder, &roots, &bad_closure, &reach, &manifest, None)
                .unwrap_err();
        assert!(err.contains("host tools are not admissible"), "{err}");
    }

    // re #469 round-10: builder-identity leg of the policy. A blessed host tool
    // named as the drv's BUILDER reds even though its bytes are vouched and
    // reachable; a RecipeOutput-typed executable (a td-built tool) and the
    // engine's own tree stay admissible.
    #[test]
    fn a_host_tool_is_never_a_drv_builder() {
        let bash_tree = format!("/gnu/store/{}-bash-5.2.37", "c".repeat(32));
        let builder = format!("{bash_tree}/bin/bash");
        let si = |origin| sandbox::StagedInput { nar_hash: "sha256:00".to_string(), origin };
        let mut manifest = sandbox::StageManifest::new();
        manifest.insert(bash_tree.clone(), si(sandbox::InputOrigin::BlessedSeedClosure));
        let roots = vec![bash_tree.clone()];
        let reach: std::collections::BTreeSet<String> =
            [bash_tree.clone()].into_iter().collect();
        let closure = vec![bash_tree.clone()];
        let err = enforce_realize_input_policy(&builder, &roots, &closure, &reach, &manifest, None)
            .unwrap_err();
        assert!(err.contains("not admissible executable provenance"), "{err}");
        // A td recipe output IS an admissible builder.
        let tool_tree = format!("/gnu/store/{}-td-tool-1.0", "d".repeat(32));
        let tool = format!("{tool_tree}/bin/td-tool");
        let mut m2 = sandbox::StageManifest::new();
        m2.insert(tool_tree.clone(), si(sandbox::InputOrigin::RecipeOutput));
        enforce_realize_input_policy(
            &tool,
            std::slice::from_ref(&tool_tree),
            std::slice::from_ref(&tool_tree),
            &std::collections::BTreeSet::new(),
            &m2,
            None,
        )
        .unwrap();
        // The engine realizing with ITSELF is admissible (the self carve-out).
        let self_tree = format!("/gnu/store/{}-td-builder-0.1.0", "e".repeat(32));
        let self_builder = format!("{self_tree}/bin/td-builder");
        enforce_realize_input_policy(
            &self_builder,
            &[],
            &[],
            &std::collections::BTreeSet::new(),
            &sandbox::StageManifest::new(),
            Some(&self_tree),
        )
        .unwrap();
    }

    // re #469 round-10 P0 #2: a self-content-addressed tree with NO stage0
    // lineage record cannot be typed `ControlPlaneBuilder` — content addressing
    // proves integrity, not that the bytes came from `stage0-place`'s own
    // compile of this repo's builder/ source.
    #[test]
    fn an_unrecorded_builder_tree_cannot_be_typed_control_plane() {
        let d = std::env::temp_dir().join(format!("td-lineage-intake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let canonical = format!("/gnu/store/{}-not-a-stage0-1.0", "f".repeat(32));
        let reg = OutputReg {
            store_path: canonical.clone(),
            nar_hash: format!("sha256:{}", "ab".repeat(32)),
            nar_size: 1,
            refs: vec![],
            deriver: String::new(),
        };
        let db = d.join("builder.db");
        write_output_db(std::slice::from_ref(&reg), &db).unwrap();
        let ov = BuilderOverride {
            canonical,
            on_disk: d.join("tree").display().to_string(),
            db: db.display().to_string(),
        };
        let err = verify_builder_lineage(&ov).unwrap_err();
        assert!(err.contains("no stage0 lineage record"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // authenticate_seed_db (re #469 round-8): the plan seed db can vouch only
    // for the pinned seed universe — every row must content-address to itself
    // AND land on a basename the compiled seed-digest table derives. Green:
    // a real repo patch seed (its basename IS pinned). Red: a CA-valid item
    // the table never pinned. Absent db: vacuous (no rows, no authority).
    #[test]
    fn authenticate_seed_db_rejects_unpinned_rows_and_passes_pinned_ones() {
        let d = std::env::temp_dir().join(format!("td-auth-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let reg = |p: &str, h: &str| OutputReg {
            store_path: p.to_string(),
            nar_hash: h.to_string(),
            nar_size: 7,
            refs: vec![],
            deriver: "/td/store/x.drv".to_string(),
        };
        // Green: the interned repo patch derives a pinned basename.
        let items = d.join("items");
        std::fs::create_dir_all(&items).unwrap();
        let path = intern_real_patch_seed(&items, "patch-glibc-boot-2.16.0");
        let base = path.rsplit('/').next().unwrap();
        let hash = nar_hash_path(&items.join(base)).unwrap();
        let good_db = d.join("seed-good.db");
        write_output_db(std::slice::from_ref(&reg(&path, &hash)), &good_db).unwrap();
        authenticate_seed_db(&good_db.to_string_lossy(), &items).unwrap();
        // Red: CA-valid but the compiled table never pinned this basename.
        let items2 = d.join("items2");
        std::fs::create_dir_all(&items2).unwrap();
        let upath = intern_test_seed(&items2, "unpinned-1.0", b"x");
        let ubase = upath.rsplit('/').next().unwrap();
        let uhash = nar_hash_path(&items2.join(ubase)).unwrap();
        let bad_db = d.join("seed-bad.db");
        write_output_db(std::slice::from_ref(&reg(&upath, &uhash)), &bad_db).unwrap();
        let err = authenticate_seed_db(&bad_db.to_string_lossy(), &items2).unwrap_err();
        assert!(err.contains("not a basename the compiled seed-digest table pins"), "{err}");
        // Absent db: authenticates vacuously — no rows means no authority.
        authenticate_seed_db(&d.join("absent.db").to_string_lossy(), &items).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    // authenticate_seed_capture_db (re #469 round-8): TD_SEED_DB types
    // AuditedSeed only through the seed-unpack sidecar binding the db to a
    // capture tarball whose sha256 the COMPILED pins file admits, with the db
    // bytes themselves fixed by the sidecar. Red: no sidecar; a post-write db
    // edit; an unpinned capture (including against the SHIPPED pins file,
    // which pins nothing yet — the fail-closed default). Green: pinned
    // capture + untouched db, via the pins-parameterized core.
    #[test]
    fn authenticate_seed_capture_db_binds_capture_and_db_bytes() {
        let d = std::env::temp_dir().join(format!("td-auth-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("cap.db");
        std::fs::write(&db, b"db bytes").unwrap();
        let dbp = db.to_string_lossy().into_owned();
        // Red: a db with no sidecar (any db seed-unpack did not write).
        let err = authenticate_seed_capture_db(&dbp).unwrap_err();
        assert!(err.contains("no seed-tarball binding"), "{err}");
        let tb_sha = sha256_hex(b"tarball bytes");
        let db_sha = sha256_hex(b"db bytes");
        std::fs::write(
            d.join("cap.db.seed-tarball"),
            format!("sha256 {tb_sha} seed.tar\ndb-sha256 {db_sha}\n"),
        )
        .unwrap();
        // Red: the SHIPPED pins file admits nothing yet — fail closed.
        let err = authenticate_seed_capture_db(&dbp).unwrap_err();
        assert!(err.contains("is not pinned"), "{err}");
        // Green: a pins file that admits this capture.
        let pins = format!("# audited\n{tb_sha} frozen seed capture\n");
        authenticate_seed_capture_db_with(&dbp, &pins).unwrap();
        // Red: db bytes changed after seed-unpack wrote the sidecar.
        std::fs::write(&db, b"db bytes edited").unwrap();
        let err = authenticate_seed_capture_db_with(&dbp, &pins).unwrap_err();
        assert!(err.contains("modified after seed-unpack"), "{err}");
        std::fs::write(&db, b"db bytes").unwrap();
        // Red: a malformed pin line is a hard error, not a skip.
        let err = authenticate_seed_capture_db_with(&dbp, "not-a-sha admit-all\n").unwrap_err();
        assert!(err.contains("malformed pin"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // Every compiled-table row backed by an in-repo patch file must recompute
    // from those bytes — the cold-CI freshness gate for the table: a changed
    // patch without a regenerated table reds here, not first at ladder time.
    #[test]
    fn seed_digest_patch_rows_recompute_from_the_repo_bytes() {
        let mut verified = 0usize;
        for line in SEED_DIGESTS.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(key), Some(pinned)) = (it.next(), it.next()) else {
                panic!("malformed row {line}");
            };
            let Some(stem) = key.strip_prefix("patch-") else {
                continue;
            };
            let file = seed_repo_root().join("seed/patches").join(format!("{stem}.patch"));
            if !file.is_file() {
                continue; // a `patch-`-prefixed SOURCE pin (tarball), not a repo patch
            }
            let nar = nar_hash_path(&file).unwrap();
            let hex = nar.strip_prefix("sha256:").unwrap();
            let derived = store::make_store_path_in("/td/store", "source", hex, key);
            let derived_base = derived.rsplit('/').next().unwrap();
            assert_eq!(
                derived_base, pinned,
                "seed/seed-digests.txt row for {key} does not match the repo patch bytes — \
                 regenerate with `td-recipe-eval seed-digests`"
            );
            verified += 1;
        }
        assert!(verified > 0, "no in-repo patch rows verified — table missing patches?");
    }

    // --auto: synthesize a recipe's WHOLE lock straight from its declared graph — no
    // hand-written base lock (#429). An owned dep (its own recipe JSON exists) becomes a
    // `td-recipe-output` pending placeholder; every other declared input resolves through
    // MAP as `seed` — and only if its path is a canonical store item actually interned
    // in the seed store AND its bytes content-address to that path; the declared
    // `sourceInput` becomes the `<name>-source` line.
    #[test]
    fn auto_synthesize_lock_marks_owned_deps_and_resolves_the_rest() {
        let d = std::env::temp_dir().join(format!("td-auto-synth-{}", std::process::id()));
        let seeds = d.join("seed-store");
        std::fs::create_dir_all(&seeds).unwrap();
        // Seed inputs must be REAL compiled-table keys with their audited repo
        // bytes — the table gate reds anything else at synthesis now.
        std::fs::write(
            d.join("gcc-mesboot0.json"),
            r#"{"name":"gcc-mesboot0","sourceInput":"patch-gcc-boot-2.95.3","nativeInputs":["binutils-mesboot0"],"inputs":["patch-glibc-boot-2.16.0","patch-glibc-boot-2.2.5"]}"#,
        )
        .unwrap();
        std::fs::write(d.join("binutils-mesboot0.json"), r#"{"name":"binutils-mesboot0"}"#).unwrap();
        let mut map = std::collections::BTreeMap::new();
        let src = intern_real_patch_seed(&seeds, "patch-gcc-boot-2.95.3");
        let in_a = intern_real_patch_seed(&seeds, "patch-glibc-boot-2.16.0");
        let in_b = intern_real_patch_seed(&seeds, "patch-glibc-boot-2.2.5");
        map.insert("patch-gcc-boot-2.95.3".to_string(), src.clone());
        map.insert("patch-glibc-boot-2.16.0".to_string(), in_a.clone());
        map.insert("patch-glibc-boot-2.2.5".to_string(), in_b.clone());
        let got =
            auto_synthesize_lock(&d.to_string_lossy(), &map, "gcc-mesboot0", "/td/store", &seeds)
                .unwrap();
        assert!(got.contains(&format!("gcc-mesboot0-source {src} source")));
        assert!(got.contains("binutils-mesboot0 /td/store/pending-binutils-mesboot0 td-recipe-output"));
        assert!(got.contains(&format!("patch-glibc-boot-2.16.0 {in_a} seed")));
        assert!(got.contains(&format!("patch-glibc-boot-2.2.5 {in_b} seed")));
        std::fs::remove_dir_all(&d).ok();
    }

    // build_plan re-gates every seed/source lock entry by its digest key. A recipe's
    // own source is named `{name}-source` in the lock (so steps reference
    // `{in:{name}-source}`) but pinned under its `sourceInput` key — which for a
    // shared/renamed seed differs (gcc-mesboot0-source <- patch-gcc-boot-2.95.3,
    // mesboot-headers-source <- linux-headers). seed_gate_key must recover the pin key
    // for that entry, and gate every other seed by its own name; else the re-gate reds
    // a valid rung with "no compiled expected digest" for `{name}-source`.
    #[test]
    fn seed_gate_key_resolves_a_recipe_source_to_its_pin_key() {
        let renamed = json::parse(r#"{"name":"mesboot-headers","sourceInput":"linux-headers"}"#).unwrap();
        // The rung's own source entry gates by the sourceInput pin key, not its name.
        assert_eq!(
            seed_gate_key("mesboot-headers-source", "mesboot-headers-source", &renamed),
            "linux-headers"
        );
        // A sibling seed entry gates by its OWN name even though a sourceInput exists.
        assert_eq!(
            seed_gate_key("patch-glibc-boot-2.16.0", "mesboot-headers-source", &renamed),
            "patch-glibc-boot-2.16.0"
        );
        // A conventional rung (sourceInput == `{name}-source`) is unchanged.
        let conventional = json::parse(r#"{"name":"mes","sourceInput":"mes-source"}"#).unwrap();
        assert_eq!(seed_gate_key("mes-source", "mes-source", &conventional), "mes-source");
        // A recipe with no sourceInput falls back to the entry name.
        let none = json::parse(r#"{"name":"make-test"}"#).unwrap();
        assert_eq!(seed_gate_key("make-test-source", "make-test-source", &none), "make-test-source");
    }

    // The registered-host-item behavioral reds (re #469): `store-add-recursive` is a
    // public verb, so a seed store + db pair can be MADE to vouch for arbitrary host
    // bytes. Planning must therefore not accept existence — or even honest
    // self-consistency — as authority. Three layers, each red separately below:
    // an UNPINNED key reds against the compiled seed-digest table whatever it
    // resolves to; a pinned key resolving to an honestly content-addressed item
    // the pins never derived (the forged-but-self-consistent store) reds against
    // the table's basename; and an item whose bytes were swapped after interning
    // reds at the NAR recompute. Verified red: before these checks, every case
    // below PASSED `auto_seed_provenance`.
    #[test]
    fn auto_seed_provenance_rejects_self_registered_and_tampered_items() {
        let d = std::env::temp_dir().join(format!("td-auto-selfreg-{}", std::process::id()));
        let seeds = d.join("seed-store");
        std::fs::create_dir_all(&seeds).unwrap();
        // Green: the audited repo bytes at the pinned basename pass.
        let good = intern_real_patch_seed(&seeds, "patch-glibc-boot-2.16.0");
        auto_seed_provenance("/td/store", &seeds, "glibc-mesboot0", "patch-glibc-boot-2.16.0", &good)
            .unwrap();
        // Red 1: an unpinned key — arbitrary bytes parked under a
        // canonical-LOOKING name red before any IO.
        let fake = format!("{}-bash-5.2", "0".repeat(32));
        std::fs::write(seeds.join(&fake), b"#!/bin/sh\nhost bash\n").unwrap();
        let err = auto_seed_provenance(
            "/td/store",
            &seeds,
            "mes",
            "bash",
            &format!("/td/store/{fake}"),
        )
        .unwrap_err();
        assert!(err.contains("provenance rejected"), "{err}");
        assert!(err.contains("no compiled expected digest"), "{err}");
        // Red 2: a pinned key resolving to an item that content-addresses
        // honestly but was never derived from the pins — the forged
        // map/store/db triple `build-plan --auto` used to accept.
        let forged = intern_test_seed(&seeds, "patch-glibc-boot-2.2.5", b"not the audited patch");
        let err = auto_seed_provenance(
            "/td/store",
            &seeds,
            "glibc-mesboot0",
            "patch-glibc-boot-2.2.5",
            &forged,
        )
        .unwrap_err();
        assert!(err.contains("provenance rejected"), "{err}");
        assert!(err.contains("the compiled table pins"), "{err}");
        // Red 3: the real item whose bytes were swapped after interning — the
        // pinned basename no longer reproduces from its own bytes.
        let base = good.rsplit('/').next().unwrap();
        std::fs::write(seeds.join(base), b"tampered bytes").unwrap();
        let err = auto_seed_provenance(
            "/td/store",
            &seeds,
            "glibc-mesboot0",
            "patch-glibc-boot-2.16.0",
            &good,
        )
        .unwrap_err();
        assert!(err.contains("provenance rejected"), "{err}");
        assert!(err.contains("content-address"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // --auto: the MAP file is NOT a host-path ingress channel (re #469). A map value
    // outside the canonical store prefix (`/usr/bin/env`, `/gnu/store/…`) reds at
    // SYNTHESIS with `provenance rejected` — verified red against the pre-gate
    // behavior, which admitted exactly these strings as `seed` lock entries.
    #[test]
    fn auto_synthesize_lock_rejects_host_paths_at_planning() {
        let d = std::env::temp_dir().join(format!("td-auto-hostpath-{}", std::process::id()));
        let seeds = d.join("seed-store");
        std::fs::create_dir_all(&seeds).unwrap();
        std::fs::write(d.join("mes.json"), r#"{"name":"mes","inputs":["bash"]}"#).unwrap();
        for bad in ["/usr/bin/env", "/gnu/store/aaa-bash", "bash", "/td/store/a/b"] {
            let mut map = std::collections::BTreeMap::new();
            map.insert("bash".to_string(), bad.to_string());
            let err = auto_synthesize_lock(&d.to_string_lossy(), &map, "mes", "/td/store", &seeds)
                .unwrap_err();
            assert!(err.contains("provenance rejected"), "`{bad}': {err}");
            assert!(err.contains("not a canonical /td/store item"), "`{bad}': {err}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    // build_plan gates EVERY lock entry by class before anything is staged or built
    // (re #469): a hand-authored lock naming a host path as `seed` reds with
    // `provenance rejected`, and a vendored-crate entry is inadmissible in a
    // bootstrap plan outright. Verified red against the pre-gate build_plan, which
    // copied any non-td-recipe-output path through unchanged.
    #[test]
    fn build_plan_rejects_host_path_and_crate_lock_entries() {
        let d = std::env::temp_dir().join(format!("td-plan-gate-{}", std::process::id()));
        let seeds = d.join("seed-store");
        std::fs::create_dir_all(&seeds).unwrap();
        std::fs::write(d.join("mes.json"), r#"{"name":"mes"}"#).unwrap();
        for (lock_body, want) in [
            ("bash /usr/bin/env seed\n", "provenance rejected"),
            ("bash /gnu/store/aaa-bash seed\n", "provenance rejected"),
            ("mes-source /gnu/store/bbb-mes.tar.gz source\n", "provenance rejected"),
            ("itoa-1.0.11.crate /td/store/ccc-itoa.crate crate\n", "vendored crate"),
            // The unavailable-prior-output red (re #469): a td-recipe-output
            // entry whose producing step never ran is a loud planning error,
            // never a silent fall-through to some other resolution.
            (
                "tcc /td/store/pending-tcc td-recipe-output\n",
                "no earlier step built it",
            ),
        ] {
            let lock = d.join("gate.lock");
            std::fs::write(&lock, lock_body).unwrap();
            let plan = d.join("gate.plan");
            std::fs::write(
                &plan,
                format!("step {} {}\n", d.join("mes.json").display(), lock.display()),
            )
            .unwrap();
            let err = build_plan(
                &plan.to_string_lossy(),
                &seeds.to_string_lossy(),
                &d.join("seed.db").to_string_lossy(),
                &d.join("scratch"),
                None,
                None,
            )
            .unwrap_err();
            assert!(err.contains(want), "lock `{lock_body}': {err}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    // The provenance manifest's oracle round-trips: a registration written by
    // write_output_db is read back by hashes_by_path with the exact recorded NAR
    // hash — the same record realize_drv assembles the staging manifest from.
    #[test]
    fn output_db_hashes_round_trip_for_the_staging_manifest() {
        let d = std::env::temp_dir().join(format!("td-manifest-db-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let reg = OutputReg {
            store_path: "/td/store/aaa-mes-0.27.1".to_string(),
            nar_hash: "sha256:deadbeef".to_string(),
            nar_size: 7,
            refs: vec![],
            deriver: "/td/store/bbb-mes-0.27.1.drv".to_string(),
        };
        let db_path = d.join("td.db");
        write_output_db(std::slice::from_ref(&reg), &db_path).unwrap();
        let db = store_db_read::Db::open(std::fs::read(&db_path).unwrap()).unwrap();
        let hashes = db.hashes_by_path().unwrap();
        assert_eq!(
            hashes.get("/td/store/aaa-mes-0.27.1").map(String::as_str),
            Some("sha256:deadbeef")
        );
        std::fs::remove_dir_all(&d).ok();
    }

    // store-add-recursive MERGES into OUT-DB: the runner interns EVERY seed into
    // one db and passes it as build-plan's strict-provenance SEED-DB, so a later
    // intern must never un-vouch an earlier one — the pre-fix clobber left only
    // the LAST seed registered, which would red the manifest completeness gate
    // on every multi-seed rung (subagent review, round 4; re #469).
    #[test]
    fn store_add_recursive_accumulates_every_interned_seed_in_one_db() {
        let d = std::env::temp_dir().join(format!("td-seed-db-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (name, bytes) in [("one", "alpha\n"), ("two", "beta\n")] {
            let s = d.join(name);
            std::fs::create_dir_all(&s).unwrap();
            std::fs::write(s.join("file"), bytes).unwrap();
        }
        let (store, db) = (d.join("store"), d.join("seed.db"));
        let (store_s, db_s) = (
            store.to_string_lossy().to_string(),
            db.to_string_lossy().to_string(),
        );
        let one = d.join("one").to_string_lossy().to_string();
        let two = d.join("two").to_string_lossy().to_string();
        let p1 = store_add_recursive("seed-one", &one, &store_s, &db_s).unwrap();
        let p2 = store_add_recursive("seed-two", &two, &store_s, &db_s).unwrap();
        // Re-interning is idempotent — same path, no duplicate row.
        assert_eq!(p1, store_add_recursive("seed-one", &one, &store_s, &db_s).unwrap());
        let hashes = store_db_read::Db::open(std::fs::read(&db).unwrap())
            .unwrap()
            .hashes_by_path()
            .unwrap();
        assert_eq!(hashes.len(), 2, "both seeds stay vouched: {hashes:?}");
        for p in [&p1, &p2] {
            assert!(
                hashes.get(p.as_str()).is_some_and(|h| h.starts_with("sha256:")),
                "{p} missing from the merged seed db: {hashes:?}"
            );
        }
        std::fs::remove_dir_all(&d).ok();
    }

    // A single db registering one path under two DIFFERENT hashes is corrupt: the
    // provenance oracle errors instead of silently letting the later row win.
    // Duplicate rows with the SAME hash are merely redundant, not corrupt.
    #[test]
    fn hashes_by_path_rejects_intra_db_hash_conflicts() {
        let d = std::env::temp_dir().join(format!("td-dup-hash-db-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mk = |h: &str| OutputReg {
            store_path: "/td/store/aaa-mes-0.27.1".to_string(),
            nar_hash: h.to_string(),
            nar_size: 1,
            refs: vec![],
            deriver: String::new(),
        };
        let db_path = d.join("dup.db");
        write_output_db(&[mk("sha256:aa"), mk("sha256:bb")], &db_path).unwrap();
        let err = store_db_read::Db::open(std::fs::read(&db_path).unwrap())
            .unwrap()
            .hashes_by_path()
            .unwrap_err();
        assert!(err.contains("conflicting hashes"), "{err}");
        write_output_db(&[mk("sha256:aa"), mk("sha256:aa")], &db_path).unwrap();
        let hashes = store_db_read::Db::open(std::fs::read(&db_path).unwrap())
            .unwrap()
            .hashes_by_path()
            .unwrap();
        assert_eq!(hashes.len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    // --auto: even the exact PINNED basename reds at synthesis when the item was
    // never interned in the seed store — the map cannot name seeds the classified
    // planning pass didn't produce (uses a real table key so the compiled-table
    // gate passes and the not-interned arm is what fires).
    #[test]
    fn auto_synthesize_lock_rejects_uninterned_seeds_at_planning() {
        let d = std::env::temp_dir().join(format!("td-auto-unintern-{}", std::process::id()));
        let seeds = d.join("seed-store");
        std::fs::create_dir_all(&seeds).unwrap();
        std::fs::write(
            d.join("mes.json"),
            r#"{"name":"mes","inputs":["patch-glibc-boot-2.16.0"]}"#,
        )
        .unwrap();
        let pinned = seed_digests_expected("patch-glibc-boot-2.16.0").unwrap().unwrap();
        let mut map = std::collections::BTreeMap::new();
        map.insert("patch-glibc-boot-2.16.0".to_string(), format!("/td/store/{pinned}"));
        let err = auto_synthesize_lock(&d.to_string_lossy(), &map, "mes", "/td/store", &seeds)
            .unwrap_err();
        assert!(err.contains("provenance rejected"), "{err}");
        assert!(err.contains("not interned in the seed store"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // --auto: a recipe with no declared `sourceInput` (make-test — it only RUNS a
    // sibling rung's output) gets NO `<name>-source` line at all — no nominal-source alias.
    #[test]
    fn auto_synthesize_lock_omits_the_source_line_when_none_declared() {
        let d = std::env::temp_dir().join(format!("td-auto-synth-nosrc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("make-test.json"), r#"{"name":"make-test","nativeInputs":["make-x86-64"]}"#).unwrap();
        std::fs::write(d.join("make-x86-64.json"), r#"{"name":"make-x86-64"}"#).unwrap();
        let map = std::collections::BTreeMap::new();
        let got =
            auto_synthesize_lock(&d.to_string_lossy(), &map, "make-test", "/td/store", &d).unwrap();
        assert!(!got.contains("-source"), "unexpected source line: {got}");
        assert!(got.contains("make-x86-64 /td/store/pending-make-x86-64 td-recipe-output"));
        std::fs::remove_dir_all(&d).ok();
    }

    // --auto: a declared input that is neither an owned recipe nor in MAP is a loud
    // error, never a silently dropped edge.
    #[test]
    fn auto_synthesize_lock_errors_on_an_unresolvable_input() {
        let d = std::env::temp_dir().join(format!("td-auto-synth-err-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("tcc.json"), r#"{"name":"tcc","inputs":["mystery-tool"]}"#).unwrap();
        let map = std::collections::BTreeMap::new();
        let err =
            auto_synthesize_lock(&d.to_string_lossy(), &map, "tcc", "/td/store", &d).unwrap_err();
        assert!(err.contains("mystery-tool"), "unexpected error: {err}");
        std::fs::remove_dir_all(&d).ok();
    }

    // Verified-red lever (issue #429's Done criterion): perturbing a recipe's declared
    // inputs changes the synthesized lock (hence the plan build_plan_auto derives from
    // it) — the synthesis genuinely reads the recipe graph, not a stale hand-written one.
    #[test]
    fn auto_synthesize_lock_changes_when_declared_inputs_change() {
        let d = std::env::temp_dir().join(format!("td-auto-synth-perturb-{}", std::process::id()));
        let seeds = d.join("seed-store");
        std::fs::create_dir_all(&seeds).unwrap();
        let mut map = std::collections::BTreeMap::new();
        let in_a = intern_real_patch_seed(&seeds, "patch-glibc-boot-2.16.0");
        let in_b = intern_real_patch_seed(&seeds, "patch-glibc-boot-2.2.5");
        map.insert("patch-glibc-boot-2.16.0".to_string(), in_a);
        map.insert("patch-glibc-boot-2.2.5".to_string(), in_b.clone());
        std::fs::write(
            d.join("gcc-mesboot0.json"),
            r#"{"name":"gcc-mesboot0","inputs":["patch-glibc-boot-2.16.0"]}"#,
        )
        .unwrap();
        let before =
            auto_synthesize_lock(&d.to_string_lossy(), &map, "gcc-mesboot0", "/td/store", &seeds)
                .unwrap();
        assert!(before.contains("patch-glibc-boot-2.16.0"));
        assert!(!before.contains("patch-glibc-boot-2.2.5"));
        std::fs::write(
            d.join("gcc-mesboot0.json"),
            r#"{"name":"gcc-mesboot0","inputs":["patch-glibc-boot-2.16.0","patch-glibc-boot-2.2.5"]}"#,
        )
        .unwrap();
        let after =
            auto_synthesize_lock(&d.to_string_lossy(), &map, "gcc-mesboot0", "/td/store", &seeds)
                .unwrap();
        assert!(after.contains(&format!("patch-glibc-boot-2.2.5 {in_b} seed")));
        assert_ne!(before, after, "synthesized lock did not change when declared inputs changed");
        std::fs::remove_dir_all(&d).ok();
    }

    // subst-export writes, for each member, a narinfo with the right StorePath/References
    // and a nar that RESTORES (read_nar) to the original tree with the recorded NarHash —
    // the durable round-trip of the substitute server's store-coupled half, no DB/network.
    #[test]
    fn subst_export_writes_narinfos_and_restorable_nars() {
        let base = std::env::temp_dir().join(format!("td-subst-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Two synthetic store paths; "app" references "lib".
        let lib = "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib";
        let app = "/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app";
        let phys_lib = base.join("phys/lib");
        std::fs::create_dir_all(&phys_lib).unwrap();
        std::fs::write(phys_lib.join("libfoo"), b"lib bytes\n").unwrap();
        let phys_app = base.join("phys/app");
        std::fs::create_dir_all(&phys_app).unwrap();
        std::fs::write(phys_app.join("run"), b"app\n").unwrap();

        let members = vec![
            SubstMember { store_path: lib.into(), physical: phys_lib.clone(), refs: vec![] },
            SubstMember { store_path: app.into(), physical: phys_app.clone(), refs: vec![lib.into()] },
        ];
        let outdir = base.join("out");
        let written = subst_export(&outdir, &members).unwrap();
        assert_eq!(written.len(), 2);

        // The app narinfo carries the right StorePath and records the ref as a BASENAME.
        let ni = std::fs::read_to_string(outdir.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app.narinfo")).unwrap();
        assert!(ni.contains(&format!("StorePath: {app}\n")), "narinfo: {ni}");
        assert!(ni.contains("References: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib\n"), "narinfo: {ni}");
        let narhash = ni.lines().find_map(|l| l.strip_prefix("NarHash: ")).unwrap();
        let narfile = ni.lines().find_map(|l| l.strip_prefix("NarFile: ")).unwrap();
        // The recorded NarHash is the TRUE nar hash of the source path.
        assert_eq!(narhash, nar_hash_size_path(&phys_app).unwrap().0);
        // The served nar RESTORES to the original tree (durable round-trip).
        let restored = base.join("restored-app");
        let mut r = std::io::BufReader::new(std::fs::File::open(outdir.join(narfile)).unwrap());
        nar::read_nar(&mut r, &restored).unwrap();
        assert_eq!(std::fs::read(restored.join("run")).unwrap(), b"app\n");

        std::fs::remove_dir_all(&base).unwrap();
    }

    // harness_subst_export (#314): the WHOLE harness tree — a store/ with multiple entries
    // AND loose files (the /td/store/ld loader), plus the rel + toolchain metadata — ships as
    // ONE fixed-name nar and restores byte-for-byte. This is the tree-set variant the toolchain
    // per-path export can't express (ld is not a `<hash>-name` store path).
    #[test]
    fn harness_subst_export_ships_the_whole_tree_under_a_fixed_name() {
        let base = std::env::temp_dir().join(format!("td-harness-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let hdir = base.join("harness");
        // A harness-shaped fixture: store/<rel>/bin/busybox (exec), a loose store/ld loader,
        // plus the rel + toolchain manifest.
        let rel = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-td-harness-fixture";
        let bind = hdir.join("store").join(rel).join("bin");
        std::fs::create_dir_all(&bind).unwrap();
        std::fs::write(bind.join("busybox"), b"#!/bin/sh\necho hi\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bind.join("busybox"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(hdir.join("store").join("ld"), b"loader bytes\n").unwrap();
        std::fs::write(hdir.join("rel"), format!("{rel}\n")).unwrap();
        std::fs::write(hdir.join("toolchain"), b"HT_TARGET=x86_64-pc-linux-gnu\nHT_GCC=g\n").unwrap();

        let outdir = base.join("out");
        let written = harness_subst_export(&outdir, &hdir).unwrap();
        assert_eq!(written, vec!["td-harness".to_string()]);
        let ni = std::fs::read_to_string(outdir.join("td-harness.narinfo")).unwrap();
        assert!(ni.contains("StorePath: /td/store/td-harness\n"), "narinfo: {ni}");
        assert!(ni.contains("References: \n"), "harness has no refs: {ni}");
        let narfile = ni.lines().find_map(|l| l.strip_prefix("NarFile: ")).unwrap();

        // The served nar RESTORES the WHOLE tree — the store subdir (its entry + the loose ld)
        // and both metadata files — byte-for-byte, exec bit preserved on the binary.
        let restored = base.join("restored");
        let mut r = std::io::BufReader::new(std::fs::File::open(outdir.join(narfile)).unwrap());
        nar::read_nar(&mut r, &restored).unwrap();
        assert_eq!(std::fs::read(restored.join("store").join(rel).join("bin/busybox")).unwrap(),
                   b"#!/bin/sh\necho hi\n");
        assert_eq!(std::fs::read(restored.join("store").join("ld")).unwrap(), b"loader bytes\n");
        assert_eq!(std::fs::read_to_string(restored.join("rel")).unwrap(), format!("{rel}\n"));
        assert!(std::fs::read_to_string(restored.join("toolchain")).unwrap().contains("HT_GCC=g"));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(restored.join("store").join(rel).join("bin/busybox"))
                .unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "exec bit not preserved through the harness nar");
        }

        // A non-harness dir (no store/ or rel) is rejected — the producer never ships junk.
        assert!(harness_subst_export(&outdir, &base).is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    // subst-export `--paths` exports EXACTLY the named roots (no closure walk) — the
    // per-output granularity the substitute consumer fetches — while the default closure
    // mode pulls in the external refs. A build output's refs (glibc, …) are recorded in the
    // build db but NOT staged in its newstore, so a per-output publish must skip them.
    #[test]
    fn subst_export_members_paths_only_exports_roots_not_their_closure() {
        let dir = std::env::temp_dir().join(format!("td-subst-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = "/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app".to_string();
        let lib = "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib".to_string();
        // app references lib (an external dep: in the build db with a null hash, not staged).
        let regs = vec![OutputReg {
            store_path: app.clone(),
            nar_hash: "sha256:00".into(),
            nar_size: 1,
            refs: vec![lib.clone()],
            deriver: String::new(),
        }];
        let db_path = dir.join("td.db");
        write_output_db(&regs, &db_path).unwrap();
        let db = store_db_read::Db::open(std::fs::read(&db_path).unwrap()).unwrap();
        let roots = vec![app.clone()];

        // Default (closure) mode pulls in the external ref — a whole-closure mirror.
        let full = subst_export_members(&db, "/store", &roots, true).unwrap();
        let fp: std::collections::BTreeSet<&str> =
            full.iter().map(|m| m.store_path.as_str()).collect();
        assert!(
            fp.contains(app.as_str()) && fp.contains(lib.as_str()),
            "closure mode must include the external ref: {fp:?}"
        );

        // Paths-only exports EXACTLY the root, but still lists its refs in the narinfo so the
        // consumer can scan-verify the restored bytes (deps assumed already present).
        let only = subst_export_members(&db, "/store", &roots, false).unwrap();
        assert_eq!(only.len(), 1, "paths-only must not pull in the closure");
        assert_eq!(only[0].store_path, app);
        assert_eq!(only[0].refs, vec![lib.clone()]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // restore_substitute: a fetched narinfo + nar (as subst-export produces) restores to
    // the original tree, the OutputReg's NarHash equals the signed one, and a corrupted nar
    // is REJECTED (the durable equality leg — a substitute is only accepted if it restores
    // to the bytes the publisher signed). The consumer's core, no network/DB.
    #[test]
    fn restore_substitute_round_trips_and_rejects_corruption() {
        let base = std::env::temp_dir().join(format!("td-subst-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store_path = "/td/store/cccccccccccccccccccccccccccccccc-app";
        let app_base = "cccccccccccccccccccccccccccccccc-app";
        let phys = base.join("phys/app");
        std::fs::create_dir_all(&phys).unwrap();
        std::fs::write(phys.join("run"), b"app payload\n").unwrap();

        // Export it (the server side) → a narinfo + nar, exactly what `fetch` would write.
        let served = base.join("served");
        subst_export(&served, &[SubstMember { store_path: store_path.into(), physical: phys.clone(), refs: vec![] }]).unwrap();
        let ni = std::fs::read_to_string(served.join(format!("{app_base}.narinfo"))).unwrap();
        let narfile = served.join(narinfo_field(&ni, "NarFile").unwrap());

        // Restore it (the consumer side) into a fresh newstore.
        let newstore = base.join("newstore");
        let reg = restore_substitute(&ni, &narfile, store_path, &newstore, "x.drv").unwrap();
        assert_eq!(reg.store_path, store_path);
        assert_eq!(reg.nar_hash, narinfo_field(&ni, "NarHash").unwrap());
        assert_eq!(std::fs::read(newstore.join(app_base).join("run")).unwrap(), b"app payload\n");

        // Self-discrimination (wrong output): the narinfo is a perfectly valid,
        // hash-consistent export of `store_path`, but we ask restore to treat it as a
        // DIFFERENT output. A signed narinfo for one path must not be accepted as
        // another (the StorePath-binding check) even though every byte verifies.
        let other_path = "/td/store/dddddddddddddddddddddddddddddddd-other";
        assert!(
            restore_substitute(&ni, &narfile, other_path, &newstore, "x.drv").is_err(),
            "restore accepted a narinfo whose signed StorePath != the requested output"
        );

        // Self-discrimination: corrupt the nar's file CONTENTS (structure intact, so
        // read_nar still parses) → restore must reject on the NarHash check specifically.
        let mut bytes = std::fs::read(&narfile).unwrap();
        let pos = bytes.windows(3).position(|w| w == b"app").expect("payload in nar");
        bytes[pos] ^= 0xff;
        std::fs::write(&narfile, &bytes).unwrap();
        assert!(
            restore_substitute(&ni, &narfile, store_path, &newstore, "x.drv").is_err(),
            "restore accepted a nar whose contents do not match the signed NarHash"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    // A substitute whose NAR is structurally TRUNCATED (read_nar creates the dest dir + starts
    // the file, then hits EOF mid-contents) must be rejected AND leave NO partial tree under
    // newstore. This is the cleanup-on-failure leg: a half-restored output left behind would let
    // the build fallback write its fresh outputs on top of it (contaminating a multi-output drv).
    // The old code only cleaned on a NarHash mismatch, not on a parse/write error.
    #[test]
    fn restore_substitute_cleans_partial_tree_on_parse_error() {
        let base = std::env::temp_dir().join(format!("td-subst-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store_path = "/td/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-app";
        let app_base = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-app";
        let phys = base.join("phys/app");
        std::fs::create_dir_all(&phys).unwrap();
        std::fs::write(phys.join("run"), b"app payload\n").unwrap();

        let served = base.join("served");
        subst_export(&served, &[SubstMember { store_path: store_path.into(), physical: phys.clone(), refs: vec![] }]).unwrap();
        let ni = std::fs::read_to_string(served.join(format!("{app_base}.narinfo"))).unwrap();
        let narfile = served.join(narinfo_field(&ni, "NarFile").unwrap());

        // Truncate inside the file contents: read_nar creates dest + the `run` file, then EOFs
        // part way through copy_n — a partial tree exists at the moment the error is returned.
        let bytes = std::fs::read(&narfile).unwrap();
        let pos = bytes.windows(3).position(|w| w == b"app").expect("payload in nar");
        let truncated = base.join("truncated.nar");
        std::fs::write(&truncated, &bytes[..pos + 4]).unwrap();

        let newstore = base.join("newstore");
        assert!(
            restore_substitute(&ni, &truncated, store_path, &newstore, "x.drv").is_err(),
            "a truncated NAR must be rejected"
        );
        assert!(
            !newstore.join(app_base).exists(),
            "a rejected (parse-error) substitute must leave no partial tree under newstore"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    // The build cache hits only on a present + NAR-verified output whose engine
    // RECEIPT matches the CURRENT plan identity (re #469 round-7), and misses on a
    // corrupted, deleted, never-recorded, receipt-less, or identity-mismatched one —
    // so a CHANGED recipe (different drv ⇒ different identity AND output path)
    // always rebuilds, a forged registration without the current-plan receipt is
    // never served, and a corrupted cache entry rebuilds rather than serving garbage.
    #[test]
    fn cached_realization_hits_only_on_a_present_and_nar_verified_output() {
        let base = std::env::temp_dir().join(format!("td-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scratch = base.join("b");
        let store_path = "/gnu/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-thing-1.0";
        let outdir = scratch.join("newstore").join("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-thing-1.0");
        std::fs::create_dir_all(&outdir).unwrap();
        std::fs::write(outdir.join("data"), b"hello cache").unwrap();
        // Real NAR hash of the output (same scan/nar the registration is written with).
        let mut sc = scan::Scanner::new(&[]).unwrap();
        nar::write_nar(&mut sc, &outdir).unwrap();
        let (hash, size, _) = sc.finish();
        let drv = drv::Derivation {
            outputs: vec![drv::Output {
                name: "out".into(),
                path: store_path.into(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            platform: String::new(),
            builder: String::new(),
            args: vec![],
            env: vec![],
        };
        let expect = ReceiptExpect {
            drv_sha256: "aa".repeat(32),
            manifest_sha256: "bb".repeat(32),
            builder: String::new(), // matches the test drv's builder field
        };
        let write_reg = |h: &str| {
            std::fs::write(
                scratch.join("registration"),
                format!("path {store_path}\nnar-hash {h}\nnar-size {size}\nderiver x.drv\n\n"),
            )
            .unwrap();
        };
        let write_receipt = |e: &ReceiptExpect, h: &str| {
            let reg = OutputReg {
                store_path: store_path.to_string(),
                nar_hash: h.to_string(),
                nar_size: size,
                refs: vec![],
                deriver: "x.drv".to_string(),
            };
            std::fs::write(scratch.join("receipt"), receipt_text(e, std::slice::from_ref(&reg)))
                .unwrap();
        };

        // (a0) valid registration + bytes but NO engine receipt -> MISS: a record
        // beside the bytes is not its own authority.
        write_reg(&hash);
        assert!(
            cached_realization(&drv, &scratch, &expect).unwrap().is_none(),
            "a registration without the engine receipt must miss"
        );

        // (a) present + matching hash + current-plan receipt -> HIT.
        write_receipt(&expect, &hash);
        assert!(
            cached_realization(&drv, &scratch, &expect).unwrap().is_some(),
            "valid entry must hit"
        );

        // (a1) the CURRENT plan moved (input-manifest digest changed) -> MISS: the
        // stored receipt cannot vouch a plan it was not issued for.
        let moved = ReceiptExpect { manifest_sha256: "cc".repeat(32), ..expect.clone() };
        assert!(
            cached_realization(&drv, &scratch, &moved).unwrap().is_none(),
            "an identity-mismatched receipt must miss"
        );

        // (a2) registration hash diverges from the receipt's (forged record beside
        // honest receipt) -> MISS.
        write_reg("sha256:0123");
        write_receipt(&expect, &hash);
        assert!(
            cached_realization(&drv, &scratch, &expect).unwrap().is_none(),
            "a registration disagreeing with the receipt must miss"
        );

        // (b) recorded hash wrong everywhere (output content changed under us) -> MISS
        // at NAR re-verification.
        write_reg("sha256:deadbeef");
        write_receipt(&expect, "sha256:deadbeef");
        assert!(
            cached_realization(&drv, &scratch, &expect).unwrap().is_none(),
            "hash mismatch must miss"
        );

        // (c) output directory gone -> MISS.
        write_reg(&hash);
        write_receipt(&expect, &hash);
        std::fs::remove_dir_all(&outdir).unwrap();
        assert!(
            cached_realization(&drv, &scratch, &expect).unwrap().is_none(),
            "absent output must miss"
        );

        // (d) never built here (no registration) -> MISS.
        std::fs::remove_file(scratch.join("registration")).unwrap();
        assert!(
            cached_realization(&drv, &scratch, &expect).unwrap().is_none(),
            "no registration must miss"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // `copy_canonical` must reproduce a tree byte-identically by NAR — exercising
    // the properties NAR captures that the `store-add-tree` rung's source tree does
    // not have: an EXECUTABLE file and a SYMLINK (plus a subdir + a plain file).
    #[test]
    fn copy_canonical_is_nar_identical_with_exec_and_symlink() {
        let base = std::env::temp_dir().join(format!("td-cc-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(src.join("sub/run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        // A GROUP-exec-only file (0o654): NAR keys off OWNER-exec, so this must be
        // restored NON-executable — a regression guard for the `& 0o100` (not `0o111`)
        // exec test, matching nar.rs / the daemon.
        std::fs::write(src.join("group-exec"), b"data").unwrap();
        std::fs::set_permissions(src.join("group-exec"), std::fs::Permissions::from_mode(0o654))
            .unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();

        copy_canonical(&src, &dst).unwrap();

        // Structure + contents + exec bit + symlink target all preserved ⇒ same NAR.
        assert_eq!(
            nar_hash_path(&src).unwrap(),
            nar_hash_path(&dst).unwrap(),
            "canonical copy is NAR-identical to the source"
        );
        // The executable bit (the one perm NAR distinguishes) is preserved.
        let mode = std::fs::metadata(dst.join("sub/run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "exec bit preserved on dst");
        // The symlink is recreated as a symlink, not followed.
        assert!(std::fs::symlink_metadata(dst.join("link")).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&base);
    }

    // commit_tree_checked guards the ABI-token invariant: output paths are keyed on the ABI
    // token, not content, so an already-present REGISTERED dest is re-hashed and a MISMATCH
    // (an output changed without a BUILDER_ABI bump) fails closed with an ABI-bump demand
    // instead of silently keeping stale bytes under a fresh store record.
    #[test]
    fn commit_tree_checked_rejects_mismatched_existing_dest() {
        let base = std::env::temp_dir().join(format!("td-ctc-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("out"), b"built-bytes").unwrap();
        let want = nar_hash_path(&src).unwrap();

        // Absent dest -> a plain (atomic) copy, NAR-identical to the source, no staging temp.
        commit_tree_checked(&src, &dst, &want, false).unwrap();
        assert_eq!(nar_hash_path(&dst).unwrap(), want, "absent dest copied");
        assert!(!has_commit_temp(&base), "no staging temp left after a clean commit");

        // Present dest with the SAME bytes -> idempotent skip, no error, no change.
        commit_tree_checked(&src, &dst, &want, false).unwrap();
        assert_eq!(nar_hash_path(&dst).unwrap(), want, "matching dest unchanged");

        // Present REGISTERED dest with DIFFERENT bytes at the SAME (ABI-token) path -> fail
        // closed with an ABI-bump demand, and the stale tree is left intact.
        let stale = base.join("stale");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("out"), b"stale-different-bytes").unwrap();
        let err = commit_tree_checked(&src, &stale, &want, true).unwrap_err();
        assert!(err.contains("BUILDER_ABI"), "mismatch must demand an ABI bump: {err}");
        assert_ne!(nar_hash_path(&stale).unwrap(), want, "stale dest left intact");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Any `.commit-tmp.*` staging entry left under DIR (a leaked/uncleaned commit temp).
    fn has_commit_temp(dir: &Path) -> bool {
        std::fs::read_dir(dir).map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(".commit-tmp."))
            })
        }).unwrap_or(false)
    }

    // The poisoning fix: a torn tree an interrupted commit left at the final path is
    // UNREGISTERED (its db merge never ran), so commit_tree_checked recovers it — removes
    // the orphan and re-commits — instead of failing closed on it forever and wedging the
    // shared cache. Contrast the registered case above, which MUST fail closed.
    #[test]
    fn commit_tree_checked_recovers_a_torn_unregistered_orphan() {
        let base = std::env::temp_dir().join(format!("td-ctc-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("out"), b"the-real-bytes").unwrap();
        let want = nar_hash_path(&src).unwrap();

        // A torn/partial tree sitting at the final path, NOT registered in any db.
        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("out"), b"partial-torn").unwrap();
        assert_ne!(nar_hash_path(&dest).unwrap(), want, "precondition: dest is torn");

        // Unregistered mismatch -> recovered: the orphan is replaced with the real tree.
        commit_tree_checked(&src, &dest, &want, false).unwrap();
        assert_eq!(nar_hash_path(&dest).unwrap(), want, "torn orphan recovered to the real tree");
        assert!(!has_commit_temp(&base), "recovery leaves no staging temp");
        let _ = std::fs::remove_dir_all(&base);
    }

    // sweep_commit_temps reaps ONLY crash-orphaned temps whose owning pid is dead; a live
    // pid's staging tree (a concurrent committer, or our own in-flight one) is never removed.
    #[test]
    fn sweep_commit_temps_reaps_dead_pid_temps_only() {
        let dir = std::env::temp_dir().join(format!("td-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A dead pid (u32::MAX is never a live process) and this live test process.
        let dead = dir.join(format!(".commit-tmp.{}.out-1.0", u32::MAX));
        let live = dir.join(format!(".commit-tmp.{}.out-1.0", std::process::id()));
        let keep = dir.join("real-store-item"); // not a temp — never touched
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&keep).unwrap();

        sweep_commit_temps(&dir);
        assert!(!dead.exists(), "a dead pid's orphan temp is reaped");
        assert!(live.exists(), "a live pid's staging temp is left alone");
        assert!(keep.exists(), "a real store item is never mistaken for a temp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // write_atomic replaces PATH via a sibling temp + rename, so a reader sees the old or the
    // new bytes but never a truncated file — the torn-db/torn-receipt failure mode.
    #[test]
    fn write_atomic_replaces_without_leaving_a_temp() {
        let dir = std::env::temp_dir().join(format!("td-watomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("db");
        write_atomic(&f, b"first").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"first");
        write_atomic(&f, b"second-longer-and-different").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"second-longer-and-different");
        assert!(!has_commit_temp(&dir), "no staging temp left after atomic writes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // read_registered_paths: a missing db is the first commit (empty set); a written db yields
    // its registered paths; a torn/corrupt db is surfaced with a recovery hint, never read as
    // empty (which would misjudge every registered path a torn orphan and clobber the cache).
    #[test]
    fn read_registered_paths_distinguishes_missing_from_torn() {
        let dir = std::env::temp_dir().join(format!("td-regpaths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("db");
        assert!(read_registered_paths(&db).unwrap().is_empty(), "missing db -> first commit");

        let out_path = format!("/td/store/{}-out-1.0", "a".repeat(32));
        write_output_db(
            std::slice::from_ref(&OutputReg {
                store_path: out_path.clone(),
                nar_hash: "sha256:deadbeef".to_string(),
                nar_size: 7,
                refs: vec![],
                deriver: String::new(),
            }),
            &db,
        )
        .unwrap();
        let reg = read_registered_paths(&db).unwrap();
        assert!(reg.contains(&out_path), "a written db vouches its path: {reg:?}");

        std::fs::write(&db, b"not-a-valid-store-db").unwrap();
        let err = read_registered_paths(&db).unwrap_err();
        assert!(err.contains("unreadable"), "a torn db is surfaced, not read as empty: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A dangling db symlink reads NotFound (fs::read follows it), but the path is NOT genuinely
    // absent — reading it as an empty first-commit set would misjudge every registered path a
    // torn orphan and clobber the cache. It must fail closed.
    #[test]
    fn read_registered_paths_fails_closed_on_a_dangling_db_symlink() {
        let dir = std::env::temp_dir().join(format!("td-regpaths-dangle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("db");
        std::os::unix::fs::symlink(dir.join("no-such-target"), &db).unwrap();
        let err = read_registered_paths(&db).unwrap_err();
        assert!(err.contains("unreadable"), "a dangling db symlink fails closed: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The per-store commit lock is exclusive: while one committer holds it, a second acquirer on
    // the SAME store's lock file cannot take it (flock contends across independent open
    // descriptions even within one process), and it becomes acquirable again once released. This
    // is the single-writer guarantee that keeps the delete-capable recovery safe against a
    // committer the client-side ladder lock does not cover.
    #[test]
    fn lock_store_commit_is_exclusive_per_store() {
        let dir = std::env::temp_dir().join(format!("td-commitlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("build-cache");
        std::fs::create_dir_all(&store).unwrap();
        let db = store.join("db");
        let held = lock_store_commit(&db).unwrap();
        // The lock is a SIBLING of the store dir, not inside it — so eviction (which renames the
        // store dir aside) never changes the lock inode.
        let lock_path = dir.join("build-cache.commit.lock");
        assert!(lock_path.exists(), "lock sits beside the store dir, not inside it");
        assert!(!store.join("db.commit.lock").exists(), "lock is not inside the evictable store dir");
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(contender.try_lock().is_err(), "commit lock held exclusively while in use");
        drop(held);
        assert!(contender.try_lock().is_ok(), "commit lock acquirable again once released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gcc_toolchain_substitution_swaps_only_the_toolchain_input() {
        // corpus-toolchain-default: TD_GCC_TOOLCHAIN swaps the guix gcc-toolchain input for a
        // /td/store toolchain, leaving every other build input (glibc, make, coreutils, the source)
        // untouched, order-preserved.
        let tc = "/td/store/abc123-gcc-toolchain-tdstore";
        let mut inputs = vec![
            "/gnu/store/aaa-glibc-2.41".to_string(),
            "/gnu/store/bbb-gcc-toolchain-15.2.0".to_string(),
            "/gnu/store/ccc-make-4.4.1".to_string(),
        ];
        assert!(super::substitute_gcc_toolchain(&mut inputs, tc), "should report a swap");
        assert_eq!(
            inputs,
            vec![
                "/gnu/store/aaa-glibc-2.41".to_string(),
                tc.to_string(),
                "/gnu/store/ccc-make-4.4.1".to_string(),
            ],
            "only the gcc-toolchain input is swapped; others + order preserved"
        );
        // Near-miss basenames must NOT be swapped: a bare gcc (the package name is `gcc-…`, not
        // `gcc-toolchain-…`), and an unrelated package that merely embeds `-gcc-toolchain-` INTERIOR
        // (the name is `libfoo-…`, so the anchored match at the package name excludes it).
        let mut other = vec![
            "/gnu/store/ddd-gcc-14.3.0".to_string(),
            "/gnu/store/eee-libfoo-gcc-toolchain-helper".to_string(),
        ];
        assert!(
            !super::substitute_gcc_toolchain(&mut other, tc),
            "bare gcc + interior-substring packages are not toolchain inputs"
        );
        assert_eq!(
            other,
            vec![
                "/gnu/store/ddd-gcc-14.3.0".to_string(),
                "/gnu/store/eee-libfoo-gcc-toolchain-helper".to_string(),
            ],
            "unchanged on no-op"
        );
    }

    // Exercise the override through the REAL engine path: assemble_recipe_drv reads TD_GCC_TOOLCHAIN and
    // substitutes it for the lock's guix gcc-toolchain when it assembles the .drv — the code the corpus
    // build path runs (not just the helper). Asserts the produced drv's TD_INPUTS + input-srcs reflect the
    // swap, and that the default (env unset) is unchanged. This is the reusable-mechanism analog of the
    // per-gate lock-rewrite: a build-recipe with TD_GCC_TOOLCHAIN set compiles with td's /td/store toolchain.
    #[test]
    fn assemble_recipe_drv_honors_td_gcc_toolchain() {
        let dir = std::env::temp_dir().join(format!("td-gcctc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("fixture.lock");
        // A minimal recipe lock: source + guix gcc-toolchain + glibc + make (2-field seed inputs).
        std::fs::write(
            &lock,
            "fixture-source /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fixture-1.0.tar.gz source\n\
             /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-toolchain-15.2.0 /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-toolchain-15.2.0\n\
             /gnu/store/cccccccccccccccccccccccccccccccc-glibc-2.41 /gnu/store/cccccccccccccccccccccccccccccccc-glibc-2.41\n\
             /gnu/store/dddddddddddddddddddddddddddddddd-make-4.4.1 /gnu/store/dddddddddddddddddddddddddddddddd-make-4.4.1\n",
        )
        .unwrap();
        let recipe = r#"{"name":"fixture","version":"1.0","buildSystem":"gnu"}"#;
        let lockp = lock.to_str().unwrap();
        let tc = "/td/store/ffffffffffffffffffffffffffffffff-gcc-toolchain-tdstore";
        let td_inputs = |drv: &drv::Derivation| {
            drv.env.iter().find(|(k, _)| k == "TD_INPUTS").map(|(_, v)| v.clone()).unwrap()
        };

        // WITH the override: the guix gcc-toolchain is swapped for the /td/store toolchain.
        std::env::set_var("TD_GCC_TOOLCHAIN", tc);
        let (_p, _f, drv, _s) = assemble_recipe_drv(recipe, lockp, &dir, None).unwrap();
        std::env::remove_var("TD_GCC_TOOLCHAIN");
        let ti = td_inputs(&drv);
        assert!(ti.contains(tc), "TD_INPUTS carries the /td/store toolchain: {ti}");
        assert!(!ti.contains("gcc-toolchain-15.2.0"), "guix gcc-toolchain swapped OUT of TD_INPUTS: {ti}");
        assert!(ti.contains("-glibc-2.41") && ti.contains("-make-4.4.1"), "other inputs untouched: {ti}");
        // The swapped path is an input-src too (staged into the build), not just an env value.
        assert!(drv.input_srcs.iter().any(|s| s == tc), "override is an input-src");
        assert!(
            !drv.input_srcs.iter().any(|s| s.contains("gcc-toolchain-15.2.0")),
            "guix gcc-toolchain is not an input-src (dropped from the drv closure)"
        );

        // WITHOUT the override (default): unchanged — the guix gcc-toolchain stays.
        let (_p, _f, drv0, _s) = assemble_recipe_drv(recipe, lockp, &dir, None).unwrap();
        let ti0 = td_inputs(&drv0);
        assert!(ti0.contains("gcc-toolchain-15.2.0"), "default keeps the guix gcc-toolchain: {ti0}");
        assert!(!ti0.contains(tc), "default has no /td/store toolchain: {ti0}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ABI-token-in-drv: a recipe's drv is keyed on the STABLE builder-identity path
    // (store::builder_identity_path), NOT the builder binary. So the `builder` line and
    // the builder input-src are that path, no real builder Cb appears anywhere in the drv,
    // and re-assembly is byte-deterministic — a builder-binary change can no longer move
    // the recipe's drv path or output path. That is the whole point of the migration.
    #[test]
    fn assemble_recipe_drv_keys_the_builder_on_the_abi_identity_path() {
        let dir = std::env::temp_dir().join(format!("td-abi-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("fixture.lock");
        std::fs::write(
            &lock,
            "fixture-source /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fixture-1.0.tar.gz source\n\
             /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.41 /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.41\n",
        )
        .unwrap();
        let recipe = r#"{"name":"fixture","version":"1.0","buildSystem":"gnu"}"#;
        let lockp = lock.to_str().unwrap();

        let id = store::builder_identity_path();
        let (drv_path, _f, drv, _s) = assemble_recipe_drv(recipe, lockp, &dir, None).unwrap();

        // The builder line is the stable identity path's td-builder — not a real Cb.
        assert_eq!(drv.builder, format!("{id}/bin/td-builder"), "builder line is the ABI identity");
        // The identity path is a builder input-src (the closure root realize stages)...
        assert!(drv.input_srcs.iter().any(|s| s == &id), "identity path is a builder input-src");
        // ...and NO builder BINARY path (only the identity DIR) is baked into the drv.
        assert!(
            !drv.input_srcs.iter().any(|s| s.contains("/bin/td-builder")),
            "no builder binary path among input-srcs: {:?}",
            drv.input_srcs
        );

        // Re-assembly is byte-deterministic — same drv path AND same output path — so a
        // builder-binary change (absent from the spec now) cannot move either.
        let (drv_path2, _f2, drv2, _s2) = assemble_recipe_drv(recipe, lockp, &dir, None).unwrap();
        assert_eq!(drv_path, drv_path2, "drv store path is stable across re-assembly");
        assert_eq!(
            drv.outputs.first().map(|o| &o.path),
            drv2.outputs.first().map(|o| &o.path),
            "recipe output path is stable across re-assembly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // realize re-keys ONLY the builder's own closure entry from its real content path to
    // the stable identity path (binding the real bytes there); its runtime refs and every
    // build input pass through untouched. Both entry shapes (bare, and canonical\ton-disk).
    // A minimal little-endian ELF64: an optional PT_INTERP, a whole-file identity PT_LOAD,
    // and a PT_DYNAMIC carrying DT_STRTAB, an optional DT_RUNPATH, and DT_NULL. `embed`
    // strings are appended raw AFTER the .dynstr — they are in the file bytes (a content scan
    // sees them) but are NOT reachable through PT_INTERP or DT_RUNPATH, mirroring glibc's
    // libc.so.6 baking the bash-static path into its `_PATH_BSHELL` string constant.
    fn synth_link_elf(interp: Option<&str>, runpath: Option<&str>, embed: &[&str]) -> Vec<u8> {
        fn le64(b: &mut [u8], off: usize, v: u64) { b[off..off + 8].copy_from_slice(&v.to_le_bytes()); }
        fn le32(b: &mut [u8], off: usize, v: u32) { b[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
        fn le16(b: &mut [u8], off: usize, v: u16) { b[off..off + 2].copy_from_slice(&v.to_le_bytes()); }
        let (ehdr, phent) = (64usize, 56usize);
        let phnum = if interp.is_some() { 3 } else { 2 };
        let ph_off = ehdr;
        let interp_off = ehdr + phnum * phent;
        let mut interp_bytes: Vec<u8> = Vec::new();
        if let Some(s) = interp { interp_bytes.extend_from_slice(s.as_bytes()); interp_bytes.push(0); }
        let dyn_off = interp_off + interp_bytes.len();
        let n_dyn = 2 + usize::from(runpath.is_some());
        let dyn_size = n_dyn * 16;
        let strtab_off = dyn_off + dyn_size;
        let mut dynstr: Vec<u8> = vec![0]; // index 0: the conventional empty string
        let rp_off = dynstr.len();
        if let Some(rp) = runpath { dynstr.extend_from_slice(rp.as_bytes()); dynstr.push(0); }
        let embed_off = strtab_off + dynstr.len();
        let mut embed_bytes: Vec<u8> = Vec::new();
        for s in embed { embed_bytes.extend_from_slice(s.as_bytes()); embed_bytes.push(0); }
        let total = embed_off + embed_bytes.len();

        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // ELFCLASS64
        b[5] = 1; // ELFDATA2LSB
        le64(&mut b, 0x20, ph_off as u64);
        le16(&mut b, 0x36, phent as u16);
        le16(&mut b, 0x38, phnum as u16);
        let mut pi = ph_off;
        if !interp_bytes.is_empty() {
            le32(&mut b, pi, 3); // PT_INTERP
            le64(&mut b, pi + 8, interp_off as u64); // p_offset
            le64(&mut b, pi + 32, interp_bytes.len() as u64); // p_filesz
            pi += phent;
        }
        le32(&mut b, pi, 1); // PT_LOAD, identity-mapped over the whole file
        le64(&mut b, pi + 8, 0);
        le64(&mut b, pi + 16, 0);
        le64(&mut b, pi + 32, total as u64);
        pi += phent;
        le32(&mut b, pi, 2); // PT_DYNAMIC
        le64(&mut b, pi + 8, dyn_off as u64);
        le64(&mut b, pi + 16, dyn_off as u64);
        le64(&mut b, pi + 32, dyn_size as u64);
        let mut de = dyn_off;
        le64(&mut b, de, 5); le64(&mut b, de + 8, strtab_off as u64); de += 16; // DT_STRTAB
        if runpath.is_some() {
            le64(&mut b, de, 29); le64(&mut b, de + 8, rp_off as u64); de += 16; // DT_RUNPATH
        }
        le64(&mut b, de, 0); le64(&mut b, de + 8, 0); // DT_NULL
        b[strtab_off..strtab_off + dynstr.len()].copy_from_slice(&dynstr);
        b[embed_off..embed_off + embed_bytes.len()].copy_from_slice(&embed_bytes);
        b
    }

    // The P0 regression (re #469): the control-plane builder's runtime closure must be
    // computed by DYNAMIC LINKAGE, not a content scan, so a runnable host shell that glibc
    // merely NAMES in a string constant / a helper script never enters the sandbox. This
    // reproduces the real leak — glibc's libc.so.6 embeds the absolute bash-static path in
    // its `_PATH_BSHELL` constant, and glibc's bin/ldd shebangs it — and asserts the
    // link-closure excludes bash-static while a content scan of the SAME store would include
    // it (so the two genuinely differ and the fix is load-bearing).
    #[test]
    fn resolve_link_closure_excludes_a_string_only_host_shell() {
        let dir = std::env::temp_dir().join(format!("rlc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seed = dir.join("seed");
        let cp = "/gnu/store";
        let glibc = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc-2.41";
        let bash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash-static-5.2.37";
        let gcclib = "cccccccccccccccccccccccccccccccc-gcc-14.3.0-lib";
        std::fs::create_dir_all(seed.join(glibc).join("lib")).unwrap();
        std::fs::create_dir_all(seed.join(glibc).join("bin")).unwrap();
        std::fs::create_dir_all(seed.join(bash).join("bin")).unwrap();
        std::fs::create_dir_all(seed.join(gcclib).join("lib")).unwrap();
        let ld = format!("/gnu/store/{glibc}/lib/ld-linux-x86-64.so.2");
        let bash_bin = format!("/gnu/store/{bash}/bin/bash");
        // libc.so.6: interp -> ld, NO run-path, but EMBEDS the bash path (the leak).
        std::fs::write(seed.join(glibc).join("lib/libc.so.6"), synth_link_elf(Some(&ld), None, &[&bash_bin])).unwrap();
        // ld-linux: fully static — no interp, no run-path.
        std::fs::write(seed.join(glibc).join("lib/ld-linux-x86-64.so.2"), synth_link_elf(None, None, &[])).unwrap();
        // glibc bin/ldd: a helper SCRIPT that shebangs bash (a content ref, not a link edge).
        std::fs::write(seed.join(glibc).join("bin/ldd"), format!("#!{bash_bin}\nexec ...\n").into_bytes()).unwrap();
        // libgcc_s.so.1: run-path -> glibc/lib (the real cross-package link edge).
        std::fs::write(seed.join(gcclib).join("lib/libgcc_s.so.1"), synth_link_elf(None, Some(&format!("/gnu/store/{glibc}/lib")), &[])).unwrap();
        // bash-static: the runnable host shell — the regression target.
        std::fs::write(seed.join(bash).join("bin/bash"), b"a runnable host shell").unwrap();

        let seed_dir = seed.to_string_lossy().into_owned();
        let od = |p: &str| seed.join(p).to_string_lossy().into_owned();
        let mut on_disk = std::collections::HashMap::new();
        on_disk.insert(format!("/gnu/store/{glibc}"), od(glibc));
        on_disk.insert(format!("/gnu/store/{bash}"), od(bash));
        on_disk.insert(format!("/gnu/store/{gcclib}"), od(gcclib));

        // The builder DB's direct runtime refs (glibc + gcc-lib).
        let roots = vec![format!("/gnu/store/{glibc}"), format!("/gnu/store/{gcclib}")];
        let link = resolve_link_closure(&roots, std::slice::from_ref(&seed_dir), cp, &on_disk).unwrap();
        assert!(link.contains(&format!("/gnu/store/{glibc}")), "glibc is a real runtime lib: {link:?}");
        assert!(link.contains(&format!("/gnu/store/{gcclib}")), "gcc-lib is a real runtime lib: {link:?}");
        assert!(
            !link.contains(&format!("/gnu/store/{bash}")),
            "bash-static (a string-only reference) must be ABSENT from the builder runtime closure: {link:?}"
        );

        // Prove the fix is load-bearing: a CONTENT scan of the same roots DOES pull bash-static
        // in (via libc.so.6's constant + bin/ldd), which is exactly what used to stage a host
        // shell into the sandbox.
        let candidates = vec![format!("/gnu/store/{glibc}"), format!("/gnu/store/{bash}"), format!("/gnu/store/{gcclib}")];
        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let content = scan_closure_hybrid(&mut scanner, &on_disk, &std::collections::HashMap::new(), &roots).unwrap();
        assert!(
            content.contains(&format!("/gnu/store/{bash}")),
            "a content scan SHOULD leak bash-static (the pre-fix behavior) — otherwise this test proves nothing: {content:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Integration boundary (re #469 round-10 P0 #1): FREEZE the host-shell provenance boundary
    // by driving the REAL staging-decision pipeline against an on-disk synthetic seed — the
    // builder arm's DYNAMIC-LINKAGE closure computation (`resolve_link_closure`, mirrored from
    // realize_drv line-for-line) feeding both `enforce_realize_input_policy` (the choke point
    // every realize path goes through) and the sandbox's `verify_staged_item` bind check.
    // `resolve_link_closure_excludes_a_string_only_host_shell` and
    // `blessed_seed_items_vouch_only_the_builder_runtime_closure` each prove ONE half with
    // hand-authored inputs; this proves their COMPOSITION — the tightened `builder_reach` the
    // linkage pass actually PRODUCES is exactly the slice the gate vouches — so no seam hides a
    // divergence between what resolution computes and what enforcement checks. A host shell a
    // drv could name by absolute `Step::Run` is thereby BOTH absent from the staged closure AND
    // rejected before any bind if a future content edge re-injects it: the reviewer's "absent or
    // rejected" criterion, held by the code paths a real build runs.
    #[test]
    fn host_shell_is_absent_from_and_rejected_by_the_realize_staging_boundary() {
        // A realistic synthetic seed: glibc NAMES bash (its `_PATH_BSHELL` constant + a bin/ldd
        // shebang) but the loader never LINKS it; the builder links only glibc + gcc-lib.
        let dir = std::env::temp_dir().join(format!("hsb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seed = dir.join("seed");
        let cp = "/gnu/store";
        let glibc = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc-2.41";
        let bash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash-static-5.2.37";
        let gcclib = "cccccccccccccccccccccccccccccccc-gcc-14.3.0-lib";
        let bld = "dddddddddddddddddddddddddddddddd-td-builder-0.1.0";
        std::fs::create_dir_all(seed.join(glibc).join("lib")).unwrap();
        std::fs::create_dir_all(seed.join(glibc).join("bin")).unwrap();
        std::fs::create_dir_all(seed.join(bash).join("bin")).unwrap();
        std::fs::create_dir_all(seed.join(gcclib).join("lib")).unwrap();
        std::fs::create_dir_all(seed.join(bld).join("bin")).unwrap();
        let ld = format!("/gnu/store/{glibc}/lib/ld-linux-x86-64.so.2");
        let bash_bin = format!("/gnu/store/{bash}/bin/bash");
        // libc.so.6: interp -> ld, EMBEDS the bash path (the leak the content scan followed).
        std::fs::write(seed.join(glibc).join("lib/libc.so.6"), synth_link_elf(Some(&ld), None, &[&bash_bin])).unwrap();
        std::fs::write(seed.join(glibc).join("lib/ld-linux-x86-64.so.2"), synth_link_elf(None, None, &[])).unwrap();
        // glibc bin/ldd: a helper SCRIPT shebanging bash (a content ref, not a link edge).
        std::fs::write(seed.join(glibc).join("bin/ldd"), format!("#!{bash_bin}\nexec ...\n").into_bytes()).unwrap();
        // libgcc_s.so.1: run-path -> glibc/lib (the real cross-package link edge).
        std::fs::write(seed.join(gcclib).join("lib/libgcc_s.so.1"), synth_link_elf(None, Some(&format!("/gnu/store/{glibc}/lib")), &[])).unwrap();
        // bash-static: the runnable host shell — the regression target.
        std::fs::write(seed.join(bash).join("bin/bash"), b"a runnable host shell").unwrap();
        // The builder ELF links its real direct runtime refs (glibc + gcc-lib) and nothing else.
        std::fs::write(seed.join(bld).join("bin/td-builder"), synth_link_elf(Some(&ld), Some(&format!("/gnu/store/{glibc}/lib:/gnu/store/{gcclib}/lib")), &[])).unwrap();

        let seed_dir = seed.to_string_lossy().into_owned();
        let od = |p: &str| seed.join(p).to_string_lossy().into_owned();
        let sp = |p: &str| format!("/gnu/store/{p}");
        let mut on_disk = std::collections::HashMap::new();
        for p in [glibc, bash, gcclib, bld] {
            on_disk.insert(sp(p), od(p));
        }

        // Compute the builder closure EXACTLY as realize_drv's builder arm does: the builder
        // tree binds `canonical\ton-disk` and tracks into builder_reach; its runtime deps are
        // resolved by DYNAMIC LINKAGE from the BUILDER TREE ITSELF (the arm's root), then the
        // bare self-entry is dropped (the tree is bound tabbed). glibc + gcc-lib flow into BOTH
        // the staged closure and builder_reach; bash (glibc merely names it) does not.
        let mut closure: Vec<String> = vec![format!("{}\t{}", sp(bld), od(bld))];
        let mut builder_reach: std::collections::BTreeSet<String> = std::iter::once(sp(bld)).collect();
        let mut linkage =
            resolve_link_closure(std::slice::from_ref(&sp(bld)), std::slice::from_ref(&seed_dir), cp, &on_disk)
                .unwrap();
        linkage.remove(&sp(bld));
        for canon in linkage {
            builder_reach.insert(canon.clone());
            closure.push(canon);
        }

        // (1) ABSENT: the host shell is neither staged nor vouchable; the real runtime libs are.
        assert!(builder_reach.contains(&sp(glibc)) && builder_reach.contains(&sp(gcclib)), "runtime libs reachable: {builder_reach:?}");
        assert!(!builder_reach.contains(&sp(bash)), "host shell absent from builder_reach: {builder_reach:?}");
        assert!(!closure.iter().any(|e| e.contains(bash)), "host shell absent from the staged closure: {closure:?}");

        // The manifest the seed's dbs issue: builder ControlPlaneBuilder, runtime libs
        // BlessedSeedClosure. (The content scan the fix removed WOULD also have blessed bash.)
        let si = |o| sandbox::StagedInput { nar_hash: "sha256:00".to_string(), origin: o };
        let mut manifest = sandbox::StageManifest::new();
        manifest.insert(sp(bld), si(sandbox::InputOrigin::ControlPlaneBuilder));
        manifest.insert(sp(glibc), si(sandbox::InputOrigin::BlessedSeedClosure));
        manifest.insert(sp(gcclib), si(sandbox::InputOrigin::BlessedSeedClosure));
        let builder_exec = format!("{}/bin/td-builder", sp(bld));
        let roots = vec![sp(bld)];

        // (2) POSITIVE CONTROL: the honest linkage closure — builder + its runtime libs — passes.
        enforce_realize_input_policy(&builder_exec, &roots, &closure, &builder_reach, &manifest, None)
            .expect("builder + its dynamic-linkage runtime closure is admissible");

        // (3) REJECTED: a future content edge / crafted drv re-drags bash into the closure as a
        // blessed-seed row. Against the TIGHTENED builder_reach it is no longer vouched, so rule
        // 3 reds BEFORE any bind.
        let mut leaked = closure.clone();
        leaked.push(sp(bash));
        let mut m2 = manifest.clone();
        m2.insert(sp(bash), si(sandbox::InputOrigin::BlessedSeedClosure));
        let err = enforce_realize_input_policy(&builder_exec, &roots, &leaked, &builder_reach, &m2, None)
            .expect_err("a blessed host shell outside the builder's runtime closure must be rejected");
        assert!(err.contains(bash) && err.contains("host tools are not admissible"), "rejection names the host-tool rule: {err}");

        // (4) LOAD-BEARING: the tightening is WHAT rejects it. Had builder_reach still vouched
        // bash (the pre-fix content-scan leak), the SAME gate would ACCEPT the SAME closure — so
        // removing bash from builder_reach is exactly the fix, not incidental.
        let mut pre_fix_reach = builder_reach.clone();
        pre_fix_reach.insert(sp(bash));
        enforce_realize_input_policy(&builder_exec, &roots, &leaked, &pre_fix_reach, &m2, None)
            .expect("counterfactual: a builder_reach still vouching bash ACCEPTS it — the pre-fix leak");

        // (5) BIND-BOUNDARY BELT: even past the gate, the sandbox re-hashes every item against
        // the manifest; a host shell with no honest record can never bind (verify_staged_item).
        let err = sandbox::verify_staged_item(&manifest, &sp(bash), &od(bash))
            .expect_err("the sandbox refuses to stage an item no td-owned db vouches for");
        assert!(err.to_string().contains("no td-owned store-db record"), "bind-boundary rejection: {err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // A STATICALLY-linked stage0 builder (builder/src/stage0.rs) stages NOTHING host — even
    // though it INLINES glibc's `.rodata` and so its own bytes name glibc's store path AND
    // glibc's `_PATH_BSHELL` bash-static path (re #469). The realize builder arm roots its
    // DYNAMIC-LINKAGE walk on the builder binary itself: a static builder links nothing, so
    // its staged closure is just the builder tree. This is the completion of the static-link
    // fix — the reason the content-scanned bash-static/glibc no longer reach the sandbox.
    #[test]
    fn static_builder_stages_nothing_even_when_it_embeds_glibc_host_paths() {
        let dir = std::env::temp_dir().join(format!("sbn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seed = dir.join("seed");
        let cp = "/gnu/store";
        let glibc = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc-2.41";
        let bash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash-static-5.2.37";
        let bld = "dddddddddddddddddddddddddddddddd-td-builder-0.1.0";
        std::fs::create_dir_all(seed.join(glibc).join("lib")).unwrap();
        std::fs::create_dir_all(seed.join(bash).join("bin")).unwrap();
        std::fs::create_dir_all(seed.join(bld).join("bin")).unwrap();
        std::fs::write(seed.join(bash).join("bin/bash"), b"a runnable host shell").unwrap();
        std::fs::write(seed.join(glibc).join("lib/libc.a"), b"!<arch>\n").unwrap();
        // The STATIC builder: NO interp, NO run-path (links nothing) — but its bytes EMBED
        // glibc's own store path AND glibc's bash-static path (the inlined `.rodata`).
        let glibc_sp = format!("/gnu/store/{glibc}");
        let bash_bin = format!("/gnu/store/{bash}/bin/bash");
        std::fs::write(
            seed.join(bld).join("bin/td-builder"),
            synth_link_elf(None, None, &[&glibc_sp, &bash_bin]),
        )
        .unwrap();

        let seed_dir = seed.to_string_lossy().into_owned();
        let od = |p: &str| seed.join(p).to_string_lossy().into_owned();
        let sp = |p: &str| format!("/gnu/store/{p}");
        let mut on_disk = std::collections::HashMap::new();
        for p in [glibc, bash, bld] {
            on_disk.insert(sp(p), od(p));
        }

        // The arm: root the LINKAGE walk on the builder tree, drop the bare self.
        let mut linkage =
            resolve_link_closure(std::slice::from_ref(&sp(bld)), std::slice::from_ref(&seed_dir), cp, &on_disk)
                .unwrap();
        linkage.remove(&sp(bld));
        assert!(
            linkage.is_empty(),
            "a static builder links NOTHING, so nothing host is staged — not the glibc path nor \
             the bash-static path it merely embeds: {linkage:?}"
        );

        // Load-bearing: a CONTENT scan of the SAME static builder DOES pull both in — which is
        // exactly what used to fail provenance (bash-static) and leak glibc.
        let candidates = vec![sp(glibc), sp(bash), sp(bld)];
        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let content = scan_closure_hybrid(
            &mut scanner,
            &on_disk,
            &std::collections::HashMap::new(),
            std::slice::from_ref(&sp(bld)),
        )
        .unwrap();
        assert!(
            content.iter().any(|e| e.contains(bash)) && content.iter().any(|e| e.contains(glibc)),
            "a content scan SHOULD surface the embedded glibc + bash-static paths (the pre-fix \
             leak) — else this proves nothing: {content:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rekey_builder_entry_remaps_only_the_builder() {
        let real = "/gnu/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-td-builder-0.1.0";
        let stable = "/td/store/ssssssssssssssssssssssssssssssss-td-builder";
        // Bare, daemon-resident builder: binds from its own real path.
        let closure = vec![
            real.to_string(),
            "/gnu/store/gggggggggggggggggggggggggggggggg-glibc-2.41".to_string(),
            "/td/store/tttttttttttttttttttttttttttttttt-make-4.4.1\t/cache/tttttttttttttttttttttttttttttttt-make-4.4.1".to_string(),
        ];
        let out = rekey_builder_entry(closure, real, stable);
        assert_eq!(out.first().map(String::as_str), Some(format!("{stable}\t{real}").as_str()), "bare builder -> stable\\treal");
        assert_eq!(out.get(1).map(String::as_str), Some("/gnu/store/gggggggggggggggggggggggggggggggg-glibc-2.41"), "runtime ref untouched");
        assert!(out.get(2).is_some_and(|e| e.starts_with("/td/store/tttt")), "other on-disk entry untouched");

        // The override form: `real\ton-disk` -> `stable\ton-disk` (real bytes at stable).
        let ov = vec![format!("{real}\t/bstore/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-td-builder-0.1.0")];
        let ovo = rekey_builder_entry(ov, real, stable);
        assert_eq!(ovo.first().map(String::as_str), Some(format!("{stable}\t/bstore/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-td-builder-0.1.0").as_str()));

        // No builder entry present -> a no-op.
        let none = vec!["/gnu/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-foo".to_string()];
        assert_eq!(rekey_builder_entry(none.clone(), real, stable), none, "no builder entry -> unchanged");
    }

    // The staging manifest mirrors the real builder's record onto the stable identity path,
    // so the sandbox — which keys verify_staged_item on a closure entry's canonical (left)
    // half — accepts the builder re-keyed to `stable_id`. The real record stays (the reuse
    // digest is taken over the un-mirrored manifest), non-builder rows are untouched, and a
    // real builder with no record mirrors nothing (enforcement runs first, over the real path).
    #[test]
    fn manifest_with_builder_alias_mirrors_only_the_builder_record() {
        let real = "/gnu/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-td-builder-0.1.0".to_string();
        let stable = "/td/store/ssssssssssssssssssssssssssssssss-td-builder";
        let glibc = "/gnu/store/gggggggggggggggggggggggggggggggg-glibc-2.41".to_string();
        let mut manifest = sandbox::StageManifest::new();
        manifest.insert(
            real.clone(),
            sandbox::StagedInput {
                nar_hash: "sha256:aa".into(),
                origin: sandbox::InputOrigin::ControlPlaneBuilder,
            },
        );
        manifest.insert(
            glibc.clone(),
            sandbox::StagedInput {
                nar_hash: "sha256:bb".into(),
                origin: sandbox::InputOrigin::BlessedSeedClosure,
            },
        );

        // The stable id carries the SAME record as the real builder (same hash + origin),
        // and the real record + every other row are left in place.
        let staged = manifest_with_builder_alias(&manifest, &Some(real.clone()), stable);
        assert_eq!(staged.get(stable), manifest.get(&real), "stable id mirrors the real builder record");
        assert_eq!(staged.get(&real), manifest.get(&real), "real builder record kept");
        assert_eq!(staged.get(&glibc), manifest.get(&glibc), "non-builder row untouched");

        // A non-td drv (no real builder) mirrors nothing.
        let none = manifest_with_builder_alias(&manifest, &None, stable);
        assert!(none.get(stable).is_none(), "no real builder -> no alias");
        assert_eq!(none.len(), manifest.len(), "no-op leaves the manifest as-is");

        // A real builder absent from the manifest mirrors nothing (enforcement would reject
        // it first): there is no record to copy onto the stable id.
        let absent = "/gnu/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-td-builder-0.1.0".to_string();
        let norec = manifest_with_builder_alias(&manifest, &Some(absent), stable);
        assert!(norec.get(stable).is_none(), "real builder absent from manifest -> no alias");
    }

    // The final GCC lives below its recipe output, so run_rust needs exact
    // compiler/include paths as well as interp/RUNPATH/-B. The build sandbox
    // clears ambient env; every value must therefore ride in the drv.
    #[test]
    fn assemble_recipe_drv_forwards_td_rust_store_env() {
        let dir = std::env::temp_dir().join(format!("td-ruststore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("ripgrep.lock");
        std::fs::write(
            &lock,
            "ripgrep-source /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ripgrep-14.1.1.tar.gz source\n\
             /td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-14.3.0-x86_64-native /td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-14.3.0-x86_64-native\n\
             /td/store/cccccccccccccccccccccccccccccccc-rust-1.96.0-x86_64-store-native /td/store/cccccccccccccccccccccccccccccccc-rust-1.96.0-x86_64-store-native\n",
        )
        .unwrap();
        let recipe = r#"{"name":"ripgrep","version":"14.1.1","buildSystem":"rust","bins":["rg"]}"#;
        let lockp = lock.to_str().unwrap();
        let env_of = |drv: &drv::Derivation, k: &str| {
            drv.env.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
        };
        let interp = "/td/store/cccccccccccccccccccccccccccccccc-glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2";
        let rpath = "/td/store/cccccccccccccccccccccccccccccccc-glibc-2.41-x86_64/lib";
        let bdir = rpath;
        let cc = "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc/stage/td/store/gcc/bin/gcc";
        let cxx = "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc/stage/td/store/gcc/bin/g++";
        let include = "/td/store/dddddddddddddddddddddddddddddddd-glibc/stage/td/store/glibc/include";

        // WITH the vars set: the rust drv carries them so run_rust can bake interp/RUNPATH/-B.
        std::env::set_var("TD_RUST_STORE_INTERP", interp);
        std::env::set_var("TD_RUST_STORE_RPATH", rpath);
        std::env::set_var("TD_RUST_STORE_BDIR", bdir);
        std::env::set_var("TD_RUST_STORE_CC", cc);
        std::env::set_var("TD_RUST_STORE_CXX", cxx);
        std::env::set_var("TD_RUST_STORE_INCLUDE", include);
        let (_p, _f, drv, _s) = assemble_recipe_drv(recipe, lockp, &dir, None).unwrap();
        std::env::remove_var("TD_RUST_STORE_INTERP");
        std::env::remove_var("TD_RUST_STORE_RPATH");
        std::env::remove_var("TD_RUST_STORE_BDIR");
        std::env::remove_var("TD_RUST_STORE_CC");
        std::env::remove_var("TD_RUST_STORE_CXX");
        std::env::remove_var("TD_RUST_STORE_INCLUDE");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_INTERP").as_deref(), Some(interp), "interp forwarded to the drv env");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_RPATH").as_deref(), Some(rpath), "rpath forwarded");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_BDIR").as_deref(), Some(bdir), "bdir forwarded");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_CC").as_deref(), Some(cc), "cc forwarded");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_CXX").as_deref(), Some(cxx), "cxx forwarded");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_INCLUDE").as_deref(), Some(include), "include forwarded");

        // WITHOUT the vars (default): none are emitted.
        let (_p, _f, drv0, _s) = assemble_recipe_drv(recipe, lockp, &dir, None).unwrap();
        assert!(env_of(&drv0, "TD_RUST_STORE_INTERP").is_none(), "no interp in the drv env by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_RPATH").is_none(), "no rpath by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_BDIR").is_none(), "no bdir by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_CC").is_none(), "no cc by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_CXX").is_none(), "no cxx by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_INCLUDE").is_none(), "no include by default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // build-plan --auto derives TD_RUST_STORE_* from the declared native inputs (the env
    // is cleared in the graph build). The derived strings must equal the rust-toolchain
    // check's TD_SHELL_NATIVE_* formulas so a `td shell` and an `--auto` build of the same
    // recipe link identically (re #547).
    #[test]
    fn derive_native_rust_link_env_mirrors_the_check_formulas() {
        let mk = |name: &str, path: &str| lock::Entry {
            name: name.to_string(),
            path: path.to_string(),
            class: lock::Class::Seed,
        };
        let gcc = "/td/store/gggggggggggggggggggggggggggggggg-gcc-x86-64-self";
        let binutils = "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-binutils-x86-64-self";
        let glibc = "/td/store/llllllllllllllllllllllllllllllll-glibc-x86-64";
        let entries = vec![
            mk("uutils-source", "/td/store/ssssssssssssssssssssssssssssssss-uutils-source"),
            mk("rust-toolchain", "/td/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-rust-toolchain"),
            mk("gcc-x86-64-self", gcc),
            mk("binutils-x86-64-self", binutils),
            mk("glibc-x86-64", glibc),
            mk("busybox-x86-64", "/td/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-busybox-x86-64"),
        ];
        let d = derive_native_rust_link_env(&entries).expect("all three inputs present");
        let gp = format!("{glibc}/{NATIVE_GLIBC_STAGE}");
        let gccp = format!("{gcc}/{NATIVE_GCC_STAGE}");
        assert_eq!(d.interp, format!("{gp}/lib/ld-linux-x86-64.so.2"));
        assert_eq!(d.rpath, format!("{gp}/lib"));
        assert_eq!(d.bdir, format!("{binutils}/bin:{gp}/lib"));
        assert_eq!(d.cc, format!("{gccp}/bin/gcc"));
        assert_eq!(d.cxx, format!("{gccp}/bin/g++"));
        assert_eq!(d.include, format!("{gp}/include"));

        // A rust recipe that names no native toolchain is not linkable this way.
        assert!(derive_native_rust_link_env(&[mk("uutils-source", "/x")]).is_none());
        // Missing any ONE of the three ⇒ None (binutils absent here).
        assert!(derive_native_rust_link_env(&[
            mk("gcc-x86-64-self", gcc),
            mk("glibc-x86-64", glibc),
        ])
        .is_none());
    }

    // The --auto crate gate: every checksummed committed-lock entry must be present in the
    // warm vendor dir as `<name>-<ver>.crate` with a matching sha256; the root (no checksum)
    // is ignored, a tampered/missing crate fails closed (re #547).
    #[test]
    fn stage_verified_vendor_gates_on_the_committed_checksums() {
        let dir = std::env::temp_dir().join(format!("td-vendorverify-{}", std::process::id()));
        let staged =
            std::env::temp_dir().join(format!("td-vendorstaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write_crate = |nv: &str, bytes: &[u8]| {
            let p = dir.join(format!("{nv}.crate"));
            std::fs::write(&p, bytes).unwrap();
            crate::sha256::sha256_file(&p).unwrap()
        };
        let foo = write_crate("foo-1.2.3", b"foo crate bytes");
        let bar = write_crate("bar-0.1.0", b"bar crate bytes");
        let lock = format!(
            "version = 4\n\n\
             [[package]]\nname = \"theroot\"\nversion = \"0.9.0\"\n\n\
             [[package]]\nname = \"foo\"\nversion = \"1.2.3\"\nchecksum = \"{foo}\"\n\n\
             [[package]]\nname = \"bar\"\nversion = \"0.1.0\"\nchecksum = \"{bar}\"\n"
        );
        // All present + matching (root without a checksum is excluded ⇒ 2 verified), and
        // exactly the verified crates land in the fresh private staged tree.
        assert_eq!(stage_verified_vendor(&dir, &lock, &staged).unwrap(), 2);
        assert!(staged.join("foo-1.2.3.crate").is_file());
        assert!(staged.join("bar-0.1.0.crate").is_file());
        // A committed crate absent from the vendor dir fails closed.
        let missing = format!("{lock}\n[[package]]\nname = \"baz\"\nversion = \"2.0.0\"\nchecksum = \"{foo}\"\n");
        assert!(stage_verified_vendor(&dir, &missing, &staged).is_err());
        // A checksum mismatch on a PINNED crate (bar re-pinned to foo's sha) fails closed
        // at the per-crate hash. Pin the full dir set (foo+bar) so set-equality passes and
        // this isolates the checksum-mismatch branch rather than the reject-extras scan.
        let tampered = format!(
            "version = 4\n\n\
             [[package]]\nname = \"foo\"\nversion = \"1.2.3\"\nchecksum = \"{foo}\"\n\n\
             [[package]]\nname = \"bar\"\nversion = \"0.1.0\"\nchecksum = \"{foo}\"\n"
        );
        assert!(stage_verified_vendor(&dir, &tampered, &staged).is_err());
        // A lock pinning no checksummed crates is rejected (nothing to gate).
        assert!(stage_verified_vendor(&dir, "version = 4\n", &staged).is_err());
        // A smuggled `.crate` the lock does NOT pin fails closed (set equality, not subset).
        write_crate("evil-9.9.9", b"smuggled newer version");
        assert!(stage_verified_vendor(&dir, &lock, &staged).is_err());
        // A committed lock whose crate name carries a path component is rejected before
        // any filesystem join (no `<dir>.join(fname)` escape from the staged/vendor trees).
        let traversal = format!(
            "version = 4\n\n[[package]]\nname = \"../../../../tmp/pwn\"\nversion = \"1.0.0\"\nchecksum = \"{foo}\"\n"
        );
        assert!(stage_verified_vendor(&dir, &traversal, &staged).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&staged);
    }

    #[test]
    fn reject_unpinned_dependencies_rejects_git_and_unchecksummed() {
        let hex = "a".repeat(64);
        // A workspace root (no source) plus a checksummed registry dep: accepted.
        let clean = format!(
            "version = 4\n\n\
             [[package]]\nname = \"theroot\"\nversion = \"0.9.0\"\n\n\
             [[package]]\nname = \"foo\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{hex}\"\n"
        );
        assert!(reject_unpinned_dependencies(&clean).is_ok());
        // A git dependency is rejected (git deps are unsupported).
        let git = "version = 4\n\n[[package]]\nname = \"bar\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/bar#deadbeef\"\n";
        assert!(reject_unpinned_dependencies(git).is_err());
        // A registry dependency with no checksum is rejected.
        let no_sum = "version = 4\n\n[[package]]\nname = \"baz\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        assert!(reject_unpinned_dependencies(no_sum).is_err());
        // A registry dependency whose checksum is not 64 hex chars is rejected.
        let bad_sum = "version = 4\n\n[[package]]\nname = \"qux\"\nversion = \"3.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"nothex\"\n";
        assert!(reject_unpinned_dependencies(bad_sum).is_err());
        // Non-canonical spacing around `=` must not hide a git source from the check.
        let tight_git = "version = 4\n\n[[package]]\nname=\"bar\"\nversion=\"0.1.0\"\nsource=\"git+https://example.com/bar#deadbeef\"\n";
        assert!(reject_unpinned_dependencies(tight_git).is_err());
        // The actual committed uutils lock (verbatim upstream) must pass the gate.
        let real = concat!(env!("CARGO_MANIFEST_DIR"), "/../recipes/locks/uutils/Cargo.lock");
        if let Ok(text) = std::fs::read_to_string(real) {
            assert!(
                reject_unpinned_dependencies(&text).is_ok(),
                "the committed uutils Cargo.lock must be fully checksum-pinned"
            );
        }
    }

    // #429 code-review fix: a mesboot recipe with NO declared `sourceInput` (make-test —
    // it only RUNS a sibling rung's output) is the ONLY case a missing `<name>-source`
    // lock line is tolerated.
    #[test]
    fn assemble_recipe_drv_tolerates_no_source_only_when_the_recipe_declares_none() {
        let dir = std::env::temp_dir().join(format!("td-nosrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("make-test.lock");
        // No `make-test-source` line at all.
        std::fs::write(
            &lock,
            "make-x86-64 /td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-make-4.4.1-x86_64 td-recipe-output\n",
        )
        .unwrap();
        let recipe = r#"{"name":"make-test","version":"1.0","buildSystem":"mesboot","nativeInputs":["make-x86-64"],"steps":[]}"#;
        let (_p, _f, _drv, source) =
            assemble_recipe_drv(recipe, lock.to_str().unwrap(), &dir, None).unwrap();
        assert_eq!(source, "", "make-test has no source (none declared)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #429 code-review fix (CONFIRMED finding): a mesboot recipe that DOES declare a
    // `sourceInput` must still hard-error on a lock missing the `<name>-source` line —
    // the exemption is scoped to BOTH the build system AND the recipe's own declaration,
    // not the build system alone, so a real mistake (a rung that needs a source, but
    // whose lock lost the line) is still caught here instead of failing later, deep in
    // step execution, when a `{in:<name>-source}` template has nothing to resolve.
    #[test]
    fn assemble_recipe_drv_still_requires_source_for_a_mesboot_recipe_that_declares_one() {
        let dir = std::env::temp_dir().join(format!("td-nosrc-bug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("gcc-14.lock");
        // No `gcc-14-source` line — as if --auto's synthesis (or a hand-edit) dropped it.
        std::fs::write(
            &lock,
            "binutils-mesboot /td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-binutils-mesboot td-recipe-output\n",
        )
        .unwrap();
        let recipe = r#"{"name":"gcc-14","version":"14.3.0","buildSystem":"mesboot","sourceInput":"gcc-14-source","nativeInputs":["binutils-mesboot"],"steps":[]}"#;
        let err = assemble_recipe_drv(recipe, lock.to_str().unwrap(), &dir, None).unwrap_err();
        assert!(err.contains("lock has no `gcc-14-source' entry"), "unexpected error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- content-scan closure (retire /var/guix/db): scan_candidate_index + scan_closure_hybrid ----
    // The daemon-DB-free input-closure computation realize_drv now uses. A store DIR is
    // content-scanned for the seed roots (candidate index keyed by CANONICAL path, bytes
    // NAR-read from on-disk), UNIONed with any td-OWNED store DB's direct refs (build-plan's
    // td.dbs — a td-built dep whose bytes live OUTSIDE the scanned dir). Covers: the canonical
    // vs on-disk mapping, the `.lock` aux-file skip, a content-scanned transitive closure, and
    // the hybrid extra-refs edge (a root with no on-disk bytes resolved via the extra-db map).
    #[test]
    fn content_scan_closure_spans_seed_dir_and_extra_dbs() {
        use std::collections::HashMap;
        // 32-char nix-base32 hash parts (alphabet omits e,o,u,t).
        let glibc_h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let gcc_h = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let tddep_h = "cccccccccccccccccccccccccccccccc";
        let dir = std::env::temp_dir().join(format!("td-cscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let od = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let canon = |name: &str| format!("/gnu/store/{name}");
        // glibc: a leaf with no store references. gcc: references glibc (its 32-char hash is
        // literally in the bytes, the daemon's own reference criterion). A `.lock` aux file
        // shares glibc's hash and MUST be skipped by the candidate index.
        std::fs::write(od(&format!("{glibc_h}-glibc-2.41")), b"a leaf, no store references here\n").unwrap();
        std::fs::write(
            od(&format!("{gcc_h}-gcc-14")),
            format!("gcc links libc at /gnu/store/{glibc_h}-glibc-2.41/lib\n").as_bytes(),
        )
        .unwrap();
        std::fs::write(od(&format!("{glibc_h}-glibc-2.41.lock")), b"").unwrap();

        let dirs = [dir.to_string_lossy().into_owned()];
        let (candidates, on_disk) = scan_candidate_index(&dirs, "/gnu/store").unwrap();
        // Two candidates (the .lock aux file is skipped), keyed by CANONICAL path.
        assert_eq!(candidates.len(), 2, "candidates (lock aux file skipped): {candidates:?}");
        assert!(candidates.contains(&canon(&format!("{glibc_h}-glibc-2.41"))));
        assert!(candidates.contains(&canon(&format!("{gcc_h}-gcc-14"))));
        // Canonical path maps to the ON-DISK bytes (here dir == canonical prefix's stand-in).
        assert_eq!(on_disk[&canon(&format!("{glibc_h}-glibc-2.41"))], od(&format!("{glibc_h}-glibc-2.41")));

        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let empty: HashMap<String, Vec<String>> = HashMap::new();

        // Pure content-scan from the gcc root: BFS finds glibc via gcc's bytes.
        let cl = scan_closure_hybrid(&mut scanner, &on_disk, &empty, &[canon(&format!("{gcc_h}-gcc-14"))]).unwrap();
        let cl: Vec<String> = cl.into_iter().collect();
        assert_eq!(
            cl,
            vec![canon(&format!("{glibc_h}-glibc-2.41")), canon(&format!("{gcc_h}-gcc-14"))],
            "content-scan closure of gcc must be {{gcc, glibc}}"
        );

        // Hybrid: a td-built dep whose bytes live OUTSIDE the scanned dir. Its refs come from
        // the extra-db map (td.db), then that ref (gcc) is content-scanned into glibc.
        let mut extra: HashMap<String, Vec<String>> = HashMap::new();
        extra.insert(canon(&format!("{tddep_h}-mylib-1")), vec![canon(&format!("{gcc_h}-gcc-14"))]);
        let hy = scan_closure_hybrid(&mut scanner, &on_disk, &extra, &[canon(&format!("{tddep_h}-mylib-1"))]).unwrap();
        let hy: Vec<String> = hy.into_iter().collect();
        assert_eq!(
            hy,
            vec![
                canon(&format!("{glibc_h}-glibc-2.41")),
                canon(&format!("{gcc_h}-gcc-14")),
                canon(&format!("{tddep_h}-mylib-1")),
            ],
            "hybrid closure must span the td-dep (extra db) + its content-scanned seed refs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- multi-store store-closure-scan: candidate index spans several dirs -------------
    // The store-closure-scan primitive R3 uses to close a td-built subject whose OUTPUT tree
    // lives in a build scratch's `newstore` while its deps live in the seed store: the
    // candidate index spans BOTH dirs, the FIRST dir is the canonical prefix the roots use,
    // and — because matching is by 32-char HASH, not by prefix — a member whose bytes sit
    // under the non-canonical dir still resolves. This mirrors scan_candidate_index(&[seed,
    // newstore], seed) exactly as the `store-closure-scan seed,newstore ROOT` arm calls it.
    #[test]
    fn multi_store_scan_spans_seed_and_newstore() {
        use std::collections::HashMap;
        let glibc_h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let subject_h = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let base = std::env::temp_dir().join(format!("td-multiscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed"); // the canonical /gnu/store stand-in (deps live here)
        let newstore = base.join("newstore"); // a build scratch's newstore (the output only)
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::create_dir_all(&newstore).unwrap();
        // glibc is a leaf in the seed; subject is the output, present ONLY in newstore,
        // and its bytes reference glibc by hash (the daemon's own reference criterion).
        std::fs::write(seed.join(format!("{glibc_h}-glibc-2.41")), b"a libc leaf\n").unwrap();
        std::fs::write(
            newstore.join(format!("{subject_h}-subject-1.0")),
            format!("subject links /gnu/store/{glibc_h}-glibc-2.41/lib/libc.so\n").as_bytes(),
        )
        .unwrap();

        let seed_s = seed.to_string_lossy().into_owned();
        let newstore_s = newstore.to_string_lossy().into_owned();
        let canon = |name: &str| format!("/gnu/store/{name}");
        // The FIRST dir is the canonical prefix; both dirs are byte sources.
        let (candidates, on_disk) =
            scan_candidate_index(&[seed_s.clone(), newstore_s.clone()], "/gnu/store").unwrap();
        // The subject's canonical path uses the /gnu/store prefix, but its BYTES come from newstore.
        let subject_c = canon(&format!("{subject_h}-subject-1.0"));
        let glibc_c = canon(&format!("{glibc_h}-glibc-2.41"));
        assert!(candidates.contains(&subject_c) && candidates.contains(&glibc_c));
        assert_eq!(on_disk[&subject_c], newstore.join(format!("{subject_h}-subject-1.0")).to_string_lossy());
        assert_eq!(on_disk[&glibc_c], seed.join(format!("{glibc_h}-glibc-2.41")).to_string_lossy());

        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        // Closing from the subject root pulls glibc out of the OTHER store dir, by hash.
        let mut cl: Vec<String> =
            scan_closure_hybrid(&mut scanner, &on_disk, &empty, &[subject_c.clone()]).unwrap().into_iter().collect();
        cl.sort();
        assert_eq!(cl, vec![glibc_c, subject_c], "multi-store closure must span both stores");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- #292: roots whose canonical prefix differs from the candidate index's ----------
    // TD_STORE_DIR=/td/store builds from locks whose seed roots
    // are /gnu/store paths. The walk only content-scans a path whose CANONICAL form is an
    // index key — so a /gnu/store root against a single-prefix-canonicalized index collapsed
    // to "roots only" and dropped every transitive runtime dep (coreutils → gmp: expr died
    // on libgmp.so.10). VERIFIED-RED: composed the pre-fix way (prefix = the active
    // /td/store, no overrides), the gmp assertion below fails with closure == roots. This
    // composes index + overrides + walk exactly as realize_drv now does: /gnu/store as the
    // seed dirs' canonical home, per-hash TRUE canonicals restored from the drv roots and
    // the td-owned extra DBs (recanonicalize_candidates).
    #[test]
    fn cross_prefix_roots_keep_transitive_deps() {
        use std::collections::HashMap;
        let cu_h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // coreutils (a /gnu/store lock root)
        let gmp_h = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"; // gmp (transitive: only in coreutils' bytes)
        let tc_h = "cccccccccccccccccccccccccccccccc"; // td-built toolchain (a /td/store root)
        let gl_h = "dddddddddddddddddddddddddddddddd"; // td-built glibc (a /td/store root, td-db-registered)
        let dir = std::env::temp_dir().join(format!("td-xprefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wr = |name: &str, bytes: String| std::fs::write(dir.join(name), bytes).unwrap();
        // The warm-seed staging dir mixes guix-captured entries and copied-in td-built ones.
        wr(&format!("{cu_h}-coreutils-9.1"), format!("expr RPATHs /gnu/store/{gmp_h}-gmp-6.3.0/lib\n"));
        wr(&format!("{gmp_h}-gmp-6.3.0"), "a guix-built leaf\n".to_string());
        wr(&format!("{tc_h}-gcc-toolchain-tdstore"), format!("wrapper: -Wl,--dynamic-linker /td/store/{gl_h}-glibc-2.41/lib/ld-linux.so.2\n"));
        wr(&format!("{gl_h}-glibc-2.41"), "a td-built leaf\n".to_string());
        let dirs = [dir.to_string_lossy().into_owned()];
        // The gate's roots: guix seed entries at /gnu/store + the td-built toolchain pair
        // at /td/store (the substituted lock lines).
        let cu_c = format!("/gnu/store/{cu_h}-coreutils-9.1");
        let tc_c = format!("/td/store/{tc_h}-gcc-toolchain-tdstore");
        let gl_c = format!("/td/store/{gl_h}-glibc-2.41");
        let roots = [cu_c.clone(), tc_c.clone(), gl_c.clone()];
        // realize_drv's composition: /gnu/store is the seed dirs' canonical home; the
        // roots + the td-owned DB registrations (glibc rides bgdb in the gate) override
        // per hash; everything else keeps the seed prefix.
        let mut overrides: HashMap<String, String> = HashMap::new();
        overrides.insert(gl_h.to_string(), gl_c.clone()); // a typed recipe-output-db registration
        for r in &roots {
            overrides.insert(store::hash_from_store_path(r).unwrap().to_string(), r.clone());
        }
        let (mut candidates, mut on_disk) = scan_candidate_index(&dirs, "/gnu/store").unwrap();
        recanonicalize_candidates(&mut candidates, &mut on_disk, &overrides);
        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        let cl = scan_closure_hybrid(&mut scanner, &on_disk, &empty, &roots).unwrap();
        let gmp_c = format!("/gnu/store/{gmp_h}-gmp-6.3.0");
        assert!(
            cl.contains(&gmp_c),
            "transitive runtime dep gmp dropped from the closure (#292): {cl:?}"
        );
        // The toolchain's byte-scanned glibc ref must resolve to the td-built glibc at its
        // TRUE /td/store canonical — a phantom /gnu/store twin would poison the output
        // reference scan's candidate set (duplicate hash, last-in wins).
        assert!(cl.contains(&gl_c), "td-built glibc missing: {cl:?}");
        assert!(
            !cl.contains(&format!("/gnu/store/{gl_h}-glibc-2.41")),
            "td-built glibc duplicated under /gnu/store: {cl:?}"
        );
        // All four members, each at exactly its true canonical.
        assert_eq!(cl.len(), 4, "closure must be exactly the 4 true-canonical members: {cl:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
