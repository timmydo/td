//! Replay of the recorded conversations in `../spec`.
//!
//! The hand-laid corpus in `corpus.rs` proves this crate agrees with the
//! specification as the author read it, which a misreading passes too. These
//! fixtures are the other kind of evidence: bytes a real client and the
//! reference `dbus-daemon` actually exchanged, replayed against this crate with
//! the daemon's own replies as the expected output. Recorded by
//! `../examples/dbus-capture.rs`; provenance in `../spec/README`.
//!
//! This is HOST-SIDE, under `#[cfg(test)]`, and deliberately not in the shipped
//! selftest. `include_str!` in a `cfg`-disabled module is never expanded, so the
//! recipe — which stages `src/*.rs` alone and never `spec/` — compiles this file
//! with the corpus absent. That is the trade: the hand-laid fixtures run on the
//! target because they are small and fixed, while the recordings are host-side
//! because they are neither. Rung 14's routing recordings will be whole method
//! call/reply exchanges, and a `panic=abort`, `opt-level=s` target binary is not
//! where a growing interop corpus belongs.

#[cfg(test)]
mod tests {
    use crate::auth::{Guid, Handshake, PeerIdentity};
    use crate::message;
    use std::collections::BTreeSet;

    /// One committed recording.
    struct Recording {
        file: &'static str,
        text: &'static str,
        /// A member name the CLIENT's bytes must contain. A fixture named for
        /// traffic it does not hold is worse than no fixture: the first version
        /// of the `BecomeMonitor` recording was named, noted and documented as
        /// `AddMatch` and contained none, because `dbus-monitor` uses
        /// `BecomeMonitor` with an empty rule array. Nothing noticed until a
        /// reviewer extracted the ASCII by hand, so now this does.
        covers: &'static str,
    }

    /// Every committed recording. `the_corpus_is_whole` reads `spec/` and
    /// requires this list to name all of it, so a file added and not listed
    /// reds rather than being silently ignored.
    const RECORDINGS: &[Recording] = &[
        Recording {
            file: "libdbus-listnames.conversation",
            text: include_str!("../spec/libdbus-listnames.conversation"),
            covers: "ListNames",
        },
        Recording {
            file: "libdbus-introspect.conversation",
            text: include_str!("../spec/libdbus-introspect.conversation"),
            covers: "Introspect",
        },
        Recording {
            file: "libdbus-addmatch.conversation",
            text: include_str!("../spec/libdbus-addmatch.conversation"),
            covers: "AddMatch",
        },
        Recording {
            file: "libdbus-becomemonitor.conversation",
            text: include_str!("../spec/libdbus-becomemonitor.conversation"),
            covers: "BecomeMonitor",
        },
        Recording {
            file: "sdbus-busctl.conversation",
            text: include_str!("../spec/sdbus-busctl.conversation"),
            covers: "Hello",
        },
    ];

    /// The uid the recordings were made as, used only where a client states no
    /// identity at all (the `DATA` spelling). It is what a claim must resolve
    /// to, never what the connection is charged to — see `peer_for`.
    const RECORDER_UID: u32 = 1001;

    /// A credential no recording contains, deliberately.
    ///
    /// The peer is built MAPPED — claiming the recorded uid, credentialed as
    /// this — so `uid()` has two different numbers to choose between. Built
    /// `unmapped(claimed)` the two would coincide, and a regression that
    /// recorded the CLAIM instead of the CREDENTIAL would replay clean. That is
    /// exactly the bug the previous landing's first review cycle fixed, and a
    /// reviewer of this one showed the corpus was blind to it.
    const SENTINEL_CREDENTIAL: u32 = 424_242;

    /// A GUID no recording contains, for the same reason: the byte-for-byte
    /// replay feeds each daemon's own GUID back in, so it cannot tell an `OK`
    /// line carrying the CONFIGURED guid from one echoing the recorded bytes.
    /// The second replay uses this and requires the difference.
    const SENTINEL_GUID: &str = "00112233445566778899aabbccddeeff";

    /// One read's worth of bytes, in the direction it travelled.
    #[derive(Debug)]
    struct Frame {
        from_client: bool,
        bytes: Vec<u8>,
    }

    struct Conversation {
        name: String,
        frames: Vec<Frame>,
    }

    fn unhex(text: &str) -> Result<Vec<u8>, String> {
        let raw = text.as_bytes();
        if !raw.len().is_multiple_of(2) {
            return Err(format!("odd-length hex, {} digits", raw.len()));
        }
        let mut out = Vec::with_capacity(raw.len() / 2);
        for pair in raw.chunks(2) {
            let text = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
            let byte = u8::from_str_radix(text, 16).map_err(|e| format!("{text}: {e}"))?;
            out.push(byte);
        }
        Ok(out)
    }

    /// The format `dbus-capture` writes, and `spec/README` documents.
    fn parse(file: &str, text: &str) -> Result<Conversation, String> {
        let mut name = None;
        let mut frames = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("name:") {
                name = Some(rest.trim().to_string());
                continue;
            }
            if line.starts_with("note:") {
                continue;
            }
            let mut parts = line.split_whitespace();
            let tag = parts.next().unwrap_or("");
            let hex = parts.next().unwrap_or("");
            if parts.next().is_some() {
                return Err(format!("{file}:{}: more than two fields", number + 1));
            }
            let from_client = match tag {
                "C" => true,
                "S" => false,
                other => return Err(format!("{file}:{}: bad direction {other}", number + 1)),
            };
            let bytes = unhex(hex).map_err(|e| format!("{file}:{}: {e}", number + 1))?;
            if bytes.is_empty() {
                return Err(format!("{file}:{}: an empty frame", number + 1));
            }
            frames.push(Frame { from_client, bytes });
        }
        Ok(Conversation {
            name: name.ok_or_else(|| format!("{file}: no name: line"))?,
            frames,
        })
    }

    fn conversations() -> Vec<Conversation> {
        RECORDINGS
            .iter()
            .map(|r| match parse(r.file, r.text) {
                Ok(conversation) => conversation,
                // A corpus that will not parse is an error, not a smaller
                // corpus: the failure mode to design against is a suite that
                // silently stops testing anything.
                Err(e) => panic!("{e}"),
            })
            .collect()
    }

    /// Split what the daemon sent into its auth replies and its message stream.
    ///
    /// By CONTENT, not by frame: sd-bus pipelines the entire handshake into one
    /// write, so every daemon reply arrives after the client has already sent
    /// `BEGIN`, and a reader that stopped at the client's `BEGIN` frame would
    /// decide the daemon said nothing.
    fn split_server(conversation: &Conversation) -> (Vec<u8>, Vec<u8>) {
        let mut stream = Vec::new();
        for frame in &conversation.frames {
            if !frame.from_client {
                stream.extend_from_slice(&frame.bytes);
            }
        }
        let mut at = 0usize;
        while let Some(rest) = stream.get(at..) {
            let Some(end) = rest.windows(2).position(|w| w == b"\r\n") else {
                break;
            };
            let Some(line) = rest.get(..end) else { break };
            let text = String::from_utf8_lossy(line);
            let is_auth = text.starts_with("OK ")
                || text == "DATA"
                || text.starts_with("DATA ")
                || text == "AGREE_UNIX_FD"
                || text.starts_with("REJECTED")
                || text.starts_with("ERROR");
            if !is_auth {
                break;
            }
            at = at.saturating_add(end).saturating_add(2);
        }
        let auth = stream.get(..at).unwrap_or(&[]).to_vec();
        let messages = stream.get(at..).unwrap_or(&[]).to_vec();
        (auth, messages)
    }

    /// Every byte the client sent, concatenated.
    fn client_stream(conversation: &Conversation) -> Vec<u8> {
        let mut out = Vec::new();
        for frame in &conversation.frames {
            if frame.from_client {
                out.extend_from_slice(&frame.bytes);
            }
        }
        out
    }

    /// The server GUID this conversation's daemon used, read out of its own
    /// `OK` line — so a re-recording against a different daemon still replays.
    fn guid_of(conversation: &Conversation) -> String {
        let (auth, _) = split_server(conversation);
        let text = String::from_utf8_lossy(&auth);
        for line in text.split("\r\n") {
            if let Some(rest) = line.strip_prefix("OK ") {
                return rest.to_string();
            }
        }
        String::new()
    }

    /// The uid the client STATED, if it stated one.
    ///
    /// `None` for the `DATA` spelling, which claims nothing. The leading NUL
    /// matters: libdbus puts it in the same read as its `AUTH` line, so the
    /// first line is `\0AUTH EXTERNAL …` and a bare `strip_prefix` finds
    /// nothing — which is how the first version of this function returned its
    /// fallback for every recording while appearing to read them.
    fn stated_uid(conversation: &Conversation) -> Option<u32> {
        let stream = client_stream(conversation);
        let text = String::from_utf8_lossy(&stream);
        for line in text.split("\r\n") {
            let line = line.trim_start_matches('\0');
            let Some(rest) = line.strip_prefix("AUTH EXTERNAL ") else {
                continue;
            };
            let decoded = unhex(rest.trim()).ok()?;
            let text = String::from_utf8(decoded).ok()?;
            return text.parse().ok();
        }
        None
    }

    /// The peer a recording is replayed as: mapped, so the identity it CLAIMS
    /// and the credential it is CHARGED to are different numbers.
    fn peer_for(conversation: &Conversation) -> PeerIdentity {
        PeerIdentity::mapped(
            SENTINEL_CREDENTIAL,
            stated_uid(conversation).unwrap_or(RECORDER_UID),
        )
    }

    fn guid_for(conversation: &Conversation) -> Guid<'static> {
        let text = guid_of(conversation);
        // Leaked so the Guid can outlive this frame; a test binary is the one
        // place that is free.
        let text: &'static str = Box::leak(text.into_boxed_str());
        Guid::new(text)
            .unwrap_or_else(|e| panic!("{}: recorded guid {text:?}: {e}", conversation.name))
    }

    /// Feed a conversation's client frames through a handshake, returning what
    /// this crate said, whatever bytes the handshake did not consume, and the
    /// identity it settled on.
    fn replay(conversation: &Conversation, guid: Guid<'_>) -> (Vec<u8>, Vec<u8>, Option<u32>) {
        let mut shake = Handshake::new(peer_for(conversation), guid);
        let mut spoken = Vec::new();
        let mut stream = Vec::new();
        for frame in &conversation.frames {
            if !frame.from_client {
                continue;
            }
            if shake.begun() {
                stream.extend_from_slice(&frame.bytes);
                continue;
            }
            let fed = shake.feed(&frame.bytes).unwrap_or_else(|e| {
                panic!("{}: refused a real client's bytes: {e}", conversation.name)
            });
            spoken.extend_from_slice(&fed.reply);
            if let Some(rest) = frame.bytes.get(fed.consumed..) {
                stream.extend_from_slice(rest);
            }
        }
        assert!(
            shake.begun(),
            "{}: the recorded handshake never reached BEGIN",
            conversation.name
        );
        (spoken, stream, shake.uid())
    }

    /// The core claim: fed a real client's bytes, this crate answers with the
    /// reference daemon's bytes.
    #[test]
    fn every_recorded_handshake_answers_as_the_reference_daemon_did() {
        for conversation in conversations() {
            let (spoken, _, uid) = replay(&conversation, guid_for(&conversation));
            let (expected, _) = split_server(&conversation);
            assert!(
                !expected.is_empty(),
                "{}: the recording holds no daemon reply to compare against",
                conversation.name
            );
            assert_eq!(
                String::from_utf8_lossy(&spoken),
                String::from_utf8_lossy(&expected),
                "{}: this crate answered differently from dbus-daemon",
                conversation.name
            );
            // The peer is mapped, so this is the CREDENTIAL, not the uid the
            // client claimed. A regression that recorded the claim would put
            // the recording's own uid here.
            assert_eq!(
                uid,
                Some(SENTINEL_CREDENTIAL),
                "{}: charged to the claimed uid rather than the credential",
                conversation.name
            );
        }
    }

    /// The `OK` line must carry the guid this broker was CONFIGURED with.
    ///
    /// The replay above feeds each daemon's own guid back in, so it cannot tell
    /// that from an implementation that echoed the recorded bytes. Replaying
    /// with a guid no recording contains can.
    #[test]
    fn the_ok_line_carries_the_configured_guid_not_the_recorded_one() {
        let sentinel = Guid::new(SENTINEL_GUID).unwrap_or_else(|e| panic!("{e}"));
        for conversation in conversations() {
            let recorded = guid_of(&conversation);
            assert_ne!(recorded, SENTINEL_GUID, "the sentinel is not a sentinel");
            let (spoken, _, _) = replay(&conversation, sentinel);
            let text = String::from_utf8_lossy(&spoken).to_string();
            assert!(
                text.contains(&format!("OK {SENTINEL_GUID}\r\n")),
                "{}: the OK line did not carry the configured guid: {text:?}",
                conversation.name
            );
            assert!(
                !text.contains(&recorded),
                "{}: the OK line echoed the recorded guid",
                conversation.name
            );
        }
    }

    /// Everything the client sent after `BEGIN` must decode, exactly, leaving
    /// nothing over — and every recording must contribute at least one message,
    /// or a conversation could stop being covered without the suite noticing.
    #[test]
    fn every_recorded_client_message_decodes() {
        for conversation in conversations() {
            let (_, stream, _) = replay(&conversation, guid_for(&conversation));
            let mut decoded = 0usize;
            let mut at = 0usize;
            while at < stream.len() {
                let Some(rest) = stream.get(at..) else { break };
                let (message, used) = message::decode_from_client(rest, 0).unwrap_or_else(|e| {
                    panic!(
                        "{}: a real client's message at byte {at} was refused: {e}",
                        conversation.name
                    )
                });
                // The recorder cannot forward descriptors (no surface #10), so
                // a fixture referencing one would describe bytes it does not
                // have. `decode_from_client(_, 0)` already refuses any message
                // declaring one, so this states the property directly rather
                // than leaving it implied by another error's wording.
                assert_eq!(
                    message.fields.unix_fds.unwrap_or(0),
                    0,
                    "{}: a recorded message declares descriptors the recording cannot carry",
                    conversation.name
                );
                assert!(used > 0, "{}: a zero-length frame", conversation.name);
                at = at.saturating_add(used);
                decoded += 1;
            }
            assert_eq!(at, stream.len(), "{}: trailing bytes", conversation.name);
            assert!(
                decoded > 0,
                "{}: no client message decoded, so this recording covers nothing",
                conversation.name
            );
        }
    }

    /// Every reply the daemon sent must decode too. These are the shapes this
    /// crate has to PRODUCE at rung 14, so decoding them is the cheapest
    /// available check that the encoder has a target to match.
    #[test]
    fn every_recorded_daemon_message_decodes() {
        for conversation in conversations() {
            let (_, stream) = split_server(&conversation);
            let mut decoded = 0usize;
            let mut at = 0usize;
            while at < stream.len() {
                let Some(rest) = stream.get(at..) else { break };
                let (_, used) = message::decode(rest, 0).unwrap_or_else(|e| {
                    panic!(
                        "{}: a real daemon's message at byte {at} was refused: {e}",
                        conversation.name
                    )
                });
                at = at.saturating_add(used);
                decoded += 1;
            }
            assert_eq!(
                at,
                stream.len(),
                "{}: trailing bytes in the daemon's stream",
                conversation.name
            );
            assert!(
                decoded > 0,
                "{}: the daemon's message stream is empty",
                conversation.name
            );
        }
    }

    /// A real client's pipelined `BEGIN` must leave its message behind.
    ///
    /// `every_recorded_client_message_decodes` alone does not hold this: a
    /// handshake that ate the pipelined `Hello` would leave that recording's
    /// LATER frames still decoding, and its floor would still be met. So the
    /// property is asserted where it happens.
    #[test]
    fn a_recorded_pipelined_begin_leaves_its_message() {
        let mut pipelined = 0usize;
        for conversation in conversations() {
            let mut shake = Handshake::new(peer_for(&conversation), guid_for(&conversation));
            for frame in &conversation.frames {
                if !frame.from_client || shake.begun() {
                    continue;
                }
                let fed = shake.feed(&frame.bytes).unwrap_or_else(|e| {
                    panic!("{}: refused a real client's bytes: {e}", conversation.name)
                });
                if !shake.begun() {
                    continue;
                }
                let Some(rest) = frame.bytes.get(fed.consumed..) else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                pipelined += 1;
                let (message, used) = message::decode_from_client(rest, 0).unwrap_or_else(|e| {
                    panic!(
                        "{}: the message pipelined with BEGIN did not decode: {e}",
                        conversation.name
                    )
                });
                assert_eq!(
                    used,
                    rest.len(),
                    "{}: the pipelined message did not use its bytes exactly",
                    conversation.name
                );
                assert_eq!(
                    message.fields.member,
                    Some("Hello"),
                    "{}: the first message a client sends is Hello",
                    conversation.name
                );
            }
        }
        assert!(
            pipelined > 0,
            "no recording pipelines a message with BEGIN, so nothing here holds \
`consumed` to leaving it"
        );
    }

    /// The recordings must cover both spellings of EXTERNAL that real clients
    /// use. libdbus states its uid; sd-bus sends an empty `AUTH EXTERNAL` and
    /// answers the `DATA` challenge — and pipelines the whole handshake into
    /// one write, which is the case a per-line reader gets wrong.
    #[test]
    fn the_recordings_cover_both_spellings_real_clients_use() {
        let mut stated = 0usize;
        let mut challenged = 0usize;
        for conversation in conversations() {
            let stream = client_stream(&conversation);
            let text = String::from_utf8_lossy(&stream);
            if text.contains("AUTH EXTERNAL\r\n") {
                challenged += 1;
            }
            if stated_uid(&conversation).is_some() {
                stated += 1;
            }
        }
        assert!(stated > 0, "no recording states an identity");
        assert!(challenged > 0, "no recording uses the DATA challenge");
    }

    /// Every fixture must contain the traffic its name claims.
    #[test]
    fn every_recording_holds_the_member_it_is_named_for() {
        for (recording, conversation) in RECORDINGS.iter().zip(conversations()) {
            let stream = client_stream(&conversation);
            let needle = recording.covers.as_bytes();
            assert!(
                stream.windows(needle.len()).any(|w| w == needle),
                "{}: does not contain {}",
                recording.file,
                recording.covers
            );
        }
    }

    /// `spec/` and `RECORDINGS` must name the same files.
    ///
    /// Read from the DIRECTORY, because the list is reached only through
    /// `include_str!`: a `.conversation` nobody listed is invisible to every
    /// other test here, and comparing the list against itself would not notice.
    /// This is what `td-txt`'s harness gets from `read_dir` on its corpus.
    #[test]
    fn the_corpus_is_whole() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/spec");
        let mut on_disk = BTreeSet::new();
        let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {dir}: {e}"));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("reading {dir}: {e}"));
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".conversation") {
                on_disk.insert(name);
            }
        }
        let listed: BTreeSet<String> = RECORDINGS.iter().map(|r| r.file.to_string()).collect();
        assert_eq!(
            on_disk, listed,
            "spec/ and RECORDINGS disagree: a file in one and not the other is a \
recording nothing replays"
        );
        assert!(!listed.is_empty(), "no recordings");

        for conversation in conversations() {
            assert!(
                conversation.frames.len() >= 4,
                "{}: too few frames to be a conversation",
                conversation.name
            );
            assert!(
                conversation.frames.iter().any(|f| f.from_client),
                "{}: nothing from the client",
                conversation.name
            );
            assert!(
                conversation.frames.iter().any(|f| !f.from_client),
                "{}: nothing from the daemon",
                conversation.name
            );
        }
    }

    /// The replay must be able to FAIL, at the parser and at the comparison.
    /// A corpus whose runner cannot go red proves nothing, which is the lesson
    /// the fuzz sweep in `auth.rs` learned by reaching the parser zero times.
    #[test]
    fn a_corrupted_recording_is_refused() {
        assert!(parse("x", "name: x\nC zz\n").is_err(), "bad hex accepted");
        assert!(parse("x", "name: x\nC 0\n").is_err(), "odd hex accepted");
        assert!(parse("x", "C 00\n").is_err(), "a nameless file accepted");
        assert!(
            parse("x", "name: x\nX 00\n").is_err(),
            "bad direction accepted"
        );
        assert!(
            parse("x", "name: x\nC 00 00\n").is_err(),
            "three fields accepted"
        );

        let Some(first) = RECORDINGS.first() else {
            panic!("no recordings")
        };
        // The readers must read the file rather than agree with a constant:
        // drop the daemon's frames and the guid has to vanish with them.
        let serverless: String = first
            .text
            .lines()
            .filter(|line| !line.starts_with("S "))
            .collect::<Vec<_>>()
            .join("\n");
        let stripped = parse(first.file, &serverless).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(guid_of(&stripped), "", "a guid survived the daemon's removal");
        assert!(split_server(&stripped).0.is_empty());

        // ...and the golden comparison must discriminate. If a corrupted
        // expectation still matched, the assertion in the replay test above
        // would be decorative.
        let whole = parse(first.file, first.text).unwrap_or_else(|e| panic!("{e}"));
        let (spoken, _, _) = replay(&whole, guid_for(&whole));
        let (expected, _) = split_server(&whole);
        assert_eq!(spoken, expected, "the unmodified replay should match");
        let mut corrupted = expected.clone();
        if let Some(byte) = corrupted.first_mut() {
            *byte ^= 0x20;
        }
        assert_ne!(
            spoken, corrupted,
            "the comparison does not distinguish a corrupted expectation"
        );
    }
}
