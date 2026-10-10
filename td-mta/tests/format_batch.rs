#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use td_mta::{
    format::{
        batch::Batch,
        operation::{Operation, Value},
        Error, Table, MAX_TRANSACTION_BYTES, MAX_TRANSACTION_OPERATIONS,
    },
    ports::{Mutation, OperationKind, StagedOperation, TransactionInput},
};
fn hex(input: &str) -> Vec<u8> {
    let digits: String = input.split_whitespace().collect();
    let (pairs, rest) = digits.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn input(bytes: &[u8], count: usize) -> TransactionInput<'_> {
    TransactionInput { bytes, count }
}
fn sentinel() -> StagedOperation {
    StagedOperation {
        kind: OperationKind::Delete,
        type_tag: 9,
        key_offset: 99,
        key_len: 99,
        value_offset: 99,
        value_len: 99,
        ordinal: 99,
    }
}
#[test]
fn literal_mixed_batch_reborrows_original_bytes_and_preserves_slot_suffix() {
    let mut bytes = hex(include_str!("fixtures/format-v2/operation-put-blob.hex"));
    let put_len = bytes.len();
    bytes.extend(hex(include_str!(
        "fixtures/format-v2/operation-delete-change.hex"
    )));
    let mut slots = [Some(sentinel()); 4];
    {
        let batch = Batch::decode(input(&bytes, 3), &mut slots).unwrap();
        assert_eq!(batch.len(), 3);
        assert!(!batch.is_empty());
        for (i, range) in [0..put_len, put_len..put_len + 28, put_len + 28..bytes.len()]
            .into_iter()
            .enumerate()
        {
            let expected = Operation::decode(&bytes[range]).unwrap();
            let actual = batch.get(i).unwrap().unwrap();
            assert_eq!(actual, expected);
            assert_eq!(batch.descriptor(i).unwrap().ordinal as usize, i);
            let expected_key = match expected.value() {
                Value::Row(Mutation::Put { key, .. } | Mutation::Delete(key)) => Some(key),
                Value::Change(_) => None,
            };
            assert_eq!(batch.row_key(i), Ok(expected_key));
            assert_eq!(actual.key_bytes().as_ptr(), expected.key_bytes().as_ptr());
            assert_eq!(
                actual.value_bytes().as_ptr(),
                expected.value_bytes().as_ptr()
            );
        }
        assert_eq!(batch.row_key(3), Ok(None));
        assert_eq!(batch.row_key(usize::MAX), Ok(None));
        assert_eq!(batch.descriptor(3), None);
        assert_eq!(batch.descriptor(usize::MAX), None);
        assert_eq!(batch.get(3), Ok(None));
        assert_eq!(batch.get(usize::MAX), Ok(None));
    }
    assert_eq!(slots[0].unwrap().key_offset, 12);
    assert_eq!(slots[0].unwrap().value_offset, 28);
    assert_eq!(slots[1].unwrap().key_offset as usize, put_len + 12);
    assert_eq!(slots[2].unwrap().ordinal, 2);
    assert_eq!(slots[3], Some(sentinel()));
}
#[test]
fn all_truncations_wrong_counts_and_trailing_bytes_refuse_whole_completion() {
    let bytes = hex(include_str!(
        "fixtures/format-v2/operation-delete-change.hex"
    ));
    let mut slots = [Some(sentinel()); 3];
    for end in 0..bytes.len() {
        assert_eq!(
            Batch::decode(input(&bytes[..end], 2), &mut slots).err(),
            Some(Error::Truncated)
        );
        assert_eq!(slots[2], Some(sentinel()));
    }
    assert_eq!(
        Batch::decode(input(&bytes, 1), &mut slots).err(),
        Some(Error::TrailingBytes)
    );
    assert_eq!(
        Batch::decode(input(&bytes, 3), &mut slots).err(),
        Some(Error::Truncated)
    );
    assert_eq!(
        Batch::decode(input(&[], 0), &mut slots).err(),
        Some(Error::InvalidValue)
    );
    assert_eq!(
        Batch::decode(input(&bytes, 0), &mut slots).err(),
        Some(Error::InvalidValue)
    );
    let mut extra = bytes.clone();
    extra.push(0);
    assert_eq!(
        Batch::decode(input(&extra, 2), &mut slots).err(),
        Some(Error::TrailingBytes)
    );
}
#[test]
fn malformed_last_row_and_header_cannot_yield_a_complete_prefix() {
    let first = hex(include_str!(
        "fixtures/format-v2/operation-delete-change.hex"
    ));
    let put = hex(include_str!("fixtures/format-v2/operation-put-blob.hex"));
    let mut bytes = first.clone();
    bytes.extend(&put);
    let mut slots = [None; 3];
    bytes[first.len() + 8] = 47; // declared row omits its final time byte
    assert_eq!(
        Batch::decode(input(&bytes, 3), &mut slots).err(),
        Some(Error::Truncated)
    );
    bytes[first.len() + 8] = put[8];
    bytes[first.len()] = 255;
    assert_eq!(
        Batch::decode(input(&bytes, 3), &mut slots).err(),
        Some(Error::InvalidTag)
    );
    // Earlier provisional offsets confer no whole-batch authority; reuse overwrites them.
    assert_eq!(
        Batch::decode(input(&first, 2), &mut slots).unwrap().len(),
        2
    );
}
#[test]
fn maximum_count_and_exact_byte_cap_fit_without_growing_caller_slots() {
    let key = [1; 16];
    let delete = Operation::delete(Table::Blobs, &key).unwrap();
    let mut encoded = [0; 28];
    delete.encode(&mut encoded).unwrap();
    let bytes = encoded.repeat(MAX_TRANSACTION_OPERATIONS);
    let mut slots = vec![None; MAX_TRANSACTION_OPERATIONS];
    let batch = Batch::decode(input(&bytes, MAX_TRANSACTION_OPERATIONS), &mut slots).unwrap();
    assert_eq!(batch.get(MAX_TRANSACTION_OPERATIONS - 1), Ok(Some(delete)));
    assert_eq!(
        Batch::decode(input(&bytes, MAX_TRANSACTION_OPERATIONS + 1), &mut slots).err(),
        Some(Error::Limit)
    );
    assert_eq!(
        Batch::decode(
            input(&bytes, MAX_TRANSACTION_OPERATIONS),
            &mut slots[..MAX_TRANSACTION_OPERATIONS - 1]
        )
        .err(),
        Some(Error::OutputFull)
    );
    // 1024 DELETEs with 1012-byte canonical thread-anchor keys occupy exactly 1 MiB.
    let name = "x".repeat(992);
    let anchor =
        td_mta::format::key::Key::ThreadAnchor(&name, td_mta::ids::EmailId::from_bytes(key));
    let mut key_bytes = [0; 1024];
    let n = anchor.encode(&mut key_bytes).unwrap();
    assert_eq!(n, 1012);
    let mut operation_bytes = [0; 1024];
    assert_eq!(
        Operation::delete(Table::ThreadAnchors, &key_bytes[..n])
            .unwrap()
            .encode(&mut operation_bytes),
        Ok(1024)
    );
    let mut exact = operation_bytes.repeat(1024);
    assert_eq!(exact.len(), MAX_TRANSACTION_BYTES);
    assert_eq!(
        Batch::decode(input(&exact, 1024), &mut slots)
            .unwrap()
            .len(),
        1024
    );
    exact.push(0);
    assert_eq!(
        Batch::decode(input(&exact, 1024), &mut slots).err(),
        Some(Error::Limit)
    );
}
