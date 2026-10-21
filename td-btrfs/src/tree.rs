//! Building a whole B-tree from its sorted items at once.
//!
//! A tree's shape (which items fill which leaf, which children each node
//! holds) depends only on its items, so a writer can count every tree's
//! blocks, assign them addresses, and only then serialize, filling each
//! node's child pointers.

use std::io;
use std::ops::Range;

use crate::crc32c;
use crate::format::{
    invalid, place, put32, put64, Header, Item, Key, BLOCK_FLAGS, HEADER_SIZE, ITEM_SIZE,
    LEAF_DATA, MAX_LEVEL, NODESIZE, NODE_PTRS,
};

/// Which items each leaf holds, and which children each node above holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shape {
    leaves: Vec<Range<usize>>,
    /// `levels[n]` are the nodes at level `n + 1`, each a range of the
    /// level below.
    levels: Vec<Vec<Range<usize>>>,
}

impl Shape {
    /// Pack `items` (sorted, unique keys) greedily into leaves, then nodes.
    pub fn of(items: &[Item]) -> io::Result<Self> {
        let mut leaves = Vec::new();
        let mut start = 0;
        let mut used = 0;
        for (index, item) in items.iter().enumerate() {
            let cost = ITEM_SIZE + item.data.len();
            if cost > LEAF_DATA {
                return Err(invalid(format!(
                    "item {:?} is {} bytes, more than a leaf holds",
                    item.key,
                    item.data.len()
                )));
            }
            if used + cost > LEAF_DATA {
                leaves.push(start..index);
                start = index;
                used = 0;
            }
            used += cost;
        }
        leaves.push(start..items.len());
        let mut levels: Vec<Vec<Range<usize>>> = Vec::new();
        let mut below = leaves.len();
        while below > 1 {
            let nodes: Vec<Range<usize>> = (0..below)
                .step_by(NODE_PTRS)
                .map(|first| first..(first + NODE_PTRS).min(below))
                .collect();
            below = nodes.len();
            levels.push(nodes);
        }
        if levels.len() > usize::from(MAX_LEVEL) {
            return Err(invalid("tree is deeper than Btrfs allows"));
        }
        Ok(Self { leaves, levels })
    }

    pub fn blocks(&self) -> usize {
        self.leaves.len() + self.levels.iter().map(Vec::len).sum::<usize>()
    }

    /// The root's level.
    pub fn level(&self) -> u8 {
        self.levels.len() as u8
    }

    /// Each block's level, in serialization order: leaves first, then each
    /// level of nodes, the root last.
    pub fn block_levels(&self) -> Vec<u8> {
        let mut out = vec![0; self.leaves.len()];
        for (index, nodes) in self.levels.iter().enumerate() {
            out.extend(std::iter::repeat_n(index as u8 + 1, nodes.len()));
        }
        out
    }

    /// Serialize every block. `addresses` gives each block's logical address
    /// in the order `block_levels` lists them.
    pub fn serialize(
        &self,
        items: &[Item],
        addresses: &[u64],
        header: &Header,
    ) -> io::Result<Vec<Vec<u8>>> {
        if addresses.len() != self.blocks() {
            return Err(invalid("tree addresses do not match its shape"));
        }
        let mut blocks = Vec::with_capacity(self.blocks());
        // The first key under each block of the level just written.
        let mut firsts = Vec::with_capacity(self.leaves.len());
        let mut next = 0;
        for range in &self.leaves {
            let leaf = items
                .get(range.clone())
                .ok_or_else(|| invalid("leaf range outside the items"))?;
            let address = *addresses.get(next).ok_or_else(|| invalid("no address"))?;
            blocks.push(leaf_block(leaf, address, header)?);
            firsts.push(leaf.first().map(|item| item.key).unwrap_or_default());
            next += 1;
        }
        let mut below = 0;
        for (level, nodes) in self.levels.iter().enumerate() {
            let mut keys = Vec::with_capacity(nodes.len());
            for range in nodes {
                let address = *addresses.get(next).ok_or_else(|| invalid("no address"))?;
                let children = range.clone().map(|child| {
                    let key = firsts.get(child).copied();
                    let pointer = addresses.get(below + child).copied();
                    key.zip(pointer)
                        .ok_or_else(|| invalid("node child outside the level below"))
                });
                let children = children.collect::<io::Result<Vec<_>>>()?;
                keys.push(children.first().map(|(key, _)| *key).unwrap_or_default());
                blocks.push(node_block(&children, level as u8 + 1, address, header)?);
                next += 1;
            }
            below = next - nodes.len();
            firsts = keys;
        }
        Ok(blocks)
    }
}

fn header_bytes(header: &Header, address: u64, nritems: usize, level: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(NODESIZE as usize);
    out.extend_from_slice(&[0; 32]);
    out.extend_from_slice(&header.fsid);
    put64(&mut out, address);
    put64(&mut out, BLOCK_FLAGS);
    out.extend_from_slice(&header.chunk_tree_uuid);
    put64(&mut out, header.generation);
    put64(&mut out, header.owner);
    put32(&mut out, nritems as u32);
    out.push(level);
    out
}

fn leaf_block(items: &[Item], address: u64, header: &Header) -> io::Result<Vec<u8>> {
    let mut block = header_bytes(header, address, items.len(), 0);
    let mut end = LEAF_DATA;
    let mut data = Vec::with_capacity(items.len());
    for item in items {
        end = end
            .checked_sub(item.data.len())
            .ok_or_else(|| invalid("leaf overflow"))?;
        item.key.encode(&mut block);
        put32(&mut block, end as u32);
        put32(&mut block, item.data.len() as u32);
        data.push((end, &item.data));
    }
    if block.len() > HEADER_SIZE + end {
        return Err(invalid("leaf items overlap their data"));
    }
    block.resize(NODESIZE as usize, 0);
    for (at, bytes) in data {
        place(&mut block, HEADER_SIZE + at, bytes)?;
    }
    seal(&mut block)?;
    Ok(block)
}

fn node_block(
    children: &[(Key, u64)],
    level: u8,
    address: u64,
    header: &Header,
) -> io::Result<Vec<u8>> {
    let mut block = header_bytes(header, address, children.len(), level);
    for (key, pointer) in children {
        key.encode(&mut block);
        put64(&mut block, *pointer);
        put64(&mut block, header.generation);
    }
    if block.len() > NODESIZE as usize {
        return Err(invalid("node overflow"));
    }
    block.resize(NODESIZE as usize, 0);
    seal(&mut block)?;
    Ok(block)
}

/// Store a block's CRC-32C over everything after the checksum field.
pub fn seal(block: &mut [u8]) -> io::Result<()> {
    let sum = crc32c::checksum(block.get(32..).ok_or_else(|| invalid("short block"))?);
    place(block, 0, &[0; 32])?;
    place(block, 0, &sum.to_le_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::format::KEY_PTR_SIZE;

    fn items(count: usize, size: usize) -> Vec<Item> {
        (0..count)
            .map(|n| Item::new(Key::new(n as u64, 1, 0), vec![n as u8; size]))
            .collect()
    }

    #[test]
    fn an_empty_tree_is_one_empty_leaf() {
        let shape = Shape::of(&[]).unwrap();
        assert_eq!((shape.blocks(), shape.level()), (1, 0));
    }

    #[test]
    fn leaves_fill_greedily_and_nodes_stack() {
        // 1000-byte items: 16 fit a leaf (16 * 1025 = 16400 > 16283).
        let shape = Shape::of(&items(15, 1000)).unwrap();
        assert_eq!(shape.blocks(), 1);
        let shape = Shape::of(&items(16, 1000)).unwrap();
        assert_eq!((shape.blocks(), shape.level()), (3, 1));
        // 493 leaves fit one node; 494 need a second level.
        let per_leaf = 15;
        let shape = Shape::of(&items(per_leaf * 493, 1000)).unwrap();
        assert_eq!((shape.blocks(), shape.level()), (494, 1));
        let shape = Shape::of(&items(per_leaf * 494, 1000)).unwrap();
        assert_eq!((shape.blocks(), shape.level()), (494 + 2 + 1, 2));
        assert_eq!(shape.block_levels().last(), Some(&2));
    }

    #[test]
    fn an_oversized_item_is_refused() {
        assert!(Shape::of(&items(1, LEAF_DATA)).is_err());
        assert!(Shape::of(&items(1, LEAF_DATA - ITEM_SIZE)).is_ok());
    }

    #[test]
    fn serialized_blocks_carry_their_header_and_checksum() {
        let all = items(16, 1000);
        let shape = Shape::of(&all).unwrap();
        let header = Header {
            fsid: [1; 16],
            chunk_tree_uuid: [2; 16],
            generation: 3,
            owner: 5,
        };
        let blocks = shape.serialize(&all, &[100, 200, 300], &header).unwrap();
        assert_eq!(blocks.len(), 3);
        let root = &blocks[2];
        assert_eq!(crate::format::get64(root, 48).unwrap(), 300);
        assert_eq!(crate::format::get32(root, 96).unwrap(), 2);
        assert_eq!(root[100], 1);
        // Second child pointer: key of item 15, address 200.
        let second = HEADER_SIZE + KEY_PTR_SIZE;
        assert_eq!(Key::decode(root, second).unwrap(), Key::new(15, 1, 0));
        assert_eq!(crate::format::get64(root, second + 17).unwrap(), 200);
        for block in &blocks {
            let sum = crate::format::get32(block, 0).unwrap();
            assert_eq!(sum, crc32c::checksum(&block[32..]));
        }
        assert!(shape.serialize(&all, &[1, 2], &header).is_err());
    }
}
