//! Test-only guarded, non-growing Linux worker mapping evidence.
pub(crate) fn bounded_stack_mapping(label: &str, ceiling: usize) -> usize {
    use std::io::Read;
    let marker = 0u8;
    let address = std::hint::black_box(&marker) as *const u8 as usize;
    let mut text = String::new();
    std::fs::File::open("/proc/self/smaps")
        .unwrap()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.len() <= 1024 * 1024);
    let size = stack_mapping(&text, address, ceiling).unwrap();
    println!("{label}={size}");
    size
}

pub(crate) fn stack_mapping(text: &str, address: usize, ceiling: usize) -> Option<usize> {
    let mut previous: Option<(usize, usize, &str)> = None;
    let mut selected = None;
    for line in text.lines() {
        if let Some(flags) = line.strip_prefix("VmFlags:") {
            if let Some(size) = selected {
                return (!flags.split_whitespace().any(|flag| flag == "gd")).then_some(size);
            }
        }
        let mut fields = line.split_whitespace();
        let Some((low, high)) = fields.next().and_then(|word| word.split_once('-')) else {
            continue;
        };
        if selected.is_some() {
            return None;
        }
        let low = usize::from_str_radix(low, 16).ok()?;
        let high = usize::from_str_radix(high, 16).ok()?;
        let size = high.checked_sub(low)?;
        let perms = fields.next()?;
        if (low..high).contains(&address) {
            let (guard_low, guard_high, guard_perms) = previous?;
            if perms != "rw-p"
                || guard_perms != "---p"
                || guard_high != low
                || guard_high.checked_sub(guard_low)? < 4096
                || size > ceiling
            {
                return None;
            }
            selected = Some(size);
        }
        previous = Some((low, high, perms));
    }
    None
}
