use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use td_recipe::catalog;

const TD_STORE_DIR: &str = "/td/store";
const TOOL_SPECS: &[(&str, &str)] = &[
    ("bash", "bash"),
    ("coreutils", "ls"),
    ("sed", "sed"),
    ("grep", "grep"),
    ("gawk", "awk"),
    ("tar", "tar"),
    ("gzip", "gzip"),
    ("bzip2", "bzip2"),
    ("xz", "xz"),
    ("findutils", "find"),
    ("diffutils", "diff"),
    ("flex", "flex"),
    ("bison", "bison"),
    ("m4", "m4"),
    ("make", "make"),
    ("python", "python3"),
];

const BASE_SOURCES: &[(&str, &str)] = &[
    ("mes-0.27.1", "mes-source"),
    ("nyacc-1.00.2", "nyacc"),
    ("tcc-0.9.26", "tcc-source"),
    ("make-3.80", "make-mesboot0-source"),
    ("patch-2.5.9", "patch-mesboot-source"),
    ("binutils-2.20.1a", "binutils-mesboot-source"),
    ("gcc-core-2.95.3", "gcc-core-source"),
    ("glibc-2.2.5", "glibc-mesboot0-source"),
    ("make-3.82", "make-mesboot-source"),
    ("gcc-core-4.6.4", "gcc-464-core"),
    ("gcc-g++-4.6.4", "gcc-464-gpp"),
    ("gmp-4.3.2", "gmp"),
    ("mpfr-2.4.2", "mpfr"),
    ("mpc-1.0.3", "mpc"),
    ("gawk-3.1.8", "gawk-mesboot-source"),
    ("glibc-mesboot-2.16.0", "glibc-216-source"),
    ("gcc-4.9.4", "gcc-494-source"),
    ("gcc-14.3.0", "gcc-14-source"),
    ("gcc14-gmp-6.3.0", "gmp63"),
    ("gcc14-mpfr-4.2.1", "mpfr421"),
    ("gcc14-mpc-1.3.1", "mpc131"),
    ("binutils-2.44", "binutils-244-source"),
    ("glibc-2.41", "glibc-241-source"),
];

const PATCHES: &[&str] = &[
    "binutils-boot-2.20.1a",
    "gcc-boot-2.95.3",
    "glibc-boot-2.2.5",
    "glibc-bootstrap-system-2.2.5",
    "gcc-boot-4.6.4",
    "glibc-boot-2.16.0",
    "glibc-bootstrap-system-2.16.0",
];

const CROSS_RECIPES: &[&str] = &[
    "stage0",
    "mes",
    "tcc",
    "make-mesboot0",
    "patch-mesboot",
    "binutils-mesboot0",
    "gcc-core-mesboot0",
    "mesboot-headers",
    "glibc-mesboot0",
    "gcc-mesboot0",
    "binutils-mesboot1",
    "make-mesboot",
    "gcc-mesboot1",
    "binutils-mesboot",
    "gawk-mesboot",
    "glibc-mesboot",
    "gcc-mesboot",
    "glibc-mesboot-shared",
    "gcc-14",
    "binutils-244",
    "glibc-241",
    "binutils-x86-64",
    "gcc-x86-64-stage1",
    "glibc-x86-64",
    "gcc-x86-64-stage2",
];

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
    match stem {
        "make-test" => runner.run_make_test(),
        "busybox-test" => runner.run_busybox_test(),
        "rust-toolchain" => runner.run_rust_toolchain_check(),
        other => Err(format!(
            "{other} has a recipe-owned check, but no Rust check-runner implementation yet"
        )),
    }
}

fn usage() -> String {
    "usage: check-run STEM [pr|daily|all] [INDEX]".to_string()
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
}

impl RecipeCheckRunner {
    fn new(root: PathBuf) -> Result<Self, String> {
        let stage0_base = env::var_os("TD_STAGE0_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".td-build-cache/td-shell"));
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
        })
    }

    fn lock_path(&self) -> PathBuf {
        self.lw.with_extension("lock")
    }

    fn setup(&self) -> Result<(), String> {
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("mkdir {}: {e}", self.scratch.display()))?;
        self.write_tools_map()?;
        let pinsum = self.setup_pinsum()?;
        let setup_ok = self.lw.join("setup-ok");
        let warm = fs::read_to_string(&setup_ok)
            .map(|s| s == pinsum)
            .unwrap_or(false)
            && self.lw.join("srcs.map").is_file();
        if warm {
            if !self.map_has("linux-headers-x86-64")? {
                self.intern_linux_headers("linux-headers-x86-64", "x86_64")?;
            }
            self.stage_tdstore()?;
            return Ok(());
        }

        remove_path_if_exists(&self.store)?;
        remove_path_if_exists(&self.db)?;
        remove_path_if_exists(&self.lw.join("srcs.map"))?;
        remove_path_if_exists(&setup_ok)?;
        fs::create_dir_all(&self.store)
            .map_err(|e| format!("mkdir {}: {e}", self.store.display()))?;
        File::create(self.lw.join("srcs.map")).map_err(|e| format!("create srcs.map: {e}"))?;

        for (stem, name) in BASE_SOURCES {
            self.intern_source(name, stem)?;
        }
        self.intern_linux_headers("linux-headers", "i386")?;
        self.intern_linux_headers("linux-headers-x86-64", "x86_64")?;
        for patch in PATCHES {
            let file = self
                .root
                .join("seed")
                .join("patches")
                .join(format!("{patch}.patch"));
            if !file.is_file() {
                return Err(format!("ladder: missing {}", file.display()));
            }
            let path = self.store_add_recursive(&format!("patch-{patch}"), &file)?;
            self.append_src_map(&format!("patch-{patch}"), &path)?;
        }
        let stage0 = self.root.join("seed/stage0");
        let path = self.store_add_recursive("stage0-source", &stage0)?;
        self.append_src_map("stage0-source", &path)?;
        fs::write(&setup_ok, pinsum).map_err(|e| format!("write {}: {e}", setup_ok.display()))?;
        self.stage_tdstore()
    }

    fn write_tools_map(&self) -> Result<(), String> {
        let path = self.lw.join("tools.map");
        let mut file =
            File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        for (name, probe) in TOOL_SPECS {
            let root = self.tool_root(name, probe)?;
            writeln!(file, "{name} {}", root.display())
                .map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn setup_pinsum(&self) -> Result<String, String> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ladder-setup-v3\n");
        for file in files_with_suffix(&self.root.join("seed/sources"), ".lock")? {
            append_file_bytes(&file, &mut bytes)?;
        }
        for file in files_with_suffix(&self.root.join("seed/patches"), ".patch")? {
            append_file_bytes(&file, &mut bytes)?;
        }
        let files_out =
            self.builder_output(&["files", "seed/stage0"], "td-builder files seed/stage0")?;
        for line in files_out.lines() {
            let path = self.root.join(line);
            append_file_bytes(&path, &mut bytes)?;
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

    fn intern_source(&self, intern_name: &str, lock_stem: &str) -> Result<(), String> {
        let lock = self.source_lock(lock_stem)?;
        let file_name = lock_value(&lock, "file")?;
        let file = self.root.join(".td-build-cache/sources").join(&file_name);
        if !file.is_file() {
            return Err(format!(
                "ladder: pinned tarball not warm ({}) - run 'td-feed warm sources'",
                file.display()
            ));
        }
        let path = self.store_add_recursive(intern_name, &file)?;
        self.append_src_map(intern_name, &path)
    }

    fn intern_extra(&self, intern_name: &str, lock_stem: &str) -> Result<(), String> {
        if self.map_has(intern_name)? {
            return Ok(());
        }
        self.intern_source(intern_name, lock_stem)?;
        let path = self.map_value(intern_name)?;
        self.stage_store_path(&path)
    }

    fn intern_linux_headers(&self, intern_name: &str, arch: &str) -> Result<(), String> {
        let lock = self.source_lock("linux-")?;
        let file_name = lock_value(&lock, "file")?;
        let version = linux_version_from_file(&file_name)?;
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

    fn emit_recipes(&self, names: &[&str]) -> Result<(), String> {
        fs::create_dir_all(&self.recipes)
            .map_err(|e| format!("mkdir {}: {e}", self.recipes.display()))?;
        for name in names {
            let recipe = catalog::lookup(name)
                .ok_or_else(|| format!("ladder: no td recipe for `{name}'"))?;
            fs::write(
                self.recipes.join(format!("{name}.json")),
                recipe.to_json().to_canonical(),
            )
            .map_err(|e| format!("ladder: emit {name}: {e}"))?;
        }
        Ok(())
    }

    fn emit_cross(&self) -> Result<(), String> {
        self.emit_recipes(CROSS_RECIPES)
    }

    fn build_plan(&self, target: &str) -> Result<(), String> {
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
            .map_err(|e| format!("write build-plan stdout: {e}"))
    }

    fn ladder_out(&self, rung: &str) -> Result<PathBuf, String> {
        let prefix = format!("STEP {rung} ");
        let mut got = None;
        let mut files = read_dir_sorted(&self.lw)?;
        files.retain(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("build-") && n.ends_with(".out"))
                .unwrap_or(false)
        });
        for file in files {
            let contents =
                fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix(&prefix) {
                    got = Some(rest.trim().to_string());
                }
            }
        }
        let path = got.ok_or_else(|| format!("ladder: no STEP output recorded for {rung}"))?;
        let base = path_basename_str(&path)?;
        Ok(self.scratch.join("tdstore").join(base))
    }

    fn run_x86_64_native(&self) -> Result<(), String> {
        self.emit_cross()?;
        self.emit_recipes(&["binutils-x86-64-native", "gcc-x86-64-native"])?;
        self.build_plan("gcc-x86-64-native")?;
        let xnbu = self.ladder_out("binutils-x86-64-native")?;
        let xngcc = self
            .ladder_out("gcc-x86-64-native")?
            .join("stage/td/store/gcc-14.3.0-x86_64-native");
        if !xnbu.is_dir() {
            return Err(format!(
                "run_x86_64_native: no native binutils tree ({})",
                xnbu.display()
            ));
        }
        if !xngcc.is_dir() {
            return Err(format!(
                "run_x86_64_native: no native gcc tree ({})",
                xngcc.display()
            ));
        }
        println!(
            "   [ladder] x86_64 NATIVE toolchain via build-plan --auto: cross graph -> native binutils 2.44 -> native gcc 14.3.0 (ELF64 x86_64)"
        );
        Ok(())
    }

    fn run_x86_64_rust_toolchain(&self) -> Result<PathBuf, String> {
        self.emit_cross()?;
        self.intern_extra("rust-toolchain-source", "rust-1.96.0")?;
        self.intern_extra("zlib-x86-64-source", "zlib-1.3.1")?;
        self.emit_recipes(&["zlib-x86-64", "rust-toolchain"])?;
        self.build_plan("rust-toolchain")?;
        let rust_tree = self.ladder_out("rust-toolchain")?;
        println!(
            "   [ladder] x86_64 rust-toolchain via build-plan --auto: cross toolchain -> zlib-x86-64 -> rust-toolchain (relinked rustc/cargo tree {})",
            rust_tree.display()
        );
        Ok(rust_tree)
    }

    fn run_make_test(&self) -> Result<(), String> {
        self.run_x86_64_native()?;
        self.intern_extra("make-x86-64-source", "make-4.4.1")?;
        self.emit_recipes(&["make-x86-64", "make-test"])?;
        self.build_plan("make-test")?;
        println!(
            "PASS: make-test - GNU make 4.4.1 built on the native /td/store toolchain drove a real build in the recipe sandbox"
        );
        Ok(())
    }

    fn run_busybox_test(&self) -> Result<(), String> {
        self.run_x86_64_native()?;
        self.intern_extra("make-x86-64-source", "make-4.4.1")?;
        self.intern_extra("busybox-x86-64-source", "busybox-1.37.0")?;
        self.emit_recipes(&["make-x86-64", "busybox-x86-64", "busybox-test"])?;
        self.build_plan("busybox-test")?;
        println!(
            "PASS: busybox-test - BusyBox 1.37.0 built by make-x86-64 on the native /td/store toolchain ran installed sh/ls/grep/sed applet links"
        );
        Ok(())
    }

    fn run_rust_toolchain_check(&self) -> Result<(), String> {
        let rust_tree = self.run_x86_64_rust_toolchain()?;
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

    fn source_lock(&self, stem: &str) -> Result<PathBuf, String> {
        let mut matches = Vec::new();
        for file in read_dir_sorted(&self.root.join("seed/sources"))? {
            let name = match file.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if name.starts_with(stem) && name.ends_with(".lock") {
                matches.push(file);
            }
        }
        matches
            .first()
            .cloned()
            .ok_or_else(|| format!("ladder: no seed/sources lock for {stem}"))
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

    fn builder_output(&self, args: &[&str], label: &str) -> Result<String, String> {
        let mut cmd = self.builder_command();
        for arg in args {
            cmd.arg(arg);
        }
        command_output(&mut cmd, label)
    }
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

fn lock_value(lock: &Path, key: &str) -> Result<String, String> {
    let contents = fs::read_to_string(lock).map_err(|e| format!("read {}: {e}", lock.display()))?;
    for line in contents.lines() {
        let mut cols = line.splitn(2, ' ');
        if cols.next() == Some(key) {
            return cols
                .next()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("{}: key {key} has no value", lock.display()));
        }
    }
    Err(format!("{}: no {key} entry", lock.display()))
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
