//! A minimal, zero-dependency writer for the guix/Nix store SQLite database —
//! the daemon's `ValidPaths`/`Refs`/`DerivationOutputs` authority, in Rust.
//!
//! The daemon writes this DB via libsqlite; td writes the SQLite *file format*
//! itself (td-store-db track: begin replacing guix-daemon). This is the real
//! replacement of the C++ daemon's store-DB writing — no `sqlite3` engine, no
//! crate. Scope: the three store tables, each as a proper table b-tree —
//! leaf pages (type 0x0d) packed and, when the rows exceed one page, interior
//! pages (type 0x05) above them, to arbitrary depth — so a whole closure
//! (thousands of paths, e.g. the rootless image) fits. The schema decorations
//! the daemon also keeps (indexes, the DeleteSelfRefs trigger, FailedPaths,
//! sqlite_sequence) are added separately by the caller where needed; sqlite3 and
//! the guix-daemon read the tables we write either way.
//!
//! Format references: the SQLite "Database File Format"
//! (https://www.sqlite.org/fileformat2.html) — the 100-byte header, table
//! b-tree leaf pages (page type 0x0d), and the record format (a header of
//! serial-type varints followed by the values). All multi-byte integers are
//! big-endian; lengths are SQLite varints (big-endian base-128, high bit =
//! continue, up to 9 bytes).

const PAGE_SIZE: usize = 4096;
const HEADER_LEN: usize = 100;
// Table b-tree leaf page: type byte 0x0d, then a 7-byte page header (no
// right-most pointer on a leaf), then the cell-pointer array.
const LEAF: u8 = 0x0d;
const LEAF_HEADER_LEN: usize = 8;
// Table b-tree INTERIOR page: type byte 0x05, then a 12-byte page header (the
// last 4 bytes are the right-most child page number), then the cell-pointer
// array. Used when a table's rows span more than one leaf page.
const INTERIOR: u8 = 0x05;
const INTERIOR_HEADER_LEN: usize = 12;

/// A column value in a row. The store schema uses only integers and UTF-8 text
/// (and NULL for an `integer primary key` alias column, whose value is the rowid).
pub enum Value {
    Null,
    Int(i64),
    Text(String),
}

/// One table to materialize as a table b-tree (leaf pages + interior pages as
/// needed); `write_db` assigns its rootpage.
pub struct Table {
    pub name: &'static str,
    pub sql: &'static str,
    pub rows: Vec<(i64, Vec<Value>)>, // (rowid, column values)
}

/// Append `n` as a SQLite varint (big-endian base-128, 1..=9 bytes). The 9-byte
/// form is special: its first 8 bytes carry 7 bits each (high bit = continue)
/// and the LAST byte carries a full 8 bits — i.e. values needing more than 8
/// 7-bit groups (>= 2^56) put their low 8 bits in byte 9.
fn put_varint(out: &mut Vec<u8>, n: u64) {
    if n < (1u64 << 56) {
        // 1..=8 bytes: pure 7-bit groups, most-significant first.
        let mut bytes = [0u8; 9];
        let mut i = 0;
        let mut v = n;
        loop {
            bytes[i] = (v & 0x7f) as u8;
            v >>= 7;
            i += 1;
            if v == 0 {
                break;
            }
        }
        for j in (1..i).rev() {
            out.push(bytes[j] | 0x80);
        }
        out.push(bytes[0]);
    } else {
        // 9-byte form: 8 groups of 7 bits (with continue) then a final full byte.
        for k in (0..8).rev() {
            out.push((((n >> (k * 7 + 8)) & 0x7f) as u8) | 0x80);
        }
        out.push((n & 0xff) as u8);
    }
}

/// The serial type for a value (SQLite record format) and its serialized body.
fn serial(value: &Value) -> (u64, Vec<u8>) {
    match value {
        Value::Null => (0, Vec::new()),
        Value::Int(i) => {
            // Smallest fitting big-endian signed width; 0/1 have dedicated
            // serial types 8/9 (no body).
            let i = *i;
            if i == 0 {
                (8, Vec::new())
            } else if i == 1 {
                (9, Vec::new())
            } else if i >= -0x80 && i < 0x80 {
                (1, vec![i as i8 as u8])
            } else if i >= -0x8000 && i < 0x8000 {
                (2, (i as i16).to_be_bytes().to_vec())
            } else if i >= -0x80_0000 && i < 0x80_0000 {
                (3, (i as i32).to_be_bytes()[1..].to_vec())
            } else if i >= -0x8000_0000 && i < 0x8000_0000 {
                (4, (i as i32).to_be_bytes().to_vec())
            } else if i >= -0x80_0000_0000 && i < 0x80_0000_0000 {
                (5, (i as i64).to_be_bytes()[2..].to_vec())
            } else {
                (6, (i as i64).to_be_bytes().to_vec())
            }
        }
        Value::Text(s) => (13 + 2 * s.len() as u64, s.as_bytes().to_vec()),
    }
}

/// Encode a row as a SQLite record (the cell payload): a header of
/// (header-length, serial-types...) varints, then the value bodies.
fn record(values: &[Value]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut body = Vec::new();
    for v in values {
        let (st, mut b) = serial(v);
        put_varint(&mut types, st);
        body.append(&mut b);
    }
    // Header length includes the bytes used to encode the header length itself.
    // It is small here (a handful of single-byte serial-type varints), so the
    // header-length varint is one byte and header_len = 1 + types.len().
    let mut out = Vec::new();
    let header_len = 1 + types.len() as u64;
    debug_assert!(header_len < 128, "header length must be a 1-byte varint here");
    put_varint(&mut out, header_len);
    out.extend_from_slice(&types);
    out.extend_from_slice(&body);
    out
}

/// The leaf cell for one row: (payload-length varint, rowid varint, payload).
fn leaf_cell(rowid: i64, values: &[Value]) -> Vec<u8> {
    let payload = record(values);
    let mut cell = Vec::new();
    put_varint(&mut cell, payload.len() as u64);
    put_varint(&mut cell, rowid as u64);
    cell.extend_from_slice(&payload);
    cell
}

/// Build a table b-tree LEAF page (type 0x0d) from already-encoded cells. Cells
/// fill from the page end downward; the cell-pointer array (big-endian u16) grows
/// from just after the page header. `page_is_first` reserves the 100-byte file
/// header at the start of page 1. The caller guarantees the cells fit (`pack`).
fn leaf_page_from_cells(cells: &[Vec<u8>], page_is_first: bool) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    let hdr_off = if page_is_first { HEADER_LEN } else { 0 };
    let mut content_end = PAGE_SIZE; // cells fill downward from the page end
    let mut pointers: Vec<u16> = Vec::with_capacity(cells.len());
    for cell in cells {
        content_end -= cell.len();
        page[content_end..content_end + cell.len()].copy_from_slice(cell);
        pointers.push(content_end as u16);
    }
    page[hdr_off] = LEAF;
    page[hdr_off + 3..hdr_off + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    page[hdr_off + 5..hdr_off + 7].copy_from_slice(&(content_end as u16).to_be_bytes());
    let mut ptr_off = hdr_off + LEAF_HEADER_LEN;
    for p in &pointers {
        page[ptr_off..ptr_off + 2].copy_from_slice(&p.to_be_bytes());
        ptr_off += 2;
    }
    assert!(ptr_off <= content_end, "leaf page overflow (a single cell too large?)");
    page
}

/// Build a table b-tree INTERIOR page (type 0x05). `children` are (child-page,
/// key) in ascending key order; the LAST is the right-most pointer (carried in
/// the 12-byte header), the rest are cells of (4-byte left-child page, rowid-key
/// varint). An interior cell points at the child whose keys are <= the cell key.
fn interior_page(children: &[(u32, i64)]) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    let (rightmost, cell_children) = children.split_last().expect("interior page needs a child");
    let mut content_end = PAGE_SIZE;
    let mut pointers: Vec<u16> = Vec::with_capacity(cell_children.len());
    for (child, key) in cell_children {
        let mut cell = Vec::new();
        cell.extend_from_slice(&child.to_be_bytes());
        put_varint(&mut cell, *key as u64);
        content_end -= cell.len();
        page[content_end..content_end + cell.len()].copy_from_slice(&cell);
        pointers.push(content_end as u16);
    }
    page[0] = INTERIOR;
    page[3..5].copy_from_slice(&(cell_children.len() as u16).to_be_bytes());
    page[5..7].copy_from_slice(&(content_end as u16).to_be_bytes());
    page[8..12].copy_from_slice(&rightmost.0.to_be_bytes()); // right-most child
    let mut ptr_off = INTERIOR_HEADER_LEN;
    for p in &pointers {
        page[ptr_off..ptr_off + 2].copy_from_slice(&p.to_be_bytes());
        ptr_off += 2;
    }
    assert!(ptr_off <= content_end, "interior page overflow");
    page
}

/// Greedily pack items of the given byte `costs` into contiguous groups that each
/// fit `avail`; returns the `(start, end)` row ranges. Each item must fit alone
/// (our store records are far under a page — no overflow pages).
fn pack(costs: &[usize], avail: usize) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    let mut used = 0usize;
    for (i, &c) in costs.iter().enumerate() {
        assert!(c <= avail, "a single b-tree cell exceeds one page (overflow pages unsupported)");
        if i > start && used + c > avail {
            groups.push((start, i));
            start = i;
            used = 0;
        }
        used += c;
    }
    groups.push((start, costs.len()));
    groups
}

/// Build a table's b-tree (leaf pages, then interior levels until one root page),
/// appending each page to `out` in page-number order and returning the rootpage.
/// `cells`/`rowids` are the rows in ascending-rowid order.
fn build_btree(cells: &[Vec<u8>], rowids: &[i64], next_page: &mut u32, out: &mut Vec<u8>) -> u32 {
    // Leaf level: cost = cell bytes + its 2-byte pointer.
    let leaf_costs: Vec<usize> = cells.iter().map(|c| c.len() + 2).collect();
    let mut level: Vec<(u32, i64)> = Vec::new(); // (page number, max rowid on/under it)
    for (s, e) in pack(&leaf_costs, PAGE_SIZE - LEAF_HEADER_LEN) {
        let pn = *next_page;
        *next_page += 1;
        out.extend_from_slice(&leaf_page_from_cells(&cells[s..e], false));
        level.push((pn, rowids[e - 1]));
    }
    // Interior levels until a single page remains. Cost per child counts a full
    // cell (4-byte child + key varint + 2-byte pointer) even for the right-most
    // one (which needs none) — a harmless over-count that only adds pages.
    while level.len() > 1 {
        let costs: Vec<usize> = level
            .iter()
            .map(|(_, k)| {
                let mut v = Vec::new();
                put_varint(&mut v, *k as u64);
                4 + v.len() + 2
            })
            .collect();
        let mut next_level: Vec<(u32, i64)> = Vec::new();
        for (s, e) in pack(&costs, PAGE_SIZE - INTERIOR_HEADER_LEN) {
            let pn = *next_page;
            *next_page += 1;
            out.extend_from_slice(&interior_page(&level[s..e]));
            next_level.push((pn, level[e - 1].1));
        }
        level = next_level;
    }
    level[0].0
}

/// Serialize a complete store DB: the 100-byte file header on page 1 (which also
/// carries the `sqlite_master` schema b-tree), then each table's b-tree (one or
/// more pages, deep enough for the whole closure). `sqlite_master` points each
/// table at its computed rootpage.
pub fn write_db(tables: &[Table]) -> Vec<u8> {
    // Build each table's b-tree first (pages 2..), collecting rootpages.
    let mut table_pages: Vec<u8> = Vec::new();
    let mut next_page: u32 = 2;
    let mut rootpages: Vec<u32> = Vec::with_capacity(tables.len());
    for t in tables {
        let cells: Vec<Vec<u8>> = t.rows.iter().map(|(rid, vals)| leaf_cell(*rid, vals)).collect();
        let rowids: Vec<i64> = t.rows.iter().map(|(rid, _)| *rid).collect();
        rootpages.push(build_btree(&cells, &rowids, &mut next_page, &mut table_pages));
    }
    let total_pages = next_page - 1;

    // sqlite_master rows: (type, name, tbl_name, rootpage, sql) — page 1, single
    // leaf (a handful of tables). rootpage points at each table's b-tree root.
    let master_rows: Vec<(i64, Vec<Value>)> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            (
                i as i64 + 1,
                vec![
                    Value::Text("table".to_string()),
                    Value::Text(t.name.to_string()),
                    Value::Text(t.name.to_string()),
                    Value::Int(rootpages[i] as i64),
                    Value::Text(t.sql.to_string()),
                ],
            )
        })
        .collect();
    let master_cells: Vec<Vec<u8>> =
        master_rows.iter().map(|(rid, vals)| leaf_cell(*rid, vals)).collect();
    let mut page1 = leaf_page_from_cells(&master_cells, true);
    write_file_header(&mut page1, total_pages);

    let mut db = Vec::with_capacity(total_pages as usize * PAGE_SIZE);
    db.extend_from_slice(&page1);
    db.extend_from_slice(&table_pages);
    db
}

/// The 100-byte SQLite file header (written into the start of page 1).
fn write_file_header(page1: &mut [u8; PAGE_SIZE], total_pages: u32) {
    page1[0..16].copy_from_slice(b"SQLite format 3\0");
    page1[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_be_bytes()); // page size
    page1[18] = 1; // file format write version (legacy/rollback journal)
    page1[19] = 1; // file format read version
    page1[20] = 0; // reserved space per page
    page1[21] = 64; // max embedded payload fraction
    page1[22] = 32; // min embedded payload fraction
    page1[23] = 32; // leaf payload fraction
    page1[24..28].copy_from_slice(&1u32.to_be_bytes()); // file change counter
    page1[28..32].copy_from_slice(&total_pages.to_be_bytes()); // db size in pages
    // 32..36 first freelist page = 0; 36..40 freelist count = 0
    page1[40..44].copy_from_slice(&1u32.to_be_bytes()); // schema cookie
    page1[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format number
    // 48..52 default page cache size = 0
    // 52..56 largest root btree page (autovacuum) = 0
    page1[56..60].copy_from_slice(&1u32.to_be_bytes()); // text encoding: UTF-8
    // 60..64 user version = 0; 64..68 incremental-vacuum = 0; 68..92 reserved
    page1[92..96].copy_from_slice(&1u32.to_be_bytes()); // version-valid-for
    page1[96..100].copy_from_slice(&3_046_000u32.to_be_bytes()); // SQLite version
}

/// Test-only re-export so the `store_db_read` round-trip tests can encode
/// varints with the exact writer and assert the reader decodes them.
#[cfg(test)]
pub(crate) fn put_varint_for_test(out: &mut Vec<u8>, n: u64) {
    put_varint(out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(n: u64) -> Vec<u8> {
        let mut v = Vec::new();
        put_varint(&mut v, n);
        v
    }

    #[test]
    fn varints_match_sqlite() {
        assert_eq!(varint(0), vec![0x00]);
        assert_eq!(varint(1), vec![0x01]);
        assert_eq!(varint(127), vec![0x7f]);
        assert_eq!(varint(128), vec![0x81, 0x00]);
        assert_eq!(varint(282616), vec![0x91, 0x9f, 0x78]);
        // Boundary: (2^56 - 1) is the largest 8-byte varint; 2^56 needs 9; the
        // 9-byte form's last byte is a FULL 8 bits (not 7) — u64::MAX is all 0xff.
        // (2^56-1) is 8 groups of 7 bits: 7 continue bytes then a terminal 0x7f.
        assert_eq!(varint((1u64 << 56) - 1), vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert_eq!(varint(1u64 << 56).len(), 9);
        assert_eq!(varint(u64::MAX), vec![0xff; 9]);
    }

    #[test]
    fn serial_types() {
        assert!(matches!(serial(&Value::Null), (0, _)));
        assert!(matches!(serial(&Value::Int(0)), (8, _)));
        assert!(matches!(serial(&Value::Int(1)), (9, _)));
        // 282616 needs 3 bytes -> serial type 3, 3-byte body.
        let (st, body) = serial(&Value::Int(282616));
        assert_eq!(st, 3);
        assert_eq!(body, vec![0x04, 0x4f, 0xf8]);
        // "out" -> text, serial 13 + 2*3 = 19.
        let (st, body) = serial(&Value::Text("out".to_string()));
        assert_eq!(st, 19);
        assert_eq!(body, b"out");
    }

    #[test]
    fn record_header_and_body() {
        // (NULL, "out") -> header len 3, types [0, 19], body "out".
        let r = record(&[Value::Null, Value::Text("out".to_string())]);
        assert_eq!(r, vec![0x03, 0x00, 0x13, b'o', b'u', b't']);
    }

    #[test]
    fn db_has_valid_header_and_page_count() {
        let t = Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text)",
            rows: vec![(1, vec![Value::Null, Value::Text("/gnu/store/x".to_string())])],
        };
        let db = write_db(&[t]);
        assert_eq!(&db[0..16], b"SQLite format 3\0");
        assert_eq!(u16::from_be_bytes([db[16], db[17]]), PAGE_SIZE as u16);
        assert_eq!(u32::from_be_bytes([db[28], db[29], db[30], db[31]]), 2); // 2 pages
        assert_eq!(db.len(), 2 * PAGE_SIZE);
        // page 1 carries the sqlite_master leaf (type 0x0d at offset 100).
        assert_eq!(db[HEADER_LEN], LEAF);
        // page 2 is the ValidPaths leaf with one cell.
        assert_eq!(db[PAGE_SIZE], LEAF);
        assert_eq!(u16::from_be_bytes([db[PAGE_SIZE + 3], db[PAGE_SIZE + 4]]), 1);
    }
}
