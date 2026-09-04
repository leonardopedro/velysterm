use std::ops::Range;

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>, // index into matches
    origin: usize,
}

impl SearchState {
    pub fn start(&mut self, origin: usize) {
        self.query.clear();
        self.matches.clear();
        self.current = None;
        self.origin = origin;
    }

    pub fn update_query(&mut self, text: &str, query: &str) {
        self.query = query.to_string();
        self.matches = find_matches(text, query);

        if self.matches.is_empty() {
            self.current = None;
        } else {
            self.current = self
                .matches
                .iter()
                .position(|r| r.start >= self.origin)
                .or(Some(0));
        }
    }

    pub fn next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let current_idx = self.current.unwrap_or(0);
        let next_idx = (current_idx + 1) % self.matches.len();
        self.current = Some(next_idx);
        self.origin = self.matches[next_idx].start;
    }

    pub fn prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let current_idx = self.current.unwrap_or(0);
        let prev_idx = if current_idx == 0 {
            self.matches.len() - 1
        } else {
            current_idx - 1
        };
        self.current = Some(prev_idx);
        self.origin = self.matches[prev_idx].start;
    }

    pub fn on_doc_changed(&mut self, text: &str) {
        self.matches = find_matches(text, &self.query);
        if self.matches.is_empty() {
            self.current = None;
        } else {
            self.current = self
                .matches
                .iter()
                .position(|r| r.start >= self.origin)
                .or(Some(0));
        }
    }
}

pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return vec![];
    }

    let is_case_insensitive = query.chars().all(|c| !c.is_uppercase());
    let mut matches = Vec::new();
    let mut cursor = 0;

    while cursor < text.len() {
        let remainder = &text[cursor..];
        let found = if is_case_insensitive {
            find_case_insensitive_range(remainder, query)
        } else {
            remainder.find(query).map(|o| o..o + query.len())
        };

        match found {
            Some(range) => {
                let start = cursor + range.start;
                let len = range.end - range.start;
                if len == 0 {
                    break;
                }
                matches.push(start..start + len);
                cursor = start + len;
            }
            None => break,
        }
    }

    matches
}

fn find_case_insensitive_range(text: &str, query: &str) -> Option<Range<usize>> {
    let q_chars: Vec<char> = query.chars().collect();
    if q_chars.is_empty() {
        return None;
    }

    let text_len = text.len();
    let mut search_pos = 0;

    while search_pos < text_len {
        let current_slice = &text[search_pos..];
        let mut match_found = true;
        let mut current_offset = 0;

        let mut text_iter = current_slice.chars();
        for &q_char in &q_chars {
            if let Some(t_char) = text_iter.next() {
                if t_char.to_lowercase().next() != Some(q_char.to_lowercase().next().unwrap()) {
                    match_found = false;
                    break;
                }
                current_offset += t_char.len_utf8();
            } else {
                match_found = false;
                break;
            }
        }

        if match_found {
            return Some(search_pos..search_pos + current_offset);
        }

        // Advance search_pos to next char boundary
        if let Some(c) = current_slice.chars().next() {
            search_pos += c.len_utf8();
        } else {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_case_sensitivity() {
        // Case-insensitive: query is all lowercase
        assert_eq!(find_matches("Hello World", "hello"), vec![0..5]);
        assert_eq!(
            find_matches("Hello World", "WORLD"),
            Vec::<Range<usize>>::new()
        ); // Query has uppercase -> case-sensitive
        assert_eq!(find_matches("Hello World", "Hello"), vec![0..5]);
    }

    #[test]
    fn test_non_overlapping() {
        // "aaaa" query "aa" -> 0..2, 2..4
        assert_eq!(find_matches("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn test_multibyte() {
        // "αβα" query "α" -> two matches
        let text = "αβα";
        let matches = find_matches(text, "α");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], 0..2);
        assert_eq!(matches[1], 4..6);
    }

    #[test]
    fn test_search_state_navigation() {
        let mut state = SearchState::default();
        state.start(0);
        state.update_query("aaaa", "aa");

        assert_eq!(state.current, Some(0));
        state.next();
        assert_eq!(state.current, Some(1));
        state.next();
        assert_eq!(state.current, Some(0)); // wraparound
        state.prev();
        assert_eq!(state.current, Some(1)); // wraparound
    }

    #[test]
    fn test_origin_retention() {
        let mut state = SearchState::default();
        state.start(0);
        state.update_query("ab", "ab");
        assert_eq!(state.current, Some(0));

        // User extends query to "abc"
        state.update_query("abc", "abc");
        assert_eq!(state.current, Some(0));
    }

    #[test]
    fn test_on_doc_changed() {
        let mut state = SearchState::default();
        state.start(0);
        state.update_query("apple banana", "a");
        assert_eq!(state.matches.len(), 4);
        state.current = Some(2); // at 'a' in banana
        state.origin = state.matches[2].start;

        state.on_doc_changed("apple bnanana");
        assert_eq!(state.matches.len(), 4);
        // Should still be at 'a' in bnanana
        assert!(state.current.is_some());
        assert!(state.matches[state.current.unwrap()].start >= state.origin);
    }
}
