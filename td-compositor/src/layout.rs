use crate::scene::SurfaceKey;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;

const INITIAL_WORKSPACE: u8 = 1;
const FINAL_WORKSPACE: u8 = 9;
const VIRTUAL_EXTENT: usize = 65_536;

/// Which way the FIRST split on a workspace goes — the only insertion a tree
/// with one leaf cannot answer for itself, since a lone window is in no
/// container. Horizontal, so the second window opens as a second COLUMN:
/// columns are the arrangement this compositor is built around, and every
/// later window joins whatever container it opens in.
///
/// There is deliberately no command to change it. `Super+v`/`Super+h` used to
/// set a per-workspace split axis, and they now choose a column's
/// PRESENTATION — a thing the operator can see, rather than a mode that only
/// shows up when the next window opens. On a workspace holding ONE window
/// that contrast is at its weakest, since there is no column yet and the
/// choice does wait for the second window; what keeps it from being the old
/// mode is that the band's lit button says which one is waiting. Getting a
/// second window into one
/// column is `Super+Shift+Down` or a drop onto its band, both of which say so
/// at the moment they are used.
const FIRST_SPLIT: Axis = Axis::Horizontal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// How a container presents its children. `Split` tiles them along its axis;
/// the other two GROUP them, giving every leaf beneath a title band in a run
/// and one leaf the whole area below it.
///
/// The two grouped modes differ only in which way that run travels, and that
/// is why they are one enum rather than a flag beside a bool: every rule that
/// cares — the band geometry, which arrow keys walk the run, where a drop's
/// block goes — asks for the run's AXIS, and a mode that could not answer
/// would have to be handled at each of those sites separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Presentation {
    Split,
    /// Bands down the container's top, one per leaf.
    Stacked,
    /// Bands across the container's top, side by side.
    Tabbed,
}

impl Presentation {
    /// The direction this presentation's band run travels, or `None` when it
    /// draws no run at all.
    pub fn run(self) -> Option<Axis> {
        match self {
            Presentation::Split => None,
            Presentation::Stacked => Some(Axis::Vertical),
            Presentation::Tabbed => Some(Axis::Horizontal),
        }
    }

    /// The presentation whose run travels along `axis` — `run`'s inverse, so
    /// a placement can name the presentation it is in rather than leaving
    /// every reader to map the direction back.
    fn of_run(axis: Axis) -> Presentation {
        match axis {
            Axis::Vertical => Presentation::Stacked,
            Axis::Horizontal => Presentation::Tabbed,
        }
    }

    fn grouped(self) -> bool {
        self.run().is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Horizontal,
            Direction::Up | Direction::Down => Axis::Vertical,
        }
    }

    /// Whether this direction runs along its axis with the children's order
    /// or against it, which is what makes a move a step right or a step left
    /// once the axis has answered which container to step in.
    fn forward(self) -> bool {
        matches!(self, Direction::Right | Direction::Down)
    }
}

/// How far a subtree got with a move it was asked to make.
enum Moved {
    /// Handled here; the tree is already changed.
    Done,
    /// The leaf is in this subtree but it cannot move it — the container does
    /// not run along the asked-for axis, or the leaf is at its edge. The
    /// caller tries, which is what pulls the leaf OUT of this container.
    Escalate,
    /// The leaf is not in this subtree at all.
    Absent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Focus(Direction),
    Move(Direction),
    /// Group the container the focused leaf is DISPLAYED in, and say which way
    /// its bands run. `Presentation::Split` ungroups it.
    SetPresentation(Presentation),
    /// Group that container if it is not, and ungroup it if it is.
    ToggleGrouped,
    SwitchWorkspace(u8),
    MoveToWorkspace(u8),
    ToggleFullscreen,
}

/// What landing on a window MEANS, which the five zones of a tile answer.
/// A drop is not one gesture with a side to it any more: over the middle the
/// two windows trade places, and over an edge the dragged one lands on that
/// side — along an axis the drop NAMES rather than one read off the target's
/// container, since "above" over a window in a row has to make the column it
/// needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropKind {
    Swap,
    Beside {
        axis: Axis,
        before: bool,
    },
    /// A place in the target's own container, whatever that container is —
    /// what a drop onto a title BAND means. It names no axis deliberately:
    /// a run is a list, and a position in it is the only thing a drop onto
    /// one can mean, so resolving an axis here would be a second answer to a
    /// question `insert_beside` already has one for.
    InRun {
        before: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placement {
    pub key: SurfaceKey,
    /// The CLIENT's own area — what the blit covers, what the hit test asks
    /// about, and what the client is told it has.
    pub rect: Rect,
    /// The title band. Its own rectangle rather than "the top of `rect`"
    /// because the two need not touch: a stacked container puts every child's
    /// band in a run at its top and gives the content below all of them, so a
    /// derived band could not say where this one is. Zero height where the
    /// arrangement carries no decoration, which is fullscreen: a window with a
    /// band across the top of it is not fullscreen.
    pub band: Rect,
    pub focused: bool,
    /// The direction the band run travels for the container presenting this
    /// leaf, or `None` for an ordinary tile. Carried rather than derived from
    /// the tree because every reader wants the AXIS and not the fact: the
    /// renderer asks because a border wraps a window's band together with its
    /// client area and a grouped container is where it must not — a band
    /// abuts the content exactly as an ordinary band does, so adjacency cannot
    /// tell the two apart — and the drop asks because a block along the run
    /// goes on the edge the run runs to.
    pub run: Option<Axis>,
    /// Which of the three presentations the container showing this leaf is in.
    /// The same fact as `run` for every leaf in a container — and NOT the same
    /// for a lone leaf, which has no container and is presented by its
    /// WORKSPACE. That one is laid out as an ordinary tile whatever the
    /// presentation says, since a run of one leaf draws one band over one
    /// content rectangle either way, so it has no run and the border still
    /// wraps its band together with its client. The band's buttons ask this;
    /// the geometry asks `run`.
    pub presented: Presentation,
    /// Whether the CLIENT's pixels are shown. A leaf stacked away keeps the
    /// `rect` it WOULD have rather than an empty one, so this cannot be
    /// inferred from the rectangle: five sites ask instead — the border pass,
    /// the blit, the hit test, the grab, and `views`, which is what tells the
    /// client itself. The band pass is the one that deliberately does not.
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewLayout {
    pub key: SurfaceKey,
    pub rect: Rect,
    pub visible: bool,
    pub activated: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Node {
    Leaf(SurfaceKey),
    Split {
        axis: Axis,
        children: Vec<Node>,
        /// How this container shows its children. GROUPED — stacked or tabbed
        /// — gives every leaf beneath it a title band in a run at its top and
        /// the focused one all the space below that run. Per container rather
        /// than per workspace, so a nested column groups without the rest of
        /// the screen following it.
        ///
        /// Independent of `axis`, which stays the arrangement the container
        /// would tile as and is what it returns to when ungrouped.
        presentation: Presentation,
    },
}

impl Node {
    fn contains(&self, key: SurfaceKey) -> bool {
        match self {
            Node::Leaf(candidate) => *candidate == key,
            Node::Split { children, .. } => children.iter().any(|child| child.contains(key)),
        }
    }

    fn leaves(&self, keys: &mut Vec<SurfaceKey>) {
        match self {
            Node::Leaf(key) => keys.push(*key),
            Node::Split { children, .. } => {
                for child in children {
                    child.leaves(keys);
                }
            }
        }
    }

    /// Put a leaf immediately after the focused one, in the container that
    /// holds it DIRECTLY. There is no axis to choose: that container already
    /// has one, and the only case with no container — a workspace whose whole
    /// root is the focused leaf — is the `FIRST_SPLIT` a first window makes.
    ///
    /// The direct parent rather than the first ancestor running some asked-for
    /// way, because same-axis nesting is reachable — a drop leaves a container
    /// holding one child and the collapse lifts it into a parent that may run
    /// the same way — and an ancestor taking the insert puts the new window
    /// beside the container instead of in it. Where that container is GROUPED
    /// the difference is on screen: the window the operator just opened is not
    /// in the run they are looking at.
    fn insert_after(&mut self, focused: SurfaceKey, key: SurfaceKey) -> bool {
        match self {
            Node::Leaf(current) if *current == focused => {
                *self = Node::Split {
                    axis: FIRST_SPLIT,
                    children: vec![Node::Leaf(*current), Node::Leaf(key)],
                    presentation: Presentation::Split,
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { children, .. } => {
                let own = children.iter().position(
                    |child| matches!(child, Node::Leaf(candidate) if *candidate == focused),
                );
                if let Some(index) = own {
                    children.insert(index.saturating_add(1), Node::Leaf(key));
                    return true;
                }
                let Some(index) = children.iter().position(|child| child.contains(focused)) else {
                    return false;
                };
                children
                    .get_mut(index)
                    .is_some_and(|child| child.insert_after(focused, key))
            }
        }
    }

    /// Put a leaf immediately before or after a NAMED one. This is the drop
    /// half of a drag, where the destination is a window the pointer is over
    /// rather than a direction, so there is no walk.
    ///
    /// `axis` is the one the drop NAMED. Where the target's own container
    /// already runs that way this is an ordinary sibling insert, which is
    /// what keeps a drop into a column a reorder of that column. Where it
    /// does not, the target leaf is REPLACED by a split of the asked-for axis
    /// holding the two of them: that is what makes "above" and "below" mean
    /// something over a window in a row, where nothing in the tree runs
    /// vertically yet.
    ///
    /// A GROUPED container refuses that second case. `place_group` draws one
    /// band per LEAF beneath it, so a split among its children is a container
    /// nothing presents and whose leaves join the run anyway — an arrangement
    /// that says one thing and shows another, and that reappears the moment
    /// the stack is undone. A drop into a stack is therefore a place in its
    /// run whatever axis it named, which is what the stack already looks like.
    ///
    /// `in_stack` carries that down the walk rather than reading it here,
    /// because the refusal is about the outermost stacked ANCESTOR: a stack
    /// already holding a split presents the leaves under it flattened, so a
    /// second split built one level further down would be just as invisible.
    ///
    /// `None` asks for the target's own container whatever it is, which is
    /// what a drop onto a title BAND means — a place in that run.
    fn insert_beside(
        &mut self,
        target: SurfaceKey,
        key: SurfaceKey,
        axis: Option<Axis>,
        before: bool,
        in_stack: bool,
    ) -> bool {
        let Node::Split {
            axis: own,
            children,
            presentation,
        } = self
        else {
            return false;
        };
        let own = *own;
        let in_stack = in_stack || presentation.grouped();
        let beside = children
            .iter()
            .position(|child| matches!(child, Node::Leaf(candidate) if *candidate == target));
        if let Some(index) = beside {
            if let Some(axis) = axis.filter(|axis| *axis != own && !in_stack) {
                let Some(slot) = children.get_mut(index) else {
                    return false;
                };
                *slot = split_of(axis, key, target, before);
                return true;
            }
            let at = if before {
                index
            } else {
                index.saturating_add(1)
            };
            children.insert(at, Node::Leaf(key));
            return true;
        }
        let Some(index) = children.iter().position(|child| child.contains(target)) else {
            return false;
        };
        children
            .get_mut(index)
            .is_some_and(|child| child.insert_beside(target, key, axis, before, in_stack))
    }

    /// Exchange two leaves in place, leaving every container standing.
    /// Answers nothing: the caller has already established that both are in
    /// this tree, and a walk that found neither would be the same no-op.
    fn swap(&mut self, one: SurfaceKey, other: SurfaceKey) {
        match self {
            Node::Leaf(key) if *key == one => *key = other,
            Node::Leaf(key) if *key == other => *key = one,
            Node::Leaf(_) => {}
            Node::Split { children, .. } => {
                for child in children {
                    child.swap(one, other);
                }
            }
        }
    }

    /// Move a leaf one step along `axis`, i3's way: the nearest ancestor that
    /// RUNS along that axis is the one that acts, and what it does depends on
    /// what is beside the leaf there. A neighbouring window swaps places with
    /// it; a neighbouring CONTAINER is entered at its near edge; and a leaf
    /// with no neighbour that way leaves the container it is in and becomes a
    /// sibling of it.
    ///
    /// Nothing mutates until an arm commits, so an `Escalate` walking back up
    /// leaves the subtree exactly as it was — which is what lets the caller
    /// treat the tree it gets back as untouched.
    fn move_leaf(&mut self, key: SurfaceKey, axis: Axis, forward: bool) -> Moved {
        let Node::Split {
            axis: own,
            children,
            ..
        } = self
        else {
            return match self {
                Node::Leaf(candidate) if *candidate == key => Moved::Escalate,
                _ => Moved::Absent,
            };
        };
        let Some(index) = children.iter().position(|child| child.contains(key)) else {
            return Moved::Absent;
        };
        let direct =
            matches!(children.get(index), Some(Node::Leaf(candidate)) if *candidate == key);
        if !direct {
            match children
                .get_mut(index)
                .map(|child| child.move_leaf(key, axis, forward))
            {
                Some(Moved::Done) => return Moved::Done,
                Some(Moved::Escalate) => {}
                Some(Moved::Absent) | None => return Moved::Absent,
            }
        }
        if *own != axis {
            return Moved::Escalate;
        }
        if !direct {
            // The leaf came UP out of `children[index]`; it becomes a sibling
            // of the container it was in, on the side it was heading.
            let child = children.remove(index);
            let mut at = index;
            if let Some(remainder) = remove_node(child, key) {
                children.insert(index, remainder);
                if forward {
                    at = index.saturating_add(1);
                }
            }
            children.insert(at, Node::Leaf(key));
            return Moved::Done;
        }
        let target = if forward {
            index.saturating_add(1)
        } else {
            match index.checked_sub(1) {
                Some(target) => target,
                None => return Moved::Escalate,
            }
        };
        if target >= children.len() {
            return Moved::Escalate;
        }
        if let Some(Node::Split {
            children: inner, ..
        }) = children.get_mut(target)
        {
            // Entering from the left lands FIRST, entering from the right
            // lands last. Insert THEN remove, rather than the reverse: taking
            // the leaf out first leaves an arm where it is out of the tree
            // with nowhere to go, and a window that vanishes is worse than
            // one briefly in two places. It also means no index shifts.
            let position = if forward { 0 } else { inner.len() };
            inner.insert(position, Node::Leaf(key));
            children.remove(index);
        } else {
            children.swap(index, target);
        }
        Moved::Done
    }

    /// The presentation of the container a leaf is DISPLAYED in, to change.
    /// `None` for a lone window, which is its workspace's whole root and has
    /// no container to present it.
    ///
    /// An already-GROUPED ancestor wins over the leaf's own parent, and the
    /// OUTERMOST such, because a group runs every leaf BENEATH it and so hides
    /// whatever the containers under it are doing — that ancestor is what the
    /// leaf is displayed in, whatever they say. Descending past it would change
    /// a container nothing can see, and would leave a group undoable only from
    /// the leaves that happen to be its DIRECT children: in `H{grouped}[V[1,
    /// 3], 2]` the chord would work from 2 and not from 1 or 3, though the run
    /// shows all three and the operator cannot see the difference. With no such
    /// ancestor it is the leaf's own parent.
    ///
    /// Nothing READS this rule from the tree. What a band's buttons mark is
    /// derived from the placement instead, which already carries the run this
    /// same container produced — so there is no second walk to disagree with
    /// this one, and no per-band descent on the paint path.
    fn presented_mut(&mut self, key: SurfaceKey) -> Option<&mut Presentation> {
        let Node::Split {
            children,
            presentation,
            ..
        } = self
        else {
            return None;
        };
        // A key is in exactly one child, so the child that CONTAINS it is a
        // leaf only when it IS it — no second search for the leaf itself.
        let index = children.iter().position(|child| child.contains(key))?;
        let own = children
            .get(index)
            .is_some_and(|child| matches!(child, Node::Leaf(_)));
        if presentation.grouped() || own {
            return Some(presentation);
        }
        children.get_mut(index)?.presented_mut(key)
    }

    #[cfg(test)]
    fn validate(&self, seen: &mut BTreeSet<SurfaceKey>) -> Result<(), String> {
        match self {
            Node::Leaf(key) => {
                if !seen.insert(*key) {
                    return Err(format!(
                        "surface {}:{} appears more than once",
                        key.client, key.object
                    ));
                }
            }
            Node::Split { children, .. } => {
                if children.len() < 2 {
                    return Err("split container has fewer than two children".into());
                }
                for child in children {
                    child.validate(seen)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Workspace {
    root: Option<Node>,
    focused: Option<SurfaceKey>,
    fullscreen: Option<SurfaceKey>,
    /// Every mapped leaf, most-recently-focused first. A stack shows the one
    /// of its own leaves that comes first here, so it keeps showing what it
    /// showed when focus left it rather than snapping back to its first.
    /// Focus alone cannot answer that: it names one leaf per workspace, and a
    /// stack the operator is not in has none of it.
    recent: Vec<SurfaceKey>,
    /// How the workspace presents its root while that root is a LONE LEAF.
    ///
    /// A single window is in no container, so without this it has no
    /// presentation to change and its band offers nothing — the operator's
    /// first window is the one that says least about how the shell works. The
    /// workspace is that window's container of last resort, and the setting is
    /// carried onto the real root the moment a second window makes one, so
    /// choosing tabs while ONE window is open means the second opens into a
    /// tab rather than being an instruction that quietly expired. An EMPTY
    /// workspace is not that case and cannot be set up in advance: no leaf is
    /// focused, so the command returns before reaching this.
    presentation: Presentation,
}

impl Workspace {
    fn new() -> Workspace {
        Workspace {
            root: None,
            focused: None,
            fullscreen: None,
            recent: Vec::new(),
            presentation: Presentation::Split,
        }
    }

    /// The one way `focused` is assigned, so the MRU record cannot fall out of
    /// step with it.
    fn focus(&mut self, key: SurfaceKey) {
        self.focused = Some(key);
        self.recent.retain(|candidate| *candidate != key);
        self.recent.insert(0, key);
    }

    fn leaves(&self) -> Vec<SurfaceKey> {
        let mut keys = Vec::new();
        if let Some(root) = &self.root {
            root.leaves(&mut keys);
        }
        keys
    }

    fn map(&mut self, key: SurfaceKey) {
        self.fullscreen = None;
        let Some(mut root) = self.root.take() else {
            self.root = Some(Node::Leaf(key));
            self.focus(key);
            return;
        };
        // The workspace presents a LONE leaf, and this is where it stops being
        // one: whatever the operator chose while there was nothing to group
        // moves onto the container that now exists, so `Super+h` before a
        // second window opens is tabs when it does rather than a setting that
        // quietly expired. Handed OVER rather than copied — a workspace that
        // kept it would re-apply it to some later lone leaf that never asked.
        let carried = if matches!(root, Node::Leaf(_)) {
            std::mem::replace(&mut self.presentation, Presentation::Split)
        } else {
            Presentation::Split
        };
        let focused = self.focused.or_else(|| {
            let mut keys = Vec::new();
            root.leaves(&mut keys);
            keys.first().copied()
        });
        // A new window JOINS the container the focused one is in, so opening
        // one in a column extends that column and opening one in the root row
        // adds a column. No axis is chosen here at all: the container that
        // already holds the focused leaf has one, and `FIRST_SPLIT` is only
        // what a workspace of ONE window splits along, having no container yet.
        let inserted = focused.is_some_and(|current| root.insert_after(current, key));
        if !inserted {
            root = Node::Split {
                axis: FIRST_SPLIT,
                children: vec![root, Node::Leaf(key)],
                presentation: carried,
            };
        } else if let Node::Split { presentation, .. } = &mut root {
            // `insert_after` turned the lone leaf into the split itself.
            if carried.grouped() {
                *presentation = carried;
            }
        }
        self.root = Some(root);
        self.focus(key);
    }

    /// Put a mapped auxiliary window in its parent's run. Toplevel parents do
    /// not make a second kind of scene node: td remains a tiling compositor,
    /// so "above" is expressed by the child following the parent in the same
    /// run and taking focus when a grouped presentation makes the two overlap.
    fn place_after(&mut self, key: SurfaceKey, parent: SurfaceKey) -> bool {
        if key == parent {
            return false;
        }
        let Some(root) = self.root.take() else {
            return false;
        };
        if !root.contains(parent) {
            self.root = Some(root);
            return false;
        }
        let unchanged = root.clone();
        let mut next = if root.contains(key) {
            let Some(rest) = detach(root, key) else {
                self.root = Some(unchanged);
                return false;
            };
            rest
        } else {
            root
        };
        if !next.insert_beside(parent, key, None, false, false) {
            // A lone parent has no run yet. It becomes the first split in the
            // same way `map` introduces an ordinary second window.
            if next == Node::Leaf(parent) {
                next = Node::Split {
                    axis: FIRST_SPLIT,
                    children: vec![next, Node::Leaf(key)],
                    presentation: std::mem::replace(
                        &mut self.presentation,
                        Presentation::Split,
                    ),
                };
            } else {
                self.root = Some(unchanged);
                return false;
            }
        }
        let Some(next) = collapsed(next) else {
            self.root = Some(unchanged);
            return false;
        };
        let next = Some(next);
        // An auxiliary toplevel is still an ordinary tile. Revealing it exits
        // fullscreen just as `map` does; otherwise focus would move to a leaf
        // which the fullscreen projection does not paint.
        let left_fullscreen = self.fullscreen.take().is_some();
        let changed = next != Some(unchanged) || self.focused != Some(key) || left_fullscreen;
        self.root = next;
        self.focus(key);
        changed
    }

    fn unmap(&mut self, key: SurfaceKey) {
        let before = self.leaves();
        let removed = before.iter().position(|candidate| *candidate == key);
        let Some(root) = self.root.take() else {
            return;
        };
        self.root = remove_node(root, key);
        if self.root.is_none() {
            // A container is destroyed with its last child and takes its
            // presentation with it. The workspace's own is that setting one
            // level out, so it goes the same way: keeping it would group a
            // later window that never asked — the copy `map` refuses, one
            // step further on.
            self.presentation = Presentation::Split;
        }
        self.recent.retain(|candidate| *candidate != key);
        let after = self.leaves();
        if self.fullscreen == Some(key) {
            self.fullscreen = None;
        }
        if self.focused != Some(key) && self.focused.is_some_and(|focused| after.contains(&focused))
        {
            return;
        }
        self.focused = None;
        if let Some(next) = removed.and_then(|index| {
            let candidate = index.min(after.len().saturating_sub(1));
            after.get(candidate).copied()
        }) {
            self.focus(next);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    workspaces: BTreeMap<u8, Workspace>,
    homes: BTreeMap<SurfaceKey, u8>,
    active: u8,
}

impl Layout {
    pub fn new() -> Layout {
        let mut workspaces = BTreeMap::new();
        workspaces.insert(INITIAL_WORKSPACE, Workspace::new());
        Layout {
            workspaces,
            homes: BTreeMap::new(),
            active: INITIAL_WORKSPACE,
        }
    }

    pub fn active_workspace(&self) -> u8 {
        self.active
    }

    /// The workspaces holding a window, in number order. A workspace exists
    /// in the map as soon as anything ASKS for it — switching to an empty one
    /// creates the entry — so holding a root is what separates a workspace
    /// somebody is using from a number nobody has been to.
    pub fn occupied_workspaces(&self) -> Vec<u8> {
        self.workspaces
            .iter()
            .filter(|(_, workspace)| workspace.root.is_some())
            .map(|(number, _)| *number)
            .collect()
    }

    /// An empty workspace to send a window TO, and the reason the strip can be
    /// dragged onto at all: without it the bar names only workspaces that
    /// already exist, so on a machine using one there is nowhere to drop a
    /// window that is not where it already is, and a second desktop can be
    /// reached only by the keyboard.
    ///
    /// The LOWEST free number rather than one past the last, so a gap left by
    /// emptying a workspace is offered again before the range is walked
    /// further along, and the answer stays inside `INITIAL..=FINAL` instead of
    /// running off the end of it.
    ///
    /// Never the ACTIVE workspace, even when that one is empty: a window can
    /// only be dragged from the workspace being looked at, so a cell naming it
    /// is a drop that moves nothing, and offering it as the spare would leave
    /// an operator on an empty desktop with no way to reach a new one.
    ///
    /// `None` only when every workspace in the range holds something except
    /// the active one, which is nine windows deep and the one case where the
    /// strip cannot grow.
    pub fn spare_workspace(&self) -> Option<u8> {
        for number in INITIAL_WORKSPACE..=FINAL_WORKSPACE {
            let empty = self
                .workspaces
                .get(&number)
                .is_none_or(|workspace| workspace.root.is_none());
            if number != self.active && empty {
                return Some(number);
            }
        }
        None
    }

    /// The workspace a window lives on, which is not always the one in view: a
    /// drop asks so it can decline to promise a move to where the window
    /// already is.
    pub fn workspace_of(&self, key: SurfaceKey) -> Option<u8> {
        self.homes.get(&key).copied()
    }

    /// Send ONE window to a workspace, which is what a drop on the strip does.
    ///
    /// `move_to_workspace` moves whatever is focused, because a keyboard
    /// command has no other way to say which window it means. A drag names its
    /// own, and the two are not always the same: a press on a band focuses it
    /// today, so this is the same window in practice, but a drop that moved
    /// the FOCUSED one would be a different gesture the moment that stops
    /// being true.
    ///
    /// The source is the window's own home rather than the active workspace,
    /// so nothing here depends on the dragged window being on the workspace in
    /// view. Answers whether anything moved.
    pub fn move_key_to_workspace(&mut self, key: SurfaceKey, number: u8) -> bool {
        if !valid_workspace(number) {
            return false;
        }
        let Some(source) = self.homes.get(&key).copied() else {
            return false;
        };
        if source == number {
            return false;
        }
        // `homes` deliberately REMEMBERS an unmapped window, so that a client
        // which maps again lands where it was. That makes it the wrong
        // question to ask alone: moving one would put a window with no surface
        // into a tree, and `check_invariants` does not catch it. `drop_onto`
        // asks the tree it is about to change, so this asks the same.
        if !self
            .workspaces
            .get(&source)
            .is_some_and(|workspace| workspace.leaves().contains(&key))
        {
            return false;
        }
        self.workspace_mut(source).unmap(key);
        self.workspace_mut(number).map(key);
        self.homes.insert(key, number);
        true
    }

    pub fn focused(&self) -> Option<SurfaceKey> {
        self.workspaces
            .get(&self.active)
            .and_then(|workspace| workspace.focused)
    }

    /// The axis of the container a leaf sits DIRECTLY in. A lone window is in
    /// no container and answers None.
    ///
    /// Test-only: the drop reads `run_direction` instead, since a band's run
    /// and its container's axis are the same thing only outside a stack, and
    /// `drop_onto` resolves its own from the tree it is about to change. What
    /// this remains is the assertion the tree-shape tests are written on —
    /// which container each leaf ended up in is otherwise only observable as
    /// geometry.
    #[cfg(test)]
    pub fn parent_axis(&self, key: SurfaceKey) -> Option<Axis> {
        parent_axis(self.workspaces.get(&self.active)?.root.as_ref()?, key)
    }

    /// The direction this leaf's title BANDS run, which is what a drop onto
    /// one measures its half along. A GROUPED container's bands run the way
    /// its presentation says whatever its container's axis is — down for a
    /// stack, across for tabs — and otherwise the container's own axis, since
    /// each leaf then carries its own band at the top of its own tile. A lone
    /// window has neither.
    pub fn run_direction(&self, key: SurfaceKey) -> Option<Axis> {
        let root = self.workspaces.get(&self.active)?.root.as_ref()?;
        if let Some((_, run)) = stack_run(root, key) {
            return Some(run);
        }
        parent_axis(root, key)
    }

    /// Whether two leaves are presented by the same grouped container and
    /// therefore compete for one client rectangle.
    pub fn grouped_together(&self, one: SurfaceKey, other: SurfaceKey) -> bool {
        let Some(workspace) = self
            .homes
            .get(&one)
            .and_then(|number| self.workspaces.get(number))
        else {
            return false;
        };
        let Some(root) = workspace.root.as_ref() else {
            return false;
        };
        stack_run(root, one).is_some_and(|(run, _)| run.contains(&other))
    }

    /// Whether dragging this window could reach anywhere at all. Asked BEFORE
    /// a gesture takes a button, since one that cannot move anything would
    /// swallow the click with nothing to show for it.
    ///
    /// A second window to land beside, OR a DESKTOP to send it to. The second
    /// arm is what the workspace strip added: a lone window had nowhere to go
    /// and so could not be picked up, and now the bar always names an empty
    /// workspace that is not this one. Without this the two ways of picking a
    /// window up disagree — its title band would drag it to the bar while the
    /// same drag held by Alt never started.
    ///
    /// Fullscreen still refuses, and for its own reason rather than for want
    /// of a destination: the one placement covers the output, so claiming the
    /// modifier would take every Alt click in that client. A fullscreen window
    /// keeps `Super+Shift+N`.
    pub fn can_drag(&self, key: SurfaceKey) -> bool {
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return false;
        };
        if workspace.fullscreen.is_some() {
            return false;
        }
        let leaves = workspace.leaves();
        if !leaves.contains(&key) {
            return false;
        }
        leaves.len() > 1 || self.workspace_elsewhere()
    }

    /// Whether the strip names a workspace that is not the active one — which
    /// is a destination for a drag, occupied or not. Stated as a search rather
    /// than as "there is always one": it happens to be true while the range
    /// holds more than one number, and that is an arithmetic coincidence for a
    /// gesture to rest on.
    fn workspace_elsewhere(&self) -> bool {
        self.spare_workspace().is_some()
            || self
                .workspaces
                .iter()
                .any(|(number, workspace)| *number != self.active && workspace.root.is_some())
    }

    /// Drop a dragged window onto a target one — the pointer half of a move,
    /// where the destination is a window rather than a direction, and the
    /// KIND says what landing on it means.
    ///
    /// Answers whether the tree CHANGED — exactly that, and not whether the
    /// call was made. A drop that lands the window where it already was is the
    /// commonest gesture there is, and reporting it as a move would cost a
    /// round of configures for an identical arrangement.
    pub fn drop_onto(&mut self, dragged: SurfaceKey, target: SurfaceKey, drop: DropKind) -> bool {
        if dragged == target {
            return false;
        }
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return false;
        };
        if workspace.fullscreen.is_some() {
            return false;
        }
        let Some(root) = workspace.root.as_ref() else {
            return false;
        };
        if !root.contains(dragged) || !root.contains(target) {
            return false;
        }
        let (axis, before) = match drop {
            DropKind::Swap => return self.swap_leaves(dragged, target),
            DropKind::Beside { axis, before } => (Some(axis), before),
            // No axis, which is what `InRun` means all the way down:
            // `insert_beside` walks to the target's own container and puts
            // the leaf in it. Resolving one HERE would be a second answer to
            // a question that already has one, read off a tree the insert
            // does not walk.
            DropKind::InRun { before } => (None, before),
        };
        let Some(root) = self.workspace_mut(self.active).root.take() else {
            return false;
        };
        // Kept so the answer can be about what CHANGED rather than about the
        // call having been made: dropping a window back exactly where it came
        // from is the commonest gesture there is, and reporting it as a move
        // costs a repaint and a round of configures for an identical screen.
        // A layout tree is a handful of nodes, and this runs once per release.
        let unchanged = root.clone();
        // DETACHED rather than removed, which is the whole of this operation's
        // correctness. A removal collapses the container the leaf came out of,
        // and for a two-window column that container is the one the TARGET
        // sits in — so `H[1, V[2, 3]]` dropping 2 beside 3 would collapse the
        // column, land 3's neighbour in the row instead, and flatten it to
        // `H[1, 2, 3]`. It also takes the container's presentation with it, so
        // reordering a two-window stack would silently unstack it. Collapsing
        // once at the END leaves the target's container standing.
        let Some(mut rest) = detach(root, dragged) else {
            // Unreachable — both keys were in the tree and they differ — but
            // the root is already TAKEN here, so returning without putting one
            // back would leave the workspace with no windows at all.
            self.workspace_mut(self.active).root = Some(unchanged);
            return false;
        };
        if !rest.insert_beside(target, dragged, axis, before, false) {
            // Unreachable: the target was in the tree above and detaching a
            // DIFFERENT leaf cannot remove it. Kept so that no path can end
            // with the dragged window in neither the tree nor anywhere else.
            rest = Node::Split {
                axis: axis.unwrap_or(Axis::Horizontal),
                children: vec![rest, Node::Leaf(dragged)],
                presentation: Presentation::Split,
            };
        }
        let rebuilt = collapsed(rest);
        if rebuilt.as_ref() == Some(&unchanged) {
            self.workspace_mut(self.active).root = rebuilt;
            return false;
        }
        self.workspace_mut(self.active).root = rebuilt;
        self.workspace_mut(self.active).focus(dragged);
        true
    }

    /// Exchange two leaves where they stand. Deliberately NOT detach and
    /// reinsert: both windows keep their neighbours, their containers and
    /// those containers' presentation, which is the whole of what a swap
    /// means and what a detach would destroy — dropping into the middle of a
    /// stacked column would silently unstack it.
    fn swap_leaves(&mut self, dragged: SurfaceKey, target: SurfaceKey) -> bool {
        let Some(root) = self.workspace_mut(self.active).root.as_mut() else {
            return false;
        };
        root.swap(dragged, target);
        self.workspace_mut(self.active).focus(dragged);
        true
    }

    /// Point focus at a surface by IDENTITY rather than by direction, which
    /// is what a click means. Answers whether focus moved, so a caller can
    /// skip the repaint a click on the already-focused tile does not owe.
    pub fn focus_key(&mut self, key: SurfaceKey) -> bool {
        let Some(workspace) = self.workspaces.get_mut(&self.active) else {
            return false;
        };
        if workspace.focused == Some(key) {
            return false;
        }
        // Only a leaf of the ACTIVE workspace, and under fullscreen only the
        // fullscreen leaf: focusing anything else would leave the pointer on
        // one surface and the keyboard on another the operator cannot see.
        if !workspace
            .root
            .as_ref()
            .is_some_and(|root| root.contains(key))
        {
            return false;
        }
        if workspace.fullscreen.is_some_and(|full| full != key) {
            return false;
        }
        workspace.focus(key);
        true
    }

    /// Bring a mapped leaf's workspace into view and focus that exact leaf.
    /// A launcher activation is an explicit request to reveal the application,
    /// so it also leaves an unrelated fullscreen leaf that would hide it.
    pub fn activate_key(&mut self, key: SurfaceKey) -> Option<bool> {
        let number = self.homes.get(&key).copied()?;
        if !self
            .workspaces
            .get(&number)
            .and_then(|workspace| workspace.root.as_ref())
            .is_some_and(|root| root.contains(key))
        {
            return None;
        }
        let old_active = self.active;
        self.active = number;
        let workspace = self.workspace_mut(number);
        let changed = old_active != number
            || workspace.focused != Some(key)
            || workspace.fullscreen.is_some_and(|full| full != key);
        if workspace.fullscreen.is_some_and(|full| full != key) {
            workspace.fullscreen = None;
        }
        workspace.focus(key);
        Some(changed)
    }

    pub fn contains(&self, key: SurfaceKey) -> bool {
        self.workspaces.values().any(|workspace| {
            workspace
                .root
                .as_ref()
                .is_some_and(|root| root.contains(key))
        })
    }

    pub fn map(&mut self, key: SurfaceKey) {
        if self.contains(key) {
            return;
        }
        let workspace = self.homes.get(&key).copied().unwrap_or(self.active);
        self.workspace_mut(workspace).map(key);
        self.homes.insert(key, workspace);
    }

    /// Place a mapped child immediately after a mapped parent, moving it to
    /// the parent's workspace when necessary. A relationship whose parent is
    /// absent is deliberately a no-op; the shell layer normalizes that case
    /// to an unset parent before reaching here.
    pub fn place_after(&mut self, key: SurfaceKey, parent: SurfaceKey) -> bool {
        if key == parent {
            return false;
        }
        let Some(parent_workspace) = self.workspaces.iter().find_map(|(number, workspace)| {
            workspace
                .root
                .as_ref()
                .is_some_and(|root| root.contains(parent))
                .then_some(*number)
        }) else {
            return false;
        };
        let child_workspace = self.workspaces.iter().find_map(|(number, workspace)| {
            workspace
                .root
                .as_ref()
                .is_some_and(|root| root.contains(key))
                .then_some(*number)
        });
        let mut changed = false;
        if child_workspace.is_some_and(|number| number != parent_workspace) {
            if let Some(number) = child_workspace {
                self.workspace_mut(number).unmap(key);
                changed = true;
            }
        }
        changed |= self.workspace_mut(parent_workspace).place_after(key, parent);
        self.homes.insert(key, parent_workspace);
        changed
    }

    pub fn unmap(&mut self, key: SurfaceKey) {
        let mut workspace = None;
        for (number, candidate) in &self.workspaces {
            if candidate
                .root
                .as_ref()
                .is_some_and(|root| root.contains(key))
            {
                workspace = Some(*number);
                break;
            }
        }
        if let Some(number) = workspace {
            self.workspace_mut(number).unmap(key);
        }
    }

    pub fn forget(&mut self, key: SurfaceKey) {
        self.unmap(key);
        self.homes.remove(&key);
    }

    pub fn unmap_client(&mut self, client: u64) {
        let keys: Vec<SurfaceKey> = self
            .workspaces
            .values()
            .flat_map(Workspace::leaves)
            .filter(|key| key.client == client)
            .collect();
        for key in keys {
            self.unmap(key);
        }
        self.homes.retain(|key, _| key.client != client);
    }

    pub fn apply(&mut self, command: Command) {
        match command {
            Command::Focus(direction) => self.focus_direction(direction),
            Command::Move(direction) => self.move_direction(direction),
            Command::SwitchWorkspace(number) if valid_workspace(number) => {
                self.active = number;
                self.workspace_mut(number);
            }
            Command::MoveToWorkspace(number) if valid_workspace(number) => {
                self.move_to_workspace(number)
            }
            Command::SetPresentation(_) | Command::ToggleGrouped => {
                let Some(focused) = self.focused() else {
                    return;
                };
                // Refused under fullscreen as focus and move are: nothing on
                // screen would report the change, and the operator would leave
                // fullscreen into an arrangement they did not ask for.
                if self
                    .workspaces
                    .get(&self.active)
                    .is_some_and(|workspace| workspace.fullscreen.is_some())
                {
                    return;
                }
                // A lone leaf has no container, and the WORKSPACE is its
                // presenter — same rule the placement pass and the band's
                // buttons read, so all three agree about what a first window
                // is in.
                let workspace = self.workspace_mut(self.active);
                let slot = match workspace.root.as_mut() {
                    Some(Node::Leaf(_)) => Some(&mut workspace.presentation),
                    Some(root) => root.presented_mut(focused),
                    // Unreachable rather than a case: a workspace with no root
                    // has no focused leaf, and the focus above already
                    // returned. An EMPTY workspace therefore cannot be asked
                    // for a presentation, which is why this is not the way to
                    // set one up before the first window opens.
                    None => None,
                };
                if let Some(slot) = slot {
                    *slot = match command {
                        // Grouping picks STACKED, and ungrouping forgets which
                        // mode was in use: a container remembers its axis, not
                        // its presentation, and a mode nobody can see is a
                        // second thing to keep in step with the tree.
                        Command::ToggleGrouped if slot.grouped() => Presentation::Split,
                        Command::ToggleGrouped => Presentation::Stacked,
                        Command::SetPresentation(wanted) => wanted,
                        _ => *slot,
                    };
                }
            }
            Command::ToggleFullscreen => {
                let workspace = self.workspace_mut(self.active);
                workspace.fullscreen = match (workspace.fullscreen, workspace.focused) {
                    (Some(_), _) => None,
                    (None, focused) => focused,
                };
            }
            Command::SwitchWorkspace(_) | Command::MoveToWorkspace(_) => {}
        }
    }

    /// `band` is the height of one title band, passed in beside the gap rather
    /// than known here: how tall a band is belongs to whatever draws one, and
    /// where it goes is what this module decides.
    pub fn placements(
        &self,
        width: usize,
        height: usize,
        gap: usize,
        band: usize,
    ) -> Vec<Placement> {
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return Vec::new();
        };
        visible_placements(workspace, width, height, gap, band)
    }

    pub fn views(&self, width: usize, height: usize, gap: usize, band: usize) -> Vec<ViewLayout> {
        let mut views = Vec::new();
        for (number, workspace) in &self.workspaces {
            let workspace_visible = *number == self.active;
            for mut placement in tiled_placements(workspace, width, height, gap, band) {
                let fullscreen_leaf = workspace.fullscreen == Some(placement.key);
                if fullscreen_leaf {
                    // Undecorated on ANY workspace, not only the visible one:
                    // the rect is overridden here regardless, and the
                    // `fullscreen` field below is gated on visibility, so
                    // deriving the carve from that field would size a hidden
                    // client for the whole output and carve it for a band it
                    // does not have.
                    placement.rect = Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    };
                    // The band goes with it. Nothing reads it here — a
                    // `ViewLayout` drops it — but a `Placement` whose band
                    // overlapped its own client area would contradict the
                    // invariant every other constructor holds.
                    placement.band = Rect {
                        x: 0,
                        y: 0,
                        width,
                        height: 0,
                    };
                }
                views.push(ViewLayout {
                    key: placement.key,
                    rect: placement.rect,
                    visible: workspace_visible
                        && (workspace.fullscreen.is_none() || fullscreen_leaf)
                        && (fullscreen_leaf || placement.visible),
                    activated: workspace_visible && placement.focused,
                    fullscreen: workspace_visible && fullscreen_leaf,
                });
            }
        }
        views
    }

    #[cfg(test)]
    pub fn check_invariants(&self) -> Result<(), String> {
        if !self.workspaces.contains_key(&self.active) {
            return Err(format!("active workspace {} does not exist", self.active));
        }
        let mut seen = BTreeSet::new();
        for (number, workspace) in &self.workspaces {
            // The workspace presents a LONE leaf and nothing else, so a choice
            // held beside any other root is one `map` would hand to a container
            // that already has its own — or would hand twice.
            if workspace.presentation.grouped() && !matches!(workspace.root, Some(Node::Leaf(_))) {
                return Err(format!(
                    "workspace {number} holds a presentation with no lone leaf to present"
                ));
            }
            match (&workspace.root, workspace.focused) {
                (None, None) => {}
                (None, Some(_)) => {
                    return Err(format!("empty workspace {number} retains focus"));
                }
                (Some(_), None) => {
                    return Err(format!("workspace {number} has no focused leaf"));
                }
                (Some(root), Some(focused)) => {
                    root.validate(&mut seen)?;
                    if !root.contains(focused) {
                        return Err(format!("workspace {number} focus is not a leaf"));
                    }
                    for key in workspace.leaves() {
                        if self.homes.get(&key) != Some(number) {
                            return Err(format!(
                                "workspace {number} leaf has the wrong remembered workspace"
                            ));
                        }
                    }
                }
            }
            // The MRU record is what a stack shows, so it has to name every
            // leaf exactly once and nothing else: a stale key outranks a live
            // one and shows a window that is not there.
            let mut ranked = BTreeSet::new();
            for key in &workspace.recent {
                if !ranked.insert(*key) {
                    return Err(format!("workspace {number} ranks a leaf twice"));
                }
                if !workspace
                    .root
                    .as_ref()
                    .is_some_and(|root| root.contains(*key))
                {
                    return Err(format!("workspace {number} ranks a leaf it does not hold"));
                }
            }
            if ranked.len() != workspace.leaves().len() {
                return Err(format!("workspace {number} leaves an unranked leaf"));
            }
            if workspace
                .focused
                .is_some_and(|focused| workspace.recent.first() != Some(&focused))
            {
                return Err(format!("workspace {number} focus is not its most recent"));
            }
            if workspace.fullscreen.is_some_and(|key| {
                !workspace
                    .root
                    .as_ref()
                    .is_some_and(|root| root.contains(key))
            }) {
                return Err(format!("workspace {number} fullscreen leaf is absent"));
            }
            if workspace.fullscreen.is_some() && workspace.fullscreen != workspace.focused {
                return Err(format!("workspace {number} fullscreen leaf is not focused"));
            }
        }
        for (key, number) in &self.homes {
            if !valid_workspace(*number) || !self.workspaces.contains_key(number) {
                return Err(format!(
                    "surface {}:{} remembers an invalid workspace",
                    key.client, key.object
                ));
            }
        }
        let mut activated = self
            .views(VIRTUAL_EXTENT, VIRTUAL_EXTENT, 0, 0)
            .into_iter()
            .filter(|view| view.visible && view.activated)
            .map(|view| view.key);
        if activated.next() != self.focused() || activated.next().is_some() {
            return Err("layout focus does not match exactly one activated view".into());
        }
        Ok(())
    }

    fn workspace_mut(&mut self, number: u8) -> &mut Workspace {
        self.workspaces.entry(number).or_insert_with(Workspace::new)
    }

    fn focus_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused() else {
            return;
        };
        if self
            .workspaces
            .get(&self.active)
            .is_some_and(|workspace| workspace.fullscreen.is_some())
        {
            return;
        }
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return;
        };
        // Inside a stack, `Up`/`Down` walk the RUN rather than the geometry.
        // Nothing else can: a stack gives every leaf beneath it the same
        // content rectangle, so the ranking below has no way to tell them
        // apart, and a stack made from a ROW would answer left and right for
        // bands that visibly run top to bottom. Falling through at the ends
        // is what lets the same key leave the stack for whatever is beyond it.
        if let Some(root) = workspace.root.as_ref() {
            if let Some(target) = stack_neighbour(root, focused, direction) {
                self.workspace_mut(self.active).focus(target);
                return;
            }
        }
        let mut placements = unstacked_placements(workspace, VIRTUAL_EXTENT, VIRTUAL_EXTENT, 0);
        // Past the run, a group is ONE tile. These placements ignore grouping,
        // so a group's leaves lie where the tree puts them, and ranking against
        // them would walk the run a second way: a tabbed column is a vertical
        // split, so `Up` — which its bands do not run along, and which
        // `stack_neighbour` therefore declined — would step to the tab above in
        // the TREE, one the screen shows beside it. Dropping the group's other
        // leaves leaves both pairs of directions with one meaning each: along
        // the run above, and out of the group entirely here.
        if let Some((run, _)) = workspace
            .root
            .as_ref()
            .and_then(|root| stack_run(root, focused))
        {
            placements
                .retain(|placement| placement.key == focused || !run.contains(&placement.key));
        }
        let Some(target) = directional_target(&placements, focused, direction) else {
            return;
        };
        let target = workspace
            .root
            .as_ref()
            .and_then(|root| leaf_entering_stack(root, target, &workspace.recent))
            .unwrap_or(target);
        self.workspace_mut(self.active).focus(target);
    }

    fn move_direction(&mut self, direction: Direction) {
        let Some(focused) = self.focused() else {
            return;
        };
        if self
            .workspaces
            .get(&self.active)
            .is_some_and(|workspace| workspace.fullscreen.is_some())
        {
            return;
        }
        let Some(mut root) = self.workspace_mut(self.active).root.take() else {
            return;
        };
        let axis = direction.axis();
        // Whether the workspace ALREADY runs the way the move is going, which
        // is what tells the two escalations apart. A leaf escalating out of a
        // root that runs this way is at the workspace's EDGE — it went past
        // the last sibling — and wrapping there would nest a container inside
        // one of its own axis: `H[1, 2, 3]` moving 1 left would become
        // `H[1, H[2, 3]]`, thirds turning into a half and two quarters, for a
        // chord that should have done nothing.
        let along_axis = matches!(&root, Node::Split { axis: own, .. } if *own == axis);
        let root = match root.move_leaf(focused, axis, direction.forward()) {
            // No ancestor runs along this axis at all — moving a window up out
            // of a row of them. i3 wraps the workspace in a container that
            // does, and so does this: the alternative is a chord that does
            // nothing on the commonest arrangement there is, two side by side.
            Moved::Escalate if !along_axis => match remove_node(root, focused) {
                Some(rest) => Some(Node::Split {
                    axis,
                    children: if direction.forward() {
                        vec![rest, Node::Leaf(focused)]
                    } else {
                        vec![Node::Leaf(focused), rest]
                    },
                    presentation: Presentation::Split,
                }),
                // The only window on the workspace: nothing to move it past.
                None => Some(Node::Leaf(focused)),
            },
            // A container the leaf left may hold one child now, which the
            // removing path collapses for itself and this one cannot. The
            // untouched cases run it too, and harmlessly: it is a no-op on a
            // tree that has no degenerate container to begin with.
            Moved::Done | Moved::Absent | Moved::Escalate => collapsed(root),
        };
        self.workspace_mut(self.active).root = root;
    }

    fn move_to_workspace(&mut self, number: u8) {
        if number == self.active {
            return;
        }
        let source = self.active;
        let Some(key) = self
            .workspaces
            .get(&source)
            .and_then(|workspace| workspace.focused)
        else {
            return;
        };
        self.workspace_mut(source).unmap(key);
        self.workspace_mut(number).map(key);
        self.homes.insert(key, number);
    }
}

fn visible_placements(
    workspace: &Workspace,
    width: usize,
    height: usize,
    gap: usize,
    band: usize,
) -> Vec<Placement> {
    if let Some(fullscreen) = workspace.fullscreen {
        return vec![Placement {
            key: fullscreen,
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            band: Rect {
                x: 0,
                y: 0,
                width,
                height: 0,
            },
            focused: workspace.focused == Some(fullscreen),
            run: None,
            // A fullscreen window is presented by the output, not by anything
            // in the tree, and its band is zero-height — so nothing marks a
            // presentation while it is up, as no command may set one.
            presented: Presentation::Split,
            visible: true,
        }];
    }
    tiled_placements(workspace, width, height, gap, band)
}

fn tiled_placements(
    workspace: &Workspace,
    width: usize,
    height: usize,
    gap: usize,
    band: usize,
) -> Vec<Placement> {
    laid_out(workspace, width, height, gap, band, true)
}

/// The arrangement the TREE describes, with every stack expanded back into
/// the split it presents. Directional focus and move read this rather than
/// what is on screen: in a stack every leaf shares one content rectangle, so
/// there is no geometry to rank them by — and the order a stack shows is its
/// split's order anyway, so walking the split is walking the stack.
fn unstacked_placements(
    workspace: &Workspace,
    width: usize,
    height: usize,
    gap: usize,
) -> Vec<Placement> {
    laid_out(workspace, width, height, gap, 0, false)
}

fn laid_out(
    workspace: &Workspace,
    width: usize,
    height: usize,
    gap: usize,
    band: usize,
    honour_stacking: bool,
) -> Vec<Placement> {
    let Some(root) = &workspace.root else {
        return Vec::new();
    };
    let inset_x = gap.min(width.saturating_sub(1) / 2);
    let inset_y = gap.min(height.saturating_sub(1) / 2);
    let rect = Rect {
        x: inset_x,
        y: inset_y,
        width: width.saturating_sub(inset_x.saturating_mul(2)),
        height: height.saturating_sub(inset_y.saturating_mul(2)),
    };
    let mut placements = Vec::new();
    let pass = Pass {
        gap,
        band,
        focused: workspace.focused,
        recent: &workspace.recent,
        honour_stacking,
    };
    place_node(root, rect, &pass, &mut placements);
    // A LONE LEAF is presented by its WORKSPACE, the only container it has.
    // The presentation reaches the band's buttons and NOTHING else: a run of
    // one leaf is one band over one content rectangle, which is what an
    // ordinary tile already is, so the geometry is the geometry `place_node`
    // just produced and `run` stays empty — with it the border that wraps a
    // window's band together with its client, which a run gives up.
    //
    // Not on the UNSTACKED pass, which reports what the arrangement would be
    // with every group expanded — a grouped container's leaves come back
    // `Split` there because `place_group` is skipped, and a lone leaf would
    // otherwise be the one placement in that pass still naming a group.
    if honour_stacking && matches!(root, Node::Leaf(_)) {
        if let Some(alone) = placements.first_mut() {
            alone.presented = workspace.presentation;
        }
    }
    placements
}

/// What a placement pass needs beyond the tree and the rectangle it fills.
/// Bundled rather than passed one by one because `place_node` recurses with
/// every one of them unchanged, and the list has outgrown a call signature.
struct Pass<'a> {
    gap: usize,
    band: usize,
    focused: Option<SurfaceKey>,
    /// The workspace's MRU record, most recent first. Only a stack reads it.
    recent: &'a [SurfaceKey],
    /// False for the arrangement directional focus and move walk, where a
    /// stack is expanded back into the split it presents.
    honour_stacking: bool,
}

fn valid_workspace(number: u8) -> bool {
    (INITIAL_WORKSPACE..=FINAL_WORKSPACE).contains(&number)
}

/// The two-child container a drop makes when the target's own runs the other
/// way, with `before` choosing which of them comes first.
fn split_of(axis: Axis, key: SurfaceKey, target: SurfaceKey, before: bool) -> Node {
    let children = if before {
        vec![Node::Leaf(key), Node::Leaf(target)]
    } else {
        vec![Node::Leaf(target), Node::Leaf(key)]
    };
    Node::Split {
        axis,
        children,
        presentation: Presentation::Split,
    }
}

/// Take a leaf out WITHOUT collapsing what it leaves behind, so a container
/// reduced to one child survives long enough for a drop to put the leaf back
/// into it. `collapsed` tidies up afterwards.
fn detach(node: Node, key: SurfaceKey) -> Option<Node> {
    match node {
        Node::Leaf(candidate) if candidate == key => None,
        Node::Leaf(candidate) => Some(Node::Leaf(candidate)),
        Node::Split {
            axis,
            children,
            presentation,
        } => {
            let retained: Vec<Node> = children
                .into_iter()
                .filter_map(|child| detach(child, key))
                .collect();
            if retained.is_empty() {
                return None;
            }
            Some(Node::Split {
                axis,
                children: retained,
                presentation,
            })
        }
    }
}

/// The leaf `direction` reaches from `key` WITHIN the group presenting it, or
/// `None` when there is no group, the direction does not run along it, or the
/// step would leave it.
///
/// The OUTERMOST grouped ancestor is the one that presents `key`, because that
/// is where the renderer stops: `place_node` hands the first grouped container
/// it meets to `place_group`, which draws one band per LEAF beneath it. A group
/// nested inside another is therefore not shown at all, and walking its run
/// would step through bands nobody can see. Same container `presented_mut`
/// finds, for the same reason.
///
/// Only the pair ALONG the run walks it — `Up`/`Down` for a stack, whose bands
/// go down, and `Left`/`Right` for tabs, whose bands go across. Answering
/// `None` for the other pair sends it to the geometry, which leaves the group
/// WHOLE rather than walking it a second way: `focus_direction` drops the
/// group's own leaves before ranking. So a stacked column can be left for its
/// neighbour and a tabbed one stepped out of downwards. Which pair is which is
/// now visible on screen, where under a single stacked mode it was not: a
/// stacked ROW and a stacked COLUMN drew identically and answered
/// `Left`/`Right` differently.
fn stack_neighbour(node: &Node, key: SurfaceKey, direction: Direction) -> Option<SurfaceKey> {
    let (run, axis) = stack_run(node, key)?;
    let step: isize = match (axis, direction) {
        (Axis::Vertical, Direction::Up) | (Axis::Horizontal, Direction::Left) => -1,
        (Axis::Vertical, Direction::Down) | (Axis::Horizontal, Direction::Right) => 1,
        _ => return None,
    };
    let at = run.iter().position(|candidate| *candidate == key)?;
    let next = isize::try_from(at).ok()?.checked_add(step)?;
    run.get(usize::try_from(next).ok()?).copied()
}

/// Where in a stack's run the leaf it SHOWS sits: its own most recently
/// focused. Both the renderer and directional focus ask, and a second copy of
/// the rule would let them disagree about which window is on screen.
///
/// A leaf the record does not name ranks last, and an empty run answers the
/// first. Both are states `check_invariants` forbids and no key sequence
/// reaches; the alternative to a fallback is a panic.
fn shown_index(run: &[SurfaceKey], recent: &[SurfaceKey]) -> usize {
    run.iter()
        .enumerate()
        .min_by_key(|(_, key)| {
            recent
                .iter()
                .position(|candidate| candidate == *key)
                .unwrap_or(usize::MAX)
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// The leaf a directional step ENTERING a group lands on: the one it shows.
///
/// The ranking that produced `target` runs over the UNGROUPED geometry, where a
/// group's leaves hold a fraction of the container each, and nothing about
/// those fractions is on screen — the group draws one leaf. `None` where the
/// target is in no group and the ranking's answer stands.
///
/// Every step that reaches here comes from OUTSIDE the group it lands in, so
/// there is no same-group case to exclude: `focus_direction` drops the leaves
/// of the focused leaf's own group before ranking, and a group holding the
/// focused leaf would be that group or nested in it.
fn leaf_entering_stack(
    root: &Node,
    target: SurfaceKey,
    recent: &[SurfaceKey],
) -> Option<SurfaceKey> {
    let (run, _) = stack_run(root, target)?;
    run.get(shown_index(&run, recent)).copied()
}

/// Every leaf the outermost GROUPED ancestor of `key` presents, in band order
/// — which is `leaves` exactly, since `place_group` builds its run the same
/// way — together with the direction that run travels.
fn stack_run(node: &Node, key: SurfaceKey) -> Option<(Vec<SurfaceKey>, Axis)> {
    let Node::Split {
        children,
        presentation,
        ..
    } = node
    else {
        return None;
    };
    let index = children.iter().position(|child| child.contains(key))?;
    if let Some(run) = presentation.run() {
        let mut keys = Vec::new();
        node.leaves(&mut keys);
        return Some((keys, run));
    }
    stack_run(children.get(index)?, key)
}

fn parent_axis(node: &Node, key: SurfaceKey) -> Option<Axis> {
    let Node::Split { axis, children, .. } = node else {
        return None;
    };
    if children
        .iter()
        .any(|child| matches!(child, Node::Leaf(candidate) if *candidate == key))
    {
        return Some(*axis);
    }
    let index = children.iter().position(|child| child.contains(key))?;
    parent_axis(children.get(index)?, key)
}

fn remove_node(node: Node, key: SurfaceKey) -> Option<Node> {
    match node {
        Node::Leaf(candidate) if candidate == key => None,
        Node::Leaf(candidate) => Some(Node::Leaf(candidate)),
        Node::Split {
            axis,
            children,
            presentation,
        } => {
            let retained: Vec<Node> = children
                .into_iter()
                .filter_map(|child| remove_node(child, key))
                .collect();
            rebuilt(axis, retained, presentation)
        }
    }
}

/// Collapse any container left holding a single child, which a move that
/// takes a leaf out of one can produce in place where a removal cannot.
fn collapsed(node: Node) -> Option<Node> {
    match node {
        Node::Leaf(key) => Some(Node::Leaf(key)),
        Node::Split {
            axis,
            children,
            presentation,
        } => {
            let retained: Vec<Node> = children.into_iter().filter_map(collapsed).collect();
            rebuilt(axis, retained, presentation)
        }
    }
}

fn rebuilt(axis: Axis, mut children: Vec<Node>, presentation: Presentation) -> Option<Node> {
    match children.len() {
        0 => None,
        // A container that collapses to one child is gone, and its
        // presentation goes with it: the survivor is not a stack.
        1 => children.pop(),
        _ => Some(Node::Split {
            axis,
            children,
            presentation,
        }),
    }
}

fn place_node(node: &Node, rect: Rect, pass: &Pass, placements: &mut Vec<Placement>) {
    match node {
        Node::Leaf(key) => {
            // The band and the client PARTITION the tile: the band is taken
            // first and clipped to a tile too short to hold one, and the
            // client is what is left. Written once, so the two cannot be made
            // to disagree by subtracting the same number twice.
            let taken = pass.band.min(rect.height);
            placements.push(Placement {
                key: *key,
                rect: Rect {
                    x: rect.x,
                    y: rect.y.saturating_add(taken),
                    width: rect.width,
                    height: rect.height.saturating_sub(taken),
                },
                band: Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: taken,
                },
                focused: pass.focused == Some(*key),
                run: None,
                presented: Presentation::Split,
                visible: true,
            });
        }
        Node::Split {
            axis,
            children,
            presentation,
        } => {
            if let Some(run) = presentation.run().filter(|_| pass.honour_stacking) {
                place_group(children, rect, run, pass, placements);
                return;
            }
            let rects = split_rects(rect, *axis, children.len(), pass.gap);
            for (child, child_rect) in children.iter().zip(rects) {
                place_node(child, child_rect, pass, placements);
            }
        }
    }
}

/// A GROUPED container: one band per LEAF beneath it, in a run at the top, and
/// the whole area below the run to whichever leaf is focused.
///
/// Per leaf rather than per CHILD, which is where this diverges from i3: td
/// has no container titles, so a split child's band would have to borrow some
/// leaf's name. A nested split's arrangement is therefore not shown while its
/// container is grouped, and ungrouping restores it untouched.
///
/// `run` is the whole of the difference between the two grouped modes. A
/// STACKED run travels down, so it costs one band of height per leaf and the
/// content starts below all of them; a TABBED run travels across, so it costs
/// one band of height however many there are and the bands divide that strip
/// between them. The clipping differs with it: a stack too short for its run
/// is all band and no content, where tabs too narrow simply get thinner and
/// the content is untouched.
fn place_group(
    children: &[Node],
    rect: Rect,
    run: Axis,
    pass: &Pass,
    placements: &mut Vec<Placement>,
) {
    let band = pass.band;
    let mut keys = Vec::new();
    for child in children {
        child.leaves(&mut keys);
    }
    // How much HEIGHT the run costs, which is the one number the two modes
    // disagree about: a band each going down, one band shared going across.
    let taken = match run {
        Axis::Vertical => band.saturating_mul(keys.len()),
        Axis::Horizontal => band,
    }
    .min(rect.height);
    let content = Rect {
        x: rect.x,
        y: rect.y.saturating_add(taken),
        width: rect.width,
        height: rect.height.saturating_sub(taken),
    };
    let bands = match run {
        Axis::Vertical => {
            let bottom = rect.y.saturating_add(rect.height);
            (0..keys.len())
                .map(|index| {
                    let top = rect
                        .y
                        .saturating_add(index.saturating_mul(band))
                        .min(bottom);
                    Rect {
                        x: rect.x,
                        y: top,
                        width: rect.width,
                        height: top.saturating_add(band).min(bottom).saturating_sub(top),
                    }
                })
                .collect()
        }
        // No gap, for the reason bands in a stack have none: they are a run
        // rather than tiles, and a border between them would read as a
        // separation the arrangement does not have.
        Axis::Horizontal => split_rects(
            Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: taken,
            },
            Axis::Horizontal,
            keys.len(),
            0,
        ),
    };
    // The group's own MOST RECENTLY FOCUSED leaf gets the content, which is
    // the focused one whenever focus is in the group at all, since focusing is
    // what puts a leaf at the front of that record. Focus alone would answer
    // only for the group the operator is in and snap every other one back to
    // its first leaf. Both fallbacks are for a leaf the record does not name,
    // which `check_invariants` forbids and no expressible key sequence
    // reaches; the record cannot say "must be present" without a panic.
    let shown = shown_index(&keys, pass.recent);
    for (index, key) in keys.iter().enumerate() {
        let Some(own) = bands.get(index) else {
            continue;
        };
        placements.push(Placement {
            key: *key,
            rect: content,
            band: *own,
            focused: pass.focused == Some(*key),
            run: Some(run),
            presented: Presentation::of_run(run),
            visible: index == shown,
        });
    }
}

fn split_rects(rect: Rect, axis: Axis, count: usize, gap: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let length = match axis {
        Axis::Horizontal => rect.width,
        Axis::Vertical => rect.height,
    };
    let slots = count.saturating_sub(1);
    let preserved = count.min(length);
    let gap_budget = length.saturating_sub(preserved);
    let effective_gap = if slots == 0 {
        0
    } else {
        gap.min(gap_budget.checked_div(slots).unwrap_or(0))
    };
    let available = length.saturating_sub(effective_gap.saturating_mul(slots));
    let base = available.checked_div(count).unwrap_or(0);
    let remainder = available.checked_rem(count).unwrap_or(0);
    let mut offset = 0usize;
    let mut rects = Vec::with_capacity(count);
    for index in 0..count {
        let extent = base.saturating_add(usize::from(index < remainder));
        let child = match axis {
            Axis::Horizontal => Rect {
                x: rect.x.saturating_add(offset),
                y: rect.y,
                width: extent,
                height: rect.height,
            },
            Axis::Vertical => Rect {
                x: rect.x,
                y: rect.y.saturating_add(offset),
                width: rect.width,
                height: extent,
            },
        };
        rects.push(child);
        offset = offset.saturating_add(extent).saturating_add(effective_gap);
    }
    rects
}

fn directional_target(
    placements: &[Placement],
    focused: SurfaceKey,
    direction: Direction,
) -> Option<SurfaceKey> {
    let origin_index = placements
        .iter()
        .position(|placement| placement.key == focused)?;
    let origin = placements.get(origin_index)?;
    placements
        .iter()
        .filter(|candidate| candidate.key != focused)
        .filter_map(|candidate| {
            direction_rank(origin.rect, candidate.rect, direction).map(|rank| (rank, candidate.key))
        })
        .min_by_key(|(rank, key)| (*rank, *key))
        .map(|(_, key)| key)
}

fn direction_rank(origin: Rect, candidate: Rect, direction: Direction) -> Option<(u128, u128)> {
    let origin_x = center(origin.x, origin.width);
    let origin_y = center(origin.y, origin.height);
    let candidate_x = center(candidate.x, candidate.width);
    let candidate_y = center(candidate.y, candidate.height);
    let (ahead, primary, cross, overlap) = match direction {
        Direction::Left => (
            candidate_x < origin_x,
            origin_x.saturating_sub(candidate_x),
            origin_y.abs_diff(candidate_y),
            interval_gap(origin.y, origin.height, candidate.y, candidate.height),
        ),
        Direction::Right => (
            candidate_x > origin_x,
            candidate_x.saturating_sub(origin_x),
            origin_y.abs_diff(candidate_y),
            interval_gap(origin.y, origin.height, candidate.y, candidate.height),
        ),
        Direction::Up => (
            candidate_y < origin_y,
            origin_y.saturating_sub(candidate_y),
            origin_x.abs_diff(candidate_x),
            interval_gap(origin.x, origin.width, candidate.x, candidate.width),
        ),
        Direction::Down => (
            candidate_y > origin_y,
            candidate_y.saturating_sub(origin_y),
            origin_x.abs_diff(candidate_x),
            interval_gap(origin.x, origin.width, candidate.x, candidate.width),
        ),
    };
    (ahead && overlap == 0).then_some((primary, cross))
}

fn center(start: usize, length: usize) -> u128 {
    (start as u128)
        .saturating_mul(2)
        .saturating_add(length as u128)
}

fn interval_gap(
    first_start: usize,
    first_length: usize,
    second_start: usize,
    second_length: usize,
) -> u128 {
    let first_start = first_start as u128;
    let first_end = first_start.saturating_add(first_length as u128);
    let second_start = second_start as u128;
    let second_end = second_start.saturating_add(second_length as u128);
    if first_end <= second_start {
        second_start.saturating_sub(first_end).saturating_add(1)
    } else if second_end <= first_start {
        first_start.saturating_sub(second_end).saturating_add(1)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every placement literal below asks for band height 0, so a band is
    /// the tile's top edge with no height. Spelled once rather than five
    /// times, since none of these tests is about the band.
    fn band_at(x: usize, y: usize, width: usize) -> Rect {
        Rect {
            x,
            y,
            width,
            height: 0,
        }
    }

    /// The drop every test below used to spell as a bare `before`, now that
    /// a drop names the axis it lands along.
    fn beside(axis: Axis, before: bool) -> DropKind {
        DropKind::Beside { axis, before }
    }

    fn key(object: u32) -> SurfaceKey {
        SurfaceKey { client: 1, object }
    }

    #[test]
    fn a_spare_workspace_is_always_offered_and_is_never_the_active_one() {
        let mut layout = Layout::new();
        // Nothing open: workspace 1 is active, so the spare is 2 — the active
        // one is refused even though it is empty, because a window can only be
        // dragged from the workspace in view and a cell naming it moves
        // nothing.
        assert_eq!(layout.active_workspace(), 1);
        assert_eq!(layout.spare_workspace(), Some(2));

        layout.map(key(1));
        assert_eq!(layout.occupied_workspaces(), [1]);
        assert_eq!(layout.spare_workspace(), Some(2));

        // A gap is offered again before the range is walked further along, so
        // emptying a workspace makes its number reusable rather than stranding
        // it. 1 and 3 in use, active 1 → the spare is the hole at 2.
        assert!(layout.move_key_to_workspace(key(1), 3));
        layout.map(key(2));
        assert_eq!(layout.occupied_workspaces(), [1, 3]);
        assert_eq!(layout.spare_workspace(), Some(2));

        // The one case with no spare: everything in the range holds a window
        // except the active one, which cannot be its own drop target.
        let mut full = Layout::new();
        for number in 1..=9u8 {
            full.map(key(u32::from(number)));
            if number != 9 {
                assert!(full.move_key_to_workspace(key(u32::from(number)), number + 1));
            }
        }
        // Windows on 2..=9, active 1 and empty: 1 is the only free number and
        // it is the active one.
        assert_eq!(full.active_workspace(), 1);
        assert_eq!(full.spare_workspace(), None);
    }

    #[test]
    fn a_lone_window_can_be_dragged_once_there_is_a_desktop_to_send_it_to() {
        let mut layout = Layout::new();
        layout.map(key(1));
        // The only window on the workspace, which had nowhere to land before
        // the strip named a spare desktop and now has one. Both ways of
        // picking a window up must agree about this: the band drags it either
        // way, and a refusal here would make Alt the odd one out.
        assert_eq!(layout.spare_workspace(), Some(2));
        assert!(layout.can_drag(key(1)));

        // A window on ANOTHER workspace is not draggable from this one: the
        // gesture is about what is under the pointer, and nothing off the
        // active workspace is on screen to be under it.
        assert!(layout.move_key_to_workspace(key(1), 2));
        assert!(!layout.can_drag(key(1)));

        // Fullscreen still refuses, and for its own reason rather than for
        // want of a destination — a spare desktop exists throughout.
        layout.apply(Command::SwitchWorkspace(2));
        assert_eq!(layout.active_workspace(), 2);
        assert!(layout.can_drag(key(1)));
        layout.apply(Command::ToggleFullscreen);
        assert!(layout.spare_workspace().is_some());
        assert!(!layout.can_drag(key(1)));
    }

    #[test]
    fn a_window_moves_to_a_workspace_by_name_rather_than_by_focus() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        // `key(2)` is what a keyboard command would move, being focused. This
        // one names the OTHER window, which is the whole difference between a
        // drag and `move_to_workspace`.
        assert_eq!(layout.focused(), Some(key(2)));
        assert!(layout.move_key_to_workspace(key(1), 4));
        assert_eq!(layout.workspace_of(key(1)), Some(4));
        assert_eq!(layout.workspace_of(key(2)), Some(1));
        assert_eq!(layout.occupied_workspaces(), [1, 4]);

        // Moving one to where it already is changes nothing and says so, which
        // is what stops a drop onto the active cell reporting a repaint.
        assert!(!layout.move_key_to_workspace(key(1), 4));
        // Outside the range is refused rather than creating a tenth desktop.
        assert!(!layout.move_key_to_workspace(key(1), 0));
        assert!(!layout.move_key_to_workspace(key(1), 10));
        assert_eq!(layout.workspace_of(key(1)), Some(4));
        // A window nothing knows about has no home to move out of.
        assert!(!layout.move_key_to_workspace(key(99), 2));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_presentation_and_the_direction_it_runs_name_each_other() {
        // `of_run` recovers what `run` discarded, which is sound only while
        // `run` is injective over the grouped presentations — a property the
        // compiler does not check and a third grouped presentation would
        // quietly break, leaving `place_group` naming the wrong one.
        for presentation in [Presentation::Stacked, Presentation::Tabbed] {
            let run = presentation.run().expect("a grouped presentation runs");
            assert_eq!(Presentation::of_run(run), presentation);
        }
        assert_eq!(Presentation::Split.run(), None);
        assert!(!Presentation::Split.grouped());
    }

    /// Open `object` BELOW whatever is focused, splitting that window's tile
    /// rather than joining its container — which is what `SetSplit(Vertical)`
    /// followed by a map used to do in one step. Performed as the drop an
    /// operator makes on a window's bottom edge, so the arrangement it
    /// produces is one the running compositor can be driven into.
    fn map_below(layout: &mut Layout, object: u32) {
        let over = layout.focused().expect("nothing focused to open below");
        layout.map(key(object));
        assert!(
            layout.drop_onto(key(object), over, beside(Axis::Vertical, false)),
            "the drop that opens a window below another moved nothing"
        );
    }

    /// Open `object` BESIDE whatever is focused, splitting that window's tile
    /// horizontally — the mirror of `map_below`, and the way to put a ROW
    /// inside a column now that a new window joins the container it opens in.
    fn map_beside(layout: &mut Layout, object: u32) {
        let over = layout.focused().expect("nothing focused to open beside");
        layout.map(key(object));
        assert!(
            layout.drop_onto(key(object), over, beside(Axis::Horizontal, false)),
            "the drop that opens a window beside another moved nothing"
        );
    }

    /// Map `objects` as one COLUMN, the way an operator reaches one: windows
    /// open as a row of columns, and moving the second DOWN is what makes a
    /// column at all — after which every later window joins it, since a new
    /// window joins the container the focused one is in.
    ///
    /// This replaced `SetSplit(Axis::Vertical)`, which chose the axis of the
    /// next split before it happened. The sequence is longer and it is the
    /// one a person performs, so a test built this way cannot pass on a rule
    /// no key sequence can reach.
    fn column(layout: &mut Layout, objects: &[u32]) {
        for (index, object) in objects.iter().enumerate() {
            layout.map(key(*object));
            if index == 1 {
                layout.apply(Command::Move(Direction::Down));
            }
        }
    }

    fn rect(layout: &Layout, object: u32) -> Rect {
        let placements = layout.placements(100, 100, 0, 0);
        let index = placements
            .iter()
            .position(|placement| placement.key == key(object))
            .unwrap();
        placements
            .get(index)
            .map(|placement| placement.rect)
            .unwrap()
    }

    #[test]
    fn a_stacked_column_runs_its_bands_and_gives_the_focused_leaf_the_rest() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);

        // Unstacked first, so the SAME arrangement is measured both ways and
        // the difference is the presentation rather than the tree.
        let split = layout.placements(100, 300, 0, 20);
        assert_eq!(split.len(), 3);
        assert!(split.iter().all(|placement| placement.visible));
        assert_eq!(
            split.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 100, 200]
        );

        layout.apply(Command::ToggleGrouped);
        let stacked = layout.placements(100, 300, 0, 20);
        assert_eq!(stacked.len(), 3);
        // Every band in a run at the top, one per LEAF and in tree order.
        assert_eq!(
            stacked.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20, 40]
        );
        assert!(stacked
            .iter()
            .all(|p| p.band.height == 20 && p.band.width == 100));
        // The content is below the whole run, and every leaf is SIZED for it
        // — a stacked-away client keeps its buffer rather than resizing down
        // and back on every toggle.
        for placement in &stacked {
            assert_eq!(
                placement.rect,
                Rect {
                    x: 0,
                    y: 60,
                    width: 100,
                    height: 240
                }
            );
        }
        // ...but only the focused one is shown.
        assert_eq!(
            stacked.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(layout.focused(), Some(key(3)));

        // Focus moves the shown leaf without touching the run.
        layout.apply(Command::Focus(Direction::Up));
        let moved = layout.placements(100, 300, 0, 20);
        assert_eq!(
            moved.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20, 40]
        );
        assert_eq!(
            moved.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [false, true, false]
        );

        // And toggling back restores the split exactly: nothing about the
        // tree changed, only how it is presented.
        layout.apply(Command::ToggleGrouped);
        let restored = layout.placements(100, 300, 0, 20);
        let geometry = |placements: &[Placement]| {
            placements
                .iter()
                .map(|p| (p.key, p.rect, p.band, p.run.is_some(), p.visible))
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(&restored), geometry(&split));
        // Focus is the one thing that should differ: it moved in between.
        assert_eq!(
            restored
                .iter()
                .filter(|p| p.focused)
                .map(|p| p.key)
                .collect::<Vec<_>>(),
            [key(2)]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_new_window_joins_its_neighbours_own_container_not_a_grandparent() {
        // `H[H[1, 3], 2]` — same-axis nesting, which a drop and the collapse
        // after it really do build: 3 is dropped beside 1 across the column
        // it was in, leaving that column holding one child.
        let mut layout = Layout::new();
        for object in 1..=3 {
            layout.map(key(object));
        }
        assert!(layout.drop_onto(
            key(3),
            key(1),
            DropKind::Beside {
                axis: Axis::Vertical,
                before: false,
            }
        ));
        assert!(layout.drop_onto(
            key(3),
            key(1),
            DropKind::Beside {
                axis: Axis::Horizontal,
                before: false,
            }
        ));
        assert!(layout.focus_key(key(1)));
        layout.apply(Command::SetPresentation(Presentation::Tabbed));
        let grouped = |layout: &Layout, object: u32| {
            layout
                .placements(120, 300, 0, 20)
                .into_iter()
                .position(|placement| placement.key == key(object) && placement.run.is_some())
        };
        assert!(
            grouped(&layout, 1).is_some(),
            "the inner pair is not a group"
        );
        assert!(grouped(&layout, 2).is_none(), "2 is inside the group");

        // Opening a window with 1 focused must land it in 1's OWN container —
        // the one drawn as a strip of tabs — rather than in the root, which
        // runs the same way and would otherwise take the insert. Landing it
        // outside is landing it somewhere the operator is not looking.
        layout.map(key(4));
        assert!(grouped(&layout, 4).is_some(), "4 landed outside the group");
        assert_eq!(layout.focused(), Some(key(4)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_tabbed_column_runs_its_bands_across_and_costs_one_bands_height() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::SetPresentation(Presentation::Tabbed));

        let tabbed = layout.placements(100, 300, 0, 20);
        assert_eq!(tabbed.len(), 3);
        // The bands divide ONE strip across the top rather than running down
        // it: same row, side by side, in tree order.
        assert_eq!(
            tabbed
                .iter()
                .map(|p| (p.band.x, p.band.width))
                .collect::<Vec<_>>(),
            [(0, 34), (34, 33), (67, 33)]
        );
        assert!(tabbed.iter().all(|p| p.band.y == 0 && p.band.height == 20));
        // So the run costs one band of height, not three — which is the whole
        // difference between the two modes and what a tab is FOR.
        for placement in &tabbed {
            assert_eq!(
                placement.rect,
                Rect {
                    x: 0,
                    y: 20,
                    width: 100,
                    height: 280
                }
            );
        }
        assert_eq!(
            tabbed.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert!(tabbed.iter().all(|p| p.run == Some(Axis::Horizontal)));

        // Switching to stacked is a change of PRESENTATION and nothing else:
        // the same three leaves in the same order, laid out the other way.
        layout.apply(Command::SetPresentation(Presentation::Stacked));
        let stacked = layout.placements(100, 300, 0, 20);
        assert_eq!(
            stacked.iter().map(|p| p.key).collect::<Vec<_>>(),
            tabbed.iter().map(|p| p.key).collect::<Vec<_>>()
        );
        assert_eq!(
            stacked
                .iter()
                .map(|p| (p.band.y, p.band.height))
                .collect::<Vec<_>>(),
            [(0, 20), (20, 20), (40, 20)]
        );
        assert!(stacked.iter().all(|p| p.run == Some(Axis::Vertical)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_tabbed_run_walks_across_where_a_stacked_one_walks_down() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::SetPresentation(Presentation::Stacked));
        assert_eq!(layout.focused(), Some(key(3)));

        // A stacked run answers to Up/Down and ignores Left/Right, since its
        // bands are what the direction names.
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(2)));

        // Tabbed is the same run seen the other way round, so the pair of
        // directions that walks it swaps with the pair that does not.
        layout.apply(Command::SetPresentation(Presentation::Tabbed));
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::Focus(Direction::Right));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.apply(Command::Focus(Direction::Left));
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(1)));
        // And the ends hold rather than wrapping, as the stacked run's do.
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(1)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn ungrouping_a_tabbed_column_and_grouping_it_again_gives_a_stack() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2]);
        layout.apply(Command::SetPresentation(Presentation::Tabbed));
        let grouped = |layout: &Layout| {
            layout
                .placements(100, 300, 0, 20)
                .first()
                .and_then(|placement| placement.run)
        };
        assert_eq!(grouped(&layout), Some(Axis::Horizontal));

        // A split container records no former presentation, so the toggle has
        // nothing to restore and picks the one it always did. Choosing the
        // other back is what Super+h is for.
        layout.apply(Command::ToggleGrouped);
        assert_eq!(grouped(&layout), None);
        layout.apply(Command::ToggleGrouped);
        assert_eq!(grouped(&layout), Some(Axis::Vertical));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn grouping_takes_the_focused_leafs_own_container_and_a_lone_windows_workspace() {
        let mut layout = Layout::new();
        layout.map(key(1));
        // A lone window IS the root and has no container, so its WORKSPACE
        // presents it — which is what puts a presentation on the very first
        // window rather than leaving its band with nothing to offer. The whole
        // placement but `presented` is unchanged, `run` among it: a run of one
        // leaf draws one band over one content rectangle, which is what an
        // ordinary tile already is, and a `run` would cost this window the
        // border around its own title bar.
        let alone = layout.placements(100, 300, 0, 20);
        layout.apply(Command::ToggleGrouped);
        let grouped = layout.placements(100, 300, 0, 20);
        assert_eq!(
            grouped.first().map(|placement| placement.presented),
            Some(Presentation::Stacked),
            "the workspace did not take the presentation"
        );
        let but_presented = |placements: &[Placement]| {
            placements
                .iter()
                .map(|placement| Placement {
                    presented: Presentation::Split,
                    ..*placement
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(but_presented(&grouped), but_presented(&alone));
        layout.apply(Command::ToggleGrouped);
        assert_eq!(layout.placements(100, 300, 0, 20), alone);

        // A column nested inside a row: stacking from inside it takes THAT
        // container, so the row beside it keeps its own arrangement.
        layout.map(key(2));
        map_below(&mut layout, 3);
        assert_eq!(layout.focused(), Some(key(3)));
        layout.apply(Command::ToggleGrouped);
        let placements = layout.placements(200, 300, 0, 20);
        let shown: Vec<bool> = placements.iter().map(|p| p.visible).collect();
        assert_eq!(shown, [true, false, true], "the outer row was stacked too");
        // key(1) keeps the left half; the stacked pair shares the right.
        let left = placements.first().unwrap();
        assert_eq!((left.rect.x, left.rect.width), (0, 100));
        assert_eq!(left.band.y, 0);
        // The stack fills the CONTAINER's half, spelled out rather than read
        // back off the placement: a run laid out at the output's left edge
        // instead of its container's is a stack drawn over its neighbour, and
        // every coordinate of it agrees with itself.
        for placement in placements.iter().skip(1) {
            assert_eq!((placement.band.x, placement.band.width), (100, 100));
            assert_eq!((placement.rect.x, placement.rect.width), (100, 100));
        }
        let stacked_band: Vec<usize> = placements.iter().skip(1).map(|p| p.band.y).collect();
        assert_eq!(stacked_band, [0, 20]);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn the_workspace_hands_its_presentation_over_and_keeps_no_copy() {
        // Both halves of the hand-over are load-bearing. The workspace must
        // not KEEP the choice: a container that collapses back to one leaf is
        // gone and its presentation with it, so the leaf left behind is
        // ungrouped rather than presented by a workspace still holding what
        // that container was given.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::ToggleGrouped);
        layout.map(key(2));
        layout.unmap(key(2));
        assert_eq!(
            layout
                .placements(100, 300, 0, 20)
                .first()
                .map(|placement| placement.presented),
            Some(Presentation::Split),
            "the workspace kept a copy of what it handed over"
        );
        layout.check_invariants().unwrap();

        // And a workspace holding NOTHING must not overwrite the container a
        // window arrives into. `Split` is both "no choice" and a presentation,
        // so handing it on unconditionally would ungroup a grouped root every
        // time a window opened in it.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleGrouped);
        layout.map(key(3));
        assert!(
            layout
                .placements(100, 300, 0, 20)
                .iter()
                .all(|placement| placement.presented == Presentation::Stacked),
            "a new window ungrouped the container it joined"
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn an_emptied_workspace_forgets_the_presentation_its_last_window_had() {
        // A container is destroyed with its last child and its presentation
        // goes with it. The workspace's own is that setting one level out, so
        // it goes the same way — otherwise a window opened long afterwards is
        // grouped by one that is gone, and the window after THAT joins it.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::ToggleGrouped);
        layout.unmap(key(1));
        layout.map(key(2));
        // `presented` and NOT `run`: a lone leaf never carries a run, which is
        // the whole of why the two are separate fields, so asking about that
        // one here would be asking a question with a constant answer.
        assert_eq!(
            layout
                .placements(100, 300, 0, 20)
                .first()
                .map(|placement| placement.presented),
            Some(Presentation::Split),
            "a new window inherited the closed one's grouping"
        );
        layout.map(key(3));
        assert!(
            layout
                .placements(100, 300, 0, 20)
                .iter()
                .all(|placement| placement.run.is_none()
                    && placement.presented == Presentation::Split),
            "the container it grew was grouped by a window that is gone"
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_group_undoes_from_a_leaf_that_is_no_child_of_it() {
        // A row holding a COLUMN and a leaf, stacked from the leaf — which is
        // the only way to group a container that holds a split, since
        // grouping always takes the innermost container the focused leaf is a
        // direct child of. From 1 or 3 that would be the column; from 2 it is
        // the row.
        //
        // Undoing it then has to work from 1 as well, and 1 is no child of
        // that row: `Mod+S` reaches it by walking DOWN to the leaf and taking
        // the OUTERMOST grouped ancestor, rather than up from the leaf's own
        // parent. Without that the row could never be undone from a window it
        // is showing.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        assert!(layout.focus_key(key(1)));
        map_below(&mut layout, 3);
        assert!(layout.focus_key(key(2)));
        layout.apply(Command::ToggleGrouped);

        // All three run in the one stack, sharing the one content rectangle:
        // the column inside it is a container the stack never shows.
        let stacked = layout.placements(200, 400, 0, 20);
        assert_eq!(stacked.len(), 3);
        let content = stacked.first().unwrap().rect;
        assert!(stacked
            .iter()
            .all(|p| p.rect == content && p.run == Some(Axis::Vertical)));
        assert_eq!(
            stacked.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20, 40]
        );

        assert!(layout.focus_key(key(1)));
        layout.apply(Command::ToggleGrouped);
        let split = layout.placements(200, 400, 0, 20);
        assert!(
            split.iter().all(|p| p.visible && p.run.is_none()),
            "a leaf that is no child of the stack could not unstack it"
        );
        // A column of two beside a lone window, which is what the tree said
        // all along.
        assert_eq!(
            split
                .iter()
                .map(|p| (p.band.x, p.band.y))
                .collect::<Vec<_>>(),
            [(0, 0), (0, 200), (100, 0)]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stack_survives_an_unmap_and_loses_its_presentation_only_when_it_collapses() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::ToggleGrouped);

        // Closing one window of three leaves a stack of two. `remove_node`
        // rebuilds the container, so its presentation has to be carried over
        // rather than defaulted.
        layout.unmap(key(2));
        let two = layout.placements(100, 300, 0, 20);
        assert_eq!(two.len(), 2);
        assert!(two.iter().all(|p| p.run.is_some()));
        assert_eq!(two.iter().map(|p| p.band.y).collect::<Vec<_>>(), [0, 20]);

        // Closing a second collapses the container into its survivor, and a
        // lone window is not a stack: there is nothing left to present.
        layout.unmap(key(1));
        let one = layout.placements(100, 300, 0, 20);
        assert_eq!(one.len(), 1);
        assert!(one.iter().all(|p| p.run.is_none() && p.visible));
        assert_eq!(one.first().unwrap().band.y, 0);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stacked_away_leaf_is_published_hidden_at_the_size_it_would_have() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2]);
        layout.apply(Command::ToggleGrouped);

        // What the CLIENT is told, which is the one consumer of `visible`
        // outside the renderer: not visible, but sized for the content area
        // it would fill, so it holds a buffer of the right size for the
        // moment it is shown.
        let views = layout.views(100, 300, 0, 20);
        // `position` + `get` rather than the iterator method that would read
        // more naturally: the bootstrap ladder's guard scans this source for
        // that name and cannot tell an iterator from GNU findutils.
        let view_of = |target| {
            let at = views.iter().position(|view| view.key == target).unwrap();
            *views.get(at).unwrap()
        };
        let hidden = view_of(key(1));
        let shown = view_of(key(2));
        assert!(!hidden.visible && shown.visible);
        assert_eq!(hidden.rect, shown.rect);
        assert_eq!(
            hidden.rect,
            Rect {
                x: 0,
                y: 40,
                width: 100,
                height: 260
            }
        );
        assert!(!hidden.activated && shown.activated);
    }

    #[test]
    fn fullscreen_refuses_to_stack_as_it_refuses_to_focus_and_move() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2]);
        layout.apply(Command::ToggleFullscreen);

        // Nothing on screen could report the change, so the operator would
        // leave fullscreen into an arrangement they never asked for.
        layout.apply(Command::ToggleGrouped);
        layout.apply(Command::ToggleFullscreen);
        let placements = layout.placements(100, 300, 0, 20);
        assert!(placements.iter().all(|p| p.run.is_none() && p.visible));
        assert_eq!(
            placements.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 150]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn moving_inside_a_stack_reorders_the_run_and_the_shown_leaf_with_it() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::ToggleGrouped);
        assert_eq!(layout.focused(), Some(key(3)));

        // Move walks the UNSTACKED arrangement, so it reaches the leaf above
        // even though the two share one content rectangle on screen. The run
        // is the tree's order, so the band order changes with it.
        layout.apply(Command::Move(Direction::Up));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key).collect::<Vec<_>>(),
            [key(1), key(3), key(2)]
        );
        assert_eq!(
            placements.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20, 40]
        );
        // The moved leaf is still the one shown; it is still focused.
        assert_eq!(
            placements.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [false, true, false]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stack_keeps_showing_its_own_last_focused_leaf_after_focus_leaves_it() {
        // A stacked column on the right of a lone window. Focus the MIDDLE
        // leaf of the stack, then leave for the window beside it: the stack
        // must still be showing that leaf. Focus names one leaf per workspace
        // and it is no longer in the stack, so anything reading focus alone
        // falls back to the stack's FIRST leaf and the column snaps.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        layout.map(key(4));
        layout.apply(Command::ToggleGrouped);

        assert!(layout.focus_key(key(3)));
        let inside = layout.placements(200, 300, 0, 20);
        assert_eq!(
            inside.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [true, false, true, false]
        );

        assert!(layout.focus_key(key(1)));
        let outside = layout.placements(200, 300, 0, 20);
        assert_eq!(
            outside.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [true, false, true, false],
            "the stack snapped back to its first leaf"
        );
        assert!(outside.iter().all(|p| !p.focused || p.key == key(1)));

        // And it follows the record rather than freezing: focusing a
        // different leaf of the stack and leaving again shows that one.
        assert!(layout.focus_key(key(4)));
        assert!(layout.focus_key(key(1)));
        assert_eq!(
            layout
                .placements(200, 300, 0, 20)
                .iter()
                .map(|p| p.visible)
                .collect::<Vec<_>>(),
            [true, false, false, true]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stack_taller_than_its_container_clips_its_run_and_keeps_no_content() {
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::ToggleGrouped);

        // Three 20-row bands want 60 rows and the container has 50. The run
        // is clipped to the container rather than overhanging it, the last
        // band takes what is left, and there is no content area at all.
        let placements = layout.placements(100, 50, 0, 20);
        assert_eq!(
            placements
                .iter()
                .map(|p| (p.band.y, p.band.height))
                .collect::<Vec<_>>(),
            [(0, 20), (20, 20), (40, 10)]
        );
        for placement in &placements {
            assert_eq!(placement.rect.height, 0);
            assert_eq!(placement.rect.y, 50);
        }
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_tile_too_short_for_a_band_is_all_band_and_no_client() {
        let mut layout = Layout::new();
        layout.map(key(1));

        // Clipped to the tile rather than overhanging it. Asserted HERE
        // rather than through the renderer's partition test, which derives
        // the tile from the band plus the client and so cannot see this: an
        // unclipped 20-row band on a 12-row tile leaves the client at zero
        // height either way, and band-plus-client is then 20, which agrees
        // with itself. Same self-satisfying shape that let a whole output
        // size stop testing anything two commits ago.
        let short = layout.placements(40, 12, 0, 20);
        let placement = short.first().unwrap();
        assert_eq!((placement.band.y, placement.band.height), (0, 12));
        assert_eq!((placement.rect.y, placement.rect.height), (12, 0));

        // And a tile with room keeps exactly the band it asked for.
        let tall = layout.placements(40, 100, 0, 20);
        let placement = tall.first().unwrap();
        assert_eq!((placement.band.y, placement.band.height), (0, 20));
        assert_eq!((placement.rect.y, placement.rect.height), (20, 80));
        assert_eq!(placement.band.width, placement.rect.width);
    }

    #[test]
    fn maps_horizontal_then_nests_the_selected_vertical_split() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);

        assert_eq!(
            layout.placements(100, 100, 0, 0),
            [
                Placement {
                    key: key(1),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 50,
                        height: 100
                    },
                    band: band_at(0, 0, 50),
                    focused: false,
                    run: None,
                    presented: Presentation::Split,
                    visible: true
                },
                Placement {
                    key: key(2),
                    rect: Rect {
                        x: 50,
                        y: 0,
                        width: 50,
                        height: 50
                    },
                    band: band_at(50, 0, 50),
                    focused: false,
                    run: None,
                    presented: Presentation::Split,
                    visible: true
                },
                Placement {
                    key: key(3),
                    rect: Rect {
                        x: 50,
                        y: 50,
                        width: 50,
                        height: 50
                    },
                    band: band_at(50, 50, 50),
                    focused: true,
                    run: None,
                    presented: Presentation::Split,
                    visible: true
                }
            ]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn focus_key_takes_a_visible_leaf_and_refuses_everything_else() {
        let mut layout = Layout::new();
        layout.map(key(1));
        map_below(&mut layout, 2);
        assert_eq!(layout.focused(), Some(key(2)));

        assert!(layout.focus_key(key(1)));
        assert_eq!(layout.focused(), Some(key(1)));
        // Already focused: no move, so a caller owes no repaint.
        assert!(!layout.focus_key(key(1)));
        // Never mapped.
        assert!(!layout.focus_key(key(99)));
        assert_eq!(layout.focused(), Some(key(1)));

        // A leaf of ANOTHER workspace is not focusable from this one: the
        // keyboard would go somewhere the screen does not show.
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(3));
        assert!(!layout.focus_key(key(1)));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.apply(Command::SwitchWorkspace(1));
        assert!(!layout.focus_key(key(3)));

        // Fullscreen hides its siblings, so only the fullscreen leaf takes.
        layout.apply(Command::ToggleFullscreen);
        assert!(!layout.focus_key(key(2)));
        assert_eq!(layout.focused(), Some(key(1)));
        layout.apply(Command::ToggleFullscreen);
        assert!(layout.focus_key(key(2)));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn application_activation_reveals_its_workspace_and_leaves_other_fullscreen() {
        let mut layout = Layout::new();
        layout.map(key(1));
        map_below(&mut layout, 2);
        layout.apply(Command::ToggleFullscreen);
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(3));

        assert_eq!(layout.activate_key(key(1)), Some(true));
        assert_eq!(layout.focused(), Some(key(1)));
        let views = layout.views(100, 100, 0, 0);
        assert!(views
            .iter()
            .any(|view| view.key == key(1) && view.visible && view.activated));
        assert!(views.iter().all(|view| !view.fullscreen));
        assert_eq!(layout.activate_key(key(1)), Some(false));
        assert_eq!(layout.activate_key(key(99)), None);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_workspace_is_occupied_only_while_it_holds_a_window() {
        // Switching to a workspace CREATES its entry in the map, so a filter
        // on the map alone would name every number anybody has been to — and
        // the bar draws a cell per name. The trip that separates the two is
        // going somewhere empty and coming back.
        let mut layout = Layout::new();
        layout.map(key(1));
        assert_eq!(layout.occupied_workspaces(), vec![1]);

        layout.apply(Command::SwitchWorkspace(7));
        assert_eq!(layout.occupied_workspaces(), vec![1]);
        layout.map(key(2));
        assert_eq!(layout.occupied_workspaces(), vec![1, 7]);

        // And it stops being occupied when its last window goes, rather than
        // keeping a cell for as long as the compositor runs.
        layout.unmap(key(2));
        assert_eq!(layout.occupied_workspaces(), vec![1]);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn focus_entering_a_stack_lands_on_the_leaf_it_is_showing() {
        // Leaving a stack and coming back has to be a round trip. It was not:
        // the ranking runs over the UNSTACKED geometry, where 2 and 3 have a
        // half of the column each, so a step in from the left landed on 2 —
        // and since focusing re-fronts the record `place_group` reads, the
        // stack came back SHOWING 2 as well. The operator left 3 and returned
        // to a different window with a different one drawn.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        layout.apply(Command::ToggleGrouped);
        assert_eq!(layout.focused(), Some(key(3)));
        // `visible`, not the rectangle: a leaf stacked away keeps the `rect`
        // it WOULD have, so the two say different things here.
        let shown = |layout: &Layout| {
            layout
                .placements(240, 600, 0, 20)
                .iter()
                .filter(|placement| placement.visible)
                .map(|placement| placement.key)
                .collect::<Vec<_>>()
        };
        let drawn = shown(&layout);
        assert!(
            drawn.contains(&key(3)) && !drawn.contains(&key(2)),
            "the stack is not showing 3 to begin with: {drawn:?}"
        );

        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(1)), "the stack was not left");
        layout.apply(Command::Focus(Direction::Right));
        assert_eq!(
            layout.focused(),
            Some(key(3)),
            "coming back landed on a leaf the stack was not showing"
        );
        assert_eq!(shown(&layout), drawn, "the round trip redrew the stack");

        // Again with the run's FIRST leaf shown, which is what makes this
        // about the RECORD rather than about a position in the run: 3 is the
        // run's last as well as the shown one, so the trip above holds just as
        // well for an answer that always took the last leaf.
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(2)));
        let drawn = shown(&layout);
        assert!(
            drawn.contains(&key(2)) && !drawn.contains(&key(3)),
            "the stack is not showing 2: {drawn:?}"
        );
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(1)), "the stack was not left");
        layout.apply(Command::Focus(Direction::Right));
        assert_eq!(
            layout.focused(),
            Some(key(2)),
            "coming back took a position in the run rather than the record"
        );
        assert_eq!(shown(&layout), drawn, "the round trip redrew the stack");
        layout.check_invariants().unwrap();
    }

    #[test]
    fn up_and_down_walk_a_stack_whatever_axis_it_was_made_from() {
        // A stack shows its bands top to bottom whatever the container is, so
        // Up/Down follow that run. The ROW is the case that needs saying: its
        // leaves expand back into a horizontal split, which answered only
        // Left/Right while the operator was looking at a vertical list.
        let mut layout = Layout::new();
        for object in 1..=3 {
            layout.map(key(object));
        }
        assert_eq!(layout.parent_axis(key(3)), Some(Axis::Horizontal));
        layout.apply(Command::ToggleGrouped);
        assert_eq!(layout.focused(), Some(key(3)));

        for expected in [2, 1] {
            layout.apply(Command::Focus(Direction::Up));
            assert_eq!(layout.focused(), Some(key(expected)), "up to {expected}");
        }
        // The top of the run, and nothing above the stack to fall through to.
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(1)));
        for expected in [2, 3] {
            layout.apply(Command::Focus(Direction::Down));
            assert_eq!(layout.focused(), Some(key(expected)), "down to {expected}");
        }
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(3)));

        // Left/Right do not walk it, though this stack was made FROM a row and
        // the tree beneath still is one. The bands run down, so the pair that
        // runs across leaves the group — and there is nothing outside this one
        // to leave for.
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn left_and_right_leave_a_stack_rather_than_walking_it() {
        // A stacked COLUMN beside another window is what tells the two apart:
        // in a stacked ROW the run and the geometry agree either way, so only
        // this shape shows that Left LEAVES rather than walking. Walking the
        // run in every direction would trap the operator in the stack. The
        // `Right` back is the redirected step, not an untouched one.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        assert_eq!(layout.parent_axis(key(3)), Some(Axis::Vertical));
        layout.apply(Command::ToggleGrouped);
        assert_eq!(layout.focused(), Some(key(3)));

        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(1)), "Left must leave the stack");
        // Coming BACK lands on the leaf the stack is SHOWING, so the trip is
        // a round one; `focus_entering_a_stack_lands_on_the_leaf_it_is_showing`
        // is that property by itself.
        layout.apply(Command::Focus(Direction::Right));
        assert_eq!(layout.focused(), Some(key(3)));
        // Up still walks the run it is in.
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_nested_stack_walks_the_run_that_is_actually_drawn() {
        // `Hstacked[1, Vstacked[2, 3]]`. The renderer stops at the OUTER
        // stack and gives it one band per leaf beneath it, so the screen shows
        // a single run [1, 2, 3] and the inner stack is not presented at all.
        // Walking the nearest stacked ancestor instead would consult the
        // hidden run [2, 3], where `Up` from 2 has nowhere to go.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        layout.apply(Command::ToggleGrouped);
        assert_eq!(layout.focused(), Some(key(3)));
        // Now stack the outer row as well, from a leaf that is its child.
        layout.focus_key(key(1));
        layout.apply(Command::ToggleGrouped);
        let bands = layout.placements(200, 400, 0, 20);
        assert!(bands.iter().all(|placement| placement.run.is_some()));
        assert_eq!(
            bands
                .iter()
                .map(|placement| placement.key.object)
                .collect::<Vec<_>>(),
            [1, 2, 3],
            "the drawn run is the outer stack's"
        );

        layout.focus_key(key(2));
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(1)), "up the DRAWN run");
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_step_off_the_end_of_a_stack_leaves_it() {
        // `V[Hstacked[1, 3], 2]` — the stack must NOT be the root, or every
        // step is an ordinary run walk and nothing here leaves anything. The
        // run is `[1, 3]` across the top half, with 2 below the whole of it,
        // so `Down` from the LAST band is the fall-through: the run has no
        // next leaf and the geometry answers with the window underneath.
        let mut layout = Layout::new();
        layout.map(key(1));
        map_below(&mut layout, 2);
        assert!(layout.focus_key(key(1)));
        map_beside(&mut layout, 3);
        layout.apply(Command::ToggleGrouped);
        let run: Vec<u32> = layout
            .placements(200, 400, 0, 20)
            .iter()
            .filter(|placement| placement.run.is_some())
            .map(|placement| placement.key.object)
            .collect();
        assert_eq!(run, [1, 3], "the stack is the top half, not the root");

        assert_eq!(layout.focused(), Some(key(3)));
        layout.apply(Command::Focus(Direction::Up));
        assert_eq!(layout.focused(), Some(key(1)), "walked the run");
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(3)), "walked it back");
        // Off the end of the run, so the geometry answers: 2 is below.
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(2)), "left the stack");
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stack_of_splits_walks_every_band_it_draws() {
        // `Hstacked[V[1, 3], 2]`. td draws one band per LEAF beneath a stack
        // rather than one per child — `place_group` says so — so the run is
        // all three. Walking per CHILD would step 1 to 2 and skip the band for
        // 3 that is drawn between them.
        //
        // The column is made FIRST and the row grouped from 2, the leaf still
        // directly in it: grouping takes the innermost container the focused
        // leaf is a direct child of, so doing it from 1 would take the column.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        assert!(layout.focus_key(key(1)));
        map_below(&mut layout, 3);
        assert!(layout.focus_key(key(2)));
        layout.apply(Command::ToggleGrouped);
        let placements = layout.placements(200, 400, 0, 20);
        assert!(
            placements.iter().all(|placement| placement.run.is_some()),
            "the ROW is the stack, and it presents all three"
        );
        let run: Vec<u32> = placements
            .iter()
            .map(|placement| placement.key.object)
            .collect();
        assert_eq!(run, [1, 3, 2], "the drawn band order");
        assert!(layout.focus_key(key(1)));

        for expected in [3, 2] {
            layout.apply(Command::Focus(Direction::Down));
            assert_eq!(layout.focused(), Some(key(expected)), "down to {expected}");
        }
        layout.check_invariants().unwrap();
    }

    #[test]
    fn directional_focus_covers_all_four_directions_and_edges() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);

        for (direction, expected) in [(Direction::Up, 2), (Direction::Left, 1)] {
            layout.apply(Command::Focus(direction));
            assert_eq!(layout.focused(), Some(key(expected)), "{direction:?}");
        }
        for direction in [Direction::Up, Direction::Down] {
            layout.apply(Command::Focus(direction));
            assert_eq!(layout.focused(), Some(key(1)), "{direction:?}");
        }
        layout.apply(Command::Focus(Direction::Right));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn directional_move_covers_all_directions_and_keeps_focus_on_the_leaf() {
        // One row holding a window and a column: `H[1, V[2, 3]]`. Each
        // direction reaches a different arm, and what is asserted is the
        // TREE's order rather than a rectangle, since a rectangle is what a
        // swap and a move can agree on while the arrangement differs.
        // The WIDTHS go with the order, because `Right`'s order is the one
        // the tree already had: `H[1, V[2, 3]]` places `[1, 2, 3]` before the
        // move as well as after, so order alone cannot tell "entered the
        // column" from "did nothing". Entering makes it one column.
        for (direction, prepare, order, widths) in [
            // 3 leaves the column on its left: a sibling of it, before it.
            (Direction::Left, None, [1, 3, 2], [34, 33, 33]),
            // 1 enters the column beside it, at its top.
            (
                Direction::Right,
                Some(Direction::Left),
                [1, 2, 3],
                [100, 100, 100],
            ),
            // 3 has a neighbour inside its own column, so the two trade.
            (Direction::Up, None, [1, 3, 2], [50, 50, 50]),
            (
                Direction::Down,
                Some(Direction::Up),
                [1, 3, 2],
                [50, 50, 50],
            ),
        ] {
            let mut layout = Layout::new();
            layout.map(key(1));
            layout.map(key(2));
            map_below(&mut layout, 3);
            if let Some(direction) = prepare {
                layout.apply(Command::Focus(direction));
            }
            let focused = layout.focused().unwrap();
            layout.apply(Command::Move(direction));
            assert_eq!(layout.focused(), Some(focused), "{direction:?}");
            let placements = layout.placements(100, 100, 0, 0);
            assert_eq!(
                placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
                order,
                "{direction:?}"
            );
            assert_eq!(
                placements.iter().map(|p| p.rect.width).collect::<Vec<_>>(),
                widths,
                "{direction:?}"
            );
            layout.check_invariants().unwrap();
        }
    }

    #[test]
    fn a_move_reparents_where_a_swap_could_only_have_traded_two_keys() {
        // `H[1, V[2, 3]]`: 1 and 3 are in different containers, and moving 1
        // right puts it INSIDE the column — an arrangement no exchange of two
        // keys can reach, since a swap leaves the tree's shape untouched.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        assert!(layout.focus_key(key(1)));

        let before = layout.placements(100, 100, 0, 0);
        assert_eq!(before.len(), 3);
        // Two columns: one full-height window beside a stack of two.
        assert_eq!(before.first().unwrap().rect.height, 100);

        layout.apply(Command::Move(Direction::Right));
        let after = layout.placements(100, 100, 0, 0);
        // One column of three, each a third of the height and the full width.
        assert_eq!(
            after.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(after.iter().all(|p| p.rect.width == 100));
        assert_eq!(
            after.iter().map(|p| p.rect.height).collect::<Vec<_>>(),
            [34, 33, 33]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_puts_a_window_beside_the_one_it_was_dropped_on() {
        // `H[1, V[2, 3]]`: 1 is dropped onto 3's TOP half, so it lands in the
        // column above 3 — the arrangement the keyboard reaches only as a
        // sequence, and the whole point of dropping on a window rather than
        // stepping in a direction.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);

        assert!(layout.drop_onto(key(1), key(3), beside(Axis::Vertical, true)));
        assert_eq!(
            layout
                .placements(100, 100, 0, 0)
                .iter()
                .map(|p| p.key.object)
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
        // The dropped window is the focused one: it is what was acted on.
        assert_eq!(layout.focused(), Some(key(1)));
        layout.check_invariants().unwrap();

        // And the other half puts it below: 1 is above 3 now, so dropping it
        // BELOW 3 has somewhere to go and the order changes again.
        assert!(layout.drop_onto(key(1), key(3), beside(Axis::Vertical, false)));
        assert_eq!(
            layout
                .placements(100, 100, 0, 0)
                .iter()
                .map(|p| p.key.object)
                .collect::<Vec<_>>(),
            [2, 3, 1]
        );
        assert_eq!(layout.parent_axis(key(1)), Some(Axis::Vertical));

        // Dropping it back exactly where it already is CHANGES nothing, and
        // says so: the answer is about the arrangement, not about the call.
        let settled = layout.placements(100, 100, 0, 0);
        assert!(!layout.drop_onto(key(1), key(3), beside(Axis::Vertical, false)));
        assert_eq!(layout.placements(100, 100, 0, 0), settled);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_onto_the_last_other_window_rebuilds_the_container_it_shared() {
        // Two windows in a ROW, and one dropped on the other. Removing the
        // dragged one collapses the row, so the pair has to be rebuilt — on
        // the axis they were arranged on, or a drag silently turns a row into
        // a column without the operator asking for one.
        // `before` is also whether this moves at all: 2 is already after 1,
        // so dropping it after 1 is the no-op the contract reports as false.
        for (before, order) in [(true, [2, 1]), (false, [1, 2])] {
            let mut layout = Layout::new();
            layout.map(key(1));
            layout.map(key(2));
            assert_eq!(layout.parent_axis(key(1)), Some(Axis::Horizontal));

            assert_eq!(
                layout.drop_onto(key(2), key(1), beside(Axis::Horizontal, before)),
                before
            );
            let placements = layout.placements(100, 100, 0, 0);
            assert_eq!(
                placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
                order,
                "{before}"
            );
            // Still a row, each half the width.
            assert!(placements.iter().all(|p| p.rect.width == 50), "{before}");
            layout.check_invariants().unwrap();
        }
    }

    #[test]
    fn a_band_drop_keeps_the_container_it_lands_in_whatever_that_is() {
        // `InRun` names no axis, and that is the point. The aim geometry has
        // the dragged window taken OUT, so a container of two collapses there
        // and can answer nothing at all — an axis read from it would default
        // to something and turn this column into a row.
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let mut layout = Layout::new();
            layout.map(key(1));
            match axis {
                Axis::Horizontal => layout.map(key(2)),
                Axis::Vertical => map_below(&mut layout, 2),
            }
            assert_eq!(layout.parent_axis(key(1)), Some(axis), "{axis:?}");

            assert!(layout.drop_onto(key(1), key(2), DropKind::InRun { before: false }));
            assert_eq!(
                layout
                    .placements(100, 100, 0, 0)
                    .iter()
                    .map(|p| p.key.object)
                    .collect::<Vec<_>>(),
                [2, 1],
                "{axis:?}"
            );
            assert_eq!(
                layout.parent_axis(key(1)),
                Some(axis),
                "the drop changed the container's axis: {axis:?}"
            );
            layout.check_invariants().unwrap();
        }

        // And a NESTED container answers for itself rather than for the
        // workspace: `H[1, V[2, 3]]`, where a band drop onto 3 is a reorder
        // of the column and never a third tile in the row.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        assert!(layout.drop_onto(key(2), key(3), DropKind::InRun { before: false }));
        let placements = layout.placements(100, 100, 0, 0);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 2]
        );
        assert_eq!(
            placements.iter().map(|p| p.rect.width).collect::<Vec<_>>(),
            [50, 50, 50],
            "the column became part of the row"
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_on_the_middle_trades_two_windows_where_they_stand() {
        // `H[1, V[2, 3]]`, and 1 is swapped with 3 — one in the row and one
        // in the column, so a swap that went through detach-and-reinsert
        // would have to destroy a container to do it. Both keep the place
        // the OTHER had, and the column is still a column.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        let before = layout.placements(100, 100, 0, 0);
        assert_eq!(
            before.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 2, 3]
        );

        assert!(layout.drop_onto(key(1), key(3), DropKind::Swap));
        let after = layout.placements(100, 100, 0, 0);
        assert_eq!(
            after.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [3, 2, 1]
        );
        // The GEOMETRY is untouched — only which window sits in each slot.
        assert_eq!(
            after.iter().map(|p| p.rect).collect::<Vec<_>>(),
            before.iter().map(|p| p.rect).collect::<Vec<_>>()
        );
        assert_eq!(layout.parent_axis(key(1)), Some(Axis::Vertical));
        assert_eq!(layout.parent_axis(key(3)), Some(Axis::Horizontal));
        // The window that was dragged is the focused one, as for every drop.
        assert_eq!(layout.focused(), Some(key(1)));
        layout.check_invariants().unwrap();

        // And it is its own inverse: swapping the pair back puts every window
        // in the slot it started in. FOCUS is not part of that — each drop
        // focuses what was dragged, so two swaps leave it on the window that
        // moved rather than on whatever held it before.
        assert!(layout.drop_onto(key(1), key(3), DropKind::Swap));
        let back = layout.placements(100, 100, 0, 0);
        assert_eq!(
            back.iter().map(|p| (p.key, p.rect)).collect::<Vec<_>>(),
            before.iter().map(|p| (p.key, p.rect)).collect::<Vec<_>>()
        );
        assert_eq!(layout.focused(), Some(key(1)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_swap_into_a_stack_keeps_it_stacked_and_shows_the_arrival() {
        // The container a swap lands in keeps its presentation, which is the
        // reason a swap is done in place: a detach takes the container's
        // `stacked` bit with it whenever the removal empties it.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        layout.apply(Command::ToggleGrouped);
        assert_eq!(
            layout
                .placements(100, 300, 0, 20)
                .iter()
                .map(|p| p.key.object)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(layout
            .placements(100, 300, 0, 20)
            .iter()
            .filter(|p| p.key.object != 1)
            .all(|p| p.run.is_some()));

        assert!(layout.drop_onto(key(1), key(3), DropKind::Swap));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert!(
            placements
                .iter()
                .filter(|p| p.key.object != 3)
                .all(|p| p.run.is_some()),
            "the swap unstacked the container it landed in"
        );
        // Focused, so the stack shows the window that just arrived rather
        // than leaving the operator looking at the one it replaced.
        let shown = placements
            .iter()
            .filter(|p| p.run.is_some() && p.visible)
            .map(|p| p.key.object)
            .collect::<Vec<_>>();
        assert_eq!(shown, [1]);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_swap_refuses_itself_and_a_stranger() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        let before = layout.placements(100, 100, 0, 0);
        assert!(!layout.drop_onto(key(1), key(1), DropKind::Swap));
        assert!(!layout.drop_onto(key(1), key(9), DropKind::Swap));
        assert!(!layout.drop_onto(key(9), key(1), DropKind::Swap));
        layout.apply(Command::ToggleFullscreen);
        assert!(!layout.drop_onto(key(2), key(1), DropKind::Swap));
        layout.apply(Command::ToggleFullscreen);
        assert_eq!(layout.placements(100, 100, 0, 0), before);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_across_the_axis_makes_the_container_it_needs() {
        // `H[1, 2, 3]` and 1 is dropped BELOW 2. Nothing in the tree runs
        // vertically where 2 sits — its container is the row — so the drop
        // has to make the column it is asking for, in 2's place, rather than
        // fall back to the row's own axis and land beside it. This is the
        // whole of what the top and bottom zones buy: an arrangement the
        // two-zone drop could not reach at all.
        let mut layout = Layout::new();
        for object in 1..=3 {
            layout.map(key(object));
        }
        assert_eq!(
            layout
                .placements(100, 100, 0, 0)
                .iter()
                .map(|p| p.key.object)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(layout.parent_axis(key(2)), Some(Axis::Horizontal));

        assert!(layout.drop_onto(key(1), key(2), beside(Axis::Vertical, false)));
        assert_eq!(
            layout
                .placements(100, 100, 0, 0)
                .iter()
                .map(|p| p.key.object)
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
        assert_eq!(layout.parent_axis(key(1)), Some(Axis::Vertical));
        assert_eq!(layout.parent_axis(key(2)), Some(Axis::Vertical));
        // The pair share a column of their own, so they are one above the
        // other and NOT the full width of what the row gave them.
        let placements = layout.placements(100, 100, 0, 0);
        let of = |object: u32| {
            let at = placements
                .iter()
                .position(|p| p.key.object == object)
                .unwrap();
            *placements.get(at).unwrap()
        };
        assert_eq!(of(1).rect.x, of(2).rect.x);
        assert_eq!(of(1).rect.width, of(2).rect.width);
        assert_ne!(of(1).rect.y, of(2).rect.y);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_between_two_siblings_leaves_the_container_they_share_standing() {
        // `H[1, V[2, 3]]` and 2 is dropped below 3 — both already in the same
        // column, so only their ORDER should change. Taking 2 out with a
        // collapsing removal would reduce the column to `Leaf(3)`, match 3 in
        // the ROW instead, and flatten the whole thing to `H[1, 2, 3]`.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);

        assert!(layout.drop_onto(key(2), key(3), beside(Axis::Vertical, false)));
        let placements = layout.placements(100, 100, 0, 0);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 2]
        );
        // Still a window beside a column, not three across.
        assert_eq!(
            placements.iter().map(|p| p.rect.width).collect::<Vec<_>>(),
            [50, 50, 50]
        );
        assert_eq!(layout.parent_axis(key(2)), Some(Axis::Vertical));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn reordering_a_two_window_stack_by_dropping_keeps_it_stacked() {
        // The pair's container is the one the target sits in, so a collapsing
        // removal destroys it and its presentation with it — and the operator
        // asked to reorder a stack, not to unstack one.
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2]);
        layout.apply(Command::ToggleGrouped);
        assert!(layout
            .placements(100, 300, 0, 20)
            .iter()
            .all(|p| p.run.is_some()));

        // A drop onto the stack's own axis, which is what an edge of a tile
        // inside one answers.
        assert!(layout.drop_onto(key(2), key(1), beside(Axis::Vertical, true)));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(
            placements.iter().all(|p| p.run.is_some()),
            "reordering a stack unstacked it"
        );
        assert_eq!(
            placements.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20]
        );
        layout.check_invariants().unwrap();

        // And the gesture this is named for, which names no axis at all: a
        // drop onto a title BAND. The two reach the same container by
        // different routes, so both are asked here.
        assert!(layout.drop_onto(key(1), key(2), DropKind::InRun { before: true }));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(
            placements.iter().all(|p| p.run.is_some()),
            "a band drop unstacked the pair"
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_cross_axis_drop_into_a_stack_joins_its_run_rather_than_splitting_it() {
        // A stack draws one band per LEAF beneath it, so a split among its
        // children is a container it never shows: the operator would see the
        // leaf join the run either way, and meet a row waiting for them when
        // they later unstacked. The axis is refused inside a stack instead.
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::ToggleGrouped);
        assert!(layout
            .placements(100, 300, 0, 20)
            .iter()
            .all(|p| p.run.is_some()));

        // "1 to the RIGHT of 2", across a column that presents as a list.
        assert!(layout.drop_onto(key(1), key(2), beside(Axis::Horizontal, false)));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [2, 1, 3]
        );
        assert!(
            placements.iter().all(|p| p.run.is_some()),
            "the drop unstacked the run"
        );
        assert_eq!(
            layout.parent_axis(key(1)),
            Some(Axis::Vertical),
            "the drop built a row inside the stack"
        );
        assert_eq!(
            placements.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20, 40],
            "the run is not three bands"
        );
        layout.check_invariants().unwrap();

        // The same drop with the stack UNDONE does build the row, which is
        // what makes the refusal above about the presentation rather than
        // about the axis being ignored everywhere.
        layout.apply(Command::ToggleGrouped);
        assert!(layout.drop_onto(key(1), key(3), beside(Axis::Horizontal, false)));
        assert_eq!(
            layout.parent_axis(key(1)),
            Some(Axis::Horizontal),
            "an unstacked column refused the axis too"
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stack_refuses_the_axis_for_a_leaf_below_a_split_too() {
        // The refusal is about the outermost grouped ANCESTOR, not about the
        // container the leaf sits in. A group already holding a split shows
        // the leaves under it flattened all the same, so a second split one
        // level further down would be exactly as invisible as the first.
        // `V{stacked}[1, 2, H[3, 4]]`, built the way one is now reached: the
        // split is made while the column is SEPARATE, since a drop into a
        // group joins its run whatever axis it names, and the column is
        // grouped afterwards from a leaf that is still a direct child of it.
        let shape = || {
            let mut layout = Layout::new();
            column(&mut layout, &[1, 2, 3]);
            layout.map(key(4));
            assert!(layout.drop_onto(key(4), key(3), beside(Axis::Horizontal, false)));
            assert!(layout.focus_key(key(1)));
            layout.apply(Command::ToggleGrouped);
            layout
        };
        let mut layout = shape();
        assert_eq!(layout.parent_axis(key(4)), Some(Axis::Horizontal));
        assert!(layout
            .placements(100, 400, 0, 20)
            .iter()
            .all(|p| p.run.is_some()));

        assert!(layout.drop_onto(key(1), key(3), beside(Axis::Vertical, true)));
        assert_eq!(
            layout.parent_axis(key(1)),
            Some(Axis::Horizontal),
            "the drop built a column below the stack's split child"
        );
        let placements = layout.placements(100, 400, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [2, 1, 3, 4]
        );
        assert!(
            placements.iter().all(|p| p.run.is_some()),
            "the drop unstacked the run"
        );
        layout.check_invariants().unwrap();

        // The band drop into the same shape, which names no axis to refuse,
        // reaches the same position in the run. It lands INSIDE that row
        // rather than among the stack's own children — the row's doing
        // rather than the drop's, and harmless to what the operator sees,
        // since a split's leaves are contiguous in the run either way.
        let mut layout = shape();
        assert!(layout.drop_onto(key(1), key(3), DropKind::InRun { before: true }));
        let placements = layout.placements(100, 400, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [2, 1, 3, 4]
        );
        assert!(placements.iter().all(|p| p.run.is_some()));
        assert_eq!(layout.parent_axis(key(1)), Some(Axis::Horizontal));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_refuses_itself_a_stranger_and_a_fullscreen_workspace() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        let before = layout.placements(100, 100, 0, 0);

        // Onto itself: the pointer never left the window it picked up.
        assert!(!layout.drop_onto(key(1), key(1), beside(Axis::Horizontal, true)));
        // A window that does not exist at all.
        assert!(!layout.drop_onto(key(1), key(9), beside(Axis::Horizontal, true)));
        assert!(!layout.drop_onto(key(9), key(1), beside(Axis::Horizontal, true)));
        assert_eq!(layout.placements(100, 100, 0, 0), before);

        // And one that exists on ANOTHER workspace, which is on no screen the
        // pointer can reach — mapped for real, since a key nobody mapped
        // would be refused by the first check and prove nothing about this.
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(3));
        layout.apply(Command::SwitchWorkspace(1));
        assert!(!layout.drop_onto(key(1), key(3), beside(Axis::Horizontal, true)));
        assert!(!layout.drop_onto(key(3), key(1), beside(Axis::Horizontal, true)));
        assert_eq!(layout.placements(100, 100, 0, 0), before);

        // Under fullscreen, as every other rearranging command is refused.
        layout.apply(Command::ToggleFullscreen);
        assert!(!layout.drop_onto(key(2), key(1), beside(Axis::Horizontal, true)));
        layout.apply(Command::ToggleFullscreen);
        assert_eq!(layout.placements(100, 100, 0, 0), before);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_into_a_stack_joins_the_run_and_parent_axis_answers_per_container() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        assert!(layout.focus_key(key(1)));
        map_below(&mut layout, 3);
        layout.apply(Command::ToggleGrouped);

        // The axis is the container's own, not the workspace's: 2 sits in the
        // row and 1 in the stacked column, so a drop measures a different
        // half for each.
        assert_eq!(layout.parent_axis(key(2)), Some(Axis::Horizontal));
        assert_eq!(layout.parent_axis(key(1)), Some(Axis::Vertical));

        assert!(layout.drop_onto(key(2), key(3), beside(Axis::Vertical, false)));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 2]
        );
        assert!(placements.iter().all(|p| p.run.is_some()));
        // The arrival is focused, so it is the leaf the stack shows.
        assert_eq!(
            placements.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [false, false, true]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_lone_window_has_no_container_and_so_no_parent_axis() {
        let mut layout = Layout::new();
        layout.map(key(1));
        assert_eq!(layout.parent_axis(key(1)), None);
        assert_eq!(layout.parent_axis(key(2)), None);
    }

    #[test]
    fn a_leaf_entering_a_container_lands_at_the_end_it_came_in_by() {
        // `H[V[1, 3], 2]` and 2 moves LEFT into the column. Entering from the
        // right lands LAST — at the column's bottom — which is the half of
        // that rule no other test reaches, since every entry elsewhere goes
        // rightward and lands first.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        assert!(layout.focus_key(key(1)));
        map_below(&mut layout, 3);
        assert!(layout.focus_key(key(2)));

        layout.apply(Command::Move(Direction::Left));
        let placements = layout.placements(100, 100, 0, 0);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 2]
        );
        // One column: 2 came in at the bottom rather than the top.
        assert!(placements.iter().all(|p| p.rect.width == 100));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_move_into_and_out_of_a_stack_carries_the_presentation_correctly() {
        // Entering a stacked column makes the arriving window one of its
        // leaves, and the one SHOWN, because it is the focused one.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        assert!(layout.focus_key(key(1)));
        map_below(&mut layout, 3);
        layout.map(key(4));
        layout.apply(Command::ToggleGrouped);
        assert!(layout.focus_key(key(2)));

        layout.apply(Command::Move(Direction::Left));
        let entered = layout.placements(100, 300, 0, 20);
        assert_eq!(
            entered.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
        assert!(entered.iter().all(|p| p.run.is_some()));
        assert_eq!(
            entered.iter().map(|p| p.visible).collect::<Vec<_>>(),
            [false, false, false, true]
        );

        // And leaving one leaves the remainder stacked.
        layout.apply(Command::Move(Direction::Right));
        let left = layout.placements(100, 300, 0, 20);
        assert_eq!(
            left.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
        assert_eq!(
            left.iter().map(|p| p.run.is_some()).collect::<Vec<_>>(),
            [true, true, true, false]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_container_a_move_empties_to_one_child_loses_its_stacking_with_it() {
        // `Hstacked[1, V[2, 3]]`: moving 1 into the column leaves the row
        // holding a single child, so the row is gone and its presentation
        // with it. `remove_node` has that rule tested; the move's own
        // collapse is a second copy of it and needs its own.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);
        assert!(layout.focus_key(key(1)));
        layout.apply(Command::ToggleGrouped);
        assert!(layout
            .placements(100, 300, 0, 20)
            .iter()
            .all(|p| p.run.is_some()));

        layout.apply(Command::Move(Direction::Right));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(
            placements.iter().all(|p| p.run.is_none() && p.visible),
            "the collapsed row left its stacking behind"
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_leaf_leaving_its_container_lands_on_the_side_it_was_heading() {
        // `H[V[1, 3], 2]`, and 1 leaves the column either way. Which SIDE of
        // the column it lands on is the only thing the direction still
        // decides once the leaf is out, since both are a sibling of it.
        for (direction, order) in [(Direction::Right, [3, 1, 2]), (Direction::Left, [1, 3, 2])] {
            let mut layout = Layout::new();
            layout.map(key(1));
            layout.map(key(2));
            assert!(layout.focus_key(key(1)));
            map_below(&mut layout, 3);
            assert!(layout.focus_key(key(1)));

            layout.apply(Command::Move(direction));
            assert_eq!(
                layout
                    .placements(100, 100, 0, 0)
                    .iter()
                    .map(|p| p.key.object)
                    .collect::<Vec<_>>(),
                order,
                "{direction:?}"
            );
            layout.check_invariants().unwrap();
        }
    }

    #[test]
    fn a_move_across_the_grain_wraps_the_workspace_in_a_container_that_runs_that_way() {
        // Two windows side by side is the commonest arrangement there is, and
        // no ancestor of either runs vertically. i3 makes one rather than
        // letting the chord do nothing; so does this.
        //
        // BOTH directions, each from a FRESH row, because the wrap changes
        // the workspace's own axis: moving back down afterwards is a
        // neighbour trade inside the column the first move built, not a
        // second wrap, and proves nothing about which side a wrap picks.
        for (direction, order) in [(Direction::Up, [2, 1]), (Direction::Down, [1, 2])] {
            let mut layout = Layout::new();
            layout.map(key(1));
            layout.map(key(2));
            assert_eq!(layout.focused(), Some(key(2)));

            layout.apply(Command::Move(direction));
            let wrapped = layout.placements(100, 100, 0, 0);
            assert_eq!(
                wrapped.iter().map(|p| p.key.object).collect::<Vec<_>>(),
                order,
                "{direction:?}"
            );
            // One above the other now, not side by side.
            assert!(wrapped.iter().all(|p| p.rect.width == 100), "{direction:?}");
            layout.check_invariants().unwrap();
        }
    }

    #[test]
    fn a_lone_window_and_an_edge_inside_a_row_have_nowhere_to_move() {
        let mut layout = Layout::new();
        layout.map(key(1));
        let alone = layout.placements(100, 100, 0, 0);
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            layout.apply(Command::Move(direction));
            assert_eq!(layout.placements(100, 100, 0, 0), alone, "{direction:?}");
        }

        // At the left edge of the only row there is. THREE windows, not two:
        // with two, removing one and wrapping the survivor rebuilds the row
        // it started as, so the wrong answer and the right one agree and the
        // assertion is about nothing. With three, wrapping would nest a row
        // inside a row — `H[1, H[2, 3]]` — and turn thirds into a half and
        // two quarters.
        layout.map(key(2));
        layout.map(key(3));
        assert!(layout.focus_key(key(1)));
        let row = layout.placements(100, 100, 0, 0);
        assert_eq!(
            row.iter().map(|p| p.rect.width).collect::<Vec<_>>(),
            [34, 33, 33]
        );
        layout.apply(Command::Move(Direction::Left));
        assert_eq!(layout.placements(100, 100, 0, 0), row);
        layout.check_invariants().unwrap();

        // And the far edge the same way.
        assert!(layout.focus_key(key(3)));
        let row = layout.placements(100, 100, 0, 0);
        layout.apply(Command::Move(Direction::Right));
        assert_eq!(layout.placements(100, 100, 0, 0), row);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stacked_workspace_edge_keeps_its_presentation_when_a_move_has_nowhere_to_go() {
        // The same edge, on a STACKED root. Wrapping would have rebuilt it as
        // an unstacked container around a stacked one, so the run the operator
        // is looking at would come apart under a chord that does nothing.
        let mut layout = Layout::new();
        column(&mut layout, &[1, 2, 3]);
        layout.apply(Command::ToggleGrouped);
        assert!(layout.focus_key(key(1)));

        let stack = layout.placements(100, 300, 0, 20);
        assert!(stack.iter().all(|p| p.run.is_some()));
        layout.apply(Command::Move(Direction::Up));
        assert_eq!(layout.placements(100, 300, 0, 20), stack);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn workspace_switch_and_move_keep_independent_focus() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::MoveToWorkspace(2));
        assert_eq!(layout.active_workspace(), 1);
        assert_eq!(layout.focused(), Some(key(1)));
        assert_eq!(layout.placements(100, 100, 0, 0).len(), 1);

        layout.apply(Command::SwitchWorkspace(2));
        assert_eq!(layout.focused(), Some(key(2)));
        assert_eq!(layout.placements(100, 100, 0, 0).len(), 1);
        layout.apply(Command::MoveToWorkspace(2));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::SwitchWorkspace(3));
        assert_eq!(layout.focused(), None);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn view_layouts_distinguish_visible_hidden_active_and_fullscreen() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::MoveToWorkspace(2));
        assert_eq!(
            layout.views(100, 80, 0, 0),
            [
                ViewLayout {
                    key: key(1),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 80,
                    },
                    visible: true,
                    activated: true,
                    fullscreen: false,
                },
                ViewLayout {
                    key: key(2),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 80,
                    },
                    visible: false,
                    activated: false,
                    fullscreen: false,
                },
            ]
        );
        layout.apply(Command::SwitchWorkspace(2));
        layout.apply(Command::ToggleFullscreen);
        assert_eq!(
            layout.views(100, 80, 0, 0),
            [
                ViewLayout {
                    key: key(1),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 80,
                    },
                    visible: false,
                    activated: false,
                    fullscreen: false,
                },
                ViewLayout {
                    key: key(2),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 80,
                    },
                    visible: true,
                    activated: true,
                    fullscreen: true,
                },
            ]
        );
    }

    #[test]
    fn moving_to_a_populated_workspace_inserts_after_its_focus() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(3));
        layout.apply(Command::SwitchWorkspace(1));
        layout.apply(Command::MoveToWorkspace(2));
        assert_eq!(layout.focused(), Some(key(1)));
        layout.apply(Command::SwitchWorkspace(2));
        assert_eq!(layout.focused(), Some(key(2)));
        assert_eq!(
            layout
                .placements(100, 100, 0, 0)
                .into_iter()
                .map(|placement| placement.key)
                .collect::<Vec<_>>(),
            [key(3), key(2)]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn transient_unmap_remembers_workspace_but_forget_drops_that_assignment() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::MoveToWorkspace(3));
        layout.unmap(key(1));
        layout.map(key(1));
        assert!(layout.placements(100, 100, 0, 0).is_empty());
        layout.apply(Command::SwitchWorkspace(3));
        assert_eq!(layout.focused(), Some(key(1)));

        layout.forget(key(1));
        layout.apply(Command::SwitchWorkspace(1));
        layout.map(key(1));
        assert_eq!(layout.focused(), Some(key(1)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn commands_outside_the_nine_workspaces_are_noops() {
        let mut layout = Layout::new();
        layout.map(key(1));
        let before = layout.clone();
        for command in [
            Command::SwitchWorkspace(0),
            Command::SwitchWorkspace(10),
            Command::MoveToWorkspace(0),
            Command::MoveToWorkspace(10),
        ] {
            layout.apply(command);
            assert_eq!(layout, before);
        }
        layout.check_invariants().unwrap();
    }

    #[test]
    fn fullscreen_is_workspace_local_and_blocks_focus_and_move() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleFullscreen);
        // Band 20, not 0: this is the ONE literal in the module that is about
        // the band, so asking for none would make its zero height the
        // argument's answer rather than the arrangement's — true of every arm
        // and therefore about no arm.
        assert_eq!(
            layout.placements(80, 60, 9, 20),
            [Placement {
                key: key(2),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 60
                },
                // The fullscreen arrangement carries no band: a window with
                // one across the top of it is not fullscreen.
                band: band_at(0, 0, 80),
                focused: true,
                run: None,
                presented: Presentation::Split,
                visible: true
            }]
        );
        layout.apply(Command::Focus(Direction::Left));
        layout.apply(Command::Move(Direction::Left));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(3));
        assert_eq!(layout.placements(80, 60, 9, 0).len(), 1);
        layout.apply(Command::SwitchWorkspace(1));
        assert_eq!(layout.placements(80, 60, 9, 0).len(), 1);
        layout.apply(Command::ToggleFullscreen);
        assert_eq!(layout.placements(80, 60, 9, 0).len(), 2);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn mapping_a_new_surface_leaves_fullscreen_and_focuses_the_new_leaf() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::ToggleFullscreen);
        layout.map(key(2));
        assert_eq!(layout.focused(), Some(key(2)));
        assert_eq!(layout.placements(80, 60, 0, 0).len(), 2);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn placing_a_parented_surface_leaves_fullscreen_and_reveals_its_focus() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::ToggleFullscreen);

        // A child first mapped with a parent exits the parent's fullscreen
        // projection rather than taking keyboard focus while hidden.
        assert!(layout.place_after(key(2), key(1)));
        assert_eq!(layout.focused(), Some(key(2)));
        let views = layout.views(80, 60, 0, 0);
        assert!(views.iter().all(|view| view.visible && !view.fullscreen));
        assert_eq!(
            views
                .iter()
                .filter(|view| view.activated)
                .map(|view| view.key)
                .collect::<Vec<_>>(),
            vec![key(2)]
        );
        layout.check_invariants().unwrap();

        // Setting the relationship after both surfaces are mapped follows the
        // same policy, including when their order already happens to match.
        assert!(layout.focus_key(key(1)));
        layout.apply(Command::ToggleFullscreen);
        assert!(layout.place_after(key(2), key(1)));
        let views = layout.views(80, 60, 0, 0);
        assert!(views.iter().all(|view| view.visible && !view.fullscreen));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn unmap_collapses_containers_and_selects_next_then_previous() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        map_below(&mut layout, 3);

        layout.unmap(key(2));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.unmap(key(3));
        assert_eq!(layout.focused(), Some(key(1)));
        layout.unmap(key(1));
        assert_eq!(layout.focused(), None);
        assert!(layout.placements(100, 100, 0, 0).is_empty());
        layout.check_invariants().unwrap();
    }

    #[test]
    fn unmapping_a_nonfocused_leaf_preserves_focus_and_focused_unmap_exits_fullscreen() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.unmap(key(1));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::ToggleFullscreen);
        layout.unmap(key(2));
        assert_eq!(layout.focused(), Some(key(3)));
        assert_eq!(layout.placements(100, 100, 0, 0).len(), 1);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn duplicate_maps_and_missing_unmaps_are_noops() {
        let mut layout = Layout::new();
        layout.map(key(1));
        let before = layout.clone();
        layout.map(key(1));
        layout.unmap(key(99));
        assert_eq!(layout, before);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn client_unmap_spans_workspaces_without_touching_other_clients() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(2));
        let other = SurfaceKey {
            client: 2,
            object: 1,
        };
        layout.map(other);
        layout.unmap_client(1);
        assert!(!layout.contains(key(1)));
        assert!(!layout.contains(key(2)));
        assert!(layout.contains(other));
        assert_eq!(layout.focused(), Some(other));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn client_unmap_forgets_dormant_workspace_assignments() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::MoveToWorkspace(2));
        layout.unmap(key(1));
        layout.unmap_client(1);
        layout.map(key(1));
        assert_eq!(layout.focused(), Some(key(1)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn split_geometry_conserves_odd_extents_and_bounds_every_tile() {
        let rects = split_rects(
            Rect {
                x: 5,
                y: 7,
                width: 11,
                height: 9,
            },
            Axis::Horizontal,
            3,
            2,
        );
        assert_eq!(
            rects,
            [
                Rect {
                    x: 5,
                    y: 7,
                    width: 3,
                    height: 9
                },
                Rect {
                    x: 10,
                    y: 7,
                    width: 2,
                    height: 9
                },
                Rect {
                    x: 14,
                    y: 7,
                    width: 2,
                    height: 9
                }
            ]
        );
        assert!(split_rects(rects.first().copied().unwrap(), Axis::Vertical, 0, 2).is_empty());
        let tiny = split_rects(
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            Axis::Vertical,
            3,
            9,
        );
        assert_eq!(tiny.iter().map(|rect| rect.height).sum::<usize>(), 1);
        let narrow = split_rects(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            Axis::Horizontal,
            3,
            24,
        );
        assert_eq!(
            narrow.iter().map(|rect| rect.width).collect::<Vec<_>>(),
            [1, 1, 0]
        );
        assert_eq!(narrow.iter().map(|rect| rect.width).sum::<usize>(), 2);
    }

    #[test]
    fn a_lone_tile_keeps_a_pixel_and_the_window_below_it_halves_the_output() {
        // On a workspace of its own, so the map path is exercised from empty
        // rather than from whatever another test left.
        let mut layout = Layout::new();
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(1));
        assert_eq!(
            layout.placements(1, 1, 24, 0),
            [Placement {
                key: key(1),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                },
                band: band_at(0, 0, 1),
                focused: true,
                run: None,
                presented: Presentation::Split,
                visible: true
            }]
        );
        map_below(&mut layout, 2);
        assert_eq!(rect(&layout, 1).y, 0);
        assert_eq!(rect(&layout, 2).y, 50);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn command_sequence_preserves_all_model_invariants() {
        let mut layout = Layout::new();
        for object in 1..=4 {
            layout.map(key(object));
            layout.check_invariants().unwrap();
        }
        for command in [
            Command::Focus(Direction::Left),
            Command::Focus(Direction::Right),
            Command::Focus(Direction::Up),
            Command::Focus(Direction::Down),
            Command::Move(Direction::Left),
            Command::Move(Direction::Right),
            Command::Move(Direction::Up),
            Command::Move(Direction::Down),
            Command::SetPresentation(Presentation::Stacked),
            Command::SetPresentation(Presentation::Tabbed),
            Command::SetPresentation(Presentation::Split),
            Command::ToggleGrouped,
            Command::ToggleGrouped,
            Command::ToggleFullscreen,
            Command::ToggleFullscreen,
            Command::MoveToWorkspace(9),
            Command::SwitchWorkspace(9),
            Command::SwitchWorkspace(1),
        ] {
            layout.apply(command);
            layout.check_invariants().unwrap();
        }
    }
}
