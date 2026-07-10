use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use td_recipe::{
    catalog, source_pins,
    types::{CheckRunner, Recipe, SourcePin},
};

pub(crate) const TD_STORE_DIR: &str = "/td/store";

/// The stable in-crate marker for a planning-time provenance rejection.
/// `main` maps an error carrying it to exit 69 (EX_UNAVAILABLE — the same
/// "nothing can run here" signal td-builder's loop uses for
/// EXIT_UNPROVISIONED): the graph is structurally unbuildable on EVERY host
/// until the rejected input exists as a td recipe output — a bootstrap gap,
/// not a code regression. The cross-process contract is the exit code; this
/// prose never crosses a process boundary as an interface.
pub(crate) const PROVENANCE_REJECTED: &str = "provenance rejected: ";

/// A graph input with no admissible provenance (issue #469): not a recipe
/// output, not a pinned seed source. Names the recipe and the input so the
/// gap is actionable — the fix is always "build it as a rung", never "point
/// at a host path".
fn provenance_rejection(stem: &str, input: &str) -> String {
    format!(
        "{PROVENANCE_REJECTED}recipe {stem}: input `{input}' is neither a td recipe \
         output (catalog) nor a pinned seed source/patch. Host executables are not \
         admissible bootstrap inputs (re #469) — the chain must build `{input}' as a \
         recipe output before anything can declare it."
    )
}

pub fn cli(args: &[String]) -> Result<(), String> {
    let stem = args.first().ok_or_else(usage)?.as_str();
    let scope = args.get(1).map(String::as_str).unwrap_or("daily");
    let index = parse_index(args.get(2))?;
    if args.get(3).is_some() {
        return Err(usage());
    }
    let check_runner = selected_check_runner(stem, scope, index)?;
    // Provenance planning FIRST — before the runner exists, so a rejected
    // graph spawns no subprocess at all (re #469).
    ensure_targets_provenance(&[stem])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("check", &[stem, scope, &index.to_string()]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    crate::checks::run(check_runner, &runner, stem)
}

pub fn build_cli(args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or_else(build_usage)?.as_str();
    if catalog::lookup(target).is_none() {
        return Err(format!("unknown recipe stem '{target}' (try `list`)"));
    }
    let outputs: Vec<&str> = if args.get(1).is_some() {
        args.iter().skip(1).map(String::as_str).collect()
    } else {
        vec![target]
    };
    for output in &outputs {
        if catalog::lookup(output).is_none() {
            return Err(format!(
                "unknown output recipe stem '{output}' (try `list`)"
            ));
        }
    }

    // Provenance planning FIRST — before the runner exists, so a rejected
    // graph spawns no subprocess at all (re #469).
    ensure_targets_provenance(&[target])?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let scratch_name = scratch_name("build", &[target]);
    let runner = RecipeCheckRunner::new(root, &scratch_name)?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    runner.build_recipe_target(target, &outputs)
}

fn usage() -> String {
    "usage: check-run STEM [pr|daily|all] [INDEX]".to_string()
}

fn build_usage() -> String {
    "usage: build-run TARGET [OUTPUT_STEM ...]".to_string()
}

fn parse_index(arg: Option<&String>) -> Result<usize, String> {
    match arg {
        Some(s) => {
            let n = s
                .parse::<usize>()
                .map_err(|_| format!("check index '{s}' is not a positive integer"))?;
            if n == 0 {
                return Err("check index must be 1-based".to_string());
            }
            Ok(n)
        }
        None => Ok(1),
    }
}

fn parse_tier(arg: &str) -> Result<Option<td_recipe::types::CheckTier>, String> {
    match arg {
        "all" => Ok(None),
        "pr" => Ok(Some(td_recipe::types::CheckTier::Pr)),
        "daily" => Ok(Some(td_recipe::types::CheckTier::Daily)),
        other => Err(format!(
            "unknown check tier '{other}' (expected pr|daily|all)"
        )),
    }
}

fn scratch_name(prefix: &str, parts: &[&str]) -> String {
    let mut out = sanitize_scratch_component(prefix);
    for part in parts {
        out.push('-');
        out.push_str(&sanitize_scratch_component(part));
    }
    out.push('-');
    out.push_str(&process::id().to_string());
    out
}

fn sanitize_scratch_component(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "x".to_string()
    } else {
        out
    }
}

fn selected_check_runner(stem: &str, scope: &str, index: usize) -> Result<CheckRunner, String> {
    let tier = parse_tier(scope)?;
    let recipe = catalog::lookup(stem)
        .ok_or_else(|| format!("unknown recipe stem '{stem}' (try `list`)"))?;
    let mut count = 0;
    if let Some(checks) = &recipe.checks {
        for check in checks {
            if tier.map(|t| check.tier == t).unwrap_or(true) {
                count += 1;
                if count == index {
                    return check.runner.ok_or_else(|| {
                        format!(
                            "{stem} check index {index} has no Rust check-runner implementation"
                        )
                    });
                }
            }
        }
    }
    if count == 0 {
        return Err(format!("{stem} has no checks in the requested tier"));
    }
    Err(format!(
        "{stem} has only {count} check(s) in the requested tier; index {index} is out of range"
    ))
}

fn lock_file(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open lock {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("lock {}: {e}", path.display()))?;
    Ok(file)
}

pub(crate) struct RecipeCheckRunner {
    root: PathBuf,
    tb: PathBuf,
    builder_path: String,
    builder_store: PathBuf,
    builder_db: PathBuf,
    lw: PathBuf,
    store: PathBuf,
    db: PathBuf,
    recipes: PathBuf,
    scratch: PathBuf,
    force_cold: bool,
}

struct RecipeNode {
    stem: String,
    recipe: Recipe,
}

#[derive(Debug)]
enum SeedInput {
    Stage0 { key: String },
    Source { key: String, pin: SourcePin },
    LinuxHeaders { key: String, arch: &'static str },
    Patch { key: String, patch: String },
}

impl SeedInput {
    fn key(&self) -> &str {
        match self {
            SeedInput::Stage0 { key }
            | SeedInput::Source { key, .. }
            | SeedInput::LinuxHeaders { key, .. }
            | SeedInput::Patch { key, .. } => key,
        }
    }
}

impl RecipeCheckRunner {
    fn new(root: PathBuf, scratch_name: &str) -> Result<Self, String> {
        let stage0_base = env::var_os("TD_STAGE0_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".td-build-cache/stage0"));
        let td_builder_self = find_td_builder_self(&root)?;
        let cb = place_stage0_builder(&root, &stage0_base, &td_builder_self)?;
        let cb_base = path_basename_str(&cb)?;
        let tb = stage0_base
            .join("store")
            .join(cb_base)
            .join("bin")
            .join("td-builder");
        if !is_executable(&tb) {
            return Err(format!(
                "stage0 td-builder not executable at {}",
                tb.display()
            ));
        }

        let home = env::var_os("HOME").map(PathBuf::from);
        let chain_cache = match env::var("TD_CHECK_CHAIN_CACHE") {
            Ok(v) => v,
            Err(_) => home
                .as_ref()
                .map(|h| h.join(".td/build-daemon/chain").display().to_string())
                .unwrap_or_default(),
        };
        let lw = env::var_os("TD_RECIPE_CHECK_WORK")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if chain_cache.is_empty() {
                    root.join(".td-build-cache/ladder-cold")
                } else {
                    home.map(|h| h.join(".td/build-daemon/ladder"))
                        .unwrap_or_else(|| root.join(".td-build-cache/ladder-cold"))
                }
            });
        let store = lw.join("store");
        let db = lw.join("db");
        let recipes = lw.join("recipes");
        let scratch = lw.join("scratch").join(scratch_name);
        Ok(Self {
            root,
            tb,
            builder_path: cb,
            builder_store: stage0_base.join("store"),
            builder_db: stage0_base.join("builder.db"),
            lw,
            store,
            db,
            recipes,
            scratch,
            force_cold: chain_cache.is_empty()
                && env::var_os("TD_RECIPE_CHECK_PRESERVE_WORK").is_none(),
        })
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.lw.with_extension("lock")
    }

    pub(crate) fn setup(&self) -> Result<(), String> {
        if self.force_cold {
            remove_path_if_exists(&self.lw)?;
        }
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        remove_path_if_exists(&self.scratch)?;
        fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("mkdir {}: {e}", self.scratch.display()))?;
        let pinsum = self.setup_pinsum()?;
        let setup_ok = self.lw.join("setup-ok");
        let warm = fs::read_to_string(&setup_ok)
            .map(|s| s == pinsum)
            .unwrap_or(false)
            && self.lw.join("srcs.map").is_file();
        if warm {
            return Ok(());
        }

        remove_path_if_exists(&self.store)?;
        remove_path_if_exists(&self.db)?;
        remove_path_if_exists(&self.lw.join("srcs.map"))?;
        // tools.map: nothing writes it anymore (the host-tool resolution it
        // carried is deleted, re #469) — scrub the stale pre-v8 artifact.
        remove_path_if_exists(&self.lw.join("tools.map"))?;
        remove_path_if_exists(&setup_ok)?;
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        File::create(self.lw.join("srcs.map")).map_err(|e| format!("create srcs.map: {e}"))?;
        fs::write(&setup_ok, pinsum).map_err(|e| format!("write {}: {e}", setup_ok.display()))?;
        Ok(())
    }

    // v8: the seed-tools lock (and with it every host-tool ladder input) is
    // DELETED — the pinsum keys only what the chain may consume: the pinned
    // sources and the in-repo seed patches. The bump itself wipes every ladder
    // store built with host scaffolding, so nothing tainted is grandfathered.
    fn setup_pinsum(&self) -> Result<String, String> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ladder-setup-v8\n");
        for pin in source_pins::all() {
            bytes.extend_from_slice(pin.key.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(pin.url.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(pin.sha256.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(pin.file.as_bytes());
            bytes.push(b'\n');
        }
        for file in files_with_suffix(&self.root.join("seed/patches"), ".patch")? {
            append_file_bytes(&file, &mut bytes)?;
        }
        Ok(sha256sum(&bytes))
    }

    fn intern_source(&self, intern_name: &str, pin: &SourcePin) -> Result<(), String> {
        validate_source_file_basename(pin)?;
        let file = self.root.join(".td-build-cache/sources").join(&pin.file);
        if !file.is_file() {
            return Err(format!(
                "ladder: pinned tarball not warm ({}) - run 'td-feed warm sources'",
                file.display()
            ));
        }
        verify_source_pin(&file, pin)?;
        let path = self.store_add_recursive(intern_name, &file)?;
        self.append_src_map(intern_name, &path)
    }

    fn intern_linux_headers(&self, intern_name: &str, arch: &str) -> Result<(), String> {
        let pin = source_pin_for_key("linux-source")?;
        validate_source_file_basename(&pin)?;
        let version = linux_version_from_file(&pin.file)?;
        let file = self
            .root
            .join(".td-build-cache/sources")
            .join(format!("linux-headers-{version}-{arch}.tar.gz"));
        if !file.is_file() {
            return Err(format!(
                "ladder: kernel-headers tarball not warm ({})",
                file.display()
            ));
        }
        let path = self.store_add_recursive(intern_name, &file)?;
        self.append_src_map(intern_name, &path)
    }

    fn intern_patch(&self, intern_name: &str, patch: &str) -> Result<(), String> {
        let file = self
            .root
            .join("seed")
            .join("patches")
            .join(format!("{patch}.patch"));
        if !file.is_file() {
            return Err(format!("ladder: missing {}", file.display()));
        }
        let path = self.store_add_recursive(intern_name, &file)?;
        self.append_src_map(intern_name, &path)
    }

    fn intern_stage0_source(&self, intern_name: &str) -> Result<(), String> {
        let tarball = self.stage0_source_tarball()?;
        let extract = self.scratch.join("stage0-source-extract");
        remove_path_if_exists(&extract)?;
        fs::create_dir_all(&extract).map_err(|e| format!("mkdir {}: {e}", extract.display()))?;
        let tar_s = path_str(&tarball)?;
        let extract_s = path_str(&extract)?;
        let mut cmd = self.builder_command();
        cmd.arg("tar-gz-extract").arg(tar_s).arg(extract_s);
        command_output(&mut cmd, "td-builder tar-gz-extract stage0 source")?;
        let stage0 = single_subdir_path(&extract)?;
        clean_stage0_build_dirs(&stage0)?;
        if !stage0
            .join("bootstrap-seeds/POSIX/AMD64/hex0-seed")
            .is_file()
            || !stage0.join("AMD64/mescc-tools-seed-kaem.kaem").is_file()
        {
            return Err(format!(
                "{} did not unpack to the expected stage0 source tree",
                tarball.display()
            ));
        }
        let path = self.store_add_recursive(intern_name, &stage0)?;
        self.append_src_map(intern_name, &path)
    }

    fn stage0_source_tarball(&self) -> Result<PathBuf, String> {
        let pin = source_pin_for_key("stage0-source")?;
        validate_source_file_basename(&pin)?;
        let tarball = self.root.join(".td-build-cache/sources").join(&pin.file);
        if !tarball.is_file() {
            return Err(format!(
                "ladder: pinned stage0 source not warm ({}) - run 'td-feed warm sources'",
                tarball.display()
            ));
        }
        verify_source_pin(&tarball, &pin)?;
        Ok(tarball)
    }

    fn store_add_recursive(&self, name: &str, src: &Path) -> Result<String, String> {
        let src_s = path_str(src)?;
        let store_s = path_str(&self.store)?;
        let db_s = path_str(&self.db)?;
        let mut cmd = self.builder_command();
        cmd.arg("store-add-recursive")
            .arg(name)
            .arg(src_s)
            .arg(store_s)
            .arg(db_s);
        let out = command_output(&mut cmd, &format!("store-add-recursive {name}"))?;
        out.lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("store-add-recursive {name} produced no path"))
    }

    fn append_src_map(&self, name: &str, path: &str) -> Result<(), String> {
        let map = self.lw.join("srcs.map");
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&map)
            .map_err(|e| format!("open {}: {e}", map.display()))?;
        writeln!(file, "{name} {path}").map_err(|e| format!("write {}: {e}", map.display()))
    }

    fn map_has(&self, name: &str) -> Result<bool, String> {
        Ok(self.map_value_opt(name)?.is_some())
    }

    fn map_value(&self, name: &str) -> Result<String, String> {
        self.map_value_opt(name)?
            .ok_or_else(|| format!("ladder: no map entry for `{name}'"))
    }

    fn map_value_opt(&self, name: &str) -> Result<Option<String>, String> {
        let map = self.lw.join("srcs.map");
        if !map.is_file() {
            return Ok(None);
        }
        let contents =
            fs::read_to_string(&map).map_err(|e| format!("read {}: {e}", map.display()))?;
        for line in contents.lines() {
            let mut cols = line.splitn(2, ' ');
            let key = match cols.next() {
                Some(k) => k,
                None => continue,
            };
            if key == name {
                return Ok(cols.next().map(str::trim).map(str::to_string));
            }
        }
        Ok(None)
    }

    fn stage_tdstore(&self) -> Result<(), String> {
        fs::create_dir_all(self.scratch.join("tdstore"))
            .map_err(|e| format!("mkdir tdstore: {e}"))?;
        let contents = fs::read_to_string(self.lw.join("srcs.map"))
            .map_err(|e| format!("read srcs.map: {e}"))?;
        for line in contents.lines() {
            let mut cols = line.splitn(2, ' ');
            let _name = cols.next();
            if let Some(path) = cols.next() {
                self.stage_store_path(path.trim())?;
            }
        }
        Ok(())
    }

    fn stage_store_path(&self, store_path: &str) -> Result<(), String> {
        let base = path_basename_str(store_path)?;
        let src = self.store.join(base);
        let dst = self.scratch.join("tdstore").join(base);
        if dst.exists() {
            return Ok(());
        }
        copy_tree(&src, &dst).map_err(|e| {
            format!(
                "ladder: stage {} into tdstore failed ({} -> {}): {e}",
                base,
                src.display(),
                dst.display()
            )
        })
    }

    fn emit_recipe_graph(&self, nodes: &[RecipeNode]) -> Result<(), String> {
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        for node in nodes {
            fs::write(
                self.recipes.join(format!("{}.json", node.stem)),
                node.recipe.to_json().to_canonical(),
            )
            .map_err(|e| format!("ladder: emit {}: {e}", node.stem))?;
        }
        Ok(())
    }

    pub(crate) fn prepare_recipe_target(&self, target: &str) -> Result<(), String> {
        let graph = recipe_closure(&[target])?;
        self.ensure_graph_inputs(&graph)?;
        self.emit_recipe_graph(&graph)?;
        self.stage_tdstore()
    }

    /// Classify then realize every input in the graph: `classify_graph_inputs`
    /// (the pure planning pass — see its doc for the #469 trust boundary),
    /// then intern each admitted seed into the ladder store.
    fn ensure_graph_inputs(&self, nodes: &[RecipeNode]) -> Result<(), String> {
        for input in classify_graph_inputs(nodes)? {
            self.ensure_seed_input(&input)?;
        }
        Ok(())
    }

    fn ensure_seed_input(&self, input: &SeedInput) -> Result<(), String> {
        if self.map_has(input.key())? {
            let path = self.map_value(input.key())?;
            return self.stage_store_path(&path);
        }
        match input {
            SeedInput::Stage0 { key } => self.intern_stage0_source(key)?,
            SeedInput::Source { key, pin } => self.intern_source(key, pin)?,
            SeedInput::LinuxHeaders { key, arch } => self.intern_linux_headers(key, arch)?,
            SeedInput::Patch { key, patch } => self.intern_patch(key, patch)?,
        }
        let path = self.map_value(input.key())?;
        self.stage_store_path(&path)
    }

    pub(crate) fn build_plan(&self, target: &str) -> Result<PathBuf, String> {
        // The auto map is srcs.map verbatim: every non-owned input is an
        // interned seed source. There is no tools map — a host executable is
        // not an admissible input, so build-plan's content-scan candidate dir
        // is the ladder's OWN store of interned seeds, never a host store.
        let srcs = fs::read(self.lw.join("srcs.map")).map_err(|e| format!("read srcs.map: {e}"))?;
        let auto_map = self.scratch.join("auto-map");
        let mut map =
            File::create(&auto_map).map_err(|e| format!("create {}: {e}", auto_map.display()))?;
        map.write_all(&srcs)
            .map_err(|e| format!("write {}: {e}", auto_map.display()))?;

        let home = path_str(&self.lw)?;
        let tmp = path_str(&self.lw)?;
        let builder_store = path_str(&self.builder_store)?;
        let builder_db = path_str(&self.builder_db)?;
        let recipes = path_str(&self.recipes)?;
        let auto_map_s = path_str(&auto_map)?;
        let scratch = path_str(&self.scratch)?;
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env_clear()
            .env("HOME", home)
            .env("TMPDIR", tmp)
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", builder_store)
            .env("TD_BUILDER_DB", builder_db)
            .arg("build-plan")
            .arg("--auto")
            .arg(target)
            .arg(recipes)
            .arg(auto_map_s)
            .arg(path_str(&self.store)?)
            .arg(scratch);
        let out = cmd
            .output()
            .map_err(|e| format!("spawn build-plan --auto {target}: {e}"))?;
        let out_file = self.scratch.join(format!("build-{target}.out"));
        let err_file = self.scratch.join(format!("build-{target}.err"));
        fs::write(&out_file, &out.stdout)
            .map_err(|e| format!("write {}: {e}", out_file.display()))?;
        fs::write(&err_file, &out.stderr)
            .map_err(|e| format!("write {}: {e}", err_file.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{}\nladder: build-plan --auto {target} failed",
                tail_bytes(&out.stderr, 40)
            ));
        }
        io::stdout()
            .write_all(&out.stdout)
            .map_err(|e| format!("write build-plan stdout: {e}"))?;
        Ok(out_file)
    }

    pub(crate) fn ladder_out_from(&self, build_out: &Path, rung: &str) -> Result<PathBuf, String> {
        let prefix = format!("STEP {rung} ");
        let mut got = None;
        let contents = fs::read_to_string(build_out)
            .map_err(|e| format!("read {}: {e}", build_out.display()))?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix(&prefix) {
                got = Some(rest.trim().to_string());
            }
        }
        let path = got.ok_or_else(|| format!("ladder: no STEP output recorded for {rung}"))?;
        let base = path_basename_str(&path)?;
        Ok(self.scratch.join("tdstore").join(base))
    }

    fn build_recipe_target(&self, target: &str, outputs: &[&str]) -> Result<(), String> {
        self.prepare_recipe_target(target)?;
        let build_out = self.build_plan(target)?;
        println!("TD_RECIPE_RUN_WORK {}", self.lw.display());
        println!(
            "TD_RECIPE_RUN_TDSTORE {}",
            self.scratch.join("tdstore").display()
        );
        for output in outputs {
            let path = self.ladder_out_from(&build_out, output)?;
            println!("TD_RECIPE_RUN_OUT {output} {}", path.display());
        }
        Ok(())
    }

    pub(crate) fn store_ns_output(
        &self,
        argv: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, String> {
        let store_path = self.scratch.join("tdstore");
        let store = path_str(&store_path)?;
        let mut cmd = self.builder_command();
        cmd.arg("store-ns").arg(store).arg("--");
        for arg in argv {
            cmd.arg(arg);
        }
        match stdin {
            Some(input) => command_output_with_stdin(&mut cmd, "store-ns", input),
            None => command_output(&mut cmd, "store-ns"),
        }
    }

    pub(crate) fn builder_command(&self) -> Command {
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", &self.builder_store)
            .env("TD_BUILDER_DB", &self.builder_db);
        cmd
    }
}

fn recipe_closure(targets: &[&str]) -> Result<Vec<RecipeNode>, String> {
    let mut visiting = HashSet::new();
    let mut emitted = HashSet::new();
    let mut out = Vec::new();
    for target in targets {
        visit_recipe(target, &mut visiting, &mut emitted, &mut out)?;
    }
    Ok(out)
}

fn visit_recipe(
    stem: &str,
    visiting: &mut HashSet<String>,
    emitted: &mut HashSet<String>,
    out: &mut Vec<RecipeNode>,
) -> Result<(), String> {
    if emitted.contains(stem) {
        return Ok(());
    }
    if !visiting.insert(stem.to_string()) {
        return Err(format!("ladder: cycle in recipe nativeInputs at `{stem}'"));
    }
    let recipe =
        catalog::lookup(stem).ok_or_else(|| format!("ladder: no td recipe for `{stem}'"))?;
    if let Some(native_inputs) = &recipe.native_inputs {
        for dep in native_inputs {
            if catalog::lookup(dep).is_some() {
                visit_recipe(dep, visiting, emitted, out)?;
            }
        }
    }
    if let Some(inputs) = &recipe.inputs {
        for dep in inputs {
            if catalog::lookup(dep).is_some() {
                visit_recipe(dep, visiting, emitted, out)?;
            }
        }
    }
    visiting.remove(stem);
    emitted.insert(stem.to_string());
    out.push(RecipeNode {
        stem: stem.to_string(),
        recipe,
    });
    Ok(())
}

fn push_seed_input(inputs: &mut Vec<SeedInput>, seen: &mut HashSet<String>, input: SeedInput) {
    if seen.insert(input.key().to_string()) {
        inputs.push(input);
    }
}

/// The PURE planning pass (issue #469's trust boundary): classify every input
/// of every node into exactly TWO admissible provenances —
///
///   - **RecipeOutput** — the input names another td recipe in the catalog,
///     realized by an earlier plan step;
///   - **AuditedSeed** — the input names a pinned, hash-verified source /
///     seed patch / stage0 artifact, interned into the ladder store by td's
///     own addToStore.
///
/// ANYTHING else is rejected HERE, during planning — there is no host-tool
/// class, no lock of store paths, no PATH lookup, and no store discovery.
/// Every declaration channel is classified: `sourceInput`, `inputs`, AND
/// `nativeInputs`. A rung that declares scaffolding the chain has not built
/// (bash, coreutils, make, …) fails closed with `PROVENANCE_REJECTED` until
/// that tool exists as a recipe output. Deliberately pure — no subprocess, no
/// filesystem — so the entry points run it BEFORE any ambient execution
/// (stage0 placement, interning): a rejected graph executes NOTHING.
fn classify_graph_inputs(nodes: &[RecipeNode]) -> Result<Vec<SeedInput>, String> {
    let mut seen = HashSet::new();
    let mut seed_inputs = Vec::new();
    for node in nodes {
        if let Some(key) = &node.recipe.source_input {
            let input = seed_input_for_recipe_source(key, &node.recipe)?;
            push_seed_input(&mut seed_inputs, &mut seen, input);
        }
        for input in node
            .recipe
            .inputs
            .iter()
            .chain(node.recipe.native_inputs.iter())
            .flatten()
        {
            if catalog::lookup(input).is_some() {
                continue;
            }
            match seed_input_for_recipe_input(input)? {
                Some(seed_input) => push_seed_input(&mut seed_inputs, &mut seen, seed_input),
                None => return Err(provenance_rejection(&node.stem, input)),
            }
        }
    }
    Ok(seed_inputs)
}

fn seed_input_for_recipe_source(key: &str, recipe: &Recipe) -> Result<SeedInput, String> {
    match special_seed_input(key)? {
        Some(input) => Ok(input),
        None => {
            let pin = source_pin_for_key(key).map_err(|e| {
                format!(
                    "ladder: cannot resolve sourceInput `{key}' for {}-{} to a recipe source \
                     pin: {e}",
                    recipe.name, recipe.version
                )
            })?;
            Ok(SeedInput::Source {
                key: key.to_string(),
                pin,
            })
        }
    }
}

fn seed_input_for_recipe_input(key: &str) -> Result<Option<SeedInput>, String> {
    if let Some(input) = special_seed_input(key)? {
        return Ok(Some(input));
    }
    Ok(source_pins::by_key(key).map(|pin| SeedInput::Source {
        key: key.to_string(),
        pin,
    }))
}

/// Planning-only provenance gate over TARGETS' full recipe closures — the
/// FIRST act of `check-run` and `build-run`, before the runner exists and
/// before ANY subprocess (stage0 placement, source interning, builds): a
/// graph with a forbidden input reds here and nothing ambient ever executes
/// for it (re #469).
fn ensure_targets_provenance(targets: &[&str]) -> Result<(), String> {
    let graph = recipe_closure(targets)?;
    classify_graph_inputs(&graph).map(|_| ())
}

fn special_seed_input(key: &str) -> Result<Option<SeedInput>, String> {
    if key == "stage0-source" {
        return Ok(Some(SeedInput::Stage0 {
            key: key.to_string(),
        }));
    }
    if key == "linux-headers" {
        return Ok(Some(SeedInput::LinuxHeaders {
            key: key.to_string(),
            arch: "i386",
        }));
    }
    if key == "linux-headers-x86-64" {
        return Ok(Some(SeedInput::LinuxHeaders {
            key: key.to_string(),
            arch: "x86_64",
        }));
    }
    if let Some(patch) = key.strip_prefix("patch-") {
        if patch.is_empty() {
            return Err(format!("ladder: malformed patch input `{key}'"));
        }
        // A pinned source whose key happens to start with `patch-` (the GNU
        // patch program's own `patch-mesboot-source`) is a Source, not a
        // seed/patches/*.patch file — the pin table wins over the prefix
        // convention. Warm hosts never hit this (the src map short-circuits
        // before intern_patch); a cold host fails the whole chain on it.
        if source_pins::by_key(key).is_none() {
            return Ok(Some(SeedInput::Patch {
                key: key.to_string(),
                patch: patch.to_string(),
            }));
        }
    }
    Ok(None)
}

fn source_pin_for_key(key: &str) -> Result<SourcePin, String> {
    source_pins::by_key(key).ok_or_else(|| format!("no recipe source pin for `{key}'"))
}

fn validate_source_file_basename(pin: &SourcePin) -> Result<(), String> {
    if pin.file.is_empty() || pin.file.contains('/') {
        return Err(format!(
            "recipe source pin `{}` has non-basename file `{}`",
            pin.key, pin.file
        ));
    }
    Ok(())
}

fn verify_source_pin(path: &Path, pin: &SourcePin) -> Result<(), String> {
    let mut bytes = Vec::new();
    append_file_bytes(path, &mut bytes)?;
    let got = sha256sum(&bytes);
    if got != pin.sha256 {
        return Err(format!(
            "{} sha256 {got} != recipe source pin {}",
            path.display(),
            pin.sha256
        ));
    }
    Ok(())
}

fn find_td_builder_self(root: &Path) -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("TD_BUILDER_SELF").map(PathBuf::from) {
        if is_executable(&path) {
            return Ok(path);
        }
        return Err(format!(
            "TD_BUILDER_SELF is not executable: {}",
            path.display()
        ));
    }
    let release = root.join("builder/target/release/td-builder");
    if is_executable(&release) {
        return Ok(release);
    }
    Err(format!(
        "TD_BUILDER_SELF is unset and {} is not executable; run `cargo build --release --manifest-path builder/Cargo.toml`",
        release.display()
    ))
}

fn place_stage0_builder(
    root: &Path,
    base: &Path,
    td_builder_self: &Path,
) -> Result<String, String> {
    fs::create_dir_all(base).map_err(|e| format!("mkdir {}: {e}", base.display()))?;
    let mut cmd = Command::new("sh");
    cmd.current_dir(root)
        .arg("tests/stage0-builder.sh")
        .arg(base)
        .env("TD_BUILDER_SELF", td_builder_self);
    let out = command_output(&mut cmd, "stage0-builder.sh")?;
    out.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "stage0-builder produced no output".to_string())
}

fn command_output(cmd: &mut Command, label: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("spawn {label}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{label} output not UTF-8: {e}"))
}

fn command_output_with_stdin(
    cmd: &mut Command,
    label: &str,
    stdin: &str,
) -> Result<String, String> {
    command_output_with_stdin_bytes(cmd, label, stdin.as_bytes())
}

fn command_output_with_stdin_bytes(
    cmd: &mut Command,
    label: &str,
    stdin: &[u8],
) -> Result<String, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {label}: {e}"))?;
    match child.stdin.as_mut() {
        Some(input) => input
            .write_all(stdin)
            .map_err(|e| format!("write {label} stdin: {e}"))?,
        None => return Err(format!("{label}: stdin pipe unavailable")),
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait {label}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{label} output not UTF-8: {e}"))
}

/// Hex SHA-256 of a byte string. In-process (`crate::sha256`) — pin
/// verification must not depend on an ambient host `sha256sum` (re #469).
fn sha256sum(bytes: &[u8]) -> String {
    crate::sha256::hex_digest(bytes)
}

fn append_file_bytes(path: &Path, out: &mut Vec<u8>) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.read_to_end(out)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(())
}

fn files_with_suffix(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>, String> {
    let mut files = read_dir_sorted(dir)?;
    files.retain(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(suffix))
            .unwrap_or(false)
            && p.is_file()
    });
    Ok(files)
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read dir {}: {e}", dir.display()))? {
        out.push(
            entry
                .map_err(|e| format!("read dir {} entry: {e}", dir.display()))?
                .path(),
        );
    }
    out.sort();
    Ok(out)
}

pub(crate) fn linux_version_from_file(file_name: &str) -> Result<String, String> {
    let rest = file_name
        .strip_prefix("linux-")
        .ok_or_else(|| format!("linux source file name is malformed: {file_name}"))?;
    rest.split(".tar")
        .next()
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("linux source file name is malformed: {file_name}"))
}

fn path_basename_str(path: &str) -> Result<&str, String> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("path has no UTF-8 basename: {path}"))
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub(crate) fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_dir() {
                make_user_writable(path)?;
                fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))
            } else {
                make_file_user_writable(path, &meta)?;
                fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

fn make_user_writable(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o700);
        fs::set_permissions(path, perms)
            .map_err(|e| format!("chmod u+rwx {}: {e}", path.display()))?;
        for child in read_dir_sorted(path)? {
            make_user_writable(&child)?;
        }
    } else {
        make_file_user_writable(path, &meta)?;
    }
    Ok(())
}

fn make_file_user_writable(path: &Path, meta: &fs::Metadata) -> Result<(), String> {
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() | 0o600);
    fs::set_permissions(path, perms).map_err(|e| format!("chmod u+rw {}: {e}", path.display()))
}

fn single_subdir_path(dir: &Path) -> Result<PathBuf, String> {
    let mut subdirs = Vec::new();
    for path in read_dir_sorted(dir)? {
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    match subdirs.len() {
        1 => subdirs
            .pop()
            .ok_or_else(|| format!("expected one top-level dir under {}", dir.display())),
        n => Err(format!(
            "expected one top-level dir under {}, found {n}",
            dir.display()
        )),
    }
}

fn clean_stage0_build_dirs(root: &Path) -> Result<(), String> {
    for dir in ["AMD64/artifact", "AMD64/bin"] {
        let path = root.join(dir);
        remove_path_if_exists(&path)?;
        fs::create_dir_all(&path).map_err(|e| format!("mkdir {}: {e}", path.display()))?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        symlink(target, dst)?;
        return Ok(());
    }
    if ftype.is_dir() {
        fs::create_dir_all(dst)?;
        let mut children = Vec::new();
        for entry in fs::read_dir(src)? {
            children.push(entry?.path());
        }
        children.sort();
        for child in children {
            if let Some(name) = child.file_name() {
                copy_tree(&child, &dst.join(name))?;
            }
        }
        fs::set_permissions(dst, meta.permissions())?;
        return Ok(());
    }
    if ftype.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        fs::set_permissions(dst, meta.permissions())?;
    }
    Ok(())
}

fn tail_bytes(bytes: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut selected: Vec<&str> = text.lines().rev().take(lines).collect();
    selected.reverse();
    selected.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_patch_prefixed_source_is_a_source_not_a_seed_patch() {
        // `patch-mesboot-source` pins the GNU patch PROGRAM's tarball; the
        // `patch-` prefix convention must not shadow it into a (nonexistent)
        // seed/patches/mesboot-source.patch — that broke every cold-host
        // chain build at the first mesboot rung.
        assert!(special_seed_input("patch-mesboot-source")
            .unwrap()
            .is_none());
        match special_seed_input("patch-binutils-boot-2.20.1a").unwrap() {
            Some(SeedInput::Patch { patch, .. }) => {
                assert_eq!(patch, "binutils-boot-2.20.1a")
            }
            _ => panic!("expected the binutils-boot seed patch input"),
        }
    }

    #[test]
    fn recipe_closure_is_derived_from_catalog_edges() {
        let graph = recipe_closure(&["busybox-test"]).unwrap();
        let stems: Vec<&str> = graph.iter().map(|node| node.stem.as_str()).collect();

        for expected in [
            "stage0",
            "mes",
            "gcc-x86-64-stage2",
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "make-x86-64",
            "busybox-x86-64",
            "busybox-test",
        ] {
            assert!(
                stems.iter().any(|stem| stem == &expected),
                "missing {expected} from busybox-test closure: {stems:?}"
            );
        }

        let busybox_pos = stems
            .iter()
            .position(|stem| stem == &"busybox-x86-64")
            .unwrap();
        let test_pos = stems
            .iter()
            .position(|stem| stem == &"busybox-test")
            .unwrap();
        assert!(
            busybox_pos < test_pos,
            "dependency should be emitted before dependent: {stems:?}"
        );
    }

    /// #469 structural test, verified RED against the pre-cutover code: the
    /// old runner classified `bash`/`coreutils`/… as a "seed tool" class and
    /// resolved them through tests/td-seed-tools.lock, then td-subst.lock,
    /// then ambient PATH, so planning ACCEPTED this graph. Now the graph
    /// fails closed at planning —
    /// before any intern/build — until the scaffolding rungs exist as recipe
    /// outputs. The classification is a FREE function needing no runner (and
    /// therefore no stage0 placement, no subprocess): the check-run/build-run
    /// entry points call it before constructing the runner, so a rejected
    /// graph executes nothing ambient. When the chain becomes self-hosting,
    /// this test flips to asserting acceptance (delete it in the same PR that
    /// adds the rungs).
    #[test]
    fn bootstrap_graph_with_host_scaffolding_is_rejected_at_planning() {
        for target in [
            "make-test",
            "busybox-test",
            "gcc-x86-64-stage2-test",
            "gcc-x86-64-native-test",
            "gcc-x86-64-self-test",
        ] {
            let err = ensure_targets_provenance(&[target]).unwrap_err();
            assert!(
                err.starts_with(PROVENANCE_REJECTED),
                "{target}: expected a provenance rejection, got: {err}"
            );
        }
    }

    /// #469 structural test: a synthetic recipe declaring a host tool, an
    /// absolute host path, or a host-store path is rejected during planning —
    /// on the `inputs` channel AND the `nativeInputs` channel (review finding:
    /// the native channel must not sail through planning and surface later at
    /// lock synthesis). The classifier admits exactly catalog outputs and
    /// pinned seeds; no name, path string, or store prefix is provenance.
    #[test]
    fn synthetic_recipes_with_forbidden_inputs_are_rejected_at_planning() {
        for forbidden in [
            "bash",
            "make",
            "python",
            "/usr/bin/env",
            "/gnu/store/abc123-gcc-toolchain-15.2.0",
        ] {
            for native in [false, true] {
                let recipe = Recipe::mesboot("synthetic-red", "0");
                let recipe = if native {
                    recipe.native_inputs(&[forbidden])
                } else {
                    recipe.inputs_owned(vec![forbidden.to_string()])
                };
                let nodes = vec![RecipeNode {
                    stem: "synthetic-red".to_string(),
                    recipe,
                }];
                let err = classify_graph_inputs(&nodes).unwrap_err();
                assert!(
                    err.starts_with(PROVENANCE_REJECTED) && err.contains(forbidden),
                    "input `{forbidden}' (native={native}): expected a provenance \
                     rejection, got: {err}"
                );
            }
        }
    }

    /// The classifier itself: a non-special, non-pinned input has NO seed
    /// interpretation (the caller rejects it); pinned sources and the special
    /// seed keys still classify as AuditedSeed.
    #[test]
    fn only_pinned_seeds_classify_as_seed_inputs() {
        for tool in ["bash", "coreutils", "sed", "make", "python", "flex"] {
            assert!(
                seed_input_for_recipe_input(tool).unwrap().is_none(),
                "`{tool}' must not classify as a seed input"
            );
        }
        assert!(seed_input_for_recipe_input("stage0-source")
            .unwrap()
            .is_some());
        assert!(seed_input_for_recipe_input("linux-headers-x86-64")
            .unwrap()
            .is_some());
    }

    #[test]
    fn output_lookup_uses_the_current_build_log_only() {
        let tmp = env::temp_dir().join(format!("td-recipe-runner-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let old = tmp.join("build-old.out");
        let current = tmp.join("build-current.out");
        fs::write(&old, "STEP rust-toolchain /td/store/stale-rust\n").unwrap();
        fs::write(&current, "STEP rust-toolchain /td/store/current-rust\n").unwrap();
        let runner = RecipeCheckRunner {
            root: PathBuf::new(),
            tb: PathBuf::new(),
            builder_path: String::new(),
            builder_store: PathBuf::new(),
            builder_db: PathBuf::new(),
            lw: tmp.clone(),
            store: PathBuf::new(),
            db: PathBuf::new(),
            recipes: PathBuf::new(),
            scratch: tmp.join("scratch"),
            force_cold: false,
        };

        let got = runner.ladder_out_from(&current, "rust-toolchain").unwrap();

        assert_eq!(got, tmp.join("scratch/tdstore/current-rust"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
