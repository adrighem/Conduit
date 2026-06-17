use crate::models::SlackMessage;

pub fn extract_user_ids(message: &SlackMessage) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(user) = message.user.as_ref() {
        ids.push(user.clone());
    }
    extract_mentions(&message.body_text(), &mut ids);
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
    fn extracts_author_and_mentioned_user_ids() {
        let message = SlackMessage {
            user: Some("U123".to_string()),
            text: Some("hi <@U999> and <@U123>".to_string()),
            ..Default::default()
        };

        assert_eq!(
            extract_user_ids(&message),
            vec!["U123".to_string(), "U999".to_string()]
        );
    }
}
