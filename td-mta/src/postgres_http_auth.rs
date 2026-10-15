//! Fresh primary authentication for the fixed HTTPS credential worker.
use crate::{
    clock::RuntimeClock,
    device_credentials::Presented,
    http_auth::Authenticator,
    ids::DeviceId,
    ports::{Deadline, Error},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_resolver,
    postgres_runtime_profile::Binding,
    postgres_snapshot,
};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Preparation adapter, not a serving entry point. The HTTPS service must use
/// this same RuntimeClock. One call may run; no queue or background connection.
pub struct Authentication<'a> {
    loaded: &'a Loaded,
    executable: &'a Path,
    binding: &'a Binding,
    clock: &'a RuntimeClock,
    active: AtomicBool,
}
impl<'a> Authentication<'a> {
    pub fn new(
        loaded: &'a Loaded,
        executable: &'a Path,
        binding: &'a Binding,
        clock: &'a RuntimeClock,
    ) -> Result<Self, Error> {
        if !executable.is_absolute()
            || !binding.roles().jmap()
            || loaded.bootstrap.profile() != binding.profile()
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            loaded,
            executable,
            binding,
            clock,
            active: AtomicBool::new(false),
        })
    }
}
struct Permit<'a>(&'a AtomicBool);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Authenticator for Authentication<'_> {
    fn authenticate(&self, password: &[u8], deadline: Deadline) -> Result<Option<DeviceId>, Error> {
        let now = Instant::now();
        let deadline = self.clock.deadline_instant(deadline)?.min(
            now.checked_add(Duration::from_secs(10))
                .ok_or(Error::Invalid)?,
        );
        if now >= deadline {
            return Err(Error::Deadline);
        }
        if Presented::parse(password).is_none() {
            return Ok(None);
        }
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _permit = Permit(&self.active);
        let dns_deadline = deadline.min(
            now.checked_add(Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses =
            postgres_resolver::resolve(self.executable, &self.loaded.bootstrap, dns_deadline)
                .map_err(|error| match error {
                    postgres_resolver::Error::Deadline => Error::Deadline,
                    _ => Error::Dns,
                })?;
        for address in addresses {
            let mut request = self.loaded.bootstrap.request(address);
            request.mode = Mode::Runtime;
            let presented = Presented::parse(password).ok_or(Error::Invalid)?;
            let device = presented.id();
            let binding = self.binding.clone();
            let result = postgres_probe::with_client(
                &request,
                &self.loaded.trust,
                deadline,
                move |client| {
                    Box::pin(
                        async move { postgres_snapshot::begin(client, &binding, presented).await },
                    )
                },
            );
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            match result {
                Ok(Ok(_)) => return Ok(Some(device)),
                Ok(Err(postgres_snapshot::Error::Unauthenticated)) => return Ok(None),
                Ok(Err(_)) => return Err(Error::Invalid),
                Err(postgres_probe::Error::Transport) => continue,
                Err(postgres_probe::Error::Deadline) => return Err(Error::Deadline),
                Err(_) => return Err(Error::Invalid),
            }
        }
        Err(Error::Invalid)
    }
}
