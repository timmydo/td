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
    pub rect: Rect,
    pub focused: bool,
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
    Split { axis: Axis, children: Vec<Node> },
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
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split {
                axis: current_axis,
                children,
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
}

impl Workspace {
    fn new() -> Workspace {
        Workspace {
            root: None,
            focused: None,
            pending_axis: Axis::Horizontal,
            fullscreen: None,
        }
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
            self.focused = Some(key);
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
            };
        }
        self.root = Some(root);
        self.focused = Some(key);
    }

    fn unmap(&mut self, key: SurfaceKey) {
        let before = self.leaves();
        let removed = before.iter().position(|candidate| *candidate == key);
        let Some(root) = self.root.take() else {
            return;
        };
        self.root = remove_node(root, key);
        let after = self.leaves();
        if self.fullscreen == Some(key) {
            self.fullscreen = None;
        }
        if self.focused != Some(key) && self.focused.is_some_and(|focused| after.contains(&focused))
        {
            return;
        }
        self.focused = removed.and_then(|index| {
            let candidate = index.min(after.len().saturating_sub(1));
            after.get(candidate).copied()
        });
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

    pub fn placements(&self, width: usize, height: usize, gap: usize) -> Vec<Placement> {
        let Some(workspace) = self.workspaces.get(&self.active) else {
            return Vec::new();
        };
        visible_placements(workspace, width, height, gap)
    }

    pub fn views(&self, width: usize, height: usize, gap: usize) -> Vec<ViewLayout> {
        let mut views = Vec::new();
        for (number, workspace) in &self.workspaces {
            let workspace_visible = *number == self.active;
            for mut placement in tiled_placements(workspace, width, height, gap) {
                let fullscreen = workspace.fullscreen == Some(placement.key);
                if fullscreen {
                    placement.rect = Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    };
                }
                views.push(ViewLayout {
                    key: placement.key,
                    rect: placement.rect,
                    visible: workspace_visible && (workspace.fullscreen.is_none() || fullscreen),
                    activated: workspace_visible && placement.focused,
                    fullscreen: workspace_visible && fullscreen,
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
            if workspace.fullscreen.is_some_and(|key| {
                !workspace
                    .root
                    .as_ref()
                    .is_some_and(|root| root.contains(key))
            }) {
                return Err(format!("workspace {number} fullscreen leaf is absent"));
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
        let placements = self.placements(VIRTUAL_EXTENT, VIRTUAL_EXTENT, 0);
        if let Some(target) = directional_target(&placements, focused, direction) {
            self.workspace_mut(self.active).focused = Some(target);
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
        let placements = self.placements(VIRTUAL_EXTENT, VIRTUAL_EXTENT, 0);
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
            focused: workspace.focused == Some(fullscreen),
        }];
    }
    tiled_placements(workspace, width, height, gap)
}

fn tiled_placements(
    workspace: &Workspace,
    width: usize,
    height: usize,
    gap: usize,
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
    place_node(root, rect, gap, workspace.focused, &mut placements);
    placements
}

fn valid_workspace(number: u8) -> bool {
    (INITIAL_WORKSPACE..=FINAL_WORKSPACE).contains(&number)
}

fn remove_node(node: Node, key: SurfaceKey) -> Option<Node> {
    match node {
        Node::Leaf(candidate) if candidate == key => None,
        Node::Leaf(candidate) => Some(Node::Leaf(candidate)),
        Node::Split { axis, children } => {
            let mut retained: Vec<Node> = children
                .into_iter()
                .filter_map(|child| remove_node(child, key))
                .collect();
            match retained.len() {
                0 => None,
                1 => retained.pop(),
                _ => Some(Node::Split {
                    axis,
                    children: retained,
                }),
            }
        }
    }
}

fn place_node(
    node: &Node,
    rect: Rect,
    gap: usize,
    focused: Option<SurfaceKey>,
    placements: &mut Vec<Placement>,
) {
    match node {
        Node::Leaf(key) => placements.push(Placement {
            key: *key,
            rect,
            focused: focused == Some(*key),
        }),
        Node::Split { axis, children } => {
            let rects = split_rects(rect, *axis, children.len(), gap);
            for (child, child_rect) in children.iter().zip(rects) {
                place_node(child, child_rect, gap, focused, placements);
            }
        }
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

    fn key(object: u32) -> SurfaceKey {
        SurfaceKey { client: 1, object }
    }

    fn rect(layout: &Layout, object: u32) -> Rect {
        let placements = layout.placements(100, 100, 0);
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
    fn maps_horizontal_then_nests_the_selected_vertical_split() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.map(key(2));
        layout.apply(Command::SetSplit(Axis::Vertical));
        layout.map(key(3));

        assert_eq!(
            layout.placements(100, 100, 0),
            [
                Placement {
                    key: key(1),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 50,
                        height: 100
                    },
                    focused: false
                },
                Placement {
                    key: key(2),
                    rect: Rect {
                        x: 50,
                        y: 0,
                        width: 50,
                        height: 50
                    },
                    focused: false
                },
                Placement {
                    key: key(3),
                    rect: Rect {
                        x: 50,
                        y: 50,
                        width: 50,
                        height: 50
                    },
                    focused: true
                }
            ]
        );
        layout.check_invariants().unwrap();
    }

    #[test]
    fn directional_focus_covers_all_emacs_directions_and_edges() {
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
        assert_eq!(layout.placements(100, 100, 0).len(), 1);

        layout.apply(Command::SwitchWorkspace(2));
        assert_eq!(layout.focused(), Some(key(2)));
        assert_eq!(layout.placements(100, 100, 0).len(), 1);
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
            layout.views(100, 80, 0),
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
            layout.views(100, 80, 0),
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
                .placements(100, 100, 0)
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
        assert!(layout.placements(100, 100, 0).is_empty());
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
        assert_eq!(
            layout.placements(80, 60, 9),
            [Placement {
                key: key(2),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 60
                },
                focused: true
            }]
        );
        layout.apply(Command::Focus(Direction::Left));
        layout.apply(Command::Move(Direction::Left));
        assert_eq!(layout.focused(), Some(key(2)));
        layout.apply(Command::SwitchWorkspace(2));
        layout.map(key(3));
        assert_eq!(layout.placements(80, 60, 9).len(), 1);
        layout.apply(Command::SwitchWorkspace(1));
        assert_eq!(layout.placements(80, 60, 9).len(), 1);
        layout.apply(Command::ToggleFullscreen);
        assert_eq!(layout.placements(80, 60, 9).len(), 2);
        layout.check_invariants().unwrap();
    }

    #[test]
    fn mapping_a_new_surface_leaves_fullscreen_and_focuses_the_new_leaf() {
        let mut layout = Layout::new();
        layout.map(key(1));
        layout.apply(Command::ToggleFullscreen);
        layout.map(key(2));
        assert_eq!(layout.focused(), Some(key(2)));
        assert_eq!(layout.placements(80, 60, 0).len(), 2);
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
        assert!(layout.placements(100, 100, 0).is_empty());
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
        assert_eq!(layout.placements(100, 100, 0).len(), 1);
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
            layout.placements(1, 1, 24),
            [Placement {
                key: key(1),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                },
                focused: true
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
