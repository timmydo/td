//! Admission for JMAP input and dispatch-owned heap, through their final owner.
//! Response files, TLS and PostgreSQL have separate contracts.
use crate::{limits::ResourcePlan, ports::Error};
use td_alloc::{Budget, Buffer};
use td_json::arena::Token;

/// Shared by every account/origin served by the same HTTPS worker pool.
pub struct RequestMemory {
    budget: Budget,
    bytes: usize,
    tokens: usize,
    methods: usize,
    request_limit: usize,
}
impl RequestMemory {
    pub fn new(resources: &ResourcePlan) -> Result<Self, Error> {
        let limits = resources.limits();
        let request_limit = limits
            .json_tokens
            .checked_mul(std::mem::size_of::<Token>())
            .and_then(|n| n.checked_add(limits.json_bytes))
            .and_then(|n| n.checked_add(limits.jmap_method_bytes))
            .and_then(|n| n.checked_add(Budget::metadata_bytes()))
            .ok_or(Error::Capacity)?;
        let maximum = request_limit
            .checked_mul(resources.https_body_workers())
            .and_then(|n| n.checked_add(Budget::metadata_bytes()))
            .ok_or(Error::Capacity)?;
        Ok(Self {
            budget: Budget::new(maximum).map_err(|_| Error::Capacity)?,
            bytes: limits.json_bytes,
            tokens: limits.json_tokens,
            methods: limits.jmap_method_bytes,
            request_limit,
        })
    }
    pub fn used(&self) -> usize {
        self.budget.used()
    }
    pub fn limit(&self) -> usize {
        self.budget.limit()
    }

    pub(crate) fn allocate(
        &self,
        resources: &ResourcePlan,
    ) -> Result<(Buffer<u8>, Buffer<Token>), Error> {
        if self.bytes != resources.limits().json_bytes
            || self.tokens != resources.limits().json_tokens
            || self.methods != resources.limits().jmap_method_bytes
        {
            return Err(Error::Capacity);
        }
        let request = self
            .budget
            .child(self.request_limit)
            .map_err(|_| Error::Capacity)?;
        let bytes = Buffer::filled(&request, self.bytes, 0u8).map_err(|_| Error::Capacity)?;
        let tokens =
            Buffer::filled(&request, self.tokens, Token::EMPTY).map_err(|_| Error::Capacity)?;
        Ok((bytes, tokens))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::limits::Limits;

    #[test]
    fn allocator_refusal_returns_every_partial_charge() {
        let resources = Limits::default().plan().unwrap();
        let memory = RequestMemory::new(&resources).unwrap();
        for allowance in 0..3 {
            assert!(
                td_alloc::with_reservation_limit(allowance, || memory.allocate(&resources))
                    .is_err()
            );
            assert_eq!(memory.used(), Budget::metadata_bytes());
        }
        let buffers = td_alloc::with_reservation_limit(3, || memory.allocate(&resources)).unwrap();
        drop(buffers);
        assert_eq!(memory.used(), Budget::metadata_bytes());
    }

    #[test]
    fn retained_request_storage_cannot_replenish_the_worker_pool() {
        let resources = Limits::default().plan().unwrap();
        let memory = RequestMemory::new(&resources).unwrap();
        let mut held = Vec::new();
        for _ in 0..resources.https_body_workers() {
            let buffers = memory.allocate(&resources).unwrap();
            let method_storage = td_alloc::Buffer::filled(
                buffers.0.budget(),
                resources.limits().jmap_method_bytes,
                0u8,
            )
            .unwrap();
            held.push((buffers, method_storage));
        }
        assert_eq!(memory.used(), memory.limit());
        assert!(memory.allocate(&resources).is_err());
        drop(held.pop());
        drop(memory.allocate(&resources).unwrap());
        drop(held);
        assert_eq!(memory.used(), Budget::metadata_bytes());
    }

    #[test]
    fn method_storage_cannot_borrow_another_requests_allowance() {
        let resources = Limits::default().plan().unwrap();
        let memory = RequestMemory::new(&resources).unwrap();
        let (bytes, tokens) = memory.allocate(&resources).unwrap();
        let backing = memory.used();
        let methods = resources.limits().jmap_method_bytes;
        let scratch = Buffer::filled(bytes.budget(), methods, 0u8).unwrap();
        assert!(memory.used() < memory.limit());
        assert!(Buffer::filled(bytes.budget(), 1, 0u8).is_err());
        assert_eq!(memory.used(), backing + methods);
        drop((bytes, tokens));
        // A moved-out method owner retains the request child after cancellation.
        assert_eq!(memory.used(), 2 * Budget::metadata_bytes() + methods);
        drop(scratch);
        assert_eq!(memory.used(), Budget::metadata_bytes());
        drop(memory.allocate(&resources).unwrap());
        assert_eq!(memory.used(), Budget::metadata_bytes());
    }

    #[test]
    fn changed_method_allowance_requires_a_matching_pool() {
        let resources = Limits::default().plan().unwrap();
        let memory = RequestMemory::new(&resources).unwrap();
        let changed = Limits {
            jmap_method_bytes: resources.limits().jmap_method_bytes / 2,
            ..Limits::default()
        }
        .plan()
        .unwrap();
        assert!(memory.allocate(&changed).is_err());
        assert_eq!(memory.used(), Budget::metadata_bytes());
    }

    #[test]
    fn parsing_validation_and_recovered_backing_keep_admission() {
        let resources = Limits::default().plan().unwrap();
        let memory = RequestMemory::new(&resources).unwrap();
        let (mut bytes, tokens) = memory.allocate(&resources).unwrap();
        let input = br#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[]}"#;
        bytes.get_mut(..input.len()).unwrap().copy_from_slice(input);
        let limits = resources.limits();
        let mut cursor = td_json::arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            input.len(),
            td_json::arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_tokens: limits.json_tokens,
                max_depth: limits.json_depth,
            },
        )
        .unwrap();
        let charged = memory.used();
        while !cursor.step().unwrap() {}
        let document = cursor.finish().unwrap();
        let mut validation = crate::jmap_request::OwnedValidation::new(
            document,
            &resources,
            &[crate::jmap_request::CORE],
        )
        .unwrap();
        while !validation.step().unwrap() {}
        let request = validation.finish().unwrap();
        assert_eq!(memory.used(), charged);
        let mut preflight =
            crate::jmap_request::response::Preflight::new(request, "state", 65536).unwrap();
        let mut output = [0; 256];
        while !preflight.step(&mut output).unwrap() {}
        let mut writer = preflight.finish().unwrap().fallback().unwrap();
        while !writer.poll(&mut output).unwrap().complete {
            assert_eq!(memory.used(), charged);
        }
        let request = writer.finish().unwrap();
        assert_eq!(memory.used(), charged);
        let (bytes, tokens) = request.into_buffers();
        assert_eq!(memory.used(), charged);
        drop(bytes);
        assert!(memory.used() > Budget::metadata_bytes());
        drop(tokens);
        assert_eq!(memory.used(), Budget::metadata_bytes());
    }

    #[test]
    fn parser_cancellation_and_malformed_input_release_admission() {
        let resources = Limits::default().plan().unwrap();
        let memory = RequestMemory::new(&resources).unwrap();
        for complete in [false, true] {
            let (mut bytes, tokens) = memory.allocate(&resources).unwrap();
            bytes.get_mut(..3).unwrap().copy_from_slice(b"[x]");
            let limits = resources.limits();
            let mut cursor = td_json::arena::OwnedCursor::with_input_length(
                bytes,
                tokens,
                3,
                td_json::arena::ParseLimits {
                    max_bytes: limits.json_bytes,
                    max_tokens: limits.json_tokens,
                    max_depth: limits.json_depth,
                },
            )
            .unwrap();
            if complete {
                assert!(cursor.step().is_err());
            }
            drop(cursor);
            assert_eq!(memory.used(), Budget::metadata_bytes());
        }
    }
}
