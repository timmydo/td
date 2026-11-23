#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::loader::{header, interp_segment, interpreter, listed, store_root};
use super::store::{check_tree, nar_hash_line, place, stale_partial, Placed, Store};
use super::*;

const PG: &str = "31f6m76fp7k1scqd064a892wz5d4aqcq-postgresql-x86-64-17.12";
const GLIBC: &str = "dgkqaph4a8q8hk5b5qg4prcp0q5q17x0-glibc-x86-64-2.41";
const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn job(closure: &str, reviewed: &str) -> Json {
    td_json::parse(&format!(
        r#"{{"mode":"postgres-binaries","closure":{closure},"postgres_launcher_minor":"{reviewed}"}}"#
    ))
    .unwrap()
}

fn entries(first: &str, second: &str) -> String {
    format!(
        r#"[{{"basename":"{first}","nar_sha256":"{HASH}"}},{{"basename":"{second}","nar_sha256":"{HASH}"}}]"#
    )
}

#[test]
fn the_job_names_postgresql_then_glibc_and_an_older_live_minor() {
    let closure = Closure::from_job(&job(&entries(PG, GLIBC), "17.6")).unwrap();
    assert_eq!((closure.minor, closure.live), (12, 6));
    assert!(closure.rebuilds_gin().unwrap());
    assert!(!Closure::from_job(&job(&entries(PG, GLIBC), "17.12"))
        .unwrap()
        .rebuilds_gin()
        .unwrap());
    assert_eq!(closure.basenames(), [PG, GLIBC]);
    assert_eq!(
        closure.postgres().unwrap(),
        Path::new(&format!("/td/store/{PG}/bin/postgres"))
    );
    Closure::from_job(&job(&entries(PG, GLIBC), "17.12")).unwrap();
    for refused in [
        job(&entries(GLIBC, PG), "17.6"),
        job(&entries(PG, PG), "17.6"),
        job(&entries(PG, GLIBC), "17.13"),
        job(&entries(PG, GLIBC), "16.4"),
        job(&entries(PG, GLIBC), "17."),
        job(&entries(&format!("../{PG}"), GLIBC), "17.6"),
        job(&entries(&format!("{PG}/x"), GLIBC), "17.6"),
        job(
            &format!(r#"[{{"basename":"{PG}","nar_sha256":"{HASH}"}}]"#),
            "17.6",
        ),
        job(
            &entries(PG, GLIBC).replacen(HASH, &HASH.to_uppercase(), 1),
            "17.6",
        ),
        td_json::parse(&format!(r#"{{"closure":{}}}"#, entries(PG, GLIBC))).unwrap(),
    ] {
        assert!(Closure::from_job(&refused).is_err(), "{refused}");
    }
}

#[test]
fn releases_come_from_the_banner_and_the_version_number() {
    assert_eq!(banner_minor("postgres (PostgreSQL) 17.12").unwrap(), 12);
    assert_eq!(
        banner_minor("postgres (PostgreSQL) 17.6 (Debian 17.6-1.pgdg120+1)").unwrap(),
        6
    );
    for refused in [
        "postgres (PostgreSQL) 16.4",
        "psql (PostgreSQL) 17.6",
        "postgres (PostgreSQL) 17.6beta1",
        "postgres (PostgreSQL) 17.x",
    ] {
        assert!(banner_minor(refused).is_err(), "{refused}");
    }
    assert_eq!(number_minor("170012").unwrap(), 12);
    assert!(number_minor("160004").is_err());
    assert!(number_minor("17.12").is_err());
}

fn property(path: &str, argv: &[&str], flags: &[&str]) -> Json {
    let strings = |items: &[&str]| Json::Arr(items.iter().map(|s| Json::from(*s)).collect());
    Json::Obj(vec![(
        "data".into(),
        Json::Arr(vec![Json::Arr(vec![
            Json::from(path),
            strings(argv),
            strings(flags),
            Json::from(0_u64),
        ])]),
    )])
}

#[test]
fn the_td_command_keeps_the_debian_arguments_exactly() {
    let debian = "/opt/td-mta/postgresql/usr/lib/postgresql/17/bin/postgres";
    let td = format!("/td/store/{PG}/bin/postgres");
    let argv = [
        debian,
        "-D",
        "/var/lib/td-mta/mail/postgres",
        "-c",
        "config_file=/var/lib/td-mta/mail/postgresql.conf",
    ];
    for flags in [&[][..], &["no-env-expand"][..]] {
        let current = property(debian, &argv, flags);
        let args = server_arguments(&current, debian).unwrap();
        assert_eq!(args, argv[1..]);
        let text = dropin_text(&td, &args).unwrap();
        assert_eq!(
            text,
            format!(
                "[Service]\nExecStart=\nExecStart=:{td} -D /var/lib/td-mta/mail/postgres -c config_file=/var/lib/td-mta/mail/postgresql.conf\nUnsetEnvironment=LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT\n"
            )
        );
        assert_eq!(dropin_program(&text).unwrap(), td);
        // The reloaded unit must say the same.
        let mut switched = argv;
        switched[0] = &td;
        let reloaded = property(&td, &switched, &["no-env-expand"]);
        assert_eq!(
            dropin_text(&td, &server_arguments(&reloaded, &td).unwrap()).unwrap(),
            text
        );
    }
    let mut wrapper = argv;
    wrapper[0] = "/opt/td-mta/postgresql/usr/bin/pg_ctlcluster";
    assert!(server_arguments(&property(wrapper[0], &wrapper, &[]), debian).is_err());
    assert!(server_arguments(&property(debian, &argv, &["ignore-failure"]), debian).is_err());
    assert!(server_arguments(&property("/bin/sh", &argv, &[]), debian).is_err());
    for argument in ["$PGDATA", "%h/data", "a b", "'x'", "x;y", ""] {
        let mut bad = argv;
        bad[2] = argument;
        assert!(
            server_arguments(&property(debian, &bad, &[]), debian).is_err(),
            "{argument}"
        );
    }
    let two = Json::Obj(vec![(
        "data".into(),
        Json::Arr(vec![
            property(debian, &argv, &[])
                .get("data")
                .unwrap()
                .as_arr()
                .unwrap()[0]
                .clone(),
            property(debian, &argv, &[])
                .get("data")
                .unwrap()
                .as_arr()
                .unwrap()[0]
                .clone(),
        ]),
    )]);
    assert!(server_arguments(&two, debian).is_err());
}

#[test]
fn only_a_drop_in_this_controller_writes_selects_store_tools() {
    let td = format!("/td/store/{PG}/bin/postgres");
    let text = dropin_text(&td, &["-D", "/data"]).unwrap();
    assert_eq!(dropin_program(&text).unwrap(), td);
    for refused in [
        text.replace("UnsetEnvironment", "Environment"),
        text.replace(
            &td,
            "/opt/td-mta/postgresql/usr/lib/postgresql/17/bin/postgres",
        ),
        text.replace(&td, &format!("/td/store/{GLIBC}/bin/postgres")),
        text.replace(&td, &format!("/td/store/../{PG}/bin/postgres")),
        text.replace(" -D", "  -D"),
        text.replace("ExecStart=:", "ExecStart="),
        format!("{text}Environment=LD_PRELOAD=/x\n"),
        text.replace("[Service]\nExecStart=\n", "[Service]\n"),
    ] {
        assert!(dropin_program(&refused).is_err(), "{refused}");
    }
    assert!(dropin_text(&td, &["$X"]).is_err());
}

#[test]
fn gate_rows_refuse_icu_lz4_and_newer_compression() {
    assert_eq!(cluster_gates("0|pglz|off|170006").unwrap(), 6);
    for wal in ["on", "pglz"] {
        cluster_gates(&format!("0|pglz|{wal}|170006")).unwrap();
    }
    for refused in [
        "1|pglz|off|170006",
        "0|lz4|off|170006",
        "0|pglz|lz4|170006",
        "0|pglz|zstd|170006",
        "0|pglz|off|160006",
        "0|pglz|off",
        "0|pglz|off|170006|x",
    ] {
        assert!(cluster_gates(refused).is_err(), "{refused}");
    }
    database_gates("0|0|0|0|0|0", true).unwrap();
    database_gates("0|0|0|0|3|0", false).unwrap();
    for refused in [
        "1|0|0|0|0|0",
        "0|1|0|0|0|0",
        "0|0|2|0|0|0",
        "0|0|0|1|0|0",
        "0|0|0|0|0|1",
        "0|0|0|0|0",
        "0|0|0|0|0|0|0",
    ] {
        assert!(database_gates(refused, false).is_err(), "{refused}");
        assert!(database_gates(refused, true).is_err(), "{refused}");
    }
    assert!(database_gates("0|0|0|0|3|0", true).is_err());
    assert_eq!(trial_cluster("t|0|170012").unwrap(), 12);
    for refused in ["f|0|170012", "t|1|170012", "t|0"] {
        assert!(trial_cluster(refused).is_err(), "{refused}");
    }
}

#[test]
fn the_lz4_scan_names_every_offending_column() {
    assert!(lz4_values("scanned").unwrap().is_empty());
    assert_eq!(
        lz4_values("td_mta.blobs.body\n\"odd\".x.\"two words\"").unwrap(),
        ["td_mta.blobs.body", "\"odd\".x.\"two words\""]
    );
    for refused in ["", "scanned\nscanned", "ERROR", "td_mta.t.c\n\n"] {
        assert!(lz4_values(refused).is_err(), "{refused:?}");
    }
    // Every table and populated materialized view, and the result as rows.
    assert!(LZ4_SCAN.contains("c.relkind IN ('r', 'm') AND c.relispopulated"));
    assert!(LZ4_SCAN.ends_with("'scanned');\n"));
}

#[test]
fn the_unit_may_name_the_debian_tree_only_in_its_server_command() {
    let debian = "/opt/td-mta/postgresql/usr/lib/postgresql/17/bin";
    let plain = "Type=exec\nExecCondition=\nExecStartPre=\nEnvironment=\n";
    unit_state(plain).unwrap();
    unit_state(&plain.replace("exec", "simple")).unwrap();
    // The loader variables the drop-in unsets may name it.
    unit_state(&plain.replace(
        "Environment=\n",
        "Environment=LD_LIBRARY_PATH=/opt/td-mta/postgresql/usr/lib/x86_64-linux-gnu LANG=C\n",
    ))
    .unwrap();
    for refused in [
        plain.replace("exec", "notify"),
        plain.replace("Type=exec\n", ""),
        format!(
            "{plain}ExecCondition={{ path={debian}/pg_ctl ; argv[]={debian}/pg_ctl status }}\n"
        ),
        format!(
            "{plain}ExecStartPre={{ path={debian}/postgres ; argv[]={debian}/postgres -C x }}\n"
        ),
        format!("{plain}ExecStopPost={{ path={debian}/pg_ctl }}\n"),
        plain.replace(
            "Environment=\n",
            "Environment=LD_LIBRARY_PATH=/x PATH=/opt/td-mta/postgresql/usr/lib/postgresql/17/bin:/usr/bin\n",
        ),
        format!("{plain}unparsed\n"),
    ] {
        assert!(unit_state(&refused).is_err(), "{refused}");
    }
    for property in ["ExecCondition", "Environment", "ExecStop", "Type"] {
        assert!(UNIT_PROPERTIES.contains(&property));
    }
}

#[test]
fn the_trigram_probe_must_plan_on_the_search_index() {
    let used = "1.6\nt\nAggregate\n  ->  Bitmap Heap Scan on blob_search\n        Recheck Cond: (candidate_text ~~ '%td-mta binary probe%'::text)\n        ->  Bitmap Index Scan on blob_search_candidates\n              Index Cond: (candidate_text ~~ '%td-mta binary probe%'::text)\n0";
    trigram(used).unwrap();
    for refused in [
        used.replace("\nt\n", "\nf\n"),
        used.replace("blob_search_candidates", "blob_search_pkey"),
        used.replace("\n0", "\nERROR"),
        used.replacen("1.6\n", "\n", 1),
        // The index name must be a plan line, not the count.
        "1.6\nt\nSeq Scan on blob_search\nBitmap Index Scan on blob_search_candidates".into(),
    ] {
        assert!(trigram(&refused).is_err(), "{refused}");
    }
}

#[test]
fn only_closure_code_may_be_mapped_executable() {
    let lib = format!("/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/lib/libc.so.6");
    let exe = format!("/td/store/{PG}/bin/postgres");
    let mut maps = format!(
        "55d0-55e0 r--p 00000000 08:01 11 {exe}\n\
         55e0-55f0 r-xp 00001000 08:01 11                         {exe}\n\
         7f00-7f10 r-xp 00028000 08:01 12                         {lib}\n\
         7f10-7f20 rw-p 00000000 00:00 0 \n\
         7f20-7f30 r--p 00000000 08:01 13                         /usr/lib/locale/locale-archive\n\
         7f30-7f40 rw-s 00000000 00:01 14                         /dev/shm/PostgreSQL.1 (deleted)\n\
         7ffd-7ffe r-xp 00000000 00:00 0                          [vdso]\n"
    );
    assert_eq!(foreign_code(&maps, &[PG, GLIBC]), None);
    assert_eq!(
        foreign_code(&maps, &[PG]),
        Some(lib.as_str()),
        "td glibc is part of the closure"
    );
    maps.push_str(
        "7f50-7f60 r-xp 00000000 08:01 15 /usr/lib/x86_64-linux-gnu/libnss_systemd.so.2\n",
    );
    assert_eq!(
        foreign_code(&maps, &[PG, GLIBC]),
        Some("/usr/lib/x86_64-linux-gnu/libnss_systemd.so.2")
    );
    let other = format!("7f70-7f80 r-xp 0 08:01 16 /td/store/{PG}-other/lib/x.so\n");
    assert!(foreign_code(&other, &[PG, GLIBC]).is_some());
}

#[test]
fn store_paths_hold_files_not_the_paths_themselves() {
    let libc = format!("/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/lib/libc.so.6");
    assert!(in_store(&libc, &[PG, GLIBC]));
    for refused in [
        format!("/td/store/{GLIBC}"),
        format!("/td/store/{GLIBC}/"),
        format!("/td/store/{GLIBC}/../{PG}/bin/postgres"),
        format!("/td/store/{GLIBC}/./lib/libc.so.6"),
        format!("/td/store/{GLIBC}/lib//libc.so.6"),
        format!("/td/store/{PG}x/bin/postgres"),
        format!("td/store/{PG}/bin/postgres"),
        "/usr/lib/x86_64-linux-gnu/libc.so.6".into(),
    ] {
        assert!(!in_store(&refused, &[PG, GLIBC]), "{refused}");
    }
    assert_eq!(
        store_root(&libc, "glibc-x86-64").unwrap(),
        Path::new(&format!("/td/store/{GLIBC}"))
    );
    assert!(store_root(&libc, "postgresql-x86-64").is_err());
    assert!(store_root("/opt/x/lib/libc.so.6", "glibc-x86-64").is_err());
}

#[test]
fn the_loader_listing_must_find_everything() {
    let g = format!("/td/store/{GLIBC}/stage/td/store/glibc-2.41-x86_64/lib");
    let report = format!(
        "\tlinux-vdso.so.1 (0x00007fc986e1d000)\n\
         \tlibpq.so.5 => /td/store/{PG}/lib/libpq.so.5 (0x00007fc986d3e000)\n\
         \tlibc.so.6 => {g}/libc.so.6 (0x00007fc986a58000)\n\
         \t{g}/ld-linux-x86-64.so.2 (0x00007fc986e1f000)\n"
    );
    assert_eq!(
        listed(&report).unwrap(),
        [
            format!("/td/store/{PG}/lib/libpq.so.5"),
            format!("{g}/libc.so.6"),
            format!("{g}/ld-linux-x86-64.so.2"),
        ]
    );
    assert!(listed("\tstatically linked\n").unwrap().is_empty());
    for refused in [
        format!("{report}\tlibfoo.so.1 => not found\n"),
        report.replace("linux-vdso.so.1", "libvdso.so.1"),
        format!("{report}\tgarbage\n"),
        format!("{report}\tlibm.so.6 => libm.so.6 (0x1)\n"),
    ] {
        assert!(listed(&refused).is_err(), "{refused}");
    }
}

// A minimal x86-64 ELF64 image: header, `phnum` program headers, and the
// interpreter string at 0x100 if PT_INTERP is among `types`.
fn elf_image(kind: u16, types: &[u32], interp: &[u8]) -> Vec<u8> {
    let mut image = vec![0u8; 0x200];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[16..18].copy_from_slice(&kind.to_le_bytes());
    image[18..20].copy_from_slice(&62u16.to_le_bytes());
    image[32..40].copy_from_slice(&64u64.to_le_bytes());
    image[54..56].copy_from_slice(&56u16.to_le_bytes());
    image[56..58].copy_from_slice(&(types.len() as u16).to_le_bytes());
    for (i, kind) in types.iter().enumerate() {
        let row = 64 + i * 56;
        image[row..row + 4].copy_from_slice(&kind.to_le_bytes());
        image[row + 8..row + 16].copy_from_slice(&0x100u64.to_le_bytes());
        image[row + 32..row + 40].copy_from_slice(&(interp.len() as u64).to_le_bytes());
    }
    image[0x100..0x100 + interp.len()].copy_from_slice(interp);
    image
}

#[test]
fn the_interpreter_comes_from_pt_interp() {
    let loader = b"/td/store/x/lib/ld-linux-x86-64.so.2\0";
    let image = elf_image(3, &[6, 3, 1], loader);
    let (kind, offset, entry, count) = header(&image).unwrap();
    assert_eq!((kind, offset, entry, count), (3, 64, 56, 3));
    assert_eq!(
        interp_segment(&image[64..64 + 3 * 56], 56),
        Some((0x100, loader.len() as u64))
    );
    assert_eq!(interp_segment(&image[64..64 + 2 * 56], 0), None);
    assert_eq!(
        interp_segment(&elf_image(3, &[1, 2], b"")[64..176], 56),
        None
    );
    let mut other = image.clone();
    other[18] = 183;
    assert!(header(&other).is_none(), "not x86-64");
    other = image.clone();
    other[4] = 1;
    assert!(header(&other).is_none(), "not ELF64");
    assert!(header(b"#!/bin/sh\n").is_none());

    let dir = td_fs::TempDir::new("td-mta-interp").unwrap();
    let program = dir.path().join("program");
    std::fs::write(&program, &image).unwrap();
    assert_eq!(
        interpreter(&program).unwrap().as_deref(),
        Some("/td/store/x/lib/ld-linux-x86-64.so.2")
    );
    std::fs::write(&program, elf_image(3, &[1], b"")).unwrap();
    assert_eq!(interpreter(&program).unwrap(), None);
    std::fs::write(&program, elf_image(3, &[3], b"no terminator")).unwrap();
    assert!(interpreter(&program).is_err());
    std::fs::write(&program, b"#!/bin/sh\n").unwrap();
    assert!(interpreter(&program).is_err());
}

#[test]
fn only_this_basenames_partial_trees_are_stale() {
    assert!(stale_partial(
        &format!(".{PG}.partial-0011aabbccddeeff"),
        PG
    ));
    for kept in [
        format!(".{PG}.partial-0011aabbccddeef"),
        format!(".{PG}.partial-0011AABBCCDDEEFF"),
        format!(".{PG}.partial-0011aabbccddeeff0"),
        format!("{PG}.partial-0011aabbccddeeff"),
        format!(".{GLIBC}.partial-0011aabbccddeeff"),
        format!(".{PG}x.partial-0011aabbccddeeff"),
        PG.to_owned(),
    ] {
        assert!(!stale_partial(&kept, PG), "{kept}");
    }
}

#[test]
fn store_trees_refuse_writable_set_id_and_special_entries() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let dir = td_fs::TempDir::new("td-mta-store-tree").unwrap();
    let meta = std::fs::metadata(dir.path()).unwrap();
    let owner = (meta.uid(), meta.gid());
    let other = (owner.0.wrapping_add(1), owner.1);
    let tree = dir.path().join("tree");
    let mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    std::fs::create_dir_all(tree.join("lib")).unwrap();
    mode(&tree, 0o755);
    mode(&tree.join("lib"), 0o755);
    let file = tree.join("lib/libpq.so.5.17");
    std::fs::write(&file, b"elf").unwrap();
    mode(&file, 0o755);
    symlink("libpq.so.5.17", tree.join("lib/libpq.so.5")).unwrap();
    check_tree(&tree, owner).unwrap();
    // Another owner, here of every entry including the link.
    assert!(check_tree(&tree, other).is_err());

    for (path, bad) in [
        (&file, 0o775),
        (&file, 0o757),
        (&file, 0o4755),
        (&file, 0o2755),
        (&tree.join("lib"), 0o775),
        (&tree.join("lib"), 0o1755),
    ] {
        mode(path, bad);
        assert!(
            check_tree(&tree, owner).is_err(),
            "{} {bad:o}",
            path.display()
        );
        mode(path, 0o755);
    }
    check_tree(&tree, owner).unwrap();

    let socket = tree.join("lib/socket");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    mode(&socket, 0o755);
    assert!(
        check_tree(&tree, owner).is_err(),
        "a socket is no store entry"
    );
    drop(listener);
    std::fs::remove_file(&socket).unwrap();
    check_tree(&tree, owner).unwrap();

    // A store path itself is a directory, never a link to one or a file.
    let link = dir.path().join("link");
    symlink(&tree, &link).unwrap();
    assert!(check_tree(&link, owner).is_err());
    assert!(check_tree(&file, owner).is_err());
}

#[test]
fn nar_hash_output_is_one_lowercase_sha256() {
    assert_eq!(nar_hash_line(&format!("sha256:{HASH}")).unwrap(), HASH);
    for refused in [
        HASH.to_owned(),
        format!("sha256:{}", HASH.to_uppercase()),
        format!("sha256:{HASH}0"),
        format!("md5:{HASH}"),
    ] {
        assert!(nar_hash_line(&refused).is_err(), "{refused}");
    }
}

/// A store that records each step and answers from fixed state.
struct Fake {
    exists: bool,
    tree_hash: &'static str,
    nar_digest: &'static str,
    fail: Option<&'static str>,
    steps: Vec<String>,
}

impl Fake {
    fn new() -> Self {
        Self {
            exists: false,
            tree_hash: HASH,
            nar_digest: HASH,
            fail: None,
            steps: Vec::new(),
        }
    }
    fn step(&mut self, name: &str, path: &Path) -> R<()> {
        self.steps.push(format!("{name} {}", path.display()));
        if self.fail == Some(name) {
            return Err("injected fault".into());
        }
        Ok(())
    }
}

impl Store for Fake {
    fn exists(&mut self, target: &Path) -> R<bool> {
        self.step("exists", target)?;
        Ok(self.exists)
    }
    fn nar_hash(&mut self, tree: &Path) -> R<String> {
        self.step("nar-hash", tree)?;
        Ok(self.tree_hash.into())
    }
    fn nar_digest(&mut self, basename: &str) -> R<String> {
        self.step("nar-digest", Path::new(basename))?;
        Ok(self.nar_digest.into())
    }
    fn restore(&mut self, basename: &str, partial: &Path) -> R<()> {
        self.step("restore", partial)?;
        self.step("from", Path::new(basename))
    }
    fn seal(&mut self, partial: &Path) -> R<()> {
        self.step("seal", partial)
    }
    fn publish(&mut self, partial: &Path, target: &Path) -> R<()> {
        self.step("publish", partial)?;
        self.step("to", target)
    }
    fn discard(&mut self, partial: &Path) -> R<()> {
        self.step("discard", partial)
    }
}

#[test]
fn store_entries_are_verified_before_and_after_unpacking_and_never_replaced() {
    let entry = Entry {
        basename: PG.into(),
        nar_sha256: HASH.into(),
    };
    let partial = format!("/td/store/.{PG}.partial-0011aabb");
    let partial = Path::new(&partial);
    let target = format!("/td/store/{PG}");

    let mut store = Fake::new();
    assert_eq!(
        place(&mut store, &entry, partial).unwrap(),
        Placed::Installed
    );
    assert_eq!(
        store.steps,
        [
            format!("exists {target}"),
            format!("nar-digest {PG}"),
            format!("restore {}", partial.display()),
            format!("from {PG}"),
            format!("seal {}", partial.display()),
            format!("nar-hash {}", partial.display()),
            format!("publish {}", partial.display()),
            format!("to {target}"),
        ]
    );

    // An existing path is only hashed: never unpacked over or replaced.
    let mut store = Fake {
        exists: true,
        ..Fake::new()
    };
    assert_eq!(place(&mut store, &entry, partial).unwrap(), Placed::Present);
    assert_eq!(
        store.steps,
        [format!("exists {target}"), format!("nar-hash {target}")]
    );
    let mut store = Fake {
        exists: true,
        tree_hash: "differs",
        ..Fake::new()
    };
    assert!(place(&mut store, &entry, partial).is_err());
    assert_eq!(store.steps.len(), 2);

    // A NAR that is not the one built is never unpacked.
    let mut store = Fake {
        nar_digest: "differs",
        ..Fake::new()
    };
    assert!(place(&mut store, &entry, partial).is_err());
    assert_eq!(store.steps.len(), 2);

    // Any later failure removes the partial tree and publishes nothing.
    for fail in ["restore", "seal", "nar-hash", "publish"] {
        let mut store = Fake {
            fail: Some(fail),
            ..Fake::new()
        };
        assert!(place(&mut store, &entry, partial).is_err(), "{fail}");
        assert_eq!(
            store.steps.last().unwrap(),
            &format!("discard {}", partial.display())
        );
        assert!(!store.steps.contains(&format!("to {target}")) || fail == "publish");
    }
    let mut store = Fake {
        tree_hash: "differs",
        ..Fake::new()
    };
    assert!(place(&mut store, &entry, partial).is_err());
    assert!(!store.steps.iter().any(|s| s.starts_with("publish")));
    assert!(store.steps.last().unwrap().starts_with("discard"));
}

#[test]
fn minor_release_notes_refuse_what_they_concern_and_size_the_window() {
    let (release, note) = postgres_minors::span(5, 6).unwrap().next().unwrap();
    concerned(release, note, "0").unwrap();
    let refused = concerned(release, note, "2").unwrap_err().to_string();
    assert!(
        refused.contains("17.6") && refused.contains(note.id),
        "{refused}"
    );
    assert!(concerned(release, note, "x").is_err());
    assert_eq!(last_count("\n3\n").unwrap(), 3);
    assert!(last_count("").is_err());
    assert_eq!(gin_window(0.2), (31, 60));
    assert_eq!(gin_window(20.0), (70, 120));
    assert_eq!(gin_window(1e9), (1230, 1560));
    let rows = postgres_minors::span(10, 12)
        .unwrap()
        .map(|(r, n)| note_record(r, n).unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(rows.iter().any(|r| r.contains("GIN rebuild rehearsed")));
    assert!(rows.iter().any(|r| r.contains("exempt: ")));
    assert!(rows
        .iter()
        .all(|r| r.contains(r#""release":"17.11""#) || r.contains(r#""release":"17.12""#)));
    // A td release the table does not list cannot be switched to.
    let unlisted = PG.replace("17.12", "17.13");
    let job = job(&entries(&unlisted, GLIBC), "17.6");
    assert!(Closure::from_job(&job).is_err());
}
