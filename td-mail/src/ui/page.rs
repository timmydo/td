//! A message's HTML as a styled page (README.md, "HTML mail"): made by
//! td-page from the HTML part alone, its `style` elements and `style`
//! attributes, never a sheet or picture it names elsewhere, so nothing
//! is fetched; and above it td-mail's own text of the headers and the
//! attachments, a page of its own the message's styles cannot reach,
//! under a rule td-mail draws. The two scroll as one document. Both are
//! made on the window's thread within one budget, laid out again when
//! the body's size changes, and let go by the view before it renders a
//! text, so a page is never held beside a rendering.

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::AtomicBool;

use td_alloc::{Budget, BudgetShared, BudgetString};
use td_page::fonts::Measure;
use td_page::page::{Laid, Want};
use td_page::paint::{self, Frame, Marks, Scratch};
use td_page::pipeline::{self, Page, Refusal};
use td_ui::prose::{Faces, Metrics};
use td_ui::raster::{Draw, Primitive, Rect, Scale, TextFamily, BORDER, PAPER};

/// What a message's pages are made in: the HTML's tree, styles and
/// layouts, and the head's. A page past it steps down td-page's ladder,
/// to the user-agent sheets, then to its text.
pub const BUDGET: usize = 32 << 20;
/// The inset each side of the pages, in device pixels at scale one.
const INSET: i64 = 8;
/// The space above and below the rule between the head and the page,
/// and the rule's own height, in device pixels at scale one.
const GAP: i64 = 6;
const RULE: i64 = 1;

/// How far a laid page reaches below its top, in device pixels.
fn bottom(laid: &Laid, scale: i64) -> i64 {
    let o = laid.layout.overflow;
    ((o.y + o.height).max(laid.layout.height).max(0.0) * scale as f64).ceil() as i64
}

/// The band of `rect` from `top` to `end`, none when they leave nothing.
fn band(rect: Rect, top: i64, end: i64) -> Option<Rect> {
    let top = top.max(rect.y);
    let end = end.min(rect.y + i64::from(rect.height));
    let height = u32::try_from(end - top).ok().filter(|h| *h > 0)?;
    Some(Rect {
        y: top,
        height,
        ..rect
    })
}

/// A message shown as its pages: the head, a rule, then the page, laid
/// out for the body they fill and scrolled through it together.
pub struct Paged {
    key: BudgetString,
    /// The body laid out for, its scale and its line.
    rect: Rect,
    scale: i64,
    line: i64,
    head: Page,
    page: Page,
    /// The head's height, and the page's top, below the body's top in
    /// device pixels when scrolled to the top.
    head_height: i64,
    page_top: i64,
    /// Scrolled down, in device pixels.
    y: i64,
    /// The HTML was longer than td-mail's input limit and cut at it.
    cut: bool,
    /// The limit the message's HTML was refused by at every rung, when it
    /// was: td-mail's notice is shown in its place.
    pub refused: Option<&'static str>,
    measure: RefCell<Measure>,
    scratch: RefCell<Scratch>,
    /// The pages' budget, their parent: let go after them.
    budget: Budget,
}

fn want(rect: Rect, scale: i64) -> Option<Want> {
    let css = |px: i64| (px.max(1) as f64) / scale as f64;
    Some(Want {
        width: css(i64::from(rect.width) - 2 * INSET * scale),
        height: css(i64::from(rect.height)),
        scale: u8::try_from(scale).ok()?,
        body: TextFamily::Sans,
        styles: true,
        pictures: 0,
    })
}

impl Paged {
    /// The message whose view names it `key`: `head` as td-mail's text,
    /// `html` as its page, laid out for `rect` at `scale` over `faces`
    /// within a budget of `limit` bytes. None when td-alloc refuses even
    /// the head's.
    #[allow(clippy::too_many_arguments)]
    fn make(
        key: &str,
        head: &str,
        html: &str,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
        limit: usize,
    ) -> Option<Self> {
        let s = i64::try_from(scale.value()).ok()?.max(1);
        let want = want(rect, s)?;
        let budget = Budget::new(limit).ok()?;
        let metrics = Metrics::new(faces, scale);
        let line = i64::from(metrics.line().height.max(1));
        let mut measure = Measure::new(metrics, TextFamily::Sans);
        let cancel = AtomicBool::new(false);
        let head_page = pipeline::text_page(head, want, &mut measure, &cancel, &budget).ok()?;
        let head_height = head_page.laid.as_deref().map_or(0, |laid| bottom(laid, s));
        let markup = crate::compose::HTML_LIMITS.read(html);
        let page = BudgetString::copy(&budget, markup)
            .ok()
            .and_then(|markup| BudgetShared::new(&budget, markup).ok());
        let made = match page {
            Some(markup) => pipeline::markup_page(markup, want, &mut measure, &cancel, &budget),
            None => Err(Refusal::Limit("memory")),
        };
        let (page, refused) = match made {
            Ok(made) => (made, None),
            Err(Refusal::Limit(name)) => {
                let notice = "This message's HTML is past what td-mail lays out; \
                              h shows it as text.";
                let own = pipeline::own_page(notice, want, &mut measure, &cancel, &budget).ok()?;
                (own, Some(name))
            }
            Err(Refusal::Cancelled) => return None,
        };
        Some(Self {
            key: BudgetString::copy(&budget, key).ok()?,
            rect,
            scale: s,
            line,
            head: head_page,
            page,
            head_height,
            page_top: head_height + (2 * GAP + RULE) * s,
            y: 0,
            cut: markup.len() < html.len(),
            refused,
            measure: RefCell::new(measure),
            scratch: RefCell::new(Scratch::new_empty()),
            budget,
        })
    }

    /// Whether it is the message `key` laid out for `rect` at `scale`.
    pub fn is(&self, key: &str, rect: Rect, scale: Scale) -> bool {
        self.key.as_str() == key
            && self.rect == rect
            && i64::try_from(scale.value()).is_ok_and(|s| s.max(1) == self.scale)
    }

    /// The body it was laid out for.
    #[cfg(test)]
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// What the status row says of how the page was made, when it was not
    /// made whole: its styles left out, laid out as its text, refused, or
    /// its HTML cut at the input limit.
    pub fn note(&self) -> Option<String> {
        let notes = self.page.notes;
        let rung = if let Some(limit) = self.refused {
            Some(("HTML past td-mail's limits", limit))
        } else if let Some(limit) = notes.text {
            Some(("HTML shown as its text", limit))
        } else {
            notes.styles.map(|limit| ("HTML without its styles", limit))
        };
        let cut = crate::compose::HTML_LIMITS.input >> 10;
        match (rung, self.cut) {
            (None, false) => None,
            (None, true) => {
                td_alloc::format(format_args!("HTML past {cut} KiB not shown; h: text")).ok()
            }
            (Some((what, limit)), false) => {
                td_alloc::format(format_args!("{what} ({limit}); h: text")).ok()
            }
            (Some((what, limit)), true) => td_alloc::format(format_args!(
                "{what} ({limit}), past {cut} KiB not shown; h: text"
            ))
            .ok(),
        }
    }

    /// The lines the body shows, which the page keys move by.
    pub fn rows(&self) -> usize {
        usize::try_from(i64::from(self.rect.height) / self.line)
            .unwrap_or(1)
            .max(1)
    }

    /// How far it scrolls: the head and the page, less the body.
    fn most(&self) -> i64 {
        let page = self
            .page
            .laid
            .as_deref()
            .map_or(0, |laid| bottom(laid, self.scale));
        (self.page_top + page - i64::from(self.rect.height)).max(0)
    }

    /// Scrolled by `lines` body lines; whether it moved.
    pub fn scroll_lines(&mut self, lines: isize) -> bool {
        self.scroll_by(self.line.saturating_mul(lines as i64))
    }

    /// Scrolled by `pages` bodies, less a line so one stays in view.
    pub fn scroll_pages(&mut self, pages: isize) -> bool {
        let step = (i64::from(self.rect.height) - self.line).max(self.line);
        self.scroll_by(step.saturating_mul(pages as i64))
    }

    /// To the top, or the bottom.
    pub fn scroll_to(&mut self, bottom: bool) -> bool {
        let was = self.y;
        self.y = if bottom { self.most() } else { 0 };
        self.y != was
    }

    fn scroll_by(&mut self, dy: i64) -> bool {
        let was = self.y;
        self.y = self.y.saturating_add(dy).clamp(0, self.most());
        self.y != was
    }

    /// The body painted: paper, the head, the rule, then the page, each
    /// clipped to its own band, so nothing the message lays out, wherever
    /// it places it, reaches the head or the rule.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.rect) else {
            return;
        };
        sink(Draw {
            clip: damage,
            primitive: Primitive::Fill {
                rect: self.rect,
                color: PAPER,
            },
        });
        let top = self.rect.y - self.y;
        let rule = top + self.head_height + GAP * self.scale;
        let inset = INSET * self.scale;
        if let Some(line) = band(self.rect, rule, rule + RULE * self.scale) {
            let line = Rect {
                x: line.x + inset,
                width: u32::try_from(i64::from(line.width) - 2 * inset).unwrap_or(0),
                ..line
            };
            if let Some(clip) = damage.intersection(line) {
                sink(Draw {
                    clip,
                    primitive: Primitive::Fill {
                        rect: line,
                        color: BORDER,
                    },
                });
            }
        }
        let (Ok(mut measure), Ok(mut scratch)) =
            (self.measure.try_borrow_mut(), self.scratch.try_borrow_mut())
        else {
            return;
        };
        let parts = [
            (&self.head, top, band(self.rect, top, rule)),
            (
                &self.page,
                top + self.page_top,
                band(self.rect, top + self.page_top, i64::MAX),
            ),
        ];
        for (page, y, clip) in parts {
            let (Some(laid), Some(clip)) = (page.laid.as_deref(), clip) else {
                continue;
            };
            let Some(damage) = damage.intersection(clip) else {
                continue;
            };
            // The window reserves a paint's glyph scratch so glyphs are
            // drawn by face and size; one it cannot have draws them as
            // they come.
            let _ = paint::reserve(&mut scratch, &self.budget);
            let frame = Frame {
                pane: clip,
                x: self.rect.x + inset,
                y,
                scale: self.scale,
            };
            paint::paint(
                laid,
                &mut measure,
                &frame,
                damage,
                &Marks::default(),
                &mut scratch,
                sink,
            );
        }
    }
}

/// A view's pages while its body is one, and the frame they were last
/// refused for, which is not tried again until the key, the body or the
/// scale changes.
#[derive(Default)]
pub struct Held {
    paged: Option<Paged>,
    refused: Option<(u64, Rect, usize)>,
}

impl Held {
    pub fn get(&self) -> Option<&Paged> {
        self.paged.as_ref()
    }

    pub fn get_mut(&mut self) -> Option<&mut Paged> {
        self.paged.as_mut()
    }

    /// Let go, before a text is rendered or when the body is no page.
    pub fn clear(&mut self) {
        self.paged = None;
        self.refused = None;
    }

    /// `Paged::make`'s pages for `key` at `rect` and `scale`, unless it holds
    /// them already or they were refused there; the old pages go before
    /// the new are made.
    pub fn show(
        &mut self,
        key: &str,
        head: &str,
        html: &str,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
    ) {
        self.show_within(key, head, html, rect, scale, faces, BUDGET);
    }

    /// `show` within a budget of `limit` bytes.
    #[allow(clippy::too_many_arguments)]
    fn show_within(
        &mut self,
        key: &str,
        head: &str,
        html: &str,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
        limit: usize,
    ) {
        if self.get().is_some_and(|paged| paged.is(key, rect, scale)) {
            return;
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let tried = (hasher.finish(), rect, scale.value());
        if self.refused == Some(tried) {
            return;
        }
        self.paged = None;
        self.paged = Paged::make(key, head, html, rect, scale, faces, limit);
        self.refused = self.paged.is_none().then_some(tried);
    }

    /// What the status row says of the pages.
    pub fn note(&self) -> Option<String> {
        match &self.paged {
            Some(paged) => paged.note(),
            None if self.refused.is_some() => {
                td_alloc::format(format_args!("HTML could not be laid out; h: text")).ok()
            }
            None => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use td_ui::raster::{Composition, Surface};

    const BODY: Rect = Rect {
        x: 0,
        y: 24,
        width: 640,
        height: 400,
    };

    /// The pages painted over a surface, read back as td-ui reads a frame.
    struct Shown<'a>(&'a Paged);

    impl Composition for Shown<'_> {
        fn surface(&self) -> Surface {
            Surface::new(640, 480, Scale::default()).unwrap()
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.0.emit(damage, sink);
        }
    }

    fn text(paged: &Paged) -> String {
        td_ui::driven::text(&Shown(paged)).unwrap().2
    }

    fn made(head: &str, html: &str) -> Paged {
        Paged::make("m", head, html, BODY, Scale::default(), None, BUDGET).expect("made")
    }

    /// The head is td-mail's text above the page and the page is the
    /// message's HTML styled by its own sheet: what it hides is not shown,
    /// and its styles do not reach the head, another document.
    #[test]
    fn the_head_and_the_styled_page_are_shown_apart() {
        let html = "<style>.gone{display:none} *{color:red}</style>\
                    <p>Seen words<span class=gone>hidden words</span>\
                    <style>body{display:none}</style>";
        let paged = made("From: a@example.org\nSubject: Hi", html);
        let shown = text(&paged);
        assert!(shown.contains("From: a@example.org"), "{shown}");
        assert!(shown.contains("Subject: Hi"), "{shown}");
        assert!(!shown.contains("hidden words"), "{shown}");
        // The message's `body{display:none}` hid its own page.
        assert!(!shown.contains("Seen words"), "{shown}");
        assert_eq!(paged.note(), None);
        assert!(paged.head_height > 0);
        assert!(paged.page_top > paged.head_height);
        // Without the rule the page shows.
        let plain = made("Subject: Hi", "<p>Seen words");
        assert!(text(&plain).contains("Seen words"));
    }

    /// Between the head and the page td-mail draws its rule, and nothing
    /// the message lays out, however far up it places it, is drawn over
    /// the head or the rule.
    #[test]
    fn the_message_draws_below_the_rule_alone() {
        let html = "<p style='margin-top:-200px'>over the head</p>\
                    <p style='margin-top:200px'>in its place";
        let paged = made("Subject: Hi", html);
        let shown = text(&paged);
        assert!(shown.contains("Subject: Hi"), "{shown}");
        assert!(!shown.contains("over the head"), "{shown}");
        assert!(shown.contains("in its place"), "{shown}");
        let mut rules = Vec::new();
        let mut lowest_page_draw = i64::MAX;
        paged.emit(BODY, &mut |draw| match draw.primitive {
            Primitive::Fill { rect, color } if color == BORDER => rules.push(rect),
            _ if draw.clip.y > BODY.y => {
                lowest_page_draw = lowest_page_draw.min(draw.clip.y);
            }
            _ => {}
        });
        let [rule] = rules[..] else {
            panic!("one rule: {rules:?}");
        };
        assert_eq!(rule.y, BODY.y + paged.head_height + GAP);
        assert_eq!(rule.height, RULE as u32);
        assert!(lowest_page_draw > rule.y, "{lowest_page_draw}");
    }

    /// A head of any length scrolls away with the page, so all of it can
    /// be read, and the page keys and the ends move through both.
    #[test]
    fn a_long_head_scrolls_away_with_the_page() {
        let head: Vec<String> = (0..200).map(|n| format!("To: n{n}@example.org")).collect();
        let mut paged = made(&head.join("\n"), "<p>the body words");
        let top = text(&paged);
        assert!(top.contains("To: n0@example.org"), "{top}");
        assert!(!top.contains("the body words"), "{top}");
        assert!(paged.scroll_to(true));
        let end = text(&paged);
        assert!(end.contains("To: n199@example.org"), "{end}");
        assert!(end.contains("the body words"), "{end}");
        assert!(!end.contains("To: n0@example.org"), "{end}");
        assert!(paged.scroll_to(false));
        assert!(text(&paged).contains("To: n0@example.org"));
    }

    /// The page scrolls by lines, by bodies and to its ends, within its
    /// extent; a short message does not move.
    #[test]
    fn the_pages_scroll_within_their_extent() {
        let long: String = (0..300).map(|n| format!("<p>line {n}")).collect();
        let mut paged = made("Subject: long", &long);
        let line = paged.line;
        assert!(paged.scroll_lines(3));
        assert_eq!(paged.y, 3 * line);
        assert!(paged.scroll_pages(1));
        assert_eq!(paged.y, 3 * line + i64::from(BODY.height) - line);
        assert!(paged.scroll_to(true));
        let bottom = paged.y;
        assert_eq!(bottom, paged.most());
        assert!(!paged.scroll_lines(1), "already at the bottom");
        let end = text(&paged);
        assert!(end.contains("line 299"), "{end}");
        assert!(!end.contains("Subject: long"), "{end}");
        assert!(paged.scroll_lines(-1));
        assert!(paged.scroll_to(false));
        assert_eq!(paged.y, 0);
        assert!(!paged.scroll_lines(-1));
        assert!(paged.rows() > 1);
        let mut short = made("Subject: short", "<p>one line");
        assert!(!short.scroll_lines(1));
        assert!(!short.scroll_to(true));
    }

    /// The pages are the message's for its key, its body and its scale.
    #[test]
    fn the_pages_are_made_for_their_key_body_and_scale() {
        let paged = made("Subject: Hi", "<p>a");
        let one = Scale::default();
        assert!(paged.is("m", BODY, one));
        assert!(!paged.is("n", BODY, one));
        assert!(!paged.is("m", Rect { width: 600, ..BODY }, one));
        assert!(!paged.is("m", BODY, Scale::new(2).unwrap()));
    }

    /// A message too large for its budget steps down td-page's ladder and
    /// says so in the status row; at the least budget the pages are made
    /// in, every rung was refused and td-mail's notice is in its place.
    #[test]
    fn a_page_past_its_budget_steps_down_and_says_so() {
        let html: String = (0..400)
            .map(|n| format!("<div style='margin:{}px'><p>w{n}</div>", n % 9))
            .collect();
        let laid = |limit: usize| {
            Paged::make(
                "m",
                "Subject: big",
                &html,
                BODY,
                Scale::default(),
                None,
                limit,
            )
        };
        assert_eq!(laid(BUDGET).expect("made").note(), None);
        // The least budget that makes the pages at all, by halving.
        let (mut low, mut high) = (0, BUDGET);
        while high - low > 1024 {
            let mid = low + (high - low) / 2;
            if laid(mid).is_some() {
                high = mid;
            } else {
                low = mid;
            }
        }
        let least = laid(high).expect("made at the least");
        assert!(least.refused.is_some());
        let note = least.note().expect("a note");
        assert!(note.starts_with("HTML past td-mail's limits ("), "{note}");
        assert!(note.ends_with("; h: text"), "{note}");
        assert!(text(&least).contains("past what td-mail lays out"));
    }

    /// HTML past the input limit is cut at it, and the status row says so.
    #[test]
    fn html_past_the_input_limit_is_cut_and_says_so() {
        let most = crate::compose::HTML_LIMITS.input;
        let mut html = String::from("<p>");
        html.push_str(&"a ".repeat(most / 2));
        let whole = made("Subject: Hi", &html[..most]);
        assert_eq!(whole.note(), None);
        html.push_str("past the limit");
        let paged = made("Subject: Hi", &html);
        assert!(paged.cut);
        assert_eq!(
            paged.note().as_deref(),
            Some("HTML past 512 KiB not shown; h: text")
        );
    }

    /// A view's pages are kept for the same key, body and scale, made
    /// again for another, and let go; pages refused are said, and not
    /// tried again for the same frame.
    #[test]
    fn held_pages_are_kept_remade_refused_and_let_go() {
        let one = Scale::default();
        let mut held = Held::default();
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None);
        let first: *const Paged = held.get().expect("made");
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None);
        assert_eq!(held.get().map(|p| p as *const Paged), Some(first));
        let wider = Rect { width: 600, ..BODY };
        held.show("m", "Subject: Hi", "<p>a", wider, one, None);
        assert!(held.get().is_some_and(|paged| paged.is("m", wider, one)));
        held.clear();
        assert!(held.get().is_none());
        assert_eq!(held.note(), None);
        held.show_within("m", "Subject: Hi", "<p>a", BODY, one, None, 1024);
        assert!(held.get().is_none());
        assert_eq!(
            held.note().as_deref(),
            Some("HTML could not be laid out; h: text")
        );
        // The same frame is not tried again, even with the room for it.
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None);
        assert!(held.get().is_none());
        held.show("m", "Subject: Hi", "<p>a", wider, one, None);
        assert!(held.get().is_some());
        assert_eq!(held.note(), None);
    }

    /// Host measurement (README.md, "Memory"), release build: each hostile
    /// shape as a message whose body is HTML alone, opened in its view,
    /// which keeps its text, lines and links, made into its pages for a
    /// body at 2880x1800 (scale two), and then quoted for a reply, the
    /// view letting its pages go first: the pages' budget use and the
    /// resident peak over the whole, reset before each. `TD_MAIL_SHAPE`
    /// picks one shape a run, so none reuses another's freed heap, and
    /// `TD_MAIL_SANS` and `TD_MAIL_SERIF` name the prose faces' folders.
    #[test]
    #[ignore = "host measurement, release build"]
    fn measure_pages() {
        use crate::backend::BackendResponse;
        use crate::jmap::types::{BodyPart, BodyValue, Email};
        use crate::ui::input::Key;
        use crate::ui::views::{email_view::EmailView, View};
        use std::collections::HashMap;

        fn resident() -> usize {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
            line.split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap()
                * 1024
        }
        let most = crate::compose::HTML_LIMITS.input;
        let fill_to = |most: usize, head: &str, unit: &str| {
            let mut out = String::from(head);
            while out.len() + unit.len() <= most {
                out.push_str(unit);
            }
            out
        };
        let fill = |head: &str, unit: &str| fill_to(most, head, unit);
        let email = |html: &str| {
            let part = BodyPart {
                part_id: "1".to_string(),
                blob_id: None,
                r#type: Some("text/html".to_string()),
                name: None,
                size: None,
            };
            let mut values = HashMap::new();
            values.insert(
                "1".to_string(),
                BodyValue {
                    value: html.to_string(),
                    is_encoding_problem: false,
                    is_truncated: false,
                },
            );
            Email {
                id: "e1".to_string(),
                thread_id: None,
                from: None,
                to: None,
                cc: None,
                reply_to: None,
                subject: Some("shape".to_string()),
                received_at: None,
                sent_at: None,
                preview: None,
                text_body: Some(vec![part.clone()]),
                html_body: Some(vec![part]),
                body_values: values,
                keywords: HashMap::new(),
                mailbox_ids: HashMap::new(),
                message_id: None,
                references: None,
                attachments: None,
                extra: HashMap::new(),
            }
        };
        // Ordinary mail: a sheet, and articles laid out by nested tables
        // and inline styles, as newsletters are.
        let mut newsletter = String::from(
            "<html><head><style>body{margin:0;background:#eee}.btn{color:#fff;\
             background:#36c;padding:8px 16px;border-radius:4px}h2{font-size:20px}\
             </style></head><body><table width=100% cellpadding=0 cellspacing=0>\
             <tr><td align=center><table width=600 style='background:#fff'>",
        );
        let mut n = 0;
        while newsletter.len() < 100 << 10 {
            newsletter.push_str(&format!(
                "<tr><td style='padding:16px 24px;font-family:Arial,sans-serif;\
                 font-size:15px;line-height:22px;color:#333'><h2>Article {n}</h2>\
                 <table width=100%><tr><td width=120><img src='https://e.example/{n}.png' \
                 width=120 height=80 alt='picture {n}'></td><td style='padding-left:12px'>\
                 Some words about the article, a sentence or two of it, with \
                 <a href='https://e.example/a/{n}' style='color:#36c'>a link</a>.</td>\
                 </tr></table><p><a class=btn href='https://e.example/r/{n}'>Read more</a>\
                 </td></tr>"
            ));
            n += 1;
        }
        newsletter.push_str("</table></td></tr></table></body></html>");
        let link = format!("<a href=\"https://e.example/{}\">", "x".repeat(16 << 10));
        let quoted = format!("{link}{}", "<blockquote>".repeat(62));
        let nested_tables = format!(
            "{}w{}",
            "<table><tr><td>".repeat(12),
            "</td></tr></table>".repeat(12)
        );
        let nested_blocks = format!(
            "{}w{}",
            "<span style='display:inline-block'>".repeat(40),
            "</span>".repeat(40)
        );
        let shapes = [
            ("quoted letters in a link", fill(&quoted, "<p>x</p>")),
            ("open quoted letters in a link", fill(&quoted, "<p>x")),
            (
                "floats",
                fill("", "<div style='float:left;width:3px'>w</div>"),
            ),
            (
                "inline blocks",
                fill("", "<span style='display:inline-block'>w</span>"),
            ),
            ("pictures' alt", fill("", "<img alt=x>")),
            ("empty paragraphs", fill("", "<p>")),
            (
                "class floats",
                fill(
                    "<style>.f{float:left;width:3px}</style>",
                    "<div class=f>w</div>",
                ),
            ),
            ("nested tables", fill("", &nested_tables)),
            ("nested inline blocks", fill("", &nested_blocks)),
            (
                "styled divs",
                fill(
                    "<style>div{margin:1px;border:1px solid}</style>",
                    "<div>w</div>",
                ),
            ),
            ("nested divs", fill("", "<div>")),
            ("a table", fill("<table>", "<tr><td>a<td>b<td>c")),
            ("prose", fill("<p>", "lorem ipsum dolor sit amet ")),
            (
                "inline styles",
                fill("", "<span style='color:red;margin:1px'>w</span>"),
            ),
            ("a 100 KiB newsletter", newsletter),
            // The costliest that still fit the budget.
            (
                "styled divs within the budget",
                fill_to(
                    300 << 10,
                    "<style>div{margin:1px;border:1px solid}</style>",
                    "<div>w</div>",
                ),
            ),
            (
                "quoted letters within the budget",
                fill_to(220 << 10, &quoted, "<p>x</p>"),
            ),
        ];
        let body = Rect {
            x: 0,
            y: 48,
            width: 2880,
            height: 1800 - 96,
        };
        let scale = Scale::new(2).unwrap();
        // The prose faces from `TD_MAIL_SANS` and `TD_MAIL_SERIF`, two
        // directories, when they are set: what they hold, read.
        let faces = std::env::var_os("TD_MAIL_SANS").map(|sans| {
            let serif = std::env::var_os("TD_MAIL_SERIF").unwrap();
            let _ = std::fs::write("/proc/self/clear_refs", "5");
            let before = resident();
            let faces = td_ui::pinned_face::prose_from(sans.as_ref(), serif.as_ref()).unwrap();
            println!(
                "the prose faces: {:.1} MB",
                resident().saturating_sub(before) as f64 / 1e6
            );
            faces
        });
        // One shape a process, `TD_MAIL_SHAPE` its index, so none reuses
        // another's freed heap.
        let only: Option<usize> = std::env::var("TD_MAIL_SHAPE")
            .ok()
            .map(|v| v.parse().unwrap());
        for (index, (name, html)) in shapes.iter().enumerate() {
            if only.is_some_and(|only| only != index) {
                continue;
            }
            let message = email(html);
            let _ = std::fs::write("/proc/self/clear_refs", "5");
            let before = resident();
            let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
            let mut view = EmailView::new(
                cmd_tx,
                "me@example.com".to_string(),
                "e1".to_string(),
                Vec::new(),
                0,
                false,
                Vec::new(),
                "Archive".to_string(),
                "Trash".to_string(),
                None,
            );
            view.on_response(&BackendResponse::EmailBody {
                id: "e1".to_string(),
                result: Box::new(Ok(message.clone())),
            });
            let viewed = resident();
            let making = std::time::Instant::now();
            view.lay_out_page(Some(body), scale, faces.as_ref());
            let paged = view.page().and_then(Held::get).expect("pages");
            let (used, note) = (paged.budget.used(), paged.note());
            let made = resident();
            let laid = making.elapsed();
            let started = std::time::Instant::now();
            view.handle_key(Key::Char('r'), 10);
            view.on_response(&BackendResponse::EmailForReply {
                id: "e1".to_string(),
                result: Box::new(Ok(message.clone())),
            });
            assert!(view.page().and_then(Held::get).is_none());
            let reply = view.take_pending_action().expect("a draft");
            let took = started.elapsed();
            let peak = resident();
            println!(
                "{name}: view {:.1} MB, then the pages {:.1} MB ({:.1} MiB of the budget, \
                 {laid:.0?}), then the reply {:.1} MB ({took:.0?}): {:.1} MB in all, {:?}",
                viewed.saturating_sub(before) as f64 / 1e6,
                made.saturating_sub(viewed) as f64 / 1e6,
                used as f64 / 1048576.0,
                peak.saturating_sub(made) as f64 / 1e6,
                peak.saturating_sub(before) as f64 / 1e6,
                note,
            );
            drop((reply, view));
        }
    }
}
