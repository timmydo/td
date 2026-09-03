//! Host prep for the INTERACTIVE operator commands (`run`, `qemu-boot*`): fetch
//! missing declared inputs and reauthenticate a complete-looking exact OSTree
//! graph until recipe admission records it, instead of stopping to tell the
//! operator to run a warm command by hand.
//!
//! Every other build entry point sits behind a prelude that warms first
//! (`td-builder check`'s heavy_warms, which is where the gates' sources come
//! from). The operator commands have no prelude, so on a cold checkout the very
//! first `./start` reported a missing tarball as a hard error with a command to
//! type. Fetching and offline OSTree authentication happen on the HOST, before
//! the ladder lock, and only for pins the recipes already declare and `td-feed`
//! authenticates by compiled checksums. A marker-complete but unadmitted graph
//! therefore reaches td-feed's repair path; the build sandboxes stay offline.
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

use td_recipe::catalog;
use td_recipe::types::{OstreePin, Recipe, SourcePin};

use crate::check_runner::{
    classify_graph_inputs, is_executable, linux_version_from_file, recipe_closure,
    ostree_cache_is_warm, source_pin_for_key, RecipeCheckRunner, RecipeNode, SeedInput,
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
    /// Exact reviewed deploy graphs missing from the shared OSTree cache.
    ostree: Vec<OstreePin>,
    /// Kernel-header seed arches missing from the same cache.
    headers: Vec<&'static str>,
    /// Rust rungs whose vendored dep closure is missing.
    vendors: Vec<VendorJob>,
}

impl Cold {
    fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && self.ostree.is_empty()
            && self.headers.is_empty()
            && self.vendors.is_empty()
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
        if !self.ostree.is_empty() {
            let keys: Vec<&str> = self.ostree.iter().map(|pin| pin.key.as_str()).collect();
            parts.push(format!(
                "{} exact OSTree graph{} ({})",
                keys.len(),
                plural(keys.len()),
                elided(&keys)
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

/// Warm missing declared inputs on the host before the caller takes the ladder
/// lock (a multi-minute fetch must not hold it). Explicit mode also
/// reauthenticates every selected OSTree graph; automatic mode does so for a
/// complete-looking graph until exact recipe admission records it.
///
/// `Err` is what could not be warmed, and it is the CALLER's to weigh: the
/// commands that only piggy-back on this report it and build anyway, while the
/// explicit `warm` command — whose whole job is this — fails on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WarmMode {
    Explicit,
    Automatic,
}

impl WarmMode {
    fn verifies_all_ostree(self) -> bool {
        matches!(self, WarmMode::Explicit)
    }
}

pub(crate) fn preflight(
    runner: &RecipeCheckRunner,
    targets: &[&str],
    mode: WarmMode,
) -> Result<(), String> {
    let root = runner.repo_root();
    let cold = survey(root, runner.sources_dir(), runner.ostree_dir(), targets).map_err(|e| {
        format!(
            "could not survey the declared inputs of {}: {e}",
            targets.join(" ")
        )
    })?;
    let graph = recipe_closure(targets)?;
    let mut ostree_jobs = cold.ostree.clone();
    for input in classify_graph_inputs(&graph)? {
        let SeedInput::Ostree { pin, .. } = input else {
            continue;
        };
        let cache = runner.ostree_dir().join(&pin.cache);
        let admitted = runner.ostree_pin_is_admitted(&pin)?;
        if should_verify_ostree(&cache, &pin, mode.verifies_all_ostree(), admitted)
            && !ostree_jobs.iter().any(|held| held.key == pin.key)
        {
            ostree_jobs.push(pin);
        }
    }
    if cold.is_empty() && ostree_jobs.is_empty() {
        return Ok(());
    }
    // `warm sources` has no per-pin form: it fetches every recipe-owned pin that
    // is cold, which for a small target is more than that target needs. Say so
    // rather than promise a scope the fetcher does not have.
    if !cold.is_empty() {
        println!(
            "   [warm] not cached yet — {}.\n         \
             Fetching now (declared source archives use their compiled SHA-256 pins; exact\n         \
             OSTree graphs use their commit/content checksums). The source fetch covers every\n         \
             cold recipe pin, not only this target. Needs the network, and may build a toolchain\n         \
             first. Cached archives skip their fetch; explicit OSTree warm still performs the\n         \
             bounded offline authentication pass.\n",
            cold.describe()
        );
    } else {
        println!(
            "   [warm] reauthenticating {} exact OSTree graph{} offline.\n",
            ostree_jobs.len(),
            plural(ostree_jobs.len())
        );
    }
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
    let mut failed_ostree = Vec::new();
    for pin in &ostree_jobs {
        let destination = runner.ostree_dir().join(&pin.cache);
        let args = ostree_warm_args(pin, &destination)?;
        if !run_warm(&td_net, root, builder, &args) {
            failed_ostree.push(pin.key.clone());
        }
    }

    // Re-survey rather than trust the exit status: `td-feed`'s warms are
    // best-effort and report a failed pin without failing, so what is on disk
    // now is the only honest answer.
    let cold = survey(root, runner.sources_dir(), runner.ostree_dir(), targets)
        .map_err(|e| format!("could not re-survey the declared inputs: {e}"))?;
    if !cold.is_empty() {
        return Err(format!(
            "still not cached after the fetch — {}",
            cold.describe()
        ));
    }
    if !failed_ostree.is_empty() {
        return Err(format!(
            "exact OSTree graph verification or repair failed ({})",
            failed_ostree.join(" ")
        ));
    }
    println!("   [warm] every declared input is cached.\n");
    Ok(())
}

fn should_verify_ostree(
    cache: &Path,
    pin: &OstreePin,
    verify_all: bool,
    admitted: bool,
) -> bool {
    verify_all || (ostree_cache_is_warm(cache, pin) && !admitted)
}

/// Classify `target`'s closure the way the ladder itself does, then keep only the
/// inputs that are not on disk. Patches and in-tree local sources are committed
/// bytes — never fetched, so never cold.
fn survey(
    root: &Path,
    sources_dir: &Path,
    ostree_dir: &Path,
    targets: &[&str],
) -> Result<Cold, String> {
    let graph = recipe_closure(targets)?;
    let mut sources: Vec<SourcePin> = Vec::new();
    let mut ostree: Vec<OstreePin> = Vec::new();
    let mut headers: Vec<&'static str> = Vec::new();
    for input in classify_graph_inputs(&graph)? {
        match input {
            // Stage0's tarball is pinned under its own key, like any other source.
            SeedInput::Stage0 { key } => {
                push_cold_source(&mut sources, sources_dir, source_pin_for_key(&key)?)
            }
            SeedInput::Source { pin, .. } => push_cold_source(&mut sources, sources_dir, pin),
            SeedInput::Ostree { pin, .. } => {
                let cache = ostree_dir.join(&pin.cache);
                if !ostree_cache_is_warm(&cache, &pin) {
                    ostree.push(pin);
                }
            }
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
        ostree,
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
        // A rung with no committed lock declares no crate closure at all. Every
        // non-Rust rung in the graph is one, so without this the survey asks
        // `vendor_warm_plan` about zlib and gcc and reports ~55 "no warm
        // command" lines per run — and a `.crate`-pinned rung without a
        // cargoLock would yield a real job that can never be satisfied.
        if node.recipe.cargo_lock.is_none() {
            continue;
        }
        match vendor_warm_plan(&node.recipe, &node.stem) {
            Ok((args, rel)) => {
                let lock = root.join(rel);
                if vendor_is_warm(root, &node.stem, &lock) {
                    continue;
                }
                jobs.push(VendorJob {
                    dest: node.stem.clone(),
                    args,
                });
            }
            Err(e) => eprintln!(
                "   [warm] no warm command for {}'s crate closure ({e}) — skipping it",
                node.stem
            ),
        }
    }
    jobs
}

/// Every `td-feed warm` argv that vendors a rust rung's locked dep closure in
/// `target`'s graph, one TAB-joined argv per line (no field can contain a tab:
/// they are pin basenames, hashes, repo-relative paths and recipe stems).
///
/// The derivation lives here because the recipe metadata does. `td-builder`
/// does not depend on the catalog, so a warm it drove itself had to restate a
/// rung's source pin, committed lock and stem -- fields it cannot keep honest,
/// and whose drift nothing would notice until a recipe check failed for an
/// apparently unrelated reason.
///
/// Unlike `cold_vendor_jobs` this reports EVERY rung with a lock, warm or not:
/// td-feed skips a complete vendor itself, so a caller that decides nothing
/// cannot disagree with what the recipes declare.
pub fn vendor_warm_args_cli(args: &[String]) -> Result<(), String> {
    const STEM: &str = "system-x86-64";
    // Arity before the stem lookup: `vendor-warm-args a b` is a usage error, not
    // a report that `a' is an unknown recipe.
    if args.get(1).is_some() {
        return Err("usage: vendor-warm-args [TARGET]".to_string());
    }
    for line in vendor_warm_args_for(args.first().map(String::as_str).unwrap_or(STEM))? {
        println!("{line}");
    }
    Ok(())
}

/// The lines `vendor_warm_args_cli` prints, so the derivation is testable
/// without capturing stdout -- a CLI whose body is only reachable through
/// `println!` can be gutted to `Ok(())` with the suite still green.
fn vendor_warm_args_for(stem: &str) -> Result<Vec<String>, String> {
    if catalog::lookup(stem).is_none() {
        return Err(format!("unknown recipe stem '{stem}' (try `list`)"));
    }
    Ok(vendor_warm_lines(&recipe_closure(&[stem])?))
}

/// The printable form, split out so a test can assert the derivation without
/// capturing stdout.
fn vendor_warm_lines(graph: &[RecipeNode]) -> Vec<String> {
    let mut lines = Vec::new();
    for node in graph {
        if node.recipe.cargo_lock.is_none() {
            continue;
        }
        match vendor_warm_plan(&node.recipe, &node.stem) {
            // The lock LAST, after the stem, so a reader that wants only the
            // argv drops one field and one that must judge completeness has the
            // file td-feed will stamp — rather than deriving a second opinion.
            Ok((args, lock)) => lines.push(format!(
                "{}\t{}",
                args.join("\t"),
                lock.display()
            )),
            Err(e) => eprintln!(
                "   [warm] no warm command for {}'s crate closure ({e}) -- skipping it",
                node.stem
            ),
        }
    }
    lines
}

/// td-feed's OWN completion predicate: the `.warm-complete` marker it renames
/// into `crate-vendor/<dest>/vendor` only once the whole locked closure is
/// published, AND the lock digest that marker carries. Anything weaker (say,
/// "some `.crate` is here", or the marker's mere presence) would read an
/// interrupted or SUPERSEDED warm as done and skip the repair, leaving the
/// build to fail the vendor gate's set-equality check — the outcome this
/// preflight exists to avoid.
///
/// The digest half is why this must move with td-feed: the marker is only
/// meaningful for the lock it was published from, and a preflight that skipped
/// on presence alone would decline to repair exactly the bumped rung td-feed
/// would have re-warmed.
fn vendor_is_warm(root: &Path, dest: &str, lock: &Path) -> bool {
    let Ok(marked) = std::fs::read_to_string(
        root.join(".td-build-cache/crate-vendor")
            .join(dest)
            .join("vendor")
            .join(".warm-complete"),
    ) else {
        return false;
    };
    let Ok(want) = td_engine::sha256::sha256_file(lock) else {
        return false;
    };
    marked.lines().next().map(str::trim) == Some(want.as_str())
}

/// The `td-feed` argv that vendors one rust rung's dep closure into
/// `crate-vendor/<dest>`: an in-tree crate vendors from its own directory, a
/// crates.io one from the package the recipe pins.
/// The argv AND the lock td-feed will hash into the completion marker for it.
///
/// Derived together, deliberately. Each `warm` form pins its closure with a
/// DIFFERENT lock — `crate-local` with the in-tree directory's, `crate-source`
/// with the committed one the recipe names, and `crate` with the lock SHIPPED
/// inside the fetched package — and a preflight that guessed differently would
/// compute a digest td-feed never wrote. It would then ask for a repair on
/// every run while td-feed reported `already warm` and skipped: a standoff that
/// resolves only when someone deletes the vendor dir by hand. Today all three
/// crates.io rungs happen to ship a lock byte-identical to their committed
/// copy, which is exactly the sort of coincidence that stops being true
/// quietly (review finding).
fn vendor_warm_plan(recipe: &Recipe, dest: &str) -> Result<(Vec<String>, PathBuf), String> {
    if let Some(rel) = &recipe.local_source {
        return Ok((
            vec![s("warm"), s("crate-local"), rel.clone(), dest.to_string()],
            PathBuf::from(rel).join("Cargo.lock"),
        ));
    }
    let key = recipe
        .source_input
        .as_deref()
        .ok_or_else(|| format!("`{dest}' declares a cargoLock but no source"))?;
    let pin = source_pin_for_key(key)?;
    if pin.file.ends_with(".crate") {
        let name = crate_name_from_pin(&pin.file, &recipe.version)?;
        // `warm crate` fetches the package and pins the closure with the lock
        // that package SHIPS, so that is the file whose digest it stamps.
        let shipped = PathBuf::from(".td-build-cache/crate-vendor")
            .join(dest)
            .join("src")
            .join(format!("{name}-{}", recipe.version))
            .join("Cargo.lock");
        return Ok((
            vec![
                s("warm"),
                s("crate"),
                name,
                recipe.version.clone(),
                dest.to_string(),
            ],
            shipped,
        ));
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
    Ok((
        vec![
            s("warm"),
            s("crate-source"),
            pin.file,
            pin.sha256,
            lock.to_string(),
            dest.to_string(),
        ],
        PathBuf::from(lock),
    ))
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
fn ostree_warm_args(pin: &OstreePin, destination: &Path) -> Result<Vec<String>, String> {
    let destination = destination
        .to_str()
        .ok_or_else(|| {
            format!(
                "OSTree cache destination is not UTF-8: {}",
                destination.display()
            )
        })?;
    Ok(vec![
        s("warm"),
        s("ostree"),
        pin.repository.clone(),
        pin.exact_ref.clone(),
        pin.commit.clone(),
        pin.content.clone(),
        destination.to_string(),
    ])
}

fn run_warm(td_net: &Path, root: &Path, builder: &Path, args: &[String]) -> bool {
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
        Ok(st) if st.success() => true,
        Ok(st) => {
            eprintln!("   [warm] td-feed {} exited {st}", args.join(" "));
            false
        }
        Err(e) => {
            eprintln!("   [warm] could not run td-feed {}: {e}", args.join(" "));
            false
        }
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
        let cold = survey(&root, &sources, &root.join("ostree"), &["system-x86-64"]).unwrap();
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

    #[test]
    fn firefox_surveys_and_completes_its_exact_graph_cache() {
        let root = scratch("firefox-root");
        let sources = scratch("firefox-sources");
        let ostree = root.join("ostree");
        let cold = survey(&root, &sources, &ostree, &["firefox"]).unwrap();
        assert!(cold.sources.is_empty());
        assert_eq!(cold.ostree.len(), 2);
        let pin = cold
            .ostree
            .iter()
            .find(|pin| pin.key == "firefox-154-source")
            .expect("the Firefox pin is cold");
        assert_eq!(pin.key, "firefox-154-source");

        let cache = ostree.join(&pin.cache);
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("graph.v1"), b"authenticated graph").unwrap();
        assert_eq!(
            survey(&root, &sources, &ostree, &["firefox"])
                .unwrap()
                .ostree
                .len(),
            2,
            "a graph without the exact owner record is an interrupted warm"
        );
        fs::write(cache.join("td-ostree-cache.v1"), b"wrong owner").unwrap();
        assert_eq!(
            survey(&root, &sources, &ostree, &["firefox"])
                .unwrap()
                .ostree
                .len(),
            2,
            "a marker for another graph must not suppress warming"
        );
        fs::write(
            cache.join("td-ostree-cache.v1"),
            format!(
                "format=1\nrepository={}\nref={}\ncommit={}\ncontent={}\n",
                pin.repository, pin.exact_ref, pin.commit, pin.content
            ),
        )
        .unwrap();
        let platform = cold
            .ostree
            .iter()
            .find(|candidate| candidate.key == "freedesktop-platform-25-08-source")
            .expect("the platform pin is cold");
        let platform_cache = ostree.join(&platform.cache);
        fs::create_dir_all(&platform_cache).unwrap();
        fs::write(platform_cache.join("graph.v1"), b"authenticated graph").unwrap();
        fs::write(
            platform_cache.join("td-ostree-cache.v1"),
            format!(
                "format=1\nrepository={}\nref={}\ncommit={}\ncontent={}\n",
                platform.repository, platform.exact_ref, platform.commit, platform.content
            ),
        )
        .unwrap();
        let warm = survey(&root, &sources, &ostree, &["firefox"]).unwrap();
        assert!(warm.is_empty(), "completed exact graph stayed cold: {}", warm.describe());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&sources);
    }

    #[test]
    fn marker_complete_unadmitted_cache_is_selected_for_offline_repair() {
        let root = scratch("ostree-repair");
        let pin = td_recipe::ostree_pins::by_key("firefox-154-source")
            .expect("reviewed Firefox pin");
        fs::create_dir_all(root.join("objects/00")).unwrap();
        fs::write(root.join("graph.v1"), b"manifest naming a corrupt object").unwrap();
        fs::write(root.join("objects/00/corrupt.filez"), b"corrupt").unwrap();
        fs::write(
            root.join("td-ostree-cache.v1"),
            format!(
                "format=1\nrepository={}\nref={}\ncommit={}\ncontent={}\n",
                pin.repository, pin.exact_ref, pin.commit, pin.content
            ),
        )
        .unwrap();

        assert!(should_verify_ostree(&root, &pin, false, false));
        assert!(!should_verify_ostree(&root, &pin, false, true));
        assert!(should_verify_ostree(&root, &pin, true, true));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_warm_cli_wires_full_graph_verification() {
        assert!(WarmMode::Explicit.verifies_all_ostree());
        assert!(!WarmMode::Automatic.verifies_all_ostree());
        let runner = include_str!("check_runner.rs");
        let wiring =
            "crate::warm::preflight(&runner, &targets, crate::warm::WarmMode::Explicit)";
        assert_eq!(runner.matches(wiring).count(), 1);
    }

    #[test]
    fn ostree_warm_argv_refuses_lossy_cache_paths() {
        use std::os::unix::ffi::OsStringExt;

        let pin = td_recipe::ostree_pins::by_key("firefox-154-source")
            .expect("reviewed Firefox pin");
        let args = ostree_warm_args(&pin, Path::new("/cache/firefox")).unwrap();
        assert_eq!(
            args,
            vec![
                "warm",
                "ostree",
                pin.repository.as_str(),
                pin.exact_ref.as_str(),
                pin.commit.as_str(),
                pin.content.as_str(),
                "/cache/firefox",
            ]
        );
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
        assert!(ostree_warm_args(&pin, &invalid).is_err());
    }

    // …and the converse, which is what keeps a warm tree free of both the fetch
    // and the noise: a cache holding every declared file surveys clean.
    #[test]
    fn a_fully_cached_tree_surveys_clean() {
        let root = scratch("warm-root");
        let sources = scratch("warm-sources");
        let cold = survey(&root, &sources, &root.join("ostree"), &["system-x86-64"]).unwrap();
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
        for pin in &cold.ostree {
            let cache = root.join("ostree").join(&pin.cache);
            fs::create_dir_all(&cache).unwrap();
            fs::write(cache.join("graph.v1"), b"authenticated graph").unwrap();
            fs::write(
                cache.join("td-ostree-cache.v1"),
                format!(
                    "format=1\nrepository={}\nref={}\ncommit={}\ncontent={}\n",
                    pin.repository, pin.exact_ref, pin.commit, pin.content
                ),
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
        let partial = survey(
            &root,
            &sources,
            &root.join("ostree"),
            &["system-x86-64"],
        )
        .unwrap();
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
            // As td-feed publishes it: the digest of the lock this set is for.
            // An empty or count-only marker is what the pre-digest scheme wrote
            // and is deliberately no longer enough.
            // Ask the derivation which lock td-feed would stamp for this rung,
            // rather than writing one the preflight would never look at.
            let recipe = td_recipe::catalog::lookup(&job.dest).unwrap();
            let (_, rel) = vendor_warm_plan(&recipe, &job.dest).unwrap();
            let lock = root.join(rel);
            if let Some(parent) = lock.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&lock, format!("[[package]]\nname = \"{}\"\n", job.dest)).unwrap();
            let digest = td_engine::sha256::sha256_file(&lock).unwrap();
            fs::write(vendor.join(".warm-complete"), format!("{digest}\n1\n")).unwrap();
        }
        let after = survey(
            &root,
            &sources,
            &root.join("ostree"),
            &["system-x86-64"],
        )
        .unwrap();
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
        let error = vendor_warm_plan(&recipe, "uutils").unwrap_err();
        assert!(error.contains("is not `<name>-wrong-version.crate'"), "{error}");
    }

    // The vendor warms the distro's closure needs from recipe metadata rather
    // than from a hand-kept package list.
    #[test]
    fn vendor_jobs_cover_the_committed_lock_rungs() {
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
        let _ = fs::remove_dir_all(&root);
    }

    /// The default target's graph derives a vendor warm for every rung that
    /// declares a committed lock -- codex included.
    ///
    /// This is the property `td-builder check`'s prelude now depends on: it asks
    /// for this list rather than restating it, so a rung that falls out of the
    /// derivation silently stops being warmed, and `provision_auto_vendor`
    /// reports it much later as a vendor dir that is simply absent.
    #[test]
    fn the_default_target_derives_a_vendor_warm_for_every_locked_rung() {
        let graph = recipe_closure(&["system-x86-64"]).unwrap();
        let locked: Vec<&str> = graph
            .iter()
            .filter(|node| node.recipe.cargo_lock.is_some())
            .map(|node| node.stem.as_str())
            .collect();
        assert!(
            locked.contains(&"codex"),
            "codex declares a committed lock but is not in the default graph: {locked:?}"
        );
        let lines = vendor_warm_lines(&graph);
        for stem in &locked {
            assert!(
                lines.iter().any(|line| {
                    let mut fields = line.split('\t').rev();
                    let _lock = fields.next();
                    fields.next() == Some(stem)
                }),
                "no vendor warm derived for `{stem}': {lines:?}"
            );
        }
        assert_eq!(
            lines.len(),
            locked.len(),
            "every locked rung derives exactly one warm"
        );
    }

    /// No derived field may contain the separator. Asserted BEFORE the join,
    /// which is the only place it is still visible -- a tab inside a field
    /// would silently shift every later field and make `vendor_dest` name the
    /// wrong recipe.
    #[test]
    fn no_derived_vendor_warm_field_contains_the_separator() {
        let graph = recipe_closure(&["system-x86-64"]).unwrap();
        let mut seen = 0;
        for node in &graph {
            if node.recipe.cargo_lock.is_none() {
                continue;
            }
            let (args, _) = vendor_warm_plan(&node.recipe, &node.stem).unwrap();
            for field in &args {
                assert!(
                    !field.contains('\t') && !field.contains('\n'),
                    "{}: field `{field}' would break the line format",
                    node.stem
                );
            }
            seen += 1;
        }
        assert!(seen > 0, "no locked rung to check");
    }

    /// The entry point, not just the derivation behind it.
    #[test]
    fn the_vendor_warm_args_verb_checks_its_arguments_and_derives_lines() {
        let lines = vendor_warm_args_for("system-x86-64").unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\tcodex\t") && line.ends_with("Cargo.lock")),
            "the default target derives no codex warm: {lines:?}"
        );
        assert!(vendor_warm_args_for("no-such-recipe-stem").is_err());
        let two = [s("system-x86-64"), s("extra")];
        assert!(vendor_warm_args_cli(&two).is_err(), "arity is unchecked");
    }

    /// The line format the prelude parses: TAB-joined, `warm` first, then the
    /// recipe stem, then the lock td-feed pins that rung with. The lock is last
    /// so a reader that wants only the argv drops one field.
    #[test]
    fn a_derived_vendor_warm_line_is_tab_joined_with_the_stem_then_the_lock() {
        let graph = recipe_closure(&["system-x86-64"]).unwrap();
        let lines = vendor_warm_lines(&graph);
        assert!(!lines.is_empty(), "the default graph derives no vendor warm");
        for line in &lines {
            let fields: Vec<&str> = line.split('\t').collect();
            assert!(
                fields.len() >= 4,
                "`{line}' is too short to name a warm, a dest and a lock"
            );
            assert_eq!(fields.first(), Some(&"warm"), "`{line}' is not a warm argv");
            assert!(
                fields.last().is_some_and(|f| f.ends_with("Cargo.lock")),
                "`{line}' does not end in the lock it is judged against"
            );
            assert!(
                !line.contains('\n'),
                "`{line}' would not survive line-based parsing"
            );
            for field in &fields {
                assert!(!field.is_empty(), "`{line}' has an empty field");
            }
        }
    }

    /// The preflight's skip must agree with td-feed's own predicate, or it
    /// declines to repair exactly the rung td-feed would have re-warmed.
    ///
    /// Both read the digest the marker carries. A marker for a superseded lock
    /// — or one written by the pre-digest scheme, which carries a count where
    /// the digest belongs — is not warm on either side.
    #[test]
    fn a_vendor_marked_for_another_lock_is_not_warm() {
        let root = scratch("vendor-marker");
        let vendor = root.join(".td-build-cache/crate-vendor/x/vendor");
        fs::create_dir_all(&vendor).unwrap();
        let lock = root.join("Cargo.lock");
        fs::write(&lock, "[[package]]\nname = \"a\"\n").unwrap();

        assert!(!vendor_is_warm(&root, "x", &lock), "no marker is not warm");

        let digest = td_engine::sha256::sha256_file(&lock).unwrap();
        fs::write(vendor.join(".warm-complete"), format!("{digest}\n7\n")).unwrap();
        assert!(vendor_is_warm(&root, "x", &lock), "its own lock is warm");

        fs::write(&lock, "[[package]]\nname = \"a\"\nversion = \"2\"\n").unwrap();
        assert!(
            !vendor_is_warm(&root, "x", &lock),
            "a bumped lock must be repaired, not skipped"
        );

        fs::write(vendor.join(".warm-complete"), "1189\n").unwrap();
        assert!(
            !vendor_is_warm(&root, "x", &lock),
            "a pre-digest marker must be repaired, not skipped"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Each warm form is judged against the lock td-feed actually pins it with.
    ///
    /// This is the derivation, not the comparison: handing a predicate a lock
    /// proves nothing about which lock it would have chosen. `warm crate`
    /// fetches a package and uses the lock that package SHIPS, so a preflight
    /// that reached for the committed copy instead would compute a digest
    /// td-feed never wrote — asking for a repair every run while td-feed
    /// reported `already warm`. The two files are byte-identical for every
    /// crates.io rung today, which is exactly why nothing would notice.
    #[test]
    fn each_warm_form_is_judged_against_the_lock_td_feed_pins_it_with() {
        let uutils = td_recipe::catalog::lookup("uutils").unwrap();
        let (args, lock) = vendor_warm_plan(&uutils, "uutils").unwrap();
        assert_eq!(args.get(1).map(String::as_str), Some("crate"));
        assert_eq!(
            lock,
            PathBuf::from(".td-build-cache/crate-vendor/uutils/src")
                .join(format!("coreutils-{}", uutils.version))
                .join("Cargo.lock"),
            "a crates.io rung is pinned by the lock its package ships"
        );

        let codex = td_recipe::catalog::lookup("codex").unwrap();
        let (args, lock) = vendor_warm_plan(&codex, "codex").unwrap();
        assert_eq!(args.get(1).map(String::as_str), Some("crate-source"));
        assert_eq!(
            lock,
            PathBuf::from("recipes/locks/codex/Cargo.lock"),
            "an archive rung is pinned by its committed lock"
        );
        // The argv td-feed receives names that same lock, so the two cannot be
        // about different files.
        assert_eq!(args.get(4).map(String::as_str), lock.to_str());
    }

    #[test]
    fn fixed_output_workspace_archives_warm_from_the_committed_lock() {
        let codex = td_recipe::catalog::lookup("codex").unwrap();
        assert_eq!(
            vendor_warm_plan(&codex, "codex").unwrap().0,
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
