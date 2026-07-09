use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use td_recipe::{
    catalog, source_pins,
    types::{Recipe, SourcePin},
};

const TD_STORE_DIR: &str = "/td/store";

pub fn cli(args: &[String]) -> Result<(), String> {
    let stem = args.first().ok_or_else(usage)?.as_str();
    let scope = args.get(1).map(String::as_str).unwrap_or("daily");
    let index = parse_index(args.get(2))?;
    if args.get(3).is_some() {
        return Err(usage());
    }
    assert_selected_check(stem, scope, index)?;

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let runner = RecipeCheckRunner::new(root)?;
    let _lock = lock_file(&runner.lock_path())?;
    runner.setup()?;
    match (stem, index) {
        ("make-test", 1) => runner.run_make_test(),
        ("busybox-test", 1) => runner.run_busybox_test(),
        ("rust-toolchain", 1) => runner.run_rust_toolchain_check(),
        ("gcc-x86-64-stage2", 1) => runner.run_x86_64_cross_toolchain_check(),
        ("gcc-x86-64-native", 1) => runner.run_x86_64_native_gcc_check(),
        ("gcc-x86-64-native", 2) => runner.run_x86_64_self_gcc_check(),
        other => Err(format!(
            "{} check index {} has no Rust check-runner implementation yet",
            other.0, other.1
        )),
    }
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

    let root = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let runner = RecipeCheckRunner::new(root)?;
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

fn assert_selected_check(stem: &str, scope: &str, index: usize) -> Result<(), String> {
    let tier = parse_tier(scope)?;
    let recipe = catalog::lookup(stem)
        .ok_or_else(|| format!("unknown recipe stem '{stem}' (try `list`)"))?;
    let count = recipe
        .checks
        .as_ref()
        .map(|checks| {
            checks
                .iter()
                .filter(|check| tier.map(|t| check.tier == t).unwrap_or(true))
                .count()
        })
        .unwrap_or(0);
    if count == 0 {
        return Err(format!("{stem} has no checks in the requested tier"));
    }
    if index > count {
        return Err(format!(
            "{stem} has only {count} check(s) in the requested tier; index {index} is out of range"
        ));
    }
    Ok(())
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

struct RecipeCheckRunner {
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
    fn new(root: PathBuf) -> Result<Self, String> {
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
        let scratch = lw.join("scratch");
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

    fn lock_path(&self) -> PathBuf {
        self.lw.with_extension("lock")
    }

    fn setup(&self) -> Result<(), String> {
        if self.force_cold {
            remove_path_if_exists(&self.lw)?;
        }
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("mkdir {}: {e}", self.scratch.display()))?;
        let pinsum = self.setup_pinsum()?;
        let setup_ok = self.lw.join("setup-ok");
        let warm = fs::read_to_string(&setup_ok)
            .map(|s| s == pinsum)
            .unwrap_or(false)
            && self.lw.join("srcs.map").is_file()
            && self.lw.join("tools.map").is_file();
        if warm {
            return Ok(());
        }

        remove_path_if_exists(&self.store)?;
        remove_path_if_exists(&self.db)?;
        remove_path_if_exists(&self.lw.join("srcs.map"))?;
        remove_path_if_exists(&self.lw.join("tools.map"))?;
        remove_path_if_exists(&setup_ok)?;
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        File::create(self.lw.join("srcs.map")).map_err(|e| format!("create srcs.map: {e}"))?;
        File::create(self.lw.join("tools.map")).map_err(|e| format!("create tools.map: {e}"))?;
        fs::write(&setup_ok, pinsum).map_err(|e| format!("write {}: {e}", setup_ok.display()))?;
        Ok(())
    }

    fn setup_pinsum(&self) -> Result<String, String> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ladder-setup-v6\n");
        append_file_bytes(&self.root.join("tests/td-subst.lock"), &mut bytes)?;
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
        sha256sum(&bytes)
    }

    fn tool_root(&self, name: &str, probe: &str) -> Result<PathBuf, String> {
        let mut lock_cmd = self.builder_command();
        lock_cmd
            .arg("lock")
            .arg("path")
            .arg("tests/td-subst.lock")
            .arg(name);
        if let Ok(out) = command_output(&mut lock_cmd, "td-builder lock path") {
            let path = PathBuf::from(out.trim());
            if is_executable(&path.join("bin").join(probe)) {
                return Ok(path);
            }
        }

        if let Some(bin) = find_in_path(probe) {
            let real = fs::canonicalize(&bin).unwrap_or(bin);
            if let Some(root) = real.parent().and_then(Path::parent).map(Path::to_path_buf) {
                if is_executable(&root.join("bin").join(probe)) {
                    return Ok(root);
                }
            }
        }

        let mut candidates = Vec::new();
        if let Ok(entries) = fs::read_dir("/gnu/store") {
            for entry in entries {
                let entry = entry.map_err(|e| format!("read /gnu/store entry: {e}"))?;
                let root = entry.path();
                let base = match root.file_name().and_then(|b| b.to_str()) {
                    Some(b) => b,
                    None => continue,
                };
                let normal = format!("-{name}-");
                let minimal = format!("-{name}-minimal-");
                if (base.contains(&normal) || base.contains(&minimal))
                    && is_executable(&root.join("bin").join(probe))
                {
                    candidates.push(root);
                }
            }
        }
        candidates.sort();
        if let Some(root) = candidates.first() {
            return Ok(root.clone());
        }
        Err(format!(
            "ladder: cannot resolve the {name} package (probe {probe}) - not in tests/td-subst.lock, not on PATH, no /gnu/store/*-{name}-* on this host"
        ))
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

    fn append_tools_map(&self, name: &str, root: &Path) -> Result<(), String> {
        let map = self.lw.join("tools.map");
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&map)
            .map_err(|e| format!("open {}: {e}", map.display()))?;
        writeln!(file, "{name} {}", root.display())
            .map_err(|e| format!("write {}: {e}", map.display()))
    }

    fn map_has(&self, name: &str) -> Result<bool, String> {
        Ok(self.map_value_opt(name)?.is_some())
    }

    fn map_value(&self, name: &str) -> Result<String, String> {
        self.map_value_opt(name)?
            .ok_or_else(|| format!("ladder: no map entry for `{name}'"))
    }

    fn map_value_opt(&self, name: &str) -> Result<Option<String>, String> {
        for map in [self.lw.join("srcs.map"), self.lw.join("tools.map")] {
            if !map.is_file() {
                continue;
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

    fn prepare_recipe_target(&self, target: &str) -> Result<(), String> {
        let graph = recipe_closure(&[target])?;
        self.ensure_graph_inputs(&graph)?;
        self.emit_recipe_graph(&graph)?;
        self.stage_tdstore()
    }

    fn ensure_graph_inputs(&self, nodes: &[RecipeNode]) -> Result<(), String> {
        let mut seen_seed_inputs = HashSet::new();
        let mut seed_inputs = Vec::new();
        let mut host_tools = Vec::new();

        for node in nodes {
            if let Some(key) = &node.recipe.source_input {
                let input = self.seed_input_for_recipe_source(key, &node.recipe)?;
                push_seed_input(&mut seed_inputs, &mut seen_seed_inputs, input);
            }
            if let Some(inputs) = &node.recipe.inputs {
                for input in inputs {
                    if catalog::lookup(input).is_some() {
                        continue;
                    }
                    if let Some(seed_input) = self.seed_input_for_recipe_input(input)? {
                        push_seed_input(&mut seed_inputs, &mut seen_seed_inputs, seed_input);
                    } else {
                        self.assert_host_tool(input)?;
                        push_unique_string(&mut host_tools, input);
                    }
                }
            }
        }

        for input in seed_inputs {
            self.ensure_seed_input(&input)?;
        }
        for tool in host_tools {
            self.ensure_host_tool(&tool)?;
        }
        Ok(())
    }

    fn seed_input_for_recipe_source(
        &self,
        key: &str,
        recipe: &Recipe,
    ) -> Result<SeedInput, String> {
        match special_seed_input(key)? {
            Some(input) => Ok(input),
            None => {
                let pin = self.source_pin_for_recipe_source(key, recipe)?;
                Ok(SeedInput::Source {
                    key: key.to_string(),
                    pin,
                })
            }
        }
    }

    fn seed_input_for_recipe_input(&self, key: &str) -> Result<Option<SeedInput>, String> {
        if let Some(input) = special_seed_input(key)? {
            return Ok(Some(input));
        }
        if self.host_tool_probe(key).is_some() {
            return Ok(None);
        }
        Ok(self
            .source_pin_for_input_key(key)?
            .map(|pin| SeedInput::Source {
                key: key.to_string(),
                pin,
            }))
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

    fn ensure_host_tool(&self, name: &str) -> Result<(), String> {
        let probe = self.assert_host_tool(name)?;
        let tools_map = self.lw.join("tools.map");
        if let Some(root) = map_value_in(&tools_map, name)? {
            let path = PathBuf::from(&root);
            if is_executable(&path.join("bin").join(probe)) {
                return Ok(());
            }
            remove_map_entry(&tools_map, name)?;
        }
        let root = self.tool_root(name, probe)?;
        self.append_tools_map(name, &root)
    }

    fn assert_host_tool<'a>(&self, name: &'a str) -> Result<&'a str, String> {
        self.host_tool_probe(name).ok_or_else(|| {
            format!(
                "ladder: input `{name}' is neither a recipe, source/patch input, nor a known host tool"
            )
        })
    }

    fn host_tool_probe<'a>(&self, name: &'a str) -> Option<&'a str> {
        match name {
            "coreutils" => Some("ls"),
            "gawk" => Some("awk"),
            "findutils" => Some("find"),
            "diffutils" => Some("diff"),
            "python" => Some("python3"),
            "bash" | "sed" | "grep" | "tar" | "gzip" | "bzip2" | "xz" | "flex" | "bison" | "m4"
            | "make" => Some(name),
            _ => None,
        }
    }

    fn source_pin_for_recipe_source(
        &self,
        key: &str,
        recipe: &Recipe,
    ) -> Result<SourcePin, String> {
        source_pin_for_key(key).map_err(|e| {
            format!(
                "ladder: cannot resolve sourceInput `{key}' for {}-{} to a recipe source pin: {e}",
                recipe.name, recipe.version
            )
        })
    }

    fn source_pin_for_input_key(&self, key: &str) -> Result<Option<SourcePin>, String> {
        match source_pins::by_key(key) {
            Some(pin) => Ok(Some(pin)),
            None => Ok(None),
        }
    }

    fn build_plan(&self, target: &str) -> Result<PathBuf, String> {
        let srcs = fs::read(self.lw.join("srcs.map")).map_err(|e| format!("read srcs.map: {e}"))?;
        let tools =
            fs::read(self.lw.join("tools.map")).map_err(|e| format!("read tools.map: {e}"))?;
        let auto_map = self.lw.join("auto-map");
        let mut map =
            File::create(&auto_map).map_err(|e| format!("create {}: {e}", auto_map.display()))?;
        map.write_all(&srcs)
            .map_err(|e| format!("write {}: {e}", auto_map.display()))?;
        map.write_all(&tools)
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
            .arg("/gnu/store")
            .arg(scratch);
        let out = cmd
            .output()
            .map_err(|e| format!("spawn build-plan --auto {target}: {e}"))?;
        let out_file = self.lw.join(format!("build-{target}.out"));
        let err_file = self.lw.join(format!("build-{target}.err"));
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

    fn ladder_out_from(&self, build_out: &Path, rung: &str) -> Result<PathBuf, String> {
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

    fn run_make_test(&self) -> Result<(), String> {
        self.prepare_recipe_target("make-test")?;
        self.build_plan("make-test")?;
        println!(
            "PASS: make-test - GNU make 4.4.1 built on the native /td/store toolchain drove a real build in the recipe sandbox"
        );
        Ok(())
    }

    fn run_busybox_test(&self) -> Result<(), String> {
        self.prepare_recipe_target("busybox-test")?;
        self.build_plan("busybox-test")?;
        println!(
            "PASS: busybox-test - BusyBox 1.37.0 built by make-x86-64 on the native /td/store toolchain ran installed sh/ls/grep/sed applet links"
        );
        Ok(())
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

    fn run_rust_toolchain_check(&self) -> Result<(), String> {
        self.prepare_recipe_target("rust-toolchain")?;
        let build_out = self.build_plan("rust-toolchain")?;
        let rust_tree = self.ladder_out_from(&build_out, "rust-toolchain")?;
        println!(
            "   [ladder] x86_64 rust-toolchain via build-plan --auto: catalog dependency closure -> rust-toolchain (relinked rustc/cargo tree {})",
            rust_tree.display()
        );
        let rustc = rust_tree.join("bin/rustc");
        let cargo = rust_tree.join("bin/cargo");
        if !is_executable(&rustc) {
            return Err(format!(
                "rustc missing from rust-toolchain output ({})",
                rustc.display()
            ));
        }
        if !is_executable(&cargo) {
            return Err(format!(
                "cargo missing from rust-toolchain output ({})",
                cargo.display()
            ));
        }
        let rbase = rust_tree
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                format!(
                    "malformed rust-toolchain output path {}",
                    rust_tree.display()
                )
            })?;
        let rpath = format!("{TD_STORE_DIR}/{rbase}");
        let rustc_version =
            self.store_ns_output(&[&format!("{rpath}/bin/rustc"), "--version"], None)?;
        if !rustc_version.starts_with("rustc 1.96.0") {
            return Err(format!(
                "rustc version did not match the pinned 1.96.0 release: {}",
                rustc_version.trim()
            ));
        }
        self.store_ns_output(&[&format!("{rpath}/bin/cargo"), "--version"], None)?;
        self.store_ns_output(
            &[
                &format!("{rpath}/bin/rustc"),
                "--crate-name",
                "td_rust_smoke",
                "--crate-type=lib",
                "--edition=2021",
                "-",
                "-o",
                "/tmp/librust_smoke.rlib",
            ],
            Some("pub fn td_rust_smoke() -> u32 { 42 }\n"),
        )?;
        println!(
            "PASS: rust-toolchain: Rust 1.96.0 relinked onto /td/store runs rustc/cargo and compiles a simple library"
        );
        Ok(())
    }

    fn run_x86_64_cross_toolchain_check(&self) -> Result<(), String> {
        self.prepare_recipe_target("gcc-x86-64-stage2")?;
        let build_out = self.build_plan("gcc-x86-64-stage2")?;
        let xbu = self.ladder_out_from(&build_out, "binutils-x86-64")?;
        let xgcc2 = self
            .ladder_out_from(&build_out, "gcc-x86-64-stage2")?
            .join("stage/td/store/gcc-14.3.0-x86_64");
        let xglibc = self
            .ladder_out_from(&build_out, "glibc-x86-64")?
            .join("stage/td/store/glibc-2.41_x86_64");
        self.verify_x86_64_cross_outputs(&xbu, &xgcc2, &xglibc)?;
        println!(
            "PASS: gcc-x86-64-stage2 - recipe graph built the x86_64 cross toolchain and its dynamic C/C++ outputs run in td's own /td/store root"
        );
        Ok(())
    }

    fn run_x86_64_native_gcc_check(&self) -> Result<(), String> {
        let (xnbu, xngcc, xglibc) = self.build_x86_64_native_recipe_outputs()?;
        self.verify_x86_64_native_outputs(&xnbu, &xngcc, &xglibc, "gcc-14.3.0-x86_64-native")?;
        println!(
            "PASS: gcc-x86-64-native - recipe graph built an ELF64 native x86_64 gcc that compiles and runs C/C++ outputs in td's own /td/store root"
        );
        Ok(())
    }

    fn run_x86_64_self_gcc_check(&self) -> Result<(), String> {
        let (xnbu, xngcc, xglibc) = self.build_x86_64_native_recipe_outputs()?;
        let self_out = self.scratch.join("x86_64-self-gcc");
        remove_path_if_exists(&self_out)?;
        fs::create_dir_all(&self_out).map_err(|e| format!("mkdir {}: {e}", self_out.display()))?;
        let cpath = self.curated_path()?;
        let mut cmd = self.builder_command();
        cmd.arg("toolchain-recipe")
            .arg("x86_64-self")
            .env("TDXS_CPATH", cpath)
            .env("TDXS_BUILDER_GCC", &xngcc)
            .env("TDXS_BUILDER_BINUTILS", &xnbu)
            .env("TDXS_GLIBC", &xglibc)
            .env(
                "TDXS_BINUTILS_TAR",
                self.source_file_for_key("binutils-244-source")?,
            )
            .env("TDXS_GCC_TAR", self.source_file_for_key("gcc-14-source")?)
            .env("TDXS_GMP_TAR", self.source_file_for_key("gmp63")?)
            .env("TDXS_MPFR_TAR", self.source_file_for_key("mpfr421")?)
            .env("TDXS_MPC_TAR", self.source_file_for_key("mpc131")?)
            .env(
                "TDXS_KERNEL_HEADERS_TAR",
                self.linux_headers_file("x86_64")?,
            )
            .env("TDXS_OUT", &self_out)
            .env(
                "X86_MAKE_J",
                env::var("X86_MAKE_J").unwrap_or_else(|_| "-j4".to_string()),
            );
        let log = command_output(&mut cmd, "td-builder toolchain-recipe x86_64-self")?;
        io::stdout()
            .write_all(log.as_bytes())
            .map_err(|e| format!("write self-gcc log: {e}"))?;
        let xsbu = extract_line_value(&log, "SELF_BINUTILS=")
            .map(PathBuf::from)
            .ok_or_else(|| "toolchain-recipe x86_64-self returned no SELF_BINUTILS".to_string())?;
        let xsgcc = extract_line_value(&log, "SELF_GCC=")
            .map(PathBuf::from)
            .ok_or_else(|| "toolchain-recipe x86_64-self returned no SELF_GCC".to_string())?;
        self.assert_codegen_agreement(&xngcc, &xsgcc)?;
        let staged_sbu = self.stage_tree_under_tdstore(&xsbu)?;
        let staged_sgcc = self.stage_tree_under_tdstore(&xsgcc)?;
        self.verify_x86_64_native_outputs(
            &staged_sbu,
            &staged_sgcc,
            &xglibc,
            "gcc-14.3.0-x86_64-self",
        )?;
        println!(
            "PASS: gcc-x86-64-native self-host - native recipe output rebuilt gcc and the rebuilt compiler agrees on codegen and runs in td's own /td/store root"
        );
        Ok(())
    }

    fn build_x86_64_native_recipe_outputs(&self) -> Result<(PathBuf, PathBuf, PathBuf), String> {
        self.prepare_recipe_target("gcc-x86-64-native")?;
        let build_out = self.build_plan("gcc-x86-64-native")?;
        let xnbu = self.ladder_out_from(&build_out, "binutils-x86-64-native")?;
        let xngcc = self
            .ladder_out_from(&build_out, "gcc-x86-64-native")?
            .join("stage/td/store/gcc-14.3.0-x86_64-native");
        let xglibc = self
            .ladder_out_from(&build_out, "glibc-x86-64")?
            .join("stage/td/store/glibc-2.41_x86_64");
        Ok((xnbu, xngcc, xglibc))
    }

    fn verify_x86_64_cross_outputs(
        &self,
        xbu: &Path,
        xgcc: &Path,
        xglibc: &Path,
    ) -> Result<(), String> {
        let readelf = xbu.join("bin/x86_64-pc-linux-gnu-readelf");
        require_exec(&readelf, "cross readelf")?;
        require_exec(&xgcc.join("bin/x86_64-pc-linux-gnu-gcc"), "cross gcc")?;
        require_exec(&xgcc.join("bin/x86_64-pc-linux-gnu-g++"), "cross g++")?;
        require_file(&xglibc.join("lib/libc.so.6"), "x86_64 libc")?;
        for path in [
            xglibc.join("lib/libc.so.6"),
            xgcc.join("bin/x86_64-pc-linux-gnu-gcc"),
            find_first_named(xgcc, "cc1")?,
        ] {
            reject_embedded_gnu_store(&path)?;
        }
        let work = self.fresh_scratch("x86-cross-probe")?;
        write_x86_probe_sources(&work)?;
        let glibc_logical = self.logical_tdstore_path(xglibc)?;
        compile_x86_64_cross(
            xbu,
            xgcc,
            xglibc,
            &glibc_logical,
            &work,
            "gcc",
            "c.c",
            "c.out",
        )?;
        compile_x86_64_cross(
            xbu,
            xgcc,
            xglibc,
            &glibc_logical,
            &work,
            "g++",
            "cpp.cc",
            "cpp.out",
        )?;
        assert_elf64_x86_64(&readelf, &work.join("c.out"), "cross C probe")?;
        assert_interp(&readelf, &work.join("c.out"), &glibc_logical)?;
        reject_embedded_gnu_store(&work.join("c.out"))?;
        reject_embedded_gnu_store(&work.join("cpp.out"))?;
        self.stage_and_run_x86_probes(&work, "x86-cross-probe")?;
        Ok(())
    }

    fn verify_x86_64_native_outputs(
        &self,
        xnbu: &Path,
        xngcc: &Path,
        xglibc: &Path,
        expected_gcc_name: &str,
    ) -> Result<(), String> {
        let readelf = xnbu.join("bin/readelf");
        require_exec(&readelf, "native readelf")?;
        require_exec(&xngcc.join("bin/gcc"), "native gcc")?;
        require_exec(&xngcc.join("bin/g++"), "native g++")?;
        require_exec(&xnbu.join("bin/as"), "native as")?;
        require_exec(&xnbu.join("bin/ld"), "native ld")?;
        require_file(&xglibc.join("lib/libc.so.6"), "x86_64 libc")?;
        assert_elf64_x86_64(&readelf, &xngcc.join("bin/gcc"), expected_gcc_name)?;
        for path in [
            xngcc.join("bin/gcc"),
            find_first_named(xngcc, "cc1")?,
            xnbu.join("bin/as"),
            xnbu.join("bin/ld"),
            xglibc.join("lib/libc.so.6"),
        ] {
            reject_embedded_gnu_store(&path)?;
        }
        let work = self.fresh_scratch("x86-native-probe")?;
        write_x86_probe_sources(&work)?;
        let glibc_logical = self.logical_tdstore_path(xglibc)?;
        let nbu_logical = self.logical_tdstore_path(xnbu)?;
        let ngcc_logical = self.logical_tdstore_path(xngcc)?;
        compile_x86_64_native_in_ownroot(
            self,
            &ngcc_logical,
            &nbu_logical,
            &glibc_logical,
            "gcc",
            "c",
            c_probe_source(),
        )?;
        compile_x86_64_native_in_ownroot(
            self,
            &ngcc_logical,
            &nbu_logical,
            &glibc_logical,
            "g++",
            "cpp",
            cpp_probe_source(),
        )?;
        let work = self.fresh_scratch("x86-native-host-probe")?;
        write_x86_probe_sources(&work)?;
        compile_x86_64_native_host(
            xnbu,
            xngcc,
            xglibc,
            &glibc_logical,
            &work,
            "gcc",
            "c.c",
            "c.out",
        )?;
        compile_x86_64_native_host(
            xnbu,
            xngcc,
            xglibc,
            &glibc_logical,
            &work,
            "g++",
            "cpp.cc",
            "cpp.out",
        )?;
        assert_elf64_x86_64(&readelf, &work.join("c.out"), "native C probe")?;
        assert_interp(&readelf, &work.join("c.out"), &glibc_logical)?;
        reject_embedded_gnu_store(&work.join("c.out"))?;
        reject_embedded_gnu_store(&work.join("cpp.out"))?;
        Ok(())
    }

    fn assert_codegen_agreement(&self, native_gcc: &Path, self_gcc: &Path) -> Result<(), String> {
        let work = self.fresh_scratch("x86-self-codegen")?;
        fs::write(
            work.join("cg.c"),
            "unsigned fib(unsigned n) { unsigned a = 0, b = 1; while (n--) { unsigned t = a + b; a = b; b = t; } return a; }\n\
             int classify(int x) { switch (x & 3) { case 0: return x / 3; case 1: return x * 5; case 2: return x - 7; default: return -x; } }\n\
             int main(void) { return (fib(12) == 144 && classify(9) == 45) ? 42 : 1; }\n",
        )
        .map_err(|e| format!("write codegen C source: {e}"))?;
        fs::write(
            work.join("cg.cc"),
            "template <typename T> struct Acc { T v; explicit Acc(T s) : v(s) {} Acc &add(T x) { v += x; return *this; } };\n\
             template <typename T> T sq(T x) { return x * x; }\n\
             int main() { Acc<int> a(2); a.add(sq(3)).add(sq(5)); return a.v == 36 ? 42 : 1; }\n",
        )
        .map_err(|e| format!("write codegen C++ source: {e}"))?;
        for (tree, prefix) in [(native_gcc, "native"), (self_gcc, "self")] {
            require_exec(&tree.join("bin/gcc"), "codegen gcc")?;
            require_exec(&tree.join("bin/g++"), "codegen g++")?;
            compile_to_assembly(tree, "gcc", &work, "cg.c", &format!("{prefix}-c.s"))?;
            compile_to_assembly(tree, "g++", &work, "cg.cc", &format!("{prefix}-cpp.s"))?;
        }
        for (label, a, b) in [
            ("c", "native-c.s", "self-c.s"),
            ("cpp", "native-cpp.s", "self-cpp.s"),
        ] {
            let ha = sha256_file(&work.join(a))?;
            let hb = sha256_file(&work.join(b))?;
            if ha != hb {
                return Err(format!(
                    "{label} assembly differs between native gcc ({ha}) and self-rebuilt gcc ({hb})"
                ));
            }
        }
        Ok(())
    }

    fn stage_and_run_x86_probes(&self, work: &Path, name: &str) -> Result<(), String> {
        let root = self.scratch.join("tdstore").join(name);
        remove_path_if_exists(&root)?;
        fs::create_dir_all(root.join("bin"))
            .map_err(|e| format!("mkdir {}: {e}", root.join("bin").display()))?;
        for (src, dst) in [("c.out", "c"), ("cpp.out", "cpp")] {
            let to = root.join("bin").join(dst);
            fs::copy(work.join(src), &to)
                .map_err(|e| format!("copy {src} to {}: {e}", to.display()))?;
            set_executable(&to)?;
        }
        let c = format!("{TD_STORE_DIR}/{name}/bin/c");
        let c_out = self.store_ns_output(&[c.as_str()], None)?;
        require_output_line(&c_out, "C-RAN", "cross C probe did not run")?;
        require_output_line(
            &c_out,
            "GNU-ABSENT",
            "/gnu/store is present in cross C probe root",
        )?;
        let cpp = format!("{TD_STORE_DIR}/{name}/bin/cpp");
        let cpp_out = self.store_ns_output(&[cpp.as_str()], None)?;
        require_output_line(&cpp_out, "CPP-RAN", "cross C++ probe did not run")?;
        require_output_line(
            &cpp_out,
            "GNU-ABSENT",
            "/gnu/store is present in cross C++ probe root",
        )
    }

    fn stage_static_bash(&self) -> Result<String, String> {
        let src = match env::var_os("TD_GATE_INPUT_BASH_STATIC").map(PathBuf::from) {
            Some(path) if is_executable(&path.join("bin/bash")) => path,
            _ => {
                let lock_rel = "tests/td-subst.lock";
                let lock_text = fs::read_to_string(self.root.join(lock_rel))
                    .map_err(|e| format!("read {lock_rel}: {e}"))?;
                let bash = lock_text
                    .lines()
                    .find(|line| line.contains("-bash-") && !line.contains("static"))
                    .and_then(|line| line.split_once(' ').map(|(_, path)| path.trim()))
                    .ok_or_else(|| format!("no dynamic bash entry in {lock_rel}"))?;
                let mut cmd = self.builder_command();
                cmd.arg("store-closure-scan").arg("/gnu/store").arg(bash);
                let scan = command_output(&mut cmd, "store-closure-scan bash")?;
                scan.lines()
                    .find(|line| line.contains("-bash-static-"))
                    .map(|line| PathBuf::from(line.trim()))
                    .ok_or_else(|| {
                        format!("no bash-static member in the /gnu/store closure of {bash}")
                    })?
            }
        };
        require_exec(&src.join("bin/bash"), "static bash")?;
        let base = src
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("static bash path has no UTF-8 basename: {}", src.display()))?;
        let dst = self.scratch.join("tdstore").join(base);
        if !is_executable(&dst.join("bin/bash")) {
            remove_path_if_exists(&dst)?;
            copy_tree(&src, &dst).map_err(|e| {
                format!(
                    "stage static bash into tdstore failed ({} -> {}): {e}",
                    src.display(),
                    dst.display()
                )
            })?;
        }
        Ok(base.to_string())
    }

    fn store_ns_bash(&self, script: &str) -> Result<String, String> {
        let bash_base = self.stage_static_bash()?;
        let bash = format!("{TD_STORE_DIR}/{bash_base}/bin/bash");
        self.store_ns_output(&[bash.as_str(), "-c", script], None)
    }

    fn source_file_for_key(&self, key: &str) -> Result<PathBuf, String> {
        let pin = source_pin_for_key(key)?;
        validate_source_file_basename(&pin)?;
        let file = self.root.join(".td-build-cache/sources").join(&pin.file);
        if !file.is_file() {
            return Err(format!(
                "pinned source not warm for {key}: {}",
                file.display()
            ));
        }
        verify_source_pin(&file, &pin)?;
        Ok(file)
    }

    fn linux_headers_file(&self, arch: &str) -> Result<PathBuf, String> {
        let pin = source_pin_for_key("linux-source")?;
        let version = linux_version_from_file(&pin.file)?;
        let file = self
            .root
            .join(".td-build-cache/sources")
            .join(format!("linux-headers-{version}-{arch}.tar.gz"));
        if !file.is_file() {
            return Err(format!("kernel headers not warm: {}", file.display()));
        }
        Ok(file)
    }

    fn fresh_scratch(&self, name: &str) -> Result<PathBuf, String> {
        let dir = self.scratch.join(name);
        remove_path_if_exists(&dir)?;
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        Ok(dir)
    }

    fn logical_tdstore_path(&self, path: &Path) -> Result<String, String> {
        let root = self.scratch.join("tdstore");
        let rel = path
            .strip_prefix(&root)
            .map_err(|_| format!("{} is not under {}", path.display(), root.display()))?;
        Ok(format!("{TD_STORE_DIR}/{}", path_str(rel)?))
    }

    fn stage_tree_under_tdstore(&self, tree: &Path) -> Result<PathBuf, String> {
        let base = tree
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("tree has no UTF-8 basename: {}", tree.display()))?;
        let dst = self.scratch.join("tdstore").join(base);
        remove_path_if_exists(&dst)?;
        copy_tree(tree, &dst).map_err(|e| {
            format!(
                "stage {} under tdstore failed ({} -> {}): {e}",
                base,
                tree.display(),
                dst.display()
            )
        })?;
        Ok(dst)
    }

    fn curated_path(&self) -> Result<String, String> {
        let dir = self.fresh_scratch("curated-bin")?;
        if let Some(paths) = env::var_os("PATH") {
            for path in env::split_paths(&paths) {
                if !path.is_dir() {
                    continue;
                }
                for entry in read_dir_sorted(&path)? {
                    let Some(name) = entry
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    if is_bad_build_path_tool(&name) {
                        continue;
                    }
                    let link = dir.join(&name);
                    if link.exists() {
                        continue;
                    }
                    let _ = symlink(&entry, link);
                }
            }
        }
        path_str(&dir).map(str::to_string)
    }

    fn store_ns_output(&self, argv: &[&str], stdin: Option<&str>) -> Result<String, String> {
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

    fn builder_command(&self) -> Command {
        let mut cmd = Command::new(&self.tb);
        cmd.current_dir(&self.root)
            .env("TD_STORE_DIR", TD_STORE_DIR)
            .env("TD_BUILDER_PATH", &self.builder_path)
            .env("TD_BUILDER_STORE", &self.builder_store)
            .env("TD_BUILDER_DB", &self.builder_db);
        cmd
    }
}

fn require_exec(path: &Path, label: &str) -> Result<(), String> {
    if is_executable(path) {
        Ok(())
    } else {
        Err(format!("{label} is not executable: {}", path.display()))
    }
}

fn require_file(path: &Path, label: &str) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("{label} is missing: {}", path.display()))
    }
}

fn reject_embedded_gnu_store(path: &Path) -> Result<(), String> {
    if file_contains(path, b"/gnu/store")? {
        Err(format!("{} contains /gnu/store bytes", path.display()))
    } else {
        Ok(())
    }
}

fn file_contains(path: &Path, needle: &[u8]) -> Result<bool, String> {
    if needle.is_empty() {
        return Ok(true);
    }
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(bytes.windows(needle.len()).any(|window| window == needle))
}

fn write_x86_probe_sources(work: &Path) -> Result<(), String> {
    fs::write(work.join("c.c"), c_probe_source()).map_err(|e| format!("write C probe: {e}"))?;
    fs::write(work.join("cpp.cc"), cpp_probe_source())
        .map_err(|e| format!("write C++ probe: {e}"))?;
    Ok(())
}

fn c_probe_source() -> &'static str {
    "#include <stdio.h>\n#include <unistd.h>\nint main(void) { puts(\"C-RAN\"); puts(access(\"/gnu/store\", F_OK) == 0 ? \"GNU-PRESENT\" : \"GNU-ABSENT\"); return 0; }\n"
}

fn cpp_probe_source() -> &'static str {
    "#include <iostream>\n#include <unistd.h>\n#include <vector>\nint main() { std::vector<int> v; for (int i = 0; i < 43; ++i) v.push_back(i); if (v[42] != 42) return 1; std::cout << \"CPP-RAN\\n\" << (access(\"/gnu/store\", F_OK) == 0 ? \"GNU-PRESENT\\n\" : \"GNU-ABSENT\\n\"); return 0; }\n"
}

fn compile_x86_64_cross(
    xbu: &Path,
    xgcc: &Path,
    xglibc: &Path,
    glibc_logical: &str,
    work: &Path,
    compiler: &str,
    src: &str,
    out: &str,
) -> Result<(), String> {
    let bin_name = match compiler {
        "gcc" => "x86_64-pc-linux-gnu-gcc",
        "g++" => "x86_64-pc-linux-gnu-g++",
        other => return Err(format!("unknown x86_64 cross compiler `{other}'")),
    };
    let mut cmd = Command::new(xgcc.join("bin").join(bin_name));
    cmd.current_dir(work)
        .env("PATH", prepend_path(&xbu.join("bin"))?)
        .arg("-isystem")
        .arg(xglibc.join("include"))
        .arg(format!("-B{}", xglibc.join("lib").display()))
        .arg(format!("-L{}", xglibc.join("lib").display()))
        .arg("-static-libgcc");
    if compiler == "g++" {
        cmd.arg("-static-libstdc++");
    }
    cmd.arg("-Wl,--dynamic-linker")
        .arg(format!("-Wl,{glibc_logical}/lib/ld-linux-x86-64.so.2"))
        .arg("-Wl,--enable-new-dtags")
        .arg("-Wl,-rpath")
        .arg(format!("-Wl,{glibc_logical}/lib"))
        .arg("-o")
        .arg(out)
        .arg(src);
    command_ok(&mut cmd, &format!("x86_64 cross {compiler} compile"))
}

fn compile_x86_64_native_host(
    xnbu: &Path,
    xngcc: &Path,
    xglibc: &Path,
    glibc_logical: &str,
    work: &Path,
    compiler: &str,
    src: &str,
    out: &str,
) -> Result<(), String> {
    let bin_name = match compiler {
        "gcc" | "g++" => compiler,
        other => return Err(format!("unknown x86_64 native compiler `{other}'")),
    };
    let mut cmd = Command::new(xngcc.join("bin").join(bin_name));
    cmd.current_dir(work)
        .env("PATH", prepend_path(&xnbu.join("bin"))?)
        .arg("-idirafter")
        .arg(xglibc.join("include"))
        .arg(format!("-B{}", xnbu.join("bin").display()))
        .arg(format!("-B{}", xglibc.join("lib").display()))
        .arg(format!("-L{}", xglibc.join("lib").display()))
        .arg("-static-libgcc");
    if compiler == "g++" {
        cmd.arg("-static-libstdc++");
    }
    cmd.arg("-Wl,--dynamic-linker")
        .arg(format!("-Wl,{glibc_logical}/lib/ld-linux-x86-64.so.2"))
        .arg("-Wl,--enable-new-dtags")
        .arg("-Wl,-rpath")
        .arg(format!("-Wl,{glibc_logical}/lib"))
        .arg("-o")
        .arg(out)
        .arg(src);
    command_ok(&mut cmd, &format!("x86_64 native host {compiler} compile"))
}

fn compile_x86_64_native_in_ownroot(
    runner: &RecipeCheckRunner,
    ngcc_logical: &str,
    nbu_logical: &str,
    glibc_logical: &str,
    compiler: &str,
    out_stem: &str,
    source: &str,
) -> Result<(), String> {
    let (lang, src_name, run_marker, class_marker, machine_marker, interp_marker) =
        match (compiler, out_stem) {
            ("gcc", "c") => ("c", "probe.c", "C-RAN", "C-ELF64", "C-MACH", "C-INTERP"),
            ("g++", "cpp") => (
                "c++",
                "probe.cc",
                "CPP-RAN",
                "CPP-ELF64",
                "CPP-MACH",
                "CPP-INTERP",
            ),
            (other_compiler, other_stem) => {
                return Err(format!(
                    "unsupported native own-root probe {other_compiler}/{other_stem}"
                ))
            }
        };
    let stdcxx = if compiler == "g++" {
        " -static-libstdc++"
    } else {
        ""
    };
    let out_path = format!("/tmp/td-x86-{out_stem}");
    let script = format!(
        "set -eu\n\
         export PATH={ngcc_logical}/bin:{nbu_logical}/bin\n\
         cd /tmp\n\
         cat > {src_name} <<'TD_X86_PROBE'\n\
{source}\
TD_X86_PROBE\n\
         {ngcc_logical}/bin/{compiler} -x {lang} \
           -idirafter {glibc_logical}/include \
           -B{nbu_logical}/bin \
           -B{glibc_logical}/lib \
           -L{glibc_logical}/lib \
           -static-libgcc{stdcxx} \
           -Wl,--dynamic-linker \
           -Wl,{glibc_logical}/lib/ld-linux-x86-64.so.2 \
           -Wl,--enable-new-dtags \
           -Wl,-rpath \
           -Wl,{glibc_logical}/lib \
           -o {out_path} {src_name}\n\
         hdr=$({nbu_logical}/bin/readelf -h {out_path})\n\
         case \"$hdr\" in *ELF64*) echo {class_marker}=ELF64 ;; *) echo {class_marker}=BAD ;; esac\n\
         case \"$hdr\" in *X86-64*|*x86-64*) echo {machine_marker}=x86-64 ;; *) echo {machine_marker}=BAD ;; esac\n\
         phdr=$({nbu_logical}/bin/readelf -l {out_path})\n\
         case \"$phdr\" in *\"{glibc_logical}/lib/ld-linux-x86-64.so.2\"*) echo {interp_marker}=OK ;; *) echo {interp_marker}=BAD ;; esac\n\
         {out_path}\n\
         [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT\n"
    );
    let out = runner.store_ns_bash(&script)?;
    require_output_line(
        &out,
        &format!("{class_marker}=ELF64"),
        "native own-root compiler did not emit ELF64",
    )?;
    require_output_line(
        &out,
        &format!("{machine_marker}=x86-64"),
        "native own-root compiler did not emit x86-64",
    )?;
    require_output_line(
        &out,
        &format!("{interp_marker}=OK"),
        "native own-root compiler did not use the /td/store x86_64 loader",
    )?;
    require_output_line(&out, run_marker, "native own-root probe did not run")?;
    require_output_line(
        &out,
        "GNU-ABSENT",
        "/gnu/store is present in native own-root probe",
    )
}

fn assert_elf64_x86_64(readelf: &Path, bin: &Path, label: &str) -> Result<(), String> {
    let mut cmd = Command::new(readelf);
    cmd.arg("-h").arg(bin);
    let out = command_output(&mut cmd, &format!("readelf -h {label}"))?;
    if !out.contains("ELF64") {
        return Err(format!("{label} is not ELF64"));
    }
    if !(out.contains("X86-64") || out.contains("x86-64")) {
        return Err(format!("{label} is not x86-64"));
    }
    Ok(())
}

fn assert_interp(readelf: &Path, bin: &Path, glibc_logical: &str) -> Result<(), String> {
    let mut cmd = Command::new(readelf);
    cmd.arg("-l").arg(bin);
    let out = command_output(&mut cmd, "readelf -l probe")?;
    let expected = format!("{glibc_logical}/lib/ld-linux-x86-64.so.2");
    if out.contains(&expected) {
        Ok(())
    } else {
        Err(format!(
            "{} interp does not reference {expected}",
            bin.display()
        ))
    }
}

fn require_output_line(text: &str, line: &str, msg: &str) -> Result<(), String> {
    if text.lines().any(|got| got == line) {
        Ok(())
    } else {
        Err(format!("{msg}; output was:\n{text}"))
    }
}

fn find_first_named(root: &Path, name: &str) -> Result<PathBuf, String> {
    find_first_named_opt(root, name)?
        .ok_or_else(|| format!("no file named {name} under {}", root.display()))
}

fn find_first_named_opt(root: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    for path in read_dir_sorted(root)? {
        if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Ok(Some(path));
        }
        let meta =
            fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            if let Some(found) = find_first_named_opt(&path, name)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

fn set_executable(path: &Path) -> Result<(), String> {
    let mut perms = fs::metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms).map_err(|e| format!("chmod +x {}: {e}", path.display()))
}

fn extract_line_value(text: &str, prefix: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.strip_prefix(prefix)
            .map(|value| value.trim().to_string())
    })
}

fn compile_to_assembly(
    gcc_tree: &Path,
    compiler: &str,
    work: &Path,
    src: &str,
    out: &str,
) -> Result<(), String> {
    let bin = match compiler {
        "gcc" | "g++" => gcc_tree.join("bin").join(compiler),
        other => return Err(format!("unknown codegen compiler `{other}'")),
    };
    let mut cmd = Command::new(bin);
    cmd.current_dir(work)
        .arg("-O2")
        .arg("-S")
        .arg("-frandom-seed=tdselfcodegen")
        .arg("-o")
        .arg(out)
        .arg(src);
    command_ok(&mut cmd, &format!("codegen {compiler}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    append_file_bytes(path, &mut bytes)?;
    sha256sum(&bytes)
}

fn is_bad_build_path_tool(name: &str) -> bool {
    matches!(
        name,
        "as" | "ld" | "gcc" | "g++" | "cc" | "c++" | "cpp" | "ar" | "ranlib"
    )
}

fn prepend_path(dir: &Path) -> Result<String, String> {
    let dir_s = path_str(dir)?;
    match env::var_os("PATH") {
        Some(path) if !path.is_empty() => {
            let mut out = String::from(dir_s);
            out.push(':');
            out.push_str(&path.to_string_lossy());
            Ok(out)
        }
        _ => Ok(dir_s.to_string()),
    }
}

fn command_ok(cmd: &mut Command, label: &str) -> Result<(), String> {
    command_output(cmd, label).map(|_| ())
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

fn push_unique_string(v: &mut Vec<String>, item: &str) {
    if !v.iter().any(|existing| existing == item) {
        v.push(item.to_string());
    }
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
        return Ok(Some(SeedInput::Patch {
            key: key.to_string(),
            patch: patch.to_string(),
        }));
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
    let got = sha256sum(&bytes)?;
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

fn sha256sum(bytes: &[u8]) -> Result<String, String> {
    let mut cmd = Command::new("sha256sum");
    let out = command_output_with_stdin_bytes(&mut cmd, "sha256sum", bytes)?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| "sha256sum produced no digest".to_string())
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

fn map_value_in(map: &Path, name: &str) -> Result<Option<String>, String> {
    if !map.is_file() {
        return Ok(None);
    }
    let contents = fs::read_to_string(map).map_err(|e| format!("read {}: {e}", map.display()))?;
    for line in contents.lines() {
        let mut cols = line.splitn(2, ' ');
        if cols.next() == Some(name) {
            return Ok(cols.next().map(str::trim).map(str::to_string));
        }
    }
    Ok(None)
}

fn remove_map_entry(map: &Path, name: &str) -> Result<(), String> {
    if !map.is_file() {
        return Ok(());
    }
    let contents = fs::read_to_string(map).map_err(|e| format!("read {}: {e}", map.display()))?;
    let mut out = String::new();
    for line in contents.lines() {
        let mut cols = line.splitn(2, ' ');
        if cols.next() == Some(name) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    fs::write(map, out).map_err(|e| format!("write {}: {e}", map.display()))
}

fn linux_version_from_file(file_name: &str) -> Result<String, String> {
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

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    })
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_dir() {
                fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))
            } else {
                fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
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

    #[test]
    fn selected_check_closures_resolve_their_declared_seed_inputs() {
        let recipes_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = recipes_dir.parent().unwrap();
        let runner = RecipeCheckRunner {
            root: root.to_path_buf(),
            tb: PathBuf::new(),
            builder_path: String::new(),
            builder_store: PathBuf::new(),
            builder_db: PathBuf::new(),
            lw: PathBuf::new(),
            store: PathBuf::new(),
            db: PathBuf::new(),
            recipes: PathBuf::new(),
            scratch: PathBuf::new(),
            force_cold: false,
        };

        for target in [
            "make-test",
            "busybox-test",
            "rust-toolchain",
            "gcc-x86-64-stage2",
            "gcc-x86-64-native",
        ] {
            let graph = recipe_closure(&[target]).unwrap();
            for node in graph {
                if let Some(key) = &node.recipe.source_input {
                    runner
                        .seed_input_for_recipe_source(key, &node.recipe)
                        .unwrap();
                }
                if let Some(inputs) = &node.recipe.inputs {
                    for input in inputs {
                        if catalog::lookup(input).is_none() {
                            let _ = runner.seed_input_for_recipe_input(input).unwrap();
                        }
                    }
                }
            }
        }
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
