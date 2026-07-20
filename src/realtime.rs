#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeTransport {
    SocketMode,
    BrowserSession,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RealtimePhase {
    #[default]
    NotConfigured,
    Connecting,
    Online,
    Reconnecting,
    ConfigurationError,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RealtimeStatus {
    pub transport: Option<RealtimeTransport>,
    pub phase: RealtimePhase,
}

impl RealtimeStatus {
    pub(crate) const fn connecting(transport: RealtimeTransport) -> Self {
        Self {
            transport: Some(transport),
            phase: RealtimePhase::Connecting,
        }
    }

    pub(crate) const fn online(transport: RealtimeTransport) -> Self {
        Self {
            transport: Some(transport),
            phase: RealtimePhase::Online,
        }
    }

    pub(crate) const fn reconnecting(transport: RealtimeTransport) -> Self {
        Self {
            transport: Some(transport),
            phase: RealtimePhase::Reconnecting,
        }
    }

    pub(crate) const fn configuration_error() -> Self {
        Self {
            transport: None,
            phase: RealtimePhase::ConfigurationError,
        }
    }
}
