//! Stable visible hierarchy and column inputs for the shared tree table.
use crate::CELL_WIDTH;
pub const ROWS: usize = 32_768;
pub const DEPTH: usize = 256;
pub const COLUMNS: usize = 16;
pub const LABEL_BYTES: usize = 128;
pub const CELL_BYTES: usize = 4096;
pub const COLUMN_WIDTH: u32 = 8192;
pub const MODEL_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    InvalidHierarchy,
    InvalidColumn,
    InvalidText,
    Allocation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "tree table limit exceeded",
            Self::InvalidHierarchy => "invalid visible hierarchy",
            Self::InvalidColumn => "invalid table column",
            Self::InvalidText => "invalid table text",
            Self::Allocation => "tree table allocation failed",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row<I> {
    pub id: I,
    pub parent: Option<I>,
    pub depth: u16,
    pub children: bool,
    pub expanded: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Column<'a> {
    pub title: &'a str,
    pub minimum: u32,
    pub preferred: u32,
    pub numeric: bool,
}
#[derive(Debug)]
pub struct Heading {
    title: String,
    minimum: u32,
    preferred: u32,
    numeric: bool,
}
impl Heading {
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn minimum(&self) -> u32 {
        self.minimum
    }
    pub fn preferred(&self) -> u32 {
        self.preferred
    }
    pub fn numeric(&self) -> bool {
        self.numeric
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell<'a> {
    text: &'a str,
}
impl<'a> Cell<'a> {
    pub fn new(text: &'a str) -> Result<Self, Error> {
        if text.len() > CELL_BYTES {
            return Err(Error::Limit);
        }
        if text.chars().any(char::is_control) {
            return Err(Error::InvalidText);
        }
        Ok(Self { text })
    }
    pub fn empty() -> Self {
        Self { text: "" }
    }
    pub fn text(self) -> &'a str {
        self.text
    }
}
#[derive(Debug)]
pub struct Model<I> {
    rows: Vec<Row<I>>,
    index: Vec<(I, usize)>,
    columns: Vec<Heading>,
    storage_bytes: usize,
}
impl<I: Copy + Ord> Model<I> {
    pub fn new(rows: &[Row<I>], columns: &[Column<'_>]) -> Result<Self, Error> {
        if rows.len() > ROWS || columns.len() > COLUMNS {
            return Err(Error::Limit);
        }
        if columns.is_empty() {
            return Err(Error::InvalidColumn);
        }
        let bytes = rows
            .len()
            .checked_mul(std::mem::size_of::<Row<I>>() + std::mem::size_of::<(I, usize)>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .and_then(|bytes| {
                columns
                    .len()
                    .checked_mul(std::mem::size_of::<Heading>())
                    .and_then(|headings| bytes.checked_add(headings))
            })
            .ok_or(Error::Limit)?;
        let bytes = columns.iter().try_fold(bytes, |bytes, column| {
            bytes.checked_add(column.title.len()).ok_or(Error::Limit)
        })?;
        if bytes > MODEL_BYTES {
            return Err(Error::Limit);
        }
        for (index, column) in columns.iter().enumerate() {
            if column.title.is_empty()
                || column.title.len() > LABEL_BYTES
                || column.title.chars().any(char::is_control)
            {
                return Err(Error::InvalidText);
            }
            let label = (column.title.chars().count() + 2) * CELL_WIDTH + 1;
            if column.minimum < label as u32
                || column.preferred < column.minimum
                || column.preferred > COLUMN_WIDTH
                || (index == 0 && column.minimum < 32)
            {
                return Err(Error::InvalidColumn);
            }
        }
        let mut index = Vec::new();
        index
            .try_reserve_exact(rows.len())
            .map_err(|_| Error::Allocation)?;
        index.extend(rows.iter().enumerate().map(|(index, row)| (row.id, index)));
        index.sort_unstable_by_key(|(id, _)| *id);
        if index.windows(2).any(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(a, b)| a.0 == b.0)
        }) {
            return Err(Error::InvalidHierarchy);
        }
        let mut ancestors: [Option<usize>; DEPTH + 1] = [None; DEPTH + 1];
        for (position, row) in rows.iter().enumerate() {
            let depth = usize::from(row.depth);
            if depth > DEPTH {
                return Err(Error::Limit);
            }
            if depth == 0 {
                if row.parent.is_some() {
                    return Err(Error::InvalidHierarchy);
                }
            } else {
                let ancestor = depth
                    .checked_sub(1)
                    .and_then(|level| ancestors.get(level))
                    .copied()
                    .flatten();
                let parent = ancestor
                    .and_then(|index| rows.get(index))
                    .ok_or(Error::InvalidHierarchy)?;
                if row.parent != Some(parent.id) || !parent.children || !parent.expanded {
                    return Err(Error::InvalidHierarchy);
                }
            }
            if let Some(slot) = ancestors.get_mut(depth) {
                *slot = Some(position);
            }
            // A later depth jump must not revive a previous subtree's parent.
            if let Some(deeper) = ancestors.get_mut(depth + 1..) {
                deeper.fill(None);
            }
        }
        let mut captured = Vec::new();
        captured
            .try_reserve_exact(rows.len())
            .map_err(|_| Error::Allocation)?;
        captured.extend_from_slice(rows);
        let mut headings = Vec::new();
        headings
            .try_reserve_exact(columns.len())
            .map_err(|_| Error::Allocation)?;
        for column in columns {
            let mut title = String::new();
            title
                .try_reserve_exact(column.title.len())
                .map_err(|_| Error::Allocation)?;
            title.push_str(column.title);
            headings.push(Heading {
                title,
                minimum: column.minimum,
                preferred: column.preferred,
                numeric: column.numeric,
            });
        }
        let storage_bytes = captured
            .capacity()
            .checked_mul(std::mem::size_of::<Row<I>>())
            .and_then(|bytes| {
                index
                    .capacity()
                    .checked_mul(std::mem::size_of::<(I, usize)>())
                    .and_then(|index| bytes.checked_add(index))
            })
            .and_then(|bytes| {
                headings
                    .capacity()
                    .checked_mul(std::mem::size_of::<Heading>())
                    .and_then(|headings| bytes.checked_add(headings))
            })
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or(Error::Limit)?;
        let storage_bytes = headings.iter().try_fold(storage_bytes, |bytes, heading| {
            bytes
                .checked_add(heading.title.capacity())
                .ok_or(Error::Limit)
        })?;
        if storage_bytes > MODEL_BYTES {
            return Err(Error::Limit);
        }
        Ok(Self {
            rows: captured,
            index,
            columns: headings,
            storage_bytes,
        })
    }
    pub fn storage_bytes(&self) -> usize {
        self.storage_bytes
    }
    pub fn rows(&self) -> &[Row<I>] {
        &self.rows
    }
    pub fn columns(&self) -> &[Heading] {
        &self.columns
    }
    pub fn find(&self, id: I) -> Option<usize> {
        let index = self.index.binary_search_by_key(&id, |(key, _)| *key).ok()?;
        self.index.get(index).map(|(_, row)| *row)
    }
    pub fn parent(&self, index: usize) -> Option<usize> {
        self.find(self.rows.get(index)?.parent?)
    }
    pub fn first_child(&self, index: usize) -> Option<usize> {
        let row = self.rows.get(index)?;
        let next = index.checked_add(1)?;
        self.rows
            .get(next)
            .filter(|child| child.parent == Some(row.id))
            .map(|_| next)
    }
}
