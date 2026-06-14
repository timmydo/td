//! A minimal, zero-dependency writer for the guix/Nix store SQLite database —
//! the daemon's `ValidPaths`/`Refs`/`DerivationOutputs` authority, in Rust.
//!
//! The daemon writes this DB via libsqlite; td writes the SQLite *file format*
//! itself (td-store-db track: begin replacing guix-daemon). This is the real
//! replacement of the C++ daemon's store-DB writing — no `sqlite3` engine, no
//! crate. Scope (this increment): the three store tables with their rows in a
//! SINGLE leaf b-tree page each (sufficient for an artifact + its references);
//! the full schema (indexes, the DeleteSelfRefs trigger, sqlite_sequence) and
//! multi-page b-trees for the whole closure are LATER increments — sqlite3/guix
//! read the tables we write without them.
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

/// A column value in a row. The store schema uses only integers and UTF-8 text
/// (and NULL for an `integer primary key` alias column, whose value is the rowid).
pub enum Value {
    Null,
    Int(i64),
    Text(String),
}

/// One table to materialize as a single leaf-page b-tree at `rootpage`.
pub struct Table {
    pub name: &'static str,
    pub sql: &'static str,
    pub rows: Vec<(i64, Vec<Value>)>, // (rowid, column values)
}

/// Append `n` as a SQLite varint (big-endian base-128, max 9 bytes).
fn put_varint(out: &mut Vec<u8>, n: u64) {
    if n == 0 {
        out.push(0);
        return;
    }
    // The 9-byte form uses all 8 bits of the last byte; handle the general 1..=9
    // case by emitting 7-bit groups, most-significant first, with the
    // continue bit set on all but the last.
    let mut bytes = [0u8; 10];
    let mut i = 0;
    let mut v = n;
    while v > 0 {
        bytes[i] = (v & 0x7f) as u8;
        v >>= 7;
        i += 1;
    }
    // bytes[0..i] are little-endian 7-bit groups; emit reversed with continue bits.
    for j in (1..i).rev() {
        out.push(bytes[j] | 0x80);
    }
    out.push(bytes[0]);
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

/// Build a single table b-tree leaf page from its rows. Each cell is
/// (payload-length varint, rowid varint, payload); cells are laid out from the
/// end of the page downward, with a cell-pointer array (big-endian u16 offsets)
/// growing from just after the page header. `page_is_first` reserves the 100-byte
/// file header at the start of page 1. Panics if the rows do not fit one page
/// (single-page scope — see the module note).
fn leaf_page(table: &Table, page_is_first: bool) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    let hdr_off = if page_is_first { HEADER_LEN } else { 0 };
    let mut content_end = PAGE_SIZE; // cells fill downward from the page end
    let mut pointers: Vec<u16> = Vec::with_capacity(table.rows.len());
    for (rowid, values) in &table.rows {
        let payload = record(values);
        let mut cell = Vec::new();
        put_varint(&mut cell, payload.len() as u64);
        put_varint(&mut cell, *rowid as u64);
        cell.extend_from_slice(&payload);
        content_end -= cell.len();
        page[content_end..content_end + cell.len()].copy_from_slice(&cell);
        pointers.push(content_end as u16);
    }
    // Page header (8 bytes for a leaf), at hdr_off.
    page[hdr_off] = LEAF;
    // bytes 1..2: first freeblock offset (0 = none)
    // bytes 3..4: number of cells
    page[hdr_off + 3..hdr_off + 5].copy_from_slice(&(table.rows.len() as u16).to_be_bytes());
    // bytes 5..6: cell content area start (0 means 65536; not needed here)
    page[hdr_off + 5..hdr_off + 7].copy_from_slice(&(content_end as u16).to_be_bytes());
    // byte 7: fragmented free bytes (0)
    // The cell-pointer array follows the header, one big-endian u16 per cell.
    let mut ptr_off = hdr_off + LEAF_HEADER_LEN;
    for p in &pointers {
        page[ptr_off..ptr_off + 2].copy_from_slice(&p.to_be_bytes());
        ptr_off += 2;
    }
    assert!(
        ptr_off <= content_end,
        "table `{}' does not fit in one leaf page (single-page scope)",
        table.name
    );
    page
}

/// Serialize a complete store DB: the 100-byte file header on page 1 (which also
/// carries the `sqlite_master` schema b-tree), then one data page per table.
/// `tables[i]` is materialized at rootpage `i + 2`.
pub fn write_db(tables: &[Table]) -> Vec<u8> {
    // sqlite_master rows: (type, name, tbl_name, rootpage, sql). Its rowid is the
    // ordinal; rootpage points at the table's data page.
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
                    Value::Int(i as i64 + 2),
                    Value::Text(t.sql.to_string()),
                ],
            )
        })
        .collect();
    let master = Table { name: "sqlite_master", sql: "", rows: master_rows };

    let total_pages = 1 + tables.len();
    let mut db = Vec::with_capacity(total_pages * PAGE_SIZE);

    // Page 1: header (first 100 bytes) + the sqlite_master leaf b-tree.
    let mut page1 = leaf_page(&master, true);
    write_file_header(&mut page1, total_pages as u32);
    db.extend_from_slice(&page1);

    // Pages 2..: one leaf b-tree per table.
    for t in tables {
        db.extend_from_slice(&leaf_page(t, false));
    }
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
