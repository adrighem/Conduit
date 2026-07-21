use crate::models::SlackMessage;
use std::collections::HashMap;

pub fn extract_user_ids(message: &SlackMessage) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(user) = message.user.as_ref() {
        ids.push(user.clone());
    }
    extract_mentions(&message.body_text(), &mut ids);
    ids.extend(
        message
            .reactions
            .as_ref()
            .into_iter()
            .flatten()
            .flat_map(|reaction| reaction.users.as_ref().into_iter().flatten().cloned()),
    );
    ids.sort();
    ids.dedup();
    ids
}

fn extract_mentions(text: &str, ids: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            return;
        };
        let user_id = rest[..end].split('|').next().unwrap_or_default().trim();
        if !user_id.is_empty() {
            ids.push(user_id.to_string());
        }
        rest = &rest[end + 1..];
    }
}

pub fn resolve_user_mentions(text: &str, user_names: &HashMap<String, String>) -> Option<String> {
    let mut resolved = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("<@") {
        resolved.push_str(&rest[..start]);
        let mention = &rest[start + 2..];
        let Some(end) = mention.find('>') else {
            resolved.push_str(&rest[start..]);
            return Some(resolved);
        };
        let user_id = mention[..end].split('|').next().unwrap_or_default().trim();
        let display_name = user_names.get(user_id)?.trim();
        if display_name.is_empty() {
            return None;
        }
        resolved.push('@');
        resolved.push_str(display_name);
        rest = &mention[end + 1..];
    }

    resolved.push_str(rest);
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_author_mentions_and_reaction_user_ids() {
        let message = SlackMessage {
            user: Some("U123".to_string()),
            text: Some("hi <@U999> and <@U123>".to_string()),
            reactions: Some(vec![crate::models::SlackReaction {
                users: Some(vec!["U456".to_string(), "U123".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(
            extract_user_ids(&message),
            vec!["U123".to_string(), "U456".to_string(), "U999".to_string()]
        );
    }

    #[test]
    fn resolves_user_mentions_to_display_names() {
        let names = std::collections::HashMap::from([
            ("U123".to_string(), "Ada Lovelace".to_string()),
            ("U456".to_string(), "Grace Hopper".to_string()),
        ]);

        assert_eq!(
            resolve_user_mentions("Hi <@U123>, meet <@U456|grace>.", &names).as_deref(),
            Some("Hi @Ada Lovelace, meet @Grace Hopper.")
        );
        assert_eq!(resolve_user_mentions("Hi <@U999>", &names), None);
        assert_eq!(
            resolve_user_mentions("Malformed <@U123", &names).as_deref(),
            Some("Malformed <@U123")
        );
    }
}
