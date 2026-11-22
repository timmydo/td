//! Private pathname Unix endpoints in a trusted, stable filesystem namespace.
//!
//! Root and same-UID namespace writers are trusted. Checks reject symlinks and
//! other-user replacement authority; they do not make pathname lookup atomic
//! against a process already holding the same filesystem authority.
//!
//! A server endpoint may instead be group-shared: a socket another non-root
//! UID (the server) owns, reachable only through one group the caller is in.
//! Root and the server UID are then the trusted writers.
use std::{
    fs::{self, File},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::Path,
};

/// A canonical Linux pathname socket address, owned without a heap allocation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SocketPath {
    bytes: [u8; 107],
    length: u8,
}
impl SocketPath {
    pub fn new(path: &Path) -> io::Result<Self> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() > 107
            || bytes.first() != Some(&b'/')
            || bytes.contains(&0)
            || bytes
                .split(|byte| *byte == b'/')
                .skip(1)
                .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Unix socket pathname",
            ));
        }
        let mut value = Self {
            bytes: [0; 107],
            length: u8::try_from(bytes.len()).map_err(|_| refused())?,
        };
        value
            .bytes
            .get_mut(..bytes.len())
            .ok_or_else(refused)?
            .copy_from_slice(bytes);
        Ok(value)
    }
    pub fn as_path(&self) -> &Path {
        Path::new(std::ffi::OsStr::from_bytes(
            self.bytes
                .get(..usize::from(self.length))
                .unwrap_or_default(),
        ))
    }
}
impl std::fmt::Debug for SocketPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_path().fmt(f)
    }
}

/// Validate a private endpoint before connecting. Call on every connection,
/// including reconnects; a previously checked path is not retained authority.
pub fn check_private_socket(path: &Path) -> io::Result<()> {
    check_private_socket_parent(path)?;
    let metadata = fs::symlink_metadata(path)?;
    let uid = fs::metadata("/proc/self")?.uid();
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(refused());
    }
    Ok(())
}

/// Validate the socket pathname and private parent before binding. The caller
/// owns listener exclusion, mode-0600 publication and inode-aware cleanup.
pub fn check_private_socket_parent(path: &Path) -> io::Result<()> {
    SocketPath::new(path)?;
    let uid = fs::metadata("/proc/self")?.uid();
    let parent = path.parent().ok_or_else(refused)?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(refused());
    }
    for ancestor in parent.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err(refused());
        }
    }
    Ok(())
}

/// Validate a server's endpoint before connecting, on every connection. It
/// is either private (`check_private_socket`) or group-shared: socket mode
/// 0660 and direct parent mode 0750, both owned by one non-root server UID
/// and one group, every further ancestor owned by root or that server UID
/// and writable by neither group nor others, sticky or not: a shared
/// directory under a sticky one could be anyone's. A shared socket admits
/// root, its server UID and the members of its group.
pub fn check_server_socket(path: &Path) -> io::Result<()> {
    SocketPath::new(path)?;
    let caller = fs::metadata("/proc/self")?.uid();
    let socket = fs::symlink_metadata(path)?;
    if socket.uid() == caller && check_private_socket(path).is_ok() {
        return Ok(());
    }
    let (parent, server) = shared_endpoint(path, caller, &socket)?;
    for ancestor in parent.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !trusted_ancestor(&Node::of(&metadata), server) {
            return Err(not_shared());
        }
    }
    Ok(())
}

/// The shared form's socket and direct parent; returns the parent and the
/// server UID its ancestors are checked against.
fn shared_endpoint<'a>(
    path: &'a Path,
    caller: u32,
    socket: &fs::Metadata,
) -> io::Result<(&'a Path, u32)> {
    let parent = path.parent().ok_or_else(not_shared)?;
    let directory = fs::symlink_metadata(parent)?;
    let mut groups = [0u32; MAX_GROUPS];
    // Root and the server need no membership, so read none for them.
    let count = if caller == 0 || caller == socket.uid() {
        0
    } else {
        caller_groups(&mut groups)?
    };
    let shared = Shared {
        caller,
        member: groups.get(..count).unwrap_or_default(),
        socket: Node::of(socket),
        parent: Node::of(&directory),
    };
    if !shared.admits() {
        return Err(not_shared());
    }
    Ok((parent, directory.uid()))
}

/// The filesystem GID and at most 1024 supplementary groups; more refuse.
const MAX_GROUPS: usize = 1 + 1024;

#[derive(Clone, Copy)]
struct Node {
    kind: Kind,
    uid: u32,
    gid: u32,
    mode: u32,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Socket,
    Directory,
    Other,
}
impl Node {
    fn of(metadata: &fs::Metadata) -> Self {
        let kind = if metadata.file_type().is_socket() {
            Kind::Socket
        } else if metadata.is_dir() {
            Kind::Directory
        } else {
            Kind::Other
        };
        Self {
            kind,
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode() & 0o7777,
        }
    }
}

struct Shared<'a> {
    caller: u32,
    member: &'a [u32],
    socket: Node,
    parent: Node,
}
impl Shared<'_> {
    fn admits(&self) -> bool {
        let (socket, parent) = (self.socket, self.parent);
        socket.kind == Kind::Socket
            && parent.kind == Kind::Directory
            && socket.uid != 0
            && parent.uid == socket.uid
            && parent.gid == socket.gid
            && socket.mode == 0o660
            && parent.mode == 0o750
            && (self.caller == 0 || socket.uid == self.caller || self.member.contains(&socket.gid))
    }
}

fn trusted_ancestor(node: &Node, server: u32) -> bool {
    node.kind == Kind::Directory && (node.uid == 0 || node.uid == server) && node.mode & 0o022 == 0
}

/// The caller's filesystem and supplementary groups, from procfs, into `out`.
fn caller_groups(out: &mut [u32; MAX_GROUPS]) -> io::Result<usize> {
    // Groups: holds at most MAX_GROUPS ten-digit IDs; the rest is small.
    let mut text = [0u8; 16 * 1024];
    let mut file = File::open("/proc/self/status")?;
    let mut used = 0;
    loop {
        let rest = text.get_mut(used..).ok_or_else(not_shared)?;
        if rest.is_empty() {
            return Err(not_shared());
        }
        match file.read(rest) {
            Ok(0) => break,
            Ok(n) => used += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    parse_groups(text.get(..used).unwrap_or_default(), out)
}

fn parse_groups(status: &[u8], out: &mut [u32; MAX_GROUPS]) -> io::Result<usize> {
    let mut count = 0;
    let mut push = |field: &[u8]| -> io::Result<()> {
        let value = std::str::from_utf8(field)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(not_shared)?;
        *out.get_mut(count).ok_or_else(not_shared)? = value;
        count += 1;
        Ok(())
    };
    let (mut gid, mut groups) = (false, false);
    for line in status.split(|b| *b == b'\n') {
        let mut fields = line
            .split(|b| *b == b' ' || *b == b'\t')
            .filter(|f| !f.is_empty());
        match fields.next() {
            // Real, effective, saved and filesystem; access uses the last.
            Some(b"Gid:") if !gid => {
                gid = true;
                push(fields.nth(3).ok_or_else(not_shared)?)?;
            }
            Some(b"Groups:") if !groups => {
                groups = true;
                for field in fields {
                    push(field)?;
                }
            }
            _ => {}
        }
    }
    if gid && groups {
        Ok(count)
    } else {
        Err(not_shared())
    }
}

fn not_shared() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Unix socket is neither private nor a trusted group-shared endpoint",
    )
}
fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Unix socket requires a private owned directory and mode-0600 owned socket",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::TempDir;
    use std::os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    };
    fn mode(path: &Path, value: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(value)).unwrap();
    }

    #[test]
    fn only_private_owned_socket_paths_are_admitted() {
        let root = TempDir::new("private-socket").unwrap();
        let path = root.join("socket");
        check_private_socket_parent(&path).unwrap();
        assert!(check_private_socket(&path).is_err());
        let _listener = UnixListener::bind(&path).unwrap();
        mode(&path, 0o600);
        check_private_socket(&path).unwrap();
        mode(&path, 0o660);
        assert!(check_private_socket(&path).is_err());
        mode(&path, 0o600);
        mode(&root, 0o750);
        assert!(check_private_socket(&path).is_err());
        mode(&root, 0o700);
        check_private_socket(&path).unwrap();
    }
    #[test]
    fn a_caller_owned_server_socket_is_private_or_shared() {
        let root = TempDir::new("server-socket").unwrap();
        let path = root.join("socket");
        assert!(check_server_socket(&path).is_err());
        let _listener = UnixListener::bind(&path).unwrap();
        mode(&path, 0o600);
        check_server_socket(&path).unwrap();
        // Neither form takes the other's modes.
        mode(&path, 0o660);
        assert!(check_server_socket(&path).is_err());
        mode(&path, 0o600);
        mode(&root, 0o750);
        assert!(check_server_socket(&path).is_err());
        // Its own group-shared socket: the server's maintenance account.
        // The fixture's root is shared-writable, so ancestors refuse here
        // and the endpoint itself is checked below them.
        mode(&path, 0o660);
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let socket = || fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            shared_endpoint(&path, uid, &socket()).unwrap(),
            (root.path(), uid)
        );
        mode(&root, 0o755);
        assert!(shared_endpoint(&path, uid, &socket()).is_err());
        assert!(check_server_socket(&path).is_err());
        mode(&root, 0o750);
        // Links to an admitted socket or directory are refused.
        let linked = root.join("linked");
        symlink(&path, &linked).unwrap();
        let meta = |p: &Path| fs::symlink_metadata(p).unwrap();
        assert!(shared_endpoint(&linked, uid, &meta(&linked)).is_err());
        let outer = TempDir::new("server-socket-link").unwrap();
        let directory = outer.join("directory");
        symlink(root.path(), &directory).unwrap();
        let through = directory.join("socket");
        assert!(shared_endpoint(&through, uid, &meta(&through)).is_err());
        assert!(shared_endpoint(&path, uid, &meta(&path)).is_ok());
        mode(&root, 0o700);
    }

    fn node(kind: Kind, uid: u32, gid: u32, mode: u32) -> Node {
        Node {
            kind,
            uid,
            gid,
            mode,
        }
    }

    #[test]
    fn the_shared_form_admits_only_one_server_and_a_member_group() {
        let good = || Shared {
            caller: 2000,
            member: &[2000, 70],
            socket: node(Kind::Socket, 1000, 70, 0o660),
            parent: node(Kind::Directory, 1000, 70, 0o750),
        };
        assert!(good().admits());
        let refusals: &[fn(&mut Shared<'_>)] = &[
            |s| s.member = &[2000],
            |s| s.socket.mode = 0o666,
            |s| s.socket.mode = 0o600,
            |s| s.socket.mode = 0o670,
            |s| s.parent.mode = 0o755,
            |s| s.parent.mode = 0o770,
            |s| s.parent.mode = 0o751,
            |s| s.socket.uid = 1001,
            |s| s.parent.uid = 1001,
            |s| s.socket.gid = 71,
            |s| s.parent.gid = 71,
            |s| {
                s.socket.uid = 0;
                s.parent.uid = 0
            },
            |s| s.socket.kind = Kind::Other,
            |s| s.parent.kind = Kind::Other,
        ];
        // The server account and root reach it without the group.
        for caller in [1000, 0] {
            let mut own = good();
            own.member = &[];
            own.caller = caller;
            assert!(own.admits());
        }
        for change in refusals {
            let mut shared = good();
            change(&mut shared);
            assert!(!shared.admits());
        }
    }

    #[test]
    fn shared_ancestors_belong_to_root_or_the_server() {
        assert!(trusted_ancestor(&node(Kind::Directory, 0, 0, 0o755), 1000));
        assert!(trusted_ancestor(
            &node(Kind::Directory, 1000, 70, 0o750),
            1000
        ));
        // A sticky shared directory protects names, not who made the child.
        assert!(!trusted_ancestor(
            &node(Kind::Directory, 0, 0, 0o1777),
            1000
        ));
        assert!(!trusted_ancestor(
            &node(Kind::Directory, 0, 0, 0o1770),
            1000
        ));
        assert!(!trusted_ancestor(
            &node(Kind::Directory, 2000, 0, 0o755),
            1000
        ));
        assert!(!trusted_ancestor(
            &node(Kind::Directory, 0, 70, 0o775),
            1000
        ));
        assert!(!trusted_ancestor(&node(Kind::Directory, 0, 0, 0o757), 1000));
        assert!(!trusted_ancestor(&node(Kind::Other, 0, 0, 0o755), 1000));
    }

    #[test]
    fn groups_come_from_the_filesystem_gid_and_the_supplementary_list() {
        let mut out = [0u32; MAX_GROUPS];
        let status = b"Name:\tx\nGid:\t1\t2\t3\t4\nGroups:\t70 71 \nUid:\t0\n";
        let n = parse_groups(status, &mut out).unwrap();
        assert_eq!(out.get(..n).unwrap(), &[4, 70, 71]);
        let empty = b"Gid:\t5\t5\t5\t5\nGroups:\n";
        assert_eq!(parse_groups(empty, &mut out).unwrap(), 1);
        for bad in [
            &b"Gid:\t1\t2\t3\t4\n"[..],
            b"Groups:\t70\n",
            b"Gid:\t1\t2\nGroups:\n",
            b"Gid:\t1\t2\t3\tx\nGroups:\n",
            b"Gid:\t1\t2\t3\t4\nGroups:\t-1\n",
        ] {
            assert!(parse_groups(bad, &mut out).is_err());
        }
        let mut many = Vec::from(&b"Gid:\t1\t1\t1\t1\nGroups:"[..]);
        for _ in 0..1024 {
            many.extend_from_slice(b" 9");
        }
        assert_eq!(parse_groups(&many, &mut out).unwrap(), MAX_GROUPS);
        many.extend_from_slice(b" 9");
        assert!(parse_groups(&many, &mut out).is_err());
        let mut live = [0u32; MAX_GROUPS];
        let n = caller_groups(&mut live).unwrap();
        let gid = fs::metadata("/proc/self").unwrap().gid();
        assert!(live.get(..n).unwrap().contains(&gid));
    }

    #[test]
    fn links_regular_files_and_noncanonical_names_are_refused() {
        let root = TempDir::new("socket-path-refusal").unwrap();
        let path = root.join("socket");
        fs::write(&path, b"").unwrap();
        mode(&path, 0o600);
        assert!(check_private_socket(&path).is_err());
        let linked = root.join("link");
        symlink(&path, &linked).unwrap();
        assert!(check_private_socket(&linked).is_err());
        let directory = root.join("directory");
        symlink(root.path(), &directory).unwrap();
        assert!(check_private_socket_parent(&directory.join("socket")).is_err());
        for name in [
            "relative",
            "@abstract",
            "/",
            "/tmp/../socket",
            "/tmp/./socket",
            "/tmp//socket",
            "/tmp/socket/",
        ] {
            assert_eq!(
                check_private_socket_parent(Path::new(name))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(check_private_socket_parent(Path::new(&format!("/{}", "x".repeat(107)))).is_err());
    }
}
