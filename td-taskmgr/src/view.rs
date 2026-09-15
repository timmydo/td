//! Budgeted adapter from process projections to the shared tree table.
use crate::budget::{Budget, Charge, MemoryVec};
use crate::format::Text;
use crate::history::SampleId;
use crate::projection::{self, Expansion, Key, Projection, Sort};
use crate::snapshot::Snapshot;
use std::cell::RefCell;
use std::fmt::Write;
type CachedCells = MemoryVec<(Key, [Text<32>; 8], Text<4096>)>;
use std::sync::Arc;
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::tree_table::{self as tree, Cell, Column, Heading};
const COLUMNS: [Column<'static>; 9] = [
    Column {
        title: "Process",
        minimum: 160,
        preferred: 280,
        numeric: false,
    },
    Column {
        title: "PID",
        minimum: 64,
        preferred: 80,
        numeric: true,
    },
    Column {
        title: "UID",
        minimum: 64,
        preferred: 80,
        numeric: true,
    },
    Column {
        title: "State",
        minimum: 64,
        preferred: 72,
        numeric: false,
    },
    Column {
        title: "CPU % / core",
        minimum: 120,
        preferred: 128,
        numeric: true,
    },
    Column {
        title: "CPU time",
        minimum: 144,
        preferred: 144,
        numeric: true,
    },
    Column {
        title: "RSS",
        minimum: 112,
        preferred: 128,
        numeric: true,
    },
    Column {
        title: "Tree % / core",
        minimum: 128,
        preferred: 136,
        numeric: true,
    },
    Column {
        title: "RSS sum (shared)",
        minimum: 152,
        preferred: 160,
        numeric: true,
    },
];
#[derive(Debug)]
pub struct View {
    pub table: tree::Controller<Key>,
    pub sample: SampleId,
    pub projection: Projection,
    cells: RefCell<CachedCells>,
    _charge: Charge,
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn model(
    budget: &Arc<Budget>,
    projection: &Projection,
) -> Result<(tree::Model<Key>, Charge), String> {
    let mut rows = MemoryVec::new(budget, projection.rows().len()).map_err(error)?;
    for row in projection.rows() {
        rows.push(tree::Row {
            id: row.key,
            parent: row.parent,
            depth: row.depth,
            children: row.children,
            expanded: row.expanded,
        })
        .map_err(|_| "tree projection limit")?;
    }
    let bytes = rows
        .len()
        .checked_mul(std::mem::size_of::<tree::Row<Key>>() + std::mem::size_of::<(Key, usize)>())
        .and_then(|v| {
            v.checked_add(
                std::mem::size_of::<tree::Model<Key>>()
                    + COLUMNS.len() * std::mem::size_of::<Heading>()
                    + COLUMNS.iter().map(|c| c.title.len()).sum::<usize>(),
            )
        })
        .ok_or("tree storage overflow")?;
    let mut charge = budget
        .charge(bytes + std::mem::size_of::<View>())
        .map_err(error)?;
    let model = tree::Model::new(&rows, &COLUMNS).map_err(error)?;
    charge
        .additional(model.storage_bytes().saturating_sub(bytes))
        .map_err(error)?;
    Ok((model, charge))
}
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    pub sample: SampleId,
    pub snapshot: &'a Snapshot,
    pub sort: Sort,
    pub query: &'a str,
    pub selected: Option<crate::hierarchy::ProcessKey>,
    pub expansion: &'a Expansion,
    pub root: Option<crate::hierarchy::ProcessKey>,
}
impl View {
    pub fn new(
        budget: &Arc<Budget>,
        input: Inputs<'_>,
        surface: Surface,
        rect: Rect,
    ) -> Result<Self, String> {
        let Inputs {
            sample,
            snapshot,
            sort,
            query,
            selected,
            expansion,
            root,
        } = input;
        let projection =
            Projection::for_root(budget, snapshot, sort, query, selected, expansion, root)
                .map_err(error)?;
        let (model, charge) = model(budget, &projection)?;
        let table = tree::Controller::new(model, surface, rect).map_err(error)?;
        Ok(Self {
            table,
            sample,
            projection,
            cells: RefCell::new(MemoryVec::new(budget, 0).map_err(error)?),
            _charge: charge,
        })
    }
    pub fn replace(&mut self, budget: &Arc<Budget>, input: Inputs<'_>) -> Result<(), String> {
        let Inputs {
            sample,
            snapshot,
            sort,
            query,
            selected,
            expansion,
            root,
        } = input;
        let projection =
            Projection::for_root(budget, snapshot, sort, query, selected, expansion, root)
                .map_err(error)?;
        let (model, charge) = model(budget, &projection)?;
        self.table.replace(model).map_err(error)?;
        self.projection = projection;
        self.sample = sample;
        self._charge = charge;
        Ok(())
    }
    pub fn reserve_paint(&mut self) -> Result<(), String> {
        let count = self.table.geometry().map(|g| g.visible()).unwrap_or(0);
        self.cells.get_mut().reserve(count).map_err(error)
    }
    pub fn paint(
        &self,
        snapshot: &Snapshot,
        sort: Sort,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) -> Result<(), String> {
        let Some(geometry) = self.table.geometry() else {
            return Ok(());
        };
        let first = geometry.first();
        let mut values = self
            .cells
            .try_borrow_mut()
            .map_err(|_| "visible cell cache in use")?;
        values.clear();
        snapshot
            .with_identities(|names| -> Result<(), String> {
                for (offset, row) in self
                    .table
                    .model()
                    .rows()
                    .iter()
                    .skip(first)
                    .take(geometry.visible())
                    .enumerate()
                {
                    let mut cells = [Text::<32>::default(); 8];
                    if let Key::Process(key) = row.id {
                        if let Ok(index) =
                            snapshot.processes().binary_search_by_key(&key, |p| p.key)
                        {
                            if let Some(p) = snapshot.processes().get(index) {
                                let node = snapshot.ancestry().get(index);
                                cells = [
                                    Text::number(Some(u64::from(p.key.pid))),
                                    Text::number(p.uid.map(u64::from)),
                                    Text::new(std::str::from_utf8(&[p.state]).unwrap_or("?")),
                                    Text::percent(p.cpu, false),
                                    Text::cpu_time(p.cpu_time_ms),
                                    Text::bytes(p.rss, false),
                                    Text::percent(
                                        node.and_then(|n| n.cpu.value()),
                                        node.is_some_and(|n| n.cpu.partial),
                                    ),
                                    Text::bytes(
                                        node.and_then(|n| n.rss.value()),
                                        node.is_some_and(|n| n.rss.partial),
                                    ),
                                ];
                            }
                        }
                    }
                    let projected = self.projection.rows().get(first + offset);
                    let mut name = Text::<4096>::default();
                    if projected.is_some_and(|r| r.exception) {
                        let _ = name.write_str("[selected outside search] ");
                    } else if projected.is_some_and(|r| r.context) {
                        let _ = name.write_str("[context] ");
                    }
                    let text = match row.id {
                        Key::Unavailable => "Parent unavailable",
                        Key::Process(key) => snapshot
                            .processes()
                            .binary_search_by_key(&key, |p| p.key)
                            .ok()
                            .and_then(|i| names.get(i))
                            .map(|n| n.name())
                            .unwrap_or("No longer observed"),
                    };
                    // Leave room for the explicit search label without splitting UTF-8.
                    let bounded = Text::<4000>::truncated(text);
                    let _ = name.write_str(bounded.as_str());
                    values
                        .push((row.id, cells, name))
                        .map_err(|_| "visible cell limit")?;
                }
                Ok(())
            })
            .map_err(error)??;
        values.sort_unstable_by_key(|(id, _, _)| *id);
        self.table.emit(
            Some(tree::Sort {
                column: sort.column as usize,
                direction: if sort.descending {
                    tree::Direction::Descending
                } else {
                    tree::Direction::Ascending
                },
            }),
            damage,
            &mut |key, column| {
                let row = values
                    .binary_search_by_key(&key, |(id, _, _)| *id)
                    .ok()
                    .and_then(|i| values.get(i));
                let text = if column == 0 {
                    row.map(|(_, _, name)| name.as_str()).unwrap_or("")
                } else {
                    row.and_then(|(_, cells, _)| column.checked_sub(1).and_then(|c| cells.get(c)))
                        .map(Text::as_str)
                        .unwrap_or("")
                };
                Cell::new(text).unwrap_or_else(|_| Cell::empty())
            },
            sink,
        );
        Ok(())
    }
}
pub fn column(index: usize) -> Option<projection::Column> {
    [
        projection::Column::Name,
        projection::Column::Pid,
        projection::Column::Uid,
        projection::Column::State,
        projection::Column::Cpu,
        projection::Column::CpuTime,
        projection::Column::Rss,
        projection::Column::TreeCpu,
        projection::Column::TreeRss,
    ]
    .get(index)
    .copied()
}
