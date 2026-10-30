//! Root owns the worker until its verified result and successful exit agree.

use crate::consent::{Operation, Request};
use crate::login::Pin;
use crate::notebook_wire::{field, Bytes, PendingWire, Reader};
use crate::unlock::Event;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(PartialEq, Eq)]
enum Phase {
    Starting,
    Present,
    Running,
    Pin,
    Exit,
    Stopping,
    Done,
}

pub(crate) struct Token {
    request: Request,
    child: Option<Child>,
    wire: Option<PendingWire>,
    deadline: Instant,
    phase: Phase,
    pending_pin: Option<Bytes>,
    result: Option<Bytes>,
    failure: Option<String>,
}

impl Token {
    pub fn start(request: Request, token: &[u8]) -> Result<Self, String> {
        let command = crate::unlock::command("notebook-token", request.owner());
        Self::spawn(request, token, command)
    }

    fn spawn(request: Request, token: &[u8], mut command: Command) -> Result<Self, String> {
        let Operation::Notebook(binding) = request.operation() else {
            return Err("not a notebook token request".into());
        };
        let mut digest = crate::sha256::Sha256::new();
        digest.update(token);
        if binding.step != 0 || digest.finalize() != binding.token || token.len() > 16384 {
            return Err("notebook token differs from admitted request".into());
        }
        let mut initial = Bytes(Vec::new());
        field(&mut initial, &request.encode())?;
        field(&mut initial, token)?;
        let (root, worker) = UnixStream::pair().map_err(|_| "create notebook token endpoint")?;
        let mut wire = PendingWire::new(root)?;
        wire.queue(initial)?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(120))
            .ok_or("notebook deadline overflow")?;
        let child = command
            .stdin(Stdio::from(OwnedFd::from(worker)))
            .spawn()
            .map_err(|_| "start notebook token worker")?;
        Ok(Self {
            request,
            child: Some(child),
            wire: Some(wire),
            deadline,
            phase: Phase::Starting,
            pending_pin: None,
            result: None,
            failure: None,
        })
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        if self.phase != Phase::Present
            || request != &self.request
            || Instant::now() >= self.deadline
        {
            self.cancel()?;
            return Err("stale notebook presentation receipt".into());
        }
        let mut ack = Bytes(vec![0x41]);
        field(&mut ack, &request.encode())?;
        self.wire
            .as_mut()
            .ok_or("missing notebook worker")?
            .queue(ack)?;
        self.phase = if request.asks_pin() {
            Phase::Pin
        } else {
            Phase::Running
        };
        Ok(())
    }

    pub fn pin(&mut self, request: &Request, pin: Pin) -> Result<bool, String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) || Instant::now() >= self.deadline {
            self.cancel()?;
            return Ok(false);
        }
        if self.phase != Phase::Pin
            || request != &self.request
            || self.pending_pin.is_some()
            || !pin.is_pin()
        {
            self.cancel()?;
            return Err("stale notebook PIN".into());
        }
        let mut frame = Bytes(vec![0x42]);
        field(&mut frame, &request.encode())?;
        field(&mut frame, pin.bytes())?;
        self.pending_pin = Some(frame);
        self.phase = Phase::Running;
        Ok(true)
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        if matches!(self.phase, Phase::Stopping | Phase::Done) {
            return Ok(());
        }
        self.wire = None;
        self.pending_pin = None;
        self.result = None;
        self.failure = Some("notebook token operation ended without a result".into());
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
        self.phase = Phase::Stopping;
        self.deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .ok_or("notebook cleanup deadline overflow")?;
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        if self.phase == Phase::Done {
            return Ok(self.terminal());
        }
        if self.phase == Phase::Stopping {
            return self.reap();
        }
        if Instant::now() >= self.deadline {
            self.cancel()?;
            return self.reap();
        }
        match self.advance() {
            Ok(event) => Ok(event),
            Err(_) => {
                self.cancel()?;
                self.reap()
            }
        }
    }

    fn advance(&mut self) -> Result<Event, String> {
        if self.phase == Phase::Exit {
            let finished = self
                .wire
                .as_mut()
                .ok_or("missing notebook worker")?
                .finished()?;
            if let Some(child) = &mut self.child {
                if let Some(status) = child.try_wait().map_err(|_| "reap notebook token worker")? {
                    self.child = None;
                    if !status.success() {
                        return Err("notebook token worker failed its exit".into());
                    }
                }
            }
            if finished && self.child.is_none() {
                self.wire = None;
                self.phase = Phase::Done;
                return Ok(Event::Complete);
            }
            return Ok(Event::Waiting);
        }
        let wire = self.wire.as_mut().ok_or("missing notebook worker")?;
        if !wire.sending() {
            if let Some(pin) = self.pending_pin.take() {
                wire.queue(pin)?;
            }
        }
        let frame = wire.poll()?;
        if let Some(frame) = frame {
            let mut input = Reader::new(&frame.0);
            match input.byte()? {
                0x40 if matches!(self.phase, Phase::Starting | Phase::Running)
                    && self.pending_pin.is_none() =>
                {
                    let request = Request::decode(input.field(256)?)?;
                    input.finish()?;
                    let valid = if self.phase == Phase::Starting {
                        request == self.request
                    } else {
                        match (self.request.operation(), request.operation()) {
                            (Operation::Notebook(before), Operation::Notebook(after)) => {
                                request.nonce() == self.request.nonce()
                                    && request.owner() == self.request.owner()
                                    && after.follows(before)
                            }
                            _ => false,
                        }
                    };
                    if !valid {
                        return Err("notebook worker changed its presentation".into());
                    }
                    self.request = request;
                    self.phase = Phase::Present;
                }
                0x44 if self.phase == Phase::Running && self.pending_pin.is_none() => {
                    let Operation::Notebook(binding) = self.request.operation() else {
                        return Err("notebook completion has no binding".into());
                    };
                    if !matches!((binding.method, binding.step), (0x30, 3) | (0x31, 1)) {
                        return Err("notebook completion skipped its PIN ceremony".into());
                    }
                    let result = input.field(2048)?;
                    input.finish()?;
                    if result.first() != Some(&binding.method) {
                        return Err("notebook result changed method".into());
                    }
                    self.result = Some(Bytes(result.to_vec()));
                    self.phase = Phase::Exit;
                }
                _ => return Err("unexpected notebook worker frame".into()),
            }
        }
        Ok(match self.phase {
            Phase::Present => Event::Present(self.request.clone()),
            Phase::Pin => Event::Pin(self.request.clone()),
            _ => Event::Waiting,
        })
    }

    fn terminal(&self) -> Event {
        match &self.failure {
            Some(why) => Event::Failed(why.clone()),
            None => Event::Complete,
        }
    }

    fn reap(&mut self) -> Result<Event, String> {
        if let Some(child) = &mut self.child {
            if child
                .try_wait()
                .map_err(|_| "reap cancelled notebook worker")?
                .is_some()
            {
                self.child = None;
            }
        }
        if self.child.is_some() {
            if Instant::now() >= self.deadline {
                return Err("notebook worker could not be reaped".into());
            }
            return Ok(Event::Waiting);
        }
        self.phase = Phase::Done;
        Ok(self.terminal())
    }

    pub fn take_result(&mut self) -> Option<Bytes> {
        if self.phase == Phase::Done && self.failure.is_none() {
            self.result.take()
        } else {
            None
        }
    }

    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        self.cancel()?;
        while self.phase != Phase::Done {
            self.reap()?;
            if self.phase != Phase::Done {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.wire = None;
        self.pending_pin = None;
        self.result = None;
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::consent::Notebook;
    use crate::notebook_wire::Wire;
    use std::os::fd::AsFd;

    fn request() -> Request {
        let mut hash = crate::sha256::Sha256::new();
        hash.update(&[0x31, 3, 1]);
        Request::new(
            [8; 32],
            1000,
            Operation::Notebook(Notebook {
                method: 0x31,
                action: 3,
                purpose: 3,
                role: 1,
                step: 0,
                operation: [9; 32],
                token: hash.finalize(),
            }),
        )
        .unwrap()
    }

    #[test]
    fn scripted_worker() {
        let Ok(script) = std::env::var("TD_NOTEBOOK_WORKER_SCRIPT") else {
            return;
        };
        let input = std::io::stdin();
        let endpoint = UnixStream::from(input.as_fd().try_clone_to_owned().unwrap());
        let mut wire = Wire::new(endpoint).unwrap();
        let initial = wire.receive(Duration::from_secs(5)).unwrap();
        let mut reader = Reader::new(&initial.0);
        let mut request = Request::decode(reader.field(256).unwrap()).unwrap();
        let _ = reader.field(16384).unwrap();
        reader.finish().unwrap();
        if script == "early-result" {
            let mut result = Bytes(vec![0x44]);
            field(&mut result, &[0x31; 37]).unwrap();
            wire.send(&result.0).unwrap();
            return;
        }
        for step in [0, 1] {
            let Operation::Notebook(mut binding) = request.operation().clone() else {
                panic!();
            };
            binding.step = step;
            request = Request::new(
                *request.nonce(),
                request.owner(),
                Operation::Notebook(binding),
            )
            .unwrap();
            let mut frame = Bytes(vec![0x40]);
            field(&mut frame, &request.encode()).unwrap();
            wire.send(&frame.0).unwrap();
            let ack = wire.receive(Duration::from_secs(5)).unwrap();
            let mut reader = Reader::new(&ack.0);
            assert_eq!(reader.byte().unwrap(), 0x41);
            assert_eq!(reader.field(256).unwrap(), request.encode());
            reader.finish().unwrap();
        }
        let pin = wire.receive(Duration::from_secs(5)).unwrap();
        let mut reader = Reader::new(&pin.0);
        assert_eq!(reader.byte().unwrap(), 0x42);
        assert_eq!(reader.field(256).unwrap(), request.encode());
        assert_eq!(reader.field(63).unwrap(), b"123456");
        reader.finish().unwrap();
        let mut result = Bytes(vec![0x44]);
        field(&mut result, &[0x31; 37]).unwrap();
        wire.send(&result.0).unwrap();
        if script == "bad-exit" {
            std::process::exit(1);
        }
        if script == "extra-byte" {
            // A second completion cannot be mistaken for a clean EOF.
            wire.send(&result.0).unwrap();
        }
    }

    fn worker(script: &str) -> Token {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "notebook_token::tests::scripted_worker",
                "--nocapture",
            ])
            .env_clear()
            .env("TD_NOTEBOOK_WORKER_SCRIPT", script)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Token::spawn(request(), &[0x31, 3, 1], command).unwrap()
    }

    fn drive(token: &mut Token) -> Event {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let event = token.poll().unwrap();
            match event {
                Event::Present(ref request) => token.presented(request).unwrap(),
                Event::Pin(ref request) => {
                    assert!(token.pin(request, Pin::entry(b"123456").unwrap()).unwrap());
                }
                Event::Complete | Event::Failed(_) => return event,
                _ => {}
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn results_need_the_pin_ceremony_clean_eof_and_successful_exit() {
        let mut good = worker("success");
        assert!(good.take_result().is_none());
        assert_eq!(drive(&mut good), Event::Complete);
        assert_eq!(good.take_result().unwrap().0, [0x31; 37]);
        assert!(good.take_result().is_none());
        for script in ["early-result", "bad-exit", "extra-byte"] {
            let mut bad = worker(script);
            assert!(matches!(drive(&mut bad), Event::Failed(_)), "{script}");
            assert!(bad.take_result().is_none());
            assert!(bad.child.is_none());
        }
    }

    #[test]
    fn cancellation_reaps_the_worker_and_discards_queued_pin_and_result() {
        let mut token = worker("success");
        token.cancel().unwrap();
        assert!(matches!(drive(&mut token), Event::Failed(_)));
        assert!(token.take_result().is_none());
        assert!(token.child.is_none());
    }
}
