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
    extra_dbs: &[String],
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
    let builder_override = builder_store.map(|(canonical, store_dir, db)| {
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        BuilderOverride {
            canonical: canonical.to_string(),
            on_disk: format!("{store_dir}/{base}"),
            db: db.to_string(),
        }
    });
    // The builder store path: the td-placed stage0 (override) or, by default, the
    // running binary (self_store_path — the guix-built td-builder).
    let builder_path = match &builder_override {
        Some(ov) => ov.canonical.clone(),
        None => self_store_path()?,
    };
    // td assembles the .drv ITSELF (pure Rust, no guix (derivation …), no Guile, no
    // daemon) and writes it to SCRATCH — the SAME assembly `assemble-recipe` uses, so a
    // separate process (the build daemon) realizes a byte-identical td-assembled drv.
    let (drv_path, drv_file, parsed, source) = assemble_recipe_drv(
        recipe_json,
        lock_file,
        scratch,
        &builder_path,
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
    // Content-addressed build cache: if SCRATCH already holds a valid realization of
    // this exact (deterministic) drv, reuse it — skip the build. The gate points
    // SCRATCH at a persistent cache, so an unchanged recipe is a cache HIT and only a
    // CHANGED recipe (⇒ different drv hash ⇒ different output path, a miss) rebuilds.
    if let Some(regs) = cached_realization(&parsed, scratch)? {
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
    // invocation built it — and its tree re-verifies, read it back instead of rebuilding.
    // The daemon's valid-path skip, backed by an on-disk store across process boundaries.
    if let Some((ps, pd)) = persist {
        if let Some(regs) = persistent_realization(&parsed, ps, Path::new(pd), scratch)? {
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
    // SUBSTITUTE-OR-BUILD (opt-in, TD_SUBST_URL): fetch the outputs from a substitute
    // server instead of building. OFF for the verification loop — it never sets the env,
    // so this is a no-op there (directive 1: the loop always builds from source + --check).
    if let Some(regs) = try_substitute(&parsed, &drv_path, scratch)? {
        eprintln!(
            "td-builder: build-recipe SUBSTITUTED {} output(s) for {drv_path} (verified signature + NarHash); skipping the build",
            regs.len()
        );
        println!("CACHE=subst");
        return Ok(regs);
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
