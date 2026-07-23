use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationKind {
    Unknown,
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
    Reconciled,
    Historical,
    #[cfg_attr(not(test), allow(dead_code))]
    Stale,
    #[cfg_attr(not(test), allow(dead_code))]
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttentionCandidate<'a> {
    pub(crate) text: &'a str,
    pub(crate) subtype: Option<&'a str>,
    pub(crate) mutation: MessageMutation,
    pub(crate) author_is_self: bool,
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
pub struct AttentionPreferences {
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
pub enum AttentionReason {
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
    HistoricalDelivery,
    StaleDelivery,
    DuplicateDelivery,
}

impl AttentionReason {
    pub(crate) const COUNT: usize = 19;
    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::MembershipLifecycle,
        Self::NonMessageNoise,
        Self::EmptyMessage,
        Self::SelfAuthored,
        Self::NonPostedMutation,
        Self::OrdinaryMessage,
        Self::DirectMessage,
        Self::DirectMention,
        Self::NameOrAlias,
        Self::KeywordOrPhrase,
        Self::StartedThreadReply,
        Self::ParticipatedThreadReply,
        Self::SubscribedThreadReply,
        Self::NotificationsDisabled,
        Self::MutedConversation,
        Self::ActiveTarget,
        Self::HistoricalDelivery,
        Self::StaleDelivery,
        Self::DuplicateDelivery,
    ];

    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::MembershipLifecycle => "membership_lifecycle",
            Self::NonMessageNoise => "non_message_noise",
            Self::EmptyMessage => "empty_message",
            Self::SelfAuthored => "self_authored",
            Self::NonPostedMutation => "non_posted_mutation",
            Self::OrdinaryMessage => "ordinary_message",
            Self::DirectMessage => "direct_message",
            Self::DirectMention => "direct_mention",
            Self::NameOrAlias => "name_or_alias",
            Self::KeywordOrPhrase => "keyword_or_phrase",
            Self::StartedThreadReply => "started_thread_reply",
            Self::ParticipatedThreadReply => "participated_thread_reply",
            Self::SubscribedThreadReply => "subscribed_thread_reply",
            Self::NotificationsDisabled => "notifications_disabled",
            Self::MutedConversation => "muted_conversation",
            Self::ActiveTarget => "active_target",
            Self::HistoricalDelivery => "historical_delivery",
            Self::StaleDelivery => "stale_delivery",
            Self::DuplicateDelivery => "duplicate_delivery",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::MembershipLifecycle => 0,
            Self::NonMessageNoise => 1,
            Self::EmptyMessage => 2,
            Self::SelfAuthored => 3,
            Self::NonPostedMutation => 4,
            Self::OrdinaryMessage => 5,
            Self::DirectMessage => 6,
            Self::DirectMention => 7,
            Self::NameOrAlias => 8,
            Self::KeywordOrPhrase => 9,
            Self::StartedThreadReply => 10,
            Self::ParticipatedThreadReply => 11,
            Self::SubscribedThreadReply => 12,
            Self::NotificationsDisabled => 13,
            Self::MutedConversation => 14,
            Self::ActiveTarget => 15,
            Self::HistoricalDelivery => 16,
            Self::StaleDelivery => 17,
            Self::DuplicateDelivery => 18,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionDecision {
    pub(crate) record_unread: bool,
    pub(crate) send_notification: bool,
    pub(crate) reasons: Vec<AttentionReason>,
}

impl AttentionDecision {
    pub(crate) fn remains_notification_relevant(
        &self,
        text: &str,
        preferences: &AttentionPreferences,
    ) -> bool {
        if !self.send_notification || !preferences.desktop_notifications {
            return false;
        }

        self.reasons.iter().copied().any(|reason| match reason {
            AttentionReason::DirectMessage => preferences.direct_messages,
            AttentionReason::DirectMention => preferences.mentions_and_names,
            AttentionReason::StartedThreadReply
            | AttentionReason::ParticipatedThreadReply
            | AttentionReason::SubscribedThreadReply => preferences.thread_replies,
            _ => false,
        }) || (preferences.mentions_and_names
            && configured_terms_match(text, &preferences.names_and_aliases))
            || configured_terms_match(text, &preferences.keywords)
    }
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
        if candidate.author_is_self {
            return AttentionDecision {
                record_unread: false,
                send_notification: false,
                reasons: vec![AttentionReason::SelfAuthored],
            };
        }

        let normalized_text = normalize_text(candidate.text);
        let mut reasons = Vec::new();
        if matches!(
            candidate.conversation,
            ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage
        ) {
            reasons.push(AttentionReason::DirectMessage);
        }
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
        if self
            .keywords
            .iter()
            .any(|term| contains_configured_term(&normalized_text, term))
        {
            reasons.push(AttentionReason::KeywordOrPhrase);
        }
        if candidate.thread_relationship.is_relevant() {
            reasons.push(match candidate.thread_relationship {
                ThreadRelationship::Started => AttentionReason::StartedThreadReply,
                ThreadRelationship::Participated => AttentionReason::ParticipatedThreadReply,
                ThreadRelationship::Subscribed => AttentionReason::SubscribedThreadReply,
                ThreadRelationship::NotAReply | ThreadRelationship::UnrelatedReply => {
                    unreachable!()
                }
            });
        }
        let notification_relevant = reasons
            .iter()
            .copied()
            .any(|reason| relevance_reason_enabled(reason, &self.preferences));
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
        let records_unread = matches!(
            candidate.delivery,
            DeliveryState::Fresh | DeliveryState::Reconciled
        );
        match candidate.delivery {
            DeliveryState::Fresh => {}
            DeliveryState::Reconciled | DeliveryState::Historical => {
                reasons.push(AttentionReason::HistoricalDelivery);
            }
            DeliveryState::Stale => reasons.push(AttentionReason::StaleDelivery),
            DeliveryState::Duplicate => reasons.push(AttentionReason::DuplicateDelivery),
        }

        let record_unread = records_unread;
        let send_notification = candidate.delivery == DeliveryState::Fresh
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

impl Default for AttentionPolicy {
    fn default() -> Self {
        Self::new(AttentionPreferences::default())
    }
}

fn relevance_reason_enabled(reason: AttentionReason, preferences: &AttentionPreferences) -> bool {
    match reason {
        AttentionReason::DirectMessage => preferences.direct_messages,
        AttentionReason::DirectMention | AttentionReason::NameOrAlias => {
            preferences.mentions_and_names
        }
        AttentionReason::KeywordOrPhrase => true,
        AttentionReason::StartedThreadReply
        | AttentionReason::ParticipatedThreadReply
        | AttentionReason::SubscribedThreadReply => preferences.thread_replies,
        _ => false,
    }
}

fn configured_terms_match(text: &str, terms: &[String]) -> bool {
    let normalized_text = normalize_text(text);
    normalized_terms(terms)
        .iter()
        .any(|term| contains_configured_term(&normalized_text, term))
}

fn normalized_terms(terms: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for term in terms {
        let term = normalize_configured_term(term);
        if term.chars().any(char::is_alphanumeric) && !normalized.contains(&term) {
            normalized.push(term);
        }
    }
    normalized
}

pub(crate) fn normalize_configured_term(value: &str) -> String {
    normalize_text(value)
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
    use std::hint::black_box;
    use std::time::Instant;

    use super::*;

    fn candidate() -> AttentionCandidate<'static> {
        AttentionCandidate {
            text: "status update",
            subtype: None,
            mutation: MessageMutation::Posted,
            author_is_self: false,
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
    fn reason_codes_and_counter_indexes_are_unique_and_exhaustive() {
        let mut codes = AttentionReason::ALL
            .iter()
            .map(|reason| reason.code())
            .collect::<Vec<_>>();
        let mut indexes = AttentionReason::ALL
            .iter()
            .map(|reason| reason.index())
            .collect::<Vec<_>>();
        codes.sort_unstable();
        codes.dedup();
        indexes.sort_unstable();
        indexes.dedup();

        assert_eq!(codes.len(), AttentionReason::COUNT);
        assert_eq!(indexes, (0..AttentionReason::COUNT).collect::<Vec<_>>());
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
                mutate: |message| message.author_is_self = true,
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
                name: "reconciled unread",
                mutate: |message| message.delivery = DeliveryState::Reconciled,
                unread: true,
                reason: AttentionReason::HistoricalDelivery,
            },
            Case {
                name: "historical read",
                mutate: |message| message.delivery = DeliveryState::Historical,
                unread: false,
                reason: AttentionReason::HistoricalDelivery,
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
    fn pending_notification_revalidation_preserves_any_current_trigger() {
        let mut message = candidate();
        message.conversation = ConversationKind::DirectMessage;
        message.text = "please page me";
        let decision = AttentionPolicy::new(AttentionPreferences {
            direct_messages: false,
            keywords: vec!["page me".to_string()],
            ..AttentionPreferences::default()
        })
        .decide(message);
        assert!(decision.send_notification);
        assert!(decision.reasons.contains(&AttentionReason::DirectMessage));
        assert!(decision.reasons.contains(&AttentionReason::KeywordOrPhrase));

        let through_keyword = AttentionPreferences {
            direct_messages: false,
            names_and_aliases: vec!["unrelated alias".to_string()],
            keywords: vec!["page me".to_string()],
            ..AttentionPreferences::default()
        };
        assert!(decision.remains_notification_relevant(message.text, &through_keyword));

        let through_newly_enabled_direct_message = AttentionPreferences {
            direct_messages: true,
            keywords: Vec::new(),
            ..AttentionPreferences::default()
        };
        assert!(decision
            .remains_notification_relevant(message.text, &through_newly_enabled_direct_message));

        let no_remaining_trigger = AttentionPreferences {
            direct_messages: false,
            keywords: Vec::new(),
            ..AttentionPreferences::default()
        };
        assert!(!decision.remains_notification_relevant(message.text, &no_remaining_trigger));
        assert!(!decision.remains_notification_relevant(
            message.text,
            &AttentionPreferences {
                desktop_notifications: false,
                ..through_keyword
            }
        ));
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

    fn classifier_burst_candidate(index: usize) -> AttentionCandidate<'static> {
        let mut message = candidate();
        match index {
            0..4000 => message.text = "ordinary channel update",
            4000..6000 => {
                message.text = "direct conversation update";
                message.conversation = ConversationKind::DirectMessage;
            }
            6000..7000 => message.text = "explicit <@U_SELF> update",
            7000..8000 => message.text = "configured priority phrase",
            8000..9000 => {
                message.text = "participated thread update";
                message.thread_relationship = ThreadRelationship::Participated;
            }
            9000..10000 => {
                message.text = "membership update";
                message.subtype = Some("channel_join");
            }
            _ => unreachable!("the measurement workload has exactly 10,000 candidates"),
        }
        message
    }

    fn measure_classifier_burst(policy: &AttentionPolicy) -> (u128, u64, u64) {
        let started = Instant::now();
        let mut unread = 0_u64;
        let mut notification_candidates = 0_u64;
        for index in 0..10_000 {
            let decision = black_box(policy.decide(black_box(classifier_burst_candidate(index))));
            unread += u64::from(decision.record_unread);
            notification_candidates += u64::from(decision.send_notification);
        }
        (
            started.elapsed().as_nanos(),
            unread,
            notification_candidates,
        )
    }

    #[test]
    #[ignore = "release-mode attention classifier measurement"]
    fn realtime_attention_classifier_burst_measurement() {
        let policy = AttentionPolicy::new(AttentionPreferences {
            keywords: vec!["priority phrase".into()],
            ..AttentionPreferences::default()
        });
        let (_, warmup_unread, warmup_notifications) = measure_classifier_burst(&policy);
        assert_eq!(warmup_unread, 9_000);
        assert_eq!(warmup_notifications, 5_000);

        let mut elapsed = Vec::with_capacity(5);
        for _ in 0..5 {
            let (nanoseconds, unread, notification_candidates) = measure_classifier_burst(&policy);
            assert_eq!(unread, 9_000);
            assert_eq!(notification_candidates, 5_000);
            elapsed.push(nanoseconds);
        }
        elapsed.sort_unstable();
        let median_batch_nanoseconds = elapsed[elapsed.len() / 2];
        eprintln!(
            "attention_classifier_burst decisions=10000 iterations=5 \
             median_batch_ns={median_batch_nanoseconds} median_ns_per_decision={} \
             unread=9000 notification_candidates=5000",
            median_batch_nanoseconds / 10_000
        );
    }
}
