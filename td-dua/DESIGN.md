# td-dua

td-dua is td's disk usage analyzer, in the spirit of WinDirStat: one
window over one directory, the hierarchy above sorted by size, a treemap
below in which every file is a rectangle in proportion to the space it
takes, and a delete list to reclaim it. It is dependency-free Rust over
td-ui's widget window, tree table, split pane and confirmation dialog.
This document is its component contract; the root `AGENTS.md` and
`DEVELOPMENT.md` govern changes and submission.

## Status and scope

Built: the crate, its scan, tree, list, treemap, delete list and window,
with unit, filesystem, driven-state and confinement tests. It joins the
gate by existing (`[package.metadata.td-gate]`, all-target Clippy).

Not yet: a recipe, image integration and a launcher entry; a driven
control socket; a native-compositor process case; a legend for the
treemap's colours; cushion shading. Each is its own increment.

## Running

`td-dua [DIRECTORY]` scans DIRECTORY, else the working directory, after
resolving it to an absolute path with links resolved; anything that is
not a directory is refused before a window opens. `--preview
WIDTHxHEIGHT [DIRECTORY [SELECT]]` scans on the calling thread and writes
the first frame as a PPM in the bitmap face, with SELECT (a path relative
to DIRECTORY) revealed and selected. `--font-license` prints the face
notices; `--help` the usage and keys.

## The window

From the top: the tree table, the split's divider, the treemap, and the
status row. The split is vertical with logical minima of 96 and 48
pixels and starts at half; dragging the divider resizes both halves. A
window too small for both minima shows neither, keeping its state.

The list's columns are Name, Size, % of parent, Files and Modified. The
root row is the scanned directory's path and starts open and selected.
A directory's name ends in `/`, a symbolic link's in ` @`, a mount in
`(other file system)`, an unreadable directory in `(unreadable)`, and an
entry on the delete list starts with `[delete] `. Files counts the
entries beneath a directory that are not directories. Modified is the
newest modification time in the subtree as a UTC date. Each directory's
entries sort by the chosen column, ties by name: by size descending at
start; clicking a heading sorts by it in its first direction (names A to
Z, the rest largest or newest first), clicking it again reverses. % of
parent sorts as Size.

A directory shows its first 500 entries (`view::SHOWN`) and then a row
saying how many more it holds; activating that row shows 500 more and
selects the first of them; that row is never a delete target. The whole
list stops at the toolkit's row limit (`tree_table::ROWS`), and the
status row says so while it does. Names are shown lossily, each control
character a `?`.

The treemap lays out the whole tree by the squarified algorithm into the
lower half. Each directory's rectangle is divided among its entries by
subtree size and its own blocks; a directory narrower or shorter than
three pixels (scaled) is one tile, and so is one whose entries would take
the tiles made and still to lay past `treemap::MAX_TILES`, which bounds
the tiles a layout makes. A change to the delete list only recolours the
tiles; the tree, the measure or the area changing lays them out again.
Edges are rounded independently, so tiles tile the area with no gap or
overlap. A file is coloured by extension:
the twelve extensions holding the most space take the palette's colours
in rank order, other files are grey, directories' own blocks and
undivided directories brown, links, devices and mounts light grey, and
anything the delete list holds, itself or through an ancestor, dark. Each
tile has a light top-left and dark bottom-right edge. The selected entry,
or its nearest laid-out ancestor, is outlined.

Clicking a tile selects the entry it shows and opens the list to it:
every ancestor is expanded, each shows enough entries to include the
path, the entry itself is expanded if it is a directory, and the row is
scrolled into view. An entry the list cannot show (deeper than
`tree_table::DEPTH`, or past the row limit) leaves nothing selected and
says so, so no earlier selection stays the target of a delete key.

## Keys

Arrows, Page Up/Down, Home and End move in the list; Left collapses or
goes to the parent, Right expands or goes to the first child; Enter,
Space or a double click opens or closes a directory; Shift+Left/Right
scroll sideways; the wheel scrolls three rows a notch.

- `d` adds the selected entry to the delete list. The root, a mount, a
  directory holding a mount, an entry already listed, and an entry
  beneath a listed directory are refused, saying why; `D` refuses the
  same first three. Adding a directory above listed entries is
  allowed; the status row and the question count only the entries no
  other listed entry lies above, which is what is deleted.
- `u` takes the last added entry off the list.
- `x` asks, in td-ui's confirmation dialog, whether to delete the list:
  its title gives the count and total, its details each entry's path and
  size. Cancel has the initial focus, so Enter alone deletes nothing;
  Tab then Enter deletes. A list or tree that changes while the question
  is open closes it unanswered.
- `D` deletes the selected entry at once, without asking, as asked of
  this tool. With Caps Lock on, the toolkit's keymap spells `d` as `D`
  too, so a person meaning to list an entry deletes it instead. To keep
  that to one entry, a deletion that removes the selected entry leaves
  nothing selected: a second `D` says to select an entry first rather
  than climbing to the directory that held it.
- `r` rescans the selected directory, the one holding the selected file,
  or the directory whose "more" row is selected. On the root it scans
  everything into a fresh tree; elsewhere the result is grafted in place.
  The open directories, how many entries each shows and the delete list
  follow their entries by path; a listed entry is carried only to the
  same inode of the same kind, else it leaves the delete list, and the
  status says how many did. A selection inside the refreshed directory
  follows its entry by path, a "more" row staying a "more" row, and
  leaves nothing selected when the entry is gone; a selection elsewhere,
  made while the scan ran, stays. A mount
  refreshed this way is entered, since the scan starts on its file
  system.
- `a` switches sizes between blocks allocated on disk (`du`'s figure, the
  default) and apparent lengths.
- Ctrl+Q or the compositor's close quits.

While a scan or deletion is running, `x`, `D` and `r` are refused, so
replies always apply to the tree they were asked about; `d` and `u` are
list edits and always work. The status row shows the running scan's
entry count and bytes, the delete list's count and total, and the last
message.

## Scan

`scan::scan` walks iteratively, so depth costs no stack. It reads entries
with `DirEntry::metadata`, which does not follow symbolic links, and never
enters a directory on a device other than the root's: such a directory is
a `Mount` node with no children and, as `du -x` counts, no bytes. An
inode with more than one link that is not a directory is counted once
per scan; later names own no bytes, and every name is marked linked. A
directory's own blocks count toward its total, as `du` counts them. An
unreadable listing or entry marks its directory unreadable and the tree
partial, and the walk goes on. The tree holds at most `tree::MAX_NODES`
nodes, retired ones included; a scan that reaches it stops and is
partial. A refresh's hard-link set is its own, so a link whose other name
lies outside the refreshed directory is counted again there.

## Tree

The tree is an arena whose ids are never reused: a refresh below the root
retires the replaced subtree and appends the new one, keeping the
refreshed node's id, and a deletion retires the deleted subtree. A
selection, expansion or delete-list entry naming a retired id reads as
gone and is pruned. Retired slots are reclaimed only by a root refresh,
which replaces the tree; a graft that would pass `tree::MAX_NODES` is
refused and says to refresh the root. Totals and newest times are summed
once after a scan and carried along the ancestors on each graft, removal
and change of a node's own bytes.

## Deletion

Deletion runs on the worker thread. Each target carries the path, the
device and inode the scan recorded, whether it was a directory, and its
length and modification time. A path that now names another inode, an
entry of the other type, or a non-directory whose length or time moved is
refused as changed since the scan. A device and inode alone would not
do: a file system may give a freed inode number to the next file
created. A target on another device than the directory holding it is
refused, which covers mounts and btrfs subvolumes that the mount table
does not list. A file or link is unlinked. A directory is refused if
`/proc/self/mountinfo` cannot be read or lists a mount point at or
beneath its canonical path (the table is read as bytes, so a mount point
need not be UTF-8); otherwise it is removed depth first by td-dua's own
walk, never `remove_dir_all`. The walk follows no link, refuses any
entry on another device, and checks each directory again just before
listing it: still a directory, not a link, with the device and inode it
was listed with. An entry beneath the target that another process
removed first, file or directory, is passed over.
Any other failure stops that target where it was, and its directory is
rescanned so the tree matches the disk. Each succeeded target is removed
from the tree and failed ones stay on the delete list. A deleted name
that owned a linked inode's bytes hands them to a surviving name of the
same inode in the tree, and only the bytes no surviving name in the
tree holds are reported as freed. A deletion that removes the selected
entry leaves nothing selected.

The checks are path-based (std offers no `openat` family, and td-dua has
no `unsafe`), so they narrow races but cannot close them: a directory
swapped for a symbolic link between its check and its listing would be
listed through the link, and its entries unlinked wherever the link
points, on any file system. std's `remove_dir_all` closes that race with
`openat` and `O_NOFOLLOW` but enters mounts, which td-dua refuses to do.
Making such a swap takes write access inside the target; td-dua runs
with its user's authority and deletes only what that user may.

## Threads

The window's thread owns the state (`app::App`), which touches no file
system and hands `worker::Job`s out; the worker thread runs them in order
and answers `worker::Reply`s, polled each turn (every 100 ms while a job
is out, else up to a second). The window reads its own clock at every
input, so a double click is timed when it happens. Dropping the worker
cancels a running scan; a deletion in progress is left to the process's
exit.

## Confinement

`tests/confinement.rs` pins the source inventory, `forbid(unsafe_code)`
in both crate roots, that `app.rs`, `tree.rs`, `treemap.rs` and `view.rs`
reach no system interface (and use no grouped `std::{` import that would
hide one from the scan), that only `delete.rs` removes and nothing
writes, renames or changes permissions, that deletion reads the mount
table and never calls `remove_dir_all`, and that the manifest's one
dependency is td-ui.

## Tests

- Unit: tree sums, paths, removal, grafting (at the root too), relative
  paths and their resolution, own-byte changes; the treemap's coverage,
  disjointness, proportion, colour ranking, hit, tile budget and extreme
  ratios; formats; the visible preorder, its sort and its more row;
  mount-table unescaping, non-UTF-8 included; the device comparison.
- `tests/fs.rs`: hard links counted once, links not followed, a replaced
  path refused, a changed length, time or type refused, a directory's
  moved time accepted, a tree removed without following a link out of
  it.
- `tests/app.rs`: the initial sort and keyboard navigation, heading
  sorts, a treemap click selecting and expanding, an entry past the
  depth limit leaving nothing selected, the more row, the delete list's
  refusals, undo, Escape and Cancel keeping everything and Tab-Enter
  deleting, a stale question closing, `D` leaving nothing selected and a
  second `D` doing nothing, busy refusals, a surviving hard link taking
  the bytes, `r` grafting a new file and reselecting by path, a
  selection made elsewhere during a refresh kept, a vanished selection
  leaving nothing selected, a root refresh replacing the tree and
  carrying state by path, and painting at scales one to four.

Not covered by a test: a target on another device or a mount point
(neither can be made without privilege; the device comparison and the
mount-table reading are unit-tested apart), the per-directory re-check's
refusal (it needs a racing swap), and dragging the divider.
