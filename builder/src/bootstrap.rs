//! Structured source-bootstrap recipes — the `tests/bootstrap-*.sh` drivers as
//! TYPED Rust data + one shared leg runner (rust-migration C2,
//! "C. Scripts → Rust"; sibling of C1 `affected.rs`).
//!
//! Every `tests/bootstrap-*.sh` is the SAME leg skeleton:
//!
//! ```text
//! [pinned-input]  the input bytes == the recipe-owned source pin
//! build           rung-specific: drive the seed / prior-rung tools over the source
//! [no-guix]       the artifact carries no /gnu/store bytes; the build ran guix-off-env
//! [behavioral]    the artifact does its job (assembles, evaluates, returns 42, …)
//! [repro]         a second independent build is byte-identical
//! ```
//!
//! Only **build** and **behavioral** differ per rung — the shell copies the other
//! three legs once per script. Here they are ONE implementation ([`run`]); each
//! rung is a [`Recipe`] value. The build steps run with a SCRUBBED env
//! ([`scrubbed`] = `Command::env_clear`, the Rust `env -i`) — the "guix off env"
//! proof; the `[no-guix]` leg then confirms no `/gnu/store` byte reached the output.
//!
//! These gates are **all-durable** (the seed chain IS the irreducible bottom; there
//! is no guix oracle), so the Rust runner asserts exactly the durable legs the shell
//! asserts. The shell `tests/bootstrap-*.sh` stay the live driver + removable
//! differential oracle (no cutover — same scoping as C1 #226).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::sha256::sha256_file;
use crate::tar;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const HEX0_PIN: &str = "66c95985e668f20f2465c2b876f83fef066fd7c8c2dd3adb51a969f2d7120c8b";
const KAEM_PIN: &str = "153b8915b73bd07132b59538d10fe53d26578eb160a67db72af07aaa61c51b3b";
const STAGE0_SOURCE_KEY: &str = "stage0-source";
const MES_SOURCE_KEY: &str = "mes-source";

/// Where a recipe finds its warmed-source cache (`.td-build-cache/sources/`,
/// populated by
/// `td-feed warm sources` in check.sh's HOST prelude — the offline loop
/// never egresses).
pub struct Ctx {
    pub sources_dir: PathBuf,
    source_pins: Vec<SourcePin>,
}

impl Ctx {
    /// Default context: repo root = CWD (the gate runs from there), sources =
    /// `<root>/.td-build-cache/sources` unless `TD_SOURCES_DIR` overrides it.
    pub fn discover() -> Result<Ctx, String> {
        let repo_root = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
        Ctx::rooted(repo_root)
    }
    pub fn rooted(repo_root: PathBuf) -> Result<Ctx, String> {
        let sources_dir = std::env::var_os("TD_SOURCES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root.join(".td-build-cache/sources"));
        let source_pins = load_recipe_source_pins(&repo_root)?;
        Ok(Ctx {
            sources_dir,
            source_pins,
        })
    }
    fn source_pin(&self, key: &str) -> Result<&SourcePin, String> {
        self.source_pins
            .iter()
            .find(|pin| pin.key == key)
            .ok_or_else(|| format!("recipe source pin `{key}` is missing from td-recipe-eval source-pins"))
    }
}

fn load_recipe_source_pins(repo_root: &Path) -> Result<Vec<SourcePin>, String> {
    let out = recipe_source_pins_output(repo_root)?;
    if !out.status.success() {
        return Err(format!(
            "td-recipe-eval source-pins failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text =
        String::from_utf8(out.stdout).map_err(|e| format!("source-pins output not UTF-8: {e}"))?;
    parse_source_pins(&text)
}

fn recipe_source_pins_output(repo_root: &Path) -> Result<Output, String> {
    if let Some(eval) = std::env::var_os("TD_RECIPE_EVAL").filter(|v| !v.is_empty()) {
        let eval = PathBuf::from(eval);
        if !is_executable_file(&eval) {
            return Err(format!(
                "TD_RECIPE_EVAL is not an executable file: {}",
                eval.display()
            ));
        }
        return Command::new(&eval)
            .arg("source-pins")
            .current_dir(repo_root)
            .output()
            .map_err(|e| format!("spawn {} source-pins: {e}", eval.display()));
    }

    if !repo_root.join("recipes/Cargo.toml").is_file() {
        return Err(format!(
            "no recipes/Cargo.toml under {} to resolve recipe source pins",
            repo_root.display()
        ));
    }
    Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg("recipes/Cargo.toml")
        .arg("--target-dir")
        .arg(repo_root.join(".td-build-cache/recipe-source-pins/target"))
        .arg("--bin")
        .arg("td-recipe-eval")
        .arg("--")
        .arg("source-pins")
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("spawn cargo run td-recipe-eval source-pins: {e}"))
}

fn parse_source_pins(text: &str) -> Result<Vec<SourcePin>, String> {
    let mut pins = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(key), Some(url), Some(sha256), Some(file), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(format!(
                "source-pins line {} is not four TSV fields",
                idx + 1
            ));
        };
        if key.is_empty() || url.is_empty() || sha256.is_empty() || file.is_empty() {
            return Err(format!("source-pins line {} has an empty field", idx + 1));
        }
        if file.contains('/') {
            return Err(format!(
                "source-pins line {} has non-basename file `{file}`",
                idx + 1
            ));
        }
        if pins.iter().any(|pin: &SourcePin| pin.key == key) {
            return Err(format!("source-pins line {} duplicates key `{key}`", idx + 1));
        }
        pins.push(SourcePin {
            key: key.to_string(),
            url: url.to_string(),
            sha256: sha256.to_lowercase(),
            file: file.to_string(),
        });
    }
    if pins.is_empty() {
        return Err("td-recipe-eval source-pins returned no pins".into());
    }
    Ok(pins)
}

/// A pinned input — the `[pinned-input]` leg's flavours.
pub enum Pin {
    /// A td-fetched upstream stage0 source tarball, plus the two binary seed
    /// files inside matching their per-file pins.
    Stage0Source { key: &'static str },
    /// A td-fetched tarball keyed by a recipe-owned fixed-output source pin:
    /// the warmed `.td-build-cache/sources/<file>` must match the pin sha256.
    Source { key: &'static str },
}

/// A durable post-build assertion — the rung-specific `[behavioral]` (and, for the
/// seed, `[self-reproduction]`) legs. `desc` carries its own `[tag]`.
pub struct Check {
    pub desc: &'static str,
    pub run: fn(&Ctx, &Built) -> Result<(), String>,
}

/// A rung's build output: the root dir holding the artifacts (relative paths).
pub struct Built {
    pub dir: PathBuf,
}

/// One source-bootstrap rung, declared as data.
pub struct Recipe {
    pub name: &'static str,
    pub brick: u32,
    pub pins: Vec<Pin>,
    /// Build the rung with a scrubbed env; returns the output root.
    pub build: fn(&Ctx) -> Result<Built, String>,
    /// Artifacts (relative to `Built.dir`) the `[no-guix]` + `[repro]` legs cover.
    pub artifacts: Vec<&'static str>,
    pub checks: Vec<Check>,
    /// The trailing PASS summary clause.
    pub summary: &'static str,
}

/// Look up a recipe by name.
pub fn lookup(name: &str) -> Option<Recipe> {
    match name {
        "seed" => Some(seed_recipe()),
        "mes" => Some(mes_recipe()),
        _ => None,
    }
}

/// Every migrated rung, in brick order.
pub fn all_names() -> &'static [&'static str] {
    &["seed", "mes"]
}

// --- the shared leg runner -------------------------------------------------------

fn leg(report: &mut String, body: &str) {
    report.push_str("   ");
    report.push_str(body);
    report.push('\n');
}

/// Run one recipe: pinned-input → build → no-guix → checks → repro → PASS. Returns
/// the leg-by-leg PASS report (the caller prints it); `Err` on the first failed leg.
pub fn run(cx: &Ctx, recipe: &Recipe) -> Result<String, String> {
    let mut report = String::new();

    // [pinned-input]
    for pin in &recipe.pins {
        let msg = verify_pin(cx, pin)?;
        leg(&mut report, &format!("[pinned-input] {msg}"));
    }

    // build (r1) — a scrubbed-env build, the artifacts must exist + be non-empty.
    let r1 = (recipe.build)(cx)?;
    let _g1 = Cleanup(r1.dir.clone());
    require_artifacts(&r1, &recipe.artifacts)?;

    // [no-guix] — no /gnu/store byte reached any artifact (the build ran env-cleared).
    for a in &recipe.artifacts {
        if contains_gnu_store(&r1.dir.join(a)).map_err(io_err("read artifact"))? {
            return Err(format!(
                "{a} contains /gnu/store bytes — not a clean non-guix build"
            ));
        }
    }
    leg(
        &mut report,
        &format!(
            "[no-guix] the build ran with guix/Guile scrubbed from env (env_clear); no /gnu/store byte in {}",
            recipe.artifacts.join(", ")
        ),
    );

    // checks ([self-reproduction], [behavioral], …)
    for c in &recipe.checks {
        (c.run)(cx, &r1)?;
        leg(&mut report, c.desc);
    }

    // [repro] — a second independent build is byte-identical.
    let r2 = (recipe.build)(cx)?;
    let _g2 = Cleanup(r2.dir.clone());
    require_artifacts(&r2, &recipe.artifacts)?;
    for a in &recipe.artifacts {
        let s1 = sha256_file(&r1.dir.join(a)).map_err(io_err("sha r1"))?;
        let s2 = sha256_file(&r2.dir.join(a)).map_err(io_err("sha r2"))?;
        if s1 != s2 {
            return Err(format!("{a} is NOT reproducible — r1={s1} r2={s2}"));
        }
    }
    leg(
        &mut report,
        "[repro] two independent builds produce byte-identical artifacts (reproducible)",
    );

    report.push_str(&format!(
        "PASS: source-bootstrap brick {} ({}) — {}\n",
        recipe.brick, recipe.name, recipe.summary
    ));
    Ok(report)
}

fn require_artifacts(b: &Built, artifacts: &[&str]) -> Result<(), String> {
    for a in artifacts {
        let p = b.dir.join(a);
        match fs::metadata(&p) {
            Ok(m) if m.len() > 0 => {}
            Ok(_) => return Err(format!("the build produced an EMPTY artifact: {a}")),
            Err(_) => return Err(format!("the build produced no artifact: {a}")),
        }
    }
    Ok(())
}

// --- pinned-input verification ---------------------------------------------------

fn verify_pin(cx: &Ctx, pin: &Pin) -> Result<String, String> {
    match pin {
        Pin::Stage0Source { key } => {
            let pin = cx.source_pin(key)?;
            let (_, _) = verified_source_tarball(cx, pin)?;
            let root = unpack_stage0_source(cx, pin)?;
            let _cleanup = Cleanup(root.clone());
            verify_seed_binary(&root, "bootstrap-seeds/POSIX/AMD64/hex0-seed", HEX0_PIN)?;
            verify_seed_binary(
                &root,
                "bootstrap-seeds/POSIX/AMD64/kaem-optional-seed",
                KAEM_PIN,
            )?;
            Ok(format!(
                "td-fetched {} matches the source pin sha256 ({}) and contains the pinned binary seeds — auditable, NOT guix-built, no /gnu/store bytes",
                pin.file, pin.sha256
            ))
        }
        Pin::Source { key } => {
            let pin = cx.source_pin(key)?;
            let (pin, _) = verified_source_tarball(cx, pin)?;
            Ok(format!(
                "td-fetched {} matches the source pin sha256 ({}) — building from the pinned upstream bytes, not vendored/guix-fetched",
                pin.file, pin.sha256
            ))
        }
    }
}

fn verified_source_tarball<'a>(
    cx: &Ctx,
    pin: &'a SourcePin,
) -> Result<(&'a SourcePin, PathBuf), String> {
    let tarball = cx.sources_dir.join(&pin.file);
    if !tarball.exists() {
        return Err(format!(
            "the pinned tarball is not warm ({}) — run 'td-feed warm sources' to td-fetch {} (needs network); check.sh's prelude does this",
            tarball.display(),
            pin.url
        ));
    }
    let got = sha256_file(&tarball).map_err(|e| format!("read {}: {e}", tarball.display()))?;
    if got != pin.sha256 {
        return Err(format!(
            "warmed {} sha256 {got} != source pin {} — corrupt fetch or stale source pin",
            pin.file, pin.sha256
        ));
    }
    Ok((pin, tarball))
}

fn unpack_stage0_source(cx: &Ctx, pin: &SourcePin) -> Result<PathBuf, String> {
    let (pin, tarball) = verified_source_tarball(cx, pin)?;
    let out = scratch_dir("td-bootstrap-stage0").map_err(io_err("scratch dir"))?;
    tar::extract_tar_gz(&tarball, &out)?;
    let root = single_subdir(&out)?;
    clean_stage0_build_dirs(&root)?;
    let seed = root.join("bootstrap-seeds/POSIX/AMD64/hex0-seed");
    let kaem = root.join("AMD64/mescc-tools-seed-kaem.kaem");
    if !seed.is_file() || !kaem.is_file() {
        return Err(format!(
            "{} did not unpack to the expected stage0 source tree",
            pin.file
        ));
    }
    Ok(root)
}

fn verify_seed_binary(root: &Path, rel: &str, sha256: &str) -> Result<(), String> {
    let path = root.join(rel);
    let got = sha256_file(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if got != sha256 {
        return Err(format!("{rel} sha256 {got} != pin {sha256}"));
    }
    if contains_gnu_store(&path).map_err(|e| format!("scan {}: {e}", path.display()))? {
        return Err(format!(
            "{rel} contains /gnu/store bytes — not a clean non-guix seed"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePin {
    pub key: String,
    pub url: String,
    pub sha256: String,
    pub file: String,
}

// --- the seed recipe (brick 0) ---------------------------------------------------

fn seed_recipe() -> Recipe {
    Recipe {
        name: "seed",
        brick: 0,
        pins: vec![Pin::Stage0Source {
            key: STAGE0_SOURCE_KEY,
        }],
        build: build_seed,
        artifacts: vec!["AMD64/artifact/hex0", "AMD64/artifact/kaem-0"],
        checks: vec![
            Check {
                desc: "[self-reproduction] the seed assembles its OWN hex source to a byte-identical seed (hex0 + kaem-0) — the binary seeds are verifiable from the auditable hex source, not blind trust",
                run: seed_self_reproduction,
            },
            Check {
                desc: "[behavioral] the seed-built hex0 runs as an assembler and reproduces kaem-0 — it works",
                run: seed_behavioral,
            },
        ],
        summary: "td's 229-byte auditable hex0-seed (NOT guix-built) drives the kaem seed build with guix/Guile off env, producing a full hex0 + kaem-0 that self-reproduce from their hex source, work as an assembler, and are reproducible — the irreducible guix-free bottom of the /td/store toolchain",
    }
}

/// Unpack the pinned stage0 source to a fresh scratch dir, chmod the two seeds,
/// and run the FIRST kaem step (seed → `AMD64/artifact/{hex0,kaem-0}`)
/// env-cleared. Returns the scratch dir. Shared by brick 0 (`build_seed`) and
/// brick 2's toolchain (`mes_toolchain`, which drives a second kaem step on top).
fn seed_stage0_tree(cx: &Ctx) -> Result<PathBuf, String> {
    let out = unpack_stage0_source(cx, cx.source_pin(STAGE0_SOURCE_KEY)?)?;
    let amd = "bootstrap-seeds/POSIX/AMD64";
    make_executable(&out.join(format!("{amd}/hex0-seed"))).map_err(io_err("chmod hex0-seed"))?;
    make_executable(&out.join(format!("{amd}/kaem-optional-seed")))
        .map_err(io_err("chmod kaem-optional-seed"))?;

    let kaem = out.join(format!("{amd}/kaem-optional-seed"));
    let status = scrubbed(&kaem)
        .arg("./AMD64/mescc-tools-seed-kaem.kaem")
        .current_dir(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(io_err("exec kaem seed build"))?;
    if !status.success() {
        return Err(format!("the seed kaem build failed in {}", out.display()));
    }
    Ok(out)
}

/// Brick 0 build: the seed stage0 tree, producing `AMD64/artifact/{hex0,kaem-0}`.
/// (Ported from `run_seed_build` in the former tests/bootstrap-seed.sh.)
fn build_seed(cx: &Ctx) -> Result<Built, String> {
    Ok(Built {
        dir: seed_stage0_tree(cx)?,
    })
}

fn seed_self_reproduction(_cx: &Ctx, b: &Built) -> Result<(), String> {
    let hex0 = sha256_file(&b.dir.join("AMD64/artifact/hex0")).map_err(io_err("sha hex0"))?;
    if hex0 != HEX0_PIN {
        return Err(format!(
            "seed-built hex0 {hex0} != hex0-seed {HEX0_PIN} — the hex source does not assemble to the seed"
        ));
    }
    let kaem = sha256_file(&b.dir.join("AMD64/artifact/kaem-0")).map_err(io_err("sha kaem-0"))?;
    if kaem != KAEM_PIN {
        return Err(format!(
            "seed-built kaem-0 {kaem} != kaem-optional-seed {KAEM_PIN}"
        ));
    }
    Ok(())
}

fn seed_behavioral(_cx: &Ctx, b: &Built) -> Result<(), String> {
    // The seed-built hex0 assembles kaem-minimal.hex0 → kaem-0b, which must match
    // the kaem pin (the produced tool does its job). hex0 is static, invoked by
    // absolute path → safe to run env-cleared.
    let hex0 = b.dir.join("AMD64/artifact/hex0");
    make_executable(&hex0).map_err(io_err("chmod built hex0"))?;
    let out = b.dir.join("AMD64/artifact/kaem-0b");
    let status = scrubbed(&hex0)
        .arg(b.dir.join("AMD64/kaem-minimal.hex0"))
        .arg(&out)
        .stderr(Stdio::null())
        .status()
        .map_err(io_err("exec built hex0"))?;
    if !status.success() {
        return Err("the seed-built hex0 could not run as an assembler".into());
    }
    let got = sha256_file(&out).map_err(io_err("sha kaem-0b"))?;
    if got != KAEM_PIN {
        return Err(format!(
            "the seed-built hex0 assembled a wrong kaem-0 ({got})"
        ));
    }
    Ok(())
}

// --- the mes recipe (brick 2) ----------------------------------------------------

fn mes_recipe() -> Recipe {
    Recipe {
        name: "mes",
        brick: 2,
        pins: vec![Pin::Source {
            key: MES_SOURCE_KEY,
        }],
        build: build_mes,
        artifacts: vec!["bin/mes-m2"],
        checks: vec![Check {
            desc: "[behavioral] the seed-built mes-m2 evaluates Scheme from the Mes module tree: (display 'Hello,M2-mes!)→Hello,M2-mes! and (+ 1 2 3 4)→10 — a working interpreter, not just a linked ELF",
            run: mes_behavioral,
        }],
        summary: "from the seed, td drives M2-Planet + mescc-tools over the td-fetched (pinned, not vendored) GNU Mes 0.27.1 source to a working Scheme interpreter (mes-m2); it evaluates Scheme, carries no /gnu/store bytes, and is reproducible",
    }
}

/// Brick 2 toolchain: the seed stage0 tree + a SECOND kaem step
/// (`mescc-tools-mini-kaem.kaem`, driven by the seed-built kaem-0) → M2-Planet +
/// mescc-tools (`AMD64/artifact/{M2,blood-elf-0}`, `AMD64/bin/{M1,hex2}`).
/// (Ported from `build_toolchain` in the former tests/bootstrap-mes.sh.)
fn mes_toolchain(cx: &Ctx) -> Result<PathBuf, String> {
    let tc = seed_stage0_tree(cx)?;
    let kaem0 = tc.join("AMD64/artifact/kaem-0");
    make_executable(&kaem0).map_err(io_err("chmod kaem-0"))?;
    let status = scrubbed(&kaem0)
        .arg("./AMD64/mescc-tools-mini-kaem.kaem")
        .current_dir(&tc)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(io_err("exec mescc-tools-mini-kaem"))?;
    if !status.success() {
        return Err(format!(
            "the seed toolchain (M2-Planet + mescc-tools) build failed in {}",
            tc.display()
        ));
    }
    Ok(tc)
}

/// Extract the M2-Planet input units from the tarball's own `kaem.run` — the port of
/// the bootstrap-mes.sh `sed` pipeline: take the block from the `^M2-Planet` line to
/// the `-o m2/mes.M1` line, pull each `-f ${srcdest}<path>` token (the LAST on a
/// line, like the greedy `.*`), and substitute `${mes_cpu}` → `x86_64`. The build
/// recipe is upstream's, not ours; only `include/mes/config.h` + `include/arch/` are
/// generated (as `configure.sh` does for the non-system-libc path).
fn parse_m2planet_units(kaem_run: &str) -> Vec<String> {
    const MARK: &str = "-f ${srcdest}";
    let mut out = Vec::new();
    let mut in_range = false;
    for line in kaem_run.lines() {
        // sed `/^M2-Planet/,/-o m2\/mes\.M1/`: the range opens on the start line and
        // the end marker is only checked on SUBSEQUENT lines (so it never closes on
        // the line that opened it).
        let just_started = !in_range && line.starts_with("M2-Planet");
        if just_started {
            in_range = true;
        }
        if !in_range {
            continue;
        }
        if let Some(idx) = line.rfind(MARK) {
            let rest = &line[idx + MARK.len()..];
            let tok: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if !tok.is_empty() {
                out.push(tok.replace("${mes_cpu}", "x86_64"));
            }
        }
        if !just_started && line.contains("-o m2/mes.M1") {
            break;
        }
    }
    out
}

/// Brick 2 build: build the seed toolchain, unpack the pinned Mes tarball, generate
/// the non-system-libc `config.h` + arch headers, and drive M2-Planet → blood-elf →
/// M1 → hex2 over the tarball's own input list to produce `bin/mes-m2`. Returns the
/// mes scratch dir (also its `MES_PREFIX`). (Ported from `build_mes` in the former
/// tests/bootstrap-mes.sh.)
fn build_mes(cx: &Ctx) -> Result<Built, String> {
    let tc = mes_toolchain(cx)?;
    let _tc_guard = Cleanup(tc.clone());
    let m2p = tc.join("AMD64/artifact/M2");
    let be = tc.join("AMD64/artifact/blood-elf-0");
    let m1 = tc.join("AMD64/bin/M1");
    let hex2 = tc.join("AMD64/bin/hex2");
    for t in [&m2p, &be, &m1, &hex2] {
        make_executable(t).map_err(io_err("chmod toolchain tool"))?;
    }

    // Unpack the warmed, pin-verified tarball into a fresh scratch dir.
    let (_, tarball) = verified_source_tarball(cx, cx.source_pin(MES_SOURCE_KEY)?)?;
    let work = scratch_dir("td-bootstrap-mes").map_err(io_err("scratch dir"))?;
    extract_tar_gz(&tarball, &work)?;
    let m = single_subdir(&work)?;
    if !m.join("kaem.run").is_file() || !m.join("src/mes.c").is_file() {
        return Err(format!(
            "unpacked Mes tree missing kaem.run/src ({})",
            m.display()
        ));
    }

    // Generated, exactly as configure.sh does for the non-system-libc path.
    let ver = read_make_var(&m.join("configure.sh"), "VERSION")?;
    for d in ["include/mes", "include/arch", "m2", "bin"] {
        fs::create_dir_all(m.join(d)).map_err(io_err("mkdir mes subdir"))?;
    }
    fs::write(
        m.join("include/mes/config.h"),
        format!("#undef SYSTEM_LIBC\n#define MES_VERSION \"{ver}\"\n"),
    )
    .map_err(io_err("write config.h"))?;
    for h in ["kernel-stat.h", "signal.h", "syscall.h"] {
        fs::copy(
            m.join(format!("include/linux/x86_64/{h}")),
            m.join(format!("include/arch/{h}")),
        )
        .map_err(io_err("cp arch header"))?;
    }

    // M2-Planet: the tarball's own input list (config.h is generated above).
    let kaem_run = fs::read_to_string(m.join("kaem.run")).map_err(io_err("read kaem.run"))?;
    let units = parse_m2planet_units(&kaem_run);
    if units.is_empty() {
        return Err("kaem.run yielded no M2-Planet input units (format drift?)".into());
    }
    let mut m2_args: Vec<String> = vec![
        "--debug".into(),
        "--architecture".into(),
        "amd64".into(),
        "-D".into(),
        "__x86_64__=1".into(),
        "-D".into(),
        "__linux__=1".into(),
    ];
    for u in &units {
        m2_args.push("-f".into());
        m2_args.push(u.clone());
    }
    m2_args.push("-o".into());
    m2_args.push("m2/mes.M1".into());
    run_step(&m2p, &str_args(&m2_args), &m, "M2-Planet mes.M1")?;

    run_step(
        &be,
        &[
            "--64",
            "--little-endian",
            "-f",
            "m2/mes.M1",
            "-o",
            "m2/mes.blood-elf-M1",
        ],
        &m,
        "blood-elf",
    )?;
    run_step(
        &m1,
        &[
            "--architecture",
            "amd64",
            "--little-endian",
            "-f",
            "lib/m2/x86_64/x86_64_defs.M1",
            "-f",
            "lib/x86_64-mes/x86_64.M1",
            "-f",
            "lib/linux/x86_64-mes-m2/crt1.M1",
            "-f",
            "m2/mes.M1",
            "-f",
            "m2/mes.blood-elf-M1",
            "-o",
            "m2/mes.hex2",
        ],
        &m,
        "M1 assemble",
    )?;
    run_step(
        &hex2,
        &[
            "--architecture",
            "amd64",
            "--little-endian",
            "--base-address",
            "0x1000000",
            "-f",
            "lib/m2/x86_64/ELF-x86_64.hex2",
            "-f",
            "m2/mes.hex2",
            "-o",
            "bin/mes-m2",
        ],
        &m,
        "hex2 link",
    )?;
    make_executable(&m.join("bin/mes-m2")).map_err(io_err("chmod mes-m2"))?;
    Ok(Built { dir: m })
}

fn mes_behavioral(_cx: &Ctx, b: &Built) -> Result<(), String> {
    // mes-m2 finds its boot via MES_PREFIX + resolves modules via GUILE_LOAD_PATH;
    // both absolute since we run it from outside the mes scratch. (env -i + these two.)
    let prefix = b.dir.as_os_str();
    let load_path = format!(
        "{}:{}",
        b.dir.join("mes/module").display(),
        b.dir.join("module").display()
    );
    let mes = b.dir.join("bin/mes-m2");
    let eval = |code: &str| -> Result<String, String> {
        let out = scrubbed(&mes)
            .env("MES_PREFIX", prefix)
            .env("GUILE_LOAD_PATH", &load_path)
            .arg("-c")
            .arg(code)
            .output()
            .map_err(io_err("exec mes-m2"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "mes-m2 failed to evaluate `{code}`: {}",
                err.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let hello = eval("(display 'Hello,M2-mes!) (newline)")?;
    if hello != "Hello,M2-mes!" {
        return Err(format!(
            "mes-m2 display gave [{hello}], want [Hello,M2-mes!]"
        ));
    }
    let arith = eval("(display (+ 1 2 3 4)) (newline)")?;
    if arith != "10" {
        return Err(format!("mes-m2 arithmetic gave [{arith}], want [10]"));
    }
    Ok(())
}

// --- CLI -------------------------------------------------------------------------

const USAGE: &str = "usage: td-builder bootstrap-recipe <name> | --list";

/// `td-builder bootstrap-recipe <name> | --list`.
pub fn cli(args: &[String]) -> ExitCode {
    match args.get(2).map(String::as_str) {
        Some("--list") | Some("list") => {
            for n in all_names() {
                println!("{n}");
            }
            ExitCode::SUCCESS
        }
        Some(name) if !name.starts_with('-') => {
            let recipe = match lookup(name) {
                Some(r) => r,
                None => {
                    eprintln!(
                        "td-builder: bootstrap-recipe: unknown rung `{name}' (known: {})",
                        all_names().join(", ")
                    );
                    return ExitCode::FAILURE;
                }
            };
            let cx = match Ctx::discover() {
                Ok(cx) => cx,
                Err(e) => {
                    eprintln!("td-builder: bootstrap-recipe: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match run(&cx, &recipe) {
                Ok(report) => {
                    print!("{report}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("FAIL: bootstrap-recipe {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

// --- small std-only helpers ------------------------------------------------------

/// A `Command` with the environment cleared — the Rust `env -i`: a green build with
/// nothing on PATH/env proves NO guix process is in the chain (the static seed/rung
/// tools exec their inputs by relative path from `current_dir`).
fn scrubbed(prog: &Path) -> Command {
    let mut c = Command::new(prog);
    c.env_clear();
    c
}

fn io_err(ctx: &'static str) -> impl Fn(io::Error) -> String {
    move |e| format!("{ctx}: {e}")
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// pub(crate): also the per-file half of build::require_no_gnu_store (#378) —
// ONE copy of the north-star "no guix bytes" predicate.
pub(crate) fn contains_gnu_store(p: &Path) -> io::Result<bool> {
    let bytes = fs::read(p)?;
    Ok(find_sub(&bytes, b"/gnu/store"))
}

fn find_sub(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// `&[String]` → `&[&str]` for `Command::args`.
fn str_args(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Run a scrubbed-env build step with cwd `dir`; on failure include the stderr tail.
fn run_step(prog: &Path, args: &[&str], dir: &Path, what: &str) -> Result<(), String> {
    let out = scrubbed(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("exec {what}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!("{what} failed:\n{}", tail.join("\n")));
    }
    Ok(())
}

/// Extract a `.tar.gz` into `dest` with td's std-only gzip + tar readers.
fn extract_tar_gz(tarball: &Path, dest: &Path) -> Result<(), String> {
    tar::extract_tar_gz(tarball, dest)
}

/// The single top-level subdirectory of a freshly-unpacked tarball dir.
fn single_subdir(dir: &Path) -> Result<PathBuf, String> {
    let mut subdirs: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
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

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_dir() {
                fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))
            } else {
                fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

/// Read a `KEY=value` make/shell variable (first `^KEY=` line) from a file.
fn read_make_var(file: &Path, key: &str) -> Result<String, String> {
    let text = fs::read_to_string(file).map_err(|e| format!("read {}: {e}", file.display()))?;
    let prefix = format!("{key}=");
    for line in text.lines() {
        if let Some(v) = line.strip_prefix(&prefix) {
            return Ok(v.trim().to_string());
        }
    }
    Err(format!("{} has no {key}= line", file.display()))
}

// pub(crate): also chmods the two binary seeds in build::run_stage0 (#378).
pub(crate) fn make_executable(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)?.permissions();
    perm.set_mode(perm.mode() | 0o755);
    fs::set_permissions(p, perm)
}

fn scratch_dir(prefix: &str) -> io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{n}", std::process::id()));
    // Keep it ABSOLUTE: build steps use `current_dir(&scratch)` + an absolute program
    // path; a relative scratch (a relative TMPDIR) would make those programs resolve
    // against the parent cwd, not the scratch (the relative-program/current_dir gotcha).
    if dir.is_relative() {
        dir = std::env::current_dir()?.join(dir);
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Best-effort scratch-dir cleanup on scope exit (the runner builds twice). Set
/// `TD_BOOTSTRAP_KEEP=1` to keep the scratch dirs for debugging a rung's output.
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::env::var_os("TD_BOOTSTRAP_KEEP").is_none() {
            let _ = fs::remove_dir_all(&self.0);
        } else {
            eprintln!("td-bootstrap: kept scratch {}", self.0.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        // builder/ crate dir → repo root.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// The repo-tree integration tests need the warmed stage0 fixed-output
    /// tarball. When `cargo test` runs INSIDE the hermetic
    /// td-builder derivation build (check-engine compiles td-builder as a
    /// reproducible package and runs its unit tests there), only the `builder/`
    /// crate is staged; when a developer has not warmed sources, the tarball is
    /// absent. Those tests skip there; the bootstrap-seed / bootstrap-mes gates
    /// run the full source-backed proof.
    fn require_repo_tree() -> Option<Ctx> {
        let root = repo_root();
        if !root.join("recipes/Cargo.toml").is_file() {
            eprintln!("skip: repo recipe catalog absent at {}", root.display());
            return None;
        }
        let cx = match Ctx::rooted(root.clone()) {
            Ok(cx) => cx,
            Err(e) => {
                eprintln!("skip: could not load recipe source pins: {e}");
                return None;
            }
        };
        let stage0 = match cx.source_pin(STAGE0_SOURCE_KEY) {
            Ok(pin) => pin,
            Err(e) => {
                eprintln!("skip: {e}");
                return None;
            }
        };
        let tarball = cx.sources_dir.join(&stage0.file);
        if !tarball.is_file() {
            eprintln!(
                "skip: stage0 source tarball absent ({}) — run td-feed warm sources",
                tarball.display()
            );
            return None;
        }
        Some(cx)
    }

    fn synth_ctx() -> Ctx {
        Ctx {
            sources_dir: repo_root().join(".td-build-cache/sources"),
            source_pins: Vec::new(),
        }
    }

    #[test]
    fn m2planet_units_extracted_in_order_with_cpu_subst() {
        // The block runs from the `M2-Planet` line to `-o m2/mes.M1`; only
        // `-f ${srcdest}<path>` lines yield units, `${mes_cpu}` → x86_64.
        let kaem = "\
some preamble line\n\
M2-Planet \\\n\
    --debug \\\n\
    -f ${srcdest}include/mes/config.h \\\n\
    -f ${srcdest}lib/linux/${mes_cpu}-mes-m2/crt1.c \\\n\
                \\\n\
    -f ${srcdest}src/mes.c \\\n\
    -o m2/mes.M1\n\
    -f ${srcdest}should/not/appear.c\n";
        let units = parse_m2planet_units(kaem);
        assert_eq!(
            units,
            vec![
                "include/mes/config.h",
                "lib/linux/x86_64-mes-m2/crt1.c",
                "src/mes.c",
            ]
        );
    }

    #[test]
    fn find_sub_matches() {
        assert!(find_sub(b"aaa/gnu/storebbb", b"/gnu/store"));
        assert!(!find_sub(b"clean bytes", b"/gnu/store"));
    }

    // DURABLE end-to-end: build the source-free `seed` rung + assert all its legs.
    // No guix oracle (the seed IS the bottom); runs on every PR via cargo-test /
    // check-engine. Verified-red by perturbing each pin/leg (see plan notes).
    #[test]
    fn seed_recipe_builds_and_passes_all_legs() {
        let Some(cx) = require_repo_tree() else {
            return;
        };
        let recipe = seed_recipe();
        let report = run(&cx, &recipe).expect("seed recipe should pass all legs");
        assert!(report.contains("[pinned-input]"), "report:\n{report}");
        assert!(report.contains("[no-guix]"), "report:\n{report}");
        assert!(report.contains("[self-reproduction]"), "report:\n{report}");
        assert!(report.contains("[behavioral]"), "report:\n{report}");
        assert!(report.contains("[repro]"), "report:\n{report}");
        assert!(
            report.contains("PASS: source-bootstrap brick 0"),
            "report:\n{report}"
        );
    }

    // Verified-red harness as a test: a wrong source pin must red the pinned-input leg.
    #[test]
    fn wrong_stage0_source_pin_reds_pinned_input() {
        let Some(mut cx) = require_repo_tree() else {
            return;
        };
        let mut bad_pin = cx
            .source_pin(STAGE0_SOURCE_KEY)
            .expect("stage0 pin loaded")
            .clone();
        bad_pin.sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
            .to_string();
        cx.source_pins.retain(|pin| pin.key != STAGE0_SOURCE_KEY);
        cx.source_pins.push(bad_pin);
        let recipe = Recipe {
            pins: vec![Pin::Stage0Source {
                key: STAGE0_SOURCE_KEY,
            }],
            ..seed_recipe()
        };
        let e = run(&cx, &recipe).unwrap_err();
        assert!(e.contains("!= source pin"), "got: {e}");
    }

    // --- per-leg verified-red, via tiny synthetic recipes (no guix, no network) ---
    // These exercise each shared leg's RED path directly, so the leg is proven to
    // fail when the thing it checks breaks (verified-red discipline).

    fn synth(build: fn(&Ctx) -> Result<Built, String>, checks: Vec<Check>) -> Recipe {
        Recipe {
            name: "synth",
            brick: 0,
            pins: vec![],
            build,
            artifacts: vec!["art"],
            checks,
            summary: "synthetic test recipe",
        }
    }

    fn write_art(dir: &Path, bytes: &[u8]) -> Result<Built, String> {
        fs::write(dir.join("art"), bytes).map_err(|e| e.to_string())?;
        Ok(Built {
            dir: dir.to_path_buf(),
        })
    }

    fn build_deterministic(_cx: &Ctx) -> Result<Built, String> {
        let d = scratch_dir("td-synth-det").map_err(|e| e.to_string())?;
        write_art(&d, b"stable-bytes")
    }

    fn build_nondeterministic(_cx: &Ctx) -> Result<Built, String> {
        let d = scratch_dir("td-synth-nondet").map_err(|e| e.to_string())?;
        // A distinct counter per call → the two repro builds differ.
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        write_art(&d, format!("nondet-{n}").as_bytes())
    }

    fn build_with_gnu_store(_cx: &Ctx) -> Result<Built, String> {
        let d = scratch_dir("td-synth-gnu").map_err(|e| e.to_string())?;
        write_art(&d, b"refers to /gnu/store/abc-foo and is dirty")
    }

    #[test]
    fn green_synthetic_passes() {
        let cx = synth_ctx();
        run(&cx, &synth(build_deterministic, vec![])).expect("deterministic synth passes");
    }

    #[test]
    fn nondeterministic_build_reds_repro() {
        let cx = synth_ctx();
        let e = run(&cx, &synth(build_nondeterministic, vec![])).unwrap_err();
        assert!(e.contains("NOT reproducible"), "got: {e}");
    }

    #[test]
    fn gnu_store_in_artifact_reds_no_guix() {
        let cx = synth_ctx();
        let e = run(&cx, &synth(build_with_gnu_store, vec![])).unwrap_err();
        assert!(e.contains("/gnu/store"), "got: {e}");
    }

    #[test]
    fn failing_check_reds_run() {
        let cx = synth_ctx();
        let recipe = synth(
            build_deterministic,
            vec![Check {
                desc: "[behavioral] deliberately failing check",
                run: |_cx, _b| Err("artifact does not do its job".into()),
            }],
        );
        let e = run(&cx, &recipe).unwrap_err();
        assert!(e.contains("does not do its job"), "got: {e}");
    }
}
