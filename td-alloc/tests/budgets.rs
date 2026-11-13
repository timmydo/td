#![allow(clippy::unwrap_used, clippy::panic, clippy::non_canonical_clone_impl)]
use td_alloc::{
    with_reservation_limit, Arena, Budget, BudgetError, BudgetString, BudgetVec, Buffer,
};

#[test]
fn shrinking_returns_only_excess_to_all_ancestors_without_allocating() {
    let meta = Budget::metadata_bytes();
    let root = Budget::new(3 * meta + 100).unwrap();
    let child = root.child(2 * meta + 100).unwrap();
    let leaf = child.child(meta + 100).unwrap();
    let mut charge = leaf.charge(100).unwrap();
    with_reservation_limit(0, || {
        charge.shrink_to(40).unwrap();
        charge.shrink_to(40).unwrap();
        assert_eq!(charge.shrink_to(41), Err(BudgetError::Limit));
        assert_eq!(charge.shrink_to(usize::MAX), Err(BudgetError::Limit));
    });
    assert_eq!(charge.bytes(), 40);
    assert_eq!(root.used(), 3 * meta + 40);
    assert_eq!(child.used(), 2 * meta + 40);
    assert_eq!(leaf.used(), meta + 40);
    let spare = root.charge(60).unwrap();
    assert!(matches!(root.charge(1), Err(BudgetError::Limit)));
    drop(charge);
    assert_eq!(root.used(), 3 * meta + 60);
    assert_eq!(leaf.used(), meta);
    drop(spare);
    let mut charge = leaf.charge(100).unwrap();
    charge.shrink_to(0).unwrap();
    drop(leaf);
    drop(child);
    assert_eq!(root.used(), 3 * meta);
    assert_eq!(charge.budget().used(), meta);
    drop(charge);
    assert_eq!(root.used(), meta);
}

#[test]
fn shrinking_keeps_retained_bytes_admitted_while_a_sibling_uses_headroom() {
    let meta = Budget::metadata_bytes();
    let root = Budget::new(3 * meta + 100).unwrap();
    let child = root.child(meta + 100).unwrap();
    let sibling = root.child(meta + 100).unwrap();
    let mut charge = child.charge(100).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let start = std::time::Instant::now();
            loop {
                if let Ok(spare) = sibling.charge(60) {
                    assert!(root.used() >= 3 * meta + 100);
                    assert!(matches!(sibling.charge(1), Err(BudgetError::Limit)));
                    return spare;
                }
                assert!(start.elapsed() < std::time::Duration::from_secs(10));
                std::thread::yield_now();
            }
        });
        charge.shrink_to(40).unwrap();
        let spare = worker.join().unwrap();
        assert_eq!(root.used(), 3 * meta + 100);
        drop(spare);
    });
    drop(charge);
    assert_eq!(root.used(), 3 * meta);
}

#[test]
fn ancestors_charge_live_owners_and_failed_admission_rolls_back() {
    let meta = Budget::metadata_bytes();
    let root = Budget::new(3 * meta + 100).unwrap();
    let a = root.child(meta + 100).unwrap();
    let b = root.child(meta + 100).unwrap();
    let first = Buffer::filled(&a, 60, 1u8).unwrap();
    assert_eq!(root.used(), 3 * meta + 60);
    assert!(matches!(
        Buffer::filled(&b, 41, 0u8),
        Err(BudgetError::Limit)
    ));
    assert_eq!(b.used(), meta);
    let second = Buffer::filled(&b, 40, 2u8).unwrap();
    drop(a);
    assert_eq!(root.used(), 3 * meta + 100);
    drop(first);
    assert_eq!(root.used(), 2 * meta + 40);
    drop(second);
    drop(b);
    assert_eq!(root.used(), meta);
}

#[test]
fn growth_admits_overlap_and_preserves_original_on_refusal() {
    let meta = Budget::metadata_bytes();
    let budget = Budget::new(meta + 20).unwrap();
    let mut v = BudgetVec::filled(&budget, 8, 7u8).unwrap();
    assert!(matches!(v.try_reserve_exact(5), Err(BudgetError::Limit)));
    assert_eq!(&*v, &[7; 8]);
    assert_eq!(v.capacity(), 8);
    assert_eq!(budget.used(), meta + 8);
    v.try_reserve_exact(4).unwrap();
    assert_eq!(v.capacity(), 12);
    assert_eq!(budget.used(), meta + 12);
    v.clear();
    assert_eq!(budget.used(), meta + 12);
    drop(v);
    assert_eq!(budget.used(), meta);
}

#[test]
fn every_allocator_refusal_returns_admission() {
    let meta = Budget::metadata_bytes();
    assert!(matches!(
        with_reservation_limit(0, || Budget::new(4096)),
        Err(BudgetError::Allocation)
    ));
    let root = Budget::new(4096).unwrap();
    assert!(matches!(
        with_reservation_limit(0, || root.child(1024)),
        Err(BudgetError::Allocation)
    ));
    assert_eq!(root.used(), meta);
    let mut bytes = BudgetVec::filled(&root, 16, 4u8).unwrap();
    assert!(matches!(
        with_reservation_limit(0, || bytes.try_reserve_exact(16)),
        Err(BudgetError::Allocation)
    ));
    assert_eq!(root.used(), meta + 16);
    assert_eq!(&*bytes, &[4; 16]);
    bytes.try_reserve_exact(16).unwrap();
    assert_eq!(root.used(), meta + 32);
}

#[test]
fn zero_sized_values_overalignment_and_overflow() {
    #[derive(Clone, Copy)]
    #[repr(align(256))]
    struct Aligned(u8);
    let root = Budget::new(4096).unwrap();
    let zero = with_reservation_limit(0, || Buffer::filled(&root, 100, ())).unwrap();
    assert_eq!(zero.len(), 100);
    assert_eq!(root.used(), Budget::metadata_bytes());
    let data = Buffer::filled(&root, 3, Aligned(5)).unwrap();
    assert_eq!(data.as_ptr() as usize % 256, 0);
    assert!(data.iter().all(|v| v.0 == 5));
    assert!(matches!(
        BudgetVec::<u64>::with_capacity(&root, usize::MAX),
        Err(BudgetError::Overflow)
    ));
}

#[test]
fn shared_budget_crosses_threads_and_survives_original_owner() {
    let meta = Budget::metadata_bytes();
    let root = Budget::new(meta + 64).unwrap();
    let other = root.try_clone().unwrap();
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let bytes = Buffer::filled(&other, 64, 9u8).unwrap();
        send.send(()).unwrap();
        wait.recv().unwrap();
        bytes
    });
    recv.recv().unwrap();
    assert!(matches!(root.charge(1), Err(BudgetError::Limit)));
    drop(root);
    release.send(()).unwrap();
    let bytes = worker.join().unwrap();
    assert_eq!(&*bytes, &[9; 64]);
    assert_eq!(bytes.budget().used(), meta + 64);
}

#[test]
fn concurrent_siblings_never_exceed_ancestor() {
    let meta = Budget::metadata_bytes();
    let root = Budget::new(9 * meta + 128).unwrap();
    let children: [_; 8] = std::array::from_fn(|_| root.child(meta + 128).unwrap());
    std::thread::scope(|scope| {
        for child in children {
            let root = &root;
            scope.spawn(move || {
                for _ in 0..1000 {
                    if let Ok(bytes) = Buffer::filled(&child, 48, 0u8) {
                        assert!(root.used() <= root.limit());
                        std::hint::black_box(bytes);
                    }
                }
            });
        }
    });
    assert_eq!(root.used(), meta);
}

#[test]
fn typed_arena_drops_values_and_indices_survive_growth() {
    struct Count<'a>(&'a std::cell::Cell<usize>);
    impl Drop for Count<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = std::cell::Cell::new(0);
    let root = Budget::new(8192).unwrap();
    let mut arena = Arena::new(&root).unwrap();
    for i in 0..100 {
        assert_eq!(arena.try_insert(Count(&drops)).unwrap(), i);
    }
    assert_eq!(drops.get(), 0);
    assert!(arena.get(99).is_some());
    assert!(arena.get(100).is_none());
    drop(arena);
    assert_eq!(drops.get(), 100);
    assert_eq!(root.used(), Budget::metadata_bytes());
}

#[test]
fn utf8_and_copy_do_not_call_clone() {
    #[derive(Copy)]
    struct CopyOnly;
    impl Clone for CopyOnly {
        fn clone(&self) -> Self {
            panic!("must copy");
        }
    }
    let root = Budget::new(4096).unwrap();
    let mut v = BudgetVec::filled(&root, 2, CopyOnly).unwrap();
    v.try_extend_from_slice(&[CopyOnly; 4]).unwrap();
    assert_eq!(v.len(), 6);
    let mut text = BudgetString::new(&root).unwrap();
    text.try_push_str("héllö 🌍").unwrap();
    assert_eq!(text.as_str(), "héllö 🌍");
}

#[test]
fn depth_limit_and_unwind_return_all_charges() {
    let root = Budget::new(65536).unwrap();
    let mut current = root.try_clone().unwrap();
    for _ in 1..Budget::MAX_DEPTH {
        current = current.child(65536).unwrap();
    }
    assert!(matches!(current.child(65536), Err(BudgetError::Limit)));
    drop(current);
    let result = std::panic::catch_unwind(|| {
        let child = root.child(4096).unwrap();
        let _bytes = Buffer::filled(&child, 128, 0u8).unwrap();
        panic!("operation cancellation");
    });
    assert!(result.is_err());
    assert_eq!(root.used(), Budget::metadata_bytes());
}

#[test]
fn tight_budget_vector_growth_uses_one_allocation_instead_of_exact_steps() {
    let meta = Budget::metadata_bytes();
    for ancestor_limited in [false, true] {
        let root = Budget::new(2 * meta + if ancestor_limited { 5000 } else { 8000 }).unwrap();
        let child = root
            .child(meta + if ancestor_limited { 8000 } else { 5000 })
            .unwrap();
        let mut values = BudgetVec::filled(&child, 2048, 7u8).unwrap();
        with_reservation_limit(1, || {
            for _ in 2048..2500 {
                values.try_push(7).unwrap();
            }
        });
        assert_eq!(values.len(), 2500);
        assert!(values.iter().all(|&value| value == 7));
        assert_eq!(root.used(), 2 * meta + values.capacity());
        assert!(root.used() <= root.limit());
        assert!(child.used() <= child.limit());
        drop(values);
        assert_eq!(root.used(), 2 * meta);
    }
}

#[test]
fn tight_budget_string_growth_uses_one_allocation_instead_of_exact_steps() {
    let meta = Budget::metadata_bytes();
    let root = Budget::new(meta + 5000).unwrap();
    let mut text = BudgetString::with_capacity(&root, 2048).unwrap();
    for _ in 0..2048 {
        text.try_push_str("x").unwrap();
    }
    with_reservation_limit(1, || {
        for _ in 2048..2500 {
            text.try_push_str("x").unwrap();
        }
    });
    assert_eq!(text.as_str().len(), 2500);
    assert!(text.as_str().bytes().all(|byte| byte == b'x'));
    assert!(root.used() <= root.limit());
    drop(text);
    assert_eq!(root.used(), meta);
}

#[test]
fn tight_budget_arena_growth_uses_one_allocation_instead_of_exact_steps() {
    let root = Budget::new(Budget::metadata_bytes() + 5000).unwrap();
    let mut arena = Arena::with_capacity(&root, 2048).unwrap();
    for index in 0..2048 {
        assert_eq!(arena.try_insert(7u8).unwrap(), index);
    }
    with_reservation_limit(1, || {
        for index in 2048..2500 {
            assert_eq!(arena.try_insert(7).unwrap(), index);
        }
    });
    assert_eq!(arena.get(0), Some(&7));
    assert_eq!(arena.get(2499), Some(&7));
    drop(arena);
    assert_eq!(root.used(), Budget::metadata_bytes());
}

#[test]
fn shared_values_charge_once_and_release_nested_storage_after_the_last_owner() {
    use td_alloc::BudgetShared;
    let root = Budget::new(4096).unwrap();
    let nested = Buffer::filled(&root, 128, 9u8).unwrap();
    let value = BudgetShared::new(&root, nested).unwrap();
    let charged = root.used();
    assert!(charged > Budget::metadata_bytes() + 128);
    let other = with_reservation_limit(0, || value.try_clone()).unwrap();
    assert_eq!(root.used(), charged);
    drop(value);
    assert_eq!(&**other, &[9; 128]);
    assert_eq!(root.used(), charged);
    std::thread::spawn(move || drop(other)).join().unwrap();
    assert_eq!(root.used(), Budget::metadata_bytes());
    let nested = Buffer::filled(&root, 128, 9u8).unwrap();
    assert!(matches!(
        with_reservation_limit(0, || BudgetShared::new(&root, nested)),
        Err(BudgetError::Allocation)
    ));
    assert_eq!(root.used(), Budget::metadata_bytes());
}

#[test]
fn growing_preserves_overaligned_values_and_drops_each_once() {
    #[repr(align(256))]
    struct Item<'a>(&'a std::cell::Cell<usize>, usize);
    impl Drop for Item<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = std::cell::Cell::new(0);
    let root = Budget::new(65536).unwrap();
    let mut values = BudgetVec::new(&root).unwrap();
    for i in 0..65 {
        values.try_push(Item(&drops, i)).unwrap();
        assert_eq!(values.as_ptr() as usize % 256, 0);
        assert_eq!(drops.get(), 0);
        assert!(values.iter().enumerate().all(|(at, v)| at == v.1));
    }
    assert!(values.remove(65).is_none());
    drop(values.remove(0).unwrap());
    assert_eq!(drops.get(), 1);
    drop(values);
    assert_eq!(drops.get(), 65);
    assert_eq!(root.used(), Budget::metadata_bytes());
}

#[test]
fn string_growth_and_copy_preserve_utf8_and_refused_owners() {
    let root = Budget::new(4096).unwrap();
    let mut text = BudgetString::copy(&root, "é🌎").unwrap();
    let before = root.used();
    assert!(matches!(
        with_reservation_limit(0, || text.try_push('語')),
        Err(BudgetError::Allocation)
    ));
    assert_eq!(text.as_str(), "é🌎");
    assert_eq!(root.used(), before);
    text.try_push('語').unwrap();
    let copy = text.try_clone().unwrap();
    assert_eq!(text, copy);
    assert_eq!(text.as_str(), "é🌎語");
    drop(text);
    drop(copy);
    assert_eq!(root.used(), Budget::metadata_bytes());
}
