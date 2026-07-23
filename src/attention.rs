// The domain API is wired into the workspace pipeline in the next track phase.
#![allow(dead_code)]

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationKind {
    Channel,
    DirectMessage,
    GroupDirectMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageMutation {
    Posted,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ThreadRelationship {
    #[default]
    NotAReply,
    UnrelatedReply,
    Started,
    Participated,
    Subscribed,
}

impl ThreadRelationship {
    const fn is_relevant(self) -> bool {
        matches!(self, Self::Started | Self::Participated | Self::Subscribed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryState {
    Fresh,
    Stale,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttentionCandidate<'a> {
    pub(crate) text: &'a str,
    pub(crate) subtype: Option<&'a str>,
    pub(crate) mutation: MessageMutation,
    pub(crate) author_user_id: Option<&'a str>,
    pub(crate) current_user_id: Option<&'a str>,
    pub(crate) conversation: ConversationKind,
    pub(crate) thread_relationship: ThreadRelationship,
    pub(crate) has_content: bool,
    pub(crate) no_notifications: bool,
    pub(crate) muted: bool,
    pub(crate) actively_reading: bool,
    pub(crate) delivery: DeliveryState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttentionPreferences {
    pub(crate) desktop_notifications: bool,
    pub(crate) direct_messages: bool,
    pub(crate) mentions_and_names: bool,
    pub(crate) thread_replies: bool,
    pub(crate) names_and_aliases: Vec<String>,
    pub(crate) keywords: Vec<String>,
}

impl Default for AttentionPreferences {
    fn default() -> Self {
        Self {
            desktop_notifications: true,
            direct_messages: true,
            mentions_and_names: true,
            thread_replies: true,
            names_and_aliases: Vec::new(),
            keywords: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionReason {
    MembershipLifecycle,
    NonMessageNoise,
    EmptyMessage,
    SelfAuthored,
    NonPostedMutation,
    OrdinaryMessage,
    DirectMessage,
    DirectMention,
    NameOrAlias,
    KeywordOrPhrase,
    StartedThreadReply,
    ParticipatedThreadReply,
    SubscribedThreadReply,
    NotificationsDisabled,
    MutedConversation,
    ActiveTarget,
    StaleDelivery,
    DuplicateDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttentionDecision {
    pub(crate) record_unread: bool,
    pub(crate) send_notification: bool,
    pub(crate) reasons: Vec<AttentionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttentionPolicy {
    preferences: AttentionPreferences,
    names_and_aliases: Vec<String>,
    keywords: Vec<String>,
}

impl AttentionPolicy {
    pub(crate) fn new(preferences: AttentionPreferences) -> Self {
        let names_and_aliases = normalized_terms(&preferences.names_and_aliases);
        let keywords = normalized_terms(&preferences.keywords);
        Self {
            preferences,
            names_and_aliases,
            keywords,
        }
    }

    pub(crate) fn decide(&self, candidate: AttentionCandidate<'_>) -> AttentionDecision {
        if is_membership_lifecycle_subtype(candidate.subtype) {
            return AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::MembershipLifecycle],
            };
        }
        if candidate.no_notifications || is_non_message_noise_subtype(candidate.subtype) {
            return AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::NonMessageNoise],
            };
        }
        if !candidate.has_content {
            return AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::EmptyMessage],
            };
        }
        if candidate.mutation != MessageMutation::Posted {
            return AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::NonPostedMutation],
            };
        }
        if candidate
            .author_user_id
            .zip(candidate.current_user_id)
            .is_some_and(|(author, current)| author == current)
        {
            return AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::SelfAuthored],
            };
        }

        let normalized_text = normalize_text(candidate.text);
        let mut reasons = Vec::new();
        if self.preferences.direct_messages
            && matches!(
                candidate.conversation,
                ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage
            )
        {
            reasons.push(AttentionReason::DirectMessage);
        }
        if self.preferences.mentions_and_names {
            if contains_direct_mention(candidate.text, candidate.current_user_id) {
                reasons.push(AttentionReason::DirectMention);
            }
            if self
                .names_and_aliases
                .iter()
                .any(|term| contains_configured_term(&normalized_text, term))
            {
                reasons.push(AttentionReason::NameOrAlias);
            }
        }
        if self
            .keywords
            .iter()
            .any(|term| contains_configured_term(&normalized_text, term))
        {
            reasons.push(AttentionReason::KeywordOrPhrase);
        }
        if self.preferences.thread_replies && candidate.thread_relationship.is_relevant() {
            reasons.push(match candidate.thread_relationship {
                ThreadRelationship::Started => AttentionReason::StartedThreadReply,
                ThreadRelationship::Participated => AttentionReason::ParticipatedThreadReply,
                ThreadRelationship::Subscribed => AttentionReason::SubscribedThreadReply,
                ThreadRelationship::NotAReply | ThreadRelationship::UnrelatedReply => {
                    unreachable!()
                }
            });
        }
        let notification_relevant = !reasons.is_empty();
        if !notification_relevant {
            reasons.push(AttentionReason::OrdinaryMessage);
        }

        if !self.preferences.desktop_notifications {
            reasons.push(AttentionReason::NotificationsDisabled);
        }
        if candidate.muted {
            reasons.push(AttentionReason::MutedConversation);
        }
        if candidate.actively_reading {
            reasons.push(AttentionReason::ActiveTarget);
        }
        let fresh = candidate.delivery == DeliveryState::Fresh;
        match candidate.delivery {
            DeliveryState::Fresh => {}
            DeliveryState::Stale => reasons.push(AttentionReason::StaleDelivery),
            DeliveryState::Duplicate => reasons.push(AttentionReason::DuplicateDelivery),
        }

        let record_unread = fresh;
        let send_notification = record_unread
            && notification_relevant
            && self.preferences.desktop_notifications
            && !candidate.muted
            && !candidate.actively_reading;
        AttentionDecision {
            record_unread,
            send_notification,
            reasons,
        }
    }
}

fn normalized_terms(terms: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for term in terms {
        let term = normalize_text(term);
        if term.chars().any(char::is_alphanumeric) && !normalized.contains(&term) {
            normalized.push(term);
        }
    }
    normalized
}

fn normalize_text(value: &str) -> String {
    let normalized = value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Configured phrases match after Unicode case/diacritic normalization and
/// whitespace collapsing. Punctuation remains significant. Alphanumeric
/// starts and ends must land on word boundaries, so `alert` does not match
/// `alerts`.
fn contains_configured_term(normalized_text: &str, normalized_term: &str) -> bool {
    if normalized_term.is_empty() {
        return false;
    }
    let starts_with_word = normalized_term
        .chars()
        .next()
        .is_some_and(is_word_character);
    let ends_with_word = normalized_term
        .chars()
        .next_back()
        .is_some_and(is_word_character);

    normalized_text
        .match_indices(normalized_term)
        .any(|(start, _)| {
            let before_is_word = normalized_text[..start]
                .chars()
                .next_back()
                .is_some_and(is_word_character);
            let end = start + normalized_term.len();
            let after_is_word = normalized_text[end..]
                .chars()
                .next()
                .is_some_and(is_word_character);
            (!starts_with_word || !before_is_word) && (!ends_with_word || !after_is_word)
        })
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn contains_direct_mention(text: &str, current_user_id: Option<&str>) -> bool {
    let Some(current_user_id) = current_user_id
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
    else {
        return false;
    };
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            return false;
        };
        let mentioned_user = rest[..end].split('|').next().unwrap_or_default().trim();
        if mentioned_user == current_user_id {
            return true;
        }
        rest = &rest[end + 1..];
    }
    false
}

fn is_membership_lifecycle_subtype(subtype: Option<&str>) -> bool {
    matches!(
        subtype,
        Some(
            "channel_join"
                | "channel_leave"
                | "group_join"
                | "group_leave"
                | "member_joined_channel"
                | "member_left_channel"
        )
    )
}

fn is_non_message_noise_subtype(subtype: Option<&str>) -> bool {
    matches!(
        subtype,
        Some(
            "channel_archive"
                | "channel_name"
                | "channel_purpose"
                | "channel_topic"
                | "channel_unarchive"
                | "group_archive"
                | "group_name"
                | "group_purpose"
                | "group_topic"
                | "group_unarchive"
                | "huddle_thread"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> AttentionCandidate<'static> {
        AttentionCandidate {
            text: "status update",
            subtype: None,
            mutation: MessageMutation::Posted,
            author_user_id: Some("U_OTHER"),
            current_user_id: Some("U_SELF"),
            conversation: ConversationKind::Channel,
            thread_relationship: ThreadRelationship::NotAReply,
            has_content: true,
            no_notifications: false,
            muted: false,
            actively_reading: false,
            delivery: DeliveryState::Fresh,
        }
    }

    fn decision(candidate: AttentionCandidate<'_>) -> AttentionDecision {
        AttentionPolicy::new(AttentionPreferences::default()).decide(candidate)
    }

    #[test]
    fn decision_matrix_separates_unread_from_notification_relevance() {
        struct Case {
            name: &'static str,
            candidate: AttentionCandidate<'static>,
            unread: bool,
            notify: bool,
            reason: AttentionReason,
        }

        let mut direct_message = candidate();
        direct_message.conversation = ConversationKind::DirectMessage;
        let mut group_direct_message = candidate();
        group_direct_message.conversation = ConversationKind::GroupDirectMessage;
        let mut mention = candidate();
        mention.text = "hello <@U_SELF>";
        let mut ordinary = candidate();
        ordinary.text = "hello channel";
        let mut thread_started = candidate();
        thread_started.thread_relationship = ThreadRelationship::Started;
        let mut thread_participated = candidate();
        thread_participated.thread_relationship = ThreadRelationship::Participated;
        let mut thread_subscribed = candidate();
        thread_subscribed.thread_relationship = ThreadRelationship::Subscribed;
        let mut thread_unrelated = candidate();
        thread_unrelated.thread_relationship = ThreadRelationship::UnrelatedReply;

        let cases = [
            Case {
                name: "direct message",
                candidate: direct_message,
                unread: true,
                notify: true,
                reason: AttentionReason::DirectMessage,
            },
            Case {
                name: "group direct message",
                candidate: group_direct_message,
                unread: true,
                notify: true,
                reason: AttentionReason::DirectMessage,
            },
            Case {
                name: "direct mention",
                candidate: mention,
                unread: true,
                notify: true,
                reason: AttentionReason::DirectMention,
            },
            Case {
                name: "ordinary channel message",
                candidate: ordinary,
                unread: true,
                notify: false,
                reason: AttentionReason::OrdinaryMessage,
            },
            Case {
                name: "reply to a thread the user started",
                candidate: thread_started,
                unread: true,
                notify: true,
                reason: AttentionReason::StartedThreadReply,
            },
            Case {
                name: "reply to a thread the user participated in",
                candidate: thread_participated,
                unread: true,
                notify: true,
                reason: AttentionReason::ParticipatedThreadReply,
            },
            Case {
                name: "reply to a subscribed thread",
                candidate: thread_subscribed,
                unread: true,
                notify: true,
                reason: AttentionReason::SubscribedThreadReply,
            },
            Case {
                name: "reply to an unrelated thread",
                candidate: thread_unrelated,
                unread: true,
                notify: false,
                reason: AttentionReason::OrdinaryMessage,
            },
        ];

        for case in cases {
            let actual = decision(case.candidate);
            assert_eq!(actual.record_unread, case.unread, "{} unread", case.name);
            assert_eq!(
                actual.send_notification, case.notify,
                "{} notification",
                case.name
            );
            assert!(
                actual.reasons.contains(&case.reason),
                "{} reasons: {:?}",
                case.name,
                actual.reasons
            );
        }
    }

    #[test]
    fn all_supported_membership_lifecycle_subtypes_are_noise() {
        for subtype in [
            "channel_join",
            "channel_leave",
            "group_join",
            "group_leave",
            "member_joined_channel",
            "member_left_channel",
        ] {
            let mut message = candidate();
            message.subtype = Some(subtype);
            let actual = decision(message);
            assert!(!actual.record_unread, "{subtype} unread");
            assert!(!actual.send_notification, "{subtype} notification");
            assert_eq!(
                actual.reasons,
                vec![AttentionReason::MembershipLifecycle],
                "{subtype} reasons"
            );
        }
    }

    #[test]
    fn configured_names_aliases_keywords_and_phrases_use_exact_boundaries() {
        let policy = AttentionPolicy::new(AttentionPreferences {
            names_and_aliases: vec!["Žilvinas".into(), "Vince".into()],
            keywords: vec!["database down".into(), "on-call".into()],
            ..AttentionPreferences::default()
        });
        let cases = [
            ("hey ZILVINAS", AttentionReason::NameOrAlias),
            ("thanks, vince!", AttentionReason::NameOrAlias),
            (
                "The DATABASE   DOWN alert fired",
                AttentionReason::KeywordOrPhrase,
            ),
            ("Please page on-call.", AttentionReason::KeywordOrPhrase),
        ];

        for (text, reason) in cases {
            let mut message = candidate();
            message.text = text;
            let actual = policy.decide(message);
            assert!(actual.send_notification, "{text}");
            assert!(
                actual.reasons.contains(&reason),
                "{text}: {:?}",
                actual.reasons
            );
        }

        for text in ["convincement", "database downtime", "on call", "vince_team"] {
            let mut message = candidate();
            message.text = text;
            let actual = policy.decide(message);
            assert!(!actual.send_notification, "{text}: {:?}", actual.reasons);
        }
    }

    #[test]
    fn suppression_matrix_preserves_unread_where_appropriate() {
        struct Case {
            name: &'static str,
            mutate: fn(&mut AttentionCandidate<'static>),
            unread: bool,
            reason: AttentionReason,
        }

        let cases = [
            Case {
                name: "self authored",
                mutate: |message| message.author_user_id = Some("U_SELF"),
                unread: false,
                reason: AttentionReason::SelfAuthored,
            },
            Case {
                name: "muted",
                mutate: |message| message.muted = true,
                unread: true,
                reason: AttentionReason::MutedConversation,
            },
            Case {
                name: "active target",
                mutate: |message| message.actively_reading = true,
                unread: true,
                reason: AttentionReason::ActiveTarget,
            },
            Case {
                name: "stale",
                mutate: |message| message.delivery = DeliveryState::Stale,
                unread: false,
                reason: AttentionReason::StaleDelivery,
            },
            Case {
                name: "duplicate",
                mutate: |message| message.delivery = DeliveryState::Duplicate,
                unread: false,
                reason: AttentionReason::DuplicateDelivery,
            },
        ];

        for case in cases {
            let mut message = candidate();
            message.conversation = ConversationKind::DirectMessage;
            (case.mutate)(&mut message);
            let actual = decision(message);
            assert_eq!(actual.record_unread, case.unread, "{} unread", case.name);
            assert!(!actual.send_notification, "{} notification", case.name);
            assert!(
                actual.reasons.contains(&case.reason),
                "{} reasons: {:?}",
                case.name,
                actual.reasons
            );
        }
    }

    #[test]
    fn preferences_disable_only_their_notification_triggers() {
        let mut direct_message = candidate();
        direct_message.conversation = ConversationKind::DirectMessage;
        let mut mention = candidate();
        mention.text = "<@U_SELF>";
        let mut thread = candidate();
        thread.thread_relationship = ThreadRelationship::Subscribed;

        for (preferences, message) in [
            (
                AttentionPreferences {
                    direct_messages: false,
                    ..AttentionPreferences::default()
                },
                direct_message,
            ),
            (
                AttentionPreferences {
                    mentions_and_names: false,
                    ..AttentionPreferences::default()
                },
                mention,
            ),
            (
                AttentionPreferences {
                    thread_replies: false,
                    ..AttentionPreferences::default()
                },
                thread,
            ),
        ] {
            let actual = AttentionPolicy::new(preferences).decide(message);
            assert!(actual.record_unread);
            assert!(!actual.send_notification);
        }
    }

    #[test]
    fn master_notification_setting_does_not_disable_unread() {
        let mut message = candidate();
        message.conversation = ConversationKind::DirectMessage;
        let actual = AttentionPolicy::new(AttentionPreferences {
            desktop_notifications: false,
            ..AttentionPreferences::default()
        })
        .decide(message);

        assert!(actual.record_unread);
        assert!(!actual.send_notification);
        assert!(actual
            .reasons
            .contains(&AttentionReason::NotificationsDisabled));
    }

    #[test]
    fn malformed_or_other_user_mentions_do_not_match() {
        for text in ["<@U_OTHER>", "<@U_SELFISH>", "<@U_SELF", "<!channel>"] {
            let mut message = candidate();
            message.text = text;
            assert!(!decision(message).send_notification, "{text}");
        }
        let mut labeled = candidate();
        labeled.text = "<@U_SELF|vincent>";
        assert!(decision(labeled).send_notification);
    }

    #[test]
    fn changed_and_deleted_messages_do_not_create_attention() {
        for mutation in [MessageMutation::Changed, MessageMutation::Deleted] {
            let mut message = candidate();
            message.mutation = mutation;
            let actual = decision(message);
            assert!(!actual.record_unread);
            assert!(!actual.send_notification);
            assert_eq!(actual.reasons, vec![AttentionReason::NonPostedMutation]);
        }
    }

    #[test]
    fn multiple_triggers_still_make_one_notification_with_all_reasons() {
        let mut message = candidate();
        message.conversation = ConversationKind::DirectMessage;
        message.text = "Vince, database down: <@U_SELF>";
        message.thread_relationship = ThreadRelationship::Subscribed;
        let actual = AttentionPolicy::new(AttentionPreferences {
            names_and_aliases: vec!["vince".into()],
            keywords: vec!["database down".into()],
            ..AttentionPreferences::default()
        })
        .decide(message);

        assert!(actual.send_notification);
        assert_eq!(
            actual.reasons,
            vec![
                AttentionReason::DirectMessage,
                AttentionReason::DirectMention,
                AttentionReason::NameOrAlias,
                AttentionReason::KeywordOrPhrase,
                AttentionReason::SubscribedThreadReply,
            ]
        );
    }

    #[test]
    fn empty_and_non_message_noise_are_not_unread() {
        let mut empty = candidate();
        empty.text = " ";
        empty.has_content = false;
        assert_eq!(
            decision(empty),
            AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::EmptyMessage],
            }
        );

        let mut noise = candidate();
        noise.no_notifications = true;
        assert_eq!(
            decision(noise),
            AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::NonMessageNoise],
            }
        );
    }
}
