use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::ops::Range;

/// Stable identifier for a block of text.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct BlockId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: BlockId,
    pub range: Range<usize>,
    pub hash: u64,
}

#[derive(Debug, Default, Clone)]
pub struct BlockIndex {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Default)]
pub struct BlockDamage {
    pub dirty: HashSet<BlockId>,
    pub removed: HashSet<BlockId>,
}

impl BlockIndex {
    /// Updates the block index based on the new document text.
    /// Returns the damage report specifying which blocks changed or were removed.
    pub fn update(&mut self, text: &str) -> BlockDamage {
        let new_ranges = split_blocks(text);
        let mut new_blocks = Vec::with_capacity(new_ranges.len());
        let mut damage = BlockDamage::default();

        // 1. Calculate hashes and initial IDs for new blocks
        for range in new_ranges {
            let content = &text[range.clone()];
            let mut s =
                std::collections::hash_map::DefaultHasher::new();
            content.hash(&mut s);
            let hash = s.finish();

            // Temporary ID (will be stabilized in step 2)
            new_blocks.push(Block {
                id: BlockId(hash),
                range,
                hash,
            });
        }

        // 2. Stabilize IDs by matching against old blocks
        let old_blocks = self.blocks.clone();
        let mut matched_old = HashSet::new();

        for new_block in &mut new_blocks {
            // Try exact match (range + hash)
            if let Some(old) = old_blocks.iter().find(|o| {
                o.range == new_block.range && o.hash == new_block.hash
            }) {
                new_block.id = old.id;
                matched_old.insert(old.id);
                continue;
            }

            // Try fingerprint match (same hash, nearby position)
            if let Some(old) =
                old_blocks.iter().find(|o| o.hash == new_block.hash)
            {
                // If the block is reasonably close to the old one, assume it's the same block moved
                let dist = (new_block.range.start as isize
                    - old.range.start as isize)
                    .abs();
                if dist < 1000 {
                    new_block.id = old.id;
                    matched_old.insert(old.id);
                    continue;
                }
            }
        }

        // 3. Fallback: Positional match for blocks that changed content
        let mut old_idx = 0;
        for new_block in &mut new_blocks {
            if matched_old.contains(&new_block.id) {
                continue;
            }

            while old_idx < old_blocks.len()
                && matched_old.contains(&old_blocks[old_idx].id)
            {
                old_idx += 1;
            }

            if old_idx < old_blocks.len() {
                new_block.id = old_blocks[old_idx].id;
                matched_old.insert(old_blocks[old_idx].id);
                old_idx += 1;
            } else {
                // Truly new block: generate unique ID based on range and text
                let mut s =
                    std::collections::hash_map::DefaultHasher::new();
                new_block.range.start.hash(&mut s);
                new_block.hash.hash(&mut s);
                new_block.id = BlockId(s.finish());
            }
        }

        // 4. Identify damage
        for old in &old_blocks {
            if !matched_old.contains(&old.id) {
                damage.removed.insert(old.id);
            }
        }

        for new in &new_blocks {
            if let Some(old) =
                old_blocks.iter().find(|o| o.id == new.id)
            {
                if old.hash != new.hash {
                    damage.dirty.insert(new.id);
                }
            } else {
                damage.dirty.insert(new.id);
            }
        }

        self.blocks = new_blocks;
        damage
    }
}

pub fn split_blocks(text: &str) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut current_block_start: Option<usize> = None;
    let mut in_math = false;
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            if current_block_start.is_none() {
                current_block_start = Some(i);
            }
            chars.next();
            continue;
        }

        if c == '$' {
            in_math = !in_math;
            if current_block_start.is_none() {
                current_block_start = Some(i);
            }
            continue;
        }

        if in_math {
            continue;
        }

        if c == '\n' {
            let is_blank = {
                let mut blank = true;
                let temp_chars =
                    text[i + 1..].char_indices().peekable();
                for (_, ch) in temp_chars {
                    if ch == '\n' {
                        break;
                    }
                    if !ch.is_whitespace() {
                        blank = false;
                        break;
                    }
                }
                blank
            };

            if is_blank {
                if let Some(start) = current_block_start {
                    let mut end = i;
                    while end > start
                        && (text.as_bytes()[end - 1] == b'\n'
                            || text.as_bytes()[end - 1]
                                .is_ascii_whitespace())
                    {
                        end -= 1;
                    }
                    if end > start {
                        blocks.push(start..end);
                    }
                }
                current_block_start = None;

                while let Some((_, ch)) = chars.peek() {
                    if *ch == '\n' || (*ch).is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
        }

        if c == '=' && (i == 0 || text.as_bytes()[i - 1] == b'\n') {
            if let Some(start) = current_block_start {
                let mut end = i;
                while end > start {
                    if text.as_bytes()[end - 1] == b'\n'
                        || text.as_bytes()[end - 1]
                            .is_ascii_whitespace()
                    {
                        end -= 1;
                    } else {
                        break;
                    }
                }
                if end > start {
                    blocks.push(start..end);
                }
            }
            current_block_start = Some(i);
        } else if current_block_start.is_none() && !c.is_whitespace()
        {
            current_block_start = Some(i);
        }
    }

    if let Some(start) = current_block_start {
        let mut end = text.len();
        while end > start {
            if text.as_bytes()[end - 1] == b'\n'
                || text.as_bytes()[end - 1].is_ascii_whitespace()
            {
                end -= 1;
            } else {
                break;
            }
        }
        if end > start {
            blocks.push(start..end);
        }
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_stability() {
        let mut index = BlockIndex::default();
        let text1 = "Line 1\n\nLine 2";
        let damage1 = index.update(text1);

        assert_eq!(index.blocks.len(), 2);
        let id1 = index.blocks[0].id;
        let id2 = index.blocks[1].id;
        assert!(damage1.dirty.contains(&id1));
        assert!(damage1.dirty.contains(&id2));

        // Change content of Line 1
        let text2 = "Line 1 changed\n\nLine 2";
        let damage2 = index.update(text2);

        assert_eq!(index.blocks.len(), 2);
        assert_eq!(index.blocks[0].id, id1);
        assert_eq!(index.blocks[1].id, id2);
        assert!(damage2.dirty.contains(&id1));
        assert!(!damage2.dirty.contains(&id2));
        assert!(damage2.removed.is_empty());
    }
}
