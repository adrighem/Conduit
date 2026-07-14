use std::collections::{HashMap, HashSet};

use crate::search::{SearchField, SearchQuery, PRIMARY_FIELD_WEIGHT, SECONDARY_FIELD_WEIGHT};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmojiValue {
    Unicode(&'static str),
    CustomImage(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiEntry {
    pub name: String,
    pub label: String,
    pub category: &'static str,
    pub value: EmojiValue,
}

pub struct EmojiCatalog<'a> {
    custom: &'a HashMap<String, String>,
}

impl<'a> EmojiCatalog<'a> {
    pub fn new(custom: &'a HashMap<String, String>) -> Self {
        Self { custom }
    }

    pub fn resolve(&self, name: &str) -> Option<EmojiValue> {
        self.resolve_with_seen(name, &mut HashSet::new())
    }

    fn resolve_with_seen(&self, name: &str, seen: &mut HashSet<String>) -> Option<EmojiValue> {
        if !seen.insert(name.to_string()) {
            return None;
        }
        if let Some(value) = self.custom.get(name) {
            if let Some(target) = value.strip_prefix("alias:") {
                return self.resolve_with_seen(target, seen);
            }
            if value.starts_with("https://") || value.starts_with("http://") {
                return Some(EmojiValue::CustomImage(value.clone()));
            }
        }
        emojis::get_by_shortcode(name).map(|emoji| EmojiValue::Unicode(emoji.as_str()))
    }

    pub fn entries(&self) -> Vec<EmojiEntry> {
        let mut entries = emojis::iter()
            .filter_map(|emoji| {
                Some(EmojiEntry {
                    name: emoji.shortcode()?.to_string(),
                    label: emoji.name().to_string(),
                    category: category_label(emoji.group()),
                    value: EmojiValue::Unicode(emoji.as_str()),
                })
            })
            .collect::<Vec<_>>();

        let mut custom_names = self.custom.keys().cloned().collect::<Vec<_>>();
        custom_names.sort_by_key(|name| name.to_lowercase());
        entries.extend(custom_names.into_iter().filter_map(|name| {
            Some(EmojiEntry {
                label: name.replace(['_', '-'], " "),
                value: self.resolve(&name)?,
                name,
                category: "Workspace",
            })
        }));
        entries
    }
}

/// Widget-independent emoji picker data. Both the native composer popover and
/// the WebView reaction picker are rendered from this model.
#[derive(Debug, Clone)]
pub struct EmojiPickerModel {
    entries: Vec<EmojiEntry>,
}

impl EmojiPickerModel {
    pub fn new(entries: Vec<EmojiEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[EmojiEntry] {
        &self.entries
    }

    pub fn search(&self, query: &str) -> Vec<EmojiEntry> {
        let query = SearchQuery::parse(query);
        let mut matches = self
            .entries()
            .iter()
            .cloned()
            .enumerate()
            .filter_map(|(index, entry)| {
                let score = query.score([
                    SearchField::new(&entry.name, PRIMARY_FIELD_WEIGHT),
                    SearchField::new(&entry.label, SECONDARY_FIELD_WEIGHT),
                ])?;
                Some((score.band(), index, entry))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        matches.into_iter().map(|(_, _, entry)| entry).collect()
    }
}

pub fn emoji_picker_accessible_label(entry: &EmojiEntry) -> String {
    format!(":{}: — {}", entry.name, entry.label)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmojiPickerMove {
    Previous,
    Next,
}

/// Shared, clamped selection behavior for every emoji picker frontend.
pub fn move_emoji_picker_selection(
    selected: Option<usize>,
    item_count: usize,
    movement: EmojiPickerMove,
) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    let current = selected.unwrap_or(0).min(item_count - 1);
    Some(match movement {
        EmojiPickerMove::Previous => current.saturating_sub(1),
        EmojiPickerMove::Next => (current + 1).min(item_count - 1),
    })
}

fn category_label(group: emojis::Group) -> &'static str {
    match group {
        emojis::Group::SmileysAndEmotion => "Smileys",
        emojis::Group::PeopleAndBody => "People",
        emojis::Group::AnimalsAndNature => "Nature",
        emojis::Group::FoodAndDrink => "Food",
        emojis::Group::TravelAndPlaces => "Travel",
        emojis::Group::Activities => "Activities",
        emojis::Group::Objects => "Objects",
        emojis::Group::Symbols => "Symbols",
        emojis::Group::Flags => "Flags",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_resolves_unicode_custom_and_alias_emoji() {
        let custom = HashMap::from([
            (
                "party_parrot".to_string(),
                "https://emoji.example/parrot.gif".to_string(),
            ),
            ("parrot_alias".to_string(), "alias:party_parrot".to_string()),
            ("ship_it".to_string(), "alias:rocket".to_string()),
        ]);
        let catalog = EmojiCatalog::new(&custom);

        assert_eq!(catalog.resolve("rocket"), Some(EmojiValue::Unicode("🚀")));
        assert_eq!(
            catalog.resolve("parrot_alias"),
            Some(EmojiValue::CustomImage(
                "https://emoji.example/parrot.gif".to_string()
            ))
        );
        assert_eq!(catalog.resolve("ship_it"), Some(EmojiValue::Unicode("🚀")));
    }

    #[test]
    fn catalog_rejects_alias_cycles() {
        let custom = HashMap::from([
            ("one".to_string(), "alias:two".to_string()),
            ("two".to_string(), "alias:one".to_string()),
        ]);
        assert_eq!(EmojiCatalog::new(&custom).resolve("one"), None);
    }

    #[test]
    fn catalog_searches_shortcodes_labels_and_workspace_emoji() {
        let custom = HashMap::from([
            (
                "party_parrot".to_string(),
                "https://emoji.example/parrot.gif".to_string(),
            ),
            ("ship_it".to_string(), "alias:rocket".to_string()),
        ]);
        let catalog = EmojiCatalog::new(&custom);
        let model = EmojiPickerModel::new(catalog.entries());

        assert!(model
            .search("party parr")
            .iter()
            .any(|entry| entry.name == "party_parrot"));
        assert!(model
            .search("ship it")
            .iter()
            .any(|entry| entry.name == "ship_it"));
        assert!(model
            .search("grinning face")
            .iter()
            .any(|entry| entry.name == "grinning"));
        assert!(model.search("definitely-not-an-emoji").is_empty());
    }

    #[test]
    fn catalog_search_prioritizes_stronger_shortcode_matches() {
        let custom = HashMap::from([
            (
                "parrot".to_string(),
                "https://emoji.example/parrot.gif".to_string(),
            ),
            (
                "party_parrot".to_string(),
                "https://emoji.example/party-parrot.gif".to_string(),
            ),
        ]);
        let matches = EmojiPickerModel::new(EmojiCatalog::new(&custom).entries()).search("parrot");

        assert_eq!(
            matches.first().map(|entry| entry.name.as_str()),
            Some("parrot")
        );
    }

    #[test]
    fn picker_model_preserves_catalog_filtering_and_accessible_labels() {
        let custom = HashMap::from([
            (
                "parrot".to_string(),
                "https://emoji.example/parrot.gif".to_string(),
            ),
            (
                "party_parrot".to_string(),
                "https://emoji.example/party.gif".to_string(),
            ),
        ]);
        let model = EmojiPickerModel::new(EmojiCatalog::new(&custom).entries());
        let matches = model.search("parrot");

        assert_eq!(matches[0].name, "parrot");
        assert_eq!(
            emoji_picker_accessible_label(&matches[0]),
            ":parrot: — parrot"
        );
    }

    #[test]
    fn picker_selection_is_clamped_for_both_directions() {
        assert_eq!(
            move_emoji_picker_selection(Some(1), 3, EmojiPickerMove::Previous),
            Some(0)
        );
        assert_eq!(
            move_emoji_picker_selection(Some(1), 3, EmojiPickerMove::Next),
            Some(2)
        );
        assert_eq!(
            move_emoji_picker_selection(Some(0), 3, EmojiPickerMove::Previous),
            Some(0)
        );
        assert_eq!(
            move_emoji_picker_selection(Some(2), 3, EmojiPickerMove::Next),
            Some(2)
        );
        assert_eq!(
            move_emoji_picker_selection(Some(0), 0, EmojiPickerMove::Next),
            None
        );
    }
}
