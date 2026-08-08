// Verified adapter implementations are intentionally absent until Slack's private
// bootstrap contract and a packaged Chime bridge can be tested safely.
#![allow(dead_code)]

use std::fmt;

use crate::huddles::model::ActiveHuddle;
use crate::huddles::state::HuddleFailure;

const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_SIGNALING_URL_BYTES: usize = 8 * 1024;
const MAX_JOIN_TOKEN_BYTES: usize = 16 * 1024;
const MAX_TURN_URIS: usize = 16;
const MAX_TURN_URI_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeJoinUnavailableReason {
    BrowserSessionRequired,
    SlackBootstrapContractUnverified,
    ChimeBridgeUnavailable,
}

impl NativeJoinUnavailableReason {
    pub fn failure(self) -> HuddleFailure {
        match self {
            Self::SlackBootstrapContractUnverified => HuddleFailure::protocol_changed(),
            Self::BrowserSessionRequired | Self::ChimeBridgeUnavailable => {
                HuddleFailure::unsupported()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeJoinCapability {
    Available {
        slack_contract_revision: &'static str,
        chime_bridge_revision: &'static str,
    },
    Unavailable(NativeJoinUnavailableReason),
}

impl NativeJoinCapability {
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackBootstrapCapability {
    BrowserSessionRequired,
    ContractUnverified,
    Verified { contract_revision: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChimeBridgeCapability {
    Unavailable,
    Verified { bridge_revision: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignalingError {
    #[error("native Slack huddle joining is unavailable: {0:?}")]
    Unavailable(NativeJoinUnavailableReason),
    #[error("Slack returned an invalid huddle bootstrap session")]
    InvalidSession,
    #[error("Slack huddle bootstrap failed")]
    BootstrapFailed,
    #[error("the Amazon Chime bridge failed")]
    ChimeBridgeFailed,
    #[error("a native huddle session is already connected or pending cleanup")]
    AlreadyConnected,
}

pub struct EphemeralSecret(Box<[u8]>);

impl EphemeralSecret {
    fn from_validated(value: &str) -> Self {
        Self(value.as_bytes().to_vec().into_boxed_slice())
    }

    pub(crate) fn expose(&self) -> Result<&str, SignalingError> {
        std::str::from_utf8(&self.0).map_err(|_| SignalingError::InvalidSession)
    }
}

impl fmt::Debug for EphemeralSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for EphemeralSecret {
    fn drop(&mut self) {
        volatile_zeroize(&mut self.0);
        #[cfg(test)]
        notify_sensitive_secret_zeroized(
            SensitiveSecretStorage::Ephemeral,
            self.0.iter().all(|byte| *byte == 0),
        );
    }
}

struct EphemeralSecretInputs(Vec<String>);

impl Drop for EphemeralSecretInputs {
    fn drop(&mut self) {
        for value in &mut self.0 {
            // SAFETY: bytes stay valid UTF-8 after zeroing and String is not read again.
            let bytes = unsafe { value.as_bytes_mut() };
            volatile_zeroize(bytes);
            #[cfg(test)]
            notify_sensitive_secret_zeroized(
                SensitiveSecretStorage::OwnedTurnInput,
                bytes.iter().all(|byte| *byte == 0),
            );
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SensitiveSecretStorage {
    Ephemeral,
    OwnedTurnInput,
}

#[cfg(test)]
type SensitiveSecretZeroizeHook = Box<dyn Fn(SensitiveSecretStorage, bool)>;

#[cfg(test)]
std::thread_local! {
    static SENSITIVE_SECRET_ZEROIZE_HOOK: std::cell::RefCell<
        Option<SensitiveSecretZeroizeHook>,
    > = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_sensitive_secret_zeroize_hook(hook: Option<SensitiveSecretZeroizeHook>) {
    SENSITIVE_SECRET_ZEROIZE_HOOK.with(|current| *current.borrow_mut() = hook);
}

#[cfg(test)]
fn notify_sensitive_secret_zeroized(storage: SensitiveSecretStorage, was_zeroized: bool) {
    SENSITIVE_SECRET_ZEROIZE_HOOK.with(|current| {
        if let Some(hook) = current.borrow().as_ref() {
            hook(storage, was_zeroized);
        }
    });
}

struct SensitiveIdentifier(String);

impl SensitiveIdentifier {
    fn from_validated(value: &str) -> Self {
        Self(value.to_string())
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SensitiveIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for SensitiveIdentifier {
    fn drop(&mut self) {
        // SAFETY: bytes stay valid UTF-8 after zeroing and String is not read again.
        let bytes = unsafe { self.0.as_bytes_mut() };
        volatile_zeroize(bytes);
        #[cfg(test)]
        notify_sensitive_identifier_zeroized(bytes.iter().all(|byte| *byte == 0));
    }
}

#[cfg(test)]
type SensitiveIdentifierZeroizeHook = Box<dyn Fn(bool)>;

#[cfg(test)]
std::thread_local! {
    static SENSITIVE_IDENTIFIER_ZEROIZE_HOOK: std::cell::RefCell<
        Option<SensitiveIdentifierZeroizeHook>,
    > =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_sensitive_identifier_zeroize_hook(hook: Option<SensitiveIdentifierZeroizeHook>) {
    SENSITIVE_IDENTIFIER_ZEROIZE_HOOK.with(|current| *current.borrow_mut() = hook);
}

#[cfg(test)]
fn notify_sensitive_identifier_zeroized(was_zeroized: bool) {
    SENSITIVE_IDENTIFIER_ZEROIZE_HOOK.with(|current| {
        if let Some(hook) = current.borrow().as_ref() {
            hook(was_zeroized);
        }
    });
}

fn volatile_zeroize(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: byte is a valid, uniquely borrowed pointer for a one-byte volatile write.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

pub struct SlackJoinSession {
    meeting_id: SensitiveIdentifier,
    attendee_id: SensitiveIdentifier,
    signaling_url: EphemeralSecret,
    join_token: EphemeralSecret,
    turn_uris: Vec<EphemeralSecret>,
}

impl SlackJoinSession {
    pub fn new_for_adapter(
        meeting_id: &str,
        attendee_id: &str,
        signaling_url: &str,
        join_token: &str,
        turn_uris: Vec<String>,
    ) -> Result<Self, SignalingError> {
        let turn_uris = EphemeralSecretInputs(turn_uris);
        let meeting_id = required_identifier(meeting_id)?;
        let attendee_id = required_identifier(attendee_id)?;
        let signaling_url = required_value(signaling_url, MAX_SIGNALING_URL_BYTES)?;
        let join_token = required_value(join_token, MAX_JOIN_TOKEN_BYTES)?;
        if !signaling_url.starts_with("wss://")
            || turn_uris.0.is_empty()
            || turn_uris.0.len() > MAX_TURN_URIS
        {
            return Err(SignalingError::InvalidSession);
        }
        for uri in &turn_uris.0 {
            required_value(uri, MAX_TURN_URI_BYTES)?;
        }

        Ok(Self {
            meeting_id: SensitiveIdentifier::from_validated(meeting_id),
            attendee_id: SensitiveIdentifier::from_validated(attendee_id),
            signaling_url: EphemeralSecret::from_validated(signaling_url),
            join_token: EphemeralSecret::from_validated(join_token),
            turn_uris: turn_uris
                .0
                .iter()
                .map(|uri| EphemeralSecret::from_validated(uri.trim()))
                .collect(),
        })
    }

    pub(crate) fn meeting_id(&self) -> &str {
        self.meeting_id.expose()
    }

    pub(crate) fn attendee_id(&self) -> &str {
        self.attendee_id.expose()
    }

    pub(crate) fn signaling_url(&self) -> Result<&str, SignalingError> {
        self.signaling_url.expose()
    }

    pub(crate) fn join_token(&self) -> Result<&str, SignalingError> {
        self.join_token.expose()
    }

    pub(crate) fn turn_uris(&self) -> Result<Vec<&str>, SignalingError> {
        self.turn_uris.iter().map(EphemeralSecret::expose).collect()
    }
}

impl fmt::Debug for SlackJoinSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackJoinSession")
            .field("meeting_id", &"<redacted>")
            .field("attendee_id", &"<redacted>")
            .field("signaling_url", &self.signaling_url)
            .field("join_token", &self.join_token)
            .field("turn_uris", &"<redacted>")
            .finish()
    }
}

/// Bootstrap adapters must make `leave` idempotent and safe after partial or failed bootstrap.
/// Failed cleanup may be retried; successful cleanup will not be called again by the gate.
pub trait SlackHuddleBootstrap: Send {
    fn capability(&self) -> SlackBootstrapCapability;
    fn bootstrap(&mut self, huddle: &ActiveHuddle) -> Result<SlackJoinSession, SignalingError>;
    fn leave(&mut self, call_id: &str) -> Result<(), SignalingError>;
}

/// Bridge adapters must make `disconnect` idempotent and safe after partial or failed connect.
/// Failed cleanup may be retried; successful cleanup will not be called again by the gate.
/// `connect` only borrows bounded session material. Implementations must volatile-zeroize any
/// credential copies they create on successful disconnect and on adapter drop.
pub trait ChimeMediaBridge: Send {
    fn capability(&self) -> ChimeBridgeCapability;
    fn connect(&mut self, session: &SlackJoinSession) -> Result<(), SignalingError>;
    fn disconnect(&mut self) -> Result<(), SignalingError>;
}

pub struct NativeJoinGate<B, C> {
    bootstrap: B,
    bridge: C,
    cleanup_call_id: Option<SensitiveIdentifier>,
    bridge_disconnect_pending: bool,
    bootstrap_leave_pending: bool,
}

impl<B, C> NativeJoinGate<B, C>
where
    B: SlackHuddleBootstrap,
    C: ChimeMediaBridge,
{
    pub fn new(bootstrap: B, bridge: C) -> Self {
        Self {
            bootstrap,
            bridge,
            cleanup_call_id: None,
            bridge_disconnect_pending: false,
            bootstrap_leave_pending: false,
        }
    }

    pub fn capability(&self) -> NativeJoinCapability {
        let slack_contract_revision = match self.bootstrap.capability() {
            SlackBootstrapCapability::BrowserSessionRequired => {
                return NativeJoinCapability::Unavailable(
                    NativeJoinUnavailableReason::BrowserSessionRequired,
                );
            }
            SlackBootstrapCapability::ContractUnverified => {
                return NativeJoinCapability::Unavailable(
                    NativeJoinUnavailableReason::SlackBootstrapContractUnverified,
                );
            }
            SlackBootstrapCapability::Verified { contract_revision } => contract_revision,
        };
        let chime_bridge_revision = match self.bridge.capability() {
            ChimeBridgeCapability::Unavailable => {
                return NativeJoinCapability::Unavailable(
                    NativeJoinUnavailableReason::ChimeBridgeUnavailable,
                );
            }
            ChimeBridgeCapability::Verified { bridge_revision } => bridge_revision,
        };
        NativeJoinCapability::Available {
            slack_contract_revision,
            chime_bridge_revision,
        }
    }

    pub fn begin_join(&mut self, huddle: &ActiveHuddle) -> Result<(), SignalingError> {
        if self.has_pending_cleanup() {
            return Err(SignalingError::AlreadyConnected);
        }
        let call_id = required_identifier(&huddle.call_id)?;
        if let NativeJoinCapability::Unavailable(reason) = self.capability() {
            return Err(SignalingError::Unavailable(reason));
        }

        self.cleanup_call_id = Some(SensitiveIdentifier::from_validated(call_id));
        self.bootstrap_leave_pending = true;
        let session = match self.bootstrap.bootstrap(huddle) {
            Ok(session) => session,
            Err(error) => {
                let _ = self.cleanup_pending();
                return Err(error);
            }
        };

        self.bridge_disconnect_pending = true;
        let connect = self.bridge.connect(&session);
        // The bridge only borrows the session, so gate-owned credentials can be wiped now.
        drop(session);
        if let Err(error) = connect {
            let _ = self.cleanup_pending();
            return Err(error);
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), SignalingError> {
        self.cleanup_pending()
    }

    fn has_pending_cleanup(&self) -> bool {
        self.cleanup_call_id.is_some()
            || self.bridge_disconnect_pending
            || self.bootstrap_leave_pending
    }

    fn cleanup_pending(&mut self) -> Result<(), SignalingError> {
        let mut bridge_error = None;
        if self.bridge_disconnect_pending {
            match self.bridge.disconnect() {
                Ok(()) => self.bridge_disconnect_pending = false,
                Err(error) => bridge_error = Some(error),
            }
        }

        let mut leave_error = None;
        if self.bootstrap_leave_pending {
            let result = self
                .cleanup_call_id
                .as_ref()
                .map(SensitiveIdentifier::expose)
                .ok_or(SignalingError::InvalidSession)
                .and_then(|call_id| self.bootstrap.leave(call_id));
            match result {
                Ok(()) => {
                    self.bootstrap_leave_pending = false;
                    self.cleanup_call_id = None;
                }
                Err(error) => leave_error = Some(error),
            }
        } else {
            self.cleanup_call_id = None;
        }

        if let Some(error) = bridge_error {
            Err(error)
        } else if let Some(error) = leave_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct ProductionSlackBootstrap {
    browser_session_available: bool,
}

impl SlackHuddleBootstrap for ProductionSlackBootstrap {
    fn capability(&self) -> SlackBootstrapCapability {
        if self.browser_session_available {
            SlackBootstrapCapability::ContractUnverified
        } else {
            SlackBootstrapCapability::BrowserSessionRequired
        }
    }

    fn bootstrap(&mut self, _huddle: &ActiveHuddle) -> Result<SlackJoinSession, SignalingError> {
        Err(SignalingError::BootstrapFailed)
    }

    fn leave(&mut self, _call_id: &str) -> Result<(), SignalingError> {
        Ok(())
    }
}

#[derive(Debug)]
struct ProductionChimeBridge;

impl ChimeMediaBridge for ProductionChimeBridge {
    fn capability(&self) -> ChimeBridgeCapability {
        ChimeBridgeCapability::Unavailable
    }

    fn connect(&mut self, _session: &SlackJoinSession) -> Result<(), SignalingError> {
        Err(SignalingError::ChimeBridgeFailed)
    }

    fn disconnect(&mut self) -> Result<(), SignalingError> {
        Ok(())
    }
}

pub fn production_native_join_capability(browser_session_available: bool) -> NativeJoinCapability {
    NativeJoinGate::new(
        ProductionSlackBootstrap {
            browser_session_available,
        },
        ProductionChimeBridge,
    )
    .capability()
}

fn required_identifier(value: &str) -> Result<&str, SignalingError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        Err(SignalingError::InvalidSession)
    } else {
        Ok(value)
    }
}

fn required_value(value: &str, max_bytes: usize) -> Result<&str, SignalingError> {
    if value.len() > max_bytes {
        return Err(SignalingError::InvalidSession);
    }
    let value = value.trim();
    if value.is_empty() {
        Err(SignalingError::InvalidSession)
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use super::*;

    #[test]
    fn production_capability_never_claims_unverified_private_join_support() {
        assert_eq!(
            production_native_join_capability(false),
            NativeJoinCapability::Unavailable(NativeJoinUnavailableReason::BrowserSessionRequired)
        );
        assert_eq!(
            production_native_join_capability(true),
            NativeJoinCapability::Unavailable(
                NativeJoinUnavailableReason::SlackBootstrapContractUnverified
            )
        );
    }

    #[test]
    fn gate_checks_both_capabilities_before_using_private_bootstrap() {
        let calls = Arc::new(AtomicUsize::new(0));
        let bootstrap = CountingBootstrap {
            calls: Arc::clone(&calls),
        };
        let bridge = UnavailableBridge;
        let mut gate = NativeJoinGate::new(bootstrap, bridge);

        assert_eq!(
            gate.capability(),
            NativeJoinCapability::Unavailable(NativeJoinUnavailableReason::ChimeBridgeUnavailable)
        );
        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::Unavailable(NativeJoinUnavailableReason::ChimeBridgeUnavailable)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ephemeral_join_credentials_are_always_redacted_from_debug_output() {
        let session = SlackJoinSession::new_for_adapter(
            "secret-meeting-id",
            "secret-attendee-id",
            "wss://signal.example.test/?token=secret-signal",
            "secret-join-token",
            vec!["turn:turn.example.test?credential=secret-turn".to_string()],
        )
        .unwrap();

        let debug = format!("{session:?}");
        assert!(!debug.contains("secret-signal"));
        assert!(!debug.contains("secret-join-token"));
        assert!(!debug.contains("secret-turn"));
        assert!(!debug.contains("secret-meeting-id"));
        assert!(!debug.contains("secret-attendee-id"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn stored_identifiers_are_zeroized_on_drop_and_cleanup_clear() {
        let session_zeroizes = Arc::new(AtomicUsize::new(0));
        set_sensitive_identifier_zeroize_hook(Some(Box::new({
            let session_zeroizes = Arc::clone(&session_zeroizes);
            move |was_zeroized| {
                assert!(was_zeroized);
                session_zeroizes.fetch_add(1, Ordering::SeqCst);
            }
        })));
        drop(
            new_session(
                "meeting",
                "attendee",
                "wss://signal.invalid",
                "token",
                one_turn_uri(),
            )
            .unwrap(),
        );
        set_sensitive_identifier_zeroize_hook(None);
        assert_eq!(session_zeroizes.load(Ordering::SeqCst), 2);

        let (mut gate, _trace) = fake_gate(Ok(()), Ok(()), Vec::new(), Vec::new());
        gate.begin_join(&huddle()).unwrap();
        let cleanup_zeroizes = Arc::new(AtomicUsize::new(0));
        set_sensitive_identifier_zeroize_hook(Some(Box::new({
            let cleanup_zeroizes = Arc::clone(&cleanup_zeroizes);
            move |was_zeroized| {
                assert!(was_zeroized);
                cleanup_zeroizes.fetch_add(1, Ordering::SeqCst);
            }
        })));
        gate.stop().unwrap();
        set_sensitive_identifier_zeroize_hook(None);
        assert_eq!(cleanup_zeroizes.load(Ordering::SeqCst), 1);

        let (mut gate, _trace) = fake_gate(Ok(()), Ok(()), Vec::new(), Vec::new());
        gate.begin_join(&huddle()).unwrap();
        let drop_zeroizes = Arc::new(AtomicUsize::new(0));
        set_sensitive_identifier_zeroize_hook(Some(Box::new({
            let drop_zeroizes = Arc::clone(&drop_zeroizes);
            move |was_zeroized| {
                assert!(was_zeroized);
                drop_zeroizes.fetch_add(1, Ordering::SeqCst);
            }
        })));
        drop(gate);
        set_sensitive_identifier_zeroize_hook(None);
        assert_eq!(drop_zeroizes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn secret_storage_is_zeroized_on_drop_and_constructor_rejection() {
        let ephemeral_zeroizes = Arc::new(AtomicUsize::new(0));
        let input_zeroizes = Arc::new(AtomicUsize::new(0));
        set_sensitive_secret_zeroize_hook(Some(Box::new({
            let ephemeral_zeroizes = Arc::clone(&ephemeral_zeroizes);
            let input_zeroizes = Arc::clone(&input_zeroizes);
            move |storage, was_zeroized| {
                assert!(was_zeroized);
                match storage {
                    SensitiveSecretStorage::Ephemeral => {
                        ephemeral_zeroizes.fetch_add(1, Ordering::SeqCst);
                    }
                    SensitiveSecretStorage::OwnedTurnInput => {
                        input_zeroizes.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        })));

        assert_invalid_session(new_session(
            "meeting\u{7f}",
            "attendee",
            "wss://signal.invalid",
            "token",
            vec![
                "turn:one.invalid".to_string(),
                "turn:two.invalid".to_string(),
            ],
        ));
        assert_eq!(ephemeral_zeroizes.load(Ordering::SeqCst), 0);
        assert_eq!(input_zeroizes.load(Ordering::SeqCst), 2);

        drop(
            new_session(
                "meeting",
                "attendee",
                "wss://signal.invalid",
                "token",
                one_turn_uri(),
            )
            .unwrap(),
        );
        set_sensitive_secret_zeroize_hook(None);
        assert_eq!(ephemeral_zeroizes.load(Ordering::SeqCst), 3);
        assert_eq!(input_zeroizes.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn join_session_accepts_exact_material_caps() {
        let meeting_id = "m".repeat(MAX_IDENTIFIER_BYTES);
        let attendee_id = "a".repeat(MAX_IDENTIFIER_BYTES);
        let signaling_url = format!(
            "wss://{}",
            "s".repeat(MAX_SIGNALING_URL_BYTES - "wss://".len())
        );
        let join_token = "j".repeat(MAX_JOIN_TOKEN_BYTES);
        let turn_uri = format!("turn:{}", "t".repeat(MAX_TURN_URI_BYTES - "turn:".len()));
        let turn_uris = vec![turn_uri; MAX_TURN_URIS];

        let session = SlackJoinSession::new_for_adapter(
            &meeting_id,
            &attendee_id,
            &signaling_url,
            &join_token,
            turn_uris,
        )
        .unwrap();

        assert_eq!(session.meeting_id().len(), MAX_IDENTIFIER_BYTES);
        assert_eq!(session.attendee_id().len(), MAX_IDENTIFIER_BYTES);
        assert_eq!(
            session.signaling_url().unwrap().len(),
            MAX_SIGNALING_URL_BYTES
        );
        assert_eq!(session.join_token().unwrap().len(), MAX_JOIN_TOKEN_BYTES);
        let turn_uris = session.turn_uris().unwrap();
        assert_eq!(turn_uris.len(), MAX_TURN_URIS);
        assert!(turn_uris.iter().all(|uri| uri.len() == MAX_TURN_URI_BYTES));
    }

    #[test]
    fn join_session_rejects_each_material_cap_plus_one() {
        assert_invalid_session(new_session(
            &"m".repeat(MAX_IDENTIFIER_BYTES + 1),
            "attendee",
            "wss://signal.invalid",
            "token",
            one_turn_uri(),
        ));
        assert_invalid_session(new_session(
            "meeting",
            &"a".repeat(MAX_IDENTIFIER_BYTES + 1),
            "wss://signal.invalid",
            "token",
            one_turn_uri(),
        ));
        let long_signaling_url = format!(
            "wss://{}",
            "s".repeat(MAX_SIGNALING_URL_BYTES + 1 - "wss://".len())
        );
        assert_invalid_session(new_session(
            "meeting",
            "attendee",
            &long_signaling_url,
            "token",
            one_turn_uri(),
        ));
        assert_invalid_session(new_session(
            "meeting",
            "attendee",
            "wss://signal.invalid",
            &"j".repeat(MAX_JOIN_TOKEN_BYTES + 1),
            one_turn_uri(),
        ));
        assert_invalid_session(new_session(
            "meeting",
            "attendee",
            "wss://signal.invalid",
            "token",
            vec!["turn:relay.invalid".to_string(); MAX_TURN_URIS + 1],
        ));
        assert_invalid_session(new_session(
            "meeting",
            "attendee",
            "wss://signal.invalid",
            "token",
            vec![format!(
                "turn:{}",
                "t".repeat(MAX_TURN_URI_BYTES + 1 - "turn:".len())
            )],
        ));
    }

    #[test]
    fn identifiers_reject_control_characters() {
        assert_invalid_session(new_session(
            "meeting\u{7f}",
            "attendee",
            "wss://signal.invalid",
            "token",
            one_turn_uri(),
        ));

        let (mut gate, trace) = fake_gate(Ok(()), Ok(()), Vec::new(), Vec::new());
        let mut huddle = huddle();
        huddle.call_id = "call\u{7f}".to_string();
        assert_eq!(
            gate.begin_join(&huddle).unwrap_err(),
            SignalingError::InvalidSession
        );
        assert!(adapter_calls(&trace).is_empty());
    }

    #[test]
    fn oversized_call_id_is_rejected_before_any_adapter_call() {
        let (mut gate, trace) = fake_gate(Ok(()), Ok(()), Vec::new(), Vec::new());
        let mut huddle = huddle();
        huddle.call_id = "c".repeat(MAX_IDENTIFIER_BYTES + 1);

        assert_eq!(
            gate.begin_join(&huddle).unwrap_err(),
            SignalingError::InvalidSession
        );
        assert!(adapter_calls(&trace).is_empty());
    }

    #[test]
    fn exact_call_id_cap_can_join_and_cleanup() {
        let (mut gate, trace) = fake_gate(Ok(()), Ok(()), Vec::new(), Vec::new());
        let mut huddle = huddle();
        huddle.call_id = "c".repeat(MAX_IDENTIFIER_BYTES);

        gate.begin_join(&huddle).unwrap();
        gate.stop().unwrap();

        assert_eq!(
            adapter_calls(&trace),
            vec![
                AdapterCall::BootstrapCapability,
                AdapterCall::BridgeCapability,
                AdapterCall::Bootstrap,
                AdapterCall::Connect,
                AdapterCall::Disconnect,
                AdapterCall::Leave,
            ]
        );
    }

    #[test]
    fn second_join_is_rejected_without_adapter_calls() {
        let (mut gate, trace) = fake_gate(Ok(()), Ok(()), Vec::new(), Vec::new());
        gate.begin_join(&huddle()).unwrap();
        let calls_after_first_join = adapter_calls(&trace);

        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::AlreadyConnected
        );
        assert_eq!(adapter_calls(&trace), calls_after_first_join);
    }

    #[test]
    fn bootstrap_failure_attempts_leave_and_retains_failed_cleanup() {
        let (mut gate, trace) = fake_gate(
            Err(SignalingError::BootstrapFailed),
            Ok(()),
            vec![Err(SignalingError::InvalidSession), Ok(())],
            Vec::new(),
        );

        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::BootstrapFailed
        );
        assert_eq!(
            adapter_calls(&trace),
            vec![
                AdapterCall::BootstrapCapability,
                AdapterCall::BridgeCapability,
                AdapterCall::Bootstrap,
                AdapterCall::Leave,
            ]
        );
        let calls_after_failure = adapter_calls(&trace);
        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::AlreadyConnected
        );
        assert_eq!(adapter_calls(&trace), calls_after_failure);

        gate.stop().unwrap();
        assert_eq!(adapter_calls(&trace).last(), Some(&AdapterCall::Leave));
    }

    #[test]
    fn connect_failure_cleans_bridge_then_bootstrap_and_returns_original_error() {
        let (mut gate, trace) = fake_gate(
            Ok(()),
            Err(SignalingError::InvalidSession),
            vec![Ok(())],
            vec![Ok(())],
        );

        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::InvalidSession
        );
        assert_eq!(
            adapter_calls(&trace),
            vec![
                AdapterCall::BootstrapCapability,
                AdapterCall::BridgeCapability,
                AdapterCall::Bootstrap,
                AdapterCall::Connect,
                AdapterCall::Disconnect,
                AdapterCall::Leave,
            ]
        );
        gate.stop().unwrap();
        assert_eq!(adapter_calls(&trace).len(), 6);
    }

    #[test]
    fn connect_failure_retains_failed_cleanup_and_retries_original_obligations() {
        let (mut gate, trace) = fake_gate(
            Ok(()),
            Err(SignalingError::InvalidSession),
            vec![Err(SignalingError::BootstrapFailed), Ok(())],
            vec![Err(SignalingError::ChimeBridgeFailed), Ok(())],
        );
        let identifier_zeroizes = Arc::new(AtomicUsize::new(0));
        set_sensitive_identifier_zeroize_hook(Some(Box::new({
            let identifier_zeroizes = Arc::clone(&identifier_zeroizes);
            move |was_zeroized| {
                assert!(was_zeroized);
                identifier_zeroizes.fetch_add(1, Ordering::SeqCst);
            }
        })));
        let ephemeral_zeroizes = Arc::new(AtomicUsize::new(0));
        set_sensitive_secret_zeroize_hook(Some(Box::new({
            let ephemeral_zeroizes = Arc::clone(&ephemeral_zeroizes);
            move |storage, was_zeroized| {
                assert!(was_zeroized);
                if storage == SensitiveSecretStorage::Ephemeral {
                    ephemeral_zeroizes.fetch_add(1, Ordering::SeqCst);
                }
            }
        })));

        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::InvalidSession
        );
        assert_eq!(identifier_zeroizes.load(Ordering::SeqCst), 2);
        assert_eq!(ephemeral_zeroizes.load(Ordering::SeqCst), 3);
        assert_eq!(
            adapter_calls(&trace),
            vec![
                AdapterCall::BootstrapCapability,
                AdapterCall::BridgeCapability,
                AdapterCall::Bootstrap,
                AdapterCall::Connect,
                AdapterCall::Disconnect,
                AdapterCall::Leave,
            ]
        );
        let calls_after_failure = adapter_calls(&trace);
        assert_eq!(
            gate.begin_join(&huddle()).unwrap_err(),
            SignalingError::AlreadyConnected
        );
        assert_eq!(adapter_calls(&trace), calls_after_failure);
        assert_eq!(identifier_zeroizes.load(Ordering::SeqCst), 2);
        assert_eq!(ephemeral_zeroizes.load(Ordering::SeqCst), 3);

        gate.stop().unwrap();
        set_sensitive_identifier_zeroize_hook(None);
        set_sensitive_secret_zeroize_hook(None);
        assert_eq!(identifier_zeroizes.load(Ordering::SeqCst), 3);
        assert_eq!(ephemeral_zeroizes.load(Ordering::SeqCst), 3);
        assert_eq!(
            &adapter_calls(&trace)[6..],
            &[AdapterCall::Disconnect, AdapterCall::Leave]
        );
    }

    #[test]
    fn stop_attempts_all_cleanup_and_prioritizes_bridge_error() {
        let (mut gate, trace) = fake_gate(
            Ok(()),
            Ok(()),
            vec![Err(SignalingError::BootstrapFailed), Ok(())],
            vec![Err(SignalingError::ChimeBridgeFailed), Ok(())],
        );
        gate.begin_join(&huddle()).unwrap();
        clear_adapter_calls(&trace);

        assert_eq!(gate.stop().unwrap_err(), SignalingError::ChimeBridgeFailed);
        assert_eq!(
            adapter_calls(&trace),
            vec![AdapterCall::Disconnect, AdapterCall::Leave]
        );
        clear_adapter_calls(&trace);

        gate.stop().unwrap();
        assert_eq!(
            adapter_calls(&trace),
            vec![AdapterCall::Disconnect, AdapterCall::Leave]
        );
        clear_adapter_calls(&trace);

        gate.stop().unwrap();
        assert!(adapter_calls(&trace).is_empty());
    }

    #[test]
    fn stop_retries_only_failed_cleanup_steps() {
        let (mut gate, trace) = fake_gate(
            Ok(()),
            Ok(()),
            vec![Ok(())],
            vec![Err(SignalingError::ChimeBridgeFailed), Ok(())],
        );
        gate.begin_join(&huddle()).unwrap();
        clear_adapter_calls(&trace);

        assert_eq!(gate.stop().unwrap_err(), SignalingError::ChimeBridgeFailed);
        assert_eq!(
            adapter_calls(&trace),
            vec![AdapterCall::Disconnect, AdapterCall::Leave]
        );
        clear_adapter_calls(&trace);

        gate.stop().unwrap();
        assert_eq!(adapter_calls(&trace), vec![AdapterCall::Disconnect]);
        clear_adapter_calls(&trace);

        gate.stop().unwrap();
        assert!(adapter_calls(&trace).is_empty());

        let (mut gate, trace) = fake_gate(
            Ok(()),
            Ok(()),
            vec![Err(SignalingError::BootstrapFailed), Ok(())],
            vec![Ok(())],
        );
        gate.begin_join(&huddle()).unwrap();
        clear_adapter_calls(&trace);

        assert_eq!(gate.stop().unwrap_err(), SignalingError::BootstrapFailed);
        assert_eq!(
            adapter_calls(&trace),
            vec![AdapterCall::Disconnect, AdapterCall::Leave]
        );
        clear_adapter_calls(&trace);

        gate.stop().unwrap();
        assert_eq!(adapter_calls(&trace), vec![AdapterCall::Leave]);
    }

    fn new_session(
        meeting_id: &str,
        attendee_id: &str,
        signaling_url: &str,
        join_token: &str,
        turn_uris: Vec<String>,
    ) -> Result<SlackJoinSession, SignalingError> {
        SlackJoinSession::new_for_adapter(
            meeting_id,
            attendee_id,
            signaling_url,
            join_token,
            turn_uris,
        )
    }

    fn one_turn_uri() -> Vec<String> {
        vec!["turn:relay.invalid".to_string()]
    }

    fn assert_invalid_session(result: Result<SlackJoinSession, SignalingError>) {
        assert_eq!(result.unwrap_err(), SignalingError::InvalidSession);
    }

    fn huddle() -> crate::huddles::model::ActiveHuddle {
        crate::huddles::model::ActiveHuddle {
            team_id: "T123".to_string(),
            channel_id: "C123".to_string(),
            call_id: "R123".to_string(),
            name: None,
            participant_ids: Vec::new(),
            started_at: None,
            huddle_link: None,
        }
    }

    struct CountingBootstrap {
        calls: Arc<AtomicUsize>,
    }

    impl SlackHuddleBootstrap for CountingBootstrap {
        fn capability(&self) -> SlackBootstrapCapability {
            SlackBootstrapCapability::Verified {
                contract_revision: "synthetic-v1",
            }
        }

        fn bootstrap(
            &mut self,
            _huddle: &crate::huddles::model::ActiveHuddle,
        ) -> Result<SlackJoinSession, SignalingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("capability gate must stop before bootstrap")
        }

        fn leave(&mut self, _call_id: &str) -> Result<(), SignalingError> {
            Ok(())
        }
    }

    struct UnavailableBridge;

    impl ChimeMediaBridge for UnavailableBridge {
        fn capability(&self) -> ChimeBridgeCapability {
            ChimeBridgeCapability::Unavailable
        }

        fn connect(&mut self, _session: &SlackJoinSession) -> Result<(), SignalingError> {
            unreachable!("capability gate must stop before bridge connect")
        }

        fn disconnect(&mut self) -> Result<(), SignalingError> {
            Ok(())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AdapterCall {
        BootstrapCapability,
        BridgeCapability,
        Bootstrap,
        Connect,
        Disconnect,
        Leave,
    }

    type AdapterTrace = Arc<Mutex<Vec<AdapterCall>>>;

    struct FakeBootstrap {
        trace: AdapterTrace,
        bootstrap_result: Result<(), SignalingError>,
        leave_results: VecDeque<Result<(), SignalingError>>,
    }

    impl SlackHuddleBootstrap for FakeBootstrap {
        fn capability(&self) -> SlackBootstrapCapability {
            self.trace
                .lock()
                .expect("adapter trace lock poisoned")
                .push(AdapterCall::BootstrapCapability);
            SlackBootstrapCapability::Verified {
                contract_revision: "test-v1",
            }
        }

        fn bootstrap(
            &mut self,
            _huddle: &ActiveHuddle,
        ) -> Result<SlackJoinSession, SignalingError> {
            self.trace
                .lock()
                .expect("adapter trace lock poisoned")
                .push(AdapterCall::Bootstrap);
            self.bootstrap_result?;
            new_session(
                "meeting",
                "attendee",
                "wss://signal.invalid",
                "token",
                one_turn_uri(),
            )
        }

        fn leave(&mut self, _call_id: &str) -> Result<(), SignalingError> {
            self.trace
                .lock()
                .expect("adapter trace lock poisoned")
                .push(AdapterCall::Leave);
            self.leave_results.pop_front().unwrap_or(Ok(()))
        }
    }

    struct FakeBridge {
        trace: AdapterTrace,
        connect_result: Result<(), SignalingError>,
        disconnect_results: VecDeque<Result<(), SignalingError>>,
    }

    impl ChimeMediaBridge for FakeBridge {
        fn capability(&self) -> ChimeBridgeCapability {
            self.trace
                .lock()
                .expect("adapter trace lock poisoned")
                .push(AdapterCall::BridgeCapability);
            ChimeBridgeCapability::Verified {
                bridge_revision: "test-v1",
            }
        }

        fn connect(&mut self, _session: &SlackJoinSession) -> Result<(), SignalingError> {
            self.trace
                .lock()
                .expect("adapter trace lock poisoned")
                .push(AdapterCall::Connect);
            self.connect_result
        }

        fn disconnect(&mut self) -> Result<(), SignalingError> {
            self.trace
                .lock()
                .expect("adapter trace lock poisoned")
                .push(AdapterCall::Disconnect);
            self.disconnect_results.pop_front().unwrap_or(Ok(()))
        }
    }

    fn fake_gate(
        bootstrap_result: Result<(), SignalingError>,
        connect_result: Result<(), SignalingError>,
        leave_results: Vec<Result<(), SignalingError>>,
        disconnect_results: Vec<Result<(), SignalingError>>,
    ) -> (NativeJoinGate<FakeBootstrap, FakeBridge>, AdapterTrace) {
        let trace = Arc::new(Mutex::new(Vec::new()));
        (
            NativeJoinGate::new(
                FakeBootstrap {
                    trace: Arc::clone(&trace),
                    bootstrap_result,
                    leave_results: leave_results.into(),
                },
                FakeBridge {
                    trace: Arc::clone(&trace),
                    connect_result,
                    disconnect_results: disconnect_results.into(),
                },
            ),
            trace,
        )
    }

    fn adapter_calls(trace: &AdapterTrace) -> Vec<AdapterCall> {
        trace.lock().expect("adapter trace lock poisoned").clone()
    }

    fn clear_adapter_calls(trace: &AdapterTrace) {
        trace.lock().expect("adapter trace lock poisoned").clear();
    }
}
