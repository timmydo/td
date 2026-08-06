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

    fn swap(&mut self, first: SurfaceKey, second: SurfaceKey) {
        match self {
            Node::Leaf(key) if *key == first => *key = second,
            Node::Leaf(key) if *key == second => *key = first,
            Node::Leaf(_) => {}
            Node::Split { children, .. } => {
                for child in children {
                    child.swap(first, second);
                }
            }
        }
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
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return;
        };
        let placements = unstacked_placements(workspace, VIRTUAL_EXTENT, VIRTUAL_EXTENT, 0);
        let Some(target) = directional_target(&placements, focused, direction) else {
            return;
        };
        if let Some(root) = self.workspace_mut(self.active).root.as_mut() {
            root.swap(focused, target);
        }
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

fn remove_node(node: Node, key: SurfaceKey) -> Option<Node> {
    match node {
        Node::Leaf(candidate) if candidate == key => None,
        Node::Leaf(candidate) => Some(Node::Leaf(candidate)),
        Node::Split {
            axis,
            children,
            stacked,
        } => {
            let mut retained: Vec<Node> = children
                .into_iter()
                .filter_map(|child| remove_node(child, key))
                .collect();
            match retained.len() {
                0 => None,
                // A container that collapses to one child is gone, and its
                // presentation goes with it: the survivor is not a stack.
                1 => retained.pop(),
                _ => Some(Node::Split {
                    axis,
                    children: retained,
                    stacked,
                }),
            }
        }
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
        for (direction, prepare, target) in [
            (Direction::Left, None, 1),
            (Direction::Right, Some(Direction::Left), 2),
            (Direction::Up, None, 2),
            (Direction::Down, Some(Direction::Up), 3),
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
            let target_rect = rect(&layout, target);
            layout.apply(Command::Move(direction));
            assert_eq!(layout.focused(), Some(focused), "{direction:?}");
            assert_eq!(rect(&layout, focused.object), target_rect, "{direction:?}");
            layout.check_invariants().unwrap();
        }
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
