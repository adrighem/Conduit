use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Registry;

static ENABLED: AtomicBool = AtomicBool::new(false);
static FILTER_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = EnvFilter::try_new(tracing_filter_spec(false, rust_log().as_deref()))?;
    let (filter_layer, filter_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_target(false),
        )
        .try_init()?;
    let _ = FILTER_HANDLE.set(filter_handle);
    Ok(())
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if let Some(handle) = FILTER_HANDLE.get() {
        let filter = EnvFilter::new(tracing_filter_spec(enabled, rust_log().as_deref()));
        let _ = handle.reload(filter);
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn log(scope: &str, message: &str) {
    if enabled() {
        tracing::debug!(target: "conduit", scope, message);
    }
}

fn rust_log() -> Option<String> {
    std::env::var("RUST_LOG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn tracing_filter_spec(debug: bool, environment: Option<&str>) -> String {
    match (
        debug,
        environment.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (true, Some(environment)) => format!("conduit=debug,{environment}"),
        (true, None) => "conduit=debug".to_string(),
        (false, Some(environment)) => environment.to_string(),
        (false, None) => "off".to_string(),
    }
}

pub fn url_for_log(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return truncate(value);
    };

    if url.scheme() == "data" {
        return "data:<redacted>".to_string();
    }

    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    truncate(url.as_str())
}

fn truncate(value: &str) -> String {
    const MAX_LENGTH: usize = 160;
    if value.chars().count() <= MAX_LENGTH {
        value.to_string()
    } else {
        format!("{}...", value.chars().take(MAX_LENGTH).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_filter_is_quiet_by_default_and_debuggable_on_request() {
        assert_eq!(tracing_filter_spec(false, None), "off");
        assert_eq!(tracing_filter_spec(true, None), "conduit=debug");
    }

    #[test]
    fn tracing_filter_preserves_explicit_environment_overrides() {
        assert_eq!(
            tracing_filter_spec(false, Some("conduit::runtime=trace")),
            "conduit::runtime=trace"
        );
        assert_eq!(
            tracing_filter_spec(true, Some("conduit::runtime=trace")),
            "conduit=debug,conduit::runtime=trace"
        );
    }

    #[test]
    fn diagnostic_urls_remove_credentials_queries_fragments_and_data() {
        assert_eq!(
            url_for_log(
                "https://viewer:password@files.slack.com/path/image.png?token=signed-secret#preview"
            ),
            "https://files.slack.com/path/image.png"
        );
        assert_eq!(
            url_for_log("data:image/png;base64,sensitive"),
            "data:<redacted>"
        );
    }
}
