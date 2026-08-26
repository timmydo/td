//! Host prep for the INTERACTIVE operator commands (`run`, `qemu-boot*`): fetch
//! the declared, pinned inputs a target's closure needs but no cache holds yet,
//! instead of stopping to tell the operator to run `td-feed warm sources` by hand.
//!
//! Every other build entry point sits behind a prelude that warms first
//! (`td-builder check`'s heavy_warms, which is where the gates' sources come
//! from). The operator commands have no prelude, so on a cold checkout the very
//! first `./start` reported a missing tarball as a hard error with a command to
//! type. This closes that gap and nothing else: fetching happens on the HOST,
//! before the ladder lock, and only for pins the recipes already declare and
//! `td-feed` already verifies by sha256 — the build sandboxes stay offline.
//!
//! Nothing it does can fail a BUILD. A warm that cannot happen (no network, no
//! host toolchain for the fetcher, an unfamiliar rung) is reported and skipped;
//! the callers' existing cold-input errors remain the backstop, so a tree this
//! cannot help is no worse off than before. That also keeps a build-run memo hit
//! — which needs no inputs at all — reachable on a machine whose caches are cold.
//! The explicit `warm` command is the one caller that treats the same report as
//! a failure, because warming is all it was asked to do.
use std::path::{Path, PathBuf};
use std::process::Command;

use td_recipe::types::{Recipe, SourcePin};

use crate::check_runner::{
    classify_graph_inputs, is_executable, linux_version_from_file, recipe_closure,
    source_pin_for_key, RecipeCheckRunner, RecipeNode, SeedInput,
};

/// One `td-feed warm crate`/`warm crate-local` job: the argv that populates
/// `.td-build-cache/crate-vendor/<dest>/vendor`, the locked dep closure a rust
/// rung's committed `Cargo.lock` is verified against at build time.
struct VendorJob {
    dest: String,
    args: Vec<String>,
}

/// What a target's closure declares that no cache holds yet.
struct Cold {
    /// Pinned tarballs missing from the shared sources cache.
    sources: Vec<SourcePin>,
    /// Kernel-header seed arches missing from the same cache.
    headers: Vec<&'static str>,
    /// Rust rungs whose vendored dep closure is missing.
    vendors: Vec<VendorJob>,
}

impl Cold {
    fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.headers.is_empty() && self.vendors.is_empty()
    }

    /// One operator-facing line naming what is missing. A cold checkout is
    /// missing dozens of tarballs, so the names are elided past a handful — the
    /// count is what says how much of a wait this is.
    fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.sources.is_empty() {
            let files: Vec<&str> = self.sources.iter().map(|p| p.file.as_str()).collect();
            parts.push(format!(
                "{} pinned source{} ({})",
                files.len(),
                plural(files.len()),
                elided(&files)
            ));
        }
        if !self.headers.is_empty() {
            parts.push(format!(
                "{} kernel-header seed{} ({})",
                self.headers.len(),
                plural(self.headers.len()),
                self.headers.join(" ")
            ));
        }
        if !self.vendors.is_empty() {
            let dests: Vec<&str> = self.vendors.iter().map(|v| v.dest.as_str()).collect();
            parts.push(format!(
                "{} crate closure{} ({})",
                dests.len(),
                plural(dests.len()),
                elided(&dests)
            ));
        }
        parts.join("; ")
    }
}

/// Warm whatever TARGETS' closures declare and the caches lack, on the host,
/// before the caller takes the ladder lock (a multi-minute fetch must not hold
/// it). Silent and stat-only when everything is already cached.
///
/// `Err` is what could not be warmed, and it is the CALLER's to weigh: the
/// commands that only piggy-back on this report it and build anyway, while the
/// explicit `warm` command — whose whole job is this — fails on it.
pub(crate) fn preflight(runner: &RecipeCheckRunner, targets: &[&str]) -> Result<(), String> {
    let root = runner.repo_root();
    let cold = survey(root, runner.sources_dir(), targets).map_err(|e| {
        format!(
            "could not survey the declared inputs of {}: {e}",
            targets.join(" ")
        )
    })?;
    if cold.is_empty() {
        return Ok(());
    }
    // `warm sources` has no per-pin form: it fetches every recipe-owned pin that
    // is cold, which for a small target is more than that target needs. Say so
    // rather than promise a scope the fetcher does not have.
    println!(
        "   [warm] not cached yet — {}.\n         \
         Fetching now (declared fixed-output sources, each sha256-verified against its recipe\n         \
         pin; the source fetch covers every cold recipe pin, not only this target's). Needs the\n         \
         network, and may build a toolchain first. A cached tree skips all of it.\n",
        cold.describe()
    );
    let builder = runner.control_builder_path();
    let td_net = resolve_td_net(builder).map_err(|e| format!("no td-net fetch tool ({e})"))?;

    // `warm sources` fetches every cold pin and derives BOTH kernel-header seeds
    // from the pinned linux source, so one call covers both classes.
    if !cold.sources.is_empty() || !cold.headers.is_empty() {
        run_warm(&td_net, root, builder, &[s("warm"), s("sources")]);
    }
    for job in &cold.vendors {
        run_warm(&td_net, root, builder, &job.args);
    }

    // Re-survey rather than trust the exit status: `td-feed`'s warms are
    // best-effort and report a failed pin without failing, so what is on disk
    // now is the only honest answer.
    let cold = survey(root, runner.sources_dir(), targets)
        .map_err(|e| format!("could not re-survey the declared inputs: {e}"))?;
    if !cold.is_empty() {
        return Err(format!(
            "still not cached after the fetch — {}",
            cold.describe()
        ));
    }
    println!("   [warm] every declared input is cached.\n");
    Ok(())
}

/// Classify `target`'s closure the way the ladder itself does, then keep only the
/// inputs that are not on disk. Patches and in-tree local sources are committed
/// bytes — never fetched, so never cold.
fn survey(root: &Path, sources_dir: &Path, targets: &[&str]) -> Result<Cold, String> {
    let graph = recipe_closure(targets)?;
    let mut sources: Vec<SourcePin> = Vec::new();
    let mut headers: Vec<&'static str> = Vec::new();
    for input in classify_graph_inputs(&graph)? {
        match input {
            // Stage0's tarball is pinned under its own key, like any other source.
            SeedInput::Stage0 { key } => {
                push_cold_source(&mut sources, sources_dir, source_pin_for_key(&key)?)
            }
            SeedInput::Source { pin, .. } => push_cold_source(&mut sources, sources_dir, pin),
            SeedInput::LinuxHeaders { arch, .. } => {
                if !header_seed_is_warm(sources_dir, arch)? {
                    headers.push(arch);
                }
            }
            SeedInput::Patch { .. } | SeedInput::LocalSource { .. } => {}
        }
    }
    Ok(Cold {
        sources,
        headers,
        vendors: cold_vendor_jobs(root, &graph),
    })
}

/// Record `pin` if its file is not in the cache. Deduplicated by FILE, not key:
/// `classify_graph_inputs` dedups by key, and two keys can name one tarball —
/// which would inflate the count the operator reads as "how long is this".
fn push_cold_source(cold: &mut Vec<SourcePin>, sources_dir: &Path, pin: SourcePin) {
    if sources_dir.join(&pin.file).is_file() || cold.iter().any(|held| held.file == pin.file) {
        return;
    }
    cold.push(pin);
}

/// The generated `linux-headers-<version>-<arch>.tar` seed, named exactly as the
/// intern will look for it — derived from the pinned linux source, so a pin bump
/// makes the old seed cold rather than passing a stale one off as warm.
fn header_seed_is_warm(sources_dir: &Path, arch: &str) -> Result<bool, String> {
    let pin = source_pin_for_key("linux-source")?;
    let version = linux_version_from_file(&pin.file)?;
    Ok(sources_dir
        .join(format!("linux-headers-{version}-{arch}.tar"))
        .is_file())
}

/// The warm jobs for rust rungs in the closure whose vendored dep closure is
/// missing. A rung whose warm command cannot be derived is reported and left out
/// — the build's own vendor error is more precise than a guess would be.
fn cold_vendor_jobs(root: &Path, graph: &[RecipeNode]) -> Vec<VendorJob> {
    let mut jobs = Vec::new();
    for node in graph {
        if node.recipe.cargo_lock.is_none() || vendor_is_warm(root, &node.stem) {
            continue;
        }
        match vendor_warm_args(&node.recipe, &node.stem) {
            Ok(args) => jobs.push(VendorJob {
                dest: node.stem.clone(),
                args,
            }),
            Err(e) => eprintln!(
                "   [warm] no warm command for {}'s crate closure ({e}) — skipping it",
                node.stem
            ),
        }
    }
    jobs
}

/// td-feed's OWN completion predicate: the `.warm-complete` marker it renames
/// into `crate-vendor/<dest>/vendor` only once the whole locked closure is
/// published. Anything weaker (say, "some `.crate` is here") would read an
/// interrupted warm as done and skip the repair, leaving the build to fail the
/// vendor gate's set-equality check — the outcome this preflight exists to avoid.
fn vendor_is_warm(root: &Path, dest: &str) -> bool {
    root.join(".td-build-cache/crate-vendor")
        .join(dest)
        .join("vendor")
        .join(".warm-complete")
        .is_file()
}

/// The `td-feed` argv that vendors one rust rung's dep closure into
/// `crate-vendor/<dest>`: an in-tree crate vendors from its own directory, a
/// crates.io one from the package the recipe pins.
fn vendor_warm_args(recipe: &Recipe, dest: &str) -> Result<Vec<String>, String> {
    if let Some(rel) = &recipe.local_source {
        return Ok(vec![
            s("warm"),
            s("crate-local"),
            rel.clone(),
            dest.to_string(),
        ]);
    }
    let key = recipe
        .source_input
        .as_deref()
        .ok_or_else(|| format!("`{dest}' declares a cargoLock but no source"))?;
    let pin = source_pin_for_key(key)?;
    if pin.file.ends_with(".crate") {
        let name = crate_name_from_pin(&pin.file, &recipe.version)?;
        return Ok(vec![
            s("warm"),
            s("crate"),
            name,
            recipe.version.clone(),
            dest.to_string(),
        ]);
    }
    // Fixed-output archives are materialized from their top-level source root;
    // fail during warming if the later build has not selected its Cargo workspace.
    recipe.cargo_subdir.as_deref().ok_or_else(|| {
        format!(
            "fixed-output source archive `{}` needs an explicit cargoSubdir",
            pin.file
        )
    })?;
    let lock = recipe
        .cargo_lock
        .as_deref()
        .ok_or_else(|| format!("`{dest}' declares no committed Cargo.lock"))?;
    Ok(vec![
        s("warm"),
        s("crate-source"),
        pin.file,
        pin.sha256,
        lock.to_string(),
        dest.to_string(),
    ])
}

/// The crates.io package name behind a pinned `<name>-<version>.crate`. Taken by
/// stripping the recipe's own version rather than splitting on the last hyphen:
/// the name may itself contain one (`fd-find`), and it need not match the recipe
/// stem at all (`uutils` builds the `coreutils` crate).
fn crate_name_from_pin(file: &str, version: &str) -> Result<String, String> {
    file.strip_suffix(&format!("-{version}.crate"))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("pinned file `{file}' is not `<name>-{version}.crate'"))
}

/// Run one `td-feed` warm with the environment it needs, streaming its output so
/// a multi-minute fetch is not a silent wait. `warm sources` resolves the
/// recipe-owned pins by BUILDING an evaluator through tests/recipe-eval-tool.sh,
/// which hard-requires TD_BUILDER_SELF — without it the warm dies on the unset
/// variable before fetching anything.
fn run_warm(td_net: &Path, root: &Path, builder: &Path, args: &[String]) {
    let mut cmd = Command::new(td_net);
    cmd.arg("feed");
    for arg in args {
        cmd.arg(arg);
    }
    let status = cmd
        .current_dir(root)
        .env("TD_ROOT", root)
        .env("TD_BUILDER_SELF", builder)
        .status();
    match status {
        Ok(st) if st.success() => {}
        Ok(st) => eprintln!("   [warm] td-feed {} exited {st}", args.join(" ")),
        Err(e) => eprintln!("   [warm] could not run td-feed {}: {e}", args.join(" ")),
    }
}

/// td's own fetch multicall, resolved through `td-builder provision-net` — the
/// SAME statically linked host build the check prelude uses (`host_cargo_bin`),
/// not a second copy of it: that build pins the compiler and fails closed on a
/// dynamic result, because a control-plane binary carrying a mutable guix-home
/// runpath dies at exec time on the hosts this is meant to help. Cargo decides
/// freshness there, so an edited net/ cannot leave a stale fetcher behind.
fn resolve_td_net(builder: &Path) -> Result<PathBuf, String> {
    println!("   [warm] resolving td's fetch tool (net/); a cold tree builds it once.\n");
    // stdout is the path; stderr is INHERITED, not captured — resolving can mean
    // a minutes-long cargo build, and swallowing its progress would make the
    // coldest path the one that looks hung.
    let out = Command::new(builder)
        .arg("provision-net")
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn {} provision-net: {e}", builder.display()))?;
    if !out.status.success() {
        return Err(format!(
            "td-builder provision-net exited {} (its diagnosis is above)",
            out.status
        ));
    }
    let path = PathBuf::from(
        String::from_utf8(out.stdout)
            .map_err(|e| format!("provision-net path is not UTF-8: {e}"))?
            .trim(),
    );
    if !is_executable(&path) {
        return Err(format!(
            "td-builder provision-net named {}, which is not executable",
            path.display()
        ));
    }
    Ok(path)
}

fn s(v: &str) -> String {
    v.to_string()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The first few names, then how many more.
fn elided(names: &[&str]) -> String {
    const SHOWN: usize = 6;
    match names.len() > SHOWN {
        false => names.join(" "),
        true => match names.get(..SHOWN) {
            Some(head) => format!("{} +{} more", head.join(" "), names.len() - SHOWN),
            None => names.join(" "),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("td-warm-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // The whole point of the preflight: on a machine with nothing cached, the
    // distro target's own closure is what says which pins to fetch — including
    // the stage0 tarball whose absence is what stopped `./start`.
    #[test]
    fn cold_sources_are_surveyed_from_the_target_closure() {
        let root = scratch("cold-root");
        let sources = scratch("cold-sources");
        let cold = survey(&root, &sources, &["system-x86-64"]).unwrap();
        let files: Vec<&str> = cold.sources.iter().map(|p| p.file.as_str()).collect();
        assert!(
            files.iter().any(|f| f.starts_with("stage0-posix-")),
            "the stage0 tarball must be reported cold: {files:?}"
        );
        assert!(
            cold.headers.contains(&"x86_64"),
            "the x86_64 kernel-header seed must be reported cold: {:?}",
            cold.headers
        );
        assert!(!cold.is_empty());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&sources);
    }

    // …and the converse, which is what keeps a warm tree free of both the fetch
    // and the noise: a cache holding every declared file surveys clean.
    #[test]
    fn a_fully_cached_tree_surveys_clean() {
        let root = scratch("warm-root");
        let sources = scratch("warm-sources");
        let cold = survey(&root, &sources, &["system-x86-64"]).unwrap();
        for pin in &cold.sources {
            fs::write(sources.join(&pin.file), b"pinned bytes").unwrap();
        }
        let linux = source_pin_for_key("linux-source").unwrap();
        let version = linux_version_from_file(&linux.file).unwrap();
        for arch in &cold.headers {
            fs::write(
                sources.join(format!("linux-headers-{version}-{arch}.tar")),
                b"headers",
            )
            .unwrap();
        }
        for job in &cold.vendors {
            let vendor = root
                .join(".td-build-cache/crate-vendor")
                .join(&job.dest)
                .join("vendor");
            fs::create_dir_all(&vendor).unwrap();
            fs::write(vendor.join("dep-1.0.0.crate"), b"dep").unwrap();
        }
        // Crates alone are an INTERRUPTED warm — td-feed publishes the marker
        // last — and must still read cold, or the repair is skipped and the
        // build fails the vendor gate instead.
        let partial = survey(&root, &sources, &["system-x86-64"]).unwrap();
        assert_eq!(
            partial.vendors.len(),
            cold.vendors.len(),
            "a vendor dir with crates but no completion marker must stay cold"
        );
        for job in &cold.vendors {
            let vendor = root
                .join(".td-build-cache/crate-vendor")
                .join(&job.dest)
                .join("vendor");
            fs::write(vendor.join(".warm-complete"), b"").unwrap();
        }
        let after = survey(&root, &sources, &["system-x86-64"]).unwrap();
        assert!(
            after.is_empty(),
            "still cold after caching every declared input: {}",
            after.describe()
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&sources);
    }

    // A stale header seed must read as COLD: the name carries the pinned linux
    // version, so a pin bump cannot be satisfied by the previous version's tar.
    #[test]
    fn a_header_seed_for_another_linux_version_is_not_warm() {
        let sources = scratch("hdr");
        fs::write(sources.join("linux-headers-0.0.0-x86_64.tar"), b"stale").unwrap();
        assert!(!header_seed_is_warm(&sources, "x86_64").unwrap());
        let linux = source_pin_for_key("linux-source").unwrap();
        let version = linux_version_from_file(&linux.file).unwrap();
        fs::write(
            sources.join(format!("linux-headers-{version}-x86_64.tar")),
            b"fresh",
        )
        .unwrap();
        assert!(header_seed_is_warm(&sources, "x86_64").unwrap());
        let _ = fs::remove_dir_all(&sources);
    }

    // The crates.io name is not the recipe stem and not "everything before the
    // last hyphen" either; both of td's vendoring shapes have to come out right.
    #[test]
    fn crate_names_come_from_the_pin_minus_the_recipe_version() {
        assert_eq!(
            crate_name_from_pin("fd-find-10.2.0.crate", "10.2.0").unwrap(),
            "fd-find"
        );
        assert_eq!(
            crate_name_from_pin("coreutils-0.9.0.crate", "0.9.0").unwrap(),
            "coreutils"
        );
        assert!(crate_name_from_pin("coreutils-0.9.0.crate", "0.9.1").is_err());
        assert!(crate_name_from_pin("linux-6.6.tar.xz", "6.6").is_err());
        assert!(crate_name_from_pin("-1.0.0.crate", "1.0.0").is_err());
    }

    #[test]
    fn malformed_crate_pin_is_not_reinterpreted_as_a_workspace_archive() {
        let mut recipe = td_recipe::catalog::lookup("uutils").unwrap();
        recipe.version = "wrong-version".into();
        let error = vendor_warm_args(&recipe, "uutils").unwrap_err();
        assert!(error.contains("is not `<name>-wrong-version.crate'"), "{error}");
    }

    // The vendor warms the distro's closure needs, derived from the recipes
    // rather than a hand-kept list: a crates.io rung and the in-tree one.
    #[test]
    fn vendor_jobs_cover_both_crate_and_local_source_rungs() {
        let root = scratch("vendor-root");
        let graph = recipe_closure(&["system-x86-64"]).unwrap();
        let jobs = cold_vendor_jobs(&root, &graph);
        let uutils = jobs
            .iter()
            .find(|j| j.dest == "uutils")
            .expect("the distro's uutils rung vendors from a committed lock");
        // Read the version off the recipe rather than writing it down again: a
        // pin bump must not red a test about where the NAME comes from.
        let version = td_recipe::catalog::lookup("uutils").unwrap().version;
        assert_eq!(
            uutils.args,
            vec!["warm", "crate", "coreutils", &version, "uutils"]
        );
        if let Some(sshd) = jobs.iter().find(|j| j.dest == "sshd") {
            assert_eq!(
                sshd.args,
                vec!["warm", "crate-local", "tests/sshd", "sshd"],
                "an in-tree crate vendors from its directory"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fixed_output_workspace_archives_warm_from_the_committed_lock() {
        let codex = td_recipe::catalog::lookup("codex").unwrap();
        assert_eq!(
            vendor_warm_args(&codex, "codex").unwrap(),
            vec![
                "warm",
                "crate-source",
                "codex-rust-v0.148.0.tar.gz",
                "a45e90403eb36b7d6093b167fe1c7dba9b36063bef6d39359eed52c47a21f94a",
                "recipes/locks/codex/Cargo.lock",
                "codex",
            ]
        );
    }
}
