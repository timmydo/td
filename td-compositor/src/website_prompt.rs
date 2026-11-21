//! One on-glass choice for the website transaction on the paired channel.
use crate::authority::{
    consent::{Operation, Request, Website},
    Exchange,
};
use crate::secret_client::Attempt;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ticket {
    pub epoch: [u8; 32],
    pub id: u64,
}

fn capacity(_: crate::allocation::Error) -> String {
    String::new()
}

impl Ticket {
    fn call(&self, wire: &mut impl Exchange, op: u8, args: &[u8]) -> Result<Vec<u8>, String> {
        self.send(wire, op, args, || true)?;
        self.receive(wire)
    }
    fn send(
        &self,
        wire: &mut impl Exchange,
        op: u8,
        args: &[u8],
        current: impl FnOnce() -> bool,
    ) -> Result<(), String> {
        let mut buf = [0u8; 106];
        let (prefix, rest) = buf.split_at_mut(34);
        let (header, epoch) = prefix.split_at_mut(2);
        header.copy_from_slice(&[0x2d, 2]);
        epoch.copy_from_slice(&self.epoch);
        let (header, rest) = rest.split_at_mut(9);
        let (verb, ticket) = header.split_at_mut(1);
        verb.copy_from_slice(&[op]);
        ticket.copy_from_slice(&self.id.to_be_bytes());
        rest.get_mut(..args.len())
            .ok_or("oversized website action")?
            .copy_from_slice(args);
        let sent = if current() {
            wire.send(buf.get(..43 + args.len()).ok_or("invalid website frame")?)
        } else {
            Err("website request cancelled".into())
        };
        buf.fill(0);
        std::hint::black_box(&mut buf);
        sent
    }
    fn receive(&self, wire: &mut impl Exchange) -> Result<Vec<u8>, String> {
        let reply = wire.receive()?;
        let Some((prefix, result)) = reply.split_at_checked(33) else {
            return Err("website request expired".into());
        };
        if prefix.first() != Some(&0xad)
            || prefix.get(1..) != Some(self.epoch.as_slice())
            || result.first() != Some(&0)
        {
            return Err("website operation refused or expired".into());
        }
        crate::allocation::copy(result.get(1..).ok_or("missing website result")?).map_err(capacity)
    }
}
fn field<'a>(buf: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let (size, rest) = buf.split_at_checked(2).ok_or("short website label")?;
    let n = u16::from_be_bytes(size.try_into().map_err(|_| "invalid label size")?) as usize;
    let (v, next) = rest.split_at_checked(n).ok_or("invalid website label")?;
    *buf = next;
    Ok(v)
}

#[derive(Debug, Eq, PartialEq)]
struct Offer {
    kind: u8,
    vault: u8,
    site: String,
    account: String,
    revision: [u8; 8],
    keys: Vec<u8>,
}
fn account_label(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > 256 {
        return Err("oversized website account name".into());
    }
    // Only printable ASCII fits the trusted font. A site-supplied string
    // cannot add a line to the trusted instructions.
    if bytes.len() <= 255 && bytes.iter().all(|b| (32..=126).contains(b)) {
        crate::allocation::string(
            std::str::from_utf8(bytes).map_err(|_| "invalid website account")?,
        )
        .map_err(capacity)
    } else {
        crate::allocation::string("(account name unavailable)").map_err(capacity)
    }
}
impl Offer {
    fn decode(kind: u8, mut b: &[u8]) -> Result<Self, String> {
        let [hw, vault, rest @ ..] = b else {
            return Err("short website offer".into());
        };
        b = rest;
        let site = crate::allocation::string(
            std::str::from_utf8(field(&mut b)?).map_err(|_| "invalid website site")?,
        )
        .map_err(capacity)?;
        let account = account_label(field(&mut b)?)?;
        let (stamp, keys) = b.split_at_checked(8).ok_or("missing website attachments")?;
        if *vault > 2
            || *hw != u8::from(!keys.is_empty())
            || keys.len() > 256
            || !keys.windows(2).all(|w| matches!(w,[a,b] if a<b))
        {
            return Err("invalid website attachments".into());
        }
        Ok(Self {
            kind,
            vault: *vault,
            site,
            account,
            revision: stamp.try_into().map_err(|_| "invalid website revision")?,
            keys: crate::allocation::copy(keys).map_err(capacity)?,
        })
    }
    fn choices(&self) -> impl Iterator<Item = Option<u8>> + '_ {
        let software = self.vault == 2 || (self.kind == 1 && self.vault == 1);
        software
            .then_some(None)
            .into_iter()
            .chain(self.keys.iter().copied().map(Some))
    }
}

enum State {
    Idle,
    Offer(Ticket, Offer),
    Event(Ticket, u8),
}
fn status(wire: &mut impl Exchange) -> Result<State, String> {
    let r = wire.exchange(&[0x2d, 1])?;
    if r == [0xad] {
        return Ok(State::Idle);
    }
    let (head, rest) = r.split_at_checked(33).ok_or("invalid website status")?;
    if head.first() != Some(&0xad) || rest.first() != Some(&0) {
        return Ok(State::Idle);
    }
    let epoch = head
        .get(1..)
        .and_then(|b| b.try_into().ok())
        .ok_or("missing website generation")?;
    let [0, body @ ..] = rest else {
        return Ok(State::Idle);
    };
    if body.is_empty() {
        return Ok(State::Idle);
    }
    let (serial, tail) = body.split_at_checked(8).ok_or("short website ticket")?;
    let id = u64::from_be_bytes(serial.try_into().map_err(|_| "invalid website ticket")?);
    let t = Ticket { epoch, id };
    match tail {
        [kind @ 0..=3, b @ ..] => Ok(State::Offer(t, Offer::decode(*kind, b)?)),
        [6, 1..=255] => Ok(State::Event(t, 6)),
        [k @ (7 | 8 | 10)] => Ok(State::Event(t, *k)),
        _ => Err("invalid website state".into()),
    }
}

#[derive(Default)]
pub(crate) struct Client {
    pending: Option<(Ticket, Offer)>,
    run: Option<Interaction>,
}
struct Interaction {
    t: Ticket,
    offer: Offer,
    attempt: Arc<Attempt>,
    choice: usize,
    stage: u8,
    shown: Option<Request>,
    account: Option<([u8; 32], String)>,
    index: u8,
}
impl Drop for Interaction {
    fn drop(&mut self) {
        self.attempt.close_field();
        self.attempt.forget_website_pin();
    }
}
impl Client {
    pub fn start(&mut self, t: Ticket, attempt: Arc<Attempt>) -> Result<(), String> {
        let Some((ticket, offer)) = self.pending.take().filter(|(id, _)| *id == t) else {
            return attempt.notice(crate::attention::Notice::Unavailable);
        };
        self.run = Some(Interaction {
            t: ticket,
            offer,
            attempt,
            choice: 0,
            stage: 0,
            shown: None,
            account: None,
            index: 0,
        });
        Ok(())
    }
    pub fn poll(
        &mut self,
        wire: &mut impl Exchange,
        login: &crate::authority::Login,
        memory: fn() -> Result<(), String>,
    ) -> Result<(), String> {
        let state = status(wire)?;
        if let Some(mut run) = self.run.take() {
            let same = match &state {
                State::Offer(t, _) | State::Event(t, _) => *t == run.t,
                _ => false,
            };
            if same && run.attempt.active() {
                match run.advance(wire, state, memory) {
                    Ok(()) => self.run = Some(run),
                    Err(error) => {
                        eprintln!("td-compositor: website prompt: {error}");
                        let _ = run.t.call(wire, 3, &[]);
                        run.attempt.notice(crate::attention::Notice::Failed)?;
                    }
                }
            } else if same {
                let _ = run.t.call(wire, 3, &[]);
            } else {
                run.attempt.close_website()?;
            }
            login.website(None);
            return Ok(());
        }
        self.pending = match state {
            State::Offer(t, o) if o.kind == 0 => {
                let _ = t.call(wire, 9, &[]);
                None
            }
            State::Offer(t, o) => Some((t, o)),
            _ => None,
        };
        login.website(self.pending.as_ref().map(|(t, _)| *t));
        Ok(())
    }
}
impl Interaction {
    fn authorize(&self, wire: &mut impl Exchange, op: u8, args: &[u8]) -> Result<(), String> {
        self.attempt.claim_website_send()?;
        let sent = self.t.send(wire, op, args, || self.attempt.sending());
        self.attempt.finish_sending();
        sent?;
        self.t.receive(wire).map(|_| ())
    }
    fn selected(&self) -> Result<Option<u8>, String> {
        self.offer
            .choices()
            .nth(self.choice)
            .ok_or_else(|| "no website provider is available".into())
    }
    fn prompt(&self) -> Result<Request, String> {
        let account = self
            .account
            .as_ref()
            .map(|(_, label)| label.as_str())
            .unwrap_or(&self.offer.account);
        Request::new(
            self.t.epoch,
            1000,
            Operation::Website(Website {
                ticket: self.t.id,
                stage: self.stage,
                register: self.offer.kind == 1,
                key: self.selected()?,
                credential: self.account.as_ref().map(|(id, _)| *id),
                site: crate::allocation::string(&self.offer.site).map_err(capacity)?,
                account: crate::allocation::string(account).map_err(capacity)?,
            }),
        )
    }
    fn stage(&mut self, stage: u8) {
        self.stage = stage;
        self.shown = None;
        self.attempt.close_field();
    }
    fn row(&mut self, wire: &mut impl Exchange, index: u8) -> Result<bool, String> {
        let row = self.t.call(wire, 7, &[index])?;
        if row.is_empty() {
            return Ok(false);
        }
        let (id, mut rest) = row.split_at_checked(32).ok_or("short credential id")?;
        let label = account_label(field(&mut rest)?)?;
        let _user = field(&mut rest)?;
        if !rest.is_empty() {
            return Err("invalid account reply".into());
        }
        self.account = Some((id.try_into().map_err(|_| "invalid credential id")?, label));
        self.index = index;
        Ok(true)
    }
    fn advance(
        &mut self,
        wire: &mut impl Exchange,
        state: State,
        memory: fn() -> Result<(), String>,
    ) -> Result<(), String> {
        match state {
            State::Offer(_, offer) if self.stage == 0 && self.offer != offer => {
                self.offer = offer;
                self.choice = 0;
                self.shown = None;
            }
            State::Event(_, 7) if self.stage == 5 => {
                if self.offer.kind == 2 {
                    if !self.row(wire, 0)? {
                        return Err("no account for this website".into());
                    }
                    self.stage(3);
                } else {
                    self.authorize(wire, 8, &[])?;
                    self.stage(6);
                }
            }
            State::Event(_, 6) if self.stage == 4 && self.shown.is_some() => self.stage(2),
            _ => (),
        }
        if matches!(self.stage, 5 | 6) {
            return Ok(());
        }
        if self.shown.is_none() {
            let request = self.prompt()?;
            self.shown = Some(self.attempt.present(request)?);
            if matches!(self.stage, 2 | 7 | 8) {
                memory()?;
                self.attempt
                    .open_field(self.shown.as_ref().ok_or("missing PIN prompt")?)?;
            }
            return Ok(());
        }
        if self.attempt.take_website_next() {
            if self.stage == 0 {
                let count = self.offer.choices().count();
                self.choice = if self.choice + 1 < count {
                    self.choice + 1
                } else {
                    0
                };
            } else if self.stage == 3 && !self.row(wire, self.index.wrapping_add(1))? {
                self.row(wire, 0)?;
            }
            self.shown = None;
            return Ok(());
        }
        if matches!(self.stage, 2 | 7 | 8) {
            let mut pin = [0; 63];
            let Some(n) = self.attempt.take_pin(&mut pin) else {
                return Ok(());
            };
            let result = (|| {
                let p = pin.get(..n).ok_or("invalid PIN extent")?;
                if self.stage != 2 {
                    self.attempt.enrollment_pin(p, self.stage == 8)?;
                }
                if self.stage != 7 {
                    self.t.send(wire, 6, p, || self.attempt.sending())?;
                }
                Ok::<(), String>(())
            })();
            pin.fill(0);
            std::hint::black_box(&mut pin);
            self.attempt.finish_sending();
            result?;
            if self.stage == 7 {
                self.stage(8);
            } else {
                self.t.receive(wire)?;
                self.stage(if self.selected()?.is_some() { 4 } else { 5 });
            }
            return Ok(());
        }
        if !self
            .shown
            .as_ref()
            .is_some_and(|r| self.attempt.approved(r))
        {
            return Ok(());
        }
        match self.stage {
            0 => match self.selected()? {
                Some(index) => {
                    let mut selected = [0; 9];
                    let (stamp, key) = selected.split_at_mut(8);
                    stamp.copy_from_slice(&self.offer.revision);
                    key.copy_from_slice(&[index]);
                    self.authorize(wire, 4, &selected)?;
                    self.stage(4);
                }
                None if self.offer.vault == 1 => self.stage(1),
                None => {
                    self.authorize(wire, 5, &[0])?;
                    self.stage(2);
                }
            },
            1 => {
                self.authorize(wire, 5, &[1])?;
                self.stage(7);
            }
            3 => {
                self.authorize(
                    wire,
                    8,
                    &self.account.as_ref().ok_or("missing website account")?.0,
                )?;
                self.stage(6);
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_passkeys_default_to_td_pass_and_offer_every_connected_yubikey() {
        let mut data = vec![1, 1, 0, 11];
        data.extend_from_slice(b"www.example");
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&9u64.to_be_bytes());
        data.extend_from_slice(&[2, 6]);
        let offer = Offer::decode(1, &data).unwrap();
        assert_eq!(
            offer.choices().collect::<Vec<_>>(),
            [None, Some(2), Some(6)]
        );
        // An unenrolled vault can enroll only on a creation.
        assert_eq!(
            Offer::decode(2, &data).unwrap().choices().next(),
            Some(Some(2))
        );
        data[1] = 2;
        assert_eq!(
            Offer::decode(1, &data).unwrap().choices().next(),
            Some(None)
        );
        data[1] = 0;
        assert_eq!(
            Offer::decode(1, &data)
                .unwrap()
                .choices()
                .collect::<Vec<_>>(),
            [Some(2), Some(6)]
        );
    }
}
