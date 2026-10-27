//! Private, epoch-bound operational progress, not mail authority.
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, Instant},
};
use td_json::{json, Json};
use td_mta::{
    ids::StoreEpoch,
    operator_files::{Inputs, Role},
    postgres_blob_scrub::{Key, Report},
    store_fs::{LockError, LockedRoot, PrivateRoot},
};
type Result<T> = std::result::Result<T, &'static str>;
const MAX_BYTES: u64 = 8192;
pub(super) struct FileState {
    path: PathBuf,
    lock: LockedRoot,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct State {
    pub epoch: StoreEpoch,
    pub active: bool,
    pub started: u64,
    pub completed: u64,
    pub observed: u64,
    pub last_failed: u64,
    pub operation_failed: bool,
    pub scanned: u64,
    pub failed: u64,
    pub bytes: u64,
    pub after: Option<Key>,
    pub through: Option<Key>,
    pub first_failure: Option<Key>,
}
impl State {
    pub fn new(epoch: StoreEpoch) -> Self {
        Self {
            epoch,
            active: false,
            started: 0,
            completed: 0,
            observed: 0,
            last_failed: 0,
            operation_failed: false,
            scanned: 0,
            failed: 0,
            bytes: 0,
            after: None,
            through: None,
            first_failure: None,
        }
    }
    pub fn observe(&mut self, now: u64) -> Result<()> {
        if now < self.observed || now < self.started || now < self.completed {
            return Err("scrub-clock-backward");
        }
        self.observed = now;
        Ok(())
    }
    pub fn begin(&mut self, now: u64) {
        self.observed = now;
        self.active = true;
        self.started = now;
        self.scanned = 0;
        self.failed = 0;
        self.bytes = 0;
        self.after = None;
        self.through = None;
        self.first_failure = None;
    }
    pub fn apply(&mut self, page: Report, now: u64) -> Result<()> {
        self.observe(now)?;
        self.scanned = self
            .scanned
            .checked_add(page.scanned as u64)
            .ok_or("scrub-capacity")?;
        self.failed = self
            .failed
            .checked_add(page.failed as u64)
            .ok_or("scrub-capacity")?;
        self.bytes = self
            .bytes
            .checked_add(page.verified_bytes)
            .ok_or("scrub-capacity")?;
        self.after = page.after;
        self.through = page.through;
        if self.first_failure.is_none() {
            self.first_failure = page.first_failure;
        }
        self.operation_failed = false;
        if page.done {
            self.active = false;
            self.completed = now;
            self.last_failed = self.failed;
        }
        Ok(())
    }
    pub fn healthy(&self, now: u64, age: u64) -> bool {
        now >= self.observed
            && !self.operation_failed
            && self.failed == 0
            && self.last_failed == 0
            && self.completed != 0
            && now.checked_sub(self.completed).is_some_and(|n| n <= age)
    }
    pub fn json(&self) -> Json {
        json!({"version":1,"epoch":self.epoch.to_string(),"active":self.active,"started":self.started,"completed":self.completed,"observed":self.observed,"last_failed":self.last_failed,"operation_failed":self.operation_failed,"scanned":self.scanned,"failed":self.failed,"verified_bytes":self.bytes,"after":key_json(self.after),"through":key_json(self.through),"first_failure":key_json(self.first_failure)})
    }
    fn parse(text: &str, epoch: StoreEpoch) -> Result<Self> {
        let j = td_json::parse(text).map_err(|_| "scrub-state-json")?;
        let number = |k| j.get(k).and_then(Json::as_u64).ok_or("scrub-state-value");
        let boolean = |k| j.get(k).and_then(Json::as_bool).ok_or("scrub-state-value");
        if number("version")? != 1
            || j.get("epoch").and_then(Json::as_str) != Some(epoch.to_string().as_str())
        {
            return Err("scrub-state-epoch-or-version");
        }
        let state = Self {
            epoch,
            active: boolean("active")?,
            started: number("started")?,
            completed: number("completed")?,
            observed: number("observed")?,
            last_failed: number("last_failed")?,
            operation_failed: boolean("operation_failed")?,
            scanned: number("scanned")?,
            failed: number("failed")?,
            bytes: number("verified_bytes")?,
            after: read_key(&j, "after")?,
            through: read_key(&j, "through")?,
            first_failure: read_key(&j, "first_failure")?,
        };
        if state.observed < state.started
            || state.observed < state.completed
            || state.failed > state.scanned
            || state
                .after
                .is_some_and(|k| state.through.is_none_or(|end| k > end))
            || (state.failed == 0) != state.first_failure.is_none()
        {
            return Err("scrub-state-value");
        }
        Ok(state)
    }
}
fn key_json(key: Option<Key>) -> Json {
    key.map(|k| json!(format!("{}:{}", k.account, k.blob)))
        .unwrap_or(Json::Null)
}
fn read_key(j: &Json, name: &str) -> Result<Option<Key>> {
    match j.get(name) {
        Some(Json::Null) => Ok(None),
        Some(v) => super::blob_scrub::parse_key(v.as_str().ok_or("scrub-state-value")?)
            .map(Some)
            .ok_or("scrub-state-value"),
        None => Err("scrub-state-value"),
    }
}
impl FileState {
    pub fn open(path: &str, wait: Duration) -> Result<Self> {
        let until = Instant::now() + wait;
        let lock = loop {
            match PrivateRoot::open(path)
                .map_err(|_| "scrub-state-root")?
                .try_lock()
            {
                Ok(lock) => break lock,
                Err(LockError::Busy) if Instant::now() < until => {
                    std::thread::sleep(
                        Duration::from_millis(100)
                            .min(until.saturating_duration_since(Instant::now())),
                    );
                }
                Err(_) => return Err("scrub-state-locked"),
            }
        };
        let state = Self {
            path: PathBuf::from(path),
            lock,
        };
        for entry in fs::read_dir(path).map_err(|_| "scrub-state-read")? {
            let entry = entry.map_err(|_| "scrub-state-read")?;
            let name = entry.file_name();
            if name == "LOCK" {
                continue;
            }
            if name != "state.json" && name != "partial" {
                return Err("scrub-state-entry");
            }
            let file = state.file(name.to_str().ok_or("scrub-state-entry")?)?;
            if file.metadata().map_err(|_| "scrub-state-read")?.len() > MAX_BYTES {
                return Err("scrub-state-size");
            }
        }
        Ok(state)
    }
    fn file(&self, name: &str) -> Result<File> {
        let mut inputs = Inputs::new();
        inputs
            .bind(self.lock.root())
            .map_err(|_| "scrub-state-owner")?;
        inputs
            .open(
                self.path.join(name).to_str().ok_or("scrub-state-path")?,
                Role::Secret,
            )
            .map_err(|_| "scrub-state-file")
    }
    pub fn load(&self, epoch: StoreEpoch) -> Result<State> {
        match fs::symlink_metadata(self.path.join("state.json")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::new(epoch)),
            Err(_) => return Err("scrub-state-read"),
            Ok(_) => (),
        }
        let mut text = String::new();
        self.file("state.json")?
            .take(MAX_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| "scrub-state-read")?;
        if text.len() as u64 > MAX_BYTES {
            return Err("scrub-state-size");
        }
        State::parse(&text, epoch)
    }
    pub fn save(&self, state: &State) -> Result<()> {
        let text = state.json().to_string();
        if text.len() as u64 > MAX_BYTES {
            return Err("scrub-state-size");
        }
        match fs::symlink_metadata(self.path.join("partial")) {
            Ok(_) => {
                self.file("partial")?;
                fs::remove_file(self.path.join("partial")).map_err(|_| "scrub-state-write")?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("scrub-state-read"),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join("partial"))
            .map_err(|_| "scrub-state-write")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "scrub-state-write")?;
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| "scrub-state-write")?;
        fs::rename(self.path.join("partial"), self.path.join("state.json"))
            .map_err(|_| "scrub-state-write")?;
        File::open(&self.path)
            .and_then(|f| f.sync_all())
            .map_err(|_| "scrub-state-sync")
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use td_mta::ids::{AccountId, BlobId};
    #[test]
    fn failures_survive_pages_and_clear_only_after_a_clean_cycle() {
        let epoch = StoreEpoch::from_bytes([1; 16]);
        let key = Key {
            account: AccountId::from_bytes([2; 16]),
            blob: BlobId::from_bytes([3; 16]),
        };
        let mut state = State::new(epoch);
        state.begin(100);
        let mut page = Report {
            after: Some(key),
            through: Some(key),
            scanned: 1,
            failed: 1,
            first_failure: Some(key),
            verified_bytes: 0,
            done: false,
        };
        state.apply(page, 101).unwrap();
        assert!(!state.healthy(101, 172800));
        page.scanned = 0;
        page.failed = 0;
        page.first_failure = None;
        page.done = true;
        state.apply(page, 102).unwrap();
        assert_eq!(state.last_failed, 1);
        assert_eq!(
            State::parse(&state.json().to_string(), epoch).unwrap(),
            state
        );
        state.begin(200);
        assert!(!state.healthy(200, 172800));
        state.apply(page, 201).unwrap();
        assert!(state.healthy(202, 172800));
        assert!(!state.healthy(200, 172800));
        assert!(!state.healthy(173002, 172800));
        state.operation_failed = true;
        assert!(!state.healthy(202, 172800));
        assert!(State::parse(&state.json().to_string(), StoreEpoch::from_bytes([4; 16])).is_err());
    }
    #[test]
    fn every_observation_prevents_stale_status_becoming_fresh_after_rollback() {
        let epoch = StoreEpoch::from_bytes([1; 16]);
        let mut state = State::new(epoch);
        state.begin(100);
        let page = Report {
            after: None,
            through: None,
            scanned: 0,
            failed: 0,
            first_failure: None,
            verified_bytes: 0,
            done: false,
        };
        state.apply(page, 200).unwrap();
        assert_eq!(state.observe(150), Err("scrub-clock-backward"));
        state.apply(Report { done: true, ..page }, 300).unwrap();
        state.observe(400).unwrap(); // Waiting invocation.
        assert_eq!(state.observe(350), Err("scrub-clock-backward"));
        state.observe(200000).unwrap(); // Stale status observation.
        assert!(!state.healthy(200000, 172800));
        let mut restored = State::parse(&state.json().to_string(), epoch).unwrap();
        assert_eq!(restored.observe(500), Err("scrub-clock-backward"));
        assert!(!restored.healthy(500, 172800));
    }
}
