#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::path::Path;

#[test]
fn the_production_source_and_raw_boundary_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("autotests = false"));
    assert_eq!(manifest.matches("[[example]]").count(), 1);
    assert!(manifest
        .contains("name = \"terminal-launch-vm\"\npath = \"tests/launch_vm.rs\"\ntest = true"));
    for target in ["[[bin]]", "[[test]]", "[[bench]]", "[lib]"] {
        assert!(!manifest.contains(target));
    }
    let mut files: Vec<String> = std::fs::read_dir(root.join("src"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "nested source directory"
            );
            entry.file_name().into_string().unwrap()
        })
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "application.rs",
            "application_cache.rs",
            "backoff.rs",
            "channel.rs",
            "consent.rs",
            "deployment.rs",
            "disk_install.rs",
            "elevation.rs",
            "grant_requests.rs",
            "grant_views.rs",
            "handoff.rs",
            "inspection.rs",
            "launch.rs",
            "login.rs",
            "login_status.rs",
            "main.rs",
            "mount_sys.rs",
            "notebook.rs",
            "notebook_mail.rs",
            "notebook_token.rs",
            "portal_files.rs",
            "primary_account.rs",
            "proof.rs",
            "revocation.rs",
            "rollback.rs",
            "secret_intake.rs",
            "secret_request.rs",
            "secret_sys.rs",
            "session.rs",
            "set_hostname.rs",
            "software.rs",
            "stderr_tail.rs",
            "storage.rs",
            "suspend.rs",
            "sys.rs",
            "unlock.rs",
            "workspace.rs"
        ]
    );
    for (name, count) in [
        ("application.rs", 0),
        ("application_cache.rs", 0),
        ("grant_requests.rs", 0),
        ("grant_views.rs", 0),
        ("main.rs", 1),
        ("notebook.rs", 0),
        ("notebook_mail.rs", 0),
        ("notebook_token.rs", 0),
        ("channel.rs", 0),
        ("consent.rs", 0),
        ("deployment.rs", 0),
        ("disk_install.rs", 0),
        ("elevation.rs", 0),
        ("handoff.rs", 0),
        ("rollback.rs", 0),
        ("set_hostname.rs", 0),
        ("software.rs", 0),
        ("stderr_tail.rs", 0),
        ("backoff.rs", 0),
        ("sys.rs", 4),
        ("launch.rs", 0),
        ("unlock.rs", 0),
        ("login.rs", 0),
        ("login_status.rs", 0),
        ("session.rs", 0),
        ("secret_intake.rs", 0),
        ("secret_request.rs", 0),
        ("secret_sys.rs", 4),
        ("inspection.rs", 0),
        ("mount_sys.rs", 4),
        ("portal_files.rs", 0),
        ("primary_account.rs", 0),
        ("proof.rs", 0),
        ("revocation.rs", 0),
        ("storage.rs", 0),
        ("suspend.rs", 0),
        ("workspace.rs", 0),
    ] {
        let source = std::fs::read_to_string(root.join("src").join(name)).unwrap();
        assert_eq!(source.matches("unsafe").count(), count, "{name}");
        // The first sender is authoritative only when trusted startup has
        // not delegated its inherited endpoint before the greeting.
        for forbidden in [
            "::Command",
            "::thread",
            ".spawn(",
            ".exec(",
            "fork(",
            "cfg_attr",
            "/*",
            "include!",
            "include_str!",
            "include_bytes!",
            "println!",
            "eprintln!",
        ] {
            let child_api = matches!(
                name,
                "deployment.rs"
                    | "disk_install.rs"
                    | "launch.rs"
                    | "application.rs"
                    | "application_cache.rs"
                    | "software.rs"
                    | "grant_requests.rs"
                    | "unlock.rs"
                    | "login.rs"
                    | "storage.rs"
                    | "rollback.rs"
                    | "session.rs"
                    | "set_hostname.rs"
                    | "revocation.rs"
                    | "inspection.rs"
                    | "notebook.rs"
                    | "notebook_token.rs"
                    | "stderr_tail.rs"
                    | "workspace.rs"
            ) && ["::Command", "::thread", ".spawn(", ".exec("]
                .contains(&forbidden);
            let mapping_child_api =
                name == "portal_files.rs" && ["::Command", ".spawn("].contains(&forbidden);
            // The launch lock's bounded wait sleeps; pinned below.
            let lock_wait = name == "grant_views.rs" && forbidden == "::thread";
            if !child_api && !mapping_child_api && !lock_wait {
                assert!(!source.contains(forbidden), "{name}: {forbidden}");
            }
        }
    }
    // The shared tail launches one child, waits by sleeping, and hands it
    // nothing but the tail's own end as stderr.
    let tail = include_str!("../src/stderr_tail.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(tail), STDERR_TAIL_FINGERPRINT);
    assert_eq!(tail.matches(".spawn(").count(), 1);
    assert_eq!(tail.matches("::thread").count(), 1);
    assert_eq!(tail.matches("std::thread::sleep(WAIT_STEP)").count(), 1);
    assert_eq!(tail.matches(".stderr(").count(), 2);
    assert_eq!(
        tail.matches(".stderr(Stdio::from(child_end)).spawn()")
            .count(),
        1
    );
    assert_eq!(tail.matches("command.stderr(Stdio::null());").count(), 1);
    for forbidden in [".exec(", "inherit", "pre_exec", ".env(", "Command::new("] {
        assert!(!tail.contains(forbidden), "stderr_tail.rs: {forbidden}");
    }
    let time_setting = include_str!("../../td-time/src/request.rs");
    assert_eq!(time_setting.matches("unsafe").count(), 0);
    assert!(time_setting.contains("#[path = \"../../td-civil/src/tzif.rs\"]"));
    // The volume codec is std-only and declares no further shared source.
    let volume_setting = include_str!("../../td-disks/src/setting.rs");
    assert_eq!(volume_setting.matches("unsafe").count(), 0);
    assert!(!volume_setting.contains("#[path"));
    assert!(!volume_setting.contains("std::process"));
    assert!(!volume_setting.contains("std::fs"));
    assert_eq!(
        fingerprint(include_str!("../src/consent.rs")),
        0x15cb18f54f2439bd,
        "shared consent changed: reconcile td-secret/src/lib.rs, compositor confinement and this pin"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/unlock.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x605ce7ad6a78ead6,
        "private unlock supervisor changed"
    );
    let login = include_str!("../src/login.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    // Since activation (TOKEN-LOGIN.md increment 5's A5) every login
    // selection, a write included, starts the one fixed worker, whose
    // baseline decides; no build switch refuses one first. A login cancel
    // kills and reaps that worker and never relocks the secret session.
    assert!(!login.contains("WRITES"));
    assert!(!login.contains("cfg!(test)"));
    assert_eq!(
        login.matches("command(\"login-operation\", owner)").count(),
        1
    );
    for forbidden in ["lock-session", "Command::new(", "unlock-operation", "/bin/"] {
        assert!(!login.contains(forbidden), "login.rs: {forbidden}");
    }
    // A disclosure step (TOKEN-LOGIN.md increment 5's A4), which consent
    // alone decides, awaits the person's Enter: its receipt waits for
    // root's deadline. Only the nonce is drawn; no approval key exists.
    assert_eq!(login.matches("File::open(\"/dev/urandom\")").count(), 1);
    for gone in ["ApprovalKey", "approval_key", ".disclosed(", "% 8"] {
        assert!(!login.contains(gone), "login.rs: {gone}");
    }
    assert!(login
        .contains("            Some(request) if request.awaits_approval() => self.deadline,\n"));
    // Root's operation deadline is the description's ceiling, the worker's
    // and the compositor's, reading allowance included; the clock is read
    // once per receipt.
    assert_eq!(
        login.matches("self.deadline = self.deadline.min(").count(),
        1
    );
    // Five, and two in `exited`'s grace.
    assert_eq!(login.matches("Instant::now()").count(), 7);
    assert_eq!(
        fingerprint(login),
        0x66fba2fcb04b1842,
        "login worker supervisor changed"
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/session.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        SESSION_FINGERPRINT,
        "paired secret controller changed"
    );
    // `1b` has one answer that starts nothing, the generation's end: no
    // `9b 00` refusal remains.
    let session = include_str!("../src/session.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(!session.contains("WRITES"));
    assert!(!session.contains("0x9b, 0]"));
    // Generation cleanup's two seconds, from the constant alone.
    for pin in [
        "const CLEANUP_TIME: Duration = Duration::from_secs(2);",
        "cleanup_time: CLEANUP_TIME,",
    ] {
        assert_eq!(session.matches(pin).count(), 1, "session.rs: {pin}");
    }
    assert_eq!(
        session.matches("self.cleanup_time").count(),
        2,
        "session.rs: cleanup_time's uses"
    );
    // Amendment 10: no storage worker starts in production until
    // td-install/ENCRYPTION.md's item 9, and the one worker is td-install's
    // fixed verb.
    let storage = include_str!("../src/storage.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        storage
            .matches("pub(crate) const OPERATIONS: bool = cfg!(test);")
            .count(),
        1
    );
    assert_eq!(storage.matches("if !OPERATIONS ||").count(), 1);
    assert_eq!(storage.matches("Command::new(").count(), 1);
    assert!(storage.contains("Command::new(\"/bin/td-install\")"));
    assert!(storage.contains(".args([\"storage-operation\", \"--uid\", &owner.to_string()])"));
    // An upgrade reads the login state itself, from `/`, and only `start`,
    // behind `OPERATIONS`, launches a selection's worker.
    assert_eq!(
        storage
            .matches("login_state::state(Path::new(\"/\"), owner) == login_state::State::Enrolled")
            .count(),
        1
    );
    assert_eq!(storage.matches("Self::launch(").count(), 1);
    assert!(storage.contains("Self::launch(owner, selection, enrolled, command(owner))"));
    assert!(!session.contains("::launch("));
    assert_eq!(
        fingerprint(storage),
        0x6efc37146ff88255,
        "storage worker supervisor changed"
    );
    assert_eq!(
        session
            .matches("Request::Login(selection) => self.begin_login(selection, Login::start),")
            .count(),
        1
    );
    // Amendment 10's disk proof: only a build whose `storage::OPERATIONS`
    // is set decodes `1b 0b`, so until td-install/ENCRYPTION.md's item 9
    // a production build reads one as a protocol violation, and the
    // storage worker it starts is the same fixed verb under the same
    // gate. Its two workers start only through `proof::Proving`.
    assert_eq!(
        login
            .matches("[11, proof, rest @ ..] if crate::storage::OPERATIONS =>")
            .count(),
        1
    );
    assert_eq!(login.matches("[11").count(), 1);
    assert_eq!(storage.matches("if !OPERATIONS {").count(), 1);
    assert_eq!(
        storage
            .matches("Self::spawn_proof(owner, nonce, first, login, deadline, command(owner))")
            .count(),
        1
    );
    assert_eq!(
        session
            .matches("self.begin_proof(selection, Proving::start)")
            .count(),
        1
    );
    let proof = include_str!("../src/proof.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        proof
            .matches("Login::begin_with(owner, nonce, selection, true)")
            .count(),
        1
    );
    assert_eq!(
        proof
            .matches(".map(|proving| proving.storage_with(Box::new(Storage::start_proof)))")
            .count(),
        1
    );
    assert_eq!(
        proof
            .matches("login::protected(Path::new(\"/\"), Owner::ROOT)")
            .count(),
        1
    );
    assert_eq!(
        fingerprint(proof),
        0x490367828e8f787d,
        "disk proof orchestrator changed"
    );
    // Amendment 9: only an installed boot's Prepare consumes the admission
    // record, with the production paths, and only the generation's first
    // `1a` carries what it found.
    let session = include_str!("../src/session.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    let installed = session
        .split_once("            if crate::disk_install::live_boot()? {\n")
        .unwrap()
        .1
        .split_once("        }\n        let answer = self.answer_with(")
        .unwrap()
        .0
        .split_once("            } else {\n")
        .unwrap()
        .1;
    assert!(installed.ends_with(
        "                self.admit(&crate::handoff::Paths::production());\n            }\n"
    ));
    assert_eq!(session.matches("Paths::production()").count(), 1);
    assert_eq!(session.matches("crate::handoff::consume(").count(), 1);
    assert_eq!(session.matches("self.admit(").count(), 1);
    assert_eq!(session.matches("self.admitted = ").count(), 1);
    assert_eq!(
        session
            .matches("let admitted = std::mem::take(&mut self.admitted);")
            .count(),
        1
    );
    assert_eq!(session.matches("admitted").count(), 9);
    assert_eq!(
        fingerprint(
            include_str!("../src/inspection.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        0x2c7e51db738098fa,
        "read-only store controller changed"
    );
    let inspection = include_str!("../src/inspection.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    // Two fixed read-only helpers, one launch each.
    assert_eq!(inspection.matches("Command::new(").count(), 2);
    assert_eq!(
        inspection
            .matches("Command::new(\"/bin/td-secret\")")
            .count(),
        2
    );
    assert_eq!(inspection.matches("\"inspect-store\"").count(), 1);
    assert_eq!(inspection.matches("\"inspect-login\"").count(), 1);
    // Request 1a: the shared predicate first, the helper only through the
    // inspection launch, and nothing it reads names a path from the peer.
    let login_state = include_str!("../src/login_status.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(login_state), LOGIN_STATE_FINGERPRINT);
    assert!(login_state.contains("#[path = \"../../td-secret/src/login_state.rs\"]"));
    assert_eq!(login_state.matches("#[path").count(), 1);
    // One copy of the hostname rules, which consent's hostname descriptions
    // share.
    assert!(login_state.contains("\nuse crate::hostname;\n"));
    assert!(include_str!("../src/main.rs")
        .contains("#[path = \"../../td-firstboot/src/hostname.rs\"]\nmod hostname;\n"));
    assert!(include_str!("../src/consent.rs").contains("\nuse super::hostname::Hostname;\n"));
    assert_eq!(login_state.matches("Inspection::login").count(), 1);
    for forbidden in ["Command", "spawn", "/bin/", "write", "remove", "create"] {
        assert!(
            !login_state.contains(forbidden),
            "login_status.rs: {forbidden}"
        );
    }
    assert_eq!(
        fingerprint(include_str!("../../td-secret/src/login_state.rs")),
        SHARED_LOGIN_STATE_FINGERPRINT,
        "shared login-state predicate changed: reconcile td-firstboot and this pin"
    );
    assert_eq!(
        fingerprint(include_str!("../../td-firstboot/src/hostname.rs")),
        SHARED_HOSTNAME_FINGERPRINT,
        "shared hostname rules, which also serve consent's hostname descriptions, changed: reconcile td-compositor's and td-secret's confinement and this pin"
    );
    let installation = include_str!("../src/deployment.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(installation), INSTALLATION_FINGERPRINT);
    // Request 19's tier marker (amendment 8): one reviewed shared reader,
    // compiled once at the crate root, reading only the queued deployment's
    // held directory.
    assert_eq!(installation.matches("#[path").count(), 0);
    assert!(installation.contains("\nuse crate::login_tier;\n"));
    assert_eq!(
        include_str!("../src/main.rs")
            .matches("#[path = \"../../td-secret/src/login_tier.rs\"]\n")
            .count(),
        1
    );
    assert_eq!(installation.matches("login_tier::").count(), 1);
    assert!(installation
        .contains("login_tier::read(&self.source, &self.deployment, self.owner, self.give_up)"));
    // The test seam: production sets the budget only from the constant.
    assert_eq!(installation.matches("give_up").count(), 5);
    assert_eq!(installation.matches("give_up: MARKER_GIVE_UP,").count(), 1);
    assert_eq!(installation.matches("give_up: ready.give_up,").count(), 1);
    // Admission's five seconds, which the fixtures lengthen: DESIGN.md's
    // "Admission expires in five seconds".
    assert_eq!(
        installation.matches("deadline: Some(expires(5)?),").count(),
        1
    );
    assert_eq!(
        installation
            .matches("const MARKER_GIVE_UP: Duration = Duration::from_secs(2);")
            .count(),
        1
    );
    assert_eq!(
        fingerprint(
            include_str!("../../td-secret/src/login_tier.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        SHARED_LOGIN_TIER_FINGERPRINT,
        "shared tier reader changed: reconcile td-secret's confinement and this pin"
    );
    let session = include_str!("../src/session.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        session
            .matches("self.login_state.admits(|| ready.reads())")
            .count(),
        1
    );
    assert_eq!(session.matches("vec![0x99, 1]").count(), 1);
    assert_eq!(installation.matches("Command::new(").count(), 1);
    assert!(installation.contains("Command::new(\"/bin/td-update\")"));
    assert!(installation.contains(".args([\"apply-operation\", deployment])"));
    assert!(installation.contains("sender.descriptor.is_some()"));
    for forbidden in [
        "send_descriptor(",
        "create_credential(",
        "seal_credential(",
        "pre_exec",
        "CommandExt",
        "setsid",
        "process_group",
    ] {
        assert!(
            !installation.contains(forbidden),
            "installation: {forbidden}"
        );
    }
    // Request 19's `deploy-publish` (td-authd/DESIGN.md, "Elevation
    // operations"): the update queue's own backoff row and the production
    // table, the table's grant checked and the backoff written before the
    // admission byte as admission's last steps, a refusal's byte sent
    // before the connection closes; only a nonce drawn; and at
    // commit the confirmation consumed, then the backoff cleared, before
    // the one helper starts.
    for pin in [
        "            backoff: Backoff::system(Row::Update),\n            table: Table::load,",
        "            .and_then(|mut random| random.read_exact(&mut nonce))",
        "                    Err(refused) => {\n                        let _ = self.stream.write(&[refused.byte()]);\n                        return Err(error(refused));\n                    }",
        "        self.committed = true;\n        // The approval clears the queue's backoff; one root cannot clear\n        // fails the installation before anything starts, said on the\n        // authority's standard error as admission's refusal is.\n        if let Err(why) = self.backoff.approved() {\n            let _ = writeln!(\n                io::stderr(),\n                \"td-authd: update installation failed: clear the backoff: {why}\"\n            );\n            self.result = Some(false);\n            return Ok(());\n        }\n        let child = spawn(source, deployment);",
    ] {
        assert!(installation.contains(pin), "deployment.rs: {pin}");
    }
    let admit = installation.split("\nfn admit(").nth(1).unwrap();
    let admit = admit.split("\n}\n").next().unwrap();
    for (name, source) in [
        ("deployment.rs", installation),
        ("rollback.rs", include_str!("../src/rollback.rs")),
        ("set_hostname.rs", include_str!("../src/set_hostname.rs")),
        ("storage.rs", include_str!("../src/storage.rs")),
    ] {
        for gone in ["ApprovalKey", "approval_key", "% 8"] {
            assert!(!source.contains(gone), "{name}: {gone}");
        }
    }
    assert!(admit.ends_with(
        "    if !table()\n        .map_err(Refused::Other)?\n        .grants(owner, DeployPublish)\n    {\n        return Err(Refused::Other(\n            \"the principal table does not grant the requester\".into(),\n        ));\n    }\n    backoff.admitted(entry, now).map_err(unusable)?;\n    Ok(ready)"
    ));
    assert!(
        admit.find("if entry.refuses(now) {").unwrap()
            < admit.find("Ready::capture(body, owner)").unwrap()
    );
    assert_eq!(installation.matches("backoff.admitted(").count(), 1);
    assert_eq!(installation.matches(".approved()").count(), 1);
    assert_eq!(installation.matches("Ready::capture(").count(), 1);
    assert_eq!(installation.matches("table: Table::load,").count(), 1);
    assert_eq!(installation.matches(".grants(").count(), 1);
    assert!(session.contains(
        "            Request::Install => self.begin_install(crate::elevation::Table::load),\n"
    ));
    // The table before the queue, the queue before the record.
    let begin = session.split("    fn begin_install(").nth(1).unwrap();
    let begin = begin.split("\n    }\n").next().unwrap();
    assert!(
        begin.find("if self.setup.is_some() {").unwrap()
            < begin.find("if !table().is_ok_and(").unwrap()
    );
    assert!(
        begin
            .find("table.grants(self.owner, DeployPublish)")
            .unwrap()
            < begin.find("intake.select()").unwrap()
    );
    assert_eq!(session.matches("vec![0x99, 2]").count(), 1);
    // Request 1d (td-authd/DESIGN.md, "Elevation operations"): the table
    // before the selectors, both before a description; the selectors only
    // through the shared reader's held volume, read again before acting;
    // and one fixed helper on exactly the approved pair.
    let rollback = include_str!("../src/rollback.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(rollback), ROLLBACK_FINGERPRINT);
    assert_eq!(rollback.matches("Command::new(").count(), 1);
    assert!(rollback.contains(
        "            Command::new(\"/bin/td-boot\")\n                .args([\"on-volume\", \"rollback\", \"/run/td-update\", current, previous])\n                .env_clear()\n                .current_dir(\"/\")\n                .stdin(Stdio::null())\n                .stdout(Stdio::null())\n                .stderr(Stdio::inherit())\n                .spawn()\n"
    ));
    assert_eq!(rollback.matches(".spawn()").count(), 1);
    assert_eq!(rollback.matches("login_tier::open_volume(").count(), 1);
    assert_eq!(rollback.matches("login_tier::selected(").count(), 2);
    assert_eq!(rollback.matches("login_tier::").count(), 5);
    assert!(rollback
        .contains("        self.committed = true;\n        if !self.selectors.unchanged() {\n"));
    assert!(rollback.contains(
        "    if !table.is_ok_and(|table| table.grants(owner, elevation::Operation::DeployRollback)) {\n        return Err(Refusal::Principal);\n    }\n    Selectors::read(volume).map_err(|_| Refusal::Selectors)\n"
    ));
    assert!(rollback.contains("            .and_then(|mut random| random.read_exact(&mut nonce))"));
    for forbidden in [
        "send_descriptor(",
        "sys::",
        "pre_exec",
        "CommandExt",
        "setsid",
        "process_group",
        ".arg(",
        ".env(",
        "fs::write",
        "remove",
        "rename",
    ] {
        assert!(!rollback.contains(forbidden), "rollback.rs: {forbidden}");
    }
    assert_eq!(
        session
            .matches("crate::rollback::admit(self.owner, table(), volume)")
            .count(),
        1
    );
    assert!(session.contains(
        "            Request::Rollback => self.begin_rollback(\n                crate::elevation::Table::load,\n                Path::new(crate::login_tier::VOLUME),\n            ),\n"
    ));
    // Request 1e (td-authd/DESIGN.md, "Elevation operations"): the public
    // intake on the update intake's binder and sender rules, refusing every
    // descriptor; the backoff written before admission is sent, a failed
    // write refusing until one succeeds; the table for the owner before
    // the queue; and at commit, the confirmation consumed, the backoff
    // cleared, the saved name read again and written through
    // td-firstboot's shared source. Only time, volume and initial admin-key
    // enrollment start their fixed helpers.
    let rename = include_str!("../src/set_hostname.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(rename), SET_HOSTNAME_FINGERPRINT);
    for pin in [
        "deadline: Some(expires(5)?),",
        "const SOCKET: &str = \"/run/td-authd/1000/hostname\";",
        "const GREETING: &[u8; 8] = b\"TDHST01\\n\";",
        "const SAVED: &str = \"/var/lib/td/hostname\";",
        "let (listener, identity) = bind_intake(SOCKET, owner)?;",
        "if sender.credentials.uid != self.owner || sender.descriptor.is_some() {",
        "if sys::peer_uid(&stream)? != owner {",
        "        if owner != 1000 {\n            return Err(\"unsupported hostname requester\".into());",
        "            table: Table::load,",
        "            backoff: Backoff::system(Row::Hostname),",
        "        self.committed = true;\n        self.result = Some(save(&self.saved, self.owner, &self.backoff, old, new).is_ok());",
        "    backoff.approved()?;\n    let saved = saved::read_hostname(path, owner)?",
        "    saved::write_hostname(path, &Hostname::parse(new)?)",
        "            .and_then(|mut random| random.read_exact(&mut nonce))",
    ] {
        assert!(rename.contains(pin), "set_hostname.rs: {pin}");
    }
    // Admission only after the backoff counts the request: the write is
    // admission's last step, and the admission byte follows its success.
    let admit = rename.split("\nfn admit(").nth(1).unwrap();
    let admit = admit.split("\n}\n").next().unwrap();
    assert!(
        admit.ends_with("    status.written(places.backoff.admitted(entry, now))?;\n    Ok(())")
    );
    assert!(
        admit
            .find("status.written(places.backoff.clamp(entry, now))?")
            .unwrap()
            < admit.find("if entry.refuses(now) {").unwrap()
    );
    assert!(rename.contains(
        "                if let Err(refused) = admit(&name) {\n                    let _ = self.stream.write(&[refused.byte()]);\n                    return Err(error(refused));\n                }\n                self.name = Some(name);"
    ));
    assert_eq!(rename.matches("places.backoff.admitted(").count(), 1);
    assert_eq!(rename.matches("self.unwritten = None;").count(), 1);
    // A failed write is sticky: `1f` answers 02 while one stands.
    assert!(rename.contains("            _ => vec![0x9f, 2, 0],"));
    assert_eq!(rename.matches("Command::new(").count(), 4);
    assert!(rename.contains("command.args([\"accept-publisher\", fingerprint]);"));
    assert!(rename.contains("Command::new(\"/bin/td-admin\")"));
    assert!(rename.contains("command.args([\"accept-key\", blob]);"));
    assert!(rename.contains("Command::new(\"/bin/td-time\")"));
    assert!(rename.contains(
        "            let mut command = Command::new(\"/bin/td-disks\");\n            command.arg(\"apply\").args(change.words());"
    ));
    for forbidden in [
        ".spawn(",
        "send_descriptor(",
        "create_credential(",
        "seal_credential(",
        "pre_exec",
        "fs::write",
        "remove",
        "rename(",
        "set_permissions",
        "chown",
        "sethostname",
        "/proc/sys/kernel",
    ] {
        assert!(!rename.contains(forbidden), "set_hostname.rs: {forbidden}");
    }
    assert!(session.contains(
        "            Request::Hostname => self.begin_hostname(crate::elevation::Table::load),\n"
    ));
    let begin = session.split("    fn begin_hostname(").nth(1).unwrap();
    let begin = begin.split("\n    }\n").next().unwrap();
    assert!(
        begin.find("let Ok(table) = table() else {").unwrap()
            < begin.find("intake.select()").unwrap()
    );
    assert!(
        begin
            .find("table.grants(ready.requester(), ready.operation())")
            .unwrap()
            < begin.find("intake.describe(").unwrap()
    );
    let backoff = include_str!("../src/backoff.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(backoff), BACKOFF_FINGERPRINT);
    for pin in [
        "const DIRECTORY: &str = \"/var/lib/td/authd\";",
        "const FILE: &str = \"backoff\";",
        "const NOFOLLOW: i32 = 0x20000;",
        "const NONBLOCK: i32 = 0x800;",
        "const DIRECTORY_ONLY: i32 = 0x10000;",
        "Self::at(Path::new(DIRECTORY), (0, 0), row)",
        "|| metadata.mode() & 0o7777 != 0o600",
        "|| metadata.mode() & 0o7777 != 0o700",
        "saved::write_synced(&self.directory.join(FILE), &encode(rows), 0o600, None)?;",
        // The directory, when missing, beneath an admitted parent: std's
        // DirBuilder, mode 0700, then permissions set again by path.
        "        std::fs::DirBuilder::new()\n            .mode(0o700)\n            .create(&self.directory)",
        "        std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))",
    ] {
        assert!(backoff.contains(pin), "backoff.rs: {pin}");
    }
    assert_eq!(backoff.matches("DirBuilder").count(), 2);
    assert_eq!(backoff.matches("set_permissions(").count(), 1);
    assert_eq!(backoff.matches("write_synced(").count(), 1);
    for forbidden in [
        "Command",
        "spawn",
        "fs::write",
        "remove",
        "rename(",
        "chown",
    ] {
        assert!(!backoff.contains(forbidden), "backoff.rs: {forbidden}");
    }
    // td-firstboot's source for the saved name and the synced write, one
    // reviewed copy compiled at the crate root.
    assert_eq!(
        include_str!("../src/main.rs")
            .matches("#[path = \"../../td-firstboot/src/saved.rs\"]\nmod saved;\n")
            .count(),
        1
    );
    let shared = include_str!("../../td-firstboot/src/saved.rs");
    assert_eq!(
        fingerprint(shared),
        SHARED_SAVED_FINGERPRINT,
        "shared saved-name source changed: reconcile td-firstboot and this pin"
    );
    for forbidden in [
        "unsafe",
        "Command",
        "spawn",
        "#[path",
        "include",
        "remove_dir",
    ] {
        assert!(!shared.contains(forbidden), "saved.rs: {forbidden}");
    }
    // The application policy, the workload account ledger reader it joins,
    // the grant table's format and the mountinfo derivation, each one
    // reviewed copy compiled at the crate root: they read two root-owned
    // files, parse text and write, run and include nothing.
    let main = include_str!("../src/main.rs");
    for (attribute, module) in [
        (
            "#[path = \"../../td-busd/src/app_policy.rs\"]\n#[allow(dead_code, reason = \"td-authd resolves one application's account\")]\nmod app_policy;\n",
            "mod app_policy;",
        ),
        (
            "#[path = \"../../engine/src/workload_accounts.rs\"]\n#[allow(dead_code, reason = \"the application policy reads the ledger\")]\nmod workload_accounts;\n",
            "mod workload_accounts;",
        ),
        (
            "#[path = \"../../engine/src/workload_grants.rs\"]\n#[allow(dead_code, reason = \"firstboot seeds; td-jail reads manifests\")]\nmod workload_grants;\n",
            "mod workload_grants;",
        ),
        (
            "#[path = \"../../engine/src/mount_identity.rs\"]\n#[allow(dead_code, reason = \"td-jail derives more identities than views\")]\nmod mount_identity;\n",
            "mod mount_identity;",
        ),
    ] {
        assert_eq!(main.matches(attribute).count(), 1, "{module}");
        assert_eq!(main.matches(module).count(), 1, "{module}");
    }
    for (name, shared) in [
        (
            "app_policy.rs",
            include_str!("../../td-busd/src/app_policy.rs"),
        ),
        (
            "workload_accounts.rs",
            include_str!("../../engine/src/workload_accounts.rs"),
        ),
        (
            "workload_grants.rs",
            include_str!("../../engine/src/workload_grants.rs"),
        ),
        (
            "mount_identity.rs",
            include_str!("../../engine/src/mount_identity.rs"),
        ),
    ] {
        // A workspace plan's `write` role names td-jail's spec key, a word
        // and no file operation: its one literal is taken out first.
        let production: String = shared
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .replacen("Self::Write => \"write\",", "", 1)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        assert_eq!(shared.matches("unsafe").count(), 0, "{name}");
        for forbidden in [
            "Command",
            "spawn",
            "std::process",
            "std::net",
            "std::env",
            "#[path",
            "include!",
            "include_str!",
            "include_bytes!",
            "write",
            "remove",
            "create",
            "rename",
            "set_permissions",
            "chown",
        ] {
            assert!(!production.contains(forbidden), "{name}: {forbidden}");
        }
    }
    // Amendment 7: two fixed root helpers, td-svc's control socket, the
    // line's hand-back and the record through firstboot's one shared
    // publication; production's deadlines only from the constants.
    let revocation = include_str!("../src/revocation.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(revocation), REVOCATION_FINGERPRINT);
    assert_eq!(revocation.matches("Command::new(").count(), 3);
    for pin in [
        "let mut command = Command::new(\"/bin/td-firstboot\");\n                command.arg(\"render-ssh-policy\");\n                Inspection::printing(command, RENDER_OUTPUT, RENDER_TIME)\n",
        "let mut command = Command::new(\"/bin/td-svc\");\n                command.arg(\"reboot\");\n                Inspection::printing(command, REPLY_OUTPUT, EXCHANGE_TIME)\n",
        // Each control exchange: a td-svc client child naming a fixed verb
        // and one of the fixed units.
        "control: Box::new(|verb, unit| {\n                let mut command = Command::new(\"/bin/td-svc\");\n                command.args([verb.word(), unit]);\n                Inspection::printing(command, REPLY_OUTPUT, EXCHANGE_TIME)\n",
        "control: Box<dyn Fn(Verb, &'static str) -> Result<Inspection, String>>,",
        "            Self::Status => \"status\",\n            Self::Restart => \"restart\",\n",
        "const REPLY_OUTPUT: usize = 256;",
        "const POLL_STEP: Duration = Duration::from_millis(250);",
        "const TEARDOWN_REAP: Duration = Duration::from_secs(1);",
        "const RENDER_OUTPUT: usize = 16;",
        "const LINE: &str = \"/dev/ttyS0\";",
        "const GUARD: &str = \"/var/lib/td/login/cutover-reboot\";",
        "const UNITS: &[&str] = &[\"sshd\", \"greeter\"];",
        "const RENDER_TIME: Duration = Duration::from_secs(10);",
        "const EXCHANGE_TIME: Duration = Duration::from_secs(12);",
        "const UNIT_TIME: Duration = Duration::from_secs(30);",
        "unit_time: UNIT_TIME,",
        "teardown_reap: TEARDOWN_REAP,",
        "record: Path::new(\"/\").join(cutover::RECORD),",
        "uid: 0,\n            gid: 0,",
    ] {
        assert_eq!(revocation.matches(pin).count(), 1, "revocation.rs: {pin}");
    }
    for (needle, count) in [
        ("Self {\n            record: ", 1),
        ("Self::with_host(", 1),
        ("cutover::publish(", 1),
        ("cutover::remove(", 1),
        ("Directory::open(", 1),
        ("(self.host.control)", 0),
        ("(host.control)(verb, name)", 1),
        ("(self.host.reboot)()", 2),
        ("(host.reboot)()", 1),
        ("lchown(", 1),
        ("set_permissions(", 2),
        ("remove_file(", 1),
        ("Inspection::printing(", 3),
        ("create_new(true)", 1),
    ] {
        assert_eq!(
            revocation.matches(needle).count(),
            count,
            "revocation.rs: {needle}"
        );
    }
    for forbidden in [
        "unsafe",
        ".spawn(",
        "#[path",
        "include",
        "remove_dir",
        "rename(",
        "UnixStream",
    ] {
        assert!(
            !revocation.contains(forbidden),
            "revocation.rs: {forbidden}"
        );
    }
    let session = include_str!("../src/session.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        session
            .matches("revocation: Some(crate::revocation::Revocation::new()),")
            .count(),
        1
    );
    assert_eq!(session.matches("self.revocation = None;").count(), 1);
    // The shared record bytes and publication, one reviewed copy.
    assert_eq!(
        include_str!("../src/main.rs")
            .matches("#[path = \"../../td-firstboot/src/cutover.rs\"]\nmod cutover;\n")
            .count(),
        1
    );
    let cutover = include_str!("../../td-firstboot/src/cutover.rs");
    assert_eq!(
        fingerprint(cutover),
        SHARED_CUTOVER_FINGERPRINT,
        "shared cutover record and publication changed: reconcile td-firstboot and this pin"
    );
    for forbidden in [
        "unsafe",
        "Command",
        "spawn",
        "#[path",
        "include",
        "remove_dir",
    ] {
        assert!(!cutover.contains(forbidden), "cutover.rs: {forbidden}");
    }
    let table = include_str!("../src/elevation.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(table), ELEVATION_FINGERPRINT);
    for pin in [
        "const DIRECTORY: &str = \"/etc\";",
        "const TABLE: &str = \"td-elevation.tsv\";",
        "const NOFOLLOW: i32 = 0x20000;",
        "const NONBLOCK: i32 = 0x800;",
        "const DIRECTORY_ONLY: i32 = 0x10000;",
        "Self::load_from(Path::new(DIRECTORY), (0, 0))",
        "|| metadata.mode() & 0o7777 != 0o444",
    ] {
        assert!(table.contains(pin), "elevation.rs: {pin}");
    }
    for forbidden in [
        "Command",
        "spawn",
        "write",
        "remove",
        "create",
        "rename",
        "chmod",
        "set_permissions",
    ] {
        assert!(!table.contains(forbidden), "elevation.rs: {forbidden}");
    }
    // The live installer's service: one fixed program and operands, the
    // installer's socket and td-authd's channel as its only descriptors.
    let disk = include_str!("../src/disk_install.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(fingerprint(disk), DISK_INSTALL_FINGERPRINT);
    assert_eq!(disk.matches("Command::new(").count(), 1);
    assert!(disk.contains("Command::new(\"/bin/td-install\")"));
    assert!(disk.contains(
        "\"serve\",\n                \"/bin/td-boot\",\n                \"/run/td-media\",\n                \"/\",\n                \"/bin/td-firstboot\","
    ));
    assert!(disk.contains("sys::peer_uid(peer).is_ok_and(|uid| uid == self.installer)"));
    assert!(disk.contains("if self.admits(&installer) {"));
    assert!(disk.contains("bind_intake(SOCKET, INSTALLER_UID)"));
    assert_eq!(disk.matches("self.installer").count(), 1);
    for forbidden in [
        "send_descriptor(",
        "sys::receive(",
        "create_credential(",
        "seal_credential(",
        "pre_exec",
        "CommandExt",
        "setsid",
        "process_group",
    ] {
        assert!(!disk.contains(forbidden), "disk installation: {forbidden}");
    }
    // The shared codec is data only, pinned as reviewed.
    let codec = include_str!("../../td-install/src/installation_consent.rs");
    assert_eq!(fingerprint(codec), CONSENT_CODEC_FINGERPRINT);
    for forbidden in [
        "unsafe",
        "std::fs",
        "std::io",
        "std::process",
        "std::net",
        "std::os",
        "std::thread",
        "std::env",
        "extern",
        "asm!",
        "include",
        "#[path",
        "print",
    ] {
        assert!(!codec.contains(forbidden), "consent codec: {forbidden}");
    }
    let intake_raw = include_str!("../src/secret_sys.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(intake_raw.matches("#[allow(unsafe_code)]").count(), 2);
    assert_eq!(intake_raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(intake_raw.matches("OwnedFd::from_raw_fd").count(), 1);
    assert_eq!(intake_raw.matches("const SYS_").count(), 7);
    for constant in [
        "const SYS_POLL: usize = 7;",
        "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_SETSOCKOPT: usize = 54;",
        "const SYS_GETSOCKOPT: usize = 55;",
        "const SYS_FCNTL: usize = 72;",
        "const SYS_MEMFD_CREATE: usize = 319;",
        "const F_ADD_SEALS: usize = 1033;",
        "const F_GET_SEALS: usize = 1034;",
        "const REQUIRED_SEALS: usize = 15;",
        "const MEMFD_FLAGS: usize = 3;",
        "const MSG_NOSIGNAL: usize = 0x4000;",
        "const SO_PASSCRED: usize = 16;",
        "const SO_PASSPIDFD: usize = 76;",
        "const SCM_RIGHTS: i32 = 1;",
        "const SCM_CREDENTIALS: i32 = 2;",
        "const SCM_PIDFD: i32 = 4;",
        "const CONTROL: usize = 128;",
        "const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;",
    ] {
        assert!(intake_raw.contains(constant), "{constant}");
    }
    assert_eq!(fingerprint(intake_raw), INTAKE_RAW_FINGERPRINT);
    assert_eq!(
        fingerprint(
            include_str!("../src/secret_intake.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        INTAKE_FINGERPRINT
    );
    assert_eq!(
        fingerprint(
            include_str!("../src/secret_request.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap()
        ),
        WRITE_REQUEST_FINGERPRINT
    );
    let application = include_str!("../src/application.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(application),
        APPLICATION_FINGERPRINT,
        "application launch controller changed"
    );
    for forbidden in [
        "thread::spawn",
        "thread::Builder",
        "thread::scope",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
    ] {
        assert!(
            !application.contains(forbidden),
            "application.rs: {forbidden}"
        );
    }
    assert_eq!(application.matches(".spawn(").count(), 1);
    // The one td-firstboot child runs one of six fixed verbs: the launch
    // admission, an install's allocation, a sandbox's or a workspace
    // slot's allocation when td-sandbox or td-agent creates it, and a
    // sandbox launch's, a slot's first instance's or either's deletion's
    // preparation of its existing row, the kind its one fixed word.
    assert!(application.contains(
        "            .arg(check.verb)\n            .arg(owner.to_string())\n            .args(check.words)\n            .arg(name)\n"
    ));
    assert_eq!(application.matches("    verb: \"").count(), 6);
    assert!(application.contains("    verb: \"check-launch-application\",\n    words: &[],\n"));
    assert!(application.contains("    verb: \"allocate-application\",\n    words: &[],\n"));
    for kind in ["sandbox", "workspace"] {
        for verb in ["prepare-workload", "allocate-workload"] {
            assert!(application.contains(&format!(
                "    verb: \"{verb}\",\n    words: &[\"{kind}\"],\n"
            )));
        }
    }
    assert_eq!(application.matches("allocate-workload").count(), 2);
    assert_eq!(application.matches("admit(&ALLOCATE, ").count(), 1);
    assert_eq!(application.matches(".exec()").count(), 2);
    assert_eq!(application.matches(".stdin(Stdio::null())").count(), 2);
    // The admission check's stderr is root's private tail, quoted in its refusal.
    assert_eq!(application.matches(".stderr(Stdio::null())").count(), 1);
    assert_eq!(
        application
            .matches(".stderr(Stdio::from(said_end))")
            .count(),
        1
    );
    assert_eq!(application.matches(".stdout(Stdio::null())").count(), 1);
    // The cache operation runs one fixed program, the credential drop into
    // the admitted account, and td-app's verb and the name after it.
    let cache = include_str!("../src/application_cache.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(cache),
        CACHE_FINGERPRINT,
        "application cache operation changed"
    );
    for forbidden in [
        "thread::spawn",
        "thread::Builder",
        "thread::scope",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
        ".exec()",
        ".env(",
        ".envs(",
        ".env_remove(",
        ".arg(",
    ] {
        assert!(
            !cache.contains(forbidden),
            "application_cache.rs: {forbidden}"
        );
    }
    assert_eq!(cache.matches("Command::new(").count(), 1);
    assert!(cache.contains(
        "Command::new(\"/bin/td-login\");\n    command\n        .args([\"exec-service-as\", account, \"--\", \"/bin/td-app\"])\n        .args(arguments);"
    ));
    // The only two accounts td-app runs as: an application's, by the UID
    // its admission or an install's allocation selected, and the compiled
    // runtime account.
    assert_eq!(cache.matches("\n    td_app(").count(), 2);
    assert!(cache.contains("    td_app(&format!(\"tda{uid}\"), &arguments)\n"));
    assert!(cache.contains("    td_app(RUNTIME_ACCOUNT, &arguments)\n"));
    assert!(cache.contains("let mut arguments = vec![word, name];"));
    assert!(cache.contains("let mut arguments = vec![\"runtime\", word, runtime.name];"));
    assert_eq!(cache.matches(".spawn(").count(), 1);
    assert_eq!(cache.matches(".env_clear()").count(), 1);
    assert_eq!(cache.matches(".stdin(Stdio::null())").count(), 1);
    // The spawn's command is dropped before its output is read, so EOF
    // comes when td-app exits.
    assert!(cache.contains("    drop(command);\n    let limit"));
    // Each of the two operations audits its startup once.
    assert_eq!(
        cache
            .matches("crate::launch::require_operation_startup()")
            .count(),
        2
    );
    let runtime = cache.split_once("pub(crate) fn run_runtime(").unwrap().1;
    let order = [
        "RuntimeRequest::parse(arguments)?",
        "crate::launch::require_operation_startup()",
        "operate_runtime(&mut System, &request, deadline)",
        // A removal asks the applications before it spawns anything.
        "named_elsewhere(host, flathub_app::FLATHUB_SOURCES, runtime, None, deadline)",
        "host.td_app(",
    ]
    .map(|call| runtime.find(call).unwrap());
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    let application = cache.split_once("pub(crate) fn run(").unwrap().1;
    let order = [
        "Request::parse(arguments)?",
        "crate::launch::require_operation_startup()",
        // An application the image ships is admitted and has no cache.
        "crate::application::admitted_uid(&request.name)?",
        "operate(&mut System, &request, source, deadline)",
    ]
    .map(|call| application.find(call).unwrap());
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    // A Flathub application's operation selects its account before any
    // td-app run: an install by allocating it, every other verb by
    // admitting one the ledger holds, or none, which runs nothing.
    let operation = application.split_once("\nfn operate(").unwrap().1;
    let order = [
        "Verb::Install => host.allocate(&request.name)?,",
        "_ => match host.account(&request.name)? {",
        "Some(_) => host.admitted_uid(&request.name)?,",
        "None => return Err(format!(\"{} is not installed\", request.name)),",
        "host.td_app(",
    ]
    .map(|call| operation.find(call).unwrap());
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    assert_eq!(cache.matches("host.allocate(").count(), 1);
    assert_eq!(
        cache
            .matches("crate::application::allocated_uid(name)")
            .count(),
        1
    );
    // Every td-app run goes through the host, whose only production form
    // is the one spawn.
    assert_eq!(cache.matches("run_td_app(").count(), 2);
    assert!(cache.contains("        run_td_app(command, status, deadline, what)\n"));
    assert_eq!(
        cache
            .matches("crate::application::admitted_uid(&request.name)?")
            .count(),
        1
    );
    // td-app's record grammar, compiled into root: it performs no I/O, so
    // its owners' edits need no pin here.
    let record = include_str!("../../engine/src/flathub_app.rs");
    for forbidden in [
        "unsafe",
        "std::fs",
        "std::process",
        "std::net",
        "std::os",
        "std::io",
        "File",
        "print!",
        "println!",
        "eprint",
        "#[path =",
        "include!",
    ] {
        assert!(!record.contains(forbidden), "flathub_app.rs: {forbidden}");
    }
    // Parse, then the startup audit, then admission, then td-app: nothing
    // inherited reaches td-firstboot's child or td-app.
    let order = [
        "Request::parse(arguments)?",
        "crate::launch::require_operation_startup()",
        "crate::application::admitted_uid(&request.name)?",
        ".spawn()",
    ]
    .map(|call| cache.find(call).unwrap());
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    // Request `24` runs that operation for a name root validated, as a
    // process of its own: one fixed program, null stdin, no environment,
    // and only the peer's verb byte and name among its arguments.
    let software = include_str!("../src/software.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(software),
        SOFTWARE_FINGERPRINT,
        "application requests changed"
    );
    for forbidden in [
        "thread::",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
        ".exec()",
        ".env(",
        ".envs(",
        ".arg(",
        "process_group",
        "unsafe",
    ] {
        assert!(!software.contains(forbidden), "software.rs: {forbidden}");
    }
    assert_eq!(software.matches("Command::new(").count(), 1);
    assert!(software.contains(
        "Command::new(\"/bin/td-authd\");\n    command.args([\"application-cache\", &owner.to_string(), name, verb.word()]);"
    ));
    assert_eq!(software.matches(".spawn()").count(), 1);
    assert_eq!(software.matches(".env_clear()").count(), 1);
    assert_eq!(software.matches(".stdin(Stdio::null())").count(), 1);
    // The peer's name, and each registry name, in the registry grammar.
    assert_eq!(
        software
            .matches("crate::consent::application_name(name)")
            .count(),
        2
    );
    // A running operation is killed only past its own bound and root's
    // backstop beyond it, never because the peer asked again.
    assert_eq!(software.matches(".kill()").count(), 2);
    assert!(software.contains(".and_then(|end| end.checked_add(BACKSTOP));"));
    // The one file it reads is the image's immutable registry, bounded,
    // for the names `23` follows.
    assert!(software.contains("pub(crate) const REGISTRY: &str = \"/etc/td-applications.tsv\";"));
    assert_eq!(software.matches("File::open(").count(), 1);
    assert_eq!(software.matches("std::fs::File::open(REGISTRY)").count(), 1);
    assert!(software.contains("file.take(REGISTRY_READ).read_to_string(&mut text)"));
    assert!(software.contains("const REGISTRY_READ: u64 = REGISTRY_BYTES as u64 + 1;"));
    assert!(software.contains("const REGISTRY_BYTES: usize = 1024 * 1024;"));
    for forbidden in ["fs::read", "fs::write", "OpenOptions", "remove_"] {
        assert!(!software.contains(forbidden), "software.rs: {forbidden}");
    }
    // Request `2c`, and the workspace intake's grant verbs and deletion,
    // run the grant table's one writer, td-firstboot, with the owner, a
    // fixed verb, the kind from two fixed words, and the peer's validated
    // name, access or network and path: one fixed program, null stdin, no
    // environment, no credential change, waited for within its bound by
    // one sleep loop. `perform` is its one entry; `2c` pages what it reads.
    let grants = include_str!("../src/grant_requests.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(grants),
        GRANT_REQUESTS_FINGERPRINT,
        "grant requests changed"
    );
    for forbidden in [
        "thread::spawn",
        "thread::Builder",
        "thread::scope",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
        ".exec()",
        ".env(",
        ".envs(",
        ".arg(",
        "process_group",
        "unsafe",
        "fs::",
        "OpenOptions",
        "File",
        "mount_sys",
    ] {
        assert!(
            !grants.contains(forbidden),
            "grant_requests.rs: {forbidden}"
        );
    }
    assert_eq!(grants.matches("Command::new(").count(), 1);
    assert!(grants.contains(
        "Command::new(\"/bin/td-firstboot\");\n    command.args([\n        \"grant-table\",\n        &owner.to_string(),\n        verb,\n        request.kind.as_str(),\n        &request.name,\n    ]);"
    ));
    // The kind is application or sandbox, a workspace's waits for its
    // launches to go through root, and only a sandbox's network is set.
    assert!(grants.contains(
        "let kind = match kind {\n            0 => Kind::Application,\n            1 => Kind::Sandbox,\n            _ => return Err(invalid()),\n        };"
    ));
    assert!(grants.contains(
        "            (4, [0]) if kind == Kind::Sandbox => Verb::Network(Network::Off),\n            (4, [1]) if kind == Kind::Sandbox => Verb::Network(Network::Host),\n"
    ));
    assert!(grants.contains("    command.args(word);\n"));
    assert!(grants.contains("    command.args(path);\n"));
    assert_eq!(grants.matches("command.args(").count(), 3);
    // The path reaches the writer only once the table's validator took it:
    // a grant's by `validate_path`, a revoke's by `table_path`, which still
    // reaches an inert row.
    for check in [
        "workload_grants::validate_path(path)?;",
        "workload_grants::table_path(path)?;",
    ] {
        assert_eq!(grants.matches(check).count(), 1, "{check}");
        assert!(grants.find(check).unwrap() < grants.find("Command::new(").unwrap());
    }
    assert_eq!(grants.matches(".spawn()").count(), 1);
    assert_eq!(
        grants.matches("perform_with(owner, request, run)").count(),
        2
    );
    assert_eq!(grants.matches("perform_with(").count(), 3);
    assert_eq!(grants.matches(".env_clear()").count(), 1);
    assert_eq!(grants.matches(".stdin(Stdio::null())").count(), 1);
    assert!(grants.contains("    drop(command);\n"));
    assert_eq!(grants.matches("std::thread::sleep(READ_STEP)").count(), 1);
    assert!(grants.contains("const WRITER_TIME: Duration = Duration::from_secs(3);"));
    assert!(grants.contains("const OUTPUT_BYTES: usize = 64 * 1024;"));
    let launch = include_str!("../src/launch.rs");
    // A live boot refuses `2c` as it refuses `0d` and `24`.
    assert!(launch.contains(
        "Request::Grants(request) => {\n                if std::fs::symlink_metadata(LIVE_MEDIUM).is_ok() {\n                    return Ok(vec![0xff, 3]);\n                }\n                Ok(crate::grant_requests::answer(config.owner, &request))"
    ));
    for forbidden in [
        "thread::spawn",
        "thread::Builder",
        "thread::scope",
        "pre_exec",
        ".uid(",
        ".gid(",
        ".groups(",
    ] {
        assert!(!launch.contains(forbidden), "launch.rs: {forbidden}");
    }
    assert_eq!(launch.matches("thread::sleep(").count(), 1);
    assert_eq!(launch.matches(".spawn(").count(), 1);
    assert_eq!(launch.matches(".exec()").count(), 2);
    assert_eq!(launch.matches(".process_group(0)").count(), 1);
    assert_eq!(launch.matches("Command::new(").count(), 7);
    // The root commands a peer can cause beside the credential helper:
    // its supervisor starting an application's unit or the fixed update
    // unit, and reading an application unit's status line. The name is
    // the peer's, in the registry grammar and in the registry.
    assert!(launch.contains(
        "Command::new(\"/bin/td-svc\");\n        command.args([\"start\", unit.name()]);"
    ));
    assert!(launch
        .contains("Command::new(\"/bin/td-svc\");\n        command.args([\"status\", name]);"));
    assert!(launch.contains(".filter(|name| crate::consent::application_name(name))"));
    assert_eq!(launch.matches("match self.registered(").count(), 2);
    assert_eq!(
        launch
            .matches("Some(false) => return Ok(vec![0xff, 4]),")
            .count(),
        2
    );
    // `0f`'s bounds come only from its constants.
    for bound in [
        "const STATE_INTERVAL: Duration = Duration::from_secs(1);",
        "const STATE_TIME: Duration = Duration::from_secs(5);",
        "const STATE_REPLY: usize = 256;",
        "const SUPERVISED_LIMIT: usize = 16;",
        "Inspection::printing(command, STATE_REPLY, STATE_TIME)",
    ] {
        assert_eq!(launch.matches(bound).count(), 1, "{bound}");
    }
    assert_eq!(launch.matches("Inspection::printing(").count(), 1);
    // `0d` starts only the unit a registry name names; the retired update
    // unit, `0e`'s, is named nowhere.
    assert!(launch.contains("let Self::Start(name) = self;\n        name"));
    assert!(!launch.contains("firefox-update"));
    assert_eq!(launch.matches(".stdin(Stdio::null())").count(), 1);
    assert_eq!(launch.matches(".stdout(Stdio::null())").count(), 1);
    // Every child's stderr is null but the session check's, root's private tail.
    assert_eq!(launch.matches(".stderr(stderr)").count(), 1);
    assert_eq!(
        launch.matches("spawn_with(command, Stdio::null())").count(),
        1
    );
    assert_eq!(launch.matches("spawn_with(").count(), 3);
    // The one peer-named program argument is td-software's selection, a
    // registry name checked before the command is built.
    assert_eq!(
        launch
            .matches("command.args([\"--select\", name]);")
            .count(),
        1
    );
    assert_eq!(launch.matches("fn valid_selection(").count(), 1);
    // Root's helper argv carries it only for td-software's own variant.
    assert_eq!(launch.matches("command.arg(name);").count(), 1);
    assert!(launch
        .contains("if let Launch::SoftwareAt(name) = launch {\n            command.arg(name);"));
    assert!(launch.contains("Self::SoftwareAt(_) => Program::Software,"));
    let gate = launch
        .find(
            "if select.is_some_and(|name| terminal != Program::Software || !valid_selection(name))",
        )
        .unwrap();
    assert!(gate < launch.find("command.args([\"--select\", name]);").unwrap());
    // A PIN request lives on only in its clearing owner.
    assert_eq!(launch.matches("bytes.fill(0);").count(), 1);
    assert_eq!(fingerprint(launch), LAUNCH_FINGERPRINT);
    let main = include_str!("../src/main.rs");
    assert!(main.starts_with("#![deny(unsafe_code)]"));
    assert_eq!(main.matches("mod channel;").count(), 1);
    assert_eq!(main.matches("mod sys;").count(), 1);
    let raw = include_str!("../src/sys.rs");
    assert!(raw.contains("#[allow(unsafe_code)]\nfn syscall5("));
    assert!(raw.contains("#[allow(unsafe_code)]\nfn adopt("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("OwnedFd::from_raw_fd").count(), 1);
    for constant in [
        "const SYS_POLL: usize = 7;",
        "const POLLRDHUP: i16 = 0x2000;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_SETSOCKOPT: usize = 54;",
        "const SYS_GETSOCKOPT: usize = 55;",
        "const SO_PASSCRED: usize = 16;",
        "const SO_PASSPIDFD: usize = 76;",
        "const SO_PEERCRED: usize = 17;",
        "const SOL_SOCKET: usize = 1;",
        "const SCM_PIDFD: i32 = 4;",
        "const SCM_CREDENTIALS: i32 = 2;",
        "const SCM_RIGHTS: i32 = 1;",
        "const CONTROL: usize = 128;",
        "const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;",
    ] {
        assert!(raw.contains(constant), "{constant}");
    }
    assert_eq!(raw.matches("const SYS_").count(), 4);
    assert_eq!(raw.matches("syscall5(").count(), 6);
    assert_eq!(raw.matches("adopt(").count(), 2);
    let mounts = include_str!("../src/mount_sys.rs");
    assert_eq!(
        fingerprint(mounts),
        0x5348cf939e0013fa,
        "fixed mount boundary changed"
    );
    for constant in [
        "const SYS_UNSHARE: usize = 272;",
        "const SYS_OPEN_TREE: usize = 428;",
        "const SYS_MOVE_MOUNT: usize = 429;",
        "const SYS_MOUNT_SETATTR: usize = 442;",
        "const CLONE_NEWUSER: usize = 0x1000_0000;",
        "const AT_EMPTY_PATH: usize = 0x1000;",
        "const OPEN_TREE_FLAGS: usize = 1 | 0x80000 | AT_EMPTY_PATH;",
        "const READ_ONLY_VIEW: u64 = 0x100000 | 1 | 2 | 4 | 8;",
        "const WRITABLE_VIEW: u64 = 0x100000 | 2 | 4 | 8;",
        "const READ_ONLY_EXECUTABLE_VIEW: u64 = 0x100000 | 1 | 2 | 4;",
        "const WRITABLE_EXECUTABLE_VIEW: u64 = 0x100000 | 2 | 4;",
        "const MOVE_FLAGS: usize = 4 | 0x40;",
    ] {
        assert!(mounts.contains(constant), "{constant}");
    }
    assert_eq!(mounts.matches("const SYS_").count(), 4);
    assert_eq!(mounts.matches("core::arch::asm!").count(), 1);
    assert_eq!(mounts.matches("OwnedFd::from_raw_fd").count(), 1);
    assert_eq!(mounts.matches("#[allow(unsafe_code)]").count(), 2);
    assert_eq!(mounts.matches("syscall5(").count(), 5);
    assert_eq!(mounts.matches("adopt(").count(), 2);
    // The four attribute words are the only ones a view takes, and only
    // the two executable ones lack NOEXEC (8).
    assert_eq!(mounts.matches("_VIEW: u64 = ").count(), 4);
    assert_eq!(mounts.matches("_EXECUTABLE_VIEW: u64 = ").count(), 2);
    assert_eq!(mounts.matches("set_attributes(").count(), 2);
    let files = include_str!("../src/portal_files.rs");
    assert_eq!(
        fingerprint(files),
        0x0a06a1b75210af46,
        "root portal grant controller changed"
    );
    assert_eq!(files.matches("Command::new(\"/bin/td-authd\")").count(), 1);
    // One unmount, the fixed applet, for the portal's two fixed views, the
    // Downloads view and the Firefox handoff view, and the grant views.
    assert_eq!(files.matches("Command::new(\"/bin/umount\")").count(), 1);
    assert_eq!(files.matches(".arg(path)").count(), 1);
    assert!(files.contains("grant_views::unmount_all(VIEW)"));
    assert!(files.contains("unmount(HANDOFF_VIEW)?"));
    assert_eq!(files.matches(".spawn(").count(), 1);
    assert_eq!(
        files.matches("launch::require_launch_startup()?").count(),
        5
    );
    assert_eq!(files.matches("mount_sys::new_user_namespace(").count(), 1);
    assert_eq!(files.matches("mount_sys::clone_directory(").count(), 2);
    // The Downloads view is read-only; only the handoff view is writable,
    // and only Firefox's UID maps to it.
    assert_eq!(files.matches("mount_sys::view_attributes(").count(), 2);
    assert_eq!(files.matches("ViewAccess::ReadOnly)").count(), 1);
    assert_eq!(files.matches("ViewAccess::Writable)").count(), 1);
    assert_eq!(files.matches("namespace_mapping(uid, PORTAL)").count(), 1);
    assert_eq!(files.matches("admitted_uid(\"firefox\")").count(), 1);
    assert_eq!(files.matches("mount_sys::publish(").count(), 2);
    // The Downloads view exists only while FileChooser's application holds
    // the grant of it.
    assert!(files.contains("pub(crate) const FILE_CHOOSER_APPLICATION: &str = \"firefox\";"));
    assert!(files.contains("const FILE_CHOOSER_DIRECTORY: &str = \"Downloads\";"));
    // Grant views: one clone, one attribute call selected by the row's
    // access, one publication, one mapping per launch to the account's
    // uid, release through the portal's one unmount, a lock waited for by
    // sleeping to a deadline, and the manifest only ever replaced by a
    // rename, never removed.
    let views = include_str!("../src/grant_views.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(views),
        GRANT_VIEWS_FINGERPRINT,
        "root grant view publisher changed"
    );
    for (call, count) in [
        ("mount_sys::clone_directory(", 1),
        ("mount_sys::view_attributes(", 1),
        ("mount_sys::publish(", 1),
        ("mount_sys::new_user_namespace(", 0),
        ("portal_files::namespace_for(self.uid)", 1),
        ("portal_files::unmount(", 1),
        ("launch::require_launch_startup()?", 1),
        ("Command", 0),
        (".spawn(", 0),
        (".exec(", 0),
        ("namespace_mapping", 0),
        ("::thread", 1),
        ("std::thread::sleep(LOCK_POLL)", 1),
        ("file.try_lock()", 1),
        (".lock()", 1),
        ("io::stderr().lock()", 1),
        ("fs::rename(", 1),
        ("remove_file(directory, STAGED_MANIFEST)?", 1),
        ("grants::MANIFEST_NAME)", 1),
        ("mount_identity::human_source_identity(", 1),
        ("with_account(", 2),
    ] {
        assert_eq!(views.matches(call).count(), count, "grant_views.rs: {call}");
    }
    // A workspace's views come from its running instances' plans, under
    // its account's lock, and never from the grant table; every view is
    // admitted for its account's kind, td-agent's own trees for a
    // workspace's alone.
    assert!(views.contains(
        "    with_account(Kind::Workspace, uid, |live, _, mountinfo, home| {\n        let grants = grants_for(held()?, Kind::Workspace, mountinfo, home)?;\n"
    ));
    assert!(views.contains(
        "    require_kind_uid(kind, uid)?;\n    if kind == Kind::Workspace {\n        return Err("
    ));
    assert!(views.contains(
        "        grants::validate_view(grant.kind, grant.access, grant.path.as_bytes())?;\n"
    ));
    assert!(views.contains("view_access(grant.access, grant.executable)"));
    // The account's kind alone selects executable views; no row, request
    // or caller does.
    assert_eq!(
        views.matches("executable: executable_views(kind),").count(),
        1
    );
    assert!(views.contains(
        "fn executable_views(kind: Kind) -> bool {\n    match kind {\n        Kind::Application => false,\n        Kind::Sandbox | Kind::Workspace => true,\n    }\n}"
    ));
    // The workspace intake: the primary human's peers alone, one fixed
    // helper argv for the ledger account, one fixed jail argv and one
    // fixed emptier argv, the accepted connection a launch's only
    // descriptors beside a private stderr. It allocates only at `create`,
    // prepares an existing row for a launch, a slot's first instance and
    // a deletion, reaches the grant table only through grant_requests'
    // writer, and publishes a slot's views only from its plans.
    let workspace = include_str!("../src/workspace.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(
        fingerprint(workspace),
        WORKSPACE_FINGERPRINT,
        "root workspace launch intake changed"
    );
    for (call, count) in [
        ("Command::new(", 3),
        ("Command::new(\"/bin/td-login\")", 2),
        ("Command::new(\"/bin/td-jail\")", 1),
        (".spawn(", 2),
        (".exec()", 1),
        ("stderr_tail::status(&mut command)", 1),
        ("crate::secret_sys::peer_uid(&stream)", 1),
        ("grant_views::publish(Kind::Sandbox, name, uid)?", 1),
        ("grant_views::publish(", 1),
        (
            "grant_views::publish_plan(uid, || self.instances.union(uid))",
            2,
        ),
        ("application::admit(", 4),
        ("&application::PREPARE_SANDBOX", 2),
        ("&application::ALLOCATE_SANDBOX", 1),
        ("&application::PREPARE_WORKSPACE", 2),
        ("&application::ALLOCATE_WORKSPACE", 1),
        (".open(pinned(&directory, \"cgroup.kill\"))", 1),
        ("std::thread::sleep(STOP_POLL)", 2),
        ("crate::sys::channel_open(monitor)", 1),
        ("grant_requests::perform(", 1),
        ("application::require_process(", 2),
        ("Stdio::from(OwnedFd::from(", 3),
        ("fs::remove_dir_all(", 1),
        ("unsafe", 0),
    ] {
        assert_eq!(
            workspace.matches(call).count(),
            count,
            "workspace.rs: {call}"
        );
    }
    assert!(workspace.contains("matches!(peer, Ok(uid) if uid == primary_account::UID)"));
    // workspace-exec's form is its ledger row's kind's, checked before the
    // process; a failed instance's account is prepared again; a launch's
    // hold and spec are given back by a guard, a panic included.
    assert!(workspace.contains(
        "    let launch = Exec::parse(arguments)?;\n    require_form(&accounts::load()?, &launch)?;\n    application::require_process(\n"
    ));
    assert!(workspace.contains(
        "            self.release_views(prepared.uid, id);\n            if failed {\n                self.instances.forget(prepared.uid);\n            }\n"
    ));
    assert!(workspace.contains(
        "impl<S: Serve> Drop for Launched<'_, S> {\n    fn drop(&mut self) {\n        self.serve.release(&self.prepared, self.failed);\n        let _ = fs::remove_file(&self.prepared.spec);\n"
    ));
    // The service runs each connection through the real preparation and
    // launch, checks what the human can mend before its answer, and never
    // leaves its accept loop over a failed accept.
    assert!(workspace.contains(
        "                let root = Root {\n                    instances: &active.instances,\n                };\n                handle(stream, id, &active, &root);\n"
    ));
    assert_eq!(workspace.matches("handle(").count(), 2);
    let prepare = workspace
        .split(
            "fn prepare(&self, name: &str, launch: &Launch, id: u64) -> Result<Prepared, String> {",
        )
        .nth(1)
        .unwrap();
    let at = |needle: &str| prepare.find(needle).unwrap();
    assert!(
        at("account_uid(&accounts::load()?, Kind::Sandbox, name)?;")
            < at("require_runtime(Path::new(flathub_app::RUNTIME_STORE), launch.runtime)?;")
            && at("require_runtime(") < at("application::admit(")
            && at("application::admit(") < at("grant_views::publish(")
            && at("grant_views::publish(") < at("specs().write(uid, id, &text)?")
    );
    // A slot's instance: its own row, its runtime, its account prepared
    // once a boot, its plan held, then its views and spec; a refusal
    // or a panic after the hold gives it back, through its guard.
    let instance = workspace
        .split("fn instance(&self, slot: &str, instance: &Instance, id: u64) -> Result<Prepared, String> {")
        .nth(1)
        .unwrap();
    let at = |needle: &str| instance.find(needle).unwrap();
    assert!(
        at("account_uid(&accounts::load()?, Kind::Workspace, slot)?;")
            < at("require_runtime(Path::new(flathub_app::RUNTIME_STORE), instance.runtime)?;")
            && at("require_runtime(") < at("self.instances.prepare_once(uid, ||")
            && at("prepare_once(") < at("self.instances.hold(uid, id, &instance.plan)?;")
            && at("hold(") < at("let hold = PlanHold {")
            && at("let hold = PlanHold {") < at("self.launch_instance(uid, instance, id)?;")
            && at("launch_instance(") < at("std::mem::forget(hold);")
    );
    // A deletion first ends the account's processes, then clears the
    // rows, and the account, not root, empties its home.
    let delete = workspace
        .split("fn delete(\n    kind: Kind,\n    name: &str,")
        .nth(1)
        .unwrap();
    let at = |needle: &str| delete.find(needle).unwrap();
    assert!(
        at("let account = account()?;") < at("stop(uid)?;")
            && at("stop(uid)?;") < at("match clear(&request) {")
            && at("match clear(&request) {") < at("let Some(uid) = account else {")
            && at("let Some(uid) = account else {") < at("if prepare()? != uid {")
            && at("prepare()?") < at("empty(uid)")
    );
    assert!(
        workspace.contains(".args([\"/bin/td-authd\", \"workspace-empty\", &uid.to_string()]);")
    );
    // The account's emptying, after its credentials are checked, walks its
    // home and then its scratch tree's directories, each by the one walk.
    assert!(workspace.contains(
        "    empty_home(&Kind::Sandbox.home(uid), uid)?;\n    empty_scratch(Path::new(accounts::DIRECTORY), uid)\n}"
    ));
    assert!(workspace.contains(
        "        empty_home(&directory, uid)?;\n        // The walk opened it to its owner alone; it is a `/tmp` again,\n        // through a handle that follows no link.\n        let handle = OpenOptions::new()\n            .read(true)\n            .custom_flags(O_PATH | O_NOFOLLOW | O_DIRECTORY)\n            .open(&directory)\n"
    ));
    assert!(workspace
        .contains("fs::set_permissions(fd_path(&handle)?, fs::Permissions::from_mode(0o1777))"));
    assert_eq!(workspace.matches("empty_scratch(").count(), 2);
    let accepting = workspace
        .split("pub(crate) fn serve()")
        .nth(1)
        .and_then(|serve| serve.split("    loop {").nth(1))
        .unwrap();
    assert_eq!(accepting.matches("return").count(), 0);
    assert_eq!(accepting.matches("?;").count(), 0);
    assert_eq!(
        workspace
            .matches(".args([\"exec-service-as\", &format!(\"tdw{uid}\"), \"--\"])")
            .count(),
        2
    );
    assert!(views.contains("format!(\"{}/{}/{index}\", grants::VIEW_ROOT, self.uid)"));
    // The portal's Downloads view: one lock for its two callers, and every
    // mount at it released before one is published.
    assert_eq!(files.matches("let _lock = lock_view(&var)?;").count(), 2);
    assert!(files.contains(
        "    release_view()?;\n    let target = ensure_root_child(&session, \"Downloads\")?;"
    ));
    assert!(files.contains("    grant_views::unmount_all(VIEW)\n"));
    let channel = include_str!("../src/channel.rs");
    assert_eq!(channel.matches("sys::prepare(").count(), 1);
    assert_eq!(channel.matches("sys::receive(").count(), 1);
    assert_eq!(channel.matches("sys::alive(").count(), 2);
    assert!(channel.contains("Self::connect(stream, expected_uid, 0)"));
    assert_eq!(channel.matches("Self::connect(").count(), 1);
    assert_eq!(channel.matches("fn connect(").count(), 1);
    assert!(!main.contains("sys::"));
    // Whole-source pin makes argument layout and adoption provenance an edit
    // to the review record rather than slack in keyword counts.
    let digest = raw.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    });
    assert_eq!(digest, RAW_FINGERPRINT);
    // Pin startup as well as raw code: aliases can evade API-name scans.
    assert_eq!(
        fingerprint(main),
        MAIN_FINGERPRINT,
        "main.rs: production startup changed"
    );
    assert_eq!(
        fingerprint(channel),
        0xdf20e4130b2d96e2,
        "channel.rs: production startup changed"
    );
}

const RAW_FINGERPRINT: u64 = 0xb64878d738ec4031;
const GRANT_REQUESTS_FINGERPRINT: u64 = 0x8d1f61d7a37a8d2c;
const GRANT_VIEWS_FINGERPRINT: u64 = 0xea86f6e9834554d2;
const WORKSPACE_FINGERPRINT: u64 = 0x6b8b6d3339f5ebc1;
const APPLICATION_FINGERPRINT: u64 = 0xc03468866b2802cb;

fn fingerprint(source: &str) -> u64 {
    source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

const LOGIN_STATE_FINGERPRINT: u64 = 0x10b1af1d87d02b94;
const SHARED_LOGIN_STATE_FINGERPRINT: u64 = 0x36b736e5dda17cc6;
const SHARED_HOSTNAME_FINGERPRINT: u64 = 0x49026f28c1db76ec;

const CACHE_FINGERPRINT: u64 = 0x8ecf8ebf4fff8eaf;
const SOFTWARE_FINGERPRINT: u64 = 0x2637cc331de38b7c;
const LAUNCH_FINGERPRINT: u64 = 0x33d421b9691fb2fb;
const MAIN_FINGERPRINT: u64 = 0xed3640ea05832242;
const STDERR_TAIL_FINGERPRINT: u64 = 0x3b8b113de258e6ee;
const SESSION_FINGERPRINT: u64 = 0xd7705b7717a76af5;

const INTAKE_RAW_FINGERPRINT: u64 = 0x320c8b6ddbfe29af;
const INTAKE_FINGERPRINT: u64 = 0x755e883a75780a83;
const WRITE_REQUEST_FINGERPRINT: u64 = 0x188c619caba6ceb8;

const INSTALLATION_FINGERPRINT: u64 = 0xabb4e7cd418585ae;
const SHARED_LOGIN_TIER_FINGERPRINT: u64 = 0xb0d5ccf021f64f9d;
const ROLLBACK_FINGERPRINT: u64 = 0xf96a32041165e345;
const ELEVATION_FINGERPRINT: u64 = 0x21e50efe028ca4b6;
const SET_HOSTNAME_FINGERPRINT: u64 = 0x9c3353d7916f6012;
const BACKOFF_FINGERPRINT: u64 = 0x2749dbb039baaa3a;
const SHARED_SAVED_FINGERPRINT: u64 = 0x181983faf45559fa;
const REVOCATION_FINGERPRINT: u64 = 0x7090a6da9e98d2e9;
const SHARED_CUTOVER_FINGERPRINT: u64 = 0x358939aac41ef40a;
const DISK_INSTALL_FINGERPRINT: u64 = 0x45e5e48767f17d03;
const CONSENT_CODEC_FINGERPRINT: u64 = 0x855c6ad96aa12772;
