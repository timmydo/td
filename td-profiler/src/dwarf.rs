use std::collections::BTreeMap;

pub const MAX_LINE_SECTION_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_LINE_STRING_BYTES: u64 = 32 * 1024 * 1024;
pub const DEBUG_STR_REQUIRED: &str = "DWARF line table requires .debug_str";
const MAX_LINE_ROWS: usize = 1_000_000;
const MAX_LINE_FILES: usize = 200_000;
const MAX_LINE_PATH_BYTES: usize = 4096;
const MAX_FORMAT_FIELDS: usize = 32;
const FIXED_PARSER_HEAP_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    pub file: Vec<u8>,
    pub line: u64,
    pub column: u64,
    pub discriminator: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BorrowedLocation<'a> {
    pub file: &'a [u8],
    pub line: u64,
    pub column: u64,
    pub discriminator: u64,
}

pub struct Lines {
    entries: Vec<Entry>,
    max_ends: Vec<u64>,
    paths: Vec<Vec<u8>>,
}

#[derive(Clone)]
struct Entry {
    start: u64,
    end: u64,
    path: usize,
    line: u64,
    column: u64,
    discriminator: u64,
}

#[derive(Clone)]
struct FileEntry {
    name: Vec<u8>,
    directory: u64,
}

#[derive(Clone, Copy)]
struct Format {
    content: u64,
    form: u64,
    implicit: Option<i64>,
}

struct State {
    address: u64,
    op_index: u64,
    file: u64,
    line: i64,
    column: u64,
    discriminator: u64,
    pending: Option<Pending>,
}

struct Pending {
    address: u64,
    path: usize,
    line: u64,
    column: u64,
    discriminator: u64,
}

struct Builder {
    entries: Vec<Entry>,
    paths: Vec<Vec<u8>>,
    path_ids: BTreeMap<Vec<u8>, usize>,
    retained: usize,
    transient: usize,
    limit: usize,
    unit_files: usize,
}

impl Lines {
    pub fn resolve(&self, address: u64) -> Option<BorrowedLocation<'_>> {
        let mut at = self
            .entries
            .partition_point(|entry| entry.start <= address)
            .checked_sub(1)?;
        loop {
            let entry = self.entries.get(at)?;
            if address < entry.end {
                return Some(BorrowedLocation {
                    file: self.paths.get(entry.path)?.as_slice(),
                    line: entry.line,
                    column: entry.column,
                    discriminator: entry.discriminator,
                });
            }
            if at == 0 || self.max_ends.get(at.checked_sub(1)?)? <= &address {
                return None;
            }
            at = at.saturating_sub(1);
        }
    }

    pub fn heap_bytes(&self) -> usize {
        self.entries
            .capacity()
            .saturating_mul(std::mem::size_of::<Entry>())
            .saturating_add(
                self.max_ends
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u64>()),
            )
            .saturating_add(
                self.paths
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Vec<u8>>()),
            )
            .saturating_add(
                self.paths
                    .iter()
                    .fold(0usize, |total, path| total.saturating_add(path.capacity())),
            )
    }
}

pub fn parse<'a>(
    line: &'a [u8],
    line_strings: &'a [u8],
    debug_strings: &'a [u8],
    heap_limit: usize,
) -> Result<Lines, String> {
    let mut cursor = Cursor::new(line);
    let mut builder = Builder {
        entries: Vec::new(),
        paths: Vec::new(),
        path_ids: BTreeMap::new(),
        retained: FIXED_PARSER_HEAP_BYTES,
        transient: 0,
        limit: heap_limit,
        unit_files: 0,
    };
    if builder.retained > builder.limit {
        return Err(format!(
            "parsed DWARF line table expands beyond {} bytes",
            builder.limit
        ));
    }
    let mut units = 0usize;
    while !cursor.empty() {
        let remaining = cursor.remaining()?;
        if (remaining.len() < 4 || remaining.get(..4) == Some(&[0, 0, 0, 0]))
            && remaining.iter().all(|byte| *byte == 0)
        {
            break;
        }
        parse_unit(&mut cursor, line_strings, debug_strings, &mut builder)?;
        units = units
            .checked_add(1)
            .ok_or("DWARF line-unit count overflow")?;
    }
    if units == 0 {
        return Err("DWARF line section contains no units".into());
    }
    builder.entries.sort_unstable_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then_with(|| left.end.cmp(&right.end))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
            .then_with(|| left.discriminator.cmp(&right.discriminator))
    });
    builder.entries.dedup_by(|left, right| {
        left.start == right.start
            && left.end == right.end
            && left.path == right.path
            && left.line == right.line
            && left.column == right.column
            && left.discriminator == right.discriminator
    });
    let max_end_bytes = builder
        .entries
        .len()
        .checked_mul(std::mem::size_of::<u64>())
        .ok_or("DWARF line max-end byte count overflow")?;
    builder.claim(max_end_bytes)?;
    let mut max_ends = Vec::new();
    max_ends
        .try_reserve_exact(builder.entries.len())
        .map_err(|_| "cannot allocate bounded DWARF line max-end index")?;
    let mut max_end = 0u64;
    for entry in &builder.entries {
        max_end = max_end.max(entry.end);
        max_ends.push(max_end);
    }
    drop(builder.path_ids);
    let lines = Lines {
        entries: builder.entries,
        max_ends,
        paths: builder.paths,
    };
    if lines.heap_bytes() > heap_limit {
        return Err(format!(
            "parsed DWARF line table expands beyond {heap_limit} bytes"
        ));
    }
    Ok(lines)
}

fn parse_unit(
    section: &mut Cursor<'_>,
    line_strings: &[u8],
    debug_strings: &[u8],
    builder: &mut Builder,
) -> Result<(), String> {
    builder.begin_unit()?;
    let initial = section.u32()?;
    let (length, offset_size) = if initial == u32::MAX {
        (section.u64()?, 8usize)
    } else if initial >= 0xffff_fff0 {
        return Err("DWARF line unit uses a reserved initial length".into());
    } else {
        (u64::from(initial), 4usize)
    };
    if length == 0 {
        return Err("DWARF line unit is empty".into());
    }
    let unit_end = section.end_after(length, "DWARF line unit")?;
    let version = section.u16()?;
    if !(2..=5).contains(&version) {
        return Err(format!("unsupported DWARF line version {version}"));
    }
    let (address_size, segment_size) = if version == 5 {
        (usize::from(section.byte()?), section.byte()?)
    } else {
        (8usize, 0)
    };
    if !matches!(address_size, 1 | 2 | 4 | 8) || segment_size != 0 {
        return Err("unsupported DWARF line address or segment size".into());
    }
    let header_length = section.unsigned(offset_size)?;
    let header_end = section.end_after(header_length, "DWARF line header")?;
    if header_end > unit_end {
        return Err("DWARF line header runs past its unit".into());
    }
    let mut header = Cursor::range(section.bytes, section.at, header_end)?;
    let minimum_instruction_length = u64::from(header.byte()?);
    let maximum_operations = if version >= 4 {
        u64::from(header.byte()?)
    } else {
        1
    };
    let _default_is_statement = header.byte()?;
    let line_base = i64::from(header.byte()? as i8);
    let line_range = u64::from(header.byte()?);
    let opcode_base = header.byte()?;
    if minimum_instruction_length == 0
        || maximum_operations == 0
        || line_range == 0
        || opcode_base == 0
    {
        return Err("DWARF line header contains an invalid zero field".into());
    }
    let mut standard_lengths = Vec::new();
    for _ in 1..opcode_base {
        standard_lengths.push(header.byte()?);
    }
    const STANDARD_OPERANDS: [u8; 12] = [0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];
    if standard_lengths
        .iter()
        .zip(STANDARD_OPERANDS)
        .any(|(actual, expected)| *actual != expected)
    {
        return Err("DWARF line header has noncanonical standard-opcode operands".into());
    }
    let (directories, mut files, file_formats) = if version == 5 {
        let directory_formats = read_formats(&mut header)?;
        let directories = read_directories(
            &mut header,
            &directory_formats,
            offset_size,
            address_size,
            line_strings,
            debug_strings,
            builder,
        )?;
        let file_formats = read_formats(&mut header)?;
        let files = read_files_v5(
            &mut header,
            &file_formats,
            offset_size,
            address_size,
            line_strings,
            debug_strings,
            builder,
        )?;
        (directories, files, file_formats)
    } else {
        let directories = read_directories_legacy(&mut header, builder)?;
        let files = read_files_legacy(&mut header, builder)?;
        (directories, files, Vec::new())
    };
    section.at = header_end;
    let mut program = Cursor::range(section.bytes, header_end, unit_end)?;
    let mut state = State {
        address: 0,
        op_index: 0,
        file: 1,
        line: 1,
        column: 0,
        discriminator: 0,
        pending: None,
    };
    while !program.empty() {
        let opcode = program.byte()?;
        if opcode == 0 {
            let extended_length = program.uleb()?;
            if extended_length == 0 {
                return Err("DWARF extended line opcode is empty".into());
            }
            let extended_end = program.end_after(extended_length, "DWARF extended line opcode")?;
            let mut extended = Cursor::range(program.bytes, program.at, extended_end)?;
            let kind = extended.byte()?;
            match kind {
                1 => {
                    close_sequence(&mut state, builder)?;
                    state = State {
                        address: 0,
                        op_index: 0,
                        file: 1,
                        line: 1,
                        column: 0,
                        discriminator: 0,
                        pending: None,
                    };
                }
                2 => {
                    state.address = extended.unsigned(address_size)?;
                    state.op_index = 0;
                }
                3 => {
                    let file = if version == 5 {
                        read_file_v5(
                            &mut extended,
                            &file_formats,
                            offset_size,
                            address_size,
                            line_strings,
                            debug_strings,
                            builder,
                        )?
                    } else {
                        read_file_legacy(&mut extended, builder)?
                    };
                    files.push(file);
                }
                4 => state.discriminator = extended.uleb()?,
                _ => {}
            }
            program.at = extended_end;
            continue;
        }
        if opcode < opcode_base {
            match opcode {
                1 => emit(&mut state, version, &directories, &files, builder)?,
                2 => advance_address(
                    &mut state,
                    program.uleb()?,
                    minimum_instruction_length,
                    maximum_operations,
                )?,
                3 => {
                    state.line = state
                        .line
                        .checked_add(program.sleb()?)
                        .ok_or("DWARF line number overflow")?;
                }
                4 => state.file = program.uleb()?,
                5 => state.column = program.uleb()?,
                6 | 7 | 10 | 11 => {}
                8 => {
                    let advance = u64::from(255u8.saturating_sub(opcode_base)) / line_range;
                    advance_address(
                        &mut state,
                        advance,
                        minimum_instruction_length,
                        maximum_operations,
                    )?;
                }
                9 => {
                    state.address = state
                        .address
                        .checked_add(u64::from(program.u16()?))
                        .ok_or("DWARF line address overflow")?;
                    state.op_index = 0;
                }
                12 => {
                    let _ = program.uleb()?;
                }
                other => {
                    let count = standard_lengths
                        .get(usize::from(other.saturating_sub(1)))
                        .copied()
                        .ok_or("DWARF standard opcode has no operand descriptor")?;
                    for _ in 0..count {
                        let _ = program.uleb()?;
                    }
                }
            }
            continue;
        }
        let adjusted = u64::from(opcode - opcode_base);
        advance_address(
            &mut state,
            adjusted / line_range,
            minimum_instruction_length,
            maximum_operations,
        )?;
        let delta = line_base
            .checked_add(i64::try_from(adjusted % line_range).map_err(|_| "line delta overflow")?)
            .ok_or("DWARF line delta overflow")?;
        state.line = state
            .line
            .checked_add(delta)
            .ok_or("DWARF line number overflow")?;
        emit(&mut state, version, &directories, &files, builder)?;
    }
    if state.pending.is_some() {
        return Err("DWARF line program ends without end_sequence".into());
    }
    drop(files);
    drop(directories);
    builder.finish_unit();
    section.at = unit_end;
    Ok(())
}

fn advance_address(
    state: &mut State,
    operation_advance: u64,
    minimum_instruction_length: u64,
    maximum_operations: u64,
) -> Result<(), String> {
    let operations = state
        .op_index
        .checked_add(operation_advance)
        .ok_or("DWARF operation index overflow")?;
    let instructions = operations / maximum_operations;
    state.address = state
        .address
        .checked_add(
            minimum_instruction_length
                .checked_mul(instructions)
                .ok_or("DWARF line address advance overflow")?,
        )
        .ok_or("DWARF line address overflow")?;
    state.op_index = operations % maximum_operations;
    Ok(())
}

fn emit(
    state: &mut State,
    version: u16,
    directories: &[Vec<u8>],
    files: &[FileEntry],
    builder: &mut Builder,
) -> Result<(), String> {
    if let Some(pending) = state.pending.take() {
        push_range(pending, state.address, builder)?;
    }
    if state.line > 0 {
        let file_index = if version == 5 {
            usize::try_from(state.file).map_err(|_| "DWARF file index overflow")?
        } else {
            usize::try_from(
                state
                    .file
                    .checked_sub(1)
                    .ok_or("DWARF file index is zero")?,
            )
            .map_err(|_| "DWARF file index overflow")?
        };
        let file = files
            .get(file_index)
            .ok_or("DWARF line program names an absent file")?;
        let (path, transient_bytes) = joined_path(version, directories, file, builder)?;
        let path = builder.intern(path, transient_bytes)?;
        state.pending = Some(Pending {
            address: state.address,
            path,
            line: u64::try_from(state.line).map_err(|_| "DWARF line number is negative")?,
            column: state.column,
            discriminator: state.discriminator,
        });
    }
    state.discriminator = 0;
    Ok(())
}

fn close_sequence(state: &mut State, builder: &mut Builder) -> Result<(), String> {
    if let Some(pending) = state.pending.take() {
        push_range(pending, state.address, builder)?;
    }
    Ok(())
}

fn push_range(pending: Pending, end: u64, builder: &mut Builder) -> Result<(), String> {
    if end < pending.address {
        return Err("DWARF line sequence moves backwards".into());
    }
    if end == pending.address {
        return Ok(());
    }
    if builder.entries.len() >= MAX_LINE_ROWS {
        return Err(format!("DWARF line table exceeds {MAX_LINE_ROWS} rows"));
    }
    builder.reserve_entry()?;
    builder.entries.push(Entry {
        start: pending.address,
        end,
        path: pending.path,
        line: pending.line,
        column: pending.column,
        discriminator: pending.discriminator,
    });
    Ok(())
}

fn joined_path(
    version: u16,
    directories: &[Vec<u8>],
    file: &FileEntry,
    builder: &mut Builder,
) -> Result<(Vec<u8>, usize), String> {
    if file.name.starts_with(b"/") {
        builder.claim_transient(file.name.len())?;
        let mut path = Vec::new();
        path.try_reserve_exact(file.name.len())
            .map_err(|_| "cannot allocate bounded DWARF source path")?;
        path.extend_from_slice(&file.name);
        return Ok((path, file.name.len()));
    }
    let directory = if version == 5 {
        usize::try_from(file.directory)
            .ok()
            .and_then(|index| directories.get(index))
    } else if file.directory == 0 {
        None
    } else {
        usize::try_from(file.directory.saturating_sub(1))
            .ok()
            .and_then(|index| directories.get(index))
    };
    let Some(directory) = directory else {
        if file.directory != 0 || version == 5 {
            return Err("DWARF file names an absent directory".into());
        }
        builder.claim_transient(file.name.len())?;
        let mut path = Vec::new();
        path.try_reserve_exact(file.name.len())
            .map_err(|_| "cannot allocate bounded DWARF source path")?;
        path.extend_from_slice(&file.name);
        return Ok((path, file.name.len()));
    };
    let separator = usize::from(!directory.is_empty() && !directory.ends_with(b"/"));
    let length = directory
        .len()
        .checked_add(separator)
        .and_then(|value| value.checked_add(file.name.len()))
        .ok_or("DWARF source path length overflow")?;
    if length > MAX_LINE_PATH_BYTES {
        return Err(format!(
            "DWARF source path exceeds {MAX_LINE_PATH_BYTES} bytes"
        ));
    }
    builder.claim_transient(length)?;
    let mut path = Vec::new();
    path.try_reserve_exact(length)
        .map_err(|_| "cannot allocate bounded DWARF source path")?;
    path.extend_from_slice(directory);
    if separator != 0 {
        path.push(b'/');
    }
    path.extend_from_slice(&file.name);
    Ok((path, length))
}

fn read_directories_legacy(
    cursor: &mut Cursor<'_>,
    builder: &mut Builder,
) -> Result<Vec<Vec<u8>>, String> {
    let mut directories = Vec::new();
    loop {
        let value = cursor.cstring(MAX_LINE_PATH_BYTES)?;
        if value.is_empty() {
            break;
        }
        builder.claim_transient(value.len().saturating_add(64))?;
        directories.push(value.to_vec());
        builder.count_file()?;
    }
    Ok(directories)
}

fn read_files_legacy(
    cursor: &mut Cursor<'_>,
    builder: &mut Builder,
) -> Result<Vec<FileEntry>, String> {
    let mut files = Vec::new();
    loop {
        let at = cursor.at;
        let name = cursor.cstring(MAX_LINE_PATH_BYTES)?;
        if name.is_empty() {
            break;
        }
        cursor.at = at;
        files.push(read_file_legacy(cursor, builder)?);
    }
    Ok(files)
}

fn read_file_legacy(cursor: &mut Cursor<'_>, builder: &mut Builder) -> Result<FileEntry, String> {
    let name = cursor.cstring(MAX_LINE_PATH_BYTES)?;
    if name.is_empty() {
        return Err("DWARF define_file has an empty name".into());
    }
    let directory = cursor.uleb()?;
    let _timestamp = cursor.uleb()?;
    let _size = cursor.uleb()?;
    builder.claim_transient(name.len().saturating_add(64))?;
    builder.count_file()?;
    Ok(FileEntry {
        name: name.to_vec(),
        directory,
    })
}

fn read_formats(cursor: &mut Cursor<'_>) -> Result<Vec<Format>, String> {
    let count = usize::from(cursor.byte()?);
    if count == 0 || count > MAX_FORMAT_FIELDS {
        return Err("DWARF line table has an invalid format count".into());
    }
    let mut formats = Vec::with_capacity(count);
    for _ in 0..count {
        let content = cursor.uleb()?;
        let form = cursor.uleb()?;
        let implicit = if form == 0x21 {
            Some(cursor.sleb()?)
        } else {
            None
        };
        formats.push(Format {
            content,
            form,
            implicit,
        });
    }
    Ok(formats)
}

#[allow(clippy::too_many_arguments)]
fn read_directories<'a>(
    cursor: &mut Cursor<'a>,
    formats: &[Format],
    offset_size: usize,
    address_size: usize,
    line_strings: &'a [u8],
    debug_strings: &'a [u8],
    builder: &mut Builder,
) -> Result<Vec<Vec<u8>>, String> {
    let count = usize::try_from(cursor.uleb()?).map_err(|_| "DWARF directory count overflow")?;
    if count > MAX_LINE_FILES {
        return Err(format!(
            "DWARF line table exceeds {MAX_LINE_FILES} directories"
        ));
    }
    builder.claim_transient(
        count
            .saturating_mul(std::mem::size_of::<Vec<u8>>())
            .saturating_add(64),
    )?;
    let mut directories = Vec::new();
    directories
        .try_reserve_exact(count)
        .map_err(|_| "cannot allocate bounded DWARF directory roster")?;
    for _ in 0..count {
        let mut path = None;
        for format in formats {
            let value = read_form(
                cursor,
                *format,
                offset_size,
                address_size,
                line_strings,
                debug_strings,
            )?;
            if format.content == 1 {
                path = value.bytes;
            }
        }
        let path = path.ok_or("DWARF directory table has no path field")?;
        if path.len() > MAX_LINE_PATH_BYTES {
            return Err(format!(
                "DWARF source path exceeds {MAX_LINE_PATH_BYTES} bytes"
            ));
        }
        builder.claim_transient(path.len().saturating_add(64))?;
        builder.count_file()?;
        directories.push(path.to_vec());
    }
    Ok(directories)
}

#[allow(clippy::too_many_arguments)]
fn read_files_v5<'a>(
    cursor: &mut Cursor<'a>,
    formats: &[Format],
    offset_size: usize,
    address_size: usize,
    line_strings: &'a [u8],
    debug_strings: &'a [u8],
    builder: &mut Builder,
) -> Result<Vec<FileEntry>, String> {
    let count = usize::try_from(cursor.uleb()?).map_err(|_| "DWARF file count overflow")?;
    if count > MAX_LINE_FILES {
        return Err(format!("DWARF line table exceeds {MAX_LINE_FILES} files"));
    }
    builder.claim_transient(
        count
            .saturating_mul(std::mem::size_of::<FileEntry>())
            .saturating_add(64),
    )?;
    let mut files = Vec::new();
    files
        .try_reserve_exact(count)
        .map_err(|_| "cannot allocate bounded DWARF file roster")?;
    for _ in 0..count {
        files.push(read_file_v5(
            cursor,
            formats,
            offset_size,
            address_size,
            line_strings,
            debug_strings,
            builder,
        )?);
    }
    Ok(files)
}

#[allow(clippy::too_many_arguments)]
fn read_file_v5<'a>(
    cursor: &mut Cursor<'a>,
    formats: &[Format],
    offset_size: usize,
    address_size: usize,
    line_strings: &'a [u8],
    debug_strings: &'a [u8],
    builder: &mut Builder,
) -> Result<FileEntry, String> {
    let mut name = None;
    let mut directory = 0u64;
    for format in formats {
        let value = read_form(
            cursor,
            *format,
            offset_size,
            address_size,
            line_strings,
            debug_strings,
        )?;
        match format.content {
            1 => name = value.bytes,
            2 => {
                directory = value
                    .unsigned
                    .ok_or("DWARF directory index is not numeric")?
            }
            _ => {}
        }
    }
    let name = name.ok_or("DWARF file table has no path field")?;
    if name.is_empty() || name.len() > MAX_LINE_PATH_BYTES {
        return Err("DWARF file table has an invalid path".into());
    }
    builder.claim_transient(name.len().saturating_add(64))?;
    builder.count_file()?;
    Ok(FileEntry {
        name: name.to_vec(),
        directory,
    })
}

struct FormValue<'a> {
    bytes: Option<&'a [u8]>,
    unsigned: Option<u64>,
}

fn read_form<'a>(
    cursor: &mut Cursor<'a>,
    format: Format,
    offset_size: usize,
    address_size: usize,
    line_strings: &'a [u8],
    debug_strings: &'a [u8],
) -> Result<FormValue<'a>, String> {
    let empty = || FormValue {
        bytes: None,
        unsigned: None,
    };
    let unsigned = |value| FormValue {
        bytes: None,
        unsigned: Some(value),
    };
    match format.form {
        0x01 => Ok(unsigned(cursor.unsigned(address_size)?)),
        0x03 => {
            let length = usize::from(cursor.u16()?);
            let _ = cursor.take(length)?;
            Ok(empty())
        }
        0x04 => {
            let length = usize::try_from(cursor.u32()?).map_err(|_| "DWARF block overflow")?;
            let _ = cursor.take(length)?;
            Ok(empty())
        }
        0x05 => Ok(unsigned(u64::from(cursor.u16()?))),
        0x06 => Ok(unsigned(u64::from(cursor.u32()?))),
        0x07 => Ok(unsigned(cursor.u64()?)),
        0x08 => Ok(FormValue {
            bytes: Some(cursor.cstring(MAX_LINE_PATH_BYTES)?),
            unsigned: None,
        }),
        0x09 | 0x18 => {
            let length = usize::try_from(cursor.uleb()?).map_err(|_| "DWARF block overflow")?;
            let _ = cursor.take(length)?;
            Ok(empty())
        }
        0x0a => {
            let length = usize::from(cursor.byte()?);
            let _ = cursor.take(length)?;
            Ok(empty())
        }
        0x0b | 0x0c => Ok(unsigned(u64::from(cursor.byte()?))),
        0x0d => {
            let value = cursor.sleb()?;
            Ok(FormValue {
                bytes: None,
                unsigned: u64::try_from(value).ok(),
            })
        }
        0x0e => {
            if debug_strings.is_empty() {
                return Err(DEBUG_STR_REQUIRED.into());
            }
            let at = usize::try_from(cursor.unsigned(offset_size)?)
                .map_err(|_| "DWARF string offset overflow")?;
            Ok(FormValue {
                bytes: Some(cstring_at(debug_strings, at)?),
                unsigned: None,
            })
        }
        0x0f | 0x15 | 0x1a | 0x1b | 0x22 | 0x23 => Ok(unsigned(cursor.uleb()?)),
        0x10 | 0x17 | 0x1d => Ok(unsigned(cursor.unsigned(offset_size)?)),
        0x11 | 0x25 | 0x29 => Ok(unsigned(u64::from(cursor.byte()?))),
        0x12 | 0x26 | 0x2a => Ok(unsigned(u64::from(cursor.u16()?))),
        0x13 | 0x1c | 0x28 | 0x2c => Ok(unsigned(u64::from(cursor.u32()?))),
        0x14 | 0x20 | 0x24 => Ok(unsigned(cursor.u64()?)),
        0x27 | 0x2b => Ok(unsigned(cursor.unsigned(3)?)),
        0x19 => Ok(unsigned(1)),
        0x1e => {
            let _ = cursor.take(16)?;
            Ok(empty())
        }
        0x1f => {
            let at = usize::try_from(cursor.unsigned(offset_size)?)
                .map_err(|_| "DWARF line-string offset overflow")?;
            Ok(FormValue {
                bytes: Some(cstring_at(line_strings, at)?),
                unsigned: None,
            })
        }
        0x21 => Ok(FormValue {
            bytes: None,
            unsigned: format.implicit.and_then(|value| u64::try_from(value).ok()),
        }),
        other => Err(format!("unsupported DWARF line-table form 0x{other:x}")),
    }
}

fn cstring_at(bytes: &[u8], at: usize) -> Result<&[u8], String> {
    let rest = bytes
        .get(at..)
        .ok_or("DWARF string offset is out of bounds")?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("unterminated DWARF string")?;
    if end > MAX_LINE_PATH_BYTES {
        return Err(format!("DWARF string exceeds {MAX_LINE_PATH_BYTES} bytes"));
    }
    rest.get(..end).ok_or("DWARF string slice overflow".into())
}

impl Builder {
    fn claim(&mut self, bytes: usize) -> Result<(), String> {
        let retained = self
            .retained
            .checked_add(bytes)
            .ok_or("DWARF line-table memory count overflow")?;
        if retained
            .checked_add(self.transient)
            .is_none_or(|total| total > self.limit)
        {
            return Err(format!(
                "parsed DWARF line table expands beyond {} bytes",
                self.limit
            ));
        }
        self.retained = retained;
        Ok(())
    }

    fn claim_transient(&mut self, bytes: usize) -> Result<(), String> {
        let transient = self
            .transient
            .checked_add(bytes)
            .ok_or("DWARF transient memory count overflow")?;
        if self
            .retained
            .checked_add(transient)
            .is_none_or(|total| total > self.limit)
        {
            return Err(format!(
                "parsed DWARF line table expands beyond {} bytes",
                self.limit
            ));
        }
        self.transient = transient;
        Ok(())
    }

    fn release_transient(&mut self, bytes: usize) -> Result<(), String> {
        self.transient = self
            .transient
            .checked_sub(bytes)
            .ok_or("DWARF transient memory accounting underflows")?;
        Ok(())
    }

    fn retain_transient(&mut self, bytes: usize) -> Result<(), String> {
        self.transient = self
            .transient
            .checked_sub(bytes)
            .ok_or("DWARF transient memory accounting underflows")?;
        self.retained = self
            .retained
            .checked_add(bytes)
            .ok_or("DWARF retained memory count overflow")?;
        Ok(())
    }

    fn begin_unit(&mut self) -> Result<(), String> {
        if self.transient != 0 || self.unit_files != 0 {
            return Err("DWARF line-unit memory accounting is not empty".into());
        }
        Ok(())
    }

    fn finish_unit(&mut self) {
        self.transient = 0;
        self.unit_files = 0;
    }

    fn count_file(&mut self) -> Result<(), String> {
        self.unit_files = self
            .unit_files
            .checked_add(1)
            .ok_or("DWARF file count overflow")?;
        if self.unit_files > MAX_LINE_FILES {
            return Err(format!("DWARF line table exceeds {MAX_LINE_FILES} files"));
        }
        Ok(())
    }

    fn reserve_entry(&mut self) -> Result<(), String> {
        if self.entries.len() < self.entries.capacity() {
            return Ok(());
        }
        let current = self.entries.capacity();
        let target = current
            .max(1)
            .checked_mul(2)
            .unwrap_or(MAX_LINE_ROWS)
            .min(MAX_LINE_ROWS);
        let growth = target
            .checked_sub(current)
            .and_then(|entries| entries.checked_mul(std::mem::size_of::<Entry>()))
            .ok_or("DWARF line-row capacity overflow")?;
        self.claim(growth)?;
        self.entries
            .try_reserve_exact(target.saturating_sub(self.entries.len()))
            .map_err(|_| "cannot allocate bounded DWARF line rows")?;
        Ok(())
    }

    fn intern(&mut self, path: Vec<u8>, transient_bytes: usize) -> Result<usize, String> {
        if path.is_empty() || path.len() > MAX_LINE_PATH_BYTES {
            return Err("DWARF source path has an invalid length".into());
        }
        if let Some(index) = self.path_ids.get(&path) {
            let index = *index;
            drop(path);
            self.release_transient(transient_bytes)?;
            return Ok(index);
        }
        let index = self.paths.len();
        self.retain_transient(transient_bytes)?;
        self.claim(path.len().saturating_add(160))?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(path.len())
            .map_err(|_| "cannot allocate bounded interned DWARF source path")?;
        copy.extend_from_slice(&path);
        self.paths
            .try_reserve_exact(1)
            .map_err(|_| "cannot allocate bounded DWARF source-path roster")?;
        self.paths.push(copy);
        self.path_ids.insert(path, index);
        Ok(index)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    end: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            end: bytes.len(),
        }
    }

    fn range(bytes: &'a [u8], at: usize, end: usize) -> Result<Self, String> {
        if at > end || end > bytes.len() {
            return Err("DWARF cursor range is out of bounds".into());
        }
        Ok(Self { bytes, at, end })
    }

    fn empty(&self) -> bool {
        self.at == self.end
    }

    fn remaining(&self) -> Result<&'a [u8], String> {
        self.bytes
            .get(self.at..self.end)
            .ok_or("DWARF remaining slice is out of bounds".into())
    }

    fn end_after(&self, length: u64, label: &str) -> Result<usize, String> {
        let length = usize::try_from(length).map_err(|_| format!("{label} length overflow"))?;
        let end = self
            .at
            .checked_add(length)
            .ok_or_else(|| format!("{label} range overflow"))?;
        if end > self.end {
            return Err(format!("{label} runs past its container"));
        }
        Ok(end)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(length).ok_or("DWARF cursor overflow")?;
        if end > self.end {
            return Err("truncated DWARF data".into());
        }
        let value = self.bytes.get(self.at..end).ok_or("DWARF slice overflow")?;
        self.at = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, String> {
        let value = *self.take(1)?.first().ok_or("truncated DWARF byte")?;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| "truncated DWARF u16")?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| "truncated DWARF u32")?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| "truncated DWARF u64")?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn unsigned(&mut self, width: usize) -> Result<u64, String> {
        match width {
            1 => Ok(u64::from(self.byte()?)),
            2 => Ok(u64::from(self.u16()?)),
            3 => {
                let bytes: [u8; 3] = self
                    .take(3)?
                    .try_into()
                    .map_err(|_| "truncated DWARF u24")?;
                let [low, middle, high] = bytes;
                Ok(u64::from(low) | (u64::from(middle) << 8) | (u64::from(high) << 16))
            }
            4 => Ok(u64::from(self.u32()?)),
            8 => self.u64(),
            _ => Err("unsupported DWARF integer width".into()),
        }
    }

    fn uleb(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            let payload = u64::from(byte & 0x7f);
            if shift == 63 && payload > 1 {
                return Err("DWARF ULEB128 overflows u64".into());
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("DWARF ULEB128 is too long".into())
    }

    fn sleb(&mut self) -> Result<i64, String> {
        let mut value = 0i128;
        let mut shift = 0u32;
        loop {
            if shift >= 70 {
                return Err("DWARF SLEB128 is too long".into());
            }
            let byte = self.byte()?;
            value |= i128::from(byte & 0x7f) << shift;
            shift = shift.saturating_add(7);
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    value |= -1i128 << shift;
                }
                return i64::try_from(value).map_err(|_| "DWARF SLEB128 overflows i64".into());
            }
        }
    }

    fn cstring(&mut self, maximum: usize) -> Result<&'a [u8], String> {
        let rest = self
            .bytes
            .get(self.at..self.end)
            .ok_or("DWARF string cursor is out of bounds")?;
        let length = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("unterminated DWARF string")?;
        if length > maximum {
            return Err(format!("DWARF string exceeds {maximum} bytes"));
        }
        let value = rest.get(..length).ok_or("DWARF string slice overflow")?;
        self.at = self
            .at
            .checked_add(length.saturating_add(1))
            .ok_or("DWARF string cursor overflow")?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        joined_path, parse, read_form, Builder, Cursor, FileEntry, Format, FIXED_PARSER_HEAP_BYTES,
    };

    fn unit_v4() -> Vec<u8> {
        let mut header = vec![1, 1, 1, 0xfb, 14, 13];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        header.extend_from_slice(b"src\0\0main.rs\0\x01\0\0\0");
        let mut program = vec![0, 9, 2];
        program.extend_from_slice(&0x1000u64.to_le_bytes());
        program.extend_from_slice(&[3, 9, 1, 2, 4, 3, 1, 1, 0, 1, 1]);
        let length = 2usize + 4 + header.len() + program.len();
        let mut unit = Vec::new();
        unit.extend_from_slice(&(u32::try_from(length).unwrap()).to_le_bytes());
        unit.extend_from_slice(&4u16.to_le_bytes());
        unit.extend_from_slice(&(u32::try_from(header.len()).unwrap()).to_le_bytes());
        unit.extend_from_slice(&header);
        unit.extend_from_slice(&program);
        unit
    }

    #[test]
    fn resolves_half_open_v4_rows_and_paths() {
        let lines = parse(&unit_v4(), &[], &[], 1024 * 1024).unwrap();
        assert_eq!(lines.resolve(0x0fff), None);
        let first = lines.resolve(0x1000).unwrap();
        assert_eq!(first.file, b"src/main.rs");
        assert_eq!(first.line, 10);
        assert_eq!(lines.resolve(0x1003).unwrap().line, 10);
        assert_eq!(lines.resolve(0x1004), None);
    }

    #[test]
    fn malformed_or_over_budget_tables_fail_closed() {
        let mut malformed = unit_v4();
        let _ = malformed.pop();
        assert!(parse(&malformed, &[], &[], 1024 * 1024).is_err());
        assert!(parse(&unit_v4(), &[], &[], 1).is_err());
    }

    #[test]
    fn trailing_zero_padding_and_multiple_units_are_tolerated() {
        let mut bytes = unit_v4();
        bytes.extend_from_slice(&unit_v4());
        bytes.extend_from_slice(&[0; 7]);
        let lines = parse(&bytes, &[], &[], 1024 * 1024).unwrap();
        assert_eq!(lines.resolve(0x1000).unwrap().line, 10);
    }

    #[test]
    fn resolves_multiple_sequences_in_one_unit() {
        let mut bytes = unit_v4();
        let mut second = vec![0, 9, 2];
        second.extend_from_slice(&0x2000u64.to_le_bytes());
        second.extend_from_slice(&[3, 19, 1, 2, 2, 0, 1, 1]);
        let length = u32::from_le_bytes(bytes.get(..4).unwrap().try_into().unwrap());
        bytes.get_mut(..4).unwrap().copy_from_slice(
            &length
                .checked_add(u32::try_from(second.len()).unwrap())
                .unwrap()
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&second);

        let lines = parse(&bytes, &[], &[], 1024 * 1024).unwrap();
        assert_eq!(lines.resolve(0x1000).unwrap().line, 10);
        assert_eq!(lines.resolve(0x2000).unwrap().line, 20);
        assert_eq!(lines.resolve(0x2002), None);
    }

    #[test]
    fn repeated_paths_and_unit_rosters_are_peak_accounted() {
        let name = vec![b'x'; 64];
        let file = FileEntry { name, directory: 0 };
        let mut builder = Builder {
            entries: Vec::new(),
            paths: Vec::new(),
            path_ids: std::collections::BTreeMap::new(),
            retained: FIXED_PARSER_HEAP_BYTES,
            transient: 0,
            limit: FIXED_PARSER_HEAP_BYTES + 480,
            unit_files: 0,
        };
        for _ in 0..100 {
            builder.begin_unit().unwrap();
            builder
                .claim_transient(file.name.len().saturating_add(64))
                .unwrap();
            builder.count_file().unwrap();
            for _ in 0..10_000 {
                let (path, transient) = joined_path(4, &[], &file, &mut builder).unwrap();
                assert_eq!(builder.intern(path, transient).unwrap(), 0);
            }
            builder.finish_unit();
        }
        assert_eq!(builder.paths, vec![file.name]);
        assert_eq!(builder.transient, 0);
        assert_eq!(builder.unit_files, 0);
    }

    #[test]
    fn indexed_forms_consume_their_exact_dwarf_five_widths() {
        for (form, width, expected) in [
            (0x27, 3, 0x03_02_01),
            (0x2b, 3, 0x03_02_01),
            (0x28, 4, 0x04_03_02_01),
            (0x2c, 4, 0x04_03_02_01),
        ] {
            let bytes = [1, 2, 3, 4, 0xaa, 0xbb, 0xcc, 0xdd];
            let mut cursor = Cursor::new(&bytes);
            let value = read_form(
                &mut cursor,
                Format {
                    content: 0,
                    form,
                    implicit: None,
                },
                4,
                8,
                &[],
                &[],
            )
            .unwrap();
            assert_eq!(cursor.at, width);
            assert_eq!(value.unsigned, Some(expected));
        }

        let mut cursor = Cursor::new(&[0x81, 0x01, 0xff]);
        let value = read_form(
            &mut cursor,
            Format {
                content: 0,
                form: 0x1b,
                implicit: None,
            },
            4,
            8,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(cursor.at, 2);
        assert_eq!(value.unsigned, Some(129));
    }

    #[test]
    fn strp_reads_the_external_debug_string_section() {
        let offset = 5u32.to_le_bytes();
        let mut cursor = Cursor::new(&offset);
        let value = read_form(
            &mut cursor,
            Format {
                content: 0,
                form: 0x0e,
                implicit: None,
            },
            4,
            8,
            &[],
            b"zero\0src/lib.rs\0",
        )
        .unwrap();
        assert_eq!(cursor.at, 4);
        assert_eq!(value.bytes, Some(b"src/lib.rs".as_slice()));
    }

    #[test]
    fn resolves_v5_line_string_forms_and_zero_based_directories() {
        let mut header = vec![1, 1, 1, 0xfb, 14, 13];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        header.extend_from_slice(&[1, 1, 0x1f, 1]);
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&[2, 1, 0x1f, 2, 0x0b, 2]);
        header.extend_from_slice(&4u32.to_le_bytes());
        header.push(0);
        header.extend_from_slice(&4u32.to_le_bytes());
        header.push(0);
        let mut program = vec![4, 0, 0, 9, 2];
        program.extend_from_slice(&0x2000u64.to_le_bytes());
        program.extend_from_slice(&[3, 20, 1, 2, 3, 0, 1, 1]);
        let length = 2usize + 2 + 4 + header.len() + program.len();
        let mut unit = Vec::new();
        unit.extend_from_slice(&(u32::try_from(length).unwrap()).to_le_bytes());
        unit.extend_from_slice(&5u16.to_le_bytes());
        unit.extend_from_slice(&[8, 0]);
        unit.extend_from_slice(&(u32::try_from(header.len()).unwrap()).to_le_bytes());
        unit.extend_from_slice(&header);
        unit.extend_from_slice(&program);
        let lines = parse(&unit, b"src\0main.c\0", &[], 1024 * 1024).unwrap();
        let location = lines.resolve(0x2001).unwrap();
        assert_eq!(location.file, b"src/main.c");
        assert_eq!(location.line, 21);
        assert_eq!(lines.resolve(0x2003), None);
    }
}
