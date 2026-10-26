//! Slack's standard emoji names, generated from emoji-datasource by
//! `scripts/update-slack-emoji-names.py`.

use std::collections::HashMap;
use std::sync::LazyLock;

const TABLE: &str = include_str!("slack_names.tsv");

struct SlackNames {
    /// Any Slack name -> (emoji, canonical Slack name).
    by_name: HashMap<&'static str, (&'static str, &'static str)>,
    /// Emoji without variation selectors -> canonical Slack name.
    by_emoji: HashMap<String, &'static str>,
}

static SLACK_NAMES: LazyLock<SlackNames> = LazyLock::new(|| {
    let mut by_name = HashMap::new();
    let mut by_emoji = HashMap::new();
    for line in TABLE.lines().filter(|line| !line.starts_with('#')) {
        let (emoji, names) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("malformed Slack emoji line: {line:?}"));
        let mut names = names.split(',');
        let canonical = names.next().expect("split yields at least one item");
        for name in std::iter::once(canonical).chain(names) {
            by_name.insert(name, (emoji, canonical));
        }
        by_emoji.insert(without_variation_selectors(emoji), canonical);
    }
    SlackNames { by_name, by_emoji }
});

fn without_variation_selectors(emoji: &str) -> String {
    emoji.chars().filter(|c| *c != '\u{fe0f}').collect()
}

pub(super) fn emoji(name: &str) -> Option<&'static str> {
    SLACK_NAMES.by_name.get(name).map(|(emoji, _)| *emoji)
}

pub(super) fn canonical_name(name: &str) -> Option<&'static str> {
    SLACK_NAMES
        .by_name
        .get(name)
        .map(|(_, canonical)| *canonical)
}

pub(super) fn name_for_emoji(emoji: &str) -> Option<&'static str> {
    SLACK_NAMES
        .by_emoji
        .get(&without_variation_selectors(emoji))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_well_formed_and_names_are_unique() {
        let rows = TABLE.lines().filter(|line| !line.starts_with('#'));
        let name_count = rows
            .map(|line| {
                line.split_once('\t')
                    .expect("tab separated")
                    .1
                    .split(',')
                    .count()
            })
            .sum::<usize>();
        assert!(name_count > 1900);
        assert_eq!(SLACK_NAMES.by_name.len(), name_count);
    }

    #[test]
    fn looks_up_names_aliases_and_emoji() {
        assert_eq!(emoji("flag-nl"), Some("🇳🇱"));
        assert_eq!(emoji("thumbsup"), Some("👍"));
        assert_eq!(canonical_name("thumbsup"), Some("+1"));
        assert_eq!(name_for_emoji("☂"), Some("umbrella"));
        assert_eq!(name_for_emoji("☂️"), Some("umbrella"));
        assert_eq!(emoji("hugs"), None);
    }
}
