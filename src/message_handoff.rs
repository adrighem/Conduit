use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use crate::rich_message::MessageControlKey;

const MAX_HANDLE_TOKEN_ATTEMPTS: usize = 32;
const MIN_HANDLE_TOKEN_BYTES: usize = 16;
const MAX_HANDLE_TOKEN_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MessageRef {
    channel_id: String,
    timestamp: String,
}

impl MessageRef {
    pub(crate) fn new(
        channel_id: impl Into<String>,
        timestamp: impl Into<String>,
    ) -> Result<Self, MessageRefError> {
        let channel_id = channel_id.into();
        let timestamp = timestamp.into();
        if channel_id.is_empty()
            || !channel_id
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Err(MessageRefError::InvalidChannel);
        }
        if permalink_timestamp(&timestamp).is_none() {
            return Err(MessageRefError::InvalidTimestamp);
        }
        Ok(Self {
            channel_id,
            timestamp,
        })
    }

    pub(crate) fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub(crate) fn timestamp(&self) -> &str {
        &self.timestamp
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageRefError {
    InvalidChannel,
    InvalidTimestamp,
}

impl fmt::Display for MessageRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChannel => formatter.write_str("invalid Slack conversation identifier"),
            Self::InvalidTimestamp => formatter.write_str("invalid Slack message timestamp"),
        }
    }
}

impl std::error::Error for MessageRefError {}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct MessageControlHandle(String);

impl MessageControlHandle {
    fn parse(token: impl Into<String>) -> Result<Self, HandleResolutionError> {
        let token = token.into();
        if token.len() < MIN_HANDLE_TOKEN_BYTES
            || token.len() > MAX_HANDLE_TOKEN_BYTES
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(HandleResolutionError::Malformed);
        }
        Ok(Self(token))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn synthetic() -> Self {
        Self("00000000000000000000000000000000".to_string())
    }
}

impl fmt::Debug for MessageControlHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MessageControlHandle([redacted])")
    }
}

pub(crate) trait HandleTokenSource {
    fn next_token(&mut self) -> String;
}

#[derive(Debug, Default)]
pub(crate) struct RandomHandleTokenSource;

impl HandleTokenSource for RandomHandleTokenSource {
    fn next_token(&mut self) -> String {
        format!("{:032x}", rand::random::<u128>())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TimelineSurfaceId {
    Main,
    Thread,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum MessageControlSelection {
    MessageHandoff,
    Control(MessageControlKey),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MessageControlTarget {
    surface: TimelineSurfaceId,
    message: MessageRef,
    selection: MessageControlSelection,
}

impl MessageControlTarget {
    fn handoff(surface: TimelineSurfaceId, message: MessageRef) -> Self {
        Self {
            surface,
            message,
            selection: MessageControlSelection::MessageHandoff,
        }
    }

    fn control(surface: TimelineSurfaceId, message: MessageRef, key: MessageControlKey) -> Self {
        Self {
            surface,
            message,
            selection: MessageControlSelection::Control(key),
        }
    }

    pub(crate) fn message(&self) -> &MessageRef {
        &self.message
    }

    pub(crate) fn selection(&self) -> MessageControlSelection {
        self.selection
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PresentationRevision(u64);

impl PresentationRevision {
    fn next(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("message presentation revision space exhausted"),
        )
    }
}

#[derive(Clone, Debug)]
struct HandleEntry {
    session_epoch: u64,
    target: MessageControlTarget,
    revision: PresentationRevision,
}

pub(crate) struct MessageControlRegistry<T = RandomHandleTokenSource> {
    session_epoch: u64,
    revision: PresentationRevision,
    entries: HashMap<MessageControlHandle, HandleEntry>,
    active: HashMap<MessageControlTarget, (PresentationRevision, MessageControlHandle)>,
    claimed: HashSet<MessageControlHandle>,
    token_source: T,
}

impl<T> fmt::Debug for MessageControlRegistry<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessageControlRegistry")
            .field("session_epoch", &self.session_epoch)
            .field("revision", &self.revision)
            .field("registered_handles", &self.entries.len())
            .finish()
    }
}

impl Default for MessageControlRegistry<RandomHandleTokenSource> {
    fn default() -> Self {
        Self::with_token_source(RandomHandleTokenSource)
    }
}

impl<T: HandleTokenSource> MessageControlRegistry<T> {
    pub(crate) fn with_token_source(token_source: T) -> Self {
        Self {
            session_epoch: 0,
            revision: PresentationRevision(0),
            entries: HashMap::new(),
            active: HashMap::new(),
            claimed: HashSet::new(),
            token_source,
        }
    }

    pub(crate) fn reset_session(&mut self) {
        self.session_epoch = self
            .session_epoch
            .checked_add(1)
            .expect("message-control session epoch exhausted");
        self.entries.clear();
        self.active.clear();
        self.claimed.clear();
    }

    #[cfg(test)]
    pub(crate) fn replace_surface(
        &mut self,
        surface: TimelineSurfaceId,
        messages: impl IntoIterator<Item = MessageRef>,
    ) -> Result<HashMap<MessageRef, MessageControlHandle>, HandleRegistrationError> {
        let revoked = self
            .active
            .iter()
            .filter_map(|(target, (_, handle))| {
                (target.surface == surface).then_some(handle.clone())
            })
            .collect::<Vec<_>>();
        self.active.retain(|target, _| target.surface != surface);
        for handle in revoked {
            self.entries.remove(&handle);
            self.claimed.remove(&handle);
        }

        self.revision = self.revision.next();
        let revision = self.revision;
        let mut registered = HashMap::new();
        let mut unique = HashSet::new();
        for message in messages {
            if unique.insert(message.clone()) {
                let target = MessageControlTarget::handoff(surface, message.clone());
                let handle = self.register(target, revision)?;
                registered.insert(message, handle);
            }
        }
        Ok(registered)
    }

    #[cfg(test)]
    pub(crate) fn replace_message(
        &mut self,
        surface: TimelineSurfaceId,
        message: MessageRef,
    ) -> Result<MessageControlHandle, HandleRegistrationError> {
        self.remove_message(surface, &message);
        self.revision = self.revision.next();
        let revision = self.revision;
        self.register(MessageControlTarget::handoff(surface, message), revision)
    }

    pub(crate) fn replace_surface_with_controls(
        &mut self,
        surface: TimelineSurfaceId,
        messages: impl IntoIterator<Item = (MessageRef, Vec<MessageControlKey>)>,
    ) -> Result<HashMap<MessageControlTarget, MessageControlHandle>, HandleRegistrationError> {
        let revoked = self
            .active
            .iter()
            .filter_map(|(target, (_, handle))| {
                (target.surface == surface).then_some(handle.clone())
            })
            .collect::<Vec<_>>();
        self.active.retain(|target, _| target.surface != surface);
        for handle in revoked {
            self.entries.remove(&handle);
            self.claimed.remove(&handle);
        }

        self.revision = self.revision.next();
        let revision = self.revision;
        let mut registered = HashMap::new();
        let mut unique = HashSet::new();
        for (message, control_keys) in messages {
            if !unique.insert(message.clone()) {
                continue;
            }
            let fallback = MessageControlTarget::handoff(surface, message.clone());
            let handle = self.register(fallback.clone(), revision)?;
            registered.insert(fallback, handle);
            for key in control_keys {
                let target = MessageControlTarget::control(surface, message.clone(), key);
                let handle = self.register(target.clone(), revision)?;
                registered.insert(target, handle);
            }
        }
        Ok(registered)
    }

    pub(crate) fn replace_message_with_controls(
        &mut self,
        surface: TimelineSurfaceId,
        message: MessageRef,
        control_keys: Vec<MessageControlKey>,
    ) -> Result<HashMap<MessageControlTarget, MessageControlHandle>, HandleRegistrationError> {
        self.remove_message(surface, &message);
        self.revision = self.revision.next();
        let revision = self.revision;
        let mut registered = HashMap::new();
        let fallback = MessageControlTarget::handoff(surface, message.clone());
        let handle = self.register(fallback.clone(), revision)?;
        registered.insert(fallback, handle);
        for key in control_keys {
            let target = MessageControlTarget::control(surface, message.clone(), key);
            let handle = self.register(target.clone(), revision)?;
            registered.insert(target, handle);
        }
        Ok(registered)
    }

    pub(crate) fn remove_message(&mut self, surface: TimelineSurfaceId, message: &MessageRef) {
        let revoked = self
            .active
            .iter()
            .filter_map(|(target, (_, handle))| {
                (target.surface == surface && &target.message == message).then_some(handle.clone())
            })
            .collect::<Vec<_>>();
        self.active
            .retain(|target, _| target.surface != surface || &target.message != message);
        for handle in revoked {
            self.entries.remove(&handle);
            self.claimed.remove(&handle);
        }
    }

    pub(crate) fn active_handle(
        &self,
        surface: TimelineSurfaceId,
        message: &MessageRef,
    ) -> Option<MessageControlHandle> {
        self.active
            .get(&MessageControlTarget::handoff(surface, message.clone()))
            .map(|(_, handle)| handle.clone())
    }

    pub(crate) fn active_control_handle(
        &self,
        surface: TimelineSurfaceId,
        message: &MessageRef,
        key: MessageControlKey,
    ) -> Option<MessageControlHandle> {
        self.active
            .get(&MessageControlTarget::control(
                surface,
                message.clone(),
                key,
            ))
            .map(|(_, handle)| handle.clone())
    }

    pub(crate) fn resolve(
        &self,
        handle: &MessageControlHandle,
    ) -> Result<MessageRef, HandleResolutionError> {
        let entry = self
            .entries
            .get(handle)
            .ok_or(HandleResolutionError::Unknown)?;
        if entry.session_epoch != self.session_epoch {
            return Err(HandleResolutionError::Stale);
        }
        let is_active = self
            .active
            .get(&entry.target)
            .is_some_and(|(revision, active_handle)| {
                *revision == entry.revision && active_handle == handle
            });
        if !is_active {
            return Err(HandleResolutionError::Stale);
        }
        Ok(entry.target.message.clone())
    }

    pub(crate) fn resolve_target(
        &self,
        handle: &MessageControlHandle,
    ) -> Result<MessageControlTarget, HandleResolutionError> {
        self.resolve(handle)?;
        self.entries
            .get(handle)
            .map(|entry| entry.target.clone())
            .ok_or(HandleResolutionError::Unknown)
    }

    #[cfg(test)]
    pub(crate) fn resolve_token(&self, token: &str) -> Result<MessageRef, HandleResolutionError> {
        self.resolve(&MessageControlHandle::parse(token)?)
    }

    #[cfg(test)]
    pub(crate) fn activate_token(
        &mut self,
        token: &str,
    ) -> Result<MessageRef, HandleResolutionError> {
        let handle = MessageControlHandle::parse(token)?;
        let target = self.resolve(&handle)?;
        let entry = self
            .entries
            .remove(&handle)
            .ok_or(HandleResolutionError::Unknown)?;
        self.active.remove(&entry.target);
        self.claimed.remove(&handle);
        Ok(target)
    }

    pub(crate) fn claim_token(
        &mut self,
        token: &str,
    ) -> Result<(MessageControlHandle, MessageControlTarget), HandleResolutionError> {
        let handle = MessageControlHandle::parse(token)?;
        let target = self.resolve_target(&handle)?;
        if !self.claimed.insert(handle.clone()) {
            return Err(HandleResolutionError::Claimed);
        }
        Ok((handle, target))
    }

    pub(crate) fn release(&mut self, handle: &MessageControlHandle) {
        self.claimed.remove(handle);
    }

    pub(crate) fn complete(&mut self, handle: &MessageControlHandle) {
        self.claimed.remove(handle);
        if let Some(entry) = self.entries.remove(handle) {
            self.active.remove(&entry.target);
        }
    }

    fn register(
        &mut self,
        target: MessageControlTarget,
        revision: PresentationRevision,
    ) -> Result<MessageControlHandle, HandleRegistrationError> {
        let handle = (0..MAX_HANDLE_TOKEN_ATTEMPTS)
            .find_map(|_| {
                MessageControlHandle::parse(self.token_source.next_token())
                    .ok()
                    .filter(|handle| !self.entries.contains_key(handle))
            })
            .ok_or(HandleRegistrationError::TokenSourceExhausted)?;
        let entry = HandleEntry {
            session_epoch: self.session_epoch,
            target: target.clone(),
            revision,
        };
        self.entries.insert(handle.clone(), entry);
        self.active.insert(target, (revision, handle.clone()));
        Ok(handle)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandleRegistrationError {
    TokenSourceExhausted,
}

impl fmt::Display for HandleRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("could not allocate an opaque message-control handle")
    }
}

impl std::error::Error for HandleRegistrationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandleResolutionError {
    Malformed,
    Unknown,
    Stale,
    Claimed,
}

impl fmt::Display for HandleResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("malformed message-control handle"),
            Self::Unknown => formatter.write_str("unknown message-control handle"),
            Self::Stale => formatter.write_str("stale message-control handle"),
            Self::Claimed => formatter.write_str("message-control handle is already in use"),
        }
    }
}

impl std::error::Error for HandleResolutionError {}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SafeSlackPermalink(url::Url);

impl SafeSlackPermalink {
    pub(crate) fn construct(
        workspace_url: &str,
        target: &MessageRef,
    ) -> Result<Self, PermalinkPolicyError> {
        let mut workspace = validated_workspace_url(workspace_url)?;
        let timestamp =
            permalink_timestamp(target.timestamp()).ok_or(PermalinkPolicyError::Message)?;
        workspace.set_path(&format!("/archives/{}/p{timestamp}", target.channel_id()));
        workspace.set_query(None);
        workspace.set_fragment(None);
        Ok(Self(workspace))
    }

    pub(crate) fn validate_authoritative(
        permalink: &str,
        workspace_url: &str,
        target: &MessageRef,
    ) -> Result<Self, PermalinkPolicyError> {
        let expected = Self::construct(workspace_url, target)?;
        let candidate = url::Url::parse(permalink).map_err(|_| PermalinkPolicyError::Permalink)?;
        if candidate.scheme() != "https"
            || !candidate.username().is_empty()
            || candidate.password().is_some()
            || candidate.host_str() != expected.0.host_str()
            || candidate.port_or_known_default() != expected.0.port_or_known_default()
            || candidate.path() != expected.0.path()
            || candidate.fragment().is_some()
            || !valid_message_permalink_query(&candidate, target.channel_id())
        {
            return Err(PermalinkPolicyError::Permalink);
        }
        Ok(Self(candidate))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SafeSlackPermalink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SafeSlackPermalink([redacted])")
    }
}

fn validated_workspace_url(workspace_url: &str) -> Result<url::Url, PermalinkPolicyError> {
    let workspace = url::Url::parse(workspace_url).map_err(|_| PermalinkPolicyError::Workspace)?;
    if workspace.scheme() != "https"
        || !workspace.username().is_empty()
        || workspace.password().is_some()
        || !workspace
            .host_str()
            .is_some_and(|host| host.ends_with(".slack.com"))
    {
        return Err(PermalinkPolicyError::Workspace);
    }
    Ok(workspace)
}

fn valid_message_permalink_query(candidate: &url::Url, channel_id: &str) -> bool {
    let mut thread_ts = None;
    let mut cid = None;
    for (key, value) in candidate.query_pairs() {
        match key.as_ref() {
            "thread_ts" if thread_ts.is_none() && permalink_timestamp(&value).is_some() => {
                thread_ts = Some(value);
            }
            "cid" if cid.is_none() && value == channel_id => cid = Some(value),
            _ => return false,
        }
    }
    thread_ts.is_some() == cid.is_some()
}

fn permalink_timestamp(timestamp: &str) -> Option<String> {
    let (seconds, fraction) = timestamp.split_once('.')?;
    if seconds.is_empty()
        || fraction.is_empty()
        || fraction.len() > 6
        || !seconds.chars().all(|character| character.is_ascii_digit())
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    Some(format!("{seconds}{fraction:0<6}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermalinkPolicyError {
    Workspace,
    Message,
    Permalink,
}

impl fmt::Display for PermalinkPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace => formatter.write_str("invalid Slack workspace URL"),
            Self::Message => formatter.write_str("invalid Slack message location"),
            Self::Permalink => formatter.write_str("unsafe Slack message permalink"),
        }
    }
}

impl std::error::Error for PermalinkPolicyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderFailure {
    Authentication,
    Connectivity,
    RateLimited,
    PermissionDenied,
    NotFound,
    Unsupported,
    Unexpected,
}

impl ProviderFailure {
    fn fallback_reason(self) -> Option<FallbackReason> {
        match self {
            Self::Connectivity => Some(FallbackReason::Connectivity),
            Self::RateLimited => Some(FallbackReason::RateLimited),
            Self::PermissionDenied => Some(FallbackReason::PermissionDenied),
            Self::NotFound => Some(FallbackReason::NotFound),
            Self::Unsupported => Some(FallbackReason::Unsupported),
            Self::Authentication | Self::Unexpected => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FallbackReason {
    Connectivity,
    RateLimited,
    PermissionDenied,
    NotFound,
    Unsupported,
    InvalidAuthoritativeResponse,
}

impl From<ProviderFailure> for FallbackReason {
    fn from(failure: ProviderFailure) -> Self {
        failure
            .fallback_reason()
            .expect("non-fallback provider failure cannot become fallback provenance")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandoffProvenance {
    Authoritative,
    CachedAuthoritative,
    ConstructedFallback(FallbackReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMessageHandoff {
    pub(crate) target: MessageRef,
    pub(crate) permalink: SafeSlackPermalink,
    pub(crate) provenance: HandoffProvenance,
}

pub(crate) struct MessageHandoffResolver {
    cache_capacity: usize,
    cache: HashMap<MessageRef, SafeSlackPermalink>,
    recency: VecDeque<MessageRef>,
}

impl MessageHandoffResolver {
    pub(crate) fn new(cache_capacity: usize) -> Self {
        Self {
            cache_capacity,
            cache: HashMap::new(),
            recency: VecDeque::new(),
        }
    }

    pub(crate) fn cached(&mut self, target: &MessageRef) -> Option<ResolvedMessageHandoff> {
        let permalink = self.cache.get(target)?.clone();
        self.touch(target);
        Some(ResolvedMessageHandoff {
            target: target.clone(),
            permalink,
            provenance: HandoffProvenance::CachedAuthoritative,
        })
    }

    pub(crate) fn resolve_provider_result(
        &mut self,
        workspace_url: &str,
        target: &MessageRef,
        provider_result: Result<String, ProviderFailure>,
    ) -> Result<ResolvedMessageHandoff, HandoffResolutionError> {
        match provider_result {
            Ok(permalink) => {
                match SafeSlackPermalink::validate_authoritative(&permalink, workspace_url, target)
                {
                    Ok(permalink) => {
                        self.insert(target.clone(), permalink.clone());
                        Ok(ResolvedMessageHandoff {
                            target: target.clone(),
                            permalink,
                            provenance: HandoffProvenance::Authoritative,
                        })
                    }
                    Err(_) => self.construct_fallback(
                        workspace_url,
                        target,
                        FallbackReason::InvalidAuthoritativeResponse,
                    ),
                }
            }
            Err(failure) => match failure.fallback_reason() {
                Some(reason) => self.construct_fallback(workspace_url, target, reason),
                None => Err(HandoffResolutionError::Provider(failure)),
            },
        }
    }

    fn construct_fallback(
        &self,
        workspace_url: &str,
        target: &MessageRef,
        reason: FallbackReason,
    ) -> Result<ResolvedMessageHandoff, HandoffResolutionError> {
        let permalink = SafeSlackPermalink::construct(workspace_url, target)
            .map_err(HandoffResolutionError::Policy)?;
        Ok(ResolvedMessageHandoff {
            target: target.clone(),
            permalink,
            provenance: HandoffProvenance::ConstructedFallback(reason),
        })
    }

    fn insert(&mut self, target: MessageRef, permalink: SafeSlackPermalink) {
        if self.cache_capacity == 0 {
            return;
        }
        self.cache.insert(target.clone(), permalink);
        self.touch(&target);
        while self.cache.len() > self.cache_capacity {
            if let Some(evicted) = self.recency.pop_front() {
                self.cache.remove(&evicted);
            }
        }
    }

    fn touch(&mut self, target: &MessageRef) {
        self.recency.retain(|cached| cached != target);
        self.recency.push_back(target.clone());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandoffResolutionError {
    Policy(PermalinkPolicyError),
    Provider(ProviderFailure),
}

impl fmt::Display for HandoffResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(error) => error.fmt(formatter),
            Self::Provider(_) => formatter.write_str("could not resolve Slack message permalink"),
        }
    }
}

impl std::error::Error for HandoffResolutionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExternalOpenError {
    message: String,
}

impl ExternalOpenError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ExternalOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExternalOpenError {}

pub(crate) trait ExternalOpener {
    fn open(&self, permalink: &SafeSlackPermalink) -> Result<(), ExternalOpenError>;
}

pub(crate) fn open_resolved_handoff(
    opener: &impl ExternalOpener,
    handoff: &ResolvedMessageHandoff,
) -> Result<(), ExternalOpenError> {
    opener.open(&handoff.permalink)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::rich_message::{MessageControl, MessageDocument, MessageNode};

    #[derive(Debug)]
    struct SequenceTokenSource {
        tokens: VecDeque<String>,
    }

    impl SequenceTokenSource {
        fn new(tokens: &[&str]) -> Self {
            Self {
                tokens: tokens.iter().map(ToString::to_string).collect(),
            }
        }
    }

    impl HandleTokenSource for SequenceTokenSource {
        fn next_token(&mut self) -> String {
            self.tokens
                .pop_front()
                .expect("test token sequence exhausted")
        }
    }

    fn target(channel_id: &str, timestamp: &str) -> MessageRef {
        MessageRef::new(channel_id, timestamp).expect("valid test message reference")
    }

    fn registry(tokens: &[&str]) -> MessageControlRegistry<SequenceTokenSource> {
        MessageControlRegistry::with_token_source(SequenceTokenSource::new(tokens))
    }

    #[test]
    fn message_reference_validates_channel_and_timestamp() {
        assert!(MessageRef::new("C123ABC", "1710000000.000100").is_ok());
        assert!(MessageRef::new("D123", "1710000000.1").is_ok());

        for (channel_id, timestamp) in [
            ("", "1710000000.000100"),
            ("C123/path", "1710000000.000100"),
            ("C123", ""),
            ("C123", "1710000000"),
            ("C123", "1710000000."),
            ("C123", "1710000000.0000000"),
            ("C123", "1710000000.secret"),
        ] {
            assert!(MessageRef::new(channel_id, timestamp).is_err());
        }
    }

    #[test]
    fn opaque_handle_debug_output_is_redacted() {
        let mut registry = registry(&["0123456789abcdef0123456789abcdef"]);
        let handle = registry
            .replace_message(TimelineSurfaceId::Main, target("C123", "1710000000.000100"))
            .unwrap();

        assert_eq!(format!("{handle:?}"), "MessageControlHandle([redacted])");
        assert!(!handle.as_str().contains("C123"));
        assert!(!handle.as_str().contains("1710000000"));
    }

    #[test]
    fn token_source_rejects_invalid_tokens_and_retries_collisions() {
        let mut registry = registry(&[
            "bad token",
            "0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef",
            "fedcba9876543210fedcba9876543210",
        ]);
        let first = registry
            .replace_message(TimelineSurfaceId::Main, target("C1", "1710000000.000100"))
            .unwrap();
        let second = registry
            .replace_message(TimelineSurfaceId::Main, target("C2", "1710000000.000200"))
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(second.as_str(), "fedcba9876543210fedcba9876543210");
    }

    #[test]
    fn unknown_and_malformed_handles_are_rejected() {
        let registry = registry(&[]);
        assert_eq!(
            registry.resolve_token("0123456789abcdef0123456789abcdef"),
            Err(HandleResolutionError::Unknown)
        );
        assert_eq!(
            registry.resolve_token("channel=C123"),
            Err(HandleResolutionError::Malformed)
        );
    }

    #[test]
    fn activating_a_handle_is_one_shot_and_rejects_replay() {
        let mut registry = registry(&["0123456789abcdef0123456789abcdef"]);
        let message = target("C123", "1710000000.000100");
        let handle = registry
            .replace_message(TimelineSurfaceId::Main, message.clone())
            .unwrap();

        assert_eq!(registry.activate_token(handle.as_str()).unwrap(), message);
        assert_eq!(
            registry.activate_token(handle.as_str()),
            Err(HandleResolutionError::Unknown)
        );
    }

    #[test]
    fn per_control_handles_claim_release_and_complete_independently() {
        let mut registry = registry(&[
            "00000000000000000000000000000000",
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ]);
        let message = target("C123", "1710000000.000100");
        let document = MessageDocument::new(
            vec![MessageNode::Actions(vec![
                MessageControl::new("Approve", None),
                MessageControl::new("Decline", None),
            ])],
            None,
        );
        let keys = document.control_keys();
        let handles = registry
            .replace_message_with_controls(TimelineSurfaceId::Main, message.clone(), keys.clone())
            .unwrap();
        let approve = handles
            .get(&MessageControlTarget::control(
                TimelineSurfaceId::Main,
                message.clone(),
                keys[0],
            ))
            .unwrap();
        let decline = handles
            .get(&MessageControlTarget::control(
                TimelineSurfaceId::Main,
                message,
                keys[1],
            ))
            .unwrap();

        assert_ne!(approve, decline);
        let (claimed_handle, claimed_target) = registry.claim_token(approve.as_str()).unwrap();
        assert_eq!(
            claimed_target.selection(),
            MessageControlSelection::Control(keys[0])
        );
        assert_eq!(
            registry.claim_token(approve.as_str()),
            Err(HandleResolutionError::Claimed)
        );
        registry.release(&claimed_handle);
        let (claimed_handle, _) = registry.claim_token(approve.as_str()).unwrap();
        registry.complete(&claimed_handle);
        assert_eq!(
            registry.claim_token(approve.as_str()),
            Err(HandleResolutionError::Unknown)
        );
        assert!(registry.claim_token(decline.as_str()).is_ok());
    }

    #[test]
    fn resetting_session_revokes_all_handles() {
        let mut registry = registry(&["0123456789abcdef0123456789abcdef"]);
        let handle = registry
            .replace_message(TimelineSurfaceId::Main, target("C123", "1710000000.000100"))
            .unwrap();
        registry.reset_session();

        assert_eq!(
            registry.resolve(&handle),
            Err(HandleResolutionError::Unknown)
        );
    }

    #[test]
    fn replacing_message_revokes_old_revision_only() {
        let mut registry = registry(&[
            "0123456789abcdef0123456789abcdef",
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ]);
        let first_message = target("C1", "1710000000.000100");
        let other_message = target("C2", "1710000000.000200");
        let first_handle = registry
            .replace_message(TimelineSurfaceId::Main, first_message.clone())
            .unwrap();
        let other_handle = registry
            .replace_message(TimelineSurfaceId::Main, other_message.clone())
            .unwrap();
        let replacement = registry
            .replace_message(TimelineSurfaceId::Main, first_message.clone())
            .unwrap();

        assert_eq!(
            registry.resolve(&first_handle),
            Err(HandleResolutionError::Unknown)
        );
        assert_eq!(registry.resolve(&replacement), Ok(first_message));
        assert_eq!(registry.resolve(&other_handle), Ok(other_message));
    }

    #[test]
    fn replacing_main_surface_preserves_thread_handles() {
        let mut registry = registry(&[
            "0123456789abcdef0123456789abcdef",
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ]);
        let old_main = registry
            .replace_message(TimelineSurfaceId::Main, target("C1", "1710000000.000100"))
            .unwrap();
        let thread_target = target("C1", "1710000000.000200");
        let thread = registry
            .replace_message(TimelineSurfaceId::Thread, thread_target.clone())
            .unwrap();
        registry
            .replace_surface(TimelineSurfaceId::Main, [target("C2", "1710000000.000300")])
            .unwrap();

        assert_eq!(
            registry.resolve(&old_main),
            Err(HandleResolutionError::Unknown)
        );
        assert_eq!(registry.resolve(&thread), Ok(thread_target));
    }

    #[test]
    fn replacing_surface_deduplicates_messages_and_removes_absent_entries() {
        let mut registry = registry(&[
            "0123456789abcdef0123456789abcdef",
            "11111111111111111111111111111111",
        ]);
        let removed_target = target("C1", "1710000000.000100");
        let removed = registry
            .replace_message(TimelineSurfaceId::Main, removed_target)
            .unwrap();
        let retained_target = target("C2", "1710000000.000200");
        let handles = registry
            .replace_surface(
                TimelineSurfaceId::Main,
                [retained_target.clone(), retained_target.clone()],
            )
            .unwrap();

        assert_eq!(handles.len(), 1);
        assert_eq!(
            registry.resolve(&removed),
            Err(HandleResolutionError::Unknown)
        );
        assert_eq!(
            registry.resolve(handles.get(&retained_target).unwrap()),
            Ok(retained_target)
        );
    }

    #[test]
    fn removing_one_message_preserves_other_entries() {
        let mut registry = registry(&[
            "0123456789abcdef0123456789abcdef",
            "11111111111111111111111111111111",
        ]);
        let removed_target = target("C1", "1710000000.000100");
        let kept_target = target("C2", "1710000000.000200");
        let removed = registry
            .replace_message(TimelineSurfaceId::Main, removed_target.clone())
            .unwrap();
        let kept = registry
            .replace_message(TimelineSurfaceId::Main, kept_target.clone())
            .unwrap();
        registry.remove_message(TimelineSurfaceId::Main, &removed_target);

        assert_eq!(
            registry.resolve(&removed),
            Err(HandleResolutionError::Unknown)
        );
        assert_eq!(registry.resolve(&kept), Ok(kept_target));
    }

    #[test]
    fn constructed_permalink_is_normalized_and_workspace_scoped() {
        let target = target("C123", "1710000000.1");
        let permalink =
            SafeSlackPermalink::construct("https://example.slack.com/", &target).unwrap();

        assert_eq!(
            permalink.as_str(),
            "https://example.slack.com/archives/C123/p1710000000100000"
        );
        assert_eq!(format!("{permalink:?}"), "SafeSlackPermalink([redacted])");
    }

    #[test]
    fn authoritative_permalink_validation_allows_only_exact_safe_destination() {
        let target = target("C123", "1710000000.000100");
        let expected = "https://example.slack.com/archives/C123/p1710000000000100";
        assert!(SafeSlackPermalink::validate_authoritative(
            &format!("{expected}?thread_ts=1710000000.000000&cid=C123"),
            "https://example.slack.com/",
            &target,
        )
        .is_ok());

        for invalid in [
            "http://example.slack.com/archives/C123/p1710000000000100",
            "https://other.slack.com/archives/C123/p1710000000000100",
            "https://example.slack.com/archives/C999/p1710000000000100",
            "https://example.slack.com/archives/C123/p1710000000000200",
            "https://example.slack.com/archives/C123/p1710000000000100#fragment",
            "https://example.slack.com/archives/C123/p1710000000000100?token=secret",
            "https://example.slack.com/archives/C123/p1710000000000100?cid=C999",
            "https://example.slack.com/archives/C123/p1710000000000100?thread_ts=bad&cid=C123",
        ] {
            assert!(
                SafeSlackPermalink::validate_authoritative(
                    invalid,
                    "https://example.slack.com/",
                    &target,
                )
                .is_err(),
                "{invalid} should be rejected"
            );
        }
    }

    #[test]
    fn workspace_validation_rejects_non_slack_and_credentialed_urls() {
        let target = target("C123", "1710000000.000100");
        for workspace in [
            "http://example.slack.com/",
            "https://example.invalid/",
            "https://user@example.slack.com/",
            "https://user:password@example.slack.com/",
        ] {
            assert!(SafeSlackPermalink::construct(workspace, &target).is_err());
        }
    }

    #[test]
    fn authoritative_resolution_is_cached_with_explicit_provenance() {
        let target = target("C123", "1710000000.000100");
        let mut resolver = MessageHandoffResolver::new(2);
        let resolved = resolver
            .resolve_provider_result(
                "https://example.slack.com/",
                &target,
                Ok("https://example.slack.com/archives/C123/p1710000000000100".to_string()),
            )
            .unwrap();
        let cached = resolver.cached(&target).unwrap();

        assert_eq!(resolved.provenance, HandoffProvenance::Authoritative);
        assert_eq!(cached.provenance, HandoffProvenance::CachedAuthoritative);
        assert_eq!(resolved.permalink, cached.permalink);
    }

    #[test]
    fn cache_is_bounded_and_lru() {
        let mut resolver = MessageHandoffResolver::new(2);
        let one = target("C1", "1710000000.000100");
        let two = target("C2", "1710000000.000200");
        let three = target("C3", "1710000000.000300");
        for target in [&one, &two] {
            resolver
                .resolve_provider_result(
                    "https://example.slack.com/",
                    target,
                    Ok(
                        SafeSlackPermalink::construct("https://example.slack.com/", target)
                            .unwrap()
                            .as_str()
                            .to_string(),
                    ),
                )
                .unwrap();
        }
        assert!(resolver.cached(&one).is_some());
        resolver
            .resolve_provider_result(
                "https://example.slack.com/",
                &three,
                Ok(
                    SafeSlackPermalink::construct("https://example.slack.com/", &three)
                        .unwrap()
                        .as_str()
                        .to_string(),
                ),
            )
            .unwrap();

        assert!(resolver.cached(&one).is_some());
        assert!(resolver.cached(&two).is_none());
        assert!(resolver.cached(&three).is_some());
    }

    #[test]
    fn zero_capacity_cache_never_retains_entries() {
        let target = target("C123", "1710000000.000100");
        let mut resolver = MessageHandoffResolver::new(0);
        resolver
            .resolve_provider_result(
                "https://example.slack.com/",
                &target,
                Ok("https://example.slack.com/archives/C123/p1710000000000100".to_string()),
            )
            .unwrap();

        assert!(resolver.cached(&target).is_none());
    }

    #[test]
    fn explicit_transient_failures_use_constructed_fallback() {
        let target = target("C123", "1710000000.000100");
        for failure in [
            ProviderFailure::Connectivity,
            ProviderFailure::RateLimited,
            ProviderFailure::PermissionDenied,
            ProviderFailure::NotFound,
            ProviderFailure::Unsupported,
        ] {
            let mut resolver = MessageHandoffResolver::new(2);
            let result = resolver
                .resolve_provider_result("https://example.slack.com/", &target, Err(failure))
                .unwrap();
            assert_eq!(
                result.provenance,
                HandoffProvenance::ConstructedFallback(failure.into())
            );
            assert!(resolver.cached(&target).is_none());
        }
    }

    #[test]
    fn authentication_and_unexpected_failures_are_propagated() {
        let target = target("C123", "1710000000.000100");
        for failure in [ProviderFailure::Authentication, ProviderFailure::Unexpected] {
            let mut resolver = MessageHandoffResolver::new(2);
            assert_eq!(
                resolver.resolve_provider_result(
                    "https://example.slack.com/",
                    &target,
                    Err(failure),
                ),
                Err(HandoffResolutionError::Provider(failure))
            );
        }
    }

    #[test]
    fn unsafe_authoritative_result_uses_safe_fallback_without_caching() {
        let target = target("C123", "1710000000.000100");
        let mut resolver = MessageHandoffResolver::new(2);
        let result = resolver
            .resolve_provider_result(
                "https://example.slack.com/",
                &target,
                Ok("https://attacker.invalid/message".to_string()),
            )
            .unwrap();

        assert_eq!(
            result.provenance,
            HandoffProvenance::ConstructedFallback(FallbackReason::InvalidAuthoritativeResponse)
        );
        assert_eq!(
            result.permalink.as_str(),
            "https://example.slack.com/archives/C123/p1710000000000100"
        );
        assert!(resolver.cached(&target).is_none());
    }

    #[derive(Default)]
    struct RecordingOpener {
        opened: RefCell<Vec<String>>,
        fail: bool,
    }

    impl ExternalOpener for RecordingOpener {
        fn open(&self, permalink: &SafeSlackPermalink) -> Result<(), ExternalOpenError> {
            if self.fail {
                return Err(ExternalOpenError::new("synthetic open failure"));
            }
            self.opened
                .borrow_mut()
                .push(permalink.as_str().to_string());
            Ok(())
        }
    }

    #[test]
    fn external_opener_receives_only_safe_permalink_once() {
        let target = target("C123", "1710000000.000100");
        let permalink =
            SafeSlackPermalink::construct("https://example.slack.com/", &target).unwrap();
        let handoff = ResolvedMessageHandoff {
            target,
            permalink,
            provenance: HandoffProvenance::Authoritative,
        };
        let opener = RecordingOpener::default();
        open_resolved_handoff(&opener, &handoff).unwrap();

        assert_eq!(opener.opened.borrow().len(), 1);
    }

    #[test]
    fn external_opener_failure_is_propagated() {
        let target = target("C123", "1710000000.000100");
        let handoff = ResolvedMessageHandoff {
            permalink: SafeSlackPermalink::construct("https://example.slack.com/", &target)
                .unwrap(),
            target,
            provenance: HandoffProvenance::Authoritative,
        };
        let opener = RecordingOpener {
            fail: true,
            ..RecordingOpener::default()
        };

        assert!(open_resolved_handoff(&opener, &handoff).is_err());
        assert!(opener.opened.borrow().is_empty());
    }
}
