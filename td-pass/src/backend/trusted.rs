//! The root-launched notebook client. No hardware or standalone vault access.

use super::*;
use crate::protocol::KeyKind;
#[path = "../../../td-secret/src/notebook_wire.rs"]
#[allow(
    dead_code,
    reason = "shared field codec; transport is the admitted pass API"
)]
mod wire;
use wire::{field, Bytes as Frame, Reader};

/// How long root may take to answer: an operation's ceiling, ten
/// minutes from root's receipt with the wait for the slot within it, and
/// the vault service's own allowance after it; and, once declined, root's
/// reply saying so.
#[derive(Clone, Copy)]
struct Times {
    answer: Duration,
    decline: Duration,
}

const TIMES: Times = Times {
    answer: Duration::from_secs(650),
    decline: Duration::from_secs(30),
};

/// Root's end of the window's private channel.
trait Link {
    type Message: Message;
    fn send(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn poll(&mut self) -> Result<Option<Self::Message>, String>;
}

trait Message {
    fn bytes(&self) -> &[u8];
}

impl Link for pass::Trusted {
    type Message = pass::TrustedMessage;
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        pass::Trusted::send(self, bytes)
    }
    fn poll(&mut self) -> Result<Option<pass::TrustedMessage>, String> {
        pass::Trusted::poll(self)
    }
}

impl Message for pass::TrustedMessage {
    fn bytes(&self) -> &[u8] {
        pass::TrustedMessage::bytes(self)
    }
}

struct Backend<'a, L: Link> {
    transport: L,
    times: Times,
    answers: &'a Receiver<(Op, Answer)>,
    enrolled: Vec<pass::Key>,
    using: Option<usize>,
    revision: u64,
    copy: Option<(Vec<u8>, Vec<pass::Key>)>,
    /// Root did not answer in time: the channel's replies can no longer
    /// be matched to requests, and the thread ends.
    broken: bool,
}

/// Root's prompt for a step (`7d`): its number, the step's purpose, the
/// key's role, the step, whether it is this computer's TPM, and the
/// enrolled key's fingerprint when the step names one.
fn prompt(bytes: &[u8]) -> Result<([u8; 8], Ask), String> {
    let mut input = Reader::new(bytes);
    input.byte()?;
    let number = input.array()?;
    let purpose = input.byte()?;
    let operation = pass::step_operation(purpose).ok_or("root prompted for an unknown purpose")?;
    let role = match input.byte()? {
        1 => Role::Primary,
        2 => Role::Backup,
        _ => return Err("root prompted for an unknown key role".into()),
    };
    let step = input.byte()?;
    let computer = input.boolean()?;
    // A computer key's enrollment proves its new PIN with an unseal: a
    // second typing of it.
    let enrolling = matches!(purpose, 1 | 2 | 6 | 8);
    let pin = match step {
        0 => None,
        1 if computer && enrolling => Some(PinUse::Proof),
        1 => Some(PinUse::Authorize),
        2 => Some(PinUse::Enroll),
        3 => Some(PinUse::Proof),
        _ => return Err("root prompted for an unknown step".into()),
    };
    let key = if input.boolean()? {
        Some(fingerprint(input.array()?)?)
    } else {
        None
    };
    input.finish()?;
    Ok((
        number,
        Ask {
            operation,
            role,
            key,
            pin,
            computer,
        },
    ))
}

/// A key kind's byte on the service protocol.
fn kind(kind: KeyKind) -> u8 {
    match kind {
        KeyKind::SecurityKey => 0,
        KeyKind::Computer => 1,
    }
}

/// The window's answer to root's prompt `number` (`7c`): proceed, the
/// PIN, or decline, which names no prompt.
fn answer(number: [u8; 8], answer: Answer) -> Result<Frame, String> {
    let (kind, pin) = match &answer {
        Answer::Proceed => (0, None),
        Answer::Pin(pin) => (1, Some(pin.as_slice())),
        Answer::Decline => (2, None),
    };
    let size = pin.map_or(0, |pin| 4 + pin.len());
    // Reserved whole, so no reallocation leaves a copy of the PIN behind.
    let mut frame = Frame(td_alloc::with_capacity(10 + size).map_err(|_| ALLOCATION)?);
    let head = [[0x7c].as_slice(), &number, &[kind]];
    for part in head {
        td_alloc::extend_from_slice(&mut frame.0, part).map_err(|_| ALLOCATION)?;
    }
    if let Some(pin) = pin {
        field(&mut frame, pin)?;
    }
    Ok(frame)
}

const ALLOCATION: &str = "the notebook window could not allocate a message";

/// A key's fingerprint in hex, as standalone mode shows it.
fn fingerprint(key: [u8; 4]) -> Result<String, String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = td_alloc::with_capacity(8).map_err(|_| ALLOCATION)?;
    for byte in key {
        for nibble in [byte >> 4, byte & 15] {
            let digit = HEX.get(usize::from(nibble)).copied().unwrap_or(b'0');
            td_alloc::push(&mut text, digit).map_err(|_| ALLOCATION)?;
        }
    }
    String::from_utf8(text).map_err(|_| ALLOCATION.into())
}

impl<L: Link> Backend<'_, L> {
    /// One request and root's reply. Root's prompts for a hardware step
    /// go to the window as the standalone prompt's, and the window's
    /// answers back; abandoning the operation declines it, and the
    /// reply root then sends, saying so, ends the exchange.
    fn exchange(
        &mut self,
        frame: Frame,
        op: Op,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<L::Message, String> {
        match self.exchange_framed(frame, op, cancel, replies) {
            Ok(reply) => reply,
            Err(broken) => {
                self.broken = true;
                Err(broken)
            }
        }
    }

    /// The exchange's reply, root's refusal as the inner error; the outer
    /// one is the channel's, after which no reply can be trusted to
    /// answer its request.
    fn exchange_framed(
        &mut self,
        frame: Frame,
        op: Op,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<Result<L::Message, String>, String> {
        self.transport.send(&frame.0)?;
        let mut end = Instant::now()
            .checked_add(self.times.answer)
            .ok_or("notebook deadline overflow")?;
        let mut asked: Option<[u8; 8]> = None;
        let mut declined = false;
        loop {
            if Instant::now() >= end {
                return Err(
                    "the notebook service stopped answering; close and reopen the window".into(),
                );
            }
            if cancel.cancelled() && !declined {
                declined = true;
                asked = None;
                end = Instant::now()
                    .checked_add(self.times.decline)
                    .ok_or("notebook deadline overflow")?;
                self.transport.send(&answer([0; 8], Answer::Decline)?.0)?;
            }
            // Only the asked operation's answer counts; a late one is
            // dropped, clearing any PIN it carried.
            while let Ok((answered, reply)) = self.answers.try_recv() {
                if answered != op || declined {
                    continue;
                }
                if let Some(number) = asked.take() {
                    declined = matches!(reply, Answer::Decline);
                    self.transport.send(&answer(number, reply)?.0)?;
                }
            }
            if let Some(message) = self.transport.poll()? {
                let mut input = Reader::new(message.bytes());
                match input.byte()? {
                    0x7d => {
                        let (number, ask) = prompt(message.bytes())?;
                        if !declined {
                            asked = Some(number);
                            replies
                                .send(Reply::Ask { op, ask })
                                .map_err(|_| "notebook window closed")?;
                        }
                    }
                    0 => return Ok(Ok(message)),
                    1 => {
                        let error = td_alloc::copy(input.text(4096)?.as_bytes())
                            .map_err(|_| ALLOCATION)
                            .and_then(|bytes| String::from_utf8(bytes).map_err(|_| ALLOCATION))?;
                        input.finish()?;
                        return Ok(Err(error));
                    }
                    _ => return Err("invalid trusted notebook reply".into()),
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn call(
        &mut self,
        bytes: Vec<u8>,
        op: Op,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<L::Message, String> {
        self.exchange(Frame(bytes), op, cancel, replies)
    }
    fn keys(
        &mut self,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<Option<Vec<KeyLabel>>, String> {
        let response = self.call(vec![1], 0, cancel, replies)?;
        let mut input = success(&response)?;
        let count = input.byte()?;
        if count > 8 {
            return Err("too many notebook keys".into());
        }
        let mut keys = Vec::new();
        // Each key's role, a computer's with its high nibble set.
        for _ in 0..count {
            let byte = input.byte()?;
            let role = match byte & 0x0f {
                1 => pass::KeyRole::Primary,
                2 => pass::KeyRole::Backup,
                _ => return Err("invalid notebook key role".into()),
            };
            let kind = match byte >> 4 {
                0 => pass::KeyKind::SecurityKey,
                1 => pass::KeyKind::Computer,
                _ => return Err("invalid notebook key kind".into()),
            };
            keys.push(pass::Key::from_parts(role, kind, input.field(1024)?)?);
        }
        let using = input.field(1024)?;
        self.using = if using.is_empty() {
            None
        } else {
            Some(
                keys.iter()
                    .position(|key| key.credential() == using)
                    .ok_or("session key is not enrolled")?,
            )
        };
        input.finish()?;
        self.enrolled = keys;
        Ok((count != 0).then(|| self.enrolled.iter().map(super::label).collect()))
    }
    fn entries(
        &mut self,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<Vec<Item>, String> {
        let response = self.call(vec![4], 0, cancel, replies)?;
        let mut input = success(&response)?;
        self.revision = input.number()?;
        let count = u32::from_be_bytes(input.array()?) as usize;
        if count > crate::protocol::MAX_ENTRIES {
            return Err("too many notebook entries".into());
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(Item {
                id: input.array()?,
                revision: input.number()?,
                title: Text::new(input.text(crate::protocol::MAX_TITLE)?.to_owned()),
            });
        }
        input.finish()?;
        Ok(entries)
    }
    fn listing(&self) -> Keys {
        Keys {
            labels: self.enrolled.iter().map(super::label).collect(),
            using: self.using,
        }
    }
    fn unlocked(
        &mut self,
        op: Op,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<Reply, String> {
        self.keys(cancel, replies)?;
        let entries = self.entries(cancel, replies)?;
        Ok(Reply::Unlocked {
            op,
            entries,
            keys: self.listing(),
        })
    }
    fn command(
        &mut self,
        command: Command,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<Reply, String> {
        match command {
            Command::Open => Ok(Reply::Opened {
                keys: self.keys(cancel, replies)?,
            }),
            Command::AcceptSwap => {
                Err("trusted notebook mode does not accept storage-backed swap".into())
            }
            Command::Create {
                op,
                primary,
                backup,
            } => {
                let request = vec![
                    2,
                    u8::from(backup.is_some()),
                    kind(primary),
                    backup.map_or(0, kind),
                ];
                let response = self.call(request, op, cancel, replies)?;
                success(&response)?.finish()?;
                self.using = Some(0);
                self.unlocked(op, cancel, replies)
            }
            Command::Unlock { op, key } => {
                let credential = self
                    .enrolled
                    .get(key)
                    .ok_or("key is no longer listed")?
                    .credential();
                let mut request = Frame(vec![3]);
                field(&mut request, credential)?;
                let response = self.exchange(request, op, cancel, replies)?;
                success(&response)?.finish()?;
                self.using = Some(key);
                self.unlocked(op, cancel, replies)
            }
            Command::BindMail {
                op,
                id,
                base,
                binding,
            } => {
                let mut request = Frame(vec![14]);
                request.extend(&id)?;
                request.extend(&base.to_be_bytes())?;
                request.push(1)?;
                field(&mut request, binding.account().as_bytes())?;
                field(&mut request, binding.server().as_bytes())?;
                field(&mut request, binding.username().as_bytes())?;
                let response = self.exchange(request, op, cancel, replies)?;
                let mut input = success(&response)?;
                let revision = input.number()?;
                input.finish()?;
                Ok(Reply::MailBound { op, id, revision })
            }
            Command::Read { id } => {
                let mut request = Frame(vec![5]);
                request.extend(&id)?;
                let response = self.exchange(request, 0, cancel, replies)?;
                let mut input = success(&response)?;
                let revision = input.number()?;
                let title = Text::new(input.text(crate::protocol::MAX_TITLE)?.to_owned());
                let body = Bytes::copy(input.field(crate::protocol::MAX_BODY)?);
                let password = Text::new(input.text(4096)?.to_owned());
                let count = input.byte()?;
                if count > 16 {
                    return Err("too many notebook bindings".into());
                }
                for _ in 0..count {
                    let _ = input.text(256)?;
                    let _ = input.text(2048)?;
                    let _ = input.text(1024)?;
                }
                input.finish()?;
                Ok(Reply::Entry {
                    id,
                    revision,
                    title,
                    body,
                    password,
                })
            }
            Command::Apply { op, change } => {
                let mut request = Frame(vec![6]);
                match change {
                    Change::Credential {
                        id,
                        base,
                        title,
                        body,
                        password,
                    } => {
                        request.push(6)?;
                        request.push(u8::from(id.is_some()))?;
                        if let Some(id) = id {
                            request.extend(&id)?;
                        }
                        request.extend(&base.to_be_bytes())?;
                        field(&mut request, title.as_str().as_bytes())?;
                        request.push(u8::from(body.is_some()))?;
                        if let Some(body) = body {
                            field(&mut request, body.as_str().as_bytes())?;
                        }
                        field(&mut request, password.as_str().as_bytes())?;
                    }
                    Change::Create { title, body } => {
                        request.push(1)?;
                        field(&mut request, title.as_str().as_bytes())?;
                        field(&mut request, body.as_str().as_bytes())?;
                    }
                    Change::Edit {
                        id,
                        base,
                        title,
                        body,
                    } => {
                        request.push(2)?;
                        request.extend(&id)?;
                        request.extend(&base.to_be_bytes())?;
                        field(&mut request, title.as_str().as_bytes())?;
                        field(&mut request, body.as_str().as_bytes())?;
                    }
                    Change::Rename { id, base, title } => {
                        request.push(3)?;
                        request.extend(&id)?;
                        request.extend(&base.to_be_bytes())?;
                        field(&mut request, title.as_str().as_bytes())?;
                    }
                    Change::Delete { id, base } => {
                        request.push(4)?;
                        request.extend(&id)?;
                        request.extend(&base.to_be_bytes())?;
                    }
                }
                let response = self.exchange(request, op, cancel, replies)?;
                let mut input = success(&response)?;
                let id = input.array()?;
                let revision = input.number()?;
                input.finish()?;
                Ok(Reply::Committed {
                    op,
                    id,
                    revision: (revision != 0).then_some(revision),
                })
            }
            Command::AddKey { op, kind: added } => {
                let response = self.call(vec![9, kind(added)], op, cancel, replies)?;
                success(&response)?.finish()?;
                self.keys(cancel, replies)?;
                Ok(Reply::Keys {
                    op,
                    keys: self.listing(),
                })
            }
            Command::ReplaceKeys {
                op,
                revoked,
                kind: replacement,
            } => {
                let mut request = Frame(vec![10, kind(replacement)]);
                request.push(u8::try_from(revoked.len()).map_err(|_| "too many revoked keys")?)?;
                for index in revoked {
                    field(
                        &mut request,
                        self.enrolled
                            .get(index)
                            .ok_or("key is no longer listed")?
                            .credential(),
                    )?;
                }
                let response = self.exchange(request, op, cancel, replies)?;
                success(&response)?.finish()?;
                self.keys(cancel, replies)?;

                Ok(Reply::Keys {
                    op,
                    keys: self.listing(),
                })
            }
            Command::Export { op, folder } => {
                // Entry writes and key changes both advance the vault revision.
                self.entries(cancel, replies)?;
                let response = self.call(vec![7], op, cancel, replies)?;
                let mut input = success(&response)?;
                let copy = input.field(pass::MAX_COPY)?;
                input.finish()?;
                let name = format!("td-pass-notebook-r{}.tdpass", self.revision);
                let path = crate::files::write_copy(&folder, &name, copy)?;
                Ok(Reply::Exported {
                    op,
                    path: path.display().to_string(),
                })
            }
            Command::ReadCopy { op, path } => {
                self.copy = None;
                let bytes = crate::files::read_copy(&path, pass::MAX_COPY)?;
                let keys = pass::keys_of(&bytes).map_err(|_| "invalid encrypted notebook copy")?;
                let labels = keys.iter().map(super::label).collect();
                self.copy = Some((bytes, keys));
                Ok(Reply::Copy { op, keys: labels })
            }
            Command::Import { op, key } => {
                let (copy, keys) = self.copy.as_ref().ok_or("no encrypted copy was selected")?;
                let mut request = Frame(vec![8]);
                field(
                    &mut request,
                    keys.get(key)
                        .ok_or("copy key is no longer listed")?
                        .credential(),
                )?;
                field(&mut request, copy)?;
                let response = self.exchange(request, op, cancel, replies)?;
                success(&response)?.finish()?;
                self.copy = None;
                self.using = Some(key);
                self.unlocked(op, cancel, replies)
            }
            Command::Lock => {
                let response = self.call(vec![11], 0, cancel, replies)?;
                success(&response)?.finish()?;
                self.copy = None;
                self.using = None;
                Ok(Reply::Locked {
                    keys: self.keys(cancel, replies)?,
                })
            }
            Command::ReadStore { .. } | Command::ImportStore { .. } | Command::DropStore { .. } => {
                Err("password-store shell import is unavailable in trusted notebook mode".into())
            }
        }
    }
}
fn success(message: &impl Message) -> Result<Reader<'_>, String> {
    let mut reader = Reader::new(message.bytes());
    if reader.byte()? != 0 {
        return Err("notebook request failed".into());
    }
    Ok(reader)
}
fn operation(command: &Command) -> Op {
    match command {
        Command::Create { op, .. }
        | Command::Unlock { op, .. }
        | Command::BindMail { op, .. }
        | Command::Apply { op, .. }
        | Command::AddKey { op, .. }
        | Command::ReplaceKeys { op, .. }
        | Command::Export { op, .. }
        | Command::ReadCopy { op, .. }
        | Command::Import { op, .. }
        | Command::ReadStore { op, .. }
        | Command::ImportStore { op, .. }
        | Command::DropStore { op } => *op,
        _ => 0,
    }
}
pub(super) fn serve(
    transport: pass::Trusted,
    jobs: &Receiver<Job>,
    answers: &Receiver<(Op, Answer)>,
    replies: &Sender<Reply>,
) {
    serve_on(transport, TIMES, jobs, answers, replies);
}

fn serve_on(
    transport: impl Link,
    times: Times,
    jobs: &Receiver<Job>,
    answers: &Receiver<(Op, Answer)>,
    replies: &Sender<Reply>,
) {
    let mut backend = Backend {
        transport,
        times,
        answers,
        enrolled: Vec::new(),
        using: None,
        revision: 0,
        copy: None,
        broken: false,
    };
    loop {
        // An answer that came after its operation ended is dropped here,
        // clearing any PIN it carried.
        while answers.try_recv().is_ok() {}
        let job = match jobs.recv_timeout(Duration::from_millis(750)) {
            Ok(job) => job,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if backend.using.is_none() {
                    continue;
                }
                let result = (|| {
                    let response = backend.call(vec![15], 0, &pass::Cancel::new(), replies)?;
                    let mut input = success(&response)?;
                    let count = input.byte()?;
                    if count > 16 {
                        return Err("too many mail requests".into());
                    }
                    let mut requests = Vec::new();
                    for _ in 0..count {
                        requests.push(pass::MailBinding::new(
                            input.text(256)?.to_owned(),
                            input.text(2048)?.to_owned(),
                            input.text(1024)?.to_owned(),
                        )?);
                    }
                    input.finish()?;
                    Ok::<_, String>(requests)
                })();
                match result {
                    Ok(requests) => {
                        if replies.send(Reply::MailRequests(requests)).is_err() {
                            break;
                        }
                    }
                    Err(text) => {
                        let _ = replies.send(Reply::Refused { text });
                        break;
                    }
                }
                continue;
            }
        };
        let op = operation(&job.command);
        let result = backend.command(job.command, &job.cancel, replies);
        let cancelled = job.cancel.cancelled();
        let reply = match result {
            Ok(reply) => reply,
            Err(text) if op == 0 => Reply::Refused { text },
            Err(text) => Reply::Failed {
                op,
                failure: Failure {
                    stale: text.contains("Stale"),
                    uncertain: text.contains("Uncertain"),
                    text,
                    cancelled,
                },
            },
        };
        // A declined operation fails like any other; the next is served.
        if replies.send(reply).is_err() || backend.broken {
            break;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn frame(purpose: u8, role: u8, step: u8, key: Option<[u8; 4]>) -> Vec<u8> {
        computer_frame(purpose, role, step, false, key)
    }

    fn computer_frame(
        purpose: u8,
        role: u8,
        step: u8,
        computer: bool,
        key: Option<[u8; 4]>,
    ) -> Vec<u8> {
        let mut bytes = vec![
            0x7d,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            9,
            purpose,
            role,
            step,
            u8::from(computer),
        ];
        match key {
            Some(key) => {
                bytes.push(1);
                bytes.extend_from_slice(&key);
            }
            None => bytes.push(0),
        }
        bytes
    }

    /// Root's prompt becomes the standalone prompt's ask, in its words:
    /// step zero asks for the key, later steps for its PIN by use.
    #[test]
    fn root_prompts_read_as_the_standalone_prompt() {
        let (number, ask) = prompt(&frame(3, 1, 0, Some([0xab, 0, 0x12, 0xff]))).unwrap();
        assert_eq!(number, [0, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(
            ask,
            Ask {
                operation: pass::step_operation(3).unwrap(),
                role: Role::Primary,
                key: Some("ab0012ff".into()),
                pin: None,
                computer: false,
            }
        );
        assert!(ask.operation.starts_with("unlock"));
        for (step, pin) in [
            (1, PinUse::Authorize),
            (2, PinUse::Enroll),
            (3, PinUse::Proof),
        ] {
            let (_, ask) = prompt(&frame(6, 2, step, None)).unwrap();
            assert_eq!(
                (ask.role, ask.key, ask.pin),
                (Role::Backup, None, Some(pin))
            );
        }
        for bad in [
            frame(4, 1, 0, None),
            frame(3, 3, 0, None),
            frame(3, 1, 4, None),
            frame(3, 1, 0, None)[..12].to_vec(),
            [frame(3, 1, 0, None), vec![0]].concat(),
            computer_frame(3, 1, 1, false, None)
                .iter()
                .enumerate()
                .map(|(at, byte)| if at == 12 { 2 } else { *byte })
                .collect(),
        ] {
            assert!(prompt(&bad).is_err());
        }
    }

    /// This computer's TPM asks for its PIN alone: the unlock's PIN, and
    /// an enrollment's new PIN then, as its proof, the same PIN again.
    #[test]
    fn a_computer_s_steps_read_as_its_pin() {
        let fingerprint = Some([1, 2, 3, 4]);
        let (_, unlock) = prompt(&computer_frame(3, 1, 1, true, fingerprint)).unwrap();
        assert!(unlock.computer);
        assert_eq!(unlock.pin, Some(PinUse::Authorize));
        assert_eq!(unlock.key.as_deref(), Some("01020304"));
        let (_, seal) = prompt(&computer_frame(6, 2, 2, true, None)).unwrap();
        assert_eq!((seal.computer, seal.pin), (true, Some(PinUse::Enroll)));
        let (_, proof) = prompt(&computer_frame(6, 2, 1, true, fingerprint)).unwrap();
        assert_eq!(proof.pin, Some(PinUse::Proof));
        // A security key's repeat assertion stays an authorization.
        let (_, repeat) = prompt(&frame(6, 2, 1, fingerprint)).unwrap();
        assert_eq!(repeat.pin, Some(PinUse::Authorize));
    }

    /// A link whose replies a script gives from the frames sent so far.
    struct Scripted<F: FnMut(&[Vec<u8>]) -> Option<Vec<u8>>> {
        sent: std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>,
        script: F,
    }

    struct Plain(Vec<u8>);
    impl Message for Plain {
        fn bytes(&self) -> &[u8] {
            &self.0
        }
    }

    impl<F: FnMut(&[Vec<u8>]) -> Option<Vec<u8>>> Link for Scripted<F> {
        type Message = Plain;
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.sent.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
        fn poll(&mut self) -> Result<Option<Plain>, String> {
            Ok((self.script)(&self.sent.borrow()).map(Plain))
        }
    }

    const QUICK: Times = Times {
        answer: Duration::from_secs(5),
        decline: Duration::from_millis(100),
    };

    type Sent = std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>;

    fn backend<'a, F: FnMut(&[Vec<u8>]) -> Option<Vec<u8>>>(
        script: F,
        answers: &'a Receiver<(Op, Answer)>,
    ) -> (Backend<'a, Scripted<F>>, Sent) {
        let sent = Sent::default();
        (
            Backend {
                transport: Scripted {
                    sent: sent.clone(),
                    script,
                },
                times: QUICK,
                answers,
                enrolled: Vec::new(),
                using: None,
                revision: 0,
                copy: None,
                broken: false,
            },
            sent,
        )
    }

    fn failure(text: &str) -> Vec<u8> {
        let mut bytes = Frame(vec![1]);
        field(&mut bytes, text.as_bytes()).unwrap();
        bytes.0.clone()
    }

    /// Abandoning an operation declines it once, naming no prompt, and
    /// root then has only the decline's time to answer.
    #[test]
    fn a_cancel_declines_once_and_shortens_the_wait() {
        let (_answering, answers) = std::sync::mpsc::channel();
        let (replies, _shown) = std::sync::mpsc::channel();
        let (mut backend, sent) = backend(|_| None, &answers);
        let cancel = pass::Cancel::new();
        cancel.cancel();
        let start = Instant::now();
        assert!(backend
            .exchange(Frame(vec![3]), 4, &cancel, &replies)
            .is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(backend.broken);
        let sent = sent.borrow();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1], answer([0; 8], Answer::Decline).unwrap().0);
    }

    /// A prompt root sends after the decline is not shown, and root's
    /// reply saying so ends the exchange as that failure.
    #[test]
    fn a_prompt_after_a_decline_is_not_shown() {
        let (_answering, answers) = std::sync::mpsc::channel();
        let (replies, shown) = std::sync::mpsc::channel();
        let mut turn = 0;
        let (mut backend, _) = backend(
            |sent| {
                if sent.len() < 2 {
                    return None;
                }
                turn += 1;
                match turn {
                    1 => Some(vec![0x7d, 0, 0, 0, 0, 0, 0, 0, 2, 3, 1, 1, 0, 0]),
                    2 => Some(failure("cancelled")),
                    _ => None,
                }
            },
            &answers,
        );
        let cancel = pass::Cancel::new();
        cancel.cancel();
        let reply = backend
            .exchange_framed(Frame(vec![3]), 4, &cancel, &replies)
            .unwrap();
        assert_eq!(reply.err().unwrap(), "cancelled");
        assert!(shown.try_recv().is_err());
        assert!(!backend.broken);
    }

    /// Only the asked operation's answer reaches root, naming its prompt.
    #[test]
    fn answers_for_another_operation_are_dropped() {
        let (answering, answers) = std::sync::mpsc::channel();
        let (replies, shown) = std::sync::mpsc::channel();
        let mut prompted = false;
        let (mut backend, sent) = backend(
            |sent| {
                if !prompted {
                    prompted = true;
                    return Some(vec![0x7d, 0, 0, 0, 0, 0, 0, 0, 7, 3, 1, 1, 0, 0]);
                }
                if shown_answer(sent) {
                    return Some(vec![0]);
                }
                answering.send((5, Answer::Proceed)).unwrap();
                answering
                    .send((4, Answer::Pin(Bytes::copy(b"123456"))))
                    .unwrap();
                None
            },
            &answers,
        );
        let reply = backend
            .exchange_framed(Frame(vec![3]), 4, &pass::Cancel::new(), &replies)
            .unwrap()
            .unwrap();
        assert_eq!(reply.bytes(), [0]);
        assert!(matches!(shown.try_recv(), Ok(Reply::Ask { op: 4, .. })));
        let sent = sent.borrow();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1],
            answer(
                [0, 0, 0, 0, 0, 0, 0, 7],
                Answer::Pin(Bytes::copy(b"123456"))
            )
            .unwrap()
            .0
        );
    }

    fn shown_answer(sent: &[Vec<u8>]) -> bool {
        sent.iter().any(|frame| frame.first() == Some(&0x7c))
    }

    /// A channel root stopped answering ends the thread, and an answer
    /// left from an ended operation is dropped before the next job.
    #[test]
    fn a_broken_channel_ends_the_thread_and_late_answers_are_dropped() {
        let (answering, answers) = std::sync::mpsc::channel();
        let (replies, shown) = std::sync::mpsc::channel();
        let (jobs, queue) = std::sync::mpsc::channel();
        answering
            .send((9, Answer::Pin(Bytes::copy(b"123456"))))
            .unwrap();
        jobs.send(Job {
            command: Command::Open,
            cancel: pass::Cancel::new(),
        })
        .unwrap();
        let sent = Sent::default();
        let link = Scripted {
            sent: sent.clone(),
            script: |_: &[Vec<u8>]| None,
        };
        let times = Times {
            answer: Duration::from_millis(100),
            decline: Duration::from_millis(100),
        };
        // The jobs channel stays open: only the broken channel ends it.
        serve_on(link, times, &queue, &answers, &replies);
        assert!(answers.try_recv().is_err());
        assert!(matches!(shown.try_recv(), Ok(Reply::Refused { .. })));
        assert_eq!(sent.borrow().len(), 1);
        drop(jobs);
    }

    #[test]
    fn answers_name_their_prompt_and_a_decline_names_none() {
        let number = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            answer(number, Answer::Proceed).unwrap().0,
            [0x7c, 1, 2, 3, 4, 5, 6, 7, 8, 0]
        );
        assert_eq!(
            answer(number, Answer::Pin(Bytes::copy(b"123456")))
                .unwrap()
                .0,
            [0x7c, 1, 2, 3, 4, 5, 6, 7, 8, 1, 0, 0, 0, 6, b'1', b'2', b'3', b'4', b'5', b'6']
        );
        assert_eq!(answer([0; 8], Answer::Decline).unwrap().0[9], 2);
    }
}
