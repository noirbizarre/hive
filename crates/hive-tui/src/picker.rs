use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::widgets::ListState;

pub type Choice = (String, String, String);

pub struct Picker {
    pub session: String,
    pub query: String,
    pub list: ListState,
    pub choices: Vec<Choice>,
    pub pending: bool,
    pub message: String,
    source: Vec<Choice>,
    matched_query: String,
}

impl Picker {
    pub fn new(session: String, choices: Vec<Choice>) -> Self {
        let selected = choices.iter().position(|choice| choice.2.ends_with(" *"));
        let mut picker = Self {
            session,
            query: String::new(),
            list: ListState::default().with_selected(selected),
            choices,
            pending: false,
            message: String::new(),
            source: Vec::new(),
            matched_query: String::new(),
        };
        picker.refresh(picker.choices.clone());
        picker
    }

    pub fn refresh(&mut self, choices: Vec<Choice>) {
        // Redraws happen every 50 ms; unchanged snapshots must not repeatedly rescore the catalog.
        if choices == self.source && self.query == self.matched_query {
            return;
        }
        let query_changed = self.query != self.matched_query;
        self.source.clone_from(&choices);
        self.matched_query.clone_from(&self.query);
        let identity = self.selected().map(|c| (c.0.clone(), c.1.clone()));
        let previous = self.list.selected().unwrap_or(0);
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buffer = Vec::new();
        let mut ranked: Vec<_> = choices
            .into_iter()
            .filter_map(|choice| {
                // The current-value decoration must not influence search ranking.
                let label = choice.2.trim_end_matches(" *");
                pattern
                    .score(Utf32Str::new(label, &mut buffer), &mut matcher)
                    .map(|score| (score, choice))
            })
            .collect();
        // Stable ties retain the agent's advertised order, including the empty query.
        ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.choices = ranked.into_iter().map(|(_, choice)| choice).collect();
        let selected = identity
            .and_then(|(config, value)| {
                self.choices
                    .iter()
                    .position(|c| c.0 == config && c.1 == value)
            })
            .unwrap_or_else(|| {
                if query_changed {
                    0
                } else {
                    previous.min(self.choices.len().saturating_sub(1))
                }
            });
        self.list
            .select((!self.choices.is_empty()).then_some(selected));
    }

    pub fn selected(&self) -> Option<&Choice> {
        self.list
            .selected()
            .and_then(|index| self.choices.get(index))
    }

    pub fn navigate(&mut self, amount: isize) {
        if !self.choices.is_empty() {
            self.list.select(Some(
                self.list
                    .selected()
                    .unwrap_or(0)
                    .saturating_add_signed(amount)
                    .min(self.choices.len() - 1),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<Choice> {
        vec![
            ("model".into(), "fast".into(), "Provider / Fast *".into()),
            (
                "model".into(),
                "careful".into(),
                "Provider / Careful".into(),
            ),
            ("other".into(), "cafe".into(), "Café / Über".into()),
        ]
    }

    #[test]
    fn fuzzy_queries_match_noncontiguous_labels_and_unicode_without_changing_ids() {
        let mut picker = Picker::new("session".into(), choices());
        picker.query = "PRV CRFL".into();
        picker.refresh(choices());
        assert_eq!(picker.choices.len(), 1);
        assert_eq!(picker.selected().unwrap().1, "careful");
        picker.query = "cafe uber".into();
        picker.refresh(choices());
        assert_eq!(picker.selected().unwrap().0, "other");
        assert_eq!(picker.selected().unwrap().1, "cafe");
    }

    #[test]
    fn refreshes_preserve_identity_and_removed_choices_have_a_valid_fallback() {
        let mut picker = Picker::new("session".into(), choices());
        picker.navigate(1);
        let mut reordered = choices();
        reordered.reverse();
        picker.refresh(reordered.clone());
        assert_eq!(picker.selected().unwrap().1, "careful");
        reordered.retain(|c| c.1 != "careful");
        picker.refresh(reordered);
        assert!(picker.selected().is_some());
        picker.query = "no such model".into();
        picker.refresh(choices());
        assert!(picker.selected().is_none());
        picker.navigate(10);
        assert!(picker.list.selected().is_none());
        picker.query.clear();
        picker.refresh(choices());
        assert_eq!(picker.selected().unwrap().1, "fast");
    }

    #[test]
    fn equal_scores_preserve_advertised_order_and_queries_keep_matching_selections() {
        let rows = vec![
            ("a".into(), "one".into(), "Same".into()),
            ("b".into(), "two".into(), "Same".into()),
        ];
        let mut picker = Picker::new("session".into(), rows.clone());
        picker.navigate(1);
        picker.query = "sm".into();
        picker.refresh(rows.clone());
        assert_eq!(picker.choices, rows);
        assert_eq!(picker.selected().unwrap().1, "two");
    }
}
