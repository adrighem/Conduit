use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn log(scope: &str, message: &str) {
    if enabled() {
        eprintln!("[conduit::{scope}] {message}");
    }
}

pub fn url_for_log(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return truncate(value);
    };

    if url.scheme() == "data" {
        return "data:<redacted>".to_string();
    }

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
