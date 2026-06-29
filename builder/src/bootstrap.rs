//! Structured source-bootstrap recipes — the `tests/bootstrap-*.sh` drivers as
//! TYPED Rust data + one shared leg runner (rust-migration C2,
//! `plan/rust-migration.md` "C. Scripts → Rust"; sibling of C1 `affected.rs`).
//!
//! Every `tests/bootstrap-*.sh` is the SAME leg skeleton:
//!
//! ```text
//! [pinned-input]  the input bytes == the pin (a vendored sha, or a td-fetched
//!                 tarball == its seed/sources/*.lock sha256)
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

use crate::sha256::{to_base16, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// stage0-posix x86 3b9c2bb seed pins (mirror tests/bootstrap-seed.sh).
const HEX0_PIN: &str = "66c95985e668f20f2465c2b876f83fef066fd7c8c2dd3adb51a969f2d7120c8b";
const KAEM_PIN: &str = "153b8915b73bd07132b59538d10fe53d26578eb160a67db72af07aaa61c51b3b";

/// Where a recipe finds its inputs: the repo root (vendored seed, lock files) and
/// the warmed-source cache (`.td-build-cache/sources/`, populated by
/// `tools/warm-bootstrap-sources.sh` in check.sh's HOST prelude — the offline loop
/// never egresses).
pub struct Ctx {
    pub repo_root: PathBuf,
    pub sources_dir: PathBuf,
}

impl Ctx {
    /// Default context: repo root = CWD (the gate runs from there), sources =
    /// `<root>/.td-build-cache/sources` unless `TD_SOURCES_DIR` overrides it.
    pub fn discover() -> io::Result<Ctx> {
        let repo_root = std::env::current_dir()?;
        Ok(Ctx::rooted(repo_root))
    }
    pub fn rooted(repo_root: PathBuf) -> Ctx {
        let sources_dir = std::env::var_os("TD_SOURCES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root.join(".td-build-cache/sources"));
        Ctx {
            repo_root,
            sources_dir,
        }
    }
}

/// A pinned input — the `[pinned-input]` leg's two flavours.
pub enum Pin {
    /// In-repo auditable bytes (the seed bricks): `<repo>/path` sha256 == `sha256`,
    /// and (the seed's own `no-guix` assertion) no `/gnu/store` byte in it.
    Vendored {
        path: &'static str,
        sha256: &'static str,
    },
    /// A td-fetched tarball keyed by `seed/sources/<lock>` (url/sha256/file): the
    /// warmed `.td-build-cache/sources/<file>` must match the lock sha256.
    Source { lock: &'static str },
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
        _ => None,
    }
}

/// Every migrated rung, in brick order.
pub fn all_names() -> &'static [&'static str] {
    &["seed"]
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
            return Err(format!("{a} contains /gnu/store bytes — not a clean non-guix build"));
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
        Pin::Vendored { path, sha256 } => {
            let p = cx.repo_root.join(path);
            let got = sha256_file(&p).map_err(|e| format!("read vendored {path}: {e}"))?;
            if got != *sha256 {
                return Err(format!("vendored {path} sha256 {got} != pin {sha256} (seed drifted?)"));
            }
            if contains_gnu_store(&p).map_err(|e| format!("scan vendored {path}: {e}"))? {
                return Err(format!("vendored {path} contains /gnu/store bytes — not a clean non-guix seed"));
            }
            Ok(format!("vendored {path} matches pin sha256 ({sha256}) — auditable, NOT guix-built, no /gnu/store bytes"))
        }
        Pin::Source { lock } => {
            let lock_path = cx.repo_root.join("seed/sources").join(lock);
            let text = fs::read_to_string(&lock_path)
                .map_err(|e| format!("read lock {}: {e}", lock_path.display()))?;
            let pin = parse_source_lock(&text, lock)?;
            let tarball = cx.sources_dir.join(&pin.file);
            if !tarball.exists() {
                return Err(format!(
                    "the pinned tarball is not warm ({}) — run 'sh tools/warm-bootstrap-sources.sh' (needs network + td-fetch); check.sh's prelude does this",
                    tarball.display()
                ));
            }
            let got = sha256_file(&tarball).map_err(|e| format!("read {}: {e}", tarball.display()))?;
            if got != pin.sha256 {
                return Err(format!(
                    "warmed {} sha256 {got} != lock pin {} — corrupt fetch or stale lock",
                    pin.file, pin.sha256
                ));
            }
            Ok(format!(
                "td-fetched {} matches the lock sha256 ({}) — building from the pinned upstream bytes, not vendored/guix-fetched",
                pin.file, pin.sha256
            ))
        }
    }
}

/// A parsed `seed/sources/*.lock` (the `url`/`sha256`/`file` key/value format).
#[derive(Debug)]
pub struct SourcePin {
    pub url: String,
    pub sha256: String,
    pub file: String,
}

/// Parse a `seed/sources/*.lock` — `# comments` and blank lines skipped, each other
/// line is `<key> <value>`; `url`, `sha256` and `file` are required.
pub fn parse_source_lock(text: &str, lock_name: &str) -> Result<SourcePin, String> {
    let (mut url, mut sha256, mut file) = (None, None, None);
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("").trim();
        match key {
            "url" => url = Some(val.to_string()),
            "sha256" => sha256 = Some(val.to_string()),
            "file" => file = Some(val.to_string()),
            _ => {}
        }
    }
    Ok(SourcePin {
        url: url.ok_or_else(|| format!("{lock_name}: missing `url`"))?,
        sha256: sha256.ok_or_else(|| format!("{lock_name}: missing `sha256`"))?,
        file: file.ok_or_else(|| format!("{lock_name}: missing `file`"))?,
    })
}

// --- the seed recipe (brick 0) ---------------------------------------------------

fn seed_recipe() -> Recipe {
    Recipe {
        name: "seed",
        brick: 0,
        pins: vec![
            Pin::Vendored {
                path: "seed/stage0/bootstrap-seeds/POSIX/AMD64/hex0-seed",
                sha256: HEX0_PIN,
            },
            Pin::Vendored {
                path: "seed/stage0/bootstrap-seeds/POSIX/AMD64/kaem-optional-seed",
                sha256: KAEM_PIN,
            },
        ],
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

/// Brick 0 build: copy the vendored seed tree, run the kaem seed build env-cleared,
/// producing `AMD64/artifact/{hex0,kaem-0}`. (Mirrors `run_seed_build` in
/// tests/bootstrap-seed.sh.)
fn build_seed(cx: &Ctx) -> Result<Built, String> {
    let seed = cx.repo_root.join("seed/stage0");
    let out = scratch_dir("td-bootstrap-seed").map_err(io_err("scratch dir"))?;
    copy_tree(&seed, &out).map_err(io_err("copy seed tree"))?;
    let amd = "bootstrap-seeds/POSIX/AMD64";
    make_executable(&out.join(format!("{amd}/hex0-seed"))).map_err(io_err("chmod hex0-seed"))?;
    make_executable(&out.join(format!("{amd}/kaem-optional-seed")))
        .map_err(io_err("chmod kaem-optional-seed"))?;
    fs::create_dir_all(out.join("AMD64/artifact")).map_err(io_err("mkdir artifact"))?;

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
    Ok(Built { dir: out })
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
        return Err(format!("seed-built kaem-0 {kaem} != kaem-optional-seed {KAEM_PIN}"));
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
        return Err(format!("the seed-built hex0 assembled a wrong kaem-0 ({got})"));
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

fn sha256_file(p: &Path) -> io::Result<String> {
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_base16(&h.finalize()))
}

fn contains_gnu_store(p: &Path) -> io::Result<bool> {
    let bytes = fs::read(p)?;
    Ok(find_sub(&bytes, b"/gnu/store"))
}

fn find_sub(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn make_executable(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)?.permissions();
    perm.set_mode(perm.mode() | 0o755);
    fs::set_permissions(p, perm)
}

/// Recursively copy a directory tree (files + dirs, permission bits preserved by
/// `fs::copy`). The seed/source trees carry no symlinks.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn scratch_dir(prefix: &str) -> io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{n}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Best-effort scratch-dir cleanup on scope exit (the runner builds twice).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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

    #[test]
    fn source_lock_parses_url_sha_file() {
        let pin = parse_source_lock(
            "# comment\n\nurl https://ftp.gnu.org/gnu/mes/mes-0.27.1.tar.gz\nsha256 abc123\nfile mes-0.27.1.tar.gz\n",
            "mes.lock",
        )
        .unwrap();
        assert_eq!(pin.sha256, "abc123");
        assert_eq!(pin.file, "mes-0.27.1.tar.gz");
        assert!(pin.url.ends_with("mes-0.27.1.tar.gz"));
    }

    #[test]
    fn source_lock_missing_field_errors() {
        let e = parse_source_lock("url x\nfile y\n", "broken.lock").unwrap_err();
        assert!(e.contains("missing `sha256`"), "got: {e}");
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
        let cx = Ctx::rooted(repo_root());
        let recipe = seed_recipe();
        let report = run(&cx, &recipe).expect("seed recipe should pass all legs");
        assert!(report.contains("[pinned-input]"), "report:\n{report}");
        assert!(report.contains("[no-guix]"), "report:\n{report}");
        assert!(report.contains("[self-reproduction]"), "report:\n{report}");
        assert!(report.contains("[behavioral]"), "report:\n{report}");
        assert!(report.contains("[repro]"), "report:\n{report}");
        assert!(report.contains("PASS: source-bootstrap brick 0"), "report:\n{report}");
    }

    // Verified-red harness as a test: a wrong pin must red the pinned-input leg.
    #[test]
    fn wrong_vendored_pin_reds_pinned_input() {
        let cx = Ctx::rooted(repo_root());
        let recipe = Recipe {
            pins: vec![Pin::Vendored {
                path: "seed/stage0/bootstrap-seeds/POSIX/AMD64/hex0-seed",
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            }],
            ..seed_recipe()
        };
        let e = run(&cx, &recipe).unwrap_err();
        assert!(e.contains("!= pin"), "got: {e}");
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
        let cx = Ctx::rooted(repo_root());
        run(&cx, &synth(build_deterministic, vec![])).expect("deterministic synth passes");
    }

    #[test]
    fn nondeterministic_build_reds_repro() {
        let cx = Ctx::rooted(repo_root());
        let e = run(&cx, &synth(build_nondeterministic, vec![])).unwrap_err();
        assert!(e.contains("NOT reproducible"), "got: {e}");
    }

    #[test]
    fn gnu_store_in_artifact_reds_no_guix() {
        let cx = Ctx::rooted(repo_root());
        let e = run(&cx, &synth(build_with_gnu_store, vec![])).unwrap_err();
        assert!(e.contains("/gnu/store"), "got: {e}");
    }

    #[test]
    fn failing_check_reds_run() {
        let cx = Ctx::rooted(repo_root());
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
