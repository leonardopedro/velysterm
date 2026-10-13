use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::ops::Range;

/// Stable identifier for a block of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// Returns the damage report specifying which blocks changed or
    /// were removed.
    pub fn update(&mut self, text: &str) -> BlockDamage {
        let new_ranges = split_blocks(text);
        let mut new_blocks = Vec::with_capacity(new_ranges.len());
        let mut damage = BlockDamage::default();

        // 1. Calculate hashes and initial IDs for new blocks
        for range in new_ranges {
            let content = &text[range.clone()];
            let mut s = std::collections::hash_map::DefaultHasher::new();
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
            if let Some(old) = old_blocks
                .iter()
                .find(|o| o.range == new_block.range && o.hash == new_block.hash)
            {
                new_block.id = old.id;
                matched_old.insert(old.id);
                continue;
            }

            // Try fingerprint match (same hash, nearby position)
            if let Some(old) = old_blocks.iter().find(|o| o.hash == new_block.hash) {
                // If the block is reasonably close to the old one,
                // assume it's the same block moved
                let dist = (new_block.range.start as isize - old.range.start as isize).abs();
                if dist < 1000 {
                    new_block.id = old.id;
                    matched_old.insert(old.id);
                    continue;
                }
            }
        }

        // 3. Fallback: Positional match for blocks that changed
        //    content
        let mut old_idx = 0;
        for new_block in &mut new_blocks {
            if matched_old.contains(&new_block.id) {
                continue;
            }

            while old_idx < old_blocks.len() && matched_old.contains(&old_blocks[old_idx].id) {
                old_idx += 1;
            }

            if old_idx < old_blocks.len() {
                new_block.id = old_blocks[old_idx].id;
                matched_old.insert(old_blocks[old_idx].id);
                old_idx += 1;
            } else {
                // Truly new block: generate unique ID based on range
                // and text
                let mut s = std::collections::hash_map::DefaultHasher::new();
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
            if let Some(old) = old_blocks.iter().find(|o| o.id == new.id) {
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

    // A `\slint` block holds Slint DSL, which is multi-line and brace
    // delimited. The usual blank-line split is therefore suspended until the
    // component's braces have opened and balanced again, so a widget can
    // contain blank lines without being shredded into Typst blocks.
    //
    // `slint_body_seen` keeps a blank line between the `\slint` marker and
    // the component from ending the block before it starts. The comment and
    // string flags make brace counting ignore `// comments` and `"..."`.
    let mut in_slint = false;
    let mut slint_depth: i32 = 0;
    let mut slint_body_seen = false;
    let mut slint_in_comment = false;
    let mut slint_in_string = false;
    // `import { A, B } from "x.slint";` opens and closes braces without
    // declaring a component, so its braces must not count — otherwise a blank
    // line between the imports and the component would split the block.
    let mut slint_in_import = false;

    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        // Brace/string/comment bookkeeping runs before every other rule so
        // that a `\` or `$` inside a Slint string cannot derail it.
        if in_slint {
            if slint_in_comment {
                if c == '\n' {
                    slint_in_comment = false;
                }
            } else if slint_in_string {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' | '\n' => slint_in_string = false,
                    _ => {}
                }
            } else {
                match c {
                    '"' => slint_in_string = true,
                    '/' if chars.peek().is_some_and(|(_, p)| *p == '/') => {
                        slint_in_comment = true;
                    }
                    'i' if !slint_in_import
                            // Not the tail of an identifier such as `reimport`.
                            && (i == 0 || {
                                let prev = text.as_bytes()[i - 1];
                                !(prev.is_ascii_alphanumeric() || prev == b'_')
                            })
                            && text[i..].starts_with("import")
                            && text[i + 6..]
                                .chars()
                                .next()
                                .is_none_or(|ch| ch.is_whitespace() || ch == '{') =>
                    {
                        slint_in_import = true;
                    }
                    ';' => slint_in_import = false,
                    '{' if !slint_in_import => {
                        slint_depth += 1;
                        slint_body_seen = true;
                    }
                    '}' if !slint_in_import => slint_depth = (slint_depth - 1).max(0),
                    _ => {}
                }
            }
        }

        if c == '\\' {
            if current_block_start.is_none() {
                current_block_start = Some(i);
                // `\slint` opens an inline Slint block, but only at a token
                // boundary so prose such as `\slintly` is not captured.
                if let Some(after) = text[i + 1..].strip_prefix("slint")
                    && matches!(after.chars().next(), None | Some(' ' | '\t' | '\r' | '\n'))
                {
                    in_slint = true;
                    slint_depth = 0;
                    slint_body_seen = false;
                    slint_in_comment = false;
                    slint_in_string = false;
                    slint_in_import = false;
                    // A `$` opened by a preceding block must not leak in.
                    in_math = false;
                }
            }
            chars.next();
            continue;
        }

        if c == '$' && !in_slint {
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
                let temp_chars = text[i + 1..].char_indices().peekable();
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

            // A `\slint` block only ends once its component's braces have
            // opened and balanced again.
            let slint_settled = !in_slint || (slint_body_seen && slint_depth == 0);

            if is_blank && slint_settled {
                if let Some(start) = current_block_start {
                    let mut end = i;
                    while end > start
                        && (text.as_bytes()[end - 1] == b'\n'
                            || text.as_bytes()[end - 1].is_ascii_whitespace())
                    {
                        end -= 1;
                    }
                    if end > start {
                        blocks.push(start..end);
                    }
                }
                current_block_start = None;
                in_slint = false;
                slint_depth = 0;
                slint_body_seen = false;
                slint_in_comment = false;
                slint_in_string = false;
                slint_in_import = false;

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

        // Slint assignments can legitimately begin a line, so the `=` rule
        // never applies inside a `\slint` block (it ends on a blank line).
        if c == '=' && !in_slint && (i == 0 || text.as_bytes()[i - 1] == b'\n') {
            if let Some(start) = current_block_start {
                let mut end = i;
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
            current_block_start = Some(i);
        } else if current_block_start.is_none() && !c.is_whitespace() {
            current_block_start = Some(i);
        }
    }

    if let Some(start) = current_block_start {
        let mut end = text.len();
        while end > start {
            if text.as_bytes()[end - 1] == b'\n' || text.as_bytes()[end - 1].is_ascii_whitespace() {
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

    /// Content of each block, for readable assertions.
    fn contents(text: &str) -> Vec<String> {
        split_blocks(text)
            .into_iter()
            .map(|r| text[r].to_string())
            .collect()
    }

    #[test]
    fn test_non_slint_docs_are_unchanged() {
        // Prose, headings and math split exactly as before: `\slint` handling
        // must not perturb the ordinary rules.
        let text = "= title\n\nFirst paragraph.\n\n$ x + y $\n\n\\function(#1,#2)\n\nLast line.";
        assert_eq!(
            contents(text),
            vec![
                "= title",
                "First paragraph.",
                "$ x + y $",
                "\\function(#1,#2)",
                "Last line.",
            ]
        );
    }

    #[test]
    fn test_slint_block_survives_blank_lines_and_braces() {
        let text = "intro\n\n\\slint\nexport component Demo inherits Window {\n    VerticalBox {\n        Button { text: \"ok\"; }\n    }\n\n    // a comment, and a blank line above, with an unbalanced { in it\n}\n\noutro";
        assert_eq!(
            contents(text),
            vec![
                "intro",
                "\\slint\nexport component Demo inherits Window {\n    VerticalBox {\n        Button { text: \"ok\"; }\n    }\n\n    // a comment, and a blank line above, with an unbalanced { in it\n}",
                "outro",
            ]
        );
    }

    #[test]
    fn test_slint_block_ends_after_balanced_braces() {
        // The block ends at the first blank line *after* the component's
        // braces balance — the second widget is a separate block.
        let text = "\\slint\nexport component A inherits Window { Rectangle {} }\n\n\\slint\nexport component B inherits Window { Text { text: \"b\"; } }";
        assert_eq!(contents(text).len(), 2);
        assert!(contents(text)[0].contains("component A"));
        assert!(contents(text)[1].contains("component B"));
    }

    #[test]
    fn test_slint_requires_token_boundary() {
        // `\slintly` is prose, not a Slint marker: it must not put the
        // scanner into Slint mode.
        let text = "\\slintly is a word\n\nparagraph";
        assert_eq!(contents(text), vec!["\\slintly is a word", "paragraph"]);
    }

    #[test]
    fn test_slint_imports_do_not_end_the_block() {
        // `import { … } from "…";` balances its braces before the component is
        // declared, so a blank line between the imports and the component must
        // not split the block.
        let text = "\\slint\nimport { VerticalBox, Button } from \"std-widgets.slint\";\n\nexport component Demo inherits Window {\n    VerticalBox { Button { text: \"ok\"; } }\n}\n\nafter";
        let blocks = contents(text);
        assert_eq!(blocks.len(), 2, "got {blocks:?}");
        assert!(blocks[0].contains("component Demo"));
        assert!(blocks[0].contains("import { VerticalBox"));
        assert_eq!(blocks[1], "after");
    }

    #[test]
    fn test_slint_string_escapes_do_not_unbalance_braces() {
        // `\"` inside a string must not toggle the string, and braces inside
        // the string must not affect the depth.
        let text = "\\slint\nexport component C inherits Window {\n    Text { text: \"} { \" \" }\"; }\n}\n\nafter";
        assert_eq!(contents(text).len(), 2);
        assert!(contents(text)[0].contains("component C"));
        assert_eq!(contents(text)[1], "after");
    }
}
