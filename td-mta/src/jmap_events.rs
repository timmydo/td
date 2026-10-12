//! Bounded EventSource options and coalesced state frames; no socket or read view.
use crate::{
    bounded::TextBuffer,
    ids::AccountId,
    ports::{Error, Tick},
    sync::{DataState, DataType, IdentityState},
    wire,
};
use td_crypto::Digest;

const TYPES: &[&str] = &["Mailbox", "Thread", "Email", "EmailSubmission", "Identity"];
const IDENTITY_INDEX: usize = 4;
const ALL_TYPES: u8 = (1 << TYPES.len()) - 1;
const _: () = assert!(TYPES.len() == IDENTITY_INDEX + 1 && TYPES.len() < 8);
const _: () = assert!(DataState::WIRE_BYTES <= IdentityState::WIRE_BYTES);
const FRAME_BYTES: usize = 2048;
const QUERY_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    types: u8,
    close_after_state: bool,
    ping_seconds: u64,
}
impl Options {
    /// Parse the query component, without `?`, after route/HTTP validation.
    /// URI-template percent escapes are decoded once; `+` stays literal.
    pub fn parse(query: &str) -> Result<Self, Error> {
        if query.len() > QUERY_BYTES {
            return Err(Error::Capacity);
        }
        let mut types = None;
        let mut close_after_state = None;
        let mut ping_seconds = None;
        let mut decoded = [0; QUERY_BYTES];
        for field in query.split('&') {
            let (key, value) = field.split_once('=').ok_or(Error::Invalid)?;
            let value = crate::http_request::decode_uri_component(value, &mut decoded)?;
            match key {
                "types" if types.is_none() => {
                    let mut mask = 0;
                    if value == "*" {
                        mask = ALL_TYPES;
                    } else {
                        for name in value.split(',') {
                            if name.is_empty() || name.contains('*') {
                                return Err(Error::Invalid);
                            }
                            for (n, known) in TYPES.iter().enumerate() {
                                if name == *known {
                                    mask |= 1u8.checked_shl(n as u32).ok_or(Error::Invalid)?;
                                }
                            }
                        }
                    }
                    types = Some(mask);
                }
                "closeafter" if close_after_state.is_none() => {
                    close_after_state = Some(match value {
                        "state" => true,
                        "no" => false,
                        _ => return Err(Error::Invalid),
                    });
                }
                "ping" if ping_seconds.is_none() => {
                    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(Error::Invalid);
                    }
                    let value = value.parse::<u64>().map_err(|_| Error::Invalid)?;
                    ping_seconds = Some(if value == 0 { 0 } else { value.clamp(1, 300) });
                }
                _ => return Err(Error::Invalid),
            }
        }
        Ok(Self {
            types: types.ok_or(Error::Invalid)?,
            close_after_state: close_after_state.ok_or(Error::Invalid)?,
            ping_seconds: ping_seconds.ok_or(Error::Invalid)?,
        })
    }
    pub fn ping_seconds(self) -> u64 {
        self.ping_seconds
    }
}
/// Passive account state, captured under authorization before passing it here.
/// The fixed roster contains only supported account data types. Omit types not
/// yet available to this credential; no PushSubscription/account state exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    account: AccountId,
    data: [Option<DataState>; 4],
    identity: Option<IdentityState>,
}
impl Snapshot {
    pub fn new(
        account: AccountId,
        data: &[DataState],
        identity: Option<IdentityState>,
    ) -> Result<Self, Error> {
        if data.len() > 4 || identity.is_some_and(|s| s.account != account) {
            return Err(Error::Invalid);
        }
        let mut result = Self {
            account,
            data: [None; 4],
            identity,
        };
        let mut epoch = identity.map(|s| s.epoch);
        for state in data {
            if state.account != account || epoch.is_some_and(|e| e != state.epoch) {
                return Err(Error::Invalid);
            }
            epoch = Some(state.epoch);
            let index = match state.kind {
                DataType::Mailbox => 0,
                DataType::Thread => 1,
                DataType::Email => 2,
                DataType::Submission => 3,
            };
            let slot = result.data.get_mut(index).ok_or(Error::Invalid)?;
            if slot.replace(*state).is_some() {
                return Err(Error::Invalid);
            }
        }
        Ok(result)
    }
    fn token(&self, index: usize, bytes: &mut [u8]) -> Result<Option<usize>, Error> {
        if index == IDENTITY_INDEX {
            return self
                .identity
                .map(|s| s.encode(bytes).map(str::len).map_err(|_| Error::Invalid))
                .transpose();
        }
        self.data
            .get(index)
            .ok_or(Error::Invalid)?
            .map(|s| s.encode(bytes).map(str::len).map_err(|_| Error::Invalid))
            .transpose()
    }
    fn event_id(&self, types: u8) -> Result<[u8; 64], Error> {
        let mut digest = td_crypto::Sha256::try_new().map_err(Error::from)?;
        digest
            .update(b"td-mta EventSource state v1\0")
            .map_err(Error::from)?;
        digest
            .update(self.account.as_bytes())
            .map_err(Error::from)?;
        digest.update(&[types]).map_err(Error::from)?;
        for n in 0..TYPES.len() {
            let mut bytes = [0; IdentityState::WIRE_BYTES];
            let length = self.token(n, &mut bytes)?.unwrap_or(0);
            digest
                .update(&[
                    u8::try_from(n).map_err(|_| Error::Invalid)?,
                    u8::try_from(length).map_err(|_| Error::Invalid)?,
                ])
                .map_err(Error::from)?;
            digest
                .update(bytes.get(..length).ok_or(Error::Invalid)?)
                .map_err(Error::from)?;
        }
        let mut id = [0; 64];
        wire::hex(&digest.finish().map_err(Error::from)?, &mut id).map_err(|_| Error::Invalid)?;
        Ok(id)
    }
}

/// One connection's pending frame and last acknowledged snapshot. The runtime
/// owns authentication/revocation, slot limits, deadlines and transport flush.
/// Frames become acknowledged only after the complete event is flushed.
pub struct Stream {
    options: Options,
    account: AccountId,
    sent: Option<Snapshot>,
    pending: Option<Snapshot>,
    bytes: [u8; FRAME_BYTES],
    length: usize,
    last_sent: Tick,
    last_poll: Tick,
    closed: bool,
}
impl Stream {
    pub fn new(
        options: Options,
        snapshot: Snapshot,
        last_event_id: Option<&str>,
        now: Tick,
    ) -> Result<Self, Error> {
        let id = snapshot.event_id(options.types)?;
        let resumed = last_event_id.is_some_and(|value| value.as_bytes() == id);
        Ok(Self {
            options,
            account: snapshot.account,
            sent: resumed.then_some(snapshot),
            pending: None,
            bytes: [0; FRAME_BYTES],
            length: 0,
            last_sent: now,
            last_poll: now,
            closed: false,
        })
    }
    /// Returns the same immutable pending frame until `acknowledge` is called.
    /// A later snapshot is coalesced on the next call after acknowledgement.
    pub fn prepare(&mut self, snapshot: Snapshot, now: Tick) -> Result<&[u8], Error> {
        if self.closed || now.0 < self.last_poll.0 || snapshot.account != self.account {
            return Err(Error::Invalid);
        }
        self.last_poll = now;
        if self.length != 0 {
            return self.bytes.get(..self.length).ok_or(Error::Invalid);
        }
        let mut selected = 0u8;
        for n in 0..TYPES.len() {
            let bit = 1u8.checked_shl(n as u32).ok_or(Error::Invalid)?;
            if self.options.types & bit == 0 {
                continue;
            }
            let mut current = [0; IdentityState::WIRE_BYTES];
            let current_len = snapshot.token(n, &mut current)?;
            if let Some(length) = current_len {
                let mut previous = [0; IdentityState::WIRE_BYTES];
                let previous_len = self
                    .sent
                    .map(|s| s.token(n, &mut previous))
                    .transpose()?
                    .flatten();
                if previous_len != Some(length) || current.get(..length) != previous.get(..length) {
                    selected |= bit;
                }
            }
        }
        let mut out = TextBuffer::new(&mut self.bytes);
        if selected != 0 {
            let mut account = [0; wire::IdKind::WIRE_BYTES];
            let account =
                wire::encode_id(self.account, &mut account).map_err(|_| Error::Invalid)?;
            let id = snapshot.event_id(self.options.types)?;
            let id = std::str::from_utf8(&id).map_err(|_| Error::Invalid)?;
            out.format(format_args!("event: state\nid: {id}\n"))
                .map_err(|_| Error::Capacity)?;
            out.format(format_args!(
                "data: {{\"@type\":\"StateChange\",\"changed\":{{\"{account}\":{{"
            ))
            .map_err(|_| Error::Capacity)?;
            let mut first = true;
            for (n, name) in TYPES.iter().enumerate() {
                if selected & (1u8.checked_shl(n as u32).ok_or(Error::Invalid)?) == 0 {
                    continue;
                }
                let mut token = [0; IdentityState::WIRE_BYTES];
                let length = snapshot.token(n, &mut token)?.ok_or(Error::Invalid)?;
                let token = std::str::from_utf8(token.get(..length).ok_or(Error::Invalid)?)
                    .map_err(|_| Error::Invalid)?;
                out.format(format_args!(
                    "{}\"{name}\":\"{token}\"",
                    if first { "" } else { "," }
                ))
                .map_err(|_| Error::Capacity)?;
                first = false;
            }
            out.append("}}}\n\n").map_err(|_| Error::Capacity)?;
            self.pending = Some(snapshot);
        } else if self.options.ping_seconds != 0
            && now.0.saturating_sub(self.last_sent.0)
                >= self
                    .options
                    .ping_seconds
                    .checked_mul(1000)
                    .ok_or(Error::Invalid)?
        {
            out.format(format_args!(
                "event: ping\ndata: {{\"interval\":{}}}\n\n",
                self.options.ping_seconds
            ))
            .map_err(|_| Error::Capacity)?;
        }
        self.length = out.len();
        self.bytes.get(..self.length).ok_or(Error::Invalid)
    }
    pub fn acknowledge(&mut self, now: Tick) -> Result<(), Error> {
        if self.closed || self.length == 0 || now.0 < self.last_poll.0 {
            return Err(Error::Invalid);
        }
        self.last_poll = now;
        self.last_sent = now;
        if let Some(snapshot) = self.pending.take() {
            self.sent = Some(snapshot);
            self.closed = self.options.close_after_state;
        }
        self.length = 0;
        Ok(())
    }
    pub fn closed(&self) -> bool {
        self.closed
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{format::Sequence, ids::StoreEpoch};
    fn snapshot(sequence: u64) -> Snapshot {
        let account = AccountId::from_bytes([1; 16]);
        Snapshot::new(
            account,
            &[
                DataState {
                    account,
                    epoch: StoreEpoch::from_bytes([2; 16]),
                    kind: DataType::Email,
                    sequence: Sequence::from_u64(sequence),
                },
                DataState {
                    account,
                    epoch: StoreEpoch::from_bytes([2; 16]),
                    kind: DataType::Mailbox,
                    sequence: Sequence::from_u64(sequence),
                },
            ],
            None,
        )
        .unwrap()
    }
    fn options(types: &str, close: &str, ping: u64) -> Options {
        Options::parse(&format!("types={types}&closeafter={close}&ping={ping}")).unwrap()
    }
    #[test]
    fn uri_template_queries_decode_once_and_bound_options() {
        assert_eq!(options("Email%2CMailbox", "state", 999).ping_seconds(), 300);
        assert_eq!(options("%2A", "no", 0).types, 31);
        assert_eq!(options("Unknown", "no", 1).types, 0);
        assert_eq!(options("Email,Email,Mailbox", "no", 1).types, 5);
        for query in [
            "",
            "types=*&ping=0",
            "types=*&closeafter=no&ping=-1",
            "types=*&closeafter=bad&ping=0",
            "types=Email,,Mailbox&closeafter=no&ping=0",
            "types=Email,*&closeafter=no&ping=0",
            "types=%FF&closeafter=no&ping=0",
            "types=%&closeafter=no&ping=0",
            "types=*&closeafter=no&ping=18446744073709551616",
            "types=*&closeafter=no&ping=0&ping=1",
            "types=*&closeafter=no&ping=0&unknown=1",
        ] {
            assert!(Options::parse(query).is_err(), "{query}");
        }
        assert_eq!(
            Options::parse(&"x".repeat(QUERY_BYTES + 1)),
            Err(Error::Capacity)
        );
    }
    #[test]
    fn state_frames_filter_coalesce_and_wait_for_flush() {
        let mut stream =
            Stream::new(options("Email", "no", 0), snapshot(1), None, Tick(0)).unwrap();
        let first = stream.prepare(snapshot(1), Tick(0)).unwrap().to_vec();
        let text = std::str::from_utf8(&first).unwrap();
        assert!(text.starts_with("event: state\nid: "));
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let json = td_json::parse(data).unwrap();
        assert_eq!(json["@type"].as_str(), Some("StateChange"));
        let account = json["changed"].as_object().unwrap();
        assert_eq!(account.len(), 1);
        assert_eq!(account[0].1.as_object().unwrap().len(), 1);
        assert!(account[0].1["Email"].as_str().is_some());
        assert_eq!(stream.prepare(snapshot(9), Tick(1)).unwrap(), first);
        stream.acknowledge(Tick(2)).unwrap();
        let changed = stream.prepare(snapshot(9), Tick(3)).unwrap().to_vec();
        assert_ne!(changed, first);
        stream.acknowledge(Tick(4)).unwrap();
        assert!(stream.prepare(snapshot(9), Tick(5)).unwrap().is_empty());
        assert!(!stream.closed());
        assert_eq!(stream.acknowledge(Tick(6)), Err(Error::Invalid));
    }
    #[test]
    fn reconnect_id_suppresses_only_matching_snapshot_and_ping_has_no_id() {
        let id = snapshot(1).event_id(ALL_TYPES).unwrap();
        let id = std::str::from_utf8(&id).unwrap();
        let mut stream =
            Stream::new(options("*", "no", 5), snapshot(1), Some(id), Tick(100)).unwrap();
        assert!(stream.prepare(snapshot(1), Tick(5099)).unwrap().is_empty());
        assert_eq!(
            stream.prepare(snapshot(1), Tick(5100)).unwrap(),
            b"event: ping\ndata: {\"interval\":5}\n\n"
        );
        stream.acknowledge(Tick(5500)).unwrap();
        assert!(stream.prepare(snapshot(1), Tick(10499)).unwrap().is_empty());
        assert!(stream
            .prepare(snapshot(2), Tick(10500))
            .unwrap()
            .starts_with(b"event: state\n"));
        let mut reconnect =
            Stream::new(options("*", "no", 0), snapshot(2), Some(id), Tick(0)).unwrap();
        assert!(reconnect
            .prepare(snapshot(2), Tick(0))
            .unwrap()
            .starts_with(b"event: state\n"));
    }
    #[test]
    fn close_after_state_does_not_close_on_ping_or_before_flush() {
        let mut stream =
            Stream::new(options("Email", "state", 1), snapshot(1), None, Tick(0)).unwrap();
        assert!(!stream.prepare(snapshot(1), Tick(0)).unwrap().is_empty());
        assert!(!stream.closed());
        stream.acknowledge(Tick(1)).unwrap();
        assert!(stream.closed());
        assert_eq!(stream.prepare(snapshot(1), Tick(2)), Err(Error::Invalid));
        let mut empty =
            Stream::new(options("Unknown", "state", 1), snapshot(1), None, Tick(0)).unwrap();
        assert!(empty
            .prepare(snapshot(1), Tick(1000))
            .unwrap()
            .starts_with(b"event: ping\n"));
        empty.acknowledge(Tick(1001)).unwrap();
        assert!(!empty.closed());
    }
    #[test]
    fn reconnect_selection_identity_and_unselected_changes() {
        let original = snapshot(1);
        let selected = options("Email", "no", 0);
        let id = original.event_id(selected.types).unwrap();
        let id = std::str::from_utf8(&id).unwrap();
        let mut changed_selection =
            Stream::new(options("Mailbox", "no", 0), original, Some(id), Tick(0)).unwrap();
        assert!(!changed_selection
            .prepare(original, Tick(0))
            .unwrap()
            .is_empty());
        let mut stream = Stream::new(selected, original, Some(id), Tick(0)).unwrap();
        let mut changed = original;
        changed.data[0].as_mut().unwrap().sequence = Sequence::from_u64(2);
        assert!(stream.prepare(changed, Tick(1)).unwrap().is_empty());
        changed.data[2].as_mut().unwrap().sequence = Sequence::from_u64(2);
        let text = std::str::from_utf8(stream.prepare(changed, Tick(2)).unwrap()).unwrap();
        assert!(text.contains("Email"));
        assert!(!text.contains("Mailbox"));
        let identity = IdentityState {
            account: original.account,
            epoch: StoreEpoch::from_bytes([2; 16]),
            digest: [7; 32],
        };
        let full = Snapshot::new(original.account, &[], Some(identity)).unwrap();
        let mut stream = Stream::new(options("Identity", "no", 0), full, None, Tick(0)).unwrap();
        let text = std::str::from_utf8(stream.prepare(full, Tick(0)).unwrap()).unwrap();
        let json =
            td_json::parse(text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap()).unwrap();
        let mut account = [0; wire::IdKind::WIRE_BYTES];
        let account = wire::encode_id(original.account, &mut account).unwrap();
        assert!(json["changed"][account]["Identity"].as_str().is_some());
        assert_eq!(options("Email+Mailbox", "no", 0).types, 0);
        assert_eq!(options("Email%2BMailbox", "no", 0).types, 0);
        let first = original.data[0].unwrap();
        let mut other = original.data[2].unwrap();
        other.epoch = StoreEpoch::from_bytes([9; 16]);
        assert_eq!(
            Snapshot::new(original.account, &[first, other], None),
            Err(Error::Invalid)
        );
    }
    #[test]
    fn account_epoch_duplicate_and_clock_boundaries_refuse() {
        let state = DataState {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
            kind: DataType::Email,
            sequence: Sequence::from_u64(1),
        };
        assert_eq!(
            Snapshot::new(AccountId::from_bytes([3; 16]), &[state], None),
            Err(Error::Invalid)
        );
        assert_eq!(
            Snapshot::new(state.account, &[state, state], None),
            Err(Error::Invalid)
        );
        assert_eq!(
            Snapshot::new(
                state.account,
                &[state],
                Some(IdentityState {
                    account: state.account,
                    epoch: StoreEpoch::from_bytes([4; 16]),
                    digest: [0; 32]
                })
            ),
            Err(Error::Invalid)
        );
        let mut stream = Stream::new(options("*", "no", 0), snapshot(1), None, Tick(10)).unwrap();
        assert_eq!(stream.prepare(snapshot(1), Tick(9)), Err(Error::Invalid));
        let other = Snapshot::new(AccountId::from_bytes([9; 16]), &[], None).unwrap();
        assert_eq!(stream.prepare(other, Tick(11)), Err(Error::Invalid));
        stream.prepare(snapshot(1), Tick(12)).unwrap();
        assert_eq!(stream.acknowledge(Tick(11)), Err(Error::Invalid));
    }
}
