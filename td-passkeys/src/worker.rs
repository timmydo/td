//! Root's private control socket is the only input that can choose a
//! credential or return consent. The jail sees just the UHID interface.
use crate::{
    hid::Command,
    service::{Action, Session, VaultAction},
    uhid, Approval, Enrollment, Error, Result, Vault,
};
use std::{
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::AsFd,
        unix::{fs::MetadataExt, net::UnixDatagram},
    },
    path::Path,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};
use td_fido::{
    fido_device::{self, Cancellation, Device, Program},
    fido_pin::Pin as KeyPin,
    web_key::{perform, Ceremony},
    web_request::Request,
};
use td_protector::pin::Pin;
#[path = "../../td-secret/src/memory.rs"]
mod memory;

const DIRECTORY: &str = "/var/lib/td-passkeys";
const PRIMARY: &str = "/run/td-admit/td-passkey-primary";
const MAX: usize = 2048;
const PAUSE: Duration = Duration::from_millis(5);

fn protected() -> Result<()> {
    memory::with_memory_state(
        fs::read_to_string("/proc/swaps"),
        fs::read_to_string("/proc/self/limits"),
        &|p| fs::read_to_string(p),
        || Ok(()),
    )
    .map_err(|_| Error::Refused)
}
struct HidWorker;
impl Program for HidWorker {
    const PATH: &'static str = "/bin/td-secret";
    const ROOT: &'static str = "hid-worker";
    const DESKTOP: &'static str = "hid-worker-desktop";
}
enum Event {
    Pin(u8),
    Done(u8, Vec<u8>),
}
struct Hardware {
    ticket: u64,
    cancel: Cancellation,
    events: Receiver<Event>,
    pin: SyncSender<KeyPin>,
    retries: Option<u8>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Hardware {
    fn start(command: Command, key: Device) -> Result<Self> {
        let ticket = command.ticket();
        let cancel = Cancellation::new();
        let interrupted = cancel.clone();
        let (sender, events) = mpsc::sync_channel(2);
        let (pin, entry) = mpsc::sync_channel(1);
        let end = command.deadline();
        if Instant::now() >= end {
            return Err(Error::Stale);
        }
        let helper = thread::Builder::new().spawn(move || {
            let result = (|| {
                let mut channel = fido_device::Session::<HidWorker>::open_cancellable(
                    key,
                    end,
                    interrupted.clone(),
                )
                .map_err(|_| Error::Refused)?;
                let request = command.request()?;
                let intent = match &request {
                    Request::Create(r) => Ceremony::Register(r),
                    Request::Get(r) => Ceremony::Sign(r),
                    _ => return Err(Error::Invalid),
                };
                let ask = |n| {
                    sender.send(Event::Pin(n)).map_err(|_| "ceremony ended")?;
                    loop {
                        if interrupted.cancelled() || Instant::now() >= end {
                            return Err("ceremony ended".into());
                        }
                        match entry.recv_timeout(Duration::from_millis(25)) {
                            Ok(pin) => return Ok(pin),
                            Err(mpsc::RecvTimeoutError::Timeout) => (),
                            Err(_) => return Err("ceremony ended".into()),
                        }
                    }
                };
                perform(intent, &mut channel, ask, &mut |b| {
                    td_primitives::random::fill(b).map_err(|_| "website entropy unavailable".into())
                })
                .map_err(|_| Error::Refused)
            })();
            let (status, body) = result.unwrap_or_else(|_| (0x27, Vec::new()));
            let _ = sender.send(Event::Done(status, body));
        })?;
        Ok(Self {
            ticket,
            cancel,
            events,
            pin,
            retries: None,
            thread: Some(helper),
        })
    }
}
impl Drop for Hardware {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn storage() -> Result<(File, [u8; 34])> {
    let (mut f, m) = td_fs::open_real_file(Path::new(PRIMARY), "passkey primary")?;
    if (m.uid(), m.gid(), m.nlink(), m.mode() & 0o7777, m.len()) != (0, 0, 1, 0o600, 34) {
        return Err(Error::Refused);
    }
    let mut name = [0; 34];
    f.read_exact(&mut name)?;
    if name.get(..2) != Some(&[0, 11]) {
        return Err(Error::Refused);
    }
    td_fs::private_dir(Path::new(DIRECTORY))?;
    Ok((File::open(DIRECTORY)?, name))
}

enum VaultReply {
    Open(Vault),
    Complete(Vault, Instant, Vec<u8>),
}

struct VaultJob {
    ticket: u64,
    cancel: Cancellation,
    thread: thread::JoinHandle<Result<VaultReply>>,
}

impl VaultJob {
    fn start(
        ticket: u64,
        end: Instant,
        run: impl FnOnce(&dyn Fn() -> bool) -> Result<VaultReply> + Send + 'static,
    ) -> Result<Self> {
        let cancel = Cancellation::new();
        let token = cancel.clone();
        let thread = thread::Builder::new().spawn(move || {
            let current = || !token.cancelled() && Instant::now() < end;
            if !current() {
                return Err(Error::Stale);
            }
            run(&current)
        })?;
        Ok(Self {
            ticket,
            cancel,
            thread,
        })
    }
}

#[derive(Default)]
struct Attachments {
    revision: u64,
    refreshed: Option<Instant>,
    keys: Vec<Device>,
}

impl Attachments {
    fn refresh(&mut self) -> Result<()> {
        if self
            .refreshed
            .is_some_and(|t| t.elapsed() < Duration::from_millis(400))
        {
            return Ok(());
        }
        let keys = Device::website_devices().map_err(|_| Error::Refused)?;
        if self.keys != keys {
            self.revision = self.revision.checked_add(1).ok_or(Error::Full)?;
            self.keys = keys;
        }
        self.refreshed = Some(Instant::now());
        Ok(())
    }
    fn select(&self, stamp: &[u8], index: u8) -> Result<Device> {
        if stamp != self.revision.to_be_bytes() {
            return Err(Error::Stale);
        }
        self.keys
            .iter()
            .copied()
            .find(|k| k.hidraw_index() == index)
            .ok_or(Error::Missing)
    }
}
struct Worker {
    io: uhid::Device,
    session: Session,
    hardware: Option<Hardware>,
    attachments: Attachments,
    vault_job: Option<VaultJob>,
    pin_for: Option<(u64, VaultAction)>,
}
impl Worker {
    fn status(&mut self, reply: &mut Vec<u8>) -> Result<()> {
        if let Some(j) = &self.vault_job {
            if self.session.active(j.ticket) {
                td_alloc::extend_from_slice(reply, &j.ticket.to_be_bytes())?;
                td_alloc::push(reply, 8)?;
            }
            return Ok(());
        }
        if let Some(h) = &self.hardware {
            let Some(n) = h.retries else { return Ok(()) };
            td_alloc::extend_from_slice(reply, &h.ticket.to_be_bytes())?;
            td_alloc::extend_from_slice(reply, &[6, n])?;
            return Ok(());
        }
        let Some((ticket, action)) = self.session.describe() else {
            return Ok(());
        };
        let (verb, rp, name) = match action {
            Action::ChooseDevice => (0, "", ""),
            Action::Register(r, n) => (1, r, n.unwrap_or("")),
            Action::Sign(r) => (2, r, ""),
            Action::CheckHandle(r) => (3, r, ""),
        };
        if self.session.verified(ticket) {
            td_alloc::extend_from_slice(reply, &ticket.to_be_bytes())?;
            return Ok(td_alloc::push(reply, 7)?);
        }
        self.attachments.refresh()?;
        td_alloc::extend_from_slice(reply, &ticket.to_be_bytes())?;
        td_alloc::push(reply, verb)?;
        let stored = match fs::symlink_metadata("/var/lib/td-passkeys/passkeys") {
            Ok(_) => 2,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 1,
            Err(_) => 0,
        };
        let tpm = if fs::symlink_metadata(PRIMARY).is_ok() {
            stored
        } else {
            0
        };
        td_alloc::extend_from_slice(reply, &[u8::from(!self.attachments.keys.is_empty()), tpm])?;
        crate::put(reply, rp.as_bytes())?;
        crate::put(reply, name.as_bytes())?;
        td_alloc::extend_from_slice(reply, &self.attachments.revision.to_be_bytes())?;
        for key in &self.attachments.keys {
            td_alloc::push(reply, key.hidraw_index())?;
        }
        Ok(())
    }
    fn answer(&mut self, packet: &[u8], reply: &mut Vec<u8>) -> Result<()> {
        if packet == [1] {
            return self.status(reply);
        }
        if packet == [2] {
            if let Some(h) = self.hardware.take() {
                let ticket = h.ticket;
                drop(h);
                if self.session.active(ticket) {
                    self.session
                        .finish(ticket, 0x2d, &[], |r| self.io.input(r))?;
                }
            }
            self.pin_for = None;
            if let Some(j) = &self.vault_job {
                j.cancel.cancel();
                if self.session.active(j.ticket) {
                    self.session
                        .finish(j.ticket, 0x2d, &[], |r| self.io.input(r))?;
                }
            }
            self.session.lock(|r| self.io.input(r))?;
            return Ok(());
        }
        let (&op, rest) = packet.split_first().ok_or(Error::Invalid)?;
        let (ticket, body) = rest.split_at_checked(8).ok_or(Error::Invalid)?;
        let ticket = u64::from_be_bytes(ticket.try_into().map_err(|_| Error::Invalid)?);
        if !self.session.active(ticket) {
            return Err(Error::Stale);
        }
        if op != 3 && self.vault_job.is_some() {
            return Err(Error::Locked);
        }
        match (op, body) {
            (3, []) => {
                if let Some(j) = &self.vault_job {
                    j.cancel.cancel();
                }
                self.hardware = None;
                self.pin_for = None;
                self.session.finish(ticket, 0x2d, &[], |r| self.io.input(r))
            }
            (4, data) if self.hardware.is_none() => {
                let (revision, tail) = data.split_at_checked(8).ok_or(Error::Invalid)?;
                let [index] = tail else {
                    return Err(Error::Invalid);
                };
                self.attachments.refresh()?;
                let key = self.attachments.select(revision, *index)?;
                let cmd = self.session.take_physical(ticket)?;
                self.pin_for = None;
                match Hardware::start(cmd, key) {
                    Ok(h) => self.hardware = Some(h),
                    Err(e) => {
                        self.session
                            .finish(ticket, 0x27, &[], |r| self.io.input(r))?;
                        return Err(e);
                    }
                }
                Ok(())
            }
            (5, [choice]) if *choice <= 1 && self.hardware.is_none() => {
                self.pin_for = Some((
                    ticket,
                    if *choice == 0 {
                        VaultAction::UseExisting
                    } else {
                        VaultAction::EnrollAcceptingLossOfOriginalTpm
                    },
                ));
                Ok(())
            }
            (6, pin) => {
                protected()?;
                if let Some(h) = self
                    .hardware
                    .as_mut()
                    .filter(|h| h.ticket == ticket && h.retries.is_some())
                {
                    if !(4..=63).contains(&pin.len()) {
                        return Err(Error::Invalid);
                    }
                    // Make the exact-size allocation before copying any secret.
                    let mut owned: Box<[u8]> = td_alloc::filled(pin.len(), 0)?.into();
                    owned.copy_from_slice(pin);
                    h.pin
                        .try_send(KeyPin::new(owned).map_err(|_| Error::Invalid)?)
                        .map_err(|_| Error::Stale)?;
                    h.retries = None;
                } else {
                    let (_, choice) = self
                        .pin_for
                        .take()
                        .filter(|(t, _)| *t == ticket)
                        .ok_or(Error::Stale)?;
                    let pin = Pin::parse(pin).map_err(|_| Error::Invalid)?;
                    let deadline = self.session.begin_open(ticket, choice)?;
                    self.vault_job = Some(VaultJob::start(ticket, deadline, move |current| {
                        let (dir, name) = storage()?;
                        let tpm = td_tpm::Client::new(td_tpm::Device::open_io()?);
                        if !current() {
                            return Err(Error::Stale);
                        }
                        let vault = match choice {
                            VaultAction::UseExisting => Vault::unlock(&dir, tpm, &name, &pin)?,
                            VaultAction::EnrollAcceptingLossOfOriginalTpm => Vault::enroll_if(
                                &dir,
                                tpm,
                                &name,
                                &pin,
                                Enrollment::AcceptLossOfOriginalTpm,
                                current,
                            )?,
                        };
                        Ok(VaultReply::Open(vault))
                    })?);
                }
                Ok(())
            }
            (7, [index]) => {
                let mut i = 0usize;
                self.session.accounts(ticket, |a| {
                    if i == usize::from(*index) {
                        td_alloc::extend_from_slice(reply, a.id)?;
                        crate::put(reply, a.name.as_bytes())?;
                        crate::put(reply, a.user)?;
                    }
                    i += 1;
                    Ok(())
                })
            }
            (8, id) if id.is_empty() || id.len() == 32 => {
                protected()?;
                let account: Option<[u8; 32]> = id.try_into().ok();
                let (command, mut vault, until) = self.session.begin_result(ticket)?;
                let job = VaultJob::start(ticket, command.deadline(), move |current| {
                    let reply = match command.request()? {
                        Request::Create(r) => vault.register_if(&r, Approval::Verified, current)?,
                        Request::Get(r) if !r.presence() => vault.preflight(&r)?,
                        Request::Get(r) => {
                            vault.sign(&r, &account.ok_or(Error::Missing)?, Approval::Verified)?
                        }
                        _ => return Err(Error::Invalid),
                    };
                    Ok(VaultReply::Complete(vault, until, reply))
                });
                match job {
                    Ok(j) => self.vault_job = Some(j),
                    Err(e) => {
                        self.session
                            .finish(ticket, 0x27, &[], |r| self.io.input(r))?;
                        return Err(e);
                    }
                }
                Ok(())
            }
            (9, []) if matches!(self.session.describe(), Some((_, Action::ChooseDevice))) => {
                self.session.finish(ticket, 0x35, &[], |r| self.io.input(r))
            }
            _ => Err(Error::Invalid),
        }
    }
    fn advance(&mut self) -> Result<()> {
        for _ in 0..32 {
            let Some(r) = self.io.output()? else {
                break;
            };
            self.session
                .receive(&r, Instant::now(), |r| self.io.input(r))?;
        }
        self.session.poll(Instant::now(), |r| self.io.input(r))?;
        if let Some(j) = &self.vault_job {
            if !self.session.active(j.ticket) {
                j.cancel.cancel();
            }
            if j.thread.is_finished() {
                let job = self.vault_job.take().ok_or(Error::Stale)?;
                let result = job.thread.join().unwrap_or(Err(Error::Refused));
                if !job.cancel.cancelled() && self.session.active(job.ticket) {
                    match result {
                        Ok(VaultReply::Open(v)) => self.session.opened(job.ticket, v)?,
                        Ok(VaultReply::Complete(v, until, reply)) => {
                            self.session
                                .finish(job.ticket, 0, &reply, |r| self.io.input(r))?;
                            self.session.cache(v, until);
                        }
                        Err(e) => self.session.finish(
                            job.ticket,
                            super::service::status(&e),
                            &[],
                            |r| self.io.input(r),
                        )?,
                    }
                }
            }
        }
        if let Some(h) = &mut self.hardware {
            if !self.session.active(h.ticket) {
                self.hardware = None;
                return Ok(());
            }
            match h.events.try_recv() {
                Ok(Event::Pin(n)) => h.retries = Some(n),
                Ok(Event::Done(code, data)) => {
                    self.session
                        .finish(h.ticket, code, &data, |r| self.io.input(r))?;
                    self.hardware = None;
                }
                Err(mpsc::TryRecvError::Empty) => (),
                Err(_) => {
                    self.session
                        .finish(h.ticket, 0x27, &[], |r| self.io.input(r))?;
                    self.hardware = None;
                }
            }
        }
        Ok(())
    }
}

/// Only a root-created unnamed socketpair is admitted. Root owns the
/// child lifetime and ends it when the authority generation disconnects.
pub fn run(group: u32) -> std::result::Result<(), Error> {
    fido_device::require_root().map_err(|_| Error::Refused)?;
    protected()?;
    let control = UnixDatagram::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    if !control.local_addr()?.is_unnamed() || !control.peer_addr()?.is_unnamed() {
        return Err(Error::Refused);
    }
    control.set_nonblocking(true)?;
    let mut io = uhid::Device::create()?;
    let end = Instant::now() + Duration::from_secs(5);
    while !io.publish(group)? {
        if Instant::now() >= end {
            return Err(Error::Stale);
        }
        thread::sleep(PAUSE);
    }
    let mut w = Worker {
        io,
        session: Session::default(),
        hardware: None,
        vault_job: None,
        attachments: Attachments::default(),
        pin_for: None,
    };
    let mut input = crate::Secret([0; MAX + 1]);
    let mut last_control = Instant::now();
    loop {
        if last_control.elapsed() >= Duration::from_secs(8) {
            return Err(Error::Stale);
        }
        w.advance()?;
        match control.recv(&mut input.0) {
            Ok(n) if (1..=MAX).contains(&n) => {
                last_control = Instant::now();
                let mut reply = td_alloc::with_capacity(MAX)?;
                td_alloc::push(&mut reply, 0)?;
                if let Err(err) = w.answer(input.0.get(..n).ok_or(Error::Invalid)?, &mut reply) {
                    reply.clear();
                    td_alloc::push(&mut reply, super::service::status(&err))?;
                }
                td_tpm::zero(&mut input.0);
                if control.send(&reply)? != reply.len() {
                    return Err(Error::Stale);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(PAUSE),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            _ => return Err(Error::Invalid),
        }
    }
}
