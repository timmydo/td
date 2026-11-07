//! SFTP version 3, in-process as the logged-in account, with that account's
//! ordinary filesystem permissions (DESIGN.md section 6). It is a post-auth
//! parser of an authenticated peer's input, so it is bounded but not a boundary:
//! paths are not jailed.

use crate::conn::{Credit, Event, EventSender};
use crate::wire::{Reader, Writer};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, FileTimes, Metadata, OpenOptions, ReadDir};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// `MKDIR` without attributes, before the umask floor.
const DEFAULT_DIRECTORY_MODE: u32 = 0o777;
const VERSION: u32 = 3;
const FX_INIT: u8 = 1;
const FX_VERSION: u8 = 2;
const FX_OPEN: u8 = 3;
const FX_CLOSE: u8 = 4;
const FX_READ: u8 = 5;
const FX_WRITE: u8 = 6;
const FX_LSTAT: u8 = 7;
const FX_FSTAT: u8 = 8;
const FX_SETSTAT: u8 = 9;
const FX_FSETSTAT: u8 = 10;
const FX_OPENDIR: u8 = 11;
const FX_READDIR: u8 = 12;
const FX_REMOVE: u8 = 13;
const FX_MKDIR: u8 = 14;
const FX_RMDIR: u8 = 15;
const FX_REALPATH: u8 = 16;
const FX_STAT: u8 = 17;
const FX_RENAME: u8 = 18;
const FX_READLINK: u8 = 19;
const FX_SYMLINK: u8 = 20;
const FX_STATUS: u8 = 101;
const FX_HANDLE: u8 = 102;
const FX_DATA: u8 = 103;
const FX_NAME: u8 = 104;
const FX_ATTRS: u8 = 105;
const FX_EXTENDED: u8 = 200;
const FX_EXTENDED_REPLY: u8 = 201;

const STATUS_OK: u32 = 0;
const STATUS_EOF: u32 = 1;
const STATUS_NO_SUCH_FILE: u32 = 2;
const STATUS_PERMISSION_DENIED: u32 = 3;
const STATUS_FAILURE: u32 = 4;
const STATUS_BAD_MESSAGE: u32 = 5;
const STATUS_OP_UNSUPPORTED: u32 = 8;

const PF_READ: u32 = 1;
const PF_WRITE: u32 = 2;
const PF_APPEND: u32 = 4;
const PF_CREAT: u32 = 8;
const PF_TRUNC: u32 = 0x10;
const PF_EXCL: u32 = 0x20;

const ATTR_SIZE: u32 = 1;
const ATTR_UIDGID: u32 = 2;
const ATTR_PERMISSIONS: u32 = 4;
const ATTR_ACMODTIME: u32 = 8;

const MAX_REQUEST: usize = 256 * 1024 + 1024;
const MAX_IO: u32 = 256 * 1024;
const MAX_HANDLES: usize = 64;
const READDIR_BATCH: usize = 128;

/// The extensions this server names in its `VERSION` reply.
const EXTENSIONS: &[(&str, &str)] = &[
    ("posix-rename@openssh.com", "1"),
    ("fsync@openssh.com", "1"),
    ("limits@openssh.com", "1"),
    ("expand-path@openssh.com", "1"),
];

/// Channel input as a byte stream. The window reopens as bytes are consumed.
pub struct ChannelReader {
    channel: u32,
    receiver: Receiver<Vec<u8>>,
    events: EventSender,
    buffer: Vec<u8>,
    offset: usize,
}

impl ChannelReader {
    pub fn new(channel: u32, receiver: Receiver<Vec<u8>>, events: EventSender) -> Self {
        Self {
            channel,
            receiver,
            events,
            buffer: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.offset >= self.buffer.len() {
            match self.receiver.recv() {
                Ok(data) => {
                    let bytes = u32::try_from(data.len()).unwrap_or(u32::MAX);
                    let _ = self.events.send(Event::Consumed {
                        channel: self.channel,
                        bytes,
                    });
                    self.buffer = data;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let rest = self.buffer.get(self.offset..).unwrap_or_default();
        let count = rest.len().min(out.len());
        let (head, _) = out.split_at_mut(count);
        head.copy_from_slice(rest.get(..count).unwrap_or_default());
        self.offset += count;
        Ok(count)
    }
}

/// Channel output as a byte stream: the same credit a child's pipe blocks on.
pub struct ChannelWriter {
    channel: u32,
    max_packet: usize,
    credit: Arc<Credit>,
    events: EventSender,
}

impl ChannelWriter {
    pub fn new(channel: u32, max_packet: usize, credit: Arc<Credit>, events: EventSender) -> Self {
        Self {
            channel,
            max_packet: max_packet.max(1),
            credit,
            events,
        }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let wanted = data.len().min(self.max_packet);
        let Some(granted) = self.credit.take(wanted) else {
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        let event = Event::Output {
            channel: self.channel,
            stderr: false,
            data: data.get(..granted).unwrap_or_default().to_vec(),
        };
        self.events
            .send(event)
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(granted)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Handle {
    File(File),
    Directory {
        entries: Option<ReadDir>,
        path: PathBuf,
        sent_dots: bool,
    },
}

/// Runs until the channel closes or the peer sends something malformed.
pub fn serve(mut input: impl Read, mut output: impl Write, home: &str) {
    let mut server = Server {
        home: PathBuf::from(home),
        handles: HashMap::new(),
        next_handle: 0,
    };
    loop {
        let mut length = [0u8; 4];
        if input.read_exact(&mut length).is_err() {
            return;
        }
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_REQUEST {
            return;
        }
        let mut request = vec![0u8; length];
        if input.read_exact(&mut request).is_err() {
            return;
        }
        let Some(reply) = server.handle(&request) else {
            return;
        };
        let framed = u32::try_from(reply.len()).unwrap_or(u32::MAX).to_be_bytes();
        if output.write_all(&framed).is_err() || output.write_all(&reply).is_err() {
            return;
        }
    }
}

struct Server {
    home: PathBuf,
    handles: HashMap<u32, Handle>,
    next_handle: u32,
}

fn status_code(error: &io::Error) -> u32 {
    match error.kind() {
        io::ErrorKind::NotFound => STATUS_NO_SUCH_FILE,
        io::ErrorKind::PermissionDenied => STATUS_PERMISSION_DENIED,
        _ => STATUS_FAILURE,
    }
}

fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

fn status_reply(id: u32, code: u32, text: &str) -> Vec<u8> {
    let mut reply = Writer::message(FX_STATUS);
    reply.u32(id).u32(code).str(text).str("");
    reply.into_vec()
}

fn io_reply(id: u32, result: io::Result<()>) -> Vec<u8> {
    match result {
        Ok(()) => status_reply(id, STATUS_OK, "Success"),
        Err(error) => status_reply(id, status_code(&error), &error.to_string()),
    }
}

/// `ATTRS`, for a reply: everything `stat` knows, with times truncated to the
/// protocol's 32-bit seconds.
fn write_attrs(writer: &mut Writer, metadata: &Metadata) {
    writer.u32(ATTR_SIZE | ATTR_UIDGID | ATTR_PERMISSIONS | ATTR_ACMODTIME);
    writer.u64(metadata.len());
    writer.u32(metadata.uid()).u32(metadata.gid());
    writer.u32(metadata.mode());
    writer.u32(metadata.atime() as u32);
    writer.u32(metadata.mtime() as u32);
}

#[derive(Default)]
struct Attrs {
    size: Option<u64>,
    owner: Option<(u32, u32)>,
    mode: Option<u32>,
    times: Option<(u32, u32)>,
}

fn read_attrs(reader: &mut Reader<'_>) -> crate::error::Result<Attrs> {
    let flags = reader.u32()?;
    let mut attrs = Attrs::default();
    if flags & ATTR_SIZE != 0 {
        attrs.size = Some(reader.u64()?);
    }
    if flags & ATTR_UIDGID != 0 {
        attrs.owner = Some((reader.u32()?, reader.u32()?));
    }
    if flags & ATTR_PERMISSIONS != 0 {
        attrs.mode = Some(reader.u32()?);
    }
    if flags & ATTR_ACMODTIME != 0 {
        attrs.times = Some((reader.u32()?, reader.u32()?));
    }
    Ok(attrs)
}

fn file_times(atime: u32, mtime: u32) -> FileTimes {
    let at = |seconds: u32| SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(seconds));
    FileTimes::new()
        .set_accessed(at(atime))
        .set_modified(at(mtime))
}

fn apply_attrs_to_path(path: &Path, attrs: &Attrs) -> io::Result<()> {
    if let Some(size) = attrs.size {
        OpenOptions::new().write(true).open(path)?.set_len(size)?;
    }
    if let Some(mode) = attrs.mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let Some((uid, gid)) = attrs.owner {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }
    if let Some((atime, mtime)) = attrs.times {
        File::open(path)?.set_times(file_times(atime, mtime))?;
    }
    Ok(())
}

fn apply_attrs_to_file(file: &File, attrs: &Attrs) -> io::Result<()> {
    if let Some(size) = attrs.size {
        file.set_len(size)?;
    }
    if let Some(mode) = attrs.mode {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let Some((uid, gid)) = attrs.owner {
        std::os::unix::fs::fchown(file, Some(uid), Some(gid))?;
    }
    if let Some((atime, mtime)) = attrs.times {
        file.set_times(file_times(atime, mtime))?;
    }
    Ok(())
}

/// One `ls -l`-like line, which `sftp`'s `ls` prints as given.
fn long_name(name: &[u8], metadata: &Metadata) -> Vec<u8> {
    let mode = metadata.mode();
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let mut bits = String::with_capacity(9);
    for shift in [6u32, 3, 0] {
        let part = mode >> shift;
        bits.push(if part & 4 != 0 { 'r' } else { '-' });
        bits.push(if part & 2 != 0 { 'w' } else { '-' });
        bits.push(if part & 1 != 0 { 'x' } else { '-' });
    }
    let mut line = format!(
        "{kind}{bits} {:>3} {:<8} {:<8} {:>12} {} ",
        metadata.nlink(),
        metadata.uid(),
        metadata.gid(),
        metadata.len(),
        metadata.mtime(),
    )
    .into_bytes();
    line.extend_from_slice(name);
    line
}

/// Lexically removes `.` and `..` without touching the filesystem, for paths
/// `canonicalize` cannot resolve because their last components do not yet exist.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl Server {
    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.home.join(path)
        }
    }

    fn real(&self, path: &Path) -> PathBuf {
        let empty = path.as_os_str().is_empty();
        let target = if empty {
            self.home.clone()
        } else {
            self.absolute(path)
        };
        fs::canonicalize(&target).unwrap_or_else(|_| lexical(&target))
    }

    fn insert(&mut self, handle: Handle) -> Option<u32> {
        if self.handles.len() >= MAX_HANDLES {
            return None;
        }
        let id = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.handles.insert(id, handle);
        Some(id)
    }

    /// One reply, or None when the request cannot even be framed as one, which
    /// ends the subsystem.
    fn handle(&mut self, request: &[u8]) -> Option<Vec<u8>> {
        let mut reader = Reader::new(request);
        let kind = reader.u8().ok()?;
        if kind == FX_INIT {
            let mut reply = Writer::message(FX_VERSION);
            reply.u32(VERSION);
            for (name, value) in EXTENSIONS {
                reply.str(name).str(value);
            }
            return Some(reply.into_vec());
        }
        let id = reader.u32().ok()?;
        Some(
            self.dispatch(kind, id, &mut reader)
                .unwrap_or_else(|_| status_reply(id, STATUS_BAD_MESSAGE, "malformed request")),
        )
    }

    fn dispatch(
        &mut self,
        kind: u8,
        id: u32,
        reader: &mut Reader<'_>,
    ) -> crate::error::Result<Vec<u8>> {
        Ok(match kind {
            FX_OPEN => {
                let path = self.absolute(&path_of(reader.string()?));
                let flags = reader.u32()?;
                let attrs = read_attrs(reader)?;
                self.open(id, &path, flags, &attrs)
            }
            FX_CLOSE => {
                let handle = read_handle(reader)?;
                match self.handles.remove(&handle) {
                    Some(_) => status_reply(id, STATUS_OK, "Success"),
                    None => status_reply(id, STATUS_FAILURE, "no such handle"),
                }
            }
            FX_READ => {
                let handle = read_handle(reader)?;
                let offset = reader.u64()?;
                let length = reader.u32()?;
                self.read(id, handle, offset, length)
            }
            FX_WRITE => {
                let handle = read_handle(reader)?;
                let offset = reader.u64()?;
                let data = reader.string()?;
                self.write(id, handle, offset, data)
            }
            FX_LSTAT | FX_STAT => {
                let path = self.absolute(&path_of(reader.string()?));
                let metadata = if kind == FX_STAT {
                    fs::metadata(&path)
                } else {
                    fs::symlink_metadata(&path)
                };
                attrs_reply(id, metadata)
            }
            FX_FSTAT => {
                let handle = read_handle(reader)?;
                match self.handles.get(&handle) {
                    Some(Handle::File(file)) => attrs_reply(id, file.metadata()),
                    _ => status_reply(id, STATUS_FAILURE, "no such file handle"),
                }
            }
            FX_SETSTAT => {
                let path = self.absolute(&path_of(reader.string()?));
                let attrs = read_attrs(reader)?;
                io_reply(id, apply_attrs_to_path(&path, &attrs))
            }
            FX_FSETSTAT => {
                let handle = read_handle(reader)?;
                let attrs = read_attrs(reader)?;
                match self.handles.get(&handle) {
                    Some(Handle::File(file)) => io_reply(id, apply_attrs_to_file(file, &attrs)),
                    _ => status_reply(id, STATUS_FAILURE, "no such file handle"),
                }
            }
            FX_OPENDIR => {
                let path = self.absolute(&path_of(reader.string()?));
                match fs::read_dir(&path) {
                    Ok(entries) => {
                        let handle = Handle::Directory {
                            entries: Some(entries),
                            path,
                            sent_dots: false,
                        };
                        self.handle_reply(id, handle)
                    }
                    Err(error) => status_reply(id, status_code(&error), &error.to_string()),
                }
            }
            FX_READDIR => {
                let handle = read_handle(reader)?;
                self.read_directory(id, handle)
            }
            FX_REMOVE => {
                let path = self.absolute(&path_of(reader.string()?));
                io_reply(id, fs::remove_file(path))
            }
            FX_MKDIR => {
                let path = self.absolute(&path_of(reader.string()?));
                let attrs = read_attrs(reader)?;
                // Through the kernel, as sftp-server does, so the umask floor applies:
                // a chmod afterwards would not ask it.
                let mode = attrs
                    .mode
                    .map_or(DEFAULT_DIRECTORY_MODE, |mode| mode & 0o7777);
                io_reply(id, fs::DirBuilder::new().mode(mode).create(&path))
            }
            FX_RMDIR => {
                let path = self.absolute(&path_of(reader.string()?));
                io_reply(id, fs::remove_dir(path))
            }
            FX_REALPATH => {
                let path = path_of(reader.string()?);
                self.name_reply_for(id, &self.real(&path))
            }
            FX_RENAME => {
                let old = self.absolute(&path_of(reader.string()?));
                let new = self.absolute(&path_of(reader.string()?));
                // Version 3's RENAME refuses to replace; posix-rename replaces.
                if fs::symlink_metadata(&new).is_ok() {
                    status_reply(id, STATUS_FAILURE, "target exists")
                } else {
                    io_reply(id, fs::rename(old, new))
                }
            }
            FX_READLINK => {
                let path = self.absolute(&path_of(reader.string()?));
                match fs::read_link(path) {
                    Ok(target) => self.name_reply_for(id, &target),
                    Err(error) => status_reply(id, status_code(&error), &error.to_string()),
                }
            }
            FX_SYMLINK => {
                // OpenSSH's argument order, which reverses the draft's: target, link.
                let target = path_of(reader.string()?);
                let link = self.absolute(&path_of(reader.string()?));
                io_reply(id, symlink(target, link))
            }
            FX_EXTENDED => {
                let name = reader.str()?.to_string();
                self.extended(id, &name, reader)?
            }
            _ => status_reply(id, STATUS_OP_UNSUPPORTED, "unsupported request"),
        })
    }

    fn extended(
        &mut self,
        id: u32,
        name: &str,
        reader: &mut Reader<'_>,
    ) -> crate::error::Result<Vec<u8>> {
        Ok(match name {
            "posix-rename@openssh.com" => {
                let old = self.absolute(&path_of(reader.string()?));
                let new = self.absolute(&path_of(reader.string()?));
                io_reply(id, fs::rename(old, new))
            }
            "fsync@openssh.com" => {
                let handle = read_handle(reader)?;
                match self.handles.get(&handle) {
                    Some(Handle::File(file)) => io_reply(id, file.sync_all()),
                    _ => status_reply(id, STATUS_FAILURE, "no such file handle"),
                }
            }
            "limits@openssh.com" => {
                let mut reply = Writer::message(FX_EXTENDED_REPLY);
                reply.u32(id);
                reply.u64(MAX_REQUEST as u64);
                reply.u64(u64::from(MAX_IO));
                reply.u64(u64::from(MAX_IO));
                reply.u64(MAX_HANDLES as u64);
                reply.into_vec()
            }
            "expand-path@openssh.com" => {
                let raw = reader.string()?;
                let text = path_of(raw);
                let expanded = match raw {
                    b"~" => self.home.clone(),
                    _ => match text.strip_prefix("~/") {
                        Ok(rest) => self.home.join(rest),
                        Err(_) => text,
                    },
                };
                self.name_reply_for(id, &self.real(&expanded))
            }
            _ => status_reply(id, STATUS_OP_UNSUPPORTED, "unsupported extension"),
        })
    }

    fn name_reply_for(&self, id: u32, path: &Path) -> Vec<u8> {
        let bytes = path.as_os_str().as_bytes();
        let mut reply = Writer::message(FX_NAME);
        reply.u32(id).u32(1).string(bytes).string(bytes);
        reply.u32(0);
        reply.into_vec()
    }

    fn handle_reply(&mut self, id: u32, handle: Handle) -> Vec<u8> {
        match self.insert(handle) {
            Some(number) => {
                let mut reply = Writer::message(FX_HANDLE);
                reply.u32(id).string(&number.to_be_bytes());
                reply.into_vec()
            }
            None => status_reply(id, STATUS_FAILURE, "too many open handles"),
        }
    }

    fn open(&mut self, id: u32, path: &Path, flags: u32, attrs: &Attrs) -> Vec<u8> {
        let mut options = OpenOptions::new();
        options
            .read(flags & PF_READ != 0)
            .write(flags & PF_WRITE != 0)
            .append(flags & PF_APPEND != 0)
            .truncate(flags & PF_TRUNC != 0);
        if flags & PF_EXCL != 0 && flags & PF_CREAT != 0 {
            options.create_new(true);
        } else if flags & PF_CREAT != 0 {
            options.create(true);
        }
        options.mode(attrs.mode.map_or(0o666, |mode| mode & 0o7777));
        match options.open(path) {
            Ok(file) => self.handle_reply(id, Handle::File(file)),
            Err(error) => status_reply(id, status_code(&error), &error.to_string()),
        }
    }

    fn read(&mut self, id: u32, handle: u32, offset: u64, length: u32) -> Vec<u8> {
        let Some(Handle::File(file)) = self.handles.get_mut(&handle) else {
            return status_reply(id, STATUS_FAILURE, "no such file handle");
        };
        let mut buffer = vec![0u8; length.min(MAX_IO) as usize];
        let got = file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| file.read(&mut buffer));
        match got {
            Ok(0) => status_reply(id, STATUS_EOF, "End of file"),
            Ok(count) => {
                let mut reply = Writer::message(FX_DATA);
                reply
                    .u32(id)
                    .string(buffer.get(..count).unwrap_or_default());
                reply.into_vec()
            }
            Err(error) => status_reply(id, status_code(&error), &error.to_string()),
        }
    }

    fn write(&mut self, id: u32, handle: u32, offset: u64, data: &[u8]) -> Vec<u8> {
        let Some(Handle::File(file)) = self.handles.get_mut(&handle) else {
            return status_reply(id, STATUS_FAILURE, "no such file handle");
        };
        // What limits@openssh.com advertises. Status replies cannot say a write
        // was partial, so an oversized one is refused whole, not cut short.
        if data.len() > MAX_IO as usize {
            return status_reply(id, STATUS_FAILURE, "write is longer than the limit");
        }
        io_reply(
            id,
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.write_all(data)),
        )
    }

    fn read_directory(&mut self, id: u32, handle: u32) -> Vec<u8> {
        let Some(Handle::Directory {
            entries,
            path,
            sent_dots,
        }) = self.handles.get_mut(&handle)
        else {
            return status_reply(id, STATUS_FAILURE, "no such directory handle");
        };
        let mut names: Vec<(Vec<u8>, Metadata)> = Vec::new();
        if !*sent_dots {
            *sent_dots = true;
            if let Ok(metadata) = fs::symlink_metadata(&*path) {
                names.push((b".".to_vec(), metadata));
            }
            let parent = path.parent().unwrap_or(path);
            if let Ok(metadata) = fs::symlink_metadata(parent) {
                names.push((b"..".to_vec(), metadata));
            }
        }
        if let Some(reader) = entries {
            while names.len() < READDIR_BATCH {
                match reader.next() {
                    Some(Ok(entry)) => {
                        if let Ok(metadata) = fs::symlink_metadata(entry.path()) {
                            names.push((entry.file_name().into_vec(), metadata));
                        }
                    }
                    Some(Err(error)) => {
                        return status_reply(id, status_code(&error), &error.to_string())
                    }
                    None => {
                        *entries = None;
                        break;
                    }
                }
            }
        }
        if names.is_empty() {
            return status_reply(id, STATUS_EOF, "End of file");
        }
        let mut reply = Writer::message(FX_NAME);
        reply.u32(id).u32(u32::try_from(names.len()).unwrap_or(0));
        for (name, metadata) in &names {
            reply.string(name);
            reply.string(&long_name(name, metadata));
            write_attrs(&mut reply, metadata);
        }
        reply.into_vec()
    }
}

fn read_handle(reader: &mut Reader<'_>) -> crate::error::Result<u32> {
    let bytes: [u8; 4] = reader.array()?;
    Ok(u32::from_be_bytes(bytes))
}

fn attrs_reply(id: u32, metadata: io::Result<Metadata>) -> Vec<u8> {
    match metadata {
        Ok(metadata) => {
            let mut reply = Writer::message(FX_ATTRS);
            reply.u32(id);
            write_attrs(&mut reply, &metadata);
            reply.into_vec()
        }
        Err(error) => status_reply(id, status_code(&error), &error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    struct Harness {
        server: Server,
        next_id: u32,
    }

    impl Harness {
        fn new(home: &Path) -> Self {
            Self {
                server: Server {
                    home: home.to_path_buf(),
                    handles: HashMap::new(),
                    next_handle: 0,
                },
                next_id: 1,
            }
        }

        fn call(&mut self, kind: u8, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
            let id = self.next_id;
            self.next_id += 1;
            let mut request = Writer::message(kind);
            request.u32(id);
            body(&mut request);
            self.server.handle(request.as_slice()).unwrap()
        }

        fn status(&mut self, kind: u8, body: impl FnOnce(&mut Writer)) -> u32 {
            let reply = self.call(kind, body);
            assert_eq!(reply[0], FX_STATUS, "expected a status for {kind}");
            let mut reader = Reader::new(&reply[1..]);
            reader.u32().unwrap();
            reader.u32().unwrap()
        }

        fn handle(&mut self, kind: u8, body: impl FnOnce(&mut Writer)) -> [u8; 4] {
            let reply = self.call(kind, body);
            assert_eq!(reply[0], FX_HANDLE);
            let mut reader = Reader::new(&reply[1..]);
            reader.u32().unwrap();
            reader.array().unwrap()
        }
    }

    fn no_attrs(writer: &mut Writer) {
        writer.u32(0);
    }

    #[test]
    fn init_advertises_version_three_and_the_extensions() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        let mut harness = Harness::new(dir.path());
        let mut request = Writer::message(FX_INIT);
        request.u32(3);
        let reply = harness.server.handle(request.as_slice()).unwrap();
        let mut reader = Reader::new(&reply);
        assert_eq!(reader.u8().unwrap(), FX_VERSION);
        assert_eq!(reader.u32().unwrap(), 3);
        let mut names = Vec::new();
        while !reader.is_empty() {
            names.push(reader.str().unwrap().to_string());
            reader.str().unwrap();
        }
        assert!(names.contains(&"posix-rename@openssh.com".to_string()));
        assert!(names.contains(&"limits@openssh.com".to_string()));
    }

    #[test]
    fn a_file_is_written_read_stated_renamed_and_removed() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        let mut harness = Harness::new(dir.path());
        let handle = harness.handle(FX_OPEN, |w| {
            w.str("file");
            w.u32(PF_WRITE | PF_CREAT | PF_EXCL);
            no_attrs(w);
        });
        assert_eq!(
            harness.status(FX_WRITE, |w| {
                w.string(&handle).u64(3).string(b"xyz");
            }),
            STATUS_OK
        );
        assert_eq!(
            harness.status(FX_CLOSE, |w| {
                w.string(&handle);
            }),
            STATUS_OK
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"\0\0\0xyz");

        let read_handle = harness.handle(FX_OPEN, |w| {
            w.str("file").u32(PF_READ);
            no_attrs(w);
        });
        let reply = harness.call(FX_READ, |w| {
            w.string(&read_handle).u64(3).u32(100);
        });
        assert_eq!(reply[0], FX_DATA);
        let mut reader = Reader::new(&reply[1..]);
        reader.u32().unwrap();
        assert_eq!(reader.string().unwrap(), b"xyz");
        assert_eq!(
            harness.status(FX_READ, |w| {
                w.string(&read_handle).u64(100).u32(10);
            }),
            STATUS_EOF
        );

        let stat = harness.call(FX_STAT, |w| {
            w.str("file");
        });
        assert_eq!(stat[0], FX_ATTRS);

        assert_eq!(
            harness.status(FX_RENAME, |w| {
                w.str("file").str("renamed");
            }),
            STATUS_OK
        );
        assert_eq!(
            harness.status(FX_REMOVE, |w| {
                w.str("renamed");
            }),
            STATUS_OK
        );
        assert_eq!(
            harness.status(FX_REMOVE, |w| {
                w.str("renamed");
            }),
            STATUS_NO_SUCH_FILE
        );
    }

    #[test]
    fn a_write_past_the_advertised_limit_is_refused_whole() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        let mut harness = Harness::new(dir.path());
        let handle = harness.handle(FX_OPEN, |w| {
            w.str("file").u32(PF_WRITE | PF_CREAT);
            no_attrs(w);
        });
        let limit = MAX_IO as usize;
        assert_eq!(
            harness.status(FX_WRITE, |w| {
                w.string(&handle).u64(0).string(&vec![7u8; limit + 1]);
            }),
            STATUS_FAILURE
        );
        assert_eq!(fs::metadata(dir.path().join("file")).unwrap().len(), 0);
        assert_eq!(
            harness.status(FX_WRITE, |w| {
                w.string(&handle).u64(0).string(&vec![7u8; limit]);
            }),
            STATUS_OK
        );
        assert_eq!(
            fs::metadata(dir.path().join("file")).unwrap().len(),
            u64::from(MAX_IO)
        );
    }

    #[test]
    fn rename_refuses_to_replace_but_posix_rename_does() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        fs::write(dir.path().join("a"), b"a").unwrap();
        fs::write(dir.path().join("b"), b"b").unwrap();
        let mut harness = Harness::new(dir.path());
        assert_eq!(
            harness.status(FX_RENAME, |w| {
                w.str("a").str("b");
            }),
            STATUS_FAILURE
        );
        assert_eq!(
            harness.status(FX_EXTENDED, |w| {
                w.str("posix-rename@openssh.com").str("a").str("b");
            }),
            STATUS_OK
        );
        assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"a");
    }

    #[test]
    fn directories_list_with_dots_then_eof() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        fs::write(dir.path().join("f"), b"").unwrap();
        let mut harness = Harness::new(dir.path());
        assert_eq!(
            harness.status(FX_MKDIR, |w| {
                w.str("sub");
                w.u32(ATTR_PERMISSIONS).u32(0o700);
            }),
            STATUS_OK
        );
        let handle = harness.handle(FX_OPENDIR, |w| {
            w.str(".");
        });
        let reply = harness.call(FX_READDIR, |w| {
            w.string(&handle);
        });
        assert_eq!(reply[0], FX_NAME);
        let mut reader = Reader::new(&reply[1..]);
        reader.u32().unwrap();
        let count = reader.u32().unwrap();
        let mut names = Vec::new();
        for _ in 0..count {
            names.push(String::from_utf8(reader.string().unwrap().to_vec()).unwrap());
            reader.string().unwrap();
            read_attrs(&mut reader).unwrap();
        }
        for expected in [".", "..", "f", "sub"] {
            assert!(names.iter().any(|name| name == expected), "{expected}");
        }
        assert_eq!(
            harness.status(FX_READDIR, |w| {
                w.string(&handle);
            }),
            STATUS_EOF
        );
        assert_eq!(
            harness.status(FX_RMDIR, |w| {
                w.str("sub");
            }),
            STATUS_OK
        );
    }

    #[test]
    fn realpath_resolves_relative_empty_and_missing_paths() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        let mut harness = Harness::new(&home);
        let realpath = |harness: &mut Harness, path: &str| {
            let path = path.to_string();
            let reply = harness.call(FX_REALPATH, |w| {
                w.str(&path);
            });
            let mut reader = Reader::new(&reply[1..]);
            reader.u32().unwrap();
            reader.u32().unwrap();
            String::from_utf8(reader.string().unwrap().to_vec()).unwrap()
        };
        assert_eq!(realpath(&mut harness, ""), home.to_str().unwrap());
        assert_eq!(realpath(&mut harness, "."), home.to_str().unwrap());
        assert_eq!(
            realpath(&mut harness, "missing/../other"),
            home.join("other").to_str().unwrap()
        );
        // scp's home-relative form.
        let reply = harness.call(FX_EXTENDED, |w| {
            w.str("expand-path@openssh.com").str("~/x");
        });
        let mut reader = Reader::new(&reply[1..]);
        reader.u32().unwrap();
        reader.u32().unwrap();
        assert_eq!(
            reader.string().unwrap(),
            home.join("x").to_str().unwrap().as_bytes()
        );
    }

    #[test]
    fn handles_are_capped_and_stale_ones_refused() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        fs::write(dir.path().join("f"), b"").unwrap();
        let mut harness = Harness::new(dir.path());
        let mut last = [0u8; 4];
        for _ in 0..MAX_HANDLES {
            last = harness.handle(FX_OPEN, |w| {
                w.str("f").u32(PF_READ);
                no_attrs(w);
            });
        }
        assert_eq!(
            harness.status(FX_OPEN, |w| {
                w.str("f").u32(PF_READ);
                no_attrs(w);
            }),
            STATUS_FAILURE
        );
        assert_eq!(
            harness.status(FX_CLOSE, |w| {
                w.string(&last);
            }),
            STATUS_OK
        );
        assert_eq!(
            harness.status(FX_CLOSE, |w| {
                w.string(&last);
            }),
            STATUS_FAILURE
        );
    }

    #[test]
    fn malformed_requests_get_bad_message_and_nothing_panics() {
        let dir = td_fs::TempDir::new("td-ssh-sftp").unwrap();
        let mut harness = Harness::new(dir.path());
        for kind in 0..=255u8 {
            for tail in [&[][..], &[0xff; 3], &[0, 0, 0, 200, 1, 2]] {
                let mut request = Writer::message(kind);
                request.u32(1).raw(tail);
                let _ = harness.server.handle(request.as_slice());
            }
        }
        assert!(harness.server.handle(&[]).is_none());
        assert_eq!(harness.status(99, |_| {}), STATUS_OP_UNSUPPORTED);
    }

    #[test]
    fn serve_runs_a_framed_conversation_and_stops_at_oversize() {
        let mut init = Writer::message(FX_INIT);
        init.u32(3);
        let body = init.into_vec();
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&body);
        wire.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut output = Vec::new();
        serve(&wire[..], &mut output, "/");
        assert_eq!(output[4], FX_VERSION);
    }
}
