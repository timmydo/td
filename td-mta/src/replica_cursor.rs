//! Versioned passive source inventory continuation.
use crate::{
    ids::EmailId,
    postgres_read::ReplicaInventoryCursor,
    sync::{DataState, DataType},
    wire::{self, Error},
};

impl ReplicaInventoryCursor {
    pub const WIRE_BYTES: usize = 153;

    pub fn encode(self, output: &mut [u8]) -> Result<&str, Error> {
        if self.base.kind != DataType::Email || self.after.as_bytes() > self.upper.as_bytes() {
            return Err(Error::Syntax);
        }
        let output = output
            .get_mut(..Self::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        let (prefix, rest) = output.split_at_mut_checked(4).ok_or(Error::OutputFull)?;
        prefix.copy_from_slice(b"ri1_");
        let (base, rest) = rest.split_at_mut_checked(85).ok_or(Error::OutputFull)?;
        self.base.encode(base)?;
        let (upper, after) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        wire::hex(self.upper.as_bytes(), upper)?;
        wire::hex(self.after.as_bytes(), after)?;
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }

    pub fn decode(value: &str) -> Result<Self, Error> {
        if value.len() != Self::WIRE_BYTES {
            return Err(Error::Syntax);
        }
        let (prefix, rest) = value.as_bytes().split_at_checked(4).ok_or(Error::Syntax)?;
        if prefix != b"ri1_" {
            return Err(Error::Syntax);
        }
        let (base, rest) = rest.split_at_checked(85).ok_or(Error::Syntax)?;
        let base = DataState::decode(std::str::from_utf8(base).map_err(|_| Error::Syntax)?)?;
        let (upper, after) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        let result = Self {
            base,
            upper: EmailId::from_bytes(wire::unhex(upper)?),
            after: EmailId::from_bytes(wire::unhex(after)?),
        };
        if result.base.kind != DataType::Email || result.after.as_bytes() > result.upper.as_bytes()
        {
            return Err(Error::Syntax);
        }
        Ok(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        format::Sequence,
        ids::{AccountId, StoreEpoch},
    };
    const GOLDEN: &str = concat!(
        "ri1_d1_03",
        "01010101010101010101010101010101",
        "02020202020202020202020202020202",
        "000000000000000a",
        "04040404040404040404040404040404",
        "03030303030303030303030303030303",
    );

    #[test]
    fn inventory_cursor_has_fixed_versioned_scope_and_bounds() {
        let cursor = ReplicaInventoryCursor {
            base: DataState {
                account: AccountId::from_bytes([1; 16]),
                epoch: StoreEpoch::from_bytes([2; 16]),
                kind: DataType::Email,
                sequence: Sequence::from_u64(10),
            },
            upper: EmailId::from_bytes([4; 16]),
            after: EmailId::from_bytes([3; 16]),
        };
        let mut bytes = [0; ReplicaInventoryCursor::WIRE_BYTES];
        assert_eq!(cursor.encode(&mut bytes).unwrap(), GOLDEN);
        assert_eq!(ReplicaInventoryCursor::decode(GOLDEN).unwrap(), cursor);
        assert_eq!(cursor.encode(&mut bytes[..152]), Err(Error::OutputFull));
        for end in 0..GOLDEN.len() {
            assert_eq!(
                ReplicaInventoryCursor::decode(&GOLDEN[..end]),
                Err(Error::Syntax)
            );
            let mut damaged = GOLDEN.as_bytes().to_vec();
            *damaged.get_mut(end).unwrap() = b'!';
            assert_eq!(
                ReplicaInventoryCursor::decode(std::str::from_utf8(&damaged).unwrap()),
                Err(Error::Syntax)
            );
        }
        assert_eq!(
            ReplicaInventoryCursor::decode(&format!("{GOLDEN}0")),
            Err(Error::Syntax)
        );
        let wrong_kind = ReplicaInventoryCursor {
            base: DataState {
                kind: DataType::Mailbox,
                ..cursor.base
            },
            ..cursor
        };
        assert_eq!(wrong_kind.encode(&mut bytes), Err(Error::Syntax));
        let reversed = ReplicaInventoryCursor {
            after: cursor.upper,
            upper: cursor.after,
            ..cursor
        };
        assert_eq!(reversed.encode(&mut bytes), Err(Error::Syntax));
    }
}
