//! Search preparation progress is operational evidence, never mail authority.
use td_json::{json, Json};
use td_mta::{
    ids::StoreEpoch,
    postgres_search_prepare::{Key, Report, VERSION},
};

type Result<T> = std::result::Result<T, &'static str>;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct State {
    pub epoch: StoreEpoch,
    pub active: bool,
    pub in_flight: bool,
    pub operation_failed: bool,
    pub observed: u64,
    pub started: u64,
    pub completed: u64,
    pub scanned: u64,
    pub written: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub after: Option<Key>,
    pub through: Option<Key>,
}
impl State {
    pub fn new(epoch: StoreEpoch) -> Self {
        Self {
            epoch,
            active: false,
            in_flight: false,
            operation_failed: false,
            observed: 0,
            started: 0,
            completed: 0,
            scanned: 0,
            written: 0,
            skipped: 0,
            bytes: 0,
            after: None,
            through: None,
        }
    }
    pub fn observe(&mut self, now: u64) -> Result<()> {
        if now < self.observed || now < self.started || now < self.completed {
            return Err("search-clock-backward");
        }
        self.observed = now;
        Ok(())
    }
    pub fn begin(&mut self, now: u64) -> Result<()> {
        self.observe(now)?;
        if self.in_flight {
            return Err("search-commit-unknown");
        }
        self.active = true;
        self.started = now;
        self.scanned = 0;
        self.written = 0;
        self.skipped = 0;
        self.bytes = 0;
        self.after = None;
        self.through = None;
        Ok(())
    }
    pub fn apply(&mut self, page: Report, now: u64) -> Result<()> {
        self.observe(now)?;
        if !self.active || !self.in_flight {
            return Err("search-state-value");
        }
        self.scanned = self
            .scanned
            .checked_add(page.scanned as u64)
            .ok_or("search-state-capacity")?;
        self.written = self
            .written
            .checked_add(page.written as u64)
            .ok_or("search-state-capacity")?;
        self.skipped = self
            .skipped
            .checked_add(page.skipped as u64)
            .ok_or("search-state-capacity")?;
        self.bytes = self
            .bytes
            .checked_add(page.verified_bytes)
            .ok_or("search-state-capacity")?;
        self.after = page.after;
        self.through = page.through;
        self.in_flight = false;
        self.operation_failed = false;
        if page.done {
            self.active = false;
            self.completed = now;
        }
        Ok(())
    }
    pub fn healthy(&self, now: u64) -> bool {
        !self.in_flight
            && !self.operation_failed
            && now >= self.observed
            && self.completed != 0
            && now.checked_sub(self.completed).is_some_and(|n| n <= 86400)
    }
    pub fn json(&self) -> Json {
        let key = |v: Option<Key>| {
            v.map(|k| json!(format!("{}:{}", k.account, k.blob)))
                .unwrap_or(Json::Null)
        };
        json!({"version":1,"search_version":VERSION,"epoch":self.epoch.to_string(),
            "active":self.active,"in_flight":self.in_flight,"operation_failed":self.operation_failed,
            "observed":self.observed,"started":self.started,"completed":self.completed,
            "scanned":self.scanned,"written":self.written,"skipped":self.skipped,
            "verified_bytes":self.bytes,"after":key(self.after),"through":key(self.through)})
    }
    pub fn parse(text: &str, epoch: StoreEpoch) -> Result<Self> {
        let value = td_json::parse(text).map_err(|_| "search-state-json")?;
        let number = |key| {
            value
                .get(key)
                .and_then(Json::as_u64)
                .ok_or("search-state-value")
        };
        let boolean = |key| {
            value
                .get(key)
                .and_then(Json::as_bool)
                .ok_or("search-state-value")
        };
        let key = |name| match value.get(name) {
            Some(Json::Null) => Ok(None),
            Some(v) => super::blob_scrub::parse_key(v.as_str().ok_or("search-state-value")?)
                .map(Some)
                .ok_or("search-state-value"),
            None => Err("search-state-value"),
        };
        if number("version")? != 1
            || number("search_version")? != u64::from(VERSION)
            || value.get("epoch").and_then(Json::as_str) != Some(epoch.to_string().as_str())
        {
            return Err("search-state-epoch-or-version");
        }
        let state = Self {
            epoch,
            active: boolean("active")?,
            in_flight: boolean("in_flight")?,
            operation_failed: boolean("operation_failed")?,
            observed: number("observed")?,
            started: number("started")?,
            completed: number("completed")?,
            scanned: number("scanned")?,
            written: number("written")?,
            skipped: number("skipped")?,
            bytes: number("verified_bytes")?,
            after: key("after")?,
            through: key("through")?,
        };
        if state.observed < state.started
            || state.observed < state.completed
            || state.written.checked_add(state.skipped) != Some(state.scanned)
            || (state.in_flight && !state.active)
            || state
                .after
                .is_some_and(|k| state.through.is_none_or(|end| k > end))
        {
            return Err("search-state-value");
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn uncertain_progress_survives_restart_and_skips_do_not_claim_completeness() {
        let epoch = StoreEpoch::from_bytes([1; 16]);
        let mut state = State::new(epoch);
        state.begin(100).unwrap();
        state.in_flight = true;
        let mut restored = State::parse(&state.json().to_string(), epoch).unwrap();
        assert_eq!(restored.begin(101), Err("search-commit-unknown"));
        assert!(!restored.healthy(101));
        restored
            .apply(
                Report {
                    after: None,
                    through: None,
                    scanned: 1,
                    written: 0,
                    skipped: 1,
                    verified_bytes: 8,
                    done: true,
                },
                102,
            )
            .unwrap();
        assert!(restored.healthy(103));
        assert_eq!(restored.skipped, 1);
        restored.observe(200000).unwrap();
        assert!(!restored.healthy(200000));
        assert_eq!(restored.observe(104), Err("search-clock-backward"));
        assert!(State::parse(
            &restored.json().to_string(),
            StoreEpoch::from_bytes([2; 16])
        )
        .is_err());
        let wrong = restored
            .json()
            .to_string()
            .replace("\"search_version\":1", "\"search_version\":2");
        assert!(State::parse(&wrong, epoch).is_err());
    }
}
