pub fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.chars().count() >= width {
            break;
        }
        out.push(ch);
    }
    out
}

pub fn strip_newlines(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}
