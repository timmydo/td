//! The scanned hierarchy as an arena of nodes with subtree totals. Pure:
//! the scanner builds it, the window reads, grafts and prunes it.
//!
//! A node id is never reused. A refresh or a deletion retires the nodes it
//! replaces, so an id a delete list or a selection still holds reads as
//! gone rather than naming some later file.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub type NodeId = u32;

/// The most nodes one tree holds, retired ones included. A scan that
/// reaches it stops adding entries and says the tree is partial.
pub const MAX_NODES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    /// A device, FIFO or socket.
    Other,
    /// A directory on another file system, shown but not entered.
    Mount,
}

/// Bytes two ways: the blocks the file system allocated (`du`'s figure)
/// and the length a reader sees.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Size {
    pub allocated: u64,
    pub apparent: u64,
}

impl Size {
    fn add(self, other: Size) -> Size {
        Size {
            allocated: self.allocated.saturating_add(other.allocated),
            apparent: self.apparent.saturating_add(other.apparent),
        }
    }
    fn sub(self, other: Size) -> Size {
        Size {
            allocated: self.allocated.saturating_sub(other.allocated),
            apparent: self.apparent.saturating_sub(other.apparent),
        }
    }
    pub fn get(self, measure: Measure) -> u64 {
        match measure {
            Measure::Allocated => self.allocated,
            Measure::Apparent => self.apparent,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Measure {
    #[default]
    Allocated,
    Apparent,
}

/// The file system's name for an inode: device and inode number.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug)]
pub struct Node {
    pub name: OsString,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub kind: Kind,
    /// The node's own blocks: a directory's own entry table, or a file's
    /// contents. A second hard link to a counted inode owns nothing.
    pub own: Size,
    /// `own` plus every live descendant's.
    pub total: Size,
    /// Entries that are not directories, this one included.
    pub files: u64,
    /// The node's own modification time, seconds since the epoch.
    pub mtime: i64,
    /// The newest `mtime` in the subtree.
    pub modified: i64,
    pub identity: Identity,
    /// The length a reader sees, even for a name that owns no bytes.
    pub length: u64,
    /// A file whose inode has other names, which may own its bytes.
    pub links: bool,
    /// A directory whose listing could not be read whole.
    pub unreadable: bool,
    alive: bool,
}

impl Node {
    pub fn new(name: OsString, kind: Kind, own: Size, modified: i64, identity: Identity) -> Self {
        Self {
            name,
            parent: None,
            children: Vec::new(),
            kind,
            own,
            total: own,
            files: u64::from(!matches!(kind, Kind::Dir | Kind::Mount)),
            mtime: modified,
            modified,
            identity,
            length: own.apparent,
            links: false,
            unreadable: false,
            alive: true,
        }
    }
    pub fn is_directory(&self) -> bool {
        self.kind == Kind::Dir
    }
}

#[derive(Debug)]
pub struct Tree {
    root: PathBuf,
    nodes: Vec<Node>,
    /// The scan stopped at `MAX_NODES` or was refused part of its walk.
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Past `MAX_NODES`.
    Full,
    /// No such live node, or the root where it cannot be.
    Gone,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "the tree holds as many entries as it may",
            Self::Gone => "that entry is no longer in the tree",
        })
    }
}

impl std::error::Error for Error {}

pub const ROOT: NodeId = 0;

impl Tree {
    /// A tree of one node, its root, at `root`, an absolute path the root
    /// node's name repeats.
    pub fn new(root: PathBuf, mut node: Node) -> Self {
        node.name = root.as_os_str().to_owned();
        node.parent = None;
        Self {
            root,
            nodes: vec![node],
            partial: false,
        }
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Every slot, retired ones included; ids are below this.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// A live node.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes
            .get(usize::try_from(id).ok()?)
            .filter(|node| node.alive)
    }

    fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes
            .get_mut(usize::try_from(id).ok()?)
            .filter(|node| node.alive)
    }

    /// The names from `ancestor` down to `id`, when `id` lies beneath it.
    pub fn relative(&self, id: NodeId, ancestor: NodeId) -> Option<Vec<OsString>> {
        let mut names = Vec::new();
        let mut at = id;
        while at != ancestor {
            let node = self.get(at)?;
            names.push(node.name.clone());
            at = node.parent?;
        }
        names.reverse();
        Some(names)
    }

    /// The live node reached from `ancestor` by `names`.
    pub fn resolve(&self, ancestor: NodeId, names: &[OsString]) -> Option<NodeId> {
        let mut at = ancestor;
        for name in names {
            at = *self.get(at)?.children.iter().find(|child| {
                self.get(**child)
                    .is_some_and(|node| node.name.as_os_str() == name.as_os_str())
            })?;
        }
        self.get(at).map(|_| at)
    }

    /// Every live node at `id` and beneath it.
    pub fn subtree(&self, id: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut stack = vec![id];
        while let Some(at) = stack.pop() {
            if let Some(node) = self.get(at) {
                found.push(at);
                stack.extend(node.children.iter().copied());
            }
        }
        found
    }

    /// Makes `size` a live node's own bytes, carrying the difference up.
    pub fn set_own(&mut self, id: NodeId, size: Size) -> Result<(), Error> {
        let node = self.get_mut(id).ok_or(Error::Gone)?;
        let old = node.own;
        node.own = size;
        node.total = node.total.sub(old).add(size);
        let parent = node.parent;
        self.carry(parent, (old, 0), (size, 0));
        Ok(())
    }

    /// Notes that a directory's listing could not be read whole.
    pub fn mark_unreadable(&mut self, id: NodeId) {
        if let Some(node) = self.get_mut(id) {
            node.unreadable = true;
        }
    }

    pub fn root(&self) -> Option<&Node> {
        self.get(ROOT)
    }

    /// Adds `node` under `parent` and returns its id. Totals are not
    /// carried up: a scanner calls `sum` once it is done, a caller adding
    /// to a finished tree calls `carry`.
    pub fn push(&mut self, parent: NodeId, mut node: Node) -> Result<NodeId, Error> {
        if self.nodes.len() >= MAX_NODES {
            return Err(Error::Full);
        }
        let id = NodeId::try_from(self.nodes.len()).map_err(|_| Error::Full)?;
        self.get_mut(parent).ok_or(Error::Gone)?.children.push(id);
        node.parent = Some(parent);
        self.nodes.push(node);
        Ok(id)
    }

    /// Recomputes every live node's totals from its own size up. Children
    /// always follow their parent in the arena, so one reverse pass sums.
    pub fn sum(&mut self) {
        for node in self.nodes.iter_mut().filter(|node| node.alive) {
            node.total = node.own;
            node.modified = node.mtime;
            node.files = u64::from(!matches!(node.kind, Kind::Dir | Kind::Mount));
        }
        for index in (1..self.nodes.len()).rev() {
            let Some(node) = self.nodes.get(index).filter(|node| node.alive) else {
                continue;
            };
            let (total, files, modified) = (node.total, node.files, node.modified);
            let Some(parent) = node
                .parent
                .and_then(|parent| self.nodes.get_mut(parent as usize))
            else {
                continue;
            };
            parent.total = parent.total.add(total);
            parent.files = parent.files.saturating_add(files);
            parent.modified = parent.modified.max(modified);
        }
        for node in &mut self.nodes {
            node.children.shrink_to_fit();
        }
    }

    /// The node's path: the root's, then each name down to it.
    pub fn path(&self, id: NodeId) -> Option<PathBuf> {
        let mut names: Vec<&OsStr> = Vec::new();
        let mut at = self.get(id)?;
        while let Some(parent) = at.parent {
            names.push(&at.name);
            at = self.get(parent)?;
        }
        let mut path = self.root.clone();
        for name in names.iter().rev() {
            path.push(name);
        }
        Some(path)
    }

    /// Whether `id` is `ancestor` or lies beneath it.
    pub fn within(&self, id: NodeId, ancestor: NodeId) -> bool {
        let mut at = Some(id);
        while let Some(current) = at {
            if current == ancestor {
                return true;
            }
            at = self.get(current).and_then(|node| node.parent);
        }
        false
    }

    /// The live ancestors of `id`, nearest first.
    pub fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut at = self.get(id).and_then(|node| node.parent);
        while let Some(current) = at {
            found.push(current);
            at = self.get(current).and_then(|node| node.parent);
        }
        found
    }

    /// Takes `total` and `files` off every ancestor from `from` up, adds
    /// `plus_total` and `plus_files`, and recomputes their newest time.
    fn carry(&mut self, from: Option<NodeId>, minus: (Size, u64), plus: (Size, u64)) {
        let mut at = from;
        while let Some(id) = at {
            let modified = self.newest(id);
            let Some(node) = self.get_mut(id) else {
                return;
            };
            node.total = node.total.sub(minus.0).add(plus.0);
            node.files = node.files.saturating_sub(minus.1).saturating_add(plus.1);
            node.modified = modified;
            at = node.parent;
        }
    }

    /// A node's newest time from its own and its live children's.
    fn newest(&self, id: NodeId) -> i64 {
        let Some(node) = self.get(id) else {
            return 0;
        };
        node.children
            .iter()
            .filter_map(|child| self.get(*child))
            .map(|child| child.modified)
            .fold(node.mtime, i64::max)
    }

    /// Retires `id` and everything beneath it, freeing their names and
    /// child lists; the parent still lists `id` until the caller unlinks.
    fn retire(&mut self, id: NodeId) {
        let mut stack = vec![id];
        while let Some(at) = stack.pop() {
            let Some(node) = self.nodes.get_mut(at as usize) else {
                continue;
            };
            node.alive = false;
            node.name = OsString::new();
            stack.append(&mut node.children);
            node.children = Vec::new();
        }
    }

    /// Removes a live node that is not the root, with its subtree, and
    /// takes its totals off its ancestors.
    pub fn remove(&mut self, id: NodeId) -> Result<(), Error> {
        let node = self.get(id).ok_or(Error::Gone)?;
        let parent = node.parent.ok_or(Error::Gone)?;
        let minus = (node.total, node.files);
        self.retire(id);
        if let Some(parent) = self.get_mut(parent) {
            parent.children.retain(|child| *child != id);
        }
        self.carry(Some(parent), minus, (Size::default(), 0));
        Ok(())
    }

    /// Replaces the live node `at` with the root of `sub`, a fresh scan of
    /// the same path: `at` keeps its id, name and parent, its old subtree
    /// is retired and `sub`'s descendants get new ids.
    pub fn graft(&mut self, at: NodeId, sub: Tree) -> Result<(), Error> {
        let old = self.get(at).ok_or(Error::Gone)?;
        let parent = old.parent;
        let minus = (old.total, old.files);
        let count = sub.nodes.len();
        if self.nodes.len().saturating_add(count) > MAX_NODES {
            return Err(Error::Full);
        }
        let children = self.get(at).map(|node| node.children.clone());
        for child in children.unwrap_or_default() {
            self.retire(child);
        }
        // Sub ids 1.. become base.. ; the sub root becomes `at`.
        let base = self.nodes.len() as u64;
        let remap = |id: NodeId| -> NodeId {
            if id == ROOT {
                at
            } else {
                (base + u64::from(id) - 1) as NodeId
            }
        };
        let mut nodes = sub.nodes.into_iter();
        let Some(mut top) = nodes.next() else {
            return Err(Error::Gone);
        };
        let plus = (top.total, top.files);
        top.children = top.children.iter().map(|id| remap(*id)).collect();
        top.parent = parent;
        if let Some(slot) = self.get_mut(at) {
            top.name = std::mem::take(&mut slot.name);
            *slot = top;
        }
        for mut node in nodes {
            node.parent = node.parent.map(remap);
            node.children = node.children.iter().map(|id| remap(*id)).collect();
            self.nodes.push(node);
        }
        self.partial |= sub.partial;
        self.carry(parent, minus, plus);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, bytes: u64) -> Node {
        Node::new(
            name.into(),
            Kind::File,
            Size {
                allocated: bytes,
                apparent: bytes,
            },
            bytes as i64,
            Identity::default(),
        )
    }

    fn dir(name: &str) -> Node {
        Node::new(
            name.into(),
            Kind::Dir,
            Size::default(),
            0,
            Identity::default(),
        )
    }

    fn sample() -> (Tree, NodeId, NodeId, NodeId) {
        let mut tree = Tree::new("/r".into(), dir("r"));
        let a = tree.push(ROOT, dir("a")).unwrap();
        let x = tree.push(a, file("x", 10)).unwrap();
        let y = tree.push(ROOT, file("y", 5)).unwrap();
        tree.sum();
        (tree, a, x, y)
    }

    #[test]
    fn sums_and_paths() {
        let (tree, a, x, y) = sample();
        let root = tree.root().unwrap();
        assert_eq!(root.total.allocated, 15);
        assert_eq!(root.files, 2);
        assert_eq!(root.modified, 10);
        assert_eq!(tree.get(a).unwrap().total.apparent, 10);
        assert_eq!(tree.path(x).unwrap(), PathBuf::from("/r/a/x"));
        assert_eq!(tree.path(ROOT).unwrap(), PathBuf::from("/r"));
        assert!(tree.within(x, a) && tree.within(x, ROOT) && !tree.within(y, a));
        assert_eq!(tree.ancestors(x), vec![a, ROOT]);
    }

    #[test]
    fn remove_carries_totals_and_retires_ids() {
        let (mut tree, a, x, _) = sample();
        tree.remove(a).unwrap();
        assert!(tree.get(a).is_none() && tree.get(x).is_none());
        let root = tree.root().unwrap();
        assert_eq!((root.total.allocated, root.files, root.modified), (5, 1, 5));
        assert_eq!(tree.remove(a), Err(Error::Gone));
        assert_eq!(tree.remove(ROOT), Err(Error::Gone));
    }

    #[test]
    fn graft_at_the_root_and_paths_back() {
        let (mut tree, a, x, _) = sample();
        assert_eq!(tree.relative(x, ROOT), Some(vec!["a".into(), "x".into()]));
        assert_eq!(tree.relative(x, a), Some(vec!["x".into()]));
        assert_eq!(tree.relative(a, x), None);
        let mut sub = Tree::new("/r".into(), dir("r"));
        sub.push(ROOT, file("only", 7)).unwrap();
        sub.sum();
        tree.graft(ROOT, sub).unwrap();
        let root = tree.root().unwrap();
        assert_eq!((root.total.allocated, root.files, root.modified), (7, 1, 7));
        assert_eq!(tree.path(ROOT).unwrap(), PathBuf::from("/r"));
        let only = tree.resolve(ROOT, &["only".into()]).unwrap();
        assert_eq!(tree.path(only).unwrap(), PathBuf::from("/r/only"));
        assert_eq!(tree.resolve(ROOT, &["a".into()]), None);
        tree.set_own(
            only,
            Size {
                allocated: 9,
                apparent: 9,
            },
        )
        .unwrap();
        assert_eq!(tree.root().unwrap().total.apparent, 9);
    }

    #[test]
    fn graft_replaces_a_subtree_under_its_old_id() {
        let (mut tree, a, x, _) = sample();
        let mut sub = Tree::new("/r/a".into(), dir("a"));
        let z = sub.push(ROOT, file("z", 100)).unwrap();
        sub.push(ROOT, file("w", 1)).unwrap();
        sub.sum();
        let before = tree.len();
        tree.graft(a, sub).unwrap();
        assert!(tree.get(x).is_none());
        let node = tree.get(a).unwrap();
        assert_eq!(node.name, "a");
        assert_eq!(node.total.allocated, 101);
        assert_eq!(node.children.len(), 2);
        let new_z = before as NodeId + z - 1;
        assert_eq!(tree.path(new_z).unwrap(), PathBuf::from("/r/a/z"));
        assert_eq!(tree.root().unwrap().total.allocated, 106);
        assert_eq!(tree.root().unwrap().files, 3);
    }
}
