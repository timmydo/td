//! The EXTERNAL auth handshake every connection completes before its first
//! message.
//!
//! `APPLICATIONS.md` §D fixes the transcript:
//!
//! ```text
//! C: \0AUTH EXTERNAL 31303030      ("1000" hex-encoded)
//! S: OK <32-hex-guid>
//! C: NEGOTIATE_UNIX_FD
//! S: AGREE_UNIX_FD
//! C: BEGIN
//! ```
//!
//! Two properties carry this module doc because they are what a transport
//! would otherwise get wrong.
//!
//! * **A claim is VERIFIED against what the peer believes it is; the
//!   connection is CHARGED to what the kernel says it is.** The two are the
//!   same number today, since every peer shares the session uid, and they part
//!   the day per-app uids land: a sandboxed app in a user namespace believes it
//!   is uid 1000 and sends that, while `SO_PEERCRED` read outside the namespace
//!   reports the mapped uid. Verifying by equality would refuse every jailed
//!   client, and the failure would present as "D-Bus stopped working" rather
//!   than as anything about identity. Recording the CLAIM would be the opposite
//!   error: a jailed app would be indistinguishable from the desktop user to
//!   anything downstream that reads `uid()`. So `uid()` is the credential on
//!   every path into it.
//!
//!   Be exact about what the claim then does, because it is less than it looks:
//!   EXTERNAL ADMITS BY CREDENTIAL. A peer that states an identity that does
//!   not resolve is refused, but it may simply not state one — an empty
//!   `AUTH EXTERNAL` and an empty `DATA` reach the same `accept`, as
//!   dbus-daemon allows and §D blesses. So `resolves` decides whether a STATED
//!   claim costs a retry; it is not a gate on admission, and nothing later
//!   should be built as though a connection's uid had been checked against
//!   something the peer said.
//! * **Nothing here reads a socket.** The peer credential arrives as a
//!   parameter, which is what keeps `getsockopt` — and with it `UNSAFE.md`
//!   surface #10 — out of this landing.

use std::fmt;

/// §D's caps. A line is bounded as it ACCUMULATES rather than once it is
/// complete: a peer that never sends a terminator would otherwise be buffered
/// without limit, which is the same argument that applies the message layer's
/// ceilings in `frame_len` rather than after a frame has arrived.
pub const MAX_LINE: usize = 4096;
pub const MAX_COMMANDS: usize = 16;
pub const GUID_LEN: usize = 32;

/// A server GUID: exactly 32 lowercase hex digits.
///
/// It is a parameter rather than something this module invents, because the
/// broker's identity outlives one connection. A transport can read one out of
/// `/proc/sys/kernel/random/uuid` as an ordinary file, so nothing about it
/// needs a syscall either.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guid<'a>(&'a str);

impl<'a> Guid<'a> {
    pub fn new(text: &'a str) -> Result<Self, AuthError> {
        if text.len() != GUID_LEN {
            return Err(AuthError::BadGuid);
        }
        if !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AuthError::BadGuid);
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &'a str {
        self.0
    }
}

/// Who the peer is, and what identity it may claim.
///
/// `credential` is what `SO_PEERCRED` reported OUTSIDE any namespace the peer
/// is in; `claimable` is the uid that peer sees itself as. They are equal for
/// an unmapped peer, which is every peer today.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    credential: u32,
    claimable: u32,
}

impl PeerIdentity {
    /// A peer that is not remapped: it sees the uid it actually is.
    pub fn unmapped(credential: u32) -> Self {
        Self {
            credential,
            claimable: credential,
        }
    }

    /// A peer inside a user namespace, from its registered jail instance: it
    /// believes it is `claimable`, and the kernel reports `credential`.
    pub fn mapped(credential: u32, claimable: u32) -> Self {
        Self {
            credential,
            claimable,
        }
    }

    /// The uid the broker charges this connection to: what the kernel
    /// reported, never what the peer said.
    pub fn credential(&self) -> u32 {
        self.credential
    }

    /// Does `claimed` resolve to this peer? This is the whole of what
    /// EXTERNAL verifies, and it is deliberately not `==` against
    /// `credential`.
    pub fn resolves(&self, claimed: u32) -> bool {
        claimed == self.claimable
    }
}

/// A failure that ends the connection, as opposed to one the peer may retry.
///
/// The split matters and is not arbitrary. A mechanism this broker does not
/// serve, or an identity that does not resolve, earns `REJECTED` — the peer is
/// allowed to try again, which is how a client that probes `ANONYMOUS` before
/// `EXTERNAL` reaches a working session. What is fatal is STRUCTURAL: a line
/// that never ends, a peer talking around `BEGIN`, a command budget spent. §D
/// lists both under "refused"; the D-Bus specification distinguishes them, and
/// the 16-command cap is what keeps the retryable half bounded.
///
/// A third class sits between them: a command out of its phase is answered
/// `ERROR` and moves nothing, which is the specification's "receive other" in
/// every server state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthError {
    MissingNulPrefix(u8),
    LineTooLong,
    TooManyCommands,
    BareNewline,
    StrayCarriageReturn,
    InteriorNul,
    NonAscii,
    PrematureBegin,
    BeginWithArgument,
    AfterBegin,
    /// The one variant `feed` never produces: a broker whose own GUID is
    /// malformed, which is a configuration error and not a peer's doing.
    BadGuid,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNulPrefix(byte) => {
                write!(f, "a connection must open with a NUL, not {byte:#x}")
            }
            Self::LineTooLong => write!(f, "an auth line ran past {MAX_LINE} bytes"),
            Self::TooManyCommands => write!(f, "more than {MAX_COMMANDS} auth commands"),
            Self::BareNewline => f.write_str("an auth line ended LF without CR"),
            Self::StrayCarriageReturn => f.write_str("a CR that no LF followed"),
            Self::InteriorNul => f.write_str("an auth line carries a NUL"),
            Self::NonAscii => f.write_str("an auth line is not ASCII"),
            Self::PrematureBegin => f.write_str("the peer sent BEGIN before it authenticated"),
            Self::BeginWithArgument => f.write_str("BEGIN takes no argument"),
            Self::AfterBegin => f.write_str("the peer spoke auth after BEGIN"),
            Self::BadGuid => f.write_str("a server GUID is not 32 lowercase hex digits"),
        }
    }
}

/// What one `feed` produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fed {
    /// Bytes to write back to the peer.
    pub reply: Vec<u8>,
    /// How many of the fed bytes the handshake took. Once `begun` is true the
    /// REST ARE THE MESSAGE STREAM and this is what stops them being eaten: a
    /// peer may put `BEGIN\r\n` and its first message in one write, and a
    /// handshake that consumed the whole buffer would swallow the message.
    pub consumed: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Before the leading NUL byte.
    Nul,
    /// Ready for AUTH.
    Auth,
    /// An empty `AUTH EXTERNAL` opened the DATA exchange.
    Data,
    /// Authenticated; waiting for BEGIN or NEGOTIATE_UNIX_FD.
    Ready,
    /// BEGIN seen. Everything after this is messages.
    Begun,
}

pub struct Handshake<'a> {
    phase: Phase,
    identity: PeerIdentity,
    guid: Guid<'a>,
    commands: usize,
    pending: Vec<u8>,
    unix_fd: bool,
    uid: Option<u32>,
    /// The error that ended this handshake, if one has. See `feed`.
    failed: Option<AuthError>,
}

impl<'a> Handshake<'a> {
    pub fn new(identity: PeerIdentity, guid: Guid<'a>) -> Self {
        Self {
            phase: Phase::Nul,
            identity,
            guid,
            commands: 0,
            pending: Vec::new(),
            unix_fd: false,
            uid: None,
            failed: None,
        }
    }

    /// True once `BEGIN` has been read and the connection is a message stream.
    pub fn begun(&self) -> bool {
        self.phase == Phase::Begun
    }

    /// Whether descriptor passing was negotiated. A peer that skipped
    /// `NEGOTIATE_UNIX_FD` is refused any message carrying one, per §D, and
    /// this is what the message layer asks.
    pub fn unix_fd(&self) -> bool {
        self.unix_fd
    }

    /// The uid this connection is charged to, once it has authenticated. This
    /// is the peer credential on every path, never the identity it claimed —
    /// see the module doc, and `GetConnectionUnixUser`, which must answer what
    /// the kernel said.
    pub fn uid(&self) -> Option<u32> {
        self.uid
    }

    /// Feed bytes off the socket. Partial lines are held, so a caller may hand
    /// over whatever one read returned.
    ///
    /// An error discards any reply the same call had already earned — a peer
    /// that puts `AUTH ANONYMOUS` and an unterminated flood in one write never
    /// sees its `REJECTED`. That is correct, since every error here ends the
    /// connection, but it is worth saying: otherwise a transport author
    /// rediscovers it while explaining a client that was never told why.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Fed, AuthError> {
        // Every error here ends the connection, so it LATCHES: a transport that
        // logs and keeps reading must not be able to splice a violated line
        // back into a legal one. Without this, `AUTH EXTERNAL 3130` + bare LF
        // then `3030\r\n` authenticates across the failure, and the line cap is
        // enforced only by the caller's willingness to hang up.
        if let Some(failed) = self.failed {
            return Err(failed);
        }
        let fed = self.scan(bytes);
        if let Err(failed) = fed {
            self.failed = Some(failed);
            self.pending.clear();
        }
        fed
    }

    fn scan(&mut self, bytes: &[u8]) -> Result<Fed, AuthError> {
        if self.phase == Phase::Begun {
            return Err(AuthError::AfterBegin);
        }
        let mut reply = Vec::new();
        let mut at = 0usize;

        if self.phase == Phase::Nul {
            match bytes.first() {
                None => return Ok(Fed { reply, consumed: 0 }),
                Some(0) => {
                    at = 1;
                    self.phase = Phase::Auth;
                }
                Some(other) => return Err(AuthError::MissingNulPrefix(*other)),
            }
        }

        while let Some(&byte) = bytes.get(at) {
            if self.phase == Phase::Begun {
                break;
            }
            // `bytes.get(at)` just succeeded, so `at < bytes.len()` and this
            // cannot saturate. Written this way rather than as a checked add so
            // there is no unreachable error variant to pick wrongly.
            at = at.saturating_add(1);
            let after_cr = self.pending.last() == Some(&b'\r');
            match byte {
                b'\n' => {
                    if !after_cr {
                        return Err(AuthError::BareNewline);
                    }
                    self.pending.pop();
                    let line = std::mem::take(&mut self.pending);
                    reply.extend_from_slice(&self.command(&line)?);
                }
                // A CR is legal only as the first half of CRLF, and it came
                // first, so it is the violation to name.
                _ if after_cr => return Err(AuthError::StrayCarriageReturn),
                0 => return Err(AuthError::InteriorNul),
                _ => {
                    // The terminator is not part of the line, so it does not
                    // count against the cap: §D bounds lines over 4 KiB, and a
                    // CR that pushed a full one over would make the real bound
                    // 4095. `pending` still tops out at `MAX_LINE + 1`.
                    if self.pending.len() >= MAX_LINE && byte != b'\r' {
                        return Err(AuthError::LineTooLong);
                    }
                    self.pending.push(byte);
                }
            }
        }
        Ok(Fed {
            reply,
            consumed: at,
        })
    }

    fn command(&mut self, line: &[u8]) -> Result<Vec<u8>, AuthError> {
        self.commands = self
            .commands
            .checked_add(1)
            .ok_or(AuthError::TooManyCommands)?;
        if self.commands > MAX_COMMANDS {
            return Err(AuthError::TooManyCommands);
        }
        if !line.is_ascii() {
            return Err(AuthError::NonAscii);
        }
        let (verb, rest) = split_word(line);

        // Dispatch is on (phase, command), which is how the specification's
        // server state table reads. A command that is right for another state
        // is not a failed authentication, and answering one with REJECTED is
        // worse than useless: a client that has already authenticated and
        // re-sends AUTH would be told to tear down an attempt that in fact
        // succeeded, and libdbus responds to REJECTED by trying the next
        // mechanism — so the two would trade lines until the command budget
        // closed the connection for a reason neither had anything to do with.
        match verb {
            // The specification TERMINATES on a premature BEGIN rather than
            // inviting a retry, and it is right to: BEGIN changes what the
            // stream means, so a peer that sends one early has desynchronised
            // and there is nothing left to agree about.
            b"BEGIN" => {
                if self.phase != Phase::Ready {
                    return Err(AuthError::PrematureBegin);
                }
                // BEGIN takes no argument, and one that carries one cannot be
                // answered ERROR: the peer that sent it believes the stream has
                // begun and may write a message next, while a server left in
                // auth mode would read those bytes as auth lines. Every BEGIN
                // therefore either starts the stream or ends the connection —
                // the two can never disagree about which one this is.
                if !rest.is_empty() {
                    return Err(AuthError::BeginWithArgument);
                }
                self.phase = Phase::Begun;
                Ok(Vec::new())
            }
            b"AUTH" if self.phase == Phase::Auth => Ok(self.auth(rest)),
            b"DATA" if self.phase == Phase::Data => Ok(self.data(rest)),
            b"NEGOTIATE_UNIX_FD" if self.phase == Phase::Ready && rest.is_empty() => {
                self.unix_fd = true;
                Ok(b"AGREE_UNIX_FD\r\n".to_vec())
            }
            // The peer's ERROR says it did not understand us and CANCEL
            // abandons an attempt in progress. Both are answered REJECTED and
            // both unwind to where AUTH is expected. CANCEL with no attempt to
            // cancel is not one of these — it falls through as "other".
            b"ERROR" => {
                self.reset_attempt();
                Ok(rejected())
            }
            b"CANCEL" if self.phase != Phase::Auth && rest.is_empty() => {
                self.reset_attempt();
                Ok(rejected())
            }
            // "Unknown commands get ERROR without changing state" — §D, and the
            // specification's "receive other" in every server state. That is
            // also where a known command out of its phase lands, and where
            // `BEGIN junk` lands: neither takes an argument, and one that
            // carries one is not the command it resembles. This is the server's
            // ERROR, which is not the client's above.
            _ => Ok(b"ERROR\r\n".to_vec()),
        }
    }

    fn auth(&mut self, rest: &[u8]) -> Vec<u8> {
        let (mechanism, identity) = split_word(rest);
        // A bare AUTH is the peer asking which mechanisms exist.
        if mechanism.is_empty() {
            return rejected();
        }
        // ANONYMOUS and DBUS_COOKIE_SHA1 are named refusals in §D; everything
        // else unknown lands in the same place, since REJECTED lists what IS
        // served rather than judging what was asked for.
        if mechanism != b"EXTERNAL" {
            return rejected();
        }
        if identity.is_empty() {
            // sd-bus's spelling: an empty EXTERNAL opens the DATA exchange.
            self.phase = Phase::Data;
            return b"DATA\r\n".to_vec();
        }
        self.settle(identity)
    }

    fn data(&mut self, rest: &[u8]) -> Vec<u8> {
        // An empty DATA in EXTERNAL means "whoever the credential says I am",
        // which is the case the exchange exists for. It reaches the same
        // `accept` as a stated identity does, so a peer cannot pick which uid
        // the connection records by picking how it spells the handshake.
        if rest.is_empty() {
            return self.accept();
        }
        self.settle(rest)
    }

    /// Decode a hex-encoded identity and check that it resolves.
    fn settle(&mut self, hex: &[u8]) -> Vec<u8> {
        let Some(decoded) = unhex(hex) else {
            return self.reject_identity();
        };
        // The decoded bytes are the identity as TEXT — "1000", not 1000 — and
        // §D refuses a non-numeric one. What this rejects that `parse` would
        // not is a leading sign: "+1000" parses to 1000, and a uid text never
        // carries one. It does not canonicalise — "01000" still authenticates
        // — which is harmless because the claim is compared as a number and
        // then discarded.
        let Some(text) = std::str::from_utf8(&decoded).ok().filter(|text| {
            !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
        }) else {
            return self.reject_identity();
        };
        let Ok(claimed) = text.parse::<u32>() else {
            return self.reject_identity();
        };
        if !self.identity.resolves(claimed) {
            return self.reject_identity();
        }
        self.accept()
    }

    /// Record what the connection is charged to. The claim has bought
    /// admission and is deliberately not kept: `credential` is what the kernel
    /// reported, and it is the answer whichever spelling got here.
    fn accept(&mut self) -> Vec<u8> {
        self.uid = Some(self.identity.credential());
        // Entering `Ready` carries no capability in, as leaving it carries none
        // out. That holds today because `unix_fd` is only ever set in `Ready`
        // and both exits unwind — an argument about the whole machine, which is
        // exactly what a later edit breaks. Here it is a property of `accept`.
        self.unix_fd = false;
        self.phase = Phase::Ready;
        let mut reply = Vec::from("OK ");
        reply.extend_from_slice(self.guid.as_str().as_bytes());
        reply.extend_from_slice(b"\r\n");
        reply
    }

    /// Unwind to where AUTH is expected. The descriptor capability goes with
    /// the identity that asked for it: an attempt that succeeds afterwards has
    /// not negotiated one, and a capability outliving its identity fails in the
    /// permissive direction.
    fn reset_attempt(&mut self) {
        self.phase = Phase::Auth;
        self.uid = None;
        self.unix_fd = false;
    }

    fn reject_identity(&mut self) -> Vec<u8> {
        self.reset_attempt();
        rejected()
    }
}

/// The mechanisms this broker serves, which is the one §D names.
fn rejected() -> Vec<u8> {
    b"REJECTED EXTERNAL\r\n".to_vec()
}

/// Split off the first space-delimited word, returning it and the remainder
/// with its leading spaces removed.
fn split_word(line: &[u8]) -> (&[u8], &[u8]) {
    let end = line.iter().position(|byte| *byte == b' ').unwrap_or(line.len());
    let head = line.get(..end).unwrap_or(&[]);
    let mut tail = line.get(end..).unwrap_or(&[]);
    while tail.first() == Some(&b' ') {
        tail = tail.get(1..).unwrap_or(&[]);
    }
    (head, tail)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.wrapping_sub(b'0')),
        b'a'..=b'f' => Some(byte.wrapping_sub(b'a').wrapping_add(10)),
        b'A'..=b'F' => Some(byte.wrapping_sub(b'A').wrapping_add(10)),
        _ => None,
    }
}

/// Decode an even-length hex string. `None` for anything else, which is what
/// makes a malformed identity a refusal rather than a partial decode.
fn unhex(text: &[u8]) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.chunks(2) {
        let high = hex_nibble(*pair.first()?)?;
        let low = hex_nibble(*pair.get(1)?)?;
        out.push(high.wrapping_mul(16).wrapping_add(low));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUID: &str = "0123456789abcdef0123456789abcdef";

    fn guid() -> Guid<'static> {
        Guid::new(GUID).expect("a valid guid")
    }

    fn handshake() -> Handshake<'static> {
        Handshake::new(PeerIdentity::unmapped(1000), guid())
    }

    fn feed(shake: &mut Handshake<'_>, bytes: &[u8]) -> String {
        let fed = shake.feed(bytes).expect("the handshake accepted the bytes");
        String::from_utf8(fed.reply).expect("replies are ASCII")
    }

    /// §D's transcript, byte for byte.
    #[test]
    fn the_specified_transcript_authenticates() {
        let mut shake = handshake();
        assert_eq!(
            feed(&mut shake, b"\0AUTH EXTERNAL 31303030\r\n"),
            format!("OK {GUID}\r\n")
        );
        assert_eq!(shake.uid(), Some(1000));
        assert_eq!(feed(&mut shake, b"NEGOTIATE_UNIX_FD\r\n"), "AGREE_UNIX_FD\r\n");
        assert!(shake.unix_fd());
        assert_eq!(feed(&mut shake, b"BEGIN\r\n"), "");
        assert!(shake.begun());
    }

    /// A peer may put BEGIN and its first message in ONE write. A handshake
    /// that consumed the whole buffer would swallow the message, and the
    /// failure would look like a client that never spoke.
    #[test]
    fn begin_does_not_eat_the_message_after_it() {
        let mut shake = handshake();
        shake
            .feed(b"\0AUTH EXTERNAL 31303030\r\n")
            .expect("auth accepted");
        let fed = shake
            .feed(b"BEGIN\r\nl\x01\x00\x01rest-of-a-message")
            .expect("begin accepted");
        assert!(shake.begun());
        assert_eq!(fed.consumed, b"BEGIN\r\n".len());
        // ...and speaking auth after BEGIN is the "second BEGIN" refusal.
        assert_eq!(shake.feed(b"BEGIN\r\n"), Err(AuthError::AfterBegin));
    }

    /// A socket delivers whatever it delivers, so a line split across reads —
    /// including one split BETWEEN the CR and the LF — must behave as one.
    #[test]
    fn a_line_split_across_reads_is_still_one_line() {
        let mut shake = handshake();
        let whole = b"\0AUTH EXTERNAL 31303030\r\n";
        for split in 1..whole.len() {
            let mut shake = handshake();
            let head = whole.get(..split).expect("in range");
            let tail = whole.get(split..).expect("in range");
            let mut reply = feed(&mut shake, head);
            reply.push_str(&feed(&mut shake, tail));
            assert_eq!(reply, format!("OK {GUID}\r\n"), "split at {split}");
        }
        assert_eq!(feed(&mut shake, whole), format!("OK {GUID}\r\n"));
    }

    /// A claim is verified against what the peer believes it is, and the
    /// connection is charged to what the kernel says it is. Both halves, and
    /// the property that ties them: the answer cannot depend on which legal
    /// spelling the client chose.
    #[test]
    fn a_mapped_peer_is_verified_by_its_claim_and_charged_to_its_credential() {
        let mapped = PeerIdentity::mapped(100_000, 1000);
        assert_eq!(mapped.credential(), 100_000);

        // Stated identity: admitted, because 1000 is what this peer sees.
        let mut shake = Handshake::new(mapped, guid());
        assert_eq!(
            feed(&mut shake, b"\0AUTH EXTERNAL 31303030\r\n"),
            format!("OK {GUID}\r\n")
        );
        assert_eq!(shake.uid(), Some(100_000));

        // The DATA spelling of the same handshake, and the same answer — a
        // client that could pick between them could pick its own identity.
        let mut shake = Handshake::new(mapped, guid());
        assert_eq!(feed(&mut shake, b"\0AUTH EXTERNAL\r\n"), "DATA\r\n");
        assert_eq!(feed(&mut shake, b"DATA\r\n"), format!("OK {GUID}\r\n"));
        assert_eq!(shake.uid(), Some(100_000));

        // ...and via DATA with the identity stated.
        let mut shake = Handshake::new(mapped, guid());
        feed(&mut shake, b"\0AUTH EXTERNAL\r\n");
        assert_eq!(feed(&mut shake, b"DATA 31303030\r\n"), format!("OK {GUID}\r\n"));
        assert_eq!(shake.uid(), Some(100_000));

        // The same claim against an unmapped peer of that credential fails,
        // which is what makes the resolution load-bearing rather than decorative.
        let mut plain = Handshake::new(PeerIdentity::unmapped(100_000), guid());
        assert_eq!(
            feed(&mut plain, b"\0AUTH EXTERNAL 31303030\r\n"),
            "REJECTED EXTERNAL\r\n"
        );
        assert_eq!(plain.uid(), None);
    }

    /// Empty `AUTH EXTERNAL` opens the DATA exchange, and an empty DATA means
    /// "whoever the credential says I am".
    #[test]
    fn an_empty_external_enters_the_data_exchange() {
        let mut shake = handshake();
        assert_eq!(feed(&mut shake, b"\0AUTH EXTERNAL\r\n"), "DATA\r\n");
        assert_eq!(shake.uid(), None);
        assert_eq!(feed(&mut shake, b"DATA\r\n"), format!("OK {GUID}\r\n"));
        assert_eq!(shake.uid(), Some(1000));

        // ...and a hex identity in DATA is resolved exactly as in AUTH.
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL\r\n");
        assert_eq!(feed(&mut shake, b"DATA 31303030\r\n"), format!("OK {GUID}\r\n"));
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL\r\n");
        assert_eq!(feed(&mut shake, b"DATA 39\r\n"), "REJECTED EXTERNAL\r\n");
    }

    /// The mechanisms §D names, plus the probe order a real client uses.
    #[test]
    fn only_external_is_served_and_a_probe_may_retry() {
        for mechanism in ["ANONYMOUS", "DBUS_COOKIE_SHA1", "GSSAPI", ""] {
            let mut shake = handshake();
            let line = format!("\0AUTH {mechanism}\r\n");
            assert_eq!(
                feed(&mut shake, line.as_bytes()),
                "REJECTED EXTERNAL\r\n",
                "{mechanism} was not refused"
            );
            // REJECTED is retryable, which is the whole point of the split.
            assert_eq!(
                feed(&mut shake, b"AUTH EXTERNAL 31303030\r\n"),
                format!("OK {GUID}\r\n")
            );
        }
    }

    /// An identity that is not hex, not even-length, not text, or not numeric.
    #[test]
    fn a_malformed_identity_is_refused() {
        for identity in [
            "zz",         // not hex
            "313",        // odd length
            "6162",       // "ab" — decodes, but is not numeric
            "2d31",       // "-1"
            // "+1000" — the one `parse::<u32>` would ACCEPT, which is why the
            // digit filter is not redundant with it: a signed spelling is a
            // second text for one uid.
            "2b31303030",
            "00",         // a NUL as text
            "3130303030303030303030303030",  // "10000000000000" — over a u32
        ] {
            let mut shake = handshake();
            let line = format!("\0AUTH EXTERNAL {identity}\r\n");
            assert_eq!(
                feed(&mut shake, line.as_bytes()),
                "REJECTED EXTERNAL\r\n",
                "identity {identity} was accepted"
            );
            assert_eq!(shake.uid(), None);
        }
    }

    /// Unknown commands get ERROR and leave the state alone, so the handshake
    /// they interrupt still completes.
    #[test]
    fn an_unknown_command_does_not_move_the_state() {
        let mut shake = handshake();
        assert_eq!(feed(&mut shake, b"\0WHAT IS THIS\r\n"), "ERROR\r\n");
        assert_eq!(feed(&mut shake, b"AUTH EXTERNAL 31303030\r\n"), format!("OK {GUID}\r\n"));
        assert_eq!(feed(&mut shake, b"NONSENSE\r\n"), "ERROR\r\n");
        // Still authenticated, so BEGIN still works.
        assert_eq!(feed(&mut shake, b"BEGIN\r\n"), "");
        assert!(shake.begun());
    }

    /// CANCEL and ERROR from the peer are answered REJECTED and return it to
    /// where AUTH is expected — dropping the identity it had settled AND the
    /// descriptor capability that identity negotiated.
    #[test]
    fn cancel_and_error_unwind_an_authenticated_connection() {
        for command in ["CANCEL", "ERROR something"] {
            let mut shake = handshake();
            feed(&mut shake, b"\0AUTH EXTERNAL 31303030\r\n");
            feed(&mut shake, b"NEGOTIATE_UNIX_FD\r\n");
            assert_eq!(shake.uid(), Some(1000));
            assert!(shake.unix_fd());

            let line = format!("{command}\r\n");
            assert_eq!(feed(&mut shake, line.as_bytes()), "REJECTED EXTERNAL\r\n");
            assert_eq!(shake.uid(), None);
            assert!(!shake.unix_fd(), "{command} kept the fd capability");

            // The attempt that succeeds next has not negotiated one, and does
            // not inherit it.
            feed(&mut shake, b"AUTH EXTERNAL 31303030\r\n");
            assert_eq!(feed(&mut shake, b"BEGIN\r\n"), "");
            assert!(shake.begun());
            assert!(!shake.unix_fd(), "{command} leaked the fd capability");
        }
    }

    /// A peer cannot reach the message stream without an identity. BEGIN is the
    /// specification's terminate rather than a retry, since it changes what the
    /// stream means; NEGOTIATE_UNIX_FD out of phase is "receive other".
    #[test]
    fn nothing_reaches_the_message_stream_unauthenticated() {
        let mut shake = handshake();
        assert_eq!(shake.feed(b"\0BEGIN\r\n"), Err(AuthError::PrematureBegin));
        assert!(!shake.begun());

        // ...including from inside the DATA exchange.
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL\r\n");
        assert_eq!(shake.feed(b"BEGIN\r\n"), Err(AuthError::PrematureBegin));
        assert!(!shake.begun());

        let mut shake = handshake();
        assert_eq!(feed(&mut shake, b"\0NEGOTIATE_UNIX_FD\r\n"), "ERROR\r\n");
        assert!(!shake.unix_fd());
        assert!(!shake.begun());
    }

    /// A command that is right for another state is not a failed
    /// authentication: it is answered ERROR and the state does not move. The
    /// case that matters is a valid AUTH from a peer that has already
    /// authenticated — REJECTED would tell it to tear down an attempt that
    /// succeeded, and libdbus would answer by trying the next mechanism.
    #[test]
    fn a_command_out_of_its_phase_is_an_error_that_moves_nothing() {
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL 31303030\r\n");
        assert_eq!(feed(&mut shake, b"AUTH EXTERNAL 31303030\r\n"), "ERROR\r\n");
        assert_eq!(shake.uid(), Some(1000));
        assert_eq!(feed(&mut shake, b"DATA 31303030\r\n"), "ERROR\r\n");
        // Still exactly where it was, so BEGIN still works.
        assert_eq!(feed(&mut shake, b"BEGIN\r\n"), "");
        assert!(shake.begun());

        // In the DATA exchange, AUTH is out of phase and DATA is not.
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL\r\n");
        assert_eq!(feed(&mut shake, b"AUTH EXTERNAL 31303030\r\n"), "ERROR\r\n");
        assert_eq!(feed(&mut shake, b"DATA\r\n"), format!("OK {GUID}\r\n"));

        // CANCEL with no attempt to cancel is "other" too, and does not unwind
        // a connection that has nothing to unwind.
        let mut shake = handshake();
        assert_eq!(feed(&mut shake, b"\0CANCEL\r\n"), "ERROR\r\n");
        assert_eq!(
            feed(&mut shake, b"AUTH EXTERNAL 31303030\r\n"),
            format!("OK {GUID}\r\n")
        );

        // NEGOTIATE_UNIX_FD takes no argument, and one that carries one is not
        // the command it resembles. ERROR is safe here because a peer that
        // believes it negotiated descriptor passing and did not is refused a
        // message carrying one — it does not disagree about the BYTE stream.
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL 31303030\r\n");
        assert_eq!(feed(&mut shake, b"NEGOTIATE_UNIX_FD now\r\n"), "ERROR\r\n");
        assert!(!shake.unix_fd());
        // ...and a trailing space is not an argument.
        assert_eq!(feed(&mut shake, b"BEGIN \r\n"), "");
        assert!(shake.begun());
    }

    /// BEGIN is the exception: it is the one command that changes what the
    /// stream MEANS, so it either starts the stream or ends the connection.
    /// Answering `BEGIN now` with ERROR would leave the server reading auth
    /// lines while the peer writes messages.
    #[test]
    fn a_begin_with_an_argument_ends_the_connection() {
        let mut shake = handshake();
        feed(&mut shake, b"\0AUTH EXTERNAL 31303030\r\n");
        assert_eq!(
            shake.feed(b"BEGIN now\r\nl\x01\x00\x01"),
            Err(AuthError::BeginWithArgument)
        );
        assert!(!shake.begun());

        // Out of phase as well as malformed: the phase is named first, since
        // that is the one a peer can reach without being broken.
        let mut shake = handshake();
        assert_eq!(
            shake.feed(b"\0BEGIN now\r\n"),
            Err(AuthError::PrematureBegin)
        );
    }

    /// Every error ends the connection, so it latches. A transport that logs
    /// and keeps reading must not be able to splice a violated line into a
    /// legal one.
    #[test]
    fn a_failure_latches_rather_than_trusting_the_caller_to_hang_up() {
        let mut shake = handshake();
        assert_eq!(
            shake.feed(b"\0AUTH EXTERNAL 3130\n"),
            Err(AuthError::BareNewline)
        );
        // The rest of the identity must not rejoin the half that came before.
        assert_eq!(shake.feed(b"3030\r\n"), Err(AuthError::BareNewline));
        assert_eq!(shake.uid(), None);
        assert!(!shake.begun());

        // ...and the line cap is a cap, not a request: a peer cannot spend the
        // buffer, take the error, and spend it again.
        let mut shake = handshake();
        let mut flood = vec![0u8];
        flood.extend(std::iter::repeat_n(b'A', MAX_LINE + 1));
        assert_eq!(shake.feed(&flood), Err(AuthError::LineTooLong));
        assert_eq!(shake.feed(b"\r\n"), Err(AuthError::LineTooLong));
    }

    /// The structural refusals, which end the connection rather than inviting
    /// a retry.
    #[test]
    fn structural_failures_end_the_connection() {
        let mut shake = handshake();
        assert_eq!(
            shake.feed(b"AUTH EXTERNAL 31303030\r\n"),
            Err(AuthError::MissingNulPrefix(b'A'))
        );

        let mut shake = handshake();
        assert_eq!(shake.feed(b"\0AUTH\n"), Err(AuthError::BareNewline));

        let mut shake = handshake();
        assert_eq!(shake.feed(b"\0AUTH\rX"), Err(AuthError::StrayCarriageReturn));

        let mut shake = handshake();
        assert_eq!(shake.feed(b"\0AUTH\0X\r\n"), Err(AuthError::InteriorNul));

        // Both are fatal, so what this pins is the diagnostic: the CR came
        // first, and naming the NUL would point at the second violation.
        let mut shake = handshake();
        assert_eq!(
            shake.feed(b"\0AUTH\r\0"),
            Err(AuthError::StrayCarriageReturn)
        );

        let mut shake = handshake();
        assert_eq!(shake.feed(b"\0AUTH \xc3\xa9\r\n"), Err(AuthError::NonAscii));

        let mut shake = handshake();
        assert_eq!(shake.feed(b"\0BEGIN\r\n"), Err(AuthError::PrematureBegin));

        // The cap is on the ACCUMULATING line: a peer that never terminates one
        // is cut off rather than buffered without limit.
        let mut shake = handshake();
        let mut flood = vec![0u8];
        flood.extend(std::iter::repeat_n(b'A', MAX_LINE + 1));
        assert_eq!(shake.feed(&flood), Err(AuthError::LineTooLong));

        // ...and it is exactly §D's bound: MAX_LINE bytes of line is a line,
        // and the CRLF that ends it does not count against the cap.
        let mut shake = handshake();
        let mut full = vec![0u8];
        full.extend(std::iter::repeat_n(b'A', MAX_LINE));
        full.extend_from_slice(b"\r\n");
        assert_eq!(feed(&mut shake, &full), "ERROR\r\n");

        // ...and it holds across reads, or a peer would get 4 KiB per read.
        let mut shake = handshake();
        assert!(shake.feed(b"\0").is_ok());
        let chunk = vec![b'A'; 1024];
        let mut hit = false;
        for _ in 0..8 {
            if shake.feed(&chunk).is_err() {
                hit = true;
                break;
            }
        }
        assert!(hit, "an unterminated line was buffered past the cap");
    }

    /// The command budget bounds the retryable half: a peer cannot probe
    /// forever, which is what keeps REJECTED from being free.
    #[test]
    fn the_command_budget_bounds_a_probing_peer() {
        let mut shake = handshake();
        assert!(shake.feed(b"\0").is_ok());
        for _ in 0..MAX_COMMANDS {
            assert!(shake.feed(b"AUTH ANONYMOUS\r\n").is_ok());
        }
        assert_eq!(
            shake.feed(b"AUTH EXTERNAL 31303030\r\n"),
            Err(AuthError::TooManyCommands)
        );
    }

    #[test]
    fn a_guid_is_thirty_two_lowercase_hex_digits() {
        assert!(Guid::new(GUID).is_ok());
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",   // 31
            "0123456789abcdef0123456789abcdef0", // 33
            "0123456789ABCDEF0123456789ABCDEF",  // uppercase
            "0123456789abcdef0123456789abcdeg",  // not hex
        ] {
            assert_eq!(Guid::new(bad), Err(AuthError::BadGuid), "{bad} was accepted");
        }
    }

    /// Every prefix of the transcript, fed byte by byte, must either progress
    /// or fail — never panic. The editor-style sweep the crate's rules ask for.
    #[test]
    fn no_byte_sequence_panics_the_handshake() {
        // Drawn from FRAGMENTS rather than from an alphabet. A byte-level
        // alphabet almost never produces a terminated line — it has to open
        // with the NUL and then reach a CRLF with no bare LF, stray CR or NUL
        // before it — so a sweep built that way dies at byte 0 and never
        // reaches `command`, which is the half that parses peer text and the
        // only half that indexes into it.
        let fragments: &[&[u8]] = &[
            b"AUTH",
            b" ",
            b"EXTERNAL",
            b"ANONYMOUS",
            b"DATA",
            b"BEGIN",
            b"CANCEL",
            b"ERROR",
            b"NEGOTIATE_UNIX_FD",
            b"31303030",
            b"313",
            b"zz",
            b"\r\n",
            b"\r",
            b"\n",
            b"\0",
            b"\xc3\xa9",
            b"",
        ];
        let mut seed = 12345u64;
        let mut parsed = 0usize;
        for _ in 0..2000 {
            let mut shake = handshake();
            // Opening with the NUL, because a connection that does not is
            // refused at byte 0 and tells this test nothing.
            let mut buf = vec![0u8];
            while buf.len() < 64 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let pick = (seed >> 33) as usize % fragments.len();
                buf.extend_from_slice(fragments.get(pick).unwrap_or(&&b""[..]));
            }
            let mut at = 0;
            let mut spoke = false;
            while at < buf.len() {
                let end = (at + 5).min(buf.len());
                let Some(chunk) = buf.get(at..end) else { break };
                match shake.feed(chunk) {
                    Ok(fed) => spoke |= !fed.reply.is_empty(),
                    Err(_) => break,
                }
                if shake.begun() {
                    break;
                }
                at = end;
            }
            if spoke {
                parsed += 1;
            }
        }
        // The floor is what keeps this a parser sweep. Without it the test goes
        // on passing after it has stopped reaching the parser at all, which is
        // how the byte-alphabet version passed while covering nothing.
        assert!(
            parsed > 200,
            "only {parsed} of 2000 streams reached the parser"
        );
    }
}
