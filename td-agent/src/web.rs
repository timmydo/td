//! `web_fetch`'s URLs and pages (DESIGN.md §12): a URL the model wrote,
//! taken only in a form whose host every parser reads alike, so the
//! destination judged is the one the fetch service connects to; a
//! redirect's `location` joined against it; and a response as text.

pub use td_url::Url;

/// The most redirects one call follows.
pub const MAX_REDIRECTS: usize = 5;
/// The most bytes of a response taken; the service refuses one past it.
pub const MAX_BODY: u64 = 8 * 1024 * 1024;
/// The most bytes of a page rendered, and the width it is rendered at,
/// which bounds a table's columns: unbounded, a wide cell over many rows
/// would pad every row to it.
pub const MAX_HTML: usize = 2 * 1024 * 1024;
const HTML_WIDTH: usize = 100;
/// What a fetch asks for: a page, else text, else whatever there is.
pub const ACCEPT: &str = "text/html, text/plain;q=0.9, */*;q=0.1";

/// A response's body as text: an HTML page rendered as text with its links
/// listed at its end, other text as it is (UTF-8, anything else replaced),
/// and anything else refused by its type.
pub fn text(content_type: Option<&str>, body: &[u8]) -> Result<String, String> {
    let media = content_type
        .and_then(|value| value.split(';').next())
        .map(|media| media.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let start: Vec<u8> = body
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(15)
        .map(u8::to_ascii_lowercase)
        .collect();
    let looks_html = start.starts_with(b"<!doctype html") || start.starts_with(b"<html");
    let html = matches!(media.as_str(), "text/html" | "application/xhtml+xml")
        || (media.is_empty() && looks_html);
    let raw = if html {
        let mut text = td_html::to_text(body.get(..MAX_HTML).unwrap_or(body), HTML_WIDTH);
        if body.len() > MAX_HTML {
            text.push_str(&format!(
                "\n[the page is {} bytes; only its first {MAX_HTML} were rendered]\n",
                body.len()
            ));
        }
        text
    } else {
        let textual = media.starts_with("text/")
            || media.ends_with("+json")
            || media.ends_with("+xml")
            || matches!(
                media.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-javascript"
                    | "application/ecmascript"
                    | "application/toml"
                    | "application/yaml"
                    | "application/x-yaml"
                    | "application/x-sh"
            )
            || (media.is_empty() && std::str::from_utf8(body).is_ok());
        if !textual {
            let kind = if media.is_empty() {
                "of no stated type".to_string()
            } else {
                media
            };
            return Err(format!(
                "the response is {kind}, {} bytes, not text; web_fetch returns only text",
                body.len()
            ));
        }
        String::from_utf8_lossy(body).into_owned()
    };
    // Lines as the page has them, without what a terminal or the window
    // would read as control.
    Ok(raw
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect())
}

/// What the model is answered: where the text came from, its status and
/// type, how much there is and which part this is, then that part, at
/// most `max_bytes` from `offset`, both moved back to a character's
/// start.
pub fn page(
    url: &str,
    status: u16,
    content_type: Option<&str>,
    text: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<String, String> {
    let total = text.len();
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    if offset > total || (offset == total && total > 0) {
        return Err(format!(
            "`offset` {offset} is past the end of the text, which is {total} bytes"
        ));
    }
    let back = |mut at: usize| {
        while at > 0 && !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let from = back(offset);
    // At least the character at `offset`, so that reading on moves.
    let mut to = back(from.saturating_add(max_bytes).min(total));
    if to == from && from < total {
        to = text
            .get(from..)
            .and_then(|rest| rest.chars().next())
            .map_or(total, |c| from + c.len_utf8());
    }
    let shown = text.get(from..to).unwrap_or_default();
    let kind = content_type
        .map(crate::tools::visible)
        .unwrap_or_else(|| "no stated type".into());
    let mut head = format!("{url}\nstatus {status}, {kind}, {total} bytes of text");
    if from > 0 || to < total {
        head.push_str(&format!("; bytes {from} to {to} shown"));
    }
    if to < total {
        head.push_str(&format!("; read on with offset {to}"));
    }
    Ok(format!("{head}\n\n{shown}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_page_is_text_and_anything_else_refused_by_its_type() {
        let page = b"<!DOCTYPE html><html><body><h1>Title</h1><p>Hello <a href=\"/x\">there</a>.</p><script>no()</script></body></html>";
        let text = text(Some("text/html; charset=utf-8"), page).unwrap();
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("/x"), "{text}");
        assert!(!text.contains('<'), "{text}");
        // A page sent with no type is still a page.
        assert_eq!(super::text(None, page).unwrap(), text);
        assert_eq!(
            super::text(Some("application/json"), b"{\"a\":1}\r\n").unwrap(),
            "{\"a\":1}\n"
        );
        assert_eq!(
            super::text(Some("text/plain"), b"a\x1b[31mb\tc").unwrap(),
            "a[31mb\tc"
        );
        assert_eq!(super::text(None, b"plain").unwrap(), "plain");
        // A table's wide cell over many rows is rendered within the width,
        // not padded on every row to it.
        let mut wide = format!("<table><tr><td>{}</td></tr>", "w ".repeat(32 * 1024));
        wide.push_str(&"<tr><td>x</td></tr>".repeat(4096));
        wide.push_str("</table>");
        let rendered = super::text(Some("text/html"), wide.as_bytes()).unwrap();
        // Each row costs the width and its rule, not the cell's 64 KiB.
        assert!(
            rendered.len() < 4096 * 2 * 4 * HTML_WIDTH,
            "{}",
            rendered.len()
        );
        // A page past the bound says it was cut.
        let long = format!("<p>{}</p>", "a ".repeat(MAX_HTML));
        let cut = super::text(Some("text/html"), long.as_bytes()).unwrap();
        assert!(
            cut.ends_with(&format!("only its first {MAX_HTML} were rendered]\n")),
            "{}",
            cut.len()
        );
        let err = super::text(Some("image/png"), b"\x89PNG").unwrap_err();
        assert!(err.contains("image/png, 4 bytes, not text"), "{err}");
        let err = super::text(None, b"\xff\xfe").unwrap_err();
        assert!(err.contains("of no stated type"), "{err}");
    }

    #[test]
    fn a_long_text_is_read_a_part_at_a_time() {
        let text = "ab€cd";
        let whole = page("https://h/", 200, Some("text/plain"), text, 0, 100).unwrap();
        assert_eq!(
            whole,
            "https://h/\nstatus 200, text/plain, 7 bytes of text\n\nab€cd"
        );
        // A part ends before a character it would split, and says where
        // to read on.
        let first = page("https://h/", 200, Some("text/plain"), text, 0, 3).unwrap();
        assert!(
            first.ends_with("bytes 0 to 2 shown; read on with offset 2\n\nab"),
            "{first}"
        );
        let next = page("https://h/", 200, None, text, 2, 3).unwrap();
        assert!(next.contains("no stated type"), "{next}");
        assert!(
            next.ends_with("bytes 2 to 5 shown; read on with offset 5\n\n€"),
            "{next}"
        );
        let last = page("https://h/", 404, None, text, 5, 100).unwrap();
        assert!(last.contains("status 404"), "{last}");
        assert!(last.ends_with("bytes 5 to 7 shown\n\ncd"), "{last}");
        // A part smaller than the character at its offset still holds it.
        let one = page("https://h/", 200, None, text, 2, 1).unwrap();
        assert!(
            one.ends_with("bytes 2 to 5 shown; read on with offset 5\n\n€"),
            "{one}"
        );
        assert!(page("https://h/", 200, None, text, 7, 1)
            .unwrap_err()
            .contains("past the end"));
        assert!(page("https://h/", 200, None, "", 0, 1).is_ok());
    }
}
