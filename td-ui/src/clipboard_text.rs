//! Immutable clipboard text whose backing and shared owner retain admission.
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetString, SmallText};

/// A copy refused before any clipboard ownership changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextError {
    TooLong,
    Resource(BudgetError),
}
impl From<BudgetError> for TextError {
    fn from(error: BudgetError) -> Self {
        Self::Resource(error)
    }
}

/// A bounded UTF-8 payload. Short text stays inline; heap-backed clones
/// share the original immutable storage and its budget.
pub struct OwnedText(Backing);
enum Backing {
    Inline(SmallText<80>),
    Long(BudgetShared<BudgetString>),
}
impl OwnedText {
    pub fn copy(budget: &Budget, text: &str) -> Result<Self, TextError> {
        if text.len() > super::clipboard::MAX_BYTES {
            return Err(TextError::TooLong);
        }
        if let Some(text) = SmallText::<80>::copy(text) {
            return Ok(Self(Backing::Inline(text)));
        }
        let text = BudgetString::copy(budget, text)?;
        Ok(Self(Backing::Long(BudgetShared::new(budget, text)?)))
    }

    /// Measure before allocating, then gather exactly that capacity. The
    /// iterator must yield the same borrowed pieces on both traversals.
    #[inline(always)]
    pub fn from_pieces<'a>(
        budget: &Budget,
        mut pieces: impl Iterator<Item = &'a str> + Clone,
        limit: usize,
    ) -> Result<Self, TextError> {
        let limit = limit.min(super::clipboard::MAX_BYTES);
        let size = Clone::clone(&pieces)
            .try_fold(0usize, |size, piece| {
                Some(size.saturating_add(piece.len())).filter(|&size| size <= limit)
            })
            .ok_or(TextError::TooLong)?;
        if size <= 80 {
            let mut text = SmallText::<80>::new();
            let mut refused = false;
            pieces.for_each(|piece| {
                if piece.len() > size.saturating_sub(text.as_str().len()) {
                    refused = true;
                } else {
                    refused |= text.try_push_str(piece).is_err();
                }
            });
            if refused {
                return Err(TextError::TooLong);
            }
            return Ok(Self(Backing::Inline(text)));
        }
        let mut text = td_alloc::BudgetString::with_capacity(budget, size)?;
        // Exact capacity also bounds a stateful iterator's second pass.
        pieces
            .try_for_each(|piece| text.push_str_within_capacity(piece))
            .map_err(|_| TextError::TooLong)?;
        Ok(Self(Backing::Long(BudgetShared::new(budget, text)?)))
    }

    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self(match &self.0 {
            Backing::Inline(text) => Backing::Inline(*text),
            Backing::Long(text) => Backing::Long(text.try_clone()?),
        }))
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Backing::Inline(text) => text.as_str(),
            Backing::Long(text) => text.as_str(),
        }
    }
}
impl std::ops::Deref for OwnedText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl PartialEq<&str> for OwnedText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl std::fmt::Debug for OwnedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedText")
            .field("bytes", &self.len())
            .finish()
    }
}
