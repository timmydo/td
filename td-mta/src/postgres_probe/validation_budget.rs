//! Finite retained validation admission for an envelope or source body.
use crate::{
    limits::{MAX_MESSAGE_BYTES, OUTBOUND_RECIPIENT_BATCH},
    ports::Error,
    smtp_relay_body::OUTPUT_BYTES,
};

// Additional attempts cover partial writes or Pending writes/flushes, not retries.
const EXTRA_IO_TURNS: u64 = 8;
#[derive(Clone, Copy)]
pub(crate) struct ValidationBudget {
    turns: usize,
    input: usize,
    output: usize,
    frames: usize,
    io: u64,
    records: u64,
}
impl ValidationBudget {
    pub(crate) fn for_envelope(recipients: usize) -> Result<Self, Error> {
        if !(1..=OUTBOUND_RECIPIENT_BATCH).contains(&recipients) {
            return Err(Error::Capacity);
        }
        let commands = u64::try_from(recipients).map_err(|_| Error::Capacity)? + 2;
        Self::from_turns(2 * commands + EXTRA_IO_TURNS)
    }
    pub(crate) fn for_source(length: u64) -> Result<Self, Error> {
        if length > MAX_MESSAGE_BYTES as u64 {
            return Err(Error::Capacity);
        }
        let encoded = length.checked_add(length / 3).ok_or(Error::Capacity)?;
        let cells = encoded.div_ceil(OUTPUT_BYTES as u64);
        // A full plaintext cell needs a later turn to drain its TLS record tail.
        let turns = cells
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .and_then(|n| n.checked_add(EXTRA_IO_TURNS))
            .ok_or(Error::Capacity)?;
        Self::from_turns(turns)
    }
    fn from_turns(turns: u64) -> Result<Self, Error> {
        let input = turns.checked_mul(1024 * 1024).ok_or(Error::Capacity)?;
        let output = turns.checked_mul(64 * 1024).ok_or(Error::Capacity)?;
        let frames = turns.checked_mul(1088).ok_or(Error::Capacity)?;
        let io = turns
            .checked_mul(1024 * 1024 + 128 * 1024)
            .ok_or(Error::Capacity)?;
        let records = turns.checked_mul(4096).ok_or(Error::Capacity)?;
        Ok(Self {
            turns: usize::try_from(turns).map_err(|_| Error::Capacity)?,
            input: usize::try_from(input).map_err(|_| Error::Capacity)?,
            output: usize::try_from(output).map_err(|_| Error::Capacity)?,
            frames: usize::try_from(frames).map_err(|_| Error::Capacity)?,
            io,
            records,
        })
    }
    pub(crate) fn turns(self) -> usize {
        self.turns
    }
    pub(crate) fn input(self) -> usize {
        self.input
    }
    pub(crate) fn output(self) -> usize {
        self.output
    }
    pub(crate) fn frames(self) -> usize {
        self.frames
    }
    pub(crate) fn io_charge(self) -> u64 {
        self.io
    }
    pub(crate) fn record_charge(self) -> u64 {
        self.records
    }
    #[cfg(test)]
    pub(super) fn test_turns(turns: u64) -> Result<Self, Error> {
        Self::from_turns(turns)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn envelope_admission_covers_commands_and_finite_backpressure() {
        assert_eq!(ValidationBudget::for_envelope(1).unwrap().turns(), 14);
        let max = ValidationBudget::for_envelope(100).unwrap();
        assert_eq!(max.turns(), 212);
        assert_eq!(max.input(), 222_298_112);
        assert_eq!(max.output(), 13_893_632);
        assert_eq!(max.frames(), 230_656);
        assert_eq!(max.io_charge(), 250_085_376);
        assert_eq!(max.record_charge(), 868_352);
        for invalid in [0, 101, usize::MAX] {
            assert!(ValidationBudget::for_envelope(invalid).is_err());
        }
    }
    #[test]
    fn source_admission_accounts_for_worst_transparency_and_finite_backpressure() {
        for (length, cells) in [
            (0, 0),
            (2, 1),
            (3, 1),
            (12_288, 1),
            (12_291, 2),
            (64_000, 6),
            (144_000, 12),
            (33_554_432, 2731),
        ] {
            assert_eq!(
                ValidationBudget::for_source(length).unwrap().turns(),
                2 * cells + 1 + 8
            );
        }
        let max = ValidationBudget::for_source(MAX_MESSAGE_BYTES as u64).unwrap();
        assert_eq!(max.turns(), 5471);
        assert_eq!(max.input(), 5_736_759_296);
        assert_eq!(max.output(), 358_547_456);
        assert_eq!(max.frames(), 5_952_448);
        assert_eq!(max.io_charge(), 6_453_854_208);
        assert_eq!(max.record_charge(), 22_409_216);
        assert!(ValidationBudget::for_source(MAX_MESSAGE_BYTES as u64 + 1).is_err());
        assert!(ValidationBudget::for_source(u64::MAX).is_err());
        assert!(ValidationBudget::from_turns(u64::MAX).is_err());
    }
}
