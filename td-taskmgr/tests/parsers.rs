#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_taskmgr::parsers::*;
fn stat(name: &str, user: &str, system: &str, start: &str, rss: &str) -> Vec<u8> {
    format!(
        "123 ({name}) S 1 0 0 0 0 0 0 0 0 0 {user} {system} 900 800 0 0 1 0 {start} 10000 {rss}\n"
    )
    .into_bytes()
}
#[test]
fn process_names_parentheses_and_field_positions_survive_hostile_text() {
    let b = stat("weird ) (name\nwith spaces", "15", "7", "1234", "20");
    let p = process(&b).unwrap();
    assert_eq!(p.pid, 123);
    assert_eq!(p.name, b"weird ) (name\nwith spaces");
    assert_eq!(p.parent, Some(1));
    assert_eq!(p.start_ticks, 1234);
    assert_eq!(p.user_ticks, Some(15));
    assert_eq!(p.system_ticks, Some(7));
    assert_eq!(p.rss_pages, Some(20));
    let b = stat("worker", "bad", "8", "20", "-1");
    let p = process(&b).unwrap();
    assert_eq!(p.user_ticks, None);
    assert_eq!(p.system_ticks, Some(8));
    assert_eq!(p.rss_pages, None);
    assert!(process(&stat("worker", "10", "10", "bad", "5")).is_err());
    assert!(process(b"0 (invalid) S").is_err());
    assert_eq!(process(&vec![b'x'; PROCESS_BYTES + 1]), Err(Error::Limit));
    assert_eq!(
        real_uid(b"Name:\tworker\nUid:\t1000 2000 3000 4000\n").unwrap(),
        Some(1000)
    );
    assert_eq!(real_uid(b"Name:\tworker\n").unwrap(), None);
    assert!(real_uid(b"Uid: 1\nUid: 2\n").is_err());
}
#[test]
fn process_cpu_uses_runtime_ticks_real_elapsed_and_no_child_counters() {
    let a = stat("worker", "10", "10", "5", "20");
    let b = stat("worker", "210", "110", "5", "20");
    let old = process(&a).unwrap();
    let next = process(&b).unwrap();
    assert_eq!(process_cpu(next, old, 100, 1_000_000_000), Some(30000));
    assert_eq!(process_cpu(next, old, 250, 2_000_000_000), Some(6000));
    assert_eq!(process_cpu(next, old, 0, 1_000_000_000), None);
    assert_eq!(process_cpu(next, old, 100, 0), None);
    assert_eq!(
        process_cpu(
            Process {
                start_ticks: 6,
                ..next
            },
            old,
            100,
            1_000_000_000
        ),
        None
    );
    assert_eq!(
        process_cpu(
            Process {
                user_ticks: Some(9),
                ..next
            },
            old,
            100,
            1_000_000_000
        ),
        None
    );
    assert_eq!(resident_bytes(next.rss_pages, 16384), Some(327680));
    assert_eq!(resident_bytes(Some(u64::MAX), 4096), None);
    assert_eq!(resident_bytes(Some(1), 0), None);
}
#[test]
fn aggregate_cpu_excludes_wait_steal_and_does_not_double_count_guest() {
    let (id, cpu) = Cpu::parse(b"cpu 100 0 0 50 25 0 0 25 999 999").unwrap();
    assert_eq!(id, None);
    assert_eq!(
        cpu.since(Cpu { counters: [0; 8] }),
        Some(CpuUsage {
            busy: 5000,
            iowait: 1250,
            steal: 1250
        })
    );
    assert_eq!(Cpu::parse(b"cpu17 1 2 3 4 5 6 7 8").unwrap().0, Some(17));
    assert_eq!(cpu.since(cpu), None);
    let mut reset = cpu;
    reset.counters[0] = 99;
    reset.counters[3] = 500;
    assert_eq!(reset.since(cpu), None);
    assert!(Cpu::parse(b"cpu 1 2 3").is_err());
    assert!(Cpu::parse(b"cpuX 1 2 3 4 5 6 7 8").is_err());
}
#[test]
fn memory_missing_fields_and_invalid_units_remain_unavailable() {
    let memory=Memory::parse(b"MemTotal: 100 kB\nMemAvailable: 25 kB\nSwapTotal: 20 kB\nSwapFree: 15 kB\nCached: 200 kB\n").unwrap();
    assert_eq!(memory.used(), Some(75 * 1024));
    assert_eq!(memory.swap_used(), Some(5 * 1024));
    assert_eq!(
        Memory::parse(b"MemTotal: 100 kB\nMemFree: 25 kB\n")
            .unwrap()
            .used(),
        None
    );
    assert_eq!(
        Memory::parse(b"MemTotal: 100 MB\nMemAvailable: 25 kB\n")
            .unwrap()
            .used(),
        None
    );
    assert_eq!(
        Memory::parse(b"MemTotal: 18446744073709551615 kB\n")
            .unwrap()
            .total,
        None
    );
    assert!(Memory::parse(b"MemTotal: 1 kB\nMemTotal: 2 kB\n").is_err());
    assert_eq!(
        Memory {
            total: Some(5),
            available: Some(6),
            ..Memory::default()
        }
        .used(),
        None
    );
}
#[test]
fn network_and_disk_rates_use_the_correct_counter_fields_and_sector_units() {
    let net = network(b"  eth0: 1000 1 2 3 4 5 6 7 2000 9 10 11 12 13 14 15").unwrap();
    assert_eq!(net.name, b"eth0");
    assert_eq!((net.received, net.sent), (1000, 2000));
    assert!(network(b"Inter-| Receive | Transmit").is_err());
    assert!(network(b"eth0: 1 2 3").is_err());
    let d = disk(b"259 0 nvme0n1 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25").unwrap();
    assert_eq!((d.major, d.minor), (259, 0));
    assert_eq!(d.name, b"nvme0n1");
    assert_eq!(
        (
            d.reads,
            d.read_sectors,
            d.writes,
            d.written_sectors,
            d.busy_ms
        ),
        (11, 13, 15, 17, 20)
    );
    assert_eq!(rate(13, 3, 512, 2_000_000_000), Some(2560));
    assert_eq!(rate(2000, 1000, 1, 500_000_000), Some(2000));
    assert_eq!(rate(1, 2, 1, 1), None);
    assert_eq!(rate(1, 0, 1, 0), None);
    assert_eq!(rate(u64::MAX, 0, u64::MAX, 1), None);
    assert!(disk(b"1 0 sda 1 2 3 4").is_err());
}
#[test]
fn arbitrary_bounded_bytes_never_panic_and_unsigned_overflow_is_refused() {
    assert_eq!(unsigned(b"18446744073709551615"), Some(u64::MAX));
    for bytes in [&b"18446744073709551616"[..], b"-1", b"+1", b"", b"1x"] {
        assert_eq!(unsigned(bytes), None);
    }
    let mut state = 7u64;
    for length in 0..512 {
        let mut bytes = Vec::new();
        for _ in 0..length {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            bytes.push((state >> 32) as u8);
        }
        let _ = process(&bytes);
        let _ = real_uid(&bytes);
        let _ = Cpu::parse(&bytes);
        let _ = Memory::parse(&bytes);
        let _ = network(&bytes);
        let _ = disk(&bytes);
    }
}
#[test]
fn escaped_names_preserve_unicode_and_bound_complete_escapes_without_growth() {
    let mut output = String::new();
    output.try_reserve_exact(128).unwrap();
    let capacity = output.capacity();
    assert!(!escaped_text(b"a\nb\0\\\xff", &mut output, 128).unwrap());
    assert_eq!(output, "a\\nb\\x00\\\\\\xff");
    assert!(!escaped_text("café\u{202e}name".as_bytes(), &mut output, 128).unwrap());
    assert_eq!(output, "café\\u{202e}name");
    assert!(escaped_text(b"abc\xffd", &mut output, 6).unwrap());
    assert_eq!(output, "abc");
    assert!(!escaped_text(b"abc\xff", &mut output, 7).unwrap());
    assert_eq!(output, "abc\\xff");
    assert_eq!(output.capacity(), capacity);
    assert!(escaped_text(b"x", &mut String::new(), 1).is_err());
    let mut small = String::new();
    small.try_reserve_exact(4).unwrap();
    let capacity = small.capacity();
    for byte in (0..32).chain(std::iter::once(127)) {
        assert!(!escaped_text(&[byte], &mut small, 4).unwrap());
        assert!(small.len() <= 4);
        assert_eq!(small.capacity(), capacity);
    }
}

#[test]
fn lifetime_cpu_uses_runtime_ticks_without_needing_a_previous_sample() {
    use td_taskmgr::parsers::cpu_time_ms;
    assert_eq!(cpu_time_ms(Some(700), Some(300), 100), Some(10_000));
    assert_eq!(cpu_time_ms(Some(2048), Some(512), 2048), Some(1250));
    assert_eq!(cpu_time_ms(Some(0), Some(0), 100), Some(0));
    assert_eq!(cpu_time_ms(Some(1), Some(1), 0), None);
    assert_eq!(cpu_time_ms(None, Some(3), 100), None);
    assert_eq!(cpu_time_ms(Some(3), None, 100), None);
    assert_eq!(cpu_time_ms(Some(u64::MAX), Some(u64::MAX), 1), None);
    assert_eq!(
        td_taskmgr::format::Text::<32>::cpu_time(Some(3_661_234)).as_str(),
        "1:01:01.234"
    );
    assert_eq!(
        td_taskmgr::format::Text::<32>::cpu_time(None).as_str(),
        "Unavailable"
    );
}
