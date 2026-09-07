#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::Path;

fn raw_module_tokens(text: &str) -> usize {
    identifier_count(text, "sys")
}

fn identifier_count(text: &str, identifier: &str) -> usize {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|token| *token == identifier)
        .count()
}

#[test]
fn source_inventory_and_allowances_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let expected: BTreeSet<_> = [
        "clipboard.rs",
        "command.rs",
        "control.rs",
        "control_frame.rs",
        "control_jobs.rs",
        "control_socket.rs",
        "control_worker.rs",
        "data.rs",
        "dialog.rs",
        "files.rs",
        "fill.rs",
        "keys.rs",
        "keyboard.rs",
        "layout.rs",
        "lib.rs",
        "main.rs",
        "menu.rs",
        "model.rs",
        "number.rs",
        "pointer.rs",
        "render.rs",
        "replace.rs",
        "replay.rs",
        "seat.rs",
        "search.rs",
        "session.rs",
        "spelling.rs",
        "sys.rs",
        "text.rs",
        "transfer.rs",
        "ui.rs",
        "wayland.rs",
        "xkb.rs",
        "xkb_compat.rs",
        "xkb_keys.rs",
        "xkb_symbols.rs",
        "xkb_syntax.rs",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(
            entry.file_type().unwrap().is_file(),
            "no nested or symlinked source"
        );
        let name = entry.file_name().into_string().unwrap();
        actual.insert(name.clone());
        let text = std::fs::read_to_string(entry.path()).unwrap();
        if !matches!(name.as_str(), "wayland.rs" | "control_frame.rs") {
            assert_eq!(
                identifier_count(&text, "Frames"),
                0,
                "frame owner in {name}"
            );
            assert_eq!(
                identifier_count(&text, "invalidate_caret"),
                0,
                "caret bypass in {name}"
            );
        }
        if !matches!(
            name.as_str(),
            "keyboard.rs"
                | "xkb.rs"
                | "xkb_syntax.rs"
                | "xkb_keys.rs"
                | "xkb_symbols.rs"
                | "xkb_compat.rs"
                | "seat.rs"
                | "wayland.rs"
        ) {
            for module in [
                "keyboard",
                "xkb",
                "xkb_syntax",
                "xkb_keys",
                "xkb_symbols",
                "xkb_compat",
            ] {
                assert_eq!(
                    identifier_count(&text, module),
                    usize::from(name == "lib.rs"),
                    "compiler access outside input adapter: {name}"
                );
            }
            for interface in ["wl_seat", "wl_keyboard"] {
                assert!(
                    !text.contains(interface),
                    "input binding outside the adapter: {name}"
                );
            }
        }
        let budget = match name.as_str() {
            "lib.rs" | "main.rs" => 1,
            "sys.rs" => 4,
            _ => 0,
        };
        assert_eq!(
            text.matches("unsafe").count(),
            budget,
            "keyword count in {name}"
        );
        assert!(
            !text.contains("cfg_attr"),
            "conditional allowance in {name}"
        );
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(!compact.contains("include!("), "generated source in {name}");
        let paths = compact.matches("#[path=").count();
        assert_eq!(
            paths,
            if name == "lib.rs" { 3 } else { 0 },
            "source paths in {name}"
        );
        if name == "lib.rs" {
            assert!(compact.starts_with("#![deny(unsafe_code)]"));
            for (file, declaration) in [
                ("font.rs", "pubmodfont;"),
                ("font_data.rs", "modfont_data;"),
                ("wire.rs", "modwire;"),
            ] {
                assert!(compact.contains(&format!(
                    "#[path=\"../../td-compositor/src/{file}\"]{declaration}"
                )));
                let shared =
                    std::fs::read_to_string(root.join("../td-compositor/src").join(file)).unwrap();
                for module in [
                    "keyboard",
                    "xkb",
                    "xkb_syntax",
                    "xkb_keys",
                    "xkb_symbols",
                    "xkb_compat",
                ] {
                    assert_eq!(
                        identifier_count(&shared, module),
                        0,
                        "partial type validation through shared source: {file}"
                    );
                }
                for interface in ["wl_seat", "wl_keyboard"] {
                    assert!(
                        !shared.contains(interface),
                        "input binding through shared source: {file}"
                    );
                }
                assert!(!shared.contains("unsafe"));
                assert_eq!(
                    raw_module_tokens(&shared),
                    0,
                    "shared raw-module access in {file}"
                );
                let shared: String = shared.chars().filter(|c| !c.is_whitespace()).collect();
                assert!(!shared.contains("#[path="));
                assert!(!shared.contains("include!("));
                assert!(!shared.contains("cfg_attr"));
            }
        }
        // The pinned procfs pathname's sys segment is not raw-module access.
        let raw_tokens = if name == "control_socket.rs" {
            let literal = "\"/proc/sys/kernel/overflowuid\"";
            assert_eq!(text.matches(literal).count(), 1);
            raw_module_tokens(&text.replacen(literal, "\"\"", 1))
        } else {
            raw_module_tokens(&text)
        };
        assert_eq!(
            raw_tokens,
            match name.as_str() {
                "lib.rs" => 1,
                "files.rs" => 1,
                "wayland.rs" => 4,
                "transfer.rs" => 2,
                _ => 0,
            },
            "unrostered raw-module access in {name}"
        );
    }
    assert_eq!(actual, expected);
}

#[test]
fn complete_raw_layer_and_production_callers_are_pinned() {
    let raw = include_str!("../src/sys.rs");
    let hash = raw.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(
        hash, 0xc1b0a580e9da8ee8,
        "review the complete raw layer before updating its fingerprint"
    );
    for pin in [
        "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_FCNTL: usize = 72;",
        "const SYS_FLISTXATTR: usize = 196;",
        "syscall3(SYS_FLISTXATTR, file.as_raw_fd() as usize, 0, 0)",
        "const F_DUPFD_CLOEXEC: usize = 1030;",
        "const F_GETFL: usize = 3;",
        "const F_SETFL: usize = 4;",
        "const O_NONBLOCK: usize = 0o4000;",
        "const O_ACCMODE: usize = 3;",
        "#[allow(unsafe_code)]\nfn syscall3(",
        "#[allow(unsafe_code)]\nfn adopt(",
    ] {
        assert!(raw.contains(pin), "{pin}");
    }
    assert!(!raw.contains("#![allow("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("OwnedFd::from_raw_fd").count(), 1);
    let transfer = include_str!("../src/transfer.rs");
    assert_eq!(transfer.matches("crate::sys::").count(), 2);
    assert_eq!(
        transfer
            .matches("crate::sys::Destination::new(fd)?")
            .count(),
        1
    );
    assert_eq!(
        transfer
            .matches("destination: crate::sys::Destination,")
            .count(),
        1
    );
    let files = include_str!("../src/files.rs");
    assert_eq!(files.matches("crate::sys::has_attributes(file)").count(), 1);
    assert_eq!(files.matches("crate::sys::").count(), 1);
    for pin in [
        "const O_NOFOLLOW: i32 = 0o400000;",
        "const O_NONBLOCK: i32 = 0o4000;",
        "const O_DIRECTORY: i32 = 0o200000;",
        ".custom_flags(O_NOFOLLOW | O_NONBLOCK)",
        "temporary.file.sync_all()?;",
        "location.parent.sync_all()?;",
        "fs::rename(&temporary.path, &location.path)?;",
        "fs::hard_link(&temporary.path, &location.path)",
    ] {
        assert!(files.contains(pin), "file transaction pin: {pin}");
    }
    let adapter = include_str!("../src/wayland.rs");
    assert_eq!(adapter.matches("Keymap::parse(source)").count(), 1);
    assert_eq!(adapter.matches("File::from(fd)").count(), 1);
    assert_eq!(adapter.matches("read_keymap(fd, format, size)").count(), 1);
    assert!(adapter.contains("file.read_exact_at(&mut bytes, 0)"));
    assert_eq!(adapter.matches("crate::sys::").count(), 4);
    for call in [
        "crate::sys::inherited(fd)",
        "crate::sys::send_file(&self.stream, suffix, file)",
        "crate::sys::receive(&self.stream, &mut self.read)",
        "crate::sys::receive_for_test(peer, &mut buf)",
    ] {
        assert_eq!(adapter.matches(call).count(), 1, "{call}");
    }
}

#[test]
fn control_socket_publication_keeps_kernel_path_and_identity_checks_explicit() {
    let source = include_str!("../src/control_socket.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    for pin in [
        "const O_DIRECTORY: i32 = 0o200000;",
        "const O_NOFOLLOW: i32 = 0o400000;",
        "const O_PATH: i32 = 0o10000000;",
        "const PATH_BYTES: usize = 107;",
        "const NAME_BYTES: usize = 80;",
        "const STATUS_BYTES: usize = 64 * 1024;",
        "UnixListener::bind(&pinned_path)",
        "custom_flags(O_PATH | O_DIRECTORY | O_NOFOLLOW)",
        "custom_flags(O_PATH | O_NOFOLLOW)",
        "File::open(\"/proc/self/status\")",
        "File::open(\"/proc/sys/kernel/overflowuid\")",
        "const UID_BYTES: usize = 11;",
        ".take(UID_BYTES as u64 + 1)",
        "unambiguous_uid(uid, overflow_uid(&bytes)?)",
        "overflow == uid || overflow == 0",
        "Permissions::from_mode(0o600)",
        "identity(&named) != identity(&self.node.metadata()?)",
        "fs::metadata(descriptor_path(&self.parent)).map_err",
        "control cleanup parent is unavailable",
        "trusted_ancestor(&current.metadata()?, uid)?",
        "metadata.uid() != uid && metadata.uid() != 0",
        "metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0",
    ] {
        assert!(production.contains(pin), "{pin}");
    }
    assert_eq!(production.matches("UnixListener::bind(").count(), 1);
    assert!(!production.contains("UnixStream::connect"));
    assert!(!production.contains("std::env"));
    assert!(!production.contains("env::"));
    assert!(!production.contains("canonicalize"));
    assert!(!production.contains("65534"));
    assert!(!production.contains("TD_TEST_TRUSTED_ROOT"));
}

#[test]
fn control_worker_keeps_bounded_nonblocking_transport_separate_from_editor_state() {
    let source = include_str!("../src/control_worker.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    for pin in [
        "const CONNECTIONS: usize = 8;",
        "const IO_BYTES: usize = 16 * 1024;",
        "Duration::from_secs(5)",
        "Duration::from_millis(10)",
        "mpsc::sync_channel(CONNECTIONS)",
        "mpsc::sync_channel(1)",
        "for _ in connections.len()..CONNECTIONS",
        "requests.try_send(job)",
        "self.requests.try_recv()",
        "reply.try_send(response)",
        "Request::parse(payload)",
        "thread::park_timeout(poll_interval(connections.is_empty()))",
        "Duration::from_millis(100)",
        ".join()",
    ] {
        assert!(production.contains(pin), "{pin}");
    }
    let socket = include_str!("../src/control_socket.rs");
    assert!(socket.contains("socket.listener.set_nonblocking(true)?"));
    assert!(socket.contains("stream.set_nonblocking(true)?"));
    for forbidden in [
        "Controller",
        "mpsc::channel(",
        ".read_exact(",
        ".write_all(",
        ".recv()",
        ".send(",
    ] {
        assert!(!production.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn only_native_caret_ticks_bypass_the_input_context_fence() {
    let ui = include_str!("../src/ui.rs");
    assert!(ui.contains(concat!(
        "Event::Tick(now) => {\n",
        "                if now < self.clock {\n",
        "                    return Err(Error::InvalidArgument);\n",
        "                }\n",
        "                let visible = self.focused && ((now - self.blink_start) / 500).is_multiple_of(2);\n",
        "                self.clock = now;\n",
        "                let changed = visible != self.caret_visible;\n",
        "                self.caret_visible = visible;\n",
        "                Ok(if changed {\n",
        "                    Outcome::Changed\n",
        "                } else {\n",
        "                    Outcome::Ignored\n",
        "                })\n",
        "            }"
    )));
    let source = include_str!("../src/wayland.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        production.matches("self.frames.invalidate_caret(").count(),
        1
    );
    let tick = production.split("fn tick(").nth(1).unwrap();
    let tick = tick.split("\n    fn ").next().unwrap();
    assert!(tick.contains(concat!(
        "let before = self.ui.generation();\n",
        "        self.ui.dispatch(Event::Tick(now)).map_err(error)?;\n",
        "        self.frames.invalidate_caret(self.ui.generation() != before);\n",
        "        self.clock = now;\n",
        "        let before = self.ui.generation();\n",
        "        self.clipboard_tick(now, repeat)?;\n",
        "        self.frames.invalidate(self.ui.generation() != before);"
    )));
}

#[test]
fn native_control_is_opt_in_and_liveness_checked_with_bounded_outer_turns() {
    let source = include_str!("../src/wayland.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    assert_eq!(production.matches("self.control_tick();").count(), 1);
    let end_turn = production.split("fn end_turn(").nth(1).unwrap();
    let end_turn = end_turn.split("\n    fn ").next().unwrap();
    assert!(end_turn.contains("self.control_tick();"));
    assert!(
        end_turn.find("self.spelling.step(").unwrap()
            < end_turn.find("self.observe_control_jobs();").unwrap()
    );
    assert!(
        end_turn.find("self.observe_control_jobs();").unwrap()
            < end_turn.find("self.control_tick();").unwrap()
    );
    assert!(end_turn.contains("self.frames.generation().map_err(error)?"));
    let poll = production.split("fn control_tick(").nth(1).unwrap();
    let poll = poll.split("\n    fn ").next().unwrap();
    assert!(production.contains("const CONTROL_JOBS_PER_TURN: usize = 2;"));
    assert!(poll.contains("let mut budget = CONTROL_JOBS_PER_TURN;"));
    assert!(poll.contains("for _ in 0..self.frame_waiters.len()"));
    assert!(poll.contains("self.frame_waiters.len() < crate::control_worker::CONNECTIONS"));
    assert!(poll.contains("error: crate::Error::Limit"));
    assert_eq!(poll.matches("budget -= 1;").count(), 2);
    assert_eq!(production.matches("self.frames.complete()").count(), 1);
    let draw = production
        .split("fn draw(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(draw.contains("self.callback.is_some()"));
    assert!(draw.contains("self.frames.generation().map_err(error)?"));
    assert!(draw.find("self.frames.capture").unwrap() < draw.find("Raster::new").unwrap());
    assert!(draw.find("words(SURFACE, 6").unwrap() < draw.find("self.frames.submit").unwrap());
    for pin in [
        "if self.closed",
        "Duration::from_millis(10)",
        "for _ in 0..CONTROL_JOBS_PER_TURN",
        "worker.try_request()",
        "job.respond_with(|request| self.control_response(request))",
    ] {
        assert!(poll.contains(pin), "{pin}");
    }
    for forbidden in [".read(", ".write(", ".recv(", "ui.dispatch("] {
        assert!(!poll.contains(forbidden), "{forbidden}");
    }
    assert!(production.contains("control: None"));
    assert!(production.contains("fn control_response(&mut self,"));
    assert!(production.contains("let response = request.response(&self.ui);"));
    let dispatch = production.split("fn control_response(").nth(1).unwrap();
    let dispatch = dispatch.split("\n    fn ").next().unwrap();
    assert!(
        dispatch.find("self.frames.generation()").unwrap()
            < dispatch.find("request.is_mutating()").unwrap()
    );
    assert!(dispatch.contains("self.closed || self.pointer_modal() || self.menu.is_some()"));
    assert!(
        dispatch.find("request.is_mutating()").unwrap()
            < dispatch.find("Operation::CloseTab").unwrap()
    );
    assert!(dispatch.contains("request.execute(&mut self.ui)"));
    assert!(dispatch.contains("request.spelling_response(&self.ui, &self.spelling)"));
    assert_eq!(dispatch.matches("self.ui.dispatch(").count(), 1);
    assert_eq!(dispatch.matches("Event::").count(), 1);
    assert!(dispatch.contains("self.ui.dispatch(Event::New)"));
    assert!(dispatch.contains("self.control_open_job(path.clone())"));
    let key = production
        .split("fn control_key(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    let mut prior = 0;
    for guard in [
        "control_key_available()",
        "generation != self.frames.input_generation()?",
        "revision_point(target.tab, target.revision)",
        "self.ui.editor().active() != Some(target.tab)",
        "validate_chord(chord)?",
        ".checked_add(8)",
    ] {
        let position = key.find(guard).unwrap();
        assert!(prior < position);
        prior = position;
        assert!(key.find(guard).unwrap() < key.find("self.chord(chord, false)").unwrap());
    }
    assert!(key.contains("self.control_input_error = Some(detail)"));
    assert!(
        end_turn.find("self.control_tick();").unwrap()
            < end_turn.rfind("self.control_input_health()?").unwrap()
    );
    assert!(
        draw.find("self.control_input_health()?").unwrap()
            < draw.find("self.frames.capture").unwrap()
    );
    assert!(
        dispatch.find("self.control_input_error.is_some()").unwrap()
            < dispatch.find("Operation::Key").unwrap()
    );
    assert!(
        key.find("self.control_mutation_accepted()").unwrap()
            < key.find("self.chord(chord, false)").unwrap()
    );
    assert!(!key.contains("Event::") && !key.contains("activation_serial ="));
    assert!(!key.contains("self.input.arm(") && !key.contains("self.clipboard_request("));
    let ready = production
        .split("fn control_key_available(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    for guard in [
        "self.prompt.is_none()",
        "self.closing.is_none()",
        "self.conflict.is_none()",
        "self.reloading.is_none()",
        "self.input.focused",
        "self.input.synchronized",
        "self.input.map.is_some()",
        "self.activation_serial.is_none()",
    ] {
        assert!(ready.contains(guard));
    }
    let open = production
        .split("fn control_open_job(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    let pointer = production
        .split("fn control_pointer_event(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    let cleanup = pointer.find("self.control_mutation_accepted()").unwrap();
    let mut prior = 0;
    for guard in [
        "control_pointer_available()",
        "generation != self.frames.input_generation()?",
        "revision_point(tab, revision)",
        "self.ui.editor().active() != Some(tab)",
        "check_revision(gesture)",
        ".checked_add(8)",
    ] {
        let at = pointer.find(guard).unwrap();
        assert!(prior <= at && at < cleanup, "{guard}");
        prior = at;
    }
    assert!(pointer.contains("self.control_input_error = Some(detail)"));
    assert!(pointer.contains("self.decoded_pointer_action(phase, fixed_x, fixed_y, extend)"));
    assert!(!pointer.contains("Event::") && !pointer.contains("activation_serial ="));
    assert!(!pointer.contains("self.pointer.x =") && !pointer.contains("self.pointer.y ="));
    let physical = production
        .split("fn pointer_action(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(physical
        .contains("self.decoded_pointer_action(phase, self.pointer.x, self.pointer.y, extend)"));
    assert!(
        dispatch.find("request.is_mutating()").unwrap()
            < dispatch.find("Operation::Open(path)").unwrap()
    );
    assert!(open.find("files.busy()").unwrap() < open.find("begin_open()").unwrap());
    assert!(open.find(".checked_add(1)").unwrap() < open.find("begin_open()").unwrap());
    assert!(open.find("begin_open()").unwrap() < open.find("files.open(path)").unwrap());
    assert!(open.contains("self.control_file_job = Some(ControlFile::Open(id))"));
    assert!(!open.contains("Event::Load") && !open.contains("std::fs::"));
    let tick = production
        .split("fn tick(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(
        tick.find("files.poll(&mut self.ui)").unwrap()
            < tick.find("self.control_jobs.opened(id, opened)").unwrap()
    );
    assert!(tick.contains("self.control_file_job.take()"));
    assert_eq!(
        production.matches("self.control_file_job.take()").count(),
        1
    );
    assert!(dispatch.contains("self.control_save_job("));
    assert!(!dispatch.contains("files.save("));
    let save = production
        .split("fn control_save_job(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(save.contains("files.queue_save(&self.ui, target.tab, target.revision, path)"));
    assert!(save.find("files.busy()").unwrap() < save.find(".begin_save(").unwrap());
    assert!(save.find(".checked_add(1)").unwrap() < save.find(".begin_save(").unwrap());
    assert!(save.find(".begin_save(").unwrap() < save.find("files.queue_save(").unwrap());
    assert!(!save.contains("files.save("));
    let session = include_str!("../src/session.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    let handoff = session
        .split("pub(crate) fn poll(")
        .nth(1)
        .unwrap()
        .split("fn finish(")
        .next()
        .unwrap();
    assert!(
        handoff.find("check_revision(&point)").unwrap()
            < handoff
                .find(".save(ui, point.tab, point.revision, path)")
                .unwrap()
    );
    assert!(session.contains("self.pending = Some(Pending::QueuedSave { point, path })"));
    let answer = production
        .split("fn control_dialog_answer(")
        .nth(1)
        .unwrap();
    let answer = answer.split("\n    fn ").next().unwrap();
    assert!(answer.contains("self.closed || self.files.is_none()"));
    assert!(answer.contains("dialog != self.last_dialog_id"));
    assert!(answer.contains(".next(self.ui.editor())?"));
    assert!(answer.contains("current.tab != target.tab"));
    assert!(answer.contains("current.revision != target.revision"));
    assert!(answer.contains("self.discard_close(target)?"));
    assert!(answer.contains("self.cancel_close()"));
    assert!(answer.contains("self.control_close_save_job(target, None)"));
    assert!(answer.contains("self.control_close_save_job(target, Some(path.clone()))"));
    assert!(!answer.contains("Event::"));
    assert!(!answer.contains("close_answer_visible"));
    let conflict = production
        .split("fn control_conflict_answer(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(conflict.contains("dialog != self.last_dialog_id"));
    assert!(conflict.contains("current.tab != target.tab"));
    assert!(conflict.contains("current.revision != target.revision"));
    assert!(conflict.contains("conflict.needs_discard()"));
    assert!(conflict.find(".checked_add(1)").unwrap() < conflict.find(".begin_reload(").unwrap());
    assert!(
        conflict.find(".begin_reload(").unwrap()
            < conflict.find(".answer(self.ui.editor(), discard)").unwrap()
    );
    assert!(conflict.contains("self.start_reload(target, permit)"));
    assert!(!conflict.contains("Event::Reload") && !conflict.contains("close_answer_visible"));
    let cancel = production
        .split("fn cancel_conflict(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(cancel.contains("files.cancel_reload()"));
    assert!(cancel.contains(".reloaded(*id, Ok(ReloadOutcome::Cancelled))"));
    assert!(!dispatch.contains("Event::Discard"));
    assert!(!dispatch.contains("Event::Saved"));
    let path = production
        .split("fn control_path_answer(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(path.contains("dialog != identity.id"));
    assert!(path.contains("target.tab != identity.point.tab"));
    assert!(path.contains("target.revision != identity.point.revision"));
    assert!(
        path.find("check_revision(&identity.point)").unwrap()
            < path.find("self.control_open_job(path)").unwrap()
    );
    assert!(path.contains("self.control_dictionary_job(path)"));
    assert!(path.contains("self.control_save_job(target, Some(path))"));
    assert!(!path.contains("Event::") && !path.contains("files."));
    let dictionary = production
        .split("fn control_dictionary_job(")
        .nth(1)
        .unwrap()
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(dictionary.find("files.busy()").unwrap() < dictionary.find(".checked_add(1)").unwrap());
    assert!(
        dictionary.find(".checked_add(1)").unwrap()
            < dictionary.find("begin_dictionary()").unwrap()
    );
    assert!(
        dictionary.find("begin_dictionary()").unwrap()
            < dictionary.find("files.dictionary(path)").unwrap()
    );
    assert!(dictionary.contains("self.control_file_job = Some(ControlFile::Dictionary(id))"));
    assert!(!dictionary.contains("std::fs::") && !dictionary.contains("Event::"));
    let startup = production.split("pub fn file_window(").nth(1).unwrap();
    assert!(startup.find("Socket::bind").unwrap() < startup.find("prepare_files(").unwrap());
    assert!(startup.contains("window.finish_control(result)"));
    let cli = include_str!("../src/main.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(cli.contains("let mut control = None;"));
    assert!(cli.contains("!literal && arg == \"--control-socket\""));
}
