// Verified adapter implementations are intentionally absent until Slack's private
// bootstrap contract and a packaged Chime bridge can be tested safely.
#![allow(dead_code)]

use std::fmt;

use crate::huddles::model::ActiveHuddle;
use crate::huddles::state::HuddleFailure;

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
    #[error("no native huddle session is connected")]
    NotConnected,
}

pub struct EphemeralSecret(Box<[u8]>);

impl EphemeralSecret {
    fn new(value: &str) -> Result<Self, SignalingError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(SignalingError::InvalidSession);
        }
        Ok(Self(value.as_bytes().to_vec().into_boxed_slice()))
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
        self.0.fill(0);
    }
}

pub struct SlackJoinSession {
    meeting_id: String,
    attendee_id: String,
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
        let meeting_id = required_identifier(meeting_id)?;
        let attendee_id = required_identifier(attendee_id)?;
        if !signaling_url.trim().starts_with("wss://") || turn_uris.is_empty() {
            return Err(SignalingError::InvalidSession);
        }
        let turn_uris = turn_uris
            .iter()
            .map(|uri| EphemeralSecret::new(uri))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            meeting_id,
            attendee_id,
            signaling_url: EphemeralSecret::new(signaling_url)?,
            join_token: EphemeralSecret::new(join_token)?,
            turn_uris,
        })
    }

    pub(crate) fn meeting_id(&self) -> &str {
        &self.meeting_id
    }

    pub(crate) fn attendee_id(&self) -> &str {
        &self.attendee_id
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

pub trait SlackHuddleBootstrap: Send {
    fn capability(&self) -> SlackBootstrapCapability;
    fn bootstrap(&mut self, huddle: &ActiveHuddle) -> Result<SlackJoinSession, SignalingError>;
    fn leave(&mut self, call_id: &str) -> Result<(), SignalingError>;
}

pub trait ChimeMediaBridge: Send {
    fn capability(&self) -> ChimeBridgeCapability;
    fn connect(&mut self, session: SlackJoinSession) -> Result<(), SignalingError>;
    fn disconnect(&mut self) -> Result<(), SignalingError>;
}

pub struct NativeJoinGate<B, C> {
    bootstrap: B,
    bridge: C,
    active_call_id: Option<String>,
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
            active_call_id: None,
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
        if let NativeJoinCapability::Unavailable(reason) = self.capability() {
            return Err(SignalingError::Unavailable(reason));
        }
        let session = self.bootstrap.bootstrap(huddle)?;
        if let Err(error) = self.bridge.connect(session) {
            let _ = self.bootstrap.leave(&huddle.call_id);
            return Err(error);
        }
        self.active_call_id = Some(huddle.call_id.clone());
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), SignalingError> {
        let call_id = self
            .active_call_id
            .take()
            .ok_or(SignalingError::NotConnected)?;
        let disconnect = self.bridge.disconnect();
        let leave = self.bootstrap.leave(&call_id);
        disconnect.and(leave)
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

    fn connect(&mut self, _session: SlackJoinSession) -> Result<(), SignalingError> {
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

fn required_identifier(value: &str) -> Result<String, SignalingError> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        Err(SignalingError::InvalidSession)
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
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
            "meeting-1",
            "attendee-1",
            "wss://signal.example.test/?token=secret-signal",
            "secret-join-token",
            vec!["turn:turn.example.test?credential=secret-turn".to_string()],
        )
        .unwrap();

        let debug = format!("{session:?}");
        assert!(!debug.contains("secret-signal"));
        assert!(!debug.contains("secret-join-token"));
        assert!(!debug.contains("secret-turn"));
        assert!(debug.contains("redacted"));
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

        fn connect(&mut self, _session: SlackJoinSession) -> Result<(), SignalingError> {
            unreachable!("capability gate must stop before bridge connect")
        }

        fn disconnect(&mut self) -> Result<(), SignalingError> {
            Ok(())
        }
    }
}
