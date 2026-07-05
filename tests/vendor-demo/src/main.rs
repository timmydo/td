// td-vendor-demo — exercises both vendored dependencies so the build genuinely
// links them: itoa formats an integer, ryu formats a float, with no std formatting
// of the numbers. Prints "2026 3.14159"; the td-vendor-demo recipe check asserts that output.
fn main() {
    let mut ib = itoa::Buffer::new();
    let mut rb = ryu::Buffer::new();
    println!("{} {}", ib.format(2026u32), rb.format(3.14159f64));
}
