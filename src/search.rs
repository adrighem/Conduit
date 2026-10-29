use std::sync::atomic::{AtomicU64, Ordering};

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// Weight used for the main, human-readable field of a search result.
pub const PRIMARY_FIELD_WEIGHT: u8 = 100;
/// Weight suitable for useful supporting fields such as labels or authors.
pub const SECONDARY_FIELD_WEIGHT: u8 = 85;
/// Weight used for opaque identifiers that should match without dominating names.
pub const ID_FIELD_WEIGHT: u8 = 55;

const EXACT_PLACEMENT_WEIGHT: u8 = 100;
const INTERIOR_PLACEMENT_WEIGHT: u8 = 75;
const PREFIX_BASE_SCORE: u32 = 50;
const PREFIX_LENGTH_SCORE: u32 = 10;
const MAX_PREFIX_SCORE: u32 = 90;
const RELEVANCE_BAND_WIDTH: u8 = 5;

#[derive(Debug, Clone, Copy)]
pub struct SearchField<'a> {
    pub value: &'a str,
    pub weight: u8,
}

impl<'a> SearchField<'a> {
    pub const fn new(value: &'a str, weight: u8) -> Self {
        Self { value, weight }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchScore(u8);

impl MatchScore {
    #[cfg(test)]
    pub const fn percentage(self) -> u8 {
        self.0
    }

    /// Returns the five-point relevance band. Scores from 95 through 100 share
    /// the highest band so a perfect match does not become an absolute sort key.
    pub const fn band(self) -> u8 {
        let band = self.0 / RELEVANCE_BAND_WIDTH;
        if band > 19 {
            19
        } else {
            band
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    terms: Vec<String>,
}

impl SearchQuery {
    pub fn parse(query: &str) -> Self {
        Self {
            terms: query
                .split_whitespace()
                .map(normalize_for_search)
                .filter(|term| !term.is_empty())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn matches_any_term(&self, value: &str) -> bool {
        if self.is_empty() {
            return false;
        }

        let value = normalize_for_search(value);
        self.terms
            .iter()
            .any(|term| best_match_in_field(term, &value).is_some())
    }

    /// Whether every match of `self` is also a match of `previous`: the terms
    /// before `previous`'s last one are unchanged and its last term only
    /// grew, or terms were added. A longer substring or an extra term can only
    /// narrow the matches, so filtering the previous matches finds them all.
    pub fn extends(&self, previous: &SearchQuery) -> bool {
        let Some(last) = previous.terms.len().checked_sub(1) else {
            return true;
        };
        self.terms.len() > last
            && self.terms[..last] == previous.terms[..last]
            && self.terms[last].starts_with(previous.terms[last].as_str())
    }

    /// Scores a match from 0 to 100, or returns `None` unless every query term
    /// occurs in at least one field.
    ///
    /// Each term uses its best field/token match. Tokens are split at Unicode
    /// non-alphanumeric characters, and lengths are Unicode character counts.
    /// A term's score is:
    ///
    /// Exact token matches score 100. Prefix matches score from 60 through 90
    /// based on query length, so useful short name prefixes are not penalized by
    /// the length of the full name. Interior substrings use completion
    /// (`term length / containing token length`) at a 75% placement weight. The
    /// result is then multiplied by the caller's field weight. Terms containing
    /// punctuation are matched against the complete field so searches such as
    /// `c-r` still work. The final score is 70% of the mean term score plus 30%
    /// of the weakest term.
    pub fn score<'a>(
        &self,
        fields: impl IntoIterator<Item = SearchField<'a>>,
    ) -> Option<MatchScore> {
        if self.is_empty() {
            return Some(MatchScore(100));
        }

        let fields = fields
            .into_iter()
            .map(|field| (normalize_for_search(field.value), field.weight))
            .collect::<Vec<_>>();
        self.score_normalized(
            fields
                .iter()
                .map(|(value, weight)| (value.as_str(), *weight)),
        )
    }

    /// Scores like [`SearchQuery::score`] for fields already passed through
    /// [`normalize_for_search`], so a cached index normalizes each field once
    /// instead of once per keystroke.
    pub fn score_normalized<'a>(
        &self,
        fields: impl IntoIterator<Item = (&'a str, u8)>,
    ) -> Option<MatchScore> {
        if self.is_empty() {
            return Some(MatchScore(100));
        }

        let fields = fields.into_iter().collect::<Vec<_>>();
        let mut term_scores = Vec::with_capacity(self.terms.len());

        for term in &self.terms {
            let best = fields
                .iter()
                .filter_map(|(value, field_weight)| {
                    best_match_in_field(term, value)
                        .map(|score| weighted_percentage(score, (*field_weight).min(100)))
                })
                .max()?;
            term_scores.push(best);
        }

        let mean = term_scores
            .iter()
            .map(|score| u32::from(*score))
            .sum::<u32>()
            / term_scores.len() as u32;
        let weakest = u32::from(*term_scores.iter().min()?);
        Some(MatchScore(((70 * mean + 30 * weakest) / 100) as u8))
    }
}

pub fn normalize_for_search(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

fn best_match_in_field(term: &str, field: &str) -> Option<u8> {
    let uses_token_matching = term.chars().all(char::is_alphanumeric);
    if uses_token_matching {
        field
            .split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .filter_map(|token| token_match_score(term, token))
            .max()
    } else {
        token_match_score(term, field)
    }
}

fn token_match_score(term: &str, token: &str) -> Option<u8> {
    let byte_offset = token.find(term)?;
    let term_length = term.chars().count() as u32;
    let token_length = token.chars().count() as u32;
    if term_length == token_length {
        Some(EXACT_PLACEMENT_WEIGHT)
    } else if byte_offset == 0 {
        Some((PREFIX_BASE_SCORE + term_length * PREFIX_LENGTH_SCORE).min(MAX_PREFIX_SCORE) as u8)
    } else {
        let completion = term_length * 100 / token_length.max(1);
        Some(weighted_percentage(
            completion as u8,
            INTERIOR_PLACEMENT_WEIGHT,
        ))
    }
}

fn weighted_percentage(score: u8, weight: u8) -> u8 {
    ((u16::from(score) * u16::from(weight)) / 100) as u8
}

static NEXT_COMPLETION_INDEX_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
thread_local! {
    static CANDIDATES_SCORED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Candidates scored on this thread since the last call, for guards that
/// bound the work of a completion keystroke.
#[cfg(test)]
pub fn take_candidates_scored() -> usize {
    CANDIDATES_SCORED.with(|scored| scored.replace(0))
}

/// One completion candidate prepared once: its search fields are normalized
/// when the index is built, not on every keystroke.
#[derive(Debug, Clone)]
pub struct CompletionEntry<T, K> {
    pub item: T,
    /// Lowercased primary name, for ranking rules that compare it.
    pub name_key: String,
    fields: Vec<(String, u8)>,
    /// Entries sharing an id collapse to the best ranked one.
    unique_id: Option<String>,
    /// Order among equally scored matches, precomputed once.
    tie: K,
}

impl<T, K> CompletionEntry<T, K> {
    pub fn new<'a>(
        item: T,
        fields: impl IntoIterator<Item = SearchField<'a>>,
        name_key: String,
        unique_id: Option<String>,
        tie: K,
    ) -> Self {
        Self {
            item,
            name_key,
            fields: fields
                .into_iter()
                .map(|field| (normalize_for_search(field.value), field.weight))
                .collect(),
            unique_id,
            tie,
        }
    }
}

/// The matches of one query, kept so the next keystroke can filter them
/// instead of scoring the whole index again.
#[derive(Debug, Clone)]
pub struct CompletionMatches {
    index_id: u64,
    raw_query: String,
    query: SearchQuery,
    /// Every match (before collapsing shared ids), best first.
    ranked: Vec<u32>,
}

/// Completion candidates prepared for repeated searching. Build it once per
/// change of the underlying data, then [`CompletionIndex::search`] on every
/// keystroke.
#[derive(Debug)]
pub struct CompletionIndex<T, K> {
    id: u64,
    entries: Vec<CompletionEntry<T, K>>,
}

impl<T: Clone, K: Ord> CompletionIndex<T, K> {
    pub fn new(entries: Vec<CompletionEntry<T, K>>) -> Self {
        Self {
            id: NEXT_COMPLETION_INDEX_ID.fetch_add(1, Ordering::Relaxed),
            entries,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The best `limit` matches for `query`, ordered by `rank` (best first,
    /// the smaller key) and then by each entry's precomputed tie key.
    ///
    /// `matches` is the previous search of this index. When `query` extends
    /// it, only those matches are scored; otherwise the whole index is.
    /// `matches` is replaced by this search's matches.
    pub fn search<P: Ord>(
        &self,
        query: &str,
        matches: &mut Option<CompletionMatches>,
        limit: usize,
        rank: impl Fn(&CompletionEntry<T, K>, MatchScore) -> P,
    ) -> Vec<T> {
        if limit == 0 {
            return Vec::new();
        }
        let previous = matches
            .take()
            .filter(|previous| previous.index_id == self.id);
        let previous = match previous {
            Some(previous) if previous.raw_query == query => {
                let found = self.top(&previous.ranked, limit);
                *matches = Some(previous);
                return found;
            }
            other => other,
        };

        let parsed = SearchQuery::parse(query);
        let mut scored = match previous
            .as_ref()
            .filter(|previous| parsed.extends(&previous.query))
        {
            Some(previous) => self.score(&parsed, previous.ranked.iter().copied(), &rank),
            None => self.score(&parsed, 0..self.entries.len() as u32, &rank),
        };
        scored.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| {
                    self.entries[*left as usize]
                        .tie
                        .cmp(&self.entries[*right as usize].tie)
                })
                .then_with(|| left.cmp(right))
        });
        let ranked = scored
            .into_iter()
            .map(|(_, index)| index)
            .collect::<Vec<_>>();
        let found = self.top(&ranked, limit);
        *matches = Some(CompletionMatches {
            index_id: self.id,
            raw_query: query.to_string(),
            query: parsed,
            ranked,
        });
        found
    }

    fn score<P>(
        &self,
        query: &SearchQuery,
        candidates: impl Iterator<Item = u32>,
        rank: &impl Fn(&CompletionEntry<T, K>, MatchScore) -> P,
    ) -> Vec<(P, u32)> {
        candidates
            .filter_map(|index| {
                let entry = &self.entries[index as usize];
                #[cfg(test)]
                CANDIDATES_SCORED.with(|scored| scored.set(scored.get() + 1));
                let score = query.score_normalized(
                    entry
                        .fields
                        .iter()
                        .map(|(value, weight)| (value.as_str(), *weight)),
                )?;
                Some((rank(entry, score), index))
            })
            .collect()
    }

    fn top(&self, ranked: &[u32], limit: usize) -> Vec<T> {
        let mut seen = std::collections::HashSet::new();
        ranked
            .iter()
            .filter_map(|&index| {
                let entry = &self.entries[index as usize];
                match entry.unique_id.as_deref() {
                    Some(id) if !seen.insert(id) => None,
                    _ => Some(entry.item.clone()),
                }
            })
            .take(limit)
            .collect()
    }
}

/// A lazily built completion index that stays until its data changes.
#[derive(Debug)]
pub struct CompletionCache<I> {
    index: Option<std::rc::Rc<I>>,
}

impl<I> Default for CompletionCache<I> {
    fn default() -> Self {
        Self { index: None }
    }
}

impl<I> CompletionCache<I> {
    /// The cached index, built by `build` only when there is none.
    pub fn get_or_build(&mut self, build: impl FnOnce() -> I) -> std::rc::Rc<I> {
        self.index
            .get_or_insert_with(|| std::rc::Rc::new(build()))
            .clone()
    }

    /// Drops the index; the next lookup rebuilds it from the new data.
    pub fn invalidate(&mut self) {
        self.index = None;
    }
}

#[cfg(test)]
fn matches_all_terms<'a>(query: &str, values: impl IntoIterator<Item = &'a str>) -> bool {
    let query = SearchQuery::parse(query);
    query
        .score(
            values
                .into_iter()
                .map(|value| SearchField::new(value, PRIMARY_FIELD_WEIGHT)),
        )
        .is_some()
}

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;

    use super::*;

    fn score(query: &str, value: &str) -> MatchScore {
        SearchQuery::parse(query)
            .score([SearchField::new(value, PRIMARY_FIELD_WEIGHT)])
            .expect("expected a match")
    }

    #[test]
    fn matches_case_insensitive_substrings_in_any_order() {
        assert!(matches_all_terms(
            "  SUPP   bro ",
            ["#broker-orange-support", "C123"]
        ));
        assert!(!matches_all_terms(
            "bro sales",
            ["#broker-orange-support", "C123"]
        ));
    }

    #[test]
    fn detects_when_any_term_matches_a_value() {
        let query = SearchQuery::parse("fat rob");

        assert!(query.matches_any_term("Fatima"));
        assert!(query.matches_any_term("Robey"));
        assert!(!query.matches_any_term("Ada Lovelace"));
    }

    #[test]
    fn terms_can_match_across_searchable_fields() {
        assert!(matches_all_terms("arch vin", ["#architecture", "Vincent"]));
    }

    #[test]
    fn scores_exact_prefix_and_interior_matches() {
        assert_eq!(score("support", "support").percentage(), 100);
        assert_eq!(score("supp", "support").percentage(), 90);
        assert_eq!(score("s", "support").percentage(), 60);
        assert_eq!(score("port", "support").percentage(), 42);
    }

    #[test]
    fn uses_best_token_and_field_weight() {
        let query = SearchQuery::parse("support");
        let score = query
            .score([
                SearchField::new("supportive", PRIMARY_FIELD_WEIGHT),
                SearchField::new("support", ID_FIELD_WEIGHT),
            ])
            .expect("expected a match");

        assert_eq!(score.percentage(), 90);
    }

    #[test]
    fn combines_mean_and_weakest_term_scores() {
        let query = SearchQuery::parse("support broker");
        let score = query
            .score([SearchField::new("support brokerage", PRIMARY_FIELD_WEIGHT)])
            .expect("expected a match");

        // support = 100; broker is a strong prefix of brokerage = 90.
        assert_eq!(score.percentage(), 93);
    }

    #[test]
    fn short_name_prefixes_receive_strong_multi_word_scores() {
        assert_eq!(score("fat rob", "Fatima Robey").percentage(), 80);
    }

    #[test]
    fn splits_tokens_on_unicode_non_alphanumeric_characters() {
        assert_eq!(score("orange", "broker-orange-support").percentage(), 100);
        assert_eq!(score("fé", "FÉdération").percentage(), 70);
        assert!(matches_all_terms("c-r", ["C-RAINBOW"]));
    }

    #[test]
    fn matches_latin_names_without_requiring_diacritics() {
        assert!(matches_all_terms("Zilvinas Kuusas", ["Žilvinas Kuusas"]));
        assert!(matches_all_terms("Žilvinas", ["Zilvinas"]));
    }

    #[test]
    fn matches_canonically_equivalent_composed_and_decomposed_text() {
        let decomposed = "Z\u{030c}ilvinas";

        assert!(matches_all_terms("Žilvinas", [decomposed]));
        assert!(matches_all_terms(decomposed, ["Žilvinas"]));
    }

    #[test]
    fn ignores_terms_that_only_contain_combining_marks() {
        assert!(SearchQuery::parse("\u{030c}").is_empty());
    }

    #[test]
    fn groups_scores_into_five_point_bands() {
        assert_eq!(MatchScore(89).band(), 17);
        assert_eq!(MatchScore(90).band(), 18);
        assert_eq!(MatchScore(94).band(), 18);
        assert_eq!(MatchScore(95).band(), 19);
        assert_eq!(MatchScore(100).band(), 19);
    }

    #[test]
    fn empty_query_matches_with_a_neutral_perfect_score() {
        assert_eq!(score("  ", "anything").percentage(), 100);
    }

    #[test]
    fn query_extension_only_narrows_matches() {
        let query = |text| SearchQuery::parse(text);

        assert!(query("ali").extends(&query("al")));
        assert!(query("al").extends(&query("al")));
        assert!(query("al").extends(&query("")));
        assert!(query("ali ro").extends(&query("ali")));
        assert!(query("fat rob").extends(&query("fat ro")));
        assert!(query("c-r").extends(&query("c-")));
        assert!(query("c-").extends(&query("c")));
        assert!(!query("a").extends(&query("al")), "backspace widens");
        assert!(!query("ro ali").extends(&query("ali")), "terms are ordered");
        assert!(
            !query("a li").extends(&query("al")),
            "a split is not growth"
        );
        assert!(!query("").extends(&query("al")));
    }

    #[test]
    fn perf_guard_completion_index_is_built_once_until_invalidated() {
        let mut builds = 0;
        let mut cache = CompletionCache::<Vec<u8>>::default();
        for _ in 0..100 {
            cache.get_or_build(|| {
                builds += 1;
                Vec::new()
            });
        }
        assert_eq!(builds, 1, "keystrokes must reuse the cached index");

        cache.invalidate();
        cache.get_or_build(|| {
            builds += 1;
            Vec::new()
        });
        assert_eq!(builds, 2, "changed data rebuilds the index");
    }

    #[test]
    fn completion_matches_of_a_replaced_index_are_never_reused() {
        let build = |names: &[&str]| {
            CompletionIndex::new(
                names
                    .iter()
                    .enumerate()
                    .map(|(position, name)| {
                        CompletionEntry::new(
                            name.to_string(),
                            [SearchField::new(name, PRIMARY_FIELD_WEIGHT)],
                            name.to_lowercase(),
                            None,
                            position,
                        )
                    })
                    .collect(),
            )
        };
        let rank = |_: &CompletionEntry<String, usize>, score: MatchScore| Reverse(score);
        let before = build(&["ada", "alan"]);
        let mut matches = None;
        assert_eq!(before.search("a", &mut matches, 10, rank), ["ada", "alan"]);

        let after = build(&["ada", "alan", "alice"]);
        assert_eq!(
            after.search("al", &mut matches, 10, rank),
            ["alan", "alice"],
            "a stale match set would miss the new entry"
        );
    }
}
