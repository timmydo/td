//! The recipe vocabulary as TYPED Rust — a faithful mirror of `tests/ts/td-spec.d.ts`.
//!
//! This is the whole point of moving the package surface off boa/TypeScript: the
//! union types (`BuildSystem`, the `Replacement`/`FileArg`/`Stmt` sums) become
//! Rust enums and the shapes become structs, so `rustc` enforces at compile time
//! exactly what `tsc` enforced via the ambient `.d.ts` — a malformed recipe does
//! not compile, the same property the `ts`/`tsgo-pin` gates buy today. Each type
//! carries a `to_json` producing the SAME JSON shape boa emitted, so the Guile
//! lowering bridge is unchanged (camelCase keys; an optional field is emitted iff
//! it is present, matching boa's "keys present in the object literal").

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::application::ApplicationDeclaration;
use crate::json::Json;
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::PermissionPolicy;

fn vs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

fn arr(xs: &[String]) -> Json {
    Json::Arr(xs.iter().map(|x| Json::Str(x.clone())).collect())
}

/// Build systems td knows how to lower (mirrors `BuildSystem` in td-spec.d.ts).
/// `Stage0` is the SEED executor (#378) — see the engine's build::run_stage0.
/// (Named `stage0`, not `seed`: `seed` is taken by the lock input class and the
/// guix seed store.) `Mesboot` is the bootstrap-RUNG executor (#378 slices 2+3):
/// the recipe carries typed `steps` (below) and the engine's build::run_mesboot
/// executes them — the toolchain ladder rungs (mes → tcc → … → glibc-2.41).
#[derive(Clone)]
pub enum BuildSystem {
    Gnu,
    Rust,
    Cmake,
    Stage0,
    Mesboot,
    /// The Rust stage0 TRANSFORM: NOT a compile. It assembles the exact upstream
    /// rustc/rust-std/Cargo snapshot selected by the pinned Rust source, co-locates
    /// its declared runtime closure, and retargets rustc/rustdoc/Cargo onto td's
    /// glibc. Only the source-built stage2 recipe may consume this trust root.
    RustStage0,
}

impl BuildSystem {
    fn as_str(&self) -> &'static str {
        match self {
            BuildSystem::Gnu => "gnu",
            BuildSystem::Rust => "rust",
            BuildSystem::Cmake => "cmake",
            BuildSystem::Stage0 => "stage0",
            BuildSystem::Mesboot => "mesboot",
            BuildSystem::RustStage0 => "rust-stage0",
        }
    }
}

/// A bootstrap-rung build STEP (the `mesboot` build system, #378 slices 2+3).
/// Steps are DATA — the engine (build::run_mesboot) executes them in order; the
/// only processes spawned are `Run` steps' argv (td interprets NO shell — a
/// configure script runs because its argv names the declared bash input).
/// Every string is a TEMPLATE: `{root}` (the build root), `{src}` ({root}/src,
/// where the primary source is unpacked), `{out}`, `{tools}` (the ToolFarm bin
/// dir, {root}/tools), and `{in:NAME}` (the store path of lock input NAME).
/// An unknown token is a hard error at execution.
#[derive(Clone)]
pub enum Step {
    /// Spawn argv[0] with argv[1..]; env EXACTLY as given (cleared otherwise —
    /// the chain's `env -i` + MAKEFLAGS= scrubbing, as engine policy); cwd=dir.
    Run {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        dir: String,
    },
    /// Symlink name → target under {tools} (the rung's PATH farm; replaces the
    /// ladder's per-rung `bin/` symlink dirs + `ls /gnu/store/*pkg*` scavenging).
    ToolFarm {
        links: Vec<(String, String)>,
    },
    /// Write a file (wrapper scripts, config.cache, stub makefiles).
    WriteFile {
        path: String,
        content: String,
        exec: bool,
    },
    /// Unpack a source tarball (`.tar`/`.tar.gz`/`.tar.bz2`/`.tar.xz` by
    /// magic bytes) into dest with the ENGINE's own std-only readers — no
    /// tar/gzip/bzip2/xz package in the sandbox at all (re #469: an unpacker
    /// was every rung's excuse for host-tool inputs). `keep_top: false`
    /// strips the single top-level directory (`tar --strip-components=1`);
    /// the engine hard-errors if the tarball has no unique top-level dir.
    Unpack {
        input: String,
        dest: String,
        keep_top: bool,
    },
    /// The ENGINE-NATIVE mes bootstrap rung (builder's `mes_boot::run`, re
    /// #469): the Rust port of the mes tarball's configure.sh, bootstrap.sh,
    /// and install.sh. The only subprocesses are stage0 recipe outputs (kaem
    /// driving upstream's own kaem.run) and the just-built mes running
    /// upstream's mescc.scm — the rung declares NO host shell or coreutils.
    MesBoot {
        source: String,
        nyacc: String,
        stage0: String,
    },
    /// Copy files (flat) into dest, made user-writable (build trees are written into).
    CopyFiles {
        files: Vec<String>,
        dest: String,
    },
    /// Recursive tree copy (kernel-header overlays, module trees).
    CopyTree {
        from: String,
        dest: String,
    },
    /// Create and verify deterministic debug companions for every ET_EXEC and
    /// ET_DYN below `root`. `objcopy` is an explicitly declared target tool;
    /// the engine walks the tree and validates GNU build IDs, symbols and line
    /// tables without depending on a shell or host ELF utility.
    SplitDebugTree {
        root: String,
        objcopy: String,
    },
    /// Count hard-link-deduplicated files below every `lib/debug` in `root`,
    /// refuse the compiled ceiling, and write a stable machine-readable report.
    AssertDebugSize {
        root: String,
        report: String,
        scope: String,
        ceiling: u64,
    },
    /// Refuse unless two regular files are byte-identical. Used by bounded
    /// reproducibility oracles without adding a shell or comparison utility.
    CompareFiles {
        left: String,
        right: String,
    },
    /// Stage the transitive ELF-loader and symlink store closure of explicit
    /// roots under `dest`. Script interpreters and data-only `dlopen` paths are
    /// outside this step's graph. Every reachable store item must be a declared
    /// recipe input; the engine rejects ambient or merely transitive paths.
    StageRuntimeClosure {
        roots: Vec<String>,
        dest: String,
    },
    /// Compile builder-authenticated package exports into the image's resolver
    /// registry and compositor launcher table. Expected names are literal
    /// labels; package and runtime paths use the data channel.
    CompileApplicationTables {
        names: Vec<String>,
        packages: Vec<String>,
        runtimes: Vec<String>,
        registry: String,
        launcher: String,
    },
    /// Pack `root` as a deterministic EROFS image at `output` using the
    /// control-plane engine's dependency-free image writer.
    PackErofs {
        root: String,
        output: String,
    },
    /// Write a deterministic manifest of `(name, file)` SHA-256 entries.
    /// Names are artifact labels, while file paths are template-expanded.
    Sha256Manifest {
        output: String,
        entries: Vec<(String, String)>,
    },
    Symlink {
        target: String,
        link: String,
    },
    MkDir {
        path: String,
    },
    /// Create `path` (or resize it) to exactly `bytes`, engine-native.
    ///
    /// A disk image is a file of a declared SIZE and almost entirely hole, and
    /// the shell spelling of that is `dd … count=0 seek=N` — which needs a `dd`
    /// no roster declares. `busybox-x86-64` serves a curated applet set and `dd`
    /// is not in it, so a recipe reaching for one depends on the multicall
    /// carrying an applet nothing checks. This is `set_len(2)`, which is what
    /// that `dd` does and what an installer's destination needs.
    ///
    /// Sparse where the filesystem has holes, which every filesystem td builds
    /// on does; a filesystem without them would allocate `bytes` for real.
    Truncate {
        path: String,
        bytes: u64,
    },
    /// Rewrite `#!/bin/sh`-style shebangs under dir to the given shell (the
    /// engine's own patch_shebangs — the sandbox has no /bin/sh).
    PatchShebangs {
        dir: String,
        shell: String,
    },
    /// Rewrite glibc text linker scripts under `dir/*.so`, stripping
    /// `<prefix>/lib/` from their member names. Real ELF shared objects are
    /// skipped by the engine's GNU-ld-script marker check.
    RelocateLdScripts {
        dir: String,
        prefix: String,
    },
    /// Assert products exist (and are executable files if exec) — fail HERE with
    /// a named path, not three rungs later.
    Require {
        paths: Vec<String>,
        exec: bool,
    },
    /// Apply literal, fail-closed text edits to a file in place — the host-free
    /// stand-in for `patch`/`sed` (re #469: the sandbox ships neither, and
    /// stage0's `replace` cannot carry the space-bearing, multi-line strings a
    /// real patch hunk needs through kaem's quote-stripping tokenizer). Each
    /// edit replaces EVERY occurrence of `from` with `to`, requiring exactly
    /// `expect` (≥ 1) occurrences first, so a drift in the pinned source — or a
    /// transcription slip — reds the rung instead of silently doing nothing.
    /// Edits apply in order (an edit sees the previous edits' result). Only
    /// `file` is template-expanded; `from`/`to` are LITERAL source text, so C
    /// braces and `{…}` pass through untouched. The engine also reds an empty
    /// `from`, an `expect` of 0, or any non-ASCII byte in `from`/`to` (the
    /// build-JSON wire reader is Latin-1, so only ASCII round-trips faithfully).
    SubstituteText {
        file: String,
        edits: Vec<TextEdit>,
    },
    /// Assert each product is a FULLY STATIC ELF — no `PT_INTERP` (host loader),
    /// no `DT_NEEDED` (host libc), no `DT_RPATH`/`DT_RUNPATH` — the runtime-
    /// provenance gate for the pre-libc rungs (re #469). tcc/make/yacc are all
    /// linked `-static`; a regression that reintroduced a program interpreter or
    /// a `libc.so` dependency would drag a host loader + glibc back in at run
    /// time — exactly the host-runtime ingress #469 closes. This reds the rung,
    /// naming the offending binary and leak, so the leak never reaches a later
    /// rung. Fails closed on a non-ELF too (the parser rejects bad magic).
    AssertStatic {
        paths: Vec<String>,
    },
    /// Validate the complete `files/` tree of a static seeded application.
    /// `entry` is the manifest's `/app/...` path and `runtime` is the declared
    /// payload name, not a template: the builder resolves it only through the
    /// payload map, so this check cannot accidentally reopen the tool channel.
    ValidateStaticApplication {
        entry: String,
        runtime: String,
    },
}

/// One literal edit within a [`Step::SubstituteText`]: replace every occurrence
/// of `from` with `to`, requiring exactly `expect` (≥ 1) occurrences. `from`/`to`
/// must be ASCII (the build-JSON wire reader is Latin-1).
#[derive(Clone)]
pub struct TextEdit {
    pub from: String,
    pub to: String,
    pub expect: usize,
}

impl TextEdit {
    pub fn new(from: &str, to: &str, expect: usize) -> TextEdit {
        TextEdit {
            from: from.into(),
            to: to.into(),
            expect,
        }
    }

    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("from".into(), Json::Str(self.from.clone())),
            ("to".into(), Json::Str(self.to.clone())),
            // Counts ride as strings because the builder's minimal JSON API
            // intentionally exposes strings, arrays and booleans only.
            ("expect".into(), Json::Str(self.expect.to_string())),
        ])
    }
}

impl Step {
    /// `Run` with an empty env; chain `.env()` for each variable.
    pub fn run(dir: &str, argv: &[&str]) -> Step {
        Step::Run {
            argv: vs(argv),
            env: Vec::new(),
            dir: dir.into(),
        }
    }
    /// Add one env var to a `Run` (no-op on other variants).
    pub fn env(self, k: &str, v: &str) -> Step {
        match self {
            Step::Run { argv, mut env, dir } => {
                env.push((k.into(), v.into()));
                Step::Run { argv, env, dir }
            }
            other => other,
        }
    }
    /// Apply a list of literal, count-checked [`TextEdit`]s to `file` in place.
    pub fn substitute_text(file: &str, edits: Vec<TextEdit>) -> Step {
        Step::SubstituteText {
            file: file.into(),
            edits,
        }
    }
    /// Assert `paths` are fully static ELF binaries (no host loader/libc/run-path).
    pub fn assert_static(paths: &[&str]) -> Step {
        Step::AssertStatic { paths: vs(paths) }
    }
    /// Apply the target debug-companion policy to an installed package tree.
    pub fn split_debug_tree(root: &str, objcopy: &str) -> Step {
        Step::SplitDebugTree {
            root: root.into(),
            objcopy: objcopy.into(),
        }
    }
    pub fn assert_debug_size(root: &str, report: &str, scope: &str, ceiling: u64) -> Step {
        Step::AssertDebugSize {
            root: root.into(),
            report: report.into(),
            scope: scope.into(),
            ceiling,
        }
    }
    pub fn compare_files(left: &str, right: &str) -> Step {
        Step::CompareFiles {
            left: left.into(),
            right: right.into(),
        }
    }
    pub fn validate_static_application(declaration: &ApplicationDeclaration) -> Step {
        Step::ValidateStaticApplication {
            entry: declaration.entry().to_string(),
            runtime: declaration.runtime().to_string(),
        }
    }
    fn to_json(&self) -> Json {
        let pair_arr = |xs: &[(String, String)]| {
            Json::Arr(
                xs.iter()
                    .map(|(a, b)| Json::Arr(vec![Json::Str(a.clone()), Json::Str(b.clone())]))
                    .collect(),
            )
        };
        match self {
            Step::Run { argv, env, dir } => Json::Obj(vec![(
                "run".into(),
                Json::Obj(vec![
                    ("argv".into(), arr(argv)),
                    ("env".into(), pair_arr(env)),
                    ("dir".into(), Json::Str(dir.clone())),
                ]),
            )]),
            Step::ToolFarm { links } => Json::Obj(vec![("toolFarm".into(), pair_arr(links))]),
            Step::WriteFile {
                path,
                content,
                exec,
            } => Json::Obj(vec![(
                "writeFile".into(),
                Json::Obj(vec![
                    ("path".into(), Json::Str(path.clone())),
                    ("content".into(), Json::Str(content.clone())),
                    ("exec".into(), Json::Bool(*exec)),
                ]),
            )]),
            Step::Unpack {
                input,
                dest,
                keep_top,
            } => Json::Obj(vec![(
                "unpack".into(),
                Json::Obj(vec![
                    ("input".into(), Json::Str(input.clone())),
                    ("dest".into(), Json::Str(dest.clone())),
                    ("keepTop".into(), Json::Bool(*keep_top)),
                ]),
            )]),
            Step::MesBoot {
                source,
                nyacc,
                stage0,
            } => Json::Obj(vec![(
                "mesBoot".into(),
                Json::Obj(vec![
                    ("source".into(), Json::Str(source.clone())),
                    ("nyacc".into(), Json::Str(nyacc.clone())),
                    ("stage0".into(), Json::Str(stage0.clone())),
                ]),
            )]),
            Step::CopyFiles { files, dest } => Json::Obj(vec![(
                "copyFiles".into(),
                Json::Obj(vec![
                    ("files".into(), arr(files)),
                    ("dest".into(), Json::Str(dest.clone())),
                ]),
            )]),
            Step::CopyTree { from, dest } => Json::Obj(vec![(
                "copyTree".into(),
                Json::Obj(vec![
                    ("from".into(), Json::Str(from.clone())),
                    ("dest".into(), Json::Str(dest.clone())),
                ]),
            )]),
            Step::SplitDebugTree { root, objcopy } => Json::Obj(vec![(
                "splitDebugTree".into(),
                Json::Obj(vec![
                    ("root".into(), Json::Str(root.clone())),
                    ("objcopy".into(), Json::Str(objcopy.clone())),
                ]),
            )]),
            Step::AssertDebugSize {
                root,
                report,
                scope,
                ceiling,
            } => Json::Obj(vec![(
                "assertDebugSize".into(),
                Json::Obj(vec![
                    ("root".into(), Json::Str(root.clone())),
                    ("report".into(), Json::Str(report.clone())),
                    ("scope".into(), Json::Str(scope.clone())),
                    ("ceiling".into(), Json::Num(ceiling.to_string())),
                ]),
            )]),
            Step::CompareFiles { left, right } => Json::Obj(vec![(
                "compareFiles".into(),
                Json::Obj(vec![
                    ("left".into(), Json::Str(left.clone())),
                    ("right".into(), Json::Str(right.clone())),
                ]),
            )]),
            Step::StageRuntimeClosure { roots, dest } => Json::Obj(vec![(
                "stageRuntimeClosure".into(),
                Json::Obj(vec![
                    ("roots".into(), arr(roots)),
                    ("dest".into(), Json::Str(dest.clone())),
                ]),
            )]),
            Step::CompileApplicationTables {
                names,
                packages,
                runtimes,
                registry,
                launcher,
            } => Json::Obj(vec![(
                "compileApplicationTables".into(),
                Json::Obj(vec![
                    ("names".into(), arr(names)),
                    ("packages".into(), arr(packages)),
                    ("runtimes".into(), arr(runtimes)),
                    ("registry".into(), Json::Str(registry.clone())),
                    ("launcher".into(), Json::Str(launcher.clone())),
                ]),
            )]),
            Step::PackErofs { root, output } => Json::Obj(vec![(
                "packErofs".into(),
                Json::Obj(vec![
                    ("root".into(), Json::Str(root.clone())),
                    ("output".into(), Json::Str(output.clone())),
                ]),
            )]),
            Step::Sha256Manifest { output, entries } => Json::Obj(vec![(
                "sha256Manifest".into(),
                Json::Obj(vec![
                    ("output".into(), Json::Str(output.clone())),
                    ("entries".into(), pair_arr(entries)),
                ]),
            )]),
            Step::Symlink { target, link } => Json::Obj(vec![(
                "symlink".into(),
                Json::Obj(vec![
                    ("target".into(), Json::Str(target.clone())),
                    ("link".into(), Json::Str(link.clone())),
                ]),
            )]),
            Step::MkDir { path } => Json::Obj(vec![("mkDir".into(), Json::Str(path.clone()))]),
            // The size goes over as a STRING, as every other number in this
            // encoding does: a disk image is bigger than an f64 counts exactly,
            // and a JSON number that rounded would be a destination of the wrong
            // size with nothing to say so.
            Step::Truncate { path, bytes } => Json::Obj(vec![(
                "truncate".into(),
                Json::Obj(vec![
                    ("path".into(), Json::Str(path.clone())),
                    ("bytes".into(), Json::Str(bytes.to_string())),
                ]),
            )]),
            Step::PatchShebangs { dir, shell } => Json::Obj(vec![(
                "patchShebangs".into(),
                Json::Obj(vec![
                    ("dir".into(), Json::Str(dir.clone())),
                    ("shell".into(), Json::Str(shell.clone())),
                ]),
            )]),
            Step::RelocateLdScripts { dir, prefix } => Json::Obj(vec![(
                "relocateLdScripts".into(),
                Json::Obj(vec![
                    ("dir".into(), Json::Str(dir.clone())),
                    ("prefix".into(), Json::Str(prefix.clone())),
                ]),
            )]),
            Step::Require { paths, exec } => Json::Obj(vec![(
                "require".into(),
                Json::Obj(vec![
                    ("paths".into(), arr(paths)),
                    ("exec".into(), Json::Bool(*exec)),
                ]),
            )]),
            Step::AssertStatic { paths } => Json::Obj(vec![(
                "assertStatic".into(),
                Json::Obj(vec![("paths".into(), arr(paths))]),
            )]),
            Step::ValidateStaticApplication { entry, runtime } => Json::Obj(vec![(
                "validateStaticApplication".into(),
                Json::Obj(vec![
                    ("entry".into(), Json::Str(entry.clone())),
                    ("runtime".into(), Json::Str(runtime.clone())),
                ]),
            )]),
            Step::SubstituteText { file, edits } => Json::Obj(vec![(
                "substituteText".into(),
                Json::Obj(vec![
                    ("file".into(), Json::Str(file.clone())),
                    (
                        "edits".into(),
                        Json::Arr(edits.iter().map(TextEdit::to_json).collect()),
                    ),
                ]),
            )]),
        }
    }
}

/// An upstream source URI — a single URL or a list of mirror URLs (these lower to
/// DIFFERENT source derivations, so the shape is load-bearing — `Source` union).
#[derive(Clone)]
pub enum Uri {
    One(String),
    List(Vec<String>),
}

impl Uri {
    fn to_json(&self) -> Json {
        match self {
            Uri::One(u) => Json::Str(u.clone()),
            Uri::List(us) => Json::Arr(us.iter().map(|u| Json::Str(u.clone())).collect()),
        }
    }
}

/// An upstream source: a URI (or mirror list) + its nix-base32 sha256.
#[derive(Clone)]
pub struct Source {
    pub uri: Uri,
    pub sha256: String,
}

impl Source {
    pub fn one(uri: &str, sha256: &str) -> Source {
        Source {
            uri: Uri::One(uri.into()),
            sha256: sha256.into(),
        }
    }
    pub fn list(uris: &[&str], sha256: &str) -> Source {
        Source {
            uri: Uri::List(vs(uris)),
            sha256: sha256.into(),
        }
    }
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("uri".into(), self.uri.to_json()),
            ("sha256".into(), Json::Str(self.sha256.clone())),
        ])
    }
}

/// A td-owned fixed-output source pin. These are the URL/sha256/file triples
/// that used to live in the external source lock directory; recipes carry them as
/// metadata so warmers/checks resolve from the typed catalog instead of an
/// external lock directory. They are intentionally not emitted into build JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePin {
    pub key: String,
    pub url: String,
    pub sha256: String,
    pub file: String,
    /// APPLICATIONS.md §B.8: a prebuilt payload that is NOT a bootstrap seed and
    /// never becomes one. The mark rides the SOURCE pin rather than the finished
    /// package, because marking only the output would leave the packaging recipe
    /// free to execute the pinned bytes while building it, and would let a second
    /// recipe consume the same archive and emit an unmarked output.
    ///
    /// PRIVATE where the other four are public, so `mark_foreign` is the single
    /// spelling: a struct literal carrying `foreign: true` would set it without
    /// naming the thing being done, and the whole argument for a method over a
    /// fifth constructor argument is that a trust decision should be greppable
    /// by one token. Every reader is in this crate.
    foreign: bool,
}

/// One exact OSTree deploy graph reviewed as a foreign prebuilt payload.
///
/// Unlike `SourcePin`, this is not one URL whose response is the payload. The
/// commit and content checksums authenticate a bounded graph below `repository`,
/// while `exact_ref` binds the graph's signed metadata to its reviewed role.
/// `cache` is only a host cache identity; it is permanently bound to the other
/// fields by td-net's ownership record and carries no artifact authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OstreePin {
    pub key: String,
    pub repository: String,
    pub exact_ref: String,
    pub commit: String,
    pub content: String,
    pub signing_key_fingerprint: String,
    pub cache: String,
    pub expected: OstreeGraphStats,
    pub(crate) foreign: bool,
}

/// Reviewed accounting for one exact OSTree `files/` graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OstreeGraphStats {
    pub objects: usize,
    pub paths: usize,
    pub directories: usize,
    pub regular_files: usize,
    pub symlinks: usize,
    pub decoded_bytes: u64,
    pub transfer_bytes: u64,
}

impl OstreePin {
    pub fn foreign(&self) -> bool {
        self.foreign
    }
}

impl SourcePin {
    pub fn new(key: &str, url: &str, sha256: &str, file: &str) -> SourcePin {
        SourcePin {
            key: key.into(),
            url: url.into(),
            sha256: sha256.into(),
            file: file.into(),
            foreign: false,
        }
    }

    /// A method rather than a fifth argument to `new`: a bare `true` at a call
    /// site is the least reviewable way to spell a trust decision, and this one
    /// is §B.8's reviewed exception to "no foreign binary in the build graph".
    pub fn mark_foreign(mut self) -> SourcePin {
        self.foreign = true;
        self
    }

    pub fn foreign(&self) -> bool {
        self.foreign
    }
}

/// A part of a `string-append`/`format` replacement (`RefPart` in td-spec.d.ts).
#[derive(Clone)]
pub enum RefPart {
    Lit(String),
    Output(String),
    Input(String),
    Var(String),
}

impl RefPart {
    fn to_json(&self) -> Json {
        match self {
            RefPart::Lit(x) => Json::Str(x.clone()),
            RefPart::Output(x) => Json::Obj(vec![("output".into(), Json::Str(x.clone()))]),
            RefPart::Input(x) => Json::Obj(vec![("input".into(), Json::Str(x.clone()))]),
            RefPart::Var(x) => Json::Obj(vec![("var".into(), Json::Str(x.clone()))]),
        }
    }
}

/// A `substitute*` replacement (`Replacement` union in td-spec.d.ts).
#[derive(Clone)]
pub enum Replacement {
    Lit(String),
    Var(String),
    Which(String),
    StringAppend(Vec<RefPart>),
    /// `{ format: [FMT, PART…] }`.
    Format(String, Vec<RefPart>),
}

impl Replacement {
    fn to_json(&self) -> Json {
        match self {
            Replacement::Lit(x) => Json::Str(x.clone()),
            Replacement::Var(x) => Json::Obj(vec![("var".into(), Json::Str(x.clone()))]),
            Replacement::Which(x) => Json::Obj(vec![("which".into(), Json::Str(x.clone()))]),
            Replacement::StringAppend(parts) => Json::Obj(vec![(
                "stringAppend".into(),
                Json::Arr(parts.iter().map(|p| p.to_json()).collect()),
            )]),
            Replacement::Format(fmt, parts) => {
                let mut a = vec![Json::Str(fmt.clone())];
                a.extend(parts.iter().map(|p| p.to_json()));
                Json::Obj(vec![("format".into(), Json::Arr(a))])
            }
        }
    }
}

/// A `substitute*` FILE argument (`FileArg` union in td-spec.d.ts).
#[derive(Clone)]
pub enum FileArg {
    Lit(String),
    List(Vec<String>),
    FindFiles(String, String),
    Cons(Box<FileArg>, Box<FileArg>),
}

impl FileArg {
    fn to_json(&self) -> Json {
        match self {
            FileArg::Lit(x) => Json::Str(x.clone()),
            FileArg::List(xs) => Json::Obj(vec![(
                "list".into(),
                Json::Arr(xs.iter().map(|x| Json::Str(x.clone())).collect()),
            )]),
            FileArg::FindFiles(d, r) => Json::Obj(vec![(
                "findFiles".into(),
                Json::Arr(vec![Json::Str(d.clone()), Json::Str(r.clone())]),
            )]),
            FileArg::Cons(a, b) => Json::Obj(vec![(
                "cons".into(),
                Json::Arr(vec![a.to_json(), b.to_json()]),
            )]),
        }
    }
}

/// One `substitute*` clause `((FROM MATCH-VAR…) TO)` (`Clause` in td-spec.d.ts).
#[derive(Clone)]
pub struct Clause {
    pub from: String,
    pub matches: Option<Vec<String>>,
    pub to: Replacement,
}

impl Clause {
    pub fn new(from: &str, to: Replacement) -> Clause {
        Clause {
            from: from.into(),
            matches: None,
            to,
        }
    }
    pub fn matching(mut self, xs: &[&str]) -> Clause {
        self.matches = Some(vs(xs));
        self
    }
    fn to_json(&self) -> Json {
        let mut o = vec![("from".into(), Json::Str(self.from.clone()))];
        if let Some(m) = &self.matches {
            o.push(("match".into(), arr(m)));
        }
        o.push(("to".into(), self.to.to_json()));
        Json::Obj(o)
    }
}

/// A phase-body statement (`Stmt` union in td-spec.d.ts).
#[derive(Clone)]
pub enum Stmt {
    Substitute {
        file: FileArg,
        clauses: Vec<Clause>,
    },
    LetWhich {
        binds: Vec<(String, String)>,
        body: Vec<Stmt>,
    },
    WithDefaultPortEncodingFalse {
        body: Vec<Stmt>,
    },
}

impl Stmt {
    fn to_json(&self) -> Json {
        match self {
            Stmt::Substitute { file, clauses } => Json::Obj(vec![
                ("substitute".into(), file.to_json()),
                (
                    "clauses".into(),
                    Json::Arr(clauses.iter().map(|c| c.to_json()).collect()),
                ),
            ]),
            Stmt::LetWhich { binds, body } => Json::Obj(vec![
                (
                    "letWhich".into(),
                    Json::Arr(
                        binds
                            .iter()
                            .map(|(n, p)| {
                                Json::Obj(vec![
                                    ("name".into(), Json::Str(n.clone())),
                                    ("prog".into(), Json::Str(p.clone())),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "body".into(),
                    Json::Arr(body.iter().map(|s| s.to_json()).collect()),
                ),
            ]),
            Stmt::WithDefaultPortEncodingFalse { body } => Json::Obj(vec![
                ("withDefaultPortEncodingFalse".into(), Json::Bool(true)),
                (
                    "body".into(),
                    Json::Arr(body.iter().map(|s| s.to_json()).collect()),
                ),
            ]),
        }
    }
}

/// A flat `substitute*` over one source file (`Substitution` in td-spec.d.ts).
#[derive(Clone)]
pub struct Substitution {
    pub file: String,
    pub from: String,
    pub to: Replacement,
}

impl Substitution {
    pub fn new(file: &str, from: &str, to: Replacement) -> Substitution {
        Substitution {
            file: file.into(),
            from: from.into(),
            to,
        }
    }
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("file".into(), Json::Str(self.file.clone())),
            ("from".into(), Json::Str(self.from.clone())),
            ("to".into(), self.to.to_json()),
        ])
    }
}

/// A custom build phase (`Phase` in td-spec.d.ts).
#[derive(Clone)]
pub struct Phase {
    pub position: String,
    pub anchor: String,
    pub name: String,
    pub lambda_args: Option<Vec<String>>,
    pub substitutions: Option<Vec<Substitution>>,
    pub return_true: Option<bool>,
    pub body: Option<Vec<Stmt>>,
}

/// One package selected from a fixed-output archive of a Cargo Git source.
/// `path` is relative to the archive's single top-level directory; `.` names
/// that directory itself. The lock gate binds the declared name and version to
/// the exact Git source entry; Cargo then validates the copied manifest.
#[derive(Clone)]
pub struct CargoGitPackage {
    pub name: String,
    pub version: String,
    pub path: String,
}

impl CargoGitPackage {
    pub fn new(name: &str, version: &str, path: &str) -> CargoGitPackage {
        CargoGitPackage {
            name: name.into(),
            version: version.into(),
            path: path.into(),
        }
    }

    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("name".into(), Json::Str(self.name.clone())),
            ("version".into(), Json::Str(self.version.clone())),
            ("path".into(), Json::Str(self.path.clone())),
        ])
    }
}

/// A Cargo `git+` source represented as an ordinary td fixed-output input.
/// `source` is the exact Cargo.lock source id, including its full commit;
/// `input` names a source pin whose URL and SHA-256 authenticate the commit
/// archive. No Git client or network access enters the target build.
#[derive(Clone)]
pub struct CargoGitSource {
    pub source: String,
    pub input: String,
    pub packages: Vec<CargoGitPackage>,
}

impl CargoGitSource {
    pub fn new(
        source: &str,
        input: &str,
        packages: Vec<CargoGitPackage>,
    ) -> CargoGitSource {
        CargoGitSource {
            source: source.into(),
            input: input.into(),
            packages,
        }
    }

    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("source".into(), Json::Str(self.source.clone())),
            ("input".into(), Json::Str(self.input.clone())),
            (
                "packages".into(),
                Json::Arr(self.packages.iter().map(CargoGitPackage::to_json).collect()),
            ),
        ])
    }
}

/// A literal, count-checked patch to one Cargo.toml or build.rs below the
/// selected Rust workspace. This is narrower than a generic build phase, and
/// the Rust runner applies the edits before enforcing the exact reviewed
/// Cargo.lock and invoking frozen Cargo.
#[derive(Clone)]
pub struct CargoSourcePatch {
    pub file: String,
    pub edits: Vec<TextEdit>,
}

impl CargoSourcePatch {
    pub fn new(file: &str, edits: Vec<TextEdit>) -> CargoSourcePatch {
        CargoSourcePatch {
            file: file.into(),
            edits,
        }
    }

    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("file".into(), Json::Str(self.file.clone())),
            (
                "edits".into(),
                Json::Arr(self.edits.iter().map(TextEdit::to_json).collect()),
            ),
        ])
    }
}

impl Phase {
    pub fn new(position: &str, anchor: &str, name: &str) -> Phase {
        Phase {
            position: position.into(),
            anchor: anchor.into(),
            name: name.into(),
            lambda_args: None,
            substitutions: None,
            return_true: None,
            body: None,
        }
    }
    pub fn lambda_args(mut self, xs: &[&str]) -> Phase {
        self.lambda_args = Some(vs(xs));
        self
    }
    pub fn substitutions(mut self, xs: Vec<Substitution>) -> Phase {
        self.substitutions = Some(xs);
        self
    }
    pub fn return_true(mut self) -> Phase {
        self.return_true = Some(true);
        self
    }
    pub fn body(mut self, xs: Vec<Stmt>) -> Phase {
        self.body = Some(xs);
        self
    }
    fn to_json(&self) -> Json {
        let mut o = vec![
            ("position".into(), Json::Str(self.position.clone())),
            ("anchor".into(), Json::Str(self.anchor.clone())),
            ("name".into(), Json::Str(self.name.clone())),
        ];
        if let Some(la) = &self.lambda_args {
            o.push(("lambdaArgs".into(), arr(la)));
        }
        if let Some(subs) = &self.substitutions {
            o.push((
                "substitutions".into(),
                Json::Arr(subs.iter().map(|s| s.to_json()).collect()),
            ));
        }
        if let Some(rt) = self.return_true {
            o.push(("returnTrue".into(), Json::Bool(rt)));
        }
        if let Some(body) = &self.body {
            o.push((
                "body".into(),
                Json::Arr(body.iter().map(|s| s.to_json()).collect()),
            ));
        }
        Json::Obj(o)
    }
}

/// A package recipe — the coordinates that determine the build derivation
/// (`Recipe` in td-spec.d.ts). Built with the `gnu`/`rust`/`cmake` constructors
/// plus chained setters; an unset optional field is omitted from the JSON.
#[derive(Clone)]
pub struct Recipe {
    pub name: String,
    pub version: String,
    pub source: Option<Source>,
    /// The MAP KEY (in the `build-plan --auto` tool/source map `ladder_setup`
    /// interns) that resolves to this recipe's OWN `<name>-source` lock entry
    /// (#429) — distinct from `source` (an actual declared upstream fetch): a
    /// mesboot rung's source is a recipe-pinned tarball ALREADY interned
    /// under some other name (e.g. gcc-mesboot1 builds from the map key
    /// `gcc-464-core`, not `gcc-mesboot1-source`), so this just names which
    /// interned entry to alias in. `None` means the recipe has no source of its
    /// own (e.g. make-test, which only RUNS a sibling rung's output) — the
    /// synthesizer then emits no `<name>-source` line at all.
    pub source_input: Option<String>,
    pub build_system: BuildSystem,
    pub inputs: Option<Vec<String>>,
    /// Staged builders (#378): inputs that are themselves td recipes and act as
    /// this rung's COMPILER/tools — the prior rung's output used to build this
    /// one (guix's native-inputs). `build-plan --auto` chains them like inputs.
    pub native_inputs: Option<Vec<String>>,
    /// DATA inputs (APPLICATIONS.md §B.8): declared to be copied or named rather
    /// than built with. `inputs`/`native_inputs` are the tool, compilation and
    /// execution channel; this is the other one, and the split exists because an
    /// edge carried no label saying WHY an input was there — so a check could
    /// see "image recipe → firefox" and not tell payload from compiler.
    ///
    /// It is a channel rather than a predicate over the existing list because
    /// the difference has to be enforceable, not merely readable. What is
    /// ENFORCED is exactly two things: a payload resolves only through its own
    /// `{payload:NAME}` template, which the expander for a step that runs a
    /// command has no name for, and its sandbox bind is `noexec` so it cannot
    /// be executed where it lies. Neither stops a step COPYING a payload and
    /// running or linking the copy; §B.8 carries that concession and what
    /// closing it would cost, and this doc deliberately claims no more than the
    /// two properties above.
    pub payload_inputs: Option<Vec<String>>,
    /// Immutable application-package metadata. The declaration omits identity,
    /// version and provenance; the derivation assembler binds those from this
    /// final Recipe and materializes `{out}/manifest` for application-capable
    /// build systems; bootstrap trust-root systems refuse the declaration.
    pub application: Option<ApplicationDeclaration>,
    /// Authored launcher presentation. The derivation assembler binds the final
    /// recipe name and materializes the canonical export beside the manifest.
    pub application_launcher: Option<LauncherDeclaration>,
    /// Immutable permission defaults compiled into the jail spec. Kept beside,
    /// not inside, the package manifest because an operator override has a
    /// separate lifecycle and the manifest is not a mount plan.
    pub application_permissions: Option<PermissionPolicy>,
    /// The `mesboot` build system's typed step list (#378 slices 2+3).
    pub steps: Option<Vec<Step>>,
    pub configure_flags: Option<Vec<String>>,
    pub make_flags: Option<Vec<String>>,
    pub outputs: Option<Vec<String>>,
    pub phases: Option<Vec<Phase>>,
    pub tests: Option<bool>,
    pub bins: Option<Vec<String>>,
    /// Relative path from the materialized source root to a self-contained Cargo
    /// workspace. No ancestor below the source root may carry another Cargo.toml,
    /// because Cargo could select that ancestor's lock. Absent means the source root
    /// itself, preserving existing recipes.
    pub cargo_subdir: Option<String>,
    /// Cargo package selected from the workspace. Absent builds the workspace's
    /// normal default target set, preserving existing recipes. When set, every
    /// `bins` entry must name a binary target owned by this package.
    pub cargo_package: Option<String>,
    pub no_default_features: Option<bool>,
    pub features: Option<Vec<String>>,
    /// Package-owned behavioral/reproducibility checks. The gate runner consumes
    /// these through `td-recipe-eval check-*`; the build path ignores them.
    pub checks: Option<Vec<RecipeCheck>>,
    /// Recipe-owned fixed-output source pins. The recipe/check/feed surfaces
    /// consume these; the build JSON deliberately omits them because `sourceInput`
    /// is the staged input key the builder already understands.
    pub source_pins: Option<Vec<SourcePin>>,
    /// Exact multi-object OSTree deploy pins. Like `source_pins`, these are
    /// recipe-evaluator metadata and are omitted from build JSON; only the
    /// aggregate/source-specific foreign marks cross that boundary.
    pub ostree_pins: Option<Vec<OstreePin>>,
    /// Repo-relative path to the committed Cargo.lock that pins this rust recipe's
    /// crate closure. Under `build-plan --auto` the builder verifies every vendored
    /// `.crate` against this lock's checksums before admitting the closure — the
    /// committed-checksum ingress that lets a rust node build in the graph without
    /// reopening the #469 crate-provenance gate.
    pub cargo_lock: Option<String>,
    /// Replace the materialized source workspace's existing regular Cargo.lock with
    /// the exact committed `cargo_lock` before the frozen build. Absent verifies byte
    /// equality instead. This is an explicit escape hatch for reviewed normalized
    /// workspace locks; ordinary recipes must use the upstream source's embedded lock
    /// verbatim. It does not generate a lock for source that omits one.
    pub replace_cargo_lock: Option<bool>,
    /// Exact Cargo Git sources admitted by explicit review. Each declaration binds
    /// lock source id + commit to a fixed-output archive input and the packages td
    /// may expose from it. The input is also added to the ordinary source/tool graph;
    /// the Rust runner consumes it as source data and never invokes Git.
    pub cargo_git_sources: Option<Vec<CargoGitSource>>,
    /// Reviewed, literal edits to Cargo manifests or build scripts in the
    /// selected workspace. Each edit pins its expected occurrence count so
    /// upstream drift fails before Cargo runs.
    pub cargo_source_patches: Option<Vec<CargoSourcePatch>>,
    /// Repo-relative path to an IN-TREE source directory this recipe builds from
    /// (#469 local-source provenance). Set via `local_source`, which also points
    /// `source_input` at this recipe's own `<name>-source` key. The runner
    /// (`td-recipe-eval`) copies the committed tree into the store and interns it
    /// under that key, pinned by the compiled seed-digest table. Like `cargo_lock`
    /// and `source_pins` it is a runner-side concern and is deliberately omitted
    /// from the build JSON — the builder only needs `sourceInput`.
    pub local_source: Option<String>,
}

#[derive(Clone)]
pub struct RecipeCheck {
    pub script: String,
    pub runner: Option<CheckRunner>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckRunner {
    BuildOnly,
    Codex,
    RustToolchain,
}

impl RecipeCheck {
    pub fn new(script: &str) -> RecipeCheck {
        RecipeCheck {
            script: script.into(),
            runner: None,
        }
    }

    pub fn with_runner(mut self, runner: CheckRunner) -> RecipeCheck {
        self.runner = Some(runner);
        self
    }
}

impl Recipe {
    fn base(name: &str, version: &str, bs: BuildSystem) -> Recipe {
        Recipe {
            name: name.into(),
            version: version.into(),
            source: None,
            source_input: None,
            build_system: bs,
            inputs: None,
            native_inputs: None,
            payload_inputs: None,
            application: None,
            application_launcher: None,
            application_permissions: None,
            steps: None,
            configure_flags: None,
            make_flags: None,
            outputs: None,
            phases: None,
            tests: None,
            bins: None,
            cargo_subdir: None,
            cargo_package: None,
            no_default_features: None,
            features: None,
            checks: None,
            source_pins: None,
            ostree_pins: None,
            cargo_lock: None,
            replace_cargo_lock: None,
            cargo_git_sources: None,
            cargo_source_patches: None,
            local_source: None,
        }
    }
    pub fn gnu(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Gnu)
    }
    pub fn rust(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Rust)
    }
    pub fn cmake(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Cmake)
    }
    /// The stage0 SEED build system (#378): the pinned upstream source tarball
    /// rides in through the lock's `<name>-source` entry, unpacked and interned
    /// by the caller.
    pub fn stage0(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Stage0)
    }
    /// A bootstrap-ladder rung (#378 slices 2+3): typed `steps` executed by the
    /// engine's build::run_mesboot; `native_inputs` name the prior rungs.
    pub fn mesboot(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Mesboot)
    }
    /// The stage0-only Rust snapshot transform. The final `rust-toolchain` is a
    /// separate source build and must never use this build-system constructor.
    pub fn rust_stage0(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::RustStage0)
    }

    /// Attaches source pins like `inputs` does, and that is not symmetry for its
    /// own sake: `check_runner` resolves pin keys out of `inputs`,
    /// `native_inputs` AND `payload_inputs`, staging each as a seed source — so
    /// a pin named here had its bytes fetched and interned while the recipe
    /// carried no record of it. §B.8's table names `nativeInputs` as one of the
    /// two channels a marked path must be REFUSED on, and a channel the recipe
    /// cannot see cannot refuse anything. No shipped recipe names a pin key
    /// here today, so this changes no emitted recipe.
    pub fn native_inputs(mut self, xs: &[&str]) -> Recipe {
        self.native_inputs = Some(vs(xs));
        self.add_source_pins_for_keys(xs.iter().copied());
        self.add_ostree_pins_for_keys(xs.iter().copied());
        self
    }
    /// Declare DATA inputs — see `Recipe::payload_inputs`. A path named here is
    /// staged for `Unpack`, `CopyTree`, `StageRuntimeClosure`, or
    /// `CompileApplicationTables` to read and has no name in command-step
    /// template expansion.
    pub fn payload_inputs(mut self, xs: &[&str]) -> Recipe {
        self.payload_inputs = Some(vs(xs));
        self
    }
    pub fn application(mut self, declaration: ApplicationDeclaration) -> Recipe {
        self.application = Some(declaration);
        self
    }
    pub fn application_launcher(mut self, launcher: LauncherDeclaration) -> Recipe {
        self.application_launcher = Some(launcher);
        self
    }
    pub fn application_permissions(mut self, permissions: PermissionPolicy) -> Recipe {
        self.application_permissions = Some(permissions);
        self
    }
    pub fn steps(mut self, xs: Vec<Step>) -> Recipe {
        self.steps = Some(xs);
        self
    }

    pub fn source(mut self, src: Source) -> Recipe {
        self.source = Some(src);
        self
    }
    /// Declare the tool/source MAP KEY this rung's own `<name>-source` lock
    /// entry resolves from (see `source_input`'s doc comment). A recipe with no
    /// source of its own (make-test) simply never calls this.
    pub fn source_input(mut self, key: &str) -> Recipe {
        self.source_input = Some(key.into());
        self.add_source_pin_for_key(key);
        self.add_ostree_pin_for_key(key);
        self
    }
    pub fn inputs(mut self, xs: &[&str]) -> Recipe {
        let mut inputs = vs(xs);
        if let Some(sources) = &self.cargo_git_sources {
            for source in sources {
                if !inputs.contains(&source.input) {
                    inputs.push(source.input.clone());
                }
            }
        }
        self.inputs = Some(inputs);
        self.add_source_pins_for_keys(xs.iter().copied());
        self.add_ostree_pins_for_keys(xs.iter().copied());
        self
    }
    /// Owned-string variant of `inputs`, for `ladder::mesboot0_inputs(...)` which
    /// assembles the extras + MESBOOT0_TOOLS list at runtime.
    pub fn inputs_owned(mut self, xs: Vec<String>) -> Recipe {
        self.add_source_pins_for_keys(xs.iter().map(String::as_str));
        self.add_ostree_pins_for_keys(xs.iter().map(String::as_str));
        let mut inputs = xs;
        if let Some(sources) = &self.cargo_git_sources {
            for source in sources {
                if !inputs.contains(&source.input) {
                    inputs.push(source.input.clone());
                }
            }
        }
        self.inputs = Some(inputs);
        self
    }
    pub fn configure_flags(mut self, xs: &[&str]) -> Recipe {
        self.configure_flags = Some(vs(xs));
        self
    }
    pub fn make_flags(mut self, xs: &[&str]) -> Recipe {
        self.make_flags = Some(vs(xs));
        self
    }
    pub fn outputs(mut self, xs: &[&str]) -> Recipe {
        self.outputs = Some(vs(xs));
        self
    }
    pub fn phases(mut self, p: Vec<Phase>) -> Recipe {
        self.phases = Some(p);
        self
    }
    pub fn tests(mut self, t: bool) -> Recipe {
        self.tests = Some(t);
        self
    }
    pub fn bins(mut self, xs: &[&str]) -> Recipe {
        self.bins = Some(vs(xs));
        self
    }
    pub fn cargo_subdir(mut self, path: &str) -> Recipe {
        self.cargo_subdir = Some(path.into());
        self
    }
    pub fn cargo_package(mut self, package: &str) -> Recipe {
        self.cargo_package = Some(package.into());
        self
    }
    pub fn no_default_features(mut self) -> Recipe {
        self.no_default_features = Some(true);
        self
    }
    pub fn features(mut self, xs: &[&str]) -> Recipe {
        self.features = Some(vs(xs));
        self
    }
    pub fn checks(mut self, xs: Vec<RecipeCheck>) -> Recipe {
        self.checks = Some(xs);
        self
    }
    pub fn source_pin(mut self, pin: SourcePin) -> Recipe {
        self.push_source_pin(pin);
        self
    }
    /// Name the committed Cargo.lock (repo-relative) that pins this rust recipe's
    /// crate closure for the `--auto` committed-checksum vendor gate.
    pub fn cargo_lock(mut self, path: &str) -> Recipe {
        self.cargo_lock = Some(path.into());
        self
    }
    /// Make the exact committed Cargo.lock authoritative for the materialized
    /// workspace. The default is stricter: require the source's lock to match it.
    pub fn replace_cargo_lock(mut self) -> Recipe {
        self.replace_cargo_lock = Some(true);
        self
    }
    /// Admit explicitly reviewed Cargo Git dependencies through fixed-output
    /// commit archives. The archive inputs join `inputs` whichever order these
    /// setters are called; downstream seed provenance still authenticates the
    /// resolved bytes before they may be staged.
    pub fn cargo_git_sources(mut self, sources: Vec<CargoGitSource>) -> Recipe {
        for source in &sources {
            let inputs = self.inputs.get_or_insert_with(Vec::new);
            if !inputs.contains(&source.input) {
                inputs.push(source.input.clone());
            }
            self.add_source_pin_for_key(&source.input);
        }
        self.cargo_git_sources = Some(sources);
        self
    }
    pub fn cargo_source_patches(mut self, patches: Vec<CargoSourcePatch>) -> Recipe {
        self.cargo_source_patches = Some(patches);
        self
    }
    /// Build this recipe from an IN-TREE source directory (repo-relative `path`),
    /// interned as its own `<name>-source` seed input by the runner (#469
    /// local-source provenance). Unlike `source`/`source_input` this declares NO
    /// fixed-output fetch: the bytes are the committed tree, pinned by the compiled
    /// seed-digest table. Sets `source_input` directly (no fetch pin) so the build
    /// plan aliases in the interned tree under `<name>-source`.
    pub fn local_source(mut self, path: &str) -> Recipe {
        self.source_input = Some(format!("{}-source", self.name));
        self.local_source = Some(path.into());
        self
    }
    pub fn source_pins(mut self, pins: Vec<SourcePin>) -> Recipe {
        for pin in pins {
            self.push_source_pin(pin);
        }
        self
    }

    fn add_source_pins_for_keys<'a, I>(&mut self, keys: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for key in keys {
            self.add_source_pin_for_key(key);
        }
    }

    fn add_source_pin_for_key(&mut self, key: &str) {
        if let Some(pin) = crate::source_pins::by_key(key) {
            self.push_source_pin(pin);
        }
    }

    fn add_ostree_pin_for_key(&mut self, key: &str) {
        if let Some(pin) = crate::ostree_pins::by_key(key) {
            self.push_ostree_pin(pin);
        }
    }

    fn add_ostree_pins_for_keys<'a, I>(&mut self, keys: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for key in keys {
            self.add_ostree_pin_for_key(key);
        }
    }

    fn push_source_pin(&mut self, pin: SourcePin) {
        let pins = self.source_pins.get_or_insert_with(Vec::new);
        if let Some(existing) = pins.iter_mut().find(|existing| existing.key == pin.key) {
            // Dedup by key is pre-existing; dropping the newcomer's MARK with it
            // would not be. Two pins under one key disagreeing about §B.8's
            // answer is a conflict either order could hide, so the taint sticks
            // to the pin that is kept.
            existing.foreign |= pin.foreign();
            return;
        }
        pins.push(pin);
    }

    fn push_ostree_pin(&mut self, pin: OstreePin) {
        let pins = self.ostree_pins.get_or_insert_with(Vec::new);
        if let Some(existing) = pins.iter_mut().find(|existing| existing.key == pin.key) {
            existing.foreign |= pin.foreign();
            return;
        }
        pins.push(pin);
    }

    /// Whether this recipe's own source is a §B.8 foreign payload.
    ///
    /// COMPUTED from the pins rather than cached beside them: `source_pins` is
    /// public and consumers hold it, so a cache is a second answer that anything
    /// appending to the vector — or flipping a mark on a pin already in it —
    /// silently desyncs. There is one source of truth and this reads it.
    ///
    /// It has to exist at all because `to_json` deliberately DROPS `source_pins`
    /// (`source_pins_are_recipe_metadata_not_build_json`): a pin is how a recipe
    /// was authored, and emitting pins into the build JSON would change every
    /// derivation hash in the tree. So the mark crosses that wall on its own key,
    /// or the rule holds in only one of the two places a plan is built.
    pub fn is_foreign(&self) -> bool {
        self.source_pins
            .as_ref()
            .is_some_and(|pins| pins.iter().any(|pin| pin.foreign()))
            || self
                .ostree_pins
                .as_ref()
                .is_some_and(|pins| pins.iter().any(|pin| pin.foreign()))
    }

    /// Whether `source_input` itself resolves to a foreign pin. A recipe may be
    /// foreign because some other attached pin is foreign, so the aggregate mark
    /// cannot decide how its own source is staged.
    pub fn is_foreign_source(&self) -> bool {
        let Some(source) = &self.source_input else {
            return false;
        };
        let canonical = crate::source_pins::by_key(source)
            .map(|pin| pin.key)
            .unwrap_or_else(|| source.clone());
        self.source_pins
            .as_ref()
            .is_some_and(|pins| pins.iter().any(|pin| pin.key == canonical && pin.foreign()))
            || self
                .ostree_pins
                .as_ref()
                .is_some_and(|pins| pins.iter().any(|pin| pin.key == *source && pin.foreign()))
    }

    /// The build system as its JSON/lowering token ("gnu"/"rust"/"cmake"/"stage0").
    pub fn build_system_name(&self) -> &'static str {
        self.build_system.as_str()
    }

    pub fn to_json(&self) -> Json {
        let mut o = vec![
            ("name".into(), Json::Str(self.name.clone())),
            ("version".into(), Json::Str(self.version.clone())),
        ];
        if let Some(src) = &self.source {
            o.push(("source".into(), src.to_json()));
        }
        if let Some(k) = &self.source_input {
            o.push(("sourceInput".into(), Json::Str(k.clone())));
        }
        o.push((
            "buildSystem".into(),
            Json::Str(self.build_system.as_str().into()),
        ));
        if let Some(x) = &self.inputs {
            o.push(("inputs".into(), arr(x)));
        }
        if let Some(x) = &self.native_inputs {
            o.push(("nativeInputs".into(), arr(x)));
        }
        // Emitted only when declared, so every recipe that has none hashes
        // exactly as it did — this key must not rebuild the world to land.
        if let Some(x) = &self.payload_inputs {
            o.push(("payloadInputs".into(), arr(x)));
        }
        // Likewise emitted only when TRUE, and never as `false`: the mark must
        // cross into the build JSON (§B.8 — `build_plan_auto` sees nothing else),
        // and a `"foreign":false` on every recipe would change every derivation
        // hash in the tree to say what their absence already says.
        if self.is_foreign() {
            o.push(("foreign".into(), Json::Bool(true)));
        }
        // Source-specific because the aggregate `foreign` mark can come from a
        // different attached pin. The builder uses this to withhold only the
        // prebuilt source from its command-visible map.
        if self.is_foreign_source() {
            o.push(("foreignSource".into(), Json::Bool(true)));
        }
        if let Some(application) = &self.application {
            o.push(("application".into(), application.to_json()));
        }
        if let Some(launcher) = &self.application_launcher {
            o.push(("applicationLauncher".into(), launcher.to_json()));
        }
        if let Some(permissions) = &self.application_permissions {
            o.push((
                "applicationPermissions".into(),
                Json::Str(permissions.to_keyfile()),
            ));
        }
        if let Some(x) = &self.steps {
            o.push((
                "steps".into(),
                Json::Arr(x.iter().map(|s| s.to_json()).collect()),
            ));
        }
        if let Some(x) = &self.configure_flags {
            o.push(("configureFlags".into(), arr(x)));
        }
        if let Some(x) = &self.make_flags {
            o.push(("makeFlags".into(), arr(x)));
        }
        if let Some(x) = &self.outputs {
            o.push(("outputs".into(), arr(x)));
        }
        if let Some(x) = &self.phases {
            o.push((
                "phases".into(),
                Json::Arr(x.iter().map(|p| p.to_json()).collect()),
            ));
        }
        if let Some(t) = self.tests {
            o.push(("tests".into(), Json::Bool(t)));
        }
        if let Some(x) = &self.bins {
            o.push(("bins".into(), arr(x)));
        }
        if let Some(path) = &self.cargo_subdir {
            o.push(("cargoSubdir".into(), Json::Str(path.clone())));
        }
        if let Some(package) = &self.cargo_package {
            o.push(("cargoPackage".into(), Json::Str(package.clone())));
        }
        if let Some(b) = self.no_default_features {
            o.push(("noDefaultFeatures".into(), Json::Bool(b)));
        }
        if let Some(x) = &self.features {
            o.push(("features".into(), arr(x)));
        }
        if let Some(l) = &self.cargo_lock {
            o.push(("cargoLock".into(), Json::Str(l.clone())));
        }
        if let Some(replace) = self.replace_cargo_lock {
            o.push(("replaceCargoLock".into(), Json::Bool(replace)));
        }
        if let Some(sources) = &self.cargo_git_sources {
            o.push((
                "cargoGitSources".into(),
                Json::Arr(sources.iter().map(CargoGitSource::to_json).collect()),
            ));
        }
        if let Some(patches) = &self.cargo_source_patches {
            o.push((
                "cargoSourcePatches".into(),
                Json::Arr(patches.iter().map(CargoSourcePatch::to_json).collect()),
            ));
        }
        Json::Obj(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_recipe_emits_expected_shape() {
        let r = Recipe::gnu("fixture", "1.0").source(Source::one(
            "mirror://gnu/fixture/fixture-1.0.tar.gz",
            "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js",
        ));
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","name":"fixture","source":{"sha256":"1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js","uri":"mirror://gnu/fixture/fixture-1.0.tar.gz"},"version":"1.0"}"#
        );
    }

    #[test]
    fn optional_fields_are_omitted_when_unset() {
        let r = Recipe::rust("cat", "0.9.0").bins(&["cat"]);
        // no source / inputs / tests keys
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"bins":["cat"],"buildSystem":"rust","name":"cat","version":"0.9.0"}"#
        );
    }

    #[test]
    fn cargo_lock_replacement_is_explicit_recipe_data() {
        let plain = Recipe::rust("tool", "1.0")
            .bins(&["tool"])
            .cargo_lock("recipes/locks/tool/Cargo.lock");
        assert_eq!(
            plain.to_json().to_canonical(),
            r#"{"bins":["tool"],"buildSystem":"rust","cargoLock":"recipes/locks/tool/Cargo.lock","name":"tool","version":"1.0"}"#
        );
        assert_eq!(
            plain.replace_cargo_lock().to_json().to_canonical(),
            r#"{"bins":["tool"],"buildSystem":"rust","cargoLock":"recipes/locks/tool/Cargo.lock","name":"tool","replaceCargoLock":true,"version":"1.0"}"#
        );
    }

    #[test]
    fn cargo_git_sources_are_typed_and_attach_their_fixed_output_inputs() {
        let source = CargoGitSource::new(
            "git+https://example.invalid/tool?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef0123456789abcdef01234567",
            "tool-git-source",
            vec![CargoGitPackage::new("tool", "1.2.3", ".")],
        );
        let recipe = Recipe::rust("consumer", "1")
            .cargo_git_sources(vec![source.clone()])
            .inputs(&["rust-toolchain"]);
        assert_eq!(
            recipe.to_json().to_canonical(),
            r#"{"buildSystem":"rust","cargoGitSources":[{"input":"tool-git-source","packages":[{"name":"tool","path":".","version":"1.2.3"}],"source":"git+https://example.invalid/tool?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef0123456789abcdef01234567"}],"inputs":["rust-toolchain","tool-git-source"],"name":"consumer","version":"1"}"#
        );
        let owned = Recipe::rust("consumer", "1")
            .cargo_git_sources(vec![source])
            .inputs_owned(vec!["rust-toolchain".into()]);
        assert_eq!(
            owned.inputs,
            Some(vec!["rust-toolchain".into(), "tool-git-source".into()])
        );
    }

    #[test]
    fn rust_workspace_selection_is_explicit_recipe_data() {
        let r = Recipe::rust("codex", "0.149.1")
            .bins(&["codex"])
            .cargo_subdir("codex-rs")
            .cargo_package("codex-cli");
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"bins":["codex"],"buildSystem":"rust","cargoPackage":"codex-cli","cargoSubdir":"codex-rs","name":"codex","version":"0.149.1"}"#
        );
    }

    #[test]
    fn cargo_source_patches_are_literal_count_checked_recipe_data() {
        let r = Recipe::rust("tool", "1").cargo_source_patches(vec![CargoSourcePatch::new(
            "nested/Cargo.toml",
            vec![TextEdit::new("native-tls", "rustls", 1)],
        )]);
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"buildSystem":"rust","cargoSourcePatches":[{"edits":[{"expect":"1","from":"native-tls","to":"rustls"}],"file":"nested/Cargo.toml"}],"name":"tool","version":"1"}"#
        );
    }

    /// The payload channel is a SEPARATE key, and its absence is byte-identical
    /// to the shape that existed before it — otherwise landing §B.8's channel
    /// would change every derivation hash in the tree for a metadata reason,
    /// which is the same argument that keeps `source_pins` out of this JSON.
    #[test]
    fn the_payload_channel_is_its_own_key_and_costs_nothing_unset() {
        let plain = Recipe::gnu("fixture", "1.0").inputs(&["gcc"]);
        assert_eq!(
            plain.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","inputs":["gcc"],"name":"fixture","version":"1.0"}"#,
            "a recipe declaring no payload must hash exactly as it did before"
        );
        // Declared, it is neither merged into `inputs` nor able to displace one:
        // the two lists travel side by side, which is the whole point of the
        // channel — an edge that says WHY it is there.
        let with = plain.clone().payload_inputs(&["firefox"]);
        assert_eq!(
            with.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","inputs":["gcc"],"name":"fixture","payloadInputs":["firefox"],"version":"1.0"}"#
        );
    }

    #[test]
    fn an_application_declaration_carries_no_snapshot_of_final_recipe_answers() {
        let declaration = ApplicationDeclaration::new("runtime", "/app/bin/app").unwrap();
        let mut recipe = Recipe::gnu("before", "1")
            .application(declaration)
            .source_pin(SourcePin::new("app-source", "u", "s", "f").mark_foreign());
        recipe.name = "after".into();
        recipe.version = "2".into();
        let json = recipe.to_json();
        assert_eq!(
            json.to_canonical(),
            r#"{"application":{"entry":"/app/bin/app","runtime":"runtime"},"buildSystem":"gnu","foreign":true,"name":"after","version":"2"}"#
        );
        let application = json.get("application").unwrap();
        assert!(application.get("name").is_none());
        assert!(application.get("version").is_none());
        assert!(application.get("provenance").is_none());
    }

    /// §B.8's containment edge must attach NO pin, and that is what keeps the
    /// taint off it: `inputs`, `native_inputs` and `source_input` all attach, so
    /// an image naming a payload as DATA would otherwise be marked for the
    /// payload's own pin — making `root.erofs`, the deployment and td foreign
    /// outputs, which §B.8 calls absurd and the end of the closure query.
    #[test]
    fn the_containment_edge_attaches_no_source_pin() {
        // A REAL pin key: with a made-up one nothing attaches on any channel and
        // the test cannot tell a channel that attaches from one that does not.
        const REAL: &str = "stage0-source";
        assert!(crate::source_pins::by_key(REAL).is_some(), "fixture guard");
        let data = Recipe::gnu("image", "1").payload_inputs(&[REAL]);
        assert!(
            data.source_pins.is_none(),
            "the DATA channel must not attach a pin"
        );
        assert!(!data.is_foreign());
        let tool = Recipe::gnu("builder", "1").inputs(&[REAL]);
        assert_eq!(tool.source_pins.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn recipe_checks_are_not_build_json() {
        let r = Recipe::gnu("fixture", "1.0").checks(vec![RecipeCheck::new("echo ok")]);
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","name":"fixture","version":"1.0"}"#
        );
    }

    #[test]
    fn source_pins_are_recipe_metadata_not_build_json() {
        let r = Recipe::gnu("stage0", "1.9.1").source_input("stage0-source");
        assert_eq!(r.source_pins.as_ref().unwrap().len(), 1);
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","name":"stage0","sourceInput":"stage0-source","version":"1.9.1"}"#
        );
    }

    /// §B.8's mark crosses the wall the test above keeps `source_pins` behind.
    /// The pin is the source of truth and stays out of the build JSON; the
    /// derived flag is what `build_plan_auto` — which sees the emitted JSON and
    /// nothing else — has to read, so a landing that marked the pin and stopped
    /// would enforce the rule in one of the two places a plan is built.
    #[test]
    fn a_foreign_pin_marks_the_recipe_and_the_mark_reaches_the_build_json() {
        let pin = SourcePin::new("app-source", "u", "s", "f").mark_foreign();
        let r = Recipe::gnu("app", "1.0").source_pin(pin);
        assert!(r.is_foreign());
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","foreign":true,"name":"app","version":"1.0"}"#
        );
        // An ordinary pin attached afterwards describes ITSELF, not the recipe.
        let both = r.source_pin(SourcePin::new("zlib", "u", "s", "f"));
        assert!(both.is_foreign(), "an ordinary pin must not clear the mark");
        // ...and in the other order, since `inputs` attaches pins too.
        let ordinary_first = Recipe::gnu("app", "1.0")
            .source_pin(SourcePin::new("zlib", "u", "s", "f"))
            .source_pin(SourcePin::new("app-source", "u", "s", "f").mark_foreign());
        assert!(ordinary_first.is_foreign());
    }

    #[test]
    fn the_source_specific_mark_distinguishes_an_own_source_from_another_pin() {
        let seed = Recipe::mesboot("seed", "1").source_input("ripgrep-seed-source");
        assert!(seed.is_foreign());
        assert!(seed.is_foreign_source());
        assert_eq!(
            seed.to_json().to_canonical(),
            r#"{"buildSystem":"mesboot","foreign":true,"foreignSource":true,"name":"seed","sourceInput":"ripgrep-seed-source","version":"1"}"#
        );

        let other = Recipe::mesboot("other", "1")
            .source_input("stage0-source")
            .inputs(&["ripgrep-seed-source"]);
        assert!(other.is_foreign(), "the aggregate mark still sees the other pin");
        assert!(!other.is_foreign_source());
        assert!(other.to_json().get("foreignSource").is_none());
    }

    /// The taint is STICKY under dedup, both orders. `push_source_pin` drops a
    /// pin whose key it already holds — pre-existing and right — but dropping
    /// its MARK with it would let the answer depend on which of two pins under
    /// one key happened to arrive first.
    #[test]
    fn deduplicating_two_pins_under_one_key_keeps_the_mark() {
        let plain = SourcePin::new("k", "u", "s", "f");
        let marked = SourcePin::new("k", "u", "s", "f").mark_foreign();
        let marked_second = Recipe::gnu("app", "1.0")
            .source_pin(plain.clone())
            .source_pin(marked.clone());
        assert!(
            marked_second.is_foreign(),
            "a foreign pin discarded as a duplicate must still mark the recipe"
        );
        let marked_first = Recipe::gnu("app", "1.0")
            .source_pin(marked)
            .source_pin(plain);
        assert!(marked_first.is_foreign());
        // Still one pin: this is the dedup, not a second entry.
        assert_eq!(marked_second.source_pins.as_ref().map(Vec::len), Some(1));
    }

    /// `source_pins` is public and consumers hold it, so the answer must be
    /// READ from it rather than cached beside it — a cache is a second answer
    /// that appending to the vector, or flipping a mark on a pin already in it,
    /// silently desyncs.
    #[test]
    fn the_answer_is_read_from_the_pins_not_cached_beside_them() {
        let mut r = Recipe::gnu("app", "1.0").source_pin(SourcePin::new("k", "u", "s", "f"));
        assert!(!r.is_foreign());
        if let Some(pins) = r.source_pins.as_mut() {
            if let Some(pin) = pins.first_mut() {
                pin.foreign = true;
            }
        }
        assert!(
            r.is_foreign(),
            "mutating a held pin must change the recipe's answer"
        );
        assert!(r.to_json().to_canonical().contains(r#""foreign":true"#));
    }

    /// Landing the mark rebuilds nothing: the key is emitted only when TRUE, so
    /// every recipe in the tree hashes exactly as it did. `false` would be the
    /// same statement as the key's absence and would change every hash to make
    /// it.
    #[test]
    fn the_mark_costs_an_ordinary_recipe_nothing_in_the_build_json() {
        let plain = Recipe::gnu("fixture", "1.0").inputs(&["gcc"]);
        assert!(!plain.is_foreign());
        assert_eq!(
            plain.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","inputs":["gcc"],"name":"fixture","version":"1.0"}"#
        );
        assert!(
            plain.to_json().get("foreign").is_none(),
            "no key at all, not `foreign:false'"
        );
        // The whole ordinary catalog, which is the claim that matters. Asked for
        // the KEY rather than matched as text: a check-script body containing the
        // word would otherwise false-red it.
        for (stem, recipe) in crate::catalog::all() {
            if matches!(
                stem,
                "ripgrep-seed" | "firefox" | "freedesktop-platform-25-08"
            ) {
                assert!(recipe.to_json().get("foreign").is_some());
                assert!(recipe.to_json().get("foreignSource").is_some());
                continue;
            }
            assert!(
                recipe.to_json().get("foreign").is_none(),
                "ordinary recipe {stem} emits the mark"
            );
            assert!(recipe.to_json().get("foreignSource").is_none());
        }
    }

    /// Both input channels attach the pin a key resolves to. `native_inputs` did
    /// NOT before this commit, and `check_runner` resolves pin keys out of that
    /// list too — so a foreign pin named there would have had its bytes fetched
    /// and interned while the recipe carried no record of it, the funnel claim
    /// failing on one of the two channels §B.8's table names explicitly.
    ///
    /// `inputs` is asserted beside it because nothing asserted it either:
    /// deleting its attachment left the whole suite green, and thirty shipped
    /// recipes reach the fetch/warm surface through it.
    #[test]
    fn both_input_channels_attach_the_pin_a_key_resolves_to() {
        for r in [
            Recipe::gnu("x", "1").native_inputs(&["zlib-x86-64-source"]),
            Recipe::gnu("x", "1").inputs(&["zlib-x86-64-source"]),
        ] {
            assert_eq!(
                r.source_pins.as_ref().map(Vec::len),
                Some(1),
                "a pin key in an input list must attach its pin"
            );
        }
        // Not a pin key: no pin, and no error either — these lists are mostly
        // other recipes' outputs.
        assert!(Recipe::gnu("x", "1").native_inputs(&["gcc"]).source_pins.is_none());
        assert!(Recipe::gnu("x", "1").inputs(&["gcc"]).source_pins.is_none());
    }

    #[test]
    fn both_tool_channels_attach_the_ostree_pin_they_will_refuse() {
        for recipe in [
            Recipe::gnu("x", "1").inputs(&["firefox-154-source"]),
            Recipe::gnu("x", "1").native_inputs(&["firefox-154-source"]),
            Recipe::gnu("x", "1").inputs_owned(vec!["firefox-154-source".into()]),
        ] {
            let pins = recipe
                .ostree_pins
                .as_ref()
                .expect("the classified OSTree pin is attached");
            assert_eq!(pins.len(), 1);
            assert_eq!(
                pins.first().map(|pin| pin.key.as_str()),
                Some("firefox-154-source")
            );
            assert!(recipe.is_foreign());
        }
    }

    // The wire contract builder::build::run_mesboot dispatches on ("mesBoot"
    // + the three expandable fields) — a drift here strands the step.
    #[test]
    fn mes_boot_step_emits_the_engine_dispatch_shape() {
        let s = Step::MesBoot {
            source: "{in:mes-source}".into(),
            nyacc: "{in:nyacc}".into(),
            stage0: "{in:stage0}".into(),
        };
        assert_eq!(
            s.to_json().to_canonical(),
            r#"{"mesBoot":{"nyacc":"{in:nyacc}","source":"{in:mes-source}","stage0":"{in:stage0}"}}"#
        );
    }

    #[test]
    fn deployment_steps_emit_the_engine_dispatch_shape() {
        let closure = Step::StageRuntimeClosure {
            roots: vec!["{in:uutils}".into()],
            dest: "{root}/real-root".into(),
        };
        assert_eq!(
            closure.to_json().to_canonical(),
            r#"{"stageRuntimeClosure":{"dest":"{root}/real-root","roots":["{in:uutils}"]}}"#
        );

        let pack = Step::PackErofs {
            root: "{root}/real-root".into(),
            output: "{out}/deployment/root.erofs".into(),
        };
        assert_eq!(
            pack.to_json().to_canonical(),
            r#"{"packErofs":{"output":"{out}/deployment/root.erofs","root":"{root}/real-root"}}"#
        );

        let manifest = Step::Sha256Manifest {
            output: "{out}/deployment/manifest".into(),
            entries: vec![
                ("bzImage".into(), "{out}/deployment/bzImage".into()),
                (
                    "initramfs.cpio".into(),
                    "{out}/deployment/initramfs.cpio".into(),
                ),
            ],
        };
        assert_eq!(
            manifest.to_json().to_canonical(),
            r#"{"sha256Manifest":{"entries":[["bzImage","{out}/deployment/bzImage"],["initramfs.cpio","{out}/deployment/initramfs.cpio"]],"output":"{out}/deployment/manifest"}}"#
        );
    }

    #[test]
    fn split_debug_tree_emits_the_engine_dispatch_shape() {
        let split = Step::split_debug_tree("{out}", "{in:binutils}/bin/objcopy");
        assert_eq!(
            split.to_json().to_canonical(),
            r#"{"splitDebugTree":{"objcopy":"{in:binutils}/bin/objcopy","root":"{out}"}}"#
        );
        let size = Step::assert_debug_size("{out}", "{out}/debug-size", "toolchain", 4096);
        assert_eq!(
            size.to_json().to_canonical(),
            r#"{"assertDebugSize":{"ceiling":4096,"report":"{out}/debug-size","root":"{out}","scope":"toolchain"}}"#
        );
        let compare = Step::compare_files("{root}/one", "{root}/two");
        assert_eq!(
            compare.to_json().to_canonical(),
            r#"{"compareFiles":{"left":"{root}/one","right":"{root}/two"}}"#
        );
    }
}
