//! Upstream's own auth-script suite, replayed against this crate's handshake.
//!
//! The third kind of evidence, and the only one that carries upstream's
//! INTENT. `corpus.rs` is what the author read the specification to mean, and a
//! misreading writes the code and the fixture alike. `recorded.rs` is what one
//! client and one daemon version actually did, which covers only what a
//! well-behaved peer sends. These files are dbus's own tests for its own
//! server: they state what the reference implementation is REQUIRED to answer,
//! including for peers no client would be.
//!
//! They earned their place immediately. `corpus.rs` asserted that a non-hex
//! identity is `REJECTED`; `invalid-hex-encoding.auth-script` says `ERROR`, and
//! upstream is right — see `auth.rs::settle`.
//!
//! HOST-SIDE, under `#[cfg(test)]`, for the reason `recorded.rs` gives: the
//! recipe stages `src/*.rs` alone, and a `cfg`-disabled module never reaches
//! for `spec/`.

#[cfg(test)]
mod tests {
    use crate::auth::{AuthError, Guid, Handshake, PeerIdentity};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    const SPEC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/auth");

    /// The uid `USERID_HEX` stands for. Upstream substitutes whoever is running
    /// the test; a replay picks one, and the scripts are written not to care.
    const SCRIPT_UID: u32 = 1000;
    const WRONG_UID: u32 = 1001;
    const SCRIPT_USERNAME: &str = "tester";
    const WRONG_USERNAME: &str = "nobody";

    /// What the kernel reports, kept different from every uid any script
    /// claims. A broker that recorded the CLAIM rather than the credential
    /// would pass every one of these scripts — none of them inspects the
    /// number — so `the_connection_is_charged_to_the_credential` inspects it
    /// here, and this constant is what makes the two answers distinguishable.
    const SENTINEL_CREDENTIAL: u32 = 424_242;

    const GUID: &str = "00112233445566778899aabbccddeeff";

    /// Why a script naming a peer the kernel cannot identify is not replayed.
    /// Named once, because `DEVIATIONS` cites it and `run` returns it, and an
    /// exemption that quotes a message nothing produces exempts nothing.
    const ABSENT_CREDENTIAL: &str =
        "NO_CREDENTIALS: this crate has no credential-absent identity";

    /// A script this replay does not require to pass, and why. Everything in
    /// `spec/auth` not named here MUST pass; a name here that is missing from
    /// the directory, or that turns out to pass anyway, reds — an overlay that
    /// silently outlives its reason is how a suite stops measuring anything.
    ///
    /// Every deviation is replayed. An earlier draft had two kinds, and the
    /// second was a hole: the scripts opening with `NO_CREDENTIALS` were
    /// skipped by name, and their exemption was checked against the fact that
    /// the SCRIPT still said NO_CREDENTIALS — a property of the file, which
    /// never changes. Teaching the crate to model an absent credential, which
    /// is the whole content of the excuse, left both skipped forever. So there
    /// is one kind now: a deviation is a script that must FAIL, and `expects`
    /// is what its failure must say. The refusal to model an absent credential
    /// is itself one of those failures, and stops being one the day it stops
    /// being true.
    struct Deviation {
        file: &'static str,
        /// A substring the failure must contain. Requiring only that it FAILS
        /// would let it fail for some other reason — a parse bug, a
        /// disconnect, the right refusal at the wrong step — and report the
        /// divergence as still understood.
        expects: &'static str,
        why: &'static str,
    }

    const DEVIATIONS: &[Deviation] = &[
        Deviation {
            file: "anonymous-server-successful.auth-script",
            expects: "expected OK, got \"REJECTED EXTERNAL\"",
            why: "§D serves EXTERNAL alone. ANONYMOUS is a named refusal: an \
                  unauthenticated peer on the session bus is not a peer this \
                  broker has anything to say to. Expects OK, gets REJECTED.",
        },
        Deviation {
            file: "cookie-sha1.auth-script",
            expects: "expected DATA, got \"REJECTED EXTERNAL\"",
            why: "DBUS_COOKIE_SHA1 is a named refusal in §D: it authenticates \
                  by a shared file under ~/.dbus-keyrings, which is a second \
                  credential store beside the kernel's and weaker than it on \
                  the only transport this broker serves. Expects DATA on the \
                  second attempt, gets REJECTED.",
        },
        Deviation {
            file: "cookie-sha1-username.auth-script",
            expects: "expected DATA, got \"REJECTED EXTERNAL\"",
            why: "As above, by username rather than uid.",
        },
        Deviation {
            file: "external-failed.auth-script",
            expects: ABSENT_CREDENTIAL,
            why: "NO_CREDENTIALS: the peer's identity is unknown to the \
                  kernel. `PeerIdentity` has no such state, because on this \
                  broker's only transport SO_PEERCRED always answers — and a \
                  peer it could not answer for must be refused before a \
                  handshake exists, which is the transport's job (UNSAFE.md \
                  surface #10, not built). Modelling it here as `a credential \
                  nothing can claim` would pass for the reason \
                  external-silly already covers, and measure nothing new.",
        },
        Deviation {
            file: "fail-after-n-attempts.auth-script",
            expects: ABSENT_CREDENTIAL,
            why: "NO_CREDENTIALS, as above. Its own subject — a bounded number \
                  of attempts before the connection ends — this broker has, at \
                  a different bound: §D caps 16 COMMANDS where upstream cuts \
                  off after 6 failed attempts. `MAX_COMMANDS` is what enforces \
                  it and `auth.rs` covers it; adapting the script to td's \
                  number would make it td's fixture wearing upstream's name.",
        },
    ];

    /// Upstream's server states, as `dbus-auth-script.c` names them — with
    /// one deliberate difference, which is the only place this replay reads a
    /// script other than the way upstream's own driver does.
    ///
    /// `AUTHENTICATED_WITH_UNUSED_BYTES` is not a `DBusAuthState`. It appears
    /// in exactly one script and nowhere else in dbus, and
    /// `auth_state_from_string` resolves it by PREFIX match against
    /// `AUTHENTICATED` — so upstream's driver reads extra-bytes' two
    /// `EXPECT_STATE` lines as the same state, and the distinction its author
    /// plainly meant to draw is not enforced by the thing that runs it.
    ///
    /// Read here at face value instead, which is strictly stronger: the first
    /// line requires bytes to be left over and the second requires none. The
    /// cost is that a future script using the spelling loosely would red where
    /// upstream passes, which is a failure that would need reading rather than
    /// believing. That is the right way round; the other one is silent.
    ///
    /// This is the only directive read DIFFERENTLY. It is not the only one
    /// upstream's driver handles and this does not: `WAITING_FOR_MEMORY`,
    /// `HAVE_BYTES_TO_SEND`, `ALLOWED_MECHS` and `WIN_ONLY` are refused rather
    /// than interpreted, which no committed script reaches and which fails
    /// loudly if one ever does.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum State {
        WaitingForInput,
        Authenticated,
        AuthenticatedWithUnusedBytes,
        NeedDisconnect,
    }

    impl State {
        fn parse(text: &str) -> Result<Self, String> {
            match text {
                "WAITING_FOR_INPUT" => Ok(State::WaitingForInput),
                "AUTHENTICATED" => Ok(State::Authenticated),
                "AUTHENTICATED_WITH_UNUSED_BYTES" => {
                    Ok(State::AuthenticatedWithUnusedBytes)
                }
                "NEED_DISCONNECT" => Ok(State::NeedDisconnect),
                other => Err(format!("unknown EXPECT_STATE {other}")),
            }
        }
    }

    enum Step {
        Send(Vec<u8>),
        ExpectCommand(String),
        ExpectState(State),
        ExpectUnused(Vec<u8>),
        ExpectCredentials(bool),
    }

    /// What the kernel is to say this peer is.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Credential {
        /// From ROOT_/SILLY_CREDENTIALS, or the uid the scripts claim.
        Uid(u32),
        /// NO_CREDENTIALS: the kernel knows nothing about this peer. Parsed
        /// like any other directive and refused at the point of REPLAY rather
        /// than of reading, so that a script this crate cannot run is still a
        /// script it fully validates.
        Absent,
    }

    struct Script {
        credential: Credential,
        steps: Vec<Step>,
    }

    /// Decode one SEND/EXPECT_UNUSED argument the way `append_quoted_string`
    /// does: `'…'` runs are taken verbatim, `\r` `\n` `\\` are escapes anywhere,
    /// and an unquoted blank ends the argument — which is how `SEND AUTH`
    /// carries no mechanism.
    fn quoted(text: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut quotes = false;
        let mut backslash = false;
        for byte in text.bytes() {
            if backslash {
                out.push(match byte {
                    b'r' => b'\r',
                    b'n' => b'\n',
                    b'\\' => b'\\',
                    other => return Err(format!("bad escape \\{}", other as char)),
                });
                backslash = false;
            } else if byte == b'\\' {
                backslash = true;
            } else if quotes {
                if byte == b'\'' {
                    quotes = false;
                } else {
                    out.push(byte);
                }
            } else if byte == b'\'' {
                quotes = true;
            } else if byte == b' ' || byte == b'\t' {
                break;
            } else {
                out.push(byte);
            }
        }
        // Upstream leaves the loop with `in_backslash` set and returns TRUE,
        // dropping it. Refusing here would red on a script its own driver
        // passes, which is the one thing a conformance replay must not do — so
        // it is dropped, and the test below pins that it is deliberate.
        Ok(out)
    }

    fn hex(text: &str) -> String {
        text.bytes().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Upstream's substitutions, longest first: `WRONG_USERID_HEX` contains
    /// `USERID_HEX`, and `dbus-auth-script.c` is explicit that the order is
    /// what keeps the two apart.
    fn substitute(line: &str) -> String {
        let mut out = line.to_string();
        for (name, value) in [
            ("WRONG_USERNAME_HEX", hex(WRONG_USERNAME)),
            ("WRONG_USERID_HEX", hex(&WRONG_UID.to_string())),
            ("USERNAME_HEX", hex(SCRIPT_USERNAME)),
            ("USERID_HEX", hex(&SCRIPT_UID.to_string())),
        ] {
            out = out.replace(name, &value);
        }
        out
    }

    fn parse(text: &str) -> Result<Script, String> {
        let mut steps = Vec::new();
        let mut credential = None;
        let mut side = None;
        for (number, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = number + 1;
            let (verb, rest) = match line.split_once(char::is_whitespace) {
                Some((verb, rest)) => (verb, rest.trim_start()),
                None => (line, ""),
            };
            match verb {
                "SERVER" | "CLIENT" => {
                    if side.is_some() {
                        return Err(format!("line {at}: a second side marker"));
                    }
                    side = Some(verb.to_string());
                }
                // Every credentials directive must precede the first SEND, so
                // that reading them up front is the same thing as applying them
                // in order. All fifteen do; a future one that did not would be
                // replayed against the wrong peer, silently.
                "NO_CREDENTIALS" | "ROOT_CREDENTIALS" | "SILLY_CREDENTIALS" => {
                    if steps.iter().any(|step| matches!(step, Step::Send(_))) {
                        return Err(format!("line {at}: {verb} after a SEND"));
                    }
                    if credential.is_some() {
                        return Err(format!("line {at}: a second {verb}"));
                    }
                    credential = Some(match verb {
                        "NO_CREDENTIALS" => Credential::Absent,
                        "ROOT_CREDENTIALS" => Credential::Uid(0),
                        _ => Credential::Uid(4312),
                    });
                }
                // td builds for Linux alone, so the precondition holds. It is
                // matched rather than ignored: an unknown directive must be an
                // error, and that is only true if the known ones are listed.
                "UNIX_ONLY" => {}
                "SEND" => steps.push(Step::Send(quoted(&substitute(rest))?)),
                "EXPECT_COMMAND" => steps.push(Step::ExpectCommand(rest.into())),
                "EXPECT_STATE" => steps.push(Step::ExpectState(State::parse(rest)?)),
                "EXPECT_UNUSED" => {
                    steps.push(Step::ExpectUnused(quoted(&substitute(rest))?))
                }
                "EXPECT_HAVE_NO_CREDENTIALS" => {
                    steps.push(Step::ExpectCredentials(false))
                }
                "EXPECT_HAVE_SOME_CREDENTIALS" => {
                    steps.push(Step::ExpectCredentials(true))
                }
                other => return Err(format!("line {at}: unknown directive {other}")),
            }
        }
        match side.as_deref() {
            Some("SERVER") => {}
            Some(other) => return Err(format!("{other} script, not SERVER")),
            None => return Err("no side marker".into()),
        }
        Ok(Script {
            credential: credential.unwrap_or(Credential::Uid(SCRIPT_UID)),
            steps,
        })
    }

    /// What upstream's driver holds beside the auth itself: the replies not yet
    /// claimed by an EXPECT_COMMAND, and the bytes the handshake did not
    /// consume.
    struct Run<'a> {
        shake: Handshake<'a>,
        pending: Vec<u8>,
        unused: Vec<u8>,
        /// The error that ended it, kept rather than reduced to a flag: which
        /// one it was is the difference between a peer that opened its stream
        /// wrongly and one that misspoke afterwards, and the replay supplies
        /// the byte that tells them apart.
        disconnected: Option<AuthError>,
        /// Whether the connection's leading NUL has been sent. No script sends
        /// one: upstream's `DBusAuth` is handed the stream AFTER the transport
        /// has taken the credentials byte off it, so the scripts start at the
        /// first command. This crate's handshake owns that byte instead — it
        /// is the one place a peer's very first write is checked — so the
        /// replay supplies it, once, ahead of the first SEND.
        opened: bool,
    }

    impl Run<'_> {
        fn state(&self) -> State {
            if self.disconnected.is_some() {
                State::NeedDisconnect
            } else if !self.shake.begun() {
                State::WaitingForInput
            } else if self.unused.is_empty() {
                State::Authenticated
            } else {
                State::AuthenticatedWithUnusedBytes
            }
        }

        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            if self.disconnected.is_some() {
                return Err("SEND after the connection had to end".into());
            }
            // No committed script sends again while bytes are still unclaimed,
            // and what should happen if one did is a real question — the
            // leftovers are past BEGIN, so they belong to the message stream
            // rather than to the handshake. Refused rather than guessed at: a
            // replay that invented an answer here would report a pass for
            // behaviour nobody specified.
            if !self.unused.is_empty() {
                return Err(format!(
                    "SEND while {} bytes are still unclaimed",
                    self.unused.len()
                ));
            }
            let mut input = Vec::new();
            if !self.opened {
                input.push(0);
                self.opened = true;
            }
            input.extend_from_slice(bytes);
            match self.shake.feed(&input) {
                // Every auth error ends the connection, which is upstream's
                // NEED_DISCONNECT and this crate's poison latch. Note this is
                // a RESULT the replay can report, where unclaimed leftovers
                // above are a gap in the replay's model — hence one is a state
                // and the other is a refusal to guess.
                Err(why) => self.disconnected = Some(why),
                Ok(fed) => {
                    self.pending.extend_from_slice(&fed.reply);
                    self.unused = input.get(fed.consumed..).unwrap_or(&[]).to_vec();
                }
            }
            Ok(())
        }

        /// Pop one CRLF line and compare its first word, as `same_first_word`
        /// does — which is what lets `OK` stand for `OK <guid>`.
        ///
        /// The ARGUMENT is deliberately not compared, and that is upstream's
        /// rule rather than a shortcut: `same_first_word` compares the first
        /// words and nothing else, so `EXPECT_COMMAND REJECTED EXTERNAL` would
        /// accept a bare `REJECTED` there too. Tightening it here would red on
        /// a future script that upstream's own driver passes, which is the one
        /// thing a conformance replay must not do.
        fn expect_command(&mut self, want: &str) -> Result<(), String> {
            let end = self
                .pending
                .windows(2)
                .position(|pair| pair == b"\r\n")
                .ok_or_else(|| format!("expected {want}, nothing was sent"))?;
            let line = self.pending.drain(..end + 2).collect::<Vec<u8>>();
            let text = String::from_utf8_lossy(&line).trim_end().to_string();
            let got = text.split(' ').next().unwrap_or("");
            if got != want.split(' ').next().unwrap_or("") {
                return Err(format!("expected {want}, got {text:?}"));
            }
            Ok(())
        }
    }

    /// Replay one script under both peers and rule on the pair.
    ///
    /// Twice, because one peer cannot be both things this needs. FAITHFUL: the
    /// kernel says exactly what the script says it says, so ROOT_CREDENTIALS
    /// means a peer that IS uid 0 rather than a literal in an otherwise
    /// ordinary script, and no script's DATA is reinterpreted to run it.
    /// MAPPED: what the peer claims and what the kernel calls it are different
    /// numbers, so a broker recording whatever the peer claimed does not pass
    /// all fifteen — no script inspects the number.
    ///
    /// BOTH outcomes are taken. Running the faithful one with `?` would return
    /// before the mapped one for every script expected to fail, and the xfails
    /// are exactly those: a regression reachable only through a mapping — say
    /// ANONYMOUS accepted for remapped identities alone — would never have
    /// been replayed, while "twice, and the same verdict" was claimed for it.
    fn run(script: &Script) -> Result<(), String> {
        match script.credential {
            Credential::Uid(stated) => {
                let faithful = run_as(script, PeerIdentity::unmapped(stated), None);
                let mapped = run_as(
                    script,
                    PeerIdentity::mapped(SENTINEL_CREDENTIAL, stated),
                    Some(SENTINEL_CREDENTIAL),
                );
                rule_on(faithful, mapped)
            }
            // Refusing rather than substituting is the point: a peer the
            // kernel cannot name has no `PeerIdentity`, and inventing one
            // would report a pass for behaviour nobody has specified. This is
            // a FAILURE like any other, ruled on by `verdict` against the
            // reason `DEVIATIONS` records — so the day the crate can model
            // such a peer, the excuse stops matching and reds.
            Credential::Absent => Err(ABSENT_CREDENTIAL.into()),
        }
    }

    /// Rule on the pair. The peers differ only in what the kernel is taken to
    /// have said, and no script asks about that, so a disagreement is a
    /// finding in its own right and says which way round it went, rather than
    /// being reported as whichever half was looked at first.
    ///
    /// Separate from `run` because no committed script can make the two
    /// disagree — the difference between them reaches only `credential()`,
    /// which only the mapped replay interrogates — so a disagreement arrives
    /// solely from a regression in the crate, and would otherwise be logic
    /// nothing ever executed. `the_two_peers_are_ruled_on_together` runs it.
    fn rule_on(
        faithful: Result<(), String>,
        mapped: Result<(), String>,
    ) -> Result<(), String> {
        match (faithful, mapped) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(one), Err(two)) if one == two => Err(one),
            (Err(one), Err(two)) => Err(format!(
                "the two peers fail differently: faithful {one:?}, mapped {two:?}"
            )),
            (Ok(()), Err(two)) => Err(format!(
                "held for a faithful peer and not a mapped one: {two}"
            )),
            (Err(one), Ok(())) => Err(format!(
                "held for a mapped peer and not a faithful one: {one}"
            )),
        }
    }

    /// `charged` is the credential the connection must end up recorded
    /// against, when the peer was built so that this can be told apart from
    /// the claim. `None` where the two coincide and the question is unaskable.
    fn run_as(
        script: &Script,
        identity: PeerIdentity,
        charged: Option<u32>,
    ) -> Result<(), String> {
        let guid = Guid::new(GUID).map_err(|e| format!("{e:?}"))?;
        let mut run = Run {
            shake: Handshake::new(identity, guid),
            pending: Vec::new(),
            unused: Vec::new(),
            disconnected: None,
            opened: false,
        };
        for step in &script.steps {
            match step {
                Step::Send(bytes) => {
                    // The driver terminates every SEND, which is why
                    // `'BEGIN\r\nHello'` leaves `Hello\r\n` over.
                    let mut line = bytes.clone();
                    line.extend_from_slice(b"\r\n");
                    run.send(&line)?;
                }
                Step::ExpectCommand(want) => run.expect_command(want)?,
                Step::ExpectState(want) => {
                    let got = run.state();
                    if got != *want {
                        return Err(format!("expected state {want:?}, in {got:?}"));
                    }
                }
                Step::ExpectUnused(want) => {
                    if run.unused != *want {
                        return Err(format!(
                            "expected unused {:?}, have {:?}",
                            String::from_utf8_lossy(want),
                            String::from_utf8_lossy(&run.unused)
                        ));
                    }
                    // Upstream's EXPECT_UNUSED consumes what it checked.
                    // That is why extra-bytes is AUTHENTICATED on the line
                    // after HERE; it is not upstream's reason, which is that
                    // both spellings of that line are one state to its prefix
                    // match. Upstream's own leftover check is the end-of-script
                    // one, which this replay does not implement.
                    run.unused.clear();
                }
                Step::ExpectCredentials(want) => {
                    if run.shake.uid().is_some() != *want {
                        return Err(format!(
                            "expected credentials {want}, uid is {:?}",
                            run.shake.uid()
                        ));
                    }
                }
            }
        }
        // Upstream's two end-of-script checks, which an earlier draft of this
        // comment wrongly called a strengthening: `_dbus_auth_script_run`
        // fails a script that leaves data from the auth unclaimed, and fails
        // an AUTHENTICATED one that leaves unused bytes behind — "scripts must
        // specify explicitly if they are expected", which is what EXPECT_UNUSED
        // is for.
        if !run.pending.is_empty() {
            return Err(format!(
                "unclaimed reply {:?}",
                String::from_utf8_lossy(&run.pending)
            ));
        }
        if run.shake.begun() && !run.unused.is_empty() {
            return Err(format!(
                "authenticated with {:?} unused and no EXPECT_UNUSED for it",
                String::from_utf8_lossy(&run.unused)
            ));
        }
        // td's own invariant over upstream's fixture, asked only of the peer
        // that can answer it. Two things are ruled out at once and the message
        // names both, because from here they look alike: a broker that
        // recorded the claim, and a REPLAY built with one number for the claim
        // and the credential, which could not have told the two apart.
        if let Some(credential) = charged {
            if run.shake.begun() && run.shake.uid() != Some(credential) {
                return Err(format!(
                    "authenticated as {:?}, which is not the credential \
                     {credential}: either the connection was charged to what \
                     the peer claimed, or this peer cannot tell the two apart",
                    run.shake.uid()
                ));
            }
        }
        Ok(())
    }

    /// Every `.auth-script` committed, from the directory rather than a list:
    /// adding a file is enough to have it replayed.
    fn scripts() -> Vec<(String, String)> {
        let mut found = Vec::new();
        let entries = fs::read_dir(SPEC)
            .unwrap_or_else(|e| panic!("{SPEC} is not readable: {e}"));
        for entry in entries {
            let path = entry.unwrap_or_else(|e| panic!("{SPEC}: {e}")).path();
            let name = Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            if !name.ends_with(".auth-script") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{name} is not readable: {e}"));
            found.push((name, text));
        }
        assert!(!found.is_empty(), "{SPEC} holds no scripts");
        found.sort();
        found
    }

    fn deviation(file: &str) -> Option<&'static Deviation> {
        DEVIATIONS.iter().find(|entry| entry.file == file)
    }

    /// Whether one script's outcome is the one `DEVIATIONS` says to expect.
    /// Both directions are findings: a script that stops passing, and a
    /// recorded divergence that stops diverging. The second is the louder,
    /// because it means td changed its mind about something written down.
    fn verdict(file: &str, outcome: Result<(), String>) -> Result<(), String> {
        match (deviation(file), outcome) {
            (None, Ok(())) => Ok(()),
            (None, Err(why)) => Err(format!("{file}: {why}")),
            (Some(entry), Err(why)) => {
                if why.contains(entry.expects) {
                    Ok(())
                } else {
                    Err(format!(
                        "{file} still diverges, but NOT as recorded: the \
                         failure should mention {:?} and says {why:?}",
                        entry.expects
                    ))
                }
            }
            (Some(entry), Ok(())) => Err(format!(
                "{file} now PASSES, and is recorded as a deliberate \
                 divergence: {}",
                entry.why
            )),
        }
    }

    /// The suite itself: every script upstream requires a server to satisfy,
    /// satisfied — except the ones `DEVIATIONS` names and says why.
    #[test]
    fn the_reference_suite_holds_against_this_handshake() {
        let mut ran = 0;
        for (file, text) in scripts() {
            let script = parse(&text)
                .unwrap_or_else(|e| panic!("{file} did not parse: {e}"));
            if let Err(why) = verdict(&file, run(&script)) {
                panic!("{why}");
            }
            ran += 1;
        }
        // Exact, not a floor. A floor lets a fixture be DELETED and the suite
        // stay green while measuring less — which is the same failure as an
        // exemption outliving its reason, arriving from the other side. The
        // number is the selection rule's: 15 SERVER scripts of upstream's 19.
        assert_eq!(scripts().len(), 15, "the committed corpus changed size");
        assert_eq!(ran, 15, "the number of scripts ruled on changed");
    }

    /// A count says how many files are here, not which. Swapping one script
    /// for a byte copy of another keeps the count and the parse and quietly
    /// drops a case; requiring them to be distinct closes that door, since
    /// upstream ships no two identical scripts and never would — each exists
    /// to cover something the others do not. An EDIT to a file is a different
    /// hole, and the per-file SHA-256 list in `spec/README` is what a reader
    /// checks that against; nothing here recomputes it, because this crate has
    /// no hash function and one carried for a test would outlive its use.
    #[test]
    fn no_two_committed_scripts_are_the_same_file() {
        let all = scripts();
        for (index, (file, text)) in all.iter().enumerate() {
            for (other, twin) in all.iter().skip(index + 1) {
                assert_ne!(text, twin, "{file} and {other} are the same bytes");
            }
        }
    }

    /// An overlay entry naming a file that is not there is an exemption for
    /// nothing, and the reason a suite quietly shrinks.
    #[test]
    fn every_deviation_names_a_committed_script() {
        let files: BTreeSet<String> =
            scripts().into_iter().map(|(file, _)| file).collect();
        for entry in DEVIATIONS {
            assert!(
                files.contains(entry.file),
                "{} is exempted and not committed",
                entry.file
            );
            assert!(!entry.why.is_empty(), "{} is exempted with no reason", entry.file);
            // A deviation with nothing to expect is one that passes on any
            // failure at all, which is where this overlay started.
            assert!(
                !entry.expects.is_empty(),
                "{} is exempted without saying how it fails",
                entry.file
            );
        }
    }

    /// The scripts are vendored pristine, so nothing here may need editing to
    /// run. A CLIENT script would be replayed against the wrong role, and an
    /// unknown directive silently skipped would turn a script into a shorter
    /// one that still passes.
    #[test]
    fn every_committed_script_parses_as_a_server_script() {
        for (file, text) in scripts() {
            // Every one of them, including the two nothing replays: a
            // script that stops being read at its third line is a script whose
            // remaining directives could be anything. That was true while
            // NO_CREDENTIALS ended the parse, and it meant an unknown directive
            // in either unsupported file would have gone unnoticed.
            if let Err(why) = parse(&text) {
                panic!("{file}: {why}");
            }
        }
    }

    /// The two `EXPECT_STATE` lines extra-bytes draws a distinction between
    /// are two states here, where upstream's prefix match makes them one.
    #[test]
    fn the_unused_bytes_state_is_read_at_face_value() {
        assert_eq!(State::parse("AUTHENTICATED"), Ok(State::Authenticated));
        assert_eq!(
            State::parse("AUTHENTICATED_WITH_UNUSED_BYTES"),
            Ok(State::AuthenticatedWithUnusedBytes),
            "collapsed into AUTHENTICATED, the way upstream's prefix match \
             would have it"
        );
        // And an unknown state is an error, not a prefix of a known one.
        assert!(State::parse("AUTHENTICATED_SOMEHOW").is_err());
        assert!(State::parse("WAITING_FOR_MEMORY").is_err());
    }

    /// The quoting rules `SEND` arguments are written in, which decide what
    /// bytes reach the handshake. `'BEGIN\r\nHello'` is the whole reason the
    /// `consumed` contract has a fixture at all.
    #[test]
    fn the_quoting_rules_are_upstreams() {
        for (text, want) in [
            ("AUTH", &b"AUTH"[..]),
            ("AUTH EXTERNAL", b"AUTH"),
            ("'AUTH EXTERNAL'", b"AUTH EXTERNAL"),
            ("'BEGIN\\r\\nHello'", b"BEGIN\r\nHello"),
            ("'Hello\\r\\n'", b"Hello\r\n"),
            ("''", b""),
            ("a'b c'd", b"ab cd"),
            ("\\\\", b"\\"),
        ] {
            assert_eq!(quoted(text).as_deref(), Ok(want), "quoting {text}");
        }
        assert!(quoted("\\q").is_err(), "an unknown escape is not an error");
        // Upstream drops it rather than refusing, so this does too.
        assert_eq!(quoted("x\\").as_deref(), Ok(&b"x"[..]));
    }

    /// `WRONG_USERID_HEX` contains `USERID_HEX`; substituted in the other order
    /// it becomes `WRONG_` followed by the RIGHT uid, and cookie-sha1's first
    /// attempt would claim the identity its second one is there to contrast.
    #[test]
    fn the_substitutions_do_not_eat_each_other() {
        assert_eq!(substitute("USERID_HEX"), hex("1000"));
        assert_eq!(substitute("WRONG_USERID_HEX"), hex("1001"));
        assert_eq!(substitute("USERNAME_HEX"), hex(SCRIPT_USERNAME));
        assert_eq!(substitute("WRONG_USERNAME_HEX"), hex(WRONG_USERNAME));
        assert_eq!(substitute("AUTH EXTERNAL USERID_HEX"), "AUTH EXTERNAL 31303030");
    }

    /// The replay must be able to fail. A runner that reported success whatever
    /// the handshake did would satisfy every assertion above.
    #[test]
    fn a_script_the_handshake_does_not_satisfy_is_refused() {
        let cases = [
            // The reply is right and the script asks for another.
            "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND REJECTED\n",
            // The state is right and the script asks for another.
            "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND OK\n\
             EXPECT_STATE AUTHENTICATED\n",
            // Nothing was sent to claim.
            "SERVER\nSEND 'BEGIN'\nEXPECT_COMMAND OK\n",
            // A second SEND while the first one's leftovers are unclaimed.
            "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND OK\n\
             SEND 'BEGIN\\r\\nHello'\nSEND 'more'\n",
            // A reply the script never claimed.
            "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\n",
            // Unused bytes that are not those bytes.
            "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND OK\n\
             SEND 'BEGIN\\r\\nHello'\nEXPECT_UNUSED 'Goodbye\\r\\n'\n",
            // Credentials before there are any.
            "SERVER\nEXPECT_HAVE_SOME_CREDENTIALS\n",
            // Authenticated with bytes left over and nothing claiming them,
            // which is upstream's "scripts must specify explicitly if they
            // are expected".
            "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND OK\n\
             SEND 'BEGIN\\r\\nHello'\n",
        ];
        for text in cases {
            let script = parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert!(run(&script).is_err(), "accepted {text:?}");
        }
        // And the shape those all share must be satisfiable, or they would
        // fail for having been written wrong.
        let good = "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND OK\n\
                    EXPECT_STATE WAITING_FOR_INPUT\nSEND 'BEGIN'\n\
                    EXPECT_STATE AUTHENTICATED\nEXPECT_HAVE_SOME_CREDENTIALS\n";
        let script = parse(good).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run(&script), Ok(()));
    }

    /// The credential check at the end of every replay must be able to fail,
    /// or the sentinel is decoration. Replayed against a peer built the obvious
    /// way — one number for what it claims and what the kernel says — a script
    /// that authenticates must be refused, because the connection is then
    /// charged to a number the peer chose.
    #[test]
    fn a_peer_whose_claim_is_its_credential_is_refused() {
        let text = "SERVER\nSEND 'AUTH EXTERNAL USERID_HEX'\nEXPECT_COMMAND OK\n\
                    SEND 'BEGIN'\nEXPECT_STATE AUTHENTICATED\n";
        let script = parse(text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run(&script), Ok(()));
        let flat = PeerIdentity::unmapped(SCRIPT_UID);
        let refused = run_as(&script, flat, Some(SENTINEL_CREDENTIAL));
        assert!(
            refused.is_err(),
            "a peer charged to its own claim replayed clean: {refused:?}"
        );
    }

    /// ROOT_CREDENTIALS names a peer the kernel calls root, and under the
    /// faithful peer it is one: uid 0 reaches `accept` and is what the
    /// connection is charged to. Replayed only through the mapping, this
    /// script would be external-successful with a different literal in it and
    /// no uid 0 would go anywhere near the handshake.
    #[test]
    fn root_credentials_authenticate_a_peer_the_kernel_calls_root() {
        let text = scripts()
            .into_iter()
            .find(|(file, _)| file == "external-root.auth-script")
            .map(|(_, text)| text)
            .unwrap_or_else(|| panic!("external-root.auth-script is not committed"));
        let script = parse(&text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(script.credential, Credential::Uid(0), "not ROOT_CREDENTIALS");

        let guid = Guid::new(GUID).unwrap_or_else(|e| panic!("{e:?}"));
        let mut root = Handshake::new(PeerIdentity::unmapped(0), guid);
        let fed = root
            .feed(b"\0AUTH EXTERNAL 30\r\n")
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(fed.reply, format!("OK {GUID}\r\n").into_bytes());
        assert_eq!(root.uid(), Some(0), "root authenticated as somebody else");

        // And the whole script, both ways, is what the suite already requires.
        assert_eq!(run(&script), Ok(()));
    }

    /// The overlay must rule in both directions. A ruling that only ever says
    /// "fine" would let every deviation above outlive its reason in silence.
    #[test]
    fn a_deviation_that_stops_diverging_is_a_finding() {
        let deviant = "cookie-sha1.auth-script";
        let ordinary = "cancel.auth-script";
        assert!(deviation(deviant).is_some(), "{deviant} is not exempted");
        assert!(deviation(ordinary).is_none(), "{ordinary} is exempted");

        assert_eq!(verdict(ordinary, Ok(())), Ok(()));
        assert!(verdict(ordinary, Err("no".into())).is_err());

        let ruling = verdict(deviant, Ok(()));
        assert!(ruling.is_err(), "an xfail that passes was accepted");
        assert!(
            ruling.unwrap_err().contains("now PASSES"),
            "the finding does not say what happened"
        );

        // Failing is not enough: it must fail the way the overlay says. A
        // parse bug, a disconnect, or the right refusal at the wrong step
        // would all satisfy "it still fails" while meaning the recorded
        // reason no longer holds.
        let recorded = deviation(deviant).unwrap_or_else(|| panic!("exempted"));
        assert!(!recorded.expects.is_empty(), "an xfail expecting anything");
        assert_eq!(verdict(deviant, Err(recorded.expects.into())), Ok(()));
        let wrong = verdict(deviant, Err("expected DATA, got \"ERROR\"".into()));
        assert!(
            wrong.is_err(),
            "an xfail that fails for another reason was accepted"
        );
        assert!(
            wrong.unwrap_err().contains("NOT as recorded"),
            "the finding does not say what happened"
        );
    }

    /// A script naming a peer the kernel cannot identify is exempted for that
    /// reason and no other, and the exemption quotes the message `run` really
    /// produces. An exemption citing a message nothing emits would never match
    /// and never expire.
    #[test]
    fn the_absent_credential_exemption_quotes_what_the_replay_says() {
        let mut exempted = 0;
        for (file, text) in scripts() {
            let script = parse(&text)
                .unwrap_or_else(|e| panic!("{file} did not parse: {e}"));
            if script.credential != Credential::Absent {
                continue;
            }
            exempted += 1;
            let entry = deviation(&file)
                .unwrap_or_else(|| panic!("{file} needs NO_CREDENTIALS and is \
                                           not exempted"));
            assert_eq!(entry.expects, ABSENT_CREDENTIAL, "{file}");
            // The message is the one the replay actually returns, so the
            // exemption expires with the refusal rather than outliving it.
            let refused = run(&script).unwrap_err();
            assert!(refused.contains(ABSENT_CREDENTIAL), "{file}: {refused}");
        }
        assert_eq!(exempted, 2, "the NO_CREDENTIALS scripts changed in number");
    }

    /// Both replays are ruled on together, and a disagreement between them is
    /// its own finding. No committed script can produce one, so this drives
    /// the ruling directly; without it the arms below are unreachable code
    /// that could say anything.
    #[test]
    fn the_two_peers_are_ruled_on_together() {
        let one = || Err("first".to_string());
        let two = || Err("second".to_string());
        assert_eq!(rule_on(Ok(()), Ok(())), Ok(()));
        // The same failure both ways is the script's failure, and is passed
        // through unchanged so an xfail's recorded substring still matches.
        assert_eq!(rule_on(one(), one()), one());

        let differing = rule_on(one(), two()).unwrap_err();
        assert!(differing.contains("fail differently"), "{differing}");
        assert!(differing.contains("first") && differing.contains("second"));

        let mapped_only = rule_on(Ok(()), two()).unwrap_err();
        assert!(mapped_only.contains("not a mapped one"), "{mapped_only}");
        let faithful_only = rule_on(one(), Ok(())).unwrap_err();
        assert!(faithful_only.contains("not a faithful one"), "{faithful_only}");
    }

    /// A malformed script is an error, not a shorter script that passes.
    #[test]
    fn a_script_that_does_not_parse_is_refused() {
        for text in [
            "SEND 'AUTH'\n",                                  // no side marker
            "CLIENT\nSEND 'AUTH'\n",                          // the wrong role
            "SERVER\nSEND 'AUTH'\nWHAT_IS_THIS foo\n",        // unknown directive
            "SERVER\nEXPECT_STATE NO_SUCH_STATE\n",           // unknown state
            "SERVER\nSERVER\n",                               // two markers
            "SERVER\nSEND 'AUTH'\nROOT_CREDENTIALS\n",        // credentials late
            "SERVER\nROOT_CREDENTIALS\nSILLY_CREDENTIALS\n",  // two of them
            "SERVER\nSEND '\\q'\n",                           // bad escape
        ] {
            assert!(parse(text).is_err(), "parsed {text:?}");
        }
    }

    /// The connection ends where the crate says it does, which is the state
    /// upstream calls NEED_DISCONNECT.
    #[test]
    fn an_auth_error_reaches_need_disconnect() {
        let guid = Guid::new(GUID).unwrap_or_else(|e| panic!("{e:?}"));
        let mut run = Run {
            shake: Handshake::new(PeerIdentity::unmapped(SCRIPT_UID), guid),
            pending: Vec::new(),
            unused: Vec::new(),
            disconnected: None,
            opened: false,
        };
        // A bare newline is §D's BareNewline, and every auth error latches.
        run.send(b"\nAUTH EXTERNAL 31303030\r\n").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run.state(), State::NeedDisconnect);
        // WHICH error, because that is the contrast the supplied NUL creates:
        // through `send` the stream opened correctly and then carried a bare
        // newline, where the same bytes fed to a fresh handshake are a missing
        // NUL. A replay that quietly stopped supplying the byte would turn the
        // first into the second, and both end the connection.
        assert!(
            matches!(run.disconnected, Some(AuthError::BareNewline)),
            "ended with {:?}",
            run.disconnected
        );
        assert!(run.send(b"AUTH\r\n").is_err(), "sent after the latch");
        // Exactly which error, not merely that there was one: a fresh
        // handshake fed a newline first has not reached the line scanner, so
        // this is the missing NUL and never `BareNewline`.
        assert!(matches!(
            Handshake::new(PeerIdentity::unmapped(SCRIPT_UID), guid)
                .feed(b"\nx\r\n"),
            Err(AuthError::MissingNulPrefix(b'\n'))
        ));
    }
}
