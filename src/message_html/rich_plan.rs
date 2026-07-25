use super::rich_model::RichDocument;
use crate::message_handoff::MessageControlHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RichRenderPlan {
    pub document: RichDocument,
    pub control_handle: Option<MessageControlHandle>,
}

impl RichRenderPlan {
    pub fn new(document: RichDocument, control_handle: Option<MessageControlHandle>) -> Self {
        Self {
            document,
            control_handle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ControlPlan {
    Navigate { label: String, url: String },
    SlackHandoff { label: String },
    Unavailable { label: String },
}

pub(super) fn plan_control(
    label: &str,
    url: Option<&str>,
    confirmation_required: bool,
) -> ControlPlan {
    match url {
        Some(url) if !super::is_http_url(url) => ControlPlan::Unavailable {
            label: label.to_string(),
        },
        Some(url) if !confirmation_required => ControlPlan::Navigate {
            label: label.to_string(),
            url: url.to_string(),
        },
        _ => ControlPlan::SlackHandoff {
            label: label.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_safe_external_and_unavailable_controls() {
        assert!(matches!(
            plan_control("Open", Some("https://example.test/item"), false),
            ControlPlan::Navigate { .. }
        ));
        assert!(matches!(
            plan_control("Approve", None, false),
            ControlPlan::SlackHandoff { .. }
        ));
        assert!(matches!(
            plan_control("Confirm", Some("https://example.test/item"), true),
            ControlPlan::SlackHandoff { .. }
        ));
        assert!(matches!(
            plan_control("Unsafe", Some("javascript:alert(1)"), false),
            ControlPlan::Unavailable { .. }
        ));
    }
}
