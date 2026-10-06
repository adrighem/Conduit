//! Search result rows with query highlighting.

use gettextrs::gettext;

use super::model::{is_http_url, plain_text, SearchRow};
use crate::message_html::MessageHtmlContext;
use crate::models::SearchMatch;

/// Search rows in API rank order, with the query terms highlighted.
pub(crate) fn search_rows(
    results: &[SearchMatch],
    query: &str,
    known_channel_title: &dyn Fn(&str) -> Option<String>,
    context: &MessageHtmlContext,
) -> Vec<SearchRow> {
    let terms = highlight_terms(query);
    results
        .iter()
        .map(|result| {
            let channel_title = result
                .channel
                .as_ref()
                .and_then(|channel| {
                    channel
                        .id
                        .as_deref()
                        .and_then(known_channel_title)
                        .or_else(|| channel.name.as_deref().map(|name| format!("#{name}")))
                })
                .unwrap_or_else(|| "Slack".to_string());
            let author = result
                .user
                .as_deref()
                .and_then(|user_id| context.user_names.get(user_id).cloned())
                .or_else(|| result.username.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| gettext("Unknown"));
            let plain_text = plain_text(result.text.as_deref().unwrap_or_default(), context);
            SearchRow {
                location: result.message_location(),
                channel_title,
                author,
                ts: result.ts.clone(),
                snippet_markup: highlight_markup(&plain_text, &terms),
                plain_text,
                permalink: result.permalink.clone().filter(|url| is_http_url(url)),
            }
        })
        .collect()
}

/// Lowercased content terms of a Slack search query: modifiers such as
/// `in:#general` or `-excluded` are not highlighted.
pub(crate) fn highlight_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split_whitespace()
        .filter(|term| !term.starts_with('-') && !term.contains(':'))
        .map(|term| term.trim_matches(['"', '*']).to_lowercase())
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    terms.sort_by_key(|term| std::cmp::Reverse(term.chars().count()));
    terms.dedup();
    terms
}

/// Escapes `text` for Pango and wraps case-insensitive term matches in `<b>`.
pub(crate) fn highlight_markup(text: &str, terms: &[String]) -> String {
    let chars = text.char_indices().collect::<Vec<_>>();
    let mut highlighted = vec![false; chars.len()];
    for term in terms {
        let term_chars = term.chars().collect::<Vec<_>>();
        if term_chars.is_empty() {
            continue;
        }
        for start in 0..chars.len() {
            let matches = term_chars.len() <= chars.len() - start
                && term_chars.iter().enumerate().all(|(offset, expected)| {
                    chars[start + offset]
                        .1
                        .to_lowercase()
                        .eq(expected.to_lowercase())
                });
            if matches {
                highlighted[start..start + term_chars.len()].fill(true);
            }
        }
    }
    let mut markup = String::with_capacity(text.len() + 16);
    let mut open = false;
    for (index, (_, character)) in chars.iter().enumerate() {
        if highlighted[index] != open {
            markup.push_str(if open { "</b>" } else { "<b>" });
            open = highlighted[index];
        }
        push_escaped_markup(&mut markup, *character);
    }
    if open {
        markup.push_str("</b>");
    }
    markup
}

fn push_escaped_markup(output: &mut String, character: char) {
    match character {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        '"' => output.push_str("&quot;"),
        '\'' => output.push_str("&#39;"),
        _ => output.push(character),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SlackSearchChannel;
    use crate::secondary_views::model::tests::context;

    #[test]
    fn highlight_terms_skip_modifiers_and_strip_wildcards() {
        assert_eq!(
            highlight_terms("Deploy in:#ops -draft \"roll*\" from:@bob x"),
            ["deploy", "roll", "x"]
        );
    }

    #[test]
    fn highlight_markup_is_case_insensitive_and_escaped() {
        let terms = highlight_terms("deploy");
        assert_eq!(
            highlight_markup("<Deploy> & DEPLOY", &terms),
            "&lt;<b>Deploy</b>&gt; &amp; <b>DEPLOY</b>"
        );
        assert_eq!(highlight_markup("no match", &terms), "no match");
    }

    #[test]
    fn highlight_markup_merges_overlapping_terms() {
        let terms = highlight_terms("ab bc");
        assert_eq!(highlight_markup("xabcx", &terms), "x<b>abc</b>x");
    }

    #[test]
    fn search_rows_resolve_channel_author_and_safe_permalink() {
        let results = vec![
            SearchMatch {
                channel: Some(SlackSearchChannel {
                    id: Some("C1".to_string()),
                    name: Some("general".to_string()),
                }),
                user: Some("U2".to_string()),
                text: Some("ship <@U1> *now*".to_string()),
                ts: Some("10.0".to_string()),
                permalink: Some("https://example.slack.com/archives/C1/p10".to_string()),
                ..SearchMatch::default()
            },
            SearchMatch {
                channel: Some(SlackSearchChannel {
                    id: Some("C9".to_string()),
                    name: Some("random".to_string()),
                }),
                username: Some("bot".to_string()),
                text: Some("ship".to_string()),
                permalink: Some("javascript:alert(1)".to_string()),
                ..SearchMatch::default()
            },
        ];
        let known = |id: &str| (id == "C1").then(|| "General".to_string());
        let rows = search_rows(&results, "ship", &known, &context());

        assert_eq!(rows[0].channel_title, "General");
        assert_eq!(rows[0].author, "Bob");
        assert!(rows[0].plain_text.contains("@Alice"));
        assert!(rows[0].snippet_markup.starts_with("<b>ship</b>"));
        assert!(rows[0].location.is_some());
        assert!(rows[0].permalink.is_some());
        assert_eq!(rows[1].channel_title, "#random");
        assert_eq!(rows[1].author, "bot");
        assert!(rows[1].location.is_none());
        assert!(rows[1].permalink.is_none());
    }
}
