use crate::models::SlackMessage;

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
        ids.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
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
}
