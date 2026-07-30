use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::search::{SearchField, SearchQuery, PRIMARY_FIELD_WEIGHT, SECONDARY_FIELD_WEIGHT};

static UNICODE_BY_CANONICAL_NAME: LazyLock<HashMap<String, Option<&'static emojis::Emoji>>> =
    LazyLock::new(|| {
        let mut by_name = HashMap::new();
        for emoji in emojis::iter() {
            let name = canonical_emoji_name(emoji.name());
            by_name
                .entry(name)
                .and_modify(|resolved| *resolved = None)
                .or_insert(Some(emoji));
        }
        by_name
    });

pub const EMOJI_PICKER_PROTOCOL_VERSION: u8 = 1;
pub const EMOJI_PICKER_RESULT_LIMIT: usize = 64;
pub const EMOJI_PICKER_MAX_QUERY_CHARS: usize = 128;
pub const EMOJI_PICKER_CATEGORIES: &[&str] = &[
    "Smileys",
    "People",
    "Nature",
    "Food",
    "Travel",
    "Activities",
    "Objects",
    "Symbols",
    "Flags",
    "Workspace",
];

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
        emojis::get_by_shortcode(name)
            .or_else(|| {
                UNICODE_BY_CANONICAL_NAME
                    .get(&canonical_emoji_name(name))
                    .copied()
                    .flatten()
            })
            .map(|emoji| EmojiValue::Unicode(emoji.as_str()))
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

fn canonical_emoji_name(name: &str) -> String {
    let mut canonical = String::with_capacity(name.len());
    let mut pending_separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !canonical.is_empty() {
                canonical.push('_');
            }
            canonical.push(character.to_ascii_lowercase());
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    canonical
}

/// Widget-independent emoji picker data. Both the native composer popover and
/// the WebView reaction picker are rendered from this model.
#[derive(Debug, Clone)]
pub struct EmojiPickerModel {
    entries: Vec<EmojiEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmojiPickerQuery {
    pub version: u8,
    pub generation: u64,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmojiPickerResult {
    pub version: u8,
    pub generation: u64,
    pub offset: usize,
    pub total: usize,
    pub has_previous: bool,
    pub has_more: bool,
    pub entries: Vec<EmojiPickerResultEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmojiPickerResultEntry {
    pub name: String,
    pub label: String,
    pub category: &'static str,
    pub accessible_label: String,
    pub value_kind: EmojiPickerResultValueKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmojiPickerResultValueKind {
    Unicode,
    CustomImage,
}

impl From<&EmojiEntry> for EmojiPickerResultEntry {
    fn from(entry: &EmojiEntry) -> Self {
        let (value_kind, value) = match &entry.value {
            EmojiValue::Unicode(value) => {
                (EmojiPickerResultValueKind::Unicode, (*value).to_string())
            }
            EmojiValue::CustomImage(value) => {
                (EmojiPickerResultValueKind::CustomImage, value.clone())
            }
        };
        Self {
            name: entry.name.clone(),
            label: entry.label.clone(),
            category: entry.category,
            accessible_label: emoji_picker_accessible_label(entry),
            value_kind,
            value,
        }
    }
}

#[derive(Debug, Default)]
pub struct EmojiPickerGenerationGate {
    latest: u64,
}

impl EmojiPickerGenerationGate {
    pub fn accept(&mut self, generation: u64) -> bool {
        if generation == 0 || generation <= self.latest {
            return false;
        }
        self.latest = generation;
        true
    }
}

impl EmojiPickerModel {
    pub fn new(entries: Vec<EmojiEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[EmojiEntry] {
        &self.entries
    }

    pub fn search(&self, query: &str) -> Vec<EmojiEntry> {
        self.ranked_entries(query).into_iter().cloned().collect()
    }

    pub fn query(&self, request: &EmojiPickerQuery) -> Option<EmojiPickerResult> {
        if request.version != EMOJI_PICKER_PROTOCOL_VERSION
            || request.generation == 0
            || request.query.chars().count() > EMOJI_PICKER_MAX_QUERY_CHARS
            || request
                .category
                .as_deref()
                .is_some_and(|category| !EMOJI_PICKER_CATEGORIES.contains(&category))
        {
            return None;
        }

        let query = request.query.trim();
        let mut matches = if query.is_empty() {
            self.entries.iter().collect::<Vec<_>>()
        } else {
            self.ranked_entries(query)
        };
        if let Some(category) = request.category.as_deref() {
            matches.retain(|entry| entry.category == category);
        }

        let total = matches.len();
        let offset = request.offset.min(total);
        let entries = matches
            .into_iter()
            .skip(offset)
            .take(EMOJI_PICKER_RESULT_LIMIT)
            .map(EmojiPickerResultEntry::from)
            .collect::<Vec<_>>();
        let has_more = offset + entries.len() < total;

        Some(EmojiPickerResult {
            version: EMOJI_PICKER_PROTOCOL_VERSION,
            generation: request.generation,
            offset,
            total,
            has_previous: offset > 0,
            has_more,
            entries,
        })
    }

    fn ranked_entries(&self, query: &str) -> Vec<&EmojiEntry> {
        let query = SearchQuery::parse(query);
        let mut matches = self
            .entries()
            .iter()
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
    fn catalog_resolves_slack_names_from_unicode_canonical_names() {
        let custom = HashMap::from([(
            "skeptical".to_string(),
            "alias:face_with_raised_eyebrow".to_string(),
        )]);
        let catalog = EmojiCatalog::new(&custom);

        assert_eq!(
            catalog.resolve("face_with_raised_eyebrow"),
            Some(EmojiValue::Unicode("🤨"))
        );
        assert_eq!(
            catalog.resolve("skeptical"),
            Some(EmojiValue::Unicode("🤨"))
        );
    }

    #[test]
    fn exact_custom_emoji_shadows_unicode_canonical_name_fallback() {
        let custom = HashMap::from([(
            "face_with_raised_eyebrow".to_string(),
            "https://emoji.example/skeptical.png".to_string(),
        )]);

        assert_eq!(
            EmojiCatalog::new(&custom).resolve("face_with_raised_eyebrow"),
            Some(EmojiValue::CustomImage(
                "https://emoji.example/skeptical.png".to_string()
            ))
        );
    }

    #[test]
    fn canonical_name_fallback_rejects_ambiguous_names() {
        let custom = HashMap::new();

        assert_eq!(EmojiCatalog::new(&custom).resolve("keycap"), None);
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
    fn catalog_searches_symbolic_and_numeric_shortcodes() {
        let matches =
            EmojiPickerModel::new(EmojiCatalog::new(&HashMap::new()).entries()).search("+1");

        assert_eq!(matches.first().map(|entry| entry.name.as_str()), Some("+1"));
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

    #[test]
    fn picker_queries_are_generation_scoped_and_bounded() {
        let custom = (0..(EMOJI_PICKER_RESULT_LIMIT + 20))
            .map(|index| {
                (
                    format!("workspace_{index:03}"),
                    format!("https://emoji.example/{index:03}.png"),
                )
            })
            .collect::<HashMap<_, _>>();
        let model = EmojiPickerModel::new(EmojiCatalog::new(&custom).entries());
        let request = EmojiPickerQuery {
            version: EMOJI_PICKER_PROTOCOL_VERSION,
            generation: 7,
            query: String::new(),
            category: Some("Workspace".to_string()),
            offset: 0,
        };

        let result = model
            .query(&request)
            .expect("valid picker query should return a result");

        assert_eq!(result.generation, 7);
        assert_eq!(result.entries.len(), EMOJI_PICKER_RESULT_LIMIT);
        assert_eq!(result.total, EMOJI_PICKER_RESULT_LIMIT + 20);
        assert!(result.has_more);
        assert!(result
            .entries
            .iter()
            .all(|entry| entry.category == "Workspace"));

        let next_page = model
            .query(&EmojiPickerQuery {
                offset: EMOJI_PICKER_RESULT_LIMIT,
                generation: 8,
                ..request
            })
            .unwrap();
        assert_eq!(next_page.entries.len(), 20);
        assert!(next_page.has_previous);
        assert!(!next_page.has_more);
    }

    #[test]
    fn picker_query_pages_preserve_search_and_custom_emoji() {
        let custom = HashMap::from([
            (
                "party_parrot".to_string(),
                "https://emoji.example/parrot.gif".to_string(),
            ),
            (
                "party_penguin".to_string(),
                "https://emoji.example/penguin.gif".to_string(),
            ),
        ]);
        let model = EmojiPickerModel::new(EmojiCatalog::new(&custom).entries());
        let request = EmojiPickerQuery {
            version: EMOJI_PICKER_PROTOCOL_VERSION,
            generation: 9,
            query: "party parr".to_string(),
            category: None,
            offset: 0,
        };

        let result = model.query(&request).unwrap();

        assert_eq!(
            result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["party_parrot"]
        );
        assert_eq!(
            result.entries[0].value_kind,
            EmojiPickerResultValueKind::CustomImage
        );
        assert_eq!(result.entries[0].value, "https://emoji.example/parrot.gif");
    }

    #[test]
    fn picker_generation_gate_rejects_zero_repeated_and_stale_requests() {
        let mut gate = EmojiPickerGenerationGate::default();

        assert!(!gate.accept(0));
        assert!(gate.accept(4));
        assert!(!gate.accept(4));
        assert!(!gate.accept(3));
        assert!(gate.accept(5));
    }

    #[test]
    fn picker_query_rejects_invalid_protocol_inputs() {
        let model = EmojiPickerModel::new(EmojiCatalog::new(&HashMap::new()).entries());
        let valid = EmojiPickerQuery {
            version: EMOJI_PICKER_PROTOCOL_VERSION,
            generation: 1,
            query: String::new(),
            category: Some("Smileys".to_string()),
            offset: 0,
        };

        assert!(model
            .query(&EmojiPickerQuery {
                version: EMOJI_PICKER_PROTOCOL_VERSION + 1,
                ..valid.clone()
            })
            .is_none());
        assert!(model
            .query(&EmojiPickerQuery {
                generation: 0,
                ..valid.clone()
            })
            .is_none());
        assert!(model
            .query(&EmojiPickerQuery {
                query: "x".repeat(EMOJI_PICKER_MAX_QUERY_CHARS + 1),
                ..valid.clone()
            })
            .is_none());
        assert!(model
            .query(&EmojiPickerQuery {
                category: Some("Unknown".to_string()),
                ..valid
            })
            .is_none());
    }

    #[test]
    fn picker_results_serialize_untrusted_fields_as_typed_data() {
        let unsafe_name = "workspace_\"</script><script>alert(1)</script>";
        let custom = HashMap::from([(
            unsafe_name.to_string(),
            "https://emoji.example/image.png?label=\"quoted\"".to_string(),
        )]);
        let model = EmojiPickerModel::new(EmojiCatalog::new(&custom).entries());
        let result = model
            .query(&EmojiPickerQuery {
                version: EMOJI_PICKER_PROTOCOL_VERSION,
                generation: 2,
                query: String::new(),
                category: Some("Workspace".to_string()),
                offset: 0,
            })
            .unwrap();

        let serialized = serde_json::to_string(&result).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(decoded["generation"], 2);
        assert_eq!(decoded["entries"][0]["name"], unsafe_name);
        assert_eq!(decoded["entries"][0]["value_kind"], "custom-image");
        assert_eq!(
            decoded["entries"][0]["value"],
            "https://emoji.example/image.png?label=\"quoted\""
        );
    }
}
