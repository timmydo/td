//! The root-launched notebook client. No hardware or standalone vault access.

use super::*;
#[path = "../../../td-secret/src/notebook_wire.rs"]
#[allow(
    dead_code,
    reason = "shared field codec; transport is the admitted pass API"
)]
mod wire;
use wire::{field, Bytes as Frame, Reader};

struct Backend {
    transport: pass::Trusted,
    enrolled: Vec<pass::Key>,
    using: Option<usize>,
    revision: u64,
    copy: Option<(Vec<u8>, Vec<pass::Key>)>,
}

impl Backend {
    fn exchange(
        &mut self,
        frame: Frame,
        op: Op,
        cancel: &pass::Cancel,
        replies: &Sender<Reply>,
    ) -> Result<pass::TrustedMessage, String> {
        self.transport.send(&frame.0)?;
        let end = Instant::now()
            .checked_add(Duration::from_secs(650))
            .ok_or("notebook deadline overflow")?;
        loop {
            if cancel.cancelled() || Instant::now() >= end {
                return Err("notebook operation cancelled; close and reopen the window".into());
            }
            if let Some(message) = self.transport.poll()? {
                if message.bytes() == [0x7e] {
                    replies
                        .send(Reply::TrustedAttention { op })
                        .map_err(|_| "notebook window closed")?;
                } else {
                    let mut input = Reader::new(message.bytes());
                    match input.byte()? {
                        0 => return Ok(message),
                        1 => {
                            let error = input.text(4096)?.to_owned();
                            input.finish()?;
                            return Err(error);
                        }
                        _ => return Err("invalid trusted notebook reply".into()),
                    }
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
    ) -> Result<pass::TrustedMessage, String> {
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
        for _ in 0..count {
            let role = match input.byte()? {
                1 => pass::KeyRole::Primary,
                2 => pass::KeyRole::Backup,
                _ => return Err("invalid notebook key role".into()),
            };
            keys.push(pass::Key::from_credential(role, input.field(1024)?)?);
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
            Command::Create { op, backup } => {
                let response = self.call(vec![2, u8::from(backup)], op, cancel, replies)?;
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
            Command::AddKey { op } => {
                let response = self.call(vec![9], op, cancel, replies)?;
                success(&response)?.finish()?;
                self.keys(cancel, replies)?;
                Ok(Reply::Keys {
                    op,
                    keys: self.listing(),
                })
            }
            Command::ReplaceKeys { op, revoked } => {
                let mut request = Frame(vec![10]);
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
fn success(message: &pass::TrustedMessage) -> Result<Reader<'_>, String> {
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
        | Command::Apply { op, .. }
        | Command::AddKey { op }
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
pub(super) fn serve(transport: pass::Trusted, jobs: &Receiver<Job>, replies: &Sender<Reply>) {
    let mut backend = Backend {
        transport,
        enrolled: Vec::new(),
        using: None,
        revision: 0,
        copy: None,
    };
    while let Ok(job) = jobs.recv() {
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
        if replies.send(reply).is_err() || cancelled {
            break;
        }
    }
}
