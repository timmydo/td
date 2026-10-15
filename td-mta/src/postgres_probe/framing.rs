//! Bound input before the driver's codec allocates or authenticates.
pub(super) struct Input {
    prefix: [u8; 9],
    used: usize,
    target: usize,
    body: usize,
    bytes: usize,
    frames: usize,
    authentication: u8,
    failed: bool,
}

impl Input {
    pub(super) fn new() -> Self {
        Self {
            prefix: [0; 9],
            used: 0,
            target: 5,
            body: 0,
            bytes: 1024 * 1024,
            frames: 128,
            authentication: 0,
            failed: false,
        }
    }

    pub(super) fn schema() -> Self {
        Self {
            frames: 1024,
            ..Self::new()
        }
    }

    pub(super) fn mailboxes() -> Self {
        Self {
            frames: 512,
            ..Self::new()
        }
    }

    pub(super) fn mutations() -> Self {
        Self {
            frames: 1024,
            ..Self::new()
        }
    }

    pub(super) fn runtime(self) -> Self {
        Self {
            frames: self.frames.max(1024),
            ..self
        }
    }

    pub(super) fn body() -> Self {
        Self {
            bytes: 128 * 1024 * 1024,
            frames: 8192,
            ..Self::new()
        }
    }

    pub(super) fn configuration() -> Self {
        Self {
            bytes: 4 * 1024 * 1024,
            frames: 512,
            ..Self::new()
        }
    }

    pub(super) fn accept(&mut self, input: &[u8]) -> Result<(), ()> {
        if self.failed {
            return Err(());
        }
        let result = self.consume(input);
        self.failed = result.is_err();
        result
    }

    fn consume(&mut self, mut input: &[u8]) -> Result<(), ()> {
        self.bytes = self.bytes.checked_sub(input.len()).ok_or(())?;
        while !input.is_empty() {
            if self.body != 0 {
                let take = self.body.min(input.len());
                self.body -= take;
                input = input.get(take..).ok_or(())?;
                continue;
            }
            let take = (self.target - self.used).min(input.len());
            self.prefix
                .get_mut(self.used..self.used + take)
                .ok_or(())?
                .copy_from_slice(input.get(..take).ok_or(())?);
            self.used += take;
            input = input.get(take..).ok_or(())?;
            if self.used != self.target {
                continue;
            }
            let length = u32::from_be_bytes(
                self.prefix
                    .get(1..5)
                    .ok_or(())?
                    .try_into()
                    .map_err(|_| ())?,
            ) as usize;
            if self.target == 5 {
                self.frames = self.frames.checked_sub(1).ok_or(())?;
                if !(4..=128 * 1024).contains(&length) {
                    return Err(());
                }
                match self.prefix.first() {
                    Some(b'R') => {
                        if length < 8 {
                            return Err(());
                        }
                        self.target = 9;
                        continue;
                    }
                    Some(b'Z') if self.authentication != 4 => return Err(()),
                    _ => {}
                }
                self.body = length - 4;
            } else {
                let method = u32::from_be_bytes(
                    self.prefix
                        .get(5..9)
                        .ok_or(())?
                        .try_into()
                        .map_err(|_| ())?,
                );
                self.authentication = match (self.authentication, method) {
                    (0, 10) => 1,
                    (1, 11) => 2,
                    (2, 12) => 3,
                    (3, 0) => 4,
                    _ => return Err(()),
                };
                self.body = length - 8;
            }
            self.target = 5;
            self.used = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    fn authenticated() -> Vec<u8> {
        let mut bytes = Vec::new();
        for method in [10u32, 11, 12, 0] {
            bytes.extend(frame(b'R', &method.to_be_bytes()));
        }
        bytes.extend(frame(b'Z', b"I"));
        bytes
    }

    #[test]
    fn framing_preserves_all_fragmentation_boundaries() {
        let mut bytes = authenticated();
        bytes.extend(frame(b'D', &[42; 65536]));
        bytes.extend(frame(b'Z', b"I"));
        for width in [1, 2, 4, 5, 7, 9, 1024, 16384] {
            let mut input = Input::new();
            for chunk in bytes.chunks(width) {
                assert!(input.accept(chunk).is_ok());
            }
            assert_eq!(input.body, 0);
            assert_eq!(input.used, 0);
            assert_eq!(input.authentication, 4);
        }
    }

    #[test]
    fn legacy_trust_and_out_of_order_authentication_are_terminal() {
        for method in [0u32, 2, 3, 5, 7, 8, 9, 11, 12, u32::MAX] {
            let bytes = frame(b'R', &method.to_be_bytes());
            for split in 0..bytes.len() {
                let mut input = Input::new();
                assert!(input.accept(&bytes[..split]).is_ok());
                assert!(input.accept(&bytes[split..]).is_err());
                assert!(input.accept(&authenticated()).is_err());
            }
        }
        assert!(Input::new().accept(&frame(b'Z', b"I")).is_err());
        let mut input = Input::new();
        assert!(input.accept(&authenticated()).is_ok());
        assert!(input.accept(&frame(b'R', &10u32.to_be_bytes())).is_err());
    }

    #[test]
    fn body_budget_is_finite_and_preserves_frame_and_authentication_limits() {
        let mut input = Input::body();
        for _ in 0..8192 {
            assert!(input.accept(&frame(b'N', b"\0")).is_ok());
        }
        assert!(input.accept(&frame(b'N', b"\0")).is_err());
        assert!(input.accept(&[]).is_err());
        assert!(Input::body().accept(&frame(b'Z', b"I")).is_err());
        assert!(Input::body()
            .accept(&frame(b'D', &[0; 128 * 1024]))
            .is_err());
        let packet = frame(b'D', &[0; 128 * 1024 - 4]);
        let mut input = Input::body();
        for _ in 0..1023 {
            assert!(input.accept(&packet).is_ok());
        }
        assert!(input.accept(&packet).is_err());
    }

    #[test]
    fn mailbox_budget_is_finite_and_keeps_authentication_and_frame_bounds() {
        let mut input = Input::mailboxes();
        for _ in 0..512 {
            assert!(input.accept(&frame(b'N', b"\0")).is_ok());
        }
        assert!(input.accept(&frame(b'N', b"\0")).is_err());
        assert!(input.accept(&[]).is_err());
        assert!(Input::mailboxes().accept(&frame(b'Z', b"I")).is_err());
        assert!(Input::mailboxes()
            .accept(&frame(b'D', &[0; 128 * 1024]))
            .is_err());
        let mut input = Input::mailboxes();
        for _ in 0..7 {
            assert!(input.accept(&frame(b'D', &[0; 128 * 1024 - 4])).is_ok());
        }
        assert!(input.accept(&frame(b'D', &[0; 128 * 1024 - 4])).is_err());
    }

    #[test]
    fn mutation_budget_is_finite_and_keeps_authentication_and_byte_limits() {
        let mut input = Input::mutations();
        for _ in 0..1024 {
            assert!(input.accept(&frame(b'N', b"\0")).is_ok());
        }
        assert!(input.accept(&frame(b'N', b"\0")).is_err());
        assert!(input.accept(&[]).is_err());
        assert!(Input::mutations().accept(&frame(b'Z', b"I")).is_err());
        assert!(Input::mutations()
            .accept(&frame(b'D', &[0; 128 * 1024]))
            .is_err());
        let mut input = Input::mutations();
        let packet = frame(b'D', &[0; 128 * 1024 - 4]);
        for _ in 0..7 {
            assert!(input.accept(&packet).is_ok());
        }
        assert!(input.accept(&packet).is_err());
    }

    #[test]
    fn schema_command_budget_is_finite_and_keeps_other_input_limits() {
        let mut input = Input::schema();
        for _ in 0..1024 {
            assert!(input.accept(&frame(b'N', b"\0")).is_ok());
        }
        assert!(input.accept(&frame(b'N', b"\0")).is_err());
        assert!(input.accept(&[]).is_err());
        assert!(Input::schema().accept(&frame(b'Z', b"I")).is_err());
        let mut input = Input::schema();
        let large = frame(b'D', &vec![0; 128 * 1024 - 4]);
        for _ in 0..7 {
            assert!(input.accept(&large).is_ok());
        }
        assert!(input.accept(&large).is_err());
    }

    #[test]
    fn configuration_budget_keeps_authentication_and_per_frame_limits() {
        let mut input = Input::configuration();
        for _ in 0..512 {
            assert!(input.accept(&frame(b'N', b"\0")).is_ok());
        }
        assert!(input.accept(&frame(b'N', b"\0")).is_err());
        assert!(input.accept(&[]).is_err());
        assert!(Input::configuration().accept(&frame(b'Z', b"I")).is_err());
        assert!(Input::configuration()
            .accept(&frame(b'D', &vec![0; 128 * 1024]))
            .is_err());
        let mut input = Input::configuration();
        let large = frame(b'D', &vec![0; 128 * 1024 - 4]);
        for _ in 0..31 {
            assert!(input.accept(&large).is_ok());
        }
        assert!(input.accept(&large).is_err());
        assert!(input.accept(&[]).is_err());
    }

    #[test]
    fn lengths_frame_counts_and_aggregate_bytes_are_bounded() {
        for length in [0u32, 1, 2, 3, 128 * 1024 + 1, u32::MAX] {
            let mut bytes = vec![b'D'];
            bytes.extend(length.to_be_bytes());
            assert!(Input::new().accept(&bytes).is_err());
        }
        assert!(Input::new().accept(&frame(b'R', &[0; 3])).is_err());
        let mut input = Input::new();
        for _ in 0..128 {
            assert!(input.accept(&frame(b'N', b"\0")).is_ok());
        }
        assert!(input.accept(&frame(b'N', b"\0")).is_err());
        let mut input = Input::new();
        let large = frame(b'D', &vec![0; 128 * 1024 - 4]);
        for _ in 0..7 {
            assert!(input.accept(&large).is_ok());
        }
        assert!(input.accept(&large).is_err());
        assert!(input.accept(&[]).is_err());
    }
}
