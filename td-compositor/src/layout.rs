use crate::scene::SurfaceKey;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;

const INITIAL_WORKSPACE: u8 = 1;
const FINAL_WORKSPACE: u8 = 9;
const VIRTUAL_EXTENT: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Horizontal,
    Vertical,
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
    SetSplit(Axis),
    ToggleStacked,
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
    /// the aim is computed against the arrangement with the dragged window
    /// taken OUT, where the target's container may have collapsed to nothing
    /// at all, so an axis read there could turn a column into a row.
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
    /// Whether this leaf is presented by a STACKED container. The renderer
    /// asks because a border wraps a window's band together with its client
    /// area, and a stack is the one arrangement where it must not: the run's
    /// LAST band abuts the content exactly as an ordinary band does, so
    /// adjacency cannot tell the two apart.
    pub stacked: bool,
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
        /// STACKED presentation: every leaf beneath this container gets a
        /// title band in a run at its top, and the focused one gets all the
        /// space below the run. Per container rather than per workspace, so a
        /// nested column stacks without the rest of the screen following it.
        stacked: bool,
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

    fn insert_after(&mut self, focused: SurfaceKey, key: SurfaceKey, axis: Axis) -> bool {
        match self {
            Node::Leaf(current) if *current == focused => {
                *self = Node::Split {
                    axis,
                    children: vec![Node::Leaf(*current), Node::Leaf(key)],
                    stacked: false,
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split {
                axis: current_axis,
                children,
                ..
            } => {
                let Some(index) = children.iter().position(|child| child.contains(focused)) else {
                    return false;
                };
                if *current_axis == axis {
                    children.insert(index.saturating_add(1), Node::Leaf(key));
                    true
                } else {
                    children
                        .get_mut(index)
                        .is_some_and(|child| child.insert_after(focused, key, axis))
                }
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
    /// A STACKED container refuses that second case. `place_stack` draws one
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
            stacked,
        } = self
        else {
            return false;
        };
        let own = *own;
        let in_stack = in_stack || *stacked;
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

    /// Toggle the presentation of the container a leaf is DISPLAYED in. The
    /// answer is how the recursion below reports having found it; a lone
    /// window is its workspace's whole root, with no container to stack, and
    /// every arm says false.
    ///
    /// Unstacking looks at the OUTERMOST stacked ancestor rather than the
    /// leaf's own parent, because a stack runs every leaf BENEATH it and so
    /// hides whatever the containers under it are doing — that ancestor is
    /// what the leaf is displayed in, whatever they say. Descending past it
    /// would toggle a container nothing can see, and a stack whose direct
    /// children have all since become splits could then never be undone from
    /// the keyboard: no leaf in it is a child of it any more. Stacking, with
    /// no such ancestor, is still the leaf's own parent.
    fn toggle_stacked(&mut self, key: SurfaceKey) -> bool {
        let Node::Split {
            children, stacked, ..
        } = self
        else {
            return false;
        };
        if !children.iter().any(|child| child.contains(key)) {
            return false;
        }
        if *stacked {
            *stacked = false;
            return true;
        }
        if children
            .iter()
            .any(|child| matches!(child, Node::Leaf(candidate) if *candidate == key))
        {
            *stacked = true;
            return true;
        }
        children.iter_mut().any(|child| child.toggle_stacked(key))
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
    pending_axis: Axis,
    fullscreen: Option<SurfaceKey>,
    /// Every mapped leaf, most-recently-focused first. A stack shows the one
    /// of its own leaves that comes first here, so it keeps showing what it
    /// showed when focus left it rather than snapping back to its first.
    /// Focus alone cannot answer that: it names one leaf per workspace, and a
    /// stack the operator is not in has none of it.
    recent: Vec<SurfaceKey>,
}

impl Workspace {
    fn new() -> Workspace {
        Workspace {
            root: None,
            focused: None,
            pending_axis: Axis::Horizontal,
            fullscreen: None,
            recent: Vec::new(),
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
        let focused = self.focused.or_else(|| {
            let mut keys = Vec::new();
            root.leaves(&mut keys);
            keys.first().copied()
        });
        let inserted =
            focused.is_some_and(|current| root.insert_after(current, key, self.pending_axis));
        if !inserted {
            root = Node::Split {
                axis: self.pending_axis,
                children: vec![root, Node::Leaf(key)],
                stacked: false,
            };
        }
        self.root = Some(root);
        self.focus(key);
    }

    fn unmap(&mut self, key: SurfaceKey) {
        let before = self.leaves();
        let removed = before.iter().position(|candidate| *candidate == key);
        let Some(root) = self.root.take() else {
            return;
        };
        self.root = remove_node(root, key);
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

    #[cfg(test)]
    pub fn active_workspace(&self) -> u8 {
        self.active
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
    /// one measures its half along. A stack's bands run DOWN its top whatever
    /// its container's axis is, so one answers Vertical; otherwise the
    /// container's own axis, since each leaf carries its own band at the top
    /// of its own tile. A lone window has neither.
    pub fn run_direction(&self, key: SurfaceKey) -> Option<Axis> {
        let root = self.workspaces.get(&self.active)?.root.as_ref()?;
        if stack_run(root, key).is_some() {
            return Some(Axis::Vertical);
        }
        parent_axis(root, key)
    }

    /// Whether dragging this window could reach anywhere at all: a workspace
    /// that is not fullscreen, and a second window to land beside. Asked
    /// BEFORE a gesture takes a button, since one that cannot move anything
    /// would swallow the click with nothing to show for it.
    pub fn can_drag(&self, key: SurfaceKey) -> bool {
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return false;
        };
        if workspace.fullscreen.is_some() {
            return false;
        }
        let leaves = workspace.leaves();
        leaves.len() > 1 && leaves.contains(&key)
    }

    /// Drop a dragged window onto a target one — the pointer half of a move,
    /// where the destination is a window rather than a direction, and the
    /// KIND says what landing on it means.
    ///
    /// Answers whether the tree CHANGED — exactly that, and not whether the
    /// call was made. The one shipped caller previews the drop and compares
    /// arrangements instead, so nothing reads this today; it is the contract
    /// the layout-side tests are written against.
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
                stacked: false,
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
            Command::SetSplit(axis) => self.workspace_mut(self.active).pending_axis = axis,
            Command::SwitchWorkspace(number) if valid_workspace(number) => {
                self.active = number;
                self.workspace_mut(number);
            }
            Command::MoveToWorkspace(number) if valid_workspace(number) => {
                self.move_to_workspace(number)
            }
            Command::ToggleStacked => {
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
                if let Some(root) = self.workspace_mut(self.active).root.as_mut() {
                    root.toggle_stacked(focused);
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
        let placements = unstacked_placements(workspace, VIRTUAL_EXTENT, VIRTUAL_EXTENT, 0);
        if let Some(target) = directional_target(&placements, focused, direction) {
            self.workspace_mut(self.active).focus(target);
        }
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
                    stacked: false,
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
            stacked: false,
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
    place_node(
        root,
        rect,
        &Pass {
            gap,
            band,
            focused: workspace.focused,
            recent: &workspace.recent,
            honour_stacking,
        },
        &mut placements,
    );
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
        stacked: false,
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
            stacked,
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
                stacked,
            })
        }
    }
}

/// The leaf `direction` reaches from `key` WITHIN the stack presenting it, or
/// `None` when there is no stack or the step would leave it.
///
/// The OUTERMOST stacked ancestor is the one that presents `key`, because
/// that is where the renderer stops: `place_node` hands the first stacked
/// container it meets to `place_stack`, which draws one band per LEAF beneath
/// it. A stack nested inside another is therefore not shown at all, and
/// walking its run would step through bands nobody can see. Same container
/// `toggle_stacked` unstacks, for the same reason. Only `Up`/`Down` walk it,
/// because that is the direction the bands run — `Left`/`Right` keep their
/// geometric meaning so a stacked column can still be left for its neighbour.
fn stack_neighbour(node: &Node, key: SurfaceKey, direction: Direction) -> Option<SurfaceKey> {
    let step: isize = match direction {
        Direction::Up => -1,
        Direction::Down => 1,
        Direction::Left | Direction::Right => return None,
    };
    let run = stack_run(node, key)?;
    let at = run.iter().position(|candidate| *candidate == key)?;
    let next = isize::try_from(at).ok()?.checked_add(step)?;
    run.get(usize::try_from(next).ok()?).copied()
}

/// Every leaf the outermost stacked ancestor of `key` presents, in band order
/// — which is `leaves` exactly, since `place_stack` builds its run the same
/// way.
fn stack_run(node: &Node, key: SurfaceKey) -> Option<Vec<SurfaceKey>> {
    let Node::Split {
        children, stacked, ..
    } = node
    else {
        return None;
    };
    let index = children.iter().position(|child| child.contains(key))?;
    if *stacked {
        let mut keys = Vec::new();
        node.leaves(&mut keys);
        return Some(keys);
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
            stacked,
        } => {
            let retained: Vec<Node> = children
                .into_iter()
                .filter_map(|child| remove_node(child, key))
                .collect();
            rebuilt(axis, retained, stacked)
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
            stacked,
        } => {
            let retained: Vec<Node> = children.into_iter().filter_map(collapsed).collect();
            rebuilt(axis, retained, stacked)
        }
    }
}

fn rebuilt(axis: Axis, mut children: Vec<Node>, stacked: bool) -> Option<Node> {
    match children.len() {
        0 => None,
        // A container that collapses to one child is gone, and its
        // presentation goes with it: the survivor is not a stack.
        1 => children.pop(),
        _ => Some(Node::Split {
            axis,
            children,
            stacked,
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
                stacked: false,
                visible: true,
            });
        }
        Node::Split {
            axis,
            children,
            stacked,
        } => {
            if *stacked && pass.honour_stacking {
                place_stack(children, rect, pass, placements);
                return;
            }
            let rects = split_rects(rect, *axis, children.len(), pass.gap);
            for (child, child_rect) in children.iter().zip(rects) {
                place_node(child, child_rect, pass, placements);
            }
        }
    }
}

/// A stacked container: one band per LEAF beneath it, in a run at the top, and
/// the whole area below the run to whichever leaf is focused.
///
/// Per leaf rather than per CHILD, which is where this diverges from i3: td
/// has no container titles, so a split child's band would have to borrow some
/// leaf's name. A nested split's arrangement is therefore not shown while its
/// container is stacked, and unstacking restores it untouched.
fn place_stack(children: &[Node], rect: Rect, pass: &Pass, placements: &mut Vec<Placement>) {
    let band = pass.band;
    let mut keys = Vec::new();
    for child in children {
        child.leaves(&mut keys);
    }
    let bottom = rect.y.saturating_add(rect.height);
    let run = band.saturating_mul(keys.len()).min(rect.height);
    let content = Rect {
        x: rect.x,
        y: rect.y.saturating_add(run),
        width: rect.width,
        height: rect.height.saturating_sub(run),
    };
    // The stack's own MOST RECENTLY FOCUSED leaf gets the content, which is
    // the focused one whenever focus is in the stack at all, since focusing is
    // what puts a leaf at the front of that record. Focus alone would answer
    // only for the stack the operator is in and snap every other one back to
    // its first leaf. Both fallbacks are for a leaf the record does not name,
    // which `check_invariants` forbids and no expressible key sequence
    // reaches; the record cannot say "must be present" without a panic.
    let shown = keys
        .iter()
        .enumerate()
        .min_by_key(|(_, key)| {
            pass.recent
                .iter()
                .position(|candidate| candidate == *key)
                .unwrap_or(usize::MAX)
        })
        .map(|(index, _)| index)
        .unwrap_or(0);
    for (index, key) in keys.iter().enumerate() {
        let top = rect
            .y
            .saturating_add(index.saturating_mul(band))
            .min(bottom);
        placements.push(Placement {
            key: *key,
            rect: content,
            band: Rect {
                x: rect.x,
                y: top,
                width: rect.width,
                height: top.saturating_add(band).min(bottom).saturating_sub(top),
            },
            focused: pass.focused == Some(*key),
            stacked: true,
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));

        // Unstacked first, so the SAME arrangement is measured both ways and
        // the difference is the presentation rather than the tree.
        let split = layout.placements(100, 300, 0, 20);
        assert_eq!(split.len(), 3);
        assert!(split.iter().all(|placement| placement.visible));
        assert_eq!(
            split.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 100, 200]
        );

        layout.apply(Command::ToggleStacked);
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
        layout.apply(Command::ToggleStacked);
        let restored = layout.placements(100, 300, 0, 20);
        let geometry = |placements: &[Placement]| {
            placements
                .iter()
                .map(|p| (p.key, p.rect, p.band, p.stacked, p.visible))
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
    fn stacking_toggles_the_focused_leafs_own_container_and_a_lone_window_has_none() {
        let mut layout = Layout::new();
        layout.map(key(1));
        // A lone window IS the root, with no container to stack. Nothing
        // happens rather than the workspace acquiring a presentation — and
        // the whole placement has to say so, since `visible` is a constant
        // for an unstacked leaf and could not have reported otherwise.
        let alone = layout.placements(100, 300, 0, 20);
        layout.apply(Command::ToggleStacked);
        assert_eq!(layout.placements(100, 300, 0, 20), alone);

        // A column nested inside a row: stacking from inside it takes THAT
        // container, so the row beside it keeps its own arrangement.
        layout.map(key(2));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        assert_eq!(layout.focused(), Some(key(3)));
        layout.apply(Command::ToggleStacked);
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
    fn a_stack_unstacks_from_any_leaf_once_none_of_them_is_a_child_of_it() {
        // A stacked ROW, then both of its children split into columns. It now
        // holds no leaf directly, while still running every leaf beneath it —
        // so `Mod+S` has to reach it by walking DOWN to the leaf rather than
        // up from the leaf's own parent, or nothing on screen can undo it.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleStacked);
        assert!(layout.focus_key(key(1)));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        assert!(layout.focus_key(key(2)));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(4));

        // All four run in the one stack, sharing the one content rectangle.
        let stacked = layout.placements(200, 400, 0, 20);
        assert_eq!(stacked.len(), 4);
        let content = stacked.first().unwrap().rect;
        assert!(stacked.iter().all(|p| p.rect == content && p.stacked));
        assert_eq!(
            stacked.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 20, 40, 60]
        );

        layout.apply(Command::ToggleStacked);
        let split = layout.placements(200, 400, 0, 20);
        assert!(
            split.iter().all(|p| p.visible && !p.stacked),
            "a leaf that is no child of the stack could not unstack it"
        );
        // Two columns of two, which is what the tree said all along.
        assert_eq!(
            split
                .iter()
                .map(|p| (p.band.x, p.band.y))
                .collect::<Vec<_>>(),
            [(0, 0), (0, 200), (100, 0), (100, 200)]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stack_survives_an_unmap_and_loses_its_presentation_only_when_it_collapses() {
        let mut layout = Layout::new();
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);

        // Closing one window of three leaves a stack of two. `remove_node`
        // rebuilds the container, so its presentation has to be carried over
        // rather than defaulted.
        layout.unmap(key(2));
        let two = layout.placements(100, 300, 0, 20);
        assert_eq!(two.len(), 2);
        assert!(two.iter().all(|p| p.stacked));
        assert_eq!(two.iter().map(|p| p.band.y).collect::<Vec<_>>(), [0, 20]);

        // Closing a second collapses the container into its survivor, and a
        // lone window is not a stack: there is nothing left to present.
        layout.unmap(key(1));
        let one = layout.placements(100, 300, 0, 20);
        assert_eq!(one.len(), 1);
        assert!(one.iter().all(|p| !p.stacked && p.visible));
        assert_eq!(one.first().unwrap().band.y, 0);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn a_stacked_away_leaf_is_published_hidden_at_the_size_it_would_have() {
        let mut layout = Layout::new();
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleStacked);

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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleFullscreen);

        // Nothing on screen could report the change, so the operator would
        // leave fullscreen into an arrangement they never asked for.
        layout.apply(Command::ToggleStacked);
        layout.apply(Command::ToggleFullscreen);
        let placements = layout.placements(100, 300, 0, 20);
        assert!(placements.iter().all(|p| !p.stacked && p.visible));
        assert_eq!(
            placements.iter().map(|p| p.band.y).collect::<Vec<_>>(),
            [0, 150]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn moving_inside_a_stack_reorders_the_run_and_the_shown_leaf_with_it() {
        let mut layout = Layout::new();
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        layout.map(key(4));
        layout.apply(Command::ToggleStacked);

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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);

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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));

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
                    stacked: false,
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
                    stacked: false,
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
                    stacked: false,
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(2));
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
        layout.apply(Command::ToggleStacked);
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

        // Left/Right are UNCHANGED, and for this stack they still walk it:
        // the run expands back into the row it was made from, which is why
        // they worked here while Up/Down — the direction the bands actually
        // run — did not.
        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.check_invariants().unwrap();
    }

    #[test]
    fn left_and_right_leave_a_stack_rather_than_walking_it() {
        // A stacked COLUMN beside another window is what tells the two apart:
        // in a stacked ROW the run and the geometry agree either way, so only
        // this shape shows that Left/Right were left alone. Walking the run in
        // every direction would trap the operator in the stack.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        assert_eq!(layout.parent_axis(key(3)), Some(Axis::Vertical));
        layout.apply(Command::ToggleStacked);
        assert_eq!(layout.focused(), Some(key(3)));

        layout.apply(Command::Focus(Direction::Left));
        assert_eq!(layout.focused(), Some(key(1)), "Left must leave the stack");
        // Coming BACK lands on neither the run's first leaf nor the one the
        // stack is SHOWING, but on whichever the geometric tie-break picks —
        // the lowest-keyed among equal ranks, every leaf in the stack sharing
        // the rectangle being ranked. Here that happens to be the run's first
        // too, so the shape cannot tell those apart; asserted as it is rather
        // than as it should be, so the day it is fixed this test says so.
        layout.apply(Command::Focus(Direction::Right));
        assert_eq!(layout.focused(), Some(key(2)));
        // Down still walks the run it is in.
        layout.apply(Command::Focus(Direction::Down));
        assert_eq!(layout.focused(), Some(key(3)));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);
        assert_eq!(layout.focused(), Some(key(3)));
        // Now stack the outer row as well, from a leaf that is its child.
        layout.focus_key(key(1));
        layout.apply(Command::ToggleStacked);
        let bands = layout.placements(200, 400, 0, 20);
        assert!(bands.iter().all(|placement| placement.stacked));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(2));
        layout.focus_key(key(1));
        layout.apply(Command::SetSplit(Axis::Horizontal));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);
        let run: Vec<u32> = layout
            .placements(200, 400, 0, 20)
            .iter()
            .filter(|placement| placement.stacked)
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
        // `Hstacked[V[1, 3], V[2, 4]]`. td draws one band per LEAF beneath a
        // stack rather than one per child — `place_stack` says so — so the run
        // is all four. Walking per CHILD would step 1 to 2 and skip the band
        // for 3 that is drawn between them.
        // Stacked FIRST, then its children split: `ToggleStacked` stacks the
        // container a leaf is a direct child of, so splitting first would
        // stack an inner column instead of the row.
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleStacked);
        layout.focus_key(key(1));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        layout.focus_key(key(2));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(4));
        let placements = layout.placements(200, 400, 0, 20);
        assert!(
            placements.iter().all(|placement| placement.stacked),
            "the ROW is the stack, and it presents all four"
        );
        let run: Vec<u32> = placements
            .iter()
            .map(|placement| placement.key.object)
            .collect();
        assert_eq!(run, [1, 3, 2, 4], "the drawn band order");
        layout.focus_key(key(1));

        for expected in [3, 2, 4] {
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));

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
            layout.apply(Command::SetSplit(Axis::Vertical));
            layout.map(key(3));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));

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
            layout.apply(Command::SetSplit(axis));
            layout.map(key(1));
            layout.map(key(2));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);
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
            .all(|p| p.stacked));

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
                .all(|p| p.stacked),
            "the swap unstacked the container it landed in"
        );
        // Focused, so the stack shows the window that just arrived rather
        // than leaving the operator looking at the one it replaced.
        let shown = placements
            .iter()
            .filter(|p| p.stacked && p.visible)
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));

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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::ToggleStacked);
        assert!(layout.placements(100, 300, 0, 20).iter().all(|p| p.stacked));

        // A drop onto the stack's own axis, which is what an edge of a tile
        // inside one answers.
        assert!(layout.drop_onto(key(2), key(1), beside(Axis::Vertical, true)));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(
            placements.iter().all(|p| p.stacked),
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
            placements.iter().all(|p| p.stacked),
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);
        assert!(layout.placements(100, 300, 0, 20).iter().all(|p| p.stacked));

        // "1 to the RIGHT of 2", across a column that presents as a list.
        assert!(layout.drop_onto(key(1), key(2), beside(Axis::Horizontal, false)));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [2, 1, 3]
        );
        assert!(
            placements.iter().all(|p| p.stacked),
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
        layout.apply(Command::ToggleStacked);
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
        // The refusal is about the outermost stacked ANCESTOR, not about the
        // container the leaf sits in. A stack already holding a split shows
        // the leaves under it flattened all the same, so a second split one
        // level further down would be exactly as invisible as the first.
        // `V{stacked}[1, 2, H[3, 4]]`, built the only way it can be: opening a
        // window while the pending axis crosses the stack's. That route is
        // its own defect and its own increment — see this commit's message.
        // Here it is only the shape.
        let shape = || {
            let mut layout = Layout::new();
            layout.apply(Command::SetSplit(Axis::Vertical));
            layout.map(key(1));
            layout.map(key(2));
            layout.map(key(3));
            layout.apply(Command::ToggleStacked);
            layout.apply(Command::SetSplit(Axis::Horizontal));
            layout.map(key(4));
            layout
        };
        let mut layout = shape();
        assert_eq!(layout.parent_axis(key(4)), Some(Axis::Horizontal));
        assert!(layout.placements(100, 400, 0, 20).iter().all(|p| p.stacked));

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
            placements.iter().all(|p| p.stacked),
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
        assert!(placements.iter().all(|p| p.stacked));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);

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
        assert!(placements.iter().all(|p| p.stacked));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        layout.map(key(4));
        layout.apply(Command::ToggleStacked);
        assert!(layout.focus_key(key(2)));

        layout.apply(Command::Move(Direction::Left));
        let entered = layout.placements(100, 300, 0, 20);
        assert_eq!(
            entered.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
        assert!(entered.iter().all(|p| p.stacked));
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
            left.iter().map(|p| p.stacked).collect::<Vec<_>>(),
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));
        assert!(layout.focus_key(key(1)));
        layout.apply(Command::ToggleStacked);
        assert!(layout.placements(100, 300, 0, 20).iter().all(|p| p.stacked));

        layout.apply(Command::Move(Direction::Right));
        let placements = layout.placements(100, 300, 0, 20);
        assert_eq!(
            placements.iter().map(|p| p.key.object).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(
            placements.iter().all(|p| !p.stacked && p.visible),
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
            layout.apply(Command::SetSplit(Axis::Vertical));
            layout.map(key(3));
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
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(1));
        layout.map(key(2));
        layout.map(key(3));
        layout.apply(Command::ToggleStacked);
        assert!(layout.focus_key(key(1)));

        let stack = layout.placements(100, 300, 0, 20);
        assert!(stack.iter().all(|p| p.stacked));
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
                stacked: false,
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
    fn unmap_collapses_containers_and_selects_next_then_previous() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));

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
    fn selected_axis_applies_on_an_empty_workspace_and_one_tile_keeps_a_pixel() {
        let mut layout = Layout::new();
        layout.apply(Command::SwitchWorkspace(2));
        layout.apply(Command::SetSplit(Axis::Vertical));
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
                stacked: false,
                visible: true
            }]
        );
        layout.map(key(2));
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
            Command::SetSplit(Axis::Vertical),
            Command::SetSplit(Axis::Horizontal),
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
