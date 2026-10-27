//! Bounded reuse for fully consumed, counted HTTP exchanges.
use super::{Method, Policy, PolicyResolver, Request, WORKERS};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const IDLE: Duration = Duration::from_secs(4);
const LIFETIME: Duration = Duration::from_secs(60);
const REQUESTS: usize = 16;

pub(super) fn can_reuse(request: &Request) -> bool {
    request
        .url
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("https"))
        && !request.public
        && !request.stream
        && match request.method {
            Method::Get => request.redirects == Some(0),
            Method::Post => !request.body.is_empty(),
        }
}

#[derive(PartialEq, Eq)]
struct Key {
    roots: Option<Vec<u8>>,
    redirects: u32,
    loopback: bool,
    budget: Duration,
}
impl Key {
    fn for_request(request: &Request, policy: Policy, budget: Duration) -> Option<Self> {
        // Public requests must resample local networks. Streams retain their
        // process-owned idle deadlines. ureq 3 resends nothing on a pooled
        // connection; an empty POST, possibly a complete mutation, stays off
        // one regardless.
        if !can_reuse(request) {
            return None;
        }
        Some(Self {
            roots: request.tls_ca.clone(),
            redirects: match request.method {
                Method::Get => request.redirects.unwrap_or(super::MAX_REDIRECTS),
                Method::Post => 0,
            },
            loopback: policy.allow_loopback,
            budget,
        })
    }
}

pub(super) struct Agent {
    pub agent: ureq::Agent,
    pub resolver: Arc<PolicyResolver>,
}

struct Cached {
    key: Key,
    agent: Agent,
    born: Instant,
    idle: Instant,
    requests: usize,
}
impl Cached {
    fn reusable(&self, now: Instant) -> bool {
        self.requests < REQUESTS
            && now
                .checked_duration_since(self.born)
                .is_some_and(|age| age < LIFETIME)
            && now
                .checked_duration_since(self.idle)
                .is_some_and(|age| age < IDLE)
    }
}

#[derive(Default)]
pub(super) struct Pool {
    idle: Mutex<Vec<Cached>>,
}

/// Exclusively owns an agent until its response has been completely consumed.
/// Drop discards it; only an explicitly successful exchange may return it.
pub(super) struct Lease {
    agent: Agent,
    reuse: Option<(Key, Instant, usize)>,
}
impl Lease {
    pub fn agent(&self) -> &Agent {
        &self.agent
    }
}
impl Pool {
    pub fn acquire(
        &self,
        request: &Request,
        policy: Policy,
        budget: Duration,
        now: Instant,
        build: impl FnOnce() -> Result<Agent, super::Fault>,
    ) -> Result<Lease, super::Fault> {
        let key = Key::for_request(request, policy, budget);
        if let Some(key) = key.as_ref() {
            if let Ok(mut entries) = self.idle.lock() {
                entries.retain(|entry| entry.reusable(now));
                if let Some(index) = entries.iter().rposition(|entry| entry.key == *key) {
                    // The agent's whole-exchange deadline starts with each
                    // call, so only the last refusal is cleared.
                    let entry = entries.swap_remove(index);
                    *entry
                        .agent
                        .resolver
                        .refusal
                        .lock()
                        .map_err(|_| super::Fault::Transport("resolver unavailable".into()))? =
                        None;
                    return Ok(Lease {
                        agent: entry.agent,
                        reuse: Some((entry.key, entry.born, entry.requests)),
                    });
                }
            }
        }
        let agent = build()?;
        Ok(Lease {
            agent,
            reuse: key.map(|key| (key, now, 0)),
        })
    }

    pub fn completed(&self, lease: Lease, now: Instant) {
        let Some((key, born, requests)) = lease.reuse else {
            return;
        };
        let entry = Cached {
            key,
            agent: lease.agent,
            born,
            idle: now,
            requests: requests.saturating_add(1),
        };
        if !entry.reusable(now) {
            return;
        }
        if let Ok(mut entries) = self.idle.lock() {
            entries.retain(|entry| entry.reusable(now));
            if entries.len() >= WORKERS {
                entries.remove(0);
            }
            entries.push(entry);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;
    const POLICY: Policy = Policy {
        allow_loopback: true,
    };
    fn request() -> Request {
        Request {
            tls_ca: None,
            method: Method::Get,
            url: "https://localhost/".into(),
            headers: Vec::new(),
            limit: 10,
            redirects: Some(0),
            stream: false,
            public: false,
            body: Vec::new(),
        }
    }
    fn acquire(pool: &Pool, request: &Request, now: Instant, builds: &Cell<usize>) -> Lease {
        pool.acquire(request, POLICY, Duration::from_secs(60), now, || {
            builds.set(builds.get() + 1);
            Ok(Agent {
                agent: ureq::Agent::new_with_defaults(),
                resolver: Arc::new(PolicyResolver {
                    policy: POLICY,
                    public: None,
                    refusal: Mutex::new(None),
                }),
            })
        })
        .unwrap()
    }
    #[test]
    fn exclusive_checkout_successful_return_and_trust_separation() {
        let pool = Pool::default();
        let builds = Cell::new(0);
        let now = Instant::now();
        let request = request();
        let first = acquire(&pool, &request, now, &builds);
        let concurrent = acquire(&pool, &request, now, &builds);
        assert_eq!(builds.get(), 2);
        drop(concurrent); // An incomplete response must not return an agent.
        pool.completed(first, now);
        let reused = acquire(&pool, &request, now, &builds);
        assert_eq!(builds.get(), 2);
        pool.completed(reused, now);
        let mut changed = request;
        changed.tls_ca = Some(b"distinct-root-policy".to_vec());
        let different = acquire(&pool, &changed, now, &builds);
        assert_eq!(builds.get(), 3);
        drop(different);
        let again = acquire(&pool, &changed, now, &builds);
        assert_eq!(builds.get(), 4);
        drop(again);
    }
    #[test]
    fn retirement_uses_injected_time_and_request_counts() {
        let pool = Pool::default();
        let builds = Cell::new(0);
        let now = Instant::now();
        let request = request();
        for _ in 0..16 {
            let lease = acquire(&pool, &request, now, &builds);
            assert_eq!(builds.get(), 1);
            pool.completed(lease, now);
        }
        let lease = acquire(&pool, &request, now, &builds);
        assert_eq!(builds.get(), 2);
        pool.completed(lease, now);
        let lease = acquire(&pool, &request, now + Duration::from_secs(4), &builds);
        assert_eq!(builds.get(), 3);
        // Artificially model a long exchange completing near the lifetime cap.
        pool.completed(lease, now + Duration::from_secs(63));
        let lease = acquire(&pool, &request, now + Duration::from_secs(64), &builds);
        assert_eq!(builds.get(), 4);
        drop(lease);
    }
}
