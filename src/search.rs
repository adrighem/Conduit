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
            terms: query.split_whitespace().map(str::to_lowercase).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn matches_any_term(&self, value: &str) -> bool {
        if self.is_empty() {
            return false;
        }

        let value = value.to_lowercase();
        self.terms
            .iter()
            .any(|term| best_match_in_field(term, &value).is_some())
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
            .map(|field| (field.value.to_lowercase(), field.weight.min(100)))
            .collect::<Vec<_>>();
        let mut term_scores = Vec::with_capacity(self.terms.len());

        for term in &self.terms {
            let best = fields
                .iter()
                .filter_map(|(value, field_weight)| {
                    best_match_in_field(term, value)
                        .map(|score| weighted_percentage(score, *field_weight))
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
        assert!(matches_all_terms("   ", ["anything"]));
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
}
