use super::*;
fn identity() -> ViewIdentity {
    ViewIdentity {
        account: AccountId::from_bytes([0x11; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
        committed_sequence: Sequence::from_u64(0x1234),
        history_floor: Sequence::default(),
    }
}
#[test]
fn literal_query_states_use_policy_tags_and_current_version() -> Result<(), Error> {
    let mut output = [0; QueryState::WIRE_BYTES];
    for (kind, tag) in [
        (QueryType::Mailbox, "01"),
        (QueryType::Email, "03"),
        (QueryType::Submission, "05"),
    ] {
        let state = QueryState::current(identity(), kind);
        let expected = format!(
            "q1_{tag}1111111111111111111111111111111122222222222222222222222222222222000000000000123400000001"
        );
        assert_eq!(state.encode(&mut output)?, expected);
        assert_eq!(QueryState::decode(&expected)?, state);
        assert_eq!(DataState::decode(&expected), Err(Error::Syntax));
        assert_eq!(
            DataState::decode(expected.get(..DataState::WIRE_BYTES).ok_or(Error::Syntax)?),
            Err(Error::Syntax)
        );
        assert_eq!(
            super::super::IdentityState::decode(&expected),
            Err(Error::Syntax)
        );
    }
    Ok(())
}
#[test]
fn every_short_buffer_is_unchanged_and_larger_tail_is_untouched() -> Result<(), Error> {
    let state = QueryState::current(identity(), QueryType::Email);
    let mut full = [0; QueryState::WIRE_BYTES];
    let expected = state.encode(&mut full)?;
    for length in 0..QueryState::WIRE_BYTES {
        let mut short = vec![0xa5; length];
        assert_eq!(state.encode(&mut short), Err(Error::OutputFull));
        assert!(short.iter().all(|byte| *byte == 0xa5));
        assert_eq!(
            QueryState::decode(expected.get(..length).ok_or(Error::Syntax)?),
            Err(Error::Syntax)
        );
    }
    let mut extra = [0xa5; QueryState::WIRE_BYTES + 3];
    assert_eq!(state.encode(&mut extra)?, expected);
    assert_eq!(
        extra.get(QueryState::WIRE_BYTES..),
        Some([0xa5; 3].as_slice())
    );
    assert_eq!(
        QueryState::decode(&format!("{expected}0")),
        Err(Error::Syntax)
    );
    Ok(())
}
#[test]
fn syntax_rejects_non_query_types_prefixes_and_noncanonical_hex() -> Result<(), Error> {
    let state = QueryState {
        interpretation: 0xabcdef01,
        ..QueryState::current(identity(), QueryType::Email)
    };
    let mut full = [0; QueryState::WIRE_BYTES];
    let expected = state.encode(&mut full)?;
    for invalid in [
        expected.replacen("q1_", "d1_", 1),
        expected.replacen("q1_", "q2_", 1),
        expected.replacen("q1_03", "q1_02", 1),
        expected.replacen("q1_03", "q1_04", 1),
        expected.replacen("q1_03", "q1_ff", 1),
        expected.replace("abcdef", "ABCDEF"),
        expected.replace("abcdef", "abcdeg"),
    ] {
        assert_eq!(QueryState::decode(&invalid), Err(Error::Syntax));
    }
    for offset in 0..QueryState::WIRE_BYTES {
        let mut bad = expected.as_bytes().to_vec();
        *bad.get_mut(offset).ok_or(Error::Syntax)? = b'G';
        assert_eq!(
            QueryState::decode(std::str::from_utf8(&bad).map_err(|_| Error::Syntax)?),
            Err(Error::Syntax)
        );
    }
    Ok(())
}
#[test]
fn every_scope_and_interpretation_component_changes_the_token() -> Result<(), Error> {
    let base = QueryState::current(identity(), QueryType::Email);
    let mut first = [0; QueryState::WIRE_BYTES];
    let original = base.encode(&mut first)?;
    for value in [
        QueryState {
            account: AccountId::from_bytes([3; 16]),
            ..base
        },
        QueryState {
            epoch: StoreEpoch::from_bytes([3; 16]),
            ..base
        },
        QueryState {
            kind: QueryType::Mailbox,
            ..base
        },
        QueryState {
            sequence: Sequence::default(),
            ..base
        },
        QueryState {
            sequence: Sequence::from_u64(u64::MAX),
            ..base
        },
        QueryState {
            interpretation: 0,
            ..base
        },
        QueryState {
            interpretation: u32::MAX,
            ..base
        },
    ] {
        let mut output = [0; QueryState::WIRE_BYTES];
        let encoded = value.encode(&mut output)?;
        assert_ne!(encoded, original);
        assert!(!value.matches_current(identity(), QueryType::Email));
        assert_eq!(QueryState::decode(encoded)?, value);
    }
    assert_eq!(QueryState::current(identity(), QueryType::Email), base);
    assert!(base.matches_current(identity(), QueryType::Email));
    let retired = ViewIdentity {
        history_floor: Sequence::from_u64(0x1234),
        ..identity()
    };
    assert_eq!(QueryState::current(retired, QueryType::Email), base);
    Ok(())
}

#[test]
fn lowercase_is_enforced_in_each_letter_bearing_hex_field() -> Result<(), Error> {
    let value = QueryState {
        account: AccountId::from_bytes([0xab; 16]),
        epoch: StoreEpoch::from_bytes([0xcd; 16]),
        sequence: Sequence::from_u64(0xdeadbeef),
        interpretation: 0xabcdef01,
        ..QueryState::current(identity(), QueryType::Email)
    };
    let mut output = [0; QueryState::WIRE_BYTES];
    let valid = value.encode(&mut output)?;
    for offset in 3..QueryState::WIRE_BYTES {
        let mut bytes = valid.as_bytes().to_vec();
        let byte = bytes.get_mut(offset).ok_or(Error::Syntax)?;
        if matches!(*byte, b'a'..=b'f') {
            *byte = byte.to_ascii_uppercase();
            assert_eq!(
                QueryState::decode(std::str::from_utf8(&bytes).map_err(|_| Error::Syntax)?),
                Err(Error::Syntax)
            );
        }
    }
    Ok(())
}
#[test]
fn thread_is_a_data_type_but_never_a_query_type() -> Result<(), Error> {
    let state = DataState {
        account: identity().account,
        epoch: identity().epoch,
        kind: DataType::Thread,
        sequence: identity().committed_sequence,
    };
    let mut output = [0; DataState::WIRE_BYTES];
    let data = state.encode(&mut output)?;
    assert_eq!(DataState::decode(data)?.kind, DataType::Thread);
    let query = format!("q1_{}00000001", data.get(3..).ok_or(Error::Syntax)?);
    assert_eq!(QueryState::decode(&query), Err(Error::Syntax));
    Ok(())
}
