//! Fuzzy/substring search over the prompt history.

/// State for the history search popup.
#[derive(Debug, Clone, Default)]
pub(crate) struct HistorySearch {
    pub query: String,
    pub selected: usize,
}

impl HistorySearch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return indices into `history` whose content contains `query` as a
    /// substring (case-insensitive). When `query` is empty, all entries match.
    pub fn filtered<'a>(&self, history: &'a [String]) -> Vec<(usize, &'a String)> {
        let q = self.query.to_lowercase();
        history
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, text)| q.is_empty() || text.to_lowercase().contains(&q))
            .map(|(idx, text)| (idx, text))
            .collect()
    }

    pub fn move_up(&mut self, count: usize) {
        self.selected = self.selected.saturating_sub(1);
        let _ = count;
    }

    pub fn move_down(&mut self, count: usize) {
        self.selected = (self.selected + 1).min(count.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_all_reversed() {
        let history = vec!["a".into(), "b".into(), "c".into()];
        let hs = HistorySearch::new();
        let filtered = hs.filtered(&history);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].1, "c");
        assert_eq!(filtered[2].1, "a");
    }

    #[test]
    fn substring_filter_is_case_insensitive() {
        let history = vec!["Hello World".into(), "goodbye".into(), "HELLO".into()];
        let mut hs = HistorySearch::new();
        hs.query = "hello".into();
        let filtered: Vec<_> = hs
            .filtered(&history)
            .into_iter()
            .map(|(_, t)| t.clone())
            .collect();
        assert!(filtered.contains(&"Hello World".into()));
        assert!(filtered.contains(&"HELLO".into()));
        assert!(!filtered.contains(&"goodbye".into()));
    }

    #[test]
    fn move_down_clamps_to_last() {
        let mut hs = HistorySearch::new();
        hs.move_down(3);
        hs.move_down(3);
        assert_eq!(hs.selected, 2);
    }

    #[test]
    fn move_up_saturates_at_zero() {
        let mut hs = HistorySearch::new();
        hs.move_down(3);
        hs.move_up(3);
        hs.move_up(3);
        assert_eq!(hs.selected, 0);
    }
}
