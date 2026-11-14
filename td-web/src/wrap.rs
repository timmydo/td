// Budget-owned browser wrapping and navigation caches.
use td_ui::prose::Items;

/// One wrapped line: the logical line it is part of, where its own text
/// starts there (its logical line's start for its first line; past the
/// marks a continuation repeats for the rest), and its pieces and
/// glyphs, each piece tagged with its link, `NO_LINK` for none.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Wrapped {
    pub logical: usize,
    pub start: (usize, usize),
    pub line: prose::Line,
}

impl Wrapped {
    /// Where it starts, as a place another wrap of the page finds.
    pub fn place(&self) -> Place {
        Place {
            logical: self.logical,
            run: self.start.0,
            at: self.start.1,
        }
    }

    /// Whether `piece` shows the page's text at the bytes it names,
    /// rather than marks its continuation repeats from the line's start.
    pub fn shows(&self, piece: &prose::Piece) -> bool {
        self.start == (0, 0) || piece.run >= self.start.0
    }
}

/// A page laid out at one width in one body family, as it is shown: only
/// the wrapped lines near those shown are laid and kept, a stretch of
/// consecutive ones from wherever the reader is, made again from the
/// logical lines when the reader moves past it (DESIGN.md §5). Every
/// place it answers is where one of its wrapped lines starts.
// A safe ring over budget-owned slots. Front removal/prepending is constant
// time; only capacity growth moves the retained rows. Vacated slots are empty.
#[derive(Debug, Default)]
pub(crate) struct Rows {
    slots: Items<Option<Wrapped>>,
    head: usize,
    len: usize,
}
impl Rows {
    fn new(budget: &Budget) -> Result<Self, BudgetError> {
        Ok(Self {
            slots: Items::new(budget)?,
            head: 0,
            len: 0,
        })
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn at(&self, n: usize) -> usize {
        let at = self.head + n;
        if at >= self.slots.len() {
            at - self.slots.len()
        } else {
            at
        }
    }
    fn get(&self, n: usize) -> Option<&Wrapped> {
        if n >= self.len {
            return None;
        }
        self.slots.get(self.at(n))?.as_ref()
    }
    pub(crate) fn first(&self) -> Option<&Wrapped> {
        self.get(0)
    }
    fn last(&self) -> Option<&Wrapped> {
        self.get(self.len.checked_sub(1)?)
    }
    fn iter(&self) -> impl Iterator<Item = &Wrapped> {
        (0..self.len).filter_map(|n| self.get(n))
    }
    fn partition_point(&self, before: impl Fn(&Wrapped) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get(mid).is_some_and(&before) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
    fn admit(&mut self, additional: usize) -> Result<(), BudgetError> {
        let needed = self
            .len
            .checked_add(additional)
            .ok_or(BudgetError::Overflow)?;
        if needed <= self.slots.len() {
            return Ok(());
        }
        let budget = self.slots.budget().ok_or(BudgetError::Limit)?;
        let capacity = needed.max(self.slots.len().saturating_mul(2)).max(4);
        let mut next = match Items::try_with_capacity(budget, capacity) {
            Ok(next) => next,
            Err(BudgetError::Limit | BudgetError::Overflow) if capacity > needed => {
                Items::try_with_capacity(budget, needed)?
            }
            Err(error) => return Err(error),
        };
        for _ in 0..next.capacity() {
            next.try_push(None)?;
        }
        let len = self.len;
        for slot in next.iter_mut().take(len) {
            *slot = self.pop_front();
        }
        self.len = len;
        self.slots = next;
        self.head = 0;
        Ok(())
    }
    fn try_push(&mut self, value: Wrapped) -> Result<(), BudgetError> {
        self.admit(1)?;
        let at = self.at(self.len);
        *self.slots.get_mut(at).ok_or(BudgetError::Limit)? = Some(value);
        self.len += 1;
        Ok(())
    }
    fn try_prepend(&mut self, value: Wrapped) -> Result<(), BudgetError> {
        self.admit(1)?;
        self.head = if self.head == 0 {
            self.slots.len() - 1
        } else {
            self.head - 1
        };
        *self.slots.get_mut(self.head).ok_or(BudgetError::Limit)? = Some(value);
        self.len += 1;
        Ok(())
    }
    fn pop_front(&mut self) -> Option<Wrapped> {
        if self.len == 0 {
            return None;
        }
        let value = self.slots.get_mut(self.head)?.take();
        self.head = self.at(1);
        self.len -= 1;
        value
    }
    fn pop(&mut self) -> Option<Wrapped> {
        let n = self.len.checked_sub(1)?;
        let at = self.at(n);
        let value = self.slots.get_mut(at)?.take();
        self.len = n;
        value
    }
    #[cfg(test)]
    fn try_clone_with(
        &self,
        mut copy: impl FnMut(&Wrapped) -> Result<Wrapped, BudgetError>,
    ) -> Result<Self, BudgetError> {
        let Some(budget) = self.slots.budget() else {
            return Ok(Self::default());
        };
        let mut out = Self::new(budget)?;
        out.admit(self.len)?;
        for row in self.iter() {
            out.try_push(copy(row)?)?;
        }
        Ok(out)
    }
}
impl PartialEq for Rows {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}
impl Eq for Rows {}
pub(crate) struct RowIter(Rows);
impl Iterator for RowIter {
    type Item = Wrapped;
    fn next(&mut self) -> Option<Wrapped> {
        self.0.pop_front()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.0.len, Some(self.0.len))
    }
}
impl DoubleEndedIterator for RowIter {
    fn next_back(&mut self) -> Option<Wrapped> {
        self.0.pop()
    }
}
impl ExactSizeIterator for RowIter {}
impl IntoIterator for Rows {
    type Item = Wrapped;
    type IntoIter = RowIter;
    fn into_iter(self) -> Self::IntoIter {
        RowIter(self)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
impl std::ops::Index<usize> for Rows {
    type Output = Wrapped;
    fn index(&self, n: usize) -> &Wrapped {
        self.get(n).unwrap()
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
impl std::ops::IndexMut<usize> for Rows {
    fn index_mut(&mut self, n: usize) -> &mut Wrapped {
        assert!(n < self.len);
        let at = self.at(n);
        self.slots.get_mut(at).unwrap().as_mut().unwrap()
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Rendered {
    pub page: PageRef,
    /// The width in pixels it is wrapped at.
    pub width: u32,
    body: TextFamily,
    /// Consecutive wrapped lines of the page, in order.
    laid: Rows,
    /// Where the wrapped line after the last laid starts; none past the
    /// page's end.
    after: Option<Place>,
    /// For the last `WALKED` logical lines walked, the last last, where
    /// every `STRIDE`th of its wrapped lines starts, from its first, as a
    /// walk along it found them: a walk back into it starts near where it
    /// is going, and one between two long lines walks neither again.
    stops: Items<(usize, Items<Place>)>,
    memory: WrapBudget,
    refused: bool,
}

#[derive(Default)]
struct WrapBudget(Option<Budget>);
impl std::fmt::Debug for WrapBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WrapBudget")
            .field(&self.0.as_ref().map(Budget::used))
            .finish()
    }
}
impl PartialEq for WrapBudget {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for WrapBudget {}
impl Rendered {
    /// `page` to be laid into lines no wider than `width` pixels (at
    /// least one), its body text in `body`; a line that may not wrap is
    /// one line however wide. Nothing is laid yet.
    pub fn new(page: &PageRef, width: u32, body: TextFamily) -> Result<Self, BudgetError> {
        let Some(parent) = page.budget() else {
            return Ok(Self {
                width: width.max(1),
                body,
                ..Self::default()
            });
        };
        let budget = parent.child(memory::WRAP_WORK)?;
        Ok(Self {
            page: page.try_clone()?,
            width: width.max(1),
            body,
            laid: Rows::new(&budget)?,
            stops: Items::new(&budget)?,
            memory: WrapBudget(Some(budget)),
            ..Self::default()
        })
    }

    /// Release layout backing before replacement while retaining the page for
    /// selection/copy and logical navigation if new admission fails.
    pub fn discard_layout(&mut self) {
        self.laid = Rows::default();
        self.stops = Items::default();
        self.memory = WrapBudget::default();
        self.after = None;
        self.refused = true;
    }

    fn budget(&self) -> Result<&Budget, BudgetError> {
        self.memory.0.as_ref().ok_or(BudgetError::Limit)
    }
    pub fn refused(&self) -> bool {
        self.refused
    }
    /// Begin another user action; previously admitted cache entries remain usable.
    pub fn retry(&mut self) {
        self.refused = false;
    }
    fn attempt<T>(
        &mut self,
        fallback: T,
        work: impl FnOnce(&mut Self) -> Result<T, BudgetError>,
    ) -> T {
        if self.refused {
            return fallback;
        }
        match work(self) {
            Ok(value) => value,
            Err(_) => {
                self.refused = true;
                fallback
            }
        }
    }
    pub fn settle(&mut self, metrics: &mut Metrics, top: Place, rows: usize) {
        self.attempt((), |this| this.try_settle(metrics, top, rows));
    }
    pub fn forward(&mut self, metrics: &mut Metrics, top: Place, count: usize) -> (Place, usize) {
        self.attempt((top, 0), |this| this.try_forward(metrics, top, count))
    }
    pub fn back(&mut self, metrics: &mut Metrics, top: Place, count: usize) -> (Place, usize) {
        self.attempt((top, 0), |this| this.try_back(metrics, top, count))
    }
    pub fn last(&mut self, metrics: &mut Metrics) -> Place {
        self.attempt(Place::default(), |this| this.try_last(metrics))
    }
    pub fn snap(&mut self, metrics: &mut Metrics, place: Place) -> Place {
        self.attempt(place, |this| this.try_snap(metrics, place))
    }

    /// The laid line starting at `place`, by its index.
    fn index(&self, place: Place) -> Option<usize> {
        let at = self.laid.partition_point(|wrapped| wrapped.place() < place);
        self.laid
            .get(at)
            .filter(|wrapped| wrapped.place() == place)
            .map(|_| at)
    }

    /// Where the logical line after `logical` starts; none after the
    /// page's last.
    fn next_line(&self, logical: usize) -> Option<Place> {
        let next = logical.saturating_add(1);
        (next < self.page.lines.len()).then(|| Place::line(next))
    }

    /// The lines shown from `top`, at most `rows`, as laid: none when
    /// `top` is not (`settle` lays them).
    pub fn shown(&self, top: Place, rows: usize) -> impl Iterator<Item = &Wrapped> {
        let from = self.index(top).unwrap_or(self.laid.len());
        self.laid.iter().skip(from).take(rows)
    }

    /// Where the reader is, as the status row says it, when `rows` lines
    /// from `top` do not show all of the page: the logical line the last
    /// of them is part of, counted from one. Read from the lines laid
    /// (`settle`), which reach a line past those shown where there is one.
    pub fn bottom(&self, top: Place, rows: usize) -> Option<usize> {
        let at = self.index(top)?;
        let rows = rows.max(1);
        let after = self.laid.len() - at;
        if top == Place::default() && self.after.is_none() && after <= rows {
            return None;
        }
        let last = self.laid.get(at + after.min(rows) - 1)?;
        Some(last.logical + 1)
    }

    /// The laid lines from `top` through the line after `rows` more,
    /// where the page has them, and none further than a screenful, or
    /// `STRIDE` lines, either side: the wrap held, however long the page.
    fn try_settle(
        &mut self,
        metrics: &mut Metrics,
        top: Place,
        rows: usize,
    ) -> Result<(), BudgetError> {
        if self.page.lines.is_empty() {
            return Ok(());
        }
        let mut at = match self.index(top) {
            Some(at) => at,
            None => {
                self.reset(metrics, top)?;
                0
            }
        };
        while self.laid.len() <= at.saturating_add(rows) && self.grow(metrics)? {}
        let margin = rows.max(STRIDE);
        while at > margin && self.laid.pop_front().is_some() {
            at -= 1;
        }
        while self.laid.len() > at.saturating_add(rows).saturating_add(margin) {
            if let Some(gone) = self.laid.pop() {
                self.after = Some(gone.place());
            }
        }
        Ok(())
    }

    /// The laid lines from `top`, where a line starts, and no others.
    fn reset(&mut self, metrics: &mut Metrics, top: Place) -> Result<(), BudgetError> {
        let (lines, next) = self.lay(metrics, top, STRIDE)?;
        self.laid = lines;
        self.after = next.or_else(|| self.next_line(top.logical));
        Ok(())
    }

    /// Lays up to `STRIDE` lines after those laid; whether there were any.
    fn grow(&mut self, metrics: &mut Metrics) -> Result<bool, BudgetError> {
        let Some(from) = self.after.filter(|_| !self.laid.is_empty()) else {
            return Ok(false);
        };
        let (lines, next) = self.lay(metrics, from, STRIDE)?;
        if lines.is_empty() {
            self.after = None;
            return Ok(false);
        }
        self.laid.admit(lines.len())?;
        for line in lines {
            self.laid.try_push(line)?;
        }
        self.after = next.or_else(|| self.next_line(from.logical));
        Ok(true)
    }

    /// Lays up to `STRIDE` lines before those laid; whether there were
    /// any.
    fn grow_back(&mut self, metrics: &mut Metrics) -> Result<bool, BudgetError> {
        let Some(first) = self.laid.first().map(Wrapped::place) else {
            return Ok(false);
        };
        let (lines, _) = if !first.starts_line() {
            self.walk(metrics, first.logical, Some(first), STRIDE)?
        } else if first.logical > 0 {
            self.walk(metrics, first.logical - 1, None, STRIDE)?
        } else {
            return Ok(false);
        };
        let grew = !lines.is_empty();
        let count = lines.len();
        self.laid.admit(count)?;
        for line in lines.into_iter().rev() {
            self.laid.try_prepend(line)?;
        }
        Ok(grew)
    }

    /// Where the line `count` lines after the one starting at `top`
    /// starts, else the page's last line, and how many lines on that is.
    /// The lines passed are laid and kept until `settle`, so a caller
    /// moves a screenful or so at a time.
    fn try_forward(
        &mut self,
        metrics: &mut Metrics,
        top: Place,
        count: usize,
    ) -> Result<(Place, usize), BudgetError> {
        let (mut at, mut moved) = (top, 0);
        if self.page.lines.is_empty() || count == 0 {
            return Ok((at, moved));
        }
        loop {
            let index = match self.index(at) {
                Some(index) => index,
                None => {
                    self.reset(metrics, at)?;
                    0
                }
            };
            let step = (count - moved).min(self.laid.len().saturating_sub(index + 1));
            if let Some(line) = self.laid.get(index + step) {
                at = line.place();
            }
            moved += step;
            if moved == count || !self.grow(metrics)? {
                return Ok((at, moved));
            }
        }
    }

    /// Where the line `count` lines before the one starting at `top`
    /// starts, else the page's first, and how many lines back that is.
    fn try_back(
        &mut self,
        metrics: &mut Metrics,
        top: Place,
        count: usize,
    ) -> Result<(Place, usize), BudgetError> {
        let (mut at, mut moved) = (top, 0);
        if self.page.lines.is_empty() || count == 0 {
            return Ok((at, moved));
        }
        loop {
            let index = match self.index(at) {
                Some(index) => index,
                None => {
                    self.reset(metrics, at)?;
                    0
                }
            };
            let step = (count - moved).min(index);
            if let Some(line) = self.laid.get(index - step) {
                at = line.place();
            }
            moved += step;
            if moved == count || !self.grow_back(metrics)? {
                return Ok((at, moved));
            }
        }
    }

    /// Where the page's last line starts, laying it and those before it
    /// back from the last logical line, none of the rest.
    fn try_last(&mut self, metrics: &mut Metrics) -> Result<Place, BudgetError> {
        if self.after.is_none() {
            if let Some(last) = self.laid.last() {
                return Ok(last.place());
            }
        }
        let Some(logical) = self.page.lines.len().checked_sub(1) else {
            return Ok(Place::default());
        };
        let (lines, _) = self.walk(metrics, logical, None, STRIDE)?;
        self.laid = lines;
        self.after = None;
        Ok(self
            .laid
            .last()
            .map_or(Place::line(logical), Wrapped::place))
    }

    /// Where the line that shows `place` starts: the last that starts at
    /// or before it, else the first; a place past the page's end is on
    /// its last line. Found among those laid, else by laying its logical
    /// line up to it.
    fn try_snap(&mut self, metrics: &mut Metrics, place: Place) -> Result<Place, BudgetError> {
        if place.logical >= self.page.lines.len() {
            return self.try_last(metrics);
        }
        let at = self
            .laid
            .partition_point(|wrapped| wrapped.place() <= place);
        if let Some(line) = at.checked_sub(1).and_then(|at| self.laid.get(at)) {
            let next = self.laid.get(at).map(Wrapped::place).or(self.after);
            if next.is_none_or(|next| next > place) {
                return Ok(line.place());
            }
        }
        let (lines, _) = self.walk(metrics, place.logical, Some(place.after()), 1)?;
        Ok(lines
            .last()
            .map_or(Place::line(place.logical), Wrapped::place))
    }

    /// Lays logical line `logical` from its start, or from the nearest
    /// start a walk along it remembered, keeping the last `keep` lines
    /// that start before `before` (to its end when none) and remembering
    /// where every `STRIDE`th starts; and where the line after those kept
    /// starts, when it is in this logical line.
    fn walk(
        &mut self,
        metrics: &mut Metrics,
        logical: usize,
        before: Option<Place>,
        keep: usize,
    ) -> Result<(Rows, Option<Place>), BudgetError> {
        let mut starts = match self.stops.iter().position(|(line, _)| *line == logical) {
            Some(at) => self.stops.remove(at).ok_or(BudgetError::Limit)?.1,
            None => Items::new(self.budget()?)?,
        };
        if starts.is_empty() {
            starts.try_push(Place::line(logical))?;
        }
        let walked = self.walk_from(metrics, before, keep, &mut starts)?;
        if self.stops.len() == WALKED {
            self.stops.remove(0);
        }
        self.stops.try_push((logical, starts))?;
        Ok(walked)
    }

    /// `walk` along the logical line whose remembered starts are
    /// `starts`, the first its own start, remembering more as it finds
    /// them.
    fn walk_from(
        &self,
        metrics: &mut Metrics,
        before: Option<Place>,
        keep: usize,
        starts: &mut Items<Place>,
    ) -> Result<(Rows, Option<Place>), BudgetError> {
        // The last remembered start before where it is going.
        let mut stop = match before {
            Some(before) => starts.partition_point(|&start| start < before),
            None => starts.len(),
        }
        .saturating_sub(1);
        let mut kept = Rows::new(self.budget()?)?;
        let Some(&first) = starts.get(stop) else {
            return Ok((kept, None));
        };
        let mut from = first;
        loop {
            let (lines, next) = self.lay(metrics, from, STRIDE)?;
            for line in lines {
                if before.is_some_and(|before| line.place() >= before) {
                    return Ok((kept, Some(line.place())));
                }
                if kept.len() == keep {
                    kept.pop_front();
                }
                kept.try_push(line)?;
            }
            let Some(next) = next else {
                return Ok((kept, None));
            };
            stop += 1;
            if stop == starts.len() {
                starts.try_push(next)?;
            }
            from = next;
        }
    }

    /// Calls `found` with each character of logical line `logical` that
    /// find searches, as it is, and where it is, until `found` says to
    /// stop: all of a line that wraps, and only what is laid of one that
    /// does not, the characters whose pens fall before the pane's edge and
    /// `prose::OVERHANG` past it, as `within` lays them.
    pub fn searched(
        &self,
        metrics: &mut Metrics,
        logical: usize,
        mut found: impl FnMut(char, Place) -> bool,
    ) {
        let Some(line) = self.page.lines.get(logical) else {
            return;
        };
        let edge = (!line.wrap).then(|| self.width.saturating_add(prose::OVERHANG));
        let mut pen = 0u64;
        for (run, part) in line.runs.iter().enumerate() {
            let style = part.style.glyph(self.body);
            for (at, scalar) in part.text.char_indices() {
                if let Some(edge) = edge {
                    if pixels(pen) >= edge {
                        return;
                    }
                    let shown = prose::shown(scalar);
                    let advance = metrics.advance(style.family, style.font_style, shown);
                    pen = pen.saturating_add(u64::from(advance));
                }
                if !found(scalar, Place { logical, run, at }) {
                    return;
                }
            }
        }
    }

    /// The lines of logical line `from.logical` from `from`, where one of
    /// its lines starts, at most `most`, and where its next line starts,
    /// none after its last. A line that may not wrap is one line, laid
    /// only as far as the pane shows it. One that starts with leading
    /// structure (`leading`) hangs: its text after that structure is
    /// wrapped at the width less the structure's, every line of it set
    /// that far in, the first after the whole structure and each
    /// continuation after its marks alone; a structure wider than half the
    /// width does not hang, so the text keeps room. The text is handed to
    /// `prose` a slice at a time (`slice`): every line but a slice's last
    /// is as the whole line would wrap, and the wrap starts again at the
    /// last. A slice of `MAX_ROW_BYTES` holding no whole line is one line,
    /// and the next starts at the first character after it that is no
    /// space, as at any break: when spaces, which a line never breaks at,
    /// overflowed it, that is the line a wrap of the whole would make, and
    /// a line of spaces alone is none. When the slice starts with spaces
    /// its wrap drops, a line of their own, the wrap starts again after
    /// them, as the whole wrap does, so a word past the slice is not cut.
    /// Only characters of no advance end a line there that the whole wrap
    /// would not.
    fn lay(
        &self,
        metrics: &mut Metrics,
        from: Place,
        most: usize,
    ) -> Result<(Rows, Option<Place>), BudgetError> {
        let budget = self.budget()?;
        let mut out = Rows::new(budget)?;
        let Some(logical) = self.page.lines.get(from.logical) else {
            return Ok((out, None));
        };
        let line = |line: prose::Line, start: (usize, usize)| Wrapped {
            logical: from.logical,
            start,
            line,
        };
        if !logical.wrap {
            let shown = within(
                metrics,
                logical,
                self.body,
                self.width.saturating_add(prose::OVERHANG),
                budget,
            )?;
            let laid = prose::wrap_at_most(metrics, &shown, u32::MAX, 1, budget)?
                .pop()
                .unwrap_or_default();
            out.try_push(line(laid, (0, 0)))?;
            return Ok((out, None));
        }
        let hang = Hang::of(metrics, logical, self.body, self.width, budget)?;
        let (base, width) = match &hang {
            Some(hang) => (hang.first, self.width - hang.width),
            None => (0, self.width),
        };
        let mut first = from.starts_line();
        let mut at = if first {
            (base, 0)
        } else {
            (from.run, from.at).max((base, 0))
        };
        let mut bytes = FIRST_SLICE;
        while out.len() < most {
            let want = most - out.len();
            let (runs, cut) = slice(logical, self.body, at, bytes, budget)?;
            let lines = prose::wrap_at_most(metrics, &runs, width, want + 1, budget)?;
            let whole = match cut {
                None => lines.len(),
                Some(_) if lines.len() > want => want + 1,
                Some(_) => lines.len().saturating_sub(1),
            };
            if whole == 0 {
                if bytes < MAX_ROW_BYTES {
                    bytes = bytes.saturating_mul(2).min(MAX_ROW_BYTES);
                    continue;
                }
                // A slice's text that holds no break is one line, but for
                // spaces alone, which a break drops.
                let laid = lines.into_iter().next().unwrap_or_default();
                // Spaces a slice starts with that its wrap dropped, a line
                // of their own, leave a wrap starting afresh after them,
                // whose line may hold more than this slice.
                let dropped = first_scalar(logical, at).is_some_and(|c| prose::shown(c) == ' ')
                    && laid
                        .glyphs
                        .first()
                        .is_some_and(|glyph| prose::shown(glyph.scalar) != ' ');
                if let Some(past) = past_spaces(logical, at).filter(|_| dropped) {
                    at = past;
                    continue;
                }
                let next = cut.and_then(|cut| past_spaces(logical, cut));
                if !laid.glyphs.is_empty() || (next.is_none() && out.is_empty()) {
                    let start = if first { (0, 0) } else { at };
                    out.try_push(line(
                        hung(laid, at, hang.as_ref(), first, logical, self.body, budget)?,
                        start,
                    ))?;
                    first = false;
                }
                match next {
                    Some(next) => at = next,
                    None => return Ok((out, None)),
                }
                continue;
            }
            for (number, laid) in lines.into_iter().enumerate() {
                let start = if number == 0 { at } else { started(&laid, at) };
                if number == whole {
                    // A slice's last line, which another slice lays.
                    at = start;
                    break;
                }
                if out.len() == most {
                    let next = Place {
                        logical: from.logical,
                        run: start.0,
                        at: start.1,
                    };
                    return Ok((out, Some(next)));
                }
                let shown = if first { (0, 0) } else { start };
                out.try_push(line(
                    hung(laid, at, hang.as_ref(), first, logical, self.body, budget)?,
                    shown,
                ))?;
                first = false;
            }
            if cut.is_none() {
                return Ok((out, None));
            }
        }
        let next = Place {
            logical: from.logical,
            run: at.0,
            at: at.1,
        };
        Ok((out, Some(next)))
    }
}

/// One character of a line's leading structure, from where it is in its
/// run, and whether it is a mark its continuation lines repeat: a
/// quote's `>` or a heading's `#`.
#[derive(Clone, Copy, Default)]
struct Lead {
    run: usize,
    at: usize,
    scalar: char,
    mark: bool,
}

// Leading structure borrows its source runs; inspecting it stores no glyphs.
struct Leads<'a> {
    runs: &'a [Run],
    len: usize,
}
impl Leads<'_> {
    fn len(&self) -> usize {
        self.len
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn visit(&self, mut visit: impl FnMut(Lead) -> bool) -> bool {
        self.runs
            .iter()
            .enumerate()
            .all(|(run, part)| structure(&part.text, run, &mut visit))
    }
}
/// Whole leading runs of list, quote, heading or definition structure, at
/// most MAX_LEAD characters; text sharing a marker's run never hangs.
fn leading(runs: &[Run]) -> (Leads<'_>, usize) {
    let mut len = 0;
    for (index, run) in runs.iter().enumerate() {
        let before = len;
        if run.text.is_empty()
            || !structure(&run.text, index, &mut |_| {
                len += 1;
                len <= MAX_LEAD
            })
        {
            return (
                Leads {
                    runs: runs.get(..index).unwrap_or(&[]),
                    len: before,
                },
                index,
            );
        }
    }
    (Leads { runs, len }, runs.len())
}
/// Parse bounded structure a token at a time. Byte peeks concern only ASCII
/// punctuation/digits; emitted character offsets remain UTF-8 boundaries.
fn structure(text: &str, run: usize, visit: &mut impl FnMut(Lead) -> bool) -> bool {
    if text.chars().take(MAX_LEAD + 1).count() > MAX_LEAD {
        return false;
    }
    let mut at = 0;
    while let Some(rest) = text.get(at..).filter(|rest| !rest.is_empty()) {
        let Some(first) = rest.chars().next() else {
            break;
        };
        let (bytes, mark) = match first {
            ' ' => (1, false),
            '>' if rest.starts_with("> ") => (2, true),
            '#' => {
                let count = rest.bytes().take_while(|&c| c == b'#').count();
                if count <= 6 && rest.as_bytes().get(count) == Some(&b' ') {
                    (count + 1, true)
                } else {
                    (0, false)
                }
            }
            '*' | '-' | '+' | '•' | '◦' | '▪'
                if rest
                    .get(first.len_utf8()..)
                    .is_some_and(|r| r.starts_with(' ')) =>
            {
                (first.len_utf8() + 1, false)
            }
            '-' | '0'..='9' => {
                let sign = usize::from(first == '-');
                let digits = rest
                    .bytes()
                    .skip(sign)
                    .take_while(u8::is_ascii_digit)
                    .count();
                let end = sign + digits;
                if digits > 0
                    && matches!(rest.as_bytes().get(end), Some(b'.' | b')'))
                    && rest.as_bytes().get(end + 1) == Some(&b' ')
                {
                    (end + 2, false)
                } else {
                    (0, false)
                }
            }
            _ => (0, false),
        };
        if bytes == 0 {
            return false;
        }
        for (offset, scalar) in rest.get(..bytes).unwrap_or_default().char_indices() {
            if !visit(Lead {
                run,
                at: at + offset,
                scalar,
                mark: mark && scalar != ' ',
            }) {
                return false;
            }
        }
        at += bytes;
    }
    true
}

/// `units` of `prose::UNIT` rounded to the nearest pixel, as `prose`
/// rounds a pen.
fn pixels(units: u64) -> u32 {
    let unit = u64::from(prose::UNIT);
    u32::try_from(units.saturating_add(unit / 2) / unit).unwrap_or(u32::MAX)
}

/// The start of `runs` that an unwrapped line shows at `edge` pixels: the
/// characters whose pens, laid as prose lays them, fall before it. A pane
/// never shows the rest, so it is neither laid nor kept; each run keeps
/// its offsets, so pieces still name their run's text.
fn within<'r>(
    metrics: &mut Metrics,
    logical: &'r Logical,
    body: TextFamily,
    edge: u32,
    budget: &Budget,
) -> Result<Items<prose::Run<'r>>, BudgetError> {
    let mut pen = 0u64;
    let mut shown = Items::new(budget)?;
    for part in &logical.runs {
        let run = part.prose(body, &part.text);
        let mut end = run.text.len();
        for (at, scalar) in run.text.char_indices() {
            if pixels(pen) >= edge {
                end = at;
                break;
            }
            let advance =
                metrics.advance(run.style.family, run.style.font_style, prose::shown(scalar));
            pen = pen.saturating_add(u64::from(advance));
        }
        shown.try_push(prose::Run {
            text: run.text.get(..end).unwrap_or_default(),
            ..run
        })?;
        if end < run.text.len() {
            break;
        }
    }
    Ok(shown)
}

/// Where a line of a slice's wrap starts, as the logical line has it: a
/// slice starting at `at` holds its run from that byte on, and the runs
/// after it whole. A line holding nothing starts where the slice does.
fn started(line: &prose::Line, at: (usize, usize)) -> (usize, usize) {
    match line.pieces.first() {
        Some(piece) if piece.run == 0 => (at.0, at.1 + piece.text.start),
        Some(piece) => (at.0 + piece.run, piece.text.start),
        None => at,
    }
}

/// The first character of `logical` at or after `at`, past empty runs.
fn first_scalar(logical: &Logical, at: (usize, usize)) -> Option<char> {
    logical
        .runs
        .iter()
        .enumerate()
        .skip(at.0)
        .find_map(|(index, run)| {
            let from = if index == at.0 { at.1 } else { 0 };
            run.text.get(from..).and_then(|text| text.chars().next())
        })
}

/// Where the first character of `logical` at or after `at` that is no
/// space is, as `prose` takes a tab for one; none when only spaces are
/// left.
fn past_spaces(logical: &Logical, at: (usize, usize)) -> Option<(usize, usize)> {
    for (index, run) in logical.runs.iter().enumerate().skip(at.0) {
        let from = if index == at.0 { at.1 } else { 0 };
        let text = run.text.get(from..).unwrap_or_default();
        if let Some((byte, _)) = text.char_indices().find(|&(_, c)| prose::shown(c) != ' ') {
            return Some((index, from + byte));
        }
    }
    None
}

/// The runs of `logical` from `at` as prose measures them, holding at most
/// `budget` bytes of text with one more for each run, and where they stop
/// when that is before the logical line's end: within a run, at a
/// character, so a slice holds at least one character of the first.
type RunSlice<'a> = (Items<prose::Run<'a>>, Option<(usize, usize)>);
fn slice<'a>(
    logical: &'a Logical,
    body: TextFamily,
    at: (usize, usize),
    bytes: usize,
    budget: &Budget,
) -> Result<RunSlice<'a>, BudgetError> {
    let mut runs = Items::new(budget)?;
    let mut spent = 0usize;
    for (index, run) in logical.runs.iter().enumerate().skip(at.0) {
        let from = if index == at.0 { at.1 } else { 0 };
        let text = run.text.get(from..).unwrap_or_default();
        let room = bytes.saturating_sub(spent.saturating_add(1));
        if spent >= bytes || text.len() > room {
            let mut end = room.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                runs.try_push(run.prose(body, text.get(..end).unwrap_or_default()))?;
            }
            return Ok((runs, Some((index, from + end))));
        }
        runs.try_push(run.prose(body, text))?;
        spent += text.len() + 1;
    }
    Ok((runs, None))
}

/// What a logical line's lines hang from (`leading`): the first run after
/// its leading structure, that structure's width in pixels, and each of
/// its characters with its pen and advance in `prose::UNIT`s.
struct Hang {
    first: usize,
    width: u32,
    placed: Items<(Lead, u64, u64)>,
}

impl Hang {
    /// How `logical`'s lines hang at `width` pixels, its body text in
    /// `body`; none when it starts with no structure, holds nothing after
    /// it, or the structure is wider than half the width.
    fn of(
        metrics: &mut Metrics,
        logical: &Logical,
        body: TextFamily,
        width: u32,
        budget: &Budget,
    ) -> Result<Option<Self>, BudgetError> {
        let (lead, first) = leading(&logical.runs);
        let rest = logical.runs.get(first..).unwrap_or_default();
        if lead.is_empty() || rest.iter().all(|run| run.text.is_empty()) {
            return Ok(None);
        }
        let mut pen = 0u64;
        let mut placed = Items::try_with_capacity(budget, lead.len())?;
        let mut failure = None;
        lead.visit(|one| {
            let Some(run) = logical.runs.get(one.run) else {
                return false;
            };
            let style = run.style.glyph(body);
            let shown = prose::shown(one.scalar);
            let advance = u64::from(metrics.advance(style.family, style.font_style, shown));
            if let Err(error) = placed.try_push((one, pen, advance)) {
                failure = Some(error);
                return false;
            }
            pen = pen.saturating_add(advance);
            true
        });
        if let Some(error) = failure {
            return Err(error);
        }
        let hang = pixels(pen);
        Ok((hang.saturating_mul(2) <= width).then_some(Self {
            first,
            width: hang,
            placed,
        }))
    }
}

/// `line`, laid from a slice of `logical` starting at `at`, as the logical
/// line has it: its pieces naming the line's runs and bytes, and, when it
/// hangs, set in by the hang after its structure, the first line's
/// whole, a continuation's marks alone, as one piece a run with the text
/// between them left out.
fn hung(
    mut line: prose::Line,
    at: (usize, usize),
    hang: Option<&Hang>,
    first: bool,
    logical: &Logical,
    body: TextFamily,
    budget: &Budget,
) -> Result<prose::Line, BudgetError> {
    let placed = hang.map_or(&[][..], |hang| &hang.placed);
    let shift = hang.map_or(0, |hang| hang.width);
    let marks = placed
        .iter()
        .filter(|(one, _, _)| first || one.mark)
        .count();
    if marks == 0 {
        for piece in &mut line.pieces {
            if piece.run == 0 {
                piece.text.start += at.1;
                piece.text.end += at.1;
            }
            piece.run += at.0;
            piece.left = piece.left.saturating_add(shift);
            piece.right = piece.right.saturating_add(shift);
        }
        for glyph in &mut line.glyphs {
            glyph.x = glyph.x.saturating_add(shift);
        }
        if !line.glyphs.is_empty() || hang.is_none() {
            line.width = line.width.saturating_add(shift);
        }
        return Ok(line);
    }
    let mut laid = prose::Line {
        width: 0,
        pieces: Items::try_with_capacity(budget, line.pieces.len() + marks)?,
        glyphs: Items::try_with_capacity(budget, line.glyphs.len() + marks)?,
    };
    for &(one, pen, advance) in placed {
        if !first && !one.mark {
            continue;
        }
        let Some(run) = logical.runs.get(one.run) else {
            continue;
        };
        let x = pixels(pen);
        let right = pixels(pen.saturating_add(advance));
        let glyph = laid.glyphs.len();
        let end = one.at + one.scalar.len_utf8();
        match laid.pieces.last_mut() {
            Some(piece) if piece.run == one.run => {
                piece.text.end = end;
                piece.right = piece.right.max(right);
                piece.glyphs.end = glyph + 1;
            }
            _ => laid.pieces.try_push(prose::Piece {
                run: one.run,
                tag: run.link.unwrap_or(NO_LINK),
                style: run.style.glyph(body),
                text: one.at..end,
                left: x,
                right,
                glyphs: glyph..glyph + 1,
            })?,
        }
        laid.glyphs.try_push(prose::Placed {
            x,
            scalar: prose::shown(one.scalar),
        })?;
        laid.width = laid.width.max(right);
    }
    let base = laid.glyphs.len();
    for piece in line.pieces {
        let text = if piece.run == 0 {
            piece.text.start + at.1..piece.text.end + at.1
        } else {
            piece.text
        };
        laid.pieces.try_push(prose::Piece {
            run: piece.run + at.0,
            text,
            left: piece.left.saturating_add(shift),
            right: piece.right.saturating_add(shift),
            glyphs: piece.glyphs.start + base..piece.glyphs.end + base,
            ..piece
        })?;
    }
    for glyph in &line.glyphs {
        laid.glyphs.try_push(prose::Placed {
            x: glyph.x.saturating_add(shift),
            ..*glyph
        })?;
    }
    if !line.glyphs.is_empty() || hang.is_none() {
        laid.width = laid.width.max(line.width.saturating_add(shift));
    }
    Ok(laid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::type_complexity)]
mod wrap_allocation_tests {
    use super::*;
    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    impl Clone for Wrapped {
        fn clone(&self) -> Self {
            Self {
                logical: self.logical,
                start: self.start,
                line: self.line.try_clone().unwrap(),
            }
        }
    }
    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    impl Clone for Rendered {
        fn clone(&self) -> Self {
            Self {
                page: self.page.clone(),
                width: self.width,
                body: self.body,
                laid: self.laid.try_clone_with(|w| Ok(w.clone())).unwrap(),
                after: self.after,
                stops: self
                    .stops
                    .try_clone_with(|(n, places)| Ok((*n, places.try_clone_with(|p| Ok(*p))?)))
                    .unwrap(),
                memory: WrapBudget(self.memory.0.as_ref().map(|b| b.try_clone().unwrap())),
                refused: self.refused,
            }
        }
    }

    use crate::view::{Selection, View};
    use td_alloc::with_reservation_limit;
    use td_ui::raster::Scale;
    fn metrics() -> Metrics {
        Metrics::new(None, Scale::new(1).unwrap())
    }
    fn document(root: &Budget) -> Document {
        Document::from_reply_in(
            Url::parse("https://example.invalid/").unwrap(),
            200,
            Some("text/html"),
            format!(
                "<blockquote><ul><li>{}</li><li>{}</li></ul></blockquote>",
                "alpha beta <b>gamma</b> ".repeat(100),
                "delta epsilon ".repeat(100)
            )
            .into_bytes(),
            &AtomicBool::new(false),
            root,
        )
        .unwrap()
    }
    fn snapshot(rendered: &Rendered, view: &View) -> Vec<(Place, u32, Vec<(u32, char)>)> {
        rendered
            .shown(view.top, 8)
            .map(|w| {
                (
                    w.place(),
                    w.line.width,
                    w.line.glyphs.iter().map(|g| (g.x, g.scalar)).collect(),
                )
            })
            .collect()
    }
    #[test]
    fn navigation_refusals_preserve_positions_and_recover_with_exact_lines() {
        let root = memory::root().unwrap();
        let doc = document(&root);
        let baseline = root.used();
        for action in 0..4 {
            let mut expected = doc.render(160, TextFamily::Sans).unwrap();
            let mut expected_view = View::default();
            expected_view.scroll(10, &mut expected, &mut metrics(), 8);
            expected_view.selection = Some(Selection {
                anchor: Place::default(),
                focus: expected_view.top,
            });
            let navigate = |view: &mut View, rendered: &mut Rendered| match action {
                0 => view.scroll(70, rendered, &mut metrics(), 8),
                1 => view.end(rendered, &mut metrics(), 8),
                2 => view.home(rendered, &mut metrics(), 8),
                _ => view.reveal(Place::line(2), rendered, &mut metrics(), 8),
            };
            navigate(&mut expected_view, &mut expected);
            assert!(!expected.refused());
            let wanted = snapshot(&expected, &expected_view);
            drop(expected);
            assert_eq!(root.used(), baseline);
            let mut succeeded = false;
            for n in 0..4096 {
                let mut rendered = doc.render(160, TextFamily::Sans).unwrap();
                let mut view = View::default();
                view.scroll(10, &mut rendered, &mut metrics(), 8);
                view.selection = Some(Selection {
                    anchor: Place::default(),
                    focus: view.top,
                });
                let before = (view.top, view.focus, view.selection);
                with_reservation_limit(n, || navigate(&mut view, &mut rendered));
                if rendered.refused() {
                    assert_eq!(
                        (view.top, view.focus, view.selection),
                        before,
                        "action {action}, allowance {n}"
                    );
                    navigate(&mut view, &mut rendered);
                    assert!(!rendered.refused(), "recovery {action}, allowance {n}");
                } else {
                    succeeded = true;
                }
                assert_eq!(view.top, expected_view.top);
                assert_eq!(snapshot(&rendered, &view), wanted);
                drop(rendered);
                assert_eq!(root.used(), baseline, "action {action}, allowance {n}");
                if succeeded {
                    break;
                }
            }
            assert!(succeeded, "action {action}");
        }
    }
    #[test]
    fn wrapping_owns_actual_storage_and_shared_pressure_is_recoverable() {
        let root = memory::root().unwrap();
        let base = root.used();
        let doc = document(&root);
        let page_bytes = root.used();
        let mut first = doc.render(160, TextFamily::Sans).unwrap();
        assert_eq!(root.used(), page_bytes + Budget::metadata_bytes());
        let mut view = View::default();
        view.scroll(0, &mut first, &mut metrics(), 8);
        assert!(!first.refused());
        let live = root.used();
        assert!(live > page_bytes && live - page_bytes < memory::WRAP_WORK / 4);
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        let before = view.top;
        view.end(&mut first, &mut metrics(), 8);
        assert!(first.refused());
        assert_eq!(view.top, before);
        assert!(doc.render(80, TextFamily::Sans).is_err());
        drop(blocker);
        view.end(&mut first, &mut metrics(), 8);
        assert!(!first.refused());
        let mut second = doc.render(80, TextFamily::Sans).unwrap();
        second.settle(&mut metrics(), Place::default(), 8);
        assert!(!second.refused());
        drop(doc);
        assert!(!first.page.lines.is_empty());
        drop(first);
        assert!(root.used() > base);
        drop(second);
        assert_eq!(root.used(), base);
    }
}
